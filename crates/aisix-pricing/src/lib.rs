//! The control plane's pricing engine.
//!
//! Pure: no I/O, no clock, no randomness and no global mutable state. The
//! same inputs always give the same charge. A model's baseline is its
//! official USD list price, in integer micro-USD per million units; a group
//! rate converts that to CNY, in ten-thousandths of a CNY per USD; a charge
//! is integer micro-CNY, rounded once upward with the rate and the tenant's
//! discount inside that one rounding. Only an upstream's cost stays
//! micro-USD. Nothing on the charge path is floating point.

mod book;
mod catalogue;
mod engine;
mod rates;
mod usage;

pub use book::{
    BaselineRow, CostPriceRow, CostSource, DiscountRow, GroupRateRow, InvalidRevision, PriceBook,
    PriceRows, RevisionTable,
};
pub use catalogue::{Catalogue, CatalogueEntry, CatalogueError};
pub use engine::{
    charge_micro_cny, price_event, BaselineRef, ChargeLine, CostPriced, DiscountRef, Exemption,
    Outcome, Payer, PricedEvent, PricingError, PricingWarning, RateRef,
};
pub use rates::{
    cny_per_million, cny_per_token, CnyPerUsd, Dimension, LongContextBand, MicroCny, MicroUsd,
    Rate, RateCard, RateCardError, Rates, ServiceTier, TierRates,
};
pub use usage::{BillableUsage, Inconsistency};

/// Stamped on every charge and price revision; bumped on any rule change.
pub const ENGINE_VERSION: u32 = 2;
