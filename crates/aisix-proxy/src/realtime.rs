//! `/v1/realtime` — OpenAI Realtime WebSocket relay (#721,
//! #873 §⑤).
//!
//! Authenticates on connect, resolves the target Model from `?model=`,
//! opens the provider WebSocket and relays frames bidirectionally.
//!
//! The relay is verbatim with one exception, `session.model` — see
//! [`restamp_session_model_out`] and its mirror.
//!
//! ## Protocol scope
//!
//! v1 relays the **OpenAI Realtime wire protocol**: adapter `openai`
//! (api.openai.com and any OpenAI-compatible `api_base`, which covers
//! xAI-style vendors) and `azure-openai` (`{base}/openai/realtime
//! ?api-version=…&deployment=…`, `api-key` header). Gemini Live / AWS
//! Bedrock speak entirely different session/event models and need a
//! cross-protocol translation layer (a mainstream gateway ships those as dedicated
//! per-provider `transform_realtime_request/response` modules) — that is
//! a separate feature, not part of this endpoint.
//!
//! OpenAI's Realtime API is GA and its GA endpoint **rejects**
//! `openai-beta: realtime=v1` (`beta_api_shape_disabled`), so the
//! gateway does not send it on its own. Beta and GA use different event
//! vocabularies, so the opt-in belongs to the caller that parses those
//! events: a client that sends the beta opt-in — as the `openai-beta`
//! header, or as the `openai-beta.realtime-v1` subprotocol item that the
//! browser flow uses — has it forwarded upstream, and nothing else does.
//!
//! ## Auth
//!
//! Two credential channels, checked before the upgrade completes:
//!
//! 1. `Authorization: Bearer <key>` / `x-api-key` headers — server-side
//!    clients.
//! 2. The `sec-websocket-protocol` item `openai-insecure-api-key.<key>`
//!    — browser clients cannot set headers; this is the documented
//!    OpenAI browser flow. The gateway echoes the `realtime` subprotocol
//!    when offered.
//!
//! Auth/ACL/quota failures reject the HTTP upgrade itself (401/403/429
//! envelope) rather than accept-then-close-1008: same enforcement point,
//! observable to every WS client as a failed handshake.
//!
//! ## Forwarded client headers
//!
//! The ProviderKey's `request.forward_client_headers` applies here as on
//! every other `/v1/*` face: the named headers ride the upstream
//! handshake, and a named credential slot displaces the ProviderKey's own
//! rather than joining it. The handshake slots this surface owns are the
//! one addition to the shared refusals — see [`REALTIME_HANDSHAKE_SLOTS`].
//! `request.default_headers` is a separate feature that this face has
//! never applied.
//!
//! ## Usage
//!
//! The relay harvests `response.done` usage frames (and
//! `conversation.item.input_audio_transcription.completed` token usage)
//! from the upstream stream, charges each to the spend metric and the limits
//! as it arrives, and emits ONE aggregated UsageEvent per
//! session (`inbound_protocol = "realtime"`), committing total tokens to
//! the rate-limit reservation like the other non-chat surfaces (#911
//! [21]).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use aisix_core::models::model::Adapter;
use aisix_obs::{AccessLog, UsageEvent};
use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as TgCloseFrame;
use tokio_tungstenite::tungstenite::Message as TgMessage;

use crate::auth::AuthenticatedKey;

/// Largest realtime message the gateway will assemble, and the largest single
/// frame within it. Both match the current tungstenite defaults; naming them
/// keeps the bound stable if that default moves, and makes it reviewable.
const REALTIME_MAX_MESSAGE_BYTES: usize = 64 << 20;
const REALTIME_MAX_FRAME_BYTES: usize = 16 << 20;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::state::ProxyState;
use aisix_gateway::BridgeError;

/// Azure Realtime GA api-version (see the jobs surface twin constant).
const AZURE_REALTIME_API_VERSION: &str = "2024-10-01-preview";

/// Subprotocol item carrying the caller's API key in the browser flow.
const SUBPROTOCOL_KEY_PREFIX: &str = "openai-insecure-api-key.";

/// Header value by which a client opts into the legacy beta event shape.
const HEADER_BETA_VALUE: &str = "realtime=v1";

/// Subprotocol item by which a browser client opts into the legacy beta
/// event shape (a subprotocol token cannot contain `=`, so the header's
/// `realtime=v1` is spelled `realtime-v1` here).
const SUBPROTOCOL_BETA_ITEM: &str = "openai-beta.realtime-v1";

/// Handshake slots this surface owns, refused to `forward_client_headers`
/// on top of the shared lists.
///
/// Every other `/v1/*` face rebuilds an HTTP request; this one performs a
/// second WebSocket handshake, and these headers describe the handshake
/// the CALLER made rather than the request's content.
///
/// `sec-websocket-protocol` is the dangerous one: the documented browser
/// flow puts the caller's own AISIX key in it
/// (`openai-insecure-api-key.<key>`), so relaying it would hand the
/// provider the credential this gateway authenticates with. It also
/// selects the subprotocol the gateway echoes back to the client, which
/// is the gateway's answer to make, not the upstream's. Nothing else
/// declines it — the outbound handshake does not carry one, so a matching
/// pattern would insert the caller's list verbatim. `-extensions` is the
/// same shape and enables a compression the gateway's own codec never
/// negotiated.
///
/// `sec-websocket-key` and `-version` are listed for completeness rather
/// than because they are reachable today: the outbound request already
/// carries both, and a non-credential name already present is declined on
/// delivery. Naming them here keeps that from being the only thing
/// standing between a caller's key and the accept value computed from it.
const REALTIME_HANDSHAKE_SLOTS: &[&str] = &[
    "sec-websocket-accept",
    "sec-websocket-extensions",
    "sec-websocket-key",
    "sec-websocket-protocol",
    "sec-websocket-version",
];

/// Is `needle` one of the comma-separated items across every value of
/// `name`? Both headers are list-valued and may repeat, so a first-value
/// substring test would both miss a repeat and accept `realtime=v10`.
fn header_list_has(headers: &HeaderMap, name: &str, needle: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|item| item.trim().eq_ignore_ascii_case(needle))
}

/// Did the caller ask for the legacy beta Realtime shape?
///
/// OpenAI's beta and GA Realtime APIs use different event vocabularies,
/// so the opt-in belongs to the client that has to parse those events —
/// not to us. We forward `openai-beta: realtime=v1` upstream only when
/// the caller sent the same opt-in, via either channel it has: the
/// header (server-side clients) or the `sec-websocket-protocol` item
/// (browser clients, which cannot set headers).
fn client_requested_beta_realtime(headers: &HeaderMap) -> bool {
    header_list_has(headers, "openai-beta", HEADER_BETA_VALUE)
        || header_list_has(headers, "sec-websocket-protocol", SUBPROTOCOL_BETA_ITEM)
}

type UpstreamDial = Result<
    (
        tokio_tungstenite::WebSocketStream<HandedBack>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    DialFailure,
>;

/// A dial that opened no session, with the stream when the handshake got far
/// enough to have one to give back.
struct DialFailure {
    error: tokio_tungstenite::tungstenite::Error,
    stream: Option<aisix_gateway::upstream_dial::UpstreamStream>,
}

/// The dial's stream, handed back when the handshake drops it. tungstenite keeps
/// only the bytes that came with a refused upgrade's response head and drops the
/// stream, so the rest of the body can only be read from here.
struct HandedBack {
    stream: Option<aisix_gateway::upstream_dial::UpstreamStream>,
    back: Option<tokio::sync::oneshot::Sender<aisix_gateway::upstream_dial::UpstreamStream>>,
}

impl HandedBack {
    fn io(
        self: std::pin::Pin<&mut Self>,
    ) -> std::pin::Pin<&mut aisix_gateway::upstream_dial::UpstreamStream> {
        std::pin::Pin::new(
            self.get_mut()
                .stream
                .as_mut()
                .expect("the stream leaves only on drop"),
        )
    }
}

impl Drop for HandedBack {
    fn drop(&mut self) {
        if let (Some(stream), Some(back)) = (self.stream.take(), self.back.take()) {
            // No receiver once the upgrade succeeded: the stream just closes.
            let _ = back.send(stream);
        }
    }
}

impl tokio::io::AsyncRead for HandedBack {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.io().poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for HandedBack {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.io().poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.io().poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.io().poll_shutdown(cx)
    }
}

/// What a refused upgrade answered, for the failure row.
struct RefusedResponse {
    headers: HeaderMap,
    body: bytes::Bytes,
}

/// What a failed dial reports. An upgrade the upstream answered with an error
/// reads as an HTTP route's upstream failure, the OpenAI envelope included, and
/// comes with what it answered. A dial past its budget is a timeout, as it is on
/// those routes: their `reqwest::Error::is_timeout` also takes the kernel's
/// `ETIMEDOUT` for one.
async fn connect_failure(
    failure: DialFailure,
    elapsed: Duration,
    read_budget: Option<Duration>,
    request_id: &str,
    client_gone: impl std::future::Future<Output = ()>,
) -> (BridgeError, Option<RefusedResponse>) {
    use tokio_tungstenite::tungstenite::Error;
    let response = match failure.error {
        Error::Http(response) => response,
        Error::Io(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            let error = BridgeError::Timeout {
                elapsed_ms: elapsed.as_millis() as u64,
                cause: aisix_gateway::error_with_causes(&e),
            };
            return (error, None);
        }
        other => {
            let error = BridgeError::Transport(aisix_gateway::error_with_causes(&other));
            return (error, None);
        }
    };
    let status = response.status().as_u16();
    let retry_after = aisix_gateway::parse_retry_after(response.headers());
    let wire = aisix_gateway::UpstreamWire::OpenAI;
    let body = read_refused_body(&response, failure.stream, read_budget, client_gone).await;
    if let Some(reason) = &body.read_error {
        tracing::warn!(status, request_id, %reason, "failed to read upstream error response body");
    }
    let blank = body.read_error.is_none() && body.bytes.iter().all(u8::is_ascii_whitespace);
    let error = if status < 400 && blank {
        BridgeError::UpstreamStatus {
            status,
            message: String::new(),
            parsed: None,
            wire,
            retry_after,
            diagnostic: Some(format!(
                "upstream returned HTTP {status} during the realtime handshake"
            )),
        }
    } else {
        aisix_gateway::upstream_status_of_body(
            status,
            body.whole(),
            retry_after,
            wire,
            aisix_provider_openai::parse_openai_error_envelope,
        )
        .with_body_read_error(body.read_error.clone())
    };
    let refused = RefusedResponse {
        headers: response.headers().clone(),
        body: body.bytes,
    };
    (error, Some(refused))
}

/// The session's `error` frame for a failed dial: an upstream's answer in the
/// envelope an HTTP route would send, fixed text when there was no answer.
fn connect_failure_frame(err: &ProxyError) -> String {
    let error = match err {
        ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamStatus { .. }) => {
            err.envelope().error
        }
        _ => {
            crate::error::ErrorEnvelope::new(
                "failed to connect to the upstream realtime endpoint",
                "upstream_error",
            )
            .error
        }
    };
    serde_json::json!({ "type": "error", "error": error }).to_string()
}

/// The refused upgrade's body, up to the bound and the time limit of an HTTP
/// route's error body, or until the client leaves, which on this detached task
/// would otherwise cancel nothing. A compressed one stays unread: the handshake
/// asks for no encoding and nothing here decodes one.
async fn read_refused_body(
    response: &tokio_tungstenite::tungstenite::handshake::client::Response,
    stream: Option<aisix_gateway::upstream_dial::UpstreamStream>,
    budget: Option<Duration>,
    client_gone: impl std::future::Future<Output = ()>,
) -> aisix_gateway::UpstreamErrorBody {
    let headers = response.headers();
    let coding = headers
        .get(axum::http::header::TRANSFER_ENCODING)
        .map(|v| v.to_str().unwrap_or_default());
    let chunked = coding.is_some_and(|c| c.eq_ignore_ascii_case("chunked"));
    let other_coding = coding.is_some_and(|c| !chunked && !c.eq_ignore_ascii_case("identity"));
    if other_coding || headers.contains_key(axum::http::header::CONTENT_ENCODING) {
        return aisix_gateway::UpstreamErrorBody {
            bytes: bytes::Bytes::new(),
            read_error: None,
        };
    }
    let declared = if matches!(response.status().as_u16(), 204 | 304) {
        Some(0)
    } else {
        headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse::<usize>().ok())
    };
    let mut raw = response.body().clone().unwrap_or_default();
    let read = read_body_framing(&mut raw, chunked, declared, stream);
    let timed = async {
        match budget {
            Some(budget) => tokio::time::timeout(budget, read)
                .await
                .unwrap_or_else(|_| Err(format!("timed out after {}ms", budget.as_millis()))),
            None => read.await,
        }
    };
    let read_error = tokio::select! {
        outcome = timed => outcome.err(),
        () = client_gone => Some("the client closed the session".to_string()),
    };
    let mut body = if chunked {
        decode_chunked(&raw).map_or_else(Vec::new, |(body, _)| body)
    } else {
        raw
    };
    if let Some(declared) = declared {
        body.truncate(declared);
    }
    body.truncate(aisix_gateway::MAX_UPSTREAM_ERROR_BODY_BYTES);
    aisix_gateway::UpstreamErrorBody {
        bytes: aisix_gateway::scrub_upstream_error_body(&body.into()),
        read_error,
    }
}

/// Read into `raw` by the framing the head declares: `Content-Length`, chunked,
/// or up to the end of the connection. Nothing past the bound is read, so a
/// longer body reads as an HTTP route's does once cut at the same bound.
async fn read_body_framing(
    raw: &mut Vec<u8>,
    chunked: bool,
    declared: Option<usize>,
    mut stream: Option<aisix_gateway::upstream_dial::UpstreamStream>,
) -> Result<(), String> {
    use tokio::io::AsyncReadExt as _;
    const LIMIT: usize = aisix_gateway::MAX_UPSTREAM_ERROR_BODY_BYTES;
    let mut buf = [0u8; 8 * 1024];
    loop {
        if chunked {
            match decode_chunked(raw) {
                Some((_, true)) => return Ok(()),
                Some((body, false)) if body.len() >= LIMIT => return Ok(()),
                // Room for a body in one-byte chunks; only broken framing gets here.
                Some(_) if raw.len() < 8 * LIMIT => {}
                _ => return Err("malformed chunked body".to_string()),
            }
        } else if raw.len() >= declared.unwrap_or(LIMIT).min(LIMIT) {
            return Ok(());
        }
        let read = match stream.as_mut() {
            Some(stream) => stream.read(&mut buf).await,
            None => Ok(0),
        };
        match read {
            Ok(0) if chunked || declared.is_some() => {
                return Err("the connection closed before the whole body arrived".to_string())
            }
            Ok(0) => return Ok(()),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The payload of the chunked bytes so far, and whether they reached the last
/// chunk. `None` when they are not chunked framing.
fn decode_chunked(mut raw: &[u8]) -> Option<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    loop {
        let Some(line_end) = raw.windows(2).position(|w| w == b"\r\n") else {
            return Some((body, false));
        };
        let size = std::str::from_utf8(&raw[..line_end]).ok()?;
        let size = size.split(';').next()?.trim();
        let size = usize::from_str_radix(size, 16).ok()?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            return Some((body, true));
        }
        if raw.len() < size {
            body.extend_from_slice(raw);
            return Some((body, false));
        }
        body.extend_from_slice(&raw[..size]);
        raw = &raw[size..];
        if raw.len() < 2 {
            return Some((body, false));
        }
        raw = raw.strip_prefix(b"\r\n")?;
    }
}

/// Dial the upstream Realtime endpoint under the deployment's outbound
/// TLS trust, on the deployment's connect budget, through the
/// environment proxy that takes it (`upstream_dial`).
///
/// `connect_async` would build its own connector over the compiled-in
/// root set only, which leaves this the one upstream path that ignores
/// `upstream.tls` *and* `SSL_CERT_FILE` — a self-hosted Realtime
/// endpoint behind an enterprise CA would fail here while the same
/// provider's `/v1/chat/completions` worked.
async fn connect_upstream(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
) -> UpstreamDial {
    connect_upstream_within(
        aisix_gateway::upstream_http::config().connect_timeout,
        request,
    )
    .await
}

/// The dial itself, budget passed in so a test can use one far shorter
/// than the deployment default.
///
/// The budget covers the WHOLE dial — DNS, TCP, any proxy tunnel, TLS
/// *and* the WebSocket handshake exchange — where the HTTP routes'
/// `connect_timeout` stops at the end of TLS. The extra phase is the one
/// that matters most here: an upstream that completes TLS and then never answers the upgrade is as
/// stuck as one that never answers the SYN, and nothing downstream
/// bounds it — the session's idle deadline only starts once the socket
/// is up. Left unbounded the upgrade hangs until the kernel exhausts its
/// SYN retries, minutes after every other route would have failed.
async fn connect_upstream_within(
    budget: Option<Duration>,
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
) -> UpstreamDial {
    let (back, mut handed_back) = tokio::sync::oneshot::channel();
    let dial = async {
        let uri = request.uri();
        let secure = match uri.scheme_str() {
            Some("wss") => true,
            Some("ws") => false,
            _ => {
                return Err(tokio_tungstenite::tungstenite::Error::Url(
                    tokio_tungstenite::tungstenite::error::UrlError::UnsupportedUrlScheme,
                ))
            }
        };
        let host = uri
            .host()
            .ok_or(tokio_tungstenite::tungstenite::Error::Url(
                tokio_tungstenite::tungstenite::error::UrlError::NoHostName,
            ))?;
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        let mut stream = aisix_gateway::upstream_dial::connect(host, port, secure).await?;
        if secure {
            stream = aisix_gateway::upstream_dial::connect_tls(stream, host).await?;
        }
        let stream = HandedBack {
            stream: Some(stream),
            back: Some(back),
        };
        tokio_tungstenite::client_async_with_config(request, stream, None).await
    };
    let outcome = match budget {
        None => dial.await,
        Some(budget) => tokio::time::timeout(budget, dial)
            .await
            .unwrap_or_else(|_| {
                Err(tokio_tungstenite::tungstenite::Error::Io(
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("upstream connect exceeded upstream.connect_timeout ({budget:?})"),
                    ),
                ))
            }),
    };
    outcome.map_err(|error| DialFailure {
        error,
        stream: handed_back.try_recv().ok(),
    })
}

