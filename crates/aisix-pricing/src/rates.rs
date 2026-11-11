//! The rate model: what one unit of each metered dimension costs, per
//! service tier and long-context band.
//!
//! Every amount is an integer. A rate is micro-USD per million units, so
//! `quantity × rate / 1_000_000` is a charge in micro-USD that stays exact
//! until the engine rounds the sum once.
//!
//! A card is written so that a rate it lacks can never be filled silently
//! from a different, possibly more expensive one: fallbacks stay within one
//! rate set (`Rates::rate_for`), and `RateCard::validate` makes every tier
//! and every band carry the rates its reference carries.

use std::collections::btree_map::{BTreeMap, Entry};
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// Micro-USD in one USD, and the number of units a rate is quoted per.
const MILLION: u64 = 1_000_000;

/// A price per one million metered units, in micro-USD (equivalently,
/// pico-USD per unit).
///
/// Read and written as a decimal string of USD per million units with at
/// most 6 decimals, e.g. `"2.5"`. That is the unit list prices are published
/// in, and a string keeps it exact where a JSON number would arrive as a
/// binary float.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rate(pub u64);

impl Rate {
    /// USD 1,000,000 per million units: far above any list price, and low
    /// enough that a charge's numerator stays exact in `u128` (asserted below).
    pub const MAX: Rate = Rate(1_000_000 * MILLION);

    /// Reads ASCII digits, then optionally `.` and 1–6 more digits. Anything
    /// else is `BadRate`; a value above `Rate::MAX` is `RateTooLarge`.
    pub fn parse(s: &str) -> Result<Rate, RateCardError> {
        let bad = || RateCardError::BadRate(s.to_owned());
        let (whole, frac) = match s.split_once('.') {
            Some((whole, frac)) if (1..=6).contains(&frac.len()) => (whole, frac),
            Some(_) => return Err(bad()),
            None => (s, ""),
        };
        let all_digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty() || !all_digits(whole) || !all_digits(frac) {
            return Err(bad());
        }
        let digit = |b: u8| u64::from(b - b'0');
        let whole = whole
            .bytes()
            .try_fold(0u64, |n, b| n.checked_mul(10)?.checked_add(digit(b)))
            .ok_or(RateCardError::RateTooLarge)?;
        // At most six digits, padded to six: below 10^6, so no overflow.
        let frac = frac
            .bytes()
            .chain(std::iter::repeat(b'0'))
            .take(6)
            .fold(0u64, |n, b| n * 10 + digit(b));
        whole
            .checked_mul(MILLION)
            .and_then(|w| w.checked_add(frac))
            .filter(|v| *v <= Rate::MAX.0)
            .map(Rate)
            .ok_or(RateCardError::RateTooLarge)
    }
}

// The engine computes a charge as Σ quantity × rate × (10_000 − discount bps)
// in u128, over at most one line per dimension. At the cap, that sum cannot
// overflow even with every quantity at u64::MAX.
const _: () = assert!(
    Dimension::ALL.len() as u128 * Rate::MAX.0 as u128 * 10_000 <= u128::MAX / u64::MAX as u128
);

/// The shortest exact form: no trailing zeros, no `.` for a whole amount.
impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, frac) = (self.0 / MILLION, self.0 % MILLION);
        if frac == 0 {
            return write!(f, "{whole}");
        }
        let frac = format!("{frac:06}");
        write!(f, "{whole}.{}", frac.trim_end_matches('0'))
    }
}

impl Serialize for Rate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Rate, D::Error> {
        Rate::parse(&String::deserialize(d)?).map_err(de::Error::custom)
    }
}

/// CNY per USD, in ten-thousandths: `72_000` is 7.2 CNY for each USD of a
/// model's baseline price. A rate of 0 makes a model free.
///
/// Read and written as a decimal string of 1–3 digits and at most 4
/// decimals, from `"0"` to `"100"`, e.g. `"7.2"`; a JSON number is refused,
/// as it is for [`Rate`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CnyPerUsd(pub u32);

impl CnyPerUsd {
    /// One CNY per USD: the charge in µ¥ is the baseline's in µ$.
    pub const ONE: CnyPerUsd = CnyPerUsd(10_000);
    /// One hundred CNY per USD.
    pub const MAX: CnyPerUsd = CnyPerUsd(1_000_000);

    /// Reads `^\d{1,3}(\.\d{1,4})?$` in ASCII digits, up to `CnyPerUsd::MAX`.
    /// Anything else is `BadCnyPerUsd`.
    pub fn parse(s: &str) -> Result<CnyPerUsd, RateCardError> {
        let bad = || RateCardError::BadCnyPerUsd(s.to_owned());
        let (whole, frac) = match s.split_once('.') {
            Some((whole, frac)) if (1..=4).contains(&frac.len()) => (whole, frac),
            Some(_) => return Err(bad()),
            None => (s, ""),
        };
        let all_digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
        if !(1..=3).contains(&whole.len()) || !all_digits(whole) || !all_digits(frac) {
            return Err(bad());
        }
        // Three digits, then the decimals padded to four: below 10^7.
        let e4 = whole
            .bytes()
            .chain(frac.bytes().chain(std::iter::repeat(b'0')).take(4))
            .fold(0u32, |n, b| n * 10 + u32::from(b - b'0'));
        if e4 > CnyPerUsd::MAX.0 {
            return Err(bad());
        }
        Ok(CnyPerUsd(e4))
    }
}

/// The shortest exact form: `"7.2"`, `"100"`.
impl fmt::Display for CnyPerUsd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&exact_decimal(u128::from(self.0), 4))
    }
}

