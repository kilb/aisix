//! 真实管理写入、PG/kine/mTLS、生产网关 WS、OTLP HTTP 与指标的联合组件合同。
//! 供应商仅为本地 WS 协议替身；不声称真实供应商或电话媒体端到端覆盖。

#[path = "native_realtime_gateway_accounting/asr_duration.rs"]
mod asr_duration;
#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aisix_admin::{metrics_router, ConfigStore, InMemoryStore, ModelsStatusState};
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::config::PrometheusConfig;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{ConfigStatus, ProxyConfig, SourceKind};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::Engine as _;
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

const MODEL: &str = "realtime-gateway-accounting";
const PROJECT: &str = "proj_realtime_gateway_accounting";
const SESSION: &str = "sess_realtime_gateway_accounting";
const FIRST: &str = "resp_realtime_gateway_first";
const SECOND: &str = "resp_realtime_gateway_cancelled";
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct ProviderState {
    secret: String,
    output: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    received: Mutex<Vec<String>>,
    handshakes: Mutex<Vec<bool>>,
    secondary_models: Mutex<Vec<bool>>,
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
            .expect("bind the controlled WS provider");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("provider address")
        );
        let state = Arc::new(ProviderState {
            secret: format!("sk-ws-component-{}", Uuid::new_v4().simple()),
            output: Mutex::new(None),
            received: Mutex::new(Vec::new()),
            handshakes: Mutex::new(Vec::new()),
            secondary_models: Mutex::new(Vec::new()),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("accept the controlled provider connection");
            let observed = shared.clone();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response| {
                    let authorized = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        == Some(format!("Bearer {}", observed.secret).as_str());
                    let project = request
                        .headers()
                        .get("openai-project")
                        .and_then(|value| value.to_str().ok())
                        == Some(PROJECT);
                    let path_matches = request.uri().path() == "/v1/realtime";
                    let model_matches = request.uri().query().is_some_and(|query| {
                        query.split('&').any(|field| field == "model=gpt-realtime")
                    });
                    let route = path_matches && model_matches;
                    if !(authorized && project && route) {
                        let path_without_v1 = request.uri().path() == "/realtime";
                        eprintln!("realtime component handshake mismatch: authorization_match={authorized} project_match={project} path_match={path_matches} model_match={model_matches} path_without_v1={path_without_v1}");
                    }
                    observed
                        .handshakes
                        .lock()
                        .expect("provider handshake log")
                        .push(authorized && project && route);
                    Ok(response)
                },
            )
            .await
            .expect("perform the real provider WS handshake");
            let (sender, mut output) = mpsc::unbounded_channel();
            *shared.output.lock().expect("provider output channel") = Some(sender);
            socket.send(Message::Text(json!({"type":"session.created","event_id":"event_session_created",
                "session":{"type":"realtime","object":"realtime.session","id":SESSION,"model":"gpt-realtime","output_modalities":["text"],"tools":[],"tool_choice":"auto","max_output_tokens":"inf","expires_at":chrono::Utc::now().timestamp()+3600}}).to_string())).await.expect("send the actual provider session identity");
            loop {
                tokio::select! {
                    message = output.recv() => {
                        let Some(message) = message else { break; };
                        let close = message.is_close();
                        if socket.send(message).await.is_err() || close { break; }
                    }
                    message = socket.next() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let event: Value = serde_json::from_str(&text).expect("client event JSON");
                                let event_type = event.get("type").and_then(Value::as_str).expect("an explicit client event type").to_owned();
                                if event_type == "session.update" {
                                    if let Some(model) = event.pointer("/session/audio/input/transcription/model").and_then(Value::as_str) {
                                        shared.secondary_models.lock().expect("provider secondary model log").push(model == ASR);
                                    }
                                }
                                shared.received.lock().expect("provider input log").push(event_type);
                            }
                            Some(Ok(Message::Ping(bytes))) => { if socket.send(Message::Pong(bytes)).await.is_err() { break; } }
                            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                            _ => {}
                        }
                    }
                }
            }
        });
        Self { base, state, task }
    }

    fn send(&self, event: Value) {
        self.state
            .output
            .lock()
            .expect("provider output channel")
            .as_ref()
            .expect("provider handshake completed")
            .send(Message::Text(event.to_string()))
            .expect("send a controlled official provider event");
    }

    fn close(&self) {
        self.state
            .output
            .lock()
            .expect("provider output channel")
            .as_ref()
            .expect("provider handshake completed")
            .send(Message::Close(None))
            .expect("close the actual provider transport without inventing a terminal usage event");
    }

    async fn received(&self, expected: &[&str]) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let events = self
                    .state
                    .received
                    .lock()
                    .expect("provider input log")
                    .clone();
                if events.len() >= expected.len() {
                    assert!(
                        events
                            .iter()
                            .map(String::as_str)
                            .eq(expected.iter().copied()),
                        "the provider receives exactly the authorized client event types"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("actual client events reach the provider");
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Collector {
    base: String,
    spans: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Collector {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the real local OTLP HTTP collector");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("collector address")
        );
        let spans = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .fallback(collect_spans)
            .with_state(spans.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve the local OTLP collector");
        });
        Self { base, spans, task }
    }

    async fn aggregate(&self) -> BTreeMap<String, Value> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let mut usage_spans = Vec::new();
                for payload in self.spans.lock().expect("collector payloads").iter() {
                    for resource in payload
                        .get("resourceSpans")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        for scope in resource
                            .get("scopeSpans")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            for span in scope
                                .get("spans")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                            {
                                let attributes: BTreeMap<_, _> = span
                                    .get("attributes")
                                    .and_then(Value::as_array)
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|field| {
                                        Some((
                                            field.get("key")?.as_str()?.to_owned(),
                                            field.get("value")?.clone(),
                                        ))
                                    })
                                    .collect();
                                if optional_otlp_integer(&attributes, "gen_ai.usage.input_tokens")
                                    == Some(20)
                                    && optional_otlp_integer(
                                        &attributes,
                                        "gen_ai.usage.output_tokens",
                                    ) == Some(6)
                                {
                                    usage_spans.push(attributes);
                                }
                            }
                        }
                    }
                }
                if !usage_spans.is_empty() {
                    assert_eq!(
                        usage_spans.len(),
                        1,
                        "one complete session aggregate is exported exactly once"
                    );
                    break usage_spans.remove(0);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("real OTLP HTTP export contains the full usage aggregate")
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn collect_spans(
    State(spans): State<Arc<Mutex<Vec<Value>>>>,
    request: Request<Body>,
) -> StatusCode {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.uri().path(), "/v1/traces");
    let body = to_bytes(request.into_body(), 4 << 20)
        .await
        .expect("read bounded OTLP payload");
    let payload = serde_json::from_slice(&body).expect("the real emitter sends OTLP HTTP JSON");
    spans.lock().expect("collector payloads").push(payload);
    StatusCode::OK
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    provider: Provider,
    collector: Collector,
    tenant: Uuid,
    key: Uuid,
    model: Uuid,
    member: Uuid,
    bearer: String,
    bundle: Bundle,
}

