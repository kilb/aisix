//! Responses WebSocket：每轮走完整请求链，原生派发保留连接内的上游状态。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_gateway::upstream_io::{Protocol, UpstreamIoObserver};
use aisix_gateway::BridgeError;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::{protocol::Role, Message as UpMessage};
use tower::ServiceExt;
use tracing::Instrument;

use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::state::ProxyState;

type UpSocket = tokio_tungstenite::WebSocketStream<reqwest::Upgraded>;
const MAX_LANES: usize = 32;
const QUEUED_PER_LANE: usize = 8;
const EVENT_QUEUE: usize = 8;
const UPSTREAM_EVENT_QUEUE: usize = 64;
const ACTIVE_RESPONSES: usize = 16;
const UPSTREAM_CONNECTIONS: usize = 16;

/// 在实际套接字接受消息之后提交标量观测，不能把等待 ready 的负载算成已发送。
pub(crate) struct ObservedUpstreamSocket<S> {
    inner: S,
    io: UpstreamIoObserver,
}

impl<S> ObservedUpstreamSocket<S> {
    pub(crate) fn new(inner: S, io: UpstreamIoObserver) -> Self {
        Self { inner, io }
    }
}

impl<S: futures::Stream + Unpin> futures::Stream for ObservedUpstreamSocket<S> {
    type Item = S::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }
}

impl<S: futures::Sink<UpMessage> + Unpin> futures::Sink<UpMessage> for ObservedUpstreamSocket<S> {
    type Error = S::Error;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_ready(cx)
    }

    fn start_send(self: std::pin::Pin<&mut Self>, message: UpMessage) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let prepared = match &message {
            UpMessage::Text(text) => Some(this.io.prepare_request_message(text.as_bytes())),
            UpMessage::Binary(bytes) => Some(this.io.prepare_request_message(bytes)),
            _ => None,
        };
        std::pin::Pin::new(&mut this.inner).start_send(message)?;
        if let Some(prepared) = prepared {
            this.io.observe_prepared_request_message(prepared);
        }
        Ok(())
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

#[derive(Default)]
struct SessionConnection {
    upstreams: HashMap<String, Arc<TargetConnection>>,
}

type TargetConnection = Mutex<Option<Arc<UpstreamConnection>>>;

impl SessionConnection {
    fn target(&mut self, identity: String) -> Result<Arc<TargetConnection>, BridgeError> {
        if let Some(slot) = self.upstreams.get(&identity) {
            return Ok(slot.clone());
        }
        if self.upstreams.len() >= UPSTREAM_CONNECTIONS {
            // 有调用持有或等待的握手槽也占配额；失败或取消后空槽可以回收。
            self.upstreams.retain(|_, slot| {
                Arc::strong_count(slot) > 1
                    || slot
                        .try_lock()
                        .is_ok_and(|slot| slot.as_ref().is_some_and(|up| up.is_open()))
            });
            if self.upstreams.len() >= UPSTREAM_CONNECTIONS {
                return Err(BridgeError::Config(
                    "WebSocket upstream connection budget exhausted; open a new client connection"
                        .into(),
                ));
            }
        }
        let slot = Arc::new(Mutex::new(None));
        self.upstreams.insert(identity, slot.clone());
        Ok(slot)
    }
}

struct UpstreamConnection {
    commands: mpsc::Sender<UpstreamCommand>,
    task: tokio::task::AbortHandle,
}

impl UpstreamConnection {
    fn is_open(&self) -> bool {
        !self.task.is_finished() && !self.commands.is_closed()
    }
}

impl Drop for UpstreamConnection {
    fn drop(&mut self) {
        // 已关闭命令通道的任务只剩有界收尾，不能中断其自动 Close 回复。
        if !self.commands.is_closed() {
            self.task.abort();
        }
    }
}

enum UpstreamCommand {
    Create {
        lane: Option<String>,
        token: String,
        payload: String,
        output: mpsc::Sender<String>,
        expires: Option<tokio::time::Instant>,
        io: UpstreamIoObserver,
    },
    Cancel {
        lane: Option<String>,
        token: String,
    },
}

struct UpstreamRound {
    token: String,
    response_id: Option<String>,
    output: mpsc::Sender<String>,
    io: UpstreamIoObserver,
    terminal: bool,
}

impl Drop for UpstreamRound {
    fn drop(&mut self) {
        if !self.terminal {
            self.io.fail_response();
        }
    }
}

struct CancelRound {
    upstream: Arc<UpstreamConnection>,
    lane: Option<String>,
    token: String,
}

impl Drop for CancelRound {
    fn drop(&mut self) {
        // 终态前释放流时关闭共享连接，不能让迟到事件进入同lane的下一轮。
        if matches!(
            self.upstream.commands.try_send(UpstreamCommand::Cancel {
                lane: self.lane.clone(),
                token: self.token.clone(),
            }),
            Err(mpsc::error::TrySendError::Full(_))
        ) {
            self.upstream.task.abort();
        }
    }
}

#[derive(Clone)]
struct RoundTransport {
    connection: Arc<Mutex<SessionConnection>>,
    stream_id: Option<String>,
    create_attempted: Arc<AtomicBool>,
}

tokio::task_local! {
    static ROUND: RoundTransport;
}

pub(crate) fn may_retry() -> bool {
    ROUND
        .try_with(|round| !round.create_attempted.load(Ordering::Acquire))
        .unwrap_or(true)
}

/// 从已审查、已重写的原生 HTTP 请求建立同一账号边界的 WebSocket。
pub(crate) async fn send_native(
    request: reqwest::RequestBuilder,
    deadline: Option<Duration>,
    started: Instant,
) -> Result<reqwest::Response, BridgeError> {
    let Some(round) = ROUND.try_with(Clone::clone).ok() else {
        return crate::stream_timeout::send_with_deadline_observed(
            request,
            deadline,
            started,
            Protocol::Responses,
        )
        .await;
    };
    let (client, built) = request.build_split();
    let request = built.map_err(|e| crate::dispatch::reqwest_error_to_bridge(&e, started))?;
    aisix_gateway::response_tap::note_websocket_origin(
        request.url().scheme(),
        &request.url()[url::Position::BeforeHost..url::Position::BeforePath],
    );
    let mut event: Value = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .and_then(|b| serde_json::from_slice(b).ok())
        .ok_or_else(|| BridgeError::Config("native WebSocket request must be JSON".into()))?;
    let object = event
        .as_object_mut()
        .ok_or_else(|| BridgeError::Config("native WebSocket request must be an object".into()))?;
    object.remove("stream");
    object.insert("type".into(), Value::String("response.create".into()));
    if let Some(id) = &round.stream_id {
        object.insert("stream_id".into(), Value::String(id.clone()));
    }
    let payload = serde_json::to_string(&event)
        .map_err(|_| BridgeError::Config("native WebSocket request cannot be encoded".into()))?;
    // 账号、凭据和URL任一改变都不复用旧连接；不把凭据写入错误或日志。
    let mut identity = Sha256::new();
    identity.update(request.url().as_str());
    for (name, value) in request.headers() {
        if name == "x-aisix-request-id" {
            continue;
        }
        identity.update([0]);
        identity.update(name.as_str());
        identity.update([0]);
        identity.update(value.as_bytes());
    }
    let target = format!("{:x}", identity.finalize());
    let expires =
        deadline.map(|limit| tokio::time::Instant::now() + limit.saturating_sub(started.elapsed()));
    let connect = connect_target(&round.connection, target, client, request, started, expires);
    let handshake = match expires {
        Some(at) => tokio::time::timeout_at(at, connect)
            .await
            .map_err(|_| handshake_timeout(started))?,
        None => connect.await,
    }?;
    let upstream = match handshake {
        Handshake::Ready(upstream) => upstream,
        Handshake::Rejected(response) => return Ok(response),
    };
    // 已就绪的 future 可以在 timeout_at 到点时先被轮询，过期轮不能再投递 create。
    if expires.is_some_and(|at| at <= tokio::time::Instant::now()) {
        return Err(handshake_timeout(started));
    }
    let token = crate::request_id::new_request_id();
    let (output, mut events) = mpsc::channel::<String>(UPSTREAM_EVENT_QUEUE);
    // 每轮句柄随命令进入连接任务；共享套接字不能读取任务局部的“最新”观测。
    let io = UpstreamIoObserver::websocket(Protocol::Responses);
    round.create_attempted.store(true, Ordering::Release);
    let send = upstream.commands.send(UpstreamCommand::Create {
        lane: round.stream_id.clone(),
        token: token.clone(),
        payload,
        output,
        expires,
        io: io.clone(),
    });
    let sent = match expires {
        Some(at) => tokio::time::timeout_at(at, send)
            .await
            .is_ok_and(|result| result.is_ok()),
        None => send.await.is_ok(),
    };
    if !sent {
        io.fail_request();
        io.fail_response();
        if !upstream.commands.is_closed() {
            upstream.task.abort();
        }
        return Ok(stream_failure_response());
    }
    let cancel = CancelRound {
        upstream,
        lane: round.stream_id.clone(),
        token,
    };
    let stream = async_stream::stream! {
        let _cancel = cancel;
        while let Some(text) = events.recv().await {
            let terminal = serde_json::from_str::<Value>(&text).ok().is_some_and(|event| is_terminal(&event));
            yield Ok::<Bytes, std::io::Error>(native_event_frame(&text));
            if terminal { return; }
        }
        yield Ok(Bytes::from_static(FAILED_STREAM));
    };
    let mut response = axum::http::Response::new(reqwest::Body::wrap_stream(
        crate::request_id::in_request_span(stream),
    ));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    Ok(response.into())
}

