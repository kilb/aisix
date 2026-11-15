//! 真实 RedisStore、CP/PG/kine/mTLS 与网关 WS 的 token gate 工作边界组件合同。
//! decorator 完整委托 RateStore，仅计真实 backend 完成的 token-only acquire 次数；不声称 Redis wire 命令数。
//! 供应商仅本地受控 WS peer；只按实际报告的 A13/B6 计量和 A144/B700 收费，不估算 PCM 用量。

#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{ProxyConfig, RateLimit, RedisConnConfig, RedisMode};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use aisix_ratelimit::{
    Limiter, RateLimitError, RateLimitStatus, RateStore, RedisStore, StreamLeaseRefresh,
};
use async_trait::async_trait;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use chrono::{NaiveDate, Utc};
use futures::{SinkExt as _, StreamExt as _};
use hyper_util::rt::TokioIo;
use native_gateway::{projected_snapshot, HttpRegistry};
use serde_json::{json, Value};
use support::{Bundle, OperatorSession, TestCp};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, Message};
use tower::ServiceExt as _;
use uuid::Uuid;

const A: &str = "redis-work-main";
const B: &str = "redis-work-asr";
const ASR: &str = "gpt-4o-mini-transcribe";
const PROJECT: &str = "proj_redis_work_component";
const SESSION: &str = "sess_redis_work_component";
const RESPONSE: &str = "resp_redis_work_main";
const LIMIT: Duration = Duration::from_secs(15);
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct CountedRedis {
    inner: Arc<RedisStore>,
    completed: Mutex<BTreeMap<String, u64>>,
    denied: Mutex<BTreeMap<String, u64>>,
}

fn token_only(limits: &RateLimit) -> bool {
    (limits.tpm.is_some() || limits.tph.is_some() || limits.tpd.is_some())
        && limits.rps.is_none()
        && limits.rpm.is_none()
        && limits.rph.is_none()
        && limits.rpd.is_none()
        && limits.concurrency.is_none()
}

#[async_trait]
impl RateStore for CountedRedis {
    async fn acquire(
        &self,
        key: &str,
        limits: &RateLimit,
        member: &str,
    ) -> Result<(), RateLimitError> {
        let result = self.inner.acquire(key, limits, member).await;
        if token_only(limits) {
            if matches!(&result, Err(RateLimitError::Tokens {detail,..}) if matches!(detail.dimension,"tph"|"tpd"))
            {
                *self
                    .denied
                    .lock()
                    .expect("实际 backend 长窗口 token 拒绝历史")
                    .entry(key.to_owned())
                    .or_default() += 1;
            }
            *self
                .completed
                .lock()
                .expect("完整 backend acquire 历史")
                .entry(key.to_owned())
                .or_default() += 1;
        }
        result
    }
    async fn commit(&self, key: &str, tokens: u64, member: &str) {
        self.inner.commit(key, tokens, member).await;
    }
    fn release(&self, key: &str, member: &str) {
        self.inner.release(key, member);
    }
    fn add_tokens(&self, key: &str, tokens: u64) {
        self.inner.add_tokens(key, tokens);
    }
    fn add_tokens_all(&self, keys: &[String], tokens: u64) {
        self.inner.add_tokens_all(keys, tokens);
    }
    fn reap(&self, idle_for: Duration) {
        self.inner.reap(idle_for);
    }
    fn stream_lease_refresh_interval(&self) -> Option<Duration> {
        self.inner.stream_lease_refresh_interval()
    }
    async fn refresh_stream_lease(&self, key: &str, member: &str) {
        self.inner.refresh_stream_lease(key, member).await;
    }
    async fn refresh_stream_lease_outcome(&self, key: &str, member: &str) -> StreamLeaseRefresh {
        self.inner.refresh_stream_lease_outcome(key, member).await
    }
    async fn peek(&self, key: &str, limits: &RateLimit) -> Option<RateLimitStatus> {
        self.inner.peek(key, limits).await
    }
    async fn claim_once(&self, key: &str, ttl: Duration) -> bool {
        self.inner.claim_once(key, ttl).await
    }
    fn claims_outlive_process(&self) -> bool {
        self.inner.claims_outlive_process()
    }
    fn local_key_count(&self) -> Option<u64> {
        self.inner.local_key_count()
    }
}

struct SharedRedis {
    measured: Arc<CountedRedis>,
    peer: RedisStore,
    day: NaiveDate,
}