fn id(body: &Value, pointer: &str) -> Uuid {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
        .expect("an explicit resource UUID")
}

async fn admin_post(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let response = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(response.status, 201, "actual admin fixture write succeeds");
    response.body
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start_with_kine().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = Provider::start().await;
    let collector = Collector::start().await;
    let account = admin_post(&cp, &admin, "/admin/resources/provider_keys", json!({"doc":{
        "display_name":"realtime-accounting-ws-account","provider":"openai","adapter":"openai","api_base":provider.base,
        "api_key":provider.state.secret,"groups":["shared"],"request":{"default_headers":{"OpenAI-Project":PROJECT}}
    }})).await;
    let model = admin_post(&cp, &admin, "/admin/resources/models", json!({"doc":{
        "display_name":MODEL,"provider":"openai","model_name":"gpt-realtime","provider_key_id":id(&account,"/id")
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
    assert_eq!(granted.status, 200);
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    let minted = admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":"realtime-accounting-ws-key","member_id":member,"group_id":group}),
    )
    .await;
    let key = id(&minted, "/key/id");
    let bearer = minted["plaintext"]
        .as_str()
        .expect("the real issued bearer")
        .to_owned();
    admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/credits"),
        json!({"amount_cny":"1","reason":"实时 WS 账务组件测试余额"}),
    )
    .await;
    admin_post(&cp, &admin, &format!("/admin/provider-groups/{group}/rates"), json!({"model_id":null,"cny_per_usd":"1","effective_from":support::world::db_now(&cp).await})).await;
    admin_post(&cp, &admin, "/admin/pricing/baselines", json!({"model_id":model,"effective_from":support::world::db_now(&cp).await,"note":"实时 WS 固定非零价格","card":{"default":{"base":{"input":"10","cache_read":"2","output":"20"}}}})).await;
    admin_post(&cp, &admin, "/admin/resources/observability_exporters", json!({"doc":{
        "name":"realtime-accounting-otlp","enabled":true,"kind":"otlp_http","endpoint":format!("{}/v1/traces", collector.base),
        "headers":{},"content_mode":"metadata_only","sample_rate":1
    }})).await;
    let bundle = cp
        .issue_bundle(&admin, "realtime-accounting-real-ws-gateway")
        .await;
    Some(Fixture {
        cp,
        admin,
        provider,
        collector,
        tenant,
        key,
        model,
        member,
        bearer,
        bundle,
    })
}

struct Gateway {
    state: ProxyState,
    address: SocketAddr,
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
        .expect("gateway configuration");
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
            .expect("bind the actual gateway HTTP listener");
        let address = listener.local_addr().expect("gateway address");
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener
                    .accept()
                    .await
                    .expect("accept a gateway connection");
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

    async fn connect(&self, bearer: &str) -> Socket {
        let mut request = format!("ws://{}/v1/realtime?model={MODEL}", self.address)
            .into_client_request()
            .expect("official WS request");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {bearer}").parse().expect("bearer header"),
        );
        let (socket, response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("the actual gateway authenticates and upgrades the connection");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        socket
    }

    async fn assert_billing_sink_empty(&mut self) {
        assert!(
            matches!(
                self.billing.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "the CP ordinary billing sink receives no canonical session aggregate"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), self.billing.recv())
                .await
                .is_err(),
            "no delayed aggregate is queued for ordinary CP billing"
        );
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn next_event(socket: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    break serde_json::from_str(&text).expect("an official JSON server event")
                }
                Some(Ok(Message::Ping(bytes))) => socket
                    .send(Message::Pong(bytes))
                    .await
                    .expect("answer the ping"),
                _ => panic!("the gateway must forward the expected server event before closing"),
            }
        }
    })
    .await
    .expect("the real gateway forwards a provider event")
}

fn done(response: &str, event: &str, status: &str, reported_zero: bool) -> Value {
    let mut usage = json!({"input_tokens":10,"output_tokens":3,"total_tokens":13,
        "input_token_details":{"cached_tokens":2,"text_tokens":10},"output_token_details":{"text_tokens":3}});
    if reported_zero {
        usage["input_token_details"]["audio_tokens"] = json!(0);
        usage["output_token_details"]["audio_tokens"] = json!(0);
        usage["input_token_details"]["cached_tokens_details"] =
            json!({"text_tokens":2,"audio_tokens":0});
    }
    json!({"type":"response.done","event_id":event,"response":{"object":"realtime.response","id":response,"status":status,"status_details":null,"output":[],"usage":usage}})
}

async fn created(provider: &Provider, socket: &mut Socket, response: &str) {
    provider.send(json!({"type":"response.created","event_id":format!("created_{response}"),"response":{"object":"realtime.response","id":response,"status":"in_progress","output":[]}}));
    let event = next_event(socket).await;
    assert!(
        event.get("type") == Some(&json!("response.created"))
            && event.pointer("/response/id") == Some(&json!(response)),
        "the expected response is accepted before it can report paid work"
    );
}

async fn paid(f: &Fixture, number: i64) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rows: (i64, i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM native_live_journal), \
                (SELECT count(*) FROM native_live_outbox), (SELECT count(*) FROM pricing_charges_cny WHERE tenant_id=$1::text AND status='charged'), \
                count(*),COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'")
                .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("read every actual canonical fact, priced row and tenant debit");
            assert!(rows.0 <= number && rows.1 <= number && rows.2 <= number && rows.3 <= number && rows.4 <= number*144, "no duplicate fact or parallel aggregate debit: {rows:?}");
            if rows == (number, number, number, number, number*144) { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the production WS observer journals and actually prices each nonzero response");
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(f.tenant)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("read the actual balance");
    assert_eq!(
        balance,
        1_000_000 - number * 144,
        "144 micro-CNY per response including the cached subset"
    );
}

async fn scrape(state: &ProxyState) -> String {
    let response = metrics_router(
        state.metrics.clone(),
        ConfigStatus::new(SourceKind::Etcd),
        &PrometheusConfig {
            enabled: true,
            path: "/metrics".into(),
            addr: "127.0.0.1:0".into(),
        },
        ModelsStatusState {
            store: InMemoryStore::new() as Arc<dyn ConfigStore>,
            runtime_status_tracker: None,
        },
    )
    .oneshot(
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .expect("an actual metrics GET"),
    )
    .await
    .expect("the production metrics router answers");
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .expect("read actual metrics exposition")
            .to_bytes()
            .to_vec(),
    )
    .expect("UTF8 metrics")
}

