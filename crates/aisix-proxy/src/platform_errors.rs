//! Platform mode's error classes.
//!
//! In platform mode one gateway serves many tenants from upstream channels
//! and accounts the platform owns, and a tenant must never learn which
//! channel or account served it. So every error that came from an upstream,
//! and every gateway refusal whose text names the provider, provider key or
//! upstream account a request resolved to, reaches the caller as one of the
//! closed set of classes below: a fixed status, `error.type`, `error.code`
//! and message, with only the upstream's `param` (a field name of the
//! caller's own request) carried over. Streamed error frames and upstream
//! error events relayed from a native stream are rewritten the same way.
//!
//! The operator still sees what happened: the usage event's `error_message`,
//! the access log and the per-attempt records are all written from the
//! original error, never from the class.
//!
//! The gateway's own refusals that name no upstream (budgets, rate limits,
//! guardrails, request validation, authentication) keep their envelopes in
//! platform mode. With platform mode off nothing here applies, and every
//! error renders exactly as it did before, upstream text relayed through the
//! shared scrubber included.
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
    UpstreamUnavailable,
    Internal,
}

impl PlatformErrorClass {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::ContextLengthExceeded | Self::InvalidRequest { .. } | Self::ContentFiltered => {
                StatusCode::BAD_REQUEST
            }
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
            | Self::NotFound => "invalid_request_error",
            Self::RateLimited => "rate_limit_error",
            Self::Authentication => "authentication_error",
            Self::UpstreamUnavailable | Self::Internal => "server_error",
        }
    }

    /// `error.code`, the same in every envelope and stream frame.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::ContextLengthExceeded => "context_length_exceeded",
            Self::RateLimited => "rate_limited",
            Self::InvalidRequest { .. } => "invalid_request",
            Self::ContentFiltered => "content_filter",
            Self::Authentication => "invalid_api_key",
            Self::NotFound => "not_found",
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

    /// The OpenAI-shape envelope a caller receives for this class.
    pub(crate) fn envelope(&self) -> ErrorEnvelope {
        let param = match self {
            Self::InvalidRequest { param } => param.clone(),
            _ => None,
        };
        ErrorEnvelope {
            error: ErrorBody {
                message: platform_message(self).to_string(),
                kind: self.kind().to_string(),
                param,
                code: Some(self.code().to_string()),
                budget: None,
                policy: None,
            },
        }
    }
}

impl IntoResponse for PlatformErrorClass {
    fn into_response(self) -> Response {
        (self.status(), axum::Json(self.envelope())).into_response()
    }
}

/// The fixed, neutral message of each class.
pub(crate) fn platform_message(class: &PlatformErrorClass) -> &'static str {
    match class {
        PlatformErrorClass::ContextLengthExceeded => {
            "The request exceeds the model's context length."
        }
        PlatformErrorClass::RateLimited => "Rate limit exceeded. Retry later.",
        PlatformErrorClass::InvalidRequest { .. } => "The request is invalid.",
        PlatformErrorClass::ContentFiltered => {
            "The request or response was blocked by content policy."
        }
        PlatformErrorClass::Authentication => "Invalid API key.",
        PlatformErrorClass::NotFound => "The requested resource was not found.",
        PlatformErrorClass::UpstreamUnavailable => {
            "The model is temporarily unavailable. Retry later."
        }
        PlatformErrorClass::Internal => "Internal error.",
    }
}

