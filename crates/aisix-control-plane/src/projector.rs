//! The projector: the one writer of kine.
//!
//! **One writer per instance set.** Every control-plane instance runs a
//! projector, and exactly one of them projects: the one whose session holds
//! `pg_try_advisory_lock(PROJECTOR_LOCK_KEY)`, on a connection of its own
//! with TCP keepalives, so that Postgres notices a dead leader host within
//! about 20 seconds and frees the lock. The leader does all its database
//! work on that session and, before every kine write, retries included,
//! checks that the session still holds the lock: a projector whose session
//! has ended writes nothing more. An instance competes for the lock only
//! while it can reach kine, and a leader whose kine has been unreachable for
//! 30 seconds steps down, so one with a working kine can take over.
//!
//! **What the check cannot stop.** kine has no fencing token, so a write
//! already sent when the session ends can still land, after a new leader has
//! started. It is a compare-and-swap, so it fails if the new leader has
//! rewritten that key, but it lands if the new leader has not touched it,
//! and it can land after the new tenure's first reconcile. That is why a
//! tenure reconciles again every [`RECONCILE_EVERY`]: the periodic
//! reconcile repairs what such a write leaves, and is not redundant with the
//! one a tenure starts with.
//!
//! The same holds across databases. A control plane whose environment has
//! just been taken over learns it from the claim, which it reads at the
//! start of every pass, before each batch of rows it writes and before it
//! deletes strays: what it already sent before the next read can still
//! land, at most one batch of rows, and the new owner's periodic reconcile
//! repairs it.
//!
//! **One owner per environment.** A database mints its own id
//! (`cp_settings.database_id`) at first boot, and the projector of only one
//! database writes an environment's prefix: the one named by the claim
//! `<prefix>/_cp/<env_id>/owner` in kine, outside every prefix a gateway
//! reads. A tenure claims it when it is absent and reads it on every pass. A
//! control plane finding it held under another id stops projecting, reports
//! `cp_settings.projection_blocked` (naming both ids) and asks again every
//! minute. Moving an environment to another database is an admin's
//! `POST /admin/rollout/ownership` on the new one. A restored copy of a
//! database carries the same id, so it must never run beside the original.
//!
//! **Order.** A tenure reconciles kine with the database (guarded: see
//! [`reconcile`]), again every 10 minutes, then projects the rows due on
//! each `gw_resources_dirty` notification, and at least every second. Rows
//! go in ascending `seq`, which is commit order (`write_resource` holds the
//! resources lock), and a pass stops at the first row it cannot project
//! instead of skipping ahead. The stall is logged and named in
//! `cp_settings.projection_stalled` until a pass gets through. Failed passes
//! are retried after 200 ms, doubling up to 10 s.
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
use sqlx::postgres::{PgConnectOptions, PgListener, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::{Db, DbError};
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::kine::{KeyValue, KineClient, KineError, KineWriter, WriteOutcome};
use crate::resources::write::WriteCtx;
use crate::resources::{secrets, Kind};
use crate::settings::{self, EnvSettings};
use crate::AppState;

/// Held, for as long as it leads, by the session of the one instance that
/// writes kine.
pub const PROJECTOR_LOCK_KEY: i64 = 0x6169_7369_7863_7031;

/// `cp_settings` key: whether the last reconcile found stray keys it would
/// not delete on its own, and deleted none of those.
pub const RECONCILE_BLOCKED: &str = "reconcile_blocked";

/// `cp_settings` key: what projection is stopped at, while it is:
/// `{kind, id, seq, error}`, or `{error: "kine unavailable"}`.
pub const PROJECTION_STALLED: &str = "projection_stalled";

/// `cp_settings` key: set while another control plane owns this
/// environment's prefix and this one therefore projects nothing.
pub const PROJECTION_BLOCKED: &str = "projection_blocked";

/// `cp_settings` key: this database's own id, the owner it claims an
/// environment's prefix as.
pub const DATABASE_ID: &str = "database_id";

/// How often a tenure reconciles again, besides when it starts. Not
/// redundant: a write a deposed leader had already sent can land after the
/// new tenure's first reconcile (kine has no fencing token), and this is
/// what repairs it (see the module docs).
pub const RECONCILE_EVERY: Duration = Duration::from_secs(600);

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
/// A request older than this is dropped unrun.
const ANSWER_WAIT: Duration = Duration::from_secs(30);

/// The lock session's TCP keepalives, so that the server notices a leader
/// host that died without closing its connection, ends its session and
/// frees the lock within about 20 seconds instead of the operating
/// system's two hours.
const LOCK_SESSION_KEEPALIVES: [(&str, u32); 4] = [
    ("tcp_keepalives_idle", 5),
    ("tcp_keepalives_interval", 5),
    ("tcp_keepalives_count", 3),
    ("tcp_user_timeout", 15_000),
];

/// The projector's intervals, all of them at their production values in
/// [`Timing::default`]; tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How often a tenure reconciles again.
    pub reconcile_every: Duration,
    /// How long a leader keeps the lock while every pass finds kine
    /// unreachable.
    pub kine_down_step_down: Duration,
    /// How long a control plane whose environment another one owns waits
    /// before it asks again.
    pub owned_elsewhere_retry: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            reconcile_every: RECONCILE_EVERY,
            kine_down_step_down: Duration::from_secs(30),
            owned_elsewhere_retry: Duration::from_secs(60),
        }
    }
}

