//! 真实 CP/PG/kine/mTLS 与生产网关的多尾量排空联合组件合同。
//! 供应商为本地 WS 协议组件；透明 TCP 延迟器只延迟已在途字节，不冒充供应商 E2E。

#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::native_metering::{ReconciliationRequest, TypedCompleteRequest};
use aisix_core::native_objects::*;
use aisix_core::native_realtime::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::ProxyConfig;
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::native_api::registry::{
    LiveUsageAuthority, NativeObjectRegistry, NativeRegistryError,
};
use aisix_proxy::{build_router, ProxyState};
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures::{future::BoxFuture, SinkExt as _, StreamExt as _};
use hyper_util::rt::TokioIo;
use native_gateway::{projected_snapshot, HttpRegistry};
use serde_json::{json, Value};
use support::{Bundle, OperatorSession, TestCp};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, Message};
use tower::ServiceExt as _;
use uuid::Uuid;

const MODEL: &str = "realtime-ws-paid-tail-queue-public";
const UPSTREAM: &str = "gpt-realtime";
const PROJECT: &str = "proj_ws_paid_tail_queue_component";
const SESSION: &str = "sess_ws_paid_tail_queue_component";
const FIRST: &str = "resp_ws_tail_queue_positive";
const TAIL_A: &str = "resp_ws_tail_queue_distinct_a";
const TAIL_B: &str = "resp_ws_tail_queue_distinct_b";
const IO_LIMIT: Duration = Duration::from_secs(10);
const HELD_ACK: Duration = Duration::from_millis(3250);
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct AppendGate {
    armed: AtomicBool,
    held_at: Mutex<Option<Instant>>,
    held_operation: Mutex<Option<Uuid>>,
    release: Semaphore,
    canceled: AtomicBool,
    successful_nonempty: AtomicUsize,
}

impl AppendGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(false),
            held_at: Mutex::new(None),
            held_operation: Mutex::new(None),
            release: Semaphore::new(0),
            canceled: AtomicBool::new(false),
            successful_nonempty: AtomicUsize::new(0),
        })
    }

    async fn held(&self) -> Instant {
        tokio::time::timeout(IO_LIMIT, async {
            loop {
                if let Some(at) = *self.held_at.lock().expect("actual append ack observation") {
                    return at;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the first actual nonzero CP append commits before its real ack is held")
    }
}

struct HeldAck<'a> {
    gate: &'a AppendGate,
    returned: bool,
}

impl Drop for HeldAck<'_> {
    fn drop(&mut self) {
        if !self.returned {
            self.gate.canceled.store(true, Ordering::Release);
        }
    }
}

// 所有调用都沿原 mTLS adapter；只持有一次真实持久 append 的原样成功回答。
struct GatedRegistry {
    real: HttpRegistry,
    gate: Arc<AppendGate>,
}

macro_rules! delegate {
    ($method:ident, $request:ty) => {
        fn $method(
            &self,
            request: $request,
        ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            self.real.$method(request)
        }
    };
}

impl NativeObjectRegistry for GatedRegistry {
    fn live_usage_authority(&self) -> LiveUsageAuthority {
        self.real.live_usage_authority()
    }
    fn id_key(&self) -> Option<[u8; 32]> {
        self.real.id_key()
    }
    fn resource_id(&self, kind: &str, raw: &str) -> Result<Uuid, NativeRegistryError> {
        self.real.resource_id(kind, raw)
    }
    delegate!(begin, BeginRequest);
    delegate!(bind, BindRequest);
    delegate!(lookup, LookupRequest);
    delegate!(owned, OwnedRequest);
    delegate!(tombstone, TombstoneRequest);
    delegate!(claim, ClaimRequest);
    delegate!(renew, LeaseRequest);
    delegate!(reschedule, LeaseRequest);
    delegate!(complete, CompleteRequest);
    delegate!(begin_related, BeginRelatedRequest);
    delegate!(bind_related, BindRelatedRequest);
    delegate!(reserve_relations, ReserveRelationsRequest);
    delegate!(commit_relations, CommitRelationsRequest);
    delegate!(lookup_relations, LookupRelationsRequest);
    delegate!(resolve_reservation, ResolveReservationRequest);
    delegate!(bind_observed_artifacts, BindObservedArtifactsRequest);
    delegate!(reserve_delete, ObjectDeletionRequest);
    delegate!(commit_delete, ObjectDeletionRequest);
    delegate!(complete_typed, TypedCompleteRequest);
    delegate!(mark_reconciliation, ReconciliationRequest);
    delegate!(mint_credential, MintCredentialRequest);
    delegate!(redeem_credential, RedeemCredentialRequest);
    delegate!(create_live_session, CreateLiveSessionRequest);
    delegate!(checkpoint_live_session, CheckpointLiveSessionRequest);
    delegate!(claim_live_sessions, ClaimLiveSessionsRequest);
    delegate!(signed_sip_intake, SignedSipIntakeRequest);

    fn append_live_usage(
        &self,
        request: AppendLiveUsageRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let nonempty = !request.usage.is_empty();
            let hold = nonempty && self.gate.armed.swap(false, Ordering::AcqRel);
            let operation = request.operation_id;
            let reply = self.real.append_live_usage(request).await;
            if nonempty && reply.is_ok() {
                self.gate.successful_nonempty.fetch_add(1, Ordering::AcqRel);
            }
            if hold {
                assert_eq!(reply.as_ref().expect("the delayed ack belongs to a successful real CP append").appended,Some(true),
                    "the first held supplier fact was really appended, not a fabricated reply or duplicate");
                let mut guard = HeldAck {
                    gate: &self.gate,
                    returned: false,
                };
                *self
                    .gate
                    .held_operation
                    .lock()
                    .expect("held append identity") = Some(operation);
                *self
                    .gate
                    .held_at
                    .lock()
                    .expect("actual append ack observation") = Some(Instant::now());
                self.gate
                    .release
                    .acquire()
                    .await
                    .expect("controlled actual ack delay")
                    .forget();
                guard.returned = true;
            }
            reply
        })
    }
}

