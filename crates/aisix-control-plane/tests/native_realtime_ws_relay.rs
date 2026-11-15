//! 轻隔离 WS 的真实 CP/PG/kine/mTLS 与生产网关联合组件合同。
//! 本地供应商只实现公开 WS 协议；不声称付费供应商或媒体端到端覆盖。

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
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, Message};
use tower::ServiceExt as _;
use uuid::Uuid;

const MODEL: &str = "realtime-ws-relay-public";
const UPSTREAM: &str = "gpt-realtime";
const PROJECT: &str = "proj_ws_relay_component";
const SESSION: &str = "sess_ws_relay_component";
const FIRST: &str = "resp_ws_relay_paid_positive";
const TAIL: &str = "resp_ws_relay_accepted_tail";
const IO_LIMIT: Duration = Duration::from_secs(10);
const RELAY_LIMIT: Duration = Duration::from_secs(3);
const RENEWAL_OBSERVATION: Duration = Duration::from_secs(12);
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum HoldPhase {
    BeforeRpc,
    AfterReply,
    AfterStopReply,
}

#[derive(Clone)]
struct HeldCheckpoint {
    phase: HoldPhase,
    request: CheckpointLiveSessionRequest,
    reply: Option<LiveSessionRecord>,
    entered_at: Instant,
    calls_at_entry: usize,
    last_confirmed_expiry: DateTime<Utc>,
}

struct CheckpointGate {
    armed: Mutex<Option<HoldPhase>>,
    held: Mutex<Option<HeldCheckpoint>>,
    release: Semaphore,
    returned: AtomicBool,
    periodic_calls: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    canceled: AtomicBool,
    last_confirmed_expiry: Mutex<Option<DateTime<Utc>>>,
}

impl CheckpointGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: Mutex::new(None),
            held: Mutex::new(None),
            release: Semaphore::new(0),
            returned: AtomicBool::new(false),
            periodic_calls: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            canceled: AtomicBool::new(false),
            last_confirmed_expiry: Mutex::new(None),
        })
    }

    fn arm(&self, phase: HoldPhase) {
        assert!(self.held.lock().expect("checkpoint observation").is_none());
        *self.armed.lock().expect("one controlled checkpoint") = Some(phase);
    }

    async fn held(&self) -> HeldCheckpoint {
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                if let Some(held) = self.held.lock().expect("checkpoint observation").clone() {
                    return held;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("an actual periodic authority call reaches the explicit transport/reply gate")
    }

    async fn hold(&self, held: HeldCheckpoint) {
        self.held
            .lock()
            .expect("checkpoint observation")
            .get_or_insert(held);
        self.release
            .acquire()
            .await
            .expect("the controlled checkpoint gate remains available")
            .forget();
        self.returned.store(true, Ordering::Release);
    }

    fn open(&self) {
        self.release.add_permits(1);
    }

    fn remember_delivered(&self, reply: &Result<NativeReply, NativeRegistryError>) {
        if let Some(expiry) = reply
            .as_ref()
            .ok()
            .and_then(|reply| reply.live_session.as_ref())
            .and_then(|session| session.lease_expires_at)
        {
            *self
                .last_confirmed_expiry
                .lock()
                .expect("last actually delivered authority") = Some(expiry);
        }
    }
}

// 取消也会减少 active：合法 timeout 后的后续低频重试不能冒充同时在途的第二个 RPC。
struct CheckpointCall<'a> {
    gate: &'a CheckpointGate,
    held: bool,
    completed: bool,
}

impl Drop for CheckpointCall<'_> {
    fn drop(&mut self) {
        self.gate.in_flight.fetch_sub(1, Ordering::AcqRel);
        if self.held && !self.completed {
            self.gate.canceled.store(true, Ordering::Release);
        }
    }
}

