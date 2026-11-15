//! 仅现行 GA client_secrets：真实 protected SQLite/GW 与同账号 HTTP/WS 供应商组件。
//! 官方 SDK 3dae6090 与 Realtime 页签合同：成功200；transcription 只用 nested ASR，
//! intent=transcription 且省略 model；Translation 使用独立 family 与 model query。
//! token 本身是 bearer，不再要求持有人提供永久 key；不覆盖真实供应商/CP/计价或强身份证明。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_proxy::native_api::registry::{
    LiveUsageAuthority, NativeObjectRegistry, NativeRegistryError,
};
use aisix_proxy::native_api::standalone_store::StandaloneNativeStore;
use aisix_proxy::{build_router, ProxyState};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use futures::{future::BoxFuture, SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, Message};
use uuid::Uuid;

const KEY: &str = "33333333-3333-4333-8333-333333339902";
const OTHER_KEY: &str = "33333333-3333-4333-8333-333333339903";
const PROVIDER: &str = "22222222-2222-4222-8222-222222229902";
const PROJECT: &str = "proj_ga_secret_component";
const LIMIT: Duration = Duration::from_secs(10);
const STORE_KEY: [u8; 32] = [73; 32];
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Realtime,
    Transcription,
    Translation,
}

impl Purpose {
    fn name(self) -> &'static str {
        match self {
            Self::Realtime => "realtime",
            Self::Transcription => "transcription",
            Self::Translation => "translation",
        }
    }
    fn model_id(self) -> &'static str {
        match self {
            Self::Realtime => "11111111-1111-4111-8111-111111119911",
            Self::Transcription => "11111111-1111-4111-8111-111111119912",
            Self::Translation => "11111111-1111-4111-8111-111111119913",
        }
    }
    fn alias(self) -> &'static str {
        match self {
            Self::Realtime => "ga-public-main-A",
            Self::Transcription => "ga-public-asr-B",
            Self::Translation => "ga-public-translation-T",
        }
    }
    fn upstream(self) -> &'static str {
        match self {
            Self::Realtime => "gpt-realtime-2.1",
            Self::Transcription => "gpt-live-transcribe",
            Self::Translation => "gpt-realtime-translate",
        }
    }
    fn mint_path(self) -> &'static str {
        if self == Self::Translation {
            "/v1/realtime/translations/client_secrets"
        } else {
            "/v1/realtime/client_secrets"
        }
    }
    fn ws_path(self) -> &'static str {
        if self == Self::Translation {
            "/v1/realtime/translations"
        } else {
            "/v1/realtime"
        }
    }
    fn model_pointer(self) -> &'static str {
        if self == Self::Transcription {
            "/session/audio/input/transcription/model"
        } else {
            "/session/model"
        }
    }
    fn ttl(self) -> i64 {
        if self == Self::Translation {
            20
        } else {
            60
        }
    }
    fn request(self) -> Value {
        let session = match self {
            Self::Realtime => {
                json!({"type":"realtime","model":self.alias(),"instructions":"controlled GA defaults","max_output_tokens":64,"output_modalities":["text"]})
            }
            Self::Transcription => {
                json!({"type":"transcription","audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"model":self.alias()},"turn_detection":null}}})
            }
            Self::Translation => json!({"model":self.alias(),"audio":{"output":{"language":"es"}}}),
        };
        json!({"expires_after":{"anchor":"created_at","seconds":self.ttl()},"session":session})
    }
    fn query(self) -> String {
        if self == Self::Transcription {
            "intent=transcription".to_owned()
        } else {
            format!("model={}", self.alias())
        }
    }
    fn actual_session(self, id: &str) -> Value {
        let mut session = self.request()["session"].clone();
        // 初始供应商配置刻意与 mint 默认值不同，缺少真实 replay 不能靠同一 fixture 假绿。
        match self {
            Self::Realtime => {
                session["instructions"] = json!("initial provider instructions");
                session["max_output_tokens"] = json!(32);
            }
            Self::Transcription => session["audio"]["input"]["transcription"] = Value::Null,
            Self::Translation => session["audio"]["output"]["language"] = json!("fr"),
        }
        if self == Self::Transcription {
            session["object"] = json!("realtime.transcription_session");
        } else {
            session["model"] = json!(self.upstream());
            if self == Self::Realtime {
                session["object"] = json!("realtime.session");
            }
        }
        session["type"] = json!(self.name());
        session["id"] = json!(id);
        session["expires_at"] = json!(Utc::now().timestamp() + 3600);
        session
    }
}

struct MintTrace {
    body: Value,
    valid: bool,
    reply: Value,
}
struct ProviderSession {
    id: String,
    valid: bool,
    inputs: Mutex<Vec<Value>>,
    applied: Mutex<Vec<Value>>,
    finished: AtomicBool,
}
struct PeerState {
    purpose: Purpose,
    secret: String,
    mints: Mutex<Vec<MintTrace>>,
    sessions: Mutex<Vec<Arc<ProviderSession>>>,
}
struct Peer {
    base: String,
    state: Arc<PeerState>,
    task: tokio::task::JoinHandle<()>,
}

fn account_headers(headers: &HeaderMap, state: &PeerState) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {}", state.secret).as_str())
        && headers
            .get("openai-project")
            .and_then(|value| value.to_str().ok())
            == Some(PROJECT)
}

