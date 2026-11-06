//! Per-virtual-model routing state + target selection.
//!
//! When a request lands on a Model with `routing` configured, the proxy
//! asks the [`RoutingRegistry`] for an iterator of underlying target
//! Model names in attempt-order. The registry owns the per-virtual-
//! model state (smooth-WRR counters, consistent-hash rings); selection
//! itself is pure given that state.
//!
//! Targets partition into **priority tiers** first (`priority`, higher
//! value preferred, APISIX-style — each tier gets its own balancing
//! state, mirroring APISIX's per-priority pickers): the strategy orders
//! targets within each tier, tiers concatenate best-first, and a lower
//! tier is only reached after every higher-tier target failed or was
//! filtered as unavailable.
//!
//! Positional strategies pick a starting target per tier, then walk
//! forward on failure:
//! - **failover**: always start at the tier's first target, walk forward.
//! - **round_robin**: smooth weighted round-robin over target `weight`s
//!   (equal weights degrade to a plain cycle).
//! - **consistent_hash**: ketama-style hashing of the request's hash key
//!   (the `hash_on` chain) over the tier's ring; the walk follows the
//!   ring, so a failed target's keys spread to their ring successors and
//!   every other key keeps its mapping.
//!
//! Metric-ordered strategies rank targets by a runtime signal within each
//! tier (attempted best-first, then falling forward). They can't be ordered
//! from `pick_targets` because the ranking key lives on the resolved target
//! Models / runtime state, so `resolve_attempt_models` ranks them instead:
//! - **least_cost**: cheapest target first, by combined input+output per-1K
//!   price; targets without a `cost` rank last.
//! - **least_latency**: fastest target first, by an EWMA of observed upstream
//!   latency; targets with no samples yet rank first (probe, then exploit).
//! - **least_busy**: least-loaded target first, by in-flight requests
//!   divided by target `weight` (the APISIX least_conn score).

use aisix_core::models::{LivePricingIndex, PricingIndex};
use aisix_core::{
    AisixSnapshot, HashOnType, Model, Routing, RoutingStrategy, RoutingTarget,
    WhenAllUnavailablePolicy,
};
use aisix_gateway::BridgeError;
use axum::http::HeaderMap;
use dashmap::DashMap;
use rand::Rng;
use std::sync::Mutex;
use std::time::Duration;

use crate::error::ProxyError;

/// Default Retry-After (in seconds) returned to the client when every
/// candidate is background-unhealthy and no cooldown timer is available
/// to derive a more precise hint. Operators tune per-model cooldown
/// TTLs via `cooldown.default_seconds`; this is only the all-unhealthy
/// fallback for the `when_all_unavailable: fail` path.
const FALLBACK_ALL_UNHEALTHY_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Whether a Bridge error is retryable at all, optionally treating 429
/// as retryable. Non-429 4xx is the caller's mistake — retrying won't
/// help and may amplify damage. An undecodable 2xx body was already
/// served. Everything else (5xx, timeout, transport, config, stream
/// abort) gets the retry/failover path.
///
/// `fallback_on_statuses` (#1012) is the routing model's
/// explicit opt-in list for providers that use 4xx codes for transient
/// conditions (overload, queue full, quota): a status in the list is
/// retryable regardless of the default classification. Empty by default,
/// which preserves the historical behavior exactly.
pub fn is_retryable(err: &BridgeError, retry_on_429: bool, fallback_on_statuses: &[u16]) -> bool {
    match err {
        BridgeError::UpstreamStatus { status, .. } => {
            if fallback_on_statuses.contains(status) {
                return true;
            }
            if *status == 429 {
                return retry_on_429;
            }
            !(400..500).contains(status)
        }
        // An in-band stream error with an embedded status follows the
        // same status rules as an HTTP status error (a mainstream gateway applies
        // its non-429-4xx filter to in-body stream errors identically).
        // Without a status the provider reported an unspecified stream
        // fault — transient by assumption, like Transport.
        BridgeError::UpstreamInBand { status, .. } => match status {
            Some(s) => {
                if fallback_on_statuses.contains(s) {
                    return true;
                }
                if *s == 429 {
                    return retry_on_429;
                }
                !(400..500).contains(s)
            }
            None => true,
        },
        // Customer-fixable config / credentials (#367): re-sending to this
        // target won't help, same as a non-429 4xx. Another target may be
        // fine, which is `target_unusable`'s question.
        BridgeError::InvalidUpstreamConfig(_)
        | BridgeError::InvalidRequest(_)
        | BridgeError::RequestTooLarge { .. }
        | BridgeError::InvalidUpstreamCredentials(_) => false,
        // A capability the adapter simply does not implement. Static per
        // adapter, so the same call answers the same way every time and a
        // retry can only add latency to a refusal the caller is going to
        // get anyway. Spelled as `Config` before #1093, which made it
        // retryable and burned the whole budget before the 501 surfaced.
        //
        // The three routes that can raise it — `/v1/completions`,
        // `/v1/embeddings`, `/v1/images/generations` — answer it with a
        // 501 naming the unsupported capability, so a Model Group stops on
        // it rather than failing over (`dispatch_with_failover`), the same
        // as on any other non-retryable refusal.
        BridgeError::UnsupportedCapability(_) => false,
        // The upstream answered 2xx and the gateway could not read the
        // body: the call was served — and billed — already, so another
        // attempt would run (and charge for) the same request again.
        BridgeError::UpstreamDecode(_) | BridgeError::UpstreamBodyUnread { .. } => false,
        BridgeError::Timeout { .. }
        | BridgeError::TokenExchangeDecode(_)
        | BridgeError::Transport(_)
        | BridgeError::Config(_)
        | BridgeError::StreamAborted => true,
    }
}

/// Whether a failure is this target's own configuration rather than anything
/// about the request or the upstream's answer: a key that cannot be used, a
/// row that names no provider key, an adapter with no bridge. Re-sending to
/// this target cannot help ([`is_retryable`] says no), but the next target
/// has a configuration of its own, so a dispatch loop moves on instead of
/// ending the request. With no target left the caller gets this error, as a
/// direct model's caller does.
pub(crate) fn target_unusable(err: &ProxyError) -> bool {
    match err {
        ProxyError::UnusableTarget(_) | ProxyError::ProviderUnavailable => true,
        ProxyError::Bridge(be) => bridge_target_unusable(be),
        _ => false,
    }
}

/// [`target_unusable`] for the `BridgeError`-typed dispatch loops.
pub(crate) fn bridge_target_unusable(err: &BridgeError) -> bool {
    matches!(
        err,
        BridgeError::InvalidUpstreamConfig(_) | BridgeError::InvalidUpstreamCredentials(_)
    )
}

/// The refusal a Model Group's caller gets when its walk ended on a target's
/// own configuration. That error's text names the member and its provider
/// key, for a direct model's operator; a group's caller gets the name it
/// addressed ([`refused_model_label`]), and the attempt's row keeps the rest.
pub(crate) fn group_refusal(err: ProxyError, addressed: &str) -> ProxyError {
    match err {
        ProxyError::UnusableTarget(_) => ProxyError::UnusableTarget(format!(
            "{} has no usable target",
            refused_model_label(addressed)
        )),
        other => other,
    }
}

/// One WARN per failed routing-target attempt, naming the target, the
/// error, whether the loop will move on, and the `fallback_on_statuses`
/// list that answer was computed against.
///
/// Kept in one place because the endpoint family had already drifted:
/// `/v1/chat/completions` wrote this line on both its branches while
/// `/v1/messages`, `/v1/responses` and `/v1/messages/count_tokens`
/// emitted nothing at all on a failed attempt, at any level.
///
/// The status list is on the line because the retry/failover decision is
/// not reconstructable without it. A group configured to fail over on an
/// upstream 400, whose projected snapshot never carried the list, refuses
/// to fail over and leaves exactly the trace a group with one reachable
/// candidate leaves — one attempt, error class `upstream_status`
/// (AISIX-Cloud#1499).
///
/// `request_id` is a field of the line itself, not only of the enclosing
/// `request` span: that span is INFO-level, so at `log_level=warn` it is
/// disabled and this line would otherwise carry no way back to the request
/// (AISIX-Cloud#1136).
pub(crate) fn log_attempt_failure(
    request_id: &str,
    target_model: &str,
    attempt_number: usize,
    err: &dyn std::fmt::Display,
    retryable: bool,
    fallback_on_statuses: &[u16],
) {
    tracing::warn!(
        request_id = %aisix_core::LogValue(request_id),
        target_model = %target_model,
        target_attempt = attempt_number,
        error = %err,
        retryable,
        ?fallback_on_statuses,
        "routing target attempt failed",
    );
}

/// Base delay before the first same-target retry. Each subsequent retry
/// doubles it, capped at [`RETRY_BACKOFF_MAX_MS`].
const RETRY_BACKOFF_BASE_MS: u64 = 250;
/// Ceiling for the exponential term — bounds the worst-case added latency.
const RETRY_BACKOFF_MAX_MS: u64 = 2_000;
/// Additive jitter ceiling, sampled uniformly in `[0, this]` and added on
/// top of the exponential term.
const RETRY_BACKOFF_JITTER_MS: u64 = 250;

/// Longest upstream-supplied `Retry-After` we are willing to sit on before
/// falling back to our own exponential term. A mainstream gateway honours anything up to
/// 60s (`_calculate_retry_after`); an inline proxy cannot — the wait burns
/// the caller's own latency budget, and a 45s hold reads as a hang to the
/// client. Same reason the exponential bounds below are tightened relative
/// to a mainstream gateway's library defaults.
const RETRY_AFTER_HONOR_MAX_MS: u64 = 5_000;

/// Backoff before retrying the **same** target, for 1-based retry number
/// `retry` (`retry == 0` → no wait).
///
/// When the upstream told us how long to wait (`Retry-After`, typically on
/// a 429) and the hint is within [`RETRY_AFTER_HONOR_MAX_MS`], we do what
/// it says — a provider's own quota window beats a guess. Otherwise:
/// exponential term `base * 2^(retry-1)` capped at [`RETRY_BACKOFF_MAX_MS`].
/// Either way uniform additive jitter in `[0, RETRY_BACKOFF_JITTER_MS]` is
/// added, so a fleet retrying off the same upstream fault does not
/// synchronise.
///
/// Same strategy as a mainstream gateway's router (retry-after calculation: honour a
/// sane `Retry-After`, else capped exponential floor + additive jitter —
/// not full-jitter-to-zero, so a struggling upstream always gets a real
/// pause), with bounds tightened from a mainstream gateway's library defaults (0.5s base
/// / 8s cap / 60s `Retry-After` ceiling) to suit an inline proxy where the
/// retry runs inside a single request's latency budget. Cross-target
/// fallover is deliberately NOT backed off — a different, presumably
/// healthy target should be tried immediately (a mainstream gateway's healthy-deployment
/// fast-path).
pub fn retry_backoff(retry: u32, retry_after: Option<Duration>) -> Duration {
    if retry == 0 {
        return Duration::ZERO;
    }
    let jitter = rand::thread_rng().gen_range(0..=RETRY_BACKOFF_JITTER_MS);
    if let Some(hint) = retry_after {
        let hint_ms = hint.as_millis().min(u64::MAX as u128) as u64;
        if hint_ms > 0 && hint_ms <= RETRY_AFTER_HONOR_MAX_MS {
            return Duration::from_millis(hint_ms + jitter);
        }
    }
    let exp = RETRY_BACKOFF_BASE_MS.saturating_mul(1u64 << (retry - 1).min(20));
    let base = exp.min(RETRY_BACKOFF_MAX_MS);
    Duration::from_millis(base + jitter)
}

/// The `Retry-After` hint an upstream attached to this failure, if any.
/// Only [`BridgeError::UpstreamStatus`] carries one (parsed by
/// `aisix_gateway::parse_retry_after`); transport faults and timeouts have
/// nothing to report.
pub fn retry_after_hint(err: &BridgeError) -> Option<Duration> {
    match err {
        BridgeError::UpstreamStatus { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// Retry budget for one dispatch target, resolved across the three levels
/// an operator can set it at.
///
/// `target.retries` (this model's own budget) wins, then the group's
/// `routing.retries` (the historical knob, now a group-wide default), then
/// the deployment-wide `upstream.retries` from the DP config.
///
/// Per-target beats per-group because a routing target *is* a Model: "how
/// many times may this upstream be re-hit" is a property of that upstream,
/// and target A tolerating three retries says nothing about target B. A
/// direct (non-group) model has no `group`, which is exactly why it used to
/// end up with a hardcoded zero — the knob only ever existed on the group.
///
/// `has_fallback_targets` says whether another candidate target is still
/// queued behind this one. It only gates the DEPLOYMENT DEFAULT: when the
/// operator configured nothing and a fallback is available, prefer failing
/// over to grinding the same failing upstream. An explicitly configured
/// budget — at either level, including `0` — is always honoured as written.
///
/// That distinction is what keeps the default from silently degrading
/// `timeout`-driven fail-over (#554): a two-target group whose first target
/// times out should move on after one timeout, not after three. It also
/// tracks what a mainstream gateway actually does, which is easy to misread. Its
/// `num_retries` does not re-hit one deployment — each retry re-enters
/// deployment selection, and the failed deployment has meanwhile been
/// cooled down, so a retry inside a multi-deployment group lands on a
/// DIFFERENT deployment. Same-target grinding is what a mainstream gateway does only
/// when a group holds a single deployment, which is exactly the case this
/// keeps the default for.
pub fn effective_retries(
    target: &aisix_core::Model,
    group_retries: Option<u32>,
    deployment_default: u32,
    has_fallback_targets: bool,
) -> RetryBudget {
    if let Some(explicit) = target.retries.or(group_retries) {
        return RetryBudget {
            attempts: explicit as usize,
            configured: true,
        };
    }
    RetryBudget {
        attempts: if has_fallback_targets {
            0
        } else {
            deployment_default as usize
        },
        configured: false,
    }
}

/// The group-level slot of the member → group → deployment-default
/// retries chain, resolved from the caller-addressed parent entry:
/// `routing.retries` for a Model Group, the parent's own top-level
/// `retries` otherwise — a semantic router has no `routing` block, so
/// its group level lives on the Model itself (the same place a routing
/// group keeps its group-level `timeout`).
pub fn group_retries_of(parent: &aisix_core::Model) -> Option<u32> {
    match parent.routing.as_ref() {
        // A Model Group's group slot is `routing.retries` alone — a
        // stray top-level `retries` on the group Model stays inert
        // (the schema-convergence work forbids that shape outright).
        Some(routing) => routing.retries,
        None => parent.retries,
    }
}

/// How many same-target retries this dispatch may spend, and whether the
/// operator asked for them.
#[derive(Debug, Clone, Copy)]
pub struct RetryBudget {
    /// Retries after the initial attempt.
    pub attempts: usize,
    /// True when the number came from `Model.retries` or `routing.retries`
    /// rather than from the deployment default.
    configured: bool,
}

impl RetryBudget {
    /// Whether `err` is allowed to spend this budget.
    ///
    /// A budget the operator never configured does not retry timeouts. A
    /// `timeout` is an explicit "stop waiting on this upstream" threshold,
    /// so spending an unasked-for budget on it triples the very wait the
    /// operator bounded — and an upstream that just burned the full budget
    /// will most likely burn it again. Transport faults and 5xx are the
    /// opposite: they fail fast and are often momentary, which is exactly
    /// what a retry is for.
    ///
    /// An explicitly configured budget retries everything retryable,
    /// timeouts included — the operator asked for it by name.
    ///
    /// Timeouts remain retryable for FAIL-OVER purposes either way
    /// (`is_retryable`); this only governs re-hitting the same target.
    pub fn covers(&self, err: &BridgeError) -> bool {
        self.configured || !matches!(err, BridgeError::Timeout { .. })
    }
}

/// Request/stream deadlines for one dispatch target, resolved across the
/// same levels as [`effective_retries`]: the target model, then its group,
/// then the deployment-wide `upstream.timeout_ms` /
/// `upstream.stream_timeout_ms` defaults from the DP config.
#[derive(Debug, Clone, Copy)]
pub struct TimeoutBudget {
    /// End-to-end deadline for a non-streaming call. `None` = unbounded.
    pub request: Option<std::time::Duration>,
    /// Streaming budget: bounds the connect phase and the gap between
    /// chunks. `None` = unbounded.
    pub stream: Option<std::time::Duration>,
    /// True when `stream` came from the target/group resources rather than
    /// the deployment defaults. Gates the pre-200 first-chunk peek: an
    /// operator who configured a streaming budget on the resource asked
    /// for slow-first-token FAILOVER (#554), which requires withholding
    /// the 200 until the first chunk arrives. The deployment default must
    /// NOT do that — it is a backstop, and withholding headers for its
    /// (long) duration would also silence the SSE heartbeats that exist
    /// precisely to cover a slow first token (#1126). With the
    /// default budget, a first-chunk stall surfaces as an in-band timeout
    /// after the 200 instead of failing over. Same shape as
    /// [`RetryBudget::covers`]: explicit config opts into the sharper
    /// behaviour, the deployment default stays conservative.
    pub stream_configured: bool,
}

/// Deployment-wide timeout defaults (`upstream.timeout_ms` /
/// `upstream.stream_timeout_ms`) with the `0` = "no default" sentinel
/// already folded to `None`.
#[derive(Debug, Clone, Copy)]
pub struct TimeoutDefaults {
    pub request: Option<std::time::Duration>,
    pub stream: Option<std::time::Duration>,
}

impl Default for TimeoutDefaults {
    /// Mirrors `UpstreamConfig::default()` so an embedded ProxyState built
    /// without config wiring behaves like a default deployment.
    fn default() -> Self {
        Self {
            request: Some(std::time::Duration::from_millis(
                aisix_core::config::DEFAULT_UPSTREAM_TIMEOUT_MS,
            )),
            stream: None,
        }
    }
}

/// Resolve the request/stream deadlines for one dispatch target.
///
/// `timeout` resolves model → group → deployment default, first level that
/// says anything wins. An explicit `0` at model or group level resolves to
/// "no deadline" and STOPS the chain — that is how an operator opts a
/// long-running model out of the deployment backstop.
///
/// The streaming budget resolves the RESOURCE levels first — the model /
/// group `stream_timeout` (`0` defers, its historical semantics), then the
/// resource-resolved `timeout` — and only then the deployment defaults,
/// `stream_timeout_ms` falling back to `timeout_ms`. Within that, the
/// dedicated stream knob outranks the generic one at EVERY level: a
/// group's `stream_timeout` beats a member's `timeout` for streams, and
/// supplies a budget even to a member whose `timeout: 0` opted out of the
/// request deadline. (A model with only `timeout` still gets that value
/// as its streaming budget, and its `timeout: 0` still opts the stream
/// out, whenever no resource-level `stream_timeout` exists.) This is a mainstream
/// gateway router's shape: the `stream_timeout` chain is exhausted before
/// the non-stream `timeout` chain is consulted at all.
pub fn effective_timeouts(
    target: &Model,
    group: Option<&Model>,
    defaults: TimeoutDefaults,
) -> TimeoutBudget {
    let request_level = target
        .request_timeout_level()
        .or_else(|| group.and_then(|g| g.request_timeout_level()));
    let request = request_level.unwrap_or(defaults.request);
    let resource_stream = target
        .stream_read_timeout()
        .or_else(|| group.and_then(|g| g.stream_read_timeout()));
    let (stream, stream_configured) = if let Some(d) = resource_stream {
        (Some(d), true)
    } else if let Some(r) = request_level {
        (r, r.is_some())
    } else {
        (defaults.stream.or(defaults.request), false)
    };
    TimeoutBudget {
        request,
        stream,
        stream_configured,
    }
}

/// Drive one single-model upstream call under that model's retry budget.
///
/// For a caller-addressed Model use [`dispatch_with_failover`] instead —
/// it walks a Model Group's targets. This is for the callers that already
/// hold ONE concrete model: an ensemble member, a passthrough route.
///
/// `retry_on_429` / `fallback_on_statuses` are group-level knobs, so the
/// default classification applies: 5xx, timeout, transport, decode and
/// stream-abort retry; every 4xx (429 included) is returned as-is.
pub(crate) async fn retrying_dispatch<F, Fut, T>(
    state: &crate::ProxyState,
    model: &aisix_core::Model,
    endpoint: &'static str,
    call: F,
) -> Result<T, BridgeError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, BridgeError>>,
{
    retrying_dispatch_gated(state, model, endpoint, |_| true, call).await
}

/// [`retrying_dispatch`] with a caller-supplied `permit` predicate that can
/// veto spending the budget on a particular failure.
///
/// Exists for the two endpoints that replay requests they did not author —
/// passthrough and /v1/videos — where a retry can re-execute a
/// **non-idempotent upstream write**. The dangerous case is a failure
/// AFTER the upstream returned its status: the operation committed, only
/// the response body was lost, and a retry duplicates it (a second file
/// upload, a second paid video task whose id the caller never saw). Those
/// callers veto `UpstreamDecode` for non-idempotent methods. Send-phase
/// transport failures stay retryable — whether the request reached the
/// upstream is unknowable there, and the OpenAI SDK and mainstream gateway routers both
/// accept that ambiguity and retry POSTs on connection errors.
///
/// The first-class endpoints don't need a veto: their POST bodies are
/// generation requests the gateway itself authored, where a replay is the
/// documented cost of retrying (same as every provider SDK).
pub(crate) async fn retrying_dispatch_gated<P, F, Fut, T>(
    state: &crate::ProxyState,
    model: &aisix_core::Model,
    endpoint: &'static str,
    permit: P,
    mut call: F,
) -> Result<T, BridgeError>
where
    P: Fn(&BridgeError) -> bool,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, BridgeError>>,
{
    let budget = effective_retries(model, None, state.default_retries, false);
    let mut last_err: Option<BridgeError> = None;
    for attempt_idx in 0..=budget.attempts {
        if attempt_idx > 0 {
            let hint = last_err.as_ref().and_then(retry_after_hint);
            let backoff = retry_backoff(attempt_idx as u32, hint);
            tracing::debug!(
                endpoint,
                model = %model.display_name,
                next_attempt = attempt_idx + 1,
                backoff_ms = backoff.as_millis() as u64,
                "backing off before retry",
            );
            tokio::time::sleep(backoff).await;
        }
        match call().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !is_retryable(&e, false, &[]) || !budget.covers(&e) || !permit(&e) {
                    return Err(e);
                }
                tracing::warn!(
                    endpoint,
                    model = %model.display_name,
                    attempt = attempt_idx + 1,
                    max_attempts = budget.attempts + 1,
                    error = %e,
                    "retryable upstream failure",
                );
                last_err = Some(e);
            }
        }
    }
    // Unreachable with `last_err == None`: the loop body either returns or
    // stores an error, and it runs at least once.
    Err(last_err.unwrap_or_else(|| BridgeError::Config("retry loop produced no error".into())))
}

/// What [`dispatch_with_failover`] hands back on success: the call's own
/// value, the concrete target that produced it, when that attempt began,
/// and that target's model-layer rate-limit reservation (`None` for a
/// direct model), which the caller merges into its request reservation
/// before committing tokens so the member's TPM/TPD is billed too
/// (#1087).
pub(crate) struct Dispatched<T> {
    pub value: T,
    pub target: AttemptModel,
    /// Start of the attempt that produced `value` — what a relayed
    /// response's end-of-stream usage event measures its latency from, so
    /// the failed attempts before it are not counted twice.
    pub attempt_started: std::time::Instant,
    pub member_reservation: Option<aisix_ratelimit::MultiReservation>,
    /// The target's `least_busy` in-flight count, raised for the winning
    /// attempt. A buffered response is complete once `value` exists, so
    /// dropping it with the rest is right; a relayed stream moves it into
    /// its body so the target counts as busy until the relay ends.
    pub in_flight: crate::health::InFlightGuard,
    /// Only from [`dispatch_stream_with_failover`]: call
    /// [`crate::attempt::DeferredAttempt::finish`] on it once the stream's
    /// real terminal status is known, or the winning attempt never reaches
    /// the deployment counters at all.
    pub deferred: Option<crate::attempt::DeferredAttempt>,
}

/// The caller-addressed model as a refusal names it: only that name, never
/// the dispatched row, so a refusal cannot be used to probe which members a
/// Model Group holds or which row serves a wildcard alias. An operator finds
/// the member from the failed attempt's row and the access log.
pub(crate) fn refused_model_label(addressed: &str) -> String {
    format!("model `{addressed}`")
}

/// Dispatch a single-shot (non-streaming-relay) request to the Model the
/// caller addressed: a direct model is its own only target, a Model Group
/// walks its `routing.targets` exactly as `/v1/chat/completions` does
/// (strategy order, health/cooldown filter, per-target client-IP allowlist,
/// per-target model rate-limit layers), with same-target retries before
/// failing over.
///
/// `call` performs ONE attempt against the target it is handed, with that
/// target's resolved deadlines. It must build all the per-target state it
/// needs (provider key, upstream model id, request body rewrite) from
/// the target, never from the caller-addressed entry — a group has no
/// provider. A retryable `ProxyError::Bridge` or a failure of the target's own
/// configuration ([`target_unusable`]) moves on; any other error (a 4xx) is
/// returned as-is. `permit` can veto spending the
/// retry/fail-over budget on a failure after which a replay would duplicate
/// a non-idempotent upstream write (see [`retrying_dispatch_gated`]).
///
/// A failed attempt's health mark lands on the target, so one bad member is
/// what goes down rather than the group; `call` feeds the target's cooldown
/// from its own error and the caller records the winner's recovery.
///
/// Every attempt is recorded on `telemetry` — the same
/// [`crate::attempt::RoutingTelemetry`] chat, messages and responses use —
/// so the per-attempt usage events and the `aisix_deployment_*` /
/// `aisix_routing_*_fallbacks_total` metrics come out of the one
/// chokepoint (#655). `unsupported` names an `Ok` value that is really the
/// route's own 501 for a provider that lacks the capability: that attempt
/// ends the dispatch like any non-retryable refusal and is recorded as a
/// failed, never-dispatched one rather than as a success.
///
/// A successful attempt also feeds the target's latency to the
/// `least_latency` strategy, exactly as chat does, so a group on that
/// strategy learns its targets on every endpoint of the family. Every
/// attempt is counted in flight for `least_busy` while it runs, and the
/// winner's count is handed back in [`Dispatched::in_flight`].
///
/// The one chokepoint for the single-shot endpoint family — completions,
/// embeddings, rerank, images, audio, videos — so they dispatch Model
/// Groups identically (#1111).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_with_failover<T, P, U, F, Fut>(
    state: &crate::ProxyState,
    snapshot: &AisixSnapshot,
    auth: &crate::auth::AuthenticatedKey,
    client: &crate::client_ip::ClientContext,
    requested_name: &str,
    entry: &aisix_core::ResourceEntry<Model>,
    telemetry: &mut crate::attempt::RoutingTelemetry,
    permit: P,
    unsupported: U,
    call: F,
) -> Result<Dispatched<T>, ProxyError>
where
    P: Fn(&BridgeError) -> bool,
    U: Fn(&T) -> Option<String>,
    F: FnMut(AttemptModel, TimeoutBudget) -> Fut,
    Fut: std::future::Future<Output = Result<T, ProxyError>>,
{
    walk_targets(
        state,
        snapshot,
        auth,
        client,
        requested_name,
        entry,
        telemetry,
        false,
        permit,
        unsupported,
        call,
    )
    .await
}

