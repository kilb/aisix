//! Balance and quota marks: what blocks a tenant at the gateway once its
//! money runs out, or a quota is used up (`billing::budgets`).
//!
//! Every customer tenant past pending has one `budgets` document, its
//! balance mark: `scope: team` over the tenant's own team, written under the
//! id its account mints (`balance_budget_id`) and owned by
//! [`owner`]. While the tenant is [`exhausted`], the document carries an
//! `exhausted` mark the gateway refuses the team's requests on, with a
//! reason that names only the tenant's own scope and a message for its
//! billing mode; it has no `period_start`, so the block holds until the
//! control plane lifts it, and no amounts. The internal tenant is never
//! marked: its usage is the platform's own.
//!
//! Only [`project`] writes a mark, balance and quotas alike, under the
//! tenant's billing lock (`tenants::lock_billing`, so the account is held
//! and neither the balance nor a quota's spend can move underneath): at the
//! billing step of every tenant projection, and from [`settle`]. Usage
//! intake never writes a document, and never takes the resources lock: it
//! only flags the account when a batch moves its balance across the floor
//! ([`crossings`]) or a quota across its limit
//! (`budgets::count_charges`), and the settler (`billing::settler`) settles
//! the flag once the batch has committed.

use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::budgets;
use super::ledger::{self, Account};
use crate::db::{Db, DbError};
use crate::error::ApiError;
use crate::resources::write::WriteCtx;
use crate::resources::Kind;
use crate::tenants::{self, BillingMode, TenantRow, TenantStatus};

/// What a prepaid tenant's refused request is told.
pub const BALANCE_MESSAGE_PREPAID: &str =
    "Account balance exhausted. Ask an owner or billing member of your organization to top up.";

/// What a postpaid tenant's refused request is told.
pub const BALANCE_MESSAGE_POSTPAID: &str =
    "Credit limit reached. Contact the platform to settle the balance.";

/// The channel ingest notifies when it flags an account.
pub const DIRTY_CHANNEL: &str = "billing_marks_dirty";

/// Whether a tenant in `mode` with `balance` and `credit_limit` (micro-USD)
/// is out of money. A prepaid tenant is at its floor at 0, whatever its
/// credit limit; a postpaid one at minus its credit limit. A sum that leaves
/// the `i64` range counts as exhausted.
pub fn exhausted(mode: BillingMode, balance: i64, credit_limit: i64) -> bool {
    match mode {
        BillingMode::Prepaid => balance <= 0,
        BillingMode::Postpaid => balance
            .checked_add(credit_limit)
            .is_none_or(|available| available <= 0),
    }
}

/// Who owns tenant `tenant_id`'s billing documents.
pub fn owner(tenant_id: Uuid) -> String {
    format!("billing:{tenant_id}")
}

fn message(mode: BillingMode) -> &'static str {
    match mode {
        BillingMode::Prepaid => BALANCE_MESSAGE_PREPAID,
        BillingMode::Postpaid => BALANCE_MESSAGE_POSTPAID,
    }
}

/// Tenant `tenant_id`'s balance document: over its own team, with an
/// `exhausted` mark when `exhausted` is true. `period` is required by the
/// document and means nothing here: the mark has no `period_start`, so it
/// holds in every period.
pub fn balance_doc(tenant_id: Uuid, mode: BillingMode, exhausted: bool) -> Value {
    let mut doc = json!({"scope": "team", "scope_ref": tenant_id, "period": "day"});
    if exhausted {
        doc["exhausted"] = json!({"reason": {
            "message": message(mode),
            "scope": "team",
            "scope_ref": tenant_id,
        }});
    }
    doc
}

/// Writes `tenant`'s balance document as its account now stands, records
/// what it says on the account and clears the account's flag, then writes
/// the document of every live quota of the tenant as its spend now stands
/// (`budgets::project`). Writes nothing for the internal tenant. The caller
/// holds the tenant's billing lock (`tenants::lock_billing`); the account is
/// read under it.
pub async fn project(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant: &TenantRow,
) -> Result<(), ApiError> {
    if tenant.is_internal {
        return Ok(());
    }
    let account = ledger::lock_account(&mut *conn, tenant.id).await?;
    let exhausted = exhausted(
        tenant.billing_mode,
        account.balance_micro_usd,
        account.credit_limit_micro_usd,
    );
    tenants::put(
        &mut *conn,
        ctx,
        Kind::Budgets,
        account.balance_budget_id,
        &balance_doc(tenant.id, tenant.billing_mode, exhausted),
        &owner(tenant.id),
    )
    .await?;
    sqlx::query(
        "UPDATE billing_accounts SET marked_exhausted = $2, marks_dirty = false \
         WHERE tenant_id = $1",
    )
    .bind(tenant.id)
    .bind(exhausted)
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;
    budgets::project(conn, ctx, tenant.id).await
}

