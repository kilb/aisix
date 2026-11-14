//! 托管原生对象的提交必须先持久化，归属由控制面的当前密钥记录决定。

mod support;

use aisix_control_plane::auth::OperatorRole;
use ring::hmac;
use serde_json::{json, Value};
use support::world::{seed_key, seed_tenant};
use support::{DpClient, Resp, TestCp};
use uuid::Uuid;

const BEGIN: &str = "/dp/v1/native-objects/begin";
const ACCOUNT_BASE: &str = "https://native-registry.example/v1";
const ACCOUNT_CREDENTIAL: &str = "sk-native-registry-fixture-7c1";

fn id_at(body: &Value, pointer: &str) -> Uuid {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(|| panic!("a UUID at {pointer} in {body}"))
}

// 固定的内部契约采用长度前缀，避免不同账号字段拼成相同输入。
fn fixture_boundary(cp: &TestCp, provider_key: Uuid) -> String {
    let key = aisix_core::native_objects::platform_id_key_from_secret(
        &cp.state().pki.platform_id_key_hex(),
    );
    let mut input = b"aisix-native-origin-v1\0".to_vec();
    let provider_key = provider_key.to_string();
    for field in [
        provider_key.as_str(),
        "openai",
        "openai",
        ACCOUNT_BASE,
        ACCOUNT_CREDENTIAL,
    ] {
        input.extend_from_slice(&(field.len() as u64).to_be_bytes());
        input.extend_from_slice(field.as_bytes());
    }
    // 此账号没有作用域请求头；非零个数时依名称排序后写入名称和值。
    input.extend_from_slice(&0_u64.to_be_bytes());
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), &input);
    format!("v1:{}", hex::encode(tag.as_ref()))
}

// 此夹具只创建控制面资源，不访问上游，也不伪造推理结果。
async fn submission(cp: &TestCp) -> (Value, Uuid, Uuid, Uuid) {
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": "native-account-7c1",
                "provider": "openai",
                "api_key": ACCOUNT_CREDENTIAL,
                "api_base": ACCOUNT_BASE,
                "groups": ["shared"],
            }})),
        )
        .await;
    assert_eq!(account.status, 201, "{account:?}");
    let provider_key = id_at(&account.body, "/id");
    let model = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": "native-response-7c1",
                "provider": "openai",
                "model_name": "gpt-4o-mini",
                "provider_key_id": provider_key,
            }})),
        )
        .await;
    assert_eq!(model.status, 201, "{model:?}");
    let model = id_at(&model.body, "/id");
    let tenant = seed_tenant(cp, &admin).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("the current tenant version");
    let grant = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids": [model]}),
        )
        .await;
    assert_eq!(grant.status, 200, "{grant:?}");
    let member = cp
        .seed_member(tenant, aisix_control_plane::auth::MemberRole::Member)
        .await;
    let key = Uuid::parse_str(&seed_key(cp, &admin, tenant, Some(member.id)).await)
        .expect("the current member's key");
    let operation = Uuid::new_v4();
    (
        json!({
            "operation_id": operation,
            "api_key_id": key,
            "protocol": "openai",
            "kind": "response",
            "origin": {
                "model_id": model,
                "provider_key_id": provider_key,
                "boundary": fixture_boundary(cp, provider_key),
            },
            "parent_id": null,
            "requested_model_id": model,
            "requested_model": "native-response-7c1",
            "upstream_model": "gpt-4o-mini",
            "operation": "responses",
            "request_id": "req-native-submission-7c1",
            "asynchronous": true,
        }),
        tenant,
        member.id,
        key,
    )
}

// 删除提交的持久化或只回显请求会使数据库归属断言失败；缺少路由先以 404 失败。
#[tokio::test]
async fn a_native_submission_persists_the_authenticated_keys_authoritative_owner() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, tenant, member, key) = submission(&cp).await;
    let operation = id_at(&body, "/operation_id");
    let dp = cp.issue_dp_client("gw-native-submit-7c1").await;
    let registered = dp.send("POST", BEGIN, Some(body.clone())).await;
    assert_eq!(registered.status, 200, "{registered:?}");
    assert_eq!(registered.body["operation_id"], json!(operation));
    assert_eq!(registered.body["state"], "pending_submission");
    assert_eq!(registered.body["origin"], body["origin"]);

    let authority: (Uuid, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT tenant_id, api_key_id, member_id FROM native_operations \
         WHERE environment_id = $1 AND operation_id = $2",
    )
    .bind(cp.env().env_id)
    .bind(operation)
    .fetch_one(cp.db().pool())
    .await
    .expect("a durable submission in the authenticated environment");
    assert_eq!(authority, (tenant, key, Some(member)));

    let second = cp.restart().await;
    let dp2 = second.issue_dp_client("gw-native-replay-7c1").await;
    let replay = dp2.send("POST", BEGIN, Some(body)).await;
    assert_eq!(replay.status, 200, "{replay:?}");
    assert_eq!(replay.body["operation_id"], json!(operation));
    let copies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM native_operations \
         WHERE environment_id = $1 AND operation_id = $2",
    )
    .bind(second.env().env_id)
    .bind(operation)
    .fetch_one(second.db().pool())
    .await
    .expect("count the durable submission after replay from another gateway");
    assert_eq!(copies, 1);
}

async fn bind_submission(dp: &DpClient, body: &Value, raw_id: &str) -> Value {
    let begun = dp.send("POST", BEGIN, Some(body.clone())).await;
    assert_eq!(begun.status, 200, "{begun:?}");
    let bound = dp
        .send(
            "POST",
            "/dp/v1/native-objects/bind",
            Some(json!({
                "operation_id": body["operation_id"],
                "api_key_id": body["api_key_id"],
                "raw_id": raw_id,
            })),
        )
        .await;
    assert_eq!(bound.status, 200, "{bound:?}");
    assert_eq!(bound.body["record"]["raw_id"], raw_id);
    bound.body["record"].clone()
}

// 真实提交、租约与 outbox 链路：受理/轮询不算推理，重复完成不能增加分母。
#[tokio::test]
async fn background_monitoring_counts_durable_inference_once_and_keeps_real_retries() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (mut body, tenant, member, key) = submission(&cp).await;
    let admin = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let dp = cp.issue_dp_client("gw-native-monitoring-v4").await;
    let model = id_at(&body, "/origin/model_id");
    let account = id_at(&body, "/origin/provider_key_id");
    // 调用方重复 request_id 不得把不同原生操作合并。
    body["request_id"] = json!("same-client-request-id");
    let mut excluded = 0;
    for (index, (terminal_status, http_status)) in
        [("completed", 200), ("failed", 502), ("cancelled", 499)]
            .into_iter()
            .enumerate()
    {
        body["operation_id"] = json!(Uuid::new_v4());
        let operation = id_at(&body, "/operation_id");
        let record = bind_submission(&dp, &body, &format!("resp-monitoring-{index}")).await;
        let record_id = id_at(&record, "/record_id");
        let event = |stage: &str, status: u16| {
            json!({
                "event_id": Uuid::new_v4(), "request_id": body["request_id"],
                "occurred_at": chrono::Utc::now().to_rfc3339(),
                "team_id": tenant, "user_id": member, "api_key_id": key,
                "model_id": model, "requested_model_id": model, "provider_key_id": account,
                "operation": "responses", "upstream_dispatched": true,
                "native_operation_id": operation, "inference_stage": stage,
                "is_final_attempt": false, "status_code": status,
                "prompt_tokens": 0, "completion_tokens": 0, "upstream_latency_ms": 10,
            })
        };
        let mut rows = vec![event("accepted", 200)];
        rows.extend((0..3).map(|_| event("management", 200)));
        if index == 0 {
            let mut retry = event("attempt", 503);
            retry["error_class"] = json!("upstream_status");
            rows.push(retry);
        }
        let delivered = support::world::deliver(&dp, Uuid::new_v4(), rows).await;
        assert_eq!(delivered.status, 200, "{delivered:?}");
        assert_eq!(delivered.body["rejected"], 0, "{delivered:?}");
        excluded += 4;
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
            before.body["totals"]["attempts"],
            json!(index + 1),
            "受理不能成为新推理结果: {before:?}"
        );
        assert_eq!(before.body["totals"]["excluded_attempts"], excluded);
        let claim_body = json!({"limit": 1, "record_id": record_id});
        let claim = dp
            .send(
                "POST",
                "/dp/v1/native-objects/claim",
                Some(claim_body.clone()),
            )
            .await;
        assert_eq!(claim.status, 200, "{claim:?}");
        let generation = claim.body["records"][0]["lease_generation"]
            .as_i64()
            .expect("租约代数");
        let recovery;
        let (complete_dp, generation) = if index == 0 {
            sqlx::query("UPDATE native_objects SET lease_expires_at = clock_timestamp() - interval '1 second' WHERE id = $1")
                .bind(record_id).execute(cp.db().pool()).await.expect("使隔离租约到期");
            recovery = cp.issue_dp_client("gw-native-monitor-recovery-v4").await;
            let claim = recovery
                .send("POST", "/dp/v1/native-objects/claim", Some(claim_body))
                .await;
            assert_eq!(claim.status, 200, "{claim:?}");
            let renewed = claim.body["records"][0]["lease_generation"]
                .as_i64()
                .expect("恢复代数");
            assert!(renewed > generation);
            (&recovery, renewed)
        } else {
            (&dp, generation)
        };
        let complete = json!({"lease": {"record_id": record_id, "generation": generation},
            "terminal_status": terminal_status, "usage": [{"entry_id": "",
            "occurred_at": chrono::Utc::now().to_rfc3339(), "status_code": http_status,
            "prompt_tokens": 40, "completion_tokens": 10, "total_tokens": 50}]});
        for _ in 0..2 {
            let done = complete_dp
                .send(
                    "POST",
                    "/dp/v1/native-objects/complete",
                    Some(complete.clone()),
                )
                .await;
            assert_eq!(done.status, 200, "{done:?}");
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let delivered: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM native_terminal_outbox WHERE object_id = $1 AND delivered_at IS NOT NULL)")
                    .bind(record_id).fetch_one(cp.db().pool()).await.expect("终态投递");
                if delivered { break; }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }).await.expect("真实 outbox 完成投递");
        let (copies,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM native_terminal_outbox WHERE object_id = $1")
                .bind(record_id)
                .fetch_one(cp.db().pool())
                .await
                .expect("持久终态数量");
        assert_eq!(copies, 1);
        let stored: Value =
            sqlx::query_scalar("SELECT event FROM native_terminal_outbox WHERE object_id = $1")
                .bind(record_id)
                .fetch_one(cp.db().pool())
                .await
                .expect("终态事实");
        assert_eq!(stored["inference_stage"], "terminal");
        assert_eq!(stored["native_operation_id"], json!(operation));
        assert_eq!(stored["native_terminal_status"], terminal_status);
        assert_eq!(stored["upstream_dispatched"], true);
        // outbox 在确认投递前重试同一事件，换 telemetry batch 也不能重复纳入统计。
        let duplicate = support::world::deliver(&dp, Uuid::new_v4(), vec![stored]).await;
        assert_eq!(duplicate.status, 200, "{duplicate:?}");
        assert_eq!(duplicate.body["rejected"], 0, "{duplicate:?}");
        let poll =
            support::world::deliver(&dp, Uuid::new_v4(), vec![event("management", 200)]).await;
        assert_eq!(poll.status, 200, "{poll:?}");
        excluded += 1;
    }
    let response = cp
        .send(
            "GET",
            "/admin/usage/direct-models",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(response.status, 200, "{response:?}");
    let totals = &response.body["totals"];
    assert_eq!(totals["attributed_attempts"], 19);
    assert_eq!(totals["attempts"], 4);
    assert_eq!(totals["excluded_attempts"], excluded);
    assert_eq!(totals["unmeasured_attempts"], 0);
    assert_eq!(totals["succeeded_attempts"], 1);
    assert_eq!(totals["failed_attempts"], 2);
    assert_eq!(totals["unknown_outcome_attempts"], 1);
    let failure_rate = totals["failure_rate"].as_f64().expect("已知推理结果分母");
    assert!((failure_rate - 2.0 / 3.0).abs() < 0.000001);
    assert_eq!(totals["tokens"]["prompt_tokens"], 120);
    assert_eq!(totals["tokens"]["completion_tokens"], 30);
}

