//! An organization's quota, set by operators through
//! `/admin/tenants/{id}/budgets`: operators read every level of a tenant's
//! quotas and write only their own (the `team` level). Another level's quota
//! answers `403 budget_managed`, another tenant's `404`.

mod support;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::billing::budgets::{self, BudgetRow, BudgetScope, Kind, NewBudget, SetBy};
use aisix_control_plane::resources::write::WriteCtx;
use aisix_control_plane::tenants;
use aisix_core::models::BudgetPeriod;
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{seed_tenant, world, World};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

async fn send(
    cp: &TestCp,
    op: &OperatorSession,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Resp {
    cp.send(method, path, Some(op.cookie()), body).await
}

async fn post(w: &World, tenant: Uuid, body: Value) -> Resp {
    send(
        &w.cp,
        &w.admin,
        "POST",
        &format!("/admin/tenants/{tenant}/budgets"),
        Some(body),
    )
    .await
}

async fn get(w: &World, tenant: Uuid) -> Resp {
    send(
        &w.cp,
        &w.admin,
        "GET",
        &format!("/admin/tenants/{tenant}/budgets"),
        None,
    )
    .await
}

/// A quota of 5 $ at another level, through the domain write.
async fn other_level(
    cp: &TestCp,
    tenant: Uuid,
    scope: BudgetScope,
    scope_ref: Uuid,
    set_by: SetBy,
) -> BudgetRow {
    let ctx = WriteCtx::of(cp.state());
    let new = NewBudget {
        scope,
        scope_ref,
        set_by,
        kind: Kind::Periodic,
        period: Some(BudgetPeriod::Month),
        limit_micro_cny: 5_000_000,
        created_by_kind: "system",
        created_by: None,
    };
    cp.db()
        .tx(move |conn| {
            async move {
                let (row, _) = tenants::lock_billing(&mut *conn, tenant).await?;
                budgets::create(conn, &ctx, &row, new, Actor::System).await
            }
            .boxed()
        })
        .await
        .expect("create the quota")
}

async fn audits(cp: &TestCp, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1")
        .bind(action)
        .fetch_one(cp.db().pool())
        .await
        .expect("count audit rows")
}

async fn live_docs(cp: &TestCp, id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources WHERE kind = 'budgets' AND id = $1::uuid AND NOT deleted",
    )
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("count documents")
}

fn detail_path(r: &Resp) -> &Value {
    &r.body["details"][0]["path"]
}

