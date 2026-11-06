//! What a stored usage row bills.
//!
//! [`BillableUsage`] reads the fields pricing needs from one gateway usage
//! event, in the upstream's own shape and never converted. [`quantities`]
//! turns it into one quantity per [`Dimension`], and every token the
//! upstream counted lands in exactly one of them.
//!
//! The two cache vocabularies on the row have opposite arithmetic: the
//! OpenAI-shape `cached_prompt_tokens` and `cache_write_tokens` are subsets
//! of `prompt_tokens`, while the Anthropic-shape `cache_read_tokens` and
//! `cache_creation_tokens` sit on top of it. That reading lives once, in
//! `aisix_core::input_tokens_for_pricing`, which the gateway's own spend
//! computation calls too; this module calls it and keeps no second copy.

use aisix_core::{input_tokens_for_pricing, reasoning_beside_completion};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::rates::Dimension;

/// The fields pricing reads from one element of a `/dp/telemetry` body's
/// `events` array, named exactly as on the gateway's wire. Unknown fields
/// are ignored. `event_id` and `occurred_at` are required (a row without
/// them does not deserialize); every other field is `#[serde(default)]`.
///
/// An absent counter reads as `0`, which is what absence means on that
/// wire: the gateway omits every zero counter except `prompt_tokens` and
/// `completion_tokens`, which it always writes. An absent breakdown (the
/// `Option` counters) means the upstream reported none, and its tokens bill
/// in the bucket that contains them.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
pub struct BillableUsage {
    /// The gateway's own id for this row, unique per row.
    pub event_id: String,
    /// When the upstream call completed.
    pub occurred_at: DateTime<Utc>,
    /// The tenant the authenticating key belongs to.
    #[serde(default)]
    pub team_id: String,
    /// The model entry the caller addressed.
    #[serde(default)]
    pub requested_model_id: String,
    /// What the caller asked the gateway to do (`chat`, `embeddings`, ...).
    #[serde(default)]
    pub operation: String,
    /// The status the gateway returned to the caller.
    #[serde(default)]
    pub status_code: u16,
    /// The response-cache outcome; `hit` means no upstream served the row.
    #[serde(default)]
    pub cache_status: String,

    /// Prompt tokens as the upstream counted them. OpenAI-shape upstreams
    /// include their cached and cache-written tokens here; Anthropic-shape
    /// ones do not.
    #[serde(default)]
    pub prompt_tokens: u32,
    /// Completion tokens as the upstream counted them.
    #[serde(default)]
    pub completion_tokens: u32,
    /// OpenAI-shape prompt-cache hit: a subset of `prompt_tokens`.
    #[serde(default)]
    pub cached_prompt_tokens: u32,
    /// OpenAI-shape prompt-cache write: a subset of `prompt_tokens`.
    #[serde(default)]
    pub cache_write_tokens: Option<u32>,
    /// Reasoning tokens: a subset of `completion_tokens`, unless the
    /// upstream total says they are counted beside it.
    #[serde(default)]
    pub reasoning_tokens: u32,
    /// The upstream's own total, `0` when it reported none.
    #[serde(default)]
    pub total_tokens: u32,
    /// Anthropic-shape prompt-cache write: on top of `prompt_tokens`.
    #[serde(default)]
    pub cache_creation_tokens: u32,
    /// The part of `cache_creation_tokens` written for five minutes.
    #[serde(default)]
    pub cache_creation_5m_tokens: Option<u32>,
    /// The part of `cache_creation_tokens` written for one hour.
    #[serde(default)]
    pub cache_creation_1h_tokens: Option<u32>,
    /// Anthropic-shape prompt-cache read: on top of `prompt_tokens`.
    #[serde(default)]
    pub cache_read_tokens: u32,
    /// Prompt audio tokens: a subset of `prompt_tokens`, cached part included.
    #[serde(default)]
    pub prompt_audio_tokens: Option<u32>,
    /// Prompt image tokens: a subset of `prompt_tokens`, cached part included.
    #[serde(default)]
    pub prompt_image_tokens: Option<u32>,
    /// Completion audio tokens: a subset of `completion_tokens`.
    #[serde(default)]
    pub completion_audio_tokens: Option<u32>,
    /// Completion image tokens: a subset of `completion_tokens`.
    #[serde(default)]
    pub completion_image_tokens: Option<u32>,
    /// Web searches the provider ran server-side, billed per search.
    #[serde(default)]
    pub web_search_requests: u32,
    /// True when the gateway estimated a counter the upstream did not report.
    #[serde(default)]
    pub usage_estimated: bool,

