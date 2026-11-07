# aisix-proxy

## Response-body streams and spawned tasks must re-attach the request span

`request_id::ensure_request_id` opens the `request{request_id=…}` span that puts a
`request_id` on every log line a request emits — that field is what joins a deep
diagnostic (e.g. the Aliyun guardrail's `aliyun_request_id`) back to the
`x-aisix-request-id` the caller was handed.

Two places fall outside it, and neither errors when missed — the logs are just
silently uncorrelated, which reads exactly like working code:

- **Streamed response bodies.** Hyper polls the generator after the middleware has
  returned. Wrap it in `request_id::in_request_span(…)` **from the handler's
  stack** (it captures `Span::current()`, so calling it elsewhere attaches a no-op
  span). Every `async_stream::stream!` returned as a body needs this.
- **Detached tasks.** Anything reached via `tokio::spawn` or axum's
  `WebSocketUpgrade::on_upgrade` inherits nothing; attach the span to the future
  with `.instrument()` (see `realtime::realtime`).

Do not hold a span guard across an await to work around this — it leaks the span
onto whatever the executor runs next on that thread.

A `text/event-stream` body needs a second wrapper for the same reason — nothing
errors when it is missed. Pass it through `sse_keepalive::with_heartbeat(…,
sse_keepalive::interval())` (or, on an axum `Sse`, `keep_alive` with that
interval) so a model that is slow to its first token doesn't look like an
abandoned connection to a proxy in front. Only for SSE: the same wrapper on an
opaque binary passthrough (audio, images) corrupts it.

## Turning a buffered relay into a streamed one moves four things, not one

Swapping `resp.bytes()` for `resp.bytes_stream()` is the visible part of the
change and the least of it. The handler now returns while the response is still
being delivered, so three request-scoped things that used to be finished by then
are not — and a fourth rule decides whether the path may stream at all. None of
the four errors when missed:

- **The reqwest request-level timeout has to go.** `RequestBuilder::timeout`
  bounds the body read too, so it cuts a long stream off mid-response. Bound the
  connect phase with `stream_timeout::send_with_deadline` and each chunk with
  `stream_timeout::with_read_timeout_bytes`, both from the model's `stream`
  budget (`routing::effective_timeouts`).
- **The rate-limit reservation has to become a `into_stream_hold()`.** Dropping it
  at handler return releases the concurrency slot while the stream is still
  running, which is how a key capped at N serves more than N at once. The
  terminal token cost then rides `Limiter::add_tokens_post_stream` instead of
  `commit_tokens`.
- **The UsageEvent moves into the stream's Drop guard.** Counts that arrive on a
  terminal SSE event are unknown at handler return, so the guard owns the emit —
  fired at end-of-stream AND on client disconnect, since SDKs routinely close on
  the terminal frame. Whatever flag says "the guard owns it" must gate the
  handler's own emit, or the request is counted twice.
- **A block- or mask-capable output guardrail keeps the buffered path.** It has to
  see the whole response before any of it reaches the caller, or the streaming
  request is a way around the check the same request gets without it. That is
  `runs_on_output(chain) && chain.stream_output_policy().holds_back()`. A
  monitor-only chain resolves to `EndOfStreamCheck` and must NOT hold the stream
  back — it takes the live path and scans once at
  end-of-stream via `guardrail_stream::EosOutputScan`. The text it keeps for that
  scan stops at `guardrail_stream::live_scan_cap`, 128 times the chain's
  `max_buffer_bytes`: the scan reads what was kept, and the row records
  `output_buffer_exceeded` for the rest (`LiveScanBudget`,
  `record_live_scan_outgrown`).

Retry semantics change too, and that is intended: the retryable unit ends at the
status check, because once the first frame is on the wire there is no failing
over left to do.

An e2e that asserts the response CONTENT cannot see any of this — a buffered relay
and a streamed one return identical bytes. Assert the delivery shape instead: a
mock upstream that trickles its response over ~1s, and a client read loop that
counts chunks and measures first-byte against last-byte
(`audio-stream-relay-e2e.test.ts`).

## A guardrail-raised refusal is gated on the direction, not on `is_empty()`

`GuardrailIndex::resolve` matches on SCOPE only — env / model / mcp_server /
api_key / team. It does not filter by `hook_point`, so a chain resolved for a
request is non-empty whenever any attachment is in scope, including one attached
on the output hook alone. Each guardrail then no-ops on the hook it is not
configured for, which makes `!chain.is_empty()` a safe gate for *running* the
checks and a wrong gate for *refusing*: a refusal the proxy raises itself —
`unscannable_body`, and anything else the code decides rather than a verdict —
is only justified when something would have read that side of the exchange.

Gate the refusal on `Guardrail::refuses_unevaluable_input(&chain)` for a
request-side one and `refuses_unevaluable_output(&chain)` for a response-side
one. Those fold TWO conditions per member — the member reads that side, and its
failure policy is fail-closed — and a chain is `any` over its members. Both
halves must hold on the SAME member, which is why there is a combined predicate
rather than two you could `&&` together: a chain of [output-only fail-closed,
input-only fail-open] reads the request and contains a fail-closed row, yet
nothing in it justifies refusing a request. A monitored row answers `false` to
both whatever its policy, because monitor mode never refuses; what it could not
read is a bypass like any other.

The failure policy is per hook: the row's `fail_open` governs the input hook,
and each remote kind's own `<Kind>Config::output_fail_open` governs the output
hook. `keyword` and `pii` have no `output_fail_open` — they never call out — so
their row-level `fail_open` governs both of their hooks.

`/mcp` is the sharp case for the direction half: it resolves ONE chain and uses
it in both directions, so an input-only row reaches the output arm and vice
versa. Note also that `hook_point` defaults to `both` and `fail_open` to
`false`, so this only bites a deployment that set one of them explicitly.

Not gated by these predicates: the refusals that need a HELD-BACK stream.
`output_buffer_exceeded` has its own knob — each holding member's
`on_buffer_exceeded`, folded by `StreamOutputPolicy::stricter` (fail-open only
when every holding member says so) — and every route honours it: past the cap,
what was held is released unscanned, the rest streams without an output scan,
and the request records `guardrail_bypassed_reason: output_buffer_exceeded`.
`mask_writeback_failed` and `unscannable_body` on a buffered SSE body are not
governed by `fail_open`, deliberately: honouring it there would mean releasing
already-buffered bytes that were never scanned — a different decision, and
one nobody has taken.

## The bypass reason rides the audit log, and every emitter must set it

`usage_events.guardrail_bypassed_reason` is the record that a guardrail on
this request did not evaluate and its failure policy let the request past
it. Its failure mode is silent in both directions: the caller gets a normal
200, and an emitter that leaves the field at its `String::default()`
reports "nothing was bypassed" for a request that was — indistinguishable
from a screened one.

**It is not mutually exclusive with `guardrail_blocked`, and must not be
suppressed when that is set.** A chain can fail open on one member and be
refused by another, and an input hook can fail open on a prompt the
provider already answered before the output hook refused it. Both are
requests where something really did go unscreened, and the second is the
more compliance-relevant of the two — `chat.rs` has always carried the
input bypass onto the billed-then-blocked event through `UpstreamCharge`.
"Reached a provider unscreened" is the two fields read together, never this
one alone.

So it is not threaded per handler. The chain folds record the first `Bypass`
they see onto the request's `GuardrailAuditLog`, which every handler already
clones for `guardrail_enforced_hits`, and every emitter reads it back with
`usage_attr::bypass_reason(audit)`. Not terminal-only, unlike the enforced
hits: every attempt of a request that failed open went upstream unscreened.
`chat.rs` still threads its own copy because its per-attempt and ensemble
sub-call emitters are handed the value captured earlier in the request,
which a request-scoped snapshot cannot express; the two agree by
construction.

Two rules follow, and both are enforced mechanically rather than by review:

- A new `UsageEvent` literal in this crate sets the field — and
  `applied_guardrails`, which rides the same log
  (`usage_attr::applied_guardrails(audit)`) — or carries a
  `NO-GUARDRAIL-CHAIN: <why>` comment saying no chain was ever resolved
  (`usage_attr`'s `every_usage_event_this_crate_builds_answers_the_guardrail_questions`
  parses the crate's own source for both).
- A pass-through the PROXY performs on a body it could not scan is recorded
  with `GuardrailChain::record_unevaluable_{input,output}_bypass`, never
  `record_bypass` directly. That pass has two causes and only one is a
  bypass: a chain whose readers of that side are all fail-open let an
  unscreened request through, while a chain where NOTHING reads that side
  never offered to screen it. Tagging the second would fire the field on
  requests that were never going to be screened, which breaks the negative
  answer just as thoroughly as dropping the first.

Deliberately NOT recorded, so the next person does not read the gated
unscannable sites as the full set: the places that scan a mangled copy
unconditionally, with no failure policy involved. `jobs::scan_output_blob`
scans `String::from_utf8_lossy` of a batch or fine-tuning response and, where
the verdict lets it through, relays the original bytes whatever the row's
failure policy says; `passthrough_route` scans the lossy body. Neither
consults `refuses_unevaluable_*`, so a fail-CLOSED row does not refuse there
either — not something telemetry can paper over, and making them refuse is a
behaviour change rather than an observability one. Tagging them instead
would fire the field on every job response and every passthrough body, which
destroys the negative answer just as thoroughly.
Note that `record_unevaluable_*` is the wrong helper at such a site: its
predicate assumes the fail-closed case was already refused, so at a site that
never refuses it would silently drop exactly the case worth reporting.

The multipart `prompt` on `audio.rs` and `images_edits.rs` is not in that set:
both call `dispatch::require_utf8_prompt_fields` after model resolution and
before the chain runs, so an undecodable `prompt` part is answered 400 and the
`filter_map` in their scan builders cannot be reached by one. Audio's
transcript OUTPUT scan is a separate site and is gated rather than dropped:
`transcription_output_text` reports whether the plain-text fallback (`text` /
`srt` / `vtt`) had to decode lossily, and its caller runs the same gate as
`/mcp`'s tool-result arm — refuse under `refuses_unevaluable_output`, record
the bypass otherwise. Those two and `/a2a`, on an answer and on each streamed
event, are the typed routes' `refuses_unevaluable_output` call sites: `/v1/messages` and `/v1/messages/count_tokens` gate the INPUT side
with the mirror predicate, and their response-side `unscannable_body` arms are
the held-back-stream ones listed above as deliberately ungated. Audio differs
from `/mcp` in still scanning the lossy text when it does not refuse, because a
transcript is prose the caller reads: only the bytes `from_utf8_lossy` replaced
go unread. The flag it gates on reports a failed DECODE, not scan coverage — a
JSON body with no transcript field scans nothing and reports no bypass, the same
as the empty transcript a silent recording returns.

Values come from the guardrail kind's own `bypass_tag()` — the same bounded
vocabulary `GuardrailVerdict::block_unavailable` carries, so one outage reads
the same whichever way the row is configured — plus `unscannable_body` for
the proxy-raised pass. The audit log clamps whatever it is given through
`bounded_failure_tag`, because the value lands on an unsanitized metric
label and on a wire field the control plane stores as `varchar(64)`.

## Every terminal path emits the access log — including the ones that give up early

The access log and `request_metrics::record` are emitted **by the handler**, at
the end of dispatch, because that is the only place that knows the provider, model
and token counts. A path that returns before reaching that tail therefore logs
nothing, and nothing errors: the caller gets a correct status while the gateway
keeps no record of the request, which is indistinguishable from the request never
arriving.

Every line goes through `attribution::emit_access_log` (or
`emit_access_log_spanning_body`), never `AccessLog::emit` directly: the line
carries the request and response body sizes, which the telemetry middleware
counts at its outermost layer and only knows once the response body is done
with, so the cell holds the line and the middleware's guard writes it then. The
cell also fills a line that carries no token counts from the request's terminal
usage event (`attribution::note_terminal_event`), so a handler need not thread
the counts into its line. A line emitted directly is written without either
size or those counts, and nothing errors.

One exception, and it is the whole of it: **a STREAMED response's line — and
its request metrics — are not the handler's to write.** Its tail runs when the
head goes out, which is not when the request ends — so it parks the line on the
attribution cell (`attribution::PendingAccessLog`), together with its
`request_metrics::PendingRequestMetrics` instead of calling
`request_metrics::record` (`defer_access_log` takes both), and both are
completed in
`usage_attr::emit_usage` with the request's TERMINAL usage event, whichever of
the stream's endings produced it. That is what makes the line and the row agree
on `status_code`, `error_class` and the error message by construction, and puts the
request families on the same status; writing either at the tail instead would
report every abandoned stream as a `200` beside its own `499` row. Two consequences for a new streaming family: park
the line whenever the response IS a stream (a "telemetry already emitted" flag
is NOT the same predicate — chat's buffered ensemble sets one), and make sure
the stream really does emit a terminal usage event, because that emit is now
the only thing that writes the line.

Two shapes give up early, and both must answer through
`reject::reject_before_dispatch` (it renders the envelope *and* emits the
telemetry, so the two can't drift apart):

- **Middleware short-circuits** — anything that returns instead of calling
  `next.run(request)` (see `enforce_request_body_limit`). These run ahead of
  authentication, so they pass `api_key_id: None`.
- **Extractor rejections a handler unwraps at its top** — the
  `Result<Json<T>, JsonRejection>` / `Result<Bytes, BytesRejection>` parameters.
  Auth already ran here, so pass the key id.

Authentication denials are the one early exit that writes its own line: the
`AuthenticatedKey` extractor emits it, and records the request metrics, on
every `Err` it returns, so a handler must not emit again for an auth failure (`/v1/messages` re-renders the error,
it does not log it). A surface that authenticates without the extractor
(`/v1/realtime`, passthrough routes) writes its line in its own error arm.

A route that answers without dispatching at all — `/v1/models`, the A2A
agent card, the OAuth discovery documents, the unrouted-path 404, the router's
405 (`method_not_allowed_fallback`, which must stay the LAST call before the
layers or later routes miss it) — writes its line through
`reject::emit_unrouted_access_log`. `/livez` and `/readyz` deliberately write
none: they are platform probes, not client traffic.

A handler that instead wraps its whole dispatch and logs the wrapper's status
(`/mcp`, `/a2a`, `/passthrough`, `/v1/videos`, `/v1/files`) is already covered —
don't add a second emit to those, or the request logs twice.

Emit the request metrics through `request_metrics::record` (or, for a stream,
the parked `PendingRequestMetrics`) and nothing else. It
writes the legacy `aisix_requests_total` **and** the detailed `aisix_proxy_*` /
`aisix_llm_*` families from one call, so calling `Metrics::record_request`
directly silently produces a request that exists in one family and not the
others.

There is a third shape, and it reaches no tail at all: a caller that hangs up
before the response head is written. axum **drops** the handler future, so the
access log, the metrics and the usage events are all continuation code that
never runs — and nothing errors, exactly as above. `ClientCancelGuard` writes
the line, the cancel counter and the usage events from `Drop` instead
(`cancel.rs`). What that costs everyone else is a
standing rule: **anything a cancelled request must report has to be published to
the request's attribution cell (`attribution.rs`) at a chokepoint the handlers
already pass through — never held only in a handler local.** A value kept in a
local is correct on every path except the one nobody tests, and the symptom is a
missing row rather than an error.

That guard's lifetime does **not** end when the handler returns: it rides the
response body (`TelemetryBody`), because a streaming family's own terminal
emitter is built INSIDE its `async_stream!` generator and so does not exist
until the body's first poll — a body dropped before that emits nothing
anywhere. Two rules follow for anything that touches the middleware's response
handling. Keep the body's first poll observable: wrapping it in a combinator
that hides `poll_frame` (`map_frame` sees only delivered frames, not the poll
that returned `Pending`) hands every delivered stream a second, contradicting
`499` row. And keep the body's **drop** inside the request's attribution scope
(`attribution::sync_scope`): two families build their emitter outside the
generator, so it fires on an unpolled drop, and that scope is the only reason
the guard can see that it already spoke.

## A passthrough route meters an identified protocol with the typed endpoint's parser

`passthrough_route.rs` relays bytes verbatim, but it *identifies* the
protocol of the exchange and extracts from it — there is no bridge behind it
to fill anything in. Its usage is read by the typed endpoint's own parser for
that protocol (`completions::completion_usage_stats`,
`responses::response_usage_stats`, `messages::anthropic_usage_stats`,
`embeddings::embedding_usage_stats`, `images::image_usage_stats`, the OpenAI
wire reader, …), never by a reader of its own — and never by guessing a
protocol from a usage object's field names: a token field's
meaning depends on the protocol that produced it (`cache_read_input_tokens`
is additive beside Anthropic's `input_tokens` and a subset of an agent
backend's whole-input `prompt_tokens`), so a reader keyed on field names
mis-bills one of them. A new token dimension or a fix to a typed parser
therefore reaches passthrough by itself; a new protocol needs one dispatch
arm, and a shape no typed endpoint serves needs an adapter that states which
counters are subsets and which are additive. Observations that are not
parsed from the body still need wiring here by hand: a new attribution field
needs setting on `RouteTelemetry`.

Guardrail text and capture are the exception, and deliberately WIDER than
the typed scans: the relay forwards every field, so they read every decoded
string value except known-opaque slots. Do not swap them for a typed parse
of the request — that narrows what the guardrail sees.

Two rules bound the extraction:

- An **opaque** (`Raw`) body is never mined for meaning. Opaque responses
  are not probed for usage at all, buffered or streamed, except a flat token
  report on a frame the server itself labelled one (`event: token_usage`) —
  a caller-shaped body must not be able to mint tokens.
- Usage accumulates **field-wise max** across frames, never last-wins: one
  stream reports it in pieces (Anthropic's `message_start` carries the input
  and cache counters, its `message_delta` only the output ones), so an
  assignment truncates whatever arrived earlier.

## A new proxy route has to be declared in three places

Adding a `.route(…)` in `build_router` is not enough, and nothing fails loudly
if you stop there:

1. `normalize_endpoint_label` — an unlisted path collapses to `"other"`, so the
   route is invisible per-endpoint in every request series.
2. `request_metrics::LLM_ENDPOINTS` — decides whether the route counts as model
   inference. Unlisted means proxy-only, which is the safe default but a silent
   one.
3. The `ROUTES` table in `request_metrics`' tests — the only thing that makes
   (1) and (2) fail loudly. It is a hand-maintained list of every route; a route
   missing from it is a route the tests cannot check.
4. The route table in `tests/e2e/src/cases/metric-endpoint-parity-e2e.test.ts`
   — see the next section.

## Every client-facing route emits every per-request metric family

A per-request family — the request families, `aisix_request_e2e_latency_seconds`,
the TTFT pair, the token families, `aisix_deployment_*`,
`aisix_ratelimit_remaining_*` — is emitted on every client-facing proxy route,
unless the quantity it measures does not exist there (no stream, no time to
first token; no model call, no deployment). A route that skips a family for
any other reason is a bug, however it is labelled.

Record through the shared emit, never per handler:
`request_metrics::record` / `record_in_tier` / `PendingRequestMetrics` for the
request families and the `downstream` end-to-end side; `usage_attr::emit_usage`
for the `upstream` side; `request_metrics::record_usage` for tokens;
`RoutingTelemetry::record` (or `record_deployment` outside a routing loop) for
deployments; `quota::enforce` for the remaining-quota gauges.

`metric-endpoint-parity-e2e.test.ts` holds one row per route with the families
it must emit; a new route, or a family a route cannot have, is declared there.

**Every request-metric emit states what ended the request.** `request_metrics::record` and its siblings take an `ErrorSource` so a new path cannot leave it out; give it the failure, never the status code: `attempt::error_source(&err)` for a typed error, `error_source_of_class` for a stream's terminal class, `answered_error_source` for a response a handler answers with itself, `relayed_error_source` for an upstream's response relayed as sent, and `attribution::rendered_error_source` where only a rendered response survives (note the source first with `note_error_source` if no usage event names it). A new class an upstream's failure reports goes into `UPSTREAM_ERROR_CLASSES`; anything unlisted reads as the gateway's. A local refusal rewritten as `BridgeError::UpstreamStatus` reads as the upstream's, so keep the `ProxyError` (ensemble members and judges are the known exception).

## A per-model gate must say whether it binds the requested entry or each target

`resolve_attempt_models` expands a routing model into targets, so `model_entry` /
`virtual_entry` is the **group**, which carries none of a member's config. A gate
written against it silently never runs for group traffic, and nothing errors —
requests keep succeeding on a target that should have been excluded.

**The default is that a per-model gate binds each target.** Anything an operator
configures ON a model — rate limits, `allowed_cidrs`, cooldown, health, timeouts —
is a statement about that model, and reaching it through a group must not strip it.
The only deliberately entry-scoped gate is the group's own copy of any of the
above. Anything else that only checks `model_entry` / `virtual_entry` is a bug.

**Guardrail attachment is the one deliberate exception, and it is settled
design.** Its scope follows the entry the caller addresses: a guardrail attached
to a model runs only for requests addressed to that model. When the model is
reached as a member of a routing, semantic, or ensemble group, its guardrails do
not run — the operator attaches the guardrail to the group instead. The chain is
resolved once from the addressed entry before a target is picked, which is what
makes an input guardrail's verdict independent of which member a failover lands
on. Do not "fix" a member-scoped guardrail into running for group traffic, do not
re-audit it as a finding, and do not cite it as precedent for scoping any OTHER
per-model gate to the entry — it is the exception because the decision is
about which entry the caller addressed, not about a member's own configuration.

The OTHER member gates on **ensemble** sub-calls (member `allowed_cidrs`,
cooldown/health consumption, Prometheus usage, caching) are a separate matter:
ensemble is an experimental surface and its parity gaps are deliberate TODOs,
not fresh findings — semantic-router targets got these gates first because they
share the single-winner dispatch shape; graduate ensemble deliberately, in one
pass.

Two shapes, both already implemented — copy the nearest one:

- **Filter the candidate set** (static per-caller predicates like `allowed_cidrs`):
  drop ineligible targets in `routing::resolve_attempt_models` *before* the strategy
  picks, so `max_fallbacks` budgets attempts across reachable targets and a
  metric-based strategy ranks only those. Empty result → the gate's own error.
  Do NOT fold these into `filter_attempt_models`: its
  `when_all_unavailable: try_anyway` policy hands back the unfiltered list, which
  would defeat an allowlist. See `routing::targets_allowed_for_ip`.
- **Check per attempt** (dynamic/stateful gates like a rate-limit reservation):
  resolve from the attempt model *inside* the dispatch loop, in each loop that
  walks targets — chat, messages, count_tokens and responses keep their own, and
  `routing::dispatch_with_failover` serves the single-shot family (completions,
  embeddings, rerank, images, audio, video submits) — and in both the
  streaming and non-streaming branches; skip the target and continue rather than
  failing the whole request. See `quota::reserve_routing_target`, which also shows
  the non-double-charge rule: it returns `None` for non-routing dispatch, whose
  model layers the pre-dispatch `quota::enforce*` already reserved.

Whichever shape, the group's own gate stays enforced pre-dispatch — the two tiers
are additive, not either/or — and a caller-visible rejection must keep the
direct-model envelope (`ModelIpRestricted` names no model and no CIDR), so a group
never becomes a probe for which members exist.

## `request_id` is caller-controlled input, not a gateway-minted UUID

A caller can supply the request id via a configured inbound header, and
`request_id::ensure_request_id` adopts it verbatim, so every
`ClientContext.request_id` / `RequestId` value downstream may be a string the
caller chose. It is only guaranteed to be 1..=256 bytes of visible ASCII
(`request_id::is_acceptable`) — **not** a UUID, and not unique: nothing stops two
requests carrying the same id, so an id is a grouping key, never an identity.

New code that consumes it must therefore treat it as untrusted: escape it for the
sink rather than interpolating it raw (a URL, a file path, a shell argument, a log
format that isn't structured). Never make it a Prometheus label — unbounded
cardinality straight from the caller. The existing sinks are already safe and show
the shape: an OTLP span attribute, an HTTP header value, a `tracing` field, a
parameterised SQL bind.

`is_acceptable` is half of a cross-repo contract with the control plane's `validRequestID`
(the control plane's telemetry ingest): tightening this side alone
silently strands ids, and tightening THAT side alone silently drops the request
from billing and /logs while the caller still gets a 200 carrying the id. Change
both or neither.

## An upstream call reports itself to the request's telemetry, by hand

Three request-scoped observations are fed from the point an upstream is
called, and a call site that skips them reports nothing wrong — the field is
just absent or off, which reads like a quiet upstream:

- **Response headers.** Every upstream HTTP response that answers an attempt —
  success or failure — goes through `aisix_gateway::note_upstream_response`
  as soon as its head is in hand (`stream_timeout::send_with_deadline` and the
  provider bridges already do; Bedrock through an SDK interceptor). That is
  the only source of an exporter's `upstream_response_headers` and of the
  `provider_request_id` a failed attempt or a token count records.
- **Upstream wait.** `gateway_overhead_ms` subtracts the time the request
  spent inside attempt windows. `RoutingTelemetry::begin_attempt` / `record`
  open and close them; a surface that dispatches without it calls
  `attribution::note_upstream_wait_begin` / `note_upstream_wait_end`, and
  every stream that stamps `upstream_ttft_ms` also calls
  `attribution::note_upstream_first_frame` at that point.
- **Streamed output parts.** The structured output an exporter reports is read
  off the client-facing SSE bytes by the telemetry middleware
  (`capture_streamed_output`, keyed on the route), so a new streaming family
  that carries an assistant turn has to be added there; a buffered answer
  attaches its parts at its own `CapturedContent` site with
  `aisix_obs::OutputCollector::from_body` over the JSON the caller receives.

## Every upstream credential a request spends passes the provider-key budget

A `provider_key` budget caps one upstream credential, so it is judged where the
credential is chosen, not once per request: `budget::BudgetGate::check_model` /
`check_provider_key` against the provider key the attempt will actually use.
`routing::resolve_attempt_models` covers every route that goes through it (it
filters group targets beside the client-IP allowlist and refuses a direct
model); the paths that resolve a provider key on their own — semantic
`select_eligible`, the ensemble member and streaming-judge calls, realtime,
`inject` passthrough routes, the files/batches/fine-tuning and video follow-up
routes — each call the gate themselves. A new path that resolves a provider key
for a client request must do the same. Gateway-internal calls (health checks,
the semantic-routing and guardrail embedders) and MCP / A2A do not spend a
provider key's budget.

## A Failure Row Carries Both Bodies Under Full Content

**With any exporter at `content_mode: full`, every usage event with a status of 400 or above or an `error_class` exports the request body and that attempt's own response body, whichever endpoint and status it is.** `usage_attr::emit_usage` fills them in, so a family cannot opt out by what it passes; it can only starve the fill. A new or changed route owes two things:

- **Note the request body before its first possible failure** with `usage_attr::note_failure_request`, and after the chain has finished with it with `usage_attr::note_screened_request`, in the representation the route's success path captures. While an enforcing input rule applies and has not finished with the request, its rows carry no request text. A route calls `usage_attr::hold_ahead_of_chain` as soon as it knows the key and the model named, which covers a 400 or 404 ahead of the chain, on a 403 the row keeps its body; `quota::reserve_before_costly_check` takes over from there, covering its own 429 and a caller hanging up during the check with no exception; and `note_screened_request` or `release_failure_request` ends it, so a route that forgets exports no body. A route with no chain to run does not call it. A route whose rate limit comes ahead of its chain notes a masked copy on that exit with `note_unscreened_request`. A check that failed open (`verdict.is_bypass()`) processed none of the text, so the route notes an empty body and leaves the request out of what it captures, on every route that runs a chain. A guardrail block returns before the mask runs: the refusal notes its body with `usage_attr::note_refused_request`, which masks every hit of the chain's input rules, block rules included, so a blocked row never exports what the success path would have hidden or what the request was refused for. That copy is only for the row; the caller's refusal and the upstream path do not see it. A refused request without one, or whose hits cannot be located (a remote or script verdict names no span, a check failed open, a key cannot be renamed), exports no body: `guardrail_block_error` is what takes the earlier note off the row, so a route that forgets the call fails closed. `usage_attr::note_client_error` holds the error body the terminal row reports when no upstream answered; a route that does not answer with the OpenAI envelope uses `note_client_error_body`.
- **Read every non-2xx upstream body through `aisix_gateway::read_body_capped`** (or hand a body it must relay anyway to `note_upstream_error_body`). That is what reaches the row, and a read that swallows its failure leaves `error_message` naming nothing. All of it is gated on the exporters: under `metadata_only` nothing extra may be read or copied, and a route never reads a body it would otherwise leave unread, because that delays its caller.

**Every failure row also names the failure in `error_message`, at any `content_mode`, whoever wrote the text.** A typed error goes through `attempt::gateway_error_message`. A non-2xx relayed as sent has no typed error: keep at most `MAX_RELAYED_ERROR_DETAIL_BYTES` of a body the route holds or relays anyway, without holding the relay for it, and build the row's fields with `attempt::relayed_failure`. A guardrail refusal stays `guardrail_block_message`: the message and the guardrail's name, never what it matched.

**`usage_attr::emit_usage` guarantees a failure row a non-empty `error_message`, and `AccessLog` gives a line with a status of 400 or above a non-empty `error`, both with `request_failed_message`.** That wording is the backstop for a failure nobody anticipated, not a way to name one: a route that can say why writes it at the source. A row with a status of 400 or above that a route answers itself, like a 501 for a provider lacking the capability, sets `error_class` too, since the access log reads the row's failure only through it.

**A stream's failure does not withhold what it delivered.** A stream's `response_capture_safe` answers one question, whether a scan that was owed has read the text. An unguarded stream starts safe and stays safe when it fails, so its row reports what the caller received; a guarded stream starts unsafe and only its completed scan makes it safe, so one that fails never exports text no guardrail read. Do not clear a stream's capture on the failure itself, on any family.

**None of this changes what a caller is sent.** `BridgeError::UpstreamStatus.message` decides that (blank falls back to the status and reason phrase), so wording for logs and rows belongs in the variant's `Display` (`with_body_read_error`) and never in that field; a stream's error frame takes `stream_frame_message`, never `Display`. A row on a `499` has no error body: the caller left, so nothing was sent to report.

## Platform mode

**Platform mode is for a deployment that serves many tenants through upstream accounts the platform owns. A tenant must not learn which upstream channel, provider or account served it, and must not reach anything another tenant left upstream. The mode is off by default, and with it off every exit answers byte for byte as it would without the feature.**

### The switch

- `proxy.platform.enabled` turns it on, and `proxy.platform.id_key_env` names the environment variable that holds the platform secret. Both are read once at startup. Config validation refuses `enabled: true` without `id_key_env`, and startup refuses a variable that is missing or holds fewer than 32 bytes; the error names the variable, never its value.
- The id key is the SHA-256 of the secret (`platform::PlatformSettings`, whose `Debug` hides it and every key derived from it). Every use of it is separated by a label of its own: the tool-call mapping puts the protocol's label in what it hashes, and each Responses seal derives a key of its own (`opaque_id::derive_key`), so no two kinds of id can be turned into each other. The settings derive every key once, at startup, and requests share them; a request never sets up a key.
- Code reads the mode through `ProxyState::platform()` where a handler holds the state, and through `platform_errors::active()` where an error or frame is rendered. The second reads the request's attribution cell, which is installed around the handler and around every poll of its response body, so a stream generator sees it too. The two agree by construction.
- A request's tenant is `platform::tenant_owner`: `team:<team id>` for a key in a team, `key:<key entry id>` for one without. Everything sealed for a caller is sealed to that owner and opens only for it.
- Keep `expose_debug_headers` off in a platform deployment. It gates the cache headers below, and on chat it also names the routing target that answered.

### The exits it changes

Platform mode covers the tenant endpoints: `/v1/chat/completions`, `/v1/messages` with `/v1/messages/count_tokens`, `/v1/responses`, `/v1/embeddings` and `/v1/models`. Each rule applies on the native and the translated path, and on the buffered and the streamed branch.

- **Ids name no channel** (`opaque_id`, `responses_affinity`). An upstream's ids often carry a channel marker.
  - Chat Completions and Messages: the response id is minted fresh for each response (`chatcmpl-` or `msg_` and a random UUID), and every frame of a stream carries the same one. A tool-call id is mapped one way, to an HMAC of the upstream id behind the protocol's own prefix (`call_`, `toolu_`). The mapping is deterministic and both protocols resend the whole conversation, so the id a caller puts into the next turn's history travels upstream unchanged and still pairs. An id already in mapped form (its prefix and exactly 32 lowercase hex digits) is left as it is, since only the caller's own history carries one: an upstream that echoes it back (a Messages `pause_turn` continuation, a code-execution `caller.tool_id`) still pairs with it.
  - On Messages that covers every tool-use id of an answer: the `id` of a `tool_use`, `server_tool_use` or `mcp_tool_use` block, the `tool_use_id` of every result block, and a `caller`'s `tool_id`. Each keeps its family prefix (`toolu_`, `srvtoolu_`, `mcptoolu_`), which the Messages API's request patterns for these fields require (`^srvtoolu_[a-zA-Z0-9_]+$` for a server tool's), so a mapped id sent back as history is accepted and pairs.
  - Responses: an output item's `id`, `call_id`, `approval_request_id` and `caller.caller_id` are sealed reversibly (AES-256-GCM under a nonce derived from the plaintext), and are unsealed again when a request sends them back in `input[]`. That unseal runs before guardrails, cache keys, logs or dispatch read the body. These ids cannot be one-way: a continuation through `previous_response_id` pairs only ids the upstream issued itself. The response id is sealed with a random nonce, together with the target that served it.
  - An item id (`id`, `approval_request_id`) keeps its short prefix (`fc_`, `msg_`), because the upstream validates it. A field that names a call (`call_id`, `caller.caller_id`) is shown behind a fixed `call_` whatever the upstream wrote: on the translated path the call id is the Chat Completions or Messages upstream's own tool-call id, whose prefix names the channel (`tooluse_`, `toolu_`). The kind and the shown prefix are part of each seal (`opaque_id::IdKind`), so an item seal never opens as a call or the other way round.
  - Every Responses seal is bound to the owner (an item or call id in its nonce and its associated data, the response id inside its encrypted plaintext), and pads the upstream id to a 64-byte bucket, so the length of a sealed id does not tell channels apart. A response id's head, the owner and the target, is padded to a 128-byte bucket of its own, so its length does not name the target either. A seal made for another tenant does not open.
  - An item or call id that does not open passes upstream as the opaque string it is. An `item_reference` (an item of that `type`, or one with no `type` and no `role`) whose own `id` does not open is refused instead, whatever other id beside it opens, as a reference to an object stored upstream (`param` `input[i].id`), since the upstream would resolve it against everything the shared account stored.
  - A `previous_response_id` that does not open (another tenant's, a forged one, a bare upstream id) is answered exactly like a response that does not exist, `404` with code `not_found`, before any upstream call. One that opens must still pass the key's provider-group check on the target it recovers, and is answered the same way when it does not. Behind a routing group, the tag that records which target produced an item's encrypted content (`aisix_enc_v2:`) is sealed to the tenant too.
  - Also removed: the usage counters the OpenAI shape does not define (`prompt_cache_hit_tokens`, `prompt_cache_miss_tokens`, `prompt_tokens_details.cache_creation_tokens`) on a rendered Chat Completions answer, and the `cache_creation_tokens` a translated Responses answer adds.
- **An answer keeps only what its API documents** (`opaque_id::MESSAGE_BODY`, `messages_event_shape`, `RESPONSE_BODY`, `RESPONSES_EVENT`). Which members and counters an upstream writes beside the documented ones names the channel, so they are left out of what the caller is sent, after telemetry has read them.
  - A Messages answer, buffered or streamed, native or translated, keeps the members the Messages API's type definitions document but `container` and `diagnostics`, which answer request fields platform mode refuses. Its usage keeps `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` and `cache_creation` (the 5-minute and 1-hour split), which every channel reports; `service_tier`, `inference_geo`, `speed`, `server_tool_use` and the rest are dropped.
  - Each Messages stream event keeps the members its type documents. A native stream relays no `ping`, no frame without a payload (an SSE comment) and no event of an undocumented type; each is still read for telemetry first.
  - A Responses `Response` object, buffered or in a snapshot event, keeps its documented members, and its usage the documented counters. A native stream event loses `obfuscation` and a cloud platform's invocation metrics; its other members are kept.
  - Every rewrite runs after telemetry has read the response, so the usage event keeps the upstream's own id as `provider_request_id`.
- **A relayed answer goes out under a fixed content type** (`platform::relayed_content_type`). The native exits that relay an upstream's answer (the `/v1/messages` and `/v1/responses` event streams, live or held back for a guardrail, and the `/v1/messages/count_tokens` answer) write `text/event-stream` or `application/json` rather than the upstream's `content-type`, whose spelling differs by channel. Every other exit sends a body the gateway serialised itself.
- **Upstream errors reach the caller as the gateway's own classes** (`platform_errors`). An error that came from an upstream, and a gateway refusal whose text names the provider, provider key or account the request resolved to, is answered as one of eight classes with a fixed status, `type`, `code` and message: context length exceeded (`400`), rate limited (`429`, `Retry-After` kept), invalid request (`400`, the upstream's `param` kept when it is shaped like a field name), content filtered (`400`), authentication (`401`), not found (`404`), upstream unavailable (`503`) and internal (`500`). An upstream `401` or `403` is upstream unavailable, since the platform's account is not the caller's to fix. The Anthropic envelope carries the same message and code under Anthropic's documented `type`.
  - Where it applies: the buffered renderers (`ProxyError::envelope`, `caller_status`, `anthropic_envelope`), stream error frames built from a `BridgeError`, upstream error events relayed from a native stream, a buffered Responses object with `status: "failed"`, and relayed upstream error bodies. Because the buffered renderers are shared, a buffered upstream error that any other endpoint returns as a `ProxyError` is classed too.
  - What keeps its envelope: the gateway's own refusals that name no upstream, which are budgets, rate limits, guardrails, request validation and authentication.
  - The operator still sees what happened. The usage event's `status_code`, `error_class` and `error_message`, the access log and the metrics are written from the original error.
- **References to objects stored upstream are refused** (`stored_refs`). Tenants share the platform's upstream accounts, so an object one tenant's request made an upstream store (a container, a file, a vector store, a conversation, a stored prompt, a custom skill) can be used by anyone who names its id. On chat, messages, count_tokens and responses, a request field that names such an object is refused with `400` `invalid_request`, its `param` naming the field's path and never the id. The check runs right after the model access and client-IP checks, before guardrails, rate limits, the cache and any upstream. The module doc lists every field with its source in the API's type definitions. The ids the gateway seals itself (response, item and call ids) are not refused, except an item reference whose id is not the caller's own seal (above).
- **A request cannot choose its own model** (`stored_refs`). The key's model access decides which model a tenant may use, and the platform pays the upstream for the model that actually ran. So on chat, messages, count_tokens and responses, a field with which the caller would pick another one is refused the same way, `400` `invalid_request` with `param` naming the field: a top-level `models`, `provider` or `fallbacks` (an aggregator's model list and provider routing, a fallback chain), and a tool definition naming the model it runs (Responses `image_generation`'s `model`, wherever a tool definition appears, and any Messages tool with a `model`, such as `advisor_20260301`). A `moderation.model` picks the classifier, not the model that answers, and is left alone.
- **Cache headers are debug headers** (`response_cache::apply_cache_headers`). On messages, responses and embeddings, `x-aisix-cache` and `x-aisix-cache-layer` are sent only when `expose_debug_headers` is on, which is what chat does in either mode. Otherwise a hit would tell a tenant that someone asked the same thing.
- **A forbidden model answers like an unknown one** (`platform_errors::caller_error`). A `ModelForbidden` reaches the caller with the status, headers and body of a `ModelNotFound` for the same name, so a key cannot map the catalog by asking for names. It is applied at the three render primitives in `error.rs`, and telemetry still records `403` `permission_denied`.
- **A `429` shows only the caller's own limits** (`error.rs`).
  - A `budget_exceeded` keeps its structured fields (`scope`, `scope_ref`, `limit_usd`, `spent_usd`, `period`, `period_resets_at`) and its `Retry-After` only when the budget is the caller's own: scope `api_key`, `member`, `team` or `team_member`, by both its typed scope and its text. Any other budget (`org`, `environment`, `tag`, `provider_key`, or one with no scope) is answered with the same type and code, the message `Spending limit reached.`, and no `Retry-After`, because its reset time is a figure of the platform's budget. A spend ceiling raised by a policy gets a neutral message of its own.
  - A rate-limit `429` names no policy: no `error.policy` block, and no policy name in the message. A bucket that several callers share (a model's inline limit, or a policy not grouped by key, member or team, `quota::policy_bucket_is_callers`) sends none of `x-ratelimit-limit`, `-remaining`, `-reset` and `-scope`. `Retry-After` stays.
- **Model listings name no vendor** (`models::owned_by`). `/v1/models` and `/v1/models/{id}` in the OpenAI shape report `owned_by: "aisix"` for every model. The Anthropic shape names no owner.

Provider-key groups (`allowed_provider_groups`) and the `team` cache scope are what a platform deployment uses to keep tenants apart upstream and in the cache. They work with platform mode on or off. A key with provider groups is answered as if no ensemble existed: a request naming one gets `404` `model_not_found` (`model_resolve::refuse_ensemble_to_grouped_key`), and `/v1/models` and `/v1/models/{id}` do not show one.

### Operations

- Every replica of a platform deployment must hold the same `id_key_env` secret. A replica with another secret opens none of the ids the others sealed, so a continuation that lands on it is answered as a response that does not exist.
- Rotating the secret invalidates every sealed Responses id handed out under the old one: a `previous_response_id` gets `404` `not_found`, an echoed item reference is refused, and an echoed item or call id reaches the upstream as an opaque string it does not know. Chat Completions and Messages tool-call ids map differently afterwards; a conversation that spans the rotation still pairs within each request, but a client that matches one call's id across turns sees it change.

### Every new exit applies it

Platform mode is a property of each exit, and a missed exit fails silently: nothing errors, the tenant simply sees the upstream's id, text or figures. When a change adds or reshapes a client-facing exit on a tenant endpoint (a new stream branch, a new relay of upstream bytes, a new error variant, a new response header), it applies every rule above in the same change, on both paths and both branches, and pins the result with a platform-mode test beside the off-mode one.

- Render errors through `ProxyError::envelope`, `caller_status` and `anthropic_envelope`, or through the stream and relay helpers in `platform_errors`, never from `Display` or `kind()`. A new variant whose text names the upstream belongs in `platform_errors::hides`.
- Pass every id an upstream issued through `opaque_id` or `responses_affinity`, after telemetry has read it.
- Call the family's `stored_refs::refuse_in_*` check before the first upstream call. A newly found field that names an upstream-stored object goes into `stored_refs`.
- Write cache headers only through `apply_cache_headers`, and a relayed upstream's content type only through `platform::relayed_content_type`.
- With platform mode off, leave the bytes unchanged.

### Known follow-ups

These surfaces are not adapted. Until each is, a platform deployment must not let tenant keys reach it.

- `/v1/completions` returns the upstream's `id` and `system_fingerprint`.
- Realtime frames inside an established session (connect failures, relayed upstream `error` events) are not classed, because the session runs outside the request's attribution scope. Realtime and `/v1/audio/speech` voice ids are not checked as stored objects.
- Passthrough routes relay the upstream's headers and body as sent, Responses bodies included, and the stored-object check does not read them.
- The files, batches and fine-tuning surface, including Responses requests inside a batch file, is outside the id and stored-object rules. Its relayed error bodies are classed.
- Images, audio, video, MCP and A2A have no adaptation beyond the shared error renderers.

On the tenant endpoints themselves:

- Paths that fail open forward an upstream's ids as sent rather than corrupt the frame: a frame the JSON splice cannot rewrite. An oversized unterminated frame ends a native stream on a decode failure instead.
- An operator's `param_renames` runs after the stored-object check, so it could map a caller's field onto a refused one.
- Skills are refused by type `custom`. An allow-list of type `anthropic` would be stricter.
- A bare-string `audio.voice` that names a custom voice is not refused; the object form is.
- A broad `forward_client_headers` rule can forward `anthropic-workspace-id` or `anthropic-user-profile-id` upstream.
- A response cache in `env` scope would replay one tenant's sealed ids to another. Platform deployments use the `team` or `api_key` scope.