async fn lookup(dp: &DpClient, key: Uuid, raw_id: &str) -> Resp {
    dp.send(
        "POST",
        "/dp/v1/native-objects/lookup",
        Some(json!({"api_key_id": key, "locator": {
            "protocol": "openai", "kind": "response", "raw_id": raw_id,
        }})),
    )
    .await
}

// 去掉成员过滤、缓存旧角色或把密钥停用误当作仅影响新请求，都会放行这些读取。
#[tokio::test]
async fn raw_object_access_follows_current_member_visibility_and_key_admission() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, tenant, member, key) = submission(&cp).await;
    let dp = cp.issue_dp_client("gw-native-owner-7c1").await;
    let record = bind_submission(&dp, &body, "resp-owned-7c1").await;
    let known = lookup(&dp, key, "resp-owned-7c1").await;
    assert_eq!(known.status, 200, "{known:?}");
    assert_eq!(known.body["record"]["record_id"], record["record_id"]);

    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let same_member_key =
        Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(member)).await).expect("a key");
    let same_member = lookup(&dp, same_member_key, "resp-owned-7c1").await;
    assert_eq!(same_member.status, 200, "{same_member:?}");
    let peer = cp
        .seed_member(tenant, aisix_control_plane::auth::MemberRole::Member)
        .await;
    let peer_key =
        Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(peer.id)).await).expect("a peer key");
    let foreign = lookup(&dp, peer_key, "resp-owned-7c1").await;
    let missing = lookup(&dp, peer_key, "resp-missing-7c1").await;
    assert_eq!(foreign.status, 404, "{foreign:?}");
    assert_eq!(foreign.body, missing.body);

    sqlx::query("UPDATE members SET role = 'admin' WHERE id = $1")
        .bind(peer.id)
        .execute(cp.db().pool())
        .await
        .expect("promote the peer");
    let promoted = lookup(&dp, peer_key, "resp-owned-7c1").await;
    assert_eq!(promoted.status, 200, "{promoted:?}");
    sqlx::query("UPDATE members SET role = 'member' WHERE id = $1")
        .bind(peer.id)
        .execute(cp.db().pool())
        .await
        .expect("restore the peer role");
    assert_eq!(lookup(&dp, peer_key, "resp-owned-7c1").await.status, 404);

    sqlx::query("UPDATE tenant_keys SET disabled = true WHERE id = $1")
        .bind(key)
        .execute(cp.db().pool())
        .await
        .expect("disable the original key");
    assert_eq!(lookup(&dp, key, "resp-owned-7c1").await.status, 404);
    let same_member = lookup(&dp, same_member_key, "resp-owned-7c1").await;
    assert_eq!(same_member.status, 200, "{same_member:?}");
}

// 只信网关声明的摘要会接受伪造值；散列整份账号文档则会错误地拒绝纯名称修改。
#[tokio::test]
async fn origin_is_recomputed_from_current_account_material_and_ignores_display_names() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, _, _, key) = submission(&cp).await;
    let dp = cp.issue_dp_client("gw-native-origin-7c1").await;
    bind_submission(&dp, &body, "resp-origin-7c1").await;
    let mut forged = body.clone();
    forged["operation_id"] = json!(Uuid::new_v4());
    forged["origin"]["boundary"] = json!(format!("v1:{}", "00".repeat(32)));
    let refused = dp.send("POST", BEGIN, Some(forged.clone())).await;
    assert_eq!(refused.status, 409, "{refused:?}");
    assert_eq!(refused.body["code"], "native_origin_changed");
    let persisted: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM native_operations WHERE operation_id = $1)",
    )
    .bind(id_at(&forged, "/operation_id"))
    .fetch_one(cp.db().pool())
    .await
    .expect("look for the forged submission");
    assert!(!persisted);

    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let path = format!(
        "/admin/resources/provider_keys/{}",
        id_at(&body, "/origin/provider_key_id")
    );
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let mut doc = current.body["doc"].clone();
    doc["display_name"] = json!("native-account-renamed-7c1");
    let renamed = cp
        .send_conditional(
            "PUT",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().expect("a version"),
            json!({"doc": doc}),
        )
        .await;
    assert_eq!(renamed.status, 200, "{renamed:?}");
    let found = lookup(&dp, key, "resp-origin-7c1").await;
    assert_eq!(found.status, 200, "{found:?}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    let mut doc = current.body["doc"].clone();
    doc["api_key"] = json!("sk-native-registry-rotated-7c1");
    let rotated = cp
        .send_conditional(
            "PUT",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().expect("a version"),
            json!({"doc": doc}),
        )
        .await;
    assert_eq!(rotated.status, 200, "{rotated:?}");
    let refused = lookup(&dp, key, "resp-origin-7c1").await;
    assert_eq!(refused.status, 409, "{refused:?}");
    assert_eq!(refused.body["code"], "native_origin_changed");
}

// 删除租约代次检查会让旧进程提交；删除永久确认会在用量保留期后再次记账。
#[tokio::test]
async fn terminal_accounting_survives_lease_reclaim_and_usage_retention_without_replay() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, tenant, member, key) = submission(&cp).await;
    let dp = cp.issue_dp_client("gw-native-terminal-7c1").await;
    let record = bind_submission(&dp, &body, "resp-terminal-7c1").await;
    let id = id_at(&record, "/record_id");
    let claim = json!({"limit": 1, "record_id": id});
    let first = dp
        .send("POST", "/dp/v1/native-objects/claim", Some(claim.clone()))
        .await;
    assert_eq!(first.status, 200, "{first:?}");
    let generation1 = first.body["records"][0]["lease_generation"]
        .as_i64()
        .expect("a lease generation");
    assert_eq!(first.body["records"][0]["record_id"], json!(id));
    let dp2 = cp.issue_dp_client("gw-native-terminal-7c2").await;
    let held = dp2
        .send("POST", "/dp/v1/native-objects/claim", Some(claim.clone()))
        .await;
    assert_eq!(held.status, 200, "{held:?}");
    assert!(held.body["records"]
        .as_array()
        .expect("an explicit claim list")
        .is_empty());
    sqlx::query("UPDATE native_objects SET lease_expires_at = clock_timestamp() - interval '1 second' WHERE id = $1")
        .bind(id).execute(cp.db().pool()).await.expect("expire the first lease");
    let second = dp2
        .send("POST", "/dp/v1/native-objects/claim", Some(claim))
        .await;
    assert_eq!(second.status, 200, "{second:?}");
    let generation2 = second.body["records"][0]["lease_generation"]
        .as_i64()
        .expect("a renewed generation");
    assert!(generation2 > generation1);
    let mut complete = json!({
        "lease": {"record_id": id, "generation": generation1},
        "terminal_status": "completed",
        "usage": [{"entry_id": "", "occurred_at": chrono::Utc::now().to_rfc3339(),
            "status_code": 200, "prompt_tokens": 40, "completion_tokens": 10,
            "total_tokens": 50, "cache_usage_reported": true, "team_id": Uuid::new_v4(), "api_key_id": Uuid::new_v4()}],
    });
    let stale = dp
        .send(
            "POST",
            "/dp/v1/native-objects/complete",
            Some(complete.clone()),
        )
        .await;
    assert_eq!(stale.status, 409, "{stale:?}");
    complete["lease"]["generation"] = json!(generation2);
    let done = dp2
        .send(
            "POST",
            "/dp/v1/native-objects/complete",
            Some(complete.clone()),
        )
        .await;
    assert_eq!(done.status, 200, "{done:?}");
    assert_eq!(done.body["state"], "completed");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let delivered: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM native_terminal_outbox WHERE object_id = $1 AND delivered_at IS NOT NULL)",
            ).bind(id).fetch_one(cp.db().pool()).await.expect("observe durable delivery");
            if delivered { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("the real outbox worker delivers the terminal event");
    let (event_id, event): (Uuid, Value) =
        sqlx::query_as("SELECT event_id, event FROM native_terminal_outbox WHERE object_id = $1")
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("the immutable terminal event");
    assert_eq!(event["team_id"], json!(tenant));
    assert_eq!(event["api_key_id"], json!(key));
    assert_eq!(event["user_id"], json!(member));
    assert_eq!(event["prompt_tokens"], 40);
    assert_eq!(event["total_tokens"], 50);
    assert_eq!(event["cache_usage_reported"], true);
    let priced: String =
        sqlx::query_scalar("SELECT status FROM pricing_charges_cny WHERE event_id = $1")
            .bind(event_id.to_string())
            .fetch_one(cp.db().pool())
            .await
            .expect("the normal pricing pipeline processed this event");
    assert_eq!(
        priced, "unpriced",
        "the fixture intentionally configured no baseline"
    );
    sqlx::query("DELETE FROM pricing_charges_cny WHERE event_id = $1")
        .bind(event_id.to_string())
        .execute(cp.db().pool())
        .await
        .expect("expire the unpriced fixture charge");
    sqlx::query("DELETE FROM usage_events WHERE event_id = $1")
        .bind(event_id.to_string())
        .execute(cp.db().pool())
        .await
        .expect("expire the ordinary usage row");
    sqlx::query("DELETE FROM usage_batches WHERE batch_id = $1")
        .bind(event_id)
        .execute(cp.db().pool())
        .await
        .expect("expire ordinary batch deduplication");
    let replay = dp2
        .send("POST", "/dp/v1/native-objects/complete", Some(complete))
        .await;
    assert_eq!(replay.status, 200, "{replay:?}");
    let terminal_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox WHERE object_id = $1")
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("count permanent terminal identities");
    assert_eq!(terminal_rows, 1);
    let revived: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM usage_events WHERE event_id = $1)")
            .bind(event_id.to_string())
            .fetch_one(cp.db().pool())
            .await
            .expect("look for a late duplicate delivery");
    assert!(!revived);
}

