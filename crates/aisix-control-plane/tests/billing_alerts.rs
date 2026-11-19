mod support;

use aisix_control_plane::auth::MemberRole;
use aisix_control_plane::tenants::TenantStatus;
use serde_json::json;
use support::TestCp;

const ROUTE: &str = "/api/billing/alerts";

#[tokio::test]
async fn alert_preferences_are_opt_in_tenant_scoped_and_owner_admin_only() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let admin = cp.member_login_seeded(tenant, MemberRole::Admin).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let other_tenant = cp.seed_tenant(TenantStatus::Active).await;
    let other_owner = cp
        .member_login_seeded(other_tenant, MemberRole::Owner)
        .await;

    let current = cp.send("GET", ROUTE, Some(owner.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    assert_eq!(current.body["webhook_configured"], false);
    assert_eq!(
        current.body.get("low_balance_micro_cny"),
        Some(&json!(null))
    );
    assert_eq!(current.body.get("budget_percent"), Some(&json!(null)));
    assert_eq!(current.body.get("spend_spike_percent"), Some(&json!(null)));
    assert_eq!(current.body.get("email"), Some(&json!(null)));

    // 实际余额高于测试阈值，因此后台任务不会访问配置的占位地址。
    sqlx::query("UPDATE billing_accounts SET balance_micro_cny = 2000000 WHERE tenant_id = $1")
        .bind(tenant)
        .execute(cp.db().pool())
        .await
        .expect("raise test balance above notification threshold");
    let secret_url = "https://webhook.example.test/receive?token=opaque";
    let saved = cp.send("PUT", ROUTE, Some(admin.cookie()), Some(json!({
        "low_balance_micro_cny": 1_500_000, "budget_percent": 90, "spend_spike_percent": 250,
        "email": owner.email, "webhook_url": secret_url,
    }))).await;
    assert_eq!(saved.status, 200, "{saved:?}");
    assert_eq!(saved.body["low_balance_micro_cny"], 1_500_000);
    assert_eq!(saved.body["budget_percent"], 90);
    assert_eq!(saved.body["spend_spike_percent"], 250);
    assert_eq!(saved.body["email"], owner.email);
    assert_eq!(saved.body["webhook_configured"], true);
    assert!(!saved.body.to_string().contains("opaque"));

    let read = cp.send("GET", ROUTE, Some(owner.cookie()), None).await;
    assert_eq!(read.body, saved.body);
    let other = cp
        .send("GET", ROUTE, Some(other_owner.cookie()), None)
        .await;
    assert_eq!(other.body, current.body);
    for method in ["GET", "PUT"] {
        let answer = cp
            .send(
                method,
                ROUTE,
                Some(member.cookie()),
                (method == "PUT").then(|| json!({})),
            )
            .await;
        assert_eq!(answer.status, 403, "{answer:?}");
    }

    let preserved = cp
        .send(
            "PUT",
            ROUTE,
            Some(owner.cookie()),
            Some(json!({"budget_percent": 50})),
        )
        .await;
    assert_eq!(preserved.status, 200, "{preserved:?}");
    assert_eq!(preserved.body["webhook_configured"], true);
    assert_eq!(preserved.body["budget_percent"], 50);
    let removed = cp
        .send(
            "PUT",
            ROUTE,
            Some(owner.cookie()),
            Some(json!({"webhook_url": null})),
        )
        .await;
    assert_eq!(removed.body, current.body);
}

#[tokio::test]
async fn notification_destinations_and_thresholds_are_validated_before_saving() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    for (body, field) in [
        (json!({"email": "someone@external.test"}), "/email"),
        (
            json!({"webhook_url": "http://public.example.test"}),
            "/webhook_url",
        ),
        (json!({"webhook_url": "https://127.0.0.1"}), "/webhook_url"),
        (
            json!({"webhook_url": "https://public.example.test:8000"}),
            "/webhook_url",
        ),
        (json!({"budget_percent": 101}), "/budget_percent"),
        (json!({"spend_spike_percent": 100}), "/spend_spike_percent"),
        (
            json!({"low_balance_micro_cny": 1}),
            "/low_balance_micro_cny",
        ),
    ] {
        let answer = cp
            .send("PUT", ROUTE, Some(owner.cookie()), Some(body))
            .await;
        assert_eq!(answer.status, 422, "{answer:?}");
        assert_eq!(answer.body["details"][0]["path"], field);
    }
}

