//! 管理员对一个确切直连目标执行一次有界探测，不经公共名称路由、重试或故障转移。
//!
//! 指标绑定被探测的存储行（含渠道生成的内部直连），组合模型与通配别名不能代替
//! 一个确切目标。显式 embedding 走向量接口，其余直连执行文本协议探测；一次文本
//! 失败不说明图像、音频等其它能力不可用。脱敏样本写入历史，不更改健康或正式用量。
//!
//! 首字只计非空可见正文，角色、推理、工具与 usage 均不计：
//! <https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events>
//! <https://platform.claude.com/docs/en/build-with-claude/streaming>

mod capability;
mod providers;
mod sdk_http;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_core::models::{Model, ProviderKey};
use aisix_gateway::chat::{ChoiceFinishCensus, EmbeddingUsageSource, EmbeddingVector};
use aisix_gateway::response_tap::{self, ResponseHeaderTap};
use aisix_gateway::{
    Bridge, BridgeContext, BridgeError, ChatChunk, ChatFormat, ChatMessage, EmbeddingRequest,
};
use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::model_probe_history::{self as history, Snapshot};
use crate::resources::{self, store, Kind};
use crate::AppState;

pub const MAX_IN_FLIGHT: usize = 3;
pub const TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_BUDGET: Duration = Duration::from_secs(55);
// 最后五秒只用于保存最终结果，不能再发出模型或凭据交换调用。
const WORK_BUDGET: Duration = Duration::from_secs(50);
const MAX_TOKENS: u32 = 64;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_CHUNKS: usize = 4096;
const MAX_CAPABILITY_TOKENS: u32 = 256;
const MAX_CAPABILITY_TEXT_BYTES: usize = 8 * 1024;
const PROMPT: &str = "Reply with the word OK.";

#[derive(Default, Deserialize)]
pub struct ProbeBody {
    #[serde(default)]
    capability_check: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Available,
    Failed,
    Timeout,
    Unsupported,
}

#[derive(Default, Serialize)]
struct Usage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

#[derive(Serialize)]
struct Fidelity {
    status: &'static str,
    reason: &'static str,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CapabilityStatus {
    NotTested,
    Passed,
    Failed,
    Error,
    Timeout,
    Unsupported,
}

#[derive(Serialize)]
struct CapabilityResult {
    status: CapabilityStatus,
    checks: capability::Checks,
    total_duration_ms: Option<f64>,
    usage: Option<Usage>,
    error: Option<String>,
    note: Option<String>,
}

impl CapabilityResult {
    fn not_tested(note: &str) -> Self {
        Self {
            status: CapabilityStatus::NotTested,
            checks: capability::Checks::default(),
            total_duration_ms: None,
            usage: None,
            error: None,
            note: Some(note.to_string()),
        }
    }

    fn unsupported(reason: &str) -> Self {
        Self {
            status: CapabilityStatus::Unsupported,
            error: Some(reason.to_string()),
            ..Self::not_tested("未发出额外的模型请求")
        }
    }
}

#[derive(Serialize)]
pub struct ProbeResult {
    model_id: Uuid,
    status: Status,
    ttft_ms: Option<f64>,
    total_duration_ms: f64,
    /// 只保留适配器实际给出的 HTTP 错误状态；成功状态未暴露时不编造 200。
    http_status: Option<u16>,
    usage: Option<Usage>,
    /// 上游报告的输出 token / 探测全程秒数，包含首字等待及可能的推理 token。
    output_tokens_per_second: Option<f64>,
    error: Option<String>,
    note: Option<String>,
    checked_at: DateTime<Utc>,
    fidelity: Fidelity,
    capability_check: CapabilityResult,
}

impl ProbeResult {
    fn new(model_id: Uuid) -> Self {
        Self {
            model_id,
            status: Status::Failed,
            ttft_ms: None,
            total_duration_ms: 0.0,
            http_status: None,
            usage: None,
            output_tokens_per_second: None,
            error: None,
            note: None,
            checked_at: Utc::now(),
            fidelity: Fidelity {
                status: "not_tested",
                reason: "本功能不验证模型身份，能力抽测也只代表单次样本",
            },
            capability_check: CapabilityResult::not_tested("未启用能力抽测，未发出额外模型请求"),
        }
    }

    fn unsupported(mut self, reason: &str) -> Self {
        self.status = Status::Unsupported;
        self.error = Some(reason.to_string());
        self
    }
}

