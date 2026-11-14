//! 显式 start 之后的一次性工作器；数据库许可先于网络，不重放失联的预留。

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_gateway::response_tap::{self, ResponseHeaderTap};
use aisix_gateway::{BridgeContext, BridgeError, ChatFormat};
use chrono::Utc;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

use super::contracts::*;
use super::history::{self, ClaimedRun, Reservation, ReserveOutcome};
use super::{collector, scoring, target};
use crate::error::ApiError;
use crate::model_probes::providers;
use crate::AppState;

// 125ms 周期与125ms数据库上限使远端取消/失联检查不会超过250ms才中断本地I/O。
const CONTROL_INTERVAL: Duration = Duration::from_millis(125);
const CONTROL_TIMEOUT: Duration = Duration::from_millis(125);
const LOCAL_CONCURRENCY: usize = 2;

pub fn spawn(state: AppState, run_id: Uuid) {
    tokio::spawn(async move {
        if execute(state, run_id).await.is_err() {
            // 错误对象可能包含配置或上游文字；这里只记录作业标识。
            tracing::warn!(%run_id, "能力对照工作器停止，请查看持久记录");
        }
    });
}

async fn execute(state: AppState, run_id: Uuid) -> Result<(), ApiError> {
    let Some(claimed) = history::claim_run(&state, run_id).await? else {
        return Ok(());
    };
    let claimed = Arc::new(claimed);
    let mut control = watch_control(state.clone(), Arc::clone(&claimed));
    let mut active: FuturesUnordered<BoxFuture<'static, Result<(), ApiError>>> =
        FuturesUnordered::new();
    let mut tick = tokio::time::interval(CONTROL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let stop = 'work: loop {
        if Utc::now() >= claimed.deadline_at {
            break CompletionReason::Deadline;
        }
        while active.len() < LOCAL_CONCURRENCY {
            let Ok(permit) = Arc::clone(&state.model_probes).try_acquire_owned() else {
                break;
            };
            let reservation_started = Instant::now();
            let pending_reservation =
                tokio::time::timeout(CONTROL_TIMEOUT, history::reserve_next(&state, &claimed));
            tokio::pin!(pending_reservation);
            // 在途派发可能持有数据库锁；等待新预留时也必须继续驱动它们提交。
            // 固定同一预留future，不能因另一次派发完成而丢弃、重开可能已提交的预留。
            let reservation = loop {
                tokio::select! {
                    biased;
                    _ = control.changed() => break 'work (*control.borrow()).unwrap_or(CompletionReason::OwnerLost),
                    result = &mut pending_reservation => break result,
                    result = active.next(), if !active.is_empty() => {
                        if result.is_some_and(|result| result.is_err()) {
                            break 'work CompletionReason::OwnerLost;
                        }
                    }
                }
            };
            match reservation {
                Ok(Ok(ReserveOutcome::Reserved(reservation))) => {
                    let state = state.clone();
                    let claimed = Arc::clone(&claimed);
                    active.push(Box::pin(async move {
                        let started = Instant::now();
                        let result = dispatch(state, claimed, reservation, permit).await;
                        if result.is_err() {
                            tracing::warn!(%run_id, cause="dispatch_persistence_failed", elapsed_ms=started.elapsed().as_millis() as u64, "能力对照持久化操作失败");
                        }
                        result
                    }));
                }
                Ok(Ok(ReserveOutcome::Wait)) => break,
                Ok(Ok(ReserveOutcome::Done)) if active.is_empty() => {
                    history::complete_run(&state, &claimed).await?;
                    return Ok(());
                }
                Ok(Ok(ReserveOutcome::Done)) => break,
                Ok(Ok(ReserveOutcome::Stopped(reason))) => break 'work reason,
                // 超时不证明事务未提交；既不续领，也不重放可能已保留的sample。
                Ok(Err(_)) => {
                    tracing::warn!(%run_id, cause="reserve_failed", elapsed_ms=reservation_started.elapsed().as_millis() as u64, "能力对照预留停止");
                    break 'work CompletionReason::OwnerLost;
                }
                Err(_) => {
                    tracing::warn!(%run_id, cause="reserve_timeout", elapsed_ms=reservation_started.elapsed().as_millis() as u64, "能力对照预留停止");
                    break 'work CompletionReason::OwnerLost;
                }
            }
        }
        tokio::select! {
            biased;
            _ = control.changed() => break (*control.borrow()).unwrap_or(CompletionReason::OwnerLost),
            _ = tick.tick() => {},
            result = active.next(), if !active.is_empty() => {
                if result.is_some_and(|result| result.is_err()) { break CompletionReason::OwnerLost }
            }
        }
    };
    // 先drop本地等待，再持久终态。未知执行的slot由history保留到原deadline+60s。
    drop(active);
    history::stop_run(&state, &claimed, stop).await
}

