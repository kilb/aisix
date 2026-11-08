//! Error envelopes for the proxy endpoints.
//!
//! Two on-the-wire envelope shapes — one per inbound protocol:
//!
//! - **OpenAI** (default, used by `/v1/chat/completions` and every other
//!   non-Anthropic endpoint) — spec §3 shape:
//!
//!   ```json
//!   {
//!     "error": {
//!       "message": "…",
//!       "type": "invalid_request_error",
//!       "param": null,
//!       "code": null
//!     }
//!   }
//!   ```
//!
//! - **Anthropic** (used by `/v1/messages` — closes #336). Per
//!   <https://docs.anthropic.com/en/api/errors>:
//!
//!   ```json
//!   {
//!     "type": "error",
//!     "error": {
//!       "type": "…",
//!       "message": "…"
//!     }
//!   }
//!   ```
//!
//!   The nested `error.type` maps from HTTP status onto the
//!   Anthropic SDK's strict `ErrorType` literal
//!   (`invalid_request_error` / `authentication_error` /
//!   `permission_error` / `not_found_error` / `request_too_large` /
//!   `rate_limit_error` / `timeout_error` / `overloaded_error` /
//!   `api_error`). Diverges from the OpenAI envelope's DP-stable
//!   taxonomy because the Anthropic SDK's `ErrorType` is a strict
//!   literal — emitting `"upstream_error"` would silently break
//!   customers branching on `e.body['error']['type']`. See
//!   [`anthropic_kind_from_status`] for the ecosystem-aligned mapping
//!   table.
//!
//! `ProxyError` is the internal error taxonomy; it implements
//! `IntoResponse` for the OpenAI shape so non-Anthropic handlers
//! `?`-propagate without ceremony. `/v1/messages` calls
//! [`ProxyError::into_anthropic_response`] explicitly so the
//! Anthropic shape lands on its responses.

use aisix_gateway::BridgeError;
use aisix_ratelimit::RateLimitError;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize, Clone)]
pub struct ErrorBody {
    pub message: String,
    /// `error.type` token. Was `&'static str` before #322 — widened to
    /// owned `String` because the type can now reflect an upstream-
    /// derived OpenAI taxonomy token (`rate_limit_exceeded`,
    /// `insufficient_quota`, …) when the error_translate layer maps a
    /// non-OpenAI upstream to OpenAI shape.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Budget-denial detail (prd-09b §5.8), flattened into the `error`
    /// block on a `budget_exceeded` 429 only. `None` (and thus absent
    /// from the wire) for every other error — upstream-translated
    /// errors, rate limits, validation, etc. — so the bare OpenAI
    /// {message,type,param,code} shape is preserved everywhere else.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetErrorFields>,
    /// Identity of the rate-limit policy that rejected the request —
    /// present on policy-layer 429s only (#892: with several
    /// policies live, an unattributed 429 is undebuggable). Same
    /// additive convention as `budget`: absent everywhere else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicyErrorRef>,
}

/// The `error.policy` block on a policy-layer 429.
#[derive(Debug, Serialize, Clone)]
pub struct PolicyErrorRef {
    pub id: String,
    pub name: String,
}

/// The structured budget fields that `budget_exceeded` 429s lift from
/// the exhausted budget's reason. Flattened into `ErrorBody`. Each field
/// is omitted when the reason does not carry it.
#[derive(Debug, Serialize, Clone)]
pub struct BudgetErrorFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_usd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_usd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_resets_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

