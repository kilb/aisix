//! 真实 CP、Postgres、kine、mTLS 与进程内生产网关路由的轻隔离组件测试。
//! 只有上游 HTTP 是脚本替身；不称为真实供应商或网关二进制 E2E。
//! 归属期望来自 2026-11-15 已确认合同，不从产品实现反推。
//!
//! 官方线协议依据：openai/openai-python@3dae6090cc31cc64a437fe0bcee740b6d81ed2cc
//! src/openai/types/responses/{response_create_params,response,response_output_message,response_usage}.py
//! conversation 是正式请求槽，Response.conversation 是可省略的响应字段。
//! 模型支持：https://developers.openai.com/api/docs/models/gpt-6-sol.md
//! https://developers.openai.com/api/docs/models/gpt-6-luna.md

#[allow(dead_code)]
#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aisix_control_plane::auth::OperatorRole;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::ProxyConfig;
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use http_body_util::{BodyExt, Limited};
use native_gateway::{projected_snapshot, HttpRegistry};
use serde_json::{json, Value};
use support::{Bundle, OperatorSession, TestCp, TEST_PASSWORD};
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

const MODEL_A: &str = "conversation-light-model-a";
const MODEL_B: &str = "conversation-light-model-b";
const UPSTREAM_A: &str = "gpt-6-sol";
const UPSTREAM_B: &str = "gpt-6-luna";
const UPSTREAM_KEY: &str = "sk-light-mapping-component-fixture";
const CONVERSATION: &str = "conv_light_owner_a";
const RESPONSE_ID: &str = "resp_light_caller_b";
const ITEM: &str = "msg_light_conversation_item";
const OUTPUT_TEXT: &str = "Completed. The text conv_unrelated_output is only text.";

fn uuid(body: &Value, pointer: &str) -> Uuid {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(|| panic!("[setup:decode] expected UUID at {pointer}"))
}

async fn create(
    cp: &TestCp,
    admin: &OperatorSession,
    phase: &str,
    path: &str,
    body: Value,
) -> Value {
    let reply = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(reply.status, 201, "[setup:{phase}] {reply:?}");
    reply.body
}

struct Caller {
    member: Uuid,
    key: Uuid,
    bearer: String,
}

// Full 只由现有管理员邀请接口赋予；不直接写成员、权限或 key 投影。
async fn caller(
    cp: &TestCp,
    admin: &OperatorSession,
    tenant: Uuid,
    role: &str,
    models: &[Uuid],
) -> Caller {
    let invited = create(
        cp,
        admin,
        "invite-member",
        &format!("/admin/tenants/{tenant}/invites"),
        json!({"email":format!("light-{role}-{}@tenant.test",Uuid::new_v4().simple()),"role":role}),
    )
    .await;
    let accepted = cp
        .send(
            "POST",
            "/api/invites/accept",
            None,
            Some(json!({"token":invited["invite_token"],"password":TEST_PASSWORD})),
        )
        .await;
    assert_eq!(accepted.status, 201, "[setup:accept-member] {accepted:?}");
    assert_eq!(accepted.body["member"]["role"], role, "[setup:member-role]");
    assert_eq!(
        accepted.body["tenant"]["id"],
        json!(tenant),
        "[setup:member-tenant]"
    );
    let member = uuid(&accepted.body, "/member/id");
    let group = support::world::group_id(cp, "shared").await;
    let key = create(
        cp,
        admin,
        "mint-key",
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":format!("light-{role}"),"member_id":member,"group_id":group,"model_ids":models}),
    )
    .await;
    Caller {
        member,
        key: uuid(&key, "/key/id"),
        bearer: key["plaintext"]
            .as_str()
            .expect("[setup:mint-key] actual bearer")
            .to_owned(),
    }
}

fn gateway(
    cp: &TestCp,
    bundle: &Bundle,
    snapshot: aisix_core::AisixSnapshot,
) -> (ProxyState, mpsc::Receiver<UsageEvent>) {
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0"})).expect("gateway configuration");
    let (tx, rx) = mpsc::channel(32);
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            &cp.state().pki.platform_id_key_hex(),
        ))
        .with_native_objects(Arc::new(HttpRegistry::new(cp, bundle)))
        .with_usage_sink(UsageSink::new(tx));
    (state, rx)
}

