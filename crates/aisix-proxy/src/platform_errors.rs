//! Platform mode's error classes, and the upstream request errors it relays.
//!
//! In platform mode one gateway serves many tenants from upstream channels
//! and accounts the platform owns. Hiding which channel or account served a
//! tenant is best effort, and never at the cost of what a client does with
//! an error.
//!
//! So an upstream's answer to a request error reaches the caller as the
//! upstream sent it when the upstream speaks the caller's own protocol
//! ([`relayed_status`]): a `400`, `413` or `422`, with the upstream's own
//! status, `error.type`, `error.code`, `error.param` and message, buffered or
//! as the frame or failed Response that ends a stream. Clients act on those
//! words. Claude Code (2.1.319) compacts its conversation when a message says
//! "prompt is too long", and retries or degrades on the texts of a bad
//! thinking signature, `cache_control`, `effort`, a media limit or a
//! `tool_use_id`; the OpenAI SDK and Codex key off codes such as
//! `context_length_exceeded` and `previous_response_not_found`. What is
//! relayed has been through the shared scrubber
//! ([`aisix_gateway::upstream_error_scrub`]): the credentials the gateway
//! sent, AWS ARNs and account ids, Google Cloud project ids, internal hosts in
//! URLs, bearer tokens and prefixed keys are out of it.
//!
//! Every other error that came from an upstream, and every gateway refusal
//! whose text names the provider, provider key or upstream account a request
//! resolved to, reaches the caller as one of the closed set of classes below:
//! a fixed status, `error.type`, `error.code` and message, with only the
//! upstream's `param` (a field name of the caller's own request) carried over
//! from a native dispatch. That is any other status (an upstream `401`, `403`,
//! `404`, `408`, `409`, `424`, `429` or `5xx`), a transport failure or
//! timeout, and any error of a translated dispatch, whose words are another
//! protocol's and would not help the caller's client. Streamed error frames
//! and upstream error events relayed from a native stream are rewritten the
//! same way.
//!
//! The operator still sees what happened: the usage event's `error_message`,
//! the access log and the per-attempt records are all written from the
//! original error, never from the class or the relayed copy.
//!
//! The gateway's own refusals that name no upstream (budgets, rate limits,
//! guardrails, request validation, authentication) keep their envelopes in
//! platform mode, with one exception: a model the key may not use is answered
//! exactly as a model that does not exist ([`caller_error`]), so a key cannot
//! map the catalog by asking for names. With platform mode off nothing here
//! applies, and every error renders exactly as it did before, upstream text
//! relayed through the shared scrubber included.
//!
//! The mode is read from the request's attribution cell
//! ([`crate::attribution::platform_mode`]), which is installed around the
//! handler and around every poll of its response body, so the decision is
//! taken where the error is rendered rather than threaded through every
//! handler.

use aisix_gateway::{BridgeError, UpstreamErrorView, UpstreamWire};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::error::{ErrorBody, ErrorEnvelope, ProxyError};

/// What a caller is told about an error platform mode replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlatformErrorClass {
    ContextLengthExceeded,
    RateLimited,
    /// `param` is the field of the caller's request the upstream named.
    InvalidRequest {
        param: Option<String>,
    },
    ContentFiltered,
    Authentication,
    NotFound,
    /// The gateway's own refusal of a `previous_response_id` it cannot open,
    /// under the code of the upstream's own answer for one it does not have.
    PreviousResponseNotFound,
    UpstreamUnavailable,
    Internal,
}

impl PlatformErrorClass {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::ContextLengthExceeded
            | Self::InvalidRequest { .. }
            | Self::ContentFiltered
            | Self::PreviousResponseNotFound => StatusCode::BAD_REQUEST,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Authentication => StatusCode::UNAUTHORIZED,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::UpstreamUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The OpenAI envelope's `error.type`.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::ContextLengthExceeded
            | Self::InvalidRequest { .. }
            | Self::ContentFiltered
            | Self::NotFound
            | Self::PreviousResponseNotFound => "invalid_request_error",
            Self::RateLimited => "rate_limit_error",
            Self::Authentication => "authentication_error",
            Self::UpstreamUnavailable | Self::Internal => "server_error",
        }
    }

    /// `error.code`, the same in every envelope and stream frame. The
    /// rate-limit code is the one OpenAI documents
    /// (<https://platform.openai.com/docs/guides/error-codes>, and
    /// `rate_limit_exceeded` in openai-python 3.41.0
    /// `types/responses/response_error.py:49`), which the gateway's own
    /// `429`s carry too. `previous_response_not_found` is the code a client
    /// resends its history on (openai-python 3.41.0
    /// `lib/responses_websocket/README.md`), whether the upstream or the
    /// gateway could not find the response.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::ContextLengthExceeded => "context_length_exceeded",
            Self::RateLimited => "rate_limit_exceeded",
            Self::InvalidRequest { .. } => "invalid_request",
            Self::ContentFiltered => "content_filter",
            Self::Authentication => "invalid_api_key",
            Self::NotFound => "not_found",
            Self::PreviousResponseNotFound => "previous_response_not_found",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::Internal => "internal_error",
        }
    }

    /// The Anthropic envelope's `error.type`: the type Anthropic documents
    /// for the class's status (<https://docs.anthropic.com/en/api/errors>:
    /// `invalid_request_error` for 400, `authentication_error` for 401,
    /// `not_found_error` for 404, `rate_limit_error` for 429, `api_error`
    /// for 500, `overloaded_error` for a temporarily unavailable API), read
    /// through the table every other Anthropic envelope here uses.
    pub(crate) fn anthropic_kind(&self) -> &'static str {
        crate::error::anthropic_kind_from_status(self.status())
    }

    /// The field of the caller's request the class names, in every envelope
    /// and stream frame.
    pub(crate) fn param(&self) -> Option<String> {
        match self {
            Self::InvalidRequest { param } => param.clone(),
            Self::PreviousResponseNotFound => Some("previous_response_id".into()),
            _ => None,
        }
    }

    /// The OpenAI-shape envelope a caller receives for this class.
    pub(crate) fn envelope(&self) -> ErrorEnvelope {
        ErrorEnvelope {
            error: ErrorBody {
                message: platform_message(self).to_string(),
                kind: self.kind().to_string(),
                param: self.param(),
                code: Some(self.code().to_string()),
                budget: None,
                policy: None,
            },
        }
    }
}

/// The `error.code` a failed Response carries for `class`: the code of a
/// `response.failed` event's Response and of a buffered Response object with
/// `status: "failed"`. That slot takes the Responses API's own vocabulary
/// (openai-python 3.41.0 `types/responses/response_error.py:47-69`), and a
/// client reads it to decide what to do with the turn. Codex
/// (`codex-rs/codex-api/src/sse/responses_error.rs`, `parse_failed_response`)
/// reads `context_length_exceeded` as a context overflow, `invalid_prompt`
/// as a request that will fail the same way again, `rate_limit_exceeded` as a
/// rate limit, and any other code as a stream error worth retrying. So the
/// classes' own codes (`invalid_request`, `content_filter`, …) would have it
/// resend a turn that cannot succeed. The flat `error` event and the HTTP
/// envelopes keep [`PlatformErrorClass::code`].
pub(crate) fn responses_failed_code(class: &PlatformErrorClass) -> &'static str {
    match class {
        PlatformErrorClass::RateLimited => "rate_limit_exceeded",
        PlatformErrorClass::ContextLengthExceeded => "context_length_exceeded",
        PlatformErrorClass::InvalidRequest { .. }
        | PlatformErrorClass::ContentFiltered
        | PlatformErrorClass::PreviousResponseNotFound => "invalid_prompt",
        PlatformErrorClass::Authentication
        | PlatformErrorClass::NotFound
        | PlatformErrorClass::UpstreamUnavailable
        | PlatformErrorClass::Internal => "server_error",
    }
}

impl IntoResponse for PlatformErrorClass {
    fn into_response(self) -> Response {
        (self.status(), axum::Json(self.envelope())).into_response()
    }
}

/// The fixed, neutral message of each class.
///
/// A context overflow's message opens with Anthropic's own words for it,
/// because a client that compacts its conversation on an overflow reads it
/// off the message, never the code: Claude Code (2.1.319) treats an error as
/// a prompt too long when its lowercased message contains
/// "prompt is too long", and only then compacts and retries. Any other
/// wording leaves such a client failing every later turn.
pub(crate) fn platform_message(class: &PlatformErrorClass) -> &'static str {
    match class {
        PlatformErrorClass::ContextLengthExceeded => {
            "prompt is too long: the request exceeds the model's context length."
        }
        PlatformErrorClass::RateLimited => "Rate limit exceeded. Retry later.",
        PlatformErrorClass::InvalidRequest { .. } => "The request is invalid.",
        PlatformErrorClass::ContentFiltered => {
            "The request or response was blocked by content policy."
        }
        PlatformErrorClass::Authentication => "Invalid API key.",
        PlatformErrorClass::NotFound => "The requested resource was not found.",
        PlatformErrorClass::PreviousResponseNotFound => "Previous response not found.",
        PlatformErrorClass::UpstreamUnavailable => {
            "The model is temporarily unavailable. Retry later."
        }
        PlatformErrorClass::Internal => "Internal error.",
    }
}

/// The class `err` belongs to.
///
/// Platform mode replaces only the errors [`hides`] selects. A variant with
/// no arm of its own is classed by its status, which is how one added later
/// is classed until it gets one; `Authentication` is reached that way only,
/// by a 401 refusal not on [`keeps_own_envelope`]'s list, and none is today
/// (an upstream 401 is the platform's account, `UpstreamUnavailable`).
pub(crate) fn classify(err: &ProxyError) -> PlatformErrorClass {
    match err {
        ProxyError::Bridge(e) => classify_bridge(e),
        // The target's own configuration: the platform's to fix, not the
        // caller's, like an upstream that refuses the platform's account.
        ProxyError::UnusableTarget(_) => PlatformErrorClass::UpstreamUnavailable,
        // The model cannot serve this endpoint, whichever provider is behind
        // it: the caller's `model` is the field to change.
        ProxyError::TargetRefused(_) => PlatformErrorClass::InvalidRequest {
            param: Some("model".into()),
        },
        // The gateway's own refusal of a `previous_response_id` it cannot
        // open (another tenant's, a forged one, one sealed under another
        // secret): the status, type, `param` and code of the upstream's own
        // answer for one it does not have
        // (<https://developers.openai.com/api/docs/guides/websocket-mode>),
        // which is relayed as sent ([`relayed_status`]). A client that
        // resends its history on that code recovers either way, and the id
        // still never opens.
        ProxyError::ResponseNotFound => PlatformErrorClass::PreviousResponseNotFound,
        // The caller's own field, which the class names.
        ProxyError::StoredObjectReference(field) | ProxyError::ModelSelectorRefused(field) => {
            PlatformErrorClass::InvalidRequest {
                param: Some(field.clone()),
            }
        }
        other => match other.status().as_u16() {
            401 => PlatformErrorClass::Authentication,
            404 => PlatformErrorClass::NotFound,
            422 => PlatformErrorClass::ContentFiltered,
            429 => PlatformErrorClass::RateLimited,
            503 => PlatformErrorClass::UpstreamUnavailable,
            400..=499 => PlatformErrorClass::InvalidRequest { param: None },
            _ => PlatformErrorClass::Internal,
        },
    }
}

