//! Pricing in the control plane: the price tables ([`store`]), pricing
//! each usage row intake stores, inside the transaction that stores it
//! ([`price_stored`]), and the operators' API over both ([`api`]).
//!
//! # Who a row bills
//!
//! Every gateway of the environment serves every tenant, so a gateway
//! credential can name any tenant in a row. A row is therefore priced for
//! the tenant the control plane's own key record gives its `api_key_id`
//! (`tenant_keys`; a revoked key still resolves, since its usage happened
//! before the revoke), and only when the row's `team_id` is that tenant. A
//! row failing either check is stored unpriced, with its reason as the
//! charge's `error_code`, and logged at ERROR. A row naming neither a key nor
//! a tenant (a request refused before its key was resolved) claims no one and
//! goes to the engine as it is, which bills nobody for it; one that carries
//! billable usage is logged at ERROR too.
//!
//! The tenant and member a charge is filed under come from the same key
//! record (`tenant_keys.tenant_id`, `member_id`), never from the row, so a
//! forged row is filed under the key it names rather than the tenant it
//! claims; the row's own claims stay in `usage_events`. A row whose key does
//! not resolve is filed under no tenant and no member.
//!
//! The charge is computed from the row's token counts and the control
//! plane's own price book; the row's `cost_usd` is never read. The gateway
//! and batch a row came in are the intake's own (`usage_events.dp_id`,
//! `batch_id`), never anything the row says.
//!
//! Pricing never refuses or drops a row: a row that cannot be priced is
//! stored with an `unpriced` charge. Only a database error fails the batch,
//! which the gateway then sends again.

pub mod api;
pub mod baselines;
pub mod import;
pub mod rates;
pub mod store;

use std::collections::{HashMap, HashSet};

use aisix_pricing::{price_event, BillableUsage, Outcome, Payer, PricingError};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::billing::budgets::{self, ChargedRow, CountMode};
use crate::billing::ledger::{self, Account, Posting, Source};
use crate::billing::marks;
use crate::db::DbError;
use store::{record_charge, ChargeError, ChargeIdentity, PriceBookCache, Recorded};

/// A usage row the intake transaction has just stored, as stored.
pub struct StoredEvent {
    pub event_id: String,
    pub occurred_at: DateTime<Utc>,
    pub event: Value,
    pub received_at: DateTime<Utc>,
    /// `usage_events.request_id`: the row's request id as stored, which its
    /// charge carries too.
    pub request_id: Option<String>,
}

/// What pricing a delivery's stored rows came to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Priced {
    /// Rows charged at a sell price.
    pub charged: u64,
    /// Rows stored unpriced.
    pub unpriced: u64,
}

