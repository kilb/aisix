//! Guard the "handler families stay in lockstep" rule that `CLAUDE.md`
//! restates in three places.
//!
//! The client-facing handlers share dispatch, auth, quota and telemetry, so a
//! per-request mechanism landed on one of them almost always applies to the
//! rest — and a gap on the unfixed siblings is SILENT: nothing errors, the
//! behaviour just quietly degrades for whoever uses that endpoint. Every
//! occurrence in this repo's history was found by a person noticing, which is
//! why it keeps recurring.
//!
//! A source scan is crude and deliberately so: it fails loudly when a new
//! model-dispatch handler appears without the mechanism, which is the moment
//! the drift is cheapest to fix. Add the endpoint to `MODEL_DISPATCH_HANDLERS`
//! when you add the file.

use std::path::{Path, PathBuf};

/// Handlers that resolve a Model, reserve quota against it, and return a
/// response to an SDK client. `/mcp`, `/a2a`, `/passthrough_route` and the
/// jobs surface are deliberately absent: they resolve no Model.
const MODEL_DISPATCH_HANDLERS: &[&str] = &[
    "chat.rs",
    "messages.rs",
    "responses.rs",
    "completions.rs",
    "embeddings.rs",
    "rerank.rs",
    "images.rs",
    "audio.rs",
    "videos.rs",
    "count_tokens.rs",
];

/// Per-request mechanisms every one of those handlers must carry, with what
/// breaks when one is missing.
const REQUIRED_MECHANISMS: &[(&str, &str)] = &[
    (
        "publish_rate_limit_window",
        "the caller gets no `x-ratelimit-*` headers and the remaining-quota \
         gauges skip this endpoint, so an SDK backs off blindly into 429s",
    ),
    (
        "check_ip_access",
        "the model's `allowed_cidrs` is not enforced on this endpoint, so a \
         source restriction an operator set is silently absent",
    ),
];

fn handlers_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