pub(crate) async fn realtime(
    State(state): State<ProxyState>,
    method: Method,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    mut client: ClientContext,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let request_id = client.request_id.clone();
    let started = Instant::now();

    // A non-WebSocket request (plain GET, malformed upgrade headers, a
    // connection that cannot upgrade, or a HEAD — `get()` serves HEAD
    // too) used to get axum's bare rejection — no access log, no metrics,
    // no usage event, no envelope; the same silent class #863/#880/#884
    // collected (#885). Map it into this endpoint's normal error arm,
    // keeping axum's own status classification (400 / 426 / 405) and its
    // per-variant diagnostic.
    //
    // Auth runs here rather than through the extractor (the browser flow
    // carries the credential in a subprotocol item), so a failure between
    // auth and dispatch must hand the resolved key back for attribution —
    // pre-#932 those errors were emitted as if anonymous.
    //
    // One snapshot for the whole pre-upgrade phase (#941): `prepare`
    // resolves the model against it and the rejection arm below reports
    // through it, so a refused upgrade cannot straddle two config
    // generations. The detached session's terminal emit deliberately reads
    // a fresh one — it can run for minutes.
    let snapshot = state.snapshot.load();
    let outcome = match ws {
        Ok(ws) => match authenticate(&state, &headers, &client).await {
            Ok(auth) => {
                // Every other family gets `client.jwt` published by the
                // auth extractor; do the same here so the session clone
                // and the error emits below attribute the JWT identity.
                client.jwt = auth.jwt.clone();
                prepare(&state, &snapshot, &params, &headers, &client, auth.clone())
                    .await
                    .map(|prep| (ws, prep))
                    .map_err(|err| (Some(auth), err))
            }
            Err(err) => Err((None, err)),
        },
        Err(rejection) => Err((
            None,
            crate::error::ProxyError::WebSocketUpgradeRequired {
                status: rejection.status(),
                detail: rejection.body_text(),
            },
        )),
    };

    match outcome {
        Ok((ws, prep)) => {
            let state2 = state.clone();
            let client2 = client.clone();
            // `on_upgrade` runs the session on a detached task, so the
            // request span has to be attached to the future rather than
            // inherited — without it the session's guardrail checks log
            // without a `request_id` (#1060).
            let span = tracing::Span::current();
            // The session is in flight for the drain exactly like a
            // streaming response, so a shutdown waits for it. Taken here,
            // while the request's own guard is still held, and released
            // only after `run_session` has emitted the session's
            // UsageEvent, so the usage sender cannot stop between the two.
            let in_flight = crate::DrainGuard::new(state.livez.clone());
            // The session reads the upstream's errors off the request, so it
            // takes the credentials the request noted with it.
            let tap = aisix_gateway::response_tap::detached();
            ws.protocols(["realtime"])
                // State the frame bounds rather than inheriting them. These
                // are today's tungstenite defaults, so this changes nothing
                // now — the point is that a DoS-relevant bound on a
                // long-lived, caller-driven socket should be a decision here,
                // not whatever a transitive dependency happens to default to.
                //
                // Deliberately NOT `proxy.request_body_limit_bytes`: that caps
                // one HTTP body, while a realtime session streams base64 audio
                // in `input_audio_buffer.append` frames, so an operator's
                // (reasonable) 1 MB request cap would break audio outright.
                .max_message_size(REALTIME_MAX_MESSAGE_BYTES)
                .max_frame_size(REALTIME_MAX_FRAME_BYTES)
                .on_upgrade(move |socket| {
                    use tracing::Instrument as _;
                    aisix_gateway::response_tap::scope(
                        tap,
                        async move {
                            run_session(state2, prep, socket, client2, request_id, started).await;
                            drop(in_flight);
                        }
                        .instrument(span),
                    )
                })
        }
        Err((auth, err)) => {
            let status = err.status().as_u16();
            let api_key_id = auth.as_ref().map(|a| a.entry.id.as_str());
            // `prepare` notes the model when it finds it and the target when
            // it selects one, so what is empty here is what it never reached.
            let attributed = crate::attribution::current().unwrap_or_default();
            let last_target = crate::request_metrics::LastTarget::new(&snapshot, &attributed);
            let addressed = Some(attributed.requested_model.as_str()).filter(|m| !m.is_empty());
            emit_access_log(
                &method,
                status,
                started.elapsed(),
                &request_id,
                api_key_id,
                addressed.map(|model| {
                    let provider = last_target.provider();
                    let known = provider != crate::request_metrics::UNKNOWN;
                    (if known { provider } else { "" }, model)
                }),
                Some(&err),
                None,
                None,
            );
            // Count the refusal like every other pre-dispatch rejection —
            // logs and the request-rate metrics must not disagree about
            // whether these requests exist. Before `prepare` found the model
            // the labels are unresolved, same as `reject_before_dispatch`.
            // Authentication may not have run, so the caller is attributed
            // only when a key was resolved (#932).
            crate::request_metrics::record(
                &state,
                "/v1/realtime",
                match auth.as_ref() {
                    Some(a) => crate::request_metrics::Caller::new(a),
                    None => crate::request_metrics::Caller::unattributed(None),
                },
                last_target.upstream(
                    &addressed.map_or(
                        std::borrow::Cow::Borrowed(crate::usage_attr::UNRESOLVED_MODEL_LABEL),
                        |model| crate::usage_attr::metric_model_label(&snapshot, model),
                    ),
                    false,
                    false,
                ),
                status,
                crate::attempt::error_source(&err),
                started.elapsed(),
            );
            crate::usage_attr::note_client_error(&snapshot, &err);
            crate::usage_attr::emit_error_usage_event(
                &state,
                &snapshot,
                crate::operation::REALTIME,
                "realtime",
                &request_id,
                params.get("model").map_or("", |model| model.trim()),
                api_key_id.unwrap_or(""),
                status,
                crate::usage_attr::EventError::of(&err),
                err.is_guardrail_block(),
                &client,
                // Refused before the handshake, so no chain was ever
                // resolved: none applied, and none can have enforced
                // anything — nor scored anything, nor been bypassed.
                Vec::new(),
                Vec::new(),
                Vec::new(),
                String::new(),
            );
            err.into_response()
        }
    }
}

/// Everything resolved before the upgrade is accepted.
struct Prepared {
    auth: AuthenticatedKey,
    model_entry: std::sync::Arc<aisix_core::ResourceEntry<aisix_core::Model>>,
    pk_id: String,
    upstream_request: tokio_tungstenite::tungstenite::handshake::client::Request,
    reservation: aisix_ratelimit::MultiReservation,
    requested_model: String,
    /// Provider-side model id the upstream session was opened with. Only
    /// [`restamp_session_model_in`] reads it — see there for why.
    upstream_model: String,
    provider_label: String,
}

async fn prepare(
    state: &ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    client: &ClientContext,
    auth: AuthenticatedKey,
) -> Result<Prepared, ProxyError> {
    let beta_realtime = client_requested_beta_realtime(headers);
    let requested_model = params
        .get("model")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if requested_model.is_empty() {
        return Err(ProxyError::InvalidRequest(
            "`model` query parameter is required on /v1/realtime".into(),
        ));
    }

    let model_entry = crate::model_resolve::resolve_model(snapshot, &requested_model)
        .ok_or_else(|| ProxyError::ModelNotFound(requested_model.to_string()))?;
    if !auth.key().can_access(snapshot, &requested_model) {
        return Err(ProxyError::ModelForbidden(requested_model.to_string()));
    }
    let model = &model_entry.value;
    if model.is_routing() || model.is_ensemble() || model.is_semantic() {
        return Err(ProxyError::InvalidRequest(format!(
            "model {requested_model:?} is a virtual router; /v1/realtime requires a direct model"
        )));
    }
    // The key's provider groups hold for a session as for every other
    // dispatch: it is opened only with a provider key in one of them, and
    // otherwise refused the way a routing group with no target left is.
    if !crate::routing::target_in_groups(snapshot, model, &auth.key().allowed_provider_groups) {
        return Err(crate::routing::no_target_in_groups(&requested_model));
    }
    crate::dispatch::check_ip_access(model, &client.source_ip)?;

    let pk_entry = crate::dispatch::resolve_provider_key(snapshot, model)?;
    let secret = crate::dispatch::require_api_key(&pk_entry.value, model)?.to_string();
    let upstream_model = crate::dispatch::require_upstream_model(model)?.to_string();

    // The ProviderKey's `request.forward_client_headers`, resolved against
    // the caller's own handshake. This face builds its upstream request by
    // hand instead of through the shared bridge pipeline, which is exactly
    // where a per-request mechanism goes silently missing — an operator
    // who declared that this upstream reads a credential the caller sends
    // got it on every other `/v1/*` endpoint and not here.
    let mut forwarded = aisix_gateway::ForwardedClientHeaders::resolve(
        &aisix_gateway::UpstreamHeaderContext::from_overrides(pk_entry.value.request.as_ref())
            .with_client_headers(headers)
            .with_surface_blocked(REALTIME_HANDSHAKE_SLOTS),
    );
    // The WebSocket client renders the handshake as text and refuses a
    // header value it cannot read as a string, failing the whole upstream
    // connection. Dropping the entry keeps a caller who sent one obs-text
    // byte from being unable to open a session at all — on every other
    // face the same header is forwarded byte-for-byte.
    let dropped = forwarded.drop_non_ascii_values();
    if dropped > 0 {
        tracing::debug!(
            dropped,
            "forwarded client headers with non-ASCII values are not sent on a realtime handshake"
        );
    }

    let mut upstream_request = match pk_entry.value.adapter {
        Some(Adapter::Openai) => {
            let base = crate::dispatch::resolve_base_url(&pk_entry.value)?;
            let url = crate::dispatch::build_openai_url(&base, "/realtime");
            let url = format!(
                "{}?model={}",
                to_ws_scheme(&url)?,
                urlencode(&upstream_model)
            );
            let mut req = url.into_client_request().map_err(|e| {
                ProxyError::InvalidRequest(format!("invalid upstream realtime URL: {e}"))
            })?;
            req.headers_mut().insert(
                "authorization",
                format!("Bearer {secret}").parse().map_err(|_| {
                    ProxyError::InvalidRequest("provider secret is not header-safe".into())
                })?,
            );
            // Only when the CALLER opted into the legacy beta shape.
            // OpenAI's GA `/v1/realtime` rejects the header outright
            // (`beta_api_shape_disabled`) and closes the session, so
            // sending it unconditionally broke every GA connection.
            // The established gateway convention:
            // forward it iff the client asked for it.
            if beta_realtime {
                req.headers_mut()
                    .insert("openai-beta", "realtime=v1".parse().unwrap());
            }
            req
        }
        Some(Adapter::AzureOpenai) => {
            let base = pk_entry
                .value
                .api_base
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .ok_or_else(|| {
                    ProxyError::InvalidRequest(format!(
                        "azure provider_key {:?} has no api_base",
                        pk_entry.value.display_name
                    ))
                })?
                .trim_end_matches('/')
                .to_string();
            let url = format!(
                "{}/openai/realtime?api-version={AZURE_REALTIME_API_VERSION}&deployment={}",
                to_ws_scheme(&base)?,
                urlencode(&upstream_model)
            );
            let mut req = url.into_client_request().map_err(|e| {
                ProxyError::InvalidRequest(format!("invalid upstream realtime URL: {e}"))
            })?;
            req.headers_mut().insert(
                "api-key",
                secret.parse().map_err(|_| {
                    ProxyError::InvalidRequest("provider secret is not header-safe".into())
                })?,
            );
            req
        }
        _ => {
            return Err(ProxyError::InvalidRequest(format!(
                "model {requested_model:?} uses provider {:?} which does not speak the OpenAI \
                 Realtime protocol; /v1/realtime supports OpenAI-compatible and Azure OpenAI \
                 providers",
                pk_entry.value.provider
            )));
        }
    };
    // AFTER the per-adapter build, which is what lets a credential slot
    // the operator named displace the ProviderKey's own — the ordering
    // `apply` documents and every other face follows. It leaves any other
    // header the arms above set alone: those select how the exchange
    // works, not who it is from.
    forwarded.apply(upstream_request.headers_mut());

    let reservation = crate::quota::enforce(
        state,
        snapshot,
        &auth,
        Some(&crate::quota::ModelRateLimit::from_model(
            &model_entry.value.display_name,
            &model_entry.id,
            &model_entry.value,
        )),
    )
    .await?;
    crate::budget::gate(state, snapshot).check_provider_key(&pk_entry.id)?;

    let provider_label = model.provider.clone().unwrap_or_default();
    Ok(Prepared {
        auth,
        pk_id: pk_entry.id.to_string(),
        model_entry,
        upstream_request,
        reservation,
        requested_model,
        upstream_model,
        provider_label,
    })
}

/// Header credential (`x-aisix-api-key` / `Authorization` / `x-api-key`,
/// see [`crate::auth::header_credential`]) first, then the browser
/// subprotocol credential.
async fn authenticate(
    state: &ProxyState,
    headers: &HeaderMap,
    client: &ClientContext,
) -> Result<AuthenticatedKey, ProxyError> {
    // `/v1/realtime` is a WebSocket upgrade, so there are no request parts
    // to read here — the resolved client context carries the same two
    // identifying fields the HTTP extractor puts on a denial.
    let ctx = crate::auth::DenialContext {
        method: "GET",
        path: "/v1/realtime",
        request_id: &client.request_id,
        source_ip: crate::auth::LazySourceIp::Ready(&client.source_ip),
    };
    if let Some(token) = crate::auth::header_credential(headers) {
        return crate::auth::authenticate_token(state, &token?, ctx).await;
    }
    // List-valued and splittable across repeated fields, like the beta
    // opt-in above: reading only the first field 401s a caller whose
    // credential rides a later one.
    for proto in headers.get_all("sec-websocket-protocol").iter() {
        let Ok(s) = proto.to_str() else { continue };
        for item in s.split(',') {
            if let Some(token) = item.trim().strip_prefix(SUBPROTOCOL_KEY_PREFIX) {
                if !token.is_empty() {
                    return crate::auth::authenticate_token(state, token, ctx).await;
                }
            }
        }
    }
    Err(ProxyError::MissingAuth(crate::auth::AUTHORIZATION_LABEL))
}

fn to_ws_scheme(url: &str) -> Result<String, ProxyError> {
    if let Some(rest) = url.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = url.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else if url.starts_with("ws://") || url.starts_with("wss://") {
        Ok(url.to_string())
    } else {
        // A 400 the caller reads: never with the password of a configured URL.
        let shown = aisix_core::redact_userinfo(url);
        Err(ProxyError::InvalidRequest(format!(
            "api_base {shown:?} has no http(s) scheme"
        )))
    }
}

fn urlencode(s: &str) -> String {
    // Conservative percent-encoding for the query-value position.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Restamp `session.model` on its way DOWN to the client.
///
/// `session.created` and `session.updated` are the only Realtime server
/// events that name a model, and they name the one the provider is running —
/// so a caller who connected with a gateway alias was told a different name
/// than the one they addressed. That is the `model_echo` contract, applied on
/// this surface (#1088).
///
/// A frame that names no model, or that the splice scanner refuses, is
/// returned unchanged.
fn restamp_session_model_out(text: String, client_facing_model: &str) -> String {
    splice_or_keep(text, |bytes| {
        crate::model_echo::restamp_json_bytes(
            bytes,
            client_facing_model,
            crate::model_echo::realtime_session_model,
        )
    })
}

/// Translate `session.model` back on its way UP to the provider.
///
/// The mirror of [`restamp_session_model_out`], and it exists because of it.
/// Realtime clients routinely take the `session` object the gateway just
/// handed them, change one field and send the whole thing back as
/// `session.update` — which now carries the gateway's alias where it used to
/// carry the provider's own id. Only that alias is translated; a client that
/// names anything else reaches the provider with its own words, and gets the
/// provider's own answer about it.
///
/// Forwarding those other values verbatim is what this relay has always
/// done, and this function does not change it — but note what it is and is
/// not. The upstream session's model is fixed by the connect-time query
/// parameter, and the Realtime protocol documents `model` as one of the two
/// fields `session.update` cannot change, so a provider that follows the
/// spec ignores whatever a client puts there. That is the PROVIDER's
/// guarantee, not one the gateway enforces: against a permissive
/// OpenAI-compatible server that did honour it, a caller could name a model
/// the gateway attributed nothing to. Pre-existing either way, and out of
/// scope here — do not read the passthrough as a check.
fn restamp_session_model_in(
    text: String,
    client_facing_model: &str,
    upstream_model: &str,
) -> String {
    splice_or_keep(text, |bytes| {
        crate::model_echo::splice_model_value(
            bytes,
            crate::model_echo::realtime_session_model,
            |named| (named == client_facing_model).then(|| upstream_model.to_string()),
        )
    })
}

/// Run a splice over one WebSocket text frame, keeping the original string
/// whenever there is nothing to rewrite.
///
/// The fast path matters: every audio delta is a text frame, and only two
/// event types in the protocol carry a session at all.
///
/// It tests for a backslash as well as for the literal key, and that second
/// condition is what makes it safe rather than merely quick. JSON lets a key
/// be spelled with escapes, so `session` can also arrive as
/// `\u0073ession` — the splice walker decodes keys and would match it, but a
/// fast path looking only for the literal spelling would have skipped the
/// frame before the walker ever saw it. An escape needs a backslash, so a
/// frame carrying neither cannot name `session` at all, and skipping it is
/// sound. (An audio delta is base64, which has no backslash, so the cheap
/// case stays cheap.)
fn splice_or_keep(text: String, splice: impl FnOnce(&[u8]) -> Option<Vec<u8>>) -> String {
    if !text.contains("\"session\"") && !text.contains('\\') {
        return text;
    }
    match splice(text.as_bytes()).map(String::from_utf8) {
        Some(Ok(rewritten)) => rewritten,
        _ => text,
    }
}

/// Accumulated session usage harvested from upstream frames.
#[derive(Default)]
struct SessionUsage {
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    responses: u32,
    /// Summed across the session's usage frames, `None` where none
    /// reported the modality.
    modality: aisix_gateway::ModalityTokens,
    /// The upstream's own `total_tokens` across the usage frames.
    upstream_total: crate::usage_attr::ReportedTotalSum,
}

impl SessionUsage {
    fn absorb(&mut self, text: &str) {
        // Fast path: only parse frames that can carry usage.
        if !text.contains("\"response.done\"")
            && !text.contains("\"conversation.item.input_audio_transcription.completed\"")
        {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("response.done") => {
                let usage = &v["response"]["usage"];
                let input = usage["input_tokens"].as_u64().unwrap_or(0);
                let output = usage["output_tokens"].as_u64().unwrap_or(0);
                self.input_tokens += input;
                self.output_tokens += output;
                self.upstream_total.add(usage, input + output > 0);
                self.cached_tokens += usage["input_token_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                self.modality = self.modality.saturating_add(
                    &aisix_gateway::ModalityTokens::from_openai_details(
                        usage.get("input_token_details"),
                        usage.get("output_token_details"),
                    ),
                );
                self.responses += 1;
            }
            Some("conversation.item.input_audio_transcription.completed") => {
                // Transcription-intent sessions bill via this frame.
                let usage = &v["usage"];
                if usage.get("type").and_then(Value::as_str) == Some("tokens") {
                    let input = usage["input_tokens"].as_u64().unwrap_or(0);
                    let output = usage["output_tokens"].as_u64().unwrap_or(0);
                    self.input_tokens += input;
                    self.output_tokens += output;
                    self.upstream_total.add(usage, input + output > 0);
                    self.modality = self.modality.saturating_add(
                        &aisix_gateway::ModalityTokens::from_openai_details(
                            usage.get("input_token_details"),
                            None,
                        ),
                    );
                    self.responses += 1;
                }
            }
            _ => {}
        }
    }
}

/// What a session has charged so far, and everything it needs to charge more.
///
/// A session runs for as long as its traffic does, so nothing it spends can wait
/// for the close: a ledger reading the spend counter would not see it, a limit
/// could not act on it, and a process that dies in between would never bill it.
/// It settles each usage frame as it arrives.
struct SessionMeter<'a> {
    state: &'a ProxyState,
    auth: &'a crate::auth::AuthenticatedKey,
    model_entry: &'a aisix_core::ResourceEntry<aisix_core::Model>,
    pk_id: &'a str,
    provider_label: &'a str,
    requested_model: &'a str,
    client_type: &'a str,
    token_keys: Vec<String>,
    spend_keys: Vec<String>,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    micro_usd: u64,
    /// What the session has cost so far: each response priced when it arrived.
    cost_usd: f64,
    responses: u32,
}

