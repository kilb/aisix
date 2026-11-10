//! Golden wire rows: the usage rows a real gateway sends, one file per
//! provider family, priced by the engine and held to the gateway's own
//! spend figure.
//!
//! - `golden/wire/<family>.json` is written by the gateway e2e
//!   `tests/e2e/src/cases/pricing-wire-golden-e2e.test.ts` when it runs with
//!   `UPDATE_PRICING_GOLDEN=1`. Each row is what a gateway in managed mode
//!   delivered to its control plane, reduced to the keys `BillableUsage`
//!   reads plus `guardrail_blocked` and `cost_usd`, with its ids and
//!   timestamps replaced by stable names.
//! - `golden/priced/<family>.json` is every row's `PricedEvent`. This file
//!   rewrites it with `UPDATE_GOLDEN=1`; otherwise a priced event that
//!   differs from it fails.
//! - `golden/book.json` prices every family's model at one baseline card,
//!   for one group at a rate of 1.0000 CNY per USD, with no tenant discount
//!   and no cost rate. Its `default` base rates are the `Model.cost` every
//!   seeded model carries, so a row's `cost_usd` is the gateway's own figure
//!   for the same price.

use std::path::{Path, PathBuf};

use aisix_core::spend_micro_usd;
use aisix_pricing::{
    price_event, BillableUsage, Dimension, Outcome, Payer, PriceBook, PriceRows, PricedEvent,
    ServiceTier,
};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// 2026-11-06T00:00:01Z: when the control plane received every golden row,
/// one second after the instant the e2e stamps on each of them.
const RECEIVED_AT: &str = "2026-11-06T00:00:01Z";

/// Each family's one row: its outcome and its charge in micro-CNY, worked
/// out by hand from the mock upstream's usage and the book's baseline card.
/// The book's group rate is 1.0000, at which a charge in µ¥ is the
/// baseline's in µ$, so these are the gateway's own figures too.
const EXPECTED: &[(&str, &str, i64)] = &[
    ("openai-chat", "charged", 340), // flex: 200×0.5 + 800×0.05 + 200×1
    ("openai-chat-stream", "charged", 340),
    ("openai-via-messages", "charged", 340),
    ("openai-responses", "charged", 1250), // 500 + 150 + 600
    ("openai-responses-stream", "charged", 1250),
    ("openai-cache-write", "charged", 980), // 500 + 300×0.1 + 200×1.25 + 100×2
    ("anthropic-messages", "charged", 20150), // 37 + 0.9 + 1.25 + 6 + 104 + 2×10000 = 20149.15, rounded up
    ("anthropic-messages-stream", "charged", 20150),
    ("anthropic-via-chat", "charged", 20150),
    ("gemini-chat", "charged", 200),           // 100 + (20+30)×2
    ("gemini-cached", "charged", 164),         // 60 + 40×0.1 + (20+30)×2
    ("gemini-no-total", "charged", 200),       // 100 + (20+30)×2, with no total reported
    ("bedrock-converse-chat", "charged", 150), // 149.15, rounded up
    ("openai-embeddings", "charged", 8),
    ("anthropic-count-tokens", "nothing_to_bill", 0),
    ("cache-hit-messages", "exempt", 0),
    ("cache-hit-chat", "exempt", 0),
];

/// The families whose rows the gateway's `Model.cost` prices exactly as the
/// engine does. Every other family is billed at a tier or a web search that
/// `Model.cost` cannot express.
const PRICED_ALIKE: [&str; 11] = [
    "openai-responses",
    "openai-responses-stream",
    "openai-cache-write",
    "gemini-chat",
    "gemini-cached",
    "gemini-no-total",
    "bedrock-converse-chat",
    "openai-embeddings",
    "anthropic-count-tokens",
    "cache-hit-messages",
    "cache-hit-chat",
];

/// The dimensions `Model.cost` has a rate for: input, cache read, cache
/// write, the one-hour cache write, and output, which also prices reasoning.
const MODEL_COST_DIMENSIONS: [Dimension; 6] = [
    Dimension::Input,
    Dimension::CacheRead,
    Dimension::CacheWrite,
    Dimension::CacheWrite1h,
    Dimension::Output,
    Dimension::Reasoning,
];

fn golden(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(relative)
}

fn updating() -> bool {
    std::env::var_os("UPDATE_GOLDEN").is_some_and(|v| v == "1")
}

fn book() -> PriceBook {
    let path = golden("book.json");
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let rows: PriceRows = serde_json::from_str(&json).expect("book.json holds price rows");
    let book = PriceBook::from_rows(rows);
    assert!(
        book.invalid_revisions().is_empty(),
        "every revision in book.json is readable: {:?}",
        book.invalid_revisions()
    );
    book
}

