//! 密钥与初始费用额度应在一个事务中生效，失败时不能留下无额度的密钥。

mod support;

use std::sync::Arc;
use std::time::Duration;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::projector::key_for;
use aisix_control_plane::resources::Kind;
use aisix_control_plane::tenants::TenantStatus;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ProxyConfig};
use aisix_etcd::{build_snapshot, PrefixSet, RawEntry};
use aisix_gateway::Hub;
use aisix_proxy::{build_router, ProxyState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use etcd_client::{Client, GetOptions};
use http_body_util::{BodyExt, Limited};
use serde_json::{json, Value};
use support::{Resp, TestCp};
use tower::ServiceExt;
use uuid::Uuid;

async fn projected_snapshot(cp: &TestCp) -> AisixSnapshot {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE projected_version IS DISTINCT FROM version")
                .fetch_one(cp.db().pool()).await.expect("读取实际投影进度");
            if pending == 0 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("真实控制面投影至Kine");
    let mut kine = Client::connect([support::kine_endpoint().expect("Kine配置")], None)
        .await
        .expect("连接真实Kine");
    let prefix = cp.env().env_prefix();
    let loaded = kine
        .get(prefix.clone(), Some(GetOptions::new().with_prefix()))
        .await
        .expect("读取实际网关资源");
    let entries: Vec<_> = loaded
        .kvs()
        .iter()
        .map(|kv| RawEntry {
            key: String::from_utf8(kv.key().to_vec()).expect("资源路径"),
            value: kv.value().to_vec(),
            revision: kv.mod_revision(),
        })
        .collect();
    assert!(!entries.is_empty(), "实际Kine资源不能为空");
    let (snapshot, result) = build_snapshot(&PrefixSet::single(prefix), &entries);
    assert_eq!(result.accepted, entries.len(), "{result:?}");
    assert!(result.rejections.is_empty(), "{result:?}");
    snapshot
}

async fn list_budgets(cp: &TestCp, tenant: Uuid, cookie: &str) -> Vec<Value> {
    let response = cp
        .send(
            "GET",
            &format!("/admin/tenants/{tenant}/budgets"),
            Some(cookie),
            None,
        )
        .await;
    assert_eq!(response.status, 200, "{response:?}");
    response.body["items"].as_array().expect("额度列表").clone()
}

async fn budget_for_key(cp: &TestCp, tenant: Uuid, cookie: &str, key: &Value) -> Value {
    let budgets = list_budgets(cp, tenant, cookie).await;
    let matching: Vec<_> = budgets
        .into_iter()
        .filter(|budget| budget["target"]["id"] == key["id"])
        .collect();
    assert_eq!(matching.len(), 1, "一个明确的密钥额度：{matching:?}");
    let ordered: bool = sqlx::query_scalar("SELECT budget.seq < api_key.seq FROM gw_resources budget CROSS JOIN gw_resources api_key WHERE budget.kind = 'budgets' AND budget.id = $1::uuid AND api_key.kind = 'api_keys' AND api_key.id = $2::uuid")
        .bind(matching[0]["id"].as_str().expect("额度标识"))
        .bind(key["id"].as_str().expect("密钥标识"))
        .fetch_one(cp.db().pool()).await.expect("初始投影顺序");
    assert!(ordered, "预算必须在可用密钥之前进入投影序列");
    matching[0].clone()
}

async fn assert_budget_audit(cp: &TestCp, budget: &Value, kind: &str, actor: Uuid) {
    let stored: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT created_by_kind, created_by FROM billing_budgets_cny WHERE id = $1::uuid",
    )
    .bind(budget["id"].as_str().expect("额度标识"))
    .fetch_one(cp.db().pool())
    .await
    .expect("读取真实设置方");
    assert_eq!(stored, (kind.to_owned(), Some(actor)));
}

async fn assert_key_absent(cp: &TestCp, tenant: Uuid, label: &str) {
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_keys WHERE tenant_id = $1 AND label = $2")
            .bind(tenant)
            .bind(label)
            .fetch_one(cp.db().pool())
            .await
            .expect("读取未提交密钥");
    assert_eq!(count, 0, "额度失败时不能留下已创建的密钥");
}

