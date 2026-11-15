//! 动态头普通 WS 的基础 ASR 授权、别名、独立计量与普通遥测出口合同。
//! CP/PG/kine/mTLS、网关、指标和定价均为真实组件；供应商仅是本地受控 WS peer。
//! 不声称真实供应商、跨重启 journal、强身份或逐音频包 CP 权限一致性覆盖。

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

const A: &str = "forwarded-realtime-main";
const B: &str = "forwarded-independent-asr";
const ASR: &str = "gpt-4o-mini-transcribe";
const PROJECT: &str = "proj_forwarded_asr_component";
const CONTEXT: &str = "forwarded-asr-public-context";
const SESSION: &str = "sess_forwarded_asr_component";
const RESPONSE: &str = "resp_forwarded_asr_main";
const LIMIT: Duration = Duration::from_secs(15);
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct ProviderState {
    secret: String,
    output: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    inputs: Mutex<Vec<Value>>,
    handshakes: Mutex<Vec<[bool; 5]>>,
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
            .expect("绑定本地供应商 peer");
        let base = format!("http://{}", listener.local_addr().expect("peer 地址"));
        let state = Arc::new(ProviderState {
            secret: format!("sk-forwarded-component-{}", Uuid::new_v4().simple()),
            output: Mutex::new(None),
            inputs: Mutex::new(Vec::new()),
            handshakes: Mutex::new(Vec::new()),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("接受真实 provider TCP");
            let observed = shared.clone();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response| {
                    let header = |name: &str| {
                        request
                            .headers()
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                    };
                    let checks = [
                        header("authorization")
                            == Some(format!("Bearer {}", observed.secret).as_str()),
                        header("openai-project") == Some(PROJECT),
                        request.uri().path() == "/v1/realtime",
                        request.uri().query().is_some_and(|query| {
                            query.split('&').any(|part| part == "model=gpt-realtime")
                        }),
                        header("x-request-context") == Some(CONTEXT),
                    ];
                    observed.handshakes.lock().expect("握手日志").push(checks);
                    Ok(response)
                },
            )
            .await
            .expect("真实 provider WS 升级");
            let (sender, mut output) = mpsc::unbounded_channel();
            *shared.output.lock().expect("peer 输出槽") = Some(sender);
            socket.send(Message::Text(json!({
                "type":"session.created","event_id":"forwarded_session_created",
                "session":{"id":SESSION,"object":"realtime.session","type":"realtime","model":"gpt-realtime",
                    "output_modalities":["text"],"tools":[],"tool_choice":"auto","max_output_tokens":"inf",
                    "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,"turn_detection":null}},
                    "expires_at":chrono::Utc::now().timestamp()+3600}
            }).to_string())).await.expect("发送真实初始 session");
            loop {
                tokio::select! {
                    outgoing = output.recv() => {
                        let Some(message) = outgoing else { break };
                        if socket.send(message).await.is_err() { break; }
                    }
                    incoming = socket.next() => {
                        match incoming {
                            Some(Ok(Message::Text(text))) => {
                                let event: Value = serde_json::from_str(&text).expect("provider 收到 JSON");
                                assert!(event.get("type").and_then(Value::as_str).is_some(), "provider 实际读取带类型的事件");
                                shared.inputs.lock().expect("完整输入历史").push(event);
                            }
                            Some(Ok(Message::Ping(bytes))) => {
                                if socket.send(Message::Pong(bytes)).await.is_err() { break; }
                            }
                            Some(Ok(Message::Close(frame))) => {
                                let _ = socket.send(Message::Close(frame)).await;
                                break;
                            }
                            None | Some(Err(_)) => break,
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
            .expect("peer 输出槽")
            .as_ref()
            .expect("实际握手已完成")
            .send(Message::Text(event.to_string()))
            .expect("发送受控官方事件");
    }

    fn history(&self) -> Vec<Value> {
        self.state.inputs.lock().expect("完整输入历史").clone()
    }

    async fn received(&self, expected: &[Value]) {
        tokio::time::timeout(LIMIT, async {
            loop {
                let history = self.history();
                assert!(
                    history.len() <= expected.len(),
                    "provider 不接收额外配置或音频"
                );
                assert!(
                    history
                        .iter()
                        .zip(expected)
                        .all(|(got, wanted)| got == wanted),
                    "provider 完整事件字段和 PCM 字节与实际授权输入一致"
                );
                if history.len() == expected.len() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("真实 provider reader 读到全部正向输入");
    }

    fn assert_handshake(&self) {
        assert!(
            self.state.handshakes.lock().expect("握手日志").as_slice() == [[true; 5]],
            "唯一实际 provider 握手保持原凭据、project、路径、主模型及转发的 benign header"
        );
    }

    async fn drained(&mut self) {
        tokio::time::timeout(LIMIT, &mut self.task)
            .await
            .expect("最终无新增输入断言前真实 provider reader 已停止")
            .expect("provider reader 真实退出，不能因 panic/cancel 假停止");
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
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
            .expect("绑定真实 OTLP HTTP collector");
        let base = format!("http://{}", listener.local_addr().expect("collector 地址"));
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .fallback(collect_spans)
            .with_state(payloads.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("本地 OTLP HTTP 服务");
        });
        Self {
            base,
            payloads,
            task,
        }
    }

    fn summaries(&self, input: i64, output: i64) -> Vec<BTreeMap<String, Value>> {
        let mut found = Vec::new();
        for payload in self.payloads.lock().expect("OTLP 完整历史").iter() {
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
                        let attributes: BTreeMap<String, Value> = span
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
                        if otlp_integer(&attributes, "gen_ai.usage.input_tokens") == Some(input)
                            && otlp_integer(&attributes, "gen_ai.usage.output_tokens")
                                == Some(output)
                        {
                            found.push(attributes);
                        }
                    }
                }
            }
        }
        found
    }

    async fn summary_once(&self, with_b: bool) {
        let (input, output) = if with_b { (20, 5) } else { (10, 3) };
        tokio::time::timeout(LIMIT, async {
            loop {
                let summaries = self.summaries(input, output);
                assert!(
                    summaries.len() <= 1,
                    "whole-session summary 不因按模型拆分而重复导出"
                );
                if let Some(summary) = summaries.first() {
                    assert_eq!(
                        otlp_integer(summary, "gen_ai.usage.cache_read.input_tokens"),
                        Some(2)
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("真实 HTTP OTLP 收到 A 或 A+B 的完整 session summary");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            self.summaries(input, output).len(),
            1,
            "完整 collector 历史没有第二份同一 session summary"
        );
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn collect_spans(
    State(payloads): State<Arc<Mutex<Vec<Value>>>>,
    request: Request<Body>,
) -> StatusCode {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.uri().path(), "/v1/traces");
    let bytes = to_bytes(request.into_body(), 4 << 20)
        .await
        .expect("有界 OTLP payload");
    payloads
        .lock()
        .expect("OTLP 完整历史")
        .push(serde_json::from_slice(&bytes).expect("真实 OTLP JSON"));
    StatusCode::OK
}

fn otlp_integer(attributes: &BTreeMap<String, Value>, field: &str) -> Option<i64> {
    let value = attributes.get(field)?.get("intValue")?;
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

struct Fixture {
    cp: TestCp,
    provider: Provider,
    collector: Collector,
    bundle: Bundle,
    tenant: Uuid,
    key: Uuid,
    member: Uuid,
    account: Uuid,
    a: Uuid,
    b: Uuid,
    policy: Uuid,
    bearer: String,
}

fn id(body: &Value, pointer: &str) -> Uuid {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|text| text.parse().ok())
        .expect("实际 public resource UUID")
}

async fn admin_post(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let response = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(response.status, 201, "真实 Admin fixture 写入成功");
    response.body
}

async fn fixture(grant_b: bool) -> Option<Fixture> {
    let cp = TestCp::start_with_kine().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = Provider::start().await;
    let collector = Collector::start().await;
    let account = admin_post(
        &cp,
        &admin,
        "/admin/resources/provider_keys",
        json!({"doc":{
            "display_name":"forwarded-asr-original-account","provider":"openai","adapter":"openai",
            "api_base":provider.base,"api_key":provider.state.secret,"groups":["shared"],
            "request":{"default_headers":{"OpenAI-Project":PROJECT}}
        }}),
    )
    .await;
    let account = id(&account, "/id");
    // GET 隐去全部 credential slots；PUT 只增加转发头并明确补回原 secret/project，绝不换目的地。
    let path = format!("/admin/resources/provider_keys/{account}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    assert!(
        current.body.pointer("/doc/api_base") == Some(&json!(provider.base)),
        "原 configured endpoint 明确存在"
    );
    let mut doc = current.body["doc"].clone();
    doc["api_key"] = json!(provider.state.secret);
    doc["request"]["default_headers"] = json!({"OpenAI-Project":PROJECT});
    doc["request"]["forward_client_headers"] = json!(["x-request-context"]);
    let updated = cp
        .send_conditional(
            "PUT",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().expect("account version"),
            json!({"doc":doc}),
        )
        .await;
    assert_eq!(
        updated.status, 200,
        "合法 benign forwarding 配置成功且未更换原 account/provider"
    );
    let saved = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(saved.status, 200);
    assert!(saved.body.pointer("/doc/api_base") == Some(&json!(provider.base)));
    assert_eq!(
        saved.body.pointer("/doc/request/forward_client_headers"),
        Some(&json!(["x-request-context"]))
    );
    let mut models = Vec::new();
    for (alias, upstream) in [(A, "gpt-realtime"), (B, ASR)] {
        let created = admin_post(&cp,&admin,"/admin/resources/models",json!({"doc":{
            "display_name":alias,"provider":"openai","model_name":upstream,"provider_key_id":account,
            "rate_limit":{"tpm":100000}
        }})).await;
        models.push(id(&created, "/id"));
    }
    let (a, b) = (models[0], models[1]);
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let path = format!("/admin/tenants/{tenant}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    let grants = if grant_b { vec![a, b] } else { vec![a] };
    let granted = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().expect("tenant version"),
            json!({"allowed_model_ids":grants}),
        )
        .await;
    assert_eq!(granted.status, 200);
    let actual_grants: Vec<Uuid> =
        sqlx::query_scalar("SELECT allowed_model_ids FROM tenants WHERE id=$1")
            .bind(tenant)
            .fetch_one(cp.db().pool())
            .await
            .expect("只读实际 Admin grant 结果");
    assert!(
        actual_grants.contains(&a) && actual_grants.contains(&b) == grant_b,
        "两例的实际区别只有 B grant"
    );
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    let minted = admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":"forwarded-asr-key","member_id":member,"group_id":group}),
    )
    .await;
    let key = id(&minted, "/key/id");
    let bearer = minted["plaintext"]
        .as_str()
        .expect("实际签发的 APIkey")
        .to_owned();
    admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/credits"),
        json!({"amount_cny":"1","reason":"动态 WS 基础计量合同正控"}),
    )
    .await;
    admin_post(&cp,&admin,&format!("/admin/provider-groups/{group}/rates"),json!({"model_id":null,"cny_per_usd":"1","effective_from":support::world::db_now(&cp).await})).await;
    for (model, card) in [
        (a, json!({"input":"10","cache_read":"2","output":"20"})),
        (
            b,
            json!({"input":"100","audio_input":"100","output":"200","audio_output":"200","cache_read":"20"}),
        ),
    ] {
        admin_post(&cp,&admin,"/admin/pricing/baselines",json!({"model_id":model,"effective_from":support::world::db_now(&cp).await,"note":"按已有合法 token/audio 维度非零定价","card":{"default":{"base":card}}})).await;
    }
    let policy = admin_post(&cp,&admin,"/admin/resources/rate_limit_policies",json!({"doc":{
        "name":"forwarded-asr-request-tpm","scope":"api_key","scope_ref":key,"window":"minute","max_tokens":100000
    }})).await;
    let policy = id(&policy, "/id");
    admin_post(&cp,&admin,"/admin/resources/observability_exporters",json!({"doc":{
        "name":"forwarded-asr-otlp","enabled":true,"kind":"otlp_http","endpoint":format!("{}/v1/traces",collector.base),
        "headers":{},"content_mode":"metadata_only","sample_rate":1
    }})).await;
    let bundle = cp.issue_bundle(&admin, "forwarded-asr-real-gateway").await;
    Some(Fixture {
        cp,
        provider,
        collector,
        bundle,
        tenant,
        key,
        member,
        account,
        a,
        b,
        policy,
        bearer,
    })
}

struct Gateway {
    state: Option<ProxyState>,
    address: SocketAddr,
    billing: Option<mpsc::Receiver<UsageEvent>>,
    task: Option<tokio::task::JoinHandle<()>>,
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
        .expect("公开 gateway 配置");
        let snapshot = projected_snapshot(&f.cp).await;
        let projected = snapshot
            .provider_keys
            .get_by_id(&f.account.to_string())
            .expect("原 account 真实投射到 kine");
        assert!(
            projected.value.api_base.as_deref() == Some(f.provider.base.as_str())
                && projected.value.api_key == f.provider.state.secret,
            "snapshot 保留原 configured account/credential"
        );
        let request = projected
            .value
            .request
            .as_ref()
            .expect("真实投射的公开 request 配置");
        assert_eq!(request.forward_client_headers, ["x-request-context"]);
        assert!(
            request
                .default_headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("openai-project"))
                .map(|(_, value)| value.as_str())
                == Some(PROJECT)
        );
        let (sender, billing) = mpsc::channel(128);
        let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
            .without_cache()
            .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
                &f.cp.state().pki.platform_id_key_hex(),
            ))
            .with_native_objects(Arc::new(HttpRegistry::new(&f.cp, &f.bundle)))
            .with_usage_sink(UsageSink::new(sender));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("真实网关 listener");
        let address = listener.local_addr().expect("网关地址");
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.expect("接受真实 gateway HTTP");
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
            state: Some(state),
            address,
            billing: Some(billing),
            task: Some(task),
        }
    }

    fn state(&self) -> &ProxyState {
        self.state.as_ref().expect("owned gateway state")
    }

    async fn connect(&self, bearer: &str) -> Socket {
        let mut request = format!("ws://{}/v1/realtime?model={A}", self.address)
            .into_client_request()
            .expect("合法 ordinary WS URL");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {bearer}")
                .parse()
                .expect("实际 bearer header"),
        );
        request
            .headers_mut()
            .insert("x-request-context", CONTEXT.parse().expect("benign header"));
        let (socket, response) = tokio_tungstenite::connect_async(request)
            .await
            .unwrap_or_else(|_| panic!("真实 key 必须通过网关 WS upgrade；不显示响应体或凭据"));
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        socket
    }

    async fn finish(mut self, f: &Fixture, with_b: bool) -> Vec<UsageEvent> {
        let limiter = self.state().limiter.clone();
        // 仅停止 owned HTTP accept loop；已升级的 WS 已由真实 close/drain 收尾，不取消 relay/observer。
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        drop(self.state.take());
        let mut billing = self.billing.take().expect("真实普通 sink receiver");
        let events = tokio::time::timeout(LIMIT, async {
            let mut all = Vec::new();
            while let Some(event) = billing.recv().await {
                all.push(event);
            }
            all
        })
        .await
        .expect("释放全部 owned sender 后实际 sink 到 EOF；不以 quiet window 截断晚到计量");
        // limiter/metrics 的独立 Arc 不含 sender；EOF 后再次检查，迟到的重复计量也必须失败。
        let limit = aisix_core::RateLimit {
            tpm: Some(100_000),
            ..Default::default()
        };
        let a = limiter
            .peek(&format!("model:{A}"), &limit)
            .await
            .map(|row| row.tpm_used);
        let b = limiter
            .peek(&format!("model:{B}"), &limit)
            .await
            .map(|row| row.tpm_used);
        let request = limiter
            .peek(&format!("policy:api_key:{}:{}", f.key, f.policy), &limit)
            .await
            .map(|row| row.tpm_used);
        assert_eq!(a, Some(13));
        if with_b {
            assert_eq!(b, Some(12));
        } else {
            assert!(b.is_none_or(|value| value == 0));
        }
        assert_eq!(request, Some(if with_b { 25 } else { 13 }));
        events
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn next_event(socket: &mut Socket) -> Value {
    tokio::time::timeout(LIMIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    break serde_json::from_str(&text).expect("server event JSON")
                }
                Some(Ok(Message::Ping(bytes))) => {
                    socket.send(Message::Pong(bytes)).await.expect("真实 pong")
                }
                _ => panic!("正向 server event 到达之前不能关闭 WS"),
            }
        }
    })
    .await
    .expect("真实网关实际转发 server event")
}