impl SessionMeter<'_> {
    /// Whether frames arrived that are not charged yet.
    fn behind(&self, usage: &SessionUsage) -> bool {
        self.responses != usage.responses
    }

    /// What these tokens cost at the price in force now, through the index every
    /// other reader of a model's price uses, from the row the session started on:
    /// it can outlive the generation that had that row, and re-reading it could
    /// price the whole session at nothing.
    fn price_usd(
        &self,
        snap: &aisix_core::AisixSnapshot,
        input: u64,
        cached: u64,
        output: u64,
    ) -> f64 {
        // The session's `input_tokens` includes what it read back from the
        // prompt cache (the OpenAI convention), so `cached_tokens` goes in the
        // subset slot. This surface reports no cache writes and none of the
        // separate read or write counters.
        let input = crate::usage_attr::input_tokens_for_pricing(input, cached, 0, 0, 0);
        self.state
            .pricing
            .for_snapshot(snap)
            .resolve(&self.model_entry.value)
            .map(|c| c.calculate_with_cache(input, output))
            .unwrap_or(0.0)
    }

    /// Charge what arrived since the last call, at the price in force when it
    /// arrived: a price that changes mid-session applies from then on, not to
    /// what was already charged. The metric and the limits get the difference
    /// from the running total, so what they add up to is exactly what the
    /// session's one usage event reports.
    fn settle(&mut self, usage: &SessionUsage) {
        if !self.behind(usage) {
            return;
        }
        let snap = self.state.snapshot.load();
        let pk = crate::usage_attr::ResolvedPk::resolve(&snap, self.pk_id);
        let input = usage.input_tokens.saturating_sub(self.input_tokens);
        let output = usage.output_tokens.saturating_sub(self.output_tokens);
        let cached = usage.cached_tokens.saturating_sub(self.cached_tokens);
        (self.input_tokens, self.output_tokens, self.cached_tokens) =
            (usage.input_tokens, usage.output_tokens, usage.cached_tokens);
        self.cost_usd += self.price_usd(&snap, input, cached, output);
        let micro_usd = crate::usage_attr::spend_micro_usd(self.cost_usd);
        let spend = micro_usd.saturating_sub(self.micro_usd);
        self.micro_usd = self.micro_usd.max(micro_usd);
        self.responses = usage.responses;

        let limiter = &self.state.limiter;
        limiter.add_tokens_post_stream_all(&self.token_keys, input + output);
        limiter.add_tokens_post_stream_all(&self.spend_keys, spend);
        // A realtime session bills real tokens against a real model. Its cost
        // is resolved from the model row, same as every other endpoint, and
        // feeds `aisix_llm_spend_micro_usd_total`.
        crate::request_metrics::record_usage(
            self.state,
            "/v1/realtime",
            crate::request_metrics::Caller::new(self.auth),
            crate::request_metrics::Upstream {
                provider: self.provider_label,
                // The requested string so the emit chokepoint folds a
                // wildcard-served alias's pair to the row's identities
                // (non-wildcard: requested == display_name, unchanged).
                model: self.requested_model,
                upstream_model: self.model_entry.value.upstream_model().unwrap_or("unknown"),
                pk: pk.labels(),
                ..Default::default()
            },
            crate::request_metrics::Tokens {
                input: input.min(u32::MAX as u64) as u32,
                output: output.min(u32::MAX as u64) as u32,
                total: (input + output).min(u32::MAX as u64) as u32,
                // The Realtime session reports its cache hits in the
                // OpenAI shape — inside `input_tokens`, never beside it.
                cached: cached.min(u32::MAX as u64) as u32,
                cache_read: 0,
                cache_creation: 0,
                spend_usd: spend as f64 / 1_000_000.0,
                client_type: self.client_type,
            },
        );
    }

    /// Why this session may not go on, read off the current config: the key that
    /// opened it was disabled, deleted, rotated or has expired, it may no longer
    /// use the model, or a budget it or its provider key answers to ran out.
    /// Admission only looked once, and a session can run for hours.
    fn refusal(&self) -> Option<ProxyError> {
        let snap = self.state.snapshot.load();
        match snap.apikeys.get_by_id(&self.auth.entry.id) {
            Some(key) if key.value.is_expired_at(chrono::Utc::now()) => {
                return Some(ProxyError::ApiKeyExpired)
            }
            // A rotated credential is a revoked one for the session it opened.
            Some(key)
                if key.value.disabled || key.value.key_hash != self.auth.entry.value.key_hash =>
            {
                return Some(ProxyError::ApiKeyDisabled)
            }
            Some(key) if !key.value.can_access(&snap, self.requested_model) => {
                return Some(ProxyError::ModelForbidden(self.requested_model.to_string()))
            }
            Some(_) => {}
            None => return Some(ProxyError::ApiKeyDisabled),
        }
        crate::budget::enforce_key(self.state, &snap, self.auth)
            .and_then(|()| crate::budget::gate(self.state, &snap).check_provider_key(self.pk_id))
            .err()
    }
}

async fn run_session(
    state: ProxyState,
    prep: Prepared,
    client_ws: WebSocket,
    client: ClientContext,
    request_id: String,
    started: Instant,
) {
    let Prepared {
        auth,
        model_entry,
        pk_id,
        upstream_request,
        reservation,
        requested_model,
        upstream_model,
        provider_label,
    } = prep;

    let guardrail_ctx = aisix_guardrails::RequestContext {
        passthrough_route_id: "",
        model_id: &model_entry.id,
        mcp_server_id: "",
        api_key_id: &auth.entry.id,
        team_id: auth.key().team_id.as_deref(),
    };
    let chain = state.guardrail_index.resolve(&guardrail_ctx);
    // Read back on every terminal emit below — the session's own event and
    // the upstream-connect failure (AISIX-Cloud#1330 / #1024).
    let audit = chain.audit_log();

    let (client_tx, mut client_rx) = client_ws.split();

    // The upstream wait of a session is everything from the dial on, so
    // the gateway's own share is the time before it (`gateway_overhead_ms`).
    // Measured here because the session runs detached from the request's
    // attribution scope that measures every other family.
    let dial_started = Instant::now();
    let mut upstream_response_headers = std::collections::BTreeMap::new();
    // Stream idle deadline, resolved model → `upstream.stream_timeout_ms`
    // / `timeout_ms`. Realtime sessions are long-lived by design, so the
    // deployment default (6000 s) only reaps sessions with no traffic in
    // either direction for that long; `timeout: 0` on the model lifts it.
    // It is also the bound on reading a refused upgrade's body, as a
    // streaming HTTP route's error body has its stream budget.
    let idle_cap =
        crate::routing::effective_timeouts(&model_entry.value, None, state.default_timeouts).stream;
    let mut client_tx = ClientSink::new(client_tx, idle_cap);
    let (upstream, upgrade_request_id) = match connect_upstream(upstream_request).await {
        Ok((ws, resp)) => {
            let upgrade_request_id = crate::usage_attr::sanitize_provider_response_id(
                &aisix_gateway::provider_request_id(resp.headers()),
            );
            let names = crate::client_ip::exported_header_names(&state.snapshot.load(), true);
            if !names.is_empty() {
                upstream_response_headers =
                    aisix_gateway::capture_header_values(resp.headers(), &names);
            }
            (ws, upgrade_request_id)
        }
        Err(failure) => {
            tracing::warn!(error = %failure.error, model = %requested_model, "realtime upstream connect failed");
            let client_gone = async {
                while let Some(Ok(message)) = client_rx.next().await {
                    if matches!(message, AxMessage::Close(_)) {
                        break;
                    }
                }
            };
            let (connect_err, refused) = connect_failure(
                failure,
                dial_started.elapsed(),
                idle_cap,
                &request_id,
                client_gone,
            )
            .await;
            let provider_request_id = refused
                .as_ref()
                .map(|r| {
                    crate::usage_attr::sanitize_provider_response_id(
                        &aisix_gateway::provider_request_id(&r.headers),
                    )
                })
                .unwrap_or_default();
            let status = connect_err.http_status();
            crate::attempt::RoutingTelemetry::record_deployment(&state, &model_entry.id, status);
            // `note_failure` hands the error back, so the same value that
            // drove the cooldown decision also names the failure to the
            // caller and in the access log instead of being rebuilt.
            let connect_err = ProxyError::Bridge(crate::cooldown::note_failure(
                &state.runtime_status,
                &model_entry.id,
                model_entry.value.cooldown.as_ref(),
                connect_err,
            ));
            let _ = client_tx
                .send(AxMessage::Text(connect_failure_frame(&connect_err)))
                .await;
            let _ = client_tx
                .send(AxMessage::Close(Some(CloseFrame {
                    code: 1011,
                    reason: "upstream connect failed".into(),
                })))
                .await;
            emit_access_log(
                &Method::GET,
                status,
                started.elapsed(),
                &request_id,
                Some(&auth.entry.id),
                Some((&provider_label, &requested_model)),
                Some(&connect_err),
                None,
                Some(&provider_request_id)
                    .filter(|id| !id.is_empty())
                    .map(String::as_str),
            );
            // One load shared by the ProviderKey resolution below and the
            // usage event, like the session's own terminal path (#941) —
            // and, as there, not by `request_metrics::record`, which takes
            // its own for the model-label collapse.
            let snap = state.snapshot.load();
            // Count the failure like the session that did open, and like
            // every pre-dispatch rejection above — logs and the
            // request-rate metrics must not disagree about whether these
            // requests exist. Attribution is fully resolved here: `prepare`
            // has already picked the model and the ProviderKey, so this
            // carries the same labels a successful session would, not the
            // `unknown` placeholders of a path that never selected a
            // target.
            let pk = crate::usage_attr::ResolvedPk::resolve(&snap, &pk_id);
            crate::request_metrics::record(
                &state,
                "/v1/realtime",
                crate::request_metrics::Caller::new(&auth),
                crate::request_metrics::Upstream {
                    provider: &provider_label,
                    // The requested string, as in the session's own emit below.
                    model: &requested_model,
                    upstream_model: model_entry
                        .value
                        .upstream_model()
                        .unwrap_or(crate::request_metrics::UNKNOWN),
                    pk: pk.labels(),
                    ..Default::default()
                },
                status,
                aisix_obs::ErrorSource::Upstream,
                started.elapsed(),
            );
            let mut event = crate::usage_attr::build_error_usage_event(
                "realtime",
                &request_id,
                &requested_model,
                &auth.entry.id,
                status,
                crate::usage_attr::EventError::of(&connect_err),
                // Failing to open the upstream socket is not a guardrail
                // decision, whatever the chain went on to allow.
                /* guardrail_blocked */
                false,
                &client,
                crate::usage_attr::applied_guardrails(&audit),
                crate::usage_attr::enforced_hits(&audit),
                crate::usage_attr::guardrail_scores(&audit),
                crate::usage_attr::bypass_reason(&audit),
            );
            // Same reason as the session-end event: no attribution cell here.
            // Realtime serves only direct models, so the entry is the target.
            event.model_id = model_entry.id.clone();
            event.requested_model_id = model_entry.id.clone();
            event.provider_request_id = provider_request_id;
            let model = crate::usage_attr::usage_event_model_label(&snap, &requested_model);
            let unresolved_pk = crate::usage_attr::ResolvedPk::unresolved();
            let labels = crate::usage_attr::usage_event_labels(&model, &unresolved_pk);
            if let Some(refused) = &refused {
                let names = crate::client_ip::exported_header_names(&snap, true);
                if !names.is_empty() {
                    event.upstream_response_headers =
                        aisix_gateway::capture_header_values(&refused.headers, &names);
                }
                event.upstream_latency_ms = crate::attempt::measured_ms(dial_started.elapsed());
                let content = upstream_failure_content(&snap, &refused.body);
                crate::usage_attr::emit_usage(
                    &state,
                    &snap,
                    crate::operation::REALTIME,
                    event,
                    labels,
                    content.as_ref(),
                    client.trace.as_ref(),
                    /* terminal */ true,
                    /* dispatched */ true,
                );
                return;
            }
            crate::usage_attr::emit_prepared_usage_event(
                &state,
                &snap,
                crate::operation::REALTIME,
                event,
                labels,
                client.trace.as_ref(),
            );
            return;
        }
    };
    let (mut up_tx, mut up_rx) = upstream.split();

    let client_type = state.client_classifier.classify(&client.user_agent);
    let mut meter = SessionMeter {
        state: &state,
        auth: &auth,
        model_entry: &model_entry,
        pk_id: &pk_id,
        provider_label: &provider_label,
        requested_model: &requested_model,
        client_type,
        token_keys: reservation.token_keys(),
        spend_keys: reservation.spend_keys(),
        input_tokens: 0,
        output_tokens: 0,
        cached_tokens: 0,
        micro_usd: 0,
        cost_usd: 0.0,
        responses: 0,
    };
    let mut usage = SessionUsage::default();
    let mut monitor_hits: Vec<aisix_core::GuardrailMonitorHit> = Vec::new();
    let mut close_status: u16 = 200;
    // Paired with `close_status`: every branch that sets a FAILING status
    // also names the failure, so the access log can say why a session
    // ended (#1093).
    //
    // A client-side transport error (`Some(Err(_))` on the receive half)
    // is deliberately not one of them: it keeps `close_status` 200 and
    // stays `None`. Reclassifying it is a behaviour change, not a logging
    // one — `RequestOutcome::from_status` would flip that session from
    // `success` to `client_error` and move every operator's realtime
    // success rate. That belongs with the termination-reason taxonomy
    // (`downstream_remote_disconnect` and friends) the issue asks for
    // separately, which needs its own status decision.
    let mut session_error: Option<ProxyError> = None;
    let mut opening = SessionOpening::default();
    let mut held_output = aisix_guardrails::Guardrail::runs_on_output(&chain)
        .then(|| aisix_guardrails::Guardrail::stream_output_policy(&chain).hold_cap())
        .flatten()
        .map(|(max_buffer_bytes, fail_open)| HeldOutput::new(max_buffer_bytes, fail_open));

    loop {
        let next = async {
            tokio::select! {
                m = client_rx.next() => Dir::FromClient(m),
                m = up_rx.next() => Dir::FromUpstream(m),
            }
        };
        let event = match idle_cap {
            Some(cap) => match tokio::time::timeout(cap, next).await {
                Ok(r) => r,
                Err(_) => {
                    let _ = client_tx
                        .send(AxMessage::Close(Some(CloseFrame {
                            code: 1001,
                            reason: "idle timeout".into(),
                        })))
                        .await;
                    close_status = 504;
                    session_error = Some(ProxyError::Bridge(BridgeError::Timeout {
                        elapsed_ms: cap.as_millis() as u64,
                        cause: "no realtime frame within the stream idle budget".into(),
                    }));
                    break;
                }
            },
            None => next.await,
        };

        match event {
            Dir::FromClient(m) => match m {
                Some(Ok(AxMessage::Text(text))) => {
                    if !chain.is_empty() {
                        if let Some(resp) = guardrail_block_event(
                            &chain,
                            &model_entry.value.display_name,
                            &text,
                            true,
                            &mut monitor_hits,
                        )
                        .await
                        {
                            let (resp, refusal) = resp;
                            let _ = client_tx.send(AxMessage::Text(resp)).await;
                            let _ = client_tx
                                .send(AxMessage::Close(Some(CloseFrame {
                                    code: 1011,
                                    reason: "content policy".into(),
                                })))
                                .await;
                            close_status = 400;
                            session_error = Some(refusal);
                            break;
                        }
                    }
                    let text = restamp_session_model_in(text, &requested_model, &upstream_model);
                    if up_tx.send(TgMessage::Text(text)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(AxMessage::Binary(b))) => {
                    if let Some((resp, refusal)) = binary_frame_refusal(&chain, true) {
                        let _ = client_tx.send(AxMessage::Text(resp)).await;
                        let _ = client_tx
                            .send(AxMessage::Close(Some(CloseFrame {
                                code: 1011,
                                reason: "content policy".into(),
                            })))
                            .await;
                        close_status = 400;
                        session_error = Some(refusal);
                        break;
                    }
                    if up_tx.send(TgMessage::Binary(b)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(AxMessage::Close(frame))) => {
                    // The transport has already queued its reply (the
                    // client's code, or the library's 1002 for one that may
                    // not be sent); it goes out on the flush, and without
                    // one the client sees 1006.
                    let _ = client_tx.close().await;
                    // A client code that may not be sent arrives as the
                    // library's substitute; the upstream gets 1011, not a
                    // 1002 the client never sent.
                    let frame = frame.map(|f| {
                        if (f.code, f.reason.as_ref()) == LIBRARY_REJECTED_CLOSE {
                            TgCloseFrame {
                                code: 1011.into(),
                                reason: "".into(),
                            }
                        } else {
                            TgCloseFrame {
                                code: f.code.into(),
                                reason: f.reason.to_string().into(),
                            }
                        }
                    });
                    close_upstream(&mut up_tx, &mut up_rx, frame, |text| {
                        usage.absorb(text);
                        opening.observe(&state, &model_entry.id, Some(text));
                    })
                    .await;
                    break;
                }
                Some(Ok(_)) => {} // ping/pong handled by the transports
                Some(Err(_)) | None => {
                    close_upstream(&mut up_tx, &mut up_rx, None, |text| {
                        usage.absorb(text);
                        opening.observe(&state, &model_entry.id, Some(text));
                    })
                    .await;
                    break;
                }
            },
            Dir::FromUpstream(m) => match m {
                Some(Ok(TgMessage::Text(text))) => {
                    usage.absorb(&text);
                    let usage_arrived = meter.behind(&usage);
                    meter.settle(&usage);
                    opening.observe(&state, &model_entry.id, Some(&text));
                    let mut refused = None;
                    if !chain.is_empty() {
                        refused = guardrail_block_event(
                            &chain,
                            &model_entry.value.display_name,
                            &text,
                            false,
                            &mut monitor_hits,
                        )
                        .await;
                    }
                    let step = match held_output.as_mut().filter(|_| refused.is_none()) {
                        Some(held) => held.observe(text),
                        None => OutputStep::release(text),
                    };
                    let (scan, frames) = match step {
                        OutputStep::Held => continue,
                        OutputStep::Release { scan, frames } => (scan, frames),
                        OutputStep::Overflow { fail_open, frames } => {
                            if fail_open {
                                // The rest of this response goes out unscanned, as
                                // on the other routes; the next one is held.
                                crate::guardrail_stream::release_output_buffer_unscanned(&chain);
                            } else {
                                aisix_guardrails::Guardrail::record_output_buffer_exceeded(&chain);
                                let refusal = crate::error::guardrail_block_error(
                                    "response",
                                    None,
                                    Some(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED),
                                );
                                refused = Some((content_filtered_event(&refusal), refusal));
                            }
                            (Vec::new(), frames)
                        }
                    };
                    // What was held is judged whole before any of it goes out.
                    for text in &scan {
                        if refused.is_some() {
                            break;
                        }
                        refused = guardrail_block_event(
                            &chain,
                            &model_entry.value.display_name,
                            text,
                            false,
                            &mut monitor_hits,
                        )
                        .await;
                    }
                    if let Some((resp, refusal)) = refused {
                        let _ = client_tx.send(AxMessage::Text(resp)).await;
                        let _ = client_tx
                            .send(AxMessage::Close(Some(CloseFrame {
                                code: 1011,
                                reason: "content policy".into(),
                            })))
                            .await;
                        close_status = 400;
                        session_error = Some(refusal);
                        break;
                    }
                    let mut client_gone = false;
                    for text in frames {
                        let text = restamp_session_model_out(text, &requested_model);
                        let text = without_sent_credentials(text);
                        if !client_tx.send(AxMessage::Text(text)).await {
                            client_gone = true;
                            break;
                        }
                    }
                    if client_gone {
                        break;
                    }
                    // Between responses, so what the caller paid for is
                    // delivered first.
                    if let Some(refusal) = usage_arrived.then(|| meter.refusal()).flatten() {
                        let _ = client_tx
                            .send(AxMessage::Close(Some(CloseFrame {
                                code: 1008,
                                reason: "no longer authorized".into(),
                            })))
                            .await;
                        close_status = refusal.status().as_u16();
                        session_error = Some(refusal);
                        break;
                    }
                }
                Some(Ok(TgMessage::Binary(b))) => {
                    opening.observe(&state, &model_entry.id, None);
                    if let Some((resp, refusal)) = binary_frame_refusal(&chain, false) {
                        let _ = client_tx.send(AxMessage::Text(resp)).await;
                        let _ = client_tx
                            .send(AxMessage::Close(Some(CloseFrame {
                                code: 1011,
                                reason: "content policy".into(),
                            })))
                            .await;
                        close_status = 400;
                        session_error = Some(refusal);
                        break;
                    }
                    if !client_tx.send(AxMessage::Binary(b)).await {
                        break;
                    }
                }
                Some(Ok(TgMessage::Close(frame))) => {
                    let end = match &frame {
                        Some(f) => UpstreamEnd::Close(f.code.into(), &f.reason),
                        None => UpstreamEnd::NoStatus,
                    };
                    close_client_after_upstream(&mut client_tx, end, opening.refused()).await;
                    break;
                }
                Some(Ok(_)) => {} // ping/pong/raw frames
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "realtime upstream stream error");
                    let error = aisix_gateway::error_with_causes(&e);
                    close_client_after_upstream(
                        &mut client_tx,
                        UpstreamEnd::Failed(&error),
                        opening.refused(),
                    )
                    .await;
                    if !opening.refused() {
                        close_status = 502;
                        session_error = Some(ProxyError::Bridge(BridgeError::Transport(error)));
                    }
                    break;
                }
                None => {
                    close_client_after_upstream(
                        &mut client_tx,
                        UpstreamEnd::Dropped,
                        opening.refused(),
                    )
                    .await;
                    break;
                }
            },
        }
    }

    // Every other way out of the loop leaves the upstream half open: send its
    // Close and read what it sends back, as the client's own close does, so a
    // `response.done` already on its way is still billed. Where the upstream
    // already closed, the send fails and this returns at once; the flush below
    // then sends the reply to the Close it sent.
    close_upstream(&mut up_tx, &mut up_rx, None, |text| usage.absorb(text)).await;
    let _ = tokio::time::timeout(UPSTREAM_CLOSE_ACK, up_tx.close()).await;

    // Whoever hung up first, a session that only ever reported an error failed.
    // Anything that already named the session's end (a guardrail, the idle
    // deadline) keeps it.
    let refusal = if session_error.is_none() {
        opening.refusal.take()
    } else {
        None
    };
    let mut refusal_frame = None;
    if let Some(refusal) = refusal {
        close_status = refusal.error.http_status();
        crate::attempt::RoutingTelemetry::record_deployment(&state, &model_entry.id, close_status);
        session_error = Some(ProxyError::Bridge(crate::cooldown::note_failure(
            &state.runtime_status,
            &model_entry.id,
            model_entry.value.cooldown.as_ref(),
            refusal.error,
        )));
        refusal_frame = Some(refusal.frame);
    } else if !opening.usable {
        // No frame took this sample. A session that an upstream failure ended
        // says so; otherwise the upgrade itself was the success.
        let upstream_failed = session_error
            .as_ref()
            .is_some_and(|e| crate::attempt::error_source(e) == aisix_obs::ErrorSource::Upstream);
        let status = if upstream_failed { close_status } else { 200 };
        crate::attempt::RoutingTelemetry::record_deployment(&state, &model_entry.id, status);
    }
    let provider_request_id = if refusal_frame.is_some() {
        upgrade_request_id
    } else {
        String::new()
    };

    let elapsed = started.elapsed();
    // A realtime session can run for minutes, so its terminal emits read a
    // FRESH snapshot rather than the one `prepare` resolved against (#941) —
    // one ProviderKey lookup shared by the request metric and the usage
    // event, where each used to do its own. The load
    // itself is shared by everything here except `request_metrics::record`,
    // which takes its own for the model-label collapse.
    let snap = state.snapshot.load();
    let pk = crate::usage_attr::ResolvedPk::resolve(&snap, &pk_id);
    // What the last frames brought, then the session's whole cost: one figure,
    // the same one the event reports and the charges along the way add up to.
    meter.settle(&usage);
    let cost_usd = meter.cost_usd;
    reservation.commit_tokens_no_spend(0).await;

    emit_access_log(
        &Method::GET,
        close_status,
        elapsed,
        &request_id,
        Some(&auth.entry.id),
        Some((&provider_label, &requested_model)),
        session_error.as_ref(),
        Some((usage.input_tokens, usage.output_tokens)),
        Some(provider_request_id.as_str()).filter(|id| !id.is_empty()),
    );
    crate::request_metrics::record(
        &state,
        "/v1/realtime",
        crate::request_metrics::Caller::new(&auth),
        crate::request_metrics::Upstream {
            provider: &provider_label,
            // The requested string so the emit chokepoint folds a
            // wildcard-served alias's pair to the row's identities (#451).
            model: &requested_model,
            // AISIX-Cloud#1325: a session that ends on an upstream error
            // still names the key it was talking to — these two used to be
            // `unknown` on every realtime sample, success included.
            upstream_model: model_entry.value.upstream_model().unwrap_or("unknown"),
            pk: pk.labels(),
            ..Default::default()
        },
        close_status,
        session_error
            .as_ref()
            .map_or(aisix_obs::ErrorSource::None, crate::attempt::error_source),
        elapsed,
    );
    let mut event = UsageEvent {
        request_id: request_id.clone(),
        occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
        model_id: model_entry.id.clone(),
        api_key_id: auth.entry.id.clone(),
        requested_model: requested_model.clone(),
        // The session runs outside the request scope, so the emit
        // chokepoint cannot read it off the attribution cell; realtime
        // serves direct models only, which address themselves.
        requested_model_id: model_entry.id.clone(),
        provider_request_id,
        prompt_tokens: usage.input_tokens.min(u32::MAX as u64) as u32,
        completion_tokens: usage.output_tokens.min(u32::MAX as u64) as u32,
        total_tokens: usage.upstream_total.recorded(),
        cached_prompt_tokens: usage.cached_tokens.min(u32::MAX as u64) as u32,
        modality_tokens: usage.modality,
        status_code: close_status,
        // Single-attempt endpoint: the attempt spans the whole request, so
        // the upstream figure and what the caller waited for coincide.
        upstream_latency_ms: crate::attempt::measured_ms(elapsed),
        downstream_latency_ms: crate::attempt::measured_ms(elapsed),
        gateway_overhead_ms: Some(
            dial_started
                .saturating_duration_since(started)
                .as_millis()
                .min(u32::MAX as u128) as u32,
        ),
        upstream_response_headers,
        cost_usd,
        inbound_protocol: "realtime".to_string(),
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        request_headers: client.request_headers.clone(),
        // A frame the chain refused ends the session, so the session's one
        // terminal event is where the refusal has to be recorded — this is
        // the only realtime row the "Guardrail blocks" view can ever see
        // (AISIX-Cloud#1428).
        guardrail_blocked: session_error
            .as_ref()
            .is_some_and(ProxyError::is_guardrail_block),
        // Whatever ended the session names itself, as on every other
        // surface and as the session's access-log line does: a guardrail
        // refusal, or the upstream failure or timeout that cut it off.
        error_class: session_error
            .as_ref()
            .map(|e| crate::attempt::error_class(e).to_string())
            .unwrap_or_default(),
        error_message: session_error
            .as_ref()
            .map(crate::attempt::gateway_error_message)
            .unwrap_or_default(),
        guardrail_monitor_hits: monitor_hits,
        applied_guardrails: crate::usage_attr::applied_guardrails(&audit),
        guardrail_enforced_hits: crate::usage_attr::enforced_hits(&audit),
        guardrail_scores: crate::usage_attr::guardrail_scores(&audit),
        guardrail_bypassed_reason: crate::usage_attr::bypass_reason(&audit),
        ..Default::default()
    };
    crate::usage_attr::apply_pk_telemetry(&mut event, &pk);
    crate::usage_attr::apply_caller_identity(&mut event, auth.jwt.as_ref(), &auth.caller());
    let usage_model =
        crate::usage_attr::usage_event_model_label(&snap, &event.requested_model).into_owned();
    let content = refusal_frame
        .as_deref()
        .and_then(|frame| upstream_failure_content(&snap, frame.as_bytes()));
    crate::usage_attr::emit_usage(
        &state,
        &snap,
        crate::operation::REALTIME,
        event.clone(),
        crate::usage_attr::usage_event_labels(&usage_model, &pk),
        content.as_ref(),
        client.trace.as_ref(),
        /* terminal */ true,
        /* dispatched */ true,
    );
}