/// [`dispatch_with_failover`] for a route whose `Ok` means "the stream
/// OPENED", not "the request succeeded". The winning attempt is staged
/// rather than recorded: booking a deployment success at that point counts a
/// stream that may still die, and nothing afterwards corrects it. The caller
/// finishes it through [`Dispatched::deferred`] once the terminal outcome is
/// known.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_stream_with_failover<T, P, U, F, Fut>(
    state: &crate::ProxyState,
    snapshot: &AisixSnapshot,
    auth: &crate::auth::AuthenticatedKey,
    client: &crate::client_ip::ClientContext,
    requested_name: &str,
    entry: &aisix_core::ResourceEntry<Model>,
    telemetry: &mut crate::attempt::RoutingTelemetry,
    permit: P,
    unsupported: U,
    call: F,
) -> Result<Dispatched<T>, ProxyError>
where
    P: Fn(&BridgeError) -> bool,
    U: Fn(&T) -> Option<String>,
    F: FnMut(AttemptModel, TimeoutBudget) -> Fut,
    Fut: std::future::Future<Output = Result<T, ProxyError>>,
{
    walk_targets(
        state,
        snapshot,
        auth,
        client,
        requested_name,
        entry,
        telemetry,
        true,
        permit,
        unsupported,
        call,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn walk_targets<T, P, U, F, Fut>(
    state: &crate::ProxyState,
    snapshot: &AisixSnapshot,
    auth: &crate::auth::AuthenticatedKey,
    client: &crate::client_ip::ClientContext,
    requested_name: &str,
    entry: &aisix_core::ResourceEntry<Model>,
    telemetry: &mut crate::attempt::RoutingTelemetry,
    defer_outcome: bool,
    permit: P,
    unsupported: U,
    mut call: F,
) -> Result<Dispatched<T>, ProxyError>
where
    P: Fn(&BridgeError) -> bool,
    U: Fn(&T) -> Option<String>,
    F: FnMut(AttemptModel, TimeoutBudget) -> Fut,
    Fut: std::future::Future<Output = Result<T, ProxyError>>,
{
    use crate::attempt::{AttemptRecord, AttemptTarget};

    let budget_gate = crate::budget::gate(state, snapshot);
    let attempt_models = resolve_attempt_models(
        &state.routing,
        &state.runtime_status,
        &state.pricing,
        snapshot,
        requested_name,
        &entry.id,
        &entry.value,
        RoutingRequest {
            tags: &client.routing_tags,
            headers: Some(&client.headers),
            api_key_id: auth.entry.id.as_str(),
            source_ip: &client.source_ip,
            context_origin: None,
            budget: Some(&budget_gate),
            serves: None,
            allowed_groups: &auth.key().allowed_provider_groups,
        },
    )?;
    let routing = entry.value.routing.as_ref();
    let retry_on_429 = routing.is_some_and(|r| r.retry_on_429_or_default());
    let fallback_statuses: &[u16] = routing
        .map(|r| r.fallback_on_statuses_or_default())
        .unwrap_or(&[]);
    let n = attempt_models.len();
    let mut last_err: Option<ProxyError> = None;
    for (i, target) in attempt_models.into_iter().enumerate() {
        let pk_id = crate::dispatch::resolve_provider_key(snapshot, &target.model)
            .map(|e| e.id.clone())
            .unwrap_or_default();
        // The routing target's name on the attempt record; empty for a
        // direct model, whose `model_id` already identifies it.
        let target_model = if routing.is_some() {
            target.model.display_name.clone()
        } else {
            String::new()
        };
        let begin = |telemetry: &mut crate::attempt::RoutingTelemetry| {
            telemetry.begin_attempt(AttemptTarget {
                display_name: &target.model.display_name,
                target_model: &target_model,
                model_id: &target.id,
            })
        };
        let member_reservation = match crate::quota::reserve_routing_target(
            state,
            snapshot,
            auth,
            routing.is_some().then_some(crate::quota::RoutingParent {
                name: &entry.value.display_name,
                entry_id: &entry.id,
            }),
            &target.model.display_name,
            &target.id,
            &target.model,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                // A target over its own limits is an attempt that never
                // reached an upstream — recorded, as chat records it, so the
                // failover it forced is visible.
                let (index, kind) = begin(telemetry);
                telemetry.record(
                    state,
                    AttemptRecord {
                        index,
                        kind,
                        target_model: target_model.clone(),
                        target_model_id: target.id.clone(),
                        provider_key_id: pk_id.clone(),
                        status: 429,
                        success: false,
                        error_class: "rate_limit_exceeded".to_string(),
                        error_message: e.to_string(),
                        latency_ms: 0,
                        dispatched: false,
                    },
                );
                last_err = Some(e);
                continue;
            }
        };
        let budget = effective_retries(
            &target.model,
            group_retries_of(&entry.value),
            state.default_retries,
            i + 1 < n,
        );
        let timeouts =
            effective_timeouts(&target.model, Some(&entry.value), state.default_timeouts);
        for attempt_idx in 0..=budget.attempts {
            if attempt_idx > 0 {
                let hint = last_err.as_ref().and_then(|e| match e {
                    ProxyError::Bridge(be) => retry_after_hint(be),
                    _ => None,
                });
                tokio::time::sleep(retry_backoff(attempt_idx as u32, hint)).await;
            }
            let (index, kind) = begin(telemetry);
            let attempt_started = std::time::Instant::now();
            let in_flight = state.runtime_status.count_in_flight(&target.id);
            match call(target.clone(), timeouts).await {
                Ok(value) => {
                    let refusal = unsupported(&value);
                    // The route answered its own 501 without contacting the
                    // provider, so it is no evidence of upstream health.
                    let dispatched = refusal.is_none();
                    let latency_ms = crate::attempt::ms_since(attempt_started);
                    // Feed the least_latency EWMA, as chat does: the round
                    // trip for a buffered answer, time to the upstream's
                    // response head for a relayed stream.
                    if dispatched {
                        state.runtime_status.record_latency(&target.id, latency_ms);
                    }
                    let record = AttemptRecord {
                        index,
                        kind,
                        target_model: target_model.clone(),
                        target_model_id: target.id.clone(),
                        provider_key_id: pk_id.clone(),
                        status: if refusal.is_some() { 501 } else { 200 },
                        success: refusal.is_none(),
                        // The class `BridgeError::UnsupportedCapability`
                        // reports (`attempt::routing_error_class`).
                        error_class: if refusal.is_some() {
                            "config".to_string()
                        } else {
                            String::new()
                        },
                        error_message: refusal.unwrap_or_default(),
                        latency_ms,
                        dispatched,
                    };
                    let deferred = if defer_outcome && dispatched {
                        // `Ok` here only means the stream opened. Stage the
                        // attempt and let the caller finish it from the
                        // terminal event, so a stream that dies is not
                        // already counted as a success.
                        telemetry.stage(record);
                        let handle = crate::attempt::DeferredAttempt::default();
                        handle.install(telemetry, index);
                        Some(handle)
                    } else {
                        telemetry.record(state, record);
                        None
                    };
                    return Ok(Dispatched {
                        value,
                        target,
                        attempt_started,
                        member_reservation,
                        in_flight,
                        deferred,
                    });
                }
                Err(e) => {
                    let (retryable, budget_covers) = match &e {
                        ProxyError::Bridge(be) => {
                            let transient = is_retryable(be, retry_on_429, fallback_statuses);
                            // Failure attribution is per TARGET: a group whose
                            // one bad member goes down keeps serving from the
                            // rest, which is the whole point of the group.
                            if transient {
                                state.health.record_failure(&target.model.display_name);
                                state.runtime_status.penalize_latency(&target.id);
                            }
                            (transient && permit(be), budget.covers(be))
                        }
                        _ => (false, true),
                    };
                    if routing.is_some() || retryable {
                        log_attempt_failure(
                            &client.request_id,
                            &target.model.display_name,
                            attempt_idx + 1,
                            &e,
                            retryable,
                            fallback_statuses,
                        );
                    }
                    let (error_class, error_message) = crate::attempt::attempt_error_from_proxy(&e);
                    telemetry.record(
                        state,
                        AttemptRecord {
                            index,
                            kind,
                            target_model: target_model.clone(),
                            target_model_id: target.id.clone(),
                            provider_key_id: pk_id.clone(),
                            status: e.status().as_u16(),
                            success: false,
                            error_class,
                            error_message,
                            latency_ms: crate::attempt::ms_since(attempt_started),
                            dispatched: crate::attempt::attempt_reached_upstream(&e),
                        },
                    );
                    if !retryable {
                        if target_unusable(&e) {
                            last_err = Some(e);
                            break;
                        }
                        return Err(e);
                    }
                    last_err = Some(e);
                    if !budget_covers {
                        break;
                    }
                }
            }
        }
    }
    let err = last_err.unwrap_or(ProxyError::ProviderUnavailable);
    Err(if routing.is_some() {
        group_refusal(err, requested_name)
    } else {
        err
    })
}

/// Balancing state is keyed per (virtual model, tier priority). Priority is
/// part of the identity — the APISIX convention: tiers hold disjoint target
/// sets and must not share rotation state.
type TierKey = (String, i32);

/// Suffix of the state key a route serving only part of a group rotates
/// under ([`RoutingRequest::serves`]). A NUL cannot appear in a model name.
const PARTIAL_ROUTE_ROTATION: &str = "\0partial";

/// Smooth weighted round-robin state for one (virtual model, tier).
struct WrrState {
    /// Fingerprint of the group's whole configured target list. A config
    /// change restarts every rotation.
    fingerprint: u64,
    /// One rotation per request-filtered subset of the tier, by that subset's
    /// fingerprint. The subset differs per caller (tags, client IP) and with
    /// each target's health, so a single rotation would have to restart at
    /// every change: interleaved callers would wipe each other's counters on
    /// each request and every one would land on its subset's first target.
    /// Bounded by the subsets that actually occur, and cleared with the config.
    rotations: std::collections::HashMap<u64, Vec<i64>>,
}

const RING_POINTS_PER_UNIT: usize = 160;
/// Weights are reduced (gcd, then proportional scaling) to at most this many
/// total units before being multiplied by [`RING_POINTS_PER_UNIT`], bounding
/// ring memory per tier regardless of the configured weight magnitudes.
const RING_MAX_UNITS: u64 = 64;

/// How a tier's weights become ring units: divided by their gcd, then scaled
/// down when they add up to more than [`RING_MAX_UNITS`]. Taken from the
/// tier as configured, never from the subset one request can use, so a
/// target's point count depends on its own weight alone. Derived from the
/// subset, removing one target (a cooldown, a caller's tags) could change
/// the gcd or the sum, and so every other target's points, and keys would
/// move between targets that are both still up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RingScale {
    gcd: u64,
    sum: u64,
}

impl RingScale {
    fn of<'a>(configured_tier: impl Iterator<Item = &'a RoutingTarget>) -> Self {
        fn gcd(mut a: u64, mut b: u64) -> u64 {
            while b != 0 {
                (a, b) = (b, a % b);
            }
            a
        }
        let weights: Vec<u64> = configured_tier
            .map(|t| u64::from(t.weight_or_default().max(1)))
            .collect();
        let gcd = weights.iter().fold(0, |acc, w| gcd(acc, *w)).max(1);
        Self {
            gcd,
            sum: weights.iter().map(|w| w / gcd).sum(),
        }
    }

    /// The ring units for one target's weight.
    fn units(self, weight: u32) -> u64 {
        let units = (u64::from(weight.max(1)) / self.gcd).max(1);
        if self.sum > RING_MAX_UNITS {
            ((units * RING_MAX_UNITS) / self.sum).max(1)
        } else {
            units
        }
    }
}

/// One tier's ketama-style consistent-hash ring. Every target owns
/// `160 × weight-units` pseudo-random points on the u64 hash circle; a key
/// maps to the first point at or after its own hash. Removing a target
/// (health filtering) moves only that target's keys — each lands on its
/// ring successor — and every other key keeps its mapping, because the
/// units come from the configured tier ([`RingScale`]).
struct HashRing {
    fingerprint: u64,
    /// (hash point, index into the tier's target list), sorted by point.
    points: Vec<(u64, u32)>,
}

impl HashRing {
    fn build(fingerprint: u64, targets: &[RoutingTarget], scale: RingScale) -> Self {
        // A 0 only gets here when every candidate is at 0 (see the weight
        // filter in `resolve_attempt_models`): they have no share to compare,
        // so each gets one unit rather than the ring going empty.
        let units: Vec<u64> = targets
            .iter()
            .map(|t| scale.units(t.weight_or_default()))
            .collect();
        let total_points = units.iter().sum::<u64>() as usize * RING_POINTS_PER_UNIT;
        let mut points = Vec::with_capacity(total_points);
        for (idx, (target, units)) in targets.iter().zip(&units).enumerate() {
            for i in 0..(*units as usize) * RING_POINTS_PER_UNIT {
                let mut h = fnv1a_extend(FNV_OFFSET_BASIS, target.model.as_bytes());
                h = fnv1a_extend(h, &[0]);
                h = fnv1a_extend(h, &(i as u32).to_le_bytes());
                points.push((mix64(h), idx as u32));
            }
        }
        points.sort_unstable();
        Self {
            fingerprint,
            points,
        }
    }

    /// The tier's targets in this key's deterministic preference order:
    /// the key's own point first, then successive ring positions. This
    /// doubles as the failover order within the tier.
    fn preference_order(&self, key_hash: u64, n_targets: usize) -> Vec<u32> {
        let mut order = Vec::with_capacity(n_targets);
        let mut seen = vec![false; n_targets];
        if !self.points.is_empty() {
            let start = self.points.partition_point(|(p, _)| *p < key_hash);
            for off in 0..self.points.len() {
                let (_, idx) = self.points[(start + off) % self.points.len()];
                if !seen[idx as usize] {
                    seen[idx as usize] = true;
                    order.push(idx);
                    if order.len() == n_targets {
                        return order;
                    }
                }
            }
        }
        // Degenerate rings (no points) still yield every target.
        for (i, present) in seen.iter().enumerate() {
            if !present {
                order.push(i as u32);
            }
        }
        order
    }
}

#[derive(Default)]
pub struct RoutingRegistry {
    wrr: DashMap<TierKey, Mutex<WrrState>>,
    rings: DashMap<TierKey, std::sync::Arc<HashRing>>,
}

impl RoutingRegistry {
    /// Drop balancing state for routing models the snapshot no longer
    /// carries. Keyed by configuration, so nothing else reclaims it.
    pub fn retain_configured(&self, is_configured: &dyn Fn(&str) -> bool) {
        let configured =
            |name: &str| is_configured(name.strip_suffix(PARTIAL_ROUTE_ROTATION).unwrap_or(name));
        self.wrr.retain(|(name, _), _| configured(name));
        self.rings.retain(|(name, _), _| configured(name));
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Pick the target order for one request. The first element is the
    /// initial target; subsequent elements are later fallback targets.
    /// Targets partition into priority tiers (higher value first); the
    /// strategy orders each tier independently and tiers concatenate, so a
    /// lower tier is reached only after every higher-tier target failed or
    /// was filtered. Length is bounded by the initial target plus
    /// `routing.max_fallbacks_or_default()`.
    /// `targets` is the request-eligible subset (tag filter, then client-IP,
    /// budget, weight and health filters), passed alongside `routing` rather than spliced
    /// into a clone of it: the strategy knobs come from `routing`, the
    /// candidate list from here, and rebuilding a whole `Routing` just to
    /// swap one field copied its own target vector only to drop it.
    pub fn pick_targets(
        &self,
        virtual_name: &str,
        routing: &Routing,
        targets: &[RoutingTarget],
        hash_key: &str,
    ) -> Vec<String> {
        if targets.is_empty() {
            return Vec::new();
        }
        // Metric-ordered strategies (least_cost, …) can't be ranked here:
        // the ranking key lives on the resolved target Models / runtime
        // state, which `resolve_attempt_models` has and this does not. Hand
        // back the full declaration-order list; tier-aware ranking and
        // `max_fallbacks` truncation happen there instead.
        if routing.strategy.is_metric_based() {
            return targets.iter().map(|t| t.model.clone()).collect();
        }
        let mut order = Vec::with_capacity(targets.len());
        let config_fingerprint = tier_fingerprint(&routing.targets);
        for tier in partition_by_priority(targets) {
            let priority = tier[0].priority_or_default();
            match routing.strategy {
                RoutingStrategy::Failover => {
                    order.extend(tier.iter().map(|t| t.model.clone()));
                }
                RoutingStrategy::RoundRobin => {
                    let start = self.wrr_pick(virtual_name, priority, config_fingerprint, &tier);
                    order.extend(attempt_order(&tier, start, tier.len()));
                }
                RoutingStrategy::ConsistentHash => {
                    let scale = RingScale::of(
                        routing
                            .targets
                            .iter()
                            .filter(|t| t.priority_or_default() == priority),
                    );
                    let ring = self.ring_for(virtual_name, priority, &tier, scale);
                    for idx in ring.preference_order(stable_hash(hash_key), tier.len()) {
                        order.push(tier[idx as usize].model.clone());
                    }
                }
                RoutingStrategy::LeastCost
                | RoutingStrategy::LeastLatency
                | RoutingStrategy::LeastBusy => {
                    unreachable!("metric strategies short-circuit above")
                }
            }
        }
        order.truncate(routing.max_fallbacks_or_default() + 1);
        order
    }

    /// Smooth weighted round-robin (the nginx algorithm): every pick adds
    /// each target's weight to its running counter, takes the max, and
    /// subtracts the weight total from the winner. Proportional AND
    /// interleaved; equal weights degrade to a declaration-order cycle.
    fn wrr_pick(
        &self,
        virtual_name: &str,
        priority: i32,
        config_fingerprint: u64,
        tier: &[RoutingTarget],
    ) -> usize {
        let weights: Vec<i64> = tier
            .iter()
            .map(|t| i64::from(t.weight_or_default().max(1)))
            .collect();
        let total: i64 = weights.iter().sum();
        let entry = self
            .wrr
            .entry((virtual_name.to_string(), priority))
            .or_insert_with(|| {
                Mutex::new(WrrState {
                    fingerprint: config_fingerprint,
                    rotations: std::collections::HashMap::new(),
                })
            });
        let mut state = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.fingerprint != config_fingerprint {
            state.fingerprint = config_fingerprint;
            state.rotations.clear();
        }
        let current = state
            .rotations
            .entry(tier_fingerprint(tier))
            .or_insert_with(|| vec![0; weights.len()]);
        let mut best = 0;
        for (i, w) in weights.iter().enumerate() {
            current[i] += w;
            if current[i] > current[best] {
                best = i;
            }
        }
        current[best] -= total;
        best
    }

    /// The cached ring for one (virtual model, tier), rebuilt when the
    /// tier's (model, weight) fingerprint changes — a config edit, or a
    /// different tag/IP-filtered subset. One entry per key: alternating
    /// subsets rebuild rather than accumulate, keeping the map bounded by
    /// the number of configured (group, tier) pairs.
    fn ring_for(
        &self,
        virtual_name: &str,
        priority: i32,
        tier: &[RoutingTarget],
        scale: RingScale,
    ) -> std::sync::Arc<HashRing> {
        let fingerprint = fnv1a_extend(
            fnv1a_extend(tier_fingerprint(tier), &scale.gcd.to_le_bytes()),
            &scale.sum.to_le_bytes(),
        );
        let key = (virtual_name.to_string(), priority);
        if let Some(ring) = self.rings.get(&key) {
            if ring.fingerprint == fingerprint {
                return ring.clone();
            }
        }
        let ring = std::sync::Arc::new(HashRing::build(fingerprint, tier, scale));
        self.rings.insert(key, ring.clone());
        ring
    }
}

impl std::fmt::Debug for RoutingRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingRegistry")
            .field("wrr_tiers", &self.wrr.len())
            .field("rings", &self.rings.len())
            .finish()
    }
}

