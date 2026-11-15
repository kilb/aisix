//! 原生后台响应的持久观察器；租约和最终用量去重由控制面负责。

use std::time::Duration;

use aisix_core::native_objects::{
    configured_native_origin, ClaimRequest, CompleteRequest, LeaseRequest, LeaseToken,
    NativeObjectKind, NativeProtocol, NativeRecord, TerminalUsage,
};
use axum::http::{HeaderMap, HeaderValue};
use chrono::{DateTime, Utc};
#[cfg(test)]
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::watch;
use tracing::Instrument;

use super::registry::{NativeObjectRegistry, NativeRegistryError};
use crate::error::ProxyError;
use crate::state::ProxyState;

const CLAIM_SIZE: u32 = 8;
const MAX_POLL_TIME: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const PENDING_DELAY_MS: u32 = 2_000;
const ERROR_DELAY_MS: u32 = 30_000;

/// 服务启动一次；退出时丢弃未完成的读取，控制面租约到期后由别的进程接手。
pub async fn run(state: ProxyState, cancel: watch::Receiver<bool>) {
    let Some(registry) = state.native_objects.as_deref() else {
        return;
    };
    super::worker::run(
        registry,
        claim_request(None),
        |record| observe_record(&state, registry, record),
        cancel,
    )
    .await;
}

fn claim_request(record_id: Option<uuid::Uuid>) -> ClaimRequest {
    ClaimRequest {
        limit: if record_id.is_some() { 1 } else { CLAIM_SIZE },
        record_id,
        protocol: Some(NativeProtocol::Openai),
        kind: Some(NativeObjectKind::Response),
    }
}

/// 每批数量等于并发上限，不让刚领取的对象在队列里耗尽租期。
#[cfg(test)]
pub(crate) async fn run_once(state: &ProxyState) -> Result<usize, NativeRegistryError> {
    let registry = state
        .native_objects
        .as_ref()
        .ok_or(NativeRegistryError::Unavailable)?;
    let reply = registry.claim(claim_request(None)).await?;
    if reply.state != "leased" || reply.records.len() > CLAIM_SIZE as usize {
        return Err(NativeRegistryError::Unavailable);
    }
    let mut polls = futures::stream::iter(
        reply
            .records
            .into_iter()
            .map(|record| observe_record(state, registry.as_ref(), record)),
    )
    .buffer_unordered(CLAIM_SIZE as usize);
    let mut completed = 0;
    while let Some(result) = polls.next().await {
        match result {
            Ok(true) => completed += 1,
            Ok(false) => {}
            Err(_) => tracing::warn!("native response observation will be retried after its lease"),
        }
    }
    Ok(completed)
}

async fn observe_record(
    state: &ProxyState,
    registry: &dyn NativeObjectRegistry,
    record: NativeRecord,
) -> Result<bool, NativeRegistryError> {
    let span = tracing::info_span!("native_response_observe", record_id = %record.record_id);
    poll_record(state, registry, record).instrument(span).await
}

fn lease_of(record: &NativeRecord) -> Option<LeaseToken> {
    let generation = record
        .lease_generation
        .filter(|generation| *generation > 0)?;
    record.lease_expires_at?;
    Some(LeaseToken {
        record_id: record.record_id,
        generation,
    })
}

async fn postpone(
    registry: &dyn NativeObjectRegistry,
    lease: LeaseToken,
    delay: u32,
) -> Result<bool, NativeRegistryError> {
    let reply = registry
        .reschedule(LeaseRequest {
            lease,
            poll_after_ms: delay,
        })
        .await?;
    if !matches!(reply.state.as_str(), "pending" | "completed") {
        return Err(NativeRegistryError::Unavailable);
    }
    Ok(false)
}

async fn finish(
    registry: &dyn NativeObjectRegistry,
    terminal: CompleteRequest,
) -> Result<bool, NativeRegistryError> {
    // 网络读取有界；完成前再续期，避免边界上的旧租约提交。
    let renewed = registry
        .renew(LeaseRequest {
            lease: terminal.lease.clone(),
            poll_after_ms: 0,
        })
        .await?;
    if renewed.state == "completed" {
        return Ok(false);
    }
    if renewed.state != "leased" {
        return Err(NativeRegistryError::Unavailable);
    }
    let reply = registry.complete(terminal).await?;
    if reply.state != "completed" {
        return Err(NativeRegistryError::Unavailable);
    }
    Ok(true)
}

async fn poll_record(
    state: &ProxyState,
    registry: &dyn NativeObjectRegistry,
    record: NativeRecord,
) -> Result<bool, NativeRegistryError> {
    let lease = lease_of(&record).ok_or(NativeRegistryError::Unavailable)?;
    match retrieve(state, &record).await {
        Ok(response) => match terminal_observation(&record, &response, Utc::now()) {
            Some(terminal) => finish(registry, terminal).await,
            None => postpone(registry, lease, PENDING_DELAY_MS).await,
        },
        Err(_) => {
            tracing::warn!("native response observation unavailable; accounting remains pending");
            postpone(registry, lease, ERROR_DELAY_MS).await
        }
    }
}