// 只观察公开 WS 帧边界；字节本身一字不改，不解析产品请求或用量来选择 expected。
#[derive(Default)]
struct FrameObserver {
    bytes: Vec<u8>,
    upgraded: bool,
}

impl FrameObserver {
    fn observe(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.bytes.extend_from_slice(bytes);
        if !self.upgraded {
            let Some(end) = self
                .bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            else {
                assert!(
                    self.bytes.len() < 65536,
                    "the component handshake header is bounded"
                );
                return Vec::new();
            };
            self.bytes.drain(..end + 4);
            self.upgraded = true;
        }
        let mut opcodes = Vec::new();
        loop {
            if self.bytes.len() < 2 {
                break;
            }
            let mut offset = 2;
            let length = match self.bytes[1] & 0x7f {
                126 => {
                    if self.bytes.len() < 4 {
                        break;
                    }
                    offset = 4;
                    u16::from_be_bytes([self.bytes[2], self.bytes[3]]) as usize
                }
                127 => {
                    if self.bytes.len() < 10 {
                        break;
                    }
                    offset = 10;
                    usize::try_from(u64::from_be_bytes(
                        self.bytes[2..10].try_into().expect("eight WS length bytes"),
                    ))
                    .expect("bounded test WS length")
                }
                value => value as usize,
            };
            if self.bytes[1] & 0x80 != 0 {
                offset += 4;
            }
            assert!(length < 4 << 20, "the component frame length is bounded");
            if self.bytes.len() < offset + length {
                break;
            }
            opcodes.push(self.bytes[0] & 0x0f);
            self.bytes.drain(..offset + length);
        }
        opcodes
    }
}

struct RelayTransportState {
    holding: AtomicBool,
    buffered_text_frames: AtomicUsize,
    connections: AtomicUsize,
    close_seen: AtomicBool,
    release: Notify,
    delivered: AtomicBool,
    delivered_notify: Notify,
}

struct DelayedTransport {
    base: String,
    state: Arc<RelayTransportState>,
    task: tokio::task::JoinHandle<()>,
}

