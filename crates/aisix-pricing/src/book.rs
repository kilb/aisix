//! The price book: every price revision, indexed for lookup at an instant.
//!
//! Three revision tables feed it: sell prices (the platform's, and each
//! tenant's overrides), cost prices (an upstream model's, and a provider
//! key's), and tenant discounts. A revision applies from its
//! `effective_from` until a later revision of the same key starts; a price
//! never changes in place.
//!
//! Each revision is read on its own. One this engine cannot use is listed in
//! [`PriceBook::invalid_revisions`] and keeps its slot in time, so a lookup
//! that lands on it reports it instead of reaching an older price, and no
//! other key is affected.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::catalogue::Catalogue;
use crate::rates::{RateCard, RateCardError};
use crate::ENGINE_VERSION;

/// The reason given for a revision stamped with a later `ENGINE_VERSION`.
const NEWER_ENGINE: &str = "written by a newer engine";

/// The reason given for a platform sell revision without a card.
const NO_CARD: &str = "a platform price has no card; only a tenant override can end";

/// Rows exactly as the three revision tables hold them. Cards arrive as raw
/// JSON so that one bad or newer card is isolated to its own revision
/// instead of failing the whole book.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceRows {
    pub sell: Vec<SellPriceRow>,
    pub cost: Vec<CostPriceRow>,
    pub discounts: Vec<DiscountRow>,
}

/// One sell-price revision: what the platform charges for a model or, with
/// a `tenant_id`, what that one tenant is charged for it instead.
#[derive(Debug, Clone, Deserialize)]
pub struct SellPriceRow {
    pub revision_id: i64,
    pub engine_version: u32,
    /// The platform model the caller addresses.
    pub model_id: String,
    /// `None`: the platform price. Set: that tenant's override.
    pub tenant_id: Option<String>,
    pub effective_from: DateTime<Utc>,
    /// A [`RateCard`]. `None` only on a tenant row, where it ends the
    /// override and restores the platform price.
    pub card: Option<serde_json::Value>,
}

/// One cost-price revision: what an upstream model costs the platform or,
/// with a `provider_key_id`, what it costs through that one key.
#[derive(Debug, Clone, Deserialize)]
pub struct CostPriceRow {
    pub revision_id: i64,
    pub engine_version: u32,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub provider_key_id: Option<String>,
    pub effective_from: DateTime<Utc>,
    /// A [`RateCard`].
    pub card: serde_json::Value,
}

/// One discount revision: a tenant's discount off the platform price, in
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
    Sell,
    Cost,
    Discount,
}

/// Where a cost price came from. It names the upstream channel, so it stays
/// operator-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CostSource {
    /// The cost revision of the provider key that served the row.
    ProviderKey { revision_id: i64 },
    /// The cost revision of the upstream model, whichever key served it.
    Upstream { revision_id: i64 },
    /// The built-in catalogue's list price: the catalogue's version, the
    /// entry's own key (whichever alias matched) and the entry's
    /// `effective_from`, which tells a key's dated entries apart (`None`:
    /// the entry that applies from the beginning of time).
    Catalogue {
        version: String,
        key: String,
        effective_from: Option<NaiveDate>,
    },
}

/// The sell price in effect for one tenant and model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SellPrice<'a> {
    pub(crate) revision_id: i64,
    /// The tenant's own override priced it, so no discount applies on top.
    pub(crate) tenant_override: bool,
    pub(crate) card: &'a RateCard,
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

/// What one cost source says at `at`: `None` when it has no revision in
/// effect, so a less specific source may decide; otherwise its revision in
/// effect, `Err(revision_id)` when that revision is invalid.
fn cost_at(
    revisions: &[Revision<RateCard>],
    at: DateTime<Utc>,
) -> Option<Result<(i64, &RateCard), i64>> {
    let r = in_effect(revisions, at)?;
    Some(r.read().map(|card| (r.revision_id, card)))
}

/// One model's sell prices.
#[derive(Debug, Default)]
struct ModelSell {
    platform: Vec<Revision<RateCard>>,
    /// By tenant id. A `None` card ended the override.
    overrides: BTreeMap<String, Vec<Revision<Option<RateCard>>>>,
}

