//! 版本行损坏的恢复不能复用另一控制面仍持有的价格版本。

mod support;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::pricing::store::{self, NewDiscount};
use chrono::Utc;
use futures::FutureExt;
use serde_json::json;
use sqlx::{Connection, PgConnection};
use support::world::{db_now, deliver, price, seed_model, tokens_680, usage, world};
use uuid::Uuid;

async fn write_discount(cp: &support::TestCp, tenant: Uuid) {
    let at = db_now(cp).await;
    cp.db()
        .tx(move |conn| {
            async move {
                store::insert_discount(
                    conn,
                    &NewDiscount {
                        tenant_id: tenant,
                        effective_from: at,
                        discount_bps: 0,
                    },
                    Actor::System,
                )
                .await
                .map(|_| ())
            }
            .boxed()
        })
        .await
        .expect("a price write repairs the version atomically");
}

async fn cached_version(cp: &support::TestCp) -> i64 {
    let mut conn = cp.db().pool().acquire().await.expect("price reader");
    cp.state()
        .prices
        .current_with_version(&mut conn)
        .await
        .expect("read the current book")
        .0
}

#[tokio::test]
async fn missing_and_malformed_versions_recover_beyond_both_control_planes_caches() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let peer = w.cp.restart().await;
    let mut previous = cached_version(&w.cp).await;
    assert_eq!(cached_version(&peer).await, previous);
    for bad in [
        None,
        Some("\"seven\""),
        Some("1.5"),
        Some("null"),
        Some("9223372036854775808"),
        Some("-9223372036854775809"),
    ] {
        if let Some(value) = bad {
            sqlx::query("UPDATE cp_settings SET value = $1::jsonb WHERE key = 'pricing_version'")
                .bind(value)
                .execute(w.cp.db().pool())
                .await
                .expect("corrupt the version");
        } else {
            sqlx::query("DELETE FROM cp_settings WHERE key = 'pricing_version'")
                .execute(w.cp.db().pool())
                .await
                .expect("remove the version");
        }
        let loads = (w.cp.state().prices.loads(), peer.state().prices.loads());
        write_discount(&w.cp, w.tenant).await;
        let mut conn = w.cp.db().pool().acquire().await.expect("version reader");
        let version = store::pricing_version(&mut conn)
            .await
            .expect("read repaired version")
            .expect("the writer must restore a reader-compatible version");
        assert!(
            version > previous,
            "{bad:?}: {version} must exceed cached {previous}"
        );
        assert_eq!(cached_version(&w.cp).await, version);
        assert_eq!(cached_version(&peer).await, version);
        assert_eq!(
            (w.cp.state().prices.loads(), peer.state().prices.loads()),
            (loads.0 + 1, loads.1 + 1)
        );
        assert_eq!(cached_version(&peer).await, version);
        assert_eq!(
            peer.state().prices.loads(),
            loads.1 + 1,
            "a healthy version stays cached"
        );
        previous = version;
    }
    // 滚动升级旧进程或手工合法写同样必须留下高水位。
    let high = previous + 100_000;
    sqlx::query(
        "UPDATE cp_settings SET value = to_jsonb($1::bigint) WHERE key = 'pricing_version'",
    )
    .bind(high)
    .execute(w.cp.db().pool())
    .await
    .expect("an old-style version write");
    sqlx::query("UPDATE cp_settings SET value = to_jsonb((value::text)::bigint + 1) WHERE key = 'pricing_version'")
        .execute(w.cp.db().pool()).await.expect("an old writer bumps the version");
    assert_eq!(cached_version(&w.cp).await, high + 1);
    assert_eq!(cached_version(&peer).await, high + 1);
    sqlx::query("UPDATE cp_settings SET value = '\"bad again\"' WHERE key = 'pricing_version'")
        .execute(w.cp.db().pool())
        .await
        .expect("corrupt after old-style writer");
    write_discount(&w.cp, w.tenant).await;
    assert!(cached_version(&w.cp).await > high + 1);
    assert_eq!(cached_version(&peer).await, cached_version(&w.cp).await);
    let sequence: i64 = sqlx::query_scalar("SELECT last_value FROM pricing_version_high_water")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read high water");
    let response = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(
            &w,
            "version-does-not-move-on-intake",
            model,
            tokens_680(),
        )],
    )
    .await;
    assert_eq!(
        (response.status, response.body),
        (200, json!({"accepted":1,"rejected":0,"duplicates":0}))
    );
    let after: i64 = sqlx::query_scalar("SELECT last_value FROM pricing_version_high_water")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("high water after intake");
    assert_eq!(
        after, sequence,
        "intake must never allocate a price version"
    );
}