#[tokio::test]
async fn operator_creates_a_key_and_its_own_initial_cumulative_quota() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let response = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(operator.cookie()),
            Some(json!({
                "label": "operator-quota-469",
                "budget": {"kind": "cumulative", "limit_cny": "12.34"}
            })),
        )
        .await;
    assert_eq!(response.status, 201, "{response:?}");
    assert!(response.body["plaintext"].is_string(), "{response:?}");
    let budget = budget_for_key(&cp, tenant, operator.cookie(), &response.body["key"]).await;
    assert_eq!(budget["scope"], "api_key");
    assert_eq!(budget["set_by"], "operator");
    assert_eq!(budget["kind"], "cumulative");
    assert_eq!(budget.get("period"), Some(&Value::Null));
    assert_eq!(budget["limit_micro_cny"], json!(12_340_000));
    assert_eq!(budget["spent_micro_cny"], json!(0));
    assert_budget_audit(&cp, &budget, "operator", operator.id).await;

    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let refused = cp
        .send(
            "PATCH",
            &format!("/api/budgets/{}", budget["id"].as_str().expect("额度标识")),
            Some(owner.cookie()),
            Some(json!({"limit_cny": "99"})),
        )
        .await;
    assert_eq!(refused.status, 403, "{refused:?}");
    assert_eq!(refused.body["code"], "budget_managed");
}

#[tokio::test]
async fn owner_and_member_create_initial_quotas_at_their_existing_levels() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    for (role, setter) in [
        (MemberRole::Owner, "org_admin"),
        (MemberRole::Member, "member"),
    ] {
        let member = cp.member_login_seeded(tenant, role).await;
        let response = cp
            .send(
                "POST",
                "/api/keys",
                Some(member.cookie()),
                Some(json!({
                    "label": format!("{setter}-quota-469"),
                    "budget": {"kind": "periodic", "period": "day", "limit_cny": "0.01"}
                })),
            )
            .await;
        assert_eq!(response.status, 201, "{response:?}");
        let budget = budget_for_key(&cp, tenant, operator.cookie(), &response.body["key"]).await;
        assert_eq!(budget["scope"], "api_key");
        assert_eq!(budget["set_by"], setter);
        assert_eq!(budget["kind"], "periodic");
        assert_eq!(budget["period"], "day");
        assert_eq!(budget["limit_micro_cny"], json!(10_000));
        assert_budget_audit(&cp, &budget, "member", member.id).await;
    }
}

#[tokio::test]
async fn invalid_initial_quota_rejects_without_creating_either_object() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let before = cp.count("billing_budgets_cny").await;
    for (path, cookie, label) in [
        (
            format!("/admin/tenants/{tenant}/keys"),
            operator.cookie(),
            "bad-admin-469",
        ),
        ("/api/keys".to_owned(), owner.cookie(), "bad-owner-469"),
    ] {
        let response = cp
            .send(
                "POST",
                &path,
                Some(cookie),
                Some(json!({
                    "label": label,
                    "budget": {"kind": "cumulative", "limit_cny": "0"}
                })),
            )
            .await;
        assert_eq!(response.status, 422, "{response:?}");
        let details = response.body["details"].as_array().expect("校验明细");
        assert!(
            details
                .iter()
                .any(|entry| entry["path"] == "/budget/limit_cny"),
            "{response:?}"
        );
        assert_key_absent(&cp, tenant, label).await;
    }
    assert_eq!(cp.count("billing_budgets_cny").await, before);
}

#[tokio::test]
async fn an_operator_can_add_a_quota_to_an_existing_key_but_not_a_member() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let minted = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(operator.cookie()),
            Some(json!({"label": "unlimited-469"})),
        )
        .await;
    assert_eq!(minted.status, 201, "{minted:?}");
    assert!(list_budgets(&cp, tenant, operator.cookie())
        .await
        .is_empty());
    let created = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/budgets"),
            Some(operator.cookie()),
            Some(json!({
                "scope": "api_key", "scope_ref": minted.body["key"]["id"],
                "kind": "cumulative", "limit_cny": "5.67"
            })),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    assert_eq!(created.body["scope"], "api_key");
    assert_eq!(created.body["set_by"], "operator");
    assert_eq!(created.body["limit_micro_cny"], json!(5_670_000));
    assert_budget_audit(&cp, &created.body, "operator", operator.id).await;

    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let denied: Resp = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/budgets"),
            Some(operator.cookie()),
            Some(json!({
                "scope": "member", "scope_ref": owner.id,
                "kind": "cumulative", "limit_cny": "9"
            })),
        )
        .await;
    assert_eq!(denied.status, 422, "{denied:?}");
}