/// Split targets into priority tiers, highest priority first, declaration
/// order preserved within each tier. Every target defaults to priority 0,
/// so an unconfigured group is a single tier and this is a no-op shape.
fn partition_by_priority(targets: &[RoutingTarget]) -> Vec<Vec<RoutingTarget>> {
    let mut priorities: Vec<i32> = targets.iter().map(|t| t.priority_or_default()).collect();
    priorities.sort_unstable_by(|a, b| b.cmp(a));
    priorities.dedup();
    priorities
        .into_iter()
        .map(|p| {
            targets
                .iter()
                .filter(|t| t.priority_or_default() == p)
                .cloned()
                .collect()
        })
        .collect()
}

/// Fingerprint of a target list's identity for balancing-state reuse: the
/// ordered (model, weight) pairs. Priorities are already part of the state
/// key and tags do not affect selection within a surviving subset. A target
/// named by id carries that id, since its `model` is empty until it is
/// normalized.
fn tier_fingerprint(tier: &[RoutingTarget]) -> u64 {
    let mut h = FNV_OFFSET_BASIS;
    for t in tier {
        h = fnv1a_extend(h, t.model.as_bytes());
        h = fnv1a_extend(h, &[0]);
        h = fnv1a_extend(h, t.model_id.as_deref().unwrap_or("").as_bytes());
        h = fnv1a_extend(h, &[0]);
        h = fnv1a_extend(h, &t.weight_or_default().to_le_bytes());
        h = fnv1a_extend(h, &[1]);
    }
    h
}

/// Narrow a routing model's targets to those eligible for this request's
/// routing tags, mirroring a mainstream gateway's tag-based routing:
///   * No target is tagged → tag routing isn't in use; every target eligible.
///   * Request carries tags → targets whose tags intersect it (match-any); if
///     none match, fall back to `"default"`-tagged targets.
///   * Request has no tags → `"default"`-tagged targets if any, else all.
///
/// Borrows whenever nothing is filtered out — the default, since tag routing
/// is opt-in — and only allocates for the subset when it actually narrows.
/// An empty result means the request asked for a tag tier with no matching
/// target and no default; the caller turns that into an error.
fn eligible_targets<'a>(
    targets: &'a [RoutingTarget],
    request_tags: &[String],
) -> std::borrow::Cow<'a, [RoutingTarget]> {
    use std::borrow::Cow;
    if !targets.iter().any(RoutingTarget::has_tags) {
        return Cow::Borrowed(targets);
    }
    let defaults = || -> Vec<RoutingTarget> {
        targets
            .iter()
            .filter(|t| t.is_default_target())
            .cloned()
            .collect()
    };
    if request_tags.is_empty() {
        let d = defaults();
        return if d.is_empty() {
            Cow::Borrowed(targets)
        } else {
            Cow::Owned(d)
        };
    }
    let matched: Vec<RoutingTarget> = targets
        .iter()
        .filter(|t| t.matches_request_tags(request_tags))
        .cloned()
        .collect();
    Cow::Owned(if matched.is_empty() {
        defaults()
    } else {
        matched
    })
}

/// Build the target-order vector starting at `start_idx`, walking forward
/// (wrap-around) for `limit` distinct entries.
fn attempt_order(targets: &[RoutingTarget], start_idx: usize, limit: usize) -> Vec<String> {
    let n = targets.len();
    let mut order = Vec::with_capacity(limit);
    for i in 0..limit {
        let t = &targets[(start_idx + i) % n];
        order.push(t.model.clone());
    }
    order
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Fold `bytes` into a running 64-bit FNV-1a hash. Deterministic across
/// processes, replicas, and toolchains by design (the std hasher is not) —
/// every consistent-hash artifact (ring points, key hashes, fingerprints)
/// must agree everywhere, and MUST NOT change across DP versions: changing
/// this function remaps every session's target.
fn fnv1a_extend(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// The splitmix64 finalizer. FNV-1a alone has weak avalanche on short,
/// structured inputs — ring points hashed from `name\0index` cluster into
/// narrow bands, handing one target most of the circle (observed: 32
/// distinct keys all mapping to one of two equal-weight targets). Ketama
/// implementations use MD5/CRC32 for exactly this reason; a strong final
/// mix restores uniform dispersion while keeping the pipeline dependency-
/// free and deterministic. Same stability contract as `fnv1a_extend`.
fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x
}

/// Stable, well-dispersed 64-bit hash of a consistent-hash key.
fn stable_hash(s: &str) -> u64 {
    mix64(fnv1a_extend(FNV_OFFSET_BASIS, s.as_bytes()))
}

/// Resolve the `consistent_hash` key for one request by walking the
/// routing model's `hash_on` chain (default: the `x-aisix-routing-key`
/// header, then the caller's API key id). The first source yielding a
/// non-empty value wins; when nothing yields, the empty string keeps the
/// pick deterministic rather than random.
fn resolve_hash_key(routing: &Routing, req: &RoutingRequest<'_>) -> String {
    for source in routing.hash_on_or_default() {
        let value = match source.source_type {
            HashOnType::Header => source.name.as_deref().and_then(|name| {
                req.headers
                    .and_then(|h| h.get(name))
                    .and_then(|v| v.to_str().ok())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            }),
            HashOnType::Cookie => source
                .name
                .as_deref()
                .and_then(|name| req.headers.and_then(|h| cookie_value(h, name))),
            HashOnType::ApiKey => Some(req.api_key_id.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            HashOnType::ClientIp => Some(req.source_ip.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        };
        if let Some(value) = value {
            return value;
        }
    }
    String::new()
}

/// Extract a cookie's value from the request's `Cookie` header(s):
/// `name=value` pairs separated by `;`, first match wins. Values are taken
/// verbatim (no unquoting) — the key only needs to be stable, not parsed.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for header in headers.get_all(axum::http::header::COOKIE) {
        let Ok(raw) = header.to_str() else { continue };
        for pair in raw.split(';') {
            let mut it = pair.splitn(2, '=');
            let key = it.next().unwrap_or("").trim();
            if key == name {
                let value = it.next().unwrap_or("").trim();
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}

/// Combined per-1K unit price used to rank `least_cost` targets. A target
/// Model with no price at all — no `pricing_key` that resolves, and no
/// inline `cost` — sorts last (treated as +∞) so a misconfigured target
/// is deprioritised rather than silently preferred.
fn cost_key(pricing: &PricingIndex, model: &Model) -> f64 {
    pricing
        .resolve(model)
        .map(|c| c.input_per_1k + c.output_per_1k)
        .unwrap_or(f64::INFINITY)
}

/// Observed-latency key used to rank `least_latency` targets. A target with
/// no latency samples yet sorts first (treated as −∞) so it gets probed;
/// once it has an EWMA it ranks by that. A target that just failed sorts
/// last until its penalty lapses, whatever it had before.
fn latency_key(runtime_status: &crate::ModelRuntimeStatusTracker, id: &str) -> f64 {
    if runtime_status.latency_penalized(id) {
        return f64::INFINITY;
    }
    runtime_status
        .latency_ewma_ms(id)
        .unwrap_or(f64::NEG_INFINITY)
}

/// Rank the resolved attempt list by the strategy's runtime metric,
/// best-first (ascending). Stable, so equal-metric targets keep their
/// declaration order. Only metric-based strategies reach here; positional
/// strategies are ordered in [`RoutingRegistry::pick_targets`].
fn order_attempts_by_metric(
    strategy: RoutingStrategy,
    attempts: &mut [AttemptModel],
    runtime_status: &crate::ModelRuntimeStatusTracker,
    snapshot: &AisixSnapshot,
    pricing: &LivePricingIndex,
) {
    match strategy {
        RoutingStrategy::LeastCost => {
            // Built here rather than per comparison: the sort calls the
            // key function O(n log n) times, and the index is shared with
            // whatever else prices this snapshot.
            let pricing = pricing.for_snapshot(snapshot);
            attempts.sort_by(|a, b| {
                cost_key(&pricing, &a.model).total_cmp(&cost_key(&pricing, &b.model))
            });
        }
        RoutingStrategy::LeastLatency => {
            attempts.sort_by(|a, b| {
                latency_key(runtime_status, &a.id).total_cmp(&latency_key(runtime_status, &b.id))
            });
        }
        RoutingStrategy::LeastBusy => {
            // The APISIX least_conn score: in-flight scaled by 1/weight, so a
            // heavier target absorbs proportionally more concurrency. `+1`
            // keeps idle targets ranked by weight instead of tying at 0.
            let score = |a: &AttemptModel| {
                (runtime_status.in_flight(&a.id) as f64 + 1.0) / f64::from(a.weight.max(1))
            };
            attempts.sort_by(|a, b| score(a).total_cmp(&score(b)));
        }
        RoutingStrategy::Failover
        | RoutingStrategy::RoundRobin
        | RoutingStrategy::ConsistentHash => {}
    }
}

/// One concrete (non-routing) Model the dispatch loop will attempt, paired
/// with its snapshot id so health/cooldown tracking can key on it, and the
/// routing-target attributes selection still needs after resolution
/// (priority for tier-aware metric ranking, weight for `least_busy`).
#[derive(Clone)]
pub(crate) struct AttemptModel {
    pub id: String,
    /// Shared with the snapshot row rather than deep-copied: the dispatch
    /// only reads it, and a routing group used to clone the whole `Model`
    /// once per target just to hand it to the bridge.
    pub model: std::sync::Arc<Model>,
    pub priority: i32,
    pub weight: u32,
}

/// A candidate the health/cooldown filter dropped, kept so the caller can
/// say WHICH target went and WHY.
///
/// Without it the exclusion is invisible: the request records one attempt
/// against the surviving target and nothing anywhere names the one that was
/// never tried, so "the group failed over to nobody" and "the group only
/// ever had one candidate" produce the identical trace
/// (AISIX-Cloud#1499).
pub(crate) struct ExcludedCandidate {
    /// Snapshot id of the dropped target — the key health/cooldown state
    /// is tracked under.
    pub id: String,
    /// The target's configured `display_name`, i.e. the name the operator
    /// wrote in `routing.targets`.
    pub model: String,
    pub reason: &'static str,
}

/// `reason` on an [`ExcludedCandidate`] dropped for an unexpired
/// request-path cooldown.
pub(crate) const EXCLUDED_COOLING: &str = "cooling";
/// `reason` on an [`ExcludedCandidate`] dropped because its background
/// health check has it marked down.
pub(crate) const EXCLUDED_UNHEALTHY: &str = "unhealthy";
/// `reason` on an [`ExcludedCandidate`] whose name resolves to no Model in
/// this snapshot: its row was deleted while the group still names it.
pub(crate) const EXCLUDED_MISSING: &str = "missing";

/// Outcome of routing-candidate filtering. Lifts the "all candidates
/// excluded" case out into a typed result so the dispatch loop can
/// short-circuit to a 503 + Retry-After instead of sending traffic to
/// a target we just confirmed is bad.
pub(crate) enum FilterOutcome {
    /// At least one candidate survived the filter. `attempts` is the
    /// filtered list, in the original strategy order minus the excluded
    /// entries; `excluded` names what was dropped to get there.
    Selected {
        attempts: Vec<AttemptModel>,
        excluded: Vec<ExcludedCandidate>,
    },
    /// Every candidate is currently background-unhealthy and the
    /// routing model is configured with `when_all_unavailable: fail`. The
    /// caller should surface a 503 with the supplied Retry-After hint
    /// (in seconds), if any.
    AllUnhealthy {
        retry_after_secs: Option<u64>,
        excluded: Vec<ExcludedCandidate>,
    },
}

fn excluded(attempts: &[AttemptModel], reason: &'static str) -> Vec<ExcludedCandidate> {
    attempts
        .iter()
        .map(|a| ExcludedCandidate {
            id: a.id.clone(),
            model: a.model.display_name.clone(),
            reason,
        })
        .collect()
}

pub(crate) fn filter_attempt_models(
    runtime_status: &crate::ModelRuntimeStatusTracker,
    attempts: Vec<AttemptModel>,
    policy: WhenAllUnavailablePolicy,
) -> FilterOutcome {
    let mut healthy = Vec::new();
    let mut cooldown_only = Vec::new();
    let mut unhealthy = Vec::new();

    for attempt in attempts.iter().cloned() {
        let stale_after = attempt
            .model
            .background_model_check
            .as_ref()
            .map(|cfg| Duration::from_secs(cfg.stale_after_seconds));
        let snapshot = runtime_status.status_with_stale(&attempt.id, stale_after);
        match snapshot.status {
            crate::RuntimeStatus::Unhealthy => unhealthy.push(attempt),
            crate::RuntimeStatus::Cooldown => cooldown_only.push(attempt),
            crate::RuntimeStatus::Healthy | crate::RuntimeStatus::NotApplicable => {
                healthy.push(attempt)
            }
        }
    }

    if !healthy.is_empty() {
        let mut dropped = excluded(&cooldown_only, EXCLUDED_COOLING);
        dropped.extend(excluded(&unhealthy, EXCLUDED_UNHEALTHY));
        return FilterOutcome::Selected {
            attempts: healthy,
            excluded: dropped,
        };
    }
    // No healthy candidates — prefer cooldown over unhealthy when
    // some non-unhealthy candidates exist. Sending to a target whose
    // cooldown timer hasn't expired is still better than sending to
    // a target that an active probe just confirmed is broken.
    //
    // Reuse the single status read from the classification loop above:
    // with `healthy` empty here, the non-unhealthy candidates are
    // exactly the `cooldown_only` ones. Re-reading runtime_status to
    // re-filter would add a redundant per-candidate query and open a
    // race window — a candidate flipping to unhealthy between the two
    // reads could yield an empty `Selected`, which streaming callers
    // turn into a panic by indexing `attempt_models[0]`.
    if !cooldown_only.is_empty() {
        return FilterOutcome::Selected {
            excluded: excluded(&unhealthy, EXCLUDED_UNHEALTHY),
            attempts: cooldown_only,
        };
    }
    // All candidates are excluded. Policy decides.
    //
    // Retry-After for the fail path is a coarse fallback (30s by
    // default — see FALLBACK_ALL_UNHEALTHY_RETRY_AFTER). We could
    // try to derive it from per-candidate cooldown timers, but the
    // categorisation above routes cooldown candidates into
    // `cooldown_only` (returned via the Selected branch above), so
    // by construction every candidate that reaches here is in the
    // background-unhealthy state and has no cooldown timer to read.
    match policy {
        WhenAllUnavailablePolicy::Fail => FilterOutcome::AllUnhealthy {
            retry_after_secs: Some(FALLBACK_ALL_UNHEALTHY_RETRY_AFTER.as_secs()),
            excluded: excluded(&unhealthy, EXCLUDED_UNHEALTHY),
        },
        // `try_anyway` dispatches the unfiltered list, so nothing was
        // dropped and there is no exclusion to report.
        WhenAllUnavailablePolicy::TryAnyway => FilterOutcome::Selected {
            attempts,
            excluded: Vec::new(),
        },
    }
}

/// Per-request routing inputs threaded into [`resolve_attempt_models`]: the
/// tags that gate tag/metadata routing, the raw material for the
/// `consistent_hash` hash key (the inbound headers plus the caller's API key
/// id — the configured `hash_on` chain is evaluated against them at
/// resolution time), and the caller's resolved source IP, used both for the
/// per-target client-IP allowlist and as the `client_ip` hash source.
///
/// `source_ip` defaults to the empty string, which
/// [`aisix_core::Model::ip_allowed`] treats as "not in range" — so a caller
/// that forgets to thread it fails closed on restricted targets rather than
/// silently disabling the allowlist.
#[derive(Clone, Copy, Default)]
pub(crate) struct RoutingRequest<'a> {
    pub tags: &'a [String],
    pub headers: Option<&'a HeaderMap>,
    pub api_key_id: &'a str,
    pub source_ip: &'a str,
    /// Model id of the target that produced the context a `/v1/responses`
    /// follow-up carries ([`crate::responses_affinity`]). When set, that
    /// target and its provider-key peers are attempted first, ahead of the
    /// strategy's order. `None` everywhere else.
    pub context_origin: Option<&'a str>,
    /// The request's budget gate. A target whose provider key's budget is
    /// exhausted is not a candidate, and a direct model on one is refused.
    /// `None` only in tests that exercise no budget.
    pub budget: Option<&'a crate::budget::BudgetGate>,
    /// Set by a route that only some of a group's targets can answer. The
    /// others are not candidates, so they cannot use up a `max_fallbacks`
    /// slot, and a group with none left resolves to an empty list for the
    /// route to refuse in its own words. Such a route also rotates on its
    /// own, because its traffic says nothing about the group's other routes:
    /// a client alternating it with those would otherwise always land them
    /// on the same target.
    pub serves: Option<&'a dyn Fn(&Model) -> bool>,
    /// The caller's API key's `allowed_provider_groups`. When non-empty, a
    /// target whose provider key is in none of these groups is not a
    /// candidate, and a direct model outside them is refused. Empty — the
    /// default — means unconstrained. See [`target_in_groups`].
    ///
    /// Every production call site sets this field explicitly, never through
    /// `..Default::default()`: the default is unconstrained, so a site that
    /// leaves it out silently lifts the key's restriction.
    pub allowed_groups: &'a [String],
}

/// Whether `model` may serve a key restricted to the provider groups in
/// `allowed`: always when `allowed` is empty, otherwise only when the
/// provider key the model dispatches with lists at least one of them. A
/// model whose provider key cannot be found belongs to no group.
///
/// Keys on the DISPATCHED target, never the requested entry: a routing or
/// semantic model has no provider key of its own, so each of its targets is
/// judged by its own provider key (`resolve_attempt_models`,
/// `semantic::select_eligible`); a direct or embedding model and a wildcard
/// alias are judged by the provider key of the row that serves them; an
/// ensemble never gets this far for a grouped key
/// ([`crate::model_resolve::refuse_ensemble_to_grouped_key`]).
///
/// The paths that pick a provider key outside this module call it
/// themselves: `/v1/realtime`, the files/batches/fine-tuning target
/// (`jobs::resolve_target`, named and default pick alike), and the video
/// polls; an `inject` passthrough route holds a provider key rather than a
/// model and calls [`provider_key_in_groups`]. Each of them refuses with
/// [`no_target_in_groups`], except the video polls, which answer as for a
/// video that does not exist.
pub(crate) fn target_in_groups(
    snapshot: &AisixSnapshot,
    model: &Model,
    allowed: &[String],
) -> bool {
    if allowed.is_empty() {
        return true;
    }
    model
        .provider_key_id
        .as_deref()
        .and_then(|id| snapshot.provider_keys.get_by_id(id))
        .is_some_and(|pk| provider_key_in_groups(&pk.value, allowed))
}

/// Whether a request restricted to the provider groups in `allowed` may be
/// sent with `pk`: always when `allowed` is empty, otherwise only when `pk`
/// lists at least one of them. [`target_in_groups`] for a path that holds a
/// provider key rather than a model (a passthrough route that injects one).
pub(crate) fn provider_key_in_groups(pk: &aisix_core::ProviderKey, allowed: &[String]) -> bool {
    allowed.is_empty() || pk.groups.iter().any(|g| allowed.contains(g))
}

/// Keep the targets [`target_in_groups`] admits. Runs ahead of every other
/// filter, the routing-tag one included, so a request header can only narrow
/// what the key's groups allow and a `default`-tagged target outside them is
/// no fallback. A target whose reference resolves to no model cannot be
/// shown to be in a group, so a grouped key drops it here. Borrows when
/// `allowed` is empty — every key without groups.
fn targets_in_groups<'a>(
    snapshot: &AisixSnapshot,
    targets: &'a [RoutingTarget],
    allowed: &[String],
) -> std::borrow::Cow<'a, [RoutingTarget]> {
    if allowed.is_empty() {
        return std::borrow::Cow::Borrowed(targets);
    }
    std::borrow::Cow::Owned(
        targets
            .iter()
            .filter(|t| {
                snapshot
                    .models
                    .get_by_name(&t.model_ref(snapshot))
                    .is_some_and(|entry| target_in_groups(snapshot, &entry.value, allowed))
            })
            .cloned()
            .collect(),
    )
}

/// What a grouped key is told when nothing inside its groups is left: the
/// answer every upstream being unavailable gets, naming no group, tag or
/// target, so the refusal cannot be used to learn how a model is grouped.
/// The one refusal every provider-group gate returns; `addressed` is what the
/// caller named (a model, a router, a passthrough route) and reaches only the
/// debug log.
pub(crate) fn no_target_in_groups(addressed: &str) -> ProxyError {
    tracing::debug!(
        addressed = %addressed,
        "nothing the request addressed is in the API key's provider groups",
    );
    ProxyError::AllCandidatesUnavailable {
        retry_after_secs: None,
    }
}

/// Drop the targets whose own `allowed_cidrs` excludes `source_ip`.
///
/// Deliberately NOT folded into [`filter_attempt_models`]: that filter's
/// `when_all_unavailable: try_anyway` policy hands back the *unfiltered*
/// candidate list, which would send a request to a target the operator just
/// declared off-limits for this caller. An allowlist has no "try anyway".
/// Stays borrowed when no target is excluded — the common case — so the
/// default path through routing copies no target at all.
fn targets_allowed_for_ip<'a>(
    snapshot: &AisixSnapshot,
    targets: std::borrow::Cow<'a, [RoutingTarget]>,
    source_ip: &str,
) -> std::borrow::Cow<'a, [RoutingTarget]> {
    // An unresolvable name is left in place for the resolution below to
    // count as the missing member it is, rather than being silently swallowed
    // here as an IP rejection.
    let allowed = |t: &RoutingTarget| {
        snapshot
            .models
            .get_by_name(&t.model)
            .is_none_or(|entry| entry.value.ip_allowed(source_ip))
    };
    if targets.iter().all(allowed) {
        return targets;
    }
    std::borrow::Cow::Owned(targets.iter().filter(|t| allowed(t)).cloned().collect())
}

/// The caller's routing tags, when they can steer this model to a different
/// target. A cached answer came from whichever target those tags selected, so
/// they belong in the cache key: `eu` and `us` must not share an entry, and
/// neither must two models a `premium` tag chooses between. `None` otherwise,
/// so tags a model ignores cannot fragment its cache.
pub(crate) fn cache_vary_tags(model: &Model, tags: &[String]) -> Option<String> {
    let routing = model.routing.as_ref()?;
    if tags.is_empty() || !routing.targets.iter().any(RoutingTarget::has_tags) {
        return None;
    }
    let mut tags = tags.to_vec();
    tags.sort_unstable();
    tags.dedup();
    Some(tags.join(","))
}