#[tokio::test]
async fn workers_share_claims_preserve_payload_on_retry_and_stop_after_three_attempts() {
    use aisix_control_plane::billing::alerts::reserve;

    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let settings = cp
        .send(
            "PUT",
            ROUTE,
            Some(owner.cookie()),
            Some(json!({"low_balance_micro_cny": 0, "email": owner.email})),
        )
        .await;
    assert_eq!(settings.status, 200, "{settings:?}");
    assert!(reserve(cp.state(), tenant, false)
        .await
        .expect("mail disabled")
        .is_empty());

    // 交给两个真实数据库连接同时认领，只有一个工作进程拿到发送权。
    let (left, right) = tokio::join!(
        reserve(cp.state(), tenant, true),
        reserve(cp.state(), tenant, true),
    );
    let mut claims = left.expect("first worker");
    claims.extend(right.expect("second worker"));
    assert_eq!(claims.len(), 1);
    let first = &claims[0];
    assert_eq!(first.kind, "low_balance");
    assert_eq!(first.channel, "email");
    assert_eq!(first.observed_micro_cny, 0);
    assert_eq!(first.threshold_micro_cny, 0);
    assert!(reserve(cp.state(), tenant, true)
        .await
        .expect("lease active")
        .is_empty());

    sqlx::query("UPDATE billing_accounts SET balance_micro_cny = -500000 WHERE tenant_id = $1")
        .bind(tenant)
        .execute(cp.db().pool())
        .await
        .expect("lower the actual balance");
    for outcome in ["claimed", "failed"] {
        sqlx::query(
            "UPDATE billing_alert_states SET outcome = $2, next_attempt_at = now()
             WHERE tenant_id = $1",
        )
        .bind(tenant)
        .bind(outcome)
        .execute(cp.db().pool())
        .await
        .expect("make a terminated or failed claim due");
        let retried = reserve(cp.state(), tenant, true)
            .await
            .expect("retry a due alert");
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].delivery_id, first.delivery_id);
        assert_eq!(retried[0].measured_at, first.measured_at);
        assert_eq!(retried[0].observed_micro_cny, 0);
    }
    sqlx::query("UPDATE billing_alert_states SET next_attempt_at = now() WHERE tenant_id = $1")
        .bind(tenant)
        .execute(cp.db().pool())
        .await
        .expect("expire the last lease");
    assert!(reserve(cp.state(), tenant, true)
        .await
        .expect("retry cap")
        .is_empty());

    let removed = cp
        .send("PUT", ROUTE, Some(owner.cookie()), Some(json!({})))
        .await;
    assert_eq!(removed.status, 200, "{removed:?}");
    assert!(reserve(cp.state(), tenant, true)
        .await
        .expect("disabled")
        .is_empty());
}

