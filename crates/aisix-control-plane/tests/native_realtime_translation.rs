//! 真实 CP/PG/kine/mTLS 与生产网关的 Translation TCP WS 联合组件合同；不运行媒体或付费供应商。

#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::ProxyConfig;
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{Request, StatusCode},
    response::IntoResponse,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::BodyExt as _;
use hyper_util::rt::TokioIo;
use native_gateway::{projected_snapshot, HttpRegistry};
use serde_json::{json, Value};
use support::{Bundle, OperatorSession, TestCp};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, Message};
use tower::ServiceExt as _;
use uuid::Uuid;

const TRANSLATION: &str = "translation-public-main";
const REALTIME: &str = "ordinary-public-main";
const ASR_ALIAS: &str = "translation-source-asr";
const TRANSLATION_UPSTREAM: &str = "gpt-realtime-translate";
const REALTIME_UPSTREAM: &str = "gpt-realtime";
const ASR_UPSTREAM: &str = "gpt-realtime-whisper";
const PROJECT: &str = "proj_translation_component";
const SUPPLIER_SECRET: &str = "ek_supplier_component_stays_internal";
const INPUT: &[u8] = &[1, 0, 2, 0, 3, 0, 4, 0];
const EARLY_INPUT: &[u8] = &[5, 0, 6, 0];
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum Command {
    Event(Value),
    Ack,
}

struct Session {
    id: String,
    translation: bool,
    handshake: bool,
    commands: mpsc::UnboundedSender<Command>,
    updates: Mutex<Vec<Value>>,
    input: Mutex<Vec<(Vec<u8>, bool)>>,
    types: Mutex<Vec<String>>,
    acknowledged: AtomicBool,
    finished: AtomicBool,
}

impl Session {
    fn emit(&self, event: Value) {
        self.commands
            .send(Command::Event(event))
            .expect("send controlled official server event");
    }
    fn ack(&self) {
        self.commands
            .send(Command::Ack)
            .expect("release the actual provider configuration acknowledgment");
    }
    async fn update(&self, number: usize) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(update) = self
                    .updates
                    .lock()
                    .expect("provider update log")
                    .get(number)
                    .cloned()
                {
                    break update;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("gateway defaults or an authorized update reaches the actual provider")
    }
    async fn input(&self, bytes: &[u8]) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let seen = self
                    .input
                    .lock()
                    .expect("provider PCM log")
                    .iter()
                    .any(|(actual, acked)| actual == bytes && *acked);
                if seen {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("exact PCM input reaches the provider after actual defaults acknowledgment");
    }
    async fn closed(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.finished.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the actual provider reader drains to transport close");
    }
}

struct ProviderState {
    key: String,
    attempts: AtomicUsize,
    mint_attempts: AtomicUsize,
    sessions: Mutex<Vec<Arc<Session>>>,
}