impl Serialize for CnyPerUsd {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CnyPerUsd {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<CnyPerUsd, D::Error> {
        CnyPerUsd::parse(&String::deserialize(d)?).map_err(de::Error::custom)
    }
}

/// An amount of money in micro-CNY, millionths of a yuan. A charge is never
/// negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize)]
#[serde(transparent)]
pub struct MicroCny(pub i64);

/// The most one dimension of one row bills. The three input buckets add up
/// to the prompt plus the cache reads and writes, three `u32` counters
/// (`usage.rs`, `quantities`), and no other quantity is larger.
pub(crate) const MAX_QUANTITY: u128 = 3 * u32::MAX as u128;

// Ruling R1: the CNY charge is Σ quantity × rate × CNY rate × (10_000 − bps)
// in u128, over at most one line per dimension. With every quantity at
// MAX_QUANTITY and every factor at its cap, that sum stays inside u128, so a
// row's only overflow is an amount past i64.
const _: () = assert!(
    Dimension::ALL.len() as u128 * Rate::MAX.0 as u128 * CnyPerUsd::MAX.0 as u128 * 10_000
        <= u128::MAX / MAX_QUANTITY
);

/// What one million units cost at `rate`, in CNY, after the CNY rate and a
/// discount of `discount_bps`: `rate × cny × (10000 − bps) / 10¹⁴`, exact,
/// in its shortest form. `None` for a discount above 10000 bps.
pub fn cny_per_million(rate: Rate, cny: CnyPerUsd, discount_bps: u16) -> Option<String> {
    Some(exact_decimal(listed(rate, cny, discount_bps)?, 14))
}

/// What one unit costs at `rate`, in CNY: [`cny_per_million`] over another
/// million, `/ 10²⁰`.
pub fn cny_per_token(rate: Rate, cny: CnyPerUsd, discount_bps: u16) -> Option<String> {
    Some(exact_decimal(listed(rate, cny, discount_bps)?, 20))
}

/// `rate × cny × (10000 − bps)`, or `None` for a discount above 10000 bps.
/// A reachable rate is at most `Rate::MAX` × `CnyPerUsd::MAX` × 10⁴ = 10²²,
/// below 2⁷⁴, so it is exact for any rate this engine admits.
fn listed(rate: Rate, cny: CnyPerUsd, discount_bps: u16) -> Option<u128> {
    let kept_bps = 10_000u128.checked_sub(u128::from(discount_bps))?;
    Some(u128::from(rate.0) * u128::from(cny.0) * kept_bps)
}

/// `n / 10^scale`, exact and shortest: no trailing zeros, and no `.` for a
/// whole number.
fn exact_decimal(n: u128, scale: u32) -> String {
    let unit = 10u128.pow(scale);
    let (whole, frac) = (n / unit, n % unit);
    if frac == 0 {
        return whole.to_string();
    }
    let frac = format!("{frac:0width$}", width = scale as usize);
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// A metered quantity a rate card can price. Each name is also the name of
/// its field in [`Rates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    /// Prompt tokens neither read from nor written to the provider's cache.
    Input,
    /// Prompt tokens read from the provider's prompt cache.
    CacheRead,
    /// Prompt tokens written into the provider's prompt cache; the
    /// five-minute write where the provider prices durations apart
    /// (<https://platform.claude.com/docs/en/about-claude/pricing>: a
    /// five-minute write is 1.25x the base input price, a one-hour write 2x).
    CacheWrite,
    /// Prompt tokens written into the provider's prompt cache for one hour.
    // `snake_case` alone would spell it `cache_write1h`.
    #[serde(rename = "cache_write_1h")]
    CacheWrite1h,
    /// Completion tokens.
    Output,
    /// Reasoning (thinking) tokens.
    Reasoning,
    /// Prompt audio tokens.
    AudioInput,
    /// Completion audio tokens.
    AudioOutput,
    /// Prompt image tokens.
    ImageInput,
    /// Completion image tokens.
    ImageOutput,
    /// Web search requests: a rate per million searches, as providers price
    /// search per call rather than per token.
    WebSearch,
}

impl Dimension {
    pub(crate) const ALL: [Dimension; 11] = [
        Dimension::Input,
        Dimension::CacheRead,
        Dimension::CacheWrite,
        Dimension::CacheWrite1h,
        Dimension::Output,
        Dimension::Reasoning,
        Dimension::AudioInput,
        Dimension::AudioOutput,
        Dimension::ImageInput,
        Dimension::ImageOutput,
        Dimension::WebSearch,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Dimension::Input => "input",
            Dimension::CacheRead => "cache_read",
            Dimension::CacheWrite => "cache_write",
            Dimension::CacheWrite1h => "cache_write_1h",
            Dimension::Output => "output",
            Dimension::Reasoning => "reasoning",
            Dimension::AudioInput => "audio_input",
            Dimension::AudioOutput => "audio_output",
            Dimension::ImageInput => "image_input",
            Dimension::ImageOutput => "image_output",
            Dimension::WebSearch => "web_search",
        }
    }
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One set of rates: the base of a tier, or one of its long-context bands.
/// `input` and `output` are required; every other rate is optional, and an
/// absent one falls back as [`Rates::rate_for`] says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rates {
    pub input: Rate,
    pub output: Rate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<Rate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<Rate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<Rate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Rate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_input: Option<Rate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_output: Option<Rate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_input: Option<Rate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_output: Option<Rate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<Rate>,
}

impl Rates {
    /// The rate this set itself carries for `d`, with no fallback.
    pub(crate) fn explicit(&self, d: Dimension) -> Option<Rate> {
        match d {
            Dimension::Input => Some(self.input),
            Dimension::Output => Some(self.output),
            Dimension::CacheRead => self.cache_read,
            Dimension::CacheWrite => self.cache_write,
            Dimension::CacheWrite1h => self.cache_write_1h,
            Dimension::Reasoning => self.reasoning,
            Dimension::AudioInput => self.audio_input,
            Dimension::AudioOutput => self.audio_output,
            Dimension::ImageInput => self.image_input,
            Dimension::ImageOutput => self.image_output,
            Dimension::WebSearch => self.web_search,
        }
    }

    /// The rate `d` is billed at, falling back within this set only: cache
    /// reads and writes, audio and image input to `input`; the one-hour write
    /// to `cache_write`, then `input`; reasoning, audio and image output to
    /// `output`. Web search has no fallback.
    ///
    /// These are the gateway's own pricing rules (`aisix_core::ModelCost`):
    /// a cache read or write without a rate of its own bills at the input
    /// rate, a one-hour write without one at the write rate, and with no rate
    /// for reasoning, audio or image tokens those bill at the input or output
    /// rate of the side they are counted on.
    pub(crate) fn rate_for(&self, d: Dimension) -> Option<Rate> {
        self.rate_with_source(d).map(|(rate, _)| rate)
    }

    // 来源与最终价格由同一次回退解析产生，报价不复制计费规则。
    pub(crate) fn rate_with_source(&self, d: Dimension) -> Option<(Rate, Option<Dimension>)> {
        if let Some(rate) = self.explicit(d) {
            return Some((rate, None));
        }
        let source = match d {
            Dimension::CacheRead
            | Dimension::CacheWrite
            | Dimension::AudioInput
            | Dimension::ImageInput => Dimension::Input,
            Dimension::CacheWrite1h if self.cache_write.is_some() => Dimension::CacheWrite,
            Dimension::CacheWrite1h => Dimension::Input,
            Dimension::Reasoning | Dimension::AudioOutput | Dimension::ImageOutput => {
                Dimension::Output
            }
            Dimension::Input | Dimension::Output | Dimension::WebSearch => return None,
        };
        self.explicit(source).map(|rate| (rate, Some(source)))
    }

    /// The first optional rate `reference` carries that this set lacks.
    fn first_missing_from(&self, reference: &Rates) -> Option<Dimension> {
        Dimension::ALL
            .into_iter()
            .find(|d| reference.explicit(*d).is_some() && self.explicit(*d).is_none())
    }

    fn check_cap(&self) -> Result<(), RateCardError> {
        let over = Dimension::ALL
            .into_iter()
            .filter_map(|d| self.explicit(d))
            .any(|r| r > Rate::MAX);
        if over {
            return Err(RateCardError::RateTooLarge);
        }
        Ok(())
    }