impl DelayedTransport {
    async fn start(upstream: std::net::SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the transparent byte-delay component");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("transport address")
        );
        let state = Arc::new(RelayTransportState {
            holding: AtomicBool::new(false),
            buffered_text_frames: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            close_seen: AtomicBool::new(false),
            release: Notify::new(),
            delivered: AtomicBool::new(false),
            delivered_notify: Notify::new(),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (downstream,_)=accepted.expect("accept the configured gateway upstream TCP connection");
                        shared.connections.fetch_add(1,Ordering::AcqRel);
                        let state=shared.clone();
                        connections.spawn(async move {
                            let upstream=tokio::net::TcpStream::connect(upstream).await.expect("connect the real loopback WS supplier");
                            let (mut client_read,mut client_write)=downstream.into_split();
                            let (mut server_read,mut server_write)=upstream.into_split();
                            let toward_supplier=async {
                                let mut observer=FrameObserver::default();
                                let mut bytes=[0_u8;16384];
                                loop {
                                    let length=client_read.read(&mut bytes).await.expect("read actual gateway transport bytes");
                                    if length==0 { break; }
                                    let opcodes=observer.observe(&bytes[..length]);
                                    if state.holding.load(Ordering::Acquire) && opcodes.contains(&8) {
                                        state.close_seen.store(true,Ordering::Release);
                                        state.release.notify_one();
                                        loop {
                                            let delivered=state.delivered_notify.notified();
                                            if state.delivered.load(Ordering::Acquire) { break; }
                                            delivered.await;
                                        }
                                    }
                                    if server_write.write_all(&bytes[..length]).await.is_err() { break; }
                                }
                                let _=server_write.shutdown().await;
                            };
                            let toward_gateway=async {
                                let mut observer=FrameObserver::default();
                                let mut bytes=[0_u8;16384];
                                let mut buffered=Vec::new();
                                let mut released=false;
                                loop {
                                    tokio::select! {
                                        _=state.release.notified(),if !released=>{
                                            client_write.write_all(&buffered).await.expect("deliver already-in-flight supplier tails after actual gateway Close");
                                            client_write.flush().await.expect("flush original supplier bytes unchanged");
                                            buffered.clear();
                                            released=true;
                                            state.delivered.store(true,Ordering::Release);
                                            state.delivered_notify.notify_waiters();
                                        }
                                        length=server_read.read(&mut bytes)=>{
                                            let length=length.expect("read actual supplier transport bytes");
                                            if length==0 { break; }
                                            let opcodes=observer.observe(&bytes[..length]);
                                            if state.holding.load(Ordering::Acquire) && !released {
                                                buffered.extend_from_slice(&bytes[..length]);
                                                state.buffered_text_frames.fetch_add(opcodes.iter().filter(|opcode|**opcode==1).count(),Ordering::AcqRel);
                                            } else if client_write.write_all(&bytes[..length]).await.is_err() { break; }
                                        }
                                    }
                                }
                                let _=client_write.shutdown().await;
                            };
                            tokio::join!(toward_supplier,toward_gateway);
                        });
                    }
                    done=connections.join_next(),if !connections.is_empty()=>{
                        if let Some(done)=done { done.expect("the transparent transport component must not panic"); }
                    }
                }
            }
        });
        Self { base, state, task }
    }

    async fn buffered(&self, count: usize) {
        tokio::time::timeout(IO_LIMIT,async {
            while self.state.buffered_text_frames.load(Ordering::Acquire)<count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("all distinct supplier tails and redeliveries are legally on the wire before gateway Close");
        assert_eq!(
            self.state.buffered_text_frames.load(Ordering::Acquire),
            count
        );
        assert!(
            !self.state.close_seen.load(Ordering::Acquire),
            "no supplier tail was emitted after this peer received Close"
        );
    }
}

impl Drop for DelayedTransport {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ProviderState {
    secret: String,
    output: Mutex<Option<mpsc::UnboundedSender<(Message, oneshot::Sender<()>)>>>,
    received: Mutex<Vec<Value>>,
    handshakes: Mutex<Vec<bool>>,
    closed: AtomicBool,
}

struct Provider {
    base: String,
    state: Arc<ProviderState>,
    transport: DelayedTransport,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the real loopback supplier component");
        let address = listener.local_addr().expect("actual supplier address");
        let transport = DelayedTransport::start(address).await;
        let base = transport.base.clone();
        let state = Arc::new(ProviderState {
            secret: format!("sk-ws-tail-queue-component-{}", Uuid::new_v4().simple()),
            output: Mutex::new(None),
            received: Mutex::new(Vec::new()),
            handshakes: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("accept the actual supplier TCP connection");
            let observed = shared.clone();
            let mut socket=tokio_tungstenite::accept_hdr_async(stream,
                move |request:&tokio_tungstenite::tungstenite::handshake::server::Request,response| {
                    let authorized=request.headers().get("authorization").and_then(|value|value.to_str().ok())
                        ==Some(format!("Bearer {}",observed.secret).as_str());
                    let project=request.headers().get("openai-project").and_then(|value|value.to_str().ok())==Some(PROJECT);
                    let route=request.uri().path()=="/v1/realtime" && request.uri().query()==Some(format!("model={UPSTREAM}").as_str());
                    observed.handshakes.lock().expect("actual supplier binding").push(authorized && project && route);
                    Ok(response)
                }).await.expect("perform the actual supplier WebSocket handshake through the transparent byte relay");
            let (sender, mut output) = mpsc::unbounded_channel::<(Message, oneshot::Sender<()>)>();
            *shared.output.lock().expect("supplier output") = Some(sender);
            socket.send(Message::Text(json!({"type":"session.created","event_id":"paid_tail_queue_session_created",
                "session":{"type":"realtime","object":"realtime.session","id":SESSION,"model":UPSTREAM,
                "output_modalities":["audio"],"tools":[],"tool_choice":"auto","max_output_tokens":"inf",
                "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"turn_detection":null},
                    "output":{"format":{"type":"audio/pcm","rate":24000},"voice":"marin"}},
                "expires_at":Utc::now().timestamp()+3600}}).to_string())).await.expect("write the actual supplier session");
            loop {
                tokio::select! {
                    message=output.recv()=>{
                        let Some((message,written))=message else { break; };
                        if socket.send(message).await.is_err() { break; }
                        let _=written.send(());
                    }
                    message=socket.next()=>{
                        match message {
                            Some(Ok(Message::Text(text)))=>{
                                let event:Value=serde_json::from_str(&text).expect("actual client WS event JSON");
                                shared.received.lock().expect("actual supplier events").push(event);
                            }
                            Some(Ok(Message::Ping(bytes)))=>{ if socket.send(Message::Pong(bytes)).await.is_err() { break; } }
                            Some(Ok(Message::Close(_)))=>{
                                // TCP 延迟器先交付此前已在途的尾量，才把 Close 交给此供应商。
                                let _=socket.flush().await;
                                break;
                            }
                            None | Some(Err(_))=>break,
                            _=>{}
                        }
                    }
                }
            }
            shared.closed.store(true, Ordering::Release);
        });
        Self {
            base,
            state,
            transport,
            task,
        }
    }

    async fn send(&self, event: Value) {
        let (written, observed) = oneshot::channel();
        self.state
            .output
            .lock()
            .expect("supplier output")
            .as_ref()
            .expect("actual supplier connected")
            .send((Message::Text(event.to_string()), written))
            .expect("queue the actual supplier event before Close");
        tokio::time::timeout(IO_LIMIT, observed)
            .await
            .expect("actual supplier write completes")
            .expect("actual supplier frame was flushed onto the real TCP connection");
    }

    async fn received(&self, event_id: &str, limit: Duration) -> Value {
        tokio::time::timeout(limit, async {
            loop {
                if let Some(event) = self
                    .state
                    .received
                    .lock()
                    .expect("supplier input")
                    .iter()
                    .find(|event| event.get("event_id") == Some(&json!(event_id)))
                    .cloned()
                {
                    return event;
                }
                assert!(
                    !self.state.closed.load(Ordering::Acquire),
                    "the supplier closed before accepting expected real work"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the real supplier receives the identified actual client event")
    }

    async fn drained(&self) {
        tokio::time::timeout(IO_LIMIT,async {
            while !self.state.closed.load(Ordering::Acquire) || !self.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("the actual supplier drains and completes its legal Close handshake within a finite budget");
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// 下列资源、真实网关和计价 setup 沿用已通过的 WS 联合组件 fixture；不改原文件。
struct Fixture {
    cp: TestCp,
    provider: Provider,
    bundle: Bundle,
    tenant: Uuid,
    key: Uuid,
    model: Uuid,
    bearer: String,
}

fn id(body: &Value, pointer: &str) -> Uuid {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
        .expect("an explicit actual resource UUID")
}

async fn admin_post(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let response = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(
        response.status, 201,
        "the real public admin fixture write succeeds"
    );
    response.body
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start_with_kine().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = Provider::start().await;
    let account = admin_post(&cp, &admin, "/admin/resources/provider_keys", json!({"doc":{
        "display_name":"realtime-ws-relay-account","provider":"openai","adapter":"openai","api_base":provider.base,
        "api_key":provider.state.secret,"groups":["shared"],"request":{"default_headers":{"OpenAI-Project":PROJECT}}
    }})).await;
    let model = admin_post(&cp, &admin, "/admin/resources/models", json!({"doc":{
        "display_name":MODEL,"provider":"openai","model_name":UPSTREAM,"provider_key_id":id(&account,"/id")
    }})).await;
    let model = id(&model, "/id");
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let path = format!("/admin/tenants/{tenant}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    let granted = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"]
                .as_i64()
                .expect("actual tenant version"),
            json!({"allowed_model_ids":[model]}),
        )
        .await;
    assert_eq!(
        granted.status, 200,
        "the real tenant is granted the selected realtime model"
    );
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/credits"),
        json!({"amount_cny":"1","reason":"轻隔离 WS 真实非零计费正控"}),
    )
    .await;
    admin_post(&cp, &admin, &format!("/admin/provider-groups/{group}/rates"),
        json!({"model_id":null,"cny_per_usd":"1","effective_from":support::world::db_now(&cp).await})).await;
    admin_post(&cp, &admin, "/admin/pricing/baselines", json!({"model_id":model,
        "effective_from":support::world::db_now(&cp).await,"note":"WS 独立且非零价格",
        "card":{"default":{"base":{"input":"10","audio_input":"10","cache_read":"2","output":"20","audio_output":"20"}}}})).await;
    let key = admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":"ws-relay-real-key","member_id":member,"group_id":group}),
    )
    .await;
    let bearer = key["plaintext"]
        .as_str()
        .expect("the actual issued bearer")
        .to_owned();
    let key = id(&key, "/key/id");
    let bundle = cp.issue_bundle(&admin, "ws-relay-real-gateway").await;
    Some(Fixture {
        cp,
        provider,
        bundle,
        tenant,
        key,
        model,
        bearer,
    })
}

struct Gateway {
    base: String,
    billing: mpsc::Receiver<UsageEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl Gateway {
    async fn start(f: &Fixture, gate: Arc<AppendGate>) -> Self {
        let hub = Arc::new(Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        let config: ProxyConfig = serde_json::from_value(json!({
            "addr":"127.0.0.1:0","request_body_limit_bytes":1048576
        }))
        .expect("the normal gateway startup configuration");
        let (sender, billing) = mpsc::channel(128);
        let state = ProxyState::new(
            SnapshotHandle::new(projected_snapshot(&f.cp).await),
            hub,
            &config,
        )
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            &f.cp.state().pki.platform_id_key_hex(),
        ))
        .with_native_objects(Arc::new(GatedRegistry {
            real: HttpRegistry::new(&f.cp, &f.bundle),
            gate,
        }))
        .with_usage_sink(UsageSink::new(sender));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the production gateway router");
        let base = format!("ws://{}", listener.local_addr().expect("gateway address"));
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (socket, _) = accepted.expect("accept the actual client connection");
                        let app = build_router(state.clone());
                        connections.spawn(async move {
                            let service = hyper::service::service_fn(move |request:Request<hyper::body::Incoming>|app.clone().oneshot(request));
                            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(socket),service).with_upgrades().await;
                        });
                    }
                    done=connections.join_next(), if !connections.is_empty()=>{
                        if let Some(done)=done { done.expect("the actual gateway connection must not panic"); }
                    }
                }
            }
        });
        Self {
            base,
            billing,
            task,
        }
    }

    async fn connect(&self, f: &Fixture) -> Socket {
        let mut request = format!("{}/v1/realtime?model={MODEL}", self.base)
            .into_client_request()
            .expect("official client WS request");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", f.bearer)
                .parse()
                .expect("actual bearer header"),
        );
        let (mut socket, response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("the real gateway accepts the authenticated ordinary WS");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        let event = next_event(&mut socket, IO_LIMIT).await;
        assert!(
            event.get("type") == Some(&json!("session.created"))
                && event.pointer("/session/id") == Some(&json!(SESSION)),
            "the configured provider actually opens the original session"
        );
        assert!(
            f.provider
                .state
                .handshakes
                .lock()
                .expect("provider binding")
                .as_slice()
                == [true],
            "the real handshake uses the configured upstream account, project, route and model"
        );
        socket
    }

    async fn no_aggregate(&mut self) {
        assert!(
            matches!(
                self.billing.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "canonical facts cannot enqueue an additional ordinary billing aggregate"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), self.billing.recv())
                .await
                .is_err(),
            "no delayed aggregate may bill the same accepted responses again"
        );
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn next_event(socket: &mut Socket, limit: Duration) -> Value {
    tokio::time::timeout(limit, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text).expect("official server event JSON")
                }
                Some(Ok(Message::Ping(bytes))) => socket
                    .send(Message::Pong(bytes))
                    .await
                    .expect("answer actual WS ping"),
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    panic!("the actual WS closed before its expected official event")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the actual WS event arrives within the independent relay budget")
}

