//! The role matrix: every route in `ROUTE_ROLES` that needs an operator
//! refuses an unauthenticated request with 401 and the role just below its
//! minimum with 403, and lets its minimum role through; every route that
//! needs a member does the same for the tenant roles its row does not name.
//!
//! Each route needs a fixture here: a request that its minimum role may
//! send. A route added without one fails this test, which is the point.
//!
//! A refused request must also have changed nothing. Every change writes an
//! audit row in its own transaction, so an unchanged `audit_log` count
//! stands for "nothing happened".

mod support;

use std::collections::HashMap;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::http::{MinRole, ROUTE_ROLES};
use aisix_control_plane::tenants::TenantStatus;
use serde_json::{json, Value};
use support::members::tenant_fixture;
use support::{SeededOperator, TestCp, TEST_PASSWORD};
use uuid::Uuid;

/// The request `method path` is tested with: the concrete path and the
/// body. Called once per attempt, so a fixture that creates something
/// creates something new each time.
fn fixture(method: &str, path: &str, target: &SeededOperator) -> (String, Option<Value>) {
    match (method, path) {
        // Rejected as too short, after the role check.
        ("POST", "/admin/password") => (
            path.to_string(),
            Some(json!({"current_password": TEST_PASSWORD, "new_password": "short"})),
        ),
        ("GET", "/admin/operators") | ("GET", "/admin/audit") => (path.to_string(), None),
        ("POST", "/admin/operators") => (
            path.to_string(),
            Some(json!({
                "email": format!("roles-{}@ops.test", Uuid::new_v4().simple()),
                "role": "viewer",
                "password": "roles-password-long",
            })),
        ),
        ("PATCH", "/admin/operators/{id}") => (
            format!("/admin/operators/{}", target.id),
            Some(json!({"display_name": "roles-target"})),
        ),
        ("GET", "/admin/environment")
        | ("GET", "/admin/gateways/credentials")
        | ("GET", "/admin/gateways/instances") => (path.to_string(), None),
        ("POST", "/admin/gateways/credentials") => {
            (path.to_string(), Some(json!({"name": "roles-gateway"})))
        }
        // No such credential: 404 once the role check has passed.
        ("POST", "/admin/gateways/credentials/{dp_id}/revoke") => (
            format!("/admin/gateways/credentials/{}/revoke", Uuid::new_v4()),
            None,
        ),
        // Gateway resources. The single-resource routes name an id that
        // matches nothing: past the role check, a read answers 404 and a
        // write without If-Match 428, so nothing is written.
        ("GET", "/admin/schemas/{resource}") => ("/admin/schemas/model".to_string(), None),
        ("GET", "/admin/resources/{kind}") => ("/admin/resources/provider_keys".to_string(), None),
        ("POST", "/admin/resources/{kind}") => (
            "/admin/resources/provider_keys".to_string(),
            Some(json!({"doc": {
                "display_name": format!("roles-{}", Uuid::new_v4().simple()),
                "provider": "openai",
                "api_key": "sk-roles-7c1",
            }})),
        ),
        ("GET" | "PUT" | "DELETE", "/admin/resources/{kind}/{id}") => (
            format!("/admin/resources/provider_keys/{}", Uuid::nil()),
            (method == "PUT").then(|| json!({"doc": {}})),
        ),
        // Tenants. The single-tenant routes name an id that matches nothing:
        // past the role check, a read or an action answers 404 and a PATCH
        // without If-Match 428, so nothing is written.
        ("GET", "/admin/tenants")
        | ("GET", "/admin/settings/tenant-defaults")
        | ("GET", "/admin/provider-groups") => (path.to_string(), None),
        // Groups as data. The id matches nothing: past the role check, a read
        // answers 404 and a write 404 too, so nothing is written.
        ("POST", "/admin/provider-groups") => (
            path.to_string(),
            Some(json!({
                "name": format!("roles-{}", Uuid::new_v4().simple()),
                "display_name": "roles-group",
                "default_cny_per_usd": "7.2",
            })),
        ),
        ("PATCH", "/admin/provider-groups/{id}") => (
            format!("/admin/provider-groups/{}", Uuid::nil()),
            Some(json!({"display_name": "roles-group"})),
        ),
        ("POST", "/admin/provider-groups/{id}/retire")
        | ("POST", "/admin/provider-groups/{id}/make-default") => (
            path.replace("{id}", &Uuid::nil().to_string()),
            Some(json!({})),
        ),
        ("GET", "/admin/provider-groups/{id}/rates") => (
            format!("/admin/provider-groups/{}/rates", Uuid::nil()),
            None,
        ),
        ("POST", "/admin/provider-groups/{id}/rates") => (
            format!("/admin/provider-groups/{}/rates", Uuid::nil()),
            Some(json!({"model_id": null, "cny_per_usd": "7.2", "effective_from": chrono::Utc::now()})),
        ),
        ("POST", "/admin/tenants") => (path.to_string(), Some(json!({"name": "roles-tenant"}))),
        ("GET", "/admin/tenants/{id}") => (format!("/admin/tenants/{}", Uuid::nil()), None),
        ("PATCH", "/admin/tenants/{id}") => (
            format!("/admin/tenants/{}", Uuid::nil()),
            Some(json!({"name": "roles-tenant"})),
        ),
        ("POST", "/admin/tenants/{id}/approve")
        | ("POST", "/admin/tenants/{id}/suspend")
        | ("POST", "/admin/tenants/{id}/reactivate") => {
            (path.replace("{id}", &Uuid::nil().to_string()), None)
        }
        ("PUT", "/admin/settings/tenant-defaults") => (
            path.to_string(),
            Some(json!({"concurrency": 20, "default_provider_groups": ["shared"]})),
        ),
        // Members. The ids match nothing: 404 once the role check passed.
        ("GET", "/admin/tenants/{id}/members") => {
            (format!("/admin/tenants/{}/members", Uuid::nil()), None)
        }
        ("POST", "/admin/tenants/{id}/invites") => (
            format!("/admin/tenants/{}/invites", Uuid::nil()),
            Some(json!({"email": "roles-invite@tenant.test", "role": "member"})),
        ),
        ("PATCH", "/admin/members/{id}") => (
            format!("/admin/members/{}", Uuid::nil()),
            Some(json!({"disabled": true})),
        ),
        // Keys. The ids match nothing: past the role check, a read or a
        // create answers 404, and a change without If-Match 428.
        ("GET" | "POST", "/admin/tenants/{id}/keys") => (
            format!("/admin/tenants/{}/keys", Uuid::nil()),
            (method == "POST").then(|| json!({"label": "roles-key"})),
        ),
        ("GET" | "PATCH" | "DELETE", "/admin/keys/{id}") => (
            format!("/admin/keys/{}", Uuid::nil()),
            (method == "PATCH").then(|| json!({"label": "roles-key"})),
        ),
        ("GET", "/admin/rollout") => (path.to_string(), None),
        // This control plane reaches no kine, so no projector leads: past
        // the role check, the request is refused and nothing is deleted.
        ("POST", "/admin/rollout/reconcile") => {
            (path.to_string(), Some(json!({"allow_deletes": false})))
        }
        // Past the role check, kine cannot be reached: nothing moves.
        ("POST", "/admin/rollout/ownership") => (
            path.to_string(),
            Some(json!({"take_over_from": Uuid::nil()})),
        ),
        // Stored usage.
        ("GET", "/admin/usage/events")
        | ("GET", "/admin/usage/batches")
        | ("GET", "/admin/usage/rejects") => (path.to_string(), None),
        // Prices and charges. Each write is refused after the role check
        // (no such model or tenant, a card without a default tier), so
        // nothing is written.
        ("GET", "/admin/pricing/catalogue")
        | ("GET", "/admin/pricing/sell")
        | ("GET", "/admin/pricing/cost")
        | ("GET", "/admin/pricing/discounts")
        | ("GET", "/admin/pricing/charges")
        | ("GET", "/admin/pricing/status") => (path.to_string(), None),
        ("POST", "/admin/pricing/sell") => (
            path.to_string(),
            Some(json!({
                "model_id": Uuid::nil(),
                "effective_from": chrono::Utc::now(),
                "card": {"default": {"base": {"input": "1", "output": "2"}}},
            })),
        ),
        ("POST", "/admin/pricing/cost") => (
            path.to_string(),
            Some(json!({
                "upstream_provider": "openai",
                "upstream_model": "gpt-4o-mini",
                "effective_from": chrono::Utc::now(),
                "card": {"flex": {"base": {"input": "1", "output": "2"}}},
            })),
        ),
        ("POST", "/admin/pricing/discounts") => (
            path.to_string(),
            Some(json!({
                "tenant_id": Uuid::nil(),
                "effective_from": chrono::Utc::now(),
                "discount_bps": 500,
            })),
        ),
        // Price views. The tenant matches nothing: 404 once the role check
        // passed.
        ("GET", "/admin/pricing/models") => (path.to_string(), None),
        ("GET", "/admin/pricing/tenants/{id}") => {
            (format!("/admin/pricing/tenants/{}", Uuid::nil()), None)
        }
        // Billing accounts. The tenant matches nothing: 404 once the role
        // check passed, so no limit is written.
        ("GET", "/admin/billing/accounts") => (path.to_string(), None),
        ("GET" | "PATCH", "/admin/tenants/{id}/billing") => (
            format!("/admin/tenants/{}/billing", Uuid::nil()),
            (method == "PATCH").then(|| json!({"credit_limit_usd": "1.00"})),
        ),
        // The ledger and its totals. The tenant matches nothing: 404 once
        // the role check passed.
        ("GET", "/admin/tenants/{id}/billing/ledger") => (
            format!("/admin/tenants/{}/billing/ledger", Uuid::nil()),
            None,
        ),
        ("GET", "/admin/tenants/{id}/billing/summary") => (
            format!(
                "/admin/tenants/{}/billing/summary?group=day\
                 &from=2026-01-01T00:00:00Z&to=2026-01-01T01:00:00Z",
                Uuid::nil()
            ),
            None,
        ),
        // The tenant matches nothing: 404 once the role check passed.
        ("GET", "/admin/tenants/{id}/usage/summary") => (
            format!(
                "/admin/tenants/{}/usage/summary?group=day&from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z",
                Uuid::nil()
            ),
            None,
        ),
        // Top-ups. The top-up matches nothing: 404 once the role check
        // passed, so nothing is decided.
        ("GET", "/admin/billing/topups") => (path.to_string(), None),
        ("POST", "/admin/billing/topups/{id}/approve") => (
            format!("/admin/billing/topups/{}/approve", Uuid::nil()),
            Some(json!({})),
        ),
        ("POST", "/admin/billing/topups/{id}/reject") => (
            format!("/admin/billing/topups/{}/reject", Uuid::nil()),
            Some(json!({"reason": "roles"})),
        ),
        // Operator credits. The tenant matches nothing: 404 once the role
        // check passed, so nothing is credited.
        ("POST", "/admin/tenants/{id}/credits") => (
            format!("/admin/tenants/{}/credits", Uuid::nil()),
            Some(json!({"amount_usd": "0.05", "reason": "roles"})),
        ),
        // Organization quotas. The tenant matches nothing: 404 once the role
        // check passed, so no quota is written.
        ("GET" | "POST", "/admin/tenants/{id}/budgets") => (
            format!("/admin/tenants/{}/budgets", Uuid::nil()),
            (method == "POST").then(|| json!({"limit_usd": "1.00"})),
        ),
        ("PATCH" | "DELETE", "/admin/tenants/{id}/budgets/{bid}") => (
            format!(
                "/admin/tenants/{}/budgets/{}",
                Uuid::nil(),
                Uuid::nil()
            ),
            (method == "PATCH").then(|| json!({"limit_usd": "1.00"})),
        ),
        // Re-pricing runs. The preview selects nothing (a past window with no
        // usage) and posts nothing; the run matches nothing: 404 once the
        // role check passed, so nothing is applied.
        ("GET", "/admin/billing/repricing") => (path.to_string(), None),
        ("POST", "/admin/billing/repricing") => (
            path.to_string(),
            Some(json!({
                "from": "2026-01-01T00:00:00Z",
                "to": "2026-01-02T00:00:00Z",
                "reason": "roles",
            })),
        ),
        ("GET", "/admin/billing/repricing/{id}") => (
            format!("/admin/billing/repricing/{}", Uuid::nil()),
            None,
        ),
        ("POST", "/admin/billing/repricing/{id}/apply") => (
            format!("/admin/billing/repricing/{}/apply", Uuid::nil()),
            Some(json!({})),
        ),
        // Reconciliation reads and changes nothing.
        ("GET", "/admin/billing/reconciliation") => (path.to_string(), None),
        // Baselines. The model matches nothing: 422 once the role check
        // passed, so nothing is written.
        ("GET", "/admin/pricing/baselines") => (path.to_string(), None),
        ("POST", "/admin/pricing/baselines") => (
            path.to_string(),
            Some(json!({
                "model_id": Uuid::nil(),
                "effective_from": chrono::Utc::now(),
                "card": {"default": {"base": {"input": "1", "output": "2"}}},
                "note": "roles",
            })),
        ),
        // The price import. The upload is refused at `/dataset_ref`, so
        // nothing is written; the batch matches nothing: 404.
        ("GET", "/admin/pricing/imports") => (path.to_string(), None),
        ("POST", "/admin/pricing/imports") => (
            path.to_string(),
            Some(json!({"dataset_ref": "not a ref", "data": []})),
        ),
        ("GET", "/admin/pricing/imports/{id}") => {
            (format!("/admin/pricing/imports/{}", Uuid::nil()), None)
        }
        // An upstream account's own model list. The account matches nothing:
        // past the role check it answers 404 and never dials an upstream.
        ("GET", "/admin/provider-keys/{id}/upstream-models") => (
            format!("/admin/provider-keys/{}/upstream-models", Uuid::nil()),
            None,
        ),
        // Deciding a proposal. Both ids match nothing: past the role check,
        // 404, so nothing is written.
        ("POST", "/admin/pricing/imports/{id}/proposals/{pid}/confirm") => (
            format!(
                "/admin/pricing/imports/{}/proposals/{}/confirm",
                Uuid::nil(),
                Uuid::nil()
            ),
            Some(json!({
                "model_id": Uuid::nil(),
                "effective_from": chrono::Utc::now(),
                "attested": true,
            })),
        ),
        ("POST", "/admin/pricing/imports/{id}/proposals/{pid}/reject") => (
            format!(
                "/admin/pricing/imports/{}/proposals/{}/reject",
                Uuid::nil(),
                Uuid::nil()
            ),
            Some(json!({"reason": "roles"})),
        ),
        _ => panic!("no fixture for {method} {path}: add one to tests/roles.rs"),
    }
}