/// JSON 或 SSE 观察到终态时共用同一租约；读取已记账对象不会再次产生用量。
pub(crate) async fn observe(
    state: &ProxyState,
    record: &NativeRecord,
    response: &Value,
) -> Result<(), ProxyError> {
    if terminal_status(response).is_none()
        || response.get("id").and_then(Value::as_str) != Some(record.raw_id.as_str())
    {
        return Ok(());
    }
    let Some(registry) = state.native_objects.as_ref() else {
        return Ok(());
    };
    let reply = registry
        .claim(claim_request(Some(record.record_id)))
        .await
        .map_err(|_| ProxyError::Bridge(aisix_gateway::BridgeError::DependencyUnavailable))?;
    if reply.state != "leased" || reply.records.len() > 1 {
        return Err(ProxyError::Bridge(
            aisix_gateway::BridgeError::DependencyUnavailable,
        ));
    }
    let Some(claimed) = reply.records.first() else {
        // 同步对象、已完成对象或另一个观察器持有的对象均不重复记账。
        return Ok(());
    };
    if claimed.record_id != record.record_id
        || claimed.origin != record.origin
        || claimed.raw_id != record.raw_id
    {
        return Err(ProxyError::Bridge(
            aisix_gateway::BridgeError::DependencyUnavailable,
        ));
    }
    let terminal = terminal_observation(claimed, response, Utc::now()).ok_or(
        ProxyError::Bridge(aisix_gateway::BridgeError::DependencyUnavailable),
    )?;
    finish(registry.as_ref(), terminal)
        .await
        .map_err(|_| ProxyError::Bridge(aisix_gateway::BridgeError::DependencyUnavailable))?;
    Ok(())
}

async fn retrieve(state: &ProxyState, record: &NativeRecord) -> Result<Value, NativeRegistryError> {
    let unavailable = || NativeRegistryError::Unavailable;
    if record.protocol != Some(NativeProtocol::Openai)
        || record.kind != Some(NativeObjectKind::Response)
    {
        return Err(unavailable());
    }
    crate::jobs::require_safe_upstream_id(&record.raw_id).map_err(|_| unavailable())?;
    let binding = record.origin.as_ref().ok_or_else(unavailable)?;
    let platform = state.platform().ok_or_else(unavailable)?;
    let snapshot = state.snapshot.load();
    let provider = super::openai::provider_entry_by_id(state, &snapshot, binding.provider_key_id)
        .map_err(|_| unavailable())?;
    let current = configured_native_origin(
        &platform.id_key,
        binding.provider_key_id,
        &provider.value,
        NativeProtocol::Openai,
        NativeObjectKind::Response,
    )
    .map_err(|_| unavailable())?;
    if current != binding.boundary {
        return Err(unavailable());
    }
    let base =
        crate::dispatch::resolve_base_url_for(&provider.value, aisix_core::ApiSurface::Responses)
            .map_err(|_| unavailable())?;
    let mut url = url::Url::parse(&base).map_err(|_| unavailable())?;
    let mut base_path = url.path().trim_end_matches('/').to_owned();
    for suffix in ["/responses", "/chat/completions", "/completions"] {
        if let Some(stripped) = base_path.strip_suffix(suffix) {
            base_path = stripped.to_owned();
            break;
        }
    }
    if base_path.is_empty() {
        base_path.push_str("/v1");
    }
    // path 单独设置，配置 URL 的 query 和 userinfo 保持原样。
    url.set_path(&format!("{base_path}/responses/{}", record.raw_id));
    // 已受理的任务沿用账号边界，模型撤销或改路由不妨碍原账号的用量观察。
    let secret = provider.value.api_key.as_str();
    if secret.is_empty() {
        return Err(unavailable());
    }
    aisix_gateway::note_sent_credential(secret);
    aisix_gateway::note_url_userinfo(&base);
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| unavailable())?,
    );
    headers.insert(
        axum::http::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    aisix_gateway::apply_request_headers(
        &mut headers,
        &aisix_gateway::UpstreamHeaderContext::from_overrides(provider.value.request.as_ref()),
    );
    let timeout = state
        .default_timeouts
        .request
        .unwrap_or(MAX_POLL_TIME)
        .min(MAX_POLL_TIME);
    let client = crate::http_client::client_for(provider.value.upstream_connection().as_ref());
    let read = async {
        let mut response = aisix_gateway::upstream_io::send(
            client.get(url).headers(headers).timeout(timeout),
            aisix_gateway::upstream_io::Protocol::Unknown,
        )
        .await
        .map_err(|_| unavailable())?;
        if !response.status().is_success() {
            // 404 和其他失败不是零用量终态；保留待处理记录，不跟随重定向。
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| unavailable())
    };
    tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| unavailable())?
}