async fn send(
    state: &ProxyState,
    method: &str,
    path: &str,
    caller: &Caller,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {}", caller.bearer))
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .expect("gateway request");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("production router response");
    let status = response.status();
    let bytes = Limited::new(response.into_body(), 1 << 20)
        .collect()
        .await
        .expect("complete gateway response")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).expect("gateway response JSON"),
    )
}

#[derive(Clone)]
struct Wire {
    method: String,
    path: String,
    query: String,
    body: Value,
}

#[derive(Default)]
struct ScriptState {
    wire: Vec<Wire>,
    completed: bool,
}

struct Upstream {
    base: String,
    state: Arc<Mutex<ScriptState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Upstream {
    async fn start() -> Self {
        let state = Arc::new(Mutex::new(ScriptState::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("script upstream listener");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("script upstream address")
        );
        let app = Router::new().fallback(upstream).with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("script upstream server")
        });
        Self { base, state, task }
    }

    fn wire(&self) -> Vec<Wire> {
        self.state.lock().expect("script wire log").wire.clone()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn item() -> Value {
    json!({"id":ITEM,"type":"message","status":"completed","role":"assistant",
        "content":[{"type":"output_text","text":OUTPUT_TEXT,"annotations":[]}]})
}

fn response() -> Value {
    // 只用官方字段；省略可选 conversation echo，归属必须来自正式请求槽。
    json!({"id":RESPONSE_ID,"object":"response","created_at":1794614400,
        "status":"completed","model":UPSTREAM_B,"background":false,"store":true,
        "error":null,"incomplete_details":null,"instructions":null,
        "metadata":{"note":"conv_unrelated_metadata and resp_unrelated_metadata are only text"},
        "parallel_tool_calls":true,"tool_choice":"auto","tools":[],
        "output":[item()],"text":{"format":{"type":"text"}},
        "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
            "output_tokens":20,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":120}})
}

async fn upstream(
    State(state): State<Arc<Mutex<ScriptState>>>,
    request: Request<Body>,
) -> Response {
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {UPSTREAM_KEY}").as_str());
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let bytes = to_bytes(request.into_body(), 1 << 20)
        .await
        .expect("upstream request bytes");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("upstream request JSON")
    };
    let mut state = state.lock().expect("script state");
    state.wire.push(Wire {
        method: method.clone(),
        path: path.clone(),
        query,
        body: body.clone(),
    });
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match (method.as_str(), path.as_str()) {
        ("POST", "/v1/conversations") => Json(json!({"id":CONVERSATION,"object":"conversation","created_at":1794614400,"metadata":body["metadata"]})).into_response(),
        ("POST", "/v1/responses") if body["model"] == UPSTREAM_B && body["conversation"] == CONVERSATION => {
            state.completed = true;
            Json(response()).into_response()
        }
        ("GET", "/v1/responses/resp_light_caller_b") if state.completed => Json(response()).into_response(),
        ("GET", "/v1/conversations/conv_light_owner_a/items") if state.completed => Json(json!({"object":"list","data":[item()],"has_more":false,"first_id":ITEM,"last_id":ITEM})).into_response(),
        _ => (StatusCode::BAD_REQUEST, Json(json!({"error":{"type":"invalid_request_error","message":"Unexpected fixture request","param":null,"code":null}}))).into_response(),
    }
}

fn drain(rx: &mut mpsc::Receiver<UsageEvent>) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(serde_json::to_value(event).expect("actual gateway telemetry"));
    }
    events
}

async fn accounting(cp: &TestCp, tenant: Uuid) -> (i64, i64) {
    sqlx::query_as("SELECT count(*),COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'")
        .bind(tenant).fetch_one(cp.db().pool()).await.expect("read actual usage ledger")
}

async fn assert_paid_once(cp: &TestCp, tenant: Uuid) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let posted = accounting(cp, tenant).await;
            if posted.0 > 0 {
                assert_eq!(
                    posted,
                    (1, 280),
                    "[billing:paid-once] 100 input × 2 + 20 output × 4 at rate 1"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("[billing:paid-once] actual nonzero usage debit must arrive");
    let charge: (String, Option<i64>) = sqlx::query_as("SELECT c.status,c.charge_micro_cny FROM pricing_charges_cny c JOIN billing_ledger_cny l ON l.event_id=c.event_id WHERE l.tenant_id=$1 AND l.kind='usage'")
        .bind(tenant).fetch_one(cp.db().pool()).await.expect("actual pricing charge behind ledger");
    assert_eq!(
        charge,
        ("charged".into(), Some(280)),
        "[billing:price-model-b]"
    );
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(tenant)
            .fetch_one(cp.db().pool())
            .await
            .expect("actual tenant balance");
    assert_eq!(balance, 1_000_000 - 280, "[billing:tenant-balance]");
}

// 删正式槽映射、偷换成模型 A、把 I 归 B 或给重复观察再收费，分别会使对应关卡变红。
#[tokio::test]
async fn full_member_response_uses_requested_model_and_preserves_conversation_item_owner_once() {
    assert!(
        std::env::var("CP_TEST_DATABASE_URL").is_ok(),
        "[setup:services] ROOT isolated CP_TEST_DATABASE_URL is required"
    );
    assert!(
        std::env::var("CP_TEST_KINE_ENDPOINT").is_ok(),
        "[setup:services] ROOT isolated CP_TEST_KINE_ENDPOINT is required"
    );
    let cp = TestCp::start_with_kine()
        .await
        .expect("[setup:services] real isolated Postgres and kine");
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let upstream = Upstream::start().await;
    let provider = create(
        &cp,
        &admin,
        "provider",
        "/admin/resources/provider_keys",
        json!({"doc":{
            "display_name":"light-mapping-account","provider":"openai","adapter":"openai",
            "api_base":format!("{}/v1",upstream.base),"api_key":UPSTREAM_KEY,"groups":["shared"]
        }}),
    )
    .await;
    let mut models = Vec::new();
    for (alias, upstream_model, input, output) in [
        (MODEL_A, UPSTREAM_A, "50", "70"),
        (MODEL_B, UPSTREAM_B, "2", "4"),
    ] {
        let model = create(&cp, &admin, "model", "/admin/resources/models", json!({"doc":{
            "display_name":alias,"provider":"openai","model_name":upstream_model,"provider_key_id":uuid(&provider,"/id")
        }})).await;
        let id = uuid(&model, "/id");
        create(&cp, &admin, "baseline", "/admin/pricing/baselines", json!({
            "model_id":id,"effective_from":support::world::db_now(&cp).await,
            "card":{"default":{"base":{"input":input,"output":output}}},"note":"轻隔离映射组件测试价格"
        })).await;
        models.push(id);
    }
    let group = support::world::group_id(&cp, "shared").await;
    create(
        &cp,
        &admin,
        "group-rate",
        &format!("/admin/provider-groups/{group}/rates"),
        json!({
            "model_id":null,"cny_per_usd":"1","effective_from":support::world::db_now(&cp).await
        }),
    )
    .await;
    let tenant = create(
        &cp,
        &admin,
        "tenant",
        "/admin/tenants",
        json!({"name":"light-conversation-mapping"}),
    )
    .await;
    let tenant_id = uuid(&tenant, "/id");
    let granted = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant_id}"),
            admin.cookie(),
            tenant["version"].as_i64().expect("tenant version"),
            json!({"model_scope":"selected","allowed_model_ids":models}),
        )
        .await;
    assert_eq!(granted.status, 200, "[setup:grant-both-models] {granted:?}");
    create(
        &cp,
        &admin,
        "credit",
        &format!("/admin/tenants/{tenant_id}/credits"),
        json!({"amount_cny":"1","reason":"轻隔离映射组件测试预付余额"}),
    )
    .await;
    let a = caller(&cp, &admin, tenant_id, "member", &models).await;
    let b = caller(&cp, &admin, tenant_id, "admin", &models).await;
    let bundle = cp
        .issue_bundle(&admin, "light-conversation-mapping-gateway")
        .await;
    // 不伪造心跳、版本或快照；使用控制面实际投影与网关正式加载器。
    let (gateway, mut usage) = gateway(&cp, &bundle, projected_snapshot(&cp).await);
    let conversation_body = json!({"metadata":{"owner":"member-a"}});
    let (status, conversation) = send(
        &gateway,
        "POST",
        &format!("/v1/conversations?model={MODEL_A}"),
        &a,
        Some(conversation_body.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "[runtime:create-conversation] {conversation}"
    );
    assert_eq!(
        conversation,
        json!({"id":CONVERSATION,"object":"conversation","created_at":1794614400,"metadata":conversation_body["metadata"]}),
        "[wire:conversation]"
    );
    let request_body = json!({"model":MODEL_B,"conversation":CONVERSATION,"store":true,"stream":false,
        "input":"Complete this request. conv_unrelated_input and resp_unrelated_input are only text.",
        "metadata":{"note":"conv_unrelated_request is only metadata"}});
    let (status, returned) = send(
        &gateway,
        "POST",
        "/v1/responses",
        &b,
        Some(request_body.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "[runtime:response-dispatch] {returned}"
    );
    let mut expected = response();
    expected["model"] = json!(MODEL_B);
    assert_eq!(
        returned, expected,
        "[wire:response-and-alias] preserve all official metadata, text and usage"
    );
    let wire = upstream.wire();
    let creates: Vec<_> = wire
        .iter()
        .filter(|request| request.method == "POST" && request.path == "/v1/responses")
        .collect();
    assert_eq!(creates.len(), 1, "[wire:one-paid-dispatch]");
    for field in ["conversation", "store", "stream", "input", "metadata"] {
        assert_eq!(
            creates[0].body.get(field),
            request_body.get(field),
            "[wire:formal-request-slot] {field}"
        );
    }
    assert_eq!(
        creates[0].body.get("model"),
        Some(&json!(UPSTREAM_B)),
        "[wire:execute-model-b]"
    );
    let create_conversation = wire
        .iter()
        .find(|request| request.method == "POST" && request.path == "/v1/conversations")
        .expect("[wire:conversation-create] actual upstream request");
    assert_eq!(
        create_conversation.body, conversation_body,
        "[wire:conversation-create]"
    );
    assert!(
        create_conversation.query.is_empty(),
        "[wire:conversation-selector] routing selector stays gateway-side"
    );
    let telemetry = drain(&mut usage);
    let paid: Vec<_> = telemetry
        .iter()
        .filter(|event| {
            event["prompt_tokens"].as_u64().unwrap_or(0) > 0
                || event["completion_tokens"].as_u64().unwrap_or(0) > 0
        })
        .collect();
    assert_eq!(
        paid.len(),
        1,
        "[billing:actual-response-usage] one actual nonzero gateway event"
    );
    for (field, value) in [
        ("api_key_id", json!(b.key)),
        ("user_id", json!(b.member)),
        ("requested_model_id", json!(models[1])),
        ("model_id", json!(models[1])),
        ("operation", json!("responses")),
        ("prompt_tokens", json!(100)),
        ("completion_tokens", json!(20)),
    ] {
        assert_eq!(
            paid[0].get(field),
            Some(&value),
            "[billing:actual-response-usage] {field}"
        );
    }
    let dp = cp.dp_client(&bundle);
    let delivered = support::world::deliver(&dp, Uuid::new_v4(), telemetry.clone()).await;
    assert_eq!(delivered.status, 200, "[billing:real-intake] {delivered:?}");
    assert_eq!(
        delivered.body.get("rejected"),
        Some(&json!(0)),
        "[billing:real-intake]"
    );
    assert_paid_once(&cp, tenant_id).await;

    // 账务正向流程通过后才检查归属，且必须早于任何 C.items 的发现读取。
    let conversations: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as("SELECT id,tenant_id,api_key_id FROM native_objects WHERE environment_id=$1 AND kind='conversation' AND raw_id=$2")
        .bind(cp.env().env_id).bind(CONVERSATION).fetch_all(cp.db().pool()).await.expect("[ownership:conversation] durable rows");
    assert_eq!(
        conversations.len(),
        1,
        "[ownership:conversation] one durable C"
    );
    assert_eq!(
        (conversations[0].1, conversations[0].2),
        (tenant_id, a.key),
        "[ownership:conversation] A remains owner"
    );
    let roots: Vec<(Uuid, Uuid, Uuid, Uuid, Option<Uuid>, Value)> = sqlx::query_as("SELECT n.id,n.tenant_id,n.api_key_id,n.operation_id,o.member_id,o.submission FROM native_objects n JOIN native_operations o ON o.environment_id=n.environment_id AND o.operation_id=n.operation_id WHERE n.environment_id=$1 AND n.kind='response' AND n.raw_id=$2")
        .bind(cp.env().env_id).bind(RESPONSE_ID).fetch_all(cp.db().pool()).await.expect("[ownership:response-root] durable rows");
    assert_eq!(roots.len(), 1, "[ownership:response-root] one durable R");
    assert_eq!(
        (roots[0].1, roots[0].2, roots[0].4),
        (tenant_id, b.key, Some(b.member)),
        "[ownership:response-root] B owns and called R"
    );
    for (pointer, value) in [
        ("/api_key_id", json!(b.key)),
        ("/requested_model_id", json!(models[1])),
        ("/requested_model", json!(MODEL_B)),
        ("/origin/model_id", json!(models[1])),
        ("/upstream_model", json!(UPSTREAM_B)),
    ] {
        assert_eq!(
            roots[0].5.pointer(pointer),
            Some(&value),
            "[ownership:accepted-model-b] {pointer}"
        );
    }
    let children: Vec<(Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as("SELECT n.id,m.parent_id,n.tenant_id,n.api_key_id FROM native_objects n JOIN native_object_memberships m ON m.environment_id=n.environment_id AND m.child_id=n.id WHERE n.environment_id=$1 AND n.kind='response_item' AND n.raw_id=$2 AND m.relation='conversation_item' AND m.deleted_at IS NULL")
        .bind(cp.env().env_id).bind(ITEM).fetch_all(cp.db().pool()).await.expect("[ownership:conversation-item] durable rows before first list");
    assert_eq!(children.len(), 1, "[ownership:conversation-item] formal conversation intent must bind I before a later list discovers it");
    assert_eq!(
        (children[0].1, children[0].2, children[0].3),
        (conversations[0].0, tenant_id, a.key),
        "[ownership:conversation-item] C's normal member A owns I"
    );
    let list_path = format!("/v1/conversations/{CONVERSATION}/items");
    let (status, items) = send(&gateway, "GET", &list_path, &a, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "[ownership:a-reads-own-items] {items}"
    );
    assert_eq!(
        items["data"],
        json!([item()]),
        "[ownership:a-reads-own-items]"
    );
    let (status, replay) = send(
        &gateway,
        "GET",
        &format!("/v1/responses/{RESPONSE_ID}"),
        &b,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "[replay:response-observation] {replay}"
    );
    assert_eq!(replay, expected, "[replay:response-wire]");
    let (status, items) = send(&gateway, "GET", &list_path, &a, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "[replay:a-still-reads-items] {items}"
    );
    assert_eq!(
        items["data"],
        json!([item()]),
        "[replay:a-still-reads-items]"
    );
    let observed = drain(&mut usage);
    for events in [observed, telemetry] {
        let delivered = support::world::deliver(&dp, Uuid::new_v4(), events).await;
        assert_eq!(delivered.status, 200, "[replay:real-intake] {delivered:?}");
        assert_eq!(
            delivered.body.get("rejected"),
            Some(&json!(0)),
            "[replay:real-intake]"
        );
    }
    assert_paid_once(&cp, tenant_id).await;
    let child_replay: Vec<(Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as("SELECT n.id,m.parent_id,n.tenant_id,n.api_key_id FROM native_objects n JOIN native_object_memberships m ON m.environment_id=n.environment_id AND m.child_id=n.id WHERE n.environment_id=$1 AND n.kind='response_item' AND n.raw_id=$2 AND m.relation='conversation_item' AND m.deleted_at IS NULL")
        .bind(cp.env().env_id).bind(ITEM).fetch_all(cp.db().pool()).await.expect("[replay:durable-item] actual durable rows");
    assert_eq!(
        child_replay, children,
        "[replay:durable-item] no duplicate or ownership transfer"
    );
    let root_count: i64 = sqlx::query_scalar("SELECT count(*) FROM native_objects WHERE environment_id=$1 AND kind='response' AND raw_id=$2")
        .bind(cp.env().env_id).bind(RESPONSE_ID).fetch_one(cp.db().pool()).await.expect("[replay:durable-root] actual count");
    assert_eq!(root_count, 1, "[replay:durable-root]");
    assert_eq!(
        upstream
            .wire()
            .iter()
            .filter(|request| request.method == "POST" && request.path == "/v1/responses")
            .count(),
        1,
        "[replay:no-paid-redispatch]"
    );
}
