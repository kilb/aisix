//! `/a2a/:agent` — the downstream-facing A2A gateway endpoint.
//!
//! AISIX fronts each registered A2A agent: a caller reaches an agent through
//! `/a2a/<agent>`, and its card is served (with the service URL rewritten to
//! point back at the gateway) at `/a2a/<agent>/.well-known/agent-card.json`.
//! The caller authenticates with an AISIX API key — the [`AuthenticatedKey`]
//! extractor rejects a missing or invalid key with `401` before the request
//! reaches the agent. The endpoint is rebuilt from the current configuration
//! snapshot on each request, so it always reflects the live `a2a_agents` set.
//!
//! A `message/send` (and every other JSON-RPC call) is governed by the SAME
//! pipeline as an LLM request, keyed on the caller's API key: per-agent access
//! control (the key's `allowed_agents`), rate-limit + budget (`quota::enforce`),
//! and a usage event into the shared sink. The upstream credential is held
//! gateway-side and never reaches the caller. Content policy applies too: the
//! request's message is scanned before the call and every unary result after
//! it (a read such as `tasks/get` included), so a rule that blocks a phrase on
//! an LLM endpoint is not bypassable by asking an agent the same thing. The
//! chain resolves with no model and no MCP server — an A2A call has neither —
//! so `env`, `api_key` and `team` scoped policies govern it; a per-agent scope
//! would need a new attachment dimension in the resource model. A STREAMED
//! answer is held back and released as it scans clean, under the same
//! streamed-output policy the LLM streaming paths apply; and while an output
//! guardrail applies, a call that would set up push notifications is refused,
//! since the agent would deliver those straight to the caller's webhook.
//!
//! The request body is forwarded verbatim to the upstream agent, so the caller
//! speaks whichever A2A wire version the agent is pinned to; the gateway does
//! not translate between the 0.3 and 1.0 formats here.
//!
//! `message/stream` and `tasks/resubscribe` are relayed as a live SSE stream —
//! each event reaches the caller as the agent pushes it, which is the whole
//! point of those methods: an A2A task runs for minutes or hours and reports
//! progress as it goes. Those calls carry no upstream deadline for the same
//! reason (the unary `timeout_ms` would cut every long task off), so downstream
//! liveness rests on the SSE keep-alive. Their usage event and quota
//! reservation are tied to the stream's real lifetime via a drop guard, so a
//! caller that walks away mid-task is still accounted for.

use std::time::{Duration, Instant};

use aisix_a2a::{
    canonical_operation, is_stream_end, is_streaming_operation, request_text,
    upstream_from_a2a_agent, A2aBridge, A2aCallFacts, A2aError, HttpBridge, ResultText,
};
use aisix_obs::{content_capture_cap, AccessLog, CapturedContent, UsageEvent};
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::auth::AuthenticatedKey;
use crate::reject::AisixPath;
use crate::request_id::new_request_id;
use crate::state::ProxyState;

/// Bounded `model` metric label for /a2a requests — A2A has no resolved model,
/// and the agent name is a path segment (bounded by the registered set, but
/// kept as a fixed label to match the /mcp convention and #451).
const A2A_MODEL_LABEL: &str = "a2a";

/// One A2A call's protocol-level identity, read off its JSON-RPC envelopes and
/// carried to the usage emit.
///
/// [`A2aCallFacts`] keeps accumulating as responses (or stream events) arrive,
/// so the emit records the task the call ended on rather than the one it
/// started with.
struct A2aCall {
    /// The wire method exactly as the caller wrote it — unbounded, kept for
    /// forensics.
    method: String,
    /// The bounded operation that method names, for aggregation.
    operation: &'static str,
    /// The wire version the agent is pinned to (`0.3` / `1.0`).
    protocol_version: &'static str,
    facts: A2aCallFacts,
    /// How the stream behaved, for a streaming call. Left at its default for
    /// a unary one, which observes none of the stream series.
    stream: A2aStreamProgress,
    /// What was said, for token metering and opt-in content capture.
    text: A2aCallText,
    /// Set when a content policy blocked this call, so the usage event
    /// records it the way a blocked LLM request is recorded.
    guardrail_blocked: bool,
    /// The `{kind, hook}` set of policies that governed the call, and any
    /// monitor-mode observations they made.
    applied_guardrails: Vec<aisix_core::AppliedGuardrail>,
    guardrail_monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    /// What a `mask` rule rewrote, by entity type. Counts only — the event
    /// must not become the sink for the content the rule removed.
    redactions: crate::redact::RedactionCounts,
}

/// The words exchanged on one A2A call.
///
/// An agent reports no token usage — there is no `usage` block in the
/// protocol — so the only way a call can be metered at all is to count what
/// passed through. Both buffers are bounded by
/// [`crate::token_estimate::push_capped`]: a request body has no size limit by
/// default and an A2A task may stream for hours, and both are retained for the
/// life of the call. Past the bound the text becomes a prefix and the estimate
/// a lower bound, rather than the buffer growing without limit.
struct A2aCallText {
    /// The caller's message text.
    request: String,
    /// The agent's, under the protocol's own per-artifact append/replace rule.
    response: ResultText,
}

/// What a streamed A2A call did on the wire, accumulated as events pass.
///
/// A stream is the only place these can be observed: once the response head is
/// out, nothing downstream of it can say how long the first event took, how
/// many followed, or whether the caller was still there at the end.
#[derive(Default)]
struct A2aStreamProgress {
    /// Time from the call starting to the upstream's first event. `None` until
    /// one arrives — a stream that opens and produces nothing never sets it,
    /// and no figure is invented for it.
    ttfb: Option<Duration>,
    /// Events relayed downstream.
    event_count: u32,
    /// Set when the upstream stream ends — exhausted or faulted. Still false
    /// when the guard drops means the generator was cancelled underneath us,
    /// which is a caller that hung up mid-task.
    reached_end: bool,
}

/// Whether an operation's words are the agent GENERATING something, and so
/// worth metering and capturing.
///
/// `message/send` and `message/stream` are the two that make an agent produce.
/// Everything else reads back a task the gateway has already accounted for:
/// `tasks/get` returns the whole task on every poll and `tasks/resubscribe`
/// replays its stream from the start, so counting those would report one
/// answer as many times as a client cared to look at it.
fn meters_content(operation: &str) -> bool {
    matches!(operation, "message/send" | "message/stream")
}

/// Serve a JSON-RPC request to `/a2a/:agent`. Authentication (`401`), per-agent
/// ACL (`403`), and rate-limit + budget (`429` / budget error) gate the call
/// before the request is forwarded to the upstream agent; a usage event is
/// emitted either way.
pub async fn a2a_endpoint(
    auth: AuthenticatedKey,
    client: crate::client_ip::ClientContext,
    AisixPath(agent): AisixPath<String>,
    State(state): State<ProxyState>,
    request: Request,
) -> Response {
    let started = Instant::now();
    let request_id = request
        .extensions()
        .get::<crate::request_id::RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_else(new_request_id);
    let api_key_id = auth.entry.id.clone();
    let http_method = request.method().clone();
    // `dispatch` takes the key by value; the terminal emit below still needs
    // the caller's team / user labels (the handle is an `Arc` clone).
    let caller_auth = auth.clone();

    let response = dispatch(auth, &client, &agent, &state, request, &request_id).await;

    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    AccessLog {
        method: http_method.as_str(),
        path: "/a2a",
        status,
        latency: elapsed,
        provider: Some("a2a"),
        model: None,
        api_key_id: Some(&api_key_id),
        // Counted inside `dispatch`, which hands back only a rendered
        // `Response` — and for a stream, not until its drop guard fires, long
        // after this line. The usage event carries them.
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        request_id: &request_id,
        // Same as `/mcp`: `dispatch` returns an already-rendered `Response`,
        // so no typed error reaches this point.
        error_kind: None,
        error: None,
        provider_request_id: None,
        served_by_model: None,
        routing_attempt_count: None,
        routing_fallback_count: None,
    }
    .emit();
    crate::request_metrics::record(
        &state,
        "/a2a",
        crate::request_metrics::Caller::new(&caller_auth),
        crate::request_metrics::Upstream {
            provider: "a2a",
            model: A2A_MODEL_LABEL,
            ..Default::default()
        },
        status,
        elapsed,
    );
    response
}

