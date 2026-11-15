//! 密钥费用通过真实 HTTP 与 Postgres 验证；历史行只在各测试的独立数据库中准备。

mod support;

use std::collections::BTreeSet;

use aisix_control_plane::auth::MemberRole;
use aisix_control_plane::tenants::TenantStatus;
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::{json, Value};
use support::members::MemberSession;
use support::world::{seed_key, seed_tenant, world, World};
use support::Resp;
use uuid::Uuid;

fn path(from: DateTime<Utc>, to: DateTime<Utc>) -> String {
    format!(
        "/api/usage/keys?from={}&to={}",
        from.to_rfc3339_opts(SecondsFormat::Micros, true),
        to.to_rfc3339_opts(SecondsFormat::Micros, true)
    )
}

async fn get(w: &World, by: &MemberSession, uri: &str) -> Resp {
    w.cp.send("GET", uri, Some(by.cookie()), None).await
}

fn items(response: &Resp) -> &[Value] {
    assert_eq!(response.status, 200, "{response:?}");
    response.body["items"].as_array().expect("费用数组")
}

fn ids(response: &Resp) -> BTreeSet<String> {
    items(response)
        .iter()
        .map(|row| row["api_key_id"].as_str().expect("密钥编号").to_string())
        .collect()
}

fn row<'a>(response: &'a Resp, key: &str) -> &'a Value {
    items(response)
        .iter()
        .find(|row| row["api_key_id"] == key)
        .unwrap_or_else(|| panic!("缺少密钥 {key}: {response:?}"))
}

async fn charge(
    w: &World,
    id: &str,
    scope: (Uuid, &str, Uuid),
    at: DateTime<Utc>,
    status: &str,
    amount: Option<i64>,
    final_attempt: bool,
) {
    let (tenant, key, member) = scope;
    let mut tx = w.cp.db().pool().begin().await.expect("历史记录事务");
    sqlx::query("INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, event) VALUES ($1,$2,$2,$3,$4)")
        .bind(id).bind(Uuid::new_v4()).bind(at)
        .bind(json!({"is_final_attempt": final_attempt, "user_id": Uuid::new_v4(), "api_key_id": Uuid::new_v4()}))
        .execute(&mut *tx).await.expect("合法的历史遥测行");
    sqlx::query("INSERT INTO pricing_charges_cny (event_id,tenant_id,api_key_id,user_id,requested_model_id,occurred_at,status,charge_micro_cny,engine_version) VALUES ($1,$2,$3,$4,'',$5,$6,$7,$8)")
        .bind(id).bind(tenant.to_string()).bind(key).bind(member.to_string()).bind(at)
        .bind(status).bind(amount).bind(i64::from(aisix_pricing::ENGINE_VERSION))
        .execute(&mut *tx).await.expect("控制面核定的历史费用行");
    tx.commit().await.expect("保存历史记录");
}

async fn adjust(w: &World, id: &str, delta: i64) {
    let run = Uuid::new_v4();
    let mut tx = w.cp.db().pool().begin().await.expect("旧修正事务");
    sqlx::query("INSERT INTO billing_repricing_runs_cny (id,status,filter,reason,pricing_version,engine_version,events,changed,still_unpriced,skipped_unattributed,delta_micro_cny,by_tenant,created_by,applied_by,applied_at) VALUES ($1,'applied','{}','历史修正',0,$2,1,1,0,0,$3,'[]',$4,$4,now())")
        .bind(run).bind(i64::from(aisix_pricing::ENGINE_VERSION)).bind(delta).bind(w.admin.id)
        .execute(&mut *tx).await.expect("旧修正运行");
    sqlx::query("INSERT INTO pricing_adjustments_cny (run_id,event_id,delta_micro_cny,engine_version,reason,created_by) VALUES ($1,$2,$3,$4,'历史修正',$5)")
        .bind(run).bind(id).bind(delta).bind(i64::from(aisix_pricing::ENGINE_VERSION)).bind(w.admin.id)
        .execute(&mut *tx).await.expect("不提供明细的旧修正");
    tx.commit().await.expect("保存旧修正");
}

