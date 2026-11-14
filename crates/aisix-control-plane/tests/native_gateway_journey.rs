//! 真实控制面 HTTP/mTLS、Postgres、kine 与两个独立网关状态的联合组件测试。
//! 网关使用进程内生产路由而非两个二进制进程；仅供应商 HTTP 服务为协议替身。

#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::sync::Arc;
use std::time::Duration;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ProxyConfig};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::{BodyExt, Limited};
use native_gateway::{
    projected_snapshot, HttpRegistry, ProviderSubstitute, BATCH, INPUT_FILE, OUTPUT_FILE,
};
use serde_json::{json, Value};
use support::{Bundle, OperatorSession, TestCp};
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

const MODEL_NAME: &str = "joint-batch-model-v7";

fn id_at(body: &Value, pointer: &str) -> Uuid {
    Uuid::parse_str(
        body.pointer(pointer)
            .and_then(Value::as_str)
            .expect(pointer),
    )
    .expect("a resource UUID")
}

struct Caller {
    id: Uuid,
    bearer: String,
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    upstream: ProviderSubstitute,
    model: Uuid,
    tenant: Uuid,
    member: Uuid,
    submitter: Caller,
    reader: Caller,
    foreign: Caller,
    gateway_a: Bundle,
    gateway_b: Bundle,
    snapshot: AisixSnapshot,
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
            current.body["version"].as_i64().expect("a version"),
            json!({"allowed_model_ids":models}),
        )
        .await;
    assert_eq!(changed.status, 200, "{changed:?}");
}

async fn fund(cp: &TestCp, admin: &OperatorSession, tenant: Uuid) {
    let credited = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/credits"),
            Some(admin.cookie()),
            Some(json!({"amount_cny":"1","reason":"联合原生对象组件测试的预付余额"})),
        )
        .await;
    assert_eq!(credited.status, 201, "{credited:?}");
    assert_eq!(credited.body["amount_micro_cny"], 1_000_000);
}

async fn mint(
    cp: &TestCp,
    admin: &OperatorSession,
    tenant: Uuid,
    member: Uuid,
    label: &str,
) -> Caller {
    let group = support::world::group_id(cp, "shared").await;
    let created = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(admin.cookie()),
            Some(json!({"label":label,"group_id":group,"member_id":member})),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    Caller {
        id: id_at(&created.body, "/key/id"),
        bearer: created.body["plaintext"]
            .as_str()
            .expect("the minted bearer")
            .to_owned(),
    }
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start_with_kine().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let upstream = ProviderSubstitute::start().await;
    let provider = cp.send("POST", "/admin/resources/provider_keys", Some(admin.cookie()), Some(json!({"doc":{
        "display_name":"joint-provider-v7","provider":"openai","adapter":"openai",
        "api_base":upstream.base,"api_key":native_gateway::PROVIDER_SECRET,"groups":["shared"]
    }}))).await;
    assert_eq!(provider.status, 201, "{provider:?}");
    let model = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc":{
                "display_name":MODEL_NAME,"provider":"openai","model_name":"gpt-4o-mini",
                "provider_key_id":id_at(&provider.body,"/id")
            }})),
        )
        .await;
    assert_eq!(model.status, 201, "{model:?}");
    let model = id_at(&model.body, "/id");
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    fund(&cp, &admin, tenant).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let submitter = mint(&cp, &admin, tenant, member, "joint-submitter-v7").await;
    let reader = mint(&cp, &admin, tenant, member, "joint-reader-v7").await;
    let foreign_tenant = support::world::seed_tenant(&cp, &admin).await;
    fund(&cp, &admin, foreign_tenant).await;
    grant(&cp, &admin, foreign_tenant, &[model]).await;
    let foreign_member = cp.seed_member(foreign_tenant, MemberRole::Member).await.id;
    let foreign = mint(
        &cp,
        &admin,
        foreign_tenant,
        foreign_member,
        "joint-foreign-v7",
    )
    .await;
    support::world::insert_baseline_backdated(
        cp.db(),
        model,
        json!({
            "default":{"base":{"input":"4","output":"8","cache_read":"1"}},
            "batch":{"base":{"input":"2","output":"4","cache_read":"0.5"}}
        }),
        "2000-01-01T00:00:00Z"
            .parse()
            .expect("the fixture price start"),
    )
    .await;
    let gateway_a = cp.issue_bundle(&admin, "joint-gateway-a-v7").await;
    let gateway_b = cp.issue_bundle(&admin, "joint-gateway-b-v7").await;
    assert_ne!(
        gateway_a.dp_id, gateway_b.dp_id,
        "the two gateways present distinct mTLS identities"
    );
    let snapshot = projected_snapshot(&cp).await;
    Some(Fixture {
        cp,
        admin,
        upstream,
        model,
        tenant,
        member,
        submitter,
        reader,
        foreign,
        gateway_a,
        gateway_b,
        snapshot,
    })
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
            .expect("the gateway configuration");
    let (tx, rx) = mpsc::channel(128);
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        // 和生产启动一致：使用控制面 bundle 导出的原始 hex 字符串派生平台设置。
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(&cp.state().pki.platform_id_key_hex()))
        .with_native_objects(Arc::new(HttpRegistry::new(cp, bundle)))
        .with_usage_sink(UsageSink::new(tx));
    (state, rx)
}