/// Prices each of `stored`, the rows this transaction has just stored, and
/// records its one charge, on `conn`. The book is read on `conn` too, so a
/// database error anywhere fails the whole transaction and nothing of the
/// batch is kept.
///
/// Each row charged above 0 debits its tenant's balance with one ledger
/// entry, posted in the same transaction once every charge is recorded, and
/// counts against every quota of its tenant it falls under
/// (`budgets::count_charges`), before the posting. A charge of 0 (a free
/// model, a full discount) posts and counts nothing, and neither does a row
/// that is not charged. The accounts of every tenant the rows' keys belong
/// to are locked first, in ascending tenant id, so that two batches naming
/// the same tenants cannot deadlock; no tenant row is locked, so a tenant an
/// operator is editing does not hold up intake. An account whose balance
/// the batch moves across its tenant's floor (`marks::crossings`), or one
/// of whose quotas it moves across a limit, is flagged for the settler in
/// the posting's own update of the account, its one update in the batch, so
/// no tenant row is key-shared either; no gateway document is written here,
/// so intake never waits on the resources lock.
pub async fn price_stored(
    conn: &mut PgConnection,
    prices: &PriceBookCache,
    dp_id: Uuid,
    batch_id: Uuid,
    mut stored: Vec<StoredEvent>,
) -> Result<Priced, DbError> {
    let mut priced = Priced::default();
    if stored.is_empty() {
        return Ok(priced);
    }
    let book = prices.current(&mut *conn).await?;
    let keys = key_records(&mut *conn, &stored).await?;
    let mut tenants: Vec<Uuid> = keys.values().map(|k| k.tenant_id).collect();
    let accounts = ledger::lock_accounts(&mut *conn, &mut tenants).await?;
    // A fixed order, by event_id compared as bytes, for the charge inserts
    // and the ledger entries.
    stored.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    let mut debits = Vec::new();
    let mut charged = Vec::new();
    for row in &stored {
        let claim = Claim::of(&row.event);
        let key_uuid = key_id(claim.api_key_id);
        let key = key_uuid.and_then(|id| keys.get(&id));
        let (tenant_id, member_id) = match key {
            Some(k) => (
                k.tenant_id.to_string(),
                k.member_id.map(|m| m.to_string()).unwrap_or_default(),
            ),
            None => (String::new(), String::new()),
        };
        // Who pays: the tenant the key belongs to and the group it names, both
        // from the control plane's own key record, never from the row. The
        // group is read unlocked: `tenant_keys.group_id` is set at most once
        // and never changes after, and a group retired underneath this batch
        // leaves the rate it already in force, which is what the row was
        // billed at anyway.
        let payer = Payer {
            tenant_id: tenant_id.clone(),
            group_id: key.and_then(|k| k.group_id).map(|g| g.to_string()),
        };
        if key.is_some() && claim.user_id != member_id {
            tracing::error!(
                event_id = row.event_id,
                %dp_id,
                %batch_id,
                key_member = member_id,
                "a usage row names a member other than its key's; the key's member is recorded"
            );
        }
        let charge = match attribute(&claim, key) {
            Err(e) => {
                tracing::error!(
                    event_id = row.event_id,
                    %dp_id,
                    %batch_id,
                    code = e.code(),
                    key_tenant = tenant_id,
                    "a usage row does not name the tenant its key belongs to; it is stored \
                     unpriced"
                );
                Err(e)
            }
            Ok(()) => match BillableUsage::deserialize(&row.event) {
                Err(_) => Err(ChargeError::Undecodable),
                Ok(usage) => price_event(&book, &usage, &payer, row.received_at)
                    .map_err(ChargeError::Pricing),
            },
        };
        if charge == Err(ChargeError::Pricing(PricingError::UnknownTenant)) {
            // Only a row naming neither a key nor a tenant gets this far
            // without one.
            tracing::error!(
                event_id = row.event_id,
                %dp_id,
                %batch_id,
                "a usage row carries billable usage but names no key and no tenant; it is \
                 stored unpriced"
            );
        }
        match &charge {
            Ok(p) if p.outcome == Outcome::Charged => priced.charged += 1,
            Ok(_) => {}
            Err(_) => priced.unpriced += 1,
        }
        let ident = ChargeIdentity {
            event_id: &row.event_id,
            occurred_at: row.occurred_at,
            team_id: &tenant_id,
            api_key_id: claim.api_key_id,
            user_id: &member_id,
            requested_model_id: claim.requested_model_id,
            group_id: key.and_then(|k| k.group_id),
            request_id: row.request_id.as_deref(),
        };
        let recorded = record_charge(&mut *conn, &ident, &charge).await?;
        if let (Recorded::Inserted, Ok(p), Some(k), Some(api_key_id)) =
            (recorded, &charge, key, key_uuid)
        {
            if p.outcome == Outcome::Charged && p.charge.0 > 0 {
                debits.push(Posting {
                    tenant_id: k.tenant_id,
                    amount_micro_cny: -p.charge.0,
                    source: Source::Usage {
                        event_id: &row.event_id,
                    },
                });
                charged.push(ChargedRow {
                    tenant_id: k.tenant_id,
                    member_id: k.member_id,
                    api_key_id,
                    price_instant: p.price_instant,
                    amount: p.charge.0,
                });
            }
        }
    }
    if !debits.is_empty() {
        // Counted before the posting, so that every flag, a quota's or the
        // balance's, goes into the posting's one update of each account.
        let mut flag = budgets::count_charges(&mut *conn, &charged, CountMode::Usage).await?;
        flag.extend(marks::crossings(&mut *conn, &balances_after(accounts, &debits)).await?);
        flag.sort();
        flag.dedup();
        ledger::post_flagging(&mut *conn, &debits, &flag).await?;
        if !flag.is_empty() {
            marks::notify(&mut *conn).await?;
        }
    }
    Ok(priced)
}