/// [`classify`] for an upstream-side failure.
pub(crate) fn classify_bridge(err: &BridgeError) -> PlatformErrorClass {
    match err {
        BridgeError::UpstreamStatus {
            status,
            message,
            parsed,
            wire,
            ..
        } => classify_upstream(Some(*status), parsed.as_deref(), message, *wire),
        BridgeError::UpstreamInBand {
            status,
            message,
            parsed,
            wire,
        } => classify_upstream(*status, parsed.as_deref(), message, *wire),
        // A request field this provider's bridge cannot translate: the text
        // names the provider, the refusal is still of the request.
        BridgeError::InvalidRequest(_) | BridgeError::RequestTooLarge { .. } => {
            PlatformErrorClass::InvalidRequest { param: None }
        }
        // The endpoint asked for something this model cannot do: the
        // request's to change, and not a fault a retry gets past.
        BridgeError::UnsupportedCapability(_) => PlatformErrorClass::InvalidRequest { param: None },
        BridgeError::Config(_) => PlatformErrorClass::Internal,
        // The platform's credential or channel configuration, a transport
        // fault, a timeout, or an answer that could not be read.
        BridgeError::InvalidUpstreamConfig(_)
        | BridgeError::InvalidUpstreamCredentials(_)
        | BridgeError::Timeout { .. }
        | BridgeError::UpstreamDecode(_)
        | BridgeError::UpstreamBodyUnread { .. }
        | BridgeError::TokenExchangeDecode(_)
        | BridgeError::Transport(_)
        | BridgeError::StreamAborted => PlatformErrorClass::UpstreamUnavailable,
    }
}

/// Error codes an upstream gives, under a request-error status, when the
/// platform's own account or channel is at fault rather than the request:
/// - OpenAI's `billing_hard_limit_reached`, a 400 an organization over its
///   own monthly budget gets (reported at
///   <https://community.openai.com/t/billing-hard-limit-reached-api-error/572481>;
///   not on OpenAI's error-codes page, whose own spend codes are 429s);
/// - Bedrock's `ServiceQuotaExceededException`, documented as a 400 that
///   "exceeds the service quota for your account"
///   (<https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModel.html#API_runtime_InvokeModel_Errors>).
const ACCOUNT_STATE_CODES: &[&str] = &[
    "billing_hard_limit_reached",
    "ServiceQuotaExceededException",
];

/// What an upstream writes, lowercased, when the platform's own account is at
/// fault under a request-error status and no code of its own says so. Each is
/// a 400 `invalid_request_error`:
/// - Anthropic's own spend limit, whose message "starts with `You have reached
///   your specified API usage limits`, or `You have reached your specified
///   workspace API usage limits` for a workspace limit, and states when access
///   resumes" (<https://platform.claude.com/docs/en/api/rate-limits>,
///   "Setting your own spend limit");
/// - Anthropic's "Your credit balance is too low to access the Anthropic API"
///   and "This organization has been disabled.", both of which Claude Code
///   (2.1.319) matches as account state rather than a request error;
/// - OpenAI's "Your organization must be verified to stream this model" (code
///   `unsupported_value`, `param` `stream`; reported at
///   <https://community.openai.com/t/requests-fail-even-after-verifying-organization/1231444>);
/// - the platform's channel configuration on Bedrock, which answers it with
///   the same `ValidationException` (400) as a malformed request
///   (<https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModel.html#API_runtime_InvokeModel_Errors>),
///   so only its words tell it apart: a model id that must be called through
///   an inference profile ("Invocation of model ID … with on-demand
///   throughput isn't supported. Retry your request with the ID or ARN of an
///   inference profile", reported at
///   <https://repost.aws/questions/QUEU82wbYVQk2oU4eNwyiong>), and a model
///   id the region or account does not offer ("The provided model identifier
///   is invalid.").
const ACCOUNT_STATE_MARKERS: &[&str] = &[
    "you have reached your specified api usage limits",
    "you have reached your specified workspace api usage limits",
    "credit balance is too low",
    "organization has been disabled",
    "organization must be verified",
    "with on-demand throughput isn",
    "the provided model identifier is invalid",
];

/// Codes and types upstreams use for a request their content policy refused:
/// Azure OpenAI's `content_filter` and its inner `ResponsibleAIPolicyViolation`,
/// OpenAI's `content_policy_violation`, and the policy codes of a failed
/// Response (openai-python 3.41.0 `types/responses/response_error.py:47`).
const CONTENT_FILTER_TOKENS: &[&str] = &[
    "content_filter",
    "content_policy_violation",
    "ResponsibleAIPolicyViolation",
    "image_content_policy_violation",
    "bio_policy",
    "misalignment_policy_violation",
];

/// The codes of a failed Response that blame the request itself, its prompt
/// or an image it carries (`types/responses/response_error.py:47`). An
/// in-band failure has no status to say so, and such a request fails the
/// same way however often it is retried.
const CALLER_FAULT_TOKENS: &[&str] = &[
    "invalid_prompt",
    "invalid_image",
    "invalid_image_format",
    "invalid_base64_image",
    "invalid_image_url",
    "image_too_large",
    "image_too_small",
    "image_parse_error",
    "invalid_image_mode",
    "image_file_too_large",
    "unsupported_image_media_type",
    "empty_image_file",
    "failed_to_download_image",
    "image_file_not_found",
];

/// What upstreams write for a prompt that does not fit the model's context
/// window, lowercased: OpenAI and the OpenAI-compatible servers ("maximum
/// context length"), Anthropic ("prompt is too long", and "input length and
/// `max_tokens` exceed context limit" when the prompt fits but leaves no
/// room for the requested output), Gemini ("exceeds the maximum number of
/// tokens"), Bedrock's publishers ("input is too long"), and the code itself
/// where a server writes it into the text. The mainstream gateways read a
/// context overflow off the same phrases, and Claude Code reads both of
/// Anthropic's phrasings as one ("prompt_too_long").
const CONTEXT_OVERFLOW_MARKERS: &[&str] = &[
    "context_length_exceeded",
    "maximum context length",
    "context window",
    "prompt is too long",
    "input is too long",
    "exceed context limit",
    "exceeds the maximum number of tokens",
];

/// Codes and types a status-less in-band error uses for a rate limit: the
/// OpenAI Responses stream's `rate_limit_exceeded` and Anthropic's
/// `rate_limit_error`.
const RATE_LIMIT_TOKENS: &[&str] = &["rate_limit_exceeded", "rate_limit_error"];

fn classify_upstream(
    status: Option<u16>,
    view: Option<&UpstreamErrorView>,
    message: &str,
    wire: UpstreamWire,
) -> PlatformErrorClass {
    let names = |tokens: &[&str]| {
        view.is_some_and(|v| {
            [v.code.as_deref(), v.kind.as_deref()]
                .into_iter()
                .flatten()
                .any(|t| tokens.contains(&t))
        })
    };
    // The platform's account (its spend limit, its credit, its organization's
    // standing), not the caller's request: the platform's to fix, as an
    // upstream 401 or 403 is, and never relayed, since it describes an account
    // every tenant shares and can carry the date its limit resets.
    if matches!(status, None | Some(400)) && {
        let text = message.to_ascii_lowercase();
        names(ACCOUNT_STATE_CODES) || ACCOUNT_STATE_MARKERS.iter().any(|m| text.contains(m))
    } {
        return PlatformErrorClass::UpstreamUnavailable;
    }
    if names(CONTENT_FILTER_TOKENS) {
        return PlatformErrorClass::ContentFiltered;
    }
    if matches!(status, None | Some(400)) {
        let text = message.to_ascii_lowercase();
        if names(&["context_length_exceeded"])
            || CONTEXT_OVERFLOW_MARKERS.iter().any(|m| text.contains(m))
        {
            return PlatformErrorClass::ContextLengthExceeded;
        }
    }
    // Only what has the shape of a field name of the caller's request:
    // anything else is the upstream's own text. And only from a native
    // dispatch: after a translation it names a field of the request the
    // gateway built, in the upstream's protocol, which tells the caller what
    // that protocol is.
    let param = || {
        view.and_then(|v| v.param.clone())
            .filter(|_| dispatched_natively(wire))
            .filter(|p| {
                p.len() <= 256
                    && p.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.[]".contains(&b))
            })
    };
    match status {
        Some(429) => PlatformErrorClass::RateLimited,
        // The platform's account was refused, or its billing was (Anthropic's
        // 402 `billing_error`, <https://platform.claude.com/docs/en/api/errors>):
        // not the caller's credential, and not something the caller may learn
        // about.
        Some(401..=403) => PlatformErrorClass::UpstreamUnavailable,
        Some(404) => PlatformErrorClass::NotFound,
        // A timeout and a conflict, which the OpenAI SDKs retry
        // (openai-python 3.41.0 `_base_client.py:944`), and a failed
        // dependency upstream (Bedrock's `ModelErrorException`): the
        // platform's to get past, not a request the caller must change.
        Some(408 | 409 | 424) => PlatformErrorClass::UpstreamUnavailable,
        Some(400..=499) => PlatformErrorClass::InvalidRequest { param: param() },
        Some(_) => PlatformErrorClass::UpstreamUnavailable,
        None if names(RATE_LIMIT_TOKENS) => PlatformErrorClass::RateLimited,
        // A native one is relayed ([`relayed_status`]), and a translated
        // one's `param` is another protocol's: no field is kept.
        None if names(CALLER_FAULT_TOKENS) => PlatformErrorClass::InvalidRequest { param: None },
        None => PlatformErrorClass::UpstreamUnavailable,
    }
}

/// The statuses of an upstream's answer to a request error, which platform
/// mode relays from an upstream of the caller's own protocol: a bad request,
/// a request too large and an unprocessable one.
const RELAYED_STATUSES: [u16; 3] = [400, 413, 422];