impl ErrorEnvelope {
    pub fn new(message: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                message: message.into(),
                kind: kind.into(),
                param: None,
                code: None,
                budget: None,
                policy: None,
            },
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.error.code = Some(code.into());
        self
    }

    /// Attach the offending policy's identity to the error block. Only
    /// the policy-layer 429 path calls this.
    pub fn with_policy(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.error.policy = Some(PolicyErrorRef {
            id: id.into(),
            name: name.into(),
        });
        self
    }

    /// Attach the structured budget detail to the error block. Only
    /// the budget_exceeded path calls this.
    pub fn with_budget(mut self, r: &crate::budget::BudgetReason) -> Self {
        self.error.budget = Some(BudgetErrorFields {
            scope: r.scope.clone(),
            scope_ref: r.scope_ref.clone(),
            limit_usd: r.limit_usd.clone(),
            spent_usd: r.spent_usd.clone(),
            period: r.period.clone(),
            period_resets_at: r.period_resets_at.clone(),
            retry_after_seconds: r.retry_after_seconds,
        });
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// No usable gateway credential. Carries the credential header that
    /// failed, so the message points at the header to fix.
    #[error("missing or malformed {0} header")]
    MissingAuth(&'static str),
    /// A `header_key` passthrough route saw no gateway key in its configured
    /// header. Named separately from [`ProxyError::MissingAuth`] because on
    /// these routes `Authorization` deliberately carries the caller's own
    /// upstream credential — telling the integrator to fix `Authorization`
    /// points at exactly the wrong header.
    #[error("missing or malformed {0} header (this route's gateway API key header)")]
    MissingRouteAuthHeader(String),
    #[error("invalid API key")]
    InvalidApiKey,
    /// The presented key exists but its `expires_at` deadline has
    /// passed (#933). Deliberately caller-visible as "expired" (not a
    /// generic invalid-key 401): the caller already holds the secret,
    /// so naming the reason leaks nothing and tells them to request a
    /// fresh key instead of debugging a typo.
    #[error("API key has expired")]
    ApiKeyExpired,
    /// The presented key exists but was administratively disabled
    /// (#933). Same disclosure reasoning as [`Self::ApiKeyExpired`].
    #[error("API key has been disabled")]
    ApiKeyDisabled,
    /// The bearer was a JWT whose validation failed: malformed token,
    /// untrusted issuer, bad signature, audience mismatch, or an
    /// unresolvable signing key. Deliberately collapsed into one
    /// caller-visible reason so a probe cannot use the taxonomy as an
    /// oracle for which issuers this gateway trusts; the detailed
    /// reason goes to the auth decision log only.
    #[error("invalid JWT")]
    JwtInvalid,
    /// The JWT's `exp` deadline has passed. Caller-visible as
    /// "expired" for the same reason as [`Self::ApiKeyExpired`]: the
    /// caller already holds the token, and naming the reason tells
    /// them to fetch a fresh one from their identity provider instead
    /// of debugging a rejection.
    #[error("JWT has expired")]
    JwtExpired,
    /// The JWT verified but does not satisfy the trust provider's
    /// `bound_claims` requirements. 403: the caller is authenticated,
    /// just not entitled. (Scope failures are the sibling
    /// [`Self::JwtInsufficientScope`]; both render identically.)
    #[error("JWT does not satisfy the required scopes or claims")]
    JwtClaimsRejected,
    /// The JWT verified but its granted scopes do not cover the trust
    /// provider's `required_scopes`. Split from
    /// [`Self::JwtClaimsRejected`] so the active `/mcp` OAuth surface
    /// can attach an RFC 6750 `insufficient_scope` challenge naming the
    /// scopes this operation needs (AISIX-Cloud#1143) — a scope failure
    /// is curable by re-consenting, a `bound_claims` policy denial is
    /// not, so only the former may carry a challenge. Renders
    /// byte-identically to `JwtClaimsRejected` on the wire (same 403,
    /// `permission_denied` type, `jwt_claims_rejected` code): the `/v1`
    /// contract is unchanged.
    #[error("JWT does not satisfy the required scopes or claims")]
    JwtInsufficientScope { required_scopes: Vec<String> },
    /// The JWT verified but no API key carries a `jwt_subject` equal
    /// to its identity claim. Named explicitly (not a generic invalid
    /// credential) because the fix — binding a key to the identity —
    /// belongs to the gateway operator, and the caller-visible code is
    /// what they'll be shown when onboarding a fleet of agents.
    #[error("no API key is bound to this JWT identity")]
    JwtIdentityUnmapped,
    /// The signing keys for the matched trust provider could not be
    /// fetched (identity provider unreachable and nothing cached).
    /// 503 rather than 401: the token was not judged invalid, the
    /// gateway just cannot verify it right now — retryable.
    #[error("unable to fetch the identity provider's signing keys")]
    JwksUnavailable,
    /// The single-server MCP endpoint could not obtain an upstream token for
    /// the caller from an `oauth2_token_exchange` server's token endpoint.
    #[error("upstream token exchange failed: {0}")]
    McpTokenExchange(aisix_mcp::TokenExchangeFailure),
    /// `/mcp/{server}` has no usable upstream token for the caller of an
    /// `oauth2_authorization_code` server.
    #[error("{}", mcp_user_oauth_message(.failure, .server))]
    McpUserOAuth {
        failure: aisix_mcp::UserOAuthFailure,
        server: String,
    },
    #[error("model {0:?} not found")]
    ModelNotFound(String),
    /// A `/v1/videos/{video_id}` id that this gateway could not have
    /// minted — undecodable, or referencing a Model entry that no longer
    /// exists in the snapshot. 404, mirroring how the upstream videos
    /// API treats unknown job ids. The id echoes back verbatim: the
    /// caller supplied it, so it leaks nothing.
    #[error("video {0:?} not found")]
    VideoNotFound(String),
    /// A file, batch or fine-tuning id that names another owner, or no owner
    /// to a caller that has one. Refused on the id alone, so it reads the same
    /// whether or not the upstream holds it. The id echoes back verbatim: the
    /// caller supplied it, so it leaks nothing.
    #[error("resource {0:?} not found")]
    JobResourceNotFound(String),
    /// Platform mode: a `previous_response_id` that names no response the
    /// caller may continue from. It was not issued to the caller's tenant,
    /// or the target that served it is outside the key's provider groups.
    /// The caller is answered as for a response the upstream does not have
    /// (`crate::platform_errors`), so the refusal cannot tell which; the
    /// text names neither, since it reaches the usage event.
    #[error("previous response not found")]
    ResponseNotFound,
    /// Platform mode: a request field, named by its path, that names an
    /// object stored upstream which the gateway cannot bind to the caller's
    /// tenant (`crate::stored_refs`). Answered as an invalid request naming
    /// the field.
    #[error(
        "request field `{0}` names an object stored upstream, which platform mode does not allow"
    )]
    StoredObjectReference(String),
    /// Platform mode: a request field, named by its path, with which the
    /// caller would choose the model that answers past the key's model
    /// access, such as a fallback chain or a tool's own model, or have the
    /// upstream run a response the gateway cannot bill, a Responses
    /// `background` (`crate::stored_refs`). Answered as an invalid request
    /// naming the field.
    #[error(
        "request field `{0}` chooses the model that answers or runs a response the gateway \
         cannot bill, which platform mode does not allow"
    )]
    ModelSelectorRefused(String),
    /// A list of files, batches or fine-tuning jobs asked for on a provider key
    /// that keys belonging to a team or user reach. The upstream's list spans
    /// its whole account, and the gateway cannot say which of it the caller
    /// owns.
    #[error(
        "listing is not available where keys that belong to a team or user share the \
         provider: the provider's list cannot be attributed to an owner. Use the ids \
         returned when the resources were created"
    )]
    JobListRefused,
    #[error("API key is not allowed to use model {0:?}")]
    ModelForbidden(String),
    /// The resolved client IP is outside the model's `allowed_cidrs`
    /// allowlist (#557). Caller-visible message is intentionally generic and
    /// MUST NOT echo the configured CIDR ranges — handing back the allowlist
    /// lets a probe enumerate it (mirrors the #153 guardrail-redaction rule).
    /// The model name rides along for operator logs only.
    #[error("Access denied: your client IP is not allowed to access this model")]
    ModelIpRestricted(String),
    /// The resolved client IP is outside the passthrough route's
    /// `source_cidrs` allowlist. Same generic-message rule as
    /// [`Self::ModelIpRestricted`] — never echo the configured ranges; the
    /// route name rides along for operator logs only.
    #[error("Access denied: your client IP is not allowed to access this route")]
    RouteIpRestricted(String),
    /// The caller's API key does not carry the passthrough route in its
    /// `allowed_routes` grants.
    #[error("API key is not allowed to use passthrough route {0:?}")]
    RouteForbidden(String),
    /// The caller's API key lists the endpoint families it may call
    /// (`allowed_endpoints`) and this route's family is not among them
    /// (`crate::endpoint_acl`). The message names neither the family nor
    /// what the key may call.
    #[error("API key is not allowed to call this endpoint.")]
    EndpointForbidden(aisix_core::models::EndpointFamily),
    #[error("request payload is invalid: {0}")]
    InvalidRequest(String),
    /// [`Self::InvalidRequest`] on the wire, raised for what a target's own
    /// configuration lacks: a provider key that does not exist, an empty
    /// secret, no base URL. Its own variant because the dispatch loops treat
    /// the two differently: the next target of a Model Group has a
    /// configuration of its own (`routing::target_unusable`), while a bad
    /// request is bad on every target.
    #[error("request payload is invalid: {0}")]
    UnusableTarget(String),
    /// [`Self::InvalidRequest`] on the wire, raised for a refusal whose text
    /// names the provider or provider key the requested model resolved to:
    /// a provider the endpoint cannot serve, or a provider key configured in
    /// a way the endpoint cannot use. Its own variant so platform mode, where
    /// a caller must not learn which upstream serves it, can tell it from a
    /// refusal of the request itself (`crate::platform_errors`).
    #[error("request payload is invalid: {0}")]
    TargetRefused(String),
    /// A non-WebSocket request reached the WebSocket-only realtime
    /// endpoint. Carries the upgrade layer's own classification — 400 for
    /// malformed upgrade headers, 426 for a connection that cannot
    /// upgrade, 405 for a HEAD request (axum's `get()` also serves HEAD,
    /// so the extractor's method check is reachable) — plus its
    /// per-variant reason, so the refusal keeps both the status and the
    /// diagnostic the bare rejection used to carry.
    #[error("this endpoint requires a WebSocket upgrade: {detail}")]
    WebSocketUpgradeRequired { status: StatusCode, detail: String },
    #[error("no bridge registered for provider")]
    ProviderUnavailable,
    /// A semantic router could not make its routing decision — the
    /// classifier or embedding call failed — and its failure policy is
    /// `fail`. `reason` is a closed vocabulary; the underlying error,
    /// which can quote the prompt, stays on the router's warn line.
    #[error("semantic router '{router}' could not pick a route ({reason})")]
    SemanticDecisionFailed {
        router: String,
        reason: crate::semantic::DecisionFailure,
    },
    /// Every routing candidate is background-unhealthy and the routing
    /// model is configured with `when_all_unavailable: fail`. Caller-visible
    /// as 503 with a fixed Retry-After hint. Candidates in cooldown never
    /// get here: with no healthy one left they are dispatched
    /// (`routing::filter_attempt_models`). See
    /// [`aisix_core::WhenAllUnavailablePolicy`].
    #[error("all routing candidates are unavailable")]
    AllCandidatesUnavailable { retry_after_secs: Option<u64> },
    /// Caller-visible message MUST NOT carry the matched-pattern detail.
    /// Per #153, leaking the matched literal back to the caller defeats
    /// the point of an output guardrail (the whole purpose is to keep the
    /// forbidden content from reaching the caller; echoing it in the
    /// error envelope is a partial bypass and lets anyone who can
    /// trigger the guardrail enumerate the policy's blocklist).
    /// Every guardrail-block site builds the redacted public message via
    /// [`guardrail_block_message`] — generic policy wording plus the NAME
    /// of the guardrail that fired (#519 B.4b) — and emits the rich
    /// detail to `tracing` for operators.
    ///
    /// `unavailable` carries the guardrail's bounded failure tag when the
    /// refusal was an AVAILABILITY failure on a `fail_open: false` row —
    /// the screening service was unreachable, or an operator-supplied
    /// script produced no usable verdict — rather than a content decision.
    /// It is the same closed, label-safe vocabulary
    /// `GuardrailVerdict::Block::unavailable` carries, never free text and
    /// never matched content, so it is safe on the wire.
    ///
    /// Keeping the two inside ONE variant is deliberate: `status()`,
    /// `kind()`, and the dispatch-loop predicates must not diverge between
    /// them (a guardrail refusal is a guardrail refusal for retry,
    /// failover and outcome-classification purposes), so the distinction
    /// lives only where it is read by a human or branched on by an SDK —
    /// the message and `error.code`.
    #[error("{message}")]
    ContentFiltered {
        message: String,
        unavailable: Option<String>,
    },
    // Carries the budget document's structured reason. Display forwards
    // its message verbatim (it's already a complete customer sentence —
    // "<scope> budget '<name>' exceeded ($X/period). Resets …"); the
    // structured fields ride along in the 429 error block via
    // `with_budget` (prd-09b §5.8).
    // Boxed: BudgetReason is ~184 bytes; inlining it would make this the
    // largest ProxyError variant and trip clippy::result_large_err across
    // every `Result<_, ProxyError>` in the hot path. The box keeps the
    // enum small (budget denial is rare, so the extra alloc is fine).
    #[error("{}", .0.message)]
    BudgetExceeded(Box<crate::budget::BudgetReason>),
    /// A budget applies to the request, but the environment's budget
    /// policy is `closed` and usage has not reached the control plane for
    /// longer than its grace period, so whether the budget still has room
    /// cannot be known.
    #[error("budget could not be verified: usage has not reached the control plane")]
    BudgetUnverified,
    /// An account lookup (`GET /v1/key`) the control plane could not answer:
    /// it timed out, could not be reached, failed, or answered with something
    /// that does not decode. Only the lookup fails; model requests never ask
    /// the control plane. It names no upstream, so it keeps this envelope in
    /// platform mode.
    #[error("Account information is temporarily unavailable.")]
    AccountUnavailable,
    /// Per RFC 9110 §15.5.14, a request body that exceeds a server-
    /// imposed limit gets a `413 Content Too Large`. The caller-visible
    /// `message` is intentionally bare of the actual incoming size
    /// (the limit is the only stable detail the caller needs). Set by
    /// the body-limit middleware in `lib.rs::enforce_request_body_limit`
    /// when the inbound `Content-Length` exceeds the configured cap.
    #[error("request body exceeds {limit_bytes}-byte limit")]
    RequestTooLarge { limit_bytes: usize },
    #[error(transparent)]
    RateLimit(#[from] RateLimitError),
    /// The model's own inline limit refused the request. Same status, type
    /// and headers as [`Self::RateLimit`]; its own variant because the bucket
    /// is shared by every caller of the model, so in platform mode its
    /// `x-ratelimit-*` figures are the platform's and are not sent.
    #[error(transparent)]
    ModelRateLimit(RateLimitError),
    /// A policy-layer rate-limit rejection carrying the offending
    /// policy's identity (#892). Same status/type/headers as
    /// [`Self::RateLimit`]; the envelope adds `error.policy` so a
    /// caller hitting one of several live policies can tell which. The
    /// Display form names the policy too, so every path that flattens
    /// this error into a message (routing attempt records, mid-stream
    /// failover, ensemble logs) keeps the attribution.
    #[error("{source} (policy '{policy_name}')")]
    PolicyRateLimit {
        source: RateLimitError,
        // Boxed strs: with the flag below, a `String` pair would take the
        // error past the size clippy allows in a `Result`.
        policy_id: Box<str>,
        policy_name: Box<str>,
        /// Whether the bucket holds only the caller's own traffic
        /// ([`crate::quota::policy_bucket_is_callers`]).
        callers_bucket: bool,
    },
    #[error(transparent)]
    Bridge(#[from] BridgeError),
}

/// Failure tags for the two refusals the PROXY itself raises on a
/// guardrail's behalf, where no `GuardrailVerdict` exists to carry one.
/// Same closed, label-safe vocabulary as a kind's own tag, and here for
/// the same reason: in both cases the content was never screened, so
/// telling the caller its content violated a policy states something that
/// did not happen.
///
/// `output_buffer_exceeded` — a streamed response outgrew the hold-back
/// cap and the row is fail-closed, so it is refused unscanned. Under
/// `on_buffer_exceeded: fail_open` the same tag is the request's
/// `guardrail_bypassed_reason`: there it means the output scan was skipped.
/// `unscannable_body` — the body could not be walked, so the guardrail was
/// never offered the content to scan.
/// `mask_writeback_failed` — a mask verdict could not be spliced back into
/// the body, so the request is refused rather than forwarded unmasked.
pub(crate) const TAG_OUTPUT_BUFFER_EXCEEDED: &str = "output_buffer_exceeded";
pub(crate) const TAG_UNSCANNABLE_BODY: &str = "unscannable_body";
pub(crate) const TAG_MASK_WRITEBACK_FAILED: &str = "mask_writeback_failed";

/// The caller-visible message for a guardrail `Block` verdict.
///
/// Carries WHICH guardrail fired — `guardrail_name` is operator-assigned
/// metadata, safe to surface (#519 B.4b) — but never the matched-pattern
/// detail (per #153 that detail stays in `tracing` only; echoing it lets
/// callers enumerate the blocklist or extract the blocked output).
/// `side` is `"request"` (input hook) or `"response"` (output hook) — the MCP
/// endpoints pass `"tool call"` / `"tool result"` instead. Every endpoint
/// family builds its rejection text through this helper so the wording can't
/// drift between siblings, even where the envelope differs (422 or an SSE
/// error event on the LLM routes; an `isError` tool result on `/mcp`).
///
/// `unavailable` splits the sentence in two. A guardrail that REFUSED the
/// content and a guardrail that COULD NOT EVALUATE it are the same 422 to
/// every consumer downstream, and until this parameter existed they were
/// also the same sentence — so an operator whose fail-closed row was
/// broken (a screening service down, a script returning a verdict the
/// gateway does not understand) saw their own working policy's message on
/// every single request, with nothing in the response to suggest
/// otherwise. AISIX-Cloud#1365 split the two on the audit event and the
/// metric; this is the third surface, the one the caller actually reads.
/// The tag is named in the text because it is what an operator greps for
/// and what the dashboard shows.
pub(crate) fn guardrail_block_message(
    side: &str,
    guardrail_name: Option<&str>,
    unavailable: Option<&str>,
) -> String {
    match (unavailable, guardrail_name) {
        (Some(tag), Some(name)) => {
            format!("{side} rejected: guardrail '{name}' could not evaluate it ({tag})")
        }
        (Some(tag), None) => format!("{side} rejected: a guardrail could not evaluate it ({tag})"),
        (None, Some(name)) => format!("{side} blocked by content policy (guardrail '{name}')"),
        (None, None) => format!("{side} blocked by content policy"),
    }
}

/// [`guardrail_block_message`] as the caller is sent it. In platform mode a
/// fail-closed refusal names no failure tag: the tag can name the
/// guardrail's vendor and the state of its account (`bedrock_throttled`),
/// which is the platform's. The refusal still says the guardrail could not
/// evaluate it, its code is still `guardrail_unavailable`, and the error and
/// the usage row keep the tag.
pub(crate) fn caller_guardrail_block_message(
    side: &str,
    guardrail_name: Option<&str>,
    unavailable: Option<&str>,
) -> String {
    match (unavailable, guardrail_name) {
        (Some(_), Some(name)) if crate::platform_errors::active() => {
            format!("{side} rejected: guardrail '{name}' could not evaluate it")
        }
        (Some(_), None) if crate::platform_errors::active() => {
            format!("{side} rejected: a guardrail could not evaluate it")
        }
        _ => guardrail_block_message(side, guardrail_name, unavailable),
    }
}

/// [`guardrail_block_message`] wrapped in the error the LLM handlers
/// return. The `Option<String>` sites (SSE error frames, `/mcp` tool
/// results, hand-built JSON bodies) use the message helper directly; every
/// site that returns a [`ProxyError`] goes through this one so the message
/// and the `unavailable` tag can never be set from different verdicts.
///
/// Every refusal passes through here, so this is also what takes a refused
/// request's body off its failure rows, unless the route holds a masked copy
/// for them (`usage_attr::note_refused_request`): a route that forgets exports
/// nothing rather than the body it noted before the chain ran.
pub(crate) fn guardrail_block_error(
    side: &str,
    guardrail_name: Option<&str>,
    unavailable: Option<&str>,
) -> ProxyError {
    if !matches!(side, "response" | "tool result") {
        crate::attribution::note_request_refused();
    }
    ProxyError::ContentFiltered {
        message: guardrail_block_message(side, guardrail_name, unavailable),
        unavailable: unavailable.map(str::to_owned),
    }
}

/// The `data:` payload of the OpenAI-shape SSE `event: error` frame that
/// ends a streamed response an output guardrail refused. It is the buffered
/// 422 body for the same refusal, so the two halves of an endpoint carry the
/// same `error.type` and the same `error.code` (`guardrail_unavailable` for
/// a fail-closed refusal, none for a policy block).
pub(crate) fn guardrail_block_frame_payload(
    guardrail_name: Option<&str>,
    unavailable: Option<&str>,
) -> String {
    serde_json::to_string(
        &guardrail_block_error("response", guardrail_name, unavailable).envelope(),
    )
    .expect("an error envelope serializes")
}

impl ProxyError {
    pub fn status(&self) -> StatusCode {
        match self {
            ProxyError::MissingRouteAuthHeader(_)
            | ProxyError::MissingAuth(_)
            | ProxyError::InvalidApiKey
            | ProxyError::ApiKeyExpired
            | ProxyError::ApiKeyDisabled
            | ProxyError::JwtInvalid
            | ProxyError::JwtExpired
            | ProxyError::JwtIdentityUnmapped => StatusCode::UNAUTHORIZED,
            ProxyError::JwtClaimsRejected | ProxyError::JwtInsufficientScope { .. } => {
                StatusCode::FORBIDDEN
            }
            ProxyError::JwksUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::McpTokenExchange(failure) => match failure {
                aisix_mcp::TokenExchangeFailure::NoSubject
                | aisix_mcp::TokenExchangeFailure::SubjectRejected { .. } => {
                    StatusCode::UNAUTHORIZED
                }
                aisix_mcp::TokenExchangeFailure::Misconfigured => StatusCode::INTERNAL_SERVER_ERROR,
                aisix_mcp::TokenExchangeFailure::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            },
            ProxyError::McpUserOAuth { failure, .. } => match failure {
                aisix_mcp::UserOAuthFailure::AuthorizationRequired => StatusCode::UNAUTHORIZED,
                // Never a 401: an MCP client answers any 401 by opening a
                // browser, and signing in cannot give this caller a user.
                aisix_mcp::UserOAuthFailure::NoSubject => StatusCode::FORBIDDEN,
                aisix_mcp::UserOAuthFailure::Misconfigured => StatusCode::INTERNAL_SERVER_ERROR,
                aisix_mcp::UserOAuthFailure::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            },
            ProxyError::ModelForbidden(_) => StatusCode::FORBIDDEN,
            ProxyError::ModelIpRestricted(_) => StatusCode::FORBIDDEN,
            ProxyError::RouteIpRestricted(_) => StatusCode::FORBIDDEN,
            ProxyError::RouteForbidden(_) => StatusCode::FORBIDDEN,
            ProxyError::EndpointForbidden(_) => StatusCode::FORBIDDEN,
            ProxyError::ModelNotFound(_) => StatusCode::NOT_FOUND,
            ProxyError::VideoNotFound(_) => StatusCode::NOT_FOUND,
            ProxyError::JobResourceNotFound(_) => StatusCode::NOT_FOUND,
            ProxyError::ResponseNotFound => StatusCode::NOT_FOUND,
            ProxyError::StoredObjectReference(_) | ProxyError::ModelSelectorRefused(_) => {
                StatusCode::BAD_REQUEST
            }
            ProxyError::JobListRefused => StatusCode::FORBIDDEN,
            ProxyError::InvalidRequest(_)
            | ProxyError::UnusableTarget(_)
            | ProxyError::TargetRefused(_) => StatusCode::BAD_REQUEST,
            ProxyError::WebSocketUpgradeRequired { status, .. } => *status,
            ProxyError::ProviderUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::SemanticDecisionFailed { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::AllCandidatesUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::ContentFiltered { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            ProxyError::BudgetExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            ProxyError::BudgetUnverified => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::AccountUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::RequestTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            ProxyError::RateLimit(_) | ProxyError::ModelRateLimit(_) => {
                StatusCode::TOO_MANY_REQUESTS
            }
            ProxyError::PolicyRateLimit { .. } => StatusCode::TOO_MANY_REQUESTS,
            ProxyError::Bridge(b) => {
                StatusCode::from_u16(b.http_status()).unwrap_or(StatusCode::BAD_GATEWAY)
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ProxyError::MissingRouteAuthHeader(_)
            | ProxyError::MissingAuth(_)
            | ProxyError::InvalidApiKey
            | ProxyError::ApiKeyExpired
            | ProxyError::ApiKeyDisabled
            | ProxyError::JwtInvalid
            | ProxyError::JwtExpired
            | ProxyError::JwtIdentityUnmapped => "invalid_api_key",
            ProxyError::JwtClaimsRejected | ProxyError::JwtInsufficientScope { .. } => {
                "permission_denied"
            }
            // Auth infrastructure fault, not a credential judgment — the
            // generic server-fault family, like a 5xx from dispatch.
            ProxyError::JwksUnavailable => "api_error",
            ProxyError::McpTokenExchange(failure) => match failure {
                aisix_mcp::TokenExchangeFailure::NoSubject
                | aisix_mcp::TokenExchangeFailure::SubjectRejected { .. } => "authentication_error",
                aisix_mcp::TokenExchangeFailure::Misconfigured
                | aisix_mcp::TokenExchangeFailure::Unavailable => "api_error",
            },
            ProxyError::McpUserOAuth { failure, .. } => match failure {
                aisix_mcp::UserOAuthFailure::AuthorizationRequired => "authentication_error",
                aisix_mcp::UserOAuthFailure::NoSubject => "permission_denied",
                aisix_mcp::UserOAuthFailure::Misconfigured
                | aisix_mcp::UserOAuthFailure::Unavailable => "api_error",
            },
            ProxyError::ModelForbidden(_) => "permission_denied",
            ProxyError::ModelIpRestricted(_) => "permission_denied",
            ProxyError::RouteIpRestricted(_) => "permission_denied",
            ProxyError::RouteForbidden(_) => "permission_denied",
            ProxyError::EndpointForbidden(_) => "permission_denied",
            ProxyError::ModelNotFound(_) => "model_not_found",
            ProxyError::VideoNotFound(_) => "video_not_found",
            ProxyError::JobResourceNotFound(_) => "resource_not_found",
            ProxyError::ResponseNotFound => "response_not_found",
            ProxyError::StoredObjectReference(_) | ProxyError::ModelSelectorRefused(_) => {
                "invalid_request_error"
            }
            ProxyError::JobListRefused => "permission_denied",
            ProxyError::InvalidRequest(_)
            | ProxyError::UnusableTarget(_)
            | ProxyError::TargetRefused(_) => "invalid_request_error",
            ProxyError::WebSocketUpgradeRequired { .. } => "websocket_upgrade_required",
            ProxyError::RequestTooLarge { .. } => "invalid_request_error",
            ProxyError::ProviderUnavailable => "provider_unavailable",
            // A gateway-side dependency failed, like `JwksUnavailable`;
            // `error.code` carries the distinction.
            ProxyError::SemanticDecisionFailed { .. } => "api_error",
            ProxyError::AllCandidatesUnavailable { .. } => "all_candidates_unavailable",
            ProxyError::ContentFiltered { .. } => "content_filter",
            ProxyError::BudgetExceeded(_) => "billing_error",
            // A gateway-side dependency failed, like `JwksUnavailable`;
            // `error.code` carries the distinction.
            ProxyError::BudgetUnverified => "api_error",
            // A gateway-side dependency failed, like `JwksUnavailable`;
            // `error.code` carries the distinction.
            ProxyError::AccountUnavailable => "api_error",
            ProxyError::RateLimit(_) | ProxyError::ModelRateLimit(_) => "rate_limit_exceeded",
            ProxyError::PolicyRateLimit { .. } => "rate_limit_exceeded",
            ProxyError::Bridge(b) => b.error_type(),
        }
    }

    /// Whether this failure IS a guardrail refusal — the request was
    /// stopped by the guardrail machinery rather than by an upstream, a
    /// quota, a credential or a malformed body.
    ///
    /// Drives `UsageEvent::guardrail_blocked`, which is the indexed column
    /// the dashboard's "Guardrail blocks" view and the
    /// `guardrail_blocked=true` usage query filter on. Every failure-path
    /// emitter reads it from here rather than re-deriving the match, so a
    /// new handler cannot join the family with the flag silently left at
    /// its `false` default (AISIX-Cloud#1428).
    ///
    /// A fail-closed refusal (`unavailable: Some(_)` — the guardrail could
    /// not evaluate the request and its row refuses what it cannot check)
    /// counts too: the guardrail machinery is still what stopped the
    /// request, and hiding it from the Blocked view would leave an operator
    /// with a 422 that nothing accounts for. Which of the two it was stays
    /// legible on `guardrail_enforced_hits.action`
    /// (`blocked` vs `blocked_unavailable`).
    pub(crate) fn is_guardrail_block(&self) -> bool {
        matches!(self, ProxyError::ContentFiltered { .. })
    }

    /// Seconds the client should wait before retrying. Only present for
    /// rate-limit-style rejections so the proxy can emit a `Retry-After`
    /// header.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            ProxyError::RateLimit(e) | ProxyError::ModelRateLimit(e) => e.retry_after_secs(),
            ProxyError::PolicyRateLimit { source, .. } => source.retry_after_secs(),
            ProxyError::AllCandidatesUnavailable { retry_after_secs } => *retry_after_secs,
            // Source the Retry-After header from the same value the 429
            // body carries (prd-09b §5.8 retry_after_seconds), so the
            // header and body agree — SDKs back off on the header.
            ProxyError::BudgetExceeded(r) => r.retry_after_seconds,
            // Forward an upstream 429's Retry-After hint so SDK clients
            // back off on the provider's actual cool-down instead of a
            // default. The hint is parsed into `UpstreamStatus.retry_after`
            // by the bridge; only 429 carries a meaningful value.
            ProxyError::Bridge(BridgeError::UpstreamStatus {
                status: 429,
                retry_after: Some(d),
                ..
            }) => Some(d.as_secs()),
            _ => None,
        }
    }

    /// The OpenAI-shape envelope the caller receives. In platform mode an
    /// upstream's failure, or a refusal naming the target, is its platform
    /// class instead (`crate::platform_errors`); the original error still
    /// names the failure in the usage event and the access log. Platform mode
    /// also answers a model the key may not use as an unknown one
    /// (`crate::platform_errors::caller_error`).
    pub fn envelope(&self) -> ErrorEnvelope {
        if let Some(answered) = crate::platform_errors::caller_error(self) {
            return answered.envelope();
        }
        if let Some(class) = crate::platform_errors::caller_class(self) {
            return class.envelope();
        }
        // Bridge-surface upstream errors get special handling: the
        // bridge has best-effort-parsed the upstream envelope into a
        // structured [`UpstreamErrorView`], and for a 4xx we translate
        // its fields into the client's envelope. 5xx and non-JSON bodies
        // take the generic envelope; every one carries the upstream's own
        // message.
        if let ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamStatus {
            status,
            message,
            parsed,
            wire,
            ..
        }) = self
        {
            return render_bridge_upstream_envelope(*status, message, parsed.as_deref(), *wire);
        }
        // An in-band stream error (provider error inside a 2xx stream,
        // caught pre-first-chunk) carries the same parsed view — render
        // it with the embedded status so a provider's in-band 429 /
        // overloaded gets the same translated envelope its HTTP twin
        // would. No embedded status → the generic 502 family.
        if let ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamInBand {
            status,
            message,
            parsed,
            wire,
        }) = self
        {
            return render_bridge_upstream_envelope(
                status.unwrap_or(502),
                message,
                parsed.as_deref(),
                *wire,
            );
        }
        // A timeout's transport cause names the upstream host and the
        // connection-layer fault it hit. Same rule as the 5xx body below:
        // that is operator diagnostics, so it reaches the logs and the
        // per-attempt telemetry through `Display`, while the caller keeps
        // the bare sentence it has always had — an `api_base` is internal
        // topology and does not belong in a customer-facing envelope.
        if let ProxyError::Bridge(aisix_gateway::BridgeError::Timeout { elapsed_ms, .. }) = self {
            return ErrorEnvelope::new(
                format!("upstream request timed out after {elapsed_ms}ms"),
                self.kind(),
            );
        }
        if let ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamBodyUnread {
            decode_error,
            ..
        }) = self
        {
            let as_decode = aisix_gateway::BridgeError::UpstreamDecode(decode_error.clone());
            return ErrorEnvelope::new(as_decode.to_string(), self.kind());
        }
        let env = ErrorEnvelope::new(self.to_string(), self.kind());
        match self {
            // In platform mode a fail-closed refusal names no failure tag
            // (`caller_guardrail_block_message`); the error keeps it.
            ProxyError::ContentFiltered {
                message,
                unavailable: Some(tag),
            } if crate::platform_errors::active() => {
                let message = message
                    .strip_suffix(&format!(" ({tag})"))
                    .unwrap_or(message);
                ErrorEnvelope::new(message, self.kind()).with_code("guardrail_unavailable")
            }
            // In platform mode a budget outside the caller's own scope says
            // nothing but that a limit was reached.
            ProxyError::BudgetExceeded(r) if hides_budget_scope(r) => {
                ErrorEnvelope::new(HIDDEN_BUDGET_MESSAGE, self.kind()).with_code("budget_exceeded")
            }
            ProxyError::BudgetExceeded(r) => {
                let mut env = env.with_code("budget_exceeded").with_budget(r);
                // A message that names a policy is the platform's wording.
                if let (true, Some(m)) = (crate::platform_errors::active(), &r.caller_message) {
                    env.error.message = m.clone();
                }
                env
            }
            ProxyError::BudgetUnverified => env.with_code("budget_unverified"),
            ProxyError::AccountUnavailable => env.with_code("control_plane_unavailable"),
            // Attribution for policy-layer 429s (#892): the
            // OpenAI envelope names the offending policy. The Anthropic
            // envelope keeps its strict {type,message} shape.
            // Platform mode names no policy: it is the platform's, and its
            // name is in the Display text as well as in the block.
            ProxyError::PolicyRateLimit { source, .. } if crate::platform_errors::active() => {
                ErrorEnvelope::new(source.to_string(), self.kind())
            }
            ProxyError::PolicyRateLimit {
                policy_id,
                policy_name,
                ..
            } => env.with_policy(policy_id.to_string(), policy_name.to_string()),
            // Stable machine-readable code for SDKs to branch on, distinct
            // from the generic `permission_denied` type shared with
            // ModelForbidden (#557 AC-1).
            ProxyError::ModelIpRestricted(_) => env.with_code("ip_restricted"),
            ProxyError::RouteIpRestricted(_) => env.with_code("ip_restricted"),
            // Stable machine-readable codes so SDKs can branch on the
            // lifecycle reason without parsing the message, while the
            // `error.type` stays the family-wide `invalid_api_key`.
            ProxyError::ApiKeyExpired => env.with_code("api_key_expired"),
            ProxyError::ApiKeyDisabled => env.with_code("api_key_disabled"),
            // Same stable-code convention for the JWT auth path
            // (#1080/#1081): SDKs and agent frameworks branch
            // on `error.code` to decide between refreshing the token
            // (`jwt_expired`), fixing the token request
            // (`jwt_invalid` / `jwt_claims_rejected`), and asking the
            // gateway operator to bind the identity to a key
            // (`jwt_identity_unmapped`).
            ProxyError::JwtInvalid => env.with_code("jwt_invalid"),
            ProxyError::JwtExpired => env.with_code("jwt_expired"),
            ProxyError::JwtClaimsRejected | ProxyError::JwtInsufficientScope { .. } => {
                env.with_code("jwt_claims_rejected")
            }
            ProxyError::JwtIdentityUnmapped => env.with_code("jwt_identity_unmapped"),
            ProxyError::JwksUnavailable => env.with_code("jwks_unavailable"),
            ProxyError::McpTokenExchange(failure) => env.with_code(match failure {
                aisix_mcp::TokenExchangeFailure::NoSubject => "token_exchange_subject_missing",
                aisix_mcp::TokenExchangeFailure::SubjectRejected { .. } => {
                    "token_exchange_subject_rejected"
                }
                aisix_mcp::TokenExchangeFailure::Misconfigured => "token_exchange_misconfigured",
                aisix_mcp::TokenExchangeFailure::Unavailable => "token_exchange_unavailable",
            }),
            ProxyError::McpUserOAuth { failure, .. } => env.with_code(match failure {
                aisix_mcp::UserOAuthFailure::AuthorizationRequired => {
                    "mcp_oauth_authorization_required"
                }
                aisix_mcp::UserOAuthFailure::NoSubject => "mcp_oauth_no_subject",
                aisix_mcp::UserOAuthFailure::Misconfigured => "mcp_oauth_misconfigured",
                aisix_mcp::UserOAuthFailure::Unavailable => "mcp_oauth_unavailable",
            }),
            ProxyError::SemanticDecisionFailed { .. } => env.with_code("semantic_routing_failed"),
            // Same stable-code convention, for the one distinction a
            // caller cannot make from `error.type`: `content_filter`
            // covers both a policy refusal and a fail-closed row that
            // could not evaluate the request. The `type` stays
            // `content_filter` — it is a shipped contract and both really
            // are guardrail refusals — while the code lets an SDK branch,
            // and lets an operator's dashboard stop counting a broken
            // script as policy volume.
            ProxyError::ContentFiltered {
                unavailable: Some(_),
                ..
            } => env.with_code("guardrail_unavailable"),
            _ => env,
        }
    }
}