/// The response column of a failure row, under a full-content export only.
fn upstream_failure_content(
    snap: &aisix_core::AisixSnapshot,
    body: &[u8],
) -> Option<aisix_obs::CapturedContent> {
    if body.is_empty() {
        return None;
    }
    let cap = crate::usage_attr::content_cap(snap)?;
    Some(aisix_obs::CapturedContent::new(
        "",
        &String::from_utf8_lossy(body),
        cap as usize,
    ))
}

/// An upstream frame on its way to the client, less the credentials this
/// session sent when it is an error ([`aisix_gateway::scrub_upstream_error_frame`]).
fn without_sent_credentials(text: String) -> String {
    match aisix_gateway::scrub_upstream_error_frame(text.as_bytes()) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(scrubbed) => String::from_utf8_lossy(&scrubbed).into_owned(),
    }
}

/// An upstream that completes the upgrade and then reports an error instead of
/// opening a session, which is how OpenAI answers a wrong key, a model it does
/// not serve, or the beta header.
struct SessionRefusal {
    error: BridgeError,
    frame: String,
}

/// How far a session has opened. It is usable from the first frame that is not
/// an `error` event: OpenAI's is `session.created`, but a compatible server need
/// not open with that, and an error after it is one the session survives.
#[derive(Default)]
struct SessionOpening {
    usable: bool,
    refusal: Option<SessionRefusal>,
}

impl SessionOpening {
    /// Note one upstream frame, `text` being `None` for a binary one. The first
    /// usable frame is what counts the upgrade as a deployment success.
    fn observe(&mut self, state: &ProxyState, model_id: &str, text: Option<&str>) {
        if self.usable {
            return;
        }
        match text.and_then(session_refusal_error) {
            Some(error) => {
                if self.refusal.is_none() {
                    self.refusal = Some(SessionRefusal {
                        error,
                        frame: without_sent_credentials(text.unwrap_or_default().to_string()),
                    });
                }
            }
            None => {
                self.usable = true;
                self.refusal = None;
                // A completed upgrade answers `101`, which is a success; the
                // deployment families classify by HTTP outcome, so it counts
                // as a `200`.
                crate::attempt::RoutingTelemetry::record_deployment(state, model_id, 200);
            }
        }
    }

    fn refused(&self) -> bool {
        self.refusal.is_some()
    }
}

/// The failure an `error` event reports. No HTTP status comes with it, so the
/// status is read off OpenAI's event vocabulary, the same `type` and `code` its
/// HTTP errors use; anything else has none and renders as a 502.
fn session_refusal_error(text: &str) -> Option<BridgeError> {
    let event = serde_json::from_str::<Value>(text).ok()?;
    if event.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let mut error = crate::responses::responses_in_band_error(&event)?;
    if let BridgeError::UpstreamInBand { status, parsed, .. } = &mut error {
        let view = parsed.as_deref();
        let code = view.and_then(|v| v.code.as_deref());
        let kind = view.and_then(|v| v.kind.as_deref());
        *status = status.or(match (code, kind) {
            (Some("invalid_api_key"), _) => Some(401),
            (Some("invalid_model" | "model_not_found"), _) => Some(404),
            (Some("rate_limit_exceeded" | "insufficient_quota"), _) => Some(429),
            (_, Some("invalid_request_error")) => Some(400),
            _ => None,
        });
    }
    Some(error)
}

/// How long a session that the client closed waits for the upstream to
/// acknowledge its own close before dropping the connection.
const UPSTREAM_CLOSE_ACK: Duration = Duration::from_secs(2);

/// Close the upstream socket and wait, briefly, for its acknowledgement,
/// so the upstream sees a completed handshake rather than a reset. A text frame
/// that arrives meanwhile (a response finishing) still goes to `on_text`.
async fn close_upstream<T, R>(
    up_tx: &mut T,
    up_rx: &mut R,
    frame: Option<TgCloseFrame<'static>>,
    mut on_text: impl FnMut(&str),
) where
    T: futures::Sink<TgMessage> + Unpin,
    R: futures::Stream<Item = Result<TgMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    if up_tx.send(TgMessage::Close(frame)).await.is_err() {
        return;
    }
    let _ = tokio::time::timeout(UPSTREAM_CLOSE_ACK, async {
        while let Some(Ok(m)) = up_rx.next().await {
            match m {
                TgMessage::Text(text) => on_text(&text),
                TgMessage::Close(_) => break,
                _ => {}
            }
        }
    })
    .await;
}

/// Close codes an endpoint may put in a Close frame (RFC 6455 §7.4.1 and
/// the IANA registry), plus the 3000–4999 application range.
fn sendable_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

/// The longest reason a Close frame carries: 125 payload bytes minus the
/// two-byte code.
const CLOSE_REASON_MAX_BYTES: usize = 123;

/// How the upstream's half of a session ended.
enum UpstreamEnd<'a> {
    /// A Close frame with a status code and reason.
    Close(u16, &'a str),
    /// A Close frame without a status code.
    NoStatus,
    /// A transport or protocol error, with its description.
    Failed(&'a str),
    /// The stream ended without a Close frame.
    Dropped,
}

/// The longest a send to the client may take: the session's idle budget, and no
/// more than this even where the model lifts that budget.
const CLIENT_SEND_MAX: Duration = Duration::from_secs(60);

/// The client's half of a session, with every send bounded. A client that stops
/// reading would otherwise hold a send pending for as long as its socket stays
/// up: the loop never returns to its idle deadline, and the session keeps its
/// concurrency slot, its upstream socket and the shutdown drain. One that
/// accepts none of a frame within the bound has stopped reading, and is treated
/// as gone, so later sends do not wait for it again.
struct ClientSink<S> {
    tx: S,
    bound: Duration,
    stuck: bool,
}

impl<S> ClientSink<S>
where
    S: futures::Sink<AxMessage> + Unpin,
{
    fn new(tx: S, idle_cap: Option<Duration>) -> Self {
        Self {
            tx,
            bound: idle_cap.map_or(CLIENT_SEND_MAX, |cap| cap.min(CLIENT_SEND_MAX)),
            stuck: false,
        }
    }

    /// Whether `msg` reached the client.
    async fn send(&mut self, msg: AxMessage) -> bool {
        if self.stuck {
            return false;
        }
        match tokio::time::timeout(self.bound, self.tx.send(msg)).await {
            Ok(sent) => sent.is_ok(),
            Err(_) => {
                self.stuck = true;
                false
            }
        }
    }

    async fn close(&mut self) {
        if !self.stuck {
            let _ = tokio::time::timeout(self.bound, self.tx.close()).await;
        }
    }
}

/// tungstenite (under both the upstream client and axum's server side)
/// replaces a received Close whose code may not be sent (1004, 1016–2999,
/// …) with exactly this frame and drops the original code, so it is
/// indistinguishable from the peer sending it itself.
const LIBRARY_REJECTED_CLOSE: (u16, &str) = (1002, "Protocol violation");

/// Relay the upstream's end of the session to the client: its code and
/// reason when the upstream closed with one that may be sent, 1011
/// otherwise. An end other than a normal close is preceded by a Realtime
/// `error` event describing it, since many clients surface only events, unless
/// the upstream's own error event already told the client (`refused`).
async fn close_client_after_upstream<S>(
    client_tx: &mut ClientSink<S>,
    end: UpstreamEnd<'_>,
    refused: bool,
) where
    S: futures::Sink<AxMessage> + Unpin,
{
    let (code, reason, message) = match end {
        UpstreamEnd::Close(1000, reason) => (1000, reason, None),
        UpstreamEnd::Close(code, reason) if (code, reason) == LIBRARY_REJECTED_CLOSE => (
            1011,
            "",
            Some(
                "upstream websocket sent a close frame the gateway cannot relay: an invalid \
                 close code, or 1002 \"Protocol violation\""
                    .to_string(),
            ),
        ),
        UpstreamEnd::Close(code, reason) => (
            if sendable_close_code(code) {
                code
            } else {
                1011
            },
            reason,
            Some(if reason.is_empty() {
                format!("upstream websocket closed with code {code}")
            } else {
                format!("upstream websocket closed with code {code}: {reason}")
            }),
        ),
        UpstreamEnd::NoStatus => (
            1011,
            "",
            Some("upstream websocket closed without a status code".to_string()),
        ),
        UpstreamEnd::Failed(error) => (
            1011,
            "upstream error",
            Some(format!("upstream websocket connection failed: {error}")),
        ),
        UpstreamEnd::Dropped => (
            1011,
            "",
            Some("upstream websocket connection ended without a close frame".to_string()),
        ),
    };
    if let Some(message) = message.filter(|_| !refused) {
        let event = serde_json::json!({
            "type": "error",
            "error": { "type": "server_error", "message": message }
        });
        let _ = client_tx.send(AxMessage::Text(event.to_string())).await;
    }
    let mut cut = reason.len().min(CLOSE_REASON_MAX_BYTES);
    while !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    let _ = client_tx
        .send(AxMessage::Close(Some(CloseFrame {
            code,
            reason: reason[..cut].to_string().into(),
        })))
        .await;
}

enum Dir {
    FromClient(Option<Result<AxMessage, axum::Error>>),
    FromUpstream(Option<Result<TgMessage, tokio_tungstenite::tungstenite::Error>>),
}

/// Whole-frame guardrail scan (the `/passthrough` blob precedent applied
/// per WS text frame). Returns the client-facing error event on Block,
/// with the refusal it reports.
async fn guardrail_block_event(
    chain: &aisix_guardrails::GuardrailChain,
    model_name: &str,
    text: &str,
    input_side: bool,
    monitor_hits: &mut Vec<aisix_core::GuardrailMonitorHit>,
) -> Option<(String, ProxyError)> {
    let text = frame_scan_text(text);
    let (verdict, hits) = if input_side {
        let chat = aisix_gateway::ChatFormat::new(
            model_name,
            vec![aisix_gateway::ChatMessage::user(text)],
        );
        aisix_guardrails::Guardrail::check_input_unmaskable_observed(chain, &chat).await
    } else {
        let synth = aisix_gateway::ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: String::new(),
            model: model_name.to_string(),
            message: aisix_gateway::ChatMessage::assistant(text),
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::default(),
            service_tier: None,
        };
        aisix_guardrails::Guardrail::check_output_unmaskable_observed(chain, &synth).await
    };
    monitor_hits.extend(hits);
    if let aisix_guardrails::GuardrailVerdict::Block {
        reason,
        guardrail_name,
        unavailable,
    } = verdict
    {
        let side = if input_side { "input" } else { "output" };
        tracing::warn!(
            guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
            guardrail_hook = side,
            reason = %reason,
            "guardrail blocked realtime frame",
        );
        let refusal = crate::error::guardrail_block_error(
            if input_side { "request" } else { "response" },
            guardrail_name.as_deref(),
            unavailable.as_deref(),
        );
        return Some((content_filtered_event(&refusal), refusal));
    }
    None
}