async fn send_input(
    socket: &mut Socket,
    provider: &Provider,
    expected: &mut Vec<Value>,
    event: Value,
) {
    socket
        .send(Message::Text(event.to_string()))
        .await
        .expect("真实 client event 写入 WS");
    expected.push(event);
    provider.received(expected).await;
}

fn append(delivery: &str) -> Value {
    // 24kHz PCM16 单声道 100ms；只驱动本地组件，绝不请求真实语音服务。
    json!({"type":"input_audio_buffer.append","event_id":delivery,
        "audio":base64::engine::general_purpose::STANDARD.encode([3_u8;4800])})
}

async fn accepted_audio(
    f: &Fixture,
    socket: &mut Socket,
    expected: &mut Vec<Value>,
    item: &str,
    previous: Option<&str>,
) {
    send_input(
        socket,
        &f.provider,
        expected,
        append(&format!("append_{item}")),
    )
    .await;
    send_input(
        socket,
        &f.provider,
        expected,
        json!({"type":"input_audio_buffer.commit","event_id":format!("commit_{item}")}),
    )
    .await;
    f.provider.send(json!({"type":"input_audio_buffer.committed","event_id":format!("committed_{item}"),"item_id":item,"previous_item_id":previous}));
    let committed = next_event(socket).await;
    assert!(
        committed.get("type") == Some(&json!("input_audio_buffer.committed"))
            && committed.get("item_id") == Some(&json!(item)),
        "实际 provider 接受了精确输入 item"
    );
    f.provider.send(json!({"type":"conversation.item.created","event_id":format!("created_{item}"),"previous_item_id":previous,
        "item":{"id":item,"object":"realtime.item","type":"message","status":"completed","role":"user",
            "content":[{"type":"input_audio","transcript":null}]}}));
    let created = next_event(socket).await;
    assert!(
        created.get("type") == Some(&json!("conversation.item.created"))
            && created.pointer("/item/id") == Some(&json!(item))
            && created.pointer("/item/content/0/type") == Some(&json!("input_audio")),
        "content_index=0 是实际已接受的 audio part，不能拿最近 response 代替"
    );
}

