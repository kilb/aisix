//! The projector: the one writer of kine.
//!
//! Every control-plane instance runs one, and exactly one of them projects:
//! the one whose session holds `pg_try_advisory_lock(PROJECTOR_LOCK_KEY)`,
//! on a connection of its own. The leader does all its database work on
//! that connection and checks that the connection's session still holds
//! the lock before each kine write, so an instance whose session ended
//! (and with it the lock) stops writing before another can start. An
//! instance competes for the lock only while it can reach kine, so one with
//! a kine it cannot reach never holds up another that could project.
//!
//! Once leader, it reconciles kine with the database (guarded: see
//! [`reconcile`]), then projects the rows due for it ([`project_once`]) on
//! each `gw_resources_dirty` notification, and at least every second. Rows
//! go in ascending `seq`, which is commit order (`write_resource` holds the
//! resources lock), and a pass stops at the first row it cannot project
//! instead of skipping ahead: a row written later is never live in kine
//! before one written earlier. The stalled row is logged and named in
//! `cp_settings.projection_stalled` until a pass gets through, and kine
//! errors are retried after 200 ms, doubling up to 10 s.
//!
//! An admin's `POST /admin/rollout/reconcile` may reach any instance; it is
//! handed to the leader through `cp_settings` and answered once the leader
//! has run it, so it never writes kine beside the projector.
//!
//! Credentials are unsealed only here, into the value sent to kine; no
//! value of a document reaches a log line, an error or an audit row.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::{PgListener, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::{Db, DbError};
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::kine::{KeyValue, KineError, KineWriter, WriteOutcome};
use crate::resources::write::WriteCtx;
use crate::resources::{secrets, Kind};
use crate::settings::{self, EnvSettings};
use crate::AppState;

/// Held, for as long as it leads, by the session of the one instance that
/// writes kine.
pub const PROJECTOR_LOCK_KEY: i64 = 0x6169_7369_7863_7031;

/// `cp_settings` key: whether the last reconcile found more stray keys than
/// it deletes on its own, and deleted none.
pub const RECONCILE_BLOCKED: &str = "reconcile_blocked";

/// `cp_settings` key: the row projection is stopped at, while it is,
/// `{kind, id, seq, error}`.
pub const PROJECTION_STALLED: &str = "projection_stalled";

/// What `write_resource` notifies on every write.
const DIRTY_CHANNEL: &str = "gw_resources_dirty";

/// `cp_settings` keys of reconcile requests and of their answers, followed
/// by the request's id.
const REQUEST_PREFIX: &str = "reconcile_request:";
const RESULT_PREFIX: &str = "reconcile_result:";

/// How often the leader looks for work without a notification, and a
/// follower tries for the lock.
const POLL: Duration = Duration::from_secs(1);
const BACKOFF_MIN: Duration = Duration::from_millis(200);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// Rows read per query: documents are up to 1 MiB each.
const BATCH: i64 = 64;

/// Writes tried for one row before the pass stops at it.
const ATTEMPTS: usize = 3;

/// Reconcile deletes up to this many stray keys on its own, or 5% of the
/// prefix's keys when that is more.
const STRAY_FLOOR: usize = 10;

/// How long `POST /admin/rollout/reconcile` waits for the leader's answer.
const ANSWER_WAIT: Duration = Duration::from_secs(30);

