//! 真实路由、JWT 验签、CP/mTLS、Postgres 和终态 outbox 的元数据回归。
//! 供应商 HTTP 和自定义扫描服务为本地协议替身，不冒充外部供应商端到端验收。
//! 业务断言来自调用者合同；不调用元数据快照、填充或合并 helper。

#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

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
    response::{IntoResponse, Response},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http_body_util::{BodyExt, Limited};
use native_gateway::{projected_snapshot, HttpRegistry};
use ring::hmac;
use serde_json::{json, Value};
use support::{Bundle, OperatorSession, TestCp};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use uuid::Uuid;

const ALIAS: &str = "metadata-public-alias";
const SECOND: &str = "metadata-second-public";
const LABEL: &str = "metadata-original-key";
const UPSTREAM_KEY: &str = "sk-local-metadata-provider-contract";
const SIGNING_KEY: &str = "metadata-hmac-fixture-at-least-thirty-two-bytes";
const COOKIE_CANARY: &str = "metadata_cookie_secret_31e8";
const RESPONSE_ID: &str = "resp_metadata_background";
const BATCH_ID: &str = "batch_metadata_entries";
const INPUT_FILE: &str = "file_metadata_original";
const STAGED_FILE: &str = "file_metadata_staged";
const OUTPUT_FILE: &str = "file_metadata_output";
const A_TEXT: &str = "metadata-entry-a alice@example.com";
const A_MASKED: &str = "metadata-entry-a MASKED";
const B_TEXT: &str = "metadata-entry-b clean";
const COMPLETED_AT: i64 = 1_794_614_500;

#[derive(Default)]
struct ProviderState {
    terminal: AtomicBool,
    scans: Mutex<Vec<String>>,
    calls: Mutex<Vec<(String, String, Vec<u8>)>>,
    files: Mutex<Vec<(String, Vec<u8>)>>,
    batch_input: Mutex<Option<String>>,
}