async fn audit_rows(cp: &TestCp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log")
        .fetch_one(cp.db().pool())
        .await
        .expect("count audit rows")
}

fn below(role: OperatorRole) -> Option<OperatorRole> {
    match role {
        OperatorRole::Viewer => None,
        OperatorRole::Operator => Some(OperatorRole::Viewer),
        OperatorRole::Admin => Some(OperatorRole::Operator),
    }
}

#[tokio::test]
async fn every_operator_route_refuses_the_role_below_its_minimum() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let target = cp.seed_operator(OperatorRole::Viewer).await;
    let mut sessions = HashMap::new();
    for role in [
        OperatorRole::Viewer,
        OperatorRole::Operator,
        OperatorRole::Admin,
    ] {
        sessions.insert(role, cp.admin_login_seeded(role).await);
    }

    let mut checked = 0;
    for (method, path, min) in ROUTE_ROLES {
        let MinRole::Op(min) = *min else {
            continue;
        };
        let before = audit_rows(&cp).await;
        let (uri, body) = fixture(method, path, &target);
        let r = cp.send(method, &uri, None, body).await;
        assert_eq!(r.status, 401, "{method} {path} without a session: {r:?}");
        assert_eq!(r.body["code"], "unauthenticated", "{method} {path}");
        assert_eq!(
            audit_rows(&cp).await,
            before,
            "{method} {path} without a session changed something"
        );

        if let Some(below) = below(min) {
            let (uri, body) = fixture(method, path, &target);
            let r = cp
                .send(method, &uri, Some(sessions[&below].cookie()), body)
                .await;
            assert_eq!(r.status, 403, "{method} {path} as {below:?}: {r:?}");
            assert_eq!(r.body["code"], "forbidden", "{method} {path}");
            assert_eq!(
                audit_rows(&cp).await,
                before,
                "{method} {path} as {below:?} changed something"
            );
        }

        // Last, since it may change something.
        let (uri, body) = fixture(method, path, &target);
        let r = cp
            .send(method, &uri, Some(sessions[&min].cookie()), body)
            .await;
        assert!(
            r.status != 401 && r.status != 403,
            "{method} {path} as its minimum {min:?}: {r:?}"
        );
        checked += 1;
    }
    assert!(checked >= 5, "only {checked} operator routes were checked");
}

