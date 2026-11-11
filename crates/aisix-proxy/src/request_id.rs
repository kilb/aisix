use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::state::ProxyState;

/// Response header carrying the gateway request id so a client can
/// correlate a response to its usage event (both key on this id).
pub(crate) const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-aisix-request-id");

/// The id minted when the caller supplies none. A UUID rather than
/// something shorter so an id the gateway generated is recognisable as
/// such next to one a caller chose.
pub(crate) fn new_request_id() -> String {
    Uuid::new_v4().to_string()
}

/// The correlation id a fronting proxy stamps on the requests it
/// forwards. `x-request-id` is what Envoy, nginx and the ingresses built
/// on them all emit by default, which is why it is fixed here rather than
/// configurable.
///
/// It is **recorded, never adopted**: whether a caller-supplied id becomes
/// this gateway's own `request_id` stays governed by
/// `proxy.request_id.accept_headers`, which by default takes only
/// `x-aisix-request-id`. So a deployment behind an ingress logs two ids
/// that mean different things — the ingress's `downstream_request_id` and
/// the gateway's `request_id` — and neither can be mistaken for the other.
/// When an operator does list this header in `accept_headers`, the two
/// carry the same value, which is the honest report of what happened.
const DOWNSTREAM_REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Longest caller-supplied request id accepted. Matches the control plane's
/// `maxRequestIDLen`.
const MAX_REQUEST_ID_LEN: usize = 256;

/// Whether a caller-supplied request id may be used as-is.
///
/// MUST stay byte-identical to the control plane's `validRequestID`
/// (the control plane's telemetry ingest). This side decides what
/// to hand back to the caller and stamp on every usage event; that side
/// decides what to persist. If the control plane were stricter, a request the caller
/// was told succeeded — with this id in its `x-aisix-request-id` — would be
/// dropped at telemetry ingest and vanish from billing and /logs, which is
/// the failure #1288 exists to remove.
///
/// Visible ASCII only (0x21..=0x7E): no control characters, no spaces, no
/// non-ASCII. Covers every id shape in the wild (UUID, ULID, `req_abc123`,
/// nginx `$request_id`) while keeping the value safe to echo back onto the
/// wire verbatim — in a response header, in the upstream request, and in a
/// telemetry field.
pub(crate) fn is_acceptable(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_LEN
        && id.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

/// The caller's request id, if they sent an acceptable one in a header the
/// operator allows it from. Headers are consulted in configured order and
/// the first acceptable value wins.
///
/// An unacceptable value is ignored rather than rejected: it arrives from
/// callers whose tracing predates the gateway, and failing their requests
/// over a correlation id would be a worse outcome than correlating them by
/// a minted one. The fallback is logged at debug so an operator chasing a
/// missing id has something to find.
fn client_request_id(
    headers: &axum::http::HeaderMap,
    accept: &[HeaderName],
    auth_slot: Option<&HeaderName>,
) -> Option<String> {
    for name in accept {
        // 只排除当前路由的认证槽，其他路由同名的普通关联头仍可采用。
        if auth_slot == Some(name) {
            continue;
        }
        let Some(raw) = headers.get(name) else {
            continue;
        };
        match raw.to_str() {
            Ok(id) if is_acceptable(id) => return Some(id.to_owned()),
            _ => {
                tracing::debug!(
                    header = %name,
                    "ignoring unusable client request id; minting one instead"
                );
            }
        }
    }
    None
}

/// The fronting proxy's own request id, when it sent an acceptable one.
///
/// Screened by the same [`is_acceptable`] rule as an adopted id: this
/// value is written into logs verbatim, so it has to be visible ASCII of
/// bounded length whether or not the gateway ever hands it back.
fn downstream_request_id<'a>(
    headers: &'a axum::http::HeaderMap,
    auth_slot: Option<&HeaderName>,
) -> Option<&'a str> {
    if auth_slot == Some(&DOWNSTREAM_REQUEST_ID_HEADER) {
        return None;
    }
    headers
        .get(&DOWNSTREAM_REQUEST_ID_HEADER)
        .and_then(|raw| raw.to_str().ok())
        .filter(|id| is_acceptable(id))
}

