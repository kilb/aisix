//! 合法 UUID 与 named 配置共享真实 protected SQLite/GW/供应商 WS 旅程。
//! 供应商只是本地协议组件；不声称真实 CP、供应商处理、定价或 GET /metrics E2E。
//! 归属、来源和计量断言来自公开 typed resource/DTO/schema 与基本兼容合同。

use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aisix_core::native_metering::{ObservedMeter, ObservedUsage};
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::native_api::standalone_store::{stable_resource_id, StandaloneNativeStore};
use aisix_proxy::{build_router, ProxyState};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, Message};
use uuid::Uuid;

const ALIAS: &str = "named-compatible-realtime";
const UPSTREAM: &str = "gpt-realtime";
const PROJECT: &str = "proj_named_ws_component";
const ROUTE: &str = "configured_named_ws_route";
const SESSION: &str = "sess_named_ws_component";
const RESPONSE: &str = "resp_named_ws_component";
const LIMIT: Duration = Duration::from_secs(10);
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Copy)]
struct Ids {
    key: &'static str,
    model: &'static str,
    provider: &'static str,
}

const UUID_IDS: Ids = Ids {
    key: "33333333-3333-4333-8333-333333339901",
    model: "11111111-1111-4111-8111-111111119901",
    provider: "22222222-2222-4222-8222-222222229901",
};
const NAMED_IDS: Ids = Ids {
    key: "ordinary-local-owner-key",
    model: "ordinary-local-main-model",
    provider: "ordinary-local-provider-account",
};

fn response_done(delivery: &str) -> Value {
    json!({"type":"response.done","event_id":delivery,"response":{
        "object":"realtime.response","id":RESPONSE,"status":"completed","status_details":null,
        "output":[],"usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,
            "input_token_details":{"cached_tokens":2,"text_tokens":10},
            "output_token_details":{"text_tokens":3}}}})
}

struct PeerState {
    secret: String,
    handshakes: Mutex<Vec<bool>>,
    inputs: Mutex<Vec<Value>>,
}

struct Peer {
    base: String,
    state: Arc<PeerState>,
    task: tokio::task::JoinHandle<()>,
}

