//! Billing accounts and their ledger.
//!
//! Each tenant has one account (`billing_accounts`), holding its signed
//! balance in micro-USD, and an append-only ledger (`billing_ledger`) of the
//! entries that moved it. The balance on the account is a cache of the
//! ledger: [`post`] moves both in one transaction, under the account's lock,
//! so the balance always equals the sum of the tenant's entries and each
//! entry's `balance_after` the running sum up to it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgTypeInfo, PgValueRef};
use sqlx::{Decode, PgConnection, Postgres, Type};
use uuid::Uuid;

use crate::db::DbError;

/// What a ledger entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// One usage row charged above 0, debited.
    Usage,
    /// One top-up, credited.
    Topup,
    /// One re-pricing run's correction for one tenant.
    Adjustment,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Usage => "usage",
            EntryKind::Topup => "topup",
            EntryKind::Adjustment => "adjustment",
        }
    }

    fn parse(s: &str) -> Option<EntryKind> {
        [EntryKind::Usage, EntryKind::Topup, EntryKind::Adjustment]
            .into_iter()
            .find(|kind| kind.as_str() == s)
    }
}

impl Type<Postgres> for EntryKind {
    fn type_info() -> PgTypeInfo {
        <&str as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <&str as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for EntryKind {
    fn decode(value: PgValueRef<'r>) -> Result<EntryKind, BoxDynError> {
        let s = <&str as Decode<Postgres>>::decode(value)?;
        EntryKind::parse(s).ok_or_else(|| format!("unknown ledger entry kind {s:?}").into())
    }
}

/// The event an entry is posted for; it decides the entry's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'a> {
    /// The usage row `event_id`'s charge.
    Usage { event_id: &'a str },
    /// An approved top-up or an operator credit.
    Topup { topup_id: Uuid },
    /// A re-pricing run's correction.
    Repricing { run_id: Uuid },
}

impl Source<'_> {
    pub fn kind(&self) -> EntryKind {
        match self {
            Source::Usage { .. } => EntryKind::Usage,
            Source::Topup { .. } => EntryKind::Topup,
            Source::Repricing { .. } => EntryKind::Adjustment,
        }
    }
}

/// One entry to post: `amount_micro_usd` moves `tenant_id`'s balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posting<'a> {
    pub tenant_id: Uuid,
    pub amount_micro_usd: i64,
    pub source: Source<'a>,
}

/// One `billing_accounts` row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Account {
    pub tenant_id: Uuid,
    pub balance_micro_usd: i64,
    pub credit_limit_micro_usd: i64,
    /// The id of the tenant's balance `budgets` document.
    pub balance_budget_id: Uuid,
    /// Whether the balance document as last projected says exhausted;
    /// `None` when none has been projected.
    pub marked_exhausted: Option<bool>,
    pub marks_dirty: bool,
}

/// One `billing_ledger` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct LedgerEntry {
    /// Posting order, for paging only.
    pub seq: i64,
    pub tenant_id: Uuid,
    pub kind: EntryKind,
    pub amount_micro_usd: i64,
    pub balance_after_micro_usd: i64,
    pub event_id: Option<String>,
    pub topup_id: Option<Uuid>,
    pub repricing_run_id: Option<Uuid>,
    pub posted_at: DateTime<Utc>,
}

const ACCOUNT_COLUMNS: &str = "tenant_id, balance_micro_usd, credit_limit_micro_usd, \
     balance_budget_id, marked_exhausted, marks_dirty";