struct Provider {
    base: String,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

fn provider_session(id: &str, translation: bool, model: &str, audio: Value) -> Value {
    if translation {
        json!({"id":id,"type":"translation","model":model,"audio":audio,"expires_at":Utc::now().timestamp()+3600})
    } else {
        json!({"id":id,"object":"realtime.session","type":"realtime","model":model,"audio":audio,"expires_at":Utc::now().timestamp()+3600,"output_modalities":["text"],"tools":[],"tool_choice":"auto","max_output_tokens":"inf"})
    }
}

impl Provider {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind controlled multi-session provider");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("provider address")
        );
        let state = Arc::new(ProviderState {
            key: format!("sk-translation-component-{}", Uuid::new_v4().simple()),
            attempts: AtomicUsize::new(0),
            mint_attempts: AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let mut peers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream,_) = accepted.expect("accept provider connection");
                        let state = shared.clone();
                        peers.spawn(async move {
                            let mut method = [0_u8;1];
                            if stream.peek(&mut method).await.expect("peek provider HTTP method") == 0 { return; }
                            if method[0] == b'G' { provider_ws(stream,state).await; }
                            else {
                                let app = Router::new().fallback(provider_http).with_state(state);
                                let service = hyper::service::service_fn(move |request: Request<hyper::body::Incoming>|app.clone().oneshot(request));
                                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream),service).await;
                            }
                        });
                    }
                    _ = peers.join_next(), if !peers.is_empty() => {}
                }
            }
        });
        Self { base, state, task }
    }
    async fn session(&self, number: usize) -> Arc<Session> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(session) = self
                    .state
                    .sessions
                    .lock()
                    .expect("provider sessions")
                    .get(number)
                    .cloned()
                {
                    break session;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("an actual independent provider WS session exists")
    }
    fn attempts(&self) -> usize {
        self.state.attempts.load(Ordering::Acquire)
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn provider_http(
    State(state): State<Arc<ProviderState>>,
    request: Request<Body>,
) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    state.mint_attempts.fetch_add(1, Ordering::AcqRel);
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {}", state.key).as_str());
    let project = request
        .headers()
        .get("openai-project")
        .and_then(|value| value.to_str().ok())
        == Some(PROJECT);
    if !authorized || !project {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if request.method() != "POST"
        || !matches!(
            path.as_str(),
            "/v1/realtime/client_secrets" | "/v1/realtime/translations/client_secrets"
        )
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = to_bytes(request.into_body(), 1 << 20)
        .await
        .expect("read bounded secret creation request");
    let body: Value = serde_json::from_slice(&body).expect("official secret creation JSON");
    let translation = path.contains("/translations/");
    let model = body
        .pointer("/session/model")
        .and_then(Value::as_str)
        .expect("the upstream request names its real model");
    let seconds = body
        .pointer("/expires_after/seconds")
        .and_then(Value::as_i64)
        .unwrap_or(600);
    Json(json!({"value":SUPPLIER_SECRET,"expires_at":Utc::now().timestamp()+seconds,
        "session":provider_session("sess_supplier_mint_metadata",translation,model,body.pointer("/session/audio").cloned().unwrap_or_else(||json!({})))})).into_response()
}

async fn provider_ws(stream: tokio::net::TcpStream, state: Arc<ProviderState>) {
    state.attempts.fetch_add(1, Ordering::AcqRel);
    let handshake = Arc::new(Mutex::new(None));
    let observed = handshake.clone();
    let configured = state.clone();
    let mut socket = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            let translation = request.uri().path() == "/v1/realtime/translations";
            let path_ok = translation || request.uri().path() == "/v1/realtime";
            let model = request
                .uri()
                .query()
                .and_then(|query| {
                    query
                        .split('&')
                        .find_map(|field| field.strip_prefix("model="))
                })
                .unwrap_or_default()
                .to_owned();
            let actual_model = if translation {
                TRANSLATION_UPSTREAM
            } else {
                REALTIME_UPSTREAM
            };
            let credential = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok());
            let auth = credential == Some(format!("Bearer {}", configured.key).as_str())
                || credential == Some(format!("Bearer {SUPPLIER_SECRET}").as_str());
            let project = request
                .headers()
                .get("openai-project")
                .and_then(|value| value.to_str().ok())
                == Some(PROJECT);
            *observed.lock().expect("provider handshake metadata") = Some((
                translation,
                model.clone(),
                path_ok && model == actual_model && auth && project,
            ));
            Ok(response)
        },
    )
    .await
    .expect("real provider WS handshake");
    let (translation, model, valid) = handshake
        .lock()
        .expect("handshake metadata")
        .take()
        .expect("a recorded actual handshake");
    let id = format!("sess_translation_component_{}", Uuid::new_v4().simple());
    let (commands, mut receive) = mpsc::unbounded_channel();
    let session = Arc::new(Session {
        id: id.clone(),
        translation,
        handshake: valid,
        commands,
        updates: Mutex::new(Vec::new()),
        input: Mutex::new(Vec::new()),
        types: Mutex::new(Vec::new()),
        acknowledged: AtomicBool::new(!translation),
        finished: AtomicBool::new(false),
    });
    state
        .sessions
        .lock()
        .expect("provider sessions")
        .push(session.clone());
    let mut audio = json!({});
    socket.send(Message::Text(json!({"type":"session.created","event_id":format!("created_{id}"),"session":provider_session(&id,translation,&model,audio.clone())}).to_string())).await.expect("send official first server event");
    loop {
        tokio::select! {
            command = receive.recv() => {
                match command {
                    Some(Command::Event(event)) => { if socket.send(Message::Text(event.to_string())).await.is_err() { break; } }
                    Some(Command::Ack) => {
                        if let Some(update) = session.updates.lock().expect("provider update log").last() {
                            if let Some(value) = update.pointer("/session/audio") { audio = value.clone(); }
                        }
                        let event = json!({"type":"session.updated","event_id":format!("updated_{id}"),"session":provider_session(&id,translation,&model,audio.clone())});
                        if socket.send(Message::Text(event.to_string())).await.is_err() { break; }
                        session.acknowledged.store(true,Ordering::Release);
                    }
                    None => { let _ = socket.close(None).await; break; }
                }
            }
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let event: Value = serde_json::from_str(&text).expect("actual client event JSON");
                        let kind = event.get("type").and_then(Value::as_str).expect("an official client event type").to_owned();
                        session.types.lock().expect("provider client event types").push(kind.clone());
                        if kind == "session.update" {
                            if !translation { if let Some(value)=event.pointer("/session/audio") { audio=value.clone(); } }
                            session.updates.lock().expect("provider update log").push(event);
                            if !translation {
                                let updated=json!({"type":"session.updated","event_id":format!("updated_{id}"),"session":provider_session(&id,translation,&model,audio.clone())});
                                if socket.send(Message::Text(updated.to_string())).await.is_err() { break; }
                            }
                        }
                        else if kind == "input_audio_buffer.append" {
                            let bytes = STANDARD.decode(event.get("audio").and_then(Value::as_str).expect("PCM base64")).expect("valid encoded PCM");
                            session.input.lock().expect("provider PCM log").push((bytes,session.acknowledged.load(Ordering::Acquire)));
                        }
                    }
                    Some(Ok(Message::Ping(bytes))) => { if socket.send(Message::Pong(bytes)).await.is_err() { break; } }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    session.finished.store(true, Ordering::Release);
}