    fn try_map(
        &self,
        f: impl Fn(Rate) -> Result<Rate, RateCardError>,
    ) -> Result<Rates, RateCardError> {
        let opt = |r: Option<Rate>| r.map(&f).transpose();
        Ok(Rates {
            input: f(self.input)?,
            output: f(self.output)?,
            cache_read: opt(self.cache_read)?,
            cache_write: opt(self.cache_write)?,
            cache_write_1h: opt(self.cache_write_1h)?,
            reasoning: opt(self.reasoning)?,
            audio_input: opt(self.audio_input)?,
            audio_output: opt(self.audio_output)?,
            image_input: opt(self.image_input)?,
            image_output: opt(self.image_output)?,
            web_search: opt(self.web_search)?,
        })
    }
}

/// Rates that replace a tier's base for every dimension once a request's
/// prompt is strictly longer than `above_input_tokens`. Providers word it the
/// same way: "prompts > 200k tokens"
/// (<https://aistudio.google.com/docs/pricing>), "over 100,000 tokens",
/// counting cache reads and writes in the prompt
/// (<https://platform.claude.com/docs/en/about-claude/pricing>).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongContextBand {
    pub above_input_tokens: u64,
    pub rates: Rates,
}

/// One service tier's rates: its base, and its long-context bands in rising
/// order of threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierRates {
    pub base: Rates,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub long_context: Vec<LongContextBand>,
}

impl TierRates {
    fn thresholds(&self) -> impl Iterator<Item = u64> + '_ {
        self.long_context.iter().map(|b| b.above_input_tokens)
    }

    fn try_map(
        &self,
        f: impl Fn(Rate) -> Result<Rate, RateCardError> + Copy,
    ) -> Result<TierRates, RateCardError> {
        Ok(TierRates {
            base: self.base.try_map(f)?,
            long_context: self
                .long_context
                .iter()
                .map(|b| {
                    Ok(LongContextBand {
                        above_input_tokens: b.above_input_tokens,
                        rates: b.rates.try_map(f)?,
                    })
                })
                .collect::<Result<_, RateCardError>>()?,
        })
    }
}

/// A model's rates by service tier. A card must carry a `default` tier;
/// every other tier is explicit, so a tier the card does not name is never
/// billed at another tier's rates. Providers publish their batch and
/// priority prices as tables of their own, with the same long-context split
/// as the standard table (<https://aistudio.google.com/docs/pricing>,
/// <https://developers.openai.com/api/docs/pricing>).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RateCard(pub BTreeMap<ServiceTier, TierRates>);

/// The rates a request is billed at, and the threshold of the band that
/// supplied them (`None` for the tier's base).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selected<'a> {
    pub(crate) band_above: Option<u64>,
    pub(crate) rates: &'a Rates,
}

impl RateCard {
    /// Checks every rule a card is written under, beyond each rate's format:
    ///
    /// - every rate is at most `Rate::MAX`, however the card was built;
    /// - the card has a `default` tier, and every tier name is one a served
    ///   tier can select: it matches `^[a-z0-9][a-z0-9_-]{0,31}$` and is
    ///   neither `standard` nor `auto`, both aliases of `default`;
    /// - in each tier, band thresholds are above 0 and strictly increasing;
    /// - every band carries every optional rate its own tier's base carries;
    /// - every other tier has bands at exactly the default tier's
    ///   thresholds; its base carries every optional rate the default base
    ///   carries, and each of its bands every optional rate the default band
    ///   at the same threshold carries.
    ///
    /// Together these make a missing rate impossible to fill silently from a
    /// more expensive one: a band never falls back to its base, and a tier
    /// never to `default`.
    pub fn validate(&self) -> Result<(), RateCardError> {
        let default_tier = ServiceTier::default_tier();
        let default = self
            .0
            .get(&default_tier)
            .ok_or(RateCardError::NoDefaultTier)?;
        for (tier, rates) in &self.0 {
            // A name `normalize` does not give back unchanged can never be
            // selected, so its prices would be silently ignored.
            if ServiceTier::normalize(Some(tier.as_str()), false).as_ref() != Some(tier) {
                return Err(RateCardError::BadTierName(tier.0.clone()));
            }
            rates.base.check_cap()?;
            let mut below = 0;
            for band in &rates.long_context {
                band.rates.check_cap()?;
                if band.above_input_tokens == 0 {
                    return Err(RateCardError::ZeroBandThreshold);
                }
                if band.above_input_tokens <= below {
                    return Err(RateCardError::BandsNotRising);
                }
                below = band.above_input_tokens;
                if let Some(dimension) = band.rates.first_missing_from(&rates.base) {
                    return Err(RateCardError::BandMissingRate {
                        tier: tier.clone(),
                        above: band.above_input_tokens,
                        dimension,
                    });
                }
            }
            if *tier == default_tier {
                continue;
            }
            if !rates.thresholds().eq(default.thresholds()) {
                return Err(RateCardError::BandsDifferFromDefault { tier: tier.clone() });
            }
            // Same thresholds, so the bands pair up by position.
            for (band, default_band) in rates.long_context.iter().zip(&default.long_context) {
                if let Some(dimension) = band.rates.first_missing_from(&default_band.rates) {
                    return Err(RateCardError::BandMissingRate {
                        tier: tier.clone(),
                        above: band.above_input_tokens,
                        dimension,
                    });
                }
            }
            if let Some(dimension) = rates.base.first_missing_from(&default.base) {
                return Err(RateCardError::TierMissingRate {
                    tier: tier.clone(),
                    dimension,
                });
            }
        }
        Ok(())
    }

    /// The same card with every rate raised by `markup_bps` basis points,
    /// each rounded half up to a whole micro-USD per million units. Refuses,
    /// rather than caps, a result above `Rate::MAX`.
    pub fn with_markup(&self, markup_bps: u32) -> Result<RateCard, RateCardError> {
        let factor = 10_000 + u128::from(markup_bps);
        // u64 × (10_000 + u32::MAX) is far inside u128.
        let mark = |r: Rate| {
            u64::try_from((u128::from(r.0) * factor + 5_000) / 10_000)
                .ok()
                .filter(|v| *v <= Rate::MAX.0)
                .map(Rate)
                .ok_or(RateCardError::RateTooLarge)
        };
        self.0
            .iter()
            .map(|(tier, rates)| Ok((tier.clone(), rates.try_map(mark)?)))
            .collect::<Result<_, RateCardError>>()
            .map(RateCard)
    }

    /// The rates `tier` bills a request with `input_total` prompt tokens at:
    /// the last band whose threshold the prompt is strictly above, else the
    /// tier's base. `None` when the card has no such tier.
    pub(crate) fn select(&self, tier: &ServiceTier, input_total: u64) -> Option<Selected<'_>> {
        let rates = self.0.get(tier)?;
        Some(
            match rates
                .long_context
                .iter()
                .rev()
                .find(|b| input_total > b.above_input_tokens)
            {
                Some(band) => Selected {
                    band_above: Some(band.above_input_tokens),
                    rates: &band.rates,
                },
                None => Selected {
                    band_above: None,
                    rates: &rates.base,
                },
            },
        )
    }
}