    /// The tier the upstream reports it served, verbatim.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// The tier the caller's request asked for, verbatim.
    #[serde(default)]
    pub requested_service_tier: Option<String>,
    /// True on the row that records a finished batch job's usage.
    #[serde(default)]
    pub is_batch: bool,
    /// The provider of the model that served the row.
    #[serde(default)]
    pub upstream_provider: Option<String>,
    /// The upstream model name that served the row.
    #[serde(default)]
    pub upstream_model: Option<String>,
    /// The provider key that served the row.
    #[serde(default)]
    pub provider_key_id: Option<String>,
}

/// A row whose counters contradict each other, so no reading of it is the
/// upstream's. It is refused rather than billed at a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inconsistency {
    /// `cached_prompt_tokens` plus `cache_write_tokens`, both subsets of
    /// `prompt_tokens`, exceed it.
    CachedExceedsPrompt,
    /// A write-lifetime split is half reported, or its parts do not add up
    /// to `cache_creation_tokens`.
    CacheWriteSplitMismatch,
    /// `reasoning_tokens` is read as a subset of `completion_tokens` (the
    /// upstream total does not place it beside the completion) and exceeds it.
    ReasoningExceedsCompletion,
}

/// What one row bills, one disjoint quantity per [`Dimension`]: each token
/// the upstream counted is in exactly one of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Quantities {
    pub(crate) input: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
    pub(crate) cache_write_1h: u64,
    pub(crate) output: u64,
    pub(crate) reasoning: u64,
    pub(crate) audio_input: u64,
    pub(crate) audio_output: u64,
    pub(crate) image_input: u64,
    pub(crate) image_output: u64,
    pub(crate) web_search: u64,
}

impl Quantities {
    pub(crate) fn get(&self, d: Dimension) -> u64 {
        match d {
            Dimension::Input => self.input,
            Dimension::CacheRead => self.cache_read,
            Dimension::CacheWrite => self.cache_write,
            Dimension::CacheWrite1h => self.cache_write_1h,
            Dimension::Output => self.output,
            Dimension::Reasoning => self.reasoning,
            Dimension::AudioInput => self.audio_input,
            Dimension::AudioOutput => self.audio_output,
            Dimension::ImageInput => self.image_input,
            Dimension::ImageOutput => self.image_output,
            Dimension::WebSearch => self.web_search,
        }
    }

    pub(crate) fn is_zero(&self) -> bool {
        *self == Quantities::default()
    }

    /// Every prompt token, fresh, cached or written: the prompt size a
    /// long-context band is selected by.
    pub(crate) fn input_total(&self) -> u64 {
        self.input
            + self.cache_read
            + self.cache_write
            + self.cache_write_1h
            + self.audio_input
            + self.image_input
    }
}

