//! The Postgres data layer: one pool, transactions that outlive a dropped
//! caller, and constraint errors typed by SQLSTATE.
//!
//! Rules every query in this crate follows (Postgres, not SQLite):
//! placeholders are `$n`; integers decoded as `i64` are `bigint`, flags
//! `boolean`, times `timestamptz`; every SUM aggregate is cast `::bigint`,
//! because Postgres sums a `bigint` column into `numeric`, which does not
//! decode as `i64`, or, where the total can pass the `bigint` range, cast
//! `::numeric` explicitly and narrowed before it is decoded (the cast is
//! enforced over `src/` by [`uncast_sums`]; the narrowing is the query's);
//! every
//! `ON CONFLICT … DO UPDATE` reads the proposed row through `EXCLUDED.` and
//! qualifies the existing row's columns with the table name; a unique
//! violation is recognised by SQLSTATE and constraint name, never by message
//! text.

use std::str::FromStr;
use std::time::Duration;

use futures::future::BoxFuture;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};

/// Connections per control-plane instance.
const MAX_CONNECTIONS: u32 = 10;

/// How long a request waits for a connection of the pool, rather than the
/// 30 s default: a gateway gives up on a request after 10 s, and one whose
/// store cannot get a connection is answered `503` before then.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a boot waits to connect and apply the migrations, including the
/// wait for another instance's run to finish. See [`Db::connect`].
pub const MIGRATION_BOUND: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// SQLSTATE `23505`, with the violated constraint's name.
    #[error("unique constraint {constraint} violated")]
    UniqueViolation { constraint: String },
    /// SQLSTATE `55P03`: a lock was not granted within `lock_timeout`.
    #[error("a lock was not granted within lock_timeout")]
    LockTimeout,
    /// Connecting and migrating took longer than the boot allows.
    #[error(
        "the migrations did not finish within {after:?}: another instance may be holding \
         the migration lock, or a migration is stuck"
    )]
    MigrationTimeout { after: Duration },
    #[error(transparent)]
    Db(sqlx::Error),
}

impl From<sqlx::Error> for DbError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            match db.code().as_deref() {
                Some("23505") => {
                    return DbError::UniqueViolation {
                        constraint: db.constraint().unwrap_or_default().to_string(),
                    }
                }
                Some("55P03") => return DbError::LockTimeout,
                _ => {}
            }
        }
        DbError::Db(e)
    }
}