fn main_done(delivery: &str) -> Value {
    json!({"type":"response.done","event_id":delivery,"response":{"object":"realtime.response","id":RESPONSE,
        "status":"completed","status_details":null,"output":[],"usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,
            "input_token_details":{"cached_tokens":2,"text_tokens":10},"output_token_details":{"text_tokens":3}}}})
}

async fn marker(provider: &Provider, socket: &mut Socket, delivery: &str) {
    provider.send(json!({"type":"rate_limits.updated","event_id":delivery,"rate_limits":[]}));
    tokio::time::timeout(LIMIT, async {
        loop {
            let event = next_event(socket).await;
            if event.get("type") == Some(&json!("rate_limits.updated"))
                && event.get("event_id") == Some(&json!(delivery))
            {
                break;
            }
            assert!(
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("response.done" | "conversation.item.input_audio_transcription.completed")
                ),
                "marker 之前只允许已知的重复 delivery；不要求重复必须转发或必须丢弃"
            );
        }
    })
    .await
    .expect("provider 同一真实 WS 的尾部 marker 穿过网关");
}

async fn primary_positive(f: &Fixture, gateway: &Gateway) -> (Socket, Value, Vec<Value>) {
    let mut socket = gateway.connect(&f.bearer).await;
    let initial = next_event(&mut socket).await;
    assert!(
        initial.get("type") == Some(&json!("session.created"))
            && initial.pointer("/session/id") == Some(&json!(SESSION))
            && initial.pointer("/session/model") == Some(&json!("gpt-realtime")),
        "真实原 provider session/main 模型正控成立"
    );
    f.provider.assert_handshake();
    let mut expected = Vec::new();
    accepted_audio(f, &mut socket, &mut expected, "item_primary_pcm", None).await;
    send_input(&mut socket,&f.provider,&mut expected,json!({"type":"response.create","event_id":"positive_main_create","response":{"output_modalities":["text"]}})).await;
    f.provider.send(json!({"type":"response.created","event_id":"main_response_created","response":{"id":RESPONSE,"object":"realtime.response","status":"in_progress","output":[]}}));
    let created = next_event(&mut socket).await;
    assert!(
        created.get("type") == Some(&json!("response.created"))
            && created.pointer("/response/id") == Some(&json!(RESPONSE))
    );
    f.provider.send(main_done("main_nonzero_first_delivery"));
    let done = next_event(&mut socket).await;
    assert!(
        done.get("type") == Some(&json!("response.done"))
            && done.pointer("/response/id") == Some(&json!(RESPONSE))
            && done.pointer("/response/usage/total_tokens") == Some(&json!(13)),
        "拒绝反例前必须真实完成非零 A response 和 PCM 输入"
    );
    f.provider.send(main_done("same_main_changed_delivery"));
    marker(
        &f.provider,
        &mut socket,
        "main_duplicate_processing_barrier",
    )
    .await;
    (socket, initial, expected)
}

