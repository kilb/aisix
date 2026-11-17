//! 免费计数设置走真实控制面 HTTP、数据库与投影，不访问外部提供商。
mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

const BUILD: &str = "dev+sha-a11ce11000000000000000000000000000000000";
const OLD_DEV: &str = "dev+sha-641ccf95bc96569e565d23845c9c3f612c662703";

async fn create(cp: &TestCp, admin: &OperatorSession, kind: &str, doc: Value) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/resources/{kind}"),
        Some(admin.cookie()),
        Some(json!({"doc":doc})),
    )
    .await
}
async fn seed(cp: &TestCp, admin: &OperatorSession, provider: &str) -> (String, String) {
    let pk = create(cp, admin, "provider_keys", json!({"display_name":format!("account-{provider}"),"provider":provider,"api_key":"test-not-a-real-key"})).await;
    assert_eq!(pk.status, 201, "{pk:?}");
    let key_id = pk.body["id"].as_str().expect("account id").to_owned();
    let model = create(cp, admin, "models", json!({"display_name":format!("model-{provider}"),"provider":provider,"model_name":"source-v1","provider_key_id":key_id})).await;
    assert_eq!(model.status, 201, "{model:?}");
    (
        key_id,
        model.body["id"].as_str().expect("model id").to_owned(),
    )
}
fn settings(field: &str, pk: &str, model: &str) -> Value {
    json!({field:{"provider_key_id":pk,"model_mappings":[{"model_id":model,"source_model_name":"source-v1","model_name":"counter-v1"}]}})
}
async fn beat(cp: &TestCp, admin: &OperatorSession, versions: &[&str]) -> String {
    let bundle = cp.issue_bundle(admin, "token-counting-gateway").await;
    let client = cp.dp_client(&bundle);
    for (index, version) in versions.iter().enumerate() {
        let r = client.heartbeat(json!({"dp_id":bundle.dp_id,"instance_id":format!("counter-{index}"),"hostname":"counter-test","version":version,"uptime_seconds":1,"applied_revision":0,"supported_guardrail_kinds":[],"exporter_health":[]})).await;
        assert_eq!(r.status, 200, "{r:?}");
    }
    bundle.dp_id
}
fn refused(r: &Resp) {
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "gateway_too_old", "{r:?}");
    assert!(r.body["details"][0]["path"]
        .as_str()
        .expect("pointer")
        .starts_with('/'));
}
async fn replace(
    cp: &TestCp,
    admin: &OperatorSession,
    kind: &str,
    id: &str,
    version: i64,
    doc: Value,
) -> Resp {
    cp.send_conditional(
        "PUT",
        &format!("/admin/resources/{kind}/{id}"),
        admin.cookie(),
        version,
        json!({"doc":doc}),
    )
    .await
}
async fn delete(cp: &TestCp, admin: &OperatorSession, kind: &str, id: &str, version: i64) -> Resp {
    cp.send_conditional(
        "DELETE",
        &format!("/admin/resources/{kind}/{id}"),
        admin.cookie(),
        version,
        Value::Null,
    )
    .await
}

// 若未注册单例、环境 ID 或版本并发保护，本旅程会失败。
#[tokio::test]
async fn token_counting_singleton_lifecycle_and_empty_fleet_contract() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (pk, model) = seed(&cp, &admin, "anthropic").await;
    let doc = settings("messages_count_tokens", &pk, &model);
    // 与既有门禁相同：尚无网关上报时允许预配置，不能因此宣称网关已应用。
    let r = create(&cp, &admin, "token_counting", doc.clone()).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["id"], cp.env().env_id.to_string());
    assert!(
        matches!(
            r.body["rollout"]["state"].as_str(),
            Some("pending" | "no_gateways")
        ),
        "{r:?}"
    );
    let duplicate = create(&cp, &admin, "token_counting", json!({})).await;
    assert_eq!(duplicate.status, 409, "{duplicate:?}");
    let id = cp.env().env_id.to_string();
    let stale = replace(&cp, &admin, "token_counting", &id, 0, json!({})).await;
    assert_eq!(stale.status, 412, "{stale:?}");
    let removed = delete(&cp, &admin, "token_counting", &id, 1).await;
    assert_eq!(removed.status, 200, "{removed:?}");
    let listed = cp
        .send(
            "GET",
            "/admin/resources/token_counting",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(listed.status, 200, "{listed:?}");
    assert!(listed.body["items"].as_array().expect("items").is_empty());
}

