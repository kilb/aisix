//! Moving a database from USD billing to CNY (`0027_billing_cny`).
//!
//! The migration is additive and refuses, rather than converts, a database
//! that holds customer money in USD: there is no exchange rate anywhere in
//! this platform. What it always does is fence every USD money table against
//! new rows, give the money path its `_cny` twins, and fund the platform's own
//! tenant postpaid.
//!
//! Each case seeds a database at migration 26 and then applies the rest, as a
//! boot does. `support::migrate` gates on `CP_TEST_DATABASE_URL` as
//! `TestCp::start` does.

mod support;

use sqlx::PgPool;
use support::migrate::{database_at, drop_database, migrate_rest};
use support::TestCp;
use uuid::Uuid;

/// The last migration before the money path moved to CNY.
const AT_USD: i64 = 26;

/// The tenancy the platform's own usage is filed under: created here, since a
/// database migrated only through `0006` has no boot to seed it. Its account
/// is the one `0014` opens for every tenant.
async fn internal_tenant(db: &PgPool) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO tenants (id, name, status, is_internal, billing_mode) \
         VALUES ($1, '平台内部', 'active', true, 'prepaid') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(db)
    .await
    .expect("the platform's own tenant");
    sqlx::query("INSERT INTO billing_accounts (tenant_id) VALUES ($1)")
        .bind(id)
        .execute(db)
        .await
        .expect("open its account");
    id
}

/// A customer tenant, active and past pending.
async fn customer_tenant(db: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tenants (id, name, status, is_internal, billing_mode, approved_at) \
         VALUES ($1, $2, 'active', false, 'prepaid', now())",
    )
    .bind(id)
    .bind(name)
    .execute(db)
    .await
    .expect("insert a customer");
    sqlx::query("INSERT INTO billing_accounts (tenant_id) VALUES ($1)")
        .bind(id)
        .execute(db)
        .await
        .expect("open its account");
    id
}

/// One usage row, its USD charge and its ledger entry for `tenant_id`, as a
/// database at 26 that has been serving traffic holds them.
async fn usd_usage(db: &PgPool, tenant_id: Uuid, event_id: &str, charge: i64) {
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event) \
         VALUES ($1, $2, $2, now(), $3::text, '', '{}')",
    )
    .bind(event_id)
    .bind(Uuid::new_v4())
    .bind(tenant_id.to_string())
    .execute(db)
    .await
    .expect("insert the usage row");
    sqlx::query(
        "INSERT INTO pricing_charges \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, price_instant, \
          status, charge_micro_usd, engine_version) \
         VALUES ($1, $2::text, '', '', '', now(), now(), 'charged', $3, 1)",
    )
    .bind(event_id)
    .bind(tenant_id.to_string())
    .bind(charge)
    .execute(db)
    .await
    .expect("insert the USD charge");
    sqlx::query(
        "INSERT INTO billing_ledger (tenant_id, kind, amount_micro_usd, balance_after_micro_usd, \
          event_id) \
         VALUES ($1, 'usage', $2, $2, $3)",
    )
    .bind(tenant_id)
    .bind(-charge)
    .bind(event_id)
    .execute(db)
    .await
    .expect("post the USD entry");
}

/// The row count of `table`, or -1 when it does not exist.
async fn count(db: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(db)
        .await
        .unwrap_or(-1)
}