enum Handshake {
    Ready(Arc<UpstreamConnection>),
    Rejected(reqwest::Response),
}

fn handshake_timeout(started: Instant) -> BridgeError {
    BridgeError::Timeout {
        elapsed_ms: started.elapsed().as_millis() as u64,
        cause: String::new(),
    }
}

async fn connect_target(
    session: &Mutex<SessionConnection>,
    identity: String,
    client: reqwest::Client,
    mut request: reqwest::Request,
    started: Instant,
    expires: Option<tokio::time::Instant>,
) -> Result<Handshake, BridgeError> {
    // 会话锁只负责取槽；等待 101 的只有同一身份的请求。
    let slot = session.lock().await.target(identity)?;
    let mut cached = slot.lock().await;
    // 目标锁和计时器可能同时就绪，已经到期的轮不能发起新的握手。
    if expires.is_some_and(|at| at <= tokio::time::Instant::now()) {
        return Err(handshake_timeout(started));
    }
    if let Some(upstream) = cached.as_ref().filter(|up| up.is_open()) {
        return Ok(Handshake::Ready(upstream.clone()));
    }
    *request.method_mut() = Method::GET;
    *request.version_mut() = axum::http::Version::HTTP_11;
    *request.body_mut() = None;
    request.headers_mut().remove(header::CONTENT_LENGTH);
    request.headers_mut().remove(header::CONTENT_TYPE);
    for name in [
        "sec-websocket-protocol",
        "sec-websocket-extensions",
        "sec-websocket-accept",
    ] {
        request.headers_mut().remove(name);
    }
    request.headers_mut().insert(
        header::CONNECTION,
        header::HeaderValue::from_static("Upgrade"),
    );
    request.headers_mut().insert(
        header::UPGRADE,
        header::HeaderValue::from_static("websocket"),
    );
    request.headers_mut().insert(
        "sec-websocket-version",
        header::HeaderValue::from_static("13"),
    );
    let key = tokio_tungstenite::tungstenite::handshake::client::generate_key();
    let expected = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    request.headers_mut().insert(
        "sec-websocket-key",
        header::HeaderValue::from_str(&key)
            .map_err(|_| BridgeError::Config("WebSocket handshake key is invalid".into()))?,
    );
    // 使用同一个 reqwest client，继承账号私有CA、解析地址和环境代理。
    let response = client
        .execute(request)
        .await
        .map_err(|e| crate::dispatch::reqwest_error_to_bridge(&e, started))?;
    aisix_gateway::note_upstream_response(response.headers());
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        if response.status().is_success() {
            return Err(BridgeError::UpstreamDecode(
                "upstream did not accept WebSocket upgrade".into(),
            ));
        }
        return Ok(Handshake::Rejected(response));
    }
    let valid = response
        .headers()
        .get("sec-websocket-accept")
        .and_then(|v| v.to_str().ok())
        == Some(expected.as_str())
        && response
            .headers()
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        && response
            .headers()
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            });
    if !valid {
        return Err(BridgeError::UpstreamDecode(
            "invalid WebSocket handshake".into(),
        ));
    }
    let upgraded = response
        .upgrade()
        .await
        .map_err(|e| crate::dispatch::reqwest_error_to_bridge(&e, started))?;
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(aisix_gateway::sse::MAX_SSE_FRAME_BYTES),
        max_frame_size: Some(aisix_gateway::sse::MAX_SSE_FRAME_BYTES),
        ..Default::default()
    };
    let socket = UpSocket::from_raw_socket(upgraded, Role::Client, Some(config)).await;
    let (commands, receive) = mpsc::channel(ACTIVE_RESPONSES * 2);
    let task = tokio::spawn(run_upstream(socket, receive).instrument(tracing::Span::current()));
    let upstream = Arc::new(UpstreamConnection {
        commands,
        task: task.abort_handle(),
    });
    *cached = Some(upstream.clone());
    Ok(Handshake::Ready(upstream))
}

fn is_terminal(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("response.completed" | "response.failed" | "response.incomplete" | "error")
    )
}

async fn run_upstream<S>(mut socket: S, mut commands: mpsc::Receiver<UpstreamCommand>)
where
    S: futures::Stream<Item = Result<UpMessage, tokio_tungstenite::tungstenite::Error>>
        + futures::Sink<UpMessage, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let mut rounds = HashMap::<Option<String>, UpstreamRound>::new();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(UpstreamCommand::Create { lane, token, payload, output, expires, io }) => {
                    if expires.is_some_and(|at| at <= tokio::time::Instant::now()) {
                        io.fail_request();
                        io.fail_response();
                        continue;
                    }
                    if rounds.contains_key(&lane) { break; }
                    io.begin_response(Some("application/json"));
                    rounds.insert(lane, UpstreamRound { token, response_id: None, output, io: io.clone(), terminal: false });
                    let mut observed = ObservedUpstreamSocket::new(&mut socket, io.clone());
                    let send = observed.send(UpMessage::Text(payload));
                    let sent = match expires {
                        Some(expires) => tokio::time::timeout_at(expires, send).await.is_ok_and(|result| result.is_ok()),
                        None => send.await.is_ok(),
                    };
                    if !sent { io.fail_request(); break; }
                    io.finish_request();
                }
                Some(UpstreamCommand::Cancel { lane, token }) => {
                    if rounds.get(&lane).is_some_and(|round| round.token == token) { break; }
                }
                None => break,
            },
            message = socket.next() => match message {
                Some(Ok(UpMessage::Text(text))) => {
                    let Ok(event) = serde_json::from_str::<Value>(&text) else {
                        observe_unroutable_payload(&rounds, text.as_bytes());
                        break;
                    };
                    if !event.is_object() || event.get("type").and_then(Value::as_str).is_none() {
                        observe_unroutable_payload(&rounds, text.as_bytes());
                        break;
                    }
                    let lane = match event.get("stream_id") {
                        None => None,
                        Some(Value::String(id)) => Some(id.clone()),
                        _ => break,
                    };
                    let Some(round) = rounds.get_mut(&lane) else { break; };
                    if let Some(id) = event.get("response").and_then(|response| response.get("id")).or_else(|| event.get("response_id")) {
                        let Some(id) = id.as_str().filter(|id| !id.is_empty()) else { break; };
                        if round.response_id.as_deref().is_some_and(|bound| bound != id) { break; }
                        round.response_id = Some(id.to_owned());
                    }
                    // 先观察上游原始消息，再交给内部 SSE 桥；不计合成的 data 前缀。
                    round.io.observe_response_message(text.as_bytes());
                    if is_terminal(&event) {
                        round.io.finish_response();
                        round.terminal = true;
                    }
                    // 一个慢lane不能堵住其余lane，也不能丢帧后继续计作成功。
                    if round.output.try_send(text).is_err() { break; }
                    if is_terminal(&event) { rounds.remove(&lane); }
                    tokio::task::yield_now().await;
                }
                Some(Ok(UpMessage::Ping(bytes))) => {
                    if socket.send(UpMessage::Pong(bytes)).await.is_err() { break; }
                }
                Some(Ok(UpMessage::Pong(_))) => {}
                Some(Ok(UpMessage::Close(_))) => {
                    // 先让后续 create 建立新连接，并使已接受轮立即独立结算。
                    drop(commands);
                    drop(rounds);
                    // 库已排队原样 Close 回复；仅终止路径执行一次有界冲刷。
                    let _ = tokio::time::timeout(Duration::from_secs(2), socket.flush()).await;
                    return;
                }
                Some(Ok(UpMessage::Binary(bytes))) => {
                    observe_unroutable_payload(&rounds, &bytes);
                    break;
                }
                _ => break,
            }
        }
    }
    // 关闭所有接收端使各自的既有流守卫独立结算失败，不重派任何已写入create。
}

