//! 组织导出通过真实会话、控制面和 PostgreSQL 验证。
mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use chrono::{SecondsFormat, TimeDelta, Utc};
use serde_json::json;
use support::world::{deliver, price, seed_key, seed_model, seed_tenant, tokens_680, usage, world};
use support::Resp;
use uuid::Uuid;

fn csv<'a>(reply: &'a Resp, name: &str) -> &'a str {
    assert_eq!(reply.status, 200, "{reply:?}");
    assert_eq!(
        reply.header("content-type"),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(
        reply.header("content-disposition"),
        Some(format!("attachment; filename=\"{name}.csv\"").as_str())
    );
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    let text = reply.body.as_str().expect("CSV body");
    assert!(text.ends_with("\r\n"));
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

#[tokio::test]
async fn usage_export_matches_grouped_filters_and_key_ownership() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let other_model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let own_key = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let other = seed_tenant(&w.cp, &w.admin).await;
    let foreign_key = seed_key(&w.cp, &w.admin, other, None).await;
    let foreign_owner = w.cp.member_login_seeded(other, MemberRole::Owner).await;
    let now = Utc::now() + TimeDelta::seconds(2);
    let to = now + TimeDelta::seconds(3);

    let mut common = tokens_680();
    common["is_final_attempt"] = json!(true);
    common["occurred_at"] = json!(now);
    common["requested_model"] = json!("  =2+2,\"字\"\r\nnext");
    common["provider_key_name"] = json!("private-upstream-account");
    common["request_id"] = json!("private-user@example.test");
    let mut own = common.clone();
    own["api_key_id"] = json!(own_key);
    own["trace_id"] = json!("1234567890abcdef1234567890abcdef");
    own["attempt_index"] = json!(1);
    let mut attempt = own.clone();
    attempt["is_final_attempt"] = json!(false);
    attempt["attempt_index"] = json!(0);
    let mut foreign = common.clone();
    foreign["team_id"] = json!(other);
    foreign["api_key_id"] = json!(foreign_key);
    let mut outside = own.clone();
    outside["occurred_at"] = json!(to);
    outside["trace_id"] = json!("1234567890abcdef1234567890abcdee");
    let answer = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(&w, "own-attempt", model, attempt),
            usage(&w, "own-representative", model, own),
            usage(&w, "same-org-other-member", model, common.clone()),
            usage(&w, "same-org-other-model", other_model, common.clone()),
            usage(&w, "other-tenant-marker", model, foreign),
            usage(&w, "exclusive-upper-bound", model, outside),
        ],
    )
    .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body.get("rejected"), Some(&json!(0)), "{answer:?}");

    let from = now.to_rfc3339_opts(SecondsFormat::Micros, true);
    let to = to.to_rfc3339_opts(SecondsFormat::Micros, true);
    let path = format!("/api/usage/calls/export?from={from}&to={to}&model_id={model}");
    let reply = w.cp.get_with_cookie(&path, owner.cookie()).await;
    let all = csv(&reply, "usage-calls");
    assert!(all.contains("same-org-other-member"), "{all}");
    assert!(all.contains("own-representative"), "{all}");
    for hidden in [
        "same-org-other-model",
        "own-attempt",
        "other-tenant-marker",
        "exclusive-upper-bound",
        "private-upstream-account",
        "private-user@example.test",
        "sk-pricing-upstream-7c1",
    ] {
        assert!(!all.contains(hidden), "unexpected {hidden} in {all}");
    }
    assert!(all.contains("\"'  =2+2,\"\"字\"\"\r\nnext\""), "{all}");

    let own_reply = w.cp.get_with_cookie(&path, member.cookie()).await;
    let owned = csv(&own_reply, "usage-calls");
    assert!(owned.contains("own-representative"), "{owned}");
    assert!(!owned.contains("same-org-other-member"), "{owned}");
    assert!(
        owned.contains(r#""succeeded",2,1360,1000,200,800,"#),
        "组费用应包含两次计费，代表调用的 Token 数保持原值：{owned}"
    );
    let filtered =
        w.cp.get_with_cookie(&format!("{path}&api_key_id={}", w.key), member.cookie())
            .await;
    assert_eq!(csv(&filtered, "usage-calls").lines().count(), 1);
    let key_reply =
        w.cp.get_with_cookie(&format!("{path}&api_key_id={own_key}"), owner.cookie())
            .await;
    assert_eq!(csv(&key_reply, "usage-calls"), owned);
    let other = w.cp.get_with_cookie(&path, foreign_owner.cookie()).await;
    assert!(csv(&other, "usage-calls").contains("other-tenant-marker"));
    assert!(!csv(&other, "usage-calls").contains("own-representative"));
}