#[tokio::test]
async fn quota_limit_failure_rolls_back_the_key_its_document_and_audit() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let mut made = 0;
    // 现有公开上限为100；经真实接口准备两个设置方的累计、日、周、月额度。
    for number in 0..13 {
        let minted = cp
            .send(
                "POST",
                &format!("/admin/tenants/{tenant}/keys"),
                Some(operator.cookie()),
                Some(json!({
                    "label": format!("existing-cap-{number}-469"), "member_id": member.id
                })),
            )
            .await;
        assert_eq!(minted.status, 201, "{minted:?}");
        for cookie in [owner.cookie(), member.cookie()] {
            for period in [None, Some("day"), Some("week"), Some("month")] {
                if made == 100 {
                    break;
                }
                let mut body = json!({
                    "scope": "api_key", "scope_ref": minted.body["key"]["id"],
                    "kind": if period.is_some() { "periodic" } else { "cumulative" },
                    "limit_cny": "1"
                });
                if let Some(period) = period {
                    body["period"] = json!(period);
                }
                let response = cp
                    .send("POST", "/api/budgets", Some(cookie), Some(body))
                    .await;
                assert_eq!(response.status, 201, "{response:?}");
                made += 1;
            }
        }
    }
    assert_eq!(made, 100);
    let before = (
        cp.count("tenant_keys").await,
        cp.count("gw_resources").await,
        cp.count("audit_log").await,
        cp.count("billing_budgets_cny").await,
    );
    let failed = cp
        .send(
            "POST",
            "/api/keys",
            Some(owner.cookie()),
            Some(json!({
                "label": "must-roll-back-469", "budget": {"kind": "cumulative", "limit_cny": "1"}
            })),
        )
        .await;
    assert_eq!(failed.status, 409, "{failed:?}");
    assert_eq!(failed.body["code"], "budget_limit");
    assert_key_absent(&cp, tenant, "must-roll-back-469").await;
    assert_eq!(
        before,
        (
            cp.count("tenant_keys").await,
            cp.count("gw_resources").await,
            cp.count("audit_log").await,
            cp.count("billing_budgets_cny").await,
        )
    );
}