// 取消引用追踪会使缺失/删除目标可保存，或使被引用对象可删除。
#[tokio::test]
async fn token_counting_checks_live_references_and_blocks_deleting_targets_and_sources() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (_, model) = seed(&cp, &admin, "anthropic").await;
    // 计数账号没有任何 Model 引用，删除保护必须来自本单例。
    let counter = create(&cp, &admin, "provider_keys", json!({"display_name":"unmodeled-counter","provider":"anthropic","api_key":"test-only-counter"})).await;
    assert_eq!(counter.status, 201, "{counter:?}");
    let pk = counter.body["id"]
        .as_str()
        .expect("counter account")
        .to_owned();
    for (target, source, path) in [
        (
            Uuid::new_v4().to_string(),
            model.clone(),
            "/messages_count_tokens/provider_key_id",
        ),
        (
            pk.clone(),
            Uuid::new_v4().to_string(),
            "/messages_count_tokens/model_mappings/0/model_id",
        ),
    ] {
        let r = create(
            &cp,
            &admin,
            "token_counting",
            settings("messages_count_tokens", &target, &source),
        )
        .await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{r:?}");
    }
    let r = create(
        &cp,
        &admin,
        "token_counting",
        settings("messages_count_tokens", &pk, &model),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    for (kind, id) in [("provider_keys", &pk), ("models", &model)] {
        let r = delete(&cp, &admin, kind, id, 1).await;
        assert_eq!(r.status, 409, "{r:?}");
    }
    assert_eq!(
        delete(
            &cp,
            &admin,
            "token_counting",
            &cp.env().env_id.to_string(),
            1
        )
        .await
        .status,
        200
    );
    assert_eq!(delete(&cp, &admin, "models", &model, 1).await.status, 200);
    let r = create(
        &cp,
        &admin,
        "token_counting",
        settings("messages_count_tokens", &pk, &model),
    )
    .await;
    assert_eq!(r.status, 422, "deleted source: {r:?}");
    assert_eq!(
        delete(&cp, &admin, "provider_keys", &pk, 1).await.status,
        200
    );
    let r = create(
        &cp,
        &admin,
        "token_counting",
        json!({"messages_count_tokens":{"provider_key_id":pk}}),
    )
    .await;
    assert_eq!(r.status, 422, "deleted account: {r:?}");
}

// 协议、精确 direct 源或源快照校验失效时会错误接受以下配置。
#[tokio::test]
async fn token_counting_validates_protocol_exact_sources_and_source_snapshot() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (pk, model) = seed(&cp, &admin, "anthropic").await;
    let (openai, _) = seed(&cp, &admin, "openai").await;
    for (field, account) in [
        ("messages_count_tokens", &openai),
        ("responses_input_tokens", &pk),
    ] {
        let r = create(
            &cp,
            &admin,
            "token_counting",
            settings(field, account, &model),
        )
        .await;
        assert_eq!(r.status, 422, "wrong protocol: {r:?}");
    }
    let mut stale = settings("messages_count_tokens", &pk, &model);
    stale["messages_count_tokens"]["model_mappings"][0]["source_model_name"] = json!("source-v0");
    let r = create(&cp, &admin, "token_counting", stale).await;
    assert_eq!(r.status, 422, "stale snapshot: {r:?}");
    assert_eq!(
        r.body["details"][0]["path"],
        "/messages_count_tokens/model_mappings/0/source_model_name"
    );
    for source in [
        json!({"display_name":"wildcard-*","provider":"anthropic","model_name":"source-*","provider_key_id":pk}),
        json!({"display_name":"routing","routing":{"strategy":"round_robin","targets":[{"model_id":model}]}}),
    ] {
        let r = create(&cp, &admin, "models", source).await;
        assert_eq!(r.status, 201, "{r:?}");
        let r = create(
            &cp,
            &admin,
            "token_counting",
            settings(
                "messages_count_tokens",
                &pk,
                r.body["id"].as_str().expect("source"),
            ),
        )
        .await;
        assert_eq!(r.status, 422, "non-exact source: {r:?}");
    }
    let r = create(
        &cp,
        &admin,
        "token_counting",
        settings("responses_input_tokens", &openai, &model),
    )
    .await;
    assert_eq!(r.status, 201, "cross-protocol source model is valid: {r:?}");
}

