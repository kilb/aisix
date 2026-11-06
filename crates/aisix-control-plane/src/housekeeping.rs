//! Pruning what is left behind: browser sessions past their expiry, and the
//! rows of gateway instances not heard from for a week (a gateway mints a
//! new `instance_id` at every boot, so each restart leaves one behind).
//!
//! Every instance runs the pass, every 10 minutes from boot; the deletes
//! are idempotent, so instances running it together do no harm.

use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::db::{Db, DbError};
use crate::stopped;

const PERIOD: Duration = Duration::from_secs(600);

/// What one pass deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub sessions: u64,
    pub instances: u64,
}

/// One pass: deletes expired sessions and instances last seen more than 7
/// days ago.
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
    Ok(Pruned {
        sessions,
        instances,
    })
}

/// Runs a pass now and every 10 minutes until `shutdown` turns true (or its
/// sender is gone). A failed pass is logged and retried at the next one.
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