#[tokio::test]
async fn internal_key_quota_counts_real_usage_and_blocks_the_production_router() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Admin).await;
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(cp.db().pool())
        .await
        .expect("内部租户");
    let group = support::world::group_id(&cp, "shared").await;
    let account = cp.send("POST", "/admin/resources/provider_keys", Some(operator.cookie()), Some(json!({"doc": {
        "display_name": "quota-closed-upstream-469", "provider": "openai",
        "api_base": "http://127.0.0.1:9", "api_key": "sk-quota-local-unused-469", "groups": ["shared"]
    }}))).await;
    assert_eq!(account.status, 201, "{account:?}");
    let model_response = cp.send("POST", "/admin/resources/models", Some(operator.cookie()), Some(json!({"doc": {
        "display_name": "quota-model-469", "provider": "openai", "model_name": "gpt-4o-mini",
        "provider_key_id": account.body["id"]
    }}))).await;
    assert_eq!(model_response.status, 201, "{model_response:?}");
    let model = Uuid::parse_str(model_response.body["id"].as_str().expect("模型标识"))
        .expect("UUID模型标识");
    support::world::price(&cp, model).await;
    let model_name: String = sqlx::query_scalar(
        "SELECT doc->>'display_name' FROM gw_resources WHERE kind = 'models' AND id = $1",
    )
    .bind(model)
    .fetch_one(cp.db().pool())
    .await
    .expect("实际模型名称");
    let created = cp
        .send(
            "POST",
            &format!("/admin/tenants/{internal}/keys"),
            Some(operator.cookie()),
            Some(json!({
            "label": "internal-capped-469", "group_id": group, "allowed_models": [model_name],
                    "budget": {"kind": "cumulative", "limit_cny": "0.01"}
                })),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    let key = &created.body["key"];
    let budget = budget_for_key(&cp, internal, operator.cookie(), key).await;
    assert_eq!(budget["set_by"], "operator");
    let _initial_snapshot = projected_snapshot(&cp).await;
    let mut kine =
        etcd_client::Client::connect([support::kine_endpoint().expect("真实Kine配置")], None)
            .await
            .expect("连接真实Kine");
    let read_revision = async |kine: &mut etcd_client::Client, kind: Kind, id: Uuid| {
        let response = kine
            .get(key_for(cp.env(), kind, id), None)
            .await
            .expect("读取实际投影版本");
        assert_eq!(response.kvs().len(), 1, "实际Kine文档必须存在");
        response.kvs()[0].mod_revision()
    };
    let budget_revision = read_revision(
        &mut kine,
        Kind::Budgets,
        Uuid::parse_str(budget["id"].as_str().expect("额度标识")).expect("额度UUID"),
    )
    .await;
    let key_revision = read_revision(
        &mut kine,
        Kind::ApiKeys,
        Uuid::parse_str(key["id"].as_str().expect("密钥标识")).expect("密钥UUID"),
    )
    .await;
    assert!(
        budget_revision < key_revision,
        "实际Kine必须先发布预算再发布密钥：{budget_revision}/{key_revision}"
    );
    let dp = cp.issue_dp_client("key-quota-469").await;
    let event_id = format!("internal-key-quota-{}", Uuid::new_v4());
    let delivered = support::world::deliver(&dp, Uuid::new_v4(), vec![json!({
        "event_id": event_id, "request_id": event_id, "occurred_at": Utc::now().to_rfc3339(),
        "team_id": internal, "api_key_id": key["id"], "requested_model_id": model,
        "operation": "chat", "status_code": 200, "prompt_tokens": 20_000, "completion_tokens": 0,
        "is_final_attempt": true
    })]).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    let charge: i64 =
        sqlx::query_scalar("SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = $1")
            .bind(&event_id)
            .fetch_one(cp.db().pool())
            .await
            .expect("实际计价记录");
    assert_eq!(charge, 20_000);
    let budget_id = budget["id"].as_str().expect("额度标识");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let exhausted: bool = sqlx::query_scalar("SELECT coalesce(bool_or(doc ? 'exhausted'),false) FROM gw_resources WHERE kind = 'budgets' AND id = $1::uuid AND NOT deleted")
                .bind(budget_id).fetch_one(cp.db().pool()).await.expect("真实异步额度标记");
            if exhausted { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("计费触发额度结算，而不是测试强制补标记");
    let quota = dp
        .send(
            "GET",
            &format!(
                "/dp/v1/keys/{}/quota",
                key["id"].as_str().expect("密钥标识")
            ),
            None,
        )
        .await;
    assert_eq!(quota.status, 200, "{quota:?}");
    assert_eq!(quota.body.get("org"), Some(&Value::Null));
    assert_eq!(quota.body["effective_remaining"], "0.000000", "{quota:?}");
    assert_eq!(quota.body["binding_level"], "key");
    let balance_marks: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources r JOIN billing_accounts a ON r.id = a.balance_budget_id WHERE a.tenant_id = $1 AND r.kind = 'budgets' AND NOT r.deleted")
        .bind(internal).fetch_one(cp.db().pool()).await.expect("内部余额标记");
    assert_eq!(balance_marks, 0, "内部租户仍不按余额停用");

    // 真实kine投影进入生产路由；真实适配器只配置未监听的本机端口，不发付费或替身请求。
    let snapshot = projected_snapshot(&cp).await;
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr": "127.0.0.1:0"})).expect("网关配置");
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            &cp.state().pki.platform_id_key_hex(),
        ));
    let response = build_router(state).oneshot(Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", created.body["plaintext"].as_str().expect("密钥明文")))
        .body(Body::from(json!({"model":model_name,"messages":[{"role":"user","content":"quota check"}]}).to_string()))
        .expect("真实路由请求")).await.expect("生产路由返回");
    let status = response.status();
    let wire: Value = serde_json::from_slice(
        &Limited::new(response.into_body(), 1_048_576)
            .collect()
            .await
            .expect("拒绝响应")
            .to_bytes(),
    )
    .expect("JSON拒绝响应");
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{wire}");
    assert_eq!(wire["error"]["code"], "budget_exceeded");
}

