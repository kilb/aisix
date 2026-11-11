//! 成员汇总通过真实 HTTP、Postgres 与遥测入口验证；不启动模型替身。

mod support;

use std::collections::BTreeSet;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::tenants::TenantStatus;
use chrono::{Datelike, TimeDelta, Utc};
use serde_json::{json, Value};
use support::members::MemberSession;
use support::world::{
    deliver, insert_baseline_backdated, price, seed_key, seed_model, seed_tenant, tokens_680,
    usage, world, World,
};
use support::Resp;
use uuid::Uuid;

async fn get(w: &World, by: &MemberSession, path: &str) -> Resp {
    w.cp.send("GET", path, Some(by.cookie()), None).await
}

fn items(r: &Resp) -> &[Value] {
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items 数组")
}

fn member(r: &Resp, id: Uuid) -> &Value {
    items(r)
        .iter()
        .find(|row| row["member_id"] == id.to_string())
        .unwrap_or_else(|| panic!("缺少成员 {id}: {r:?}"))
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("对象").keys().cloned().collect()
}

fn key_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| name.to_string()).collect()
}

async fn intake(w: &World, events: Vec<Value>) {
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
}

fn event(w: &World, id: &str, model: Uuid, key: &str, claimed_member: Uuid) -> Value {
    let mut row = usage(w, id, model, tokens_680());
    row["api_key_id"] = json!(key);
    row["user_id"] = json!(claimed_member);
    row["is_final_attempt"] = json!(true);
    row
}

#[tokio::test]
async fn managers_get_one_whitelisted_batch_with_key_record_ownership_and_zero_members() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let admin = w.cp.member_login_seeded(w.tenant, MemberRole::Admin).await;
    let second = w.cp.seed_member(w.tenant, MemberRole::Member).await;
    let zero = w.cp.seed_member(w.tenant, MemberRole::Member).await;
    let second_key = seed_key(&w.cp, &w.admin, w.tenant, Some(second.id)).await;
    let another_key = seed_key(&w.cp, &w.admin, w.tenant, Some(w.member)).await;
    let org_key = seed_key(&w.cp, &w.admin, w.tenant, None).await;
    let other_tenant = seed_tenant(&w.cp, &w.admin).await;
    let outsider = w.cp.seed_member(other_tenant, MemberRole::Owner).await;
    let outside_key = seed_key(&w.cp, &w.admin, other_tenant, Some(outsider.id)).await;
    intake(
        &w,
        vec![
            event(&w, "member-first", model, &w.key, second.id),
            event(&w, "member-second-key", model, &another_key, owner.id),
            event(&w, "second-member", model, &second_key, w.member),
            event(&w, "organization-key", model, &org_key, owner.id),
            event(&w, "other-tenant-key", model, &outside_key, w.member),
        ],
    )
    .await;

    for by in [&owner, &admin] {
        let r = get(&w, by, "/api/members/usage").await;
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(keys(&r.body), key_set(&["from", "to", "items"]));
        assert_eq!(items(&r).len(), 5, "外租户或组织 key 不应成为成员: {r:?}");
        assert_eq!(member(&r, w.member)["requests"], 2);
        assert_eq!(member(&r, w.member)["charged_micro_cny"], "1360");
        assert_eq!(
            member(&r, w.member)["quantities"],
            json!({"input": 400, "output": 400, "cache_read": 1600})
        );
        assert_eq!(member(&r, second.id)["requests"], 1);
        assert_eq!(member(&r, second.id)["charged_micro_cny"], "680");
        for id in [owner.id, admin.id, zero.id] {
            let row = member(&r, id);
            assert_eq!(row["requests"], 0, "{row}");
            assert_eq!(row["charged_micro_cny"], "0", "{row}");
            assert_eq!(row["quantities"], json!({}), "{row}");
            assert_eq!(row["pending_requests"], 0, "{row}");
            assert_eq!(row["unpriced_requests"], 0, "{row}");
            assert_eq!(row["unknown_quantity_requests"], 0, "{row}");
        }
        for row in items(&r) {
            assert_eq!(
                keys(row),
                key_set(&[
                    "member_id",
                    "requests",
                    "charged_micro_cny",
                    "estimated_requests",
                    "quantities",
                    "pending_requests",
                    "unpriced_requests",
                    "unknown_quantity_requests",
                ])
            );
            assert_eq!(row["estimated_requests"], 0);
        }
        let from = r.body["from"]
            .as_str()
            .expect("from")
            .parse::<chrono::DateTime<Utc>>()
            .expect("时间");
        let to = r.body["to"]
            .as_str()
            .expect("to")
            .parse::<chrono::DateTime<Utc>>()
            .expect("时间");
        let now = Utc::now();
        assert_eq!(
            from.date_naive(),
            now.date_naive().with_day(1).expect("月初")
        );
        assert_eq!(from.time(), chrono::NaiveTime::MIN);
        assert!(to > now - TimeDelta::minutes(1) && to <= now + TimeDelta::minutes(1));
    }
    let outsider_session = w.cp.member_login_as(&outsider).await;
    let outside = get(&w, &outsider_session, "/api/members/usage").await;
    assert_eq!(items(&outside).len(), 1);
    assert_eq!(member(&outside, outsider.id)["requests"], 1);
}