/// The per-request correlation id, stashed in the request extensions by
/// [`ensure_request_id`] so every handler resolves the SAME id for both
/// its usage event and the response header. Handlers with a
/// [`ClientContext`](crate::client_ip::ClientContext) read it from there;
/// the few that don't take one use an `Extension<RequestId>` extractor.
#[derive(Debug, Clone)]
pub(crate) struct RequestId(pub String);

// 复用真实 Axum 路由表，只探测 typed 路径优先级；三个出口均不执行业务逻辑。
pub(crate) fn typed_route_probe(
    router: axum::Router<ProxyState>,
    state: ProxyState,
) -> axum::Router {
    router
        .fallback(|| async { axum::http::StatusCode::NOT_FOUND })
        .method_not_allowed_fallback(|| async { axum::http::StatusCode::NO_CONTENT })
        .route_layer(axum::middleware::from_fn(|_: Request, _: Next| async {
            axum::http::StatusCode::NO_CONTENT
        }))
        .with_state(state)
}

/// The downstream protocol, as a bounded label for the request span.
///
/// It rides the span alongside `peer` because it is the same kind of
/// fact — a property of the connection rather than of the request — and
/// so reaches the access log, the per-attempt `provider call completed`
/// line, and every diagnostic in between without 29 call sites having to
/// thread it down.
///
/// It is worth a field because the two protocols retire differently
/// during a drain, and an operator sizing
/// `terminationGracePeriodSeconds` has to know which they have: an
/// HTTP/1.1 connection retires itself through `Connection: close` as it
/// is used, while an HTTP/2 one retires on the GOAWAY the listener sends
/// when the drain starts (AISIX-Cloud#1395). Until this field existed
/// nothing the gateway logged said which a deployment was seeing.
///
/// The set is closed, so it is safe as a metric label too.
fn http_version_label(version: axum::http::Version) -> &'static str {
    match version {
        axum::http::Version::HTTP_09 => "0.9",
        axum::http::Version::HTTP_10 => "1.0",
        axum::http::Version::HTTP_11 => "1.1",
        axum::http::Version::HTTP_2 => "2",
        axum::http::Version::HTTP_3 => "3",
        _ => "other",
    }
}

