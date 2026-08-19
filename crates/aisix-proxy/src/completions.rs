//! `POST /v1/completions` — OpenAI-compatible legacy text completions.
//!
//! This endpoint is a thin passthrough to the provider's `/completions`
//! surface. The upstream `model` field is rewritten to the provider's own
//! model id; everything else in the request body is forwarded verbatim.
//!
//! Flow:
//! 1. [`AuthenticatedKey`] extractor — 401 if auth fails.
//! 2. Parse the body as a JSON object.
//! 3. Validate `model` is present.
//! 4. Resolve model name → `Model` in snapshot → 404 if absent.
//! 5. Check `allowed_models` → 403 if denied.
//! 6. Look up Bridge on Hub → 503 if not registered.
//! 7. Call `bridge.complete(body, ctx)` → JSON response.
//! 8. Providers that don't support completions return 501.

mod stream;

use aisix_gateway::{BridgeError, ChatMessage, ChatResponse, FinishReason, UsageStats};
use aisix_obs::{content_capture_cap, AccessLog, CapturedContent, LatencyLabels, UsageEvent};
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use stream::{
    build_completion_passthrough_stream, completion_usage_with_estimates, CompletionSseAccumulator,
    CompletionStreamFailure, CompletionStreamOutcome, PreparedCompletionStream,
};

use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::{ErrorEnvelope, ProxyError};
use crate::state::ProxyState;

fn note_completion_failure(
    health: &crate::health::HealthTracker,
    runtime_status: &crate::health::ModelRuntimeStatusTracker,
    model_display_name: &str,
    model_id: &str,
    cooldown: Option<&aisix_core::CooldownConfig>,
    error: BridgeError,
) -> BridgeError {
    if error.http_status() >= 500 {
        health.record_failure(model_display_name);
    }
    crate::cooldown::note_failure(runtime_status, model_id, cooldown, error)
}

/// Per-request payload from a successful dispatch — carries the
/// response + provider + the bits the handler needs to emit a
/// UsageEvent on the success path (#403).
struct CompletionDispatchSuccess {
    response: Response,
    provider: String,
    /// UUID of the resolved Model row — required for UsageEvent
    /// `model_id`. Always populated on every success arm (including
    /// the 501 NotImplemented branch where no upstream call
    /// happened); the emit gate is `usage.is_some()`, not this
    /// field. Audit MEDIUM-1 on PR #426 clarified.
    model_id: String,
    /// Resolved ProviderKey UUID — feeds per-PK telemetry attribution
    /// (AISIX-Cloud#867 parity).
    provider_key_id: String,
    /// Provider-side model name, for the `upstream_model` metric label
    /// (AISIX-Cloud#1234 parity with chat / messages / responses).
    upstream_model: String,
    /// Legacy-completions response object `id` (`cmpl-…`). Empty on the 501
    /// NotImplemented path (no upstream call) and when the upstream omitted
    /// it (AISIX-Cloud#1289).
    provider_request_id: String,
    /// Upstream-reported token counts. `None` on the 501
    /// NotImplemented path (provider doesn't support completions)
    /// or on a 200 with no `usage` block (rare edge). Handler
    /// gates UsageEvent emission on this being `Some`.
    usage: Option<CompletionUsage>,
    /// Per-detector PII mask counts (#932), input + output merged.
    /// Attached to the emitted UsageEvent. Empty = no redaction.
    redactions: crate::redact::RedactionCounts,
    /// Monitor-mode guardrail observations (AISIX-Cloud#562), input +
    /// output merged. Attached to the emitted UsageEvent.
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    /// Typed failure class for a held stream that failed after generating
    /// billable output. Empty on successful/gateway-blocked responses.
    error_class: String,
    /// True when the response leg was blocked by an OUTPUT guardrail
    /// AFTER the upstream billed for it (#911 [23]). The response body is
    /// the redacted 422, but `usage` still carries the billed counts so
    /// the UsageEvent (marked `guardrail_blocked`) keeps cp-api's budget
    /// ledger + /logs from under-reporting spend the provider charged for
    /// — the output analog of chat.rs's UpstreamCharge / responses.rs #543.
    guardrail_blocked: bool,
    /// Captured request/response content for content-capturing exporters
    /// (AISIX-Cloud#947). `Some` only when an enabled exporter opted into
    /// `content_mode = full`; threaded to `fan_out` via the handler's emit,
    /// never to the CP sink.
    captured_content: Option<CapturedContent>,
    /// Live streams emit usage and end-to-end latency from their terminal
    /// callback. Buffered and non-streaming responses emit in the handler.
    usage_handled_by_stream: bool,
    /// Which group member served and what the walk cost.
    routing: crate::routing::RoutingAttribution,
}

#[derive(Default)]
struct CompletionDispatchTelemetry {
    applied_guardrails: Vec<aisix_core::AppliedGuardrail>,
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
}

/// Subset of the OpenAI legacy /v1/completions response `usage`
/// block surfaced for telemetry. Field naming mirrors the wire:
/// `prompt_tokens` + `completion_tokens` are both present (unlike
/// embeddings which has only prompt_tokens). Source:
/// <https://platform.openai.com/docs/api-reference/completions/object>
#[derive(Clone, Default)]
struct CompletionUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    /// True when any counter was filled by the local estimator because
    /// the upstream reported no usage (AISIX-Cloud#1074).
    usage_estimated: bool,
}

pub async fn completions(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    // Result-wrapped so an extractor-layer 413 (chunked body over the
    // cap) maps to the OpenAI envelope instead of axum's stock
    // text/plain rejection — same discriminate-then-map pattern as
    // chat.rs / messages.rs.
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let started = Instant::now();
    let Json(body) = match body {
        Ok(json) => json,
        // Answer through `reject` so the refusal still produces the access
        // log line + request metrics the handler tail emits for a served
        // request — the tail it never reaches.
        Err(rej) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                "/v1/completions",
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_json_rejection(
                    rej,
                    state.request_body_limit_for("/v1/completions"),
                ),
            );
        }
    };
    let request_id = client.request_id.clone();
    let api_key_id = auth.entry.id.clone();
    let model_name = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let is_stream = body.get("stream").and_then(Value::as_bool) == Some(true);
    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();
    let mut telemetry = CompletionDispatchTelemetry::default();

    match dispatch(
        &state,
        &snapshot,
        &auth,
        body,
        &request_id,
        &client,
        &mut telemetry,
    )
    .await
    {
        Ok(success) => {
            let elapsed = started.elapsed();
            // Audit MEDIUM-2 on PR #426: use the actual response
            // status, not a hardcoded 200. The 501 NotImplemented
            // branch returns `Ok(success)` with a 501 response —
            // logging status=200 there made it impossible for
            // operators to distinguish real successes from "provider
            // does not support completions". Matches the convention
            // PR #404 (responses) and PR #405 (rerank) adopted.
            let status = success.response.status().as_u16();
            emit_access_log(
                &model_name,
                &success.provider,
                &api_key_id,
                status,
                elapsed,
                &request_id,
                Some(success.provider_request_id.as_str()),
                Some(&success.routing),
                None,
            );
            // One ProviderKey lookup for the metric emit + the usage event
            // below (#941).
            let pk = crate::usage_attr::ResolvedPk::resolve(&snapshot, &success.provider_key_id);
            crate::request_metrics::record(
                &state,
                "/v1/completions",
                crate::request_metrics::Caller::new(&auth),
                crate::request_metrics::Upstream {
                    provider: &success.provider,
                    model: &model_name,
                    upstream_model: &success.upstream_model,
                    pk: pk.labels(),
                    stream: is_stream,
                    ..Default::default()
                },
                status,
                elapsed,
            );
            if !success.usage_handled_by_stream {
                let metric_model = crate::usage_attr::metric_model_label(&snapshot, &model_name);
                state.metrics.record_request_e2e_latency(
                    LatencyLabels {
                        endpoint: "/v1/completions",
                        model: metric_model.as_ref(),
                        provider: &success.provider,
                        status,
                        streaming: is_stream,
                    },
                    elapsed,
                );
            }
            // Issue #403: emit UsageEvent so cp-api's budget ledger
            // and customer-facing /logs see /v1/completions spend.
            // Pre-#403 the legacy completions handler dropped the
            // event entirely. Skip emit on the 501 NotImplemented
            // path (no upstream call) and on 200 without a usage
            // block (rare edge) — both surface as `usage: None`.
            if let Some(usage) = success.usage {
                emit_usage_event(
                    &state,
                    &snapshot,
                    &pk,
                    &request_id,
                    &success.model_id,
                    &model_name,
                    &api_key_id,
                    &success.provider,
                    &success.upstream_model,
                    status,
                    elapsed,
                    &usage,
                    &success.provider_request_id,
                    &success.routing,
                    &client,
                    is_stream,
                    &success.error_class,
                    success.guardrail_blocked,
                    telemetry.applied_guardrails.clone(),
                    success.redactions.clone(),
                    success.monitor_hits.clone(),
                    success.captured_content.as_ref(),
                );
            }
            // Same window /v1/chat/completions publishes, so an SDK client
            // on this endpoint can schedule back-off from real numbers
            // instead of blind-retrying into a 429.
            let mut response = success.response;
            let rl_limits = auth.key().rate_limit.clone().unwrap_or_default();
            crate::request_metrics::publish_rate_limit_window(
                &state.metrics,
                &state.limiter,
                &snapshot,
                &api_key_id,
                &rl_limits,
                &model_name,
                &success.upstream_model,
                &mut response,
            )
            .await;
            response
        }
        Err(err) => {
            let status = err.status().as_u16();
            let elapsed = started.elapsed();
            emit_access_log(
                &model_name,
                "unknown",
                &api_key_id,
                status,
                elapsed,
                &request_id,
                None,
                None,
                Some(&err),
            );
            let metric_model = crate::usage_attr::metric_model_label(&snapshot, &model_name);
            crate::request_metrics::record(
                &state,
                "/v1/completions",
                crate::request_metrics::Caller::new(&auth),
                crate::request_metrics::Upstream {
                    model: metric_model.as_ref(),
                    stream: is_stream,
                    ..Default::default()
                },
                status,
                elapsed,
            );
            state.metrics.record_request_e2e_latency(
                LatencyLabels {
                    endpoint: "/v1/completions",
                    model: metric_model.as_ref(),
                    provider: "unknown",
                    status,
                    streaming: is_stream,
                },
                elapsed,
            );
            // Per #655 parity: surface the failed request in Logs with a
            // zero-token event (status + error class), instead of dropping it.
            crate::usage_attr::emit_guardrail_error_usage_event(
                &state,
                &snapshot,
                "completions",
                "openai",
                &request_id,
                &model_name,
                &api_key_id,
                status,
                err.kind(),
                &client,
                matches!(&err, ProxyError::ContentFiltered(_)),
                telemetry.applied_guardrails,
                telemetry.monitor_hits,
            );
            err.into_response()
        }
    }
}