#[tokio::test]
async fn a_platform_with_only_internal_usd_history_migrates() {
    let Some(db) = database_at(AT_USD).await else {
        return;
    };
    let internal = internal_tenant(&db).await;
    // The platform's own history: its charges and its ledger entries.
    usd_usage(&db, internal, "e-internal-1", 680).await;
    usd_usage(&db, internal, "e-internal-2", 680).await;
    // A customer with one usage row that was never priced and no money.
    let customer = customer_tenant(&db, "migrating-7c1").await;
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event) \
         VALUES ('e-customer-1', $1, $1, now(), $2::text, '', '{}')",
    )
    .bind(Uuid::new_v4())
    .bind(customer.to_string())
    .execute(&db)
    .await
    .expect("insert the unpriced row");

    migrate_rest(&db)
        .await
        .expect("a platform with no customer money migrates");

    // The USD rows are untouched; the CNY tables exist and are empty.
    assert_eq!(count(&db, "pricing_charges").await, 2);
    assert_eq!(count(&db, "billing_ledger").await, 2);
    assert_eq!(count(&db, "pricing_charges_cny").await, 0);
    assert_eq!(count(&db, "billing_ledger_cny").await, 0);
    assert_eq!(count(&db, "billing_topups_cny").await, 0);
    assert_eq!(count(&db, "pricing_adjustments_cny").await, 0);
    assert_eq!(count(&db, "billing_repricing_runs_cny").await, 0);

    // The platform funds itself postpaid (R2), and its balance starts at 0.
    let mode: String = sqlx::query_scalar("SELECT billing_mode FROM tenants WHERE is_internal")
        .fetch_one(&db)
        .await
        .expect("read its mode");
    assert_eq!(mode, "postpaid");
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id = $1")
            .bind(internal)
            .fetch_one(&db)
            .await
            .expect("read its balance");
    assert_eq!(balance, 0);

    // A customer account past pending is flagged, so its first balance mark is
    // written from the CNY columns rather than the old ones.
    let dirty: bool =
        sqlx::query_scalar("SELECT marks_dirty FROM billing_accounts WHERE tenant_id = $1")
            .bind(customer)
            .fetch_one(&db)
            .await
            .expect("read the flag");
    assert!(dirty, "a customer account is marked for the settler");
    drop_database(db).await;
}

#[tokio::test]
async fn customer_usd_money_refuses_the_upgrade_and_names_the_counts() {
    // Each of the four counts on its own: a ledger entry, a pending top-up, a
    // live quota and a credit limit.
    for (what, expected_fragment) in [
        ("ledger", "tenants with ledger entries: 1"),
        ("topup", "pending top-ups: 1"),
        ("quota", "live quotas: 1"),
        ("credit", "credit limits: 1"),
    ] {
        let Some(db) = database_at(AT_USD).await else {
            return;
        };
        let customer = customer_tenant(&db, &format!("usd-{what}-7c1")).await;
        match what {
            "ledger" => {
                usd_usage(&db, customer, "e-ledger-1", 680).await;
            }
            "topup" => {
                // A request by one of the tenant's own members: a `tenant`
                // origin row is the only kind that can be pending, and its
                // `requested_by` is required.
                let member = sqlx::query_scalar::<_, Uuid>(
                    "INSERT INTO members (id, tenant_id, email, password_hash, role) \
                     VALUES ($1, $2, $3, 'x', 'owner') RETURNING id",
                )
                .bind(Uuid::new_v4())
                .bind(customer)
                .bind(format!("topup-{}@tenant.test", Uuid::new_v4().simple()))
                .fetch_one(&db)
                .await
                .expect("a member");
                sqlx::query(
                    "INSERT INTO billing_topups (id, tenant_id, amount_micro_usd, origin, status, \
                      requested_by) \
                     VALUES ($1, $2, 10000, 'tenant', 'pending', $3)",
                )
                .bind(Uuid::new_v4())
                .bind(customer)
                .bind(member)
                .execute(&db)
                .await
                .expect("a pending top-up");
            }
            "quota" => {
                sqlx::query(
                    "INSERT INTO billing_budgets \
                     (id, tenant_id, scope, scope_ref, set_by, kind, counted_from, \
                      limit_micro_usd, created_by_kind) \
                     VALUES ($1, $2, 'team', $2, 'operator', 'cumulative', now(), 10000, 'system')",
                )
                .bind(Uuid::new_v4())
                .bind(customer)
                .execute(&db)
                .await
                .expect("a live quota");
            }
            "credit" => {
                sqlx::query(
                    "UPDATE billing_accounts SET credit_limit_micro_usd = 1 WHERE tenant_id = $1",
                )
                .bind(customer)
                .execute(&db)
                .await
                .expect("a credit limit");
            }
            other => panic!("no case {other}"),
        }

        let e = migrate_rest(&db)
            .await
            .expect_err("a database holding customer USD money refuses the upgrade");
        let message = e.to_string();
        assert!(
            message.contains("money is CNY from this release and is never converted"),
            "{what}: {message}"
        );
        assert!(message.contains(expected_fragment), "{what}: {message}");
        // It fails before it writes anything: the CNY tables do not exist.
        assert_eq!(count(&db, "pricing_charges_cny").await, -1, "{what}");
        assert_eq!(count(&db, "billing_ledger_cny").await, -1, "{what}");
        // And the USD tables take rows as they did: no fence was installed.
        let operator = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO operators (id, email, password_hash, role) \
             VALUES ($1, $2, 'x', 'operator') RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(format!("usd-{}@ops.test", Uuid::new_v4().simple()))
        .fetch_one(&db)
        .await
        .expect("an operator");
        sqlx::query(
            "INSERT INTO billing_topups (id, tenant_id, amount_micro_usd, origin, status, \
              requested_by, credit_reason, decided_by, decided_at) \
             VALUES ($1, $2, 10000, 'operator', 'approved', NULL, 'the fence check', $3, now())",
        )
        .bind(Uuid::new_v4())
        .bind(customer)
        .bind(operator)
        .execute(&db)
        .await
        .unwrap_or_else(|e| panic!("{what}: the USD table still takes a row: {e}"));
        drop_database(db).await;
    }
}