struct Provider {
    base: String,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start() -> Self {
        let state = Arc::new(ProviderState::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the provider protocol substitute");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(provider).with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }

    fn complete(&self) {
        self.state.terminal.store(true, Ordering::SeqCst);
    }

    fn scans(&self) -> Vec<String> {
        self.state.scans.lock().unwrap().clone()
    }

    fn file(&self, id: &str) -> Vec<Value> {
        let files = self.state.files.lock().unwrap();
        let bytes = &files.iter().find(|file| file.0 == id).unwrap().1;
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn posts(&self, path: &str) -> Vec<Value> {
        self.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.0 == "POST" && call.1 == path)
            .map(|call| serde_json::from_slice(&call.2).unwrap())
            .collect()
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// 形状来自官方 SDK：openai-python/src/openai/types/{responses/response,batch}.py。
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response.py
// https://github.com/openai/openai-python/blob/main/src/openai/types/batch.py
fn response_body(done: bool) -> Value {
    let mut body = json!({
        "id":RESPONSE_ID,"object":"response","created_at":COMPLETED_AT - 100,
        "status":if done { "completed" } else { "queued" },"model":"gpt-4o-mini",
        "background":true,"store":true,"error":null,"incomplete_details":null,
        "output":[],"usage":null
    });
    if done {
        body["completed_at"] = json!(COMPLETED_AT);
        body["output"] = json!([{"id":"msg_metadata_answer","type":"message",
            "role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"classified","annotations":[]}]}]);
        body["usage"] = json!({"input_tokens":7,"output_tokens":3,"total_tokens":10});
    }
    body
}

fn batch_body(state: &ProviderState) -> Value {
    let done = state.terminal.load(Ordering::SeqCst);
    json!({"id":BATCH_ID,"object":"batch","completion_window":"24h",
        "endpoint":"/v1/chat/completions","created_at":COMPLETED_AT - 100,
        "input_file_id":state.batch_input.lock().unwrap().clone(),
        "status":if done { "completed" } else { "in_progress" },
        "completed_at":if done { Some(COMPLETED_AT) } else { None },
        "output_file_id":if done { Some(OUTPUT_FILE) } else { None },
        "request_counts":{"total":2,"completed":if done { 2 } else { 0 },"failed":0}})
}

fn output_lines() -> String {
    [("A", "gpt-4o-mini", 100, 10), ("B", "gpt-4.1-mini", 200, 30)]
        .into_iter()
        .map(|(id, model, input, output)| {
            format!("{}\n", json!({"id":format!("batch_req_metadata_{id}"),"custom_id":id,
                "response":{"status_code":200,"request_id":format!("provider-{id}"),"body":{
                    "id":format!("chatcmpl_metadata_{id}"),"object":"chat.completion",
                    "created":COMPLETED_AT,"model":model,
                    "choices":[{"index":0,"message":{"role":"assistant","content":"classified"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":input + output}}},
                "error":null}))
        })
        .collect()
}

fn file_body(id: &str, bytes: usize) -> Value {
    json!({"id":id,"object":"file","purpose":"batch","filename":"metadata.jsonl",
        "created_at":COMPLETED_AT - 100,"bytes":bytes})
}

async fn provider(State(state): State<Arc<ProviderState>>, request: Request<Body>) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {UPSTREAM_KEY}").as_str());
    let bytes = to_bytes(request.into_body(), 1 << 20).await.unwrap();
    if path == "/scan" {
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        state
            .scans
            .lock()
            .unwrap()
            .push(body["text"].as_str().unwrap().to_owned());
        return Json(json!({"ok":true})).into_response();
    }
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state
        .calls
        .lock()
        .unwrap()
        .push((method.clone(), path.clone(), bytes.to_vec()));
    match (method.as_str(), path.as_str()) {
        ("POST", "/v1/responses") => Json(response_body(false)).into_response(),
        ("GET", "/v1/responses/resp_metadata_background") => {
            Json(response_body(state.terminal.load(Ordering::SeqCst))).into_response()
        }
        ("GET", "/v1/responses/resp_metadata_background/input_items") => {
            Json(json!({"object":"list","data":[],"has_more":false})).into_response()
        }
        ("POST", "/v1/files") => {
            let boundary = content_type
                .split("boundary=")
                .nth(1)
                .unwrap()
                .trim_matches('"');
            let text = std::str::from_utf8(&bytes).unwrap();
            let file = text
                .split(&format!("--{boundary}"))
                .find(|part| part.contains("filename=\""))
                .and_then(|part| part.split_once("\r\n\r\n"))
                .map(|(_, body)| body.strip_suffix("\r\n").unwrap().as_bytes().to_vec())
                .expect("the real gateway sent a multipart file");
            let mut files = state.files.lock().unwrap();
            let id = if files.is_empty() {
                INPUT_FILE
            } else {
                STAGED_FILE
            };
            let body = file_body(id, file.len());
            files.push((id.to_owned(), file));
            Json(body).into_response()
        }
        ("POST", "/v1/batches") => {
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let input = body["input_file_id"].as_str().unwrap();
            if !state
                .files
                .lock()
                .unwrap()
                .iter()
                .any(|file| file.0 == input)
            {
                return StatusCode::NOT_FOUND.into_response();
            }
            *state.batch_input.lock().unwrap() = Some(input.to_owned());
            Json(batch_body(&state)).into_response()
        }
        ("GET", "/v1/batches/batch_metadata_entries") => Json(batch_body(&state)).into_response(),
        ("GET", "/v1/files/file_metadata_output/content") => {
            ([("content-type", "application/jsonl")], output_lines()).into_response()
        }
        ("GET", "/v1/files/file_metadata_output") => {
            Json(file_body(OUTPUT_FILE, output_lines().len())).into_response()
        }
        ("GET", path) if path.starts_with("/v1/files/") => {
            let raw = path.strip_prefix("/v1/files/").unwrap();
            let (id, content) = raw
                .strip_suffix("/content")
                .map_or((raw, false), |id| (id, true));
            let files = state.files.lock().unwrap();
            let Some((_, file)) = files.iter().find(|file| file.0 == id) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            if content {
                file.clone().into_response()
            } else {
                Json(file_body(id, file.len())).into_response()
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    provider: Provider,
    bundle: Bundle,
    alias: Uuid,
    target: Uuid,
    second: Uuid,
    account: Uuid,
    tenant: Uuid,
    member: Uuid,
    key: Uuid,
}

fn uuid_at(value: &Value, pointer: &str) -> Uuid {
    Uuid::parse_str(
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .expect(pointer),
    )
    .unwrap()
}

async fn resource(cp: &TestCp, admin: &OperatorSession, kind: &str, doc: Value) -> Uuid {
    let reply = cp
        .send(
            "POST",
            &format!("/admin/resources/{kind}"),
            Some(admin.cookie()),
            Some(json!({"doc":doc})),
        )
        .await;
    assert_eq!(reply.status, 201, "setup: create {kind}: {reply:?}");
    uuid_at(&reply.body, "/id")
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start_with_kine().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = Provider::start().await;
    let account = resource(
        &cp,
        &admin,
        "provider_keys",
        json!({
        "display_name":"metadata-account","provider":"openai","adapter":"openai",
        "api_base":provider.base,"api_key":UPSTREAM_KEY,"groups":["shared"]}),
    )
    .await;
    let target = resource(
        &cp,
        &admin,
        "models",
        json!({"display_name":"metadata-actual-target",
        "provider":"openai","model_name":"gpt-4o-mini","provider_key_id":account}),
    )
    .await;
    let second = resource(
        &cp,
        &admin,
        "models",
        json!({"display_name":SECOND,
        "provider":"openai","model_name":"gpt-4.1-mini","provider_key_id":account}),
    )
    .await;
    let alias = resource(
        &cp,
        &admin,
        "models",
        json!({"display_name":ALIAS,
        "routing":{"strategy":"failover","retries":0,"targets":[{"model_id":target}]}}),
    )
    .await;
    assert_ne!(
        alias, target,
        "scenario: the public alias must differ from the dispatched target"
    );
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let funded = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/credits"),
            Some(admin.cookie()),
            Some(json!({"amount_cny":"1","reason":"原生元数据真实旅程余额"})),
        )
        .await;
    assert_eq!(funded.status, 201, "{funded:?}");
    let path = format!("/admin/tenants/{tenant}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let granted = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().unwrap(),
            json!({"allowed_model_ids":[alias,target,second]}),
        )
        .await;
    assert_eq!(granted.status, 200, "{granted:?}");
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    let minted = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(admin.cookie()),
            Some(json!({"label":LABEL,"group_id":group,"member_id":member})),
        )
        .await;
    assert_eq!(minted.status, 201, "{minted:?}");
    let key = uuid_at(&minted.body, "/key/id");
    // 两个合法 JWT 映射到同一个真实 key；观察者不能替换接受时的历史文本或标签。
    for phase in ["admission", "observer"] {
        resource(&cp, &admin, "oidc_providers", json!({
            "name":format!("metadata-{phase}-idp"),"hmac_secret":SIGNING_KEY,
            "issuer":format!("https://metadata-{phase}.example"),"tag_claims":{"department":"department"}})).await;
        resource(
            &cp,
            &admin,
            "claim_mappings",
            json!({"name":format!("metadata-{phase}-mapping"),
            "jwt_provider":format!("metadata-{phase}-idp"),
            "match":[{"claim":"sub","op":"exact","values":[format!("metadata-{phase}-subject")]}],
            "resolve":{"api_key_id":key}}),
        )
        .await;
    }
    resource(
        &cp,
        &admin,
        "observability_exporters",
        json!({"name":"metadata-selected-headers",
        "kind":"otlp_http","endpoint":format!("{}/unused-traces", provider.base),
        "request_headers":["x-metadata-project"]}),
    )
    .await;
    let script = format!(
        r#"export async function checkInput(ctx) {{
        const reply = await fetch("{}/scan", {{method:"POST", headers:{{"content-type":"application/json"}}, body:JSON.stringify({{text:ctx.text}})}});
        if (!reply.ok) throw new Error("scan failed");
        if (ctx.text.includes("metadata-entry-a")) throw new Error("entry-a scanner unavailable");
        return {{action:"allow"}};
    }}"#,
        provider.base
    );
    let rules = [
        json!({"name":"metadata-fail-open","kind":"custom","hook_point":"input","fail_open":true,"script":script,"timeout_ms":5000}),
        json!({"name":"metadata-monitor","kind":"keyword","hook_point":"input","enforcement_mode":"monitor", "patterns":[{"kind":"literal","value":"metadata-entry-a"}]}),
        json!({"name":"metadata-mask","kind":"pii","hook_point":"input","detectors":[],
            "custom_patterns":[{"name":"private_email","regex":"alice@example\\.com","action":"mask","replacement":"MASKED"}]}),
    ];
    for (priority, rule) in rules.into_iter().enumerate() {
        let id = resource(&cp, &admin, "guardrails", rule).await;
        for model in [alias, second] {
            resource(
                &cp,
                &admin,
                "guardrail_attachments",
                json!({"guardrail_id":id,
                "scope_type":"model","scope_id":model,"priority":100 + priority}),
            )
            .await;
        }
    }
    for model in [alias, target, second] {
        support::world::insert_baseline_backdated(cp.db(), model,
            json!({"default":{"base":{"input":"2","output":"4"}},"batch":{"base":{"input":"2","output":"4"}}}),
            "2000-01-01T00:00:00Z".parse().unwrap()).await;
    }
    let bundle = cp.issue_bundle(&admin, "metadata-handler-gateway").await;
    Some(Fixture {
        cp,
        admin,
        provider,
        bundle,
        alias,
        target,
        second,
        account,
        tenant,
        member,
        key,
    })
}

async fn gateway(f: &Fixture) -> (ProxyState, mpsc::Receiver<UsageEvent>) {
    let snapshot = SnapshotHandle::new(projected_snapshot(&f.cp).await);
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .unwrap();
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let (tx, rx) = mpsc::channel(64);
    let state = ProxyState::new(snapshot, hub, &config)
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            &f.cp.state().pki.platform_id_key_hex(),
        ))
        .with_native_objects(Arc::new(HttpRegistry::new(&f.cp, &f.bundle)))
        .with_guardrail_index(guardrails)
        .with_usage_sink(UsageSink::new(tx));
    (state, rx)
}

async fn verified_auth_preflight(state: &ProxyState) {
    let good = token("admission");
    let (header_and_claims, _) = good.rsplit_once('.').unwrap();
    let bad = format!("{header_and_claims}.{}", URL_SAFE_NO_PAD.encode([0_u8; 32]));
    for (bearer, expected) in [(good, StatusCode::OK), (bad, StatusCode::UNAUTHORIZED)] {
        let request = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap();
        let response = build_router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        Limited::new(response.into_body(), 1 << 20)
            .collect()
            .await
            .unwrap();
        assert_eq!(
            status, expected,
            "setup: real JWT verification accepts the signature and rejects its forgery"
        );
    }
}

fn token(phase: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        json!({"sub":format!("metadata-{phase}-subject"),
        "iss":format!("https://metadata-{phase}.example"),"department":phase,
        "exp":chrono::Utc::now().timestamp() + 3600})
        .to_string(),
    );
    let message = format!("{header}.{claims}");
    let signature = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, SIGNING_KEY.as_bytes()),
        message.as_bytes(),
    );
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
}

async fn send(
    state: &ProxyState,
    method: &str,
    path: &str,
    phase: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {}", token(phase)))
        .header("cookie", format!("session={COOKIE_CANARY}"))
        .header("x-metadata-project", phase)
        .header("content-type", content_type)
        .body(Body::from(bytes))
        .unwrap();
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = Limited::new(response.into_body(), 1 << 20)
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let body =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, body)
}

async fn json_request(
    state: &ProxyState,
    method: &str,
    path: &str,
    phase: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        state,
        method,
        path,
        phase,
        "application/json",
        serde_json::to_vec(&body).unwrap(),
    )
    .await
}

fn drain(rx: &mut mpsc::Receiver<UsageEvent>) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(serde_json::to_value(event).unwrap());
    }
    events
}

fn original_metadata(event: &Value) {
    assert_eq!(
        event["api_key_name"], LABEL,
        "the accepted key label is historical"
    );
    assert_eq!(event["tags"], json!({"department":"admission"}));
    assert_eq!(event["jwt_subject"], "metadata-admission-subject");
    assert_eq!(event["jwt_provider"], "metadata-admission-idp");
    assert_eq!(event["jwt_claim_mapping"], "metadata-admission-mapping");
    assert_eq!(
        event["request_headers"],
        json!({"x-metadata-project":"admission"})
    );
    let wire = event.to_string();
    for secret in [COOKIE_CANARY, SIGNING_KEY, UPSTREAM_KEY] {
        assert!(
            !wire.contains(secret),
            "credential values must not become metadata"
        );
    }
    assert!(
        !wire.contains(&URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#)),
        "the raw signed credential must not become metadata"
    );
    let headers = event["request_headers"].as_object().unwrap();
    assert!(!headers.contains_key("authorization"));
    assert!(!headers.contains_key("cookie"));
}

fn input_audit(event: &Value, is_a: bool) {
    let applied = event["applied_guardrails"]
        .as_array()
        .expect("executed input rules are recorded");
    for kind in ["custom", "keyword", "pii"] {
        assert!(
            applied
                .iter()
                .any(|rule| rule["hook"] == "input" && rule["kind"] == kind),
            "missing {kind}: {event}"
        );
    }
    let bypass = event
        .get("guardrail_bypassed_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let monitor: Vec<_> = event
        .get("guardrail_monitor_hits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let enforced: Vec<_> = event
        .get("guardrail_enforced_hits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    if is_a {
        assert!(
            !bypass.is_empty(),
            "the actually failed-open entry must retain its bypass"
        );
        assert!(monitor
            .iter()
            .any(|hit| hit["guardrail_name"] == "metadata-monitor"
                && hit["hook"] == "input"
                && hit["action"] == "would_block"));
        assert!(enforced
            .iter()
            .any(|hit| hit["guardrail_name"] == "metadata-mask"
                && hit["hook"] == "input"
                && hit["action"] == "masked"
                && hit["counts"]["private_email"] == 1));
    } else {
        assert!(bypass.is_empty(), "entry B never bypassed: {event}");
        assert!(
            monitor.is_empty(),
            "entry A's monitor result must not be broadcast to B: {event}"
        );
        assert!(
            enforced.is_empty(),
            "entry A's mask must not be broadcast to B: {event}"
        );
    }
}

async fn record(f: &Fixture, raw: &str) -> (Uuid, Uuid) {
    let row: (Uuid, Uuid, Uuid, Uuid, Option<Uuid>, Uuid, Uuid) = sqlx::query_as(
        "SELECT id,operation_id,tenant_id,api_key_id,member_id,model_id,provider_key_id FROM native_objects WHERE raw_id=$1")
        .bind(raw).fetch_one(f.cp.db().pool()).await.expect("positive control: admission stored a real native object");
    assert_eq!(
        (row.2, row.3, row.4, row.5, row.6),
        (f.tenant, f.key, Some(f.member), f.target, f.account)
    );
    (row.0, row.1)
}

async fn terminals(f: &Fixture, record: Uuid, count: i64) -> Vec<(String, Value, i64)> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let posted: i64 = sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox o JOIN usage_events u ON u.event_id=o.event_id::text JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1 AND o.delivered_at IS NOT NULL")
                .bind(record).fetch_one(f.cp.db().pool()).await.unwrap();
            if posted >= count { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("positive control: real worker, terminal outbox, sink and nonzero ledger all complete");
    let rows: Vec<(String, Value, i64, Value, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT o.entry_id,o.event,c.charge_micro_cny,u.event,-l.amount_micro_cny,u.occurred_at FROM native_terminal_outbox o JOIN pricing_charges_cny c ON c.event_id=o.event_id::text JOIN usage_events u ON u.event_id=o.event_id::text JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1 ORDER BY o.entry_id")
        .bind(record).fetch_all(f.cp.db().pool()).await.unwrap();
    assert_eq!(
        rows.len(),
        count as usize,
        "one priced and delivered terminal per entry"
    );
    rows.into_iter()
        .map(|(entry, event, charge, delivered, debit, occurred)| {
            assert!(charge > 0, "the terminal must actually be billed");
            assert_eq!(charge, debit);
            assert_eq!(
                event, delivered,
                "the sink must retain the complete canonical terminal"
            );
            assert_eq!(
                occurred.timestamp(),
                COMPLETED_AT,
                "provider completion time remains authoritative"
            );
            assert_eq!(
                chrono::DateTime::parse_from_rfc3339(event["occurred_at"].as_str().unwrap())
                    .unwrap()
                    .timestamp(),
                COMPLETED_AT
            );
            assert_eq!(event["team_id"], json!(f.tenant));
            assert_eq!(event["user_id"], json!(f.member));
            assert_eq!(event["api_key_id"], json!(f.key));
            assert_eq!(event["inference_stage"], "terminal");
            (entry, event, charge)
        })
        .collect()
}

async fn rename_key(f: &Fixture) {
    let path = format!("/admin/keys/{}", f.key);
    let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let changed =
        f.cp.send_conditional(
            "PATCH",
            &path,
            f.admin.cookie(),
            current.body["version"].as_i64().unwrap(),
            json!({"label":"metadata-current-key"}),
        )
        .await;
    assert_eq!(changed.status, 200, "{changed:?}");
}

async fn replay_once(
    f: &Fixture,
    record: Uuid,
    events: Vec<Value>,
    expected_count: i64,
    expected_total: i64,
) {
    let delivered =
        support::world::deliver(&f.cp.dp_client(&f.bundle), Uuid::new_v4(), events).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(delivered.body["accepted"], 0);
    assert_eq!(delivered.body["rejected"], 0);
    assert_eq!(delivered.body["duplicates"], expected_count);
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT count(*),count(l.event_id),COALESCE(sum(-l.amount_micro_cny),0)::bigint FROM native_terminal_outbox o LEFT JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(counts, (expected_count, expected_count, expected_total));
}

async fn stop(stop: watch::Sender<bool>, worker: tokio::task::JoinHandle<()>) {
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("the real worker shuts down")
        .unwrap();
}

// 删除 handler 的接受时元数据采集，或用观察者身份重新构建终态，会丢失原始非空值。
#[tokio::test]
async fn background_response_keeps_original_verified_metadata_through_real_authority_and_sink() {
    let Some(f) = fixture().await else { return };
    let (state, mut usage) = gateway(&f).await;
    verified_auth_preflight(&state).await;
    let (status, body) = json_request(
        &state,
        "POST",
        "/v1/responses",
        "admission",
        json!({"model":ALIAS,"background":true,"input":A_TEXT}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: real background admission: {body}"
    );
    assert_eq!(body["id"], RESPONSE_ID);
    assert_eq!(body["status"], "queued");
    let (object, operation) = record(&f, RESPONSE_ID).await;
    let sent = f.provider.posts("/v1/responses");
    assert_eq!(
        sent.len(),
        1,
        "the real handler dispatched one accepted request"
    );
    assert_eq!(sent[0]["model"], "gpt-4o-mini");
    assert_eq!(
        sent[0]["input"], A_MASKED,
        "positive control: actual input mask reached the provider"
    );
    assert_eq!(
        f.provider.scans().len(),
        1,
        "the custom input rule actually ran"
    );
    assert!(f.provider.scans()[0].contains("metadata-entry-a"));
    let accepted: Vec<_> = drain(&mut usage)
        .into_iter()
        .filter(|event| event["inference_stage"] == "accepted")
        .collect();
    assert_eq!(
        accepted.len(),
        1,
        "one real accepted event, no fabricated terminal"
    );
    assert_eq!(accepted[0]["native_operation_id"], json!(operation));
    rename_key(&f).await;
    let (recovery, mut observation) = gateway(&f).await;
    f.provider.complete();
    let (cancel, stopped) = watch::channel(false);
    let worker = tokio::spawn(aisix_proxy::native_api::background::run(
        recovery.clone(),
        stopped,
    ));
    let rows = terminals(&f, object, 1).await;
    stop(cancel, worker).await;
    assert_eq!(rows[0].0, "");
    assert_eq!(rows[0].2, 26, "7 input * 2 + 3 output * 4 micro-CNY");
    assert_eq!(rows[0].1["model_id"], json!(f.target));
    assert_eq!(rows[0].1["requested_model_id"], json!(f.alias));
    assert_eq!(rows[0].1["requested_model"], ALIAS);
    assert_eq!(rows[0].1["upstream_model"], "gpt-4o-mini");
    assert_eq!(rows[0].1["native_operation_id"], json!(operation));
    assert_eq!(
        (
            rows[0].1["prompt_tokens"].as_u64(),
            rows[0].1["completion_tokens"].as_u64()
        ),
        (Some(7), Some(3))
    );
    original_metadata(&accepted[0]);
    input_audit(&accepted[0], true);
    original_metadata(&rows[0].1);
    input_audit(&rows[0].1, true);
    let (status, body) = json_request(
        &recovery,
        "GET",
        &format!("/v1/responses/{RESPONSE_ID}"),
        "observer",
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a different verified identity on the same current key can read: {body}"
    );
    assert_eq!(body["status"], "completed");
    let observed = drain(&mut observation);
    assert!(
        observed
            .iter()
            .any(|event| event["jwt_subject"] == "metadata-observer-subject"),
        "positive control: the observer JWT was actually verified"
    );
    assert_eq!(
        terminals(&f, object, 1).await[0].1,
        rows[0].1,
        "current observer metadata cannot rewrite historical metadata"
    );
    assert_eq!(f.provider.posts("/v1/responses").len(), 1);
    replay_once(&f, object, vec![rows[0].1.clone()], 1, 26).await;
}

// 让批次根审计广播给每个 entry，B 会错误得到 A 的旁路、monitor 和 mask。
#[tokio::test]
async fn actual_two_entry_batch_keeps_each_executed_input_audit_with_its_own_terminal() {
    let Some(f) = fixture().await else { return };
    let (state, mut usage) = gateway(&f).await;
    verified_auth_preflight(&state).await;
    let lines: String = [("A", ALIAS, A_TEXT), ("B", SECOND, B_TEXT)]
        .into_iter()
        .map(|(id, model, text)| {
            format!(
                "{}\n",
                json!({"custom_id":id,"method":"POST","url":"/v1/chat/completions",
            "body":{"model":model,"messages":[{"role":"user","content":text}]}})
            )
        })
        .collect();
    let boundary = "metadata-batch-input";
    // 文件传输使用同账号、不挂扫描规则的 direct 模型；输入规则在批次准入逐 entry 执行。
    let upload = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nmetadata-actual-target\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"metadata.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{lines}\r\n--{boundary}--\r\n");
    let (status, body) = send(
        &state,
        "POST",
        "/v1/files",
        "admission",
        &format!("multipart/form-data; boundary={boundary}"),
        upload.into_bytes(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: real uploaded input file: {body}"
    );
    assert_eq!(body["id"], INPUT_FILE);
    f.provider.state.scans.lock().unwrap().clear();
    drain(&mut usage);
    let (status, body) = json_request(&state, "POST", "/v1/batches", "admission",
        json!({"input_file_id":INPUT_FILE,"endpoint":"/v1/chat/completions","completion_window":"24h"})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: real two-entry batch admission: {body}"
    );
    assert_eq!(body["id"], BATCH_ID);
    let (object, operation) = record(&f, BATCH_ID).await;
    let scans = f.provider.scans();
    assert_eq!(
        scans.len(),
        2,
        "the actual batch admission independently scans both entries"
    );
    assert_eq!(
        scans
            .iter()
            .filter(|text| text.contains("metadata-entry-a") && !text.contains("metadata-entry-b"))
            .count(),
        1
    );
    assert_eq!(
        scans
            .iter()
            .filter(|text| text.contains("metadata-entry-b") && !text.contains("metadata-entry-a"))
            .count(),
        1
    );
    let staged = f.provider.file(STAGED_FILE);
    assert_eq!(staged.len(), 2);
    assert_eq!(staged[0]["custom_id"], "A");
    assert_eq!(staged[0]["body"]["model"], "gpt-4o-mini");
    assert_eq!(staged[0]["body"]["messages"][0]["content"], A_MASKED);
    assert_eq!(staged[1]["custom_id"], "B");
    assert_eq!(staged[1]["body"]["model"], "gpt-4.1-mini");
    assert_eq!(staged[1]["body"]["messages"][0]["content"], B_TEXT);
    assert_eq!(f.provider.posts("/v1/batches").len(), 1);
    rename_key(&f).await;
    let (recovery, _) = gateway(&f).await;
    f.provider.complete();
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&recovery)
            .await
            .unwrap(),
        1
    );
    let rows = terminals(&f, object, 2).await;
    // 先检查两条真实非零终态、独立 entry 和计量，再检查元数据隔离。
    for (index, (entry, event, charge)) in rows.iter().enumerate() {
        let (expected_entry, requested, target, upstream, input, output, expected_charge) =
            if index == 0 {
                ("A", f.alias, f.target, "gpt-4o-mini", 100, 10, 240)
            } else {
                ("B", f.second, f.second, "gpt-4.1-mini", 200, 30, 520)
            };
        assert_eq!(entry, expected_entry);
        assert_eq!(event["requested_model_id"], json!(requested));
        assert_eq!(event["model_id"], json!(target));
        assert_eq!(event["upstream_model"], upstream);
        assert_eq!(event["native_operation_id"], json!(operation));
        assert_eq!(event["prompt_tokens"], input);
        assert_eq!(event["completion_tokens"], output);
        assert_eq!(*charge, expected_charge);
        assert_eq!(event["is_batch"], true);
    }
    for (entry, event, _) in &rows {
        original_metadata(event);
        input_audit(event, entry == "A");
    }
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&recovery)
            .await
            .unwrap(),
        0
    );
    replay_once(
        &f,
        object,
        rows.into_iter().map(|(_, event, _)| event).collect(),
        2,
        760,
    )
    .await;
    assert_eq!(f.provider.posts("/v1/batches").len(), 1);
}
