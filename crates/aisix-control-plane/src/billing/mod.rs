//! Billing in the control plane.
//!
//! Money is integer micro-USD (`i64`, `bigint`) everywhere on the charge,
//! ledger and quota path, never a float and never rounded again, with amounts
//! a person types read as dollars with at most two decimals
//! ([`money::parse_usd`]) and amounts shown as exact decimal strings
//! ([`money::usd_string`]). The ledger is append-only: no code updates or
//! deletes a `billing_ledger` row, a trigger refuses it, and each entry is
//! posted in the transaction of the event that causes it. The lock order is
//! the tenant row, then its billing account (`tenants::lock_billing` takes
//! both, for every path that moves money or decides a mark), then
//! `RESOURCES_LOCK_KEY`, and ingest takes billing accounts only, in ascending
//! tenant id, never waiting on a tenant row. No row ingest inserts references
//! `tenants` directly except one: [`ledger::lock_account`]'s fallback insert
//! of a missing account, which [`ledger::backfill_accounts`], run at boot and
//! on every housekeeping tick, keeps unreachable in practice.

pub mod access;
pub mod accounts;
pub mod budget_routes;
pub mod budgets;
pub mod calls;
pub mod credits;
pub mod ledger;
pub mod lookup_prices;
pub mod lookup_quota;
pub mod lookup_requests;
pub mod marks;
pub mod money;
pub mod plans;
pub mod range;
pub mod reconcile;
pub mod repricing;
pub mod settler;
pub mod statements;
pub mod tenant_prices;
pub mod topups;