/// Whether the upstream that answered speaks the caller's own protocol, so
/// that its error is in words the caller's client reads. Messages,
/// count_tokens and Responses record it for each attempt
/// ([`crate::attribution::note_translated_dispatch`]). Every other route is
/// an OpenAI-shaped endpoint that reaches an upstream of another protocol only
/// through a bridge, so there the wire the error came in says it: an
/// OpenAI-wire upstream, Azure OpenAI's included, speaks it, and any other
/// was translated to.
fn dispatched_natively(wire: UpstreamWire) -> bool {
    match crate::attribution::translated_dispatch() {
        Some(translated) => !translated,
        None => matches!(wire, UpstreamWire::OpenAI | UpstreamWire::AzureOpenAI),
    }
}

/// The status `err` reaches the caller with when platform mode relays it as
/// the upstream sent it: an upstream's answer to a request error
/// ([`RELAYED_STATUSES`]) from an upstream of the caller's own protocol, in
/// that protocol's own error envelope. An error an upstream reports inside a
/// stream or a `2xx` body may carry no status of its own; it is a request
/// error when its own code says so (a context overflow, a refused prompt or
/// image, a content filter), which is when its class is a `400`.
///
/// `None` when platform mode is off, and for every error that keeps a class:
/// one that reports the platform's own account under a request-error status
/// ([`ACCOUNT_STATE_MARKERS`]), and a body the upstream's envelope parser did
/// not recognise (an intermediary's HTML page, another API's shape), whose
/// words are in no protocol a client reads.
pub(crate) fn relayed_status(err: &BridgeError) -> Option<u16> {
    if !active() {
        return None;
    }
    let (status, wire, parsed) = match err {
        BridgeError::UpstreamStatus {
            status,
            wire,
            parsed,
            ..
        } => (Some(*status), *wire, parsed),
        BridgeError::UpstreamInBand {
            status,
            wire,
            parsed,
            ..
        } => (*status, *wire, parsed),
        _ => return None,
    };
    if parsed.is_none() {
        return None;
    }
    let class = classify_bridge(err);
    if class == PlatformErrorClass::UpstreamUnavailable {
        return None;
    }
    let status = status.unwrap_or_else(|| class.status().as_u16());
    (RELAYED_STATUSES.contains(&status) && dispatched_natively(wire)).then_some(status)
}

/// Whether platform mode replaces `err` for the caller: everything but the
/// gateway's own refusals that name no upstream, which keep their own
/// envelope ([`keeps_own_envelope`]). A variant that is not on that list is
/// replaced, so one added later is replaced until someone decides
/// otherwise: a refusal whose text names the provider, provider key or
/// account cannot reach a tenant by being forgotten here.
fn hides(err: &ProxyError) -> bool {
    match err {
        ProxyError::Bridge(e) => hides_bridge(e),
        other => !keeps_own_envelope(other),
    }
}

/// The gateway's own refusals that keep their envelope in platform mode:
/// authentication, a model or route the key may not reach, request
/// validation, guardrails, budgets and rate limits. Each is decided on the
/// caller's own key, request or limits and names no upstream, and its
/// envelope is what a caller acts on (a budget's figures, a rate limit's
/// `Retry-After`, a guardrail's name). Also the MCP token refusals, which
/// are about the caller's identity at its own identity provider.
fn keeps_own_envelope(err: &ProxyError) -> bool {
    matches!(
        err,
        ProxyError::MissingAuth(_)
            | ProxyError::MissingRouteAuthHeader(_)
            | ProxyError::InvalidApiKey
            | ProxyError::ApiKeyExpired
            | ProxyError::ApiKeyDisabled
            | ProxyError::JwtInvalid
            | ProxyError::JwtExpired
            | ProxyError::JwtClaimsRejected
            | ProxyError::JwtInsufficientScope { .. }
            | ProxyError::JwtIdentityUnmapped
            | ProxyError::JwksUnavailable
            | ProxyError::McpTokenExchange(_)
            | ProxyError::McpUserOAuth { .. }
            | ProxyError::ModelNotFound(_)
            | ProxyError::ModelForbidden(_)
            | ProxyError::ModelIpRestricted(_)
            | ProxyError::RouteIpRestricted(_)
            | ProxyError::RouteForbidden(_)
            | ProxyError::JobListRefused
            | ProxyError::InvalidRequest(_)
            | ProxyError::RequestTooLarge { .. }
            | ProxyError::WebSocketUpgradeRequired { .. }
            | ProxyError::ContentFiltered { .. }
            | ProxyError::BudgetExceeded(_)
            | ProxyError::BudgetUnverified
            | ProxyError::RateLimit(_)
            | ProxyError::ModelRateLimit(_)
            | ProxyError::PolicyRateLimit { .. }
    )
}

/// The upstream-side errors left alone are the gateway's own request size
/// limit, which names nothing, and the request errors platform mode relays
/// ([`relayed_status`]).
fn hides_bridge(err: &BridgeError) -> bool {
    !matches!(err, BridgeError::RequestTooLarge { .. }) && relayed_status(err).is_none()
}

/// Whether the request being served is served in platform mode.
pub(crate) fn active() -> bool {
    crate::attribution::platform_mode()
}

/// The class `err` reaches the caller as: `Some` when the request is served
/// in platform mode and `err` is one platform mode replaces, `None` when
/// `err` renders as it always has.
pub(crate) fn caller_class(err: &ProxyError) -> Option<PlatformErrorClass> {
    (active() && hides(err)).then(|| classify(err))
}

/// The error the caller is answered with in place of `err`, when platform
/// mode answers `err` as another of the gateway's own errors: a model the key
/// may not use is answered in status, headers and body as a model that does
/// not exist. Otherwise a key could tell the models it is refused from the
/// names nobody configured. `None` when `err` is answered as itself.
///
/// Only the caller's answer changes. The usage event, the access log and the
/// metrics are written from `err`, so the operator still sees a `403`
/// `permission_denied` refusal.
pub(crate) fn caller_error(err: &ProxyError) -> Option<ProxyError> {
    match err {
        ProxyError::ModelForbidden(model) if active() => {
            Some(ProxyError::ModelNotFound(model.clone()))
        }
        _ => None,
    }
}

/// [`caller_class`] for the failure that ends a stream.
pub(crate) fn stream_class(err: &BridgeError) -> Option<PlatformErrorClass> {
    (active() && hides_bridge(err)).then(|| classify_bridge(err))
}

/// The `data:` payload of the OpenAI-shape SSE error frame ending a chat or
/// legacy completions stream on `err`, in platform mode: the same body the
/// buffered response carries, the upstream's own for a request error it
/// relays and the class's envelope otherwise.
pub(crate) fn openai_stream_payload(err: &BridgeError) -> Option<String> {
    if let Some(relayed) = crate::error::relayed_upstream_envelope(err) {
        return Some(serde_json::to_string(&relayed).unwrap_or_default());
    }
    stream_class(err).map(|class| openai_payload(&class))
}

fn openai_payload(class: &PlatformErrorClass) -> String {
    serde_json::to_string(&class.envelope()).unwrap_or_default()
}

/// The Anthropic `event: error` frame ending a `/v1/messages` stream on
/// `err`, in platform mode.
pub(crate) fn anthropic_stream_frame(err: &BridgeError) -> Option<String> {
    stream_class(err).map(|class| anthropic_frame(&class))
}

fn anthropic_frame(class: &PlatformErrorClass) -> String {
    format!(
        "event: error\ndata: {}\n\n",
        crate::error::anthropic_platform_body(class)
    )
}

/// An upstream's Anthropic `error` event, relayed from a native stream, as
/// platform mode sends it: the class of the error it reports, or, for a
/// request error it relays ([`relayed_status`]), the event with its text
/// through the shared scrubber. `event` is the frame's parsed payload, `None`
/// when it did not parse. `None` too when the frame goes out as the upstream
/// wrote it, less the credentials the caller takes out of its bytes.
pub(crate) fn relayed_anthropic_error(event: Option<&Value>) -> Option<String> {
    if !active() {
        return None;
    }
    let err = event.and_then(aisix_provider_anthropic::wire::body_in_band_error);
    if let (Some(event), Some(err)) = (event, &err) {
        if relayed_status(err).is_some() {
            return scrubbed_error(event, "/error")
                .map(|event| format!("event: error\ndata: {event}\n\n"));
        }
    }
    let class = err.map_or(PlatformErrorClass::UpstreamUnavailable, |e| {
        classify_bridge(&e)
    });
    Some(anthropic_frame(&class))
}

/// `event` with every string of the error object at `pointer` through the
/// shared scrubber, which a relayed body's bytes only get for the
/// credentials the request sent. `None` when that changes nothing, and the
/// event goes out as the upstream wrote it.
fn scrubbed_error(event: &Value, pointer: &str) -> Option<Value> {
    let mut scrubbed = event.clone();
    scrub_strings(scrubbed.pointer_mut(pointer)?).then_some(scrubbed)
}

/// Every string in `value` through the shared scrubber, then cut at the
/// length a buffered relay's text is cut at
/// ([`aisix_gateway::MAX_UPSTREAM_ERROR_MESSAGE_BYTES`]); whether any changed.
fn scrub_strings(value: &mut Value) -> bool {
    match value {
        Value::String(text) => {
            let scrubbed = aisix_gateway::truncate_lossy(
                &aisix_gateway::scrub_upstream_error_text(text),
                aisix_gateway::MAX_UPSTREAM_ERROR_MESSAGE_BYTES,
            );
            let changed = scrubbed != *text;
            *text = scrubbed;
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |any, v| scrub_strings(v) | any),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |any, v| scrub_strings(v) | any),
        _ => false,
    }
}

/// `frame` carrying `payload`, under the `event:` name it was sent with.
fn reframed(frame: &[u8], payload: &Value) -> Vec<u8> {
    let name = String::from_utf8_lossy(frame)
        .lines()
        .find_map(|line| line.strip_prefix("event:").map(|n| n.trim().to_owned()));
    match name {
        Some(name) => format!("event: {name}\ndata: {payload}\n\n"),
        None => format!("data: {payload}\n\n"),
    }
    .into_bytes()
}

