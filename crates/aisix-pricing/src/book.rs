//! The price book: every price revision, indexed for lookup at an instant.
//!
//! Four revision tables feed it: baselines (each model's official list
//! price), group rates (a group's CNY-per-USD rate, default and per-model
//! overrides), cost rates (an upstream account's CNY-per-USD rate, default
//! and per-model overrides, read the way group rates are), and tenant
//! discounts. A revision applies from its `effective_from` until a later
//! revision of the same key starts; a price never changes in place.
//!
//! Each revision is read on its own. One this engine cannot use is listed in
//! [`PriceBook::invalid_revisions`] and keeps its slot in time, so a lookup
//! that lands on it reports it instead of reaching an older price, and no
//! other key is affected.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::rates::{CnyPerUsd, RateCard, RateCardError};
use crate::ENGINE_VERSION;

/// The reason given for a revision stamped with a later `ENGINE_VERSION`.
const NEWER_ENGINE: &str = "written by a newer engine";

/// The reason given for a group's default-rate row without a rate.
const NO_DEFAULT_RATE: &str = "a group's default rate row has no rate";

/// The reason given for an account's default cost-rate row without a rate.
const NO_DEFAULT_COST_RATE: &str = "an account's default cost rate row has no rate";

/// Rows exactly as the four revision tables hold them. Cards arrive as raw
/// JSON so that one bad or newer card is isolated to its own revision
/// instead of failing the whole book.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceRows {
    pub baselines: Vec<BaselineRow>,
    pub rates: Vec<GroupRateRow>,
    pub cost_rates: Vec<CostRateRow>,
    pub discounts: Vec<DiscountRow>,
}

/// One baseline revision: a model's official USD list price, as a full rate
/// card.
#[derive(Debug, Clone, Deserialize)]
pub struct BaselineRow {
    pub revision_id: i64,
    pub engine_version: u32,
    /// The model the caller addresses.
    pub model_id: String,
    pub effective_from: DateTime<Utc>,
    /// A [`RateCard`].
    pub card: serde_json::Value,
}

/// One group-rate revision: what a group charges, in CNY per USD of the
/// baseline, as ten-thousandths. With a `model_id` it overrides that one
/// model; without one it is the group's default.
#[derive(Debug, Clone, Deserialize)]
pub struct GroupRateRow {
    pub revision_id: i64,
    pub engine_version: u32,
    pub group_id: String,
    /// `None`: the group's default rate. Set: that model's override, whose
    /// `None` rate ends it.
    pub model_id: Option<String>,
    pub effective_from: DateTime<Utc>,
    /// Ten-thousandths of a CNY per USD, `0..=1_000_000`. `None` only on a
    /// model row, where it ends the override.
    pub cny_per_usd_e4: Option<i64>,
}

/// One cost-rate revision: what an upstream account (a provider key) costs
/// the platform, in CNY per USD of the baseline, as ten-thousandths. With a
/// `model_id` it overrides that one model; without one it is the account's
/// default. The same shape as a [`GroupRateRow`], keyed on the account.
#[derive(Debug, Clone, Deserialize)]
pub struct CostRateRow {
    pub revision_id: i64,
    pub engine_version: u32,
    pub provider_key_id: String,
    /// `None`: the account's default rate. Set: that model's override, whose
    /// `None` rate ends it.
    pub model_id: Option<String>,
    pub effective_from: DateTime<Utc>,
    /// Ten-thousandths of a CNY per USD, `0..=1_000_000`. `None` only on a
    /// model row, where it ends the override.
    pub cny_per_usd_e4: Option<i64>,
}

/// One discount revision: a tenant's discount off its group's price, in
/// basis points from 0 to 10000.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscountRow {
    pub revision_id: i64,
    pub engine_version: u32,
    pub tenant_id: String,
    pub effective_from: DateTime<Utc>,
    pub discount_bps: i64,
}

/// A revision this engine cannot use, and why. The rows it would price are
/// unpriced while it is in effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvalidRevision {
    pub table: RevisionTable,
    pub revision_id: i64,
    pub reason: String,
}

/// The revision table an [`InvalidRevision`] is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionTable {
    Baseline,
    Rate,
    /// The cost rates.
    Cost,
    Discount,
}

/// The baseline in effect for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Baseline<'a> {
    pub(crate) revision_id: i64,
    pub(crate) card: &'a RateCard,
}

/// The CNY-per-USD rate in effect for one model: a group's, or an
/// account's cost rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RateInEffect {
    pub(crate) revision_id: i64,
    pub(crate) cny: CnyPerUsd,
    /// The model's own override priced it, not the default.
    pub(crate) model_override: bool,
}

/// Marks a revision this engine cannot use. Its reason is in
/// [`PriceBook::invalid_revisions`].
#[derive(Debug)]
struct Invalid;

/// One revision in its slot in time.
#[derive(Debug)]
struct Revision<T> {
    effective_from: DateTime<Utc>,
    revision_id: i64,
    value: Result<T, Invalid>,
}