async fn dispatch(
    auth: AuthenticatedKey,
    client: &crate::client_ip::ClientContext,
    agent: &str,
    state: &ProxyState,
    request: Request,
    request_id: &str,
) -> Response {
    // Resolve the agent from the live snapshot. A disabled agent is treated as
    // absent — not served, same as a missing one.
    let snapshot = state.snapshot.load();
    let entry = match snapshot.a2a_agents.get_by_name(agent) {
        Some(entry) if entry.value.enabled => entry,
        _ => return (StatusCode::NOT_FOUND, format!("unknown A2A agent: {agent}")).into_response(),
    };

    // Per-agent access control, keyed on the same API key object as LLM/MCP
    // access. A key with no `allowed_agents` reaches none (grant is explicit).
    if !auth.key().can_access_agent(agent) {
        return (
            StatusCode::FORBIDDEN,
            format!("this key may not reach A2A agent: {agent}"),
        )
            .into_response();
    }

    // Client IP allowlist. A 403 rather than the absent-fold `/mcp` uses:
    // this endpoint already discloses that an agent exists by answering the
    // ACL denial above with 403, so hiding only the IP case would buy nothing
    // and would make two refusals for the same agent answer differently.
    if !entry.value.ip_allowed(&client.source_ip) {
        tracing::warn!(
            agent = %agent,
            source_ip = %client.source_ip,
            "A2A request rejected: client IP not in agent allowed_cidrs",
        );
        return (
            StatusCode::FORBIDDEN,
            format!("client IP not allowed for A2A agent: {agent}"),
        )
            .into_response();
    }

    let upstream = upstream_from_a2a_agent(&entry.value);

    let (_parts, body) = request.into_parts();
    let body_limit = state.request_body_limit_for("/a2a/:agent");
    let bytes = match to_bytes(body, crate::error::body_read_cap(body_limit)).await {
        Ok(bytes) => bytes,
        // Cap hit → 413 in the standard envelope, matching the
        // Content-Length middleware's answer on this route.
        Err(err) if crate::error::is_length_limit_error(&err) => {
            return crate::error::ProxyError::RequestTooLarge {
                limit_bytes: body_limit,
            }
            .into_response();
        }
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid request body").into_response(),
    };
    let mut value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON-RPC body").into_response(),
    };
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let rpc_id = value.get("id").cloned();
    let operation = canonical_operation(&method);
    let mut call = A2aCall {
        operation,
        method,
        protocol_version: upstream.protocol_version.as_wire_str(),
        facts: A2aCallFacts::default(),
        stream: A2aStreamProgress::default(),
        text: A2aCallText {
            // Only the operations that make an agent GENERATE are metered.
            // A read (`tasks/get`, `tasks/resubscribe`) hands back the same
            // answer on every poll, and counting those would let a client
            // polling a ten-minute task report its answer six hundred times —
            // swamping exactly the per-agent figures this exists to produce.
            request: if meters_content(operation) {
                request_text(&value, crate::token_estimate::push_capped)
            } else {
                String::new()
            },
            response: ResultText::default(),
        },
        guardrail_blocked: false,
        applied_guardrails: Vec::new(),
        guardrail_monitor_hits: Vec::new(),
        redactions: crate::redact::RedactionCounts::new(),
    };
    // Read before the upstream is contacted, so a call that never lands still
    // records which task the caller was asking about.
    call.facts.observe_request(&value);

    // Content policy over the words. A2A had auth, access control, rate
    // limiting and budget — every gate except the one about what is being
    // said — so a rule that blocked a phrase on /v1/chat/completions was
    // bypassable by asking an agent the same thing.
    //
    // The chain resolves with no model and no MCP server, because an A2A call
    // has neither: `env`, `api_key` and `team` scoped policies therefore
    // govern it, which is what "a global content policy applies everywhere"
    // means. A per-agent scope would need a new attachment dimension in the
    // resource model and the paired control-plane work; it is not something
    // this can invent locally.
    let guardrail_chain = state
        .guardrail_index
        .resolve(&aisix_guardrails::RequestContext {
            passthrough_route_id: "",
            model_id: "",
            mcp_server_id: "",
            api_key_id: &auth.entry.id,
            team_id: auth.key().team_id.as_deref(),
        });
    call.applied_guardrails = guardrail_chain.applied().to_vec();
    // A push notification is the agent POSTing the task's progress straight
    // to a webhook the caller names, so an answer delivered that way never
    // comes back through here for the output check — asking for one routes
    // around it. While an output guardrail can refuse an answer, a call that
    // sets up push delivery is answered the way the specification has an
    // agent without push support answer it, so a client falls back to
    // polling or streaming, both of which are checked.
    if enforces_output(&guardrail_chain) && aisix_a2a::requests_push_delivery(&value) {
        tracing::warn!(
            guardrail_hook = "output",
            agent = %agent,
            operation = %call.operation,
            "guardrail refused A2A push notification setup",
        );
        call.guardrail_blocked = true;
        emit_a2a_usage(
            state,
            &snapshot,
            &auth,
            request_id,
            agent,
            &call,
            StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
            Duration::ZERO,
        );
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(a2a_error_envelope_coded(
                rpc_id,
                PUSH_NOTIFICATION_NOT_SUPPORTED,
                "push notifications are not available while a content policy reviews this \
                 agent's answers; poll or stream the task instead",
            )),
        )
            .into_response();
    }
    // Run BEFORE the quota gate so a content-policy refusal does not burn an
    // RPM slot, matching /v1/chat/completions (#542).
    //
    // Scan every word the agent will receive: text parts, and the string
    // values and keys of data parts, uncapped. `call.text.request` is capped
    // for metering and reads text parts only, and the body is forwarded
    // whole: scanning that copy let anything past the cap, or inside a data
    // part, through unread. Decided on the message itself rather than on the
    // operation, so any call that hands an agent a message is read.
    let request_scan = (!guardrail_chain.is_empty()).then(|| aisix_a2a::request_scan(&value));
    // Content the scan cannot read — file bytes that are neither text nor a
    // binary media type, a value of the wrong type where the protocol has
    // text — is refused rather than forwarded unread: whatever reads it on
    // the other side may decode it more leniently than any scan predicts.
    if let Some(why) = request_scan.as_ref().and_then(|scan| scan.unreadable) {
        tracing::warn!(
            guardrail_hook = "input",
            agent = %agent,
            operation = %call.operation,
            reason = why,
            "guardrail blocked A2A request: content the scan cannot read",
        );
        call.guardrail_blocked = true;
        emit_a2a_usage(
            state,
            &snapshot,
            &auth,
            request_id,
            agent,
            &call,
            StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
            Duration::ZERO,
        );
        return a2a_error_response(
            rpc_id,
            StatusCode::UNPROCESSABLE_ENTITY,
            &crate::error::guardrail_block_message("request", None),
        );
    }
    let request_words = request_scan.map(|scan| scan.text).unwrap_or_default();
    if !request_words.is_empty() {
        let chat = aisix_gateway::ChatFormat::new(
            "",
            vec![aisix_gateway::ChatMessage::user(request_words)],
        );
        let (verdict, hits) =
            aisix_guardrails::Guardrail::check_input_observed(&guardrail_chain, &chat).await;
        call.guardrail_monitor_hits.extend(hits);
        if let aisix_guardrails::GuardrailVerdict::Block {
            reason,
            guardrail_name,
        } = verdict
        {
            tracing::warn!(
                guardrail_hook = "input",
                agent = %agent,
                operation = %call.operation,
                reason = %reason,
                "guardrail blocked A2A request",
            );
            call.guardrail_blocked = true;
            emit_a2a_usage(
                state,
                &snapshot,
                &auth,
                request_id,
                agent,
                &call,
                StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                Duration::ZERO,
            );
            return a2a_error_response(
                rpc_id,
                StatusCode::UNPROCESSABLE_ENTITY,
                &crate::error::guardrail_block_message("request", guardrail_name.as_deref()),
            );
        }

        // #932's order: block first, then rewrite, so the masked text is what
        // the agent receives.
        //
        // This module's own header is the argument for doing it here at all —
        // content policy applies "so a rule that blocks a phrase on an LLM
        // endpoint is not bypassable by asking an agent the same thing". A
        // MASK rule was exactly that bypassable: the chain resolved, the text
        // was scanned, and nothing rewrote it, so an env-scoped `pii` mask
        // that redacts a prompt on /v1/chat/completions let the same content
        // through here untouched — with no block, no rewrite, and no record
        // (`would_mask` is monitor-mode only).
        //
        // Only `params.message.parts[].text` is rewritten. The ids, method
        // name and metadata around them are protocol rather than caller
        // content. The scan above also read data parts, and any parts list
        // outside `params.message`, which the rewrite does not reach (see
        // `request_would_mask_unrewritable`): a mask that would change one
        // refuses the call, the way the other endpoints refuse a structural
        // field they cannot rewrite, rather than forwarding it unmasked.
        if aisix_guardrails::Guardrail::redacts_input(&guardrail_chain) {
            let chain = &guardrail_chain;
            if aisix_a2a::request_would_mask_unrewritable(&value, |text| {
                aisix_guardrails::Guardrail::redact_input_text(chain, text).map(|r| r.text)
            }) {
                tracing::warn!(
                    guardrail_hook = "input",
                    agent = %agent,
                    operation = %call.operation,
                    "guardrail blocked A2A request: a mask rule matched content the rewrite cannot reach",
                );
                call.guardrail_blocked = true;
                emit_a2a_usage(
                    state,
                    &snapshot,
                    &auth,
                    request_id,
                    agent,
                    &call,
                    StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                    Duration::ZERO,
                );
                return a2a_error_response(
                    rpc_id,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &crate::error::guardrail_block_message("request", None),
                );
            }
            // Collected outside the closure: `redact_request_text` takes an
            // `Fn`, so the counts cannot be merged into `call` from inside it.
            let collected = std::cell::RefCell::new(crate::redact::RedactionCounts::new());
            let rewritten = aisix_a2a::redact_request_text(&mut value, |text| {
                aisix_guardrails::Guardrail::redact_input_text(chain, text).map(|r| {
                    crate::redact::merge_counts(&mut collected.borrow_mut(), r.counts);
                    r.text
                })
            });
            crate::redact::merge_counts(&mut call.redactions, collected.into_inner());
            if rewritten > 0 && meters_content(call.operation) {
                // Keep the inspected copy in step with what was sent, so the
                // usage event's token estimate and any captured content
                // describe the same text the agent got. Only for a metered
                // operation: the scan reads any call that carries a message,
                // and rewriting one must not start metering it.
                call.text.request = aisix_a2a::request_text(&value, |b, s| b.push_str(s));
            }
        }
    }

    // Reuse the LLM path's rate-limit + budget gate. The reservation is held
    // for the call and released without committing tokens: the counts this
    // endpoint reports are the gateway's own reading of the words, not an
    // agent's billed usage, so they are REPORTED but never CHARGED. Token
    // windows and token budgets therefore do not move on A2A traffic, which
    // is deliberate — inferring a spend limit from an estimate would throttle
    // callers on a number no provider ever confirmed. On 429 /
    // budget-exceeded this returns before the upstream is contacted.
    let reservation = match crate::quota::enforce(state, &snapshot, &auth, None).await {
        Ok(reservation) => reservation,
        Err(err) => {
            let response = err.into_response();
            emit_a2a_usage(
                state,
                &snapshot,
                &auth,
                request_id,
                agent,
                &call,
                response.status().as_u16(),
                Duration::ZERO,
            );
            return response;
        }
    };

    if is_streaming_operation(call.operation) {
        return dispatch_stream(
            auth,
            agent,
            state,
            &snapshot,
            request_id,
            upstream,
            value,
            call,
            rpc_id,
            reservation,
            guardrail_chain,
        )
        .await;
    }
    let _reservation = reservation;

    let bridge = HttpBridge::new(upstream);
    let started = Instant::now();
    let result = bridge.send(&value).await;
    let latency = started.elapsed();

    match result {
        Ok(response_value) => {
            call.facts.observe_result(&response_value);
            if meters_content(call.operation) {
                call.text
                    .response
                    .observe(&response_value, crate::token_estimate::push_capped);
            }
            // Output policy over the agent's answer. The agent has already
            // done the work by now, so a block replaces the answer rather
            // than preventing it — the same trade the LLM output hook makes,
            // and the reason the usage event is still emitted.
            //
            // A streamed answer is reviewed in `dispatch_stream`, held back
            // under the chain's streamed-output policy.
            //
            // The full observed check, not the `_non_segment` one: a masking
            // segment moderator (bedrock/lakera/presidio/aliyun_ai_guardrail)
            // is skipped by the latter on the promise of a segment pass, and
            // an A2A answer has no masked write-back to run one with. Like
            // the input hook above, such a guardrail answers through its
            // check here, where a maskable answer is blocked.
            //
            // Every unary result is checked, not only the metered operations:
            // metering skips a read because it restates an answer already
            // counted, but a read hands the caller those words all the same,
            // so a `tasks/get` would read back an answer `message/send` was
            // refused.
            if !guardrail_chain.is_empty() {
                // Everything the caller receives, uncapped: the whole
                // envelope — `result` and `error` alike — every key, string
                // and number, file content decoded. The metered copy in
                // `call.text.response` is capped and reads text parts of the
                // answer only.
                let scan = aisix_a2a::result_scan(&response_value);
                // Refused only where a guardrail would act on the answer: an
                // input-only or monitor-mode chain never refuses one, and
                // must not start doing so here.
                if let Some(why) = scan
                    .unreadable
                    .filter(|_| enforces_output(&guardrail_chain))
                {
                    tracing::warn!(
                        guardrail_hook = "output",
                        agent = %agent,
                        operation = %call.operation,
                        reason = why,
                        "guardrail blocked A2A response: content the scan cannot read",
                    );
                    call.guardrail_blocked = true;
                    emit_a2a_usage(
                        state,
                        &snapshot,
                        &auth,
                        request_id,
                        agent,
                        &call,
                        StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                        latency,
                    );
                    return a2a_error_response(
                        rpc_id,
                        StatusCode::UNPROCESSABLE_ENTITY,
                        &crate::error::guardrail_block_message("response", None),
                    );
                }
                let answer = scan.text;
                if !answer.is_empty() {
                    let (verdict, hits) = check_answer(&guardrail_chain, answer).await;
                    call.guardrail_monitor_hits.extend(hits);
                    if let aisix_guardrails::GuardrailVerdict::Block {
                        reason,
                        guardrail_name,
                    } = verdict
                    {
                        tracing::warn!(
                            guardrail_hook = "output",
                            agent = %agent,
                            operation = %call.operation,
                            reason = %reason,
                            "guardrail blocked A2A response",
                        );
                        call.guardrail_blocked = true;
                        emit_a2a_usage(
                            state,
                            &snapshot,
                            &auth,
                            request_id,
                            agent,
                            &call,
                            StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                            latency,
                        );
                        return a2a_error_response(
                            rpc_id,
                            StatusCode::UNPROCESSABLE_ENTITY,
                            &crate::error::guardrail_block_message(
                                "response",
                                guardrail_name.as_deref(),
                            ),
                        );
                    }
                }
            }
            emit_a2a_usage(
                state,
                &snapshot,
                &auth,
                request_id,
                agent,
                &call,
                StatusCode::OK.as_u16(),
                latency,
            );
            axum::Json(response_value).into_response()
        }
        Err(err) => {
            let status = a2a_error_status(&err);
            tracing::warn!(agent = %agent, error = %err, "A2A upstream call failed");
            emit_a2a_usage(
                state,
                &snapshot,
                &auth,
                request_id,
                agent,
                &call,
                status.as_u16(),
                latency,
            );
            a2a_error_response(rpc_id, status, &err.to_string())
        }
    }
}