async fn supplier_mint(
    State(state): State<Arc<PeerState>>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let purpose = state.purpose;
    let valid = uri.path() == purpose.mint_path()
        && account_headers(&headers, &state)
        && body.pointer(purpose.model_pointer()) == Some(&json!(purpose.upstream()))
        && body.pointer("/expires_after/anchor") == Some(&json!("created_at"))
        && body.pointer("/expires_after/seconds") == Some(&json!(purpose.ttl()))
        && (purpose != Purpose::Realtime
            || body.pointer("/session/type") == Some(&json!("realtime")))
        && (purpose != Purpose::Transcription
            || (body.pointer("/session/model").is_none()
                && body.pointer("/session/type") == Some(&json!("transcription"))));
    let id = format!("sess_ga_minted_{}", Uuid::new_v4().simple());
    let mut session = body.get("session").cloned().unwrap_or(Value::Null);
    session["id"] = json!(id);
    session["type"] = json!(purpose.name());
    session["expires_at"] = json!(Utc::now().timestamp() + 3600);
    if purpose != Purpose::Translation {
        session["object"] = json!(if purpose == Purpose::Realtime {
            "realtime.session"
        } else {
            "realtime.transcription_session"
        });
    }
    let reply = json!({"value":format!("ek_controlled_{}",Uuid::new_v4().simple()),
        "expires_at":Utc::now().timestamp()+purpose.ttl(),"session":session});
    state
        .mints
        .lock()
        .expect("complete actual mint history")
        .push(MintTrace {
            body,
            valid,
            reply: reply.clone(),
        });
    if valid {
        (StatusCode::OK, Json(reply))
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"code":"component_contract"}})),
        )
    }
}