// 每种失效状态都独立从一次成功读取出发；漏掉任一条件都会读回已知对象。
#[tokio::test]
async fn every_current_key_tenant_member_and_group_admission_condition_closes_object_access() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, tenant, member, key) = submission(&cp).await;
    let dp = cp.issue_dp_client("gw-native-admission-7c1").await;
    bind_submission(&dp, &body, "resp-admission-7c1").await;
    let conditions = [
        ("UPDATE tenant_keys SET operator_disabled = true WHERE id = $1", "UPDATE tenant_keys SET operator_disabled = false WHERE id = $1", key),
        ("UPDATE tenant_keys SET expires_at = clock_timestamp() - interval '1 second' WHERE id = $1", "UPDATE tenant_keys SET expires_at = NULL WHERE id = $1", key),
        ("UPDATE tenants SET status = 'suspended' WHERE id = $1", "UPDATE tenants SET status = 'active' WHERE id = $1", tenant),
        ("UPDATE members SET disabled = true WHERE id = $1", "UPDATE members SET disabled = false WHERE id = $1", member),
        ("UPDATE members SET removed_at = clock_timestamp() WHERE id = $1", "UPDATE members SET removed_at = NULL WHERE id = $1", member),
        ("UPDATE members SET role = 'billing' WHERE id = $1", "UPDATE members SET role = 'member' WHERE id = $1", member),
        ("UPDATE tenants SET allowed_provider_groups = '{}' WHERE id = $1", "UPDATE tenants SET allowed_provider_groups = ARRAY['shared'] WHERE id = $1", tenant),
    ];
    for (close, restore, id) in conditions {
        let before = lookup(&dp, key, "resp-admission-7c1").await;
        assert_eq!(before.status, 200, "before {close}: {before:?}");
        sqlx::query(close)
            .bind(id)
            .execute(cp.db().pool())
            .await
            .expect("apply one current admission change");
        let refused = lookup(&dp, key, "resp-admission-7c1").await;
        assert_eq!(refused.status, 404, "after {close}: {refused:?}");
        sqlx::query(restore)
            .bind(id)
            .execute(cp.db().pool())
            .await
            .expect("restore the admitted fixture");
    }
    assert_eq!(lookup(&dp, key, "resp-admission-7c1").await.status, 200);
}

// 两个租户都有相同模型和账号的权限；只漏掉租户过滤就会泄露原始对象 ID。
#[tokio::test]
async fn another_tenants_object_is_indistinguishable_from_an_unknown_raw_id() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, _, _, key) = submission(&cp).await;
    let dp = cp.issue_dp_client("gw-native-isolation-7c1").await;
    bind_submission(&dp, &body, "resp-private-7c1").await;
    assert_eq!(lookup(&dp, key, "resp-private-7c1").await.status, 200);
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = seed_tenant(&cp, &admin).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("the other tenant's version");
    let grant = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids": [body["requested_model_id"]]}),
        )
        .await;
    assert_eq!(grant.status, 200, "{grant:?}");
    let member = cp
        .seed_member(tenant, aisix_control_plane::auth::MemberRole::Owner)
        .await;
    let other_key = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(member.id)).await)
        .expect("the other tenant's key");
    let mut own_submission = body.clone();
    own_submission["operation_id"] = json!(Uuid::new_v4());
    own_submission["api_key_id"] = json!(other_key);
    bind_submission(&dp, &own_submission, "resp-other-owned-7c1").await;
    let known_own = lookup(&dp, other_key, "resp-other-owned-7c1").await;
    assert_eq!(known_own.status, 200, "{known_own:?}");
    let foreign = lookup(&dp, other_key, "resp-private-7c1").await;
    let unknown = lookup(&dp, other_key, "resp-unknown-7c1").await;
    assert_eq!(foreign.status, 404, "{foreign:?}");
    assert_eq!(foreign.status, unknown.status);
    assert_eq!(foreign.body, unknown.body);
    let mut collision = own_submission;
    collision["operation_id"] = json!(Uuid::new_v4());
    let begun = dp.send("POST", BEGIN, Some(collision.clone())).await;
    assert_eq!(begun.status, 200, "{begun:?}");
    let refused = dp.send("POST", "/dp/v1/native-objects/bind", Some(json!({
        "api_key_id": other_key, "operation_id": collision["operation_id"], "raw_id": "resp-private-7c1",
    }))).await;
    assert_eq!(refused.status, 409, "{refused:?}");
    let still_private = lookup(&dp, key, "resp-private-7c1").await;
    assert_eq!(still_private.status, 200, "{still_private:?}");
    assert_eq!(lookup(&dp, other_key, "resp-private-7c1").await.status, 404);
}

// 恢复时若只带直接目标，只有路由父模型权限的调用方就无法继续访问原对象。
#[tokio::test]
async fn recovery_keeps_routing_parent_entitlement_and_requested_tier_after_restart() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (mut body, tenant, _, key) = submission(&cp).await;
    let direct = id_at(&body, "/origin/model_id");
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let parent = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": "native-parent-7c2", "routing": {"targets": [{"model_id": direct}]},
            }})),
        )
        .await;
    assert_eq!(parent.status, 201, "{parent:?}");
    let parent = id_at(&parent.body, "/id");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("the tenant version");
    let narrowed = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids": [parent]}),
        )
        .await;
    assert_eq!(narrowed.status, 200, "{narrowed:?}");
    let dp = cp.issue_dp_client("gw-native-parent-7c2").await;
    let denied_direct = dp.send("POST", BEGIN, Some(body.clone())).await;
    assert_eq!(
        denied_direct.status, 404,
        "a direct-only grant would mask this scenario: {denied_direct:?}"
    );
    body["operation_id"] = json!(Uuid::new_v4());
    body["requested_model_id"] = json!(parent);
    body["requested_model"] = json!("native-parent-7c2");
    body["requested_service_tier"] = json!("flex");
    let record = bind_submission(&dp, &body, "resp-parent-recovery-7c2").await;
    let allowed = lookup(&dp, key, "resp-parent-recovery-7c2").await;
    assert_eq!(allowed.status, 200, "{allowed:?}");
    let restarted = cp.restart().await;
    let worker = restarted
        .issue_dp_client("gw-native-parent-recovered-7c2")
        .await;
    let claimed = worker.send("POST", "/dp/v1/native-objects/claim", Some(json!({
        "limit": 1, "record_id": record["record_id"], "protocol": "openai", "kind": "response",
    }))).await;
    assert_eq!(claimed.status, 200, "{claimed:?}");
    let recovered = &claimed.body["records"][0];
    assert_eq!(recovered["origin"]["model_id"], json!(direct));
    assert_eq!(
        recovered["requested_model_id"],
        json!(parent),
        "the worker must retain the entitled parent"
    );
    assert_eq!(recovered["requested_model"], "native-parent-7c2");
    assert_eq!(recovered["requested_service_tier"], "flex");
    assert_eq!(allowed.body["record"]["requested_model_id"], json!(parent));
    assert_eq!(allowed.body["record"]["requested_service_tier"], "flex");
    let persisted: Option<String> = sqlx::query_scalar(
        "SELECT submission ->> 'requested_service_tier' FROM native_operations WHERE operation_id = $1",
    ).bind(id_at(&body, "/operation_id")).fetch_one(restarted.db().pool()).await.expect("the original requested tier");
    assert_eq!(persisted.as_deref(), Some("flex"));

    let completed = worker.send("POST", "/dp/v1/native-objects/complete", Some(json!({
        "lease": {"record_id": recovered["record_id"], "generation": recovered["lease_generation"]},
        "terminal_status": "completed", "usage": [{"entry_id": "", "occurred_at": chrono::Utc::now().to_rfc3339(),
            "status_code": 200, "prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5,
            "requested_service_tier": "priority"}],
    }))).await;
    assert_eq!(completed.status, 200, "{completed:?}");
    let event: Value =
        sqlx::query_scalar("SELECT event FROM native_terminal_outbox WHERE object_id = $1")
            .bind(id_at(recovered, "/record_id"))
            .fetch_one(restarted.db().pool())
            .await
            .expect("the terminal event snapshot");
    assert_eq!(event["requested_model_id"], json!(parent));
    assert_eq!(
        event["requested_service_tier"], "flex",
        "the observer must not replace the admitted request tier"
    );
}

