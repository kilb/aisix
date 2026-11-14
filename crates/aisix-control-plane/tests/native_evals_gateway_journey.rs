//! 真实控制面 HTTP/mTLS、Postgres、kine 与两个独立网关状态的 Evals 联合组件旅程。
//! 只有供应商 HTTP 是协议替身；不声称两个网关二进制进程或真实供应商验收。

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
use aisix_core::{AisixSnapshot, ProxyConfig};
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
use support::{Bundle, OperatorSession, TestCp};
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

const BUILD: &str = "dev+sha-74ac291";
const MODEL_NAME: &str = "joint-evals-model";
const UPSTREAM_MODEL: &str = "eval-provider-model";
const EVAL: &str = "eval_joint_owned";
const RUN: &str = "evalrun_joint_owned";

#[derive(Default)]
struct ProviderState {
    terminal: AtomicBool,
    requests: Mutex<Vec<(String, String)>>,
}

struct Provider {
    base: String,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Provider {
    async fn start() -> Self {
        let state = Arc::new(ProviderState::default());
        let app = Router::new()
            .fallback(provider_request)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the protocol substitute");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("the bound address")
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve the protocol substitute");
        });
        Self { base, state, task }
    }

    fn request_count(&self) -> usize {
        self.state.requests.lock().expect("request list").len()
    }
}

async fn provider_request(
    State(state): State<Arc<ProviderState>>,
    request: Request<Body>,
) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let bytes = to_bytes(request.into_body(), 1 << 20)
        .await
        .expect("read the substitute request");
    state
        .requests
        .lock()
        .expect("request list")
        .push((method.clone(), path.clone()));
    let eval = json!({"id":EVAL,"object":"eval","name":"classification"});
    match (method.as_str(), path.as_str()) {
        ("POST", "/v1/evals") => Json(eval).into_response(),
        ("GET", "/v1/evals") => Json(json!({"object":"list","data":[eval,{"id":"eval_foreign","object":"eval"}],"has_more":false})).into_response(),
        ("GET", p) if p == format!("/v1/evals/{EVAL}") => Json(eval).into_response(),
        ("POST", p) if p == format!("/v1/evals/{EVAL}/runs") => {
            let body: Value = serde_json::from_slice(&bytes).expect("the run request is JSON");
            assert_eq!(body["data_source"]["model"], UPSTREAM_MODEL);
            Json(json!({"id":RUN,"eval_id":EVAL,"object":"eval.run","status":"queued"})).into_response()
        }
        ("GET", p) if p == format!("/v1/evals/{EVAL}/runs/{RUN}") => {
            let mut run = json!({"id":RUN,"eval_id":EVAL,"object":"eval.run","status":"queued"});
            if state.terminal.load(Ordering::Acquire) {
                run["status"] = json!("completed");
                run["per_model_usage"] = json!([{"model_name":UPSTREAM_MODEL,"invocation_count":1,"prompt_tokens":100,"cached_tokens":20,"completion_tokens":10,"total_tokens":110}]);
            }
            Json(run).into_response()
        }
        _ => (StatusCode::NOT_FOUND, Json(json!({"error":{"type":"invalid_request_error","message":"unknown protocol operation"}}))).into_response(),
    }
}

fn id_at(value: &Value, pointer: &str) -> Uuid {
    Uuid::parse_str(
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .expect(pointer),
    )
    .expect("a UUID field")
}

async fn grant(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, models: &[Uuid]) {
    let path = format!("/admin/tenants/{tenant}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let changed = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().expect("tenant version"),
            json!({"allowed_model_ids":models}),
        )
        .await;
    assert_eq!(changed.status, 200, "{changed:?}");
}