/// `<prefix>/<env_id>/<segment>/<id>`: where a resource lives in kine. The
/// budget policy singleton's id is the `env_id`.
pub fn key_for(env: &EnvSettings, kind: Kind, id: Uuid) -> String {
    format!("{}{}/{id}", env.env_prefix(), kind.segment())
}

/// `<prefix>/_cp/<env_id>/owner`: which database's control plane writes the
/// environment's prefix. No gateway reads it: a gateway reads
/// `<prefix>/<env_id>/` and `<prefix>/global/` only
/// (`aisix-core` `EtcdConfig::effective_prefix` and `global_prefix`).
pub fn claim_key(env: &EnvSettings) -> String {
    format!(
        "{}/_cp/{}/owner",
        env.etcd_prefix.trim_end_matches('/'),
        env.env_id
    )
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
    /// kine failed a call that is not about one row.
    #[error(transparent)]
    Kine(#[from] KineError),
    #[error("database: {0}")]
    Db(#[from] DbError),
    /// The session that held the projector lock has ended.
    #[error("this instance no longer holds the projector lock")]
    LockLost,
    /// Another session holds the projector lock.
    #[error("another session holds the projector lock")]
    NotLeader,
    /// Another database's control plane owns this environment's prefix:
    /// the claim names `owner` (`None` when it names no readable id), and
    /// this database is `mine`.
    #[error("another control plane owns this environment")]
    OwnedElsewhere { owner: Option<Uuid>, mine: Uuid },
}

impl From<sqlx::Error> for ProjectError {
    fn from(e: sqlx::Error) -> ProjectError {
        ProjectError::Db(e.into())
    }
}

impl ProjectError {
    /// Whether this failed because kine could not be reached.
    fn kine_unavailable(&self) -> bool {
        let kine = match self {
            ProjectError::Kine(e)
            | ProjectError::Row {
                cause: RowFailure::Kine(e),
                ..
            } => e,
            _ => return false,
        };
        matches!(kine, KineError::Unavailable(_) | KineError::Timeout(_))
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
    /// Strays were found that it would not delete, and it deleted none of
    /// them.
    pub blocked: bool,
}

/// Runs the projector until `shutdown` resolves, at the production
/// [`Timing`]. Stopping it at any point is safe: a write whose answer was
/// lost is recognised by the next leader.
pub fn spawn(
    db: Db,
    kine: Arc<dyn KineWriter>,
    ctx: WriteCtx,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    spawn_with(db, kine, ctx, Timing::default(), shutdown)
}

/// [`spawn`], at the given [`Timing`].
pub fn spawn_with(
    db: Db,
    kine: Arc<dyn KineWriter>,
    ctx: WriteCtx,
    timing: Timing,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            () = shutdown => {}
            () = run(&db, kine.as_ref(), &ctx, timing) => {}
        }
    })
}

/// Projects every row due for it, in ascending `seq`, and returns how many.
/// The first row that cannot be projected ends the pass with
/// [`ProjectError::Row`]; the rows after it are left for the next pass.
///
/// Holds the projector lock for the call on a session of its own, and
/// refuses with [`ProjectError::NotLeader`] while another session holds it,
/// and with [`ProjectError::OwnedElsewhere`] while the claim names another
/// database.
pub async fn project_once(
    db: &Db,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
) -> Result<usize, ProjectError> {
    let mut conn = locked_session(db).await?;
    let projected = match check_claim(&mut conn, kine, ctx).await {
        Ok(mine) => project_pending(&mut conn, kine, ctx, mine).await,
        Err(e) => Err(e),
    };
    let _ = conn.close().await;
    projected
}