/// Ingress+egress middleware that resolves the request id — the caller's
/// own when they supplied one, a fresh UUID otherwise — and gives every
/// proxied response an `x-aisix-request-id` header derived from the same id
/// the handler attributes its usage event to.
///
/// Reusing the caller's id (#1288) happens HERE, at the single
/// mint point, which is what makes it hold everywhere at once: the response
/// header, every retry/failover attempt's usage event, the access log, the
/// tracing span, and the `x-aisix-request-id` each bridge sends upstream all
/// read this one value. A caller can therefore find a gateway request by an
/// id its own logs already carry, instead of keeping a second mapping.
///
/// The caller's copy of the header is still never forwarded upstream by the
/// `forward_client_headers` allowlist (`x-aisix-` is in
/// `NEVER_FORWARD_PREFIXES`); the bridges insert this resolved id
/// themselves, so the upstream sees exactly one value.
///
/// One shared mechanism instead of a per-handler header insert: the
/// family had drifted (some handlers set it, some didn't — chat /
/// completions / embeddings / responses / messages all shipped without
/// it in v0.3.0), which is exactly the kind of gap the
/// fix-the-whole-class rule exists to prevent. Minting here and reading
/// it back through `ClientContext` keeps the header equal to the
/// telemetry `request_id`, so the header is actually usable for
/// correlation rather than a second, unrelated id.
///
/// It also opens the request-scoped tracing span, so every log line a
/// request emits carries its `request_id` without each call site having
/// to thread one down (#1060). That is what makes a deep
/// diagnostic — e.g. the Aliyun guardrail's `aliyun_request_id` — join
/// back to the `x-aisix-request-id` the caller was handed. The span is
/// attached to the future rather than entered with a guard: a guard held
/// across an await would leak the span onto whatever else the executor
/// runs on this thread.
///
/// Two connection-level identities ride the same span, for the same
/// reason and with the same reach — the access log, the per-attempt
/// `provider call completed` line, and every diagnostic in between:
///
/// - `peer`, the accepted socket's remote address INCLUDING its port — or,
///   when a trusted peer opened the connection with a PROXY protocol
///   header, the source address that header names. It
///   is deliberately not the resolved client IP that
///   [`ClientContext::source_ip`](crate::client_ip::ClientContext) already
///   carries: that one answers "who is the caller", follows
///   `proxy.real_ip`, and has no port. This one answers "which TCP
///   connection", and the port is the whole of its value — behind a
///   layer-4 load balancer with the gateway on the host network it is the
///   only field that joins a gateway log line to the fronting proxy's
///   record of the same connection.
/// - `downstream_request_id`, from [`DOWNSTREAM_REQUEST_ID_HEADER`], so a
///   trace id minted before the gateway can be searched for here.
///
/// Both are recorded rather than declared, so a request that carried
/// neither logs neither field instead of an empty one.
///
/// `http_version` is the third, and it is always known, so it is
/// declared. See [`http_version_label`] for why it is worth a field.
///
/// Response-body streams (SSE) are polled after this middleware returns,
/// so they fall outside the span. Generators that moderate streamed
/// output re-attach it explicitly — see `chat::build_sse_stream`.
pub(crate) async fn ensure_request_id(
    State((state, typed_routes)): State<(ProxyState, axum::Router)>,
    mut request: Request,
    next: Next,
) -> Response {
    // span 创建前按实际改写与选路规则识别认证槽，adoption 和 capture 共用结果。
    let uri = crate::rewrite::rewritten_uri(&state.url_rewrites, &request);
    let host = crate::host::inbound_host(&request);
    let snapshot = state.snapshot.load();
    let auth_slot = crate::passthrough_route::request_auth_header(
        &snapshot,
        request.method(),
        host.as_deref(),
        &uri,
        &typed_routes,
    )
    .await;
    request
        .extensions_mut()
        .insert(crate::passthrough_route::RouteSnapshot(snapshot));
    let id = request
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .or_else(|| {
            client_request_id(
                request.headers(),
                &state.request_id_accept,
                auth_slot.as_ref(),
            )
        })
        .unwrap_or_else(new_request_id);
    request.extensions_mut().insert(RequestId(id.clone()));

    // The request's trace identity (AISIX-Cloud#1279), minted at the same
    // single point as the request id so every usage event and exported
    // span of this request shares one trace. A valid inbound W3C
    // `traceparent` continues the caller's trace; anything malformed —
    // or a duplicated header — degrades to a locally-rooted one, and the
    // caller's `tracestate` is only kept alongside a valid parent.
    let remote = remote_trace_context(request.headers(), auth_slot.as_ref());
    let http = server_span_request(&request);
    request.extensions_mut().insert(std::sync::Arc::new(
        aisix_obs::RequestTraceBundle::new(remote).with_http_server(http),
    ));

    // The id may be the caller's own, so it is written through `LogValue`:
    // as is when it is an ordinary id, quoted when it could read as fields of
    // the line, the same reason `downstream_request_id` below is a string.
    let span = tracing::info_span!(
        "request",
        request_id = %aisix_core::LogValue(&id),
        http_version = http_version_label(request.version()),
        peer = tracing::field::Empty,
        downstream_request_id = tracing::field::Empty,
    );
    if let Some(peer) = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
    {
        span.record("peer", tracing::field::display(peer.0));
    }
    if let Some(downstream) = downstream_request_id(request.headers(), auth_slot.as_ref()) {
        // Recorded as a string, not `display`: the value is caller-supplied
        // and `is_acceptable` permits `{`, `}` and `:` — the characters the
        // fmt subscriber delimits a span's field list with. As a string it
        // is rendered quoted and escaped, so it cannot forge one.
        span.record("downstream_request_id", downstream);
    }
    let mut response = next.run(request).instrument(span).await;

    // If the handler already stamped the header (from the same id), keep
    // it; otherwise stamp it here so no response is ever without one.
    if !response.headers().contains_key(&REQUEST_ID_HEADER) {
        if let Ok(hv) = HeaderValue::from_str(&id) {
            response.headers_mut().insert(REQUEST_ID_HEADER, hv);
        }
    }
    response
}

