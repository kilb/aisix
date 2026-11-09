//! One usage row into one price: what the tenant is charged, and what the
//! upstream cost the platform.
//!
//! [`price_event`] reads one stored row against a [`PriceBook`] and gives
//! exactly one result: a [`PricedEvent`] (charged, exempt, or nothing to
//! bill), or a [`PricingError`] that leaves the row unpriced. Every amount
//! is an integer, and the charge is rounded once, upward: the model's USD
//! baseline list price, times the payer's group rate in CNY per USD, times
//! the tenant's discount, so any amount above zero charges at least 1 µ¥.

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;

use crate::book::{CostSource, PriceBook};
use crate::rates::{CnyPerUsd, Dimension, MicroCny, MicroUsd, Rate, Rates, ServiceTier};
use crate::usage::{quantities, BillableUsage, Inconsistency, Quantities};
use crate::ENGINE_VERSION;

/// How far past its receipt a row may say it occurred. A gateway whose clock
/// runs fast must not reach a price that starts later.
const CLOCK_SKEW: TimeDelta = TimeDelta::minutes(5);

/// The units a rate is quoted per: `quantity × rate / PER_MILLION` is µ$.
const PER_MILLION: u128 = 1_000_000;

/// Basis points in a whole.
const WHOLE_BPS: u128 = 10_000;

/// Operations whose rows carry no model usage, or only the gateway's own
/// estimate of it: an A2A agent reports no usage, so an A2A row's tokens are
/// the gateway's word count (`aisix-proxy/src/a2a.rs`, `usage_estimated`).
const NOT_MODEL_USAGE: [&str; 5] = ["a2a", "mcp", "files", "batches", "fine_tuning"];

/// Operations an upstream can bill in units no usage counter carries yet:
/// images, audio and video seconds, characters. A successful one that
/// reported no tokens is unpriced, never free. Rerank is added: an upstream
/// that bills rerank by search unit reports no tokens
/// (`aisix-proxy/src/rerank.rs`).
const PRICED_IN_OTHER_UNITS: [&str; 7] = [
    "image_generation",
    "image_edit",
    "transcription",
    "translation",
    "speech",
    "video_generation",
    "rerank",
];

/// The price of one usage row.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct PricedEvent {
    pub event_id: String,
    /// The payer's tenant, never the row's own `team_id`.
    pub tenant_id: String,
    /// The group the payer's key names, if it names one.
    pub group_id: Option<String>,
    pub requested_model_id: String,
    pub occurred_at: DateTime<Utc>,
    /// The instant every price was looked up at:
    /// `min(occurred_at, received_at + 5 min)`.
    pub price_instant: DateTime<Utc>,
    pub outcome: Outcome,
    /// What the tenant owes in micro-CNY; 0 unless `Charged`.
    pub charge: MicroCny,
    /// The baseline that priced the row.
    pub baseline: Option<BaselineRef>,
    /// The group rate that priced the row.
    pub rate: Option<RateRef>,
    /// The discount applied; `None` when the tenant has none in effect.
    pub discount: Option<DiscountRef>,
    /// The tier whose rates were used.
    pub service_tier: Option<ServiceTier>,
    /// The tier the caller asked for, verbatim. It never selects a rate.
    pub requested_service_tier: Option<String>,
    /// The threshold of the long-context band whose rates were used.
    pub long_context_above: Option<u64>,
    /// One line per dimension billed, in `Dimension` order.
    pub lines: Vec<ChargeLine>,
    /// What the upstream cost the platform; `None` when unknown. It names
    /// the upstream channel, so it stays operator-side.
    pub cost: Option<CostPriced>,
    /// The row's usage was estimated by the gateway.
    pub estimated: bool,
    pub warnings: Vec<PricingWarning>,
    pub engine_version: u32,
}

/// Who pays for a row: the tenant, and the group its key names.
///
/// The tenant is the payer's, never the row's own `team_id`: a row is
/// priced for whoever its key belongs to, whatever it claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payer {
    pub tenant_id: String,
    /// The group the key names. `None`: the key names none, so the row is
    /// `NoGroup` rather than priced at a guessed rate.
    pub group_id: Option<String>,
}

/// What became of a row that priced.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Billed at its baseline times its group's rate: `charge` is what the
    /// tenant owes.
    Charged,
    /// Not billed, by rule, whatever it reports.
    Exempt(Exemption),
    /// Nothing billable: every quantity is 0.
    NothingToBill,
}

impl Outcome {
    /// The stored status: `charged`, `exempt` or `nothing_to_bill`.
    pub fn status(&self) -> &'static str {
        match self {
            Outcome::Charged => "charged",
            Outcome::Exempt(_) => "exempt",
            Outcome::NothingToBill => "nothing_to_bill",
        }
    }
}

/// Why a row is not billed.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Exemption {
    /// The response cache served it, exactly or semantically, so no
    /// upstream did; the tokens it reports are the cached response's.
    ResponseCacheHit,
    /// Agent, tool or file traffic, which carries no model usage.
    NotModelUsage,
}

/// The baseline that priced a row.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaselineRef {
    pub revision_id: i64,
}

/// The group rate that priced a row.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateRef {
    pub revision_id: i64,
    /// The rate in effect, ten-thousandths of a CNY per USD.
    pub cny_per_usd: CnyPerUsd,
    /// The model's own override priced it, not the group's default.
    pub model_override: bool,
}

/// The discount revision applied to a row.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscountRef {
    pub revision_id: i64,
    pub bps: u16,
}

/// One billed dimension: `quantity` units at `rate` per million.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeLine {
    pub dimension: Dimension,
    pub quantity: u64,
    pub rate: Rate,
}

/// What a row cost the platform, and which cost price said so.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct CostPriced {
    pub amount: MicroUsd,
    pub source: CostSource,
}

/// Something a charge did not take as the row says.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PricingWarning {
    /// The caller asked for a tier and the upstream reported none served;
    /// the default rates applied.
    ServedTierUnreported { requested: String },
    /// The row claims to have occurred more than five minutes after it was
    /// received; it was priced five minutes after receipt instead.
    OccurredAtClamped { occurred_at: DateTime<Utc> },
}

/// Why a row cannot be priced. It is stored unpriced, to be priced again
/// once the cause is fixed.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum PricingError {
    #[error("the usage row has no event_id")]
    MissingEventId,
    /// Billable usage with an empty `team_id`.
    #[error("billable usage names no tenant")]
    UnknownTenant,
    /// Billable usage with an empty `requested_model_id`.
    #[error("billable usage names no model")]
    UnknownModel,
    /// The payer's key names no group, so no rate can price the row.
    #[error("the payer's key names no provider group")]
    NoGroup,
    #[error("model {model_id} has no baseline in effect at {at}")]
    MissingPrice { model_id: String, at: DateTime<Utc> },
    /// No rate is in effect for the payer's group and the requested model.
    #[error("group {group_id} has no rate for model {model_id} in effect at {at}")]
    MissingRate {
        group_id: String,
        model_id: String,
        at: DateTime<Utc>,
    },
    /// The revision in effect cannot be read by this engine; it is listed in
    /// `PriceBook::invalid_revisions`.
    #[error("price revision {revision_id} is in effect and cannot be read by this engine")]
    InvalidPrice { revision_id: i64 },
    /// The served tier (or `batch`) has no rates on the card. `tier` is the
    /// upstream's own string, so it stays operator-side.
    #[error("the baseline has no rates for service tier {tier:?}")]
    MissingTierPrice { tier: String },
    /// The row bills a quantity the rates in use carry no rate for, even by
    /// fallback. It is refused rather than billed at nothing.
    #[error("the baseline has no rate for {dimension}")]
    MissingDimensionPrice { dimension: Dimension },
    #[error("the usage counters contradict each other: {0:?}")]
    InconsistentUsage(Inconsistency),
    /// A successful row with no tokens of an operation priced in other units.
    #[error("operation {0} reported no quantity this engine can price")]
    UnpricedOperation(String),
    /// A discount above 10000 bps: an operator or a writer error, never an
    /// amount. It is refused rather than clamped, because clamping would bill
    /// a row at a price nobody wrote.
    #[error("discount {bps} bps is above 10000")]
    DiscountAboveWhole { bps: u16 },
    /// An amount past `i64`, or a line product past `u128`. Within
    /// `Rate::MAX`, `CnyPerUsd::MAX` and `MAX_QUANTITY` a row cannot get
    /// there; a caller's own `ChargeLine`s can.
    #[error("the amount does not fit in i64")]
    Overflow,
}

impl PricingError {
    /// The stored error code.
    pub fn code(&self) -> &'static str {
        match self {
            PricingError::MissingEventId => "missing_event_id",
            PricingError::UnknownTenant => "unknown_tenant",
            PricingError::UnknownModel => "unknown_model",
            PricingError::NoGroup => "no_group",
            PricingError::MissingPrice { .. } => "missing_price",
            PricingError::MissingRate { .. } => "missing_rate",
            PricingError::InvalidPrice { .. } => "invalid_price",
            PricingError::MissingTierPrice { .. } => "missing_tier_price",
            PricingError::MissingDimensionPrice { .. } => "missing_dimension_price",
            PricingError::InconsistentUsage(_) => "inconsistent_usage",
            PricingError::UnpricedOperation(_) => "unpriced_operation",
            PricingError::DiscountAboveWhole { .. } => "discount_above_whole",
            PricingError::Overflow => "overflow",
        }
    }
}