/// The family's captured rows, as the gateway sent them.
fn wire(family: &str) -> Vec<Value> {
    let path = golden(&format!("wire/{family}.json"));
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}. Capture it with the gateway e2e \
             pricing-wire-golden-e2e.test.ts under UPDATE_PRICING_GOLDEN=1.",
            path.display()
        )
    });
    let rows: Vec<Value> = serde_json::from_str(&json).expect("a wire golden is a JSON array");
    assert_eq!(
        rows.len(),
        1,
        "{family}: one request gives one row: {rows:?}"
    );
    rows
}

fn received_at() -> DateTime<Utc> {
    RECEIVED_AT.parse().expect("a valid instant")
}

/// Every row of `family`, priced, beside the row it priced. The payer is the
/// tenant the goldens are stamped with, whose key names the book's group.
fn priced(book: &PriceBook, family: &str) -> Vec<(Value, PricedEvent)> {
    wire(family)
        .into_iter()
        .map(|row| {
            let usage: BillableUsage = serde_json::from_value(row.clone())
                .unwrap_or_else(|e| panic!("{family}: not a usage row: {e}: {row}"));
            let payer = Payer {
                tenant_id: usage.team_id.clone(),
                group_id: Some("group-golden".into()),
            };
            let priced = price_event(book, &usage, &payer, received_at())
                .unwrap_or_else(|e| panic!("{family}: unpriced ({}): {e}", e.code()));
            (row, priced)
        })
        .collect()
}

#[test]
fn the_captured_families_are_exactly_the_expected_ones() {
    let dir = golden("wire");
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| {
            entry
                .expect("a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    found.sort();
    let mut expected: Vec<String> = EXPECTED.iter().map(|(f, ..)| format!("{f}.json")).collect();
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn each_family_prices_to_its_hand_computed_charge() {
    let book = book();
    for &(family, status, charge) in EXPECTED {
        for (_, priced) in priced(&book, family) {
            let PricedEvent {
                outcome,
                charge: got,
                cost,
                ..
            } = &priced;
            assert_eq!(
                (outcome.status(), got.0),
                (status, charge),
                "{family}: {priced:?}"
            );
            // The book has no cost rate.
            assert_eq!(*cost, None, "{family}: {priced:?}");
        }
    }
}

#[test]
fn the_priced_goldens_are_current() {
    let book = book();
    for &(family, ..) in EXPECTED {
        let events: Vec<PricedEvent> = priced(&book, family).into_iter().map(|(_, p)| p).collect();
        let got = serde_json::to_value(&events).expect("a priced event serializes");
        let path = golden(&format!("priced/{family}.json"));
        if updating() {
            std::fs::create_dir_all(path.parent().expect("a parent directory"))
                .expect("the priced directory can be created");
            let text = serde_json::to_string_pretty(&got).expect("JSON serializes") + "\n";
            std::fs::write(&path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            continue;
        }
        let json = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("{}: {e}. Rewrite it with UPDATE_GOLDEN=1.", path.display())
        });
        let want: Value = serde_json::from_str(&json).expect("a priced golden is JSON");
        assert_eq!(
            got,
            want,
            "{family}: the priced rows differ from {}. If the change is \
             intended, rewrite it with UPDATE_GOLDEN=1.",
            path.display()
        );
    }
}

/// Whether the gateway's `Model.cost` can price this row as the engine did:
/// nothing billed, or billed at the `default` tier below every band, on
/// dimensions `Model.cost` has a rate for.
fn model_cost_can_say(p: &PricedEvent) -> bool {
    match p.outcome {
        Outcome::Exempt(_) | Outcome::NothingToBill => true,
        Outcome::Charged => {
            p.service_tier == Some(ServiceTier::default_tier())
                && p.long_context_above.is_none()
                && p.lines
                    .iter()
                    .all(|l| MODEL_COST_DIMENSIONS.contains(&l.dimension))
        }
    }
}

#[test]
fn the_engine_charges_what_the_gateway_reported() {
    let book = book();
    let mut alike = Vec::new();
    for &(family, ..) in EXPECTED {
        for (row, priced) in priced(&book, family) {
            if !model_cost_can_say(&priced) {
                continue;
            }
            alike.push(family);
            let reported = row["cost_usd"]
                .as_f64()
                .unwrap_or_else(|| panic!("{family}: the row carries no cost_usd: {row}"));
            let gateway = i64::try_from(spend_micro_usd(reported)).expect("a spend fits in i64");
            let PricedEvent { charge, .. } = &priced;
            // At a rate of 1.0000 a charge in µ¥ is the baseline's in µ$. The
            // engine rounds up once and the gateway rounds its float to the
            // nearest µ$, so the engine is equal or one above, never below.
            let diff = charge.0 - gateway;
            assert!(
                (0..=1).contains(&diff),
                "{family}: the engine charges {} µ¥, the gateway reported {reported} USD \
                 ({gateway} µ$), engine - gateway = {diff}: {priced:?}",
                charge.0
            );
        }
    }
    // The rows compared are the ones `Model.cost` can price: a family that
    // left or joined that set means a row changed how it is billed.
    assert_eq!(alike, PRICED_ALIKE);
}
