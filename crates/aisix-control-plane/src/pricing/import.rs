//! The third-party price import: an operator uploads a public price dataset,
//! and each priced entry of it, per date it takes effect, becomes a proposal
//! labelled third-party. Nothing here is a price until an operator confirms a
//! proposal.
//!
//! Every price is read from its decimal text and never through a binary
//! float: a number that is not plain digits with at most six decimals is
//! flagged, never rounded or truncated.
//!
//! # How an entry becomes proposals
//!
//! The dataset is an array of providers, each with its `pricing_urls` and
//! its `models`. A model's `prices` is a price map, or a list of maps each
//! with an optional `constraint`; its `price_variants` are partial maps
//! selected by a `when` condition, each with an optional `constraint`. The
//! dataset's own resolution, as its reference package resolves a model's
//! prices for a request and prices a tiered field (read at the commit the
//! test snapshot was taken from), is followed exactly:
//!
//! - **Dates.** In a list, the last entry whose `start_date` (UTC day) has
//!   come, or that has no constraint, is the standard price; if none, the
//!   first. One proposal is made for the moment of upload and one for each
//!   later `start_date` of an entry or a variant, each priced as the dataset
//!   would price that day. A date already past makes nothing of its own.
//! - **Variants.** `{processing: batch}` is the `batch` tier and
//!   `{service_tier: <name>}` that tier. A variant overrides only the prices
//!   it names; every other price of its tier is the standard one. For each
//!   price the first active variant of the tier wins. A `default` variant
//!   that repeats the standard prices changes nothing; one that differs, a
//!   name the rate model refuses as a tier, and any other condition are
//!   flagged `unsupported_variant`.
//! - **Tiers.** `{base, tiers: [{start, price}]}` applies a tier's price to
//!   every token once the prompt is strictly longer than `start`, as a
//!   long-context band does. Each distinct `start` across the card is one
//!   band, and in it each price is its tier with the greatest `start` at or
//!   below the band's threshold, or its base.
//! - **Fields** map to card dimensions one to one; `web_searches_kcount` is
//!   priced per thousand searches and becomes `web_search` per million. Any
//!   other field is flagged `unsupported_field` and left off the card.
//! - **Time windows** (`start_time`, `end_time`) anywhere in a model flag
//!   every proposal of it `time_of_day`; the price without the window is the
//!   card.
//!
//! A proposal with a `bad_number` has no card, and neither does one whose
//! prices do not make a card [`RateCard::validate`] accepts (`invalid_card`).
//! Any other flag leaves the card in place for the comparison, but a flagged
//! proposal is never confirmable.

use std::collections::{BTreeMap, BTreeSet};

use aisix_pricing::{Dimension, LongContextBand, Rate, RateCard, Rates, ServiceTier, TierRates};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;

use crate::error::{ApiError, Detail};

/// The dataset this import reads, recorded on every batch and on every
/// baseline confirmed from it. The one place its name appears in code.
pub const DATASET: &str = "pydantic/genai-prices";

/// The largest upload `POST /admin/pricing/imports` reads.
pub const MAX_IMPORT_BYTES: usize = 8 << 20;

/// The most proposals one upload may make.
pub const MAX_IMPORT_PROPOSALS: usize = 10_000;

/// Why a proposal cannot be confirmed as it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// A price that is not plain digits with at most six decimals.
    BadNumber,
    /// A price for a quantity no card dimension meters.
    UnsupportedField,
    /// A price under a condition a card cannot express.
    UnsupportedVariant,
    /// A price that holds only at some times of day.
    TimeOfDay,
    /// The prices do not make a card the rate model accepts.
    InvalidCard,
}

/// One dataset entry at one date: the card it comes to, or the flags that
/// say why it comes to none.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedProposal {
    /// The dataset's provider id.
    pub provider: String,
    /// The dataset's model id.
    pub model: String,
    /// `None`: in effect at upload. Otherwise the UTC midnight of the date it
    /// takes effect.
    pub effective_from: Option<DateTime<Utc>>,
    pub card: Option<RateCard>,
    pub flags: Vec<Flag>,
    /// The provider's first cited pricing page.
    pub pricing_url: Option<String>,
    /// The date the dataset says it checked the model's prices, if it says.
    pub prices_checked: Option<NaiveDate>,
}

/// A `422` at `path`.
fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

