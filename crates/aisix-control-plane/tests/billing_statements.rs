//! The ledger read as statements: entries page newest first, a usage entry
//! names its model while the tenant may still use it, and the per-period
//! totals are added up from the entries when asked for.

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::tenants::TenantStatus;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use support::world::{deliver, price, seed_model, tokens_680, usage, world, World};
use support::TestCp;
use uuid::Uuid;

/// The ledger of `w`'s tenant as an operator reads it.
async fn admin_ledger(w: &World, query: &str) -> support::Resp {
    w.cp.send(
        "GET",
        &format!("/admin/tenants/{}/billing/ledger{query}", w.tenant),
        Some(w.admin.cookie()),
        None,
    )
    .await
}

async fn seqs(pool: &PgPool, tenant: Uuid) -> Vec<i64> {
    sqlx::query_scalar("SELECT seq FROM billing_ledger_cny WHERE tenant_id = $1 ORDER BY seq DESC")
        .bind(tenant)
        .fetch_all(pool)
        .await
        .expect("read the seqs")
}

fn seqs_of(r: &support::Resp) -> Vec<i64> {
    r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["seq"].as_i64().expect("a seq"))
        .collect()
}

#[tokio::test]
async fn the_ledger_pages_newest_first_without_gaps() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let events: Vec<Value> = (0..5)
        .map(|i| usage(&w, &format!("page-{i}"), model, tokens_680()))
        .collect();
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
    let all = seqs(w.cp.db().pool(), w.tenant).await;
    assert_eq!(all.len(), 5);

    let mut seen = Vec::new();
    let mut before: Option<i64> = None;
    let mut pages = 0;
    loop {
        let query = match before {
            Some(b) => format!("?limit=2&before={b}"),
            None => "?limit=2".to_string(),
        };
        let r = admin_ledger(&w, &query).await;
        assert_eq!(r.status, 200, "{r:?}");
        pages += 1;
        seen.extend(seqs_of(&r));
        match r.body["next_before"].as_i64() {
            Some(next) => {
                assert_eq!(Some(&next), seqs_of(&r).last(), "{r:?}");
                before = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, all, "every entry once, newest first");

    // The kind filter and the item shape.
    let r = admin_ledger(&w, "?kind=topup").await;
    assert_eq!(r.body, json!({"items": [], "next_before": null}), "{r:?}");
    let r = admin_ledger(&w, "?limit=1").await;
    let item = &r.body["items"][0];
    assert_eq!(item["kind"], "usage");
    assert_eq!(item["amount_micro_cny"], -680);
    assert_eq!(item["balance_after_micro_cny"], -3400);
    assert_eq!(item["event_id"], "page-4");
    assert_eq!(item["topup_id"], Value::Null);
    let r = admin_ledger(&w, "?kind=grant").await;
    assert_eq!(r.status, 422, "{r:?}");
}

#[tokio::test]
async fn a_ledger_item_hides_an_ungranted_models_id() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    sqlx::query("UPDATE tenants SET allowed_model_ids = $2 WHERE id = $1")
        .bind(w.tenant)
        .bind(vec![model])
        .execute(pool)
        .await
        .expect("grant the model");
    let named = usage(&w, "named", model, {
        let mut f = tokens_680();
        f["requested_model"] = json!("chat-bill");
        f
    });
    let unnamed = usage(&w, "unnamed", model, tokens_680());
    let r = deliver(&w.dp, Uuid::new_v4(), vec![named, unnamed]).await;
    assert_eq!(r.status, 200, "{r:?}");
    let display: String =
        sqlx::query_scalar("SELECT doc->>'display_name' FROM gw_resources WHERE id = $1")
            .bind(model)
            .fetch_one(pool)
            .await
            .expect("the display name");

    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let read = || async {
        let r =
            w.cp.send("GET", "/api/billing/ledger", Some(owner.cookie()), None)
                .await;
        assert_eq!(r.status, 200, "{r:?}");
        let models: Vec<Value> = r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["model"].clone())
            .collect();
        models
    };
    // Newest first: the unnamed event, then the named one.
    assert_eq!(
        read().await,
        vec![
            json!({"id": model.to_string(), "name": display}),
            json!({"id": model.to_string(), "name": "chat-bill"}),
        ]
    );

    sqlx::query("UPDATE tenants SET allowed_model_ids = '{}' WHERE id = $1")
        .bind(w.tenant)
        .execute(pool)
        .await
        .expect("ungrant the model");
    // The name the caller sent stays; the model's current name is the
    // tenant's to read only while it is granted, as in the call log.
    assert_eq!(
        read().await,
        vec![
            json!({"id": null, "name": "未知模型"}),
            json!({"id": null, "name": "chat-bill"}),
        ]
    );
}