fn observe_unroutable_payload(rounds: &HashMap<Option<String>, UpstreamRound>, bytes: &[u8]) {
    // 唯一活跃轮可归属坏帧；多 lane 下无法判定归属，绝不把一帧复制到各租户。
    if rounds.len() == 1 {
        if let Some(round) = rounds.values().next() {
            round.io.observe_response_message(bytes);
            round.io.mark_output_text_partial();
        }
    }
}

const FAILED_STREAM: &[u8] = b"data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"code\":\"upstream_stream_error\",\"message\":\"upstream response stream ended unexpectedly\"}}\n\n";

fn native_event_frame(text: &str) -> Bytes {
    // WS事件允许带JSON排版换行；每行均加data前缀，不能把后续行当作SSE字段丢掉。
    let mut frame = String::with_capacity(text.len() + 8);
    frame.push_str("data: ");
    let mut rest = text;
    while let Some(end) = memchr::memchr2(b'\r', b'\n', rest.as_bytes()) {
        frame.push_str(&rest[..end]);
        frame.push_str("\ndata: ");
        let cr = rest.as_bytes()[end] == b'\r';
        rest = &rest[end + 1..];
        if cr {
            rest = rest.strip_prefix('\n').unwrap_or(rest);
        }
    }
    frame.push_str(rest);
    frame.push_str("\n\n");
    Bytes::from(frame)
}

fn stream_failure_response() -> reqwest::Response {
    let mut response = axum::http::Response::new(reqwest::Body::from(FAILED_STREAM));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    response.into()
}

pub(crate) async fn responses_websocket(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    peer: Option<ConnectInfo<std::net::SocketAddr>>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let started = Instant::now();
    crate::attribution::note_handler_started(started);
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "GET",
                "/v1/responses",
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                ProxyError::WebSocketUpgradeRequired {
                    status: rejection.status(),
                    detail: rejection.body_text(),
                },
            )
        }
    };
    crate::reject::emit_unrouted_access_log(
        "GET",
        "/v1/responses",
        &client.request_id,
        Some(&auth.entry.id),
        101,
        started,
    );
    crate::reject::record_undispatched(
        &state,
        "/v1/responses",
        crate::request_metrics::Caller::new(&auth),
        101,
        aisix_obs::ErrorSource::None,
        started,
    );
    let guard = crate::DrainGuard::new(state.livez.clone());
    let limit = state.request_body_limit_for("/v1/responses");
    // HTTP 正文的 0 表示不额外限额；WebSocket 仍保留库的 64 MiB 消息、16 MiB 帧上限。
    let ws = if limit == 0 {
        ws
    } else {
        ws.max_message_size(limit).max_frame_size(limit)
    };
    let span = tracing::Span::current();
    ws.on_upgrade(move |socket| {
        async move {
            run_session(state, client, peer, socket).await;
            drop(guard);
        }
        .instrument(span)
    })
}

struct Create {
    stream_id: Option<String>,
    body: Value,
}

fn parse_create(mut event: Value) -> Result<Create, ProxyError> {
    let object = event
        .as_object_mut()
        .ok_or_else(|| ProxyError::InvalidRequest("WebSocket event must be an object".into()))?;
    if object.get("type").and_then(Value::as_str) != Some("response.create") {
        return Err(ProxyError::InvalidRequest(
            "type must be response.create".into(),
        ));
    }
    if object.contains_key("stream") {
        return Err(ProxyError::InvalidRequest(
            "stream is implicit over WebSocket".into(),
        ));
    }
    if object
        .get("background")
        .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
    {
        return Err(ProxyError::InvalidRequest(
            "background is not supported over WebSocket".into(),
        ));
    }
    object.remove("background");
    let stream_id = match object.remove("stream_id") {
        None => None,
        Some(Value::String(id)) if !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) => Some(id),
        _ => return Err(ProxyError::InvalidRequest("stream_id must contain 1 to 256 ASCII letters, digits, underscores, hyphens or periods".into())),
    };
    object.remove("type");
    object.insert("stream".into(), Value::Bool(true));
    Ok(Create {
        stream_id,
        body: event,
    })
}

fn websocket_error(
    code: &str,
    message: &str,
    stream_id: Option<&str>,
    param: Option<&str>,
) -> Value {
    let mut event = json!({"type":"error","status":400,"error":{"type":"invalid_request_error","code":code,"message":message}});
    if let Some(id) = stream_id {
        event["stream_id"] = json!(id);
    }
    if let Some(param) = param {
        event["error"]["param"] = json!(param);
    }
    event
}

fn error_event(error: ProxyError, stream_id: Option<&str>) -> Value {
    let mut value = serde_json::to_value(error.envelope())
        .unwrap_or_else(|_| json!({"error":{"type":"api_error","message":"request failed"}}));
    value["type"] = json!("error");
    value["status"] = json!(error.status().as_u16());
    if let Some(id) = stream_id {
        value["stream_id"] = json!(id);
    }
    value
}

#[derive(Clone)]
struct Steer {
    id: String,
    parent: String,
    input: Value,
}

#[derive(Default)]
struct LaneState {
    body: Value,
    response_id: Option<String>,
    active: bool,
    native: Option<Arc<AtomicBool>>,
    sequence: u64,
    pending: Vec<Steer>,
    committing: Vec<Steer>,
    successor: Option<Value>,
}

enum SessionEvent {
    Begin {
        lane: Option<String>,
        body: Value,
        native: Arc<AtomicBool>,
        reply: oneshot::Sender<Value>,
    },
    Output(Value),
    Finished {
        lane: Option<String>,
        reply: oneshot::Sender<Option<Value>>,
    },
}

fn lane_event(mut event: Value, lane: Option<&str>, state: &mut LaneState) -> Value {
    event["sequence_number"] = json!(state.sequence);
    state.sequence = state.sequence.saturating_add(1);
    if let Some(lane) = lane {
        event["stream_id"] = json!(lane);
    } else if let Some(object) = event.as_object_mut() {
        object.remove("stream_id");
    }
    event
}

fn steer_failed(event: &Value, code: &str, message: &str, id: Option<&str>) -> Value {
    let mut failed = json!({
        "type":"response.steer.failed",
        "error":{"type":"invalid_request_error","code":code,"message":message},
        "steer":{"previous_response_id":event.get("previous_response_id").cloned().unwrap_or(Value::Null),"input":event.get("input").cloned().unwrap_or(Value::Null)}
    });
    if let Some(id) = id {
        failed["steer"]["id"] = json!(id);
    }
    failed
}

fn valid_steer(event: &Value) -> bool {
    if event.as_object().is_none_or(|fields| {
        fields
            .keys()
            .any(|field| !matches!(field.as_str(), "type" | "previous_response_id" | "input"))
    }) || event
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_none_or(|id| id.is_empty())
    {
        return false;
    }
    match event.get("input") {
        Some(Value::String(_)) => true,
        Some(Value::Array(items)) if !items.is_empty() => items.iter().all(|item| {
            item.as_object().is_some_and(|fields| {
                fields
                    .keys()
                    .all(|field| matches!(field.as_str(), "type" | "role" | "content"))
            }) && item.get("role").and_then(Value::as_str) == Some("user")
                && item.get("type").is_none_or(|kind| kind == "message")
                && match item.get("content") {
                    Some(Value::String(_)) => true,
                    Some(Value::Array(parts)) => parts.iter().all(|part| {
                        matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("input_text" | "input_image" | "input_file")
                        )
                    }),
                    _ => false,
                }
        }),
        _ => false,
    }
}

