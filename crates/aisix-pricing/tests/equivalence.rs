//! The engine charges what the gateway's own spend computation says.
//!
//! The gateway prices a request from the price on its model,
//! `spend_micro_usd(ModelCost::calculate_with_cache(input_tokens_for_pricing(..), completion))`,
//! on the completion it holds after folding any separately reported
//! reasoning in (`request_cost_usd` in `aisix-proxy/src/usage_attr.rs`, as
//! `chat.rs` calls it). Root `CLAUDE.md` requires a control plane that
//! recomputes spend to use the same reading and the same formula. These tests
//! hold `price_event` to that: for a rate card that says what a `ModelCost`
//! says, the engine's charge on the gateway's own `UsageEvent`, read off the
//! wire, is the gateway's spend, to within the rounding bound below.
//!
//! # The card a `ModelCost` is
//!
//! `book_for` puts the cost's rates on a `default` base and nothing else:
//! `input`, `output`, and `cache_read` / `cache_write` only where the cost has
//! them. Every dimension a generated row bills then has a rate (ruling
//! P2-R15), through the fallbacks a rate set shares with `ModelCost`:
//!
//! - cache reads and cache writes fall back to `input`, as `ModelCost` does;
//! - the one-hour write falls back to `cache_write`, then `input`:
//!   `ModelCost` has one write rate for both lifetimes;
//! - reasoning, audio output and image output fall back to `output`, and
//!   audio and image input to `input`: the gateway prices them inside the
//!   completion and the fresh prompt that contain them.
//!
//! The rows carry no web searches. `ModelCost` has no per-search rate and the
//! gateway's spend leaves searches out, so a row with searches has no
//! equivalent card; the engine refuses it rather than bill its tokens alone.
//! A rate here is a whole µ$ per million tokens, which is `rate / 10⁹` USD per
//! 1,000; a `ModelCost` priced more finely than that has no exact card either.
//!
//! # The rows
//!
//! Consistent rows only (ruling P2-R10): the subset counters fit inside what
//! contains them, and a write-lifetime split adds up to the write. On an
//! inconsistent row the engine refuses and the gateway saturates, and neither
//! reading is the upstream's.
//!
//! # The bound
//!
//! Let `X = Σ quantity × rate / 10⁶` µ$, the exact amount. The engine charges
//! `⌈X⌉`, in integers. The gateway charges `round(X + ε)`, rounding half away
//! from zero, where `ε` is the error of evaluating `X` in `f64`: four rate
//! conversions, four products, three additions, `÷ 1000` and `× 10⁶`, at most
//! seven roundings of 2⁻⁵³ each on any term, so `|ε| ≤ 7 × 2⁻⁵³ × X` to first
//! order. Its
//! `max(0.0)` and its saturating `as u64` never act on a non-negative amount
//! this size.
//!
//! While `|ε| < ½`, `round(X + ε)` lies in `(X − 1, X + 1)` and `⌈X⌉` in
//! `[X, X + 1)`, so `engine − gateway` is an integer in `(−1, 2)`: **0 or 1**.
//! The engine is never below the gateway and at most 1 µ$ above it. That is
//! one-sided where `|engine − gateway| ≤ 1` is not, and the difference matters:
//! an engine that rounded down would pass the symmetric bound.
//!
//! The generated rows stay at or below `X = 8 × 10⁸` µ$ (at most 8,000,000
//! tokens on a row, at most USD 100 per million), so `|ε| < 10⁻⁶` µ$. The
//! bound holds up to `X ≈ 6 × 10¹⁴` µ$, USD 600 million on one row.

use std::sync::Arc;

use aisix_core::{input_tokens_for_pricing, spend_micro_usd, ModelCost};
use aisix_gateway::chat::recorded_completion_tokens;
use aisix_gateway::{CacheCreationTtl, ModalityTokens};
use aisix_obs::UsageEvent;
use aisix_pricing::{
    price_event, BillableUsage, Catalogue, ChargeLine, Dimension, Exemption, Outcome, PriceBook,
    PriceRows, PricedEvent, PricingError, Rate, RateCard, Rates, SellPriceRow, ServiceTier,
    TierRates, ENGINE_VERSION,
};
use chrono::{DateTime, Utc};
use proptest::option;
use proptest::prelude::*;

/// 2026-11-06T00:00:01Z: when the control plane received the rows, one
/// second after they occurred.
const RECEIVED: DateTime<Utc> = match DateTime::from_timestamp(1_793_923_201, 0) {
    Some(t) => t,
    None => panic!("a representable instant"),
};