impl<T> Revision<T> {
    /// Its value, or `Err(revision_id)` when it is invalid.
    fn read(&self) -> Result<&T, i64> {
        self.value.as_ref().map_err(|Invalid| self.revision_id)
    }
}

/// The revision in effect at `at`: the greatest `effective_from <= at`, ties
/// going to the greatest `revision_id`. `revisions` is sorted by both.
fn in_effect<T>(revisions: &[Revision<T>], at: DateTime<Utc>) -> Option<&Revision<T>> {
    let started = revisions.partition_point(|r| r.effective_from <= at);
    revisions.get(started.checked_sub(1)?)
}

/// One group's rates, or one account's cost rates.
#[derive(Debug, Default)]
struct RateRevisions {
    default: Vec<Revision<CnyPerUsd>>,
    /// By model id. A `None` rate ended the override.
    overrides: BTreeMap<String, Vec<Revision<Option<CnyPerUsd>>>>,
}

impl RateRevisions {
    /// The rate for `model_id` at `at`. The model's override in effect wins.
    /// One in effect that ended gives way to the default in effect. An
    /// invalid revision in effect, override or default, is `Err(revision_id)`
    /// and never gives way to another rate. `None` when no rate is in effect.
    fn at(&self, model_id: &str, at: DateTime<Utc>) -> Option<Result<RateInEffect, i64>> {
        let model = self.overrides.get(model_id);
        if let Some(r) = model.and_then(|revisions| in_effect(revisions, at)) {
            match r.read() {
                Ok(Some(cny)) => {
                    return Some(Ok(RateInEffect {
                        revision_id: r.revision_id,
                        cny: *cny,
                        model_override: true,
                    }))
                }
                // The override ended.
                Ok(None) => {}
                Err(revision_id) => return Some(Err(revision_id)),
            }
        }
        let r = in_effect(&self.default, at)?;
        Some(r.read().map(|cny| RateInEffect {
            revision_id: r.revision_id,
            cny: *cny,
            model_override: false,
        }))
    }
}

/// One rate revision as both rate tables hold it: its owner (a group, or an
/// account), and the rest of the row.
struct RateRow {
    owner: String,
    revision_id: i64,
    engine_version: u32,
    model_id: Option<String>,
    effective_from: DateTime<Utc>,
    cny_per_usd_e4: Option<i64>,
}

/// Every rate revision of one table, by owner, each in its slot. A default
/// row (no model) without a rate is invalid with `no_default`. `rows` are in
/// time order.
fn index_rates(
    invalid: &mut Vec<InvalidRevision>,
    table: RevisionTable,
    no_default: &str,
    rows: impl IntoIterator<Item = RateRow>,
) -> BTreeMap<String, RateRevisions> {
    let mut by_owner: BTreeMap<String, RateRevisions> = BTreeMap::new();
    for row in rows {
        let cny = this_engine(row.engine_version).and_then(|()| read_rate(row.cny_per_usd_e4));
        let owner = by_owner.entry(row.owner).or_default();
        let (id, from) = (row.revision_id, row.effective_from);
        match row.model_id {
            Some(model) => {
                let revision = admit(invalid, table, id, from, cny);
                owner.overrides.entry(model).or_default().push(revision);
            }
            None => {
                let cny = cny.and_then(|cny| cny.ok_or_else(|| no_default.to_owned()));
                owner.default.push(admit(invalid, table, id, from, cny));
            }
        }
    }
    by_owner
}

/// Every price revision, indexed by its key, each key's revisions sorted by
/// `(effective_from, revision_id)`.
#[derive(Debug)]
pub struct PriceBook {
    /// By model id.
    baselines: BTreeMap<String, Vec<Revision<RateCard>>>,
    /// By group id.
    rates: BTreeMap<String, RateRevisions>,
    /// By provider key id.
    cost_rates: BTreeMap<String, RateRevisions>,
    /// By tenant id.
    discounts: BTreeMap<String, Vec<Revision<u16>>>,
    invalid: Vec<InvalidRevision>,
}