/// Refuse a caller no routing target admits, for an answer that will not reach
/// [`resolve_attempt_models`]: a cache hit dispatches nothing, and an allowlist
/// on a member has to hold for it all the same. Only the IP half of that walk.
/// A target that names nothing admits nobody, since nothing could be
/// dispatched to it; only when every target names nothing is the request left
/// for dispatch to report.
pub(crate) fn check_member_ip_access(
    snapshot: &AisixSnapshot,
    virtual_name: &str,
    model: &Model,
    tags: &[String],
    source_ip: &str,
) -> Result<(), ProxyError> {
    let Some(routing) = model.routing.as_ref() else {
        return Ok(());
    };
    let eligible = eligible_targets(&routing.targets, tags);
    let mut live = eligible
        .iter()
        .filter_map(|t| snapshot.models.get_by_name(&t.model_ref(snapshot)))
        .peekable();
    let admitted = live.peek().is_none() || live.any(|entry| entry.value.ip_allowed(source_ip));
    if admitted {
        Ok(())
    } else {
        Err(ProxyError::ModelIpRestricted(virtual_name.to_string()))
    }
}

/// Drop the targets whose provider key has an exhausted `provider_key`
/// budget (or, in strict mode, any budget it can no longer verify). With
/// every target dropped, the request is refused with the budget's own
/// answer: `budget_exceeded` when any target was exhausted, otherwise
/// `budget_unverified`.
fn targets_within_budget<'a>(
    snapshot: &AisixSnapshot,
    gate: &crate::budget::BudgetGate,
    targets: std::borrow::Cow<'a, [RoutingTarget]>,
) -> Result<std::borrow::Cow<'a, [RoutingTarget]>, ProxyError> {
    // An unresolvable name stays, for the resolution loop to report.
    let verdict = |t: &RoutingTarget| match snapshot.models.get_by_name(&t.model) {
        Some(entry) => gate.check_model(&entry.value),
        None => Ok(()),
    };
    if targets.iter().all(|t| verdict(t).is_ok()) {
        return Ok(targets);
    }
    let mut refusal: Option<ProxyError> = None;
    let kept: Vec<RoutingTarget> = targets
        .iter()
        .filter(|t| match verdict(t) {
            Ok(()) => true,
            Err(err) => {
                tracing::debug!(
                    target_model = %t.model,
                    error = %err,
                    "routing target excluded by its provider key's budget",
                );
                if !matches!(refusal, Some(ProxyError::BudgetExceeded(_))) {
                    refusal = Some(err);
                }
                false
            }
        })
        .cloned()
        .collect();
    match refusal {
        Some(err) if kept.is_empty() => Err(err),
        _ => Ok(std::borrow::Cow::Owned(kept)),
    }
}

/// Resolve the ordered list of concrete Models a request will attempt.
///
/// For a routing model (Model Group), narrow `routing.targets` to the ones
/// this request can use (the API key's provider groups, tags, client IP,
/// budget, weight, whether the name still resolves, health/cooldown), then
/// order what is left per the
/// configured strategy and cap it at `max_fallbacks`. For a direct
/// (non-routing) model, the list is just the model itself, once it has
/// passed the same provider-group and budget checks.
///
/// Shared by `/v1/chat/completions` and `/v1/messages` so both endpoints
/// dispatch Model Groups identically (ai-gateway#471).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_attempt_models(
    routing_registry: &RoutingRegistry,
    runtime_status: &crate::ModelRuntimeStatusTracker,
    pricing: &LivePricingIndex,
    snapshot: &AisixSnapshot,
    virtual_name: &str,
    virtual_id: &str,
    virtual_model: &std::sync::Arc<Model>,
    req: RoutingRequest<'_>,
) -> Result<Vec<AttemptModel>, ProxyError> {
    crate::model_resolve::refuse_ensemble_to_grouped_key(
        virtual_name,
        virtual_model,
        req.allowed_groups,
    )?;
    let Some(routing) = virtual_model.routing.as_ref() else {
        // A semantic parent has no provider key of its own, so the group
        // check would refuse it with the 503 meant for a model outside the
        // groups. Only `/v1/chat/completions` serves semantic routing, and it
        // never gets here (it resolves through `semantic::resolve`, which
        // applies the groups to each candidate); every other endpoint refuses
        // the parent with its own permanent 4xx before anything goes
        // upstream, and keeps that answer for a grouped key too.
        if !virtual_model.is_semantic()
            && !target_in_groups(snapshot, virtual_model, req.allowed_groups)
        {
            return Err(no_target_in_groups(virtual_name));
        }
        if let Some(gate) = req.budget {
            gate.check_model(virtual_model)?;
        }
        return Ok(vec![AttemptModel {
            id: virtual_id.to_string(),
            model: std::sync::Arc::clone(virtual_model),
            priority: 0,
            weight: 1,
        }]);
    };

    // The API key's provider groups are a hard constraint and come first:
    // a target outside them is not a candidate for anything below, so the
    // routing-tag header can only narrow what is left, and neither a
    // `default` target nor a `when_all_unavailable` policy reaches past it.
    let grouped = targets_in_groups(snapshot, &routing.targets, req.allowed_groups);
    if grouped.is_empty() {
        return Err(no_target_in_groups(virtual_name));
    }
    // Tag/metadata pre-filter: narrow the targets to those eligible for this
    // request's routing tags, then let the configured strategy order whatever
    // survives. A no-op when no target is tagged.
    let eligible = eligible_targets(&grouped, req.tags);
    if eligible.is_empty() {
        // A grouped key gets the all-unavailable answer, not the 400 below
        // that lists the tags: what a tag matches outside the key's groups
        // is none of its business.
        if !req.allowed_groups.is_empty() {
            return Err(no_target_in_groups(virtual_name));
        }
        return Err(ProxyError::InvalidRequest(format!(
            "no routing target matches request tags {:?}",
            req.tags
        )));
    }
    // Normalize each target's model reference to the display name the
    // models table is keyed by, before ANY of the machinery below looks one
    // up: the IP pre-filter, the balancing state (WRR fingerprints, hash
    // rings) and the resolution loop all key on `target.model`, so
    // resolving once here is what keeps a `model_id` target from having to
    // be handled at each of them. A target written as `model_id` follows a
    // rename of the model it points at; one whose id resolves to nothing
    // keeps the id as its name and is reported below as the missing target
    // it is — the same outcome a dangling `model` gets.
    //
    // Collapsing to one entry per resolved model is part of the same step,
    // and not an optimisation: the loop below finds a picked name's target
    // with `find`, so two entries resolving to the same model would give
    // the second attempt the FIRST one's weight and priority and spend two
    // `max_fallbacks` slots on one upstream. The write path rejects
    // duplicate targets, but it can only compare the spelling each entry
    // used — `{"model": "beta"}` beside `{"model_id": "<beta>"}` is one
    // model written two ways and reaches this side intact.
    //
    // Stays borrowed when every target already names its model once — the
    // common case — so the default path through routing copies no target.
    let already_normalized = {
        let mut seen = std::collections::HashSet::with_capacity(eligible.len());
        eligible
            .iter()
            .all(|t| t.model_id.is_none() && seen.insert(t.model.as_str()))
    };
    let eligible = if already_normalized {
        eligible
    } else {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        std::borrow::Cow::Owned(
            eligible
                .iter()
                .filter_map(|t| {
                    let model = t.model_ref(snapshot).into_owned();
                    if !seen.insert(model.clone()) {
                        tracing::debug!(
                            virtual_model = %virtual_name,
                            target_model = %model,
                            "routing targets resolve to the same model; keeping the first",
                        );
                        return None;
                    }
                    Some(RoutingTarget {
                        model,
                        model_id: None,
                        ..t.clone()
                    })
                })
                .collect::<Vec<_>>(),
        )
    };
    let eligible = match req.serves {
        Some(serves) => {
            let kept: Vec<RoutingTarget> = eligible
                .iter()
                .filter(|t| {
                    snapshot
                        .models
                        .get_by_name(&t.model)
                        .is_some_and(|entry| serves(&entry.value))
                })
                .cloned()
                .collect();
            if kept.is_empty() {
                return Ok(Vec::new());
            }
            std::borrow::Cow::Owned(kept)
        }
        None => eligible,
    };
    // Client-IP pre-filter (#1087 follow-up): a target whose own
    // `allowed_cidrs` excludes this caller is not a candidate. Applied BEFORE
    // the strategy picks, so `max_fallbacks` budgets attempts across the
    // targets this caller may actually reach, and a metric-based strategy
    // ranks only those. The group's own `allowed_cidrs` is separately enforced
    // pre-dispatch by `dispatch::check_ip_access`; this adds the member tier
    // that a group previously bypassed entirely.
    let eligible = targets_allowed_for_ip(snapshot, eligible, req.source_ip);
    if eligible.is_empty() {
        // Report the name the caller asked for, not the excluded members —
        // matching `ModelForbidden`, and without disclosing group internals.
        return Err(ProxyError::ModelIpRestricted(virtual_name.to_string()));
    }
    // Provider-key budget pre-filter: an upstream credential whose budget is
    // exhausted is not a candidate either, for the same reasons as the IP
    // allowlist — it must not consume a `max_fallbacks` slot, and it has no
    // "try anyway".
    let eligible = match req.budget {
        Some(gate) => targets_within_budget(snapshot, gate, eligible)?,
        None => eligible,
    };

    // Everything still eligible, resolved. A name that resolves to nothing
    // is a member deleted while the group still names it, and it costs the
    // group that member rather than the whole group. The refusal for a group
    // left with none names nobody, for the reason `refused_model_label`
    // gives.
    let mut dropped = Vec::new();
    let mut candidates = Vec::with_capacity(eligible.len());
    for t in eligible.iter() {
        match snapshot.models.get_by_name(&t.model) {
            Some(entry) => candidates.push(AttemptModel {
                id: entry.id.clone(),
                model: entry.value.clone(),
                priority: t.priority_or_default(),
                weight: t.weight_or_default(),
            }),
            None => dropped.push(ExcludedCandidate {
                id: t.model.clone(),
                model: t.model.clone(),
                reason: EXCLUDED_MISSING,
            }),
        }
    }
    if candidates.is_empty() {
        log_candidate_exclusions(runtime_status, virtual_name, 0, &dropped);
        return Err(ProxyError::InvalidRequest(
            "routing model has no target that resolves to a Model".into(),
        ));
    }
    // `weight: 0` takes a target out of rotation on the strategies that read
    // weight, which is how an operator drains one, as long as another
    // candidate has a share. Judged after resolution: a deleted member is no
    // candidate, so it must not drain a standby kept at 0. With every
    // candidate at 0 there is no share to compare, so they all stay and the
    // strategies treat them as equal rather than refuse the request.
    // `failover`, `least_cost` and `least_latency` do not read the field.
    if matches!(
        routing.strategy,
        RoutingStrategy::RoundRobin | RoutingStrategy::ConsistentHash | RoutingStrategy::LeastBusy
    ) && candidates.iter().any(|c| c.weight > 0)
    {
        candidates.retain(|c| c.weight > 0);
    }

    // The health and cooldown filter runs ahead of the strategy, like the
    // client-IP and budget filters above and for the same reason: an
    // unavailable target must not use up a `max_fallbacks` slot, or a healthy
    // target or a whole backup tier goes untried, and its share belongs to
    // the targets that can serve. It also means a cooling target's traffic
    // spreads across the rest instead of landing on its neighbour.
    let attempts = match filter_attempt_models(
        runtime_status,
        candidates,
        routing.when_all_unavailable_or_default(),
    ) {
        FilterOutcome::Selected { attempts, excluded } => {
            dropped.extend(excluded);
            attempts
        }
        FilterOutcome::AllUnhealthy {
            retry_after_secs,
            excluded,
        } => {
            dropped.extend(excluded);
            log_candidate_exclusions(runtime_status, virtual_name, 0, &dropped);
            tracing::warn!(
                virtual_model = %virtual_name,
                retry_after_secs,
                "all routing candidates are unavailable; failing fast",
            );
            return Err(ProxyError::AllCandidatesUnavailable { retry_after_secs });
        }
    };
    let available = if attempts.len() == eligible.len() {
        eligible
    } else {
        std::borrow::Cow::Owned(
            eligible
                .iter()
                .filter(|t| attempts.iter().any(|a| a.model.display_name == t.model))
                .cloned()
                .collect::<Vec<_>>(),
        )
    };

    let hash_key = if routing.strategy == RoutingStrategy::ConsistentHash {
        resolve_hash_key(routing, &req)
    } else {
        String::new()
    };
    let rotation = match req.serves {
        Some(_) => std::borrow::Cow::Owned(format!("{virtual_name}{PARTIAL_ROUTE_ROTATION}")),
        None => std::borrow::Cow::Borrowed(virtual_name),
    };
    let names = routing_registry.pick_targets(&rotation, routing, &available, &hash_key);
    if names.is_empty() {
        return Err(ProxyError::InvalidRequest(
            "routing model has no targets".into(),
        ));
    }
    let mut resolved: Vec<AttemptModel> = names
        .iter()
        .filter_map(|name| {
            attempts
                .iter()
                .find(|a| a.model.display_name == *name)
                .cloned()
        })
        .collect();
    // Metric-ordered strategies get the full target set from `pick_targets`;
    // rank it best-first here (target Models are now resolved) and cap it to
    // the same attempt budget the positional strategies apply upstream. The
    // metric sort runs first, then a stable sort on priority — so tiers
    // concatenate highest-first with the metric order preserved inside each.
    if routing.strategy.is_metric_based() {
        order_attempts_by_metric(
            routing.strategy,
            &mut resolved,
            runtime_status,
            snapshot,
            pricing,
        );
        resolved.sort_by_key(|a| std::cmp::Reverse(a.priority));
    }
    if let Some(origin) = req.context_origin {
        resolved = pin_context_origin(snapshot, runtime_status, &available, resolved, origin);
        tracing::debug!(
            virtual_model = %virtual_name,
            origin,
            first = resolved.first().map(|a| a.model.display_name.as_str()),
            "responses context affinity reordered routing candidates",
        );
    }
    resolved.truncate(routing.max_fallbacks_or_default() + 1);
    log_candidate_exclusions(runtime_status, virtual_name, resolved.len(), &dropped);
    Ok(resolved)
}

/// Put the targets that can read a conversation's context first: the origin
/// target, then the other targets on its provider key, then the rest in the
/// strategy's order. Targets the strategy left out of `resolved` (its
/// `max_fallbacks` cut) are brought back behind the ones it ordered, since
/// the pinned order can push those out and the cut is applied again after
/// this. Only targets that survived the filters are considered — `targets`
/// is already that set. When every target left is cooling, a boundary member
/// goes behind the others, so it cannot spend the `max_fallbacks` budget
/// ahead of one that can serve whatever it is sent.
fn pin_context_origin(
    snapshot: &AisixSnapshot,
    runtime_status: &crate::ModelRuntimeStatusTracker,
    targets: &[RoutingTarget],
    resolved: Vec<AttemptModel>,
    origin: &str,
) -> Vec<AttemptModel> {
    let boundary = crate::responses_affinity::Boundary::of(snapshot, origin);
    let mut candidates = resolved;
    for t in targets {
        let Some(entry) = snapshot.models.get_by_name(&t.model) else {
            continue;
        };
        if candidates.iter().any(|a| a.id == entry.id) {
            continue;
        }
        candidates.push(AttemptModel {
            id: entry.id.clone(),
            model: entry.value.clone(),
            priority: t.priority_or_default(),
            weight: t.weight_or_default(),
        });
    }
    let (mut inside, outside): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|a| boundary.contains(&a.id, a.model.provider_key_id.as_deref()));
    let unavailable = |a: &AttemptModel| {
        let stale_after = a
            .model
            .background_model_check
            .as_ref()
            .map(|cfg| Duration::from_secs(cfg.stale_after_seconds));
        matches!(
            runtime_status.status_with_stale(&a.id, stale_after).status,
            crate::RuntimeStatus::Unhealthy | crate::RuntimeStatus::Cooldown
        )
    };
    // Stable, so the peers keep the strategy's order behind the origin.
    inside.sort_by_key(|a| !boundary.is_origin(&a.id));
    let (mut order, demoted): (Vec<_>, Vec<_>) = inside.into_iter().partition(|a| !unavailable(a));
    order.extend(outside);
    order.extend(demoted);
    order
}