/// A tier named twice is refused, the way a rate named twice in [`Rates`]
/// is, instead of the later one silently replacing the earlier.
impl<'de> Deserialize<'de> for RateCard {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<RateCard, D::Error> {
        struct CardVisitor;
        impl<'de> Visitor<'de> for CardVisitor {
            type Value = RateCard;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map from service tier to its rates")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RateCard, A::Error> {
                let mut tiers = BTreeMap::new();
                while let Some((tier, rates)) = map.next_entry::<ServiceTier, TierRates>()? {
                    match tiers.entry(tier) {
                        Entry::Vacant(slot) => {
                            slot.insert(rates);
                        }
                        Entry::Occupied(slot) => {
                            return Err(de::Error::custom(format!(
                                "duplicate service tier `{}`",
                                slot.key()
                            )));
                        }
                    }
                }
                Ok(RateCard(tiers))
            }
        }
        d.deserialize_map(CardVisitor)
    }
}

/// A service tier's name: `^[a-z0-9][a-z0-9_-]{0,31}$`. `default` is the
/// standard tier (a served `standard` is an alias of it, never a tier of its
/// own) and `batch` the batch tier; any other name is the lowercased tier the
/// upstream reports it served.
///
/// Deserializing accepts any string, so that a card with a bad name is
/// reported by [`RateCard::validate`] as `BadTierName`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServiceTier(String);

impl ServiceTier {
    pub fn default_tier() -> Self {
        ServiceTier("default".to_owned())
    }

    pub fn batch() -> Self {
        ServiceTier("batch".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The tier a row's rates come from. A batch row is `batch` whatever its
    /// upstream reports. Otherwise the served tier is lowercased first; then
    /// absent, empty, `default`, `standard` and `auto` are `default`, any
    /// other name matching the pattern is itself, and anything else is
    /// `None`.
    ///
    /// Upstreams report the tier they served in their own words: one names
    /// its standard tier `default` (`openai-python:
    /// src/openai/types/chat/chat_completion.py`, `service_tier`), another
    /// `standard` (`anthropic-sdk-python: src/anthropic/types/usage.py`,
    /// `service_tier`); the gateway records the value verbatim (the
    /// `service_tier` field of `aisix_obs::UsageEvent`).
    ///
    /// `auto` is OpenAI's own request-side default — "the request will be
    /// processed with the service tier configured in the Project settings.
    /// Unless otherwise configured, the Project will use `default`"
    /// (`openai-python`, `chat_completion.py`, the `service_tier` field). An
    /// OpenAI-compatible upstream that echoes the request value back reports
    /// it as the served tier, and it prices at `default`, which is what that
    /// setting means; leaving it unselectable refused every such row
    /// (`missing_tier_price`) instead of billing it.
    pub(crate) fn normalize(served: Option<&str>, is_batch: bool) -> Option<ServiceTier> {
        if is_batch {
            return Some(ServiceTier::batch());
        }
        let served = served.unwrap_or_default().to_ascii_lowercase();
        match served.as_str() {
            "" | "default" | "standard" | "auto" => Some(ServiceTier::default_tier()),
            name if is_tier_name(name) => Some(ServiceTier(served)),
            _ => None,
        }
    }
}

impl fmt::Display for ServiceTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `^[a-z0-9][a-z0-9_-]{0,31}$`
fn is_tier_name(s: &str) -> bool {
    match s.as_bytes().split_first() {
        Some((first, rest)) => {
            rest.len() <= 31
                && matches!(first, b'a'..=b'z' | b'0'..=b'9')
                && rest
                    .iter()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
        }
        None => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RateCardError {
    #[error("rate {0:?} is not a USD amount per million units with at most 6 decimals")]
    BadRate(String),
    #[error("rate is above USD 1000000 per million units")]
    RateTooLarge,
    #[error("CNY per USD {0:?} is not a number from 0 to 100 with at most 4 decimals")]
    BadCnyPerUsd(String),
    #[error("rate card does not decode: {0}")]
    Malformed(String),
    #[error("rate card has no default tier")]
    NoDefaultTier,
    #[error(
        "service tier name {0:?} can never be selected: a name matches \
         ^[a-z0-9][a-z0-9_-]{{0,31}}$, and `standard` and `auto` are aliases \
         of `default`"
    )]
    BadTierName(String),
    #[error("long-context band thresholds must strictly increase")]
    BandsNotRising,
    #[error("a long-context band threshold must be above 0")]
    ZeroBandThreshold,
    #[error("tier {tier} has no {dimension} rate, which the default tier's base has")]
    TierMissingRate {
        tier: ServiceTier,
        dimension: Dimension,
    },
    #[error(
        "tier {tier}'s band above {above} input tokens has no {dimension} rate, which its base \
         or the default tier's band at that threshold has"
    )]
    BandMissingRate {
        tier: ServiceTier,
        above: u64,
        dimension: Dimension,
    },
    #[error("tier {tier}'s long-context bands are not at the default tier's thresholds")]
    BandsDifferFromDefault { tier: ServiceTier },
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn rates(json: &str) -> Rates {
        serde_json::from_str(json).unwrap()
    }

    fn card(json: &str) -> RateCard {
        serde_json::from_str(json).unwrap()
    }

    fn two_rates(input: Rate, output: Rate) -> Rates {
        Rates {
            input,
            output,
            cache_read: None,
            cache_write: None,
            cache_write_1h: None,
            reasoning: None,
            audio_input: None,
            audio_output: None,
            image_input: None,
            image_output: None,
            web_search: None,
        }
    }

    fn one_rate_card(r: Rate) -> RateCard {
        RateCard(BTreeMap::from([(
            ServiceTier::default_tier(),
            TierRates {
                base: two_rates(r, r),
                long_context: Vec::new(),
            },
        )]))
    }