/// Brings kine's environment prefix in line with the database.
///
/// - Keys no row owns (strays) are deleted when `allow_deletes`, or when
///   they are at most `max(10, ceil(5%))` of the prefix's keys and the
///   database has at least one live row besides the platform tenant's own
///   documents, which every booted database holds. Otherwise none is
///   deleted, the refusal is logged, and `cp_settings.reconcile_blocked` is
///   set until a reconcile deletes them: a database restored from backup
///   would otherwise wipe every gateway's configuration. A live row owns its
///   key, and so does a deleted row until its delete is projected. A key
///   that is not UTF-8 is never deleted (none of this control plane's is),
///   and keeps reconcile blocked until an operator removes it.
/// - What is about to be deleted is written to the audit log first.
/// - Live rows whose key is missing, or holds other bytes than the
///   document, are marked for projection again, keeping their place in the
///   write order.
///
/// Nothing outside the environment's prefix is read or written. Holds the
/// projector lock and checks the claim for the call, as [`project_once`]
/// does.
pub async fn reconcile(
    db: &Db,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    allow_deletes: bool,
) -> Result<ReconcileReport, ProjectError> {
    let mut conn = locked_session(db).await?;
    let report = match check_claim(&mut conn, kine, ctx).await {
        Ok(mine) => reconcile_on(&mut conn, kine, ctx, mine, allow_deletes, None).await,
        Err(e) => Err(e),
    };
    let _ = conn.close().await;
    report
}

/// This database's id, minted the first time it is asked for. A boot asks,
/// so every database has one from its first boot on.
pub async fn ensure_database_id(db: &Db) -> Result<Uuid, DbError> {
    let mut conn = db.pool().acquire().await?;
    database_id(&mut conn).await
}

