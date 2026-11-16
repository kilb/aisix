//! 合法长握手在真实 CP 租约自然到期点前续租，保持同一计费操作与 generation。
//! 只使用真实管理 API、PG/kine/mTLS、生产网关和本地 WS 协议对端；不运行媒体或付费供应商。

#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::ProxyConfig;
use aisix_gateway::{Hub, UpstreamHttpConfig};
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use native_gateway::{projected_snapshot, HttpRegistry};
use serde_json::{json, Value};
use support::{Bundle, OperatorSession, TestCp};
use tokio::sync::{mpsc, Semaphore};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, protocol::Role, Message};
use tower::ServiceExt as _;
use uuid::Uuid;

const MODEL: &str = "realtime-longdial-public-main";
const UPSTREAM: &str = "gpt-realtime";
const PROJECT: &str = "proj_realtime_longdial_component";
const SESSION: &str = "sess_realtime_longdial_component";
const RESPONSE: &str = "resp_realtime_longdial_component";
const CONNECT_BUDGET: Duration = Duration::from_secs(120);
const MINIMUM_HOLD: Duration = Duration::from_secs(46);
const IO_LIMIT: Duration = Duration::from_secs(15);
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct ProviderState {
    secret: String,
    attempts: Mutex<Vec<bool>>,
    held_at: Mutex<Option<Instant>>,
    gate: Semaphore,
    opened: AtomicBool,
    finished: AtomicBool,
    output: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    client_events: Mutex<Vec<String>>,
}