/// Every tenant role.
const MEMBER_ROLES: [MemberRole; 4] = [
    MemberRole::Owner,
    MemberRole::Admin,
    MemberRole::Member,
    MemberRole::Billing,
];

/// The tenant matrix: every route in `ROUTE_ROLES` that needs a member
/// refuses an unauthenticated request with 401 and each role its row does
/// not name with 403, changing nothing, and lets each role it names
/// through. The tenant is active, so only the role decides.
#[tokio::test]
async fn every_tenant_route_refuses_the_roles_below_its_minimum() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let mut sessions = HashMap::new();
    for role in MEMBER_ROLES {
        sessions.insert(role, cp.member_login_seeded(tenant, role).await);
    }

    let mut checked = 0;
    for (method, path, min) in ROUTE_ROLES {
        let MinRole::Member(allowed) = *min else {
            continue;
        };
        let before = audit_rows(&cp).await;
        let (uri, body) = tenant_fixture(method, path);
        let r = cp.send(method, &uri, None, body).await;
        assert_eq!(r.status, 401, "{method} {path} without a session: {r:?}");
        assert_eq!(r.body["code"], "unauthenticated", "{method} {path}");

        for role in MEMBER_ROLES.iter().filter(|r| !allowed.contains(r)) {
            let (uri, body) = tenant_fixture(method, path);
            let r = cp
                .send(method, &uri, Some(sessions[role].cookie()), body)
                .await;
            assert_eq!(r.status, 403, "{method} {path} as {role:?}: {r:?}");
            assert_eq!(r.body["code"], "forbidden", "{method} {path}");
        }
        assert_eq!(
            audit_rows(&cp).await,
            before,
            "{method} {path} refused, but changed something"
        );

        // Last, since they may change something.
        for role in allowed {
            let (uri, body) = tenant_fixture(method, path);
            let r = cp
                .send(method, &uri, Some(sessions[role].cookie()), body)
                .await;
            assert!(
                r.status != 401 && r.status != 403,
                "{method} {path} as {role:?}, which its row admits: {r:?}"
            );
        }
        checked += 1;
    }
    assert!(checked >= 9, "only {checked} tenant routes were checked");
}