/// Name every target the health/cooldown filter dropped, and how many
/// candidates the dispatch loop is left with.
///
/// At WARN, because a group running on fewer targets than the operator
/// configured is a degraded state they want to see without having raised
/// verbosity first — an incident is diagnosed from the log level the
/// gateway was already running at, and `info` is the default. Throttled per
/// (target, reason) by
/// [`crate::ModelRuntimeStatusTracker::should_log_exclusion`] so a cooling
/// target in a busy group produces one line a minute rather than one per
/// request.
fn log_candidate_exclusions(
    runtime_status: &crate::ModelRuntimeStatusTracker,
    virtual_name: &str,
    candidates: usize,
    excluded: &[ExcludedCandidate],
) {
    for ex in excluded {
        if !runtime_status.should_log_exclusion(virtual_name, &ex.id, ex.reason) {
            continue;
        }
        tracing::warn!(
            virtual_model = %virtual_name,
            target_model = %ex.model,
            reason = ex.reason,
            candidates,
            "routing candidate excluded before dispatch",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_core::{Routing, RoutingStrategy, RoutingTarget};
    use aisix_gateway::BridgeCapability;

    fn r(
        strategy: RoutingStrategy,
        targets: Vec<RoutingTarget>,
        max_fallbacks: Option<u32>,
    ) -> Routing {
        Routing {
            strategy,
            targets,
            retries: None,
            max_fallbacks,
            retry_on_429: None,
            fallback_on_statuses: None,
            when_all_unavailable: None,
            hash_on: None,
        }
    }

    fn tagged(model: &str, tags: &[&str]) -> RoutingTarget {
        RoutingTarget::new(model).with_tags(tags.iter().map(|s| s.to_string()).collect())
    }

    fn model_names(targets: &[RoutingTarget]) -> Vec<&str> {
        targets.iter().map(|t| t.model.as_str()).collect()
    }

    #[test]
    fn stable_hash_is_deterministic() {
        assert_eq!(stable_hash("session-abc"), stable_hash("session-abc"));
        assert_ne!(stable_hash("a"), stable_hash("b"));
    }

    #[test]
    fn chash_preference_order_is_deterministic_per_key() {
        let targets = vec![
            RoutingTarget::new("a").with_weight(50),
            RoutingTarget::new("b").with_weight(50),
        ];
        let ring = HashRing::build(
            tier_fingerprint(&targets),
            &targets,
            RingScale::of(targets.iter()),
        );
        let first = ring.preference_order(stable_hash("session-1"), targets.len());
        assert_eq!(first.len(), 2);
        for _ in 0..50 {
            assert_eq!(
                ring.preference_order(stable_hash("session-1"), targets.len()),
                first
            );
        }
    }

    #[test]
    fn chash_spreads_distinct_keys() {
        // Distinct keys shouldn't all funnel to one target.
        let targets = vec![
            RoutingTarget::new("a").with_weight(50),
            RoutingTarget::new("b").with_weight(50),
        ];
        let ring = HashRing::build(
            tier_fingerprint(&targets),
            &targets,
            RingScale::of(targets.iter()),
        );
        let mut seen = [false; 2];
        for i in 0..200 {
            seen[ring.preference_order(stable_hash(&format!("k{i}")), 2)[0] as usize] = true;
        }
        assert!(seen[0] && seen[1]);
    }

    #[test]
    fn chash_weight_scales_a_targets_share_of_keys() {
        let targets = vec![
            RoutingTarget::new("heavy").with_weight(90),
            RoutingTarget::new("light").with_weight(10),
        ];
        let ring = HashRing::build(
            tier_fingerprint(&targets),
            &targets,
            RingScale::of(targets.iter()),
        );
        let mut heavy = 0;
        for i in 0..1000 {
            if ring.preference_order(stable_hash(&format!("k{i}")), 2)[0] == 0 {
                heavy += 1;
            }
        }
        // 90/10 configured; allow generous slack for hash variance.
        assert!(
            (800..=980).contains(&heavy),
            "expected ~900/1000 keys on the heavy target, got {heavy}"
        );
    }

    #[test]
    fn chash_removing_a_target_only_moves_its_own_keys() {
        // The consistent-hash property the whole feature hangs on: dropping
        // one target must not remap keys whose first choice survives.
        let full = vec![
            RoutingTarget::new("a"),
            RoutingTarget::new("b"),
            RoutingTarget::new("c"),
        ];
        let scale = RingScale::of(full.iter());
        let ring = HashRing::build(tier_fingerprint(&full), &full, scale);
        let shrunk: Vec<RoutingTarget> = vec![full[0].clone(), full[2].clone()]; // drop "b"
        let shrunk_ring = HashRing::build(tier_fingerprint(&shrunk), &shrunk, scale);
        for i in 0..500 {
            let h = stable_hash(&format!("k{i}"));
            let before = full[ring.preference_order(h, 3)[0] as usize].model.clone();
            let after = shrunk[shrunk_ring.preference_order(h, 2)[0] as usize]
                .model
                .clone();
            if before != "b" {
                assert_eq!(before, after, "key k{i} moved although its target survived");
            }
        }
    }

    #[test]
    fn chash_pick_targets_pins_a_key_and_walks_the_ring() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::ConsistentHash,
            vec![
                RoutingTarget::new("stable").with_weight(90),
                RoutingTarget::new("canary").with_weight(10),
            ],
            None,
        );
        let first = reg.pick_targets("v", &routing, &routing.targets, "user-42");
        assert_eq!(first.len(), 2, "walk covers the whole tier");
        for _ in 0..20 {
            assert_eq!(
                reg.pick_targets("v", &routing, &routing.targets, "user-42"),
                first
            );
        }
    }

    #[test]
    fn eligible_no_tagged_target_returns_all() {
        // No target is tagged → tag routing isn't in use, even with request tags.
        let targets = vec![RoutingTarget::new("a"), RoutingTarget::new("b")];
        assert_eq!(
            model_names(&eligible_targets(&targets, &["x".into()])),
            vec!["a", "b"]
        );
    }

    #[test]
    fn eligible_matches_any_overlapping_tag() {
        let targets = vec![tagged("eu", &["eu"]), tagged("us", &["us"])];
        assert_eq!(
            model_names(&eligible_targets(&targets, &["eu".into()])),
            vec!["eu"]
        );
    }

    #[test]
    fn eligible_tagged_no_match_falls_back_to_default() {
        let targets = vec![tagged("eu", &["eu"]), tagged("fallback", &["default"])];
        assert_eq!(
            model_names(&eligible_targets(&targets, &["apac".into()])),
            vec!["fallback"]
        );
    }

    #[test]
    fn eligible_untagged_request_prefers_default() {
        let targets = vec![tagged("eu", &["eu"]), tagged("fallback", &["default"])];
        assert_eq!(
            model_names(&eligible_targets(&targets, &[])),
            vec!["fallback"]
        );
    }

    #[test]
    fn eligible_untagged_request_without_default_returns_all() {
        let targets = vec![tagged("eu", &["eu"]), tagged("us", &["us"])];
        assert_eq!(
            model_names(&eligible_targets(&targets, &[])),
            vec!["eu", "us"]
        );
    }

    // ───────────────── targets named by resource id ─────────────────

    /// A routing model whose targets are named by `model_id`. Every stage
    /// downstream of the normalization keys on `target.model`, so the
    /// assertion that matters is what the whole resolution walk hands back.
    fn resolve_group(
        snapshot: &AisixSnapshot,
        targets: Vec<RoutingTarget>,
    ) -> Result<Vec<String>, ProxyError> {
        let group: Model = serde_json::from_value(serde_json::json!({
            "display_name": "group",
            "routing": {"strategy": "failover", "targets": []},
        }))
        .unwrap();
        let mut group = group;
        group.routing = Some(r(RoutingStrategy::Failover, targets, None));
        resolve_attempt_models(
            &RoutingRegistry::new(),
            &crate::ModelRuntimeStatusTracker::new(),
            &LivePricingIndex::new(),
            snapshot,
            "group",
            "g-1",
            &std::sync::Arc::new(group),
            RoutingRequest::default(),
        )
        .map(|attempts| {
            attempts
                .into_iter()
                .map(|a| a.model.display_name.clone())
                .collect()
        })
    }

    #[test]
    fn a_target_named_by_id_resolves_to_that_model() {
        // `ip_snapshot` assigns ids `m-0`, `m-1`, … in declaration order.
        let snap = ip_snapshot(&[("alpha", None), ("beta", None)]);
        assert_eq!(
            resolve_group(&snap, vec![RoutingTarget::by_id("m-1")]).unwrap(),
            vec!["beta"]
        );
    }

    #[test]
    fn a_target_named_by_id_follows_a_rename() {
        let target = vec![RoutingTarget::by_id("m-0")];
        let before = ip_snapshot(&[("alpha", None)]);
        assert_eq!(
            resolve_group(&before, target.clone()).unwrap(),
            vec!["alpha"]
        );
        // Same id, new display name; the routing document is untouched.
        let after = ip_snapshot(&[("alpha-v2", None)]);
        assert_eq!(resolve_group(&after, target).unwrap(), vec!["alpha-v2"]);
    }

    #[test]
    fn an_id_target_wins_over_the_name_beside_it() {
        let snap = ip_snapshot(&[("alpha", None), ("beta", None)]);
        let conflicting = RoutingTarget {
            model: "alpha".into(),
            model_id: Some("m-1".into()),
            weight: None,
            priority: None,
            tags: None,
        };
        assert_eq!(
            resolve_group(&snap, vec![conflicting]).unwrap(),
            vec!["beta"]
        );
    }

    #[test]
    fn an_unresolvable_id_target_fails_like_an_unresolvable_name() {
        let snap = ip_snapshot(&[("alpha", None)]);
        let by_id = resolve_group(&snap, vec![RoutingTarget::by_id("m-gone")]).unwrap_err();
        let by_name = resolve_group(&snap, vec![RoutingTarget::new("m-gone")]).unwrap_err();
        assert!(
            matches!(by_id, ProxyError::InvalidRequest(_)),
            "expected the same config error a dangling name raises, got {by_id:?}"
        );
        assert_eq!(by_id.to_string(), by_name.to_string());
    }

    /// The per-target IP allowlist keys on the target's Model, so it has to
    /// see the resolved one — a group that gated only name-form targets
    /// would let an id-form target through a restriction the operator set.
    #[test]
    fn an_id_target_is_still_subject_to_its_own_ip_allowlist() {
        let snap = ip_snapshot(&[("restricted", Some(vec!["10.0.0.0/8"]))]);
        let group: Model = serde_json::from_value(serde_json::json!({
            "display_name": "group",
            "routing": {"strategy": "failover", "targets": []},
        }))
        .unwrap();
        let mut group = group;
        group.routing = Some(r(
            RoutingStrategy::Failover,
            vec![RoutingTarget::by_id("m-0")],
            None,
        ));
        let out_of_range = resolve_attempt_models(
            &RoutingRegistry::new(),
            &crate::ModelRuntimeStatusTracker::new(),
            &LivePricingIndex::new(),
            &snap,
            "group",
            "g-1",
            &std::sync::Arc::new(group),
            RoutingRequest {
                source_ip: "8.8.8.8",
                ..Default::default()
            },
        );
        assert!(matches!(
            out_of_range,
            Err(ProxyError::ModelIpRestricted(_))
        ));
    }

    // ───────────────── per-target client-IP allowlist ─────────────────

    fn pk_snapshot(models: &[(&str, &str)]) -> AisixSnapshot {
        let table = aisix_core::snapshot::ResourceTable::default();
        for (name, pk) in models {
            let model: Model = serde_json::from_value(serde_json::json!({
                "display_name": name,
                "provider": "openai",
                "model_name": "up",
                "provider_key_id": pk,
            }))
            .unwrap();
            table.insert(aisix_core::ResourceEntry::new(
                format!("id-{name}"),
                model,
                1,
            ));
        }
        AisixSnapshot {
            models: table,
            ..Default::default()
        }
    }

    fn pinned_order(
        snap: &AisixSnapshot,
        status: &crate::ModelRuntimeStatusTracker,
        strategy: RoutingStrategy,
        targets: &[&str],
        max_fallbacks: Option<u32>,
        origin: &str,
    ) -> Vec<String> {
        let mut group: Model = serde_json::from_value(serde_json::json!({
            "display_name": "group",
            "routing": {"strategy": "failover", "targets": []},
        }))
        .unwrap();
        group.routing = Some(r(
            strategy,
            targets.iter().map(|t| RoutingTarget::new(*t)).collect(),
            max_fallbacks,
        ));
        resolve_attempt_models(
            &RoutingRegistry::new(),
            status,
            &LivePricingIndex::new(),
            snap,
            "group",
            "g-1",
            &std::sync::Arc::new(group),
            RoutingRequest {
                context_origin: Some(origin),
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|a| a.model.display_name.clone())
        .collect()
    }

    #[test]
    fn context_origin_goes_first_then_its_provider_key_peers() {
        let snap = pk_snapshot(&[("c", "pk-2"), ("a", "pk-1"), ("b", "pk-3"), ("a2", "pk-1")]);
        let status = crate::ModelRuntimeStatusTracker::new();
        assert_eq!(
            pinned_order(
                &snap,
                &status,
                RoutingStrategy::Failover,
                &["c", "a", "b", "a2"],
                None,
                "id-a2"
            ),
            ["a2", "a", "c", "b"],
        );
        // A peer the max_fallbacks cut left out is brought back ahead of the rest.
        assert_eq!(
            pinned_order(
                &snap,
                &status,
                RoutingStrategy::Failover,
                &["c", "b", "a2"],
                Some(1),
                "id-a"
            ),
            ["a2", "c"],
        );
    }

    #[test]
    fn an_unavailable_boundary_does_not_spend_the_fallback_budget() {
        let snap = pk_snapshot(&[("a", "pk-1"), ("a2", "pk-1"), ("c", "pk-2")]);
        let status = crate::ModelRuntimeStatusTracker::new();
        status.mark_cooldown("id-a", Duration::from_secs(60), "test");
        status.mark_cooldown("id-a2", Duration::from_secs(60), "test");
        assert_eq!(
            pinned_order(
                &snap,
                &status,
                RoutingStrategy::Failover,
                &["a", "a2", "c"],
                Some(1),
                "id-a"
            ),
            ["c"],
        );
    }

    // ───────────────── the selection path, filters and strategy together ─────────

    fn group_model(routing: Routing) -> std::sync::Arc<Model> {
        let mut group: Model = serde_json::from_value(serde_json::json!({
            "display_name": "group",
            "routing": {"strategy": "failover", "targets": []},
        }))
        .unwrap();
        group.routing = Some(routing);
        std::sync::Arc::new(group)
    }

    /// One real walk through `resolve_attempt_models`, naming what comes out.
    fn select(
        registry: &RoutingRegistry,
        status: &crate::ModelRuntimeStatusTracker,
        snap: &AisixSnapshot,
        routing: Routing,
        req: RoutingRequest<'_>,
    ) -> Result<Vec<String>, ProxyError> {
        resolve_attempt_models(
            registry,
            status,
            &LivePricingIndex::new(),
            snap,
            "group",
            "g-1",
            &group_model(routing),
            req,
        )
        .map(|attempts| {
            attempts
                .into_iter()
                .map(|a| a.model.display_name.clone())
                .collect()
        })
    }

    fn snap_of(names: &[&str]) -> AisixSnapshot {
        let rows: Vec<(&str, &str)> = names.iter().map(|n| (*n, "pk-1")).collect();
        pk_snapshot(&rows)
    }

    fn cooling(names: &[&str]) -> crate::ModelRuntimeStatusTracker {
        let status = crate::ModelRuntimeStatusTracker::new();
        for name in names {
            status.mark_cooldown(&format!("id-{name}"), Duration::from_secs(60), "test");
        }
        status
    }

    /// How often each target comes first over `rounds` requests.
    fn first_picks<'a>(
        rounds: usize,
        mut pick: impl FnMut(usize) -> Vec<String> + 'a,
    ) -> std::collections::BTreeMap<String, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for round in 0..rounds {
            *counts.entry(pick(round).remove(0)).or_default() += 1;
        }
        counts
    }

    #[test]
    fn a_cooling_target_does_not_spend_the_fallback_budget() {
        let snap = snap_of(&["a", "b", "c"]);
        let status = cooling(&["a", "b"]);
        let routing = r(
            RoutingStrategy::Failover,
            vec![
                RoutingTarget::new("a"),
                RoutingTarget::new("b"),
                RoutingTarget::new("c"),
            ],
            Some(1),
        );
        assert_eq!(
            select(
                &RoutingRegistry::new(),
                &status,
                &snap,
                routing,
                RoutingRequest::default()
            )
            .unwrap(),
            ["c"],
        );
    }

    #[test]
    fn a_cooling_tier_leaves_the_backup_tier_reachable() {
        let snap = snap_of(&["a", "b", "backup"]);
        let status = cooling(&["a", "b"]);
        let routing = r(
            RoutingStrategy::Failover,
            vec![
                RoutingTarget::new("a").with_priority(1),
                RoutingTarget::new("b").with_priority(1),
                RoutingTarget::new("backup"),
            ],
            Some(1),
        );
        assert_eq!(
            select(
                &RoutingRegistry::new(),
                &status,
                &snap,
                routing,
                RoutingRequest::default()
            )
            .unwrap(),
            ["backup"],
        );
    }

    #[test]
    fn round_robin_with_no_fallbacks_never_picks_a_cooling_target() {
        let snap = snap_of(&["a", "b"]);
        let status = cooling(&["a"]);
        let registry = RoutingRegistry::new();
        let picks = first_picks(100, |_| {
            select(
                &registry,
                &status,
                &snap,
                r(
                    RoutingStrategy::RoundRobin,
                    vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
                    Some(0),
                ),
                RoutingRequest::default(),
            )
            .unwrap()
        });
        assert_eq!(picks.get("a"), None);
        assert_eq!(picks.get("b"), Some(&100));
    }

    /// The cap still bounds the walk, over the targets that can serve.
    #[test]
    fn the_fallback_cap_applies_to_the_targets_left_after_the_filter() {
        let snap = snap_of(&["a", "b", "c", "d"]);
        let status = cooling(&["a"]);
        let routing = r(
            RoutingStrategy::Failover,
            ["a", "b", "c", "d"]
                .iter()
                .map(|t| RoutingTarget::new(*t))
                .collect(),
            Some(1),
        );
        assert_eq!(
            select(
                &RoutingRegistry::new(),
                &status,
                &snap,
                routing,
                RoutingRequest::default()
            )
            .unwrap(),
            ["b", "c"],
        );
    }

    #[test]
    fn a_cooling_targets_share_spreads_across_the_rest() {
        let snap = snap_of(&["a", "b", "c"]);
        let status = cooling(&["a"]);
        let registry = RoutingRegistry::new();
        let picks = first_picks(3000, |_| {
            select(
                &registry,
                &status,
                &snap,
                r(
                    RoutingStrategy::RoundRobin,
                    ["a", "b", "c"]
                        .iter()
                        .map(|t| RoutingTarget::new(*t))
                        .collect(),
                    None,
                ),
                RoutingRequest::default(),
            )
            .unwrap()
        });
        assert_eq!(picks.get("b"), Some(&1500));
        assert_eq!(picks.get("c"), Some(&1500));
    }

    /// Callers with different tags take turns in one rotation, and each tag's
    /// targets split that tag's traffic. A rotation that restarted on every
    /// change of subset sent all of it to each tag's first target.
    #[test]
    fn interleaved_tag_subsets_keep_their_rotation() {
        let snap = snap_of(&["eu1", "eu2", "us1", "us2"]);
        let status = crate::ModelRuntimeStatusTracker::new();
        let registry = RoutingRegistry::new();
        let tags = [vec!["eu".to_string()], vec!["us".to_string()]];
        let picks = first_picks(4000, |round| {
            select(
                &registry,
                &status,
                &snap,
                r(
                    RoutingStrategy::RoundRobin,
                    vec![
                        tagged("eu1", &["eu"]),
                        tagged("eu2", &["eu"]),
                        tagged("us1", &["us"]),
                        tagged("us2", &["us"]),
                    ],
                    None,
                ),
                RoutingRequest {
                    tags: &tags[round % 2],
                    ..Default::default()
                },
            )
            .unwrap()
        });
        for target in ["eu1", "eu2", "us1", "us2"] {
            assert_eq!(picks.get(target), Some(&1000), "{target}: {picks:?}");
        }
    }

    /// Subsets that overlap: a target only some callers may use keeps its
    /// share of those callers, and the others split the remainder.
    #[test]
    fn overlapping_ip_subsets_share_one_fair_rotation() {
        let table = aisix_core::snapshot::ResourceTable::default();
        for (name, cidrs) in [("a", Some(vec!["10.0.0.0/8"])), ("b", None), ("c", None)] {
            let model: Model = serde_json::from_value(serde_json::json!({
                "display_name": name,
                "provider": "openai",
                "model_name": "up",
                "provider_key_id": "pk-1",
                "allowed_cidrs": cidrs,
            }))
            .unwrap();
            table.insert(aisix_core::ResourceEntry::new(
                format!("id-{name}"),
                model,
                1,
            ));
        }
        let snap = AisixSnapshot {
            models: table,
            ..Default::default()
        };
        let status = crate::ModelRuntimeStatusTracker::new();
        let registry = RoutingRegistry::new();
        let picks = first_picks(3000, |round| {
            select(
                &registry,
                &status,
                &snap,
                r(
                    RoutingStrategy::RoundRobin,
                    ["a", "b", "c"]
                        .iter()
                        .map(|t| RoutingTarget::new(*t))
                        .collect(),
                    None,
                ),
                RoutingRequest {
                    source_ip: if round % 2 == 0 {
                        "10.1.2.3"
                    } else {
                        "8.8.8.8"
                    },
                    ..Default::default()
                },
            )
            .unwrap()
        });
        assert_eq!(picks.get("a"), Some(&500), "{picks:?}");
        assert_eq!(picks.get("b"), Some(&1250), "{picks:?}");
        assert_eq!(picks.get("c"), Some(&1250), "{picks:?}");
    }

    #[test]
    fn a_target_at_weight_zero_gets_no_traffic() {
        let snap = snap_of(&["live", "drained"]);
        let status = crate::ModelRuntimeStatusTracker::new();
        for strategy in [
            RoutingStrategy::RoundRobin,
            RoutingStrategy::ConsistentHash,
            RoutingStrategy::LeastBusy,
        ] {
            let registry = RoutingRegistry::new();
            let headers: Vec<HeaderMap> = (0..64)
                .map(|i| {
                    let mut h = HeaderMap::new();
                    h.insert(
                        "x-aisix-routing-key",
                        format!("key-{i}").parse().expect("header value"),
                    );
                    h
                })
                .collect();
            let picks = first_picks(64, |round| {
                select(
                    &registry,
                    &status,
                    &snap,
                    r(
                        strategy,
                        vec![
                            RoutingTarget::new("live").with_weight(1),
                            RoutingTarget::new("drained").with_weight(0),
                        ],
                        None,
                    ),
                    RoutingRequest {
                        headers: Some(&headers[round]),
                        ..Default::default()
                    },
                )
                .unwrap()
            });
            assert_eq!(picks.get("live"), Some(&64), "{strategy:?}: {picks:?}");
        }
    }

    /// A stored `weighted` group's `weight: 0` target was its backup: never
    /// the first choice, still tried once the others failed or went down.
    /// Loaded through the stored-row upgrade, it still is, whichever strategy
    /// the row lands on.
    #[test]
    fn a_stored_weighted_backup_stays_a_fallback() {
        let snap = snap_of(&["primary", "backup"]);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-aisix-routing-key",
            "cohort".parse().expect("header value"),
        );
        for sticky in [false, true] {
            let mut doc = serde_json::json!({ "display_name": "group", "routing": {
                "strategy": "weighted",
                "sticky": sticky,
                "targets": [
                    { "model": "primary", "weight": 100 },
                    { "model": "backup", "weight": 0 }
                ]
            }});
            aisix_core::models::upgrade_stored_routing(&mut doc);
            let routing: Routing = serde_json::from_value(doc["routing"].clone()).unwrap();
            let status = crate::ModelRuntimeStatusTracker::new();
            let req = || RoutingRequest {
                headers: Some(&headers),
                ..Default::default()
            };
            for _ in 0..8 {
                assert_eq!(
                    select(
                        &RoutingRegistry::new(),
                        &status,
                        &snap,
                        routing.clone(),
                        req()
                    )
                    .unwrap(),
                    ["primary", "backup"],
                    "sticky={sticky}"
                );
            }
            status.mark_unhealthy("id-primary", Some(503), "background_check_failed");
            assert_eq!(
                select(&RoutingRegistry::new(), &status, &snap, routing, req()).unwrap(),
                ["backup"],
                "sticky={sticky}"
            );
        }
    }

    /// `failover` documents that it does not read `weight`, so a 0 there
    /// changes nothing.
    #[test]
    fn weight_zero_means_nothing_to_a_strategy_that_ignores_weight() {
        let snap = snap_of(&["a", "b"]);
        let routing = r(
            RoutingStrategy::Failover,
            vec![
                RoutingTarget::new("a").with_weight(0),
                RoutingTarget::new("b"),
            ],
            None,
        );
        assert_eq!(
            select(
                &RoutingRegistry::new(),
                &crate::ModelRuntimeStatusTracker::new(),
                &snap,
                routing,
                RoutingRequest::default()
            )
            .unwrap(),
            ["a", "b"],
        );
    }

    /// With nothing above 0 there is no share to compare, so the candidates
    /// stay equal rather than every request being refused.
    #[test]
    fn candidates_all_at_weight_zero_stay_equal() {
        let snap = snap_of(&["a", "b"]);
        let status = crate::ModelRuntimeStatusTracker::new();
        let registry = RoutingRegistry::new();
        let picks = first_picks(10, |_| {
            select(
                &registry,
                &status,
                &snap,
                r(
                    RoutingStrategy::RoundRobin,
                    vec![
                        RoutingTarget::new("a").with_weight(0),
                        RoutingTarget::new("b").with_weight(0),
                    ],
                    None,
                ),
                RoutingRequest::default(),
            )
            .unwrap()
        });
        assert_eq!(picks.get("a"), Some(&5));
        assert_eq!(picks.get("b"), Some(&5));
    }

    #[test]
    fn a_deleted_member_costs_the_group_only_that_member() {
        let snap = snap_of(&["good"]);
        for targets in [
            vec![RoutingTarget::new("good"), RoutingTarget::new("deleted")],
            vec![RoutingTarget::new("deleted"), RoutingTarget::new("good")],
        ] {
            assert_eq!(
                select(
                    &RoutingRegistry::new(),
                    &crate::ModelRuntimeStatusTracker::new(),
                    &snap,
                    r(RoutingStrategy::Failover, targets, None),
                    RoutingRequest::default()
                )
                .unwrap(),
                ["good"],
            );
        }
    }

    /// A deleted member must not use up the fallback budget either.
    /// The first target each of `keys` routing keys lands on.
    fn first_by_key(
        registry: &RoutingRegistry,
        status: &crate::ModelRuntimeStatusTracker,
        snap: &AisixSnapshot,
        targets: &[RoutingTarget],
        keys: usize,
    ) -> Vec<String> {
        (0..keys)
            .map(|i| {
                let mut headers = HeaderMap::new();
                headers.insert(
                    "x-aisix-routing-key",
                    format!("session-{i}").parse().expect("header value"),
                );
                select(
                    registry,
                    status,
                    snap,
                    r(RoutingStrategy::ConsistentHash, targets.to_vec(), Some(0)),
                    RoutingRequest {
                        headers: Some(&headers),
                        ..Default::default()
                    },
                )
                .unwrap()
                .remove(0)
            })
            .collect()
    }

    /// A target cooling moves its own keys and no others, whatever the
    /// weights: a ring scaled from the subset that is up reshuffled keys
    /// between two targets that both stayed up, and each move is a lost
    /// prompt cache or session.
    #[test]
    fn a_cooling_target_moves_only_its_own_consistent_hash_keys() {
        let snap = snap_of(&["a", "b", "c"]);
        for weights in [[2, 2, 1], [33, 33, 34]] {
            let targets: Vec<RoutingTarget> = ["a", "b", "c"]
                .iter()
                .zip(weights)
                .map(|(name, w)| RoutingTarget::new(*name).with_weight(w))
                .collect();
            let registry = RoutingRegistry::new();
            let up = crate::ModelRuntimeStatusTracker::new();
            let before = first_by_key(&registry, &up, &snap, &targets, 2000);
            let after = first_by_key(&registry, &cooling(&["c"]), &snap, &targets, 2000);
            let moved = before
                .iter()
                .zip(&after)
                .filter(|(was, now)| *was != "c" && was != now)
                .count();
            assert_eq!(moved, 0, "weights {weights:?}");
            assert!(after.iter().all(|t| t != "c"), "weights {weights:?}");
        }
    }

    /// A deleted member is no candidate, so it cannot be the positive weight
    /// that drains a standby kept at 0.
    #[test]
    fn a_deleted_member_does_not_drain_a_standby_at_weight_zero() {
        let snap = snap_of(&["standby"]);
        for strategy in [
            RoutingStrategy::RoundRobin,
            RoutingStrategy::ConsistentHash,
            RoutingStrategy::LeastBusy,
        ] {
            let routing = r(
                strategy,
                vec![
                    RoutingTarget::new("deleted").with_weight(1),
                    RoutingTarget::new("standby").with_weight(0),
                ],
                None,
            );
            assert_eq!(
                select(
                    &RoutingRegistry::new(),
                    &crate::ModelRuntimeStatusTracker::new(),
                    &snap,
                    routing,
                    RoutingRequest::default()
                )
                .unwrap(),
                ["standby"],
                "{strategy:?}",
            );
        }
    }

    /// A target whose row was deleted admits nobody to an answer that skips
    /// dispatch, since nothing could be dispatched to it. A group whose every
    /// target is gone is left for dispatch to report.
    #[test]
    fn a_dangling_target_admits_nobody_to_a_member_allowlisted_answer() {
        let snap = ip_snapshot(&[("restricted", Some(vec!["10.0.0.0/8"]))]);
        let group = group_model(r(
            RoutingStrategy::Failover,
            vec![
                RoutingTarget::new("restricted"),
                RoutingTarget::new("deleted"),
            ],
            None,
        ));
        assert!(matches!(
            check_member_ip_access(&snap, "group", &group, &[], "8.8.8.8"),
            Err(ProxyError::ModelIpRestricted(_))
        ));
        assert!(check_member_ip_access(&snap, "group", &group, &[], "10.1.2.3").is_ok());
        let all_gone = group_model(r(
            RoutingStrategy::Failover,
            vec![RoutingTarget::new("deleted")],
            None,
        ));
        assert!(check_member_ip_access(&snap, "group", &all_gone, &[], "8.8.8.8").is_ok());
    }

    #[test]
    fn a_deleted_member_does_not_spend_the_fallback_budget() {
        let snap = snap_of(&["good"]);
        let routing = r(
            RoutingStrategy::Failover,
            vec![RoutingTarget::new("deleted"), RoutingTarget::new("good")],
            Some(0),
        );
        assert_eq!(
            select(
                &RoutingRegistry::new(),
                &crate::ModelRuntimeStatusTracker::new(),
                &snap,
                routing,
                RoutingRequest::default()
            )
            .unwrap(),
            ["good"],
        );
    }

    #[test]
    fn a_group_with_no_member_left_is_refused_without_naming_one() {
        let snap = snap_of(&["unrelated"]);
        let err = select(
            &RoutingRegistry::new(),
            &crate::ModelRuntimeStatusTracker::new(),
            &snap,
            r(
                RoutingStrategy::Failover,
                vec![RoutingTarget::new("secret-member")],
                None,
            ),
            RoutingRequest::default(),
        )
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
        assert!(!err.to_string().contains("secret-member"), "{err}");
    }

    /// `when_all_unavailable: fail` answers for a group a health check has
    /// entirely marked down. Cooling targets are still sent to: nothing shows
    /// them broken, and a success is what brings them back early.
    #[test]
    fn every_target_cooling_is_still_dispatched_under_fail() {
        let snap = snap_of(&["a", "b"]);
        let status = cooling(&["a", "b"]);
        let mut routing = r(
            RoutingStrategy::Failover,
            vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
            None,
        );
        routing.when_all_unavailable = Some(WhenAllUnavailablePolicy::Fail);
        assert_eq!(
            select(
                &RoutingRegistry::new(),
                &status,
                &snap,
                routing,
                RoutingRequest::default()
            )
            .unwrap(),
            ["a", "b"],
        );
    }

    #[test]
    fn every_target_unhealthy_is_refused_under_fail() {
        let snap = snap_of(&["a", "b"]);
        let status = crate::ModelRuntimeStatusTracker::new();
        status.mark_unhealthy("id-a", Some(503), "background_check_failed");
        status.mark_unhealthy("id-b", Some(503), "background_check_failed");
        let mut routing = r(
            RoutingStrategy::Failover,
            vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
            None,
        );
        routing.when_all_unavailable = Some(WhenAllUnavailablePolicy::Fail);
        assert!(matches!(
            select(
                &RoutingRegistry::new(),
                &status,
                &snap,
                routing,
                RoutingRequest::default()
            ),
            Err(ProxyError::AllCandidatesUnavailable { .. })
        ));
    }

    #[test]
    fn try_anyway_tries_unhealthy_targets_up_to_the_fallback_cap() {
        let snap = snap_of(&["a", "b", "c"]);
        let status = crate::ModelRuntimeStatusTracker::new();
        for id in ["id-a", "id-b", "id-c"] {
            status.mark_unhealthy(id, Some(503), "background_check_failed");
        }
        let mut routing = r(
            RoutingStrategy::Failover,
            vec![
                RoutingTarget::new("a"),
                RoutingTarget::new("b"),
                RoutingTarget::new("c"),
            ],
            Some(1),
        );
        routing.when_all_unavailable = Some(WhenAllUnavailablePolicy::TryAnyway);
        assert_eq!(
            select(
                &RoutingRegistry::new(),
                &status,
                &snap,
                routing,
                RoutingRequest::default()
            )
            .unwrap(),
            ["a", "b"],
        );
    }

    /// A target that failed drops behind the others instead of being tried
    /// first on every request, whether it had no samples or a fast history.
    #[test]
    fn a_failed_target_ranks_last_on_least_latency() {
        let snap = snap_of(&["broken", "ok"]);
        let routing = || {
            r(
                RoutingStrategy::LeastLatency,
                vec![RoutingTarget::new("broken"), RoutingTarget::new("ok")],
                None,
            )
        };
        let run = |status: &crate::ModelRuntimeStatusTracker| {
            select(
                &RoutingRegistry::new(),
                status,
                &snap,
                routing(),
                RoutingRequest::default(),
            )
            .unwrap()
        };
        let unmeasured = crate::ModelRuntimeStatusTracker::new();
        unmeasured.record_latency("id-ok", 900);
        assert_eq!(run(&unmeasured), ["broken", "ok"], "a probe goes first");
        unmeasured.penalize_latency("id-broken");
        assert_eq!(run(&unmeasured), ["ok", "broken"]);

        let was_fast = crate::ModelRuntimeStatusTracker::new();
        was_fast.record_latency("id-ok", 900);
        was_fast.record_latency("id-broken", 50);
        assert_eq!(run(&was_fast), ["broken", "ok"]);
        was_fast.penalize_latency("id-broken");
        assert_eq!(run(&was_fast), ["ok", "broken"]);
    }

    /// A route only some targets can answer: the others are not candidates,
    /// so they cannot use up the fallback budget.
    #[test]
    fn a_partial_route_only_sees_the_targets_it_can_use() {
        let snap = snap_of(&["foreign", "native"]);
        let status = crate::ModelRuntimeStatusTracker::new();
        let registry = RoutingRegistry::new();
        let serves = |m: &Model| m.display_name == "native";
        for _ in 0..4 {
            assert_eq!(
                select(
                    &registry,
                    &status,
                    &snap,
                    r(
                        RoutingStrategy::RoundRobin,
                        vec![RoutingTarget::new("foreign"), RoutingTarget::new("native")],
                        Some(0),
                    ),
                    RoutingRequest {
                        serves: Some(&serves),
                        ..Default::default()
                    },
                )
                .unwrap(),
                ["native"],
            );
        }
        let serves_nothing = |_: &Model| false;
        assert_eq!(
            select(
                &registry,
                &status,
                &snap,
                r(
                    RoutingStrategy::RoundRobin,
                    vec![RoutingTarget::new("foreign")],
                    None,
                ),
                RoutingRequest {
                    serves: Some(&serves_nothing),
                    ..Default::default()
                },
            )
            .unwrap(),
            Vec::<String>::new(),
        );
    }

    /// A client alternating a partial route with the group's other routes
    /// must not always land the others on the same target.
    #[test]
    fn a_partial_route_does_not_move_the_groups_rotation() {
        let snap = snap_of(&["a", "b"]);
        let status = crate::ModelRuntimeStatusTracker::new();
        let registry = RoutingRegistry::new();
        let group = || {
            r(
                RoutingStrategy::RoundRobin,
                vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
                None,
            )
        };
        let serves = |_: &Model| true;
        let picks = first_picks(10, |_| {
            let _ = select(
                &registry,
                &status,
                &snap,
                group(),
                RoutingRequest {
                    serves: Some(&serves),
                    ..Default::default()
                },
            );
            select(
                &registry,
                &status,
                &snap,
                group(),
                RoutingRequest::default(),
            )
            .unwrap()
        });
        assert_eq!(picks.get("a"), Some(&5));
        assert_eq!(picks.get("b"), Some(&5));
    }

    /// The partial route's rotation is reclaimed with its group, and only then.
    #[test]
    fn a_partial_routes_rotation_lives_and_dies_with_its_group() {
        let registry = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
            None,
        );
        let partial = format!("group{PARTIAL_ROUTE_ROTATION}");
        registry.pick_targets(&partial, &routing, &routing.targets, "");
        registry.retain_configured(&|name| name == "group");
        assert_eq!(registry.wrr.len(), 1);
        registry.retain_configured(&|_| false);
        assert_eq!(registry.wrr.len(), 0);
    }

    #[test]
    fn a_targets_own_configuration_moves_the_loop_on_and_a_bad_request_does_not() {
        for unusable in [
            ProxyError::UnusableTarget("no provider key".into()),
            ProxyError::ProviderUnavailable,
            ProxyError::Bridge(BridgeError::InvalidUpstreamConfig("no base".into())),
            ProxyError::Bridge(BridgeError::InvalidUpstreamCredentials("empty".into())),
        ] {
            assert!(target_unusable(&unusable), "{unusable:?}");
        }
        for other in [
            ProxyError::InvalidRequest("bad body".into()),
            ProxyError::Bridge(BridgeError::InvalidRequest("bad body".into())),
            ProxyError::Bridge(BridgeError::upstream_status(401, "expired")),
            ProxyError::Bridge(BridgeError::UpstreamDecode("x".into())),
        ] {
            assert!(!target_unusable(&other), "{other:?}");
        }
    }

    // ───────────────── key-bound provider groups ─────────────────

    /// `model-a` and `model-d` on `pk-alpha` (group `grp-alpha`), `model-b`
    /// on `pk-beta` (group `grp-beta`), `model-none` on `pk-none` (no group).
    /// The tokens are distinctive so an assertion that a refusal does not
    /// name them cannot pass or fail on ordinary English text.
    fn groups_snapshot() -> AisixSnapshot {
        let snap = pk_snapshot(&[
            ("model-a", "pk-id-alpha"),
            ("model-b", "pk-id-beta"),
            ("model-d", "pk-id-alpha"),
            ("model-none", "pk-id-none"),
        ]);
        for (id, name, groups) in [
            ("pk-id-alpha", "pk-alpha", vec!["grp-alpha"]),
            ("pk-id-beta", "pk-beta", vec!["grp-beta"]),
            ("pk-id-none", "pk-none", vec![]),
        ] {
            let pk: aisix_core::ProviderKey = serde_json::from_value(serde_json::json!({
                "display_name": name,
                "api_key": "sk-x",
                "groups": groups,
            }))
            .unwrap();
            snap.provider_keys
                .insert(aisix_core::ResourceEntry::new(id, pk, 1));
        }
        snap
    }

    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn every_grouped_target() -> Vec<RoutingTarget> {
        vec![
            RoutingTarget::new("model-a"),
            RoutingTarget::new("model-b"),
            RoutingTarget::new("model-none"),
        ]
    }

    fn grouped_select(
        snap: &AisixSnapshot,
        targets: Vec<RoutingTarget>,
        req: RoutingRequest<'_>,
    ) -> Result<Vec<String>, ProxyError> {
        select(
            &RoutingRegistry::new(),
            &crate::ModelRuntimeStatusTracker::new(),
            snap,
            r(RoutingStrategy::Failover, targets, Some(5)),
            req,
        )
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    /// Every name a refusal to a grouped key must keep to itself.
    fn assert_names_no_group_or_member(err: &ProxyError) {
        let text = format!("{err} {err:?}");
        for secret in [
            "grp-alpha",
            "grp-beta",
            "grp-zeta",
            "pk-alpha",
            "pk-beta",
            "pk-none",
            "model-a",
            "model-b",
            "model-d",
            "model-none",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn a_key_with_groups_only_reaches_targets_in_those_groups() {
        let snap = groups_snapshot();
        let allowed = groups(&["grp-alpha"]);
        let order = grouped_select(
            &snap,
            every_grouped_target(),
            RoutingRequest {
                allowed_groups: &allowed,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(order, vec!["model-a"]);
    }

    #[test]
    fn a_key_with_several_groups_reaches_each_of_them() {
        let snap = groups_snapshot();
        let allowed = groups(&["grp-alpha", "grp-beta"]);
        let order = grouped_select(
            &snap,
            every_grouped_target(),
            RoutingRequest {
                allowed_groups: &allowed,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sorted(order), vec!["model-a", "model-b"]);
    }

    #[test]
    fn a_key_without_groups_is_unconstrained() {
        let snap = groups_snapshot();
        let order = grouped_select(
            &snap,
            every_grouped_target(),
            RoutingRequest {
                allowed_groups: &[],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sorted(order), vec!["model-a", "model-b", "model-none"]);
    }

    /// The routing-tag header narrows within the key's groups and never
    /// past them — not to a target tagged with the asked-for value, and not
    /// to a `default`-tagged one outside the groups either.
    #[test]
    fn header_tags_never_widen_beyond_the_keys_groups() {
        let snap = groups_snapshot();
        let targets = || {
            vec![
                tagged("model-a", &["grp-alpha"]),
                tagged("model-b", &["grp-beta"]),
                tagged("model-none", &["default"]),
            ]
        };
        let asked = groups(&["grp-beta"]);
        // Without a group constraint the header picks its target, so the
        // refusal below is the constraint at work and not a dead tag.
        assert_eq!(
            grouped_select(
                &snap,
                targets(),
                RoutingRequest {
                    tags: &asked,
                    ..Default::default()
                },
            )
            .unwrap(),
            vec!["model-b"]
        );
        let allowed = groups(&["grp-alpha"]);
        let err = grouped_select(
            &snap,
            targets(),
            RoutingRequest {
                tags: &asked,
                allowed_groups: &allowed,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, ProxyError::AllCandidatesUnavailable { .. }),
            "{err:?}"
        );
        assert_names_no_group_or_member(&err);
    }

    /// The key's groups narrow the targets BEFORE the routing-tag filter.
    /// Inside `grp-alpha` no target carries the asked-for tag, so the tag
    /// filter falls back to the `default`-tagged target there. Filtering by
    /// tag first would pick the out-of-group target, then lose it to the
    /// group filter and refuse a request an in-group target can serve.
    #[test]
    fn groups_narrow_the_targets_before_the_tags_pick_among_them() {
        let snap = groups_snapshot();
        let allowed = groups(&["grp-alpha"]);
        let asked = groups(&["grp-beta"]);
        let order = grouped_select(
            &snap,
            vec![
                tagged("model-a", &["x"]),
                tagged("model-b", &["grp-beta"]),
                tagged("model-d", &["default"]),
            ],
            RoutingRequest {
                tags: &asked,
                allowed_groups: &allowed,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(order, vec!["model-d"]);
    }

    #[test]
    fn no_target_in_the_keys_groups_answers_like_all_unavailable() {
        let snap = groups_snapshot();
        let allowed = groups(&["grp-zeta"]);
        let err = grouped_select(
            &snap,
            every_grouped_target(),
            RoutingRequest {
                allowed_groups: &allowed,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ProxyError::AllCandidatesUnavailable {
                    retry_after_secs: None
                }
            ),
            "{err:?}"
        );
        assert_names_no_group_or_member(&err);
    }

    fn resolve_direct(
        snap: &AisixSnapshot,
        name: &str,
        allowed: &[String],
    ) -> Result<Vec<String>, ProxyError> {
        let entry = snap.models.get_by_name(name).unwrap();
        resolve_attempt_models(
            &RoutingRegistry::new(),
            &crate::ModelRuntimeStatusTracker::new(),
            &LivePricingIndex::new(),
            snap,
            name,
            &entry.id,
            &entry.value,
            RoutingRequest {
                allowed_groups: allowed,
                ..Default::default()
            },
        )
        .map(|attempts| {
            attempts
                .into_iter()
                .map(|a| a.model.display_name.clone())
                .collect()
        })
    }

    #[test]
    fn a_direct_model_outside_the_keys_groups_is_refused_the_same_way() {
        let snap = groups_snapshot();
        let allowed = groups(&["grp-alpha"]);
        assert_eq!(
            resolve_direct(&snap, "model-a", &allowed).unwrap(),
            ["model-a"]
        );
        let err = resolve_direct(&snap, "model-b", &allowed).unwrap_err();
        assert!(
            matches!(
                err,
                ProxyError::AllCandidatesUnavailable {
                    retry_after_secs: None
                }
            ),
            "{err:?}"
        );
        assert_names_no_group_or_member(&err);
        // A model on a provider key with no group is outside every group.
        assert!(matches!(
            resolve_direct(&snap, "model-none", &allowed),
            Err(ProxyError::AllCandidatesUnavailable { .. })
        ));
        assert_eq!(resolve_direct(&snap, "model-b", &[]).unwrap(), ["model-b"]);
    }

    /// A target written by `model_id` is judged by the provider key of the
    /// model that id resolves to, like one written by name.
    #[test]
    fn a_target_named_by_id_is_judged_by_its_own_provider_keys_groups() {
        let snap = groups_snapshot();
        let allowed = groups(&["grp-beta"]);
        let order = grouped_select(
            &snap,
            vec![
                RoutingTarget::by_id("id-model-a"),
                RoutingTarget::by_id("id-model-b"),
            ],
            RoutingRequest {
                allowed_groups: &allowed,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(order, vec!["model-b"]);
    }

    /// A model whose provider key cannot be found has no group to be in, so
    /// a grouped key never reaches it; an ungrouped key is not asked.
    #[test]
    fn target_in_groups_refuses_a_model_whose_provider_key_is_missing() {
        let snap = pk_snapshot(&[("orphan", "pk-id-gone")]);
        let model = snap.models.get_by_name("orphan").unwrap();
        assert!(!target_in_groups(
            &snap,
            &model.value,
            &groups(&["grp-alpha"])
        ));
        assert!(target_in_groups(&snap, &model.value, &[]));
    }

    /// An ensemble dispatches its own members, and those are not filtered by
    /// group, so to a grouped key the ensemble is a model that does not exist.
    #[test]
    fn a_key_with_groups_finds_no_ensemble_to_dispatch() {
        let snap = groups_snapshot();
        let ensemble: Model = serde_json::from_value(serde_json::json!({
            "display_name": "panel",
            "ensemble": {
                "panel": [{"model": "model-a"}],
                "judge": {"model": "model-a"},
            },
        }))
        .unwrap();
        snap.models
            .insert(aisix_core::ResourceEntry::new("id-panel", ensemble, 1));
        let err = resolve_direct(&snap, "panel", &groups(&["grp-alpha"])).unwrap_err();
        assert!(
            matches!(err, ProxyError::ModelNotFound(ref m) if m == "panel"),
            "{err:?}"
        );
        assert_eq!(resolve_direct(&snap, "panel", &[]).unwrap(), ["panel"]);
    }

    fn ip_snapshot(models: &[(&str, Option<Vec<&str>>)]) -> AisixSnapshot {
        let table = aisix_core::snapshot::ResourceTable::default();
        for (i, (name, cidrs)) in models.iter().enumerate() {
            let model: Model = serde_json::from_value(serde_json::json!({
                "display_name": name,
                "provider": "openai",
                "model_name": "up",
                "provider_key_id": "pk-1",
                "allowed_cidrs": cidrs,
            }))
            .unwrap();
            table.insert(aisix_core::ResourceEntry::new(format!("m-{i}"), model, 1));
        }
        AisixSnapshot {
            models: table,
            ..Default::default()
        }
    }

    #[test]
    fn ip_filter_drops_only_the_out_of_range_target() {
        let snap = ip_snapshot(&[("restricted", Some(vec!["10.0.0.0/8"])), ("open", None)]);
        let targets = vec![tagged("restricted", &[]), tagged("open", &[])];

        // In range → both stay candidates.
        assert_eq!(
            model_names(&targets_allowed_for_ip(
                &snap,
                std::borrow::Cow::Owned(targets.clone()),
                "10.1.2.3"
            )),
            vec!["restricted", "open"]
        );
        // Out of range → the restricted member drops out, the group still serves.
        assert_eq!(
            model_names(&targets_allowed_for_ip(
                &snap,
                std::borrow::Cow::Owned(targets),
                "8.8.8.8"
            )),
            vec!["open"]
        );
    }

    #[test]
    fn ip_filter_empties_when_every_target_excludes_the_caller() {
        // The caller turns an empty result into a 403 rather than dispatching.
        let snap = ip_snapshot(&[
            ("a", Some(vec!["10.0.0.0/8"])),
            ("b", Some(vec!["192.168.0.0/16"])),
        ]);
        let targets = vec![tagged("a", &[]), tagged("b", &[])];
        assert!(
            targets_allowed_for_ip(&snap, std::borrow::Cow::Owned(targets), "8.8.8.8").is_empty()
        );
    }

    #[test]
    fn ip_filter_fails_closed_on_an_unattributable_source_ip() {
        // Mirrors `Model::ip_allowed`: an empty/unparseable IP can never
        // satisfy a configured allowlist, so a request whose peer address
        // was lost must not reach a restricted target.
        let snap = ip_snapshot(&[("restricted", Some(vec!["10.0.0.0/8"]))]);
        let targets = vec![tagged("restricted", &[])];
        assert!(targets_allowed_for_ip(&snap, std::borrow::Cow::Owned(targets), "").is_empty());
    }

    #[test]
    fn ip_filter_keeps_unresolvable_names_for_the_config_error_path() {
        // A target naming a Model that isn't in the snapshot is counted as a
        // missing member further on, not silently swallowed here as an IP
        // rejection.
        let snap = ip_snapshot(&[("known", None)]);
        let targets = vec![tagged("ghost", &[])];
        assert_eq!(
            model_names(&targets_allowed_for_ip(
                &snap,
                std::borrow::Cow::Owned(targets),
                "8.8.8.8"
            )),
            vec!["ghost"]
        );
    }

    #[test]
    fn ip_filter_is_a_noop_when_no_target_restricts() {
        let snap = ip_snapshot(&[("a", None), ("b", None)]);
        let targets = vec![tagged("a", &[]), tagged("b", &[])];
        assert_eq!(
            model_names(&targets_allowed_for_ip(
                &snap,
                std::borrow::Cow::Owned(targets),
                "8.8.8.8"
            )),
            vec!["a", "b"]
        );
    }

    #[test]
    fn eligible_tagged_no_match_no_default_is_empty() {
        // The caller turns an empty result into a "no target matches tags" error.
        let targets = vec![tagged("eu", &["eu"]), tagged("us", &["us"])];
        assert!(eligible_targets(&targets, &["apac".into()]).is_empty());
    }

    #[test]
    fn failover_always_starts_at_index_zero() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::Failover,
            vec![
                RoutingTarget::new("primary"),
                RoutingTarget::new("secondary"),
                RoutingTarget::new("tertiary"),
            ],
            None,
        );
        for _ in 0..5 {
            let order = reg.pick_targets("v", &routing, &routing.targets, "");
            assert_eq!(order, vec!["primary", "secondary", "tertiary"]);
        }
    }

    #[test]
    fn round_robin_cycles_through_targets_per_call() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![
                RoutingTarget::new("a"),
                RoutingTarget::new("b"),
                RoutingTarget::new("c"),
            ],
            Some(1), // only the first attempt — easier to assert ordering
        );
        let mut firsts = Vec::new();
        for _ in 0..6 {
            let order = reg.pick_targets("v", &routing, &routing.targets, "");
            firsts.push(order[0].clone());
        }
        // Two full cycles of a→b→c.
        assert_eq!(firsts, vec!["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn round_robin_state_is_per_virtual_model() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
            Some(1),
        );
        // Two distinct virtual models advance independently.
        assert_eq!(
            reg.pick_targets("v1", &routing, &routing.targets, "")[0],
            "a"
        );
        assert_eq!(
            reg.pick_targets("v2", &routing, &routing.targets, "")[0],
            "a"
        );
        assert_eq!(
            reg.pick_targets("v1", &routing, &routing.targets, "")[0],
            "b"
        );
        assert_eq!(
            reg.pick_targets("v2", &routing, &routing.targets, "")[0],
            "b"
        );
    }

    #[test]
    fn fallback_walks_forward_with_wraparound() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![
                RoutingTarget::new("a"),
                RoutingTarget::new("b"),
                RoutingTarget::new("c"),
            ],
            Some(2),
        );
        // First call starts at a → a, b, c
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, ""),
            vec!["a", "b", "c"]
        );
        // Second call starts at b → b, c, a
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, ""),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn wrr_first_pick_prefers_the_heavier_weight_and_walks_forward() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![
                RoutingTarget::new("a").with_weight(99),
                RoutingTarget::new("b").with_weight(1),
            ],
            Some(1),
        );
        // Smooth WRR is deterministic: the first pick is the heavy target,
        // the walk continues in declaration order.
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, ""),
            vec!["a", "b"]
        );
    }

    #[test]
    fn wrr_distribution_matches_weights_exactly() {
        // Smooth WRR is exact, not stochastic: over one full cycle of
        // total-weight picks, each target is chosen exactly `weight` times.
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![
                RoutingTarget::new("a").with_weight(70),
                RoutingTarget::new("b").with_weight(30),
            ],
            Some(0),
        );
        let mut counts = [0usize; 2];
        for _ in 0..100 {
            match reg.pick_targets("v", &routing, &routing.targets, "")[0].as_str() {
                "a" => counts[0] += 1,
                _ => counts[1] += 1,
            }
        }
        assert_eq!(counts, [70, 30]);
    }

    #[test]
    fn wrr_interleaves_rather_than_bursting() {
        // The nginx smooth-WRR property: 2/1 yields a,b,a per cycle, not
        // a,a,b — heavier targets spread across the cycle.
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![
                RoutingTarget::new("a").with_weight(2),
                RoutingTarget::new("b").with_weight(1),
            ],
            Some(0),
        );
        let picks: Vec<String> = (0..6)
            .map(|_| {
                reg.pick_targets("v", &routing, &routing.targets, "")
                    .remove(0)
            })
            .collect();
        assert_eq!(picks, vec!["a", "b", "a", "a", "b", "a"]);
    }

    #[test]
    fn wrr_zero_weights_clamp_to_one() {
        // Candidates that are ALL at 0 reach the strategy (see the weight
        // filter in `resolve_attempt_models`): they have no share to compare, so each counts
        // as 1 rather than the rotation dividing by nothing.
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![
                RoutingTarget::new("a").with_weight(0),
                RoutingTarget::new("b").with_weight(0),
            ],
            Some(0),
        );
        let picks: Vec<String> = (0..4)
            .map(|_| {
                reg.pick_targets("v", &routing, &routing.targets, "")
                    .remove(0)
            })
            .collect();
        assert_eq!(picks, vec!["a", "b", "a", "b"]);
    }

    /// A subset is not a config change: only the configured list restarts it.
    #[test]
    fn wrr_state_survives_a_different_subset_of_the_same_config() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
            None,
        );
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, "")[0],
            "a"
        );
        let only_a = vec![RoutingTarget::new("a")];
        assert_eq!(reg.pick_targets("v", &routing, &only_a, "")[0], "a");
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, "")[0],
            "b"
        );
    }

    #[test]
    fn wrr_state_resets_when_the_tier_config_changes() {
        let reg = RoutingRegistry::new();
        let before = r(
            RoutingStrategy::RoundRobin,
            vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
            Some(0),
        );
        assert_eq!(reg.pick_targets("v", &before, &before.targets, "")[0], "a");
        assert_eq!(reg.pick_targets("v", &before, &before.targets, "")[0], "b");
        // New target list → fingerprint mismatch → rotation restarts.
        let after = r(
            RoutingStrategy::RoundRobin,
            vec![RoutingTarget::new("x"), RoutingTarget::new("y")],
            Some(0),
        );
        assert_eq!(reg.pick_targets("v", &after, &after.targets, "")[0], "x");
    }

    #[test]
    fn priority_tiers_concatenate_highest_first() {
        // The A/B two-pool shape from AISIX-Cloud#1206: priority 0 is the
        // active pool, priority -1 the backup; the walk exhausts the whole
        // active tier before any backup target.
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::Failover,
            vec![
                RoutingTarget::new("b1").with_priority(-1),
                RoutingTarget::new("a1"),
                RoutingTarget::new("a2"),
                RoutingTarget::new("b2").with_priority(-1),
            ],
            None,
        );
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, ""),
            vec!["a1", "a2", "b1", "b2"]
        );
    }

    #[test]
    fn priority_tiers_run_the_strategy_per_tier() {
        // Each tier owns its own WRR rotation (the APISIX per-priority
        // picker rule): the backup tier's order is its own strategy pick,
        // not a continuation of the active tier's.
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::RoundRobin,
            vec![
                RoutingTarget::new("a1"),
                RoutingTarget::new("a2"),
                RoutingTarget::new("b1").with_priority(-1),
                RoutingTarget::new("b2").with_priority(-1),
            ],
            None,
        );
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, ""),
            vec!["a1", "a2", "b1", "b2"]
        );
        // Second call: both tiers advanced their own rotation.
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, ""),
            vec!["a2", "a1", "b2", "b1"]
        );
    }

    #[test]
    fn priority_tiers_chash_hashes_within_each_tier() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::ConsistentHash,
            vec![
                RoutingTarget::new("a1"),
                RoutingTarget::new("a2"),
                RoutingTarget::new("a3"),
                RoutingTarget::new("b1").with_priority(-1),
                RoutingTarget::new("b2").with_priority(-1),
            ],
            None,
        );
        let order = reg.pick_targets("v", &routing, &routing.targets, "session-7");
        assert_eq!(order.len(), 5);
        // Every active-tier target precedes every backup target.
        let a_positions: Vec<usize> = order
            .iter()
            .enumerate()
            .filter(|(_, t)| t.starts_with('a'))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(a_positions, vec![0, 1, 2]);
        // Deterministic per key.
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, "session-7"),
            order
        );
        // A different key may start elsewhere but keeps the tier boundary.
        let other = reg.pick_targets("v", &routing, &routing.targets, "session-8");
        assert!(other[..3].iter().all(|t| t.starts_with('a')));
    }

    #[test]
    fn max_fallbacks_caps_across_tiers() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::Failover,
            vec![
                RoutingTarget::new("a1"),
                RoutingTarget::new("a2"),
                RoutingTarget::new("b1").with_priority(-1),
            ],
            Some(1),
        );
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, ""),
            vec!["a1", "a2"]
        );
    }

    #[test]
    fn max_fallbacks_zero_disables_failover() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::Failover,
            vec![RoutingTarget::new("a"), RoutingTarget::new("b")],
            Some(0),
        );
        let order = reg.pick_targets("v", &routing, &routing.targets, "");
        assert_eq!(order, vec!["a"]);
    }

    #[test]
    fn empty_targets_yields_empty_order() {
        let reg = RoutingRegistry::new();
        let routing = r(RoutingStrategy::Failover, vec![], None);
        assert!(reg
            .pick_targets("v", &routing, &routing.targets, "")
            .is_empty());
    }

    #[test]
    fn is_retryable_distinguishes_4xx_from_other_failures() {
        assert!(!is_retryable(
            &BridgeError::upstream_status(400, "bad request"),
            false,
            &[]
        ));
        assert!(!is_retryable(
            &BridgeError::upstream_status(429, "rate limited"),
            false,
            &[]
        ));
        assert!(is_retryable(
            &BridgeError::upstream_status(429, "rate limited"),
            true,
            &[]
        ));
        assert!(is_retryable(
            &BridgeError::upstream_status(502, "bad gateway"),
            false,
            &[]
        ));
        assert!(is_retryable(
            &BridgeError::Timeout {
                cause: String::new(),
                elapsed_ms: 1
            },
            false,
            &[]
        ));
        assert!(is_retryable(
            &BridgeError::Transport("conn".into()),
            false,
            &[]
        ));
        assert!(!is_retryable(
            &BridgeError::UpstreamDecode("x".into()),
            false,
            &[]
        ));
        // A credential exchange answered unreadably: nothing was served.
        assert!(is_retryable(
            &BridgeError::TokenExchangeDecode("x".into()),
            false,
            &[]
        ));
        assert!(is_retryable(
            &BridgeError::Config("bad key".into()),
            false,
            &[]
        ));
        assert!(is_retryable(&BridgeError::StreamAborted, false, &[]));
        // #367: customer-fixable config is a 4xx — not retryable.
        assert!(!is_retryable(
            &BridgeError::InvalidUpstreamConfig("no api_base".into()),
            false,
            &[]
        ));
        // #1093: the adapter simply does not implement this operation, and
        // that is static — the same call answers the same way every time.
        assert!(!is_retryable(
            &BridgeError::UnsupportedCapability(BridgeCapability::TextCompletions),
            false,
            &[]
        ));
    }

    /// The classifier answering `false` is only worth something if the loop
    /// stops on it. This is the half that would go red if
    /// `retrying_dispatch` ever consulted something other than
    /// `is_retryable` — and the `Config` control is what shows the harness
    /// can see a retry at all, so a mis-wired counter cannot pass by
    /// reporting one call for both.
    #[tokio::test(start_paused = true)]
    async fn the_retry_loop_spends_no_attempt_on_a_capability_gap() {
        let state = crate::ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(aisix_core::AisixSnapshot::new()),
            std::sync::Arc::new(aisix_gateway::Hub::new()),
            &aisix_core::ProxyConfig {
                addr: "127.0.0.1:0".into(),
                request_body_limit_bytes: 1_048_576,
                tls: None,
                listeners: Vec::new(),
                real_ip: Default::default(),
                request_id: Default::default(),
                thread_per_core: None,
                workers: None,
                url_rewrites: Vec::new(),
                expose_debug_headers: false,
                platform: Default::default(),
            },
        );
        let model = model_with_retries(Some(2));

        let calls = std::cell::Cell::new(0u32);
        let err = retrying_dispatch(&state, &model, "/v1/completions", || {
            calls.set(calls.get() + 1);
            async {
                Err::<(), _>(BridgeError::UnsupportedCapability(
                    BridgeCapability::TextCompletions,
                ))
            }
        })
        .await
        .expect_err("the capability gap surfaces");
        assert!(matches!(err, BridgeError::UnsupportedCapability(_)));
        assert_eq!(calls.get(), 1, "a capability gap must not spend a retry");

        // Control: the shape this used to have. Two configured retries mean
        // three calls, which is the budget the 501 was burning before the
        // variant was typed.
        let calls = std::cell::Cell::new(0u32);
        let _ = retrying_dispatch(&state, &model, "/v1/completions", || {
            calls.set(calls.get() + 1);
            async { Err::<(), _>(BridgeError::Config("serialize request body: eof".into())) }
        })
        .await;
        assert_eq!(calls.get(), 3);
    }

    /// #1222: in-band stream errors follow the same status
    /// rules as HTTP status errors; a status-less one is treated as a
    /// transient fault (retryable).
    #[test]
    fn is_retryable_classifies_in_band_errors_by_embedded_status() {
        let in_band = |status: Option<u16>| BridgeError::UpstreamInBand {
            status,
            message: "m".into(),
            parsed: None,
            wire: aisix_gateway::UpstreamWire::OpenAI,
        };
        assert!(is_retryable(&in_band(Some(500)), false, &[]));
        assert!(is_retryable(&in_band(Some(529)), false, &[]));
        assert!(!is_retryable(&in_band(Some(400)), false, &[]));
        assert!(!is_retryable(&in_band(Some(429)), false, &[]));
        assert!(is_retryable(&in_band(Some(429)), true, &[]));
        // fallback_on_statuses admits listed in-band codes too.
        assert!(is_retryable(&in_band(Some(408)), false, &[408]));
        assert!(!is_retryable(&in_band(Some(408)), false, &[]));
        assert!(is_retryable(&in_band(None), false, &[]));
    }

    /// #1012: `fallback_on_statuses` opts specific upstream
    /// status codes into retry/failover. The list is additive — codes not
    /// listed keep the default classification — and it never resurrects
    /// non-status failures (customer-fixable config stays terminal).
    #[test]
    fn fallback_on_statuses_opts_specific_codes_into_retry() {
        // A listed 4xx becomes retryable.
        assert!(is_retryable(
            &BridgeError::upstream_status(408, "request timeout"),
            false,
            &[408, 409]
        ));
        assert!(is_retryable(
            &BridgeError::upstream_status(409, "conflict"),
            false,
            &[408, 409]
        ));
        // Codes NOT in the list keep the default: terminal.
        assert!(!is_retryable(
            &BridgeError::upstream_status(422, "unprocessable"),
            false,
            &[408, 409]
        ));
        assert!(!is_retryable(
            &BridgeError::upstream_status(400, "bad request"),
            false,
            &[408, 409]
        ));
        // 429 in the list works without retry_on_429.
        assert!(is_retryable(
            &BridgeError::upstream_status(429, "rate limited"),
            false,
            &[429]
        ));
        // 5xx stays retryable whether or not listed.
        assert!(is_retryable(
            &BridgeError::upstream_status(503, "unavailable"),
            false,
            &[408]
        ));
        // The list is status-scoped: it never affects non-status errors.
        assert!(!is_retryable(
            &BridgeError::InvalidUpstreamConfig("no api_base".into()),
            false,
            &[400, 401, 403]
        ));
    }

    // ── retry_backoff ─────────────────────────────────────────────
    #[test]
    fn retry_backoff_zero_is_no_wait() {
        assert_eq!(retry_backoff(0, None), Duration::ZERO);
    }

    #[test]
    fn retry_backoff_grows_exponentially_and_caps() {
        // The exponential FLOOR (delay minus the additive jitter) must be
        // base*2^(retry-1), capped. Sample many times: the minimum observed
        // delay tracks the floor and never exceeds floor + jitter ceiling.
        let cases = [
            (1u32, 250u64), // 250 * 2^0
            (2, 500),       // 250 * 2^1
            (3, 1000),      // 250 * 2^2
            (4, 2000),      // 250 * 2^3 = 2000 (== cap)
            (5, 2000),      // capped
            (50, 2000),     // capped, no overflow
        ];
        for (retry, floor) in cases {
            let mut min = u64::MAX;
            let mut max = 0u64;
            for _ in 0..2000 {
                let ms = retry_backoff(retry, None).as_millis() as u64;
                min = min.min(ms);
                max = max.max(ms);
            }
            assert!(min >= floor, "retry {retry}: min {min} < floor {floor}");
            assert!(
                max <= floor + 250,
                "retry {retry}: max {max} > floor {floor} + jitter 250",
            );
        }
    }

    #[test]
    fn retry_backoff_honours_a_sane_retry_after() {
        // A provider-supplied hint inside the honour window wins over the
        // exponential term, even when the exponential term would be shorter
        // (retry 1 → 250ms floor, hint → 3000ms).
        let mut min = u64::MAX;
        for _ in 0..500 {
            let ms = retry_backoff(1, Some(Duration::from_millis(3_000))).as_millis() as u64;
            min = min.min(ms);
            assert!((3_000..=3_250).contains(&ms), "hint not honoured: {ms}ms");
        }
        assert!(min >= 3_000);
    }

    #[test]
    fn retry_backoff_ignores_an_out_of_range_retry_after() {
        // Above the honour ceiling we fall back to our own exponential term
        // rather than parking the caller's request for a minute. A zero hint
        // is meaningless and falls back too.
        for hint in [Duration::from_secs(60), Duration::ZERO] {
            let ms = retry_backoff(1, Some(hint)).as_millis() as u64;
            assert!(
                (250..=500).contains(&ms),
                "expected the exponential term for hint {hint:?}, got {ms}ms",
            );
        }
    }

    // ── effective_retries ─────────────────────────────────────────
    fn model_with_retries(retries: Option<u32>) -> Model {
        let mut m: Model = serde_json::from_str(
            r#"{"display_name":"m","provider":"openai","model_name":"gpt-4o","provider_key_id":"pk"}"#,
        )
        .unwrap();
        m.retries = retries;
        m
    }

    fn group_with_retries(retries: Option<u32>) -> aisix_core::models::routing::Routing {
        let mut r: aisix_core::models::routing::Routing =
            serde_json::from_str(r#"{"targets":[{"model":"a"}]}"#).unwrap();
        r.retries = retries;
        r
    }

    /// `budget(target, group, default, has_fallback)` — reads better than
    /// four positional args repeated in every assertion below.
    fn budget(
        target: Option<u32>,
        group: Option<Option<u32>>,
        default: u32,
        has_fallback: bool,
    ) -> RetryBudget {
        let m = model_with_retries(target);
        match group {
            Some(g) => effective_retries(
                &m,
                group_retries_of(&{
                    let mut parent = model_with_retries(None);
                    parent.routing = Some(group_with_retries(g));
                    parent
                }),
                default,
                has_fallback,
            ),
            None => effective_retries(&m, None, default, has_fallback),
        }
    }

    #[test]
    fn group_retries_reads_the_routing_block_then_the_parent_model() {
        // Model Group: the group slot is `routing.retries`; a stray
        // top-level value on the parent stays shadowed by it.
        let mut group_parent = model_with_retries(Some(7));
        group_parent.routing = Some(group_with_retries(Some(3)));
        assert_eq!(group_retries_of(&group_parent), Some(3));
        // …and stays INERT even when `routing.retries` is unset — the
        // routing block's presence pins the group slot, so the target →
        // routing.retries → deployment-default chain is unchanged for
        // every Model Group shape.
        let mut sparse_group = model_with_retries(Some(7));
        sparse_group.routing = Some(group_with_retries(None));
        assert_eq!(group_retries_of(&sparse_group), None);
        // Semantic router (no routing block): the parent's own top-level
        // `retries` IS the group slot — the member → group → default
        // chain unified across virtual parents.
        let semantic_parent = model_with_retries(Some(2));
        assert_eq!(group_retries_of(&semantic_parent), Some(2));
        assert_eq!(
            effective_retries(
                &model_with_retries(None),
                group_retries_of(&semantic_parent),
                9,
                false
            )
            .attempts,
            2
        );
        // Neither configured → no group level.
        assert_eq!(group_retries_of(&model_with_retries(None)), None);
    }

    #[test]
    fn effective_retries_prefers_the_target_then_the_group_then_the_default() {
        // Target wins over group.
        assert_eq!(budget(Some(1), Some(Some(5)), 2, false).attempts, 1);
        // Group applies when the target is silent.
        assert_eq!(budget(None, Some(Some(5)), 2, false).attempts, 5);
        // Deployment default applies when both are silent.
        assert_eq!(budget(None, Some(None), 2, false).attempts, 2);
        // A direct model has no group at all — the case that used to be
        // hardcoded to zero.
        assert_eq!(budget(None, None, 2, false).attempts, 2);
    }

    #[test]
    fn effective_retries_honours_an_explicit_zero_at_every_level() {
        // `Some(0)` is an opt-out, not "unset" — it must not fall through to
        // the next level, or an operator could never turn retrying off.
        assert_eq!(budget(Some(0), Some(Some(5)), 2, false).attempts, 0);
        assert_eq!(budget(None, Some(Some(0)), 2, false).attempts, 0);
        assert_eq!(budget(None, None, 0, false).attempts, 0);
    }

    #[test]
    fn effective_retries_default_defers_to_a_fallback_target() {
        // Nothing configured + another target queued behind this one: prefer
        // failing over to grinding a failing upstream. This is what keeps the
        // default from tripling the latency of `timeout`-driven fail-over
        // (#554) — and it matches a mainstream gateway, whose retries re-enter deployment
        // selection rather than re-hitting the same deployment.
        assert_eq!(budget(None, None, 2, true).attempts, 0);
        assert_eq!(budget(None, Some(None), 2, true).attempts, 0);
        // The LAST target has nothing to fall over to, so the default applies
        // there — the request still gets its retries before giving up.
        assert_eq!(budget(None, Some(None), 2, false).attempts, 2);
    }

    #[test]
    fn effective_retries_explicit_config_beats_the_fallback_heuristic() {
        // The heuristic only gates the DEFAULT. An operator who asked for
        // same-target retries gets them even with fallbacks queued up.
        assert_eq!(budget(Some(3), None, 2, true).attempts, 3);
        assert_eq!(budget(None, Some(Some(3)), 2, true).attempts, 3);
    }

    // ── effective_timeouts ────────────────────────────────────────
    fn model_with_timeouts(timeout: Option<u64>, stream_timeout: Option<u64>) -> Model {
        let mut m: Model = serde_json::from_str(
            r#"{"display_name":"m","provider":"openai","model_name":"gpt-4o","provider_key_id":"pk"}"#,
        )
        .unwrap();
        m.timeout = timeout;
        m.stream_timeout = stream_timeout;
        m
    }

    fn defaults_ms(request: Option<u64>, stream: Option<u64>) -> TimeoutDefaults {
        TimeoutDefaults {
            request: request.map(std::time::Duration::from_millis),
            stream: stream.map(std::time::Duration::from_millis),
        }
    }

    fn ms(v: u64) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(v))
    }

    #[test]
    fn effective_timeouts_prefers_the_target_then_the_group_then_the_default() {
        let group = model_with_timeouts(Some(2_000), Some(1_500));
        // Target wins over group and default.
        let t = effective_timeouts(
            &model_with_timeouts(Some(1_000), Some(500)),
            Some(&group),
            defaults_ms(Some(9_000), Some(8_000)),
        );
        assert_eq!(t.request, ms(1_000));
        assert_eq!(t.stream, ms(500));
        // Group applies when the target is silent.
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            Some(&group),
            defaults_ms(Some(9_000), Some(8_000)),
        );
        assert_eq!(t.request, ms(2_000));
        assert_eq!(t.stream, ms(1_500));
        // Deployment default applies when both are silent — the case that
        // used to mean "no deadline at all".
        let silent_group = model_with_timeouts(None, None);
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            Some(&silent_group),
            defaults_ms(Some(9_000), Some(8_000)),
        );
        assert_eq!(t.request, ms(9_000));
        assert_eq!(t.stream, ms(8_000));
        // A direct model has no group at all.
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            None,
            defaults_ms(Some(9_000), None),
        );
        assert_eq!(t.request, ms(9_000));
    }

    #[test]
    fn effective_timeouts_explicit_zero_disables_and_stops_the_chain() {
        // `timeout: 0` on the model is an opt-out of the deployment
        // backstop, not "unset" — a long-running model must be able to
        // escape the default.
        let t = effective_timeouts(
            &model_with_timeouts(Some(0), None),
            None,
            defaults_ms(Some(9_000), None),
        );
        assert_eq!(t.request, None);
        assert_eq!(t.stream, None);
        // Same at group level.
        let group_zero = model_with_timeouts(Some(0), None);
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            Some(&group_zero),
            defaults_ms(Some(9_000), None),
        );
        assert_eq!(t.request, None);
        // `upstream.timeout_ms: 0` restores the pre-default behaviour.
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            None,
            defaults_ms(None, None),
        );
        assert_eq!(t.request, None);
        assert_eq!(t.stream, None);
    }

    #[test]
    fn effective_timeouts_stream_zero_defers_and_falls_back_to_request() {
        // `stream_timeout: 0`/absent defers (its historical semantics),
        // ending at the resource-resolved request timeout.
        let t = effective_timeouts(
            &model_with_timeouts(Some(5_000), Some(0)),
            None,
            defaults_ms(Some(9_000), None),
        );
        assert_eq!(t.stream, ms(5_000));
        // Resource config beats deployment config: a model `timeout` wins
        // over the deployment stream default.
        let t = effective_timeouts(
            &model_with_timeouts(Some(5_000), None),
            None,
            defaults_ms(Some(9_000), Some(700)),
        );
        assert_eq!(t.stream, ms(5_000));
        // With no resource-level timeouts, the deployment stream default
        // applies, falling back to the deployment request default.
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            None,
            defaults_ms(Some(9_000), Some(700)),
        );
        assert_eq!(t.stream, ms(700));
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            None,
            defaults_ms(Some(9_000), None),
        );
        assert_eq!(t.stream, ms(9_000));
    }

    #[test]
    fn effective_timeouts_only_resource_config_arms_the_first_chunk_peek() {
        // Deployment-default budgets must not withhold the 200 waiting for
        // the first chunk — that would silence the SSE heartbeats that
        // cover a slow first token (#1126).
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            None,
            defaults_ms(Some(9_000), Some(700)),
        );
        assert!(!t.stream_configured);
        // A model/group streaming budget — or a model `timeout` acting as
        // one — is an explicit ask for slow-first-token failover (#554).
        let t = effective_timeouts(
            &model_with_timeouts(None, Some(700)),
            None,
            defaults_ms(Some(9_000), None),
        );
        assert!(t.stream_configured);
        let t = effective_timeouts(
            &model_with_timeouts(Some(5_000), None),
            None,
            defaults_ms(None, None),
        );
        assert!(t.stream_configured);
        let group = model_with_timeouts(None, Some(700));
        let t = effective_timeouts(
            &model_with_timeouts(None, None),
            Some(&group),
            defaults_ms(Some(9_000), None),
        );
        assert!(t.stream_configured);
        // `timeout: 0` disarms everything.
        let t = effective_timeouts(
            &model_with_timeouts(Some(0), None),
            None,
            defaults_ms(Some(9_000), None),
        );
        assert!(!t.stream_configured);
        assert_eq!(t.stream, None);
    }

    #[test]
    fn effective_timeouts_stream_knob_outranks_the_timeout_knob_across_levels() {
        // The dedicated stream knob wins at every level: a group
        // `stream_timeout` beats a member's own `timeout` for the
        // streaming budget (the member's `timeout` still governs its
        // non-streaming deadline). A mainstream gateway resolves the same way — the
        // stream chain is exhausted before the non-stream chain starts.
        let group = model_with_timeouts(None, Some(700));
        let t = effective_timeouts(
            &model_with_timeouts(Some(5_000), None),
            Some(&group),
            defaults_ms(Some(9_000), None),
        );
        assert_eq!(t.request, ms(5_000));
        assert_eq!(t.stream, ms(700));
        assert!(t.stream_configured);
        // ...including a member that opted OUT of the request deadline:
        // `timeout: 0` cannot cancel a group's explicit stream budget —
        // only the dedicated knob governs the dedicated budget.
        let t = effective_timeouts(
            &model_with_timeouts(Some(0), None),
            Some(&group),
            defaults_ms(Some(9_000), None),
        );
        assert_eq!(t.request, None);
        assert_eq!(t.stream, ms(700));
        assert!(t.stream_configured);
    }

    #[test]
    fn a_default_budget_does_not_spend_itself_on_a_timeout() {
        let timeout = BridgeError::Timeout {
            elapsed_ms: 7_000,
            cause: String::new(),
        };
        let server_error = BridgeError::upstream_status(503, "unavailable");

        // Unconfigured: a timeout must not be re-hit on the same target —
        // the operator bounded that wait on purpose, and tripling it is the
        // opposite of what `timeout` asks for. Transient 5xx still retries.
        let default = budget(None, None, 2, false);
        assert!(!default.covers(&timeout));
        assert!(default.covers(&server_error));

        // Configured: the operator named the number, so it applies to
        // everything retryable, timeouts included.
        let configured = budget(Some(2), None, 2, false);
        assert!(configured.covers(&timeout));
        assert!(configured.covers(&server_error));
        // ...including when it came from the group.
        assert!(budget(None, Some(Some(2)), 2, false).covers(&timeout));
    }

    // ── filter_attempt_models ─────────────────────────────────────
    fn am(id: &str) -> AttemptModel {
        let model: Model = serde_json::from_str(&format!(
            r#"{{
              "display_name": "{id}",
              "provider": "openai",
              "model_name": "gpt-4o-mini",
              "provider_key_id": "pk-{id}"
            }}"#
        ))
        .unwrap();
        AttemptModel {
            id: id.to_string(),
            model: std::sync::Arc::new(model),
            priority: 0,
            weight: 1,
        }
    }

    // ── order_attempts_by_metric (least_cost) ─────────────────────

    /// A snapshot with no pricing documents — the cases below rank by
    /// the models' own inline `cost`, which is what every deployment
    /// written before pricing documents existed still does.
    fn unpriced() -> AisixSnapshot {
        AisixSnapshot::new()
    }

    /// A snapshot whose shared catalog prices `key` at `input`/`output`.
    fn priced(key: &str, input: f64, output: f64) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.global_pricing.insert(aisix_core::ResourceEntry::new(
            "p-1",
            serde_json::from_str(&format!(
                r#"{{"key":"{key}","input_per_1k":{input},"output_per_1k":{output}}}"#
            ))
            .unwrap(),
            1,
        ));
        snap
    }

    /// A target priced only by reference — no inline `cost` to fall back
    /// on, so a resolution that does not happen ranks it last.
    fn am_with_pricing_key(id: &str, key: &str) -> AttemptModel {
        let model: Model = serde_json::from_str(&format!(
            r#"{{
              "display_name": "{id}",
              "provider": "openai",
              "model_name": "gpt-4o-mini",
              "provider_key_id": "pk-{id}",
              "pricing_key": "{key}"
            }}"#
        ))
        .unwrap();
        AttemptModel {
            id: id.to_string(),
            model: std::sync::Arc::new(model),
            priority: 0,
            weight: 1,
        }
    }

    /// Every reader of a model's price must go through
    /// [`PricingIndex::resolve`], so ranking and billing cannot disagree
    /// about what a model costs.
    ///
    /// A census rather than a list of the three known sites, for the
    /// reason `guardrail_coverage.rs` gives: the readers here come in a
    /// family (`least_cost` ordering, the realtime session's `cost_usd`,
    /// the batch attribution's), a fourth is added by writing one more
    /// `.cost`, and a hand-written list agrees with itself forever. The
    /// symptom of missing one is silent — a model priced by reference
    /// bills zero on the site that still reads the field.
    #[test]
    fn no_one_reads_a_models_price_off_the_field() {
        use std::path::Path;

        /// The two places allowed to touch the field, by PATH rather
        /// than by file name: excluding a bare name would also excuse a
        /// same-named file in another crate, and excluding a whole file
        /// is how the first version of this census stopped covering
        /// `cost_key`.
        const AUTHORIZED: [&str; 2] = [
            // Defines `ModelCost` and clears it in strip_kind_inapplicable.
            "aisix-core/src/models/model.rs",
            // The resolver every other reader must go through.
            "aisix-core/src/models/pricing.rs",
        ];
        /// Sources whose own `cost` is another price: the control plane's
        /// pricing engine calls the platform's cost price `cost` (its price
        /// rows, its lookup, the cost on a priced row), and it takes no
        /// `Model`, so it cannot read a model's price off the field. Its
        /// `tests/` stay scanned.
        const OWN_COST_VOCABULARY: [&str; 1] = ["/crates/aisix-pricing/src/"];

        fn walk(dir: &Path, needle: &str, out: &mut Vec<(String, usize, String)>) {
            for e in std::fs::read_dir(dir).expect("crates dir is readable") {
                let path = e.expect("dir entry").path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    walk(&path, needle, out);
                } else if path.extension().is_some_and(|x| x == "rs") {
                    let name = path.to_string_lossy().replace('\\', "/");
                    if AUTHORIZED.iter().any(|ok| name.ends_with(ok))
                        || OWN_COST_VOCABULARY.iter().any(|dir| name.contains(dir))
                    {
                        continue;
                    }
                    let src = std::fs::read_to_string(&path).expect("source is utf-8");
                    for (i, line) in src.lines().enumerate() {
                        // `.cost` as a field access, not `cost_usd` /
                        // `cost_saved_usd` / a local named `*_cost`, and
                        // not a comment.
                        let trimmed = line.trim_start();
                        if trimmed.starts_with("//") || trimmed.starts_with("///") {
                            continue;
                        }
                        let mut rest = line;
                        while let Some(at) = rest.find(needle) {
                            let after = &rest[at + needle.len()..];
                            let boundary = after
                                .chars()
                                .next()
                                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
                            if boundary {
                                out.push((name.clone(), i + 1, line.trim().to_string()));
                                break;
                            }
                            rest = after;
                        }
                    }
                }
            }
        }

        // Assembled rather than written out, so this file is scanned
        // like every other one: excluding it to stop the census matching
        // its own text would also stop it covering `cost_key`, which
        // lives here and is the reader most likely to regress.
        let needle = format!(".{}", "cost");
        let mut hits = Vec::new();
        // The whole workspace, not just this crate: a usage event is
        // assembled in more than one of them, and a direct read added
        // anywhere else would be just as silent.
        let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the crate lives under crates/")
            .to_path_buf();
        walk(&crates, &needle, &mut hits);
        assert!(
            hits.is_empty(),
            "these read a model's price off the field instead of through \
             PricingIndex::resolve, so `pricing_key` is ignored there: {hits:#?}",
        );
        // The census is worthless if it scans nothing; prove it reached
        // the crates it is meant to cover.
        assert!(
            crates.join("aisix-server/src").is_dir() && crates.join("aisix-obs/src").is_dir(),
            "the census did not reach the other crates: {}",
            crates.display(),
        );
    }

    #[test]
    fn least_cost_ranks_a_referenced_price_against_an_inline_one() {
        let t = crate::ModelRuntimeStatusTracker::new();
        // The referenced price (2/1K) undercuts the inline one (6/1K).
        // Reversed against the same models with no catalog, below, so
        // neither ordering can be the accidental one.
        let snap = priced("vendor/x", 1.0, 1.0);
        let mut attempts = vec![
            am_with_cost("inline", 3.0, 3.0),
            am_with_pricing_key("referenced", "vendor/x"),
        ];
        order_attempts_by_metric(
            RoutingStrategy::LeastCost,
            &mut attempts,
            &t,
            &snap,
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["referenced", "inline"]);
    }

    #[test]
    fn least_cost_ranks_an_unresolved_reference_last() {
        let t = crate::ModelRuntimeStatusTracker::new();
        // Same two targets, no catalog: the reference resolves to
        // nothing and the model carries no inline cost, so it is +∞ and
        // sorts behind the priced one.
        let mut attempts = vec![
            am_with_pricing_key("referenced", "vendor/x"),
            am_with_cost("inline", 3.0, 3.0),
        ];
        order_attempts_by_metric(
            RoutingStrategy::LeastCost,
            &mut attempts,
            &t,
            &unpriced(),
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["inline", "referenced"]);
    }

    fn am_with_cost(id: &str, input_per_1k: f64, output_per_1k: f64) -> AttemptModel {
        let model: Model = serde_json::from_str(&format!(
            r#"{{
              "display_name": "{id}",
              "provider": "openai",
              "model_name": "gpt-4o-mini",
              "provider_key_id": "pk-{id}",
              "cost": {{ "input_per_1k": {input_per_1k}, "output_per_1k": {output_per_1k} }}
            }}"#
        ))
        .unwrap();
        AttemptModel {
            id: id.to_string(),
            model: std::sync::Arc::new(model),
            priority: 0,
            weight: 1,
        }
    }

    #[test]
    fn least_cost_orders_cheapest_first() {
        let t = crate::ModelRuntimeStatusTracker::new();
        let mut attempts = vec![
            am_with_cost("pricey", 10.0, 20.0), // 30 / 1K
            am_with_cost("cheap", 1.0, 2.0),    // 3 / 1K
            am_with_cost("mid", 5.0, 5.0),      // 10 / 1K
        ];
        order_attempts_by_metric(
            RoutingStrategy::LeastCost,
            &mut attempts,
            &t,
            &unpriced(),
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["cheap", "mid", "pricey"]);
    }

    #[test]
    fn least_cost_ranks_missing_cost_last_and_stably() {
        let t = crate::ModelRuntimeStatusTracker::new();
        let mut attempts = vec![
            am("no-cost-a"),                 // +∞
            am_with_cost("cheap", 1.0, 1.0), // 2 / 1K
            am("no-cost-b"),                 // +∞
        ];
        order_attempts_by_metric(
            RoutingStrategy::LeastCost,
            &mut attempts,
            &t,
            &unpriced(),
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        // Priced target first; equal (missing-cost) targets keep their
        // declaration order thanks to the stable sort.
        assert_eq!(ids, vec!["cheap", "no-cost-a", "no-cost-b"]);
    }

    #[test]
    fn non_metric_strategy_leaves_order_untouched() {
        let t = crate::ModelRuntimeStatusTracker::new();
        let mut attempts = vec![am_with_cost("b", 9.0, 9.0), am_with_cost("a", 1.0, 1.0)];
        order_attempts_by_metric(
            RoutingStrategy::Failover,
            &mut attempts,
            &t,
            &unpriced(),
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    // ── order_attempts_by_metric (least_latency) ──────────────────
    #[test]
    fn least_latency_orders_fastest_first() {
        let t = crate::ModelRuntimeStatusTracker::new();
        t.record_latency("slow", 900);
        t.record_latency("fast", 50);
        t.record_latency("mid", 300);
        let mut attempts = vec![am("slow"), am("fast"), am("mid")];
        order_attempts_by_metric(
            RoutingStrategy::LeastLatency,
            &mut attempts,
            &t,
            &unpriced(),
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["fast", "mid", "slow"]);
    }

    #[test]
    fn least_latency_probes_unmeasured_targets_first() {
        let t = crate::ModelRuntimeStatusTracker::new();
        t.record_latency("measured", 100);
        // "unseen-a"/"unseen-b" have no samples → rank first (−∞), keeping
        // their declaration order via the stable sort.
        let mut attempts = vec![am("measured"), am("unseen-a"), am("unseen-b")];
        order_attempts_by_metric(
            RoutingStrategy::LeastLatency,
            &mut attempts,
            &t,
            &unpriced(),
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["unseen-a", "unseen-b", "measured"]);
    }

    #[test]
    fn record_latency_ewma_tracks_recent_samples() {
        let t = crate::ModelRuntimeStatusTracker::new();
        assert_eq!(t.latency_ewma_ms("m"), None);
        t.record_latency("m", 100);
        assert_eq!(t.latency_ewma_ms("m"), Some(100.0)); // first sample seeds
        t.record_latency("m", 200);
        // 0.3*200 + 0.7*100 = 130
        assert!((t.latency_ewma_ms("m").unwrap() - 130.0).abs() < 1e-9);
    }

    // ── order_attempts_by_metric (least_busy) ─────────────────────
    #[test]
    fn least_busy_orders_least_loaded_first() {
        let t = crate::ModelRuntimeStatusTracker::new();
        let _b1 = t.begin_in_flight("busy");
        let _b2 = t.begin_in_flight("busy"); // 2 in-flight
        let _m1 = t.begin_in_flight("mid"); // 1 in-flight
                                            // "idle" has 0 in-flight.
        let mut attempts = vec![am("busy"), am("idle"), am("mid")];
        order_attempts_by_metric(
            RoutingStrategy::LeastBusy,
            &mut attempts,
            &t,
            &unpriced(),
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["idle", "mid", "busy"]);
    }

    #[test]
    fn least_busy_cold_start_keeps_declaration_order() {
        let t = crate::ModelRuntimeStatusTracker::new();
        // All idle (0 in-flight) → stable sort preserves declaration order.
        let mut attempts = vec![am("a"), am("b"), am("c")];
        order_attempts_by_metric(
            RoutingStrategy::LeastBusy,
            &mut attempts,
            &t,
            &unpriced(),
            &LivePricingIndex::new(),
        );
        let ids: Vec<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn in_flight_guard_increments_then_decrements_on_drop() {
        let t = crate::ModelRuntimeStatusTracker::new();
        assert_eq!(t.in_flight("m"), 0);
        let g1 = t.begin_in_flight("m");
        assert_eq!(t.in_flight("m"), 1);
        let g2 = t.begin_in_flight("m");
        assert_eq!(t.in_flight("m"), 2);
        drop(g1);
        assert_eq!(t.in_flight("m"), 1);
        drop(g2);
        assert_eq!(t.in_flight("m"), 0);
    }

    #[test]
    fn metric_strategy_pick_targets_returns_full_declaration_order() {
        let reg = RoutingRegistry::new();
        let routing = r(
            RoutingStrategy::LeastCost,
            vec![
                RoutingTarget::new("a"),
                RoutingTarget::new("b"),
                RoutingTarget::new("c"),
            ],
            Some(1), // truncation is deferred to resolve_attempt_models
        );
        // Ranking needs resolved Models, so pick_targets hands back every
        // target untouched regardless of max_fallbacks.
        assert_eq!(
            reg.pick_targets("v", &routing, &routing.targets, ""),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn healthy_only_returns_all_healthy() {
        let t = crate::ModelRuntimeStatusTracker::new();
        let attempts = vec![am("a"), am("b")];
        match filter_attempt_models(&t, attempts, WhenAllUnavailablePolicy::Fail) {
            FilterOutcome::Selected { attempts, excluded } => {
                assert_eq!(attempts.len(), 2);
                assert!(excluded.is_empty());
            }
            other => panic!(
                "expected Selected, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn cooldown_skipped_when_healthy_present() {
        let t = crate::ModelRuntimeStatusTracker::new();
        t.mark_cooldown("a", Duration::from_secs(30), "retryable_failure");
        let attempts = vec![am("a"), am("b")];
        match filter_attempt_models(&t, attempts, WhenAllUnavailablePolicy::Fail) {
            FilterOutcome::Selected { attempts, excluded } => {
                assert_eq!(attempts.len(), 1);
                assert_eq!(attempts[0].id, "b");
                // The dropped target is named, with why — the record the
                // dispatch loop turns into the WARN line.
                assert_eq!(excluded.len(), 1);
                assert_eq!(excluded[0].id, "a");
                assert_eq!(excluded[0].model, "a");
                assert_eq!(excluded[0].reason, EXCLUDED_COOLING);
            }
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn healthy_survivor_reports_both_cooling_and_unhealthy_drops() {
        // The three-or-more-target group, which is the common production
        // shape: one healthy survivor, one cooling, one background-dead.
        // Without this case the `extend` that appends the unhealthy
        // drops could be deleted outright and every other filter test
        // would still pass — the group would quietly lose a target with
        // nothing naming it, which is the whole failure this record
        // exists to remove.
        let t = crate::ModelRuntimeStatusTracker::new();
        t.mark_cooldown("b", Duration::from_secs(30), "x");
        t.mark_unhealthy("c", Some(503), "background_check_failed");
        let attempts = vec![am("a"), am("b"), am("c")];
        match filter_attempt_models(&t, attempts, WhenAllUnavailablePolicy::Fail) {
            FilterOutcome::Selected { attempts, excluded } => {
                assert_eq!(attempts.len(), 1);
                assert_eq!(attempts[0].id, "a");
                let mut got: Vec<(&str, &str)> = excluded
                    .iter()
                    .map(|e| (e.model.as_str(), e.reason))
                    .collect();
                got.sort_unstable();
                assert_eq!(got, [("b", EXCLUDED_COOLING), ("c", EXCLUDED_UNHEALTHY)]);
            }
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn all_unhealthy_fail_policy_returns_retry_after_hint() {
        // H3 contract: every candidate background-unhealthy, no
        // cooldown timer → return 503 + fallback Retry-After (30s
        // default). The dispatch loop converts this to a
        // ProxyError::AllCandidatesUnavailable.
        let t = crate::ModelRuntimeStatusTracker::new();
        t.mark_unhealthy("a", Some(503), "background_check_failed");
        t.mark_unhealthy("b", Some(503), "background_check_failed");
        let attempts = vec![am("a"), am("b")];
        match filter_attempt_models(&t, attempts, WhenAllUnavailablePolicy::Fail) {
            FilterOutcome::AllUnhealthy {
                retry_after_secs,
                excluded,
            } => {
                assert_eq!(retry_after_secs, Some(30));
                let mut names: Vec<&str> = excluded.iter().map(|e| e.model.as_str()).collect();
                names.sort_unstable();
                assert_eq!(names, ["a", "b"]);
                assert!(excluded.iter().all(|e| e.reason == EXCLUDED_UNHEALTHY));
            }
            _ => panic!("expected AllUnhealthy"),
        }
    }

    #[test]
    fn one_cooldown_with_all_else_unhealthy_keeps_the_cooldown_candidate() {
        // Mixed scenario: candidates a/b are background-unhealthy, c
        // is in cooldown. The filter should pick c (cooldown beats
        // unhealthy), not fail.
        let t = crate::ModelRuntimeStatusTracker::new();
        t.mark_unhealthy("a", Some(503), "background_check_failed");
        t.mark_unhealthy("b", Some(503), "background_check_failed");
        t.mark_cooldown("c", Duration::from_secs(30), "x");
        let attempts = vec![am("a"), am("b"), am("c")];
        match filter_attempt_models(&t, attempts, WhenAllUnavailablePolicy::Fail) {
            FilterOutcome::Selected { attempts, excluded } => {
                assert_eq!(attempts.len(), 1);
                assert_eq!(attempts[0].id, "c");
                // `c` was dispatched, so only the two unhealthy targets
                // count as excluded — a candidate that gets used is not
                // reported as dropped.
                let mut names: Vec<&str> = excluded.iter().map(|e| e.model.as_str()).collect();
                names.sort_unstable();
                assert_eq!(names, ["a", "b"]);
                assert!(excluded.iter().all(|e| e.reason == EXCLUDED_UNHEALTHY));
            }
            _ => panic!("expected Selected with cooldown candidate"),
        }
    }

    #[test]
    fn all_unhealthy_try_anyway_policy_returns_full_list() {
        // Legacy opt-in: send to all candidates regardless.
        let t = crate::ModelRuntimeStatusTracker::new();
        t.mark_unhealthy("a", Some(503), "background_check_failed");
        t.mark_unhealthy("b", Some(503), "background_check_failed");
        let attempts = vec![am("a"), am("b")];
        match filter_attempt_models(&t, attempts, WhenAllUnavailablePolicy::TryAnyway) {
            FilterOutcome::Selected { attempts, excluded } => {
                assert_eq!(attempts.len(), 2);
                // Nothing was dropped: `try_anyway` dispatches the whole
                // list, so reporting an exclusion here would be a lie.
                assert!(excluded.is_empty());
            }
            _ => panic!("expected Selected under TryAnyway policy"),
        }
    }

    #[test]
    fn cooldown_no_unhealthy_returns_cooldown_candidates() {
        // No healthy, no unhealthy — all candidates have a cooldown
        // timer set. Routing should still pick from them (better than
        // erroring out when we don't have evidence anyone is *broken*).
        let t = crate::ModelRuntimeStatusTracker::new();
        t.mark_cooldown("a", Duration::from_secs(30), "x");
        t.mark_cooldown("b", Duration::from_secs(30), "x");
        let attempts = vec![am("a"), am("b")];
        match filter_attempt_models(&t, attempts, WhenAllUnavailablePolicy::Fail) {
            FilterOutcome::Selected { attempts, excluded } => {
                assert_eq!(attempts.len(), 2);
                assert!(excluded.is_empty());
            }
            _ => panic!("expected Selected for cooldown-only"),
        }
    }
}