#[tokio::test]
async fn legacy_adjustments_keep_missing_quantities_unknown_in_mixed_and_all_unknown_members() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let second = w.cp.seed_member(w.tenant, MemberRole::Member).await;
    let second_key = seed_key(&w.cp, &w.admin, w.tenant, Some(second.id)).await;
    intake(
        &w,
        vec![
            event(&w, "old-mixed", model, &w.key, owner.id),
            event(&w, "known-mixed", model, &w.key, owner.id),
            event(&w, "old-only", model, &second_key, owner.id),
        ],
    )
    .await;
    // 旧写入者不提供数量列；用同一合法旧格式保留缺失，而非改写 append-only 记录。
    let run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_repricing_runs_cny \
         (id, status, filter, reason, pricing_version, engine_version, events, changed, \
          still_unpriced, skipped_unattributed, delta_micro_cny, by_tenant, created_by, applied_by, applied_at) \
         VALUES ($1, 'applied', '{}', '旧版本数量明细缺失', 0, $2, 2, 2, 0, 0, 2, $3, $4, $4, now())",
    )
    .bind(run)
    .bind(i64::from(aisix_pricing::ENGINE_VERSION))
    .bind(json!([{"tenant_id": w.tenant, "events": 2, "changed": 2, "delta_micro_cny": 2}]))
    .bind(w.admin.id)
    .execute(w.cp.db().pool())
    .await
    .expect("旧版本已应用的重计价记录");
    sqlx::query(
        "INSERT INTO pricing_adjustments_cny \
         (run_id, event_id, delta_micro_cny, engine_version, reason, created_by) \
         SELECT $1, event_id, 1, $2, '旧版本数量明细缺失', $3 \
         FROM pricing_charges_cny WHERE event_id IN ('old-mixed', 'old-only')",
    )
    .bind(run)
    .bind(i64::from(aisix_pricing::ENGINE_VERSION))
    .bind(w.admin.id)
    .execute(w.cp.db().pool())
    .await
    .expect("旧格式 adjustment 的 lines 默认为 NULL");
    let r = get(&w, &owner, "/api/members/usage").await;
    let mixed = member(&r, w.member);
    assert_eq!(mixed["requests"], 2);
    assert_eq!(mixed["charged_micro_cny"], "1361");
    assert_eq!(
        mixed["quantities"],
        json!({"input": 200, "output": 200, "cache_read": 800})
    );
    assert_eq!(mixed["pending_requests"], 0);
    assert_eq!(mixed["unpriced_requests"], 0);
    assert_eq!(mixed["unknown_quantity_requests"], 1);
    let unknown = member(&r, second.id);
    assert_eq!(unknown["requests"], 1);
    assert_eq!(unknown["charged_micro_cny"], "681");
    assert_eq!(unknown["quantities"], json!({}));
    assert_eq!(unknown["pending_requests"], 0);
    assert_eq!(unknown["unknown_quantity_requests"], 1);
    assert_eq!(member(&r, owner.id)["unknown_quantity_requests"], 0);
}

