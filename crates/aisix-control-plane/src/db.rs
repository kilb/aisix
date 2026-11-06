//! The Postgres data layer: one pool, transactions that outlive a dropped
//! caller, and constraint errors typed by SQLSTATE.
//!
//! Rules every query in this crate follows (Postgres, not SQLite):
//! placeholders are `$n`; integers decoded as `i64` are `bigint`, flags
//! `boolean`, times `timestamptz`; every SUM aggregate is cast `::bigint`,
//! because Postgres sums a `bigint` column into `numeric`, which does not
//! decode as `i64` (enforced over `src/` by [`uncast_sums`]); every
//! `ON CONFLICT … DO UPDATE` reads the proposed row through `EXCLUDED.` and
//! qualifies the existing row's columns with the table name; a unique
//! violation is recognised by SQLSTATE and constraint name, never by message
//! text.

use std::str::FromStr;

use futures::future::BoxFuture;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool};

/// Connections per control-plane instance.
const MAX_CONNECTIONS: u32 = 10;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// SQLSTATE `23505`, with the violated constraint's name.
    #[error("unique constraint {constraint} violated")]
    UniqueViolation { constraint: String },
    /// SQLSTATE `55P03`: a lock was not granted within `lock_timeout`.
    #[error("a lock was not granted within lock_timeout")]
    LockTimeout,
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
    /// Connects, setting `lock_timeout` and `statement_timeout` on every
    /// connection, and applies the migrations.
    pub async fn connect(
        url: &str,
        lock_timeout_ms: u64,
        statement_timeout_ms: u64,
    ) -> Result<Db, DbError> {
        let options = PgConnectOptions::from_str(url)?.options([
            ("lock_timeout", lock_timeout_ms),
            ("statement_timeout", statement_timeout_ms),
        ]);
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect_with(options)
            .await?;
        // sqlx holds an advisory lock for the whole run, so instances that
        // boot together apply each migration once.
        let mut migrator = sqlx::migrate!("./migrations");
        // A database that a newer build has migrated further still opens, so
        // an older instance can run beside a newer one or after a rollback.
        // This is only safe while every migration is additive: new tables,
        // new nullable or defaulted columns, new indexes; nothing dropped,
        // renamed or tightened. A shipped migration is never edited or
        // reordered: sqlx refuses a changed checksum, and a reorder applies
        // the same set in different orders on different databases.
        migrator.set_ignore_missing(true);
        migrator
            .run(&pool)
            .await
            .map_err(|e| DbError::Db(sqlx::Error::Migrate(Box::new(e))))?;
        Ok(Db { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
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
/// `bigint`, either directly (`::bigint` right after the call) or as the
/// first argument of a COALESCE that is (`COALESCE(…, 0)::bigint`).
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
        if !is_bigint_cast(b, close + 1) && !in_bigint_coalesce(b, start) {
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

/// Whether `::bigint` (spaces allowed, any case) starts at `pos`.
fn is_bigint_cast(b: &[u8], pos: usize) -> bool {
    let pos = skip_ws(b, pos);
    if !b[pos..].starts_with(b"::") {
        return false;
    }
    let pos = skip_ws(b, pos + 2);
    b[pos..].starts_with(b"bigint") && !b.get(pos + 6).is_some_and(|&c| is_ident(c))
}

/// Whether the call at `start` is the first argument of a COALESCE whose
/// result is cast to `bigint`.
fn in_bigint_coalesce(b: &[u8], start: usize) -> bool {
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
        && matching_paren(b, paren).is_some_and(|close| is_bigint_cast(b, close + 1))
}