async fn client_event(socket: &mut Socket, event: Value) {
    socket
        .send(Message::Text(event.to_string()))
        .await
        .expect("write the actual client WS event");
}

fn pcm(seed: i16) -> Vec<u8> {
    (0..2400_i16)
        .flat_map(|sample| sample.wrapping_mul(seed).to_le_bytes())
        .collect()
}

async fn pcm_roundtrip(
    f: &Fixture,
    socket: &mut Socket,
    response: &str,
    suffix: &str,
    limit: Duration,
) {
    let input = pcm(7);
    let input_id = format!("relay_input_{suffix}");
    client_event(
        socket,
        json!({"type":"input_audio_buffer.append","event_id":input_id,
        "audio":base64::engine::general_purpose::STANDARD.encode(&input)}),
    )
    .await;
    let actual = f.provider.received(&input_id, limit).await;
    assert_eq!(
        actual.get("type"),
        Some(&json!("input_audio_buffer.append"))
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(actual["audio"].as_str().expect("actual PCM payload"))
        .expect("valid PCM base64");
    assert_eq!(
        decoded, input,
        "every actual input PCM byte reaches the configured provider unchanged"
    );
    let output = pcm(11);
    let event_id = format!("relay_output_{suffix}");
    f.provider.send(json!({"type":"response.output_audio.delta","event_id":event_id,
        "response_id":response,"item_id":format!("item_{response}"),"output_index":0,"content_index":0,
        "delta":base64::engine::general_purpose::STANDARD.encode(&output)})).await;
    let actual = next_event(socket, limit).await;
    assert!(
        actual.get("type") == Some(&json!("response.output_audio.delta"))
            && actual.get("event_id") == Some(&json!(event_id)),
        "the client receives the exact identified provider audio delta"
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(actual["delta"].as_str().expect("actual output PCM"))
        .expect("valid output PCM base64");
    assert_eq!(
        decoded, output,
        "every actual output PCM byte reaches the client unchanged"
    );
}

async fn accepted_response(f: &Fixture, socket: &mut Socket, response: &str) {
    let event_id = format!("start_{response}");
    client_event(
        socket,
        json!({"type":"response.create","event_id":event_id,
        "response":{"conversation":"none","output_modalities":["audio"]}}),
    )
    .await;
    let input = f.provider.received(&event_id, IO_LIMIT).await;
    assert_eq!(input.get("type"), Some(&json!("response.create")));
    f.provider.send(json!({"type":"response.created","event_id":format!("created_{response}"),
        "response":{"object":"realtime.response","id":response,"status":"in_progress","output":[]}})).await;
    let created = next_event(socket, IO_LIMIT).await;
    assert!(
        created.get("type") == Some(&json!("response.created"))
            && created.pointer("/response/id") == Some(&json!(response)),
        "the original upstream explicitly accepts this paid response before any stop/expiry"
    );
}

fn done(response: &str, event_id: &str) -> Value {
    json!({"type":"response.done","event_id":event_id,"response":{
        "object":"realtime.response","id":response,"status":"completed","status_details":null,
        "output":[{"id":format!("item_{response}"),"object":"realtime.item","type":"message","status":"completed","role":"assistant","content":[{"type":"audio","transcript":""}]}],
        "usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,
            "input_token_details":{"cached_tokens":2,"text_tokens":0,"audio_tokens":10,"cached_tokens_details":{"text_tokens":0,"audio_tokens":2}},
            "output_token_details":{"text_tokens":0,"audio_tokens":3}}}})
}

// 价格是人工计算的 8*10+2*2+3*20=144；查询只读真实 canonical/outbox/intake/ledger。
async fn paid(f: &Fixture, number: i64) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rows:(i64,i64,i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM native_live_journal), (SELECT count(*) FROM native_live_outbox), (SELECT count(*) FROM pricing_charges_cny WHERE tenant_id=$1::text AND status='charged'), count(*),COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'")
                .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("read actual canonical and paid quantities");
            assert!(rows.0<=number && rows.1<=number && rows.2<=number && rows.3<=number && rows.4<=number*144,
                "each accepted response is journaled, delivered, priced and debited at most once: {rows:?}");
            if rows == (number,number,number,number,number*144) { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the real WS observer and billing pipeline pay each explicit nonzero response once");
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(f.tenant)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("read actual funded tenant balance");
    assert_eq!(balance, 1_000_000 - number * 144);
    let events: Vec<Value> = sqlx::query_scalar("SELECT event FROM native_live_outbox")
        .fetch_all(f.cp.db().pool())
        .await
        .expect("read actual canonical identity without logging raw event bodies");
    assert_eq!(events.len(), number as usize);
    for event in events {
        assert!(
            event.get("api_key_id") == Some(&json!(f.key))
                && event.get("model_id") == Some(&json!(f.model))
                && event.get("requested_model_id") == Some(&json!(f.model)),
            "neither stop nor delayed renewal can change the actual payer or model"
        );
        for (field, expected) in [
            ("prompt_tokens", 10),
            ("completion_tokens", 3),
            ("cached_prompt_tokens", 2),
            ("total_tokens", 13),
        ] {
            assert_eq!(
                event.get(field),
                Some(&json!(expected)),
                "an explicit supplier quantity remains present"
            );
        }
    }
}

#[derive(sqlx::FromRow)]
struct LeaseSample {
    operation_id: Uuid,
    lease_generation: i64,
    lease_expires_at: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
    state: String,
    db_now: DateTime<Utc>,
}

async fn lease_sample(f: &Fixture) -> LeaseSample {
    let mut rows:Vec<LeaseSample> = sqlx::query_as("SELECT operation_id,lease_generation,lease_expires_at,deadline_at,state,clock_timestamp() AS db_now FROM native_live_sessions WHERE environment_id=$1 AND api_key_id=$2")
        .bind(f.cp.env().env_id).bind(f.key).fetch_all(f.cp.db().pool()).await.expect("read the actual lease and real database clock");
    assert_eq!(
        rows.len(),
        1,
        "the public WS creates one actual live session"
    );
    rows.remove(0)
}

// 可击穿点：把真实 CP append 的等待放进2秒 socket drain timeout，第一份账已提交却丢掉第二份在途尾量。
#[tokio::test]
async fn two_distinct_paid_tails_survive_first_real_append_ack_held_longer_than_socket_drain_budget(
) {
    let Some(f) = fixture().await else {
        return;
    };
    let gate = AppendGate::new();
    let mut gateway = Gateway::start(&f, gate.clone()).await;
    let mut socket = gateway.connect(&f).await;

    accepted_response(&f, &mut socket, FIRST).await;
    pcm_roundtrip(
        &f,
        &mut socket,
        FIRST,
        "healthy_before_two_paid_tails",
        IO_LIMIT,
    )
    .await;
    let positive = done(FIRST, "positive_real_nonzero_paid_fact");
    f.provider.send(positive.clone()).await;
    let observed = next_event(&mut socket, IO_LIMIT).await;
    assert!(
        observed.get("type") == Some(&json!("response.done"))
            && observed.pointer("/response/usage") == positive.pointer("/response/usage"),
        "real healthy PCM and explicit supplier usage arrive before the controlled close fault"
    );
    paid(&f, 1).await;

    // 官方 conversation:none 允许同时生成两份响应，避免伪造两份默认 conversation 并发。
    accepted_response(&f, &mut socket, TAIL_A).await;
    accepted_response(&f, &mut socket, TAIL_B).await;
    let before = lease_sample(&f).await;
    assert!(!before.operation_id.is_nil() && before.lease_generation>0
        && before.state=="live" && before.lease_expires_at>before.db_now
        && before.deadline_at>before.db_now,
        "both pending responses belong to the original live owner/model/known lease before ordinary client Close");

    gate.armed.store(true, Ordering::Release);
    f.provider
        .transport
        .state
        .holding
        .store(true, Ordering::Release);
    f.provider
        .send(done(TAIL_A, "distinct_tail_a_before_close"))
        .await;
    f.provider
        .send(done(TAIL_B, "distinct_tail_b_before_close"))
        .await;
    f.provider
        .send(done(TAIL_A, "distinct_tail_a_redelivery_before_close"))
        .await;
    f.provider
        .send(done(TAIL_B, "distinct_tail_b_redelivery_before_close"))
        .await;
    f.provider.transport.buffered(4).await;
    assert_eq!(gate.successful_nonempty.load(Ordering::Acquire),1,
        "only the healthy positive fact has reached CP; neither buffered supplier tail was consumed by normal relay");

    socket
        .close(None)
        .await
        .expect("the actual client initiates a legal ordinary WS Close");
    let held_at = gate.held().await;
    assert!(f.provider.transport.state.close_seen.load(Ordering::Acquire)
        && f.provider.transport.state.delivered.load(Ordering::Acquire),
        "actual gateway Close reaches the transparent transport before the unchanged in-flight supplier tails are delivered");
    assert_eq!(
        *gate
            .held_operation
            .lock()
            .expect("actual held append operation"),
        Some(before.operation_id),
        "the delayed real CP append belongs to the original admitted operation"
    );
    let committed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_live_journal WHERE operation_id=$1")
            .bind(before.operation_id)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("observe the real committed journal while its original ack is held");
    assert!(committed>=2,"the healthy fact and first distinct paid tail truly committed before delaying the actual ack");

    tokio::time::sleep(HELD_ACK.saturating_sub(held_at.elapsed())).await;
    assert!(held_at.elapsed()>=HELD_ACK,
        "the actual first append ack was withheld for longer than the original2-second socket drain budget");
    // 是否发生取消只作精确运行证据；允许可靠队列重试，合同要求所有独立事实最终完整一次结算。
    eprintln!(
        "two-tail component diagnostic: first_ack_canceled={} held_ms={}",
        gate.canceled.load(Ordering::Acquire),
        held_at.elapsed().as_millis()
    );
    gate.release.add_permits(1);

    f.provider.drained().await;
    paid(&f, 3).await;
    gateway.no_aggregate().await;
    paid(&f, 3).await;
    assert_eq!(
        f.provider
            .transport
            .state
            .connections
            .load(Ordering::Acquire),
        1,
        "no replacement provider dispatch repairs a dropped accepted tail"
    );
    assert!(
        f.provider
            .state
            .handshakes
            .lock()
            .expect("all actual supplier handshakes")
            .as_slice()
            == [true],
        "all paid work stays on the same real WS account/project/model binding"
    );
    let creates = f
        .provider
        .state
        .received
        .lock()
        .expect("drained supplier input")
        .iter()
        .filter(|event| event.get("type") == Some(&json!("response.create")))
        .count();
    assert_eq!(creates,3,"one healthy response and exactly two previously accepted distinct responses; no hidden redispatch");
    tokio::time::timeout(IO_LIMIT, async {
        loop {
            let actual = lease_sample(&f).await;
            assert_eq!(actual.operation_id, before.operation_id);
            assert_eq!(actual.lease_generation, before.lease_generation);
            if actual.state == "closed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the original session completes finite durable cleanup after the real ack is released");
}