#[test]
fn every_model_dispatch_handler_carries_the_shared_mechanisms() {
    let dir = handlers_dir();
    let mut missing: Vec<String> = Vec::new();

    for handler in MODEL_DISPATCH_HANDLERS {
        let path = dir.join(handler);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()));
        for (mechanism, consequence) in REQUIRED_MECHANISMS {
            if !src.contains(mechanism) {
                missing.push(format!(
                    "  {handler} is missing {mechanism}\n      → {consequence}"
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "endpoint family drift — these handlers dispatch a model but skip a \
         shared per-request mechanism:\n{}\n\nWire it, or drop the handler from \
         MODEL_DISPATCH_HANDLERS with a note saying why it is not one.",
        missing.join("\n"),
    );
}

/// A cache HIT is client-visible output, so it must run the output chain
/// before being returned — the contract #448 established for
/// `/v1/chat/completions`.
///
/// This is a CONDITIONAL rule, unlike the ones above, and that is the point.
/// The bug it guards is not "an endpoint forgot a mechanism every endpoint
/// needs"; it is "an endpoint grew a cache and did not carry the obligation
/// that comes with one". When caching was extended to `/v1/messages` and
/// `/v1/responses`, both hit paths returned the stored bytes verbatim,
/// re-introducing exactly the bypass #448 had fixed — and nothing failed,
/// because the contract lived only in chat's own e2e.
///
/// The stored body is moderated under the policy in force when it is
/// WRITTEN, so the hole is a policy TIGHTENED afterwards: until the entry's
/// TTL expires (schema max seven days) the gateway keeps serving content the
/// operator has since forbidden.
#[test]
fn a_handler_that_caches_also_guards_its_cache_hits() {
    // Markers for "this handler reads from a cache" and "this handler runs
    // the output chain over what it read".
    const READS_A_CACHE: &[&str] = &["resolve_cache_hit", "gate.lookup()"];
    const GUARDS_THE_HIT: &[&str] = &[
        // chat moderates the typed response in place;
        "check_output_non_segment_observed(&cached)",
        // the byte-bodied endpoints route it through a shared helper.
        "guard_cached_response",
    ];

    let dir = handlers_dir();
    let mut unguarded: Vec<String> = Vec::new();

    for handler in MODEL_DISPATCH_HANDLERS {
        let src = std::fs::read_to_string(dir.join(handler)).expect("handler is readable");
        let caches = READS_A_CACHE.iter().any(|m| src.contains(m));
        if !caches {
            continue;
        }
        // `/v1/embeddings` is the documented exception: its response is a
        // vector, not text, so there is no output hook to run. It is listed
        // by name rather than inferred, so a future text-bearing endpoint
        // cannot inherit the exemption by accident.
        if *handler == "embeddings.rs" {
            continue;
        }
        if !GUARDS_THE_HIT.iter().any(|m| src.contains(m)) {
            unguarded.push(format!("  {handler}"));
        }
    }

    assert!(
        unguarded.is_empty(),
        "these handlers serve responses from a cache without running the \
         output chain over what they serve:\n{}\n\nA response stored before \
         a guardrail existed — or before one was tightened — is replayed \
         past it for the whole TTL. Run the output chain on the hit, as \
         `/v1/chat/completions` does (#448).",
        unguarded.join("\n"),
    );
}

/// A cache HIT that is exported to a content-capturing observability sink
/// must pass the same capture-safety gate the FRESH response passes.
///
/// The gate is two-sided — `input_capture_safe && output_capture_safe` — and
/// it exists because a guardrail can decide that content is unsafe to hand to
/// an exporter without blocking the request. Skip it on the hit path and the
/// exporter records, on every replay, exactly the text the policy refused to
/// record on the miss that stored it. Nothing errors; the leak is only
/// visible by reading the sink.
///
/// Structural, not by name: the check reads the slice between the handler's
/// cache-hit read and the capture it builds from that hit, and requires a
/// capture-safety term inside it. Deleting the gate empties that slice.
#[test]
fn a_cached_response_is_captured_under_the_same_safety_gate() {
    // Last occurrence, so a helper's *definition* earlier in the file does
    // not anchor the slice ahead of the real hit path.
    const READS_A_CACHE: &[&str] = &["resolve_cache_hit(", "gate.lookup()"];

    let dir = handlers_dir();
    let mut ungated: Vec<String> = Vec::new();

    for handler in MODEL_DISPATCH_HANDLERS {
        let src = std::fs::read_to_string(dir.join(handler)).expect("handler is readable");
        // Same documented exemption as the guardrail rule above: an
        // embedding response carries no text to capture.
        if *handler == "embeddings.rs" {
            continue;
        }
        let Some(hit_at) = READS_A_CACHE.iter().filter_map(|m| src.rfind(m)).max() else {
            continue;
        };
        // A handler that caches but exports no content has nothing to gate.
        let Some(rel) = src[hit_at..].find("CapturedContent::new(") else {
            continue;
        };
        // The term has to appear in a CONDITION, not just anywhere in the
        // slice. Two weaker readings both pass while the gate is gone: the
        // comment explaining it survives deletion, and so does the `let
        // cached_capture_safe = …` binding it used to feed — the compiler
        // reports that as an unused variable, which is a warning nobody
        // fails a build on.
        let gated = src[hit_at..hit_at + rel]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| {
                l.contains("capture_safe")
                    && (l.contains("filter(")
                        || l.contains("&&")
                        || l.trim_start().starts_with("if "))
            });
        if !gated {
            ungated.push(format!("  {handler}"));
        }
    }

    assert!(
        ungated.is_empty(),
        "these handlers export a cache hit to a content-capturing sink \
         without the capture-safety gate the fresh path applies:\n{}\n\nGate \
         it on `input_capture_safe && output_capture_safe`, as \
         `/v1/chat/completions` does — otherwise a guardrail that withheld \
         the content from export is bypassed for the whole TTL.",
        ungated.join("\n"),
    );
}

/// The list itself rots: a handler deleted or renamed must not leave the guard
/// silently checking nothing.
#[test]
fn the_handler_list_matches_what_is_on_disk() {
    let dir = handlers_dir();
    for handler in MODEL_DISPATCH_HANDLERS {
        assert!(
            dir.join(handler).exists(),
            "MODEL_DISPATCH_HANDLERS lists `{handler}`, which no longer exists"
        );
    }
}