/// Tenant `tenant_id`'s account, locked (`FOR UPDATE`) for the rest of the
/// transaction; locking one the transaction already holds is a no-op.
///
/// An account that is missing is inserted first. That insert references
/// the tenant row: it is the one insert ingest can make that does, which
/// [`backfill_accounts`] keeps unreachable by opening every tenant's account
/// at boot and on every housekeeping tick. The tenant row is key-shared
/// before the insert, as the lock order has it, and without waiting: ingest
/// never waits on a tenant row, so when another transaction holds it
/// (`tenants::lock`) this fails at once with [`DbError::LockTimeout`], and
/// the batch is answered `503` and sent again. A caller holding the row
/// itself (`tenants::lock_billing`) is not refused by its own lock. The
/// insert is logged, since it means a tenant was created without its
/// account (by an older control plane during an upgrade, or by hand).
pub async fn lock_account(conn: &mut PgConnection, tenant_id: Uuid) -> Result<Account, DbError> {
    let select =
        format!("SELECT {ACCOUNT_COLUMNS} FROM billing_accounts WHERE tenant_id = $1 FOR UPDATE");
    if let Some(account) = sqlx::query_as(&select)
        .bind(tenant_id)
        .fetch_optional(&mut *conn)
        .await?
    {
        return Ok(account);
    }
    tracing::warn!(
        tenant = %tenant_id,
        "a tenant had no billing account; it is opened now"
    );
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR KEY SHARE NOWAIT")
        .bind(tenant_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO billing_accounts (tenant_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(tenant_id)
        .execute(&mut *conn)
        .await?;
    Ok(sqlx::query_as(&select)
        .bind(tenant_id)
        .fetch_one(conn)
        .await?)
}

/// Locks the accounts of tenants `ids`, in ascending id order (which `ids`
/// is sorted into, without repeats), so that two transactions locking
/// overlapping sets cannot deadlock.
pub async fn lock_accounts(
    conn: &mut PgConnection,
    ids: &mut Vec<Uuid>,
) -> Result<Vec<Account>, DbError> {
    ids.sort();
    ids.dedup();
    let mut accounts = Vec::with_capacity(ids.len());
    for id in ids.iter() {
        accounts.push(lock_account(&mut *conn, *id).await?);
    }
    Ok(accounts)
}

/// Opens an account for every tenant that has none, and returns how many it
/// opened. Each tenant row is key-shared before its account is inserted, in
/// ascending id, as the lock order has it; one another transaction holds is
/// skipped rather than waited for, and opened by a later run.
pub async fn backfill_accounts(conn: &mut PgConnection) -> Result<u64, DbError> {
    Ok(sqlx::query(
        "INSERT INTO billing_accounts (tenant_id) \
         SELECT t.id FROM tenants t \
         WHERE NOT EXISTS (SELECT 1 FROM billing_accounts a WHERE a.tenant_id = t.id) \
         ORDER BY t.id FOR KEY SHARE OF t SKIP LOCKED \
         ON CONFLICT DO NOTHING",
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Posts `postings` and moves each tenant's balance by them, returning the
/// entries in posting order. The caller holds the account lock of every
/// posting's tenant ([`lock_account`]).
///
/// A tenant's postings are entered in their order in the slice, each with
/// the balance after it: the locked balance plus the running sum of the
/// tenant's amounts so far. Every sum is exact; one that leaves the `bigint`
/// range fails the statement rather than wrapping. An amount of 0, or one
/// whose sign its kind does not allow, is refused by the ledger's checks and
/// fails the transaction too.
pub async fn post(
    conn: &mut PgConnection,
    postings: &[Posting<'_>],
) -> Result<Vec<LedgerEntry>, DbError> {
    if postings.is_empty() {
        return Ok(Vec::new());
    }
    let mut tenant_ids = Vec::with_capacity(postings.len());
    let mut kinds = Vec::with_capacity(postings.len());
    let mut amounts = Vec::with_capacity(postings.len());
    let mut event_ids: Vec<Option<&str>> = Vec::with_capacity(postings.len());
    let mut topup_ids: Vec<Option<Uuid>> = Vec::with_capacity(postings.len());
    let mut run_ids: Vec<Option<Uuid>> = Vec::with_capacity(postings.len());
    for posting in postings {
        tenant_ids.push(posting.tenant_id);
        kinds.push(posting.source.kind().as_str());
        amounts.push(posting.amount_micro_usd);
        let (event_id, topup_id, run_id) = match posting.source {
            Source::Usage { event_id } => (Some(event_id), None, None),
            Source::Topup { topup_id } => (None, Some(topup_id), None),
            Source::Repricing { run_id } => (None, None, Some(run_id)),
        };
        event_ids.push(event_id);
        topup_ids.push(topup_id);
        run_ids.push(run_id);
    }
    // The balance is read with a subquery, not a join: a posting whose
    // account is missing then fails on the NOT NULL balance and the foreign
    // key instead of being dropped.
    let mut entries: Vec<LedgerEntry> = sqlx::query_as(
        "INSERT INTO billing_ledger \
         (tenant_id, kind, amount_micro_usd, balance_after_micro_usd, event_id, topup_id, \
          repricing_run_id) \
         SELECT p.tenant_id, p.kind, p.amount, \
                (SELECT a.balance_micro_usd FROM billing_accounts a WHERE a.tenant_id = p.tenant_id) \
                  + SUM(p.amount) OVER (PARTITION BY p.tenant_id ORDER BY p.n)::bigint, \
                p.event_id, p.topup_id, p.run_id \
         FROM unnest($1::uuid[], $2::text[], $3::bigint[], $4::text[], $5::uuid[], $6::uuid[]) \
           WITH ORDINALITY AS p(tenant_id, kind, amount, event_id, topup_id, run_id, n) \
         ORDER BY p.n \
         RETURNING seq, tenant_id, kind, amount_micro_usd, balance_after_micro_usd, event_id, \
                   topup_id, repricing_run_id, posted_at",
    )
    .bind(&tenant_ids)
    .bind(&kinds)
    .bind(&amounts)
    .bind(&event_ids)
    .bind(&topup_ids)
    .bind(&run_ids)
    .fetch_all(&mut *conn)
    .await?;
    entries.sort_by_key(|entry| entry.seq);
    sqlx::query(
        "UPDATE billing_accounts AS a \
         SET balance_micro_usd = a.balance_micro_usd + d.delta, updated_at = now() \
         FROM (SELECT p.tenant_id, SUM(p.amount)::bigint AS delta \
               FROM unnest($1::uuid[], $2::bigint[]) AS p(tenant_id, amount) \
               GROUP BY p.tenant_id) AS d \
         WHERE a.tenant_id = d.tenant_id",
    )
    .bind(&tenant_ids)
    .bind(&amounts)
    .execute(conn)
    .await?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_kind_reads_back_as_written() {
        for kind in [EntryKind::Usage, EntryKind::Topup, EntryKind::Adjustment] {
            assert_eq!(EntryKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_value(kind).expect("serializes"),
                serde_json::json!(kind.as_str())
            );
        }
        assert_eq!(EntryKind::parse("grant"), None);
    }
}