// 只延迟真实请求或真实回答；权限、租约、用量和返回正文全部来自原 mTLS CP。
struct GatedRegistry {
    real: HttpRegistry,
    gate: Arc<CheckpointGate>,
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
    delegate!(append_live_usage, AppendLiveUsageRequest);
    delegate!(claim_live_sessions, ClaimLiveSessionsRequest);
    delegate!(signed_sip_intake, SignedSipIntakeRequest);

    fn create_live_session(
        &self,
        request: CreateLiveSessionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let result = self.real.create_live_session(request).await;
            self.gate.remember_delivered(&result);
            result
        })
    }

    fn checkpoint_live_session(
        &self,
        request: CheckpointLiveSessionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            // 新模型条目保存和 terminal 收尾照常走真实 CP；只观察空闲期的续租。
            let periodic = !request.terminal && request.entries.is_empty();
            if !periodic {
                let result = self.real.checkpoint_live_session(request).await;
                self.gate.remember_delivered(&result);
                return result;
            }
            let calls = self.gate.periodic_calls.fetch_add(1, Ordering::AcqRel) + 1;
            let active = self.gate.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            self.gate.max_in_flight.fetch_max(active, Ordering::AcqRel);
            let phase = self.gate.armed.lock().expect("checkpoint phase").take();
            if phase == Some(HoldPhase::BeforeRpc) {
                // 不可达故障持续到测试释放，合法 timeout 后的重试也不能偷偷恢复 CP 可达性。
                *self.gate.armed.lock().expect("persistent transport gate") = phase;
            }
            let mut call = CheckpointCall {
                gate: &self.gate,
                held: false,
                completed: false,
            };
            let last_confirmed_expiry = self
                .gate
                .last_confirmed_expiry
                .lock()
                .expect("last actually delivered authority")
                .expect("a live session was actually admitted");
            if phase == Some(HoldPhase::BeforeRpc) {
                call.held = true;
                self.gate
                    .hold(HeldCheckpoint {
                        phase: HoldPhase::BeforeRpc,
                        request: request.clone(),
                        reply: None,
                        entered_at: Instant::now(),
                        calls_at_entry: calls,
                        last_confirmed_expiry,
                    })
                    .await;
            }
            let result = self.real.checkpoint_live_session(request.clone()).await;
            if matches!(
                phase,
                Some(HoldPhase::AfterReply | HoldPhase::AfterStopReply)
            ) {
                // 事务已完成后才发布 held 标记；绝不把旧 GW 期限当成 DB 当前期限。
                let reply = result
                    .as_ref()
                    .expect("the held reply is a successful actual authority RPC")
                    .live_session
                    .clone()
                    .expect("the real authority explicitly returns the live record");
                if phase == Some(HoldPhase::AfterStopReply) && !reply.requires_stop {
                    // 管理写入可能与前一轮续租并行；健康回答照常返回，等真正的 stop。
                    *self.gate.armed.lock().expect("stop checkpoint phase") = phase;
                } else {
                    call.held = true;
                    self.gate
                        .hold(HeldCheckpoint {
                            phase: phase.expect("a reply hold phase is selected"),
                            request,
                            reply: Some(reply),
                            entered_at: Instant::now(),
                            calls_at_entry: calls,
                            last_confirmed_expiry,
                        })
                        .await;
                }
            }
            self.gate.remember_delivered(&result);
            call.completed = true;
            result
        })
    }
}

enum ProviderCommand {
    Event(Message, oneshot::Sender<()>),
    HoldTail {
        release: Arc<Semaphore>,
        ready: oneshot::Sender<()>,
        written: oneshot::Sender<()>,
    },
}

struct ProviderState {
    secret: String,
    output: Mutex<Option<mpsc::UnboundedSender<ProviderCommand>>>,
    received: Mutex<Vec<Value>>,
    handshakes: Mutex<Vec<bool>>,
    closed: AtomicBool,
}