async fn supplier_ws(
    State(state): State<Arc<PeerState>>,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let purpose = state.purpose;
    let parameters = if purpose == Purpose::Transcription {
        query.get("intent").map(String::as_str) == Some("transcription")
            && !query.contains_key("model")
    } else {
        query.get("model").map(String::as_str) == Some(purpose.upstream())
    };
    let session = Arc::new(ProviderSession {
        id: format!("sess_ga_live_{}", Uuid::new_v4().simple()),
        valid: uri.path() == purpose.ws_path() && parameters && account_headers(&headers, &state),
        inputs: Mutex::new(Vec::new()),
        applied: Mutex::new(Vec::new()),
        finished: AtomicBool::new(false),
    });
    state
        .sessions
        .lock()
        .expect("complete actual provider handshake history")
        .push(session.clone());
    ws.on_upgrade(move |mut socket|async move {
        use axum::extract::ws::Message as Out;
        let mut configuration=purpose.actual_session(&session.id);
        assert!(socket.send(Out::Text(json!({"type":"session.created","event_id":format!("created_{}",session.id),"session":configuration}).to_string().into())).await.is_ok(),
            "send the real provider session identity");
        while let Some(message)=socket.recv().await {
            match message {
                Ok(Out::Text(text))=>{
                    let value:Value=serde_json::from_str(&text).expect("actual provider client JSON");
                    let kind=value.get("type").and_then(Value::as_str).expect("an explicit client event type").to_owned();
                    session.inputs.lock().expect("unreset complete provider input log").push(value.clone());
                    if kind=="session.update" {
                        for (name,value) in value.get("session").and_then(Value::as_object).expect("actual applied defaults") {
                            configuration[name]=value.clone();
                        }
                        session.applied.lock().expect("actual applied default configurations").push(configuration.clone());
                        assert!(socket.send(Out::Text(json!({"type":"session.updated","event_id":format!("updated_{}",session.id),"session":configuration}).to_string().into())).await.is_ok(),
                            "acknowledge only the configuration actually received and applied");
                    } else if kind=="response.create" {
                        let response=format!("resp_{}",session.id);
                        for event in [json!({"type":"response.created","event_id":format!("accepted_{response}"),"response":{"id":response,"object":"realtime.response","status":"in_progress","output":[]}}),
                            json!({"type":"response.done","event_id":format!("done_{response}"),"response":{"id":response,"object":"realtime.response","status":"completed","status_details":null,"output":[],"usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,"input_token_details":{"cached_tokens":2,"text_tokens":10},"output_token_details":{"text_tokens":3}}}})] {
                            assert!(socket.send(Out::Text(event.to_string().into())).await.is_ok(),"send actual accepted nonzero main work");
                        }
                    } else if kind=="input_audio_buffer.commit" && purpose==Purpose::Transcription {
                        let item=format!("item_{}",session.id);
                        for event in [json!({"type":"input_audio_buffer.committed","event_id":format!("committed_{item}"),"item_id":item,"previous_item_id":null}),
                            json!({"type":"conversation.item.input_audio_transcription.delta","event_id":format!("delta_{item}"),"item_id":item,"content_index":0,"delta":"controlled GA transcription"})] {
                            assert!(socket.send(Out::Text(event.to_string().into())).await.is_ok(),"send actual accepted transcription input and delta");
                        }
                    } else if kind=="input_audio_buffer.append" && purpose==Purpose::Translation {
                        let event_id=value.get("event_id").and_then(Value::as_str).expect("actual audio event identity");
                        assert!(socket.send(Out::Text(json!({"type":"session.output_audio.delta","event_id":format!("audio_{event_id}"),"delta":value["audio"],"channels":1,"format":"pcm16","sample_rate":24000,"elapsed_ms":20}).to_string().into())).await.is_ok(),
                            "return actual translated PCM component output");
                    }
                }
                Ok(Out::Ping(bytes))=>{if socket.send(Out::Pong(bytes)).await.is_err(){break;}}
                Ok(Out::Close(_))|Err(_)=>break,
                _=>{}
            }
        }
        session.finished.store(true,Ordering::Release);
    })
}

impl Peer {
    async fn new(purpose: Purpose) -> Self {
        let state = Arc::new(PeerState {
            purpose,
            secret: format!("sk_ga_component_{}", Uuid::new_v4().simple()),
            mints: Mutex::new(Vec::new()),
            sessions: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the single-account HTTP/WS peer");
        let base = format!("http://{}", listener.local_addr().expect("peer address"));
        let router = Router::new()
            .route("/v1/realtime/client_secrets", post(supplier_mint))
            .route(
                "/v1/realtime/translations/client_secrets",
                post(supplier_mint),
            )
            .route("/v1/realtime", get(supplier_ws))
            .route("/v1/realtime/translations", get(supplier_ws))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the controlled same-account HTTP/WS provider");
        });
        Self { base, state, task }
    }
    fn counts(&self) -> (usize, usize) {
        (
            self.state.mints.lock().expect("mint history").len(),
            self.state
                .sessions
                .lock()
                .expect("actual provider handshakes")
                .len(),
        )
    }
    fn input_history(&self) -> Vec<(String, Vec<Value>)> {
        let sessions = self
            .state
            .sessions
            .lock()
            .expect("complete session history")
            .clone();
        sessions
            .iter()
            .map(|session| {
                (
                    session.id.clone(),
                    session
                        .inputs
                        .lock()
                        .expect("complete application input history")
                        .clone(),
                )
            })
            .collect()
    }
    fn session(&self, id: &str) -> Arc<ProviderSession> {
        self.state
            .sessions
            .lock()
            .expect("actual session history")
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .expect("the actual forwarded session has a matching actual provider reader")
    }
    async fn drained(&self) {
        tokio::time::timeout(LIMIT, async {
            loop {
                if self
                    .state
                    .sessions
                    .lock()
                    .expect("all actual provider readers")
                    .iter()
                    .all(|s| s.finished.load(Ordering::Acquire))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("every actual provider reader drains to close, without clearing its history");
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// 仅第4例使用的旧对象协议 facade：真实 store 仍完成基础归属，RT 方法保持 trait 默认不可用。
// 这不是 CP、mTLS 或真实旧二进制；不会更换 source、伪造许可或假装提供 realtime 能力。
struct ObjectOnlyLegacy {
    actual: Arc<StandaloneNativeStore>,
    successful_begins: AtomicUsize,
}

macro_rules! forward_object {
    ($name:ident,$request:ty) => {
        fn $name(
            &self,
            request: $request,
        ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            self.actual.$name(request)
        }
    };
}

impl NativeObjectRegistry for ObjectOnlyLegacy {
    fn id_key(&self) -> Option<[u8; 32]> {
        self.actual.id_key()
    }
    fn resource_id(&self, kind: &str, raw: &str) -> Result<Uuid, NativeRegistryError> {
        self.actual.resource_id(kind, raw)
    }
    fn begin(
        &self,
        request: BeginRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let reply = self.actual.begin(request).await?;
            self.successful_begins.fetch_add(1, Ordering::AcqRel);
            Ok(reply)
        })
    }
    forward_object!(bind, BindRequest);
    forward_object!(lookup, LookupRequest);
    forward_object!(owned, OwnedRequest);
    forward_object!(tombstone, TombstoneRequest);
    forward_object!(claim, ClaimRequest);
    forward_object!(renew, LeaseRequest);
    forward_object!(reschedule, LeaseRequest);
    forward_object!(complete, CompleteRequest);
    // 不覆盖 live_usage_authority 或七个 realtime 方法；默认 Unsupported/Unavailable 必须被遵守。
}

struct Fixture {
    purpose: Purpose,
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    peer: Peer,
    client: reqwest::Client,
    bearer: String,
    other_bearer: String,
    reader: SqlitePool,
    snapshot: SnapshotHandle<AisixSnapshot>,
    legacy: Option<Arc<ObjectOnlyLegacy>>,
    observed_live: Mutex<std::collections::BTreeSet<String>>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(purpose: Purpose) -> Self {
        Self::configured(purpose, false).await
    }
    async fn configured(purpose: Purpose, object_only: bool) -> Self {
        let peer = Peer::new(purpose).await;
        let bearer = format!("sk_ga_owner_{}", Uuid::new_v4().simple());
        let other_bearer = format!("sk_ga_other_owner_{}", Uuid::new_v4().simple());
        let snapshot = AisixSnapshot::new();
        let provider:ProviderKey=serde_json::from_value(json!({"display_name":"GA-original-account","provider":"openai","adapter":"openai","api_base":peer.base,"api_key":peer.state.secret,"request":{"default_headers":{"OpenAI-Project":PROJECT}}})).expect("the public provider document");
        snapshot
            .provider_keys
            .insert(ResourceEntry::new(PROVIDER, provider, 1));
        let model:Model=serde_json::from_value(json!({"display_name":purpose.alias(),"provider":"openai","model_name":purpose.upstream(),"provider_key_id":PROVIDER})).expect("the single actual purpose-specific main model");
        snapshot
            .models
            .insert(ResourceEntry::new(purpose.model_id(), model, 1));
        let endpoints = if object_only {
            vec!["realtime", "models", "responses"]
        } else {
            vec!["realtime", "models"]
        };
        for (id, plaintext) in [(KEY, &bearer), (OTHER_KEY, &other_bearer)] {
            let key:ApiKey=serde_json::from_value(json!({"key_hash":ApiKey::hash_bearer(plaintext),"allowed_model_ids":[purpose.model_id()],"allowed_endpoints":endpoints})).expect("each actual creator grants only the selected main model");
            snapshot.apikeys.insert(ResourceEntry::new(id, key, 1));
        }
        let snapshot = SnapshotHandle::new(snapshot);
        let directory = tempfile::tempdir().expect("owned protected-store directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("protect the owned directory");
        let path = directory.path().join("authority.sqlite3");
        drop(
            std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(&path)
                .expect("create the protected empty SQLite file"),
        );
        let authority = StandaloneNativeStore::open(&path, STORE_KEY, snapshot.clone())
            .await
            .unwrap_or_else(|_| {
                panic!("the real standalone authority opens on its protected file")
            });
        let authority = Arc::new(authority);
        let legacy = object_only.then(|| {
            Arc::new(ObjectOnlyLegacy {
                actual: authority.clone(),
                successful_begins: AtomicUsize::new(0),
            })
        });
        let backend: Arc<dyn NativeObjectRegistry> = if let Some(legacy) = &legacy {
            legacy.clone()
        } else {
            authority
        };
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
                .expect("owned SQLite file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let reader = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .read_only(true)
                    .create_if_missing(false),
            )
            .await
            .expect("read-only observation of the actual SQLite authority");
        let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"}))
            .expect("ordinary gateway configuration");
        let hub = Arc::new(Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
        let state = ProxyState::new(snapshot.clone(), hub, &config)
            .without_cache()
            .with_guardrail_index(guardrails)
            .with_native_objects(backend);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the actual GA gateway router");
        let address = listener.local_addr().expect("gateway address");
        let router = build_router(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the actual GA gateway HTTP/WS router");
        });
        let client = reqwest::Client::builder()
            .timeout(LIMIT)
            .build()
            .expect("ordinary loopback HTTP client");
        Self {
            purpose,
            address,
            task,
            peer,
            client,
            bearer,
            other_bearer,
            reader,
            snapshot,
            legacy,
            observed_live: Mutex::new(std::collections::BTreeSet::new()),
            _directory: directory,
        }
    }

    async fn mint(&self) -> Secret {
        self.mint_as(KEY, &self.bearer).await
    }

    async fn mint_as(&self, creator: &str, bearer: &str) -> Secret {
        let response = self
            .client
            .post(format!(
                "http://{}{}",
                self.address,
                self.purpose.mint_path()
            ))
            .bearer_auth(bearer)
            .json(&self.purpose.request())
            .send()
            .await
            .unwrap_or_else(|_| panic!("GA mint loopback transport failed"));
        assert_eq!(
            response.status().as_u16(),
            200,
            "current official GA client_secrets succeeds with HTTP200"
        );
        let body: Value = response
            .json()
            .await
            .expect("GA success JSON without printing private bodies");
        let value = body
            .get("value")
            .and_then(Value::as_str)
            .expect("an explicitly present gateway-only temporary bearer")
            .to_owned();
        let expiry = body
            .get("expires_at")
            .and_then(Value::as_i64)
            .expect("an explicitly present Unix expiry");
        assert!(
            value.starts_with("aisix_rt_") && expiry > Utc::now().timestamp(),
            "the gateway returns its own live temporary bearer"
        );
        let mints = self
            .peer
            .state
            .mints
            .lock()
            .expect("actual original-account mint history");
        let actual = mints
            .last()
            .expect("an actual provider mint happened before success");
        assert!(actual.valid,"the official path/body/model slot and original account/project reached the actual provider");
        let provider_value = actual
            .reply
            .get("value")
            .and_then(Value::as_str)
            .expect("controlled provider secret");
        let wire = body.to_string();
        assert!(
            !wire.contains(provider_value) && !wire.contains(&self.peer.state.secret),
            "provider credentials never appear in the client envelope"
        );
        assert!(
            expiry
                <= actual.reply["expires_at"]
                    .as_i64()
                    .expect("actual provider expiry"),
            "the gateway temporary bearer cannot outlive its supplier limit"
        );
        assert!(
            body.pointer("/session/id") == actual.reply.pointer("/session/id"),
            "the provider's nonempty session metadata remains intact"
        );
        match self.purpose {
            Purpose::Realtime => assert!(
                body.pointer("/session/instructions") == Some(&json!("controlled GA defaults"))
                    && body.pointer("/session/max_output_tokens") == Some(&json!(64)),
                "nonzero effective default metadata survives gateway-only credential replacement"
            ),
            Purpose::Transcription => assert!(
                actual.body.pointer("/session/model").is_none()
                    && actual
                        .body
                        .pointer("/session/audio/input/transcription/model")
                        == Some(&json!(self.purpose.upstream())),
                "the only ASR is the granted main model in the formal nested slot"
            ),
            Purpose::Translation => assert!(
                body.pointer("/session/audio/output/language") == Some(&json!("es")),
                "effective translation language metadata is retained"
            ),
        }
        drop(mints);
        let identity = self.credential(&value, expiry, creator, bearer).await;
        Secret {
            value,
            expiry,
            identity,
        }
    }

    async fn credential(
        &self,
        value: &str,
        expiry: i64,
        creator: &str,
        bearer: &str,
    ) -> CredentialIdentity {
        let rows:Vec<(String,String,String,String,String,String,i64,String)>=sqlx::query_as("SELECT c.credential_id,c.operation_id,c.key_id,c.owner,c.creator_key_hash,c.purpose,c.expiry_ms,o.submission FROM native_realtime_credentials c JOIN native_operations o ON o.operation_id=c.operation_id WHERE c.token_hash=?")
            .bind(ApiKey::hash_bearer(value)).fetch_all(&self.reader).await.expect("observe the real credential identity without writing authority rows");
        assert_eq!(
            rows.len(),
            1,
            "the actual mint created one credential for its opaque token hash"
        );
        let (id, operation, key, owner, key_hash, purpose, expiry_ms, submission) = &rows[0];
        assert!(
            Uuid::parse_str(id).is_ok_and(|value| !value.is_nil())
                && Uuid::parse_str(operation).is_ok_and(|value| !value.is_nil())
                && !owner.is_empty(),
            "actual durable credential/operation/opaque owner are present"
        );
        assert!(
            key == creator
                && key_hash == &ApiKey::hash_bearer(bearer)
                && purpose == self.purpose.name()
                && expiry_ms / 1000 == expiry,
            "mint remains bound to the original creator, exact purpose and actual returned expiry"
        );
        // Store owner 已确认此公开持久槽为原 BeginRequest；不解私有 Owner 或构造身份证明。
        let begin: BeginRequest = serde_json::from_str(submission)
            .expect("the owner-confirmed public BeginRequest submission slot");
        self.assert_main(&begin, creator);
        CredentialIdentity {
            id: id.clone(),
            owner: owner.clone(),
            key: creator.to_owned(),
        }
    }

    fn assert_main(&self, begin: &BeginRequest, creator: &str) {
        assert!(begin.api_key_id.to_string()==creator && begin.requested_model_id.to_string()==self.purpose.model_id() && begin.origin.model_id.to_string()==self.purpose.model_id() && begin.origin.provider_key_id.to_string()==PROVIDER && begin.upstream_model==self.purpose.upstream(),"actual native admission keeps the original creator and the one authorized purpose-specific main source");
    }

    async fn connect(&self, secret: &Secret) -> (Socket, Arc<ProviderSession>) {
        let request = self.ws_request(&secret.value, self.purpose.ws_path(), &self.purpose.query());
        let result = tokio_tungstenite::connect_async(request).await;
        let (mut socket, response) = match result {
            Ok(pair) => pair,
            Err(tokio_tungstenite::tungstenite::Error::Http(reply)) => panic!(
                "GA positive WS admission failed: status={}",
                reply.status().as_u16()
            ),
            Err(_) => panic!("GA positive WS transport failed"),
        };
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        let created = event(&mut socket, "session.created").await;
        let id = created
            .pointer("/session/id")
            .and_then(Value::as_str)
            .expect("the actual provider session identity");
        let actual = self.peer.session(id);
        assert!(actual.valid,"the actual same-account WS uses the official purpose query and configured authentication/project");
        event(&mut socket, "session.updated").await;
        let applied = actual
            .applied
            .lock()
            .expect("actual applied defaults")
            .clone();
        assert_eq!(
            applied.len(),
            1,
            "exactly one actual default configuration was applied before work"
        );
        let inputs = actual
            .inputs
            .lock()
            .expect("actual provider input history")
            .clone();
        let update = inputs
            .first()
            .expect("an actual replay frame preceded its acknowledgment");
        assert!(
            update.get("type") == Some(&json!("session.update")),
            "the acknowledgment belongs to actual replayed session defaults"
        );
        let update_wire = update.to_string();
        assert!(
            !update_wire.contains(&secret.value) && !update_wire.contains(&self.peer.state.secret),
            "configuration replay contains neither gateway nor provider authentication secret"
        );
        for mint in self
            .peer
            .state
            .mints
            .lock()
            .expect("actual provider credential history")
            .iter()
        {
            assert!(
                !update_wire.contains(
                    mint.reply["value"]
                        .as_str()
                        .expect("controlled provider secret")
                ),
                "provider temporary secrets are not replayed as session configuration"
            );
        }
        match self.purpose{
            Purpose::Realtime=>assert!(applied[0].get("instructions")==Some(&json!("controlled GA defaults")) && applied[0].get("max_output_tokens")==Some(&json!(64)),"the actual provider has the nonzero effective realtime defaults"),
            Purpose::Transcription=>assert!(applied[0].get("model").is_none() && applied[0].pointer("/audio/input/transcription/model")==Some(&json!(self.purpose.upstream())) && applied[0].get("type")==Some(&json!("transcription")),"transcription defaults use only the nested main ASR model, without dummy top-level model"),
            Purpose::Translation=>assert!(applied[0].pointer("/audio/output/language")==Some(&json!("es")),"the actual translation provider applied the retained output language"),
        }
        self.live(secret).await;
        (socket, actual)
    }

    fn ws_request(
        &self,
        token: &str,
        path: &str,
        query: &str,
    ) -> tokio_tungstenite::tungstenite::handshake::client::Request {
        let mut request = format!("ws://{}{path}?{query}", self.address)
            .into_client_request()
            .expect("official family WS request");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .expect("the temporary bearer header"),
        );
        request
    }

    async fn live(&self, secret: &Secret) {
        // 只记录 SELECT 已观察到的实际 operation；不作为许可、receipt 或续租状态。
        let previous = self
            .observed_live
            .lock()
            .expect("observed live operation identities")
            .clone();
        tokio::time::timeout(LIMIT, async {
            loop {
                let rows:Vec<(String,String,String,String,String,String)>=sqlx::query_as("SELECT s.operation_id,s.api_key_id,s.owner,s.purpose,s.transport,o.submission FROM native_live_sessions s JOIN native_operations o ON o.operation_id=s.operation_id WHERE s.credential_id=?")
                    .bind(&secret.identity.id).fetch_all(&self.reader).await.expect("observe actual successful redeem/live ownership");
                let fresh:Vec<_> = rows.iter().filter(|row| !previous.contains(&row.0)).collect();
                assert!(fresh.len() <= 1,
                    "one actual provider handshake cannot create multiple new credential-backed operations");
                if let Some((operation, key, owner, purpose, transport, submission)) = fresh.first().copied() {
                    assert!(Uuid::parse_str(operation).is_ok_and(|id| !id.is_nil())
                        && key == &secret.identity.key && owner == &secret.identity.owner
                        && purpose == self.purpose.name() && transport == "web_socket",
                        "redeem/live retain the original mint creator, opaque owner and exact purpose");
                    let begin:BeginRequest = serde_json::from_str(submission)
                        .expect("the confirmed public live-operation submission DTO");
                    self.assert_main(&begin, &secret.identity.key);
                    assert!(self.observed_live.lock().expect("observed actual operation IDs").insert(operation.clone()),
                        "the new actual session has a distinct original operation identity");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("the actual WS has one new durable credential-linked operation");
    }

    async fn denied_family_and_management(&self, secret: &Secret) {
        let before = self.peer.counts();
        let before_inputs = self.peer.input_history();
        let models = self
            .client
            .get(format!("http://{}/v1/models", self.address))
            .bearer_auth(&self.bearer)
            .send()
            .await
            .unwrap_or_else(|_| panic!("permanent-key ordinary HTTP positive transport"));
        assert_eq!(
            models.status().as_u16(),
            200,
            "the ordinary HTTP route is really present for the actual permanent key"
        );
        let denied = self
            .client
            .get(format!("http://{}/v1/models", self.address))
            .bearer_auth(&secret.value)
            .send()
            .await
            .unwrap_or_else(|_| panic!("temporary-bearer ordinary HTTP transport"));
        assert!(
            denied.status().is_client_error(),
            "temporary realtime bearer does not become an ordinary HTTP credential"
        );
        let other_path = if self.purpose == Purpose::Translation {
            "/v1/realtime"
        } else {
            "/v1/realtime/translations"
        };
        let query = format!("model={}", self.purpose.alias());
        self.denied_ws(self.ws_request(&secret.value, other_path, &query))
            .await;
        assert_eq!(
            self.peer.counts(),
            before,
            "different purpose families and ordinary HTTP cannot cause an extra provider request"
        );
        assert!(self.peer.input_history() == before_inputs,
            "purpose refusal cannot inject work into an already accepted provider connection either");
    }

    async fn denied_ws(&self, request: tokio_tungstenite::tungstenite::handshake::client::Request) {
        match tokio_tungstenite::connect_async(request).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert!(
                response.status().is_client_error(),
                "purpose or expiry rejects the actual HTTP Upgrade"
            ),
            _ => panic!(
                "purpose/expiry denial must be an actual HTTP client refusal before provider I/O"
            ),
        }
    }

    async fn reused(&self, secret: &Secret, number: usize) {
        let rows:Vec<(String,String,String)> =sqlx::query_as("SELECT operation_id,api_key_id,purpose FROM native_live_sessions WHERE credential_id=?")
            .bind(&secret.identity.id).fetch_all(&self.reader).await.expect("observe actual distinct reused live sessions");
        assert_eq!(
            rows.len(),
            number,
            "the same temporary bearer establishes multiple actual sessions before expiry"
        );
        let mut operations = std::collections::BTreeSet::new();
        for (operation, key, purpose) in rows {
            assert!(
                key == secret.identity.key && purpose == self.purpose.name(),
                "all reused sessions keep the original creator/purpose"
            );
            assert!(
                operations.insert(operation),
                "reused sessions have independent actual operations"
            );
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct CredentialIdentity {
    id: String,
    owner: String,
    key: String,
}
struct Secret {
    value: String,
    expiry: i64,
    identity: CredentialIdentity,
}

async fn event(socket: &mut Socket, wanted: &str) -> Value {
    tokio::time::timeout(LIMIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let value: Value = serde_json::from_str(&text)
                        .expect("actual server JSON without displaying bodies");
                    assert!(
                        value.get("type").and_then(Value::as_str) == Some(wanted),
                        "the actual expected GA server event arrives before any denial or close"
                    );
                    return value;
                }
                Some(Ok(Message::Ping(bytes))) => {
                    assert!(
                        socket.send(Message::Pong(bytes)).await.is_ok(),
                        "answer the actual ping"
                    );
                }
                _ => panic!("the actual positive GA session closed before its expected event"),
            }
        }
    })
    .await
    .expect("a real provider event reaches the temporary-bearer client")
}

async fn main_work(socket: &mut Socket) {
    socket.send(Message::Text(json!({"type":"response.create","event_id":"ga_main_work","response":{"output_modalities":["text"]}}).to_string())).await.expect("actual temporary-bearer client requests nonzero work");
    event(socket, "response.created").await;
    let done = event(socket, "response.done").await;
    assert!(
        done.pointer("/response/usage/input_tokens") == Some(&json!(10))
            && done.pointer("/response/usage/output_tokens") == Some(&json!(3))
            && done.pointer("/response/usage/total_tokens") == Some(&json!(13)),
        "the temporary bearer really carries accepted nonzero provider work"
    );
}

async fn audio(socket: &mut Socket, marker: u8, id: &str) {
    socket.send(Message::Text(json!({"type":"input_audio_buffer.append","event_id":id,"audio":STANDARD.encode([marker;4800])}).to_string())).await.expect("actual 24kHz PCM16 audio through the admitted temporary session");
}

async fn translated_audio(socket: &mut Socket, marker: u8, id: &str) {
    audio(socket, marker, id).await;
    let output = event(socket, "session.output_audio.delta").await;
    assert!(
        output.get("event_id") == Some(&json!(format!("audio_{id}")))
            && output
                .get("delta")
                .and_then(Value::as_str)
                .is_some_and(|data| STANDARD
                    .decode(data)
                    .is_ok_and(|bytes| bytes == [marker; 4800]))
            && output.get("sample_rate") == Some(&json!(24000)),
        "the actual distinct PCM bytes and nonzero metadata survive both gateway directions"
    );
}

async fn close(mut socket: Socket) {
    assert!(
        socket.close(None).await.is_ok(),
        "close the actual admitted client WS"
    );
}

fn exact_inputs(session: &ProviderSession, expected: &[&str]) {
    assert!(session.inputs.lock().expect("complete drained input history").iter()
        .map(|event| event.get("type").and_then(Value::as_str))
        .eq(expected.iter().copied().map(Some)),
        "the actual drained provider history contains every authorized application input exactly once");
}

#[tokio::test]
async fn ga_realtime_secret_preserves_metadata_creator_and_reusable_purpose_bound_ws() {
    let f = Fixture::new(Purpose::Realtime).await;
    let first = f.mint().await;
    let second = f.mint().await;
    assert!(
        first.value != second.value && first.identity.id != second.identity.id,
        "separate actual mints produce different gateway-only random credentials"
    );
    for _ in 0..2 {
        let (mut socket, _) = f.connect(&first).await;
        main_work(&mut socket).await;
        close(socket).await;
    }
    f.reused(&first, 2).await;
    // 第二个真实永久 key 另行 mint；持 token 者仍只交这个 token，不交额外永久 key。
    let other = f.mint_as(OTHER_KEY, &f.other_bearer).await;
    assert!(
        other.value != first.value && other.identity.key != first.identity.key,
        "different real creators keep distinct temporary credentials and their own public creator key IDs"
    );
    let (mut other_socket, _) = f.connect(&other).await;
    main_work(&mut other_socket).await;
    close(other_socket).await;
    f.reused(&other, 1).await;
    f.denied_family_and_management(&first).await;
    f.peer.drained().await;
    for session in f
        .peer
        .state
        .sessions
        .lock()
        .expect("all actual realtime sessions")
        .iter()
    {
        exact_inputs(session, &["session.update", "response.create"]);
    }
    assert_eq!(
        f.peer.counts(),
        (3, 3),
        "all original-account mint and handshake history is preserved"
    );
}

#[tokio::test]
async fn ga_transcription_nested_primary_asr_secret_uses_intent_without_top_model_and_carries_audio(
) {
    let f = Fixture::new(Purpose::Transcription).await;
    let secret = f.mint().await;
    let (mut socket, session) = f.connect(&secret).await;
    audio(&mut socket, 7, "ga_transcription_audio").await;
    socket
        .send(Message::Text(
            json!({"type":"input_audio_buffer.commit","event_id":"ga_transcription_commit"})
                .to_string(),
        ))
        .await
        .expect("commit actual accepted input audio");
    let committed = event(&mut socket, "input_audio_buffer.committed").await;
    let delta = event(
        &mut socket,
        "conversation.item.input_audio_transcription.delta",
    )
    .await;
    let item = committed
        .get("item_id")
        .and_then(Value::as_str)
        .expect("the actual committed audio has an explicitly present item identity");
    assert!(
        !item.is_empty()
            && delta.get("item_id") == Some(&json!(item))
            && delta.get("content_index") == Some(&json!(0))
            && delta.get("delta") == Some(&json!("controlled GA transcription")),
        "actual accepted audio has a matching nonempty transcript delta"
    );
    close(socket).await;
    f.reused(&secret, 1).await;
    f.denied_family_and_management(&secret).await;
    f.peer.drained().await;
    let inputs = session
        .inputs
        .lock()
        .expect("complete drained transcription inputs");
    assert!(
        inputs
            .iter()
            .map(|event| event.get("type").and_then(Value::as_str))
            .eq([
                Some("session.update"),
                Some("input_audio_buffer.append"),
                Some("input_audio_buffer.commit")
            ]),
        "the real provider received the official defaults and actual audio/commit once"
    );
    assert!(
        inputs[1].get("audio") == Some(&json!(STANDARD.encode([7u8; 4800]))),
        "actual audio bytes reached the original provider reader"
    );
    assert_eq!(
        f.peer.counts(),
        (1, 1),
        "no deprecated mint or extra purpose connection is hidden from the full history"
    );
}

#[tokio::test]
async fn ga_translation_secret_reuses_original_creator_and_expires_only_new_admission() {
    let f = Fixture::new(Purpose::Translation).await;
    let secret = f.mint().await;
    let (mut first, first_session) = f.connect(&secret).await;
    translated_audio(&mut first, 11, "ga_translation_first").await;
    let (mut second, second_session) = f.connect(&secret).await;
    translated_audio(&mut second, 13, "ga_translation_second").await;
    f.reused(&secret, 2).await;
    f.denied_family_and_management(&secret).await;
    // 真实 UTC 自然越过公开返回的到期时间；不暂停时钟、不改 DB expiry 或 lease。
    tokio::time::timeout(Duration::from_secs(25), async {
        while Utc::now().timestamp_millis() <= secret.expiry * 1000 + 100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the legal20second temporary secret really reaches its wall-clock expiry");
    translated_audio(
        &mut first,
        17,
        "ga_translation_already_accepted_after_expiry",
    )
    .await;
    let before = f.peer.counts();
    let before_inputs = f.peer.input_history();
    f.denied_ws(f.ws_request(&secret.value, f.purpose.ws_path(), &f.purpose.query()))
        .await;
    assert_eq!(
        f.peer.counts(),
        before,
        "an expired secret cannot dial another provider connection"
    );
    assert!(
        f.peer.input_history() == before_inputs,
        "expired new admission cannot reuse an established source to inject extra input"
    );
    close(first).await;
    close(second).await;
    f.peer.drained().await;
    exact_inputs(
        &first_session,
        &[
            "session.update",
            "input_audio_buffer.append",
            "input_audio_buffer.append",
        ],
    );
    exact_inputs(
        &second_session,
        &["session.update", "input_audio_buffer.append"],
    );
    f.reused(&secret, 2).await;
    assert_eq!(
        f.peer.counts(),
        (1, 2),
        "the original-account full history has one mint and two pre-expiry real sessions only"
    );
}

// 基础对象协议真实可用不等于支持新 RT；能力不足应在供应商 mint 之前被发现。
#[tokio::test]
async fn object_only_legacy_backend_rejects_ga_mint_before_any_supplier_request() {
    let f = Fixture::configured(Purpose::Realtime, true).await;
    let legacy = f
        .legacy
        .as_ref()
        .expect("the explicit object-only protocol facade");
    assert_eq!(
        legacy.live_usage_authority(),
        LiveUsageAuthority::Unsupported
    );
    assert!(legacy.id_key()==Some(STORE_KEY),"the old-object backend has the actual original source key, not a missing-registry shortcut");
    for (kind, raw) in [
        ("apikeys", KEY),
        ("models", f.purpose.model_id()),
        ("provider_keys", PROVIDER),
    ] {
        assert!(
            legacy
                .resource_id(kind, raw)
                .is_ok_and(|id| id.to_string() == raw),
            "the actual original object backend resolves configured resource identities"
        );
    }
    let snapshot = f.snapshot.load();
    let provider = snapshot
        .provider_keys
        .get_by_id(PROVIDER)
        .expect("the actual original account resource");
    let kind = NativeObjectKind::Response;
    let request = BeginRequest {
        upstream_input_text: None,
        input_file_id: None,
        operation_id: Uuid::new_v4(),
        api_key_id: Uuid::parse_str(KEY).expect("fixture creator UUID"),
        protocol: NativeProtocol::Openai,
        kind,
        origin: OriginBinding {
            surface: Some(native_origin_surface(NativeProtocol::Openai, kind)),
            model_id: Uuid::parse_str(f.purpose.model_id()).expect("actual main model UUID"),
            provider_key_id: Uuid::parse_str(PROVIDER).expect("actual provider UUID"),
            boundary: configured_native_origin(
                &STORE_KEY,
                Uuid::parse_str(PROVIDER).expect("actual provider UUID"),
                &provider.value,
                NativeProtocol::Openai,
                kind,
            )
            .expect("the existing public configured object-origin setup helper"),
        },
        parent_id: None,
        requested_model_id: Uuid::parse_str(f.purpose.model_id()).expect("actual main UUID"),
        requested_model: f.purpose.alias().to_owned(),
        upstream_model: f.purpose.upstream().to_owned(),
        upstream_provider: "openai".to_owned(),
        operation: kind.as_str().to_owned(),
        request_id: Uuid::new_v4().to_string(),
        asynchronous: false,
        requested_service_tier: None,
        entries: Vec::new(),
        observation_headers: Default::default(),
    };
    // 用已有 store 测试的公开 Begin 契约证明 facade 确实转发到了真实授权/文件事务。
    let reply = legacy
        .begin(request.clone())
        .await
        .unwrap_or_else(|_| panic!("the old basic-object Begin positive must really succeed"));
    assert!(
        reply.state == "pending_submission"
            && reply.operation_id == Some(request.operation_id)
            && reply.origin == Some(request.origin),
        "the real object authority accepted the actual original owner and configured origin"
    );
    let persisted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_operations WHERE operation_id=?")
            .bind(request.operation_id.to_string())
            .fetch_one(&f.reader)
            .await
            .expect("observe the real successful basic-object Begin");
    assert_eq!(
        persisted, 1,
        "the basic protocol positive is durable, not a constant fake reply"
    );
    assert!(
        legacy.successful_begins.load(Ordering::Acquire) >= 1,
        "the actual facade forwarded a successful real Begin"
    );
    let before = f.peer.counts();
    let before_inputs = f.peer.input_history();
    let response = f
        .client
        .post(format!("http://{}{}", f.address, f.purpose.mint_path()))
        .bearer_auth(&f.bearer)
        .json(&f.purpose.request())
        .send()
        .await
        .unwrap_or_else(|_| panic!("actual legacy-backend GA request transport"));
    assert_eq!(
        response.status().as_u16(),
        503,
        "a backend without RT support refuses GA mint locally"
    );
    assert!(
        f.peer.counts() == before && f.peer.input_history() == before_inputs,
        "unsupported RT does not spend a supplier mint before discovering defaultUnavailable"
    );
    assert_eq!(
        before,
        (0, 0),
        "the real basic-object Begin itself did not call the supplier"
    );
}
