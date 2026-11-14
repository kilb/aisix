//! 原生批次观察器按 custom_id 完整收集终态，再交给控制面原子去重。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_core::native_objects::{
    configured_native_origin, ClaimRequest, CompleteRequest, LeaseRequest, LeaseToken, NativeEntry,
    NativeObjectKind, NativeProtocol, NativeRecord, TerminalUsage,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::watch;
use tracing::Instrument;

use super::registry::{NativeObjectRegistry, NativeRegistryError};
use crate::state::ProxyState;

const CLAIM_SIZE: u32 = 8;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(300);
const RENEW_INTERVAL: Duration = Duration::from_secs(5);
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_RESULT_LINE_BYTES: usize = aisix_gateway::sse::MAX_SSE_FRAME_BYTES;
const PENDING_DELAY_MS: u32 = 2_000;
const ERROR_DELAY_MS: u32 = 30_000;
const MAX_ENTRIES: usize = 1000;

fn unavailable() -> NativeRegistryError {
    NativeRegistryError::Unavailable
}

pub async fn run(state: ProxyState, mut cancel: watch::Receiver<bool>) {
    if state.native_objects.is_none() || *cancel.borrow() {
        return;
    }
    let mut ticks = tokio::time::interval(POLL_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() { return; }
            }
            _ = ticks.tick() => {
                tokio::select! {
                    biased;
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() { return; }
                    }
                    result = run_once(&state) => {
                        if result.is_err() {
                            tracing::warn!("native batch observation authority unavailable");
                        }
                    }
                }
            }
        }
    }
}

fn claim_request(record_id: Option<uuid::Uuid>) -> ClaimRequest {
    ClaimRequest {
        limit: if record_id.is_some() { 1 } else { CLAIM_SIZE },
        record_id,
        protocol: Some(NativeProtocol::Anthropic),
        kind: Some(NativeObjectKind::MessageBatch),
    }
}

/// 每个 claim 批次有同等并发；未读完的租约由控制面收回，进程不持有唯一状态。
pub async fn run_once(state: &ProxyState) -> Result<usize, NativeRegistryError> {
    let registry = state.native_objects.as_ref().ok_or_else(unavailable)?;
    let reply = registry.claim(claim_request(None)).await?;
    if reply.state != "leased" || reply.records.len() > CLAIM_SIZE as usize {
        return Err(unavailable());
    }
    let mut jobs = futures::stream::iter(reply.records.into_iter().map(|record| {
        let span = tracing::info_span!("native_batch_observe", record_id = %record.record_id);
        let tap = Arc::new(aisix_gateway::ResponseHeaderTap::default());
        aisix_gateway::response_tap::scope(tap, poll_record(state, registry.as_ref(), record))
            .instrument(span)
    }))
    .buffer_unordered(CLAIM_SIZE as usize);
    let mut completed = 0;
    while let Some(result) = jobs.next().await {
        match result {
            Ok(true) => completed += 1,
            Ok(false) => {}
            Err(_) => tracing::warn!("native batch observation will resume after its lease"),
        }
    }
    Ok(completed)
}

