//! The settler: writes the marks usage intake has flagged.
//!
//! Intake never writes a gateway document itself: a document
//! `write_resource` refused there would fail every batch, and intake would
//! wait on the resources lock. It flags the account instead, in the update
//! that posts the batch to it (`marks::crossings`), and notifies
//! [`marks::DIRTY_CHANNEL`] at commit. Every instance runs a settler, which wakes on that notification,
//! and every second without one in case a notification was lost, and
//! settles each flagged account in a transaction of its own
//! ([`Settler::pass`]).
//!
//! **Intake never waits on the resources lock; only the settler may.** A
//! settle normally takes its tenant's row (`NOWAIT`), then the account, and
//! the resources lock only if it is free, so it never holds an account while
//! queued behind a gateway-document write. Once a tenant's settle has found
//! the resources lock busy for [`Timing::escalate_after`] (steady document
//! writes leave it free at no instant a try can see), its attempts queue for
//! the lock instead, for at most [`Timing::queued_wait`] each, holding only
//! the tenant's row, and take the account after the lock, without waiting.
//! The attempt that first queues logs a WARN naming the tenant and how long
//! it had been skipped; while the tenant stays unsettled the WARN repeats at
//! most once per [`Timing::warn_every`], and an INFO line reports it once it
//! is settled. A lock taken without waiting is never part of a wait cycle,
//! and intake, which never waits on a tenant row, is not held up by the
//! queued settle.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

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

/// The settler's thresholds, at their production values in
/// [`Timing::default`]; tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long a tenant's settle may find the resources lock busy before
    /// it queues for the lock instead.
    pub escalate_after: Duration,
    /// How long a queued settle waits for the resources lock.
    pub queued_wait: Duration,
    /// The least time between two WARN lines about one tenant still waiting
    /// for the resources lock.
    pub warn_every: Duration,
    /// The least time between two ERROR lines about one tenant whose mark
    /// keeps failing, unless the failure changes.
    pub error_every: Duration,
    /// Flagged accounts read per query; a pass reads them all, page by page.
    pub page: i64,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            escalate_after: Duration::from_secs(10),
            queued_wait: Duration::from_secs(2),
            warn_every: Duration::from_secs(60),
            error_every: Duration::from_secs(30),
            page: 100,
        }
    }
}

/// What kept one settle from running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    /// Another transaction holds the tenant's row: an edit, which settles
    /// the mark itself if it changes what the mark depends on, or another
    /// instance's settler.
    Tenant,
    /// Intake holds the account.
    Account,
    /// A gateway-document write holds the resources lock.
    Resources,
}

/// Why one account was left flagged.
enum Unsettled {
    Held(Holder),
    Failed(ApiError),
}

impl From<DbError> for Unsettled {
    fn from(e: DbError) -> Unsettled {
        Unsettled::Failed(e.into())
    }
}

/// `e`, as a skip for `holder` when it is a lock not granted in time.
fn held(holder: Holder) -> impl Fn(DbError) -> Unsettled {
    move |e| match e {
        DbError::LockTimeout => Unsettled::Held(holder),
        e => Unsettled::Failed(e.into()),
    }
}

/// What a line about a failing tenant should be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ErrorLine {
    /// The first failure, or one that differs from the last reported.
    First,
    /// Still failing: this many failures since the last line.
    Summary(u64),
    Quiet,
}

/// What the settler remembers about one flagged tenant it could not
/// settle; forgotten once the tenant is settled or no longer flagged.
#[derive(Debug, Default)]
struct Track {
    /// When its settle first found the resources lock busy.
    busy_since: Option<Instant>,
    /// When it was last reported at WARN for that.
    warned_at: Option<Instant>,
    /// Failures since the last ERROR line, and in all.
    failures: u64,
    failures_total: u64,
    errored_at: Option<Instant>,
    last_error: Option<String>,
}

impl Track {
    /// Whether its settle queues for the resources lock.
    fn escalated(&self, now: Instant, timing: &Timing) -> bool {
        self.busy_since
            .is_some_and(|since| now.duration_since(since) >= timing.escalate_after)
    }

    /// Records that the resources lock was busy; how long it has been, when
    /// that is to be reported at WARN.
    fn busy(&mut self, now: Instant, timing: &Timing) -> Option<Duration> {
        let waited = now.duration_since(*self.busy_since.get_or_insert(now));
        let due = waited >= timing.escalate_after
            && self
                .warned_at
                .is_none_or(|at| now.duration_since(at) >= timing.warn_every);
        if due {
            self.warned_at = Some(now);
        }
        due.then_some(waited)
    }

    /// Records a failure reading `error`; the line to log for it.
    fn failed(&mut self, now: Instant, error: String, timing: &Timing) -> ErrorLine {
        self.failures += 1;
        self.failures_total += 1;
        let line = if self.last_error.as_deref() != Some(error.as_str()) {
            ErrorLine::First
        } else if self
            .errored_at
            .is_none_or(|at| now.duration_since(at) >= timing.error_every)
        {
            ErrorLine::Summary(self.failures)
        } else {
            return ErrorLine::Quiet;
        };
        self.errored_at = Some(now);
        self.failures = 0;
        self.last_error = Some(error);
        line
    }
}

