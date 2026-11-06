//! `POST /v1/images/generations` — image generation pass-through.
//!
//! Flow:
//! 1. [`AuthenticatedKey`] extractor — 401 if auth fails.
//! 2. Parse the body as a JSON object.
//! 3. Validate `model` field is present.
//! 4. Resolve model name → `Model` in snapshot → 404 if absent.
//! 5. Check `allowed_models` → 403 if denied.
//! 6. Look up Bridge on Hub → 503 if not registered.
//! 7. Call `bridge.generate_image(body, ctx)` → JSON response.
//! 8. Providers that don't support image generation return 501.

use aisix_core::AppliedGuardrail;
use aisix_gateway::{BridgeCapability, BridgeError};
use aisix_obs::{content_capture_cap, AccessLog, CapturedContent, UsageEvent};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::{ErrorEnvelope, ProxyError};
use crate::state::ProxyState;

/// Per-request payload from a successful dispatch — carries the
/// response + the bits the handler needs to emit a UsageEvent (#407).
struct ImageDispatchSuccess {
    response: Response,
    provider: String,
    /// UUID of the resolved Model row — required for UsageEvent
    /// `model_id`. Always present on success.
    model_id: String,
    /// Resolved ProviderKey UUID — feeds per-PK telemetry attribution
    /// (#867 parity).
    provider_key_id: String,
    /// Provider-side model name, for the `upstream_model` metric label
    /// (#1234 parity with chat / messages / responses).
    upstream_model: String,
    /// The `{kind, hook}` set of guardrails that governed this request (#379
    /// parity) — surfaced on the emitted UsageEvent.
    applied_guardrails: Vec<AppliedGuardrail>,
    /// The upstream `usage` block when the model returns one
    /// (gpt-image-1). `None` for models that don't (dall-e-3) — those
    /// still emit a zero-token event so the request is visible +
    /// attributed.
    usage: Option<aisix_gateway::UsageStats>,
    /// `false` on the 501 NotImplemented branch (provider lacks image
    /// generation → no upstream call). That path still emits a zero-token
    /// event.
    upstream_called: bool,
    /// Per-detector PII mask counts (#932/#696) applied to the prompt.
    /// Attached to the emitted UsageEvent. Empty = no redaction.
    redactions: crate::redact::RedactionCounts,
    /// Monitor-mode guardrail observations (#562).
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    /// True when an output guardrail replaced the upstream success with 422.
    guardrail_blocked: bool,
    /// Captured request/response content for content-capturing exporters
    /// (#700: the image response JSON with its base64 replaced by a
    /// placeholder, truncated at the capture cap). `Some` only when an
    /// exporter opted into `content_mode = full`.
    captured_content: Option<CapturedContent>,
}