async fn configure(
    f: &Fixture,
    socket: &mut Socket,
    initial: &Value,
    expected: &mut Vec<Value>,
    enabled: bool,
) {
    let requested = if enabled {
        json!({"model":B})
    } else {
        Value::Null
    };
    let upstream = if enabled {
        json!({"model":ASR})
    } else {
        Value::Null
    };
    let event = json!({"type":"session.update","event_id":if enabled {"enable_granted_alias_b"} else {"disable_after_accepting_b"},
        "session":{"type":"realtime","audio":{"input":{"transcription":requested}}}});
    socket
        .send(Message::Text(event.to_string()))
        .await
        .expect("发送合法 nested ASR 配置");
    let mut wanted = event;
    wanted["session"]["audio"]["input"]["transcription"] = upstream.clone();
    expected.push(wanted);
    f.provider.received(expected).await;
    let mut applied = initial["session"].clone();
    applied["audio"]["input"]["transcription"] = upstream.clone();
    f.provider.send(json!({"type":"session.updated","event_id":if enabled {"actual_b_applied"} else {"actual_b_disabled"},"session":applied}));
    let ack = next_event(socket).await;
    assert!(
        ack.get("type") == Some(&json!("session.updated"))
            && ack.pointer("/session/id") == Some(&json!(SESSION))
            && ack.pointer("/session/audio/input/transcription") == Some(&upstream),
        "真实 provider 已应用 rewrite 后的配置，之后才允许测试发送 B audio"
    );
}