fn lease_of(record: &NativeRecord) -> Option<LeaseToken> {
    record.lease_expires_at?;
    Some(LeaseToken {
        record_id: record.record_id,
        generation: record
            .lease_generation
            .filter(|generation| *generation > 0)?,
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
        return Err(unavailable());
    }
    Ok(false)
}

async fn renew(
    registry: &dyn NativeObjectRegistry,
    lease: &LeaseToken,
) -> Result<bool, NativeRegistryError> {
    let reply = registry
        .renew(LeaseRequest {
            lease: lease.clone(),
            poll_after_ms: 0,
        })
        .await?;
    match reply.state.as_str() {
        "completed" => Ok(false),
        "leased" => Ok(true),
        _ => Err(unavailable()),
    }
}

async fn poll_record(
    state: &ProxyState,
    registry: &dyn NativeObjectRegistry,
    record: NativeRecord,
) -> Result<bool, NativeRegistryError> {
    let lease = lease_of(&record).ok_or_else(unavailable)?;
    let observed = tokio::time::timeout(
        OBSERVATION_TIMEOUT,
        collect_terminal(state, registry, &record, &lease),
    )
    .await
    .map_err(|_| unavailable())
    .and_then(|result| result);
    match observed {
        Ok(Some(terminal)) => {
            if !renew(registry, &lease).await? {
                return Ok(false);
            }
            let reply = registry.complete(terminal).await?;
            if reply.state != "completed" {
                return Err(unavailable());
            }
            Ok(true)
        }
        Ok(None) => postpone(registry, lease, PENDING_DELAY_MS).await,
        Err(_) => {
            tracing::warn!("native batch observation unavailable; accounting remains pending");
            postpone(registry, lease, ERROR_DELAY_MS).await
        }
    }
}

/// 删除前必须持久完成逐项计量；其他观察器持有租约时让调用方稍后重试。
pub(crate) async fn settle_before_delete(
    state: &ProxyState,
    record: &NativeRecord,
) -> Result<(), NativeRegistryError> {
    if record.completed {
        return Ok(());
    }
    let registry = state.native_objects.as_ref().ok_or_else(unavailable)?;
    let reply = registry
        .claim(claim_request(Some(record.record_id)))
        .await?;
    if reply.state != "leased" || reply.records.len() != 1 {
        return Err(unavailable());
    }
    let claimed = reply.records.into_iter().next().ok_or_else(unavailable)?;
    if claimed.record_id != record.record_id
        || claimed.raw_id != record.raw_id
        || claimed.origin != record.origin
    {
        return Err(unavailable());
    }
    let tap = Arc::new(aisix_gateway::ResponseHeaderTap::default());
    match aisix_gateway::response_tap::scope(tap, poll_record(state, registry.as_ref(), claimed))
        .await?
    {
        true => Ok(()),
        false => Err(unavailable()),
    }
}

struct Upstream {
    client: reqwest::Client,
    base: url::Url,
    headers: HeaderMap,
}

fn upstream(state: &ProxyState, record: &NativeRecord) -> Result<Upstream, NativeRegistryError> {
    if record.protocol != Some(NativeProtocol::Anthropic)
        || record.kind != Some(NativeObjectKind::MessageBatch)
        || record.entries.is_empty()
        || record.entries.len() > MAX_ENTRIES
    {
        return Err(unavailable());
    }
    crate::jobs::require_safe_upstream_id(&record.raw_id).map_err(|_| unavailable())?;
    let binding = record.origin.as_ref().ok_or_else(unavailable)?;
    let platform = state.platform().ok_or_else(unavailable)?;
    let snapshot = state.snapshot.load();
    let provider = snapshot
        .provider_keys
        .get_by_id(&binding.provider_key_id.to_string())
        .ok_or_else(unavailable)?;
    let boundary = configured_native_origin(
        &platform.id_key,
        binding.provider_key_id,
        &provider.value,
        NativeProtocol::Anthropic,
        NativeObjectKind::MessageBatch,
    )
    .map_err(|_| unavailable())?;
    if boundary != binding.boundary {
        return Err(unavailable());
    }
    let base =
        crate::dispatch::resolve_base_url_for(&provider.value, aisix_core::ApiSurface::Messages)
            .map_err(|_| unavailable())?;
    aisix_gateway::note_url_userinfo(&base);
    let base = url::Url::parse(&base).map_err(|_| unavailable())?;
    // 已发生推理的结算只绑定原账号；模型被删除或改指向后也不能丢掉费用。
    let secret = provider.value.api_key.as_str();
    if secret.is_empty() {
        return Err(unavailable());
    }
    aisix_gateway::note_sent_credential(secret);
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_str(secret).map_err(|_| unavailable())?,
    );
    for (name, value) in &record.observation_headers {
        if !matches!(name.as_str(), "anthropic-version" | "anthropic-beta") {
            return Err(unavailable());
        }
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| unavailable())?,
            HeaderValue::from_str(value).map_err(|_| unavailable())?,
        );
    }
    if !headers.contains_key("anthropic-version") {
        return Err(unavailable());
    }
    aisix_gateway::apply_request_headers(
        &mut headers,
        &aisix_gateway::UpstreamHeaderContext::from_overrides(provider.value.request.as_ref()),
    );
    Ok(Upstream {
        client: crate::http_client::client_for(provider.value.upstream_connection().as_ref()),
        base,
        headers,
    })
}

impl Upstream {
    fn url(&self, raw_id: &str, results: bool) -> url::Url {
        let mut url = self.base.clone();
        let path = url.path().trim_end_matches('/');
        let root = path
            .strip_suffix("/v1/messages")
            .or_else(|| path.strip_suffix("/v1"))
            .unwrap_or(path);
        url.set_path(&format!(
            "{root}/v1/messages/batches/{raw_id}{}",
            if results { "/results" } else { "" }
        ));
        url
    }
    async fn get(
        &self,
        raw_id: &str,
        results: bool,
    ) -> Result<reqwest::Response, NativeRegistryError> {
        let response = tokio::time::timeout(
            HEAD_TIMEOUT,
            aisix_gateway::upstream_io::send(
                self.client
                    .get(self.url(raw_id, results))
                    .headers(self.headers.clone()),
                aisix_gateway::upstream_io::Protocol::Unknown,
            ),
        )
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
        aisix_gateway::note_upstream_response(response.headers());
        if !response.status().is_success() {
            return Err(unavailable());
        }
        Ok(response)
    }
}