struct Collector {
    base: String,
    payloads: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Collector {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind actual OTLP collector");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("collector address")
        );
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().fallback(collect).with_state(payloads.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve actual OTLP HTTP");
        });
        Self {
            base,
            payloads,
            task,
        }
    }
    async fn observed_span(&self, minimum: Duration) {
        tokio::time::timeout(Duration::from_secs(15),async {
            loop {
                let found = self.payloads.lock().expect("OTLP payloads").iter().any(|payload| {
                    payload.get("resourceSpans").and_then(Value::as_array).into_iter().flatten().any(|resource|
                        resource.get("scopeSpans").and_then(Value::as_array).into_iter().flatten().any(|scope|
                            scope.get("spans").and_then(Value::as_array).into_iter().flatten().any(|span| {
                                let nanos = |name|span.get(name).and_then(|value|value.as_u64().or_else(||value.as_str().and_then(|value|value.parse().ok())));
                                matches!((nanos("startTimeUnixNano"),nanos("endTimeUnixNano")),(Some(start),Some(end)) if end>start && u128::from(end-start)>=minimum.as_nanos())
                            })))
                });
                if found { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("a real nonzero observed stream lifetime reaches the existing OTLP exporter");
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn collect(
    State(payloads): State<Arc<Mutex<Vec<Value>>>>,
    request: Request<Body>,
) -> StatusCode {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.uri().path(), "/v1/traces");
    let body = to_bytes(request.into_body(), 4 << 20)
        .await
        .expect("bounded OTLP bytes");
    payloads
        .lock()
        .expect("OTLP payloads")
        .push(serde_json::from_slice(&body).expect("actual OTLP HTTP JSON"));
    StatusCode::OK
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    provider: Provider,
    collector: Collector,
    bundle: Bundle,
    tenant: Uuid,
    key: Uuid,
    bearer: String,
    translation: Uuid,
    realtime: Uuid,
    asr: Uuid,
    management_ids: Mutex<Vec<String>>,
}
fn id(value: &Value, pointer: &str) -> Uuid {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
        .expect("explicit public resource UUID")
}
async fn admin_post(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let answer = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(answer.status, 201, "actual admin fixture write");
    answer.body
}
async fn grant(f: &Fixture, models: &[Uuid]) {
    let path = format!("/admin/tenants/{}", f.tenant);
    let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    let changed =
        f.cp.send_conditional(
            "PATCH",
            &path,
            f.admin.cookie(),
            current.body["version"].as_i64().expect("tenant version"),
            json!({"allowed_model_ids":models}),
        )
        .await;
    assert_eq!(changed.status, 200, "the real tenant grant changes");
}
async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start_with_kine().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = Provider::start().await;
    let collector = Collector::start().await;
    let account = admin_post(&cp,&admin,"/admin/resources/provider_keys",json!({"doc":{"display_name":"translation-component-account","provider":"openai","adapter":"openai","api_base":provider.base,"api_key":provider.state.key,"groups":["shared"],"request":{"default_headers":{"OpenAI-Project":PROJECT}}}})).await;
    let mut models = Vec::new();
    for (alias, upstream) in [
        (TRANSLATION, TRANSLATION_UPSTREAM),
        (REALTIME, REALTIME_UPSTREAM),
        (ASR_ALIAS, ASR_UPSTREAM),
    ] {
        let model = admin_post(&cp,&admin,"/admin/resources/models",json!({"doc":{"display_name":alias,"provider":"openai","model_name":upstream,"provider_key_id":id(&account,"/id")}})).await;
        models.push(id(&model, "/id"));
    }
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    let minted = admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":"translation-component-owner","group_id":group,"member_id":member}),
    )
    .await;
    let key = id(&minted, "/key/id");
    let bearer = minted["plaintext"]
        .as_str()
        .expect("actual owner bearer")
        .to_owned();
    admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/credits"),
        json!({"amount_cny":"1","reason":"Translation 组件测试余额"}),
    )
    .await;
    admin_post(&cp,&admin,&format!("/admin/provider-groups/{group}/rates"),json!({"model_id":null,"cny_per_usd":"1","effective_from":support::world::db_now(&cp).await})).await;
    for model in &models {
        admin_post(&cp,&admin,"/admin/pricing/baselines",json!({"model_id":model,"effective_from":support::world::db_now(&cp).await,"note":"Translation 未报告量不得变零价完整账","card":{"default":{"base":{"input":"10","audio_input":"10","output":"20","audio_output":"20","cache_read":"2"}}}})).await;
    }
    admin_post(&cp,&admin,"/admin/resources/observability_exporters",json!({"doc":{"name":"translation-observed-stream","enabled":true,"kind":"otlp_http","endpoint":format!("{}/v1/traces",collector.base),"headers":{},"content_mode":"metadata_only","sample_rate":1}})).await;
    let bundle = cp
        .issue_bundle(&admin, "translation-component-real-gateway")
        .await;
    let f = Fixture {
        cp,
        admin,
        provider,
        collector,
        bundle,
        tenant,
        key,
        bearer,
        translation: models[0],
        realtime: models[1],
        asr: models[2],
        management_ids: Mutex::new(Vec::new()),
    };
    grant(&f, &[f.translation, f.realtime, f.asr]).await;
    Some(f)
}

struct Gateway {
    state: ProxyState,
    address: std::net::SocketAddr,
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
        .expect("gateway config");
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
            .expect("bind production gateway router");
        let address = listener.local_addr().expect("gateway address");
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.expect("accept gateway HTTP");
                let app = build_router(shared.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        move |request: Request<hyper::body::Incoming>| app.clone().oneshot(request),
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Self {
            state,
            address,
            billing,
            task,
        }
    }
    async fn mint(
        &self,
        f: &Fixture,
        translation: bool,
        asr: bool,
        seconds: i64,
    ) -> (StatusCode, Value) {
        let session = if translation {
            let mut value = json!({"model":TRANSLATION,"audio":{"input":{"noise_reduction":{"type":"near_field","strength":0.7}},"output":{"language":"es"}}});
            if asr {
                value["audio"]["input"]["transcription"] = json!({"model":ASR_ALIAS});
            }
            value
        } else {
            json!({"type":"realtime","model":REALTIME})
        };
        let path = if translation {
            "/v1/realtime/translations/client_secrets"
        } else {
            "/v1/realtime/client_secrets"
        };
        let response=build_router(self.state.clone()).oneshot(Request::builder().method("POST").uri(path).header("authorization",format!("Bearer {}",f.bearer)).header("content-type","application/json")
            .body(Body::from(json!({"session":session,"expires_after":{"anchor":"created_at","seconds":seconds}}).to_string())).expect("official secret creation request")).await.expect("production mint route responds");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("bounded mint reply")
            .to_bytes();
        let body = serde_json::from_slice(&bytes).expect("secret reply JSON without displaying it");
        (status, body)
    }
    async fn connect(
        &self,
        token: &str,
        translation: bool,
        bound_model: &str,
    ) -> Result<Socket, StatusCode> {
        let path = if translation {
            "/v1/realtime/translations"
        } else {
            "/v1/realtime"
        };
        let mut request = format!("ws://{}{path}?model={bound_model}", self.address)
            .into_client_request()
            .expect("official WS URL");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .expect("opaque bearer header"),
        );
        match tokio_tungstenite::connect_async(request).await {
            Ok((socket, response)) => {
                assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
                Ok(socket)
            }
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                if let Some(body) = response.body() {
                    assert_public_frame(&String::from_utf8_lossy(body));
                }
                Err(response.status())
            }
            Err(_) => panic!(
                "the live gateway must give a real HTTP answer to a WS authorization attempt"
            ),
        }
    }
    async fn no_ordinary_billing(&mut self) {
        assert!(
            matches!(
                self.billing.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "unknown Translation observation never becomes an ordinary CP billing aggregate"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), self.billing.recv())
                .await
                .is_err(),
            "no delayed zero-complete billing event is queued"
        );
    }

    async fn management(&mut self, f: &Fixture, models: &[Uuid]) {
        let mut events = Vec::new();
        for model in models {
            let event = tokio::time::timeout(Duration::from_secs(10), self.billing.recv())
                .await
                .expect("actual mint telemetry arrives")
                .expect("the configured billing receiver remains open");
            let event = serde_json::to_value(event)
                .expect("serialize actual management telemetry without displaying it");
            assert_eq!(event.get("inference_stage"), Some(&json!("management")));
            assert_eq!(event.get("inbound_protocol"), Some(&json!("realtime")));
            assert_eq!(event.get("operation"), Some(&json!("realtime")));
            assert_eq!(event.get("status_code"), Some(&json!(200)));
            assert!(
                event.get("api_key_id") == Some(&json!(f.key))
                    && event.get("model_id") == Some(&json!(model))
                    && event.get("requested_model_id") == Some(&json!(model)),
                "real mint telemetry retains its actual owner and selected direct model"
            );
            let operation = id(&event, "/native_operation_id");
            let persisted: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM native_operations WHERE operation_id=$1)",
            )
            .bind(operation)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("read the actual management operation");
            assert!(
                persisted && !operation.is_nil(),
                "mint telemetry references an actual persisted operation"
            );
            for field in [
                "prompt_tokens",
                "completion_tokens",
                "cached_prompt_tokens",
                "total_tokens",
            ] {
                assert!(
                    event
                        .get(field)
                        .is_none_or(|value| value.is_null() || value.as_u64() == Some(0)),
                    "a management mint reports no supplier quantities"
                );
            }
            for field in [
                "prompt_audio_tokens",
                "completion_audio_tokens",
                "prompt_image_tokens",
                "completion_image_tokens",
            ] {
                assert!(event.get(field).is_none_or(Value::is_null),"unreported supplier modality details remain absent or null on management telemetry");
            }
            let event_id = event
                .get("event_id")
                .and_then(Value::as_str)
                .expect("an actual telemetry identity")
                .to_owned();
            assert!(
                !event_id.is_empty(),
                "a real management row has a nonempty identity"
            );
            {
                let mut ids = f.management_ids.lock().expect("observed management IDs");
                assert!(
                    !ids.contains(&event_id),
                    "each actual mint telemetry row is received once"
                );
                ids.push(event_id);
            }
            events.push(event);
        }
        let delivered =
            support::world::deliver(&f.cp.dp_client(&f.bundle), Uuid::new_v4(), events).await;
        assert_eq!(
            delivered.status, 200,
            "actual mint management telemetry reaches real CP intake"
        );
        assert_eq!(delivered.body.get("rejected"), Some(&json!(0)));
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Capability {
    value: String,
    expires_at: i64,
    credential: Option<Uuid>,
    asr: bool,
    purpose: &'static str,
}
async fn capability(
    f: &Fixture,
    gateway: &Gateway,
    translation: bool,
    asr: bool,
    seconds: i64,
) -> Capability {
    let (status, body) = gateway.mint(f, translation, asr, seconds).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "valid official client-secret creation succeeds"
    );
    let value = body
        .get("value")
        .and_then(Value::as_str)
        .expect("an explicit opaque client secret")
        .to_owned();
    assert!(
        !value.is_empty() && value != SUPPLIER_SECRET && value != f.provider.state.key,
        "supplier credentials never leave the gateway as its client capability"
    );
    let expires_at = body
        .get("expires_at")
        .and_then(Value::as_i64)
        .expect("actual public expiry");
    assert!(
        expires_at > Utc::now().timestamp() && expires_at <= Utc::now().timestamp() + seconds + 2,
        "the requested legal short TTL is reflected by actual public expiry"
    );
    let purpose = if translation {
        "translation"
    } else {
        "realtime"
    };
    let shown = body.to_string();
    for secret in [f.provider.state.key.as_str(), SUPPLIER_SECRET] {
        assert!(
            !shown.contains(secret),
            "public secret metadata never discloses a supplier credential"
        );
    }
    if translation {
        assert_eq!(body.pointer("/session/type"), Some(&json!("translation")));
        let model = body
            .pointer("/session/model")
            .and_then(Value::as_str)
            .expect("the formal effective model slot is retained");
        assert!(model==TRANSLATION || model==TRANSLATION_UPSTREAM,"effective metadata retains the selected actual model or its equivalent public alias without imposing a new rewrite contract");
        assert!(
            body.pointer("/session/id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty()),
            "the formal Translation session identity is retained"
        );
        if asr {
            let model = body
                .pointer("/session/audio/input/transcription/model")
                .and_then(Value::as_str)
                .expect("requested pending ASR defaults remain in effective metadata");
            assert!(
                model == ASR_ALIAS || model == ASR_UPSTREAM,
                "pending B configuration is retained without inheriting or fabricating its grant"
            );
        }
    }
    Capability {
        value,
        expires_at,
        credential: None,
        asr,
        purpose,
    }
}