#[tokio::test]
async fn an_operator_sets_the_organization_quota() {
    let Some(w) = world().await else { return };
    let name: String = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(w.tenant)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("the tenant's name");
    let r = post(&w, w.tenant, json!({"limit_cny": "0.01"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let b = &r.body;
    assert_eq!(b["scope"], json!("team"));
    assert_eq!(b["set_by"], json!("operator"));
    assert_eq!(b["kind"], json!("periodic"));
    assert_eq!(b["period"], json!("month"));
    assert_eq!(b["limit_micro_cny"], json!(10_000));
    assert_eq!(b["spent_micro_cny"], json!(0));
    assert_eq!(b["target"], json!({"id": w.tenant, "label": name}));
    assert_eq!(b["target_live"], json!(true));
    assert_eq!(audits(&w.cp, "budget.create").await, 1);
    // The gateway's document stands before the answer.
    assert_eq!(live_docs(&w.cp, b["id"].as_str().expect("an id")).await, 1);

    // A cumulative quota takes no period.
    let r = post(
        &w,
        w.tenant,
        json!({"kind": "cumulative", "limit_cny": "3"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        (&r.body["kind"], &r.body["period"]),
        (&json!("cumulative"), &Value::Null)
    );
    assert!(r.body["counted_from"].is_string(), "{r:?}");
}

/// A quota's limit is typed in yuan and fen, up to ¥99,999,999.99: an amount
/// with three decimals, or one past the cap, is refused at `/limit_cny`.
#[tokio::test]
async fn a_quota_limit_is_yuan_and_fen() {
    let Some(w) = world().await else { return };
    for (body, path) in [
        (json!({"limit_cny": "1.001"}), "/limit_cny"),
        (json!({"limit_cny": "100000000"}), "/limit_cny"),
        (json!({"limit_cny": "0"}), "/limit_cny"),
        (json!({"limit_cny": "-1"}), "/limit_cny"),
    ] {
        let r = post(&w, w.tenant, body.clone()).await;
        assert_eq!(
            (r.status, detail_path(&r)),
            (422, &json!(path)),
            "{body}: {r:?}"
        );
    }
    // A dollar limit is no field of this route's body: refused as a body the
    // interface does not define, which is a `422` and writes nothing. The
    // tenant takes its one monthly quota below, so this is asked first.
    let r = post(&w, w.tenant, json!({"limit_usd": "1"})).await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(audits(&w.cp, "budget.create").await, 0);

    let r = post(&w, w.tenant, json!({"limit_cny": "99999999.99"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["limit_micro_cny"], json!(99_999_999_990_000_i64));
    assert_eq!(r.body["spent_micro_cny"], json!(0));

    // A limit with one decimal is the same fen amount.
    let r = post(
        &w,
        w.tenant,
        json!({"kind": "cumulative", "limit_cny": "2.5"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["limit_micro_cny"], json!(2_500_000));
}

#[tokio::test]
async fn a_quota_the_domain_refuses_answers_at_a_path_of_this_route() {
    let Some(w) = world().await else { return };
    for (body, path) in [
        (json!({"limit_cny": "0"}), "/limit_cny"),
        (json!({"limit_cny": "0.001"}), "/limit_cny"),
        (json!({"limit_cny": "1", "kind": "yearly"}), "/kind"),
        (json!({"limit_cny": "1", "period": "year"}), "/period"),
        (
            json!({"limit_cny": "1", "kind": "cumulative", "period": "day"}),
            "/period",
        ),
    ] {
        let r = post(&w, w.tenant, body.clone()).await;
        assert_eq!(
            (r.status, detail_path(&r)),
            (422, &json!(path)),
            "{body}: {r:?}"
        );
    }
    assert_eq!(
        post(&w, w.tenant, json!({"limit_cny": "1", "extra": 1}))
            .await
            .status,
        422
    );
    assert_eq!(post(&w, w.tenant, json!({})).await.status, 422);

    let first = post(&w, w.tenant, json!({"limit_cny": "1", "period": "day"})).await;
    assert_eq!(first.status, 201, "{first:?}");
    let again = post(&w, w.tenant, json!({"limit_cny": "2", "period": "day"})).await;
    assert_eq!(
        (again.status, &again.body["code"]),
        (409, &json!("budget_exists")),
        "{again:?}"
    );

    // The internal tenant takes no quota.
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("the internal tenant");
    let r = post(&w, internal, json!({"limit_cny": "1"})).await;
    assert_eq!(
        (r.status, detail_path(&r)),
        (422, &json!("/tenant_id")),
        "{r:?}"
    );

    let r = post(&w, Uuid::new_v4(), json!({"limit_cny": "1"})).await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(audits(&w.cp, "budget.create").await, 1);
}

#[tokio::test]
async fn an_operator_reads_every_level() {
    let Some(w) = world().await else { return };
    let key = Uuid::parse_str(&w.key).expect("a key id");
    let team = post(&w, w.tenant, json!({"limit_cny": "9"})).await;
    assert_eq!(team.status, 201, "{team:?}");
    let member = other_level(
        &w.cp,
        w.tenant,
        BudgetScope::Member,
        w.member,
        SetBy::OrgAdmin,
    )
    .await;
    let cap = other_level(&w.cp, w.tenant, BudgetScope::ApiKey, key, SetBy::Member).await;
    // Another tenant's quota is not in the list.
    let other = seed_tenant(&w.cp, &w.admin).await;
    post(&w, other, json!({"limit_cny": "1"})).await;

    let r = get(&w, w.tenant).await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    let level = |id: Uuid| items.iter().find(|b| b["id"] == json!(id)).cloned();
    assert_eq!(items.len(), 3, "{r:?}");
    let m = level(member.id).expect("the member quota");
    assert_eq!(
        (&m["scope"], &m["set_by"]),
        (&json!("member"), &json!("org_admin"))
    );
    assert_eq!(m["target"]["id"], json!(w.member));
    assert_eq!(m["target_live"], json!(true));
    assert!(
        m["target"]["label"].as_str().is_some_and(|l| !l.is_empty()),
        "{m}"
    );
    let c = level(cap.id).expect("the key cap");
    assert_eq!(
        (&c["scope"], &c["set_by"]),
        (&json!("api_key"), &json!("member"))
    );
    assert_eq!(c["target"], json!({"id": key, "label": "pricing-7c1"}));
    assert_eq!(
        level(Uuid::parse_str(team.body["id"].as_str().expect("an id")).expect("a uuid"))
            .expect("the organization quota")["set_by"],
        json!("operator")
    );

    assert_eq!(get(&w, Uuid::new_v4()).await.status, 404);
}

#[tokio::test]
async fn an_operator_edits_only_operator_rows() {
    let Some(w) = world().await else { return };
    let key = Uuid::parse_str(&w.key).expect("a key id");
    let mine = post(&w, w.tenant, json!({"limit_cny": "1"})).await.body["id"]
        .as_str()
        .expect("an id")
        .to_string();
    let member = other_level(
        &w.cp,
        w.tenant,
        BudgetScope::Member,
        w.member,
        SetBy::OrgAdmin,
    )
    .await;
    let cap = other_level(&w.cp, w.tenant, BudgetScope::ApiKey, key, SetBy::Member).await;
    let path = |id: &str| format!("/admin/tenants/{}/budgets/{id}", w.tenant);

    for row in [&member, &cap] {
        let p = path(&row.id.to_string());
        let r = send(
            &w.cp,
            &w.admin,
            "PATCH",
            &p,
            Some(json!({"limit_cny": "7"})),
        )
        .await;
        assert_eq!(
            (r.status, &r.body["code"]),
            (403, &json!("budget_managed")),
            "{r:?}"
        );
        let r = send(&w.cp, &w.admin, "DELETE", &p, None).await;
        assert_eq!(
            (r.status, &r.body["code"]),
            (403, &json!("budget_managed")),
            "{r:?}"
        );
        assert_eq!(live_docs(&w.cp, &row.id.to_string()).await, 1);
    }
    assert_eq!(audits(&w.cp, "budget.update").await, 0);
    assert_eq!(audits(&w.cp, "budget.delete").await, 0);

    // The rows are untouched.
    let items = get(&w, w.tenant).await.body["items"].clone();
    let limit = |id: Uuid| {
        items
            .as_array()
            .and_then(|a| a.iter().find(|b| b["id"] == json!(id)))
            .map(|b| b["limit_micro_cny"].clone())
    };
    assert_eq!(limit(member.id), Some(json!(5_000_000)));
    assert_eq!(limit(cap.id), Some(json!(5_000_000)));

    // Another tenant's quota is a 404 through this tenant's path.
    let other = seed_tenant(&w.cp, &w.admin).await;
    let theirs = post(&w, other, json!({"limit_cny": "1"})).await.body["id"]
        .as_str()
        .expect("an id")
        .to_string();
    for id in [theirs.clone(), Uuid::new_v4().to_string()] {
        let r = send(
            &w.cp,
            &w.admin,
            "PATCH",
            &path(&id),
            Some(json!({"limit_cny": "2"})),
        )
        .await;
        assert_eq!(r.status, 404, "{r:?}");
        let r = send(&w.cp, &w.admin, "DELETE", &path(&id), None).await;
        assert_eq!(r.status, 404, "{r:?}");
    }
    assert_eq!(live_docs(&w.cp, &theirs).await, 1);

    // Its own row: edited, then deleted.
    let r = send(
        &w.cp,
        &w.admin,
        "PATCH",
        &path(&mine),
        Some(json!({"limit_cny": "2.50"})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["limit_micro_cny"], json!(2_500_000));
    assert_eq!(audits(&w.cp, "budget.update").await, 1);
    let r = send(
        &w.cp,
        &w.admin,
        "PATCH",
        &path(&mine),
        Some(json!({"limit_cny": "0"})),
    )
    .await;
    assert_eq!(
        (r.status, detail_path(&r)),
        (422, &json!("/limit_cny")),
        "{r:?}"
    );
    let r = send(&w.cp, &w.admin, "DELETE", &path(&mine), None).await;
    assert_eq!(r.status, 204, "{r:?}");
    assert_eq!(audits(&w.cp, "budget.delete").await, 1);
    assert_eq!(live_docs(&w.cp, &mine).await, 0);
    let r = send(&w.cp, &w.admin, "DELETE", &path(&mine), None).await;
    assert_eq!(r.status, 404, "{r:?}");
}

#[tokio::test]
async fn who_may_call_the_operator_quota_routes() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let operator = w.cp.admin_login_seeded(OperatorRole::Operator).await;
    let url = format!("/admin/tenants/{}/budgets", w.tenant);
    let body = json!({"limit_cny": "0.01"});

    let r = send(&w.cp, &viewer, "GET", &url, None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = send(&w.cp, &viewer, "POST", &url, Some(body.clone())).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );
    assert_eq!(audits(&w.cp, "budget.create").await, 0);
    let r = w.cp.send("GET", &url, None, None).await;
    assert_eq!(r.status, 401, "{r:?}");
    // A member of the tenant has no operator session.
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let r =
        w.cp.send("POST", &url, Some(owner.cookie()), Some(body.clone()))
            .await;
    assert_eq!(r.status, 401, "{r:?}");

    let r = send(&w.cp, &operator, "POST", &url, Some(body)).await;
    assert_eq!(r.status, 201, "{r:?}");
    let id = r.body["id"].as_str().expect("an id");
    let one = format!("{url}/{id}");
    let r = send(
        &w.cp,
        &viewer,
        "PATCH",
        &one,
        Some(json!({"limit_cny": "1"})),
    )
    .await;
    assert_eq!(r.status, 403, "{r:?}");
    let r = send(&w.cp, &viewer, "DELETE", &one, None).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(live_docs(&w.cp, id).await, 1);
}

// The tenant's own levels, through `/api/budgets`.

use support::members::MemberSession;

async fn tsend(
    cp: &TestCp,
    who: &MemberSession,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Resp {
    cp.send(method, path, Some(who.cookie()), body).await
}

fn quota(scope: &str, target: impl std::fmt::Display, limit: &str) -> Value {
    json!({"scope": scope, "scope_ref": target.to_string(), "limit_cny": limit})
}

/// A tenant with an owner O, a member M, a key of each and the sessions.
struct Tenant {
    owner: MemberSession,
    m: MemberSession,
    k_owner: Uuid,
    k_m: Uuid,
}

async fn tenant(w: &World) -> Tenant {
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let m = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let key = |member: Uuid| async move {
        Uuid::parse_str(&support::world::seed_key(&w.cp, &w.admin, w.tenant, Some(member)).await)
            .expect("a key id")
    };
    Tenant {
        k_owner: key(owner.id).await,
        k_m: key(m.id).await,
        owner,
        m,
    }
}

fn id(r: &Resp) -> String {
    r.body["id"].as_str().expect("an id").to_string()
}

#[tokio::test]
async fn owners_set_member_quotas_and_cap_any_key() {
    let Some(w) = world().await else { return };
    let t = tenant(&w).await;
    let r = tsend(
        &w.cp,
        &t.owner,
        "POST",
        "/api/budgets",
        Some(quota("member", t.m.id, "0.05")),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        (&r.body["scope"], &r.body["set_by"]),
        (&json!("member"), &json!("org_admin"))
    );
    assert_eq!(r.body["target"]["id"], json!(t.m.id));
    let admin = w.cp.member_login_seeded(w.tenant, MemberRole::Admin).await;
    let r = tsend(
        &w.cp,
        &admin,
        "POST",
        "/api/budgets",
        Some(quota("api_key", t.k_m, "0.05")),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        (&r.body["scope"], &r.body["set_by"]),
        (&json!("api_key"), &json!("org_admin"))
    );
    assert_eq!(audits(&w.cp, "budget.create").await, 2);
    assert_eq!(live_docs(&w.cp, &id(&r)).await, 1);
}

#[tokio::test]
async fn a_member_caps_only_its_own_keys() {
    let Some(w) = world().await else { return };
    let t = tenant(&w).await;
    let r = tsend(
        &w.cp,
        &t.m,
        "POST",
        "/api/budgets",
        Some(quota("api_key", t.k_m, "0.05")),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["set_by"], json!("member"));

    let r = tsend(
        &w.cp,
        &t.m,
        "POST",
        "/api/budgets",
        Some(quota("api_key", t.k_owner, "0.05")),
    )
    .await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );

    // Another tenant's key reads exactly as a key that does not exist.
    let other = seed_tenant(&w.cp, &w.admin).await;
    let theirs = support::world::seed_key(&w.cp, &w.admin, other, None).await;
    let a = tsend(
        &w.cp,
        &t.m,
        "POST",
        "/api/budgets",
        Some(quota("api_key", &theirs, "0.05")),
    )
    .await;
    let b = tsend(
        &w.cp,
        &t.m,
        "POST",
        "/api/budgets",
        Some(quota("api_key", Uuid::new_v4(), "0.05")),
    )
    .await;
    assert_eq!(
        (a.status, detail_path(&a)),
        (422, &json!("/scope_ref")),
        "{a:?}"
    );
    assert_eq!((a.status, &a.body), (b.status, &b.body), "{a:?} {b:?}");
    assert_eq!(audits(&w.cp, "budget.create").await, 1);
}

#[tokio::test]
async fn each_level_edits_only_its_own_rows() {
    let Some(w) = world().await else { return };
    let t = tenant(&w).await;
    let op = post(&w, w.tenant, json!({"limit_cny": "1"})).await;
    let op_path = format!("/api/budgets/{}", id(&op));
    let owners_cap =
        other_level(&w.cp, w.tenant, BudgetScope::ApiKey, t.k_m, SetBy::OrgAdmin).await;
    let cap_path = format!("/api/budgets/{}", owners_cap.id);
    let members_cap = other_level(
        &w.cp,
        w.tenant,
        BudgetScope::ApiKey,
        t.k_owner,
        SetBy::Member,
    )
    .await;
    let managed = |r: &Resp| (r.status, r.body["code"].clone());
    let want = (403, json!("budget_managed"));

    // The owner meets the operator's row.
    let r = tsend(
        &w.cp,
        &t.owner,
        "PATCH",
        &op_path,
        Some(json!({"limit_cny": "2"})),
    )
    .await;
    assert_eq!(managed(&r), want, "{r:?}");
    let r = tsend(&w.cp, &t.owner, "DELETE", &op_path, None).await;
    assert_eq!(managed(&r), want, "{r:?}");
    // The member meets the owner's cap on its own key.
    let r = tsend(
        &w.cp,
        &t.m,
        "PATCH",
        &cap_path,
        Some(json!({"limit_cny": "2"})),
    )
    .await;
    assert_eq!(managed(&r), want, "{r:?}");
    let r = tsend(&w.cp, &t.m, "DELETE", &cap_path, None).await;
    assert_eq!(managed(&r), want, "{r:?}");
    assert_eq!(live_docs(&w.cp, &owners_cap.id.to_string()).await, 1);
    // A member quota and a team quota are refused to POST.
    let r = tsend(
        &w.cp,
        &t.m,
        "POST",
        "/api/budgets",
        Some(quota("member", t.m.id, "1")),
    )
    .await;
    assert_eq!(managed(&r), want, "{r:?}");
    for who in [&t.owner, &t.m] {
        let r = tsend(
            &w.cp,
            who,
            "POST",
            "/api/budgets",
            Some(quota("team", w.tenant, "1")),
        )
        .await;
        assert_eq!(managed(&r), want, "{r:?}");
    }
    assert_eq!(audits(&w.cp, "budget.update").await, 0);
    assert_eq!(audits(&w.cp, "budget.delete").await, 0);

    // A member's row on the owner's key is not the member's to see: 404.
    let hidden = format!("/api/budgets/{}", members_cap.id);
    let r = tsend(&w.cp, &t.m, "DELETE", &hidden, None).await;
    assert_eq!(r.status, 404, "{r:?}");
    // The owner edits the member's row and the owner's own; the member its own.
    let r = tsend(
        &w.cp,
        &t.owner,
        "PATCH",
        &hidden,
        Some(json!({"limit_cny": "3"})),
    )
    .await;
    assert_eq!(
        (r.status, &r.body["limit_micro_cny"]),
        (200, &json!(3_000_000)),
        "{r:?}"
    );
    let r = tsend(
        &w.cp,
        &t.owner,
        "PATCH",
        &cap_path,
        Some(json!({"limit_cny": "4"})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let mine = tsend(
        &w.cp,
        &t.m,
        "POST",
        "/api/budgets",
        Some(quota("api_key", t.k_m, "1")),
    )
    .await;
    // Same key and kind as the owner's cap, from another setter: allowed.
    assert_eq!(mine.status, 201, "{mine:?}");
    let own = format!("/api/budgets/{}", id(&mine));
    let r = tsend(
        &w.cp,
        &t.m,
        "PATCH",
        &own,
        Some(json!({"limit_cny": "0.50"})),
    )
    .await;
    assert_eq!(
        (r.status, &r.body["limit_micro_cny"]),
        (200, &json!(500_000)),
        "{r:?}"
    );
    let r = tsend(&w.cp, &t.m, "DELETE", &own, None).await;
    assert_eq!(r.status, 204, "{r:?}");
    let r = tsend(&w.cp, &t.owner, "DELETE", &cap_path, None).await;
    assert_eq!(r.status, 204, "{r:?}");

    // Another tenant's quota is a 404.
    let other = seed_tenant(&w.cp, &w.admin).await;
    let theirs = post(&w, other, json!({"limit_cny": "1"})).await;
    for p in [
        format!("/api/budgets/{}", id(&theirs)),
        format!("/api/budgets/{}", Uuid::new_v4()),
    ] {
        for who in [&t.owner, &t.m] {
            let r = tsend(&w.cp, who, "DELETE", &p, None).await;
            assert_eq!(r.status, 404, "{r:?}");
            let r = tsend(&w.cp, who, "PATCH", &p, Some(json!({"limit_cny": "1"}))).await;
            assert_eq!(r.status, 404, "{r:?}");
        }
    }
    assert_eq!(live_docs(&w.cp, &id(&theirs)).await, 1);
}

#[tokio::test]
async fn a_member_reads_its_own_levels_only() {
    let Some(w) = world().await else { return };
    let t = tenant(&w).await;
    let team = post(&w, w.tenant, json!({"limit_cny": "9"})).await;
    let mine = other_level(
        &w.cp,
        w.tenant,
        BudgetScope::Member,
        t.m.id,
        SetBy::OrgAdmin,
    )
    .await;
    let on_key = other_level(&w.cp, w.tenant, BudgetScope::ApiKey, t.k_m, SetBy::Member).await;
    let others_member = other_level(
        &w.cp,
        w.tenant,
        BudgetScope::Member,
        t.owner.id,
        SetBy::OrgAdmin,
    )
    .await;
    let others_key = other_level(
        &w.cp,
        w.tenant,
        BudgetScope::ApiKey,
        t.k_owner,
        SetBy::OrgAdmin,
    )
    .await;
    let ids = |r: &Resp| -> Vec<String> {
        let mut v: Vec<String> = r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|b| b["id"].as_str().expect("an id").to_string())
            .collect();
        v.sort();
        v
    };
    let sorted = |rows: &[String]| {
        let mut v = rows.to_vec();
        v.sort();
        v
    };

    let r = tsend(&w.cp, &t.m, "GET", "/api/budgets", None).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        ids(&r),
        sorted(&[mine.id.to_string(), on_key.id.to_string()]),
        "{r:?}"
    );
    let everything = sorted(&[
        id(&team),
        mine.id.to_string(),
        on_key.id.to_string(),
        others_member.id.to_string(),
        others_key.id.to_string(),
    ]);
    let billing =
        w.cp.member_login_seeded(w.tenant, MemberRole::Billing)
            .await;
    for who in [&t.owner, &billing] {
        let r = tsend(&w.cp, who, "GET", "/api/budgets", None).await;
        assert_eq!(ids(&r), everything, "{r:?}");
    }
}

/// A member quota's target label names the member by its display name; its
/// email only to a reader who may list the members (an operator, an owner,
/// an admin) or to the member itself.
#[tokio::test]
async fn a_quota_label_shows_a_members_email_only_to_who_may_list_members() {
    let Some(w) = world().await else { return };
    let t = tenant(&w).await;
    let r = tsend(
        &w.cp,
        &t.owner,
        "POST",
        "/api/budgets",
        Some(quota("member", t.m.id, "0.05")),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let qid = id(&r);
    let billing =
        w.cp.member_login_seeded(w.tenant, MemberRole::Billing)
            .await;
    let label_for = |r: &Resp| -> String {
        assert_eq!(r.status, 200, "{r:?}");
        r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|b| b["id"] == json!(qid))
            .unwrap_or_else(|| panic!("the quota in {r:?}"))["target"]["label"]
            .as_str()
            .expect("a label")
            .to_string()
    };
    let short = t.m.id.simple().to_string()[..8].to_string();

    // M has no display name.
    let by_billing = label_for(&tsend(&w.cp, &billing, "GET", "/api/budgets", None).await);
    assert_eq!(by_billing, format!("成员 {short}"));
    assert!(!by_billing.contains(&t.m.email), "{by_billing}");
    for (who, r) in [
        (
            "owner",
            tsend(&w.cp, &t.owner, "GET", "/api/budgets", None).await,
        ),
        (
            "the member itself",
            tsend(&w.cp, &t.m, "GET", "/api/budgets", None).await,
        ),
        ("an operator", get(&w, w.tenant).await),
    ] {
        assert_eq!(label_for(&r), t.m.email, "{who}");
    }

    // Once M has one, its display name is shown to every reader.
    sqlx::query("UPDATE members SET display_name = 'Mia 7c1' WHERE id = $1")
        .bind(t.m.id)
        .execute(w.cp.db().pool())
        .await
        .expect("name the member");
    for r in [
        tsend(&w.cp, &billing, "GET", "/api/budgets", None).await,
        tsend(&w.cp, &t.owner, "GET", "/api/budgets", None).await,
        get(&w, w.tenant).await,
    ] {
        assert_eq!(label_for(&r), "Mia 7c1");
    }
}

#[tokio::test]
async fn a_quota_defaults_to_monthly() {
    let Some(w) = world().await else { return };
    let t = tenant(&w).await;
    let r = tsend(
        &w.cp,
        &t.owner,
        "POST",
        "/api/budgets",
        Some(quota("member", t.m.id, "1")),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        (&r.body["kind"], &r.body["period"]),
        (&json!("periodic"), &json!("month"))
    );
    let r = tsend(
        &w.cp,
        &t.owner,
        "POST",
        "/api/budgets",
        Some(json!({"scope": "member", "scope_ref": t.owner.id, "kind": "cumulative", "limit_cny": "1"})),
    )
    .await;
    assert_eq!(
        (&r.body["kind"], &r.body["period"]),
        (&json!("cumulative"), &Value::Null),
        "{r:?}"
    );
    let r = tsend(
        &w.cp,
        &t.owner,
        "POST",
        "/api/budgets",
        Some(quota("tenant", w.tenant, "1")),
    )
    .await;
    assert_eq!(
        (r.status, detail_path(&r)),
        (422, &json!("/scope")),
        "{r:?}"
    );
}

#[tokio::test]
async fn who_may_call_the_tenant_quota_routes() {
    let Some(w) = world().await else { return };
    let t = tenant(&w).await;
    let billing =
        w.cp.member_login_seeded(w.tenant, MemberRole::Billing)
            .await;
    let admin = w.cp.member_login_seeded(w.tenant, MemberRole::Admin).await;
    let r = tsend(&w.cp, &billing, "GET", "/api/budgets", None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = tsend(
        &w.cp,
        &billing,
        "POST",
        "/api/budgets",
        Some(quota("member", t.m.id, "1")),
    )
    .await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );
    let r = tsend(&w.cp, &t.m, "GET", "/api/budgets", None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = tsend(
        &w.cp,
        &admin,
        "POST",
        "/api/budgets",
        Some(quota("member", t.m.id, "1")),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let one = format!("/api/budgets/{}", id(&r));
    let r = tsend(
        &w.cp,
        &billing,
        "PATCH",
        &one,
        Some(json!({"limit_cny": "2"})),
    )
    .await;
    assert_eq!(r.status, 403, "{r:?}");
    let r = tsend(&w.cp, &billing, "DELETE", &one, None).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(
        live_docs(&w.cp, one.rsplit('/').next().expect("an id")).await,
        1
    );
    let r = w.cp.send("GET", "/api/budgets", None, None).await;
    assert_eq!(r.status, 401, "{r:?}");
}