async fn funded_tenant(cp: &TestCp, admin: &OperatorSession, model: Uuid) -> (Uuid, Uuid) {
    let tenant = support::world::seed_tenant(cp, admin).await;
    let funded = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/credits"),
            Some(admin.cookie()),
            Some(json!({"amount_cny":"1","reason":"原生评估联合组件验证"})),
        )
        .await;
    assert_eq!(funded.status, 201, "{funded:?}");
    grant(cp, admin, tenant, &[model]).await;
    (tenant, cp.seed_member(tenant, MemberRole::Member).await.id)
}

async fn mint(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, member: Uuid) -> (Uuid, String) {
    let group = support::world::group_id(cp, "shared").await;
    let response = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(admin.cookie()),
            Some(json!({"label":"eval-journey","group_id":group,"member_id":member})),
        )
        .await;
    assert_eq!(response.status, 201, "{response:?}");
    let key = id_at(&response.body, "/key/id");
    let projected: Value = sqlx::query_scalar(
        "SELECT doc FROM gw_resources WHERE kind='api_keys' AND id=$1 AND NOT deleted",
    )
    .bind(key)
    .fetch_one(cp.db().pool())
    .await
    .expect("the projected customer grant");
    assert!(projected["allowed_endpoints"].as_array().expect("an explicit endpoint ceiling").contains(&json!("evals")), "supported native fleet must make Evals reachable through an ordinary minted key: {projected}");
    (
        key,
        response.body["plaintext"]
            .as_str()
            .expect("the minted key")
            .to_owned(),
    )
}

async fn beat(cp: &TestCp, bundle: &Bundle, instance: &str) {
    let response = cp.dp_client(bundle).heartbeat(json!({"dp_id":bundle.dp_id,"instance_id":instance,"hostname":"eval-journey","uptime_seconds":1,"version":BUILD,"supported_guardrail_kinds":[],"applied_revision":1,"exporter_health":[],"partially_compatible_resources":[]})).await;
    assert_eq!(response.status, 200, "{response:?}");
}

fn gateway(
    cp: &TestCp,
    bundle: &Bundle,
    snapshot: AisixSnapshot,
) -> (ProxyState, mpsc::Receiver<UsageEvent>) {
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .expect("gateway configuration");
    let (tx, rx) = mpsc::channel(64);
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
    bearer: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {bearer}"));
    let body = if let Some(body) = body {
        request = request.header("content-type", "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = build_router(state.clone())
        .oneshot(request.body(body).expect("a request"))
        .await
        .expect("the production gateway router");
    let status = response.status();
    let bytes = Limited::new(response.into_body(), 1 << 20)
        .collect()
        .await
        .expect("a bounded response")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).expect("the protocol response is JSON"),
    )
}

fn drain(receiver: &mut mpsc::Receiver<UsageEvent>) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(serde_json::to_value(event).expect("usage event serialization"));
    }
    events
}

