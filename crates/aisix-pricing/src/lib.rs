//! The control plane's pricing engine.
//!
//! Pure: no I/O, no clock, no randomness and no global mutable state. The
//! same inputs always give the same charge. Money is integer micro-USD and
//! rates are integer micro-USD per million units; nothing on the charge path
//! is floating point.

mod catalogue;
mod rates;
mod usage;

pub use catalogue::{Catalogue, CatalogueEntry, CatalogueError};
pub use rates::{
    Dimension, LongContextBand, MicroUsd, Rate, RateCard, RateCardError, Rates, ServiceTier,
    TierRates,
};
pub use usage::{BillableUsage, Inconsistency};

/// Stamped on every charge and price revision; bumped on any rule change.
pub const ENGINE_VERSION: u32 = 1;