async fn send(
    state: &ProxyState,
    method: &str,
    path: &str,
    caller: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {caller}"));
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    read(
        build_router(state.clone())
            .oneshot(request.body(body).expect("a gateway request"))
            .await
            .expect("the production router answers"),
    )
    .await
}

async fn read<B>(response: http::Response<B>) -> (StatusCode, Value)
where
    B: http_body::Body<Data = bytes::Bytes>,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let status = response.status();
    let bytes = Limited::new(response.into_body(), 1048576)
        .collect()
        .await
        .expect("read the complete gateway response")
        .to_bytes();
    let body =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, body)
}

async fn upload(state: &ProxyState, caller: &str) -> (StatusCode, Value) {
    let boundary = "joint-native-batch-v7";
    let line = json!({"custom_id":"entry-a","method":"POST","url":"/v1/chat/completions",
        "body":{"model":MODEL_NAME,"service_tier":"flex","messages":[{"role":"user","content":"classify"}]}});
    let body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{MODEL_NAME}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"joint.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{line}\n\r\n--{boundary}--\r\n");
    let request = Request::builder()
        .method("POST")
        .uri("/v1/files")
        .header("authorization", format!("Bearer {caller}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .expect("a multipart upload");
    read(
        build_router(state.clone())
            .oneshot(request)
            .await
            .expect("the upload router answers"),
    )
    .await
}

fn drain(rx: &mut mpsc::Receiver<UsageEvent>) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(serde_json::to_value(event).expect("serialize actual gateway telemetry"));
    }
    events
}

// 删除真实 begin 调用、改用错误的平台派生密钥或跳过控制面持久化，都会在首次上传处失败。
#[tokio::test]
async fn gateway_platform_identity_is_accepted_by_the_real_native_authority() {
    let Some(f) = fixture().await else {
        return;
    };
    let (gateway, _) = gateway(&f.cp, &f.gateway_a, f.snapshot.clone());
    let (status, body) = upload(&gateway, &f.submitter.bearer).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "real mTLS native begin must accept a production gateway identity: {body}"
    );
    assert_eq!(
        body["id"], INPUT_FILE,
        "the provider's raw file ID is preserved"
    );
    let owned: (Uuid, Uuid) =
        sqlx::query_as("SELECT tenant_id,api_key_id FROM native_objects WHERE raw_id=$1")
            .bind(INPUT_FILE)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("the real authority persisted ownership");
    assert_eq!(owned, (f.tenant, f.submitter.id));
    assert_eq!(
        f.upstream.requests().len(),
        1,
        "one accepted upload reaches the provider exactly once"
    );
}