/// One upstream model's cost prices.
#[derive(Debug, Default)]
struct UpstreamCost {
    upstream: Vec<Revision<RateCard>>,
    /// By provider key id.
    by_key: BTreeMap<String, Vec<Revision<RateCard>>>,
}

/// Every price revision, indexed by its key, each key's revisions sorted by
/// `(effective_from, revision_id)`.
#[derive(Debug)]
pub struct PriceBook {
    catalogue: Arc<Catalogue>,
    /// By model id.
    sell: BTreeMap<String, ModelSell>,
    /// By upstream provider, then upstream model.
    cost: BTreeMap<String, BTreeMap<String, UpstreamCost>>,
    /// By tenant id.
    discounts: BTreeMap<String, Vec<Revision<u16>>>,
    invalid: Vec<InvalidRevision>,
}

impl PriceBook {
    /// Indexes every revision. It never fails as a whole: a revision this
    /// engine cannot use is listed in [`Self::invalid_revisions`] and
    /// affects only its own key. A revision is unusable when it was written
    /// by a newer engine, its card does not decode or breaks the rate
    /// model's rules, it is a platform price without a card, or it is a
    /// discount outside 0..=10000 basis points.
    pub fn from_rows(mut rows: PriceRows, catalogue: Arc<Catalogue>) -> PriceBook {
        // Taking each table in time order leaves every key's revisions
        // sorted, which `in_effect` relies on.
        rows.sell.sort_by_key(|r| (r.effective_from, r.revision_id));
        rows.cost.sort_by_key(|r| (r.effective_from, r.revision_id));
        rows.discounts
            .sort_by_key(|r| (r.effective_from, r.revision_id));
        let mut invalid = Vec::new();

        let mut sell: BTreeMap<String, ModelSell> = BTreeMap::new();
        for row in rows.sell {
            let card =
                this_engine(row.engine_version).and_then(|()| row.card.map(read_card).transpose());
            let model = sell.entry(row.model_id).or_default();
            let (id, from) = (row.revision_id, row.effective_from);
            match row.tenant_id {
                Some(tenant) => {
                    let revision = admit(&mut invalid, RevisionTable::Sell, id, from, card);
                    model.overrides.entry(tenant).or_default().push(revision);
                }
                None => {
                    let card = card.and_then(|card| card.ok_or_else(|| NO_CARD.to_owned()));
                    let revision = admit(&mut invalid, RevisionTable::Sell, id, from, card);
                    model.platform.push(revision);
                }
            }
        }

        let mut cost: BTreeMap<String, BTreeMap<String, UpstreamCost>> = BTreeMap::new();
        for row in rows.cost {
            let card = this_engine(row.engine_version).and_then(|()| read_card(row.card));
            let revision = admit(
                &mut invalid,
                RevisionTable::Cost,
                row.revision_id,
                row.effective_from,
                card,
            );
            let upstream = cost
                .entry(row.upstream_provider)
                .or_default()
                .entry(row.upstream_model)
                .or_default();
            match row.provider_key_id {
                Some(key) => upstream.by_key.entry(key).or_default().push(revision),
                None => upstream.upstream.push(revision),
            }
        }

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
            catalogue,
            sell,
            cost,
            discounts,
            invalid,
        }
    }

    /// Every revision this engine cannot use, for alerts: sell revisions,
    /// then cost, then discounts, each in time order.
    pub fn invalid_revisions(&self) -> &[InvalidRevision] {
        &self.invalid
    }

    /// The sell price of `model_id` for `tenant_id` at `at`. It keys on the
    /// model the caller addressed, never on the upstream that served it.
    ///
    /// The tenant's override in effect wins. One in effect that ended the
    /// override gives way to the platform price in effect. An invalid
    /// revision in effect, override or platform price, is `Err(revision_id)`
    /// and never gives way to another price. `None` when no price is in
    /// effect.
    pub(crate) fn sell(
        &self,
        tenant_id: &str,
        model_id: &str,
        at: DateTime<Utc>,
    ) -> Option<Result<SellPrice<'_>, i64>> {
        let model = self.sell.get(model_id)?;
        let tenant = model.overrides.get(tenant_id);
        if let Some(r) = tenant.and_then(|revisions| in_effect(revisions, at)) {
            match r.read() {
                Ok(Some(card)) => {
                    return Some(Ok(SellPrice {
                        revision_id: r.revision_id,
                        tenant_override: true,
                        card,
                    }))
                }
                // The override ended.
                Ok(None) => {}
                Err(revision_id) => return Some(Err(revision_id)),
            }
        }
        let r = in_effect(&model.platform, at)?;
        Some(r.read().map(|card| SellPrice {
            revision_id: r.revision_id,
            tenant_override: false,
            card,
        }))
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

    /// What `provider`'s `model`, served through `provider_key_id`, costs
    /// the platform at `at`. It keys on the upstream that served the row,
    /// never on the model the caller addressed.
    ///
    /// The most specific source with a revision in effect decides: the
    /// key's cost revisions, then the upstream model's, then the built-in
    /// catalogue's list price in effect at `at`. When the deciding revision
    /// is invalid the cost is unknown (`None`). It never gives way to a less
    /// specific source: a key's row exists because its cost differs, and a
    /// substituted cost could not be told apart on the charge. The revision
    /// is still listed in [`Self::invalid_revisions`].
    ///
    /// Cost revisions match `provider` and `model` exactly. The catalogue's
    /// aliases apply only to the catalogue fallback.
    pub(crate) fn cost(
        &self,
        provider_key_id: Option<&str>,
        provider: &str,
        model: &str,
        at: DateTime<Utc>,
    ) -> Option<(CostSource, &RateCard)> {
        let upstream = self.cost.get(provider).and_then(|m| m.get(model));
        let by_key = upstream
            .zip(provider_key_id)
            .and_then(|(u, key)| u.by_key.get(key))
            .and_then(|revisions| cost_at(revisions, at));
        if let Some(decided) = by_key {
            let (revision_id, card) = decided.ok()?;
            return Some((CostSource::ProviderKey { revision_id }, card));
        }
        if let Some(decided) = upstream.and_then(|u| cost_at(&u.upstream, at)) {
            let (revision_id, card) = decided.ok()?;
            return Some((CostSource::Upstream { revision_id }, card));
        }
        let entry = self.catalogue.get(provider, model, at)?;
        Some((
            CostSource::Catalogue {
                version: self.catalogue.version().to_owned(),
                key: entry.key.clone(),
                effective_from: entry.effective_from,
            },
            &entry.card,
        ))
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
    fn card(n: u32) -> Option<Value> {
        Some(json!({"default": {"base": {"input": n.to_string(), "output": n.to_string()}}}))
    }

    /// A card this engine cannot decode: `output` is required.
    fn undecodable() -> Option<Value> {
        Some(json!({"default": {"base": {"input": "1"}}}))
    }

    fn sell(
        revision_id: i64,
        model_id: &str,
        tenant_id: Option<&str>,
        from: &str,
        card: Option<Value>,
    ) -> SellPriceRow {
        SellPriceRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            model_id: model_id.to_owned(),
            tenant_id: tenant_id.map(str::to_owned),
            effective_from: ts(from),
            card,
        }
    }

    fn cost(
        revision_id: i64,
        provider: &str,
        model: &str,
        provider_key_id: Option<&str>,
        from: &str,
        card: Option<Value>,
    ) -> CostPriceRow {
        CostPriceRow {
            revision_id,
            engine_version: ENGINE_VERSION,
            upstream_provider: provider.to_owned(),
            upstream_model: model.to_owned(),
            provider_key_id: provider_key_id.map(str::to_owned),
            effective_from: ts(from),
            card: card.unwrap_or(Value::Null),
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

    /// A synthetic catalogue, no real price: `openai/up-1` (alias
    /// `up-1-latest`) at input 3 from the beginning of time, and at input 4
    /// from 2026-12-01.
    fn catalogue() -> Arc<Catalogue> {
        let entry = |from: &str, input: &str| {
            format!(
                r#"{{"key":"openai/up-1","aliases":["up-1-latest"],"source_url":"https://openai.com/api/pricing",
                    "as_of":"2026-11-06","effective_from":{from},
                    "card":{{"default":{{"base":{{"input":"{input}","output":"1"}}}}}}}}"#
            )
        };
        let json = format!(
            r#"{{"version":"2026-11-06","entries":[{},{}]}}"#,
            entry("null", "3"),
            entry(r#""2026-12-01""#, "4")
        );
        Arc::new(Catalogue::from_json(&json).unwrap())
    }

    fn book_of(
        sell: Vec<SellPriceRow>,
        cost: Vec<CostPriceRow>,
        discounts: Vec<DiscountRow>,
    ) -> PriceBook {
        PriceBook::from_rows(
            PriceRows {
                sell,
                cost,
                discounts,
            },
            catalogue(),
        )
    }

    fn book(sell: &[SellPriceRow]) -> PriceBook {
        book_of(sell.to_vec(), Vec::new(), Vec::new())
    }

    /// The default base `input` rate, which tells the test cards apart.
    fn input(card: &RateCard) -> Rate {
        card.0[&ServiceTier::default_tier()].base.input
    }

    fn usd(n: u64) -> Rate {
        Rate(n * 1_000_000)
    }

    /// The cost source for key `k1` on `openai/up-1` at `at`.
    fn source_of(b: &PriceBook, at: &str) -> CostSource {
        b.cost(Some("k1"), "openai", "up-1", ts(at)).unwrap().0
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
            sell(1, "m", None, "2026-01-01T00:00:00Z", card(1)),
            sell(2, "m", None, "2026-02-01T00:00:00Z", card(9)),
        ]);
        let at = |s| b.sell("t", "m", ts(s)).unwrap().unwrap();
        assert_eq!(at("2026-01-31T23:59:59.999Z").revision_id, 1);
        assert_eq!(at("2026-01-31T23:59:59.999999999Z").revision_id, 1);
        assert_eq!(at("2026-02-01T00:00:00Z").revision_id, 2);
        // The price is the revision's own card.
        assert_eq!(input(at("2026-01-31T23:59:59.999Z").card), usd(1));
        assert_eq!(input(at("2026-02-01T00:00:00Z").card), usd(9));
    }

    #[test]
    fn a_correction_with_the_same_start_supersedes() {
        let b = book(&[
            sell(1, "m", None, T0, card(1)),
            sell(5, "m", None, T0, card(2)),
        ]);
        assert_eq!(b.sell("t", "m", ts(T1)).unwrap().unwrap().revision_id, 5);
        assert_eq!(b.sell("t", "m", ts(T0)).unwrap().unwrap().revision_id, 5);
        // Whatever order the rows arrive in.
        let b = book(&[
            sell(5, "m", None, T0, card(2)),
            sell(1, "m", None, T0, card(1)),
        ]);
        let p = b.sell("t", "m", ts(T1)).unwrap().unwrap();
        assert_eq!((p.revision_id, input(p.card)), (5, usd(2)));
    }

    #[test]
    fn nothing_is_in_effect_before_the_first_revision() {
        assert!(book(&[sell(1, "m", None, "2026-01-01T00:00:00Z", card(1))])
            .sell("t", "m", ts("2025-12-31T23:59:59Z"))
            .is_none());
        assert!(book(&[sell(1, "m", None, T0, card(1))])
            .sell("t", "other-model", ts(T1))
            .is_none());
    }

    #[test]
    fn several_future_revisions_each_take_effect_at_their_own_start() {
        // Ruling P2-R12: an import writes a model's later-dated list prices
        // as further revisions, all of them ahead of time.
        let b = book(&[
            sell(3, "m", None, "2026-03-01T00:00:00Z", card(3)),
            sell(1, "m", None, T0, card(1)),
            sell(2, "m", None, T2, card(2)),
        ]);
        for (at, want) in [
            (T1, 1),
            (T2, 2),
            (T3, 2),
            ("2026-03-01T00:00:00Z", 3),
            ("2027-01-01T00:00:00Z", 3),
        ] {
            let p = b.sell("t", "m", ts(at)).unwrap().unwrap();
            assert_eq!(
                (p.revision_id, input(p.card)),
                (want, usd(want as u64)),
                "{at}"
            );
        }
    }

    // ---- tenant plans ----

    #[test]
    fn a_tenant_override_wins_for_that_tenant_only() {
        let b = book(&[
            sell(1, "m", None, T0, card(1)),
            sell(2, "m", Some("acme"), T0, card(2)),
            sell(3, "m2", None, T0, card(3)),
        ]);
        let p = b.sell("acme", "m", ts(T1)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.tenant_override, input(p.card)),
            (2, true, usd(2))
        );
        let p = b.sell("other", "m", ts(T1)).unwrap().unwrap();
        assert_eq!(
            (p.revision_id, p.tenant_override, input(p.card)),
            (1, false, usd(1))
        );
        // The override is for its own model only.
        let p = b.sell("acme", "m2", ts(T1)).unwrap().unwrap();
        assert_eq!((p.revision_id, p.tenant_override), (3, false));
    }

    #[test]
    fn ending_an_override_restores_the_platform_price() {
        let b = book(&[
            sell(1, "m", None, T0, card(1)),
            sell(2, "m", Some("acme"), T0, card(2)),
            sell(3, "m", Some("acme"), T2, None),
        ]);
        assert_eq!(b.sell("acme", "m", ts(T1)).unwrap().unwrap().revision_id, 2);
        let p = b.sell("acme", "m", ts(T3)).unwrap().unwrap();
        assert_eq!((p.revision_id, p.tenant_override), (1, false));
    }

    #[test]
    fn an_override_before_its_start_leaves_the_platform_price() {
        let b = book(&[
            sell(1, "m", None, T0, card(1)),
            sell(2, "m", Some("acme"), T2, card(2)),
        ]);
        let p = b.sell("acme", "m", ts(T1)).unwrap().unwrap();
        assert_eq!((p.revision_id, p.tenant_override), (1, false));
        let p = b.sell("acme", "m", ts(T2)).unwrap().unwrap();
        assert_eq!((p.revision_id, p.tenant_override), (2, true));
    }

    #[test]
    fn a_tenant_plan_never_prices_another_tenant() {
        let b = book_of(
            vec![
                sell(1, "m", None, T0, card(1)),
                sell(2, "m", Some("acme"), T0, card(2)),
                sell(3, "m", Some("broken"), T0, undecodable()),
            ],
            Vec::new(),
            vec![
                discount(4, "acme", T0, 5_000),
                discount(5, "broken", T0, 10_001),
            ],
        );
        // Each tenant's own plan prices it, a broken one included.
        let p = b.sell("acme", "m", ts(T1)).unwrap().unwrap();
        assert_eq!((p.revision_id, p.tenant_override), (2, true));
        assert_eq!(b.discount("acme", ts(T1)), Some(Ok((4, 5_000))));
        assert_eq!(b.sell("broken", "m", ts(T1)), Some(Err(3)));
        assert_eq!(b.discount("broken", ts(T1)), Some(Err(5)));
        // No other tenant sees either plan: ids match exactly.
        for other in ["other", "ACME", "acme ", "broke", ""] {
            let p = b.sell(other, "m", ts(T1)).unwrap().unwrap();
            assert_eq!((p.revision_id, p.tenant_override), (1, false), "{other:?}");
            assert_eq!(b.discount(other, ts(T1)), None, "{other:?}");
        }
    }

    // ---- isolation ----

    #[test]
    fn a_revision_the_engine_cannot_read_is_isolated_to_its_model() {
        let b = book(&[
            sell(7, "a", None, T0, undecodable()),
            sell(8, "b", None, T0, card(1)),
        ]);
        assert_eq!(b.sell("t", "a", ts(T1)), Some(Err(7)));
        assert_eq!(b.sell("t", "b", ts(T1)).unwrap().unwrap().revision_id, 8);
        let listed = b.invalid_revisions();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].table, listed[0].revision_id),
            (RevisionTable::Sell, 7)
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
            sell(7, "a", None, T0, Some(no_default)),
            sell(8, "b", None, T0, card(1)),
        ]);
        assert_eq!(b.sell("t", "a", ts(T1)), Some(Err(7)));
        assert!(b.sell("t", "b", ts(T1)).unwrap().is_ok());
        assert_eq!(
            b.invalid_revisions(),
            [invalid(
                RevisionTable::Sell,
                7,
                "rate card has no default tier"
            )]
        );
    }

    #[test]
    fn a_revision_written_by_a_newer_engine_is_isolated() {
        let newer = |mut row: SellPriceRow| {
            row.engine_version = ENGINE_VERSION + 1;
            row
        };
        // A newer engine's card, valid here or using a rate this engine
        // does not know, is unreadable for the same reason.
        let unknown_rate =
            json!({"default": {"base": {"input": "1", "output": "2", "image_count": "3"}}});
        let b = book(&[
            newer(sell(7, "m", None, T0, card(1))),
            newer(sell(9, "n", None, T0, Some(unknown_rate))),
            sell(8, "o", None, T0, card(1)),
        ]);
        assert_eq!(b.sell("t", "m", ts(T1)), Some(Err(7)));
        assert_eq!(b.sell("t", "n", ts(T1)), Some(Err(9)));
        assert!(b.sell("t", "o", ts(T1)).unwrap().is_ok());
        assert_eq!(
            b.invalid_revisions(),
            [
                invalid(RevisionTable::Sell, 7, "written by a newer engine"),
                invalid(RevisionTable::Sell, 9, "written by a newer engine"),
            ]
        );
        // A newer engine's row that ends an override carries no card, and
        // is unreadable all the same: it does not end the override.
        let b = book(&[
            sell(1, "m", None, T0, card(1)),
            sell(2, "m", Some("acme"), T0, card(2)),
            newer(sell(3, "m", Some("acme"), T2, None)),
        ]);
        assert_eq!(b.sell("acme", "m", ts(T3)), Some(Err(3)));
        // An older engine's revision is read as usual.
        let mut older = sell(1, "m", None, T0, card(1));
        older.engine_version = ENGINE_VERSION - 1;
        assert!(book(&[older]).sell("t", "m", ts(T1)).unwrap().is_ok());
    }

    #[test]
    fn an_invalid_revision_keeps_its_slot_instead_of_an_older_price() {
        let b = book(&[
            sell(1, "m", None, T0, card(1)),
            sell(2, "m", None, T2, undecodable()),
        ]);
        assert_eq!(b.sell("t", "m", ts(T1)).unwrap().unwrap().revision_id, 1);
        assert_eq!(b.sell("t", "m", ts(T3)), Some(Err(2)));
        // An invalid override is reported, never replaced by the platform
        // price or by the tenant's own older override.
        let b = book(&[
            sell(1, "m", None, T0, card(1)),
            sell(2, "m", Some("acme"), T0, card(2)),
            sell(3, "m", Some("acme"), T2, undecodable()),
        ]);
        assert_eq!(b.sell("acme", "m", ts(T1)).unwrap().unwrap().revision_id, 2);
        assert_eq!(b.sell("acme", "m", ts(T3)), Some(Err(3)));
        assert_eq!(
            b.sell("other", "m", ts(T3)).unwrap().unwrap().revision_id,
            1
        );
    }

    #[test]
    fn a_platform_price_without_a_card_is_isolated() {
        // Only a tenant override can end; a platform row needs a card.
        let b = book(&[
            sell(1, "m", None, T0, card(1)),
            sell(2, "m", None, T2, None),
        ]);
        assert_eq!(b.sell("t", "m", ts(T1)).unwrap().unwrap().revision_id, 1);
        assert_eq!(b.sell("t", "m", ts(T3)), Some(Err(2)));
        let listed = b.invalid_revisions();
        assert_eq!(
            (listed.len(), listed[0].table, listed[0].revision_id),
            (1, RevisionTable::Sell, 2)
        );
    }

    // ---- discounts ----

    #[test]
    fn the_discount_in_effect_is_the_latest_started_one() {
        let b = book_of(
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
            vec![
                cost(6, "openai", "up-1", None, T0, card(6)),
                cost(2, "openai", "up-1", None, T0, card(2)),
            ],
            vec![discount(7, "acme", T0, 700), discount(3, "acme", T0, 300)],
        );
        let (source, card) = b.cost(None, "openai", "up-1", ts(T1)).unwrap();
        assert_eq!(
            (source, input(card)),
            (CostSource::Upstream { revision_id: 6 }, usd(6))
        );
        assert_eq!(b.discount("acme", ts(T1)), Some(Ok((7, 700))));
    }

    #[test]
    fn a_discount_outside_its_range_is_isolated() {
        for bad in [10_001, -1, i64::MAX, i64::MIN] {
            let b = book_of(Vec::new(), Vec::new(), vec![discount(3, "acme", T0, bad)]);
            assert_eq!(b.discount("acme", ts(T1)), Some(Err(3)), "{bad}");
            let listed = b.invalid_revisions();
            assert_eq!(
                (listed.len(), listed[0].table, listed[0].revision_id),
                (1, RevisionTable::Discount, 3),
                "{bad}"
            );
        }
        for ok in [0, 10_000] {
            let b = book_of(Vec::new(), Vec::new(), vec![discount(3, "acme", T0, ok)]);
            assert_eq!(b.discount("acme", ts(T1)), Some(Ok((3, ok as u16))), "{ok}");
            assert!(b.invalid_revisions().is_empty());
        }
        let mut newer = discount(3, "acme", T0, 1_000);
        newer.engine_version = ENGINE_VERSION + 1;
        let b = book_of(Vec::new(), Vec::new(), vec![newer]);
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

    // ---- cost ----

    #[test]
    fn cost_prefers_the_provider_key_then_the_upstream_then_the_catalogue() {
        let b = book_of(
            Vec::new(),
            vec![
                cost(1, "openai", "up-1", Some("k1"), T0, card(1)),
                cost(2, "openai", "up-1", None, T0, card(2)),
            ],
            Vec::new(),
        );
        let (source, card) = b.cost(Some("k1"), "openai", "up-1", ts(T1)).unwrap();
        assert_eq!(
            (source, input(card)),
            (CostSource::ProviderKey { revision_id: 1 }, usd(1))
        );
        for key in [Some("k2"), None] {
            let (source, card) = b.cost(key, "openai", "up-1", ts(T1)).unwrap();
            assert_eq!(
                (source, input(card)),
                (CostSource::Upstream { revision_id: 2 }, usd(2))
            );
        }
        // Cost rows match the upstream model exactly; under the catalogue's
        // alias only the catalogue prices it.
        let (source, card) = b.cost(Some("k1"), "openai", "up-1-latest", ts(T1)).unwrap();
        let catalogue = CostSource::Catalogue {
            version: "2026-11-06".to_owned(),
            key: "openai/up-1".to_owned(),
            effective_from: None,
        };
        assert_eq!((source, input(card)), (catalogue.clone(), usd(3)));
        // No cost row at all: the catalogue, under the entry's own key.
        let empty = book(&[]);
        let (source, card) = empty.cost(Some("k1"), "openai", "up-1", ts(T1)).unwrap();
        assert_eq!((source, input(card)), (catalogue, usd(3)));
        assert!(empty.cost(None, "openai", "unknown", ts(T1)).is_none());
        assert!(empty.cost(None, "google", "up-1", ts(T1)).is_none());
    }

    #[test]
    fn a_cost_row_not_yet_in_effect_leaves_the_next_source() {
        let b = book_of(
            Vec::new(),
            vec![
                cost(1, "openai", "up-1", Some("k1"), T2, card(1)),
                cost(2, "openai", "up-1", None, T3, card(2)),
            ],
            Vec::new(),
        );
        assert!(matches!(source_of(&b, T1), CostSource::Catalogue { .. }));
        assert_eq!(
            source_of(&b, T2),
            CostSource::ProviderKey { revision_id: 1 }
        );
        let unkeyed = |at| b.cost(None, "openai", "up-1", ts(at)).unwrap().0;
        assert!(matches!(unkeyed(T2), CostSource::Catalogue { .. }));
        assert_eq!(unkeyed(T3), CostSource::Upstream { revision_id: 2 });
    }

    #[test]
    fn an_invalid_cost_revision_leaves_the_cost_unknown() {
        // Ruling P2-R14: the most specific source with a revision in effect
        // decides, and an invalid one decides "unknown".
        let b = book_of(
            Vec::new(),
            vec![
                cost(1, "openai", "up-1", Some("k1"), T0, card(1)),
                cost(2, "openai", "up-1", Some("k1"), T2, undecodable()),
                cost(3, "openai", "up-1", None, T0, card(3)),
            ],
            Vec::new(),
        );
        assert_eq!(
            source_of(&b, T1),
            CostSource::ProviderKey { revision_id: 1 }
        );
        // Neither the key's older row nor the upstream row stands in.
        assert_eq!(b.cost(Some("k1"), "openai", "up-1", ts(T3)), None);
        // Another key, or none, still reads the upstream row.
        for key in [Some("k2"), None] {
            let (source, _) = b.cost(key, "openai", "up-1", ts(T3)).unwrap();
            assert_eq!(source, CostSource::Upstream { revision_id: 3 });
        }
        let listed = b.invalid_revisions();
        assert_eq!(
            (listed.len(), listed[0].table, listed[0].revision_id),
            (1, RevisionTable::Cost, 2)
        );

        // An invalid upstream row is not replaced by the catalogue, which
        // prices this model; nor is a newer engine's key row.
        let mut newer = cost(5, "openai", "up-1", Some("k1"), T0, card(1));
        newer.engine_version = ENGINE_VERSION + 1;
        let b = book_of(
            Vec::new(),
            vec![cost(4, "openai", "up-1", None, T0, None), newer],
            Vec::new(),
        );
        assert!(book(&[]).cost(None, "openai", "up-1", ts(T1)).is_some());
        assert_eq!(b.cost(None, "openai", "up-1", ts(T1)), None);
        assert_eq!(b.cost(Some("k1"), "openai", "up-1", ts(T1)), None);
        let listed: Vec<_> = b
            .invalid_revisions()
            .iter()
            .map(|r| (r.table, r.revision_id))
            .collect();
        assert_eq!(listed, [(RevisionTable::Cost, 4), (RevisionTable::Cost, 5)]);
    }

    #[test]
    fn the_catalogue_fallback_is_dated_by_the_event_instant() {
        // Ruling P2-R7: the list price in effect when the event occurred.
        // Ruling P2-R13: the source names the dated entry that priced it.
        let b = book(&[]);
        let dec_1 = NaiveDate::from_ymd_opt(2026, 12, 1);
        for (at, want, effective_from) in [
            ("2026-11-30T23:59:59.999Z", 3, None),
            ("2026-12-01T00:00:00Z", 4, dec_1),
            ("2027-06-01T00:00:00Z", 4, dec_1),
        ] {
            for model in ["up-1", "up-1-latest"] {
                let (source, card) = b.cost(None, "openai", model, ts(at)).unwrap();
                assert_eq!(
                    (source, input(card)),
                    (
                        CostSource::Catalogue {
                            version: "2026-11-06".to_owned(),
                            key: "openai/up-1".to_owned(),
                            effective_from,
                        },
                        usd(want)
                    ),
                    "{model} at {at}"
                );
            }
        }
    }

    // ---- the rows ----

    #[test]
    fn price_rows_read_the_revision_tables_as_json() {
        let rows: PriceRows = serde_json::from_value(json!({
            "sell": [
                {"revision_id": 1, "engine_version": 1, "model_id": "m", "tenant_id": null,
                 "effective_from": T0, "card": card(1)},
                {"revision_id": 2, "engine_version": 1, "model_id": "m", "tenant_id": "acme",
                 "effective_from": T2, "card": null}
            ],
            "cost": [
                {"revision_id": 3, "engine_version": 1, "upstream_provider": "openai",
                 "upstream_model": "up-1", "provider_key_id": "k1", "effective_from": T0,
                 "card": card(2)}
            ],
            "discounts": [
                {"revision_id": 4, "engine_version": 1, "tenant_id": "acme",
                 "effective_from": T0, "discount_bps": 2500}
            ]
        }))
        .unwrap();
        let b = PriceBook::from_rows(rows, catalogue());
        assert!(b.invalid_revisions().is_empty());
        assert_eq!(b.sell("acme", "m", ts(T3)).unwrap().unwrap().revision_id, 1);
        assert_eq!(
            b.cost(Some("k1"), "openai", "up-1", ts(T1)).unwrap().0,
            CostSource::ProviderKey { revision_id: 3 }
        );
        assert_eq!(b.discount("acme", ts(T1)), Some(Ok((4, 2_500))));
    }
}