/// A Responses-API stream frame that reports an upstream's failure, relayed
/// from a native stream, as platform mode sends it. A request error it relays
/// ([`relayed_status`]) keeps the frame, its error's text through the shared
/// scrubber, and is `None` when that changes nothing. Otherwise a
/// `response.failed` keeps its Response object with the class in place of its
/// `error`, and any other failure frame becomes the flat `error` event,
/// keeping the frame's `sequence_number`.
pub(crate) fn relayed_responses_failure(frame: &[u8]) -> Option<Vec<u8>> {
    if !active() {
        return None;
    }
    let (event, _) = crate::redact::parse_sse_json_event(frame);
    let err = event
        .as_ref()
        .and_then(crate::responses::responses_in_band_error);
    if let (Some(event), Some(err)) = (&event, &err) {
        if relayed_status(err).is_some() {
            let at = if event.get("type").and_then(Value::as_str) == Some("response.failed") {
                "/response/error"
            } else if event.get("error").is_some_and(Value::is_object) {
                "/error"
            } else {
                ""
            };
            return scrubbed_error(event, at).map(|event| reframed(frame, &event));
        }
    }
    let class = err.map_or(PlatformErrorClass::UpstreamUnavailable, |e| {
        classify_bridge(&e)
    });
    let error = serde_json::json!({
        "code": responses_failed_code(&class),
        "message": platform_message(&class),
    });
    let rewritten = match event {
        Some(mut failed)
            if failed.get("type").and_then(Value::as_str) == Some("response.failed")
                && failed.get("response").is_some_and(Value::is_object) =>
        {
            failed["response"]["error"] = error;
            ("response.failed", failed)
        }
        event => (
            "error",
            flat_responses_error(
                &class,
                event.as_ref().and_then(|e| e.get("sequence_number")),
            ),
        ),
    };
    Some(format!("event: {}\ndata: {}\n\n", rewritten.0, rewritten.1).into_bytes())
}

/// The flat Responses `error` event carrying `class`, numbered `seq` when
/// the frame it replaces was.
fn flat_responses_error(class: &PlatformErrorClass, seq: Option<&Value>) -> Value {
    let mut flat = serde_json::json!({
        "type": "error",
        "code": class.code(),
        "message": platform_message(class),
        "param": class.param(),
    });
    if let Some(seq) = seq {
        flat["sequence_number"] = seq.clone();
    }
    flat
}

/// The Responses `error` event a native stream ends on when `err` cut it
/// short, in platform mode: its class, as the event a relayed failure
/// becomes ([`relayed_responses_failure`]).
pub(crate) fn responses_stream_failure(err: &BridgeError) -> Option<Vec<u8>> {
    stream_class(err).map(|class| {
        format!(
            "event: error\ndata: {}\n\n",
            flat_responses_error(&class, None)
        )
        .into_bytes()
    })
}

/// A buffered Responses-API answer that is a failed Response object
/// (`status: "failed"` with its own `error`), relayed as sent: in platform
/// mode its `error` becomes the class of the error it reports, or for a
/// request error it relays ([`relayed_status`]) stays the upstream's, through
/// the shared scrubber, as a streamed `response.failed` does
/// ([`relayed_responses_failure`]).
pub(crate) fn relayed_failed_response(body: &mut Value) {
    if !active() || body.get("status").and_then(Value::as_str) != Some("failed") {
        return;
    }
    let err = aisix_gateway::capture_in_band_error_value(body, UpstreamWire::OpenAI);
    if err.as_ref().is_some_and(|e| relayed_status(e).is_some()) {
        if let Some(error) = body.get_mut("error") {
            scrub_strings(error);
        }
        return;
    }
    let class = err.map_or(PlatformErrorClass::UpstreamUnavailable, |e| {
        classify_bridge(&e)
    });
    body["error"] = serde_json::json!({
        "code": responses_failed_code(&class),
        "message": platform_message(&class),
    });
}

/// A whole Responses-API SSE body an output guardrail held back, which ends
/// on an upstream's failure, as platform mode releases it: every failure
/// frame rewritten by [`relayed_responses_failure`], every other frame as
/// it was.
pub(crate) fn relayed_responses_buffer(buf: &[u8]) -> Option<Vec<u8>> {
    if !active() {
        return None;
    }
    let mut out = Vec::with_capacity(buf.len());
    let mut remaining = buf;
    while !remaining.is_empty() {
        let end = crate::redact::first_sse_event_end(remaining).unwrap_or(remaining.len());
        let frame = &remaining[..end];
        let (event, _) = crate::redact::parse_sse_json_event(frame);
        let failure = event
            .as_ref()
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|t| matches!(t, "error" | "response.failed"));
        match failure.then(|| relayed_responses_failure(frame)).flatten() {
            Some(rewritten) => out.extend_from_slice(&rewritten),
            None => out.extend_from_slice(frame),
        }
        remaining = &remaining[end..];
    }
    Some(out)
}

/// An OpenAI-shape stream frame relayed from a native legacy completions
/// stream, when it reports an upstream's in-band error, as platform mode
/// sends it: the gateway's own error frame carrying the class, or for a
/// request error it relays ([`relayed_status`]) the frame with its error's
/// text through the shared scrubber, `None` when that changes nothing.
pub(crate) fn relayed_openai_error(frame: &[u8]) -> Option<Vec<u8>> {
    const KEY: &[u8] = b"\"error\"";
    if !active() || !frame.windows(KEY.len()).any(|w| w == KEY) {
        return None;
    }
    let (event, _) = crate::redact::parse_sse_json_event(frame);
    let event = event?;
    let err = aisix_gateway::capture_in_band_error_value(&event, UpstreamWire::OpenAI)?;
    if relayed_status(&err).is_some() {
        return scrubbed_error(&event, "/error").map(|event| reframed(frame, &event));
    }
    Some(
        format!(
            "event: error\ndata: {}\n\n",
            openai_payload(&classify_bridge(&err))
        )
        .into_bytes(),
    )
}