/// The scheme of the listener a request arrived on — `https` behind a
/// `tls` block, else `http`. Inserted per listener by the server; read for
/// the SERVER span's `url.scheme`.
#[derive(Debug, Clone, Copy)]
pub struct DownstreamScheme(pub &'static str);

/// The inbound request as the SERVER span reports it. `http.route` is set
/// only when the path is itself one of the gateway's static routes: every
/// other path is matched by prefix or parameter, and the HTTP conventions
/// forbid reporting a raw path as a route.
fn server_span_request(request: &Request) -> aisix_obs::HttpServerFacts {
    let path = request.uri().path();
    let label = crate::normalize_endpoint_label(path);
    aisix_obs::HttpServerFacts::new(
        request.method().as_str(),
        path,
        (label == path).then_some(label),
        request.extensions().get::<DownstreamScheme>().map(|s| s.0),
    )
    .with_query(request.uri().query())
}

/// The caller's W3C trace context, when it arrived intact.
///
/// Strict on purpose — this value decides what the gateway's own
/// telemetry is filed under. Exactly one `traceparent` header (the spec
/// invalidates a duplicated one), parsed by the strict v00 grammar in
/// `aisix_obs::parse_traceparent`; `tracestate` (spec: multiple headers
/// combine as a comma-joined list) is screened and kept only when the
/// parent it travelled with was valid. Invalid → `None` → a
/// locally-rooted trace, never a rejected request.
fn remote_trace_context(
    headers: &axum::http::HeaderMap,
    auth_slot: Option<&HeaderName>,
) -> Option<aisix_obs::RemoteTraceContext> {
    // 合法的 trace 形状也可能是当前路由的凭据，不能将其复制进关联元数据。
    if auth_slot.is_some_and(|name| name.as_str() == aisix_obs::TRACEPARENT_HEADER) {
        return None;
    }
    let mut parents = headers.get_all(aisix_obs::TRACEPARENT_HEADER).iter();
    let parent = parents.next()?;
    if parents.next().is_some() {
        tracing::debug!("multiple traceparent headers; starting a local trace");
        return None;
    }
    let Some((trace_id, parent_span_id, flags)) =
        parent.to_str().ok().and_then(aisix_obs::parse_traceparent)
    else {
        tracing::debug!("unusable traceparent header; starting a local trace");
        return None;
    };
    let tracestate = if auth_slot.is_some_and(|name| name.as_str() == aisix_obs::TRACESTATE_HEADER)
    {
        None
    } else {
        // All-or-nothing: joining only the readable values would forward a
        // list the caller never sent. One unreadable value discards the
        // whole tracestate (the traceparent stays valid — the spec treats
        // state as optional refinement).
        let values: Result<Vec<&str>, _> = headers
            .get_all(aisix_obs::TRACESTATE_HEADER)
            .iter()
            .map(|v| v.to_str())
            .collect();
        match values {
            Ok(values) if !values.is_empty() => aisix_obs::screen_tracestate(&values.join(",")),
            _ => None,
        }
    };
    Some(aisix_obs::RemoteTraceContext {
        trace_id,
        parent_span_id,
        flags,
        tracestate,
    })
}

/// Stream adapter that enters `span` for the duration of every
/// `poll_next`.
struct InSpan<T> {
    inner: std::pin::Pin<Box<dyn futures::Stream<Item = T> + Send>>,
    span: tracing::Span,
}

impl<T> futures::Stream for InSpan<T> {
    type Item = T;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let span = self.span.clone();
        let _entered = span.enter();
        self.inner.as_mut().poll_next(cx)
    }
}