/// What a caller is told of a budget outside its own scope in platform mode.
const HIDDEN_BUDGET_MESSAGE: &str = "Spending limit reached.";

/// Whether a `budget_exceeded` is hidden from the caller: platform mode, and
/// a budget that is not the caller's own (key, member, team or team member).
/// A reason with no scope is hidden too, since nothing says it is theirs.
/// Keys on the budget that denied, whichever model the request addressed.
fn hides_budget_scope(r: &crate::budget::BudgetReason) -> bool {
    use aisix_core::models::BudgetScope;
    // Both the typed scope and the control plane's text must name one of the
    // caller's own scopes; either one disagreeing hides the budget.
    let own_typed = matches!(
        r.scope_kind,
        Some(
            BudgetScope::ApiKey | BudgetScope::Member | BudgetScope::Team | BudgetScope::TeamMember
        )
    );
    let own_text = matches!(
        r.scope.as_deref(),
        Some("api_key" | "member" | "team" | "team_member")
    );
    crate::platform_errors::active() && !(own_typed && own_text)
}

/// Build the customer-visible envelope for an upstream HTTP error.
///
/// **4xx**: delegate to [`crate::error_translate::render_openai_envelope`],
/// which (a) passes OpenAI-wire fields verbatim, (b) translates
/// Anthropic / Bedrock / Vertex / AzureOpenAI taxonomy via per-wire
/// tables so the OpenAI-shape `error.type` and `error.code` carry the
/// retry semantics SDKs depend on.
///
/// **5xx and synthesised errors** (`UpstreamWire::Unknown`): the generic
/// `upstream_error` envelope carrying the upstream's own message.
///
/// An upstream that answers with no usable message (an empty body, or an
/// envelope whose `message` is blank) is named by its status and reason
/// phrase instead, so the caller never receives an empty `error.message`.
/// Response headers are deliberately not echoed: a redirect's `Location`
/// names a host the operator never configured.
fn render_bridge_upstream_envelope(
    status: u16,
    message: &str,
    parsed: Option<&aisix_gateway::UpstreamErrorView>,
    wire: aisix_gateway::UpstreamWire,
) -> ErrorEnvelope {
    let status_message;
    let message = if message.trim().is_empty() {
        status_message = upstream_status_message(status);
        status_message.as_str()
    } else {
        message
    };
    let is_4xx = (400..500).contains(&status);
    if is_4xx && !matches!(wire, aisix_gateway::UpstreamWire::Unknown) {
        return ErrorEnvelope {
            error: crate::error_translate::render_openai_envelope(parsed, wire, message),
        };
    }
    ErrorEnvelope::new(message.to_string(), "upstream_error")
}