async fn event(socket: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    assert_public_frame(&text);
                    break serde_json::from_str(&text).expect("official server event JSON");
                }
                Some(Ok(Message::Ping(bytes))) => socket
                    .send(Message::Pong(bytes))
                    .await
                    .expect("answer transport ping"),
                _ => panic!("the actual WS must forward the expected server event before closing"),
            }
        }
    })
    .await
    .expect("bounded actual WS event")
}

fn assert_public_frame(text: &str) {
    assert!(
        !text.contains(SUPPLIER_SECRET) && !text.contains("sk-translation-component-"),
        "client events never expose either controlled supplier credential"
    );
}
async fn until(socket: &mut Socket, kind: &str) -> Value {
    loop {
        let value = event(socket).await;
        assert!(
            value.get("type") != Some(&json!("error")),
            "a valid post-ack client flow must not fail"
        );
        if value.get("type").and_then(Value::as_str) == Some(kind) {
            return value;
        }
    }
}
async fn defaults(
    f: &Fixture,
    gateway: &Gateway,
    cap: &mut Capability,
    number: usize,
) -> (Socket, Arc<Session>) {
    let mut socket = gateway
        .connect(&cap.value, true, TRANSLATION)
        .await
        .unwrap_or_else(|status| panic!("valid Translation capability must upgrade: {status}"));
    let session = f.provider.session(number).await;
    assert!(session.translation && session.handshake,"the actual Translation endpoint receives only its bound upstream model and supplier-side credentials");
    let update = session.update(0).await;
    assert!(
        update.pointer("/session/audio/output/language") == Some(&json!("es")),
        "minted target-language defaults are replayed"
    );
    assert!(
        update.pointer("/session/audio/input/noise_reduction")
            == Some(&json!({"type":"near_field","strength":0.7})),
        "valid audio defaults remain exact"
    );
    if cap.asr {
        assert!(
            update.pointer("/session/audio/input/transcription/model")
                == Some(&json!(ASR_UPSTREAM)),
            "the secondary public alias resolves to its actual upstream model before configuration"
        );
    } else {
        assert!(
            update
                .pointer("/session/audio/input/transcription")
                .is_none_or(Value::is_null),
            "source ASR stays absent when it was not configured"
        );
    }
    assert!(
        update.pointer("/session/model").is_none() && update.pointer("/session/type").is_none(),
        "Translation defaults use the official audio-only update contract"
    );
    socket.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":"early_before_defaults_ack","audio":STANDARD.encode(EARLY_INPUT)}).to_string())).await.expect("probe the live connection before the held provider acknowledgment");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        session
            .input
            .lock()
            .expect("active provider reader PCM log")
            .is_empty(),
        "client audio cannot reach the provider before its actual defaults acknowledgment"
    );
    session.ack();
    let acknowledged = until(&mut socket, "session.updated").await;
    assert!(
        acknowledged.pointer("/session/type") == Some(&json!("translation")),
        "effective acknowledged session remains Translation"
    );
    session.input(EARLY_INPUT).await;
    socket.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":"valid_pcm_after_defaults_ack","audio":STANDARD.encode(INPUT)}).to_string())).await.expect("send valid PCM after provider acknowledgment");
    session.input(INPUT).await;
    observe_capability(f, cap).await;
    assert!(
        session
            .input
            .lock()
            .expect("provider PCM log")
            .iter()
            .all(|(_, acked)| *acked),
        "every actually forwarded PCM chunk follows the provider acknowledgment"
    );
    (socket, session)
}