#[tokio::test]
async fn organization_threshold_uses_its_current_budget_period_and_rearms_at_rollover() {
    use aisix_control_plane::auth::OperatorRole;
    use aisix_control_plane::billing::alerts::reserve;
    use uuid::Uuid;

    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let quota = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/budgets"),
            Some(operator.cookie()),
            Some(json!({"limit_cny": "10", "period": "day"})),
        )
        .await;
    assert_eq!(quota.status, 201, "{quota:?}");
    let budget: Uuid = serde_json::from_value(quota.body["id"].clone()).expect("budget ID");
    let saved = cp
        .send(
            "PUT",
            ROUTE,
            Some(owner.cookie()),
            Some(json!({"budget_percent": 80, "email": owner.email})),
        )
        .await;
    assert_eq!(saved.status, 200, "{saved:?}");

    sqlx::query(
        "INSERT INTO billing_budget_spend_cny (budget_id, period_start, spent_micro_cny)
         VALUES ($1, date_trunc('day', now(), 'UTC') - interval '24 hours', 10000000),
                ($1, date_trunc('day', now(), 'UTC'), 7999999)
         ON CONFLICT (budget_id, period_start) DO UPDATE
           SET spent_micro_cny = EXCLUDED.spent_micro_cny",
    )
    .bind(budget)
    .execute(cp.db().pool())
    .await
    .expect("seed previous and current budget spend");
    assert!(reserve(cp.state(), tenant, true)
        .await
        .expect("below threshold")
        .is_empty());
    sqlx::query(
        "UPDATE billing_budget_spend_cny SET spent_micro_cny = 8000000
         WHERE budget_id = $1 AND period_start = date_trunc('day', now(), 'UTC')",
    )
    .bind(budget)
    .execute(cp.db().pool())
    .await
    .expect("reach the current budget threshold");
    let claims = reserve(cp.state(), tenant, true)
        .await
        .expect("budget threshold");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].kind, "budget_threshold");
    assert_eq!(claims[0].observed_micro_cny, 8_000_000);
    assert_eq!(claims[0].threshold_micro_cny, 8_000_000);
    assert!(claims[0].window_start.is_some());
    sqlx::query(
        "UPDATE billing_alert_states SET outcome = 'sent',
             last_claimed_at = now() - interval '25 hours', window_start = window_start - interval '24 hours'
         WHERE tenant_id = $1",
    )
    .bind(tenant)
    .execute(cp.db().pool())
    .await
    .expect("simulate an acknowledged alert from the previous period");
    let next = reserve(cp.state(), tenant, true)
        .await
        .expect("new budget period");
    assert_ne!(next[0].delivery_id, claims[0].delivery_id);
    assert_eq!(next[0].window_start, claims[0].window_start);
}

#[tokio::test]
async fn spend_spike_compares_settled_spend_with_seven_completed_utc_days() {
    use aisix_control_plane::billing::alerts::reserve;
    use uuid::Uuid;

    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let saved = cp
        .send(
            "PUT",
            ROUTE,
            Some(owner.cookie()),
            Some(json!({"spend_spike_percent": 200, "email": owner.email})),
        )
        .await;
    assert_eq!(saved.status, 200, "{saved:?}");
    for (days_ago, amount) in [(0_i32, 2_000_000_i64), (7, 7_000_000), (8, 700_000_000)] {
        let event_id = format!("budget-alert-{days_ago}");
        sqlx::query(
            "WITH event AS (
              INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, event)
              VALUES ($1, $2, $2, now(), $3::uuid::text, '{}'::jsonb) RETURNING event_id
            ), charge AS (
              INSERT INTO pricing_charges_cny
                (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at,
                 status, charge_micro_cny, engine_version)
              SELECT event_id, $3::uuid::text, '', '', '', now(), 'charged', $4, 1 FROM event
              RETURNING event_id
            )
            INSERT INTO billing_ledger_cny
              (tenant_id, kind, amount_micro_cny, balance_after_micro_cny, event_id, posted_at)
            SELECT $3, 'usage', -$4, 0, event_id,
                   date_trunc('day', now(), 'UTC') - $5 * interval '24 hours'
            FROM charge",
        )
        .bind(event_id)
        .bind(Uuid::nil())
        .bind(tenant)
        .bind(amount)
        .bind(days_ago)
        .execute(cp.db().pool())
        .await
        .expect("record settled spend");
        if days_ago == 0 {
            assert!(reserve(cp.state(), tenant, true)
                .await
                .expect("no history")
                .is_empty());
        }
    }
    assert!(reserve(cp.state(), tenant, true)
        .await
        .expect("spend evaluation cooldown")
        .is_empty());
    sqlx::query(
        "UPDATE billing_alert_settings SET spend_checked_at = now() - interval '6 minutes'
         WHERE tenant_id = $1",
    )
    .bind(tenant)
    .execute(cp.db().pool())
    .await
    .expect("allow the next spend aggregation");
    let alerts = reserve(cp.state(), tenant, true)
        .await
        .expect("spend threshold");
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].kind, "spend_spike");
    assert_eq!(alerts[0].observed_micro_cny, 2_000_000);
    assert_eq!(alerts[0].threshold_micro_cny, 2_000_000);
}