/// Re-attach the caller's current tracing span to a response-body stream.
///
/// A streamed response is polled by hyper AFTER [`ensure_request_id`] has
/// returned, so its span is no longer active by then: without this, every
/// log event a generator emits — notably the output-guardrail checks that
/// run at end-of-stream — lands outside the request span and loses its
/// `request_id` (#1060).
///
/// MUST be called while still inside the handler, since it captures
/// [`tracing::Span::current`] at construction time — calling it from
/// somewhere the request span isn't active silently attaches a no-op span
/// and correlation is lost with no error.
///
/// Entering inside `poll_next` rather than holding a guard across the
/// generator's awaits is what `tracing`'s own `Instrumented` future does:
/// `poll_next` is synchronous, so the guard cannot leak onto whatever the
/// executor runs next on this thread.
pub(crate) fn in_request_span<T: 'static>(
    stream: impl futures::Stream<Item = T> + Send + 'static,
) -> impl futures::Stream<Item = T> + Send + 'static {
    InSpan {
        inner: Box::pin(stream),
        span: tracing::Span::current(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ProxyConfig, RequestIdConfig};
    use aisix_gateway::Hub;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// The label is a metric-safe closed set, and the two versions that
    /// actually reach the proxy listener have to be told apart — that is
    /// the whole reason the field exists.
    #[test]
    fn http_version_labels_are_a_closed_set() {
        assert_eq!(http_version_label(axum::http::Version::HTTP_11), "1.1");
        assert_eq!(http_version_label(axum::http::Version::HTTP_2), "2");
        assert_eq!(http_version_label(axum::http::Version::HTTP_10), "1.0");
        assert_eq!(http_version_label(axum::http::Version::HTTP_09), "0.9");
        assert_eq!(http_version_label(axum::http::Version::HTTP_3), "3");
    }

    fn state_accepting(accept_headers: &[&str]) -> ProxyState {
        ProxyState::new(
            SnapshotHandle::new(AisixSnapshot::new()),
            Arc::new(Hub::new()),
            &ProxyConfig {
                addr: "127.0.0.1:0".into(),
                request_body_limit_bytes: 1_048_576,
                tls: None,
                listeners: Vec::new(),
                real_ip: Default::default(),
                request_id: RequestIdConfig {
                    accept_headers: accept_headers.iter().map(|s| (*s).to_owned()).collect(),
                },
                thread_per_core: None,
                workers: None,
                url_rewrites: Vec::new(),
                expose_debug_headers: false,
                platform: Default::default(),
            },
        )
    }

    /// The shipped default: the gateway's own header, nothing else.
    fn default_state() -> ProxyState {
        state_accepting(
            &RequestIdConfig::default()
                .accept_headers
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn a_downstream_auth_slot_is_not_captured() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            DOWNSTREAM_REQUEST_ID_HEADER,
            HeaderValue::from_static("synthetic-id"),
        );
        assert_eq!(downstream_request_id(&headers, None), Some("synthetic-id"));
        assert!(downstream_request_id(&headers, Some(&DOWNSTREAM_REQUEST_ID_HEADER)).is_none());
        assert_eq!(
            downstream_request_id(&headers, Some(&REQUEST_ID_HEADER)),
            Some("synthetic-id")
        );
    }

    // 探针复用真实路由表，但任何匹配结果都不能触发原 handler 或 fallback。
    #[tokio::test]
    async fn the_route_probe_never_executes_a_handler() {
        async fn never() -> &'static str {
            panic!("the route probe executed a real handler")
        }
        let router = Router::new()
            .route("/typed", get(never))
            .route("/any", axum::routing::any(never))
            .fallback(never)
            .method_not_allowed_fallback(never);
        let probe = typed_route_probe(router, default_state());
        for (method, path, status) in [
            ("GET", "/typed", 204),
            ("POST", "/typed", 204),
            ("TRACE", "/any", 204),
            ("GET", "/missing", 404),
        ] {
            let response = probe
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status, "{method} {path}");
        }
    }

    fn reload_snapshot(upstream: &str, slot: Option<&str>, host: Option<&str>) -> AisixSnapshot {
        let snapshot = AisixSnapshot::new();
        snapshot.apikeys.insert(aisix_core::ResourceEntry::new(
            "caller",
            serde_json::from_value::<aisix_core::ApiKey>(serde_json::json!({
                "key_hash": "8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891",
                "allowed_models": ["*"], "allowed_routes": ["*"],
            }))
            .unwrap(),
            1,
        ));
        snapshot.provider_keys.insert(aisix_core::ResourceEntry::new("pk", serde_json::from_value::<aisix_core::ProviderKey>(serde_json::json!({
            "display_name": "pk", "provider": "openai", "adapter": "openai", "api_key": "sk-upstream", "api_base": upstream,
        })).unwrap(), 1));
        if let Some(slot) = slot {
            let mut route = serde_json::json!({
                "name": "reload-route", "enabled": true, "target_url": format!("{upstream}/v1"),
                "provider_key_id": "pk", "auth_mode": "header_key", "auth_header_name": slot,
            });
            if let Some(host) = host {
                route["hosts"] = serde_json::json!([host]);
            } else {
                route["path_prefix"] = serde_json::json!("/r111");
            }
            snapshot
                .passthrough_routes
                .insert(aisix_core::ResourceEntry::new(
                    "route",
                    serde_json::from_value::<aisix_core::PassthroughRoute>(route).unwrap(),
                    1,
                ));
        }
        snapshot
    }

    // 在 admission 与 host_dispatch 之间确定性发布 B，避免靠调度时机碰撞竞态。
    fn app_reloading_after_admission(state: ProxyState, replacement: AisixSnapshot) -> Router {
        let typed = Router::new()
            .route("/typed", get(echo_extension_id))
            .fallback(crate::passthrough_route::entry);
        let probe = typed_route_probe(typed.clone(), state.clone());
        let host_entry = crate::apply_shared_proxy_layers(
            Router::new().fallback(crate::passthrough_route::entry),
            &state,
        )
        .with_state(state.clone());
        let routes = crate::apply_shared_proxy_layers(typed, &state).with_state(state.clone());
        let publish = state.clone();
        Router::new()
            .fallback_service(routes)
            .layer(axum::middleware::from_fn_with_state(
                (state.clone(), host_entry),
                crate::passthrough_route::host_dispatch,
            ))
            .layer(axum::middleware::from_fn(
                move |request: Request, next: Next| {
                    let publish = publish.clone();
                    let replacement = replacement.clone();
                    async move {
                        publish.snapshot.store(replacement);
                        next.run(request).await
                    }
                },
            ))
            .layer(axum::middleware::from_fn_with_state(
                (state, probe),
                ensure_request_id,
            ))
    }

    #[tokio::test]
    async fn a_reload_cannot_change_the_credential_slot_after_admission() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/probe"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"peer": true})),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let state = default_state();
        state
            .snapshot
            .store(reload_snapshot(&upstream.uri(), Some("x-old-auth"), None));
        let app = app_reloading_after_admission(
            state,
            reload_snapshot(&upstream.uri(), Some("x-aisix-request-id"), None),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/r111/probe")
                    .header("x-old-auth", "sk-caller")
                    .header("x-aisix-request-id", "ordinary-before-reload")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers()[REQUEST_ID_HEADER],
            "ordinary-before-reload"
        );
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"peer": true})
        );
        upstream.verify().await;
    }

    #[tokio::test]
    async fn a_reload_cannot_add_a_host_route_after_admission() {
        let upstream = wiremock::MockServer::builder().start().await;
        let state = default_state();
        state
            .snapshot
            .store(reload_snapshot(&upstream.uri(), None, None));
        let app = app_reloading_after_admission(
            state,
            reload_snapshot(
                &upstream.uri(),
                Some("x-aisix-request-id"),
                Some("r111-new.example"),
            ),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/typed")
                    .header("host", "r111-new.example")
                    .header("x-aisix-request-id", "ordinary-before-reload")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"ordinary-before-reload");
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    async fn trace_context_with_slot(slot: &str, path: &str) -> (bool, bool) {
        async fn trace_flags(request: Request) -> axum::Json<serde_json::Value> {
            let trace = request
                .extensions()
                .get::<Arc<aisix_obs::RequestTraceBundle>>()
                .unwrap();
            let emitted = trace.emission(true, 0, 0, false);
            axum::Json(
                serde_json::json!({"remote": trace.has_remote_parent(), "state": emitted.tracestate.is_some()}),
            )
        }
        let state = default_state();
        state
            .snapshot
            .store(reload_snapshot("http://127.0.0.1:9", Some(slot), None));
        let router = Router::new()
            .route("/typed", get(trace_flags))
            .fallback(trace_flags);
        let probe = typed_route_probe(router.clone(), state.clone());
        let app = router
            .with_state(state.clone())
            .layer(axum::middleware::from_fn_with_state(
                (state, probe),
                ensure_request_id,
            ));
        let response = app
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(
                        "traceparent",
                        "00-11111111111111111111111111111111-2222222222222222-01",
                    )
                    .header("tracestate", "vendor=synthetic")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let flags: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (
            flags["remote"].as_bool().unwrap(),
            flags["state"].as_bool().unwrap(),
        )
    }

    #[tokio::test]
    async fn a_traceparent_auth_slot_does_not_become_a_remote_parent() {
        assert_eq!(
            trace_context_with_slot("traceparent", "/r111/probe").await,
            (false, false)
        );
        assert_eq!(
            trace_context_with_slot("traceparent", "/typed").await,
            (true, true)
        );
    }

    #[tokio::test]
    async fn a_tracestate_auth_slot_does_not_become_trace_metadata() {
        assert_eq!(
            trace_context_with_slot("tracestate", "/r111/probe").await,
            (true, false)
        );
        assert_eq!(
            trace_context_with_slot("tracestate", "/typed").await,
            (true, true)
        );
    }

    /// R-111: a passthrough route's auth header is authentication, not a
    /// correlation id. Adopting it writes the credential into the span and
    /// the request id, which logs and telemetry would export.
    #[tokio::test]
    async fn passthrough_auth_headers_are_excluded_from_request_id_adoption() {
        use aisix_core::{PassthroughRoute, ResourceEntry};
        let snap = aisix_core::AisixSnapshot::new();
        snap.passthrough_routes.insert(ResourceEntry::new(
            "route-1",
            serde_json::from_value::<PassthroughRoute>(serde_json::json!({
                "name": "route-1",
                "enabled": true,
                "path_prefix": "/passthrough",
                "target_url": "http://localhost",
                "provider_key_id": "pk-1",
                "auth_mode": "header_key",
                "auth_header_name": "X-Secret-Token",
            }))
            .unwrap(),
            1,
        ));
        let state = state_accepting(&["X-Request-ID", "X-Secret-Token"]);
        state.snapshot.store(snap);
        let req = Request::builder()
            .uri("/passthrough/endpoint")
            .header("X-Secret-Token", "secret-credential-value")
            .body(axum::body::Body::empty())
            .unwrap();
        let typed_routes = typed_route_probe(
            Router::new().route("/", get(echo_extension_id)),
            state.clone(),
        );
        let slot = crate::passthrough_route::request_auth_header(
            &state.snapshot.load(),
            req.method(),
            crate::host::inbound_host(&req).as_deref(),
            req.uri(),
            &typed_routes,
        )
        .await;
        let adopted = client_request_id(req.headers(), &state.request_id_accept, slot.as_ref());
        assert!(
            adopted.is_none(),
            "the auth header must not be adopted as a request id"
        );
    }

    fn app_with(state: ProxyState) -> Router {
        let router = Router::new().route("/", get(echo_extension_id));
        let typed_routes = typed_route_probe(router.clone(), state.clone());
        router
            .with_state(state.clone())
            .layer(axum::middleware::from_fn_with_state(
                (state, typed_routes),
                ensure_request_id,
            ))
    }

    /// Drive one request and return (response header, id the handler saw).
    async fn run(app: Router, headers: &[(&str, &str)]) -> (String, String) {
        let mut builder = Request::builder().uri("/");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let resp = app
            .oneshot(builder.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let header = resp
            .headers()
            .get(&REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .expect("response must carry x-aisix-request-id");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (header, String::from_utf8(body.to_vec()).unwrap())
    }

    // A handler that echoes the RequestId it sees in the extensions, so
    // the test can prove the middleware exposes the SAME id it stamps on
    // the response header (header == telemetry id).
    async fn echo_extension_id(request: Request) -> Response {
        let seen = request
            .extensions()
            .get::<RequestId>()
            .map(|r| r.0.clone())
            .unwrap_or_default();
        seen.into_response()
    }

    async fn sets_own_header() -> Response {
        let mut resp = "ok".into_response();
        resp.headers_mut()
            .insert(REQUEST_ID_HEADER, HeaderValue::from_static("handler-set"));
        resp
    }

    #[tokio::test]
    async fn stamps_header_and_matches_the_extension_id() {
        let (header, seen_by_handler) = run(app_with(default_state()), &[]).await;
        assert!(
            uuid::Uuid::parse_str(&header).is_ok(),
            "stamped id must be a UUID, got {header:?}"
        );
        assert_eq!(
            header, seen_by_handler,
            "response header must equal the id the handler saw (correlation contract)"
        );
    }

    // #1288. The caller's id must become THE id — what the
    // handler attributes its usage event to AND what comes back on the
    // response — not merely be echoed back while telemetry uses another.
    #[tokio::test]
    async fn reuses_an_acceptable_client_request_id() {
        for id in [
            "req_abc123",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "9f8c2b1e4d7a6f3c0b5e8d1a2c4f7b9e",
            "4d1f0f4e-9b6a-4a1e-9a0b-1c2d3e4f5a6b",
        ] {
            let (header, seen_by_handler) =
                run(app_with(default_state()), &[("x-aisix-request-id", id)]).await;
            assert_eq!(header, id, "response header must be the caller's id");
            assert_eq!(
                seen_by_handler, id,
                "the handler (and so the usage event) must see the caller's id"
            );
        }
    }

    // An id the gateway cannot use must NOT fail the request — it degrades
    // to a minted UUID, which is the pre-#1288 behaviour.
    #[tokio::test]
    async fn unusable_client_request_ids_fall_back_to_a_minted_uuid() {
        let too_long = "a".repeat(MAX_REQUEST_ID_LEN + 1);
        for id in ["", "req abc", "req\tabc", "请求-1", too_long.as_str()] {
            let (header, seen_by_handler) =
                run(app_with(default_state()), &[("x-aisix-request-id", id)]).await;
            assert!(
                uuid::Uuid::parse_str(&header).is_ok(),
                "unusable id {id:?} must degrade to a minted UUID, got {header:?}"
            );
            assert_eq!(header, seen_by_handler);
        }
    }

    // `x-request-id` is stamped by every ingress in front of the gateway,
    // so honouring it is opt-in: on the default config it must be ignored.
    #[tokio::test]
    async fn x_request_id_is_ignored_unless_configured() {
        let (header, _) = run(
            app_with(default_state()),
            &[("x-request-id", "from-the-ingress")],
        )
        .await;
        assert_ne!(header, "from-the-ingress");
        assert!(uuid::Uuid::parse_str(&header).is_ok());

        let (header, seen_by_handler) = run(
            app_with(state_accepting(&["x-aisix-request-id", "x-request-id"])),
            &[("x-request-id", "from-the-ingress")],
        )
        .await;
        assert_eq!(header, "from-the-ingress");
        assert_eq!(seen_by_handler, "from-the-ingress");
    }

    // Configured order is priority order: the gateway's own header wins
    // over the standard one when a caller sends both.
    #[tokio::test]
    async fn first_configured_header_wins() {
        let (header, _) = run(
            app_with(state_accepting(&["x-aisix-request-id", "x-request-id"])),
            &[
                ("x-request-id", "from-the-ingress"),
                ("x-aisix-request-id", "from-the-caller"),
            ],
        )
        .await;
        assert_eq!(header, "from-the-caller");

        // ...and an unusable value in the first header does not shadow a
        // good value in the next one.
        let (header, _) = run(
            app_with(state_accepting(&["x-aisix-request-id", "x-request-id"])),
            &[
                ("x-request-id", "from-the-ingress"),
                ("x-aisix-request-id", "bad value"),
            ],
        )
        .await;
        assert_eq!(header, "from-the-ingress");
    }

    // An operator can refuse caller-supplied ids outright.
    #[tokio::test]
    async fn empty_accept_headers_always_mints() {
        let (header, _) = run(
            app_with(state_accepting(&[])),
            &[("x-aisix-request-id", "req_abc123")],
        )
        .await;
        assert_ne!(header, "req_abc123");
        assert!(uuid::Uuid::parse_str(&header).is_ok());
    }

    #[tokio::test]
    async fn preserves_a_handler_set_header() {
        let state = default_state();
        let router = Router::new().route("/", get(sets_own_header));
        let typed_routes = typed_route_probe(router.clone(), state.clone());
        let app = router
            .with_state(state.clone())
            .layer(axum::middleware::from_fn_with_state(
                (state, typed_routes),
                ensure_request_id,
            ));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.headers().get(&REQUEST_ID_HEADER).unwrap(),
            "handler-set",
            "middleware must not clobber a header the handler already set"
        );
    }
}