/// `POST /admin/models/{id}/probe`。管理员权限在任何资源读取或凭据解密之前检查。
pub async fn probe(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ApiJson(body): ApiJson<ProbeBody>,
) -> Result<Json<ProbeResult>, ApiError> {
    let route_started = tokio::time::Instant::now();
    auth.require(OperatorRole::Admin)?;
    let result = ProbeResult::new(id);
    let loading_budget = work_remaining(route_started.elapsed()).ok_or_else(preparation_timeout)?;
    let (model, model_version, account) = tokio::time::timeout(loading_budget, async {
        let mut conn = state
            .db
            .pool()
            .acquire()
            .await
            .map_err(crate::db::DbError::from)?;
        let row = store::get(&mut conn, Kind::Models, id)
            .await?
            .filter(|row| !row.deleted)
            .ok_or_else(ApiError::not_found)?;
        let model: Model = serde_json::from_value(row.doc)
            .map_err(|_| ApiError::internal("探测目标的模型配置无法读取"))?;
        let Some(pk_id) = model
            .provider_key_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            if unsupported_target(&model).is_some() {
                return Ok::<_, ApiError>((model, row.version, None));
            }
            return Err(ApiError::internal("探测目标缺少有效上游账号"));
        };
        let pk = store::get(&mut conn, Kind::ProviderKeys, pk_id)
            .await?
            .filter(|row| !row.deleted)
            .ok_or_else(ApiError::not_found)?;
        Ok((model, row.version, Some((pk_id, pk.version, pk.doc))))
    })
    .await
    .map_err(|_| preparation_timeout())??;
    if let Some(reason) = unsupported_target(&model) {
        let mut result = result.unsupported(reason);
        if body.capability_check {
            result.capability_check =
                CapabilityResult::unsupported("该目标不支持能力抽测，未追加模型请求");
        }
        let history_id = history::begin(
            &state.db,
            Snapshot {
                model_id: id,
                model_version,
                model_name: model.display_name,
                provider_key_id: account.as_ref().map(|(id, _, _)| *id),
                provider_key_version: account.as_ref().map(|(_, version, _)| *version),
                account_name: account
                    .as_ref()
                    .and_then(|(_, _, doc)| doc.get("display_name"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            },
            auth.id,
            body.capability_check,
            route_started + WORK_BUDGET,
        )
        .await?;
        return tokio::spawn(async move {
            history::save(
                &state.db,
                history_id,
                &result,
                true,
                route_started + TOTAL_BUDGET,
            )
            .await?;
            Ok(Json(result))
        })
        .await
        .map_err(|_| ApiError::internal("测试记录未能保存"))?;
    }
    let (pk_id, pk_version, mut pk_doc) =
        account.ok_or_else(|| ApiError::internal("探测目标缺少上游账号"))?;
    // 全满立即拒绝；等待慢上游时既不占数据库连接，也不积累无界等待队列。
    let permit = Arc::clone(&state.model_probes)
        .try_acquire_owned()
        .map_err(|_| ApiError {
            status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
            message: "模型测试服务繁忙，本次尚未调用上游".to_string(),
            ..ApiError::busy()
        })?;
    let account_name = pk_doc
        .get("display_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    resources::secrets::unseal(
        &state.cfg.master_key,
        Kind::ProviderKeys,
        pk_id,
        &mut pk_doc,
    )
    .map_err(ApiError::internal)?;
    let pk: ProviderKey = serde_json::from_value(pk_doc)
        .map_err(|_| ApiError::internal("探测目标的上游账号配置无法读取"))?;
    let history_id = history::begin(
        &state.db,
        Snapshot {
            model_id: id,
            model_version,
            model_name: model.display_name.clone(),
            provider_key_id: Some(pk_id),
            provider_key_version: Some(pk_version),
            account_name,
        },
        auth.id,
        body.capability_check,
        route_started + WORK_BUDGET,
    )
    .await?;
    // 已接受的调用独立完成并保存；客户端断连只停止等待，不重发也不取消上游。
    tokio::spawn(async move {
        let _permit = permit;
        execute_probe(
            state,
            model,
            (pk_id, pk),
            result,
            body.capability_check,
            route_started,
            history_id,
        )
        .await
    })
    .await
    .map_err(|_| ApiError::internal("测试执行中断，请从历史查看已保存结果"))?
}

async fn execute_probe(
    state: AppState,
    model: Model,
    (pk_id, pk): (Uuid, ProviderKey),
    result: ProbeResult,
    capability_check: bool,
    route_started: tokio::time::Instant,
    history_id: i64,
) -> Result<Json<ProbeResult>, ApiError> {
    let id = result.model_id;
    let tap = Arc::new(ResponseHeaderTap::new(
        Vec::new(),
        false,
        Uuid::new_v4().to_string(),
    ));
    let started = Instant::now();
    let mut observed = Observation::default();
    let mut result = result;
    let mut target = None;
    response_tap::scope(Arc::clone(&tap), async {
        response_tap::note_sent_credential(&pk.api_key);
        if let Some(base) = pk.api_base.as_deref() {
            response_tap::note_url_userinfo(base);
        }
        let Some(phase_timeout) = phase_budget(route_started.elapsed()) else {
            result.status = Status::Timeout;
            result.error = Some("本次探测可用工作预算已用完，未发送基础模型请求".to_string());
            return;
        };
        let attempt = async {
            let request = chat_request(&model);
            let prepared =
                providers::prepare(&state.upstream, &model, &request, &pk, phase_timeout).await?;
            let mut ctx = BridgeContext::new(
                Uuid::new_v4().to_string(),
                Arc::new(model.clone()),
                Arc::new(prepared.provider_key),
            )
            .with_deadline(phase_timeout);
            ctx.model_id = id.to_string();
            ctx.provider_key_id = pk_id.to_string();
            observed
                .run(prepared.bridge.as_ref(), &ctx, &request, started)
                .await
                .map_err(providers::PrepareError::Bridge)?;
            Ok::<_, providers::PrepareError>((prepared.bridge, ctx))
        };
        match tokio::time::timeout(phase_timeout, attempt).await {
            Ok(Ok(prepared)) => {
                result.status = Status::Available;
                target = Some(prepared);
            }
            Ok(Err(providers::PrepareError::Unsupported(reason))) => {
                result.status = Status::Unsupported;
                result.error = Some(safe_detail(&reason));
            }
            Ok(Err(providers::PrepareError::Failed(error))) => {
                result.error = Some(safe_detail(&format!(
                    "{}{}",
                    error.message,
                    error
                        .details
                        .first()
                        .map(|d| format!("：{}", d.message))
                        .unwrap_or_default()
                )));
            }
            Ok(Err(providers::PrepareError::Bridge(error))) => {
                if matches!(error, BridgeError::Timeout { .. }) {
                    result.status = Status::Timeout;
                } else if matches!(error, BridgeError::UnsupportedCapability(_)) {
                    result.status = Status::Unsupported;
                }
                result.http_status = match &error {
                    BridgeError::UpstreamStatus { status, .. }
                    | BridgeError::UpstreamBodyUnread { status, .. } => Some(*status),
                    // 流内错误的 status 是正文语义，不能冒充 HTTP 响应状态。
                    _ => None,
                };
                result.error = Some(safe_detail(&error.to_string()));
            }
            Err(_) => {
                result.status = Status::Timeout;
                result.error =
                    Some("基础探测已达到单次 30 秒或整次可用工作时限，已停止等待".to_string());
            }
        }
    })
    .await;
    result.total_duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    result.checked_at = Utc::now();
    result.ttft_ms = observed.ttft_ms;
    result.usage = observed.usage;
    if matches!(result.status, Status::Available)
        && !model.is_embedding()
        && result.ttft_ms.is_none()
    {
        result.note = Some("本次协议响应成功，但未返回可见正文，首字延迟无法测量；可能受短输出上限影响，其它模态能力未测试".to_string());
    }
    if matches!(result.status, Status::Available) && result.total_duration_ms > 0.0 {
        result.output_tokens_per_second = result
            .usage
            .as_ref()
            .and_then(|usage| usage.output_tokens)
            .map(|tokens| f64::from(tokens) * 1000.0 / result.total_duration_ms);
    }
    // 首次测量在此已固定；能力调用不计入基础状态、首字、用量或速率。
    if capability_check {
        result.capability_check =
            CapabilityResult::not_tested("基础结果已保存，能力抽测尚未取得最终结果");
    }
    history::save(
        &state.db,
        history_id,
        &result,
        !capability_check,
        route_started
            + if capability_check {
                WORK_BUDGET
            } else {
                TOTAL_BUDGET
            },
    )
    .await?;
    append_capability(
        &mut result,
        capability_check,
        model.is_embedding(),
        target,
        tap,
        route_started,
    )
    .await;
    if capability_check {
        history::save(
            &state.db,
            history_id,
            &result,
            true,
            route_started + TOTAL_BUDGET,
        )
        .await?;
    }
    Ok(Json(result))
}

async fn append_capability(
    result: &mut ProbeResult,
    enabled: bool,
    embedding: bool,
    target: Option<(Arc<dyn Bridge>, BridgeContext)>,
    tap: Arc<ResponseHeaderTap>,
    route_started: tokio::time::Instant,
) {
    if enabled {
        result.capability_check = if embedding {
            CapabilityResult::unsupported("向量模型不进行文本能力抽测，未追加模型请求")
        } else if !matches!(result.status, Status::Available) {
            CapabilityResult::not_tested("基础探测未通过，未追加能力模型请求")
        } else if result.ttft_ms.is_none() {
            CapabilityResult::not_tested(
                "基础协议成功但未返回可见正文，无法确认可文本抽测，未追加模型请求",
            )
        } else if let Some((bridge, ctx)) = target {
            match phase_budget(route_started.elapsed()) {
                Some(budget) => {
                    response_tap::scope(tap, check_capability(bridge.as_ref(), &ctx, budget)).await
                }
                None => {
                    CapabilityResult::not_tested("本次探测可用工作预算已用完，未追加能力模型请求")
                }
            }
        } else {
            CapabilityResult::not_tested("未取得可复用的直连目标，未追加模型请求")
        };
    }
}

fn work_remaining(elapsed: Duration) -> Option<Duration> {
    WORK_BUDGET
        .checked_sub(elapsed)
        .filter(|left| !left.is_zero())
}

fn phase_budget(elapsed: Duration) -> Option<Duration> {
    work_remaining(elapsed).map(|left| left.min(TIMEOUT))
}

fn preparation_timeout() -> ApiError {
    ApiError {
        message: "测试准备超出可用工作时限，本次未发送模型请求".to_string(),
        ..ApiError::unavailable()
    }
}

async fn check_capability(
    bridge: &dyn Bridge,
    ctx: &BridgeContext,
    budget: Duration,
) -> CapabilityResult {
    if budget.is_zero() {
        return CapabilityResult::not_tested("整次探测预算已用完，未追加模型请求");
    }
    let challenge = capability::Challenge::new();
    let mut request = chat_request(&ctx.model);
    request.max_tokens = Some(MAX_CAPABILITY_TOKENS);
    request.messages = vec![ChatMessage::user(challenge.prompt())];
    let started = Instant::now();
    let mut observed = Observation {
        text: Some(String::new()),
        ..Default::default()
    };
    let ctx = ctx.clone().with_deadline(budget);
    let outcome = tokio::time::timeout(budget, observed.run(bridge, &ctx, &request, started)).await;
    let mut result = CapabilityResult::not_tested(
        "单次短样本，不代表模型身份、完整能力或长期表现；本次输出上限 256 tokens",
    );
    result.total_duration_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    result.usage = observed.usage;
    match outcome {
        Ok(Ok(())) => {
            result.checks = challenge.evaluate(observed.text.as_deref().unwrap_or_default());
            result.status = if result.checks.all_passed() {
                CapabilityStatus::Passed
            } else {
                CapabilityStatus::Failed
            };
        }
        Ok(Err(error)) => {
            result.status = if matches!(error, BridgeError::Timeout { .. }) {
                CapabilityStatus::Timeout
            } else if matches!(error, BridgeError::UnsupportedCapability(_)) {
                CapabilityStatus::Unsupported
            } else {
                CapabilityStatus::Error
            };
            result.error = Some(safe_detail(&error.to_string()));
        }
        Err(_) => {
            result.status = CapabilityStatus::Timeout;
            result.error = Some("能力抽测已达到单次或整次探测时限，已停止等待".to_string());
        }
    }
    result
}

fn unsupported_target(model: &Model) -> Option<&'static str> {
    if model.is_routing() || model.is_ensemble() || model.is_semantic() {
        Some("请选择具体直连模型；组合模型的结果不能代表其中某个上游目标")
    } else if model.display_name.contains('*')
        || model.model_name.as_deref().is_some_and(|n| n.contains('*'))
    {
        Some("通配别名没有唯一上游模型，请配置具体直连目标后测试")
    } else {
        None
    }
}