/// Build a [`ChatFormat`](aisix_gateway::ChatFormat) of user messages from
/// the legacy completions `prompt` so the input guardrail chain can scan it
/// (#545). Token-id prompts are rejected before this projection whenever an
/// input guardrail applies because the gateway cannot safely rewrite them.
/// Never sent upstream.
fn completions_input_to_chat(model: &str, body: &Value) -> aisix_gateway::ChatFormat {
    let mut messages = match body.get("prompt") {
        Some(Value::String(s)) if !s.is_empty() => {
            vec![aisix_gateway::ChatMessage::user(s.clone())]
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|it| it.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| aisix_gateway::ChatMessage::user(s.to_string()))
            .collect(),
        _ => Vec::new(),
    };
    for field in ["suffix", "user"] {
        if let Some(text) = body.get(field).and_then(Value::as_str) {
            if !text.is_empty() {
                messages.push(aisix_gateway::ChatMessage::user(text.to_string()));
            }
        }
    }
    aisix_gateway::ChatFormat::new(model, messages)
}

fn completions_prompt_uses_token_ids(body: &Value) -> bool {
    body.get("prompt")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| !item.is_string()))
}

async fn dispatch(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    mut body: Value,
    request_id: &str,
    client_ctx: &ClientContext,
    telemetry: &mut CompletionDispatchTelemetry,
) -> Result<CompletionDispatchSuccess, ProxyError> {
    let model_name = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProxyError::InvalidRequest("missing `model` field".into()))?
        .to_string();
    let model_name = model_name.as_str();

    let model_entry = crate::model_resolve::resolve_model(snapshot, model_name)
        .ok_or_else(|| ProxyError::ModelNotFound(model_name.to_string()))?;

    if !auth.key().can_access(model_name) {
        return Err(ProxyError::ModelForbidden(model_name.to_string()));
    }

    // Client-IP allowlist gate (#557): reject before guardrails / upstream.
    crate::dispatch::check_ip_access(&model_entry.value, &client_ctx.source_ip)?;

    // #545: /v1/completions must run input guardrails. Before this it
    // forwarded the user `prompt` to the upstream with no configured
    // content/DLP check, so a block enforced on /v1/chat/completions was
    // bypassable by switching surface. Run the check BEFORE the rate-limit
    // reservation so a content-policy refusal doesn't burn an RPM slot
    // (matching /v1/chat/completions).
    let guardrail_ctx = aisix_guardrails::RequestContext {
        passthrough_route_id: "",
        model_id: &model_entry.id,
        mcp_server_id: "",
        api_key_id: &auth.entry.id,
        team_id: auth.key().team_id.as_deref(),
    };
    let resolved_chain = Arc::new(state.guardrail_index.resolve(&guardrail_ctx));
    telemetry.applied_guardrails = resolved_chain.applied().to_vec();
    let input_guardrail_applies = telemetry
        .applied_guardrails
        .iter()
        .any(|guardrail| matches!(guardrail.hook.as_str(), "input" | "both"));
    if input_guardrail_applies && completions_prompt_uses_token_ids(&body) {
        return Err(ProxyError::ContentFiltered(
            "request blocked by input guardrail: token-id prompts cannot be inspected safely"
                .to_string(),
        ));
    }
    let mut input_seg_counts = crate::redact::RedactionCounts::new();
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
    let mut input_capture_safe = true;
    if !resolved_chain.is_empty() {
        let chat = completions_input_to_chat(model_name, &body);
        let (verdict, hits) = aisix_guardrails::Guardrail::check_input_non_segment_observed(
            resolved_chain.as_ref(),
            &chat,
        )
        .await;
        monitor_hits.extend(hits);
        // Segment pass: one Bedrock call over the prompt slots; an
        // ANONYMIZE disposition writes the masked text back into the body
        // (#932 bedrock follow-up).
        let moderation = crate::redact::moderate_completions_request_structured(
            resolved_chain.as_ref(),
            verdict,
            &mut body,
            &mut input_seg_counts,
            &mut monitor_hits,
        )
        .await;
        input_capture_safe = moderation.capture_safe;
        let verdict = moderation.verdict;
        telemetry.monitor_hits.clone_from(&monitor_hits);
        if let aisix_guardrails::GuardrailVerdict::Block {
            reason,
            guardrail_name,
        } = verdict
        {
            // Per #153 the matched-pattern detail stays in ops logs only.
            tracing::warn!(
                guardrail_hook = "input",
                model = %model_name,
                reason = %reason,
                "guardrail blocked /v1/completions request",
            );
            return Err(ProxyError::ContentFiltered(
                crate::error::guardrail_block_message("request", guardrail_name.as_deref()),
            ));
        }
    }

    // #932: mask-action PII rules rewrite the prompt in place AFTER the
    // block check passes, BEFORE the body is forwarded upstream.
    let final_redaction =
        crate::redact::redact_completions_request_structured(resolved_chain.as_ref(), &mut body);
    if final_redaction.unrewritable_tool_key {
        return Err(ProxyError::ContentFiltered(
            crate::error::guardrail_block_message("request", None),
        ));
    }
    let mut redactions = final_redaction.counts;
    crate::redact::merge_counts(&mut redactions, input_seg_counts);

    // Content capture (AISIX-Cloud#947): the client-facing request body
    // (post-redaction, so masked PII stays masked in the exported content),
    // gated on an exporter actually wanting content.
    let content_cap = content_capture_cap(
        snapshot
            .observability_exporters
            .entries()
            .iter()
            .map(|e| &*e.value),
    );
    let captured_prompt = content_cap.map(|_| serde_json::to_string(&body).unwrap_or_default());

    let model_rl =
        crate::quota::ModelRateLimit::from_model(model_name, &model_entry.id, &model_entry.value);
    let mut reservation = crate::quota::enforce(state, snapshot, auth, Some(&model_rl)).await?;

    // A Model Group has no provider of its own — the group walk resolves one
    // per target, so the bridge, credential, deadlines and upstream model
    // are all bound inside the walk rather than hoisted out of it.
    crate::dispatch::require_dispatchable_entry(&model_entry.value)?;
    let is_stream = body.get("stream").and_then(Value::as_bool) == Some(true);
    let group_entry = crate::routing::GroupEntry {
        endpoint: "/v1/completions",
        name: model_name,
        id: &model_entry.id,
        model: &model_entry.value,
    };
    // #554: the request deadline bounds a non-streaming call; the streaming
    // deadline bounds every body read. Legacy completions use a byte stream
    // but retain typed BridgeError failures, so the first read can still
    // retry / fail over and terminal telemetry can distinguish a later
    // timeout from a downstream disconnect.
    let attempt_ctx =
        |att: &crate::routing::GroupAttempt,
         pk: &aisix_core::ResourceEntry<aisix_core::ProviderKey>| {
            let mut ctx = crate::dispatch::bridge_ctx(
                request_id,
                &att.id,
                Arc::clone(&att.model),
                &pk.id,
                Arc::clone(&pk.value),
                Some(client_ctx),
            );
            if let Some(d) = if is_stream {
                att.timeouts.stream
            } else {
                att.timeouts.request
            } {
                ctx = ctx.with_deadline(d);
            }
            ctx
        };

    if is_stream {
        let output_policy =
            aisix_guardrails::Guardrail::stream_output_policy(resolved_chain.as_ref());
        let hold_back = aisix_guardrails::Guardrail::runs_on_output(resolved_chain.as_ref())
            && output_policy.holds_back();
        let (max_buffer_bytes, fail_open) = match output_policy {
            aisix_guardrails::StreamOutputPolicy::BufferFull {
                max_buffer_bytes,
                on_exceeded_fail_open,
            } => (max_buffer_bytes, on_exceeded_fail_open),
            _ => (aisix_guardrails::DEFAULT_STREAM_OUTPUT_BUFFER_BYTES, false),
        };
        let walk = crate::routing::dispatch_over_group(
            state,
            snapshot,
            auth,
            client_ctx,
            group_entry,
            |att| {
                let body = &body;
                async move {
                    let provider = crate::dispatch::require_provider(&att.model)?;
                    let pk_entry = crate::dispatch::resolve_provider_key(snapshot, &att.model)?;
                    let bridge = crate::dispatch::resolve_bridge(&state.hub, &pk_entry.value)
                        .ok_or(ProxyError::ProviderUnavailable)?;
                    let ctx = attempt_ctx(&att, &pk_entry);
                    let facts = CompletionAttemptFacts {
                        provider: provider.to_ascii_lowercase(),
                        provider_key_id: pk_entry.id.to_string(),
                        upstream_model: crate::dispatch::require_upstream_model(&att.model)?
                            .to_string(),
                    };
                    let prepared = if hold_back {
                        async {
                        let stream = bridge
                            .complete_stream(body, &ctx)
                            .await
                            .map_err(ProxyError::Bridge)?;
                let mut stream =
                            crate::stream_timeout::with_read_timeout_completion(
                                stream,
                                att.timeouts.stream,
                            );
                let mut buffer = Vec::new();
                let mut observed = CompletionSseAccumulator::default();
                while let Some(item) = stream.next().await {
                    let chunk = match item {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            // A stream that died after generating output is
                            // still an answer: the tokens were billed, so it
                            // comes back as `Failed` rather than as an error,
                            // and the arm that handles it marks the target's
                            // health itself.
                            if observed.has_observed_bytes() {
                                let output_text = observed.finish_for_overflow_estimate();
                                return Ok(PreparedCompletionStream::Failed {
                                    error,
                                    accumulator: observed,
                                    output_text,
                                });
                            }
                            return Err(ProxyError::Bridge(error));
                        }
                    };
                    // The cap-triggering bytes were generated (and may be
                    // billed) even though fail-closed policy withholds them.
                    // Feed a separately bounded parser before enforcing the
                    // wire-buffer limit so usage/token estimates include the
                    // complete observed prefix without retaining it all.
                    observed.push(&chunk);
                    if buffer.len().saturating_add(chunk.len()) > max_buffer_bytes {
                        if fail_open {
                            tracing::warn!(
                                guardrail_hook = "output",
                                model = %model_name,
                                max_buffer_bytes,
                                "streaming /v1/completions output exceeded buffer cap; failing open"
                            );
                            let prefix =
                                futures::stream::iter([Ok(bytes::Bytes::from(buffer)), Ok(chunk)]);
                            return Ok(PreparedCompletionStream::Live {
                                stream: Box::pin(prefix.chain(stream)),
                                capture_bypassed: true,
                            });
                        }
                        tracing::warn!(
                            guardrail_hook = "output",
                            model = %model_name,
                            max_buffer_bytes,
                            "streaming /v1/completions output exceeded buffer cap; failing closed"
                        );
                        let output_text = observed.finish_for_overflow_estimate();
                        return Ok(PreparedCompletionStream::BufferExceeded {
                            accumulator: observed,
                            output_text,
                        });
                    }
                    buffer.extend_from_slice(&chunk);
                }
                let mut inspection = CompletionSseAccumulator::with_security_cap(buffer.len());
                inspection.push(&buffer);
                inspection.finish();
                if inspection.malformed_data {
                    return Err(ProxyError::Bridge(BridgeError::UpstreamDecode(
                        "malformed legacy completion SSE data event".to_string(),
                    )));
                }
                if !inspection.saw_done {
                    return Err(ProxyError::Bridge(BridgeError::StreamAborted));
                }
                Ok(PreparedCompletionStream::Buffered(buffer))
                    }
                    .await
                    } else {
                        async {
                            let stream = bridge
                                .complete_stream(body, &ctx)
                                .await
                                .map_err(ProxyError::Bridge)?;
                            let mut stream = crate::stream_timeout::with_read_timeout_completion(
                                stream,
                                att.timeouts.stream,
                            );

                            // An explicitly configured per-model stream timeout is a
                            // failover contract: wait for the first body chunk before
                            // committing 200 so a TTFT timeout remains retryable. The
                            // deployment default still wraps subsequent reads, but does
                            // not add a pre-response wait to every legacy completion.
                            if att.timeouts.stream_configured {
                                return match stream.next().await {
                                    Some(Ok(first)) => {
                                        let prefix =
                                            futures::stream::once(std::future::ready(Ok(first)));
                                        Ok(PreparedCompletionStream::Live {
                                            stream: Box::pin(prefix.chain(stream)),
                                            capture_bypassed: false,
                                        })
                                    }
                                    Some(Err(error)) => Err(ProxyError::Bridge(error)),
                                    None => Err(ProxyError::Bridge(BridgeError::StreamAborted)),
                                };
                            }

                            Ok(PreparedCompletionStream::Live {
                                stream,
                                capture_bypassed: false,
                            })
                        }
                        .await
                    }?;
                    Ok((prepared, facts))
                }
            },
        )
        .await;

        let outcome = match walk {
            Ok(o) => o,
            Err(failure) => {
                reservation.commit_tokens(0).await;
                return completion_unsupported_or_error(
                    failure,
                    &model_entry,
                    redactions,
                    monitor_hits,
                );
            }
        };
        // Everything below attributes to the row that SERVED — for a group
        // that is a member, not the entry the caller addressed.
        let routing = outcome.attribution();
        let model = std::sync::Arc::clone(&outcome.target);
        let model_entry_id = outcome.target_id.clone();
        if let Some(member) = outcome.member_reservation {
            reservation.merge(member);
        }
        let (prepared, facts) = outcome.value;
        let provider_label = facts.provider;
        let provider_key_id = facts.provider_key_id;
        let upstream_model = facts.upstream_model;

        match prepared {
            PreparedCompletionStream::Buffered(mut buffer) => {
                let mut accumulator = CompletionSseAccumulator::with_security_cap(buffer.len());
                accumulator.push(&buffer);
                accumulator.finish();
                let output_text = accumulator.output_text();
                let usage = completion_usage_with_estimates(
                    accumulator.usage.unwrap_or_default(),
                    &upstream_model,
                    body.get("prompt"),
                    &output_text,
                );
                reservation
                    .commit_tokens(
                        u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens),
                    )
                    .await;

                let synth = ChatResponse {
                    id: String::new(),
                    model: model_name.to_string(),
                    message: ChatMessage::assistant(output_text),
                    finish_reason: FinishReason::Stop,
                    usage: UsageStats::default(),
                };
                let (verdict, hits) =
                    aisix_guardrails::Guardrail::check_output_non_segment_observed(
                        resolved_chain.as_ref(),
                        &synth,
                    )
                    .await;
                monitor_hits.extend(hits);
                let moderation = crate::redact::moderate_body(
                    resolved_chain.as_ref(),
                    crate::redact::Direction::Output,
                    verdict,
                    &mut redactions,
                    &mut monitor_hits,
                    |guardrail| match crate::redact::redact_completions_sse(guardrail, &buffer) {
                        Some((rewritten, counts)) => {
                            buffer = rewritten;
                            counts
                        }
                        None => crate::redact::RedactionCounts::new(),
                    },
                )
                .await;
                let capture_safe = moderation.capture_safe;
                let verdict = moderation.verdict;
                telemetry.monitor_hits.clone_from(&monitor_hits);
                if let aisix_guardrails::GuardrailVerdict::Block {
                    reason,
                    guardrail_name,
                } = verdict
                {
                    tracing::warn!(
                        guardrail_hook = "output",
                        model = %model_name,
                        reason = %reason,
                        "guardrail blocked streaming /v1/completions response"
                    );
                    return Ok(CompletionDispatchSuccess {
                        response: ProxyError::ContentFiltered(
                            crate::error::guardrail_block_message(
                                "response",
                                guardrail_name.as_deref(),
                            ),
                        )
                        .into_response(),
                        provider: provider_label,
                        model_id: model_entry_id.clone(),
                        provider_key_id: provider_key_id.clone(),
                        upstream_model,
                        provider_request_id: accumulator.provider_request_id,
                        usage: Some(usage),
                        redactions,
                        monitor_hits,
                        error_class: String::new(),
                        guardrail_blocked: true,
                        captured_content: None,
                        usage_handled_by_stream: false,
                        routing: routing.clone(),
                    });
                }

                if let Some((rewritten, counts)) =
                    crate::redact::redact_completions_sse(resolved_chain.as_ref(), &buffer)
                {
                    buffer = rewritten;
                    crate::redact::merge_counts(&mut redactions, counts);
                }
                let final_output = {
                    let mut parsed = CompletionSseAccumulator::with_security_cap(buffer.len());
                    parsed.push(&buffer);
                    parsed.finish();
                    parsed.output_text()
                };
                let captured_content = match (&captured_prompt, content_cap) {
                    (Some(prompt), Some(cap)) if input_capture_safe && capture_safe => {
                        Some(CapturedContent::new(prompt, &final_output, cap as usize))
                    }
                    _ => None,
                };
                let mut response = Response::new(axum::body::Body::from(buffer));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                return Ok(CompletionDispatchSuccess {
                    response,
                    provider: provider_label,
                    model_id: model_entry_id.clone(),
                    provider_key_id: provider_key_id.clone(),
                    upstream_model,
                    provider_request_id: accumulator.provider_request_id,
                    usage: Some(usage),
                    redactions,
                    monitor_hits,
                    error_class: String::new(),
                    guardrail_blocked: false,
                    captured_content,
                    usage_handled_by_stream: false,
                    routing: routing.clone(),
                });
            }
            PreparedCompletionStream::BufferExceeded {
                accumulator,
                output_text,
            } => {
                let usage = completion_usage_with_estimates(
                    accumulator.usage.unwrap_or_default(),
                    &upstream_model,
                    body.get("prompt"),
                    &output_text,
                );
                reservation
                    .commit_tokens(
                        u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens),
                    )
                    .await;
                return Ok(CompletionDispatchSuccess {
                    response: ProxyError::ContentFiltered(
                        "response blocked by content policy".into(),
                    )
                    .into_response(),
                    provider: provider_label,
                    model_id: model_entry_id.clone(),
                    provider_key_id: provider_key_id.clone(),
                    upstream_model,
                    provider_request_id: accumulator.provider_request_id,
                    usage: Some(usage),
                    redactions,
                    monitor_hits,
                    error_class: String::new(),
                    guardrail_blocked: true,
                    captured_content: None,
                    usage_handled_by_stream: false,
                    routing: routing.clone(),
                });
            }
            PreparedCompletionStream::Failed {
                error,
                accumulator,
                output_text,
            } => {
                let usage = completion_usage_with_estimates(
                    accumulator.usage.unwrap_or_default(),
                    &upstream_model,
                    body.get("prompt"),
                    &output_text,
                );
                reservation
                    .commit_tokens(
                        u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens),
                    )
                    .await;
                let error_class = error.error_type().to_string();
                // A stream that died after generating output came back as
                // an answer, so the walker marked the target healthy. It
                // was not: record the failure against the row that served
                // and let its cooldown rules see it.
                let error = note_completion_failure(
                    &state.health,
                    &state.runtime_status,
                    &model.display_name,
                    &model_entry_id,
                    model.cooldown.as_ref(),
                    error,
                );
                let response = ProxyError::Bridge(error).into_response();
                return Ok(CompletionDispatchSuccess {
                    response,
                    provider: provider_label,
                    model_id: model_entry_id.clone(),
                    provider_key_id: provider_key_id.clone(),
                    upstream_model,
                    provider_request_id: accumulator.provider_request_id,
                    usage: Some(usage),
                    redactions,
                    monitor_hits,
                    error_class,
                    guardrail_blocked: false,
                    captured_content: None,
                    usage_handled_by_stream: false,
                    routing: routing.clone(),
                });
            }
            PreparedCompletionStream::Live {
                stream,
                capture_bypassed,
            } => {
                let post_stream_keys = reservation.keys();
                let stream_hold = reservation.into_stream_hold();
                let limiter = Arc::clone(&state.limiter);
                let state_for_stream = state.clone();
                let request_id_for_stream = request_id.to_string();
                let model_id_for_stream = model_entry_id.clone();
                let model_display_name_for_stream = model.display_name.clone();
                let cooldown_for_stream = model.cooldown.clone();
                let requested_model = model_name.to_string();
                let api_key_id = auth.entry.id.clone();
                let provider_for_stream = provider_label.clone();
                let provider_key_id_for_stream = provider_key_id.clone();
                let routing_for_stream = routing.clone();
                let upstream_model_for_stream = upstream_model.clone();
                let prompt = body.get("prompt").cloned();
                let client = client_ctx.clone();
                let captured_prompt_for_stream = captured_prompt.clone();
                let input_redactions = redactions.clone();
                let input_monitor_hits = monitor_hits.clone();
                let applied_guardrails = telemetry.applied_guardrails.clone();
                let stream_started = Instant::now();
                let output_observer =
                    aisix_guardrails::Guardrail::runs_on_output(resolved_chain.as_ref())
                        .then(|| Arc::clone(&resolved_chain));
                let parsed = build_completion_passthrough_stream(
                    stream,
                    output_observer,
                    upstream_model.clone(),
                    move |accumulator, outcome, output_observation| {
                        let (terminal_status, error_class) = match outcome {
                            CompletionStreamOutcome::CleanEof => {
                                state_for_stream
                                    .health
                                    .record_success(&model_display_name_for_stream);
                                state_for_stream
                                    .runtime_status
                                    .mark_healthy(&model_id_for_stream);
                                (200, "")
                            }
                            CompletionStreamOutcome::UpstreamError {
                                status,
                                failure,
                                error_class,
                            } => {
                                if status >= 500 {
                                    state_for_stream
                                        .health
                                        .record_failure(&model_display_name_for_stream);
                                }
                                let error = match failure {
                                    CompletionStreamFailure::Timeout => BridgeError::Timeout {
                                        elapsed_ms: stream_started.elapsed().as_millis() as u64,
                                        cause: "stream body".to_string(),
                                    },
                                    CompletionStreamFailure::Decode => BridgeError::UpstreamDecode(
                                        "malformed legacy completion SSE data event".to_string(),
                                    ),
                                    CompletionStreamFailure::Other => BridgeError::StreamAborted,
                                };
                                let _ = crate::cooldown::note_failure(
                                    &state_for_stream.runtime_status,
                                    &model_id_for_stream,
                                    cooldown_for_stream.as_ref(),
                                    error,
                                );
                                (status, error_class)
                            }
                            CompletionStreamOutcome::DownstreamDrop => {
                                (crate::CLIENT_CLOSED_REQUEST, "")
                            }
                        };
                        let output_text = accumulator.output_text();
                        let usage = completion_usage_with_estimates(
                            accumulator.usage.unwrap_or_default(),
                            &upstream_model_for_stream,
                            prompt.as_ref(),
                            &output_text,
                        );
                        let total =
                            u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens);
                        limiter.add_tokens_post_stream_all(&post_stream_keys, total);
                        drop(stream_hold);
                        let captured_content = match (&captured_prompt_for_stream, content_cap) {
                            (Some(prompt), Some(cap))
                                if input_capture_safe
                                    && !capture_bypassed
                                    && output_observation.capture_safe =>
                            {
                                Some(CapturedContent::new(prompt, &output_text, cap as usize))
                            }
                            _ => None,
                        };
                        let mut all_hits = input_monitor_hits;
                        all_hits.extend(output_observation.monitor_hits);
                        let snapshot = state_for_stream.snapshot.load();
                        let pk = crate::usage_attr::ResolvedPk::resolve(
                            &snapshot,
                            &provider_key_id_for_stream,
                        );
                        let (metric_model, metric_upstream) =
                            crate::usage_attr::metric_model_label_pair(
                                &snapshot,
                                &requested_model,
                                &upstream_model_for_stream,
                            );
                        state_for_stream.metrics.record_request_e2e_latency(
                            LatencyLabels {
                                endpoint: "/v1/completions",
                                model: metric_model.as_ref(),
                                provider: &provider_for_stream,
                                status: terminal_status,
                                streaming: true,
                            },
                            stream_started.elapsed(),
                        );
                        emit_usage_event(
                            &state_for_stream,
                            &snapshot,
                            &pk,
                            &request_id_for_stream,
                            &model_id_for_stream,
                            &requested_model,
                            &api_key_id,
                            &provider_for_stream,
                            metric_upstream.as_ref(),
                            terminal_status,
                            stream_started.elapsed(),
                            &usage,
                            &accumulator.provider_request_id,
                            &routing_for_stream,
                            &client,
                            true,
                            error_class,
                            false,
                            applied_guardrails,
                            input_redactions,
                            all_hits,
                            captured_content.as_ref(),
                        );
                    },
                );
                let parsed =
                    crate::sse_keepalive::with_heartbeat(parsed, crate::sse_keepalive::interval());
                let mut response = Response::new(axum::body::Body::from_stream(parsed));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                return Ok(CompletionDispatchSuccess {
                    response,
                    provider: provider_label,
                    model_id: model_entry_id.clone(),
                    provider_key_id: provider_key_id.clone(),
                    upstream_model,
                    provider_request_id: String::new(),
                    usage: None,
                    redactions,
                    monitor_hits,
                    error_class: String::new(),
                    guardrail_blocked: false,
                    captured_content: None,
                    usage_handled_by_stream: true,
                    routing: routing.clone(),
                });
            }
        }
    }

    // The walk marks each failed attempt on the target's runtime status, so
    // the cooldown / circuit-breaker sees a flapping upstream even when a
    // later retry recovers the request.
    let walk = crate::routing::dispatch_over_group(
        state,
        snapshot,
        auth,
        client_ctx,
        group_entry,
        |att| {
            let body = &body;
            async move {
                let provider = crate::dispatch::require_provider(&att.model)?;
                let pk_entry = crate::dispatch::resolve_provider_key(snapshot, &att.model)?;
                let bridge = crate::dispatch::resolve_bridge(&state.hub, &pk_entry.value)
                    .ok_or(ProxyError::ProviderUnavailable)?;
                let ctx = attempt_ctx(&att, &pk_entry);
                let facts = CompletionAttemptFacts {
                    provider: provider.to_ascii_lowercase(),
                    provider_key_id: pk_entry.id.to_string(),
                    upstream_model: crate::dispatch::require_upstream_model(&att.model)?
                        .to_string(),
                };
                let resp_json = bridge
                    .complete(body, &ctx)
                    .await
                    .map_err(ProxyError::Bridge)?;
                Ok((resp_json, facts))
            }
        },
    )
    .await;

    match walk {
        Ok(outcome) => {
            let routing = outcome.attribution();
            let model_entry_id = outcome.target_id.clone();
            if let Some(member) = outcome.member_reservation {
                reservation.merge(member);
            }
            let (resp_json, facts) = outcome.value;
            let provider_label = facts.provider;
            let provider_key_id = facts.provider_key_id;
            let upstream_model = facts.upstream_model;
            // Extract usage BEFORE moving resp_json into the Response
            // so the success struct carries typed counters rather
            // than re-parsing JSON downstream.
            //
            // Token-estimation fallback (AISIX-Cloud#1074): a missing or
            // zero usage block fills locally — legacy completions is plain
            // text on both sides, so the plain-text counting rule applies
            // to each. The 200-without-usage edge previously skipped the
            // event entirely; it now emits an estimated record instead.
            // Telemetry only — the response body forwards untouched.
            // AISIX-Cloud#1289: read the response object id BEFORE the
            // redaction pass below rewrites the body.
            let provider_request_id = crate::usage_attr::provider_response_id(&resp_json);
            let usage = {
                let mut u = extract_completion_usage(&resp_json).unwrap_or(CompletionUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    usage_estimated: false,
                });
                let est_model = upstream_model.as_str();
                if u.prompt_tokens == 0 {
                    let n = count_completion_prompt(est_model, body.get("prompt"));
                    if n > 0 {
                        u.prompt_tokens = n;
                        u.usage_estimated = true;
                    }
                }
                if u.completion_tokens == 0 {
                    let n = crate::token_estimate::count_text(
                        est_model,
                        &completion_output_text(&resp_json),
                    );
                    if n > 0 {
                        u.completion_tokens = n;
                        u.usage_estimated = true;
                    }
                }
                Some(u)
            };
            // #911 [21]: commit the actual token cost so TPM/TPD is enforced
            // for /v1/completions the same way chat + embeddings enforce it.
            // Pre-fix the reservation dropped uncommitted, so the token
            // counter never moved and a caller could bypass token limits by
            // routing traffic through this endpoint.
            let total_tokens = usage
                .as_ref()
                .map(|u| u64::from(u.prompt_tokens) + u64::from(u.completion_tokens))
                .unwrap_or(0);
            reservation.commit_tokens(total_tokens).await;

            // #911 [23]: /v1/completions must run OUTPUT guardrails too. The
            // input hook above scans the prompt, but pre-fix the model's reply
            // was returned unscanned — a content/DLP block enforced on
            // /v1/chat/completions was bypassable by switching to this surface
            // for the response leg. Mirror chat's output check: buffer the reply
            // text into a synthetic ChatResponse and run the chain. The upstream
            // already billed (tokens committed above), so a block surfaces a
            // redacted 422 rather than the response.
            let mut resp_json = resp_json;
            let mut output_capture_safe = true;
            if !resolved_chain.is_empty() {
                let synth = ChatResponse {
                    id: String::new(),
                    model: model_name.to_string(),
                    message: ChatMessage::assistant(completion_output_text(&resp_json)),
                    finish_reason: FinishReason::Stop,
                    usage: UsageStats::default(),
                };
                let (verdict, hits) =
                    aisix_guardrails::Guardrail::check_output_non_segment_observed(
                        resolved_chain.as_ref(),
                        &synth,
                    )
                    .await;
                monitor_hits.extend(hits);
                let moderation = crate::redact::moderate_body(
                    resolved_chain.as_ref(),
                    crate::redact::Direction::Output,
                    verdict,
                    &mut redactions,
                    &mut monitor_hits,
                    |g| crate::redact::redact_completions_response(g, &mut resp_json),
                )
                .await;
                output_capture_safe = moderation.capture_safe;
                let verdict = moderation.verdict;
                telemetry.monitor_hits.clone_from(&monitor_hits);
                if let aisix_guardrails::GuardrailVerdict::Block {
                    reason,
                    guardrail_name,
                } = verdict
                {
                    // Per #153 the matched-pattern detail stays in ops logs only.
                    tracing::warn!(
                        guardrail_hook = "output",
                        model = %model_name,
                        reason = %reason,
                        "guardrail blocked /v1/completions response",
                    );
                    // The upstream already billed for this response (tokens
                    // committed above), so return the redacted 422 body BUT
                    // carry the billed `usage` marked `guardrail_blocked` —
                    // recording zero tokens here would let cp-api's ledger
                    // under-report spend the customer was charged for. Same
                    // output analog as responses.rs #543 / chat.rs UpstreamCharge.
                    return Ok(CompletionDispatchSuccess {
                        response: ProxyError::ContentFiltered(
                            crate::error::guardrail_block_message(
                                "response",
                                guardrail_name.as_deref(),
                            ),
                        )
                        .into_response(),
                        provider: provider_label,
                        model_id: model_entry_id.clone(),
                        provider_key_id: provider_key_id.clone(),
                        upstream_model: upstream_model.clone(),
                        usage,
                        provider_request_id,
                        redactions,
                        monitor_hits,
                        error_class: String::new(),
                        guardrail_blocked: true,
                        // Blocked responses never reached the client — no
                        // content capture, matching the chat surface.
                        captured_content: None,
                        usage_handled_by_stream: false,
                        routing: routing.clone(),
                    });
                }
            }

            // #932: mask-action PII rules rewrite the reply text AFTER the
            // block check passes.
            crate::redact::merge_counts(
                &mut redactions,
                crate::redact::redact_completions_response(resolved_chain.as_ref(), &mut resp_json),
            );

            // Content capture (AISIX-Cloud#947): the completion text from the
            // POST-redaction body, so the exported content matches what the
            // caller received.
            let captured_content = match (&captured_prompt, content_cap) {
                (Some(prompt), Some(cap)) if input_capture_safe && output_capture_safe => Some(
                    CapturedContent::new(prompt, &completion_output_text(&resp_json), cap as usize),
                ),
                _ => None,
            };

            Ok(CompletionDispatchSuccess {
                response: Json(resp_json).into_response(),
                provider: provider_label,
                model_id: model_entry_id.clone(),
                provider_key_id: provider_key_id.clone(),
                upstream_model: upstream_model.clone(),
                usage,
                provider_request_id,
                redactions,
                monitor_hits,
                error_class: String::new(),
                guardrail_blocked: false,
                captured_content,
                usage_handled_by_stream: false,
                routing: routing.clone(),
            })
        }
        Err(failure) => {
            // No upstream answer → no tokens to count; release the
            // reservation. Cooldown / health were noted per attempt by the
            // walker.
            reservation.commit_tokens(0).await;
            completion_unsupported_or_error(failure, &model_entry, redactions, monitor_hits)
        }
    }
}