fn upstream_status_message(status: u16) -> String {
    match StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
    {
        Some(reason) => format!("upstream returned {status} {reason}"),
        None => format!("upstream returned {status}"),
    }
}

/// Auth-failure classification attached to the response as an extension
/// so the `/mcp` challenge middleware (`crate::mcp_auth`) can add the
/// RFC 9728 `WWW-Authenticate` header. Classification must happen here:
/// `IntoResponse` has no access to route or snapshot state, and only the
/// active `/mcp` surface actually renders the header.
#[derive(Debug, Clone)]
pub(crate) enum AuthChallenge {
    /// No usable credential was presented — bare `resource_metadata`.
    MissingCredentials,
    /// A credential was presented and rejected — `error="invalid_token"`.
    InvalidToken,
    /// A valid token lacks the scopes this operation requires —
    /// `error="insufficient_scope"` plus the currently-required scopes
    /// (SEP-2350 semantics: only what this operation needs, never an
    /// echo of previously granted scopes).
    InsufficientScope { required_scopes: Vec<String> },
    /// The identity provider demands a step-up (Microsoft Entra claims
    /// challenge) before it accepts the caller's token for an on-behalf-of
    /// exchange — `error="insufficient_claims"` plus the provider's claims,
    /// base64-encoded, for the client to satisfy at the provider and retry.
    InsufficientClaims { claims: String },
}

impl ProxyError {
    /// The challenge classification for this error, when it is an
    /// auth failure a Bearer challenge can describe. `JwtClaimsRejected`
    /// (`bound_claims` policy denial) deliberately maps to `None`:
    /// re-authenticating cannot cure it, so a challenge would only send
    /// clients into a retry loop. `JwksUnavailable` is a gateway-side
    /// fault, not a credential judgment.
    fn auth_challenge(&self) -> Option<AuthChallenge> {
        match self {
            ProxyError::MissingAuth(_) => Some(AuthChallenge::MissingCredentials),
            ProxyError::InvalidApiKey
            | ProxyError::ApiKeyExpired
            | ProxyError::ApiKeyDisabled
            | ProxyError::JwtInvalid
            | ProxyError::JwtExpired
            | ProxyError::JwtIdentityUnmapped => Some(AuthChallenge::InvalidToken),
            ProxyError::JwtInsufficientScope { required_scopes } => {
                Some(AuthChallenge::InsufficientScope {
                    required_scopes: required_scopes.clone(),
                })
            }
            // The token an on-behalf-of exchange needs is one the
            // environment's own trust provider issues, which is exactly
            // what the gateway's discovery document names.
            ProxyError::McpTokenExchange(aisix_mcp::TokenExchangeFailure::SubjectRejected {
                claims: Some(claims),
            }) => Some(AuthChallenge::InsufficientClaims {
                claims: claims.clone(),
            }),
            ProxyError::McpTokenExchange(
                aisix_mcp::TokenExchangeFailure::NoSubject
                | aisix_mcp::TokenExchangeFailure::SubjectRejected { claims: None },
            ) => Some(AuthChallenge::InvalidToken),
            _ => None,
        }
    }
}

impl ProxyError {
    /// The gateway limit that refused this request, when the gateway's
    /// OWN limiter is what refused it.
    ///
    /// `None` for every other 429: a budget rejection caps spend in
    /// dollars, not requests or tokens, and an upstream 429 reports the
    /// provider's quota, which the gateway does not know. Both would
    /// have to invent the numbers. That makes the presence of the
    /// `x-ratelimit-*` trio on a 429 the caller's signal that AISIX
    /// itself rejected the request rather than relaying somebody else's
    /// rejection.
    fn rate_limit_detail(&self) -> Option<aisix_ratelimit::LimitDetail> {
        match self {
            ProxyError::RateLimit(e) | ProxyError::ModelRateLimit(e) => Some(e.detail()),
            ProxyError::PolicyRateLimit { source, .. } => Some(source.detail()),
            _ => None,
        }
    }

    /// [`Self::rate_limit_detail`] as the caller is sent it. In platform mode
    /// a bucket several callers share (the model's inline limit, a policy not
    /// keyed to the caller) reports the platform's own limit and what is left
    /// of it, so none of the trio is sent. `Retry-After` stays: windows are
    /// clock-aligned, so it shows only a window's length, and SDKs back off
    /// on it.
    fn caller_rate_limit_detail(&self) -> Option<aisix_ratelimit::LimitDetail> {
        let shared = match self {
            ProxyError::ModelRateLimit(_) => true,
            ProxyError::PolicyRateLimit { callers_bucket, .. } => !callers_bucket,
            _ => false,
        };
        if shared && crate::platform_errors::active() {
            return None;
        }
        self.rate_limit_detail()
    }

    /// [`Self::retry_after_secs`] as the caller is sent it. A hidden budget's
    /// reset time is a figure of that budget, so it is not sent either.
    fn caller_retry_after_secs(&self) -> Option<u64> {
        match self {
            ProxyError::BudgetExceeded(r) if hides_budget_scope(r) => None,
            _ => self.retry_after_secs(),
        }
    }

    /// The status the caller is answered with: [`Self::status`], or in
    /// platform mode the status of the class the error reaches the caller
    /// as. Telemetry keeps reading [`Self::status`].
    fn caller_status(&self) -> StatusCode {
        if let Some(answered) = crate::platform_errors::caller_error(self) {
            return answered.caller_status();
        }
        crate::platform_errors::caller_class(self).map_or_else(|| self.status(), |c| c.status())
    }
}

/// Write the retry / rate-limit headers both envelopes owe a rejection.
///
/// Shared so the OpenAI-shape and Anthropic-shape renderers cannot
/// drift: `/v1/messages` answering a 429 without the headers
/// `/v1/chat/completions` carries would be the same class of bug #336
/// fixed for the body.
///
/// `X-RateLimit-Reset` is delta-seconds, deliberately the same number
/// as `Retry-After` for a windowed dimension — a caller may read either
/// one and back off identically, with no clock sync against the
/// gateway. `X-RateLimit-Scope` is an AISIX extension naming which
/// dimension fired; without it `x-ratelimit-limit: 100` cannot be told
/// apart between an rpm, a tpm and a concurrency cap, whose units all
/// differ.
fn apply_retry_headers(
    response: &mut Response,
    retry_after_secs: Option<u64>,
    detail: Option<aisix_ratelimit::LimitDetail>,
) {
    let headers = response.headers_mut();
    let mut set = |name: &'static str, value: String| {
        if let Ok(v) = HeaderValue::from_str(&value) {
            headers.insert(name, v);
        }
    };
    if let Some(secs) = retry_after_secs {
        set("retry-after", secs.to_string());
    }
    if let Some(d) = detail {
        set("x-ratelimit-limit", d.limit.to_string());
        set("x-ratelimit-remaining", d.remaining.to_string());
        set("x-ratelimit-reset", d.reset_secs.to_string());
        set("x-ratelimit-scope", d.dimension.to_string());
    }
}

/// The caller-facing text of [`ProxyError::McpUserOAuth`].
fn mcp_user_oauth_message(failure: &aisix_mcp::UserOAuthFailure, server: &str) -> String {
    match failure {
        aisix_mcp::UserOAuthFailure::AuthorizationRequired => {
            format!("MCP server '{server}' requires this user to sign in")
        }
        aisix_mcp::UserOAuthFailure::NoSubject => format!(
            "this gateway credential has no user, so it cannot be used with MCP server \
             '{server}', which signs in each user individually"
        ),
        aisix_mcp::UserOAuthFailure::Misconfigured => format!(
            "MCP server '{server}' cannot sign in users as configured; an administrator must \
             check its OAuth settings and the MCP authentication settings"
        ),
        aisix_mcp::UserOAuthFailure::Unavailable => format!(
            "this user's upstream token for MCP server '{server}' is temporarily unavailable; \
             try again"
        ),
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let challenge = self.auth_challenge();
        let status = self.caller_status();
        let retry_after = self.caller_retry_after_secs();
        let rate_limit_detail = self.caller_rate_limit_detail();
        let upgrade_reject = matches!(self, ProxyError::WebSocketUpgradeRequired { .. });
        let body = self.envelope();
        let mut response = (status, Json(body)).into_response();
        apply_retry_headers(&mut response, retry_after, rate_limit_detail);
        if let Some(challenge) = challenge {
            response.extensions_mut().insert(challenge);
        }
        // RFC 9110: a 426 must name the protocol to switch to (§15.5.22)
        // and a 405 must list the allowed methods (§15.5.6; GET implies
        // HEAD on this route). axum's bare rejection omitted both, but the
        // response is the gateway's own now.
        if upgrade_reject {
            match status {
                StatusCode::UPGRADE_REQUIRED => {
                    response
                        .headers_mut()
                        .insert("upgrade", HeaderValue::from_static("websocket"));
                }
                StatusCode::METHOD_NOT_ALLOWED => {
                    response
                        .headers_mut()
                        .insert("allow", HeaderValue::from_static("GET, HEAD"));
                }
                _ => {}
            }
        }
        response
    }
}