#[tokio::test]
async fn default_month_and_explicit_cross_month_windows_use_inclusive_from_and_exclusive_to() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let now = Utc::now();
    let month_start = (now.date_naive() - TimeDelta::days(i64::from(now.day0())))
        .and_time(chrono::NaiveTime::MIN)
        .and_utc();
    insert_baseline_backdated(
        w.cp.db(),
        model,
        serde_json::to_value(support::world::card()).expect("价格卡"),
        month_start - TimeDelta::days(2),
    )
    .await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let from = month_start - TimeDelta::seconds(1);
    let to = month_start + TimeDelta::microseconds(1);
    let mut events = Vec::new();
    for (id, at) in [
        ("before-from", from - TimeDelta::microseconds(1)),
        ("exact-from", from),
        ("month-start", month_start),
        ("exact-to", to),
    ] {
        let mut row = event(&w, id, model, &w.key, owner.id);
        row["occurred_at"] = json!(at);
        events.push(row);
    }
    intake(&w, events).await;
    let monthly = get(&w, &owner, "/api/members/usage").await;
    assert_eq!(member(&monthly, w.member)["requests"], 2);
    assert_eq!(member(&monthly, w.member)["charged_micro_cny"], "1360");
    let custom = get(
        &w,
        &owner,
        &format!(
            "/api/members/usage?from={}&to={}",
            from.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            to.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        ),
    )
    .await;
    assert_eq!(member(&custom, w.member)["requests"], 2);
    assert_eq!(member(&custom, w.member)["charged_micro_cny"], "1360");
    assert_eq!(
        member(&custom, w.member)["quantities"],
        json!({"input": 400, "output": 400, "cache_read": 1600})
    );
}

#[tokio::test]
async fn revoking_keys_and_removing_a_member_preserve_charged_history() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    intake(
        &w,
        vec![event(&w, "before-revoke", model, &w.key, owner.id)],
    )
    .await;
    let key_path = format!("/api/keys/{}", w.key);
    let key = get(&w, &owner, &key_path).await;
    assert_eq!(key.status, 200, "{key:?}");
    let version = key.headers[reqwest::header::ETAG]
        .to_str()
        .expect("ETag")
        .trim_matches('"')
        .parse()
        .expect("版本");
    let revoked =
        w.cp.send_conditional("DELETE", &key_path, owner.cookie(), version, json!({}))
            .await;
    assert_eq!(revoked.status, 204, "{revoked:?}");
    intake(
        &w,
        vec![event(
            &w,
            "late-intake-after-revoke",
            model,
            &w.key,
            owner.id,
        )],
    )
    .await;
    let before = get(&w, &owner, "/api/members/usage").await;
    assert_eq!(member(&before, w.member)["requests"], 2);
    assert_eq!(member(&before, w.member)["charged_micro_cny"], "1360");
    let removed =
        w.cp.send(
            "DELETE",
            &format!("/api/members/{}", w.member),
            Some(owner.cookie()),
            None,
        )
        .await;
    assert_eq!(removed.status, 204, "{removed:?}");
    let after = get(&w, &owner, "/api/members/usage").await;
    assert_eq!(member(&after, w.member), member(&before, w.member));
}