/// The accounts `postings` post to, each with the balance its tenant's last
/// entry will leave: the locked balance plus the tenant's amounts. A sum
/// past the `i64` range saturates; `ledger::post` refuses it anyway.
fn balances_after(accounts: Vec<Account>, postings: &[Posting<'_>]) -> Vec<Account> {
    let mut moved: HashMap<Uuid, i64> = HashMap::new();
    for posting in postings {
        let sum = moved.entry(posting.tenant_id).or_default();
        *sum = sum.saturating_add(posting.amount_micro_cny);
    }
    accounts
        .into_iter()
        .filter_map(|account| {
            moved.get(&account.tenant_id).map(|&delta| Account {
                balance_micro_cny: account.balance_micro_cny.saturating_add(delta),
                ..account
            })
        })
        .collect()
}

/// The text field `name` of `event`, `""` when it has none.
fn text<'a>(event: &'a Value, name: &str) -> &'a str {
    event.get(name).and_then(Value::as_str).unwrap_or_default()
}

/// What a row says about who it is for, each `""` when it does not say.
#[derive(Debug, Clone, Copy)]
struct Claim<'a> {
    team_id: &'a str,
    api_key_id: &'a str,
    user_id: &'a str,
    requested_model_id: &'a str,
}

impl<'a> Claim<'a> {
    fn of(event: &'a Value) -> Claim<'a> {
        Claim {
            team_id: text(event, "team_id"),
            api_key_id: text(event, "api_key_id"),
            user_id: text(event, "user_id"),
            requested_model_id: text(event, "requested_model_id"),
        }
    }
}

/// A key as the control plane recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyRecord {
    tenant_id: Uuid,
    /// `None` for a key bound to no member.
    member_id: Option<Uuid>,
    /// The provider group the key names, and the rate its rows are charged at.
    /// `None` for a legacy key an older replica minted without one, whose rows
    /// are stored `no_group` and priced again by a run once a group is set.
    group_id: Option<Uuid>,
}

/// The key `text` names: a UUID in the lowercase hyphenated form key ids are
/// projected in and the gateway echoes back. Any other spelling names no key.
fn key_id(text: &str) -> Option<Uuid> {
    Uuid::parse_str(text)
        .ok()
        .filter(|id| id.to_string() == text)
}

/// The record of each key the rows name, from the control plane's own key
/// records, revoked keys included.
async fn key_records(
    conn: &mut PgConnection,
    stored: &[StoredEvent],
) -> Result<HashMap<Uuid, KeyRecord>, DbError> {
    let keys: HashSet<Uuid> = stored
        .iter()
        .filter_map(|row| key_id(text(&row.event, "api_key_id")))
        .collect();
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let keys: Vec<Uuid> = keys.into_iter().collect();
    let records: Vec<(Uuid, Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, tenant_id, member_id, group_id FROM tenant_keys WHERE id = ANY($1)",
    )
    .bind(keys)
    .fetch_all(conn)
    .await?;
    Ok(records
        .into_iter()
        .map(|(id, tenant_id, member_id, group_id)| {
            (
                id,
                KeyRecord {
                    tenant_id,
                    member_id,
                    group_id,
                },
            )
        })
        .collect())
}