async fn observe_capability(f: &Fixture, cap: &mut Capability) {
    let rows:Vec<(Uuid,Uuid,String,chrono::DateTime<Utc>)>=sqlx::query_as("SELECT c.id,c.api_key_id,c.purpose,c.expires_at FROM native_live_sessions s JOIN native_realtime_credentials c ON c.id=s.credential_id WHERE s.api_key_id=$1 AND s.purpose=$2")
        .bind(f.key).bind(cap.purpose).fetch_all(f.cp.db().pool()).await.expect("read real successful live sessions joined to durable capabilities");
    assert!(
        !rows.is_empty(),
        "a positive actual WS has a durable credential binding"
    );
    let credential = rows[0].0;
    assert!(
        !credential.is_nil(),
        "the actual capability identity is non-nil"
    );
    assert!(rows.iter().all(|(id,key,purpose,expiry)|*id==credential && *key==f.key && purpose==cap.purpose && expiry.timestamp()==cap.expires_at),"successful sessions share one actual owner/purpose/expiry-bound capability without depending on opaque token internals");
    if let Some(previous) = cap.credential {
        assert!(
            previous == credential,
            "reusing one secret cannot mint another durable capability"
        );
    }
    cap.credential = Some(credential);
}

async fn denied_connection(
    f: &Fixture,
    gateway: &Gateway,
    cap: &Capability,
    translation: bool,
    bound_model: &str,
) {
    let attempts = f.provider.attempts();
    let before:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM native_operations),(SELECT count(*) FROM native_live_sessions)")
        .fetch_one(f.cp.db().pool()).await.expect("read actual authority counts before refusal");
    match gateway.connect(&cap.value, translation, bound_model).await {
        Err(status) => assert!(
            status.is_client_error(),
            "purpose/current-grant/expiry denial is a real HTTP client refusal: {status}"
        ),
        Ok(mut socket) => {
            let _ = socket.close(None).await;
            panic!("a refused capability must not establish a WS session");
        }
    }
    assert_eq!(
        f.provider.attempts(),
        attempts,
        "a refused capability never dispatches a provider handshake"
    );
    let after:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM native_operations),(SELECT count(*) FROM native_live_sessions)")
        .fetch_one(f.cp.db().pool()).await.expect("read authority counts after actual refusal");
    assert_eq!(
        after, before,
        "a refused capability creates no operation or owned live session"
    );
}

