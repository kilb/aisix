//! Evals 只以运行的 per_model_usage 结算；输出 sample 永不重复累计。
use std::collections::BTreeSet;
use std::time::Duration;

use aisix_core::native_metering::{
    MeteringTarget, ObservedMeter, ObservedUsage, ReconciliationReason, ReconciliationRequest,
    TypedCompleteRequest,
};
use aisix_core::native_objects::{
    configured_native_endpoint, configured_native_origin, ClaimRequest, LeaseRequest, LeaseToken,
    NativeObjectKind, NativeProtocol, NativeRecord, TerminalUsage,
};
use axum::http::{HeaderMap, HeaderValue};
use chrono::Utc;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::watch;
use tracing::Instrument;

use super::registry::{NativeObjectRegistry, NativeRegistryError};
use crate::state::ProxyState;

const CLAIM_SIZE: u32 = 8;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_POLL_TIME: Duration = Duration::from_secs(10);
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

fn unavailable() -> NativeRegistryError {
    NativeRegistryError::Unavailable
}

/// 多进程共享持久租约；退出不把未观察的消耗登记为零。
pub async fn run(state: ProxyState, mut cancel: watch::Receiver<bool>) {
    if state.native_objects.is_none() || *cancel.borrow() {
        return;
    }
    let mut ticks = tokio::time::interval(POLL_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            changed=cancel.changed()=>if changed.is_err() || *cancel.borrow(){return;},
            _=ticks.tick()=>{
                tokio::select! {
                    biased;
                    changed=cancel.changed()=>if changed.is_err() || *cancel.borrow(){return;},
                    result=run_once(&state)=>if result.is_err(){tracing::warn!("native eval observation authority unavailable");},
                }
            }
        }
    }
}

pub async fn run_once(state: &ProxyState) -> Result<usize, NativeRegistryError> {
    let registry = state.native_objects.as_ref().ok_or_else(unavailable)?;
    let claimed = registry
        .claim(ClaimRequest {
            limit: CLAIM_SIZE,
            record_id: None,
            protocol: Some(NativeProtocol::Openai),
            kind: Some(NativeObjectKind::EvalRun),
        })
        .await?;
    if claimed.state != "leased" || claimed.records.len() > CLAIM_SIZE as usize {
        return Err(unavailable());
    }
    let mut polls = futures::stream::iter(claimed.records.into_iter().map(|record| {
        let span = tracing::info_span!("native_eval_observe",record_id=%record.record_id);
        let tap = std::sync::Arc::new(aisix_gateway::ResponseHeaderTap::default());
        aisix_gateway::response_tap::scope(tap, poll(state, registry.as_ref(), record))
            .instrument(span)
    }))
    .buffer_unordered(CLAIM_SIZE as usize);
    let mut observed = 0;
    while let Some(result) = polls.next().await {
        match result {
            Ok(true) => observed += 1,
            Ok(false) => {}
            Err(_) => tracing::warn!("native eval observation will resume after its lease"),
        }
    }
    Ok(observed)
}

fn lease(record: &NativeRecord) -> Result<LeaseToken, NativeRegistryError> {
    record.lease_expires_at.ok_or_else(unavailable)?;
    Ok(LeaseToken {
        record_id: record.record_id,
        generation: record
            .lease_generation
            .filter(|generation| *generation > 0)
            .ok_or_else(unavailable)?,
    })
}
async fn reschedule(
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
    if !matches!(
        reply.state.as_str(),
        "pending" | "completed" | "reconciliation"
    ) {
        return Err(unavailable());
    }
    Ok(false)
}
async fn poll(
    state: &ProxyState,
    registry: &dyn NativeObjectRegistry,
    record: NativeRecord,
) -> Result<bool, NativeRegistryError> {
    let lease = lease(&record)?;
    let value = match retrieve(state, &record).await {
        Ok(value) => value,
        Err(_) => return reschedule(registry, lease, 30_000).await,
    };
    if value.get("id").and_then(Value::as_str) != Some(record.raw_id.as_str())
        || value.get("eval_id").and_then(Value::as_str) != record.parent_raw_id.as_deref()
    {
        return reschedule(registry, lease, 30_000).await;
    }
    let Some(status) = value
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "completed" | "failed" | "canceled"))
    else {
        return reschedule(registry, lease, 2_000).await;
    };
    let renewed = registry
        .renew(LeaseRequest {
            lease: lease.clone(),
            poll_after_ms: 0,
        })
        .await?;
    if matches!(renewed.state.as_str(), "completed" | "reconciliation") {
        return Ok(false);
    }
    if renewed.state != "leased" {
        return Err(unavailable());
    }
    let target = MeteringTarget::Object { lease };
    let reply = match observation(&record, &value, status) {
        Ok(usage) => {
            registry
                .complete_typed(TypedCompleteRequest {
                    target,
                    terminal_status: status.into(),
                    usage,
                    artifacts: Vec::new(),
                })
                .await?
        }
        Err(reason) => {
            registry
                .mark_reconciliation(ReconciliationRequest {
                    target,
                    terminal_status: Some(status.into()),
                    reason,
                    observed_usage: Vec::new(),
                    raw_usage: if reason == ReconciliationReason::UnknownUsageShape {
                        value.get("per_model_usage").cloned()
                    } else {
                        None
                    },
                    artifacts: Vec::new(),
                })
                .await?
        }
    };
    if !matches!(reply.state.as_str(), "completed" | "reconciliation") {
        return Err(unavailable());
    }
    Ok(true)
}