fn safe_detail(detail: &str) -> String {
    // 先完整脱敏再截断，避免截掉凭据一半使共享脱敏器失配。
    aisix_gateway::upstream_error_scrub::scrub_upstream_error_text(detail)
        .chars()
        .take(500)
        .collect()
}

fn chat_request(model: &Model) -> ChatFormat {
    // 固定单个候选，防止账号默认 n 放大小请求预算。
    // https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create
    let mut extra = serde_json::Map::new();
    extra.insert("n".to_string(), serde_json::json!(1));
    ChatFormat {
        max_tokens: Some(MAX_TOKENS),
        stream: Some(true),
        extra,
        ..ChatFormat::new(model.display_name.clone(), vec![ChatMessage::user(PROMPT)])
    }
}

#[derive(Default)]
struct Observation {
    ttft_ms: Option<f64>,
    usage: Option<Usage>,
    finish: ChoiceFinishCensus,
    bytes: usize,
    chunks: usize,
    /// 只有能力抽测收集正文，原文从不进入响应或日志。
    text: Option<String>,
}

impl Observation {
    async fn run(
        &mut self,
        bridge: &dyn Bridge,
        ctx: &BridgeContext,
        request: &ChatFormat,
        started: Instant,
    ) -> Result<(), BridgeError> {
        if ctx.model.is_embedding() {
            let request = EmbeddingRequest {
                model: ctx.model.display_name.clone(),
                input: vec![PROMPT.to_string()],
                input_was_single: true,
                encoding_format: None,
                dimensions: None,
                input_type: None,
                embedding_types: None,
                token_input: None,
                extra: Default::default(),
            };
            let response = bridge.embed(&request, ctx).await?;
            if response.usage.source == EmbeddingUsageSource::Reported {
                self.usage = Some(Usage {
                    input_tokens: Some(response.usage.prompt_tokens),
                    output_tokens: None,
                });
            }
            let valid = response.data.len() == 1
                && response.data[0].index == 0
                && match &response.data[0].embedding {
                    EmbeddingVector::Float(vector) => {
                        !vector.is_empty() && vector.iter().all(|v| v.is_finite())
                    }
                    EmbeddingVector::Base64(vector) => !vector.is_empty(),
                };
            return if valid {
                Ok(())
            } else {
                Err(BridgeError::UpstreamDecode(
                    "向量探测没有返回有效向量".to_string(),
                ))
            };
        }
        let stream = bridge.chat_stream(request, ctx).await?;
        self.consume(stream, started).await
    }