async fn denied_frame(socket: &mut Socket) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    assert_public_frame(&text);
                    let event: Value = serde_json::from_str(&text).expect("gateway event JSON");
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
    .expect("the gateway gives a bounded current-model or ASR denial");
}

async fn disconnect(mut socket: Socket, session: &Session) {
    let _ = socket.close(None).await;
    drop(socket);
    session.closed().await;
}

fn audio_delta(id: &str, bytes: &[u8], elapsed: i64) -> Value {
    json!({"type":"session.output_audio.delta","event_id":id,"delta":STANDARD.encode(bytes),"channels":1,"format":"pcm16","sample_rate":24000,"elapsed_ms":elapsed})
}

async fn finish_translation(mut socket: Socket, session: &Session, tail: bool) {
    socket
        .send(Message::Text(
            json!({"type":"session.close","event_id":"client_translation_close"}).to_string(),
        ))
        .await
        .expect("send the official Translation finish event");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if session
                .types
                .lock()
                .expect("provider event log")
                .iter()
                .any(|kind| kind == "session.close")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the official close reaches the provider before it drains output");
    if tail {
        let output = audio_delta("translation_final_draining_audio", &[17, 0, 18, 0], 600);
        session.emit(output.clone());
        assert!(
            event(&mut socket).await == output,
            "draining PCM metadata survives after client finish and before server closed"
        );
    }
    let closed = json!({"type":"session.closed","event_id":format!("closed_{}",session.id)});
    session.emit(closed.clone());
    assert!(event(&mut socket).await==closed,"official minimal Translation closed event stays id/type only, without invented supplier usage");
    disconnect(socket, session).await;
}

async fn no_supplier_charge(f: &Fixture) {
    let management = f
        .management_ids
        .lock()
        .expect("actual delivered management IDs")
        .clone();
    let amounts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM pricing_charges_cny WHERE tenant_id=$1::text AND status='charged' AND NOT(event_id=ANY($2::text[]))),count(*),COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'")
        .bind(f.tenant).bind(management).fetch_one(f.cp.db().pool()).await.expect("read new WS paid facts and every tenant debit including management");
    assert_eq!(amounts,(0,0,0),"defined Translation events report no supplier usage; no nonzero or fabricated zero-complete charge is invented");
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(f.tenant)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("read actual funded account balance");
    assert_eq!(
        balance, 1_000_000,
        "known nonzero model prices do not turn unreported supplier usage into a bill"
    );
}