// 过滤缺失会让 Responses 工作者抢走同环境的原生批处理租约。
#[tokio::test]
async fn recovery_response_claim_filters_leave_other_protocols_and_kinds_unleased() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, _, _, _) = submission(&cp).await;
    let dp = cp.issue_dp_client("gw-native-filter-responses-7c2").await;
    let response = bind_submission(&dp, &body, "resp-filter-7c2").await;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = cp.send("POST", "/admin/resources/provider_keys", Some(admin.cookie()), Some(json!({"doc": {
        "display_name": "native-anthropic-account-7c2", "provider": "anthropic",
        "api_key": "sk-ant-native-registry-fixture-7c2", "api_base": "https://native-batch.example", "groups": ["shared"],
    }}))).await;
    assert_eq!(account.status, 201, "{account:?}");
    let account = id_at(&account.body, "/id");
    let model = cp.send("POST", "/admin/resources/models", Some(admin.cookie()), Some(json!({"doc": {
        "display_name": "native-batch-model-7c2", "provider": "anthropic", "model_name": "claude-sonnet-4-5",
        "provider_key_id": account,
    }}))).await;
    assert_eq!(model.status, 201, "{model:?}");
    let model = id_at(&model.body, "/id");
    let internal: (Uuid, i64) = sqlx::query_as("SELECT id, version FROM tenants WHERE is_internal")
        .fetch_one(cp.db().pool())
        .await
        .expect("the platform's internal tenant");
    let grant = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{}", internal.0),
            admin.cookie(),
            internal.1,
            json!({"allowed_model_ids": [model]}),
        )
        .await;
    assert_eq!(grant.status, 200, "{grant:?}");
    let internal_key = seed_key(&cp, &admin, internal.0, None).await;
    let key = aisix_core::native_objects::platform_id_key_from_secret(
        &cp.state().pki.platform_id_key_hex(),
    );
    let account_text = account.to_string();
    let mut input = b"aisix-native-origin-v1\0".to_vec();
    for field in [
        account_text.as_str(),
        "anthropic",
        "anthropic",
        "https://native-batch.example",
        "sk-ant-native-registry-fixture-7c2",
    ] {
        input.extend_from_slice(&(field.len() as u64).to_be_bytes());
        input.extend_from_slice(field.as_bytes());
    }
    input.extend_from_slice(&0_u64.to_be_bytes());
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), &input);
    let batch = bind_submission(&dp, &json!({
        "operation_id": Uuid::new_v4(), "api_key_id": internal_key, "protocol": "anthropic", "kind": "message_batch",
        "origin": {"model_id": model, "provider_key_id": account, "boundary": format!("v1:{}", hex::encode(tag.as_ref()))},
        "requested_model_id": model, "requested_model": "native-batch-model-7c2", "upstream_model": "claude-sonnet-4-5",
        "operation": "messages", "request_id": "req-native-batch-filter-7c2", "asynchronous": true,
        "entries": [{"entry_id": "entry-filter-7c2", "requested_model_id": model, "model_id": model, "upstream_model": "claude-sonnet-4-5"}],
    }), "msgbatch-filter-7c2").await;
    let claims = dp
        .send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({
                "limit": 50, "protocol": "openai", "kind": "response",
            })),
        )
        .await;
    assert_eq!(claims.status, 200, "{claims:?}");
    let records = claims.body["records"]
        .as_array()
        .expect("an explicit claim list");
    assert_eq!(
        records.len(),
        1,
        "the Responses worker must not claim a native MessageBatch: {claims:?}"
    );
    assert_eq!(records[0]["record_id"], response["record_id"]);
    let batch_worker = cp.issue_dp_client("gw-native-filter-batches-7c2").await;
    let batch_claim = batch_worker
        .send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({
                "limit": 50, "protocol": "anthropic", "kind": "message_batch",
            })),
        )
        .await;
    assert_eq!(batch_claim.status, 200, "{batch_claim:?}");
    let records = batch_claim.body["records"]
        .as_array()
        .expect("an explicit batch claim list");
    assert_eq!(records.len(), 1, "{batch_claim:?}");
    assert_eq!(records[0]["record_id"], batch["record_id"]);
}

// Authorization 之外的凭据默认头仍会发送，改变它就可能切换账号。
#[tokio::test]
async fn recovery_origin_changes_when_an_additional_credential_header_is_configured() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, _, _, key) = submission(&cp).await;
    let dp = cp.issue_dp_client("gw-native-extra-credential-7c2").await;
    bind_submission(&dp, &body, "resp-extra-credential-7c2").await;
    let before = lookup(&dp, key, "resp-extra-credential-7c2").await;
    assert_eq!(before.status, 200, "{before:?}");
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let path = format!(
        "/admin/resources/provider_keys/{}",
        id_at(&body, "/origin/provider_key_id")
    );
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let mut doc = current.body["doc"].clone();
    doc["request"] = json!({"default_headers": {"x-api-key": "sk-second-account-fixture-7c2"}});
    let changed = cp
        .send_conditional(
            "PUT",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().expect("a version"),
            json!({"doc": doc}),
        )
        .await;
    assert_eq!(changed.status, 200, "{changed:?}");
    let after = lookup(&dp, key, "resp-extra-credential-7c2").await;
    assert_eq!(
        after.status, 409,
        "an effective extra credential must change account identity: {after:?}"
    );
    assert_eq!(after.body["code"], "native_origin_changed");
}

// 完整逐项归属与真实 outbox→计价→账本链路，恢复和重复完成只能记一次。
#[tokio::test]
async fn native_batch_entries_keep_models_tiers_terminal_states_and_single_ledger_posting() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (mut body, tenant, _, key) = submission(&cp).await;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = cp.send("POST", "/admin/resources/provider_keys", Some(admin.cookie()), Some(json!({"doc": {
        "display_name":"batch-account-v6", "provider":"anthropic", "api_key":"sk-native-batch-v6",
        "api_base":"https://native-batch-v6.example", "groups":["shared"]
    }}))).await;
    assert_eq!(account.status, 201, "{account:?}");
    let account_id = id_at(&account.body, "/id");
    let mut models = Vec::new();
    for (name, upstream) in [
        ("batch-one-v6", "claude-one-v6"),
        ("batch-two-v6", "claude-two-v6"),
    ] {
        let model = cp.send("POST", "/admin/resources/models", Some(admin.cookie()), Some(json!({"doc": {
            "display_name":name, "provider":"anthropic", "model_name":upstream, "provider_key_id":account_id
        }}))).await;
        assert_eq!(model.status, 201, "{model:?}");
        models.push(id_at(&model.body, "/id"));
    }
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .unwrap();
    let granted = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids":models}),
        )
        .await;
    assert_eq!(granted.status, 200, "{granted:?}");
    for (model, input, output, cache) in
        [(models[0], "2", "4", "0.4"), (models[1], "3", "6", "0.6")]
    {
        support::world::insert_baseline_backdated(
            cp.db(),
            model,
            json!({
                "default":{"base":{"input":"1","output":"2","cache_read":"0.1"}},
                "batch":{"base":{"input":input,"output":output,"cache_read":cache}}
            }),
            "2000-01-01T00:00:00Z".parse().unwrap(),
        )
        .await;
    }
    let id_key = aisix_core::native_objects::platform_id_key_from_secret(
        &cp.state().pki.platform_id_key_hex(),
    );
    let account_text = account_id.to_string();
    let mut digest_input = b"aisix-native-origin-v1\0".to_vec();
    for field in [
        account_text.as_str(),
        "anthropic",
        "anthropic",
        "https://native-batch-v6.example",
        "sk-native-batch-v6",
    ] {
        digest_input.extend_from_slice(&(field.len() as u64).to_be_bytes());
        digest_input.extend_from_slice(field.as_bytes());
    }
    digest_input.extend_from_slice(&0u64.to_be_bytes());
    let digest = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &id_key), &digest_input);
    body["protocol"] = json!("anthropic");
    body["kind"] = json!("message_batch");
    body["origin"] = json!({"model_id":models[0],"provider_key_id":account_id,"boundary":format!("v1:{}",hex::encode(digest.as_ref()))});
    body["requested_model_id"] = json!(models[0]);
    body["requested_model"] = json!("batch-one-v6");
    body["upstream_model"] = json!("claude-one-v6");
    body["operation"] = json!("batch_completion");
    body["requested_service_tier"] = json!("flex");
    body["observation_headers"] =
        json!({"anthropic-version":"2026-10-21","anthropic-beta":"batch-test-v6"});
    body["entries"] = json!([
        {"entry_id":"one","requested_model_id":models[0],"model_id":models[0],"upstream_model":"claude-one-v6","requested_service_tier":"fast"},
        {"entry_id":"two","requested_model_id":models[1],"model_id":models[1],"upstream_model":"claude-two-v6","requested_service_tier":"standard"},
        {"entry_id":"cancel","requested_model_id":models[0],"model_id":models[0],"upstream_model":"claude-one-v6"},
        {"entry_id":"failed","requested_model_id":models[1],"model_id":models[1],"upstream_model":"claude-two-v6"}
    ]);
    let dp = cp.issue_dp_client("gw-native-batch-v6").await;
    let bound = bind_submission(&dp, &body, "msgbatch-native-v6").await;
    let record = id_at(&bound, "/record_id");
    assert_eq!(bound["entries"][0]["requested_model"], "batch-one-v6");
    assert_eq!(bound["entries"][1]["requested_model"], "batch-two-v6");
    assert_eq!(bound["observation_headers"], body["observation_headers"]);
    assert_eq!(bound["completed"], false);
    let premature = dp
        .send(
            "POST",
            "/dp/v1/native-objects/tombstone",
            Some(json!({
                "api_key_id":key,"record_id":record,"version":bound["version"]
            })),
        )
        .await;
    assert_eq!(
        premature.status, 409,
        "pending accounting must survive deletion: {premature:?}"
    );
    let restarted = cp.restart().await;
    let worker = restarted
        .issue_dp_client("gw-native-batch-recovery-v6")
        .await;
    let claimed = worker
        .send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({
                "limit":1,"record_id":record,"protocol":"anthropic","kind":"message_batch"
            })),
        )
        .await;
    assert_eq!(claimed.status, 200, "{claimed:?}");
    assert_eq!(claimed.body["records"][0]["entries"], bound["entries"]);
    assert_eq!(
        claimed.body["records"][0]["observation_headers"],
        body["observation_headers"]
    );
    let lease =
        json!({"record_id":record,"generation":claimed.body["records"][0]["lease_generation"]});
    let observed = chrono::Utc::now().to_rfc3339();
    let usage = |id: &str, status: &str, http: u16, input: u32, output: u32, read: u32| {
        json!({
            "entry_id":id,"terminal_status":status,"occurred_at":observed,"status_code":http,
            "prompt_tokens":input,"completion_tokens":output,"cache_read_tokens":read,
            "cache_usage_reported":true,"service_tier":"batch","requested_service_tier":"forged-tier"
        })
    };
    let completion = json!({"lease":lease,"terminal_status":"ended","usage":[
        usage("two","completed",200,50,10,0), usage("one","completed",200,100,10,20),
        usage("cancel","cancelled",499,0,0,0), usage("failed","failed",400,0,0,0)
    ]});
    let completed = worker
        .send(
            "POST",
            "/dp/v1/native-objects/complete",
            Some(completion.clone()),
        )
        .await;
    assert_eq!(completed.status, 200, "{completed:?}");
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            let delivered:i64 = sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox WHERE object_id=$1 AND delivered_at IS NOT NULL")
                .bind(record).fetch_one(restarted.db().pool()).await.unwrap();
            if delivered==4 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("all entries pass the real pricing intake");
    let rows:Vec<(String,Value,Option<i64>)> = sqlx::query_as("SELECT o.entry_id,o.event,c.charge_micro_cny FROM native_terminal_outbox o JOIN pricing_charges_cny c ON c.event_id=o.event_id::text WHERE o.object_id=$1 ORDER BY o.entry_id")
        .bind(record).fetch_all(restarted.db().pool()).await.unwrap();
    assert_eq!(rows.len(), 4);
    for (entry, event, charge) in &rows {
        assert_eq!(event["team_id"], json!(tenant));
        assert_eq!(event["api_key_id"], json!(key));
        assert_eq!(event["is_batch"], true);
        assert_eq!(event["inference_stage"], "terminal");
        match entry.as_str() {
            "one" => {
                assert_eq!(event["requested_model_id"], json!(models[0]));
                assert_eq!(event["requested_model"], "batch-one-v6");
                assert_eq!(event["requested_service_tier"], "fast");
                assert_eq!(*charge, Some(248));
            }
            "two" => {
                assert_eq!(event["requested_model_id"], json!(models[1]));
                assert_eq!(event["requested_model"], "batch-two-v6");
                assert_eq!(event["requested_service_tier"], "standard");
                assert_eq!(*charge, Some(210));
            }
            "cancel" => {
                assert_eq!(event["native_terminal_status"], "cancelled");
                assert!(event.get("requested_service_tier").is_some());
                assert_eq!(event["requested_service_tier"], Value::Null);
                assert_eq!(*charge, Some(0));
            }
            "failed" => {
                assert_eq!(event["native_terminal_status"], "failed");
                assert_eq!(*charge, Some(0));
            }
            other => panic!("unexpected entry {other}"),
        }
    }
    for _ in 0..2 {
        let replay = worker
            .send(
                "POST",
                "/dp/v1/native-objects/complete",
                Some(completion.clone()),
            )
            .await;
        assert_eq!(replay.status, 200, "{replay:?}");
    }
    let counts:(i64,i64,i64) = sqlx::query_as("SELECT count(*),count(l.event_id),COALESCE(sum(-l.amount_micro_cny),0)::bigint FROM native_terminal_outbox o LEFT JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(restarted.db().pool()).await.unwrap();
    assert_eq!(
        counts,
        (4, 2, 458),
        "one permanent terminal per custom_id and one posting per charged entry"
    );
    let path = format!("/admin/tenants/{tenant}");
    let current = restarted
        .send("GET", &path, Some(admin.cookie()), None)
        .await;
    assert_eq!(current.status, 200, "{current:?}");
    let narrowed = restarted
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().unwrap(),
            json!({"allowed_model_ids":[models[0]]}),
        )
        .await;
    assert_eq!(narrowed.status, 200, "{narrowed:?}");
    let denied = worker
        .send(
            "POST",
            "/dp/v1/native-objects/lookup",
            Some(json!({"api_key_id":key,"locator":{
                "protocol":"anthropic","kind":"message_batch","raw_id":"msgbatch-native-v6"
            }})),
        )
        .await;
    assert_eq!(
        denied.status, 404,
        "removing a non-primary model grant closes the complete batch: {denied:?}"
    );
}