/// The class of an upstream's non-2xx answer a route relays as sent, when the
/// request is served in platform mode: the caller receives the class in
/// place of the upstream's body.
pub(crate) fn relayed_error(status: u16, body: &[u8]) -> Option<PlatformErrorClass> {
    active().then(|| {
        classify_bridge(&aisix_gateway::upstream_status_of_body(
            status,
            body,
            None,
            UpstreamWire::OpenAI,
            aisix_provider_openai::parse_openai_error_envelope,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::tests::render_platform;
    use crate::error::ProxyError;
    use aisix_gateway::{BridgeError, UpstreamErrorView, UpstreamWire};

    fn upstream_status(status: u16, message: &str) -> ProxyError {
        ProxyError::Bridge(BridgeError::upstream_status(status, message))
    }

    fn upstream_status_with_param(status: u16, message: &str, param: &str) -> ProxyError {
        ProxyError::Bridge(BridgeError::UpstreamStatus {
            status,
            message: message.into(),
            parsed: Some(Box::new(UpstreamErrorView {
                kind: Some("invalid_request_error".into()),
                message: Some(message.into()),
                code: None,
                param: Some(param.into()),
            })),
            wire: UpstreamWire::OpenAI,
            retry_after: None,
            diagnostic: None,
        })
    }

    fn budget_exceeded_for_team() -> ProxyError {
        ProxyError::BudgetExceeded(Box::new(crate::budget::BudgetReason {
            message: "team budget 'frontend' exceeded ($1.00/month).".into(),
            scope: Some("team".into()),
            ..Default::default()
        }))
    }

    #[test]
    fn an_upstream_401_is_reported_as_unavailable_not_as_the_callers_auth() {
        assert_eq!(
            classify(&upstream_status(401, "Invalid Anthropic API key")),
            PlatformErrorClass::UpstreamUnavailable
        );
    }

    #[test]
    fn an_upstream_context_error_keeps_its_meaning() {
        let e = upstream_status(400, "prompt is too long: 250000 tokens > 200000 maximum");
        assert_eq!(classify(&e), PlatformErrorClass::ContextLengthExceeded);
    }

    /// Anthropic's second phrasing of an overflow: the prompt fits, but not
    /// with the output the request asked room for.
    #[test]
    fn an_overflow_that_leaves_no_room_for_max_tokens_is_a_context_overflow() {
        let e = upstream_status(
            400,
            "input length and `max_tokens` exceed context limit: 190000 + 32000 > 200000, \
             decrease input length or `max_tokens` and try again",
        );
        assert_eq!(classify(&e), PlatformErrorClass::ContextLengthExceeded);
    }

    /// A client that compacts on an overflow reads it off the message. Claude
    /// Code (2.1.319) lowercases the message and looks for "prompt is too
    /// long"; whatever envelope the class goes out in, it must find it.
    #[tokio::test]
    async fn a_context_overflow_reads_as_one_to_a_client_that_compacts() {
        let overflow =
            || upstream_status(400, "prompt is too long: 250000 tokens > 200000 maximum");
        let openai = in_platform_mode(|| overflow().into_response());
        let anthropic = in_platform_mode(|| overflow().into_anthropic_response());
        for resp in [openai, anthropic] {
            let message = body_json(resp).await["error"]["message"]
                .as_str()
                .unwrap()
                .to_lowercase();
            assert!(message.contains("prompt is too long"), "{message}");
        }
    }

    #[test]
    fn an_upstream_param_error_keeps_the_field_name() {
        let e = upstream_status_with_param(400, "Invalid value for 'temperature'", "temperature");
        assert_eq!(
            classify(&e),
            PlatformErrorClass::InvalidRequest {
                param: Some("temperature".into())
            }
        );
    }

    /// What is relayed as `param` has the shape of a field name; anything
    /// else an upstream put there is its own text and is not.
    #[test]
    fn only_a_field_shaped_upstream_param_is_relayed() {
        assert_eq!(
            classify(&upstream_status_with_param(
                400,
                "bad",
                "messages[0].content[1].source.file_id"
            )),
            PlatformErrorClass::InvalidRequest {
                param: Some("messages[0].content[1].source.file_id".into())
            }
        );
        for unshaped in [
            "model arn:aws:bedrock:us-east-1:123:x".to_string(),
            "a b".to_string(),
            "us-east-1".to_string(),
            "x".repeat(257),
        ] {
            assert_eq!(
                classify(&upstream_status_with_param(400, "bad", &unshaped)),
                PlatformErrorClass::InvalidRequest { param: None },
                "{unshaped}"
            );
        }
    }

    /// The upstream's `param` names a field of the request the upstream
    /// received. After a translation that is not the caller's request, so a
    /// class keeps it only when the attempt was dispatched natively (a native
    /// `400` itself is relayed whole, [`relayed_status`]). A route that
    /// records nothing is told by the wire the error came in.
    #[test]
    fn a_param_is_kept_only_from_a_native_dispatch() {
        let class = |wire: UpstreamWire, translated: Option<bool>| {
            let err = ProxyError::Bridge(BridgeError::UpstreamStatus {
                status: 400,
                message: "bad".into(),
                parsed: Some(Box::new(UpstreamErrorView {
                    param: Some("messages[1].content".into()),
                    ..Default::default()
                })),
                wire,
                retry_after: None,
                diagnostic: None,
            });
            in_platform_mode(|| {
                if let Some(translated) = translated {
                    crate::attribution::note_translated_dispatch(translated);
                }
                classify(&err)
            })
        };
        let kept = PlatformErrorClass::InvalidRequest {
            param: Some("messages[1].content".into()),
        };
        let dropped = PlatformErrorClass::InvalidRequest { param: None };
        assert_eq!(class(UpstreamWire::OpenAI, Some(false)), kept);
        assert_eq!(class(UpstreamWire::Anthropic, Some(false)), kept);
        assert_eq!(class(UpstreamWire::OpenAI, Some(true)), dropped);
        assert_eq!(class(UpstreamWire::OpenAI, None), kept);
        assert_eq!(class(UpstreamWire::AzureOpenAI, None), kept);
        for wire in [
            UpstreamWire::Anthropic,
            UpstreamWire::Bedrock,
            UpstreamWire::Vertex,
            UpstreamWire::Unknown,
        ] {
            assert_eq!(class(wire, None), dropped, "{wire:?}");
        }
    }

    /// What platform mode relays as the upstream sent it: a request error
    /// (`400`, `413`, `422`, or a status-less error whose own code blames the
    /// request) from an upstream of the caller's own protocol. Exact, so a
    /// status that moves is caught.
    #[test]
    fn only_a_native_request_error_is_relayed() {
        let relayed = |status: u16, wire: UpstreamWire, translated: Option<bool>| {
            let err = BridgeError::UpstreamStatus {
                status,
                message: "upstream text".into(),
                parsed: Some(Box::new(UpstreamErrorView {
                    message: Some("upstream text".into()),
                    ..Default::default()
                })),
                wire,
                retry_after: None,
                diagnostic: None,
            };
            in_platform_mode(|| {
                if let Some(translated) = translated {
                    crate::attribution::note_translated_dispatch(translated);
                }
                relayed_status(&err)
            })
        };
        for status in [400, 413, 422] {
            // Chat, completions and embeddings record nothing: an OpenAI-wire
            // upstream speaks their protocol, any other was translated to.
            assert_eq!(relayed(status, UpstreamWire::OpenAI, None), Some(status));
            assert_eq!(
                relayed(status, UpstreamWire::AzureOpenAI, None),
                Some(status)
            );
            for wire in [
                UpstreamWire::Anthropic,
                UpstreamWire::Bedrock,
                UpstreamWire::Vertex,
                UpstreamWire::Unknown,
            ] {
                assert_eq!(relayed(status, wire, None), None, "{status} {wire:?}");
            }
            // Messages, count_tokens and Responses record it, whatever the
            // wire: Claude on a cloud platform speaks Messages natively.
            for wire in [UpstreamWire::Anthropic, UpstreamWire::Bedrock] {
                assert_eq!(relayed(status, wire, Some(false)), Some(status), "{wire:?}");
            }
            assert_eq!(relayed(status, UpstreamWire::OpenAI, Some(true)), None);
        }
        for status in [
            401, 402, 403, 404, 405, 408, 409, 424, 429, 500, 502, 503, 529,
        ] {
            assert_eq!(
                relayed(status, UpstreamWire::OpenAI, Some(false)),
                None,
                "{status}"
            );
        }
        // A body the upstream's envelope parser did not recognise, and one
        // that reports the platform's own account, keep their class.
        let unparsed = BridgeError::UpstreamStatus {
            status: 400,
            message: "<html>bad request</html>".into(),
            parsed: None,
            wire: UpstreamWire::OpenAI,
            retry_after: None,
            diagnostic: None,
        };
        assert_eq!(in_platform_mode(|| relayed_status(&unparsed)), None);
        for (wire, message, code) in [
            (UpstreamWire::Anthropic, "Your credit balance is too low to access the Anthropic API.", None),
            (
                UpstreamWire::Anthropic,
                "You have reached your specified API usage limits. You will regain access on 2026-12-01 at 00:00 UTC.",
                None,
            ),
            (UpstreamWire::Anthropic, "This organization has been disabled.", None),
            (UpstreamWire::OpenAI, "Your organization must be verified to stream this model.", None),
            (UpstreamWire::OpenAI, "Billing hard limit has been reached", Some("billing_hard_limit_reached")),
        ] {
            let err = BridgeError::UpstreamStatus {
                status: 400,
                message: message.into(),
                parsed: Some(Box::new(UpstreamErrorView {
                    message: Some(message.into()),
                    code: code.map(str::to_owned),
                    ..Default::default()
                })),
                wire,
                retry_after: None,
                diagnostic: None,
            };
            let (relayed, class) = in_platform_mode(|| {
                crate::attribution::note_translated_dispatch(false);
                (relayed_status(&err), classify_bridge(&err))
            });
            assert_eq!(relayed, None, "{message}");
            assert_eq!(class, PlatformErrorClass::UpstreamUnavailable, "{message}");
        }

        // Off platform mode nothing is relayed: everything renders as it
        // always has.
        assert_eq!(
            relayed_status(&BridgeError::UpstreamStatus {
                status: 400,
                message: "x".into(),
                parsed: None,
                wire: UpstreamWire::OpenAI,
                retry_after: None,
                diagnostic: None,
            }),
            None
        );

        // An error reported inside a stream or a 2xx body: its own status,
        // or else its own code.
        let in_band = |status: Option<u16>, code: &str| {
            let err = BridgeError::UpstreamInBand {
                status,
                message: "upstream text".into(),
                parsed: Some(Box::new(UpstreamErrorView {
                    code: Some(code.into()),
                    ..Default::default()
                })),
                wire: UpstreamWire::OpenAI,
            };
            in_platform_mode(|| relayed_status(&err))
        };
        for code in [
            "context_length_exceeded",
            "invalid_prompt",
            "invalid_image_url",
            "content_policy_violation",
        ] {
            assert_eq!(in_band(None, code), Some(400), "{code}");
        }
        for code in [
            "server_error",
            "rate_limit_exceeded",
            "vector_store_timeout",
        ] {
            assert_eq!(in_band(None, code), None, "{code}");
        }
        assert_eq!(in_band(Some(413), "server_error"), Some(413));
        assert_eq!(in_band(Some(529), "invalid_prompt"), None);

        // A failure no upstream answered with.
        for err in [
            BridgeError::Transport("connection refused".into()),
            BridgeError::Timeout {
                elapsed_ms: 10,
                cause: String::new(),
            },
            BridgeError::StreamAborted,
            BridgeError::InvalidRequest("bad".into()),
        ] {
            assert_eq!(in_platform_mode(|| relayed_status(&err)), None, "{err}");
        }
    }

    #[test]
    fn a_rendered_upstream_error_carries_no_upstream_text() {
        let body = render_platform(&upstream_status(
            500,
            "Bedrock ThrottlingException in us-east-1",
        ));
        let s = body.to_string();
        assert!(!s.contains("Bedrock") && !s.contains("us-east-1"));
        assert_eq!(body["error"]["code"], "upstream_unavailable");
    }

    #[test]
    fn gateway_budget_errors_keep_their_own_message_in_platform_mode() {
        let body = render_platform(&budget_exceeded_for_team());
        assert_eq!(body["error"]["code"], "budget_exceeded");
    }

    use crate::error::tests::in_platform_mode;
    use axum::response::IntoResponse;

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn every_class_has_the_status_code_and_message_of_the_table() {
        let table = [
            (
                PlatformErrorClass::ContextLengthExceeded,
                400,
                "invalid_request_error",
                "context_length_exceeded",
                "prompt is too long: the request exceeds the model's context length.",
            ),
            (
                PlatformErrorClass::RateLimited,
                429,
                "rate_limit_error",
                "rate_limit_exceeded",
                "Rate limit exceeded. Retry later.",
            ),
            (
                PlatformErrorClass::InvalidRequest { param: None },
                400,
                "invalid_request_error",
                "invalid_request",
                "The request is invalid.",
            ),
            (
                PlatformErrorClass::ContentFiltered,
                400,
                "invalid_request_error",
                "content_filter",
                "The request or response was blocked by content policy.",
            ),
            (
                PlatformErrorClass::Authentication,
                401,
                "authentication_error",
                "invalid_api_key",
                "Invalid API key.",
            ),
            (
                PlatformErrorClass::NotFound,
                404,
                "invalid_request_error",
                "not_found",
                "The requested resource was not found.",
            ),
            (
                PlatformErrorClass::PreviousResponseNotFound,
                400,
                "invalid_request_error",
                "previous_response_not_found",
                "Previous response not found.",
            ),
            (
                PlatformErrorClass::UpstreamUnavailable,
                503,
                "server_error",
                "upstream_unavailable",
                "The model is temporarily unavailable. Retry later.",
            ),
            (
                PlatformErrorClass::Internal,
                500,
                "server_error",
                "internal_error",
                "Internal error.",
            ),
        ];
        for (class, status, kind, code, message) in table {
            assert_eq!(class.status().as_u16(), status, "{class:?}");
            assert_eq!(class.kind(), kind, "{class:?}");
            assert_eq!(class.code(), code, "{class:?}");
            assert_eq!(platform_message(&class), message, "{class:?}");
        }
    }

    #[test]
    fn the_upstream_status_decides_the_class() {
        assert_eq!(
            classify(&upstream_status(429, "slow down")),
            PlatformErrorClass::RateLimited
        );
        assert_eq!(
            classify(&upstream_status(403, "account suspended")),
            PlatformErrorClass::UpstreamUnavailable
        );
        // Anthropic's `billing_error`: the platform's billing, not the
        // caller's request.
        assert_eq!(
            classify(&upstream_status(402, "billing issue")),
            PlatformErrorClass::UpstreamUnavailable
        );
        assert_eq!(
            classify(&upstream_status(404, "no such deployment")),
            PlatformErrorClass::NotFound
        );
        assert_eq!(
            classify(&upstream_status(422, "bad shape")),
            PlatformErrorClass::InvalidRequest { param: None }
        );
        assert_eq!(
            classify(&upstream_status(502, "bad gateway")),
            PlatformErrorClass::UpstreamUnavailable
        );
        // A context phrase on anything but a 400 is not a context overflow.
        assert_eq!(
            classify(&upstream_status(500, "maximum context length")),
            PlatformErrorClass::UpstreamUnavailable
        );
        for transport in [
            BridgeError::Transport("connection refused by 10.0.0.1".into()),
            BridgeError::Timeout {
                elapsed_ms: 10,
                cause: String::new(),
            },
            BridgeError::InvalidUpstreamCredentials("empty bedrock secret".into()),
            BridgeError::InvalidUpstreamConfig("vertex project missing".into()),
        ] {
            assert_eq!(
                classify_bridge(&transport),
                PlatformErrorClass::UpstreamUnavailable,
                "{transport}"
            );
        }
    }

    #[test]
    fn an_upstream_code_decides_the_class_where_it_names_one() {
        let with_code = |status: u16, code: &str| {
            ProxyError::Bridge(BridgeError::UpstreamStatus {
                status,
                message: "upstream text".into(),
                parsed: Some(Box::new(UpstreamErrorView {
                    code: Some(code.into()),
                    ..Default::default()
                })),
                wire: UpstreamWire::OpenAI,
                retry_after: None,
                diagnostic: None,
            })
        };
        assert_eq!(
            classify(&with_code(400, "context_length_exceeded")),
            PlatformErrorClass::ContextLengthExceeded
        );
        assert_eq!(
            classify(&with_code(400, "content_filter")),
            PlatformErrorClass::ContentFiltered
        );
        assert_eq!(
            classify(&with_code(400, "content_policy_violation")),
            PlatformErrorClass::ContentFiltered
        );
        // An in-band error with no status: its code, or else an outage.
        let in_band = |code: Option<&str>| BridgeError::UpstreamInBand {
            status: None,
            message: "Bedrock ThrottlingException".into(),
            parsed: Some(Box::new(UpstreamErrorView {
                code: code.map(str::to_owned),
                ..Default::default()
            })),
            wire: UpstreamWire::OpenAI,
        };
        assert_eq!(
            classify_bridge(&in_band(Some("rate_limit_exceeded"))),
            PlatformErrorClass::RateLimited
        );
        assert_eq!(
            classify_bridge(&in_band(Some("server_error"))),
            PlatformErrorClass::UpstreamUnavailable
        );
        assert_eq!(
            classify_bridge(&in_band(None)),
            PlatformErrorClass::UpstreamUnavailable
        );
    }

    /// A status the OpenAI SDKs retry, and an upstream's failed dependency,
    /// are the platform's to retry, not a caller's invalid request; a
    /// missing capability is the request's, not an internal fault. Exact
    /// codes, so a class that moves is caught.
    #[test]
    fn a_retryable_status_and_a_caller_fault_get_their_own_class() {
        for status in [408, 409, 424] {
            let class = classify(&upstream_status(status, "upstream text"));
            assert_eq!(class, PlatformErrorClass::UpstreamUnavailable, "{status}");
            assert_eq!(class.status().as_u16(), 503, "{status}");
            assert_eq!(class.code(), "upstream_unavailable", "{status}");
        }
        let unsupported =
            BridgeError::UnsupportedCapability(aisix_gateway::BridgeCapability::Embeddings);
        assert_eq!(
            classify_bridge(&unsupported),
            PlatformErrorClass::InvalidRequest { param: None }
        );
        assert_eq!(classify_bridge(&unsupported).status().as_u16(), 400);

        // In-band failures with no status, by their code
        // (openai-python `types/responses/response_error.py`).
        let in_band = |code: &str, param: Option<&str>| BridgeError::UpstreamInBand {
            status: None,
            message: "upstream text".into(),
            parsed: Some(Box::new(UpstreamErrorView {
                code: Some(code.into()),
                param: param.map(str::to_owned),
                ..Default::default()
            })),
            wire: UpstreamWire::OpenAI,
        };
        for code in [
            "image_content_policy_violation",
            "bio_policy",
            "misalignment_policy_violation",
        ] {
            let class = classify_bridge(&in_band(code, None));
            assert_eq!(class, PlatformErrorClass::ContentFiltered, "{code}");
            assert_eq!(class.code(), "content_filter", "{code}");
        }
        for code in [
            "invalid_prompt",
            "invalid_image",
            "invalid_image_format",
            "invalid_base64_image",
            "invalid_image_url",
            "image_too_large",
            "image_too_small",
            "image_parse_error",
            "invalid_image_mode",
            "image_file_too_large",
            "unsupported_image_media_type",
            "empty_image_file",
            "failed_to_download_image",
            "image_file_not_found",
        ] {
            let class = classify_bridge(&in_band(code, None));
            assert_eq!(
                class,
                PlatformErrorClass::InvalidRequest { param: None },
                "{code}"
            );
            assert_eq!(class.code(), "invalid_request", "{code}");
        }
        // Its field is not kept: a native one is relayed whole
        // ([`relayed_status`]), and a translated one's is another protocol's.
        assert_eq!(
            classify_bridge(&in_band(
                "invalid_image_url",
                Some("input[0].content[1].image_url")
            )),
            PlatformErrorClass::InvalidRequest { param: None }
        );
        for code in [
            "server_error",
            "vector_store_timeout",
            "data_residency_mismatch",
        ] {
            assert_eq!(
                classify_bridge(&in_band(code, None)),
                PlatformErrorClass::UpstreamUnavailable,
                "{code}"
            );
        }
    }

    #[test]
    fn with_platform_mode_off_an_upstream_error_renders_as_it_always_has() {
        let err = upstream_status(401, "Invalid Anthropic API key");
        assert_eq!(caller_class(&err), None);
        let body = serde_json::to_value(err.envelope()).unwrap();
        assert_eq!(body["error"]["message"], "Invalid Anthropic API key");
        assert_eq!(body["error"]["type"], "upstream_error");
        assert_eq!(err.into_response().status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn only_upstream_errors_and_refusals_naming_the_target_are_replaced() {
        in_platform_mode(|| {
            assert!(caller_class(&upstream_status(500, "x")).is_some());
            assert!(caller_class(&ProxyError::UnusableTarget(
                "pk 'acct-7' has no secret".into()
            ))
            .is_some());
            assert!(
                caller_class(&ProxyError::TargetRefused("provider anthropic".into())).is_some()
            );
            for own in [
                ProxyError::InvalidRequest("invalid request body: messages".into()),
                ProxyError::InvalidApiKey,
                ProxyError::ModelNotFound("m".into()),
                budget_exceeded_for_team(),
                crate::error::guardrail_block_error("request", Some("g"), None),
                ProxyError::Bridge(BridgeError::RequestTooLarge {
                    limit_bytes: 1,
                    inbound_bytes: 1,
                    downloaded_bytes: 1,
                }),
            ] {
                assert_eq!(caller_class(&own), None, "{own:?}");
            }
        });
    }

    /// Whether a variant keeps its own envelope in platform mode. Written as
    /// a match with no catch-all, so a new variant does not compile here
    /// until someone decides; the census below then checks `hides` agrees.
    fn keeps_its_envelope(err: &ProxyError) -> bool {
        match err {
            ProxyError::MissingAuth(_)
            | ProxyError::MissingRouteAuthHeader(_)
            | ProxyError::InvalidApiKey
            | ProxyError::ApiKeyExpired
            | ProxyError::ApiKeyDisabled
            | ProxyError::JwtInvalid
            | ProxyError::JwtExpired
            | ProxyError::JwtClaimsRejected
            | ProxyError::JwtInsufficientScope { .. }
            | ProxyError::JwtIdentityUnmapped
            | ProxyError::JwksUnavailable
            | ProxyError::McpTokenExchange(_)
            | ProxyError::McpUserOAuth { .. }
            | ProxyError::ModelNotFound(_)
            | ProxyError::ModelForbidden(_)
            | ProxyError::ModelIpRestricted(_)
            | ProxyError::RouteIpRestricted(_)
            | ProxyError::RouteForbidden(_)
            | ProxyError::JobListRefused
            | ProxyError::InvalidRequest(_)
            | ProxyError::RequestTooLarge { .. }
            | ProxyError::WebSocketUpgradeRequired { .. }
            | ProxyError::ContentFiltered { .. }
            | ProxyError::BudgetExceeded(_)
            | ProxyError::BudgetUnverified
            | ProxyError::RateLimit(_)
            | ProxyError::ModelRateLimit(_)
            | ProxyError::PolicyRateLimit { .. } => true,
            ProxyError::VideoNotFound(_)
            | ProxyError::JobResourceNotFound(_)
            | ProxyError::ResponseNotFound
            | ProxyError::StoredObjectReference(_)
            | ProxyError::ModelSelectorRefused(_)
            | ProxyError::UnusableTarget(_)
            | ProxyError::TargetRefused(_)
            | ProxyError::ProviderUnavailable
            | ProxyError::SemanticDecisionFailed { .. }
            | ProxyError::AllCandidatesUnavailable { .. } => false,
            ProxyError::Bridge(e) => matches!(e, BridgeError::RequestTooLarge { .. }),
        }
    }

    /// One of every variant.
    fn every_variant() -> Vec<ProxyError> {
        let limited = || aisix_ratelimit::RateLimitError::Concurrency {
            detail: aisix_ratelimit::LimitDetail {
                dimension: "concurrency",
                limit: 1,
                remaining: 0,
                reset_secs: 60,
            },
        };
        vec![
            ProxyError::MissingAuth("authorization"),
            ProxyError::MissingRouteAuthHeader("x-key".into()),
            ProxyError::InvalidApiKey,
            ProxyError::ApiKeyExpired,
            ProxyError::ApiKeyDisabled,
            ProxyError::JwtInvalid,
            ProxyError::JwtExpired,
            ProxyError::JwtClaimsRejected,
            ProxyError::JwtInsufficientScope {
                required_scopes: vec!["s".into()],
            },
            ProxyError::JwtIdentityUnmapped,
            ProxyError::JwksUnavailable,
            ProxyError::McpTokenExchange(aisix_mcp::TokenExchangeFailure::Unavailable),
            ProxyError::McpUserOAuth {
                failure: aisix_mcp::UserOAuthFailure::AuthorizationRequired,
                server: "s".into(),
            },
            ProxyError::ModelNotFound("m".into()),
            ProxyError::VideoNotFound("v".into()),
            ProxyError::JobResourceNotFound("file-1".into()),
            ProxyError::ResponseNotFound,
            ProxyError::StoredObjectReference("conversation".into()),
            ProxyError::ModelSelectorRefused("models".into()),
            ProxyError::JobListRefused,
            ProxyError::ModelForbidden("m".into()),
            ProxyError::ModelIpRestricted("m".into()),
            ProxyError::RouteIpRestricted("r".into()),
            ProxyError::RouteForbidden("r".into()),
            ProxyError::InvalidRequest("bad".into()),
            ProxyError::UnusableTarget("pk 'acct-7' has no secret".into()),
            ProxyError::TargetRefused("provider bedrock".into()),
            ProxyError::WebSocketUpgradeRequired {
                status: StatusCode::UPGRADE_REQUIRED,
                detail: "d".into(),
            },
            ProxyError::ProviderUnavailable,
            ProxyError::SemanticDecisionFailed {
                router: "r".into(),
                reason: crate::semantic::DecisionFailure::ClassifierTimeout,
            },
            ProxyError::AllCandidatesUnavailable {
                retry_after_secs: Some(30),
            },
            ProxyError::ContentFiltered {
                message: "blocked".into(),
                unavailable: None,
            },
            budget_exceeded_for_team(),
            ProxyError::BudgetUnverified,
            ProxyError::RequestTooLarge { limit_bytes: 1 },
            ProxyError::RateLimit(limited()),
            ProxyError::ModelRateLimit(limited()),
            ProxyError::PolicyRateLimit {
                source: limited(),
                policy_id: "p".into(),
                policy_name: "p".into(),
                callers_bucket: true,
            },
            upstream_status(500, "x"),
            ProxyError::Bridge(BridgeError::RequestTooLarge {
                limit_bytes: 1,
                inbound_bytes: 1,
                downloaded_bytes: 1,
            }),
        ]
    }

    /// Platform mode classes every variant but the gateway's own refusals
    /// that name no upstream: a variant left off that list is classed, so
    /// one added later fails closed.
    #[test]
    fn every_variant_is_classed_unless_it_keeps_its_own_envelope() {
        in_platform_mode(|| {
            for err in every_variant() {
                assert_eq!(
                    caller_class(&err).is_none(),
                    keeps_its_envelope(&err),
                    "{err:?}"
                );
            }
        });
        for err in every_variant() {
            assert_eq!(caller_class(&err), None, "off platform mode: {err:?}");
        }
    }

    #[tokio::test]
    async fn the_caller_gets_the_class_status_and_an_upstream_429_keeps_its_retry_after() {
        let unauthorized =
            in_platform_mode(|| upstream_status(401, "Invalid Anthropic API key").into_response());
        assert_eq!(unauthorized.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(unauthorized).await;
        assert_eq!(body["error"]["code"], "upstream_unavailable");
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(
            body["error"]["message"],
            "The model is temporarily unavailable. Retry later."
        );

        let limited = in_platform_mode(|| {
            ProxyError::Bridge(BridgeError::upstream_status_with_retry_after(
                429,
                "Anthropic org limit in us-west-2",
                Some(std::time::Duration::from_secs(7)),
            ))
            .into_response()
        });
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(limited.headers()["retry-after"], "7");
        let text = body_json(limited).await.to_string();
        assert!(
            text.contains("\"rate_limit_exceeded\"") && !text.contains("Anthropic"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn the_anthropic_envelope_carries_anthropics_type_and_the_class_code() {
        let resp = in_platform_mode(|| {
            upstream_status(400, "prompt is too long: 250000 tokens > 200000 maximum")
                .into_anthropic_response()
        });
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "context_length_exceeded");
        assert_eq!(
            body["error"]["message"],
            "prompt is too long: the request exceeds the model's context length."
        );

        let resp = in_platform_mode(|| upstream_status(401, "x").into_anthropic_response());
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(resp).await["error"]["type"], "overloaded_error");
    }

    /// A response id the gateway cannot open is refused with the status,
    /// type, field and code of the upstream's own answer for a
    /// `previous_response_id` it does not have, which is relayed as sent. A
    /// client that resends its history on that code
    /// (openai-python 3.41.0 `lib/responses_websocket/README.md`) recovers
    /// either way; only the message differs, the gateway's naming nothing.
    #[tokio::test]
    async fn an_unknown_previous_response_carries_the_upstreams_code_either_way() {
        let unknown = in_platform_mode(|| ProxyError::ResponseNotFound.into_response());
        let upstream = in_platform_mode(|| {
            let message = "Previous response with id 'resp_1' not found.";
            ProxyError::Bridge(BridgeError::UpstreamStatus {
                status: 400,
                message: message.into(),
                parsed: Some(Box::new(UpstreamErrorView {
                    kind: Some("invalid_request_error".into()),
                    message: Some(message.into()),
                    code: Some("previous_response_not_found".into()),
                    param: Some("previous_response_id".into()),
                })),
                wire: UpstreamWire::OpenAI,
                retry_after: None,
                diagnostic: None,
            })
            .into_response()
        });
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
        assert_eq!(upstream.status(), StatusCode::BAD_REQUEST);
        let (unknown, upstream) = (body_json(unknown).await, body_json(upstream).await);
        for body in [&unknown, &upstream] {
            assert_eq!(body["error"]["param"], "previous_response_id", "{body}");
            assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
            assert_eq!(
                body["error"]["code"], "previous_response_not_found",
                "{body}"
            );
        }
        assert_eq!(unknown["error"]["message"], "Previous response not found.");
        assert_eq!(
            upstream["error"]["message"],
            "Previous response with id 'resp_1' not found."
        );
    }

    #[test]
    fn a_refusal_naming_the_target_is_an_invalid_request_either_way() {
        let refusal = ProxyError::TargetRefused("model \"m\" uses provider \"bedrock\"".into());
        let off = serde_json::to_value(refusal.envelope()).unwrap();
        let plain = serde_json::to_value(
            ProxyError::InvalidRequest("model \"m\" uses provider \"bedrock\"".into()).envelope(),
        )
        .unwrap();
        assert_eq!(
            off, plain,
            "off platform mode it is an InvalidRequest byte for byte"
        );
        assert_eq!(refusal.status(), StatusCode::BAD_REQUEST);
        let on = render_platform(&refusal);
        assert_eq!(on["error"]["code"], "invalid_request");
        // The field to change is the model the caller named.
        assert_eq!(on["error"]["param"], "model");
        assert!(!on.to_string().contains("bedrock"), "{on}");
    }

    /// An Anthropic error names the field at the head of its message
    /// (`max_tokens: 64001 > 64000, …`), and Claude Code (2.1.319) matches
    /// those very texts. A native Messages caller gets the message whole; a
    /// translated one is in another protocol's words and gets the class,
    /// naming no field.
    #[tokio::test]
    async fn an_anthropic_request_error_reaches_a_native_caller_whole() {
        let message = "max_tokens: 64001 > 64000, which is the maximum allowed number of \
                       output tokens for this model";
        let err = || {
            ProxyError::Bridge(BridgeError::UpstreamStatus {
                status: 400,
                message: message.into(),
                parsed: Some(Box::new(UpstreamErrorView {
                    kind: Some("invalid_request_error".into()),
                    message: Some(message.into()),
                    code: None,
                    param: None,
                })),
                wire: UpstreamWire::Anthropic,
                retry_after: None,
                diagnostic: None,
            })
        };
        let answer = |translated: bool| {
            in_platform_mode(|| {
                crate::attribution::note_translated_dispatch(translated);
                err().into_anthropic_response()
            })
        };
        let native = answer(false);
        assert_eq!(native.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(native).await,
            serde_json::json!({"type": "error", "error": {
                "type": "invalid_request_error", "message": message}})
        );
        let translated = body_json(answer(true)).await;
        assert_eq!(translated["error"]["code"], "invalid_request");
        assert_eq!(translated["error"]["message"], "The request is invalid.");
        assert!(translated["error"].get("param").is_none(), "{translated}");
    }

    #[test]
    fn stream_frames_carry_the_class_only_in_platform_mode() {
        let err = BridgeError::UpstreamInBand {
            status: None,
            message: "Bedrock ThrottlingException in us-east-1".into(),
            parsed: None,
            wire: UpstreamWire::OpenAI,
        };
        assert_eq!(openai_stream_payload(&err), None);
        assert_eq!(anthropic_stream_frame(&err), None);
        let payload = in_platform_mode(|| openai_stream_payload(&err)).unwrap();
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["error"]["code"], "upstream_unavailable");
        let frame = in_platform_mode(|| anthropic_stream_frame(&err)).unwrap();
        assert!(frame.starts_with("event: error\ndata: "), "{frame}");
        assert!(
            frame.contains("\"overloaded_error\"") && !frame.contains("Bedrock"),
            "{frame}"
        );
    }

    #[test]
    fn a_relayed_anthropic_error_event_becomes_its_class() {
        let event = serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "Anthropic org limit in us-east-1"}
        });
        assert_eq!(relayed_anthropic_error(Some(&event)), None);
        let frame = in_platform_mode(|| relayed_anthropic_error(Some(&event))).unwrap();
        assert!(
            frame.contains("\"rate_limit_exceeded\"") && !frame.contains("Anthropic"),
            "{frame}"
        );
        // A frame that did not parse still leaves nothing of the upstream's.
        let frame = in_platform_mode(|| relayed_anthropic_error(None)).unwrap();
        assert!(frame.contains("\"upstream_unavailable\""), "{frame}");
    }

    #[test]
    fn a_relayed_responses_failure_keeps_its_place_in_the_stream() {
        let flat = b"event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Bedrock in us-east-1\",\"param\":null,\"sequence_number\":4}\n\n";
        assert_eq!(relayed_responses_failure(flat), None);
        let out = in_platform_mode(|| relayed_responses_failure(flat)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("event: error\ndata: "), "{text}");
        let (v, _) = crate::redact::parse_sse_json_event(text.as_bytes());
        let v = v.unwrap();
        assert_eq!(v["code"], "upstream_unavailable");
        assert_eq!(v["sequence_number"], 4);
        assert!(!text.contains("Bedrock"), "{text}");

        let failed = b"event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":9,\"response\":{\"id\":\"resp_1\",\"status\":\"failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Anthropic said no\"}}}\n\n";
        let out = in_platform_mode(|| relayed_responses_failure(failed)).unwrap();
        let (v, _) = crate::redact::parse_sse_json_event(&out);
        let v = v.unwrap();
        assert_eq!(v["type"], "response.failed");
        assert_eq!(v["response"]["status"], "failed");
        assert_eq!(v["response"]["error"]["code"], "rate_limit_exceeded");
        assert!(!String::from_utf8_lossy(&out).contains("Anthropic"));

        let mut buffer = b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n".to_vec();
        buffer.extend_from_slice(failed);
        let out = in_platform_mode(|| relayed_responses_buffer(&buffer)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("event: response.created\n"), "{text}");
        assert!(
            text.contains("\"rate_limit_exceeded\"") && !text.contains("Anthropic"),
            "{text}"
        );
    }

    #[test]
    fn a_relayed_failed_response_object_carries_its_class() {
        let failed = serde_json::json!({
            "id": "resp_1",
            "object": "response",
            "status": "failed",
            "output": [],
            "error": {"code": "server_error", "message": "Bedrock ThrottlingException in us-east-1"},
        });
        let mut off = failed.clone();
        relayed_failed_response(&mut off);
        assert_eq!(off, failed, "platform mode off leaves the object as sent");

        let mut on = failed.clone();
        in_platform_mode(|| relayed_failed_response(&mut on));
        assert_eq!(on["status"], "failed");
        assert_eq!(on["id"], "resp_1");
        assert_eq!(on["error"]["code"], "server_error");
        assert_eq!(
            on["error"]["message"],
            "The model is temporarily unavailable. Retry later."
        );
        let text = on.to_string();
        assert!(
            !text.contains("Bedrock") && !text.contains("us-east-1"),
            "{text}"
        );

        // A completed answer is the answer, and is left alone.
        let mut completed =
            serde_json::json!({"object": "response", "status": "completed", "error": null});
        let before = completed.clone();
        in_platform_mode(|| relayed_failed_response(&mut completed));
        assert_eq!(completed, before);
    }

    /// A failed Response carries a code of the Responses API's own, in the
    /// buffered object and in a relayed `response.failed` alike, while the
    /// flat `error` event keeps the class's code.
    #[test]
    fn a_failed_response_carries_a_code_the_responses_api_defines() {
        let table = [
            (PlatformErrorClass::RateLimited, "rate_limit_exceeded"),
            (
                PlatformErrorClass::ContextLengthExceeded,
                "context_length_exceeded",
            ),
            (
                PlatformErrorClass::InvalidRequest { param: None },
                "invalid_prompt",
            ),
            (PlatformErrorClass::ContentFiltered, "invalid_prompt"),
            (PlatformErrorClass::Authentication, "server_error"),
            (PlatformErrorClass::NotFound, "server_error"),
            (
                PlatformErrorClass::PreviousResponseNotFound,
                "invalid_prompt",
            ),
            (PlatformErrorClass::UpstreamUnavailable, "server_error"),
            (PlatformErrorClass::Internal, "server_error"),
        ];
        for (class, code) in table {
            assert_eq!(responses_failed_code(&class), code, "{class:?}");
        }

        // An upstream's own `invalid_prompt`, after a translation: a turn
        // that fails the same way however often it is sent. (A native one is
        // relayed whole, [`relayed_status`].)
        let translated = |f: &mut dyn FnMut()| {
            in_platform_mode(|| {
                crate::attribution::note_translated_dispatch(true);
                f()
            })
        };
        let mut buffered = serde_json::json!({
            "object": "response",
            "status": "failed",
            "error": {"code": "invalid_prompt", "message": "flagged by the Bedrock stack"},
        });
        translated(&mut || relayed_failed_response(&mut buffered));
        assert_eq!(buffered["error"]["code"], "invalid_prompt");
        assert_eq!(buffered["error"]["message"], "The request is invalid.");

        let failed = b"event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":3,\"response\":{\"id\":\"resp_1\",\"status\":\"failed\",\"error\":{\"code\":\"invalid_prompt\",\"message\":\"flagged by the Bedrock stack\"}}}\n\n";
        let mut out = None;
        translated(&mut || out = relayed_responses_failure(failed));
        let (v, _) = crate::redact::parse_sse_json_event(&out.unwrap());
        assert_eq!(v.unwrap()["response"]["error"]["code"], "invalid_prompt");

        let flat = b"event: error\ndata: {\"type\":\"error\",\"code\":\"invalid_prompt\",\"message\":\"flagged\",\"param\":null,\"sequence_number\":3}\n\n";
        let mut out = None;
        translated(&mut || out = relayed_responses_failure(flat));
        let (v, _) = crate::redact::parse_sse_json_event(&out.unwrap());
        assert_eq!(v.unwrap()["code"], "invalid_request");
    }

    #[test]
    fn a_relayed_completions_error_frame_becomes_its_class() {
        let frame = b"data: {\"error\":{\"message\":\"Bedrock quota in us-east-1\",\"type\":\"server_error\"}}\n\n";
        assert_eq!(relayed_openai_error(frame), None);
        let out = in_platform_mode(|| relayed_openai_error(frame)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\"upstream_unavailable\"") && !text.contains("Bedrock"),
            "{text}"
        );
        // An answer frame is left alone.
        let answer = b"data: {\"choices\":[{\"text\":\"hi\"}]}\n\n";
        assert_eq!(in_platform_mode(|| relayed_openai_error(answer)), None);
    }

    #[test]
    fn a_relayed_refusal_body_becomes_its_class() {
        assert_eq!(relayed_error(401, b"{}"), None);
        let body = br#"{"error":{"message":"Invalid value for 'purpose'","type":"invalid_request_error","param":"purpose"}}"#;
        assert_eq!(
            in_platform_mode(|| relayed_error(400, body)),
            Some(PlatformErrorClass::InvalidRequest {
                param: Some("purpose".into())
            })
        );
        assert_eq!(
            in_platform_mode(|| relayed_error(401, b"Incorrect API key for Azure resource")),
            Some(PlatformErrorClass::UpstreamUnavailable)
        );
    }

    /// A request error a native stream reports goes out as its upstream wrote
    /// it, less what the shared scrubber takes out of its text: a frame with
    /// nothing to take out is left to the caller, which relays its bytes.
    #[test]
    fn a_relayed_request_error_frame_is_the_upstreams_less_what_the_scrubber_takes_out() {
        let native = |frame: &[u8]| {
            in_platform_mode(|| {
                crate::attribution::note_translated_dispatch(false);
                relayed_responses_failure(frame)
            })
        };
        let clean = b"event: error\ndata: {\"type\":\"error\",\"code\":\"context_length_exceeded\",\"message\":\"Your input exceeds the context window.\",\"param\":\"input\",\"sequence_number\":4}\n\n";
        assert_eq!(native(clean), None);

        let arn = b"event: error\ndata: {\"type\":\"error\",\"code\":\"context_length_exceeded\",\"message\":\"Too long for arn:aws:bedrock:us-east-1:123456789012:inference-profile/x\",\"param\":\"input\",\"sequence_number\":4}\n\n";
        let out = native(arn).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("event: error\ndata: "), "{text}");
        let (v, _) = crate::redact::parse_sse_json_event(text.as_bytes());
        let v = v.unwrap();
        assert_eq!(v["code"], "context_length_exceeded");
        assert_eq!(v["param"], "input");
        assert_eq!(v["sequence_number"], 4);
        assert_eq!(v["message"], "Too long for <redacted>");

        // A stream an output guardrail held back releases it the same way.
        let mut buffer = b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n".to_vec();
        buffer.extend_from_slice(arn);
        let out = in_platform_mode(|| {
            crate::attribution::note_translated_dispatch(false);
            relayed_responses_buffer(&buffer)
        })
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("event: response.created\n"), "{text}");
        assert!(
            text.contains("\"context_length_exceeded\"")
                && text.contains("Too long for <redacted>")
                && !text.contains("123456789012"),
            "{text}"
        );
    }

    /// An Azure OpenAI request error is relayed with Azure's own code, not
    /// the code the translation table maps it to, and a request error's type.
    #[tokio::test]
    async fn an_azure_request_error_keeps_azures_own_code() {
        // As the Azure parser reads them: the code its translation keys on in
        // `kind`, Azure's own code in `code`.
        for (kind, code) in [
            ("context_length_exceeded", "context_length_exceeded"),
            ("ResponsibleAIPolicyViolation", "content_filter"),
        ] {
            let err = ProxyError::Bridge(BridgeError::UpstreamStatus {
                status: 400,
                message: "upstream text".into(),
                parsed: Some(Box::new(UpstreamErrorView {
                    kind: Some(kind.into()),
                    message: Some("upstream text".into()),
                    code: Some(code.into()),
                    param: Some("messages".into()),
                })),
                wire: UpstreamWire::AzureOpenAI,
                retry_after: None,
                diagnostic: None,
            });
            let resp = in_platform_mode(|| err.into_response());
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let body = body_json(resp).await;
            assert_eq!(
                body["error"],
                serde_json::json!({"message": "upstream text", "type": "invalid_request_error",
                                   "param": "messages", "code": code}),
                "{kind}"
            );
        }
    }

    /// A relayed frame's text is cut where a buffered relay's is, so a long
    /// request error reads the same buffered and mid-stream.
    #[test]
    fn a_relayed_frame_is_cut_where_a_buffered_relay_is() {
        let long = format!(
            "{} at arn:aws:bedrock:us-east-1:123456789012:x",
            "x".repeat(3000)
        );
        let event = serde_json::json!({"type": "error", "sequence_number": 4,
            "code": "context_length_exceeded", "message": long, "param": "input"});
        let frame = format!("event: error\ndata: {event}\n\n");
        let out = in_platform_mode(|| {
            crate::attribution::note_translated_dispatch(false);
            relayed_responses_failure(frame.as_bytes())
        })
        .unwrap();
        let (v, _) = crate::redact::parse_sse_json_event(&out);
        let buffered = match aisix_gateway::capture_in_band_error_value(
            &serde_json::json!({"error": {"message": long, "code": "context_length_exceeded"}}),
            UpstreamWire::OpenAI,
        ) {
            Some(BridgeError::UpstreamInBand { message, .. }) => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(v.unwrap()["message"], buffered.as_str());
        assert!(buffered.len() < long.len(), "{}", buffered.len());
    }

    /// Bedrock answers the platform's channel configuration with the
    /// `ValidationException` (400) a malformed request gets, and its account
    /// quota with `ServiceQuotaExceededException` (400). Neither is the
    /// tenant's request: both are the model being unavailable, never
    /// relayed; a request error under the same exception still is.
    #[test]
    fn a_bedrock_channel_configuration_400_is_the_model_being_unavailable() {
        let bedrock = |kind: &str, message: &str| BridgeError::UpstreamStatus {
            status: 400,
            message: message.into(),
            parsed: Some(Box::new(UpstreamErrorView {
                kind: Some(kind.into()),
                message: Some(message.into()),
                code: None,
                param: None,
            })),
            wire: UpstreamWire::Bedrock,
            retry_after: None,
            diagnostic: None,
        };
        let native = |err: &BridgeError| {
            in_platform_mode(|| {
                crate::attribution::note_translated_dispatch(false);
                (relayed_status(err), classify_bridge(err))
            })
        };
        for err in [
            bedrock(
                "ValidationException",
                "Invocation of model ID anthropic.claude-sonnet-4-20250514-v1:0 with on-demand \
                 throughput isn't supported. Retry your request with the ID or ARN of an \
                 inference profile that contains this model.",
            ),
            bedrock(
                "ValidationException",
                "Invocation of model ID anthropic.claude-sonnet-4-20250514-v1:0 with on-demand \
                 throughput isn’t supported.",
            ),
            bedrock(
                "ValidationException",
                "The provided model identifier is invalid.",
            ),
            bedrock(
                "ServiceQuotaExceededException",
                "Your request exceeds the service quota for your account.",
            ),
        ] {
            assert_eq!(
                native(&err),
                (None, PlatformErrorClass::UpstreamUnavailable),
                "{err}"
            );
        }
        let request = bedrock(
            "ValidationException",
            "messages.0.content.0.image.source.base64: image exceeds 5 MB maximum",
        );
        assert_eq!(native(&request).0, Some(400));
    }
}