/// `<prefix>/<env_id>/<segment>/<id>`: where a resource lives in kine. The
/// budget policy singleton's id is the `env_id`.
pub fn key_for(env: &EnvSettings, kind: Kind, id: Uuid) -> String {
    format!("{}{}/{id}", env.env_prefix(), kind.segment())
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// A row could not be projected; nothing after it was tried.
    #[error("{kind}/{id} (seq {seq}): {cause}")]
    Row {
        kind: String,
        id: Uuid,
        seq: i64,
        cause: RowFailure,
    },
    /// kine failed a read that is not about one row.
    #[error(transparent)]
    Kine(#[from] KineError),
    #[error("database: {0}")]
    Db(#[from] DbError),
    /// The session that held the projector lock has ended.
    #[error("this instance no longer holds the projector lock")]
    LockLost,
}

impl From<sqlx::Error> for ProjectError {
    fn from(e: sqlx::Error) -> ProjectError {
        ProjectError::Db(e.into())
    }
}

/// Why a row could not be projected.
#[derive(Debug, thiserror::Error)]
pub enum RowFailure {
    #[error(transparent)]
    Kine(KineError),
    #[error("a stored credential does not open under the master key")]
    Unseal,
    #[error("the key changed under each of {ATTEMPTS} writes in a row")]
    Contention,
    #[error("the kind is not one this control plane projects")]
    UnknownKind,
}

/// What [`reconcile`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    pub deleted_strays: usize,
    pub marked_dirty: usize,
    /// Strays were found and, being too many, none was deleted.
    pub blocked: bool,
}

/// Runs the projector until `shutdown` resolves. Stopping it at any point is
/// safe: a write whose answer was lost is recognised by the next leader.
pub fn spawn(
    db: Db,
    kine: Arc<dyn KineWriter>,
    ctx: WriteCtx,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            () = shutdown => {}
            () = run(&db, kine.as_ref(), &ctx) => {}
        }
    })
}

/// Projects every row due for it, in ascending `seq`, and returns how many.
/// The first row that cannot be projected ends the pass with
/// [`ProjectError::Row`]; the rows after it are left for the next pass.
///
/// The caller must be the only writer of the environment's prefix: the
/// projector calls this only while it holds the projector lock.
pub async fn project_once(
    db: &Db,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
) -> Result<usize, ProjectError> {
    let mut conn = db.pool().acquire().await.map_err(DbError::from)?;
    project_pending(&mut conn, kine, ctx, false).await
}

/// Brings kine's environment prefix in line with the database.
///
/// - Keys no live row owns (strays) are deleted when `allow_deletes`, or
///   when they are at most `max(10, ceil(5%))` of the prefix's keys and the
///   database has at least one live row. Otherwise none is deleted, the
///   refusal is logged, and `cp_settings.reconcile_blocked` is set until a
///   reconcile deletes them: a database restored from backup, or a new one
///   given the same `env_id`, would otherwise wipe every gateway's
///   configuration. A key of a deleted row whose delete is still to be
///   projected is left to the projector.
/// - Live rows whose key is missing, or holds other bytes than the
///   document, are marked for projection again, keeping their place in the
///   write order.
///
/// Nothing outside the environment's prefix is read or written. Same
/// caller requirement as [`project_once`].
pub async fn reconcile(
    db: &Db,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    allow_deletes: bool,
) -> Result<ReconcileReport, ProjectError> {
    let mut conn = db.pool().acquire().await.map_err(DbError::from)?;
    reconcile_on(&mut conn, kine, ctx, allow_deletes, false).await
}

// ---- the lock --------------------------------------------------------------

/// The `pg_locks` row of the projector lock in this database: a bigint key
/// shows its high half as `classid`, its low half as `objid`, and
/// `objsubid = 1`.
const LOCK_ROW: &str = "locktype = 'advisory' AND granted \
     AND classid = $1::bigint::oid AND objid = $2::bigint::oid AND objsubid = 1 \
     AND database = (SELECT oid FROM pg_database WHERE datname = current_database())";

const LOCK_HIGH: i64 = PROJECTOR_LOCK_KEY >> 32;
const LOCK_LOW: i64 = PROJECTOR_LOCK_KEY & 0xffff_ffff;

/// Whether this connection's session holds the projector lock. A session
/// lock lasts until the session ends, so this also proves the session is
/// still the one that took it.
async fn holds_lock(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE {LOCK_ROW} AND pid = pg_backend_pid())"
    ))
    .bind(LOCK_HIGH)
    .bind(LOCK_LOW)
    .fetch_one(conn)
    .await
}