// 受理之后撤销准入不能抹掉账号观察和账本；后续客户访问仍按当前权限拒绝。
#[tokio::test]
async fn accepted_native_work_binds_and_posts_once_after_model_deletion_and_key_revocation() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (mut body, _, _, key) = submission(&cp).await;
    let input_text = json!({"utf8_bytes":8,"unicode_scalars":3,"complete":false});
    body["upstream_input_text"] = input_text.clone();
    let model = id_at(&body, "/origin/model_id");
    support::world::insert_baseline_backdated(
        cp.db(),
        model,
        json!({"default":{"base":{"input":"2","output":"4"}}}),
        "2000-01-01T00:00:00Z".parse().unwrap(),
    )
    .await;
    let dp = cp.issue_dp_client("gw-postaccept-original-v6").await;
    let begun = dp.send("POST", BEGIN, Some(body.clone())).await;
    assert_eq!(begun.status, 200, "{begun:?}");
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let path = format!("/admin/resources/models/{model}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let deleted = cp
        .send_conditional(
            "DELETE",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().unwrap(),
            Value::Null,
        )
        .await;
    assert_eq!(deleted.status, 200, "{deleted:?}");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1")
        .bind(key)
        .fetch_one(cp.db().pool())
        .await
        .unwrap();
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
    let binding =
        json!({"operation_id":body["operation_id"],"api_key_id":key,"raw_id":"resp_postaccept_v6"});
    let unrelated = cp.issue_dp_client("gw-postaccept-unrelated-v6").await;
    let wrong = unrelated
        .send("POST", "/dp/v1/native-objects/bind", Some(binding.clone()))
        .await;
    assert_eq!(
        wrong.status, 404,
        "only the submitting DP can fix its accepted identity: {wrong:?}"
    );
    let bound = dp
        .send("POST", "/dp/v1/native-objects/bind", Some(binding))
        .await;
    assert_eq!(
        bound.status, 200,
        "post-accept identity survives admission withdrawal: {bound:?}"
    );
    let record = id_at(&bound.body, "/record/record_id");
    let denied = lookup(&dp, key, "resp_postaccept_v6").await;
    assert_eq!(
        denied.status, 404,
        "revoked clients cannot retrieve accepted work: {denied:?}"
    );
    let restarted = cp.restart().await;
    let recovery = restarted.issue_dp_client("gw-postaccept-recovery-v6").await;
    let claimed = recovery
        .send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({"limit":1,"record_id":record})),
        )
        .await;
    assert_eq!(claimed.status, 200, "{claimed:?}");
    assert_eq!(
        claimed.body["records"].as_array().unwrap().len(),
        1,
        "the persisted account survives model deletion"
    );
    assert_eq!(
        claimed.body["records"][0]["upstream_input_text"], input_text,
        "the original scalar survives restart, deleted model and revoked key"
    );
    let audit = json!({"version":2,"boundary":"upstream_attempt","response_complete":true,
        "exchange_count":1,"input_text":input_text,
        "output_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":true}});
    let occurred_at = "2026-11-01T01:02:03Z";
    let completion = json!({"lease":{"record_id":record,"generation":claimed.body["records"][0]["lease_generation"]},
        "terminal_status":"completed","usage":[{"entry_id":"","occurred_at":occurred_at,
        "status_code":200,"prompt_tokens":40,"completion_tokens":10,"total_tokens":50,"io_audit":audit}]});
    let mut invalid_io = completion.clone();
    invalid_io["usage"][0]["io_audit"]["output_text"]["utf8_bytes"] = json!(1);
    let rejected = recovery
        .send("POST", "/dp/v1/native-objects/complete", Some(invalid_io))
        .await;
    assert_eq!(
        rejected.status, 422,
        "invalid scalar cannot settle accepted work: {rejected:?}"
    );
    for _ in 0..2 {
        let done = recovery
            .send(
                "POST",
                "/dp/v1/native-objects/complete",
                Some(completion.clone()),
            )
            .await;
        assert_eq!(done.status, 200, "{done:?}");
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let delivered: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_terminal_outbox WHERE object_id=$1 AND delivered_at IS NOT NULL)")
                .bind(record).fetch_one(restarted.db().pool()).await.unwrap();
            if delivered { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("accepted work is still delivered to the real billing intake");
    let (count, charge, event): (i64, i64, Value) = sqlx::query_as(
        "SELECT count(*) OVER (),-l.amount_micro_cny,o.event FROM native_terminal_outbox o JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(restarted.db().pool()).await.unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        event["io_audit"], audit,
        "the existing single terminal outbox carries model IO"
    );
    let stored: Value =
        sqlx::query_scalar("SELECT event->'io_audit' FROM usage_events WHERE event_id=$1")
            .bind(event["event_id"].as_str().unwrap())
            .fetch_one(restarted.db().pool())
            .await
            .unwrap();
    assert_eq!(
        stored, audit,
        "the real usage intake keeps the audit without changing pricing"
    );
    assert_eq!(charge, 120);
    assert_eq!(event["requested_model_id"], json!(model));
    assert_eq!(event["api_key_id"], json!(key));
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(event["occurred_at"].as_str().unwrap()).unwrap(),
        chrono::DateTime::parse_from_rfc3339(occurred_at).unwrap()
    );
}