// 通用 Dev 版本和既有原生接口白名单都不能授权新计数能力。
#[tokio::test]
async fn token_counting_gate_checks_exact_dev_build_release_unknown_and_mixed_fleets() {
    for (cp_version, versions, allowed) in [
        (BUILD, vec![OLD_DEV], false),
        (BUILD, vec!["dev+sha-641ccf9"], false),
        (BUILD, vec![BUILD], true),
        (BUILD, vec!["dev+sha-a11ce11"], false),
        (BUILD, vec!["dev"], false),
        (BUILD, vec!["unknown"], false),
        (BUILD, vec![BUILD, OLD_DEV], false),
        ("1.9.0+sha-a11ce11", vec!["1.8.0+sha-641ccf9"], false),
        ("1.9.0+sha-a11ce11", vec!["1.9.0+sha-c0ffee0"], true),
        ("1.9.0+sha-a11ce11", vec![BUILD], false),
    ] {
        let Some(cp) = TestCp::start_stamped(cp_version).await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let (pk, model) = seed(&cp, &admin, "anthropic").await;
        beat(&cp, &admin, &versions).await;
        let r = create(
            &cp,
            &admin,
            "token_counting",
            settings("messages_count_tokens", &pk, &model),
        )
        .await;
        if allowed {
            assert_eq!(r.status, 201, "{cp_version} {versions:?}: {r:?}");
        } else {
            refused(&r);
        }
    }
}

// 保留块变更账号或映射需要重新门禁；移除块永远可以关闭。
#[tokio::test]
async fn token_counting_gate_rechecks_edits_but_allows_disabling() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let (pk, model) = seed(&cp, &admin, "anthropic").await;
    let doc = settings("messages_count_tokens", &pk, &model);
    assert_eq!(
        create(&cp, &admin, "token_counting", doc.clone())
            .await
            .status,
        201
    );
    beat(&cp, &admin, &[OLD_DEV]).await;
    let id = cp.env().env_id.to_string();
    let r = replace(&cp, &admin, "token_counting", &id, 1, doc.clone()).await;
    assert_eq!(r.status, 200, "unchanged settings: {r:?}");
    let mut changed = doc.clone();
    changed["messages_count_tokens"]["model_mappings"][0]["model_name"] = json!("counter-v2");
    refused(&replace(&cp, &admin, "token_counting", &id, 2, changed).await);
    let other = create(
        &cp,
        &admin,
        "provider_keys",
        json!({"display_name":"other","provider":"anthropic","api_key":"test-only"}),
    )
    .await;
    let mut changed = doc;
    changed["messages_count_tokens"]["provider_key_id"] = other.body["id"].clone();
    refused(&replace(&cp, &admin, "token_counting", &id, 2, changed).await);
    let r = replace(&cp, &admin, "token_counting", &id, 2, json!({})).await;
    assert_eq!(r.status, 200, "disable: {r:?}");
}