fn watch_control(
    state: AppState,
    claimed: Arc<ClaimedRun>,
) -> tokio::sync::watch::Receiver<Option<CompletionReason>> {
    let (sender, receiver) = tokio::sync::watch::channel(None);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CONTROL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = sender.closed() => return,
                _ = tick.tick() => {},
            }
            let check_started = Instant::now();
            let check = async {
                tokio::try_join!(
                    history::control(&state, &claimed),
                    history::heartbeat(&state, &claimed)
                )
            };
            let reason = match tokio::time::timeout(CONTROL_TIMEOUT, check).await {
                Ok(Ok((Some(reason), _))) => Some(reason),
                Ok(Ok((None, true))) => None,
                Ok(Ok((None, false))) => Some(CompletionReason::OwnerLost),
                Ok(Err(_)) => {
                    let run_id = claimed.run.summary.id;
                    tracing::warn!(%run_id, cause="control_failed", elapsed_ms=check_started.elapsed().as_millis() as u64, "能力对照控制检查停止");
                    Some(CompletionReason::OwnerLost)
                }
                Err(_) => {
                    let run_id = claimed.run.summary.id;
                    tracing::warn!(%run_id, cause="control_or_heartbeat_timeout", elapsed_ms=check_started.elapsed().as_millis() as u64, "能力对照控制检查停止");
                    Some(CompletionReason::OwnerLost)
                }
            };
            if let Some(reason) = reason {
                let _ = sender.send(Some(reason));
                return;
            }
        }
    });
    receiver
}