#[tokio::test]
async fn a_ledger_item_never_names_a_model_by_its_id() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    raw_usage(pool, w.tenant, "by-id", -100, -100, Utc::now()).await;
    // A charge recording the id of a model that is gone, whose event carried
    // no name.
    let gone = Uuid::new_v4();
    sqlx::query("UPDATE pricing_charges_cny SET requested_model_id = $2 WHERE event_id = $1")
        .bind("by-id")
        .bind(gone.to_string())
        .execute(pool)
        .await
        .expect("record the model id");
    let r = admin_ledger(&w, "?kind=usage").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["items"][0]["model"],
        json!({"id": null, "name": "未知模型"}),
        "{r:?}"
    );
}

/// Posts a usage entry by raw INSERT, with the event and charge it names.
async fn raw_usage(
    pool: &PgPool,
    tenant: Uuid,
    event_id: &str,
    amount: i64,
    balance_after: i64,
    posted_at: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, event) \
         VALUES ($1, gen_random_uuid(), gen_random_uuid(), $2, $3, '{}')",
    )
    .bind(event_id)
    .bind(posted_at)
    .bind(tenant.to_string())
    .execute(pool)
    .await
    .expect("insert the event");
    sqlx::query(
        "INSERT INTO pricing_charges_cny (event_id, tenant_id, api_key_id, user_id, \
         requested_model_id, occurred_at, status, charge_micro_cny, engine_version) \
         VALUES ($1, $2, '', '', '', $3, 'charged', $4, 1)",
    )
    .bind(event_id)
    .bind(tenant.to_string())
    .bind(posted_at)
    .bind(-amount)
    .execute(pool)
    .await
    .expect("insert the charge");
    sqlx::query(
        "INSERT INTO billing_ledger_cny (tenant_id, kind, amount_micro_cny, balance_after_micro_cny, \
         event_id, posted_at) VALUES ($1, 'usage', $2, $3, $4, $5)",
    )
    .bind(tenant)
    .bind(amount)
    .bind(balance_after)
    .bind(event_id)
    .bind(posted_at)
    .execute(pool)
    .await
    .expect("insert the entry");
}