impl Peer {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the controlled actual provider WS");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("provider address")
        );
        let state = Arc::new(PeerState {
            secret: format!("sk-named-component-{}", Uuid::new_v4().simple()),
            handshakes: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("accept actual provider transport");
            let observed = shared.clone();
            let upgrade = tokio_tungstenite::accept_hdr_async(
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
                    let route_header = request
                        .headers()
                        .get("x-static-route")
                        .and_then(|value| value.to_str().ok())
                        == Some(ROUTE);
                    let route = request.uri().path() == "/v1/realtime"
                        && request.uri().query().is_some_and(|query| {
                            query.split('&').any(|field| field == "model=gpt-realtime")
                        });
                    observed
                        .handshakes
                        .lock()
                        .expect("actual full handshake history")
                        .push(authorized && project && route_header && route);
                    Ok(response)
                },
            )
            .await;
            let mut socket =
                upgrade.unwrap_or_else(|_| panic!("actual provider WS handshake failed"));
            let created = json!({"type":"session.created","event_id":"named_session_created",
                "session":{"id":SESSION,"object":"realtime.session","type":"realtime",
                    "model":UPSTREAM,"output_modalities":["text"],"tools":[],
                    "tool_choice":"auto","max_output_tokens":"inf",
                    "expires_at":chrono::Utc::now().timestamp()+3600}});
            assert!(
                socket
                    .send(Message::Text(created.to_string()))
                    .await
                    .is_ok(),
                "send the actual provider session identity"
            );
            while let Some(message) = socket.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        let event: Value =
                            serde_json::from_str(&text).expect("actual provider input JSON");
                        let create = event.get("type") == Some(&json!("response.create"));
                        shared
                            .inputs
                            .lock()
                            .expect("unreset full provider input history")
                            .push(event);
                        if create {
                            assert!(socket.send(Message::Text(json!({"type":"response.created",
                                "event_id":"named_response_created","response":{"object":"realtime.response",
                                    "id":RESPONSE,"status":"in_progress","output":[]}}).to_string()))
                                .await.is_ok(), "accept the actual client response request");
                            for delivery in
                                ["named_response_done", "named_response_done_redelivery"]
                            {
                                assert!(socket.send(Message::Text(response_done(delivery).to_string())).await.is_ok(),
                                    "release nonzero supplier usage and its changed-delivery-ID duplicate");
                            }
                        }
                    }
                    Ok(Message::Ping(bytes)) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        });
        Self { base, state, task }
    }

    async fn drained(&mut self) {
        tokio::time::timeout(LIMIT, &mut self.task)
            .await
            .expect("the actual provider reader drains to transport close")
            .expect("the actual provider reader completes without panic");
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    ids: Ids,
    state: ProxyState,
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    peer: Peer,
    bearer: String,
    usage: mpsc::Receiver<UsageEvent>,
    reader: SqlitePool,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(ids: Ids) -> Self {
        let peer = Peer::new().await;
        let bearer = format!("sk-local-caller-{}", Uuid::new_v4().simple());
        let snapshot = AisixSnapshot::new();
        let provider: ProviderKey = serde_json::from_value(json!({
            "display_name":"ordinary-original-account","provider":"openai","adapter":"openai",
            "api_base":peer.base,"api_key":peer.state.secret,
            "request":{"default_headers":{"OpenAI-Project":PROJECT,"X-Static-Route":ROUTE}}
        }))
        .expect("the public configured provider resource shape");
        snapshot
            .provider_keys
            .insert(ResourceEntry::new(ids.provider, provider, 1));
        let model: Model = serde_json::from_value(json!({"display_name":ALIAS,"provider":"openai",
            "model_name":UPSTREAM,"provider_key_id":ids.provider}))
        .expect("the public direct-model resource shape");
        snapshot
            .models
            .insert(ResourceEntry::new(ids.model, model, 1));
        let key: ApiKey = serde_json::from_value(json!({"display_name":"ordinary-local-owner",
            "key_hash":ApiKey::hash_bearer(&bearer),"allowed_model_ids":[ids.model],
            "allowed_endpoints":["realtime"]}))
        .expect("the public key grants its actual resource ID");
        snapshot.apikeys.insert(ResourceEntry::new(ids.key, key, 1));
        assert!(
            snapshot
                .apikeys
                .get_by_id(ids.key)
                .expect("actual configured key row")
                .id
                == ids.key
                && snapshot
                    .models
                    .get_by_id(ids.model)
                    .expect("actual configured model row")
                    .id
                    == ids.model
                && snapshot
                    .provider_keys
                    .get_by_id(ids.provider)
                    .expect("actual configured account row")
                    .id
                    == ids.provider,
            "the fixture retains all three actual raw resource IDs"
        );
        let snapshot = SnapshotHandle::new(snapshot);
        let directory = tempfile::tempdir().expect("owned local protected-store directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("protect the owned local directory");
        let path = directory.path().join("authority.sqlite3");
        drop(
            std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(&path)
                .expect("create only the protected empty database file"),
        );
        let authority = StandaloneNativeStore::open(&path, [67; 32], snapshot.clone())
            .await
            .unwrap_or_else(|_| panic!("the real protected standalone store must open"));
        assert_eq!(
            std::fs::metadata(directory.path())
                .expect("owned directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path)
                .expect("owned database metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // 只读观察真实文件；op/key/lease/auth 均由实际网关和真实 store 创建。
        let reader = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .read_only(true)
                    .create_if_missing(false),
            )
            .await
            .expect("open a SELECT-only observation connection to the actual store");
        let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"}))
            .expect("ordinary gateway startup configuration");
        let hub = Arc::new(Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
        let (sender, usage) = mpsc::channel(32);
        let state = ProxyState::new(snapshot, hub, &config)
            .without_cache()
            .with_guardrail_index(guardrails)
            .with_native_objects(Arc::new(authority))
            .with_usage_sink(UsageSink::new(sender));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the real ordinary gateway HTTP/WS router");
        let address = listener.local_addr().expect("gateway address");
        let router = build_router(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the actual gateway router");
        });
        Self {
            ids,
            state,
            address,
            task,
            peer,
            bearer,
            usage,
            reader,
            _directory: directory,
        }
    }

    async fn connect(&self) -> Socket {
        let mut request = format!("ws://{}/v1/realtime?model={ALIAS}", self.address)
            .into_client_request()
            .expect("actual raw-key WS request");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.bearer)
                .parse()
                .expect("the actual local caller bearer"),
        );
        request.headers_mut().insert(
            "openai-project",
            "caller-spoof-project"
                .parse()
                .expect("static client header"),
        );
        request.headers_mut().insert(
            "x-static-route",
            "caller-spoof-route".parse().expect("static client header"),
        );
        match tokio_tungstenite::connect_async(request).await {
            Ok((socket, response)) => {
                assert_eq!(
                    response.status(),
                    axum::http::StatusCode::SWITCHING_PROTOCOLS
                );
                socket
            }
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                panic!(
                    "named compatibility phase=raw_ws_admission status={}",
                    response.status().as_u16()
                );
            }
            Err(_) => panic!("named compatibility phase=raw_ws_admission transport_error"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn next_event(socket: &mut Socket) -> Value {
    tokio::time::timeout(LIMIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text)
                        .expect("actual server event JSON without displaying private bodies")
                }
                Some(Ok(Message::Ping(bytes))) => {
                    assert!(
                        socket.send(Message::Pong(bytes)).await.is_ok(),
                        "answer an actual ping"
                    );
                }
                _ => {
                    panic!("the actual expected provider event must arrive before transport close")
                }
            }
        }
    })
    .await
    .expect("the actual provider event crosses the serving gateway")
}