fn metric(text: &str, name: &str) -> f64 {
    let samples: Vec<f64> = text
        .lines()
        .filter(|line| {
            line.starts_with(&format!("{name}{{"))
                && line.contains("endpoint=\"/v1/realtime\"")
                && line.contains("inbound_protocol=\"realtime\"")
        })
        .map(|line| {
            line.split_whitespace()
                .nth(1)
                .expect("metric value")
                .parse()
                .expect("numeric metric")
        })
        .collect();
    assert!(
        !samples.is_empty(),
        "the actual realtime traffic must expose the required metric family {name}"
    );
    samples.iter().sum()
}

fn otlp_integer(attributes: &BTreeMap<String, Value>, name: &str) -> i64 {
    optional_otlp_integer(attributes, name)
        .expect("an explicitly exported valid OTLP integer usage attribute")
}

fn optional_otlp_integer(attributes: &BTreeMap<String, Value>, name: &str) -> Option<i64> {
    let value = attributes.get(name)?.get("intValue")?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

// 取消终态和换投递 ID 都不能丢量；有真实导出与指标的正向证据后，空计费 sink 才能证明无双扣。
#[tokio::test]
async fn canonical_response_journal_is_paid_once_while_full_session_aggregate_exports_without_cp_rebilling(
) {
    let Some(f) = fixture().await else { return };
    let mut gateway = Gateway::start(&f).await;
    let mut socket = gateway.connect(&f.bearer).await;
    let session = next_event(&mut socket).await;
    assert!(
        session.get("type") == Some(&json!("session.created")),
        "a real provider session was established"
    );
    assert!(
        f.provider
            .state
            .handshakes
            .lock()
            .expect("provider handshake log")
            .as_slice()
            == [true],
        "the configured credential/project/model reached one actual provider handshake"
    );
    socket.send(Message::Text(json!({"type":"response.create","event_id":"client_create_first","response":{"output_modalities":["text"]}}).to_string())).await.expect("request a response over the actual gateway WS");
    f.provider.received(&["response.create"]).await;
    created(&f.provider, &mut socket, FIRST).await;
    let first = done(FIRST, "provider_done_first", "completed", true);
    f.provider.send(first.clone());
    let forwarded = next_event(&mut socket).await;
    assert!(
        forwarded.pointer("/response/usage") == first.pointer("/response/usage"),
        "reported zero detail counters remain present on the client boundary"
    );
    paid(&f, 1).await;
    let mut duplicate = first;
    duplicate["event_id"] = json!("provider_done_first_redelivery");
    f.provider.send(duplicate);
    let repeated = next_event(&mut socket).await;
    assert!(
        repeated.pointer("/response/id") == Some(&json!(FIRST)),
        "the resource identity survives a different delivery ID"
    );
    paid(&f, 1).await;
    socket.send(Message::Text(json!({"type":"response.create","event_id":"client_create_second","response":{"output_modalities":["text"]}}).to_string())).await.expect("request a separate response");
    f.provider
        .received(&["response.create", "response.create"])
        .await;
    created(&f.provider, &mut socket, SECOND).await;
    socket
        .send(Message::Text(
            json!({"type":"response.cancel","event_id":"client_cancel_second"}).to_string(),
        ))
        .await
        .expect("cancel an accepted in-progress response");
    f.provider
        .received(&["response.create", "response.create", "response.cancel"])
        .await;
    let second = done(SECOND, "provider_done_cancelled", "cancelled", false);
    f.provider.send(second.clone());
    let forwarded = next_event(&mut socket).await;
    assert!(
        forwarded.pointer("/response/status") == Some(&json!("cancelled")),
        "the actual terminal response is cancelled"
    );
    assert!(
        forwarded.pointer("/response/usage") == second.pointer("/response/usage"),
        "absent audio detail counters stay absent instead of fabricated zero"
    );
    paid(&f, 2).await;
    f.provider.close();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(Message::Ping(bytes)) => {
                    let _ = socket.send(Message::Pong(bytes)).await;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the actual transport closes without a fabricated usage terminal");
    drop(socket);
    let aggregate = f.collector.aggregate().await;
    assert_eq!(otlp_integer(&aggregate, "gen_ai.usage.input_tokens"), 20);
    assert_eq!(otlp_integer(&aggregate, "gen_ai.usage.output_tokens"), 6);
    assert_eq!(
        otlp_integer(&aggregate, "gen_ai.usage.cache_read.input_tokens"),
        4
    );
    let text = scrape(&gateway.state).await;
    for (name, expected) in [
        ("aisix_llm_input_tokens_total", 20.0),
        ("aisix_llm_output_tokens_total", 6.0),
        ("aisix_llm_total_tokens_total", 26.0),
        ("aisix_llm_cached_input_tokens_total", 4.0),
    ] {
        assert_eq!(
            metric(&text, name),
            expected,
            "actual unique response quantities reach metrics exactly once"
        );
    }
    gateway.assert_billing_sink_empty().await;
    paid(&f, 2).await;
    let events: Vec<Value> = sqlx::query_scalar("SELECT event FROM native_live_outbox")
        .fetch_all(f.cp.db().pool())
        .await
        .expect("read actual canonical paid-source events without logging bodies");
    assert_eq!(events.len(), 2);
    for event in &events {
        assert!(
            event.get("api_key_id") == Some(&json!(f.key)),
            "the actual gateway journals the authenticated owner"
        );
        assert_eq!(event.get("prompt_tokens"), Some(&json!(10)));
        assert_eq!(event.get("cached_prompt_tokens"), Some(&json!(2)));
        assert_eq!(event.get("completion_tokens"), Some(&json!(3)));
        assert_eq!(event.get("total_tokens"), Some(&json!(13)));
    }
    let replay = support::world::deliver(&f.cp.dp_client(&f.bundle), Uuid::new_v4(), events).await;
    assert_eq!(replay.status, 200);
    assert_eq!(replay.body.get("rejected"), Some(&json!(0)));
    paid(&f, 2).await;
    let sessions: Vec<(String, String)> =
        sqlx::query_as("SELECT coverage,accounting_state FROM native_live_sessions")
            .fetch_all(f.cp.db().pool())
            .await
            .expect("read the real session accounting coverage");
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].0 == "unknown" && sessions[0].1 == "awaiting_reconciliation", "ordinary transport close without a defined complete tail stays unknown, never complete zero");
}

const ASR: &str = "gpt-4o-mini-transcribe";

fn asr_done(item: &str, delivery: &str) -> Value {
    json!({"type":"conversation.item.input_audio_transcription.completed","event_id":delivery,
        "item_id":item,"content_index":0,"transcript":"component transcription",
        "usage":{"type":"tokens","input_tokens":5,"output_tokens":1,"total_tokens":6,
            "input_token_details":{"audio_tokens":5,"text_tokens":0}}})
}