/// `sql` is refused as a fenced table's insert: the error names the table and
/// says it takes no new rows.
async fn expect_refused(db: &PgPool, table: &str, sql: &str) {
    let e = sqlx::query(sql)
        .execute(db)
        .await
        .expect_err(&format!("{table} took a new USD row"));
    assert!(e.to_string().contains("takes no new rows"), "{table}: {e}");
}

#[tokio::test]
async fn the_fences_refuse_every_usd_money_table_and_the_discount_still_inserts() {
    let Some(db) = database_at(AT_USD).await else {
        return;
    };
    let internal = internal_tenant(&db).await;
    let customer = customer_tenant(&db, "fenced-2-7c1").await;
    migrate_rest(&db).await.expect("the migration runs");
    let event = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event) \
         VALUES ($1, $2, $2, now(), $3::text, '', '{}')",
    )
    .bind(&event)
    .bind(Uuid::new_v4())
    .bind(internal.to_string())
    .execute(&db)
    .await
    .expect("a usage row");
    let topup = Uuid::new_v4();
    let run = Uuid::new_v4();
    let operator = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO operators (id, email, password_hash, role) \
         VALUES ($1, $2, 'x', 'operator') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(format!("fence-{}@ops.test", Uuid::new_v4().simple()))
    .fetch_one(&db)
    .await
    .expect("an operator");

    expect_refused(
        &db,
        "pricing_charges",
        &format!(
            "INSERT INTO pricing_charges \
             (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, status, \
              charge_micro_usd, engine_version) \
             VALUES ('{event}', '', '', '', '', now(), 'charged', 0, 1)"
        ),
    )
    .await;
    expect_refused(
        &db,
        "billing_ledger",
        &format!(
            "INSERT INTO billing_ledger (tenant_id, kind, amount_micro_usd, balance_after_micro_usd, \
              event_id) VALUES ('{internal}', 'usage', -1, -1, '{event}')"
        ),
    )
    .await;
    expect_refused(
        &db,
        "billing_topups",
        &format!(
            "INSERT INTO billing_topups (id, tenant_id, amount_micro_usd, status, requested_by) \
             VALUES ('{topup}', '{customer}', 10000, 'pending', NULL)"
        ),
    )
    .await;
    expect_refused(
        &db,
        "billing_repricing_runs",
        &format!(
            "INSERT INTO billing_repricing_runs \
             (id, status, filter, reason, pricing_version, engine_version, events, changed, \
              still_unpriced, skipped_unattributed, delta_micro_usd, by_tenant, created_by) \
             VALUES ('{run}', 'previewed', '{{}}', 'r', 1, 1, 0, 0, 0, 0, 0, '[]', '{operator}')"
        ),
    )
    .await;
    expect_refused(
        &db,
        "pricing_adjustments",
        &format!(
            "INSERT INTO pricing_adjustments \
             (run_id, event_id, delta_micro_usd, engine_version, reason, created_by) \
             VALUES ('{run}', '{event}', 1, 1, 'r', '{operator}')"
        ),
    )
    .await;
    expect_refused(
        &db,
        "pricing_sell_revisions",
        &format!(
            "INSERT INTO pricing_sell_revisions \
             (engine_version, model_id, effective_from, card, source) \
             VALUES (1, '{}', now(), '{{}}', 'manual')",
            Uuid::new_v4()
        ),
    )
    .await;

    // `pricing_discount_revisions` is not fenced: the discount is kept.
    sqlx::query(
        "INSERT INTO pricing_discount_revisions \
         (engine_version, tenant_id, effective_from, discount_bps) VALUES (2, $1, now(), 500)",
    )
    .bind(customer)
    .execute(&db)
    .await
    .expect("a discount revision still inserts");
    drop_database(db).await;
}