/// Fires the A2A usage event once a streamed call is over, and owns the
/// concurrency hold for that whole span.
///
/// Both jobs have to happen on drop rather than at the end of a loop: a caller
/// walking away mid-task drops the stream without it ever completing, and that
/// is the ordinary case for a long-running A2A task, not an edge one. Emitting
/// from `Drop` means such a call still lands in usage.
///
/// The hold is a [`StreamConcurrencyGuard`], the same type the LLM streaming
/// path uses. Releasing the reservation at handler return instead would free
/// the caller's slot the moment the response headers went out, letting a key
/// capped at N run many more than N concurrent streams — #450, on a second
/// endpoint.
struct StreamUsageOnDrop {
    state: ProxyState,
    auth: AuthenticatedKey,
    request_id: String,
    agent: String,
    /// The call's protocol identity. Its [`A2aCallFacts`] keep accumulating as
    /// events arrive, so a caller that walks away mid-task is recorded against
    /// the last state the upstream actually reported rather than an invented
    /// terminal one.
    call: A2aCall,
    started: Instant,
    /// What to record for the call. Starts at the status already sent to the
    /// caller and is downgraded if the stream later faults — once the headers
    /// are out, the usage event is the only place that can say so.
    status: u16,
    _concurrency: aisix_ratelimit::StreamConcurrencyGuard,
}

impl Drop for StreamUsageOnDrop {
    fn drop(&mut self) {
        // A panic is already unwinding; emitting here would at best double-report
        // and at worst panic again while panicking.
        if std::thread::panicking() {
            return;
        }
        // A stream the caller abandoned mid-task is recorded as 499, the same
        // outcome the LLM streaming paths record for the same event. Only a
        // still-successful call is downgraded: a stream that already faulted
        // has a truer status than the hang-up that followed it.
        let status = if self.call.stream.reached_end || self.status != StatusCode::OK.as_u16() {
            self.status
        } else {
            crate::CLIENT_CLOSED_REQUEST
        };
        // A stream can outlive several config generations, so the
        // end-of-stream emit reads a FRESH snapshot rather than the one the
        // request started on (#941).
        emit_a2a_usage(
            &self.state,
            &self.state.snapshot.load(),
            &self.auth,
            &self.request_id,
            &self.agent,
            &self.call,
            status,
            self.started.elapsed(),
        );
    }
}

/// Forward a streaming JSON-RPC call as SSE, event by event.
///
/// Each event the upstream pushes is relayed as it arrives, which is the
/// entire point of `message/stream` — a caller watches a long-running task
/// progress instead of waiting for it. Under an output guardrail the events
/// first pass a [`StreamReview`], which holds them only as long as the
/// guardrail's streamed-output policy asks.
#[allow(clippy::too_many_arguments)]
async fn dispatch_stream(
    auth: AuthenticatedKey,
    agent: &str,
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    request_id: &str,
    upstream: aisix_a2a::A2aUpstream,
    request: serde_json::Value,
    call: A2aCall,
    rpc_id: Option<serde_json::Value>,
    reservation: aisix_ratelimit::MultiReservation,
    guardrail_chain: aisix_guardrails::GuardrailChain,
) -> Response {
    let started = Instant::now();
    let bridge = HttpBridge::new(upstream);
    let events = match bridge.send_stream(&request).await {
        Ok(events) => events,
        // The upstream refused before any event: the headers have not gone out,
        // so this is still an ordinary error response rather than a stream.
        Err(err) => {
            let status = a2a_error_status(&err);
            tracing::warn!(agent = %agent, error = %err, "A2A upstream stream failed to open");
            drop(reservation);
            emit_a2a_usage(
                state,
                snapshot,
                &auth,
                request_id,
                agent,
                &call,
                status.as_u16(),
                started.elapsed(),
            );
            return a2a_error_response(rpc_id, status, &err.to_string());
        }
    };

    let mut guard = StreamUsageOnDrop {
        state: state.clone(),
        auth,
        request_id: request_id.to_string(),
        agent: agent.to_string(),
        call,
        started,
        status: StatusCode::OK.as_u16(),
        _concurrency: reservation.into_stream_hold(),
    };
    let agent_label = agent.to_string();
    let mut review = aisix_guardrails::Guardrail::runs_on_output(&guardrail_chain)
        .then(|| StreamReview::new(guardrail_chain));

    // Re-attach the request span: the body is polled after the request-id
    // middleware has returned, so a mid-stream failure would otherwise be
    // logged without its `request_id` and could not be joined to the rest of
    // the request (#1060).
    let sse = crate::request_id::in_request_span(async_stream::stream! {
        let mut events = events;
        // What the stream ends with once the review has had its say: the
        // upstream's own fault, relayed after whatever cleared before it.
        let mut upstream_fault = None;
        let mut refusal = None;
        while let Some(event) = events.next().await {
            let value = match event {
                Ok(value) => value,
                Err(err) => {
                    upstream_fault = Some(err);
                    break;
                }
            };
            // Read the task's progress off the event on its way past —
            // the relay itself stays verbatim.
            guard.call.facts.observe_result(&value);
            if meters_content(guard.call.operation) {
                guard
                    .call
                    .text
                    .response
                    .observe(&value, crate::token_estimate::push_capped);
            }
            // An agent may refuse a streaming call with a JSON-RPC error at
            // HTTP 200, which arrives here as a lone envelope carrying
            // `error` instead of `result`; it is not a task stream, so it
            // does not stamp the time to first event. The agent's own time
            // to first byte is when the event ARRIVED, however long a
            // review then holds it, matching how the LLM paths stamp TTFT.
            if value.get("result").is_some() {
                guard.call.stream.ttfb.get_or_insert_with(|| started.elapsed());
            }
            let ends = is_stream_end(&value);
            let verdict = match review.as_mut() {
                Some(review) => {
                    review
                        .admit(&value, ends, &mut guard.call.guardrail_monitor_hits)
                        .await
                }
                None => StreamVerdict::Release(vec![HeldEvent::of(&value)]),
            };
            let released = match verdict {
                StreamVerdict::Release(released) => released,
                StreamVerdict::Refuse { reason, guardrail_name } => {
                    refusal = Some((reason, guardrail_name));
                    break;
                }
            };
            // Forwarding the terminal event IS the end of the response, and
            // it has to be recorded BEFORE the yield: an A2A client stops
            // reading here, and the loop only resumes when the consumer
            // pulls again — which, for an agent that leaves the connection
            // open after finishing, never happens. Marking the end at the
            // loop exit instead reported every such completed task as
            // abandoned. For the same reason a review releases everything
            // it holds at the terminal event rather than at EOF.
            if ends {
                guard.call.stream.reached_end = true;
            }
            for event in released {
                if event.is_result {
                    guard.call.stream.event_count += 1;
                }
                yield Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default().data(event.data),
                );
            }
        }
        // Whatever the review still holds was read off the upstream before
        // it ran out or faulted: it is released if it scans clean.
        if refusal.is_none() {
            if let Some(review) = review.as_mut() {
                match review.finish(&mut guard.call.guardrail_monitor_hits).await {
                    StreamVerdict::Release(released) => {
                        for event in released {
                            if event.is_result {
                                guard.call.stream.event_count += 1;
                            }
                            yield Ok(axum::response::sse::Event::default().data(event.data));
                        }
                    }
                    StreamVerdict::Refuse { reason, guardrail_name } => {
                        refusal = Some((reason, guardrail_name));
                    }
                }
            }
        }
        if let Some((reason, guardrail_name)) = refusal {
            tracing::warn!(
                guardrail_hook = "output",
                agent = %agent_label,
                operation = %guard.call.operation,
                reason = %reason,
                "guardrail blocked A2A stream",
            );
            // Mid-stream the status line is long gone, so the refusal is
            // told in-band, like the unary refusal's envelope, and nothing
            // the agent sends after it reaches the caller.
            guard.call.guardrail_blocked = true;
            guard.status = StatusCode::UNPROCESSABLE_ENTITY.as_u16();
            guard.call.stream.reached_end = true;
            yield Ok(axum::response::sse::Event::default().data(
                a2a_error_envelope(
                    rpc_id.clone(),
                    &crate::error::guardrail_block_message("response", guardrail_name.as_deref()),
                )
                .to_string(),
            ));
        } else if let Some(err) = upstream_fault {
            // Relayed as a JSON-RPC error event rather than by cutting the
            // connection, which would leave a truncated task looking complete.
            tracing::warn!(agent = %agent_label, error = %err, "A2A stream failed mid-flight");
            guard.status = a2a_error_status(&err).as_u16();
            yield Ok(axum::response::sse::Event::default()
                .data(a2a_error_envelope(rpc_id.clone(), &err.to_string()).to_string()));
        }
        // The upstream ran out or faulted without ever marking the stream
        // finished. Either way the caller was handed everything there was.
        guard.call.stream.reached_end = true;
        drop(guard);
    });

    let mut response = axum::response::Sse::new(sse);
    if let Some(interval) = crate::sse_keepalive::interval() {
        response = response.keep_alive(axum::response::sse::KeepAlive::new().interval(interval));
    }
    response.into_response()
}

/// Upper bound on what a [`StreamReview`] holds, or reads without scanning,
/// whatever the policy's own threshold. An A2A task may stream for hours: a
/// window whose character threshold events with little text never reach, or
/// a policy that scans only at the end, must not grow without bound.
const MAX_HELD_STREAM_BYTES: usize = 1024 * 1024;

/// How much of each artifact's text a review keeps across scans when the
/// policy names no overlap of its own (whole-answer hold-back, monitor mode).
const DEFAULT_CARRY_CHARS: usize = 256;