async fn dispatch(
    state: AppState,
    claimed: Arc<ClaimedRun>,
    reservation: Reservation,
    _permit: OwnedSemaphorePermit,
) -> Result<(), ApiError> {
    let started = Instant::now();
    let tap = Arc::new(ResponseHeaderTap::new(
        vec![],
        false,
        reservation.sample.id.to_string(),
    ));
    let body_limit = aisix_gateway::upstream_io::ResponseBodyLimit::new(u64::from(
        claimed.run.limits.max_response_bytes_per_request,
    ));
    aisix_gateway::upstream_io::scope_response_body_limit(
        Some(body_limit.clone()),
        response_tap::scope(Arc::clone(&tap), async {
            let mut sample = reservation.sample.clone();
            let mut answer = None;
            let mut additive_cache = false;
            let budget = remaining(reservation.dispatch_deadline_at);
            let outcome = async {
                let dispatch_target = target::for_dispatch(&state, &reservation.target)
                    .await
                    .map_err(DispatchError::Configuration)?;
                let request = request(
                    &reservation.task,
                    &dispatch_target.model,
                    &claimed.run.limits,
                )
                .map_err(DispatchError::Configuration)?;
                response_tap::note_sent_credential(&dispatch_target.account.api_key);
                if let Some(base) = &dispatch_target.account.api_base {
                    response_tap::note_url_userinfo(base);
                }
                let native = providers::native_protocol(
                    &dispatch_target.model,
                    &request,
                    &dispatch_target.account,
                )
                .map_err(DispatchError::Prepare)?;
                additive_cache = matches!(native, "anthropic.messages" | "bedrock.converse");
                sample.effective_request = effective_request(
                    &reservation,
                    native,
                    claimed.run.limits.max_output_tokens_per_request,
                )?;
                let prepared = providers::prepare(
                    &state.upstream,
                    &dispatch_target.model,
                    &request,
                    &dispatch_target.account,
                    remaining(reservation.dispatch_deadline_at),
                )
                .await
                .map_err(DispatchError::Prepare)?;
                // 许可CAS与cancel锁同一run，只有仍属于本owner的预留能进入Bridge。
                if !history::mark_sent(&state, &reservation)
                    .await
                    .map_err(DispatchError::Persistence)?
                {
                    return Err(DispatchError::Cancelled);
                }
                let mut ctx = BridgeContext::new(
                    reservation.sample.id.to_string(),
                    Arc::new(dispatch_target.model),
                    Arc::new(prepared.provider_key),
                )
                .with_deadline(remaining(reservation.dispatch_deadline_at));
                ctx.model_id = reservation.target.model_id.to_string();
                ctx.provider_key_id = dispatch_target.account_id.to_string();
                let response = prepared.bridge.chat_stream(&request, &ctx).await;
                let known_sent =
                    response.is_ok() || response.as_ref().err().is_some_and(has_http_status);
                if known_sent {
                    if !history::record_sent(&state, &reservation)
                        .await
                        .map_err(DispatchError::Persistence)?
                    {
                        return Err(DispatchError::Cancelled);
                    }
                    sample.dispatch_state = DispatchState::Sent;
                    sample.sent_at = Some(Utc::now());
                }
                let stream = response.map_err(|error| {
                    DispatchError::Collect(collector::CollectError::Bridge(error))
                })?;
                collector::collect(
                    stream,
                    &tap,
                    claimed.run.limits.max_response_bytes_per_request as usize,
                    additive_cache,
                    &mut sample.usage,
                )
                .await
                .map_err(DispatchError::Collect)
            };
            let result = if budget.is_zero() {
                drop(outcome);
                Err(None)
            } else {
                tokio::time::timeout(budget, outcome)
                    .await
                    .map_err(|_| None)
                    .and_then(|result| result.map_err(Some))
            };
            let result = if body_limit.exceeded() {
                Err(Some(DispatchError::Collect(
                    collector::CollectError::Truncated,
                )))
            } else {
                result
            };
            sample.duration_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            sample.finished_at = Some(Utc::now());
            match result {
                Ok(collected) => {
                    // 严格先评分后脱敏，不能让凭据替换改变判断。原答案从不进入DB或日志。
                    let (grade, safe, scrubbed) =
                        grade_then_scrub(&reservation.task, &collected.answer);
                    let evidence_size = serde_json::to_vec(&safe)
                        .map_err(|_| ApiError::internal("能力对照证据无法编码"))?
                        .len();
                    if evidence_size > claimed.run.limits.max_response_bytes_per_request as usize {
                        failed(&mut sample, ErrorKind::Truncated, "evidence_limit", false);
                    } else {
                        sample.dispatch_state = DispatchState::Finished;
                        sample.result = grade.result;
                        sample.score = grade.score;
                        sample.finish_reason = Some(collected.finish_reason);
                        sample.usage = collected.usage;
                        sample.output_bytes = Some(collected.output_bytes);
                        sample.evidence.checks = grade.checks;
                        sample.evidence.expected_hash = grade.expected_hash;
                        sample.evidence.answer_preview = Some(collector::preview(&safe));
                        sample.evidence.answer_scrubbed = scrubbed;
                        answer = Some(safe);
                    }
                }
                Err(None) => failed(
                    &mut sample,
                    ErrorKind::Timeout,
                    "请求达到固定绝对时限，远端执行可能继续",
                    true,
                ),
                Err(Some(DispatchError::Cancelled)) => failed(
                    &mut sample,
                    ErrorKind::Cancelled,
                    "能力对照已取消或发送许可已失效",
                    true,
                ),
                Err(Some(DispatchError::Persistence(error))) => return Err(error),
                Err(Some(DispatchError::Configuration(error))) => {
                    let changed = matches!(
                        error.code,
                        "configuration_changed" | "not_found" | "validation_failed"
                    );
                    if !changed {
                        return Err(error);
                    }
                    failed(
                        &mut sample,
                        ErrorKind::ConfigurationChanged,
                        "模型或账号配置已变化，未替换为新配置",
                        false,
                    );
                    history::finish_sample(&state, &reservation, sample, None).await?;
                    history::stop_run(&state, &claimed, CompletionReason::ConfigurationChanged)
                        .await?;
                    return Ok(());
                }
                Err(Some(DispatchError::Prepare(providers::PrepareError::Unsupported(reason)))) => {
                    failed(&mut sample, ErrorKind::Decode, &reason, false);
                    sample.result = SampleResult::Unsupported;
                    sample.error_kind = None;
                }
                Err(Some(DispatchError::Prepare(providers::PrepareError::Failed(error)))) => {
                    failed(&mut sample, ErrorKind::Transport, &error.message, false);
                }
                Err(Some(DispatchError::Prepare(providers::PrepareError::Bridge(error))))
                | Err(Some(DispatchError::Collect(collector::CollectError::Bridge(error)))) => {
                    bridge_failure(&mut sample, &error)
                }
                Err(Some(DispatchError::Collect(collector::CollectError::Truncated))) => failed(
                    &mut sample,
                    ErrorKind::Truncated,
                    "回答超过固定采集上限或以长度限制结束，未对截断答案评分",
                    true,
                ),
                Err(Some(DispatchError::Collect(collector::CollectError::Decode))) => failed(
                    &mut sample,
                    ErrorKind::Decode,
                    "未得到完整可解码的单个答案",
                    true,
                ),
            }
            collector::retain_reported_usage(
                &mut sample.usage,
                collector::partial_usage(&tap, additive_cache),
            );
            history::finish_sample(&state, &reservation, sample, answer).await?;
            Ok(())
        }),
    )
    .await
}

