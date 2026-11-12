//! The control plane's pricing engine.
//!
//! Pure: no I/O, no clock, no randomness and no global mutable state. The
//! same inputs always give the same charge. A model's baseline is its
//! official USD list price, in integer micro-USD per million units; a group
//! rate converts that to CNY, in ten-thousandths of a CNY per USD; a charge
//! is integer micro-CNY, rounded once upward with the rate and the tenant's
//! discount inside that one rounding. A cost is the same lines at the
//! serving account's cost rate, also integer micro-CNY and rounded the same
//! way. Nothing on the charge path is floating point.

mod book;
mod catalogue;
mod engine;
mod quote;
mod rates;
mod usage;

pub use book::{
    BaselineRow, CostRateRow, DiscountRow, GroupRateRow, InvalidRevision, PriceBook, PriceRows,
    RevisionTable,
};
pub use catalogue::{Catalogue, CatalogueEntry, CatalogueError};
pub use engine::{
    charge_micro_cny, price_event, BaselineRef, ChargeLine, CostPriced, DiscountRef, Exemption,
    Outcome, Payer, PricedEvent, PricingError, PricingWarning, RateRef,
};
pub use quote::{CustomerQuote, LongContextQuote, PriceRate, QuoteUnit, TierQuote};
pub use rates::{
    cny_per_million, cny_per_token, effective_multiplier, CnyPerUsd, Dimension, LongContextBand,
    MicroCny, Rate, RateCard, RateCardError, Rates, ServiceTier, TierRates,
};
pub use usage::{BillableUsage, Inconsistency};

/// Stamped on every charge and price revision; bumped on any rule change.
pub const ENGINE_VERSION: u32 = 2;