fn content_filtered_event(refusal: &ProxyError) -> String {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "code": "content_filtered",
            "message": refusal.to_string(),
        }
    })
    .to_string()
}

/// What a chain reads of a text frame. A peer decodes the JSON, so an escape
/// hides a word from the frame's bytes: where there is one, the decoded strings
/// are read beside them. A frame with no backslash has no escape, and reading
/// its bytes alone keeps an audio frame's cost where it was.
///
/// A frame with an escape that does not parse here may still parse for a peer
/// (a `NaN`, a lone `\ud800`, nesting past this parser's depth limit), so its
/// string literals are read as a lenient parser reads them
/// ([`crate::json_splice::lenient_string_literals`]).
fn frame_scan_text(frame: &str) -> String {
    if !frame.contains('\\') {
        return frame.to_owned();
    }
    match serde_json::from_str::<serde_json::Value>(frame) {
        Ok(value) => {
            let mut decoded = Vec::new();
            crate::jobs::json_decoded_strings(&value, &mut decoded);
            format!("{frame}\n{}", decoded.join("\n"))
        }
        Err(_) => format!(
            "{frame}{}",
            crate::json_splice::lenient_string_literals(frame)
        ),
    }
}

/// A binary frame holds nothing a chain can read. Refused where something reads
/// that side and fails closed, recorded as a bypass where not, as any other body
/// a chain cannot be given.
fn binary_frame_refusal(
    chain: &aisix_guardrails::GuardrailChain,
    input_side: bool,
) -> Option<(String, ProxyError)> {
    let refuses = if input_side {
        aisix_guardrails::Guardrail::refuses_unevaluable_input(chain)
    } else {
        aisix_guardrails::Guardrail::refuses_unevaluable_output(chain)
    };
    if refuses {
        let refusal = crate::error::guardrail_block_error(
            if input_side { "request" } else { "response" },
            None,
            Some(crate::error::TAG_UNSCANNABLE_BODY),
        );
        return Some((content_filtered_event(&refusal), refusal));
    }
    if input_side {
        chain.record_unevaluable_input_bypass(crate::error::TAG_UNSCANNABLE_BODY);
    } else {
        chain.record_unevaluable_output_bypass(crate::error::TAG_UNSCANNABLE_BODY);
    }
    None
}

/// Output text held until the item it belongs to can be judged whole.
///
/// A block-capable output chain has to see an answer before any of it reaches
/// the caller, and on this surface an answer is many delta frames: a literal
/// across two of them matches neither, and by the `.done` that carries it whole
/// the caller has it. So under such a chain the text deltas wait, per item, for
/// that item's `.done`, as a stream on the other routes waits for its end. The
/// audio of a voice answer cannot be read and is not held: holding it would only
/// make it late.
struct HeldOutput {
    max_buffer_bytes: usize,
    fail_open: bool,
    items: Vec<HeldOutputItem>,
    /// What `items` holds: generated text, which the cap measures, and raw
    /// frames, which it bounds too.
    content: usize,
    raw: usize,
    gauge: crate::held_content::HeldBytes,
    /// A fail-open overflow let the rest of this response through unscanned;
    /// the next one is held again.
    passing: bool,
}

struct HeldOutputItem {
    item_id: String,
    text: String,
    frames: Vec<String>,
}

/// What [`HeldOutput::observe`] did with a frame.
enum OutputStep {
    /// It went into the hold.
    Held,
    /// `frames` may go out once `scan` has been judged.
    Release {
        scan: Vec<String>,
        frames: Vec<String>,
    },
    /// The hold outgrew its cap, with `frames` all it had, in order.
    Overflow {
        fail_open: bool,
        frames: Vec<String>,
    },
}

impl OutputStep {
    fn release(frame: String) -> Self {
        Self::Release {
            scan: Vec::new(),
            frames: vec![frame],
        }
    }
}

/// The fields of an upstream frame the hold reads. Borrowed, since an audio
/// delta's payload is the bulk of a voice session's traffic.
#[derive(serde::Deserialize)]
struct OutputFrameHead<'a> {
    #[serde(rename = "type", borrow)]
    kind: std::borrow::Cow<'a, str>,
    #[serde(default, borrow)]
    item_id: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    delta: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow)]
    item: Option<OutputFrameItem<'a>>,
}

#[derive(serde::Deserialize)]
struct OutputFrameItem<'a> {
    #[serde(default, borrow)]
    id: Option<std::borrow::Cow<'a, str>>,
}

impl HeldOutput {
    fn new(max_buffer_bytes: usize, fail_open: bool) -> Self {
        Self {
            max_buffer_bytes,
            fail_open,
            items: Vec::new(),
            content: 0,
            raw: 0,
            gauge: Default::default(),
            passing: false,
        }
    }

