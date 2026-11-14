//! 原生批次观察器按 custom_id 完整收集终态，再交给控制面原子去重。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_core::native_objects::{
    configured_native_origin, ClaimRequest, CompleteRequest, LeaseRequest, LeaseToken, NativeEntry,
    NativeObjectKind, NativeProtocol, NativeRecord, TerminalUsage,
};
use axum::http::{HeaderMap, HeaderValue};
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
        protocol: Some(NativeProtocol::Openai),
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
pub(crate) async fn settle_record(
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
    if record.protocol != Some(NativeProtocol::Openai)
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
    let provider = super::openai::provider_entry_by_id(state, &snapshot, binding.provider_key_id)
        .map_err(|_| unavailable())?;
    let boundary = configured_native_origin(
        &platform.id_key,
        binding.provider_key_id,
        &provider.value,
        NativeProtocol::Openai,
        NativeObjectKind::MessageBatch,
    )
    .map_err(|_| unavailable())?;
    if boundary != binding.boundary {
        return Err(unavailable());
    }
    // 结算绑定已付费请求的原账号；不依赖仍然存在的模型或客户端密钥。
    let raw_base = provider
        .value
        .api_base
        .as_deref()
        .unwrap_or("https://api.openai.com/v1");
    let mut base = url::Url::parse(raw_base).map_err(|_| unavailable())?;
    let path = base.path().trim_end_matches('/');
    let path = ["/chat/completions", "/responses", "/completions"]
        .iter()
        .find_map(|suffix| path.strip_suffix(suffix))
        .unwrap_or(path);
    let path = if path.is_empty() { "/v1" } else { path }.to_owned();
    base.set_path(&path);
    aisix_gateway::note_url_userinfo(base.as_str());
    let secret = provider.value.api_key.as_str();
    if secret.is_empty() {
        return Err(unavailable());
    }
    aisix_gateway::note_sent_credential(secret);
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| unavailable())?,
    );
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
    async fn get(&self, path: &str) -> Result<reqwest::Response, NativeRegistryError> {
        let mut url = self.base.clone();
        url.set_path(&format!(
            "{}{}",
            self.base.path().trim_end_matches('/'),
            path
        ));
        let response = tokio::time::timeout(
            HEAD_TIMEOUT,
            aisix_gateway::upstream_io::send(
                self.client.get(url).headers(self.headers.clone()),
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
    let mut response = upstream.get(&format!("/batches/{}", record.raw_id)).await?;
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
    let status = metadata
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(unavailable)?;
    match status {
        "validating" | "in_progress" | "finalizing" | "cancelling" => return Ok(None),
        "completed" | "failed" | "expired" | "cancelled" => {}
        _ => return Err(unavailable()),
    }
    let observed_at = ["completed_at", "failed_at", "expired_at", "cancelled_at"]
        .iter()
        .find_map(|field| {
            metadata
                .get(field)
                .and_then(Value::as_i64)
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        })
        .unwrap_or_else(Utc::now);
    let mut parser = ResultLines::new(&record.entries, observed_at)?;
    let mut files = BTreeSet::new();
    for field in ["output_file_id", "error_file_id"] {
        let Some(raw) = metadata.get(field).filter(|value| !value.is_null()) else {
            continue;
        };
        let raw = raw.as_str().ok_or_else(unavailable)?;
        crate::jobs::require_safe_upstream_id(raw).map_err(|_| unavailable())?;
        if !files.insert(raw.to_owned()) {
            return Err(unavailable());
        }
        if !renew(registry, lease).await? {
            return Ok(None);
        }
        let mut response = upstream.get(&format!("/files/{raw}/content")).await?;
        let mut renewed_at = Instant::now();
        loop {
            if renewed_at.elapsed() >= RENEW_INTERVAL {
                if !renew(registry, lease).await? {
                    return Ok(None);
                }
                renewed_at = Instant::now();
            }
            let chunk = tokio::time::timeout(HEAD_TIMEOUT, response.chunk())
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
            let Some(chunk) = chunk else {
                break;
            };
            parser.push(&chunk)?;
        }
        // 文件尾也是一条完整 JSONL 边界，不允许下一份文件补全残缺记录。
        parser.flush()?;
    }
    if let Some(counts) = metadata.get("request_counts") {
        let completed = counts
            .get("completed")
            .and_then(Value::as_u64)
            .ok_or_else(unavailable)?;
        let failed = counts
            .get("failed")
            .and_then(Value::as_u64)
            .ok_or_else(unavailable)?;
        if completed.checked_add(failed) != Some(parser.seen.len() as u64) {
            return Err(unavailable());
        }
    }
    Ok(Some(CompleteRequest {
        output_file_ids: files.into_iter().collect(),
        lease: lease.clone(),
        terminal_status: status.into(),
        usage: parser.finish(status)?,
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
    fn flush(&mut self) -> Result<(), NativeRegistryError> {
        if !self.pending.is_empty() {
            self.line()?;
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
        let response = line.get("response").filter(|response| !response.is_null());
        let row = if let Some(response) = response {
            let status = response
                .get("status_code")
                .and_then(Value::as_u64)
                .filter(|status| (100..=599).contains(status))
                .ok_or_else(unavailable)? as u16;
            let body = response.get("body").unwrap_or(&Value::Null);
            if (200..=299).contains(&status) {
                // Responses 的 HTTP 2xx 也可能承载失败或取消；逐项状态来自响应体。
                let terminal = if entry.endpoint == "/v1/responses" {
                    match body.get("status") {
                        None => "completed",
                        Some(value) => match value.as_str() {
                            Some(
                                terminal @ ("completed" | "incomplete" | "failed" | "cancelled"),
                            ) => terminal,
                            _ => return Err(unavailable()),
                        },
                    }
                } else {
                    "completed"
                };
                let missing_failure_usage = matches!(terminal, "failed" | "cancelled")
                    && body.get("usage").is_none_or(Value::is_null);
                let usage = if missing_failure_usage {
                    &Value::Null
                } else if entry.endpoint == "/v1/moderations" {
                    body.get("results")
                        .and_then(Value::as_array)
                        .ok_or_else(unavailable)?;
                    &Value::Null
                } else {
                    body.get("usage")
                        .filter(|usage| usage.is_object())
                        .ok_or_else(unavailable)?
                };
                let required: &[&str] = match entry.endpoint.as_str() {
                    "/v1/moderations" => &[],
                    "/v1/responses" if missing_failure_usage => &[],
                    "/v1/responses" => &["input_tokens", "output_tokens"],
                    "/v1/images/generations" | "/v1/images/edits" => &["input_tokens"],
                    "/v1/embeddings" => &["prompt_tokens", "total_tokens"],
                    _ => &["prompt_tokens", "completion_tokens"],
                };
                for field in required {
                    usage
                        .get(*field)
                        .and_then(Value::as_u64)
                        .filter(|value| *value <= u32::MAX as u64)
                        .ok_or_else(unavailable)?;
                }
                let stats = match entry.endpoint.as_str() {
                    "/v1/moderations" => Some(aisix_gateway::UsageStats::default()),
                    "/v1/responses" => crate::responses::response_usage_stats(body)
                        .or_else(|| missing_failure_usage.then(aisix_gateway::UsageStats::default)),
                    "/v1/embeddings" => crate::embeddings::embedding_usage_stats(body),
                    "" | "/v1/chat/completions" | "/v1/completions" => {
                        serde_json::from_value::<aisix_provider_openai::wire::OpenAiUsage>(
                            usage.clone(),
                        )
                        .ok()
                        .map(aisix_provider_openai::wire::into_usage)
                    }
                    "/v1/images/generations" | "/v1/images/edits" => {
                        crate::images::image_usage_stats(body)
                    }
                    _ => None,
                }
                .ok_or_else(unavailable)?;
                let (status, error_class, message) = match terminal {
                    "failed" => (
                        502,
                        "upstream_status",
                        body.pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("Response generation failed"),
                    ),
                    "cancelled" => (499, "", "Response generation cancelled"),
                    _ => (status, "", ""),
                };
                let mut row = terminal_usage(
                    entry,
                    &stats,
                    self.observed_at,
                    status,
                    terminal,
                    error_class,
                    message,
                    body.get("service_tier")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                );
                let protocol = match entry.endpoint.as_str() {
                    "/v1/responses" => aisix_gateway::upstream_io::Protocol::Responses,
                    "/v1/embeddings" => aisix_gateway::upstream_io::Protocol::Embeddings,
                    "/v1/completions" => aisix_gateway::upstream_io::Protocol::Completions,
                    "/v1/images/generations" | "/v1/images/edits" => {
                        aisix_gateway::upstream_io::Protocol::Images
                    }
                    "/v1/moderations" => aisix_gateway::upstream_io::Protocol::Unknown,
                    _ => aisix_gateway::upstream_io::Protocol::Chat,
                };
                row.io_audit = Some(super::background::terminal_io(
                    protocol,
                    entry.upstream_input_text,
                    body,
                    terminal == "completed",
                ));
                row
            } else {
                let message = body
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Batch entry failed");
                terminal_usage(
                    entry,
                    &Default::default(),
                    self.observed_at,
                    status,
                    "failed",
                    "upstream_status",
                    message,
                    None,
                )
            }
        } else {
            let error = line
                .get("error")
                .filter(|error| error.is_object())
                .ok_or_else(unavailable)?;
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(unavailable)?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(unavailable)?;
            let (status, kind) = match code {
                "batch_expired" => (504, "expired"),
                _ => (500, "failed"),
            };
            terminal_usage(
                entry,
                &Default::default(),
                self.observed_at,
                status,
                kind,
                "upstream_status",
                message,
                None,
            )
        };
        self.usage.push(row);
        Ok(())
    }
    fn finish(mut self, status: &str) -> Result<Vec<TerminalUsage>, NativeRegistryError> {
        self.flush()?;
        if status == "completed" && self.seen.len() != self.expected.len() {
            return Err(unavailable());
        }
        for (id, entry) in &self.expected {
            if self.seen.contains(*id) {
                continue;
            }
            let (code, kind) = match status {
                "cancelled" => (499, "cancelled"),
                "expired" => (504, "expired"),
                "failed" => (500, "failed"),
                _ => return Err(unavailable()),
            };
            self.usage.push(terminal_usage(
                entry,
                &Default::default(),
                self.observed_at,
                code,
                kind,
                "",
                "Batch entry did not execute",
                None,
            ));
        }
        Ok(self.usage)
    }
}
#[allow(clippy::too_many_arguments)]
fn terminal_usage(
    entry: &NativeEntry,
    stats: &aisix_gateway::UsageStats,
    observed_at: DateTime<Utc>,
    status_code: u16,
    terminal: &str,
    error_class: &str,
    error_message: &str,
    service_tier: Option<String>,
) -> TerminalUsage {
    let occurred_at = observed_at;
    TerminalUsage {
        io_audit: None,
        entry_id: entry.entry_id.clone(),
        occurred_at,
        status_code,
        terminal_status: Some(terminal.into()),
        error_class: error_class.into(),
        error_message: aisix_gateway::truncate_lossy(
            &aisix_gateway::scrub_upstream_error_text(error_message),
            4093,
        ),
        prompt_tokens: stats.prompt_tokens,
        completion_tokens: aisix_gateway::chat::recorded_completion_tokens(
            stats.prompt_tokens,
            stats.completion_tokens,
            stats.reasoning_tokens,
            stats.reasoning_folded_into_completion,
            stats.upstream_total_tokens,
        ),
        cached_prompt_tokens: stats.cached_prompt_tokens,
        cache_usage_reported: Some(stats.cache_usage_reported().unwrap_or(false)),
        cache_write_tokens: stats.cache_write_tokens,
        cache_creation_tokens: stats.cache_creation_tokens,
        cache_read_tokens: stats.cache_read_tokens,
        cache_creation_5m_tokens: stats.cache_creation_ttl.cache_creation_5m_tokens,
        cache_creation_1h_tokens: stats.cache_creation_ttl.cache_creation_1h_tokens,
        reasoning_tokens: stats.reasoning_tokens,
        total_tokens: stats.upstream_total_tokens,
        prompt_audio_tokens: stats.modality.prompt_audio_tokens,
        prompt_image_tokens: stats.modality.prompt_image_tokens,
        completion_audio_tokens: stats.modality.completion_audio_tokens,
        completion_image_tokens: stats.modality.completion_image_tokens,
        web_search_requests: stats.web_search_requests,
        usage_estimated: false,
        service_tier,
        requested_service_tier: entry.requested_service_tier.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, tier: &str) -> NativeEntry {
        serde_json::from_value(json!({"entry_id":id,
            "requested_model_id":"11111111-1111-4111-8111-111111111111",
            "model_id":"11111111-1111-4111-8111-111111111111",
            "requested_model":"public-model", "upstream_model":"native-model",
            "requested_service_tier":tier}))
        .unwrap()
    }

    #[test]
    fn recovered_batch_io_measures_model_result_and_never_jsonl_transport() {
        let mut saved = serde_json::to_value(entry("actual", "flex")).unwrap();
        saved["upstream_input_text"] = json!({"utf8_bytes":8,"unicode_scalars":3,"complete":true});
        saved["endpoint"] = json!("/v1/chat/completions");
        let entries: Vec<NativeEntry> = serde_json::from_value(json!([saved])).unwrap();
        let mut parser = ResultLines::new(&entries, chrono::Utc::now()).unwrap();
        let line = json!({"custom_id":"actual","response":{"status_code":200,"body":{
            "choices":[{"message":{"content":"B界🙂"}}],
            "usage":{"prompt_tokens":71,"completion_tokens":23,"total_tokens":94}}}});
        let wire = format!("{line}\n");
        for chunk in wire.as_bytes().chunks(7) {
            parser.push(chunk).unwrap();
        }
        let rows = parser.finish("completed").unwrap();
        let audit = rows[0].io_audit.as_ref().expect("model-result audit");
        assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (71, 23));
        assert_eq!(audit.version, 2);
        assert_eq!(audit.boundary, "upstream_attempt");
        assert_eq!(audit.request_body_bytes, None);
        assert_eq!(audit.response_body_bytes, None);
        assert_eq!(audit.request_complete, None);
        assert_eq!(
            audit.input_text,
            Some(aisix_core::usage_io::TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true,
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
        let absent = ResultLines::new(&entries, chrono::Utc::now())
            .unwrap()
            .finish("cancelled")
            .unwrap();
        assert_eq!(
            absent[0].io_audit, None,
            "unexecuted work has no invented exchange"
        );
    }

    #[test]
    fn native_batch_results_use_custom_ids_and_preserve_zero_cache_presence() {
        let entries = vec![entry("slow", "default"), entry("fast", "fast")];
        let at = chrono::DateTime::from_timestamp(1_793_843_000, 0).unwrap();
        let mut parser = ResultLines::new(&entries, at).unwrap();
        let fast = json!({"custom_id":"fast","response":{"status_code":200,"body":{
            "model":"returned-other-model","service_tier":"priority",
            "usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,
                "prompt_tokens_details":{"cached_tokens":0},
                "completion_tokens_details":{"reasoning_tokens":4}}}}});
        let fail = json!({"custom_id":"slow","response":{"status_code":400,
            "body":{"error":{"message":"invalid option","type":"invalid_request_error"}}}});
        parser.push(format!("{fast}\n{fail}\n").as_bytes()).unwrap();
        let usage = parser.finish("completed").unwrap();
        assert_eq!(usage.len(), 2);
        let fast = usage.iter().find(|row| row.entry_id == "fast").unwrap();
        assert_eq!(
            (
                fast.prompt_tokens,
                fast.completion_tokens,
                fast.total_tokens
            ),
            (100, 20, 120)
        );
        assert_eq!(fast.cached_prompt_tokens, 0);
        assert_eq!(fast.cache_usage_reported, Some(true));
        assert_eq!(fast.service_tier.as_deref(), Some("priority"));
        assert_eq!(fast.requested_service_tier.as_deref(), Some("fast"));
        assert_eq!(fast.reasoning_tokens, 4);
        assert_eq!(fast.occurred_at, at);
        assert_eq!(
            usage
                .iter()
                .find(|row| row.entry_id == "slow")
                .unwrap()
                .status_code,
            400
        );
    }

    #[test]
    fn missing_duplicate_unknown_or_malformed_success_never_settles_partial_work() {
        let entries = vec![entry("one", "default"), entry("two", "default")];
        let at = chrono::Utc::now();
        for raw in [
            "{\"custom_id\":\"foreign\",\"response\":{\"status_code\":400}}\n".to_owned(),
            "{\"custom_id\":\"one\",\"response\":{\"status_code\":200,\"body\":{}}}\n".to_owned(),
            "{\"custom_id\":\"one\",\"response\":{\"status_code\":400}}\n{\"custom_id\":\"one\",\"response\":{\"status_code\":400}}\n".to_owned(),
        ] {
            let mut parser = ResultLines::new(&entries, at).unwrap();
            assert!(parser.push(raw.as_bytes()).is_err());
        }
        assert!(ResultLines::new(&entries, at)
            .unwrap()
            .finish("completed")
            .is_err());
        let cancelled = ResultLines::new(&entries, at)
            .unwrap()
            .finish("cancelled")
            .unwrap();
        assert_eq!(cancelled.len(), 2);
        assert!(cancelled.iter().all(
            |row| row.terminal_status.as_deref() == Some("cancelled") && row.prompt_tokens == 0
        ));
    }
    #[test]
    fn response_and_embedding_entries_use_the_declared_endpoint_and_keep_absent_details() {
        let mut responses = entry("response", "flex");
        responses.endpoint = "/v1/responses".into();
        let mut embeddings = entry("embedding", "default");
        embeddings.endpoint = "/v1/embeddings".into();
        let mut absent = entry("response-absent", "default");
        absent.endpoint = "/v1/responses".into();
        let entries = vec![responses, embeddings, absent];
        let mut parser = ResultLines::new(&entries, chrono::Utc::now()).unwrap();
        let response = json!({"custom_id":"response","response":{"status_code":200,"body":{"usage":{
            "input_tokens":11,"output_tokens":5,"total_tokens":16,"input_tokens_details":{"cache_write_tokens":0},
            "output_tokens_details":{"reasoning_tokens":2}}}}});
        let embedding = json!({"custom_id":"embedding","response":{"status_code":200,"body":{"usage":{"prompt_tokens":7,"total_tokens":7}}}});
        let absent = json!({"custom_id":"response-absent","response":{"status_code":200,"body":{"usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}}});
        parser
            .push(format!("{embedding}\n{response}\n{absent}\n").as_bytes())
            .unwrap();
        let rows = parser.finish("completed").unwrap();
        let response = rows.iter().find(|row| row.entry_id == "response").unwrap();
        assert_eq!(
            (
                response.prompt_tokens,
                response.completion_tokens,
                response.total_tokens
            ),
            (11, 5, 16)
        );
        assert_eq!(response.cache_write_tokens, Some(0));
        assert_eq!(response.cache_usage_reported, Some(true));
        assert_eq!(response.reasoning_tokens, 2);
        let absent = rows
            .iter()
            .find(|row| row.entry_id == "response-absent")
            .unwrap();
        assert_eq!(absent.cache_write_tokens, None);
        assert_eq!(absent.cache_usage_reported, Some(false));
        let embedding = rows.iter().find(|row| row.entry_id == "embedding").unwrap();
        assert_eq!(
            (
                embedding.prompt_tokens,
                embedding.completion_tokens,
                embedding.total_tokens
            ),
            (7, 0, 7)
        );
        assert_eq!(embedding.cache_write_tokens, None);
        assert_eq!(embedding.cache_usage_reported, Some(false));
    }

    #[test]
    fn failed_response_without_usage_does_not_block_its_completed_sibling() {
        let mut entries = vec![entry("good", "flex"), entry("failed", "default")];
        for entry in &mut entries {
            entry.endpoint = "/v1/responses".into();
        }
        let mut parser = ResultLines::new(&entries, chrono::Utc::now()).unwrap();
        let good = json!({"custom_id":"good","response":{"status_code":200,"body":{
            "object":"response","status":"completed","service_tier":"priority",
            "usage":{"input_tokens":31,"output_tokens":7,"total_tokens":38,
                "input_tokens_details":{"cached_tokens":11},
                "output_tokens_details":{"reasoning_tokens":2}}}}});
        let failed = json!({"custom_id":"failed","response":{"status_code":200,"body":{
            "object":"response","status":"failed","usage":null,
            "error":{"code":"server_error","message":"Generation failed"}}}});
        parser
            .push(format!("{good}\n{failed}\n").as_bytes())
            .unwrap();
        let rows = parser.finish("completed").unwrap();
        assert_eq!(rows.len(), 2);
        let good = rows.iter().find(|row| row.entry_id == "good").unwrap();
        assert_eq!(good.terminal_status.as_deref(), Some("completed"));
        assert_eq!(good.status_code, 200);
        assert_eq!(
            (
                good.prompt_tokens,
                good.completion_tokens,
                good.total_tokens
            ),
            (31, 7, 38)
        );
        assert_eq!(good.cached_prompt_tokens, 11);
        assert_eq!(good.reasoning_tokens, 2);
        let failed = rows.iter().find(|row| row.entry_id == "failed").unwrap();
        assert_eq!(failed.terminal_status.as_deref(), Some("failed"));
        assert_eq!(failed.status_code, 502);
        assert_eq!(failed.error_class, "upstream_status");
        assert_eq!(failed.error_message, "Generation failed");
        assert_eq!(
            (
                failed.prompt_tokens,
                failed.completion_tokens,
                failed.total_tokens
            ),
            (0, 0, 0)
        );
        assert_eq!(failed.cache_usage_reported, Some(false));
        assert!(!failed.usage_estimated);
    }

    #[test]
    fn response_terminal_status_preserves_reported_partial_usage() {
        let mut response = entry("response", "flex");
        response.endpoint = "/v1/responses".into();
        let entries = vec![response];
        for (terminal, status, error_class, error_message) in [
            ("failed", 502, "upstream_status", "Generation failed"),
            ("incomplete", 200, "", ""),
            ("cancelled", 499, "", "Response generation cancelled"),
        ] {
            let mut parser = ResultLines::new(&entries, chrono::Utc::now()).unwrap();
            let response = json!({"custom_id":"response","response":{"status_code":200,"body":{
                "object":"response","status":terminal,"service_tier":"priority",
                "error":if terminal == "failed" {json!({"code":"server_error","message":"Generation failed"})} else {Value::Null},
                "incomplete_details":if terminal == "incomplete" {json!({"reason":"max_output_tokens"})} else {Value::Null},
                "usage":{"input_tokens":17,"output_tokens":6,"total_tokens":23,
                    "input_tokens_details":{"cached_tokens":7,"cache_write_tokens":0},
                    "output_tokens_details":{"reasoning_tokens":3}}}}});
            parser.push(format!("{response}\n").as_bytes()).unwrap();
            let rows = parser.finish("completed").unwrap();
            assert_eq!(rows.len(), 1);
            let row = &rows[0];
            assert_eq!(row.terminal_status.as_deref(), Some(terminal));
            assert_eq!(row.status_code, status);
            assert_eq!(row.error_class, error_class);
            assert_eq!(row.error_message, error_message);
            assert_eq!(
                (row.prompt_tokens, row.completion_tokens, row.total_tokens),
                (17, 6, 23)
            );
            assert_eq!(row.cached_prompt_tokens, 7);
            assert_eq!(row.cache_write_tokens, Some(0));
            assert_eq!(row.cache_usage_reported, Some(true));
            assert_eq!(row.reasoning_tokens, 3);
            assert_eq!(row.service_tier.as_deref(), Some("priority"));
            assert_eq!(row.requested_service_tier.as_deref(), Some("flex"));
            assert!(!row.usage_estimated);
        }
    }

    #[test]
    fn response_nonterminal_or_malformed_billable_usage_stays_pending() {
        let mut response = entry("response", "default");
        response.endpoint = "/v1/responses".into();
        let entries = vec![response];
        for body in [
            json!({"status":"completed","usage":null}),
            json!({"status":"incomplete","usage":null}),
            json!({"status":"failed","usage":{"input_tokens":17}}),
            json!({"status":"failed","usage":{"input_tokens":17,"output_tokens":-1}}),
            json!({"status":"in_progress","usage":{"input_tokens":17,"output_tokens":6}}),
            json!({"status":"queued","usage":{"input_tokens":17,"output_tokens":6}}),
        ] {
            let mut parser = ResultLines::new(&entries, chrono::Utc::now()).unwrap();
            let response =
                json!({"custom_id":"response","response":{"status_code":200,"body":body}});
            assert!(
                parser.push(format!("{response}\n").as_bytes()).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn response_entries_preserve_reported_web_search_requests() {
        let mut entries = vec![
            entry("searched", "default"),
            entry("zero", "default"),
            entry("missing", "default"),
        ];
        for entry in &mut entries {
            entry.endpoint = "/v1/responses".into();
        }
        let mut parser = ResultLines::new(&entries, chrono::Utc::now()).unwrap();
        let searched = json!({"custom_id":"searched","response":{"status_code":200,"body":{
            "object":"response","status":"completed",
            "usage":{"input_tokens":31,"output_tokens":7,"total_tokens":38},
            "tool_usage":{"web_search":{"num_requests":7}}}}});
        let zero = json!({"custom_id":"zero","response":{"status_code":200,"body":{
            "object":"response","status":"completed",
            "usage":{"input_tokens":17,"output_tokens":6,"total_tokens":23},
            "tool_usage":{"web_search":{"num_requests":0}}}}});
        let missing = json!({"custom_id":"missing","response":{"status_code":200,"body":{
            "object":"response","status":"completed",
            "usage":{"input_tokens":11,"output_tokens":5,"total_tokens":16}}}});
        parser
            .push(format!("{missing}\n{searched}\n{zero}\n").as_bytes())
            .unwrap();
        let rows = parser.finish("completed").unwrap();
        assert_eq!(rows.len(), 3);
        for (id, searches, tokens) in [
            ("searched", 7, (31, 7, 38)),
            ("zero", 0, (17, 6, 23)),
            ("missing", 0, (11, 5, 16)),
        ] {
            let row = rows.iter().find(|row| row.entry_id == id).unwrap();
            assert_eq!(row.web_search_requests, searches, "{id}");
            assert_eq!(
                (row.prompt_tokens, row.completion_tokens, row.total_tokens),
                tokens,
                "{id}"
            );
            assert_eq!(row.terminal_status.as_deref(), Some("completed"));
            assert_eq!(row.status_code, 200);
            assert!(!row.usage_estimated);
        }
    }
}