impl PriceBook {
    /// Indexes every revision. It never fails as a whole: a revision this
    /// engine cannot use is listed in [`Self::invalid_revisions`] and
    /// affects only its own key. A revision is unusable when it was written
    /// by a newer engine, its card does not decode or breaks the rate
    /// model's rules, its rate is outside `0..=1_000_000`, a group's or an
    /// account's default rate row carries no rate, or it is a discount
    /// outside 0..=10000 basis points.
    pub fn from_rows(mut rows: PriceRows) -> PriceBook {
        // Taking each table in time order leaves every key's revisions
        // sorted, which `in_effect` relies on.
        rows.baselines
            .sort_by_key(|r| (r.effective_from, r.revision_id));
        rows.rates
            .sort_by_key(|r| (r.effective_from, r.revision_id));
        rows.cost_rates
            .sort_by_key(|r| (r.effective_from, r.revision_id));
        rows.discounts
            .sort_by_key(|r| (r.effective_from, r.revision_id));
        let mut invalid = Vec::new();

        let mut baselines: BTreeMap<String, Vec<Revision<RateCard>>> = BTreeMap::new();
        for row in rows.baselines {
            let card = this_engine(row.engine_version).and_then(|()| read_card(row.card));
            let revision = admit(
                &mut invalid,
                RevisionTable::Baseline,
                row.revision_id,
                row.effective_from,
                card,
            );
            baselines.entry(row.model_id).or_default().push(revision);
        }

        let rates = index_rates(
            &mut invalid,
            RevisionTable::Rate,
            NO_DEFAULT_RATE,
            rows.rates.into_iter().map(|r| RateRow {
                owner: r.group_id,
                revision_id: r.revision_id,
                engine_version: r.engine_version,
                model_id: r.model_id,
                effective_from: r.effective_from,
                cny_per_usd_e4: r.cny_per_usd_e4,
            }),
        );

        let cost_rates = index_rates(
            &mut invalid,
            RevisionTable::Cost,
            NO_DEFAULT_COST_RATE,
            rows.cost_rates.into_iter().map(|r| RateRow {
                owner: r.provider_key_id,
                revision_id: r.revision_id,
                engine_version: r.engine_version,
                model_id: r.model_id,
                effective_from: r.effective_from,
                cny_per_usd_e4: r.cny_per_usd_e4,
            }),
        );

        let mut discounts: BTreeMap<String, Vec<Revision<u16>>> = BTreeMap::new();
        for row in rows.discounts {
            let bps = this_engine(row.engine_version).and_then(|()| {
                u16::try_from(row.discount_bps)
                    .ok()
                    .filter(|bps| *bps <= 10_000)
                    .ok_or_else(|| {
                        format!("discount_bps {} is outside 0..=10000", row.discount_bps)
                    })
            });
            discounts.entry(row.tenant_id).or_default().push(admit(
                &mut invalid,
                RevisionTable::Discount,
                row.revision_id,
                row.effective_from,
                bps,
            ));
        }

        PriceBook {
            baselines,
            rates,
            cost_rates,
            discounts,
            invalid,
        }
    }

    /// Every revision this engine cannot use, for alerts: baselines, then
    /// rates, then cost rates, then discounts, each in time order.
    pub fn invalid_revisions(&self) -> &[InvalidRevision] {
        &self.invalid
    }

    /// The baseline of `model_id` at `at`. It keys on the entry the caller
    /// addressed, never on the upstream that served it.
    ///
    /// An invalid revision in effect is `Err(revision_id)` and never gives
    /// way to an older baseline. `None` when no baseline is in effect.
    pub(crate) fn baseline(
        &self,
        model_id: &str,
        at: DateTime<Utc>,
    ) -> Option<Result<Baseline<'_>, i64>> {
        let r = in_effect(self.baselines.get(model_id)?, at)?;
        Some(r.read().map(|card| Baseline {
            revision_id: r.revision_id,
            card,
        }))
    }

    /// `group_id`'s rate for `model_id` at `at`. It keys on the group the
    /// payer's key names and on the entry the caller addressed; the
    /// dispatched target plays no part.
    ///
    /// The model's override in effect wins. One in effect that ended gives
    /// way to the group's default in effect. An invalid revision in effect,
    /// override or default, is `Err(revision_id)` and never gives way to
    /// another rate. `None` when no rate is in effect.
    pub(crate) fn rate(
        &self,
        group_id: &str,
        model_id: &str,
        at: DateTime<Utc>,
    ) -> Option<Result<RateInEffect, i64>> {
        self.rates.get(group_id)?.at(model_id, at)
    }

    /// `provider_key_id`'s cost rate for `model_id` at `at`: what the
    /// upstream account that served a row costs the platform per USD of the
    /// model's baseline. It keys on the account the row names and on the
    /// entry the caller addressed, the same model the baseline and the
    /// group rate key on, so a cost and a charge always read one card.
    ///
    /// Read exactly as [`Self::rate`] reads a group's: the model's override
    /// in effect wins, an ended one gives way to the account's default, and
    /// an invalid revision in effect is `Err(revision_id)`, never another
    /// rate. `None` when no cost rate is in effect.
    pub(crate) fn cost_rate(
        &self,
        provider_key_id: &str,
        model_id: &str,
        at: DateTime<Utc>,
    ) -> Option<Result<RateInEffect, i64>> {
        self.cost_rates.get(provider_key_id)?.at(model_id, at)
    }

    /// `tenant_id`'s discount in effect at `at`, as `(revision_id, bps)`;
    /// `Err(revision_id)` when that revision is invalid.
    pub(crate) fn discount(
        &self,
        tenant_id: &str,
        at: DateTime<Utc>,
    ) -> Option<Result<(i64, u16), i64>> {
        let r = in_effect(self.discounts.get(tenant_id)?, at)?;
        Some(r.read().map(|bps| (r.revision_id, *bps)))
    }
}

/// `Err` with the reason when a newer engine wrote the revision.
fn this_engine(engine_version: u32) -> Result<(), String> {
    if engine_version > ENGINE_VERSION {
        return Err(NEWER_ENGINE.to_owned());
    }
    Ok(())
}