#[tokio::test]
async fn key_batch_keeps_current_visibility_zero_keys_and_exact_net_money() {
    let Some(w) = world().await else { return };
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let admin = w.cp.member_login_seeded(w.tenant, MemberRole::Admin).await;
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let paid = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let zero = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let big = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let inactive = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let revoked = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let org = seed_key(&w.cp, &w.admin, w.tenant, None).await;
    let other_tenant = seed_tenant(&w.cp, &w.admin).await;
    let foreign = seed_key(&w.cp, &w.admin, other_tenant, None).await;
    sqlx::query(
        "UPDATE tenant_keys SET disabled=true, expires_at=now()-interval '1 day' WHERE id=$1",
    )
    .bind(Uuid::parse_str(&inactive).expect("密钥编号"))
    .execute(w.cp.db().pool())
    .await
    .expect("过期停用密钥仍可见");
    sqlx::query("UPDATE tenant_keys SET revoked_at=now() WHERE id=$1")
        .bind(Uuid::parse_str(&revoked).expect("密钥编号"))
        .execute(w.cp.db().pool())
        .await
        .expect("准备已撤销密钥");
    let from = Utc::now() - TimeDelta::hours(1);
    let to = from + TimeDelta::hours(2);
    let at = from + TimeDelta::minutes(1);
    for (id, status, amount, final_attempt) in [
        ("paid-final", "charged", Some(1000), true),
        ("paid-retry", "charged", Some(200), false),
        ("pending-final", "unpriced", None, true),
        ("pending-retry", "unpriced", None, false),
        ("free-final", "exempt", Some(0), true),
        ("corrected-paid", "charged", Some(1000), true),
        ("corrected-pending", "unpriced", None, false),
        ("corrected-zero", "unpriced", None, true),
    ] {
        charge(
            &w,
            id,
            (w.tenant, &paid, member.id),
            at,
            status,
            amount,
            final_attempt,
        )
        .await;
    }
    adjust(&w, "corrected-paid", -50).await;
    adjust(&w, "corrected-paid", 20).await;
    adjust(&w, "corrected-pending", 500).await;
    adjust(&w, "corrected-zero", 50).await;
    adjust(&w, "corrected-zero", -50).await;
    charge(
        &w,
        "at-from",
        (w.tenant, &paid, member.id),
        from,
        "charged",
        Some(7),
        true,
    )
    .await;
    charge(
        &w,
        "before-from",
        (w.tenant, &paid, member.id),
        from - TimeDelta::microseconds(1),
        "charged",
        Some(9999),
        true,
    )
    .await;
    charge(
        &w,
        "at-to",
        (w.tenant, &paid, member.id),
        to,
        "charged",
        Some(9999),
        true,
    )
    .await;
    charge(
        &w,
        "big-money",
        (w.tenant, &big, member.id),
        at,
        "charged",
        Some(9_007_199_254_740_993),
        true,
    )
    .await;
    charge(
        &w,
        "org-money",
        (w.tenant, &org, member.id),
        at,
        "charged",
        Some(12),
        true,
    )
    .await;
    charge(
        &w,
        "inactive-money",
        (w.tenant, &inactive, member.id),
        at,
        "charged",
        Some(10),
        true,
    )
    .await;
    charge(
        &w,
        "revoked-money",
        (w.tenant, &revoked, member.id),
        at,
        "charged",
        Some(13),
        true,
    )
    .await;
    charge(
        &w,
        "foreign-money",
        (other_tenant, &foreign, member.id),
        at,
        "charged",
        Some(999),
        true,
    )
    .await;
    charge(
        &w,
        "wrong-tenant",
        (other_tenant, &paid, member.id),
        at,
        "charged",
        Some(999),
        true,
    )
    .await;
    charge(
        &w,
        "other-owner",
        (w.tenant, &w.key, w.member),
        at,
        "charged",
        Some(21),
        true,
    )
    .await;
    charge(
        &w,
        "past-other-owner",
        (w.tenant, &paid, w.member),
        at,
        "charged",
        Some(33),
        true,
    )
    .await;
    let uri = path(from, to);
    let all: BTreeSet<_> = [&w.key, &paid, &zero, &big, &inactive, &org]
        .into_iter()
        .cloned()
        .collect();
    for by in [&owner, &admin] {
        let response = get(&w, by, &uri).await;
        assert_eq!(ids(&response), all);
        assert_eq!(
            response
                .body
                .as_object()
                .expect("范围对象")
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["from", "to", "items"])
        );
        for (field, expected) in [("from", from), ("to", to)] {
            let actual =
                DateTime::parse_from_rfc3339(response.body[field].as_str().expect("显式时间范围"))
                    .expect("有效时间")
                    .with_timezone(&Utc);
            assert_eq!(
                actual.timestamp_micros(),
                expected.timestamp_micros(),
                "{field}"
            );
        }
        assert_eq!(row(&response, &paid)["charged_micro_cny"], "2710");
        assert_eq!(row(&response, &paid)["pending_requests"], 1);
        assert_eq!(row(&response, &paid)["unpriced_requests"], 1);
        assert_eq!(
            row(&response, &big)["charged_micro_cny"],
            "9007199254740993"
        );
        assert_eq!(row(&response, &org)["charged_micro_cny"], "12");
        assert_eq!(row(&response, &w.key)["charged_micro_cny"], "21");
        assert_eq!(row(&response, &inactive)["charged_micro_cny"], "10");
        for item in items(&response) {
            assert_eq!(
                item.as_object()
                    .expect("费用对象")
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([
                    "api_key_id",
                    "charged_micro_cny",
                    "pending_requests",
                    "unpriced_requests"
                ])
            );
        }
    }
    let mine = get(&w, &member, &uri).await;
    assert_eq!(
        ids(&mine),
        [&paid, &zero, &big, &inactive]
            .into_iter()
            .cloned()
            .collect()
    );
    assert_eq!(row(&mine, &paid)["charged_micro_cny"], "2677");
    assert_eq!(row(&mine, &zero)["charged_micro_cny"], "0");
    assert_eq!(row(&mine, &zero)["pending_requests"], 0);
    assert_eq!(row(&mine, &zero)["unpriced_requests"], 0);
    let widened = get(
        &w,
        &member,
        &format!("{uri}&tenant_id={other_tenant}&member_id={}", owner.id),
    )
    .await;
    assert_eq!(
        widened.body, mine.body,
        "未知查询参数不能扩大当前会话的范围"
    );
}

