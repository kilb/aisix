//! Pruning what is left behind: browser sessions past their expiry, the
//! rows of gateway instances not heard from for a week (a gateway mints a
//! new `instance_id` at every boot, so each restart leaves one behind), and
//! the tombstones of deleted gateway resources once their delete has been
//! projected for a day.
//!
//! Every instance runs the pass at boot and every 10 minutes after; the
//! deletes are idempotent, so instances running it together do no harm.

use std::time::Duration;

use futures::FutureExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::db::{Db, DbError};
use crate::resources::write::RESOURCES_LOCK_KEY;
use crate::stopped;

const PERIOD: Duration = Duration::from_secs(600);

/// How long a tombstone is kept once its delete has been projected.
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(86_400);

/// What one pass deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub sessions: u64,
    pub instances: u64,
    pub tombstones: u64,
}

/// One pass: deletes expired sessions, instances last seen more than 7
/// days ago, and tombstones projected more than a day ago.
pub async fn run_once(db: &Db) -> Result<Pruned, DbError> {
    let sessions = sqlx::query("DELETE FROM sessions WHERE expires_at <= now()")
        .execute(db.pool())
        .await?
        .rows_affected();
    let instances =
        sqlx::query("DELETE FROM dp_instances WHERE last_seen_at < now() - interval '7 days'")
            .execute(db.pool())
            .await?
            .rows_affected();
    let tombstones = prune_tombstones(db, TOMBSTONE_RETENTION).await?;
    Ok(Pruned {
        sessions,
        instances,
        tombstones,
    })
}

/// Runs a pass now and every 10 minutes until `shutdown` turns true (or its
/// sender is gone): a control plane that restarts more often than that
/// still prunes. A failed pass is logged and retried at the next one.
pub fn spawn(db: Db, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(PERIOD);
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stopped(&mut shutdown) => return,
                _ = every.tick() => {}
            }
            match run_once(&db).await {
                Ok(pruned) => tracing::debug!(?pruned, "housekeeping pass"),
                Err(e) => tracing::warn!(error = %e, "a housekeeping pass failed"),
            }
        }
    })
}

/// Deletes the tombstones of gateway resources whose delete was projected
/// and made more than `older_than` ago, and returns how many.
///
/// It runs under the resources lock, as every gateway resource write does:
/// a write may revive the tombstone it finds on its id (a singleton such as
/// `budget_policy` is re-created on the same id), and must not find it gone
/// halfway through. `updated_at` is when the delete was made, and
/// `projected_version = version` says it has reached kine since.
pub async fn prune_tombstones(db: &Db, older_than: Duration) -> Result<u64, DbError> {
    let older_than = older_than.as_secs_f64();
    db.tx(move |conn| {
        async move {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(RESOURCES_LOCK_KEY)
                .execute(&mut *conn)
                .await?;
            Ok(sqlx::query(
                "DELETE FROM gw_resources WHERE deleted AND projected_version = version \
                 AND updated_at < now() - make_interval(secs => $1)",
            )
            .bind(older_than)
            .execute(&mut *conn)
            .await?
            .rows_affected())
        }
        .boxed()
    })
    .await
}