async fn collect_terminal(
    state: &ProxyState,
    registry: &dyn NativeObjectRegistry,
    record: &NativeRecord,
    lease: &LeaseToken,
) -> Result<Option<CompleteRequest>, NativeRegistryError> {
    let upstream = upstream(state, record)?;
    let mut response = upstream.get(&record.raw_id, false).await?;
    let bytes = tokio::time::timeout(HEAD_TIMEOUT, async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if chunk.len() > MAX_METADATA_BYTES.saturating_sub(bytes.len()) {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| unavailable())??;
    let metadata: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
    if metadata.get("id").and_then(Value::as_str) != Some(record.raw_id.as_str()) {
        return Err(unavailable());
    }
    match metadata.get("processing_status").and_then(Value::as_str) {
        Some("in_progress" | "canceling") => return Ok(None),
        Some("ended") => {}
        _ => return Err(unavailable()),
    }
    let observed_at = metadata
        .get("ended_at")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    if !renew(registry, lease).await? {
        return Ok(None);
    }
    // results_url 是提供方描述，不作为凭据请求目的地；始终读取绑定账号上的固定路径。
    let mut results = upstream.get(&record.raw_id, true).await?;
    let mut parser = ResultLines::new(&record.entries, observed_at)?;
    let mut renewed_at = Instant::now();
    loop {
        if renewed_at.elapsed() >= RENEW_INTERVAL {
            if !renew(registry, lease).await? {
                return Ok(None);
            }
            renewed_at = Instant::now();
        }
        let chunk = tokio::time::timeout(HEAD_TIMEOUT, results.chunk())
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        let Some(chunk) = chunk else {
            break;
        };
        parser.push(&chunk)?;
    }
    Ok(Some(CompleteRequest {
        output_file_ids: Vec::new(),
        lease: lease.clone(),
        terminal_status: "ended".into(),
        usage: parser.finish()?,
    }))
}

struct ResultLines<'a> {
    expected: BTreeMap<&'a str, &'a NativeEntry>,
    seen: BTreeSet<String>,
    pending: Vec<u8>,
    usage: Vec<TerminalUsage>,
    observed_at: DateTime<Utc>,
}

impl<'a> ResultLines<'a> {
    fn new(
        entries: &'a [NativeEntry],
        observed_at: DateTime<Utc>,
    ) -> Result<Self, NativeRegistryError> {
        let expected: BTreeMap<_, _> = entries
            .iter()
            .map(|entry| (entry.entry_id.as_str(), entry))
            .collect();
        if expected.is_empty() || expected.len() != entries.len() || expected.len() > MAX_ENTRIES {
            return Err(unavailable());
        }
        Ok(Self {
            expected,
            seen: BTreeSet::new(),
            pending: Vec::new(),
            usage: Vec::new(),
            observed_at,
        })
    }
    fn push(&mut self, mut bytes: &[u8]) -> Result<(), NativeRegistryError> {
        while !bytes.is_empty() {
            let end = memchr::memchr(b'\n', bytes);
            let take = end.map_or(bytes.len(), |index| index + 1);
            if take > MAX_RESULT_LINE_BYTES.saturating_sub(self.pending.len()) {
                return Err(unavailable());
            }
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if end.is_some() {
                self.line()?;
            }
        }
        Ok(())
    }
    fn line(&mut self) -> Result<(), NativeRegistryError> {
        let bytes = std::mem::take(&mut self.pending);
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let line: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let id = line
            .get("custom_id")
            .and_then(Value::as_str)
            .ok_or_else(unavailable)?;
        let entry = self.expected.get(id).ok_or_else(unavailable)?;
        if !self.seen.insert(id.to_owned()) {
            return Err(unavailable());
        }
        let result = line.get("result").ok_or_else(unavailable)?;
        let kind = result
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(unavailable)?;
        let usage = match kind {
            "succeeded" => {
                let message = result.get("message").ok_or_else(unavailable)?;
                let usage = message
                    .get("usage")
                    .filter(|usage| usage.is_object())
                    .ok_or_else(unavailable)?;
                // 成功项缺失或畸形必需计数不能伪造成零用量成功。
                for field in ["input_tokens", "output_tokens"] {
                    usage
                        .get(field)
                        .and_then(Value::as_u64)
                        .ok_or_else(unavailable)?;
                }
                let mut row = terminal_usage(
                    entry,
                    usage,
                    self.observed_at,
                    200,
                    String::new(),
                    String::new(),
                );
                row.io_audit = Some(super::background::terminal_io(
                    aisix_gateway::upstream_io::Protocol::Messages,
                    entry.upstream_input_text,
                    message,
                    true,
                ));
                row
            }
            "errored" => {
                let error = result.pointer("/error/error").ok_or_else(unavailable)?;
                let kind = error
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(unavailable)?;
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(unavailable)?;
                let status = error_status(kind);
                terminal_usage(
                    entry,
                    &Value::Null,
                    self.observed_at,
                    status,
                    "upstream_status".into(),
                    aisix_gateway::truncate_lossy(
                        &aisix_gateway::scrub_upstream_error_text(message),
                        4093,
                    ),
                )
            }
            "canceled" => terminal_usage(
                entry,
                &Value::Null,
                self.observed_at,
                499,
                String::new(),
                "Batch entry canceled".into(),
            ),
            "expired" => terminal_usage(
                entry,
                &Value::Null,
                self.observed_at,
                504,
                "timeout".into(),
                "Batch entry expired".into(),
            ),
            _ => return Err(unavailable()),
        };
        self.usage.push(usage);
        Ok(())
    }
    fn finish(mut self) -> Result<Vec<TerminalUsage>, NativeRegistryError> {
        if !self.pending.is_empty() {
            self.line()?;
        }
        if self.seen.len() != self.expected.len() {
            return Err(unavailable());
        }
        Ok(self.usage)
    }
}