#[tokio::test]
async fn key_batch_matches_keys_list_roles_and_inactive_tenant_read_access() {
    let Some(w) = world().await else { return };
    let from = Utc::now() - TimeDelta::hours(1);
    let uri = path(from, Utc::now());
    let anonymous = w.cp.send("GET", &uri, None, None).await;
    assert_eq!(anonymous.status, 401, "{anonymous:?}");
    let billing =
        w.cp.member_login_seeded(w.tenant, MemberRole::Billing)
            .await;
    let forbidden = get(&w, &billing, &uri).await;
    assert_eq!(forbidden.status, 403, "{forbidden:?}");
    for status in [TenantStatus::Pending, TenantStatus::Suspended] {
        let tenant = w.cp.seed_tenant(status).await;
        let owner = w.cp.member_login_seeded(tenant, MemberRole::Owner).await;
        let response = get(&w, &owner, &uri).await;
        assert!(items(&response).is_empty());
    }
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    charge(
        &w,
        "before-suspend",
        (w.tenant, &w.key, w.member),
        from + TimeDelta::minutes(1),
        "charged",
        Some(42),
        true,
    )
    .await;
    let before = get(&w, &owner, &uri).await;
    assert_eq!(row(&before, &w.key)["charged_micro_cny"], "42");
    let suspended =
        w.cp.send(
            "POST",
            &format!("/admin/tenants/{}/suspend", w.tenant),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(suspended.status, 200, "{suspended:?}");
    let after = get(&w, &owner, &uri).await;
    assert_eq!(
        items(&after),
        items(&before),
        "停用后保留现有密钥的历史费用"
    );
}

#[tokio::test]
async fn key_batch_requires_a_valid_bounded_explicit_range() {
    let Some(w) = world().await else { return };
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    for query in [
        "",
        "?from=2026-01-01T00:00:00Z",
        "?from=bad&to=2026-01-02T00:00:00Z",
        "?from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z",
        "?from=2026-01-01T00:00:00Z&to=2026-01-01T00:00:00Z",
        "?from=2026-01-01T00:00:00Z&to=2026-04-04T00:00:00Z",
    ] {
        let response = get(&w, &owner, &format!("/api/usage/keys{query}")).await;
        assert_eq!(response.status, 422, "{response:?}");
    }
}