/// Whether the row may be priced for the tenant it names (see the module
/// docs): its key resolved to a record of the control plane's (`key`), and
/// that key's tenant is the row's `team_id`. A row naming neither a key nor a
/// tenant claims no one, and passes.
fn attribute(claim: &Claim<'_>, key: Option<&KeyRecord>) -> Result<(), ChargeError> {
    if claim.api_key_id.is_empty() && claim.team_id.is_empty() {
        return Ok(());
    }
    let key = key.ok_or(ChargeError::UnknownApiKey)?;
    if key.tenant_id.to_string() != claim.team_id {
        return Err(ChargeError::TenantMismatch);
    }
    Ok(())
}

/// The `usage_rejects` reason of a row another gateway's differing row
/// already holds the `event_id` of.
pub const FOREIGN_DUPLICATE: &str = "foreign_duplicate";

/// Flags each of `duplicates` (rows this delivery sent whose `event_id` was
/// already stored) that another gateway stored first with a different bill:
/// it is logged at ERROR and its copy kept in `usage_rejects` with reason
/// [`FOREIGN_DUPLICATE`]. The stored row stands either way; this only makes
/// the collision visible, since a credential that can name another gateway's
/// event ids could keep a row from being billed as it happened. A flagged
/// copy is not counted as rejected: the gateway delivered it, and nothing it
/// could do differently would store it.
pub async fn flag_foreign_duplicates(
    conn: &mut PgConnection,
    dp_id: Uuid,
    batch_id: Uuid,
    duplicates: &[(&str, &Value)],
) -> Result<(), DbError> {
    if duplicates.is_empty() {
        return Ok(());
    }
    let ids: Vec<&str> = duplicates.iter().map(|(id, _)| *id).collect();
    let stored: Vec<(String, Uuid, Uuid, Value)> = sqlx::query_as(
        "SELECT event_id, dp_id, batch_id, event FROM usage_events \
         WHERE event_id = ANY($1) AND dp_id <> $2",
    )
    .bind(ids)
    .bind(dp_id)
    .fetch_all(&mut *conn)
    .await?;
    let stored: HashMap<&str, (Uuid, Uuid, &Value)> = stored
        .iter()
        .map(|(id, dp, batch, event)| (id.as_str(), (*dp, *batch, event)))
        .collect();
    let mut flagged: Vec<&Value> = Vec::new();
    for (event_id, sent) in duplicates {
        if let Some((stored_dp_id, stored_batch_id, kept)) = stored.get(event_id) {
            if !bills_alike(kept, sent) {
                tracing::error!(
                    event_id,
                    %dp_id,
                    %batch_id,
                    %stored_dp_id,
                    %stored_batch_id,
                    "a gateway sent a usage row whose event_id another gateway's differing row \
                     already holds; the stored row stands, and the copy is kept in usage_rejects"
                );
                flagged.push(sent);
            }
        }
    }
    if !flagged.is_empty() {
        sqlx::query(
            "INSERT INTO usage_rejects (dp_id, batch_id, reason, raw) \
             SELECT $1, $2, $3, u.raw FROM unnest($4::jsonb[]) WITH ORDINALITY AS u(raw, n) \
             ORDER BY u.n",
        )
        .bind(dp_id)
        .bind(batch_id)
        .bind(FOREIGN_DUPLICATE)
        .bind(flagged)
        .execute(conn)
        .await?;
    }
    Ok(())
}