struct Provider {
    base: String,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start() -> Self {
        let state = Arc::new(ProviderState {
            secret: format!("sk-longdial-component-{}", Uuid::new_v4().simple()),
            attempts: Mutex::new(Vec::new()),
            held_at: Mutex::new(None),
            gate: Semaphore::new(0),
            opened: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            output: Mutex::new(None),
            client_events: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the controlled ordinary WS provider");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("provider address")
        );
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.expect("accept an actual provider connection");
                        let state = shared.clone();
                        connections.spawn(async move {
                            let service = hyper::service::service_fn(move |request|provider_http(request,state.clone()));
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream),service).with_upgrades().await;
                        });
                    }
                    done = connections.join_next(), if !connections.is_empty() => {
                        if let Some(done) = done { done.expect("the controlled provider HTTP task must not panic"); }
                    }
                }
            }
        });
        Self { base, state, task }
    }

    async fn held(&self) -> Instant {
        tokio::time::timeout(IO_LIMIT, async {
            loop {
                if let Some(held) = *self.state.held_at.lock().expect("actual handshake observation") {
                    assert!(self.state.attempts.lock().expect("all provider attempts").as_slice()==[true],
                        "one actual request has the original configured credential/project/model and official WS headers");
                    return held;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("the original provider receives and holds the actual authenticated upgrade")
    }

    fn send(&self, message: Message) {
        self.state
            .output
            .lock()
            .expect("provider output")
            .as_ref()
            .expect("actual101 completed")
            .send(message)
            .expect("queue a controlled official provider frame");
    }

    async fn received_create(&self) {
        tokio::time::timeout(IO_LIMIT, async {
            loop {
                let count = self
                    .state
                    .client_events
                    .lock()
                    .expect("all client event types")
                    .len();
                if count > 0 {
                    assert!(
                        self.state
                            .client_events
                            .lock()
                            .expect("all client event types")
                            .as_slice()
                            == ["response.create"],
                        "the actual authorized response.create reaches the original provider once"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the renewed real session forwards the authorized client response request");
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn provider_http(
    mut request: Request<hyper::body::Incoming>,
    state: Arc<ProviderState>,
) -> Result<hyper::Response<Full<Bytes>>, std::convert::Infallible> {
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {}", state.secret).as_str());
    let project = request
        .headers()
        .get("openai-project")
        .and_then(|value| value.to_str().ok())
        == Some(PROJECT);
    let route = request.method() == "GET"
        && request.uri().path() == "/v1/realtime"
        && request.uri().query() == Some(format!("model={UPSTREAM}").as_str());
    let upgrade = request
        .headers()
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let key = request
        .headers()
        .get("sec-websocket-key")
        .map(|value| value.as_bytes().to_vec());
    let valid = authorized && project && route && upgrade && key.is_some();
    state
        .attempts
        .lock()
        .expect("all provider HTTP/WS attempts")
        .push(valid);
    if !valid {
        return Ok(hyper::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Full::new(Bytes::new()))
            .expect("bounded invalid provider request response"));
    }
    {
        let mut held = state.held_at.lock().expect("actual handshake observation");
        assert!(
            held.is_none(),
            "one long dial must not be replaced by another provider handshake"
        );
        *held = Some(Instant::now());
    }
    let permit = state
        .gate
        .acquire()
        .await
        .expect("controlled actual101 gate");
    permit.forget();
    let upgrade = hyper::upgrade::on(&mut request);
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
        &key.expect("validated official WS key"),
    );
    tokio::spawn(async move {
        let upgraded = upgrade
            .await
            .expect("the actual provider HTTP connection upgrades");
        let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            TokioIo::new(upgraded),
            Role::Server,
            None,
        )
        .await;
        let (sender, mut output) = mpsc::unbounded_channel();
        *state.output.lock().expect("provider output") = Some(sender);
        socket.send(Message::Text(json!({"type":"session.created","event_id":"longdial_session_created",
            "session":{"type":"realtime","object":"realtime.session","id":SESSION,"model":UPSTREAM,"output_modalities":["text"],"tools":[],"tool_choice":"auto","max_output_tokens":"inf","expires_at":Utc::now().timestamp()+3600}}).to_string()))
            .await.expect("send the actual provider session identity after real101");
        state.opened.store(true, Ordering::Release);
        loop {
            tokio::select! {
                message=output.recv()=>{
                    let Some(message)=message else { break; };
                    if socket.send(message).await.is_err() { break; }
                }
                message=socket.next()=>{
                    match message {
                        Some(Ok(Message::Text(text)))=>{
                            let event:Value=serde_json::from_str(&text).expect("official client event JSON");
                            let kind=event.get("type").and_then(Value::as_str).expect("explicit client event type");
                            state.client_events.lock().expect("all client event types").push(kind.to_owned());
                        }
                        Some(Ok(Message::Ping(bytes)))=>{ if socket.send(Message::Pong(bytes)).await.is_err() { break; } }
                        Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,
                        _=>{}
                    }
                }
            }
        }
        state.finished.store(true, Ordering::Release);
    });
    Ok(hyper::Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", accept)
        .body(Full::new(Bytes::new()))
        .expect("actual held101 response"))
}

struct Fixture {
    cp: TestCp,
    provider: Provider,
    bundle: Bundle,
    tenant: Uuid,
    key: Uuid,
    bearer: String,
    model: Uuid,
}

fn id(body: &Value, pointer: &str) -> Uuid {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
        .expect("explicit public resource UUID without displaying the reply")
}

async fn admin_post(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let reply = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(reply.status, 201, "the actual admin fixture write succeeds");
    reply.body
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start_with_kine().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = Provider::start().await;
    let account=admin_post(&cp,&admin,"/admin/resources/provider_keys",json!({"doc":{
        "display_name":"realtime-longdial-original-account","provider":"openai","adapter":"openai","api_base":provider.base,
        "api_key":provider.state.secret,"groups":["shared"],"request":{"default_headers":{"OpenAI-Project":PROJECT}}
    }})).await;
    let model=admin_post(&cp,&admin,"/admin/resources/models",json!({"doc":{
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
            current.body["version"].as_i64().expect("tenant version"),
            json!({"allowed_model_ids":[model]}),
        )
        .await;
    assert_eq!(
        granted.status, 200,
        "the actual owner can use the selected direct realtime model"
    );
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/credits"),
        json!({"amount_cny":"1","reason":"普通 WS 合法长握手组件余额"}),
    )
    .await;
    admin_post(&cp,&admin,&format!("/admin/provider-groups/{group}/rates"),json!({"model_id":null,"cny_per_usd":"1","effective_from":support::world::db_now(&cp).await})).await;
    admin_post(&cp,&admin,"/admin/pricing/baselines",json!({"model_id":model,"effective_from":support::world::db_now(&cp).await,
        "note":"长握手后真实非零主模型价格","card":{"default":{"base":{"input":"10","cache_read":"2","output":"20"}}}})).await;
    let key = admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":"realtime-longdial-owner","member_id":member,"group_id":group}),
    )
    .await;
    let bearer = key["plaintext"]
        .as_str()
        .expect("actual issued caller bearer")
        .to_owned();
    let key = id(&key, "/key/id");
    let bundle = cp
        .issue_bundle(&admin, "realtime-longdial-real-gateway")
        .await;
    Some(Fixture {
        cp,
        provider,
        bundle,
        tenant,
        key,
        bearer,
        model,
    })
}

struct Gateway {
    base: String,
    billing: mpsc::Receiver<UsageEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl Gateway {
    async fn start(f: &Fixture) -> Self {
        let hub = Arc::new(Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        let config: ProxyConfig = serde_json::from_value(
            json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}),
        )
        .expect("actual gateway configuration");
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
        .with_native_objects(Arc::new(HttpRegistry::new(&f.cp, &f.bundle)))
        .with_usage_sink(UsageSink::new(sender));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind actual gateway HTTP/WS router");
        let base = format!("http://{}", listener.local_addr().expect("gateway address"));
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (socket,_)=accepted.expect("accept actual gateway connection");
                        let app=build_router(state.clone());
                        connections.spawn(async move {
                            let service=hyper::service::service_fn(move |request:Request<hyper::body::Incoming>|app.clone().oneshot(request));
                            let _=hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(socket),service).with_upgrades().await;
                        });
                    }
                    done=connections.join_next(), if !connections.is_empty()=>{
                        if let Some(done)=done { done.expect("the gateway connection task must not panic"); }
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
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// 0052 公开持久 schema；只有 SELECT，不通过测试直接续租、claim 或修改过期时间。
#[derive(sqlx::FromRow)]
struct LeaseSample {
    operation_id: Uuid,
    api_key_id: Uuid,
    lease_dp_id: Uuid,
    lease_generation: i64,
    lease_expires_at: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
    state: String,
    coverage: String,
    accounting_state: String,
    db_now: DateTime<Utc>,
}

async fn lease_sample(f: &Fixture) -> LeaseSample {
    let mut rows:Vec<LeaseSample>=sqlx::query_as("SELECT operation_id,api_key_id,lease_dp_id,lease_generation,lease_expires_at,deadline_at,state,coverage,accounting_state,clock_timestamp() AS db_now FROM native_live_sessions WHERE environment_id=$1 AND api_key_id=$2")
        .bind(f.cp.env().env_id).bind(f.key).fetch_all(f.cp.db().pool()).await.expect("read actual live lease and database wall clock");
    assert_eq!(
        rows.len(),
        1,
        "the public WS journey creates exactly one real live authority row"
    );
    rows.remove(0)
}

fn same_identity(f: &Fixture, initial: &LeaseSample, actual: &LeaseSample) {
    let dp: Uuid = f
        .bundle
        .dp_id
        .parse()
        .expect("actual issued DP credential identity");
    assert!(
        !initial.operation_id.is_nil() && initial.lease_generation > 0,
        "initial actual operation/generation must be explicit"
    );
    assert!(actual.operation_id==initial.operation_id && actual.api_key_id==f.key
        && actual.lease_dp_id==dp && actual.lease_dp_id==initial.lease_dp_id
        && actual.lease_generation==initial.lease_generation,
        "long dial must retain the same operation, owner, actual DP holder and generation rather than reacquire or fabricate another authority");
}

async fn hold_past_real_initial_expiry(
    f: &Fixture,
    held_at: Instant,
    client: &tokio::task::JoinHandle<(Socket, Value)>,
) -> LeaseSample {
    let initial = lease_sample(f).await;
    same_identity(f, &initial, &initial);
    assert!(
        initial.state == "live" && initial.lease_expires_at > initial.db_now,
        "the initial pre-dial lease is really live and unexpired"
    );
    let initial_remaining = (initial.lease_expires_at - initial.db_now)
        .to_std()
        .expect("positive actual initial lease lifetime");
    assert!(held_at.elapsed()+initial_remaining+Duration::from_secs(20)<CONNECT_BUDGET,
        "the actual initial expiry must leave a real release margin inside the configured connect budget; no clock/lease rewrite repairs an invalid setup");
    let required_remaining =
        MINIMUM_HOLD.max(initial_remaining + Duration::from_secs(2)) + IO_LIMIT;
    assert!(initial.deadline_at>initial.db_now+chrono::Duration::from_std(required_remaining).expect("bounded scenario duration"),
        "the actual immutable session deadline must allow this legitimate held handshake plus real response IO");
    let mut latest_expiry = initial.lease_expires_at;
    let mut extended = false;
    tokio::time::timeout(CONNECT_BUDGET-Duration::from_secs(20),async {
        loop {
            assert!(!client.is_finished(),"the legal pending dial must not terminate or invent a server event before the provider101 is released");
            let sample=lease_sample(f).await;
            same_identity(f,&initial,&sample);
            assert!(sample.deadline_at==initial.deadline_at,"renewal alone before provider101 cannot change the original session deadline");
            assert!(sample.state=="live","the gateway must retain actual live authority throughout a permitted provider handshake");
            assert!(sample.lease_expires_at>sample.db_now,"the real CP lease expired while the configured handshake budget still permits the pending dial");
            assert!(sample.lease_expires_at>=latest_expiry,"a lease renewal cannot shorten the authority interval");
            assert!(sample.coverage!="complete" && sample.accounting_state!="settled","a pending handshake supplies no complete supplier usage");
            extended|=sample.lease_expires_at>initial.lease_expires_at;
            latest_expiry=sample.lease_expires_at;
            if held_at.elapsed()>=MINIMUM_HOLD && sample.db_now>initial.lease_expires_at+chrono::Duration::seconds(2) {
                assert!(extended,"the real expiry must have advanced before the original lease deadline, with no test-generated heartbeat");
                assert!(!f.provider.state.opened.load(Ordering::Acquire),"the real101 remains withheld while pre-dial lease renewal is tested");
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.expect("real time crosses the actual initial expiry and45-second held handshake within the legal connect budget");
    assert!(
        held_at.elapsed() < CONNECT_BUDGET - Duration::from_secs(10),
        "release the actual101 within the configured budget with a real IO margin"
    );
    initial
}

async fn next_event(socket: &mut Socket, limit: Duration) -> Value {
    tokio::time::timeout(limit, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text)
                        .expect("official server event JSON without displaying private bodies")
                }
                Some(Ok(Message::Ping(bytes))) => socket
                    .send(Message::Pong(bytes))
                    .await
                    .expect("answer actual protocol ping"),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    panic!("the valid long-dial session closed before its expected official event")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the renewed live session delivers its actual expected server event")
}

async fn paid_once(f: &Fixture, operation: Uuid) {
    tokio::time::timeout(IO_LIMIT,async {
        loop {
            let rows:(i64,i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM native_live_journal),(SELECT count(*) FROM native_live_outbox),(SELECT count(*) FROM pricing_charges_cny WHERE tenant_id=$1::text AND status='charged'),count(*),COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'")
                .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("read every actual canonical fact, priced row and tenant debit");
            assert!(rows.0<=1 && rows.1<=1 && rows.2<=1 && rows.3<=1 && rows.4<=144,"expired authority, a replacement operation or parallel aggregate must not produce missing or duplicate supplier bills");
            if rows==(1,1,1,1,144) { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the same renewed operation actually pays144 microCNY through real journal/outbox/intake/pricing/ledger");
    let events: Vec<Value> = sqlx::query_scalar("SELECT event FROM native_live_outbox")
        .fetch_all(f.cp.db().pool())
        .await
        .expect("read actual canonical paid identity without printing event bodies");
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert!(
        event.get("native_operation_id") == Some(&json!(operation))
            && event.get("api_key_id") == Some(&json!(f.key))
            && event.get("model_id") == Some(&json!(f.model))
            && event.get("requested_model_id") == Some(&json!(f.model)),
        "long dial cannot substitute the original operation, payer or configured direct model"
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
            "the explicitly reported response quantity remains present"
        );
    }
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(f.tenant)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("read the actually funded tenant balance");
    assert_eq!(
        balance, 999_856,
        "8 uncached*10+2 cached*2+3 output*20=144 microCNY once"
    );
}

// 可击穿点：只在101后续租、丢失租约后重建 operation/generation、忽略合法 connect budget、丢账或重账。
#[tokio::test]
async fn ordinary_ws_long_dial_renews_the_real_lease_before_natural_expiry_and_pays_same_operation()
{
    // 公开 startup API 为进程一次配置；本 target 只有此 case，必须先于 CP/Hub/bridge 初始化。
    aisix_gateway::upstream_http::init(UpstreamHttpConfig {
        connect_timeout: Some(CONNECT_BUDGET),
        ..Default::default()
    })
    .expect(
        "install the existing public startup connect budget before constructing any runtime client",
    );
    assert_eq!(
        aisix_gateway::upstream_http::config().connect_timeout,
        Some(CONNECT_BUDGET),
        "the actual production connector must use the explicit legal120-second handshake budget"
    );
    let Some(f) = fixture().await else {
        return;
    };
    let mut gateway = Gateway::start(&f).await;
    let base = gateway.base.clone();
    let bearer = f.bearer.clone();
    let mut client = tokio::spawn(async move {
        let mut request = format!(
            "{}/v1/realtime?model={MODEL}",
            base.replacen("http://", "ws://", 1)
        )
        .into_client_request()
        .expect("official ordinary realtime WS URL");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {bearer}")
                .parse()
                .expect("actual bearer header"),
        );
        let (mut socket, response) = tokio_tungstenite::connect_async(request)
            .await
            .unwrap_or_else(|_| {
                panic!("the legal long-dial client must establish a real gateway WebSocket")
            });
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        // 等待供应商101期间继续读真实客户端 WS 并响应 Ping，避免把客户端静默当作产品缺陷。
        let session = next_event(&mut socket, CONNECT_BUDGET + IO_LIMIT).await;
        (socket, session)
    });
    let held_at = tokio::select! {
        held=f.provider.held()=>held,
        result=&mut client=>{
            let _=result.expect("actual client WS task");
            panic!("the client received a server event before the controlled provider101");
        }
    };
    let initial = hold_past_real_initial_expiry(&f, held_at, &client).await;
    f.provider.state.gate.add_permits(1);
    let (mut socket, session) = tokio::time::timeout(IO_LIMIT, client)
        .await
        .expect("actual client session opens after the legal provider release")
        .expect("actual client WS task");
    assert!(
        session.get("type") == Some(&json!("session.created"))
            && session.pointer("/session/id") == Some(&json!(SESSION)),
        "the original provider session really opens after the naturally extended lease"
    );
    let connected = lease_sample(&f).await;
    same_identity(&f, &initial, &connected);
    assert!(
        connected.state == "live"
            && connected.lease_expires_at > connected.db_now
            && connected.deadline_at > connected.db_now,
        "the original lease remains actually live after the held provider101 opens"
    );
    socket.send(Message::Text(json!({"type":"response.create","event_id":"longdial_client_create","response":{"output_modalities":["text"]}}).to_string()))
        .await.expect("request real work through the still-authorized ordinary gateway WS");
    f.provider.received_create().await;
    f.provider.send(Message::Text(json!({"type":"response.created","event_id":"longdial_created","response":{"object":"realtime.response","id":RESPONSE,"status":"in_progress","output":[]}}).to_string()));
    let created = next_event(&mut socket, IO_LIMIT).await;
    assert!(
        created.get("type") == Some(&json!("response.created"))
            && created.pointer("/response/id") == Some(&json!(RESPONSE))
    );
    let done = json!({"type":"response.done","event_id":"longdial_done","response":{"object":"realtime.response","id":RESPONSE,"status":"completed","status_details":null,"output":[],
        "usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,"input_token_details":{"cached_tokens":2,"text_tokens":10},"output_token_details":{"text_tokens":3}}}});
    f.provider.send(Message::Text(done.to_string()));
    let observed = next_event(&mut socket, IO_LIMIT).await;
    assert!(
        observed.get("type") == Some(&json!("response.done"))
            && observed.pointer("/response/usage") == done.pointer("/response/usage"),
        "the original explicit paid quantities reach the actual client"
    );
    paid_once(&f, initial.operation_id).await;
    let mut duplicate = done;
    duplicate["event_id"] = json!("longdial_done_redelivery");
    f.provider.send(Message::Text(duplicate.to_string()));
    let repeated = next_event(&mut socket, IO_LIMIT).await;
    assert!(
        repeated.get("type") == Some(&json!("response.done"))
            && repeated.pointer("/response/id") == Some(&json!(RESPONSE))
    );
    f.provider.send(Message::Close(None));
    tokio::time::timeout(IO_LIMIT, async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(Message::Ping(bytes)) => {
                    let _ = socket.send(Message::Pong(bytes)).await;
                }
                _ => {}
            }
        }
        while !f.provider.state.finished.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the actual provider/client readers drain without a fabricated terminal usage event");
    drop(socket);
    tokio::time::timeout(IO_LIMIT, async {
        loop {
            let sample = lease_sample(&f).await;
            same_identity(&f, &initial, &sample);
            assert_ne!(
                sample.accounting_state, "settled",
                "plain close cannot settle incomplete supplier coverage"
            );
            // Socket 关闭先于持久 checkpoint；live 仍可保留上一条已收费事实的 complete。
            if sample.state == "closed" {
                assert_eq!(
                    sample.coverage, "unknown",
                    "plain close must leave supplier coverage unknown"
                );
                assert_eq!(
                    sample.accounting_state, "awaiting_reconciliation",
                    "plain close must retain unconfirmed coverage for reconciliation"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the original long-dial operation closes durably Unknown and awaiting reconciliation");
    assert!(
        matches!(
            gateway.billing.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ),
        "canonical live facts must not enqueue a second ordinary CP billing aggregate"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), gateway.billing.recv())
            .await
            .is_err(),
        "no delayed ordinary aggregate may rebill the canonical response"
    );
    paid_once(&f, initial.operation_id).await;
    assert!(
        f.provider
            .state
            .attempts
            .lock()
            .expect("all provider attempts")
            .as_slice()
            == [true],
        "the original configured long dial remains one actual provider handshake"
    );
    assert!(
        f.provider
            .state
            .client_events
            .lock()
            .expect("all client event types")
            .as_slice()
            == ["response.create"],
        "no second response start or hidden provider command repairs lost authority"
    );
}