enum DispatchError {
    Configuration(ApiError),
    Persistence(ApiError),
    Prepare(providers::PrepareError),
    Collect(collector::CollectError),
    Cancelled,
}

fn grade_then_scrub(
    task: &TaskSnapshot,
    answer: &NormalizedAnswer,
) -> (scoring::SampleGrade, NormalizedAnswer, bool) {
    let grade = scoring::grade(task, answer);
    let (safe, scrubbed) = collector::scrub_answer(answer);
    (grade, safe, scrubbed)
}

impl From<ApiError> for DispatchError {
    fn from(error: ApiError) -> Self {
        Self::Persistence(error)
    }
}

fn request(
    task: &TaskSnapshot,
    model: &aisix_core::Model,
    limits: &Limits,
) -> Result<ChatFormat, ApiError> {
    let bytes = serde_json::to_vec(&task.task.normalized_task)
        .map_err(|_| ApiError::internal("能力对照题目无法编码"))?;
    if bytes.len() > limits.max_input_bytes_per_request as usize
        || bytes.len() != task.input_bytes as usize
    {
        return Err(ApiError::conflict(
            "configuration_changed",
            "冻结题目超出固定输入限额",
        ));
    }
    let mut value = task.task.normalized_task.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| ApiError::internal("能力对照题目不是对象"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "messages" | "tools" | "tool_choice" | "response_format"
        )
    }) {
        return Err(ApiError::internal("能力对照题目携带不允许的请求字段"));
    }
    object.insert(
        "model".into(),
        serde_json::Value::String(model.display_name.clone()),
    );
    let mut request: ChatFormat = serde_json::from_value(value)
        .map_err(|_| ApiError::internal("能力对照题目不是合法聊天任务"))?;
    request.max_tokens = Some(limits.max_output_tokens_per_request);
    request.stream = Some(true);
    // 题库和调用端都不能隐式发送采样参数；cap交给现有Bridge写目标平台的槽。
    if request.temperature.is_some()
        || request.top_p.is_some()
        || request.extra.contains_key("seed")
    {
        return Err(ApiError::internal("能力对照题目携带不允许的采样配置"));
    }
    request.extra.insert("n".into(), serde_json::json!(1));
    request.extra.insert(
        "stream_options".into(),
        serde_json::json!({"include_usage":true}),
    );
    let size = serde_json::to_vec(&request)
        .map_err(|_| ApiError::internal("能力对照请求无法编码"))?
        .len();
    if size > limits.max_input_bytes_per_request as usize {
        return Err(ApiError::conflict(
            "configuration_changed",
            "能力对照请求超出固定输入限额",
        ));
    }
    Ok(request)
}