// 自有文件排在 100 个其他租户文件之后；真实 owned 接口会拒绝过大的身份集合。
#[tokio::test]
async fn owned_file_listing_pages_past_one_hundred_provider_ids_through_the_real_authority() {
    let Some(f) = fixture().await else {
        return;
    };
    let (gateway_a, _) = gateway(&f.cp, &f.gateway_a, f.snapshot.clone());
    let (status, uploaded) = upload(&gateway_a, &f.submitter.bearer).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    assert_eq!(uploaded["id"], INPUT_FILE);
    let (gateway_b, _) = gateway(&f.cp, &f.gateway_b, projected_snapshot(&f.cp).await);
    let (status, listed) = send(
        &gateway_b,
        "GET",
        &format!("/v1/files?model={MODEL_NAME}&limit=1"),
        &f.reader.bearer,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owned file beyond the first provider page must remain reachable: {listed}"
    );
    let files = listed["data"].as_array().expect("a file page");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["id"], INPUT_FILE);
    assert_eq!(listed["first_id"], INPUT_FILE);
    assert_eq!(listed["last_id"], INPUT_FILE);
    assert_eq!(listed["has_more"], false);
    assert!(!listed.to_string().contains("file_joint_foreign_"));
    let requests = f.upstream.requests();
    let pages: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "GET" && request.path == "/v1/files")
        .collect();
    assert!(
        pages.len() >= 2,
        "the owned ID follows 100 unrelated provider IDs"
    );
    for page in pages {
        let query = reqwest::Url::parse(&format!("http://provider.invalid/?{}", page.query))
            .expect("the provider page query");
        let limit = query
            .query_pairs()
            .find_map(|(name, value)| {
                (name == "limit").then(|| value.parse::<usize>().expect("a numeric page limit"))
            })
            .expect("each provider page states its limit");
        assert!(
            (1..=100).contains(&limit),
            "every identity page fits the real CP authority limit: {limit}"
        );
    }
}