/// Every proposal `data` makes at `now`, in the dataset's order of
/// providers and models, each model's in date order. See the module
/// documentation.
///
/// Refused at `/data`: a body that is not an array of providers with their
/// models, one that prices nothing, and one that makes more than
/// [`MAX_IMPORT_PROPOSALS`] proposals.
pub fn parse(data: &RawValue, now: DateTime<Utc>) -> Result<Vec<ParsedProposal>, ApiError> {
    let unrecognised = || refused("/data", "不是可识别的价格数据集");
    let providers: Vec<Provider<'_>> =
        serde_json::from_str(data.get()).map_err(|_| unrecognised())?;
    let today = now.date_naive();
    let mut proposals = Vec::new();
    for provider in &providers {
        let pricing_url = provider.pricing_urls.first().cloned();
        for model in &provider.models {
            let entries = conditionals(model.prices).ok_or_else(unrecognised)?;
            let variants = model.price_variants.as_deref().unwrap_or_default();
            for (effective_from, card, flags) in model_proposals(&entries, variants, today) {
                if proposals.len() == MAX_IMPORT_PROPOSALS {
                    return Err(refused(
                        "/data",
                        format!("价格数据集产生的提议超过 {MAX_IMPORT_PROPOSALS} 条"),
                    ));
                }
                proposals.push(ParsedProposal {
                    provider: provider.id.clone(),
                    model: model.id.clone(),
                    effective_from,
                    card,
                    flags,
                    pricing_url: pricing_url.clone(),
                    prices_checked: model.prices_checked,
                });
            }
        }
    }
    if proposals.is_empty() {
        return Err(unrecognised());
    }
    Ok(proposals)
}

/// A price map: each price field to its value, kept as the text the file
/// wrote.
type PriceMap<'a> = BTreeMap<String, &'a RawValue>;

#[derive(Deserialize)]
struct Provider<'a> {
    id: String,
    #[serde(default)]
    pricing_urls: Vec<String>,
    #[serde(borrow)]
    models: Vec<Model<'a>>,
}

#[derive(Deserialize)]
struct Model<'a> {
    id: String,
    /// A price map, or a list of conditional ones ([`conditionals`]).
    #[serde(borrow)]
    prices: &'a RawValue,
    #[serde(default, borrow)]
    price_variants: Option<Vec<Variant<'a>>>,
    #[serde(default)]
    prices_checked: Option<NaiveDate>,
}

/// One entry of a model's list of prices.
#[derive(Deserialize)]
struct Conditional<'a> {
    #[serde(default, borrow)]
    constraint: Option<PriceMap<'a>>,
    #[serde(borrow)]
    prices: PriceMap<'a>,
}

#[derive(Deserialize)]
struct Variant<'a> {
    when: BTreeMap<String, String>,
    #[serde(borrow)]
    prices: PriceMap<'a>,
    #[serde(default, borrow)]
    constraint: Option<PriceMap<'a>>,
}

/// When an entry or a variant applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Condition {
    Always,
    /// From this UTC day on.
    From(NaiveDate),
    TimeOfDay,
    /// A constraint of no shape the dataset documents.
    Other,
}

impl Condition {
    fn of(constraint: Option<&PriceMap<'_>>) -> Condition {
        let Some(c) = constraint else {
            return Condition::Always;
        };
        if c.contains_key("start_time") || c.contains_key("end_time") {
            return Condition::TimeOfDay;
        }
        match (c.len(), c.get("start_date")) {
            (1, Some(day)) => {
                serde_json::from_str(day.get()).map_or(Condition::Other, Condition::From)
            }
            _ => Condition::Other,
        }
    }

    /// Whether it applies on `day`. A time window or an unknown condition
    /// never does: the proposal is flagged instead.
    fn active(self, day: NaiveDate) -> bool {
        match self {
            Condition::Always => true,
            Condition::From(from) => from <= day,
            Condition::TimeOfDay | Condition::Other => false,
        }
    }
}

/// A model's prices as a list, a single map being a list of one entry with
/// no constraint. `None` for anything else, or an empty list.
fn conditionals(prices: &RawValue) -> Option<Vec<Conditional<'_>>> {
    let text = prices.get();
    if text.starts_with('[') {
        let list: Vec<Conditional<'_>> = serde_json::from_str(text).ok()?;
        return (!list.is_empty()).then_some(list);
    }
    let prices = serde_json::from_str(text).ok()?;
    Some(vec![Conditional {
        constraint: None,
        prices,
    }])
}