/// How many artifacts a review follows at once. A stream that opens more
/// stops being followed for the oldest; each chunk is still scanned whole.
const MAX_FOLLOWED_ARTIFACTS: usize = 64;

/// One streamed event, serialized for the wire.
struct HeldEvent {
    data: String,
    /// Carries a `result` — the events the stream series count.
    is_result: bool,
}

impl HeldEvent {
    fn of(event: &serde_json::Value) -> Self {
        Self {
            data: event.to_string(),
            is_result: event.get("result").is_some(),
        }
    }
}

/// What a [`StreamReview`] decided about the events read so far.
enum StreamVerdict {
    /// These events are cleared for the caller now, in arrival order. Empty
    /// while the policy is still holding.
    Release(Vec<HeldEvent>),
    /// The answer is refused: nothing still held reaches the caller.
    Refuse {
        reason: String,
        guardrail_name: Option<String>,
    },
}

/// The text of one artifact as the caller assembles it from its chunks, for
/// the part not yet scanned plus what is carried from the last scan.
struct FollowedArtifact {
    id: String,
    text: String,
    /// Grew since the last scan.
    fresh: bool,
}

/// Output review of a streamed answer: holds events back under the chain's
/// streamed-output policy and releases them once they scan clean — the same
/// policies, with the same meaning, as the LLM streaming paths apply. A
/// whole-answer policy holds every event until the stream's terminal event; a
/// window releases events as each window scans clean, carrying its overlap
/// into the next; a chain whose output members are all in monitor mode
/// forwards live and only observes.
///
/// An A2A event is a whole JSON-RPC message, so the unit held and released is
/// an event, never part of one. Each event is read the way a unary result is
/// — every key, string and number, file content decoded — and an event with
/// content the scan cannot read refuses the answer, as a unary result would.
/// An answer streamed in artifact chunks is ALSO read as the caller assembles
/// it: a chunk with `append` continues its artifact's text, so a phrase
/// started in one event and finished in the next is read whole.
struct StreamReview {
    chain: aisix_guardrails::GuardrailChain,
    policy: aisix_guardrails::StreamOutputPolicy,
    held: Vec<HeldEvent>,
    held_bytes: usize,
    /// Scan text of the events read since the last scan, newline-separated.
    unscanned: String,
    unscanned_chars: usize,
    /// The end of the last scanned text, read again in front of the next.
    overlap: String,
    artifacts: Vec<FollowedArtifact>,
    /// Set once a fail-open buffer cap has been exceeded: from then on events
    /// pass unreviewed, as the guardrail's own setting asks.
    fail_opened: bool,
}

impl StreamReview {
    fn new(chain: aisix_guardrails::GuardrailChain) -> Self {
        let policy = aisix_guardrails::Guardrail::stream_output_policy(&chain);
        Self {
            chain,
            policy,
            held: Vec::new(),
            held_bytes: 0,
            unscanned: String::new(),
            unscanned_chars: 0,
            overlap: String::new(),
            artifacts: Vec::new(),
            fail_opened: false,
        }
    }

    /// Read one event off the upstream. `ends` marks the stream's terminal
    /// event, at which everything held is scanned and released at once: a
    /// client stops reading there, so holding on for EOF would leave it
    /// waiting on an agent that keeps the connection open.
    async fn admit(
        &mut self,
        event: &serde_json::Value,
        ends: bool,
        hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
    ) -> StreamVerdict {
        use aisix_guardrails::StreamOutputPolicy;

        let held = HeldEvent::of(event);
        if self.fail_opened {
            return StreamVerdict::Release(vec![held]);
        }
        let scan = aisix_a2a::result_scan(event);
        if let Some(why) = scan.unreadable.filter(|_| self.policy.holds_back()) {
            return StreamVerdict::Refuse {
                reason: format!("streamed {why}"),
                guardrail_name: None,
            };
        }
        if let Some(chunk) = aisix_a2a::artifact_chunk(event) {
            self.follow(chunk);
        }
        if !self.unscanned.is_empty() {
            self.unscanned.push('\n');
        }
        self.unscanned_chars += scan.text.chars().count();
        self.unscanned.push_str(&scan.text);

        match self.policy.clone() {
            StreamOutputPolicy::EndOfStreamCheck => {
                // Observing only: the event goes out now, and the text is
                // read in bounded batches rather than kept until the end.
                if ends || self.unscanned.len() > MAX_HELD_STREAM_BYTES {
                    if let Some(refusal) = self.scan(hits).await {
                        return refusal;
                    }
                }
                StreamVerdict::Release(vec![held])
            }
            StreamOutputPolicy::Window { size_chars, .. } => {
                self.hold(held);
                if ends
                    || self.unscanned_chars >= size_chars
                    || self.held_bytes > MAX_HELD_STREAM_BYTES
                {
                    self.release_if_clean(hits).await
                } else {
                    StreamVerdict::Release(Vec::new())
                }
            }
            StreamOutputPolicy::BufferFull {
                max_buffer_bytes,
                on_exceeded_fail_open,
            } => {
                self.hold(held);
                if self.held_bytes > max_buffer_bytes {
                    if on_exceeded_fail_open {
                        tracing::warn!(
                            "A2A stream exceeded the guardrail buffer cap; releasing it unscanned (fail-open)",
                        );
                        self.fail_opened = true;
                        return StreamVerdict::Release(std::mem::take(&mut self.held));
                    }
                    return StreamVerdict::Refuse {
                        reason: "streamed answer exceeded the guardrail buffer cap".to_string(),
                        guardrail_name: None,
                    };
                }
                if ends {
                    self.release_if_clean(hits).await
                } else {
                    StreamVerdict::Release(Vec::new())
                }
            }
        }
    }

    /// The upstream is done, with or without a terminal event: scan what has
    /// not been scanned and release what is still held.
    async fn finish(&mut self, hits: &mut Vec<aisix_core::GuardrailMonitorHit>) -> StreamVerdict {
        if self.fail_opened || (self.unscanned.is_empty() && self.held.is_empty()) {
            return StreamVerdict::Release(std::mem::take(&mut self.held));
        }
        self.release_if_clean(hits).await
    }

    fn hold(&mut self, event: HeldEvent) {
        self.held_bytes += event.data.len();
        self.held.push(event);
    }

    async fn release_if_clean(
        &mut self,
        hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
    ) -> StreamVerdict {
        if let Some(refusal) = self.scan(hits).await {
            return refusal;
        }
        self.held_bytes = 0;
        StreamVerdict::Release(std::mem::take(&mut self.held))
    }

    /// Continue (or, without `append`, restart) the text of the artifact a
    /// chunk belongs to.
    fn follow(&mut self, chunk: aisix_a2a::ArtifactChunk) {
        if let Some(artifact) = self
            .artifacts
            .iter_mut()
            .find(|a| a.id == chunk.artifact_id)
        {
            if !chunk.append {
                artifact.text.clear();
            }
            artifact.text.push_str(&chunk.text);
            artifact.fresh = true;
            return;
        }
        if self.artifacts.len() >= MAX_FOLLOWED_ARTIFACTS {
            self.artifacts.remove(0);
        }
        self.artifacts.push(FollowedArtifact {
            id: chunk.artifact_id,
            text: chunk.text,
            fresh: true,
        });
    }

    /// Scan everything read since the last scan — with the overlap carried
    /// from it, and each artifact that grew read as one text — and roll the
    /// carried state forward when it is clean.
    async fn scan(
        &mut self,
        hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
    ) -> Option<StreamVerdict> {
        let mut text = format!("{}{}", self.overlap, self.unscanned);
        for artifact in self.artifacts.iter().filter(|a| a.fresh) {
            text.push('\n');
            text.push_str(&artifact.text);
        }
        if !text.trim().is_empty() {
            let (verdict, found) = check_answer(&self.chain, text).await;
            hits.extend(found);
            if let aisix_guardrails::GuardrailVerdict::Block {
                reason,
                guardrail_name,
            } = verdict
            {
                return Some(StreamVerdict::Refuse {
                    reason,
                    guardrail_name,
                });
            }
        }
        let carry = match self.policy {
            aisix_guardrails::StreamOutputPolicy::Window { overlap_chars, .. } => overlap_chars,
            _ => DEFAULT_CARRY_CHARS,
        };
        self.overlap = tail_chars(&format!("{}{}", self.overlap, self.unscanned), carry);
        for artifact in self.artifacts.iter_mut().filter(|a| a.fresh) {
            artifact.text = tail_chars(&artifact.text, carry);
            artifact.fresh = false;
        }
        self.unscanned.clear();
        self.unscanned_chars = 0;
        None
    }
}

/// The last `n` characters of `s`.
fn tail_chars(s: &str, n: usize) -> String {
    let skip = s.chars().count().saturating_sub(n);
    s.chars().skip(skip).collect()
}

/// Whether an output guardrail that can refuse an answer applies: one on the
/// output hook that is not in monitor mode. The same test the LLM streaming
/// paths use to decide whether to hold a stream back.
fn enforces_output(chain: &aisix_guardrails::GuardrailChain) -> bool {
    use aisix_guardrails::Guardrail as _;
    chain.runs_on_output() && chain.stream_output_policy().holds_back()
}

/// Run the output check over an agent's words, read as one assistant answer.
async fn check_answer(
    chain: &aisix_guardrails::GuardrailChain,
    text: String,
) -> (
    aisix_guardrails::GuardrailVerdict,
    Vec<aisix_core::GuardrailMonitorHit>,
) {
    let synth = aisix_gateway::ChatResponse {
        id: String::new(),
        model: String::new(),
        message: aisix_gateway::ChatMessage::assistant(text),
        finish_reason: aisix_gateway::FinishReason::Stop,
        usage: aisix_gateway::UsageStats::default(),
    };
    aisix_guardrails::Guardrail::check_output_observed(chain, &synth).await
}

/// Serve the upstream agent's card at `/a2a/:agent/.well-known/agent-card.json`,
/// rewriting its advertised service `url` to point back at this gateway so
/// callers discover the agent through `/a2a/<agent>`.
pub async fn a2a_agent_card(
    auth: AuthenticatedKey,
    AisixPath(agent): AisixPath<String>,
    State(state): State<ProxyState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let snapshot = state.snapshot.load();
    let entry = match snapshot.a2a_agents.get_by_name(&agent) {
        Some(entry) if entry.value.enabled => entry,
        _ => return (StatusCode::NOT_FOUND, format!("unknown A2A agent: {agent}")).into_response(),
    };
    if !auth.key().can_access_agent(&agent) {
        return (
            StatusCode::FORBIDDEN,
            format!("this key may not reach A2A agent: {agent}"),
        )
            .into_response();
    }
    // Resolved BEFORE the upstream is contacted: without a public base there is
    // no card this gateway can serve, and finding that out after the fetch only
    // wastes an upstream round trip.
    let Some(base) = gateway_base(&uri, &headers) else {
        tracing::warn!(
            agent = %agent,
            "cannot derive the gateway's public base for an A2A agent card; refusing to serve one"
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot determine the gateway's public address for this agent card",
        )
            .into_response();
    };

    let upstream = upstream_from_a2a_agent(&entry.value);

    let bridge = HttpBridge::new(upstream);
    let mut card = match bridge.fetch_agent_card().await {
        Ok(card) => card,
        Err(err) => {
            tracing::warn!(agent = %agent, error = %err, "A2A agent card fetch failed");
            return (StatusCode::BAD_GATEWAY, err.to_string()).into_response();
        }
    };
    // Rewrite the advertised service endpoints to the gateway so downstream
    // callers route subsequent requests through `/a2a/<agent>`.
    rewrite_card_urls(&mut card, &format!("{base}/a2a/{agent}"));
    axum::Json(card).into_response()
}