fn rfc(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[tokio::test]
async fn a_ledger_summary_adds_up_by_utc_day() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    let today = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc();
    let day_before = today - TimeDelta::days(2);
    // The last second of one day and the first second of the next.
    raw_usage(
        pool,
        w.tenant,
        "sum-a",
        -100,
        -100,
        day_before + TimeDelta::seconds(86_399),
    )
    .await;
    raw_usage(
        pool,
        w.tenant,
        "sum-b",
        -200,
        -300,
        today - TimeDelta::days(1),
    )
    .await;
    sqlx::query("UPDATE billing_accounts SET balance_micro_cny = -300 WHERE tenant_id = $1")
        .bind(w.tenant)
        .execute(pool)
        .await
        .expect("set the balance");
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "sum-c", model, tokens_680())],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");

    let path = format!(
        "/admin/tenants/{}/billing/summary?group=day&from={}&to={}",
        w.tenant,
        rfc(day_before),
        rfc(today + TimeDelta::days(1))
    );
    let r = w.cp.send("GET", &path, Some(w.admin.cookie()), None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let row = |start: DateTime<Utc>, usage: i64, closing: i64| {
        json!({
            "period_start": rfc(start),
            "usage_micro_cny": usage,
            "topups_micro_cny": 0,
            "adjustments_micro_cny": 0,
            "closing_balance_micro_cny": closing,
        })
    };
    assert_eq!(
        r.body["items"],
        json!([
            row(day_before, -100, -100),
            row(today - TimeDelta::days(1), -200, -300),
            row(today, -680, -980),
        ]),
        "{r:?}"
    );

    // By month, the same entries fall into one or two months, and the sums
    // match the entries in each.
    let r =
        w.cp.send(
            "GET",
            &format!(
                "/admin/tenants/{}/billing/summary?group=month&from={}&to={}",
                w.tenant,
                rfc(day_before),
                rfc(today + TimeDelta::days(1))
            ),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    assert!((1..=2).contains(&items.len()), "{r:?}");
    let total: i64 = items
        .iter()
        .map(|i| i["usage_micro_cny"].as_i64().expect("usage"))
        .sum();
    assert_eq!(total, -980);
    assert_eq!(
        items.last().expect("a month")["closing_balance_micro_cny"],
        -980
    );
}

#[tokio::test]
async fn a_summary_range_is_bounded_on_the_route() {
    let Some(w) = world().await else { return };
    let from = "2026-01-01T00:00:00Z";
    let (cp, admin) = (&w.cp, &w.admin);
    let get = |group: &str, to: &str| {
        let path = format!(
            "/admin/tenants/{}/billing/summary?group={group}&from={from}&to={to}",
            w.tenant
        );
        async move { cp.send("GET", &path, Some(admin.cookie()), None).await }
    };
    // 92 days after 2026-01-01 is 2026-04-03.
    let r = get("day", "2026-04-03T00:00:00Z").await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = get("day", "2026-04-04T00:00:00Z").await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/to", "{r:?}");
    let r = get("month", "2028-01-03T00:00:00Z").await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/to", "{r:?}");
    let r = get("day", "2025-12-31T00:00:00Z").await;
    assert_eq!(r.status, 422, "{r:?}");
    // A missing group is refused too.
    let r =
        w.cp.send(
            "GET",
            &format!(
                "/admin/tenants/{}/billing/summary?from={from}&to=2026-01-02T00:00:00Z",
                w.tenant
            ),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
}

#[tokio::test]
async fn operators_read_any_tenants_ledger_and_summaries() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let range = "group=day&from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z";
    for path in [
        format!("/admin/tenants/{tenant}/billing/ledger"),
        format!("/admin/tenants/{tenant}/billing/summary?{range}"),
    ] {
        let r = cp.send("GET", &path, Some(viewer.cookie()), None).await;
        assert_eq!(r.status, 200, "{path}: {r:?}");
    }
    for path in [
        format!("/admin/tenants/{}/billing/ledger", Uuid::new_v4()),
        format!("/admin/tenants/{}/billing/summary?{range}", Uuid::new_v4()),
        "/admin/tenants/not-a-uuid/billing/ledger".to_string(),
    ] {
        let r = cp.send("GET", &path, Some(viewer.cookie()), None).await;
        assert_eq!(r.status, 404, "{path}: {r:?}");
    }
}

#[tokio::test]
async fn members_cannot_read_the_ledger() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let range = "group=day&from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z";
    let paths = [
        "/api/billing/ledger".to_string(),
        format!("/api/billing/summary?{range}"),
    ];
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let billing = cp.member_login_seeded(tenant, MemberRole::Billing).await;
    for path in &paths {
        let r = cp.send("GET", path, Some(member.cookie()), None).await;
        assert_eq!(
            (r.status, &r.body["code"]),
            (403, &json!("forbidden")),
            "{path}: {r:?}"
        );
        let r = cp.send("GET", path, Some(billing.cookie()), None).await;
        assert_eq!(r.status, 200, "{path}: {r:?}");
    }
}

#[tokio::test]
async fn a_tenant_reads_only_its_own_entries() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "mine", model, tokens_680())],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let other = w.cp.seed_tenant(TenantStatus::Active).await;
    let owner = w.cp.member_login_seeded(other, MemberRole::Owner).await;
    let r =
        w.cp.send("GET", "/api/billing/ledger", Some(owner.cookie()), None)
            .await;
    assert_eq!(r.body, json!({"items": [], "next_before": null}), "{r:?}");
}

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .expect("a time")
        .with_timezone(&Utc)
}

/// Posts a top-up or an adjustment entry by raw INSERT. The foreign keys to
/// top-ups and re-pricing runs are switched off for the transaction, since
/// the tables behind them are not what these tests are about.
async fn raw_entry(
    pool: &PgPool,
    tenant: Uuid,
    kind: &str,
    amount: i64,
    balance_after: i64,
    posted_at: DateTime<Utc>,
) {
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .expect("relax the foreign keys");
    sqlx::query(
        "INSERT INTO billing_ledger_cny (tenant_id, kind, amount_micro_cny, balance_after_micro_cny, \
         topup_id, repricing_run_id, posted_at) \
         VALUES ($1, $2, $3, $4, CASE WHEN $2 = 'topup' THEN gen_random_uuid() END, \
                 CASE WHEN $2 = 'adjustment' THEN gen_random_uuid() END, $5)",
    )
    .bind(tenant)
    .bind(kind)
    .bind(amount)
    .bind(balance_after)
    .bind(posted_at)
    .execute(&mut *tx)
    .await
    .expect("insert the entry");
    tx.commit().await.expect("commit");
}