fn observation(
    record: &NativeRecord,
    value: &Value,
    status: &str,
) -> Result<Vec<ObservedUsage>, ReconciliationReason> {
    let Some(usage) = value
        .get("per_model_usage")
        .filter(|value| !value.is_null())
    else {
        return Err(ReconciliationReason::MissingUsage);
    };
    let rows = usage
        .as_array()
        .ok_or(ReconciliationReason::UnknownUsageShape)?;
    if rows.is_empty() {
        return Err(ReconciliationReason::MissingUsage);
    }
    if rows.len() > 1000 {
        return Err(ReconciliationReason::UnknownUsageShape);
    }
    let now = Utc::now();
    let mut seen = BTreeSet::new();
    let mut observations = Vec::with_capacity(rows.len());
    for row in rows {
        let model = row
            .get("model_name")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .ok_or(ReconciliationReason::UnknownUsageShape)?;
        if !seen.insert(model) {
            return Err(ReconciliationReason::UnknownUsageShape);
        }
        let mut accepted = record
            .entries
            .iter()
            .filter(|entry| entry.upstream_model == model);
        let entry = accepted
            .next()
            .ok_or(ReconciliationReason::UnknownUsageShape)?;
        if accepted.next().is_some() {
            return Err(ReconciliationReason::UnknownUsageShape);
        }
        let count = |name: &str| {
            row.get(name)
                .and_then(Value::as_u64)
                .and_then(|number| u32::try_from(number).ok())
                .ok_or(ReconciliationReason::UnknownUsageShape)
        };
        let prompt = count("prompt_tokens")?;
        let completion = count("completion_tokens")?;
        let cached = count("cached_tokens")?;
        let total = count("total_tokens")?;
        row.get("invocation_count")
            .and_then(Value::as_u64)
            .ok_or(ReconciliationReason::UnknownUsageShape)?;
        if cached > prompt {
            return Err(ReconciliationReason::UnknownUsageShape);
        }
        let terminal:TerminalUsage=serde_json::from_value(json!({"entry_id":entry.entry_id,"occurred_at":now,"terminal_status":status,
            "status_code":match status{"failed"=>502,"canceled"=>499,_=>200},"prompt_tokens":prompt,"completion_tokens":completion,
            "cached_prompt_tokens":cached,"cache_usage_reported":true,"total_tokens":total,"usage_estimated":false})).map_err(|_|ReconciliationReason::UnknownUsageShape)?;
        observations.push(ObservedUsage {
            entry_id: entry.entry_id.clone(),
            upstream_model: model.into(),
            occurred_at: now,
            meter: ObservedMeter::Tokens { usage: terminal },
        });
    }
    Ok(observations)
}

async fn retrieve(state: &ProxyState, record: &NativeRecord) -> Result<Value, NativeRegistryError> {
    if record.protocol != Some(NativeProtocol::Openai)
        || record.kind != Some(NativeObjectKind::EvalRun)
    {
        return Err(unavailable());
    }
    let parent = record.parent_raw_id.as_deref().ok_or_else(unavailable)?;
    for raw in [&record.raw_id, parent] {
        crate::jobs::require_safe_upstream_id(raw).map_err(|_| unavailable())?;
    }
    let binding = record.origin.as_ref().ok_or_else(unavailable)?;
    let snapshot = state.snapshot.load();
    let provider = snapshot
        .provider_keys
        .get_by_id(&binding.provider_key_id.to_string())
        .ok_or_else(unavailable)?;
    let id_key = state.native_origin_key().ok_or_else(unavailable)?;
    let current = configured_native_origin(
        &id_key,
        binding.provider_key_id,
        &provider.value,
        NativeProtocol::Openai,
        NativeObjectKind::EvalRun,
    )
    .map_err(|_| unavailable())?;
    if current != binding.boundary {
        return Err(unavailable());
    }
    // 已受理的运行只依赖原账号；撤销客户端或模型不抹去原账号已发生的消耗。
    let base = configured_native_endpoint(
        &provider.value,
        NativeProtocol::Openai,
        NativeObjectKind::EvalRun,
    )
    .map_err(|_| unavailable())?;
    let mut url = url::Url::parse(&base).map_err(|_| unavailable())?;
    url.set_path(&format!(
        "{}/evals/{parent}/runs/{}",
        url.path().trim_end_matches('/'),
        record.raw_id
    ));
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
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if chunk.len() > MAX_BODY_BYTES.saturating_sub(bytes.len()) {
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