/// The class `err` belongs to.
///
/// Platform mode replaces only the errors [`hides`] selects; for the
/// gateway's own refusals this is the class by status, and they keep their
/// own envelope on the wire.
pub(crate) fn classify(err: &ProxyError) -> PlatformErrorClass {
    match err {
        ProxyError::Bridge(e) => classify_bridge(e),
        // The target's own configuration: the platform's to fix, not the
        // caller's, like an upstream that refuses the platform's account.
        ProxyError::UnusableTarget(_) => PlatformErrorClass::UpstreamUnavailable,
        // The model cannot serve this endpoint, whichever provider is behind it.
        ProxyError::TargetRefused(_) => PlatformErrorClass::InvalidRequest { param: None },
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
            ..
        } => classify_upstream(Some(*status), parsed.as_deref(), message),
        BridgeError::UpstreamInBand {
            status,
            message,
            parsed,
            ..
        } => classify_upstream(*status, parsed.as_deref(), message),
        // A request field this provider's bridge cannot translate: the text
        // names the provider, the refusal is still of the request.
        BridgeError::InvalidRequest(_) | BridgeError::RequestTooLarge { .. } => {
            PlatformErrorClass::InvalidRequest { param: None }
        }
        BridgeError::Config(_) | BridgeError::UnsupportedCapability(_) => {
            PlatformErrorClass::Internal
        }
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

/// Codes and types upstreams use for a request their content policy refused:
/// Azure OpenAI's `content_filter` and its inner `ResponsibleAIPolicyViolation`,
/// and OpenAI's `content_policy_violation`.
const CONTENT_FILTER_TOKENS: &[&str] = &[
    "content_filter",
    "content_policy_violation",
    "ResponsibleAIPolicyViolation",
];

/// What upstreams write for a prompt that does not fit the model's context
/// window, lowercased: OpenAI and the OpenAI-compatible servers ("maximum
/// context length"), Anthropic ("prompt is too long"), Gemini ("exceeds the
/// maximum number of tokens"), Bedrock's publishers ("input is too long"),
/// and the code itself where a server writes it into the text. The
/// mainstream gateways read a context overflow off the same phrases.
const CONTEXT_OVERFLOW_MARKERS: &[&str] = &[
    "context_length_exceeded",
    "maximum context length",
    "context window",
    "prompt is too long",
    "input is too long",
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
) -> PlatformErrorClass {
    let names = |tokens: &[&str]| {
        view.is_some_and(|v| {
            [v.code.as_deref(), v.kind.as_deref()]
                .into_iter()
                .flatten()
                .any(|t| tokens.contains(&t))
        })
    };
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
    match status {
        Some(429) => PlatformErrorClass::RateLimited,
        // The platform's account was refused: not the caller's credential,
        // and not something the caller may learn about.
        Some(401 | 403) => PlatformErrorClass::UpstreamUnavailable,
        Some(404) => PlatformErrorClass::NotFound,
        Some(400..=499) => PlatformErrorClass::InvalidRequest {
            param: view.and_then(|v| v.param.clone()),
        },
        Some(_) => PlatformErrorClass::UpstreamUnavailable,
        None if names(RATE_LIMIT_TOKENS) => PlatformErrorClass::RateLimited,
        None => PlatformErrorClass::UpstreamUnavailable,
    }
}

/// Whether platform mode replaces `err` for the caller: an upstream's
/// failure, or a gateway refusal whose text names the target the request
/// resolved to. The one upstream-side variant left alone is the gateway's own
/// request size limit, which names nothing.
fn hides(err: &ProxyError) -> bool {
    match err {
        ProxyError::Bridge(e) => hides_bridge(e),
        ProxyError::UnusableTarget(_) | ProxyError::TargetRefused(_) => true,
        _ => false,
    }
}

fn hides_bridge(err: &BridgeError) -> bool {
    !matches!(err, BridgeError::RequestTooLarge { .. })
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

/// [`caller_class`] for the failure that ends a stream.
pub(crate) fn stream_class(err: &BridgeError) -> Option<PlatformErrorClass> {
    (active() && hides_bridge(err)).then(|| classify_bridge(err))
}

/// The `data:` payload of the OpenAI-shape SSE error frame ending a chat or
/// legacy completions stream on `err`, in platform mode: the class's
/// envelope, the same body the buffered response carries.
pub(crate) fn openai_stream_payload(err: &BridgeError) -> Option<String> {
    stream_class(err).map(|class| openai_payload(&class))
}

fn openai_payload(class: &PlatformErrorClass) -> String {
    serde_json::to_string(&class.envelope()).expect("an error envelope serializes")
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
/// platform mode sends it: the class of the error it reports. `event` is the
/// frame's parsed payload, `None` when it did not parse.
pub(crate) fn relayed_anthropic_error(event: Option<&Value>) -> Option<String> {
    if !active() {
        return None;
    }
    let class = event
        .and_then(aisix_provider_anthropic::wire::body_in_band_error)
        .map_or(PlatformErrorClass::UpstreamUnavailable, |e| {
            classify_bridge(&e)
        });
    Some(anthropic_frame(&class))
}

/// A Responses-API stream frame that reports an upstream's failure, relayed
/// from a native stream, as platform mode sends it. A `response.failed`
/// keeps its Response object with the class in place of its `error`; any
/// other failure frame becomes the flat `error` event, keeping the frame's
/// `sequence_number`.
pub(crate) fn relayed_responses_failure(frame: &[u8]) -> Option<Vec<u8>> {
    if !active() {
        return None;
    }
    let (event, _) = crate::redact::parse_sse_json_event(frame);
    let class = event
        .as_ref()
        .and_then(crate::responses::responses_in_band_error)
        .map_or(PlatformErrorClass::UpstreamUnavailable, |e| {
            classify_bridge(&e)
        });
    let error = serde_json::json!({ "code": class.code(), "message": platform_message(&class) });
    let rewritten = match event {
        Some(mut failed)
            if failed.get("type").and_then(Value::as_str) == Some("response.failed")
                && failed.get("response").is_some_and(Value::is_object) =>
        {
            failed["response"]["error"] = error;
            ("response.failed", failed)
        }
        event => {
            let param = match &class {
                PlatformErrorClass::InvalidRequest { param } => param.clone(),
                _ => None,
            };
            let mut flat = serde_json::json!({
                "type": "error",
                "code": class.code(),
                "message": platform_message(&class),
                "param": param,
            });
            if let Some(seq) = event.as_ref().and_then(|e| e.get("sequence_number")) {
                flat["sequence_number"] = seq.clone();
            }
            ("error", flat)
        }
    };
    Some(format!("event: {}\ndata: {}\n\n", rewritten.0, rewritten.1).into_bytes())
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
/// sends it: the gateway's own error frame carrying the class.
pub(crate) fn relayed_openai_error(frame: &[u8]) -> Option<Vec<u8>> {
    const KEY: &[u8] = b"\"error\"";
    if !active() || !frame.windows(KEY.len()).any(|w| w == KEY) {
        return None;
    }
    let (event, _) = crate::redact::parse_sse_json_event(frame);
    let err = aisix_gateway::capture_in_band_error_value(&event?, UpstreamWire::OpenAI)?;
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
                "The request exceeds the model's context length.",
            ),
            (
                PlatformErrorClass::RateLimited,
                429,
                "rate_limit_error",
                "rate_limited",
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
            text.contains("\"rate_limited\"") && !text.contains("Anthropic"),
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
            "The request exceeds the model's context length."
        );

        let resp = in_platform_mode(|| upstream_status(401, "x").into_anthropic_response());
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(resp).await["error"]["type"], "overloaded_error");
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
        assert!(!on.to_string().contains("bedrock"), "{on}");
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
            frame.contains("\"rate_limited\"") && !frame.contains("Anthropic"),
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

        let failed = b"event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":9,\"response\":{\"id\":\"resp_1\",\"status\":\"failed\",\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"Anthropic said no\"}}}\n\n";
        let out = in_platform_mode(|| relayed_responses_failure(failed)).unwrap();
        let (v, _) = crate::redact::parse_sse_json_event(&out);
        let v = v.unwrap();
        assert_eq!(v["type"], "response.failed");
        assert_eq!(v["response"]["status"], "failed");
        assert_eq!(v["response"]["error"]["code"], "context_length_exceeded");
        assert!(!String::from_utf8_lossy(&out).contains("Anthropic"));

        let mut buffer = b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n".to_vec();
        buffer.extend_from_slice(failed);
        let out = in_platform_mode(|| relayed_responses_buffer(&buffer)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("event: response.created\n"), "{text}");
        assert!(
            text.contains("\"context_length_exceeded\"") && !text.contains("Anthropic"),
            "{text}"
        );
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
}