impl SharedRedis {
    async fn connect() -> Self {
        let url = std::env::var("RATELIMIT_TEST_REDIS_URL")
            .unwrap_or_else(|_| panic!("required setup missing: RATELIMIT_TEST_REDIS_URL"));
        assert!(
            !url.trim().is_empty(),
            "required setup missing: RATELIMIT_TEST_REDIS_URL"
        );
        let cfg = RedisConnConfig {
            mode: RedisMode::Single,
            url: Some(url),
            ..Default::default()
        };
        let namespace = Uuid::new_v4().to_string();
        let inner = Arc::new(
            RedisStore::connect(&cfg)
                .await
                .unwrap_or_else(|_| panic!("real Redis setup failed: RATELIMIT_TEST_REDIS_URL"))
                .with_env_namespace(&namespace),
        );
        let peer = RedisStore::connect(&cfg)
            .await
            .unwrap_or_else(|_| panic!("second real Redis setup failed: RATELIMIT_TEST_REDIS_URL"))
            .with_env_namespace(&namespace);
        let day = Utc::now().date_naive();
        let measured = Arc::new(CountedRedis {
            inner,
            completed: Mutex::new(BTreeMap::new()),
            denied: Mutex::new(BTreeMap::new()),
        });
        let shared = Self {
            measured,
            peer,
            day,
        };
        let calibration = format!("component-calibration:{}", Uuid::new_v4());
        let limits = RateLimit {
            tpm: Some(100_000),
            tph: Some(100_000),
            tpd: Some(100_000),
            ..Default::default()
        };
        let member = Uuid::new_v4().to_string();
        assert!(
            shared
                .measured
                .inner
                .acquire(&calibration, &limits, &member)
                .await
                .is_ok(),
            "actual Redis calibration admission succeeds before commit"
        );
        shared.measured.inner.commit(&calibration, 7, &member).await;
        tokio::time::timeout(LIMIT, async {
            loop {
                if shared
                    .peer
                    .peek(&calibration, &limits)
                    .await
                    .is_some_and(|row| row.tpm_used == 7)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("second actual RedisStore sees shared nonzero7; never LocalStore fallback");
        shared.exact(&calibration, 7).await;
        shared
    }

    fn day_unchanged(&self) {
        assert!(
            Utc::now().date_naive() == self.day,
            "real UTC day boundary crossed: setup window invalid; no clock/TTL changes or skip"
        );
    }

    async fn exact(&self, key: &str, expected: u64) {
        assert!(expected > 0, "分布式正控必须是非零已报告量");
        tokio::time::timeout(LIMIT,async {
            loop {
                self.day_unchanged();
                // check-only 不增 token；n 拒而 n+1 允精确夹住整数数量，避免依赖仅公开 TPM 的 peek。
                let above = RateLimit {tpd:Some(expected+1),..Default::default()};
                let member = Uuid::new_v4().to_string();
                let allowed = self.peer.acquire(key,&above,&member).await;
                assert!(allowed.is_ok(),"actual shared daily quantity cannot exceed the known unique supplier amount");
                self.peer.release(key,&member);
                let at = RateLimit {tpd:Some(expected),..Default::default()};
                let member = Uuid::new_v4().to_string();
                match self.peer.acquire(key,&at,&member).await {
                    Err(RateLimitError::Tokens {detail,..}) => {
                        assert_eq!(detail.dimension,"tpd");
                        assert_eq!(detail.limit,expected);
                        break;
                    }
                    Ok(()) => self.peer.release(key,&member),
                    Err(_) => panic!("check-only daily token probe must not fail for another dimension"),
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("actual Redis integer token quantity reaches the known supplier count without minute rollover assumptions");
    }
}

struct ProviderState {
    secret: String,
    output: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    inputs: Mutex<Vec<Value>>,
    handshakes: Mutex<Vec<[bool; 4]>>,
}

struct Provider {
    base: String,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start(vad: bool) -> Self {
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
                    "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,"turn_detection":if vad {json!({"type":"server_vad","create_response":false})} else {Value::Null}}},
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
                    history.iter().zip(expected).all(|(got, wanted)| {
                        if wanted.get("type") == Some(&json!("session.update")) {
                            got.get("type") == wanted.get("type")
                                && got.pointer("/session/type") == wanted.pointer("/session/type")
                                && got.pointer("/session/audio/input/transcription")
                                    == wanted.pointer("/session/audio/input/transcription")
                        } else {
                            got == wanted
                        }
                    }),
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
            self.state.handshakes.lock().expect("握手日志").as_slice() == [[true; 4]],
            "唯一实际 provider 握手保持原凭据、project、路径和主模型"
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

struct Fixture {
    cp: TestCp,
    peer: Provider,
    bundle: Bundle,
    redis: SharedRedis,
    tenant: Uuid,
    key: Uuid,
    a: Uuid,
    b: Uuid,
    policies: [Uuid; 2],
    bearer: String,
}

impl Fixture {
    fn buckets(&self) -> [String; 4] {
        [
            format!("model:{A}"),
            format!("policy:api_key:{}:{}", self.key, self.policies[0]),
            format!("policy:api_key:{}:{}", self.key, self.policies[1]),
            format!("model:{B}"),
        ]
    }
    fn counts(&self) -> [u64; 4] {
        let keys = self.buckets();
        let counts = self
            .redis
            .measured
            .completed
            .lock()
            .expect("完整实际 token-only acquire 历史");
        std::array::from_fn(|i| counts.get(&keys[i]).copied().unwrap_or(0))
    }
    fn b_denials(&self) -> u64 {
        self.redis
            .measured
            .denied
            .lock()
            .expect("实际 B quota 拒绝历史")
            .get(&self.buckets()[3])
            .copied()
            .unwrap_or(0)
    }
    async fn known_tokens(&self, with_b: bool) {
        let keys = self.buckets();
        for (index, amount) in [
            (0, 13),
            (1, if with_b { 19 } else { 13 }),
            (2, if with_b { 19 } else { 13 }),
        ] {
            self.redis.exact(&keys[index], amount).await;
        }
        if with_b {
            self.redis.exact(&keys[3], 6).await;
        }
    }
}

fn id(value: &Value, pointer: &str) -> Uuid {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
        .expect("明确 public resource UUID")
}

async fn admin_post(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let response = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(response.status, 201, "真实 public Admin 配置写入成功");
    response.body
}

async fn fixture(vad: bool) -> Fixture {
    let redis = SharedRedis::connect().await;
    let cp = TestCp::start_with_kine()
        .await
        .expect("required real CP/PG/kine setup: CP_TEST_DATABASE_URL and CP_TEST_KINE_ENDPOINT");
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let peer = Provider::start(vad).await;
    let account = admin_post(
        &cp,
        &admin,
        "/admin/resources/provider_keys",
        json!({"doc":{
            "display_name":"redis-work-original-account","provider":"openai","adapter":"openai",
            "api_base":peer.base,"api_key":peer.state.secret,"groups":["shared"],
            "request":{"default_headers":{"OpenAI-Project":PROJECT}}
        }}),
    )
    .await;
    let account = id(&account, "/id");
    let mut models = Vec::new();
    for (alias, upstream, cap) in [(A, "gpt-realtime", 100000), (B, ASR, 6)] {
        let result=admin_post(&cp,&admin,"/admin/resources/models",json!({"doc":{
            "display_name":alias,"provider":"openai","model_name":upstream,"provider_key_id":account,
            "rate_limit":{"tpm":100000,"tph":cap,"tpd":cap}
        }})).await;
        models.push(id(&result, "/id"));
    }
    let (a, b) = (models[0], models[1]);
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
                .expect("实际 tenant version"),
            json!({"allowed_model_ids":[a,b]}),
        )
        .await;
    assert_eq!(granted.status, 200);
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    let minted = admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":"redis-work-key","member_id":member,"group_id":group}),
    )
    .await;
    let key = id(&minted, "/key/id");
    let bearer = minted["plaintext"]
        .as_str()
        .expect("实际签发 bearer")
        .to_owned();
    admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/credits"),
        json!({"amount_cny":"1","reason":"Redis WS 工作边界真实账务正控"}),
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
        admin_post(
            &cp,
            &admin,
            "/admin/pricing/baselines",
            json!({"model_id":model,"effective_from":support::world::db_now(&cp).await,
            "note":"既有 token/audio 维度实际非零价格","card":{"default":{"base":card}}}),
        )
        .await;
    }
    let mut policies = Vec::new();
    for window in ["hour", "day"] {
        let result=admin_post(&cp,&admin,"/admin/resources/rate_limit_policies",json!({"doc":{
            "name":format!("redis-work-request-{window}"),"scope":"api_key","scope_ref":key,"window":window,"max_tokens":100000
        }})).await;
        let policy = id(&result, "/id");
        let saved = cp
            .send(
                "GET",
                &format!("/admin/resources/rate_limit_policies/{policy}"),
                Some(admin.cookie()),
                None,
            )
            .await;
        assert_eq!(saved.status, 200);
        assert!(
            saved.body.pointer("/doc/window") == Some(&json!(window))
                && saved.body.pointer("/doc/max_tokens") == Some(&json!(100000)),
            "GW request cap 始终高于最终19；peer临时19/20夹逼不修改该配置"
        );
        policies.push(policy);
    }
    let bundle = cp.issue_bundle(&admin, "redis-work-real-gateway").await;
    Fixture {
        cp,
        peer,
        bundle,
        redis,
        tenant,
        key,
        a,
        b,
        policies: [policies[0], policies[1]],
        bearer,
    }
}

struct Gateway {
    state: ProxyState,
    address: SocketAddr,
    billing: mpsc::Receiver<UsageEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl Gateway {
    async fn start(f: &Fixture) -> Self {
        let snapshot = projected_snapshot(&f.cp).await;
        for (model, cap) in [(f.a, 100000), (f.b, 6)] {
            let entry = snapshot
                .models
                .get_by_id(&model.to_string())
                .expect("真实投射的 quota model");
            let limits = entry
                .value
                .rate_limit
                .as_ref()
                .expect("实际非空 model rate_limit");
            assert!(
                limits.tpm == Some(100000) && limits.tph == Some(cap) && limits.tpd == Some(cap),
                "合法TPH/TPD实际到达 serving snapshot"
            );
        }
        for (policy, window) in f.policies.iter().zip(["hour", "day"]) {
            let entry = snapshot
                .rate_limit_policies
                .get_by_id(&policy.to_string())
                .expect("实际投射的 request policy");
            let doc = serde_json::to_value(entry.value.as_ref()).expect("public policy DTO");
            assert!(
                doc.get("window") == Some(&json!(window))
                    && doc.get("max_tokens") == Some(&json!(100000))
            );
        }
        let hub = Arc::new(Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        let config: ProxyConfig = serde_json::from_value(
            json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}),
        )
        .expect("公开 gateway config");
        let limiter = Arc::new(Limiter::with_store(f.redis.measured.clone()));
        let (sender, billing) = mpsc::channel(128);
        let state = ProxyState::with_limiter(SnapshotHandle::new(snapshot), hub, limiter, &config)
            .without_cache()
            .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
                &f.cp.state().pki.platform_id_key_hex(),
            ))
            .with_native_objects(Arc::new(HttpRegistry::new(&f.cp, &f.bundle)))
            .with_usage_sink(UsageSink::new(sender));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("真实 Gateway HTTP listener");
        let address = listener.local_addr().expect("真实 gateway address");
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.expect("真实 gateway TCP");
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
        let mut request = format!("ws://{}/v1/realtime?model={A}", self.address)
            .into_client_request()
            .expect("官方 ordinary WS request");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {bearer}")
                .parse()
                .expect("实际 bearer header"),
        );
        let (socket, response) = tokio_tungstenite::connect_async(request)
            .await
            .unwrap_or_else(|_| {
                panic!("actual Gateway WS upgrade must succeed; no body/secret diagnostics")
            });
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        socket
    }
    async fn no_ordinary_rebill(&mut self) {
        assert!(
            matches!(
                self.billing.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "canonical known facts 不并行排入普通计费 aggregate"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), self.billing.recv())
                .await
                .is_err()
        );
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn growth(before: [u64; 4], after: [u64; 4]) {
    assert!(before.iter().zip(after).all(|(a,b)|b>*a),"A/request-hour/request-day/B 每层新work实际到达 token-only RedisStore.acquire；不是常量计数或假channel");
}

fn audio(delivery: &str) -> Value {
    json!({"type":"input_audio_buffer.append","event_id":delivery,
        "audio":base64::engine::general_purpose::STANDARD.encode([3_u8;4800])})
}

async fn input(f: &Fixture, socket: &mut Socket, expected: &mut Vec<Value>, event: Value) {
    socket
        .send(Message::Text(event.to_string()))
        .await
        .expect("实际 client WS input");
    expected.push(event);
    f.peer.received(expected).await;
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

fn main_done(delivery: &str) -> Value {
    json!({"type":"response.done","event_id":delivery,"response":{"object":"realtime.response","id":RESPONSE,
        "status":"completed","status_details":null,"output":[],"usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,
            "input_token_details":{"cached_tokens":2,"text_tokens":10},"output_token_details":{"text_tokens":3}}}})
}

fn asr_done(item: &str, delivery: &str) -> Value {
    // SDK 3dae: ASR usage 属于 item/content 独立模型，event_id 只是 delivery 身份。
    json!({"type":"conversation.item.input_audio_transcription.completed","event_id":delivery,"item_id":item,"content_index":0,
        "transcript":"controlled component transcription","usage":{"type":"tokens","input_tokens":5,"output_tokens":1,"total_tokens":6,
            "input_token_details":{"audio_tokens":5,"text_tokens":0}}})
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

async fn applied_b(f: &Fixture, gateway: &Gateway) -> (Socket, Vec<Value>) {
    let mut socket = gateway.connect(&f.bearer).await;
    let initial = next_event(&mut socket).await;
    assert!(
        initial.get("type") == Some(&json!("session.created"))
            && initial.pointer("/session/id") == Some(&json!(SESSION))
            && initial.pointer("/session/model") == Some(&json!(A)),
        "真实 provider session 与调用方主模型 alias 正控"
    );
    f.peer.assert_handshake();
    let event = json!({"type":"session.update","event_id":"redis_work_enable_b",
        "session":{"type":"realtime","audio":{"input":{"transcription":{"model":B}}}}});
    socket
        .send(Message::Text(event.to_string()))
        .await
        .expect("实际配置已授权 B alias");
    let mut wanted = event;
    wanted["session"]["audio"]["input"]["transcription"]["model"] = json!(ASR);
    let mut expected = vec![wanted];
    f.peer.received(&expected).await;
    let mut applied = initial["session"].clone();
    applied["model"] = json!("gpt-realtime");
    applied["audio"]["input"]["transcription"] = json!({"model":ASR});
    f.peer.send(
        json!({"type":"session.updated","event_id":"redis_work_actual_b_ack","session":applied}),
    );
    let ack = next_event(&mut socket).await;
    assert!(
        ack.get("type") == Some(&json!("session.updated"))
            && ack.pointer("/session/audio/input/transcription/model") == Some(&json!(ASR)),
        "实际 upstream B rewrite 和 provider ACK 已发生，之后才发送 PCM"
    );
    let before = f.counts();
    input(f,&mut socket,&mut expected,json!({"type":"response.create","event_id":"redis_work_main_create","response":{"output_modalities":["text"]}})).await;
    let after = f.counts();
    assert!(
        after[0] > before[0]
            && after[1] > before[1]
            && after[2] > before[2]
            && after[3] == before[3],
        "每个 response.create 真正检查 A/request；已启用 B 不代表 response.create 新增 ASR 工作"
    );
    f.peer.send(json!({"type":"response.created","event_id":"redis_work_main_created","response":{"id":RESPONSE,"object":"realtime.response","status":"in_progress","output":[]}}));
    let created = next_event(&mut socket).await;
    assert!(
        created.get("type") == Some(&json!("response.created"))
            && created.pointer("/response/id") == Some(&json!(RESPONSE))
    );
    f.peer.send(main_done("redis_work_main_done"));
    let done = next_event(&mut socket).await;
    assert!(
        done.get("type") == Some(&json!("response.done"))
            && done.pointer("/response/usage/total_tokens") == Some(&json!(13))
    );
    f.peer.send(main_done("redis_work_main_changed_delivery"));
    marker(&f.peer, &mut socket, "redis_work_main_duplicate_processed").await;
    paid(f, false).await;
    f.known_tokens(false).await;
    (socket, expected)
}

async fn paid(f: &Fixture, with_b: bool) {
    let count = if with_b { 2 } else { 1 };
    let total = if with_b { 844 } else { 144 };
    tokio::time::timeout(LIMIT,async {
        loop {
            let rows: Vec<(String,i64)>=sqlx::query_as("SELECT requested_model_id,charge_micro_cny FROM pricing_charges_cny WHERE tenant_id=$1 AND status='charged'")
                .bind(f.tenant.to_string()).fetch_all(f.cp.db().pool()).await.expect("实际 CP 非零定价");
            assert!(rows.len()<=count,"稳定 supplier fact/changed delivery 不能重账或变成 charged0");
            if rows.len()==count {
                assert_eq!(rows.iter().filter(|row|row.0==f.a.to_string() && row.1==144).count(),1);
                if with_b { assert_eq!(rows.iter().filter(|row|row.0==f.b.to_string() && row.1==700).count(),1); }
                let money: (i64,i64)=sqlx::query_as("SELECT (SELECT COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'),(SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1)")
                    .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("实际 ledger 和 CNY balance");
                assert_eq!(money,(total,1_000_000-total));
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("实际 A144/B700 的已接受用量独立计费，不因 quota 或 duplicate 丢失/双扣");
}

async fn clear(f: &Fixture, socket: &mut Socket, expected: &mut Vec<Value>, tag: &str) {
    input(
        f,
        socket,
        expected,
        json!({"type":"input_audio_buffer.clear","event_id":format!("clear_{tag}")}),
    )
    .await;
    f.peer
        .send(json!({"type":"input_audio_buffer.cleared","event_id":format!("cleared_{tag}")}));
    assert_eq!(
        next_event(socket).await.get("type"),
        Some(&json!("input_audio_buffer.cleared"))
    );
}

async fn accepted_item(f: &Fixture, socket: &mut Socket, item: &str, previous: Option<&str>) {
    f.peer.send(json!({"type":"input_audio_buffer.committed","event_id":format!("committed_{item}"),"item_id":item,"previous_item_id":previous}));
    let committed = next_event(socket).await;
    assert!(
        committed.get("type") == Some(&json!("input_audio_buffer.committed"))
            && committed.get("item_id") == Some(&json!(item))
    );
    f.peer.send(json!({"type":"conversation.item.created","event_id":format!("created_{item}"),"previous_item_id":previous,
        "item":{"id":item,"object":"realtime.item","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}));
    let created = next_event(socket).await;
    assert!(
        created.get("type") == Some(&json!("conversation.item.created"))
            && created.pointer("/item/id") == Some(&json!(item))
            && created.pointer("/item/content/0/type") == Some(&json!("input_audio")),
        "实际已接受 audio item/content0 才能产生后续独立 B 用量"
    );
}

async fn commit(
    f: &Fixture,
    socket: &mut Socket,
    expected: &mut Vec<Value>,
    item: &str,
    previous: Option<&str>,
) {
    input(
        f,
        socket,
        expected,
        json!({"type":"input_audio_buffer.commit","event_id":format!("commit_{item}")}),
    )
    .await;
    accepted_item(f, socket, item, previous).await;
}

async fn gated_append(
    f: &Fixture,
    socket: &mut Socket,
    expected: &mut Vec<Value>,
    tag: &str,
) -> [u64; 4] {
    let before = f.counts();
    input(f, socket, expected, audio(tag)).await;
    let after = f.counts();
    growth(before, after);
    after
}

async fn same_buffer_fragments(
    f: &Fixture,
    socket: &mut Socket,
    expected: &mut Vec<Value>,
    tag: &str,
    first: [u64; 4],
    n: usize,
) {
    for i in 1..n {
        input(f, socket, expected, audio(&format!("{tag}_{i}"))).await;
        f.redis.day_unchanged();
        assert_eq!(f.counts(),first,"无 clear/commit/新fact/配置变更的连续 PCM 不能按片增加 A/request/B token-only backend acquire；不是 wire 命令计数");
    }
}

async fn inline_item(f: &Fixture, socket: &mut Socket, expected: &mut Vec<Value>) {
    let before = f.counts();
    input(f,socket,expected,json!({"type":"conversation.item.create","event_id":"independent_inline_audio","previous_item_id":"item_manual_first",
        "item":{"id":"item_inline_independent","type":"message","role":"user","content":[{"type":"input_audio",
            "audio":base64::engine::general_purpose::STANDARD.encode([3_u8;4800])}]}})).await;
    growth(before, f.counts());
    f.peer.send(json!({"type":"conversation.item.created","event_id":"actual_inline_item_created","previous_item_id":"item_manual_first",
        "item":{"id":"item_inline_independent","object":"realtime.item","type":"message","role":"user","status":"completed",
            "content":[{"type":"input_audio","transcript":null}]}}));
    let created = next_event(socket).await;
    assert!(
        created.get("type") == Some(&json!("conversation.item.created"))
            && created.pointer("/item/id") == Some(&json!("item_inline_independent"))
    );
}

async fn known_b(f: &Fixture, socket: &mut Socket, item: &str) {
    f.peer.send(asr_done(item, "redis_work_b_done"));
    let completed = next_event(socket).await;
    assert!(
        completed.get("type")
            == Some(&json!(
                "conversation.item.input_audio_transcription.completed"
            ))
            && completed.get("item_id") == Some(&json!(item))
            && completed.get("content_index") == Some(&json!(0))
    );
    assert_eq!(completed.pointer("/usage/total_tokens"), Some(&json!(6)));
    assert_eq!(
        completed.pointer("/usage/input_token_details/audio_tokens"),
        Some(&json!(5))
    );
    assert_eq!(
        completed.pointer("/usage/input_token_details/text_tokens"),
        Some(&json!(0))
    );
    f.peer
        .send(asr_done(item, "redis_work_b_same_item_changed_delivery"));
    marker(&f.peer, socket, "redis_work_b_duplicate_processed").await;
    paid(f, true).await;
    f.known_tokens(true).await;
}

async fn denied_new_turn(f: &mut Fixture, gateway: &mut Gateway, mut socket: Socket, tag: &str) {
    let snapshot = gateway.state.snapshot.load();
    let key = snapshot
        .apikeys
        .get_by_id(&f.key.to_string())
        .expect("实际 serving creator key");
    assert!(
        !key.value.disabled
            && key.value.can_access(&snapshot, A)
            && key.value.can_access(&snapshot, B),
        "实际本地 A/B grant 始终有效，负例针对已知 token cap，不靠撤权或错 route 恒拒"
    );
    let before = f.peer.history();
    let gate_before = f.counts();
    let denied_before = f.b_denials();
    socket
        .send(Message::Text(audio(tag).to_string()))
        .await
        .expect("在实际活着的已接受 WS 上请求下一turn；测试未先发 providerClose");
    tokio::time::timeout(LIMIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let event: Value =
                        serde_json::from_str(&text).expect("实际 gateway event JSON");
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
    .expect("实际已耗尽 B 的下一turn必须报错/停止，不能继承旧buffer许可");
    assert!(f.counts()[3]>gate_before[3] && f.b_denials()>denied_before,
        "下一turn实际 B token-only RedisStore.acquire 已返回 tph/tpd quota 拒绝，不是假 local error 或 peer计数");
    let _ = socket.close(None).await;
    drop(socket);
    f.peer.drained().await;
    f.peer.assert_handshake();
    assert!(
        f.peer.history() == before,
        "真实 provider reader 完整drain后没有新增 PCM/commit/config；历史未 reset"
    );
    paid(f, true).await;
    f.known_tokens(true).await;
    gateway.no_ordinary_rebill().await;
}

async fn vad_committed(
    f: &Fixture,
    socket: &mut Socket,
    item: &str,
    previous: Option<&str>,
    start: u64,
) {
    f.peer.send(json!({"type":"input_audio_buffer.speech_started","event_id":format!("start_{item}"),"audio_start_ms":start,"item_id":item}));
    let event = next_event(socket).await;
    assert!(
        event.get("type") == Some(&json!("input_audio_buffer.speech_started"))
            && event.get("item_id") == Some(&json!(item))
    );
    f.peer.send(json!({"type":"input_audio_buffer.speech_stopped","event_id":format!("stop_{item}"),"audio_end_ms":start+100,"item_id":item}));
    let event = next_event(socket).await;
    assert!(
        event.get("type") == Some(&json!("input_audio_buffer.speech_stopped"))
            && event.get("item_id") == Some(&json!(item))
    );
    accepted_item(f, socket, item, previous).await;
}

#[tokio::test]
async fn continuous_pcm_uses_one_token_admission_per_buffer_and_clear_commit_inline_start_new_work()
{
    let mut f = fixture(false).await;
    let mut gateway = Gateway::start(&f).await;
    let (mut socket, mut expected) = applied_b(&f, &gateway).await;
    let first = gated_append(&f, &mut socket, &mut expected, "first_manual_buffer_0").await;
    same_buffer_fragments(
        &f,
        &mut socket,
        &mut expected,
        "first_manual_buffer",
        first,
        16,
    )
    .await;
    clear(
        &f,
        &mut socket,
        &mut expected,
        "discard_first_manual_buffer",
    )
    .await;
    let second = gated_append(&f, &mut socket, &mut expected, "after_clear_buffer_0").await;
    same_buffer_fragments(
        &f,
        &mut socket,
        &mut expected,
        "after_clear_buffer",
        second,
        4,
    )
    .await;
    commit(&f, &mut socket, &mut expected, "item_manual_first", None).await;
    inline_item(&f, &mut socket, &mut expected).await;
    // inline item 是独立工作；不能给 commit 后尚未开始的另一个 buffer 授予许可。
    let third = gated_append(
        &f,
        &mut socket,
        &mut expected,
        "post_commit_and_inline_buffer_0",
    )
    .await;
    same_buffer_fragments(
        &f,
        &mut socket,
        &mut expected,
        "post_commit_and_inline_buffer",
        third,
        4,
    )
    .await;
    commit(
        &f,
        &mut socket,
        &mut expected,
        "item_manual_second",
        Some("item_inline_independent"),
    )
    .await;
    gated_append(
        &f,
        &mut socket,
        &mut expected,
        "next_buffer_after_explicit_commit",
    )
    .await;
    clear(
        &f,
        &mut socket,
        &mut expected,
        "discard_unreported_last_buffer",
    )
    .await;
    known_b(&f, &mut socket, "item_manual_first").await;
    denied_new_turn(&mut f, &mut gateway, socket, "new_turn_after_shared_b6").await;
}

#[tokio::test]
async fn server_vad_commit_without_client_commit_rechecks_the_next_turn_and_known_shared_b_limit() {
    let mut f = fixture(true).await;
    let mut gateway = Gateway::start(&f).await;
    let (mut socket, mut expected) = applied_b(&f, &gateway).await;
    gated_append(&f, &mut socket, &mut expected, "vad_first_turn_audio").await;
    vad_committed(&f, &mut socket, "item_vad_first", None, 0).await;
    // 此处尚无新 usage/config；只有实际 server committed 能使下一turn重新准入，防 fact reset 掩盖遗漏。
    gated_append(
        &f,
        &mut socket,
        &mut expected,
        "vad_second_turn_after_provider_commit",
    )
    .await;
    vad_committed(
        &f,
        &mut socket,
        "item_vad_second",
        Some("item_vad_first"),
        100,
    )
    .await;
    assert!(
        f.peer
            .history()
            .iter()
            .all(|event| event.get("type") != Some(&json!("input_audio_buffer.commit"))),
        "两次真实 VAD committed 均无 client commit；不构造 append ACK"
    );
    known_b(&f, &mut socket, "item_vad_first").await;
    denied_new_turn(
        &mut f,
        &mut gateway,
        socket,
        "vad_third_turn_after_shared_b6",
    )
    .await;
}

// 只供补充场景使用；旧两例仍逐字使用默认 A/request100000、B6 的原 fixture。
async fn install_wide_b_and_request_day19(f: &Fixture, gateway: &Gateway) {
    let admin = f.cp.admin_login_seeded(OperatorRole::Admin).await;
    let path = format!("/admin/resources/models/{}", f.b);
    let current = f.cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    let mut doc = current.body["doc"].clone();
    doc["rate_limit"] = json!({"tpm":100000,"tph":100000,"tpd":100000});
    let widened =
        f.cp.send_conditional(
            "PUT",
            &path,
            admin.cookie(),
            current.body["version"]
                .as_i64()
                .expect("actual B model version"),
            json!({"doc":doc}),
        )
        .await;
    assert_eq!(
        widened.status, 200,
        "real public API widens B without changing its model/account/grant"
    );
    let path = format!("/admin/resources/rate_limit_policies/{}", f.policies[1]);
    let current = f.cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    assert!(
        current.body.pointer("/doc/window") == Some(&json!("day"))
            && current.body.pointer("/doc/scope_ref") == Some(&json!(f.key))
    );
    let mut doc = current.body["doc"].clone();
    doc["max_tokens"] = json!(19);
    let limited =
        f.cp.send_conditional(
            "PUT",
            &path,
            admin.cookie(),
            current.body["version"]
                .as_i64()
                .expect("actual request-day policy version"),
            json!({"doc":doc}),
        )
        .await;
    assert_eq!(
        limited.status, 200,
        "real request-day cap19 is configured before WS admission"
    );
    let current = f.cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    assert_eq!(current.body.pointer("/doc/max_tokens"), Some(&json!(19)));
    let projected = projected_snapshot(&f.cp).await;
    gateway.state.snapshot.store(projected);
    supplementary_caps(f, gateway);
}

fn supplementary_caps(f: &Fixture, gateway: &Gateway) {
    let snapshot = gateway.state.snapshot.load();
    for model in [f.a, f.b] {
        let entry = snapshot
            .models
            .get_by_id(&model.to_string())
            .expect("actual synchronized quota model");
        let limits = entry
            .value
            .rate_limit
            .as_ref()
            .expect("actual synchronized token caps");
        assert!(
            limits.tpm == Some(100000) && limits.tph == Some(100000) && limits.tpd == Some(100000),
            "A and B remain wide; neither model can be the known19 request rejection"
        );
    }
    for (policy, cap) in [(f.policies[0], 100000), (f.policies[1], 19)] {
        let entry = snapshot
            .rate_limit_policies
            .get_by_id(&policy.to_string())
            .expect("actual synchronized request policy");
        let doc = serde_json::to_value(entry.value.as_ref()).expect("public request policy DTO");
        assert!(
            doc.get("max_tokens") == Some(&json!(cap)),
            "the serving snapshot has the exact intended request cap"
        );
    }
    let key = snapshot
        .apikeys
        .get_by_id(&f.key.to_string())
        .expect("actual unchanged creator key");
    assert!(!key.value.disabled && key.value.can_access(&snapshot,A) && key.value.can_access(&snapshot,B),
        "both grants and the same enabled creator remain valid after real configuration synchronization");
}

fn request_day_denials(f: &Fixture) -> u64 {
    f.redis
        .measured
        .denied
        .lock()
        .expect("actual request backend token-denial history")
        .get(&f.buckets()[2])
        .copied()
        .unwrap_or(0)
}

#[tokio::test]
async fn late_accepted_asr_usage_invalidates_request_permit_for_an_already_admitted_buffer() {
    let mut f = fixture(false).await;
    let mut gateway = Gateway::start(&f).await;
    // 原 start 的默认 cap 正控先原样执行；本例的 public 配置和真实投影都在 connect 前完成。
    install_wide_b_and_request_day19(&f, &gateway).await;
    let (mut socket, mut expected) = applied_b(&f, &gateway).await;
    gated_append(
        &f,
        &mut socket,
        &mut expected,
        "old_b_work_before_request_exhaustion",
    )
    .await;
    commit(
        &f,
        &mut socket,
        &mut expected,
        "item_b_accepted_for_late_request_fact",
        None,
    )
    .await;
    gated_append(
        &f,
        &mut socket,
        &mut expected,
        "another_buffer_admitted_while_request_is13",
    )
    .await;
    let admitted = f.peer.history();
    assert!(
        admitted.last().is_some_and(|event| event.get("type")
            == Some(&json!("input_audio_buffer.append"))
            && event.get("event_id") == Some(&json!("another_buffer_admitted_while_request_is13"))),
        "the other buffer is already physically forwarded after real A/request/B admission"
    );
    let policy_generation = gateway
        .state
        .snapshot
        .load()
        .rate_limit_policies
        .generation();
    // 从此没有 commit/clear/config/新 committed/marker/duplicate，只有旧已接受 B item 的真实非零尾量。
    f.peer.send(asr_done(
        "item_b_accepted_for_late_request_fact",
        "only_late_b6_advances_request19",
    ));
    let completed = next_event(&mut socket).await;
    assert!(
        completed.get("type")
            == Some(&json!(
                "conversation.item.input_audio_transcription.completed"
            ))
            && completed.get("item_id") == Some(&json!("item_b_accepted_for_late_request_fact"))
            && completed.get("content_index") == Some(&json!(0)),
        "the accepted old B fact is actually relayed while the other buffer stays open"
    );
    assert_eq!(completed.pointer("/usage/total_tokens"), Some(&json!(6)));
    assert_eq!(
        completed.pointer("/usage/input_token_details/audio_tokens"),
        Some(&json!(5))
    );
    assert_eq!(
        completed.pointer("/usage/input_token_details/text_tokens"),
        Some(&json!(0))
    );
    paid(&f, true).await;
    f.known_tokens(true).await;
    supplementary_caps(&f, &gateway);
    assert_eq!(
        gateway
            .state
            .snapshot
            .load()
            .rate_limit_policies
            .generation(),
        policy_generation,
        "no local policy publication can mask late-fact permit invalidation"
    );
    assert!(
        f.peer.history() == admitted,
        "no extra client boundary/input is introduced while waiting for actual request19"
    );
    let member = Uuid::new_v4().to_string();
    let wide = RateLimit {
        tpm: Some(100000),
        tph: Some(100000),
        tpd: Some(100000),
        ..Default::default()
    };
    assert!(f.redis.peer.acquire(&f.buckets()[3], &wide, &member).await.is_ok(),
        "the actual shared B6 is below B's real wide cap; B cannot hide a missing primary/request recheck");
    f.redis.peer.release(&f.buckets()[3], &member);
    let before_counts = f.counts();
    let request_denied_before = request_day_denials(&f);
    let b_denied_before = f.b_denials();
    socket.send(Message::Text(audio("same_open_buffer_after_late_request19").to_string())).await
        .expect("request another frame on the actual already-admitted open buffer before any test close");
    tokio::time::timeout(LIMIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let event: Value = serde_json::from_str(&text).expect("actual gateway event JSON");
                    if event.get("type") == Some(&json!("error")) { break; }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Ping(bytes))) => { let _ = socket.send(Message::Pong(bytes)).await; }
                _ => {}
            }
        }
    }).await.expect("late B usage advances shared request19; the next frame must recheck and refuse the old request permit");
    assert!(f.counts()[2] > before_counts[2] && request_day_denials(&f) > request_denied_before,
        "the actual request-day token-only RedisStore.acquire returned quota refusal; a B-only recheck is insufficient");
    assert_eq!(
        f.b_denials(),
        b_denied_before,
        "wide B must not be the denying layer"
    );
    let _ = socket.close(None).await;
    drop(socket);
    f.peer.drained().await;
    f.peer.assert_handshake();
    assert!(f.peer.history() == admitted,
        "fully drained actual provider history has no new PCM/config/commit after known request19; no counter reset");
    paid(&f, true).await;
    f.known_tokens(true).await;
    gateway.no_ordinary_rebill().await;
}