// 所有预期来自用户旅程：两个租户、当前授权、已受理工作和可手算的 batch 价格。
// 取消归属检查会增加 provider 请求；重复计费会改变精确行数或 210 微元总额。
#[tokio::test]
async fn accepted_batch_survives_gateway_and_cp_restart_and_settles_once_after_key_revocation() {
    let Some(f) = fixture().await else {
        return;
    };
    let (gateway_a, mut usage_a) = gateway(&f.cp, &f.gateway_a, f.snapshot.clone());
    let (gateway_b, mut usage_b) = gateway(&f.cp, &f.gateway_b, projected_snapshot(&f.cp).await);
    let (status, file) = upload(&gateway_a, &f.submitter.bearer).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "upload through the real native authority: {file}"
    );
    assert_eq!(file["id"], INPUT_FILE);
    let (status, batch) = send(
        &gateway_a,
        "POST",
        "/v1/batches",
        &f.submitter.bearer,
        Some(json!({
            "input_file_id":INPUT_FILE,"endpoint":"/v1/chat/completions","completion_window":"24h"
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "create through the real native authority: {batch}"
    );
    assert_eq!(batch["id"], BATCH);
    assert_eq!(batch["input_file_id"], INPUT_FILE);
    let (record, operation): (Uuid, Uuid) =
        sqlx::query_as("SELECT id,operation_id FROM native_objects WHERE raw_id=$1")
            .bind(BATCH)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("the accepted batch is durable");
    for (path, expected_id) in [
        (format!("/v1/files/{INPUT_FILE}"), INPUT_FILE),
        (format!("/v1/batches/{BATCH}"), BATCH),
    ] {
        let (status, body) = send(&gateway_b, "GET", &path, &f.reader.bearer, None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the second gateway resolves the original raw ID: {body}"
        );
        assert_eq!(body["id"], expected_id);
    }
    let before_denials = f.upstream.requests().len();
    for path in [
        format!("/v1/files/{INPUT_FILE}"),
        format!("/v1/batches/{BATCH}"),
        "/v1/files/file_unknown_joint_v7".to_owned(),
    ] {
        let (status, body) = send(&gateway_b, "GET", &path, &f.foreign.bearer, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "foreign or unknown raw IDs cannot probe upstream: {body}"
        );
    }
    grant(&f.cp, &f.admin, f.tenant, &[]).await;
    for path in [
        format!("/v1/files/{INPUT_FILE}"),
        format!("/v1/batches/{BATCH}"),
    ] {
        let (status, body) = send(&gateway_b, "GET", &path, &f.reader.bearer, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "current CP grants override the stale gateway snapshot: {body}"
        );
    }
    assert_eq!(
        f.upstream.requests().len(),
        before_denials,
        "every denied lookup stops before provider dispatch"
    );
    grant(&f.cp, &f.admin, f.tenant, &[f.model]).await;
    let mut telemetry = drain(&mut usage_a);
    telemetry.extend(drain(&mut usage_b));
    let delivered =
        support::world::deliver(&f.cp.dp_client(&f.gateway_a), Uuid::new_v4(), telemetry).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(delivered.body["rejected"], 0, "{delivered:?}");
    let before =
        f.cp.send(
            "GET",
            "/admin/usage/direct-models",
            Some(f.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(before.status, 200, "{before:?}");
    assert_eq!(
        before.body["totals"]["attempts"], 0,
        "uploads, acceptance and management are not inference: {before:?}"
    );
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1")
        .bind(f.submitter.id)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("the submitting key version");
    let revoked =
        f.cp.send_conditional(
            "DELETE",
            &format!("/admin/keys/{}", f.submitter.id),
            f.admin.cookie(),
            version,
            Value::Null,
        )
        .await;
    assert_eq!(revoked.status, 204, "{revoked:?}");
    drop(gateway_a);
    drop(gateway_b);
    let restarted_cp = f.cp.restart().await;
    drop(f.cp);
    // 新路由与连接池不持有原生对象缓存；旧配置刻意保留已撤销 key，迫使真实 CP 决策。
    let (recovery, mut recovery_usage) = gateway(&restarted_cp, &f.gateway_b, f.snapshot);
    let before_revoked = f.upstream.requests().len();
    let (status, body) = send(
        &recovery,
        "GET",
        &format!("/v1/batches/{BATCH}"),
        &f.submitter.bearer,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the revoked key cannot retrieve accepted work: {body}"
    );
    assert_eq!(f.upstream.requests().len(), before_revoked);
    f.upstream.complete();
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&recovery)
            .await
            .expect("gateway B settles through real CP mTLS"),
        1
    );
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&recovery)
            .await
            .expect("a second worker pass succeeds"),
        0
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let posted: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_terminal_outbox o JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1 AND o.delivered_at IS NOT NULL)")
                .bind(record).fetch_one(restarted_cp.db().pool()).await.expect("read the real terminal posting");
            if posted { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the real outbox worker posts the revoked submitter's accepted batch");
    let (entry_id, event, charge): (String, Value, i64) = sqlx::query_as("SELECT o.entry_id,o.event,c.charge_micro_cny FROM native_terminal_outbox o JOIN pricing_charges_cny c ON c.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(restarted_cp.db().pool()).await.expect("a priced terminal event");
    assert_eq!(event["team_id"], json!(f.tenant));
    assert_eq!(event["user_id"], json!(f.member));
    assert_eq!(event["api_key_id"], json!(f.submitter.id));
    assert_eq!(event["native_operation_id"], json!(operation));
    assert_eq!(entry_id, "entry-a");
    assert_eq!(event["inference_stage"], "terminal");
    assert_eq!(event["is_batch"], true);
    assert_eq!(event["requested_service_tier"], "flex");
    assert_eq!(event["prompt_tokens"], 100);
    assert_eq!(event["cached_prompt_tokens"], 20);
    assert_eq!(event["completion_tokens"], 10);
    assert_eq!(event["total_tokens"], 110);
    assert_eq!(
        charge, 210,
        "80 uncached*2 + 20 cached*0.5 + 10 output*4, at one CNY per USD"
    );
    let result_owner: (Uuid, Uuid, Uuid, Value) = sqlx::query_as("SELECT n.parent_id,n.tenant_id,n.api_key_id,o.submission FROM native_objects n JOIN native_operations o ON o.environment_id=n.environment_id AND o.operation_id=n.operation_id WHERE n.raw_id=$1")
        .bind(OUTPUT_FILE).fetch_one(restarted_cp.db().pool()).await.expect("the worker atomically binds the raw result file");
    assert_eq!(
        (result_owner.0, result_owner.1, result_owner.2),
        (record, f.tenant, f.submitter.id)
    );
    assert_eq!(result_owner.3["entries"][0]["entry_id"], "entry-a");
    assert_eq!(
        result_owner.3["entries"][0]["requested_model_id"],
        json!(f.model)
    );
    let (status, body) = send(
        &recovery,
        "GET",
        &format!("/v1/files/{OUTPUT_FILE}/content"),
        &f.reader.bearer,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the current owner key can retrieve the retained result: {body}"
    );
    let mut expected_result = native_gateway::result_line();
    expected_result["response"]["body"]["model"] = json!(MODEL_NAME);
    assert_eq!(body, expected_result);
    let before_result_denials = f.upstream.requests().len();
    for bearer in [&f.submitter.bearer, &f.foreign.bearer] {
        let (status, body) = send(
            &recovery,
            "GET",
            &format!("/v1/files/{OUTPUT_FILE}/content"),
            bearer,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "revoked and foreign keys cannot read the result: {body}"
        );
    }
    assert_eq!(f.upstream.requests().len(), before_result_denials);
    let management = drain(&mut recovery_usage);
    let delivered = support::world::deliver(
        &restarted_cp.dp_client(&f.gateway_b),
        Uuid::new_v4(),
        management,
    )
    .await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(delivered.body["rejected"], 0, "{delivered:?}");
    // 重放真实终态事件并再次运行观察器，不得再记一次推理或扣费。
    let replay = support::world::deliver(
        &restarted_cp.dp_client(&f.gateway_b),
        Uuid::new_v4(),
        vec![event],
    )
    .await;
    assert_eq!(replay.status, 200, "{replay:?}");
    assert_eq!(replay.body["rejected"], 0, "{replay:?}");
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&recovery)
            .await
            .expect("completed work remains idempotent"),
        0
    );
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT count(*),count(l.event_id),COALESCE(sum(-l.amount_micro_cny),0)::bigint FROM native_terminal_outbox o LEFT JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(restarted_cp.db().pool()).await.expect("count immutable terminal postings");
    assert_eq!(counts, (1, 1, 210));
    let after = restarted_cp
        .send(
            "GET",
            "/admin/usage/direct-models",
            Some(f.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(after.status, 200, "{after:?}");
    assert_eq!(
        after.body["totals"]["attempts"], 1,
        "management and replay add no LLM attempt: {after:?}"
    );
    assert_eq!(after.body["totals"]["succeeded_attempts"], 1);
    let requests = f.upstream.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/v1/files")
            .count(),
        2,
        "one original upload and one internal staged file are management work"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/v1/batches")
            .count(),
        1
    );
    assert_eq!(
        requests.iter().filter(|r| r.method == "POST").count(),
        3,
        "two file-management uploads and exactly one batch create, without redispatch"
    );
    let batch_create = requests
        .iter()
        .find(|r| r.method == "POST" && r.path == "/v1/batches")
        .expect("the single batch create");
    let batch_create: Value =
        serde_json::from_slice(&batch_create.body).expect("the actual upstream batch create");
    assert_eq!(batch_create["input_file_id"], native_gateway::STAGED_FILE);
    let expected_file = |model: &str| {
        format!("{}\n", json!({
        "custom_id":"entry-a","method":"POST","url":"/v1/chat/completions",
        "body":{"model":model,"service_tier":"flex","messages":[{"role":"user","content":"classify"}]}
    })).into_bytes()
    };
    assert_eq!(
        f.upstream.file_bytes(INPUT_FILE),
        expected_file(MODEL_NAME),
        "the caller's original file remains byte-for-byte intact with its public model alias"
    );
    assert_eq!(
        f.upstream.file_bytes(native_gateway::STAGED_FILE),
        expected_file("gpt-4o-mini"),
        "the separate staged file contains the complete remapped provider request"
    );
}

// 裸响应、item、call 和分页游标来自供应商；归属在重启后仍由真实控制面判定。
#[tokio::test]
async fn raw_response_graph_survives_cp_restart_and_continues_from_returned_input_item() {
    use native_gateway::{
        CONTINUED_RESPONSE, FUNCTION_CALL, FUNCTION_ITEM, INPUT_FIRST, INPUT_LAST, OUTPUT_MESSAGE,
        RESPONSE,
    };

    let Some(f) = fixture().await else {
        return;
    };
    let (gateway_a, _) = gateway(&f.cp, &f.gateway_a, f.snapshot.clone());
    let (status, created) = send(
        &gateway_a,
        "POST",
        "/v1/responses",
        &f.submitter.bearer,
        Some(json!({
            "model":MODEL_NAME,"store":true,
            "input":[
                {"role":"user","content":"classify this text"},
                {"role":"user","content":"return the classification"}
            ],
            "tools":[{"type":"function","name":"classify","description":"Return the classification",
                "parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false},"strict":true}]
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the real authority accepts native Responses: {created}"
    );
    assert_eq!(created["id"], RESPONSE);
    assert_eq!(created["output"][0]["id"], OUTPUT_MESSAGE);
    assert_eq!(created["output"][1]["id"], FUNCTION_ITEM);
    assert_eq!(created["output"][1]["call_id"], FUNCTION_CALL);
    let parent: Uuid = sqlx::query_scalar(
        "SELECT id FROM native_objects WHERE kind='response' AND raw_id=$1 AND tenant_id=$2",
    )
    .bind(RESPONSE)
    .bind(f.tenant)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("the raw response has durable real authority");
    let children: Vec<String> = sqlx::query_scalar("SELECT raw_id FROM native_objects WHERE parent_id=$1 AND kind='response_item' ORDER BY raw_id")
        .bind(parent).fetch_all(f.cp.db().pool()).await.expect("the raw output items have durable real authority");
    assert_eq!(
        children,
        vec![FUNCTION_ITEM.to_owned(), OUTPUT_MESSAGE.to_owned()]
    );
    drop(gateway_a);
    let restarted_cp = f.cp.restart().await;
    drop(f.cp);
    let (gateway_b, _) = gateway(&restarted_cp, &f.gateway_b, f.snapshot);
    let (status, retrieved) = send(
        &gateway_b,
        "GET",
        &format!("/v1/responses/{RESPONSE}"),
        &f.reader.bearer,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the fresh second gateway resolves the stored raw response: {retrieved}"
    );
    assert_eq!(retrieved["id"], RESPONSE);
    assert_eq!(retrieved["output"][0]["id"], OUTPUT_MESSAGE);
    assert_eq!(retrieved["output"][1]["id"], FUNCTION_ITEM);
    assert_eq!(retrieved["output"][1]["call_id"], FUNCTION_CALL);
    let (status, inputs) = send(
        &gateway_b,
        "GET",
        &format!("/v1/responses/{RESPONSE}/input_items?limit=2&order=asc"),
        &f.reader.bearer,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the fresh gateway can bind newly observed raw input items: {inputs}"
    );
    assert_eq!(
        inputs["data"]
            .as_array()
            .expect("the response input items")
            .len(),
        2
    );
    assert_eq!(inputs["data"][0]["id"], INPUT_FIRST);
    assert_eq!(inputs["data"][1]["id"], INPUT_LAST);
    assert_eq!(inputs["first_id"], INPUT_FIRST);
    assert_eq!(inputs["last_id"], INPUT_LAST);
    assert_eq!(inputs["has_more"], false);
    let (status, tail) = send(
        &gateway_b,
        "GET",
        &format!("/v1/responses/{RESPONSE}/input_items?after={INPUT_FIRST}&limit=1&order=asc"),
        &f.reader.bearer,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a returned raw item ID remains a usable cursor: {tail}"
    );
    assert_eq!(tail["data"].as_array().expect("the cursor page").len(), 1);
    assert_eq!(tail["data"][0]["id"], INPUT_LAST);
    assert_eq!(tail["first_id"], INPUT_LAST);
    assert_eq!(tail["last_id"], INPUT_LAST);
    assert_eq!(tail["has_more"], false);
    let input_owner: (Uuid, Uuid) = sqlx::query_as(
        "SELECT parent_id,tenant_id FROM native_objects WHERE kind='response_item' AND raw_id=$1",
    )
    .bind(INPUT_LAST)
    .fetch_one(restarted_cp.db().pool())
    .await
    .expect("a returned input item is durably attached to its response");
    assert_eq!(input_owner, (parent, f.tenant));
    let (status, continued) = send(
        &gateway_b, "POST", "/v1/responses", &f.reader.bearer,
        Some(json!({"model":MODEL_NAME,"store":true,"previous_response_id":retrieved["id"],"input":[
            {"type":"item_reference","id":tail["data"][0]["id"]},
            {"type":"function_call_output","call_id":retrieved["output"][1]["call_id"],"output":"classified"}
        ]})),
    ).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "returned raw response, item and call IDs can continue the same account: {continued}"
    );
    assert_eq!(continued["id"], CONTINUED_RESPONSE);
    let before_denials = f.upstream.requests().len();
    for (bearer, response) in [
        (&f.foreign.bearer, RESPONSE),
        (&f.reader.bearer, "resp_joint_unknown_v7"),
    ] {
        for suffix in ["", "/input_items"] {
            let (status, body) = send(
                &gateway_b,
                "GET",
                &format!("/v1/responses/{response}{suffix}"),
                bearer,
                None,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "foreign and unknown raw Responses cannot probe the provider: {body}"
            );
        }
        let (status, body) = send(
            &gateway_b,
            "POST",
            "/v1/responses",
            bearer,
            Some(json!({"model":MODEL_NAME,"previous_response_id":response,"input":"continue"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "foreign and unknown previous responses are denied: {body}"
        );
        assert_eq!(body["error"]["code"], "previous_response_not_found");
    }
    for (bearer, item) in [
        (&f.foreign.bearer, INPUT_LAST),
        (&f.reader.bearer, "msg_joint_unknown_v7"),
    ] {
        let (status, body) = send(
            &gateway_b,
            "POST",
            "/v1/responses",
            bearer,
            Some(json!({"model":MODEL_NAME,"input":[{"type":"item_reference","id":item}]})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "foreign and unknown raw item references are denied: {body}"
        );
    }
    assert_eq!(
        f.upstream.requests().len(),
        before_denials,
        "every rejected raw identity stops at the real CP authority before upstream"
    );
    let requests = f.upstream.requests();
    let creates: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "POST" && request.path == "/v1/responses")
        .collect();
    assert_eq!(
        creates.len(),
        2,
        "management and denied continuations cannot create extra provider responses"
    );
    let forwarded: Value =
        serde_json::from_slice(&creates[1].body).expect("the actual continued upstream request");
    assert_eq!(forwarded["previous_response_id"], RESPONSE);
    assert_eq!(forwarded["input"][0]["type"], "item_reference");
    assert_eq!(forwarded["input"][0]["id"], INPUT_LAST);
    assert_eq!(forwarded["input"][1]["call_id"], FUNCTION_CALL);
    let cursor = requests
        .iter()
        .find(|request| request.path.ends_with("/input_items") && request.query.contains("after="))
        .expect("the raw cursor was forwarded");
    let query = reqwest::Url::parse(&format!("http://provider.invalid/?{}", cursor.query))
        .expect("the upstream cursor query");
    assert_eq!(
        query
            .query_pairs()
            .find(|(key, _)| key == "after")
            .map(|(_, value)| value.into_owned()),
        Some(INPUT_FIRST.to_owned())
    );
}