/// The tenants among `after` whose accounts are to be flagged: each one
/// whose balance has crossed its tenant's floor since its mark was last
/// written (or that has never had one). `after` holds the accounts a batch
/// posts to, each with the balance its last entry will leave; the caller
/// holds their locks, and sets the flags in the posting's own update
/// (`ledger::post_flagging`), then calls [`notify`].
///
/// Each tenant's billing mode is read here with a plain read, after the
/// account lock: a mode change holds that lock too, so the mode read is
/// current. The internal tenant is never flagged.
pub async fn crossings(conn: &mut PgConnection, after: &[Account]) -> Result<Vec<Uuid>, DbError> {
    if after.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = after.iter().map(|a| a.tenant_id).collect();
    let tenants: Vec<(Uuid, bool, bool)> = sqlx::query_as(
        "SELECT id, billing_mode = 'postpaid', is_internal FROM tenants WHERE id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let dirty: Vec<Uuid> = after
        .iter()
        .filter(|account| {
            tenants
                .iter()
                .find(|(id, _, _)| *id == account.tenant_id)
                .is_some_and(|&(_, postpaid, internal)| {
                    let mode = if postpaid {
                        BillingMode::Postpaid
                    } else {
                        BillingMode::Prepaid
                    };
                    !internal
                        && account.marked_exhausted
                            != Some(exhausted(
                                mode,
                                account.balance_micro_usd,
                                account.credit_limit_micro_usd,
                            ))
                })
        })
        .map(|account| account.tenant_id)
        .collect();
    Ok(dirty)
}

/// Wakes every instance's settler, once this transaction commits.
pub async fn notify(conn: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query("SELECT pg_notify($1, '')")
        .bind(DIRTY_CHANNEL)
        .execute(conn)
        .await?;
    Ok(())
}

/// Settles tenant `tenant_id`'s marks in a transaction of its own: takes its
/// billing lock and projects them ([`settle_locked`]). For an admin action
/// that changes what a mark depends on, once that change has committed.
pub async fn settle(db: &Db, ctx: &WriteCtx, tenant_id: Uuid) -> Result<(), ApiError> {
    let ctx = WriteCtx {
        mk: crate::crypto::MasterKey(ctx.mk.0),
        env: ctx.env.clone(),
        cp_version: ctx.cp_version.clone(),
        stale_secs: ctx.stale_secs,
    };
    db.tx(move |conn| async move { settle_locked(conn, &ctx, tenant_id).await }.boxed())
        .await
}

/// Takes tenant `tenant_id`'s billing lock and projects its marks, unless
/// it is pending or the internal tenant. A pending tenant projects nothing,
/// and its approval writes the marks; the internal tenant is never marked.
/// Either one's flag is cleared, so that no pass finds it again.
pub(crate) async fn settle_locked(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant_id: Uuid,
) -> Result<(), ApiError> {
    let (tenant, _) = tenants::lock_billing(&mut *conn, tenant_id).await?;
    if tenant.status == TenantStatus::Pending || tenant.is_internal {
        sqlx::query("UPDATE billing_accounts SET marks_dirty = false WHERE tenant_id = $1")
            .bind(tenant_id)
            .execute(conn)
            .await
            .map_err(DbError::from)?;
        return Ok(());
    }
    project(conn, ctx, &tenant).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::validate::{validate_lenient, validate_strict};

    #[test]
    fn prepaid_is_exhausted_at_zero_and_not_one_micro_usd_above() {
        assert!(exhausted(BillingMode::Prepaid, 0, 0));
        assert!(!exhausted(BillingMode::Prepaid, 1, 0));
        assert!(exhausted(BillingMode::Prepaid, -5, 1_000_000));
    }

    #[test]
    fn postpaid_is_exhausted_at_its_credit_limit() {
        assert!(exhausted(BillingMode::Postpaid, -1000, 1000));
        assert!(!exhausted(BillingMode::Postpaid, -999, 1000));
        assert!(exhausted(BillingMode::Postpaid, 0, 0));
        // A sum past the range is exhausted rather than wrapping around.
        assert!(exhausted(BillingMode::Postpaid, i64::MAX, 1));
        assert!(!exhausted(BillingMode::Postpaid, i64::MAX, 0));
    }

    #[test]
    fn the_balance_document_names_only_the_callers_tenant() {
        let t = Uuid::new_v4();
        assert_eq!(
            balance_doc(t, BillingMode::Prepaid, true),
            json!({"scope": "team", "scope_ref": t, "period": "day",
            "exhausted": {"reason": {
                "message": BALANCE_MESSAGE_PREPAID, "scope": "team", "scope_ref": t,
            }}})
        );
        assert_eq!(
            balance_doc(t, BillingMode::Postpaid, true)["exhausted"]["reason"]["message"],
            json!(BALANCE_MESSAGE_POSTPAID)
        );
        for mode in [BillingMode::Prepaid, BillingMode::Postpaid] {
            let doc = balance_doc(t, mode, true);
            for field in ["limit_usd", "spent_usd", "remaining_usd", "period"] {
                assert!(doc["exhausted"]["reason"].get(field).is_none(), "{field}");
            }
            assert!(doc["exhausted"].get("period_start").is_none());
            assert_eq!(
                balance_doc(t, mode, false),
                json!({"scope": "team", "scope_ref": t, "period": "day"})
            );
        }
    }

    #[test]
    fn every_balance_document_loads_on_the_lenient_schema() {
        let t = Uuid::new_v4();
        for mode in [BillingMode::Prepaid, BillingMode::Postpaid] {
            for state in [true, false] {
                let doc = balance_doc(t, mode, state);
                validate_strict(Kind::Budgets, &doc)
                    .unwrap_or_else(|e| panic!("strict: {e:?} {doc}"));
                validate_lenient(Kind::Budgets, &doc)
                    .unwrap_or_else(|e| panic!("lenient: {e} {doc}"));
            }
        }
    }
}