struct Provider {
    base: String,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the loopback WS protocol component");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("provider address")
        );
        let state = Arc::new(ProviderState {
            secret: format!("sk-ws-relay-component-{}", Uuid::new_v4().simple()),
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
                .expect("accept the real provider TCP socket");
            let observed = shared.clone();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response| {
                    let authorized = request
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        == Some(format!("Bearer {}", observed.secret).as_str());
                    let project = request
                        .headers()
                        .get("openai-project")
                        .and_then(|v| v.to_str().ok())
                        == Some(PROJECT);
                    let route = request.uri().path() == "/v1/realtime"
                        && request.uri().query() == Some(format!("model={UPSTREAM}").as_str());
                    observed
                        .handshakes
                        .lock()
                        .expect("provider handshake observation")
                        .push(authorized && project && route);
                    Ok(response)
                },
            )
            .await
            .expect("perform the actual upstream WS handshake");
            let (sender, mut output) = mpsc::unbounded_channel::<ProviderCommand>();
            *shared.output.lock().expect("provider output") = Some(sender);
            socket.send(Message::Text(json!({"type":"session.created","event_id":"relay_session_created",
                "session":{"type":"realtime","object":"realtime.session","id":SESSION,"model":UPSTREAM,
                "output_modalities":["audio"],"tools":[],"tool_choice":"auto","max_output_tokens":"inf",
                "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"turn_detection":null},
                         "output":{"format":{"type":"audio/pcm","rate":24000},"voice":"marin"}},
                "expires_at":Utc::now().timestamp()+3600}}).to_string()))
                .await.expect("write the actual provider session identity");
            loop {
                tokio::select! {
                    message = output.recv() => {
                        let Some(message) = message else { break; };
                        match message {
                            ProviderCommand::Event(message, written) => {
                                let close = message.is_close();
                                if socket.send(message).await.is_err() { break; }
                                let _ = written.send(());
                                if close { break; }
                            }
                            ProviderCommand::HoldTail {release, ready, written} => {
                                // 这条分支持有唯一 WS reader，尚未接收 Close；允许合法交叉在途的 data。
                                let _ = ready.send(());
                                release.acquire().await.expect("controlled in-flight tail gate").forget();
                                socket.send(Message::Text(done(TAIL,"accepted_tail_crossing_stop").to_string()))
                                    .await.expect("write accepted tail before this peer receives Close");
                                socket.send(Message::Text(done(TAIL,"accepted_tail_crossing_stop_redelivery").to_string()))
                                    .await.expect("write the repeated accepted response before receiving Close");
                                let _ = written.send(());
                            }
                        }
                    }
                    message = socket.next() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let event:Value = serde_json::from_str(&text).expect("official client WS event JSON");
                                assert!(event.get("type").and_then(Value::as_str).is_some(), "client event has an explicit type");
                                shared.received.lock().expect("actual provider input observation").push(event);
                            }
                            Some(Ok(Message::Ping(bytes))) => {
                                if socket.send(Message::Pong(bytes)).await.is_err() { break; }
                            }
                            Some(Ok(Message::Close(_))) => {
                                // 已收到 Close 后只应答 Close；所有尾量在此之前真正写入连接。
                                let _ = socket.flush().await;
                                break;
                            }
                            None | Some(Err(_)) => break,
                            _ => {}
                        }
                    }
                }
            }
            shared.closed.store(true, Ordering::Release);
        });
        Self { base, state, task }
    }

    async fn send(&self, event: Value) {
        let (written, observed) = oneshot::channel();
        self.state
            .output
            .lock()
            .expect("provider output")
            .as_ref()
            .expect("the real provider handshake completed")
            .send(ProviderCommand::Event(
                Message::Text(event.to_string()),
                written,
            ))
            .expect("queue the official provider event before Close");
        tokio::time::timeout(IO_LIMIT, observed)
            .await
            .expect("the real provider write must complete")
            .expect("the real provider wrote and flushed the frame before Close");
    }

    async fn hold_tail(&self) -> (Arc<Semaphore>, oneshot::Receiver<()>) {
        let release = Arc::new(Semaphore::new(0));
        let (ready, observed) = oneshot::channel();
        let (written, flushed) = oneshot::channel();
        self.state
            .output
            .lock()
            .expect("provider output")
            .as_ref()
            .expect("the actual provider is connected")
            .send(ProviderCommand::HoldTail {
                release: release.clone(),
                ready,
                written,
            })
            .expect("pause only the protocol component reader before the close race");
        tokio::time::timeout(IO_LIMIT, observed)
            .await
            .expect("the provider takes the read gate")
            .expect("the provider reader is paused before receiving any Close");
        (release, flushed)
    }

    async fn received(&self, event_id: &str, limit: Duration) -> Value {
        tokio::time::timeout(limit, async {
            loop {
                if let Some(event) = self
                    .state
                    .received
                    .lock()
                    .expect("provider events")
                    .iter()
                    .find(|event| event.get("event_id") == Some(&json!(event_id)))
                    .cloned()
                {
                    return event;
                }
                assert!(
                    !self.state.closed.load(Ordering::Acquire),
                    "the provider closed before the expected accepted input"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the actual provider receives the identified client event within the relay budget")
    }

    async fn drained(&self) {
        tokio::time::timeout(IO_LIMIT, async {
            while !self.state.closed.load(Ordering::Acquire) || !self.task.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect(
            "the real provider reader reaches the legal WS close before no-new-work assertions",
        );
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
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
        admin,
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
    async fn start(f: &Fixture, gate: Arc<CheckpointGate>) -> Self {
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
        "response":{"output_modalities":["audio"]}}),
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

async fn positive(f: &Fixture, gateway: &Gateway) -> Socket {
    let mut socket = gateway.connect(f).await;
    // 先完成另一个响应，确保延迟/停止测试不可能靠缺路由、握手失败或零账单呈红。
    accepted_response(f, &mut socket, FIRST).await;
    pcm_roundtrip(f, &mut socket, FIRST, "healthy", IO_LIMIT).await;
    let completed = done(FIRST, "positive_nonzero_done");
    f.provider.send(completed.clone()).await;
    let observed = next_event(&mut socket, IO_LIMIT).await;
    assert!(
        observed.get("type") == Some(&json!("response.done"))
            && observed.pointer("/response/usage") == completed.pointer("/response/usage")
    );
    paid(f, 1).await;
    accepted_response(f, &mut socket, TAIL).await;
    socket
}

async fn stop_and_drain(f: &Fixture, socket: &mut Socket) {
    tokio::time::timeout(IO_LIMIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let event: Value =
                        serde_json::from_str(&text).expect("actual WS stop/tail event");
                    if event.get("type") == Some(&json!("error")) {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Ping(bytes))) => {
                    let _ = socket.send(Message::Pong(bytes)).await;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("confirmed authority stop or natural lease expiry actively stops the actual WS");
    // 客户端观察到 stop 后才试新工作；成功写入 TCP 也不能等价于上游接受。
    let _ = socket
        .send(Message::Text(
            json!({"type":"input_audio_buffer.append","event_id":"forbidden_after_confirmed_stop",
        "audio":base64::engine::general_purpose::STANDARD.encode(pcm(13))})
            .to_string(),
        ))
        .await;
    let _ = socket.close(None).await;
    f.provider.drained().await;
    assert!(
        !f.provider
            .state
            .received
            .lock()
            .expect("drained actual provider input")
            .iter()
            .any(|event| event.get("event_id") == Some(&json!("forbidden_after_confirmed_stop"))),
        "new client input after confirmed stop never reaches the real provider reader"
    );
}

async fn release_tail(tail: (Arc<Semaphore>, oneshot::Receiver<()>)) {
    let (release, written) = tail;
    release.add_permits(1);
    tokio::time::timeout(IO_LIMIT, written)
        .await
        .expect("the legal in-flight usage tail is written within the real close/drain budget")
        .expect("the provider wrote both accepted response deliveries before it consumed Close");
}

// 可击穿点：在媒体 select 内 await 周期续租；未返回时再启动续租；丢失或重复结算在途尾量。
#[tokio::test]
async fn a_held_real_renewal_reply_keeps_pcm_bidirectional_with_one_renewal_in_flight() {
    let Some(f) = fixture().await else {
        return;
    };
    let gate = CheckpointGate::new();
    let mut gateway = Gateway::start(&f, gate.clone()).await;
    let mut socket = positive(&f, &gateway).await;
    gate.arm(HoldPhase::AfterReply);
    let held = gate.held().await;
    let reply = held
        .reply
        .as_ref()
        .expect("a real successful checkpoint reply is held");
    assert!(held.phase == HoldPhase::AfterReply && !reply.requires_stop && reply.state == "live");
    let actual = lease_sample(&f).await;
    assert!(
        actual.operation_id == held.request.lease.operation_id
            && actual.lease_generation == held.request.lease.generation
            && reply.lease_expires_at == Some(actual.lease_expires_at)
            && actual.lease_expires_at > actual.db_now,
        "CP really renewed the original row before its successful answer was withheld"
    );
    assert!(held.last_confirmed_expiry>actual.db_now+chrono::Duration::seconds(RELAY_LIMIT.as_secs() as i64),
        "the independent PCM relay check runs before the gateway's last actually confirmed lease expiry");
    // 独立的3秒回环媒体预算远短于12秒故障注入；从未要求每块 PCM 去远程认证。
    pcm_roundtrip(
        &f,
        &mut socket,
        TAIL,
        "while_actual_renewal_reply_held",
        RELAY_LIMIT,
    )
    .await;
    assert!(!gate.canceled.load(Ordering::Acquire) && gate.in_flight.load(Ordering::Acquire)==1,
        "both PCM directions complete while the original delayed authority future is genuinely still active");
    assert_eq!(
        gate.periodic_calls.load(Ordering::Acquire),
        held.calls_at_entry,
        "the audio packets cannot cause an additional authority RPC beside the scheduled renewal"
    );
    let remaining = RENEWAL_OBSERVATION.saturating_sub(held.entered_at.elapsed());
    tokio::time::sleep(remaining).await;
    assert!(
        !gate.returned.load(Ordering::Acquire),
        "the real reply is still held during the observation"
    );
    if !gate.canceled.load(Ordering::Acquire) {
        assert_eq!(gate.periodic_calls.load(Ordering::Acquire),held.calls_at_entry,
            "crossing the next nominal10-second opportunity cannot start a second RPC while the first remains active");
        assert_eq!(gate.in_flight.load(Ordering::Acquire), 1);
    }
    assert_eq!(gate.max_in_flight.load(Ordering::Acquire), 1);
    gate.open();
    f.provider
        .send(done(TAIL, "tail_after_healthy_delayed_renewal"))
        .await;
    let observed = next_event(&mut socket, IO_LIMIT).await;
    assert!(
        observed.get("type") == Some(&json!("response.done"))
            && observed.pointer("/response/id") == Some(&json!(TAIL))
    );
    paid(&f, 2).await;
    let _ = socket.close(None).await;
    f.provider.drained().await;
    gateway.no_aggregate().await;
    paid(&f, 2).await;
}

// 可击穿点：收到真实 requires_stop 后仍转发新工作，或在停止分支丢弃已写入连接的合法尾量。
#[tokio::test]
async fn a_real_stop_verdict_blocks_new_work_and_drains_accepted_usage_tail_once() {
    let Some(f) = fixture().await else {
        return;
    };
    let gate = CheckpointGate::new();
    let mut gateway = Gateway::start(&f, gate.clone()).await;
    let mut socket = positive(&f, &gateway).await;
    let path = format!("/admin/keys/{}", f.key);
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1")
        .bind(f.key)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("read the persisted key version for the public conditional mutation");
    gate.arm(HoldPhase::AfterStopReply);
    let disabled =
        f.cp.send_conditional(
            "PATCH",
            &path,
            f.admin.cookie(),
            version,
            json!({"disabled":true}),
        )
        .await;
    assert_eq!(
        disabled.status, 200,
        "the real public admin API persists the key revocation"
    );
    let held = gate.held().await;
    let verdict = held.reply.as_ref().expect("the held real stop verdict");
    assert!(verdict.requires_stop && verdict.state=="requires_stop",
        "the actual next periodic CP transaction explicitly withdraws live authority; no snapshot refresh substitutes for this signal");
    let actual = lease_sample(&f).await;
    assert!(
        actual.operation_id == held.request.lease.operation_id && actual.state == "requires_stop"
    );
    // 先暂停唯一供应商 reader，尚未接收 Close；真实 stop 回答交付后立即合法写交叉尾量。
    let tail = f.provider.hold_tail().await;
    gate.open();
    tokio::time::timeout(IO_LIMIT, async {
        while !gate.returned.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the actual stop reply leaves the explicit decorator gate");
    release_tail(tail).await;
    stop_and_drain(&f, &mut socket).await;
    paid(&f, 2).await;
    gateway.no_aggregate().await;
    paid(&f, 2).await;
}

// 可击穿点：一个远程续租被阻塞时忘记本地已确认期限，使真实过期租约继续接受新工作。
#[tokio::test]
async fn a_naturally_expired_real_lease_stops_without_waiting_for_the_blocked_renewal() {
    let Some(f) = fixture().await else {
        return;
    };
    let gate = CheckpointGate::new();
    let mut gateway = Gateway::start(&f, gate.clone()).await;
    let mut socket = positive(&f, &gateway).await;
    gate.arm(HoldPhase::BeforeRpc);
    let held = gate.held().await;
    assert!(held.phase==HoldPhase::BeforeRpc && held.reply.is_none(),
        "this negative control blocks the real request before transport, rather than pretending a completed renewal did not happen");
    let before = lease_sample(&f).await;
    assert!(before.operation_id==held.request.lease.operation_id
        && before.lease_generation==held.request.lease.generation
        && before.lease_expires_at>before.db_now && before.deadline_at>before.lease_expires_at,
        "the original real lease is initially live and its lifecycle deadline is not the expiry being tested");
    // 快到真实期限才暂停唯一供应商 reader，避免把长时间不应答 Ping 变成另一种失败。
    let mut tail = None;
    tokio::time::timeout(Duration::from_secs(90),async {
        loop {
            let actual = lease_sample(&f).await;
            assert_eq!(actual.operation_id,before.operation_id);
            assert_eq!(actual.lease_generation,before.lease_generation);
            assert_eq!(actual.lease_expires_at,before.lease_expires_at,
                "no test-generated heartbeat or hidden overlapping RPC may renew the blocked real lease");
            if tail.is_none() && actual.lease_expires_at-actual.db_now<=chrono::Duration::seconds(1) {
                tail=Some(f.provider.hold_tail().await);
            }
            if actual.db_now>=actual.lease_expires_at { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the unchanged database lease naturally reaches its own actual expiry");
    assert!(!gate.returned.load(Ordering::Acquire));
    assert_eq!(gate.max_in_flight.load(Ordering::Acquire),1,
        "unavailability may cause later retries after cancellation, never overlapping live renewal attempts");
    release_tail(tail.expect("the legal provider reader gate was ready before real expiry")).await;
    // 仍不释放阻塞请求：真实到期本身必须让连接停止，不能靠测试放行失败 RPC 代劳。
    stop_and_drain(&f, &mut socket).await;
    gate.open();
    paid(&f, 2).await;
    gateway.no_aggregate().await;
    paid(&f, 2).await;
}