fn submit_steer(event: &Value, states: &mut HashMap<Option<String>, LaneState>) -> Value {
    let target = event
        .get("previous_response_id")
        .and_then(Value::as_str)
        .and_then(|id| {
            states
                .iter_mut()
                .find(|(_, state)| state.response_id.as_deref() == Some(id))
        });
    let valid = valid_steer(event);
    let Some((lane, state)) = target else {
        let mut failed = steer_failed(
            event,
            if valid {
                "response_not_found"
            } else {
                "invalid_input"
            },
            "the target response or steering input is not available",
            None,
        );
        failed["sequence_number"] = json!(0);
        return failed;
    };
    let rejection = if !valid {
        Some((
            "invalid_input",
            "response.steer accepts only user messages and no stream_id",
        ))
    } else if !state.active {
        Some((
            "response_already_completed",
            "the response no longer accepts steering input",
        ))
    } else if !state
        .native
        .as_ref()
        .is_some_and(|fact| fact.load(Ordering::Acquire))
        || ["conversation", "context_management"]
            .iter()
            .any(|field| state.body.get(field).is_some_and(|value| !value.is_null()))
    {
        Some((
            "steering_not_supported",
            "this execution mode does not support steering",
        ))
    } else if state.pending.len() >= QUEUED_PER_LANE {
        Some((
            "too_many_pending_steers",
            "too much steering input is pending",
        ))
    } else {
        None
    };
    let result = match rejection {
        Some((code, message)) => steer_failed(event, code, message, None),
        None => {
            let steer = Steer {
                id: format!("steer_{}", crate::request_id::new_request_id()),
                parent: event["previous_response_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                input: event["input"].clone(),
            };
            let accepted = json!({"type":"response.steer.accepted","steer":{"id":steer.id,"previous_response_id":steer.parent}});
            state.pending.push(steer);
            accepted
        }
    };
    lane_event(result, lane.as_deref(), state)
}

fn input_items(input: &Value) -> Vec<Value> {
    match input {
        Value::String(text) => vec![json!({"type":"message","role":"user","content":text})],
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

fn prepend_steers(body: &mut Value, steers: &[Steer]) {
    let mut input: Vec<Value> = steers
        .iter()
        .flat_map(|steer| input_items(&steer.input))
        .collect();
    input.extend(input_items(&body["input"]));
    body["input"] = json!(input);
}

fn required_input(event: &Value) -> Vec<Value> {
    event
        .get("response")
        .and_then(|response| response.get("output"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let kind = item.get("type").and_then(Value::as_str)?;
            if kind == "mcp_approval_request" {
                return item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| json!({"type":"mcp_approval_response","approval_request_id":id}));
            }
            let output = match kind {
                "function_call" => "function_call_output",
                "custom_tool_call" => "custom_tool_call_output",
                "computer_call" => "computer_call_output",
                "shell_call" => "shell_call_output",
                "apply_patch_call" => "apply_patch_call_output",
                "tool_search_call"
                    if item.get("execution").and_then(Value::as_str) == Some("client") =>
                {
                    "tool_search_output"
                }
                _ => return None,
            };
            let call_id = item.get("call_id").and_then(Value::as_str)?;
            let mut stub = json!({"type":output,"call_id":call_id});
            if kind == "function_call" {
                stub["name"] = item.get("name").cloned().unwrap_or(Value::Null);
            }
            if kind == "tool_search_call" {
                stub["execution"] = json!("client");
            }
            Some(stub)
        })
        .collect()
}

fn run_session(
    state: ProxyState,
    client: ClientContext,
    peer: Option<ConnectInfo<std::net::SocketAddr>>,
    socket: WebSocket,
) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        let lifecycle = state.livez.clone();
        let session = async move {
            let (mut writer, mut reader) = socket.split();
            let (out, mut events) = mpsc::channel::<SessionEvent>(EVENT_QUEUE);
            let mut lanes = HashMap::<Option<String>, mpsc::Sender<Value>>::new();
            let mut tasks = JoinSet::new();
            let mut states = HashMap::<Option<String>, LaneState>::new();
            let connection = Arc::new(Mutex::new(SessionConnection::default()));
            let router = crate::build_router(state.clone());
            let active = Arc::new(Semaphore::new(ACTIVE_RESPONSES));
            let expires =
                tokio::time::sleep(Duration::from_secs(60 * 60) - Duration::from_millis(50));
            tokio::pin!(expires);
            let mut drain = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    message = reader.next() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let parsed = serde_json::from_str::<Value>(&text);
                                if let Ok(event) = &parsed {
                                    if event.get("type").and_then(Value::as_str) == Some("response.steer") {
                                        let failed = submit_steer(event, &mut states);
                                        if writer.send(Message::Text(failed.to_string())).await.is_err() { break; }
                                        continue;
                                    }
                                }
                                let lane = parsed.as_ref().ok().and_then(|v| v.get("stream_id")).and_then(Value::as_str).map(str::to_owned);
                                let create = match parsed.map_err(|_| ProxyError::InvalidRequest("invalid JSON WebSocket event".into())).and_then(parse_create) {
                                    Ok(create) => create,
                                    Err(error) => { if writer.send(Message::Text(error_event(error, lane.as_deref()).to_string())).await.is_err() { break; } continue; }
                                };
                                if !lanes.contains_key(&create.stream_id) {
                                    if create.stream_id.is_some() && lanes.keys().filter(|id| id.is_some()).count() >= MAX_LANES {
                                        if writer.send(Message::Text(websocket_error("websocket_stream_limit_reached", "this connection has reached its limit of 32 distinct named stream IDs", create.stream_id.as_deref(), Some("stream_id")).to_string())).await.is_err() { break; }
                                        continue;
                                    }
                                    let (send, receive) = mpsc::channel(QUEUED_PER_LANE);
                                    lanes.insert(create.stream_id.clone(), send);
                                    tasks.spawn(run_lane(router.clone(), client.clone(), peer, create.stream_id.clone(), receive, out.clone(), active.clone(), connection.clone()).instrument(tracing::Span::current()));
                                }
                                let Some(send) = lanes.get(&create.stream_id) else { break; };
                                if send.try_send(create.body).is_err() && writer.send(Message::Text(error_event(ProxyError::InvalidRequest("too many queued requests on this WebSocket lane".into()), create.stream_id.as_deref()).to_string())).await.is_err() { break; }
                            }
                            Some(Ok(Message::Ping(bytes))) => { if writer.send(Message::Pong(bytes)).await.is_err() { break; } }
                            Some(Ok(Message::Pong(_))) => {}
                            Some(Ok(Message::Close(_))) => {
                                tasks.abort_all();
                                // 库已排队关闭回复；先取消轮次，再有界冲刷，避免客户端收到异常断连。
                                let _ = tokio::time::timeout(Duration::from_secs(2), writer.flush()).await;
                                break;
                            }
                            Some(Ok(Message::Binary(_))) => { if writer.send(Message::Text(error_event(ProxyError::InvalidRequest("WebSocket events must be JSON text".into()), None).to_string())).await.is_err() { break; } }
                            _ => break,
                        }
                    }
                    event = events.recv() => {
                        let Some(event) = event else { break; };
                        match event {
                            SessionEvent::Begin { lane, mut body, native, reply } => {
                                let state = states.entry(lane.clone()).or_default();
                                if !state.pending.is_empty() && body.get("previous_response_id").and_then(Value::as_str) == state.response_id.as_deref() {
                                    let steers = std::mem::take(&mut state.pending);
                                    prepend_steers(&mut body, &steers);
                                    state.committing = steers;
                                }
                                if !state.pending.is_empty() {
                                    let abandoned = std::mem::take(&mut state.pending);
                                    for steer in abandoned {
                                        let submission = json!({"previous_response_id":steer.parent,"input":steer.input});
                                        let failed = lane_event(steer_failed(&submission, "successor_creation_failed", "the next request does not continue the steered response", Some(&steer.id)), lane.as_deref(), state);
                                        if writer.send(Message::Text(failed.to_string())).await.is_err() { return; }
                                    }
                                }
                                state.body = body.clone();
                                state.active = false;
                                state.native = Some(native);
                                let _ = reply.send(body);
                            }
                            SessionEvent::Output(mut event) => {
                                let lane = event.get("stream_id").and_then(Value::as_str).map(str::to_owned);
                                let state = states.entry(lane.clone()).or_default();
                                let kind = event.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
                                if kind == "response.created" {
                                    state.active = true;
                                    state.response_id = event.get("response").and_then(|response| response.get("id")).and_then(Value::as_str).map(str::to_owned);
                                    state.committing.clear();
                                }
                                let terminal = is_terminal(&event);
                                if terminal { state.active = false; }
                                let required = if terminal { required_input(&event) } else { Vec::new() };
                                event = lane_event(event, lane.as_deref(), state);
                                if writer.send(Message::Text(event.to_string())).await.is_err() { break; }
                                if terminal && !state.pending.is_empty() {
                                    if kind == "response.completed" && !required.is_empty() {
                                        let steers = state.pending.clone();
                                        for steer in steers {
                                            let pending = lane_event(json!({"type":"response.steer.pending","reason":"waiting_for_required_input","required_input":required,"steer":{"id":steer.id,"previous_response_id":steer.parent}}), lane.as_deref(), state);
                                            if writer.send(Message::Text(pending.to_string())).await.is_err() { return; }
                                        }
                                    } else if kind == "response.completed" || (kind == "response.incomplete" && event["response"]["incomplete_details"]["reason"] == "steered") {
                                        let mut body = state.body.clone();
                                        body["previous_response_id"] = json!(state.response_id);
                                        body["input"] = json!([]);
                                        state.successor = Some(body);
                                    } else {
                                        let failed = std::mem::take(&mut state.pending);
                                        for steer in failed {
                                            let submission = json!({"previous_response_id":steer.parent,"input":steer.input});
                                            let failed = lane_event(steer_failed(&submission, "successor_creation_failed", "the response could not continue", Some(&steer.id)), lane.as_deref(), state);
                                            if writer.send(Message::Text(failed.to_string())).await.is_err() { return; }
                                        }
                                    }
                                }
                            }
                            SessionEvent::Finished { lane, reply } => {
                                let state = states.entry(lane.clone()).or_default();
                                let uncommitted = std::mem::take(&mut state.committing);
                                for steer in uncommitted {
                                    let submission = json!({"previous_response_id":steer.parent,"input":steer.input});
                                    let failed = lane_event(steer_failed(&submission, "successor_creation_failed", "the successor response could not be created", Some(&steer.id)), lane.as_deref(), state);
                                    if writer.send(Message::Text(failed.to_string())).await.is_err() { return; }
                                }
                                let _ = reply.send(state.successor.take());
                            }
                        }
                    }
                    _ = drain.tick() => {
                        if state.livez.is_shutting_down() { break; }
                    }
                    _ = &mut expires => {
                        let expired = websocket_error("websocket_connection_limit_reached", "Responses WebSocket connection limit reached (60 minutes); open a new connection", None, None);
                        let _ = writer.send(Message::Text(expired.to_string())).await;
                        break;
                    },
                }
            }
            // 每轮完整router持有自己的取消守卫；中断任务同时销毁未归还的上游连接。
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        };
        // 写操作阻塞也要可取消；外层独立观察寿命和排空，Drop会中断所有lane。
        let shutdown = async move {
            let mut poll = tokio::time::interval(Duration::from_millis(100));
            loop {
                poll.tick().await;
                if lifecycle.is_shutting_down() {
                    return;
                }
            }
        };
        tokio::select! {
            _ = session => {},
            _ = shutdown => {},
            _ = tokio::time::sleep(Duration::from_secs(60 * 60)) => {},
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_lane(
    router: axum::Router,
    client: ClientContext,
    peer: Option<ConnectInfo<std::net::SocketAddr>>,
    stream_id: Option<String>,
    mut queue: mpsc::Receiver<Value>,
    out: mpsc::Sender<SessionEvent>,
    active: Arc<Semaphore>,
    connection: Arc<Mutex<SessionConnection>>,
) {
    let mut successor = None;
    loop {
        let body = match successor.take() {
            Some(body) => body,
            None => match queue.recv().await {
                Some(body) => body,
                None => break,
            },
        };
        let Ok(_permit) = active.acquire().await else {
            break;
        };
        let fact = Arc::new(AtomicBool::new(false));
        let (reply, receive) = oneshot::channel();
        if out
            .send(SessionEvent::Begin {
                lane: stream_id.clone(),
                body,
                native: fact.clone(),
                reply,
            })
            .await
            .is_err()
        {
            break;
        }
        let Ok(body) = receive.await else {
            break;
        };
        let transport = RoundTransport {
            connection: connection.clone(),
            stream_id: stream_id.clone(),
            create_attempted: fact,
        };
        let round = dispatch_round(
            router.clone(),
            &client,
            peer,
            body,
            stream_id.as_deref(),
            &out,
        );
        if ROUND.scope(transport, round).await.is_err()
            && out
                .send(SessionEvent::Output(error_event(
                    ProxyError::ProviderUnavailable,
                    stream_id.as_deref(),
                )))
                .await
                .is_err()
        {
            break;
        }
        let (reply, receive) = oneshot::channel();
        if out
            .send(SessionEvent::Finished {
                lane: stream_id.clone(),
                reply,
            })
            .await
            .is_err()
        {
            break;
        }
        let Ok(next) = receive.await else {
            break;
        };
        successor = next;
    }
}

async fn dispatch_round(
    router: axum::Router,
    client: &ClientContext,
    peer: Option<ConnectInfo<std::net::SocketAddr>>,
    body: Value,
    stream_id: Option<&str>,
    out: &mpsc::Sender<SessionEvent>,
) -> Result<(), ()> {
    let mut request = Request::new(Body::from(body.to_string()));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = axum::http::Uri::from_static("/v1/responses");
    *request.headers_mut() = (*client.headers).clone();
    for name in [
        "connection",
        "upgrade",
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-protocol",
        "sec-websocket-extensions",
        "content-length",
        "content-encoding",
        "transfer-encoding",
    ] {
        request.headers_mut().remove(name);
    }
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    request
        .extensions_mut()
        .insert(crate::request_id::RequestId(
            crate::request_id::new_request_id(),
        ));
    if let Some(peer) = peer {
        request.extensions_mut().insert(peer);
    }
    let response = match router.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status();
    if !status.is_success() {
        let bytes = axum::body::to_bytes(
            response.into_body(),
            aisix_gateway::sse::MAX_SSE_FRAME_BYTES,
        )
        .await
        .map_err(|_| ())?;
        let mut event = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| error_event(ProxyError::ProviderUnavailable, stream_id));
        event["type"] = json!("error");
        event["status"] = json!(status.as_u16());
        if let Some(id) = stream_id {
            event["stream_id"] = json!(id);
        }
        return out.send(SessionEvent::Output(event)).await.map_err(|_| ());
    }
    let mut terminal = false;
    let mut stream = response.into_body().into_data_stream();
    let mut splitter =
        aisix_gateway::sse::SseFrameSplitter::new(aisix_gateway::sse::MAX_SSE_FRAME_BYTES);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        splitter.push(&chunk);
        while let Some(frame) = splitter.next_frame().map_err(|_| ())? {
            let fields = aisix_gateway::sse::parse_frame(&frame);
            let Some(data) = fields.data else {
                continue;
            };
            if data.as_ref() == b"[DONE]" {
                continue;
            }
            let mut event: Value = serde_json::from_slice(&data).map_err(|_| ())?;
            if !event.is_object() {
                return Err(());
            }
            if let Some(id) = stream_id {
                event["stream_id"] = json!(id);
            }
            terminal |= is_terminal(&event);
            out.send(SessionEvent::Output(event))
                .await
                .map_err(|_| ())?;
        }
    }
    if !terminal {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_slots_count_pending_owners_and_reclaim_abandoned_targets() {
        let mut session = SessionConnection::default();
        let mut pending: Vec<_> = (0..UPSTREAM_CONNECTIONS)
            .map(|index| session.target(index.to_string()).unwrap())
            .collect();
        assert!(matches!(
            session.target("overflow".into()),
            Err(BridgeError::Config(_))
        ));
        assert!(Arc::ptr_eq(
            &pending[0],
            &session.target("0".into()).unwrap()
        ));
        let abandoned = Arc::downgrade(pending.last().unwrap());
        pending.pop();
        let replacement = session.target("replacement".into()).unwrap();
        assert!(abandoned.upgrade().is_none());
        assert_eq!(session.upstreams.len(), UPSTREAM_CONNECTIONS);
        assert!(Arc::ptr_eq(
            &replacement,
            &session.target("replacement".into()).unwrap()
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn handshake_rechecks_expiry_as_the_target_lock_becomes_available() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = reqwest::Client::new();
        let request = client
            .post(format!(
                "http://{}/v1/responses",
                listener.local_addr().unwrap()
            ))
            .build()
            .unwrap();
        let session = Mutex::new(SessionConnection::default());
        let slot = session.lock().await.target("cold".into()).unwrap();
        let held = slot.lock().await;
        let budget = Duration::from_secs(1);
        let expires = tokio::time::Instant::now() + budget;
        let mut connect = std::pin::pin!(connect_target(
            &session,
            "cold".into(),
            client,
            request,
            Instant::now(),
            Some(expires)
        ));
        assert!(futures::poll!(connect.as_mut()).is_pending());
        tokio::time::advance(budget).await;
        drop(held);
        // 锁和截止计时器同时就绪，第一次取得锁的轮询必须立即超时。
        assert!(matches!(
            futures::poll!(connect.as_mut()),
            std::task::Poll::Ready(Err(BridgeError::Timeout { .. }))
        ));
    }

    #[tokio::test]
    async fn handshake_waiters_keep_their_deadlines_and_recover_from_cancelled_owner() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let session = Arc::new(Mutex::new(SessionConnection::default()));
        let client = reqwest::Client::new();
        let start = |lane: &'static str, deadline| {
            let attempted = Arc::new(AtomicBool::new(false));
            let round = RoundTransport {
                connection: session.clone(),
                stream_id: Some(lane.into()),
                create_attempted: attempted.clone(),
            };
            let request = client.post(&url).body(r#"{"model":"local"}"#);
            let task = tokio::spawn(ROUND.scope(round, async move {
                send_native(request, deadline, Instant::now()).await
            }));
            (attempted, task)
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let (owner_attempted, owner) = start("owner", None);
            let (held, _) = listener.accept().await.unwrap();
            let (expired_attempted, short) = start("short", Some(Duration::from_millis(80)));
            assert!(matches!(
                short.await.unwrap(),
                Err(BridgeError::Timeout { .. })
            ));
            assert!(!expired_attempted.load(Ordering::Acquire));

            let (_, next) = start("next", None);
            let (_, other) = start("other", None);
            owner.abort();
            assert!(owner.await.unwrap_err().is_cancelled());
            assert!(!owner_attempted.load(Ordering::Acquire));
            drop(held);

            // 创建者取消后两名等待者都能继续，且只在同一条替代连接派发。
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _next_body = next.await.unwrap().unwrap();
            let _other_body = other.await.unwrap().unwrap();
            let mut lanes = Vec::new();
            for _ in 0..2 {
                let message = socket.next().await.unwrap().unwrap();
                let event: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                assert_eq!(event["type"], "response.create");
                lanes.push(event["stream_id"].as_str().unwrap().to_owned());
            }
            lanes.sort();
            assert_eq!(lanes, ["next", "other"]);
        })
        .await
        .expect("等待者必须在首个握手保持悬挂时独立超时，并在取消后复用新连接");
    }

    struct HeldCloseFlush {
        close: oneshot::Receiver<()>,
        flushing: Option<oneshot::Sender<()>>,
        release: oneshot::Receiver<()>,
        sent: mpsc::UnboundedSender<UpMessage>,
        closing: bool,
    }

    impl futures::Stream for HeldCloseFlush {
        type Item = Result<UpMessage, tokio_tungstenite::tungstenite::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if !self.closing
                && std::future::Future::poll(std::pin::Pin::new(&mut self.close), cx).is_ready()
            {
                self.closing = true;
                return std::task::Poll::Ready(Some(Ok(UpMessage::Close(None))));
            }
            std::task::Poll::Pending
        }
    }

    impl futures::Sink<UpMessage> for HeldCloseFlush {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(
            self: std::pin::Pin<&mut Self>,
            message: UpMessage,
        ) -> Result<(), Self::Error> {
            let _ = self.sent.send(message);
            Ok(())
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            if self.closing {
                if let Some(flushing) = self.flushing.take() {
                    let _ = flushing.send(());
                }
                if std::future::Future::poll(std::pin::Pin::new(&mut self.release), cx).is_pending()
                {
                    return std::task::Poll::Pending;
                }
            }
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test]
    async fn provider_close_releases_rounds_and_reconnects_while_ack_flush_is_pending() {
        let (close, closed) = oneshot::channel();
        let (flushing, flush_started) = oneshot::channel();
        let (release, held) = oneshot::channel();
        let (sent, mut messages) = mpsc::unbounded_channel();
        let (commands, receive) = mpsc::channel(4);
        let actor = tokio::spawn(run_upstream(
            HeldCloseFlush {
                close: closed,
                flushing: Some(flushing),
                release: held,
                sent,
                closing: false,
            },
            receive,
        ));
        let upstream = Arc::new(UpstreamConnection {
            commands,
            task: actor.abort_handle(),
        });
        let (output, mut events) = mpsc::channel(4);
        upstream
            .commands
            .send(UpstreamCommand::Create {
                lane: None,
                token: "before-close".into(),
                payload: "before-close".into(),
                output,
                expires: None,
                io: UpstreamIoObserver::websocket(Protocol::Responses),
            })
            .await
            .unwrap();
        assert_eq!(
            messages.recv().await,
            Some(UpMessage::Text("before-close".into()))
        );
        close.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), flush_started)
            .await
            .expect("Close 必须开始冲刷自动回复")
            .unwrap();
        assert!(upstream.commands.is_closed(), "关闭中不能再接受 create");
        assert!(tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("活动轮不能等待 ACK 冲刷才结算")
            .is_none());
        drop(CancelRound {
            upstream: upstream.clone(),
            lane: None,
            token: "before-close".into(),
        });
        tokio::task::yield_now().await;
        assert!(!actor.is_finished(), "轮次清理不能中断关闭回复");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        // 该请求无自定义头，连接身份仅含 URL；将关闭中的连接放进相同缓存槽。
        let target = format!("{:x}", Sha256::digest(url.as_bytes()));
        let connection = Arc::new(Mutex::new(SessionConnection {
            upstreams: [(target, Arc::new(Mutex::new(Some(upstream))))].into(),
        }));
        let transport = RoundTransport {
            connection,
            stream_id: None,
            create_attempted: Arc::new(AtomicBool::new(false)),
        };
        let next_round = tokio::spawn(ROUND.scope(transport, async move {
            send_native(
                reqwest::Client::new()
                    .post(url)
                    .body(r#"{"model":"fresh"}"#),
                Some(Duration::from_secs(1)),
                Instant::now(),
            )
            .await
        }));
        let (tcp, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .expect("新 create 必须立即建立新连接，不等旧 ACK 冲刷")
            .unwrap();
        let mut provider = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let create = provider.next().await.unwrap().unwrap();
        let create: Value = serde_json::from_str(create.to_text().unwrap()).unwrap();
        assert_eq!(create, json!({"type":"response.create","model":"fresh"}));
        let response = next_round.await.unwrap().unwrap();
        assert!(!actor.is_finished(), "移除旧连接不能中断关闭回复");
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .unwrap()
            .unwrap();
        drop(response);
    }

    #[tokio::test(start_paused = true)]
    async fn provider_close_ack_flush_has_a_terminal_deadline() {
        let (close, closed) = oneshot::channel();
        let (flushing, flush_started) = oneshot::channel();
        let (_release, held) = oneshot::channel();
        let (sent, _messages) = mpsc::unbounded_channel();
        let (commands, receive) = mpsc::channel(4);
        let actor = tokio::spawn(run_upstream(
            HeldCloseFlush {
                close: closed,
                flushing: Some(flushing),
                release: held,
                sent,
                closing: false,
            },
            receive,
        ));
        close.send(()).unwrap();
        flush_started.await.unwrap();
        assert!(commands.is_closed());
        assert!(!actor.is_finished());
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::time::timeout(Duration::from_millis(1), actor)
            .await
            .expect("无法写入的 Close ACK 必须在终止预算内结束")
            .unwrap();
    }

    #[tokio::test]
    async fn upstream_event_overflow_closes_every_active_round_without_dropping_success() {
        let (gateway, provider) = tokio::io::duplex(64 * 1024);
        let gateway =
            tokio_tungstenite::WebSocketStream::from_raw_socket(gateway, Role::Client, None).await;
        let mut provider =
            tokio_tungstenite::WebSocketStream::from_raw_socket(provider, Role::Server, None).await;
        let (commands, receive) = mpsc::channel(ACTIVE_RESPONSES * 2);
        let actor = tokio::spawn(run_upstream(gateway, receive));
        let (slow_output, mut slow) = mpsc::channel(UPSTREAM_EVENT_QUEUE);
        let (other_output, mut other) = mpsc::channel(UPSTREAM_EVENT_QUEUE);
        for (lane, output) in [("slow", slow_output), ("other", other_output)] {
            commands
                .send(UpstreamCommand::Create {
                    lane: Some(lane.into()),
                    token: lane.into(),
                    payload: json!({"type":"response.create","stream_id":lane}).to_string(),
                    output,
                    expires: None,
                    io: UpstreamIoObserver::websocket(Protocol::Responses),
                })
                .await
                .unwrap();
            let created = provider.next().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(created.to_text().unwrap()).unwrap()["stream_id"],
                lane,
            );
        }
        // 慢轮不取事件；队列填满后的下一帧必须关闭共享连接，让每轮独立失败结算。
        for sequence in 0..=UPSTREAM_EVENT_QUEUE {
            provider
                .send(UpMessage::Text(
                    json!({"type":"response.output_text.delta","stream_id":"slow","response_id":"resp_slow","delta":"x","sequence_number":sequence}).to_string(),
                ))
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), actor)
            .await
            .expect("事件溢出不能等待慢轮继续读取")
            .unwrap();
        assert!(commands.is_closed());
        assert!(
            other.recv().await.is_none(),
            "共享连接上其余活跃轮也应看到结束"
        );
        let mut received = 0;
        while let Some(frame) = slow.recv().await {
            let event: Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(event["sequence_number"], received);
            assert!(!is_terminal(&event), "溢出不能伪造成功终态");
            received += 1;
        }
        assert_eq!(received, UPSTREAM_EVENT_QUEUE);
    }
    #[tokio::test]
    async fn websocket_keeps_raw_response_item_and_call_ids_in_both_directions() {
        let (gateway, provider) = tokio::io::duplex(64 * 1024);
        let gateway =
            tokio_tungstenite::WebSocketStream::from_raw_socket(gateway, Role::Client, None).await;
        let mut provider =
            tokio_tungstenite::WebSocketStream::from_raw_socket(provider, Role::Server, None).await;
        let (commands, receive) = mpsc::channel(4);
        let actor = tokio::spawn(run_upstream(gateway, receive));
        let (output, mut events) = mpsc::channel(8);
        let request = json!({"type":"response.create","previous_response_id":"resp_previous_exact",
            "input":[{"type":"function_call_output","call_id":"toolu_exact","output":"done"}]});
        commands
            .send(UpstreamCommand::Create {
                lane: None,
                token: "round-exact".into(),
                payload: request.to_string(),
                output,
                expires: None,
                io: UpstreamIoObserver::websocket(Protocol::Responses),
            })
            .await
            .unwrap();
        let sent = provider.next().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(sent.to_text().unwrap()).unwrap(),
            request
        );
        let expected = [
            json!({"type":"response.created","response":{"id":"resp_exact","previous_response_id":"resp_previous_exact","output":[]}}),
            json!({"type":"response.output_item.added","response_id":"resp_exact","item":{"type":"function_call","id":"fc_exact","call_id":"toolu_exact"}}),
            json!({"type":"response.function_call_arguments.delta","response_id":"resp_exact","item_id":"fc_exact","delta":"{}"}),
            json!({"type":"response.completed","response":{"id":"resp_exact","previous_response_id":"resp_previous_exact",
                "output":[{"type":"function_call","id":"fc_exact","call_id":"toolu_exact"}]}}),
        ];
        for event in expected {
            let text = event.to_string();
            provider.send(UpMessage::Text(text.clone())).await.unwrap();
            let delivered = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(delivered, text);
            let frame = native_event_frame(&delivered);
            let fields = aisix_gateway::sse::parse_frame(&frame);
            assert_eq!(
                serde_json::from_slice::<Value>(&fields.data.unwrap()).unwrap(),
                event
            );
        }
        drop(commands);
        tokio::time::timeout(Duration::from_secs(2), actor)
            .await
            .unwrap()
            .unwrap();
    }
}