/// One proposal of a model: its start (`None` at upload), card and flags.
type Dated = (Option<DateTime<Utc>>, Option<RateCard>, Vec<Flag>);

/// One model's proposals: for the moment of upload and for each later date.
fn model_proposals(
    entries: &[Conditional<'_>],
    variants: &[Variant<'_>],
    today: NaiveDate,
) -> Vec<Dated> {
    let entry_conditions: Vec<Condition> = entries
        .iter()
        .map(|e| Condition::of(e.constraint.as_ref()))
        .collect();
    let variant_conditions: Vec<Condition> = variants
        .iter()
        .map(|v| Condition::of(v.constraint.as_ref()))
        .collect();
    let mut model_flags = BTreeSet::new();
    let mut later = BTreeSet::new();
    for condition in entry_conditions.iter().chain(&variant_conditions) {
        match condition {
            Condition::TimeOfDay => {
                model_flags.insert(Flag::TimeOfDay);
            }
            Condition::Other => {
                model_flags.insert(Flag::UnsupportedVariant);
            }
            Condition::From(day) if *day > today => {
                later.insert(*day);
            }
            Condition::Always | Condition::From(_) => {}
        }
    }
    std::iter::once(None)
        .chain(later.into_iter().map(Some))
        .map(|from| {
            let day = from.unwrap_or(today);
            let mut flags = model_flags.clone();
            // The last entry in effect, else the first (there is one).
            let standard = entries
                .iter()
                .zip(&entry_conditions)
                .rev()
                .find(|(_, c)| c.active(day))
                .map_or(&entries[0].prices, |(e, _)| &e.prices);
            let mut overrides: BTreeMap<ServiceTier, PriceMap<'_>> = BTreeMap::new();
            for (variant, condition) in variants.iter().zip(&variant_conditions) {
                if !condition.active(day) {
                    continue;
                }
                let Some(tier) = variant_tier(&variant.when) else {
                    flags.insert(Flag::UnsupportedVariant);
                    continue;
                };
                let prices = overrides.entry(tier).or_default();
                for (field, value) in &variant.prices {
                    prices.entry(field.clone()).or_insert(value);
                }
            }
            let mut tiers = BTreeMap::from([(ServiceTier::default_tier(), standard.clone())]);
            for (tier, prices) in overrides {
                if tier == ServiceTier::default_tier() {
                    // The standard price is what a request naming no tier
                    // pays; a card cannot price the named default apart.
                    let differs = prices
                        .iter()
                        .any(|(f, v)| standard.get(f).map(|s| s.get()) != Some(v.get()));
                    if differs {
                        flags.insert(Flag::UnsupportedVariant);
                    }
                    continue;
                }
                let mut resolved = standard.clone();
                resolved.extend(prices);
                tiers.insert(tier, resolved);
            }
            let card = card(&tiers, &mut flags);
            (
                from.map(|d| d.and_time(NaiveTime::MIN).and_utc()),
                card,
                flags.into_iter().collect(),
            )
        })
        .collect()
}

/// The tier a variant's condition selects, or `None` when a card cannot
/// express it.
fn variant_tier(when: &BTreeMap<String, String>) -> Option<ServiceTier> {
    let mut conditions = when.iter();
    match (conditions.next(), conditions.next()) {
        (Some((key, value)), None) if key == "processing" && value == "batch" => {
            Some(ServiceTier::batch())
        }
        (Some((key, name)), None) if key == "service_tier" => tier_named(name),
        _ => None,
    }
}

/// `name` as a service tier, when the rate model accepts it as one: checked
/// by [`RateCard::validate`] itself, so the rule is the engine's and never a
/// copy of it.
fn tier_named(name: &str) -> Option<ServiceTier> {
    let tier: ServiceTier = serde_json::from_value(Value::String(name.to_owned())).ok()?;
    let flat = |r| BTreeMap::from([(Dimension::Input, r), (Dimension::Output, r)]);
    let rates = TierRates {
        base: rates(&flat(Rate(0)))?,
        long_context: Vec::new(),
    };
    let probe = RateCard(BTreeMap::from([
        (ServiceTier::default_tier(), rates.clone()),
        (tier.clone(), rates),
    ]));
    probe.validate().ok().map(|()| tier)
}

/// The card dimension a dataset price field fills, and what its number is
/// multiplied by to be a price per million units.
fn dimension(field: &str) -> Option<(Dimension, u64)> {
    Some(match field {
        "input_mtok" => (Dimension::Input, 1),
        "output_mtok" => (Dimension::Output, 1),
        "cache_read_mtok" => (Dimension::CacheRead, 1),
        "cache_write_mtok" => (Dimension::CacheWrite, 1),
        "cache_write_1h_mtok" => (Dimension::CacheWrite1h, 1),
        "input_audio_mtok" => (Dimension::AudioInput, 1),
        "output_audio_mtok" => (Dimension::AudioOutput, 1),
        "web_searches_kcount" => (Dimension::WebSearch, 1_000),
        _ => return None,
    })
}

/// One price field's value.
enum Price {
    Flat(Rate),
    /// The base, and each tier's start and price in rising order of start.
    Tiered(Rate, Vec<(u64, Rate)>),
}

impl Price {
    /// The value's text, multiplied by `per`. `None` for anything that is not
    /// a number [`Rate::parse`] reads (digits, then at most six decimals) or
    /// a tiered price of such numbers with whole-number starts.
    fn read(value: &RawValue, per: u64) -> Option<Price> {
        #[derive(Deserialize)]
        struct Tiered<'a> {
            #[serde(borrow)]
            base: &'a RawValue,
            #[serde(borrow)]
            tiers: Vec<Tier<'a>>,
        }
        #[derive(Deserialize)]
        struct Tier<'a> {
            #[serde(borrow)]
            start: &'a RawValue,
            #[serde(borrow)]
            price: &'a RawValue,
        }
        let text = value.get();
        if !text.starts_with('{') {
            return rate(text, per).map(Price::Flat);
        }
        let tiered: Tiered<'_> = serde_json::from_str(text).ok()?;
        let mut tiers = tiered
            .tiers
            .iter()
            .map(|t| Some((start(t.start.get())?, rate(t.price.get(), per)?)))
            .collect::<Option<Vec<_>>>()?;
        // Stable, so of two tiers with one start the later one wins, as in
        // the dataset's own resolution.
        tiers.sort_by_key(|(start, _)| *start);
        Some(Price::Tiered(rate(tiered.base.get(), per)?, tiers))
    }

    fn starts(&self) -> impl Iterator<Item = u64> + '_ {
        let tiers = match self {
            Price::Flat(_) => &[][..],
            Price::Tiered(_, tiers) => &tiers[..],
        };
        tiers.iter().map(|(start, _)| *start)
    }

    /// The price in the band above `threshold` input tokens, or the base
    /// for `None`.
    fn in_band(&self, threshold: Option<u64>) -> Rate {
        match (self, threshold) {
            (Price::Flat(rate), _) | (Price::Tiered(rate, _), None) => *rate,
            (Price::Tiered(base, tiers), Some(threshold)) => tiers
                .iter()
                .rev()
                .find(|(start, _)| *start <= threshold)
                .map_or(*base, |(_, rate)| *rate),
        }
    }
}