#[tokio::test]
async fn unpriced_calls_are_explicitly_pending_and_member_budgets_keep_their_own_periods() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let unpriced = seed_model(&w.cp, &w.admin).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    for body in [
        json!({"scope": "member", "scope_ref": w.member, "period": "day", "limit_cny": "1.00"}),
        json!({"scope": "member", "scope_ref": w.member, "period": "month", "limit_cny": "2.00"}),
        json!({"scope": "member", "scope_ref": w.member, "kind": "cumulative", "limit_cny": "3.00"}),
    ] {
        let r =
            w.cp.send("POST", "/api/budgets", Some(owner.cookie()), Some(body))
                .await;
        assert_eq!(r.status, 201, "{r:?}");
    }
    intake(
        &w,
        vec![
            event(&w, "priced", model, &w.key, owner.id),
            event(&w, "not-yet-priced", unpriced, &w.key, owner.id),
        ],
    )
    .await;
    let r = get(&w, &owner, "/api/members/usage").await;
    let row = member(&r, w.member);
    assert_eq!(row["requests"], 2);
    assert_eq!(row["charged_micro_cny"], "680");
    assert_eq!(row["pending_requests"], 1);
    assert_eq!(row["unpriced_requests"], 1);
    let budgets = get(&w, &owner, "/api/budgets").await;
    assert_eq!(items(&budgets).len(), 3);
    for budget in items(&budgets) {
        assert_eq!(budget["scope"], "member");
        assert_eq!(budget["target"]["id"], json!(w.member));
        assert_eq!(budget["spent_micro_cny"], 680);
        let limit = match (budget["kind"].as_str(), budget["period"].as_str()) {
            (Some("periodic"), Some("day")) => 1_000_000,
            (Some("periodic"), Some("month")) => 2_000_000,
            (Some("cumulative"), None) => 3_000_000,
            other => panic!("额度周期被折叠: {other:?}"),
        };
        assert_eq!(budget["limit_micro_cny"], limit);
        assert_eq!(budget["remaining_micro_cny"], limit - 680);
        assert!(budget.get("credit_limit_micro_cny").is_none());
    }
}

#[tokio::test]
async fn member_usage_is_manager_only_active_only_and_rejects_invalid_ranges() {
    let Some(w) = world().await else { return };
    for role in [MemberRole::Member, MemberRole::Billing] {
        let session = w.cp.member_login_seeded(w.tenant, role).await;
        assert_eq!(get(&w, &session, "/api/members/usage").await.status, 403);
    }
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    for range in [
        "from=not-a-time",
        "from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z",
        "from=2025-01-01T00:00:00Z&to=2026-01-01T00:00:00Z",
    ] {
        let r = get(&w, &owner, &format!("/api/members/usage?{range}")).await;
        assert_eq!(r.status, 422, "{r:?}");
    }
    for status in [TenantStatus::Pending, TenantStatus::Suspended] {
        let tenant = w.cp.seed_tenant(status).await;
        let session = w.cp.member_login_seeded(tenant, MemberRole::Owner).await;
        let r = get(&w, &session, "/api/members/usage").await;
        assert_eq!(r.status, 403, "{r:?}");
        assert_eq!(r.body["code"], "tenant_not_active");
        assert_eq!(get(&w, &session, "/api/members").await.status, 200);
    }
}

#[tokio::test]
async fn repricing_moves_pending_usage_to_its_corrected_charge_and_quantities() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    intake(&w, vec![event(&w, "corrected", model, &w.key, owner.id)]).await;
    let before = get(&w, &owner, "/api/members/usage").await;
    assert_eq!(member(&before, w.member)["requests"], 1);
    assert_eq!(member(&before, w.member)["pending_requests"], 1);
    assert_eq!(member(&before, w.member)["quantities"], json!({}));
    price(&w.cp, model).await;
    let to = Utc::now();
    let preview = w.cp.send("POST", "/admin/billing/repricing", Some(w.admin.cookie()), Some(json!({
        "from": to - TimeDelta::hours(1), "to": to, "model_id": model, "reason": "验证成员修正金额",
    }))).await;
    assert_eq!(preview.status, 201, "{preview:?}");
    assert_eq!(preview.body["changed"], 1);
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = preview.body["id"].as_str().expect("重计价编号");
    let applied =
        w.cp.send(
            "POST",
            &format!("/admin/billing/repricing/{id}/apply"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(applied.status, 200, "{applied:?}");
    let after = get(&w, &owner, "/api/members/usage").await;
    let row = member(&after, w.member);
    assert_eq!(row["requests"], 1);
    assert_eq!(row["charged_micro_cny"], "680");
    assert_eq!(row["pending_requests"], 0);
    assert_eq!(row["unpriced_requests"], 0);
    assert_eq!(
        row["quantities"],
        json!({"input": 200, "output": 200, "cache_read": 800})
    );
}