// 上传的批次清单也持久保存逐项身份，其他对象不能冒用这份清单。
#[tokio::test]
async fn openai_batch_input_files_keep_entry_identity_across_restart() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (mut body, _, _, key) = submission(&cp).await;
    body["kind"] = json!("file");
    body["asynchronous"] = json!(false);
    body["entries"] = json!([{"entry_id":"client-one","endpoint":"/v1/responses",
        "requested_model_id":body["requested_model_id"],"model_id":body["origin"]["model_id"],
        "upstream_model":body["upstream_model"],"requested_service_tier":"flex",
        "upstream_input_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":false}}]);
    let dp = cp.issue_dp_client("gw-openai-input-v7").await;
    let bound = bind_submission(&dp, &body, "file_openai_input_v7").await;
    assert_eq!(bound["entries"][0]["endpoint"], "/v1/responses");
    assert_eq!(bound["entries"][0]["requested_service_tier"], "flex");
    assert_eq!(
        bound["entries"][0]["upstream_input_text"],
        body["entries"][0]["upstream_input_text"]
    );
    let restarted = cp.restart().await;
    let dp2 = restarted
        .issue_dp_client("gw-openai-input-restart-v7")
        .await;
    let found = dp2
        .send(
            "POST",
            "/dp/v1/native-objects/lookup",
            Some(json!({"api_key_id":key,
        "locator":{"protocol":"openai","kind":"file","raw_id":"file_openai_input_v7"}})),
        )
        .await;
    assert_eq!(found.status, 200, "{found:?}");
    assert_eq!(found.body["record"]["entries"], bound["entries"]);
    let admin = restarted.admin_login_seeded(OperatorRole::Operator).await;
    let model_path = format!(
        "/admin/resources/models/{}",
        body["origin"]["model_id"].as_str().unwrap()
    );
    let current = restarted
        .send("GET", &model_path, Some(admin.cookie()), None)
        .await;
    assert_eq!(current.status, 200, "{current:?}");
    let mut doc = current.body["doc"].clone();
    doc["display_name"] = json!("native-response-renamed-v7");
    let renamed = restarted
        .send_conditional(
            "PUT",
            &model_path,
            admin.cookie(),
            current.body["version"].as_i64().unwrap(),
            json!({"doc":doc}),
        )
        .await;
    assert_eq!(renamed.status, 200, "{renamed:?}");
    let mut batch = body.clone();
    batch["operation_id"] = json!(Uuid::new_v4());
    batch["kind"] = json!("message_batch");
    batch["asynchronous"] = json!(true);
    batch["input_file_id"] = json!("file_openai_input_v7");
    batch["entries"] = bound["entries"].clone();
    let batch = bind_submission(&dp2, &batch, "batch_after_input_model_rename_v7").await;
    assert_eq!(
        batch["entries"][0]["requested_model"], "native-response-7c1",
        "model renames cannot rewrite the input's requested identity"
    );

    body["operation_id"] = json!(Uuid::new_v4());
    body["kind"] = json!("response");
    let wrong = dp2.send("POST", BEGIN, Some(body)).await;
    assert_eq!(
        wrong.status, 422,
        "only a batch input file or batch carries entries: {wrong:?}"
    );
}

// 终态归属和账单一起提交，撤销客户端不会丢掉已付费结果；重放不重复创建。
#[tokio::test]
async fn openai_batch_completion_atomically_binds_result_files_and_posts_once_after_revoke() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (mut body, tenant, _, key) = submission(&cp).await;
    let model = id_at(&body, "/origin/model_id");
    support::world::insert_baseline_backdated(
        cp.db(),
        model,
        json!({"default":{"base":{"input":"4","output":"8","cache_read":"1"}},"batch":{"base":{"input":"2","output":"4","cache_read":"0.5"}}}),
        "2000-01-01T00:00:00Z".parse().unwrap(),
    )
    .await;
    body["kind"] = json!("message_batch");
    body["asynchronous"] = json!(true);
    body["entries"] = json!([{"entry_id":"entry-a","endpoint":"/v1/chat/completions",
        "requested_model_id":model,"model_id":model,"upstream_model":body["upstream_model"],
        "requested_service_tier":"flex"}]);
    let dp = cp.issue_dp_client("gw-openai-batch-result-v7").await;
    let batch = bind_submission(&dp, &body, "batch_openai_result_v7").await;
    let record = id_at(&batch, "/record_id");
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1")
        .bind(key)
        .fetch_one(cp.db().pool())
        .await
        .unwrap();
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
    let restarted = cp.restart().await;
    let recovery = restarted
        .issue_dp_client("gw-openai-batch-result-recovery-v7")
        .await;
    let claimed = recovery
        .send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({
        "limit":1,"record_id":record,"protocol":"openai","kind":"message_batch"})),
        )
        .await;
    assert_eq!(claimed.status, 200, "{claimed:?}");
    assert_eq!(claimed.body["records"].as_array().unwrap().len(), 1);
    let completion = json!({"lease":{"record_id":record,"generation":claimed.body["records"][0]["lease_generation"]},
        "terminal_status":"completed","output_file_ids":["file_output_v7","file_errors_v7"],"usage":[{
        "entry_id":"entry-a","occurred_at":chrono::Utc::now().to_rfc3339(),"status_code":200,
        "prompt_tokens":100,"completion_tokens":10,"cached_prompt_tokens":20,"cache_usage_reported":true,
        "total_tokens":110,"service_tier":"default","requested_service_tier":"forged"}]});
    for _ in 0..2 {
        let done = recovery
            .send(
                "POST",
                "/dp/v1/native-objects/complete",
                Some(completion.clone()),
            )
            .await;
        assert_eq!(done.status, 200, "{done:?}");
    }
    let files:Vec<(String,Uuid,Uuid,Uuid,Value)> = sqlx::query_as("SELECT n.raw_id,n.parent_id,n.tenant_id,n.api_key_id,o.submission FROM native_objects n JOIN native_operations o ON o.environment_id=n.environment_id AND o.operation_id=n.operation_id WHERE n.parent_id=$1 ORDER BY n.raw_id")
        .bind(record).fetch_all(restarted.db().pool()).await.unwrap();
    assert_eq!(files.len(), 2, "each output identity binds once");
    for (_, parent, owner, payer, submitted) in files {
        assert_eq!((parent, owner, payer), (record, tenant, key));
        assert_eq!(
            submitted["entries"], batch["entries"],
            "result files retain every model entitlement"
        );
        assert_eq!(submitted["origin"], body["origin"]);
        assert_eq!(
            submitted["operation"], "batch_result",
            "only completion mints the trusted result-file operation"
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let delivered:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_terminal_outbox WHERE object_id=$1 AND delivered_at IS NOT NULL)")
                .bind(record).fetch_one(restarted.db().pool()).await.unwrap();
            if delivered { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("revoked submitter's completed batch reaches the real billing intake");
    let posted:(i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(-l.amount_micro_cny),0)::bigint FROM native_terminal_outbox o JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(restarted.db().pool()).await.unwrap();
    assert_eq!(
        posted,
        (1, 210),
        "one terminal and one batch-price debit independent of caller request id"
    );
    let denied = recovery
        .send(
            "POST",
            "/dp/v1/native-objects/lookup",
            Some(json!({"api_key_id":key,
        "locator":{"protocol":"openai","kind":"file","raw_id":"file_output_v7"}})),
        )
        .await;
    assert_eq!(
        denied.status, 404,
        "revoked caller cannot read its paid result: {denied:?}"
    );
}

// 裸 ID 的子对象归属跨网关和控制面重启保留；刷新只能扩展已授权的原记录。
#[tokio::test]
async fn raw_response_child_graph_survives_restart_and_rejects_foreign_refresh() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, tenant, _, key) = submission(&cp).await;
    let dp = cp.issue_dp_client("raw-response-creator-v7").await;
    let record = bind_submission(&dp, &body, "resp_raw_exact_v7").await;
    let record_id = id_at(&record, "/record_id");
    let operation_id = id_at(&body, "/operation_id");
    let second = cp.restart().await;
    let dp2 = second.issue_dp_client("raw-response-reader-v7").await;
    let refresh = json!({
        "existing_record_id": record_id, "operation_id": operation_id,
        "api_key_id": key, "raw_id":"resp_raw_exact_v7",
        "objects":[
            {"kind":"response_item","raw_id":"msg_raw_exact_v7"},
            {"kind":"response_container","raw_id":"cntr_raw_exact_v7"}
        ]
    });
    let legacy_creator_bind = dp2
        .send(
            "POST",
            "/dp/v1/native-objects/bind",
            Some(json!({
                "operation_id":operation_id,"api_key_id":key,"raw_id":"resp_raw_exact_v7"
            })),
        )
        .await;
    assert_eq!(
        legacy_creator_bind.status, 404,
        "the original creator gate still applies"
    );
    let (one, two) = tokio::join!(
        dp2.send("POST", "/dp/v1/native-objects/bind", Some(refresh.clone())),
        dp2.send("POST", "/dp/v1/native-objects/bind", Some(refresh.clone())),
    );
    for bound in [one, two] {
        assert_eq!(bound.status, 200, "{bound:?}");
        assert_eq!(bound.body["record"]["record_id"], json!(record_id));
        let children = bound.body["records"]
            .as_array()
            .expect("child binding acknowledgments");
        assert_eq!(children.len(), 2);
        for child in children {
            assert_eq!(child["parent_id"], json!(record_id));
            assert_eq!(child["origin"], body["origin"]);
        }
    }
    let durable: Vec<(String, String, Uuid, Uuid)> = sqlx::query_as(
        "SELECT kind,raw_id,tenant_id,api_key_id FROM native_objects WHERE parent_id=$1 ORDER BY kind",
    ).bind(record_id).fetch_all(second.db().pool()).await.expect("durable child graph");
    assert_eq!(
        durable,
        vec![
            (
                "response_container".into(),
                "cntr_raw_exact_v7".into(),
                tenant,
                key
            ),
            (
                "response_item".into(),
                "msg_raw_exact_v7".into(),
                tenant,
                key
            ),
        ]
    );
    let admin = second.admin_login_seeded(OperatorRole::Operator).await;
    let foreign_tenant = seed_tenant(&second, &admin).await;
    let foreign_key = seed_key(&second, &admin, foreign_tenant, None).await;
    let mut foreign = refresh.clone();
    foreign["api_key_id"] = json!(foreign_key);
    let mut unknown = foreign.clone();
    unknown["existing_record_id"] = json!(Uuid::new_v4());
    let foreign_result = dp2
        .send("POST", "/dp/v1/native-objects/bind", Some(foreign))
        .await;
    let unknown_result = dp2
        .send("POST", "/dp/v1/native-objects/bind", Some(unknown))
        .await;
    assert_eq!(foreign_result.status, 404, "{foreign_result:?}");
    assert_eq!(
        unknown_result.status, foreign_result.status,
        "{unknown_result:?}"
    );
    assert_eq!(unknown_result.body, foreign_result.body);
    let mut reassigned = refresh.clone();
    reassigned["raw_id"] = json!("resp_another");
    let refused = dp2
        .send("POST", "/dp/v1/native-objects/bind", Some(reassigned))
        .await;
    assert_eq!(refused.status, 404, "{refused:?}");
    let third = second.restart().await;
    let dp3 = third.issue_dp_client("raw-response-postrestart-v7").await;
    for (kind, raw) in [
        ("response_item", "msg_raw_exact_v7"),
        ("response_container", "cntr_raw_exact_v7"),
    ] {
        let lookup = |api_key: Value| json!({"api_key_id":api_key,"locator":{"protocol":"openai","kind":kind,"raw_id":raw}});
        let own = dp3
            .send(
                "POST",
                "/dp/v1/native-objects/lookup",
                Some(lookup(json!(key))),
            )
            .await;
        assert_eq!(own.status, 200, "{own:?}");
        assert_eq!(own.body["record"]["raw_id"], raw);
        assert_eq!(own.body["record"]["parent_id"], json!(record_id));
        let foreign = dp3
            .send(
                "POST",
                "/dp/v1/native-objects/lookup",
                Some(lookup(json!(foreign_key))),
            )
            .await;
        assert_eq!(foreign.status, 404, "{foreign:?}");
    }
    let generation: i64 = sqlx::query_scalar("SELECT count(*) FROM native_operations WHERE environment_id=$1 AND submission->>'operation'='responses'")
        .bind(third.env().env_id).fetch_one(third.db().pool()).await.expect("no new inference submission");
    assert_eq!(generation, 1);
    let outbox: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox WHERE object_id=$1")
            .bind(record_id)
            .fetch_one(third.db().pool())
            .await
            .expect("refresh creates no billing");
    assert_eq!(outbox, 0);
}