#[tokio::test]
async fn concurrent_missing_version_repairs_serialize_and_rollback_keeps_the_row_atomic() {
    let Some(w) = world().await else { return };
    let before = cached_version(&w.cp).await;
    sqlx::query("DELETE FROM cp_settings WHERE key = 'pricing_version'")
        .execute(w.cp.db().pool())
        .await
        .expect("remove version");
    let mut first =
        w.cp.db()
            .pool()
            .begin()
            .await
            .expect("begin the first recovery");
    store::insert_discount(
        &mut first,
        &NewDiscount {
            tenant_id: w.tenant,
            effective_from: Utc::now(),
            discount_bps: 0,
        },
        Actor::System,
    )
    .await
    .expect("the first recovery writes an uncommitted version row");
    let first_version = store::pricing_version(&mut first)
        .await
        .expect("read first recovery")
        .expect("first recovery restores a version");
    let mut second = PgConnection::connect(w.cp.database_url())
        .await
        .expect("connect second writer");
    sqlx::query("BEGIN")
        .execute(&mut second)
        .await
        .expect("begin second writer");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut second)
        .await
        .expect("second writer pid");
    let tenant = w.tenant;
    let writer = tokio::spawn(async move {
        store::insert_discount(
            &mut second,
            &NewDiscount {
                tenant_id: tenant,
                effective_from: Utc::now(),
                discount_bps: 0,
            },
            Actor::System,
        )
        .await
        .expect("the second recovery succeeds after the first commits");
        sqlx::query("COMMIT")
            .execute(&mut second)
            .await
            .expect("commit second writer");
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock')")
                .bind(pid).fetch_one(w.cp.db().pool()).await.expect("observe the conflicting missing-row repair");
            if blocked { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("the missing-row upsert must really serialize the second writer");
    first.commit().await.expect("commit the first recovery");
    writer.await.expect("the second writer completes");
    let after = cached_version(&w.cp).await;
    assert!(first_version > before);
    assert_eq!(after, first_version + 1);
    assert_eq!(w.cp.count("pricing_discount_revisions").await, 2);
    let mut tx =
        w.cp.db()
            .pool()
            .begin()
            .await
            .expect("begin rolled-back writer");
    store::insert_discount(
        &mut tx,
        &NewDiscount {
            tenant_id: w.tenant,
            effective_from: Utc::now(),
            discount_bps: 0,
        },
        Actor::System,
    )
    .await
    .expect("write inside rollback");
    tx.rollback()
        .await
        .expect("rollback the revision and version");
    assert_eq!(cached_version(&w.cp).await, after);
    assert_eq!(w.cp.count("pricing_discount_revisions").await, 2);
    write_discount(&w.cp, w.tenant).await;
    assert_eq!(
        cached_version(&w.cp).await,
        after + 1,
        "healthy writes retain one-step versions despite sequence gaps"
    );
}

#[tokio::test]
async fn the_recovery_migration_enters_a_namespace_above_every_old_readable_version() {
    let Some(pool) = support::migrate::database_at(57).await else {
        return;
    };
    let old_max = 999_999_999_999_999_999_i64;
    sqlx::query(
        "UPDATE cp_settings SET value = to_jsonb($1::bigint) WHERE key = 'pricing_version'",
    )
    .bind(old_max)
    .execute(&pool)
    .await
    .expect("seed the largest old cached version");
    support::migrate::migrate_rest(&pool)
        .await
        .expect("upgrade the price-version namespace");
    let version: i64 = sqlx::query_scalar(
        "SELECT pricing_version_number(value) FROM cp_settings WHERE key = 'pricing_version'",
    )
    .fetch_one(&pool)
    .await
    .expect("read upgraded version");
    assert!(version > old_max);
    for (json, expected) in [
        ("9223372036854775807", Some(i64::MAX)),
        ("-9223372036854775808", Some(i64::MIN)),
        ("9223372036854775808", None),
        ("-9223372036854775809", None),
        ("100000000000000000000000000000000000", None),
        ("\"7\"", None),
        ("1.5", None),
        ("null", None),
        ("{}", None),
    ] {
        let parsed: Option<i64> = sqlx::query_scalar("SELECT pricing_version_number($1::jsonb)")
            .bind(json)
            .fetch_one(&pool)
            .await
            .expect("invalid numbers must never raise a cast error");
        assert_eq!(parsed, expected, "{json}");
    }
    support::migrate::drop_database(pool).await;
}