fn asr_done(item: &str, delivery: &str) -> Value {
    // SDK 3dae: ASR usage 属于 item/content 独立模型，event_id 只是 delivery 身份。
    json!({"type":"conversation.item.input_audio_transcription.completed","event_id":delivery,"item_id":item,"content_index":0,
        "transcript":"controlled component transcription","usage":{"type":"tokens","input_tokens":5,"output_tokens":1,"total_tokens":6,
            "input_token_details":{"audio_tokens":5,"text_tokens":0}}})
}

async fn completed_asr(f: &Fixture, socket: &mut Socket, item: &str) {
    f.provider
        .send(asr_done(item, &format!("completed_{item}")));
    let event = next_event(socket).await;
    assert!(
        event.get("type")
            == Some(&json!(
                "conversation.item.input_audio_transcription.completed"
            ))
            && event.get("item_id") == Some(&json!(item))
            && event.get("content_index") == Some(&json!(0)),
        "逆序尾量仍属于实际已接受的精确 item/content-zero"
    );
    assert_eq!(
        event.pointer("/usage/input_token_details/audio_tokens"),
        Some(&json!(5))
    );
    assert_eq!(
        event.pointer("/usage/input_token_details/text_tokens"),
        Some(&json!(0)),
        "显式 text zero 必须存在"
    );
    assert_eq!(event.pointer("/usage/total_tokens"), Some(&json!(6)));
    f.provider
        .send(asr_done(item, &format!("changed_delivery_{item}")));
    marker(&f.provider, socket, &format!("processed_duplicate_{item}")).await;
}