/// Point every service URL the card advertises at the gateway.
///
/// The top-level `url` is what a 0.3 caller reads. A 1.0 caller instead picks
/// its endpoint out of `supportedInterfaces` (`additionalInterfaces` on a 0.3
/// card), so rewriting only the top level leaves it reading the upstream
/// address off the card and calling the agent directly — no auth, no quota, no
/// usage, and the internal address handed to the caller on the way past.
///
/// Entries are rewritten in place rather than filtered out: this gateway serves
/// JSON-RPC over HTTP only, so an entry naming a transport it does not proxy
/// now points somewhere that will reject the caller. That is the intended
/// trade — failing loudly beats silently bypassing governance.
fn rewrite_card_urls(card: &mut aisix_a2a::AgentCard, gateway_url: &str) {
    card.url = gateway_url.to_string();
    for key in ["supportedInterfaces", "additionalInterfaces"] {
        let Some(serde_json::Value::Array(interfaces)) = card.rest.get_mut(key) else {
            continue;
        };
        for interface in interfaces {
            if let Some(url) = interface.get_mut("url") {
                *url = serde_json::Value::String(gateway_url.to_string());
            }
        }
    }
}

/// Reconstruct the gateway's public base (`scheme://host`) for a request: the
/// authority, and `X-Forwarded-Proto` when a proxy set it (defaulting to
/// `https`).
///
/// The authority comes from the `Host` header, falling back to the request
/// URI's own authority. HTTP/2 carries it there as the `:authority`
/// pseudo-header and sends no `Host` at all — and this listener negotiates h2
/// whenever `proxy.tls` is set, so header-only lookup finds nothing on exactly
/// the deployments most likely to be in production.
///
/// `None` means no card can be served: the caller must fail rather than hand
/// back a card still advertising the upstream's own address.
fn gateway_base(uri: &axum::http::Uri, headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .or_else(|| uri.authority().map(|a| a.as_str()))?;
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    Some(format!("{scheme}://{host}"))
}

/// Map a bridge error to the client-visible HTTP status: whether the upstream
/// could not be reached or the call itself failed, the gateway surfaces it as
/// a bad gateway.
fn a2a_error_status(err: &A2aError) -> StatusCode {
    match err {
        A2aError::Connect(_) | A2aError::Request(_) => StatusCode::BAD_GATEWAY,
    }
}

/// Build a JSON-RPC error envelope for a gateway-side failure, echoing the
/// request id. A2A clients expect a JSON-RPC body, so the failure surfaces as
/// an error object they can handle rather than a bare HTTP error.
fn a2a_error_response(
    id: Option<serde_json::Value>,
    status: StatusCode,
    message: &str,
) -> Response {
    (status, axum::Json(a2a_error_envelope(id, message))).into_response()
}

/// The JSON-RPC error object itself, for the streaming path — mid-stream there
/// is no status line left to carry the failure, so the same envelope has to
/// travel as an event.
fn a2a_error_envelope(id: Option<serde_json::Value>, message: &str) -> serde_json::Value {
    a2a_error_envelope_coded(id, -32000, message)
}

/// `PushNotificationNotSupportedError`: the code the specification has an
/// agent answer push notification setup with when it does not offer push
/// (<https://a2a-protocol.org/v0.3.0/specification/> §8.2,
/// <https://a2a-protocol.org/latest/specification/> §5.4).
const PUSH_NOTIFICATION_NOT_SUPPORTED: i64 = -32003;

/// A JSON-RPC error envelope carrying a specific error code.
fn a2a_error_envelope_coded(
    id: Option<serde_json::Value>,
    code: i64,
    message: &str,
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(serde_json::Value::Null),
        "error": { "code": code, "message": message },
    })
}

