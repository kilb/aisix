//! The built-in list-price catalogue: upstream list prices, transcribed from
//! each provider's own pricing page into [`RateCard`]s.
//!
//! The data lives in `catalogue/list-prices.json`. Every entry names the page
//! its rates were read from and the date they were read, and loading refuses
//! a card that breaks the rate model's rules, a name used twice at one
//! effective date, a source outside the providers' own sites, and a price
//! dated after the file.
//!
//! A provider may announce a price change ahead of time. That is a second
//! entry under the same key with an `effective_from` date, and a lookup at an
//! instant takes the latest entry that is in effect then.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::Deserialize;

use crate::rates::{RateCard, RateCardError};

/// Hosts a `source_url` may name: the providers' own documentation sites.
const OFFICIAL_HOSTS: [&str; 14] = [
    "openai.com",
    "platform.openai.com",
    "developers.openai.com",
    "anthropic.com",
    "www.anthropic.com",
    "docs.anthropic.com",
    "docs.claude.com",
    "platform.claude.com",
    "claude.com",
    "ai.google.dev",
    "aistudio.google.com",
    "cloud.google.com",
    "api-docs.deepseek.com",
    "aws.amazon.com",
];

/// One model's list prices, where they came from, and when they apply.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogueEntry {
    /// `<provider>/<model_name>`.
    pub key: String,
    /// Other names of the same model under the same provider.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The provider's pricing page the rates were transcribed from.
    pub source_url: String,
    /// The day the page was read.
    pub as_of: NaiveDate,
    /// The first UTC day these rates apply. Absent means they apply from the
    /// beginning of time, until a later entry of the same key takes over.
    #[serde(default)]
    pub effective_from: Option<NaiveDate>,
    /// Which rates are derived and how, which tiers the page does not publish,
    /// validity windows and how the page's wording was read.
    #[serde(default)]
    pub notes: Option<String>,
    pub card: RateCard,
}