#[cfg(test)]
mod upstream_io_v10_tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum ReadyState {
        Pending,
        Failed,
        Ready,
    }

    struct ControlledSocket {
        ready: ReadyState,
        start_fails: bool,
        flush_fails: bool,
        accepted: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl futures::Stream for ControlledSocket {
        type Item = Result<UpMessage, tokio_tungstenite::tungstenite::Error>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Pending
        }
    }

    impl futures::Sink<UpMessage> for ControlledSocket {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            match self.ready {
                ReadyState::Pending => std::task::Poll::Pending,
                ReadyState::Failed => std::task::Poll::Ready(Err(Self::Error::AlreadyClosed)),
                ReadyState::Ready => std::task::Poll::Ready(Ok(())),
            }
        }

        fn start_send(self: std::pin::Pin<&mut Self>, _item: UpMessage) -> Result<(), Self::Error> {
            if self.start_fails {
                return Err(Self::Error::AlreadyClosed);
            }
            self.accepted.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(if self.flush_fails {
                Err(Self::Error::AlreadyClosed)
            } else {
                Ok(())
            })
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            self.poll_flush(cx)
        }
    }

    fn controlled_socket(
        ready: ReadyState,
        start_fails: bool,
        flush_fails: bool,
    ) -> (ControlledSocket, Arc<std::sync::atomic::AtomicUsize>) {
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            ControlledSocket {
                ready,
                start_fails,
                flush_fails,
                accepted: accepted.clone(),
            },
            accepted,
        )
    }

    fn assert_no_accepted_payload(io: &UpstreamIoObserver) {
        let audit = io.snapshot();
        assert_eq!(audit.request_body_bytes, Some(0));
        assert_eq!(audit.request_complete, Some(false));
        assert!(audit.input_text.is_none_or(|text| text.utf8_bytes == 0));
    }

    #[tokio::test(start_paused = true)]
    async fn upstream_io_actor_does_not_count_unready_ready_error_or_start_error() {
        for (ready, start_fails) in [
            (ReadyState::Pending, false),
            (ReadyState::Failed, false),
            (ReadyState::Ready, true),
        ] {
            let (socket, accepted) = controlled_socket(ready, start_fails, false);
            let (commands, receive) = mpsc::channel(1);
            let io = UpstreamIoObserver::websocket(Protocol::Responses);
            let (output, _events) = mpsc::channel(1);
            commands
                .send(UpstreamCommand::Create {
                    lane: None,
                    token: "not-accepted".into(),
                    payload: json!({"type":"response.create", "input":"A汉😀"}).to_string(),
                    output,
                    expires: Some(tokio::time::Instant::now() + Duration::from_secs(1)),
                    io: io.clone(),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), run_upstream(socket, receive))
                .await
                .unwrap();
            assert_eq!(accepted.load(Ordering::Acquire), 0);
            assert_no_accepted_payload(&io);
        }
    }

    #[tokio::test]
    async fn upstream_io_cancel_before_socket_ready_does_not_count_split_sink_buffer() {
        let (socket, accepted) = controlled_socket(ReadyState::Pending, false, false);
        let io = UpstreamIoObserver::websocket(Protocol::Realtime);
        let (mut tx, _rx) = ObservedUpstreamSocket::new(socket, io.clone()).split();
        futures::future::poll_fn(|cx| futures::Sink::poll_ready(std::pin::Pin::new(&mut tx), cx))
            .await
            .unwrap();
        futures::Sink::start_send(
            std::pin::Pin::new(&mut tx),
            UpMessage::Text(
                json!({"type":"response.create", "response":{"instructions":"A汉😀"}}).to_string(),
            ),
        )
        .unwrap();
        // SplitSink 已接收进暂存槽，但真实套接字仍未 ready；取消 flush 不能产生字节。
        assert_no_accepted_payload(&io);
        {
            let flush = tx.flush();
            tokio::pin!(flush);
            assert!(futures::poll!(flush.as_mut()).is_pending());
        }
        drop(tx);
        assert_eq!(accepted.load(Ordering::Acquire), 0);
        assert_no_accepted_payload(&io);
    }

    #[tokio::test]
    async fn upstream_io_accepted_payload_survives_failed_flush_as_incomplete() {
        let (socket, accepted) = controlled_socket(ReadyState::Ready, false, true);
        let io = UpstreamIoObserver::websocket(Protocol::Realtime);
        let (mut tx, _rx) = ObservedUpstreamSocket::new(socket, io.clone()).split();
        let payload =
            json!({"type":"response.create", "response":{"instructions":"A汉😀"}}).to_string();
        assert!(tx.send(UpMessage::Text(payload.clone())).await.is_err());
        io.fail_request();
        assert_eq!(accepted.load(Ordering::Acquire), 1);
        let audit = io.snapshot();
        assert_eq!(audit.request_body_bytes, Some(payload.len() as u64));
        assert_eq!(audit.request_complete, Some(false));
        let text = audit.input_text.unwrap();
        assert_eq!(text.utf8_bytes, 8);
        assert_eq!(text.unicode_scalars, 3);
        assert!(!text.complete);
    }

    #[tokio::test]
    async fn upstream_io_successful_socket_send_counts_payload_once_across_flushes() {
        let (socket, accepted) = controlled_socket(ReadyState::Ready, false, false);
        let io = UpstreamIoObserver::websocket(Protocol::Responses);
        let mut socket = ObservedUpstreamSocket::new(socket, io.clone());
        let payload = json!({"type":"response.create", "input":"A汉😀"}).to_string();
        socket.send(UpMessage::Text(payload.clone())).await.unwrap();
        socket.flush().await.unwrap();
        socket.flush().await.unwrap();
        io.finish_request();
        assert_eq!(accepted.load(Ordering::Acquire), 1);
        let audit = io.snapshot();
        assert_eq!(audit.request_body_bytes, Some(payload.len() as u64));
        assert_eq!(audit.request_complete, Some(true));
        let text = audit.input_text.unwrap();
        assert_eq!(text.utf8_bytes, 8);
        assert_eq!(text.unicode_scalars, 3);
        assert!(text.complete);
    }

    #[tokio::test]
    async fn upstream_io_uses_raw_payloads_and_keeps_concurrent_lanes_separate() {
        let (gateway, provider) = tokio::io::duplex(64 * 1024);
        let gateway =
            tokio_tungstenite::WebSocketStream::from_raw_socket(gateway, Role::Client, None).await;
        let mut provider =
            tokio_tungstenite::WebSocketStream::from_raw_socket(provider, Role::Server, None).await;
        let (commands, receive) = mpsc::channel(4);
        let actor = tokio::spawn(run_upstream(gateway, receive));
        let mut rounds = Vec::new();
        for (lane, input, answer) in [("a", "A汉😀", "原始😀"), ("b", "longer input", "B")] {
            let io = UpstreamIoObserver::websocket(Protocol::Responses);
            let payload = json!({"type":"response.create", "stream_id":lane,
                "model":"provider-model", "input":input})
            .to_string();
            let (output, events) = mpsc::channel(8);
            commands
                .send(UpstreamCommand::Create {
                    lane: Some(lane.into()),
                    token: lane.into(),
                    payload: payload.clone(),
                    output,
                    expires: None,
                    io: io.clone(),
                })
                .await
                .unwrap();
            let actual = provider.next().await.unwrap().unwrap();
            assert_eq!(actual.to_text().unwrap(), payload);
            rounds.push((lane, input, answer, payload, io, events, 0_u64));
        }
        for (lane, _, answer, _, _, _, bytes) in &mut rounds {
            for event in [
                json!({"type":"response.output_text.delta", "stream_id":lane,
                    "response_id":format!("resp_{lane}"), "item_id":"item", "output_index":0,
                    "content_index":0, "delta":answer}),
                json!({"type":"response.output_text.done", "stream_id":lane,
                    "response_id":format!("resp_{lane}"), "item_id":"item", "output_index":0,
                    "content_index":0, "text":answer}),
                json!({"type":"response.completed", "stream_id":lane, "response":{
                    "id":format!("resp_{lane}"), "output":[{"type":"message", "id":"item",
                    "content":[{"type":"output_text", "text":answer}]}]}}),
            ] {
                let text = event.to_string();
                *bytes += text.len() as u64;
                provider.send(UpMessage::Text(text)).await.unwrap();
            }
        }
        // 接收端尚未读取，观测必须已经看到上游终态，不能依赖客户端交付进度。
        tokio::time::timeout(Duration::from_secs(2), async {
            while rounds
                .iter()
                .any(|(_, _, _, _, io, _, _)| !io.snapshot().response_complete)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for (_, input, answer, payload, io, mut events, response_bytes) in rounds {
            let audit = io.snapshot();
            assert_eq!(audit.transport.as_deref(), Some("websocket_payload"));
            assert_eq!(audit.request_body_bytes, Some(payload.len() as u64));
            assert_eq!(audit.response_body_bytes, Some(response_bytes));
            assert_eq!(audit.request_complete, Some(true));
            let observed_input = audit.input_text.unwrap();
            assert_eq!(observed_input.utf8_bytes, input.len() as u64);
            assert_eq!(observed_input.unicode_scalars, input.chars().count() as u64);
            let observed_output = audit.output_text.unwrap();
            assert_eq!(observed_output.utf8_bytes, answer.len() as u64);
            assert_eq!(
                observed_output.unicode_scalars,
                answer.chars().count() as u64
            );
            assert!(observed_output.complete);
            let mut synthetic_bytes = 0;
            while let Some(text) = events.recv().await {
                synthetic_bytes += native_event_frame(&text).len() as u64;
            }
            assert!(synthetic_bytes > response_bytes);
            assert_eq!(io.snapshot().response_body_bytes, Some(response_bytes));
        }
        drop(commands);
        tokio::time::timeout(Duration::from_secs(2), actor)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn upstream_io_cancel_keeps_observed_prefix_without_a_synthetic_error_body() {
        let (gateway, provider) = tokio::io::duplex(64 * 1024);
        let gateway =
            tokio_tungstenite::WebSocketStream::from_raw_socket(gateway, Role::Client, None).await;
        let mut provider =
            tokio_tungstenite::WebSocketStream::from_raw_socket(provider, Role::Server, None).await;
        let (commands, receive) = mpsc::channel(4);
        let actor = tokio::spawn(run_upstream(gateway, receive));
        let (output, mut events) = mpsc::channel(8);
        let io = UpstreamIoObserver::websocket(Protocol::Responses);
        let payload = json!({"type":"response.create", "input":"before cancel"}).to_string();
        commands
            .send(UpstreamCommand::Create {
                lane: None,
                token: "cancelled".into(),
                payload: payload.clone(),
                output,
                expires: None,
                io: io.clone(),
            })
            .await
            .unwrap();
        provider.next().await.unwrap().unwrap();
        let prefix = json!({"type":"response.output_text.delta", "response_id":"resp_cancel",
            "item_id":"item", "output_index":0, "content_index":0, "delta":"A汉😀"})
        .to_string();
        provider
            .send(UpMessage::Text(prefix.clone()))
            .await
            .unwrap();
        assert_eq!(events.recv().await.as_deref(), Some(prefix.as_str()));
        commands
            .send(UpstreamCommand::Cancel {
                lane: None,
                token: "cancelled".into(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), actor)
            .await
            .unwrap()
            .unwrap();
        let audit = io.snapshot();
        assert_eq!(audit.request_body_bytes, Some(payload.len() as u64));
        assert_eq!(audit.response_body_bytes, Some(prefix.len() as u64));
        assert!(!audit.response_complete);
        let output = audit.output_text.unwrap();
        assert_eq!(output.utf8_bytes, 8);
        assert_eq!(output.unicode_scalars, 3);
        assert!(!output.complete);
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn upstream_io_binary_failure_counts_raw_prefix_without_a_synthetic_body() {
        let (gateway, provider) = tokio::io::duplex(64 * 1024);
        let gateway =
            tokio_tungstenite::WebSocketStream::from_raw_socket(gateway, Role::Client, None).await;
        let mut provider =
            tokio_tungstenite::WebSocketStream::from_raw_socket(provider, Role::Server, None).await;
        let (commands, receive) = mpsc::channel(4);
        let actor = tokio::spawn(run_upstream(gateway, receive));
        let (output, mut events) = mpsc::channel(8);
        let io = UpstreamIoObserver::websocket(Protocol::Responses);
        commands
            .send(UpstreamCommand::Create {
                lane: None,
                token: "binary".into(),
                payload: json!({"type":"response.create", "input":"hello"}).to_string(),
                output,
                expires: None,
                io: io.clone(),
            })
            .await
            .unwrap();
        provider.next().await.unwrap().unwrap();
        provider
            .send(UpMessage::Binary(vec![0xff, 0x00, 0x42]))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), actor)
            .await
            .unwrap()
            .unwrap();
        let audit = io.snapshot();
        assert_eq!(audit.response_body_bytes, Some(3));
        assert!(!audit.response_complete);
        assert!(audit.output_text.is_none_or(|output| !output.complete));
        assert!(events.recv().await.is_none());
    }
}
