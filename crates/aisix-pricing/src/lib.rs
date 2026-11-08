//! The control plane's pricing engine.
//!
//! Pure: no I/O, no clock, no randomness and no global mutable state. The
//! same inputs always give the same charge. Money is integer micro-USD and
//! rates are integer micro-USD per million units; nothing on the charge path
//! is floating point.

mod book;
mod catalogue;
mod engine;
mod rates;
mod usage;

pub use book::{
    CostPriceRow, CostSource, DiscountRow, InvalidRevision, PriceBook, PriceRows, RevisionTable,
    SellPriceRow,
};
pub use catalogue::{Catalogue, CatalogueEntry, CatalogueError};
pub use engine::{
    charge_micro_cny, price_event, ChargeLine, CostPriced, DiscountRef, Exemption, Outcome,
    PricedEvent, PricingError, PricingWarning, SellRef,
};
pub use rates::{
    cny_per_million, cny_per_token, CnyPerUsd, Dimension, LongContextBand, MicroCny, MicroUsd,
    Rate, RateCard, RateCardError, Rates, ServiceTier, TierRates,
};
pub use usage::{BillableUsage, Inconsistency};

/// Stamped on every charge and price revision; bumped on any rule change.
pub const ENGINE_VERSION: u32 = 1;