fn effective_request(
    reservation: &Reservation,
    native: &str,
    output_cap: u32,
) -> Result<EffectiveRequest, ApiError> {
    let configuration = serde_json::to_vec(&(
        &reservation.task.request_semantic_hash,
        &reservation.target.model_id,
        &reservation.target.model_version,
        &reservation.target.account_id,
        &reservation.target.provider_key_version,
        &reservation.target.adapter,
        native,
        output_cap,
        true,
        "upstream_defaults",
    ))
    .map_err(|_| ApiError::internal("能力对照配置摘要无法编码"))?;
    Ok(EffectiveRequest {
        native_protocol: native.into(),
        adapter_build: format!(
            "{}:{}",
            env!("CARGO_PKG_VERSION"),
            &*aisix_core::BUILD_VERSION
        ),
        stream: true,
        output_cap,
        sampling_policy: "upstream_defaults".into(),
        semantic_configuration_hash: hex::encode(Sha256::digest(configuration)),
    })
}

fn remaining(deadline: chrono::DateTime<Utc>) -> Duration {
    (deadline - Utc::now())
        .to_std()
        .unwrap_or(Duration::ZERO)
        .min(Duration::from_secs(30))
}

fn failed(sample: &mut AssessmentSample, kind: ErrorKind, detail: &str, uncertain: bool) {
    sample.dispatch_state = if uncertain {
        if sample.sent_at.is_some() {
            DispatchState::AbortedAfterSend
        } else {
            DispatchState::InterruptedUnknown
        }
    } else {
        DispatchState::Finished
    };
    sample.result = SampleResult::Error;
    sample.score = None;
    sample.error_kind = Some(kind);
    sample.error = Some(collector::safe_error(detail));
}

fn bridge_failure(sample: &mut AssessmentSample, error: &BridgeError) {
    let (status, unread) = match error {
        BridgeError::UpstreamStatus {
            status, diagnostic, ..
        } => (Some(*status), diagnostic.is_some()),
        BridgeError::UpstreamBodyUnread { status, .. } => (Some(*status), true),
        _ => (None, false),
    };
    let kind = match status {
        Some(401 | 403) => ErrorKind::Auth,
        Some(429) => ErrorKind::RateLimit,
        Some(_) => ErrorKind::UpstreamHttp,
        _ => match error {
            BridgeError::Timeout { .. } => ErrorKind::Timeout,
            BridgeError::UpstreamDecode(_) | BridgeError::StreamAborted => ErrorKind::Decode,
            _ => ErrorKind::Transport,
        },
    };
    // 已读完的HTTP错误是确定终态；只有状态头不能证明错误体已读完。
    failed(sample, kind, &error.to_string(), status.is_none() || unread);
    sample.http_status = status;
    if matches!(error, BridgeError::UnsupportedCapability(_)) {
        sample.result = SampleResult::Unsupported;
        sample.error_kind = None;
    }
}