#[tokio::test]
async fn the_cny_ledger_and_adjustments_are_append_only() {
    let Some(db) = database_at(AT_USD).await else {
        return;
    };
    let internal = internal_tenant(&db).await;
    migrate_rest(&db).await.expect("the migration runs");
    // One entry and one adjustment, written by raw SQL: nothing else is needed
    // to know what the two tables do to an UPDATE, a DELETE or a TRUNCATE.
    let event = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event) \
         VALUES ($1, $2, $2, now(), $3::text, '', '{}')",
    )
    .bind(&event)
    .bind(Uuid::new_v4())
    .bind(internal.to_string())
    .execute(&db)
    .await
    .expect("a usage row");
    sqlx::query(
        "INSERT INTO pricing_charges_cny \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, price_instant, \
          status, charge_micro_cny, engine_version) \
         VALUES ($1, $2::text, '', '', '', now(), now(), 'charged', 680, 2)",
    )
    .bind(&event)
    .bind(internal.to_string())
    .execute(&db)
    .await
    .expect("a CNY charge");
    sqlx::query(
        "INSERT INTO billing_ledger_cny (tenant_id, kind, amount_micro_cny, balance_after_micro_cny, \
          event_id) VALUES ($1, 'usage', -680, -680, $2)",
    )
    .bind(internal)
    .bind(&event)
    .execute(&db)
    .await
    .expect("a CNY entry");
    let operator = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO operators (id, email, password_hash, role) \
         VALUES ($1, $2, 'x', 'operator') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(format!("append-{}@ops.test", Uuid::new_v4().simple()))
    .fetch_one(&db)
    .await
    .expect("an operator");
    let run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_repricing_runs_cny \
         (id, status, filter, reason, pricing_version, engine_version, events, changed, \
          still_unpriced, skipped_unattributed, delta_micro_cny, by_tenant, created_by, \
          applied_by, applied_at) \
         VALUES ($1, 'applied', '{}', 'r', 1, 2, 1, 1, 0, 0, 1, '[]', $2, $2, now())",
    )
    .bind(run)
    .bind(operator)
    .execute(&db)
    .await
    .expect("a run");
    sqlx::query(
        "INSERT INTO pricing_adjustments_cny \
         (run_id, event_id, delta_micro_cny, engine_version, reason, created_by) \
         VALUES ($1, $2, 1, 2, 'r', $3)",
    )
    .bind(run)
    .bind(&event)
    .bind(operator)
    .execute(&db)
    .await
    .expect("an adjustment");

    for sql in [
        "UPDATE billing_ledger_cny SET amount_micro_cny = -1",
        "DELETE FROM billing_ledger_cny",
        // A ledger entry references the charge it was posted for, so the
        // truncate names both: on its own Postgres refuses it for the key.
        // An adjustment and a ledger entry reference the charge they are
        // about, so the truncate carries to them; with the whole chain named,
        // what refuses it is the trigger.
        "TRUNCATE billing_ledger_cny CASCADE",
        "UPDATE pricing_adjustments_cny SET delta_micro_cny = 1",
        "DELETE FROM pricing_adjustments_cny",
        "TRUNCATE pricing_adjustments_cny CASCADE",
    ] {
        let e = sqlx::query(sql).execute(&db).await.expect_err(sql);
        assert!(e.to_string().contains("append-only"), "{sql}: {e}");
    }
    assert_eq!(count(&db, "billing_ledger_cny").await, 1);
    assert_eq!(count(&db, "pricing_adjustments_cny").await, 1);
    drop_database(db).await;
}