async fn await_denial(socket: &mut Socket) {
    tokio::time::timeout(LIMIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let event: Value = serde_json::from_str(&text).expect("实际 error event JSON");
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
    .expect("真实 gateway 对未授予的 ASR 配置/新音频报错或停止");
}

async fn close_and_drain(f: &mut Fixture, mut socket: Socket) {
    let _ = socket.close(None).await;
    drop(socket);
    f.provider.drained().await;
    f.provider.assert_handshake();
}

async fn align_real_minute() {
    loop {
        let second = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("真实 Unix 时间")
            .as_secs()
            % 60;
        if second < 15 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(61 - second)).await;
    }
}

async fn independent_tpm(gateway: &Gateway, f: &Fixture, with_b: bool) {
    let limit = aisix_core::RateLimit {
        tpm: Some(100_000),
        ..Default::default()
    };
    let b_tokens = if with_b { 12 } else { 0 };
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let a = gateway.state().limiter.peek(&format!("model:{A}"),&limit).await.map(|status|status.tpm_used);
            let b = gateway.state().limiter.peek(&format!("model:{B}"),&limit).await.map(|status|status.tpm_used);
            let request = gateway.state().limiter.peek(&format!("policy:api_key:{}:{}",f.key,f.policy),&limit).await.map(|status|status.tpm_used);
            assert!(a.is_none_or(|value|value<=13) && b.is_none_or(|value|value<=b_tokens)
                && request.is_none_or(|value|value<=13+b_tokens),
                "A/B/request TPM 不得因重复 delivery 或 summary 再记一次: a={a:?} b={b:?} request={request:?}");
            if a == Some(13) && (if with_b {b == Some(12)} else {b.is_none_or(|value|value==0)})
                && request == Some(13+b_tokens) { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("真实 TPM A13/B12/request25 或仅 A13，各模型独立且 request 合计一次");
}

async fn scrape(metrics: &Arc<aisix_obs::metrics::Metrics>) -> String {
    let response = metrics_router(
        metrics.clone(),
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
            .expect("实际 metrics GET"),
    )
    .await
    .expect("生产 metrics router 回答");
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .expect("metrics HTTP body")
            .to_bytes()
            .to_vec(),
    )
    .expect("UTF8 metrics")
}

fn model_metric(text: &str, family: &str, model: &str) -> Option<f64> {
    let mut values = Vec::new();
    for line in text
        .lines()
        .filter(|line| line.starts_with(&format!("{family}{{")))
    {
        let Some((_, labels)) = line.split_once('{') else {
            continue;
        };
        let Some((labels, _)) = labels.split_once('}') else {
            continue;
        };
        let matches = |name: &str, value: &str| {
            labels
                .split(',')
                .any(|label| label == format!("{name}=\"{value}\""))
        };
        if matches("endpoint", "/v1/realtime")
            && matches("inbound_protocol", "realtime")
            && matches("model", model)
        {
            values.push(
                line.split_whitespace()
                    .nth(1)
                    .expect("metric value")
                    .parse::<f64>()
                    .expect("numeric metric"),
            );
        }
    }
    (!values.is_empty()).then(|| values.iter().sum())
}