/// Decodes and validates a stored card; `Err` is why it cannot be used.
fn read_card(card: serde_json::Value) -> Result<RateCard, String> {
    let card: RateCard = serde_json::from_value(card)
        .map_err(|e| RateCardError::Malformed(e.to_string()).to_string())?;
    card.validate().map_err(|e| e.to_string())?;
    Ok(card)
}

/// A stored rate as the engine reads it: `0..=1_000_000` ten-thousandths.
/// `Ok(None)` on a model row that ends its override, `Err` why the value
/// cannot be used.
fn read_rate(cny_per_usd_e4: Option<i64>) -> Result<Option<CnyPerUsd>, String> {
    let Some(e4) = cny_per_usd_e4 else {
        return Ok(None);
    };
    u32::try_from(e4)
        .ok()
        .filter(|e4| *e4 <= CnyPerUsd::MAX.0)
        .map(|e4| Some(CnyPerUsd(e4)))
        .ok_or_else(|| format!("cny_per_usd_e4 {e4} is outside 0..=1000000"))
}

/// Places a checked revision in its slot, listing it as invalid when the
/// check gave a reason.
fn admit<T>(
    invalid: &mut Vec<InvalidRevision>,
    table: RevisionTable,
    revision_id: i64,
    effective_from: DateTime<Utc>,
    checked: Result<T, String>,
) -> Revision<T> {
    let value = checked.map_err(|reason| {
        invalid.push(InvalidRevision {
            table,
            revision_id,
            reason,
        });
        Invalid
    });
    Revision {
        effective_from,
        revision_id,
        value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rates::{Rate, ServiceTier};
    use serde_json::{json, Value};

    const T0: &str = "2026-01-01T00:00:00Z";
    const T1: &str = "2026-01-15T00:00:00Z";
    const T2: &str = "2026-02-01T00:00:00Z";
    const T3: &str = "2026-02-15T00:00:00Z";

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A valid card whose input and output are both `n` USD per million.
    fn card(n: u32) -> Value {
        json!({"default": {"base": {"input": n.to_string(), "output": n.to_string()}}})
    }

    /// A card this engine cannot decode: `output` is required.
    fn undecodable() -> Value {
        json!({"default": {"base": {"input": "1"}}})
    }

    fn baseline(revision_id: i64, model_id: &str, from: &str, card: Value) -> BaselineRow {
        BaselineRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            model_id: model_id.to_owned(),
            effective_from: ts(from),
            card,
        }
    }

    /// A group's default rate: `e4` ten-thousandths of a CNY per USD.
    fn rate(revision_id: i64, group_id: &str, from: &str, e4: i64) -> GroupRateRow {
        GroupRateRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            group_id: group_id.to_owned(),
            model_id: None,
            effective_from: ts(from),
            cny_per_usd_e4: Some(e4),
        }
    }

    /// A model's override in `group_id`; `None` ends that model's override.
    fn override_(
        revision_id: i64,
        group_id: &str,
        model_id: &str,
        from: &str,
        e4: Option<i64>,
    ) -> GroupRateRow {
        GroupRateRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            group_id: group_id.to_owned(),
            model_id: Some(model_id.to_owned()),
            effective_from: ts(from),
            cny_per_usd_e4: e4,
        }
    }

    /// An account's cost rate: its default with no `model_id`, else that
    /// model's override, which a `None` rate ends.
    fn cost_rate(
        revision_id: i64,
        provider_key_id: &str,
        model_id: Option<&str>,
        from: &str,
        e4: Option<i64>,
    ) -> CostRateRow {
        CostRateRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            provider_key_id: provider_key_id.to_owned(),
            model_id: model_id.map(str::to_owned),
            effective_from: ts(from),
            cny_per_usd_e4: e4,
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
        cost_rates: Vec<CostRateRow>,
        discounts: Vec<DiscountRow>,
    ) -> PriceBook {
        PriceBook::from_rows(PriceRows {
            baselines,
            rates,
            cost_rates,
            discounts,
        })
    }

    fn book(baselines: &[BaselineRow]) -> PriceBook {
        book_of(baselines.to_vec(), Vec::new(), Vec::new(), Vec::new())
    }

    /// The default base `input` rate, which tells the test cards apart.
    fn input(card: &RateCard) -> Rate {
        card.0[&ServiceTier::default_tier()].base.input
    }

    /// The baseline of `m` at `at`, as the engine reads it.
    fn at<'a>(b: &'a PriceBook, at: &str) -> Option<Result<Baseline<'a>, i64>> {
        b.baseline("m", ts(at))
    }

    fn usd(n: u64) -> Rate {
        Rate(n * 1_000_000)
    }

    fn invalid(table: RevisionTable, revision_id: i64, reason: &str) -> InvalidRevision {
        InvalidRevision {
            table,
            revision_id,
            reason: reason.to_owned(),
        }
    }

    // ---- which revision is in effect ----

    #[test]
    fn the_revision_in_effect_at_the_event_prices_it() {
        // Review Focus 2: the boundary instant belongs to the new revision.
        let b = book(&[
            baseline(1, "m", "2026-01-01T00:00:00Z", card(1)),
            baseline(2, "m", "2026-02-01T00:00:00Z", card(9)),
        ]);
        assert_eq!(
            at(&b, "2026-01-31T23:59:59.999Z")
                .unwrap()
                .unwrap()
                .revision_id,
            1
        );
        assert_eq!(
            at(&b, "2026-01-31T23:59:59.999999999Z")
                .unwrap()
                .unwrap()
                .revision_id,
            1
        );
        assert_eq!(
            at(&b, "2026-02-01T00:00:00Z").unwrap().unwrap().revision_id,
            2
        );
        // The price is the revision's own card.
        assert_eq!(
            input(at(&b, "2026-01-31T23:59:59.999Z").unwrap().unwrap().card),
            usd(1)
        );
        assert_eq!(
            input(at(&b, "2026-02-01T00:00:00Z").unwrap().unwrap().card),
            usd(9)
        );
    }

    #[test]
    fn a_correction_with_the_same_start_supersedes() {
        let b = book(&[baseline(1, "m", T0, card(1)), baseline(5, "m", T0, card(2))]);
        assert_eq!(at(&b, T1).unwrap().unwrap().revision_id, 5);
        assert_eq!(at(&b, T0).unwrap().unwrap().revision_id, 5);
        // Whatever order the rows arrive in.
        let b = book(&[baseline(5, "m", T0, card(2)), baseline(1, "m", T0, card(1))]);
        let p = at(&b, T1).unwrap().unwrap();
        assert_eq!((p.revision_id, input(p.card)), (5, usd(2)));
    }

    #[test]
    fn nothing_is_in_effect_before_the_first_revision() {
        assert!(book(&[baseline(1, "m", "2026-01-01T00:00:00Z", card(1))])
            .baseline("m", ts("2025-12-31T23:59:59Z"))
            .is_none());
        assert!(book(&[baseline(1, "m", T0, card(1))])
            .baseline("other-model", ts(T1))
            .is_none());
    }

    #[test]
    fn several_future_revisions_each_take_effect_at_their_own_start() {
        // Ruling P2-R12: an import writes a model's later-dated list prices
        // as further revisions, all of them ahead of time.
        let b = book(&[
            baseline(3, "m", "2026-03-01T00:00:00Z", card(3)),
            baseline(1, "m", T0, card(1)),
            baseline(2, "m", T2, card(2)),
        ]);
        for (instant, want) in [
            (T1, 1),
            (T2, 2),
            (T3, 2),
            ("2026-03-01T00:00:00Z", 3),
            ("2027-01-01T00:00:00Z", 3),
        ] {
            let p = at(&b, instant).unwrap().unwrap();
            assert_eq!(
                (p.revision_id, input(p.card)),
                (want, usd(want as u64)),
                "{instant}"
            );
        }
    }

    // ---- group rates ----

    #[test]
    fn a_models_override_beats_the_group_default() {
        let b = book_of(
            Vec::new(),
            vec![
                rate(1, "g", T0, 50_000),
                override_(2, "g", "m", T0, Some(60_000)),
                rate(3, "h", T0, 70_000),
            ],
            Vec::new(),
            Vec::new(),
        );
        let p = b.rate("g", "m", ts(T1)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (2, CnyPerUsd(60_000), true)
        );
        // The override is for its own group and model only.
        let p = b.rate("h", "m", ts(T1)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (3, CnyPerUsd(70_000), false)
        );
        let p = b.rate("g", "m2", ts(T1)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (1, CnyPerUsd(50_000), false)
        );
    }

    #[test]
    fn an_ended_override_gives_way_to_the_default() {
        let b = book_of(
            Vec::new(),
            vec![
                rate(1, "g", T0, 50_000),
                override_(2, "g", "m", T0, Some(60_000)),
                override_(3, "g", "m", T2, None),
            ],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(b.rate("g", "m", ts(T1)).unwrap().unwrap().revision_id, 2);
        let p = b.rate("g", "m", ts(T3)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (1, CnyPerUsd(50_000), false)
        );
    }

    #[test]
    fn an_override_before_its_start_leaves_the_default() {
        let b = book_of(
            Vec::new(),
            vec![
                rate(1, "g", T0, 50_000),
                override_(2, "g", "m", T2, Some(60_000)),
            ],
            Vec::new(),
            Vec::new(),
        );
        let p = b.rate("g", "m", ts(T1)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (1, CnyPerUsd(50_000), false)
        );
        let p = b.rate("g", "m", ts(T2)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (2, CnyPerUsd(60_000), true)
        );
    }

    #[test]
    fn a_group_nobody_set_a_rate_for_has_none() {
        let b = book_of(
            Vec::new(),
            vec![rate(1, "g", T2, 50_000)],
            Vec::new(),
            Vec::new(),
        );
        assert!(b.rate("g", "m", ts(T1)).is_none());
        assert!(b.rate("unknown", "m", ts(T2)).is_none());
    }

    #[test]
    fn a_group_rate_never_prices_another_group() {
        let b = book_of(
            Vec::new(),
            vec![rate(1, "g", T0, 50_000), rate(2, "G", T0, 60_000)],
            Vec::new(),
            Vec::new(),
        );
        for (group, want) in [("g", 1), ("G", 2)] {
            assert_eq!(
                b.rate(group, "m", ts(T1)).unwrap().unwrap().revision_id,
                want,
                "{group}"
            );
        }
        for other in ["", "g ", "h"] {
            assert!(b.rate(other, "m", ts(T1)).is_none(), "{other:?}");
        }
    }

    #[test]
    fn a_rate_outside_its_range_is_isolated() {
        for bad in [1_000_001, -1, i64::MAX, i64::MIN] {
            let b = book_of(
                Vec::new(),
                vec![rate(3, "g", T0, bad)],
                Vec::new(),
                Vec::new(),
            );
            assert_eq!(b.rate("g", "m", ts(T1)), Some(Err(3)), "{bad}");
            assert_eq!(
                b.invalid_revisions(),
                [invalid(
                    RevisionTable::Rate,
                    3,
                    &format!("cny_per_usd_e4 {bad} is outside 0..=1000000")
                )],
                "{bad}"
            );
        }
        for ok in [0, CnyPerUsd::MAX.0.into()] {
            let b = book_of(
                Vec::new(),
                vec![rate(3, "g", T0, ok)],
                Vec::new(),
                Vec::new(),
            );
            assert_eq!(
                b.rate("g", "m", ts(T1)),
                Some(Ok(RateInEffect {
                    revision_id: 3,
                    cny: CnyPerUsd(ok as u32),
                    model_override: false
                })),
                "{ok}"
            );
            assert!(b.invalid_revisions().is_empty());
        }
    }

    #[test]
    fn a_default_rate_row_without_a_rate_is_invalid() {
        // Only a model row can end; a group's default row needs a rate.
        let b = book_of(
            Vec::new(),
            vec![rate(1, "g", T0, 50_000), overwrite_rate(2, "g", T2, None)],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(b.rate("g", "m", ts(T1)).unwrap().unwrap().revision_id, 1);
        assert_eq!(b.rate("g", "m", ts(T3)), Some(Err(2)));
        assert_eq!(
            b.invalid_revisions(),
            [invalid(RevisionTable::Rate, 2, NO_DEFAULT_RATE)]
        );
    }

    /// A group's default row carrying no rate at all: only raw SQL writes
    /// one, and it makes every row of the group's keys unpriced.
    fn overwrite_rate(
        revision_id: i64,
        group_id: &str,
        from: &str,
        e4: Option<i64>,
    ) -> GroupRateRow {
        GroupRateRow {
            cny_per_usd_e4: e4,
            ..rate(revision_id, group_id, from, 0)
        }
    }

    // ---- isolation ----

    #[test]
    fn a_revision_the_engine_cannot_read_is_isolated_to_its_model() {
        let b = book(&[
            baseline(7, "a", T0, undecodable()),
            baseline(8, "b", T0, card(1)),
        ]);
        assert_eq!(b.baseline("a", ts(T1)), Some(Err(7)));
        assert_eq!(b.baseline("b", ts(T1)).unwrap().unwrap().revision_id, 8);
        let listed = b.invalid_revisions();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].table, listed[0].revision_id),
            (RevisionTable::Baseline, 7)
        );
        assert!(
            listed[0].reason.starts_with("rate card does not decode: "),
            "{}",
            listed[0].reason
        );
    }

    #[test]
    fn a_card_that_breaks_the_rate_rules_is_isolated_with_its_reason() {
        let no_default = json!({"flex": {"base": {"input": "1", "output": "2"}}});
        let b = book(&[
            baseline(7, "a", T0, no_default),
            baseline(8, "b", T0, card(1)),
        ]);
        assert_eq!(b.baseline("a", ts(T1)), Some(Err(7)));
        assert!(b.baseline("b", ts(T1)).unwrap().is_ok());
        assert_eq!(
            b.invalid_revisions(),
            [invalid(
                RevisionTable::Baseline,
                7,
                "rate card has no default tier"
            )]
        );
    }

    #[test]
    fn a_revision_written_by_a_newer_engine_is_isolated() {
        let newer = |mut row: BaselineRow| {
            row.engine_version = ENGINE_VERSION + 1;
            row
        };
        // A newer engine's card, valid here or using a rate this engine
        // does not know, is unreadable for the same reason.
        let unknown_rate =
            json!({"default": {"base": {"input": "1", "output": "2", "image_count": "3"}}});
        let b = book(&[
            newer(baseline(7, "m", T0, card(1))),
            newer(baseline(9, "n", T0, unknown_rate)),
            baseline(8, "o", T0, card(1)),
        ]);
        assert_eq!(b.baseline("m", ts(T1)), Some(Err(7)));
        assert_eq!(b.baseline("n", ts(T1)), Some(Err(9)));
        assert!(b.baseline("o", ts(T1)).unwrap().is_ok());
        assert_eq!(
            b.invalid_revisions(),
            [
                invalid(RevisionTable::Baseline, 7, "written by a newer engine"),
                invalid(RevisionTable::Baseline, 9, "written by a newer engine"),
            ]
        );
        // A newer engine's rate row is unreadable the same way, and a newer
        // engine's ending of an override does not end it.
        let b = book_of(
            Vec::new(),
            vec![
                rate(1, "g", T0, 50_000),
                override_(2, "g", "m", T0, Some(60_000)),
                newer_rate(override_(3, "g", "m", T2, None)),
            ],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(b.rate("g", "m", ts(T3)), Some(Err(3)));
        // An older engine's revision is read as usual.
        let mut older = baseline(1, "m", T0, card(1));
        older.engine_version = ENGINE_VERSION - 1;
        assert!(book(&[older]).baseline("m", ts(T1)).unwrap().is_ok());
    }

    fn newer_rate(mut row: GroupRateRow) -> GroupRateRow {
        row.engine_version = ENGINE_VERSION + 1;
        row
    }

    #[test]
    fn an_invalid_revision_keeps_its_slot_instead_of_an_older_price() {
        let b = book(&[
            baseline(1, "m", T0, card(1)),
            baseline(2, "m", T2, undecodable()),
        ]);
        assert_eq!(at(&b, T1).unwrap().unwrap().revision_id, 1);
        assert_eq!(at(&b, T3), Some(Err(2)));
        // An invalid override is reported, never replaced by the group's
        // default rate or by the model's own older override.
        let b = book_of(
            Vec::new(),
            vec![
                rate(1, "g", T0, 50_000),
                override_(2, "g", "m", T0, Some(60_000)),
                override_(3, "g", "m", T2, Some(1_000_001)),
            ],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(b.rate("g", "m", ts(T1)).unwrap().unwrap().revision_id, 2);
        assert_eq!(b.rate("g", "m", ts(T3)), Some(Err(3)));
        assert_eq!(b.rate("h", "m", ts(T3)), None);
    }

    // ---- discounts ----

    #[test]
    fn the_discount_in_effect_is_the_latest_started_one() {
        let b = book_of(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![
                discount(2, "acme", T2, 2_000),
                discount(1, "acme", T0, 1_000),
            ],
        );
        assert_eq!(b.discount("acme", ts("2025-12-31T23:59:59Z")), None);
        assert_eq!(b.discount("acme", ts(T1)), Some(Ok((1, 1_000))));
        assert_eq!(b.discount("acme", ts(T2)), Some(Ok((2, 2_000))));
        assert_eq!(b.discount("acme", ts(T3)), Some(Ok((2, 2_000))));
    }

    #[test]
    fn a_same_start_correction_supersedes_in_every_table() {
        // The later revision arrives first, so only the sort can order them.
        let b = book_of(
            Vec::new(),
            Vec::new(),
            vec![
                cost_rate(6, "k1", None, T0, Some(6_000)),
                cost_rate(2, "k1", None, T0, Some(2_000)),
            ],
            vec![discount(7, "acme", T0, 700), discount(3, "acme", T0, 300)],
        );
        let p = b.cost_rate("k1", "m", ts(T1)).unwrap().unwrap();
        assert_eq!((p.revision_id, p.cny), (6, CnyPerUsd(6_000)));
        assert_eq!(b.discount("acme", ts(T1)), Some(Ok((7, 700))));
    }

    #[test]
    fn a_discount_outside_its_range_is_isolated() {
        for bad in [10_001, -1, i64::MAX, i64::MIN] {
            let b = book_of(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vec![discount(3, "acme", T0, bad)],
            );
            assert_eq!(b.discount("acme", ts(T1)), Some(Err(3)), "{bad}");
            let listed = b.invalid_revisions();
            assert_eq!(
                (listed.len(), listed[0].table, listed[0].revision_id),
                (1, RevisionTable::Discount, 3),
                "{bad}"
            );
        }
        for ok in [0, 10_000] {
            let b = book_of(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vec![discount(3, "acme", T0, ok)],
            );
            assert_eq!(b.discount("acme", ts(T1)), Some(Ok((3, ok as u16))), "{ok}");
            assert!(b.invalid_revisions().is_empty());
        }
        let mut newer = discount(3, "acme", T0, 1_000);
        newer.engine_version = ENGINE_VERSION + 1;
        let b = book_of(Vec::new(), Vec::new(), Vec::new(), vec![newer]);
        assert_eq!(b.discount("acme", ts(T1)), Some(Err(3)));
        assert_eq!(
            b.invalid_revisions(),
            [invalid(
                RevisionTable::Discount,
                3,
                "written by a newer engine"
            )]
        );
    }

    // ---- cost rates ----

    #[test]
    fn an_accounts_override_beats_its_default_until_it_ends() {
        let b = book_of(
            Vec::new(),
            Vec::new(),
            vec![
                cost_rate(1, "k1", None, T0, Some(3_000)),
                cost_rate(2, "k1", Some("m"), T1, Some(2_000)),
                cost_rate(3, "k1", Some("m"), T3, None),
            ],
            Vec::new(),
        );
        let at = |model: &str, instant: &str| {
            let p = b.cost_rate("k1", model, ts(instant)).unwrap().unwrap();
            (p.revision_id, p.cny, p.model_override)
        };
        // Before the override starts, the account's default.
        assert_eq!(at("m", T0), (1, CnyPerUsd(3_000), false));
        // From its start, the override.
        assert_eq!(at("m", T1), (2, CnyPerUsd(2_000), true));
        assert_eq!(at("m", T2), (2, CnyPerUsd(2_000), true));
        // Ended, the default again.
        assert_eq!(at("m", T3), (1, CnyPerUsd(3_000), false));
        // The override is for its own model only.
        assert_eq!(at("other", T2), (1, CnyPerUsd(3_000), false));
    }

    #[test]
    fn a_cost_rate_keys_on_its_account_alone() {
        // A group rate and a cost rate with the same owner string are two
        // different things: neither is ever read as the other.
        let b = book_of(
            Vec::new(),
            vec![rate(1, "x", T0, 70_000)],
            vec![cost_rate(2, "k1", None, T0, Some(3_000))],
            Vec::new(),
        );
        assert_eq!(
            b.cost_rate("k1", "m", ts(T1)).unwrap().unwrap().revision_id,
            2
        );
        for other in ["", "k2", "K1", "x"] {
            assert!(b.cost_rate(other, "m", ts(T1)).is_none(), "{other:?}");
        }
        assert!(b.rate("k1", "m", ts(T1)).is_none());
        // Nothing in effect before the first revision.
        assert!(b.cost_rate("k1", "m", ts("2025-12-31T23:59:59Z")).is_none());
    }

    #[test]
    fn an_unusable_cost_rate_is_isolated_and_never_gives_way() {
        let mut newer = cost_rate(5, "k1", Some("n"), T0, Some(2_000));
        newer.engine_version = ENGINE_VERSION + 1;
        let b = book_of(
            Vec::new(),
            Vec::new(),
            vec![
                cost_rate(1, "k1", None, T0, Some(3_000)),
                // Out of range: the model's cost is unknown, not the default.
                cost_rate(2, "k1", Some("m"), T0, Some(1_000_001)),
                // A default row with no rate.
                cost_rate(3, "k2", None, T0, None),
                newer,
            ],
            Vec::new(),
        );
        assert_eq!(b.cost_rate("k1", "m", ts(T1)), Some(Err(2)));
        assert_eq!(b.cost_rate("k2", "m", ts(T1)), Some(Err(3)));
        assert_eq!(b.cost_rate("k1", "n", ts(T1)), Some(Err(5)));
        assert_eq!(
            b.cost_rate("k1", "o", ts(T1)).unwrap().unwrap().revision_id,
            1
        );
        assert_eq!(
            b.invalid_revisions(),
            [
                invalid(
                    RevisionTable::Cost,
                    2,
                    "cny_per_usd_e4 1000001 is outside 0..=1000000"
                ),
                invalid(RevisionTable::Cost, 3, NO_DEFAULT_COST_RATE),
                invalid(RevisionTable::Cost, 5, "written by a newer engine"),
            ]
        );
    }

    // ---- the rows ----

    #[test]
    fn price_rows_read_the_revision_tables_as_json() {
        let rows: PriceRows = serde_json::from_value(json!({
            "baselines": [
                {"revision_id": 1, "engine_version": 1, "model_id": "m",
                 "effective_from": T0, "card": card(1)},
                {"revision_id": 2, "engine_version": 1, "model_id": "m",
                 "effective_from": T2, "card": card(9)}
            ],
            "rates": [
                {"revision_id": 5, "engine_version": 1, "group_id": "g", "model_id": null,
                 "effective_from": T0, "cny_per_usd_e4": 10000},
                {"revision_id": 6, "engine_version": 1, "group_id": "g", "model_id": "m",
                 "effective_from": T0, "cny_per_usd_e4": 72000},
                {"revision_id": 7, "engine_version": 1, "group_id": "g", "model_id": "m",
                 "effective_from": T2, "cny_per_usd_e4": null}
            ],
            "cost_rates": [
                {"revision_id": 3, "engine_version": 1, "provider_key_id": "k1", "model_id": null,
                 "effective_from": T0, "cny_per_usd_e4": 3000}
            ],
            "discounts": [
                {"revision_id": 4, "engine_version": 1, "tenant_id": "acme",
                 "effective_from": T0, "discount_bps": 2500}
            ]
        }))
        .unwrap();
        let b = PriceBook::from_rows(rows);
        assert!(b.invalid_revisions().is_empty());
        assert_eq!(b.baseline("m", ts(T3)).unwrap().unwrap().revision_id, 2);
        let p = b.rate("g", "m", ts(T1)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (6, CnyPerUsd(72_000), true)
        );
        let p = b.rate("g", "m", ts(T3)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (5, CnyPerUsd(10_000), false)
        );
        let p = b.cost_rate("k1", "m", ts(T1)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.cny, p.model_override),
            (3, CnyPerUsd(3_000), false)
        );
        assert_eq!(b.discount("acme", ts(T1)), Some(Ok((4, 2_500))));
    }
}