impl CatalogueEntry {
    fn in_effect_at(&self, at: DateTime<Utc>) -> bool {
        self.effective_from
            .is_none_or(|d| d.and_time(NaiveTime::MIN).and_utc() <= at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogueFile {
    version: NaiveDate,
    entries: Vec<CatalogueEntry>,
}

/// A validated set of list prices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalogue {
    version: String,
    entries: Vec<CatalogueEntry>,
    /// `<provider>/<name>` or `<provider>/<alias>` to the positions of the
    /// entries that carry it, earliest `effective_from` first.
    index: BTreeMap<String, Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogueError {
    #[error("catalogue does not decode: {0}")]
    Parse(String),
    #[error("entry key {0:?} is not `<provider>/<model_name>`")]
    BadKey(String),
    #[error("`{0}` is named by more than one entry or alias with the same effective date")]
    DuplicateKey(String),
    #[error("entry {key}: its rate card is invalid: {source}")]
    InvalidCard {
        key: String,
        #[source]
        source: RateCardError,
    },
    #[error("entry {key}: source {url:?} is not an official https pricing page")]
    UntrustedSource { key: String, url: String },
    #[error("entry {key}: priced on {as_of}, after the catalogue's version {version}")]
    AsOfAfterVersion {
        key: String,
        as_of: NaiveDate,
        version: NaiveDate,
    },
}

/// True for `https://<official host>/…`. The host is everything up to the
/// first `/`, `?` or `#`, so userinfo (`host@evil`) and a port never match.
fn is_official_source(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    OFFICIAL_HOSTS.contains(&host)
}

impl Catalogue {
    /// Decodes and validates a catalogue file.
    pub fn from_json(json: &str) -> Result<Catalogue, CatalogueError> {
        let file: CatalogueFile =
            serde_json::from_str(json).map_err(|e| CatalogueError::Parse(e.to_string()))?;
        let mut index: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, e) in file.entries.iter().enumerate() {
            let provider = match e.key.split_once('/') {
                Some((p, m)) if !p.is_empty() && !m.is_empty() => p,
                _ => return Err(CatalogueError::BadKey(e.key.clone())),
            };
            e.card
                .validate()
                .map_err(|source| CatalogueError::InvalidCard {
                    key: e.key.clone(),
                    source,
                })?;
            if !is_official_source(&e.source_url) {
                return Err(CatalogueError::UntrustedSource {
                    key: e.key.clone(),
                    url: e.source_url.clone(),
                });
            }
            if e.as_of > file.version {
                return Err(CatalogueError::AsOfAfterVersion {
                    key: e.key.clone(),
                    as_of: e.as_of,
                    version: file.version,
                });
            }
            let names = std::iter::once(e.key.clone())
                .chain(e.aliases.iter().map(|a| format!("{provider}/{a}")));
            for name in names {
                let slot = index.entry(name.clone()).or_default();
                if slot
                    .iter()
                    .any(|j| file.entries[*j].effective_from == e.effective_from)
                {
                    return Err(CatalogueError::DuplicateKey(format!(
                        "{name} (effective from {:?})",
                        e.effective_from
                    )));
                }
                slot.push(i);
            }
        }
        for slot in index.values_mut() {
            slot.sort_by_key(|i| file.entries[*i].effective_from);
        }
        Ok(Catalogue {
            version: file.version.to_string(),
            entries: file.entries,
            index,
        })
    }

    /// The catalogue compiled into this binary, parsed once.
    pub fn builtin() -> Result<Arc<Catalogue>, CatalogueError> {
        static BUILTIN: LazyLock<Result<Arc<Catalogue>, CatalogueError>> = LazyLock::new(|| {
            Catalogue::from_json(include_str!("../catalogue/list-prices.json")).map(Arc::new)
        });
        BUILTIN.clone()
    }

    /// The date the catalogue was last revised, `YYYY-MM-DD`.
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn entries(&self) -> &[CatalogueEntry] {
        &self.entries
    }

    /// The entry whose key or alias is `<provider>/<model>` and whose
    /// `effective_from` is the latest one at or before `at`. `None` when the
    /// name is unknown or no entry of it is in effect yet.
    pub fn get(&self, provider: &str, model: &str, at: DateTime<Utc>) -> Option<&CatalogueEntry> {
        self.index
            .get(&format!("{provider}/{model}"))?
            .iter()
            .rev()
            .map(|i| &self.entries[*i])
            .find(|e| e.in_effect_at(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rates::{Dimension, Rate, Rates, ServiceTier};

    /// One valid entry as JSON text; the tests below break one thing at a time.
    fn entry(key: &str, aliases: &str, url: &str, as_of: &str, card: &str) -> String {
        entry_from(key, aliases, url, as_of, "null", card)
    }

    fn entry_from(
        key: &str,
        aliases: &str,
        url: &str,
        as_of: &str,
        effective_from: &str,
        card: &str,
    ) -> String {
        format!(
            r#"{{"key":"{key}","aliases":{aliases},"source_url":"{url}","as_of":"{as_of}","effective_from":{effective_from},"card":{card}}}"#
        )
    }

    const OK_URL: &str = "https://openai.com/api/pricing";
    const OK_CARD: &str = r#"{"default":{"base":{"input":"1","output":"2"}}}"#;
    const NEW_CARD: &str = r#"{"default":{"base":{"input":"3","output":"4"}}}"#;

    fn file(entries: &[String]) -> String {
        format!(
            r#"{{"version":"2026-11-06","entries":[{}]}}"#,
            entries.join(",")
        )
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Today, for the builtin catalogue's lookups: the day it was read.
    fn read_day() -> DateTime<Utc> {
        at("2026-11-06T12:00:00Z")
    }

    fn builtin() -> Arc<Catalogue> {
        Catalogue::builtin().unwrap()
    }

    fn input_of(c: &Catalogue, provider: &str, model: &str, when: &str) -> Rate {
        c.get(provider, model, at(when))
            .unwrap()
            .card
            .0
            .get(&ServiceTier::default_tier())
            .unwrap()
            .base
            .input
    }

    /// Every rate set of a tier: its base, then its bands.
    fn sets(t: &crate::rates::TierRates) -> Vec<(Option<u64>, &Rates)> {
        std::iter::once((None, &t.base))
            .chain(
                t.long_context
                    .iter()
                    .map(|b| (Some(b.above_input_tokens), &b.rates)),
            )
            .collect()
    }

    fn is_embedding(key: &str) -> bool {
        matches!(
            key,
            "openai/text-embedding-3-small"
                | "openai/text-embedding-3-large"
                | "openai/text-embedding-ada-002"
                | "google/gemini-embedding-2"
                | "google/gemini-embedding-001"
        )
    }

    fn tier(name: &str) -> ServiceTier {
        ServiceTier::normalize(Some(name), false).unwrap()
    }

    // ---- loader rules ----

    #[test]
    fn the_builtin_catalogue_loads_and_validates() {
        let c = builtin();
        for p in ["openai/", "anthropic/", "google/", "deepseek/"] {
            assert!(c.entries().iter().any(|e| e.key.starts_with(p)), "{p}");
        }
        assert!(!c.version().is_empty());
    }

    #[test]
    fn an_incomplete_band_cannot_ship_in_the_catalogue() {
        let card = r#"{"default":{"base":{"input":"1","output":"2","cache_read":"0.1"},
            "long_context":[{"above_input_tokens":200000,"rates":{"input":"2","output":"3"}}]}}"#;
        let json = file(&[entry("openai/m", "[]", OK_URL, "2026-11-06", card)]);
        assert!(matches!(
            Catalogue::from_json(&json),
            Err(CatalogueError::InvalidCard { .. })
        ));
    }

    #[test]
    fn a_duplicate_key_or_alias_is_refused() {
        let dup_key = file(&[
            entry("openai/m", "[]", OK_URL, "2026-11-06", OK_CARD),
            entry("openai/m", "[]", OK_URL, "2026-11-06", OK_CARD),
        ]);
        assert!(matches!(
            Catalogue::from_json(&dup_key),
            Err(CatalogueError::DuplicateKey(_))
        ));
        let alias_is_a_key = file(&[
            entry("openai/m", "[]", OK_URL, "2026-11-06", OK_CARD),
            entry("openai/n", r#"["m"]"#, OK_URL, "2026-11-06", OK_CARD),
        ]);
        assert!(matches!(
            Catalogue::from_json(&alias_is_a_key),
            Err(CatalogueError::DuplicateKey(_))
        ));
        // The same bare name under another provider is a different model.
        let other_provider = file(&[
            entry("openai/m", "[]", OK_URL, "2026-11-06", OK_CARD),
            entry(
                "google/n",
                r#"["m"]"#,
                "https://ai.google.dev/p",
                "2026-11-06",
                OK_CARD,
            ),
        ]);
        assert!(Catalogue::from_json(&other_provider).is_ok());
    }

    #[test]
    fn a_source_outside_the_official_pages_is_refused() {
        for url in [
            "https://example.com/p",
            "http://openai.com/p",
            "https://openai.com@example.com/p",
            "https://openai.com.example.com/p",
            "https://evil-openai.com/p",
            "https://evil.developers.openai.com/p",
        ] {
            let json = file(&[entry("openai/m", "[]", url, "2026-11-06", OK_CARD)]);
            assert!(
                matches!(
                    Catalogue::from_json(&json),
                    Err(CatalogueError::UntrustedSource { .. })
                ),
                "{url}"
            );
        }
        for url in [
            "https://developers.openai.com/api/docs/pricing",
            "https://aistudio.google.com/docs/pricing",
        ] {
            let json = file(&[entry("openai/m", "[]", url, "2026-11-06", OK_CARD)]);
            assert!(Catalogue::from_json(&json).is_ok(), "{url}");
        }
    }

    #[test]
    fn a_price_dated_after_the_catalogue_version_is_refused() {
        let json = file(&[entry("openai/m", "[]", OK_URL, "2026-11-07", OK_CARD)]);
        assert!(matches!(
            Catalogue::from_json(&json),
            Err(CatalogueError::AsOfAfterVersion { .. })
        ));
    }

    #[test]
    fn lookup_matches_the_key_and_its_aliases() {
        let json = file(&[entry(
            "openai/m",
            r#"["m-2026"]"#,
            OK_URL,
            "2026-11-06",
            OK_CARD,
        )]);
        let c = Catalogue::from_json(&json).unwrap();
        let now = read_day();
        let a = c.get("openai", "m", now).unwrap();
        let b = c.get("openai", "m-2026", now).unwrap();
        assert!(std::ptr::eq(a, b));
        assert!(c.get("google", "m", now).is_none());
        assert!(c.get("openai", "other", now).is_none());
    }

    // ---- time-bounded prices ----

    fn two_prices() -> Catalogue {
        let json = file(&[
            entry_from(
                "openai/m",
                r#"["m-a"]"#,
                OK_URL,
                "2026-11-06",
                "null",
                OK_CARD,
            ),
            entry_from(
                "openai/m",
                r#"["m-a"]"#,
                OK_URL,
                "2026-11-06",
                r#""2026-12-01""#,
                NEW_CARD,
            ),
        ]);
        Catalogue::from_json(&json).unwrap()
    }

    #[test]
    fn the_new_price_applies_from_the_first_instant_of_its_day_in_utc() {
        let c = two_prices();
        let rate = |name: &str, when: &str| {
            c.get("openai", name, at(when))
                .unwrap()
                .card
                .0
                .get(&ServiceTier::default_tier())
                .unwrap()
                .base
                .input
        };
        for name in ["m", "m-a"] {
            assert_eq!(
                rate(name, "2026-11-30T23:59:59.999Z"),
                Rate::parse("1").unwrap()
            );
            assert_eq!(
                rate(name, "2026-12-01T00:00:00Z"),
                Rate::parse("3").unwrap()
            );
            assert_eq!(
                rate(name, "2027-06-01T00:00:00Z"),
                Rate::parse("3").unwrap()
            );
        }
    }

    #[test]
    fn no_entry_is_in_effect_before_the_first_effective_date() {
        let json = file(&[entry_from(
            "openai/m",
            "[]",
            OK_URL,
            "2026-11-06",
            r#""2026-12-01""#,
            OK_CARD,
        )]);
        let c = Catalogue::from_json(&json).unwrap();
        assert!(c.get("openai", "m", at("2026-11-30T23:59:59Z")).is_none());
        assert!(c.get("openai", "m", at("2026-12-01T00:00:00Z")).is_some());
    }

    #[test]
    fn a_name_may_repeat_only_at_different_effective_dates() {
        let same_date = file(&[
            entry_from(
                "openai/m",
                "[]",
                OK_URL,
                "2026-11-06",
                r#""2026-12-01""#,
                OK_CARD,
            ),
            entry_from(
                "openai/m",
                "[]",
                OK_URL,
                "2026-11-06",
                r#""2026-12-01""#,
                NEW_CARD,
            ),
        ]);
        assert!(matches!(
            Catalogue::from_json(&same_date),
            Err(CatalogueError::DuplicateKey(_))
        ));
    }

    #[test]
    fn the_announced_price_changes_are_in_the_builtin_catalogue() {
        let c = builtin();
        let r = |s: &str| Rate::parse(s).unwrap();
        // Google states each change as a date without a time zone; UTC assumed.
        for (model, before, after, last, first) in [
            (
                "gemini-3.8-flash",
                "0.75",
                "1.5",
                "2026-12-31T23:59:59Z",
                "2027-01-01T00:00:00Z",
            ),
            (
                "gemini-3.6-flash",
                "0.75",
                "1.5",
                "2026-12-31T23:59:59Z",
                "2027-01-01T00:00:00Z",
            ),
            (
                "gemini-4-argon",
                "2",
                "4",
                "2026-11-30T23:59:59Z",
                "2026-12-01T00:00:00Z",
            ),
        ] {
            assert_eq!(input_of(&c, "google", model, last), r(before), "{model}");
            assert_eq!(input_of(&c, "google", model, first), r(after), "{model}");
        }
    }

    // ---- the lineup ----

    #[test]
    fn the_current_lineup_is_present_with_its_aliases() {
        let c = builtin();
        let now = read_day();
        for (provider, model) in [
            ("openai", "gpt-6-astra"),
            ("openai", "gpt-6.1-sol"),
            ("openai", "gpt-6.1-terra"),
            ("openai", "gpt-6-luna"),
            ("openai", "gpt-5.4"),
            ("openai", "gpt-5.4-mini"),
            ("openai", "gpt-5.4-nano"),
            ("openai", "text-embedding-3-small"),
            ("openai", "text-embedding-3-large"),
            ("openai", "text-embedding-ada-002"),
            ("anthropic", "claude-fable-5-1"),
            ("anthropic", "claude-opus-5-5"),
            ("anthropic", "claude-sonnet-5-5"),
            ("anthropic", "claude-haiku-5-5"),
            ("google", "gemini-4-argon"),
            ("google", "gemini-3.1-pro-preview"),
            ("google", "gemini-3.1-pro-preview-customtools"),
            ("google", "gemini-3.8-flash"),
            ("google", "gemini-3.6-flash"),
            ("google", "gemini-3.5-flash-lite"),
            ("google", "gemini-embedding-2"),
            ("google", "gemini-embedding-001"),
            ("deepseek", "deepseek-flash"),
            ("deepseek", "deepseek-v4-flash"),
            ("deepseek", "deepseek-v4-flash-vision-exp"),
            ("deepseek", "deepseek-v4-pro"),
        ] {
            assert!(c.get(provider, model, now).is_some(), "{provider}/{model}");
        }
    }

    // ---- invariants that hard-code no price ----

    #[test]
    fn every_priced_entry_carries_the_tiers_its_provider_publishes() {
        let batch = ServiceTier::batch();
        let flex = tier("flex");
        for e in builtin().entries() {
            let google_text = e.key.starts_with("google/") && !is_embedding(&e.key);
            // OpenAI's page lists no batch price for its embedding models.
            let batch_expected = matches!(
                e.key.split_once('/').map(|p| p.0),
                Some("openai" | "anthropic" | "google")
            ) && !(e.key.starts_with("openai/") && is_embedding(&e.key));
            if batch_expected {
                assert!(e.card.0.contains_key(&batch), "{} has no batch tier", e.key);
            }
            if google_text {
                assert!(e.card.0.contains_key(&flex), "{} has no flex tier", e.key);
            }
        }
    }

    #[test]
    fn a_text_model_carries_a_cache_read_rate_and_anthropic_its_writes() {
        for e in builtin().entries().iter().filter(|e| !is_embedding(&e.key)) {
            for (t, tr) in &e.card.0 {
                for (band, r) in sets(tr) {
                    assert!(r.cache_read.is_some(), "{} {t} {band:?}: cache_read", e.key);
                    if e.key.starts_with("anthropic/") {
                        assert!(
                            r.cache_write.is_some(),
                            "{} {t} {band:?}: cache_write",
                            e.key
                        );
                        assert!(
                            r.cache_write_1h.is_some(),
                            "{} {t} {band:?}: cache_write_1h",
                            e.key
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_openai_model_the_page_shows_with_write_columns_carries_cache_write() {
        let c = builtin();
        for m in ["gpt-6-astra", "gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna"] {
            let e = c.get("openai", m, read_day()).unwrap();
            for (t, tr) in &e.card.0 {
                for (band, r) in sets(tr) {
                    assert!(r.cache_write.is_some(), "{m} {t} {band:?}");
                }
            }
        }
    }

    #[test]
    fn batch_and_flex_are_half_of_default_where_the_pages_say_fifty_percent() {
        let half = |batch: Rate, default: Rate| batch.0 * 2 == default.0;
        for e in builtin().entries() {
            let default = &e.card.0[&ServiceTier::default_tier()];
            for name in ["batch", "flex"] {
                let Some(t) = e.card.0.get(&tier(name)) else {
                    continue;
                };
                for ((band, b), (_, d)) in sets(t).into_iter().zip(sets(default)) {
                    let at = format!("{} {name} {band:?}", e.key);
                    assert!(half(b.input, d.input), "{at}: input");
                    assert!(half(b.output, d.output), "{at}: output");
                    for dim in [Dimension::CacheWrite, Dimension::CacheWrite1h] {
                        if let (Some(x), Some(y)) = (b.explicit(dim), d.explicit(dim)) {
                            assert!(half(x, y), "{at}: {dim}");
                        }
                    }
                    // A cache read is half too, give or take a published
                    // rounding (a page shows 0.13 for half of 0.25); Google
                    // does not halve it.
                    if let (false, Some(x), Some(y)) =
                        (e.key.starts_with("google/"), b.cache_read, d.cache_read)
                    {
                        assert!((x.0 * 2).abs_diff(y.0) <= 10_000, "{at}: cache_read");
                    }
                }
            }
        }
    }

    #[test]
    fn a_cache_read_costs_less_than_input_a_band_costs_at_least_its_base_and_priority_at_least_default(
    ) {
        for e in builtin().entries() {
            let default = &e.card.0[&ServiceTier::default_tier()];
            for (t, tr) in &e.card.0 {
                let all = sets(tr);
                let base = all[0].1;
                for (band, r) in &all {
                    if let Some(read) = r.cache_read {
                        assert!(read < r.input, "{} {t} {band:?}: cache_read", e.key);
                    }
                    for dim in Dimension::ALL {
                        if let (Some(hi), Some(lo)) = (r.explicit(dim), base.explicit(dim)) {
                            assert!(hi >= lo, "{} {t} {band:?}: {dim} below its base", e.key);
                        }
                    }
                }
                if t.as_str() == "priority" {
                    for ((band, p), (_, d)) in all.iter().zip(sets(default)) {
                        for dim in Dimension::ALL {
                            if let (Some(hi), Some(lo)) = (p.explicit(dim), d.explicit(dim)) {
                                assert!(hi >= lo, "{} priority {band:?}: {dim}", e.key);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_fast_tier_is_a_name_a_served_tier_can_select() {
        assert_eq!(tier("fast").as_str(), "fast");
    }

    // ---- a pinned spot table: changing a price means editing two places ----

    #[test]
    fn spot_values_match_the_pages() {
        let c = builtin();
        let now = read_day();
        let rates = |provider: &str, model: &str, t: &str, band: Option<u64>| {
            let tr = &c.get(provider, model, now).unwrap().card.0[&tier(t)];
            sets(tr)
                .into_iter()
                .find(|(b, _)| *b == band)
                .unwrap()
                .1
                .clone()
        };
        let r = |s: &str| Some(Rate::parse(s).unwrap());
        // OpenAI: a plain default input.
        assert_eq!(
            rates("openai", "gpt-5.4", "default", None).input,
            r("2.5").unwrap()
        );
        // OpenAI: the 30m write in a long-context band.
        assert_eq!(
            rates("openai", "gpt-6-astra", "default", Some(272_000)).cache_write,
            r("25")
        );
        // OpenAI: a priority output.
        assert_eq!(
            rates("openai", "gpt-6-astra", "priority", None).output,
            r("100").unwrap()
        );
        // Anthropic: a derived batch cache read (0.05 x the batch input price).
        assert_eq!(
            rates("anthropic", "claude-opus-5-5", "batch", None).cache_read,
            r("0.1")
        );
        // Anthropic: fast mode.
        assert_eq!(
            rates("anthropic", "claude-opus-5-5", "fast", None).input,
            r("8").unwrap()
        );
        // Anthropic: the over-100,000-token band.
        assert_eq!(
            rates("anthropic", "claude-haiku-5-5", "default", Some(100_000)).input,
            r("0.5").unwrap()
        );
        // Google: a flex band.
        assert_eq!(
            rates("google", "gemini-3.1-pro-preview", "flex", Some(200_000)).output,
            r("9").unwrap()
        );
        // Google: embedding audio.
        assert_eq!(
            rates("google", "gemini-embedding-2", "default", None).audio_input,
            r("6.5")
        );
        // DeepSeek: the standard (non-peak) price.
        assert_eq!(
            rates("deepseek", "deepseek-flash", "default", None).input,
            r("0.15").unwrap()
        );
    }
}
