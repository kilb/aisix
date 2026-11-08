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
        limit_micro_usd: 5_000_000,
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
    let r = post(&w, w.tenant, json!({"limit_usd": "0.01"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let b = &r.body;
    assert_eq!(b["scope"], json!("team"));
    assert_eq!(b["set_by"], json!("operator"));
    assert_eq!(b["kind"], json!("periodic"));
    assert_eq!(b["period"], json!("month"));
    assert_eq!(b["limit_micro_usd"], json!(10_000));
    assert_eq!(b["spent_micro_usd"], json!(0));
    assert_eq!(b["target"], json!({"id": w.tenant, "label": name}));
    assert_eq!(b["target_live"], json!(true));
    assert_eq!(audits(&w.cp, "budget.create").await, 1);
    // The gateway's document stands before the answer.
    assert_eq!(live_docs(&w.cp, b["id"].as_str().expect("an id")).await, 1);

    // A cumulative quota takes no period.
    let r = post(
        &w,
        w.tenant,
        json!({"kind": "cumulative", "limit_usd": "3"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        (&r.body["kind"], &r.body["period"]),
        (&json!("cumulative"), &Value::Null)
    );
    assert!(r.body["counted_from"].is_string(), "{r:?}");
}

#[tokio::test]
async fn a_quota_the_domain_refuses_answers_at_a_path_of_this_route() {
    let Some(w) = world().await else { return };
    for (body, path) in [
        (json!({"limit_usd": "0"}), "/limit_usd"),
        (json!({"limit_usd": "0.001"}), "/limit_usd"),
        (json!({"limit_usd": "1", "kind": "yearly"}), "/kind"),
        (json!({"limit_usd": "1", "period": "year"}), "/period"),
        (
            json!({"limit_usd": "1", "kind": "cumulative", "period": "day"}),
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
        post(&w, w.tenant, json!({"limit_usd": "1", "extra": 1}))
            .await
            .status,
        422
    );
    assert_eq!(post(&w, w.tenant, json!({})).await.status, 422);

    let first = post(&w, w.tenant, json!({"limit_usd": "1", "period": "day"})).await;
    assert_eq!(first.status, 201, "{first:?}");
    let again = post(&w, w.tenant, json!({"limit_usd": "2", "period": "day"})).await;
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
    let r = post(&w, internal, json!({"limit_usd": "1"})).await;
    assert_eq!(
        (r.status, detail_path(&r)),
        (422, &json!("/tenant_id")),
        "{r:?}"
    );

    let r = post(&w, Uuid::new_v4(), json!({"limit_usd": "1"})).await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(audits(&w.cp, "budget.create").await, 1);
}

#[tokio::test]
async fn an_operator_reads_every_level() {
    let Some(w) = world().await else { return };
    let key = Uuid::parse_str(&w.key).expect("a key id");
    let team = post(&w, w.tenant, json!({"limit_usd": "9"})).await;
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
    post(&w, other, json!({"limit_usd": "1"})).await;

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
    let mine = post(&w, w.tenant, json!({"limit_usd": "1"})).await.body["id"]
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
            Some(json!({"limit_usd": "7"})),
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
            .map(|b| b["limit_micro_usd"].clone())
    };
    assert_eq!(limit(member.id), Some(json!(5_000_000)));
    assert_eq!(limit(cap.id), Some(json!(5_000_000)));

    // Another tenant's quota is a 404 through this tenant's path.
    let other = seed_tenant(&w.cp, &w.admin).await;
    let theirs = post(&w, other, json!({"limit_usd": "1"})).await.body["id"]
        .as_str()
        .expect("an id")
        .to_string();
    for id in [theirs.clone(), Uuid::new_v4().to_string()] {
        let r = send(
            &w.cp,
            &w.admin,
            "PATCH",
            &path(&id),
            Some(json!({"limit_usd": "2"})),
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
        Some(json!({"limit_usd": "2.50"})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["limit_micro_usd"], json!(2_500_000));
    assert_eq!(audits(&w.cp, "budget.update").await, 1);
    let r = send(
        &w.cp,
        &w.admin,
        "PATCH",
        &path(&mine),
        Some(json!({"limit_usd": "0"})),
    )
    .await;
    assert_eq!(
        (r.status, detail_path(&r)),
        (422, &json!("/limit_usd")),
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
    let body = json!({"limit_usd": "0.01"});

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
        Some(json!({"limit_usd": "1"})),
    )
    .await;
    assert_eq!(r.status, 403, "{r:?}");
    let r = send(&w.cp, &viewer, "DELETE", &one, None).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(live_docs(&w.cp, id).await, 1);
}