#[tokio::test]
async fn an_event_stored_before_the_upgrade_is_not_priced_again() {
    // Review Focus 2. A row priced before the migration keeps its USD charge,
    // and the CNY table has no row for it, so nothing prices it a second time;
    // after the upgrade a new row is charged in CNY. The world here is
    // migrated to CNY, so the "before" state is the USD charge written by raw
    // SQL into the fenced table, which is what one looks like once the
    // migration has run.
    if TestCp::start().await.is_none() {
        return;
    }
    let w = support::world::world().await.expect("a world");
    let model = support::world::seed_model(&w.cp, &w.admin).await;
    support::world::price(&w.cp, model).await;
    // e1 was stored and charged in USD before the upgrade.
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event) \
         VALUES ('e1', $1, $1, now(), $2::text, $3::text, '{}')",
    )
    .bind(Uuid::new_v4())
    .bind(w.tenant.to_string())
    .bind(&w.key)
    .execute(w.cp.db().pool())
    .await
    .expect("store the old row");
    // The fence is off in this database because the migration created the
    // table; a USD charge is written by disabling it for one statement, which
    // is exactly the state a database at 26 carries into the upgrade.
    sqlx::query("ALTER TABLE pricing_charges DISABLE TRIGGER pricing_charges_usd_fenced")
        .execute(w.cp.db().pool())
        .await
        .expect("disable the fence");
    sqlx::query(
        "INSERT INTO pricing_charges \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, price_instant, \
          status, charge_micro_usd, engine_version) \
         VALUES ('e1', $1::text, $2::text, $3::text, $4::text, now(), now(), 'charged', 680, 1)",
    )
    .bind(w.tenant.to_string())
    .bind(&w.key)
    .bind(w.member.to_string())
    .bind(model.to_string())
    .execute(w.cp.db().pool())
    .await
    .expect("charge the old row in USD");
    sqlx::query("ALTER TABLE pricing_charges ENABLE TRIGGER pricing_charges_usd_fenced")
        .execute(w.cp.db().pool())
        .await
        .expect("restore the fence");
    // A batch with the old row again and a new one.
    let r = support::world::deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            support::world::usage(&w, "e1", model, support::world::tokens_680()),
            support::world::usage(&w, "e2", model, support::world::tokens_680()),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let cny: Option<i64> = sqlx::query_scalar(
        "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = 'e1'",
    )
    .fetch_optional(w.cp.db().pool())
    .await
    .expect("read e1's CNY charge");
    assert_eq!(
        cny, None,
        "a row stored before the upgrade is not priced again"
    );
    let fresh: Option<i64> = sqlx::query_scalar(
        "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = 'e2'",
    )
    .fetch_optional(w.cp.db().pool())
    .await
    .expect("read e2's CNY charge");
    assert_eq!(fresh, Some(680), "the new row is charged");
    let entries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_ledger_cny WHERE tenant_id = $1 AND event_id = 'e2'",
    )
    .bind(w.tenant)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("count the entries");
    assert_eq!(entries, 1);
}
