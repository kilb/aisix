//! The settler: writes the marks usage intake has flagged.
//!
//! Intake never writes a gateway document itself: a document
//! `write_resource` refused there would fail every batch, and intake would
//! wait on the resources lock. It flags the account instead
//! (`marks::flag_crossings`) and notifies [`marks::DIRTY_CHANNEL`] at
//! commit. Every instance runs a settler, which wakes on that notification,
//! and every second without one in case a notification was lost, and
//! settles each flagged account in a transaction of its own
//! ([`settle_dirty`]).

use std::time::Duration;

use futures::FutureExt;
use sqlx::postgres::{PgListener, PgPoolOptions};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::{ledger, marks};
use crate::db::{Db, DbError};
use crate::error::ApiError;
use crate::resources::write::{WriteCtx, RESOURCES_LOCK_KEY};
use crate::stopped;

/// How often the settler looks for flagged accounts without a notification.
pub const SWEEP: Duration = Duration::from_secs(1);

/// Flagged accounts one pass takes at most.
const BATCH: i64 = 100;

/// Why one account was left flagged.
enum Unsettled {
    /// Its tenant or its account was held by another transaction
    /// (SQLSTATE `55P03`), or the resources lock was.
    Held,
    Failed(ApiError),
}

impl From<DbError> for Unsettled {
    fn from(e: DbError) -> Unsettled {
        match e {
            DbError::LockTimeout => Unsettled::Held,
            e => Unsettled::Failed(e.into()),
        }
    }
}

/// One pass: settles up to 100 flagged accounts, in ascending tenant id,
/// each in a transaction of its own, and returns how many it settled.
///
/// Each transaction takes its tenant's row without waiting
/// (`FOR UPDATE NOWAIT`), so a tenant a long edit holds is left flagged for
/// a later pass rather than holding up every tenant after it; the edit, if
/// it changes what the mark depends on, settles the mark itself. It then
/// takes the account, and the resources lock only if that is free: an
/// account held while waiting for the resources lock would hold the
/// tenant's intake up behind every gateway-document write. Any other
/// failure is logged at ERROR and left flagged too. Only reading the flagged
/// accounts is an error.
pub async fn settle_dirty(db: &Db, ctx: &WriteCtx) -> Result<u64, DbError> {
    let dirty: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM billing_accounts WHERE marks_dirty ORDER BY tenant_id LIMIT $1",
    )
    .bind(BATCH)
    .fetch_all(db.pool())
    .await?;
    let mut settled = 0;
    for tenant_id in dirty {
        match settle_one(db, ctx, tenant_id).await {
            Ok(()) => settled += 1,
            Err(Unsettled::Held) => tracing::debug!(
                tenant = %tenant_id,
                "a tenant's marks were left for a later pass: another transaction holds it"
            ),
            Err(Unsettled::Failed(e)) => tracing::error!(
                tenant = %tenant_id,
                code = e.code,
                reason = %e.message,
                details = ?e.details,
                "a tenant's marks could not be settled; a later pass tries again"
            ),
        }
    }
    Ok(settled)
}

async fn settle_one(db: &Db, ctx: &WriteCtx, tenant_id: Uuid) -> Result<(), Unsettled> {
    let ctx = WriteCtx {
        mk: crate::crypto::MasterKey(ctx.mk.0),
        env: ctx.env.clone(),
        cp_version: ctx.cp_version.clone(),
        stale_secs: ctx.stale_secs,
    };
    db.tx(move |conn| {
        async move {
            sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE NOWAIT")
                .bind(tenant_id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
            // Taken here, so that waiting out `lock_timeout` behind intake
            // also leaves the account for a later pass.
            ledger::lock_account(&mut *conn, tenant_id).await?;
            // Held to the end of the transaction; `write_resource` taking it
            // again does not wait.
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(RESOURCES_LOCK_KEY)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
            if !free {
                return Err(Unsettled::Held);
            }
            marks::settle_locked(conn, &ctx, tenant_id)
                .await
                .map_err(Unsettled::Failed)
        }
        .boxed()
    })
    .await
}

/// Runs [`settle_dirty`] at once, on every [`marks::DIRTY_CHANNEL`]
/// notification and every [`SWEEP`] until `shutdown` turns true (or its
/// sender is gone); again at once after a pass that took a full batch.
pub fn spawn(db: Db, ctx: WriteCtx, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut listener = None;
        loop {
            if listener.is_none() {
                listener = listen(&db).await;
            }
            // Wake-ups already queued are covered by the pass about to run.
            if let Some(listener) = listener.as_mut() {
                while PgListener::next_buffered(listener).is_some() {}
            }
            let delay = match settle_dirty(&db, &ctx).await {
                Ok(settled) if settled >= BATCH as u64 => Duration::ZERO,
                Ok(_) => SWEEP,
                Err(e) => {
                    tracing::warn!(error = %e, "looking for flagged billing accounts failed");
                    SWEEP
                }
            };
            tokio::select! {
                () = stopped(&mut shutdown) => return,
                () = wake(&mut listener, delay) => {}
            }
        }
    })
}

/// A connection that wakes the settler on every flag, on a pool of its own
/// so that it holds none of the shared pool's connections. Without it the
/// settler still looks every second.
async fn listen(db: &Db) -> Option<PgListener> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy_with((*db.pool().connect_options()).clone());
    let listener = async {
        let mut listener = PgListener::connect_with(&pool).await?;
        listener.listen(marks::DIRTY_CHANNEL).await?;
        Ok::<_, sqlx::Error>(listener)
    };
    match listener.await {
        Ok(listener) => Some(listener),
        Err(e) => {
            tracing::warn!(error = %e, "no billing flag notifications; looking every second");
            None
        }
    }
}

/// Waits up to `delay`, or until an account is flagged.
async fn wake(listener: &mut Option<PgListener>, delay: Duration) {
    let Some(listener) = listener else {
        tokio::time::sleep(delay).await;
        return;
    };
    if let Ok(Err(e)) = tokio::time::timeout(delay, listener.recv()).await {
        tracing::warn!(error = %e, "the billing flag notifications were cut off; reconnecting");
        tokio::time::sleep(delay).await;
    }
}
