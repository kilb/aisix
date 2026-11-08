//! A key's quota at every level (`GET /dp/v1/keys/{key_id}/quota` for
//! gateways, `GET /api/keys/{id}/quota` for the portal): the organization's
//! balance, the quotas of the organization, the member and the key, the
//! smallest remaining amount and the level that holds it. What is shown of
//! the organization follows the key's member's current role.

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::billing::budgets::{self, ChargedRow, CountMode};
use chrono::Utc;
use futures::FutureExt;
use serde_json::{json, Value};
use support::members::MemberSession;
use support::world::{seed_key, seed_tenant};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The brief's world: a postpaid tenant with a credit limit of 1 000 000 µ$,
/// an operator `team` quota of 0.50 a month, an owner's quota of 0.20 on
/// member M, M's own cumulative cap of 0.05 on its key K, and 3 charges of
/// 10 000 µ$ on K.
struct Quota {
    cp: TestCp,
    admin: OperatorSession,
    tenant: Uuid,
    owner: MemberSession,
    m: MemberSession,
    key: Uuid,
    team_quota: Uuid,
}

async fn post(cp: &TestCp, who: &str, path: &str, body: Value) -> Resp {
    cp.send("POST", path, Some(who), Some(body)).await
}

async fn setup() -> Option<Quota> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = seed_tenant(&cp, &admin).await;
    let r = cp
        .send(
            "PATCH",
            &format!("/admin/tenants/{tenant}/billing"),
            Some(admin.cookie()),
            Some(json!({"credit_limit_cny": "1.00"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    sqlx::query("UPDATE tenants SET billing_mode = 'postpaid' WHERE id = $1")
        .bind(tenant)
        .execute(cp.db().pool())
        .await
        .expect("make the tenant postpaid");
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let m = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let key = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(m.id)).await).expect("a key id");
    let r = post(
        &cp,
        admin.cookie(),
        &format!("/admin/tenants/{tenant}/budgets"),
        json!({"limit_usd": "0.50"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let team_quota = Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid");
    let r = post(
        &cp,
        owner.cookie(),
        "/api/budgets",
        json!({"scope": "member", "scope_ref": m.id, "limit_usd": "0.20"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let r = post(
        &cp,
        m.cookie(),
        "/api/budgets",
        json!({"scope": "api_key", "scope_ref": key, "kind": "cumulative", "limit_usd": "0.05"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    // Three charges of 10 000 µ$ on K, counted as intake counts them.
    let rows: Vec<ChargedRow> = (0..3)
        .map(|_| ChargedRow {
            tenant_id: tenant,
            member_id: Some(m.id),
            api_key_id: key,
            price_instant: Utc::now(),
            amount: 10_000,
        })
        .collect();
    cp.db()
        .tx(move |conn| {
            async move { budgets::count_charges(conn, &rows, CountMode::Usage).await }.boxed()
        })
        .await
        .expect("count the charges");
    sqlx::query("UPDATE billing_accounts SET balance_micro_cny = -30000 WHERE tenant_id = $1")
        .bind(tenant)
        .execute(cp.db().pool())
        .await
        .expect("spend the balance");
    Some(Quota {
        cp,
        admin,
        tenant,
        owner,
        m,
        key,
        team_quota,
    })
}

async fn portal(q: &Quota, who: &MemberSession, key: Uuid) -> Resp {
    q.cp.send(
        "GET",
        &format!("/api/keys/{key}/quota"),
        Some(who.cookie()),
        None,
    )
    .await
}

async fn dp(q: &Quota, dp: &support::DpClient, key: Uuid) -> Resp {
    let _ = q;
    dp.send("GET", &format!("/dp/v1/keys/{key}/quota"), None)
        .await
}

fn scopes(r: &Resp) -> Vec<String> {
    r.body["quotas"]
        .as_array()
        .expect("quotas")
        .iter()
        .map(|f| f["scope"].as_str().expect("a scope").to_string())
        .collect()
}

#[tokio::test]
async fn a_key_quota_reports_every_level_and_the_smallest_remaining() {
    let Some(q) = setup().await else { return };
    let r = portal(&q, &q.owner, q.key).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["org"]["available_micro_cny"], json!(970_000));
    assert_eq!(r.body["org"]["balance_micro_cny"], json!(-30_000));
    assert_eq!(r.body["org"]["credit_limit_micro_cny"], json!(1_000_000));
    let remaining = |scope: &str| {
        r.body["quotas"]
            .as_array()
            .expect("quotas")
            .iter()
            .find(|f| f["scope"] == json!(scope))
            .map(|f| f["remaining_micro_usd"].clone())
    };
    assert_eq!(remaining("team"), Some(json!(470_000)));
    assert_eq!(remaining("member"), Some(json!(170_000)));
    assert_eq!(remaining("api_key"), Some(json!(20_000)));
    assert_eq!(r.body["effective_remaining_micro_usd"], json!(20_000));
    assert_eq!(r.body["binding_level"], json!("key"));
}

#[tokio::test]
async fn a_member_keys_lookup_shows_no_organization_figures() {
    let Some(q) = setup().await else { return };
    let gw = q.cp.issue_dp_client("gw-quota-a").await;
    let r = dp(&q, &gw, q.key).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["org"], Value::Null);
    let mut got = scopes(&r);
    got.sort();
    assert_eq!(got, ["api_key", "member"]);
    assert_eq!(r.body["effective_remaining_usd"], json!("0.020000"));
    assert_eq!(r.body["binding_level"], json!("key"));
    // The organization binds, and is still hidden.
    let r =
        q.cp.send(
            "PATCH",
            &format!("/admin/tenants/{}/budgets/{}", q.tenant, q.team_quota),
            Some(q.admin.cookie()),
            Some(json!({"limit_usd": "0.04"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = dp(&q, &gw, q.key).await;
    assert_eq!(r.body["effective_remaining_usd"], json!("0.010000"));
    assert_eq!(r.body["binding_level"], json!("organization"));
    assert_eq!(r.body["org"], Value::Null);
    assert!(!scopes(&r).contains(&"team".to_string()), "{r:?}");
}

#[tokio::test]
async fn owner_admin_and_billing_keys_see_the_balance() {
    let Some(q) = setup().await else { return };
    let gw = q.cp.issue_dp_client("gw-quota-b").await;
    let admin_m = q.cp.seed_member(q.tenant, MemberRole::Admin).await;
    for holder in [q.owner.id, admin_m.id] {
        let k = Uuid::parse_str(&seed_key(&q.cp, &q.admin, q.tenant, Some(holder)).await)
            .expect("a key id");
        let r = dp(&q, &gw, k).await;
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.body["org"]["available_usd"], json!("0.970000"), "{r:?}");
        assert!(scopes(&r).contains(&"team".to_string()), "{r:?}");
    }
    sqlx::query("UPDATE members SET role = 'billing' WHERE id = $1")
        .bind(q.m.id)
        .execute(q.cp.db().pool())
        .await
        .expect("set the role");
    let r = dp(&q, &gw, q.key).await;
    assert_eq!(r.body["org"]["available_usd"], json!("0.970000"), "{r:?}");
}

#[tokio::test]
async fn an_unbound_key_sees_what_a_member_key_sees() {
    let Some(q) = setup().await else { return };
    let gw = q.cp.issue_dp_client("gw-quota-c").await;
    let k = Uuid::parse_str(&seed_key(&q.cp, &q.admin, q.tenant, None).await).expect("a key id");
    let r = dp(&q, &gw, k).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["org"], Value::Null);
    assert!(scopes(&r).is_empty(), "{r:?}");
    // Only the organization applies: 0.50 less 0.03 spent against the team.
    assert_eq!(r.body["effective_remaining_usd"], json!("0.470000"));
    assert_eq!(r.body["binding_level"], json!("organization"));
}

#[tokio::test]
async fn a_role_change_shows_at_the_next_lookup() {
    let Some(q) = setup().await else { return };
    let gw = q.cp.issue_dp_client("gw-quota-d").await;
    assert_eq!(dp(&q, &gw, q.key).await.body["org"], Value::Null);
    sqlx::query("UPDATE members SET role = 'admin' WHERE id = $1")
        .bind(q.m.id)
        .execute(q.cp.db().pool())
        .await
        .expect("promote");
    let r = dp(&q, &gw, q.key).await;
    assert_eq!(r.body["org"]["available_usd"], json!("0.970000"), "{r:?}");
}

#[tokio::test]
async fn an_unknown_or_revoked_key_is_not_found() {
    let Some(q) = setup().await else { return };
    let gw = q.cp.issue_dp_client("gw-quota-e").await;
    let unknown = dp(&q, &gw, Uuid::new_v4()).await;
    assert_eq!(unknown.status, 404, "{unknown:?}");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id = $1")
        .bind(q.key)
        .fetch_one(q.cp.db().pool())
        .await
        .expect("read the key");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a client")
        .delete(q.cp.url(&format!("/admin/keys/{}", q.key)))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, q.admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .send()
        .await
        .expect("send");
    assert!(r.status().is_success(), "{r:?}");
    let revoked = dp(&q, &gw, q.key).await;
    assert_eq!(revoked.status, 404);
    assert_eq!(revoked.body, unknown.body);
}

#[tokio::test]
async fn the_internal_tenant_has_no_organization_limit() {
    let Some(q) = setup().await else { return };
    let gw = q.cp.issue_dp_client("gw-quota-f").await;
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(q.cp.db().pool())
        .await
        .expect("the internal tenant");
    let k = Uuid::parse_str(&seed_key(&q.cp, &q.admin, internal, None).await).expect("a key id");
    let r = dp(&q, &gw, k).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["org"], Value::Null);
    assert_eq!(r.body["effective_remaining_usd"], Value::Null);
    assert_eq!(r.body["binding_level"], Value::Null);
}

#[tokio::test]
async fn a_cumulative_and_a_periodic_quota_on_one_key() {
    let Some(q) = setup().await else { return };
    let r = post(
        &q.cp,
        q.owner.cookie(),
        "/api/budgets",
        json!({"scope": "api_key", "scope_ref": q.key, "period": "day", "limit_usd": "0.10"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let r = portal(&q, &q.owner, q.key).await;
    let keys: Vec<&Value> = r.body["quotas"]
        .as_array()
        .expect("quotas")
        .iter()
        .filter(|f| f["scope"] == json!("api_key"))
        .collect();
    assert_eq!(keys.len(), 2, "{r:?}");
    let periodic = keys
        .iter()
        .find(|f| f["kind"] == json!("periodic"))
        .expect("periodic");
    assert_eq!(periodic["period"], json!("day"));
    assert!(periodic["period_start"].is_string());
    assert_eq!(periodic["counted_from"], Value::Null);
    let cumulative = keys
        .iter()
        .find(|f| f["kind"] == json!("cumulative"))
        .expect("cumulative");
    assert_eq!(cumulative["period"], Value::Null);
    assert_eq!(cumulative["period_start"], Value::Null);
    assert!(cumulative["counted_from"].is_string());
}

/// Each figure of a key's quota is the quota as `GET /api/budgets` shows it,
/// without its target: the same keys, `created_at` included, and the same
/// values.
#[tokio::test]
async fn a_key_quotas_figures_are_the_budgets_without_their_target() {
    let Some(q) = setup().await else { return };
    let figures = portal(&q, &q.owner, q.key).await;
    assert_eq!(figures.status, 200, "{figures:?}");
    let budgets =
        q.cp.send("GET", "/api/budgets", Some(q.owner.cookie()), None)
            .await;
    assert_eq!(budgets.status, 200, "{budgets:?}");
    let figures = figures.body["quotas"].as_array().expect("quotas").clone();
    assert_eq!(figures.len(), 3, "{figures:?}");
    for figure in figures {
        let mut budget = budgets.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|b| b["id"] == figure["id"])
            .unwrap_or_else(|| panic!("no budget for {figure}"))
            .clone();
        let fields = budget.as_object_mut().expect("an object");
        fields.remove("target");
        fields.remove("target_live");
        assert!(fields["created_at"].is_string(), "{budget}");
        assert_eq!(figure, budget);
    }
}

#[tokio::test]
async fn the_quota_lookup_names_no_channel() {
    let Some(q) = setup().await else { return };
    let gw = q.cp.issue_dp_client("gw-quota-g").await;
    let body = [
        dp(&q, &gw, q.key).await.body.to_string(),
        portal(&q, &q.owner, q.key).await.body.to_string(),
    ];
    for body in body {
        for token in [
            "provider",
            "upstream",
            "group",
            "cost",
            "error_code",
            "model",
        ] {
            assert!(!body.contains(token), "{token} in {body}");
        }
    }
}

#[tokio::test]
async fn the_quota_lookup_is_served_only_on_the_gateway_listener() {
    let Some(q) = setup().await else { return };
    let r =
        q.cp.send(
            "GET",
            &format!("/dp/v1/keys/{}/quota", q.key),
            Some(q.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    // Without a client certificate the gateway listener refuses.
    let root = q.cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = q.cp.issue_bundle(&root, "gw-quota-h").await;
    let anonymous = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_pem(bundle.ca_pem.as_bytes()).expect("CA"))
        .http1_only()
        .resolve("localhost", q.cp.dp_addr())
        .build()
        .expect("a client")
        .get(format!(
            "https://localhost:{}/dp/v1/keys/{}/quota",
            q.cp.dp_addr().port(),
            q.key
        ))
        .send()
        .await;
    assert!(
        anonymous.map_or(true, |r| !r.status().is_success()),
        "an anonymous client was answered"
    );
}

#[tokio::test]
async fn the_portal_quota_route_follows_the_portal_roles() {
    let Some(q) = setup().await else { return };
    let r = portal(&q, &q.m, q.key).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["org"], Value::Null);
    let owner_key =
        Uuid::parse_str(&seed_key(&q.cp, &q.admin, q.tenant, Some(q.owner.id)).await).expect("id");
    assert_eq!(portal(&q, &q.m, owner_key).await.status, 404);
    let billing =
        q.cp.member_login_seeded(q.tenant, MemberRole::Billing)
            .await;
    let r = portal(&q, &billing, q.key).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body["org"].is_object());
    // Another tenant's key reads like a random id.
    let other = seed_tenant(&q.cp, &q.admin).await;
    let theirs = Uuid::parse_str(&seed_key(&q.cp, &q.admin, other, None).await).expect("id");
    assert_eq!(portal(&q, &q.owner, theirs).await.status, 404);
}