/// `text` as a rate, times `per`; `None` past [`Rate::MAX`].
fn rate(text: &str, per: u64) -> Option<Rate> {
    Rate::parse(text)
        .ok()?
        .0
        .checked_mul(per)
        .filter(|r| *r <= Rate::MAX.0)
        .map(Rate)
}

/// A tier's start: digits only.
fn start(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The rates of one band or base; `None` without an input or an output
/// price, which every card needs.
fn rates(prices: &BTreeMap<Dimension, Rate>) -> Option<Rates> {
    let get = |d| prices.get(&d).copied();
    Some(Rates {
        input: get(Dimension::Input)?,
        output: get(Dimension::Output)?,
        cache_read: get(Dimension::CacheRead),
        cache_write: get(Dimension::CacheWrite),
        cache_write_1h: get(Dimension::CacheWrite1h),
        reasoning: None,
        audio_input: get(Dimension::AudioInput),
        audio_output: get(Dimension::AudioOutput),
        image_input: None,
        image_output: None,
        web_search: get(Dimension::WebSearch),
    })
}

/// The card `tiers` (each tier's resolved price map) come to, adding to
/// `flags` what keeps them from one.
fn card(
    tiers: &BTreeMap<ServiceTier, PriceMap<'_>>,
    flags: &mut BTreeSet<Flag>,
) -> Option<RateCard> {
    let mut priced: BTreeMap<&ServiceTier, BTreeMap<Dimension, Price>> = BTreeMap::new();
    for (tier, fields) in tiers {
        let prices = priced.entry(tier).or_default();
        for (field, value) in fields {
            let Some((dimension, per)) = dimension(field) else {
                flags.insert(Flag::UnsupportedField);
                continue;
            };
            match Price::read(value, per) {
                Some(price) => {
                    prices.insert(dimension, price);
                }
                None => {
                    flags.insert(Flag::BadNumber);
                }
            }
        }
    }
    if flags.contains(&Flag::BadNumber) {
        return None;
    }
    let thresholds: BTreeSet<u64> = priced
        .values()
        .flat_map(|prices| prices.values().flat_map(Price::starts))
        .collect();
    let card = priced
        .into_iter()
        .map(|(tier, prices)| {
            let at = |threshold: Option<u64>| {
                rates(
                    &prices
                        .iter()
                        .map(|(d, p)| (*d, p.in_band(threshold)))
                        .collect(),
                )
            };
            let long_context = thresholds
                .iter()
                .map(|t| {
                    Some(LongContextBand {
                        above_input_tokens: *t,
                        rates: at(Some(*t))?,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some((
                tier.clone(),
                TierRates {
                    base: at(None)?,
                    long_context,
                },
            ))
        })
        .collect::<Option<BTreeMap<_, _>>>()
        .map(RateCard);
    match card {
        Some(card) if card.validate().is_ok() => Some(card),
        _ => {
            flags.insert(Flag::InvalidCard);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    /// 2026-11-08 12:00 UTC.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 8, 12, 0, 0).unwrap()
    }

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_string()).expect("valid JSON")
    }

    /// The proposals of `text`, a whole dataset, at [`now`].
    fn proposals(text: &str) -> Vec<ParsedProposal> {
        parse(&raw(text), now()).expect("a recognisable dataset")
    }

    /// One provider `p` with the models `models` (JSON text).
    fn dataset(models: &str) -> String {
        format!(
            r#"[{{"id":"p","pricing_urls":["https://p.example/pricing"],"models":[{models}]}}]"#
        )
    }

    /// The proposal of model `id` in effect at upload.
    fn current<'a>(all: &'a [ParsedProposal], id: &str) -> &'a ParsedProposal {
        all.iter()
            .find(|p| p.model == id && p.effective_from.is_none())
            .unwrap_or_else(|| panic!("no proposal for {id} in {all:?}"))
    }

    fn card_json(p: &ParsedProposal) -> Value {
        serde_json::to_value(p.card.as_ref().expect("a card")).expect("a card serializes")
    }

    fn default_base(p: &ParsedProposal) -> &aisix_pricing::Rates {
        &p.card.as_ref().expect("a card").0[&ServiceTier::default_tier()].base
    }

    #[test]
    fn a_price_is_read_from_its_decimal_text() {
        let all = proposals(&dataset(
            r#"{"id":"m-plain","prices":{"input_mtok":3,"output_mtok": 0.075 }},
               {"id":"m-exponent","prices":{"input_mtok":1.5e-05,"output_mtok":1}},
               {"id":"m-seventh","prices":{"input_mtok":0.0000001,"output_mtok":1}},
               {"id":"m-negative","prices":{"input_mtok":-1,"output_mtok":1}},
               {"id":"m-float-noise","prices":{"input_mtok":0.18000000000000002,"output_mtok":1}},
               {"id":"m-string","prices":{"input_mtok":"3","output_mtok":1}},
               {"id":"m-tier-start","prices":{"input_mtok":{"base":1,"tiers":[{"start":2e5,"price":2}]},"output_mtok":1}}"#,
        ));
        let plain = current(&all, "m-plain");
        assert_eq!(default_base(plain).input, Rate(3_000_000));
        assert_eq!(default_base(plain).output, Rate(75_000));
        assert!(plain.flags.is_empty(), "{plain:?}");
        for id in [
            "m-exponent",
            "m-seventh",
            "m-negative",
            "m-float-noise",
            "m-string",
            "m-tier-start",
        ] {
            let p = current(&all, id);
            assert_eq!(p.card, None, "{id}: {p:?}");
            assert_eq!(p.flags, vec![Flag::BadNumber], "{id}");
        }
    }

    #[test]
    fn dataset_fields_map_to_card_dimensions() {
        let all = proposals(&dataset(
            r#"{"id":"m-all","prices":{"input_mtok":1,"output_mtok":2,"cache_read_mtok":0.1,
                 "cache_write_mtok":1.25,"cache_write_1h_mtok":2.5,"input_audio_mtok":3,
                 "output_audio_mtok":4,"web_searches_kcount":10}},
               {"id":"m-requests","prices":{"input_mtok":1,"output_mtok":2,"requests_kcount":5}}"#,
        ));
        assert_eq!(
            card_json(current(&all, "m-all")),
            json!({"default": {"base": {
                "input": "1", "output": "2", "cache_read": "0.1", "cache_write": "1.25",
                "cache_write_1h": "2.5", "audio_input": "3", "audio_output": "4",
                "web_search": "10000",
            }}})
        );
        assert!(current(&all, "m-all").flags.is_empty());
        let requests = current(&all, "m-requests");
        assert_eq!(requests.flags, vec![Flag::UnsupportedField]);
        // What does map is still shown, for the comparison.
        assert_eq!(
            card_json(requests),
            json!({"default": {"base": {"input": "1", "output": "2"}}})
        );
    }

    #[test]
    fn web_searches_priced_per_thousand_are_priced_per_million() {
        let all = proposals(&dataset(
            r#"{"id":"m-ten","prices":{"input_mtok":1,"output_mtok":2,"web_searches_kcount":10}},
               {"id":"m-small","prices":{"input_mtok":1,"output_mtok":2,"web_searches_kcount":0.000001}},
               {"id":"m-seventh","prices":{"input_mtok":1,"output_mtok":2,"web_searches_kcount":0.0000001}}"#,
        ));
        assert_eq!(
            default_base(current(&all, "m-ten")).web_search,
            Some(Rate::parse("10000").unwrap())
        );
        assert_eq!(
            default_base(current(&all, "m-small")).web_search,
            Some(Rate::parse("0.001").unwrap())
        );
        let seventh = current(&all, "m-seventh");
        assert_eq!(
            (seventh.card.as_ref(), &seventh.flags[..]),
            (None, &[Flag::BadNumber][..])
        );
    }

    #[test]
    fn tiers_become_long_context_bands_with_every_rate_the_base_carries() {
        let all = proposals(&dataset(
            r#"{"id":"m","prices":{
                 "input_mtok":{"base":3,"tiers":[{"start":200000,"price":6}]},
                 "output_mtok":{"base":15,"tiers":[{"start":200000,"price":22.5}]},
                 "cache_read_mtok":0.3}}"#,
        ));
        let p = current(&all, "m");
        assert!(p.flags.is_empty(), "{p:?}");
        assert_eq!(
            card_json(p),
            json!({"default": {
                "base": {"input": "3", "output": "15", "cache_read": "0.3"},
                "long_context": [{"above_input_tokens": 200000,
                                  "rates": {"input": "6", "output": "22.5", "cache_read": "0.3"}}],
            }})
        );
        p.card
            .as_ref()
            .unwrap()
            .validate()
            .expect("the card validates");
    }

    #[test]
    fn fields_tiered_at_different_starts_give_one_band_per_start() {
        let all = proposals(&dataset(
            r#"{"id":"m","prices":{
                 "input_mtok":{"base":1,"tiers":[{"start":128000,"price":2}]},
                 "output_mtok":{"base":4,"tiers":[{"start":200000,"price":9}]}}}"#,
        ));
        let p = current(&all, "m");
        assert!(p.flags.is_empty(), "{p:?}");
        assert_eq!(
            card_json(p),
            json!({"default": {
                "base": {"input": "1", "output": "4"},
                "long_context": [
                    {"above_input_tokens": 128000, "rates": {"input": "2", "output": "4"}},
                    {"above_input_tokens": 200000, "rates": {"input": "2", "output": "9"}},
                ],
            }})
        );
    }

    #[test]
    fn a_batch_variant_becomes_the_batch_tier_and_a_service_tier_variant_its_tier() {
        let all = proposals(&dataset(
            r#"{"id":"m","prices":{"input_mtok":4,"output_mtok":20,"web_searches_kcount":10},
                "price_variants":[
                  {"when":{"service_tier":"default"},"prices":{}},
                  {"when":{"processing":"batch"},"prices":{"input_mtok":2,"output_mtok":10}},
                  {"when":{"service_tier":"flex"},"prices":{"input_mtok":1,"output_mtok":5}}]},
               {"id":"m-alias","prices":{"input_mtok":4,"output_mtok":20},
                "price_variants":[{"when":{"service_tier":"standard"},"prices":{"input_mtok":3}}]},
               {"id":"m-default-differs","prices":{"input_mtok":4,"output_mtok":20},
                "price_variants":[{"when":{"service_tier":"default"},"prices":{"input_mtok":3}}]}"#,
        ));
        let p = current(&all, "m");
        assert!(p.flags.is_empty(), "{p:?}");
        // A variant overrides the prices it names; the rest are the
        // standard prices, as the dataset resolves a variant.
        assert_eq!(
            card_json(p),
            json!({
                "default": {"base": {"input": "4", "output": "20", "web_search": "10000"}},
                "batch": {"base": {"input": "2", "output": "10", "web_search": "10000"}},
                "flex": {"base": {"input": "1", "output": "5", "web_search": "10000"}},
            })
        );
        // `standard` is no tier name a served tier selects.
        let alias = current(&all, "m-alias");
        assert_eq!(alias.flags, vec![Flag::UnsupportedVariant]);
        assert_eq!(
            card_json(alias),
            json!({"default": {"base": {"input": "4", "output": "20"}}})
        );
        // A named default tier priced apart from requests that name none.
        let differs = current(&all, "m-default-differs");
        assert_eq!(differs.flags, vec![Flag::UnsupportedVariant]);
        assert_eq!(
            card_json(differs),
            json!({"default": {"base": {"input": "4", "output": "20"}}})
        );
    }

    #[test]
    fn other_conditions_and_time_windows_are_flagged() {
        let all = proposals(&dataset(
            r#"{"id":"m-region","prices":{"input_mtok":1,"output_mtok":2},
                "price_variants":[{"when":{"region":"eu"},"prices":{"input_mtok":3}}]},
               {"id":"m-two-keys","prices":{"input_mtok":1,"output_mtok":2},
                "price_variants":[{"when":{"processing":"batch","service_tier":"flex"},"prices":{"input_mtok":3}}]},
               {"id":"m-window","prices":[
                 {"prices":{"input_mtok":1,"output_mtok":2}},
                 {"constraint":{"start_time":"00:30:00Z","end_time":"16:30:00Z"},
                  "prices":{"input_mtok":2,"output_mtok":4}}]},
               {"id":"m-window-variant","prices":{"input_mtok":1,"output_mtok":2},
                "price_variants":[{"when":{"processing":"batch"},"prices":{"input_mtok":0.5},
                  "constraint":{"end_time":"08:00:00Z","start_time":"01:00:00Z"}}]}"#,
        ));
        for id in ["m-region", "m-two-keys"] {
            let p = current(&all, id);
            assert_eq!(p.flags, vec![Flag::UnsupportedVariant], "{id}");
            assert_eq!(
                card_json(p),
                json!({"default": {"base": {"input": "1", "output": "2"}}}),
                "{id}"
            );
        }
        for id in ["m-window", "m-window-variant"] {
            let p = current(&all, id);
            assert_eq!(p.flags, vec![Flag::TimeOfDay], "{id}");
            assert_eq!(
                card_json(p),
                json!({"default": {"base": {"input": "1", "output": "2"}}}),
                "{id}"
            );
        }
    }

    #[test]
    fn dated_prices_give_one_proposal_per_date_from_now_on() {
        let all = proposals(&dataset(
            r#"{"id":"m-list","prices":[
                 {"prices":{"input_mtok":1,"output_mtok":2}},
                 {"constraint":{"start_date":"2026-01-01"},"prices":{"input_mtok":3,"output_mtok":4}},
                 {"constraint":{"start_date":"2027-01-01"},"prices":{"input_mtok":5,"output_mtok":6}}]},
               {"id":"m-variants","prices":{"input_mtok":4,"output_mtok":20},
                "price_variants":[
                  {"when":{"processing":"batch"},"prices":{"input_mtok":1},
                   "constraint":{"start_date":"2027-03-01"}},
                  {"when":{"processing":"batch"},"prices":{"input_mtok":2,"output_mtok":10},
                   "constraint":{"start_date":"2026-10-14"}}]},
               {"id":"m-today","prices":[
                 {"prices":{"input_mtok":1,"output_mtok":2}},
                 {"constraint":{"start_date":"2026-11-08"},"prices":{"input_mtok":7,"output_mtok":8}}]}"#,
        ));
        let dates = |id: &str| -> Vec<Option<DateTime<Utc>>> {
            all.iter()
                .filter(|p| p.model == id)
                .map(|p| p.effective_from)
                .collect()
        };
        let midnight = |y, m, d| Some(Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap());

        assert_eq!(dates("m-list"), vec![None, midnight(2027, 1, 1)]);
        let list: Vec<&ParsedProposal> = all.iter().filter(|p| p.model == "m-list").collect();
        assert_eq!(
            card_json(list[0]),
            json!({"default": {"base": {"input": "3", "output": "4"}}})
        );
        assert_eq!(
            card_json(list[1]),
            json!({"default": {"base": {"input": "5", "output": "6"}}})
        );

        // A dated variant takes effect at its own date; until then the one
        // already in effect stands.
        assert_eq!(dates("m-variants"), vec![None, midnight(2027, 3, 1)]);
        let variants: Vec<&ParsedProposal> =
            all.iter().filter(|p| p.model == "m-variants").collect();
        assert_eq!(
            card_json(variants[0]),
            json!({
                "default": {"base": {"input": "4", "output": "20"}},
                "batch": {"base": {"input": "2", "output": "10"}},
            })
        );
        assert_eq!(
            card_json(variants[1]),
            json!({
                "default": {"base": {"input": "4", "output": "20"}},
                "batch": {"base": {"input": "1", "output": "10"}},
            })
        );

        // A date that is today is in effect at upload.
        assert_eq!(dates("m-today"), vec![None]);
        assert_eq!(
            card_json(current(&all, "m-today")),
            json!({"default": {"base": {"input": "7", "output": "8"}}})
        );
    }

    #[test]
    fn a_card_the_rate_model_refuses_is_kept_flagged_without_a_card() {
        let all = proposals(&dataset(
            r#"{"id":"m-input-only","prices":{"input_mtok":0.02}},
               {"id":"m-zero-start","prices":{"input_mtok":{"base":1,"tiers":[{"start":0,"price":2}]},"output_mtok":1}}"#,
        ));
        for id in ["m-input-only", "m-zero-start"] {
            let p = current(&all, id);
            assert_eq!(
                (p.card.as_ref(), &p.flags[..]),
                (None, &[Flag::InvalidCard][..]),
                "{id}"
            );
        }
    }

    #[test]
    fn provenance_is_the_providers_first_page_and_the_models_checked_date() {
        let all = proposals(
            r#"[{"id":"p","pricing_urls":["https://p.example/a","https://p.example/b"],"models":[
                  {"id":"m-checked","prices_checked":"2026-10-19","prices":{"input_mtok":1,"output_mtok":2}},
                  {"id":"m-unchecked","prices":{"input_mtok":1,"output_mtok":2}}]},
                {"id":"q","models":[{"id":"m-uncited","prices":{"input_mtok":1,"output_mtok":2}}]}]"#,
        );
        let checked = current(&all, "m-checked");
        assert_eq!(checked.provider, "p");
        assert_eq!(checked.pricing_url.as_deref(), Some("https://p.example/a"));
        assert_eq!(
            checked.prices_checked,
            Some(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap())
        );
        assert_eq!(current(&all, "m-unchecked").prices_checked, None);
        let uncited = current(&all, "m-uncited");
        assert_eq!(
            (uncited.provider.as_str(), uncited.pricing_url.as_deref()),
            ("q", None)
        );
    }

    #[test]
    fn a_body_that_is_not_a_dataset_is_refused_at_data() {
        let too_many = dataset(
            &(0..=MAX_IMPORT_PROPOSALS)
                .map(|i| format!(r#"{{"id":"m-{i}","prices":{{"input_mtok":1,"output_mtok":2}}}}"#))
                .collect::<Vec<_>>()
                .join(","),
        );
        for text in [
            r#"{"a":1}"#,
            "[]",
            "[1]",
            r#"[{"id":"p"}]"#,
            r#"[{"id":"p","models":[{"id":"m","prices":null}]}]"#,
            too_many.as_str(),
        ] {
            let err = parse(&raw(text), now()).expect_err("refused");
            assert_eq!(err.status.as_u16(), 422, "{err:?}");
            assert_eq!(
                err.details
                    .iter()
                    .map(|d| d.path.as_str())
                    .collect::<Vec<_>>(),
                vec!["/data"],
                "{err:?}"
            );
        }
        // Exactly the limit is accepted.
        let at_limit = dataset(
            &(0..MAX_IMPORT_PROPOSALS)
                .map(|i| format!(r#"{{"id":"m-{i}","prices":{{"input_mtok":1,"output_mtok":2}}}}"#))
                .collect::<Vec<_>>()
                .join(","),
        );
        assert_eq!(proposals(&at_limit).len(), MAX_IMPORT_PROPOSALS);
    }
}