async fn database_id(conn: &mut PgConnection) -> Result<Uuid, DbError> {
    sqlx::query(
        "INSERT INTO cp_settings (key, value) VALUES ($1, to_jsonb(gen_random_uuid()::text)) \
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(DATABASE_ID)
    .execute(&mut *conn)
    .await?;
    Ok(
        sqlx::query_scalar("SELECT (value #>> '{}')::uuid FROM cp_settings WHERE key = $1")
            .bind(DATABASE_ID)
            .fetch_one(&mut *conn)
            .await?,
    )
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

/// The options of a session that may hold the lock: the pool's, plus
/// [`LOCK_SESSION_KEEPALIVES`].
fn lock_session_options(base: &PgConnectOptions) -> PgConnectOptions {
    base.clone().options(LOCK_SESSION_KEEPALIVES)
}

/// A session of its own that may hold the projector lock, with
/// [`LOCK_SESSION_KEEPALIVES`].
pub async fn lock_session(db: &Db) -> Result<PgConnection, sqlx::Error> {
    PgConnection::connect_with(&lock_session_options(&db.pool().connect_options())).await
}

async fn try_lock(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(PROJECTOR_LOCK_KEY)
        .fetch_one(conn)
        .await
}

/// A session of its own holding the lock, or [`ProjectError::NotLeader`].
async fn locked_session(db: &Db) -> Result<PgConnection, ProjectError> {
    let mut conn = lock_session(db).await?;
    if !try_lock(&mut conn).await? {
        let _ = conn.close().await;
        return Err(ProjectError::NotLeader);
    }
    Ok(conn)
}

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

/// Goes on only while the session still holds the lock. Called right before
/// every kine write.
async fn fence(conn: &mut PgConnection) -> Result<(), ProjectError> {
    match holds_lock(conn).await {
        Ok(true) => Ok(()),
        _ => Err(ProjectError::LockLost),
    }
}

// ---- projecting ------------------------------------------------------------

type RawDirtyRow = (String, Uuid, i64, i64, bool, Value, Option<i64>);

/// Projects the rows due, in batches of [`BATCH`]; before writing each
/// batch, checks that the claim still names `mine`, so a pass that started
/// as the owner stops within a batch of losing the environment.
async fn project_pending(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    mine: Uuid,
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
        if !rows.is_empty() {
            still_owner(kine, ctx, mine).await?;
        }
        let full = rows.len() as i64 == BATCH;
        for row in rows {
            project_row(conn, kine, ctx, row).await?;
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
    (segment, id, seq, version, deleted, doc, kine_mod_revision): RawDirtyRow,
) -> Result<(), ProjectError> {
    let failed = |cause| ProjectError::Row {
        kind: segment.clone(),
        id,
        seq,
        cause,
    };
    let kind = Kind::from_segment(&segment).ok_or_else(|| failed(RowFailure::UnknownKind))?;
    let desired = if deleted {
        None
    } else {
        Some(projected_bytes(ctx, kind, id, doc).ok_or_else(|| failed(RowFailure::Unseal))?)
    };
    let key = key_for(&ctx.env, kind, id);
    let written = match write_row(conn, kine, &key, desired.as_deref(), kine_mod_revision).await {
        Ok(Some(written)) => written,
        Ok(None) => return Err(failed(RowFailure::Contention)),
        Err(WriteError::Fence) => return Err(ProjectError::LockLost),
        Err(WriteError::Kine(e)) => return Err(failed(RowFailure::Kine(e))),
    };
    // A row rewritten meanwhile keeps its new version due, and its next
    // write starts from a compare-and-swap that shows what kine holds.
    sqlx::query(
        "UPDATE gw_resources SET projected_version = $3, kine_mod_revision = $4, \
           kine_revision = $5 \
         WHERE kind = $1 AND id = $2 AND version = $3",
    )
    .bind(&segment)
    .bind(id)
    .bind(version)
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

enum WriteError {
    /// The session no longer holds the lock: another projector may be
    /// writing, so this one writes nothing more.
    Fence,
    Kine(KineError),
}

/// Fails with [`WriteError::Fence`] unless the session still holds the lock.
async fn guard(conn: &mut PgConnection) -> Result<(), WriteError> {
    fence(conn).await.map_err(|_| WriteError::Fence)
}

/// Makes `key` hold `desired` (`None`: not exist), starting from the mod
/// revision the database last recorded for it. A conflict shows what kine
/// holds: the value itself means an earlier write landed and its answer was
/// lost, and it is recorded rather than written again; anything else is
/// retried against the revision kine reported. Every write, retries
/// included, goes out only while the session still holds the lock: a
/// conflict can be another projector's write, which only a deposed one
/// would see. `None` after [`ATTEMPTS`] conflicts in a row.
async fn write_row(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    key: &str,
    desired: Option<&[u8]>,
    mut known: Option<i64>,
) -> Result<Option<Written>, WriteError> {
    for _ in 0..ATTEMPTS {
        let outcome = match (desired, known) {
            (Some(value), None) => {
                guard(conn).await?;
                kine.create(key, value).await
            }
            (Some(value), Some(revision)) => {
                guard(conn).await?;
                kine.update(key, revision, value).await
            }
            (None, Some(revision)) => {
                guard(conn).await?;
                kine.delete(key, revision).await
            }
            (None, None) => match kine.get(key).await.map_err(WriteError::Kine)? {
                None => {
                    return Ok(Some(Written {
                        mod_revision: None,
                        revision: None,
                    }))
                }
                Some(kv) => {
                    guard(conn).await?;
                    kine.delete(key, kv.mod_revision).await
                }
            },
        }
        .map_err(WriteError::Kine)?;
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

/// `rollout.*` audit rows, about the environment.
async fn audit_environment(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    by: Option<&OperatorAuth>,
    action: &'static str,
    detail: Value,
) -> Result<(), DbError> {
    audit::record(
        conn,
        AuditEntry {
            actor: by.map_or(Actor::System, Actor::Operator),
            action,
            target_kind: "environment",
            target_id: Some(ctx.env.env_id.to_string()),
            tenant_id: None,
            detail,
        },
    )
    .await
}

/// [`reconcile`] on the lock's session. `by` is the operator who asked, or
/// `None` for the tenure's own reconcile; the outcome of an asked one is
/// audited with its answer.
async fn reconcile_on(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    mine: Uuid,
    allow_deletes: bool,
    by: Option<&OperatorAuth>,
) -> Result<ReconcileReport, ProjectError> {
    let prefix = ctx.env.env_prefix();
    let in_kine: HashMap<Vec<u8>, KeyValue> = kine.list(&prefix).await?.into_iter().collect();

    // The keys rows own: every live row's, and a deleted row's until its
    // delete is projected.
    let rows: Vec<(String, Uuid, bool, bool, Option<String>)> = sqlx::query_as(
        "SELECT kind, id, deleted, projected_version IS NOT DISTINCT FROM version, managed_by \
         FROM gw_resources",
    )
    .fetch_all(&mut *conn)
    .await?;
    // Every booted database holds the platform tenant's own documents, a
    // fresh or restored one included: they are no sign that the database
    // holds this environment, so they do not count as its live rows.
    let platform: Option<Uuid> = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_optional(&mut *conn)
        .await?;
    let platform = platform.map(crate::tenants::owner);
    let mut owned = HashSet::new();
    let mut live = 0usize;
    for (kind, id, deleted, projected, managed_by) in &rows {
        let Some(kind) = Kind::from_segment(kind) else {
            continue;
        };
        let boot_default = platform.is_some() && *managed_by == platform;
        if !deleted && !boot_default {
            live += 1;
        }
        if !deleted || !projected {
            owned.insert(key_for(&ctx.env, kind, *id).into_bytes());
        }
    }
    let mut strays: Vec<(String, i64)> = Vec::new();
    let mut unreadable = 0usize;
    for (key, kv) in &in_kine {
        if owned.contains(key) {
            continue;
        }
        match std::str::from_utf8(key) {
            Ok(key) => strays.push((key.to_string(), kv.mod_revision)),
            Err(_) => unreadable += 1,
        }
    }
    strays.sort();

    let few = STRAY_FLOOR.max(in_kine.len().div_ceil(20));
    let too_many = !strays.is_empty() && (strays.len() > few || live == 0);
    let delete_now = !strays.is_empty() && (allow_deletes || (!too_many && unreadable == 0));
    let blocked = unreadable > 0 || (too_many && !allow_deletes);
    if blocked {
        tracing::error!(
            strays = strays.len(),
            unreadable,
            keys = in_kine.len(),
            live_rows = live,
            %prefix,
            "reconcile found kine keys without a resource that it does not delete on its own; \
             an admin can allow it with POST /admin/rollout/reconcile {{\"allow_deletes\": true}}, \
             and a key that is not UTF-8 has to be removed by hand"
        );
    }
    let mut deleted_strays = 0;
    if delete_now {
        // The list may predate a takeover: deleting by it would delete the
        // new owner's documents.
        still_owner(kine, ctx, mine).await?;
        let keys: Vec<&str> = strays.iter().map(|(key, _)| key.as_str()).collect();
        let plan = json!({"allow_deletes": allow_deletes, "keys": keys});
        audit_environment(conn, ctx, by, "rollout.reconcile.planned", plan).await?;
        for (key, revision) in &strays {
            fence(conn).await?;
            if let WriteOutcome::Applied { .. } = kine.delete(key, *revision).await? {
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
            let current = in_kine.get(key_for(&ctx.env, kind, id).as_bytes());
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
    if by.is_none() && delete_now {
        let outcome = json!({
            "allow_deletes": allow_deletes,
            "deleted_strays": deleted_strays,
            "marked_dirty": marked_dirty,
        });
        audit_environment(conn, ctx, None, "rollout.reconcile", outcome).await?;
    }
    Ok(ReconcileReport {
        deleted_strays,
        marked_dirty,
        blocked,
    })
}

// ---- owning the environment ------------------------------------------------

/// The database id a claim value names, if it names one.
fn claim_owner(value: &[u8]) -> Option<Uuid> {
    serde_json::from_slice::<Value>(value)
        .ok()?
        .get(DATABASE_ID)?
        .as_str()?
        .parse()
        .ok()
}

/// The claim's value for database `id`.
fn claim_value(id: Uuid) -> Vec<u8> {
    json!({ "database_id": id }).to_string().into_bytes()
}

/// [`ProjectError::OwnedElsewhere`] unless the claim `kv` names `mine`.
fn not_mine(kv: &KeyValue, mine: Uuid) -> Result<(), ProjectError> {
    match claim_owner(&kv.value) {
        Some(owner) if owner == mine => Ok(()),
        owner => Err(ProjectError::OwnedElsewhere { owner, mine }),
    }
}

/// Refuses while the claim names another database. Reads only, and an
/// absent claim does not refuse: a one-off pass does not claim an
/// environment.
async fn still_owner(
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    mine: Uuid,
) -> Result<(), ProjectError> {
    match kine.get(&claim_key(&ctx.env)).await? {
        Some(kv) => not_mine(&kv, mine),
        None => Ok(()),
    }
}

/// [`still_owner`] for a one-off pass; this database's id.
async fn check_claim(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
) -> Result<Uuid, ProjectError> {
    let mine = database_id(conn).await?;
    still_owner(kine, ctx, mine).await?;
    Ok(mine)
}

/// Makes sure the claim names `mine`: reads it, and, when it is absent and
/// `may_create`, claims the environment. [`ProjectError::OwnedElsewhere`]
/// when it names another database, or is absent and may not be created: a
/// database told that another owns the environment takes it only by an
/// audited takeover, never because the claim went missing.
async fn ensure_claim(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    mine: Uuid,
    may_create: bool,
) -> Result<(), ProjectError> {
    let key = claim_key(&ctx.env);
    for _ in 0..ATTEMPTS {
        if let Some(kv) = kine.get(&key).await? {
            return not_mine(&kv, mine);
        }
        if !may_create {
            return Err(ProjectError::OwnedElsewhere { owner: None, mine });
        }
        fence(conn).await?;
        match kine.create(&key, &claim_value(mine)).await? {
            WriteOutcome::Applied { .. } => return Ok(()),
            WriteOutcome::Conflict { current: Some(kv) } => return not_mine(&kv, mine),
            // Removed between the create and its read: claim it again.
            WriteOutcome::Conflict { current: None } => {}
        }
    }
    Err(ProjectError::Kine(KineError::Unexpected("claim")))
}

/// Waits up to `limit`, or until `projection_blocked` is cleared, as a
/// takeover clears it.
async fn wait_while_blocked(db: &Db, limit: Duration) {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        tokio::time::sleep(POLL).await;
        let blocked: Result<bool, sqlx::Error> =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM cp_settings WHERE key = $1)")
                .bind(PROJECTION_BLOCKED)
                .fetch_one(db.pool())
                .await;
        if matches!(blocked, Ok(false)) {
            return;
        }
    }
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

/// Why a tenure ended.
enum TenureEnd {
    /// The session ended, failed, or kine stayed unreachable.
    SteppedDown,
    /// Another control plane owns the environment.
    OwnedElsewhere,
}

async fn run(db: &Db, kine: &dyn KineWriter, ctx: &WriteCtx, timing: Timing) {
    let mut backoff = Backoff::new();
    // A follower keeps its session between attempts.
    let mut session = None;
    loop {
        match try_lead(db, kine, ctx, &mut session).await {
            Ok(Some(conn)) => {
                backoff = Backoff::new();
                match lead(db, conn, kine, ctx, timing).await {
                    TenureEnd::SteppedDown => tokio::time::sleep(POLL).await,
                    TenureEnd::OwnedElsewhere => {
                        wait_while_blocked(db, timing.owned_elsewhere_retry).await
                    }
                }
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
        None => lock_session(db).await?,
    };
    if try_lock(&mut conn).await? {
        Ok(Some(conn))
    } else {
        *session = Some(conn);
        Ok(None)
    }
}

/// What a tenure has done so far.
#[derive(Default)]
struct Tenure {
    /// This database's id, read once per tenure.
    database_id: Option<Uuid>,
    claimed: bool,
    reconciled_at: Option<Instant>,
    kine_down_since: Option<Instant>,
}

/// What `projection_stalled` says about a failed pass, if anything.
fn stall_record(e: &ProjectError) -> Option<Value> {
    if e.kine_unavailable() {
        return Some(json!({"error": "kine unavailable"}));
    }
    match e {
        ProjectError::Row {
            kind,
            id,
            seq,
            cause,
        } => Some(json!({"kind": kind, "id": id, "seq": seq, "error": cause.to_string()})),
        _ => None,
    }
}

/// Projects while `conn`'s session holds the lock.
async fn lead(
    db: &Db,
    mut conn: PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    timing: Timing,
) -> TenureEnd {
    tracing::info!("this instance holds the projector lock");
    let mut listener = None;
    let mut backoff = Backoff::new();
    let mut tenure = Tenure::default();
    loop {
        // Wake-ups already queued are covered by the pass about to run.
        if let Some(listener) = listener.as_mut() {
            while PgListener::next_buffered(listener).is_some() {}
        }
        let result = lead_once(&mut conn, kine, ctx, timing, &mut tenure).await;
        if listener.is_none() && tenure.claimed {
            listener = listen(db).await;
        }
        let delay = match result {
            Ok(()) => {
                tenure.kine_down_since = None;
                backoff = Backoff::new();
                POLL
            }
            Err(ProjectError::OwnedElsewhere { owner, mine }) => {
                tracing::error!(
                    claim = %claim_key(&ctx.env),
                    owner = ?owner,
                    database_id = %mine,
                    "another control plane, on another database, owns this environment's \
                     prefix in kine; this one projects nothing"
                );
                let blocked = json!({
                    "reason": "another control plane owns this environment",
                    "owner_database_id": owner,
                    "database_id": mine,
                });
                let _ = settings::set(&mut conn, PROJECTION_BLOCKED, &blocked).await;
                let _ = conn.close().await;
                return TenureEnd::OwnedElsewhere;
            }
            // Contention on a row lock, not a lost session: try again.
            Err(e @ ProjectError::Db(DbError::LockTimeout)) => {
                tracing::warn!(error = %e, "a projection pass waited too long for a row");
                backoff.next()
            }
            Err(e @ (ProjectError::LockLost | ProjectError::NotLeader | ProjectError::Db(_))) => {
                tracing::warn!(error = %e, "the projector stops leading");
                let _ = conn.close().await;
                return TenureEnd::SteppedDown;
            }
            Err(e) => {
                tracing::error!(error = %e, "projection to kine is stopped");
                if let Some(stalled) = stall_record(&e) {
                    if settings::set(&mut conn, PROJECTION_STALLED, &stalled)
                        .await
                        .is_err()
                    {
                        let _ = conn.close().await;
                        return TenureEnd::SteppedDown;
                    }
                }
                if e.kine_unavailable() {
                    let since = *tenure.kine_down_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= timing.kine_down_step_down {
                        tracing::warn!(
                            "kine has been unreachable for {:?}; leaving the projector lock to \
                             an instance that can reach it",
                            timing.kine_down_step_down
                        );
                        let _ = conn.close().await;
                        return TenureEnd::SteppedDown;
                    }
                } else {
                    tenure.kine_down_since = None;
                }
                backoff.next()
            }
        };
        wake(&mut listener, delay).await;
    }
}

/// One round of the leader's work: the claim, reconcile requests, the
/// reconcile a tenure owes (at its start and then every
/// [`Timing::reconcile_every`]), and a projection pass.
async fn lead_once(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    timing: Timing,
    tenure: &mut Tenure,
) -> Result<(), ProjectError> {
    let mine = match tenure.database_id {
        Some(id) => id,
        None => *tenure.database_id.insert(database_id(conn).await?),
    };
    // On every pass: an owner may change while this tenure runs, and two
    // owners would each reconcile the other's documents away. A tenure that
    // has owned the environment may claim it back if the claim vanishes; a
    // database told another owns it may not.
    let may_create = tenure.claimed || !setting_exists(conn, PROJECTION_BLOCKED).await?;
    ensure_claim(conn, kine, ctx, mine, may_create).await?;
    if !tenure.claimed {
        tenure.claimed = true;
        delete_setting(conn, PROJECTION_BLOCKED).await?;
        tracing::info!("this instance now projects resources to kine");
    }
    if serve_requests(conn, kine, ctx, mine).await? {
        tenure.reconciled_at = Some(Instant::now());
    }
    if tenure
        .reconciled_at
        .is_none_or(|at| at.elapsed() >= timing.reconcile_every)
    {
        let report = reconcile_on(conn, kine, ctx, mine, false, None).await?;
        tracing::info!(?report, "reconciled kine with the database");
        tenure.reconciled_at = Some(Instant::now());
    }
    project_pending(conn, kine, ctx, mine).await?;
    delete_setting(conn, PROJECTION_STALLED).await?;
    Ok(())
}

async fn setting_exists(conn: &mut PgConnection, key: &str) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM cp_settings WHERE key = $1)")
            .bind(key)
            .fetch_one(conn)
            .await?,
    )
}

async fn delete_setting(conn: &mut PgConnection, key: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM cp_settings WHERE key = $1")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
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
    mine: Uuid,
) -> Result<bool, ProjectError> {
    // Nobody waits for an answer this old, and a database restored from
    // backup must not replay what an admin once allowed.
    let expired = sqlx::query(
        "DELETE FROM cp_settings WHERE starts_with(key, $1) \
           AND updated_at < now() - make_interval(secs => $2)",
    )
    .bind(REQUEST_PREFIX)
    .bind(ANSWER_WAIT.as_secs_f64())
    .execute(&mut *conn)
    .await?;
    if expired.rows_affected() > 0 {
        tracing::warn!(
            requests = expired.rows_affected(),
            "dropped reconcile requests nobody waits for any more, without running them"
        );
    }
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
            delete_setting(conn, &key).await?;
            continue;
        };
        let operator = OperatorAuth {
            id: request.operator_id,
            email: request.operator_email,
            // Only an admin may ask (`ROUTE_ROLES`).
            role: OperatorRole::Admin,
        };
        let allow_deletes = request.allow_deletes;
        let outcome = reconcile_on(conn, kine, ctx, mine, allow_deletes, Some(&operator)).await;
        let (answer, detail) = match outcome {
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
            Err(
                e @ (ProjectError::LockLost
                | ProjectError::NotLeader
                | ProjectError::OwnedElsewhere { .. }
                | ProjectError::Db(_)),
            ) => return Err(e),
            Err(e) => {
                tracing::error!(error = %e, "a requested reconcile failed");
                (
                    json!({"error": "unavailable"}),
                    json!({"allow_deletes": allow_deletes, "failed": true}),
                )
            }
        };
        let mut tx = conn.begin().await?;
        delete_setting(&mut tx, &key).await?;
        settings::set(&mut tx, &format!("{RESULT_PREFIX}{id}"), &answer).await?;
        audit_environment(&mut tx, ctx, Some(&operator), "rollout.reconcile", detail).await?;
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

// ---- moving an environment ------------------------------------------------

#[derive(Deserialize)]
pub struct OwnershipBody {
    take_over_from: Uuid,
}

/// `409 owned_elsewhere`: the claim does not name the database the request
/// takes the environment over from, or that database is this one.
fn owned_elsewhere() -> ApiError {
    ApiError::conflict(
        "owned_elsewhere",
        "该环境当前不归请求中指定的数据库所有，未做任何更改",
    )
}

/// `POST /admin/rollout/ownership` with `{take_over_from: <database id>}`
/// → `200 {owner_database_id}`: see [`take_over_with`].
pub async fn post_ownership(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<OwnershipBody>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let kine = KineClient::connect(&state.cfg.kine_endpoint)
        .await
        .map_err(|_| ApiError::unavailable())?;
    let ctx = WriteCtx::of(&state);
    let taken = take_over_with(&state.db, &kine, &ctx, &auth, body.take_over_from).await?;
    Ok(Json(taken))
}

/// Moves the environment's prefix in kine to this database, from `from`:
/// the database the claim names, which `projection_blocked` reports, or
/// none when the claim is absent. Every instance of the old control plane
/// should be stopped first; one still running stops writing at its next
/// read of the claim.
///
/// Under the projector lock, the claim is read; then, in one transaction,
/// the audit row is written and `projection_blocked` cleared, the claim is
/// moved by a compare-and-swap at the revision read (a create when it was
/// absent), and the transaction commits only once the claim has moved.
/// `409 owned_elsewhere` when the claim names another database than
/// `from`, when it changed after the read, or when `from` is this database;
/// `503 unavailable` when kine cannot be reached or a projector of this
/// database holds the lock at that moment.
pub async fn take_over_with(
    db: &Db,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    by: &OperatorAuth,
    from: Uuid,
) -> Result<Value, ApiError> {
    let mut conn = match locked_session(db).await {
        Ok(conn) => conn,
        Err(ProjectError::NotLeader) => return Err(ApiError::unavailable()),
        Err(e) => return Err(ApiError::internal(e)),
    };
    let taken = take_over(&mut conn, kine, ctx, by, from).await;
    let _ = conn.close().await;
    taken
}

/// [`take_over_with`] under the lock held by `conn`.
async fn take_over(
    conn: &mut PgConnection,
    kine: &dyn KineWriter,
    ctx: &WriteCtx,
    by: &OperatorAuth,
    from: Uuid,
) -> Result<Value, ApiError> {
    let mine = database_id(conn).await?;
    if from == mine {
        return Err(owned_elsewhere());
    }
    let key = claim_key(&ctx.env);
    let current = kine.get(&key).await.map_err(|_| ApiError::unavailable())?;
    let previous = match &current {
        None => None,
        Some(kv) if claim_owner(&kv.value) == Some(from) => Some(from),
        Some(_) => return Err(owned_elsewhere()),
    };
    let mut tx = conn.begin().await.map_err(DbError::from)?;
    let detail = json!({"from_database_id": previous, "to_database_id": mine});
    audit_environment(
        &mut tx,
        ctx,
        Some(by),
        "rollout.ownership.taken_over",
        detail,
    )
    .await?;
    delete_setting(&mut tx, PROJECTION_BLOCKED).await?;
    if fence(&mut tx).await.is_err() {
        return Err(ApiError::unavailable());
    }
    // At the revision just read: the claim moves only from the owner named,
    // or from nobody.
    let value = claim_value(mine);
    let moved = match &current {
        Some(kv) => kine.update(&key, kv.mod_revision, &value).await,
        None => kine.create(&key, &value).await,
    };
    match moved {
        Ok(WriteOutcome::Applied { .. }) => {}
        // Dropping `tx` rolls the audit row and the cleared flag back.
        Ok(WriteOutcome::Conflict { .. }) => return Err(owned_elsewhere()),
        Err(_) => return Err(ApiError::unavailable()),
    }
    if let Err(e) = tx.commit().await {
        tracing::error!(
            error = %e,
            from = ?previous,
            to = %mine,
            "the environment was taken over, but its audit row could not be committed"
        );
        return Err(DbError::from(e).into());
    }
    tracing::warn!(from = ?previous, to = %mine, "this control plane took the environment over");
    Ok(json!({ "owner_database_id": mine }))
}

#[cfg(test)]
mod tests {
    use aisix_core::config::EtcdConfig;
    use sqlx::postgres::PgConnectOptions;

    use super::{claim_key, claim_owner, lock_session_options};
    use crate::settings::EnvSettings;

    #[test]
    fn the_lock_session_notices_a_dead_peer_within_seconds() {
        let base = PgConnectOptions::new().options([("lock_timeout", 5000)]);
        let options = lock_session_options(&base);
        let set = options.get_options().unwrap_or_default();
        for expected in [
            "-c lock_timeout=5000",
            "-c tcp_keepalives_idle=5",
            "-c tcp_keepalives_interval=5",
            "-c tcp_keepalives_count=3",
            "-c tcp_user_timeout=15000",
        ] {
            assert!(set.contains(expected), "{expected} missing from {set}");
        }
    }

    #[test]
    fn the_claim_lives_outside_every_prefix_a_gateway_reads() {
        let env = EnvSettings {
            env_id: uuid::Uuid::new_v4(),
            etcd_prefix: "/aisix".to_string(),
        };
        let claim = claim_key(&env);
        assert_eq!(claim, format!("/aisix/_cp/{}/owner", env.env_id));
        // The prefixes a gateway of this environment reads, by the gateway's
        // own formula.
        let gateway = EtcdConfig {
            prefix: env.etcd_prefix.clone(),
            env_id: env.env_id.to_string(),
            ..EtcdConfig::default()
        };
        for read in [gateway.effective_prefix(), gateway.global_prefix()] {
            assert!(!claim.starts_with(&read), "{claim} is under {read}");
        }
        let id = uuid::Uuid::new_v4();
        let value = serde_json::json!({ "database_id": id }).to_string();
        assert_eq!(claim_owner(value.as_bytes()), Some(id));
        assert_eq!(claim_owner(b"{}"), None);
    }
}
