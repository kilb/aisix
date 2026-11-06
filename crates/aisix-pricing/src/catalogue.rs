//! The built-in list-price catalogue: upstream list prices, transcribed from
//! each provider's own pricing page into [`RateCard`]s.
//!
//! The data lives in `catalogue/list-prices.json`. Every entry names the page
//! its rates were read from and the date they were read, and loading refuses
//! a card that breaks the rate model's rules, a name used twice, a source
//! outside the providers' own sites, and a price dated after the file.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use chrono::NaiveDate;
use serde::Deserialize;

use crate::rates::{RateCard, RateCardError};

/// Hosts a `source_url` may name: the providers' own documentation sites.
const OFFICIAL_HOSTS: [&str; 12] = [
    "openai.com",
    "platform.openai.com",
    "anthropic.com",
    "www.anthropic.com",
    "docs.anthropic.com",
    "docs.claude.com",
    "platform.claude.com",
    "claude.com",
    "ai.google.dev",
    "cloud.google.com",
    "api-docs.deepseek.com",
    "aws.amazon.com",
];

/// One model's list prices and where they came from.
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
    pub card: RateCard,
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
    version: NaiveDate,
    entries: Vec<CatalogueEntry>,
    /// `<provider>/<name>` or `<provider>/<alias>` to the entry's position.
    index: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogueError {
    #[error("catalogue does not decode: {0}")]
    Malformed(String),
    #[error("entry key {0:?} is not `<provider>/<model_name>`")]
    BadKey(String),
    #[error("`{0}` is named by more than one entry or alias")]
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
            serde_json::from_str(json).map_err(|e| CatalogueError::Malformed(e.to_string()))?;
        let mut index = BTreeMap::new();
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
                if index.insert(name.clone(), i).is_some() {
                    return Err(CatalogueError::DuplicateKey(name));
                }
            }
        }
        Ok(Catalogue {
            version: file.version,
            entries: file.entries,
            index,
        })
    }

    /// The catalogue compiled into this binary, parsed once.
    pub fn builtin() -> Result<&'static Catalogue, &'static CatalogueError> {
        static BUILTIN: LazyLock<Result<Catalogue, CatalogueError>> =
            LazyLock::new(|| Catalogue::from_json(include_str!("../catalogue/list-prices.json")));
        BUILTIN.as_ref()
    }

    /// The date the catalogue was last revised.
    pub fn version(&self) -> NaiveDate {
        self.version
    }

    pub fn entries(&self) -> &[CatalogueEntry] {
        &self.entries
    }

    /// The entry whose key or alias is `<provider>/<model>`.
    pub fn get(&self, provider: &str, model: &str) -> Option<&CatalogueEntry> {
        self.index
            .get(&format!("{provider}/{model}"))
            .map(|i| &self.entries[*i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rates::ServiceTier;

    /// One valid entry as JSON text; the tests below break one thing at a time.
    fn entry(key: &str, aliases: &str, url: &str, as_of: &str, card: &str) -> String {
        format!(
            r#"{{"key":"{key}","aliases":{aliases},"source_url":"{url}","as_of":"{as_of}","card":{card}}}"#
        )
    }

    const OK_URL: &str = "https://openai.com/api/pricing";
    const OK_CARD: &str = r#"{"default":{"base":{"input":"1","output":"2"}}}"#;

    fn file(entries: &[String]) -> String {
        format!(
            r#"{{"version":"2026-11-06","entries":[{}]}}"#,
            entries.join(",")
        )
    }

    #[test]
    fn the_builtin_catalogue_loads_and_validates() {
        let c = Catalogue::builtin().unwrap();
        for p in ["openai/", "anthropic/", "google/", "deepseek/"] {
            assert!(c.entries().iter().any(|e| e.key.starts_with(p)), "{p}");
        }
        // `from_json` validated every card on load; do it again so that a
        // loader that stopped validating cannot go unnoticed.
        for e in c.entries() {
            e.card.validate().unwrap();
        }
    }

    #[test]
    fn every_entry_of_a_provider_that_publishes_batch_prices_has_a_batch_tier() {
        // OpenAI's page lists no batch price for its embedding models.
        for e in Catalogue::builtin()
            .unwrap()
            .entries()
            .iter()
            .filter(|e| e.key.starts_with("openai/") || e.key.starts_with("anthropic/"))
            .filter(|e| !e.key.contains("embedding"))
        {
            assert!(e.card.0.contains_key(&ServiceTier::batch()), "{}", e.key);
        }
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
        let a = c.get("openai", "m").unwrap();
        let b = c.get("openai", "m-2026").unwrap();
        assert!(std::ptr::eq(a, b));
        assert!(c.get("google", "m").is_none());
        assert!(c.get("openai", "other").is_none());
    }
}
