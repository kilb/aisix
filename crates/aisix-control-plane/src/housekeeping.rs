//! Pruning what is left behind: browser sessions past their expiry, the
//! rows of gateway instances not heard from for a week (a gateway mints a
//! new `instance_id` at every boot, so each restart leaves one behind), and
//! the tombstones of deleted gateway resources once their delete has been
//! projected for a day; catching up the key documents an older release
//! wrote ([`keys::catch_up`]); opening the billing account of any tenant
//! that has none ([`ledger::backfill_accounts`]); replacing any grant of
//! a channel's generated direct model by its public model
//! ([`tenants::replace_generated_grants`]); regenerating every public
//! model's routing document that lacks the routing settings the model
//! stores ([`channels::repair_routing`]); and flagging every balance mark
//! the rule in force would not write ([`marks::flag_stale_marks`]).
//!
//! Every instance runs the pass at boot and every 10 minutes after; the
//! deletes are idempotent, and the catch-up takes one tenant at a time, so
//! instances running it together do no harm.

use std::time::Duration;

use futures::FutureExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::billing::{ledger, marks};
use crate::db::{Db, DbError};
use crate::resources::write::{WriteCtx, RESOURCES_LOCK_KEY};
use crate::stopped;
use crate::{channels, keys, tenants};

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

/// One tick: a pruning pass ([`run_once`]), then the key documents an
/// older release wrote are caught up ([`keys::catch_up`]), then every tenant
/// without a billing account gets one ([`ledger::backfill_accounts`]: an
/// older release, during a rolling upgrade, creates tenants without one),
/// then every grant of a channel's generated direct model is replaced by its
/// public model ([`tenants::replace_generated_grants`]: an older release
/// could grant one), then every public model's routing document that lacks
/// the settings the model stores is regenerated ([`channels::repair_routing`]:
/// a control plane older than `0037` wrote it before `0039` fenced it out),
/// then every balance mark the rule in force would not write is flagged for
/// the settler ([`marks::flag_stale_marks`]: an earlier rule, or an older
/// replica, wrote it). Each part is logged on its own so
/// that one failing leaves the others to run. A tenant the gateway-version
/// gate refuses is caught up by a later tick.
pub async fn tick(db: &Db, ctx: &WriteCtx) {
    match run_once(db).await {
        Ok(pruned) => tracing::debug!(?pruned, "housekeeping pass"),
        Err(e) => tracing::warn!(error = %e, "a housekeeping pass failed"),
    }
    match keys::catch_up(db, ctx, keys::KEY_DOC_VERSION).await {
        Ok(caught_up) => tracing::debug!(?caught_up, "key documents caught up"),
        Err(e) => tracing::warn!(code = e.code, "catching up key documents failed"),
    }
    backfill_accounts(db).await;
    replace_generated_grants(db, ctx).await;
    repair_routing(db, ctx).await;
    match marks::flag_stale_marks(db).await {
        Ok(0) => {}
        Ok(flagged) => tracing::info!(
            flagged,
            "flagged the accounts whose balance mark the rule in force would not write; the \
             settler writes their marks again"
        ),
        Err(e) => tracing::warn!(error = %e, "looking for stale balance marks failed"),
    }
}

/// Replaces every grant of a channel's generated direct model by its public
/// model ([`tenants::replace_generated_grants`]), and logs what it changed,
/// in counts only, or why it failed. A tenant it leaves behind is tried
/// again by a later run.
pub(crate) async fn replace_generated_grants(db: &Db, ctx: &WriteCtx) {
    match tenants::replace_generated_grants(db, ctx).await {
        Ok(done) if done == tenants::GeneratedGrantsReplaced::default() => {}
        Ok(done) => tracing::warn!(
            tenants = done.tenants,
            grants_replaced = done.grants_replaced,
            grants_withdrawn = done.grants_withdrawn,
            keys_changed = done.keys_changed,
            deferred = done.deferred,
            failed = done.failed,
            "replaced tenant grants and key selections of channels' generated direct models \
             with their public models, withdrawing those whose public model is retired; each \
             tenant changed has a tenant.generated_grants_replaced audit row, and a deferred or \
             failed tenant is tried again at the next housekeeping tick"
        ),
        Err(e) => tracing::warn!(
            code = e.code,
            "looking for grants of channels' generated direct models failed; the next \
             housekeeping tick tries again"
        ),
    }
}

/// Regenerates every public model's routing document that lacks the
/// routing settings the model stores ([`channels::repair_routing`]), and
/// logs what it did, in counts only, or why it failed. A model it leaves
/// behind is tried again by a later run.
pub(crate) async fn repair_routing(db: &Db, ctx: &WriteCtx) {
    match channels::repair_routing(db, ctx).await {
        Ok(done) if done == channels::RoutingRepaired::default() => {}
        Ok(done) => tracing::warn!(
            repaired = done.repaired,
            deferred = done.deferred,
            failed = done.failed,
            "regenerated public models' routing documents that lacked the routing settings \
             the models store; each has a public_model.routing_regenerated audit row, and a \
             deferred or failed model is tried again at the next housekeeping tick"
        ),
        Err(e) => tracing::warn!(
            code = e.code,
            "looking for public models' routing documents without their stored settings \
             failed; the next housekeeping tick tries again"
        ),
    }
}

/// Opens the billing account of every tenant that has none
/// ([`ledger::backfill_accounts`]), and logs how many, or why it failed. A
/// tenant it leaves behind is opened by a later run.
pub(crate) async fn backfill_accounts(db: &Db) {
    let opened = async {
        let mut conn = db.pool().acquire().await.map_err(DbError::from)?;
        ledger::backfill_accounts(&mut conn).await
    };
    match opened.await {
        Ok(0) => {}
        Ok(opened) => tracing::info!(
            opened,
            "opened the billing accounts of tenants that had none"
        ),
        Err(e) => tracing::warn!(error = %e, "opening missing billing accounts failed"),
    }
}

/// Runs a tick now and every 10 minutes until `shutdown` turns true (or its
/// sender is gone): a control plane that restarts more often than that
/// still prunes. A failed tick is logged and retried at the next one.
pub fn spawn(db: Db, ctx: WriteCtx, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(PERIOD);
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stopped(&mut shutdown) => return,
                _ = every.tick() => {}
            }
            tick(&db, &ctx).await;
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