fn terminal_usage(
    entry: &NativeEntry,
    usage: &Value,
    observed_at: DateTime<Utc>,
    status_code: u16,
    error_class: String,
    error_message: String,
) -> TerminalUsage {
    let stats = aisix_provider_anthropic::wire::anthropic_usage_stats(usage);
    let occurred_at = observed_at;
    TerminalUsage {
        io_audit: None,
        entry_id: entry.entry_id.clone(),
        occurred_at,
        terminal_status: Some(
            match status_code {
                200..=299 => "completed",
                499 => "cancelled",
                504 if error_class == "timeout" => "expired",
                _ => "failed",
            }
            .into(),
        ),
        status_code,
        error_class,
        error_message,
        prompt_tokens: stats.prompt_tokens,
        completion_tokens: stats.completion_tokens,
        cached_prompt_tokens: 0,
        cache_write_tokens: None,
        cache_usage_reported: Some(stats.cache_usage_reported().unwrap_or(false)),
        cache_creation_tokens: stats.cache_creation_tokens,
        cache_read_tokens: stats.cache_read_tokens,
        cache_creation_5m_tokens: stats.cache_creation_ttl.cache_creation_5m_tokens,
        cache_creation_1h_tokens: stats.cache_creation_ttl.cache_creation_1h_tokens,
        reasoning_tokens: stats.reasoning_tokens,
        total_tokens: stats.upstream_total_tokens,
        prompt_audio_tokens: None,
        prompt_image_tokens: None,
        completion_audio_tokens: None,
        completion_image_tokens: None,
        web_search_requests: stats.web_search_requests,
        usage_estimated: false,
        service_tier: aisix_gateway::chat::anthropic_usage_service_tier(usage),
        requested_service_tier: entry.requested_service_tier.clone(),
    }
}

/// 批次错误项没有 HTTP 状态，沿用原协议错误类型的标准状态。
pub(super) fn error_status(kind: &str) -> u16 {
    match kind {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        _ => 500,
    }
}

#[cfg(test)]
mod upstream_io_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recovered_message_batch_preserves_partial_input_and_actual_output() {
        let entries: Vec<NativeEntry> = serde_json::from_value(json!([{
            "entry_id":"actual","requested_model_id":"11111111-1111-4111-8111-111111111111",
            "model_id":"11111111-1111-4111-8111-111111111111","upstream_model":"native-model",
            "upstream_input_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":false}
        },{
            "entry_id":"canceled","requested_model_id":"11111111-1111-4111-8111-111111111111",
            "model_id":"11111111-1111-4111-8111-111111111111","upstream_model":"native-model"
        }]))
        .unwrap();
        let mut parser = ResultLines::new(&entries, chrono::Utc::now()).unwrap();
        let succeeded = json!({"custom_id":"actual","result":{"type":"succeeded","message":{
            "type":"message","content":[{"type":"text","text":"B界🙂"}],
            "usage":{"input_tokens":71,"output_tokens":23}}}});
        let canceled = json!({"custom_id":"canceled","result":{"type":"canceled"}});
        let wire = format!("{succeeded}\n{canceled}\n");
        for chunk in wire.as_bytes().chunks(7) {
            parser.push(chunk).unwrap();
        }
        let rows = parser.finish().unwrap();
        let audit = rows[0].io_audit.as_ref().expect("actual message result");
        assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (71, 23));
        assert_eq!(audit.request_body_bytes, None);
        assert_eq!(audit.response_body_bytes, None);
        assert_eq!(audit.request_complete, None);
        assert_eq!(
            audit.input_text,
            Some(aisix_core::usage_io::TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: false,
            })
        );
        assert_eq!(
            audit.output_text,
            Some(aisix_core::usage_io::TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true,
            })
        );
        assert_eq!(rows[1].io_audit, None);
    }
}