fn terminal_status(response: &Value) -> Option<&str> {
    match response.get("status")?.as_str()? {
        status @ ("completed" | "incomplete" | "failed" | "cancelled" | "expired") => Some(status),
        _ => None,
    }
}

fn response_completed_at(response: &Value) -> Option<DateTime<Utc>> {
    // 完成时间是 Unix 秒数，可含小数；复用终态 JSON，不以创建时间代替。
    // https://developers.openai.com/api/reference/resources/responses/methods/retrieve.md
    let seconds = response.get("completed_at")?.as_f64()?;
    let whole_seconds = seconds.floor();
    DateTime::from_timestamp(
        whole_seconds as i64,
        ((seconds - whole_seconds) * 1_000_000_000.0) as u32,
    )
}

fn terminal_observation(
    record: &NativeRecord,
    response: &Value,
    observed_at: DateTime<Utc>,
) -> Option<CompleteRequest> {
    if record.protocol != Some(NativeProtocol::Openai)
        || record.kind != Some(NativeObjectKind::Response)
        || response.get("id").and_then(Value::as_str) != Some(record.raw_id.as_str())
    {
        return None;
    }
    let status = terminal_status(response)?;
    // 供应商报告的完成时间优先；未报告时保留观察时刻的既有回退。
    let occurred_at = if status == "completed" {
        response_completed_at(response).unwrap_or(observed_at)
    } else {
        observed_at
    };
    let usage = response.get("usage");
    let count = |path: &str| {
        usage
            .and_then(|usage| usage.pointer(path))
            .and_then(Value::as_u64)
            .map(crate::usage_attr::token_count)
    };
    let missing_usage = usage.is_none_or(Value::is_null);
    // 完成的付费推理必须报告计数；缺失不能永久登记成零用量。
    if (missing_usage && matches!(status, "completed" | "incomplete"))
        || (!missing_usage
            && (!usage.is_some_and(Value::is_object)
                || count("/input_tokens").is_none()
                || count("/output_tokens").is_none()))
    {
        return None;
    }
    let modality = aisix_gateway::ModalityTokens::from_openai_details(
        usage.and_then(|usage| usage.get("input_tokens_details")),
        usage.and_then(|usage| usage.get("output_tokens_details")),
    );
    Some(CompleteRequest {
        output_file_ids: Vec::new(),
        lease: lease_of(record)?,
        terminal_status: status.to_owned(),
        usage: vec![TerminalUsage {
            io_audit: Some(terminal_io(
                aisix_gateway::upstream_io::Protocol::Responses,
                record.upstream_input_text,
                response,
                status == "completed",
            )),
            terminal_status: None,
            error_class: String::new(),
            error_message: String::new(),
            entry_id: String::new(),
            occurred_at,
            status_code: match status {
                "failed" | "expired" => 502,
                "cancelled" => 499,
                _ => 200,
            },
            prompt_tokens: count("/input_tokens").unwrap_or(0),
            completion_tokens: count("/output_tokens").unwrap_or(0),
            cached_prompt_tokens: count("/input_tokens_details/cached_tokens").unwrap_or(0),
            cache_write_tokens: count("/input_tokens_details/cache_write_tokens"),
            cache_usage_reported: Some(
                count("/input_tokens_details/cached_tokens").is_some()
                    || count("/input_tokens_details/cache_write_tokens").is_some(),
            ),
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            reasoning_tokens: count("/output_tokens_details/reasoning_tokens").unwrap_or(0),
            total_tokens: crate::usage_attr::reported_total_tokens(usage).unwrap_or(0),
            prompt_audio_tokens: modality.prompt_audio_tokens,
            prompt_image_tokens: modality.prompt_image_tokens,
            completion_audio_tokens: modality.completion_audio_tokens,
            completion_image_tokens: modality.completion_image_tokens,
            web_search_requests: crate::responses::reported_response_web_search_requests(response)
                .unwrap_or(0),
            usage_estimated: false,
            service_tier: response
                .get("service_tier")
                .and_then(Value::as_str)
                .filter(|tier| !tier.is_empty())
                .map(str::to_owned),
            requested_service_tier: record.requested_service_tier.clone(),
        }],
    })
}

/// 终态结果只补充模型文本；检索与文件下载不是模型传输，不能充当正文长度。
pub(super) fn terminal_io(
    protocol: aisix_gateway::upstream_io::Protocol,
    input_text: Option<aisix_core::usage_io::TextMeasurement>,
    response: &Value,
    complete: bool,
) -> aisix_core::usage_io::UsageIoAudit {
    let io = aisix_gateway::upstream_io::UpstreamIoObserver::new(protocol);
    if let Some(input) = input_text {
        io.observe_input_measurement(input);
    }
    io.observe_response_json(response);
    if complete {
        io.finish_response();
    } else {
        io.fail_response();
    }
    let mut audit = io.snapshot();
    audit.request_complete = None;
    audit
}

#[cfg(test)]
#[path = "background_tests.rs"]
mod tests;
