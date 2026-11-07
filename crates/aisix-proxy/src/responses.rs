//! `POST /v1/responses` — OpenAI Responses API pass-through.
//!
//! The Responses API is an OpenAI-specific endpoint that lets callers
//! interact with the stateful responses surface (`gpt-4o` + tools). The
//! gateway proxies it transparently:
//!
//! 1. Authenticate and authorise the API key + model.
//! 2. Validate the model is an OpenAI provider.
//! 3. Rewrite the `model` field to the upstream model name.
//! 4. Relay the reply — streaming SSE and non-streaming JSON both work.
//!    The body is the upstream's own, byte for byte, except the
//!    caller-facing `model` name (see [`crate::model_echo`]).
//!
//! Only OpenAI models support this endpoint. Non-OpenAI models receive a
//! 400 with an explanatory message.

use aisix_gateway::{ChatFormat, ChatMessage, Role};
use aisix_obs::{
    content_capture_cap, AccessLog, CapturedContent, LatencyLabels, UsageEvent, UsageLabels,
};
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde_json::Value;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::attempt::{
    attempt_error_from_proxy, attempt_reached_upstream, ms_since, AttemptInfo, AttemptRecord,
    DeferredAttempt, RoutingTelemetry,
};
use crate::auth::AuthenticatedKey;
use crate::chat::sanitize_tag;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::guardrail_stream::{synth_chat_response, EosOutputScan};
use crate::state::ProxyState;
use crate::usage_attr::{total_tokens_with_cache, ResolvedPk};

type RawResponsesByteStream = std::pin::Pin<
    Box<
        dyn futures::Stream<Item = Result<bytes::Bytes, crate::stream_timeout::RawStreamError>>
            + Send,
    >,
>;

/// Per-request payload from a successful dispatch — carries the
/// response + provider label + the bits of usage data needed for
/// UsageEvent emission (#404). On the verbatim streaming path the
/// emission is owned by the response stream's Drop guard (#808), so
/// `usage = None` here and `usage_handled_by_stream = true` tells the
/// handler not to double-emit.
struct ResponseDispatchSuccess {
    response: Response,
    provider: String,
    /// Set on the non-streaming 2xx path and the buffered output-guardrail
    /// path (both parse the full body here). `None` on the verbatim
    /// streaming path, where the stream's Drop guard emits the UsageEvent
    /// from the terminal SSE event instead (#808).
    usage: Option<ResponseUsage>,
    /// UUID of the resolved Model row — needed for UsageEvent
    /// `model_id` field. Always present on success.
    model_id: String,
    /// UUID of the resolved ProviderKey for the winning target — feeds the
    /// per-PK telemetry attribution tags (provider_kind / branded_provider /
    /// pk_label / …) on the emitted UsageEvent (#867). Empty when
    /// the target carried no provider_key_id.
    provider_key_id: String,
    /// The provider-side model name the winning attempt actually called,
    /// for the `upstream_model` metric label (#1234). Same value
    /// chat + messages report, so a query can group all three endpoints by
    /// the model the provider was billed for rather than the alias.
    upstream_model: String,
    /// Per-attempt routing telemetry (#655): the failed attempts that
    /// preceded the winner plus the winning attempt itself.
    routing: RoutingTelemetry,
    /// #543: set when an OUTPUT guardrail blocked this response. The
    /// upstream already billed, so this is returned as a "success" carrying
    /// the billed `usage` + a 422 body, and the emitted UsageEvent is marked
    /// `guardrail_blocked` so the dashboard's Blocked tab + budget ledger see
    /// it (silently zeroing the tokens would underreport spend the operator
    /// paid the provider for).
    guardrail_blocked: bool,
    /// The refusal's own text, from [`crate::attempt::gateway_error_message`],
    /// so the usage event names the block the way an input refusal does.
    /// Empty when nothing was blocked.
    guardrail_block_message: String,
    /// `true` on the verbatim streaming path: the response stream's Drop
    /// guard owns the UsageEvent emit (parsed from the terminal SSE event),
    /// so the top-level handler must NOT emit the winner event again (#808).
    usage_handled_by_stream: bool,
    /// Captured request/response content for content-capturing exporters
    /// (#947). `Some` only when an enabled exporter opted into
    /// `content_mode = full`; threaded to `fan_out` via the handler's emit,
    /// never to the CP sink. `None` on the streaming paths, whose
    /// end-of-stream emit owns the capture.
    captured_content: Option<CapturedContent>,
    /// Per-detector PII mask counts applied to the response body (#932),
    /// non-streaming + buffered paths. Merged with the input-side counts by
    /// the handler before the terminal emit. Empty on the live streaming
    /// paths — their end-of-stream closures own the output-side counts.
    output_redactions: crate::redact::RedactionCounts,
    /// Monitor-mode guardrail observations on the response side
    /// (#562), same lifecycle as `output_redactions`.
    output_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    /// Whether a cache policy answered this request, and what a hit saved.
    cache: crate::response_cache::CacheTelemetry,
}

/// Dispatch error carrying the per-attempt telemetry accumulated before
/// the request ultimately failed (#655). Mirrors `chat::DispatchFailure`.
/// Run the output chain over a cached `/v1/responses` body, returning the
/// bytes to serve.
///
/// The same steps the fresh non-streaming response takes — synthesize the
/// output text, check the non-segment verdict, moderate, block, then apply
/// PII masking — so a hit and a miss are governed identically. A no-op (and
/// no re-serialisation) when no output guardrail is attached, which is the
/// common deployment.
async fn guard_cached_response(
    chain: &aisix_guardrails::GuardrailChain,
    model_name: &str,
    model: &aisix_core::Model,
    body: Vec<u8>,
    redactions_out: &mut crate::redact::RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::GuardrailMonitorHit>,
) -> Result<GuardedCache, ResponsesDispatchError> {
    if !aisix_guardrails::Guardrail::runs_on_output(chain) {
        return Ok(GuardedCache {
            body,
            capture_safe: true,
        });
    }
    // A stored body that no longer parses cannot be moderated, so it is not
    // served: failing closed on an unreadable cache entry is the only safe
    // reading when the alternative is relaying it unchecked.
    let Ok(mut json_body) = serde_json::from_slice::<Value>(&body) else {
        tracing::warn!(
            model = %model_name,
            "cached response body did not parse; refusing to serve it unchecked",
        );
        return Err(crate::error::guardrail_block_error("response", None, None).into());
    };
    let upstream_model = model.upstream_model().unwrap_or("unknown").to_string();
    let synth = synth_chat_response(&upstream_model, responses_output_text(&json_body));
    let (verdict, hits) =
        aisix_guardrails::Guardrail::check_output_non_segment_observed(chain, &synth).await;
    // Threaded out, not dropped: a monitor guardrail that fires on a cached
    // body has to appear in the request's hits, and a mask applied to it has
    // to appear in the redaction counts — otherwise every cache-served
    // request reports zero of both.
    monitor_hits_out.extend(hits);
    let moderation = crate::redact::moderate_responses_response_structured(
        chain,
        verdict,
        &mut json_body,
        redactions_out,
        monitor_hits_out,
    )
    .await;
    let mut verdict = moderation.verdict;
    if !verdict.is_block() {
        let redaction = crate::redact::redact_responses_response_structured(chain, &mut json_body);
        crate::redact::merge_counts(redactions_out, redaction.counts);
        if redaction.unrewritable_tool_key {
            verdict = crate::redact::unrewritable_tool_key_verdict();
        }
    }
    if let aisix_guardrails::GuardrailVerdict::Block {
        reason,
        guardrail_name,
        unavailable,
    } = verdict
    {
        tracing::warn!(
            guardrail_hook = "output",
            model = %model_name,
            reason = %reason,
            "guardrail blocked a cached /v1/responses response",
        );
        return Err(crate::error::guardrail_block_error(
            "response",
            guardrail_name.as_deref(),
            unavailable.as_deref(),
        )
        .into());
    }
    Ok(GuardedCache {
        body: serde_json::to_vec(&json_body).unwrap_or(body),
        capture_safe: moderation.capture_safe,
    })
}

/// A cached body that has been through the output chain, and whether the
/// chain left it safe to hand to a content-capturing exporter. Mirrors the
/// `/v1/messages` twin.
struct GuardedCache {
    body: Vec<u8>,
    capture_safe: bool,
}

/// Write path of the `/v1/responses` cache gate — the same shape
/// `/v1/messages` uses. See `messages::store_in_cache` for why the body is
/// buffered only when a policy matched and why non-200s are not stored.
async fn store_in_cache(
    state: &ProxyState,
    gate: &crate::response_cache::BodyCache,
    success: ResponseDispatchSuccess,
) -> ResponseDispatchSuccess {
    let status = gate.miss_status();
    gate.record(state, status);
    let mut success = success;
    success.cache = crate::response_cache::CacheTelemetry::of(status);
    // A blocked response carries a 422 and a guardrail body; storing it would
    // replay the block to a caller whose policy may have changed since.
    if success.response.status() != axum::http::StatusCode::OK
        || success.usage_handled_by_stream
        || success.guardrail_blocked
        || success
            .response
            .extensions()
            .get::<crate::response_cache::UnstorableAnswer>()
            .is_some()
    {
        crate::response_cache::apply_cache_headers(state, &mut success.response, status);
        return success;
    }
    let content_type = success
        .response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let (parts, body) = success.response.into_parts();
    let bytes = match axum::body::to_bytes(body, crate::response_cache::MAX_CACHED_BODY_BYTES).await
    {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(
                target: "aisix::cache",
                error = %err,
                "response body could not be buffered for caching",
            );
            success.response = ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamDecode(
                "response body too large to relay".to_string(),
            ))
            .into_response();
            return success;
        }
    };
    let (prompt_tokens, completion_tokens) = success
        .usage
        .as_ref()
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));
    if crate::response_cache::body_is_storable(&bytes) {
        gate.store(aisix_cache::CachedBody {
            content_type,
            body: bytes.to_vec(),
            prompt_tokens,
            completion_tokens,
        })
        .await;
    }
    success.response = Response::from_parts(parts, axum::body::Body::from(bytes));
    crate::response_cache::apply_cache_headers(state, &mut success.response, status);
    success
}

struct ResponsesDispatchError {
    err: ProxyError,
    routing: RoutingTelemetry,
}

impl From<ProxyError> for ResponsesDispatchError {
    /// Pre-attempt `?` failures (model-not-found, auth, budget) carry no
    /// recorded attempts.
    fn from(err: ProxyError) -> Self {
        Self {
            err,
            routing: RoutingTelemetry::default(),
        }
    }
}

/// Subset of the OpenAI Responses-API `usage` block the gateway
/// surfaces for telemetry (plus the two Anthropic cache counters carried
/// only on the #825 cross-provider bridge path). Other fields (`total_tokens`,
/// `output_tokens_details.audio_tokens`, etc.) are intentionally
/// dropped here — the control plane's `control plane_usage_events` table records only
/// the ones below.
#[derive(Default, Clone)]
struct ResponseUsage {
    /// `true` once the upstream emitted a successful semantic terminal event,
    /// i.e. `response.completed` or `response.incomplete`. Stays `false` when
    /// the consumer went away first, which is what the telemetry closure turns
    /// into `499`.
    ///
    /// Set before the end-of-stream guardrail scan: SDK clients routinely
    /// close right after the terminal frame and drop this generator at that
    /// await, and such a request was delivered in full — marking it later
    /// would report it as abandoned.
    reached_end: bool,
    prompt_tokens: u32,
    completion_tokens: u32,
    /// True when any token counter was filled by the local estimator
    /// because the upstream reported no usage (#1074).
    usage_estimated: bool,
    /// o1/o3/GPT-5 class models surface reasoning tokens as a
    /// subset of `completion_tokens` via
    /// `usage.output_tokens_details.reasoning_tokens`. Zero for
    /// models that don't expose this.
    reasoning_tokens: u32,
    /// OpenAI prompt-cache hit count, subset of `prompt_tokens`,
    /// surfaced via `usage.input_tokens_details.cached_tokens`.
    cached_prompt_tokens: u32,
    cache_write_tokens: Option<u32>,
    modality: aisix_gateway::ModalityTokens,
    cache_creation_ttl: aisix_gateway::CacheCreationTtl,
    /// Anthropic `cache_creation_input_tokens` (cache write). Always 0 on
    /// the verbatim OpenAI path; carried for the cross-provider bridge
    /// path (#825) so an Anthropic-backed /v1/responses call bills cache
    /// writes the same way /v1/messages does.
    cache_creation_tokens: u32,
    /// Anthropic `cache_read_input_tokens` (cache read). Always 0 on the
    /// verbatim OpenAI path (OpenAI surfaces cache hits via
    /// `cached_prompt_tokens` instead).
    cache_read_tokens: u32,
    /// The upstream's own `usage.total_tokens` (0 = not reported); see
    /// `UsageStats::upstream_total_tokens`.
    upstream_total_tokens: u32,
    /// See `UsageStats::reasoning_folded_into_completion`. Always 0 on the
    /// verbatim path.
    reasoning_folded_into_completion: u32,
    /// Attempt-scoped time to the upstream's first streamed frame, whatever
    /// its type (`response.created` included) — see
    /// `UsageEvent::upstream_ttft_ms`. 0 on the non-streaming paths. Before
    /// this existed `/v1/responses` reported no TTFT at all, so codex-class
    /// clients showed blank.
    upstream_ttft_ms: u32,
    /// 与 `upstream_ttft_ms` 同一时刻，保留亚毫秒精度。`Some` 当且仅当流式
    /// 路径真的看到了首帧——1ms 内到达的首帧 `_ms` 为 0，两条 TTFT 指标只认
    /// 这个字段。非流式路径恒为 `None`。
    upstream_ttft: Option<Duration>,
    /// Request-scoped time until the caller got its first response bytes.
    /// 0 until the stream forwards something (or the handler stamps it on
    /// the non-streaming paths).
    downstream_latency_ms: u32,
    /// Responses-API response object `id` (`resp_…`), or the bridged
    /// upstream's own id on the #825 cross-provider path. Empty when the
    /// upstream returned no id (#1289).
    provider_request_id: String,
    /// The tier the upstream reported serving on: the Response object's
    /// `service_tier`, or the bridged upstream's on the #825 path.
    service_tier: Option<String>,
    /// The upstream failure that ended a stream after its `200` went out.
    /// `None` on the non-streaming paths, whose failures never get this far.
    failure: Option<crate::attempt::StreamFailure>,
}

pub async fn responses(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    // Result-wrapped so an extractor-layer 413 maps to the OpenAI
    // envelope — see completions.rs.
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let started = Instant::now();
    let Json(mut body) = match body {
        Ok(json) => json,
        // Answer through `reject` — see completions.rs.
        Err(rej) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                "/v1/responses",
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_json_rejection(
                    rej,
                    state.request_body_limit_for("/v1/responses"),
                ),
            );
        }
    };
    // Before anything reads the body: no gateway context tag may reach a
    // guardrail, a log, or an upstream. In platform mode the tags are sealed
    // to the tenant they were handed to: the key's team, or the key itself.
    let affinity = match state.platform() {
        Some(platform) => {
            let owner = crate::platform::tenant_owner(&auth);
            let tenant = match crate::responses_affinity::TenantSeal::new(platform, &owner) {
                Ok(tenant) => tenant,
                // No random source, so no response id to hand out.
                Err(err) => {
                    return crate::reject::reject_before_dispatch(
                        &state,
                        "POST",
                        "/v1/responses",
                        &client.request_id,
                        Some(&auth.entry.id),
                        started,
                        crate::reject::Envelope::OpenAi,
                        err,
                    );
                }
            };
            let mut affinity = crate::responses_affinity::ContextAffinity::unwrap_sealed_request(
                &mut body, tenant,
            );
            // The item ids a client echoes from an earlier answer leave here
            // sealed; the upstream gets back the ones it issued, for the
            // tenant they were handed to only. An item reference that names
            // anything else is refused below, with the other stored objects.
            affinity.note_unopened_item_reference(crate::opaque_id::unseal_responses_input(
                platform, &owner, &mut body,
            ));
            affinity
        }
        None => crate::responses_affinity::ContextAffinity::unwrap_request(&mut body),
    };
    let request_id = client.request_id.clone();
    crate::span_facts::note_request(
        &client,
        body.as_object()
            .map(aisix_obs::GenAiRequestParams::from_openai_responses),
    );
    crate::attribution::note_requested_service_tier(
        body.get("service_tier").and_then(|v| v.as_str()),
    );
    let api_key_id = auth.entry.id.clone();

    let model_name = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Read once here rather than off `body` at each terminal emit: dispatch
    // never rewrites the field, and the failure paths must label the request
    // with what the caller asked for.
    let stream_requested = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Filled by `dispatch` with per-detector PII mask counts (#932); attached
    // to the terminal usage event on both the success and failure paths.
    let mut redaction_counts = crate::redact::RedactionCounts::new();
    // Filled by `dispatch` with monitor-mode guardrail observations
    // (#562), same lifecycle as `redaction_counts`.
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
    let mut failure_content_safe = true;
    // Filled by `dispatch` at chain resolution with the request's
    // ENFORCE-mode audit handle (#1330 / #1024), same lifecycle
    // again — read back by whichever emit below is the terminal one.
    let mut audit = crate::usage_attr::GuardrailAudit::default();
    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();
    match dispatch(
        &state,
        &snapshot,
        &auth,
        &mut body,
        &affinity,
        &request_id,
        started,
        &client,
        &mut redaction_counts,
        &mut monitor_hits,
        &mut failure_content_safe,
        &mut audit,
    )
    .await
    {
        Ok(success) => {
            // #932: fold the non-streaming response-side mask counts into
            // the per-request total before the terminal emit below.
            crate::redact::merge_counts(&mut redaction_counts, success.output_redactions.clone());
            monitor_hits.extend(success.output_monitor_hits.clone());
            let elapsed = started.elapsed();
            let status = success.response.status().as_u16();
            // A held-back stream the upstream failed goes out as a 200, and
            // its usage event records the failure's status (below). The
            // request metrics and the access log record the same outcome; a
            // guardrail refusal keeps its own status.
            let terminal_status = success
                .usage
                .as_ref()
                .and_then(|u| u.failure.as_ref())
                .filter(|_| !success.guardrail_blocked)
                .map_or(status, |f| f.status);
            let terminal_error_source =
                match success.usage.as_ref().and_then(|u| u.failure.as_ref()) {
                    Some(failure) if !success.guardrail_blocked => {
                        crate::attempt::error_source_of_class(failure.error_class)
                    }
                    _ => crate::attempt::answered_error_source(
                        success.guardrail_blocked,
                        &success.routing,
                    ),
                };
            // ONE ProviderKey lookup for both the metric emit and the
            // winner's usage event below (#941).
            let pk = ResolvedPk::resolve(&snapshot, &success.provider_key_id);
            let request_upstream = crate::request_metrics::Upstream {
                provider: &success.provider,
                model: &model_name,
                upstream_model: &success.upstream_model,
                pk: pk.labels(),
                stream: stream_requested,
                is_fallback: success.routing.fallback_count() > 0,
            };
            // See the note in `messages.rs`: the flag alone is not "the
            // response is a stream".
            if stream_requested && success.usage_handled_by_stream {
                // A streamed response has no outcome yet: the head exists, nothing
                // has been delivered, and whether the caller reads it to the end
                // or walks away is minutes from being known. Park the line and
                // let whichever terminal emitter ends the request write it, with
                // that emitter's status, tokens and message (AISIX-Cloud#1571).
                crate::attribution::defer_access_log(
                    crate::attribution::PendingAccessLog::new(
                        "POST",
                        "/v1/responses",
                        &request_id,
                        &api_key_id,
                        started,
                    )
                    .with_model(&success.provider, &model_name)
                    .with_routing(&success.routing),
                    crate::request_metrics::PendingRequestMetrics::at_stream_end(
                        &state,
                        "/v1/responses",
                        crate::request_metrics::Caller::new(&auth),
                        request_upstream,
                        started,
                    ),
                );
            } else {
                emit_access_log(
                    &model_name,
                    &success.provider,
                    &api_key_id,
                    terminal_status,
                    elapsed,
                    &request_id,
                    success
                        .usage
                        .as_ref()
                        .map(|u| u.provider_request_id.as_str()),
                    &success.routing,
                    None,
                );
                crate::request_metrics::record(
                    &state,
                    "/v1/responses",
                    crate::request_metrics::Caller::new(&auth),
                    request_upstream,
                    terminal_status,
                    terminal_error_source,
                    elapsed,
                );
            }
            // Per #655: one zero-token UsageEvent per failed attempt that
            // preceded the winner (non-streaming failover).
            emit_failed_attempts(
                &state,
                &snapshot,
                &request_id,
                &model_name,
                &api_key_id,
                &client,
                &success.routing,
                // The winner's event carries the terminal spans.
                /* terminal_last */
                false,
                // These are the attempts a WINNER superseded — the request
                // was served, so no guardrail refused it.
                /* guardrail_blocked */
                false,
                &audit,
            );
            // Issue #404: emit UsageEvent so the control plane's budget ledger
            // and customer-facing /logs analytics see /v1/responses
            // spend. Pre-#404 the responses handler dropped the event
            // entirely — every o1/o3/GPT-5 traffic via Responses API
            // was invisible to budget enforcement and billing
            // reconciliation.
            //
            // #808: the verbatim streaming path can't extract usage
            // synchronously here (the SSE bytes are consumed by the client
            // after this handler returns), so its UsageEvent is emitted from
            // the response stream's Drop guard, which parses the terminal
            // `response.completed` event. `usage_handled_by_stream` guards
            // against a double-emit; `usage` is `None` on that path.
            if !success.usage_handled_by_stream {
                if let Some(mut usage) = success.usage {
                    // Non-streaming: the caller waited for the complete
                    // response, which is exactly the request clock. Streamed
                    // responses stamp this from inside the stream and never
                    // reach this branch (`usage` is None there).
                    usage.downstream_latency_ms = crate::attempt::measured_ms(elapsed);
                    // Winning-attempt classification (#655). Direct models
                    // have no recorded attempt → AttemptInfo defaults.
                    let winner = success.routing.winner();
                    let mut attempt = winner.map(AttemptInfo::from_record).unwrap_or_default();
                    // A held-back stream the upstream failed in-band went out
                    // as a 200 carrying the failure; the usage records the
                    // failure, as the live relay does. A guardrail refusal
                    // keeps its own status.
                    let failure = usage
                        .failure
                        .as_ref()
                        .filter(|_| !success.guardrail_blocked);
                    let usage_status = match failure {
                        Some(f) => {
                            f.apply_to(&mut attempt);
                            f.status
                        }
                        None => status,
                    };
                    // An output guardrail's refusal names itself like an
                    // input refusal does.
                    if success.guardrail_blocked {
                        attempt.error_class = crate::attempt::GUARDRAIL_BLOCK_CLASS.to_string();
                        attempt.error_message = success.guardrail_block_message.clone();
                    }
                    // `latency_ms` is scoped to the winning attempt — the
                    // failed ones before it emitted their own events, so
                    // `elapsed` would double-count them. Access log keeps the
                    // request-level total.
                    let winner_latency = winner
                        .map(|w| Duration::from_millis(u64::from(w.latency_ms)))
                        .unwrap_or(elapsed);
                    emit_usage_event(
                        &state,
                        &snapshot,
                        &pk,
                        &request_id,
                        &success.model_id,
                        &model_name,
                        &model_name,
                        crate::request_metrics::Caller::new(&auth),
                        &success.provider,
                        &success.upstream_model,
                        usage_status,
                        winner_latency,
                        &usage,
                        success.cache,
                        &client,
                        attempt,
                        success.guardrail_blocked,
                        redaction_counts.clone(),
                        monitor_hits.clone(),
                        success.captured_content.as_ref(),
                        /* terminal */ true,
                        // A cache hit reached no upstream.
                        /* dispatched */
                        !matches!(success.cache.status, crate::chat::CacheStatus::Hit),
                        &audit,
                    );
                }
            }
            // Same window the OpenAI SDK reads on /v1/chat/completions, so a
            // Codex client driving /v1/responses can schedule back-off from
            // real numbers instead of blind-retrying into a 429.
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
        Err(ResponsesDispatchError { err, routing }) => {
            let status = err.status().as_u16();
            let elapsed = started.elapsed();
            let attributed = crate::attribution::current().unwrap_or_default();
            let last_target = crate::request_metrics::LastTarget::new(&snapshot, &attributed);
            emit_access_log(
                &model_name,
                last_target.provider(),
                &api_key_id,
                status,
                elapsed,
                &request_id,
                None,
                &routing,
                Some(&err),
            );
            let metric_model = crate::usage_attr::metric_model_label(&snapshot, &model_name);
            // The failed request counts on the detailed families too, so a
            // success rate over /v1/responses has the failures in its
            // denominator.
            // AISIX-Cloud#1325: name the target the request died on. This
            // branch used to emit `Upstream::default()`, so a 502 from a
            // real provider landed on `provider="unknown"` while the same
            // key's successes landed on the real one.
            crate::request_metrics::record(
                &state,
                "/v1/responses",
                crate::request_metrics::Caller::new(&auth),
                last_target.upstream(
                    metric_model.as_ref(),
                    stream_requested,
                    routing.fallback_count() > 0,
                ),
                status,
                crate::attempt::error_source(&err),
                elapsed,
            );
            // #1428: a guardrail refusal IS this failure, so the
            // terminal event must say so — it is what the dashboard's
            // "Guardrail blocks" view filters on. Every other 4xx/5xx class
            // leaves the flag alone.
            let guardrail_blocked = err.is_guardrail_block();
            // Every failure row of this request reports the (post-mask)
            // request body and, without an upstream answer, this error body
            // (#1013, #1921) — unless remote segment moderation could not
            // prove a complete rewrite, which keeps the body out of every
            // full-content exporter.
            if failure_content_safe {
                crate::usage_attr::note_failure_request(&snapshot, || {
                    serde_json::to_string(&body).unwrap_or_default()
                });
            }
            crate::usage_attr::note_client_error(&snapshot, &err);
            // Per #655: emit one zero-token UsageEvent per FAILED attempt so
            // the dashboard's Logs tab surfaces each failed upstream try.
            emit_failed_attempts(
                &state,
                &snapshot,
                &request_id,
                &model_name,
                &api_key_id,
                &client,
                &routing,
                // All-failed: the last failed attempt is the terminal
                // emission; the pre-dispatch branch below covers empty.
                /* terminal_last */
                !routing.attempts.is_empty(),
                guardrail_blocked,
                &audit,
            );
            // Pre-dispatch failure (model-not-found, auth, budget) records no
            // attempts — emit a single terminal event carrying the failure
            // class. `model_id` is the addressed model's own id when it is a
            // direct model, empty for a group (no target was selected) or a
            // name that never resolved. When attempts were recorded, each
            // was already emitted.
            if routing.attempts.is_empty() {
                emit_zero_token_event(
                    &state,
                    &snapshot,
                    &request_id,
                    &crate::attribution::unattempted_model_id(&model_name),
                    &model_name,
                    &api_key_id,
                    // Pre-dispatch failure resolved no provider key → wire NULL.
                    "",
                    status,
                    elapsed,
                    &client,
                    AttemptInfo {
                        kind: "initial".to_string(),
                        error_class: crate::attempt::error_class(&err).to_string(),
                        error_message: crate::attempt::gateway_error_message(&err),
                        ..Default::default()
                    },
                    guardrail_blocked,
                    // Input masking may have fired before the failure.
                    redaction_counts.clone(),
                    monitor_hits.clone(),
                    None,
                    /* terminal */ true,
                    // Pre-dispatch failure: no upstream contacted.
                    /* dispatched */
                    false,
                    &audit,
                );
            }
            err.into_response()
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    // `&mut` so mask-action PII guardrails (#932) can rewrite the request
    // text in place before it reaches the upstream.
    body: &mut Value,
    // The context tags `responses` removed from `body`.
    affinity: &crate::responses_affinity::ContextAffinity,
    request_id: &str,
    // Request-scoped clock + downstream client attribution, threaded so the
    // streaming path's Drop guard can stamp latency + client IP/UA on the
    // end-of-stream UsageEvent it emits (#808).
    started: Instant,
    client: &ClientContext,
    // Out-param: per-detector PII mask counts (#932). Input-side counts land
    // here as soon as the request is rewritten; the non-streaming output side
    // arrives via `ResponseDispatchSuccess::output_redactions`; streaming
    // output counts travel via the stream completion instead.
    redactions_out: &mut crate::redact::RedactionCounts,
    // Out-param: monitor-mode guardrail observations (#562),
    // same lifecycle as `redactions_out`.
    monitor_hits_out: &mut Vec<aisix_core::GuardrailMonitorHit>,
    // False when remote segment moderation could not prove a complete rewrite;
    // the handler then omits the request from full-content failure exporters.
    failure_content_safe_out: &mut bool,
    // Out-param: the request's ENFORCE-mode audit handle, cloned off the
    // resolved chain (AISIX-Cloud#1330). Handed back rather than read here
    // because the terminal event is built by the caller — and, on the
    // streaming paths, from a closure that runs after both frames are gone.
    audit_out: &mut crate::usage_attr::GuardrailAudit,
) -> Result<ResponseDispatchSuccess, ResponsesDispatchError> {
    crate::usage_attr::hold_ahead_of_chain(
        state,
        snapshot,
        auth,
        body.get("model").and_then(|v| v.as_str()),
    );
    let model_name = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProxyError::InvalidRequest("`model` field missing".into()))?
        .to_string();

    let model_entry = crate::model_resolve::resolve_model(snapshot, &model_name)
        .ok_or_else(|| ProxyError::ModelNotFound(model_name.clone()))?;

    if !auth.key().can_access(snapshot, &model_name) {
        return Err(ProxyError::ModelForbidden(model_name.clone()).into());
    }
    // A grouped key is answered as if no ensemble existed, ahead of
    // everything that could tell the two apart (the IP check, guardrails).
    crate::model_resolve::refuse_ensemble_to_grouped_key(
        &model_name,
        &model_entry.value,
        &auth.key().allowed_provider_groups,
    )?;

    // Client-IP allowlist gate (#557): reject before guardrails / upstream.
    crate::dispatch::check_ip_access(&model_entry.value, &client.source_ip)?;

    // Platform mode: nothing the request names may be an object stored
    // upstream for another tenant. A container it was handed is its own.
    crate::opaque_id::with_own_containers(state.platform(), auth, body, |body| {
        crate::stored_refs::refuse_in_responses(state.platform(), body)
    })
    .map_err(|err| affinity.at_caller_position(err))?;
    affinity.refuse_unopened_item_reference()?;

    // Platform mode: a continuation names a response this tenant was handed,
    // served by a target the key's provider groups still hold. Anything else
    // is a response that does not exist, and no upstream hears of it.
    affinity.admit_continuation(snapshot, &auth.key().allowed_provider_groups)?;

    // #719: /v1/responses must run input guardrails like /v1/chat/completions
    // and /v1/messages. Before this, user input reached the upstream without
    // any configured content/DLP check, so a content block enforced on the
    // chat surface was bypassable simply by calling /v1/responses (the same
    // violent input that 422s on chat returned 200 with the content echoed
    // here). Translate the Responses-API body into the internal ChatFormat
    // and run the resolved input guardrail chain; a Block short-circuits
    // before dispatch. (Input Bypass is not applied to the outgoing
    // Responses body — only Block is enforced, matching /v1/messages.)
    //
    // #542: run this BEFORE the rate-limit reservation so a content-policy
    // block by local rules doesn't burn an RPM slot (matching
    // /v1/chat/completions).
    let guardrail_ctx = aisix_guardrails::RequestContext {
        passthrough_route_id: "",
        model_id: &model_entry.id,
        mcp_server_id: "",
        api_key_id: &auth.entry.id,
        team_id: auth.key().team_id.as_deref(),
    };
    // Arc so the chain can be cloned into the cross-provider streaming
    // response body (which outlives this handler) for end-of-stream output
    // guardrails (#825), mirroring /v1/messages.
    let resolved_chain = Arc::new(state.guardrail_index.resolve(&guardrail_ctx));
    *audit_out = resolved_chain.audit_log();
    // A check that calls a remote moderation service is admitted by the
    // key's rate limits before it runs, and a refusal keeps the slot it took
    // (see `quota::reserve_before_costly_check`).
    let model_rl =
        crate::quota::ModelRateLimit::from_model(&model_name, &model_entry.id, &model_entry.value);
    let early_reservation = crate::quota::reserve_before_costly_check(
        resolved_chain.as_ref(),
        crate::quota::enforce(state, snapshot, auth, Some(&model_rl)),
    )
    .await?;
    if !resolved_chain.is_empty() {
        let chat = responses_input_to_chat(&model_name, body);
        let (verdict, hits) = aisix_guardrails::Guardrail::check_input_non_segment_observed(
            resolved_chain.as_ref(),
            &chat,
        )
        .await;
        monitor_hits_out.extend(hits);
        // Segment pass: one Bedrock call over the body's text slots; an
        // ANONYMIZE disposition writes the masked text back into the
        // Responses body (#932 bedrock follow-up).
        let mut moderation = crate::redact::moderate_responses_request_structured(
            resolved_chain.as_ref(),
            Some(model_name.as_str()),
            verdict,
            body,
            redactions_out,
            monitor_hits_out,
        )
        .await;
        if !moderation.verdict.is_block() {
            let redaction =
                crate::redact::redact_responses_request_structured(resolved_chain.as_ref(), body);
            crate::redact::merge_counts(redactions_out, redaction.counts);
            if redaction.unrewritable_tool_key {
                moderation.verdict = crate::redact::unrewritable_tool_key_verdict();
                moderation.capture_safe = false;
            }
        }
        // A refused request forwards nothing: its row exports the body as masked
        // in the block arm below, unless a key cannot be rewritten. Any other
        // unsafe verdict lets a body through that nothing proved fully masked.
        if !moderation.verdict.is_block() {
            *failure_content_safe_out &= moderation.capture_safe;
        }
        let verdict = moderation.verdict;
        if let aisix_guardrails::GuardrailVerdict::Block {
            reason,
            guardrail_name,
            unavailable,
        } = verdict
        {
            // Per #153 the matched-pattern detail stays in ops logs only; the
            // wire envelope names only the guardrail that fired (#519 B.4b)
            // so callers can't enumerate the blocklist by probing error
            // responses.
            // #1013: mask before returning so the failure
            // content capture exports post-mask text (see chat.rs).
            let redaction =
                crate::redact::redact_responses_request_structured(resolved_chain.as_ref(), body);
            crate::redact::merge_counts(redactions_out, redaction.counts);
            if redaction.unrewritable_tool_key {
                *failure_content_safe_out = false;
            }
            crate::usage_attr::note_refused_request(
                snapshot,
                resolved_chain.as_ref(),
                *failure_content_safe_out,
                || serde_json::to_string(&*body).unwrap_or_default(),
            );
            tracing::warn!(
                guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
                guardrail_hook = "input",
                model = %model_name,
                reason = %reason,
                "guardrail blocked /v1/responses request",
            );
            return Err(crate::error::guardrail_block_error(
                "request",
                guardrail_name.as_deref(),
                unavailable.as_deref(),
            )
            .into());
        }
        // #932: mask-action PII rules rewrite the Responses body in place
        // AFTER the block check passes — both the verbatim passthrough and
        // the cross-provider bridge forward from this body.
        // The local structured rewrite already ran immediately after remote
        // moderation so tool-argument object keys can fail closed before
        // dispatch.
    }

    if *failure_content_safe_out {
        crate::usage_attr::note_screened_request(snapshot, || {
            serde_json::to_string(&*body).unwrap_or_default()
        });
    }

    // `Option` so the winning streaming attempt can `take()` the reservation
    // and carry it into the end-of-stream guard (#688); non-streaming / failed
    // attempts leave it in place for the post-dispatch commit or a retry.
    let mut reservation = Some(match early_reservation {
        Some(reservation) => reservation,
        None => crate::quota::enforce(state, snapshot, auth, Some(&model_rl)).await?,
    });

    // Cache gate. Non-streaming only, matching the chat and messages gates:
    // a stream has no stored shape yet. Keyed on the post-redaction body —
    // what actually goes upstream — so entries follow the bytes the provider
    // saw.
    let cache_gate = if body.get("stream").and_then(Value::as_bool) == Some(true) {
        None
    } else {
        crate::response_cache::BodyCache::resolve(
            state,
            snapshot,
            auth,
            client,
            "/v1/responses",
            &model_name,
            &model_entry.value,
            || body.to_string(),
        )
    };
    if let Some(gate) = cache_gate.as_ref() {
        crate::routing::check_group_access(
            snapshot,
            &model_name,
            &model_entry.value,
            &auth.key().allowed_provider_groups,
        )?;
        crate::routing::check_member_ip_access(
            snapshot,
            &model_name,
            &model_entry.value,
            &client.routing_tags,
            &client.source_ip,
        )?;
        if let Some(hit) = gate.lookup().await {
            if let Some(r) = reservation.take() {
                r.commit_tokens_no_spend(0).await;
            }
            let cache = crate::response_cache::CacheTelemetry::hit(&hit);
            // The cache answered for the entry the caller addressed: set the
            // target attribution before the output chain below can refuse it,
            // so a blocked hit does not report a target it never dispatched
            // to. This gate is exact-only.
            crate::attribution::note_cache_hit_entry(&model_entry.value, "exact");
            // Counted at the gate's decision, before the output chain runs:
            // a hit the chain BLOCKS reached an enabled policy and must land
            // somewhere on the series, or cache events stop summing to gated
            // requests. `/v1/chat/completions` counts it at the same point.
            gate.record(state, crate::chat::CacheStatus::Hit);
            // #448: a cache hit is client-visible output just like a fresh
            // upstream response, so it runs the output chain before being
            // returned rather than bypassing it. The stored body was already
            // moderated under the policy in force when it was written — this
            // covers the case that matters, a policy TIGHTENED afterwards,
            // which would otherwise keep serving now-forbidden content for
            // the whole TTL.
            //
            // Its counts accumulate into the dispatch's out-params, not
            // locals: a guardrail that BLOCKS the hit still made
            // observations, and the `?` below would drop locals on the
            // floor — the monitor-mode record of why the replay was
            // withheld is exactly the row an operator goes looking for.
            let guarded = guard_cached_response(
                resolved_chain.as_ref(),
                &model_name,
                &model_entry.value,
                hit.body,
                redactions_out,
                monitor_hits_out,
            )
            .await?;
            let served = guarded.body;
            // A hit served content to the caller, so it is logged like a
            // fresh response — otherwise a content-capturing exporter's
            // audit trail simply omits every cached answer. Gated on an
            // exporter actually wanting content, so the common path pays no
            // parse.
            let captured_content = content_capture_cap(
                snapshot
                    .observability_exporters
                    .entries()
                    .iter()
                    .map(|e| &*e.value),
            )
            // Same two-sided gate the fresh response passes through
            // (`input_capture_safe && output_capture_safe`): a guardrail that
            // withholds content from export must withhold the replay too, on
            // either side. The input half is already settled here — the
            // request chain ran before the cache was consulted.
            .filter(|_| *failure_content_safe_out && guarded.capture_safe)
            .map(|cap| {
                let text = serde_json::from_slice::<Value>(&served)
                    .map(|v| responses_output_text(&v))
                    .unwrap_or_default();
                // Serialized HERE, not on the hot path above: with no
                // content-capturing exporter — the common deployment — a hit
                // pays nothing.
                CapturedContent::new(&body.to_string(), &text, cap as usize)
            });
            let mut response = Response::new(axum::body::Body::from(served));
            if let Ok(ct) = axum::http::HeaderValue::from_str(&hit.content_type) {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, ct);
            }
            crate::response_cache::apply_cache_headers(
                state,
                &mut response,
                crate::chat::CacheStatus::Hit,
            );
            return Ok(ResponseDispatchSuccess {
                response,
                // The entry is decoupled from whichever target produced it,
                // so a hit is attributed to the requested row.
                provider: model_entry
                    .value
                    .provider
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                usage: Some(ResponseUsage {
                    prompt_tokens: hit.prompt_tokens,
                    completion_tokens: hit.completion_tokens,
                    ..Default::default()
                }),
                model_id: model_entry.id.to_string(),
                provider_key_id: String::new(),
                upstream_model: model_entry
                    .value
                    .upstream_model()
                    .unwrap_or("unknown")
                    .to_string(),
                routing: RoutingTelemetry::default(),
                guardrail_blocked: false,
                guardrail_block_message: String::new(),
                usage_handled_by_stream: false,
                captured_content,
                // Already folded into the out-params above; returning them
                // here too would double-count every mask on a hit.
                output_redactions: crate::redact::RedactionCounts::new(),
                output_monitor_hits: Vec::new(),
                cache,
            });
        }
    }

    // Resolve the attempt list (routing-aware). A Model Group walks its
    // targets in order; a direct model resolves to itself (#471). Those
    // targets whose Provider Key serves `/v1/responses` natively take the
    // verbatim passthrough; every other target is bridged through ChatFormat
    // (#825), so a group can mix and fail over across both kinds.
    let budget_gate = crate::budget::gate(state, snapshot);
    let attempt_models = crate::routing::resolve_attempt_models(
        &state.routing,
        &state.runtime_status,
        &state.pricing,
        snapshot,
        &model_name,
        &model_entry.id,
        &model_entry.value,
        crate::routing::RoutingRequest {
            tags: &client.routing_tags,
            headers: Some(&client.headers),
            api_key_id: auth.entry.id.as_str(),
            source_ip: &client.source_ip,
            context_origin: affinity.origin(),
            budget: Some(&budget_gate),
            serves: None,
            allowed_groups: &auth.key().allowed_provider_groups,
        },
    )?;
    let retry_on_429 = model_entry
        .value
        .routing
        .as_ref()
        .map(|r| r.retry_on_429_or_default())
        .unwrap_or(false);
    let fallback_statuses: &[u16] = model_entry
        .value
        .routing
        .as_ref()
        .map(|r| r.fallback_on_statuses_or_default())
        .unwrap_or(&[]);
    // NOTE: deliberately narrower than chat's `routing.is_some() ||
    // is_semantic()`. The quota gate defers model-property policies on any
    // routing/ensemble/semantic PARENT (`ModelRateLimit::routing_parent`),
    // expecting the per-target pass to reserve them — which only runs when
    // this flag is true. Safe today because semantic/ensemble parents
    // cannot successfully dispatch on this endpoint (no provider →
    // pre-dispatch 4xx); if this endpoint ever grows semantic support,
    // widen this flag or the deferred policies are silently skipped.
    let is_routing_request = model_entry.value.routing.is_some();
    // Platform mode: the tenant this request's item ids are sealed to.
    let owner = state
        .platform()
        .map(|_| crate::platform::tenant_owner(auth))
        .unwrap_or_default();
    let mut routing = RoutingTelemetry::for_request(&model_entry.value.display_name)
        .with_trace(client.trace.clone());
    // Walk the targets, failing over on a retryable failure. Streaming and
    // non-streaming share this loop: the per-target dispatch branches
    // internally and, for streaming, only returns Ok once the first chunk
    // has arrived under `stream_timeout` (#554) — so the 200 is committed to
    // exactly one target and a slow first chunk fails over.
    let mut last_err: Option<ProxyError> = None;
    'targets: for (target_idx, target) in attempt_models.iter().enumerate() {
        // Resolved ProviderKey UUID for this target — feeds the per-PK
        // telemetry attribution tags on the emitted UsageEvent
        // (#867). Recorded on the AttemptRecord (success + failure)
        // so both the winner and each failed-attempt event can attribute it.
        let pk_id = target.model.provider_key_id.clone().unwrap_or_default();
        // Same-target retries before failing over, honoured exactly like
        // chat.rs / messages.rs (#641) and resolved per target so a direct
        // model gets a budget too.
        let budget = crate::routing::effective_retries(
            &target.model,
            crate::routing::group_retries_of(&model_entry.value),
            state.default_retries,
            target_idx + 1 < attempt_models.len(),
        );
        // Deadlines resolved target → group → deployment default, next to
        // the retry budget so the two knobs stay in lockstep.
        let timeouts = crate::routing::effective_timeouts(
            &target.model,
            Some(&model_entry.value),
            state.default_timeouts,
        );
        // A group target that cannot read some tagged ciphertext gets the
        // request without it, rather than a certain 400. Built once per
        // target: every same-target retry sends the same body.
        let stripped_body = is_routing_request
            .then(|| {
                affinity.body_for_target(
                    snapshot,
                    body,
                    &target.id,
                    target.model.provider_key_id.as_deref(),
                )
            })
            .flatten();
        let target_model = if is_routing_request {
            target.model.display_name.clone()
        } else {
            String::new()
        };
        // Reserve THIS target's own model rate-limit layers before
        // dispatching to it (#1087), once for all its retries: a same-target
        // retry is the same request, and charging it again would let the
        // retries themselves exhaust the limit. Over-limit → record a 429
        // attempt and move on to the remaining targets in strategy order.
        let mut member_reservation = match crate::quota::reserve_routing_target(
            state,
            snapshot,
            auth,
            is_routing_request.then_some(crate::quota::RoutingParent {
                name: &model_entry.value.display_name,
                entry_id: &model_entry.id,
            }),
            &target.model.display_name,
            &target.id,
            &target.model,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let (idx, kind) = routing.begin_attempt(crate::attempt::AttemptTarget {
                    display_name: &target.model.display_name,
                    target_model: &target_model,
                    model_id: &target.id,
                });
                routing.record(
                    state,
                    AttemptRecord {
                        index: idx,
                        kind,
                        target_model,
                        target_model_id: target.id.clone(),
                        provider_key_id: pk_id.clone(),
                        status: e.status().as_u16(),
                        success: false,
                        error_class: crate::attempt::error_class(&e).to_string(),
                        error_message: e.to_string(),
                        latency_ms: 0,
                        dispatched: false,
                    },
                );
                last_err = Some(e);
                continue 'targets;
            }
        };
        for attempt_idx in 0..=budget.attempts {
            // Upstream `Retry-After` when the last failure carried one, else
            // exponential backoff + jitter, before re-hitting the SAME target
            // (#641); cross-target fall-over (the outer loop) stays immediate.
            if attempt_idx > 0 {
                let hint = last_err.as_ref().and_then(|e| match e {
                    ProxyError::Bridge(be) => crate::routing::retry_after_hint(be),
                    _ => None,
                });
                tokio::time::sleep(crate::routing::retry_backoff(attempt_idx as u32, hint)).await;
            }
            let (idx, kind) = routing.begin_attempt(crate::attempt::AttemptTarget {
                display_name: &target.model.display_name,
                target_model: &target_model,
                model_id: &target.id,
            });
            let attempt_started = Instant::now();
            let deferred_attempt = DeferredAttempt::default();
            // Winning-attempt classification (#655) for the streaming path's
            // end-of-stream UsageEvent. The non-streaming / buffered paths emit
            // from the handler and ignore it.
            let attempt = AttemptInfo {
                index: idx,
                kind: kind.to_string(),
                model: target_model.clone(),
                deferred: Some(deferred_attempt.clone()),
                ..Default::default()
            };
            let attempt_body = stripped_body.as_ref().unwrap_or(body);
            let mark = affinity
                .response_mark(&target.id, is_routing_request)
                .sealing_ids(state.platform(), &owner);
            let native = crate::dispatch::serves_natively(
                snapshot,
                &target.model,
                aisix_core::ApiSurface::Responses,
            );
            crate::attribution::note_translated_dispatch(!native);
            let result = if native {
                responses_to_target(
                    state,
                    snapshot,
                    attempt_body,
                    mark,
                    &target.model,
                    &target.id,
                    timeouts,
                    request_id,
                    resolved_chain.clone(),
                    started,
                    attempt_started,
                    &model_name,
                    &auth.entry.id,
                    client,
                    attempt,
                    &mut reservation,
                    &mut member_reservation,
                    redactions_out.clone(),
                    monitor_hits_out.clone(),
                    *failure_content_safe_out,
                )
                .await
            } else {
                responses_cross_provider_to_target(
                    state,
                    snapshot,
                    attempt_body,
                    mark,
                    &target.model,
                    &target.id,
                    timeouts,
                    request_id,
                    resolved_chain.clone(),
                    started,
                    attempt_started,
                    &model_name,
                    &auth.entry.id,
                    client,
                    attempt,
                    &mut reservation,
                    &mut member_reservation,
                    redactions_out.clone(),
                    monitor_hits_out.clone(),
                    *failure_content_safe_out,
                )
                .await
            };
            match result {
                Ok(mut success) => {
                    let latency_ms = ms_since(attempt_started);
                    // Feed the least_latency EWMA for this target.
                    state.runtime_status.record_latency(&target.id, latency_ms);
                    let terminal_status = success
                        .usage
                        .as_ref()
                        .and_then(|u| u.failure.as_ref())
                        .map(|failure| failure.status);
                    let record = AttemptRecord {
                        index: idx,
                        kind,
                        target_model: target_model.clone(),
                        target_model_id: target.id.clone(),
                        provider_key_id: pk_id.clone(),
                        status: success.response.status().as_u16(),
                        success: true,
                        error_class: String::new(),
                        error_message: String::new(),
                        latency_ms,
                        dispatched: true,
                    };
                    if success.usage_handled_by_stream {
                        routing.stage(record);
                        deferred_attempt.install(&routing, idx);
                    } else if let Some(status) = terminal_status {
                        // The handler emits the buffered winner's billed
                        // UsageEvent below. Keep that winner as the single
                        // attempt event, but classify the deployment metric
                        // with the terminal stream failure discovered while
                        // holding the body.
                        routing.stage(record);
                        routing.finish_staged(state, idx, status, false);
                    } else {
                        routing.record(state, record);
                    }
                    success.routing = routing;
                    // #911 [21]: commit the reserved layers with the actual
                    // token cost so TPM/TPD is enforced for /v1/responses like
                    // chat + embeddings. The buffered / non-streaming paths
                    // carry `usage` here and commit now; the streaming path
                    // already `take()`-d the reservation into its end-of-stream
                    // guard (#688), so `reservation` is `None` and this is
                    // skipped.
                    if !success.usage_handled_by_stream {
                        if let Some(mut r) = reservation.take() {
                            // Fold this target's model-layer reservation in
                            // (#1087) so one commit bills the
                            // member's TPM/TPD too. Already `None` when the
                            // streaming path folded it into the guard.
                            if let Some(member) = member_reservation.take() {
                                r.merge(member);
                            }
                            let total = success
                                .usage
                                .as_ref()
                                .map(|u| {
                                    total_tokens_with_cache(
                                        u.prompt_tokens,
                                        u.completion_tokens,
                                        u.cache_creation_tokens,
                                        u.cache_read_tokens,
                                    )
                                })
                                .unwrap_or(0);
                            // 花费按调度到的目标行定价——和这次请求的用量
                            // 事件用的是同一个 model_id。
                            let spend = success
                                .usage
                                .as_ref()
                                .map(|u| {
                                    crate::usage_attr::request_spend_micro_usd(
                                        snapshot,
                                        &target.id,
                                        crate::usage_attr::input_tokens_for_pricing(
                                            u64::from(u.prompt_tokens),
                                            u64::from(u.cached_prompt_tokens),
                                            u64::from(u.cache_write_tokens.unwrap_or(0)),
                                            u64::from(u.cache_read_tokens),
                                            u64::from(u.cache_creation_tokens),
                                        ),
                                        u64::from(u.completion_tokens),
                                    )
                                })
                                .unwrap_or(0);
                            r.commit(total, spend).await;
                        }
                    }
                    if let Some(gate) = cache_gate.as_ref() {
                        success = store_in_cache(state, gate, success).await;
                    }
                    return Ok(success);
                }
                Err(e) => {
                    let retryable = matches!(
                        &e,
                        ProxyError::Bridge(be) if crate::routing::is_retryable(be, retry_on_429, fallback_statuses)
                    );
                    crate::routing::log_attempt_failure(
                        request_id,
                        &target.model.display_name,
                        attempt_idx + 1,
                        &e,
                        retryable,
                        fallback_statuses,
                    );
                    if retryable {
                        state.runtime_status.penalize_latency(&target.id);
                    }
                    let (error_class, error_message) = attempt_error_from_proxy(&e);
                    routing.record(
                        state,
                        AttemptRecord {
                            index: idx,
                            kind,
                            target_model: target_model.clone(),
                            target_model_id: target.id.clone(),
                            provider_key_id: pk_id.clone(),
                            status: e.status().as_u16(),
                            success: false,
                            error_class,
                            error_message,
                            latency_ms: ms_since(attempt_started),
                            dispatched: attempt_reached_upstream(&e),
                        },
                    );
                    // See `RetryBudget::covers`: a default budget skips
                    // same-target retries for timeouts; fail-over is unaffected.
                    let budget_covers = match &e {
                        ProxyError::Bridge(be) => budget.covers(be),
                        _ => true,
                    };
                    let unusable = crate::routing::target_unusable(&e);
                    last_err = Some(e);
                    // Non-retryable → stop entirely, unless the failure is
                    // this target's own configuration. Retryable → re-hit the
                    // same target until `retries` is exhausted, then fall over
                    // to the next target (the outer loop advances).
                    if !retryable {
                        if unusable {
                            break;
                        }
                        break 'targets;
                    }
                    if attempt_idx == budget.attempts || !budget_covers {
                        break;
                    }
                }
            }
        }
    }

    let err = last_err.unwrap_or(ProxyError::ProviderUnavailable);
    Err(ResponsesDispatchError {
        err: if is_routing_request {
            crate::routing::group_refusal(err, &model_name)
        } else {
            err
        },
        routing,
    })
}

/// Translate a `/v1/responses` request body into the internal
/// [`ChatFormat`] so the input guardrail chain can scan the
/// user-supplied content (#719). Only scannable text matters here — this
/// is **not** a faithful Responses→Chat transform and is never sent
/// upstream; the original `body` is forwarded verbatim.
///
/// The Responses-API `input` field is either a bare string or an array of
/// input items; a message item is `{role, content}` whose `content` is a
/// string or an array of typed parts (`input_text` / `output_text` /
/// `text`). The optional top-level `instructions` maps to a system
/// message. Roles are preserved so the guardrail's user-vs-all message
/// selection behaves the same as on /v1/chat/completions.
/// <https://platform.openai.com/docs/api-reference/responses/create>
fn responses_input_to_chat(model: &str, body: &Value) -> ChatFormat {
    let mut messages = Vec::new();

    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.is_empty() {
            messages.push(ChatMessage::system(instructions.to_string()));
        }
    }

    match body.get("input") {
        Some(Value::String(text)) => {
            if !text.is_empty() {
                messages.push(ChatMessage::user(text.clone()));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                // A bare-string array element is treated as user text; an
                // object element is a message whose role we preserve.
                //
                // EVERY item becomes a message, including one that carries
                // no readable text. That is what keeps this view aligned
                // one-to-one with `input[]`, and the alignment is what
                // makes the latest-turn window agree with the mask
                // walkers in `crate::redact`, which compute the same
                // boundary off the raw wire and cannot tell that an item
                // held no text. Drop one here and the two halves disagree
                // on where the current turn starts — a replayed
                // `{"type": "reasoning", "encrypted_content": "…"}` (what
                // an agent client sends on every turn with reasoning
                // summaries off) or a `refusal`-only assistant message
                // would make the check pass see no model turn at all and
                // widen back to the whole conversation. An empty message
                // contributes nothing to any kind's scan, so keeping it
                // costs nothing.
                if let Some(text) = item.as_str() {
                    messages.push(ChatMessage::user(text.to_string()));
                    continue;
                }
                let text = responses_item_text(item);
                messages.push(match responses_item_role(item) {
                    Role::Assistant => ChatMessage::assistant(text),
                    Role::System | Role::Developer => ChatMessage::system(text),
                    Role::Tool | Role::Function => ChatMessage::tool(text),
                    Role::User => ChatMessage::user(text),
                });
            }
        }
        _ => {}
    }

    let mut preamble = Vec::new();
    for field in [
        "tools",
        "tool_choice",
        "text",
        "user",
        "safety_identifier",
        "prompt_cache_key",
        "metadata",
    ] {
        if let Some(value) = body.get(field) {
            let text = serde_json::to_string(value).expect("serde_json::Value always serializes");
            if text != "null" && text != "[]" {
                preamble.push(ChatMessage::user(text));
            }
        }
    }
    if let Some(prompt) = body.get("prompt") {
        let text = crate::redact::responses_prompt_inspection_text_capped(prompt, usize::MAX).0;
        if !text.is_empty() {
            preamble.push(ChatMessage::user(text));
        }
    }
    // Ahead of the conversation, so its last message stays last: a
    // guardrail that has to bound how much it sends checks the newest turn
    // first and in full.
    messages.splice(0..0, preamble);

    ChatFormat::new(model, messages)
}

/// The role a Responses-API `input[]` item replays.
///
/// This API does not spell every turn as a `role`-bearing message: a model
/// turn that called a tool is a bare `function_call` item, and the caller's
/// answer is a bare `function_call_output`. Reading only `role` therefore
/// used to report an agent's whole tool loop as user text, which both hid
/// the assistant turns from anything that reasons about conversation
/// structure and mislabelled tool results. The mapping below is the
/// Responses spelling of what `/v1/chat/completions` sends as an assistant
/// message with `tool_calls` followed by `role: "tool"` messages.
/// <https://platform.openai.com/docs/api-reference/responses/create>
pub(crate) fn responses_item_role(item: &Value) -> Role {
    match item.get("role").and_then(|v| v.as_str()) {
        Some("assistant") => return Role::Assistant,
        Some("system") | Some("developer") => return Role::System,
        Some(_) => return Role::User,
        None => {}
    }
    match item.get("type").and_then(|v| v.as_str()) {
        // The model asking for a tool, and its own reasoning.
        Some("function_call" | "custom_tool_call" | "reasoning") => Role::Assistant,
        // The caller answering that request.
        Some("function_call_output" | "custom_tool_call_output") => Role::Tool,
        _ => Role::User,
    }
}

/// Collect the caller-supplied text and structural identifiers of one
/// Responses-API input/output item across every forwarded key that a
/// guardrail must inspect:
/// - `content` — message items;
/// - `output` — tool-result items (`function_call_output`,
///   `custom_tool_call_output`, `*_call_output`) the caller feeds back;
/// - `reason` — an `mcp_approval_response` justification;
/// - `summary` — a `reasoning` item's summary parts;
/// - `name` plus `arguments` / `input` — a replayed tool call. These carry
///   caller-controlled text straight to the model and a `function_call`
///   item has none of the other four keys, so without them the whole item
///   collapsed to empty text and was dropped from the scan: a block rule
///   that fires on `/v1/chat/completions` (where the same replayed call
///   rides `extra["tool_calls"]` and IS scanned) was bypassable by moving
///   the payload into a tool call on this surface. The same pair the
///   output scanner reads for a generated call; and
/// - the stable protocol identifiers beside them (`server_label`,
///   `connector_id`, and so on), through the tree walk the mask pass uses.
///
/// A call's `call_id`, an item's `id` and an approval's
/// `approval_request_id` are not scanned: they are opaque handles, left out
/// of scan text on every surface
/// (`aisix_gateway::tool_calls::tool_calls_scan_text`).
///
/// `arguments` / `input` are rewritten by the mask walker
/// (`redact::redact_responses_item`); `name` is NOT, exactly as a
/// `tool_calls` function name is scanned but never rewritten on the chat
/// wire — a tool name is structural, and masking it would break the call
/// it identifies. So a Mask rule refuses on a tool NAME rather than rewrite
/// it, and a Block rule refuses as it does anywhere.
///
/// A `reasoning` item's `content[]` parts are covered by the `content`
/// key above. Reasoning replayed on the REQUEST is caller-supplied text
/// entering the model like any other, so it is scanned — and masked, at
/// the same two slots (`redact::redact_responses_item`). Reasoning the
/// model GENERATES is a separate question and stays out of the
/// output-guardrail scope.
///
/// All are user-controlled content entering the model — the
/// `/v1/chat/completions` equivalent (a `role:"tool"` message) is
/// scanned, so leaving any of these unscanned would let the #719
/// surface-switch bypass survive on that channel. Each slot is a string
/// or an array of typed parts; we gather the `text` of each part and
/// ignore non-text parts (images, files). These items carry no `role`,
/// so the caller maps them to a user message (scanned by every guardrail
/// kind). Reading a key absent on other item types is a harmless no-op.
/// <https://platform.openai.com/docs/api-reference/responses/create>
pub(crate) fn responses_item_text(item: &Value) -> String {
    responses_item_scan_text_capped(item, usize::MAX).0
}

/// The text a whole-body guardrail reads for one item: its values, without
/// the opaque handles or the protocol's own field names, plus the object keys
/// of a JSON-encoded tool payload. Those keys are caller and model text that
/// the walk decodes past, and a kind that only reads this text sees them
/// nowhere else.
fn responses_item_scan_text_capped(item: &Value, cap: usize) -> (String, bool) {
    let view = responses_item_without_handles(item);
    let (mut text, mut truncated) = crate::redact::responses_item_content_text_capped(&view, cap);
    for field in ["arguments", "result", "output"] {
        if let Some(Ok(payload)) = view
            .get(field)
            .and_then(Value::as_str)
            .map(serde_json::from_str::<Value>)
        {
            truncated |= push_json_object_keys(&payload, &mut text, cap);
        }
    }
    (text, truncated)
}

/// Append every object key under `value`, one per line. Reports truncation
/// the way `token_estimate::push_capped_to` does.
fn push_json_object_keys(value: &Value, text: &mut String, cap: usize) -> bool {
    let mut truncated = false;
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                if !text.is_empty() {
                    truncated |= crate::token_estimate::push_capped_to(text, "\n", cap);
                }
                truncated |= crate::token_estimate::push_capped_to(text, key, cap);
                truncated |= push_json_object_keys(child, text, cap);
            }
        }
        Value::Array(items) => {
            for item in items {
                truncated |= push_json_object_keys(item, text, cap);
            }
        }
        _ => {}
    }
    truncated
}

/// `item` without the opaque handles no guardrail reads: an id links one
/// item to another and carries no content, and scanning it only produces
/// false positives, since Gemini ids carry hundreds of bytes of thought
/// signature and upstream-generated ids are random strings a keyword rule
/// can match by chance.
fn responses_item_without_handles(item: &Value) -> Value {
    let mut view = item.clone();
    if let Some(fields) = view.as_object_mut() {
        for key in aisix_gateway::tool_calls::TOOL_CALL_ID_KEYS {
            fields.remove(key);
        }
        fields.remove("approval_request_id");
    }
    view
}

/// Dispatch one concrete target's Responses-API passthrough. The caller
/// has already confirmed the target serves `/v1/responses` natively
/// ([`crate::dispatch::serves_natively`]) — which its vendor id implies
/// only for OpenAI itself, and which any vendor can declare — so the
/// URL is resolved per surface rather than from `api_base` alone.
#[allow(clippy::too_many_arguments)]
async fn responses_to_target(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    body: &Value,
    // The context tags this attempt's response carries back.
    mark: crate::responses_affinity::ResponseMark,
    model: &Arc<aisix_core::Model>,
    model_id: &str,
    // Deadlines resolved by the caller across target → group → deployment
    // default (`routing::effective_timeouts`); this fn only applies them.
    timeouts: crate::routing::TimeoutBudget,
    request_id: &str,
    // Arc so the live-forward streaming path can carry the chain into its
    // end-of-stream observation (#1010).
    chain: Arc<aisix_guardrails::GuardrailChain>,
    // #808: end-of-stream UsageEvent context for the verbatim streaming
    // path's Drop guard. Unused by the non-streaming / buffered paths,
    // which emit from the handler.
    started: Instant,
    // When THIS attempt began. The end-of-stream UsageEvent reports
    // `attempt_started.elapsed()` so `latency_ms` stays scoped to the
    // attempt (`started` spans the whole request — see `usage.rs`).
    attempt_started: Instant,
    requested_model: &str,
    api_key_id: &str,
    client_ctx: &ClientContext,
    attempt: AttemptInfo,
    reservation: &mut Option<aisix_ratelimit::MultiReservation>,
    // This target's own model-layer reservation (routing dispatch only,
    // #1087). The streaming path folds it into `reservation`
    // before the take below; the non-streaming path leaves it for the
    // handler to commit alongside `reservation`.
    member_reservation: &mut Option<aisix_ratelimit::MultiReservation>,
    // Input-side PII mask counts (#932) for the verbatim streaming path's
    // end-of-stream emit; the non-streaming/buffered emits happen in the
    // handler, which already holds them.
    input_redactions: crate::redact::RedactionCounts,
    // Input-side monitor hits (#562), same lifecycle as
    // `input_redactions`.
    input_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    input_capture_safe: bool,
) -> Result<ResponseDispatchSuccess, ProxyError> {
    let chain_arc = chain;
    let chain = chain_arc.as_ref();
    // Largest content cap any enabled content-capturing exporter wants, or
    // `None` when none do (#947). The captured prompt is the
    // client-facing request body (post-#932-redaction), not the rewritten
    // outbound one, so the log shows what the caller sent.
    let content_cap = content_capture_cap(
        snapshot
            .observability_exporters
            .entries()
            .iter()
            .map(|e| &*e.value),
    );
    let pk_entry = crate::dispatch::resolve_provider_key(snapshot, model)?;
    // Resolved PK id for per-PK telemetry attribution on the emitted
    // UsageEvent (#867).
    let provider_key_id = pk_entry.id.clone();
    let api_key = crate::dispatch::require_api_key(&pk_entry.value, model)?.to_string();
    let upstream_model = crate::dispatch::require_upstream_model(model)?.to_string();

    // Only the outbound wire bytes outlive this block: the caller keeps its
    // body for fail-over, and reqwest holds these until the response head,
    // so a `Value` copy alive across the await would be a third copy of the
    // request (see `crate::util::outbound_json`).
    let (body_bytes, is_stream) = {
        let mapped = crate::effort_mapping::responses_request(body, model);
        // The common case only renames `model`: serialize the caller's body
        // with that one field swapped instead of deep-cloning it first, which
        // would be a second whole parsed request alive at the peak.
        let only_model_renamed = match &mapped {
            Cow::Borrowed(body) => {
                crate::util::model_only_fields(body, &pk_entry.value, Some("input"))
            }
            _ => None,
        };
        if let Some(fields) = only_model_renamed {
            let wire = crate::util::ModelRenamed {
                fields,
                model: &upstream_model,
            };
            let is_stream = fields
                .get("stream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            (crate::util::outbound_json(&wire)?, is_stream)
        } else {
            let mut body = mapped.into_owned();

            // Rewrite model field to upstream name.
            if let Some(m) = body.get_mut("model") {
                *m = Value::String(upstream_model.clone());
            }
            // A Gemini thought signature riding in a call id is for Gemini only.
            if let Some(input) = body.get_mut("input") {
                aisix_gateway::tool_calls::strip_thought_signatures_in_body(input);
            }

            // Apply the PK's `request.*` overrides to the outbound body, matching the
            // OpenAI bridge's chat() path and the /v1/messages passthrough. The
            // verbatim /v1/responses path builds the request directly (bypassing the
            // Hub), so without this the override pipeline silently no-ops for Codex
            // traffic (#867 follow-up). Apply order: renames → constraints
            // → defaults; each is a no-op when its configured map is empty.
            if let Some(r) = pk_entry.value.request.as_ref() {
                aisix_provider_openai::overrides::validate_content_safe_request_overrides(r)
                    .map_err(|message| {
                        ProxyError::Bridge(aisix_gateway::BridgeError::InvalidUpstreamConfig(
                            message,
                        ))
                    })?;
                aisix_provider_openai::overrides::apply_param_renames(&mut body, &r.param_renames);
                if let Some(constraints) = &r.param_constraints {
                    aisix_provider_openai::overrides::apply_param_constraints(
                        &mut body,
                        constraints,
                    );
                }
                aisix_provider_openai::overrides::apply_default_body_fields(
                    &mut body,
                    &r.default_body_fields,
                );
            }
            let is_stream = body
                .get("stream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            (crate::util::outbound_json(&body)?, is_stream)
        }
    };

    let url = aisix_gateway::url_cache::cached_endpoint_url(
        &pk_entry.id,
        "proxy/responses",
        // Every resolve_base_url_for input (#1017) via the shared constructor.
        &crate::dispatch::pk_surface_url_fingerprint(
            &pk_entry.value,
            aisix_core::ApiSurface::Responses,
        ),
        || {
            let base = crate::dispatch::resolve_base_url_for(
                &pk_entry.value,
                aisix_core::ApiSurface::Responses,
            )?;
            Ok::<_, crate::error::ProxyError>(crate::dispatch::build_openai_url(
                &base,
                "/responses",
            ))
        },
    )?;

    // Build headers explicitly so the PK's `request.default_headers` and
    // `request.forward_client_headers` can inject operator/client headers.
    // Bridge-owned headers go in FIRST; `apply_request_headers` skips
    // already-present keys + the reserved auth blacklist, so neither can
    // clobber auth.
    let mut headers = axum::http::HeaderMap::new();
    let auth_hv = HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|e| {
        ProxyError::Bridge(aisix_gateway::BridgeError::Config(format!(
            "api key contains invalid header chars: {e}"
        )))
    })?;
    headers.insert(axum::http::header::AUTHORIZATION, auth_hv);
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let rid_hv = HeaderValue::from_str(request_id).map_err(|e| {
        ProxyError::Bridge(aisix_gateway::BridgeError::Config(format!(
            "request_id contains invalid header chars: {e}"
        )))
    })?;
    headers.insert(HeaderName::from_static("x-aisix-request-id"), rid_hv);
    aisix_gateway::apply_request_headers(
        &mut headers,
        &crate::dispatch::upstream_header_ctx(
            &pk_entry.value,
            &pk_entry.id,
            model,
            model_id,
            client_ctx,
        ),
    );

    let client = crate::http_client::client_for(pk_entry.value.upstream_connection().as_ref());
    let mut req = url
        .post_on(&client)
        .headers(headers)
        .body(body_bytes.clone());
    // #554: non-streaming gets the E2E request timeout via reqwest's
    // request-level timeout. Streaming must NOT use it (it would cap the
    // whole stream); the streaming branch below enforces the per-chunk
    // read timeout instead.
    if !is_stream {
        if let Some(d) = timeouts.request {
            req = req.timeout(d);
        }
    }
    let send_started = Instant::now();
    // least_busy: count this target as in-flight for the upstream call
    // (mirrors chat.rs). Non-streaming / error paths drop the guard at
    // function return; the streaming branch moves it into the
    // end-of-stream closure next to `stream_hold`, so the count stays
    // raised for the stream's full lifetime.
    let in_flight = state.runtime_status.begin_in_flight(model_id);
    // Streaming bounds the connect by the stream deadline (reqwest's
    // request-level timeout can't be used — it would cap the whole stream);
    // non-streaming relies on the request-level timeout set above.
    let connect_deadline = if is_stream { timeouts.stream } else { None };
    let upstream_resp =
        crate::stream_timeout::send_with_deadline(req, connect_deadline, send_started)
            .await
            .map_err(|be| {
                if be.http_status() >= 500 {
                    state.health.record_failure(&model.display_name);
                }
                crate::cooldown::note_failure(
                    &state.runtime_status,
                    model_id,
                    model.cooldown.as_ref(),
                    be,
                )
            })
            .map_err(ProxyError::Bridge)?;
    // Serialized once the upstream has answered, not before: a copy made
    // up front would be alive for the whole upstream wait.
    let captured_prompt = content_cap.map(|_| serde_json::to_string(body).unwrap_or_default());

    let status = upstream_resp.status();

    if !status.is_success() {
        let err = aisix_gateway::upstream_error_within(
            connect_deadline,
            upstream_resp,
            aisix_provider_openai::held_route_http_error,
        )
        .await;
        if err.http_status() >= 500 {
            state.health.record_failure(&model.display_name);
        }
        if let Some((ttl, reason)) = crate::cooldown::decide_cooldown(&err, model.cooldown.as_ref())
        {
            state.runtime_status.mark_cooldown(model_id, ttl, reason);
        }
        return Err(ProxyError::Bridge(err));
    }

    // The target model's own vendor id, exactly as the bridged path and
    // every other endpoint label it. A literal was correct while this
    // path was reachable only from `provider == "openai"`; now that a key
    // declares which surfaces it serves, a `deepseek` (or `byo`) model
    // can take the verbatim passthrough too, and hard-coding "openai"
    // would split its metric series across two provider labels. The
    // billed amount is unaffected — that comes from the cost inlined on
    // the model document, not from this label.
    let provider_label = model
        .provider
        .as_deref()
        .unwrap_or("unknown")
        .to_ascii_lowercase();

    // The relay branch follows what the upstream ACTUALLY sent, not the
    // request's `stream` flag — see `dispatch::upstream_body_is_sse`. A
    // JSON document answering `stream: true` takes the non-streaming
    // buffered scan+mask path below.
    //
    // That body needs its own deadline: the request-level timeout above was
    // deliberately NOT attached for a streaming request, and the per-chunk
    // read timeout lives on the SSE branch this response no longer takes,
    // so without one a stalled JSON body would be held with no bound at all.
    let buffered_body_deadline = if is_stream { timeouts.stream } else { None };
    let is_stream = is_stream && crate::dispatch::upstream_body_is_sse(upstream_resp.headers());

    if is_stream {
        let headers = upstream_resp.headers().clone();

        // #719: when an output-hook guardrail with a hold-back streaming
        // policy (Window/BufferFull — any block-capable output chain) is
        // attached, the streaming response can't be forwarded token-by-token
        // — a blocked phrase would already be on the wire before it scans
        // clean, the same surface-switch bypass via `stream:true`. Mirror the
        // chat surface's secure default (BufferFull): hold the whole SSE
        // response, scan the assistant output text, then release the bytes
        // verbatim or end on failure frames ([`build_held_responses_stream`]).
        // A monitor-only chain resolves to EndOfStreamCheck — it can never
        // block, so it must never hold the stream back nor fail closed on the
        // buffer cap (#1010); it takes the live-forward path below, which scans
        // at end-of-stream for observation. Requests with no output-hook
        // guardrail keep the zero-copy verbatim passthrough.
        let output_policy = aisix_guardrails::Guardrail::stream_output_policy(chain);
        let hold_back =
            aisix_guardrails::Guardrail::runs_on_output(chain) && output_policy.holds_back();
        // #554: enforce the per-chunk read timeout on the forwarded bytes.
        // When a `stream_timeout` is configured, peek the first byte so a
        // slow/erroring first token fails over before the 200 is committed;
        // without one, forward directly (pre-#554 behavior). A mid-stream
        // stall truncates the forwarded stream (no in-band error frame for
        // an opaque byte passthrough).
        let stream_budget = timeouts.stream;
        let wrapped: RawResponsesByteStream =
            Box::pin(crate::stream_timeout::with_read_timeout_bytes(
                upstream_resp.bytes_stream(),
                stream_budget,
            ));
        let body_stream: RawResponsesByteStream = if timeouts.stream_configured {
            let mut wrapped = wrapped;
            let first_bytes = match wrapped.next().await {
                Some(Ok(b)) => b,
                Some(Err(e)) => {
                    let err = e.into_bridge(send_started);
                    state.health.record_failure(&model.display_name);
                    if let Some((ttl, reason)) =
                        crate::cooldown::decide_cooldown(&err, model.cooldown.as_ref())
                    {
                        state.runtime_status.mark_cooldown(model_id, ttl, reason);
                    }
                    return Err(ProxyError::Bridge(err));
                }
                None => {
                    let err = aisix_gateway::BridgeError::StreamAborted;
                    state.health.record_failure(&model.display_name);
                    if let Some((ttl, reason)) =
                        crate::cooldown::decide_cooldown(&err, model.cooldown.as_ref())
                    {
                        state.runtime_status.mark_cooldown(model_id, ttl, reason);
                    }
                    return Err(ProxyError::Bridge(err));
                }
            };
            // Nothing has reached the caller yet, so a failure reported as
            // the first frame fails over like a failed status would, as it
            // does on the bridged paths.
            if let Some(err) = crate::dispatch::first_sse_frame_json(&first_bytes)
                .and_then(|frame| responses_in_band_error(&frame))
            {
                if err.http_status() >= 500 {
                    state.health.record_failure(&model.display_name);
                }
                return Err(ProxyError::Bridge(crate::cooldown::note_failure(
                    &state.runtime_status,
                    model_id,
                    model.cooldown.as_ref(),
                    err,
                )));
            }
            Box::pin(
                futures::stream::once(std::future::ready(Ok::<
                    bytes::Bytes,
                    crate::stream_timeout::RawStreamError,
                >(first_bytes)))
                .chain(wrapped),
            )
        } else {
            wrapped
        };
        // #808: wrap the verbatim byte stream so the terminal
        // `response.completed` SSE event's `usage` block is parsed in-flight
        // and a UsageEvent is emitted from the stream's Drop guard at
        // end-of-stream (or client-disconnect). Bytes forward unchanged apart
        // from the caller-facing `model` on the snapshot frames — see
        // [`crate::model_echo`]. Pre-#808 this
        // path dropped the event entirely, so every streaming /v1/responses
        // call (e.g. all Codex traffic, which always streams) was invisible
        // to the dashboard Logs and the budget ledger.
        let state_c = state.clone();
        // The chain does not survive into the end-of-stream closure, so
        // clone the audit handle here and read it there — the streaming
        // paths are where an output-hook mask actually lands
        // (AISIX-Cloud#1330 / #1024).
        let audit_c = chain.audit_log();
        let request_id_c = request_id.to_string();
        let model_id_c = model_id.to_string();
        let requested_model_c = requested_model.to_string();
        let provider_key_id_c = provider_key_id.clone();
        let provider_c = provider_label.clone();
        let upstream_model_c = upstream_model.clone();
        let model_display_name_c = model.display_name.clone();
        let cooldown_c = model.cooldown.clone();
        let (metric_model, metric_upstream_model) = crate::usage_attr::metric_model_label_pair(
            snapshot,
            requested_model,
            &upstream_model_c,
        );
        let metric_caller = crate::request_metrics::Caller::from_api_key_id(snapshot, api_key_id);
        let metric_model = metric_model.into_owned();
        let metric_upstream_model = metric_upstream_model.into_owned();
        let client_c = client_ctx.clone();
        let attempt_c = attempt.clone();
        // #688: carry the reservation into the end-of-stream guard — keys drive
        // post-stream TPM/TPD accounting, the hold keeps the concurrency slot(s)
        // until the stream ends. `take()` leaves the handler's `reservation` as
        // `None` so it won't also `commit_tokens_no_spend`.
        // Fold this target's model-layer reservation in first (#1087)
        // so the guard covers the member's limits too; `take()` leaves it `None`
        // so the handler won't also commit it.
        if let Some(member) = member_reservation.take() {
            match reservation.as_mut() {
                Some(main) => main.merge(member),
                None => *reservation = Some(member),
            }
        }
        // 花费层的桶记 micro-USD，和 token 桶不能共用一张键表——同一批键
        // 加同一个数字会把 token 数当成钱记进去。
        let post_stream_keys = reservation
            .as_ref()
            .map(|r| r.token_keys())
            .unwrap_or_default();
        let spend_post_stream_keys = reservation
            .as_ref()
            .map(|r| r.spend_keys())
            .unwrap_or_default();
        let stream_hold = reservation.take().map(|r| r.into_stream_hold());
        let limiter_c = std::sync::Arc::clone(&state.limiter);
        let captured_prompt_c = captured_prompt.clone();
        // #1010: a monitor-only output chain (EndOfStreamCheck —
        // the only way an output-hook chain reaches this live-forward branch)
        // still gets its end-of-stream scan, so would-block / would-mask
        // observations reach telemetry. `None` without an output hook.
        let eos_scan = (aisix_guardrails::Guardrail::runs_on_output(chain) && !hold_back)
            .then(|| EosOutputScan::new(Arc::clone(&chain_arc), upstream_model.clone()));
        // A live output-guarded response is uninspected until its
        // end-of-stream scan completes.
        let initial_capture_safe = eos_scan.is_none();
        // Token-estimation fallback context (#1074): the closure
        // runs at end-of-stream Drop, so it keeps the wire bytes (a shared
        // `Bytes`, no copy) and parses them only if the upstream never
        // reports usage.
        let estimator_c = crate::token_estimate::Estimator::new(
            &upstream_model,
            crate::token_estimate::PromptInput::ResponsesWire(body_bytes.clone()),
        );
        let held_verdict = Arc::new(std::sync::OnceLock::new());
        let held_verdict_c = Arc::clone(&held_verdict);
        let on_complete =
            move |mut usage: ResponseUsage,
                  out_text: String,
                  output_hits: Vec<aisix_core::GuardrailMonitorHit>,
                  output_capture_safe: bool,
                  stream_failure: Option<crate::stream_timeout::RawStreamFailure>| {
                let held: Option<&HeldResponseVerdict> = held_verdict_c.get();
                let refusal = held.and_then(|held| held.refusal.as_ref());
                let refused = refusal.is_some();
                // Streams that reach here are committed 200s — the
                // `!status.is_success()` guard above returned early on errors.
                //
                // Token-estimation fallback (#1074): a stream that
                // ends without a terminal usage event (client abort, relay
                // that omits usage) fills the missing counters from the
                // request + the assembled output text, BEFORE the TPM
                // accounting and the emit below.
                if usage.prompt_tokens == 0 || usage.completion_tokens == 0 {
                    let filled = crate::token_estimate::fill_missing(
                        &estimator_c,
                        usage.prompt_tokens,
                        usage
                            .cache_read_tokens
                            .saturating_add(usage.cache_creation_tokens),
                        usage.completion_tokens,
                        Some(&out_text),
                    );
                    if filled.estimated {
                        usage.prompt_tokens = filled.prompt_tokens;
                        usage.completion_tokens = filled.completion_tokens;
                        usage.usage_estimated = true;
                    }
                }
                // #688: apply the terminal token cost to TPM/TPD and release the
                // concurrency hold now the stream has ended (sync analog of the
                // reservation's async `commit_tokens_no_spend`, which this closure can't await).
                let streamed_tokens = total_tokens_with_cache(
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.cache_creation_tokens,
                    usage.cache_read_tokens,
                );
                limiter_c.add_tokens_post_stream_all(&post_stream_keys, streamed_tokens);
                drop(stream_hold);
                // least_busy: stream over — this target is no longer
                // in-flight.
                drop(in_flight);
                let terminal = crate::stream_timeout::finish_bridge_stream(
                    &state_c.health,
                    &state_c.runtime_status,
                    &model_display_name_c,
                    &model_id_c,
                    cooldown_c.as_ref(),
                    stream_failure.map(|failure| failure.bridge_error()),
                    usage.reached_end,
                    200,
                    attempt_started,
                );
                if let Some(deferred) = &attempt_c.deferred {
                    deferred.finish(
                        &state_c,
                        if refused { 200 } else { terminal.status },
                        refused || (terminal.error_class.is_empty() && terminal.status < 400),
                    );
                }
                let mut terminal_attempt = attempt_c;
                if let Some(refusal) = refusal {
                    refusal.apply_to(&mut terminal_attempt);
                } else if let Some(f) = usage.failure.as_ref() {
                    f.apply_to(&mut terminal_attempt);
                }
                // Content capture (#947): prompt captured up front,
                // output text assembled by the stream wrapper (empty when no
                // exporter wants content).
                // A provider call the chain failed open on left text nobody
                // read, whatever the stream itself says. A hold decides its
                // capture with its verdict.
                let captured_content = match (&captured_prompt_c, content_cap) {
                    (Some(prompt), Some(cap))
                        if !hold_back
                            && input_capture_safe
                            && output_capture_safe
                            && crate::usage_attr::bypass_reason(&audit_c).is_empty() =>
                    {
                        Some(CapturedContent::new(prompt, &out_text, cap as usize))
                    }
                    _ => None,
                };
                let attempt_elapsed = attempt_started.elapsed();
                // A stream the consumer abandoned mid-flight is reported as
                // 499. An upstream failure after the headers — a transport
                // error, a read timeout, an in-band `error` or
                // `response.failed` event — is recorded as that failure's
                // status and error. A refusal is the 200 the head committed.
                let terminal_status = crate::attempt::guarded_stream_status(
                    usage.reached_end,
                    refused,
                    usage.failure.as_ref(),
                );
                let snap_c = state_c.snapshot.load();
                let pk_c = ResolvedPk::resolve(&snap_c, &provider_key_id_c);
                // Output masking happens only under a hold; on the live path
                // (a monitor-mode chain suppresses its masks) just the
                // input-side counts apply. The end-of-stream scan's monitor
                // observations ride along with the input-side hits (#1010).
                let mut monitor_hits = input_monitor_hits.clone();
                monitor_hits.extend(output_hits);
                let mut redactions = input_redactions.clone();
                if let Some(held) = held {
                    crate::redact::merge_counts(&mut redactions, held.redactions.clone());
                }
                // A stream can outlive several config generations, so the
                // end-of-stream emit reads a FRESH snapshot rather than the
                // one the request started on (#941).
                // 花费层记账（micro-USD），与上面的 token 记账成对。取值与
                // 下面用量事件里的 cost_usd 同源：同一个快照、同一份 usage。
                limiter_c.add_tokens_post_stream_all(
                    &spend_post_stream_keys,
                    crate::usage_attr::request_spend_micro_usd(
                        &snap_c,
                        &model_id_c,
                        crate::usage_attr::input_tokens_for_pricing(
                            u64::from(usage.prompt_tokens),
                            u64::from(usage.cached_prompt_tokens),
                            u64::from(usage.cache_write_tokens.unwrap_or(0)),
                            u64::from(usage.cache_read_tokens),
                            u64::from(usage.cache_creation_tokens),
                        ),
                        u64::from(usage.completion_tokens),
                    ),
                );
                emit_usage_event(
                    &state_c,
                    &snap_c,
                    &pk_c,
                    &request_id_c,
                    &model_id_c,
                    &requested_model_c,
                    &metric_model,
                    metric_caller.as_caller(),
                    &provider_c,
                    &metric_upstream_model,
                    // The upstream work still happened, so the event is
                    // emitted however the stream ended — only its outcome
                    // differs.
                    terminal_status,
                    // Attempt-scoped: any failed attempt before this one
                    // emitted its own event.
                    attempt_elapsed,
                    &usage,
                    // Streaming is outside the cache gate.
                    crate::response_cache::CacheTelemetry::default(),
                    &client_c,
                    terminal_attempt,
                    refused,
                    redactions,
                    monitor_hits,
                    held.and_then(|held| held.captured.as_ref())
                        .or(captured_content.as_ref()),
                    /* terminal */ true,
                    /* dispatched */ true,
                    &audit_c,
                );
            };
        let parsed_stream: RawResponsesByteStream = if hold_back {
            Box::pin(build_held_responses_stream(
                body_stream,
                Arc::clone(&chain_arc),
                started,
                attempt_started,
                content_cap,
                captured_prompt.filter(|_| input_capture_safe),
                crate::responses_bridge::ResponsesReplyContext::from_request(body),
                requested_model.to_string(),
                upstream_model.clone(),
                body_bytes.clone(),
                model.display_name.clone(),
                mark,
                held_verdict,
                on_complete,
            ))
        } else {
            Box::pin(build_responses_passthrough_stream(
                body_stream,
                started,
                attempt_started,
                None,
                content_cap,
                eos_scan,
                initial_capture_safe,
                requested_model.to_string(),
                mark,
                on_complete,
            ))
        };
        let mut response = axum::response::Response::new(axum::body::Body::from_stream(
            crate::sse_keepalive::with_heartbeat(parsed_stream, crate::sse_keepalive::interval()),
        ));
        apply_passthrough_headers(&mut response, &headers, request_id, state.platform());

        Ok(ResponseDispatchSuccess {
            response,
            provider: provider_label,
            // The Drop guard owns the emit; the handler must not double-emit.
            usage: None,
            model_id: model_id.to_string(),
            provider_key_id,
            upstream_model: upstream_model.clone(),
            routing: RoutingTelemetry::default(),
            guardrail_blocked: false,
            guardrail_block_message: String::new(),
            usage_handled_by_stream: true,
            // The Drop guard's emit carries the counts.
            output_redactions: crate::redact::RedactionCounts::new(),
            output_monitor_hits: Vec::new(),
            // The Drop guard's emit carries the captured content too.
            captured_content: None,
            cache: crate::response_cache::CacheTelemetry::default(),
        })
    } else {
        let json_body: Value =
            crate::dispatch::json_body_within(upstream_resp, buffered_body_deadline)
                .await
                .and_then(|body| match responses_body_in_band_error(&body) {
                    Some(err) => Err(err),
                    None => Ok(body),
                })
                .map_err(|be| {
                    state.health.record_failure(&model.display_name);
                    crate::cooldown::note_failure(
                        &state.runtime_status,
                        model_id,
                        model.cooldown.as_ref(),
                        be,
                    )
                })
                .map_err(ProxyError::Bridge)?;
        state.health.record_success(&model.display_name);
        state.runtime_status.mark_healthy(model_id, attempt_started);

        // Extract the upstream-reported usage block for telemetry
        // emission. Pulled here (before the response is moved into
        // `Json::into_response`) so the success struct can carry
        // typed counters rather than re-parsing JSON downstream.
        // Token-estimation fallback (#1074): a body with zero or
        // missing usage fills the counters locally — telemetry only, the
        // response body is forwarded untouched. A body with no `usage`
        // object at all becomes a wholly-estimated record instead of None.
        let usage = {
            let mut u = extract_response_usage(&json_body).unwrap_or_default();
            // `extract_response_usage` returns None on a body with no usable
            // `usage` block, and the estimation fallback below then works off
            // a default — which would drop a perfectly good top-level `id`
            // (#1289).
            if u.provider_request_id.is_empty() {
                u.provider_request_id = crate::usage_attr::provider_response_id(&json_body);
            }
            if u.service_tier.is_none() {
                u.service_tier = response_service_tier(&json_body);
            }
            if u.prompt_tokens == 0 || u.completion_tokens == 0 {
                let est = crate::token_estimate::Estimator::new(
                    &upstream_model,
                    crate::token_estimate::PromptInput::Responses(crate::util::outbound_value(
                        &body_bytes,
                    )),
                );
                let filled = crate::token_estimate::fill_missing(
                    &est,
                    u.prompt_tokens,
                    u.cache_read_tokens.saturating_add(u.cache_creation_tokens),
                    u.completion_tokens,
                    Some(&responses_output_text(&json_body)),
                );
                if filled.estimated {
                    u.prompt_tokens = filled.prompt_tokens;
                    u.completion_tokens = filled.completion_tokens;
                    u.usage_estimated = true;
                }
            }
            Some(u)
        };

        // #719: run the output guardrail chain on the assistant's text so a
        // configured output block isn't bypassable by calling /v1/responses
        // (the input half is enforced in `dispatch`). Only when an
        // output-hook guardrail is attached; otherwise this is a no-op.
        let mut json_body = json_body;
        let mut output_seg_counts = crate::redact::RedactionCounts::new();
        let mut output_monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
        let mut output_capture_safe = true;
        if aisix_guardrails::Guardrail::runs_on_output(chain) {
            let synth = synth_chat_response(&upstream_model, responses_output_text(&json_body));
            let (verdict, hits) =
                aisix_guardrails::Guardrail::check_output_non_segment_observed(chain, &synth).await;
            output_monitor_hits.extend(hits);
            let moderation = crate::redact::moderate_responses_response_structured(
                chain,
                verdict,
                &mut json_body,
                &mut output_seg_counts,
                &mut output_monitor_hits,
            )
            .await;
            output_capture_safe = moderation.capture_safe;
            let mut verdict = moderation.verdict;
            if !verdict.is_block() {
                let redaction =
                    crate::redact::redact_responses_response_structured(chain, &mut json_body);
                crate::redact::merge_counts(&mut output_seg_counts, redaction.counts);
                if redaction.unrewritable_tool_key {
                    verdict = crate::redact::unrewritable_tool_key_verdict();
                }
            }
            if let aisix_guardrails::GuardrailVerdict::Block {
                reason,
                guardrail_name,
                unavailable,
            } = verdict
            {
                // Per #153 the matched-pattern detail stays in ops logs only.
                tracing::warn!(
                    guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
                    guardrail_hook = "output",
                    model = %model.display_name,
                    reason = %reason,
                    "guardrail blocked /v1/responses response",
                );
                // #543: the provider already billed for this response, so
                // return a 422 body BUT carry the billed `usage` (marked
                // guardrail_blocked) — recording zero tokens would let the
                // customer's ledger underreport spend they were charged for.
                // This is the output analog of chat.rs's UpstreamCharge.
                let block = crate::error::guardrail_block_error(
                    "response",
                    guardrail_name.as_deref(),
                    unavailable.as_deref(),
                );
                let guardrail_block_message = crate::attempt::gateway_error_message(&block);
                return Ok(ResponseDispatchSuccess {
                    response: block.into_response(),
                    provider: provider_label,
                    usage,
                    model_id: model_id.to_string(),
                    provider_key_id: provider_key_id.clone(),
                    upstream_model: upstream_model.clone(),
                    routing: RoutingTelemetry::default(),
                    guardrail_blocked: true,
                    guardrail_block_message,
                    usage_handled_by_stream: false,
                    output_redactions: crate::redact::RedactionCounts::new(),
                    // Hits observed by the blocking check still count.
                    output_monitor_hits,
                    // #1013: the billed-then-blocked event carries
                    // the (post-mask) prompt; the blocked output itself stays
                    // out of the log — blocking it and then archiving it would
                    // defeat the block.
                    captured_content: match (&captured_prompt, content_cap) {
                        (Some(p), Some(cap)) if input_capture_safe => {
                            Some(CapturedContent::new(p, "", cap as usize))
                        }
                        _ => None,
                    },
                    cache: crate::response_cache::CacheTelemetry::default(),
                });
            }
        }

        let output_redactions = output_seg_counts;

        // Echo the model name the caller addressed, not the id the upstream
        // answered with — the same contract the bridged half of this endpoint
        // already honours, so `/v1/responses` stops answering differently
        // depending on which provider happens to be behind the alias.
        crate::model_echo::restamp_body(&mut json_body, requested_model);

        crate::attribution::note_responses_result(&json_body);
        // Content capture (#947): the assistant's assembled output
        // text, read from the POST-redaction body so masked PII stays masked
        // in the exported content.
        let captured_content = match (&captured_prompt, content_cap) {
            (Some(prompt), Some(cap)) if input_capture_safe && output_capture_safe => Some(
                CapturedContent::new(
                    prompt,
                    &responses_output_content_text_capped(&json_body, usize::MAX).0,
                    cap as usize,
                )
                .with_output(Some(aisix_obs::OutputCollector::from_body(
                    aisix_obs::OutputWire::OpenAiResponses,
                    cap as usize,
                    &json_body,
                ))),
            ),
            _ => None,
        };

        // Last, after every telemetry read of the body, so the operator's
        // copy keeps the upstream's own `error` and only the caller's is a
        // platform class.
        crate::platform_errors::relayed_failed_response(&mut json_body);
        mark.apply_to_body(&mut json_body);
        Ok(ResponseDispatchSuccess {
            response: Json(json_body).into_response(),
            provider: provider_label,
            usage,
            model_id: model_id.to_string(),
            provider_key_id,
            upstream_model,
            routing: RoutingTelemetry::default(),
            guardrail_blocked: false,
            guardrail_block_message: String::new(),
            usage_handled_by_stream: false,
            output_redactions,
            output_monitor_hits,
            captured_content,
            cache: crate::response_cache::CacheTelemetry::default(),
        })
    }
}

/// Dispatch one non-OpenAI target by bridging the Responses-API request
/// through the gateway's canonical [`ChatFormat`] and the provider
/// [`Bridge`](aisix_gateway::Bridge), then re-encoding the response back
/// into the Responses-API shape (#825). This is what lets clients like
/// `codex` — which speak only the OpenAI Responses API — reach an
/// Anthropic (or any other) backend. Mirrors `messages::cross_provider_dispatch`.
#[allow(clippy::too_many_arguments)]
async fn responses_cross_provider_to_target(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    body: &Value,
    // The context tags this attempt's response carries back.
    mark: crate::responses_affinity::ResponseMark,
    model: &Arc<aisix_core::Model>,
    model_id: &str,
    // Deadlines resolved by the caller across target → group → deployment
    // default (`routing::effective_timeouts`); this fn only applies them.
    timeouts: crate::routing::TimeoutBudget,
    request_id: &str,
    chain: Arc<aisix_guardrails::GuardrailChain>,
    started: Instant,
    // When THIS attempt began — see `responses_to_target`.
    attempt_started: Instant,
    requested_model: &str,
    api_key_id: &str,
    client_ctx: &ClientContext,
    attempt: AttemptInfo,
    reservation: &mut Option<aisix_ratelimit::MultiReservation>,
    // This target's own model-layer reservation (routing dispatch only,
    // #1087). The streaming path folds it into `reservation`
    // before the take below; the non-streaming path leaves it for the
    // handler to commit alongside `reservation`.
    member_reservation: &mut Option<aisix_ratelimit::MultiReservation>,
    // Input-side PII mask counts (#932), merged into the streamed judge
    // path's end-of-stream emit; non-streaming emits happen in the handler.
    input_redactions: crate::redact::RedactionCounts,
    // Input-side monitor hits (#562), same lifecycle as
    // `input_redactions`.
    input_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    input_capture_safe: bool,
) -> Result<ResponseDispatchSuccess, ProxyError> {
    use aisix_gateway::Bridge;

    // Content capture (#947), same contract as the verbatim
    // target: prompt = the client-facing Responses request body
    // (post-#932-redaction), gated on an exporter actually wanting content.
    let content_cap = content_capture_cap(
        snapshot
            .observability_exporters
            .entries()
            .iter()
            .map(|e| &*e.value),
    );

    let provider = model
        .provider
        .as_deref()
        .ok_or_else(|| {
            ProxyError::UnusableTarget(format!("model `{requested_model}` has no provider prefix"))
        })?
        .to_string();
    let pk_entry = crate::dispatch::resolve_provider_key(snapshot, model)?;
    // Resolved PK id for per-PK telemetry attribution on the emitted
    // UsageEvent (#867).
    let provider_key_id = pk_entry.id.clone();
    let bridge: Arc<dyn Bridge> = crate::dispatch::resolve_bridge(&state.hub, &pk_entry.value)
        .ok_or(ProxyError::ProviderUnavailable)?;

    // Faithful Responses → ChatFormat transform; `chat.model` stays the
    // operator-facing name so the bridge re-resolves the upstream id via
    // `ctx.model.upstream_model()` exactly like chat.rs.
    let outbound_body = crate::effort_mapping::responses_request(body, model);
    let mut chat =
        crate::responses_bridge::responses_request_to_chat(requested_model, outbound_body.as_ref());
    chat.thoughts_requested =
        aisix_gateway::chat::effort_asks_for_thoughts(body.pointer("/reasoning/effort"));
    // A rewritten body is a whole-request copy; the translation above is
    // all this attempt needs from it.
    drop(outbound_body);
    // `custom` tools and namespace sub-tools travel upstream as plain
    // function tools, so only the request's own tool list can tell the
    // reply translators which item each of the model's calls goes back as;
    // the request also supplies the settings every Response object echoes.
    let mut reply = crate::responses_bridge::ResponsesReplyContext::from_request(body);
    if let Some(sent) = mark.previous_response_id_sent() {
        reply.echo_previous_response_id(sent);
    }

    let is_stream = chat.is_streaming();
    let mut ctx = crate::dispatch::bridge_ctx(
        request_id,
        model_id,
        Arc::clone(model),
        &provider_key_id,
        Arc::clone(&pk_entry.value),
        Some(client_ctx),
    );
    let connect_deadline = if is_stream {
        timeouts.stream
    } else {
        timeouts.request
    };
    if let Some(d) = connect_deadline {
        ctx = ctx.with_deadline(d);
    }
    // See chat.rs: the structured-output tool route answers a streaming
    // request with a non-streaming upstream leg, which is entitled to
    // the end-to-end budget rather than the per-chunk one.
    if is_stream {
        ctx = ctx.with_non_streaming_deadline(timeouts.request);
    }
    let provider_label = provider.to_ascii_lowercase();

    // least_busy: count this target as in-flight for the upstream call
    // (mirrors chat.rs). Non-streaming / error paths drop the guard at
    // function return; the streaming branch moves it into the
    // end-of-stream closure next to `stream_hold`, so the count stays
    // raised for the stream's full lifetime.
    let in_flight = state.runtime_status.begin_in_flight(model_id);

    if is_stream {
        let upstream = bridge
            .chat_stream_cow(std::borrow::Cow::Owned(chat), &ctx)
            .await
            .map_err(|err| {
                if err.http_status() >= 500 {
                    state.health.record_failure(&model.display_name);
                }
                if let Some((ttl, reason)) =
                    crate::cooldown::decide_cooldown(&err, model.cooldown.as_ref())
                {
                    state.runtime_status.mark_cooldown(model_id, ttl, reason);
                }
                ProxyError::Bridge(err)
            })?;
        // Serialized once the upstream has answered, not before: a copy made
        // up front would be alive for the whole upstream wait.
        let captured_prompt = content_cap.map(|_| serde_json::to_string(body).unwrap_or_default());
        // #554: peek the first chunk so a slow/erroring first token fails
        // over before the 200 is committed (when a stream budget is set);
        // the wrapper keeps enforcing the per-chunk read timeout either way.
        let stream_budget = timeouts.stream;
        let upstream = crate::stream_timeout::with_read_timeout(upstream, stream_budget);
        let upstream: aisix_gateway::ChatChunkStream = if timeouts.stream_configured {
            let mut upstream = upstream;
            let first_chunk = match upstream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(err)) => {
                    if err.http_status() >= 500 {
                        state.health.record_failure(&model.display_name);
                    }
                    if let Some((ttl, reason)) =
                        crate::cooldown::decide_cooldown(&err, model.cooldown.as_ref())
                    {
                        state.runtime_status.mark_cooldown(model_id, ttl, reason);
                    }
                    return Err(ProxyError::Bridge(err));
                }
                None => {
                    let err = aisix_gateway::BridgeError::StreamAborted;
                    state.health.record_failure(&model.display_name);
                    if let Some((ttl, reason)) =
                        crate::cooldown::decide_cooldown(&err, model.cooldown.as_ref())
                    {
                        state.runtime_status.mark_cooldown(model_id, ttl, reason);
                    }
                    return Err(ProxyError::Bridge(err));
                }
            };
            Box::pin(
                futures::stream::once(std::future::ready(Ok::<_, aisix_gateway::BridgeError>(
                    first_chunk,
                )))
                .chain(upstream),
            )
        } else {
            upstream
        };
        let response_id = mark.response_id(format!("resp_{}", Uuid::new_v4().simple()));
        let created_at = chrono::Utc::now().timestamp();
        let encoder = crate::responses_bridge::ResponsesSseEncoder::new(
            response_id,
            requested_model,
            created_at,
            reply,
        )
        .sealing_ids(mark.sealer());
        // Only an output-hook guardrail needs the streamed response text.
        // When attached with a hold-back policy (Window/BufferFull — any
        // block-capable chain), the bridge buffers the SSE and scans before
        // releasing it (#719 secure default); cap the buffer the same way the
        // verbatim path does so a huge response can't OOM the gateway. A
        // monitor-only chain resolves to EndOfStreamCheck — it can never
        // block, so the bridge forwards live and scans at end-of-stream for
        // observation only (#1010).
        let output_guardrail = (!chain.is_empty()
            && aisix_guardrails::Guardrail::runs_on_output(chain.as_ref()))
        .then(|| chain.clone());
        let output_policy = aisix_guardrails::Guardrail::stream_output_policy(chain.as_ref());
        let hold_back = output_policy.holds_back();
        let (max_buffer_bytes, on_exceeded_fail_open) = output_policy
            .hold_cap()
            .unwrap_or((aisix_guardrails::DEFAULT_STREAM_OUTPUT_BUFFER_BYTES, false));

        let state_c = state.clone();
        // The chain does not survive into the end-of-stream closure, so
        // clone the audit handle here and read it there — the streaming
        // paths are where an output-hook mask actually lands
        // (AISIX-Cloud#1330 / #1024).
        let audit_c = chain.audit_log();
        let request_id_c = request_id.to_string();
        let model_id_c = model_id.to_string();
        let requested_model_c = requested_model.to_string();
        let provider_key_id_c = provider_key_id.clone();
        let provider_c = provider_label.clone();
        let upstream_model_c = model.upstream_model().unwrap_or("unknown").to_string();
        let (metric_model, metric_upstream_model) = crate::usage_attr::metric_model_label_pair(
            snapshot,
            requested_model,
            &upstream_model_c,
        );
        let metric_caller = crate::request_metrics::Caller::from_api_key_id(snapshot, api_key_id);
        let metric_model = metric_model.into_owned();
        let metric_upstream_model = metric_upstream_model.into_owned();
        let client_c = client_ctx.clone();
        let attempt_c = attempt.clone();
        let model_display_name_c = model.display_name.clone();
        let cooldown_c = model.cooldown.clone();
        // #688: carry the reservation into the end-of-stream guard — keys drive
        // post-stream TPM/TPD accounting, the hold keeps the concurrency slot(s)
        // until the stream ends. `take()` leaves the handler's `reservation` as
        // `None` so it won't also `commit_tokens_no_spend`.
        // Fold this target's model-layer reservation in first (#1087)
        // so the guard covers the member's limits too; `take()` leaves it `None`
        // so the handler won't also commit it.
        if let Some(member) = member_reservation.take() {
            match reservation.as_mut() {
                Some(main) => main.merge(member),
                None => *reservation = Some(member),
            }
        }
        // 花费层的桶记 micro-USD，和 token 桶不能共用一张键表——同一批键
        // 加同一个数字会把 token 数当成钱记进去。
        let post_stream_keys = reservation
            .as_ref()
            .map(|r| r.token_keys())
            .unwrap_or_default();
        let spend_post_stream_keys = reservation
            .as_ref()
            .map(|r| r.spend_keys())
            .unwrap_or_default();
        let stream_hold = reservation.take().map(|r| r.into_stream_hold());
        let limiter_c = std::sync::Arc::clone(&state.limiter);
        let captured_prompt_c = captured_prompt.clone();
        // Token-estimation fallback context (#1074): the request
        // body is cloned because the stream owns it until an end-of-stream
        // Drop. Tokenized only if the bridged upstream never reports usage.
        let estimator = crate::token_estimate::Estimator::new(
            model.upstream_model().unwrap_or("unknown"),
            crate::token_estimate::PromptInput::Responses(body.clone()),
        );
        let sse_body = crate::responses_bridge::build_responses_bridge_stream(
            upstream,
            encoder,
            started,
            attempt_started,
            output_guardrail,
            hold_back,
            max_buffer_bytes,
            on_exceeded_fail_open,
            requested_model.to_string(),
            content_cap,
            Some(estimator),
            move |mut comp| {
                // #688: apply the terminal token cost to TPM/TPD and release the
                // concurrency hold now the stream has ended (sync analog of the
                // reservation's async `commit_tokens_no_spend`). Tokens count even on an
                // output-guardrail block — the upstream still billed them.
                let streamed_tokens = total_tokens_with_cache(
                    comp.prompt_tokens,
                    comp.completion_tokens,
                    comp.cache_creation_tokens,
                    comp.cache_read_tokens,
                );
                limiter_c.add_tokens_post_stream_all(&post_stream_keys, streamed_tokens);
                drop(stream_hold);
                // least_busy: stream over — this target is no longer
                // in-flight.
                drop(in_flight);
                let usage = ResponseUsage {
                    reached_end: comp.reached_end,
                    prompt_tokens: comp.prompt_tokens,
                    completion_tokens: comp.completion_tokens,
                    reasoning_tokens: comp.reasoning_tokens,
                    cached_prompt_tokens: comp.cached_prompt_tokens,
                    cache_write_tokens: comp.cache_write_tokens,
                    modality: comp.modality,
                    cache_creation_ttl: comp.cache_creation_ttl,
                    cache_creation_tokens: comp.cache_creation_tokens,
                    cache_read_tokens: comp.cache_read_tokens,
                    upstream_total_tokens: comp.upstream_total_tokens.unwrap_or(0),
                    reasoning_folded_into_completion: comp.reasoning_folded_into_completion,
                    usage_estimated: comp.usage_estimated,
                    upstream_ttft_ms: comp.upstream_ttft_ms,
                    upstream_ttft: comp.upstream_ttft,
                    downstream_latency_ms: comp.downstream_latency_ms,
                    provider_request_id: comp.provider_request_id,
                    service_tier: comp.service_tier,
                    failure: comp.failure.clone(),
                };
                // A typed upstream-body failure retains its 5xx status. A
                // guardrail block and a clean EOF are 200, and only an actual
                // consumer cancellation is 499. This settles health, cooldown
                // and the deployment metric, which wait for the stream's real
                // ending rather than its head.
                let terminal = crate::stream_timeout::finish_bridge_stream(
                    &state_c.health,
                    &state_c.runtime_status,
                    &model_display_name_c,
                    &model_id_c,
                    cooldown_c.as_ref(),
                    comp.terminal_failure.take(),
                    comp.reached_end || comp.guardrail_blocked,
                    200,
                    attempt_started,
                );
                if let Some(deferred) = &attempt_c.deferred {
                    deferred.finish(
                        &state_c,
                        if comp.guardrail_blocked {
                            200
                        } else {
                            terminal.status
                        },
                        comp.guardrail_blocked
                            || (terminal.error_class.is_empty() && terminal.status < 400),
                    );
                }
                // A clean stream is a committed 200, and so is an
                // output-guardrail block (or fail-closed overflow): this head
                // went out before the verdict, so the row says what the caller
                // received, bills the upstream tokens, and its guardrail_blocked
                // is what the Blocked tab + ledger see. An abandoned stream is a
                // 499, one an upstream failure ended carries that failure's
                // status and error.
                let status = crate::attempt::guarded_stream_status(
                    comp.reached_end && !comp.verdict_pending,
                    comp.guardrail_blocked,
                    comp.failure.as_ref(),
                );
                let mut attempt_c = attempt_c;
                if let Some(r) = comp.refusal.as_ref() {
                    r.apply_to(&mut attempt_c);
                } else if let Some(f) = comp.failure.as_ref().filter(|_| !comp.guardrail_blocked) {
                    f.apply_to(&mut attempt_c);
                }
                // Content capture (#947): prompt captured up front,
                // response assembled across the bridged stream into
                // `comp.response_text` (empty when no exporter wants content
                // or when the response was blocked before release).
                let captured_content = match (&captured_prompt_c, content_cap) {
                    (Some(prompt), Some(cap))
                        if input_capture_safe && comp.response_capture_safe =>
                    {
                        Some(CapturedContent::new(
                            prompt,
                            &comp.response_text,
                            cap as usize,
                        ))
                    }
                    _ => None,
                };
                let attempt_elapsed = attempt_started.elapsed();
                let snap_c = state_c.snapshot.load();
                let pk_c = ResolvedPk::resolve(&snap_c, &provider_key_id_c);
                // A stream can outlive several config generations, so the
                // end-of-stream emit reads a FRESH snapshot rather than the
                // one the request started on (#941).
                // 花费层记账（micro-USD），与上面的 token 记账成对。取值与
                // 下面用量事件里的 cost_usd 同源：同一个快照、同一份 usage。
                limiter_c.add_tokens_post_stream_all(
                    &spend_post_stream_keys,
                    crate::usage_attr::request_spend_micro_usd(
                        &snap_c,
                        &model_id_c,
                        crate::usage_attr::input_tokens_for_pricing(
                            u64::from(usage.prompt_tokens),
                            u64::from(usage.cached_prompt_tokens),
                            u64::from(usage.cache_write_tokens.unwrap_or(0)),
                            u64::from(usage.cache_read_tokens),
                            u64::from(usage.cache_creation_tokens),
                        ),
                        u64::from(usage.completion_tokens),
                    ),
                );
                emit_usage_event(
                    &state_c,
                    &snap_c,
                    &pk_c,
                    &request_id_c,
                    &model_id_c,
                    &requested_model_c,
                    &metric_model,
                    metric_caller.as_caller(),
                    &provider_c,
                    &metric_upstream_model,
                    status,
                    // Attempt-scoped — see the sibling verbatim path.
                    attempt_elapsed,
                    &usage,
                    // Streaming is outside the cache gate.
                    crate::response_cache::CacheTelemetry::default(),
                    &client_c,
                    attempt_c,
                    comp.guardrail_blocked,
                    // #932: input-side counts merged with the hold-back
                    // release's output-side counts.
                    {
                        let mut merged = input_redactions.clone();
                        crate::redact::merge_counts(&mut merged, comp.redacted_entity_counts);
                        merged
                    },
                    {
                        let mut merged = input_monitor_hits.clone();
                        merged.extend(comp.monitor_hits);
                        merged
                    },
                    captured_content.as_ref(),
                    /* terminal */ true,
                    /* dispatched */ true,
                    &audit_c,
                );
            },
        );
        let mut response = axum::response::Response::new(sse_body);
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        );
        if let Ok(hv) = HeaderValue::from_str(request_id) {
            response
                .headers_mut()
                .insert(HeaderName::from_static("x-aisix-request-id"), hv);
        }
        return Ok(ResponseDispatchSuccess {
            response,
            provider: provider_label,
            usage: None,
            model_id: model_id.to_string(),
            provider_key_id,
            upstream_model: model.upstream_model().unwrap_or("unknown").to_string(),
            routing: RoutingTelemetry::default(),
            guardrail_blocked: false,
            guardrail_block_message: String::new(),
            usage_handled_by_stream: true,
            // The stream's end-of-stream emit carries the counts.
            output_redactions: crate::redact::RedactionCounts::new(),
            output_monitor_hits: Vec::new(),
            // The stream's end-of-stream emit carries the captured content.
            captured_content: None,
            cache: crate::response_cache::CacheTelemetry::default(),
        });
    }

    // Non-streaming.
    let mut resp = bridge
        .chat_cow(std::borrow::Cow::Owned(chat), &ctx)
        .await
        .map_err(|err| {
            if err.http_status() >= 500 {
                state.health.record_failure(&model.display_name);
            }
            if let Some((ttl, reason)) =
                crate::cooldown::decide_cooldown(&err, model.cooldown.as_ref())
            {
                state.runtime_status.mark_cooldown(model_id, ttl, reason);
            }
            ProxyError::Bridge(err)
        })?;
    // Serialized once the upstream has answered, not before: a copy made
    // up front would be alive for the whole upstream wait.
    let captured_prompt = content_cap.map(|_| serde_json::to_string(body).unwrap_or_default());
    state.health.record_success(&model.display_name);
    state.runtime_status.mark_healthy(model_id, attempt_started);

    let usage = {
        let mut u = ResponseUsage {
            // Non-streaming: the response was received in full or this code
            // would not run.
            reached_end: true,
            prompt_tokens: resp.usage.prompt_tokens,
            completion_tokens: resp.usage.completion_tokens,
            reasoning_tokens: resp.usage.reasoning_tokens,
            cached_prompt_tokens: resp.usage.cached_prompt_tokens,
            cache_write_tokens: resp.usage.cache_write_tokens,
            modality: resp.usage.modality,
            cache_creation_ttl: resp.usage.cache_creation_ttl,
            cache_creation_tokens: resp.usage.cache_creation_tokens,
            cache_read_tokens: resp.usage.cache_read_tokens,
            upstream_total_tokens: resp.usage.upstream_total_tokens,
            reasoning_folded_into_completion: resp.usage.reasoning_folded_into_completion,
            usage_estimated: false,
            upstream_ttft_ms: 0,
            upstream_ttft: None,
            downstream_latency_ms: 0,
            // The bridged upstream's own id, not the `resp_…` re-encoded
            // below — that one is minted here and means nothing to the
            // provider (#1289).
            provider_request_id: crate::usage_attr::sanitize_provider_response_id(&resp.id),
            service_tier: resp.service_tier.as_ref().map(|t| t.as_str().to_owned()),
            failure: None,
        };
        // Token-estimation fallback (AISIX-Cloud#1074): fill counters the
        // bridged upstream never reported, and carry the SAME numbers into
        // the Responses JSON re-encoded below. The client-visible usage and
        // the usage record are one number: a caller told `output_tokens: 0`
        // for a response it can read the text of has no way to reconcile
        // that with what the dashboard bills. The estimate is reported in
        // the ordinary usage shape — there is no client-facing marker
        // saying it was estimated.
        if u.prompt_tokens == 0 || u.completion_tokens == 0 {
            let est = crate::token_estimate::Estimator::new(
                model.upstream_model().unwrap_or("unknown"),
                crate::token_estimate::PromptInput::Responses(body.clone()),
            );
            let filled = crate::token_estimate::fill_missing(
                &est,
                u.prompt_tokens,
                u.cache_read_tokens.saturating_add(u.cache_creation_tokens),
                u.completion_tokens,
                Some(&crate::chat::estimation_output_text(&resp)),
            );
            if filled.estimated {
                u.prompt_tokens = filled.prompt_tokens;
                u.completion_tokens = filled.completion_tokens;
                u.usage_estimated = true;
                resp.usage.prompt_tokens = filled.prompt_tokens;
                resp.usage.completion_tokens = filled.completion_tokens;
                // A total the upstream reported beside a zero sub-counter
                // no longer adds up once that zero is filled; zeroing it
                // makes the projection derive prompt + completion, the
                // same arithmetic it uses when no total was reported.
                resp.usage.total_tokens = 0;
            }
        }
        u
    };

    // #719: run output guardrails on the bridged response before re-encoding
    // it as Responses JSON — the assistant text + tool calls are
    // client-visible output, scanned the same way /v1/chat/completions does.
    let mut output_seg_counts = crate::redact::RedactionCounts::new();
    let mut output_monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
    let mut output_capture_safe = true;
    if aisix_guardrails::Guardrail::runs_on_output(chain.as_ref()) {
        let (verdict, hits) =
            aisix_guardrails::Guardrail::check_output_non_segment_observed(chain.as_ref(), &resp)
                .await;
        output_monitor_hits.extend(hits);
        let moderation = crate::redact::moderate_chat_response_structured(
            chain.as_ref(),
            verdict,
            &mut resp,
            &mut output_seg_counts,
            &mut output_monitor_hits,
        )
        .await;
        output_capture_safe = moderation.capture_safe;
        let mut verdict = moderation.verdict;
        if !verdict.is_block() {
            let redaction =
                crate::redact::redact_chat_response_structured(chain.as_ref(), &mut resp);
            crate::redact::merge_counts(&mut output_seg_counts, redaction.counts);
            if redaction.unrewritable_tool_key {
                verdict = crate::redact::unrewritable_tool_key_verdict();
            }
        }
        if let aisix_guardrails::GuardrailVerdict::Block {
            reason,
            guardrail_name,
            unavailable,
        } = verdict
        {
            tracing::warn!(
                guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
                guardrail_hook = "output",
                model = %requested_model,
                reason = %reason,
                "guardrail blocked /v1/responses (cross-provider) response",
            );
            // #543: the upstream already billed — return the 422 body but
            // carry the billed usage (marked guardrail_blocked) so the
            // ledger doesn't underreport spend.
            let block = crate::error::guardrail_block_error(
                "response",
                guardrail_name.as_deref(),
                unavailable.as_deref(),
            );
            let guardrail_block_message = crate::attempt::gateway_error_message(&block);
            return Ok(ResponseDispatchSuccess {
                response: block.into_response(),
                provider: provider_label,
                usage: Some(usage),
                model_id: model_id.to_string(),
                provider_key_id: provider_key_id.clone(),
                upstream_model: model.upstream_model().unwrap_or("unknown").to_string(),
                routing: RoutingTelemetry::default(),
                guardrail_blocked: true,
                guardrail_block_message,
                usage_handled_by_stream: false,
                output_redactions: crate::redact::RedactionCounts::new(),
                // Hits observed by the blocking check still count.
                output_monitor_hits,
                // #1013: the billed-then-blocked event carries
                // the (post-mask) prompt; the blocked output itself stays
                // out of the log — blocking it and then archiving it would
                // defeat the block.
                captured_content: match (&captured_prompt, content_cap) {
                    (Some(p), Some(cap)) if input_capture_safe => {
                        Some(CapturedContent::new(p, "", cap as usize))
                    }
                    _ => None,
                },
                cache: crate::response_cache::CacheTelemetry::default(),
            });
        }
    }

    let output_redactions = output_seg_counts;

    let created_at = chrono::Utc::now().timestamp();
    let mut json_body = crate::responses_bridge::chat_response_to_responses_json(
        &resp,
        requested_model,
        created_at,
        &reply,
    );
    crate::attribution::note_responses_result(&json_body);
    // Content capture (#947): the client-visible Responses JSON
    // (post-redaction) is the source, so the exported text matches what the
    // caller received.
    let captured_content = match (&captured_prompt, content_cap) {
        (Some(prompt), Some(cap)) if input_capture_safe && output_capture_safe => Some(
            CapturedContent::new(
                prompt,
                &responses_output_content_text_capped(&json_body, usize::MAX).0,
                cap as usize,
            )
            .with_output(Some(aisix_obs::OutputCollector::from_body(
                aisix_obs::OutputWire::OpenAiResponses,
                cap as usize,
                &json_body,
            ))),
        ),
        _ => None,
    };
    // Platform mode: the usage this bridge wrote keeps to OpenAI's fields. A
    // relayed answer keeps its upstream's, so this is not part of the mark.
    if mark.sealer().is_some() {
        crate::opaque_id::drop_bridged_usage_extensions(&mut json_body);
    }
    mark.apply_to_body(&mut json_body);
    let mut response = Json(json_body).into_response();
    if !crate::response_cache::answer_is_storable(&resp) {
        response
            .extensions_mut()
            .insert(crate::response_cache::UnstorableAnswer);
    }
    if let Ok(hv) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-aisix-request-id"), hv);
    }
    Ok(ResponseDispatchSuccess {
        response,
        provider: provider_label,
        usage: Some(usage),
        model_id: model_id.to_string(),
        provider_key_id,
        upstream_model: model.upstream_model().unwrap_or("unknown").to_string(),
        routing: RoutingTelemetry::default(),
        guardrail_blocked: false,
        guardrail_block_message: String::new(),
        usage_handled_by_stream: false,
        output_redactions,
        output_monitor_hits,
        captured_content,
        cache: crate::response_cache::CacheTelemetry::default(),
    })
}

/// Pull the usage counters out of a Responses-API non-streaming
/// response body. Returns `None` only when:
///   - The `usage` block is missing entirely, OR
///   - `usage.input_tokens` is missing / non-numeric
///
/// The caller then estimates the counts locally (AISIX-Cloud#1074). The
/// `input_tokens` gate distinguishes "no upstream usage at all" from a
/// legitimate reply.
///
/// `output_tokens`, by contrast, defaults to 0 when absent: a 200 that
/// reports an input side but omits the output side is still a real
/// billable call and must be recorded. A missing completion/output side
/// coerces to 0 and the event is still logged/billed
/// (#429 follow-up; mirrors the tolerant wire-layer decode of
/// #474). Spec:
/// <https://platform.openai.com/docs/api-reference/responses/object>
fn extract_response_usage(body: &Value) -> Option<ResponseUsage> {
    let usage = body.get("usage")?;
    let prompt_tokens =
        crate::usage_attr::token_count(usage.get("input_tokens").and_then(|v| v.as_u64())?);
    let completion_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .map(crate::usage_attr::token_count)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .map(crate::usage_attr::token_count)
        .unwrap_or(0);
    let cached_prompt_tokens = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .map(crate::usage_attr::token_count)
        .unwrap_or(0);
    let cache_write_tokens = usage
        .pointer("/input_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .map(crate::usage_attr::token_count);
    let upstream_total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .map_or(0, crate::usage_attr::token_count);
    Some(ResponseUsage {
        // Parsed from a fully buffered response body, so by definition the
        // response was delivered in full.
        reached_end: true,
        prompt_tokens,
        completion_tokens,
        usage_estimated: false,
        reasoning_tokens,
        cached_prompt_tokens,
        cache_write_tokens,
        modality: aisix_gateway::ModalityTokens::from_openai_details(
            usage.get("input_tokens_details"),
            usage.get("output_tokens_details"),
        ),
        cache_creation_ttl: aisix_gateway::CacheCreationTtl::NONE,
        // OpenAI verbatim path: no Anthropic-style cache counters.
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        upstream_total_tokens,
        reasoning_folded_into_completion: 0,
        // Carried across by the caller (`drain_responses_sse_frames`), which
        // measured these before this terminal frame arrived.
        upstream_ttft_ms: 0,
        upstream_ttft: None,
        downstream_latency_ms: 0,
        // `resp_…` straight off the upstream's own response object
        // (#1289). On the streaming path the caller carries the
        // id it saw on an earlier frame across this replacement, so an
        // upstream that only stamps it on `response.created` still records.
        provider_request_id: crate::usage_attr::provider_response_id(body),
        service_tier: response_service_tier(body),
        failure: None,
    })
}

/// A Response object's top-level `service_tier` — the tier the upstream
/// actually served on, which can differ from the one requested.
fn response_service_tier(response: &Value) -> Option<String> {
    response
        .get("service_tier")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The served tier a terminal Responses stream event reports, whether or
/// not it carries usage. Only a terminal snapshot is read: the earlier ones
/// (`response.created`, `response.in_progress`) can still name the tier the
/// request asked for, such as `auto`.
pub(crate) fn terminal_response_service_tier(event: &Value) -> Option<String> {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("response.completed" | "response.incomplete" | "response.failed")
    )
    .then(|| event.get("response").and_then(response_service_tier))
    .flatten()
}

/// Pull usage out of one parsed Responses-API SSE event if it is a terminal
/// event that carries the authoritative `usage` block (#808). The full usage
/// rides `response.completed`, and **also `response.incomplete` /
/// `response.failed`** which fire on `max_output_tokens` truncation or
/// cancellation — billing those keeps streaming parity with non-streaming.
/// The `usage` lives under the nested `response` object (unlike the
/// non-streaming body where it is top-level), so the same `extract_response_usage`
/// gate is applied to `json.response`.
/// <https://platform.openai.com/docs/api-reference/responses-streaming>
fn parse_responses_terminal_usage(json: &Value) -> Option<ResponseUsage> {
    let event_type = json.get("type").and_then(|t| t.as_str());
    matches!(
        event_type,
        Some("response.completed" | "response.incomplete" | "response.failed")
    )
    .then(|| json.get("response").and_then(extract_response_usage))
    .flatten()
    .map(|mut usage| {
        // `extract_response_usage` handles fully buffered JSON and therefore
        // defaults `reached_end` to true. On SSE, `response.failed` is a
        // billed terminal failure, not a healthy semantic completion.
        usage.reached_end = matches!(
            event_type,
            Some("response.completed" | "response.incomplete")
        );
        usage
    })
}

impl ResponseUsage {
    fn usage_stats(&self) -> aisix_gateway::UsageStats {
        aisix_gateway::UsageStats {
            cached_prompt_tokens: self.cached_prompt_tokens,
            cache_write_tokens: self.cache_write_tokens,
            modality: self.modality,
            cache_creation_ttl: self.cache_creation_ttl,
            reasoning_tokens: self.reasoning_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            cache_read_tokens: self.cache_read_tokens,
            upstream_total_tokens: self.upstream_total_tokens,
            reasoning_folded_into_completion: self.reasoning_folded_into_completion,
            ..aisix_gateway::UsageStats::new(self.prompt_tokens, self.completion_tokens)
        }
    }
}

/// This endpoint's reading of a buffered Responses body's usage
/// ([`extract_response_usage`]), shared with passthrough routes relaying the
/// same envelope.
pub(crate) fn response_usage_stats(body: &Value) -> Option<aisix_gateway::UsageStats> {
    extract_response_usage(body).map(|u| u.usage_stats())
}

/// This endpoint's reading of one streamed Responses event's usage
/// ([`parse_responses_terminal_usage`]): only a terminal event reports it.
pub(crate) fn responses_terminal_usage_stats(event: &Value) -> Option<aisix_gateway::UsageStats> {
    parse_responses_terminal_usage(event).map(|u| u.usage_stats())
}

/// Scan a fully-buffered Responses-API SSE body for the terminal event's
/// usage block (#808). Used by the buffered output-guardrail path, which
/// already holds the whole response. Returns `None` (skip emission, matching
/// the non-streaming gate) when no terminal event carried a usage block.
fn responses_sse_usage(bytes: &[u8]) -> Option<ResponseUsage> {
    let mut usage: Option<ResponseUsage> = None;
    let mut failure = None;
    let mut tier = None;
    // Per frame, like the scan and the redaction pass (#1100): a terminal
    // event written over several `data:` lines parses only once they are
    // joined, and reading one line at a time would bill it from the token
    // estimator instead of the provider's own counters.
    for json in crate::redact::parse_sse_json_stream(bytes).0 {
        if let Some(u) = parse_responses_terminal_usage(&json) {
            usage = Some(u);
        }
        if let Some(t) = terminal_response_service_tier(&json) {
            tier = Some(t);
        }
        if let Some(err) = responses_in_band_error(&json) {
            crate::attempt::StreamFailure::record(&mut failure, &err);
        }
    }
    // A held-back response that failed in-band is sent as a 200 carrying the
    // failure event, like the live relay's; its usage records the failure.
    if failure.is_some() {
        usage.get_or_insert_with(Default::default).failure = failure;
    }
    if tier.is_some() {
        usage.get_or_insert_with(Default::default).service_tier = tier;
    }
    usage
}

fn responses_sse_terminal_failure(bytes: &[u8]) -> Option<crate::stream_timeout::RawStreamFailure> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let event_end = crate::redact::first_sse_event_end(remaining).unwrap_or(remaining.len());
        let frame = &remaining[..event_end];
        let (event, malformed) = crate::redact::parse_sse_json_event(frame);
        if malformed {
            return Some(crate::stream_timeout::RawStreamFailure::UpstreamDecode);
        }
        match event
            .as_ref()
            .and_then(|event| event.get("type"))
            .and_then(Value::as_str)
        {
            Some("response.completed" | "response.incomplete") => return None,
            Some("response.failed" | "error") => {
                return Some(crate::stream_timeout::RawStreamFailure::UpstreamInBand {
                    status: event.as_ref().and_then(responses_in_band_status),
                    wire: aisix_gateway::UpstreamWire::OpenAI,
                });
            }
            _ => {}
        }
        remaining = &remaining[event_end..];
    }
    Some(crate::stream_timeout::RawStreamFailure::Upstream)
}

fn responses_in_band_status(event: &Value) -> Option<u16> {
    event
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| event.get("error"))
        .and_then(|error| error.get("status").or_else(|| error.get("code")))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
}

/// The frames that end a held-back response on `failure`, as a bridged stream's
/// do. `delivered` is what goes out ahead of them. With nothing ahead they open
/// the stream themselves, since the OpenAI SDKs reject one that does not open
/// with `response.created`; otherwise they carry on its response id and
/// numbering.
fn responses_stream_error_body(
    failure: crate::stream_timeout::RawStreamFailure,
    reply: crate::responses_bridge::ResponsesReplyContext,
    requested_model: &str,
    mark: &crate::responses_affinity::ResponseMark,
    delivered: &[u8],
) -> Vec<u8> {
    let error = failure.bridge_error();
    let events = crate::redact::parse_sse_json_stream(delivered).0;
    let response_id = events
        .iter()
        .find_map(|event| event.pointer("/response/id").and_then(Value::as_str))
        .map_or_else(
            || format!("resp_{}", Uuid::new_v4().simple()),
            str::to_owned,
        );
    let mut encoder = crate::responses_bridge::ResponsesSseEncoder::new(
        mark.response_id(response_id),
        requested_model,
        chrono::Utc::now().timestamp(),
        reply,
    );
    encoder.continue_numbering_after(
        events
            .iter()
            .filter_map(|event| event.get("sequence_number").and_then(Value::as_u64))
            .max(),
    );
    match crate::platform_errors::stream_class(&error) {
        Some(class) => crate::responses_bridge::failure_frames(
            &mut encoder,
            !delivered.is_empty(),
            false,
            class.code(),
            crate::platform_errors::responses_failed_code(&class),
            crate::platform_errors::platform_message(&class),
        ),
        None => crate::responses_bridge::failure_frames(
            &mut encoder,
            !delivered.is_empty(),
            false,
            error.error_type(),
            crate::responses_bridge::upstream_failed_code(&error),
            "upstream stream failed",
        ),
    }
    .to_vec()
}

/// What a held-back response ends on when an output guardrail refuses it: the
/// frames the translated path sends for the same refusal.
fn responses_refusal_body(
    reply: crate::responses_bridge::ResponsesReplyContext,
    requested_model: &str,
    mark: &crate::responses_affinity::ResponseMark,
    guardrail_name: Option<&str>,
    unavailable: Option<&str>,
) -> bytes::Bytes {
    let mut encoder = crate::responses_bridge::ResponsesSseEncoder::new(
        mark.response_id(format!("resp_{}", Uuid::new_v4().simple())),
        requested_model,
        chrono::Utc::now().timestamp(),
        reply,
    );
    crate::responses_bridge::guardrail_failure_frames(
        &mut encoder,
        false,
        false,
        guardrail_name,
        unavailable,
    )
}

/// What the hold-back loop has read off a response's frames: enough to settle
/// the row of a response that never reached a verdict.
struct HeldResponse {
    usage: Option<ResponseUsage>,
    capture: SseTextCapture,
    /// The bytes as they came, for a response whose frames gave up no text.
    raw: String,
    /// How the upstream ended the response.
    failure: Option<crate::stream_timeout::RawStreamFailure>,
    upstream_model: String,
    outbound_body: bytes::Bytes,
}

impl HeldResponse {
    fn new(upstream_model: &str, outbound_body: &bytes::Bytes) -> Self {
        Self {
            usage: None,
            capture: SseTextCapture::new(crate::token_estimate::OUTPUT_ACCUMULATION_CAP),
            raw: String::new(),
            failure: None,
            upstream_model: upstream_model.to_string(),
            outbound_body: outbound_body.clone(),
        }
    }

    /// What the estimator reads for the output: the text the frames carried,
    /// else the bytes themselves.
    fn output_text(&self) -> String {
        let parsed = self.capture.text();
        if parsed.is_empty() {
            self.raw.clone()
        } else {
            parsed
        }
    }

    /// The usage of a response that went no further than this: what the frames
    /// reported, with the rest estimated.
    fn take_usage(&mut self) -> (ResponseUsage, String) {
        let output_text = self.output_text();
        let usage = held_refusal_usage(
            self.usage.take(),
            &self.upstream_model,
            &self.outbound_body,
            &output_text,
        );
        (usage, output_text)
    }
}

/// What a hold's verdict adds to the row, beyond what `on_complete` takes. Unset
/// on a hold that never reached one.
struct HeldResponseVerdict {
    refusal: Option<crate::attempt::StreamRefusal>,
    /// Per-detector counts of the output mask.
    redactions: crate::redact::RedactionCounts,
    captured: Option<CapturedContent>,
}

/// How a hold ended, once it has.
struct HeldResponseEnd {
    usage: ResponseUsage,
    out_text: String,
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    failure: Option<crate::stream_timeout::RawStreamFailure>,
}

/// Completes a hold's row however its stream ends. A caller who leaves before
/// the verdict received nothing and leaves a 499, with the upstream's counts as
/// far as the frames gave them and an estimate for the rest: the provider bills
/// the call either way.
struct HeldResponseGuard<F>
where
    F: FnOnce(
        ResponseUsage,
        String,
        Vec<aisix_core::GuardrailMonitorHit>,
        bool,
        Option<crate::stream_timeout::RawStreamFailure>,
    ),
{
    complete: Option<F>,
    observed: HeldResponse,
    end: Option<HeldResponseEnd>,
}

impl<F> Drop for HeldResponseGuard<F>
where
    F: FnOnce(
        ResponseUsage,
        String,
        Vec<aisix_core::GuardrailMonitorHit>,
        bool,
        Option<crate::stream_timeout::RawStreamFailure>,
    ),
{
    fn drop(&mut self) {
        let Some(complete) = self.complete.take() else {
            return;
        };
        let end = self.end.take().unwrap_or_else(|| {
            let (mut usage, out_text) = self.observed.take_usage();
            usage.reached_end = false;
            HeldResponseEnd {
                usage,
                out_text,
                monitor_hits: Vec::new(),
                failure: self.observed.failure,
            }
        });
        complete(
            end.usage,
            end.out_text,
            end.monitor_hits,
            false,
            end.failure,
        );
    }
}

/// The usage of a held-back response refused before its terminal event was
/// read: what the frames reported, with the rest estimated. The upstream
/// billed the request whatever became of the response.
fn held_refusal_usage(
    observed: Option<ResponseUsage>,
    upstream_model: &str,
    outbound_body: &[u8],
    output_text: &str,
) -> ResponseUsage {
    let mut usage = observed.unwrap_or_default();
    let est = crate::token_estimate::Estimator::new(
        upstream_model,
        crate::token_estimate::PromptInput::Responses(crate::util::outbound_value(outbound_body)),
    );
    let filled = crate::token_estimate::fill_missing(
        &est,
        usage.prompt_tokens,
        usage
            .cache_read_tokens
            .saturating_add(usage.cache_creation_tokens),
        usage.completion_tokens,
        Some(output_text),
    );
    if filled.estimated {
        usage.prompt_tokens = filled.prompt_tokens;
        usage.completion_tokens = filled.completion_tokens;
        usage.usage_estimated = true;
    }
    usage
}

/// [`crate::stream_timeout::RawStreamError::into_bridge`] for a relay that
/// still has the error to forward.
fn raw_stream_bridge_error(
    error: &crate::stream_timeout::RawStreamError,
    started: Instant,
) -> aisix_gateway::BridgeError {
    match error {
        crate::stream_timeout::RawStreamError::Timeout { elapsed_ms } => {
            aisix_gateway::BridgeError::Timeout {
                elapsed_ms: *elapsed_ms,
                cause: String::new(),
            }
        }
        crate::stream_timeout::RawStreamError::Upstream(e) => {
            crate::dispatch::reqwest_error_to_bridge(e, started)
        }
    }
}

fn responses_sse_has_done(bytes: &[u8]) -> bool {
    aisix_gateway::sse::lines(bytes).any(|line| {
        bytes[line.start..line.end]
            .strip_prefix(b"data:")
            .is_some_and(|data| data.trim_ascii() == b"[DONE]")
    })
}

/// The `[DONE]` frame a stream that ended without one is closed with, framed
/// like the frames before it: a relay keeps the upstream's own framing, so the
/// frame it adds must not bring another line ending into the stream.
fn responses_done_frame(frames: &[u8]) -> Vec<u8> {
    let mut rest = frames;
    let mut separator: &[u8] = b"\n\n";
    while let Some((content_end, frame_end)) = aisix_gateway::sse::split_frame_end(rest) {
        separator = &rest[content_end..frame_end];
        rest = &rest[frame_end..];
    }
    [b"data: [DONE]", separator].concat()
}

fn push_capped_lossy_bytes(output: &mut String, bytes: &[u8]) {
    let remaining = crate::token_estimate::OUTPUT_ACCUMULATION_CAP.saturating_sub(output.len());
    if remaining == 0 {
        return;
    }
    let prefix = &bytes[..bytes.len().min(remaining)];
    output.push_str(&String::from_utf8_lossy(prefix));
    if output.len() > crate::token_estimate::OUTPUT_ACCUMULATION_CAP {
        let mut end = crate::token_estimate::OUTPUT_ACCUMULATION_CAP;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
    }
}

/// The `resp_…` carried by any frame of a fully-buffered Responses-API SSE
/// body — `response.created` is the first, so this survives a stream whose
/// terminal frame reported no usage and therefore produced no
/// [`ResponseUsage`] (#1289).
fn responses_sse_provider_request_id(bytes: &[u8]) -> String {
    for json in crate::redact::parse_sse_json_stream(bytes).0 {
        if let Some(r) = json.get("response") {
            let id = crate::usage_attr::provider_response_id(r);
            if !id.is_empty() {
                return id;
            }
        }
    }
    String::new()
}

/// Drain every complete SSE event from `buf`, updating `acc` with the latest
/// terminal-event usage (#808) and feeding each parsed event to the optional
/// content capture (#947). A frame ends at the first blank line; an
/// incomplete trailing frame is left in `buf` for the next chunk. Framing and
/// multi-`data` interpretation use the shared WHATWG parser. Each frame is also
/// appended to `out` with the client-facing `model` restamped onto the snapshot
/// frames that carry one.
///
/// `out` is what the client receives, so the relay forwards whole frames
/// rather than raw chunks — a value can only be spliced once the frame
/// carrying it has arrived in full, and the Responses snapshot events are
/// spread across the whole stream (`response.created` first,
/// `response.completed` last). A frame is the SSE protocol's atomic unit, so
/// holding a partial one back is not observable to a conforming client;
/// `buf` retains only that partial tail.
///
/// Returns `true` at a semantic terminal, so callers can discard coalesced
/// later frames: those bytes are not part of this response.
#[allow(clippy::too_many_arguments)]
fn drain_responses_sse_frames(
    buf: &mut Vec<u8>,
    // How much of `buf` an earlier call scanned without finding a frame end.
    scanned: &mut usize,
    acc: &mut Option<ResponseUsage>,
    mut capture: Option<&mut SseTextCapture>,
    failure: &mut Option<crate::stream_timeout::RawStreamFailure>,
    attempt_started: Instant,
    first_frame_seen: &mut bool,
    client_facing_model: &str,
    mark: &crate::responses_affinity::ResponseMark,
    out: &mut Vec<u8>,
) -> bool {
    if failure.is_some() || acc.as_ref().is_some_and(|usage| usage.reached_end) {
        return true;
    }
    while let Some(end) = aisix_gateway::sse::find_frame_end_from(buf, *scanned) {
        let frame: Vec<u8> = buf.drain(..end).collect();
        *scanned = 0;
        // This site only READS the frame — the bytes forwarded to the client
        // are the ones below.
        let terminal = observe_responses_sse_frame(
            &frame,
            acc,
            capture.as_deref_mut(),
            failure,
            attempt_started,
            first_frame_seen,
        );
        // The caller-facing model and the context tags (and in platform mode
        // the sealed ids) in one splice pass.
        let tagged = mark.restamp_and_tag_sse_frame(&frame, client_facing_model);
        let forwarded = tagged.as_deref().unwrap_or(&frame);
        // A failure the upstream reported reaches the caller as sent, less the
        // credentials this request sent it; in platform mode, as the class of
        // the error it reports.
        if failure.is_some() {
            match crate::platform_errors::relayed_responses_failure(forwarded) {
                Some(class_frame) => out.extend_from_slice(&class_frame),
                None => {
                    out.extend_from_slice(&aisix_gateway::scrub_upstream_error_bytes(forwarded))
                }
            }
        } else {
            out.extend_from_slice(forwarded);
        }
        if terminal {
            return true;
        }
    }
    *scanned = buf.len();
    false
}

/// Read one complete SSE frame into a stream's accounting: the first frame's
/// time, the provider's id, the terminal usage, the failure it reports, and
/// the optional content capture. Returns `true` at a semantic terminal: a
/// successful `response.*` end, or a failure.
fn observe_responses_sse_frame(
    frame: &[u8],
    acc: &mut Option<ResponseUsage>,
    capture: Option<&mut SseTextCapture>,
    failure: &mut Option<crate::stream_timeout::RawStreamFailure>,
    attempt_started: Instant,
    first_frame_seen: &mut bool,
) -> bool {
    // Whole payload, not the first `data:` line (#1100): a frame whose
    // JSON is written over several `data:` lines parses only once they
    // are joined. Reading one line at a time leaves the terminal
    // `response.completed` unparseable, and the buffered path then
    // bills the request from the token estimator instead of the
    // provider's own counters.
    let (json, malformed) = crate::redact::parse_sse_json_event(frame);
    if malformed {
        let decode = crate::stream_timeout::RawStreamFailure::UpstreamDecode;
        crate::attempt::StreamFailure::record(
            &mut acc.get_or_insert_with(Default::default).failure,
            &decode.bridge_error(),
        );
        *failure = Some(decode);
    }
    if let Some(json) = json {
        // First parsed frame of ANY type (`response.created`
        // included) → upstream TTFT. The industry convention
        // (mainstream gateways) stamps the same event, so
        // the figure matches external observers. A generated-output
        // whitelist here reported the END of a silent thinking
        // phase on hidden-reasoning upstreams — #1225.
        if !*first_frame_seen {
            *first_frame_seen = true;
            let usage = acc.get_or_insert_with(Default::default);
            let ttft = attempt_started.elapsed();
            usage.upstream_ttft_ms = crate::attempt::measured_ms(ttft);
            usage.upstream_ttft = Some(ttft);
            crate::attribution::note_upstream_first_frame();
        }
        // `resp_…` off any frame that carries the response object —
        // `response.created` is the first, so a stream that dies
        // before its terminal frame still records which upstream call
        // it was (#1289).
        if let Some(id) = json
            .get("response")
            .and_then(|r| r.get("id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            acc.get_or_insert_with(Default::default).provider_request_id =
                crate::usage_attr::sanitize_provider_response_id(id);
        }
        if let Some(u) = parse_responses_terminal_usage(&json) {
            // The terminal frame replaces the token counters; carry
            // the latency figures and the id observed before it
            // across — an upstream that stamps the id only on
            // `response.created` would otherwise lose it here.
            let (ttft, ttft_precise, down, prev_id, prev_failure) = acc
                .as_ref()
                .map(|a| {
                    (
                        a.upstream_ttft_ms,
                        a.upstream_ttft,
                        a.downstream_latency_ms,
                        a.provider_request_id.clone(),
                        a.failure.clone(),
                    )
                })
                .unwrap_or_default();
            let provider_request_id = if u.provider_request_id.is_empty() {
                prev_id
            } else {
                u.provider_request_id.clone()
            };
            *acc = Some(ResponseUsage {
                upstream_ttft_ms: ttft,
                upstream_ttft: ttft_precise,
                downstream_latency_ms: down,
                provider_request_id,
                failure: prev_failure,
                ..u
            });
        }
        if let Some(tier) = terminal_response_service_tier(&json) {
            acc.get_or_insert_with(Default::default).service_tier = Some(tier);
        }
        match json.get("type").and_then(Value::as_str) {
            Some("response.completed" | "response.incomplete") => {
                acc.get_or_insert_with(Default::default).reached_end = true;
            }
            Some("response.failed" | "error") => {
                *failure = Some(crate::stream_timeout::RawStreamFailure::UpstreamInBand {
                    status: responses_in_band_status(&json),
                    wire: aisix_gateway::UpstreamWire::OpenAI,
                });
                if let Some(err) = responses_in_band_error(&json) {
                    crate::attempt::StreamFailure::record(
                        &mut acc.get_or_insert_with(Default::default).failure,
                        &err,
                    );
                }
            }
            _ => {}
        }
        if let Some(c) = capture {
            c.observe(&json);
        }
    }
    failure.is_some() || acc.as_ref().is_some_and(|usage| usage.reached_end)
}

/// The upstream failure a Responses-API stream event reports, if it is one:
/// an `error` event or `response.failed`, whose Response carries
/// `error: {code, message}`. The `error` event comes in two shapes — the
/// flat one of the published `ResponseErrorEvent` (`{type, code, message,
/// param}`, what openai-python parses and what this crate emits) and the
/// one the Codex CLI reads, which nests those fields under `error` — so the
/// nested value wins when present. Read the way an in-band error on any other wire
/// is, so a code that names no HTTP status maps as one with none.
pub(crate) fn responses_in_band_error(event: &Value) -> Option<aisix_gateway::BridgeError> {
    let error = match event.get("type").and_then(Value::as_str)? {
        "error" => match event.get("error") {
            Some(nested) if !nested.is_null() => nested.clone(),
            _ => serde_json::json!({
                "message": event.get("message"),
                "code": event.get("code"),
                "param": event.get("param"),
            }),
        },
        "response.failed" => event
            .pointer("/response/error")
            .cloned()
            .unwrap_or(Value::Null),
        _ => return None,
    };
    Some(
        aisix_gateway::capture_in_band_error_value(
            &serde_json::json!({ "error": error }),
            aisix_gateway::UpstreamWire::OpenAI,
        )
        .unwrap_or_else(|| aisix_gateway::BridgeError::UpstreamInBand {
            status: None,
            message: "upstream reported the response failed".to_string(),
            parsed: None,
            wire: aisix_gateway::UpstreamWire::OpenAI,
        }),
    )
}

/// The failure a buffered 2xx Responses body reports in place of an answer:
/// a bare error envelope. A response object is the answer even when its own
/// `error` is set (`status: "failed"`) and is relayed as sent.
pub(crate) fn responses_body_in_band_error(body: &Value) -> Option<aisix_gateway::BridgeError> {
    if body.get("object").and_then(Value::as_str) == Some("response") {
        return None;
    }
    aisix_gateway::capture_in_band_error_value(body, aisix_gateway::UpstreamWire::OpenAI)
}

/// Streamed output-text accumulator for content-capturing exporters
/// (#947). Mirrors `responses_sse_output_text`'s precedence: a
/// terminal `response.*` event's full output (incl. tool-call items) wins;
/// concatenated `*.delta` text is the fallback for streams that abort before
/// a terminal object. Delta accumulation is bounded to the capture cap so a
/// long stream can't grow the buffer without limit.
struct SseTextCapture {
    cap: usize,
    deltas: String,
    terminal: Option<String>,
    truncated: bool,
    /// The terminal `response` object, kept while its text fits the cap:
    /// the end-of-stream scan rebuilds from it the slots the buffered
    /// branch's mask walker would rewrite (#1027).
    terminal_response: Option<Value>,
    /// A standalone visible event occurred before (or without) a terminal
    /// snapshot. Its text must join the monitor scan even when the terminal
    /// response is present, because an upstream can send a mismatching
    /// authoritative aggregate after clean deltas.
    saw_non_terminal_visible: bool,
}

impl SseTextCapture {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            deltas: String::new(),
            terminal: None,
            truncated: false,
            terminal_response: None,
            saw_non_terminal_visible: false,
        }
    }

    /// The local-kind segments of the terminal response, when it was kept.
    fn terminal_segments(&self) -> Option<Vec<aisix_guardrails::ScanSegment>> {
        if self.saw_non_terminal_visible {
            return None;
        }
        let mut resp = self.terminal_response.clone()?;
        Some(crate::redact::collect_segments(|g| {
            let _ = crate::redact::redact_responses_response_structured(g, &mut resp);
        }))
    }

    /// Feed one parsed SSE event's JSON.
    fn observe(&mut self, json: &Value) {
        match json.get("type").and_then(|t| t.as_str()) {
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                if let Some(resp) = json.get("response") {
                    let (full, truncated) = responses_output_content_text_capped(resp, self.cap);
                    self.terminal_response =
                        (!truncated && full.len() <= self.cap).then(|| resp.clone());
                    if !full.is_empty() {
                        self.terminal = Some(full);
                        self.truncated = truncated;
                    }
                }
            }
            _ => {
                let text = responses_stream_event_capture_text(json);
                if !text.is_empty() {
                    self.saw_non_terminal_visible = true;
                    // Pieces of one string join as they came; a whole carrier
                    // starts on its own line.
                    if !responses_event_is_delta(json) && !self.deltas.is_empty() {
                        self.truncated |=
                            crate::token_estimate::push_capped_to(&mut self.deltas, "\n", self.cap);
                    }
                    self.truncated |=
                        crate::token_estimate::push_capped_to(&mut self.deltas, &text, self.cap);
                }
            }
        }
    }

    /// The captured output text: terminal full output when seen, else the
    /// accumulated deltas. `CapturedContent::new` re-truncates to the cap.
    fn into_text(self) -> String {
        self.terminal.unwrap_or(self.deltas)
    }

    /// [`Self::into_text`] without consuming it.
    fn text(&self) -> String {
        self.terminal.clone().unwrap_or_else(|| self.deltas.clone())
    }

    /// All authoritative output carriers for the end-of-stream monitor.
    /// Capture retains its terminal preference, but a monitor must observe a
    /// visible standalone `.done`/item/part event even if the terminal
    /// snapshot later disagrees with it.
    fn scan_text(&self) -> String {
        let mut text = self.deltas.clone();
        if let Some(terminal) = &self.terminal {
            if !text.is_empty() && !terminal.is_empty() {
                text.push('\n');
            }
            text.push_str(terminal);
        }
        text
    }
}

/// Drop guard that fires `on_complete` exactly once with the usage parsed from
/// the stream's terminal SSE event — on normal end-of-stream AND on
/// client-disconnect (the async-stream generator drops at its suspension
/// point), mirroring the `/v1/messages` and chat.rs CompleteOnDrop pattern.
/// `None` means no terminal usage was seen (e.g. an abort before completion);
/// the emit then records a zero-token 200 so the request still appears in the
/// dashboard Logs. The second callback argument is the captured output text
/// (#947) — empty when no exporter wants content. The third is the
/// end-of-stream scan's monitor observations (#1010) — the Drop
/// (disconnect) path passes none: the response never completed, so there is
/// nothing final to observe, matching the chat surface's disconnect behavior.
struct ResponsesStreamState {
    usage: Option<ResponseUsage>,
    capture: Option<SseTextCapture>,
    capture_safe: bool,
    failure: Option<crate::stream_timeout::RawStreamFailure>,
}

struct ResponsesUsageGuard<
    F: FnOnce(
        ResponseUsage,
        String,
        Vec<aisix_core::GuardrailMonitorHit>,
        bool,
        Option<crate::stream_timeout::RawStreamFailure>,
    ),
> {
    slot: Option<(F, ResponsesStreamState)>,
}

impl<
        F: FnOnce(
            ResponseUsage,
            String,
            Vec<aisix_core::GuardrailMonitorHit>,
            bool,
            Option<crate::stream_timeout::RawStreamFailure>,
        ),
    > ResponsesUsageGuard<F>
{
    fn parts(&mut self) -> (&mut Option<ResponseUsage>, Option<&mut SseTextCapture>) {
        let state = &mut self
            .slot
            .as_mut()
            .expect("ResponsesUsageGuard accessed after take")
            .1;
        (&mut state.usage, state.capture.as_mut())
    }

    fn state(&mut self) -> &mut ResponsesStreamState {
        &mut self
            .slot
            .as_mut()
            .expect("ResponsesUsageGuard accessed after take")
            .1
    }
}

impl<
        F: FnOnce(
            ResponseUsage,
            String,
            Vec<aisix_core::GuardrailMonitorHit>,
            bool,
            Option<crate::stream_timeout::RawStreamFailure>,
        ),
    > Drop for ResponsesUsageGuard<F>
{
    fn drop(&mut self) {
        if let Some((f, state)) = self.slot.take() {
            f(
                state.usage.unwrap_or_default(),
                state
                    .capture
                    .map(SseTextCapture::into_text)
                    .unwrap_or_default(),
                Vec::new(),
                state.capture_safe,
                state.failure,
            );
        }
    }
}

/// The body of a response a block-capable output chain holds back. The head has
/// gone out by the time this runs and heartbeats keep the connection warm, so a
/// long hold survives whatever sits in front of the gateway; what the hold can
/// no longer do is answer with a status. The response is scanned once it is
/// complete, then released, or replaced by failure frames, which is what a
/// refusal is on every held stream. Past the chain's cap the hold follows its
/// overflow policy: refuse, or hand what it has to the live relay unscanned.
#[allow(clippy::too_many_arguments)]
fn build_held_responses_stream<F>(
    upstream: RawResponsesByteStream,
    chain: Arc<aisix_guardrails::GuardrailChain>,
    started: Instant,
    attempt_started: Instant,
    content_cap: Option<u32>,
    // `None` when the request's own text may not be exported.
    captured_prompt: Option<String>,
    reply: crate::responses_bridge::ResponsesReplyContext,
    requested_model: String,
    upstream_model: String,
    body_bytes: bytes::Bytes,
    model_label: String,
    mark: crate::responses_affinity::ResponseMark,
    verdict_out: Arc<std::sync::OnceLock<HeldResponseVerdict>>,
    on_complete: F,
) -> impl futures::Stream<Item = Result<bytes::Bytes, crate::stream_timeout::RawStreamError>> + Send
where
    F: FnOnce(
            ResponseUsage,
            String,
            Vec<aisix_core::GuardrailMonitorHit>,
            bool,
            Option<crate::stream_timeout::RawStreamFailure>,
        ) + Send
        + 'static,
{
    crate::request_id::in_request_span(async_stream::stream! {
        let mut guard = HeldResponseGuard {
            complete: Some(on_complete),
            observed: HeldResponse::new(&upstream_model, &body_bytes),
            end: None,
        };
        // Hold the whole SSE response back to scan it, but cap what is
        // held so a huge (or malicious) upstream response can't OOM the
        // gateway. Mirror the chat surface's BufferFull policy (#466):
        // count the model-generated content as it arrives (#513) and,
        // past the cap, fail closed — or, under fail-open, release it
        // unscanned. The cap and the overflow policy are the chain's
        // folded `max_buffer_bytes` / `on_buffer_exceeded`, whether it
        // resolved to a window or a whole-response hold.
        let (max_buffer_bytes, on_exceeded_fail_open) =
            aisix_guardrails::Guardrail::stream_output_policy(chain.as_ref())
                .hold_cap()
                .unwrap_or((aisix_guardrails::DEFAULT_STREAM_OUTPUT_BUFFER_BYTES, false));
        let mut upstream = upstream;
        let mut buf: Vec<u8> = Vec::new();
        // Held until the verdict: the scan reads `buf`.
        let mut buf_held = crate::held_content::HeldBytes::default();
        let mut observed_first_frame = false;
        // What `buf` holds (#513): content in `buf[..counted_upto]`,
        // which `max_buffer_bytes` caps (the SSE/JSON envelope is not
        // counted), and every raw byte, unterminated tail included.
        let mut held_content = crate::held_content::HeldBuffer::default();
        // A Responses stream repeats one logical output carrier in
        // delta, done, part, item, and terminal events. Keep that
        // bounded identity ledger across the entire held response.
        let mut responses_held_content = crate::held_content::ResponsesHeldContent::default();
        let mut counted_upto = 0usize;
        let mut parsed_upto = 0usize;
        // How much of `buf[parsed_upto..]` is known to hold no frame end.
        let mut scanned = 0usize;
        let mut exceeded = false;
        while let Some(chunk) = upstream.next().await {
            let observed = &mut guard.observed;
            let chunk = match chunk {
                Ok(chunk) => chunk,
                // The response's terminal failure, settled with the rest below.
                Err(crate::stream_timeout::RawStreamError::Timeout { elapsed_ms }) => {
                    observed.failure =
                        Some(crate::stream_timeout::RawStreamFailure::Timeout { elapsed_ms });
                    break;
                }
                Err(crate::stream_timeout::RawStreamError::Upstream(error)) => {
                    let error = aisix_gateway::upstream_body_read_error(&error);
                    crate::attempt::StreamFailure::record(
                        &mut observed.usage.get_or_insert_with(Default::default).failure,
                        &error,
                    );
                    observed.failure =
                        Some(crate::stream_timeout::RawStreamFailure::UpstreamDecode);
                    break;
                }
            };
            push_capped_lossy_bytes(&mut observed.raw, &chunk);
            buf.extend_from_slice(&chunk);
            // Parse/account through the exact semantic-terminal boundary.
            // A provider may coalesce later frames into the same transport
            // item; those bytes are not part of this response.
            let mut semantic_terminal = false;
            while let Some(end) =
                aisix_gateway::sse::find_frame_end_from(&buf[parsed_upto..], scanned)
            {
                let frame_end = parsed_upto + end;
                scanned = 0;
                semantic_terminal = observe_responses_sse_frame(
                    &buf[parsed_upto..frame_end],
                    &mut observed.usage,
                    Some(&mut observed.capture),
                    &mut observed.failure,
                    attempt_started,
                    &mut observed_first_frame,
                );
                parsed_upto = frame_end;
                if semantic_terminal {
                    buf.truncate(parsed_upto);
                    break;
                }
            }
            scanned = buf.len() - parsed_upto;
            buf_held.set(buf.len());
            held_content.hold(
                crate::held_content::responses_sse_held_frames(
                    &mut responses_held_content,
                    &buf[counted_upto..parsed_upto],
                ),
                chunk.len(),
            );
            counted_upto = parsed_upto;
            if held_content.exceeds(max_buffer_bytes) {
                exceeded = true;
                break;
            }
            if semantic_terminal {
                break;
            }
        }
        // A final frame the upstream never terminated is held content too.
        if !exceeded && counted_upto < buf.len() {
            held_content.hold(
                crate::held_content::responses_sse_held_frames(
                    &mut responses_held_content,
                    &buf[counted_upto..],
                ),
                0,
            );
            exceeded = held_content.exceeds(max_buffer_bytes);
        }
        if exceeded && on_exceeded_fail_open {
            crate::guardrail_stream::release_output_buffer_unscanned(chain.as_ref());
            // 扣留期间边读边量到的上游首帧时刻。放行后的流会把缓冲前缀重新解析
            // 一遍，那时“看到”首帧已经是放行时刻了。
            let released_ttft = guard.observed.usage.as_ref().and_then(|u| u.upstream_ttft);
            let Some(complete) = guard.complete.take() else {
                return;
            };
            buf_held.clear();
            let prefix = bytes::Bytes::from(std::mem::take(&mut buf));
            let rest: RawResponsesByteStream = Box::pin(
                futures::stream::once(std::future::ready(Ok(prefix))).chain(upstream),
            );
            let live = build_responses_passthrough_stream(
                rest,
                started,
                attempt_started,
                released_ttft,
                content_cap,
                None,
                // What goes out unscanned never reaches an exporter.
                false,
                requested_model,
                mark,
                complete,
            );
            futures::pin_mut!(live);
            while let Some(item) = live.next().await {
                yield item;
            }
            return;
        }
        if exceeded {
            tracing::warn!(
                guardrail_name = crate::guardrail_stream::output_buffer_owner(chain.as_ref()),
                guardrail_hook = "output",
                model = %model_label,
                max_buffer_bytes,
                "streaming /v1/responses output exceeded buffer cap; failing closed",
            );
            aisix_guardrails::Guardrail::record_output_buffer_exceeded(chain.as_ref());
            // The upstream ran and bills for this response even though the
            // caller gets none of it, and the terminal usage frame was
            // never reached: count what arrived rather than record zero.
            let (mut usage, out_text) = guard.observed.take_usage();
            usage.reached_end = true;
            usage.failure = None;
            usage.downstream_latency_ms = crate::attempt::measured_ms(started.elapsed());
            let _ = verdict_out.set(HeldResponseVerdict {
                refusal: Some(crate::attempt::StreamRefusal::new(
                    None,
                    Some(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED),
                )),
                redactions: crate::redact::RedactionCounts::new(),
                captured: match (&captured_prompt, content_cap) {
                    (Some(prompt), Some(cap)) => {
                        Some(CapturedContent::new(prompt, "", cap as usize))
                    }
                    _ => None,
                },
            });
            guard.end = Some(HeldResponseEnd {
                usage,
                out_text,
                monitor_hits: Vec::new(),
                failure: None,
            });
            yield Ok(responses_refusal_body(
                reply,
                &requested_model,
                &mark,
                None,
                Some(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED),
            ));
            return;
        }
        let observed_failure = guard.observed.failure;
        if observed_failure.is_none()
            && guard
                .observed
                .usage
                .as_ref()
                .is_some_and(|usage| usage.reached_end)
            && !responses_sse_has_done(&buf)
        {
            let done = responses_done_frame(&buf);
            buf.extend_from_slice(&done);
        }
        // #1091/#1100: the two passes below read this buffer
        // differently — the line-based scan reads `data:` lines it can
        // parse, the frame-based redactor reads terminator-delimited
        // frames whose payload parses. Seal it first, so both read the
        // same frames and no frame reaches the client whose payload
        // neither could parse: a final frame missing only its terminator
        // gets it, and a frame whose payload is not one JSON document is
        // cut (its text still goes to the block scan below). Parsing is
        // the whole test — see `seal_buffered_sse` for what that leaves.
        let seal = crate::redact::seal_buffered_sse(&mut buf);
        if let crate::redact::SseTailSeal::Dropped { dropped } = seal.tail {
            tracing::warn!(
                guardrail_hook = "output",
                model = %model_label,
                dropped,
                "streaming /v1/responses ended on an SSE frame that could not be \
                 parsed; dropping it rather than releasing it past the output \
                 guardrail",
            );
        }
        if !seal.excised.is_empty() {
            tracing::warn!(
                guardrail_hook = "output",
                model = %model_label,
                frames = seal.excised.len(),
                dropped = seal.excised_bytes,
                "streaming /v1/responses carried SSE frames whose payload is not one \
                 JSON document; dropping them rather than releasing them past the \
                 output guardrail (their text is still scanned)",
            );
        }
        // A terminated frame that does not parse failed the read loop above.
        // A tail the upstream never finished is a truncated response, read
        // here with that tail already gone.
        let terminal_failure = observed_failure.or_else(|| responses_sse_terminal_failure(&buf));
        // Nothing else arrived: the whole response was frames that
        // could not be scanned. Dropping them silently would hand
        // the caller an empty answer with no signal, and there is no
        // scanned content to release — refuse, the same shape the
        // buffer-cap arm above and `/v1/messages` use. A buffer that
        // was empty to begin with is an empty upstream response and
        // is left alone.
        if buf.is_empty() && seal.cut_anything() {
            let mut usage = held_refusal_usage(
                guard.observed.usage.take(),
                &upstream_model,
                &body_bytes,
                &seal.excised.join("\n"),
            );
            usage.reached_end = true;
            usage.failure = None;
            usage.downstream_latency_ms = crate::attempt::measured_ms(started.elapsed());
            let _ = verdict_out.set(HeldResponseVerdict {
                refusal: Some(crate::attempt::StreamRefusal::new(
                    None,
                    Some(crate::error::TAG_UNSCANNABLE_BODY),
                )),
                redactions: crate::redact::RedactionCounts::new(),
                captured: match (&captured_prompt, content_cap) {
                    (Some(prompt), Some(cap)) => {
                        Some(CapturedContent::new(prompt, "", cap as usize))
                    }
                    _ => None,
                },
            });
            guard.end = Some(HeldResponseEnd {
                usage,
                out_text: seal.excised.join("\n"),
                monitor_hits: Vec::new(),
                failure: terminal_failure,
            });
            yield Ok(responses_refusal_body(
                reply,
                &requested_model,
                &mark,
                None,
                Some(crate::error::TAG_UNSCANNABLE_BODY),
            ));
            return;
        }
        // #808: the whole SSE response is buffered here, so parse its
        // terminal event for usage. Do this before the guardrail verdict so a
        // billed-then-blocked response keeps both its token usage and its
        // first-frame latency.
        //
        // Token-estimation fallback (#1074): a buffered
        // stream with zero/missing usage fills the counters locally —
        // telemetry only, the buffered bytes forward untouched.
        let mut usage = responses_sse_usage(&buf).unwrap_or_default();
        // 首帧时刻是上面边读边缓冲时量的，事后重新解析缓冲区得不到；
        // 不带过来的话，扣留路径的 TTFT 就整个丢了。
        if let Some(observed) = guard.observed.usage.as_ref() {
            usage.upstream_ttft_ms = observed.upstream_ttft_ms;
            usage.upstream_ttft = observed.upstream_ttft;
            if usage.failure.is_none() {
                usage.failure.clone_from(&observed.failure);
            }
        }
        // A held-back response that ended on a failure records that
        // failure, whichever way the upstream reported it.
        if let Some(failure) = terminal_failure {
            crate::attempt::StreamFailure::record(&mut usage.failure, &failure.bridge_error());
        }
        // The usage gate is independent of the id: a stream whose
        // terminal frame reported no usage still names the upstream
        // call it was (#1289).
        if usage.provider_request_id.is_empty() {
            usage.provider_request_id = responses_sse_provider_request_id(&buf);
        }
        let out_text_estimate = responses_sse_output_text(&buf);
        if usage.prompt_tokens == 0 || usage.completion_tokens == 0 {
            let est = crate::token_estimate::Estimator::new(
                &upstream_model,
                crate::token_estimate::PromptInput::Responses(crate::util::outbound_value(
                    &body_bytes,
                )),
            );
            let filled = crate::token_estimate::fill_missing(
                &est,
                usage.prompt_tokens,
                usage
                    .cache_read_tokens
                    .saturating_add(usage.cache_creation_tokens),
                usage.completion_tokens,
                Some(&out_text_estimate),
            );
            if filled.estimated {
                usage.prompt_tokens = filled.prompt_tokens;
                usage.completion_tokens = filled.completion_tokens;
                usage.usage_estimated = true;
            }
        }
        // The upstream's part of this is over, whatever the verdict says.
        usage.reached_end = true;
        let mut out_text = responses_sse_guardrail_text(&buf);
        // #1100: an excised frame is not released, but a forbidden
        // literal inside it must still block the response — the block
        // pass reads raw text, so it can scan a payload nothing could
        // parse.
        for payload in &seal.excised {
            if !out_text.is_empty() {
                out_text.push('\n');
            }
            out_text.push_str(payload);
        }
        let synth = synth_chat_response(&upstream_model, out_text);
        let (verdict, hits) =
            aisix_guardrails::Guardrail::check_output_non_segment_observed(chain.as_ref(), &synth)
                .await;
        let mut output_monitor_hits = hits;
        // Segment pass over the held SSE frames: one Bedrock call; an
        // ANONYMIZE disposition rewrites `buf` in place (#932 bedrock
        // follow-up). The capture below reads the post-mask buffer.
        let mut output_redactions = crate::redact::RedactionCounts::new();
        let unreleased = seal
            .excised
            .iter()
            .map(|text| aisix_guardrails::ScanSegment {
                text: text.clone(),
                role: aisix_guardrails::SegmentRole::ScanOnly,
                in_latest_turn: true,
            })
            .collect();
        let moderation = crate::redact::moderate_body_local_extra(
            chain.as_ref(),
            crate::redact::Direction::Output,
            None,
            verdict,
            &mut output_redactions,
            &mut output_monitor_hits,
            unreleased,
            |g| crate::redact::redact_responses_sse_structured(g, &buf).apply_to(&mut buf),
        )
        .await;
        // The response has passed a complete moderation cycle only if the
        // upstream finished it and every provider call answered.
        let response_capture_safe = moderation.capture_safe && terminal_failure.is_none();
        let mut verdict = moderation.verdict;
        // #932: the whole SSE response is held here — mask the frames
        // (channel reassembly) before anything reaches the wire. A mask
        // that would have to rewrite a tool's structural field cannot be
        // applied, so it refuses instead.
        if !verdict.is_block() {
            let redaction = crate::redact::redact_responses_sse_structured(chain.as_ref(), &buf);
            if let Some(rewritten) = redaction.rewritten {
                buf = rewritten;
            }
            crate::redact::merge_counts(&mut output_redactions, redaction.counts);
            if redaction.unrewritable_tool_key {
                verdict = crate::redact::unrewritable_tool_key_verdict();
            }
        }
        usage.downstream_latency_ms = crate::attempt::measured_ms(started.elapsed());
        if let aisix_guardrails::GuardrailVerdict::Block {
            reason,
            guardrail_name,
            unavailable,
        } = verdict
        {
            // Per #153 the matched-pattern detail stays in ops logs only.
            tracing::warn!(
                guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
                guardrail_hook = "output",
                model = %model_label,
                reason = %reason,
                "guardrail blocked streaming /v1/responses response",
            );
            // The upstream completed and billed this request, so the row
            // carries the usage. 被拦下的正文不进采集：拦了又归档等于没拦。
            let _ = verdict_out.set(HeldResponseVerdict {
                refusal: Some(crate::attempt::StreamRefusal::new(
                    guardrail_name.as_deref(),
                    unavailable.as_deref(),
                )),
                redactions: crate::redact::RedactionCounts::new(),
                captured: match (&captured_prompt, content_cap) {
                    (Some(prompt), Some(cap)) => {
                        Some(CapturedContent::new(prompt, "", cap as usize))
                    }
                    _ => None,
                },
            });
            guard.end = Some(HeldResponseEnd {
                usage,
                out_text: out_text_estimate,
                monitor_hits: output_monitor_hits,
                failure: terminal_failure,
            });
            yield Ok(responses_refusal_body(
                reply,
                &requested_model,
                &mark,
                guardrail_name.as_deref(),
                unavailable.as_deref(),
            ));
            return;
        }
        // A block-capable output guardrail buffers the whole response and
        // releases it here, never reaching the live relay that splices
        // frame-by-frame — so the same pass runs once over the buffer.
        // Without it, attaching such a guardrail would silently change
        // which model name the caller is told (after masking, so a mask
        // cannot reintroduce the upstream id).
        let buf = crate::model_echo::restamp_sse_buffer(
            &buf,
            &requested_model,
            crate::model_echo::responses_snapshot_model,
        );
        // A failure the upstream reported is released as it sent it, less the
        // credentials this request sent, as the live relay releases it; in
        // platform mode, as the class of the error it reports.
        let buf = if matches!(
            terminal_failure,
            Some(crate::stream_timeout::RawStreamFailure::UpstreamInBand { .. })
        ) {
            let buf = crate::platform_errors::relayed_responses_buffer(&buf).unwrap_or(buf);
            aisix_gateway::scrub_upstream_error_bytes(&buf).into_owned()
        } else {
            buf
        };
        // Content capture (#947): the assembled output text,
        // read from the POST-redaction buffer so masked PII stays masked
        // in the exported content.
        let captured_content = match (&captured_prompt, content_cap) {
            (Some(prompt), Some(cap)) if response_capture_safe => Some(
                CapturedContent::new(prompt, &responses_sse_output_text(&buf), cap as usize)
                    .with_output(Some({
                        let mut c = aisix_obs::OutputCollector::new(
                            aisix_obs::OutputWire::OpenAiResponses,
                            cap as usize,
                        );
                        c.feed_sse(&buf);
                        c.finish()
                    })),
            ),
            _ => None,
        };
        use crate::stream_timeout::RawStreamFailure;
        let released = match terminal_failure {
            // The read failed, or a frame could not be understood: what
            // was held is not sent alongside the gateway's terminal
            // error. Its usage is kept for billing.
            Some(
                failure @ (RawStreamFailure::Timeout { .. } | RawStreamFailure::UpstreamDecode),
            ) => responses_stream_error_body(failure, reply, &requested_model, &mark, &[]),
            other => {
                let mut finish = aisix_obs::ResponsesFinishObserver::default();
                finish.feed_sse(&buf);
                crate::attribution::note_responses_finish(finish.finish());
                // The upstream ended without a terminal event: the scanned
                // frames go out, and the stream closes on the failure. A
                // failure the upstream declared is already in them.
                let closing = other
                    .filter(|failure| matches!(failure, RawStreamFailure::Upstream))
                    .map(|failure| {
                        responses_stream_error_body(failure, reply, &requested_model, &mark, &buf)
                    });
                // After every telemetry read above, which records the
                // upstream's own ids.
                let mut released = mark.apply_to_sse_buffer(buf);
                if let Some(closing) = closing {
                    released.extend_from_slice(&closing);
                }
                released
            }
        };
        let _ = verdict_out.set(HeldResponseVerdict {
            refusal: None,
            redactions: output_redactions,
            captured: captured_content,
        });
        guard.end = Some(HeldResponseEnd {
            usage,
            out_text: out_text_estimate,
            monitor_hits: output_monitor_hits,
            failure: terminal_failure,
        });
        buf_held.clear();
        yield Ok(bytes::Bytes::from(released));
    })
}

/// Wrap a Responses-API upstream byte stream so the terminal event's usage is
/// parsed in-flight and `on_complete` fires once at end-of-stream (or
/// client-disconnect) with the accumulated counts (#808) plus the captured
/// output text (#947, empty when `content_cap` is `None`), the
/// end-of-stream scan's monitor hits (#1010, empty without
/// `eos_scan`), whether the captured text may be exported, and the upstream
/// failure that ended the stream. Bytes forward unchanged apart from the
/// caller-facing `model` on the snapshot frames — the client sees the exact
/// upstream
/// SSE wire shape.
#[allow(clippy::too_many_arguments)]
fn build_responses_passthrough_stream<S, F>(
    upstream: S,
    // Request clock — what the CALLER waited for.
    started: Instant,
    // Attempt clock — how the UPSTREAM behaved.
    attempt_started: Instant,
    // 首帧已在别处量过（扣留缓冲 fail-open 放行）：报这个时刻，不按本流
    // 重新解析到首帧的时刻重新计。
    measured_ttft: Option<Duration>,
    content_cap: Option<u32>,
    eos_scan: Option<EosOutputScan>,
    initial_capture_safe: bool,
    // The model name the caller addressed, restamped onto the snapshot
    // frames so a streamed response echoes the alias like a buffered one.
    client_facing_model: String,
    // Context tags applied to the forwarded frames only; the usage and
    // capture parsers read each frame as the upstream sent it.
    mark: crate::responses_affinity::ResponseMark,
    on_complete: F,
) -> impl futures::Stream<Item = Result<bytes::Bytes, crate::stream_timeout::RawStreamError>>
where
    S: futures::Stream<Item = Result<bytes::Bytes, crate::stream_timeout::RawStreamError>>
        + Send
        + 'static,
    F: FnOnce(
            ResponseUsage,
            String,
            Vec<aisix_core::GuardrailMonitorHit>,
            bool,
            Option<crate::stream_timeout::RawStreamFailure>,
        ) + Send
        + 'static,
{
    // The scan and the token-estimation fallback read the same assembled
    // output text the capture produces, so the accumulator is now ALWAYS
    // on (#1074) — whether estimation is needed is only known
    // at end-of-stream — with the estimation cap as the floor. The cap
    // bounds delta accumulation; a terminal event's full output text is
    // instead bounded by MAX_SSE_FRAME_BYTES (an oversized frame
    // never parses). The end-of-stream scan reads all of it, so with one
    // the accumulator keeps up to the scan's own bound;
    // `CapturedContent::new` re-truncates to the exporter cap either way.
    let capture_cap = Some(match &eos_scan {
        Some(scan) => scan.text_cap(),
        None => content_cap
            .map(|cap| cap as usize)
            .unwrap_or(0)
            .max(crate::token_estimate::OUTPUT_ACCUMULATION_CAP),
    });
    // Only a scan that was owed can make a partial capture unsafe. An unguarded
    // stream's export is what the caller received, cut at the exporter's cap
    // like any other.
    let scan_owed = eos_scan.is_some();
    // Re-attach the request span: the body is polled after the request-id
    // middleware returns, so the end-of-stream output-guardrail scan
    // (`EosOutputScan::observe`) would otherwise log without a
    // `request_id` (#1060).
    crate::request_id::in_request_span(async_stream::stream! {
        let mut guard = ResponsesUsageGuard {
            slot: Some((
                on_complete,
                ResponsesStreamState {
                    usage: measured_ttft.map(|ttft| ResponseUsage {
                        upstream_ttft_ms: crate::attempt::measured_ms(ttft),
                        upstream_ttft: Some(ttft),
                        ..Default::default()
                    }),
                    capture: capture_cap.map(SseTextCapture::new),
                    // A live output-guarded response is uninspected until its
                    // end-of-stream provider call completes.
                    capture_safe: initial_capture_safe,
                    failure: None,
                },
            )),
        };
        futures::pin_mut!(upstream);
        let mut buf: Vec<u8> = Vec::new();
        let mut scanned = 0usize;
        // Already measured when `upstream` replays bytes held back earlier.
        let mut first_frame_seen = measured_ttft.is_some();
        // A frame that outgrew the buffer went out unparsed, so whether a
        // terminal event was among those bytes can no longer be told.
        let mut released_unparsed = false;
        while let Some(item) = upstream.next().await {
            let bytes = match item {
                Ok(bytes) => bytes,
                // A mid-stream Err is forwarded as-is; in platform mode the
                // stream ends on its class instead, as a bridged channel's
                // stream does for the same fault.
                Err(error) => {
                    let bridge = raw_stream_bridge_error(&error, attempt_started);
                    let state = guard.state();
                    crate::attempt::StreamFailure::record(
                        &mut state.usage.get_or_insert_with(Default::default).failure,
                        &bridge,
                    );
                    state.failure = Some(error.failure());
                    if let Some(frame) = crate::platform_errors::responses_stream_failure(&bridge) {
                        yield Ok(bytes::Bytes::from(frame));
                        return;
                    }
                    yield Err(error);
                    return;
                }
            };
            // Accumulate, then drain every COMPLETE frame — restamped with
            // the caller's model name — into `forward`. The client receives
            // whole frames, never a partial one; `buf` keeps the trailing
            // remainder until its terminator arrives.
            buf.extend_from_slice(&bytes);
            let mut forward: Vec<u8> = Vec::new();
            let state = guard.state();
            let terminal_seen = drain_responses_sse_frames(
                &mut buf,
                &mut scanned,
                &mut state.usage,
                state.capture.as_mut(),
                &mut state.failure,
                attempt_started,
                &mut first_frame_seen,
                &client_facing_model,
                &mark,
                &mut forward,
            );
            if terminal_seen {
                // Parsed through the exact semantic-terminal boundary. Later
                // frames coalesced into this transport item are discarded.
                buf.clear();
            } else if buf.len() > aisix_gateway::sse::MAX_SSE_FRAME_BYTES {
                // Bound the frame buffer: the happy path drains complete frames
                // above so `buf` only holds a partial trailing frame. A
                // non-conformant upstream streaming bytes without a blank-line
                // terminator would otherwise grow `buf` unboundedly; release the
                // un-terminated remainder downstream rather than OOM. Delivery is
                // preserved — only this frame's usage parse and model restamp
                // are lost, and its text never reached the capture, so the
                // capture no longer speaks for the whole response.
                //
                // The bound is the one every SSE reader shares, generous on
                // purpose: `response.completed` carries the whole Response
                // object, image results included, and it is the only frame that
                // reports the usage. A frame that does not fit has stopped being
                // one: at the cap its JSON is incomplete, so `json_splice`
                // refuses it and falls back to verbatim by design.
                tracing::warn!(
                    buffered = buf.len(),
                    "responses stream: SSE frame buffer exceeded cap without a \
                     terminator; releasing it unparsed (usage parsing and model \
                     restamp skipped)"
                );
                forward.append(&mut buf);
                scanned = 0;
                if scan_owed {
                    state.capture_safe = false;
                }
                released_unparsed = true;
            }
            if scan_owed && state.capture.as_ref().is_some_and(|capture| capture.truncated) {
                state.capture_safe = false;
            }
            // Nothing completed yet — keep reading rather than yielding an
            // empty chunk.
            if forward.is_empty() {
                if terminal_seen {
                    break;
                }
                continue;
            }
            let successful_terminal = state
                .usage
                .as_ref()
                .is_some_and(|usage| usage.reached_end);
            let closing_done = (terminal_seen
                && successful_terminal
                && !responses_sse_has_done(&forward))
            .then(|| responses_done_frame(&forward));
            // The first forward is what the caller waited for.
            let acc = state.usage.get_or_insert_with(Default::default);
            if acc.downstream_latency_ms == 0 {
                acc.downstream_latency_ms =
                    crate::attempt::measured_ms(started.elapsed());
            }
            yield Ok(bytes::Bytes::from(forward));
            if terminal_seen {
                if let Some(done) = closing_done {
                    yield Ok(bytes::Bytes::from(done));
                }
                break;
            }
        }
        // A non-conformant upstream can end without terminating its last
        // frame. Those bytes were never forwarded (they are still the partial
        // tail), so release them now rather than truncating the response.
        //
        // Restamp it on the way out. Mid-stream a fragment is a frame still
        // arriving and must be held, but the upstream has now ended: this is
        // a final frame it never terminated, and on this surface that is
        // `response.completed` — the event an SDK builds its final Response
        // object from. Same reasoning as `model_echo::restamp_sse_buffer`,
        // and this path reaches it without any guardrail attached.
        if !buf.is_empty() {
            // Parse it before releasing it. On this surface the unterminated
            // final frame is `response.completed`, which carries the
            // authoritative token counts and `response.id`: forwarding it to
            // the caller while never reading it handed them real usage in the
            // body and recorded estimated counts, with no provider request id,
            // in the UsageEvent. Completing the terminator the upstream left
            // off lets the normal drain read it; the drain consumes `buf`, and
            // stripping back exactly what was added hands the client its own
            // bytes.
            //
            // Pad by what is MISSING, not a fixed `\n\n`: a tail already
            // ending in one `\n` needs one more, and padding two would make
            // `find_frame_end` end the frame one byte early, so the strip
            // below would eat the newline the upstream actually sent.
            let pad: &[u8] = if buf.ends_with(b"\n") { b"\n" } else { b"\n\n" };
            buf.extend_from_slice(pad);
            let mut drained: Vec<u8> = Vec::new();
            let state = guard.state();
            drain_responses_sse_frames(
                &mut buf,
                &mut 0,
                &mut state.usage,
                state.capture.as_mut(),
                &mut state.failure,
                attempt_started,
                &mut first_frame_seen,
                &client_facing_model,
                &mark,
                &mut drained,
            );
            let tail = match drained.len().checked_sub(pad.len()) {
                // Drop only the bytes this branch appended.
                Some(n) if drained.ends_with(pad) => drained[..n].to_vec(),
                _ => drained,
            };
            let acc = state.usage.get_or_insert_with(Default::default);
            if acc.downstream_latency_ms == 0 {
                acc.downstream_latency_ms =
                    crate::attempt::measured_ms(started.elapsed());
            }
            yield Ok(bytes::Bytes::from(tail));
        }
        // A clean socket EOF is successful only after a semantic terminal
        // event. Missing `response.completed`/`response.incomplete` is an
        // upstream truncation; `response.failed` has already stored an
        // in-band failure while preserving its billed usage. Recorded before
        // the scan below, which awaits a remote provider and is a routine
        // drop point for clients that close on the terminal frame.
        {
            let state = guard.state();
            let usage = state.usage.get_or_insert_with(Default::default);
            if !usage.reached_end && state.failure.is_none() {
                if released_unparsed {
                    // Its terminal event may have gone out unread, and the
                    // upstream ended on its own: the response was delivered.
                    usage.reached_end = true;
                } else {
                    let truncated = crate::stream_timeout::RawStreamFailure::Upstream;
                    crate::attempt::StreamFailure::record(
                        &mut usage.failure,
                        &truncated.bridge_error(),
                    );
                    state.failure = Some(truncated);
                }
            }
        }
        // Clean end-of-stream: run the monitor observation (needs async, so
        // it can't live in the Drop guard), then complete explicitly. The
        // scan awaits a remote guardrail provider, and SDK clients routinely
        // close the connection right after the terminal frame — dropping
        // this generator at that await. The guard therefore MUST stay armed
        // across the scan: its Drop then still emits the usage event (with
        // the captured text, without hits — same as any disconnect). Taking
        // the slot before the await would silently lose the event for a
        // fully-delivered stream.
        let hits = match eos_scan {
            Some(scan) => {
                let (usage, capture) = guard.parts();
                let failed = usage.as_ref().is_some_and(|u| u.failure.is_some());
                let text = capture
                    .as_ref()
                    .map(|c| c.scan_text())
                    .unwrap_or_default();
                if capture.as_ref().is_some_and(|c| c.truncated) {
                    scan.record_outgrown();
                }
                let segments = capture.and_then(|c| c.terminal_segments());
                // A failed stream with no text is not a response to judge.
                if text.is_empty() && failed {
                    Vec::new()
                } else {
                    let hits = scan.observe(&text, segments).await;
                    // The scan has now read the whole response, which is what
                    // the capture was waiting on.
                    if !failed && !released_unparsed {
                        guard.state().capture_safe = true;
                    }
                    hits
                }
            }
            None => Vec::new(),
        };
        if let Some((f, state)) = guard.slot.take() {
            let capture_safe = state.capture_safe
                && !(scan_owed && state.capture.as_ref().is_some_and(|capture| capture.truncated));
            f(
                state.usage.unwrap_or_default(),
                state
                    .capture
                    .map(SseTextCapture::into_text)
                    .unwrap_or_default(),
                hits,
                capture_safe,
                state.failure,
            );
        }
    })
}

/// Collect the assistant's output text from a Responses-API response object
/// for output-guardrail scanning (#719/#546):
/// - the `text` of every `output_text` content part of message items, and
/// - the `name` + `arguments` (function calls) / `input` (custom tool calls)
///   of tool-call items — these are **top-level** item fields, not under
///   `content[]`, so without scanning them a blocked literal placed in a
///   tool-call's arguments would bypass the output guardrail. The chat
///   surface scans tool-call output too (`ChatResponse::guardrail_output_text`,
///   the #448 fix); this keeps the surfaces symmetric.
///
/// Reasoning items are excluded (out of output-guardrail scope, matching
/// the chat surface). That used to be left to the shape — the comment here
/// said they carry `summary`, not `content`, so they are naturally
/// skipped — but a reasoning item DOES carry `content[]` with `text`
/// parts, and the walk below reads `content` off every item regardless of
/// type, so generated reasoning was reaching the output scan. The skip is
/// explicit now.
/// <https://platform.openai.com/docs/api-reference/responses/object>
pub(crate) fn responses_output_text(resp: &Value) -> String {
    responses_output_text_capped(resp, usize::MAX).0
}

fn responses_output_text_capped(resp: &Value, cap: usize) -> (String, bool) {
    let Some(items) = resp.get("output").and_then(|v| v.as_array()) else {
        return (String::new(), false);
    };
    let mut output = String::new();
    let mut truncated = false;
    for it in items {
        let (item_text, item_truncated) = responses_output_item_text_capped(it, cap);
        if item_text.is_empty() {
            continue;
        }
        if !output.is_empty() {
            truncated |= crate::token_estimate::push_capped_to(&mut output, "\n", cap);
        }
        truncated |= crate::token_estimate::push_capped_to(&mut output, &item_text, cap);
        truncated |= item_truncated;
    }
    (output, truncated)
}

fn responses_output_content_text_capped(resp: &Value, cap: usize) -> (String, bool) {
    let Some(items) = resp.get("output").and_then(|v| v.as_array()) else {
        return (String::new(), false);
    };
    let mut output = String::new();
    let mut truncated = false;
    for item in items {
        let (item_text, item_truncated) = responses_output_item_content_text_capped(item, cap);
        if item_text.is_empty() {
            continue;
        }
        if !output.is_empty() {
            truncated |= crate::token_estimate::push_capped_to(&mut output, "\n", cap);
        }
        truncated |= crate::token_estimate::push_capped_to(&mut output, &item_text, cap);
        truncated |= item_truncated;
    }
    (output, truncated)
}

/// The client-visible text on one Responses output item, for the output
/// guardrails. This is shared by terminal `response.*` snapshots and standalone
/// `response.output_item.*` stream events so either wire form receives the same
/// output protection. Generated reasoning is out of scope.
fn responses_output_item_text(item: &Value) -> String {
    responses_output_item_text_capped(item, usize::MAX).0
}

fn responses_output_item_text_capped(item: &Value, cap: usize) -> (String, bool) {
    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
        return (String::new(), false);
    }
    responses_item_scan_text_capped(item, cap)
}

/// The same item for content capture and token estimates, without protocol
/// object-key names.
fn responses_output_item_content_text_capped(item: &Value, cap: usize) -> (String, bool) {
    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
        return (String::new(), false);
    }
    crate::redact::responses_item_content_text_capped(item, cap)
}

/// One client-visible Responses message part. A refusal has its own field,
/// unlike the ordinary text part types.
fn responses_visible_part_text(part: &Value) -> Option<&str> {
    match part.get("type").and_then(Value::as_str) {
        Some("output_text" | "text" | "input_text") => part.get("text").and_then(Value::as_str),
        Some("refusal") => part.get("refusal").and_then(Value::as_str),
        _ => None,
    }
}

/// Client-visible text from one official Responses streaming event. A
/// standalone aggregate is authoritative: providers may omit deltas and end
/// after this frame, so it cannot rely on a later terminal snapshot for
/// output enforcement.
fn responses_stream_event_text(event: &Value) -> String {
    responses_stream_carrier_text(event, responses_output_item_text)
}

/// [`responses_stream_event_text`] as content capture reads it: an output item
/// without protocol object-key names.
fn responses_stream_event_capture_text(event: &Value) -> String {
    responses_stream_carrier_text(event, |item| {
        responses_output_item_content_text_capped(item, usize::MAX).0
    })
}

/// Whether a stream event carries a piece of one string, as opposed to a
/// whole aggregate.
fn responses_event_is_delta(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some(
            "response.output_text.delta"
                | "response.refusal.delta"
                | "response.function_call_arguments.delta"
                | "response.mcp_call_arguments.delta"
                | "response.custom_tool_call_input.delta"
        )
    )
}

fn responses_stream_carrier_text(event: &Value, item_text: fn(&Value) -> String) -> String {
    let fields = |fields: &[&str]| {
        fields
            .iter()
            .filter_map(|field| event.get(*field).and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    match event.get("type").and_then(Value::as_str) {
        Some(
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.function_call_arguments.delta"
            | "response.mcp_call_arguments.delta"
            | "response.custom_tool_call_input.delta",
        ) => fields(&["delta"]),
        Some("response.output_text.done") => fields(&["text"]),
        Some("response.refusal.done") => fields(&["refusal"]),
        Some("response.function_call_arguments.done" | "response.mcp_call_arguments.done") => {
            fields(&["name", "arguments"])
        }
        Some("response.custom_tool_call_input.done") => fields(&["name", "input"]),
        Some("response.content_part.added" | "response.content_part.done") => event
            .get("part")
            .and_then(responses_visible_part_text)
            .unwrap_or_default()
            .to_owned(),
        Some("response.output_item.added" | "response.output_item.done") => {
            event.get("item").map(item_text).unwrap_or_default()
        }
        Some("response.completed" | "response.incomplete" | "response.failed") => event
            .get("response")
            .map(responses_output_text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Collect the assistant's streamed output text from a buffered
/// Responses-API SSE response (#719/#546). Prefers the authoritative full
/// output carried on a terminal `response` event — `response.completed`,
/// **and also `response.incomplete` / `response.failed`**, which carry the
/// same full `output[]` (incl. tool-call items) and fire routinely (e.g.
/// `max_output_tokens` truncation). Falls back to concatenating the streamed
/// deltas when no terminal `response` object is present (truncated/aborted):
/// both `response.output_text.delta` / `response.refusal.delta` (assistant
/// text or refusal) and
/// `response.function_call_arguments.delta` (tool-call args stream via their
/// own event, NOT output_text) — otherwise blocked tool-call args would leak
/// on a stream that never reaches a terminal object. The `type` field on each
/// `data:` JSON line drives the dispatch.
/// <https://platform.openai.com/docs/api-reference/responses-streaming>
fn responses_sse_output_text(bytes: &[u8]) -> String {
    let mut deltas = String::new();
    for json in crate::redact::parse_sse_json_stream(bytes).0 {
        match json.get("type").and_then(|t| t.as_str()) {
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                if let Some(resp) = json.get("response") {
                    let full = responses_output_text(resp);
                    if !full.is_empty() {
                        return full;
                    }
                }
            }
            Some("response.output_text.delta" | "response.refusal.delta") => {
                if let Some(d) = json.get("delta").and_then(|d| d.as_str()) {
                    deltas.push_str(d);
                }
            }
            // Tool-call argument deltas across all tool kinds — function calls,
            // MCP tool calls, and custom tools each stream their args/input via
            // their own event, not output_text.delta. On a terminal `response`
            // object these are already covered by responses_output_text; this
            // matters only when the stream aborts before any terminal object.
            // Concatenate WITHOUT a separator — these are pieces of one call's
            // string; a separator would split a literal that streamed across
            // two deltas (e.g. "BLOCK"+"ME") and miss the match.
            Some(
                "response.function_call_arguments.delta"
                | "response.mcp_call_arguments.delta"
                | "response.custom_tool_call_input.delta",
            ) => {
                if let Some(d) = json.get("delta").and_then(|d| d.as_str()) {
                    deltas.push_str(d);
                }
            }
            Some("response.output_item.added" | "response.output_item.done") => {
                if let Some(item) = json.get("item") {
                    let text = responses_item_text(item);
                    if !text.is_empty() {
                        if !deltas.is_empty() {
                            deltas.push('\n');
                        }
                        deltas.push_str(&text);
                    }
                }
            }
            _ => {}
        }
    }
    deltas
}

/// The text the buffered output guardrail evaluates. Unlike content capture
/// and token estimation, it deliberately retains every authoritative stream
/// carrier: a clean delta followed by a malicious `.done`, item, part, or
/// terminal snapshot must still block rather than being hidden by terminal
/// precedence.
fn responses_sse_guardrail_text(bytes: &[u8]) -> String {
    let mut text = String::new();
    let mut previous_was_delta = false;
    for event in crate::redact::parse_sse_json_stream(bytes).0 {
        let is_delta = responses_event_is_delta(&event);
        let event_text = responses_stream_event_text(&event);
        if event_text.is_empty() {
            continue;
        }
        if !(text.is_empty() || previous_was_delta && is_delta) {
            text.push('\n');
        }
        text.push_str(&event_text);
        previous_was_delta = is_delta;
    }
    text
}

/// Copy the upstream `content-type` onto the client response, a fixed one in
/// platform mode ([`crate::platform::relayed_content_type`]), and stamp the
/// `x-aisix-request-id` header. Shared by the streaming verbatim-passthrough
/// and buffered hold-back paths, both of which relay an event stream.
fn apply_passthrough_headers(
    response: &mut Response,
    upstream_headers: &axum::http::HeaderMap,
    request_id: &str,
    platform: Option<&crate::platform::PlatformSettings>,
) {
    if let Some(hv) = crate::platform::relayed_content_type(
        platform,
        upstream_headers,
        crate::platform::SSE_CONTENT_TYPE,
    ) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, hv);
    }
    // Platform mode: the header a bridged stream sends, so the two kinds of
    // channel answer under one header set.
    if platform.is_some() {
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        );
    }
    if let Ok(hv) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-aisix-request-id"), hv);
    }
}

/// Issue #404: push one `UsageEvent` onto the control plane's telemetry sink
/// and fan it out to per-env OTLP exporters. Mirrors the shape of
/// `embeddings::emit_usage_event` (#402) for the fields that matter
/// to /v1/responses, with one extension: `reasoning_tokens` is
/// surfaced for o1/o3/GPT-5 class models. `inbound_protocol` is
/// `"openai"` — the Responses API is OpenAI-shaped on the wire even when
/// the resolved model is bridged to a non-OpenAI provider (#825).
///
/// Other fields left at `UsageEvent::default()`:
///   - cache_creation_tokens / cache_read_tokens — populated only on the
///     #825 cross-provider bridge path (Anthropic backends); 0 otherwise
///   - provider_model_version — not yet plumbed for non-chat handlers
///     (follow-up). `finish_reason` is derived from the client-facing
///     result at the emission chokepoint (`usage_attr::emit_usage`).
///   - cache_status / cache_hit_* / ttft_ms — no caching/streaming
///     surface on Responses API non-streaming
///   - served_by_model / routing_* — Responses doesn't run routing
///
/// `provider_kind` / `provider_featured` / `branded_provider` / `pk_label` /
/// `byo_label` are populated from the resolved target's ProviderKey
/// `telemetry_tags` (#867) — same lookup as `/v1/messages` and
/// `/v1/chat/completions`, so Codex (`/v1/responses`) logs carry the upstream
/// vendor + PK label the dashboard's Logs detail shows. Empty `provider_key_id`
/// (pre-dispatch error) bypasses the lookup → wire NULL.
#[allow(clippy::too_many_arguments)]
fn emit_usage_event(
    state: &ProxyState,
    // The request's snapshot, resolved by the caller (#941). The row
    // lookup stays here because each event names its OWN attempt's key,
    // but it is now ONE lookup feeding both the wire attribution tags and
    // the `provider_key_name` metric label.
    snap: &aisix_core::AisixSnapshot,
    // Resolved by the caller so the winning attempt's row is read ONCE for
    // both this event and the handler's `record` (#941 audit L2). The
    // stream-end callers resolve their own against a fresh snapshot.
    pk: &ResolvedPk<'_>,
    request_id: &str,
    model_id: &str,
    requested_model: &str,
    metric_model: &str,
    caller: crate::request_metrics::Caller<'_>,
    // Metric labels the UsageEvent has no field for (#1234
    // follow-up): the wire struct is the CP contract, so they ride
    // alongside rather than in it.
    provider: &str,
    upstream_model: &str,
    status_code: u16,
    elapsed: Duration,
    usage: &ResponseUsage,
    // Whether a cache policy answered this request, and what a hit saved.
    cache: crate::response_cache::CacheTelemetry,
    client: &ClientContext,
    attempt: AttemptInfo,
    guardrail_blocked: bool,
    // Per-detector PII mask counts (#932), input + output merged. Detector
    // names only, never matched values. Empty = no redaction.
    redacted_entity_counts: crate::redact::RedactionCounts,
    // Monitor-mode guardrail observations (#562), input +
    // output merged.
    guardrail_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    // Captured request/response content for content-capturing exporters
    // (#947). Forwarded only to `fan_out`, never to the CP sink.
    content: Option<&CapturedContent>,
    // Whether this event ends the request (AISIX-Cloud#1279).
    terminal: bool,
    // Whether the work this event describes actually reached an upstream —
    // false for a pre-dispatch failure, whose `elapsed` is handler time.
    dispatched: bool,
    // The request's enforced-guardrail audit handle (AISIX-Cloud#1330).
    audit: &crate::usage_attr::GuardrailAudit,
) {
    let tags = pk.telemetry_tags();
    let mut event = UsageEvent {
        request_id: request_id.to_string(),
        occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
        model_id: model_id.to_string(),
        api_key_id: caller.api_key_id.to_string(),
        requested_model: requested_model.to_string(),
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: aisix_gateway::chat::recorded_completion_tokens(
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.reasoning_tokens,
            usage.reasoning_folded_into_completion,
            usage.upstream_total_tokens,
        ),
        total_tokens: usage.upstream_total_tokens,
        cache_status: cache.status.as_str().to_string(),
        cache_hit_layer: cache.layer(),
        cache_hit_saved_input_tokens: cache.saved_input_tokens,
        cache_hit_saved_output_tokens: cache.saved_output_tokens,
        // Priced from the dispatched row's `Model.cost` when the operator set
        // one, `0.0` otherwise — see `usage_attr::request_cost_usd`.
        cost_usd: cache.cost_usd(
            snap,
            model_id,
            // `/v1/responses` carries BOTH vocabularies: the OpenAI subset
            // counter on the verbatim path, the Anthropic separate counters on
            // the cross-provider bridge (#825). The splitter handles either.
            crate::usage_attr::input_tokens_for_pricing(
                u64::from(usage.prompt_tokens),
                u64::from(usage.cached_prompt_tokens),
                u64::from(usage.cache_write_tokens.unwrap_or(0)),
                u64::from(usage.cache_read_tokens),
                u64::from(usage.cache_creation_tokens),
            ),
            u64::from(usage.completion_tokens),
        ),
        cached_prompt_tokens: usage.cached_prompt_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        modality_tokens: usage.modality,
        cache_creation_ttl: usage.cache_creation_ttl,
        reasoning_tokens: usage.reasoning_tokens,
        // Anthropic cache counters (#825 cross-provider path); 0 on the
        // verbatim OpenAI path.
        cache_creation_tokens: usage.cache_creation_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        usage_estimated: usage.usage_estimated,
        upstream_latency_ms: crate::attempt::measured_ms(elapsed),
        upstream_ttft_ms: usage.upstream_ttft_ms,
        downstream_latency_ms: usage.downstream_latency_ms,
        status_code,
        provider_request_id: usage.provider_request_id.clone(),
        service_tier: usage.service_tier.clone(),
        inbound_protocol: "openai".to_string(),
        attempt_index: attempt.index,
        attempt_kind: attempt.kind,
        attempt_model: attempt.model,
        error_class: attempt.error_class,
        error_message: attempt.error_message,
        provider_kind: sanitize_tag(tags.kind.map(|k| k.as_str().to_owned()).unwrap_or_default()),
        provider_featured: tags.featured,
        branded_provider: sanitize_tag(tags.branded_provider.unwrap_or_default()),
        pk_label: sanitize_tag(tags.pk_label.unwrap_or_default()),
        byo_label: sanitize_tag(tags.byo_label.unwrap_or_default()),
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        request_headers: client.request_headers.clone(),
        guardrail_blocked,
        redacted_entity_counts,
        guardrail_monitor_hits,
        // See `emit_zero_token_event`: request-scoped, so terminal only.
        applied_guardrails: crate::usage_attr::applied_guardrails(audit),
        guardrail_enforced_hits: crate::usage_attr::terminal_enforced_hits(terminal, audit),
        guardrail_scores: crate::usage_attr::terminal_guardrail_scores(terminal, audit),
        guardrail_bypassed_reason: crate::usage_attr::bypass_reason(audit),
        ..Default::default()
    };
    event.reasoning_unfolded_from_completion = usage
        .completion_tokens
        .saturating_sub(event.completion_tokens);
    crate::usage_attr::apply_caller_identity(&mut event, client.jwt.as_ref(), &client.caller);
    let usage_model = crate::usage_attr::usage_event_model_label(snap, &event.requested_model);
    crate::usage_attr::emit_usage(
        state,
        snap,
        crate::operation::RESPONSES,
        event.clone(),
        crate::usage_attr::usage_event_labels(&usage_model, pk),
        content,
        client.trace.as_ref(),
        terminal,
        dispatched,
    );
    // #1044: token volume by inbound client type × model. Codex
    // traffic arrives on /v1/responses, so leaving this endpoint out of the
    // by-client series made an allowlisted client invisible in it. All three
    // usage-bearing paths (non-streaming, verbatim streaming, bridge
    // streaming) funnel through here. `requested_model` resolved at dispatch
    // on every path that reaches this emit, so the label is bounded by the
    // configured model set. The shared call below also keeps the per-key
    // `aisix_llm_*_tokens_total` families on the same path.
    // #1002: cache-inclusive total via the shared helper — cache counters are
    // non-zero only on the #825 Anthropic bridge path.
    let total_all = total_tokens_with_cache(
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.cache_creation_tokens,
        usage.cache_read_tokens,
    );
    crate::request_metrics::record_usage(
        state,
        "/v1/responses",
        caller,
        crate::request_metrics::Upstream {
            provider,
            model: metric_model,
            upstream_model,
            pk: pk.labels(),
            ..Default::default()
        },
        crate::request_metrics::Tokens {
            input: usage.prompt_tokens,
            output: usage.completion_tokens,
            total: total_all.min(u64::from(u32::MAX)) as u32,
            cached: usage.cached_prompt_tokens,
            cache_read: usage.cache_read_tokens,
            cache_creation: usage.cache_creation_tokens,
            spend_usd: event.cost_usd,
            client_type: state.client_classifier.classify(&client.user_agent),
        },
    );
    // 两条 TTFT 序列。`Some` 当且仅当流式路径真的看到了上游首帧（1ms 内的也算），
    // 非流式恒为 `None`，所以这里只会记流式。原生透传、桥接、扣留缓冲三条流式
    // 收尾都从这个函数出去，记在这里就不会有哪条漏掉。
    //
    // 按哪个模型记：请求方点名的那一行（路由组时是父模型），和同一个流的 e2e
    // 直方图一致；经快照收敛成配置里的名字，调用方随手写的字符串进不了标签。
    if usage.upstream_ttft.is_some() {
        let (bounded_model, bounded_upstream) =
            crate::usage_attr::metric_model_label_pair(snap, metric_model, upstream_model);
        state.metrics.record_ttft(
            LatencyLabels {
                endpoint: "/v1/responses",
                model: bounded_model.as_ref(),
                provider,
                status: status_code,
                streaming: true,
                details: UsageLabels {
                    endpoint: "/v1/responses",
                    inbound_protocol: "openai",
                    upstream_protocol: pk.labels().protocol(),
                    provider,
                    model: bounded_model.as_ref(),
                    upstream_model: bounded_upstream.as_ref(),
                    provider_key_id: pk.labels().id(),
                    provider_key_name: pk.labels().name(),
                    api_key_id: caller.api_key_id,
                    api_key_name: caller.api_key_name,
                    team_id: caller.team_id,
                    team_name: caller.team_name,
                    user_id: caller.user_id,
                    user_name: caller.user_name,
                },
            },
            usage.upstream_ttft,
            // 0 is "nothing reached the caller yet"; a measured wait reads as
            // at least 1.
            (usage.downstream_latency_ms > 0)
                .then(|| Duration::from_millis(u64::from(usage.downstream_latency_ms))),
        );
    }
}

/// Emit a zero-token `UsageEvent` for a failed / pre-dispatch attempt
/// (#655). Tokens stay 0; `status_code` + `error_*` carry the failure.
#[allow(clippy::too_many_arguments)]
fn emit_zero_token_event(
    state: &ProxyState,
    snap: &aisix_core::AisixSnapshot,
    request_id: &str,
    model_id: &str,
    requested_model: &str,
    api_key_id: &str,
    provider_key_id: &str,
    status_code: u16,
    elapsed: Duration,
    client: &ClientContext,
    attempt: AttemptInfo,
    // Whether the request ended in a guardrail refusal, from
    // [`ProxyError::is_guardrail_block`]. Request-scoped like the enforced
    // hits below, so it lands on the terminal event only
    // (AISIX-Cloud#1428).
    guardrail_blocked: bool,
    // Per-detector PII mask counts (#932): input masking may have fired
    // before the failure. Empty for most failure classes.
    redacted_entity_counts: crate::redact::RedactionCounts,
    // Monitor-mode guardrail observations (#562) that fired
    // before the failure.
    guardrail_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    // #1013: captured (post-mask) request body for failed
    // requests. Forwarded only to `fan_out`, never to the CP sink.
    content: Option<CapturedContent>,
    // Whether this event ends the request (AISIX-Cloud#1279): true for a
    // pre-dispatch / all-failed terminal, false for a superseded attempt.
    terminal: bool,
    // Whether this attempt actually reached an upstream — false on the
    // pre-dispatch path, true for a recorded failed attempt.
    dispatched: bool,
    // The request's enforced-guardrail audit handle (AISIX-Cloud#1330).
    audit: &crate::usage_attr::GuardrailAudit,
) {
    let pk = ResolvedPk::resolve(snap, provider_key_id);
    let tags = pk.telemetry_tags();
    let mut event = UsageEvent {
        request_id: request_id.to_string(),
        occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
        model_id: model_id.to_string(),
        api_key_id: api_key_id.to_string(),
        requested_model: requested_model.to_string(),
        redacted_entity_counts,
        guardrail_monitor_hits,
        upstream_latency_ms: crate::attempt::measured_ms(elapsed),
        status_code,
        inbound_protocol: "openai".to_string(),
        attempt_index: attempt.index,
        attempt_kind: attempt.kind,
        attempt_model: attempt.model,
        error_class: attempt.error_class,
        error_message: attempt.error_message,
        provider_kind: sanitize_tag(tags.kind.map(|k| k.as_str().to_owned()).unwrap_or_default()),
        provider_featured: tags.featured,
        branded_provider: sanitize_tag(tags.branded_provider.unwrap_or_default()),
        pk_label: sanitize_tag(tags.pk_label.unwrap_or_default()),
        byo_label: sanitize_tag(tags.byo_label.unwrap_or_default()),
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        request_headers: client.request_headers.clone(),
        // Guardrails run once per REQUEST, not once per attempt, so a
        // superseded attempt's event would repeat the same hit per retry.
        // Only the terminal event carries them.
        applied_guardrails: crate::usage_attr::applied_guardrails(audit),
        guardrail_enforced_hits: crate::usage_attr::terminal_enforced_hits(terminal, audit),
        guardrail_scores: crate::usage_attr::terminal_guardrail_scores(terminal, audit),
        guardrail_bypassed_reason: crate::usage_attr::bypass_reason(audit),
        // Same rule, same reason as the hits above.
        guardrail_blocked: terminal && guardrail_blocked,
        ..Default::default()
    };
    crate::usage_attr::apply_caller_identity(&mut event, client.jwt.as_ref(), &client.caller);
    let usage_model = crate::usage_attr::usage_event_model_label(snap, &event.requested_model);
    crate::usage_attr::emit_usage(
        state,
        snap,
        crate::operation::RESPONSES,
        event.clone(),
        crate::usage_attr::usage_event_labels(&usage_model, &pk),
        content.as_ref(),
        client.trace.as_ref(),
        terminal,
        dispatched,
    );
}

/// Emit one zero-token `UsageEvent` per FAILED attempt of a `/v1/responses`
/// request (#655). The winner / terminal event is emitted separately.
#[allow(clippy::too_many_arguments)]
fn emit_failed_attempts(
    state: &ProxyState,
    snap: &aisix_core::AisixSnapshot,
    request_id: &str,
    requested_model: &str,
    api_key_id: &str,
    client: &ClientContext,
    routing: &RoutingTelemetry,
    // AISIX-Cloud#1279: on the all-failed path the LAST failed attempt's
    // event is the request's terminal emission, so it carries the trace's
    // SERVER + logical spans. False on the success path.
    terminal_last: bool,
    // Whether the request ended in a guardrail refusal (AISIX-Cloud#1428).
    // Rides the same event as the audit handle below, for the same reason.
    guardrail_blocked: bool,
    // The request's enforced-guardrail audit handle (AISIX-Cloud#1330);
    // stamped only on the event this call marks terminal.
    audit: &crate::usage_attr::GuardrailAudit,
) {
    let last_failed = routing.attempts.iter().rposition(|a| !a.success);
    for (i, rec) in routing
        .attempts
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.success)
    {
        emit_zero_token_event(
            state,
            snap,
            request_id,
            // Each failed attempt records the TARGET it actually hit
            // (#790), not the group it was resolved from.
            &rec.target_model_id,
            requested_model,
            api_key_id,
            &rec.provider_key_id,
            rec.status,
            Duration::from_millis(u64::from(rec.latency_ms)),
            client,
            AttemptInfo::from_record(rec),
            guardrail_blocked,
            // Failed attempts carry no per-request redaction detail; the
            // terminal event does.
            crate::redact::RedactionCounts::new(),
            Vec::new(),
            None,
            /* terminal */ terminal_last && Some(i) == last_failed,
            // The record's own network-boundary fact: a rate-limit-refused
            // attempt never reached an upstream.
            /* dispatched */
            rec.dispatched,
            audit,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_access_log(
    model: &str,
    provider: &str,
    api_key_id: &str,
    status: u16,
    elapsed: Duration,
    request_id: &str,
    // Winning attempt's provider response id; `None` when unknown at this
    // point (streaming, guardrail block, pre-dispatch error).
    provider_request_id: Option<&str>,
    routing: &RoutingTelemetry,
    error: Option<&ProxyError>,
) {
    let (error_kind, error) = match error {
        Some(e) => {
            let (kind, msg) = crate::attempt::access_log_error(e);
            (Some(kind), Some(msg))
        }
        None => (None, None),
    };
    // Per #655 the access log stays ONE line per request, carrying the
    // user-perceived `latency` + final status plus a routing summary.
    let summary = routing.access_log_summary();
    let target = crate::attribution::AccessLogTarget::current();
    crate::attribution::emit_access_log(AccessLog {
        method: "POST",
        path: "/v1/responses",
        status,
        latency: elapsed,
        duration: elapsed,
        provider: Some(provider),
        model: Some(model),
        upstream_model: target.upstream_model(),
        provider_key_id: target.provider_key_id(),
        api_key_id: Some(api_key_id),
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        request_id,
        provider_request_id: provider_request_id.filter(|s| !s.is_empty()),
        served_by_model: summary.served_by_model,
        routing_attempt_count: summary.attempt_count,
        routing_fallback_count: summary.fallback_count,
        error_kind,
        error: error.as_deref(),
        mcp: None,
        cache: None,
        request_body_bytes: None,
        response_body_bytes: None,
    });
}

#[cfg(test)]
mod tests {
    /// Serializing the borrowed request with only `model` swapped gives the
    /// same bytes as cloning it, rewriting `model` and serializing the clone.
    #[test]
    fn model_renamed_matches_clone_and_rewrite() {
        for body in [
            serde_json::json!({
                "stream": true, "model": "alias", "input": [{"role": "user", "content": "hi"}],
                "tools": [], "metadata": {"model": "kept"}
            }),
            serde_json::json!({"model": null, "input": "quote: \" / slash: \\ / Unicode: 猫"}),
            serde_json::json!({"input": "no model field", "metadata": {"model": 42}}),
        ] {
            let fields = body.as_object().unwrap();
            let mut cloned = body.clone();
            if let Some(model) = cloned.get_mut("model") {
                *model = serde_json::Value::String("upstream\"猫".into());
            }
            assert_eq!(
                serde_json::to_vec(&crate::util::ModelRenamed {
                    fields,
                    model: "upstream\"猫"
                })
                .unwrap(),
                serde_json::to_vec(&cloned).unwrap()
            );
        }
    }

    #[test]
    fn model_rename_fast_path_keeps_every_body_rewrite() {
        use serde_json::json;
        let body = json!({"input": "hi"});
        for (request, rewrite) in [
            (json!(null), false),
            (json!({}), false),
            (json!({"default_headers": {"x-custom": "kept"}}), false),
            (
                json!({"param_renames": {"max_output_tokens": "max_tokens"}}),
                true,
            ),
            (json!({"param_constraints": {}}), true),
            (json!({"default_body_fields": {"store": false}}), true),
        ] {
            let pk = serde_json::from_value(json!({
                "display_name": "upstream", "api_key": "", "request": request
            }))
            .unwrap();
            assert_eq!(
                crate::util::model_only_fields(&body, &pk, Some("input")).is_none(),
                rewrite
            );
        }
        let fields = json!({"input": [{"type": "function_call", "call_id": "c__thought__sig"}]});
        let pk =
            serde_json::from_value(json!({"display_name": "upstream", "api_key": ""})).unwrap();
        assert!(crate::util::model_only_fields(&fields, &pk, Some("input")).is_none());
        let messages = json!({"messages": [{"type": "tool_use", "id": "c__thought__sig"}]});
        assert!(crate::util::model_only_fields(&messages, &pk, Some("messages")).is_none());
        assert!(crate::util::model_only_fields(&messages, &pk, None).is_some());
        assert!(crate::util::model_only_fields(&json!([]), &pk, None).is_none());
    }

    /// The two ways a Responses-API stream reports a failure in-band are
    /// both read as an upstream error — the `error` event in its flat and
    /// its nested shape alike; any other event is not one. A code
    /// naming no HTTP status maps as a status-less in-band error.
    #[test]
    fn responses_in_band_errors_are_read_from_both_failure_events() {
        let flat = serde_json::json!({
            "type": "error", "code": "server_error", "message": "boom", "param": null,
        });
        let nested = serde_json::json!({
            "type": "error", "sequence_number": 3,
            "error": {"type": "server_error", "code": null, "message": "nested boom", "param": null},
        });
        let failed = serde_json::json!({
            "type": "response.failed",
            "response": {"id": "resp_1", "error": {"code": "server_error", "message": "late boom"}},
        });
        for (event, message) in [
            (flat, "boom"),
            (nested, "nested boom"),
            (failed, "late boom"),
        ] {
            let err = super::responses_in_band_error(&event).expect("a failure event");
            assert_eq!(err.http_status(), 502);
            assert_eq!(
                crate::attempt::routing_error_class(&err),
                "upstream_in_band"
            );
            assert!(err.to_string().contains(message), "{err}");
        }
        for ty in [
            "response.completed",
            "response.incomplete",
            "response.output_text.delta",
        ] {
            assert!(super::responses_in_band_error(&serde_json::json!({"type": ty})).is_none());
        }
    }

    /// The Responses API spells a model's tool call as a bare
    /// `function_call` item with no `role`, and the caller's answer as a
    /// bare `function_call_output`. Reading only `role` reported both as
    /// user text — and, because a `function_call` carries none of the
    /// text keys the scan used to read, dropped the call entirely, so a
    /// payload parked in a replayed tool call reached the model unscanned
    /// while `/v1/chat/completions` screened the same replay.
    #[test]
    fn responses_input_maps_tool_loop_items_to_assistant_and_tool() {
        let body = serde_json::json!({
            "model": "m",
            "instructions": "be nice",
            "input": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "type": "message",
                 "content": [{"type": "output_text", "text": "reply"}]},
                {"role": "user", "content": "call it"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}]},
                {"type": "function_call", "call_id": "c1",
                 "name": "lookup", "arguments": "{\"q\":\"SECRET\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "result"},
            ],
        });
        let chat = super::responses_input_to_chat("m", &body);
        let seen: Vec<_> = chat
            .messages
            .iter()
            .map(|m| (m.role, m.content_str().into_owned()))
            .collect();
        assert_eq!(seen.len(), 7, "{seen:?}");
        assert_eq!(seen[0].0, aisix_gateway::Role::System);
        assert_eq!(seen[1].0, aisix_gateway::Role::User);
        assert_eq!(seen[2].0, aisix_gateway::Role::Assistant);
        assert_eq!(seen[3].0, aisix_gateway::Role::User);
        assert_eq!(seen[4].0, aisix_gateway::Role::Assistant, "reasoning");
        assert_eq!(seen[5].0, aisix_gateway::Role::Assistant, "function_call");
        assert_eq!(seen[6].0, aisix_gateway::Role::Tool, "function_call_output");
        // The call's name and arguments are the scannable text.
        assert!(seen[5].1.contains("lookup"), "{:?}", seen[5]);
        assert!(seen[5].1.contains("SECRET"), "{:?}", seen[5]);
    }

    /// The check pass and the mask walkers each answer "where does the
    /// latest turn start" for their own representation, so a model turn
    /// that carries no readable text still has to reach the parsed view —
    /// otherwise a `latest_turn` row would refuse on history that the
    /// mask walkers correctly treat as out of window.
    #[test]
    fn responses_input_keeps_a_text_empty_assistant_item_as_a_boundary() {
        let body = serde_json::json!({
            "model": "m",
            "input": [
                {"role": "user", "content": "earlier"},
                {"type": "reasoning", "encrypted_content": "opaque"},
                {"role": "user", "content": "fresh"},
            ],
        });
        let chat = super::responses_input_to_chat("m", &body);
        let roles: Vec<_> = chat.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                aisix_gateway::Role::User,
                aisix_gateway::Role::Assistant,
                aisix_gateway::Role::User,
            ],
        );
        assert_eq!(chat.messages[1].content_str(), "", "no readable text");
        let window = aisix_guardrails::latest_turn_view(&chat);
        assert_eq!(window.messages.len(), 1);
        assert_eq!(window.messages[0].content_str(), "fresh");
    }

    /// The parsed view is one message per `input[]` item, whatever the
    /// item carries. That alignment is what lets the check pass and the
    /// mask walkers land on the same boundary; dropping the text-empty
    /// item here would move the last model turn to the end of the list
    /// and widen a `latest_turn` row back to the whole conversation.
    #[test]
    fn responses_input_keeps_one_message_per_item_even_when_text_is_empty() {
        let body = serde_json::json!({
            "model": "m",
            "input": [
                {"role": "user", "content": "earlier SECRET"},
                {"role": "assistant", "type": "message",
                 "content": [{"type": "output_text", "text": "answered"}]},
                {"type": "function_call_output", "call_id": "c1", "output": ""},
            ],
        });
        let chat = super::responses_input_to_chat("m", &body);
        assert_eq!(chat.messages.len(), 3);
        let window = aisix_guardrails::latest_turn_view(&chat);
        assert_eq!(
            window.messages.len(),
            1,
            "only the empty tool result is in the window: {:?}",
            window
                .messages
                .iter()
                .map(|m| (m.role, m.content_str().into_owned()))
                .collect::<Vec<_>>(),
        );
    }

    /// Naming a consequence of the role mapping so it is not rediscovered
    /// as a bug: two kinds screen only user-role messages under their
    /// DEFAULT `text_source` — `semantic` (`user_messages`) and
    /// `azure_content_safety_text_moderation` (`concatenate_user_content`)
    /// — so a replayed tool result on `/v1/responses` is now outside what
    /// they read, exactly as a `role: "tool"` message already is on
    /// `/v1/chat/completions`. Before the mapping it was mislabelled as
    /// user text and they happened to scan it. Operators who want it
    /// screened set `text_source` to the all-messages value, on either
    /// surface.
    #[test]
    fn a_replayed_tool_result_is_not_user_role_on_either_surface() {
        let responses = super::responses_input_to_chat(
            "m",
            &serde_json::json!({
                "model": "m",
                "input": [
                    {"type": "function_call_output", "call_id": "c1", "output": "RESULT"},
                ],
            }),
        );
        let chat: aisix_gateway::ChatFormat = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "tool", "tool_call_id": "c1", "content": "RESULT"},
            ],
        }))
        .unwrap();
        assert_eq!(responses.messages[0].role, aisix_gateway::Role::Tool);
        assert_eq!(
            responses.messages[0].role, chat.messages[0].role,
            "the two surfaces must agree on what a tool result is",
        );
    }

    /// A tool RESULT is the caller answering, so it lands on `Role::Tool`
    /// and stays inside the latest-turn window.
    #[test]
    fn responses_input_maps_a_tool_result_to_the_tool_role() {
        let body = serde_json::json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "PAYLOAD"},
            ],
        });
        let chat = super::responses_input_to_chat("m", &body);
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[1].role, aisix_gateway::Role::Tool);
        assert_eq!(chat.messages[1].content_str(), "PAYLOAD");
    }

    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, Model, ProxyConfig};
    use aisix_gateway::Hub;
    use aisix_provider_anthropic::AnthropicBridge;
    use aisix_provider_openai::OpenAiBridge;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            addr: "127.0.0.1:0".into(),
            request_body_limit_bytes: 1_048_576,
            real_ip: Default::default(),
            request_id: Default::default(),
            url_rewrites: Vec::new(),
            expose_debug_headers: false,
            platform: Default::default(),
            tls: None,
            listeners: Vec::new(),
            thread_per_core: None,
            workers: None,
        }
    }

    const OPENAI_PK_ID: &str = "11111111-1111-1111-1111-111111111111";
    const ANTHROPIC_PK_ID: &str = "22222222-2222-2222-2222-222222222222";

    fn openai_model(name: &str) -> ResourceEntry<Model> {
        let json = format!(
            r#"{{"display_name":"{name}","provider":"openai","model_name":"gpt-4o","provider_key_id":"{OPENAI_PK_ID}"}}"#
        );
        let m: Model = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("m-1", m, 1)
    }

    fn anthropic_model(name: &str) -> ResourceEntry<Model> {
        let json = format!(
            r#"{{"display_name":"{name}","provider":"anthropic","model_name":"claude-3-haiku-20240307","provider_key_id":"{ANTHROPIC_PK_ID}"}}"#
        );
        let m: Model = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("m-2", m, 1)
    }

    fn openai_pk(api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"openai-up","secret":"sk-test","api_base":"{api_base}","provider":"openai","adapter":"openai"}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(OPENAI_PK_ID, pk, 1)
    }

    /// An OpenAI PK carrying per-PK `request.*` overrides (#867):
    /// a `default_body_fields` injection and a `default_headers` injection,
    /// so the verbatim Responses path can be asserted to apply both to the
    /// outbound upstream call.
    fn openai_pk_with_overrides(api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"openai-up","secret":"sk-test","api_base":"{api_base}","provider":"openai","adapter":"openai","request":{{"default_body_fields":{{"safe_flag":true}},"default_headers":{{"x-custom":"trace-on"}}}}}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(OPENAI_PK_ID, pk, 1)
    }

    /// An OpenAI PK carrying per-PK telemetry attribution tags
    /// (#867) so emitted UsageEvents can be asserted to surface the
    /// upstream vendor + PK label the dashboard's Logs detail shows.
    fn openai_pk_tagged(api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"openai-up","secret":"sk-test","api_base":"{api_base}","provider":"openai","adapter":"openai","telemetry_tags":{{"kind":"catalog","featured":true,"branded_provider":"openai","pk_label":"prod-codex-key"}}}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(OPENAI_PK_ID, pk, 1)
    }

    fn new_snap_openai_tagged(api_base: &str) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk_tagged(api_base));
        snap
    }

    fn anthropic_pk_at(api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"anthropic-up","secret":"sk-ant-test","api_base":"{api_base}","provider":"anthropic","adapter":"anthropic"}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(ANTHROPIC_PK_ID, pk, 1)
    }

    fn new_snap_openai(api_base: &str) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(api_base));
        snap
    }

    fn new_snap_anthropic_at(api_base: &str) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(anthropic_pk_at(api_base));
        snap
    }

    fn apikey_entry(allowed: &[&str]) -> ResourceEntry<ApiKey> {
        let json = format!(
            r#"{{"key_hash":"8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891","allowed_models":{}}}"#,
            serde_json::to_string(&allowed).unwrap()
        );
        let k: ApiKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("k-1", k, 1)
    }

    fn build_app(snap: AisixSnapshot) -> axum::Router {
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        // #825: the cross-provider /v1/responses path bridges non-OpenAI
        // targets through the provider Bridge; register Anthropic so those
        // tests resolve a bridge.
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        let handle = SnapshotHandle::new(snap);
        crate::build_router(crate::ProxyState::new(handle, hub, &cfg()).without_cache())
    }

    fn make_req(body: serde_json::Value) -> Request<axum::body::Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", "Bearer sk-caller")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }

    /// A keyword input guardrail, attached env-wide by
    /// `seed_env_scoped_guardrail` so it applies to every request. Blocks on a
    /// literal substring. Keyword is local (no remote call), so it's the
    /// deterministic stand-in for any input-hook guardrail kind.
    fn keyword_input_guardrail(literal: &str) -> ResourceEntry<aisix_core::Guardrail> {
        let json = format!(
            r#"{{"name":"test-block","enabled":true,"hook_point":"input","fail_open":false,"kind":"keyword","patterns":[{{"kind":"literal","value":"{literal}"}}]}}"#
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("g-1", g, 1)
    }

    /// `build_app` disables the cache backends; the cache test needs them on.
    fn build_app_with_cache(snap: AisixSnapshot) -> axum::Router {
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        crate::build_router(crate::ProxyState::new(
            SnapshotHandle::new(snap),
            hub,
            &cfg(),
        ))
    }

    /// An identical `/v1/responses` request is answered from the cache.
    ///
    /// This is where Codex traffic arrives, and it had no cache at all: a
    /// policy written for a model covered that model on
    /// `/v1/chat/completions` and quietly did nothing here.
    #[tokio::test]
    async fn identical_responses_request_is_served_from_cache() {
        let upstream = MockServer::builder().start().await;
        // ONE upstream call for TWO requests.
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_cached",
                "object": "response",
                "created_at": 0,
                "status": "completed",
                "model": "gpt-4o",
                "output": [{
                    "id": "msg_cached",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello"}]
                }],
                "usage": {"input_tokens": 9, "output_tokens": 4, "total_tokens": 13}
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let policy: aisix_core::CachePolicy = serde_json::from_str(
            r#"{"name":"resp-cache","enabled":true,"backend":"memory","ttl_seconds":60,"applies_to":"all"}"#,
        )
        .unwrap();
        snap.cache_policies
            .insert(ResourceEntry::new("cp-1", policy, 1));

        let app = build_app_with_cache(snap);
        let body = serde_json::json!({"model": "gpt-4o-resp", "input": "hi"});

        let first = app.clone().oneshot(make_req(body.clone())).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first
                .headers()
                .get(crate::chat::CACHE_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("miss"),
        );
        let first_body = to_bytes(first.into_body(), 65536).await.unwrap();

        let second = app.oneshot(make_req(body)).await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(crate::chat::CACHE_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("hit"),
        );
        let second_body = to_bytes(second.into_body(), 65536).await.unwrap();
        assert_eq!(
            first_body, second_body,
            "the Responses object must be replayed byte for byte",
        );
    }

    fn responses_cache_policy() -> ResourceEntry<aisix_core::CachePolicy> {
        let policy: aisix_core::CachePolicy = serde_json::from_str(
            r#"{"name":"resp-cache","enabled":true,"backend":"memory","ttl_seconds":60,"applies_to":"all"}"#,
        )
        .unwrap();
        ResourceEntry::new("cp-1", policy, 1)
    }

    /// 一个 200 里的 `status: "failed"` 是上游的一次失败，不是答案；`queued` 是
    /// 一次提交的回执。存进缓存就是把它们重放给之后整个 TTL 里的调用方。
    #[tokio::test]
    async fn a_response_object_that_is_not_an_answer_is_not_cached() {
        for body in [
            serde_json::json!({
                "id": "resp_failed",
                "object": "response",
                "status": "failed",
                "error": {"code": "server_error", "message": "boom"},
                "output": []
            }),
            serde_json::json!({
                "id": "resp_queued",
                "object": "response",
                "status": "queued",
                "output": []
            }),
            // Refused through a part, with an ordinary `completed` status.
            serde_json::json!({
                "id": "resp_refused",
                "object": "response",
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "refusal", "refusal": "no"}]}],
                "usage": {"input_tokens": 9, "output_tokens": 4, "total_tokens": 13}
            }),
        ] {
            let upstream = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&body))
                .expect(2)
                .mount(&upstream)
                .await;
            let snap = new_snap_openai(&upstream.uri());
            snap.models.insert(openai_model("gpt-4o-resp"));
            snap.apikeys.insert(apikey_entry(&["*"]));
            snap.cache_policies.insert(responses_cache_policy());
            let app = build_app_with_cache(snap);
            let request = serde_json::json!({"model": "gpt-4o-resp", "input": "hi"});

            for _ in 0..2 {
                let resp = app
                    .clone()
                    .oneshot(make_req(request.clone()))
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK, "{body}");
                assert_eq!(
                    resp.headers()
                        .get(crate::chat::CACHE_HEADER)
                        .map(|v| v.to_str().unwrap()),
                    Some("miss"),
                    "{body}"
                );
            }
        }
    }

    /// 成员的 `allowed_cidrs` 对缓存命中同样有效：chat 在查缓存之前就解析了目标，
    /// 这里曾经是查完缓存才解析，于是被排除的地址能读到别人填进去的答案。
    #[tokio::test]
    async fn a_cache_hit_answers_to_a_members_ip_allowlist() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_cached",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 9, "output_tokens": 4, "total_tokens": 13}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let snap = new_snap_openai(&upstream.uri());
        let member: Model = serde_json::from_str(&format!(
            r#"{{"display_name":"member","provider":"openai","model_name":"gpt-4o","provider_key_id":"{OPENAI_PK_ID}","allowed_cidrs":["10.0.0.0/8"]}}"#
        ))
        .unwrap();
        snap.models
            .insert(ResourceEntry::new("m-member", member, 1));
        snap.models.insert(routing_model("group", "member"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.cache_policies.insert(responses_cache_policy());
        let app = build_app_with_cache(snap);
        let from = |ip: &str| {
            let mut req = make_req(serde_json::json!({"model": "group", "input": "hi"}));
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
                    ip.parse().unwrap(),
                    40000,
                )));
            req
        };

        let filled = app.clone().oneshot(from("10.1.1.1")).await.unwrap();
        assert_eq!(filled.status(), StatusCode::OK);
        let hit = app.clone().oneshot(from("10.1.1.2")).await.unwrap();
        assert_eq!(
            hit.headers()
                .get(crate::chat::CACHE_HEADER)
                .map(|v| v.to_str().unwrap()),
            Some("hit"),
            "the entry has to be there for the next assertion to mean anything"
        );
        let outside = app.oneshot(from("8.8.8.8")).await.unwrap();
        assert_eq!(outside.status(), StatusCode::FORBIDDEN);
    }

    /// 路由标签选的是目标，答案属于产生它的那个目标。
    #[tokio::test]
    async fn requests_tagged_for_different_targets_do_not_share_a_cache_entry() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_cached",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 9, "output_tokens": 4, "total_tokens": 13}
            })))
            .expect(2)
            .mount(&upstream)
            .await;
        let snap = new_snap_openai(&upstream.uri());
        for name in ["member-eu", "member-us"] {
            let member: Model = serde_json::from_str(&format!(
                r#"{{"display_name":"{name}","provider":"openai","model_name":"gpt-4o","provider_key_id":"{OPENAI_PK_ID}"}}"#
            ))
            .unwrap();
            snap.models
                .insert(ResourceEntry::new(format!("m-{name}"), member, 1));
        }
        let group: Model = serde_json::from_str(
            r#"{"display_name":"group","routing":{"strategy":"failover","targets":[
                {"model":"member-eu","tags":["eu"]},{"model":"member-us","tags":["us"]}
            ]}}"#,
        )
        .unwrap();
        snap.models.insert(ResourceEntry::new("m-group", group, 1));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap.cache_policies.insert(responses_cache_policy());
        let app = build_app_with_cache(snap);
        let tagged = |tag: &str| {
            let mut req = make_req(serde_json::json!({"model": "group", "input": "hi"}));
            req.headers_mut()
                .insert(crate::client_ip::ROUTING_TAGS_HEADER, tag.parse().unwrap());
            req
        };
        let status = |resp: &axum::response::Response| {
            resp.headers()
                .get(crate::chat::CACHE_HEADER)
                .map(|v| v.to_str().unwrap().to_string())
        };

        let eu = app.clone().oneshot(tagged("eu")).await.unwrap();
        assert_eq!(status(&eu).as_deref(), Some("miss"));
        let us = app.clone().oneshot(tagged("us")).await.unwrap();
        assert_eq!(
            status(&us).as_deref(),
            Some("miss"),
            "another target's answer"
        );
        let eu_again = app.oneshot(tagged("eu")).await.unwrap();
        assert_eq!(status(&eu_again).as_deref(), Some("hit"));
    }

    /// #719 (the core fix): a configured INPUT guardrail that blocks on
    /// /v1/chat/completions must also fire on /v1/responses. The same blocked
    /// input must return 422 content_filter here — not 200 with the input
    /// echoed back — and the upstream must never be contacted (`expect(0)`).
    #[tokio::test]
    async fn input_guardrail_blocks_string_input_returns_422_content_filter() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_should_not_happen",
                "object": "response",
                "output": [{"type":"message","content":[{"type":"output_text","text":"echo: BLOCKME"}]}]
            })))
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "please BLOCKME now"
            })))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
        // Per #153 the matched literal must not leak into the wire message.
        let msg = v["error"]["message"].as_str().unwrap_or_default();
        assert!(!msg.contains("BLOCKME"), "blocklist literal leaked: {msg}");

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("blocked request must emit a UsageEvent")
            .expect("usage sink sender dropped");
        assert!(ev.guardrail_blocked);
        assert_eq!(ev.applied_guardrails.len(), 1);
        assert_eq!(ev.applied_guardrails[0].kind, "keyword");
        assert_eq!(ev.applied_guardrails[0].hook, "input");
        assert_eq!(ev.status_code, 422);
    }

    /// A routing (group) parent over one member, so the same blocked
    /// request can be addressed either directly or through the group.
    fn routing_model(name: &str, target: &str) -> ResourceEntry<Model> {
        let json = format!(
            r#"{{"display_name":"{name}","routing":{{"strategy":"failover","targets":[{{"model":"{target}"}}]}}}}"#
        );
        let m: Model = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(format!("router-{name}"), m, 1)
    }

    /// AISIX-Cloud#1428: an input-guardrail refusal on `/v1/responses` must
    /// emit a zero-token 422 UsageEvent marked `guardrail_blocked`, which is
    /// the exact predicate the dashboard's Logs "Guardrail blocks" view
    /// filters on. Before the fix the event carried the flag's `false`
    /// default, so the request appeared in the unfiltered feed and vanished
    /// from the Blocked one — which reads as the gateway having logged no
    /// guardrail activity at all.
    ///
    /// Driven over all four combinations the report names, because they
    /// take different code: a direct model refuses before any attempt is
    /// recorded (the terminal event is the pre-dispatch one), a group
    /// parent resolves its targets first, and `stream: true` changes which
    /// response the handler builds. The input hook runs before target
    /// selection either way, so all four must agree.
    #[tokio::test]
    async fn input_guardrail_block_marks_guardrail_blocked_usage_event() {
        use aisix_obs::UsageSink;

        for model in ["gpt-4o-resp", "resp-group"] {
            for stream in [false, true] {
                let upstream = MockServer::builder().start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/responses"))
                    .respond_with(ResponseTemplate::new(200))
                    .expect(0)
                    .mount(&upstream)
                    .await;

                let snap = new_snap_openai(&upstream.uri());
                snap.models.insert(openai_model("gpt-4o-resp"));
                snap.models
                    .insert(routing_model("resp-group", "gpt-4o-resp"));
                snap.apikeys.insert(apikey_entry(&["*"]));
                crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));

                let (tx, mut rx) = tokio::sync::mpsc::channel(8);
                let hub = Arc::new(Hub::new());
                hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
                let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
                    .without_cache()
                    .with_usage_sink(UsageSink::new(tx));

                let resp = crate::build_router(state)
                    .oneshot(make_req(serde_json::json!({
                        "model": model,
                        "input": "please BLOCKME now",
                        "stream": stream,
                    })))
                    .await
                    .unwrap();
                assert_eq!(
                    resp.status(),
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "model={model} stream={stream}"
                );

                let event = tokio::time::timeout(std::time::Duration::from_millis(1000), rx.recv())
                    .await
                    .expect("usage event must be emitted")
                    .expect("usage_sink sender dropped");
                assert!(
                    event.guardrail_blocked,
                    "model={model} stream={stream}: the refusal must be findable under \
                     guardrail_blocked=true"
                );
                assert_eq!(event.status_code, 422, "model={model} stream={stream}");
                // Nothing was sent upstream, so nothing is billed.
                assert_eq!(event.prompt_tokens, 0, "model={model} stream={stream}");
                assert_eq!(event.completion_tokens, 0, "model={model} stream={stream}");
                // The caller-addressed entry, group or not (AISIX-Cloud#790).
                assert_eq!(event.requested_model, model);
            }
        }
    }

    /// A provider that omits the blank line after its terminal
    /// `response.completed` used to cost the gateway that whole frame: it was
    /// forwarded to the caller but never parsed, so the body carried the real
    /// token counts while the UsageEvent carried estimates and no provider
    /// request id. At EOF the frame is complete, so it is parsed like any
    /// other — the caller's alias is stamped on it AND its usage is recorded.
    #[tokio::test]
    async fn streamed_unterminated_terminal_frame_is_still_parsed_for_usage() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        // Note the absent trailing blank line on the last frame.
        let sse = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_eof\",\"model\":\"gpt-4o-2024-11-20\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_eof\",\"model\":\"gpt-4o-2024-11-20\",\"usage\":{\"input_tokens\":31,\"output_tokens\":17,\"total_tokens\":48}}}";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));

        let resp = crate::build_router(state)
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi",
                "stream": true,
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let streamed =
            String::from_utf8(to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();

        // Delivered, restamped, and NOT given a terminator it never had.
        assert!(streamed.contains(r#""model":"gpt-4o-resp""#), "{streamed}");
        assert!(!streamed.contains("gpt-4o-2024-11-20"), "{streamed}");
        assert!(
            streamed.ends_with("}}}"),
            "no invented terminator: {streamed}"
        );

        let event = tokio::time::timeout(std::time::Duration::from_millis(1000), rx.recv())
            .await
            .expect("usage event must be emitted")
            .expect("usage_sink sender dropped");
        // The authoritative counts off that frame, not a local estimate.
        assert_eq!(event.prompt_tokens, 31, "{streamed}");
        assert_eq!(event.completion_tokens, 17, "{streamed}");
        assert!(!event.usage_estimated, "the frame was read, not guessed");
        assert_eq!(event.provider_request_id, "resp_eof");
    }

    /// `response.completed` carries the whole Response object, so an image
    /// result takes it well past a mebibyte, and it is the only frame that
    /// reports the usage: the row must have the provider's counts, not a guess.
    #[tokio::test]
    async fn a_terminal_frame_past_a_mebibyte_still_reports_its_usage() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let sse = format!(
            "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_big\",\"model\":\"gpt-4o-2024-11-20\"}}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_big\",\"model\":\"gpt-4o-2024-11-20\",\"output\":[{{\"type\":\"image_generation_call\",\"id\":\"ig_1\",\"result\":\"{}\"}}],\"usage\":{{\"input_tokens\":123456,\"output_tokens\":7890,\"total_tokens\":131346}}}}}}\n\n",
            "A".repeat(2 * 1024 * 1024),
        );
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));

        let resp = crate::build_router(state)
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi",
                "stream": true,
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let streamed = to_bytes(resp.into_body(), 8 << 20).await.unwrap();
        let streamed = String::from_utf8_lossy(&streamed);
        assert!(streamed.contains(r#""model":"gpt-4o-resp""#));
        assert!(
            !streamed.contains("gpt-4o-2024-11-20"),
            "the large frame is restamped like any other"
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("usage event must be emitted")
            .expect("usage_sink sender dropped");
        assert_eq!(event.status_code, 200);
        assert_eq!(event.prompt_tokens, 123_456);
        assert_eq!(event.completion_tokens, 7_890);
        assert!(!event.usage_estimated, "the frame was read, not guessed");
    }

    /// An upstream that sends `frames`, chunked, and then keeps the connection
    /// open: the caller is the one who ends this.
    async fn spawn_stalling_sse_upstream(frames: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = vec![0u8; 65536];
            let _ = sock.read(&mut req).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{frames}\r\n",
                frames.len(),
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        format!("http://{addr}")
    }

    /// What a refused held stream ends on, the frames the translated path sends
    /// for the same refusal: its opening, the flat `error` a client reads the
    /// reason from, and the `response.failed` that closes the response.
    fn assert_held_refusal_frames(bytes: &[u8]) {
        let events = crate::redact::parse_sse_json_stream(bytes).0;
        let kinds: Vec<_> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            kinds,
            [
                "response.created",
                "response.in_progress",
                "error",
                "response.failed"
            ],
            "{}",
            String::from_utf8_lossy(bytes),
        );
        assert_eq!(events[2]["code"], "content_filter");
        assert_eq!(events[3]["response"]["error"]["code"], "invalid_prompt");
    }

    /// An upstream's own failure frame reaches the caller less the key the
    /// request sent, whether or not an output rule holds the stream back.
    #[tokio::test]
    async fn an_upstream_error_frame_is_scrubbed_held_or_live() {
        const SECRET: &str = "sk-held-secret-0123456789";
        for held in [false, true] {
            let upstream = MockServer::builder().start().await;
            let sse = format!(
                "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_e\"}}}}\n\n\
                 event: error\ndata: {{\"type\":\"error\",\"error\":{{\"message\":\"Incorrect API key provided: {SECRET}\",\"type\":\"invalid_request_error\"}}}}\n\n"
            );
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
                .mount(&upstream)
                .await;
            let snap = AisixSnapshot::new();
            let pk: aisix_core::ProviderKey = serde_json::from_str(&format!(
                r#"{{"display_name":"openai-up","secret":"{SECRET}","api_base":"{}","provider":"openai","adapter":"openai"}}"#,
                upstream.uri()
            ))
            .unwrap();
            snap.provider_keys
                .insert(ResourceEntry::new(OPENAI_PK_ID, pk, 1));
            snap.models.insert(openai_model("gpt-4o-resp"));
            snap.apikeys.insert(apikey_entry(&["*"]));
            if held {
                crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
            }
            let resp = build_app(snap)
                .oneshot(make_req(
                    serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
            let wire = String::from_utf8_lossy(&bytes);
            assert!(
                wire.contains("Incorrect API key provided"),
                "held={held}: {wire}"
            );
            assert!(!wire.contains(SECRET), "held={held}: {wire}");
        }
    }

    /// A refusal goes out as a 200, since the head went out ahead of it, and the
    /// row says so: 200, `guardrail_blocked`, and what the upstream billed.
    #[tokio::test]
    async fn a_refused_held_stream_is_a_200_row_that_bills_the_upstream() {
        use aisix_obs::UsageSink;
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"BLOCKME\"}\n\n\
                   event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_r\",\"usage\":{\"input_tokens\":31,\"output_tokens\":17,\"total_tokens\":48}}}\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&upstream)
            .await;
        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let resp = crate::build_router(state)
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()[axum::http::header::CONTENT_TYPE],
            "text/event-stream"
        );
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert_held_refusal_frames(&bytes);
        assert!(!String::from_utf8_lossy(&bytes).contains("BLOCKME"));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("the guard must emit")
            .expect("usage_sink sender dropped");
        assert_eq!(event.status_code, 200);
        assert!(event.guardrail_blocked);
        assert_eq!(event.error_class, crate::attempt::GUARDRAIL_BLOCK_CLASS);
        assert_eq!(event.prompt_tokens, 31);
        assert_eq!(event.completion_tokens, 17);
    }

    /// The hold only starts once the upstream has answered. An upstream that
    /// refuses the request is still that refusal's own status.
    #[tokio::test]
    async fn an_upstream_refusal_keeps_its_status_under_a_held_stream() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"message": "bad", "type": "invalid_request_error"},
            })))
            .mount(&upstream)
            .await;
        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let resp = build_app(snap)
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// The head of a held stream goes out ahead of the verdict, and a caller who
    /// hangs up during the hold still leaves what the upstream is billing: the
    /// row has to say so, and the rate-limit buckets have to move.
    #[tokio::test]
    async fn a_caller_leaving_a_held_stream_is_charged_what_it_read() {
        use aisix_obs::UsageSink;
        let upstream = spawn_stalling_sse_upstream(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_held\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"a long answer the provider already billed\"}\n\n",
        )
        .await;
        let snap = new_snap_openai(&upstream);
        snap.models.insert(openai_model("gpt-4o-resp"));
        let key: aisix_core::ApiKey = serde_json::from_value(serde_json::json!({
            "key_hash": "8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891",
            "allowed_models": ["*"],
            "rate_limit": {"tpm": 5},
        }))
        .unwrap();
        snap.apikeys
            .insert(aisix_core::resource::ResourceEntry::new("k-1", key, 1));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);
        let held = app
            .clone()
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "a prompt of several words",
                "stream": true,
            })))
            .await
            .unwrap();
        assert_eq!(held.status(), StatusCode::OK);
        let mut body = held.into_body().into_data_stream();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                futures::StreamExt::next(&mut body),
            )
            .await
            .is_err(),
            "nothing is released before the verdict",
        );
        drop(body);
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("the guard must emit")
            .expect("usage_sink sender dropped");
        assert_eq!(event.status_code, 499);
        assert_eq!(event.error_class, "client_disconnected");
        assert!(event.prompt_tokens > 0, "the prompt reached the provider");
        assert!(event.completion_tokens > 0, "the answer's start did too");
        assert!(event.usage_estimated, "no usage frame had arrived");
        assert_eq!(event.provider_request_id, "resp_held");

        let next = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(next.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// When the terminal frame had arrived and only the verdict was pending,
    /// the row carries the provider's own counts.
    #[tokio::test]
    async fn a_caller_leaving_during_the_verdict_is_charged_the_upstreams_counts() {
        use aisix_obs::UsageSink;
        let moderation = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(30))
                    .set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&moderation)
            .await;
        let upstream = spawn_stalling_sse_upstream(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"an answer to moderate\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_done\",\"usage\":{\"input_tokens\":31,\"output_tokens\":17,\"total_tokens\":48}}}\n\n",
        )
        .await;
        let snap = new_snap_openai(&upstream);
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let row: aisix_core::Guardrail = serde_json::from_value(serde_json::json!({
            "name": "slow",
            "enabled": true,
            "hook_point": "output",
            "kind": "openai_moderation",
            "endpoint": moderation.uri(),
            "api_key": "k",
        }))
        .unwrap();
        crate::seed_env_scoped_guardrail(
            &snap,
            aisix_core::resource::ResourceEntry::new("g-slow", row, 1),
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let held = crate::build_router(state)
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi",
                "stream": true,
            })))
            .await
            .unwrap();
        let mut body = held.into_body().into_data_stream();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(500),
            futures::StreamExt::next(&mut body),
        )
        .await
        .is_err());
        drop(body);
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("the guard must emit")
            .expect("usage_sink sender dropped");
        assert_eq!(event.status_code, 499);
        assert_eq!(event.prompt_tokens, 31);
        assert_eq!(event.completion_tokens, 17);
        assert!(!event.usage_estimated);
    }

    /// With no output chain nothing is owed, so an answer longer than the
    /// accumulator exports what it kept, cut at the cap like chat's.
    #[tokio::test]
    async fn an_unguarded_stream_past_the_accumulator_cap_keeps_its_capture() {
        let big = "x".repeat(crate::token_estimate::OUTPUT_ACCUMULATION_CAP + 1024);
        let frames = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({"type": "response.output_text.delta", "delta": big}),
            serde_json::json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}},
            }),
        );
        let upstream = futures::stream::iter([Ok::<_, crate::stream_timeout::RawStreamError>(
            bytes::Bytes::from(frames),
        )]);
        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen_out = Arc::clone(&seen);
        let stream = super::build_responses_passthrough_stream(
            upstream,
            std::time::Instant::now(),
            std::time::Instant::now(),
            None,
            Some(64),
            None,
            true,
            "m".to_string(),
            crate::responses_affinity::ResponseMark::default(),
            move |_, text, _, safe, _| {
                *seen_out.lock().unwrap() = Some((text, safe));
            },
        );
        let _: Vec<_> = futures::StreamExt::collect(stream).await;
        let (text, safe) = seen.lock().unwrap().take().expect("completed");
        assert!(safe, "no scan was owed");
        assert!(!text.is_empty());
    }

    /// The same tail, but ending in ONE newline — a provider that wrote half
    /// its terminator. The EOF branch completes the terminator so the drain
    /// can read the frame, then strips back exactly what it added; padding a
    /// fixed `\n\n` here would end the frame a byte early and eat the newline
    /// the upstream really sent.
    #[tokio::test]
    async fn streamed_terminal_frame_with_a_half_terminator_keeps_its_own_bytes() {
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_half\",\"model\":\"gpt-4o-2024-11-20\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_half\",\"model\":\"gpt-4o-2024-11-20\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8}}}\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg()).without_cache();

        let resp = crate::build_router(state)
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi",
                "stream": true,
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let streamed =
            String::from_utf8(to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();

        assert!(
            streamed.contains(r#""model":"gpt-4o-resp""#),
            "{streamed:?}"
        );
        // The upstream's own single trailing newline survives — not stripped,
        // and not promoted to a full terminator it never sent.
        assert!(
            streamed.ends_with("}}}\n"),
            "the provider's own trailing newline must survive: {:?}",
            &streamed[streamed.len().saturating_sub(40)..]
        );
        assert!(!streamed.ends_with("}}}\n\n"), "no terminator invented");
    }

    /// #719: the Responses `input` array form (message items with typed
    /// content parts) must be scanned too — a blocked literal inside an
    /// `input_text` part blocks the call.
    #[tokio::test]
    async fn input_guardrail_blocks_array_message_items() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id":"x","object":"response","output":[]})),
            )
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "hi BLOCKME"}]}
                ]
            })))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
    }

    /// #719 companion: a benign input with a configured input guardrail must
    /// still forward to the upstream (`expect(1)`) and return 200 — the
    /// guardrail must not block clean traffic.
    #[tokio::test]
    async fn input_guardrail_allows_benign_input_forwards_200() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_ok",
                "object": "response",
                "output": [{"type":"message","content":[{"type":"output_text","text":"hi"}]}]
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "a perfectly fine request"
            })))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "response");
    }

    /// #719 (audit MEDIUM-1): a `function_call_output` item carries
    /// caller-supplied tool-result text under `output` (not `content`), and
    /// that text reaches the model. It must be scanned too — otherwise the
    /// surface-switch bypass survives on the tool-result channel. A blocked
    /// literal in `output` must 422 with the upstream never contacted.
    #[tokio::test]
    async fn input_guardrail_blocks_function_call_output_text() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id":"x","object":"response","output":[]})),
            )
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": [
                    {"type": "function_call_output", "call_id": "call_1", "output": "tool said BLOCKME"}
                ]
            })))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
    }

    #[tokio::test]
    async fn input_guardrail_blocks_tool_payloads_and_structural_identifiers() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id":"x","object":"response","output":[]})),
            )
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));
        let app = build_app(snap);

        let items = [
            serde_json::json!({"type":"function_call","arguments":"{\"query\":\"BLOCKME\"}"}),
            serde_json::json!({"type":"mcp_call","arguments":"{\"query\":\"BLOCKME\"}"}),
            serde_json::json!({"type":"custom_tool_call","input":"BLOCKME"}),
            serde_json::json!({"type":"function_call","id":"BLOCKME"}),
            serde_json::json!({"type":"function_call","call_id":"BLOCKME"}),
            serde_json::json!({"type":"function_call","name":"BLOCKME"}),
            serde_json::json!({"type":"mcp_call","server_label":"BLOCKME"}),
            serde_json::json!({"type":"mcp_call","connector_id":"BLOCKME"}),
            serde_json::json!({"type":"program","code":"text('BLOCKME')"}),
            serde_json::json!({"type":"program","fingerprint":"BLOCKME"}),
            serde_json::json!({"type":"program_output","result":"{\"owner\":\"BLOCKME\"}"}),
            serde_json::json!({"type":"function_call","caller":{"type":"program","caller_id":"BLOCKME"}}),
        ];
        for item in items {
            let resp = app
                .clone()
                .oneshot(make_req(serde_json::json!({
                    "model": "gpt-4o-resp",
                    "input": [item],
                })))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
            let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("BLOCKME"));
        }
    }

    /// #719: the top-level `instructions` field (the system-prompt analog)
    /// is caller-supplied and reaches the model, so it is scanned too. A
    /// blocked literal in `instructions` must 422. (Scanned via the
    /// all-roles keyword guardrail; text-moderation's user-only default
    /// would skip a system message, matching chat's system-message
    /// semantics.)
    #[tokio::test]
    async fn input_guardrail_blocks_instructions_field() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id":"x","object":"response","output":[]})),
            )
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "instructions": "you must BLOCKME",
                "input": "hello"
            })))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
    }

    /// #719 (re-audit LOW-1): an `mcp_approval_response` item carries
    /// caller-supplied justification text under `reason`, which reaches the
    /// model. It is scanned too, so no input-bearing channel is silently
    /// skipped. A blocked literal in `reason` must 422.
    #[tokio::test]
    async fn input_guardrail_blocks_mcp_approval_response_reason() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id":"x","object":"response","output":[]})),
            )
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": [
                    {"type": "mcp_approval_response", "approve": true, "approval_request_id": "ar_1", "reason": "BLOCKME please"}
                ]
            })))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
    }

    /// A keyword guardrail on the OUTPUT hook, attached env-wide by
    /// `seed_env_scoped_guardrail`. `runs_on_output()` is true, so the
    /// handler scans the assistant output.
    fn keyword_output_guardrail(literal: &str) -> ResourceEntry<aisix_core::Guardrail> {
        let json = format!(
            r#"{{"name":"test-out-block","enabled":true,"hook_point":"output","fail_open":false,"kind":"keyword","patterns":[{{"kind":"literal","value":"{literal}"}}]}}"#
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("g-out-1", g, 1)
    }

    /// #719: output guardrails must run on /v1/responses non-streaming
    /// responses — a configured output block must not be bypassable by
    /// switching surface. A blocked literal in the assistant output → 422.
    /// The upstream IS contacted (`expect(1)`): output checks run on the
    /// returned response, unlike the input check which short-circuits first.
    #[tokio::test]
    async fn output_guardrail_blocks_non_streaming_response() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_x",
                "object": "response",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"sure: BLOCKME here"}]}],
                "usage": {"input_tokens": 5, "output_tokens": 4}
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
        // The blocked model output must not be echoed back to the caller.
        let msg = v["error"]["message"].as_str().unwrap_or_default();
        assert!(
            !msg.contains("BLOCKME"),
            "model output leaked in error: {msg}"
        );
    }

    #[tokio::test]
    async fn output_guardrail_blocks_structural_tool_identifier() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_x",
                "object": "response",
                "output": [{
                    "type": "mcp_call",
                    "id": "item_1",
                    "call_id": "call_1",
                    "server_label": "BLOCKME",
                    "arguments": "{}"
                }],
                "usage": {"input_tokens": 5, "output_tokens": 4}
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("BLOCKME"));
    }

    /// A Responses refusal is client-visible assistant output, but it uses a
    /// `refusal` field instead of a `text` field. It must not bypass output
    /// guardrails on the non-streaming route.
    #[tokio::test]
    async fn output_guardrail_blocks_non_streaming_refusal_response() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_refusal",
                "object": "response",
                "output": [{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"sure: BLOCKME here"}]}],
                "usage": {"input_tokens": 5, "output_tokens": 4}
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
        assert!(
            !v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("BLOCKME"),
            "model refusal leaked in error: {v}"
        );
    }

    /// #719 companion: a clean non-streaming response with an output
    /// guardrail configured passes through unchanged → 200 with body.
    #[tokio::test]
    async fn output_guardrail_allows_clean_non_streaming_response() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_ok",
                "object": "response",
                "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a clean answer"}]}],
                "usage": {"input_tokens": 5, "output_tokens": 3}
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["output"][0]["content"][0]["text"], "a clean answer");
    }

    /// #719: streaming /v1/responses must also enforce output guardrails —
    /// else `stream:true` bypasses the output block. The blocked content is
    /// held back (BufferFull): the client gets 422 and never the tokens.
    #[tokio::test]
    async fn output_guardrail_blocks_streaming_response_holds_back() {
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"sure: BLOCKME\"}\n\n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"sure: BLOCKME\"}]}]}}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        // The held-back content must never reach the client.
        assert!(
            !String::from_utf8_lossy(&bytes).contains("BLOCKME"),
            "streamed content leaked despite output block",
        );
        assert_held_refusal_frames(&bytes);
    }

    /// Streaming refusals use their own delta and aggregate events. The
    /// held-back stream must block before any refusal text reaches the wire.
    #[tokio::test]
    async fn output_guardrail_blocks_streaming_refusal_response_holds_back() {
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.refusal.delta\n\
                   data: {\"type\":\"response.refusal.delta\",\"item_id\":\"msg_refusal\",\"output_index\":0,\"content_index\":0,\"delta\":\"sure: BLOCKME\"}\n\n\
                   event: response.refusal.done\n\
                   data: {\"type\":\"response.refusal.done\",\"item_id\":\"msg_refusal\",\"output_index\":0,\"content_index\":0,\"refusal\":\"sure: BLOCKME\"}\n\n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"refusal\",\"refusal\":\"sure: BLOCKME\"}]}]}}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("BLOCKME"),
            "streamed refusal leaked despite output block",
        );
        assert_held_refusal_frames(&bytes);
    }

    /// A Responses provider may emit only the final refusal event, without a
    /// delta or terminal response snapshot. The typed `/v1/responses` path
    /// must still hold and block it before it reaches the client.
    #[tokio::test]
    async fn output_guardrail_blocks_done_only_streaming_refusal() {
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.refusal.done\n\
                   data: {\"type\":\"response.refusal.done\",\"item_id\":\"msg_refusal\",\"output_index\":0,\"content_index\":0,\"refusal\":\"sure: BLOCKME\"}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("BLOCKME"),
            "done-only refusal leaked despite output block",
        );
        assert_held_refusal_frames(&bytes);
    }

    /// #719 companion: a clean streaming response with an output guardrail
    /// is scanned then released in full → 200 + the SSE body.
    #[tokio::test]
    async fn output_guardrail_allows_clean_streaming_response() {
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"a clean answer\"}\n\n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_clean\",\"status\":\"completed\",\"output\":[]}}\n\n\
                   event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"AFTER_RESPONSE_COMPLETED\"}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("a clean answer"),
            "clean SSE body must be released in full",
        );
        assert!(
            !body.contains("AFTER_RESPONSE_COMPLETED"),
            "frames after response.completed must be discarded",
        );
        assert!(body.contains("[DONE]"));
        assert!(
            body.starts_with("event: ") || body.starts_with("data: "),
            "SSE shape preserved on release",
        );
    }

    /// Anthropic Messages streaming SSE carrying a single text delta.
    fn anthropic_text_sse(text: &str) -> String {
        format!(
            "event: message_start\n\
             data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_g\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-haiku-20240307\",\"content\":[],\"usage\":{{\"input_tokens\":5,\"output_tokens\":0}}}}}}\n\n\
             event: content_block_start\n\
             data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
             event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\n\
             event: content_block_stop\n\
             data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":3}}}}\n\n\
             event: message_stop\n\
             data: {{\"type\":\"message_stop\"}}\n\n",
            text = serde_json::to_string(text).unwrap(),
        )
    }

    /// #825 + #719: the cross-provider (bridged) streaming path must enforce
    /// output guardrails too — else `stream:true` against a non-OpenAI model
    /// bypasses the block. The bridge buffers the encoded SSE and, on a
    /// block, emits only a terminal `error` event; no output_text delta with
    /// the blocked literal reaches the client.
    /// A bridged, non-streaming failure feeds the target's health like every
    /// other failure on this endpoint: the admin health surface would
    /// otherwise show the target Healthy through a whole outage.
    #[tokio::test]
    async fn a_failed_bridged_call_counts_against_the_targets_health() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&upstream)
            .await;
        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("bridged-health"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let hub = Arc::new(Hub::new());
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg()).without_cache();
        let app = crate::build_router(state.clone());
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(make_req(
                    serde_json::json!({"model":"bridged-health","input":"hi"}),
                ))
                .await
                .unwrap();
            assert!(resp.status().is_server_error(), "{}", resp.status());
        }
        assert_ne!(
            state.health.level("bridged-health"),
            crate::health::HealthLevel::Healthy
        );
    }

    #[tokio::test]
    async fn output_guardrail_blocks_streaming_cross_provider_response() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_text_sse("sure: BLOCKME here")),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("claude-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"claude-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        // The SSE 200 is committed by the first-chunk failover peek; the block
        // surfaces as an in-band terminal error event.
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("content_filter"),
            "missing block error: {body}"
        );
        assert!(
            !body.contains("BLOCKME"),
            "blocked content leaked in stream: {body}"
        );
        assert!(
            !body.contains("response.output_text.delta"),
            "held-back deltas leaked: {body}"
        );
    }

    /// #825 companion: a clean bridged streaming response with an output
    /// guardrail is scanned then released in full.
    #[tokio::test]
    async fn output_guardrail_allows_clean_streaming_cross_provider_response() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_text_sse("a clean answer")),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("claude-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"claude-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("a clean answer"),
            "clean body withheld: {body}"
        );
        assert!(body.contains("response.completed"));
    }

    /// #825: a blocked cross-provider STREAM still bills the upstream tokens
    /// and the emitted UsageEvent is marked guardrail_blocked, so the
    /// dashboard's Blocked tab and the budget ledger see it rather than
    /// recording it as clean usage. Its status is the 200 the caller received,
    /// since this head went out before the verdict.
    #[tokio::test]
    async fn streaming_cross_provider_block_emits_guardrail_blocked_usage_event() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_text_sse("sure: BLOCKME")),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("claude-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"claude-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Drain the body so the stream's Drop guard fires the usage event.
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("BLOCKME"));

        let event = tokio::time::timeout(std::time::Duration::from_millis(1000), rx.recv())
            .await
            .expect("usage event must be emitted")
            .expect("usage_sink sender dropped");
        assert!(
            event.guardrail_blocked,
            "a blocked stream must mark guardrail_blocked"
        );
        assert_eq!(event.status_code, 200);
        assert_eq!(event.error_class, "content_filter");
        // The upstream-billed tokens are still recorded.
        assert_eq!(event.prompt_tokens, 5);
        assert_eq!(event.completion_tokens, 3);
    }

    /// The refusal that needs no verdict reads the same: a bridged stream that
    /// outgrew its hold-back buffer answered a 200 and an in-band error, so its
    /// row records the 200 as well.
    #[tokio::test]
    async fn streaming_cross_provider_overflow_records_the_head_it_sent() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let big = serde_json::to_string(&"y".repeat(300_000)).unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_text_sse(&big)),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("claude-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"claude-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 1 << 21).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("content_filter"), "{body}");
        assert!(!body.contains("yyyy"), "held bytes leaked: {body}");

        let event = tokio::time::timeout(std::time::Duration::from_millis(1000), rx.recv())
            .await
            .expect("usage event must be emitted")
            .expect("usage_sink sender dropped");
        assert!(event.guardrail_blocked);
        assert_eq!(event.status_code, 200);
    }

    /// #719 (audit HIGH-1): the streaming hold-back buffer is capped so a
    /// huge (or malicious) upstream response can't OOM the gateway. A
    /// response exceeding the BufferFull cap fails closed (422) rather than
    /// being released unscanned — even when its content is otherwise clean.
    #[tokio::test]
    async fn output_guardrail_streaming_oversized_response_fails_closed() {
        let upstream = MockServer::builder().start().await;
        // One delta larger than the 256 KiB default BufferFull cap.
        let big = "x".repeat(300_000);
        let sse =
            format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{big}\"}}\n\ndata: [DONE]\n\n");
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert_held_refusal_frames(&bytes);
    }

    /// `keyword_output_guardrail` with `enforcement_mode: monitor` — the
    /// chain resolves to `EndOfStreamCheck` (never holds back, never blocks).
    fn keyword_output_guardrail_monitor(literal: &str) -> ResourceEntry<aisix_core::Guardrail> {
        let json = format!(
            r#"{{"name":"test-out-mon","enabled":true,"hook_point":"output","fail_open":false,"enforcement_mode":"monitor","kind":"keyword","patterns":[{{"kind":"literal","value":"{literal}"}}]}}"#
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("g-out-mon-1", g, 1)
    }

    /// #1010: a MONITOR-mode output guardrail must never make a
    /// streaming /v1/responses request fail closed. Same oversized stream as
    /// the fail-closed test above, but the chain resolves to EndOfStreamCheck
    /// — the bytes forward live and the client gets the full 200 SSE, not a
    /// 422. Pre-fix, any output-hook guardrail (monitor included) forced the
    /// hold-back branch and a >256 KiB response was rejected with
    /// `content_filter` — a monitor rule "blocking", which it must never do.
    #[tokio::test]
    async fn monitor_output_guardrail_oversized_stream_released() {
        let upstream = MockServer::builder().start().await;
        let big = "x".repeat(300_000);
        let sse =
            format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{big}\"}}\n\ndata: [DONE]\n\n");
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail_monitor("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a monitor-only chain must never fail a stream closed on the buffer cap",
        );
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.len() >= 300_000 && body.contains("[DONE]"),
            "the full SSE must be released, got {} bytes",
            body.len(),
        );
    }

    /// #1010 companion: on the live-forward monitor path the
    /// end-of-stream scan still runs — a violating stream is delivered
    /// verbatim (200, content included) and the emitted usage event carries
    /// the `would_block` observation instead of `guardrail_blocked`.
    #[tokio::test]
    async fn monitor_output_guardrail_streams_live_and_records_would_block() {
        use aisix_obs::UsageSink;
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"sure: BLOCKME\"}\n\n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"sure: BLOCKME\"}]}],\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail_monitor("BLOCKME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).contains("BLOCKME"),
            "monitor mode must deliver the stream verbatim",
        );

        let event = tokio::time::timeout(std::time::Duration::from_millis(1000), rx.recv())
            .await
            .expect("usage event must be emitted")
            .expect("usage_sink sender dropped");
        assert!(!event.guardrail_blocked);
        assert_eq!(event.status_code, 200);
        assert!(
            event
                .guardrail_monitor_hits
                .iter()
                .any(|h| h.hook == "output" && h.action == "would_block"),
            "the end-of-stream scan must record the would-block observation, got {:?}",
            event.guardrail_monitor_hits,
        );
    }

    /// #1010 audit H1: the end-of-stream observation awaits a
    /// remote guardrail provider, and SDK clients close the connection right
    /// after the terminal frame — the generator is dropped at that await.
    /// The completion guard must stay armed across the scan so the Drop
    /// still emits the usage event: a fully-delivered 200 stream must never
    /// lose its billing/logs record to a disconnect during the observation.
    #[tokio::test]
    async fn monitor_scan_disconnect_still_emits_usage_event() {
        use aisix_obs::UsageSink;
        use futures::StreamExt;
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello there\"}\n\n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello there\"}]}],\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        // Remote moderation backend that never answers within the test —
        // parks the end-of-stream scan so the disconnect lands mid-await.
        let acs = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/contentsafety/text:analyze"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(30))
                    .set_body_json(serde_json::json!({
                        "categoriesAnalysis": [],
                        "blocklistsMatch": []
                    })),
            )
            .mount(&acs)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let textmod_json = format!(
            r#"{{"name":"textmod-mon","enabled":true,"kind":"azure_content_safety_text_moderation","hook_point":"output","enforcement_mode":"monitor","endpoint":"{}","api_key":"k"}}"#,
            acs.uri()
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&textmod_json).unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-textmod-mon", g, 1));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Read frames until the terminal [DONE] is on the wire, then poll
        // once more so the generator advances past the loop and parks on the
        // remote-scan await — and drop the body there, like an SDK client
        // closing after the terminal frame.
        let mut body_stream = resp.into_body().into_data_stream();
        let mut wire = Vec::new();
        while !String::from_utf8_lossy(&wire).contains("[DONE]") {
            let chunk =
                tokio::time::timeout(std::time::Duration::from_millis(2000), body_stream.next())
                    .await
                    .expect("stream must deliver the terminal frame promptly")
                    .expect("stream ended before [DONE]")
                    .expect("stream errored");
            wire.extend_from_slice(chunk.as_ref());
        }
        let parked =
            tokio::time::timeout(std::time::Duration::from_millis(300), body_stream.next()).await;
        assert!(
            parked.is_err(),
            "generator should be parked on the remote scan await",
        );
        drop(body_stream);

        let event = tokio::time::timeout(std::time::Duration::from_millis(1000), rx.recv())
            .await
            .expect("disconnect during the EOS scan must still emit the usage event")
            .expect("usage_sink sender dropped");
        assert_eq!(event.status_code, 200);
        assert!(!event.guardrail_blocked);
        assert_eq!(event.prompt_tokens, 5);
        assert_eq!(event.completion_tokens, 3);
    }

    /// #1010, cross-provider bridge path: a monitor-mode output
    /// guardrail must not hold back or fail the bridged stream on the buffer
    /// cap either. An oversized bridged response is released in full with no
    /// `content_filter` error frame, and the usage event stays an unblocked
    /// 200.
    #[tokio::test]
    async fn monitor_output_guardrail_oversized_cross_provider_stream_released() {
        use aisix_obs::UsageSink;
        let upstream = MockServer::builder().start().await;
        let big = serde_json::to_string(&"y".repeat(300_000)).unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(anthropic_text_sse(&big)),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("claude-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail_monitor("BLOCKME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"claude-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 8 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains(&"y".repeat(1000)),
            "the bridged stream must be released, not withheld",
        );
        assert!(
            !body.contains("content_filter"),
            "no fail-closed error frame on a monitor-only chain",
        );

        let event = tokio::time::timeout(std::time::Duration::from_millis(1000), rx.recv())
            .await
            .expect("usage event must be emitted")
            .expect("usage_sink sender dropped");
        assert!(!event.guardrail_blocked);
        assert_eq!(event.status_code, 200);
    }

    /// #546: output tool-call arguments must be scanned. A blocked literal in
    /// a `function_call` item's `arguments` (a top-level item field, not under
    /// `content[]`) must block the non-streaming response — else tool-call
    /// output is an output-guardrail bypass.
    #[tokio::test]
    async fn output_guardrail_blocks_tool_call_arguments_non_streaming() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_tc",
                "object": "response",
                "output": [{
                    "type": "function_call",
                    "name": "lookup",
                    "arguments": "{\"q\":\"BLOCKME\"}"
                }],
                "usage": {"input_tokens": 5, "output_tokens": 4}
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "content_filter");
    }

    /// #546: tool-call arguments are scanned on the streaming path too (via
    /// the `response.completed` event), and held back — the args never reach
    /// the client.
    #[tokio::test]
    async fn output_guardrail_blocks_tool_call_arguments_streaming() {
        let upstream = MockServer::builder().start().await;
        let completed = serde_json::json!({
            "type": "response.completed",
            "response": {"output": [{
                "type": "function_call",
                "name": "lookup",
                "arguments": "{\"q\":\"BLOCKME\"}"
            }]}
        });
        let sse = format!("event: response.completed\ndata: {completed}\n\ndata: [DONE]\n\n");
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("BLOCKME"),
            "tool-call arguments leaked despite output block",
        );
        assert_held_refusal_frames(&bytes);
    }

    /// #546 (audit HIGH): tool-call args on a `response.incomplete` terminal
    /// (fires routinely on `max_output_tokens` truncation — it carries the
    /// full `output[]`) must also be scanned and held back, not only
    /// `response.completed`. Same for `response.failed`.
    #[tokio::test]
    async fn output_guardrail_blocks_tool_call_on_incomplete_terminal() {
        let upstream = MockServer::builder().start().await;
        let incomplete = serde_json::json!({
            "type": "response.incomplete",
            "response": {"output": [{
                "type": "function_call",
                "name": "lookup",
                "arguments": "{\"q\":\"BLOCKME\"}"
            }]}
        });
        let sse = format!("event: response.incomplete\ndata: {incomplete}\n\ndata: [DONE]\n\n");
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("BLOCKME"),
            "tool-call args leaked on response.incomplete terminal",
        );
        assert_held_refusal_frames(&bytes);
    }

    /// #546 (audit HIGH): streamed tool-call argument deltas must be scanned —
    /// args arrive via `response.function_call_arguments.delta`, not
    /// `output_text.delta`. The literal is split across two deltas to pin
    /// that they reassemble without a separator.
    #[tokio::test]
    async fn output_guardrail_blocks_tool_call_delta() {
        let upstream = MockServer::builder().start().await;
        let d1 = serde_json::json!({"type":"response.function_call_arguments.delta","delta":"{\"q\":\"BLOCK"});
        let d2 =
            serde_json::json!({"type":"response.function_call_arguments.delta","delta":"ME\"}"});
        let sse = format!(
            "event: response.function_call_arguments.delta\ndata: {d1}\n\n\
             event: response.function_call_arguments.delta\ndata: {d2}\n\n\
             event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_tool\",\"status\":\"completed\",\"output\":[]}}}}\n\n\
             data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("BLOCK"),
            "streamed tool-call args leaked with no terminal event",
        );
        assert_held_refusal_frames(&bytes);
    }

    /// #546 (audit HIGH): a stream that ends WITHOUT a terminal `response`
    /// object but streamed tool-call argument deltas must still be scanned —
    /// args arrive via `response.function_call_arguments.delta`, not
    /// `output_text.delta`. The literal is split across two deltas to pin
    /// that they reassemble without a separator.
    #[tokio::test]
    async fn output_guardrail_blocks_tool_call_delta_without_terminal() {
        let upstream = MockServer::builder().start().await;
        let d1 = serde_json::json!({"type":"response.function_call_arguments.delta","delta":"{\"q\":\"BLOCK"});
        let d2 =
            serde_json::json!({"type":"response.function_call_arguments.delta","delta":"ME\"}"});
        let sse = format!(
            "event: response.function_call_arguments.delta\ndata: {d1}\n\n\
             event: response.function_call_arguments.delta\ndata: {d2}\n\ndata: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("BLOCK"),
            "streamed tool-call args leaked with no terminal event",
        );
        assert_held_refusal_frames(&bytes);
    }

    /// #546 (re-audit): MCP tool-call argument deltas stream via their own
    /// event too and must still be scanned and held back.
    #[tokio::test]
    async fn output_guardrail_blocks_mcp_tool_call_delta() {
        let upstream = MockServer::builder().start().await;
        let d = serde_json::json!({"type":"response.mcp_call_arguments.delta","delta":"{\"q\":\"BLOCKME\"}"});
        let sse = format!(
            "event: response.mcp_call_arguments.delta\ndata: {d}\n\n\
             event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_mcp\",\"status\":\"completed\",\"output\":[]}}}}\n\n\
             data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("BLOCKME"));
        assert_held_refusal_frames(&bytes);
    }

    /// #546 (re-audit): MCP tool-call argument deltas stream via their own
    /// event too; a no-terminal stream of `response.mcp_call_arguments.delta`
    /// carrying a blocked literal must still be scanned and held back.
    #[tokio::test]
    async fn output_guardrail_blocks_mcp_tool_call_delta_without_terminal() {
        let upstream = MockServer::builder().start().await;
        let d = serde_json::json!({"type":"response.mcp_call_arguments.delta","delta":"{\"q\":\"BLOCKME\"}"});
        let sse =
            format!("event: response.mcp_call_arguments.delta\ndata: {d}\n\ndata: [DONE]\n\n");
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("BLOCKME"));
        assert_held_refusal_frames(&bytes);
    }

    /// #542: a guardrail-blocked request must NOT consume a rate-limit slot.
    /// With RPM=1 and a blocking guardrail, a blocked request followed by a
    /// benign one — the benign request must still succeed (the block didn't
    /// burn the only slot). Pre-fix (guardrail ran after `quota::enforce`) the
    /// block reserved+burned the slot, so the benign request got 429.
    #[tokio::test]
    async fn blocked_request_does_not_consume_rate_limit_slot() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"resp_ok","object":"response",
                "output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}],
                "usage":{"input_tokens":1,"output_tokens":1}
            })))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        // API key capped at RPM=1.
        let apikey: ApiKey = serde_json::from_str(
            r#"{"key_hash":"8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891","allowed_models":["*"],"rate_limit":{"rpm":1}}"#,
        )
        .unwrap();
        snap.apikeys.insert(ResourceEntry::new("k-1", apikey, 1));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));
        let app = build_app(snap);

        // Blocked by the guardrail — must NOT reserve the single RPM slot.
        let blocked = app
            .clone()
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"BLOCKME"}),
            ))
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Benign request — the slot must still be available.
        let ok = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hello"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            ok.status(),
            StatusCode::OK,
            "a guardrail block must not burn the RPM slot (#542)",
        );
    }

    #[tokio::test]
    async fn unauthenticated_returns_401() {
        let snap = new_snap_openai("http://unused");
        let app = build_app(snap);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"model":"m","input":"hi"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_model_returns_404() {
        let snap = new_snap_openai("http://unused");
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "no-such-model",
                "input": "hello"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// #825: an Anthropic-backed model is no longer rejected on
    /// /v1/responses — the request is bridged through ChatFormat to the
    /// Anthropic Messages upstream and the reply is re-encoded into the
    /// Responses-API shape. This is the codex-against-Anthropic path.
    #[tokio::test]
    async fn non_openai_model_bridges_to_responses_shape() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_xprov",
                "type": "message",
                "role": "assistant",
                "model": "claude-3-haiku-20240307",
                "content": [{"type": "text", "text": "Hi from Claude"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 9, "output_tokens": 4}
            })))
            .mount(&upstream)
            .await;

        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("claude-haiku"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "claude-haiku",
                "input": "hello"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["object"], "response");
        assert_eq!(body["status"], "completed");
        // Operator-facing model name echoed, not the upstream id.
        assert_eq!(body["model"], "claude-haiku");
        assert_eq!(body["output"][0]["type"], "message");
        assert_eq!(body["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(body["output"][0]["content"][0]["text"], "Hi from Claude");
        assert_eq!(body["usage"]["input_tokens"], 9);
        assert_eq!(body["usage"]["output_tokens"], 4);
    }

    /// #825 streaming: a streamed Anthropic-backed /v1/responses call emits
    /// the canonical Responses SSE event sequence ending in
    /// `response.completed` (the exact codex-tui path).
    #[tokio::test]
    async fn non_openai_streaming_bridges_to_responses_sse() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-haiku-20240307\",\"content\":[],\"usage\":{\"input_tokens\":6,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("claude-haiku"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "claude-haiku",
                "input": "hi",
                "stream": true
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.contains("event: response.created"),
            "missing created: {text}"
        );
        assert!(text.contains("event: response.output_text.delta"));
        assert!(text.contains("\"delta\":\"Hi\""));
        assert!(text.contains("event: response.completed"));
    }

    #[tokio::test]
    async fn happy_path_forwards_to_upstream() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_abc",
                "object": "response",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "Hi"}]}]
            })))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "Hello"
            })))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "response");
    }

    #[tokio::test]
    async fn upstream_error_returns_502() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "Hello"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    /// Issue #404: a successful non-streaming /v1/responses call must
    /// emit a `UsageEvent` onto the `usage_sink`. Pre-#404 the
    /// responses handler dropped the event entirely, so every
    /// o1/o3/GPT-5 traffic through Responses API was invisible to
    /// the control plane's budget ledger and customer-facing /logs analytics.
    /// This test pins the contract: after a 200 with a real
    /// upstream usage block, exactly one event arrives with the
    /// input_tokens / output_tokens / reasoning_tokens / cached
    /// counters and `inbound_protocol = "openai"`.
    #[tokio::test]
    async fn emits_usage_event_on_200_non_streaming_issue_404() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        // Responses-API wire shape. Pin specific token counts so a
        // regression that swapped semantics (input vs output) or
        // dropped reasoning_tokens would fail here. Mirrors the
        // canonical OpenAI Responses API response object.
        let upstream_body = serde_json::json!({
            "id": "resp-abc",
            "object": "response",
            "model": "gpt-4o-2024-08-06",
            "output": [{
                "type": "message",
                "id": "msg-1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }],
            "usage": {
                "input_tokens": 17,
                "input_tokens_details": {"cached_tokens": 5},
                "output_tokens": 23,
                "output_tokens_details": {"reasoning_tokens": 8},
                "total_tokens": 40
            }
        });
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hello world"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for /v1/responses 200")
            .expect("usage_sink sender dropped");

        assert_eq!(
            event.prompt_tokens, 17,
            "prompt_tokens must mirror upstream usage.input_tokens",
        );
        assert_eq!(
            event.completion_tokens, 23,
            "completion_tokens must mirror upstream usage.output_tokens",
        );
        assert_eq!(
            event.reasoning_tokens, 8,
            "reasoning_tokens must mirror usage.output_tokens_details.reasoning_tokens \
             (o1/o3/GPT-5 class models)",
        );
        assert_eq!(
            event.cached_prompt_tokens, 5,
            "cached_prompt_tokens must mirror usage.input_tokens_details.cached_tokens",
        );
        assert_eq!(event.status_code, 200);
        assert_eq!(event.api_key_id, "k-1");
        assert_eq!(event.model_id, "m-1");
        assert_eq!(event.inbound_protocol, "openai");
        assert!(!event.request_id.is_empty());
        assert!(!event.occurred_at.is_empty());
    }

    /// #1289: `/v1/responses` must record the upstream's own
    /// response object id. Before this it was one of the fields the handler
    /// left at `UsageEvent::default()` ("not yet plumbed for non-chat
    /// handlers"), so a Codex-class call had no id an operator could take to
    /// the provider's console. Fails before the fix (empty), passes after.
    #[tokio::test]
    async fn records_the_provider_response_id_non_streaming_1289() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_1289_nonstream",
                "object": "response",
                "model": "gpt-4o-2024-08-06",
                "output": [],
                "usage": {"input_tokens": 4, "output_tokens": 2}
            })))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));

        let resp = crate::build_router(state)
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hello"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted")
            .expect("usage_sink sender dropped");
        assert_eq!(ev.provider_request_id, "resp_1289_nonstream");
        // The gateway's own id must survive alongside it, not be replaced.
        assert!(!ev.request_id.is_empty());
        assert_ne!(ev.request_id, ev.provider_request_id);
    }

    /// #1289 follow-up: `extract_response_usage` gates on a usable
    /// `usage.input_tokens` and returns `None` without one, so the estimation
    /// fallback (#1074) works off a defaulted `ResponseUsage` — and
    /// used to drop a perfectly good top-level `id` with it. The estimated
    /// record must still name the upstream call it came from. Fails before the
    /// follow-up (empty), passes after.
    #[tokio::test]
    async fn records_the_provider_response_id_when_usage_is_estimated_1289() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_1289_estimated",
                "object": "response",
                "model": "gpt-4o-2024-08-06",
                "output": [{
                    "type": "message",
                    "id": "msg_e",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello there"}]
                }]
                // No `usage` block at all — the estimator fills the counters.
            })))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));

        let resp = crate::build_router(state)
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hello"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted")
            .expect("usage_sink sender dropped");
        assert!(
            ev.usage_estimated,
            "fixture has no usage block, so this must be the estimated path",
        );
        assert_eq!(ev.provider_request_id, "resp_1289_estimated");
    }

    /// The buffered-SSE reader takes the served tier off the terminal
    /// Response snapshot, never off an earlier one.
    #[test]
    fn buffered_sse_tier_is_the_terminal_snapshot_s() {
        let body = b"\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"service_tier\":\"auto\"}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"service_tier\":\"priority\"}}\n\n\
data: [DONE]\n\n";
        assert_eq!(
            super::responses_sse_usage(body)
                .and_then(|u| u.service_tier)
                .as_deref(),
            Some("priority")
        );
        // A non-terminal snapshot can still name the requested tier.
        let created_only = b"\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"service_tier\":\"auto\"}}\n\n";
        assert!(super::responses_sse_usage(created_only).is_none());
    }

    /// The buffered-SSE reader (the output-guardrail path holds the whole
    /// response) must find the id on `response.created` even when no terminal
    /// frame carried usage — that combination produces no `ResponseUsage` at
    /// all, which is exactly where the id used to vanish.
    #[test]
    fn buffered_sse_id_survives_a_stream_with_no_terminal_usage_1289() {
        let body = b"\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1289_buffered\"}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
data: [DONE]\n\n";
        assert!(
            super::responses_sse_usage(body).is_none(),
            "no terminal usage frame — the precondition for the lost-id case",
        );
        assert_eq!(
            super::responses_sse_provider_request_id(body),
            "resp_1289_buffered"
        );
    }

    #[test]
    fn buffered_sse_helpers_accept_bom_lone_cr_and_multi_data() {
        let body = concat!(
            "\u{feff}event: response.created\r",
            "data: {\"type\":\"response.created\",\r",
            "data: \"response\":{\"id\":\"resp_standard_sse\"}}\r\r",
            "event: response.output_text.delta\r",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"joined\"}\r\r",
        );
        assert_eq!(
            super::responses_sse_provider_request_id(body.as_bytes()),
            "resp_standard_sse"
        );
        assert_eq!(super::responses_sse_output_text(body.as_bytes()), "joined");
        assert!(!crate::redact::parse_sse_json_stream(body.as_bytes()).1);
    }

    #[test]
    fn responses_terminal_errors_fail_until_a_semantic_terminal_event() {
        use crate::stream_timeout::RawStreamFailure;

        let error = b"data: {\"type\":\"error\",\"error\":{\"code\":\"server_error\"}}\n\n";
        assert!(matches!(
            super::responses_sse_terminal_failure(error),
            Some(RawStreamFailure::UpstreamInBand { status: None, .. })
        ));

        let completed_then_error = b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n\
data: {\"type\":\"error\",\"error\":{\"code\":\"server_error\"}}\n\n";
        assert_eq!(
            super::responses_sse_terminal_failure(completed_then_error),
            None,
            "transport bytes after response.completed cannot overturn semantic success",
        );
        let completed_then_malformed = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            "data: not-json\n\n",
        );
        assert_eq!(
            super::responses_sse_terminal_failure(completed_then_malformed.as_bytes()),
            None,
            "malformed bytes after response.completed cannot overturn semantic success",
        );
        assert_eq!(
            super::responses_sse_terminal_failure(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"
            ),
            Some(RawStreamFailure::Upstream),
            "EOF without a semantic terminal event is a truncated upstream stream",
        );

        let failed = serde_json::json!({
            "type": "response.failed",
            "response": {
                "status": "failed",
                "error": {"code": "server_error"},
                "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7}
            }
        });
        let usage = super::parse_responses_terminal_usage(&failed).unwrap();
        assert_eq!(usage.completion_tokens, 4);
        assert!(!usage.reached_end, "a failed response is not a healthy end");
    }

    /// 首帧必须记下 `upstream_ttft`（Duration），而且要挺过终止帧对用量的整体
    /// 替换：两条 TTFT 指标只认这个字段，每条原生流都有终止帧，在那一步丢掉
    /// 就一条都不记。立即到达的首帧 `_ms` 为 0，正是 `Option<Duration>` 要区分的情形。
    #[test]
    fn first_frame_ttft_survives_the_terminal_frame() {
        let mut buf = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n"
            .to_vec();
        let mut acc = None;
        let mut failure = None;
        let mut first_frame_seen = false;
        let mut out = Vec::new();
        let terminal = super::drain_responses_sse_frames(
            &mut buf,
            &mut 0,
            &mut acc,
            None,
            &mut failure,
            std::time::Instant::now(),
            &mut first_frame_seen,
            "client-facing",
            &Default::default(),
            &mut out,
        );
        assert!(terminal);
        let usage = acc.expect("both frames parsed");
        assert_eq!(usage.prompt_tokens, 3, "the terminal frame's usage landed");
        assert!(
            usage.upstream_ttft.is_some(),
            "the first frame's upstream_ttft must survive the terminal frame"
        );
    }

    /// 扣留缓冲 fail-open 放行后，前缀会在放行时刻被重新解析；TTFT 必须是扣留时
    /// 量到的首帧时刻，而不是这一刻。首帧解析仍按“首帧”处理（去 BOM），否则
    /// 带 BOM 的 `response.created` 读不出来，它带的 id 就丢了。
    #[tokio::test]
    async fn released_stream_keeps_the_ttft_measured_before_release() {
        use futures::StreamExt;
        use std::sync::{Arc, Mutex};

        let upstream = futures::stream::iter([Ok::<_, crate::stream_timeout::RawStreamError>(
            bytes::Bytes::from_static(
                "\u{FEFF}data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_released\"}}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n"
                    .as_bytes(),
            ),
        )]);
        let captured = Arc::new(Mutex::new(None));
        let captured_out = Arc::clone(&captured);
        let measured = std::time::Duration::from_millis(250);
        let now = std::time::Instant::now();
        let stream = super::build_responses_passthrough_stream(
            upstream,
            now,
            now,
            Some(measured),
            None,
            None,
            true,
            "client-facing".to_string(),
            Default::default(),
            move |usage, _, _, _, _| {
                *captured_out.lock().unwrap() = Some(usage);
            },
        );
        futures::pin_mut!(stream);
        while stream.next().await.is_some() {}

        let usage = captured.lock().unwrap().take().expect("on_complete fired");
        assert_eq!(usage.prompt_tokens, 3, "the terminal frame's usage landed");
        assert_eq!(usage.upstream_ttft, Some(measured));
        assert_eq!(usage.upstream_ttft_ms, 250);
        assert_eq!(
            usage.provider_request_id, "resp_released",
            "the BOM-led first frame still parses as the first frame"
        );
    }

    /// With no scan owed a stream that failed still exports what the caller
    /// received, which is what its failure row reports. A stream that started
    /// unsafe (a scan is owed, or the buffer was released unscanned) stays so.
    #[tokio::test]
    async fn a_failed_stream_exports_what_the_caller_received_only_if_no_scan_was_owed() {
        use futures::StreamExt;
        use std::sync::{Arc, Mutex};

        for initial_capture_safe in [true, false] {
            let upstream = futures::stream::iter([
                Ok::<_, crate::stream_timeout::RawStreamError>(bytes::Bytes::from_static(
                    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                )),
                Err(crate::stream_timeout::RawStreamError::Timeout { elapsed_ms: 300 }),
            ]);
            let captured = Arc::new(Mutex::new(None));
            let captured_out = Arc::clone(&captured);
            let now = std::time::Instant::now();
            let stream = super::build_responses_passthrough_stream(
                upstream,
                now,
                now,
                None,
                Some(1_024),
                None,
                initial_capture_safe,
                "client-facing".to_string(),
                Default::default(),
                move |_, text, _, capture_safe, failure| {
                    *captured_out.lock().unwrap() = Some((text, capture_safe, failure));
                },
            );
            let _: Vec<_> = stream.collect().await;

            let (text, capture_safe, failure) =
                captured.lock().unwrap().take().expect("on_complete fired");
            assert!(failure.is_some());
            assert_eq!(text, "partial");
            assert_eq!(capture_safe, initial_capture_safe);
        }
    }

    /// The sentinel the relay closes a stream with is framed like the stream:
    /// the upstream's line ending survives, so the frame it adds must not bring
    /// another one in.
    #[tokio::test]
    async fn the_closing_done_frame_keeps_the_streams_own_framing() {
        use futures::StreamExt;

        for (terminator, done) in [
            ("\n\n", "data: [DONE]\n\n"),
            ("\r\n\r\n", "data: [DONE]\r\n\r\n"),
            ("\r\r", "data: [DONE]\r\r"),
        ] {
            let terminal = format!(
                "data: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":3,\"output_tokens\":2}}}}}}{terminator}"
            );
            let upstream = futures::stream::iter([Ok::<_, crate::stream_timeout::RawStreamError>(
                bytes::Bytes::from(terminal.clone()),
            )]);
            let now = std::time::Instant::now();
            let stream = super::build_responses_passthrough_stream(
                upstream,
                now,
                now,
                None,
                None,
                None,
                true,
                "client-facing".to_string(),
                Default::default(),
                |_, _, _, _, _| {},
            );
            let wire: Vec<u8> = stream.map(|item| item.unwrap().to_vec()).concat().await;
            assert_eq!(
                String::from_utf8(wire).unwrap(),
                format!("{terminal}{done}")
            );
            assert_eq!(
                super::responses_done_frame(terminal.as_bytes()),
                done.as_bytes()
            );
            assert!(super::responses_sse_has_done(done.as_bytes()), "{done:?}");
        }
        // Nothing framed yet: the framing the gateway writes itself.
        assert_eq!(super::responses_done_frame(b""), b"data: [DONE]\n\n");
    }

    #[tokio::test]
    async fn native_stream_stops_polling_after_response_completed() {
        use futures::StreamExt;
        use std::sync::{Arc, Mutex};

        let upstream = futures::stream::iter([
            Ok::<_, crate::stream_timeout::RawStreamError>(bytes::Bytes::from_static(
                b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n\
event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"AFTER_RESPONSE_COMPLETED\"}\n\n",
            )),
            Err(crate::stream_timeout::RawStreamError::Timeout { elapsed_ms: 300 }),
        ]);
        let captured = Arc::new(Mutex::new(None));
        let captured_out = Arc::clone(&captured);
        let now = std::time::Instant::now();
        let stream = super::build_responses_passthrough_stream(
            upstream,
            now,
            now,
            None,
            None,
            None,
            true,
            "client-facing".to_string(),
            Default::default(),
            move |usage, _, _, _, failure| {
                *captured_out.lock().unwrap() = Some((usage, failure));
            },
        );
        futures::pin_mut!(stream);

        let terminal = stream.next().await.unwrap().unwrap();
        let terminal = String::from_utf8_lossy(&terminal);
        assert!(terminal.contains("response.completed"));
        assert!(!terminal.contains("AFTER_RESPONSE_COMPLETED"));
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"data: [DONE]\n\n"),
        );
        assert!(stream.next().await.is_none());
        let (usage, failure) = captured.lock().unwrap().take().expect("on_complete fired");
        assert!(usage.reached_end);
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 2);
        assert!(failure.is_none());
    }

    /// #1289, streaming: the id arrives on `response.created` and
    /// the terminal `response.completed` need not repeat it — this upstream
    /// deliberately omits it there. A reader that only looked at the terminal
    /// frame, or one that let the terminal frame overwrite what earlier
    /// frames established, records nothing; both are the failure this pins.
    #[tokio::test]
    async fn streaming_records_the_provider_response_id_from_response_created_1289() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let sse_body = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1289_stream\"}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":3}}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));

        let resp = crate::build_router(state)
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi",
                "stream": true
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Draining runs the stream to completion → the end-of-stream emit.
        let _ = to_bytes(resp.into_body(), 65536).await.unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for a streaming 200")
            .expect("usage event sender dropped");
        assert_eq!(ev.prompt_tokens, 9, "terminal-frame usage must still win");
        assert_eq!(
            ev.provider_request_id, "resp_1289_stream",
            "the id seen on response.created must survive the terminal frame",
        );
    }

    /// #867: the verbatim-OpenAI /v1/responses path must apply the
    /// resolved ProviderKey's `request.*` overrides to the outbound call —
    /// both the `default_body_fields` injection (body) and the
    /// `default_headers` injection (header) must reach the upstream. The mock
    /// only matches (200) when BOTH the injected body field AND header are
    /// present, so a 200 proves the overrides were applied. Before the fix the
    /// outbound body/headers carried neither → mock wouldn't match → wiremock
    /// 404 → non-200.
    #[tokio::test]
    async fn responses_verbatim_applies_pk_request_overrides_issue_867() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(body_partial_json(serde_json::json!({"safe_flag": true})))
            .and(header("x-custom", "trace-on"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp-1",
                "object": "response",
                "output": [],
                "usage": {"input_tokens": 3, "output_tokens": 1}
            })))
            .mount(&upstream)
            .await;

        let snap = AisixSnapshot::new();
        snap.provider_keys
            .insert(openai_pk_with_overrides(&upstream.uri()));
        snap.models.insert(openai_model("gpt-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_app(snap);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-resp",
                "input": "hi"
            })))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// #543: an OUTPUT-blocked /v1/responses still records the billed
    /// upstream tokens (the provider already charged), marked
    /// `guardrail_blocked`, with status 422 — NOT a zero-token event. Zeroing
    /// would let the customer's budget ledger underreport spend they paid the
    /// provider for (the output analog of chat.rs's UpstreamCharge).
    #[tokio::test]
    async fn output_block_records_billed_tokens_issue_543() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let upstream_body = serde_json::json!({
            "id": "resp-blk",
            "object": "response",
            "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"sure: BLOCKME"}]}],
            "usage": {"input_tokens": 11, "output_tokens": 7}
        });
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model":"gpt-4o-resp","input":"hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("a UsageEvent must be emitted for an output-blocked response (#543)")
            .expect("usage_sink sender dropped");
        assert_eq!(
            event.prompt_tokens, 11,
            "billed input tokens must be recorded despite the block",
        );
        assert_eq!(
            event.completion_tokens, 7,
            "billed output tokens must be recorded despite the block",
        );
        assert_eq!(event.status_code, 422);
        assert!(
            event.guardrail_blocked,
            "the output-block event must be marked guardrail_blocked",
        );
    }

    /// Companion: an upstream 200 missing the `usage` block entirely
    /// (some relay / compat backends) now emits an ESTIMATED usage
    /// event (#1074) — the call must not stay invisible to
    /// billing. (Pre-#1074 this edge kept the pre-#404 no-emit
    /// behaviour.)
    #[tokio::test]
    async fn estimates_usage_event_when_upstream_omits_usage_block() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        // 200 OK but no `usage` field — should NOT emit.
        let upstream_body = serde_json::json!({
            "id": "resp-abc",
            "object": "response",
            "model": "gpt-4o-2024-08-06",
            "output": []
        });
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hello"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("estimated UsageEvent must be emitted when `usage` is absent")
            .expect("usage_sink sender dropped");
        // input "hello" per the cookbook message scheme: 3 per-message
        // + "user" (1) + "hello" (1) + 3 reply priming = 8. Empty
        // `output` → completion stays 0.
        assert_eq!(event.prompt_tokens, 8);
        assert_eq!(event.completion_tokens, 0);
        assert!(
            event.usage_estimated,
            "locally-counted tokens must be flagged"
        );
    }

    /// 流式 `/v1/responses`（verbatim 透传）必须在流结束后把花费记进花费桶。
    /// 这条路径不走 `MultiReservation::commit`，而是在流结束回调里拿
    /// `spend_keys()` 单独记一笔——删掉那一次调用，token 侧一切照常，
    /// 只有预算悄悄不再增长，没有任何报错。
    ///
    /// 这是 Codex 的流量入口：只测 chat 的话，这条路径会永远绿着而
    /// 预算静默失效。
    #[tokio::test]
    async fn streaming_responses_commits_spend_and_the_next_request_is_refused() {
        let upstream = MockServer::builder().start().await;
        let sse_body = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":500,\"output_tokens\":1000}}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        // 1 USD / 1k token，进出同价 → (500 + 1000) / 1000 = 1.5 USD。
        let model_json = format!(
            r#"{{"display_name":"gpt-4o-resp","provider":"openai","model_name":"gpt-4o",
                 "provider_key_id":"{OPENAI_PK_ID}",
                 "cost":{{"input_per_1k":1.0,"output_per_1k":1.0}}}}"#
        );
        let m: Model = serde_json::from_str(&model_json).unwrap();
        snap.models.insert(ResourceEntry::new("m-1", m, 1));
        snap.apikeys.insert(apikey_entry(&["*"]));
        // 只设花费上限：第二次被拒只可能是花费层干的。窗口取 minute，
        // 好让 `peek` 能回读 tpm_used。
        let policy: aisix_core::models::RateLimitPolicy =
            serde_json::from_value(serde_json::json!({
                "name": "budget",
                "scope": "api_key",
                "scope_ref": "k-1",
                "window": "minute",
                "max_spend_micro_usd": 1_000_000u64,
            }))
            .unwrap();
        snap.rate_limit_policies
            .insert(ResourceEntry::new("pol-spend", policy, 1));

        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg()).without_cache();
        let app = crate::build_router(state.clone());
        let body = serde_json::json!({
            "model": "gpt-4o-resp",
            "input": "hi",
            "stream": true,
        });

        let resp = app.clone().oneshot(make_req(body.clone())).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // 读完 body，让流结束回调触发。
        to_bytes(resp.into_body(), 65536).await.unwrap();

        let spend_limits = aisix_core::RateLimit {
            tpm: Some(1_000_000),
            ..Default::default()
        };
        let status = state
            .limiter
            .peek("spend:api_key:k-1:pol-spend", &spend_limits)
            .await
            .expect("花费桶应已存在");
        assert_eq!(
            status.tpm_used, 1_500_000,
            "花费桶里应是 micro-USD；记成 1500 就是把 token 数当成了钱",
        );

        let resp = app.oneshot(make_req(body)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "流式 /v1/responses 的花费没有提交，预算完全不生效",
        );
    }

    /// A native stream that never reports usage estimates its prompt from the
    /// request, the same count the non-streaming path above gets.
    #[tokio::test]
    async fn estimates_prompt_when_native_stream_omits_usage() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let sse = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-abc\",\"object\":\"response\",\"model\":\"gpt-4o-2024-08-06\",\"output\":[]}}\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));

        let resp = crate::build_router(state)
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hello",
                "stream": true
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        to_bytes(resp.into_body(), 65536).await.unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("usage event was never emitted")
            .expect("usage_sink sender dropped");
        assert_eq!(event.prompt_tokens, 8);
        assert!(event.usage_estimated);
    }

    /// #808: a streaming `/v1/responses` 200 (e.g. all Codex traffic, which
    /// always streams) MUST emit a UsageEvent with the tokens carried on the
    /// terminal `response.completed` event — pre-#808 the streaming path
    /// dropped the event entirely, so successful streamed calls were invisible
    /// to the dashboard Logs and the budget ledger while 4xx/5xx still logged.
    /// Bytes must still pass through verbatim (SSE shape preserved). Fails
    /// before the fix (no event), passes after.
    #[tokio::test]
    async fn streaming_path_emits_usage_event_from_terminal_event_issue_808() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        // Real Responses-API streaming: deltas then a terminal
        // `response.completed` carrying the authoritative `usage` block
        // (nested under `response`, with reasoning + cached sub-counts).
        let sse_body = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":3},\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi",
                "stream": true
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Draining the body runs the stream to completion → the Drop guard
        // fires the end-of-stream emit. Bytes must survive verbatim.
        let body_bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(
            body_bytes.starts_with(b"data: "),
            "SSE shape must pass through verbatim",
        );
        assert!(
            body_bytes.windows(b"[DONE]".len()).any(|w| w == b"[DONE]"),
            "terminal frames must reach the client unchanged",
        );

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for a streaming 200 (#808)")
            .expect("usage event sender dropped");
        assert_eq!(ev.status_code, 200);
        assert_eq!(ev.inbound_protocol, "openai");
        assert_eq!(ev.prompt_tokens, 11);
        assert_eq!(ev.completion_tokens, 7);
        assert_eq!(ev.reasoning_tokens, 3);
        assert_eq!(ev.cached_prompt_tokens, 2);
        assert!(
            rx.try_recv().is_err(),
            "exactly one UsageEvent for a single streamed request",
        );
    }

    /// #867: a streaming `/v1/responses` 200 (every Codex request,
    /// which always streams) MUST carry the resolved ProviderKey's telemetry
    /// attribution tags — provider_kind / provider_featured / branded_provider
    /// / pk_label — exactly like `/v1/messages` and `/v1/chat/completions`.
    /// Pre-fix the responses handler left these at default, so Codex logs were
    /// missing the upstream vendor + PK label that Claude-Code (Anthropic SDK)
    /// logs show. Fails before the fix (empty tags), passes after.
    #[tokio::test]
    async fn streaming_path_emits_provider_telemetry_tags_issue_867() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let sse_body = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai_tagged(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi",
                "stream": true
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = to_bytes(resp.into_body(), 65536).await.unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for a streaming 200")
            .expect("usage event sender dropped");
        assert_eq!(ev.status_code, 200);
        assert_eq!(
            ev.provider_kind, "catalog",
            "provider_kind must mirror the resolved PK's telemetry_tags.kind",
        );
        assert!(
            ev.provider_featured,
            "provider_featured must mirror telemetry_tags.featured",
        );
        assert_eq!(
            ev.branded_provider, "openai",
            "branded_provider must mirror telemetry_tags.branded_provider",
        );
        assert_eq!(
            ev.pk_label, "prod-codex-key",
            "pk_label must mirror telemetry_tags.pk_label",
        );
    }

    /// #867 (non-streaming sibling): the same per-PK telemetry
    /// attribution must land on a non-streaming `/v1/responses` 200, which
    /// emits from the handler via `ResponseDispatchSuccess.provider_key_id`
    /// (a different threading path than the streaming Drop guard above).
    #[tokio::test]
    async fn non_streaming_emits_provider_telemetry_tags_issue_867() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let upstream_body = serde_json::json!({
            "id": "resp-abc",
            "object": "response",
            "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}],
            "usage": {"input_tokens": 17, "output_tokens": 23}
        });
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai_tagged(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hello"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for /v1/responses 200")
            .expect("usage_sink sender dropped");
        assert_eq!(ev.provider_kind, "catalog");
        assert!(ev.provider_featured);
        assert_eq!(ev.branded_provider, "openai");
        assert_eq!(ev.pk_label, "prod-codex-key");
    }

    /// Per #655: a 5xx upstream now emits ONE zero-token UsageEvent for the
    /// failed attempt, so the dashboard's Logs tab surfaces the failure
    /// alongside its siblings. (Pre-#655 the responses handler dropped the
    /// event on the error path — the failed request was invisible.) The
    /// event carries the mapped status (502), zero tokens, an error class,
    /// and the initial-attempt classification.
    #[tokio::test]
    async fn upstream_5xx_emits_zero_token_failed_attempt_event() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal"))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hello"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        // A direct model now carries the deployment default retry budget (2),
        // so a persistent 5xx produces three attempts — initial + two retries
        // — each its own zero-token event. Before the per-model budget landed
        // the knob only existed on a model group, so this was one attempt.
        let mut events = Vec::new();
        for _ in 0..3 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("a failed-attempt UsageEvent must be emitted within the timeout")
                .expect("the usage sink channel must not be closed");
            events.push(ev);
        }
        events.sort_by_key(|e| e.attempt_index);
        for ev in &events {
            assert_eq!(ev.status_code, 502, "failed attempt records the mapped 502");
            assert_eq!(ev.prompt_tokens, 0, "failed attempt has zero tokens");
            assert_eq!(ev.completion_tokens, 0, "failed attempt has zero tokens");
            assert_eq!(
                ev.error_class, "upstream_status",
                "500 upstream maps to an upstream_status error class",
            );
        }
        assert_eq!(
            events
                .iter()
                .map(|e| e.attempt_kind.as_str())
                .collect::<Vec<_>>(),
            vec!["initial", "retry", "retry"],
            "the retries re-hit the SAME model, so they are retries not fallbacks",
        );

        // No further events — the budget is exhausted and there is no
        // separate terminal event (the last attempt itself is terminal).
        let extra = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        if let Ok(Some(ev)) = extra {
            panic!(
                "expected exactly three events for an exhausted budget, got a fourth: \
                 attempt_index={} status_code={}",
                ev.attempt_index, ev.status_code,
            );
        }
    }

    /// A 200 with `usage: {}` (malformed — `input_tokens` is required
    /// by the Responses-API spec) now emits an ESTIMATED usage event
    /// (#1074) instead of dropping the record. Same edge as
    /// the omitted-usage test but the gate is one layer deeper —
    /// `usage` exists but is empty. (Pre-#1074, per issue #404 audit
    /// MEDIUM-1, this dropped the event entirely.)
    #[tokio::test]
    async fn estimates_usage_event_when_usage_block_is_empty() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let upstream_body = serde_json::json!({
            "id": "resp-abc",
            "object": "response",
            "output": [],
            "usage": {}  // malformed — input_tokens required by spec
        });
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("estimated UsageEvent must be emitted for malformed `usage: {}`")
            .expect("usage_sink sender dropped");
        // input "hi" counts per the cookbook message scheme: 3 per-message
        // + "user" (1) + "hi" (1) + 3 reply priming = 8. Empty output → 0.
        assert_eq!(event.prompt_tokens, 8);
        assert_eq!(event.completion_tokens, 0);
        assert!(
            event.usage_estimated,
            "locally-counted tokens must be flagged"
        );
    }

    /// #429 follow-up: a 200 whose `usage` carries
    /// `input_tokens` but omits `output_tokens` is still a real billable
    /// call. It MUST emit a UsageEvent with `completion_tokens = 0`
    /// (coercing the missing side to 0), NOT be dropped. Only a
    /// fully absent / input-less usage block skips.
    #[tokio::test]
    async fn emits_with_zero_output_when_output_tokens_missing() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let upstream_body = serde_json::json!({
            "id": "resp-abc",
            "object": "response",
            "output": [],
            "usage": { "input_tokens": 17 }  // missing output_tokens
        });
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(serde_json::json!({
                "model": "gpt-4o-resp",
                "input": "hi"
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must still be emitted when only output_tokens is missing")
            .expect("usage_sink sender dropped");
        assert_eq!(event.prompt_tokens, 17, "input side must be recorded");
        assert_eq!(
            event.completion_tokens, 0,
            "missing output_tokens must default to 0, not drop the event"
        );
    }

    /// #947: the streamed content capture prefers the terminal
    /// `response.completed` event's full output over accumulated deltas —
    /// the terminal object is authoritative (includes tool-call items the
    /// deltas may have missed).
    #[test]
    fn sse_text_capture_prefers_terminal_full_output() {
        let mut cap = super::SseTextCapture::new(1024);
        cap.observe(&serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "partial "
        }));
        cap.observe(&serde_json::json!({
            "type": "response.completed",
            "response": {
                "output": [
                    {"type": "message", "content": [{"type": "output_text", "text": "the full text"}]}
                ]
            }
        }));
        assert_eq!(cap.into_text(), "the full text");
    }

    /// #947: a stream that aborts before any terminal event falls
    /// back to the concatenated deltas — including tool-call argument deltas,
    /// which stream via their own event type.
    #[test]
    fn sse_text_capture_falls_back_to_deltas_on_abort() {
        let mut cap = super::SseTextCapture::new(1024);
        cap.observe(&serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "hello "
        }));
        cap.observe(&serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "delta": "{\"city\":\"SF\"}"
        }));
        assert_eq!(cap.into_text(), "hello {\"city\":\"SF\"}");
    }

    /// #947: delta accumulation is bounded to the capture cap so
    /// a long stream can't grow the buffer without limit.
    #[test]
    fn sse_text_capture_bounds_delta_accumulation() {
        let mut cap = super::SseTextCapture::new(10);
        for _ in 0..100 {
            cap.observe(&serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "0123456789"
            }));
        }
        assert!(cap.truncated);
        assert_eq!(cap.into_text().len(), 10, "delta buffer must obey the cap");
    }

    #[test]
    fn responses_stream_event_text_reads_every_authoritative_output_carrier() {
        let cases = [
            (
                serde_json::json!({"type":"response.output_text.done","text":"OUTPUT_DONE"}),
                "OUTPUT_DONE",
            ),
            (
                serde_json::json!({"type":"response.refusal.done","refusal":"REFUSAL_DONE"}),
                "REFUSAL_DONE",
            ),
            (
                serde_json::json!({"type":"response.function_call_arguments.done","name":"lookup","arguments":"FUNCTION_DONE"}),
                "FUNCTION_DONE",
            ),
            (
                serde_json::json!({"type":"response.mcp_call_arguments.done","arguments":"MCP_DONE"}),
                "MCP_DONE",
            ),
            (
                serde_json::json!({"type":"response.custom_tool_call_input.done","input":"CUSTOM_DONE"}),
                "CUSTOM_DONE",
            ),
            (
                serde_json::json!({"type":"response.content_part.added","part":{"type":"refusal","refusal":"PART_ADDED"}}),
                "PART_ADDED",
            ),
            (
                serde_json::json!({"type":"response.output_item.done","item":{"type":"message","content":[{"type":"refusal","refusal":"ITEM_DONE"}]}}),
                "ITEM_DONE",
            ),
            (
                serde_json::json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"refusal","refusal":"TERMINAL"}]}]}}),
                "TERMINAL",
            ),
        ];
        for (event, expected) in cases {
            assert!(
                super::responses_stream_event_text(&event).contains(expected),
                "{event}",
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // AISIX-Cloud#1330 / #1024 — `/v1/responses` is the other family the
    // handler-family rule names by hand: it carries Codex traffic, and a
    // chat-only test would stay green forever while this one silently
    // reported nothing.
    // ─────────────────────────────────────────────────────────────────

    /// The same in-process masking row the chat / messages tests use.
    fn masking_guardrail() -> ResourceEntry<aisix_core::models::Guardrail> {
        let row: aisix_core::models::Guardrail = serde_json::from_str(
            r#"{
                "name": "eda-mask",
                "kind": "pii",
                "hook_point": "both",
                "detectors": [],
                "custom_patterns": [
                    {"name": "eda_version", "regex": "version\\s*:\\s*(\\d+(?:\\.\\d+)+)", "action": "mask", "replacement": "***"}
                ]
            }"#,
        )
        .unwrap();
        ResourceEntry::new("g-mask", row, 1)
    }

    /// `spans` is how many times the pattern occurs in what the upstream
    /// actually sent: the Responses streaming wire shape repeats the full
    /// text on the terminal `response.completed` event, so a value that
    /// appears once to a reader is two masked spans on the wire.
    #[track_caller]
    fn assert_masked_by_eda(event: &aisix_obs::UsageEvent, spans: u32) {
        let hits = &event.guardrail_enforced_hits;
        assert!(
            !hits.is_empty(),
            "the enforcing mask left no audit trail on the usage event: {event:?}",
        );
        let hit = hits
            .iter()
            .find(|h| h.hook == "output")
            .unwrap_or_else(|| panic!("no output-hook enforced hit in {hits:?}"));
        assert_eq!(hit.guardrail_name, "eda-mask");
        assert_eq!(hit.action, "masked");
        assert_eq!(hit.counts.get("eda_version").copied(), Some(spans));
        let wire = serde_json::to_string(event).expect("event serialises");
        assert!(
            !wire.contains("9.9.9"),
            "masked value reached the event: {wire}"
        );
    }

    /// Non-streaming `/v1/responses`.
    #[tokio::test]
    async fn responses_usage_event_carries_the_enforced_mask() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp-mask-1",
                "object": "response",
                "model": "gpt-4o-2024-08-06",
                "output": [{
                    "type": "message",
                    "id": "msg-1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "the version: 9.9.9 build"}]
                }],
                "usage": {"input_tokens": 5, "output_tokens": 7, "total_tokens": 12}
            })))
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, masking_guardrail());

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model": "gpt-4o-resp", "input": "hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body =
            String::from_utf8(to_bytes(resp.into_body(), 65536).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("***"), "the response was not masked: {body}");

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("usage event was never emitted")
            .expect("usage_sink sender dropped");
        assert_masked_by_eda(&event, 1);
    }

    /// Streaming `/v1/responses` — pitfall (1) of #1024 on the Codex path.
    #[tokio::test]
    async fn streaming_responses_usage_event_carries_the_enforced_mask() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let sse = "event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"the version: 9.9.9 build\"}\n\n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"the version: 9.9.9 build\"}]}],\"usage\":{\"input_tokens\":5,\"output_tokens\":7,\"total_tokens\":12}}}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&upstream)
            .await;

        let snap = new_snap_openai(&upstream.uri());
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, masking_guardrail());

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model": "gpt-4o-resp", "input": "hi", "stream": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Drain the body so the stream's Drop guard fires the usage event.
        let streamed =
            String::from_utf8(to_bytes(resp.into_body(), 65536).await.unwrap().to_vec()).unwrap();
        assert!(
            streamed.contains("***"),
            "the stream was not masked: {streamed}"
        );
        assert!(
            !streamed.contains("9.9.9"),
            "the stream leaked the value: {streamed}"
        );

        let event = tokio::time::timeout(std::time::Duration::from_millis(1000), rx.recv())
            .await
            .expect("usage event was never emitted")
            .expect("usage_sink sender dropped");
        // Two spans: the delta frame and the terminal event's repeat.
        assert_masked_by_eda(&event, 2);
    }

    /// The live `/v1/responses` relay reads each frame to find the terminal
    /// event's usage. Reading only the FIRST `data:` line left a terminal
    /// frame written over several lines unparseable, and the counters then
    /// came from the local token estimator instead of the provider — which
    /// is invisible unless the assertion pins the exact numbers, because
    /// "usage exists" is true either way.
    #[test]
    fn drain_reads_a_terminal_frame_spread_over_several_data_lines() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_ml\"}}\n\n",
        );
        // ONE frame, ONE JSON document, three `data:` lines.
        buf.extend_from_slice(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\ndata: \"response\":{\"id\":\"resp_ml\",\"usage\":\n\
              data: {\"input_tokens\":4321,\"output_tokens\":765,\"total_tokens\":5086}}}\n\n",
        );
        let mut acc = None;
        let mut failure = None;
        let mut out = Vec::new();
        let mut first = false;
        super::drain_responses_sse_frames(
            &mut buf,
            &mut 0,
            &mut acc,
            None,
            &mut failure,
            std::time::Instant::now(),
            &mut first,
            "client-facing",
            &Default::default(),
            &mut out,
        );
        let usage = acc.expect("the terminal frame must be parsed");
        assert_eq!(usage.prompt_tokens, 4321);
        assert_eq!(usage.completion_tokens, 765);
        assert_eq!(usage.provider_request_id, "resp_ml");
    }

    /// The same drain on a CRLF-framed stream with no `[DONE]` sentinel —
    /// what OpenAI's Responses API actually sends — must read identically.
    #[test]
    fn drain_reads_a_crlf_framed_stream_with_no_done_sentinel() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b": keep-alive\r\n\r\n");
        buf.extend_from_slice(
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_crlf\",\"usage\":{\"input_tokens\":11,\"output_tokens\":3,\"total_tokens\":14}}}\r\n\r\n",
        );
        let mut acc = None;
        let mut failure = None;
        let mut out = Vec::new();
        let mut first = false;
        super::drain_responses_sse_frames(
            &mut buf,
            &mut 0,
            &mut acc,
            None,
            &mut failure,
            std::time::Instant::now(),
            &mut first,
            "client-facing",
            &Default::default(),
            &mut out,
        );
        let usage = acc.expect("a CRLF terminal frame must be parsed");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 3);
        assert!(buf.is_empty(), "both complete frames were drained");
    }

    /// Generated reasoning is out of the output-guardrail scope. A
    /// `reasoning` item DOES carry `content[]` with `text` parts, and the
    /// walk reads `content` off every item regardless of type — so without
    /// an explicit skip the model's own reasoning reached the output scan
    /// on this surface while the chat and `/v1/messages` surfaces excluded
    /// it.
    #[test]
    fn responses_output_scan_excludes_generated_reasoning() {
        let resp = serde_json::json!({
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "SUMMARYSECRET"}],
                    "content": [{"type": "reasoning_text", "text": "REASONINGSECRET"}]
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "the visible answer"}]
                },
                {"type": "function_call", "name": "lookup", "arguments": "{\"q\":\"argtext\"}"}
            ]
        });
        let scanned = super::responses_output_text(&resp);
        assert!(!scanned.contains("REASONINGSECRET"), "got {scanned:?}");
        assert!(!scanned.contains("SUMMARYSECRET"), "got {scanned:?}");
        // The tool-call coverage the walk exists for is untouched.
        assert!(scanned.contains("the visible answer"));
        assert!(scanned.contains("lookup") && scanned.contains("argtext"));
    }

    /// Tool-call ids are opaque handles: neither a replayed call's
    /// `call_id` nor a result's reaches the scan, while the call's name and
    /// arguments do.
    #[test]
    fn responses_input_scan_leaves_out_call_ids() {
        let item = serde_json::json!({
            "type": "function_call", "id": "fc_ITEMID", "call_id": "call_BASEID__thought__SIGNATURE",
            "name": "lookup", "arguments": "{\"q\":\"argtext\"}"
        });
        let scanned = super::responses_item_text(&item);
        assert!(
            !scanned.contains("BASEID")
                && !scanned.contains("SIGNATURE")
                && !scanned.contains("ITEMID"),
            "got {scanned:?}"
        );
        assert!(
            scanned.contains("lookup") && scanned.contains("argtext"),
            "got {scanned:?}"
        );
        let output = serde_json::json!({
            "type": "function_call_output", "call_id": "call_BASEID", "output": "ok"
        });
        assert!(!super::responses_item_text(&output).contains("call_BASEID"));
    }

    /// The request side is the mirror: a `reasoning` item replayed by the
    /// caller is text entering the model, and both slots the mask rewrites
    /// have to be slots the scan reads.
    #[test]
    fn responses_input_scan_covers_replayed_reasoning_content_and_summary() {
        let item = serde_json::json!({
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "SUMMARYSECRET"}],
            "content": [{"type": "reasoning_text", "text": "REASONINGSECRET"}]
        });
        let scanned = super::responses_item_text(&item);
        assert!(scanned.contains("REASONINGSECRET"), "got {scanned:?}");
        assert!(scanned.contains("SUMMARYSECRET"), "got {scanned:?}");
    }

    /// A streamed `/v1/responses` relay writes ONE access-log line, at the
    /// stream's end rather than when the head went out, so each of the three
    /// endings reports its own outcome (AISIX-Cloud#1571).
    ///
    /// The upstream is a real chunked SSE server rather than a canned body:
    /// this family relays BYTES, so an upstream that answers in one chunk
    /// would hand the caller the whole stream in a single frame and the
    /// "walked away mid-stream" ending could not happen at all.
    #[tokio::test]
    async fn a_streamed_relay_writes_one_line_per_stream_ending() {
        let upstream = crate::test_log::spawn_sse_upstream(vec![
            "event: response.output_text.delta\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"a \"}\n\n"
                .to_string(),
            "event: response.output_text.delta\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"clean answer\"}\n\n"
                .to_string(),
            "event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream\",\
             \"usage\":{\"input_tokens\":5,\"output_tokens\":2,\"total_tokens\":7}}}\n\n"
                .to_string(),
            "data: [DONE]\n\n".to_string(),
        ])
        .await;

        let snap = new_snap_openai(&upstream);
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg()).without_cache();
        let metrics = state.metrics.clone();
        let app = crate::build_router(state);

        let endings = crate::test_log::three_stream_endings(app, || {
            make_req(serde_json::json!({"model":"gpt-4o-resp","input":"hi","stream":true}))
        })
        .await;
        crate::test_log::assert_one_line_per_ending(&endings, "/v1/responses", "k-1");
        crate::test_log::assert_latency_is_time_to_first_token(&endings);
        crate::test_log::assert_request_families_follow_the_endings(
            &metrics,
            "/v1/responses",
            true,
        );
        assert_eq!(
            endings.delivered.field("provider_request_id").as_deref(),
            Some("resp_stream"),
            "the id rides the terminal frame, which only the end-of-stream line can see",
        );
    }

    // ── Platform mode: a response id belongs to the tenant it was handed to ──

    use crate::error::ProxyError;
    use axum::response::IntoResponse as _;
    use serde_json::Value;

    const PLATFORM_SECRET: &str = "responses-tenant-secret";
    /// The tenant `team-a`'s keys belong to.
    const TENANT_A: &str = "team:team-a";
    /// The plaintext of tenant B's key; tenant A's is `make_req`'s.
    const TENANT_B_PLAINTEXT: &str = "sk-tenant-b";

    fn tenant_key(id: &str, plaintext: &str, extra: serde_json::Value) -> ResourceEntry<ApiKey> {
        use sha2::Digest as _;
        let mut doc = serde_json::json!({
            "key_hash": sha2::Sha256::digest(plaintext.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "allowed_models": ["*"],
        });
        if let (Some(doc), Some(extra)) = (doc.as_object_mut(), extra.as_object()) {
            doc.extend(extra.clone());
        }
        ResourceEntry::new(id, serde_json::from_value::<ApiKey>(doc).unwrap(), 1)
    }

    fn build_platform_app(snap: AisixSnapshot) -> axum::Router {
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        crate::build_router(
            crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg())
                .without_cache()
                .with_platform(crate::platform::PlatformSettings::from_secret(
                    PLATFORM_SECRET,
                )),
        )
    }

    fn response_key() -> [u8; 32] {
        crate::responses_affinity::response_id_key(
            &crate::platform::PlatformSettings::from_secret(PLATFORM_SECRET).id_key,
        )
    }

    fn request_as(plaintext: &str, body: serde_json::Value) -> Request<axum::body::Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", format!("Bearer {plaintext}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }

    /// Tenant A (`team-a`, `make_req`'s key) and tenant B (`team-b`) on one
    /// OpenAI target, `m-1`.
    fn two_tenants(api_base: &str) -> AisixSnapshot {
        let snap = new_snap_openai(api_base);
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.apikeys.insert(tenant_key(
            "k-a",
            "sk-caller",
            serde_json::json!({"team_id": "team-a"}),
        ));
        snap.apikeys.insert(tenant_key(
            "k-b",
            TENANT_B_PLAINTEXT,
            serde_json::json!({"team_id": "team-b"}),
        ));
        snap
    }

    async fn status_and_body(resp: axum::response::Response) -> (StatusCode, bytes::Bytes) {
        let status = resp.status();
        (status, to_bytes(resp.into_body(), 65536).await.unwrap())
    }

    /// What platform mode answers when an upstream says it does not have
    /// the previous response: a 400 naming `previous_response_id`
    /// (<https://developers.openai.com/api/docs/guides/websocket-mode>).
    async fn platform_not_found() -> (StatusCode, bytes::Bytes) {
        let resp = crate::error::tests::in_platform_mode(|| {
            let message = "Previous response with id 'resp_x' not found.";
            ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamStatus {
                status: 400,
                message: message.into(),
                parsed: Some(Box::new(aisix_gateway::UpstreamErrorView {
                    kind: Some("invalid_request_error".into()),
                    message: Some(message.into()),
                    code: Some("previous_response_not_found".into()),
                    param: Some("previous_response_id".into()),
                })),
                wire: aisix_gateway::UpstreamWire::OpenAI,
                retry_after: None,
                diagnostic: None,
            })
            .into_response()
        });
        status_and_body(resp).await
    }

    /// An item reference that is not the caller's own seal is refused as a
    /// reference to an object stored upstream, naming the field, and never
    /// reaches the upstream; the tenant's own reaches it as the upstream's id.
    #[tokio::test]
    async fn platform_mode_refuses_an_item_reference_that_is_not_the_callers_own() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_next", "object": "response", "output": []
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let app = build_platform_app(two_tenants(&upstream.uri()));
        let item_key = crate::opaque_id::seal_key(
            &crate::platform::PlatformSettings::from_secret(PLATFORM_SECRET).id_key,
        );
        let sealed = crate::opaque_id::seal_id(&item_key, TENANT_A, "msg_up_1");
        let referencing = |id: &str| {
            serde_json::json!({"model": "gpt-4o-resp", "input": [
                {"role": "user", "content": "go on"},
                {"type": "item_reference", "id": id},
            ]})
        };

        for (key, id) in [
            (TENANT_B_PLAINTEXT, sealed.as_str()),
            ("sk-caller", "msg_up_1"),
        ] {
            let resp = app
                .clone()
                .oneshot(request_as(key, referencing(id)))
                .await
                .unwrap();
            let (status, body) = status_and_body(resp).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{id}");
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["param"], "input[1].id", "{body}");
            assert!(!body.to_string().contains(id), "{body}");
        }

        // A decoy beside the reference that does open, the caller's own call
        // seal, is no warrant for the reference's own id.
        let own_call = crate::opaque_id::seal_call(&item_key, TENANT_A, "call_up_1");
        for decoy in [
            serde_json::json!({"type": "item_reference", "id": "msg_up_1", "call_id": own_call}),
            serde_json::json!({"id": "msg_up_1", "caller": {"type": "program", "caller_id": own_call}}),
        ] {
            let resp = app
                .clone()
                .oneshot(request_as(
                    "sk-caller",
                    serde_json::json!({"model": "gpt-4o-resp", "input": [
                        {"role": "user", "content": "go on"},
                        decoy,
                    ]}),
                ))
                .await
                .unwrap();
            let (status, body) = status_and_body(resp).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["param"], "input[1].id", "{body}");
        }
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "no refused request reaches the upstream"
        );

        let resp = app
            .oneshot(request_as("sk-caller", referencing(&sealed)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sent: Value =
            serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent["input"][1]["id"], "msg_up_1");
    }

    #[tokio::test]
    async fn platform_mode_answers_another_tenants_response_id_as_one_that_does_not_exist() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_next", "object": "response", "output": []
            })))
            .expect(0)
            .mount(&upstream)
            .await;
        let app = build_platform_app(two_tenants(&upstream.uri()));
        let not_found = platform_not_found().await;
        assert_eq!(not_found.0, StatusCode::BAD_REQUEST);

        let tenant_a_id = crate::responses_affinity::seal_response_id(
            &response_key(),
            TENANT_A,
            "m-1",
            "resp_up",
        );
        for previous in [
            tenant_a_id.as_str(),
            // An id nobody was handed, and the upstream's own id unsealed.
            "resp_0123456789abcdef0123456789abcdef",
            "resp_up",
        ] {
            let resp = app
                .clone()
                .oneshot(request_as(
                    TENANT_B_PLAINTEXT,
                    serde_json::json!({"model": "gpt-4o-resp", "input": "hi",
                                       "previous_response_id": previous}),
                ))
                .await
                .unwrap();
            assert_eq!(status_and_body(resp).await, not_found, "{previous}");
        }
        // Streamed requests are answered the same way.
        let resp = app
            .oneshot(request_as(
                TENANT_B_PLAINTEXT,
                serde_json::json!({"model": "gpt-4o-resp", "input": "hi", "stream": true,
                                   "previous_response_id": tenant_a_id}),
            ))
            .await
            .unwrap();
        assert_eq!(status_and_body(resp).await, not_found);
    }

    #[tokio::test]
    async fn platform_mode_continues_a_response_for_the_tenant_it_was_handed_to() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(body_partial_json(
                serde_json::json!({"previous_response_id": "resp_up"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_bdrk_next", "object": "response",
                "previous_response_id": "resp_up", "output": []
            })))
            .expect(2)
            .mount(&upstream)
            .await;
        let snap = two_tenants(&upstream.uri());
        // Another key of tenant A continues too: the id is the tenant's.
        snap.apikeys.insert(tenant_key(
            "k-a2",
            "sk-tenant-a-second",
            serde_json::json!({"team_id": "team-a"}),
        ));
        let app = build_platform_app(snap);
        let sent = crate::responses_affinity::seal_response_id(
            &response_key(),
            TENANT_A,
            "m-1",
            "resp_up",
        );

        for plaintext in ["sk-caller", "sk-tenant-a-second"] {
            let resp = app
                .clone()
                .oneshot(request_as(
                    plaintext,
                    serde_json::json!({"model": "gpt-4o-resp", "input": "go on",
                                       "previous_response_id": sent}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let v: serde_json::Value =
                serde_json::from_slice(&to_bytes(resp.into_body(), 65536).await.unwrap()).unwrap();
            assert_eq!(v["previous_response_id"], sent.as_str());
            let id = v["id"].as_str().unwrap();
            assert!(!id.contains("resp_bdrk_next"), "{id}");
            assert_eq!(
                crate::responses_affinity::open_response_id(&response_key(), TENANT_A, id),
                Some(("m-1".into(), "resp_bdrk_next".into()))
            );
        }
    }

    #[tokio::test]
    async fn platform_mode_seals_every_frame_of_a_stream_to_one_id_of_the_tenant() {
        let upstream = MockServer::builder().start().await;
        let sse = "event: response.created\n\
                   data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_bdrk_s\",\"status\":\"in_progress\",\"output\":[]}}\n\n\
                   event: response.in_progress\n\
                   data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_bdrk_s\",\"status\":\"in_progress\",\"output\":[]}}\n\n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_bdrk_s\",\"status\":\"completed\",\"output\":[]}}\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&upstream)
            .await;
        // Relayed live, and held back for an output scan.
        for guarded in [false, true] {
            let snap = two_tenants(&upstream.uri());
            if guarded {
                crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("BLOCKME"));
            }
            let app = build_platform_app(snap);
            let resp = app
                .oneshot(make_req(
                    serde_json::json!({"model": "gpt-4o-resp", "input": "hi", "stream": true}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = String::from_utf8(to_bytes(resp.into_body(), 65536).await.unwrap().to_vec())
                .unwrap();
            assert!(!body.contains("resp_bdrk_s"), "{body}");
            let ids: Vec<String> = crate::redact::parse_sse_json_stream(body.as_bytes())
                .0
                .iter()
                .filter_map(|e| e.pointer("/response/id").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            assert_eq!(ids.len(), 3, "{body}");
            assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
            assert_eq!(
                crate::responses_affinity::open_response_id(&response_key(), TENANT_A, &ids[0]),
                Some(("m-1".into(), "resp_bdrk_s".into())),
                "guarded: {guarded}"
            );
        }
    }

    #[tokio::test]
    async fn platform_mode_seals_a_translated_answers_id_to_the_tenant() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_bdrk_1", "type": "message", "role": "assistant",
                "model": "claude-3-haiku-20240307",
                "content": [{"type": "text", "text": "hi"}],
                "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1}
            })))
            .mount(&upstream)
            .await;
        let snap = new_snap_anthropic_at(&upstream.uri());
        snap.models.insert(anthropic_model("claude-resp"));
        snap.apikeys.insert(tenant_key(
            "k-a",
            "sk-caller",
            serde_json::json!({"team_id": "team-a"}),
        ));
        let app = build_platform_app(snap);
        let resp = app
            .clone()
            .oneshot(make_req(
                serde_json::json!({"model": "claude-resp", "input": "hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value =
            serde_json::from_slice(&to_bytes(resp.into_body(), 65536).await.unwrap()).unwrap();
        let (target, minted) = crate::responses_affinity::open_response_id(
            &response_key(),
            TENANT_A,
            v["id"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(target, "m-2");
        // Sealed once: what the seal holds is the gateway's own minted id,
        // not another seal.
        assert_eq!(
            crate::responses_affinity::open_response_id(&response_key(), TENANT_A, &minted),
            None,
            "{minted}"
        );
        assert!(
            minted
                .strip_prefix("resp_")
                .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit())),
            "{minted}"
        );

        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model": "claude-resp", "input": "hi", "stream": true}),
            ))
            .await
            .unwrap();
        let body =
            String::from_utf8(to_bytes(resp.into_body(), 65536).await.unwrap().to_vec()).unwrap();
        let ids: Vec<String> = crate::redact::parse_sse_json_stream(body.as_bytes())
            .0
            .iter()
            .filter_map(|e| e.pointer("/response/id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        assert!(ids.len() >= 2, "{body}");
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
        let (target, minted) =
            crate::responses_affinity::open_response_id(&response_key(), TENANT_A, &ids[0])
                .unwrap();
        assert_eq!(target, "m-2");
        assert_eq!(
            crate::responses_affinity::open_response_id(&response_key(), TENANT_A, &minted),
            None,
            "{minted}"
        );
    }

    /// A key restricted to provider groups continues only through a target
    /// its groups still hold; anything else is the unknown-response answer,
    /// and no upstream hears of it.
    #[tokio::test]
    async fn platform_mode_answers_a_continuation_outside_the_keys_groups_as_unknown() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_next", "object": "response", "output": []
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        // `m-1` is on the provider key of group `ga`; `m-3`, which the key
        // still reaches, on one of group `gb` at the same upstream.
        let snap = AisixSnapshot::new();
        let pk = |id: &str, group: &str| {
            let doc = serde_json::json!({"display_name": format!("pk-{group}"), "secret": "sk",
                "api_base": upstream.uri(), "provider": "openai", "adapter": "openai",
                "groups": [group]});
            ResourceEntry::new(
                id,
                serde_json::from_value::<aisix_core::ProviderKey>(doc).unwrap(),
                1,
            )
        };
        snap.provider_keys.insert(pk(OPENAI_PK_ID, "ga"));
        snap.provider_keys.insert(pk("pk-gb", "gb"));
        snap.models.insert(openai_model("gpt-4o-resp"));
        let other: Model = serde_json::from_value(serde_json::json!({
            "display_name": "gpt-4o-gb", "provider": "openai", "model_name": "gpt-4o",
            "provider_key_id": "pk-gb"}))
        .unwrap();
        snap.models.insert(ResourceEntry::new("m-3", other, 1));
        snap.apikeys.insert(tenant_key(
            "k-a",
            "sk-caller",
            serde_json::json!({"team_id": "team-a", "allowed_provider_groups": ["gb"]}),
        ));
        let app = build_platform_app(snap);
        let from_ga = crate::responses_affinity::seal_response_id(
            &response_key(),
            TENANT_A,
            "m-1",
            "resp_up",
        );
        let resp = app
            .clone()
            .oneshot(make_req(
                serde_json::json!({"model": "gpt-4o-gb", "input": "go on",
                                                 "previous_response_id": from_ga}),
            ))
            .await
            .unwrap();
        assert_eq!(status_and_body(resp).await, platform_not_found().await);

        // Inside its groups the same key continues.
        let from_gb = crate::responses_affinity::seal_response_id(
            &response_key(),
            TENANT_A,
            "m-3",
            "resp_up",
        );
        let resp = app
            .oneshot(make_req(
                serde_json::json!({"model": "gpt-4o-gb", "input": "go on",
                                                 "previous_response_id": from_gb}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A native stream and a bridged one go out under the same headers in
    /// platform mode: a header only one of them sends tells a tenant which
    /// kind of channel served it.
    #[tokio::test]
    async fn platform_mode_sends_native_and_bridged_streams_under_one_header_set() {
        let upstream = MockServer::builder().start().await;
        let responses_sse = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_r\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n";
        let messages_sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-haiku-20240307\",\"content\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n\
            event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
            event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
            event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
            event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n\
            event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        for (route, sse) in [
            ("/v1/responses", responses_sse),
            ("/v1/messages", messages_sse),
        ] {
            Mock::given(method("POST"))
                .and(path(route))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(sse, "text/event-stream; charset=utf-8"),
                )
                .mount(&upstream)
                .await;
        }
        let snap = new_snap_openai(&upstream.uri());
        snap.provider_keys.insert(anthropic_pk_at(&upstream.uri()));
        snap.models.insert(openai_model("gpt-4o-resp"));
        snap.models.insert(anthropic_model("claude-resp"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        let app = build_platform_app(snap);
        let mut sets = Vec::new();
        for model in ["gpt-4o-resp", "claude-resp"] {
            let resp = app
                .clone()
                .oneshot(make_req(
                    serde_json::json!({"model": model, "input": "hi", "stream": true}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{model}");
            let mut set: Vec<(String, String)> = resp
                .headers()
                .iter()
                .map(|(name, value)| {
                    let value = if name == "x-aisix-request-id" {
                        String::new()
                    } else {
                        value.to_str().unwrap().to_owned()
                    };
                    (name.to_string(), value)
                })
                .collect();
            set.sort();
            let _ = to_bytes(resp.into_body(), 65536).await.unwrap();
            sets.push(set);
        }
        assert_eq!(
            sets[0], sets[1],
            "native {:?}, bridged {:?}",
            sets[0], sets[1]
        );
        assert!(sets[0].contains(&("cache-control".to_owned(), "no-cache".to_owned())));
    }

    /// A transport failure mid-stream aborts a native Responses body; in
    /// platform mode the stream ends on its class instead, an event a
    /// bridged channel's stream would end on as well.
    #[tokio::test]
    async fn in_platform_mode_a_mid_stream_transport_error_ends_on_its_class() {
        use futures::StreamExt;
        for platform in [false, true] {
            let upstream = futures::stream::iter(vec![
                Ok(bytes::Bytes::from_static(
                    b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
                )),
                Err(crate::stream_timeout::RawStreamError::Timeout { elapsed_ms: 5 }),
            ]);
            let now = std::time::Instant::now();
            let mark = crate::responses_affinity::ContextAffinity::default()
                .response_mark("m-1", false)
                .sealing_ids(
                    platform
                        .then(|| crate::platform::PlatformSettings::from_secret("s"))
                        .as_ref(),
                    "team-a",
                );
            let cell = std::sync::Arc::new(
                crate::attribution::RequestAttribution::default().in_platform_mode(platform),
            );
            let (wire, aborted) = crate::attribution::scope(cell, async move {
                let stream = super::build_responses_passthrough_stream(
                    upstream,
                    now,
                    now,
                    None,
                    None,
                    None,
                    true,
                    "alias".into(),
                    mark,
                    |_, _, _, _, _| {},
                );
                futures::pin_mut!(stream);
                let mut wire = Vec::new();
                let mut aborted = false;
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(bytes) => wire.extend_from_slice(&bytes),
                        Err(_) => aborted = true,
                    }
                }
                (String::from_utf8(wire).unwrap(), aborted)
            })
            .await;
            if !platform {
                assert!(aborted, "off platform mode the body is aborted");
                continue;
            }
            assert!(!aborted);
            let last = wire.trim_end().rsplit("\n\n").next().unwrap();
            let (event, _) = crate::redact::parse_sse_json_event(last.as_bytes());
            let event = event.unwrap();
            assert_eq!(event["type"], "error", "{last}");
            assert_eq!(event["code"], "upstream_unavailable", "{last}");
        }
    }

    /// A relayed event stream goes out under the upstream's own content
    /// type, and in platform mode under a fixed `text/event-stream`: how an
    /// upstream spells its content type differs by channel. Both the live
    /// relay and the stream held back for an output guardrail.
    #[tokio::test]
    async fn platform_mode_relays_a_stream_under_a_fixed_content_type() {
        let sse = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_r\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n";
        for held in [false, true] {
            for (platform, expected) in [
                (false, "text/event-stream; charset=utf-8"),
                (true, "text/event-stream"),
            ] {
                let upstream = MockServer::builder().start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/responses"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_raw(sse, "text/event-stream; charset=utf-8"),
                    )
                    .mount(&upstream)
                    .await;
                let snap = new_snap_openai(&upstream.uri());
                snap.models.insert(openai_model("gpt-4o-resp"));
                snap.apikeys.insert(apikey_entry(&["*"]));
                if held {
                    crate::seed_env_scoped_guardrail(&snap, keyword_output_guardrail("NEVER-SEEN"));
                }
                let hub = Arc::new(Hub::new());
                hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
                let mut state =
                    crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg()).without_cache();
                if platform {
                    state =
                        state.with_platform(crate::platform::PlatformSettings::from_secret("s"));
                }
                let resp = crate::build_router(state)
                    .oneshot(make_req(
                        serde_json::json!({"model": "gpt-4o-resp", "input": "hi", "stream": true}),
                    ))
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
                assert_eq!(
                    resp.headers()[axum::http::header::CONTENT_TYPE],
                    expected,
                    "platform mode {platform}, held {held}"
                );
                let body = to_bytes(resp.into_body(), 65536).await.unwrap();
                assert!(
                    String::from_utf8_lossy(&body).contains("response.completed"),
                    "{body:?}"
                );
            }
        }
    }
}