/// Whether two copies of one event bill alike: the same usage as pricing
/// reads it, under the same key and member. When it occurred is left out:
/// a row emitted again (a batch row a restarted process or another replica
/// attributes again) carries the same id and its own emission time. A copy
/// pricing cannot read is compared whole.
fn bills_alike(stored: &Value, sent: &Value) -> bool {
    fn bill(event: &Value) -> Option<(BillableUsage, &str, &str)> {
        let mut usage = BillableUsage::deserialize(event).ok()?;
        usage.occurred_at = DateTime::<Utc>::default();
        Some((usage, text(event, "api_key_id"), text(event, "user_id")))
    }
    match (bill(stored), bill(sent)) {
        (Some(a), Some(b)) => a == b,
        _ => stored == sent,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const KEY: &str = "0b7e5a52-6a4f-4d3e-9c1b-2f6f4d1d9e10";
    const TENANT: &str = "6f0e8c1a-2b3c-4d5e-8f90-a1b2c3d4e5f6";

    /// What `price_stored` decides for a row naming `team_id` and
    /// `api_key_id`, with one key on record: `KEY`, of `TENANT`.
    fn decide(team_id: &str, api_key_id: &str) -> Result<(), ChargeError> {
        let records = HashMap::from([(
            Uuid::parse_str(KEY).expect("a uuid"),
            KeyRecord {
                tenant_id: Uuid::parse_str(TENANT).expect("a uuid"),
                member_id: None,
                group_id: None,
            },
        )]);
        let claim = Claim {
            team_id,
            api_key_id,
            user_id: "",
            requested_model_id: "",
        };
        attribute(&claim, key_id(api_key_id).and_then(|id| records.get(&id)))
    }

    #[test]
    fn a_row_is_priced_only_for_its_keys_tenant() {
        assert_eq!(decide(TENANT, KEY), Ok(()));
        let other = "11111111-2222-4333-8444-555555555555";
        assert_eq!(decide(other, KEY), Err(ChargeError::TenantMismatch));
        assert_eq!(decide("", KEY), Err(ChargeError::TenantMismatch));
        // The tenant in another spelling is not the tenant.
        let upper_tenant = TENANT.to_uppercase();
        assert_eq!(decide(&upper_tenant, KEY), Err(ChargeError::TenantMismatch));
        let (upper, simple) = (KEY.to_uppercase(), KEY.replace('-', ""));
        for key in ["", other, "k1", upper.as_str(), simple.as_str()] {
            assert_eq!(
                decide(TENANT, key),
                Err(ChargeError::UnknownApiKey),
                "{key}"
            );
        }
        // Claims no one.
        assert_eq!(decide("", ""), Ok(()));
    }

    fn row(prompt: u32, occurred_at: &str) -> Value {
        json!({
            "event_id": "b1",
            "occurred_at": occurred_at,
            "team_id": TENANT,
            "api_key_id": KEY,
            "user_id": "u1",
            "requested_model_id": "m1",
            "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
            "prompt_tokens": prompt,
            "completion_tokens": 10,
        })
    }

    #[test]
    fn copies_that_bill_alike_differ_only_in_when_and_how_they_were_emitted() {
        let first = row(100, "2026-11-06T08:15:30Z");
        let mut again = row(100, "2026-11-06T09:00:00Z");
        again["trace_id"] = json!("00000000000000000000000000000001");
        assert!(bills_alike(&first, &again));

        assert!(!bills_alike(&first, &row(999, "2026-11-06T08:15:30Z")));
        for (field, value) in [
            ("api_key_id", json!("11111111-2222-4333-8444-555555555555")),
            ("user_id", json!("u2")),
            ("team_id", json!("t2")),
            ("requested_model_id", json!("m2")),
            ("cache_status", json!("hit")),
        ] {
            let mut other = first.clone();
            other[field] = value;
            assert!(!bills_alike(&first, &other), "{field}");
        }

        // Copies pricing cannot read are compared whole.
        let mut unreadable = first.clone();
        unreadable["prompt_tokens"] = json!("many");
        assert!(bills_alike(&unreadable, &unreadable.clone()));
        assert!(!bills_alike(&unreadable, &first));
    }
}