fn has_http_status(error: &BridgeError) -> bool {
    match error {
        BridgeError::UpstreamStatus { wire, .. } => {
            *wire != aisix_gateway::UpstreamWire::CredentialEndpoint
        }
        BridgeError::UpstreamBodyUnread { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sent_sample() -> AssessmentSample {
        AssessmentSample {
            id: Uuid::new_v4(),
            ordinal: 0,
            pair_id: Uuid::new_v4().to_string(),
            cluster_id: "component.http_error".into(),
            dimension: Dimension::Protocol,
            role: Role::Candidate,
            request_semantic_hash: "semantic".into(),
            task_hash: "task".into(),
            input_bytes: 1,
            dispatch_state: DispatchState::Sent,
            result: SampleResult::NotRun,
            error_kind: None,
            owner_generation: Some(Uuid::new_v4()),
            dispatch_deadline_at: Some(Utc::now() + chrono::Duration::seconds(30)),
            sent_at: Some(Utc::now()),
            finished_at: None,
            http_status: None,
            finish_reason: None,
            score: None,
            usage: Usage::default(),
            duration_ms: None,
            output_bytes: None,
            evidence: Evidence {
                checks: vec![],
                expected_hash: "expected".into(),
                answer_preview: None,
                answer_scrubbed: false,
            },
            effective_request: EffectiveRequest {
                native_protocol: "openai.chat_completions".into(),
                adapter_build: "component".into(),
                stream: true,
                output_cap: 1024,
                sampling_policy: "upstream_defaults".into(),
                semantic_configuration_hash: "configuration".into(),
            },
            error: None,
        }
    }

    #[test]
    fn complete_http_400_and_500_are_known_errors_not_transport_or_unsupported() {
        for status in [400, 500] {
            let mut sample = sent_sample();
            bridge_failure(
                &mut sample,
                &BridgeError::upstream_status(status, "complete error"),
            );
            assert_eq!(sample.dispatch_state, DispatchState::Finished);
            assert_eq!(sample.result, SampleResult::Error);
            assert_eq!(sample.score, None);
            assert_eq!(sample.http_status, Some(status));
            // 使用序列化合同使旧产品可编译，并在错误分类的实际值上变红。
            assert_eq!(
                serde_json::to_value(sample.error_kind).unwrap(),
                json!("upstream_http")
            );
            assert_eq!(
                serde_json::to_value(sample.usage).unwrap(),
                json!({
                    "input_tokens":null, "output_tokens":null, "upstream_total_tokens":null,
                    "cached_prompt_tokens":null, "cache_read_tokens":null,
                    "cache_creation_tokens":null, "reasoning_tokens":null
                })
            );
        }
    }

    #[test]
    fn complete_auth_and_rate_limit_http_errors_keep_their_existing_categories() {
        for (status, expected) in [
            (401, ErrorKind::Auth),
            (403, ErrorKind::Auth),
            (429, ErrorKind::RateLimit),
        ] {
            let mut sample = sent_sample();
            bridge_failure(
                &mut sample,
                &BridgeError::upstream_status(status, "complete error"),
            );
            assert_eq!(sample.dispatch_state, DispatchState::Finished);
            assert_eq!(sample.result, SampleResult::Error);
            assert_eq!(sample.error_kind, Some(expected));
            assert_eq!(sample.http_status, Some(status));
        }
    }

    #[test]
    fn unread_http_error_bodies_are_uncertain_even_when_the_status_is_known() {
        for status in [400, 401, 403, 429, 500] {
            for error in [
                BridgeError::UpstreamBodyUnread {
                    status,
                    decode_error: "incomplete body".into(),
                    reason: "connection closed".into(),
                },
                BridgeError::upstream_status(status, "incomplete body")
                    .with_body_read_error(Some("connection closed".into())),
            ] {
                let mut sample = sent_sample();
                bridge_failure(&mut sample, &error);
                assert_eq!(sample.http_status, Some(status));
                assert_eq!(
                    sample.dispatch_state,
                    DispatchState::AbortedAfterSend,
                    "未读完的HTTP错误体不能释放未知执行槽"
                );
                assert_eq!(sample.result, SampleResult::Error);
                assert_eq!(sample.score, None);
            }
        }
    }

    #[test]
    fn real_transport_timeout_and_interrupted_streams_remain_uncertain() {
        for error in [
            BridgeError::Transport("connection closed".into()),
            BridgeError::Timeout {
                elapsed_ms: 30_000,
                cause: String::new(),
            },
            BridgeError::StreamAborted,
            BridgeError::UpstreamDecode("incomplete stream".into()),
        ] {
            let mut sample = sent_sample();
            bridge_failure(&mut sample, &error);
            assert_eq!(sample.dispatch_state, DispatchState::AbortedAfterSend);
            assert_eq!(sample.result, SampleResult::Error);
            assert_eq!(sample.score, None);
            assert_eq!(sample.http_status, None);
        }
    }

    fn task(value: serde_json::Value) -> TaskSnapshot {
        let input_bytes = serde_json::to_vec(&value).unwrap().len() as u32;
        TaskSnapshot {
            dimension: Dimension::Reasoning,
            request_semantic_hash: "semantic".into(),
            input_bytes,
            task: ExportTask {
                pair_id: Uuid::new_v4().to_string(),
                cluster_id: "one".into(),
                task_hash: "task".into(),
                normalized_task: value,
                expected: json!(42),
                grader_config: GraderConfig {
                    id: "exact".into(),
                    version: "1".into(),
                    build: "1".into(),
                    config: json!({}),
                },
            },
        }
    }

    #[test]
    fn frozen_request_has_one_candidate_fixed_cap_and_upstream_sampling_defaults() {
        let model = serde_json::from_value(
            json!({"display_name":"exact-resource", "model_name":"exact-upstream"}),
        )
        .unwrap();
        let request = request(
            &task(json!({"messages":[{"role":"user","content":"Return 42"}]})),
            &model,
            &Mode::Quick.limits(),
        )
        .unwrap();
        assert_eq!(request.max_tokens, Some(1024));
        assert_eq!(request.extra["n"], 1);
        assert!(request.temperature.is_none());
        assert!(request.top_p.is_none());
        assert!(!request.extra.contains_key("seed"));
        assert_eq!(request.model, "exact-resource");
    }

    #[test]
    fn stale_input_size_sampling_and_deadlines_cannot_silently_extend_work() {
        let model = serde_json::from_value(
            json!({"display_name":"exact-resource", "model_name":"exact-upstream"}),
        )
        .unwrap();
        let mut stale = task(json!({"messages":[]}));
        stale.input_bytes += 1;
        assert!(request(&stale, &model, &Mode::Quick.limits()).is_err());
        assert!(request(
            &task(json!({"messages":[], "seed":7})),
            &model,
            &Mode::Quick.limits()
        )
        .is_err());
        assert!(request(
            &task(json!({"messages":[], "temperature":0})),
            &model,
            &Mode::Quick.limits()
        )
        .is_err());
        assert_eq!(
            remaining(Utc::now() - chrono::Duration::seconds(1)),
            Duration::ZERO
        );
    }

    #[tokio::test]
    async fn grading_uses_original_memory_answer_before_credential_scrubbing() {
        let tap = Arc::new(ResponseHeaderTap::new(
            vec![],
            false,
            "grade-before-scrub".into(),
        ));
        response_tap::scope(tap, async {
            response_tap::note_sent_credential("unit-secret-credential");
            let mut task = task(json!({"messages":[]}));
            task.task.expected = json!("unit-secret-credential");
            task.task.grader_config = GraderConfig {
                id: "exact_text".into(),
                version: "1".into(),
                build: "capability-v1-grader-1".into(),
                config: json!({"whitespace":"significant","tool_calls":"forbidden"}),
            };
            let original = NormalizedAnswer {
                text: "unit-secret-credential".into(),
                tool_calls: vec![],
            };
            let (grade, safe, scrubbed) = grade_then_scrub(&task, &original);
            assert_eq!(grade.result, SampleResult::Pass);
            assert_eq!(grade.score, Some(1));
            assert!(scrubbed);
            assert!(!safe.text.contains("unit-secret-credential"));
            assert_eq!(
                scoring::grade(&task, &safe).result,
                SampleResult::Fail,
                "先脱敏再评分必须使此反证变红"
            );
        })
        .await;
    }

    #[test]
    fn a_credential_http_status_is_not_evidence_that_model_was_sent() {
        let model_error =
            BridgeError::upstream_status_with_retry_after(503, "model unavailable", None);
        assert!(has_http_status(&model_error));
        let credential_error = model_error.answered_by_credential_endpoint();
        assert!(
            !has_http_status(&credential_error),
            "token endpoint回包不能增加模型known-sent计数"
        );
    }
}