    fn observe(&mut self, frame: String) -> OutputStep {
        let Ok(head) = serde_json::from_str::<OutputFrameHead<'_>>(&frame) else {
            return OutputStep::release(frame);
        };
        let kind = head.kind.as_ref();
        if self.passing {
            self.passing = kind != "response.done";
            return OutputStep::release(frame);
        }
        // An answer is over, whichever way: nothing else will close its items.
        if kind == "response.done" {
            return self.release_items(None, frame);
        }
        if !kind.starts_with("response.") {
            return OutputStep::release(frame);
        }
        if kind.ends_with(".done") {
            let item_id = head
                .item_id
                .or_else(|| head.item.and_then(|item| item.id))
                .map(|id| id.into_owned());
            return match item_id {
                Some(item_id) => self.release_items(Some(&item_id), frame),
                None => OutputStep::release(frame),
            };
        }
        let is_audio = matches!(kind, "response.output_audio.delta" | "response.audio.delta");
        let (true, false, Some(delta)) = (kind.ends_with(".delta"), is_audio, head.delta) else {
            return OutputStep::release(frame);
        };
        let item_id = head.item_id.unwrap_or_default();
        let position = self.items.iter().position(|i| i.item_id == item_id);
        let item = match position {
            Some(position) => &mut self.items[position],
            None => {
                self.items.push(HeldOutputItem {
                    item_id: item_id.into_owned(),
                    text: String::new(),
                    frames: Vec::new(),
                });
                self.items.last_mut().expect("just pushed")
            }
        };
        item.text.push_str(&delta);
        self.content = self.content.saturating_add(delta.len());
        self.raw = self.raw.saturating_add(frame.len());
        item.frames.push(frame);
        self.gauge.set(self.raw);
        let mut held = crate::held_content::HeldBuffer::default();
        held.hold(self.content, self.raw);
        if !held.exceeds(self.max_buffer_bytes) {
            return OutputStep::Held;
        }
        let frames = self.items.drain(..).flat_map(|item| item.frames).collect();
        self.content = 0;
        self.raw = 0;
        self.gauge.clear();
        self.passing = self.fail_open;
        OutputStep::Overflow {
            fail_open: self.fail_open,
            frames,
        }
    }

    /// Release `item_id`'s item, or every item, ahead of `frame`.
    fn release_items(&mut self, item_id: Option<&str>, frame: String) -> OutputStep {
        let mut scan = Vec::new();
        let mut frames = Vec::new();
        let mut kept = Vec::new();
        for item in self.items.drain(..) {
            if item_id.is_none_or(|id| id == item.item_id) {
                self.content = self.content.saturating_sub(item.text.len());
                self.raw = self
                    .raw
                    .saturating_sub(item.frames.iter().map(String::len).sum());
                scan.push(item.text);
                frames.extend(item.frames);
            } else {
                kept.push(item);
            }
        }
        self.items = kept;
        self.gauge.set(self.raw);
        frames.push(frame);
        OutputStep::Release { scan, frames }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_access_log(
    method: &Method,
    status: u16,
    elapsed: Duration,
    request_id: &str,
    api_key_id: Option<&str>,
    target: Option<(&str, &str)>,
    error: Option<&ProxyError>,
    // The session's `(input, output)` totals, the same figures its usage
    // event records. `None` before a session ran.
    tokens: Option<(u64, u64)>,
    provider_request_id: Option<&str>,
) {
    // A session that counted nothing carries no token columns, as on every
    // other line.
    let tokens = tokens.filter(|(input, output)| input + output > 0);
    let (error_kind, error) = match error {
        Some(e) => {
            let (kind, msg) = crate::attempt::access_log_error(e);
            (Some(kind), Some(msg))
        }
        None => (None, None),
    };
    let log_target = crate::attribution::AccessLogTarget::current();
    crate::attribution::emit_access_log(AccessLog {
        method: method.as_str(),
        path: "/v1/realtime",
        status,
        latency: elapsed,
        duration: elapsed,
        provider: target.map(|(p, _)| p).filter(|p| !p.is_empty()),
        model: target.map(|(_, m)| m),
        upstream_model: log_target.upstream_model(),
        provider_key_id: log_target.provider_key_id(),
        api_key_id,
        prompt_tokens: tokens.map(|(input, _)| input),
        completion_tokens: tokens.map(|(_, output)| output),
        // OpenAI's realtime usage reports a cache hit as a subset of
        // `input_tokens`, so the cache-inclusive total is the plain sum.
        total_tokens: tokens.map(|(input, output)| input + output),
        request_id,
        provider_request_id,
        served_by_model: None,
        routing_attempt_count: None,
        routing_fallback_count: None,
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
    /// tungstenite gives a refused upgrade only the bytes that came with its
    /// head, undecoded. Only a body they demonstrably complete is quoted.
    #[tokio::test]
    async fn a_refused_upgrade_quotes_only_a_body_that_arrived_whole() {
        // With no stream left to read, what came with the head is all there is.
        async fn refused(headers: &[(&str, &str)], body: Option<&[u8]>) -> String {
            refused_with(400, headers, body).await
        }
        async fn refused_with(
            status: u16,
            headers: &[(&str, &str)],
            body: Option<&[u8]>,
        ) -> String {
            let mut response = axum::http::Response::builder().status(status);
            for (name, value) in headers {
                response = response.header(*name, *value);
            }
            let failure = super::DialFailure {
                error: tokio_tungstenite::tungstenite::Error::Http(
                    response.body(body.map(<[u8]>::to_vec)).unwrap(),
                ),
                stream: None,
            };
            super::connect_failure(failure, Duration::ZERO, None, "req", std::future::pending())
                .await
                .0
                .to_string()
        }
        let whole = br#"{"error":{"message":"bad session"}}"#;
        let quoted = "upstream returned HTTP 400: bad session";
        let length = whole.len().to_string();
        assert_eq!(
            refused(&[("content-length", &length)], Some(whole)).await,
            quoted
        );
        let json = String::from_utf8_lossy(whole);
        let chunked = format!("{:x}\r\n{json}\r\n0\r\n\r\n", whole.len());
        assert_eq!(
            refused(
                &[("transfer-encoding", "chunked")],
                Some(chunked.as_bytes())
            )
            .await,
            quoted
        );
        assert_eq!(
            refused(&[], Some(whole)).await,
            quoted,
            "up to the end of the connection"
        );

        let one_byte_chunks: String = "x"
            .repeat(30_000)
            .chars()
            .map(|c| format!("1\r\n{c}\r\n"))
            .collect();
        assert_eq!(
            refused(
                &[("transfer-encoding", "chunked")],
                Some(format!("{one_byte_chunks}0\r\n\r\n").as_bytes())
            )
            .await,
            format!("upstream returned HTTP 400: {}…", "x".repeat(1024)),
            "framing that outweighs its payload is still framing"
        );

        let cut = "upstream returned HTTP 400: failed to read response body: \
                   the connection closed before the whole body arrived";
        for (what, headers, body) in [
            (
                "detail still on its way",
                vec![("content-length", "4096")],
                Some(&b""[..]),
            ),
            (
                "part of the body",
                vec![("content-length", "4096")],
                Some(&whole[..]),
            ),
            (
                "a chunked body cut short",
                vec![("transfer-encoding", "chunked")],
                Some(&b"23\r\n{\"error\":{\"mess"[..]),
            ),
            (
                "a chunked body with no last chunk",
                vec![("transfer-encoding", "chunked")],
                Some(&b"3\r\nabc\r\n"[..]),
            ),
        ] {
            assert_eq!(refused(&headers, body).await, cut, "{what}");
        }
        assert_eq!(
            refused(&[("transfer-encoding", "chunked")], Some(b"zz\r\nabc")).await,
            "upstream returned HTTP 400: failed to read response body: malformed chunked body"
        );

        let no_detail = "upstream returned HTTP 400 with no error detail in the response";
        for (what, headers, body) in [
            (
                "compressed",
                vec![("content-length", "4"), ("content-encoding", "gzip")],
                Some(&b"\x1f\x8b\x08\x00"[..]),
            ),
            (
                "compressed in transfer",
                vec![("transfer-encoding", "gzip, chunked")],
                Some(&b"4\r\n\x1f\x8b\x08\x00\r\n0\r\n\r\n"[..]),
            ),
            ("nothing kept", vec![("content-length", "0")], None),
            (
                "an empty body",
                vec![("content-length", "0")],
                Some(&b""[..]),
            ),
        ] {
            assert_eq!(refused(&headers, body).await, no_detail, "{what}");
        }

        let empty = [("content-length", "0")];
        assert_eq!(
            refused_with(500, &empty, Some(b"")).await,
            "upstream returned HTTP 500 with no error detail in the response"
        );
        for status in [200, 302] {
            let in_handshake =
                format!("upstream returned HTTP {status} during the realtime handshake");
            assert_eq!(refused_with(status, &empty, Some(b"")).await, in_handshake);
            assert_eq!(
                refused_with(
                    status,
                    &[("content-length", "4"), ("content-encoding", "gzip")],
                    Some(b"\x1f\x8b\x08\x00"),
                )
                .await,
                in_handshake
            );
        }
    }

    /// `LIBRARY_REJECTED_CLOSE` mirrors a literal private to tungstenite;
    /// this fails if a tungstenite bump changes what it substitutes.
    #[tokio::test]
    async fn library_rejected_close_matches_what_tungstenite_substitutes() {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        use tokio_tungstenite::tungstenite::protocol::Role;
        // A Close carrying 1004, a code that may not be sent: unmasked as
        // the upstream sends it to the gateway's client role, and masked
        // (all-zero key) as a client sends it to axum's server role.
        for (role, frame) in [
            (Role::Client, &[0x88, 0x04, 0x03, 0xEC, b'r', b'1'][..]),
            (
                Role::Server,
                &[0x88, 0x84, 0, 0, 0, 0, 0x03, 0xEC, b'r', b'1'][..],
            ),
        ] {
            let (local, mut remote) = tokio::io::duplex(64);
            let mut ws =
                tokio_tungstenite::WebSocketStream::from_raw_socket(local, role, None).await;
            remote.write_all(frame).await.unwrap();
            let Some(Ok(TgMessage::Close(Some(close)))) = ws.next().await else {
                panic!("expected a Close frame ({role:?})");
            };
            assert_eq!(
                (u16::from(close.code), close.reason.as_ref()),
                super::LIBRARY_REJECTED_CLOSE,
                "{role:?}"
            );
        }
    }

    /// JSON lets both a key and a string value be spelled with escapes, and
    /// the two halves are handled in different places — so they are pinned
    /// separately.
    ///
    /// The KEY half is the one that was broken: the splice walker decodes
    /// keys and matches `"\u0073ession"` fine, but `splice_or_keep`'s fast
    /// path tested only for the literal spelling and returned before the
    /// walker ran, leaking the upstream model id.
    #[test]
    fn an_escaped_session_key_is_still_restamped() {
        let frame = r#"{"type":"session.created","\u0073ession":{"model":"up-1"}}"#;
        let out = restamp_session_model_out(frame.to_string(), "echo-realtime");
        assert!(
            out.contains(r#""model":"echo-realtime""#),
            "the fast path must not skip an escaped key: {out}"
        );
        assert!(!out.contains("up-1"));
    }

    /// The VALUE half needs no special handling and this proves it rather
    /// than assuming it: the walker offers the DECODED text to the rewrite
    /// closure, so an alias spelled with escapes compares equal and is
    /// translated back to the provider's own id.
    #[test]
    fn an_escaped_alias_value_still_translates_back_upstream() {
        let frame = r#"{"type":"session.update","session":{"model":"echo-\u0072ealtime"}}"#;
        let out = restamp_session_model_in(frame.to_string(), "echo-realtime", "gpt-realtime");
        assert!(
            out.contains(r#""model":"gpt-realtime""#),
            "an escaped spelling of the alias is still the alias: {out}"
        );
    }

    /// The fast path still skips the frames it exists for. An audio delta is
    /// base64 with no backslash and no session, so it must come back as the
    /// very same allocation-free string.
    #[test]
    fn an_audio_delta_takes_the_fast_path_unchanged() {
        let frame =
            r#"{"type":"response.output_audio.delta","delta":"UklGRiQAAABXQVZFZm10IBAAAAA="}"#;
        assert_eq!(
            restamp_session_model_out(frame.to_string(), "echo-realtime"),
            frame
        );
        assert_eq!(
            restamp_session_model_in(frame.to_string(), "echo-realtime", "gpt-realtime"),
            frame
        );
    }
    use super::*;
    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, Model, ProxyConfig};
    use aisix_gateway::Hub;
    use aisix_obs::{UsageEvent as ObsUsageEvent, UsageSink};
    use futures::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    /// An upstream that accepts the connection and then never answers
    /// the upgrade must fail on the configured budget. Without one the
    /// dial has no deadline at all — the session's idle cap only starts
    /// once the socket is up — so the upgrade hangs for as long as the
    /// far end keeps the socket open.
    #[tokio::test]
    async fn a_silent_upstream_fails_the_dial_on_its_budget() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept and hold: the sockets stay open and unanswered for as
        // long as this task lives.
        let _silent = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });

        let request = format!("ws://{addr}/v1/realtime")
            .into_client_request()
            .unwrap();
        let budget = Duration::from_millis(300);
        // The outer bound is the assertion: unbudgeted, the dial simply
        // never returns, so a plain `.await` here would hang the suite
        // rather than fail it.
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            connect_upstream_within(Some(budget), request),
        )
        .await
        .expect("the dial must end on its own budget")
        .err()
        .expect("a silent upstream cannot complete the handshake");
        let (failure, refused) = connect_failure(
            err,
            Duration::from_millis(300),
            None,
            "req",
            std::future::pending(),
        )
        .await;
        assert!(refused.is_none());
        assert!(
            matches!(
                &failure,
                BridgeError::Timeout {
                    elapsed_ms: 300,
                    ..
                }
            ),
            "the dial's budget must report as a timeout: {failure}"
        );
    }

    /// [`connect_upstream_within`] takes its budget as an argument, so
    /// only its production caller binds it to the operator's setting. The
    /// workspace scan in `upstream_http` cannot see that binding — this
    /// module names the config too — so pin it to the function body.
    #[test]
    fn the_production_dial_takes_its_budget_from_the_upstream_config() {
        let src = include_str!("realtime.rs");
        let body = src
            .split_once("async fn connect_upstream(")
            .expect("connect_upstream is defined in this file")
            .1
            .split_once("\n}\n")
            .expect("its body ends at a top-level brace")
            .0;
        assert!(
            body.contains("upstream_http::config().connect_timeout"),
            "the Realtime dial must pass `upstream.connect_timeout` as its budget: {body}"
        );
    }

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

    const PK_ID: &str = "22222222-2222-2222-2222-222222222222";

    /// This error is the caller's 400, so a base it quotes is shown without
    /// the password of a configured URL.
    #[test]
    fn a_refused_api_base_never_shows_its_password() {
        let err = to_ws_scheme("ftp://user:s3cret@upstream.example/v1").unwrap_err();
        let shown = err.to_string();
        assert!(shown.contains("upstream.example"), "{shown}");
        assert!(!shown.contains("s3cret"), "{shown}");
        // A userinfo base reaches its scheme mapping unchanged.
        assert_eq!(
            to_ws_scheme("https://user:s3cret@upstream.example/v1").unwrap(),
            "wss://user:s3cret@upstream.example/v1"
        );
    }
    // sha256("sk-caller") — the plaintext used by all tests below.
    const CALLER_HASH: &str = "8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891";

    fn snapshot(api_base: &str, adapter: &str, provider: &str) -> AisixSnapshot {
        snapshot_with_cost(api_base, adapter, provider, "")
    }

    /// `cost` 是要插进模型行的 `"cost": {...}` 片段（含前导逗号），空串表示
    /// 不配价。
    fn snapshot_with_cost(
        api_base: &str,
        adapter: &str,
        provider: &str,
        cost: &str,
    ) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        let pk_json = format!(
            r#"{{"display_name":"rt-pk","secret":"sk-up","api_base":"{api_base}","provider":"{provider}","adapter":"{adapter}"}}"#
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&pk_json).unwrap();
        snap.provider_keys.insert(ResourceEntry::new(PK_ID, pk, 1));
        let m_json = format!(
            r#"{{"display_name":"rt-model","provider":"{provider}","model_name":"gpt-realtime","provider_key_id":"{PK_ID}"{cost}}}"#
        );
        let m: Model = serde_json::from_str(&m_json).unwrap();
        snap.models.insert(ResourceEntry::new("m-rt", m, 1));
        let k_json = format!(r#"{{"key_hash":"{CALLER_HASH}","allowed_models":["*"]}}"#);
        let k: ApiKey = serde_json::from_str(&k_json).unwrap();
        snap.apikeys.insert(ResourceEntry::new("k-1", k, 1));
        snap
    }

    /// [`snapshot`] whose ProviderKey opted into forwarding `forward`.
    fn snapshot_forwarding(api_base: &str, forward: &[&str]) -> AisixSnapshot {
        let snap = snapshot(api_base, "openai", "openai");
        let pk_json = format!(
            r#"{{"display_name":"rt-pk","secret":"sk-up","api_base":"{api_base}",
                 "provider":"openai","adapter":"openai",
                 "request":{{"forward_client_headers":{}}}}}"#,
            serde_json::to_string(forward).unwrap()
        );
        let pk: aisix_core::ProviderKey = serde_json::from_str(&pk_json).unwrap();
        snap.provider_keys.insert(ResourceEntry::new(PK_ID, pk, 1));
        snap
    }

    /// Bind the full proxy router on a real TCP port (WS handshakes need a
    /// live connection; `oneshot` can't upgrade).
    async fn serve(
        snap: AisixSnapshot,
    ) -> (
        std::net::SocketAddr,
        crate::ProxyState,
        tokio::sync::mpsc::Receiver<ObsUsageEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel::<ObsUsageEvent>(16);
        let hub = Arc::new(Hub::new());
        let handle = SnapshotHandle::new(snap);
        let state = crate::ProxyState::new(handle, hub, &cfg())
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
        let app = crate::build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        (addr, state, rx)
    }

    /// One header's value as a string, `""` when absent — the mock
    /// records the whole map, and every assertion below reads one name.
    fn header_str(headers: &HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    /// Scripted mock upstream: accepts ONE WebSocket, records the request
    /// path and its FULL header map, waits for one text frame, replies
    /// with a `response.done` usage frame, then closes.
    type SeenHandshake = Option<(String, HeaderMap)>;

    async fn spawn_upstream() -> (
        std::net::SocketAddr,
        Arc<Mutex<SeenHandshake>>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let seen_handshake: Arc<Mutex<SeenHandshake>> = Arc::new(Mutex::new(None));
        let seen_frames: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hs = seen_handshake.clone();
        let frames = seen_frames.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let hs2 = hs.clone();
            let ws = tokio_tungstenite::accept_hdr_async(
                stream,
                move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    *hs2.lock().unwrap() = Some((req.uri().to_string(), req.headers().clone()));
                    Ok(resp)
                },
            )
            .await
            .unwrap();
            let (mut tx, mut rx) = ws.split();
            while let Some(Ok(msg)) = rx.next().await {
                if let TgMessage::Text(t) = msg {
                    frames.lock().unwrap().push(t.clone());
                    tx.send(TgMessage::Text(
                        serde_json::json!({
                            "type": "response.done",
                            "response": {"usage": {
                                "input_tokens": 7,
                                "output_tokens": 3,
                                "input_token_details": {"cached_tokens": 1}
                            }}
                        })
                        .to_string(),
                    ))
                    .await
                    .unwrap();
                    tx.send(TgMessage::Close(None)).await.ok();
                    break;
                }
            }
        });
        (addr, seen_handshake, seen_frames)
    }

    /// An upstream that records every frame it is sent, on any number of
    /// connections.
    async fn recording_upstream() -> (
        std::net::SocketAddr,
        Arc<Mutex<Vec<(&'static str, String)>>>,
    ) {
        let seen: Arc<Mutex<Vec<(&'static str, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorder = seen.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let recorder = recorder.clone();
                tokio::spawn(async move {
                    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    let (_tx, mut rx) = ws.split();
                    while let Some(Ok(msg)) = rx.next().await {
                        match msg {
                            TgMessage::Text(t) => recorder.lock().unwrap().push(("text", t)),
                            TgMessage::Binary(b) => recorder
                                .lock()
                                .unwrap()
                                .push(("binary", String::from_utf8_lossy(&b).into_owned())),
                            _ => {}
                        }
                    }
                });
            }
        });
        (addr, seen)
    }

    async fn client(
        addr: std::net::SocketAddr,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake")
            .0
    }

    fn user_item(text: &str) -> String {
        format!(
            r#"{{"type":"conversation.item.create","item":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
        )
    }

    async fn was_refused(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> bool {
        let mut refused = false;
        while let Ok(Some(Ok(msg))) =
            tokio::time::timeout(std::time::Duration::from_secs(3), ws.next()).await
        {
            if let TgMessage::Text(t) = &msg {
                refused |= t.contains("content_filtered");
            }
        }
        refused
    }

    /// The upstream decodes the frame, so a rule reads what it will read: an
    /// escape is no way around one, and neither is the same JSON in a frame the
    /// chain cannot read at all.
    #[tokio::test]
    async fn an_escape_or_a_binary_frame_does_not_get_past_a_block_rule() {
        let (up_addr, seen) = recording_upstream().await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let g: aisix_core::Guardrail = serde_json::from_value(serde_json::json!({
            "name": "kw", "enabled": true, "hook_point": "input", "fail_open": false,
            "kind": "keyword", "patterns": [{"kind": "literal", "value": "forbidden"}],
        }))
        .unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-kw", g, 1));
        let (addr, _state, _rx) = serve(snap).await;

        let too_deep = format!(
            "{}{}{}",
            "[".repeat(200),
            user_item("\\u0066orbidden"),
            "]".repeat(200)
        );
        for frame in [
            TgMessage::Text(user_item("forbidden")),
            TgMessage::Text(user_item("\\u0066orbidden")),
            TgMessage::Binary(user_item("forbidden").into_bytes()),
            // What this gateway cannot parse and a peer may: a lone surrogate,
            // a NaN, and nesting past the parser's depth limit.
            TgMessage::Text(user_item("\\u0066orbidden \\ud800")),
            TgMessage::Text(user_item("\\u0066orbidden").replacen("{", "{\"x\":NaN,", 1)),
            TgMessage::Text(too_deep),
        ] {
            let mut ws = client(addr).await;
            ws.send(frame.clone()).await.unwrap();
            assert!(was_refused(&mut ws).await, "{frame:?}");
        }
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
    }

    /// A row that fails open lets a binary frame through, and an output-only row
    /// has no say over what the client sends.
    #[tokio::test]
    async fn a_binary_frame_passes_a_chain_that_does_not_refuse() {
        for row in [
            serde_json::json!({"name": "kw", "enabled": true, "hook_point": "input",
                "fail_open": true, "kind": "keyword",
                "patterns": [{"kind": "literal", "value": "forbidden"}]}),
            serde_json::json!({"name": "kw", "enabled": true, "hook_point": "output",
                "fail_open": false, "kind": "keyword",
                "patterns": [{"kind": "literal", "value": "forbidden"}]}),
        ] {
            let (up_addr, seen) = recording_upstream().await;
            let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
            let g: aisix_core::Guardrail = serde_json::from_value(row).unwrap();
            crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-kw", g, 1));
            let (addr, _state, _rx) = serve(snap).await;
            let mut ws = client(addr).await;
            ws.send(TgMessage::Binary(b"opaque".to_vec()))
                .await
                .unwrap();
            for _ in 0..200 {
                if !seen.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                seen.lock().unwrap().as_slice(),
                [("binary", "opaque".to_string())]
            );
        }
    }

    /// A mock upstream that answers each client frame with one `response.done`
    /// and stays open, until the gateway closes it.
    async fn spawn_open_upstream() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            while let Some(Ok(msg)) = rx.next().await {
                if matches!(msg, TgMessage::Text(_)) {
                    let done = serde_json::json!({
                        "type": "response.done",
                        "response": {"usage": {"input_tokens": 1000, "output_tokens": 1000}}
                    });
                    if tx.send(TgMessage::Text(done.to_string())).await.is_err() {
                        return;
                    }
                }
            }
        });
        addr
    }

    /// The spend counter as `/metrics` shows it, waiting for it to reach `want`.
    async fn spend_reaches(state: &crate::ProxyState, want: u64) -> u64 {
        let mut seen = 0;
        for _ in 0..60 {
            seen = state
                .metrics
                .render()
                .lines()
                .filter(|l| l.starts_with(aisix_obs::metrics::M_LLM_SPEND_MICRO_USD_TOTAL))
                .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
                .sum();
            if seen >= want {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        seen
    }

    /// A price that changes mid-session applies from then on: what was already
    /// charged stays charged at the price it was charged at.
    #[tokio::test]
    async fn a_price_change_mid_session_applies_from_then_on() {
        let up_addr = spawn_open_upstream().await;
        let snap = snapshot_with_cost(
            &format!("http://{up_addr}/v1"),
            "openai",
            "openai",
            r#","pricing_key":"rt-price""#,
        );
        let price = |per_1k: f64, rev: i64| {
            let p: aisix_core::models::pricing::Pricing = serde_json::from_value(
                serde_json::json!({"key": "rt-price", "input_per_1k": per_1k, "output_per_1k": per_1k}),
            )
            .unwrap();
            ResourceEntry::new("p-1", p, rev)
        };
        snap.pricing.insert(price(0.5, 1));
        let (addr, state, mut rx) = serve(snap).await;
        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut tx, client_rx) = ws.split();
        let ask = || TgMessage::Text(serde_json::json!({"type": "response.create"}).to_string());

        // 1000 + 1000 tokens at $0.5/1k, then the same at $1/1k.
        tx.send(ask()).await.unwrap();
        assert_eq!(spend_reaches(&state, 1_000_000).await, 1_000_000);
        state.snapshot.load().pricing.insert(price(1.0, 2));
        tx.send(ask()).await.unwrap();
        assert_eq!(spend_reaches(&state, 3_000_000).await, 3_000_000);
        drop((tx, client_rx));

        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("the session's event")
            .expect("sink closed");
        assert!((ev.cost_usd - 3.0).abs() < 1e-9, "{}", ev.cost_usd);
        assert_eq!(
            spend_reaches(&state, 4_000_000).await,
            3_000_000,
            "nothing already charged is charged again"
        );
    }

    /// The key that opened a session stops it when it is rotated, when it may
    /// no longer use the model, and when its provider key's budget runs out.
    #[tokio::test]
    async fn a_session_ends_when_its_key_is_rotated_or_loses_the_model_or_the_budget() {
        for (change, status) in [("rotate", 401), ("acl", 403), ("budget", 0)] {
            let up_addr = spawn_open_upstream().await;
            let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
            let (addr, state, mut rx) = serve(snap).await;
            let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
                .into_client_request()
                .unwrap();
            req.headers_mut()
                .insert("authorization", "Bearer sk-caller".parse().unwrap());
            let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            let (mut tx, mut client_rx) = ws.split();
            let ask =
                || TgMessage::Text(serde_json::json!({"type": "response.create"}).to_string());
            tx.send(ask()).await.unwrap();
            assert!(matches!(
                client_rx.next().await,
                Some(Ok(TgMessage::Text(_)))
            ));

            let live = state.snapshot.load();
            match change {
                "rotate" | "acl" => {
                    let key = live.apikeys.get_by_id("k-1").unwrap();
                    let mut changed = (*key.value).clone();
                    if change == "rotate" {
                        changed.key_hash = "0".repeat(64);
                    } else {
                        changed.allowed_models = vec!["another-model".to_string()];
                    }
                    live.apikeys.insert(ResourceEntry::new("k-1", changed, 2));
                }
                _ => {
                    let budget: aisix_core::models::Budget =
                        serde_json::from_value(serde_json::json!({
                            "scope": "provider_key",
                            "scope_ref": PK_ID,
                            "period": "month",
                            "exhausted": {"reason": {"message": "pk out", "scope": "provider_key"}},
                        }))
                        .unwrap();
                    live.budgets.insert(ResourceEntry::new("b-1", budget, 1));
                }
            }

            tx.send(ask()).await.unwrap();
            let close_code = tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(Ok(msg)) = client_rx.next().await {
                    if let TgMessage::Close(frame) = msg {
                        return frame.map(|f| u16::from(f.code));
                    }
                }
                None
            })
            .await
            .ok()
            .flatten();
            assert_eq!(close_code, Some(1008), "{change}");
            let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("the session's event")
                .expect("sink closed");
            if status != 0 {
                assert_eq!(ev.status_code, status, "{change}");
            } else {
                assert!(ev.status_code >= 400, "{change}: {}", ev.status_code);
            }
        }
    }

    /// A session can run for hours. What it spends has to reach the counter a
    /// ledger reads while it is open, a key disabled meanwhile has to stop it,
    /// and what the session's one event reports is what the charges add up to.
    #[tokio::test]
    async fn an_open_session_is_charged_as_it_goes_and_ends_when_its_key_is_disabled() {
        let up_addr = spawn_open_upstream().await;
        let snap = snapshot_with_cost(
            &format!("http://{up_addr}/v1"),
            "openai",
            "openai",
            r#","cost":{"input_per_1k":0.5,"output_per_1k":0.5}"#,
        );
        let (addr, state, mut rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();
        let ask = || TgMessage::Text(serde_json::json!({"type": "response.create"}).to_string());

        // 1000 + 1000 tokens at $0.5/1k: $1 a response, with the session open.
        tx.send(ask()).await.unwrap();
        assert_eq!(spend_reaches(&state, 1_000_000).await, 1_000_000);
        tx.send(ask()).await.unwrap();
        assert_eq!(spend_reaches(&state, 2_000_000).await, 2_000_000);

        let live = state.snapshot.load();
        let key = live.apikeys.get_by_id("k-1").unwrap();
        let mut disabled = (*key.value).clone();
        disabled.disabled = true;
        live.apikeys.insert(ResourceEntry::new("k-1", disabled, 2));

        // The response in flight is still delivered, and billed; then it ends.
        tx.send(ask()).await.unwrap();
        let mut close_code = None;
        let mut responses = 0;
        while let Some(Ok(msg)) = client_rx.next().await {
            match msg {
                TgMessage::Text(_) => responses += 1,
                TgMessage::Close(frame) => {
                    close_code = frame.map(|f| u16::from(f.code));
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(close_code, Some(1008));
        assert_eq!(responses, 3);

        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("the session's event")
            .expect("sink closed");
        assert_eq!(ev.status_code, 401);
        assert_eq!(ev.prompt_tokens, 3_000);
        assert_eq!(ev.completion_tokens, 3_000);
        assert!((ev.cost_usd - 3.0).abs() < 1e-9, "{}", ev.cost_usd);
        assert_eq!(
            spend_reaches(&state, 3_000_000).await,
            3_000_000,
            "the charges add up to what the event reports, and no further"
        );
    }

    /// 虚拟父级（routing / ensemble / semantic）不能走 `/v1/realtime`。
    ///
    /// 这不只是「不支持」：父级行结构上不带 `cost`，所以一旦让它进来，整场会话
    /// 的花费恒为 0 —— 一条不计费的通道，而且没有任何信号。这道闸此前没有任何
    /// 测试。
    #[tokio::test]
    async fn a_virtual_router_is_refused_for_being_virtual() {
        let snap = snapshot("http://127.0.0.1:1/v1", "openai", "openai");
        let group: Model = serde_json::from_str(
            r#"{"display_name":"rt-group","routing":{"targets":[{"model":"rt-model"}]}}"#,
        )
        .unwrap();
        snap.models
            .insert(ResourceEntry::new("m-rt-group", group, 1));
        let handle = SnapshotHandle::new(snap);
        let live = handle.load();
        let state = crate::ProxyState::new(handle.clone(), Arc::new(Hub::new()), &cfg());
        let key: aisix_core::ApiKey =
            serde_json::from_str(r#"{"key_hash":"h","allowed_models":["*"]}"#).unwrap();
        let auth = crate::auth::AuthenticatedKey::new(
            &live,
            Arc::new(ResourceEntry::new("k-1", key, 1)),
            None,
            false,
        );
        let params = HashMap::from([("model".to_string(), "rt-group".to_string())]);
        let client = crate::client_ip::ClientContext::default();

        let err =
            match super::prepare(&state, &live, &params, &HeaderMap::new(), &client, auth).await {
                Err(e) => e,
                Ok(_) => panic!("虚拟父级不该被放进来 —— 那是一条不计费的通道"),
            };

        // **断言拒绝的理由**，不只是「被拒了」。父级没有 provider / model_name，
        // 所以把这道闸拆掉之后连接照样会失败 —— 只断言「失败」的话，这条用例
        // 无论闸在不在都是绿的（第一版就是这样，变异检查照过）。
        let msg = err.to_string();
        assert!(
            msg.contains("virtual router"),
            "拒绝理由说的不是「这是个虚拟父级」，那说明闸没起作用、是后面某步偶然\
             拦下的: {msg}"
        );
    }

    /// A key restricted to provider groups opens a session only with a
    /// provider key in one of them; outside them it is refused as a routing
    /// group with no target left is, naming no group.
    #[tokio::test]
    async fn a_key_with_groups_opens_a_session_only_inside_them() {
        async fn prepared(pk_groups: &[&str], key_groups: &[&str]) -> Result<(), ProxyError> {
            let snap = snapshot("http://127.0.0.1:1/v1", "openai", "openai");
            let pk: aisix_core::ProviderKey = serde_json::from_value(serde_json::json!({
                "display_name": "rt-pk", "secret": "sk-up",
                "api_base": "http://127.0.0.1:1/v1",
                "provider": "openai", "adapter": "openai",
                "groups": pk_groups,
            }))
            .unwrap();
            snap.provider_keys.insert(ResourceEntry::new(PK_ID, pk, 1));
            let handle = SnapshotHandle::new(snap);
            let live = handle.load();
            let state = crate::ProxyState::new(handle.clone(), Arc::new(Hub::new()), &cfg());
            let key: aisix_core::ApiKey = serde_json::from_value(serde_json::json!({
                "key_hash": "h", "allowed_models": ["*"],
                "allowed_provider_groups": key_groups,
            }))
            .unwrap();
            let auth = crate::auth::AuthenticatedKey::new(
                &live,
                Arc::new(ResourceEntry::new("k-1", key, 1)),
                None,
                false,
            );
            let params = HashMap::from([("model".to_string(), "rt-model".to_string())]);
            let client = crate::client_ip::ClientContext::default();
            super::prepare(&state, &live, &params, &HeaderMap::new(), &client, auth)
                .await
                .map(|_| ())
        }

        assert!(prepared(&[], &[]).await.is_ok());
        assert!(prepared(&["grp-alpha"], &["grp-alpha"]).await.is_ok());
        for (pk_groups, key_groups) in [
            (&[][..], &["grp-alpha"][..]),
            (&["grp-beta"], &["grp-alpha"]),
        ] {
            let err = prepared(pk_groups, key_groups).await.unwrap_err();
            assert!(
                matches!(
                    err,
                    ProxyError::AllCandidatesUnavailable {
                        retry_after_secs: None
                    }
                ),
                "{err:?}"
            );
            let text = format!("{err} {err:?}");
            assert!(!text.contains("grp-"), "{text}");
            assert!(!text.contains("rt-pk"), "{text}");
        }
    }

    /// 会话里从提示缓存读回来的 token 要按缓存费率计价，而不是全价输入。
    ///
    /// 这个表面自己算成本（`ModelCost::calculate(input, output)`），而家族其它
    /// 端点一律走 `usage_attr::request_cost_usd` + `input_tokens_for_pricing`，
    /// 那条路会把 `cached_input_per_1k` 应用上。事件里 `cached_prompt_tokens`
    /// 是照报的，所以按事件重算花费的控制面会得到另一个数字 —— 两个平面对同
    /// 一次会话给出不同的钱。
    ///
    /// 上游存根回 7 输入（其中 1 来自缓存）+ 3 输出。输入 $10/1k、缓存读
    /// $1/1k、输出 $20/1k：应为 6*0.01 + 1*0.001 + 3*0.02 = $0.121。按全价
    /// 输入算会得到 7*0.01 + 3*0.02 = $0.13。
    #[tokio::test]
    async fn cached_session_tokens_are_priced_at_the_cache_rate() {
        let (up_addr, _handshake, _frames) = spawn_upstream().await;
        let snap = snapshot_with_cost(
            &format!("http://{up_addr}/v1"),
            "openai",
            "openai",
            r#","cost":{"input_per_1k":10.0,"output_per_1k":20.0,"cached_input_per_1k":1.0}"#,
        );
        let (addr, _state, mut rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();
        tx.send(TgMessage::Text(
            serde_json::json!({"type": "session.update"}).to_string(),
        ))
        .await
        .unwrap();
        // 存根回一帧带用量的 response.done 之后就关闭；会话的记账在关闭时做。
        while let Some(Ok(msg)) = client_rx.next().await {
            if matches!(msg, TgMessage::Close(_)) {
                break;
            }
        }

        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("usage event expected")
            .expect("sink closed");
        assert_eq!(ev.cached_prompt_tokens, 1, "缓存 token 没被记下来");
        assert!(
            (ev.cost_usd - 0.121).abs() < 1e-9,
            "缓存读按全价输入计了价（应为 0.121，得到 {}）",
            ev.cost_usd
        );
    }

    /// The upstream dial failing is still the direct model's event: it
    /// names that model in `model_id`, not just in `requested_model_id`.
    #[tokio::test]
    async fn a_failed_upstream_dial_names_the_direct_model() {
        let snap = snapshot("http://127.0.0.1:1/v1", "openai", "openai");
        let (addr, _state, mut rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let _client = tokio_tungstenite::connect_async(req).await;

        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("usage event expected")
            .expect("sink closed");
        assert_eq!(ev.status_code, 502, "{ev:?}");
        assert_eq!(ev.error_class, "transport", "{ev:?}");
        assert_eq!(ev.model_id, "m-rt");
        assert_eq!(ev.requested_model_id, "m-rt");
    }

    #[tokio::test]
    async fn a_response_finishing_while_the_client_closes_still_counts() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            while let Some(Ok(msg)) = rx.next().await {
                if let TgMessage::Text(_) = msg {
                    // Finish the response after the client has hung up.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let done = serde_json::json!({
                        "type": "response.done",
                        "response": {"usage": {"input_tokens": 5, "output_tokens": 2}}
                    });
                    let _ = tx.send(TgMessage::Text(done.to_string())).await;
                }
            }
        });
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let (addr, _state, mut rx) = serve(snap).await;
        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();
        tx.send(TgMessage::Text(r#"{"type":"response.create"}"#.into()))
            .await
            .unwrap();
        tx.send(TgMessage::Close(None)).await.unwrap();
        while let Some(Ok(msg)) = client_rx.next().await {
            if matches!(msg, TgMessage::Close(_)) {
                break;
            }
        }
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("usage event expected")
            .expect("sink closed");
        assert_eq!((ev.prompt_tokens, ev.completion_tokens), (5, 2));
    }

    #[tokio::test]
    async fn relays_frames_and_emits_aggregated_usage_event() {
        let (up_addr, handshake, frames) = spawn_upstream().await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let (addr, _state, mut rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();

        tx.send(TgMessage::Text(
            serde_json::json!({"type": "session.update", "session": {"instructions": "hi"}})
                .to_string(),
        ))
        .await
        .unwrap();

        // The upstream's response.done frame must reach the client verbatim.
        let mut got_done = false;
        while let Some(Ok(msg)) = client_rx.next().await {
            match msg {
                TgMessage::Text(t) if t.contains("response.done") => {
                    got_done = true;
                }
                TgMessage::Close(_) => break,
                _ => {}
            }
        }
        assert!(got_done, "client must receive the upstream response.done");

        // Upstream saw the relayed client frame + the gateway's provider auth.
        assert_eq!(frames.lock().unwrap().len(), 1);
        assert!(frames.lock().unwrap()[0].contains("session.update"));
        let (uri, seen) = handshake
            .lock()
            .unwrap()
            .clone()
            .expect("handshake recorded");
        let auth = header_str(&seen, "authorization");
        let beta = seen.get("openai-beta").map(|_| ());
        assert!(
            uri.contains("/v1/realtime") && uri.contains("model=gpt-realtime"),
            "upstream URI must be the realtime path with the UPSTREAM model id, got {uri}"
        );
        assert_eq!(auth, "Bearer sk-up");
        assert_eq!(
            beta, None,
            "a caller that did not opt in must not have `openai-beta` forwarded upstream: \
             OpenAI's GA endpoint rejects it with beta_api_shape_disabled"
        );

        // Session-aggregate usage event.
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("usage event expected")
            .expect("sink closed");
        assert_eq!(ev.inbound_protocol, "realtime");
        assert_eq!(ev.prompt_tokens, 7);
        assert_eq!(ev.completion_tokens, 3);
        assert_eq!(ev.cached_prompt_tokens, 1);
        assert_eq!(ev.requested_model, "rt-model");
        assert_eq!(ev.requested_model_id, "m-rt");
        assert_eq!(ev.api_key_id, "k-1");
    }

    /// An upstream that plays `frames` as soon as the client speaks, then keeps
    /// the session open.
    async fn spawn_scripted_upstream(frames: Vec<serde_json::Value>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            let _ = rx.next().await;
            for frame in frames {
                tx.send(TgMessage::Text(frame.to_string())).await.unwrap();
            }
            while rx.next().await.is_some() {}
        });
        addr
    }

    /// The frames a client receives for `frames` under `guardrail`, up to the
    /// session's close or `response.done`.
    async fn relayed_frames(
        frames: Vec<serde_json::Value>,
        guardrail: &str,
    ) -> Vec<serde_json::Value> {
        let up_addr = spawn_scripted_upstream(frames).await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let row: aisix_core::Guardrail = serde_json::from_str(guardrail).unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-1", row, 1));
        let (addr, _state, _rx) = serve(snap).await;
        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        tx.send(TgMessage::Text(r#"{"type":"response.create"}"#.to_string()))
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(3), rx.next()).await
        {
            match msg {
                TgMessage::Text(t) => {
                    let frame: serde_json::Value = serde_json::from_str(&t).unwrap();
                    let last = frame["type"] == "response.done";
                    seen.push(frame);
                    if last {
                        break;
                    }
                }
                TgMessage::Close(_) => break,
                _ => {}
            }
        }
        seen
    }

    fn delta(kind: &str, item: &str, delta: &str) -> serde_json::Value {
        serde_json::json!({"type": kind, "item_id": item, "delta": delta})
    }

    const KEYWORD_OUTPUT_BLOCK: &str = r#"{"name":"out-block","enabled":true,"hook_point":"output","kind":"keyword","patterns":[{"kind":"literal","value":"FORBIDDEN"}]}"#;

    /// A client that stops reading cannot hold the session: a send it never
    /// accepts is bounded like a receive, and the session ends, settles and lets
    /// go of what it held.
    #[tokio::test]
    async fn a_client_that_stops_reading_does_not_hold_the_session() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            let _ = rx.next().await;
            let frame = serde_json::json!({
                "type": "response.output_audio.delta",
                "item_id": "i1",
                "delta": "A".repeat(1 << 20),
            })
            .to_string();
            for _ in 0..64 {
                if tx.send(TgMessage::Text(frame.clone())).await.is_err() {
                    break;
                }
            }
            while rx.next().await.is_some() {}
        });
        let snap = snapshot_with_cost(
            &format!("http://{up_addr}/v1"),
            "openai",
            "openai",
            r#","stream_timeout":500"#,
        );
        let (addr, _state, mut usage_rx) = serve(snap).await;
        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut tx, _never_read) = ws.split();
        tx.send(TgMessage::Text(r#"{"type":"response.create"}"#.to_string()))
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(15), usage_rx.recv())
            .await
            .expect("the session ends though its client is still connected")
            .expect("usage sink closed");
        assert_eq!(event.requested_model, "rt-model");
    }

    /// A session the gateway ends itself still reads what the upstream was
    /// already sending, so a response finished meanwhile is billed.
    #[tokio::test]
    async fn a_session_the_gateway_ends_bills_the_response_already_on_its_way() {
        let up_addr = spawn_scripted_upstream(vec![
            delta("response.output_text.delta", "i1", "say FORBIDDEN"),
            serde_json::json!({"type": "response.output_text.done", "item_id": "i1"}),
            serde_json::json!({"type": "response.done", "response": {"usage": {"input_tokens": 7, "output_tokens": 9}}}),
        ])
        .await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let row: aisix_core::Guardrail = serde_json::from_str(KEYWORD_OUTPUT_BLOCK).unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-1", row, 1));
        let (addr, _state, mut usage_rx) = serve(snap).await;
        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        tx.send(TgMessage::Text(r#"{"type":"response.create"}"#.to_string()))
            .await
            .unwrap();
        while let Some(Ok(msg)) = rx.next().await {
            if matches!(msg, TgMessage::Close(_)) {
                break;
            }
        }
        let event = tokio::time::timeout(Duration::from_secs(5), usage_rx.recv())
            .await
            .expect("the session's event")
            .expect("usage sink closed");
        assert_eq!(event.status_code, 400, "refused");
        assert_eq!((event.prompt_tokens, event.completion_tokens), (7, 9));
    }

    /// A literal across two deltas matches neither, so a chain that can block
    /// has to judge the item whole before any of it goes out.
    #[tokio::test]
    async fn a_literal_split_across_deltas_never_reaches_the_client() {
        for (delta_kind, done) in [
            (
                "response.output_text.delta",
                serde_json::json!({"type": "response.output_text.done", "item_id": "i1"}),
            ),
            (
                "response.output_audio_transcript.delta",
                serde_json::json!({"type": "response.output_audio_transcript.done", "item_id": "i1"}),
            ),
            (
                "response.function_call_arguments.delta",
                serde_json::json!({"type": "response.function_call_arguments.done", "item_id": "i1"}),
            ),
        ] {
            let seen = relayed_frames(
                vec![
                    delta(delta_kind, "i1", "say FORB"),
                    delta(delta_kind, "i1", "IDDEN now"),
                    done,
                ],
                KEYWORD_OUTPUT_BLOCK,
            )
            .await;
            assert_eq!(seen.len(), 1, "only the refusal: {seen:?}");
            assert_eq!(seen[0]["type"], "error", "{seen:?}");
            assert_eq!(seen[0]["error"]["code"], "content_filtered");
        }
    }

    /// A clean item goes out whole and in order, and a voice answer's audio
    /// does not wait for it.
    #[tokio::test]
    async fn a_clean_item_is_released_in_order_and_audio_is_not_held() {
        let seen = relayed_frames(
            vec![
                delta("response.output_audio_transcript.delta", "i1", "hel"),
                delta("response.output_audio.delta", "i1", "AAAA"),
                delta("response.output_audio_transcript.delta", "i1", "lo"),
                serde_json::json!({"type": "response.output_audio_transcript.done", "item_id": "i1", "transcript": "hello"}),
                serde_json::json!({"type": "response.done", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
            ],
            KEYWORD_OUTPUT_BLOCK,
        )
        .await;
        let kinds: Vec<_> = seen.iter().map(|f| f["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "response.output_audio.delta",
                "response.output_audio_transcript.delta",
                "response.output_audio_transcript.delta",
                "response.output_audio_transcript.done",
                "response.done",
            ]
        );
    }

    /// An upstream that ends the response without closing the item (a cancelled
    /// response does) still has it judged and released.
    #[tokio::test]
    async fn a_response_ending_without_its_item_done_releases_the_hold() {
        let clean = relayed_frames(
            vec![
                delta("response.output_text.delta", "i1", "partial"),
                serde_json::json!({"type": "response.done", "response": {"status": "cancelled"}}),
            ],
            KEYWORD_OUTPUT_BLOCK,
        )
        .await;
        assert_eq!(clean.len(), 2, "{clean:?}");
        assert_eq!(clean[0]["delta"], "partial");

        let blocked = relayed_frames(
            vec![
                delta("response.output_text.delta", "i1", "FORB"),
                delta("response.output_text.delta", "i1", "IDDEN"),
                serde_json::json!({"type": "response.done", "response": {"status": "cancelled"}}),
            ],
            KEYWORD_OUTPUT_BLOCK,
        )
        .await;
        assert_eq!(blocked.len(), 1, "{blocked:?}");
        assert_eq!(blocked[0]["type"], "error");
    }

    /// A chain that cannot block holds nothing back: it observes.
    #[tokio::test]
    async fn a_monitor_chain_does_not_hold_deltas() {
        let up_addr = spawn_scripted_upstream(vec![delta(
            "response.output_text.delta",
            "i1",
            "never closed",
        )])
        .await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let row: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"m","enabled":true,"hook_point":"output","enforcement_mode":"monitor","kind":"keyword","patterns":[{"kind":"literal","value":"FORBIDDEN"}]}"#,
        )
        .unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-1", row, 1));
        let (addr, _state, _rx) = serve(snap).await;
        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        tx.send(TgMessage::Text(r#"{"type":"response.create"}"#.to_string()))
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(3), rx.next())
            .await
            .expect("a monitor chain relays the delta at once")
            .unwrap()
            .unwrap();
        assert!(matches!(first, TgMessage::Text(t) if t.contains("never closed")));
    }

    /// Past the chain's cap the hold follows the row's overflow policy: refuse,
    /// or release what it has unscanned and stop holding.
    #[test]
    fn the_hold_follows_the_overflow_policy_past_its_cap() {
        let frame = |text: &str| delta("response.output_text.delta", "i1", text).to_string();
        let mut closed = HeldOutput::new(4, false);
        assert!(matches!(closed.observe(frame("abc")), OutputStep::Held));
        assert!(matches!(
            closed.observe(frame("defg")),
            OutputStep::Overflow {
                fail_open: false,
                ..
            }
        ));
        let mut open = HeldOutput::new(4, true);
        assert!(matches!(open.observe(frame("abc")), OutputStep::Held));
        match open.observe(frame("defg")) {
            OutputStep::Overflow { fail_open, frames } => {
                assert!(fail_open);
                assert_eq!(frames.len(), 2, "what it held goes out, in order");
            }
            _ => panic!("expected an overflow"),
        }
        // Only for the rest of that response: the next one is held again.
        assert!(matches!(
            open.observe(frame("more")),
            OutputStep::Release { .. }
        ));
        assert!(matches!(
            open.observe(serde_json::json!({"type": "response.done"}).to_string()),
            OutputStep::Release { .. }
        ));
        assert!(matches!(open.observe(frame("ab")), OutputStep::Held));
    }

    /// `/v1/realtime` builds its upstream handshake by hand rather than
    /// through the shared bridge pipeline, which is exactly where a
    /// per-request mechanism goes silently missing: the operator
    /// configured `forward_client_headers` on this ProviderKey and every
    /// other `/v1/*` endpoint honoured it.
    #[tokio::test]
    async fn a_named_client_header_rides_the_upstream_handshake() {
        let (up_addr, handshake, _frames) = spawn_upstream().await;
        let snap = snapshot_forwarding(
            &format!("http://{up_addr}/v1"),
            &["x-user-jwt", "sec-websocket-extensions", "x-*"],
        );
        let (addr, _state, _rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        req.headers_mut()
            .insert("x-user-jwt", "eyJraw".parse().unwrap());
        // A handshake slot the gateway's own client did NOT set, so
        // nothing would decline it on the way out — only the surface
        // list stops it.
        req.headers_mut().insert(
            "sec-websocket-extensions",
            "permessage-deflate".parse().unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();
        tx.send(TgMessage::Text("{\"type\":\"session.update\"}".into()))
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), client_rx.next()).await;

        let (_uri, seen) = handshake
            .lock()
            .unwrap()
            .clone()
            .expect("handshake recorded");
        assert_eq!(header_str(&seen, "x-user-jwt"), "eyJraw");
        // The caller did not name `authorization`, so the gateway's own
        // provider credential still authenticates the session.
        assert_eq!(header_str(&seen, "authorization"), "Bearer sk-up");
        // A `"*"`-shaped pattern is not consent to relay the handshake
        // this surface owns. `permessage-deflate` at the upstream would
        // enable a compression the gateway's own codec never negotiated,
        // so the relay would decode garbage.
        assert_eq!(
            header_str(&seen, "sec-websocket-extensions"),
            "",
            "the caller's own handshake negotiation must not reach the upstream"
        );
    }

    /// A header value the WebSocket client cannot render as text fails
    /// the whole upstream connection, not just that header — so the
    /// session must still open, minus the one entry. The same value is
    /// forwarded byte-for-byte on every other face.
    ///
    /// Driven over a raw socket rather than through `connect_async`:
    /// tungstenite renders the handshake as text on the way OUT too, so a
    /// tungstenite client cannot send this header at all. A browser can,
    /// and hyper accepts it inbound — which is exactly why the gateway
    /// has to handle it.
    #[tokio::test]
    async fn a_non_ascii_forwarded_value_does_not_sink_the_session() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (up_addr, handshake, _frames) = spawn_upstream().await;
        let snap = snapshot_forwarding(&format!("http://{up_addr}/v1"), &["x-*"]);
        let (addr, _state, _rx) = serve(snap).await;

        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        // Latin-1 `José` in `x-user-name`: legal in a header value, and
        // `x-*` admits it alongside the readable `x-user-jwt`.
        let mut req = Vec::new();
        req.extend_from_slice(b"GET /v1/realtime?model=rt-model HTTP/1.1\r\n");
        req.extend_from_slice(format!("Host: {addr}\r\n").as_bytes());
        req.extend_from_slice(b"Upgrade: websocket\r\nConnection: Upgrade\r\n");
        req.extend_from_slice(b"Sec-WebSocket-Version: 13\r\n");
        req.extend_from_slice(b"Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n");
        req.extend_from_slice(b"Authorization: Bearer sk-caller\r\n");
        req.extend_from_slice(b"x-user-jwt: eyJraw\r\n");
        req.extend_from_slice(b"x-user-name: Jos\xe9\r\n");
        req.extend_from_slice(b"\r\n");
        sock.write_all(&req).await.unwrap();

        let mut buf = [0u8; 256];
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf))
            .await
            .expect("the gateway must answer the upgrade")
            .unwrap();
        let status = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            status.starts_with("HTTP/1.1 101"),
            "one unreadable header must not fail the session, got: {}",
            status.lines().next().unwrap_or_default()
        );

        // The upstream handshake happened, carrying the readable header
        // and not the other one.
        let (_uri, seen) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(h) = handshake.lock().unwrap().clone() {
                    return h;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("upstream handshake recorded");
        assert_eq!(header_str(&seen, "x-user-jwt"), "eyJraw");
        assert!(seen.get("x-user-name").is_none());
    }

    /// The credential collision, on this face as on every other: the
    /// operator declared that this upstream reads the caller's own
    /// credential, so the ProviderKey's stands aside rather than joining
    /// it on the wire.
    #[tokio::test]
    async fn a_named_credential_slot_displaces_the_provider_key_s_own() {
        let (up_addr, handshake, _frames) = spawn_upstream().await;
        let snap = snapshot_forwarding(&format!("http://{up_addr}/v1"), &["authorization"]);
        let (addr, _state, _rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("x-aisix-api-key", "sk-caller".parse().unwrap());
        req.headers_mut()
            .insert("authorization", "Bearer sk-for-upstream".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();
        tx.send(TgMessage::Text("{\"type\":\"session.update\"}".into()))
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), client_rx.next()).await;

        let (_uri, seen) = handshake
            .lock()
            .unwrap()
            .clone()
            .expect("handshake recorded");
        assert_eq!(header_str(&seen, "authorization"), "Bearer sk-for-upstream");
        // And alone: a second value would let the upstream pick.
        assert_eq!(seen.get_all("authorization").iter().count(), 1);
    }

    /// The key the caller authenticated to the gateway with stays home even
    /// when the slot is named: the ProviderKey's secret opens the upstream.
    #[tokio::test]
    async fn the_callers_gateway_key_never_rides_the_upstream_handshake() {
        let (up_addr, handshake, _frames) = spawn_upstream().await;
        let snap = snapshot_forwarding(&format!("http://{up_addr}/v1"), &["authorization"]);
        let (addr, _state, _rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();
        tx.send(TgMessage::Text("{\"type\":\"session.update\"}".into()))
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), client_rx.next()).await;

        let (_uri, seen) = handshake
            .lock()
            .unwrap()
            .clone()
            .expect("handshake recorded");
        assert_eq!(header_str(&seen, "authorization"), "Bearer sk-up");
        assert_eq!(seen.get_all("authorization").iter().count(), 1);
    }

    /// The browser flow puts the caller's own AISIX key in
    /// `sec-websocket-protocol`. Relaying that list would hand the
    /// provider the credential this gateway authenticates with, so no
    /// pattern reaches it — not even one naming it in full.
    #[tokio::test]
    async fn the_browser_credential_never_rides_the_upstream_handshake() {
        let (up_addr, handshake, _frames) = spawn_upstream().await;
        let snap = snapshot_forwarding(
            &format!("http://{up_addr}/v1"),
            &["sec-websocket-protocol", "*"],
        );
        let (addr, _state, _rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.sk-caller"
                .parse()
                .unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("subprotocol auth must be accepted");
        let (mut tx, mut client_rx) = ws.split();
        tx.send(TgMessage::Text("{\"type\":\"session.update\"}".into()))
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), client_rx.next()).await;

        let (_uri, seen) = handshake
            .lock()
            .unwrap()
            .clone()
            .expect("handshake recorded");
        assert!(
            !header_str(&seen, "sec-websocket-protocol").contains("sk-caller"),
            "the caller's gateway key must not reach the provider"
        );
    }

    #[tokio::test]
    async fn subprotocol_key_authenticates_and_realtime_is_echoed() {
        let (up_addr, _handshake, _frames) = spawn_upstream().await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let (addr, _state, _rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        // Browser flow: no headers, key rides the subprotocol list.
        req.headers_mut().insert(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.sk-caller, openai-beta.realtime-v1"
                .parse()
                .unwrap(),
        );
        let (ws, resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("subprotocol auth must be accepted");
        assert_eq!(
            resp.headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok()),
            Some("realtime"),
            "the gateway must echo the `realtime` subprotocol"
        );
        drop(ws);
    }

    /// A browser may split its subprotocol offer across repeated header
    /// fields; the credential must still be found wherever it lands.
    #[tokio::test]
    async fn subprotocol_credential_is_found_in_a_repeated_header_field() {
        let (up_addr, handshake, _frames) = spawn_upstream().await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let (addr, _state, _rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        // Two fields: the credential and the beta opt-in ride the second.
        req.headers_mut()
            .append("sec-websocket-protocol", "realtime".parse().unwrap());
        req.headers_mut().append(
            "sec-websocket-protocol",
            "openai-insecure-api-key.sk-caller, openai-beta.realtime-v1"
                .parse()
                .unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("a credential in a later subprotocol field must authenticate");
        let (mut tx, mut client_rx) = ws.split();
        tx.send(TgMessage::Text(
            serde_json::json!({"type": "session.update"}).to_string(),
        ))
        .await
        .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), client_rx.next()).await;

        // The opt-in rode the same later field, so it reaches upstream.
        let (_uri, seen) = handshake
            .lock()
            .unwrap()
            .clone()
            .expect("handshake recorded");
        assert_eq!(header_str(&seen, "openai-beta"), "realtime=v1");
    }

    /// The predicate itself: neither channel set => GA (no header).
    #[test]
    fn beta_opt_in_is_recognised_on_both_channels() {
        let hm = |k: &'static str, v: &str| {
            let mut h = HeaderMap::new();
            h.insert(k, v.parse().unwrap());
            h
        };
        assert!(!client_requested_beta_realtime(&HeaderMap::new()));
        assert!(client_requested_beta_realtime(&hm(
            "openai-beta",
            "realtime=v1"
        )));
        assert!(client_requested_beta_realtime(&hm(
            "openai-beta",
            "Realtime=v1"
        )));
        // Browser flow: the opt-in rides the subprotocol list.
        assert!(client_requested_beta_realtime(&hm(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.sk-x, openai-beta.realtime-v1"
        )));
        // A plain browser handshake carrying only the credential is GA.
        assert!(!client_requested_beta_realtime(&hm(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.sk-x"
        )));
        // Assistants-style beta values are not the realtime opt-in.
        assert!(!client_requested_beta_realtime(&hm(
            "openai-beta",
            "assistants=v2"
        )));
        // A near match must NOT opt in: the value is a list item, not a
        // substring, so a future `realtime=v10` stays GA.
        assert!(!client_requested_beta_realtime(&hm(
            "openai-beta",
            "realtime=v10"
        )));
        // List-valued: the opt-in counts wherever it sits in the list.
        assert!(client_requested_beta_realtime(&hm(
            "openai-beta",
            "assistants=v2, realtime=v1"
        )));
        // Repeated headers: `HeaderMap::get` would only see the first.
        let mut repeated = HeaderMap::new();
        repeated.append("openai-beta", "assistants=v2".parse().unwrap());
        repeated.append("openai-beta", "realtime=v1".parse().unwrap());
        assert!(client_requested_beta_realtime(&repeated));
        // Same for the subprotocol list, which browsers may also repeat.
        let mut split_proto = HeaderMap::new();
        split_proto.append("sec-websocket-protocol", "realtime".parse().unwrap());
        split_proto.append(
            "sec-websocket-protocol",
            "openai-beta.realtime-v1".parse().unwrap(),
        );
        assert!(client_requested_beta_realtime(&split_proto));
    }

    /// A caller that DOES opt in still gets the beta header forwarded,
    /// so legacy beta clients keep working against upstreams that serve
    /// the beta shape.
    #[tokio::test]
    async fn client_beta_opt_in_is_forwarded_upstream() {
        let (up_addr, handshake, _frames) = spawn_upstream().await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let (addr, _state, _rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        req.headers_mut()
            .insert("openai-beta", "realtime=v1".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();
        tx.send(TgMessage::Text(
            serde_json::json!({"type": "session.update"}).to_string(),
        ))
        .await
        .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), client_rx.next()).await;

        let (_uri, seen) = handshake
            .lock()
            .unwrap()
            .clone()
            .expect("handshake recorded");
        assert_eq!(
            header_str(&seen, "openai-beta"),
            "realtime=v1",
            "an explicit client opt-in must reach the upstream"
        );
    }

    #[tokio::test]
    async fn missing_auth_rejects_the_handshake() {
        let snap = snapshot("http://127.0.0.1:9/v1", "openai", "openai");
        let (addr, _state, _rx) = serve(snap).await;

        let req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        let err = tokio_tungstenite::connect_async(req)
            .await
            .expect_err("handshake must fail without credentials");
        let msg = err.to_string();
        assert!(msg.contains("401"), "expected 401 rejection, got: {msg}");
    }

    #[tokio::test]
    async fn missing_model_param_rejects_with_400() {
        let snap = snapshot("http://127.0.0.1:9/v1", "openai", "openai");
        let (addr, _state, mut rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let err = tokio_tungstenite::connect_async(req)
            .await
            .expect_err("handshake must fail without ?model=");
        assert!(err.to_string().contains("400"), "got: {err}");

        // The failure surfaces in Logs under the SAME protocol tag as a
        // successful realtime session, so protocol filtering catches both.
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("a failed realtime handshake must emit an error UsageEvent")
            .expect("sink closed");
        assert_eq!(ev.status_code, 400);
        assert_eq!(
            ev.inbound_protocol, "realtime",
            "error event must carry the realtime protocol tag, not \"openai\""
        );
    }

    #[tokio::test]
    async fn non_realtime_capable_adapter_is_rejected() {
        let snap = snapshot("http://127.0.0.1:9", "anthropic", "anthropic");
        let (addr, _state, _rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let err = tokio_tungstenite::connect_async(req)
            .await
            .expect_err("handshake must fail on a non-OpenAI-protocol provider");
        assert!(err.to_string().contains("400"), "got: {err}");
    }

    #[tokio::test]
    async fn model_acl_rejects_unauthorized_key() {
        let (up_addr, _h, _f) = spawn_upstream().await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        // Restrict the caller key to a different model.
        let k_json = format!(r#"{{"key_hash":"{CALLER_HASH}","allowed_models":["other-model"]}}"#);
        let k: ApiKey = serde_json::from_str(&k_json).unwrap();
        snap.apikeys.insert(ResourceEntry::new("k-1", k, 2));
        let (addr, state, mut rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let err = tokio_tungstenite::connect_async(req)
            .await
            .expect_err("handshake must fail on model ACL");
        assert!(err.to_string().contains("403"), "got: {err}");

        // Auth succeeded before the ACL refused, so the error event must
        // name the caller — pre-#932 it was emitted as if anonymous.
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("the refusal must emit an error UsageEvent")
            .expect("sink closed");
        assert_eq!(ev.status_code, 403);
        assert_eq!(
            ev.api_key_id, "k-1",
            "a post-auth refusal must attribute the resolved key"
        );

        // Second surface of the same fix: the request-rate metric label
        // set names the caller instead of `unknown`.
        let scrape = state.metrics.render();
        assert!(
            scrape.contains(r#"api_key_id="k-1""#),
            "the refusal metric must carry the caller label, got: {scrape}"
        );
    }

    /// State + router + usage receiver for driving the endpoint's
    /// REJECTION paths with `oneshot` (no live connection needed — the
    /// point is that no upgrade happens).
    fn oneshot_router() -> (
        axum::Router,
        crate::ProxyState,
        tokio::sync::mpsc::Receiver<ObsUsageEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel::<ObsUsageEvent>(4);
        let state = crate::ProxyState::new(
            SnapshotHandle::new(AisixSnapshot::new()),
            Arc::new(Hub::new()),
            &cfg(),
        )
        .without_cache()
        .with_usage_sink(UsageSink::new(tx));
        (crate::build_router(state.clone()), state, rx)
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap_or_default();
        serde_json::from_slice(&bytes).expect("an error envelope, not a bare rejection body")
    }

    #[tokio::test]
    async fn non_websocket_request_is_recorded_and_enveloped() {
        use tower::ServiceExt as _;
        // Pre-#885 a plain GET (no upgrade headers) got axum's bare
        // rejection: nothing in the access log, metrics, or the usage
        // pipeline. It now takes this endpoint's normal error arm —
        // envelope + usage event + request metrics — keeping axum's 400
        // classification for bad/missing upgrade headers.
        let (router, state, mut rx) = oneshot_router();
        let response = router
            .oneshot(
                axum::http::Request::get("/v1/realtime?model=probe-model")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let json = body_json(response).await;
        assert_eq!(json["error"]["type"], "websocket_upgrade_required");

        let event = rx.try_recv().expect("the refusal is recorded");
        assert_eq!(event.status_code, 400);
        assert_eq!(event.inbound_protocol, "realtime");
        // The handler authenticates only after the upgrade is accepted
        // as upgradable, so a rejected upgrade never reaches auth — no
        // key is attributed; the requested model rides along.
        assert_eq!(event.api_key_id, "");
        assert_eq!(event.requested_model, "probe-model");

        // Logs and the request-rate metrics must not disagree about
        // whether these requests exist.
        let scrape = state.metrics.render();
        assert!(
            scrape.contains(r#"status="400""#) && scrape.contains(r#"model="unresolved""#),
            "the refusal must be counted, got: {scrape}"
        );
    }

    #[tokio::test]
    async fn non_upgradable_connection_keeps_its_426() {
        use tower::ServiceExt as _;
        // Correct WebSocket headers over a connection that cannot upgrade
        // (a `oneshot` request carries no hyper upgrade extension) is
        // axum's ConnectionNotUpgradable — 426 Upgrade Required. The
        // status must survive the envelope mapping rather than being
        // flattened to 400, and the response must name the protocol to
        // switch to (RFC 9110 §15.5.22).
        let (router, _state, mut rx) = oneshot_router();
        let response = router
            .oneshot(
                axum::http::Request::get("/v1/realtime")
                    .header("connection", "upgrade")
                    .header("upgrade", "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router responds");
        assert_eq!(
            response.status(),
            axum::http::StatusCode::UPGRADE_REQUIRED,
            "axum's 426 classification must survive"
        );
        assert_eq!(
            response
                .headers()
                .get("upgrade")
                .and_then(|v| v.to_str().ok()),
            Some("websocket")
        );
        let json = body_json(response).await;
        assert_eq!(json["error"]["type"], "websocket_upgrade_required");
        assert_eq!(rx.try_recv().expect("recorded").status_code, 426);
    }

    #[tokio::test]
    async fn head_request_keeps_its_405_and_allow_header() {
        use tower::ServiceExt as _;
        // axum's `get()` also serves HEAD, so a HEAD request reaches the
        // extractor's method check — 405, with the Allow header RFC 9110
        // §15.5.6 requires, and recorded like every other refusal.
        let (router, _state, mut rx) = oneshot_router();
        let response = router
            .oneshot(
                axum::http::Request::head("/v1/realtime")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router responds");
        assert_eq!(
            response.status(),
            axum::http::StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            response
                .headers()
                .get("allow")
                .and_then(|v| v.to_str().ok()),
            Some("GET, HEAD")
        );
        assert_eq!(rx.try_recv().expect("recorded").status_code, 405);
    }

    /// AISIX-Cloud#1330 / #1024: a realtime session's terminal usage
    /// event is emitted once, when the socket closes — including when a
    /// guardrail refused a frame and closed it. The audit handle is read
    /// there rather than at chain resolution because an output-hook mask
    /// can land at any point in the session's life.
    #[tokio::test]
    async fn blocked_frame_names_the_policy_on_the_session_usage_event() {
        let (up_addr, _handshake, _frames) = spawn_upstream().await;
        let snap = snapshot(&format!("http://{up_addr}/v1"), "openai", "openai");
        let g: aisix_core::Guardrail = serde_json::from_str(
            r#"{"name":"test-block","enabled":true,"hook_point":"input","fail_open":false,"kind":"keyword","patterns":[{"kind":"literal","value":"BLOCKME"}]}"#,
        )
        .unwrap();
        crate::seed_env_scoped_guardrail(
            &snap,
            aisix_core::resource::ResourceEntry::new("g-1", g, 1),
        );
        let (addr, _state, mut rx) = serve(snap).await;

        let mut req = format!("ws://{addr}/v1/realtime?model=rt-model")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer sk-caller".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("handshake");
        let (mut tx, mut client_rx) = ws.split();

        tx.send(TgMessage::Text(
            serde_json::json!({"type": "session.update", "session": {"instructions": "please BLOCKME"}})
                .to_string(),
        ))
        .await
        .unwrap();
        // Drain until the gateway closes the socket, so the session's
        // terminal emit has run.
        while let Some(Ok(msg)) = client_rx.next().await {
            if matches!(msg, TgMessage::Close(_)) {
                break;
            }
        }

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("the session must emit a UsageEvent")
            .expect("usage_sink sender dropped");
        assert_eq!(ev.guardrail_enforced_hits.len(), 1, "{ev:?}");
        assert_eq!(ev.guardrail_enforced_hits[0].guardrail_name, "test-block");
        assert_eq!(ev.guardrail_enforced_hits[0].hook, "input");
        assert_eq!(ev.guardrail_enforced_hits[0].action, "blocked");
        let wire = serde_json::to_string(&ev).unwrap();
        assert!(!wire.contains("BLOCKME"), "{wire}");
    }
}