#[tokio::test]
async fn a_month_cut_by_the_range_closes_at_its_last_entry_inside_it() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    raw_usage(
        pool,
        w.tenant,
        "cut-a",
        -100,
        -100,
        at("2026-01-10T12:00:00Z"),
    )
    .await;
    raw_usage(
        pool,
        w.tenant,
        "cut-b",
        -200,
        -300,
        at("2026-01-20T12:00:00Z"),
    )
    .await;
    raw_usage(
        pool,
        w.tenant,
        "cut-c",
        -50,
        -350,
        at("2026-01-25T12:00:00Z"),
    )
    .await;
    let summary = |to: &str| {
        let path = format!(
            "/admin/tenants/{}/billing/summary?group=month&from=2026-01-01T00:00:00Z&to={to}",
            w.tenant
        );
        let (cp, admin) = (&w.cp, &w.admin);
        async move { cp.send("GET", &path, Some(admin.cookie()), None).await }
    };
    let r = summary("2026-01-22T00:00:00Z").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["items"].as_array().map(Vec::len), Some(1), "{r:?}");
    assert_eq!(r.body["items"][0]["usage_micro_cny"], -300, "{r:?}");
    assert_eq!(
        r.body["items"][0]["closing_balance_micro_cny"], -300,
        "{r:?}"
    );
    let r = summary("2026-02-01T00:00:00Z").await;
    assert_eq!(r.body["items"][0]["usage_micro_cny"], -350, "{r:?}");
    assert_eq!(
        r.body["items"][0]["closing_balance_micro_cny"], -350,
        "{r:?}"
    );
}

#[tokio::test]
async fn topups_and_adjustments_are_listed_and_summed_beside_usage() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    raw_usage(
        pool,
        w.tenant,
        "mix-a",
        -100,
        -100,
        at("2026-02-03T10:00:00Z"),
    )
    .await;
    raw_entry(
        pool,
        w.tenant,
        "topup",
        500,
        400,
        at("2026-02-03T11:00:00Z"),
    )
    .await;
    raw_entry(
        pool,
        w.tenant,
        "adjustment",
        -7,
        393,
        at("2026-02-03T12:00:00Z"),
    )
    .await;

    let r = admin_ledger(&w, "?kind=topup").await;
    assert_eq!(r.status, 200, "{r:?}");
    let item = &r.body["items"][0];
    assert_eq!(item["kind"], "topup");
    assert_eq!(item["model"], Value::Null);
    assert!(
        item["topup_id"].is_string() && item["event_id"].is_null(),
        "{r:?}"
    );
    let r = admin_ledger(&w, "?kind=adjustment").await;
    assert!(r.body["items"][0]["repricing_run_id"].is_string(), "{r:?}");
    assert_eq!(r.body["items"][0]["model"], Value::Null);

    // The usage entry names no model anywhere: it is never shown unnamed.
    let r = admin_ledger(&w, "?kind=usage").await;
    assert_eq!(
        r.body["items"][0]["model"],
        json!({"id": null, "name": "未知模型"}),
        "{r:?}"
    );

    // A from/to window on the listing.
    let r = admin_ledger(&w, "?from=2026-02-03T10:30:00Z&to=2026-02-03T11:30:00Z").await;
    assert_eq!(r.body["items"].as_array().map(Vec::len), Some(1), "{r:?}");
    assert_eq!(r.body["items"][0]["kind"], "topup");

    let r =
        w.cp.send(
            "GET",
            &format!(
                "/admin/tenants/{}/billing/summary?group=day\
                 &from=2026-02-01T00:00:00Z&to=2026-02-10T00:00:00Z",
                w.tenant
            ),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(
        r.body["items"],
        json!([{
            "period_start": "2026-02-03T00:00:00Z",
            "usage_micro_cny": -100,
            "topups_micro_cny": 500,
            "adjustments_micro_cny": -7,
            "closing_balance_micro_cny": 393,
        }]),
        "{r:?}"
    );
}