/// The control-plane database. Cheap to clone: it is a handle on one pool.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Applies the migrations, then opens the pool, setting `lock_timeout`
    /// and `statement_timeout` on every pool connection.
    ///
    /// Connecting and migrating together are bounded by [`MIGRATION_BOUND`].
    pub async fn connect(
        url: &str,
        lock_timeout_ms: u64,
        statement_timeout_ms: u64,
    ) -> Result<Db, DbError> {
        Self::connect_within(url, lock_timeout_ms, statement_timeout_ms, MIGRATION_BOUND).await
    }

    /// [`Db::connect`] with the migration bound given.
    ///
    /// Migrations run on a connection of their own with neither timeout.
    /// sqlx holds an advisory lock for the whole run, so instances that boot
    /// together apply each migration once, and the others wait for that run;
    /// a `lock_timeout` there, or a `statement_timeout` on a long migration
    /// statement, would fail every replica's boot over and over.
    ///
    /// Without any limit, though, a run that never ends (a hung peer holding
    /// the lock, a statement blocked forever) would leave every booting
    /// replica waiting silently. So the connect and the run together get one
    /// generous `bound`, after which the boot is refused with
    /// [`DbError::MigrationTimeout`] and the supervisor's restart policy
    /// takes over. The trade-off: a migration that legitimately runs longer
    /// than the bound cannot be applied by a booting instance and has to be
    /// applied out of band. When the bound expires, this connection is
    /// dropped; Postgres rolls back a migration it was running once it
    /// notices the disconnect, which may be only after the current statement
    /// ends.
    pub async fn connect_within(
        url: &str,
        lock_timeout_ms: u64,
        statement_timeout_ms: u64,
        bound: Duration,
    ) -> Result<Db, DbError> {
        let options = PgConnectOptions::from_str(url)?;
        tokio::time::timeout(bound, Self::migrate(&options))
            .await
            .map_err(|_| DbError::MigrationTimeout { after: bound })??;
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options.options([
                ("lock_timeout", lock_timeout_ms),
                ("statement_timeout", statement_timeout_ms),
            ]))
            .await?;
        Ok(Db { pool })
    }

    /// Connects without timeouts and applies the migrations.
    async fn migrate(options: &PgConnectOptions) -> Result<(), DbError> {
        let mut migration_conn = PgConnection::connect_with(
            &options
                .clone()
                .options([("lock_timeout", 0), ("statement_timeout", 0)]),
        )
        .await?;
        // Logged before the wait, so a boot that sits here says why.
        tracing::info!(
            "applying migrations; this waits while another instance holds the migration lock"
        );
        let mut migrator = sqlx::migrate!("./migrations");
        // A database that a newer build has migrated further still opens, so
        // an older instance can run beside a newer one or after a rollback.
        // Most migrations are additive: new tables, new nullable or defaulted
        // columns, new indexes; nothing dropped, renamed or tightened. A
        // migration may also FENCE an old path — `0027`'s `BEFORE INSERT`
        // triggers stop the USD money tables taking rows — and that is a
        // one-way change: while both versions run, an older replica's intake
        // fails its transaction and its batch answers 503, to be sent again,
        // for as long as that replica lives. Fencing is allowed; a migration
        // that drops, renames or retypes a column an older release reads is
        // not, because the row it cannot load is a row that release skips. A
        // shipped migration is never edited or reordered: sqlx refuses a
        // changed checksum, and a reorder applies the same set in different
        // orders on different databases.
        migrator.set_ignore_missing(true);
        let migrated = migrator.run(&mut migration_conn).await;
        // The run's own error is the one to report; closing only spares the
        // server a dangling session.
        let _ = migration_conn.close().await;
        migrated.map_err(|e| DbError::Db(sqlx::Error::Migrate(Box::new(e))))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// A pool of its own on the same database and with the same settings,
    /// of at most `max_connections`: for work that must not queue behind
    /// request traffic on this one. It connects on first use.
    pub fn dedicated(&self, max_connections: u32) -> Db {
        let options = (*self.pool.connect_options()).clone();
        Db {
            pool: PgPoolOptions::new()
                .max_connections(max_connections)
                .connect_lazy_with(options),
        }
    }

    /// [`Db::dedicated`], waiting at most `acquire_timeout` for a connection:
    /// for work that should be refused at once rather than queue when its
    /// pool is busy.
    pub fn dedicated_within(&self, max_connections: u32, acquire_timeout: Duration) -> Db {
        let options = (*self.pool.connect_options()).clone();
        Db {
            pool: PgPoolOptions::new()
                .max_connections(max_connections)
                .acquire_timeout(acquire_timeout)
                .connect_lazy_with(options),
        }
    }

    /// Runs `f` in one READ COMMITTED transaction: committed when it returns
    /// `Ok`, rolled back when it returns `Err`.
    ///
    /// The transaction runs in a task of its own. axum drops a handler's
    /// future when its client disconnects; dropped between `BEGIN` and
    /// `COMMIT`, the work would neither finish nor be cleanly undone, and
    /// the connection would go back to the pool with a transaction still
    /// open and its locks held. In its own task it always runs to the end.
    ///
    /// READ COMMITTED does not serialise read-then-write work: such work
    /// locks its parent row (`SELECT … FOR UPDATE`) or takes a
    /// transaction-scoped advisory lock first.
    pub async fn tx<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: for<'c> FnOnce(&'c mut PgConnection) -> BoxFuture<'c, Result<T, E>> + Send + 'static,
    {
        let pool = self.pool.clone();
        let task = tokio::spawn(async move {
            let mut tx = pool
                .begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
                .await
                .map_err(DbError::from)?;
            match f(&mut tx).await {
                Ok(value) => {
                    tx.commit().await.map_err(DbError::from)?;
                    Ok(value)
                }
                Err(e) => {
                    // The error that ended the work is the one to report; a
                    // failed rollback means the connection is gone, and with
                    // it the transaction.
                    if let Err(rollback) = tx.rollback().await {
                        tracing::warn!(error = %rollback, "rollback failed");
                    }
                    Err(e)
                }
            }
        });
        match task.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            // Cancelled: the runtime is shutting down.
            Err(_) => Err(E::from(DbError::Db(sqlx::Error::WorkerCrashed))),
        }
    }
}