// 相异 Response 并发复用多个容器，不能回滚父对象或创建无归属的多余操作。
#[tokio::test]
async fn distinct_response_operations_share_child_objects_concurrently_without_reparenting() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (mut body, _, _, key) = submission(&cp).await;
    let dp = cp.issue_dp_client("raw-response-shared-children-v7").await;
    let first = bind_submission(&dp, &body, "resp_shared_first_v7").await;
    body["operation_id"] = json!(Uuid::new_v4());
    let second = bind_submission(&dp, &body, "resp_shared_second_v7").await;
    let objects = json!([
        {"kind":"response_container","raw_id":"cntr_shared_a_v7"},
        {"kind":"response_container","raw_id":"cntr_shared_b_v7"}
    ]);
    let request = |record: &Value, objects: Value| {
        json!({
            "existing_record_id":record["record_id"],"operation_id":record["operation_id"],
            "api_key_id":key,"raw_id":record["raw_id"],"objects":objects
        })
    };
    let mut reverse = objects.as_array().unwrap().clone();
    reverse.reverse();
    let first_bind = request(&first, objects);
    let second_bind = request(&second, json!(reverse));
    let (one, two) = tokio::join!(
        dp.send("POST", "/dp/v1/native-objects/bind", Some(first_bind)),
        dp.send("POST", "/dp/v1/native-objects/bind", Some(second_bind)),
    );
    assert_eq!(one.status, 200, "{one:?}");
    assert_eq!(two.status, 200, "{two:?}");
    let shared: Vec<(String,Uuid)> = sqlx::query_as("SELECT raw_id,parent_id FROM native_objects WHERE environment_id=$1 AND kind='response_container' ORDER BY raw_id")
        .bind(cp.env().env_id).fetch_all(cp.db().pool()).await.expect("shared containers");
    assert_eq!(shared.len(), 2);
    let parents = [id_at(&first, "/record_id"), id_at(&second, "/record_id")];
    assert!(shared.iter().all(|(_, parent)| parents.contains(parent)));
    assert_eq!(shared[0].0, "cntr_shared_a_v7");
    assert_eq!(shared[1].0, "cntr_shared_b_v7");
    let operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_operations WHERE environment_id=$1")
            .bind(cp.env().env_id)
            .fetch_one(cp.db().pool())
            .await
            .expect("no orphan child operations");
    assert_eq!(operations, 4);
    for response in [&one, &two] {
        let children = response.body["records"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        for child in children {
            let raw = child["raw_id"].as_str().unwrap();
            let original_parent = shared.iter().find(|(id, _)| id == raw).unwrap().1;
            assert_eq!(
                child["parent_id"],
                json!(original_parent),
                "shared identity is never reparented"
            );
        }
    }
}

// 结果文件包含整批内容，撤销非首项的模型权限同样关闭其读入口。
#[tokio::test]
async fn openai_batch_result_files_require_every_current_entry_model_grant() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (mut body, tenant, _, key) = submission(&cp).await;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let second=cp.send("POST","/admin/resources/models",Some(admin.cookie()),Some(json!({"doc":{
        "display_name":"openai-batch-second-v7","provider":"openai","model_name":"second-upstream-model",
        "provider_key_id":body["origin"]["provider_key_id"]}}))).await;
    assert_eq!(second.status, 201, "{second:?}");
    let second = id_at(&second.body, "/id");
    let first = id_at(&body, "/origin/model_id");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .unwrap();
    let granted = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids":[first,second]}),
        )
        .await;
    assert_eq!(granted.status, 200, "{granted:?}");
    body["kind"] = json!("message_batch");
    body["entries"] = json!([
        {"entry_id":"one","endpoint":"/v1/chat/completions","requested_model_id":first,"model_id":first,"upstream_model":body["upstream_model"]},
        {"entry_id":"two","endpoint":"/v1/chat/completions","requested_model_id":second,"model_id":second,"upstream_model":"second-upstream-model"}
    ]);
    let dp = cp.issue_dp_client("gw-openai-result-grants-v7").await;
    let batch = bind_submission(&dp, &body, "batch_results_grants_v7").await;
    let record = id_at(&batch, "/record_id");
    let claimed = dp
        .send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({"limit":1,"record_id":record})),
        )
        .await;
    assert_eq!(claimed.status, 200, "{claimed:?}");
    let usage = |id| json!({"entry_id":id,"occurred_at":chrono::Utc::now().to_rfc3339(),"status_code":400,"prompt_tokens":0,"completion_tokens":0});
    let completed=dp.send("POST","/dp/v1/native-objects/complete",Some(json!({"lease":{"record_id":record,"generation":claimed.body["records"][0]["lease_generation"]},
        "terminal_status":"completed","output_file_ids":["file_results_grants_v7"],"usage":[usage("one"),usage("two")]}))).await;
    assert_eq!(completed.status, 200, "{completed:?}");
    let mut child = body.clone();
    child["operation_id"] = json!(Uuid::new_v4());
    child["kind"] = json!("file");
    child["entries"] = json!([]);
    child["asynchronous"] = json!(false);
    child["parent_id"] = json!(record);
    bind_submission(&dp, &child, "file_context_parent_v7").await;
    let contextual = json!({"api_key_id":key,"consumer_family":"batches","locator":{"protocol":"openai","kind":"file","raw_id":"file_context_parent_v7"}});
    let allowed = dp
        .send(
            "POST",
            "/dp/v1/native-objects/lookup",
            Some(contextual.clone()),
        )
        .await;
    assert_eq!(
        allowed.status, 200,
        "valid File consumer retains parent ownership: {allowed:?}"
    );
    for family in ["files", "models", "messages", "unknown_consumer"] {
        let mut invalid = contextual.clone();
        invalid["consumer_family"] = json!(family);
        let denied = dp
            .send("POST", "/dp/v1/native-objects/lookup", Some(invalid))
            .await;
        assert_eq!(
            denied.status, 422,
            "unsupported consumer never becomes an arbitrary permission bypass: {denied:?}"
        );
    }
    let mut invalid = contextual.clone();
    invalid["locator"]["kind"] = json!("response");
    let denied = dp
        .send("POST", "/dp/v1/native-objects/lookup", Some(invalid))
        .await;
    assert_eq!(
        denied.status, 422,
        "only File locators carry consumer context: {denied:?}"
    );
    let other_tenant = seed_tenant(&cp, &admin).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
        .bind(other_tenant)
        .fetch_one(cp.db().pool())
        .await
        .unwrap();
    let granted = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{other_tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids":[first,second]}),
        )
        .await;
    assert_eq!(granted.status, 200, "{granted:?}");
    let member = cp
        .seed_member(other_tenant, aisix_control_plane::auth::MemberRole::Owner)
        .await;
    let other_key = seed_key(&cp, &admin, other_tenant, Some(member.id)).await;
    let mut foreign = contextual.clone();
    foreign["api_key_id"] = json!(other_key);
    let refused = dp
        .send(
            "POST",
            "/dp/v1/native-objects/lookup",
            Some(foreign.clone()),
        )
        .await;
    foreign["locator"]["raw_id"] = json!("file_context_unknown_v7");
    let unknown = dp
        .send("POST", "/dp/v1/native-objects/lookup", Some(foreign))
        .await;
    assert_eq!(
        refused.status, 404,
        "consumer context retains exact tenant ownership: {refused:?}"
    );
    assert_eq!(refused.status, unknown.status);
    assert_eq!(refused.body, unknown.body);
    let locator = json!({"api_key_id":key,"locator":{"protocol":"openai","kind":"file","raw_id":"file_results_grants_v7"}});
    let found = dp
        .send(
            "POST",
            "/dp/v1/native-objects/lookup",
            Some(locator.clone()),
        )
        .await;
    assert_eq!(found.status, 200, "{found:?}");
    let mut selector = body["origin"].clone();
    selector["model_id"] = json!(second);
    let page = json!({"api_key_id":key,"protocol":"openai","kind":"file","origin":selector,
        "raw_ids":["file_results_grants_v7","file_unknown_v7"]});
    let owned = dp
        .send("POST", "/dp/v1/native-objects/owned", Some(page.clone()))
        .await;
    assert_eq!(owned.status, 200, "{owned:?}");
    assert_eq!(
        owned.body["records"].as_array().unwrap().len(),
        1,
        "same account is listable through another allowed model"
    );
    assert_eq!(owned.body["records"][0]["raw_id"], "file_results_grants_v7");

    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .unwrap();
    let narrowed = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids":[first]}),
        )
        .await;
    assert_eq!(narrowed.status, 200, "{narrowed:?}");
    let hidden = dp
        .send("POST", "/dp/v1/native-objects/owned", Some(page))
        .await;
    assert_eq!(hidden.status, 200, "{hidden:?}");
    assert!(
        hidden.body["records"].as_array().unwrap().is_empty(),
        "owned pages enforce each saved model grant"
    );
    let denied_parent = dp
        .send("POST", "/dp/v1/native-objects/lookup", Some(contextual))
        .await;
    assert_eq!(
        denied_parent.status, 404,
        "consumer context retains every parent batch entry grant: {denied_parent:?}"
    );
    let denied = dp
        .send("POST", "/dp/v1/native-objects/lookup", Some(locator))
        .await;
    assert_eq!(
        denied.status, 404,
        "result file cannot reveal removed secondary-model output: {denied:?}"
    );
}