/// Anthropic-shape error envelope serialized on the wire.
///
/// Matches the shape Anthropic SDKs parse — `body.type === "error"`
/// is the discriminator the official SDK branches on
/// (anthropic-sdk-python `_response.py::_to_api_error`). The nested
/// `error.type` carries the DP's stable taxonomy (the same string
/// the OpenAI envelope's `error.type` carries), so SDKs that branch
/// on the inner type still see the gateway-normalized value
/// (e.g. `"upstream_error"` per ai-gateway#327).
#[derive(Debug, Serialize, Clone)]
struct AnthropicErrorEnvelope {
    /// Top-level discriminator. Always `"error"` for error envelopes.
    #[serde(rename = "type")]
    discriminator: &'static str,
    error: AnthropicErrorBody,
}

#[derive(Debug, Serialize, Clone)]
struct AnthropicErrorBody {
    #[serde(rename = "type")]
    kind: String,
    message: String,
    /// Set on a guardrail refusal ([`anthropic_guardrail_code`]): `type` is
    /// pinned to the SDK's closed literal, so without it a policy block and
    /// a malformed request are the same 422. Set too on every error platform
    /// mode replaces, to the class's code, for the same reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
    /// The field of the caller's request an invalid-request class names, in
    /// platform mode; the OpenAI envelope carries the same as `param`.
    #[serde(skip_serializing_if = "Option::is_none")]
    param: Option<String>,
}

impl AnthropicErrorEnvelope {
    /// The envelope of a platform error class.
    fn platform(class: &crate::platform_errors::PlatformErrorClass) -> Self {
        AnthropicErrorEnvelope {
            discriminator: "error",
            error: AnthropicErrorBody {
                kind: class.anthropic_kind().to_string(),
                message: crate::platform_errors::platform_message(class).to_string(),
                code: Some(class.code()),
                param: match class {
                    crate::platform_errors::PlatformErrorClass::InvalidRequest { param } => {
                        param.clone()
                    }
                    _ => None,
                },
            },
        }
    }
}

/// The Anthropic-shape body of a platform error class, shared by the HTTP
/// response and the `event: error` frame that ends a `/v1/messages` stream.
pub(crate) fn anthropic_platform_body(
    class: &crate::platform_errors::PlatformErrorClass,
) -> String {
    serde_json::to_string(&AnthropicErrorEnvelope::platform(class)).unwrap_or_default()
}

/// The Anthropic envelope's `error.code` for a guardrail refusal, shared by
/// the HTTP 422 body and the streaming `event: error` frame of
/// `/v1/messages` and `/v1/messages/count_tokens`.
///
/// `error.type` cannot carry the signal there: it stays
/// `invalid_request_error`, because the Anthropic SDK types it as a closed
/// literal and a `content_filter` type once failed its typed parse. The
/// code does: `content_filter` for a policy block, and for a fail-closed
/// refusal the same `guardrail_unavailable` the OpenAI envelope carries.
pub(crate) fn anthropic_guardrail_code(unavailable: Option<&str>) -> &'static str {
    if unavailable.is_some() {
        "guardrail_unavailable"
    } else {
        "content_filter"
    }
}

/// Map an HTTP status code to the Anthropic-canonical `error.type`
/// string (the SDK's `ErrorType` literal at
/// `anthropic-sdk-python/src/anthropic/types/shared/error_type.py`).
///
/// This deliberately diverges from the DP-stable OpenAI-shape inner
/// taxonomy (`upstream_error`, `model_not_found`, …) because the
/// Anthropic SDK's `ErrorType` is a strict `Literal[...]` — non-
/// canonical strings on `error.type` are static-type violations for
/// any customer doing `isinstance(e, anthropic.RateLimitError)` plus
/// `e.body['error']['type'] == 'rate_limit_error'`. Per CLAUDE.md §7
/// reference-implementation rule, this mapping mirrors the established
/// ecosystem's Anthropic status-to-type table verbatim — divergence from
/// the established ecosystem here would silently break Claude SDK users.
///
/// (The OpenAI envelope's inner `error.type` keeps the DP-stable
/// strings per ai-gateway#327; that contract is unchanged on
/// `/v1/chat/completions`.)
pub(crate) fn anthropic_kind_from_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 => "overloaded_error",
        // 408 timeout maps to `timeout_error` in the SDK literal; the
        // gateway doesn't emit 408 today (timeouts surface as 502 via
        // the Bridge), but the case is kept for completeness.
        408 => "timeout_error",
        // 500 / 502 / 504-599 plus anything outside the recognised
        // codes fall back to `api_error` — the SDK literal's catch-all
        // for generic upstream / server faults.
        _ => "api_error",
    }
}

impl ProxyError {
    /// Render this error as an Anthropic-shape `{type:"error", error:
    /// {type, message}}` HTTP response, plus `error.code` on a guardrail
    /// refusal ([`anthropic_guardrail_code`]). Used by `/v1/messages` so the
    /// Anthropic SDK's envelope parser sees a shape the official
    /// SDK and the broader ecosystem both treat as canonical.
    ///
    /// **Inner `error.type` policy:** maps from the HTTP status code
    /// to the Anthropic SDK's `ErrorType` literal via
    /// [`anthropic_kind_from_status`] — NOT the DP-stable OpenAI-shape
    /// inner taxonomy. The Anthropic SDK's `ErrorType` is a strict
    /// `Literal[...]`, so emitting DP-internal strings like
    /// `"upstream_error"` would break customers branching on
    /// `error.type`. The DP-stable taxonomy is preserved on the
    /// OpenAI envelope only (ai-gateway#327); the Anthropic envelope
    /// follows ecosystem convention.
    ///
    /// Reuses [`Self::envelope`] for the 4xx/5xx message-classification
    /// and upstream-message logic so the two envelope
    /// renderers can't drift on those rules.
    pub fn into_anthropic_response(self) -> Response {
        let status = self.caller_status();
        let retry_after = self.caller_retry_after_secs();
        let rate_limit_detail = self.caller_rate_limit_detail();
        let mut response = (status, Json(self.anthropic_envelope())).into_response();
        apply_retry_headers(&mut response, retry_after, rate_limit_detail);
        response
    }

    /// The body [`Self::into_anthropic_response`] sends.
    pub(crate) fn anthropic_body(&self) -> String {
        serde_json::to_string(&self.anthropic_envelope()).unwrap_or_default()
    }

    fn anthropic_envelope(&self) -> AnthropicErrorEnvelope {
        if let Some(answered) = crate::platform_errors::caller_error(self) {
            return answered.anthropic_envelope();
        }
        if let Some(class) = crate::platform_errors::caller_class(self) {
            return AnthropicErrorEnvelope::platform(&class);
        }
        let code = match self {
            ProxyError::ContentFiltered { unavailable, .. } => {
                Some(anthropic_guardrail_code(unavailable.as_deref()))
            }
            _ => None,
        };
        AnthropicErrorEnvelope {
            discriminator: "error",
            error: AnthropicErrorBody {
                kind: anthropic_kind_from_status(self.status()).to_string(),
                // Reuse OpenAI envelope only for the SAFE-MESSAGE logic
                // (the upstream message, translated on a 4xx).
                message: self.envelope().error.message,
                code,
                param: None,
            },
        }
    }
}

/// Map an axum `JsonRejection` (the body-extractor failure on a POST
/// handler) onto the internal [`ProxyError`] taxonomy. The caller
/// decides the wire envelope (`into_response` for OpenAI shape /
/// `into_anthropic_response` for the Anthropic shape) — this helper
/// only classifies the failure.
///
/// Shared by `/v1/messages` and `/v1/messages/count_tokens` so the two
/// Anthropic-protocol handlers can't drift on the discrimination rules
/// below:
///
/// - `BytesRejection` is a composite rejection whose inner
///   `FailedToBufferBody` has two variants: `LengthLimitError`
///   (`413 PAYLOAD_TOO_LARGE` — the configured body cap was exceeded
///   during read; the chunked / no-Content-Length case the
///   `enforce_request_body_limit` middleware can't catch up front) and
///   `UnknownBodyError` (`400 BAD_REQUEST` — a transport-side body-read
///   failure, e.g. peer reset mid-body). They MUST map to
///   `RequestTooLarge` vs `InvalidRequest` respectively, because the
///   Anthropic SDK's non-retriable-cap branch assumes a true cap hit —
///   mislabelling a transport failure as `request_too_large` breaks it.
///   Discriminate via the rejection's own `.status()`.
/// - `JsonRejection` is `#[non_exhaustive]`, so the fallback arm catches
///   today's `JsonDataError` / `JsonSyntaxError` / `MissingJsonContentType`
///   AND any future variant axum adds, defaulting to a 400
///   `invalid_request_error` until each gets an explicit policy.
pub(crate) fn proxy_error_from_json_rejection(
    rej: axum::extract::rejection::JsonRejection,
    limit_bytes: usize,
) -> ProxyError {
    use axum::extract::rejection::JsonRejection;
    match rej {
        JsonRejection::BytesRejection(inner) if inner.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            ProxyError::RequestTooLarge { limit_bytes }
        }
        JsonRejection::BytesRejection(_) => {
            ProxyError::InvalidRequest("failed to read request body".into())
        }
        // Valid JSON whose value does not fit the request type: name the
        // field it failed at (`messages[0].content: ...`, `max_tokens:
        // invalid value: integer `-1`, expected u32`) rather than leave the
        // caller to guess.
        JsonRejection::JsonDataError(e) => {
            let text = e.body_text();
            let detail = text
                .strip_prefix("Failed to deserialize the JSON body into the target type: ")
                .unwrap_or(&text);
            ProxyError::InvalidRequest(format!("invalid request body: {detail}"))
        }
        _ => ProxyError::InvalidRequest("invalid JSON request body".into()),
    }
}

/// [`proxy_error_from_json_rejection`]'s sibling for handlers that take
/// the raw `Bytes` extractor (batches / fine-tuning): same 413-vs-400
/// discrimination, no JSON layer.
pub(crate) fn proxy_error_from_bytes_rejection(
    rej: axum::extract::rejection::BytesRejection,
    limit_bytes: usize,
) -> ProxyError {
    if rej.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ProxyError::RequestTooLarge { limit_bytes }
    } else {
        ProxyError::InvalidRequest("failed to read request body".into())
    }
}

/// Map a multipart read failure, preserving axum's 413 discrimination:
/// an over-cap stream or part is a real `RequestTooLarge` (axum's
/// `MultipartError::status()` already classifies it 413); everything
/// else stays the 400 the call site describes via `context`. Without
/// this, an over-limit chunked upload surfaced as a generic 400
/// `invalid_request_error` instead of `request_too_large`.
pub(crate) fn proxy_error_from_multipart(
    err: axum::extract::multipart::MultipartError,
    limit_bytes: usize,
    context: &str,
) -> ProxyError {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ProxyError::RequestTooLarge { limit_bytes }
    } else {
        ProxyError::InvalidRequest(format!("{context}: {err}"))
    }
}

/// Cap for manual `axum::body::to_bytes` reads: the effective endpoint limit
/// with the explicit `0` = "no cap" sentinel widened to `usize::MAX`.
pub(crate) fn body_read_cap(limit_bytes: usize) -> usize {
    if limit_bytes == 0 {
        usize::MAX
    } else {
        limit_bytes
    }
}