fn metric(text: &str, family: &str) -> Option<f64> {
    let mut result = None;
    for line in text
        .lines()
        .filter(|line| line.starts_with(&format!("{family}{{")))
    {
        let (labels, value) = line.rsplit_once(' ').expect("actual registry sample");
        let labels = labels
            .split_once('{')
            .expect("metric labels")
            .1
            .trim_end_matches('}');
        let fields: Vec<_> = labels.split(',').collect();
        if fields.contains(&"endpoint=\"/v1/realtime\"")
            && fields.contains(&"inbound_protocol=\"realtime\"")
            && fields.contains(&format!("model=\"{ALIAS}\"").as_str())
        {
            let value: f64 = value.parse().expect("actual numeric registry quantity");
            result = Some(result.unwrap_or(0.0) + value);
        }
    }
    result
}

async fn actual_metrics(f: &Fixture) {
    tokio::time::timeout(LIMIT, async {
        loop {
            // 读取实际 WS 驱动的生产 registry，不构造测试 HTTP wrapper 冒充 GET /metrics。
            let text = f.state.metrics.render();
            let mut complete = true;
            for (family, wanted) in [
                ("aisix_llm_input_tokens_total", 10.0),
                ("aisix_llm_output_tokens_total", 3.0),
                ("aisix_llm_total_tokens_total", 13.0),
                ("aisix_llm_cached_input_tokens_total", 2.0),
            ] {
                let value = metric(&text, family);
                assert!(
                    value.is_none_or(|value| value <= wanted),
                    "changed delivery ID cannot increase the real per-alias token series"
                );
                complete &= value == Some(wanted);
            }
            if complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the actual registry contains nonzero alias-attributed 10+3=13 once");
}

#[derive(Clone)]
struct LiveIdentity {
    operation: Uuid,
    owner: String,
}

async fn actual_live_identity(f: &Fixture) -> LiveIdentity {
    tokio::time::timeout(LIMIT, async {
        loop {
            let rows: Vec<(String,String,String,Option<String>,String,String,String,String,i64,Option<i64>)> = sqlx::query_as(
                "SELECT s.operation_id,s.api_key_id,s.owner,s.provider_session_id,s.purpose,s.transport,s.state,o.owner,s.lease_generation,s.lease_expires_at FROM native_live_sessions s JOIN native_operations o ON o.operation_id=s.operation_id")
                .fetch_all(&f.reader).await.expect("observe the actual SQLite operation and live identity");
            assert!(rows.len() <= 1, "one actual raw-key WS creates one original local live operation");
            if let Some((operation, key, owner, provider_session, purpose, transport, state, operation_owner, generation, lease_expires_at)) = rows.into_iter().next() {
                let operation = Uuid::parse_str(&operation).expect("a real nonempty operation UUID");
                assert!(!operation.is_nil(), "the admitted operation identity is nonnil");
                let expected_key = stable_resource_id("apikeys", f.ids.key);
                assert!(!expected_key.is_nil() && key == expected_key.to_string(),
                    "the actual live session belongs to the originally configured APIkey resource");
                assert!(!owner.is_empty() && owner == operation_owner,
                    "opaque original owner is present and identical across actual operation and live rows");
                if !(provider_session.as_deref() == Some(SESSION) && purpose == "realtime"
                    && transport == "web_socket" && state == "live" && generation > 0) {
                    eprintln!("named live diagnostic: provider_session_present={} provider_session_match={} purpose_match={} transport_match={} state_live={} lease_expiry_present={} lease_expiry_positive={} lease_generation_positive={}",
                        provider_session.is_some(), provider_session.as_deref() == Some(SESSION),
                        purpose == "realtime", transport == "web_socket", state == "live",
                        lease_expires_at.is_some(), lease_expires_at.is_some_and(|expiry| expiry > 0), generation > 0);
                }
                assert!(provider_session.as_deref() == Some(SESSION) && purpose == "realtime"
                    && transport == "web_socket" && state == "live" && generation > 0,
                    "the original provider session has a real live WebSocket lease and purpose");
                return LiveIdentity { operation, owner };
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the actual provider session creates a durable original live identity")
}

async fn actual_unknown_close(f: &Fixture, original: &LiveIdentity) {
    tokio::time::timeout(LIMIT, async {
        loop {
            let rows: Vec<(String,String,String,String,String)> = sqlx::query_as(
                "SELECT api_key_id,owner,state,coverage,accounting_state FROM native_live_sessions WHERE operation_id=?")
                .bind(original.operation.to_string()).fetch_all(&f.reader).await
                .expect("observe the original nonempty live close outcome");
            assert_eq!(rows.len(), 1, "the actual original session remains durably represented");
            let (key, owner, state, coverage, accounting) = &rows[0];
            assert!(key == &stable_resource_id("apikeys", f.ids.key).to_string()
                && owner == &original.owner, "close cannot replace the original owner or APIkey");
            if state == "closed" && coverage == "unknown" && accounting == "awaiting_reconciliation" { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("plain transport close remains Unknown rather than complete zero");
}

async fn one_actual_aggregate(f: &mut Fixture, original: &LiveIdentity) {
    let event = tokio::time::timeout(LIMIT, f.usage.recv())
        .await
        .expect("the existing aggregate exporter receives the actual completed WS")
        .expect("the actual aggregate exporter stays connected");
    assert!(
        event.api_key_id == f.ids.key
            && event.model_id == f.ids.model
            && event.requested_model_id == f.ids.model
            && event.requested_model == ALIAS,
        "the exported aggregate retains the actual raw caller/model IDs and requested alias"
    );
    assert!(
        event.provider_key_id.as_deref() == Some(f.ids.provider)
            && event.upstream_model.as_deref() == Some(UPSTREAM)
            && event.inbound_protocol == "realtime",
        "the exported source is the exact configured original account and upstream model"
    );
    assert!(
        event.native_operation_id == Some(original.operation),
        "aggregate telemetry correlates to the actual original durable operation"
    );
    assert_eq!(
        (
            event.prompt_tokens,
            event.completion_tokens,
            event.cached_prompt_tokens,
            event.total_tokens
        ),
        (10, 3, 2, 13),
        "the real aggregate receives one nonzero response despite changed delivery ID"
    );
    assert!(
        matches!(f.usage.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "there is no parallel aggregate or per-response export in the existing session sink"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), f.usage.recv())
            .await
            .is_err(),
        "no delayed duplicate aggregate appears after the actual session has closed"
    );
}

async fn actual_nonzero_journal(f: &Fixture, original: &LiveIdentity) {
    let rows: Vec<(Option<String>, String)> =
        sqlx::query_as("SELECT response_id,usage FROM native_live_journal WHERE operation_id=?")
            .bind(original.operation.to_string())
            .fetch_all(&f.reader)
            .await
            .expect("observe actual supplier quantities in the public local journal schema");
    let mut nonempty = 0;
    for (response, usage) in rows {
        let raw: Value = serde_json::from_str(&usage).expect("the public journal usage JSON");
        assert!(
            raw.is_array(),
            "actual local journal usage is explicitly an array"
        );
        let observations: Vec<ObservedUsage> = serde_json::from_value(raw)
            .expect("existing public ObservedUsage DTO, as in the standalone-store tests");
        if observations.is_empty() {
            continue;
        }
        nonempty += 1;
        assert!(
            response.as_deref() == Some(RESPONSE) && observations.len() == 1,
            "the actual accepted main response has one independent durable usage fact"
        );
        let measured = &observations[0];
        assert!(
            measured.upstream_model == UPSTREAM,
            "the supplier quantity keeps its original configured model"
        );
        match &measured.meter {
            ObservedMeter::Tokens { usage } => {
                assert_eq!(
                    (
                        usage.prompt_tokens,
                        usage.completion_tokens,
                        usage.cached_prompt_tokens,
                        usage.total_tokens
                    ),
                    (10, 3, 2, 13),
                    "nonzero supplier tokens are actually durable, with cache remaining a subset"
                );
            }
            _ => {
                panic!("main response.done token usage must remain the existing public token meter")
            }
        }
    }
    assert_eq!(
        nonempty, 1,
        "different delivery IDs do not add another durable main response fact"
    );
}

async fn ordinary_journey(ids: Ids) {
    let mut f = Fixture::new(ids).await;
    let mut socket = f.connect().await;
    let session = next_event(&mut socket).await;
    assert!(
        session.get("type") == Some(&json!("session.created"))
            && session.pointer("/session/id") == Some(&json!(SESSION)),
        "an actual original provider session is established before trusting any observation"
    );
    assert!(f.peer.state.handshakes.lock().expect("actual provider handshake history").as_slice() == [true],
        "the actual upstream handshake uses the configured credential/project/static header/upstream model");
    let original = actual_live_identity(&f).await;
    let create = json!({"type":"response.create","event_id":"named_client_create",
        "response":{"output_modalities":["text"]}});
    socket
        .send(Message::Text(create.to_string()))
        .await
        .expect("send an actual response request through the ordinary raw-key WS");
    let accepted = next_event(&mut socket).await;
    assert!(
        accepted.get("type") == Some(&json!("response.created"))
            && accepted.pointer("/response/id") == Some(&json!(RESPONSE)),
        "the actual provider accepts nonzero work before reporting its result"
    );
    for delivery in ["named_response_done", "named_response_done_redelivery"] {
        let completed = next_event(&mut socket).await;
        let expected = response_done(delivery);
        assert!(completed.get("type") == Some(&json!("response.done"))
            && completed.get("event_id") == Some(&json!(delivery))
            && completed.pointer("/response/id") == Some(&json!(RESPONSE))
            && completed.pointer("/response/usage") == expected.pointer("/response/usage"),
            "both actual deliveries preserve the supplier response identity and complete nonzero usage");
    }
    assert!(
        socket.close(None).await.is_ok(),
        "close the actual ordinary client WS normally"
    );
    drop(socket);
    f.peer.drained().await;
    assert!(f.peer.state.inputs.lock().expect("complete drained provider history").as_slice() == [create],
        "the real provider reader received exactly the original client request, with no replay or filtered log");
    assert!(
        f.peer
            .state
            .handshakes
            .lock()
            .expect("complete provider handshake history")
            .as_slice()
            == [true],
        "the original configured provider performed the actual handshake once"
    );
    actual_unknown_close(&f, &original).await;
    actual_nonzero_journal(&f, &original).await;
    one_actual_aggregate(&mut f, &original).await;
    actual_metrics(&f).await;
}

// UUID 正控必须实际通过，才能把 named 分支失败定位为资源形态兼容 RED；不得跳过前者。
#[tokio::test]
async fn uuid_resources_reach_actual_ws_and_keep_original_local_owner_usage_and_unknown_close() {
    for id in [UUID_IDS.key, UUID_IDS.model, UUID_IDS.provider] {
        assert!(
            Uuid::parse_str(id).is_ok(),
            "the positive control really uses UUID resource IDs"
        );
    }
    ordinary_journey(UUID_IDS).await;
}

#[tokio::test]
async fn named_resources_reach_actual_ws_and_keep_original_local_owner_usage_and_unknown_close() {
    for id in [NAMED_IDS.key, NAMED_IDS.model, NAMED_IDS.provider] {
        assert!(
            Uuid::parse_str(id).is_err(),
            "the compatibility journey really uses non-UUID named resource IDs"
        );
    }
    ordinary_journey(NAMED_IDS).await;
}