/// The settler of one control-plane instance.
pub struct Settler {
    db: Db,
    ctx: WriteCtx,
    timing: Timing,
    tracks: HashMap<Uuid, Track>,
}

impl Settler {
    pub fn new(db: Db, ctx: WriteCtx, timing: Timing) -> Settler {
        Settler {
            db,
            ctx,
            timing,
            tracks: HashMap::new(),
        }
    }

    /// One pass: tries every flagged account once, in ascending tenant id,
    /// page by page, each in a transaction of its own, and returns how many
    /// it settled. A tenant that cannot be settled now is left flagged for a
    /// later pass, and no number of those keeps the pass from the tenants
    /// after them. Only reading the flagged accounts is an error.
    pub async fn pass(&mut self) -> Result<u64, DbError> {
        let mut settled = 0;
        let mut seen = HashSet::new();
        let mut after: Option<Uuid> = None;
        loop {
            let page: Vec<Uuid> = sqlx::query_scalar(
                "SELECT tenant_id FROM billing_accounts \
                 WHERE marks_dirty AND ($1::uuid IS NULL OR tenant_id > $1) \
                 ORDER BY tenant_id LIMIT $2",
            )
            .bind(after)
            .bind(self.timing.page)
            .fetch_all(self.db.pool())
            .await?;
            for &tenant_id in &page {
                seen.insert(tenant_id);
                if self.settle(tenant_id).await {
                    settled += 1;
                }
            }
            if (page.len() as i64) < self.timing.page {
                break;
            }
            after = page.last().copied();
        }
        // Settled elsewhere, by another instance or an admin action.
        self.tracks.retain(|id, _| seen.contains(id));
        Ok(settled)
    }

    /// Settles `tenant_id` and logs what kept it from that, rate-limited
    /// per tenant; whether it was settled.
    async fn settle(&mut self, tenant_id: Uuid) -> bool {
        let now = Instant::now();
        let timing = self.timing;
        let queued = match self.tracks.get_mut(&tenant_id) {
            Some(track) if track.escalated(now, &timing) => {
                if let Some(waited) = track.busy(now, &timing) {
                    tracing::warn!(
                        tenant = %tenant_id,
                        waited_secs = waited.as_secs(),
                        "a tenant's balance mark has waited this long for the resources lock, \
                         which gateway-document writes keep busy; its settle now queues for it"
                    );
                }
                Some(timing.queued_wait)
            }
            _ => None,
        };
        match settle_one(&self.db, &self.ctx, tenant_id, queued).await {
            Ok(()) => {
                if let Some(t) = self.tracks.remove(&tenant_id) {
                    if t.warned_at.is_some() || t.errored_at.is_some() {
                        tracing::info!(
                            tenant = %tenant_id,
                            waited_secs = t.busy_since.map(|s| now.duration_since(s).as_secs()),
                            failures = t.failures_total,
                            "a tenant's balance mark was settled after a delay"
                        );
                    }
                }
                true
            }
            Err(Unsettled::Held(Holder::Tenant)) => {
                tracing::debug!(
                    tenant = %tenant_id,
                    "a tenant's marks were left for a later pass: another transaction holds \
                     the tenant (an edit, or another instance's settler)"
                );
                false
            }
            Err(Unsettled::Held(Holder::Account)) => {
                tracing::debug!(
                    tenant = %tenant_id,
                    "a tenant's marks were left for a later pass: usage intake holds its account"
                );
                false
            }
            Err(Unsettled::Held(Holder::Resources)) => {
                let track = self.tracks.entry(tenant_id).or_default();
                match track.busy(now, &timing) {
                    Some(waited) => tracing::warn!(
                        tenant = %tenant_id,
                        waited_secs = waited.as_secs(),
                        "a tenant's balance mark is still waiting for the resources lock: its \
                         queued settle timed out, and the next pass queues again"
                    ),
                    None => tracing::debug!(
                        tenant = %tenant_id,
                        "a tenant's marks were left for a later pass: the resources lock is busy"
                    ),
                }
                false
            }
            Err(Unsettled::Failed(e)) => {
                let track = self.tracks.entry(tenant_id).or_default();
                let error = format!("{}: {}", e.code, e.message);
                match track.failed(now, error, &timing) {
                    ErrorLine::First => tracing::error!(
                        tenant = %tenant_id,
                        code = e.code,
                        reason = %e.message,
                        details = ?e.details,
                        failures = track.failures_total,
                        "a tenant's marks could not be settled; every pass tries again"
                    ),
                    ErrorLine::Summary(since) => tracing::error!(
                        tenant = %tenant_id,
                        code = e.code,
                        reason = %e.message,
                        failures_since_last_report = since,
                        failures = track.failures_total,
                        "a tenant's marks still cannot be settled"
                    ),
                    ErrorLine::Quiet => tracing::debug!(
                        tenant = %tenant_id,
                        code = e.code,
                        "a tenant's marks still cannot be settled"
                    ),
                }
                false
            }
        }
    }
}