    async fn consume(
        &mut self,
        mut stream: aisix_gateway::bridge::ChatChunkStream,
        started: Instant,
    ) -> Result<(), BridgeError> {
        while let Some(chunk) = stream.next().await {
            self.observe(&chunk?, started.elapsed())?;
        }
        self.complete()
    }

    fn observe(&mut self, chunk: &ChatChunk, elapsed: Duration) -> Result<(), BridgeError> {
        self.chunks += 1;
        self.bytes = self.bytes.saturating_add(
            serde_json::to_vec(chunk)
                .map_err(|_| BridgeError::Config("探测流计量失败".to_string()))?
                .len(),
        );
        if self.chunks > MAX_CHUNKS || self.bytes > MAX_OUTPUT_BYTES {
            return Err(BridgeError::UpstreamDecode(
                "本次探测超过输出大小限制".to_string(),
            ));
        }
        self.finish.observe(chunk);
        if chunk.choice_index == 0 {
            if let (Some(text), Some(delta)) = (self.text.as_mut(), chunk.delta.content.as_deref())
            {
                if text.len().saturating_add(delta.len()) > MAX_CAPABILITY_TEXT_BYTES {
                    return Err(BridgeError::UpstreamDecode(
                        "能力抽测超过正文大小限制".to_string(),
                    ));
                }
                text.push_str(delta);
            }
        }
        if self.ttft_ms.is_none()
            && chunk.choice_index == 0
            && chunk
                .delta
                .content
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
        {
            self.ttft_ms = Some(elapsed.as_secs_f64() * 1000.0);
        }
        if let Some(usage) = chunk.accounting_usage.as_ref() {
            let acc = self.usage.get_or_insert_with(Usage::default);
            acc.input_tokens = Some(
                acc.input_tokens
                    .unwrap_or(0)
                    .max(usage.openai_prompt_tokens()),
            );
        }
        if let Some(usage) = chunk.usage.as_ref() {
            let acc = self.usage.get_or_insert_with(Usage::default);
            acc.input_tokens = Some(
                acc.input_tokens
                    .unwrap_or(0)
                    .max(usage.openai_prompt_tokens()),
            );
            acc.output_tokens = Some(acc.output_tokens.unwrap_or(0).max(usage.completion_tokens));
        }
        Ok(())
    }