/// Whether any instance leads.
async fn someone_leads(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE {LOCK_ROW})"
    ))
    .bind(LOCK_HIGH)
    .bind(LOCK_LOW)
    .fetch_one(pool)
    .await
}

/// Refuses to go on unless `fenced` is off or the lock is still held.
async fn fence(conn: &mut PgConnection, fenced: bool) -> Result<(), ProjectError> {
    if fenced && !holds_lock(conn).await.map_err(|_| ProjectError::LockLost)? {
        return Err(ProjectError::LockLost);
    }
    Ok(())
}

// ---- projecting ------------------------------------------------------------

/// A row due for projection: its version differs from the projected one.
struct DirtyRow {
    kind: String,
    id: Uuid,
    seq: i64,
    version: i64,
    deleted: bool,
    /// With its credentials sealed.
    doc: Value,
    kine_mod_revision: Option<i64>,
}

type RawDirtyRow = (String, Uuid, i64, i64, bool, Value, Option<i64>);

async fn project_pending(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    fenced: bool,
) -> Result<usize, ProjectError> {
    let mut projected = 0;
    loop {
        let rows: Vec<RawDirtyRow> = sqlx::query_as(
            "SELECT kind, id, seq, version, deleted, doc, kine_mod_revision FROM gw_resources \
             WHERE projected_version IS DISTINCT FROM version ORDER BY seq LIMIT $1",
        )
        .bind(BATCH)
        .fetch_all(&mut *conn)
        .await?;
        let full = rows.len() as i64 == BATCH;
        for (kind, id, seq, version, deleted, doc, kine_mod_revision) in rows {
            let row = DirtyRow {
                kind,
                id,
                seq,
                version,
                deleted,
                doc,
                kine_mod_revision,
            };
            project_row(conn, kine, ctx, fenced, row).await?;
            projected += 1;
        }
        if !full {
            return Ok(projected);
        }
    }
}

/// The bytes the gateway is sent for `doc`: the stored document with its
/// credentials unsealed, serialized compactly.
fn projected_bytes(ctx: &WriteCtx, kind: Kind, id: Uuid, mut doc: Value) -> Option<Vec<u8>> {
    secrets::unseal(&ctx.mk, kind, id, &mut doc).ok()?;
    Some(doc.to_string().into_bytes())
}