/// Every SUM aggregate call in `sql_source` whose result is not cast to
/// `bigint`, either directly (`::bigint` right after the call, or right
/// after its window clause: `SUM(…) OVER (…)::bigint`) or as the first
/// argument of a COALESCE that is (`COALESCE(…, 0)::bigint`).
///
/// An explicit `::numeric` counts as a cast too, in the same places: it is
/// how a total that can pass the `bigint` range (counts the gateways report
/// as u64) says so. This checks only the cast: narrowing such a total before
/// it is decoded is up to its query. What it refuses is a SUM left to
/// Postgres's implicit `numeric`.
///
/// It reads any text, Rust source included: a call is the word `sum`, in
/// any case, followed by `(`. A longer identifier ending in `sum`, a method
/// call (`.sum()`) and a path (`Agg::Sum(`) are not calls. An unterminated
/// call is reported.
pub fn uncast_sums(sql_source: &str) -> Vec<String> {
    // ASCII lowering keeps every byte offset where it was.
    let lower = sql_source.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = lower[from..].find("sum") {
        let start = from + offset;
        from = start + 3;
        if start > 0 && (is_ident(b[start - 1]) || matches!(b[start - 1], b'.' | b':')) {
            continue;
        }
        let open = skip_ws(b, start + 3);
        if b.get(open) != Some(&b'(') {
            continue;
        }
        let Some(close) = matching_paren(b, open) else {
            let line_end = sql_source[start..]
                .find('\n')
                .map_or(sql_source.len(), |i| start + i);
            found.push(sql_source[start..line_end].to_string());
            continue;
        };
        if !is_sum_cast(b, past_window(b, close + 1)) && !in_cast_coalesce(b, start) {
            found.push(sql_source[start..=close].to_string());
        }
    }
    found
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn skip_ws(b: &[u8], mut pos: usize) -> usize {
    while pos < b.len() && b[pos].is_ascii_whitespace() {
        pos += 1;
    }
    pos
}

/// The index of the `)` closing the `(` at `open`.
fn matching_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &c) in b.iter().enumerate().skip(open) {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Where the result of a call ending at `pos` ends: past its window clause
/// (`OVER (…)` or `OVER name`) when it has one, `pos` otherwise.
fn past_window(b: &[u8], pos: usize) -> usize {
    let over = skip_ws(b, pos);
    if !b[over..].starts_with(b"over") || b.get(over + 4).is_some_and(|&c| is_ident(c)) {
        return pos;
    }
    let window = skip_ws(b, over + 4);
    if b.get(window) == Some(&b'(') {
        return matching_paren(b, window).map_or(pos, |close| close + 1);
    }
    window + b[window..].iter().take_while(|&&c| is_ident(c)).count()
}

/// Whether `::bigint` or `::numeric` (spaces allowed, any case) starts at
/// `pos`.
fn is_sum_cast(b: &[u8], pos: usize) -> bool {
    let pos = skip_ws(b, pos);
    if !b[pos..].starts_with(b"::") {
        return false;
    }
    let pos = skip_ws(b, pos + 2);
    [&b"bigint"[..], b"numeric"]
        .iter()
        .any(|ty| b[pos..].starts_with(ty) && !b.get(pos + ty.len()).is_some_and(|&c| is_ident(c)))
}

/// Whether the call at `start` is the first argument of a COALESCE whose
/// result is cast to `bigint` or `numeric`.
fn in_cast_coalesce(b: &[u8], start: usize) -> bool {
    let Some(paren) = b[..start].iter().rposition(|c| !c.is_ascii_whitespace()) else {
        return false;
    };
    if b[paren] != b'(' {
        return false;
    }
    let name_end = b[..paren]
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    let name_start = b[..name_end]
        .iter()
        .rposition(|&c| !is_ident(c))
        .map_or(0, |i| i + 1);
    &b[name_start..name_end] == b"coalesce"
        && matching_paren(b, paren).is_some_and(|close| is_sum_cast(b, close + 1))
}