/// Splits a row into the quantities it bills. Every counter is widened to
/// `u64` first, so no sum of `u32` counters can overflow.
///
/// 1. The three input buckets come from `aisix_core::input_tokens_for_pricing`.
///    They must hold exactly the prompt plus the two additive counters; when
///    they do not, the subset counters taken out of the prompt exceed it,
///    and the row is refused, never saturated into a guess.
/// 2. The write-lifetime split is either absent, which bills every write as
///    a five-minute one, or present on both sides and adding up to
///    `cache_creation_tokens`.
/// 3. Prompt audio and image tokens include any cached portion. The cache is
///    assumed to hold them first, which favours the tenant whenever a
///    modality token is priced above a text one, and the rest is carved
///    from fresh input, each capped by what is left.
/// 4. Reasoning counted beside the completion
///    (`aisix_core::reasoning_beside_completion`) is added to it. Otherwise
///    it is a subset of it, and a row whose reasoning exceeds the completion
///    is refused.
/// 5. Reasoning, then audio, then image output are carved out of the output
///    total, the modality counts each capped by what is left; the remainder
///    is text output.
pub(crate) fn quantities(u: &BillableUsage) -> Result<Quantities, Inconsistency> {
    let prompt = u64::from(u.prompt_tokens);
    let cached = u64::from(u.cached_prompt_tokens);
    let cache_read = u64::from(u.cache_read_tokens);
    let creation = u64::from(u.cache_creation_tokens);

    let b = input_tokens_for_pricing(
        prompt,
        cached,
        u64::from(u.cache_write_tokens.unwrap_or(0)),
        cache_read,
        creation,
    );
    if b.total() != prompt + cache_read + creation {
        return Err(Inconsistency::CachedExceedsPrompt);
    }
    let one_hour = match (u.cache_creation_5m_tokens, u.cache_creation_1h_tokens) {
        (None, None) => 0,
        (Some(five_m), Some(one_h)) if u64::from(five_m) + u64::from(one_h) == creation => {
            u64::from(one_h)
        }
        _ => return Err(Inconsistency::CacheWriteSplitMismatch),
    };

    let prompt_audio = u64::from(u.prompt_audio_tokens.unwrap_or(0));
    let prompt_image = u64::from(u.prompt_image_tokens.unwrap_or(0));
    let audio_input = prompt_audio.saturating_sub(cached).min(b.uncached);
    let cached_left = cached.saturating_sub(prompt_audio);
    let image_input = prompt_image
        .saturating_sub(cached_left)
        .min(b.uncached - audio_input);

    let completion = u64::from(u.completion_tokens);
    let reasoning = u64::from(u.reasoning_tokens);
    let beside = reasoning_beside_completion(
        u.prompt_tokens,
        u.completion_tokens,
        u.reasoning_tokens,
        u.total_tokens,
    );
    if !beside && reasoning > completion {
        return Err(Inconsistency::ReasoningExceedsCompletion);
    }
    let output_total = completion + if beside { reasoning } else { 0 };
    let mut rest = output_total - reasoning;
    let audio_output = u64::from(u.completion_audio_tokens.unwrap_or(0)).min(rest);
    rest -= audio_output;
    let image_output = u64::from(u.completion_image_tokens.unwrap_or(0)).min(rest);
    rest -= image_output;

    // Each subtraction is of an amount capped by what it is taken from:
    // `reasoning` fits in `output_total` once the refusal above has passed,
    // and `one_hour` is a part of `creation`, which `b.cache_write` contains.
    Ok(Quantities {
        input: b.uncached - audio_input - image_input,
        cache_read: b.cache_read,
        cache_write: b.cache_write - one_hour,
        cache_write_1h: one_hour,
        output: rest,
        reasoning,
        audio_input,
        audio_output,
        image_input,
        image_output,
        web_search: u64::from(u.web_search_requests),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A wire row as the gateway sends it, with the two required fields
    /// filled in when the JSON leaves them out.
    fn row(json: &str) -> BillableUsage {
        let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
        let o = v.as_object_mut().unwrap();
        o.entry("event_id").or_insert_with(|| "e".into());
        o.entry("occurred_at")
            .or_insert_with(|| "2026-11-06T00:00:00Z".into());
        serde_json::from_value(v).unwrap()
    }

    // ---- one test per cache-token shape ----

    #[test]
    fn an_openai_cache_hit_is_a_subset_of_the_prompt() {
        let q = quantities(&row(r#"{"prompt_tokens":1000,"cached_prompt_tokens":800}"#)).unwrap();
        assert_eq!((q.input, q.cache_read, q.cache_write), (200, 800, 0));
    }

    #[test]
    fn an_openai_cache_write_is_a_subset_too() {
        let q = quantities(&row(
            r#"{"prompt_tokens":1000,"cached_prompt_tokens":100,"cache_write_tokens":700}"#,
        ))
        .unwrap();
        assert_eq!((q.input, q.cache_read, q.cache_write), (200, 100, 700));
    }

    #[test]
    fn anthropic_cache_counters_sit_beside_the_prompt() {
        let q = quantities(&row(
            r#"{"prompt_tokens":37,"cache_read_tokens":9,"cache_creation_tokens":4}"#,
        ))
        .unwrap();
        assert_eq!(
            (q.input, q.cache_read, q.cache_write, q.cache_write_1h),
            (37, 9, 4, 0)
        );
    }

    #[test]
    fn a_reported_write_lifetime_split_is_billed_per_lifetime() {
        let q = quantities(&row(r#"{"prompt_tokens":37,"cache_creation_tokens":4,
            "cache_creation_5m_tokens":1,"cache_creation_1h_tokens":3}"#))
        .unwrap();
        assert_eq!((q.cache_write, q.cache_write_1h), (1, 3));
    }

    #[test]
    fn a_gemini_cached_count_is_a_subset_of_the_prompt() {
        let q = quantities(&row(r#"{"prompt_tokens":100,"cached_prompt_tokens":40}"#)).unwrap();
        assert_eq!((q.input, q.cache_read), (60, 40));
    }

    #[test]
    fn openai_reasoning_is_carved_out_of_the_completion() {
        let q = quantities(&row(
            r#"{"prompt_tokens":1000,"completion_tokens":200,"reasoning_tokens":50,"total_tokens":1200}"#,
        ))
        .unwrap();
        assert_eq!((q.output, q.reasoning), (150, 50));
    }

    #[test]
    fn gemini_reasoning_beside_the_completion_is_billed_on_top() {
        let q = quantities(&row(
            r#"{"prompt_tokens":100,"completion_tokens":20,"reasoning_tokens":30,"total_tokens":150}"#,
        ))
        .unwrap();
        assert_eq!((q.output, q.reasoning), (20, 30));
    }

    #[test]
    fn gemini_without_a_total_keeps_the_folded_completion() {
        // With no total, the gateway records the folded completion
        // (recorded_completion_tokens); read it as a subset.
        let q = quantities(&row(
            r#"{"prompt_tokens":100,"completion_tokens":50,"reasoning_tokens":30}"#,
        ))
        .unwrap();
        assert_eq!((q.output, q.reasoning), (20, 30));
    }

    #[test]
    fn cached_input_is_assumed_to_hold_modality_tokens_first() {
        let q = quantities(&row(
            r#"{"prompt_tokens":1000,"cached_prompt_tokens":200,"prompt_audio_tokens":500,
            "completion_tokens":150,"completion_audio_tokens":100}"#,
        ))
        .unwrap();
        assert_eq!(
            (
                q.input,
                q.audio_input,
                q.cache_read,
                q.output,
                q.audio_output
            ),
            (500, 300, 200, 50, 100)
        );
    }

    #[test]
    fn carve_outs_never_bill_more_than_the_upstream_counted() {
        let q = quantities(&row(
            r#"{"completion_tokens":100,"reasoning_tokens":80,"completion_audio_tokens":50}"#,
        ))
        .unwrap();
        assert_eq!((q.reasoning, q.audio_output, q.output), (80, 20, 0));
    }

    #[test]
    fn image_tokens_are_carved_after_audio_and_capped_by_what_is_left() {
        // The cache (300) holds the audio (200) first, then 100 of the 400
        // image tokens; the other 300 image tokens are fresh.
        let q = quantities(&row(
            r#"{"prompt_tokens":1000,"cached_prompt_tokens":300,"prompt_audio_tokens":200,
            "prompt_image_tokens":400}"#,
        ))
        .unwrap();
        assert_eq!(
            (q.input, q.audio_input, q.image_input, q.cache_read),
            (400, 0, 300, 300)
        );
        // Nothing cached: the audio takes 600 fresh tokens, which leaves 400
        // for the 600 image tokens the upstream claims.
        let q = quantities(&row(
            r#"{"prompt_tokens":1000,"prompt_audio_tokens":600,"prompt_image_tokens":600,
            "completion_tokens":40,"completion_audio_tokens":10,"completion_image_tokens":100}"#,
        ))
        .unwrap();
        assert_eq!((q.input, q.audio_input, q.image_input), (0, 600, 400));
        assert_eq!((q.output, q.audio_output, q.image_output), (0, 10, 30));
    }

    #[test]
    fn a_prompt_wholly_cached_or_written_is_consistent() {
        let q = quantities(&row(
            r#"{"prompt_tokens":1000,"cached_prompt_tokens":300,"cache_write_tokens":700}"#,
        ))
        .unwrap();
        assert_eq!((q.input, q.cache_read, q.cache_write), (0, 300, 700));
    }

    #[test]
    fn web_searches_are_counted_per_search() {
        let q = quantities(&row(r#"{"web_search_requests":3}"#)).unwrap();
        assert_eq!(q.web_search, 3);
        assert!(!q.is_zero());
    }

    #[test]
    fn an_impossible_cached_count_is_refused() {
        // Review Focus 3.
        assert_eq!(
            quantities(&row(r#"{"prompt_tokens":100,"cached_prompt_tokens":999}"#)),
            Err(Inconsistency::CachedExceedsPrompt)
        );
        assert_eq!(
            quantities(&row(
                r#"{"prompt_tokens":1000,"cached_prompt_tokens":600,"cache_write_tokens":500}"#
            )),
            Err(Inconsistency::CachedExceedsPrompt)
        );
    }

    #[test]
    fn a_lifetime_split_that_misses_the_total_is_refused() {
        for j in [
            r#"{"cache_creation_tokens":4,"cache_creation_5m_tokens":1,"cache_creation_1h_tokens":2}"#,
            r#"{"cache_creation_tokens":4,"cache_creation_1h_tokens":4}"#,
            r#"{"cache_creation_tokens":4,"cache_creation_5m_tokens":4}"#,
        ] {
            assert_eq!(
                quantities(&row(j)),
                Err(Inconsistency::CacheWriteSplitMismatch),
                "{j}"
            );
        }
    }

    #[test]
    fn a_row_with_no_counters_has_nothing_to_bill() {
        assert!(quantities(&row("{}")).unwrap().is_zero());
    }

    #[test]
    fn reasoning_beyond_the_completion_that_contains_it_is_refused() {
        // No total, and a total that is only prompt + completion: both read
        // the reasoning as part of the completion, which cannot hold it.
        for j in [
            r#"{"prompt_tokens":100,"completion_tokens":20,"reasoning_tokens":30}"#,
            r#"{"prompt_tokens":100,"completion_tokens":20,"reasoning_tokens":30,"total_tokens":120}"#,
        ] {
            assert_eq!(
                quantities(&row(j)),
                Err(Inconsistency::ReasoningExceedsCompletion),
                "{j}"
            );
        }
        // The same counters with a total that places the reasoning beside
        // the completion are consistent.
        let q = quantities(&row(
            r#"{"prompt_tokens":100,"completion_tokens":20,"reasoning_tokens":30,"total_tokens":150}"#,
        ))
        .unwrap();
        assert_eq!((q.output, q.reasoning), (20, 30));
        // A completion that is all reasoning is consistent too.
        let q = quantities(&row(r#"{"completion_tokens":30,"reasoning_tokens":30}"#)).unwrap();
        assert_eq!((q.output, q.reasoning), (0, 30));
    }

    #[test]
    fn a_bedrock_converse_row_bills_its_additive_cache_counters() {
        let q = quantities(&row(
            r#"{"prompt_tokens":100,"cache_read_tokens":900,"cache_creation_tokens":500,
            "cache_creation_5m_tokens":500,"cache_creation_1h_tokens":0,
            "completion_tokens":50,"total_tokens":1550}"#,
        ))
        .unwrap();
        assert_eq!(
            (q.input, q.cache_read, q.cache_write, q.cache_write_1h),
            (100, 900, 500, 0)
        );
        assert_eq!((q.output, q.reasoning), (50, 0));
    }

    // ---- decoding ----

    #[test]
    fn a_counter_that_is_not_a_u32_does_not_decode() {
        let decode = |field: &str, value: &str| {
            serde_json::from_str::<BillableUsage>(&format!(
                r#"{{"event_id":"e","occurred_at":"2026-11-06T00:00:00Z","{field}":{value}}}"#
            ))
        };
        let counters = [
            "prompt_tokens",
            "cached_prompt_tokens",
            "web_search_requests",
        ];
        let breakdowns = ["cache_write_tokens", "prompt_audio_tokens"];
        let not_a_u32 = ["-1", "4294967296", "18446744073709551616", "1.0", r#""10""#];
        for field in counters.iter().chain(&breakdowns) {
            let u = decode(field, "4294967295").unwrap();
            let read = match *field {
                "prompt_tokens" => Some(u.prompt_tokens),
                "cached_prompt_tokens" => Some(u.cached_prompt_tokens),
                "web_search_requests" => Some(u.web_search_requests),
                "cache_write_tokens" => u.cache_write_tokens,
                _ => u.prompt_audio_tokens,
            };
            assert_eq!(read, Some(u32::MAX), "{field}");
            for value in not_a_u32 {
                assert!(decode(field, value).is_err(), "{field}: {value}");
            }
        }
        // A plain counter has no "unreported" state for `null` to mean.
        for field in counters {
            assert!(decode(field, "null").is_err(), "{field}: null");
        }
    }

    #[test]
    fn a_null_breakdown_reads_as_unreported() {
        let nulls = row(
            r#"{"prompt_tokens":100,"cache_creation_tokens":4,"completion_tokens":10,
            "cache_write_tokens":null,"cache_creation_5m_tokens":null,
            "cache_creation_1h_tokens":null,"prompt_audio_tokens":null,
            "prompt_image_tokens":null,"completion_audio_tokens":null,
            "completion_image_tokens":null,"service_tier":null,"requested_service_tier":null,
            "upstream_provider":null,"upstream_model":null,"provider_key_id":null}"#,
        );
        let absent =
            row(r#"{"prompt_tokens":100,"cache_creation_tokens":4,"completion_tokens":10}"#);
        assert_eq!(nulls, absent);
        assert_eq!(quantities(&nulls), quantities(&absent));
    }

    #[test]
    fn a_wire_row_with_fields_this_engine_does_not_read_still_deserializes() {
        let u = row(
            r#"{"prompt_tokens":10,"completion_tokens":5,"cost_usd":0.0125,
            "guardrail_monitor_hits":[{"guardrail_name":"g","hook":"input","outcome":"would_block"}],
            "audio_duration_seconds":12.5,"a_field_no_gateway_sends_yet":{"nested":[1,2]}}"#,
        );
        assert_eq!((u.prompt_tokens, u.completion_tokens), (10, 5));
    }

    #[test]
    fn a_row_without_event_id_or_occurred_at_does_not_deserialize() {
        let both = r#"{"event_id":"e","occurred_at":"2026-11-06T00:00:00Z"}"#;
        assert!(serde_json::from_str::<BillableUsage>(both).is_ok());
        for j in [
            r#"{"occurred_at":"2026-11-06T00:00:00Z"}"#,
            r#"{"event_id":"e"}"#,
        ] {
            assert!(serde_json::from_str::<BillableUsage>(j).is_err(), "{j}");
        }
    }

    /// Every field is read under the name the gateway writes it with. A
    /// misspelt name would not fail to decode: it would default, and bill
    /// nothing for what it carries. Each value is distinct, so a field read
    /// from its neighbour's name fails too.
    #[test]
    fn a_gateway_usage_event_reads_field_for_field() {
        let event = aisix_obs::usage::UsageEvent {
            event_id: "e-1".into(),
            occurred_at: "2026-11-06T01:02:03.456Z".into(),
            team_id: "team".into(),
            requested_model_id: "model".into(),
            operation: "chat".into(),
            status_code: 201,
            cache_status: "miss".into(),
            prompt_tokens: 1,
            completion_tokens: 2,
            cached_prompt_tokens: 3,
            cache_write_tokens: Some(4),
            reasoning_tokens: 5,
            total_tokens: 6,
            cache_creation_tokens: 7,
            cache_creation_ttl: aisix_gateway::CacheCreationTtl {
                cache_creation_5m_tokens: Some(8),
                cache_creation_1h_tokens: Some(9),
            },
            cache_read_tokens: 10,
            modality_tokens: aisix_gateway::ModalityTokens {
                prompt_audio_tokens: Some(11),
                prompt_image_tokens: Some(12),
                completion_audio_tokens: Some(13),
                completion_image_tokens: Some(14),
            },
            web_search_requests: 15,
            usage_estimated: true,
            service_tier: Some("flex".into()),
            requested_service_tier: Some("priority".into()),
            is_batch: true,
            upstream_provider: Some("provider".into()),
            upstream_model: Some("upstream-model".into()),
            provider_key_id: Some("pk".into()),
            cost_usd: 0.5,
            ..Default::default()
        };
        let read: BillableUsage =
            serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        assert_eq!(
            read,
            BillableUsage {
                event_id: "e-1".into(),
                occurred_at: "2026-11-06T01:02:03.456Z".parse().unwrap(),
                team_id: "team".into(),
                requested_model_id: "model".into(),
                operation: "chat".into(),
                status_code: 201,
                cache_status: "miss".into(),
                prompt_tokens: 1,
                completion_tokens: 2,
                cached_prompt_tokens: 3,
                cache_write_tokens: Some(4),
                reasoning_tokens: 5,
                total_tokens: 6,
                cache_creation_tokens: 7,
                cache_creation_5m_tokens: Some(8),
                cache_creation_1h_tokens: Some(9),
                cache_read_tokens: 10,
                prompt_audio_tokens: Some(11),
                prompt_image_tokens: Some(12),
                completion_audio_tokens: Some(13),
                completion_image_tokens: Some(14),
                web_search_requests: 15,
                usage_estimated: true,
                service_tier: Some("flex".into()),
                requested_service_tier: Some("priority".into()),
                is_batch: true,
                upstream_provider: Some("provider".into()),
                upstream_model: Some("upstream-model".into()),
                provider_key_id: Some("pk".into()),
            }
        );
    }

    /// Every counter the gateway keeps for an upstream call is either read
    /// here or named as not billed, with the reason. The patterns are
    /// exhaustive, so a counter the gateway adds fails to compile here until
    /// someone decides which it is.
    #[test]
    fn every_gateway_usage_counter_is_classified() {
        let aisix_gateway::UsageStats {
            // Read: each reaches the usage row under the name BillableUsage
            // reads.
            prompt_tokens: _,
            completion_tokens: _,
            cached_prompt_tokens: _,
            cache_write_tokens: _,
            reasoning_tokens: _,
            cache_creation_tokens: _,
            cache_read_tokens: _,
            web_search_requests: _,
            // Recorded as the row's `total_tokens`.
            upstream_total_tokens: _,
            // Taken back out of the row's `completion_tokens` when the
            // upstream total shows the reasoning sat beside it
            // (`recorded_completion_tokens`), so the row is in the
            // upstream's own shape.
            reasoning_folded_into_completion: _,
            modality,
            cache_creation_ttl,
            // Not billed: the gateway's own total, which it may compute;
            // the row carries the upstream's instead.
            total_tokens: _,
            // Not billed: the provider charges nothing for a fetch; the
            // fetched content bills as input tokens.
            web_fetch_requests: _,
            // Not billed: one provider's native names for the hit and the
            // miss, kept for clients that read them. The hit is already
            // `cached_prompt_tokens`, and the two add up to `prompt_tokens`
            // (<https://api-docs.deepseek.com/api/create-chat-completion>).
            prompt_cache_hit_tokens: _,
            prompt_cache_miss_tokens: _,
            openai_details,
        } = aisix_gateway::UsageStats::default();
        let aisix_gateway::ModalityTokens {
            // Read, all four.
            prompt_audio_tokens: _,
            prompt_image_tokens: _,
            completion_audio_tokens: _,
            completion_image_tokens: _,
        } = modality;
        let aisix_gateway::CacheCreationTtl {
            // Read, both.
            cache_creation_5m_tokens: _,
            cache_creation_1h_tokens: _,
        } = cache_creation_ttl;
        let aisix_gateway::OpenAiUsageDetails {
            // Not billed: the client-facing projection only, never on the
            // row. The two flags mark presence; the text counts are each
            // side less its audio and image tokens, which `quantities` works
            // out itself; and predicted-output tokens, accepted or rejected,
            // are counted in `completion_tokens` (`openai-python`:
            // `src/openai/types/completion_usage.py`).
            cached_tokens_reported: _,
            reasoning_tokens_reported: _,
            prompt_text_tokens: _,
            completion_text_tokens: _,
            accepted_prediction_tokens: _,
            rejected_prediction_tokens: _,
        } = openai_details;
    }

    // ---- Quantities ----

    #[test]
    fn each_dimension_reads_its_own_quantity() {
        let z = Quantities::default();
        let singles = [
            (Dimension::Input, Quantities { input: 1, ..z }),
            (Dimension::CacheRead, Quantities { cache_read: 1, ..z }),
            (
                Dimension::CacheWrite,
                Quantities {
                    cache_write: 1,
                    ..z
                },
            ),
            (
                Dimension::CacheWrite1h,
                Quantities {
                    cache_write_1h: 1,
                    ..z
                },
            ),
            (Dimension::Output, Quantities { output: 1, ..z }),
            (Dimension::Reasoning, Quantities { reasoning: 1, ..z }),
            (
                Dimension::AudioInput,
                Quantities {
                    audio_input: 1,
                    ..z
                },
            ),
            (
                Dimension::AudioOutput,
                Quantities {
                    audio_output: 1,
                    ..z
                },
            ),
            (
                Dimension::ImageInput,
                Quantities {
                    image_input: 1,
                    ..z
                },
            ),
            (
                Dimension::ImageOutput,
                Quantities {
                    image_output: 1,
                    ..z
                },
            ),
            (Dimension::WebSearch, Quantities { web_search: 1, ..z }),
        ];
        assert!(z.is_zero());
        assert_eq!(singles.len(), Dimension::ALL.len());
        for (d, q) in singles {
            assert!(!q.is_zero(), "{d:?}");
            for other in Dimension::ALL {
                assert_eq!(
                    q.get(other),
                    u64::from(other == d),
                    "{d:?} read as {other:?}"
                );
            }
        }
    }

    #[test]
    fn the_input_total_is_the_six_input_side_buckets() {
        let q = Quantities {
            input: 1,
            cache_read: 2,
            cache_write: 4,
            cache_write_1h: 8,
            output: 16,
            reasoning: 32,
            audio_input: 64,
            audio_output: 128,
            image_input: 256,
            image_output: 512,
            web_search: 1024,
        };
        assert_eq!(q.input_total(), 1 + 2 + 4 + 8 + 64 + 256);
    }

    // ---- properties over consistent rows ----

    /// A counter: small ones meet at the caps and boundaries, large ones
    /// reach toward `u32::MAX`.
    fn count() -> impl Strategy<Value = u32> {
        prop_oneof![0u32..=64, any::<u32>()]
    }

    /// `raw` folded into `0..=max`.
    fn within(raw: u32, max: u32) -> u32 {
        u32::try_from(u64::from(raw) % (u64::from(max) + 1)).unwrap()
    }

    prop_compose! {
        /// A row `quantities` accepts: the subset counters fit inside the
        /// prompt, reasoning read as part of the completion fits inside it,
        /// and a write-lifetime split, when present, adds up to the write.
        /// Every other counter is arbitrary, and the upstream total is absent,
        /// the subset identity, the additive identity, or anything.
        fn consistent_row()(
            (prompt, cached, write) in (count(), count(), proptest::option::of(count())),
            (cache_read, creation, split) in (count(), count(), proptest::option::of(count())),
            (completion, reasoning, total_kind, total) in (count(), count(), 0u8..4, count()),
            modality in proptest::array::uniform4(proptest::option::of(count())),
            web_search in count(),
        ) -> BillableUsage {
            let cached = within(cached, prompt);
            let write = write.map(|w| within(w, prompt - cached));
            let split = split.map(|s| within(s, creation));
            let (p, c, r) = (prompt, completion, reasoning);
            let total = match total_kind {
                0 => 0,
                1 => p.checked_add(c).unwrap_or(0),
                2 => p.checked_add(c).and_then(|pc| pc.checked_add(r)).unwrap_or(0),
                _ => total,
            };
            let reasoning = if reasoning_beside_completion(p, c, r, total) {
                r
            } else {
                within(r, c)
            };
            BillableUsage {
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
                ..BillableUsage::default()
            }
        }
    }

    proptest! {
        #[test]
        fn billed_input_adds_up_to_what_the_gateway_splits(u in consistent_row()) {
            let q = quantities(&u).unwrap();
            let split = input_tokens_for_pricing(
                u64::from(u.prompt_tokens),
                u64::from(u.cached_prompt_tokens),
                u64::from(u.cache_write_tokens.unwrap_or(0)),
                u64::from(u.cache_read_tokens),
                u64::from(u.cache_creation_tokens),
            );
            prop_assert_eq!(
                q.input + q.cache_read + q.cache_write + q.cache_write_1h + q.audio_input + q.image_input,
                split.total()
            );
            prop_assert_eq!(q.input_total(), split.total());
            // Bucket by bucket, not only in sum: no token moves between the
            // gateway's three buckets.
            prop_assert_eq!(q.input + q.audio_input + q.image_input, split.uncached);
            prop_assert_eq!(q.cache_read, split.cache_read);
            prop_assert_eq!(q.cache_write + q.cache_write_1h, split.cache_write);
            // And the wire's own identity: the prompt plus the two additive
            // counters, whichever shape filled them.
            prop_assert_eq!(
                split.total(),
                u64::from(u.prompt_tokens) + u64::from(u.cache_read_tokens) + u64::from(u.cache_creation_tokens)
            );
        }

        #[test]
        fn billed_output_adds_up_to_the_output_total(u in consistent_row()) {
            let q = quantities(&u).unwrap();
            let beside = reasoning_beside_completion(
                u.prompt_tokens, u.completion_tokens, u.reasoning_tokens, u.total_tokens,
            );
            let reasoning = if beside { u64::from(u.reasoning_tokens) } else { 0 };
            prop_assert_eq!(
                q.output + q.reasoning + q.audio_output + q.image_output,
                u64::from(u.completion_tokens) + reasoning
            );
        }
    }
}