/// Prices one stored usage row: the tenant's charge and, when the book knows
/// it, the platform's cost.
///
/// # Which model identity prices what
///
/// A row names two models: the entry the caller addressed
/// (`requested_model_id`) and the upstream that served it
/// (`upstream_provider`, `upstream_model`, `provider_key_id`). The
/// **baseline** keys on the requested entry, the **rate** on the payer's
/// group and the requested entry, and the **cost** on the dispatched
/// upstream, so one platform model costs a tenant what its group charges
/// whichever channel served it. By model shape:
///
/// - direct and embedding models are one entry, so both keys name it;
/// - a routing or semantic group bills the group's price whichever member
///   served, and costs the member;
/// - a wildcard row bills the row's price (the row is the entry a
///   caller-minted alias resolves to), and costs the concrete upstream model;
/// - an ensemble bills each sub-call row at the ensemble's price. It is
///   experimental, and the control plane does not grant it to tenants.
///
/// # Order of evaluation
///
/// Each step decides only when the earlier ones did not:
///
/// 1. An empty `event_id` is `MissingEventId`.
/// 2. Every price is looked up at `price_instant`, which is
///    `min(occurred_at, received_at + 5 min)`. A clamped row is warned
///    `OccurredAtClamped`.
/// 3. A response-cache hit is `Exempt(ResponseCacheHit)`.
/// 4. Agent, tool and file traffic is `Exempt(NotModelUsage)`. Neither
///    exemption looks anything up.
/// 5. Counters that contradict each other are `InconsistentUsage`; they are
///    refused, never clamped.
/// 6. With every quantity 0, a successful operation priced in other units is
///    `UnpricedOperation`; any other row is `NothingToBill`, with no lookup.
/// 7. Without a payer tenant the row is `UnknownTenant`, and without a model
///    `UnknownModel`. A payer whose key names no group is `NoGroup`.
/// 8. The model's baseline in effect: none is `MissingPrice`, an unreadable
///    revision `InvalidPrice`.
/// 9. The payer's group rate for that model in effect, its own override or
///    the group's default: none is `MissingRate`, an unreadable revision
///    `InvalidPrice`.
/// 10. The rates are the served tier's (`batch` for a batch row), at the band
///     the whole prompt selects, cache reads and writes included. A tier the
///     card lacks is `MissingTierPrice`, never another tier's rates. When the
///     caller requested a tier and the upstream reported none, the row is
///     billed at `default` and warned `ServedTierUnreported`; a batch row is
///     billed at `batch` and not warned.
/// 11. Each dimension with a quantity is a line at its rate. A quantity the
///     rates carry no rate for, even by fallback, is `MissingDimensionPrice`:
///     the upstream charged for it, so it is never billed at nothing.
/// 12. The tenant's discount in effect always applies; an unreadable
///     discount revision is `InvalidPrice`.
/// 13. The charge is
///     `⌈Σ quantity × rate × cny_per_usd_e4 × (10000 − bps) / 10¹⁴⌉` µ¥: the
///     one rounding, upward, with the rate and the discount inside it.
/// 14. The cost is the dispatched upstream's cost price at the same tier and
///     band, `⌈Σ quantity × rate / 10⁶⌉` µ$, with no discount and no
///     warnings. A tier it lacks, a quantity it has no rate for, or an
///     unreadable cost revision leaves the cost unknown, never a cost that
///     leaves something out.
pub fn price_event(
    book: &PriceBook,
    usage: &BillableUsage,
    payer: &Payer,
    received_at: DateTime<Utc>,
) -> Result<PricedEvent, PricingError> {
    let u = usage;
    if u.event_id.is_empty() {
        return Err(PricingError::MissingEventId);
    }

    let mut warnings = Vec::new();
    // `received_at` is the control plane's clock, so a bound past the last
    // instant chrono represents is no bound at all.
    let price_instant = match received_at.checked_add_signed(CLOCK_SKEW) {
        Some(latest) if u.occurred_at > latest => {
            warnings.push(PricingWarning::OccurredAtClamped {
                occurred_at: u.occurred_at,
            });
            latest
        }
        _ => u.occurred_at,
    };

    if u.cache_status == "hit" {
        let exempt = Outcome::Exempt(Exemption::ResponseCacheHit);
        return Ok(unbilled(u, payer, price_instant, exempt, warnings));
    }
    if NOT_MODEL_USAGE.contains(&u.operation.as_str()) {
        let exempt = Outcome::Exempt(Exemption::NotModelUsage);
        return Ok(unbilled(u, payer, price_instant, exempt, warnings));
    }

    let q = quantities(u).map_err(PricingError::InconsistentUsage)?;
    if q.is_zero() {
        if (200..300).contains(&u.status_code)
            && PRICED_IN_OTHER_UNITS.contains(&u.operation.as_str())
        {
            return Err(PricingError::UnpricedOperation(u.operation.clone()));
        }
        let nothing = Outcome::NothingToBill;
        return Ok(unbilled(u, payer, price_instant, nothing, warnings));
    }

    if payer.tenant_id.is_empty() {
        return Err(PricingError::UnknownTenant);
    }
    if u.requested_model_id.is_empty() {
        return Err(PricingError::UnknownModel);
    }
    let Some(group_id) = &payer.group_id else {
        return Err(PricingError::NoGroup);
    };
    let baseline = match book.baseline(&u.requested_model_id, price_instant) {
        None => {
            return Err(PricingError::MissingPrice {
                model_id: u.requested_model_id.clone(),
                at: price_instant,
            })
        }
        Some(Err(revision_id)) => return Err(PricingError::InvalidPrice { revision_id }),
        Some(Ok(baseline)) => baseline,
    };
    let rate = match book.rate(group_id, &u.requested_model_id, price_instant) {
        None => {
            return Err(PricingError::MissingRate {
                group_id: group_id.clone(),
                model_id: u.requested_model_id.clone(),
                at: price_instant,
            })
        }
        Some(Err(revision_id)) => return Err(PricingError::InvalidPrice { revision_id }),
        Some(Ok(rate)) => rate,
    };

    let served = u.service_tier.as_deref();
    let input_total = q.input_total();
    let selected = ServiceTier::normalize(served, u.is_batch)
        .and_then(|tier| Some((baseline.card.select(&tier, input_total)?, tier)));
    let Some((selected, tier)) = selected else {
        // Every card carries `default`, so an unbatched row lands here only
        // with a tier its upstream reported.
        return Err(PricingError::MissingTierPrice {
            tier: if u.is_batch {
                ServiceTier::batch().to_string()
            } else {
                served.unwrap_or_default().to_owned()
            },
        });
    };
    if let Some(requested) = &u.requested_service_tier {
        let unreported = !u.is_batch && served.is_none_or(str::is_empty);
        if unreported
            && ServiceTier::normalize(Some(requested), false) != Some(ServiceTier::default_tier())
        {
            warnings.push(PricingWarning::ServedTierUnreported {
                requested: requested.clone(),
            });
        }
    }

    let lines = lines_at(selected.rates, &q)
        .map_err(|dimension| PricingError::MissingDimensionPrice { dimension })?;

    let discount = match book.discount(&payer.tenant_id, price_instant) {
        None => None,
        Some(Ok((revision_id, bps))) => Some(DiscountRef { revision_id, bps }),
        Some(Err(revision_id)) => return Err(PricingError::InvalidPrice { revision_id }),
    };
    let charge = charge_micro_cny(&lines, rate.cny, discount.map_or(0, |d| d.bps))?;

    let cost = cost_of(book, u, &tier, &q, price_instant)?;

    Ok(PricedEvent {
        charge,
        baseline: Some(BaselineRef {
            revision_id: baseline.revision_id,
        }),
        rate: Some(RateRef {
            revision_id: rate.revision_id,
            cny_per_usd: rate.cny,
            model_override: rate.model_override,
        }),
        discount,
        service_tier: Some(tier),
        long_context_above: selected.band_above,
        lines,
        cost,
        ..unbilled(u, payer, price_instant, Outcome::Charged, warnings)
    })
}

/// What `lines` cost a tenant at `cny_per_usd_e4` ten-thousandths of a CNY
/// per USD after a discount of `discount_bps`:
/// `⌈Σ quantity × rate × cny_per_usd_e4 × (10000 − bps) / 10¹⁴⌉` µ¥.
///
/// The CNY rate and the discount are inside the one rounding, upward, so no
/// factor is rounded on its own and any amount above zero is at least 1 µ¥.
/// A discount above 10000 bps is `DiscountAboveWhole`; an amount past `i64`,
/// or a line product past `u128`, is `Overflow`.
/// A line whose product would pass `u128::MAX` is `Overflow`, never a
/// wrapped amount; any line inside that bound returns its exact charge.
pub fn charge_micro_cny(
    lines: &[ChargeLine],
    cny: CnyPerUsd,
    discount_bps: u16,
) -> Result<MicroCny, PricingError> {
    let kept_bps = WHOLE_BPS
        .checked_sub(u128::from(discount_bps))
        .ok_or(PricingError::DiscountAboveWhole { bps: discount_bps })?;
    let factor = u128::from(cny.0) * kept_bps;
    // A u64 × u64 product is below u128::MAX; only the rest is checked.
    let numerator = lines
        .iter()
        .try_fold(0u128, |n, l| {
            (u128::from(l.quantity) * u128::from(l.rate.0))
                .checked_mul(factor)
                .and_then(|line| n.checked_add(line))
        })
        .ok_or(PricingError::Overflow)?;
    let divisor = PER_MILLION * u128::from(CnyPerUsd::ONE.0) * WHOLE_BPS;
    i64::try_from(numerator.div_ceil(divisor))
        .map(MicroCny)
        .map_err(|_| PricingError::Overflow)
}

/// What the dispatched upstream cost the platform, at `tier` and the band
/// the row selects. `None` when the row does not name its upstream, or the
/// book knows no usable cost for it: no readable cost price, no rates for
/// the tier, or no rate for one of the row's quantities.
fn cost_of(
    book: &PriceBook,
    u: &BillableUsage,
    tier: &ServiceTier,
    q: &Quantities,
    at: DateTime<Utc>,
) -> Result<Option<CostPriced>, PricingError> {
    let (Some(provider), Some(model)) = (&u.upstream_provider, &u.upstream_model) else {
        return Ok(None);
    };
    let Some((source, card)) = book.cost(u.provider_key_id.as_deref(), provider, model, at) else {
        return Ok(None);
    };
    let Some(selected) = card.select(tier, q.input_total()) else {
        return Ok(None);
    };
    let Ok(lines) = lines_at(selected.rates, q) else {
        return Ok(None);
    };
    let amount = ceil_micro_usd(sum(&lines), PER_MILLION)?;
    Ok(Some(CostPriced { amount, source }))
}

/// A result that bills nothing: the row's own facts and nothing priced.
fn unbilled(
    u: &BillableUsage,
    payer: &Payer,
    price_instant: DateTime<Utc>,
    outcome: Outcome,
    warnings: Vec<PricingWarning>,
) -> PricedEvent {
    PricedEvent {
        event_id: u.event_id.clone(),
        tenant_id: payer.tenant_id.clone(),
        group_id: payer.group_id.clone(),
        requested_model_id: u.requested_model_id.clone(),
        occurred_at: u.occurred_at,
        price_instant,
        outcome,
        charge: MicroCny(0),
        baseline: None,
        rate: None,
        discount: None,
        service_tier: None,
        requested_service_tier: u.requested_service_tier.clone(),
        long_context_above: None,
        lines: Vec::new(),
        cost: None,
        estimated: u.usage_estimated,
        warnings,
        engine_version: ENGINE_VERSION,
    }
}

/// A line for each dimension `q` bills, at its rate in `rates`; `Err` with
/// the first dimension that has a quantity and no rate, even by fallback.
fn lines_at(rates: &Rates, q: &Quantities) -> Result<Vec<ChargeLine>, Dimension> {
    let mut lines = Vec::new();
    for dimension in Dimension::ALL {
        let quantity = q.get(dimension);
        if quantity == 0 {
            continue;
        }
        let rate = rates.rate_for(dimension).ok_or(dimension)?;
        lines.push(ChargeLine {
            dimension,
            quantity,
            rate,
        });
    }
    Ok(lines)
}

/// `Σ quantity × rate`, exact: at most one line per dimension, and
/// `rates.rs` asserts that even `× 10000` stays inside `u128`.
fn sum(lines: &[ChargeLine]) -> u128 {
    lines
        .iter()
        .map(|l| u128::from(l.quantity) * u128::from(l.rate.0))
        .sum()
}