async fn paid_asr(f: &Fixture, asr_model: Uuid, transcriptions: usize) {
    let total = 144 + transcriptions as i64 * 700;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rows: Vec<(String,i64,String)> = sqlx::query_as("SELECT c.requested_model_id,c.charge_micro_cny,u.event->>'operation' \
                FROM pricing_charges_cny c JOIN usage_events u ON u.event_id=c.event_id WHERE c.tenant_id=$1::text AND c.status='charged'")
                .bind(f.tenant).fetch_all(f.cp.db().pool()).await.expect("read actual separately priced main and ASR facts");
            assert!(rows.len() <= transcriptions+1, "duplicate observations cannot create additional charged facts");
            if rows.len() == transcriptions+1 {
                assert_eq!(rows.iter().filter(|row| row.0 == f.model.to_string() && row.1 == 144 && row.2 == "realtime").count(),1,"the main response keeps its own price and operation");
                assert_eq!(rows.iter().filter(|row| row.0 == asr_model.to_string() && row.1 == 700 && row.2 == "transcription").count(),transcriptions,"5 ASR audio input*100 + 1 text output*200 = 700 micro-CNY per accepted item/content");
                let debits: (i64,i64) = sqlx::query_as("SELECT count(*),COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'")
                    .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("read every tenant debit");
                assert_eq!(debits,((transcriptions+1) as i64,total));
                let balance: i64 = sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
                    .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("read actual balance after separately priced ASR");
                assert_eq!(balance,1_000_000-total);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("late accepted ASR work is really charged at its own nonzero model price");
}

async fn committed_audio(
    f: &Fixture,
    socket: &mut Socket,
    item: &str,
    previous: Option<&str>,
    expected: &mut Vec<&'static str>,
) {
    // 24kHz PCM16 单声道 100ms；只作为本地协议输入，不请求真实语音处理。
    let audio = base64::engine::general_purpose::STANDARD.encode([0_u8; 4800]);
    socket.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":format!("append_{item}"),"audio":audio}).to_string())).await.expect("append actual audio input through the gateway");
    expected.push("input_audio_buffer.append");
    f.provider.received(expected).await;
    socket
        .send(Message::Text(
            json!({"type":"input_audio_buffer.commit","event_id":format!("commit_{item}")})
                .to_string(),
        ))
        .await
        .expect("commit accepted audio before revocation");
    expected.push("input_audio_buffer.commit");
    f.provider.received(expected).await;
    f.provider.send(json!({"type":"input_audio_buffer.committed","event_id":format!("committed_{item}"),"item_id":item,"previous_item_id":previous}));
    let event = next_event(socket).await;
    assert!(
        event.get("type") == Some(&json!("input_audio_buffer.committed"))
            && event.get("item_id") == Some(&json!(item)),
        "the actual provider accepted this distinct input item"
    );
    f.provider.send(json!({"type":"conversation.item.created","event_id":format!("created_{item}"),"previous_item_id":previous,
        "item":{"id":item,"object":"realtime.item","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}));
    let event = next_event(socket).await;
    assert!(
        event.get("type") == Some(&json!("conversation.item.created"))
            && event.pointer("/item/id") == Some(&json!(item)),
        "the committed item exists before its delayed transcription result"
    );
}

// 原 strict 即时 CP 撤权合同及 368 RED 已逐字归档；本轻替代先保留已接受尾量，再验证本地配置同步后的拒绝。
#[tokio::test]
async fn accepted_asr_items_keep_separate_model_prices_and_deny_new_audio_after_role_projection_sync(
) {
    let Some(f) = fixture().await else { return };
    let account: Uuid = sqlx::query_scalar(
        "SELECT (doc->>'provider_key_id')::uuid FROM gw_resources WHERE kind='models' AND id=$1",
    )
    .bind(f.model)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("read the actual configured account reference");
    let asr = admin_post(
        &f.cp,
        &f.admin,
        "/admin/resources/models",
        json!({"doc":{
            "display_name":ASR,"provider":"openai","model_name":ASR,"provider_key_id":account
        }}),
    )
    .await;
    let asr_model = id(&asr, "/id");
    let path = format!("/admin/tenants/{}", f.tenant);
    let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    let granted =
        f.cp.send_conditional(
            "PATCH",
            &path,
            f.admin.cookie(),
            current.body["version"]
                .as_i64()
                .expect("actual tenant version"),
            json!({"allowed_model_ids":[f.model,asr_model]}),
        )
        .await;
    assert_eq!(
        granted.status, 200,
        "the secondary ASR model is explicitly granted before input acceptance"
    );
    admin_post(&f.cp,&f.admin,"/admin/pricing/baselines",json!({"model_id":asr_model,"effective_from":support::world::db_now(&f.cp).await,
        "note":"ASR 独立且显式音频维度单价","card":{"default":{"base":{"input":"100","audio_input":"100","output":"200","audio_output":"200","cache_read":"20"}}}})).await;
    let owner = f.cp.member_login_seeded(f.tenant, MemberRole::Owner).await;
    let mut gateway = Gateway::start(&f).await;
    let mut socket = gateway.connect(&f.bearer).await;
    let initial = next_event(&mut socket).await;
    assert!(initial.get("type") == Some(&json!("session.created")));
    assert!(
        f.provider
            .state
            .handshakes
            .lock()
            .expect("provider handshake log")
            .as_slice()
            == [true],
        "the ASR scenario starts on the configured authenticated provider/project/model binding"
    );
    socket
        .send(Message::Text(
            json!({"type":"session.update","event_id":"configure_accepted_asr_b",
        "session":{"type":"realtime","audio":{"input":{"transcription":{"model":ASR}}}}})
            .to_string(),
        ))
        .await
        .expect("configure the explicitly granted secondary model through the gateway");
    let mut expected = vec!["session.update"];
    f.provider.received(&expected).await;
    let mut configured = initial
        .get("session")
        .expect("the provider session")
        .clone();
    configured["audio"] = json!({"input":{"transcription":{"model":ASR}}});
    f.provider
        .send(json!({"type":"session.updated","event_id":"asr_b_applied","session":configured}));
    let applied = next_event(&mut socket).await;
    assert!(
        applied.get("type") == Some(&json!("session.updated"))
            && applied.pointer("/session/audio/input/transcription/model") == Some(&json!(ASR)),
        "model B configuration was applied before accepting either input"
    );
    committed_audio(&f, &mut socket, "item_asr_first", None, &mut expected).await;
    committed_audio(
        &f,
        &mut socket,
        "item_asr_second",
        Some("item_asr_first"),
        &mut expected,
    )
    .await;
    socket.send(Message::Text(json!({"type":"response.create","event_id":"main_before_role_change","response":{"output_modalities":["text"]}}).to_string())).await.expect("request the independent main response");
    expected.push("response.create");
    f.provider.received(&expected).await;
    created(&f.provider, &mut socket, FIRST).await;
    f.provider
        .send(done(FIRST, "main_done_before_asr_tail", "completed", true));
    let event = next_event(&mut socket).await;
    assert!(event.get("type") == Some(&json!("response.done")));
    paid_asr(&f, asr_model, 0).await;
    let changed =
        f.cp.send(
            "PATCH",
            &format!("/api/members/{}", f.member),
            Some(owner.cookie()),
            Some(json!({"role":"billing"})),
        )
        .await;
    assert_eq!(
        changed.status, 200,
        "the actual tenant owner withdraws inference authority by changing the current role"
    );
    let role: String = sqlx::query_scalar("SELECT role FROM members WHERE id=$1")
        .bind(f.member)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("read the persisted current member role");
    assert_eq!(role, "billing");
    // 先有界放出已接受输入的倒序付费尾量，再读关闭；不在已关闭的客户端 socket 上构造无效发送。
    f.provider
        .send(asr_done("item_asr_second", "asr_second_late_after_role"));
    f.provider
        .send(asr_done("item_asr_first", "asr_first_later_after_role"));
    f.provider.send(asr_done(
        "item_asr_second",
        "asr_second_redelivery_new_event",
    ));
    paid_asr(&f, asr_model, 2).await;
    let current = projected_snapshot(&f.cp).await;
    let key = current
        .apikeys
        .get_by_id(&f.key.to_string())
        .expect("the actual revoked-role key remains represented");
    assert!(
        key.value.disabled,
        "the role change projects a closed key while the active gateway retains its older snapshot"
    );
    // 角色撤销后的倒序 B 尾量已真实入账 144+700+700；仅现在安装真实 CP→kine 投影。
    gateway.state.snapshot.store(current);
    assert!(
        gateway
            .state
            .snapshot
            .load()
            .apikeys
            .get_by_id(&f.key.to_string())
            .expect("the serving gateway retains the original projected key")
            .value
            .disabled,
        "the actual serving gateway has applied the revoked-role key before testing new input"
    );
    let before = f
        .provider
        .state
        .received
        .lock()
        .expect("provider positive input log")
        .clone();
    assert!(before.iter().map(String::as_str).eq(expected.iter().copied()),"the upstream actually read model configuration and both accepted inputs before the negative check");
    let attempt = socket.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":"forbidden_new_audio_after_role","audio":base64::engine::general_purpose::STANDARD.encode([0_u8;4800])}).to_string())).await;
    if attempt.is_ok() {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match socket.next().await {
                    Some(Ok(Message::Text(text))) => {
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
        .expect("current role revocation produces an actionable denial or transport close");
    }
    let _ = socket.close(None).await;
    drop(socket);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !f.provider.task.is_finished() { tokio::time::sleep(Duration::from_millis(20)).await; }
    }).await.expect("the actual upstream reader drains to transport close before trusting the no-new-input assertion");
    assert!(f.provider.state.received.lock().expect("provider negative input log").as_slice() == before.as_slice(),"new input never reaches the provider after the serving gateway applies the actual revoked-role projection");
    gateway.assert_billing_sink_empty().await;
    paid_asr(&f, asr_model, 2).await;
    let facts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM native_live_journal),(SELECT count(*) FROM native_live_outbox),(SELECT count(*) FROM usage_events)")
        .fetch_one(f.cp.db().pool())
        .await
        .expect("read canonical identities after duplicated late ASR");
    assert_eq!(facts,(3,3,3),"one main response and two distinct accepted ASR item/content facts, including content_index zero, survive delivery-ID changes without an extra canonical or intake event");
}

const ASR_ALIAS: &str = "private-asr-model-b";

async fn secondary_alias(f: &Fixture, grant: bool, tpm: Option<u64>) -> Uuid {
    let account: Uuid = sqlx::query_scalar(
        "SELECT (doc->>'provider_key_id')::uuid FROM gw_resources WHERE kind='models' AND id=$1",
    )
    .bind(f.model)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("read the actual primary account reference");
    let mut doc = json!({"display_name":ASR_ALIAS,"provider":"openai","model_name":ASR,"provider_key_id":account});
    if let Some(tpm) = tpm {
        doc["rate_limit"] = json!({"tpm":tpm});
    }
    let created = admin_post(
        &f.cp,
        &f.admin,
        "/admin/resources/models",
        json!({"doc":doc}),
    )
    .await;
    let secondary = id(&created, "/id");
    admin_post(&f.cp,&f.admin,"/admin/pricing/baselines",json!({"model_id":secondary,"effective_from":support::world::db_now(&f.cp).await,
        "note":"ASR 别名隔离的显式音频价格","card":{"default":{"base":{"input":"100","audio_input":"100","output":"200","audio_output":"200","cache_read":"20"}}}})).await;
    if grant {
        let path = format!("/admin/tenants/{}", f.tenant);
        let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
        assert_eq!(current.status, 200);
        let changed =
            f.cp.send_conditional(
                "PATCH",
                &path,
                f.admin.cookie(),
                current.body["version"].as_i64().expect("tenant version"),
                json!({"allowed_model_ids":[f.model,secondary]}),
            )
            .await;
        assert_eq!(
            changed.status, 200,
            "B has an explicit actual grant before configuration"
        );
    }
    secondary
}

async fn main_model_tpm(f: &Fixture, tpm: u64) {
    let path = format!("/admin/resources/models/{}", f.model);
    let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    let mut doc = current.body["doc"].clone();
    doc["rate_limit"] = json!({"tpm":tpm});
    let saved =
        f.cp.send_conditional(
            "PUT",
            &path,
            f.admin.cookie(),
            current.body["version"].as_i64().expect("model version"),
            json!({"doc":doc}),
        )
        .await;
    assert_eq!(
        saved.status, 200,
        "configure the primary model's independent real rate limit"
    );
}

async fn request_key_tpm(f: &Fixture, tpm: u64) -> Uuid {
    let saved=f.cp.send("POST","/admin/resources/rate_limit_policies",Some(f.admin.cookie()),Some(json!({"doc":{
        "name":"asr-request-scope-tpm","scope":"api_key","scope_ref":f.key,"window":"minute","max_tokens":tpm
    }}))).await;
    if saved.status != 201 {
        let code = match saved.body.get("code").and_then(Value::as_str) {
            Some("validation_failed") => "validation_failed",
            Some("gateway_too_old") => "gateway_too_old",
            Some("unavailable") => "unavailable",
            _ => "other",
        };
        eprintln!(
            "ASR request-scope fixture failed: status={} code={code}",
            saved.status
        );
        for detail in saved
            .body
            .get("details")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let path = match detail.get("path").and_then(Value::as_str) {
                Some("/scope") => "/scope",
                Some("/scope_ref") => "/scope_ref",
                Some("/window") => "/window",
                Some("/max_tokens") => "/max_tokens",
                Some("/name") => "/name",
                _ => "#",
            };
            eprintln!("ASR request-scope fixture validation path={path}");
        }
    }
    assert_eq!(
        saved.status, 201,
        "a real classic api-key token policy is accepted through the supported admin API"
    );
    let policy = id(&saved.body, "/id");
    let current =
        f.cp.send(
            "GET",
            &format!("/admin/resources/rate_limit_policies/{policy}"),
            Some(f.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(current.status, 200);
    let snapshot = projected_snapshot(&f.cp).await;
    let projected = snapshot
        .rate_limit_policies
        .get_by_id(&policy.to_string())
        .expect("the exact real policy ID reaches the actual gateway snapshot");
    let projected = serde_json::to_value(projected.value.as_ref())
        .expect("serialize public projected policy fields");
    for document in [&current.body["doc"], &projected] {
        assert_eq!(document.get("scope"), Some(&json!("api_key")));
        assert!(
            document.get("scope_ref") == Some(&json!(f.key)),
            "the policy targets the actual managed tenant key"
        );
        assert_eq!(document.get("window"), Some(&json!("minute")));
        assert_eq!(document.get("max_tokens"), Some(&json!(tpm)));
        assert!(
            document.get("conditions").is_none() && document.get("limits").is_none(),
            "classic and conditional policy forms stay disjoint"
        );
    }
    policy
}

async fn await_denial(socket: &mut Socket) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let event: Value = serde_json::from_str(&text).expect("a gateway event");
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
    .expect("the gateway gives an actionable secondary-model denial or closes");
}

async fn drain_provider(f: &Fixture, mut socket: Socket) {
    let _ = socket.close(None).await;
    drop(socket);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !f.provider.task.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the actual provider reader drains before a no-dispatch assertion");
}

async fn main_response(f: &Fixture, socket: &mut Socket, expected: &mut Vec<&'static str>) {
    socket.send(Message::Text(json!({"type":"response.create","event_id":"main_for_alias_isolation","response":{"output_modalities":["text"]}}).to_string())).await.expect("send a positive primary-model request");
    expected.push("response.create");
    f.provider.received(expected).await;
    created(&f.provider, socket, FIRST).await;
    f.provider
        .send(done(FIRST, "main_alias_isolation_done", "completed", true));
    let response = next_event(socket).await;
    assert!(
        response.get("type") == Some(&json!("response.done")),
        "the independent primary-model path really worked"
    );
}

// A 可用不等于 B 可配置；先有真实主模型流量，再验证未授予的独立别名在供应商前被拒。
#[tokio::test]
async fn secondary_asr_alias_without_its_own_grant_is_denied_before_provider_configuration() {
    let Some(f) = fixture().await else { return };
    let secondary = secondary_alias(&f, false, None).await;
    let grants: Vec<Uuid> = sqlx::query_scalar("SELECT allowed_model_ids FROM tenants WHERE id=$1")
        .bind(f.tenant)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("read actual current grants");
    assert!(
        grants.contains(&f.model) && !grants.contains(&secondary),
        "only primary A is currently granted"
    );
    let mut gateway = Gateway::start(&f).await;
    let mut socket = gateway.connect(&f.bearer).await;
    let session = next_event(&mut socket).await;
    assert!(session.get("type") == Some(&json!("session.created")));
    assert!(
        f.provider
            .state
            .handshakes
            .lock()
            .expect("handshake log")
            .as_slice()
            == [true]
    );
    let mut expected = Vec::new();
    main_response(&f, &mut socket, &mut expected).await;
    paid(&f, 1).await;
    socket
        .send(Message::Text(
            json!({"type":"session.update","event_id":"forbidden_secondary_alias_configuration",
        "session":{"type":"realtime","audio":{"input":{"transcription":{"model":ASR_ALIAS}}}}})
            .to_string(),
        ))
        .await
        .expect("the live positive transport accepts the attempted secondary configuration frame");
    await_denial(&mut socket).await;
    drain_provider(&f, socket).await;
    let observed = f
        .provider
        .state
        .received
        .lock()
        .expect("drained provider input log");
    assert!(
        observed
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied()),
        "an ungranted B configuration never reaches the provider despite A being granted"
    );
    drop(observed);
    assert!(
        f.provider
            .state
            .secondary_models
            .lock()
            .expect("secondary model log")
            .is_empty(),
        "no alias or actual secondary model is dispatched without the B grant"
    );
    gateway.assert_billing_sink_empty().await;
    paid(&f, 1).await;
}

async fn align_real_minute() {
    loop {
        let second = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("actual Unix time")
            .as_secs()
            % 60;
        if second < 40 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(61 - second)).await;
    }
}

async fn assert_independent_tpm(gateway: &Gateway, key: Uuid, policy: Uuid) {
    let main = aisix_core::RateLimit {
        tpm: Some(100_000),
        ..Default::default()
    };
    let secondary = aisix_core::RateLimit {
        tpm: Some(5),
        ..Default::default()
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let a = gateway
                .state
                .limiter
                .peek(&format!("model:{MODEL}"), &main)
                .await
                .map(|status| status.tpm_used);
            let b = gateway
                .state
                .limiter
                .peek(&format!("model:{ASR_ALIAS}"), &secondary)
                .await
                .map(|status| status.tpm_used);
            let request = gateway.state.limiter.peek(&format!("policy:api_key:{key}:{policy}"),&main).await.map(|status|status.tpm_used);
            assert!(
                a.is_none_or(|value| value <= 13) && b.is_none_or(|value| value <= 6) && request.is_none_or(|value|value <=19),
                "request scope receives A+B exactly once while model scopes remain independent: a={a:?} b={b:?} request={request:?}"
            );
            if a == Some(13) && b == Some(6) && request == Some(19) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual TPM buckets contain A=13, B=6 and request-level A+B=19 reported tokens");
}

fn model_metric(text: &str, family: &str, model: &str) -> Option<f64> {
    let values: Vec<f64> = text
        .lines()
        .filter(|line| {
            line.starts_with(&format!("{family}{{"))
                && exact_metric_label(line, "endpoint", "/v1/realtime")
                && exact_metric_label(line, "inbound_protocol", "realtime")
                && exact_metric_label(line, "model", model)
        })
        .map(|line| {
            line.split_whitespace()
                .nth(1)
                .expect("metric value")
                .parse()
                .expect("numeric metric")
        })
        .collect();
    (!values.is_empty()).then(|| values.iter().sum())
}

fn exact_metric_label(line: &str, name: &str, value: &str) -> bool {
    let Some((_, labels)) = line.split_once('{') else {
        return false;
    };
    let Some((labels, _)) = labels.split_once('}') else {
        return false;
    };
    labels
        .split(',')
        .any(|label| label == format!("{name}=\"{value}\""))
}

async fn assert_per_model_metrics(gateway: &Gateway) {
    let expected = [
        ("aisix_llm_input_tokens_total", MODEL, 10.0),
        ("aisix_llm_output_tokens_total", MODEL, 3.0),
        ("aisix_llm_total_tokens_total", MODEL, 13.0),
        ("aisix_llm_cached_input_tokens_total", MODEL, 2.0),
        ("aisix_llm_input_tokens_total", ASR_ALIAS, 5.0),
        ("aisix_llm_output_tokens_total", ASR_ALIAS, 1.0),
        ("aisix_llm_total_tokens_total", ASR_ALIAS, 6.0),
    ];
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let text = scrape(&gateway.state).await;
            let mut complete = true;
            for (family,model,wanted) in expected {
                let value = model_metric(&text,family,model);
                assert!(value.is_none_or(|value|value <=wanted),"actual per-model metric quantities cannot attribute B to A or double-count: family={family} model={model} value={value:?}");
                complete &= value == Some(wanted);
            }
            if complete { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("actual GET metrics exposes distinct nonzero A and B series under their public aliases");
}

async fn assert_asr_detail_presence(f: &Fixture, secondary: Uuid) {
    let details: Vec<(Value,Value)> = sqlx::query_as("SELECT o.event,j.raw_usage FROM native_live_outbox o JOIN native_live_journal j ON j.id=o.journal_id WHERE o.event->>'requested_model_id'=$1")
        .bind(secondary.to_string()).fetch_all(f.cp.db().pool()).await.expect("read bounded canonical ASR metadata without displaying raw usage");
    assert_eq!(
        details.len(),
        1,
        "one canonical ASR fact preserves its reported detail presence"
    );
    let (event, raw) = &details[0];
    assert_eq!(
        event.get("prompt_audio_tokens"),
        Some(&json!(5)),
        "canonical telemetry carries explicitly reported ASR audio tokens"
    );
    assert_eq!(
        raw.pointer("/input_token_details/audio_tokens"),
        Some(&json!(5))
    );
    assert_eq!(raw.pointer("/input_token_details/text_tokens"),Some(&json!(0)),"the reported text zero remains explicit in the canonical raw usage without inventing a billing field");
    for field in [
        "prompt_image_tokens",
        "completion_audio_tokens",
        "completion_image_tokens",
    ] {
        assert!(
            event.get(field).is_none_or(Value::is_null),
            "an unreported optional token detail stays absent or null rather than fabricated zero"
        );
    }
}

// B 的公开别名不等于上游名；配置、账单、指标和 TPM 都必须维持这条独立模型边界。
#[tokio::test]
async fn granted_asr_alias_uses_upstream_name_and_separate_paid_metrics_and_model_tpm() {
    let Some(f) = fixture().await else { return };
    let secondary = secondary_alias(&f, true, Some(5)).await;
    main_model_tpm(&f, 100_000).await;
    let request_policy = request_key_tpm(&f, 100_000).await;
    let mut gateway = Gateway::start(&f).await;
    let mut socket = gateway.connect(&f.bearer).await;
    let initial = next_event(&mut socket).await;
    assert!(initial.get("type") == Some(&json!("session.created")));
    assert!(
        f.provider
            .state
            .handshakes
            .lock()
            .expect("handshake log")
            .as_slice()
            == [true]
    );
    socket
        .send(Message::Text(
            json!({"type":"session.update","event_id":"configure_distinct_secondary_alias",
        "session":{"type":"realtime","audio":{"input":{"transcription":{"model":ASR_ALIAS}}}}})
            .to_string(),
        ))
        .await
        .expect("request the granted secondary alias over the actual gateway");
    let mut expected = vec!["session.update"];
    f.provider.received(&expected).await;
    assert!(
        f.provider
            .state
            .secondary_models
            .lock()
            .expect("actual provider secondary model log")
            .as_slice()
            == [true],
        "the provider receives its actual ASR model name rather than the private request alias"
    );
    let mut applied = initial
        .get("session")
        .expect("actual provider session")
        .clone();
    applied["audio"] = json!({"input":{"transcription":{"model":ASR}}});
    f.provider.send(json!({"type":"session.updated","event_id":"actual_secondary_model_applied","session":applied}));
    assert!(next_event(&mut socket).await.get("type") == Some(&json!("session.updated")));
    committed_audio(&f, &mut socket, "item_alias_secondary", None, &mut expected).await;
    // 使用真实分钟窗口并在有至少 20 秒余量时释放两份计量；不替换限流时钟。
    align_real_minute().await;
    main_response(&f, &mut socket, &mut expected).await;
    f.provider.send(asr_done(
        "item_alias_secondary",
        "secondary_alias_paid_fact",
    ));
    let completed = next_event(&mut socket).await;
    assert!(
        completed.get("type")
            == Some(&json!(
                "conversation.item.input_audio_transcription.completed"
            ))
            && completed.get("content_index") == Some(&json!(0)),
        "a real accepted item/content-zero ASR fact was observed"
    );
    assert_eq!(
        completed.pointer("/usage/input_token_details/audio_tokens"),
        Some(&json!(5)),
        "reported ASR audio detail remains explicit"
    );
    assert_eq!(
        completed.pointer("/usage/input_token_details/text_tokens"),
        Some(&json!(0)),
        "reported ASR text zero remains present rather than becoming absent"
    );
    assert_independent_tpm(&gateway, f.key, request_policy).await;
    let before = f
        .provider
        .state
        .received
        .lock()
        .expect("positive provider event log")
        .clone();
    assert!(before.iter().map(String::as_str).eq(expected.iter().copied()),"the provider positively read the granted configuration and accepted input before the limit denial");
    let attempt = socket.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":"over_secondary_model_tpm","audio":base64::engine::general_purpose::STANDARD.encode([0_u8;4800])}).to_string())).await;
    if attempt.is_ok() {
        await_denial(&mut socket).await;
    }
    drain_provider(&f, socket).await;
    assert!(
        f.provider
            .state
            .received
            .lock()
            .expect("drained provider limit log")
            .as_slice()
            == before.as_slice(),
        "B's own exhausted model token limit blocks further ASR audio before provider dispatch"
    );
    paid_asr(&f, secondary, 1).await;
    assert_asr_detail_presence(&f, secondary).await;
    assert_per_model_metrics(&gateway).await;
    gateway.assert_billing_sink_empty().await;
    paid_asr(&f, secondary, 1).await;
}

// 已同步的本地模型 ACL 撤 B 留 A：旧 accepted B 尾量保留，新 audio 必须在供应商前停止。
#[tokio::test]
async fn projected_secondary_grant_revocation_keeps_main_enabled_and_accepted_asr_tail_but_blocks_new_audio(
) {
    let Some(f) = fixture().await else { return };
    let secondary = secondary_alias(&f, true, None).await;
    let mut gateway = Gateway::start(&f).await;
    let initial_snapshot = gateway.state.snapshot.load();
    let initial_key = initial_snapshot
        .apikeys
        .get_by_id(&f.key.to_string())
        .expect("the actual initially granted key is in the serving snapshot");
    assert!(
        !initial_key.value.disabled
            && initial_key.value.can_access(&initial_snapshot, MODEL)
            && initial_key.value.can_access(&initial_snapshot, ASR_ALIAS),
        "the same enabled key initially has both real A and B grants"
    );
    let mut socket = gateway.connect(&f.bearer).await;
    let initial = next_event(&mut socket).await;
    assert!(initial.get("type") == Some(&json!("session.created")));
    assert!(
        f.provider
            .state
            .handshakes
            .lock()
            .expect("actual initial provider handshake log")
            .as_slice()
            == [true],
        "the original authenticated account/project/main-model provider actually upgraded"
    );
    let mut expected = Vec::new();
    committed_audio(&f, &mut socket, "item_main_before_b", None, &mut expected).await;
    main_response(&f, &mut socket, &mut expected).await;
    paid_asr(&f, secondary, 0).await;
    socket
        .send(Message::Text(
            json!({"type":"session.update","event_id":"configure_b_before_projected_revocation",
                "session":{"type":"realtime","audio":{"input":{"transcription":{"model":ASR_ALIAS}}}}})
            .to_string(),
        ))
        .await
        .expect("request the genuinely granted B alias on the positive A connection");
    expected.push("session.update");
    f.provider.received(&expected).await;
    assert!(
        f.provider
            .state
            .secondary_models
            .lock()
            .expect("actual B upstream model log")
            .as_slice()
            == [true],
        "the original provider actually receives ASR's upstream name rather than its local alias"
    );
    let mut configured = initial
        .get("session")
        .expect("actual original provider session")
        .clone();
    configured["audio"] = json!({"input":{"transcription":{"model":ASR}}});
    f.provider.send(json!({"type":"session.updated","event_id":"b_applied_before_revocation","session":configured}));
    let ack = next_event(&mut socket).await;
    assert!(
        ack.get("type") == Some(&json!("session.updated"))
            && ack.pointer("/session/audio/input/transcription/model") == Some(&json!(ASR)),
        "the real provider applies B before the input whose tail will survive revocation"
    );
    committed_audio(
        &f,
        &mut socket,
        "item_b_accepted_before_projection",
        Some("item_main_before_b"),
        &mut expected,
    )
    .await;
    let path = format!("/admin/tenants/{}", f.tenant);
    let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    let revoked =
        f.cp.send_conditional(
            "PATCH",
            &path,
            f.admin.cookie(),
            current.body["version"]
                .as_i64()
                .expect("actual tenant version"),
            json!({"allowed_model_ids":[f.model]}),
        )
        .await;
    assert_eq!(
        revoked.status, 200,
        "the actual public grant API revokes B while retaining A"
    );
    let actual_grants: Vec<Uuid> =
        sqlx::query_scalar("SELECT allowed_model_ids FROM tenants WHERE id=$1")
            .bind(f.tenant)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("read actual persisted grants without mutating authority");
    assert!(
        actual_grants.contains(&f.model) && !actual_grants.contains(&secondary),
        "the real CP retained A and withdrew only B"
    );
    let projected = projected_snapshot(&f.cp).await;
    let projected_key = projected
        .apikeys
        .get_by_id(&f.key.to_string())
        .expect("actual CP to kine projection retains the same creator key");
    assert!(
        !projected_key.value.disabled
            && projected_key.value.can_access(&projected, MODEL)
            && !projected_key.value.can_access(&projected, ASR_ALIAS),
        "the actual projected key remains enabled and allows A while denying B"
    );
    gateway.state.snapshot.store(projected);
    let applied = gateway.state.snapshot.load();
    let applied_key = applied
        .apikeys
        .get_by_id(&f.key.to_string())
        .expect("read the original key from the actual serving snapshot");
    assert!(
        !applied_key.value.disabled
            && applied_key.value.can_access(&applied, MODEL)
            && !applied_key.value.can_access(&applied, ASR_ALIAS),
        "before either tail or new work, the same serving gateway has applied B-only revocation and kept A callable"
    );
    // 新权限只约束新工作；已接受 item/content 的真实 B 数量不能被当前 grant 抹掉。
    f.provider.send(asr_done(
        "item_b_accepted_before_projection",
        "b_tail_after_actual_projection",
    ));
    let completed = next_event(&mut socket).await;
    assert!(
        completed.get("type")
            == Some(&json!(
                "conversation.item.input_audio_transcription.completed"
            ))
            && completed.get("item_id") == Some(&json!("item_b_accepted_before_projection"))
            && completed.get("content_index") == Some(&json!(0)),
        "the accepted B item/content-zero tail is actually relayed after local B revocation"
    );
    assert_eq!(
        completed.pointer("/usage/input_token_details/audio_tokens"),
        Some(&json!(5))
    );
    assert_eq!(
        completed.pointer("/usage/input_token_details/text_tokens"),
        Some(&json!(0))
    );
    assert_eq!(completed.pointer("/usage/total_tokens"), Some(&json!(6)));
    f.provider.send(asr_done(
        "item_b_accepted_before_projection",
        "same_accepted_b_changed_delivery_after_projection",
    ));
    f.provider.send(json!({"type":"rate_limits.updated","event_id":"b_duplicate_processed_before_new_audio","rate_limits":[]}));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = next_event(&mut socket).await;
            if event.get("type") == Some(&json!("rate_limits.updated"))
                && event.get("event_id") == Some(&json!("b_duplicate_processed_before_new_audio"))
            {
                break;
            }
            assert!(
                event.get("type")
                    == Some(&json!(
                        "conversation.item.input_audio_transcription.completed"
                    ))
                    && event.get("item_id") == Some(&json!("item_b_accepted_before_projection")),
                "only the known duplicate can precede the actual same-WS processing marker"
            );
        }
    })
    .await
    .expect("the actual provider duplicate precedes the received marker; no queued fake receipt");
    paid_asr(&f, secondary, 1).await;
    let before = f
        .provider
        .state
        .received
        .lock()
        .expect("complete positive provider input history")
        .clone();
    assert!(
        before.iter().map(String::as_str).eq(expected.iter().copied()),
        "the provider positively read main PCM/response and the granted B configuration/input before the refusal"
    );
    let attempt = socket
        .send(Message::Text(
            json!({"type":"input_audio_buffer.append",
        "event_id":"new_b_audio_after_applied_grant_revocation",
        "audio":base64::engine::general_purpose::STANDARD.encode([0_u8;4800])})
            .to_string(),
        ))
        .await;
    if attempt.is_ok() {
        await_denial(&mut socket).await;
    }
    drain_provider(&f, socket).await;
    assert!(
        f.provider
            .state
            .received
            .lock()
            .expect("fully drained provider input history")
            .as_slice()
            == before.as_slice(),
        "new audio or commit cannot reach active B after actual local revocation, while A's key remains enabled"
    );
    paid_asr(&f, secondary, 1).await;
    assert_asr_detail_presence(&f, secondary).await;
    assert_per_model_metrics(&gateway).await;
    gateway.assert_billing_sink_empty().await;
    let facts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM native_live_journal),(SELECT count(*) FROM native_live_outbox),(SELECT count(*) FROM usage_events)")
        .fetch_one(f.cp.db().pool()).await.expect("read all actual facts after the processed duplicate and refused new work");
    assert_eq!(
        facts,
        (2, 2, 2),
        "one A144 and one accepted B700 survive revocation and changed delivery ID exactly once"
    );
}