// 不完整清单及无标记满额旧 Dev 清单都必须阻止开启。
#[tokio::test]
async fn token_counting_gate_refuses_incomplete_and_full_fleets() {
    for full in [false, true] {
        let Some(cp) = TestCp::start_stamped(BUILD).await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let (pk, model) = seed(&cp, &admin, "anthropic").await;
        let dp_id = beat(&cp, &admin, &[BUILD]).await;
        if full {
            sqlx::query("INSERT INTO dp_instances (dp_id,instance_id,hostname,version,first_seen_at,last_seen_at,uptime_seconds,applied_revision) SELECT $1::uuid,'full-' || n,'test',$2,now(),now(),1,0 FROM generate_series(1,127) n")
                .bind(&dp_id).bind(BUILD).execute(cp.db().pool()).await.expect("fill test credential");
        } else {
            sqlx::query("UPDATE dp_credentials SET instance_cap_hit_at=now() WHERE dp_id=$1::uuid")
                .bind(&dp_id)
                .execute(cp.db().pool())
                .await
                .expect("mark incomplete test credential");
        }
        let r = create(
            &cp,
            &admin,
            "token_counting",
            settings("messages_count_tokens", &pk, &model),
        )
        .await;
        refused(&r);
        let r = create(&cp, &admin, "token_counting", json!({})).await;
        assert_eq!(r.status, 201, "no-op is safe: {r:?}");
    }
}

// 取消共用资源锁会让竞争写入留下悬空的 model_id。
#[tokio::test]
async fn token_counting_source_delete_and_save_are_serialized() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (pk, model) = seed(&cp, &admin, "anthropic").await;
    let (saved, removed) = tokio::join!(
        create(
            &cp,
            &admin,
            "token_counting",
            settings("messages_count_tokens", &pk, &model)
        ),
        delete(&cp, &admin, "models", &model, 1),
    );
    assert!(
        matches!((saved.status, removed.status), (201, 409) | (422, 200)),
        "save={saved:?}, delete={removed:?}"
    );
    let dangling: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources t JOIN gw_resources m ON m.id=$1::uuid AND m.kind='models' WHERE t.kind='token_counting' AND NOT t.deleted AND m.deleted")
        .bind(&model).fetch_one(cp.db().pool()).await.expect("check surviving references");
    assert_eq!(dangling, 0, "no live setting may name the deleted source");
}