pub async fn image_generations(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    // Result-wrapped so an extractor-layer 413 maps to the OpenAI
    // envelope — see completions.rs.
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let started = Instant::now();
    let Json(body) = match body {
        Ok(json) => json,
        // Answer through `reject` — see completions.rs.
        Err(rej) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                "/v1/images/generations",
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_json_rejection(
                    rej,
                    state.request_body_limit_for("/v1/images/generations"),
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

    // One snapshot for the whole request (#941) — see `embeddings`.
    let snapshot = state.snapshot.load();

    // See `embeddings`: filled inside `dispatch` so the failure branch —
    // where a guardrail block lands — stamps the enforced hits too
    // (AISIX-Cloud#1330 / #1024).
    let mut audit = crate::usage_attr::GuardrailAudit::default();
    // Every upstream attempt, filled by the dispatch (#655).
    let mut routing = crate::attempt::RoutingTelemetry::default();
    match dispatch(
        &state,
        &snapshot,
        &auth,
        body,
        &request_id,
        &client,
        &mut audit,
        &mut routing,
    )
    .await
    {
        Ok(success) => {
            let elapsed = started.elapsed();
            // The actual response status, not a hardcoded 200: the 501
            // NotImplemented branch also returns `Ok(success)`, and calling
            // it a 200 both mislabels the access log and books it as
            // `outcome="success"` on the request metrics. Same fix #426 made
            // for completions / responses / rerank.
            let status = success.response.status().as_u16();
            emit_access_log(
                "/v1/images/generations",
                &model_name,
                &success.provider,
                &api_key_id,
                status,
                elapsed,
                &request_id,
                &routing,
                None,
            );
            // One ProviderKey lookup for the metric emit + the usage event
            // below (#941).
            let pk = crate::usage_attr::ResolvedPk::resolve(&snapshot, &success.provider_key_id);
            crate::request_metrics::record(
                &state,
                "/v1/images/generations",
                crate::request_metrics::Caller::new(&auth),
                crate::request_metrics::Upstream {
                    provider: &success.provider,
                    model: &model_name,
                    upstream_model: &success.upstream_model,
                    pk: pk.labels(),
                    is_fallback: routing.fallback_count() > 0,
                    ..Default::default()
                },
                status,
                crate::attempt::answered_error_source(false, &routing),
                elapsed,
            );
            // One zero-token event per attempt that failed first (#655).
            // The route's own 501 is the terminal event below.
            let superseded = crate::usage_attr::split_route_refusal(&routing);
            crate::usage_attr::emit_failed_attempts(
                &state,
                &snapshot,
                crate::operation::IMAGE_GENERATION,
                &request_id,
                &model_name,
                &api_key_id,
                &client,
                &success.applied_guardrails,
                superseded,
                false,
                false,
                success.monitor_hits.clone(),
                success.redactions.clone(),
                &audit,
            );
            // Issue #407: emit UsageEvent so the control plane's budget ledger +
            // /logs see image-generation traffic. Pre-#407 the handler
            // dropped the event entirely. Every request emits, the route's
            // own 501 included, at zero tokens when there are none to
            // report. Tokens come from the upstream
            // `usage` block when present (gpt-image-1); dall-e-3 has no
            // usage block → zero tokens (precise per-image cost is a
            // documented cross-repo follow-up — needs image-count /
            // size / quality on the wire + the control plane pricing).
            {
                emit_usage_event(
                    &state,
                    &snapshot,
                    &pk,
                    "/v1/images/generations",
                    crate::operation::IMAGE_GENERATION,
                    &request_id,
                    &success.model_id,
                    &model_name,
                    &api_key_id,
                    &success.provider,
                    &success.upstream_model,
                    &success.applied_guardrails,
                    status,
                    elapsed,
                    success.usage.as_ref().unwrap_or(&Default::default()),
                    &client,
                    success.redactions.clone(),
                    success.monitor_hits.clone(),
                    success.guardrail_blocked,
                    success.captured_content.as_ref(),
                    &audit,
                    success.upstream_called,
                    routing.attempts.last(),
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
            let attributed = crate::attribution::current().unwrap_or_default();
            let last_target = crate::request_metrics::LastTarget::new(&snapshot, &attributed);
            emit_access_log(
                "/v1/images/generations",
                &model_name,
                last_target.provider(),
                &api_key_id,
                status,
                elapsed,
                &request_id,
                &routing,
                Some(&err),
            );
            let metric_model = crate::usage_attr::metric_model_label(&snapshot, &model_name);
            // AISIX-Cloud#1325: name the target the request died on. This
            // branch used to emit `Upstream::default()`, so a 502 from a
            // real provider landed on `provider="unknown"` while the same
            // key's successes landed on the real one.
            crate::request_metrics::record(
                &state,
                "/v1/images/generations",
                crate::request_metrics::Caller::new(&auth),
                last_target.upstream(metric_model.as_ref(), false, routing.fallback_count() > 0),
                status,
                crate::attempt::error_source(&err),
                elapsed,
            );
            // Per #655 parity: surface the failed request in Logs with a
            // zero-token event (status + error class), instead of dropping it.
            // Per #655: one zero-token event per failed upstream attempt,
            // the last of them terminal when every attempt failed.
            crate::usage_attr::note_client_error(&snapshot, &err);
            let failed_terminal = crate::usage_attr::failed_attempts_are_terminal(&routing);
            crate::usage_attr::emit_failed_attempts(
                &state,
                &snapshot,
                crate::operation::IMAGE_GENERATION,
                &request_id,
                &model_name,
                &api_key_id,
                &client,
                &crate::usage_attr::applied_guardrails(&audit),
                &routing.attempts,
                failed_terminal,
                err.is_guardrail_block(),
                Vec::new(),
                crate::redact::RedactionCounts::new(),
                &audit,
            );
            // A failure no attempt answers for — pre-dispatch, or after the
            // winner — is one zero-token terminal event.
            if !failed_terminal {
                crate::usage_attr::emit_error_usage_event(
                    &state,
                    &snapshot,
                    crate::operation::IMAGE_GENERATION,
                    "openai",
                    &request_id,
                    &model_name,
                    &api_key_id,
                    status,
                    crate::usage_attr::EventError::of(&err),
                    err.is_guardrail_block(),
                    &client,
                    crate::usage_attr::applied_guardrails(&audit),
                    crate::usage_attr::enforced_hits(&audit),
                    crate::usage_attr::guardrail_scores(&audit),
                    crate::usage_attr::bypass_reason(&audit),
                );
            }
            err.into_response()
        }
    }
}

/// Build a [`ChatFormat`](aisix_gateway::ChatFormat) from the image
/// generation `prompt` so the input guardrail chain can scan it (#545).
/// Never sent upstream.
fn images_input_to_chat(model: &str, body: &Value) -> aisix_gateway::ChatFormat {
    // `user` and the prompt are both new content, so they are one newest
    // message, as the segment walk (`redact_images_request_structured`)
    // treats them: `user` is unbounded, and kept as an older message of its
    // own, a guardrail bounding how much it sends could leave its front
    // unchecked.
    let parts = ["user", "prompt"]
        .into_iter()
        .filter_map(|field| body.get(field).and_then(Value::as_str));
    crate::redact::newest_content_for_inspection(model, parts)
}

fn images_output_to_chat(model: &str, body: &Value) -> aisix_gateway::ChatResponse {
    let text = body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("revised_prompt").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    aisix_gateway::ChatResponse {
        logprobs: None,
        more_choices: Vec::new(),
        id: String::new(),
        model: model.to_string(),
        message: aisix_gateway::ChatMessage::assistant(text),
        finish_reason: aisix_gateway::FinishReason::Stop,
        usage: aisix_gateway::UsageStats::default(),
        service_tier: None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    mut body: Value,
    request_id: &str,
    client_ctx: &ClientContext,
    audit_out: &mut crate::usage_attr::GuardrailAudit,
    routing_out: &mut crate::attempt::RoutingTelemetry,
) -> Result<ImageDispatchSuccess, ProxyError> {
    crate::usage_attr::note_failure_request(snapshot, || body.to_string());
    crate::usage_attr::hold_ahead_of_chain(
        state,
        snapshot,
        auth,
        body.get("model").and_then(|v| v.as_str()),
    );
    // Owned so the #696 in-place prompt masking below can borrow `body`
    // mutably.
    let model_name = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProxyError::InvalidRequest("missing `model` field".into()))?
        .to_string();
    let model_name = model_name.as_str();

    let model_entry = crate::model_resolve::resolve_model(snapshot, model_name)
        .ok_or_else(|| ProxyError::ModelNotFound(model_name.to_string()))?;

    if !auth.key().can_access(snapshot, model_name) {
        return Err(ProxyError::ModelForbidden(model_name.to_string()));
    }

    // Client-IP allowlist gate (#557): reject before guardrails / upstream.
    crate::dispatch::check_ip_access(&model_entry.value, &client_ctx.source_ip)?;

    // #1101: partial-image SSE (`stream: true`) is not relayed on this
    // route, and the dispatch below reads the upstream answer as a single
    // JSON document. Forwarding `stream` had the provider generate — and
    // charge for — a stream the gateway then failed to decode; that decode
    // failure is retryable, and this route retries it, so ONE caller
    // request re-ran the generation for
    // the whole retry budget and still answered 502.
    //
    // Refuse it here, before the provider is contacted, and in the same
    // words `/v1/images/edits` already uses — the two are one family and a
    // caller should not have to learn two refusals. Unlike /v1/completions
    // there is no streaming route to point at, so the message names none.
    //
    // Rejected AFTER model resolution so an unknown model still answers 404
    // (matching the other JSON endpoints' precedence), and BEFORE the
    // guardrail chain and the rate-limit reservation so a request that
    // cannot be served burns neither.
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(ProxyError::InvalidRequest(
            "`stream` is not supported on /v1/images/generations".into(),
        ));
    }

    // #545: /v1/images/generations must run input guardrails. Before this it
    // forwarded the user `prompt` with no configured content/DLP check, so a
    // block enforced on /v1/chat/completions was bypassable by switching
    // surface. Run before the rate-limit reservation so a content-policy
    // refusal doesn't burn an RPM slot. (Output is an image, not scannable
    // text, so there is no output hook.)
    let guardrail_ctx = aisix_guardrails::RequestContext {
        passthrough_route_id: "",
        model_id: &model_entry.id,
        mcp_server_id: "",
        api_key_id: &auth.entry.id,
        team_id: auth.key().team_id.as_deref(),
    };
    let resolved_chain = state.guardrail_index.resolve(&guardrail_ctx);
    *audit_out = resolved_chain.audit_log();
    // Record which guardrails govern this request (#379 parity) so the emitted
    // UsageEvent surfaces them in Logs, like chat / messages. Empty when no
    // guardrail is attached.
    let applied_guardrails = resolved_chain.applied().to_vec();
    // A check that calls a remote moderation service is admitted by the
    // key's rate limits before it runs, and a refusal keeps the slot it took
    // (see `quota::reserve_before_costly_check`).
    let model_rl =
        crate::quota::ModelRateLimit::from_model(model_name, &model_entry.id, &model_entry.value);
    let early_reservation = crate::quota::reserve_before_costly_check(
        &resolved_chain,
        crate::quota::enforce(state, snapshot, auth, Some(&model_rl)),
    )
    .await?;
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
    let mut input_capture_safe = true;
    let mut redactions = crate::redact::RedactionCounts::new();
    if !resolved_chain.is_empty() {
        let chat = images_input_to_chat(model_name, &body);
        let (verdict, hits) =
            aisix_guardrails::Guardrail::check_input_non_segment_observed(&resolved_chain, &chat)
                .await;
        monitor_hits.extend(hits);
        let mut moderation = crate::redact::moderate_images_request_structured(
            &resolved_chain,
            Some(model_name),
            verdict,
            &mut body,
            &mut redactions,
            &mut monitor_hits,
        )
        .await;
        if !moderation.verdict.is_block() {
            let redaction =
                crate::redact::redact_images_request_structured(&resolved_chain, &mut body);
            crate::redact::merge_counts(&mut redactions, redaction.counts);
            if redaction.unrewritable_tool_key {
                moderation.verdict = crate::redact::unrewritable_tool_key_verdict();
                moderation.capture_safe = false;
            }
        }
        input_capture_safe = moderation.capture_safe;
        let verdict = moderation.verdict;
        if let aisix_guardrails::GuardrailVerdict::Block {
            reason,
            guardrail_name,
            unavailable,
        } = verdict
        {
            // Per #153 the matched-pattern detail stays in ops logs only.
            tracing::warn!(
                guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
                guardrail_hook = "input",
                model = %model_name,
                reason = %reason,
                "guardrail blocked /v1/images/generations request",
            );
            // A blocked request exports what the success path would have sent,
            // with what refused it masked too.
            if crate::usage_attr::content_cap(snapshot).is_some() {
                let redaction = crate::redact::redact_images_request_structured(
                    &resolved_chain.without_audit(),
                    &mut body,
                );
                // A `user` no mask can rewrite stays out of the row.
                crate::usage_attr::note_refused_request(
                    snapshot,
                    &resolved_chain,
                    !redaction.unrewritable_tool_key,
                    || body.to_string(),
                );
            }
            return Err(crate::error::guardrail_block_error(
                "request",
                guardrail_name.as_deref(),
                unavailable.as_deref(),
            ));
        }
    }

    // What a mask rule rewrote above is what a failure row may report, and
    // nothing when a check failed open: the success path's capture withholds
    // that request too.
    crate::usage_attr::note_screened_request(snapshot, || {
        if input_capture_safe {
            body.to_string()
        } else {
            String::new()
        }
    });
    // Content capture (#700): the client-facing request body
    // (post-#932-redaction) is the prompt; the response is the image JSON
    // with its base64 replaced by a placeholder, truncated at the cap.
    let content_cap = content_capture_cap(
        snapshot
            .observability_exporters
            .entries()
            .iter()
            .map(|e| &*e.value),
    );

    let reservation = match early_reservation {
        Some(reservation) => reservation,
        None => crate::quota::enforce(state, snapshot, auth, Some(&model_rl)).await?,
    };

    // A Model Group has no provider of its own — the walk resolves one per
    // target, and every provider-shape check below binds to the target that
    // is about to be called.
    crate::dispatch::require_dispatchable_entry(&model_entry.value)?;
    *routing_out = crate::attempt::RoutingTelemetry::for_request(&model_entry.value.display_name)
        .with_trace(client_ctx.trace.clone());
    let dispatched = crate::routing::dispatch_with_failover(
        state,
        snapshot,
        auth,
        client_ctx,
        model_name,
        &model_entry,
        routing_out,
        |_| true,
        |(answered, ..): &(Result<_, BridgeError>, String, String)| {
            answered.as_ref().err().map(ToString::to_string)
        },
        |target, timeouts| {
            let body = &body;
            async move {
                let provider =
                    crate::dispatch::require_provider(&target.model)?.to_ascii_lowercase();
                let pk_entry = crate::dispatch::resolve_provider_key(snapshot, &target.model)?;
                // Per #168: the documented `/v1/images/generations` route +
                // body shape exists on OpenAI's API and on Azure OpenAI,
                // which serves the same shape with the deployment in the URL.
                // Anthropic has no image-generation API at all; Gemini's
                // lives at a different URL
                // (`/v1beta/models/...:generateContent`) with a different
                // body; DeepSeek doesn't expose it. Dispatching one of those
                // would silently hit an upstream that 404s, so reject
                // explicitly with 400 and name the configuration error.
                //
                // The gate stays deliberately narrow — it is not "any
                // OpenAI-compatible vendor". A long-tail compat vendor
                // (deepseek, groq, …) speaks OpenAI's chat route but has no
                // image route, so admitting it would trade a clear 400 for a
                // 404 from the provider. Azure is admitted by its ADAPTER
                // rather than its vendor string: an Azure deployment serves
                // exactly this body shape, and gating on the string refused
                // every one of them because its `provider` is not the literal
                // "openai".
                //
                // Vertex (Imagen) and Bedrock (Titan Image / SDXL) are NOT
                // admitted: each takes a per-model-family request and returns
                // base64 payloads under its own key names, so serving them
                // needs a real adapter rather than a routing change. Same
                // shape of gap as the Voyage rerank note in `rerank.rs`.
                let azure = pk_entry.value.adapter == Some(aisix_core::Adapter::AzureOpenai);
                if target.model.provider.as_deref() != Some("openai") && !azure {
                    return Err(ProxyError::TargetRefused(format!(
                        "{} is not an OpenAI provider; \
                         /v1/images/generations requires OpenAI or Azure OpenAI",
                        crate::routing::refused_model_label(model_name)
                    )));
                }
                let bridge = crate::dispatch::resolve_bridge(&state.hub, &pk_entry.value)
                    .ok_or(ProxyError::ProviderUnavailable)?;
                // #554: apply the configured request `timeout` as the upstream deadline.
                let mut ctx = crate::dispatch::bridge_ctx(
                    request_id,
                    &target.id,
                    Arc::clone(&target.model),
                    &pk_entry.id,
                    Arc::clone(&pk_entry.value),
                    Some(client_ctx),
                );
                if let Some(d) = timeouts.request {
                    ctx = ctx.with_deadline(d);
                }
                // #701: per-attempt cooldown accounting — see completions.rs.
                let answered = match bridge.generate_image(body, &ctx).await {
                    Ok(v) => Ok(v),
                    Err(
                        e @ BridgeError::UnsupportedCapability(BridgeCapability::ImageGeneration),
                    ) => Err(e),
                    Err(e) => {
                        return Err(ProxyError::Bridge(crate::cooldown::note_failure(
                            &state.runtime_status,
                            &target.id,
                            target.model.cooldown.as_ref(),
                            e,
                        )))
                    }
                };
                Ok((answered, provider, pk_entry.id.to_string()))
            }
        },
    )
    .await;
    // Serialized once the upstream has answered, not before: a copy made
    // up front would be alive for the whole upstream wait.
    let captured_prompt = content_cap.map(|_| serde_json::to_string(&body).unwrap_or_default());
    let crate::routing::Dispatched {
        value: (answered, provider_label, pk_id),
        target,
        attempt_started,
        member_reservation,
        ..
    } = match dispatched {
        Ok(d) => d,
        Err(e) => {
            reservation.commit_tokens_no_spend(0).await;
            return Err(e);
        }
    };
    // Fold the target's model-layer reservation in (#1087) so
    // one commit bills the member's TPM/TPD too.
    let mut reservation = reservation;
    if let Some(member) = member_reservation {
        reservation.merge(member);
    }
    let model = &target.model;

    match answered {
        Ok(mut resp_json) => {
            // #701: clear any cooldown/unhealthy mark now the upstream
            // answered — same recovery signal as rerank/audio/chat.
            state.health.record_success(&model.display_name);
            state
                .runtime_status
                .mark_healthy(&target.id, attempt_started);
            // Extract usage tokens (gpt-image-1 returns a `usage` block;
            // dall-e-3 doesn't) BEFORE moving resp_json into the
            // Response, so the success struct carries typed counters.
            let usage = image_usage_stats(&resp_json);
            // #911 [21]: commit the actual token cost so TPM/TPD is enforced
            // for /v1/images/generations like chat + embeddings. Pre-fix the
            // reservation dropped uncommitted and the token counter never moved.
            let total_tokens = usage.as_ref().map_or(0, |u| {
                u64::from(u.prompt_tokens) + u64::from(u.completion_tokens)
            });
            // 花费与本端点用量事件的 cost_usd 同源：按调度到的目标行定价，
            // 输入全按新鲜计（这个表面没有提示缓存）。
            let spend = usage.as_ref().map_or(0, |u| {
                crate::usage_attr::request_spend_micro_usd(
                    snapshot,
                    &target.id,
                    aisix_core::InputTokens::uncached_only(u64::from(u.prompt_tokens)),
                    u64::from(u.completion_tokens),
                )
            });
            reservation.commit(total_tokens, spend).await;
            let review = review_image_output(
                &resolved_chain,
                model_name,
                &mut resp_json,
                &mut redactions,
                &mut monitor_hits,
            )
            .await;
            let output_capture_safe = review.capture_safe;
            let guardrail_blocked = review.refusal.is_some();
            // Content capture (#700): CapturedContent::new truncates to the cap.
            let captured_content = match (&captured_prompt, content_cap) {
                (Some(prompt), Some(cap)) if input_capture_safe && output_capture_safe => {
                    Some(CapturedContent::new(
                        prompt,
                        &captured_image_response(&resp_json),
                        cap as usize,
                    ))
                }
                _ => None,
            };
            let response = match review.refusal {
                Some(refusal) => refusal.into_response(),
                None => Json(resp_json).into_response(),
            };
            Ok(ImageDispatchSuccess {
                response,
                provider: provider_label,
                // The DISPATCHED row, so pricing and per-model analytics
                // resolve against the member that served.
                model_id: target.id.clone(),
                provider_key_id: pk_id.clone(),
                upstream_model: model.upstream_model().unwrap_or("unknown").to_string(),
                applied_guardrails: applied_guardrails.clone(),
                usage,
                upstream_called: true,
                redactions,
                monitor_hits: monitor_hits.clone(),
                guardrail_blocked,
                captured_content,
            })
        }
        // `Err` carries only the unsupported-capability refusal — every
        // other failure left through the dispatch.
        Err(e) => {
            // No upstream call → no tokens to count; release the reservation.
            reservation.commit_tokens_no_spend(0).await;
            let env = ErrorEnvelope::new(e.to_string(), "not_implemented");
            Ok(ImageDispatchSuccess {
                response: (StatusCode::NOT_IMPLEMENTED, Json(env)).into_response(),
                provider: provider_label,
                model_id: target.id.clone(),
                provider_key_id: pk_id.clone(),
                upstream_model: model.upstream_model().unwrap_or("unknown").to_string(),
                applied_guardrails: applied_guardrails.clone(),
                usage: None,
                // No upstream call happened; only guardrail attribution can
                // make the handler emit this branch.
                upstream_called: false,
                redactions,
                monitor_hits: monitor_hits.clone(),
                guardrail_blocked: false,
                captured_content: None,
            })
        }
    }
}

/// The token usage of an OpenAI image response, shared with passthrough
/// routes relaying an image body. gpt-image-1 returns
/// `usage: {input_tokens, output_tokens, total_tokens, ...}`; dall-e-2/3
/// return no `usage` block → `None`. Wire shape:
/// <https://platform.openai.com/docs/api-reference/images/object>
pub(crate) fn image_usage_stats(body: &Value) -> Option<aisix_gateway::UsageStats> {
    let usage = body.get("usage")?;
    let input = crate::usage_attr::token_count(usage.get("input_tokens").and_then(Value::as_u64)?);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .map(crate::usage_attr::token_count)
        .unwrap_or(0);
    // `input_tokens_details` / `output_tokens_details` split each side
    // into `text_tokens` and `image_tokens`.
    let modality = aisix_gateway::ModalityTokens::from_openai_details(
        usage.get("input_tokens_details"),
        usage.get("output_tokens_details"),
    );
    Some(aisix_gateway::UsageStats {
        modality,
        upstream_total_tokens: crate::usage_attr::reported_total_tokens(Some(usage)).unwrap_or(0),
        ..aisix_gateway::UsageStats::new(input, output)
    })
}

/// Issue #407: push one `UsageEvent` onto the control plane's telemetry sink and
/// fan it out to per-env OTLP exporters. Mirrors
/// `embeddings::emit_usage_event` (#402). `inbound_protocol = "openai"`
/// (images are an OpenAI-shape endpoint). Tokens are populated when the
/// upstream returned a `usage` block (gpt-image-1); zero otherwise —
/// the per-image cost basis (n × size × quality) is a cross-repo
/// follow-up needing a UsageEvent wire extension + the control plane pricing.
///
/// The per-PK attribution tags (provider_kind / provider_featured /
/// branded_provider / pk_label / byo_label) are populated from the
/// resolved ProviderKey — same lookup as chat / messages / responses /
/// embeddings (#867 parity) via `usage_attr::apply_pk_telemetry`.
/// What the output chain decided about an image answer.
pub(crate) struct ImageOutputReview {
    /// False when the check failed open or refused: capture is withheld.
    pub(crate) capture_safe: bool,
    /// The error to answer with instead of the image. The upstream already
    /// billed, so the caller's usage still rides the event, marked blocked.
    pub(crate) refusal: Option<ProxyError>,
}

/// Run the output chain over an image answer and mask what it allows. Shared
/// by `/v1/images/generations` and `/v1/images/edits`: `data[].revised_prompt`
/// is model text on both, and a rule enforced on one must not be bypassable by
/// calling the other.
pub(crate) async fn review_image_output(
    chain: &aisix_guardrails::GuardrailChain,
    model_name: &str,
    resp_json: &mut Value,
    redactions: &mut crate::redact::RedactionCounts,
    monitor_hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
) -> ImageOutputReview {
    if chain.is_empty() {
        return ImageOutputReview {
            capture_safe: true,
            refusal: None,
        };
    }
    let synth = images_output_to_chat(model_name, resp_json);
    let (verdict, hits) =
        aisix_guardrails::Guardrail::check_output_non_segment_observed(chain, &synth).await;
    monitor_hits.extend(hits);
    let moderation = crate::redact::moderate_images_response_structured(
        chain,
        verdict,
        resp_json,
        redactions,
        monitor_hits,
    )
    .await;
    let mut refusal = None;
    if let aisix_guardrails::GuardrailVerdict::Block {
        guardrail_name,
        unavailable,
        ..
    } = &moderation.verdict
    {
        // The name and the `unavailable` tag come from one verdict.
        refusal = Some(crate::error::guardrail_block_error(
            "response",
            guardrail_name.as_deref(),
            unavailable.as_deref(),
        ));
    } else {
        let redaction = crate::redact::redact_images_response_structured(chain, resp_json);
        crate::redact::merge_counts(redactions, redaction.counts);
    }
    ImageOutputReview {
        capture_safe: moderation.capture_safe,
        refusal,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_usage_event(
    state: &ProxyState,
    // The request's snapshot + its one ProviderKey observation, resolved
    // by the handler (#941).
    snap: &aisix_core::AisixSnapshot,
    pk: &crate::usage_attr::ResolvedPk<'_>,
    // `/v1/images/generations` or `/v1/images/edits` — the two image
    // surfaces share this emit (AISIX-Cloud#1360), so which one this is
    // has to come from the caller: they share a handler label and must
    // NOT share an operation (AISIX-Cloud#1461).
    endpoint: &'static str,
    surface: crate::operation::Surface,
    request_id: &str,
    model_id: &str,
    requested_model: &str,
    api_key_id: &str,
    // Metric labels the UsageEvent has no field for (#1234
    // follow-up): the wire struct is the CP contract, so they ride
    // alongside rather than in it.
    provider: &str,
    upstream_model: &str,
    applied_guardrails: &[AppliedGuardrail],
    status_code: u16,
    elapsed: Duration,
    usage: &aisix_gateway::UsageStats,
    client: &ClientContext,
    // Per-detector PII mask counts (#932/#696). Empty = no redaction.
    redacted_entity_counts: crate::redact::RedactionCounts,
    // Monitor-mode guardrail observations (#562).
    guardrail_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    guardrail_blocked: bool,
    // Captured request/response content (#700). Forwarded only to `fan_out`,
    // never to the CP sink.
    content: Option<&CapturedContent>,
    // The request's enforced-guardrail audit handle (AISIX-Cloud#1330).
    // Shared by both image surfaces, like the rest of this emit.
    audit: &crate::usage_attr::GuardrailAudit,
    dispatched: bool,
    // The attempt that answered (#655); `None` when none was dispatched.
    winner: Option<&crate::attempt::AttemptRecord>,
) {
    let mut event = UsageEvent {
        request_id: request_id.to_string(),
        occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
        model_id: model_id.to_string(),
        api_key_id: api_key_id.to_string(),
        requested_model: requested_model.to_string(),
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.upstream_total_tokens,
        modality_tokens: usage.modality,
        // Priced from the dispatched row's `Model.cost` when the operator set
        // one, `0.0` otherwise — see `usage_attr::request_cost_usd`.
        cost_usd: crate::usage_attr::request_cost_usd(
            snap,
            model_id,
            // 这个表面没有提示缓存，全部按新鲜输入计价。
            aisix_core::InputTokens::uncached_only(u64::from(usage.prompt_tokens)),
            u64::from(usage.completion_tokens),
        ),
        // Narrowed to the winning attempt's own latency below
        // (`apply_winning_attempt`) once a failover put earlier attempts
        // in front of it; they emitted their own events.
        upstream_latency_ms: crate::attempt::measured_ms(elapsed),
        downstream_latency_ms: crate::attempt::measured_ms(elapsed),
        status_code,
        inbound_protocol: "openai".to_string(),
        applied_guardrails: applied_guardrails.to_vec(),
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        request_headers: client.request_headers.clone(),
        redacted_entity_counts,
        guardrail_monitor_hits,
        guardrail_blocked,
        guardrail_enforced_hits: crate::usage_attr::enforced_hits(audit),
        guardrail_scores: crate::usage_attr::guardrail_scores(audit),
        guardrail_bypassed_reason: crate::usage_attr::bypass_reason(audit),
        ..Default::default()
    };
    crate::usage_attr::apply_pk_telemetry(&mut event, pk);
    crate::usage_attr::apply_winning_attempt(&mut event, winner);
    crate::usage_attr::apply_caller_identity(&mut event, client.jwt.as_ref(), &client.caller);
    let usage_model =
        crate::usage_attr::usage_event_model_label(snap, &event.requested_model).into_owned();
    // Read before the event is handed over: the spend counter below is what
    // the portal bills from.
    let cost_usd = event.cost_usd;
    crate::usage_attr::emit_usage(
        state,
        snap,
        surface,
        event,
        crate::usage_attr::usage_event_labels(&usage_model, pk),
        content,
        client.trace.as_ref(),
        /* terminal */ true,
        dispatched,
    );
    let owned_caller = crate::request_metrics::Caller::from_api_key_id(snap, api_key_id);
    crate::request_metrics::record_usage(
        state,
        endpoint,
        owned_caller.as_caller(),
        crate::request_metrics::Upstream {
            provider,
            model: requested_model,
            upstream_model,
            pk: pk.labels(),
            ..Default::default()
        },
        crate::request_metrics::Tokens {
            input: usage.prompt_tokens,
            output: usage.completion_tokens,
            total: usage.prompt_tokens.saturating_add(usage.completion_tokens),
            spend_usd: cost_usd,
            // No upstream on this surface reports prompt-cache detail.
            cached: 0,
            cache_read: 0,
            cache_creation: 0,
            client_type: state.client_classifier.classify(&client.user_agent),
        },
    );
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_access_log(
    endpoint: &'static str,
    model: &str,
    provider: &str,
    api_key_id: &str,
    status: u16,
    latency: Duration,
    request_id: &str,
    routing: &crate::attempt::RoutingTelemetry,
    error: Option<&ProxyError>,
) {
    let (error_kind, error) = match error {
        Some(e) => {
            let (kind, msg) = crate::attempt::access_log_error(e);
            (Some(kind), Some(msg))
        }
        None => (None, None),
    };
    let target = crate::attribution::AccessLogTarget::current();
    let summary = routing.access_log_summary();
    crate::attribution::emit_access_log(AccessLog {
        method: "POST",
        path: endpoint,
        status,
        latency,
        duration: latency,
        provider: Some(provider),
        model: Some(model),
        upstream_model: target.upstream_model(),
        provider_key_id: target.provider_key_id(),
        api_key_id: Some(api_key_id),
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        request_id,
        // No provider response id: the images response is
        // `{created, data, usage}` — no id on the wire (#1289).
        provider_request_id: None,
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

/// The image response as an exporter's captured response: its base64 is a
/// placeholder, every other field as the upstream sent it.
pub(crate) fn captured_image_response(response: &serde_json::Value) -> String {
    let text = serde_json::to_string(response).unwrap_or_default();
    match aisix_obs::sink::omit_base64_media_in_json(&text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(omitted) => omitted,
    }
}

#[cfg(test)]
mod tests {

    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, Model, ProxyConfig};
    use aisix_gateway::Hub;
    use aisix_obs::UsageEvent;
    use aisix_provider_openai::OpenAiBridge;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
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

    const PK_ID: &str = "11111111-1111-1111-1111-111111111111";

    fn model_entry(name: &str) -> ResourceEntry<Model> {
        let json = format!(
            r#"{{
                "display_name": "{name}",
                "provider": "openai",
                "model_name": "dall-e-3",
                "provider_key_id": "{PK_ID}"
            }}"#
        );
        let m: Model = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("m-1", m, 1)
    }

    fn anthropic_model_entry(name: &str) -> ResourceEntry<Model> {
        let json = format!(
            r#"{{
                "display_name": "{name}",
                "provider": "anthropic",
                "model_name": "claude-3-5-haiku-20241022",
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

    /// #867: same openai PK as `provider_key_entry` but carrying
    /// `telemetry_tags`, so the emitted UsageEvent gets the per-PK
    /// attribution fields stamped via `usage_attr::apply_pk_telemetry`.
    fn provider_key_entry_tagged(api_base: &str) -> ResourceEntry<aisix_core::ProviderKey> {
        let json = format!(
            r#"{{"display_name":"openai-up","secret":"sk-up","api_base":"{api_base}","provider":"openai","adapter":"openai","telemetry_tags":{{"kind":"catalog","featured":true,"branded_provider":"openai","pk_label":"prod-images-key"}}}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&json).unwrap();
        ResourceEntry::new(PK_ID, pk, 1)
    }

    fn new_snap(api_base: &str) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(provider_key_entry(api_base));
        snap
    }

    fn new_snap_tagged(api_base: &str) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys
            .insert(provider_key_entry_tagged(api_base));
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
            .uri("/v1/images/generations")
            .header("authorization", "Bearer sk-caller")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }

    fn upstream_response() -> serde_json::Value {
        serde_json::json!({
            "created": 1_700_000_000i64,
            "data": [{"url": "https://example.com/image.png"}]
        })
    }

    fn keyword_input_guardrail(literal: &str) -> ResourceEntry<aisix_core::Guardrail> {
        let json = format!(
            r#"{{"name":"t","enabled":true,"hook_point":"input","fail_open":false,"kind":"keyword","patterns":[{{"kind":"literal","value":"{literal}"}}]}}"#
        );
        let g: aisix_core::Guardrail = serde_json::from_str(&json).unwrap();
        ResourceEntry::new("g-1", g, 1)
    }

    /// #545: a configured input guardrail must fire on /v1/images/generations
    /// — a blocked `prompt` returns 422 content_filter, upstream never hit.
    #[tokio::test]
    async fn input_guardrail_blocks_prompt_returns_422() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_response()))
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("dalle"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "dalle", "prompt": "draw BLOCKME please"});
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

    /// #545 companion: a benign prompt with a guardrail configured forwards
    /// (`expect(1)`) and returns 200.
    #[tokio::test]
    async fn input_guardrail_allows_benign_prompt_forwards_200() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_response()))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("dalle"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "dalle", "prompt": "a serene landscape"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["data"][0]["url"].is_string());
    }

    /// Issue #168 regression: only OpenAI's API has the documented
    /// `/v1/images/generations` route + body shape. A non-OpenAI
    /// Model configured here must be rejected at the gateway
    /// boundary with 400 (parallel to /v1/responses §4.6) rather
    /// than dispatched to an upstream that would 404 (or worse,
    /// hit a different Gemini route shape).
    #[tokio::test]
    async fn non_openai_provider_returns_400_invalid_request() {
        let snap = new_snap("https://api.anthropic.com");
        snap.models.insert(anthropic_model_entry("anthropic-image"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let app = build_app(snap);
        let body = serde_json::json!({
            "model": "anthropic-image",
            "prompt": "A sunset over mountains"
        });
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
        let message = v["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("requires OpenAI"),
            "rejection should reference OpenAI restriction; got {message:?}"
        );
    }

    #[tokio::test]
    async fn happy_path_returns_image_url() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_response()))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("dall-e"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let app = build_app(snap);
        let body = serde_json::json!({
            "model": "dall-e",
            "prompt": "A sunset over mountains",
            "n": 1,
            "size": "1024x1024"
        });
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["data"][0]["url"].as_str().is_some());
    }

    #[tokio::test]
    async fn unauthenticated_request_returns_401() {
        let snap = new_snap("http://unused");
        snap.models.insert(model_entry("dall-e"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let app = build_app(snap);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/images/generations")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"model":"dall-e","prompt":"hi"}"#,
            ))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn forbidden_model_returns_403() {
        let snap = new_snap("http://unused");
        snap.models.insert(model_entry("dall-e"));
        snap.apikeys.insert(apikey_entry(&["other-model"]));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "dall-e", "prompt": "hi"});
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
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("dall-e"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let app = build_app(snap);
        let body = serde_json::json!({"model": "dall-e", "prompt": "hi"});
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    fn build_app_with_sink(
        snap: AisixSnapshot,
        tx: tokio::sync::mpsc::Sender<UsageEvent>,
    ) -> axum::Router {
        use aisix_obs::UsageSink;
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        crate::build_router(state)
    }

    /// Issue #407: gpt-image-1 returns a `usage` token block — a
    /// successful generation must emit a UsageEvent carrying those
    /// tokens, attributed to the api_key + model, inbound_protocol
    /// "openai".
    #[tokio::test]
    async fn emits_usage_event_with_tokens_when_upstream_returns_usage() {
        let upstream = MockServer::builder().start().await;
        let body = serde_json::json!({
            "created": 1_700_000_000i64,
            "data": [{"b64_json": "aGVsbG8="}],
            "usage": {
                "input_tokens": 50,
                "output_tokens": 1568,
                "total_tokens": 1618,
                "input_tokens_details": {"text_tokens": 10, "image_tokens": 40}
            }
        });
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("gpt-image"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = build_app_with_sink(snap, tx);
        let req = serde_json::json!({"model": "gpt-image", "prompt": "a cat", "n": 1});
        let resp = tower::ServiceExt::oneshot(app, make_req(req))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for /v1/images/generations 200")
            .expect("usage_sink sender dropped");
        assert_eq!(event.prompt_tokens, 50);
        assert_eq!(event.completion_tokens, 1568);
        assert_eq!(event.status_code, 200);
        assert_eq!(event.api_key_id, "k-1");
        assert_eq!(event.model_id, "m-1");
        assert_eq!(event.inbound_protocol, "openai");
    }

    /// Issue #407: dall-e-3 returns NO `usage` block — the request still
    /// emits a zero-token UsageEvent so it's visible in /logs and
    /// attributed (precise per-image cost is a cross-repo follow-up).
    #[tokio::test]
    async fn emits_zero_token_event_when_upstream_omits_usage() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_response()))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("dall-e"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = build_app_with_sink(snap, tx);
        let req = serde_json::json!({"model": "dall-e", "prompt": "a dog", "n": 1});
        let resp = tower::ServiceExt::oneshot(app, make_req(req))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("zero-token UsageEvent must still be emitted (visibility)")
            .expect("usage_sink sender dropped");
        assert_eq!(event.prompt_tokens, 0);
        assert_eq!(event.completion_tokens, 0);
        assert_eq!(event.status_code, 200);
        assert_eq!(event.model_id, "m-1");
        assert_eq!(event.inbound_protocol, "openai");
    }

    /// A 501 emits a zero-token event, and one reached after a mask carries
    /// that attribution (#1083). Unlike completions and embeddings, /v1/images/generations
    /// rejects non-OpenAI providers with 400 *before* dispatch (see
    /// `non_openai_provider_returns_400_invalid_request`) and the real
    /// `OpenAiBridge` overrides `generate_image`, so the only way to reach
    /// the 501 default is an openai-provider Model whose resolved bridge
    /// leaves `Bridge::generate_image` at the trait default. We register a
    /// minimal stub under the "openai" key to exercise exactly that.
    #[tokio::test]
    async fn image_501_emits_a_zero_token_event() {
        use aisix_gateway::{
            Bridge, BridgeContext, BridgeError, ChatChunkStream, ChatFormat, ChatMessage,
            ChatResponse, FinishReason, UsageStats,
        };
        use aisix_obs::UsageSink;

        // Minimal openai-family bridge that satisfies the required chat
        // methods but leaves `generate_image` at the 501 trait default.
        struct NoImageBridge;

        #[async_trait::async_trait]
        impl Bridge for NoImageBridge {
            fn name(&self) -> &'static str {
                "no-image"
            }

            async fn chat(
                &self,
                req: &ChatFormat,
                _ctx: &BridgeContext,
            ) -> Result<ChatResponse, BridgeError> {
                Ok(ChatResponse {
                    logprobs: None,
                    more_choices: Vec::new(),
                    id: "stub".into(),
                    model: req.model.clone(),
                    message: ChatMessage::assistant("stub"),
                    finish_reason: FinishReason::Stop,
                    usage: UsageStats::new(0, 0),
                    service_tier: None,
                })
            }

            async fn chat_stream(
                &self,
                _req: &ChatFormat,
                _ctx: &BridgeContext,
            ) -> Result<ChatChunkStream, BridgeError> {
                Ok(Box::pin(futures::stream::iter(Vec::new())))
            }
        }

        let snap = new_snap("https://api.openai.com");
        snap.models.insert(model_entry("stub-image"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, masking_input_guardrail());

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(NoImageBridge));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state);

        let body = serde_json::json!({"model": "stub-image", "prompt": "a cat", "n": 1});
        let resp = tower::ServiceExt::oneshot(app.clone(), make_req(body))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_IMPLEMENTED,
            "a bridge without generate_image must surface /v1/images/generations as 501 \
             (default Bridge::generate_image returns BridgeError::Config)",
        );

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("the 501 must emit its zero-token UsageEvent")
            .expect("usage sink remains open");
        assert_eq!(ev.status_code, 501);
        assert_eq!(ev.error_class, "config", "{ev:?}");
        assert!(ev.error_message.contains("does not support"), "{ev:?}");
        assert_eq!((ev.prompt_tokens, ev.completion_tokens), (0, 0));
        assert!(ev.guardrail_enforced_hits.is_empty(), "{ev:?}");

        let body = serde_json::json!({
            "model": "stub-image",
            "prompt": "draw version: 9.9.9",
            "n": 1
        });
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("the masked 501 must emit its guardrail attribution")
            .expect("usage sink remains open");
        assert_eq!(ev.status_code, 501);
        assert_eq!((ev.prompt_tokens, ev.completion_tokens), (0, 0));
        assert_eq!(ev.guardrail_enforced_hits.len(), 1, "{ev:?}");
        assert_eq!(ev.guardrail_enforced_hits[0].action, "masked");
    }

    /// #867 parity: a successful /v1/images/generations 200 must
    /// stamp the per-PK telemetry attribution fields (provider_kind /
    /// provider_featured / branded_provider / pk_label) on the emitted
    /// UsageEvent, sourced from the resolved ProviderKey's `telemetry_tags`
    /// — exactly like /v1/chat/completions, /v1/messages, /v1/responses, and
    /// /v1/embeddings. Pre-fix these five fields were left at Default, so
    /// image-generation spend showed up unattributed in the control plane analytics.
    #[tokio::test]
    async fn emits_provider_telemetry_tags_issue_867() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_response()))
            .mount(&upstream)
            .await;

        let snap = new_snap_tagged(&upstream.uri());
        snap.models.insert(model_entry("dall-e"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = build_app_with_sink(snap, tx);
        let req = serde_json::json!({"model": "dall-e", "prompt": "a cat", "n": 1});
        let resp = tower::ServiceExt::oneshot(app, make_req(req))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for /v1/images/generations 200")
            .expect("usage_sink sender dropped");
        assert_eq!(
            ev.provider_kind, "catalog",
            "provider_kind must mirror the resolved PK's telemetry_tags.kind"
        );
        assert!(
            ev.provider_featured,
            "provider_featured must mirror telemetry_tags.featured"
        );
        assert_eq!(
            ev.branded_provider, "openai",
            "branded_provider must mirror telemetry_tags.branded_provider"
        );
        assert_eq!(
            ev.pk_label, "prod-images-key",
            "pk_label must mirror telemetry_tags.pk_label"
        );
    }

    /// #379 parity: a successful /v1/images/generations 200 governed by a
    /// configured input guardrail must surface the applied `{kind, hook}` set
    /// on the emitted UsageEvent — exactly like chat / messages / embeddings.
    /// A benign prompt that does NOT match the keyword guardrail passes through
    /// (200), and the event records the guardrail that governed the request.
    #[tokio::test]
    async fn applied_guardrails_recorded_on_usage_event() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_response()))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("dall-e"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = build_app_with_sink(snap, tx);
        // Benign prompt — does not contain the blocked literal, so it passes
        // the guardrail and reaches the upstream.
        let req = serde_json::json!({"model": "dall-e", "prompt": "a serene landscape", "n": 1});
        let resp = tower::ServiceExt::oneshot(app, make_req(req))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for /v1/images/generations 200")
            .expect("usage_sink sender dropped");
        assert!(
            !ev.applied_guardrails.is_empty(),
            "the configured guardrail must be recorded on the UsageEvent"
        );
        assert_eq!(
            ev.applied_guardrails[0].kind, "keyword",
            "applied guardrail kind must mirror the configured guardrail"
        );
        assert_eq!(
            ev.applied_guardrails[0].hook, "input",
            "applied guardrail hook must mirror the configured hook_point"
        );
    }

    /// #655 parity: an upstream 5xx on /v1/images/generations now emits ONE
    /// zero-token UsageEvent so the failed request is visible in Logs (status +
    /// error class) and attributed to the api_key — instead of being dropped.
    /// Mirrors `completions.rs::upstream_5xx_emits_zero_token_error_event`.
    #[tokio::test]
    async fn upstream_5xx_emits_zero_token_error_event() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("dall-e"));
        snap.apikeys.insert(apikey_entry(&["*"]));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = build_app_with_sink(snap, tx);
        let req = serde_json::json!({"model": "dall-e", "prompt": "a cat", "n": 1});
        let resp = tower::ServiceExt::oneshot(app, make_req(req))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("a failed /v1/images/generations must emit a zero-token UsageEvent")
            .expect("usage_sink sender dropped");
        assert_eq!(ev.status_code, 502, "upstream 5xx maps to 502");
        assert_eq!(ev.prompt_tokens, 0);
        assert_eq!(ev.api_key_id, "k-1");
        assert_eq!(ev.requested_model, "dall-e");
        assert!(
            !ev.error_class.is_empty(),
            "error_class must classify the failure"
        );
        // One event per upstream attempt (#655): each of the direct
        // model's own retries fails the same way and leaves its own row,
        // none of them billed.
        let mut events = vec![ev];
        while let Ok(more) = rx.try_recv() {
            events.push(more);
        }
        let hits = upstream.received_requests().await.unwrap_or_default().len();
        assert!(
            hits > 1,
            "the direct model's retry budget should replay a 5xx"
        );
        assert_eq!(events.len(), hits, "exactly one event per upstream attempt");
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(ev.attempt_index, i as u32);
            assert_eq!(ev.attempt_kind, if i == 0 { "initial" } else { "retry" });
            assert_eq!(ev.status_code, events[0].status_code);
            assert_eq!((ev.prompt_tokens, ev.completion_tokens), (0, 0));
            assert_eq!(ev.cost_usd, 0.0);
        }
    }

    fn pii_mask_guardrail() -> ResourceEntry<aisix_core::Guardrail> {
        let json = r#"{"name":"pii","enabled":true,"hook_point":"input","kind":"pii","detectors":[{"type":"email","action":"mask"}]}"#;
        let g: aisix_core::Guardrail = serde_json::from_str(json).unwrap();
        ResourceEntry::new("g-pii", g, 1)
    }

    fn pii_output_mask_guardrail() -> ResourceEntry<aisix_core::Guardrail> {
        let json = r#"{"name":"pii-output","enabled":true,"hook_point":"output","kind":"pii","detectors":[{"type":"email","action":"mask"}]}"#;
        let g: aisix_core::Guardrail = serde_json::from_str(json).unwrap();
        ResourceEntry::new("g-pii-output", g, 1)
    }

    #[tokio::test]
    async fn pii_mask_on_structural_user_fails_closed_before_upstream() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_response()))
            .expect(0)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("img"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, pii_mask_guardrail());

        let app = build_app(snap);
        let resp = tower::ServiceExt::oneshot(
            app,
            make_req(serde_json::json!({
                "model": "img",
                "prompt": "a clean portrait",
                "user": "alice@example.com"
            })),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(resp.into_body(), 65536).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("alice@example.com"));
    }

    #[tokio::test]
    async fn pii_mask_rewrites_revised_prompt_before_response_and_usage() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "created": 1_700_000_000,
                "data": [{
                    "url": "https://img.example/1.png",
                    "revised_prompt": "portrait of alice@example.com"
                }]
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("img"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, pii_output_mask_guardrail());

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = build_app_with_sink(snap, tx);
        let resp = tower::ServiceExt::oneshot(
            app,
            make_req(serde_json::json!({"model": "img", "prompt": "clean"})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 65536).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("[EMAIL_REDACTED]"), "body: {text}");
        assert!(!text.contains("alice@example.com"), "body: {text}");

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted")
            .expect("usage_sink sender dropped");
        assert_eq!(event.redacted_entity_counts.get("email"), Some(&1));
        assert!(!event.guardrail_blocked);
    }

    /// #696: a mask-action PII detector must rewrite the `prompt` before the
    /// body reaches the upstream. Pre-#696 the mask action was a silent
    /// no-op on /v1/images/generations — the raw text was forwarded. Also
    /// pins the counts on the emitted UsageEvent.
    #[tokio::test]
    async fn pii_mask_rewrites_prompt_before_upstream_issue_696() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "created": 1_700_000_000,
                "data": [{"url": "https://img.example/1.png"}]
            })))
            .expect(1)
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("img"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, pii_mask_guardrail());

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = build_app_with_sink(snap, tx);
        let body = serde_json::json!({
            "model": "img",
            "prompt": "a portrait of a@x.com at sunset"
        });
        let resp = tower::ServiceExt::oneshot(app, make_req(body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let reqs = upstream.received_requests().await.unwrap();
        let sent = String::from_utf8_lossy(&reqs[0].body).into_owned();
        assert!(sent.contains("[EMAIL_REDACTED]"), "sent: {sent}");
        assert!(
            !sent.contains("a@x.com"),
            "raw PII forwarded upstream: {sent}"
        );

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted")
            .expect("usage_sink sender dropped");
        assert_eq!(
            event.redacted_entity_counts.get("email"),
            Some(&1),
            "mask counts must reach the UsageEvent"
        );
    }

    /// #701: an upstream 5xx must mark the model's runtime status (cooldown)
    /// — /v1/images/generations previously never touched it.
    #[tokio::test]
    async fn upstream_5xx_marks_cooldown_issue_701() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        // Cooldown is opt-in (AISIX-Cloud#1499). The subject here is that
        // this handler routes its failures through the cooldown
        // chokepoint at all, so the model has to ask for cooldown.
        let mut entry = model_entry("img");
        Arc::make_mut(&mut entry.value).cooldown = Some(aisix_core::CooldownConfig {
            enabled: Some(true),
            ..Default::default()
        });
        snap.models.insert(entry);
        snap.apikeys.insert(apikey_entry(&["*"]));

        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg()).without_cache();
        let app = crate::build_router(state.clone());

        let body = serde_json::json!({"model": "img", "prompt": "a cat"});
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

    /// `user` and the prompt are both new content: the inspection request
    /// carries them as one message, so a guardrail that checks the newest
    /// message in full covers a long `user` too.
    #[test]
    fn user_and_prompt_are_inspected_as_one_newest_message() {
        let body = serde_json::json!({"model": "m", "prompt": "p", "user": "u"});
        let chat = super::images_input_to_chat("m", &body);
        let contents: Vec<_> = chat.messages.iter().map(|m| m.content.as_deref()).collect();
        assert_eq!(contents, [Some("u\np")]);
    }

    /// #1330 / #1024: a guardrail BLOCK leaves this handler
    /// through `Err`, so the terminal usage event is the shared
    /// zero-token error event. That branch is the one an auditor reads —
    /// "which policy refused this request" — and a drain wired only into
    /// the success path misses it silently.
    #[tokio::test]
    async fn blocked_request_names_the_policy_on_the_usage_event() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("my-image"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, keyword_input_guardrail("BLOCKME"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let app = crate::build_router(
            crate::ProxyState::new(handle, hub, &cfg())
                .without_cache()
                .with_usage_sink(UsageSink::new(tx)),
        );

        let resp = tower::ServiceExt::oneshot(
            app,
            make_req(serde_json::json!({"model": "my-image", "prompt": "please BLOCKME"})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted for the refusal")
            .expect("usage_sink sender dropped");
        assert_eq!(ev.guardrail_enforced_hits.len(), 1, "{ev:?}");
        assert_eq!(ev.guardrail_enforced_hits[0].guardrail_name, "t");
        assert_eq!(ev.guardrail_enforced_hits[0].hook, "input");
        assert_eq!(ev.guardrail_enforced_hits[0].action, "blocked");
        let wire = serde_json::to_string(&ev).unwrap();
        assert!(!wire.contains("BLOCKME"), "{wire}");
    }
    /// A `kind: "pii"` row whose one custom pattern masks a version string
    /// on input — the in-process rewrite this handler actually performs,
    /// as opposed to the keyword BLOCK the sibling test drives.
    fn masking_input_guardrail() -> ResourceEntry<aisix_core::Guardrail> {
        let g: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"eda-mask","enabled":true,"hook_point":"input","kind":"pii","detectors":[],"custom_patterns":[{"name":"eda_version","regex":"version\\s*:\\s*(\\d+(?:\\.\\d+)+)","action":"mask","replacement":"***"}]}"#,
        )
        .unwrap();
        ResourceEntry::new("g-mask", g, 1)
    }

    /// #1330 / #1024: the SUCCESS emitter drains too.
    ///
    /// The sibling test drives a refusal, which leaves through the shared
    /// error event — so on its own it would stay green if the drain were
    /// deleted from the success path. An enforcing MASK is the case that
    /// only the success emitter can report, and the case a masking
    /// deployment lives on: the request is served, nothing errors, and
    /// without the drain the /logs row is indistinguishable from one no
    /// guardrail touched.
    #[tokio::test]
    async fn masked_request_names_the_policy_on_the_usage_event() {
        use aisix_obs::UsageSink;

        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upstream_response()))
            .mount(&upstream)
            .await;

        let snap = new_snap(&upstream.uri());
        snap.models.insert(model_entry("dalle"));
        snap.apikeys.insert(apikey_entry(&["*"]));
        crate::seed_env_scoped_guardrail(&snap, masking_input_guardrail());

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let handle = SnapshotHandle::new(snap);
        let app = crate::build_router(
            crate::ProxyState::new(handle, hub, &cfg())
                .without_cache()
                .with_usage_sink(UsageSink::new(tx)),
        );

        let resp = tower::ServiceExt::oneshot(
            app,
            make_req(serde_json::json!({"model": "dalle", "prompt": "draw version: 9.9.9 ok"})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("UsageEvent must be emitted")
            .expect("usage_sink sender dropped");
        assert_eq!(ev.guardrail_enforced_hits.len(), 1, "{ev:?}");
        assert_eq!(ev.guardrail_enforced_hits[0].guardrail_name, "eda-mask");
        assert_eq!(ev.guardrail_enforced_hits[0].hook, "input");
        assert_eq!(ev.guardrail_enforced_hits[0].action, "masked");
        let wire = serde_json::to_string(&ev).unwrap();
        assert!(!wire.contains("9.9.9"), "{wire}");
    }
}