async fn unknown_sessions(f: &Fixture, cap: &Capability, number: usize) {
    tokio::time::timeout(Duration::from_secs(15),async {
        loop {
            let rows:Vec<(String,String,String)>=sqlx::query_as("SELECT state,coverage,accounting_state FROM native_live_sessions WHERE credential_id=$1")
                .bind(cap.credential.expect("a positive actual WS proved its durable credential ID")).fetch_all(f.cp.db().pool()).await.expect("read actual persisted capability-bound live sessions");
            assert_eq!(rows.len(),number,"the reusable secret creates the expected independent sessions");
            assert!(rows.iter().all(|(_,coverage,accounting)|coverage!="complete" && accounting!="settled"),"no observed Translation close can claim complete zero supplier accounting");
            if rows.iter().all(|(state,coverage,accounting)|state=="closed" && coverage=="unknown" && accounting=="awaiting_reconciliation") { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("all closed Translation streams remain unknown and awaiting reconciliation");
}

// 专用目的、真实到期、多会话和有效 PCM 路径必须一起成立；无供应商 usage 不能变成零价完整账。
#[tokio::test]
async fn translation_gateway_secret_is_reusable_purpose_bound_and_expiring_while_pcm_streams_stay_unknown(
) {
    let Some(f) = fixture().await else { return };
    let mut gateway = Gateway::start(&f).await;
    let mut translated = capability(&f, &gateway, true, true, 15).await;
    let mut ordinary = capability(&f, &gateway, false, false, 60).await;
    gateway.management(&f, &[f.translation, f.realtime]).await;
    let mut ordinary_socket = gateway
        .connect(&ordinary.value, false, REALTIME)
        .await
        .unwrap_or_else(|status| {
            panic!("a valid ordinary capability must work on its own endpoint: {status}")
        });
    let ordinary_session = f.provider.session(0).await;
    assert!(
        !ordinary_session.translation && ordinary_session.handshake,
        "ordinary purpose has a real positive transport"
    );
    assert!(
        until(&mut ordinary_socket, "session.created")
            .await
            .pointer("/session/type")
            == Some(&json!("realtime"))
    );
    observe_capability(&f, &mut ordinary).await;
    disconnect(ordinary_socket, &ordinary_session).await;
    let (mut first, first_session) = defaults(&f, &gateway, &mut translated, 1).await;
    let observed = Instant::now();
    let (second, second_session) = defaults(&f, &gateway, &mut translated, 2).await;
    assert!(
        first_session.id != second_session.id,
        "one secret admits two distinct provider sessions before actual expiry"
    );
    denied_connection(&f, &gateway, &translated, false, TRANSLATION).await;
    denied_connection(&f, &gateway, &ordinary, true, REALTIME).await;
    finish_translation(second, &second_session, false).await;
    let expiry_ms = (translated.expires_at * 1000 - Utc::now().timestamp_millis()).max(0) as u64;
    assert!(
        expiry_ms <= 30_000,
        "the bounded fixture waits only the requested actual short expiry"
    );
    tokio::time::sleep(Duration::from_millis(expiry_ms + 150)).await;
    denied_connection(&f, &gateway, &translated, true, TRANSLATION).await;
    let after_expiry = [9, 0, 10, 0];
    first.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":"established_stream_after_secret_expiry","audio":STANDARD.encode(after_expiry)}).to_string())).await.expect("an established session may continue after its creation capability expires");
    first_session.input(&after_expiry).await;
    let outputs = [
        audio_delta("translation_audio_a", &[11, 0, 12, 0], 400),
        audio_delta("translation_audio_b", &[13, 0, 14, 0], 400),
        json!({"type":"session.input_transcript.delta","event_id":"translation_source_text","delta":"source","elapsed_ms":400}),
        json!({"type":"session.output_transcript.delta","event_id":"translation_target_text","delta":"hola","elapsed_ms":400}),
    ];
    for output in outputs {
        first_session.emit(output.clone());
        assert!(event(&mut first).await==output,"distinct aligned PCM/transcript deltas retain bytes, fields and event identity even when elapsed_ms repeats");
    }
    finish_translation(first, &first_session, true).await;
    assert_eq!(
        f.provider.attempts(),
        3,
        "wrong purpose and expired attempts never dial any extra provider session"
    );
    let duration = observed
        .elapsed()
        .saturating_sub(Duration::from_millis(500));
    assert!(
        duration > Duration::from_secs(1),
        "the actual established Translation lifetime is nonzero"
    );
    f.collector.observed_span(duration).await;
    unknown_sessions(&f, &translated, 2).await;
    gateway.no_ordinary_billing().await;
    no_supplier_charge(&f).await;
}

async fn denied_default_enable(f: &Fixture, gateway: &Gateway, cap: &Capability) {
    let before = f
        .provider
        .state
        .sessions
        .lock()
        .expect("positive provider sessions")
        .len();
    match gateway.connect(&cap.value, true, TRANSLATION).await {
        Err(status) => assert!(
            status.is_client_error(),
            "current B validation may refuse before upgrading"
        ),
        Ok(mut socket) => {
            let attempt=socket.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":"audio_under_ungranted_minted_defaults","audio":STANDARD.encode(INPUT)}).to_string())).await;
            if attempt.is_ok() {
                denied_frame(&mut socket).await;
            }
            let _ = socket.close(None).await;
            drop(socket);
        }
    }
    let opened: Vec<_> = f
        .provider
        .state
        .sessions
        .lock()
        .expect("actual provider sessions")
        .iter()
        .skip(before)
        .cloned()
        .collect();
    for session in opened {
        session.closed().await;
        assert!(
            session.handshake && session.translation,
            "an optional primary connection still uses the bound Translation model"
        );
        assert!(session.updates.lock().expect("drained default updates").iter().all(|event|event.pointer("/session/audio/input/transcription").is_none_or(Value::is_null)),"ungranted default B is not sent or enabled even if the primary provider connection opened");
        assert!(
            session.input.lock().expect("drained PCM log").is_empty(),
            "no audio reaches an ungranted B default before a safe override"
        );
    }
}

// 默认 B 不是永久授权：mint 可成功，但最终启用无当前 grant 的 B 前必须阻止配置和音频。
#[tokio::test]
async fn translation_minted_secondary_defaults_need_current_grant_before_they_are_enabled() {
    let Some(f) = fixture().await else { return };
    let mut gateway = Gateway::start(&f).await;
    let mut safe = capability(&f, &gateway, true, false, 60).await;
    gateway.management(&f, &[f.translation]).await;
    let (safe_socket, safe_session) = defaults(&f, &gateway, &mut safe, 0).await;
    finish_translation(safe_socket, &safe_session, false).await;
    let issued_with_grant = capability(&f, &gateway, true, true, 60).await;
    gateway.management(&f, &[f.translation]).await;
    grant(&f, &[f.translation, f.realtime]).await;
    denied_default_enable(&f, &gateway, &issued_with_grant).await;
    let issued_without_grant = capability(&f, &gateway, true, true, 60).await;
    gateway.management(&f, &[f.translation]).await;
    denied_default_enable(&f, &gateway, &issued_without_grant).await;
    unknown_sessions(&f, &safe, 1).await;
    gateway.no_ordinary_billing().await;
    no_supplier_charge(&f).await;
}

// 原 strict stale-snapshot 即时撤权合同及 368 RED 已逐字归档；本轻替代保留动态 B gate，并在真实配置同步后验证主模型撤销。
#[tokio::test]
async fn translation_dynamic_asr_gate_and_projected_primary_grant_revocation_precede_new_audio() {
    let Some(f) = fixture().await else { return };
    grant(&f, &[f.translation, f.realtime]).await;
    let mut gateway = Gateway::start(&f).await;
    let mut cap = capability(&f, &gateway, true, false, 60).await;
    gateway.management(&f, &[f.translation]).await;
    let (mut secondary_socket, secondary_session) = defaults(&f, &gateway, &mut cap, 0).await;
    let (mut primary_socket, primary_session) = defaults(&f, &gateway, &mut cap, 1).await;
    let before = secondary_session
        .types
        .lock()
        .expect("positive provider event log")
        .clone();
    secondary_socket.send(Message::Text(json!({"type":"session.update","event_id":"ungranted_translation_source_asr","session":{"audio":{"input":{"transcription":{"model":ASR_ALIAS}}}}}).to_string())).await.expect("probe an existing positive Translation stream");
    denied_frame(&mut secondary_socket).await;
    disconnect(secondary_socket, &secondary_session).await;
    assert!(
        secondary_session
            .types
            .lock()
            .expect("drained provider event log")
            .as_slice()
            == before.as_slice(),
        "ungranted dynamic B configuration never reaches the provider"
    );
    assert_eq!(
        secondary_session
            .updates
            .lock()
            .expect("provider update log")
            .len(),
        1,
        "only acknowledged safe defaults were dispatched"
    );
    let before = primary_session
        .input
        .lock()
        .expect("positive primary PCM log")
        .clone();
    assert!(
        !before.is_empty(),
        "actual acknowledged audio precedes current main-model withdrawal"
    );
    grant(&f, &[f.realtime]).await;
    let current: Vec<Uuid> =
        sqlx::query_scalar("SELECT allowed_model_ids FROM tenants WHERE id=$1")
            .bind(f.tenant)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("read actual current model grants");
    assert!(
        !current.contains(&f.translation),
        "the primary Translation grant really was withdrawn"
    );
    // 公开资源的 id grant 为权威；CP→kine 的真实投影先保留普通模型，再撤掉 Translation。
    let projected = projected_snapshot(&f.cp).await;
    let projected_key = projected
        .apikeys
        .get_by_id(&f.key.to_string())
        .expect("the actual projected creator key remains represented");
    let projected_grants = projected_key
        .value
        .allowed_model_ids
        .as_ref()
        .expect("the actual CP projection carries authoritative model resource ids");
    assert!(
        projected_grants.contains(&f.realtime.to_string())
            && !projected_grants.contains(&f.translation.to_string()),
        "the real projected key keeps ordinary Realtime while withdrawing Translation"
    );
    gateway.state.snapshot.store(projected);
    let applied = gateway.state.snapshot.load();
    let applied_key = applied
        .apikeys
        .get_by_id(&f.key.to_string())
        .expect("the serving gateway reads the actual original creator key");
    let applied_grants = applied_key
        .value
        .allowed_model_ids
        .as_ref()
        .expect("the serving snapshot keeps authoritative model resource ids");
    assert!(
        applied_grants.contains(&f.realtime.to_string())
            && !applied_grants.contains(&f.translation.to_string()),
        "the actual serving snapshot has applied the primary-model revocation before new PCM"
    );
    let attempt=primary_socket.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":"new_pcm_after_primary_revocation","audio":STANDARD.encode([21,0,22,0])}).to_string())).await;
    if attempt.is_ok() {
        denied_frame(&mut primary_socket).await;
    }
    disconnect(primary_socket, &primary_session).await;
    assert!(primary_session.input.lock().expect("drained primary PCM log").as_slice()==before.as_slice(),"primary-model withdrawal stops new audio after the actual serving snapshot applies the revoked grant, even with an unexpired secret");
    denied_connection(&f, &gateway, &cap, true, TRANSLATION).await;
    unknown_sessions(&f, &cap, 2).await;
    gateway.no_ordinary_billing().await;
    no_supplier_charge(&f).await;
}