// 若新 kind 未接入投影、replace/delete 或 unknown_kind 回报，实际 kine 与 rollout 会分歧。
#[tokio::test]
async fn token_counting_projects_replace_delete_and_reports_unknown_kind_as_unapplied() {
    use std::time::{Duration, Instant};
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.cp_version = BUILD.into();
        cfg.kine_endpoint = endpoint.clone();
    })
    .await
    else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let (pk, model) = seed(&cp, &admin, "anthropic").await;
    let doc = settings("messages_count_tokens", &pk, &model);
    let r = create(&cp, &admin, "token_counting", doc.clone()).await;
    assert_eq!(r.status, 201, "{r:?}");
    let key = format!(
        "{}token_counting/{}",
        cp.env().env_prefix(),
        cp.env().env_id
    );
    let mut kine = etcd_client::Client::connect([endpoint], None)
        .await
        .expect("connect test kine");
    async fn await_doc(
        client: &mut etcd_client::Client,
        key: &str,
        expected: Option<&Value>,
    ) -> i64 {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let response = client.get(key, None).await.expect("read projected setting");
            let found = response
                .kvs()
                .first()
                .map(|kv| serde_json::from_slice::<Value>(kv.value()).expect("projected JSON"));
            if found.as_ref() == expected {
                return response.header().expect("revision").revision();
            }
            assert!(
                Instant::now() < deadline,
                "projection mismatch: {found:?}, wanted {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let revision = await_doc(&mut kine, &key, Some(&doc)).await;
    let bundle = cp.issue_bundle(&admin, "unknown-token-counting-kind").await;
    let r = cp.dp_client(&bundle).heartbeat(json!({"dp_id":bundle.dp_id,"instance_id":"old-token-counting","hostname":"old-test","version":OLD_DEV,"uptime_seconds":1,"applied_revision":revision,"supported_guardrail_kinds":[],"exporter_health":[],"rejected_resources":[{"key":key,"kind":"unknown_kind","error":"unsupported resource kind","timestamp_unix_secs":1_770_000_000u64}]})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp
        .send(
            "GET",
            &format!("/admin/resources/token_counting/{}", cp.env().env_id),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["rollout"]["state"], "rejected", "{r:?}");
    assert_eq!(r.body["rollout"]["applied"], 0, "{r:?}");
    let r = replace(
        &cp,
        &admin,
        "token_counting",
        &cp.env().env_id.to_string(),
        1,
        json!({}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    await_doc(&mut kine, &key, Some(&json!({}))).await;
    let r = delete(
        &cp,
        &admin,
        "token_counting",
        &cp.env().env_id.to_string(),
        2,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    await_doc(&mut kine, &key, None).await;
}

// Responses 块必须独立门禁，不能只保护 Messages。
#[tokio::test]
async fn token_counting_responses_gate_rechecks_target_and_mapping_and_allows_removal() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let (pk, model) = seed(&cp, &admin, "openai").await;
    let doc = settings("responses_input_tokens", &pk, &model);
    let dp_id = beat(&cp, &admin, &[OLD_DEV]).await;
    let r = create(&cp, &admin, "token_counting", doc.clone()).await;
    refused(&r);
    assert_eq!(r.body["details"][0]["path"], "/responses_input_tokens");
    sqlx::query("UPDATE dp_instances SET version=$2 WHERE dp_id=$1::uuid")
        .bind(&dp_id)
        .bind(BUILD)
        .execute(cp.db().pool())
        .await
        .expect("upgrade the test gateway");
    assert_eq!(
        create(&cp, &admin, "token_counting", doc.clone())
            .await
            .status,
        201
    );
    sqlx::query("UPDATE dp_instances SET version=$2 WHERE dp_id=$1::uuid")
        .bind(&dp_id)
        .bind(OLD_DEV)
        .execute(cp.db().pool())
        .await
        .expect("exercise a later older test gateway");
    let id = cp.env().env_id.to_string();
    let mut changed = doc.clone();
    changed["responses_input_tokens"]["model_mappings"][0]["model_name"] = json!("counter-v2");
    refused(&replace(&cp, &admin, "token_counting", &id, 1, changed).await);
    let other = create(
        &cp,
        &admin,
        "provider_keys",
        json!({"display_name":"other-openai","provider":"openai","api_key":"test-only"}),
    )
    .await;
    let mut changed = doc;
    changed["responses_input_tokens"]["provider_key_id"] = other.body["id"].clone();
    refused(&replace(&cp, &admin, "token_counting", &id, 1, changed).await);
    assert_eq!(
        replace(&cp, &admin, "token_counting", &id, 1, json!({}))
            .await
            .status,
        200
    );
}

// 高 revision 不证明旧网关理解新 kind；unknown_kind 明细缺失或截断仍不能显示已应用。
#[tokio::test]
async fn token_counting_rollout_requires_capability_even_without_unknown_kind_details() {
    use std::time::{Duration, Instant};
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.cp_version = BUILD.into();
        cfg.kine_endpoint = endpoint.clone();
    })
    .await
    else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let (pk, model) = seed(&cp, &admin, "anthropic").await;
    let doc = settings("messages_count_tokens", &pk, &model);
    assert_eq!(create(&cp, &admin, "token_counting", doc).await.status, 201);
    let id = cp.env().env_id.to_string();
    let key = format!("{}token_counting/{id}", cp.env().env_prefix());
    let mut kine = etcd_client::Client::connect([endpoint], None)
        .await
        .expect("connect real kine");
    let deadline = Instant::now() + Duration::from_secs(30);
    let revision = loop {
        let response = kine
            .get(key.clone(), None)
            .await
            .expect("read real projection");
        if !response.kvs().is_empty() {
            break response.header().expect("revision").revision();
        }
        assert!(Instant::now() < deadline, "setting must project");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    loop {
        let r = cp
            .send(
                "GET",
                &format!("/admin/resources/token_counting/{id}"),
                Some(admin.cookie()),
                None,
            )
            .await;
        assert_eq!(r.status, 200, "{r:?}");
        if r.body["rollout"]["state"] == "no_gateways" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "projection acknowledgement: {r:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let bundle = cp.issue_bundle(&admin, "late-old-token-counting").await;
    let client = cp.dp_client(&bundle);
    let mut heartbeat = json!({"dp_id":bundle.dp_id,"instance_id":"late-old-counting","hostname":"late-old-counting","uptime_seconds":1,"applied_revision":revision,"supported_guardrail_kinds":[],"exporter_health":[],"version":OLD_DEV});
    for truncated in [false, true] {
        if truncated {
            heartbeat["rejected_resources"] = Value::Array((0..256).map(|index| json!({"key":format!("{}future_kind/{index}",cp.env().env_prefix()),"kind":"unknown_kind","error":"unsupported future kind","timestamp_unix_secs":1_770_000_000u64})).collect());
        }
        let r = client.heartbeat(heartbeat.clone()).await;
        assert_eq!(r.status, 200, "{r:?}");
        let r = cp
            .send(
                "GET",
                &format!("/admin/resources/token_counting/{id}"),
                Some(admin.cookie()),
                None,
            )
            .await;
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(
            r.body["rollout"]["state"], "rejected",
            "truncated={truncated}: {r:?}"
        );
        assert_eq!(r.body["rollout"]["applied"], 0, "{r:?}");
        assert!(!r.body["rollout"]["rejections"]
            .as_array()
            .expect("rejections")
            .is_empty());
        let summary = cp
            .send("GET", "/admin/rollout", Some(admin.cookie()), None)
            .await;
        assert_eq!(summary.status, 200, "{summary:?}");
        assert!(
            summary.body["rejected"]
                .as_array()
                .expect("rejected rows")
                .iter()
                .any(|row| row["kind"] == "token_counting" && row["id"] == id),
            "{summary:?}"
        );
    }
    heartbeat["version"] = json!(BUILD);
    heartbeat["rejected_resources"] = json!([]);
    assert_eq!(client.heartbeat(heartbeat).await.status, 200);
    let r = cp
        .send(
            "GET",
            &format!("/admin/resources/token_counting/{id}"),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.body["rollout"]["state"], "applied", "{r:?}");
    assert_eq!(r.body["rollout"]["applied"], 1, "{r:?}");
}

// embedding 仍有直连三元组，但不属于这两个文本计数接口可映射的源模型。
#[tokio::test]
async fn token_counting_mapping_rejects_embedding_sources() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (openai, direct) = seed(&cp, &admin, "openai").await;
    let (anthropic, _) = seed(&cp, &admin, "anthropic").await;
    let source = create(&cp, &admin, "models", json!({"display_name":"embedding-source","provider":"openai","model_name":"source-v1","provider_key_id":openai,"embedding":{"dimensions":3}})).await;
    assert_eq!(source.status, 201, "{source:?}");
    let embedding_id = source.body["id"].as_str().expect("embedding id");
    for (field, account) in [
        ("messages_count_tokens", &anthropic),
        ("responses_input_tokens", &openai),
    ] {
        let r = create(
            &cp,
            &admin,
            "token_counting",
            settings(field, account, embedding_id),
        )
        .await;
        assert_eq!(r.status, 422, "embedding source at {field}: {r:?}");
        assert_eq!(
            r.body["details"][0]["path"],
            format!("/{field}/model_mappings/0/model_id"),
            "{r:?}"
        );
    }
    let r = create(
        &cp,
        &admin,
        "token_counting",
        settings("responses_input_tokens", &openai, &direct),
    )
    .await;
    assert_eq!(r.status, 201, "the text direct source remains valid: {r:?}");
}