async fn metrics_once(metrics: &Arc<aisix_obs::metrics::Metrics>, with_b: bool) {
    let mut expected = vec![
        ("aisix_llm_input_tokens_total", A, 10.0),
        ("aisix_llm_output_tokens_total", A, 3.0),
        ("aisix_llm_total_tokens_total", A, 13.0),
        ("aisix_llm_cached_input_tokens_total", A, 2.0),
    ];
    if with_b {
        expected.extend([
            ("aisix_llm_input_tokens_total", B, 10.0),
            ("aisix_llm_output_tokens_total", B, 2.0),
            ("aisix_llm_total_tokens_total", B, 12.0),
        ]);
    }
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let text = scrape(metrics).await;
            let mut complete = true;
            for &(family,model,wanted) in &expected {
                let actual = model_metric(&text,family,model);
                assert!(actual.is_none_or(|value|value<=wanted),"真实 metrics 不把 B 合入 A 或双计: family={family} model={model} actual={actual:?}");
                complete &= actual == Some(wanted);
            }
            if !with_b { assert!(model_metric(&text,"aisix_llm_total_tokens_total",B).is_none_or(|value|value==0.0)); }
            if complete { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("真实 GET metrics 必须有非零 A 和独立 B public alias series");
}

async fn bill_all_unchanged(f: &Fixture, events: Vec<UsageEvent>, with_b: bool) {
    assert!(
        !events.is_empty(),
        "实际普通 sink 非空；不能靠缺失 summary/children 假绿"
    );
    let mut wire = Vec::new();
    let mut a_positive = false;
    let mut b_positive = false;
    for event in events {
        assert!(
            event.api_key_id == f.key.to_string()
                && event.team_id == f.tenant.to_string()
                && event.user_id == f.member.to_string(),
            "全部实际 sink 行始终属于真实原 creator/key"
        );
        if event.prompt_tokens > 0 || event.completion_tokens > 0 {
            assert!(
                event.provider_key_id.as_deref() == Some(f.account.to_string().as_str())
                    && event.inbound_protocol == "realtime",
                "非零行保留原 provider 与真实 WS family"
            );
            if event.model_id == f.a.to_string() {
                assert!(
                    event.requested_model_id == f.a.to_string()
                        && event.requested_model == A
                        && event.upstream_model.as_deref() == Some("gpt-realtime"),
                    "A 原模型来源明确"
                );
                a_positive = true;
            } else {
                assert!(
                    with_b
                        && event.model_id == f.b.to_string()
                        && event.requested_model_id == f.b.to_string()
                        && event.requested_model == B
                        && event.upstream_model.as_deref() == Some(ASR),
                    "B 必须以实际 accepted ASR model 独立归属，不能猜成 mainA"
                );
                b_positive = true;
            }
        }
        // 所有真实事件原样交给正式 intake；不筛除 summary，不造 billable 字段，不重写 model/token。
        wire.push(serde_json::to_value(event).expect("真实 UsageEvent 公开序列化"));
    }
    assert!(
        a_positive && b_positive == with_b,
        "真实 sink 正向包含每个已发生的非零模型"
    );
    let delivered =
        support::world::deliver(&f.cp.dp_client(&f.bundle), Uuid::new_v4(), wire.clone()).await;
    assert_eq!(delivered.status, 200);
    assert_eq!(
        delivered.body.get("rejected"),
        Some(&json!(0)),
        "真实完整 sink 由 CP 接收，无筛选丢弃"
    );
    let wanted_b = if with_b { 1400 } else { 0 };
    let wanted_total = 144 + wanted_b;
    let rows: Vec<(String,String,String,i64)> = sqlx::query_as(
        "SELECT requested_model_id,api_key_id,user_id,charge_micro_cny FROM pricing_charges_cny WHERE tenant_id=$1 AND status='charged'")
        .bind(f.tenant.to_string()).fetch_all(f.cp.db().pool()).await.expect("只读真实普通遥测定价结果");
    let mut a = 0;
    let mut b = 0;
    for (model, key, user, amount) in rows {
        assert!(
            key == f.key.to_string() && user == f.member.to_string(),
            "收费仍固定原 key/member"
        );
        if model == f.a.to_string() {
            a += amount;
        } else {
            assert!(model == f.b.to_string(), "不能产生第三个收费模型");
            b += amount;
        }
    }
    assert_eq!(a, 144, "A: (10-2)*10 + 2*2 + 3*20 = 144 micro-CNY");
    assert_eq!(
        b, wanted_b,
        "B: 两个独立 accepted item 各 5*100 + 1*200 = 700；不固定分组行数"
    );
    let debit: i64 = sqlx::query_scalar("SELECT COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'")
        .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("全量真实 ledger debit");
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(f.tenant)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("真实 CNY 余额");
    assert_eq!(
        debit, wanted_total,
        "summary 与 children 不能并行双扣，B 不能按 A 价格收费"
    );
    assert_eq!(balance, 1_000_000 - wanted_total);
    // 连真实普通 intake 的再次投递也不得重账；不重置任何 receiver、provider 或 CP 历史。
    let replay = support::world::deliver(&f.cp.dp_client(&f.bundle), Uuid::new_v4(), wire).await;
    assert_eq!(replay.status, 200);
    assert_eq!(replay.body.get("rejected"), Some(&json!(0)));
    let after: (i64,i64) = sqlx::query_as("SELECT (SELECT COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'),(SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1)")
        .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("真实重放后的全部账本与余额");
    assert_eq!(after, (wanted_total, 1_000_000 - wanted_total));
}

#[tokio::test]
async fn forwarded_header_does_not_bypass_ungranted_asr_alias_after_real_main_pcm_and_usage() {
    let Some(mut f) = fixture(false).await else {
        return;
    };
    let gateway = Gateway::start(&f).await;
    align_real_minute().await;
    let (mut socket, _initial, expected) = primary_positive(&f, &gateway).await;
    let before = f.provider.history();
    assert!(
        before == expected,
        "拒绝前完整 positive provider history 非空且已读回"
    );
    socket
        .send(Message::Text(
            json!({"type":"session.update","event_id":"ungranted_b_on_forwarded_ws",
        "session":{"type":"realtime","audio":{"input":{"transcription":{"model":B}}}}})
            .to_string(),
        ))
        .await
        .expect("在实际成功的 ordinary WS 上请求已存在但未授予的 B");
    await_denial(&mut socket).await;
    if socket
        .send(Message::Text(
            append("new_audio_after_ungranted_b").to_string(),
        ))
        .await
        .is_ok()
    {
        await_denial(&mut socket).await;
    }
    close_and_drain(&mut f, socket).await;
    assert!(
        f.provider.history() == before,
        "最终实际 provider reader 完整 drain 后没有 B config 或新增 audio；不 reset 计数"
    );
    independent_tpm(&gateway, &f, false).await;
    let metrics = gateway.state().metrics.clone();
    metrics_once(&metrics, false).await;
    f.collector.summary_once(false).await;
    let events = gateway.finish(&f, false).await;
    metrics_once(&metrics, false).await;
    bill_all_unchanged(&f, events, false).await;
    assert_eq!(f.collector.summaries(10, 3).len(), 1);
}

#[tokio::test]
async fn forwarded_granted_asr_alias_freezes_accepted_items_and_splits_real_metrics_quota_and_cp_pricing_once(
) {
    let Some(mut f) = fixture(true).await else {
        return;
    };
    let gateway = Gateway::start(&f).await;
    align_real_minute().await;
    let (mut socket, initial, mut expected) = primary_positive(&f, &gateway).await;
    configure(&f, &mut socket, &initial, &mut expected, true).await;
    // 两份 audio 都在真实配置 ACK 之后进入；后续关闭 ASR 不得重写已接受的 item 归属。
    accepted_audio(
        &f,
        &mut socket,
        &mut expected,
        "item_b_first",
        Some("item_primary_pcm"),
    )
    .await;
    accepted_audio(
        &f,
        &mut socket,
        &mut expected,
        "item_b_second",
        Some("item_b_first"),
    )
    .await;
    configure(&f, &mut socket, &initial, &mut expected, false).await;
    completed_asr(&f, &mut socket, "item_b_second").await;
    completed_asr(&f, &mut socket, "item_b_first").await;
    marker(&f.provider, &mut socket, "all_accepted_tail_processed").await;
    close_and_drain(&mut f, socket).await;
    assert!(
        f.provider.history() == expected,
        "完整 provider history 的 config→ACK 后 audio 顺序与 PCM 字节均保持"
    );
    independent_tpm(&gateway, &f, true).await;
    let metrics = gateway.state().metrics.clone();
    metrics_once(&metrics, true).await;
    f.collector.summary_once(true).await;
    let events = gateway.finish(&f, true).await;
    metrics_once(&metrics, true).await;
    bill_all_unchanged(&f, events, true).await;
    assert_eq!(f.collector.summaries(20, 5).len(), 1);
}