/// The largest counter a generated row carries.
const MAX_COUNT: u32 = 2_000_000;

/// The largest generated rate: USD 100 per million tokens.
const MAX_RATE: u64 = 100_000_000;

/// A gateway `ModelCost`, with each rate in µ$ per million tokens.
#[derive(Debug, Clone, Copy)]
struct GatewayPrice {
    input: u64,
    output: u64,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

impl GatewayPrice {
    /// The `ModelCost` that carries these rates, in USD per 1,000 tokens.
    fn model_cost(&self) -> ModelCost {
        let per_1k = |r: u64| r as f64 / 1e9;
        ModelCost {
            input_per_1k: per_1k(self.input),
            output_per_1k: per_1k(self.output),
            cached_input_per_1k: self.cache_read.map(per_1k),
            cache_write_per_1k: self.cache_write.map(per_1k),
        }
    }

    /// The same rates as a rate set, with nothing a `ModelCost` cannot say.
    fn rates(&self) -> Rates {
        Rates {
            input: Rate(self.input),
            output: Rate(self.output),
            cache_read: self.cache_read.map(Rate),
            cache_write: self.cache_write.map(Rate),
            cache_write_1h: None,
            reasoning: None,
            audio_input: None,
            audio_output: None,
            image_input: None,
            image_output: None,
            web_search: None,
        }
    }
}

/// One upstream call's counters as the gateway holds them (`UsageStats`):
/// what its spend reads, and what its usage event records.
#[derive(Debug, Clone, Copy, Default)]
struct GatewayUsage {
    prompt: u32,
    cached: u32,
    write_subset: Option<u32>,
    cache_read: u32,
    creation: u32,
    ttl: CacheCreationTtl,
    modality: ModalityTokens,
    /// The completion with any separately reported reasoning folded in,
    /// which is what the gateway's spend prices.
    completion_folded: u32,
    reasoning: u32,
    reasoning_folded: u32,
    upstream_total: u32,
}

/// The gateway's spend on `g`, by the call chain its chat handler runs.
fn gateway_spend(g: &GatewayUsage, p: &GatewayPrice) -> u64 {
    let input = input_tokens_for_pricing(
        u64::from(g.prompt),
        u64::from(g.cached),
        u64::from(g.write_subset.unwrap_or(0)),
        u64::from(g.cache_read),
        u64::from(g.creation),
    );
    spend_micro_usd(
        p.model_cost()
            .calculate_with_cache(input, u64::from(g.completion_folded)),
    )
}

/// The usage event the gateway emits for `g`: the completion it records
/// has the folded reasoning taken back out where the upstream total shows it
/// was reported beside (`recorded_completion_tokens`).
fn usage_event(g: &GatewayUsage, cache_status: &str) -> UsageEvent {
    UsageEvent {
        event_id: "e".into(),
        occurred_at: "2026-11-06T00:00:00.000Z".into(),
        team_id: "t".into(),
        requested_model_id: "m".into(),
        operation: "chat".into(),
        status_code: 200,
        cache_status: cache_status.into(),
        prompt_tokens: g.prompt,
        completion_tokens: recorded_completion_tokens(
            g.prompt,
            g.completion_folded,
            g.reasoning,
            g.reasoning_folded,
            g.upstream_total,
        ),
        reasoning_tokens: g.reasoning,
        total_tokens: g.upstream_total,
        cached_prompt_tokens: g.cached,
        cache_write_tokens: g.write_subset,
        cache_read_tokens: g.cache_read,
        cache_creation_tokens: g.creation,
        cache_creation_ttl: g.ttl,
        modality_tokens: g.modality,
        ..Default::default()
    }
}

/// Model `m` at `rates` on a `default` base, with no other tier, band,
/// tenant plan or cost price.
fn book(rates: Rates) -> PriceBook {
    let card = RateCard(
        [(
            ServiceTier::default_tier(),
            TierRates {
                base: rates,
                long_context: vec![],
            },
        )]
        .into(),
    );
    let catalogue = Catalogue::from_json(r#"{"version":"2026-11-06","entries":[]}"#).unwrap();
    let book = PriceBook::from_rows(
        PriceRows {
            sell: vec![SellPriceRow {
                revision_id: 1,
                engine_version: ENGINE_VERSION,
                model_id: "m".into(),
                tenant_id: None,
                effective_from: "2026-01-01T00:00:00Z".parse().unwrap(),
                card: Some(serde_json::to_value(&card).unwrap()),
            }],
            cost: vec![],
            discounts: vec![],
        },
        Arc::new(catalogue),
    );
    let refused = book.invalid_revisions();
    assert!(refused.is_empty(), "the book refused the card: {refused:?}");
    book
}

/// The card that says what `p` says.
fn book_for(p: &GatewayPrice) -> PriceBook {
    book(p.rates())
}

/// The engine's price for `event`, read off the wire as the control plane
/// stores it.
fn engine_price(book: &PriceBook, event: &UsageEvent) -> Result<PricedEvent, PricingError> {
    let wire = serde_json::to_value(event).unwrap();
    let usage: BillableUsage = serde_json::from_value(wire).unwrap();
    price_event(book, &usage, RECEIVED)
}

fn charge(priced: &PricedEvent) -> u64 {
    u64::try_from(priced.charge.0).unwrap()
}

// ---- generators ----

/// A counter: small ones meet zeros and caps, large ones reach `MAX_COUNT`.
fn count() -> impl Strategy<Value = u32> {
    prop_oneof![0u32..=64, 0u32..=MAX_COUNT]
}

/// `raw` folded into `0..=max`.
fn within(raw: u32, max: u32) -> u32 {
    raw % (max + 1)
}

/// Modality counters, each a subset of its side: prompt audio and image
/// share `prompt`, completion audio and image share `completion`.
fn modality(raw: [Option<u32>; 4], prompt: u32, completion: u32) -> ModalityTokens {
    let audio_in = raw[0].map(|a| within(a, prompt));
    let audio_out = raw[2].map(|a| within(a, completion));
    ModalityTokens {
        prompt_audio_tokens: audio_in,
        prompt_image_tokens: raw[1].map(|i| within(i, prompt - audio_in.unwrap_or(0))),
        completion_audio_tokens: audio_out,
        completion_image_tokens: raw[3].map(|i| within(i, completion - audio_out.unwrap_or(0))),
    }
}

prop_compose! {
    /// OpenAI shape: the cache hit and the cache write are subsets of the
    /// prompt, reasoning is a subset of the completion and nothing is folded.
    /// The total is the upstream's `prompt + completion`, or unreported.
    /// Gemini's row is this shape too when its candidates contain its
    /// thoughts.
    fn openai()(
        (prompt, cached, write) in (count(), count(), option::of(count())),
        (completion, reasoning) in (count(), count()),
        raw_modality in proptest::array::uniform4(option::of(count())),
        reports_total in any::<bool>(),
    ) -> GatewayUsage {
        let cached = within(cached, prompt);
        GatewayUsage {
            prompt,
            cached,
            write_subset: write.map(|w| within(w, prompt - cached)),
            modality: modality(raw_modality, prompt, completion),
            completion_folded: completion,
            reasoning: within(reasoning, completion),
            upstream_total: if reports_total { prompt + completion } else { 0 },
            ..GatewayUsage::default()
        }
    }
}

prop_compose! {
    /// Anthropic shape: cache reads and writes are counters beside the
    /// prompt, with an optional five-minute / one-hour split that adds up to
    /// a non-zero write. The total is unreported, or the cache-inclusive one
    /// Bedrock's Converse reports.
    fn anthropic()(
        (prompt, cache_read, creation) in (count(), count(), count()),
        split in option::of(count()),
        completion in count(),
        reports_total in any::<bool>(),
    ) -> GatewayUsage {
        let ttl = match split.filter(|_| creation > 0) {
            Some(s) => {
                let five_m = within(s, creation);
                CacheCreationTtl {
                    cache_creation_5m_tokens: Some(five_m),
                    cache_creation_1h_tokens: Some(creation - five_m),
                }
            }
            None => CacheCreationTtl::default(),
        };
        let total = prompt + cache_read + creation + completion;
        GatewayUsage {
            prompt,
            cache_read,
            creation,
            ttl,
            completion_folded: completion,
            upstream_total: if reports_total { total } else { 0 },
            ..GatewayUsage::default()
        }
    }
}

prop_compose! {
    /// Gemini shape, where the candidates do not contain the thoughts: the
    /// cache hit is a subset of the prompt, and the gateway folds the
    /// thoughts into the completion (`reasoning_folded == reasoning > 0`;
    /// `into_usage_stats` in `aisix-provider-vertex/src/bridge.rs`). The
    /// total is Gemini's own `prompt + candidates + thoughts`, one that has
    /// not caught up with the thoughts yet (`prompt + candidates`, a streamed
    /// frame mid-thinking), or unreported. Where the candidates do contain
    /// the thoughts, nothing is folded and the row is the OpenAI shape.
    fn gemini()(
        (prompt, cached) in (count(), count()),
        (candidates, thoughts) in (count(), count()),
        raw_modality in proptest::array::uniform4(option::of(count())),
        total_kind in 0u8..3,
    ) -> GatewayUsage {
        let reasoning = thoughts.max(1);
        let completion_folded = candidates + reasoning;
        GatewayUsage {
            prompt,
            cached: within(cached, prompt),
            modality: modality(raw_modality, prompt, candidates),
            completion_folded,
            reasoning,
            reasoning_folded: reasoning,
            upstream_total: match total_kind {
                0 => 0,
                1 => prompt + completion_folded,
                _ => prompt + candidates,
            },
            ..GatewayUsage::default()
        }
    }
}

fn gateway_usage() -> impl Strategy<Value = GatewayUsage> {
    prop_oneof![openai(), anthropic(), gemini()]
}

prop_compose! {
    /// A `ModelCost` up to USD 100 per million tokens, its two cache rates
    /// each optional.
    fn gateway_price()(
        (input, output) in (0..=MAX_RATE, 0..=MAX_RATE),
        cache_read in option::of(0..=MAX_RATE),
        cache_write in option::of(0..=MAX_RATE),
    ) -> GatewayPrice {
        GatewayPrice { input, output, cache_read, cache_write }
    }
}

proptest! {
    /// The engine rounds up where the gateway rounds half up, so it is never
    /// below the gateway and at most 1 µ$ above it (see the module docs). A
    /// difference in the formula, on counts in the millions, is far more.
    #[test]
    fn the_engine_charges_what_the_gateway_spends(g in gateway_usage(), p in gateway_price()) {
        let gateway = gateway_spend(&g, &p);
        let priced = engine_price(&book_for(&p), &usage_event(&g, "miss"))
            .map_err(|e| TestCaseError::fail(format!("refused a consistent row: {e:?}")))?;
        let engine = charge(&priced);
        prop_assert!(
            matches!(engine.checked_sub(gateway), Some(0 | 1)),
            "engine {} gateway {}", engine, gateway
        );
    }
}

// ---- rows computed by hand ----

/// Per token: input 1 µ$, output 2 µ$, a cache read 0.1 µ$, a cache write
/// 1.25 µ$.
const LIST: GatewayPrice = GatewayPrice {
    input: 1_000_000,
    output: 2_000_000,
    cache_read: Some(100_000),
    cache_write: Some(1_250_000),
};

/// 500 fresh + 300 read × 0.1 + 200 written × 1.25 + 500 out × 2
/// = 500 + 30 + 250 + 1000 = 1780 µ$. The 100 reasoning tokens are part of
/// the 500.
fn openai_row() -> GatewayUsage {
    GatewayUsage {
        prompt: 1000,
        cached: 300,
        write_subset: Some(200),
        completion_folded: 500,
        reasoning: 100,
        upstream_total: 1500,
        ..GatewayUsage::default()
    }
}

/// A write split `five_m` / `one_h`, beside a 100-token prompt, 2000 cache
/// reads and 50 completion tokens.
fn anthropic_row(five_m: u32, one_h: u32) -> GatewayUsage {
    GatewayUsage {
        prompt: 100,
        cache_read: 2000,
        creation: five_m + one_h,
        ttl: CacheCreationTtl {
            cache_creation_5m_tokens: Some(five_m),
            cache_creation_1h_tokens: Some(one_h),
        },
        completion_folded: 50,
        ..GatewayUsage::default()
    }
}

/// 600 fresh + 400 read × 0.1 + (200 candidates + 300 thoughts) × 2
/// = 600 + 40 + 1000 = 1640 µ$, with the upstream total `upstream_total`.
fn gemini_row(upstream_total: u32) -> GatewayUsage {
    GatewayUsage {
        prompt: 1000,
        cached: 400,
        completion_folded: 500,
        reasoning: 300,
        reasoning_folded: 300,
        upstream_total,
        ..GatewayUsage::default()
    }
}

#[test]
fn the_engine_and_the_gateway_agree_exactly_on_whole_micro_dollar_rows() {
    let book = book_for(&LIST);
    let rows = [
        ("openai", openai_row(), 1780),
        // 100 + 2000 × 0.1 + 400 × 1.25 + 50 × 2 = 100 + 200 + 500 + 100.
        ("anthropic", anthropic_row(100, 300), 900),
        // Recorded as 200 candidates beside 300 reasoning.
        ("gemini with its total", gemini_row(1500), 1640),
        // Recorded as the folded 500, the 300 reasoning inside it.
        ("gemini without a total", gemini_row(0), 1640),
    ];
    for (shape, g, amount) in rows {
        assert_eq!(gateway_spend(&g, &LIST), amount, "{shape}: gateway");
        let priced = engine_price(&book, &usage_event(&g, "miss")).unwrap();
        assert_eq!(
            (priced.outcome, charge(&priced)),
            (Outcome::Charged, amount),
            "{shape}: engine"
        );
    }

    // A response-cache hit reports the replayed call's tokens: here the
    // OpenAI row's, which bill 1780 µ$ as a miss above. The gateway's spend
    // on a hit is 0 whatever it reports:
    // `CacheTelemetry::cost_usd` (`aisix-proxy/src/response_cache.rs`)
    // returns 0.0 before it prices anything. It is crate-private there, so
    // its answer is written out here.
    let gateway_on_a_hit = 0;
    let hit = engine_price(&book, &usage_event(&openai_row(), "hit")).unwrap();
    assert_eq!(
        (hit.outcome, charge(&hit)),
        (
            Outcome::Exempt(Exemption::ResponseCacheHit),
            gateway_on_a_hit
        )
    );
}

/// `ModelCost` has one write rate for both cache lifetimes, and a card can
/// price the one-hour write apart (one provider publishes it at 2x input
/// against 1.25x for five minutes; see `Dimension::CacheWrite`). With it set,
/// the engine charges the one-hour tokens at it and the gateway cannot, so on
/// a row whose amount is otherwise whole the engine is exactly
/// `⌈h1 × (rate_1h − rate_write) / 10⁶⌉` µ$ above the gateway. This is
/// intended: the card says more than the gateway's price can.
#[test]
fn a_one_hour_write_rate_is_where_the_engine_departs_from_the_gateway() {
    let (rate_write, rate_1h) = (1_250_000, 2_000_000);
    assert_eq!(LIST.cache_write, Some(rate_write));
    let one_hour_rate = book(Rates {
        cache_write_1h: Some(Rate(rate_1h)),
        ..LIST.rates()
    });
    let one_write_rate = book_for(&LIST);
    // 100 + 200 + 4 × 1.25 + 100 = 405, and 3 × 0.75 = 2.25 rounds up to 3.
    for (five_m, one_h, gateway_amount, departure) in [(100, 300, 900, 225), (1, 3, 405, 3)] {
        let g = anthropic_row(five_m, one_h);
        let event = usage_event(&g, "miss");
        assert_eq!(gateway_spend(&g, &LIST), gateway_amount);
        assert_eq!(
            (u64::from(one_h) * (rate_1h - rate_write)).div_ceil(1_000_000),
            departure
        );
        // Without the one-hour rate, the write rate covers both lifetimes,
        // as the gateway's does.
        let same = engine_price(&one_write_rate, &event).unwrap();
        assert_eq!(charge(&same), gateway_amount, "{one_h} one-hour tokens");
        let apart = engine_price(&one_hour_rate, &event).unwrap();
        assert_eq!(
            charge(&apart),
            gateway_amount + departure,
            "{one_h} one-hour tokens"
        );
    }
}

/// An OpenAI-shape row with nothing folded, whose upstream total is
/// `prompt + completion + reasoning` (100 + 20 + 30 = 150). The control
/// plane's rule (`aisix_core::reasoning_beside_completion`) reads that total
/// as reasoning counted beside the completion and bills 20 + 30 output
/// tokens: 100 + 50 × 2 = 200 µ$. The gateway's `ModelCost` prices the
/// completion it holds, 20, with the reasoning read as inside it:
/// 100 + 20 × 2 = 140 µ$. The two read this row differently on purpose: the
/// control plane is the billing authority, and the gateway's price is a
/// routing hint and a local signal.
#[test]
fn reasoning_that_meets_the_additive_identity_is_billed_beside() {
    let g = GatewayUsage {
        prompt: 100,
        completion_folded: 20,
        reasoning: 30,
        reasoning_folded: 0,
        upstream_total: 150,
        ..GatewayUsage::default()
    };
    let event = usage_event(&g, "miss");
    assert_eq!(event.completion_tokens, 20, "nothing was folded");
    let priced = engine_price(&book_for(&LIST), &event).unwrap();
    let line = |dimension, quantity, rate| ChargeLine {
        dimension,
        quantity,
        rate: Rate(rate),
    };
    assert_eq!(
        priced.lines,
        [
            line(Dimension::Input, 100, 1_000_000),
            line(Dimension::Output, 20, 2_000_000),
            line(Dimension::Reasoning, 30, 2_000_000),
        ]
    );
    assert_eq!(charge(&priced), 200);
    assert_eq!(gateway_spend(&g, &LIST), 140);
}