    fn card_with_band(above: u64) -> RateCard {
        card(&format!(
            r#"{{"default":{{"base":{{"input":"1","output":"2"}},
                "long_context":[{{"above_input_tokens":{above},"rates":{{"input":"2","output":"3"}}}}]}}}}"#
        ))
    }

    // ---- Rate ----

    #[test]
    fn a_rate_reads_usd_per_million_exactly() {
        assert_eq!(Rate::parse("2.5").unwrap(), Rate(2_500_000));
        assert_eq!(Rate::parse("0.000001").unwrap(), Rate(1));
        assert_eq!(Rate::parse("1000000").unwrap(), Rate(Rate::MAX.0));
        assert_eq!(Rate::parse("2.50").unwrap().to_string(), "2.5");
    }

    #[test]
    fn a_rate_refuses_what_it_cannot_hold_exactly() {
        for bad in [
            "0.0000001",
            "-1",
            "1e3",
            "",
            "1,5",
            " 1",
            "1000000.000001",
            "NaN",
            ".5",
            "5.",
        ] {
            assert!(Rate::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_malformed_rate_is_named_and_an_oversized_one_is_too_large_not_wrapped() {
        for bad in ["1e3", "+1", "1.2.3", "0x10", "1_000", "1.", "١"] {
            assert_eq!(
                Rate::parse(bad),
                Err(RateCardError::BadRate(bad.to_owned())),
                "{bad}"
            );
        }
        // Above the cap after scaling, inside u64.
        assert_eq!(
            Rate::parse("1000000.000001"),
            Err(RateCardError::RateTooLarge)
        );
        assert_eq!(
            Rate::parse("18446744073709"),
            Err(RateCardError::RateTooLarge)
        );
        // Scaling by 10^6 overflows u64.
        assert_eq!(
            Rate::parse("18446744073710"),
            Err(RateCardError::RateTooLarge)
        );
        // The integer part alone overflows u64.
        assert_eq!(
            Rate::parse("18446744073709551616"),
            Err(RateCardError::RateTooLarge)
        );
        assert_eq!(
            Rate::parse(&"9".repeat(400)),
            Err(RateCardError::RateTooLarge)
        );
    }

    #[test]
    fn a_rate_prints_in_its_shortest_exact_form() {
        assert_eq!(Rate(0).to_string(), "0");
        assert_eq!(Rate(1).to_string(), "0.000001");
        assert_eq!(Rate(100_000).to_string(), "0.1");
        assert_eq!(Rate(1_000_010).to_string(), "1.00001");
        assert_eq!(Rate::MAX.to_string(), "1000000");
        assert_eq!(Rate::parse("007.250").unwrap().to_string(), "7.25");
    }

    #[test]
    fn a_rate_is_a_string_on_the_wire_never_a_number() {
        let r: Rate = serde_json::from_str(r#""0.1""#).unwrap();
        assert_eq!(r, Rate(100_000));
        assert_eq!(serde_json::to_string(&r).unwrap(), r#""0.1""#);
        // A JSON number is a binary float by the time serde sees it.
        assert!(serde_json::from_str::<Rate>("0.1").is_err());
        assert!(serde_json::from_str::<Rate>("1").is_err());
        assert!(serde_json::from_str::<Rate>(r#""1e3""#).is_err());
    }

    proptest! {
        #[test]
        fn every_rate_round_trips_through_its_text(v in 0..=Rate::MAX.0) {
            let r = Rate(v);
            prop_assert_eq!(Rate::parse(&r.to_string()), Ok(r));
            let json = serde_json::to_string(&r).unwrap();
            prop_assert_eq!(serde_json::from_str::<Rate>(&json).unwrap(), r);
        }
    }

    // ---- CnyPerUsd ----

    #[test]
    fn a_cny_per_usd_rate_has_at_most_four_decimals_up_to_one_hundred() {
        for (text, e4) in [
            ("7.2", 72_000),
            ("0", 0),
            ("100", 1_000_000),
            ("6.6667", 66_667),
            ("007.20", 72_000),
            ("0.0001", 1),
            ("100.0000", 1_000_000),
        ] {
            assert_eq!(CnyPerUsd::parse(text), Ok(CnyPerUsd(e4)), "{text}");
        }
        assert_eq!(CnyPerUsd::parse("007.20").unwrap().to_string(), "7.2");
        for bad in [
            "100.0001", "101", "7.23456", "-1", "1e2", "", " 7", "7.", ".5", "１", "0100", "+1",
            "1.2.3",
            // A digit outside ASCII among the decimals, whose bytes would
            // otherwise read as a value under the cap.
            "0.１",
        ] {
            assert_eq!(
                CnyPerUsd::parse(bad),
                Err(RateCardError::BadCnyPerUsd(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_cny_per_usd_rate_round_trips_as_a_string() {
        let r: CnyPerUsd = serde_json::from_str(r#""7.2""#).unwrap();
        assert_eq!(r, CnyPerUsd(72_000));
        assert_eq!(serde_json::to_string(&r).unwrap(), r#""7.2""#);
        // A JSON number is a binary float by the time serde sees it.
        assert!(serde_json::from_str::<CnyPerUsd>("7.2").is_err());
        assert!(serde_json::from_str::<CnyPerUsd>("7").is_err());
        assert!(serde_json::from_str::<CnyPerUsd>(r#""101""#).is_err());
        for (e4, text) in [
            (0, "0"),
            (1, "0.0001"),
            (10_000, "1"),
            (66_667, "6.6667"),
            (72_000, "7.2"),
            (1_000_000, "100"),
        ] {
            assert_eq!(CnyPerUsd(e4).to_string(), text, "{e4}");
        }
        assert_eq!(
            (CnyPerUsd::ONE, CnyPerUsd::MAX),
            (CnyPerUsd(10_000), CnyPerUsd(1_000_000))
        );
        // Every rate there is reads back from its own text.
        for e4 in 0..=CnyPerUsd::MAX.0 {
            let r = CnyPerUsd(e4);
            assert_eq!(CnyPerUsd::parse(&r.to_string()), Ok(r));
        }
    }

    #[test]
    fn listed_cny_prices_are_exact() {
        let r = |s| Rate::parse(s).unwrap();
        let at = CnyPerUsd::parse("7.2").unwrap();
        assert_eq!(cny_per_million(r("4"), at, 0).as_deref(), Some("28.8"));
        assert_eq!(cny_per_million(r("0.15"), at, 0).as_deref(), Some("1.08"));
        assert_eq!(cny_per_token(r("4"), at, 0).as_deref(), Some("0.0000288"));
        assert_eq!(
            cny_per_token(r("4"), at, 1_000).as_deref(),
            Some("0.00002592")
        );
        assert_eq!(
            cny_per_million(Rate(1), CnyPerUsd(1), 9_999).as_deref(),
            Some("0.00000000000001")
        );
        assert_eq!(
            cny_per_token(Rate(1), CnyPerUsd(1), 9_999).as_deref(),
            Some("0.00000000000000000001")
        );
        assert_eq!(
            cny_per_million(Rate::MAX, CnyPerUsd::MAX, 0).as_deref(),
            Some("100000000")
        );
        assert_eq!(
            cny_per_token(Rate::MAX, CnyPerUsd::MAX, 0).as_deref(),
            Some("100")
        );
        assert_eq!(cny_per_million(Rate(0), at, 0).as_deref(), Some("0"));
        assert_eq!(cny_per_token(Rate(0), at, 0).as_deref(), Some("0"));
        assert_eq!(cny_per_million(r("4"), at, 10_000).as_deref(), Some("0"));
        assert_eq!(cny_per_million(r("4"), at, 10_001), None);
        assert_eq!(cny_per_token(r("4"), at, 10_001), None);
    }

    // ---- Dimension and Rates ----

    #[test]
    fn each_dimension_is_named_as_its_rate_field() {
        let all: std::collections::BTreeSet<_> = Dimension::ALL.into_iter().collect();
        assert_eq!(all.len(), 11);
        for d in Dimension::ALL {
            let name = serde_json::to_value(d).unwrap();
            assert_eq!(name, d.as_str());
            assert_eq!(d.to_string(), d.as_str());
            assert_eq!(serde_json::from_value::<Dimension>(name).unwrap(), d);

            let mut json = serde_json::json!({"input": "1", "output": "2"});
            json[d.as_str()] = "7".into();
            let r: Rates = serde_json::from_value(json).unwrap();
            assert_eq!(r.explicit(d), Some(Rate(7_000_000)), "{d}");
        }
        assert_eq!(Dimension::CacheWrite1h.as_str(), "cache_write_1h");
    }

    #[test]
    fn all_names_every_rate_field() {
        // A struct literal, so a new field must be written here; then this
        // fails until `Dimension::ALL` names it too.
        let r = Some(Rate(1));
        let full = Rates {
            input: Rate(1),
            output: Rate(1),
            cache_read: r,
            cache_write: r,
            cache_write_1h: r,
            reasoning: r,
            audio_input: r,
            audio_output: r,
            image_input: r,
            image_output: r,
            web_search: r,
        };
        let json = serde_json::to_value(&full).unwrap();
        let fields: std::collections::BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let named: std::collections::BTreeSet<&str> =
            Dimension::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(fields.len(), Dimension::ALL.len());
        assert_eq!(fields, named);
    }

    #[test]
    fn a_card_with_an_unknown_rate_is_refused() {
        assert!(serde_json::from_str::<RateCard>(
            r#"{"default":{"base":{"input":"1","output":"2","image_count":"3"}}}"#
        )
        .is_err());
    }

    #[test]
    fn an_unknown_key_anywhere_in_a_card_is_refused() {
        for bad in [
            r#"{"default":{"base":{"input":"1","output":"2"},"note":"x"}}"#,
            r#"{"default":{"base":{"input":"1","output":"2"},
                "long_context":[{"above_input_tokens":1,"rates":{"input":"1","output":"2"},"note":"x"}]}}"#,
            r#"{"default":{"base":{"input":"1"}}}"#,
            r#"{"default":{"base":{"input":1,"output":2}}}"#,
        ] {
            assert!(serde_json::from_str::<RateCard>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_tier_named_twice_is_refused_not_overwritten() {
        let twice = r#"{"default":{"base":{"input":"1","output":"2"}},
                        "default":{"base":{"input":"9","output":"9"}}}"#;
        let err = serde_json::from_str::<RateCard>(twice).unwrap_err();
        assert!(err.to_string().contains("duplicate service tier"), "{err}");
    }

    #[test]
    fn an_explicit_rate_has_no_fallback() {
        let r = rates(r#"{"input":"1","output":"2"}"#);
        assert_eq!(r.explicit(Dimension::Input), Some(Rate(1_000_000)));
        assert_eq!(r.explicit(Dimension::Output), Some(Rate(2_000_000)));
        for d in Dimension::ALL {
            if !matches!(d, Dimension::Input | Dimension::Output) {
                assert_eq!(r.explicit(d), None, "{d}");
            }
        }
    }

    #[test]
    fn absent_optional_rates_fall_back_the_way_the_gateway_prices() {
        let r = rates(r#"{"input":"1","output":"2","cache_write":"1.25"}"#);
        assert_eq!(r.rate_for(Dimension::CacheRead), Some(Rate(1_000_000)));
        assert_eq!(r.rate_for(Dimension::CacheWrite1h), Some(Rate(1_250_000)));
        assert_eq!(r.rate_for(Dimension::Reasoning), Some(Rate(2_000_000)));
        assert_eq!(r.rate_for(Dimension::AudioOutput), Some(Rate(2_000_000)));
        assert_eq!(r.rate_for(Dimension::WebSearch), None);
    }

    #[test]
    fn every_fallback_stays_within_one_rate_set() {
        use Dimension::*;
        let bare = rates(r#"{"input":"1","output":"2"}"#);
        let (i, o) = (Some(Rate(1_000_000)), Some(Rate(2_000_000)));
        for (d, want) in [
            (Input, i),
            (Output, o),
            (CacheRead, i),
            (CacheWrite, i),
            (CacheWrite1h, i),
            (Reasoning, o),
            (AudioInput, i),
            (AudioOutput, o),
            (ImageInput, i),
            (ImageOutput, o),
            (WebSearch, None),
        ] {
            assert_eq!(bare.rate_for(d), want, "{d}");
        }

        // A rate the set carries always wins over its fallback.
        let full = rates(
            r#"{"input":"1","output":"2","cache_read":"3","cache_write":"4","cache_write_1h":"5",
                "reasoning":"6","audio_input":"7","audio_output":"8","image_input":"9",
                "image_output":"10","web_search":"11"}"#,
        );
        for d in Dimension::ALL {
            assert!(full.explicit(d).is_some(), "{d}");
            assert_eq!(full.rate_for(d), full.explicit(d), "{d}");
        }
    }

    // ---- validate ----

    #[test]
    fn a_card_needs_a_default_tier() {
        let c: RateCard =
            serde_json::from_str(r#"{"flex":{"base":{"input":"1","output":"2"}}}"#).unwrap();
        assert_eq!(c.validate(), Err(RateCardError::NoDefaultTier));
    }

    #[test]
    fn a_tier_name_outside_the_pattern_is_refused() {
        for bad in ["Flex", "-flex", "_flex", "not a tier!", "", &"a".repeat(33)] {
            let mut json = serde_json::json!({"default": {"base": {"input": "1", "output": "2"}}});
            json[bad] = serde_json::json!({"base": {"input": "1", "output": "2"}});
            let c: RateCard = serde_json::from_value(json).unwrap();
            assert_eq!(
                c.validate(),
                Err(RateCardError::BadTierName(bad.to_owned())),
                "{bad:?}"
            );
        }
        let longest = "a".repeat(32);
        let mut json = serde_json::json!({"default": {"base": {"input": "1", "output": "2"}}});
        json[longest.as_str()] = serde_json::json!({"base": {"input": "1", "output": "2"}});
        json["0-flex_2"] = serde_json::json!({"base": {"input": "1", "output": "2"}});
        let c: RateCard = serde_json::from_value(json).unwrap();
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn a_tier_spelled_as_a_default_alias_is_refused() {
        // A served `standard` or `auto` selects `default`, so such a key
        // could never be selected and its prices would be silently ignored.
        for alias in ["standard", "auto"] {
            let c = card(&format!(
                r#"{{"default":{{"base":{{"input":"1","output":"2"}}}},
                    "{alias}":{{"base":{{"input":"9","output":"9"}}}}}}"#
            ));
            let err = c.validate().unwrap_err();
            assert_eq!(err, RateCardError::BadTierName(alias.to_owned()));
            assert!(
                err.to_string().contains("are aliases of `default`"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_rate_above_the_cap_is_refused_however_it_was_built() {
        let mut c = one_rate_card(Rate(1));
        assert_eq!(c.validate(), Ok(()));
        c.0.get_mut(&ServiceTier::default_tier())
            .unwrap()
            .base
            .cache_read = Some(Rate(Rate::MAX.0 + 1));
        assert_eq!(c.validate(), Err(RateCardError::RateTooLarge));
    }

    #[test]
    fn the_cap_binds_every_tier_and_band_and_admits_the_cap_itself() {
        let over = Rate(Rate::MAX.0 + 1);
        let (default, batch) = (ServiceTier::default_tier(), ServiceTier::batch());
        let two_tiers = || {
            card(
                r#"{"default":{"base":{"input":"1","output":"2","cache_read":"0.1"},
                    "long_context":[{"above_input_tokens":200000,"rates":{"input":"2","output":"3","cache_read":"0.2"}}]},
                    "batch":{"base":{"input":"0.5","output":"1","cache_read":"0.05"},
                    "long_context":[{"above_input_tokens":200000,"rates":{"input":"1","output":"1.5","cache_read":"0.1"}}]}}"#,
            )
        };
        assert_eq!(two_tiers().validate(), Ok(()));

        let mut c = two_tiers();
        c.0.get_mut(&default).unwrap().long_context[0].rates.output = over;
        assert_eq!(
            c.validate(),
            Err(RateCardError::RateTooLarge),
            "default band"
        );
        let mut c = two_tiers();
        c.0.get_mut(&batch).unwrap().base.input = over;
        assert_eq!(c.validate(), Err(RateCardError::RateTooLarge), "batch base");
        let mut c = two_tiers();
        c.0.get_mut(&batch).unwrap().long_context[0]
            .rates
            .cache_read = Some(over);
        assert_eq!(c.validate(), Err(RateCardError::RateTooLarge), "batch band");

        // Exactly the cap, in every rate of every tier and band, is valid.
        let mut c = two_tiers();
        for tier in c.0.values_mut() {
            let bands = tier.long_context.iter_mut().map(|b| &mut b.rates);
            for r in std::iter::once(&mut tier.base).chain(bands) {
                (r.input, r.output, r.cache_read) = (Rate::MAX, Rate::MAX, Some(Rate::MAX));
            }
        }
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn long_context_bands_must_rise() {
        let band =
            |n| format!(r#"{{"above_input_tokens":{n},"rates":{{"input":"2","output":"3"}}}}"#);
        let card = |bands: &[String]| {
            serde_json::from_str::<RateCard>(&format!(
                r#"{{"default":{{"base":{{"input":"1","output":"2"}},"long_context":[{}]}}}}"#,
                bands.join(",")
            ))
            .unwrap()
        };
        assert_eq!(
            card(&[band(0)]).validate(),
            Err(RateCardError::ZeroBandThreshold)
        );
        assert_eq!(
            card(&[band(200_000), band(200_000)]).validate(),
            Err(RateCardError::BandsNotRising)
        );
        assert_eq!(
            card(&[band(200_000), band(128_000)]).validate(),
            Err(RateCardError::BandsNotRising)
        );
        assert_eq!(card(&[band(128_000), band(200_000)]).validate(), Ok(()));
    }

    #[test]
    fn a_band_without_a_rate_its_base_has_is_refused() {
        // Review Focus 5
        let c: RateCard = serde_json::from_str(
            r#"{"default":{"base":{"input":"1","output":"2","cache_read":"0.1"},
            "long_context":[{"above_input_tokens":200000,"rates":{"input":"2","output":"3"}}]}}"#,
        )
        .unwrap();
        assert!(matches!(
            c.validate(),
            Err(RateCardError::BandMissingRate {
                above: 200_000,
                dimension: Dimension::CacheRead,
                ..
            })
        ));
    }

    #[test]
    fn a_non_default_band_without_a_rate_its_own_base_has_is_refused() {
        // batch's base carries cache_read and default's does not, so only
        // batch's own base can say its band is missing one.
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"2","output":"3"}}]},
                "batch":{"base":{"input":"0.5","output":"1","cache_read":"0.05"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"1","output":"1.5"}}]}}"#,
        );
        assert_eq!(
            c.validate(),
            Err(RateCardError::BandMissingRate {
                tier: ServiceTier::batch(),
                above: 200_000,
                dimension: Dimension::CacheRead,
            })
        );
    }

    #[test]
    fn a_non_default_band_without_a_rate_the_default_band_has_is_refused() {
        // Ruling P2-R3: default's band carries a cache_read rate its base
        // does not. Without one of its own, a long flex prompt's cache reads
        // would bill at the flex band's input rate.
        let flex = ServiceTier::normalize(Some("flex"), false).unwrap();
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"2","output":"3","cache_read":"0.2"}}]},
                "flex":{"base":{"input":"0.5","output":"1"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"1","output":"1.5"}}]}}"#,
        );
        assert_eq!(
            c.validate(),
            Err(RateCardError::BandMissingRate {
                tier: flex,
                above: 200_000,
                dimension: Dimension::CacheRead,
            })
        );
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"2","output":"3","cache_read":"0.2"}}]},
                "flex":{"base":{"input":"0.5","output":"1"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"1","output":"1.5","cache_read":"0.1"}}]}}"#,
        );
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn a_tier_without_a_rate_the_default_has_is_refused() {
        let c: RateCard = serde_json::from_str(
            r#"{"default":{"base":{"input":"1","output":"2","cache_read":"0.1"}},
            "batch":{"base":{"input":"0.5","output":"1"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            c.validate(),
            Err(RateCardError::TierMissingRate {
                dimension: Dimension::CacheRead,
                ..
            })
        ));
    }

    #[test]
    fn a_tier_without_the_one_hour_write_the_default_has_is_refused() {
        // Its own cache_write would otherwise price the one-hour write.
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2","cache_write":"1.25","cache_write_1h":"2"}},
                "batch":{"base":{"input":"0.5","output":"1","cache_write":"0.625"}}}"#,
        );
        assert_eq!(
            c.validate(),
            Err(RateCardError::TierMissingRate {
                tier: ServiceTier::batch(),
                dimension: Dimension::CacheWrite1h,
            })
        );
    }

    #[test]
    fn a_tier_whose_bands_differ_from_default_is_refused() {
        let flex = ServiceTier::normalize(Some("flex"), false).unwrap();
        // default has a band at 200000; flex has none.
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"2","output":"3"}}]},
                "flex":{"base":{"input":"0.5","output":"1"}}}"#,
        );
        assert_eq!(
            c.validate(),
            Err(RateCardError::BandsDifferFromDefault { tier: flex.clone() })
        );
        // flex has a band at another threshold.
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"2","output":"3"}}]},
                "flex":{"base":{"input":"0.5","output":"1"},
                "long_context":[{"above_input_tokens":128000,"rates":{"input":"1","output":"1.5"}}]}}"#,
        );
        assert_eq!(
            c.validate(),
            Err(RateCardError::BandsDifferFromDefault { tier: flex.clone() })
        );
        // flex has a band the default lacks.
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2"}},
                "flex":{"base":{"input":"0.5","output":"1"},
                "long_context":[{"above_input_tokens":200000,"rates":{"input":"1","output":"1.5"}}]}}"#,
        );
        assert_eq!(
            c.validate(),
            Err(RateCardError::BandsDifferFromDefault { tier: flex })
        );
    }

    #[test]
    fn a_complete_card_with_tiers_and_bands_is_valid() {
        let tier = |b: &str, l: &str| {
            format!(
                r#"{{"base":{b},"long_context":[{{"above_input_tokens":200000,"rates":{l}}}]}}"#
            )
        };
        let full = |i, o, cr, cw, cw1, ws| {
            format!(
                r#"{{"input":"{i}","output":"{o}","cache_read":"{cr}","cache_write":"{cw}","cache_write_1h":"{cw1}","web_search":"{ws}"}}"#
            )
        };
        let default = tier(
            &full("1", "2", "0.1", "1.25", "2", "10000"),
            &full("2", "3", "0.2", "2.5", "4", "10000"),
        );
        let cheap = tier(
            &full("0.5", "1", "0.05", "0.625", "1", "10000"),
            &full("1", "1.5", "0.1", "1.25", "2", "10000"),
        );
        let c = card(&format!(
            r#"{{"default":{default},"flex":{cheap},"batch":{cheap}}}"#
        ));
        assert_eq!(c.validate(), Ok(()));
        // What it reads back is what it was written as.
        let again: RateCard = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(again, c);
    }

    // ---- select ----

    #[test]
    fn a_band_applies_only_strictly_above_its_threshold() {
        // Review Focus 5
        let c = card_with_band(200_000);
        assert_eq!(
            c.select(&ServiceTier::default_tier(), 200_000)
                .unwrap()
                .band_above,
            None
        );
        assert_eq!(
            c.select(&ServiceTier::default_tier(), 200_001)
                .unwrap()
                .band_above,
            Some(200_000)
        );
    }

    #[test]
    fn the_highest_band_below_the_input_supplies_every_rate() {
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2"},"long_context":[
                {"above_input_tokens":128000,"rates":{"input":"2","output":"3"}},
                {"above_input_tokens":200000,"rates":{"input":"4","output":"5"}}]}}"#,
        );
        let at = |n| {
            let s = c.select(&ServiceTier::default_tier(), n).unwrap();
            (s.band_above, s.rates.input, s.rates.output)
        };
        assert_eq!(at(0), (None, Rate(1_000_000), Rate(2_000_000)));
        assert_eq!(at(128_000), (None, Rate(1_000_000), Rate(2_000_000)));
        assert_eq!(
            at(128_001),
            (Some(128_000), Rate(2_000_000), Rate(3_000_000))
        );
        assert_eq!(
            at(200_000),
            (Some(128_000), Rate(2_000_000), Rate(3_000_000))
        );
        assert_eq!(
            at(200_001),
            (Some(200_000), Rate(4_000_000), Rate(5_000_000))
        );
        assert_eq!(
            at(u64::MAX),
            (Some(200_000), Rate(4_000_000), Rate(5_000_000))
        );
    }

    #[test]
    fn a_tier_the_card_lacks_selects_nothing() {
        assert!(card_with_band(200_000)
            .select(&ServiceTier::normalize(Some("scale"), false).unwrap(), 10)
            .is_none());
    }

    // ---- ServiceTier ----

    #[test]
    fn served_tiers_normalize() {
        let n = |s, b| ServiceTier::normalize(s, b).map(|t| t.as_str().to_owned());
        assert_eq!(n(None, false).as_deref(), Some("default"));
        assert_eq!(n(Some("standard"), false).as_deref(), Some("default"));
        assert_eq!(n(Some("STANDARD"), false).as_deref(), Some("default"));
        // OpenAI's request-side default, echoed back by an OpenAI-compatible
        // upstream as the served tier: the setting means `default`.
        assert_eq!(n(Some("auto"), false).as_deref(), Some("default"));
        assert_eq!(n(Some("AUTO"), false).as_deref(), Some("default"));
        assert_eq!(n(Some("FLEX"), false).as_deref(), Some("flex"));
        assert_eq!(n(Some("priority"), true).as_deref(), Some("batch"));
        assert_eq!(n(Some("not a tier!"), false), None);
    }

    #[test]
    fn served_tiers_normalize_at_the_edges() {
        fn n(s: Option<&str>, b: bool) -> Option<String> {
            ServiceTier::normalize(s, b).map(|t| t.as_str().to_owned())
        }
        assert_eq!(n(Some(""), false).as_deref(), Some("default"));
        assert_eq!(n(Some("Default"), false).as_deref(), Some("default"));
        assert_eq!(n(None, true).as_deref(), Some("batch"));
        assert_eq!(n(Some("batch"), false).as_deref(), Some("batch"));
        assert_eq!(n(Some("PRIORITY"), false).as_deref(), Some("priority"));
        let longest = "a".repeat(32);
        assert_eq!(n(Some(&longest), false), Some(longest));
        assert_eq!(n(Some(&"a".repeat(33)), false), None);
        assert_eq!(n(Some(" flex"), false), None);
        assert_eq!(n(Some("-flex"), false), None);
        assert_eq!(ServiceTier::default_tier().as_str(), "default");
        assert_eq!(ServiceTier::batch().as_str(), "batch");
    }

    // ---- with_markup ----

    #[test]
    fn markup_rounds_each_rate_half_up_and_respects_the_cap() {
        let c = one_rate_card(Rate(3)).with_markup(5_000).unwrap(); // 3 × 1.5 = 4.5 → 5
        assert_eq!(c.0[&ServiceTier::default_tier()].base.input, Rate(5));
        assert_eq!(
            one_rate_card(Rate::MAX).with_markup(1),
            Err(RateCardError::RateTooLarge)
        );
    }

    #[test]
    fn markup_rounds_half_up_and_never_more() {
        let marked = |r, bps| {
            one_rate_card(Rate(r)).with_markup(bps).unwrap().0[&ServiceTier::default_tier()]
                .base
                .input
        };
        assert_eq!(marked(1, 4_999), Rate(1)); // 1.4999
        assert_eq!(marked(1, 5_000), Rate(2)); // 1.5
        assert_eq!(marked(7, 0), Rate(7));
        assert_eq!(marked(0, 9_999), Rate(0));
        // The largest markup cannot wrap: 1 × (1 + 4294967295 / 10000) = 429497.7295.
        assert_eq!(marked(1, u32::MAX), Rate(429_498));
        assert_eq!(marked(Rate::MAX.0, 0), Rate::MAX);
        assert_eq!(
            one_rate_card(Rate::MAX).with_markup(u32::MAX),
            Err(RateCardError::RateTooLarge)
        );
        assert_eq!(
            one_rate_card(Rate(u64::MAX)).with_markup(0),
            Err(RateCardError::RateTooLarge)
        );
    }

    #[test]
    fn markup_reaches_every_rate_and_keeps_every_threshold() {
        let c = card(
            r#"{"default":{"base":{"input":"1","output":"2","cache_write_1h":"0.000003"},
                "long_context":[{"above_input_tokens":200000,
                    "rates":{"input":"2","output":"3","cache_write_1h":"0.000005"}}]},
                "batch":{"base":{"input":"0.5","output":"1","cache_write_1h":"0.000001","web_search":"10"},
                "long_context":[{"above_input_tokens":200000,
                    "rates":{"input":"1","output":"1.5","cache_write_1h":"0.000001","web_search":"10"}}]}}"#,
        );
        let m = c.with_markup(5_000).unwrap();
        let want = card(
            r#"{"default":{"base":{"input":"1.5","output":"3","cache_write_1h":"0.000005"},
                "long_context":[{"above_input_tokens":200000,
                    "rates":{"input":"3","output":"4.5","cache_write_1h":"0.000008"}}]},
                "batch":{"base":{"input":"0.75","output":"1.5","cache_write_1h":"0.000002","web_search":"15"},
                "long_context":[{"above_input_tokens":200000,
                    "rates":{"input":"1.5","output":"2.25","cache_write_1h":"0.000002","web_search":"15"}}]}}"#,
        );
        assert_eq!(m, want);
    }
}