/// `numerator / divisor` micro-USD, rounded up.
fn ceil_micro_usd(numerator: u128, divisor: u128) -> Result<MicroUsd, PricingError> {
    i64::try_from(numerator.div_ceil(divisor))
        .map(MicroUsd)
        .map_err(|_| PricingError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use aisix_core::reasoning_beside_completion;
    use proptest::prelude::*;
    use serde_json::{json, Value};

    use crate::book::{
        BaselineRow, CostPriceRow, DiscountRow, GroupRateRow, PriceRows, RevisionTable,
    };
    use crate::catalogue::Catalogue;
    use crate::rates::{RateCard, MAX_QUANTITY};

    /// 2026-11-06T00:00:01Z: when the control plane received the test rows.
    const RECEIVED: DateTime<Utc> = match DateTime::from_timestamp(1_793_923_201, 0) {
        Some(t) => t,
        None => panic!("a representable instant"),
    };
    const T0: &str = "2026-01-01T00:00:00Z";
    const OCCURRED: &str = "2026-11-06T00:00:00Z";
    const T1: &str = "2026-01-15T00:00:00Z";
    /// After every test row occurs, so a rate that starts here is out of
    /// force at the row's instant.
    const LATER: &str = "2027-01-01T00:00:00Z";

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    // ---- the synthetic book; no real price ----

    /// A rate set carrying every rate the test card names.
    fn full(input: &str, output: &str, read: &str, write: &str, write_1h: &str) -> Value {
        json!({"input": input, "output": output, "cache_read": read, "cache_write": write,
               "cache_write_1h": write_1h, "web_search": "10000"})
    }

    fn tier(base: Value, band: Value) -> Value {
        json!({"base": base, "long_context": [{"above_input_tokens": 200_000, "rates": band}]})
    }

    /// Every tier and band complete. At `input "1"`, one token costs 1 µ$.
    fn test_card() -> Value {
        let cheap = || {
            tier(
                full("0.5", "1", "0.05", "0.625", "1"),
                full("1", "1.5", "0.1", "1.25", "2"),
            )
        };
        json!({
            "default": tier(full("1", "2", "0.1", "1.25", "2"), full("2", "3", "0.2", "2.5", "4")),
            "flex": cheap(),
            "batch": cheap(),
        })
    }

    /// `test_card` with `web_search` removed from every tier and band.
    fn card_without_web_search() -> Value {
        let mut card = test_card();
        for tier in card.as_object_mut().unwrap().values_mut() {
            tier["base"].as_object_mut().unwrap().remove("web_search");
            for band in tier["long_context"].as_array_mut().unwrap() {
                band["rates"].as_object_mut().unwrap().remove("web_search");
            }
        }
        card
    }

    /// A model's baseline: `card` is its official USD list price.
    fn baseline(revision_id: i64, model_id: &str, from: &str, card: Value) -> BaselineRow {
        BaselineRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            model_id: model_id.to_owned(),
            effective_from: ts(from),
            card,
        }
    }

    /// Group `g`'s default rate at `e4` ten-thousandths of a CNY per USD.
    fn group_rate(revision_id: i64, from: &str, e4: i64) -> GroupRateRow {
        GroupRateRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            group_id: "g".to_owned(),
            model_id: None,
            effective_from: ts(from),
            cny_per_usd_e4: Some(e4),
        }
    }

    /// Group `g`'s rate for model `model`: a model's own override.
    fn override_(revision_id: i64, model: &str, from: &str, e4: i64) -> GroupRateRow {
        GroupRateRow {
            model_id: Some(model.to_owned()),
            cny_per_usd_e4: Some(e4),
            ..group_rate(revision_id, from, e4)
        }
    }

    /// Group `g`'s default rate of one CNY per USD: the rate at which a
    /// charge in µ¥ is the baseline's in µ$, so ported rows keep their
    /// amounts.
    fn rate_one(revision_id: i64, from: &str) -> GroupRateRow {
        group_rate(revision_id, from, CnyPerUsd::ONE.0.into())
    }

    fn cost(
        revision_id: i64,
        provider_key_id: Option<&str>,
        from: &str,
        card: Value,
    ) -> CostPriceRow {
        CostPriceRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            upstream_provider: "openai".to_owned(),
            upstream_model: "up-1".to_owned(),
            provider_key_id: provider_key_id.map(str::to_owned),
            effective_from: ts(from),
            card,
        }
    }

    fn discount(revision_id: i64, tenant_id: &str, from: &str, discount_bps: i64) -> DiscountRow {
        DiscountRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            tenant_id: tenant_id.to_owned(),
            effective_from: ts(from),
            discount_bps,
        }
    }

    fn book_of(
        baselines: Vec<BaselineRow>,
        rates: Vec<GroupRateRow>,
        cost: Vec<CostPriceRow>,
        discounts: Vec<DiscountRow>,
    ) -> PriceBook {
        let catalogue = Catalogue::from_json(r#"{"version":"2026-11-06","entries":[]}"#).unwrap();
        PriceBook::from_rows(
            PriceRows {
                baselines,
                rates,
                cost,
                discounts,
            },
            Arc::new(catalogue),
        )
    }

    /// Model `m` at `test_card`, for group `g` at a rate of one; tenant `t`
    /// has no discount of its own.
    fn test_book() -> PriceBook {
        book_of(
            vec![baseline(1, "m", T0, test_card())],
            vec![rate_one(1, T0)],
            vec![],
            vec![],
        )
    }

    fn book_without_web_search() -> PriceBook {
        book_of(
            vec![baseline(1, "m", T0, card_without_web_search())],
            vec![rate_one(1, T0)],
            vec![],
            vec![],
        )
    }

    /// A wire row with `event_id`, `occurred_at`, `team_id`,
    /// `requested_model_id` and `status_code` filled in unless the JSON sets
    /// them.
    fn row(json: &str) -> BillableUsage {
        let mut v: Value = serde_json::from_str(json).unwrap();
        let o = v.as_object_mut().unwrap();
        for (k, default) in [
            ("event_id", json!("e")),
            ("occurred_at", json!(OCCURRED)),
            ("team_id", json!("t")),
            ("requested_model_id", json!("m")),
            ("status_code", json!(200)),
        ] {
            o.entry(k).or_insert(default);
        }
        serde_json::from_value(v).unwrap()
    }

    /// The default payer: tenant `t`, group `g`.
    fn payer() -> Payer {
        Payer {
            tenant_id: "t".into(),
            group_id: Some("g".into()),
        }
    }

    fn price(json: &str) -> Result<PricedEvent, PricingError> {
        price_event(&test_book(), &row(json), &payer(), RECEIVED)
    }

    fn charge(json: &str) -> i64 {
        price(json).unwrap().charge.0
    }

    fn charge_for_tenant_with_discount(json: &str, bps: i64) -> i64 {
        let book = book_of(
            vec![baseline(1, "m", T0, test_card())],
            vec![rate_one(1, T0)],
            vec![],
            vec![discount(2, "t", T0, bps)],
        );
        price_event(&book, &row(json), &payer(), RECEIVED)
            .unwrap()
            .charge
            .0
    }

    // ---- one test per cache-token shape ----

    #[test]
    fn an_openai_shape_row_bills_cached_tokens_at_the_read_rate() {
        // 200 + 80 + 400
        assert_eq!(
            charge(r#"{"prompt_tokens":1000,"cached_prompt_tokens":800,"completion_tokens":200}"#),
            680
        );
    }

    #[test]
    fn an_anthropic_shape_row_bills_reads_and_both_write_lifetimes() {
        // 37 + 0.9 + 1.25 + 6 + 104 = 149.15, rounded up.
        assert_eq!(
            charge(
                r#"{"prompt_tokens":37,"cache_read_tokens":9,"cache_creation_tokens":4,
                "cache_creation_5m_tokens":1,"cache_creation_1h_tokens":3,"completion_tokens":52}"#
            ),
            150
        );
    }

    #[test]
    fn a_gemini_row_bills_reasoning_beside_the_completion() {
        assert_eq!(
            charge(
                r#"{"prompt_tokens":100,"completion_tokens":20,"reasoning_tokens":30,"total_tokens":150}"#
            ),
            200
        );
    }

    #[test]
    fn web_searches_are_billed_per_search() {
        assert_eq!(
            charge(r#"{"prompt_tokens":10,"web_search_requests":2}"#),
            20_010
        );
    }

    // ---- what is billed ----

    #[test]
    fn a_zero_token_row_bills_nothing_and_needs_no_price() {
        let p = price(r#"{"team_id":"","requested_model_id":"nope"}"#).unwrap();
        assert_eq!((p.outcome, p.charge), (Outcome::NothingToBill, MicroCny(0)));
        // A request whose model name resolved to no entry, and used nothing.
        let p = price(r#"{"requested_model_id":"","status_code":404}"#).unwrap();
        assert_eq!((p.outcome, p.charge), (Outcome::NothingToBill, MicroCny(0)));
    }

    #[test]
    fn a_cache_hit_is_free_even_when_it_reports_tokens() {
        // Review Focus 1
        let p =
            price(r#"{"cache_status":"hit","prompt_tokens":500,"completion_tokens":100}"#).unwrap();
        assert_eq!(
            (p.outcome, p.charge),
            (Outcome::Exempt(Exemption::ResponseCacheHit), MicroCny(0))
        );
    }

    #[test]
    fn a_cache_hit_is_exempt_before_any_lookup_or_reading() {
        // No tenant, no priced model, counters that contradict each other,
        // and an operation priced in other units: the hit decides first.
        let p = price(
            r#"{"cache_status":"hit","team_id":"","requested_model_id":"nope",
            "operation":"image_generation","prompt_tokens":100,"cached_prompt_tokens":999}"#,
        )
        .unwrap();
        assert_eq!(p.outcome, Outcome::Exempt(Exemption::ResponseCacheHit));
        assert_eq!(
            (p.charge, p.rate, p.lines, p.cost),
            (MicroCny(0), None, vec![], None)
        );
    }

    #[test]
    fn agent_and_tool_traffic_is_not_model_usage() {
        let p = price(
            r#"{"operation":"a2a","usage_estimated":true,"prompt_tokens":40,"completion_tokens":60,"requested_model_id":""}"#,
        )
        .unwrap();
        assert_eq!(p.outcome, Outcome::Exempt(Exemption::NotModelUsage));
        for op in ["mcp", "files", "batches", "fine_tuning"] {
            let p = price(&format!(
                r#"{{"operation":"{op}","prompt_tokens":40,"cached_prompt_tokens":99,"requested_model_id":"nope"}}"#
            ))
            .unwrap();
            assert_eq!(
                (p.outcome, p.charge),
                (Outcome::Exempt(Exemption::NotModelUsage), MicroCny(0)),
                "{op}"
            );
        }
        // A finished batch's row carries the batch's real tokens.
        assert_eq!(
            charge(r#"{"operation":"batch_completion","is_batch":true,"prompt_tokens":1000}"#),
            500
        );
    }

    #[test]
    fn billable_usage_on_an_unpriced_model_is_an_error() {
        assert!(matches!(
            price(r#"{"requested_model_id":"other","prompt_tokens":1}"#),
            Err(PricingError::MissingPrice { .. })
        ));
        assert_eq!(
            price(r#"{"requested_model_id":"other","prompt_tokens":1}"#),
            Err(PricingError::MissingPrice {
                model_id: "other".into(),
                at: ts(OCCURRED),
            })
        );
    }

    #[test]
    fn billable_usage_without_a_model_or_tenant_is_an_error() {
        assert_eq!(
            price(r#"{"requested_model_id":"","prompt_tokens":1}"#),
            Err(PricingError::UnknownModel)
        );
        // The payer names the tenant: the row's own empty `team_id` prices
        // nothing differently.
        let p = price(r#"{"team_id":"","prompt_tokens":1}"#).unwrap();
        assert_eq!((p.tenant_id.as_str(), p.charge), ("t", MicroCny(1)));
        let anonymous = Payer {
            tenant_id: String::new(),
            group_id: Some("g".into()),
        };
        assert_eq!(
            price_event(
                &test_book(),
                &row(r#"{"prompt_tokens":1}"#),
                &anonymous,
                RECEIVED
            ),
            Err(PricingError::UnknownTenant)
        );
    }

    #[test]
    fn a_row_without_an_event_id_is_an_error_whatever_else_it_is() {
        for json in [
            r#"{"event_id":"","prompt_tokens":1}"#,
            r#"{"event_id":"","cache_status":"hit"}"#,
            r#"{"event_id":""}"#,
        ] {
            assert_eq!(price(json), Err(PricingError::MissingEventId), "{json}");
        }
    }

    #[test]
    fn a_model_whose_revision_is_invalid_is_unpriced_with_that_revision() {
        let book = book_of(
            vec![
                baseline(7, "bad", T0, json!({"default": {"base": {"input": "1"}}})),
                baseline(8, "m", T0, test_card()),
            ],
            vec![rate_one(1, T0)],
            vec![],
            vec![],
        );
        let mut u = row(r#"{"prompt_tokens":1000}"#);
        assert_eq!(
            price_event(&book, &u, &payer(), RECEIVED).unwrap().charge,
            MicroCny(1000)
        );
        u.requested_model_id = "bad".into();
        assert_eq!(
            price_event(&book, &u, &payer(), RECEIVED),
            Err(PricingError::InvalidPrice { revision_id: 7 })
        );
    }

    #[test]
    fn an_impossible_cached_count_is_refused_not_billed() {
        // Review Focus 3
        assert_eq!(
            price(r#"{"prompt_tokens":100,"cached_prompt_tokens":999}"#),
            Err(PricingError::InconsistentUsage(
                Inconsistency::CachedExceedsPrompt
            ))
        );
    }

    // ---- tiers ----

    #[test]
    fn a_flex_row_bills_at_flex_rates() {
        assert_eq!(
            charge(r#"{"service_tier":"flex","prompt_tokens":1000,"completion_tokens":100}"#),
            600
        );
    }

    #[test]
    fn a_batch_row_bills_at_batch_rates_whatever_tier_it_reports() {
        let p = price(
            r#"{"is_batch":true,"service_tier":"default","prompt_tokens":1000,"completion_tokens":100}"#,
        )
        .unwrap();
        assert_eq!(
            (p.charge, p.service_tier.unwrap().as_str()),
            (MicroCny(600), "batch")
        );
    }

    #[test]
    fn a_served_premium_tier_the_card_lacks_is_unpriced_not_billed_at_default() {
        // Review Focus 4
        assert_eq!(
            price(r#"{"service_tier":"priority","prompt_tokens":1000}"#),
            Err(PricingError::MissingTierPrice {
                tier: "priority".into()
            })
        );
        // The error carries what the upstream reported, verbatim, including
        // a value no tier can be named.
        for raw in ["PRIORITY", "not a tier!"] {
            let mut u = row(r#"{"prompt_tokens":1000}"#);
            u.service_tier = Some(raw.into());
            assert_eq!(
                price_event(&test_book(), &u, &payer(), RECEIVED),
                Err(PricingError::MissingTierPrice { tier: raw.into() }),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_batch_row_on_a_card_without_a_batch_tier_is_unpriced() {
        let book = book_of(
            vec![baseline(
                1,
                "m",
                T0,
                json!({"default": {"base": {"input": "1", "output": "2"}}}),
            )],
            vec![rate_one(1, T0)],
            vec![],
            vec![],
        );
        let u = row(r#"{"is_batch":true,"service_tier":"default","prompt_tokens":1000}"#);
        assert_eq!(
            price_event(&book, &u, &payer(), RECEIVED),
            Err(PricingError::MissingTierPrice {
                tier: "batch".into()
            })
        );
    }

    #[test]
    fn an_upper_case_default_alias_is_the_default_tier() {
        assert_eq!(
            charge(r#"{"service_tier":"STANDARD","prompt_tokens":1000,"completion_tokens":100}"#),
            1200
        );
    }

    #[test]
    fn a_requested_tier_the_upstream_did_not_report_bills_default_and_says_so() {
        let p = price(r#"{"requested_service_tier":"flex","prompt_tokens":1000}"#).unwrap();
        assert_eq!(p.charge, MicroCny(1000));
        assert_eq!(
            p.warnings,
            vec![PricingWarning::ServedTierUnreported {
                requested: "flex".into()
            }]
        );
        assert_eq!(
            (
                p.service_tier.unwrap().as_str(),
                p.requested_service_tier.as_deref()
            ),
            ("default", Some("flex"))
        );
    }

    #[test]
    fn a_requested_tier_is_noted_only_when_the_upstream_reported_none() {
        let warned = |json: &str| price(json).unwrap().warnings;
        // A reported tier, or a requested default, is nothing to note.
        for json in [
            r#"{"requested_service_tier":"flex","service_tier":"default","prompt_tokens":10}"#,
            r#"{"requested_service_tier":"standard","prompt_tokens":10}"#,
            r#"{"requested_service_tier":"","prompt_tokens":10}"#,
            r#"{"prompt_tokens":10}"#,
            // A batch row is billed at batch rates, not at default.
            r#"{"requested_service_tier":"flex","is_batch":true,"prompt_tokens":10}"#,
        ] {
            assert_eq!(warned(json), vec![], "{json}");
        }
        // An empty served tier is no report; a requested value no tier can
        // be named is not the default.
        for (json, requested) in [
            (
                r#"{"requested_service_tier":"priority","service_tier":"","prompt_tokens":10}"#,
                "priority",
            ),
            (
                r#"{"requested_service_tier":"Not A Tier","prompt_tokens":10}"#,
                "Not A Tier",
            ),
        ] {
            assert_eq!(
                warned(json),
                vec![PricingWarning::ServedTierUnreported {
                    requested: requested.into()
                }],
                "{json}"
            );
        }
    }

    // ---- long-context bands ----

    #[test]
    fn a_long_prompt_bills_every_dimension_at_the_band_rate() {
        // Review Focus 5
        assert_eq!(
            charge(r#"{"prompt_tokens":200000,"completion_tokens":10}"#),
            200_020
        );
        assert_eq!(
            charge(r#"{"prompt_tokens":200001,"completion_tokens":10}"#),
            400_032
        );
    }

    #[test]
    fn a_long_mostly_cached_prompt_bills_the_band_cache_rate() {
        // 150000×2 + 60000×0.2 + 1000×3 + 1×10000
        assert_eq!(
            charge(
                r#"{"prompt_tokens":150000,"cache_read_tokens":60000,"completion_tokens":1000,"web_search_requests":1}"#
            ),
            325_000
        );
    }

    #[test]
    fn the_long_context_threshold_counts_cached_input() {
        assert_eq!(
            price(r#"{"prompt_tokens":100000,"cache_read_tokens":100001}"#)
                .unwrap()
                .long_context_above,
            Some(200_000)
        );
        assert_eq!(
            price(r#"{"prompt_tokens":100000,"cache_read_tokens":100000}"#)
                .unwrap()
                .long_context_above,
            None
        );
    }

    // ---- discounts, overrides and rounding ----

    #[test]
    fn a_discount_is_applied_inside_the_one_rounding() {
        // 11 cache-read tokens = 1.1 µ$; at 1000 bps that is 0.99, which
        // rounds up to 1. Rounding first would give 2.
        assert_eq!(
            charge_for_tenant_with_discount(
                r#"{"prompt_tokens":11,"cached_prompt_tokens":11}"#,
                1_000
            ),
            1
        );
    }

    #[test]
    fn a_billable_row_never_charges_zero() {
        // 0.1 µ$ rounds up to 1.
        assert_eq!(charge(r#"{"prompt_tokens":1,"cached_prompt_tokens":1}"#), 1);
    }

    #[test]
    fn a_full_discount_prices_a_row_at_zero() {
        // 10000 bps is a valid discount: the row is charged, at nothing.
        assert_eq!(
            charge_for_tenant_with_discount(r#"{"prompt_tokens":1000}"#, 10_000),
            0
        );
        assert_eq!(
            charge_for_tenant_with_discount(r#"{"prompt_tokens":1000}"#, 9_999),
            1
        );
    }

    // ---- baselines, group rates and the discount ----

    /// Model `m`'s official list price: USD 4 per million input tokens and
    /// USD 20 per million output tokens.
    fn baseline_card() -> Value {
        json!({"default": {"base": {"input": "4", "output": "20"}}})
    }

    /// Group `g` at `default_e4`, with `overrides` of `(revision_id, model,
    /// from, e4)`; `e4` `None` ends that model's override.
    fn rate_book(default_e4: i64, overrides: &[(i64, &str, &str, Option<i64>)]) -> PriceBook {
        let rates = std::iter::once(group_rate(1, T0, default_e4))
            .chain(overrides.iter().map(|&(id, model, from, e4)| GroupRateRow {
                revision_id: id,
                engine_version: ENGINE_VERSION,
                group_id: "g".to_owned(),
                model_id: Some(model.to_owned()),
                effective_from: ts(from),
                cny_per_usd_e4: e4,
            }))
            .collect();
        book_of(
            vec![baseline(1, "m", T0, baseline_card())],
            rates,
            vec![],
            vec![],
        )
    }

    /// The brief's row: 1,000 input and 500 output tokens, which is USD 14
    /// per million at `baseline_card`.
    fn rate_row() -> BillableUsage {
        row(r#"{"prompt_tokens":1000,"completion_tokens":500}"#)
    }

    fn charged_at(book: &PriceBook, payer: &Payer) -> Result<PricedEvent, PricingError> {
        price_event(book, &rate_row(), payer, RECEIVED)
    }

    /// A payer of group `g`, tenant `t` unless `tenant` names another.
    fn payer_of(tenant: &str) -> Payer {
        Payer {
            tenant_id: tenant.into(),
            group_id: Some("g".into()),
        }
    }

    #[test]
    fn a_row_bills_its_baseline_at_its_groups_rate() {
        let book = rate_book(72_000, &[]);
        let p = charged_at(&book, &payer_of("t")).unwrap();
        assert_eq!((p.outcome, p.charge), (Outcome::Charged, MicroCny(100_800)));
        assert_eq!(p.baseline, Some(BaselineRef { revision_id: 1 }));
        assert_eq!(
            p.rate,
            Some(RateRef {
                revision_id: 1,
                cny_per_usd: CnyPerUsd::parse("7.2").unwrap(),
                model_override: false
            })
        );
        assert_eq!(p.group_id, Some("g".into()));
        assert_eq!(p.discount, None);
    }

    #[test]
    fn a_models_override_beats_the_group_default() {
        // Review Focus: the group default is 5.0, and `m`'s own 6.0 prices it.
        let book = rate_book(50_000, &[(2, "m", T0, Some(60_000))]);
        let p = charged_at(&book, &payer_of("t")).unwrap();
        assert_eq!(p.charge, MicroCny(84_000));
        assert_eq!(
            p.rate,
            Some(RateRef {
                revision_id: 2,
                cny_per_usd: CnyPerUsd::parse("6").unwrap(),
                model_override: true
            })
        );
    }

    #[test]
    fn an_ended_override_gives_way_to_the_default() {
        // The 6.0 override ended at T1, so the default 5.0 prices a row at T1.
        let book = rate_book(50_000, &[(2, "m", T0, Some(60_000)), (3, "m", T1, None)]);
        let mut u = rate_row();
        u.occurred_at = ts(T1);
        let p = price_event(&book, &u, &payer_of("t"), RECEIVED).unwrap();
        assert_eq!(p.charge, MicroCny(70_000));
        assert_eq!(
            p.rate,
            Some(RateRef {
                revision_id: 1,
                cny_per_usd: CnyPerUsd::parse("5").unwrap(),
                model_override: false
            })
        );
    }

    /// A card with a long-context band and a cheaper `flex` tier, so the row's
    /// selected rates are not the base ones.
    fn banded_card() -> Value {
        json!({
            "default": {
                "base": {"input": "4", "output": "20", "cache_read": "1", "cache_write": "5"},
                "long_context": [{"above_input_tokens": 200_000,
                                  "rates": {"input": "8", "output": "45",
                                            "cache_read": "2", "cache_write": "10"}}]
            },
            // A tier must carry the default's bands and every rate dimension.
            "flex": {
                "base": {"input": "5", "output": "12", "cache_read": "1", "cache_write": "5"},
                "long_context": [{"above_input_tokens": 200_000,
                                  "rates": {"input": "9", "output": "20",
                                            "cache_read": "2", "cache_write": "10"}}]
            }
        })
    }

    #[test]
    fn a_band_and_a_tier_are_priced_at_the_group_rate_inside_the_discount() {
        // The group's default is 5.0, `m`'s override is 6.0, and `t` has a
        // 1000-bps discount: the selected rates come from the baseline card
        // and the rate and discount are applied to their sum, once.
        let book = book_of(
            vec![baseline(1, "m", T0, banded_card())],
            vec![group_rate(1, T0, 50_000), override_(2, "m", T0, 60_000)],
            vec![],
            vec![discount(3, "t", T0, 1_000)],
        );
        let override_ref = RateRef {
            revision_id: 2,
            cny_per_usd: CnyPerUsd::parse("6").unwrap(),
            model_override: true,
        };

        // Past the band: 200001 × 8 + 30 × 45 = 1,601,358 µ$; × 6.0 less
        // 1000 bps is 8,647,333.2 µ¥, rounded up once. Each wrong order lands
        // on its own number: the base rates 4/20 give 4,323,262, the group
        // default 5.0 gives 7,206,111, no discount 9,608,148, and rounding the
        // discounted µ$ amount before the rate 8,647,338.
        let mut u = rate_row();
        u.prompt_tokens = 200_001;
        u.completion_tokens = 30;
        let p = price_event(&book, &u, &payer_of("t"), RECEIVED).unwrap();
        assert_eq!(
            (p.service_tier, p.long_context_above),
            (Some(ServiceTier::default_tier()), Some(200_000))
        );
        assert_eq!(p.rate, Some(override_ref));
        assert_eq!(
            p.discount,
            Some(DiscountRef {
                revision_id: 3,
                bps: 1_000
            })
        );
        assert_eq!(p.charge, MicroCny(8_647_334));

        // Below the band and on `flex`: 1000 × 5 + 100 × 12 = 6,200 µ$; × 6.0
        // less 1000 bps is exactly 33,480 µ¥.
        let mut u = rate_row();
        u.prompt_tokens = 1_000;
        u.completion_tokens = 100;
        u.service_tier = Some("flex".into());
        let p = price_event(&book, &u, &payer_of("t"), RECEIVED).unwrap();
        assert_eq!(
            (p.service_tier, p.long_context_above),
            (ServiceTier::normalize(Some("flex"), false), None)
        );
        assert_eq!(p.rate, Some(override_ref));
        assert_eq!(
            p.discount,
            Some(DiscountRef {
                revision_id: 3,
                bps: 1_000
            })
        );
        assert_eq!(p.charge, MicroCny(33_480));
    }

    #[test]
    fn a_group_with_no_rate_in_force_is_missing_rate() {
        // No override, and the group's default starts after the row.
        let book = book_of(
            vec![baseline(1, "m", T0, baseline_card())],
            vec![group_rate(1, LATER, 72_000)],
            vec![],
            vec![],
        );
        assert_eq!(
            charged_at(&book, &payer_of("t")),
            Err(PricingError::MissingRate {
                group_id: "g".into(),
                model_id: "m".into(),
                at: ts(OCCURRED),
            })
        );
        assert_eq!(
            PricingError::MissingRate {
                group_id: "g".into(),
                model_id: "m".into(),
                at: ts(OCCURRED)
            }
            .code(),
            "missing_rate"
        );
    }

    #[test]
    fn a_payer_with_no_group_is_no_group() {
        let book = rate_book(72_000, &[]);
        let no_group = Payer {
            tenant_id: "t".into(),
            group_id: None,
        };
        assert_eq!(charged_at(&book, &no_group), Err(PricingError::NoGroup));
        assert_eq!(PricingError::NoGroup.code(), "no_group");
        // A row that bills nothing needs no group: it is not billed at all.
        let p = price_event(&book, &row("{}"), &no_group, RECEIVED).unwrap();
        assert_eq!((p.outcome, p.charge), (Outcome::NothingToBill, MicroCny(0)));
    }

    #[test]
    fn an_unreadable_rate_is_invalid_and_never_falls_back() {
        // An override stamped by a newer engine decides "invalid", never the
        // default in effect behind it.
        let newer = GroupRateRow {
            revision_id: 4,
            engine_version: ENGINE_VERSION + 1,
            group_id: "g".to_owned(),
            model_id: Some("m".to_owned()),
            effective_from: ts(T0),
            cny_per_usd_e4: Some(60_000),
        };
        let book = book_of(
            vec![baseline(1, "m", T0, baseline_card())],
            vec![group_rate(1, T0, 50_000), newer],
            vec![],
            vec![],
        );
        assert_eq!(
            charged_at(&book, &payer_of("t")),
            Err(PricingError::InvalidPrice { revision_id: 4 })
        );
        assert_eq!(
            book.invalid_revisions()
                .iter()
                .map(|r| (r.table, r.revision_id))
                .collect::<Vec<_>>(),
            [(RevisionTable::Rate, 4)]
        );
        // A default carrying a rate past the cap is listed invalid too, and
        // leaves every row of the group unpriced.
        let book = rate_book(2_000_000, &[]);
        assert_eq!(
            charged_at(&book, &payer_of("t")),
            Err(PricingError::InvalidPrice { revision_id: 1 })
        );
        assert_eq!(
            book.invalid_revisions()
                .iter()
                .map(|r| (r.table, r.revision_id))
                .collect::<Vec<_>>(),
            [(RevisionTable::Rate, 1)]
        );
    }

    #[test]
    fn a_model_without_a_baseline_is_missing_price() {
        let book = book_of(Vec::new(), vec![group_rate(1, T0, 72_000)], vec![], vec![]);
        assert_eq!(
            charged_at(&book, &payer_of("t")),
            Err(PricingError::MissingPrice {
                model_id: "m".into(),
                at: ts(OCCURRED),
            })
        );
    }

    #[test]
    fn the_tenant_discount_applies_after_the_rate() {
        // 100,800 µ¥ at 7.2, less 1000 bps.
        let book = book_of(
            vec![baseline(1, "m", T0, baseline_card())],
            vec![group_rate(1, T0, 72_000)],
            vec![],
            vec![discount(4, "t", T0, 1_000)],
        );
        let p = charged_at(&book, &payer_of("t")).unwrap();
        assert_eq!(p.charge, MicroCny(90_720));
        assert_eq!(
            p.discount,
            Some(DiscountRef {
                revision_id: 4,
                bps: 1_000
            })
        );
    }

    #[test]
    fn the_payer_not_the_row_names_the_tenant() {
        // The row claims `claimed`; the key belongs to `filed`, whose discount
        // applies.
        let book = book_of(
            vec![baseline(1, "m", T0, baseline_card())],
            vec![group_rate(1, T0, 72_000)],
            vec![],
            vec![discount(4, "filed", T0, 2_000)],
        );
        let mut u = rate_row();
        u.team_id = "claimed".into();
        let p = price_event(&book, &u, &payer_of("filed"), RECEIVED).unwrap();
        assert_eq!(p.tenant_id, "filed");
        assert_eq!(
            p.discount,
            Some(DiscountRef {
                revision_id: 4,
                bps: 2_000
            })
        );
        // 100,800 less 2000 bps.
        assert_eq!(p.charge, MicroCny(80_640));
        // The row's own tenant has no discount: the same row prices higher.
        let p = price_event(&book, &u, &payer_of("claimed"), RECEIVED).unwrap();
        assert_eq!(
            (p.tenant_id.as_str(), p.charge),
            ("claimed", MicroCny(100_800))
        );
    }

    #[test]
    fn a_free_group_is_charged_zero() {
        let book = rate_book(0, &[]);
        let p = charged_at(&book, &payer_of("t")).unwrap();
        assert_eq!((p.outcome, p.charge), (Outcome::Charged, MicroCny(0)));
    }

    #[test]
    fn an_invalid_discount_unprices_the_rows_it_would_discount_only() {
        // A discount above 10000 bps is isolated to its tenant.
        let book = book_of(
            vec![baseline(1, "m", T0, baseline_card())],
            vec![group_rate(1, T0, 72_000)],
            vec![],
            vec![discount(4, "t", T0, 10_001)],
        );
        assert_eq!(
            charged_at(&book, &payer_of("t")),
            Err(PricingError::InvalidPrice { revision_id: 4 })
        );
        assert_eq!(
            charged_at(&book, &payer_of("other")).unwrap().charge,
            MicroCny(100_800)
        );
    }

    #[test]
    fn a_web_search_without_a_rate_is_refused_not_billed_at_nothing() {
        // Ruling P2-R15, departing from the brief's sketch (which billed the
        // tokens and warned): the upstream charged for the searches, so a
        // card that cannot price them cannot price the row.
        let book = book_without_web_search();
        assert_eq!(
            price_event(
                &book,
                &row(r#"{"prompt_tokens":10,"web_search_requests":2}"#),
                &payer(),
                RECEIVED
            ),
            Err(PricingError::MissingDimensionPrice {
                dimension: Dimension::WebSearch
            })
        );
        // The same card prices a row that searched nothing.
        let p = price_event(&book, &row(r#"{"prompt_tokens":10}"#), &payer(), RECEIVED).unwrap();
        assert_eq!((p.charge, p.warnings), (MicroCny(10), vec![]));
    }

    #[test]
    fn an_operation_priced_in_other_units_is_unpriced_not_free() {
        assert_eq!(
            price(r#"{"operation":"image_generation"}"#),
            Err(PricingError::UnpricedOperation("image_generation".into()))
        );
        assert_eq!(
            price(r#"{"operation":"rerank"}"#),
            Err(PricingError::UnpricedOperation("rerank".into()))
        );
        for op in [
            "image_edit",
            "transcription",
            "translation",
            "speech",
            "video_generation",
        ] {
            assert_eq!(
                price(&format!(r#"{{"operation":"{op}","status_code":201}}"#)),
                Err(PricingError::UnpricedOperation(op.into())),
                "{op}"
            );
            // A failed call that reported nothing cost nothing.
            let p = price(&format!(r#"{{"operation":"{op}","status_code":500}}"#)).unwrap();
            assert_eq!(p.outcome, Outcome::NothingToBill, "{op}");
        }
        // With tokens, the tokens are billed.
        assert_eq!(
            charge(r#"{"operation":"image_generation","prompt_tokens":10}"#),
            10
        );
        // A token-priced operation that reported none bills nothing.
        let p = price(r#"{"operation":"embeddings"}"#).unwrap();
        assert_eq!(p.outcome, Outcome::NothingToBill);
    }

    // ---- spec §7.3, row by row ----

    #[test]
    fn a_failed_attempt_that_used_tokens_is_billed() {
        assert_eq!(charge(r#"{"status_code":502,"prompt_tokens":100}"#), 100);
    }

    #[test]
    fn an_abandoned_stream_bills_its_estimate_and_marks_it() {
        let p = price(
            r#"{"status_code":499,"usage_estimated":true,"prompt_tokens":100,"completion_tokens":20}"#,
        )
        .unwrap();
        assert_eq!((p.charge, p.estimated), (MicroCny(140), true));
        assert!(!price(r#"{"prompt_tokens":100}"#).unwrap().estimated);
    }

    #[test]
    fn an_output_blocked_by_a_guardrail_bills_what_the_upstream_produced() {
        assert_eq!(
            charge(
                r#"{"guardrail_blocked":true,"status_code":400,"prompt_tokens":100,"completion_tokens":5}"#
            ),
            110
        );
    }

    #[test]
    fn the_gateways_own_cost_figure_is_ignored() {
        assert_eq!(charge(r#"{"cost_usd":999.0,"prompt_tokens":100}"#), 100);
    }

    #[test]
    fn the_gateways_own_embedding_calls_are_never_charged() {
        assert_eq!(
            charge(
                r#"{"prompt_tokens":100,"gateway_embedding_calls":[{"count":1,"purpose":"semantic_cache",
                "embedding_model_id":"e","prompt_tokens":5000,"total_tokens":5000,"usage_source":"upstream","latency_ms":3,"outcome":"ok"}]}"#
            ),
            100
        );
    }

    // ---- the pricing instant ----

    #[test]
    fn a_row_from_a_fast_clock_is_priced_no_later_than_five_minutes_after_receipt() {
        let later = RECEIVED + TimeDelta::minutes(30);
        let later = later.to_rfc3339();
        // A second rate starting a minute after receipt, so `received_at`
        // and the clamped instant resolve differently: only the clamp sees it.
        let soon = (RECEIVED + TimeDelta::minutes(1)).to_rfc3339();
        let nine = GroupRateRow {
            revision_id: 2,
            engine_version: ENGINE_VERSION,
            group_id: "g".to_owned(),
            model_id: None,
            effective_from: ts(&soon),
            cny_per_usd_e4: Some(CnyPerUsd::parse("9").unwrap().0.into()),
        };
        let one = |input: &str| json!({"default": {"base": {"input": input, "output": "1"}}});
        let book = book_of(
            vec![
                baseline(1, "m", T0, one("1")),
                baseline(2, "m", &later, one("9")),
            ],
            vec![rate_one(1, T0), nine],
            vec![
                cost(3, None, T0, one("0.4")),
                cost(4, None, &later, one("0.8")),
            ],
            vec![discount(5, "t", &later, 5_000)],
        );
        let at = |occurred_at: DateTime<Utc>| {
            let mut u = row(
                r#"{"prompt_tokens":1000,"upstream_provider":"openai","upstream_model":"up-1"}"#,
            );
            u.occurred_at = occurred_at;
            price_event(&book, &u, &payer(), RECEIVED).unwrap()
        };

        let occurred_at = RECEIVED + TimeDelta::hours(1);
        let p = at(occurred_at);
        assert_eq!(p.price_instant, RECEIVED + TimeDelta::minutes(5));
        assert_eq!(p.occurred_at, occurred_at);
        assert_eq!(
            p.warnings,
            vec![PricingWarning::OccurredAtClamped { occurred_at }]
        );
        // Every lookup is at the clamped instant: baseline rev 1, cost rev 3,
        // rate rev 2 (which starts after receipt) and no discount yet. A rate
        // resolved at `received_at` would find rev 1 and charge 1000 µ¥.
        assert_eq!(p.baseline.unwrap().revision_id, 1);
        assert_eq!(
            p.rate,
            Some(RateRef {
                revision_id: 2,
                cny_per_usd: CnyPerUsd::parse("9").unwrap(),
                model_override: false
            })
        );
        assert_eq!((p.charge, p.discount), (MicroCny(9000), None));
        assert_eq!(
            p.cost,
            Some(CostPriced {
                amount: MicroUsd(400),
                source: CostSource::Upstream { revision_id: 3 },
            })
        );

        // Up to five minutes ahead is believed; a nanosecond more is not.
        let edge = RECEIVED + TimeDelta::minutes(5);
        let p = at(edge);
        assert_eq!((p.price_instant, p.warnings), (edge, vec![]));
        assert_eq!(p.charge, MicroCny(9000));
        let p = at(edge + TimeDelta::nanoseconds(1));
        assert_eq!(p.price_instant, edge);
        assert_eq!(p.charge, MicroCny(9000));
        // A row from the past is priced when it occurred: a second before
        // receipt, so the second rate is not yet in force.
        let p = at(ts(OCCURRED));
        assert_eq!((p.price_instant, p.warnings), (ts(OCCURRED), vec![]));
        assert_eq!(p.rate.unwrap().revision_id, 1);
        assert_eq!(p.charge, MicroCny(1000));
    }

    #[test]
    fn a_late_row_is_priced_when_it_occurred_not_when_it_arrived() {
        // Review Focus 2: revision 2 of each price starts between the event
        // and its receipt.
        let boundary = ts(OCCURRED) + TimeDelta::minutes(10);
        let received = boundary + TimeDelta::minutes(20);
        let b = boundary.to_rfc3339();
        let one = |input: &str| json!({"default": {"base": {"input": input, "output": "1"}}});
        let book = book_of(
            vec![
                baseline(1, "m", T0, one("1")),
                baseline(2, "m", &b, one("9")),
            ],
            vec![rate_one(1, T0)],
            vec![cost(3, None, T0, one("0.4")), cost(4, None, &b, one("0.8"))],
            vec![discount(5, "t", &b, 5_000)],
        );
        let u =
            row(r#"{"prompt_tokens":1000,"upstream_provider":"openai","upstream_model":"up-1"}"#);
        let p = price_event(&book, &u, &payer(), received).unwrap();
        assert_eq!(p.price_instant, ts(OCCURRED));
        assert_eq!(
            (p.baseline.unwrap().revision_id, p.charge, p.discount),
            (1, MicroCny(1000), None)
        );
        assert_eq!(
            p.cost.map(|c| c.source),
            Some(CostSource::Upstream { revision_id: 3 })
        );
        // At the boundary instant the new revisions price it.
        let mut at_boundary = u.clone();
        at_boundary.occurred_at = boundary;
        let p = price_event(&book, &at_boundary, &payer(), received).unwrap();
        assert_eq!(
            (p.baseline.unwrap().revision_id, p.charge),
            (2, MicroCny(4500))
        );
        assert_eq!(p.cost.map(|c| c.amount), Some(MicroUsd(800)));
    }

    // ---- cost ----

    #[test]
    fn cost_is_priced_from_the_dispatched_upstream_without_discount() {
        let cost_card = json!({"default": {"base": {"input": "0.4", "output": "1"},
            "long_context": [{"above_input_tokens": 200_000, "rates": {"input": "0.8", "output": "1.5"}}]}});
        let book = book_of(
            vec![baseline(1, "m", T0, test_card())],
            vec![rate_one(1, T0)],
            vec![
                cost(2, None, T0, cost_card),
                cost(
                    3,
                    Some("pk-1"),
                    T0,
                    json!({"default": {"base": {"input": "0.3", "output": "1"}}}),
                ),
            ],
            vec![discount(4, "t", T0, 5_000)],
        );
        let at = |json: &str| price_event(&book, &row(json), &payer(), RECEIVED).unwrap();
        let upstream = r#""upstream_provider":"openai","upstream_model":"up-1""#;

        let p = at(&format!(r#"{{"prompt_tokens":1000,{upstream}}}"#));
        assert_eq!(p.charge, MicroCny(500));
        assert_eq!(
            p.cost,
            Some(CostPriced {
                amount: MicroUsd(400),
                source: CostSource::Upstream { revision_id: 2 },
            })
        );
        // Rounded up, once.
        let cost_of = |json: &str| at(json).cost.map(|c| c.amount);
        assert_eq!(
            cost_of(&format!(r#"{{"prompt_tokens":1001,{upstream}}}"#)),
            Some(MicroUsd(401))
        );
        // At the band the row selects.
        assert_eq!(
            cost_of(&format!(r#"{{"prompt_tokens":200001,{upstream}}}"#)),
            Some(MicroUsd(160_001))
        );
        // The key that served it, when it has its own cost.
        let p = at(&format!(
            r#"{{"prompt_tokens":1000,"provider_key_id":"pk-1",{upstream}}}"#
        ));
        assert_eq!(
            p.cost,
            Some(CostPriced {
                amount: MicroUsd(300),
                source: CostSource::ProviderKey { revision_id: 3 },
            })
        );
        // A tier the cost card lacks leaves the cost unknown, and the charge
        // stands.
        let p = at(&format!(
            r#"{{"prompt_tokens":1000,"service_tier":"flex",{upstream}}}"#
        ));
        assert_eq!((p.charge, p.cost), (MicroCny(250), None));
        // A quantity the cost card has no rate for leaves the cost unknown,
        // never a cost without it; the baseline prices the search.
        let p = at(&format!(
            r#"{{"prompt_tokens":1000,"web_search_requests":1,{upstream}}}"#
        ));
        assert_eq!((p.charge, p.cost), (MicroCny(5_500), None));
        // Without both halves of the upstream's name there is no cost.
        for json in [
            r#"{"prompt_tokens":1000}"#,
            r#"{"prompt_tokens":1000,"upstream_provider":"openai"}"#,
            r#"{"prompt_tokens":1000,"upstream_model":"up-1"}"#,
        ] {
            assert_eq!(at(json).cost, None, "{json}");
        }
    }

    #[test]
    fn the_cost_band_and_tier_are_the_rows_own() {
        let book = book_of(
            vec![baseline(1, "m", T0, test_card())],
            vec![rate_one(1, T0)],
            vec![cost(2, None, T0, test_card())],
            vec![],
        );
        let up = r#""upstream_provider":"openai","upstream_model":"up-1""#;
        let cost = |json: String| {
            price_event(&book, &row(&json), &payer(), RECEIVED)
                .unwrap()
                .cost
                .map(|c| c.amount)
        };
        // Cached input counts toward the cost card's band too:
        // 100000 × 2 + 100001 × 0.2 = 220000.2, rounded up.
        assert_eq!(
            cost(format!(
                r#"{{"prompt_tokens":100000,"cache_read_tokens":100001,{up}}}"#
            )),
            Some(MicroUsd(220_001))
        );
        // A batch row is costed at the cost card's batch tier, whatever it
        // reports.
        assert_eq!(
            cost(format!(
                r#"{{"is_batch":true,"service_tier":"default","prompt_tokens":1000,{up}}}"#
            )),
            Some(MicroUsd(500))
        );
    }

    #[test]
    fn an_unreadable_key_cost_leaves_the_cost_unknown_and_the_charge_priced() {
        // P2-R14 at the engine: the upstream's own cost is never substituted
        // for the key's unreadable one.
        let book = book_of(
            vec![baseline(1, "m", T0, test_card())],
            vec![rate_one(1, T0)],
            vec![
                cost(
                    2,
                    None,
                    T0,
                    json!({"default": {"base": {"input": "0.4", "output": "1"}}}),
                ),
                cost(
                    3,
                    Some("pk-1"),
                    T0,
                    json!({"default": {"base": {"input": "0.3"}}}),
                ),
            ],
            vec![],
        );
        let u = row(
            r#"{"prompt_tokens":1000,"provider_key_id":"pk-1","upstream_provider":"openai","upstream_model":"up-1"}"#,
        );
        let p = price_event(&book, &u, &payer(), RECEIVED).unwrap();
        assert_eq!(
            (p.outcome, p.charge, p.cost),
            (Outcome::Charged, MicroCny(1000), None)
        );
    }

    // ---- what a charge records ----

    #[test]
    fn a_charge_records_what_priced_it() {
        let mut u = row(
            r#"{"prompt_tokens":1000,"cached_prompt_tokens":800,"completion_tokens":200,
            "web_search_requests":1,"requested_service_tier":"auto","service_tier":"default"}"#,
        );
        u.event_id = "evt-1".into();
        let p = price_event(&test_book(), &u, &payer(), RECEIVED).unwrap();
        let line = |dimension, quantity, rate: &str| ChargeLine {
            dimension,
            quantity,
            rate: Rate::parse(rate).unwrap(),
        };
        assert_eq!(
            p,
            PricedEvent {
                event_id: "evt-1".into(),
                tenant_id: "t".into(),
                group_id: Some("g".into()),
                requested_model_id: "m".into(),
                occurred_at: ts(OCCURRED),
                price_instant: ts(OCCURRED),
                outcome: Outcome::Charged,
                charge: MicroCny(10_680),
                baseline: Some(BaselineRef { revision_id: 1 }),
                rate: Some(RateRef {
                    revision_id: 1,
                    cny_per_usd: CnyPerUsd::ONE,
                    model_override: false
                }),
                discount: None,
                service_tier: Some(ServiceTier::default_tier()),
                requested_service_tier: Some("auto".into()),
                long_context_above: None,
                lines: vec![
                    line(Dimension::Input, 200, "1"),
                    line(Dimension::CacheRead, 800, "0.1"),
                    line(Dimension::Output, 200, "2"),
                    line(Dimension::WebSearch, 1, "10000"),
                ],
                cost: None,
                estimated: false,
                warnings: vec![],
                engine_version: ENGINE_VERSION,
            }
        );
    }

    #[test]
    fn a_priced_event_serializes_with_the_stored_names() {
        let book = book_of(
            vec![baseline(1, "m", T0, test_card())],
            vec![rate_one(1, T0)],
            vec![cost(
                2,
                None,
                T0,
                json!({"default": {"base": {"input": "0.4", "output": "1"}}}),
            )],
            vec![discount(3, "t", T0, 2_500)],
        );
        let u = row(
            r#"{"prompt_tokens":10,"requested_service_tier":"flex","upstream_provider":"openai","upstream_model":"up-1"}"#,
        );
        let p = serde_json::to_value(price_event(&book, &u, &payer(), RECEIVED).unwrap()).unwrap();
        assert_eq!(
            p,
            json!({
                "event_id": "e", "tenant_id": "t", "group_id": "g",
                "requested_model_id": "m",
                "occurred_at": "2026-11-06T00:00:00Z", "price_instant": "2026-11-06T00:00:00Z",
                "outcome": "charged", "charge": 8,
                "baseline": {"revision_id": 1},
                "rate": {"revision_id": 1, "cny_per_usd": "1", "model_override": false},
                "discount": {"revision_id": 3, "bps": 2500},
                "service_tier": "default", "requested_service_tier": "flex",
                "long_context_above": null,
                "lines": [{"dimension": "input", "quantity": 10, "rate": "1"}],
                "cost": {"amount": 4, "source": {"kind": "upstream", "revision_id": 2}},
                "estimated": false,
                "warnings": [{"kind": "served_tier_unreported", "requested": "flex"}],
                "engine_version": ENGINE_VERSION,
            })
        );
        let exempt = price(r#"{"cache_status":"hit"}"#).unwrap();
        assert_eq!(
            serde_json::to_value(exempt.outcome).unwrap(),
            json!({"exempt": "response_cache_hit"})
        );
        assert_eq!(
            serde_json::to_value(Outcome::NothingToBill).unwrap(),
            json!("nothing_to_bill")
        );
        let clamped = PricingWarning::OccurredAtClamped {
            occurred_at: ts(OCCURRED),
        };
        assert_eq!(
            serde_json::to_value(clamped).unwrap(),
            json!({"kind": "occurred_at_clamped", "occurred_at": "2026-11-06T00:00:00Z"})
        );
    }

    #[test]
    fn every_outcome_and_error_has_its_stored_code() {
        assert_eq!(Outcome::Charged.status(), "charged");
        assert_eq!(Outcome::NothingToBill.status(), "nothing_to_bill");
        for e in [Exemption::ResponseCacheHit, Exemption::NotModelUsage] {
            assert_eq!(Outcome::Exempt(e).status(), "exempt");
        }
        let at = ts(OCCURRED);
        for (e, code) in [
            (PricingError::MissingEventId, "missing_event_id"),
            (PricingError::UnknownTenant, "unknown_tenant"),
            (PricingError::UnknownModel, "unknown_model"),
            (PricingError::NoGroup, "no_group"),
            (
                PricingError::MissingPrice {
                    model_id: "m".into(),
                    at,
                },
                "missing_price",
            ),
            (
                PricingError::MissingRate {
                    group_id: "g".into(),
                    model_id: "m".into(),
                    at,
                },
                "missing_rate",
            ),
            (
                PricingError::InvalidPrice { revision_id: 1 },
                "invalid_price",
            ),
            (
                PricingError::MissingTierPrice { tier: "x".into() },
                "missing_tier_price",
            ),
            (
                PricingError::MissingDimensionPrice {
                    dimension: Dimension::WebSearch,
                },
                "missing_dimension_price",
            ),
            (
                PricingError::InconsistentUsage(Inconsistency::CacheWriteSplitMismatch),
                "inconsistent_usage",
            ),
            (
                PricingError::UnpricedOperation("speech".into()),
                "unpriced_operation",
            ),
            (
                PricingError::DiscountAboveWhole { bps: 10_001 },
                "discount_above_whole",
            ),
            (PricingError::Overflow, "overflow"),
        ] {
            assert_eq!(e.code(), code);
            assert!(!e.to_string().is_empty(), "{code}");
        }
    }

    #[test]
    fn an_amount_past_i64_is_an_error_never_a_zero_charge() {
        let d = PER_MILLION * WHOLE_BPS;
        let max = u128::try_from(i64::MAX).unwrap();
        assert_eq!(ceil_micro_usd(max * d, d), Ok(MicroUsd(i64::MAX)));
        assert_eq!(ceil_micro_usd(max * d + 1, d), Err(PricingError::Overflow));
    }

    #[test]
    fn pricing_twice_gives_the_same_result() {
        let u = row(r#"{"prompt_tokens":1000,"completion_tokens":3}"#);
        assert_eq!(
            price_event(&test_book(), &u, &payer(), RECEIVED),
            price_event(&test_book(), &u, &payer(), RECEIVED)
        );
    }

    // ---- the CNY charge ----

    fn line(dimension: Dimension, quantity: u64, rate: &str) -> ChargeLine {
        ChargeLine {
            dimension,
            quantity,
            rate: Rate::parse(rate).unwrap(),
        }
    }

    #[test]
    fn the_rate_and_the_discount_are_inside_the_one_rounding() {
        let lines = [
            line(Dimension::Input, 1_000, "4"),
            line(Dimension::Output, 500, "20"),
        ];
        let at = CnyPerUsd::parse("7.2").unwrap();
        assert_eq!(charge_micro_cny(&lines, at, 0), Ok(MicroCny(100_800)));
        assert_eq!(charge_micro_cny(&lines, at, 1_000), Ok(MicroCny(90_720)));
        // 1.08 µ¥, rounded up once. Rounding the USD amount first would give
        // ⌈0.15⌉ = 1 µ$, so ⌈7.2⌉ = 8 µ¥.
        let one = [line(Dimension::Input, 1, "0.15")];
        assert_eq!(charge_micro_cny(&one, at, 0), Ok(MicroCny(2)));
        // 0.972 µ¥ with the discount. Rounding at the rate first would give
        // ⌈1.08⌉ = 2 µ¥, so ⌈1.8⌉ = 2 µ¥.
        assert_eq!(charge_micro_cny(&one, at, 1_000), Ok(MicroCny(1)));
        // No lines, or a line of no quantity, charges nothing.
        assert_eq!(charge_micro_cny(&[], at, 0), Ok(MicroCny(0)));
        assert_eq!(
            charge_micro_cny(&[line(Dimension::Input, 0, "4")], at, 0),
            Ok(MicroCny(0))
        );
        // The smallest charge there is: 1 rate unit at one e4 is 10⁻¹⁴ µ¥,
        // which rounds up to 1 rather than down to nothing.
        assert_eq!(
            charge_micro_cny(&[line(Dimension::Input, 1, "0.000001")], CnyPerUsd(1), 0),
            Ok(MicroCny(1))
        );
        assert_eq!(charge_micro_cny(&lines, CnyPerUsd(0), 0), Ok(MicroCny(0)));
        assert_eq!(charge_micro_cny(&lines, at, 10_000), Ok(MicroCny(0)));
        assert_eq!(
            charge_micro_cny(&lines, at, 10_001),
            Err(PricingError::DiscountAboveWhole { bps: 10_001 })
        );
    }

    #[test]
    fn every_quantity_at_its_largest_never_panics() {
        let largest = u64::try_from(MAX_QUANTITY).unwrap();
        let lines: Vec<_> = Dimension::ALL
            .into_iter()
            .map(|dimension| ChargeLine {
                dimension,
                quantity: largest,
                rate: Rate::MAX,
            })
            .collect();
        assert_eq!(
            charge_micro_cny(&lines, CnyPerUsd::MAX, 0),
            Err(PricingError::Overflow)
        );
    }

    /// The function takes any lines, not only those a row produces.
    #[test]
    fn a_charge_is_exact_up_to_i64_max_and_never_wraps_past_it() {
        let input = |quantity, rate| ChargeLine {
            dimension: Dimension::Input,
            quantity,
            rate,
        };
        // USD 1 per million units: 1 µ$, so 1 µ¥ at a rate of one, per unit.
        let usd_one = Rate(1_000_000);
        let edge = u64::try_from(i64::MAX).unwrap();
        assert_eq!(
            charge_micro_cny(&[input(edge, usd_one)], CnyPerUsd::ONE, 0),
            Ok(MicroCny(i64::MAX))
        );
        assert_eq!(
            charge_micro_cny(&[input(edge + 1, usd_one)], CnyPerUsd::ONE, 0),
            Err(PricingError::Overflow)
        );
        // Past every bound, so past u128 too: still an error.
        assert_eq!(
            charge_micro_cny(
                &[input(u64::MAX, Rate(u64::MAX)); 11],
                CnyPerUsd(u32::MAX),
                0
            ),
            Err(PricingError::Overflow)
        );
    }

    // ---- properties ----

    /// A counter up to 5,000,000: small ones meet at the boundaries.
    fn count() -> impl Strategy<Value = u32> {
        prop_oneof![0u32..=64, 0u32..=5_000_000]
    }

    /// `raw` folded into `0..=max`.
    fn within(raw: u32, max: u32) -> u32 {
        raw % (max + 1)
    }

    prop_compose! {
        /// A row `quantities` accepts, on a tier the synthetic card carries.
        /// The upstream total is absent, `prompt + completion`, or
        /// `prompt + completion + reasoning`, so one more token keeps the
        /// row's reading of its reasoning (see `one_more`).
        fn consistent_usage()(
            (prompt, cached, write) in (count(), count(), proptest::option::of(count())),
            (cache_read, creation, split) in (count(), count(), proptest::option::of(count())),
            (completion, reasoning, total_kind) in (count(), count(), 0u8..3),
            modality in proptest::array::uniform4(proptest::option::of(count())),
            web_search in count(),
            tier in 0u8..4,
        ) -> BillableUsage {
            let cached = within(cached, prompt);
            let write = write.map(|w| within(w, prompt - cached));
            let split = split.map(|s| within(s, creation));
            let reasoning = if total_kind == 2 { reasoning } else { within(reasoning, completion) };
            let total = match total_kind {
                0 => 0,
                1 => prompt + completion,
                _ => prompt + completion + reasoning,
            };
            let (service_tier, is_batch) = match tier {
                0 => (None, false),
                1 => (Some("flex"), false),
                2 => (Some("STANDARD"), false),
                _ => (Some("priority"), true),
            };
            BillableUsage {
                event_id: "e".into(),
                occurred_at: ts(OCCURRED),
                team_id: "t".into(),
                requested_model_id: "m".into(),
                status_code: 200,
                prompt_tokens: prompt,
                completion_tokens: completion,
                cached_prompt_tokens: cached,
                cache_write_tokens: write,
                reasoning_tokens: reasoning,
                total_tokens: total,
                cache_creation_tokens: creation,
                cache_creation_5m_tokens: split,
                cache_creation_1h_tokens: split.map(|five_m| creation - five_m),
                cache_read_tokens: cache_read,
                prompt_audio_tokens: modality[0],
                prompt_image_tokens: modality[1],
                completion_audio_tokens: modality[2],
                completion_image_tokens: modality[3],
                web_search_requests: web_search,
                service_tier: service_tier.map(str::to_owned),
                is_batch,
                ..BillableUsage::default()
            }
        }
    }

    /// A rate of at least `min`, in micro-USD per million units, up to the cap.
    fn rate(min: u64) -> impl Strategy<Value = u64> {
        prop_oneof![min..=64, min..=100_000_000, min..=Rate::MAX.0]
    }

    /// A quantity up to `MAX_QUANTITY`, the most one dimension of a row
    /// bills: small ones meet the rounding, large ones the bound.
    fn quantity() -> impl Strategy<Value = u64> {
        let largest = u64::try_from(MAX_QUANTITY).unwrap();
        prop_oneof![0u64..=64, 0u64..=5_000_000, 0..=largest]
    }

    /// Lines as `price_event` writes them: at most one per dimension, in
    /// `Dimension` order, each quantity up to `MAX_QUANTITY` and each rate
    /// up to `Rate::MAX`. Half of them carry every dimension in the top
    /// eighth of its quantity and rate: only there can a charge pass
    /// `i64::MAX`, and ordinary lines never reach it.
    fn any_lines() -> impl Strategy<Value = Vec<ChargeLine>> {
        let largest = u64::try_from(MAX_QUANTITY).unwrap();
        let ordinary = proptest::array::uniform11(proptest::option::of((quantity(), rate(0))));
        let top = proptest::array::uniform11(
            (
                largest - largest / 8..=largest,
                Rate::MAX.0 - Rate::MAX.0 / 8..=Rate::MAX.0,
            )
                .prop_map(Some),
        );
        prop_oneof![ordinary, top].prop_map(|picked| {
            Dimension::ALL
                .into_iter()
                .zip(picked)
                .filter_map(|(dimension, line)| {
                    line.map(|(quantity, rate)| ChargeLine {
                        dimension,
                        quantity,
                        rate: Rate(rate),
                    })
                })
                .collect()
        })
    }

    /// `Σ quantity × rate`, in units of 10⁻¹² USD.
    fn exact_usd(lines: &[ChargeLine]) -> u128 {
        lines
            .iter()
            .map(|l| u128::from(l.quantity) * u128::from(l.rate.0))
            .sum()
    }

    proptest! {
        /// At a rate of one and no discount, the CNY charge is the P2a USD
        /// charge, `⌈Σ quantity × rate / 10⁶⌉`, in the same number.
        #[test]
        fn at_rate_one_the_cny_charge_is_the_usd_charge(lines in any_lines()) {
            let usd = i64::try_from(exact_usd(&lines).div_ceil(1_000_000)).unwrap();
            prop_assert_eq!(charge_micro_cny(&lines, CnyPerUsd::ONE, 0), Ok(MicroCny(usd)));
        }

        /// The charge is the least whole µ¥ at or above the exact amount, and
        /// `Overflow` exactly when that does not fit in `i64`.
        #[test]
        fn the_charge_is_the_exact_amount_rounded_up_once(
            lines in any_lines(),
            e4 in 0u32..=1_000_000,
            bps in 0u16..=10_000,
        ) {
            let n = exact_usd(&lines) * u128::from(e4) * u128::from(10_000 - bps);
            let per = 10u128.pow(14);
            match charge_micro_cny(&lines, CnyPerUsd(e4), bps) {
                Ok(MicroCny(c)) => {
                    let c = u128::try_from(c).unwrap();
                    prop_assert!(c * per >= n, "{} µ¥ is below {}e-14", c, n);
                    prop_assert!(c == 0 || (c - 1) * per < n, "{} µ¥ is above ⌈{}e-14⌉", c, n);
                }
                Err(e) => {
                    prop_assert_eq!(e, PricingError::Overflow);
                    prop_assert!(n.div_ceil(per) > u128::try_from(i64::MAX).unwrap(), "{}", n);
                }
            }
        }
    }

    /// One rate set: `values` in `Dimension::ALL` order, each optional rate
    /// present where `present` says. Web search is always present.
    fn rates_json(values: [u64; 11], present: [bool; 11]) -> Value {
        let mut rates = serde_json::Map::new();
        for (i, d) in Dimension::ALL.into_iter().enumerate() {
            let required = matches!(
                d,
                Dimension::Input | Dimension::Output | Dimension::WebSearch
            );
            if required || present[i] {
                rates.insert(d.as_str().into(), Rate(values[i]).to_string().into());
            }
        }
        Value::Object(rates)
    }

    prop_compose! {
        /// A synthetic card that passes `validate()`: `default`, `flex` and
        /// `batch` tiers carrying the same optional rates, each with no band
        /// or one band at the same threshold. Every band rate is at least its
        /// base's, as on every published card; a band priced below its base
        /// can lower a charge as a prompt grows past the threshold. Every
        /// rate is at least `min`.
        fn synthetic_card(min: u64)(
            present in proptest::array::uniform11(any::<bool>()),
            above in proptest::option::of(1u64..=10_000_000),
            tiers in proptest::array::uniform3((
                proptest::array::uniform11(rate(min)),
                proptest::array::uniform11(0..=Rate::MAX.0),
            )),
        ) -> Value {
            let mut card = serde_json::Map::new();
            for (name, (base, extra)) in ["default", "flex", "batch"].into_iter().zip(tiers) {
                let band = std::array::from_fn(|i| base[i].saturating_add(extra[i]).min(Rate::MAX.0));
                let mut tier = json!({"base": rates_json(base, present)});
                if let Some(above) = above {
                    tier["long_context"] =
                        json!([{"above_input_tokens": above, "rates": rates_json(band, present)}]);
                }
                card.insert(name.into(), tier);
            }
            Value::Object(card)
        }
    }

    /// Model `m` at `card`, and tenant `t`'s discount of `bps`.
    fn book_with(card: &Value, bps: u16) -> PriceBook {
        let book = book_of(
            vec![baseline(1, "m", T0, card.clone())],
            vec![rate_one(1, T0)],
            vec![],
            vec![discount(2, "t", T0, i64::from(bps))],
        );
        assert!(
            book.invalid_revisions().is_empty(),
            "{card}: {:?}",
            book.invalid_revisions()
        );
        book
    }

    /// Every quantity of `q`, summed.
    fn units(q: &Quantities) -> u64 {
        Dimension::ALL.into_iter().map(|d| q.get(d)).sum()
    }

    /// `u` with one more token (or search) of `d`, counted the way the
    /// upstream counts it: an input-side token is a prompt token too, an
    /// output-side one a completion token (unless it is reasoning the row
    /// reads beside the completion), and a reported total counts either.
    fn one_more(u: &BillableUsage, d: Dimension) -> BillableUsage {
        let beside = reasoning_beside_completion(
            u.prompt_tokens,
            u.completion_tokens,
            u.reasoning_tokens,
            u.total_tokens,
        );
        let inc = |n: Option<u32>| Some(n.unwrap_or(0) + 1);
        let mut m = u.clone();
        match d {
            Dimension::Input => m.prompt_tokens += 1,
            Dimension::CacheRead => m.cache_read_tokens += 1,
            Dimension::CacheWrite => {
                m.cache_creation_tokens += 1;
                m.cache_creation_5m_tokens = m.cache_creation_5m_tokens.map(|n| n + 1);
            }
            Dimension::CacheWrite1h => {
                // Without a split, every write so far was a five-minute one.
                m.cache_creation_5m_tokens = Some(
                    u.cache_creation_5m_tokens
                        .unwrap_or(u.cache_creation_tokens),
                );
                m.cache_creation_1h_tokens = inc(u.cache_creation_1h_tokens);
                m.cache_creation_tokens += 1;
            }
            Dimension::Output => m.completion_tokens += 1,
            Dimension::Reasoning => {
                m.reasoning_tokens += 1;
                if !beside {
                    m.completion_tokens += 1;
                }
            }
            Dimension::AudioInput => {
                m.prompt_tokens += 1;
                m.prompt_audio_tokens = inc(u.prompt_audio_tokens);
            }
            Dimension::ImageInput => {
                m.prompt_tokens += 1;
                m.prompt_image_tokens = inc(u.prompt_image_tokens);
            }
            Dimension::AudioOutput => {
                m.completion_tokens += 1;
                m.completion_audio_tokens = inc(u.completion_audio_tokens);
            }
            Dimension::ImageOutput => {
                m.completion_tokens += 1;
                m.completion_image_tokens = inc(u.completion_image_tokens);
            }
            Dimension::WebSearch => m.web_search_requests += 1,
        }
        let counted_in_total = !matches!(
            d,
            Dimension::CacheRead
                | Dimension::CacheWrite
                | Dimension::CacheWrite1h
                | Dimension::WebSearch
        );
        if u.total_tokens != 0 && counted_in_total {
            m.total_tokens += 1;
        }
        m
    }

    /// One unit of one dimension on an otherwise empty row: the rows the one
    /// rounding decides.
    fn one_unit() -> impl Strategy<Value = BillableUsage> {
        (0..Dimension::ALL.len()).prop_map(|i| {
            let zero = BillableUsage {
                event_id: "e".into(),
                occurred_at: ts(OCCURRED),
                team_id: "t".into(),
                requested_model_id: "m".into(),
                status_code: 200,
                ..BillableUsage::default()
            };
            one_more(&zero, Dimension::ALL[i])
        })
    }

    proptest! {
        #[test]
        fn adding_a_token_never_lowers_the_charge(
            u in consistent_usage(),
            card in synthetic_card(0),
            bps in 0u16..=10_000,
        ) {
            // The band starts at this row's prompt, so one more input-side
            // token crosses it.
            let q = quantities(&u).unwrap();
            let mut card = card;
            for tier in card.as_object_mut().unwrap().values_mut() {
                if let Some(bands) = tier.get_mut("long_context") {
                    bands[0]["above_input_tokens"] = q.input_total().max(1).into();
                }
            }
            let book = book_with(&card, bps);
            let before = price_event(&book, &u, &payer(), RECEIVED).unwrap();
            let units_before = units(&q);
            for d in Dimension::ALL {
                let more = one_more(&u, d);
                // The row really carries one more token, and nothing else.
                prop_assert_eq!(units(&quantities(&more).unwrap()), units_before + 1, "{}", d);
                let after = price_event(&book, &more, &payer(), RECEIVED).unwrap();
                prop_assert!(after.charge >= before.charge, "{}: {:?} < {:?}", d, after.charge, before.charge);
            }
        }

        #[test]
        fn a_bigger_discount_never_raises_the_charge(
            u in consistent_usage(),
            card in synthetic_card(0),
            (a, b) in (0u16..=10_000, 0u16..=10_000),
        ) {
            let at = |bps| price_event(&book_with(&card, bps), &u, &payer(), RECEIVED).unwrap().charge;
            prop_assert!(at(a.min(b)) >= at(a.max(b)));
        }

        /// A 100% discount (10000 bps) is valid and prices any row at 0
        /// (`a_full_discount_prices_a_row_at_zero`), so it is left out here.
        /// Half the rows are one unit of one dimension, whose exact amount
        /// is a fraction of a µ$ that only rounding up keeps billable.
        #[test]
        fn a_billable_row_is_never_free(
            // Boxed: the union's value trees overflow a debug test thread's
            // stack otherwise.
            u in prop_oneof![consistent_usage().boxed(), one_unit().boxed()],
            card in synthetic_card(1),
            bps in 0u16..10_000,
        ) {
            let p = price_event(&book_with(&card, bps), &u, &payer(), RECEIVED).unwrap();
            if quantities(&u).unwrap().is_zero() {
                prop_assert_eq!(p.outcome, Outcome::NothingToBill);
            } else {
                prop_assert_eq!(p.outcome, Outcome::Charged);
                prop_assert!(p.charge >= MicroCny(1), "{:?}", p);
            }
        }

        /// It cannot tell a wrong rate on a line, so it is not the coverage
        /// of the arithmetic; the hand-computed rows above are. It does pin
        /// each line's quantity to what the row bills.
        #[test]
        fn the_charge_is_the_ceiled_sum_of_its_lines(
            u in consistent_usage(),
            card in synthetic_card(0),
            bps in 0u16..=10_000,
        ) {
            let p = price_event(&book_with(&card, bps), &u, &payer(), RECEIVED).unwrap();
            let q = quantities(&u).unwrap();
            let billed: Vec<_> = Dimension::ALL.into_iter().filter(|d| q.get(*d) > 0).collect();
            let lines: Vec<_> = p.lines.iter().map(|l| l.dimension).collect();
            prop_assert_eq!(lines, billed);
            for l in &p.lines {
                prop_assert_eq!(l.quantity, q.get(l.dimension));
            }
            prop_assert_eq!(p.discount, Some(DiscountRef { revision_id: 2, bps }));
            let sum: u128 = p.lines.iter().map(|l| u128::from(l.quantity) * u128::from(l.rate.0)).sum();
            let want = (sum * (10_000 - u128::from(bps))).div_ceil(1_000_000 * 10_000);
            prop_assert_eq!(u128::try_from(p.charge.0).unwrap(), want);
        }
    }

    #[test]
    fn the_synthetic_cards_are_valid_rate_cards() {
        // `book_with` asserts the book took every card; this reads one
        // directly, so a card the generator builds wrong fails here first.
        let card: RateCard = serde_json::from_value(json!({
            "default": {"base": rates_json([1; 11], [true; 11]),
                        "long_context": [{"above_input_tokens": 5, "rates": rates_json([2; 11], [true; 11])}]}
        }))
        .unwrap();
        assert_eq!(card.validate(), Ok(()));
    }
}