/// Pull the usage counters out of a legacy /v1/completions response
/// body. Returns `None` only when:
///   - The `usage` block is missing entirely (non-conformant edge), or
///   - `usage.prompt_tokens` is missing / non-numeric (malformed)
///
/// Those cases skip UsageEvent emission rather than attributing a
/// zero-everything noise row to the api_key. The `prompt_tokens` gate
/// distinguishes "no upstream usage at all" from a legitimate reply.
///
/// `completion_tokens`, by contrast, defaults to 0 when absent: a 200
/// that reports a prompt side but omits the completion side is still a
/// real billable call (the prompt was processed) and must be recorded.
/// A missing completion side coerces to 0 and the event is still
/// logged/billed (the usage block's `completion_tokens` defaults to 0
/// when absent) — see
/// #429 follow-up. Dropping the whole event would under-record more than
/// the zeroed-completion it was meant to avoid. Wire shape:
/// <https://platform.openai.com/docs/api-reference/completions/object>
fn extract_completion_usage(body: &Value) -> Option<CompletionUsage> {
    let usage = body.get("usage")?;
    let prompt_tokens =
        crate::usage_attr::token_count(usage.get("prompt_tokens").and_then(|v| v.as_u64())?);
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .map(crate::usage_attr::token_count)
        .unwrap_or(0);
    Some(CompletionUsage {
        prompt_tokens,
        completion_tokens,
        usage_estimated: false,
    })
}