#[tokio::test]
async fn unconfigured_internal_key_usage_stays_unlimited_without_mark_projection() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Admin).await;
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(cp.db().pool())
        .await
        .expect("内部租户");
    let group = support::world::group_id(&cp, "shared").await;
    let model = support::world::seed_model(&cp, &operator).await;
    support::world::price(&cp, model).await;
    let created = cp
        .send(
            "POST",
            &format!("/admin/tenants/{internal}/keys"),
            Some(operator.cookie()),
            Some(json!({
                "label": "internal-unlimited-469", "group_id": group
            })),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    let key = &created.body["key"];
    let dp = cp.issue_dp_client("internal-unlimited-469").await;
    let event_id = format!("internal-unlimited-{}", Uuid::new_v4());
    let delivered = support::world::deliver(&dp, Uuid::new_v4(), vec![json!({
        "event_id": event_id, "request_id": event_id, "occurred_at": Utc::now().to_rfc3339(),
        "team_id": internal, "api_key_id": key["id"], "requested_model_id": model,
        "operation": "chat", "status_code": 200, "prompt_tokens": 20_000, "completion_tokens": 0,
        "is_final_attempt": true
    })]).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    let charge: i64 =
        sqlx::query_scalar("SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = $1")
            .bind(event_id)
            .fetch_one(cp.db().pool())
            .await
            .expect("真实非零计费");
    assert_eq!(charge, 20_000);
    let dirty: bool =
        sqlx::query_scalar("SELECT marks_dirty FROM billing_accounts WHERE tenant_id = $1")
            .bind(internal)
            .fetch_one(cp.db().pool())
            .await
            .expect("已有账户状态");
    assert!(!dirty, "未配额度的内部用量不触发无效标记结算");
    assert!(list_budgets(&cp, internal, operator.cookie())
        .await
        .is_empty());
    let marks: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE kind = 'budgets'")
        .fetch_one(cp.db().pool())
        .await
        .expect("实际预算投影数量");
    assert_eq!(marks, 0, "未配额度的内部租户不产生额度或余额文档");
    let quota = dp
        .send(
            "GET",
            &format!(
                "/dp/v1/keys/{}/quota",
                key["id"].as_str().expect("密钥标识")
            ),
            None,
        )
        .await;
    assert_eq!(quota.status, 200, "{quota:?}");
    assert_eq!(quota.body.get("org"), Some(&Value::Null));
    assert_eq!(quota.body.get("effective_remaining"), Some(&Value::Null));
    assert_eq!(quota.body.get("quotas"), Some(&json!([])));
}

#[tokio::test]
async fn refused_final_key_projection_rolls_back_the_already_created_budget() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    // sequence是有意的非事务观察点：即使最终写回滚，仍能证明此前预算及文档已真实写入。
    sqlx::raw_sql(
        "CREATE SEQUENCE key469_budget_seen; \
         CREATE FUNCTION key469_refuse_final_projection() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF NEW.kind = 'api_keys' THEN \
             IF EXISTS (SELECT 1 FROM billing_budgets_cny b \
               JOIN billing_budget_spend_cny s ON s.budget_id = b.id \
               JOIN gw_resources r ON r.kind = 'budgets' AND r.id = b.id \
               WHERE b.scope = 'api_key' AND b.scope_ref = NEW.id AND b.deleted_at IS NULL AND NOT r.deleted) THEN \
               PERFORM nextval('key469_budget_seen'); \
             END IF; \
             RAISE EXCEPTION 'key469 final projection refused' USING ERRCODE = '23514'; \
           END IF; \
           RETURN NEW; \
         END $$; \
         CREATE TRIGGER key469_refuse_final_projection BEFORE INSERT OR UPDATE ON gw_resources \
         FOR EACH ROW EXECUTE FUNCTION key469_refuse_final_projection();",
    )
    .execute(cp.db().pool())
    .await
    .expect("安装仅在此隔离数据库生效的最终写拒绝");
    let counts = || async {
        (
            cp.count("tenant_keys").await,
            cp.count("billing_budgets_cny").await,
            cp.count("billing_budget_spend_cny").await,
            cp.count("gw_resources").await,
            cp.count("audit_log").await,
        )
    };
    for (number, path, cookie, label) in [
        (
            1_i64,
            format!("/admin/tenants/{tenant}/keys"),
            operator.cookie(),
            "final-admin-failure-469",
        ),
        (
            2_i64,
            "/api/keys".to_owned(),
            owner.cookie(),
            "final-owner-failure-469",
        ),
    ] {
        let before = counts().await;
        let failed = cp
            .send(
                "POST",
                &path,
                Some(cookie),
                Some(json!({
                    "label": label, "budget": {"kind": "cumulative", "limit_cny": "1"}
                })),
            )
            .await;
        assert_eq!(failed.status, 500, "{failed:?}");
        let witness: (i64, bool) =
            sqlx::query_as("SELECT last_value, is_called FROM key469_budget_seen")
                .fetch_one(cp.db().pool())
                .await
                .expect("读取实际预算成功观察点");
        assert_eq!(
            witness,
            (number, true),
            "最终密钥写被拒前，预算、spend及预算文档必须已成功"
        );
        assert_eq!(counts().await, before, "最终写失败时所有事务内容均回滚");
        assert_key_absent(&cp, tenant, label).await;
    }
}
