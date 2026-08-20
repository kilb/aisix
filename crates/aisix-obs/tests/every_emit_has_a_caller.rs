//! Guard against the most-repeated silent bug in this repo: an emit function
//! on `Metrics` that nothing calls.
//!
//! Its methods are `pub`, so dead-code analysis never fires. A unit test can
//! call it directly and pass. The only symptom is a series that never appears
//! in a scrape — indistinguishable from "no traffic yet". It has now happened
//! three times: `record_proxy_request` (until #888),
//! `record_deployment_request` + `record_routing_fallback` (until #972), and
//! `record_otlp_fanout_drop` + `record_otlp_fanout_failure` (until the
//! 2026-08-19 pass wired them).
//!
//! Two documented occurrences were enough to write the rule into `CLAUDE.md`
//! and not enough to stop the third, so it is a test now. A source scan is
//! crude, but it is the only check that can see this: the compiler cannot,
//! because the methods are reachable public API.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Emits that are deliberately uncalled. Each entry must say why, and each is
/// a debt to clear rather than a permanent exemption.
const ALLOWED_UNCALLED: &[(&str, &str)] = &[
    (
        "record_llm_request",
        "Superseded by `record_proxy_and_llm_request`, which emits the same \
         family alongside the proxy tier. Pre-existing dead code, kept only \
         because a unit test still exercises it — that test asserts behaviour \
         no production path takes. Delete both together.",
    ),
    (
        "set_budget_gauges",
        "Its only data source was the control-plane budget HTTP decision \
         (`Decision.budget: Option<BudgetDetails>`, dollar f64 amounts), which \
         the spend-budget-plan Task 6 removed in favor of local \
         `RateLimitPolicy.max_spend_micro_usd` enforcement (micro-USD u64 \
         counters). Re-wiring this gauge from the local spend layer is a \
         distinct, not-yet-scheduled unit conversion + semantics job (what do \
         limit/spent/remaining mean for a rate-limit-shaped spend bucket), \
         left for a follow-up rather than invented here.",
    ),
];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/aisix-obs.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> always has two ancestors")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_metrics_emit_has_a_caller_outside_the_module() {
    let root = workspace_root();
    let metrics_rs = root.join("crates/aisix-obs/src/metrics.rs");
    let metrics_src = std::fs::read_to_string(&metrics_rs).expect("metrics.rs is readable");

    // Emit surface: the `pub fn record_* / set_*` methods on `Metrics`.
    let mut emits: BTreeSet<String> = BTreeSet::new();
    for line in metrics_src.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("pub fn ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.starts_with("record_") || name.starts_with("set_") {
            emits.insert(name);
        }
    }
    assert!(
        emits.len() > 20,
        "the emit scan found only {} methods — the parse is wrong, not the code",
        emits.len()
    );

    // Every crate's sources except the module that defines them.
    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);
    let callers: String = sources
        .iter()
        .filter(|p| **p != metrics_rs)
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();

    let allowed: BTreeSet<&str> = ALLOWED_UNCALLED.iter().map(|(name, _)| *name).collect();
    let mut uncalled: Vec<&String> = emits
        .iter()
        .filter(|name| !allowed.contains(name.as_str()))
        .filter(|name| !callers.contains(&format!(".{name}(")))
        .collect();
    uncalled.sort();

    assert!(
        uncalled.is_empty(),
        "these `Metrics` emits have no caller outside metrics.rs, so their series \
         can never appear in a scrape:\n{}\n\nWire each one at the site that \
         produces the event, or add it to ALLOWED_UNCALLED with the reason.",
        uncalled
            .iter()
            .map(|n| format!("  - {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // An exemption that is no longer needed is its own kind of rot.
    for (name, _) in ALLOWED_UNCALLED {
        assert!(
            emits.contains(*name),
            "ALLOWED_UNCALLED lists `{name}`, which is no longer an emit on \
             Metrics — drop the entry"
        );
        assert!(
            !callers.contains(&format!(".{name}(")),
            "`{name}` is exempted as uncalled but now has a caller — drop the \
             ALLOWED_UNCALLED entry"
        );
    }
}