/// Count the legacy /v1/completions `prompt` for the token-estimation
/// fallback (AISIX-Cloud#1074): a plain string, an array of strings, an
/// array of token ids (exact count), or an array of token-id arrays.
/// Plain-text counting — the legacy surface has no message overhead.
fn count_completion_prompt(model: &str, prompt: Option<&Value>) -> u32 {
    match prompt {
        Some(Value::String(s)) => crate::token_estimate::count_text(model, s),
        Some(Value::Array(items)) => items.iter().fold(0u32, |acc, item| {
            acc.saturating_add(match item {
                Value::String(s) => crate::token_estimate::count_text(model, s),
                Value::Number(_) => 1,
                Value::Array(tokens) => tokens.len().min(u32::MAX as usize) as u32,
                _ => 0,
            })
        }),
        _ => 0,
    }
}

/// Concatenate the `text` of every choice in a /v1/completions response for
/// output-guardrail scanning (#911 [23]). Missing/non-string `text` fields are
/// skipped; the result is the client-visible completion text the content/DLP
/// output hook must inspect.
fn completion_output_text(body: &Value) -> String {
    body.get("choices")
        .and_then(|c| c.as_array())
        .map(|choices| {
            choices
                .iter()
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Issue #403: push one `UsageEvent` onto cp-api's telemetry sink
/// and fan it out to per-env OTLP exporters. Mirrors the shape of
/// `embeddings::emit_usage_event` (#402) and `responses::emit_usage_event`
/// (#404); the legacy /v1/completions endpoint has both prompt and
/// completion sides but no streaming / reasoning tokens.
///
/// `inbound_protocol = "openai"` per chat.rs convention. The per-PK
/// attribution tags (`provider_kind` / `provider_featured` /
/// `branded_provider` / `pk_label` / `byo_label`) ARE populated — same
/// lookup as chat / messages / responses / embeddings (AISIX-Cloud#867
/// parity) via `usage_attr::apply_pk_telemetry` below.
#[allow(clippy::too_many_arguments)]
fn emit_usage_event(
    state: &ProxyState,
    // The request's snapshot + its one ProviderKey observation, resolved
    // by the handler (#941).
    snap: &aisix_core::AisixSnapshot,
    pk: &crate::usage_attr::ResolvedPk<'_>,
    request_id: &str,
    model_id: &str,
    requested_model: &str,
    api_key_id: &str,
    // Metric labels the UsageEvent has no field for (AISIX-Cloud#1234
    // follow-up): the wire struct is the CP contract, so they ride
    // alongside rather than in it.
    provider: &str,
    upstream_model: &str,
    status_code: u16,
    elapsed: Duration,
    usage: &CompletionUsage,
    provider_request_id: &str,
    // Which group member served and how many attempts it took.
    routing: &crate::routing::RoutingAttribution,
    client: &ClientContext,
    stream: bool,
    error_class: &str,
    guardrail_blocked: bool,
    applied_guardrails: Vec<aisix_core::AppliedGuardrail>,
    // Per-detector PII mask counts (#932). Empty = no redaction.
    redacted_entity_counts: crate::redact::RedactionCounts,
    // Monitor-mode guardrail observations (AISIX-Cloud#562).
    guardrail_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    // Captured request/response content for content-capturing exporters
    // (AISIX-Cloud#947). Forwarded only to `fan_out`, never to the CP sink.
    content: Option<&CapturedContent>,
) {
    let mut event = UsageEvent {
        request_id: request_id.to_string(),
        occurred_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        model_id: model_id.to_string(),
        api_key_id: api_key_id.to_string(),
        requested_model: requested_model.to_string(),
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        attempt_index: routing.attempt_index,
        attempt_kind: routing.attempt_kind.to_string(),
        attempt_model: routing.served_by_model.clone(),
        // Priced from the dispatched row's `Model.cost` when the operator set
        // one, `0.0` otherwise — see `usage_attr::request_cost_usd`.
        cost_usd: crate::usage_attr::request_cost_usd(
            snap,
            model_id,
            u64::from(usage.prompt_tokens),
            u64::from(usage.completion_tokens),
        ),
        usage_estimated: usage.usage_estimated,
        // Single-attempt endpoint: the attempt spans the whole request, so
        // the upstream figure and what the caller waited for coincide.
        upstream_latency_ms: elapsed.as_millis().min(u32::MAX as u128) as u32,
        downstream_latency_ms: elapsed.as_millis().min(u32::MAX as u128) as u32,
        status_code,
        provider_request_id: provider_request_id.to_string(),
        inbound_protocol: "openai".to_string(),
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        error_class: error_class.to_string(),
        // #911 [23]: a billed-then-output-blocked completion surfaces on the
        // dashboard's Blocked tab while still carrying its billed token counts.
        guardrail_blocked,
        applied_guardrails,
        redacted_entity_counts,
        guardrail_monitor_hits,
        ..Default::default()
    };
    crate::usage_attr::apply_pk_telemetry(&mut event, pk);
    crate::usage_attr::apply_jwt_identity(&mut event, client.jwt.as_ref());
    state.usage_sink.try_emit("completions", event.clone());
    let exporters = crate::usage_attr::live_exporters(state, snap);
    state.otlp_fan_out.fan_out(
        &event,
        content,
        exporters.generation(),
        exporters.iter().map(|e| &*e.value),
    );
    let owned_caller = crate::request_metrics::Caller::from_api_key_id(snap, api_key_id);
    crate::request_metrics::record_usage(
        state,
        "/v1/completions",
        owned_caller.as_caller(),
        crate::request_metrics::Upstream {
            provider,
            model: requested_model,
            upstream_model,
            pk: pk.labels(),
            stream,
            ..Default::default()
        },
        crate::request_metrics::Tokens {
            input: usage.prompt_tokens,
            output: usage.completion_tokens,
            total: usage.prompt_tokens.saturating_add(usage.completion_tokens),
            spend_usd: event.cost_usd,
            client_type: state.client_classifier.classify(&client.user_agent),
        },
    );
}
#[allow(clippy::too_many_arguments)]
/// Per-target facts a Model Group resolves differently per member, so none
/// of them can be read off the caller-addressed entry.
struct CompletionAttemptFacts {
    provider: String,
    provider_key_id: String,
    upstream_model: String,
}

/// Render a walk that produced no answer.
///
/// A provider that does not implement legacy completions is a 501 rather
/// than a 502: the request was well-formed and the gateway reached a
/// decision without the upstream failing. Attribution goes to the row that
/// refused — with several group members, only one of them may lack the
/// surface.
fn completion_unsupported_or_error(
    failure: crate::routing::GroupFailure,
    entry: &aisix_core::ResourceEntry<aisix_core::Model>,
    redactions: crate::redact::RedactionCounts,
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
) -> Result<CompletionDispatchSuccess, ProxyError> {
    let msg = match &failure.err {
        ProxyError::Bridge(BridgeError::Config(msg))
            if msg.contains("does not support text completions")
                || msg.contains("text completions") =>
        {
            msg.clone()
        }
        _ => return Err(failure.err),
    };
    let target = failure
        .target
        .clone()
        .unwrap_or_else(|| std::sync::Arc::clone(&entry.value));
    Ok(CompletionDispatchSuccess {
        response: (
            StatusCode::NOT_IMPLEMENTED,
            Json(ErrorEnvelope::new(msg, "not_implemented")),
        )
            .into_response(),
        provider: target
            .provider
            .as_deref()
            .unwrap_or("unknown")
            .to_ascii_lowercase(),
        model_id: if failure.target_id.is_empty() {
            entry.id.to_string()
        } else {
            failure.target_id.clone()
        },
        provider_key_id: String::new(),
        upstream_model: target.upstream_model().unwrap_or("unknown").to_string(),
        // No upstream call → no usage to attribute. The handler gates
        // emission on `usage.is_some()` so a 501 stays out of /logs noise
        // (same convention as #402).
        usage: None,
        provider_request_id: String::new(),
        redactions,
        monitor_hits,
        error_class: String::new(),
        guardrail_blocked: false,
        captured_content: None,
        usage_handled_by_stream: false,
        routing: failure.attribution(),
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_access_log(
    model: &str,
    provider: &str,
    api_key_id: &str,
    status: u16,
    latency: Duration,
    request_id: &str,
    // Provider response id; `None`/empty when the call produced none.
    provider_request_id: Option<&str>,
    // Which group member served and what the walk cost; `None` for a
    // failed request.
    routing: Option<&crate::routing::RoutingAttribution>,
    error: Option<&ProxyError>,
) {
    let (error_kind, error) = match error {
        Some(e) => {
            let (kind, msg) = crate::attempt::access_log_error(e);
            (Some(kind), Some(msg))
        }
        None => (None, None),
    };
    let _now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    AccessLog {
        method: "POST",
        path: "/v1/completions",
        status,
        latency,
        provider: Some(provider),
        model: Some(model),
        api_key_id: Some(api_key_id),
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        request_id,
        provider_request_id: provider_request_id.filter(|s| !s.is_empty()),
        served_by_model: routing
            .map(|r| r.served_by_model.as_str())
            .filter(|s| !s.is_empty()),
        routing_attempt_count: routing.map(|r| r.attempt_count),
        routing_fallback_count: routing.map(|r| r.fallback_count),
        error_kind,
        error: error.as_deref(),
    }
    .emit();
}

#[cfg(test)]
mod tests {

    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, Model, ProxyConfig};
    use aisix_gateway::Hub;
    use aisix_provider_openai::OpenAiBridge;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            addr: "127.0.0.1:0".into(),
            request_body_limit_bytes: Some(1_048_576),
            real_ip: Default::default(),
            request_id: Default::default(),
            url_rewrites: Vec::new(),
            tls: None,
            thread_per_core: None,
            workers: None,
        }
    }

    const PK_ID: &str = "11111111-1111-1111-1111-111111111111";

    fn model_entry(name: &str) -> ResourceEntry<Model> {
        let json = format!(
            r#"{{
                "display_name": "{name}",
                "provider": "openai",
                "model_name": "gpt-3.5-turbo-instruct",
                "provider_key_id": "{PK_ID}"
            }}"#
        );
        let m: Model = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("m-1", m, 1)
    }

    fn provider_key_entry(api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"openai-up","secret":"sk-up","api_base":"{api_base}","provider":"openai","adapter":"openai"}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(PK_ID, pk, 1)
    }

    /// Same PK as `provider_key_entry` (reuses `PK_ID` so existing model
    /// fixtures resolve to it) but carries `telemetry_tags` so the emitted
    /// UsageEvent picks up the per-PK attribution fields (AISIX-Cloud#867).
    fn provider_key_entry_tagged(api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"openai-up","secret":"sk-up","api_base":"{api_base}","provider":"openai","adapter":"openai","telemetry_tags":{{"kind":"catalog","featured":true,"branded_provider":"openai","pk_label":"prod-completions-key"}}}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(PK_ID, pk, 1)
    }

    fn new_snap(api_base: &str) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(provider_key_entry(api_base));
        snap
    }

    fn apikey_entry(allowed: &[&str]) -> ResourceEntry<ApiKey> {
        let json = format!(
            r#"{{"key_hash": "8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891", "allowed_models": {}}}"#,
            serde_json::to_string(&allowed).unwrap()
        );
        let k: ApiKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("k-1", k, 1)
    }

    fn build_app(snap: AisixSnapshot) -> axum::Router {
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        crate::build_router(crate::ProxyState::new(handle, hub, &cfg()).without_cache())
    }

    fn make_req(body: serde_json::Value) -> Request<axum::body::Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("authorization", "Bearer sk-caller")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }

    fn keyword_input_guardrail(literal: &str) -> ResourceEntry<aisix_core::Guardrail> {
        let json = format!(
            r#"{{"name":"t","enabled":true,"hook_point":"input","fail_open":false,"kind":"keyword","patterns":[{{"kind":"literal","value":"{literal}"}}]}}"#
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("g-1", g, 1)
    }

    fn keyword_input_monitor_guardrail(literal: &str) -> ResourceEntry<aisix_core::Guardrail> {
        let json = format!(
            r#"{{"name":"t-monitor","enabled":true,"hook_point":"input","enforcement_mode":"monitor","fail_open":false,"kind":"keyword","patterns":[{{"kind":"literal","value":"{literal}"}}]}}"#
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("g-monitor", g, 1)
    }

    fn keyword_output_guardrail(literal: &str) -> ResourceEntry<aisix_core::Guardrail> {
        let json = format!(
            r#"{{"name":"t","enabled":true,"hook_point":"output","fail_open":false,"kind":"keyword","patterns":[{{"kind":"literal","value":"{literal}"}}]}}"#
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("g-1", g, 1)
    }

    fn pii_output_guardrail(max_buffer_bytes: usize) -> ResourceEntry<aisix_core::Guardrail> {
        pii_output_guardrail_with_policy(max_buffer_bytes, "fail_closed")
    }

    fn pii_output_guardrail_with_policy(
        max_buffer_bytes: usize,
        overflow_policy: &str,
    ) -> ResourceEntry<aisix_core::Guardrail> {
        let json = format!(
            r#"{{"name":"pii","enabled":true,"hook_point":"output","kind":"pii","detectors":[{{"type":"email","action":"mask"}}],"max_buffer_bytes":{max_buffer_bytes},"on_buffer_exceeded":"{overflow_policy}"}}"#
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("g-pii", g, 1)
    }

    /// #554: an output-hook guardrail must inspect the complete legacy
    /// completion stream before any bytes reach the caller. A match returns a
    /// normal OpenAI-style 422 envelope, not a late SSE error after the blocked
    /// text has already leaked.
    #[tokio::test]
    async fn output_guardrail_blocks_stream_before_release_issue_554() {
        let upstream = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"cmpl-stream\",\"object\":\"text_completion\",",
            "\"choices\":[{\"index\":0,\"text\":\"BLOCK\",\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"cmpl-stream\",\"object\":\"text_completion\",",
            "\"choices\":[{\"index\":0,\"text\":\"ME\",\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.guardrails.insert(keyword_output_guardrail("BLOCKME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(aisix_obs::UsageSink::new(tx));
        let resp = tower::ServiceExt::oneshot(
            crate::build_router(state),
            make_req(serde_json::json!({
                "model": "instruct",
                "prompt": "hello",
                "stream": true
            })),
        )
        .await
        .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65_536).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["type"], "content_filter");
        assert!(!String::from_utf8_lossy(&bytes).contains("BLOCKME"));
        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("blocked stream must emit billed usage")
            .expect("usage sink closed");
        assert_eq!(event.status_code, 422);
        assert!(event.guardrail_blocked);
        assert_eq!(event.prompt_tokens, 2);
        assert_eq!(event.completion_tokens, 2);
        assert_eq!(event.applied_guardrails.len(), 1);
        assert_eq!(event.applied_guardrails[0].kind, "keyword");
        assert_eq!(event.applied_guardrails[0].hook, "output");
    }

    /// Clean held streams are released as SSE after the complete output passes
    /// the output guardrail. The provider's frame order and done marker remain
    /// intact.
    #[tokio::test]
    async fn output_guardrail_releases_clean_stream_as_sse_issue_554() {
        let upstream = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"cmpl-clean\",\"object\":\"text_completion\",",
            "\"choices\":[{\"index\":0,\"text\":\"hello\",\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"cmpl-clean\",\"object\":\"text_completion\",",
            "\"choices\":[{\"index\":0,\"text\":\" world\",\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.guardrails.insert(keyword_output_guardrail("BLOCKME"));

        let resp = tower::ServiceExt::oneshot(
            build_app(snap),
            make_req(serde_json::json!({
                "model": "instruct",
                "prompt": "hello",
                "stream": true
            })),
        )
        .await
        .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        let bytes = to_bytes(resp.into_body(), 65_536).await.unwrap();
        assert_eq!(bytes.as_ref(), sse.as_bytes());
    }

    /// Without a blocking output policy, completion bytes remain a live
    /// passthrough and the terminal usage frame is accounted after body
    /// consumption instead of being lost when the handler returns.
    #[tokio::test]
    async fn unguarded_stream_forwards_and_emits_terminal_usage() {
        let upstream = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"cmpl-live\",\"object\":\"text_completion\",",
            "\"choices\":[{\"index\":0,\"text\":\"hello\",\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"cmpl-live\",\"object\":\"text_completion\",",
            "\"choices\":[{\"index\":0,\"text\":\" world\",\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(aisix_obs::UsageSink::new(tx));

        let resp = tower::ServiceExt::oneshot(
            crate::build_router(state),
            make_req(serde_json::json!({
                "model": "instruct",
                "prompt": "hello",
                "stream": true
            })),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65_536).await.unwrap();
        assert_eq!(bytes.as_ref(), sse.as_bytes());

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("live stream must emit usage at EOF")
            .expect("usage sink closed");
        assert_eq!(event.status_code, 200);
        assert_eq!(event.prompt_tokens, 3);
        assert_eq!(event.completion_tokens, 4);
        assert_eq!(event.provider_request_id, "cmpl-live");
    }

    /// A maskable span may cross provider frames. The held stream must rebuild
    /// the choice channel before redaction, then release only the masked value.
    #[tokio::test]
    async fn pii_mask_reassembles_text_across_completion_frames() {
        let upstream = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"cmpl-pii\",\"object\":\"text_completion\",",
            "\"choices\":[{\"index\":0,\"text\":\"alice@\",\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"cmpl-pii\",\"object\":\"text_completion\",",
            "\"choices\":[{\"index\":0,\"text\":\"example.com\",\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.guardrails.insert(pii_output_guardrail(65_536));
        let resp = tower::ServiceExt::oneshot(
            build_app(snap),
            make_req(serde_json::json!({
                "model": "instruct",
                "prompt": "hello",
                "stream": true
            })),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65_536).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!text.contains("alice@example.com"));
        assert!(text.contains("[EMAIL_REDACTED]"));
        assert!(text.contains("data: [DONE]"));
    }

    /// BufferFull is bounded. A fail-closed output guardrail returns 422 before
    /// releasing any upstream bytes when the configured cap is exceeded.
    #[tokio::test]
    async fn output_guardrail_stream_buffer_overflow_fails_closed() {
        let upstream = MockServer::start().await;
        let sse = format!(
            "data: {{\"id\":\"cmpl-large\",\"object\":\"text_completion\",\"choices\":[{{\"index\":0,\"text\":\"{}\",\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
            "x".repeat(256),
        );
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.guardrails.insert(pii_output_guardrail(64));
        let resp = tower::ServiceExt::oneshot(
            build_app(snap),
            make_req(serde_json::json!({
                "model": "instruct",
                "prompt": "hello",
                "stream": true
            })),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65_536).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!text.contains(&"x".repeat(64)));
    }

    /// The configured overflow action is part of the guardrail contract: a
    /// fail-open BufferFull chain releases the prefix and remaining live stream
    /// byte-for-byte instead of converting the overflow into a 422.
    #[tokio::test]
    async fn output_guardrail_stream_buffer_overflow_honors_fail_open() {
        let upstream = MockServer::start().await;
        let sse = format!(
            "data: {{\"id\":\"cmpl-large\",\"object\":\"text_completion\",\"choices\":[{{\"index\":0,\"text\":\"{}\",\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
            "x".repeat(256),
        );
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse.clone(), "text/event-stream"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.guardrails
            .insert(pii_output_guardrail_with_policy(64, "fail_open"));
        let resp = tower::ServiceExt::oneshot(
            build_app(snap),
            make_req(serde_json::json!({
                "model": "instruct",
                "prompt": "hello",
                "stream": true
            })),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65_536).await.unwrap();
        assert_eq!(bytes.as_ref(), sse.as_bytes());
    }

    #[tokio::test]
    async fn live_stream_transport_error_records_failure_status_and_cooldown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let event = b"data: {\"id\":\"cmpl-partial\",\"choices\":[{\"index\":0,\"text\":\"partial\"}]}\n\n";
            let headers = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            socket.write_all(headers).await.unwrap();
            socket
                .write_all(format!("{:x}\r\n", event.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(event).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            // Drop without the terminating zero-length chunk: reqwest yields
            // one valid SSE item followed by a body transport error.
        });

        let snap = new_snap(&format!("http://{address}"));
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let (usage_tx, mut usage_rx) = tokio::sync::mpsc::channel(4);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(aisix_obs::UsageSink::new(usage_tx));
        let state_probe = state.clone();
        let response = tower::ServiceExt::oneshot(
            crate::build_router(state),
            make_req(serde_json::json!({
                "model": "instruct",
                "prompt": "hello",
                "stream": true
            })),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            to_bytes(response.into_body(), 65_536).await.is_err(),
            "truncated upstream stream must surface a body error"
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), usage_rx.recv())
            .await
            .expect("stream failure must emit usage")
            .expect("usage sink closed");
        assert_eq!(event.status_code, 502);
        assert_eq!(event.error_class, "transport_error");
        let runtime = state_probe.runtime_status.status("m-1");
        assert_eq!(runtime.status, crate::health::RuntimeStatus::Cooldown);
        assert_eq!(runtime.status_reason.as_deref(), Some("transport_error"));
        server.await.unwrap();
    }

    #[test]
    fn precommit_timeout_records_health_and_runtime_failure() {
        let health = crate::health::HealthTracker::new();
        let runtime = crate::health::ModelRuntimeStatusTracker::new();
        for _ in 0..3 {
            health.record_failure("instruct");
        }

        let error = super::note_completion_failure(
            &health,
            &runtime,
            "instruct",
            "m-1",
            Some(&aisix_core::CooldownConfig::default()),
            aisix_gateway::BridgeError::Timeout {
                elapsed_ms: 300,
                cause: "first stream event".into(),
            },
        );

        assert_eq!(error.http_status(), 504);
        assert_eq!(
            health.level("instruct"),
            crate::health::HealthLevel::Degraded
        );
        let status = runtime.status("m-1");
        assert_eq!(status.status, crate::health::RuntimeStatus::Cooldown);
        assert_eq!(status.status_reason.as_deref(), Some("request_timeout"));
    }

    /// #545: a configured input guardrail must fire on /v1/completions — a
    /// blocked `prompt` returns 422 content_filter and the upstream is never
    /// contacted (`expect(0)`).
    #[tokio::test]
    async fn input_guardrail_blocks_prompt_returns_422() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"object":"text_completion"})),
            )
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.guardrails.insert(keyword_input_guardrail("BLOCKME"));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "instruct", "prompt": "please BLOCKME now"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
        assert!(!v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("BLOCKME"));
    }

    /// A monitor observation belongs to the request even if the later
    /// provider call fails. The error UsageEvent must not silently discard it.
    #[tokio::test]
    async fn input_monitor_hit_survives_upstream_error() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream failed"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.guardrails
            .insert(keyword_input_monitor_guardrail("WATCHME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(aisix_obs::UsageSink::new(tx));
        let resp = tower::ServiceExt::oneshot(
            crate::build_router(state),
            make_req(serde_json::json!({
                "model": "instruct",
                "prompt": "please WATCHME"
            })),
        )
        .await
        .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("failed request must emit usage")
            .expect("usage sink closed");
        assert!(!event.guardrail_blocked);
        assert_eq!(event.applied_guardrails.len(), 1);
        assert_eq!(event.guardrail_monitor_hits.len(), 1);
        let hit = &event.guardrail_monitor_hits[0];
        assert_eq!(hit.guardrail_name, "t-monitor");
        assert_eq!(hit.hook, "input");
        assert_eq!(hit.action, "would_block");
        assert!(!hit.reason.contains("WATCHME"));
    }

    /// #545 companion: a benign prompt with a guardrail configured still
    /// forwards (`expect(1)`) and returns 200.
    /// A Model Group addressed on /v1/completions dispatches its targets and
    /// falls over — both for a plain call and for a streamed one.
    ///
    /// Streaming is the half that would silently rot: the walk has to commit
    /// the 200 to one target only after its first chunk arrives, so a target
    /// that accepts the connection and then dies still fails over instead of
    /// handing the caller a broken stream.
    fn group_snapshot(dead: &str, live: &str) -> AisixSnapshot {
        const PK_B: &str = "33333333-3333-3333-3333-333333333333";
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(provider_key_entry(dead));
        let pk_b: aisix_core::ProviderKey = serde_json::from_str(&format!(
            r#"{{"display_name":"openai-b","secret":"sk-up","api_base":"{live}","provider":"openai","adapter":"openai"}}"#
        ))
        .unwrap();
        snap.provider_keys.insert(ResourceEntry::new(PK_B, pk_b, 1));
        for (id, name, pk) in [
            ("m-dead", "cmpl-dead", PK_ID),
            ("m-live", "cmpl-live", PK_B),
        ] {
            let m: Model = serde_json::from_str(&format!(
                r#"{{"display_name":"{name}","provider":"openai","model_name":"gpt-3.5-turbo-instruct","provider_key_id":"{pk}"}}"#
            ))
            .unwrap();
            snap.models.insert(ResourceEntry::new(id, m, 1));
        }
        let group: Model = serde_json::from_str(
            r#"{
                "display_name": "cmpl-group",
                "routing": {
                    "targets": [{"model": "cmpl-dead"}, {"model": "cmpl-live"}]
                }
            }"#,
        )
        .unwrap();
        snap.models.insert(ResourceEntry::new("m-group", group, 1));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap
    }

    #[tokio::test]
    async fn model_group_fails_over_on_completions() {
        let dead = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .expect(1)
            .mount(&dead)
            .await;
        let live = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "text_completion",
                "choices": [{"text": "ok", "index": 0, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .expect(1)
            .mount(&live)
            .await;

        let app = build_app(group_snapshot(&dead.uri(), &live.uri()));
        let resp = tower::ServiceExt::oneshot(
            app,
            make_req(serde_json::json!({"model": "cmpl-group", "prompt": "hi"})),
        )
        .await
        .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a Model Group must serve /v1/completions from a healthy target",
        );
    }

    #[tokio::test]
    async fn model_group_fails_over_on_streaming_completions() {
        let dead = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .expect(1)
            .mount(&dead)
            .await;
        let live = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"object\":\"text_completion\",\"choices\":[{\"text\":\"ok\",\"index\":0}]}\n\ndata: [DONE]\n\n",
                    ),
            )
            .expect(1)
            .mount(&live)
            .await;

        let app = build_app(group_snapshot(&dead.uri(), &live.uri()));
        let resp = tower::ServiceExt::oneshot(
            app,
            make_req(serde_json::json!({
                "model": "cmpl-group",
                "prompt": "hi",
                "stream": true
            })),
        )
        .await
        .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a streamed Model Group request must fail over to a healthy target",
        );
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("[DONE]"),
            "the surviving target's stream must reach the caller: {text}",
        );
    }

    #[tokio::test]
    async fn input_guardrail_allows_benign_prompt_forwards_200() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "text_completion",
                "choices": [{"text": "ok", "index": 0, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.guardrails.insert(keyword_input_guardrail("BLOCKME"));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "instruct", "prompt": "a fine prompt"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "text_completion");
    }

    #[tokio::test]
    async fn happy_path_forwards_to_completions_endpoint() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-abc",
                "object": "text_completion",
                "created": 1_700_000_000i64,
                "model": "gpt-3.5-turbo-instruct",
                "choices": [{
                    "text": " is a test",
                    "index": 0,
                    "logprobs": null,
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 4, "total_tokens": 9}
            })))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "instruct", "prompt": "Say this"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "text_completion");
        assert_eq!(v["choices"][0]["text"], " is a test");
    }

    #[tokio::test]
    async fn unauthenticated_request_returns_401() {
        let snap = new_snap("http://unused");
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/completions")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"model":"instruct","prompt":"hi"}"#,
            ))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn forbidden_model_returns_403() {
        let snap = new_snap("http://unused");
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["other-model"]));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "instruct", "prompt": "hi"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn unknown_model_returns_404() {
        let snap = new_snap("http://unused");
        snap.apikeys.insert(apikey_entry(&["*"]));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "nonexistent", "prompt": "hi"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn upstream_error_propagates_as_502() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("error"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "instruct", "prompt": "hi"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    /// Issue #403: a successful /v1/completions call must emit a
    /// `UsageEvent` with the upstream-reported prompt + completion
    /// tokens, status_code, model_id, api_key_id, and
    /// `inbound_protocol = "openai"`. Pre-#403 the legacy
    /// completions handler dropped the event entirely.
    #[tokio::test]
    async fn emits_usage_event_on_200_with_tokens_issue_403() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::start().await;
        // Legacy OpenAI completions wire shape. Pin specific token
        // counts so a regression that swapped prompt/completion
        // semantics would fail here.
        let upstream_body = serde_json::json!({
            "id": "cmpl-up-1",
            "object": "text_completion",
            "model": "gpt-3.5-turbo-instruct",
            "choices": [{
                "text": "hi",
                "index": 0,
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        });
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let body = serde_json::json!({"model": "instruct", "prompt": "hello"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for /v1/completions 200")
            .expect("usage_sink sender dropped");

        assert_eq!(event.prompt_tokens, 11);
        assert_eq!(event.completion_tokens, 7);
        assert_eq!(event.status_code, 200);
        assert_eq!(event.api_key_id, "k-1");
        assert_eq!(event.model_id, "m-1");
        assert_eq!(event.inbound_protocol, "openai");
        assert!(!event.request_id.is_empty());
        assert!(!event.occurred_at.is_empty());
    }

    /// AISIX-Cloud#1289: the legacy completions response object carries a
    /// `cmpl-…` id, and it must reach the UsageEvent — the handler recorded
    /// none before, so this endpoint's calls had nothing an operator could
    /// look up in the provider's console. Fails before the fix (empty),
    /// passes after.
    #[tokio::test]
    async fn records_the_provider_response_id_1289() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl_1289",
                "object": "text_completion",
                "model": "gpt-3.5-turbo-instruct",
                "choices": [{"index": 0, "text": "hi", "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
            })))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));

        let resp = tower::ServiceExt::oneshot(
            crate::build_router(state),
            make_req(serde_json::json!({"model": "instruct", "prompt": "hello"})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted")
            .expect("usage_sink sender dropped");
        assert_eq!(event.provider_request_id, "cmpl_1289");
        assert_ne!(event.request_id, event.provider_request_id);
    }

    /// Companion: an upstream 200 with `usage: {}` (malformed —
    /// `prompt_tokens` is a required field on every legitimate
    /// completion response) now emits an ESTIMATED usage event
    /// (AISIX-Cloud#1074) instead of dropping the record: the tokens
    /// are counted locally and the event is marked `usage_estimated`.
    /// (Pre-#1074 this dropped the event entirely — per audit MEDIUM-1
    /// on PR #425 — which left the request invisible to billing.)
    #[tokio::test]
    async fn estimates_usage_event_when_upstream_usage_block_is_empty() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::start().await;
        let upstream_body = serde_json::json!({
            "id": "cmpl-up-1",
            "object": "text_completion",
            "choices": [],
            "usage": {}  // malformed — prompt_tokens required by spec
        });
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let body = serde_json::json!({"model": "instruct", "prompt": "x"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("estimated UsageEvent must be emitted when usage block is malformed")
            .expect("usage_sink sender dropped");
        // prompt "x" = 1 token; the upstream body has no choices text, so
        // the completion side stays 0 (nothing to count).
        assert_eq!(event.prompt_tokens, 1);
        assert_eq!(event.completion_tokens, 0);
        assert!(
            event.usage_estimated,
            "locally-counted tokens must be flagged"
        );
    }

    /// #429 follow-up: a 200 whose `usage` carries
    /// `prompt_tokens` but omits `completion_tokens` is still a real
    /// billable call — the prompt was processed. It MUST emit a
    /// UsageEvent with `completion_tokens = 0` (coercing the missing
    /// side to 0), NOT be dropped. Only a fully
    /// absent / prompt-less usage block skips (see the two tests below).
    #[tokio::test]
    async fn emits_with_zero_completion_when_completion_tokens_missing() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::start().await;
        let upstream_body = serde_json::json!({
            "id": "cmpl-up-1",
            "object": "text_completion",
            "choices": [],
            "usage": { "prompt_tokens": 50 }  // missing completion_tokens
        });
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let body = serde_json::json!({"model": "instruct", "prompt": "x"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must still be emitted when only completion_tokens is missing")
            .expect("usage_sink sender dropped");
        assert_eq!(event.prompt_tokens, 50, "prompt side must be recorded");
        assert_eq!(
            event.completion_tokens, 0,
            "missing completion_tokens must default to 0, not drop the event"
        );
    }

    /// Per #655 parity (was #403 negative pinning): an upstream 5xx now emits
    /// ONE zero-token UsageEvent so the failed request is visible in Logs
    /// (status + error class) and attributed to the api_key — instead of being
    /// dropped, as the non-chat handlers used to do. The 501 NotImplemented
    /// path still emits nothing (no upstream call); see the test below.
    #[tokio::test]
    async fn upstream_5xx_emits_zero_token_error_event() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let body = serde_json::json!({"model": "instruct", "prompt": "x"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("a failed /v1/completions must emit a zero-token UsageEvent")
            .expect("usage_sink sender dropped");
        assert_eq!(ev.status_code, 502, "upstream 5xx maps to 502");
        assert_eq!(ev.prompt_tokens, 0);
        assert_eq!(ev.completion_tokens, 0);
        assert_eq!(ev.api_key_id, "k-1");
        assert_eq!(ev.requested_model, "instruct");
        assert!(
            !ev.error_class.is_empty(),
            "error_class must classify the failure"
        );
        assert!(
            rx.try_recv().is_err(),
            "exactly one event per failed request"
        );
    }

    /// Issue #403 audit MEDIUM-3: the 501 NotImplemented path
    /// (provider doesn't support text completions) must not emit
    /// a UsageEvent — no upstream call happened, so no usage to
    /// attribute. Without this test, a future regression that
    /// flipped `usage: None` → `Some(zero)` on the 501 branch
    /// would silently emit a bogus zero event. Triggers the path
    /// by routing /v1/completions at an Anthropic-backed model;
    /// `AnthropicBridge` doesn't override `Bridge::complete()`
    /// so the trait default returns `BridgeError::Config(...)`
    /// which maps to 501.
    #[tokio::test]
    async fn provider_lacking_complete_returns_501_without_emit() {
        use aisix_obs::UsageSink;
        use aisix_provider_anthropic::AnthropicBridge;

        const ANTHROPIC_PK_ID: &str = "22222222-2222-2222-2222-222222222222";

        let anthropic_pk_json = r#"{"display_name":"anthropic-up","secret":"sk-ant-test","provider":"anthropic","adapter":"anthropic"}"#;
        let anthropic_pk: aisix_core::ProviderKey =
            serde_json::from_str(anthropic_pk_json).unwrap();
        let anthropic_pk_entry = ResourceEntry::new(ANTHROPIC_PK_ID, anthropic_pk, 1);

        let anthropic_model_json = format!(
            r#"{{"display_name":"claude-instruct","provider":"anthropic","model_name":"claude-3-haiku-20240307","provider_key_id":"{ANTHROPIC_PK_ID}"}}"#
        );
        let anthropic_model: Model = serde_json::from_str(&anthropic_model_json).unwrap();
        let anthropic_model_entry = ResourceEntry::new("m-anthropic", anthropic_model, 1);

        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(anthropic_pk_entry);
        snap.models.insert(anthropic_model_entry);
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let body = serde_json::json!({"model": "claude-instruct", "prompt": "hi"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_IMPLEMENTED,
            "Anthropic-backed /v1/completions must surface as 501 \
             (default Bridge::complete returns BridgeError::Config)",
        );

        let recv = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        if let Ok(Some(ev)) = recv {
            panic!(
                "501 NotImplemented must not emit UsageEvent, \
                 got prompt_tokens={}, status_code={}",
                ev.prompt_tokens, ev.status_code,
            );
        }
    }

    /// A 200 response with NO `usage` block at all (vs `usage: {}`
    /// which is empty-but-present) emits an ESTIMATED usage event
    /// (AISIX-Cloud#1074) — the request must not stay invisible to
    /// billing. (Pre-#1074, per issue #403 audit LOW-1, this dropped
    /// the event entirely.)
    #[tokio::test]
    async fn estimates_usage_event_when_upstream_omits_usage_block_entirely() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::start().await;
        // No `usage` key at all — distinct from `usage: {}`.
        let upstream_body = serde_json::json!({
            "id": "cmpl-no-usage",
            "object": "text_completion",
            "choices": []
        });
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let body = serde_json::json!({"model": "instruct", "prompt": "x"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("estimated UsageEvent must be emitted when `usage` is absent")
            .expect("usage_sink sender dropped");
        // prompt "x" = 1 token; no choices text → completion stays 0.
        assert_eq!(event.prompt_tokens, 1);
        assert_eq!(event.completion_tokens, 0);
        assert!(
            event.usage_estimated,
            "locally-counted tokens must be flagged"
        );
    }

    /// AISIX-Cloud#867 parity: a successful /v1/completions 200 must stamp
    /// the five per-PK telemetry attribution fields (provider_kind /
    /// provider_featured / branded_provider / pk_label / byo_label) onto the
    /// emitted UsageEvent, sourced from the resolved ProviderKey's
    /// `telemetry_tags` — exactly like `/v1/responses` and `/v1/embeddings`.
    /// Pre-fix the completions emitter left these at Default (wire NULL).
    #[tokio::test]
    async fn emits_provider_telemetry_tags_issue_867() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::start().await;
        let upstream_body = serde_json::json!({
            "id": "cmpl-up-1",
            "object": "text_completion",
            "model": "gpt-3.5-turbo-instruct",
            "choices": [{"text": "hi", "index": 0, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        });
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys
            .insert(provider_key_entry_tagged(&upstream.uri()));
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let body = serde_json::json!({"model": "instruct", "prompt": "hello"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for /v1/completions 200")
            .expect("usage_sink sender dropped");
        assert_eq!(ev.provider_kind, "catalog");
        assert!(ev.provider_featured);
        assert_eq!(ev.branded_provider, "openai");
        assert_eq!(ev.pk_label, "prod-completions-key");
    }

    /// #701: an upstream 5xx must mark the model's runtime status (cooldown)
    /// so a flapping upstream reached only via /v1/completions trips the
    /// circuit breaker like rerank/audio/chat. Pre-#701 the status stayed
    /// Healthy.
    #[tokio::test]
    async fn upstream_5xx_marks_cooldown_issue_701() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("instruct"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg()).without_cache();
        let app = crate::build_router(state.clone());

        let body = serde_json::json!({"model": "instruct", "prompt": "x"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let status = state.runtime_status.status("m-1");
        assert!(
            status.cooldown_until.is_some(),
            "a 500 must mark the model in cooldown, got {status:?}"
        );
    }
}