/// One pass of a fresh settler, at the production [`Timing`]: no tenant
/// has been skipped before, so none queues for the resources lock.
pub async fn settle_dirty(db: &Db, ctx: &WriteCtx) -> Result<u64, DbError> {
    let ctx = WriteCtx {
        mk: crate::crypto::MasterKey(ctx.mk.0),
        env: ctx.env.clone(),
        cp_version: ctx.cp_version.clone(),
        stale_secs: ctx.stale_secs,
    };
    Settler::new(db.clone(), ctx, Timing::default())
        .pass()
        .await
}

/// Settles `tenant_id` in a transaction of its own. Its row is taken
/// without waiting. With `queued` unset, the account is taken next, and the
/// resources lock only if it is free; with `queued`, the resources lock is
/// waited for, for at most that long, and then the account is taken without
/// waiting.
async fn settle_one(
    db: &Db,
    ctx: &WriteCtx,
    tenant_id: Uuid,
    queued: Option<Duration>,
) -> Result<(), Unsettled> {
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
                .map_err(|e| held(Holder::Tenant)(e.into()))?;
            match queued {
                None => {
                    // Taken first, so that waiting out `lock_timeout` behind
                    // intake also leaves the account for a later pass.
                    ledger::lock_account(&mut *conn, tenant_id)
                        .await
                        .map_err(held(Holder::Account))?;
                    // Held to the end of the transaction; `write_resource`
                    // taking it again does not wait.
                    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                        .bind(RESOURCES_LOCK_KEY)
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                    if !free {
                        return Err(Unsettled::Held(Holder::Resources));
                    }
                }
                Some(wait) => {
                    // Queued behind the writes, holding only the tenant's
                    // row, which intake never waits on.
                    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
                        .bind(format!("{}ms", wait.as_millis()))
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                    sqlx::query("SELECT pg_advisory_xact_lock($1)")
                        .bind(RESOURCES_LOCK_KEY)
                        .execute(&mut *conn)
                        .await
                        .map_err(|e| held(Holder::Resources)(e.into()))?;
                    // Never waited for while the resources lock is held.
                    sqlx::query(
                        "SELECT 1 FROM billing_accounts WHERE tenant_id = $1 FOR UPDATE NOWAIT",
                    )
                    .bind(tenant_id)
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| held(Holder::Account)(e.into()))?;
                }
            }
            marks::settle_locked(conn, &ctx, tenant_id)
                .await
                .map_err(Unsettled::Failed)
        }
        .boxed()
    })
    .await
}

/// Runs a [`Settler`] pass at once, then on every [`marks::DIRTY_CHANNEL`]
/// notification and every [`SWEEP`], until `shutdown` turns true (or its
/// sender is gone). A notification that arrives during a pass wakes the
/// next one, which may find nothing to do.
pub fn spawn(db: Db, ctx: WriteCtx, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut listener = None;
        let mut settler = Settler::new(db.clone(), ctx, Timing::default());
        loop {
            if listener.is_none() {
                listener = listen(&db).await;
            }
            if let Err(e) = settler.pass().await {
                tracing::warn!(error = %e, "looking for flagged billing accounts failed");
            }
            tokio::select! {
                () = stopped(&mut shutdown) => return,
                () = wake(&mut listener, SWEEP) => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> Timing {
        Timing::default()
    }

    #[test]
    fn a_settle_queues_only_once_the_lock_has_been_busy_long_enough() {
        let t0 = Instant::now();
        let mut track = Track::default();
        assert!(!track.escalated(t0, &timing()));
        assert_eq!(track.busy(t0, &timing()), None);
        let almost = t0 + Duration::from_secs(9);
        assert_eq!(track.busy(almost, &timing()), None);
        assert!(!track.escalated(almost, &timing()));
        let past = t0 + Duration::from_secs(10);
        assert!(track.escalated(past, &timing()));
        // Reported once at WARN, and again only a minute later.
        assert_eq!(track.busy(past, &timing()), Some(Duration::from_secs(10)));
        for later in [11, 30, 69] {
            assert_eq!(track.busy(t0 + Duration::from_secs(later), &timing()), None);
        }
        assert_eq!(
            track.busy(t0 + Duration::from_secs(70), &timing()),
            Some(Duration::from_secs(70))
        );
    }

    #[test]
    fn a_failure_is_logged_once_then_summarised_at_most_every_30_seconds() {
        let t0 = Instant::now();
        let mut track = Track::default();
        let e = || "validation_failed: refused".to_string();
        assert_eq!(track.failed(t0, e(), &timing()), ErrorLine::First);
        for s in 1..30 {
            assert_eq!(
                track.failed(t0 + Duration::from_secs(s), e(), &timing()),
                ErrorLine::Quiet
            );
        }
        assert_eq!(
            track.failed(t0 + Duration::from_secs(30), e(), &timing()),
            ErrorLine::Summary(30)
        );
        // A different failure is reported at once.
        assert_eq!(
            track.failed(
                t0 + Duration::from_secs(31),
                "internal: x".into(),
                &timing()
            ),
            ErrorLine::First
        );
        assert_eq!(track.failures_total, 32);
    }
}