    fn complete(&self) -> Result<(), BridgeError> {
        if !self.finish.all_finished() {
            return Err(BridgeError::StreamAborted);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_gateway::chat::Role;
    use aisix_gateway::{ChatDelta, FinishReason, UsageStats};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct WaitingBridge(Arc<AtomicUsize>);

    struct FragmentedJsonBridge;

    #[async_trait::async_trait]
    impl Bridge for FragmentedJsonBridge {
        fn name(&self) -> &'static str {
            "unit-fragment-check"
        }

        async fn chat(
            &self,
            _: &ChatFormat,
            _: &BridgeContext,
        ) -> Result<aisix_gateway::ChatResponse, BridgeError> {
            panic!("能力抽测应读取流式正文");
        }

        async fn chat_stream(
            &self,
            request: &ChatFormat,
            _: &BridgeContext,
        ) -> Result<aisix_gateway::bridge::ChatChunkStream, BridgeError> {
            assert_eq!(request.max_tokens, Some(256));
            let first = chunk(ChatDelta {
                content: Some("{\"nonce\":\"deliberately-wrong\",".to_string()),
                ..Default::default()
            });
            let second = chunk(ChatDelta {
                content: Some("\"arithmetic\":-1,\"sorting\":[]}".to_string()),
                ..Default::default()
            });
            let mut finish = chunk(ChatDelta::default());
            finish.finish_reason = Some(FinishReason::Stop);
            finish.usage = Some(UsageStats::new(11, 5));
            Ok(Box::pin(futures::stream::iter([
                Ok(first),
                Ok(second),
                Ok(finish),
            ])))
        }
    }

    #[async_trait::async_trait]
    impl Bridge for WaitingBridge {
        fn name(&self) -> &'static str {
            "unit-budget-check"
        }

        async fn chat(
            &self,
            _: &ChatFormat,
            _: &BridgeContext,
        ) -> Result<aisix_gateway::ChatResponse, BridgeError> {
            panic!("能力抽测不应改用非流式请求");
        }

        async fn chat_stream(
            &self,
            request: &ChatFormat,
            ctx: &BridgeContext,
        ) -> Result<aisix_gateway::bridge::ChatChunkStream, BridgeError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            assert_eq!(request.max_tokens, Some(256));
            assert_eq!(request.extra.get("n"), Some(&serde_json::json!(1)));
            assert!(ctx.deadline.is_some_and(|d| d <= TIMEOUT));
            tokio::time::sleep(Duration::from_secs(100)).await;
            Err(BridgeError::StreamAborted)
        }
    }

    fn prepared_unit_target(calls: Arc<AtomicUsize>) -> (Arc<dyn Bridge>, BridgeContext) {
        let model = serde_json::from_value(serde_json::json!({"display_name":"budget-target"}))
            .expect("模型配置");
        let pk = serde_json::from_value(
            serde_json::json!({"display_name":"unit-account","api_key":"unused-unit-credential"}),
        )
        .expect("账号配置");
        (
            Arc::new(WaitingBridge(calls)),
            BridgeContext::new("unit", Arc::new(model), Arc::new(pk)),
        )
    }

    fn base_sample() -> ProbeResult {
        let mut result = ProbeResult::new(Uuid::new_v4());
        result.status = Status::Available;
        result.ttft_ms = Some(0.0);
        result.total_duration_ms = 17.0;
        result.usage = Some(Usage {
            input_tokens: Some(7),
            output_tokens: Some(3),
        });
        result.output_tokens_per_second = Some(3.0 * 1000.0 / 17.0);
        result
    }

    fn basic_fields(result: &ProbeResult) -> serde_json::Value {
        let mut value = serde_json::to_value(result).expect("探测响应");
        value
            .as_object_mut()
            .expect("响应对象")
            .remove("capability_check");
        value
    }

    #[test]
    fn both_phases_use_only_the_remaining_whole_handler_budget() {
        assert_eq!(phase_budget(Duration::ZERO), Some(Duration::from_secs(30)));
        assert_eq!(
            phase_budget(Duration::from_secs(29)),
            Some(Duration::from_secs(21))
        );
        assert_eq!(
            phase_budget(Duration::from_secs(49)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(phase_budget(Duration::from_secs(50)), None);
        assert_eq!(phase_budget(Duration::from_secs(56)), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_call_timeout_keeps_base_metrics_and_finishes_within_55_seconds() {
        let route_started = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_secs(29)).await;
        let mut result = base_sample();
        let original = basic_fields(&result);
        let calls = Arc::new(AtomicUsize::new(0));
        let tap = Arc::new(ResponseHeaderTap::new(Vec::new(), false, "unit".into()));
        append_capability(
            &mut result,
            true,
            false,
            Some(prepared_unit_target(Arc::clone(&calls))),
            tap,
            route_started,
        )
        .await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "真实零首字依然允许追加一次"
        );
        assert_eq!(route_started.elapsed(), WORK_BUDGET);
        assert_eq!(result.capability_check.status, CapabilityStatus::Timeout);
        assert_eq!(
            basic_fields(&result),
            original,
            "追加超时不能污染已完成的基础指标"
        );
    }

    // 实跑异步等待与保存门，不能只靠减法证明工作和最终保存共享截止。
    #[tokio::test(start_paused = true)]
    async fn model_work_and_persistence_gates_share_the_55_second_deadline() {
        let started = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(3)).await;
        history::within_budget(
            started + WORK_BUDGET,
            async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok::<_, sqlx::Error>(())
            },
            "初始保存门超时",
        )
        .await
        .expect("准备和初始保存消耗五秒");
        tokio::time::timeout(
            phase_budget(started.elapsed()).expect("基础预算"),
            tokio::time::sleep(Duration::from_secs(29)),
        )
        .await
        .expect("基础工作29秒");
        let mut result = base_sample();
        let original = basic_fields(&result);
        history::within_budget(
            started + WORK_BUDGET,
            async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                Ok::<_, sqlx::Error>(())
            },
            "基础保存门超时",
        )
        .await
        .expect("中间保存也消耗工作预算");
        let calls = Arc::new(AtomicUsize::new(0));
        append_capability(
            &mut result,
            true,
            false,
            Some(prepared_unit_target(Arc::clone(&calls))),
            Arc::new(ResponseHeaderTap::new(
                Vec::new(),
                false,
                "persist-budget".into(),
            )),
            started,
        )
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(50),
            "模型工作必须给最终保存留五秒"
        );
        assert_eq!(result.capability_check.status, CapabilityStatus::Timeout);
        assert_eq!(basic_fields(&result), original);
        history::within_budget(
            started + TOTAL_BUDGET,
            async {
                tokio::time::sleep(Duration::from_secs(4)).await;
                Ok::<_, sqlx::Error>(())
            },
            "最终保存门超时",
        )
        .await
        .expect("工作耗尽后仍有最终保存预算");
        assert_eq!(started.elapsed(), Duration::from_secs(54));
        let blocked = history::within_budget(
            started + TOTAL_BUDGET,
            std::future::pending::<Result<(), sqlx::Error>>(),
            "保存门持续阻塞",
        )
        .await;
        assert!(blocked.is_err());
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(55),
            "不足五秒时不能另开五秒等待"
        );
        let polls = AtomicUsize::new(0);
        assert!(history::within_budget(
            started + TOTAL_BUDGET,
            async {
                polls.fetch_add(1, Ordering::Relaxed);
                Ok::<_, sqlx::Error>(())
            },
            "预算耗尽"
        )
        .await
        .is_err());
        assert_eq!(
            polls.load(Ordering::Relaxed),
            0,
            "零预算不能 poll 数据库 future"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_budget_and_ineligible_results_never_poll_another_model_call() {
        for case in ["disabled", "no_text", "failed", "embedding", "exhausted"] {
            let route_started = tokio::time::Instant::now();
            let mut result = base_sample();
            if case == "no_text" {
                result.ttft_ms = None;
            }
            if case == "failed" {
                result.status = Status::Failed;
            }
            if case == "exhausted" {
                tokio::time::advance(TOTAL_BUDGET).await;
            }
            let original = basic_fields(&result);
            let calls = Arc::new(AtomicUsize::new(0));
            let tap = Arc::new(ResponseHeaderTap::new(Vec::new(), false, "unit".into()));
            append_capability(
                &mut result,
                case != "disabled",
                case == "embedding",
                Some(prepared_unit_target(Arc::clone(&calls))),
                tap,
                route_started,
            )
            .await;
            assert_eq!(
                calls.load(Ordering::Relaxed),
                0,
                "{case} 不得创建第二次请求"
            );
            assert_eq!(basic_fields(&result), original);
            assert_eq!(result.capability_check.total_duration_ms, None);
            assert_eq!(
                result
                    .capability_check
                    .usage
                    .as_ref()
                    .and_then(|u| u.output_tokens),
                None
            );
            assert_ne!(result.capability_check.status, CapabilityStatus::Passed);
        }
    }

    #[tokio::test]
    async fn capability_reads_fragmented_json_and_records_its_own_reported_usage() {
        let mut result = base_sample();
        let original = basic_fields(&result);
        let (_, ctx) = prepared_unit_target(Arc::new(AtomicUsize::new(0)));
        let bridge: Arc<dyn Bridge> = Arc::new(FragmentedJsonBridge);
        let tap = Arc::new(ResponseHeaderTap::new(
            Vec::new(),
            false,
            "unit-fragments".into(),
        ));
        append_capability(
            &mut result,
            true,
            false,
            Some((bridge, ctx)),
            tap,
            tokio::time::Instant::now(),
        )
        .await;
        assert_eq!(result.capability_check.status, CapabilityStatus::Failed);
        assert_eq!(result.capability_check.checks.json, Some(true));
        assert_eq!(result.capability_check.checks.instruction, Some(false));
        assert_eq!(result.capability_check.checks.arithmetic, Some(false));
        assert_eq!(result.capability_check.checks.sorting, Some(false));
        let extra = result
            .capability_check
            .usage
            .as_ref()
            .expect("额外调用实际报告 usage");
        assert_eq!(extra.input_tokens, Some(11));
        assert_eq!(extra.output_tokens, Some(5));
        assert_eq!(basic_fields(&result), original);
        assert!(
            !serde_json::to_string(&result)
                .expect("响应")
                .contains("deliberately-wrong"),
            "原始能力正文不应返回"
        );
    }

    fn chunk(delta: ChatDelta) -> ChatChunk {
        ChatChunk {
            id: "probe".to_string(),
            model: "target".to_string(),
            choice_index: 0,
            delta,
            finish_reason: None,
            logprobs: None,
            usage: None,
            service_tier: None,
            accounting_usage: None,
        }
    }

    #[test]
    fn account_defaults_cannot_multiply_the_short_probes_choices() {
        use aisix_provider_openai::wire::{build_request, messages_from, DeveloperRoleMode};
        let model: Model =
            serde_json::from_value(serde_json::json!({"display_name": "budget-probe"}))
                .expect("模型配置");
        let request = chat_request(&model);
        let messages = messages_from(&request, DeveloperRoleMode::Preserve);
        let mut wire =
            serde_json::to_value(build_request(&request, "exact-upstream", &messages, true))
                .expect("上游请求");
        let defaults = serde_json::json!({"n": 100, "max_tokens": 8192});
        aisix_provider_openai::overrides::apply_default_body_fields(
            &mut wire,
            defaults.as_object().expect("账号默认字段"),
        );
        assert_eq!(wire.get("n"), Some(&serde_json::json!(1)));
        assert_eq!(wire.get("max_tokens"), Some(&serde_json::json!(64)));
    }

    #[test]
    fn ttft_ignores_role_reasoning_whitespace_tools_and_usage() {
        let mut observed = Observation::default();
        for delta in [
            ChatDelta {
                role: Some(Role::Assistant),
                ..Default::default()
            },
            ChatDelta {
                reasoning_content: Some("thinking".into()),
                ..Default::default()
            },
            ChatDelta {
                content: Some(" \n".into()),
                ..Default::default()
            },
            ChatDelta {
                tool_calls: Some(vec![serde_json::json!({"function": {"arguments": "{}"}})]),
                ..Default::default()
            },
        ] {
            observed
                .observe(&chunk(delta), Duration::from_millis(10))
                .expect("启动帧有效");
            assert_eq!(observed.ttft_ms, None);
        }
        let mut usage_only = chunk(ChatDelta::default());
        usage_only.usage = Some(UsageStats::new(4, 0));
        observed
            .observe(&usage_only, Duration::from_millis(20))
            .expect("usage 有效");
        assert_eq!(observed.ttft_ms, None);
        observed
            .observe(
                &chunk(ChatDelta {
                    content: Some("OK".into()),
                    ..Default::default()
                }),
                Duration::from_millis(80),
            )
            .expect("正文有效");
        assert_eq!(observed.ttft_ms, Some(80.0));
        assert!(observed.complete().is_err(), "首字不能代替完整结束");
        let mut done = chunk(ChatDelta::default());
        done.finish_reason = Some(FinishReason::Stop);
        observed
            .observe(&done, Duration::from_millis(100))
            .expect("结束帧有效");
        assert!(observed.complete().is_ok());
    }

    #[test]
    fn missing_usage_stays_absent_and_partial_usage_does_not_invent_output() {
        let mut observed = Observation::default();
        observed
            .observe(
                &chunk(ChatDelta {
                    content: Some("OK".into()),
                    ..Default::default()
                }),
                Duration::from_millis(12),
            )
            .expect("正文有效");
        assert!(observed.usage.is_none());
        let mut partial = chunk(ChatDelta::default());
        partial.accounting_usage = Some(UsageStats::new(7, 0));
        observed
            .observe(&partial, Duration::from_millis(13))
            .expect("部分 usage 有效");
        assert_eq!(
            observed.usage.as_ref().and_then(|u| u.input_tokens),
            Some(7)
        );
        assert_eq!(observed.usage.as_ref().and_then(|u| u.output_tokens), None);
        partial.accounting_usage = None;
        partial.usage = Some(UsageStats::new(7, 3));
        observed
            .observe(&partial, Duration::from_millis(14))
            .expect("完整 usage 有效");
        observed
            .observe(&partial, Duration::from_millis(15))
            .expect("重复累计帧有效");
        assert_eq!(
            observed.usage.as_ref().and_then(|u| u.output_tokens),
            Some(3)
        );
    }

    #[test]
    fn a_finished_empty_stream_keeps_protocol_success_and_has_no_ttft() {
        let mut observed = Observation::default();
        let mut done = chunk(ChatDelta::default());
        done.finish_reason = Some(FinishReason::Stop);
        observed
            .observe(&done, Duration::from_millis(5))
            .expect("结束帧有效");
        assert!(observed.complete().is_ok());
        assert_eq!(observed.ttft_ms, None);
    }

    #[test]
    fn a_real_zero_measurement_stays_distinct_from_no_visible_text() {
        let mut observed = Observation::default();
        observed
            .observe(
                &chunk(ChatDelta {
                    content: Some("OK".into()),
                    ..Default::default()
                }),
                Duration::ZERO,
            )
            .expect("正文有效");
        assert_eq!(observed.ttft_ms, Some(0.0));
    }

    #[tokio::test]
    async fn an_error_after_text_and_finish_is_still_a_failed_probe() {
        let text = chunk(ChatDelta {
            content: Some("OK".into()),
            ..Default::default()
        });
        let mut done = chunk(ChatDelta::default());
        done.finish_reason = Some(FinishReason::Stop);
        let stream = futures::stream::iter([Ok(text), Ok(done), Err(BridgeError::StreamAborted)]);
        let mut observed = Observation::default();
        let result = observed.consume(Box::pin(stream), Instant::now()).await;
        assert!(matches!(result, Err(BridgeError::StreamAborted)));
        assert!(observed.ttft_ms.is_some(), "失败样本仍可保留实际测得的首字");
    }

    #[test]
    fn the_probe_stops_when_the_response_exceeds_its_size_bound() {
        let mut observed = Observation::default();
        let large = chunk(ChatDelta {
            content: Some("x".repeat(MAX_OUTPUT_BYTES + 1)),
            ..Default::default()
        });
        assert!(observed.observe(&large, Duration::from_millis(10)).is_err());
        assert_eq!(observed.ttft_ms, None);
    }
}
