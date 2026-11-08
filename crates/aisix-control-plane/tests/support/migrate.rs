//! A fresh database at a chosen migration, so a test can seed the state an
//! upgrade finds and then apply the rest of the migrations to it.
//!
//! [`database_at`] creates a database of its own on the `TestCp` server and
//! migrates it through one version; [`migrate_rest`] applies every migration
//! after it, exactly as a boot does. The database is not dropped for the
//! test, so it can inspect it afterwards: [`drop_database`] does that.
//!
//! Both gate on `CP_TEST_DATABASE_URL` as `TestCp::start` does: unset, they
//! return early; unset on CI, they panic instead of skipping silently.

use std::borrow::Cow;

use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

use super::{execute, gate_var, with_database};

/// The crate's migrations, filtered to those at or below `version`.
fn migrator_through(version: i64) -> Migrator {
    let mut migrator = sqlx::migrate!("./migrations");
    let kept: Vec<_> = migrator
        .iter()
        .filter(|m| m.version <= version)
        .cloned()
        .collect();
    migrator.migrations = Cow::Owned(kept);
    migrator.set_ignore_missing(true);
    migrator
}

/// A fresh database on the `TestCp` server, migrated through `version` and
/// no further; `None` when `CP_TEST_DATABASE_URL` is unset (outside CI).
pub async fn database_at(version: i64) -> Option<PgPool> {
    let server_url = gate_var("CP_TEST_DATABASE_URL")?;
    let name = format!("t_{}", Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(&server_url)
        .await
        .expect("connect to CP_TEST_DATABASE_URL");
    execute(&mut admin, &format!("CREATE DATABASE \"{name}\"")).await;
    let _ = admin.close().await;
    let url = with_database(&server_url, &name);
    let mut conn = PgConnection::connect(&url)
        .await
        .expect("connect to the new database");
    // No timeouts: a migration is bounded by nothing here, as in a boot.
    migrator_through(version)
        .run(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("migrate through {version}: {e}"));
    let _ = conn.close().await;
    Some(
        PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .expect("open the new database"),
    )
}

/// Applies every migration after the one [`database_at`] stopped at, as a
/// boot does. `Err` is the migration that refused, which is how a migration
/// whose `DO` block raises fails.
pub async fn migrate_rest(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let mut conn = pool
        .acquire()
        .await
        .expect("a connection of the new database");
    // No `ignore_missing`, as a boot has none: a migration deleted from disk
    // after being applied fails here too.
    sqlx::migrate!("./migrations").run(&mut *conn).await
}

/// Closes `pool` and drops the database behind it.
pub async fn drop_database(pool: PgPool) {
    let Some(name) = pool.connect_options().get_database().map(str::to_string) else {
        return;
    };
    pool.close().await;
    let Some(server_url) = gate_var("CP_TEST_DATABASE_URL") else {
        return;
    };
    let mut admin = PgConnection::connect(&server_url)
        .await
        .expect("connect to CP_TEST_DATABASE_URL");
    execute(
        &mut admin,
        &format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"),
    )
    .await;
    let _ = admin.close().await;
}