/// Whether a manual body read failed because it hit the length cap
/// (→ 413) rather than a transport fault (→ 400). `axum::body::to_bytes`
/// folds both into one opaque `axum::Error`; the cap case carries
/// `http_body_util::LengthLimitError` in its source chain.
pub(crate) fn is_length_limit_error(err: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if e.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = e.source();
    }
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Run `f` as a request served in platform mode would run.
    pub(crate) fn in_platform_mode<T>(f: impl FnOnce() -> T) -> T {
        let cell = std::sync::Arc::new(
            crate::attribution::RequestAttribution::default().in_platform_mode(true),
        );
        crate::attribution::sync_scope(&cell, f)
    }

    /// The OpenAI-shape body the caller of a platform-mode request receives
    /// for `err`.
    pub(crate) fn render_platform(err: &ProxyError) -> serde_json::Value {
        in_platform_mode(|| serde_json::to_value(err.envelope()).expect("an envelope serializes"))
    }

    /// A fail-closed guardrail refusal names its failure tag, which can name
    /// the guardrail's vendor and the state of its account. In platform mode
    /// the caller is told the guardrail could not evaluate the request, by
    /// name and code, without the tag; the error itself keeps it for the
    /// usage row. Every shape: the buffered bodies and the stream frames.
    #[test]
    fn in_platform_mode_a_fail_closed_refusal_names_no_tag() {
        let err = guardrail_block_error("request", Some("pii-check"), Some("bedrock_throttled"));
        let off = serde_json::to_value(err.envelope()).unwrap();
        assert_eq!(
            off["error"]["message"],
            "request rejected: guardrail 'pii-check' could not evaluate it (bedrock_throttled)"
        );
        let on = render_platform(&err);
        assert_eq!(
            on["error"]["message"],
            "request rejected: guardrail 'pii-check' could not evaluate it"
        );
        assert_eq!(on["error"]["code"], "guardrail_unavailable");
        let anthropic = in_platform_mode(|| err.anthropic_body());
        assert!(!anthropic.contains("bedrock"), "{anthropic}");
        assert!(anthropic.contains("guardrail_unavailable"), "{anthropic}");
        assert!(
            err.to_string().contains("bedrock_throttled"),
            "the error keeps it"
        );

        let payload =
            in_platform_mode(|| guardrail_block_frame_payload(None, Some("bedrock_throttled")));
        assert!(!payload.contains("bedrock"), "{payload}");
        let frame = in_platform_mode(|| {
            crate::messages::guardrail_block_frame(None, Some("bedrock_throttled"))
        });
        assert!(
            !frame.contains("bedrock") && frame.contains("could not evaluate"),
            "{frame}"
        );
        assert!(
            crate::messages::guardrail_block_frame(None, Some("bedrock_throttled"))
                .contains("bedrock_throttled")
        );
    }

    /// #1093 carries the transport cause on a timeout so an
    /// operator can tell a `connect_timeout` from an expired request
    /// budget. That cause names the upstream host, so it must reach the
    /// logs and telemetry (`Display`) but NOT the caller's envelope —
    /// same split the 5xx path already enforces.
    #[test]
    fn timeout_cause_reaches_logs_but_not_the_caller() {
        let err = ProxyError::Bridge(aisix_gateway::BridgeError::Timeout {
            elapsed_ms: 5_002,
            cause: "error sending request for url (http://10.1.2.3:8080/v1/messages): \
                    client error (Connect): tcp connect error: deadline has elapsed"
                .to_string(),
        });

        // Operator-facing: the full chain, which is what the WARN log line
        // and the per-attempt `error_message` are built from.
        let logged = err.to_string();
        assert!(logged.contains("deadline has elapsed"), "{logged}");
        assert!(logged.contains("10.1.2.3"), "{logged}");

        // Customer-facing: the bare sentence, byte-identical to what it
        // was before `cause` existed, with no internal topology in it.
        let envelope = err.envelope();
        assert_eq!(
            envelope.error.message,
            "upstream request timed out after 5002ms"
        );
        assert!(!envelope.error.message.contains("10.1.2.3"));
        assert_eq!(err.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn missing_auth_maps_to_401_invalid_api_key() {
        let e = ProxyError::MissingAuth("Authorization");
        assert_eq!(e.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(e.kind(), "invalid_api_key");
    }

    /// AISIX-Cloud#1143: the scope-failure variant split out of
    /// `JwtClaimsRejected` must stay byte-identical on the wire — same
    /// 403, `permission_denied` type, `jwt_claims_rejected` code and
    /// message — so `/v1` callers cannot observe the split. The only
    /// difference is the challenge classification for the `/mcp`
    /// middleware.
    #[test]
    fn insufficient_scope_renders_identically_to_claims_rejected() {
        let scope_err = ProxyError::JwtInsufficientScope {
            required_scopes: vec!["mcp:tools".into()],
        };
        let policy_err = ProxyError::JwtClaimsRejected;
        assert_eq!(scope_err.status(), policy_err.status());
        assert_eq!(scope_err.kind(), policy_err.kind());
        let a = serde_json::to_value(scope_err.envelope()).unwrap();
        let b = serde_json::to_value(policy_err.envelope()).unwrap();
        assert_eq!(a, b, "wire envelopes must not diverge");
    }

    #[test]
    fn auth_challenge_classification_covers_exactly_the_curable_failures() {
        assert!(matches!(
            ProxyError::MissingAuth("Authorization").auth_challenge(),
            Some(AuthChallenge::MissingCredentials)
        ));
        for e in [
            ProxyError::InvalidApiKey,
            ProxyError::ApiKeyExpired,
            ProxyError::ApiKeyDisabled,
            ProxyError::JwtInvalid,
            ProxyError::JwtExpired,
            ProxyError::JwtIdentityUnmapped,
        ] {
            assert!(matches!(
                e.auth_challenge(),
                Some(AuthChallenge::InvalidToken)
            ));
        }
        let scope_err = ProxyError::JwtInsufficientScope {
            required_scopes: vec!["mcp:tools".into()],
        };
        match scope_err.auth_challenge() {
            Some(AuthChallenge::InsufficientScope { required_scopes }) => {
                assert_eq!(required_scopes, vec!["mcp:tools".to_string()]);
            }
            other => panic!("expected InsufficientScope, got {other:?}"),
        }
        // A bound_claims policy denial is not curable by re-consenting:
        // no challenge, ever. Same for the gateway-side JWKS fault.
        assert!(ProxyError::JwtClaimsRejected.auth_challenge().is_none());
        assert!(ProxyError::JwksUnavailable.auth_challenge().is_none());

        use aisix_mcp::TokenExchangeFailure as F;
        for f in [F::NoSubject, F::SubjectRejected { claims: None }] {
            assert!(matches!(
                ProxyError::McpTokenExchange(f).auth_challenge(),
                Some(AuthChallenge::InvalidToken)
            ));
        }
        match ProxyError::McpTokenExchange(F::SubjectRejected {
            claims: Some("{}".into()),
        })
        .auth_challenge()
        {
            Some(AuthChallenge::InsufficientClaims { claims }) => assert_eq!(claims, "{}"),
            other => panic!("expected InsufficientClaims, got {other:?}"),
        }
        for f in [F::Misconfigured, F::Unavailable] {
            assert!(ProxyError::McpTokenExchange(f).auth_challenge().is_none());
        }
    }

    #[tokio::test]
    async fn into_response_attaches_the_challenge_extension() {
        let resp = ProxyError::MissingAuth("Authorization").into_response();
        assert!(matches!(
            resp.extensions().get::<AuthChallenge>(),
            Some(AuthChallenge::MissingCredentials)
        ));
        // Non-auth errors carry no marker.
        let resp = ProxyError::ModelNotFound("m".into()).into_response();
        assert!(resp.extensions().get::<AuthChallenge>().is_none());
    }

    #[test]
    fn model_forbidden_is_403_permission_denied() {
        let e = ProxyError::ModelForbidden("gpt-4o".into());
        assert_eq!(e.status(), StatusCode::FORBIDDEN);
        assert_eq!(e.kind(), "permission_denied");
    }

    #[test]
    fn platform_mode_answers_a_forbidden_model_like_an_unknown_one() {
        assert_eq!(
            render_platform(&ProxyError::ModelForbidden("x".into())),
            render_platform(&ProxyError::ModelNotFound("x".into()))
        );
    }

    /// Status, headers and body, in both envelopes. Only the caller's answer
    /// changes: the records an operator reads keep the refusal.
    #[tokio::test]
    async fn platform_mode_answers_a_forbidden_model_like_an_unknown_one_in_every_envelope() {
        async fn parts(resp: Response) -> (StatusCode, axum::http::HeaderMap, bytes::Bytes) {
            let (head, body) = resp.into_parts();
            let body = axum::body::to_bytes(body, 65536).await.unwrap();
            (head.status, head.headers, body)
        }
        let forbidden = || ProxyError::ModelForbidden("x".into());
        let unknown = || ProxyError::ModelNotFound("x".into());

        let (a, b) = in_platform_mode(|| (forbidden().into_response(), unknown().into_response()));
        let (a, b) = (parts(a).await, parts(b).await);
        assert_eq!(a.0, StatusCode::NOT_FOUND);
        assert_eq!(a, b);
        let (a, b) = in_platform_mode(|| {
            (
                forbidden().into_anthropic_response(),
                unknown().into_anthropic_response(),
            )
        });
        assert_eq!(parts(a).await, parts(b).await);
        assert_eq!(
            in_platform_mode(|| forbidden().anthropic_body()),
            in_platform_mode(|| unknown().anthropic_body())
        );

        // What the operator reads is written in the same platform-mode scope
        // the caller's answer is, and still names the real refusal.
        in_platform_mode(|| {
            let err = forbidden();
            assert_eq!(err.status(), StatusCode::FORBIDDEN);
            assert_eq!(crate::attempt::error_class(&err), "permission_denied");
            assert_eq!(
                crate::attempt::gateway_error_message(&err),
                "API key is not allowed to use model \"x\""
            );
        });
        // Off platform mode the refusal is the 403 it always was.
        assert_eq!(forbidden().into_response().status(), StatusCode::FORBIDDEN);
        assert_eq!(
            forbidden().into_anthropic_response().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn model_ip_restricted_is_403_with_ip_restricted_code() {
        let e = ProxyError::ModelIpRestricted("gpt-4o".into());
        assert_eq!(e.status(), StatusCode::FORBIDDEN);
        assert_eq!(e.kind(), "permission_denied");
        let json = serde_json::to_value(e.envelope()).unwrap();
        assert_eq!(json["error"]["type"], "permission_denied");
        assert_eq!(json["error"]["code"], "ip_restricted");
        // Message must stay generic — never echo the configured CIDR list.
        let msg = json["error"]["message"].as_str().unwrap();
        assert!(msg.contains("not allowed to access this model"));
        assert!(!msg.contains("gpt-4o"));
    }

    #[test]
    fn bridge_error_inherits_status_and_type() {
        let bridge_err = BridgeError::upstream_status(429, "rate limited");
        let wrapped = ProxyError::Bridge(bridge_err);
        assert_eq!(wrapped.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(wrapped.kind(), "upstream_error");
    }

    #[test]
    fn upstream_429_forwards_retry_after_hint() {
        // An upstream 429 carrying a parsed Retry-After must surface
        // through retry_after_secs so the response writer emits the
        // header SDKs back off on (#144).
        let wrapped = ProxyError::Bridge(BridgeError::upstream_status_with_retry_after(
            429,
            "rate limited",
            Some(std::time::Duration::from_secs(30)),
        ));
        assert_eq!(wrapped.retry_after_secs(), Some(30));
    }

    #[test]
    fn upstream_429_without_hint_has_no_retry_after() {
        let wrapped = ProxyError::Bridge(BridgeError::upstream_status(429, "rate limited"));
        assert_eq!(wrapped.retry_after_secs(), None);
    }

    #[test]
    fn non_429_upstream_does_not_forward_retry_after() {
        // Only 429 carries a meaningful hint; a stray Retry-After on a
        // 5xx must not leak to the client.
        let wrapped = ProxyError::Bridge(BridgeError::upstream_status_with_retry_after(
            503,
            "unavailable",
            Some(std::time::Duration::from_secs(30)),
        ));
        assert_eq!(wrapped.retry_after_secs(), None);
    }

    #[test]
    fn bridge_5xx_collapses_via_bridge_error_mapping() {
        let bridge_err = BridgeError::upstream_status(503, "busy");
        let wrapped = ProxyError::Bridge(bridge_err);
        assert_eq!(wrapped.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn all_candidates_unavailable_is_503_with_optional_retry_after() {
        let with_hint = ProxyError::AllCandidatesUnavailable {
            retry_after_secs: Some(42),
        };
        assert_eq!(with_hint.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(with_hint.kind(), "all_candidates_unavailable");
        assert_eq!(with_hint.retry_after_secs(), Some(42));

        let no_hint = ProxyError::AllCandidatesUnavailable {
            retry_after_secs: None,
        };
        assert_eq!(no_hint.retry_after_secs(), None);
    }

    #[test]
    fn envelope_omits_null_param_and_code_on_wire() {
        let env = ProxyError::ModelNotFound("x".into()).envelope();
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["error"]["type"], "model_not_found");
        assert!(json["error"].get("param").is_none());
        assert!(json["error"].get("code").is_none());
    }

    // ─── Anthropic envelope (#336) ────────────────────────────────────
    //
    // /v1/messages must emit `{type:"error", error:{type, message}}`
    // — the Anthropic-SDK strict envelope discriminator
    // (anthropic-sdk-python `_response.py::_to_api_error`). These tests
    // assert the wire shape AND that the DP-stable inner `error.type`
    // taxonomy (`upstream_error`, `invalid_api_key`, …) is preserved
    // unchanged from the OpenAI envelope per ai-gateway#327.

    use axum::body::to_bytes;

    async fn body_to_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Shared envelope-shape assertion used across every Anthropic
    /// envelope test below — keeps the contract surface tight against
    /// a future regression that flipped any single error variant back
    /// to the OpenAI envelope.
    async fn assert_anthropic_envelope(
        resp: Response,
        expected_status: StatusCode,
        expected_kind: &str,
    ) -> serde_json::Value {
        assert_eq!(resp.status(), expected_status);
        let json = body_to_json(resp).await;
        assert_eq!(
            json["type"], "error",
            "top-level discriminator must be the literal string \"error\""
        );
        assert_eq!(
            json["error"]["type"], expected_kind,
            "inner error.type must follow Anthropic SDK ErrorType literal"
        );
        assert!(
            json["error"]["message"].is_string(),
            "error.message must be present and a string"
        );
        assert!(
            json["error"].get("code").is_none(),
            "OpenAI-only field `code` must be absent from the Anthropic envelope"
        );
        assert!(
            json["error"].get("param").is_none(),
            "OpenAI-only field `param` must be absent from the Anthropic envelope"
        );
        json
    }

    #[test]
    fn a_guardrail_that_could_not_evaluate_does_not_claim_a_content_decision() {
        // The whole defect in one assertion: the two refusals shared a
        // sentence, so a broken fail-closed row looked exactly like a
        // working policy to the caller.
        let policy = guardrail_block_error("request", Some("my-guard"), None);
        let broken = guardrail_block_error("request", Some("my-guard"), Some("custom_no_verdict"));
        assert_eq!(
            policy.to_string(),
            "request blocked by content policy (guardrail 'my-guard')"
        );
        assert_ne!(policy.to_string(), broken.to_string());
        assert!(broken.to_string().contains("could not evaluate"));
        assert!(!broken.to_string().contains("content policy"));
        // The tag is named so an operator can grep the message straight
        // onto the dashboard series that carries the same value.
        assert!(broken.to_string().contains("custom_no_verdict"));
        // Both still name the firing row (#519 B.4b) and neither carries
        // matched content (#153).
        assert!(broken.to_string().contains("my-guard"));
    }

    #[test]
    fn an_unattributed_availability_refusal_still_says_which_kind_of_refusal_it_is() {
        let msg = guardrail_block_message("response", None, Some("output_buffer_exceeded"));
        assert_eq!(
            msg,
            "response rejected: a guardrail could not evaluate it (output_buffer_exceeded)"
        );
    }

    #[test]
    fn the_availability_refusal_keeps_the_status_and_type_but_adds_a_machine_readable_code() {
        let policy = guardrail_block_error("request", Some("g"), None);
        let broken = guardrail_block_error("request", Some("g"), Some("custom_unknown_action"));
        // Same shipped contract: an SDK branching on status or type sees
        // no change, and an operator alert on `content_filter` keeps
        // counting both.
        assert_eq!(policy.status(), broken.status());
        assert_eq!(policy.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(policy.kind(), "content_filter");
        assert_eq!(broken.kind(), "content_filter");
        // The code is the new discriminator.
        assert_eq!(policy.envelope().error.code, None);
        assert_eq!(
            broken.envelope().error.code.as_deref(),
            Some("guardrail_unavailable")
        );
    }

    #[tokio::test]
    async fn anthropic_envelope_404_maps_to_not_found_error() {
        let err = ProxyError::ModelNotFound("claude-x".into());
        let resp = err.into_anthropic_response();
        let json = assert_anthropic_envelope(resp, StatusCode::NOT_FOUND, "not_found_error").await;
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("claude-x"),
            "error message must surface the missing model id",
        );
    }

    #[tokio::test]
    async fn anthropic_envelope_401_maps_to_authentication_error() {
        let err = ProxyError::MissingAuth("Authorization");
        let resp = err.into_anthropic_response();
        assert_anthropic_envelope(resp, StatusCode::UNAUTHORIZED, "authentication_error").await;
    }

    #[tokio::test]
    async fn anthropic_envelope_403_maps_to_permission_error() {
        let err = ProxyError::ModelForbidden("gpt-4o".into());
        let resp = err.into_anthropic_response();
        assert_anthropic_envelope(resp, StatusCode::FORBIDDEN, "permission_error").await;
    }

    #[tokio::test]
    async fn anthropic_envelope_403_ip_restricted_maps_to_permission_error() {
        let err = ProxyError::ModelIpRestricted("claude-sonnet-4-5".into());
        let resp = err.into_anthropic_response();
        assert_anthropic_envelope(resp, StatusCode::FORBIDDEN, "permission_error").await;
    }

    #[tokio::test]
    async fn anthropic_envelope_400_maps_to_invalid_request_error() {
        let err = ProxyError::InvalidRequest("`max_tokens` is required".into());
        let resp = err.into_anthropic_response();
        assert_anthropic_envelope(resp, StatusCode::BAD_REQUEST, "invalid_request_error").await;
    }

    #[tokio::test]
    async fn anthropic_envelope_413_maps_to_request_too_large() {
        let err = ProxyError::RequestTooLarge {
            limit_bytes: 1_048_576,
        };
        let resp = err.into_anthropic_response();
        assert_anthropic_envelope(resp, StatusCode::PAYLOAD_TOO_LARGE, "request_too_large").await;
    }

    /// Content-filter rejections share 422 with the OpenAI side, and the
    /// Anthropic-canonical 422 is `invalid_request_error` (the SDK literal
    /// has no content-filter type). AISIX-Cloud#726: so `error.code` is the
    /// only field that tells a guardrail refusal from a malformed request —
    /// and a broken fail-closed row from a policy hit.
    #[tokio::test]
    async fn anthropic_envelope_422_content_filter_maps_to_invalid_request_error_with_a_code() {
        async fn body(err: ProxyError) -> serde_json::Value {
            let resp = err.into_anthropic_response();
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
            body_to_json(resp).await
        }
        let policy = body(guardrail_block_error("request", Some("gr"), None)).await;
        assert_eq!(policy["error"]["type"], "invalid_request_error");
        assert_eq!(policy["error"]["code"], "content_filter");

        let broken = body(guardrail_block_error(
            "request",
            Some("gr"),
            Some(TAG_UNSCANNABLE_BODY),
        ))
        .await;
        assert_eq!(broken["error"]["type"], "invalid_request_error");
        assert_eq!(broken["error"]["code"], "guardrail_unavailable");
        // The same code the OpenAI envelope carries for a fail-closed row.
        assert_eq!(
            guardrail_block_error("request", None, Some(TAG_UNSCANNABLE_BODY))
                .envelope()
                .error
                .code
                .as_deref(),
            Some("guardrail_unavailable"),
        );

        assert!(policy["error"].get("param").is_none());
    }

    fn budget_exceeded(scope: &str, scope_ref: &str, limit: &str, spent: &str) -> ProxyError {
        ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
            message: format!("{scope} budget '{scope_ref}' exceeded (${limit}/month)."),
            scope: Some(scope.into()),
            scope_kind: serde_json::from_value(serde_json::json!(scope)).ok(),
            scope_ref: Some(scope_ref.into()),
            limit_usd: Some(limit.into()),
            spent_usd: Some(spent.into()),
            period: Some("month".into()),
            period_resets_at: Some("2026-12-01T00:00:00Z".into()),
            retry_after_seconds: Some(259_200),
            ..Default::default()
        }))
    }

    fn policy_limited(id: &str, name: &str, callers_bucket: bool) -> ProxyError {
        ProxyError::PolicyRateLimit {
            source: aisix_ratelimit::RateLimitError::Requests {
                scope: aisix_core::RateLimitScope::Requests,
                detail: aisix_ratelimit::LimitDetail {
                    dimension: "rpd",
                    limit: 5_000,
                    remaining: 0,
                    reset_secs: 3_600,
                },
            },
            policy_id: id.into(),
            policy_name: name.into(),
            callers_bucket,
        }
    }

    #[test]
    fn a_team_budget_429_shows_the_teams_own_figures() {
        for scope in ["api_key", "member", "team", "team_member"] {
            let b = render_platform(&budget_exceeded(scope, "t1", "10.00", "10.02"));
            let e = &b["error"];
            assert_eq!(e["scope"], scope);
            assert_eq!(e["scope_ref"], "t1");
            assert_eq!(e["limit_usd"], "10.00");
            assert_eq!(e["spent_usd"], "10.02");
            assert_eq!(e["code"], "budget_exceeded");
        }
    }

    #[test]
    fn a_platform_side_budget_429_reveals_no_id_and_no_figure() {
        for scope in ["org", "environment", "tag", "provider_key"] {
            let err = budget_exceeded(scope, "pk-7", "500.00", "500.10");
            let b = render_platform(&err);
            let s = b.to_string();
            assert!(
                !s.contains("pk-7") && !s.contains("500") && !s.contains("2026"),
                "{s}"
            );
            assert!(!s.contains(scope), "{s}");
            assert_eq!(b["error"]["message"], "Spending limit reached.");
            assert_eq!(b["error"]["code"], "budget_exceeded");
            assert_eq!(b["error"]["type"], "billing_error");
            assert!(b["error"].get("scope").is_none());
            assert!(b["error"].get("retry_after_seconds").is_none());
        }
    }

    #[test]
    fn a_budget_429_without_a_scope_is_hidden_in_platform_mode() {
        let err = ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
            message: "spend ceiling reached for policy acme".into(),
            limit_usd: Some("500.00".into()),
            ..Default::default()
        }));
        let s = render_platform(&err).to_string();
        assert!(!s.contains("acme") && !s.contains("500"), "{s}");
    }

    /// The typed scope and the control plane's text must agree on an own
    /// scope; text alone (or a typed platform scope) does not unlock figures.
    #[test]
    fn a_budget_429_needs_the_typed_scope_and_the_text_to_agree() {
        use aisix_core::models::BudgetScope;
        let reason = |kind: Option<BudgetScope>, text: &str| {
            ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
                message: "m".into(),
                scope: Some(text.into()),
                scope_kind: kind,
                scope_ref: Some("ref-1".into()),
                limit_usd: Some("5.00".into()),
                ..Default::default()
            }))
        };
        let shown = |e: &ProxyError| render_platform(e)["error"].get("limit_usd").is_some();
        assert!(shown(&reason(Some(BudgetScope::Team), "team")));
        assert!(!shown(&reason(Some(BudgetScope::ProviderKey), "team")));
        assert!(!shown(&reason(Some(BudgetScope::Team), "provider_key")));
        assert!(!shown(&reason(None, "team")));
    }

    #[test]
    fn an_own_scope_budget_429_with_a_caller_message_names_no_policy() {
        let err = ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
            message: "spend ceiling reached for policy acme-tier".into(),
            caller_message: Some("Spend ceiling reached.".into()),
            scope: Some("team".into()),
            scope_kind: Some(aisix_core::models::BudgetScope::Team),
            ..Default::default()
        }));
        let b = render_platform(&err);
        assert_eq!(b["error"]["message"], "Spend ceiling reached.");
        assert!(!b.to_string().contains("acme-tier"));
        // Telemetry and the operator-facing text keep the policy name.
        assert!(err.to_string().contains("acme-tier"));
        let off = serde_json::to_value(err.envelope()).unwrap();
        assert!(off["error"]["message"]
            .as_str()
            .unwrap()
            .contains("acme-tier"));
    }

    fn requests_detail() -> aisix_ratelimit::RateLimitError {
        aisix_ratelimit::RateLimitError::Requests {
            scope: aisix_core::RateLimitScope::Requests,
            detail: aisix_ratelimit::LimitDetail {
                dimension: "rpd",
                limit: 5_000,
                remaining: 0,
                reset_secs: 3_600,
            },
        }
    }

    fn platform_headers(err: ProxyError) -> axum::http::HeaderMap {
        in_platform_mode(|| err.into_response()).headers().clone()
    }

    #[test]
    fn a_shared_bucket_429_sends_no_ratelimit_figures_in_platform_mode() {
        for err in [
            policy_limited("pol-1", "model-tier", false),
            ProxyError::ModelRateLimit(requests_detail()),
        ] {
            let h = platform_headers(err);
            for name in [
                "x-ratelimit-limit",
                "x-ratelimit-remaining",
                "x-ratelimit-scope",
                "x-ratelimit-reset",
            ] {
                assert!(h.get(name).is_none(), "{name} leaks a shared bucket");
            }
            // The back-off hint is only a window length.
            assert_eq!(h.get("retry-after").unwrap(), "3600");
        }
    }

    #[test]
    fn a_callers_own_bucket_429_keeps_its_ratelimit_figures() {
        for err in [
            policy_limited("pol-1", "team-tier", true),
            ProxyError::RateLimit(requests_detail()),
        ] {
            let h = platform_headers(err);
            assert_eq!(h.get("x-ratelimit-limit").unwrap(), "5000");
            assert_eq!(h.get("x-ratelimit-scope").unwrap(), "rpd");
        }
    }

    #[test]
    fn with_platform_mode_off_the_model_limit_renders_as_the_key_limit_does() {
        let render = |e: ProxyError| {
            let r = e.into_response();
            (r.status(), r.headers().clone())
        };
        let (s1, h1) = render(ProxyError::ModelRateLimit(requests_detail()));
        let (s2, h2) = render(ProxyError::RateLimit(requests_detail()));
        assert_eq!(s1, s2);
        assert_eq!(h1, h2);
        assert_eq!(
            ProxyError::ModelRateLimit(requests_detail()).kind(),
            ProxyError::RateLimit(requests_detail()).kind()
        );
        assert_eq!(
            serde_json::to_value(ProxyError::ModelRateLimit(requests_detail()).envelope()).unwrap(),
            serde_json::to_value(ProxyError::RateLimit(requests_detail()).envelope()).unwrap()
        );
    }

    #[test]
    fn budget_429_with_platform_mode_off_is_unchanged() {
        let b = serde_json::to_value(
            budget_exceeded("provider_key", "pk-7", "500.00", "500.10").envelope(),
        )
        .unwrap();
        assert_eq!(b["error"]["scope_ref"], "pk-7");
        assert_eq!(b["error"]["limit_usd"], "500.00");
        assert!(b["error"]["message"].as_str().unwrap().contains("pk-7"));
    }

    #[test]
    fn a_policy_429_names_no_policy_in_platform_mode() {
        let b = render_platform(&policy_limited("pol-1", "acme-tier", true));
        assert!(b["error"].get("policy").is_none());
        assert!(!b.to_string().contains("acme-tier"));
        assert_eq!(b["error"]["type"], "rate_limit_exceeded");
        let off =
            serde_json::to_value(policy_limited("pol-1", "acme-tier", true).envelope()).unwrap();
        assert_eq!(off["error"]["policy"]["name"], "acme-tier");
    }

    /// Retry-After describes the budget's own reset, so it goes with the
    /// figures: kept for the caller's scope, dropped for a platform one,
    /// and still sent when platform mode is off.
    #[test]
    fn a_budget_429_keeps_retry_after_only_for_the_callers_scope() {
        let header = |err: ProxyError, platform: bool| {
            let resp = if platform {
                in_platform_mode(|| err.into_response())
            } else {
                err.into_response()
            };
            assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
            resp.headers()
                .get("retry-after")
                .map(|v| v.to_str().unwrap().to_string())
        };
        assert_eq!(
            header(budget_exceeded("team", "t1", "1", "2"), true).as_deref(),
            Some("259200")
        );
        assert_eq!(
            header(budget_exceeded("environment", "e", "1", "2"), true),
            None
        );
        assert_eq!(
            header(budget_exceeded("environment", "e", "1", "2"), false).as_deref(),
            Some("259200")
        );
    }

    #[test]
    fn the_anthropic_budget_429_hides_a_platform_side_scope_too() {
        let err = budget_exceeded("provider_key", "pk-7", "500.00", "500.10");
        let (body, resp) =
            in_platform_mode(|| (err.anthropic_body(), err.into_anthropic_response()));
        assert!(
            !body.contains("pk-7") && !body.contains("500") && !body.contains("provider_key"),
            "{body}"
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(v["error"]["message"], "Spending limit reached.");
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get("retry-after").is_none());
    }

    #[tokio::test]
    async fn anthropic_envelope_429_budget_exceeded_maps_to_rate_limit_error() {
        let err = ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
            message: "ak-1".to_string(),
            ..Default::default()
        }));
        let resp = err.into_anthropic_response();
        assert_anthropic_envelope(resp, StatusCode::TOO_MANY_REQUESTS, "rate_limit_error").await;
    }

    #[test]
    fn openai_envelope_budget_exceeded_carries_structured_fields() {
        // prd-09b §5.8: the budget_exceeded 429 lifts the budget's structured
        // reason into the error block. Pin scope / scope_ref / limit_usd /
        // spent_usd / period so a regression that drops them (the old
        // String-only variant) fails here.
        let err = ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
            message: "team budget 'frontend' exceeded ($1.00/month). Resets soon.".into(),
            scope: Some("team".into()),
            scope_ref: Some("team-uuid-1".into()),
            limit_usd: Some("1.00".into()),
            spent_usd: Some("2.00".into()),
            period: Some("month".into()),
            period_resets_at: Some("2026-06-01T00:00:00Z".into()),
            retry_after_seconds: Some(259_200),
            ..Default::default()
        }));
        // The Retry-After *header* must source the same value the body
        // carries — otherwise SDKs (which back off on the header) and
        // the body disagree.
        assert_eq!(err.retry_after_secs(), Some(259_200));
        let v = serde_json::to_value(err.envelope()).unwrap();
        let e = &v["error"];
        assert_eq!(e["type"], "billing_error");
        assert_eq!(e["code"], "budget_exceeded");
        assert_eq!(e["scope"], "team");
        assert_eq!(e["scope_ref"], "team-uuid-1");
        assert_eq!(e["limit_usd"], "1.00");
        assert_eq!(e["spent_usd"], "2.00");
        assert_eq!(e["period"], "month");
        assert_eq!(e["period_resets_at"], "2026-06-01T00:00:00Z");
        assert_eq!(e["retry_after_seconds"], 259_200);
        assert!(e["message"]
            .as_str()
            .unwrap()
            .contains("team budget 'frontend'"));

        // A non-budget error must NOT carry these fields — the flatten
        // omits them so every other error keeps the bare OpenAI shape.
        let other = serde_json::to_value(ProxyError::ModelNotFound("m".into()).envelope()).unwrap();
        assert!(other["error"].get("scope").is_none());
        assert!(other["error"].get("limit_usd").is_none());
    }

    #[tokio::test]
    async fn anthropic_envelope_budget_exceeded_omits_structured_fields() {
        // The structured budget fields are an OpenAI-envelope extension
        // only. The Anthropic /v1/messages error block is the strict
        // {type, message} shape — a fully-populated reason must NOT leak
        // scope / limit_usd etc. into it.
        let err = ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
            message: "team budget 'frontend' exceeded ($1.00/month). Resets soon.".into(),
            scope: Some("team".into()),
            scope_ref: Some("team-uuid-1".into()),
            limit_usd: Some("1.00".into()),
            spent_usd: Some("2.00".into()),
            period: Some("month".into()),
            period_resets_at: Some("2026-06-01T00:00:00Z".into()),
            retry_after_seconds: Some(259_200),
            ..Default::default()
        }));
        let resp = err.into_anthropic_response();
        let json =
            assert_anthropic_envelope(resp, StatusCode::TOO_MANY_REQUESTS, "rate_limit_error")
                .await;
        assert!(json["error"].get("scope").is_none());
        assert!(json["error"].get("scope_ref").is_none());
        assert!(json["error"].get("limit_usd").is_none());
        assert!(json["error"].get("spent_usd").is_none());
    }

    #[tokio::test]
    async fn anthropic_envelope_503_all_candidates_unavailable_maps_to_overloaded_error() {
        let err = ProxyError::AllCandidatesUnavailable {
            retry_after_secs: Some(7),
        };
        let resp = err.into_anthropic_response();
        assert_anthropic_envelope(resp, StatusCode::SERVICE_UNAVAILABLE, "overloaded_error").await;
    }

    #[tokio::test]
    async fn anthropic_envelope_503_carries_retry_after_header() {
        // Anthropic SDK honors the `Retry-After` header on 503 + 429
        // (anthropic-sdk-python `_base_client.py::_should_retry`).
        // The Anthropic envelope renderer must propagate it the same
        // way the OpenAI envelope renderer does.
        let err = ProxyError::AllCandidatesUnavailable {
            retry_after_secs: Some(42),
        };
        let resp = err.into_anthropic_response();
        let retry_after = resp.headers().get("retry-after").expect("retry-after set");
        assert_eq!(retry_after.to_str().unwrap(), "42");
    }

    #[tokio::test]
    async fn anthropic_envelope_bridge_5xx_maps_to_api_error_with_the_upstream_message() {
        // 5xx collapse contract from ai-gateway#322/#327: the customer
        // sees a 502 wrapped in the Anthropic-shape envelope with
        // `error.type = "api_error"` (Anthropic's catch-all for
        // upstream/server failure), carrying the upstream's own message.
        let bridge_err = BridgeError::upstream_status(503, "engine internal panic");
        let err = ProxyError::Bridge(bridge_err);
        let resp = err.into_anthropic_response();
        let json = assert_anthropic_envelope(resp, StatusCode::BAD_GATEWAY, "api_error").await;
        assert_eq!(json["error"]["message"], "engine internal panic");
    }

    /// The four headers a gateway-produced 429 owes the caller, on the
    /// OpenAI-shape envelope. `x-ratelimit-reset` is delta-seconds and
    /// equals `retry-after` for a windowed dimension, so a client may
    /// back off on either without a clock shared with the gateway.
    #[tokio::test]
    async fn gateway_429_carries_the_standard_rate_limit_headers() {
        let err = ProxyError::RateLimit(aisix_ratelimit::RateLimitError::Requests {
            scope: aisix_core::RateLimitScope::Requests,
            detail: aisix_ratelimit::LimitDetail {
                dimension: "rpm",
                limit: 100,
                remaining: 0,
                reset_secs: 43,
            },
        });
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let h = resp.headers();
        assert_eq!(h.get("retry-after").unwrap(), "43");
        assert_eq!(h.get("x-ratelimit-limit").unwrap(), "100");
        assert_eq!(h.get("x-ratelimit-remaining").unwrap(), "0");
        assert_eq!(h.get("x-ratelimit-reset").unwrap(), "43");
        assert_eq!(h.get("x-ratelimit-scope").unwrap(), "rpm");
    }

    /// `/v1/messages` answers in the Anthropic envelope but owes the
    /// same headers — the renderers share one writer precisely so this
    /// cannot drift per endpoint.
    #[tokio::test]
    async fn anthropic_envelope_429_carries_the_same_rate_limit_headers() {
        let err = ProxyError::RateLimit(aisix_ratelimit::RateLimitError::Tokens {
            scope: aisix_core::RateLimitScope::Tokens,
            detail: aisix_ratelimit::LimitDetail {
                dimension: "tpm",
                limit: 50_000,
                remaining: 0,
                reset_secs: 12,
            },
        });
        let resp = err.into_anthropic_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let h = resp.headers();
        assert_eq!(h.get("retry-after").unwrap(), "12");
        assert_eq!(h.get("x-ratelimit-limit").unwrap(), "50000");
        assert_eq!(h.get("x-ratelimit-remaining").unwrap(), "0");
        assert_eq!(h.get("x-ratelimit-reset").unwrap(), "12");
        assert_eq!(h.get("x-ratelimit-scope").unwrap(), "tpm");
    }

    /// A policy-layer rejection is the same rejection with an attributed
    /// policy — it reports the policy's own limit, not a blank.
    #[tokio::test]
    async fn policy_429_reports_the_policy_layer_limit() {
        let err = ProxyError::PolicyRateLimit {
            source: aisix_ratelimit::RateLimitError::Requests {
                scope: aisix_core::RateLimitScope::Requests,
                detail: aisix_ratelimit::LimitDetail {
                    dimension: "rpd",
                    limit: 5_000,
                    remaining: 0,
                    reset_secs: 3_600,
                },
            },
            policy_id: "pol-1".into(),
            policy_name: "team-daily".into(),
            callers_bucket: true,
        };
        let resp = err.into_response();
        let h = resp.headers();
        assert_eq!(h.get("x-ratelimit-limit").unwrap(), "5000");
        assert_eq!(h.get("x-ratelimit-scope").unwrap(), "rpd");
        assert_eq!(h.get("x-ratelimit-reset").unwrap(), "3600");
    }

    /// A concurrency cap has no window, so it reports a fixed hint —
    /// but it DOES report all four headers. Before this contract a
    /// concurrency 429 carried no retry hint at all, leaving a client
    /// with nothing to back off on.
    #[tokio::test]
    async fn concurrency_429_carries_all_four_headers() {
        let err = ProxyError::RateLimit(aisix_ratelimit::RateLimitError::Concurrency {
            detail: aisix_ratelimit::LimitDetail {
                dimension: aisix_ratelimit::CONCURRENCY_DIMENSION,
                limit: 8,
                remaining: 0,
                reset_secs: aisix_ratelimit::CONCURRENCY_RETRY_AFTER_SECS,
            },
        });
        let resp = err.into_response();
        let h = resp.headers();
        assert_eq!(h.get("retry-after").unwrap(), "60");
        assert_eq!(h.get("x-ratelimit-limit").unwrap(), "8");
        assert_eq!(h.get("x-ratelimit-remaining").unwrap(), "0");
        assert_eq!(h.get("x-ratelimit-reset").unwrap(), "60");
        assert_eq!(h.get("x-ratelimit-scope").unwrap(), "concurrency");
    }

    /// An upstream 429 is the provider's rejection, not the gateway's.
    /// Its `Retry-After` is forwarded, but the gateway must NOT invent
    /// `x-ratelimit-*` values for a quota it does not know — their
    /// absence is how a caller tells the two 429s apart.
    #[tokio::test]
    async fn upstream_429_forwards_retry_after_but_no_gateway_ratelimit_headers() {
        let err = ProxyError::Bridge(BridgeError::upstream_status_with_retry_after(
            429,
            "rate limited by provider",
            Some(std::time::Duration::from_secs(30)),
        ));
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let h = resp.headers();
        assert_eq!(h.get("retry-after").unwrap(), "30");
        assert!(h.get("x-ratelimit-limit").is_none());
        assert!(h.get("x-ratelimit-remaining").is_none());
        assert!(h.get("x-ratelimit-reset").is_none());
        assert!(h.get("x-ratelimit-scope").is_none());
    }

    /// A budget rejection caps spend in dollars. It shares the 429
    /// status but has no request/token dimension, so the trio must stay
    /// absent rather than carry a number in the wrong unit.
    #[tokio::test]
    async fn budget_429_keeps_retry_after_without_rate_limit_headers() {
        let err = ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
            message: "budget exceeded".into(),
            scope: None,
            scope_ref: None,
            limit_usd: Some("1.00".into()),
            spent_usd: Some("2.00".into()),
            period: None,
            period_resets_at: None,
            retry_after_seconds: Some(600),
            ..Default::default()
        }));
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let h = resp.headers();
        assert_eq!(h.get("retry-after").unwrap(), "600");
        assert!(h.get("x-ratelimit-limit").is_none());
        assert!(h.get("x-ratelimit-scope").is_none());
    }

    #[tokio::test]
    async fn anthropic_envelope_bridge_429_maps_to_rate_limit_error() {
        // Upstream 429 passes through verbatim status; Anthropic-side
        // `error.type` maps to `rate_limit_error`. The upstream
        // message is preserved on 4xx (vs 5xx redaction).
        let bridge_err = BridgeError::upstream_status(429, "rate limited by anthropic");
        let err = ProxyError::Bridge(bridge_err);
        let resp = err.into_anthropic_response();
        let json =
            assert_anthropic_envelope(resp, StatusCode::TOO_MANY_REQUESTS, "rate_limit_error")
                .await;
        // 4xx message pass-through.
        let msg = json["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("rate limited"),
            "4xx upstream message must pass through to Anthropic envelope, got: {msg}",
        );
    }
}
