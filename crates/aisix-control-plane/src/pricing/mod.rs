//! Pricing in the control plane: the price tables ([`store`]), and pricing
//! each usage row intake stores, inside the transaction that stores it
//! ([`price_stored`]).
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
//! goes to the engine as it is, which bills nobody for it.
//!
//! The charge is computed from the row's token counts and the control
//! plane's own price book; the row's `cost_usd` is never read. The gateway
//! and batch a row came in are the intake's own (`usage_events.dp_id`,
//! `batch_id`), never anything the row says.
//!
//! Pricing never refuses or drops a row: a row that cannot be priced is
//! stored with an `unpriced` charge. Only a database error fails the batch,
//! which the gateway then sends again.

pub mod store;

use std::collections::{HashMap, HashSet};

use aisix_pricing::{price_event, BillableUsage, Outcome};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::db::DbError;
use store::{record_charge, ChargeError, ChargeIdentity, PriceBookCache};

/// A usage row the intake transaction has just stored, as stored.
pub struct StoredEvent {
    pub event_id: String,
    pub occurred_at: DateTime<Utc>,
    pub event: Value,
    pub received_at: DateTime<Utc>,
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
    let owners = key_owners(&mut *conn, &stored).await?;
    // In event_id order, the order intake stored them in.
    stored.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    for row in &stored {
        let ident = identity(row);
        let charge = match attribute(&ident, &owners) {
            Err(e) => {
                let key_tenant = key_id(ident.api_key_id).and_then(|k| owners.get(&k));
                tracing::error!(
                    event_id = ident.event_id,
                    %dp_id,
                    %batch_id,
                    code = e.code(),
                    ?key_tenant,
                    "a usage row does not name the tenant its key belongs to; it is stored \
                     unpriced"
                );
                Err(e)
            }
            Ok(()) => match BillableUsage::deserialize(&row.event) {
                Err(_) => Err(ChargeError::Undecodable),
                Ok(usage) => {
                    price_event(&book, &usage, row.received_at).map_err(ChargeError::Pricing)
                }
            },
        };
        match &charge {
            Ok(p) if p.outcome == Outcome::Charged => priced.charged += 1,
            Ok(_) => {}
            Err(_) => priced.unpriced += 1,
        }
        record_charge(&mut *conn, &ident, &charge).await?;
    }
    Ok(priced)
}

/// The text field `name` of `event`, `""` when it has none.
fn text<'a>(event: &'a Value, name: &str) -> &'a str {
    event.get(name).and_then(Value::as_str).unwrap_or_default()
}

fn identity(row: &StoredEvent) -> ChargeIdentity<'_> {
    ChargeIdentity {
        event_id: &row.event_id,
        occurred_at: row.occurred_at,
        team_id: text(&row.event, "team_id"),
        api_key_id: text(&row.event, "api_key_id"),
        user_id: text(&row.event, "user_id"),
        requested_model_id: text(&row.event, "requested_model_id"),
    }
}

/// The key `text` names: a UUID in the lowercase hyphenated form key ids are
/// projected in and the gateway echoes back. Any other spelling names no key.
fn key_id(text: &str) -> Option<Uuid> {
    Uuid::parse_str(text)
        .ok()
        .filter(|id| id.to_string() == text)
}

/// The tenant of each key the rows name, from the control plane's own key
/// records, revoked keys included.
async fn key_owners(
    conn: &mut PgConnection,
    stored: &[StoredEvent],
) -> Result<HashMap<Uuid, Uuid>, DbError> {
    let keys: HashSet<Uuid> = stored
        .iter()
        .filter_map(|row| key_id(text(&row.event, "api_key_id")))
        .collect();
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let keys: Vec<Uuid> = keys.into_iter().collect();
    let owners: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, tenant_id FROM tenant_keys WHERE id = ANY($1)")
            .bind(keys)
            .fetch_all(conn)
            .await?;
    Ok(owners.into_iter().collect())
}

/// Whether the row may be priced for the tenant it names (see the module
/// docs): its key is one the control plane issued, and the key's tenant is
/// the row's `team_id`. A row naming neither a key nor a tenant claims no
/// one, and passes.
fn attribute(ident: &ChargeIdentity<'_>, owners: &HashMap<Uuid, Uuid>) -> Result<(), ChargeError> {
    if ident.api_key_id.is_empty() && ident.team_id.is_empty() {
        return Ok(());
    }
    let owner = key_id(ident.api_key_id)
        .and_then(|key| owners.get(&key))
        .ok_or(ChargeError::UnknownApiKey)?;
    if owner.to_string() != ident.team_id {
        return Err(ChargeError::TenantMismatch);
    }
    Ok(())
}

/// Logs at ERROR each of `duplicates` (rows this delivery sent whose
/// `event_id` was already stored) that another gateway stored first with a
/// different bill. The stored row stands either way; this only makes the
/// collision visible, since a credential that can name another gateway's
/// event ids could keep a row from being billed as it happened.
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
    .fetch_all(conn)
    .await?;
    let stored: HashMap<&str, (Uuid, Uuid, &Value)> = stored
        .iter()
        .map(|(id, dp, batch, event)| (id.as_str(), (*dp, *batch, event)))
        .collect();
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
                     already holds; the stored row stands"
                );
            }
        }
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

    fn ident<'a>(team_id: &'a str, api_key_id: &'a str) -> ChargeIdentity<'a> {
        ChargeIdentity {
            event_id: "e",
            occurred_at: DateTime::<Utc>::default(),
            team_id,
            api_key_id,
            user_id: "",
            requested_model_id: "",
        }
    }

    fn owners() -> HashMap<Uuid, Uuid> {
        HashMap::from([(
            Uuid::parse_str(KEY).expect("a uuid"),
            Uuid::parse_str(TENANT).expect("a uuid"),
        )])
    }

    #[test]
    fn a_row_is_priced_only_for_its_keys_tenant() {
        let owners = owners();
        assert_eq!(attribute(&ident(TENANT, KEY), &owners), Ok(()));
        let other = "11111111-2222-4333-8444-555555555555";
        assert_eq!(
            attribute(&ident(other, KEY), &owners),
            Err(ChargeError::TenantMismatch)
        );
        assert_eq!(
            attribute(&ident("", KEY), &owners),
            Err(ChargeError::TenantMismatch)
        );
        // The tenant in another spelling is not the tenant.
        let upper_tenant = TENANT.to_uppercase();
        assert_eq!(
            attribute(&ident(&upper_tenant, KEY), &owners),
            Err(ChargeError::TenantMismatch)
        );
        let (upper, simple) = (KEY.to_uppercase(), KEY.replace('-', ""));
        for key in ["", other, "k1", upper.as_str(), simple.as_str()] {
            assert_eq!(
                attribute(&ident(TENANT, key), &owners),
                Err(ChargeError::UnknownApiKey),
                "{key}"
            );
        }
        // Claims no one.
        assert_eq!(attribute(&ident("", ""), &owners), Ok(()));
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