async fn project_row(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    fenced: bool,
    row: DirtyRow,
) -> Result<(), ProjectError> {
    let failed = |cause| ProjectError::Row {
        kind: row.kind.clone(),
        id: row.id,
        seq: row.seq,
        cause,
    };
    let kind = Kind::from_segment(&row.kind).ok_or_else(|| failed(RowFailure::UnknownKind))?;
    let desired = if row.deleted {
        None
    } else {
        Some(
            projected_bytes(ctx, kind, row.id, row.doc.clone())
                .ok_or_else(|| failed(RowFailure::Unseal))?,
        )
    };
    fence(conn, fenced).await?;
    let key = key_for(&ctx.env, kind, row.id);
    let written = write_row(kine, &key, desired.as_deref(), row.kine_mod_revision)
        .await
        .map_err(|e| failed(RowFailure::Kine(e)))?
        .ok_or_else(|| failed(RowFailure::Contention))?;
    // A row rewritten meanwhile keeps its new version due, and its next
    // write starts from a compare-and-swap that shows what kine holds.
    sqlx::query(
        "UPDATE gw_resources SET projected_version = $3, kine_mod_revision = $4, \
           kine_revision = $5 \
         WHERE kind = $1 AND id = $2 AND version = $3",
    )
    .bind(&row.kind)
    .bind(row.id)
    .bind(row.version)
    .bind(written.mod_revision)
    .bind(written.revision)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What a row's projection left in kine.
struct Written {
    /// The key's mod revision, `None` once it is deleted.
    mod_revision: Option<i64>,
    /// The revision to record as the projection's: the write's, or that of
    /// the earlier write found already holding the value. `None` when kine
    /// held nothing to delete.
    revision: Option<i64>,
}

/// Makes `key` hold `desired` (`None`: not exist), starting from the mod
/// revision the database last recorded for it. A conflict shows what kine
/// holds: the value itself means an earlier write landed and its answer was
/// lost, and it is recorded rather than written again; anything else is
/// retried against the revision kine reported. `None` after [`ATTEMPTS`]
/// conflicts in a row.
async fn write_row(
    kine: &dyn KineWriter,
    key: &str,
    desired: Option<&[u8]>,
    mut known: Option<i64>,
) -> Result<Option<Written>, KineError> {
    for _ in 0..ATTEMPTS {
        let outcome = match (desired, known) {
            (Some(value), None) => kine.create(key, value).await?,
            (Some(value), Some(revision)) => kine.update(key, revision, value).await?,
            (None, Some(revision)) => kine.delete(key, revision).await?,
            (None, None) => match kine.get(key).await? {
                None => {
                    return Ok(Some(Written {
                        mod_revision: None,
                        revision: None,
                    }))
                }
                Some(kv) => kine.delete(key, kv.mod_revision).await?,
            },
        };
        match outcome {
            WriteOutcome::Applied { revision } => {
                return Ok(Some(Written {
                    mod_revision: desired.map(|_| revision),
                    revision: Some(revision),
                }))
            }
            WriteOutcome::Conflict { current: None } => known = None,
            WriteOutcome::Conflict { current: Some(kv) } => {
                if desired == Some(kv.value.as_slice()) {
                    return Ok(Some(Written {
                        mod_revision: Some(kv.mod_revision),
                        revision: Some(kv.mod_revision),
                    }));
                }
                known = Some(kv.mod_revision);
            }
        }
    }
    Ok(None)
}

// ---- reconciling -----------------------------------------------------------

async fn reconcile_on(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    allow_deletes: bool,
    fenced: bool,
) -> Result<ReconcileReport, ProjectError> {
    let prefix = ctx.env.env_prefix();
    let in_kine: HashMap<String, KeyValue> = kine.list(&prefix).await?.into_iter().collect();

    // The keys rows own: every live row's, and a deleted row's until its
    // delete is projected.
    let rows: Vec<(String, Uuid, bool, bool)> = sqlx::query_as(
        "SELECT kind, id, deleted, projected_version IS NOT DISTINCT FROM version \
         FROM gw_resources",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut owned = HashSet::new();
    let mut live = 0usize;
    for (kind, id, deleted, projected) in &rows {
        let Some(kind) = Kind::from_segment(kind) else {
            continue;
        };
        if !deleted {
            live += 1;
        }
        if !deleted || !projected {
            owned.insert(key_for(&ctx.env, kind, *id));
        }
    }
    let mut strays: Vec<(&String, &KeyValue)> = in_kine
        .iter()
        .filter(|(key, _)| !owned.contains(*key))
        .collect();
    strays.sort_by(|a, b| a.0.cmp(b.0));

    let few = STRAY_FLOOR.max(in_kine.len().div_ceil(20));
    let blocked = !strays.is_empty() && !allow_deletes && (strays.len() > few || live == 0);
    let mut deleted_strays = 0;
    if blocked {
        tracing::error!(
            strays = strays.len(),
            keys = in_kine.len(),
            live_rows = live,
            %prefix,
            "reconcile found more kine keys without a resource than it deletes on its own, \
             and deleted none; an admin can allow it with POST /admin/rollout/reconcile \
             {{\"allow_deletes\": true}}"
        );
    } else {
        for (key, kv) in strays {
            fence(conn, fenced).await?;
            if let WriteOutcome::Applied { .. } = kine.delete(key, kv.mod_revision).await? {
                deleted_strays += 1;
            }
        }
    }

    let mut marked_dirty = 0;
    let mut after = i64::MIN;
    loop {
        let batch: Vec<(String, Uuid, i64, i64, Value)> = sqlx::query_as(
            "SELECT kind, id, seq, version, doc FROM gw_resources \
             WHERE NOT deleted AND projected_version = version AND seq > $1 \
             ORDER BY seq LIMIT $2",
        )
        .bind(after)
        .bind(BATCH)
        .fetch_all(&mut *conn)
        .await?;
        let full = batch.len() as i64 == BATCH;
        for (segment, id, seq, version, doc) in batch {
            after = seq;
            let Some(kind) = Kind::from_segment(&segment) else {
                continue;
            };
            let current = in_kine.get(&key_for(&ctx.env, kind, id));
            // A credential that does not open is projected again too, so
            // the projector stops at it and says so.
            let drifted = current.is_none_or(|kv| {
                projected_bytes(ctx, kind, id, doc).is_none_or(|bytes| bytes != kv.value)
            });
            if !drifted {
                continue;
            }
            let marked = sqlx::query(
                "UPDATE gw_resources SET projected_version = NULL, kine_mod_revision = $4 \
                 WHERE kind = $1 AND id = $2 AND version = $3 AND projected_version = version",
            )
            .bind(&segment)
            .bind(id)
            .bind(version)
            .bind(current.map(|kv| kv.mod_revision))
            .execute(&mut *conn)
            .await?;
            marked_dirty += marked.rows_affected() as usize;
        }
        if !full {
            break;
        }
    }

    settings::set(conn, RECONCILE_BLOCKED, &json!(blocked)).await?;
    Ok(ReconcileReport {
        deleted_strays,
        marked_dirty,
        blocked,
    })
}

// ---- leading ---------------------------------------------------------------

/// 200 ms, doubling up to 10 s.
struct Backoff(Duration);

impl Backoff {
    fn new() -> Backoff {
        Backoff(BACKOFF_MIN)
    }

    fn next(&mut self) -> Duration {
        let delay = self.0;
        self.0 = (self.0 * 2).min(BACKOFF_MAX);
        delay
    }
}

async fn run(db: &Db, kine: &dyn KineWriter, ctx: &WriteCtx) {
    let mut backoff = Backoff::new();
    // A follower keeps its session between attempts.
    let mut session = None;
    loop {
        match try_lead(db, kine, ctx, &mut session).await {
            Ok(Some(conn)) => {
                backoff = Backoff::new();
                lead(db, conn, kine, ctx).await;
                tokio::time::sleep(POLL).await;
            }
            Ok(None) => tokio::time::sleep(POLL).await,
            Err(e) => {
                tracing::warn!(error = %e, "the projector cannot lead yet");
                tokio::time::sleep(backoff.next()).await;
            }
        }
    }
}

/// The session holding the projector lock, once this instance takes it;
/// `None` while another instance holds it. kine is asked first: an instance
/// that cannot reach kine leaves the lock to one that can.
async fn try_lead(
    db: &Db,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    session: &mut Option<PgConnection>,
) -> Result<Option<PgConnection>, ProjectError> {
    kine.get(&ctx.env.env_prefix()).await?;
    let mut conn = match session.take() {
        Some(conn) => conn,
        None => PgConnection::connect_with(&db.pool().connect_options()).await?,
    };
    let won: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(PROJECTOR_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    if won {
        Ok(Some(conn))
    } else {
        *session = Some(conn);
        Ok(None)
    }
}

/// Projects while `conn`'s session holds the lock. Returns once it does not,
/// or once the session fails.
async fn lead(db: &Db, mut conn: PgConnection, kine: &dyn KineWriter, ctx: &WriteCtx) {
    tracing::info!("this instance now projects resources to kine");
    let mut listener = listen(db).await;
    let mut backoff = Backoff::new();
    let mut reconciled = false;
    loop {
        // Wake-ups already queued are covered by the pass about to run.
        if let Some(listener) = listener.as_mut() {
            while listener.next_buffered().is_some() {}
        }
        let delay = match lead_once(&mut conn, kine, ctx, &mut reconciled).await {
            Ok(()) => {
                backoff = Backoff::new();
                POLL
            }
            Err(e @ (ProjectError::LockLost | ProjectError::Db(_))) => {
                tracing::warn!(error = %e, "the projector stops leading");
                let _ = conn.close().await;
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "projection to kine is stopped");
                backoff.next()
            }
        };
        wake(&mut listener, delay).await;
    }
}

/// One round of the leader's work: reconcile requests, the reconcile a new
/// leader owes, and a projection pass.
async fn lead_once(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    reconciled: &mut bool,
) -> Result<(), ProjectError> {
    *reconciled |= serve_requests(conn, kine, ctx).await?;
    if !*reconciled {
        let report = reconcile_on(conn, kine, ctx, false, true).await?;
        tracing::info!(?report, "reconciled kine with the database");
        *reconciled = true;
    }
    match project_pending(conn, kine, ctx, true).await {
        Ok(_) => {
            sqlx::query("DELETE FROM cp_settings WHERE key = $1")
                .bind(PROJECTION_STALLED)
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        Err(ProjectError::Row {
            kind,
            id,
            seq,
            cause,
        }) => {
            let stalled = json!({"kind": kind, "id": id, "seq": seq, "error": cause.to_string()});
            settings::set(conn, PROJECTION_STALLED, &stalled).await?;
            Err(ProjectError::Row {
                kind,
                id,
                seq,
                cause,
            })
        }
        Err(e) => Err(e),
    }
}

/// A connection that wakes the leader on every resource write, on a pool of
/// its own so that it holds none of the shared pool's connections. Without
/// it the leader still looks every second.
async fn listen(db: &Db) -> Option<PgListener> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy_with((*db.pool().connect_options()).clone());
    let listener = async {
        let mut listener = PgListener::connect_with(&pool).await?;
        listener.listen(DIRTY_CHANNEL).await?;
        Ok::<_, sqlx::Error>(listener)
    };
    match listener.await {
        Ok(listener) => Some(listener),
        Err(e) => {
            tracing::warn!(error = %e, "no write notifications; looking for work every second");
            None
        }
    }
}

/// Waits up to `delay`, or until a write is notified.
async fn wake(listener: &mut Option<PgListener>, delay: Duration) {
    let Some(listener) = listener else {
        tokio::time::sleep(delay).await;
        return;
    };
    if let Ok(Err(e)) = tokio::time::timeout(delay, listener.recv()).await {
        tracing::warn!(error = %e, "the write notifications were cut off; reconnecting");
        tokio::time::sleep(delay).await;
    }
}

// ---- reconcile on request ----------------------------------------------------

/// A reconcile request, as `cp_settings` holds it for the leader.
#[derive(Deserialize)]
struct Request {
    allow_deletes: bool,
    operator_id: Uuid,
    operator_email: String,
}

/// Runs the pending reconcile requests, oldest first, and answers each one.
/// Returns whether one of them reconciled.
async fn serve_requests(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
) -> Result<bool, ProjectError> {
    let pending: Vec<(String, Value)> = sqlx::query_as(
        "SELECT key, value FROM cp_settings WHERE starts_with(key, $1) ORDER BY updated_at, key",
    )
    .bind(REQUEST_PREFIX)
    .fetch_all(&mut *conn)
    .await?;
    let mut reconciled = false;
    for (key, value) in pending {
        let (Some(id), Ok(request)) = (
            key.strip_prefix(REQUEST_PREFIX),
            serde_json::from_value::<Request>(value),
        ) else {
            tracing::warn!(%key, "dropping a reconcile request that does not parse");
            sqlx::query("DELETE FROM cp_settings WHERE key = $1")
                .bind(&key)
                .execute(&mut *conn)
                .await?;
            continue;
        };
        let allow_deletes = request.allow_deletes;
        let (answer, detail) = match reconcile_on(conn, kine, ctx, allow_deletes, true).await {
            Ok(report) => {
                reconciled = true;
                let counts = json!({
                    "deleted_strays": report.deleted_strays,
                    "marked_dirty": report.marked_dirty,
                });
                let mut detail = counts.clone();
                detail["allow_deletes"] = json!(allow_deletes);
                (counts, detail)
            }
            // Left for whichever instance leads next.
            Err(e @ (ProjectError::LockLost | ProjectError::Db(_))) => return Err(e),
            Err(e) => {
                tracing::error!(error = %e, "a requested reconcile failed");
                (
                    json!({"error": "unavailable"}),
                    json!({"allow_deletes": allow_deletes, "failed": true}),
                )
            }
        };
        let operator = OperatorAuth {
            id: request.operator_id,
            email: request.operator_email,
            // Only an admin may ask (`ROUTE_ROLES`).
            role: OperatorRole::Admin,
        };
        let mut tx = conn.begin().await?;
        sqlx::query("DELETE FROM cp_settings WHERE key = $1")
            .bind(&key)
            .execute(&mut *tx)
            .await?;
        settings::set(&mut tx, &format!("{RESULT_PREFIX}{id}"), &answer).await?;
        audit::record(
            &mut tx,
            AuditEntry {
                actor: Actor::Operator(&operator),
                action: "rollout.reconcile",
                target_kind: "environment",
                target_id: Some(ctx.env.env_id.to_string()),
                tenant_id: None,
                detail,
            },
        )
        .await?;
        tx.commit().await?;
    }
    // Answers whose asker stopped waiting.
    sqlx::query(
        "DELETE FROM cp_settings WHERE starts_with(key, $1) \
           AND updated_at < now() - interval '1 hour'",
    )
    .bind(RESULT_PREFIX)
    .execute(&mut *conn)
    .await?;
    Ok(reconciled)
}

#[derive(Deserialize)]
pub struct ReconcileBody {
    allow_deletes: bool,
}

/// `POST /admin/rollout/reconcile` with `{allow_deletes}` → `200
/// {deleted_strays, marked_dirty}`, once the projector has run the
/// reconcile. With `allow_deletes: true` every stray key is deleted, which
/// clears `reconcile_blocked`. `503 unavailable` when no projector leads,
/// kine cannot be reached, or no answer comes within 30 seconds.
pub async fn post_reconcile(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ReconcileBody>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let pool = state.db.pool();
    if !someone_leads(pool).await.map_err(DbError::from)? {
        return Err(ApiError::unavailable());
    }
    let id = Uuid::new_v4();
    let request_key = format!("{REQUEST_PREFIX}{id}");
    let answer_key = format!("{RESULT_PREFIX}{id}");
    let request = json!({
        "allow_deletes": body.allow_deletes,
        "operator_id": auth.id,
        "operator_email": auth.email,
    });
    {
        let mut conn = pool.acquire().await.map_err(DbError::from)?;
        settings::set(&mut conn, &request_key, &request).await?;
        sqlx::query("SELECT pg_notify($1, '')")
            .bind(DIRTY_CHANNEL)
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
    }
    let deadline = Instant::now() + ANSWER_WAIT;
    loop {
        let answer: Option<Value> =
            sqlx::query_scalar("DELETE FROM cp_settings WHERE key = $1 RETURNING value")
                .bind(&answer_key)
                .fetch_optional(pool)
                .await
                .map_err(DbError::from)?;
        if let Some(answer) = answer {
            if answer.get("error").is_some() {
                return Err(ApiError::unavailable());
            }
            return Ok(Json(answer));
        }
        if Instant::now() >= deadline {
            sqlx::query("DELETE FROM cp_settings WHERE key = $1")
                .bind(&request_key)
                .execute(pool)
                .await
                .map_err(DbError::from)?;
            return Err(ApiError::unavailable());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