#[tokio::test]
async fn accepted_eval_survives_real_authority_restart_and_settles_once_after_withdrawal() {
    let Some(kine) = support::kine_endpoint() else {
        return;
    };
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = kine;
        cfg.cp_version = BUILD.to_owned();
    })
    .await
    else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = Provider::start().await;
    let resource = cp.send("POST", "/admin/resources/provider_keys", Some(admin.cookie()), Some(json!({"doc":{"display_name":"joint-eval-account","provider":"openai","adapter":"openai","api_base":provider.base,"api_key":"sk-joint-eval-component","groups":["shared"]}}))).await;
    assert_eq!(resource.status, 201, "{resource:?}");
    let model = cp.send("POST", "/admin/resources/models", Some(admin.cookie()), Some(json!({"doc":{"display_name":MODEL_NAME,"provider":"openai","model_name":UPSTREAM_MODEL,"provider_key_id":id_at(&resource.body,"/id")}}))).await;
    assert_eq!(model.status, 201, "{model:?}");
    let model = id_at(&model.body, "/id");
    let gateway_a = cp.issue_bundle(&admin, "eval-gateway-a").await;
    let gateway_b = cp.issue_bundle(&admin, "eval-gateway-b").await;
    assert_ne!(gateway_a.dp_id, gateway_b.dp_id);
    beat(&cp, &gateway_a, "eval-instance-a").await;
    beat(&cp, &gateway_b, "eval-instance-b").await;
    let (tenant, member) = funded_tenant(&cp, &admin, model).await;
    let (key, bearer) = mint(&cp, &admin, tenant, member).await;
    let (_, reader) = mint(&cp, &admin, tenant, member).await;
    let (foreign_tenant, foreign_member) = funded_tenant(&cp, &admin, model).await;
    let (_, foreign) = mint(&cp, &admin, foreign_tenant, foreign_member).await;
    support::world::insert_baseline_backdated(
        cp.db(),
        model,
        json!({"default":{"base":{"input":"4","output":"8","cache_read":"1"}}}),
        "2000-01-01T00:00:00Z".parse().expect("price start"),
    )
    .await;
    let snapshot = projected_snapshot(&cp).await;
    let (state_a, mut events_a) = gateway(&cp, &gateway_a, snapshot.clone());
    let (state_b, mut events_b) = gateway(&cp, &gateway_b, projected_snapshot(&cp).await);
    let (status, eval) = send(&state_a, "POST", &format!("/v1/evals?model={MODEL_NAME}"), &bearer, Some(json!({"name":"classification","data_source_config":{"type":"custom","item_schema":{"type":"object","properties":{"answer":{"type":"string"}}}},"testing_criteria":[{"type":"string_check","name":"match","input":"{{sample.output_text}}","reference":"{{item.answer}}","operation":"eq"}]}))).await;
    assert_eq!(status, StatusCode::OK, "{eval}");
    assert_eq!(eval.get("id"), Some(&json!(EVAL)));
    let (status, run) = send(&state_a, "POST", &format!("/v1/evals/{EVAL}/runs"), &bearer, Some(json!({"data_source":{"type":"completions","model":MODEL_NAME,"input_messages":{"type":"template","template":[{"role":"user","content":"Reply yes"}]},"source":{"type":"file_content","content":[{"item":{"answer":"yes"}}]}}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run.get("id"), Some(&json!(RUN)));
    let (record, operation): (Uuid, Uuid) = sqlx::query_as("SELECT id,operation_id FROM native_objects WHERE environment_id=$1 AND kind='eval_run' AND raw_id=$2")
        .bind(cp.env().env_id).bind(RUN).fetch_one(cp.db().pool()).await.expect("the accepted run is already durable");
    let accepted = drain(&mut events_a);
    assert_eq!(accepted.len(), 2);
    assert_eq!(
        accepted[0].get("inference_stage"),
        Some(&json!("management"))
    );
    assert_eq!(accepted[1].get("inference_stage"), Some(&json!("accepted")));
    assert_eq!(
        accepted[1].get("native_operation_id"),
        Some(&json!(operation))
    );
    for event in &accepted {
        assert_eq!(event.get("prompt_tokens"), Some(&json!(0)));
        assert_eq!(event.get("completion_tokens"), Some(&json!(0)));
    }
    assert_eq!(accepted[1].get("is_final_attempt"), Some(&json!(false)));
    let delivered =
        support::world::deliver(&cp.dp_client(&gateway_a), Uuid::new_v4(), accepted).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(delivered.body["rejected"], 0, "{delivered:?}");
    let before = cp
        .send(
            "GET",
            "/admin/usage/direct-models",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(before.status, 200, "{before:?}");
    assert_eq!(
        before.body["totals"]["attempts"], 0,
        "management and acceptance do not finish inference: {before:?}"
    );
    let (status, read) = send(
        &state_b,
        "GET",
        &format!("/v1/evals/{EVAL}/runs/{RUN}"),
        &reader,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read.get("id"), Some(&json!(RUN)));
    let (status, listed) = send(
        &state_b,
        "GET",
        &format!("/v1/evals?model={MODEL_NAME}"),
        &reader,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed.get("data"), Some(&json!([eval])));
    assert_eq!(listed.get("has_more"), Some(&json!(false)));
    let before_denials = provider.request_count();
    for path in [
        format!("/v1/evals/{EVAL}"),
        format!("/v1/evals/{EVAL}/runs/{RUN}"),
    ] {
        assert_eq!(
            send(&state_b, "GET", &path, &foreign, None).await.0,
            StatusCode::NOT_FOUND
        );
    }
    grant(&cp, &admin, tenant, &[]).await;
    assert_eq!(
        send(
            &state_b,
            "GET",
            &format!("/v1/evals/{EVAL}/runs/{RUN}"),
            &reader,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(provider.request_count(), before_denials, "current CP owner/model decisions must precede provider dispatch despite stale gateway snapshots");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1")
        .bind(key)
        .fetch_one(cp.db().pool())
        .await
        .expect("the key version");
    let revoked = cp
        .send_conditional(
            "DELETE",
            &format!("/admin/keys/{key}"),
            admin.cookie(),
            version,
            Value::Null,
        )
        .await;
    assert_eq!(revoked.status, 204, "{revoked:?}");
    drop(state_a);
    drop(state_b);
    let restarted = cp.restart_stamped(BUILD).await;
    drop(cp);
    let (recovery, _) = gateway(&restarted, &gateway_b, snapshot);
    let before_revoked = provider.request_count();
    assert_eq!(
        send(
            &recovery,
            "GET",
            &format!("/v1/evals/{EVAL}/runs/{RUN}"),
            &bearer,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(provider.request_count(), before_revoked);
    provider.state.terminal.store(true, Ordering::Release);
    assert_eq!(
        aisix_proxy::native_api::openai_eval_worker::run_once(&recovery)
            .await
            .expect("the restarted observer uses real CP leases"),
        1
    );
    assert_eq!(
        aisix_proxy::native_api::openai_eval_worker::run_once(&recovery)
            .await
            .expect("the second observation is idempotent"),
        0
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let delivered: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_terminal_outbox o JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1 AND o.delivered_at IS NOT NULL)")
                .bind(record).fetch_one(restarted.db().pool()).await.expect("the terminal ledger posting");
            if delivered { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("accepted work settles after caller and model withdrawal");
    let (event, charge): (Value, i64) = sqlx::query_as("SELECT o.event,c.charge_micro_cny FROM native_terminal_outbox o JOIN pricing_charges_cny c ON c.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(restarted.db().pool()).await.expect("the priced terminal event");
    for (field, expected) in [
        ("team_id", json!(tenant)),
        ("user_id", json!(member)),
        ("api_key_id", json!(key)),
        ("native_operation_id", json!(operation)),
        ("inference_stage", json!("terminal")),
        ("prompt_tokens", json!(100)),
        ("cached_prompt_tokens", json!(20)),
        ("completion_tokens", json!(10)),
        ("total_tokens", json!(110)),
    ] {
        assert_eq!(
            event.get(field),
            Some(&expected),
            "terminal field {field} must be explicitly present"
        );
    }
    assert_eq!(
        charge, 420,
        "80 uncached*4 + 20 cached*1 + 10 output*8, at one CNY per USD"
    );
    let replay = support::world::deliver(
        &restarted.dp_client(&gateway_b),
        Uuid::new_v4(),
        vec![event],
    )
    .await;
    assert_eq!(replay.status, 200, "{replay:?}");
    assert_eq!(replay.body["rejected"], 0, "{replay:?}");
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT count(*),count(l.event_id),COALESCE(sum(-l.amount_micro_cny),0)::bigint FROM native_terminal_outbox o LEFT JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(restarted.db().pool()).await.expect("the immutable posting counts");
    assert_eq!(
        counts,
        (1, 1, 420),
        "worker recovery and event replay cannot double charge"
    );
    assert!(
        !drain(&mut events_b).is_empty(),
        "gateway B exercised actual management and denial paths"
    );
}