/// Emit a usage event for a single A2A call into the same sink as LLM usage.
///
/// The event records who called which agent with which operation, which task
/// it touched, the outcome, and latency. Token counts are the gateway's own
/// reading of the words that passed through — an agent reports none of its
/// own — and are flagged `usage_estimated`; `cost_usd` stays zero, since what
/// an agent charges is not something the gateway can know.
///
/// This is the chokepoint every A2A path emits through, so the metric
/// families ride here too: a path that accounts for a call cannot skip
/// metering it.
#[allow(clippy::too_many_arguments)]
fn emit_a2a_usage(
    state: &ProxyState,
    // The request's snapshot, loaded once by the caller (#941).
    snap: &aisix_core::AisixSnapshot,
    auth: &AuthenticatedKey,
    request_id: &str,
    agent: &str,
    call: &A2aCall,
    status_code: u16,
    latency: Duration,
) {
    // No model resolves on this endpoint, so the estimator falls back to its
    // default encoding — the same thing it does for any non-OpenAI model.
    let response_text = call.text.response.joined();
    let prompt_tokens = crate::token_estimate::count_text("", &call.text.request);
    let completion_tokens = crate::token_estimate::count_text("", &response_text);
    let mut event = UsageEvent {
        request_id: request_id.to_string(),
        occurred_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        api_key_id: auth.entry.id.clone(),
        status_code,
        // Single-attempt endpoint: the attempt spans the whole request, so
        // the upstream figure and what the caller waited for coincide.
        upstream_latency_ms: latency.as_millis().min(u32::MAX as u128) as u32,
        downstream_latency_ms: latency.as_millis().min(u32::MAX as u128) as u32,
        inbound_protocol: "a2a".to_string(),
        a2a_agent_name: agent.to_string(),
        a2a_method: call.method.clone(),
        a2a_operation: call.operation.to_string(),
        a2a_protocol_version: call.protocol_version.to_string(),
        a2a_task_id: call.facts.task_id.clone(),
        a2a_context_id: call.facts.context_id.clone(),
        a2a_task_state: call.facts.task_state.to_string(),
        a2a_stream_event_count: call.stream.event_count,
        // An A2A agent reports no usage of its own, so these are the
        // gateway's own count of the words that passed through — flagged as
        // estimated, which is exactly what `usage_estimated` is for. Cost
        // stays zero: what an agent charges is not something the gateway can
        // know, and inventing a number would be worse than reporting none.
        prompt_tokens,
        completion_tokens,
        // Set on every A2A row, not only the counted ones: a zero here is
        // the gateway's own count too, so a consumer filtering for
        // provider-billed exactness must not pick these up as exact.
        usage_estimated: true,
        // The content policies that governed the call, and whether one of
        // them blocked it — the same fields the Blocked tab reads for an LLM
        // request, so an A2A block is visible in the same place.
        applied_guardrails: call.applied_guardrails.clone(),
        guardrail_blocked: call.guardrail_blocked,
        guardrail_monitor_hits: call.guardrail_monitor_hits.clone(),
        redacted_entity_counts: call.redactions.clone(),
        upstream_ttft_ms: call
            .stream
            .ttfb
            .map(|d| d.as_millis().min(u32::MAX as u128) as u32)
            .unwrap_or_default(),
        ..Default::default()
    };
    crate::usage_attr::apply_jwt_identity(&mut event, auth.jwt.as_ref());
    // The client-perceived duration of the call. Nothing else records it for
    // `/a2a`: the handler returns the moment a stream's response head is out,
    // so `aisix_proxy_request_duration_seconds` times only how long a stream
    // took to OPEN. Recorded here rather than at the stream's drop guard so
    // the unary, quota-rejected and failed-to-open paths are in the sample
    // too — a streaming-only series would report `/a2a` as having no failures
    // at all.
    state.metrics.record_request_e2e_latency(
        aisix_obs::LatencyLabels {
            endpoint: "/a2a",
            model: A2A_MODEL_LABEL,
            provider: "a2a",
            status: status_code,
            streaming: is_streaming_operation(call.operation),
        },
        latency,
    );
    // The `aisix_a2a_*` family rides on the same chokepoint as the usage
    // event, so a path that accounts for a call cannot skip metering it.
    state.metrics.record_a2a_call(
        aisix_obs::A2aLabels {
            agent,
            operation: call.operation,
            status: status_code,
        },
        aisix_obs::A2aCallOutcome {
            ttfb: call.stream.ttfb,
            stream_events: call.stream.event_count,
            task_state: call.facts.task_state,
        },
    );
    state.usage_sink.try_emit("a2a", event.clone());
    let exporters = crate::usage_attr::live_exporters(state, snap);
    // Opt-in content capture, on the same terms as every other endpoint: only
    // an exporter configured for full content sees the words, and they never
    // travel to the control plane — the usage event above carries counts
    // only. The captured text is the message parts, not the JSON-RPC
    // envelopes, so it reads as a prompt and a completion rather than as
    // protocol scaffolding.
    let captured = content_capture_cap(exporters.iter().map(|e| &*e.value))
        .map(|cap| CapturedContent::new(&call.text.request, &response_text, cap as usize));
    state.otlp_fan_out.fan_out(
        &event,
        captured.as_ref(),
        exporters.generation(),
        exporters.iter().map(|e| &*e.value),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_status_maps_transport_errors_to_502() {
        assert_eq!(
            a2a_error_status(&A2aError::Connect("dns".into())),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            a2a_error_status(&A2aError::Request("500".into())),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn gateway_base_uses_forwarded_proto_then_defaults_https() {
        let uri = axum::http::Uri::from_static("/a2a/x/.well-known/agent-card.json");
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "gw.example.com".parse().unwrap());
        assert_eq!(
            gateway_base(&uri, &headers).as_deref(),
            Some("https://gw.example.com")
        );
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        assert_eq!(
            gateway_base(&uri, &headers).as_deref(),
            Some("http://gw.example.com")
        );
    }

    #[test]
    fn gateway_base_falls_back_to_the_uri_authority() {
        // HTTP/2 sends no `Host` — the authority arrives as the `:authority`
        // pseudo-header, which lands on the URI. This listener negotiates h2
        // whenever `proxy.tls` is set, so a header-only lookup finds nothing
        // there and the card would otherwise be served still advertising the
        // upstream's address.
        let h2_uri = axum::http::Uri::from_static("https://gw.example.com/a2a/x");
        assert_eq!(
            gateway_base(&h2_uri, &HeaderMap::new()).as_deref(),
            Some("https://gw.example.com")
        );
        // An empty Host does not shadow a usable authority either.
        let mut empty_host = HeaderMap::new();
        empty_host.insert(header::HOST, "".parse().unwrap());
        assert_eq!(
            gateway_base(&h2_uri, &empty_host).as_deref(),
            Some("https://gw.example.com")
        );
    }

    #[test]
    fn gateway_base_is_none_without_any_authority() {
        let origin_form = axum::http::Uri::from_static("/a2a/x/.well-known/agent-card.json");
        assert_eq!(gateway_base(&origin_form, &HeaderMap::new()), None);
    }

    /// A local agent that answers `message/stream` with SSE and then keeps the
    /// connection open, so a test can drop the response mid-stream the way a
    /// caller walking away does.
    async fn spawn_open_ended_stream_agent() -> String {
        use axum::response::IntoResponse;
        let app = axum::Router::new().route(
            "/a2a",
            axum::routing::post(|| async {
                let chunks: Vec<Result<String, std::convert::Infallible>> = vec![Ok(
                    "data: {\"jsonrpc\":\"2.0\",\"result\":{\"seq\":1}}\n\n".to_string(),
                )];
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(futures::stream::iter(chunks)),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{addr}/a2a")
    }

    #[tokio::test]
    async fn a_stream_dropped_mid_flight_still_emits_usage() {
        use aisix_obs::{UsageEvent, UsageSink};

        // The drop guard is the whole reason a streamed call is accounted for:
        // a caller walking away mid-task drops the body without the stream ever
        // completing, and for a long-running A2A task that is the ordinary
        // ending, not an edge case. Without the guard the call would simply
        // never appear in usage.
        let agent_url = spawn_open_ended_stream_agent().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UsageEvent>(8);
        let handle = SnapshotHandle::new(snapshot_with(&agent_url, true, serde_json::json!(["*"])));
        let hub = Arc::new(aisix_gateway::Hub::new());
        let state = ProxyState::new(handle, hub, &proxy_cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let router = build_router(state);

        let response = router
            .oneshot(
                HttpRequest::post("/a2a/invoice")
                    .header("host", "gw.example.com")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":"s","method":"message/stream"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);

        // Never read the body — this is the client disconnecting.
        drop(response);
        // Let the dropped stream's guard run.
        tokio::task::yield_now().await;

        let event = rx
            .try_recv()
            .expect("a usage event must be emitted even when the caller never reads the stream");
        assert_eq!(event.inbound_protocol, "a2a");
        assert_eq!(event.a2a_agent_name, "invoice");
        assert_eq!(event.a2a_method, "message/stream");
        // Dropped before the body was ever polled: nothing was delivered, so
        // this is the client hanging up, not a completed call.
        assert_eq!(event.status_code, crate::CLIENT_CLOSED_REQUEST);
    }

    /// An agent that answers `message/send` with a Task in the state the
    /// caller asked for, echoing the task and context ids it was given.
    async fn spawn_task_agent() -> String {
        let app = axum::Router::new().route(
            "/a2a",
            axum::routing::post(|body: axum::Json<serde_json::Value>| async move {
                let message = &body.0["params"]["message"];
                axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": body.0["id"],
                    "result": {
                        "kind": "task",
                        "id": "task-42",
                        "contextId": message["contextId"],
                        "status": {"state": "completed"},
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{addr}/a2a")
    }

    /// Drive one A2A request through the real router and return the usage
    /// event it emitted.
    async fn usage_event_for(agent_url: &str, body: serde_json::Value) -> aisix_obs::UsageEvent {
        use aisix_obs::UsageSink;

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let handle = SnapshotHandle::new(snapshot_with(agent_url, true, serde_json::json!(["*"])));
        let hub = Arc::new(aisix_gateway::Hub::new());
        let state = ProxyState::new(handle, hub, &proxy_cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let response = build_router(state)
            .oneshot(
                HttpRequest::post("/a2a/invoice")
                    .header("host", "gw.example.com")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .expect("router responds");
        // Drain the body so a streamed response reaches its end and its
        // drop guard fires.
        let _ = axum::body::to_bytes(response.into_body(), 1_048_576).await;
        tokio::task::yield_now().await;
        rx.try_recv().expect("a usage event is emitted")
    }

    /// An agent that reports a task's progress and then keeps the stream open
    /// without ever finishing it — a long-running task the caller may leave
    /// before it settles. It emits exactly the two states below and nothing
    /// more, so what a partial reader saw is not a matter of timing.
    async fn spawn_unfinished_stream_agent() -> String {
        use axum::response::IntoResponse;
        let app = axum::Router::new().route(
            "/a2a",
            axum::routing::post(|| async {
                let chunks: Vec<Result<String, std::convert::Infallible>> =
                    ["submitted", "working"]
                        .iter()
                        .map(|state| {
                            let event = serde_json::json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "kind": "status-update",
                                    "taskId": "task-88",
                                    "status": {"state": state},
                                    "final": false,
                                },
                            });
                            Ok(format!("data: {event}\n\n"))
                        })
                        .collect();
                let body = async_stream::stream! {
                    for chunk in chunks {
                        yield chunk;
                    }
                    // Never completes: the task is still running when the
                    // caller gives up on it.
                    std::future::pending::<()>().await;
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(body),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{addr}/a2a")
    }

    #[tokio::test]
    async fn a_stream_abandoned_mid_task_records_the_last_state_seen() {
        use aisix_obs::UsageSink;
        use futures::StreamExt;

        // The claim is that no terminal state is invented for a task the
        // caller stopped watching. The agent reports `submitted` then
        // `working` and never finishes, so `working` is the only honest
        // answer — and the one an operator hunting stuck tasks needs.
        let agent_url = spawn_unfinished_stream_agent().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let handle = SnapshotHandle::new(snapshot_with(&agent_url, true, serde_json::json!(["*"])));
        let hub = Arc::new(aisix_gateway::Hub::new());
        let state = ProxyState::new(handle, hub, &proxy_cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let response = build_router(state)
            .oneshot(
                HttpRequest::post("/a2a/invoice")
                    .header("host", "gw.example.com")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"message/stream"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);

        // Read what the agent has sent so far, then walk away mid-task.
        let mut body = response.into_body().into_data_stream();
        let mut seen = String::new();
        while !seen.contains("working") {
            let chunk = body.next().await.expect("the relayed events arrive");
            seen.push_str(std::str::from_utf8(&chunk.expect("a readable chunk")).unwrap());
        }
        drop(body);
        tokio::task::yield_now().await;

        let event = rx
            .try_recv()
            .expect("an abandoned stream still emits usage");
        assert_eq!(event.a2a_task_id, "task-88");
        assert_eq!(event.a2a_task_state, "working");
        // A caller that hung up mid-task is not a success. Recording 200 here
        // put abandoned streams in the same bucket as completed ones, so an
        // agent nobody waits for looked perfectly healthy.
        assert_eq!(event.status_code, crate::CLIENT_CLOSED_REQUEST);
        // Only what the caller actually received is counted.
        assert_eq!(event.a2a_stream_event_count, 2);
    }

    /// An agent that finishes the task and then leaves the SSE connection
    /// open — the shape that exposes the difference between "the caller got
    /// everything" and "the upstream closed".
    async fn spawn_completed_but_open_stream_agent() -> String {
        use axum::response::IntoResponse;
        let app = axum::Router::new().route(
            "/a2a",
            axum::routing::post(|| async {
                let event = serde_json::json!({
                    "jsonrpc": "2.0",
                    "result": {
                        "kind": "status-update",
                        "taskId": "task-55",
                        "final": true,
                        "status": {"state": "completed"},
                    },
                });
                let body = async_stream::stream! {
                    yield Ok::<_, std::convert::Infallible>(format!("data: {event}\n\n"));
                    std::future::pending::<()>().await;
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(body),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{addr}/a2a")
    }

    #[tokio::test]
    async fn a_completed_task_is_not_recorded_as_abandoned() {
        use aisix_obs::UsageSink;
        use futures::StreamExt;

        // An A2A client stops reading at the terminal event; the agent may
        // hold the connection open long after. Waiting for the upstream to
        // close before calling the response delivered therefore reports every
        // such COMPLETED task as a client hang-up — the exact inversion this
        // PR's 499 is supposed to prevent.
        let agent_url = spawn_completed_but_open_stream_agent().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let handle = SnapshotHandle::new(snapshot_with(&agent_url, true, serde_json::json!(["*"])));
        let hub = Arc::new(aisix_gateway::Hub::new());
        let state = ProxyState::new(handle, hub, &proxy_cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let response = build_router(state)
            .oneshot(
                HttpRequest::post("/a2a/invoice")
                    .header("host", "gw.example.com")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"message/stream"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .expect("router responds");

        // Read exactly the terminal event, then stop — what a conforming
        // client does.
        let mut body = response.into_body().into_data_stream();
        let chunk = body.next().await.expect("the terminal event arrives");
        assert!(String::from_utf8_lossy(&chunk.unwrap()).contains("completed"));
        drop(body);
        tokio::task::yield_now().await;

        let event = rx.try_recv().expect("a usage event is emitted");
        assert_eq!(event.a2a_task_state, "completed");
        assert_eq!(
            event.status_code,
            StatusCode::OK.as_u16(),
            "a fully delivered stream is a success, not an abandoned one"
        );
    }

    #[tokio::test]
    async fn a_failed_call_still_records_the_task_it_asked_about() {
        // The upstream is unreachable, so no response ever names the task.
        // Reading the request up front is what keeps a failed `tasks/get`
        // attributable — the whole point of observing before dispatch.
        let event = usage_event_for(
            "http://127.0.0.1:1/a2a",
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tasks/get",
                "params": {"id": "task-99"}
            }),
        )
        .await;

        assert_eq!(event.status_code, StatusCode::BAD_GATEWAY.as_u16());
        assert_eq!(event.a2a_operation, "tasks/get");
        assert_eq!(event.a2a_task_id, "task-99");
        assert_eq!(event.a2a_task_state, "", "no state may be invented");
    }

    /// An agent whose stream walks a task through several states and then
    /// ends, the way a real long-running task reports progress.
    async fn spawn_progressing_stream_agent() -> String {
        use axum::response::IntoResponse;
        let app = axum::Router::new().route(
            "/a2a",
            axum::routing::post(|| async {
                // A pause before the first event, so a time-to-first-event
                // of 0 cannot pass for one that was never stamped.
                tokio::time::sleep(Duration::from_millis(20)).await;
                let chunks: Vec<Result<String, std::convert::Infallible>> =
                    ["submitted", "working", "completed"]
                        .iter()
                        .map(|state| {
                            let event = serde_json::json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "kind": "status-update",
                                    "taskId": "task-77",
                                    "contextId": "ctx-77",
                                    "status": {"state": state},
                                },
                            });
                            Ok(format!("data: {event}\n\n"))
                        })
                        .collect();
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(futures::stream::iter(chunks)),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{addr}/a2a")
    }

    #[tokio::test]
    async fn a_stream_records_the_state_its_task_ended_in() {
        // A streamed task's outcome is only ever stated in-band, so a call
        // whose task stalled on `input-required` is indistinguishable from one
        // that completed unless the events are read on their way past.
        let event = usage_event_for(
            &spawn_progressing_stream_agent().await,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "SendStreamingMessage",
                "params": {"message": {"role": "user"}}
            }),
        )
        .await;

        assert_eq!(event.a2a_operation, "message/stream");
        assert_eq!(event.a2a_task_id, "task-77");
        assert_eq!(event.a2a_context_id, "ctx-77");
        assert_eq!(event.a2a_task_state, "completed");
        // A stream read to the end is a success, and every event it relayed
        // is counted.
        assert_eq!(event.status_code, StatusCode::OK.as_u16());
        assert_eq!(event.a2a_stream_event_count, 3);
        // The agent paused before speaking, so a stamped time-to-first-event
        // is non-zero — an unstamped one would also read as 0.
        assert!(
            event.upstream_ttft_ms > 0,
            "the first event's arrival must be stamped"
        );
    }

    #[tokio::test]
    async fn a_unary_call_observes_none_of_the_stream_figures() {
        // Nothing was streamed, so an event count or a time-to-first-event
        // would be a number with no referent.
        let event = usage_event_for(
            &spawn_task_agent().await,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "message/send",
                "params": {"message": {"role": "user", "contextId": "ctx-u"}}
            }),
        )
        .await;

        assert_eq!(event.a2a_stream_event_count, 0);
        assert_eq!(event.upstream_ttft_ms, 0);
    }

    #[tokio::test]
    async fn a_call_records_the_task_and_context_it_touched() {
        // Without these the log answers "someone called the invoice agent" and
        // nothing else — not which task it produced, nor how that task ended
        // (#1215).
        let event = usage_event_for(
            &spawn_task_agent().await,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "message/send",
                "params": {"message": {"role": "user", "contextId": "ctx-7"}}
            }),
        )
        .await;

        assert_eq!(event.a2a_method, "message/send");
        assert_eq!(event.a2a_operation, "message/send");
        assert_eq!(event.a2a_task_id, "task-42");
        assert_eq!(event.a2a_context_id, "ctx-7");
        assert_eq!(event.a2a_task_state, "completed");
        // The agent fixture pins no version, so the resource default applies —
        // and it is the version the gateway announced upstream.
        assert_eq!(event.a2a_protocol_version, "1.0");
    }

    #[tokio::test]
    async fn a_call_is_metered_from_the_words_that_passed_through() {
        // An A2A agent reports no usage of its own, so a call that is not
        // counted here is not counted anywhere: every agent's spend looks
        // identical and zero.
        let event = usage_event_for(
            &spawn_task_agent().await,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "message/send",
                "params": {"message": {"role": "user", "parts": [{"kind": "text",
                          "text": "summarise invoice 42"}]}}
            }),
        )
        .await;

        assert!(event.prompt_tokens > 0, "the caller's words are counted");
        assert!(
            event.usage_estimated,
            "the gateway counted these, not the agent — the flag says so"
        );
        // What an agent charges is not something the gateway can know.
        assert_eq!(event.cost_usd, 0.0);
    }

    /// An agent that answers with words rather than a bare task record.
    async fn spawn_talking_agent() -> String {
        let app = axum::Router::new().route(
            "/a2a",
            axum::routing::post(|body: axum::Json<serde_json::Value>| async move {
                axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": body.0["id"],
                    "result": {
                        "kind": "message",
                        "messageId": "m-1",
                        "role": "agent",
                        "parts": [{"kind": "text", "text": "The invoice totals four hundred."}],
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{addr}/a2a")
    }

    #[tokio::test]
    async fn the_agents_own_words_are_counted_too() {
        // Prompt-side only would report every agent as producing nothing.
        let event = usage_event_for(
            &spawn_talking_agent().await,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "message/send",
                "params": {"message": {"role": "user", "parts": [{"text": "how much?"}]}}
            }),
        )
        .await;

        assert!(event.prompt_tokens > 0);
        assert!(event.completion_tokens > 0, "the agent's reply is counted");
        assert!(event.usage_estimated);
    }

    #[tokio::test]
    async fn a_call_with_nothing_to_count_reports_no_tokens() {
        // A task lookup carries no words at all. Reporting a token count for
        // it would be inventing one.
        let event = usage_event_for(
            &spawn_task_agent().await,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"id": "t-1"}
            }),
        )
        .await;

        assert_eq!(event.prompt_tokens, 0);
        assert_eq!(event.completion_tokens, 0);
        // The flag rides every A2A row, zero included: the zero is the
        // gateway's own count too, and a consumer filtering for
        // provider-billed exactness must not mistake it for one.
        assert!(event.usage_estimated);
    }

    #[tokio::test]
    async fn reading_a_task_back_does_not_re_meter_its_answer() {
        // `tasks/get` returns the whole task on every poll. Counting it would
        // let a client polling a long task report one answer as many times as
        // it cared to look, swamping the per-agent figures this exists for.
        let event = usage_event_for(
            &spawn_talking_agent().await,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"id": "t-1"}
            }),
        )
        .await;

        assert_eq!(event.a2a_operation, "tasks/get");
        assert_eq!(
            event.completion_tokens, 0,
            "a read re-states an answer already counted"
        );
    }

    #[tokio::test]
    async fn a_1_0_caller_aggregates_with_its_0_3_twin() {
        // `SendMessage` and `message/send` are one operation. A gateway may
        // front agents on both versions at once, so without canonicalisation
        // every per-operation figure is silently split in two.
        let event = usage_event_for(
            &spawn_task_agent().await,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
                "params": {"message": {"role": "user", "contextId": "ctx-8"}}
            }),
        )
        .await;

        assert_eq!(event.a2a_method, "SendMessage", "the raw value is kept");
        assert_eq!(event.a2a_operation, "message/send");
    }

    #[tokio::test]
    async fn an_unrecognised_method_is_bounded_in_the_event() {
        // The method is caller-chosen, so it must never reach an aggregated
        // position unbounded.
        let event = usage_event_for(
            &spawn_task_agent().await,
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "vendor/doWhatever"}),
        )
        .await;

        assert_eq!(event.a2a_method, "vendor/doWhatever");
        assert_eq!(event.a2a_operation, "unknown");
    }

    #[test]
    fn streaming_methods_are_recognised_in_both_spellings() {
        // The body is forwarded verbatim, so the caller uses whichever spelling
        // its wire version defines. Missing one spelling would silently route a
        // 1.0 caller's stream through the buffering path.
        for method in [
            "message/stream",
            "SendStreamingMessage",
            "tasks/resubscribe",
            "SubscribeToTask",
        ] {
            assert!(
                is_streaming_operation(canonical_operation(method)),
                "{method} must stream"
            );
        }
        for method in [
            "message/send",
            "SendMessage",
            "tasks/get",
            "GetTask",
            "tasks/cancel",
            "agent/getAuthenticatedExtendedCard",
            "",
        ] {
            assert!(
                !is_streaming_operation(canonical_operation(method)),
                "{method} must not stream"
            );
        }
    }

    #[test]
    fn card_rewrite_leaves_no_upstream_url_for_a_caller_to_follow() {
        // Only the top-level `url` used to be rewritten. A 1.0 caller reads its
        // endpoint out of `supportedInterfaces` instead, so it went straight to
        // the upstream — past auth, quota and usage — and read the internal
        // address off the card on the way (#911).
        let mut card: aisix_a2a::AgentCard = serde_json::from_str(
            r#"{
                "name": "Agent",
                "url": "https://internal.upstream/a2a",
                "supportedInterfaces": [
                    {"url": "https://internal.upstream/a2a", "protocolBinding": "JSONRPC"},
                    {"url": "https://internal.upstream/grpc", "protocolBinding": "GRPC"}
                ],
                "additionalInterfaces": [
                    {"url": "https://internal.upstream/rest", "transport": "HTTP+JSON"}
                ],
                "skills": [{"id": "s1"}]
            }"#,
        )
        .unwrap();

        rewrite_card_urls(&mut card, "https://gw.example.com/a2a/billing");

        let served = serde_json::to_string(&card).unwrap();
        assert!(
            !served.contains("internal.upstream"),
            "no upstream address may survive anywhere in the served card:\n{served}"
        );
        assert_eq!(card.url, "https://gw.example.com/a2a/billing");
        for key in ["supportedInterfaces", "additionalInterfaces"] {
            for interface in card.rest[key].as_array().unwrap() {
                assert_eq!(interface["url"], "https://gw.example.com/a2a/billing");
            }
        }
        // Everything the gateway does not own is passed through untouched.
        assert_eq!(card.rest["skills"][0]["id"], "s1");
        assert_eq!(
            card.rest["supportedInterfaces"][1]["protocolBinding"],
            "GRPC"
        );
    }

    // ---- endpoint integration tests: drive the real router via oneshot ----
    use crate::build_router;
    use aisix_core::{A2aAgent, AisixSnapshot, ApiKey, ProxyConfig, ResourceEntry, SnapshotHandle};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use std::sync::Arc;
    use tower::ServiceExt;

    const TOKEN: &str = "sk-a2a-endpoint-test";

    fn proxy_cfg() -> ProxyConfig {
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

    /// Snapshot with one API key (granting `allowed_agents`, or none when
    /// `allowed_agents` is `null`) and one `invoice` agent at `agent_url`.
    fn snapshot_with(
        agent_url: &str,
        enabled: bool,
        allowed_agents: serde_json::Value,
    ) -> AisixSnapshot {
        let mut key = serde_json::json!({
            "key_hash": ApiKey::hash_bearer(TOKEN),
            "allowed_models": ["*"],
        });
        if !allowed_agents.is_null() {
            key["allowed_agents"] = allowed_agents;
        }
        let apikey: ApiKey = serde_json::from_value(key).expect("valid apikey");
        let agent: A2aAgent = serde_json::from_value(serde_json::json!({
            "display_name": "invoice",
            "url": agent_url,
            "enabled": enabled,
        }))
        .expect("valid a2a agent");

        let snap = AisixSnapshot::new();
        snap.apikeys.insert(ResourceEntry::new("ak-1", apikey, 1));
        snap.a2a_agents.insert(ResourceEntry::new("ag-1", agent, 1));
        snap
    }

    /// A2A had every gate except the one about what is being said, so a rule
    /// that blocked a phrase on `/v1/chat/completions` was bypassable by
    /// asking an agent the same thing. An `env`-scoped keyword guardrail must
    /// refuse the call — and never contact the agent.
    #[tokio::test]
    async fn an_env_guardrail_blocks_an_a2a_message_before_the_agent_is_called() {
        let agent = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"kind": "task"}}),
            ))
            // The hard contract: a content refusal never reaches the agent.
            .expect(0)
            .mount(&agent)
            .await;

        let snap = snapshot_with(&agent.uri(), true, serde_json::json!(["*"]));
        let guardrail: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"kw","enabled":true,"hook_point":"input","fail_open":false,"kind":"keyword","patterns":[{"kind":"literal","value":"BLOCKME"}]}"#,
        )
        .unwrap();
        snap.guardrails
            .insert(ResourceEntry::new("g-1", guardrail, 1));

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {"message": {"role": "user", "parts": [{"kind": "text", "text": "please BLOCKME"}]}}
        });
        let req = HttpRequest::post("/a2a/invoice")
            .header("host", "a2a.aisix.example.com")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router_with(snap).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["error"]["code"], -32000,
            "the refusal must be a JSON-RPC error the caller's client understands: {v}",
        );
    }

    /// The mirror case: a benign message reaches the agent with the same
    /// guardrail attached, so the block above is the policy firing rather
    /// than the endpoint refusing everything once a policy exists.
    #[tokio::test]
    async fn a_benign_a2a_message_passes_the_guardrail_through_to_the_agent() {
        let agent = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"kind": "task"}}),
            ))
            .expect(1)
            .mount(&agent)
            .await;

        let snap = snapshot_with(&agent.uri(), true, serde_json::json!(["*"]));
        let guardrail: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"kw","enabled":true,"hook_point":"input","fail_open":false,"kind":"keyword","patterns":[{"kind":"literal","value":"BLOCKME"}]}"#,
        )
        .unwrap();
        snap.guardrails
            .insert(ResourceEntry::new("g-1", guardrail, 1));

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {"message": {"role": "user", "parts": [{"kind": "text", "text": "an ordinary question"}]}}
        });
        let req = HttpRequest::post("/a2a/invoice")
            .header("host", "a2a.aisix.example.com")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router_with(snap).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// An agent carrying `allowed_cidrs` refuses a caller outside them.
    ///
    /// Model rows have had this gate since #557; MCP servers and A2A agents
    /// did not, so an operator who restricted a model by source network found
    /// the same restriction unavailable for the agent sitting beside it.
    #[tokio::test]
    async fn an_agent_outside_the_ip_allowlist_refuses_the_caller() {
        let agent = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"kind": "task"}}),
            ))
            // The hard contract: the agent is never contacted.
            .expect(0)
            .mount(&agent)
            .await;

        let snap = snapshot_with(&agent.uri(), true, serde_json::json!(["*"]));
        // Rewrite the agent row with an allowlist that excludes any caller a
        // oneshot test can present (it has no peer address at all, which the
        // gate fails closed on).
        let restricted: A2aAgent = serde_json::from_value(serde_json::json!({
            "display_name": "invoice",
            "url": agent.uri(),
            "enabled": true,
            "allowed_cidrs": ["10.99.0.0/24"],
        }))
        .expect("valid a2a agent");
        snap.a2a_agents
            .insert(ResourceEntry::new("ag-1", restricted, 2));

        let response = router_with(snap)
            .oneshot(a2a_post("invoice", true))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a caller outside the allowlist must be refused before the agent is called",
        );
    }

    /// The mirror case: an EMPTY allowlist is not a deny-all. The field is
    /// opt-in, so a row that carries it without entries must serve exactly as
    /// a row without the field does — otherwise adding the field to the
    /// resource model would silently break every agent on upgrade.
    ///
    /// A matching allowlist admitting the caller needs a real socket to have a
    /// peer address at all; that half is pinned by the e2e.
    #[tokio::test]
    async fn an_empty_allowlist_is_not_a_deny_all() {
        let agent = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"kind": "task"}}),
            ))
            .expect(1)
            .mount(&agent)
            .await;

        let snap = snapshot_with(&agent.uri(), true, serde_json::json!(["*"]));
        let unrestricted: A2aAgent = serde_json::from_value(serde_json::json!({
            "display_name": "invoice",
            "url": agent.uri(),
            "enabled": true,
            "allowed_cidrs": [],
        }))
        .expect("valid a2a agent");
        snap.a2a_agents
            .insert(ResourceEntry::new("ag-1", unrestricted, 2));

        let response = router_with(snap)
            .oneshot(a2a_post("invoice", true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    fn router_with(snap: AisixSnapshot) -> axum::Router {
        let handle = SnapshotHandle::new(snap);
        let hub = Arc::new(aisix_gateway::Hub::new());
        build_router(ProxyState::new(handle, hub, &proxy_cfg()).without_cache())
    }

    fn a2a_post(agent: &str, auth: bool) -> HttpRequest<Body> {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "message/send"});
        let mut b = HttpRequest::post(format!("/a2a/{agent}"))
            .header("host", "a2a.aisix.example.com")
            .header("content-type", "application/json");
        if auth {
            b = b.header("authorization", format!("Bearer {TOKEN}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn chunked_oversized_body_returns_enveloped_413() {
        // Same contract as /mcp: a chunked body over the cap surfaces as
        // the enveloped 413 from the handler's capped read, not the old
        // bare 400.
        let app = router_with(snapshot_with(
            "http://127.0.0.1:1/a2a",
            true,
            serde_json::json!(["invoice"]),
        ));
        let chunk = vec![b'a'; 200 * 1024];
        let stream =
            futures::stream::iter((0..10).map(move |_| Ok::<_, std::io::Error>(chunk.clone())));
        let req = HttpRequest::post("/a2a/invoice")
            .header("host", "a2a.aisix.example.com")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from_stream(stream))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let v: serde_json::Value =
            serde_json::from_slice(&body).expect("413 must carry the JSON envelope");
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn endpoint_denies_key_without_allowed_agents_403() {
        // Unreachable upstream on purpose: the ACL must reject BEFORE any
        // upstream call is made.
        let app = router_with(snapshot_with(
            "http://127.0.0.1:1/a2a",
            true,
            serde_json::Value::Null,
        ));
        let resp = app.oneshot(a2a_post("invoice", true)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn endpoint_disabled_agent_is_404() {
        let app = router_with(snapshot_with(
            "http://127.0.0.1:1/a2a",
            false,
            serde_json::json!(["*"]),
        ));
        let resp = app.oneshot(a2a_post("invoice", true)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn endpoint_unknown_agent_is_404() {
        let app = router_with(snapshot_with(
            "http://127.0.0.1:1/a2a",
            true,
            serde_json::json!(["*"]),
        ));
        let resp = app.oneshot(a2a_post("does-not-exist", true)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn endpoint_missing_key_is_401() {
        let app = router_with(snapshot_with(
            "http://127.0.0.1:1/a2a",
            true,
            serde_json::json!(["*"]),
        ));
        let resp = app.oneshot(a2a_post("invoice", false)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// A stub upstream that serves an agent card advertising an internal URL.
    async fn spawn_card_stub() -> std::net::SocketAddr {
        let app = axum::Router::new().route(
            "/.well-known/agent-card.json",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "name": "Invoice Agent",
                    "url": "https://upstream.internal/a2a",
                    "version": "2.1.0",
                    "skills": [{"id": "extract"}]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn endpoint_rewrites_agent_card_url_to_gateway() {
        let addr = spawn_card_stub().await;
        let app = router_with(snapshot_with(
            &format!("http://{addr}/a2a"),
            true,
            serde_json::json!(["*"]),
        ));
        let req = HttpRequest::get("/a2a/invoice/.well-known/agent-card.json")
            .header("host", "a2a.aisix.example.com")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_048_576)
            .await
            .unwrap();
        let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // The advertised service URL is rewritten to the gateway; the caller's
        // Host is reflected and every other card field is preserved.
        assert_eq!(card["url"], "https://a2a.aisix.example.com/a2a/invoice");
        assert_eq!(card["name"], "Invoice Agent");
        assert_eq!(card["version"], "2.1.0");
        assert_eq!(card["skills"][0]["id"], "extract");
    }

    /// A `mask` rule must rewrite the message before the agent sees it.
    ///
    /// This module's header is the argument: content policy applies here "so
    /// a rule that blocks a phrase on an LLM endpoint is not bypassable by
    /// asking an agent the same thing". A MASK rule was exactly that
    /// bypassable — the chain resolved, the text was scanned, and nothing
    /// rewrote it, so an env-scoped `pii` mask that redacts a prompt on
    /// /v1/chat/completions let the same content reach an agent untouched,
    /// with no block, no rewrite and no record (`would_mask` is monitor-mode
    /// only, and this is enforcement).
    #[tokio::test]
    async fn a_mask_rule_rewrites_the_message_before_the_agent_sees_it() {
        let agent = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"kind": "task"}}),
            ))
            .expect(1)
            .mount(&agent)
            .await;

        let snap = snapshot_with(&agent.uri(), true, serde_json::json!(["*"]));
        let guardrail: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"mask-email","enabled":true,"hook_point":"input","kind":"pii","detectors":[{"type":"email","action":"mask"}]}"#,
        )
        .unwrap();
        snap.guardrails
            .insert(ResourceEntry::new("g-mask", guardrail, 1));

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {"message": {"role": "user", "parts": [
                {"kind": "text", "text": "invoice alice@example.com please"},
                {"kind": "text", "text": "no address in this one"}
            ]}}
        });
        let req = HttpRequest::post("/a2a/invoice")
            .header("host", "a2a.aisix.example.com")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router_with(snap).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "a mask must not block");

        let received = agent.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let sent = String::from_utf8_lossy(&received[0].body).into_owned();
        assert!(
            !sent.contains("alice@example.com"),
            "the message reached the agent unmasked:\n{sent}"
        );
        // The rest of the envelope is protocol, not caller content, and must
        // survive verbatim — a blanket string rewrite would corrupt the call
        // it is meant to protect.
        assert!(sent.contains("message/send"), "method must survive: {sent}");
        assert!(
            sent.contains("no address in this one"),
            "a part with nothing to mask must be untouched: {sent}"
        );
    }

    /// A data part cannot be rewritten safely, so a mask rule that matches one
    /// refuses the call — forwarding it unmasked is the one outcome the rule
    /// exists to prevent.
    #[tokio::test]
    async fn a_mask_rule_matching_a_data_part_refuses_the_call() {
        let agent = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"kind": "task"}}),
            ))
            .expect(0)
            .mount(&agent)
            .await;

        let snap = snapshot_with(&agent.uri(), true, serde_json::json!(["*"]));
        let guardrail: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"mask-email","enabled":true,"hook_point":"input","kind":"pii","detectors":[{"type":"email","action":"mask"}]}"#,
        )
        .unwrap();
        snap.guardrails
            .insert(ResourceEntry::new("g-mask", guardrail, 1));

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "message/send",
            "params": {"message": {"role": "user", "parts": [
                {"kind": "text", "text": "file this form"},
                {"kind": "data", "data": {"contact": "alice@example.com"}}
            ]}}
        });
        let req = HttpRequest::post("/a2a/invoice")
            .header("host", "a2a.aisix.example.com")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router_with(snap).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["error"]["code"], -32000, "{v}");
        assert!(!v.to_string().contains("alice@example.com"), "{v}");
    }

    /// `tasks/get` hands back the whole task, `history` included, so it is
    /// checked like any other answer — and a refused read is still not
    /// metered, because metering skips reads on its own terms.
    #[tokio::test]
    async fn a_read_is_checked_but_still_not_metered() {
        use aisix_obs::UsageSink;

        let agent = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {"id": "t-1", "status": {"state": "TASK_STATE_COMPLETED"},
                        "history": [{"role": "ROLE_AGENT", "parts": [{"text": "please BLOCKME"}]}]}
                })),
            )
            .mount(&agent)
            .await;

        let snap = snapshot_with(&agent.uri(), true, serde_json::json!(["*"]));
        let guardrail: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"kw","enabled":true,"hook_point":"output","fail_open":false,"kind":"keyword","patterns":[{"kind":"literal","value":"BLOCKME"}]}"#,
        )
        .unwrap();
        snap.guardrails
            .insert(ResourceEntry::new("g-1", guardrail, 1));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let state = ProxyState::new(
            SnapshotHandle::new(snap),
            Arc::new(aisix_gateway::Hub::new()),
            &proxy_cfg(),
        )
        .without_cache()
        .with_usage_sink(UsageSink::new(tx));

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "GetTask", "params": {"id": "t-1"}
        });
        let req = HttpRequest::post("/a2a/invoice")
            .header("host", "a2a.aisix.example.com")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = build_router(state).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("BLOCKME"));

        let event = rx.try_recv().expect("a usage event is emitted");
        assert_eq!(event.a2a_operation, "tasks/get");
        assert!(event.guardrail_blocked);
        assert_eq!(event.prompt_tokens, 0);
        assert_eq!(event.completion_tokens, 0, "a read is not metered");
    }
}