// 读取已授权的容器后，新 Response 必须能沿同一成员/Full 权限复用，且不改写原归属。
async fn cross_key_response_children_reuse_contract(full: bool) {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, tenant, member, owner_key) = submission(&cp).await;
    let dp = cp.issue_dp_client("cross-key-response-child-v7").await;
    let begun = dp.send("POST", BEGIN, Some(body.clone())).await;
    assert_eq!(begun.status, 200, "{begun:?}");
    let original = dp.send("POST", "/dp/v1/native-objects/bind", Some(json!({
        "operation_id":body["operation_id"],"api_key_id":owner_key,"raw_id":"resp_child_original_v7",
        "objects":[{"kind":"response_container","raw_id":"cntr_cross_key_v7"}]
    }))).await;
    assert_eq!(original.status, 200, "{original:?}");
    let original_child = original.body["records"][0].clone();
    let original_parent = id_at(&original.body, "/record/record_id");
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let consumer_member = if full {
        cp.seed_member(tenant, aisix_control_plane::auth::MemberRole::Admin)
            .await
            .id
    } else {
        member
    };
    let consumer_key =
        Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(consumer_member)).await).unwrap();
    let refresh_key =
        Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(consumer_member)).await).unwrap();
    let child_lookup = |key: Uuid| {
        json!({"api_key_id":key,"locator":{
            "protocol":"openai","kind":"response_container","raw_id":"cntr_cross_key_v7"
        }})
    };
    let admitted = dp
        .send(
            "POST",
            "/dp/v1/native-objects/lookup",
            Some(child_lookup(consumer_key)),
        )
        .await;
    assert_eq!(
        admitted.status, 200,
        "the second key can read the same child before inference: {admitted:?}"
    );
    let mut next = body.clone();
    next["operation_id"] = json!(Uuid::new_v4());
    next["api_key_id"] = json!(consumer_key);
    let next_begun = dp.send("POST", BEGIN, Some(next.clone())).await;
    assert_eq!(next_begun.status, 200, "{next_begun:?}");
    if full {
        sqlx::query("UPDATE members SET role='member' WHERE id=$1")
            .bind(consumer_member)
            .execute(cp.db().pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE tenant_keys SET revoked_at=clock_timestamp() WHERE id=$1")
        .bind(consumer_key)
        .execute(cp.db().pool())
        .await
        .unwrap();
    let binding = json!({"operation_id":next["operation_id"],"api_key_id":consumer_key,"raw_id":"resp_child_second_v7",
        "objects":[{"kind":"response_container","raw_id":"cntr_cross_key_v7"}]});
    let reused = dp
        .send("POST", "/dp/v1/native-objects/bind", Some(binding.clone()))
        .await;
    assert_eq!(
        reused.status, 200,
        "an admitted paid response binds after role/key withdrawal: {reused:?}"
    );
    assert_eq!(
        reused.body["records"][0]["record_id"],
        original_child["record_id"]
    );
    assert_eq!(
        reused.body["records"][0]["parent_id"],
        json!(original_parent)
    );
    assert_eq!(reused.body["records"][0]["api_key_id"], json!(owner_key));
    assert_eq!(reused.body["records"][0]["origin"], body["origin"]);
    let replay = dp
        .send("POST", "/dp/v1/native-objects/bind", Some(binding))
        .await;
    assert_eq!(
        replay.status, 200,
        "accepted binding remains idempotent after revocation: {replay:?}"
    );
    let persisted: (bool, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT visibility_full,api_key_id,member_id FROM native_operations WHERE operation_id=$1",
    )
    .bind(id_at(&next, "/operation_id"))
    .fetch_one(cp.db().pool())
    .await
    .unwrap();
    assert_eq!(
        persisted,
        (full, consumer_key, Some(consumer_member)),
        "visibility was captured by CP at begin"
    );
    let mut refresh = json!({"existing_record_id":reused.body["record"]["record_id"],"operation_id":next["operation_id"],
        "api_key_id":refresh_key,"raw_id":"resp_child_second_v7",
        "objects":[{"kind":"response_container","raw_id":"cntr_cross_key_v7"}]});
    let refreshed = dp
        .send("POST", "/dp/v1/native-objects/bind", Some(refresh.clone()))
        .await;
    assert_eq!(
        refreshed.status,
        if full { 404 } else { 200 },
        "management refresh uses current child visibility: {refreshed:?}"
    );
    refresh["objects"] = json!([]);
    let own_parent = dp
        .send("POST", "/dp/v1/native-objects/bind", Some(refresh))
        .await;
    assert_eq!(
        own_parent.status, 200,
        "the live second key can still access its own member's parent: {own_parent:?}"
    );
    let immutable = sqlx::query(
        "UPDATE native_operations SET visibility_full=NOT visibility_full WHERE operation_id=$1",
    )
    .bind(id_at(&next, "/operation_id"))
    .execute(cp.db().pool())
    .await;
    assert!(
        immutable.is_err(),
        "the trusted authorization snapshot is immutable"
    );
    let after = dp
        .send(
            "POST",
            "/dp/v1/native-objects/lookup",
            Some(child_lookup(owner_key)),
        )
        .await;
    assert_eq!(after.status, 200, "{after:?}");
    assert_eq!(
        after.body["record"], original_child,
        "reusing never reparents or widens the original owner"
    );

    // 其他普通成员和外租户即使声称 Full，也不能借复用接管同一上游身份。
    for foreign_tenant in [false, true] {
        let blocked_tenant = if foreign_tenant {
            seed_tenant(&cp, &admin).await
        } else {
            tenant
        };
        if foreign_tenant {
            let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
                .bind(blocked_tenant)
                .fetch_one(cp.db().pool())
                .await
                .unwrap();
            let grant = cp
                .send_conditional(
                    "PATCH",
                    &format!("/admin/tenants/{blocked_tenant}"),
                    admin.cookie(),
                    version,
                    json!({"allowed_model_ids":[body["requested_model_id"]]}),
                )
                .await;
            assert_eq!(grant.status, 200, "{grant:?}");
        }
        let other = cp
            .seed_member(
                blocked_tenant,
                if foreign_tenant {
                    aisix_control_plane::auth::MemberRole::Admin
                } else {
                    aisix_control_plane::auth::MemberRole::Member
                },
            )
            .await;
        let blocked_key =
            Uuid::parse_str(&seed_key(&cp, &admin, blocked_tenant, Some(other.id)).await).unwrap();
        let hidden = dp
            .send(
                "POST",
                "/dp/v1/native-objects/lookup",
                Some(child_lookup(blocked_key)),
            )
            .await;
        assert_eq!(hidden.status, 404, "{hidden:?}");
        let mut forged = body.clone();
        forged["operation_id"] = json!(Uuid::new_v4());
        forged["api_key_id"] = json!(blocked_key);
        forged["visibility_full"] = json!(true);
        let begun = dp.send("POST", BEGIN, Some(forged.clone())).await;
        assert_eq!(
            begun.status, 200,
            "its own inference is allowed but the other object is not: {begun:?}"
        );
        let denied=dp.send("POST","/dp/v1/native-objects/bind",Some(json!({"operation_id":forged["operation_id"],"api_key_id":blocked_key,
            "raw_id":format!("resp_blocked_{foreign_tenant}"),"objects":[{"kind":"response_container","raw_id":"cntr_cross_key_v7"}]}))).await;
        assert_eq!(
            denied.status, 409,
            "a reused ID cannot grant another principal's object: {denied:?}"
        );
        let snapshot: bool = sqlx::query_scalar(
            "SELECT visibility_full FROM native_operations WHERE operation_id=$1",
        )
        .bind(id_at(&forged, "/operation_id"))
        .fetch_one(cp.db().pool())
        .await
        .unwrap();
        assert_eq!(
            snapshot, foreign_tenant,
            "client-supplied Full flag never changes CP's decision"
        );
    }
}

#[tokio::test]
async fn same_member_second_key_reuses_response_child_after_acceptance_withdrawal() {
    cross_key_response_children_reuse_contract(false).await;
}

#[tokio::test]
async fn full_visibility_second_key_reuses_response_child_after_acceptance_withdrawal() {
    cross_key_response_children_reuse_contract(true).await;
}

#[tokio::test]
async fn native_submission_rejects_invalid_input_measurements_before_persistence() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (body, _, _, _) = submission(&cp).await;
    let dp = cp.issue_dp_client("gw-native-io-validation-v10").await;
    for (bytes, scalars) in [(0, 1), (5, 1)] {
        let mut invalid = body.clone();
        invalid["operation_id"] = json!(Uuid::new_v4());
        invalid["upstream_input_text"] =
            json!({"utf8_bytes":bytes,"unicode_scalars":scalars,"complete":true});
        let rejected = dp.send("POST", BEGIN, Some(invalid.clone())).await;
        assert_eq!(
            rejected.status, 422,
            "invalid scalar submission: {rejected:?}"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM native_operations WHERE operation_id=$1")
                .bind(id_at(&invalid, "/operation_id"))
                .fetch_one(cp.db().pool())
                .await
                .unwrap();
        assert_eq!(count, 0);
        let mut invalid_entry = body.clone();
        invalid_entry["operation_id"] = json!(Uuid::new_v4());
        invalid_entry["kind"] = json!("message_batch");
        invalid_entry["entries"] = json!([{"entry_id":"invalid-scalar","endpoint":"/v1/responses",
            "requested_model_id":body["requested_model_id"],"model_id":body["origin"]["model_id"],
            "upstream_model":body["upstream_model"],
            "upstream_input_text":{"utf8_bytes":bytes,"unicode_scalars":scalars,"complete":true}}]);
        let rejected = dp.send("POST", BEGIN, Some(invalid_entry)).await;
        assert_eq!(
            rejected.status, 422,
            "invalid per-item scalar submission: {rejected:?}"
        );
    }
}