#[tokio::test]
async fn billing_exports_preserve_exact_money_and_exclude_personal_fields() {
    let Some(w) = world().await else { return };
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let billing =
        w.cp.member_login_seeded(w.tenant, MemberRole::Billing)
            .await;
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let other = seed_tenant(&w.cp, &w.admin).await;
    let foreign_owner = w.cp.member_login_seeded(other, MemberRole::Owner).await;
    let personal = "private-finance@example.test";
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let credit =
        w.cp.send(
            "POST",
            &format!("/admin/tenants/{}/credits", w.tenant),
            Some(admin.cookie()),
            Some(json!({"amount_cny": "123.45", "reason": personal})),
        )
        .await;
    assert_eq!(credit.status, 201, "{credit:?}");
    let credit_id = credit.body["id"].as_str().expect("credit id");
    // 除了正常充值，再保存一笔超过 JS 安全整数范围的历史账目。
    let mut tx = w.cp.db().pool().begin().await.expect("begin");
    let large_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_topups_cny
         (id,tenant_id,amount_micro_cny,origin,status,decided_by,decided_at,credit_reason)
         SELECT $1,tenant_id,9007199254740993,origin,status,decided_by,decided_at,credit_reason
         FROM billing_topups_cny WHERE id = $2",
    )
    .bind(large_id)
    .bind(Uuid::parse_str(credit_id).expect("credit UUID"))
    .execute(&mut *tx)
    .await
    .expect("store a historical exact top-up");
    sqlx::query(
        "INSERT INTO billing_ledger_cny
         (tenant_id,kind,amount_micro_cny,balance_after_micro_cny,topup_id)
         VALUES ($1,'topup',9007199254740993,9007199378190993,$2)",
    )
    .bind(w.tenant)
    .bind(large_id)
    .execute(&mut *tx)
    .await
    .expect("append the matching exact ledger amount");
    tx.commit().await.expect("commit");
    let pending =
        w.cp.send(
            "POST",
            "/api/billing/topups",
            Some(owner.cookie()),
            Some(json!({"amount_cny": "1.23", "reference": personal, "note": personal})),
        )
        .await;
    assert_eq!(pending.status, 201, "{pending:?}");
    let pending_id = pending.body["id"].as_str().expect("top-up id");

    let ledger_path = "/api/billing/ledger/export?kind=topup";
    let topup_path = "/api/billing/topups/export?status=approved";
    let ledger = w.cp.get_with_cookie(ledger_path, billing.cookie()).await;
    let topups = w.cp.get_with_cookie(topup_path, billing.cookie()).await;
    for (download, name) in [(&ledger, "billing-ledger"), (&topups, "billing-topups")] {
        let text = csv(download, name);
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(text.contains("9007199254740993"), "{text}");
        assert!(text.contains(credit_id), "{text}");
        assert!(!text.contains(pending_id), "{text}");
        assert!(!text.contains(personal), "{text}");
        assert!(!text.contains(&owner.email), "{text}");
    }
    let negative = "&from=2001-01-01T00:00:00Z&to=2001-01-02T00:00:00Z";
    for (path, name) in [
        (ledger_path, "billing-ledger"),
        (topup_path, "billing-topups"),
    ] {
        let empty =
            w.cp.get_with_cookie(&format!("{path}{negative}"), billing.cookie())
                .await;
        assert_eq!(csv(&empty, name).lines().count(), 1);
        let foreign = w.cp.get_with_cookie(path, foreign_owner.cookie()).await;
        assert_eq!(csv(&foreign, name).lines().count(), 1);
        assert_eq!(
            w.cp.get_with_cookie(path, member.cookie()).await.status,
            403
        );
        assert_eq!(w.cp.send("GET", path, None, None).await.status, 401);
    }
    let invalid =
        w.cp.get_with_cookie(
            "/api/billing/topups/export?from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z",
            owner.cookie(),
        )
        .await;
    assert_eq!(invalid.status, 422, "{invalid:?}");
}

#[tokio::test]
async fn a_large_export_is_refused_instead_of_returning_partial_data() {
    let Some(w) = world().await else { return };
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    sqlx::query(
        "INSERT INTO billing_topups_cny (id, tenant_id, amount_micro_cny, origin, status, requested_by)
         SELECT gen_random_uuid(), $1, 1, 'tenant', 'pending', $2 FROM generate_series(1, 5001)",
    )
    .bind(w.tenant)
    .bind(owner.id)
    .execute(w.cp.db().pool())
    .await
    .expect("store a range too large for one export");

    let path = "/api/billing/topups/export";
    let capped = w.cp.get_with_cookie(path, owner.cookie()).await;
    assert_eq!(capped.status, 422, "{capped:?}");
    assert_eq!(capped.body["code"], "validation_failed");
    assert!(capped.header("content-disposition").is_none());
    assert!(capped.body["details"][0]["message"]
        .as_str()
        .expect("narrowing guidance")
        .contains("缩短日期范围"));

    let narrow =
        w.cp.get_with_cookie(&format!("{path}?status=approved"), owner.cookie())
            .await;
    assert_eq!(csv(&narrow, "billing-topups").lines().count(), 1);
}
