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
/// response to an SDK client. `/mcp` and `/a2a` are deliberately absent:
/// they resolve no Model. `passthrough_route.rs` resolves one only from a
/// body that may not carry a `model` at all, and relays the upstream
/// response verbatim, so the shared response-shaping mechanisms do not
/// apply to it. `realtime.rs` answers with a WebSocket upgrade rather than
/// a response an SDK reads.
///
/// The jobs surface IS one of these and was listed as "resolves no Model"
/// for as long as `jobs.rs` had been resolving one (and IP-checking it).
/// That wrong reason is exactly how it went a whole release without
/// publishing the rate-limit window it spends — membership here is decided
/// by what the file does, never by which URL family it feels like.
const MODEL_DISPATCH_HANDLERS: &[&str] = &[
    "chat.rs",
    "jobs.rs",
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

/// A handler that inspects caller text for a BLOCK must also apply the MASK
/// rules from the same chain. Conditional, like the cache rule below, and for
/// the same reason: the bug is not "an endpoint forgot something universal",
/// it is "an endpoint wired half of one mechanism".
///
/// The two halves come from one config object. A `pii` guardrail with
/// `action: mask` is accepted, resolved, and reported in
/// `applied_guardrails` whether or not the handler ever calls the rewrite —
/// so a handler that only checks silently sends the caller's PII upstream on
/// a request that succeeds. `/v1/videos` shipped that way: it ran
/// `check_input_observed` on the prompt and never masked it.
///
/// Deliberately not in REQUIRED_MECHANISMS: a handler that inspects no caller
/// text (a poll or fetch route) has nothing to mask, and demanding the call
/// there would be noise.
#[test]
fn a_handler_that_checks_caller_text_also_masks_it() {
    let dir = handlers_dir();
    // Running an input/output chain over caller text.
    const CHECKS_TEXT: &[&str] = &[
        "check_input_observed",
        "check_input_non_segment",
        "moderate_body",
        "moderate_structured_body",
    ];
    // Applying that same chain's mask action.
    const APPLIES_MASKS: &[&str] = &[
        "redacts_input",
        "redact_input_text",
        "redact::redact_",
        "moderate_body",
        "moderate_structured_body",
    ];

    let mut missing: Vec<String> = Vec::new();
    for handler in MODEL_DISPATCH_HANDLERS {
        let path = dir.join(handler);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()));
        let checks = CHECKS_TEXT.iter().any(|m| src.contains(m));
        let masks = APPLIES_MASKS.iter().any(|m| src.contains(m));
        if checks && !masks {
            missing.push(format!(
                "  {handler} runs a guardrail check over caller text but never \
                 applies the chain's mask action"
            ));
        }
    }

    assert!(
        missing.is_empty(),
        "half-wired guardrail — a `mask` rule on these endpoints is accepted, \
         listed as applied, and changes nothing, so the caller's PII reaches \
         the provider on a request that succeeds:\n{}",
        missing.join("\n"),
    );
}

/// Third leg of the mask rule: a handler that APPLIES a mask must carry the
/// counts into its usage event.
///
/// The chain is check → mask → report, and the two tests above cover the
/// first hop. A handler that masks and reports nothing is not broken in any
/// way a caller sees — the request succeeds, the PII is gone — but the
/// operator has no record that anything was rewritten, which is the whole
/// basis for auditing what a `mask` rule did. `images`, `audio` and `videos`
/// each assert this for themselves; this is the family-wide version, so the
/// NEXT endpoint to grow a mask cannot skip it in silence.
///
/// Counts only, never the matched text — the event must not become the PII
/// sink the rule exists to avoid.
///
/// Note on how far this was verified: the counts field is load-bearing in the
/// type system, so deleting the plumbing from an existing handler does not
/// compile and cannot be used to fail this test. What was checked instead is
/// the scan itself — pointed at a marker no handler contains, it names all
/// ten masking handlers, so both halves of the detection work. The case it
/// really guards is a NEW handler that masks and never grows the field.
#[test]
fn a_handler_that_masks_also_records_that_it_masked() {
    const APPLIES_MASKS: &[&str] = &[
        "redacts_input(",
        "redact_input_text(",
        "redact::redact_",
        "moderate_body(",
        "moderate_structured_body(",
    ];
    const REPORTS_THE_COUNTS: &[&str] = &["redacted_entity_counts", "redactions"];

    /// `/v1/messages/count_tokens` emits no UsageEvent at all (see
    /// `count_tokens.rs`), so it has nowhere to put the counts. Listed by
    /// name so a handler that DOES emit one cannot inherit the exemption.
    const NO_USAGE_EVENT: &[&str] = &["count_tokens.rs"];

    let dir = handlers_dir();
    let mut unaudited: Vec<String> = Vec::new();
    for handler in MODEL_DISPATCH_HANDLERS {
        if NO_USAGE_EVENT.contains(handler) {
            continue;
        }
        let src = std::fs::read_to_string(dir.join(handler)).expect("handler is readable");
        if !APPLIES_MASKS.iter().any(|m| src.contains(m)) {
            continue;
        }
        if !REPORTS_THE_COUNTS.iter().any(|m| src.contains(m)) {
            unaudited.push(format!("  {handler}"));
        }
    }

    assert!(
        unaudited.is_empty(),
        "these handlers rewrite caller text under a `mask` rule and leave no \
         trace of it in their usage event:\n{}\n\nThe request succeeds and the \
         PII is gone, so nothing looks wrong — but nobody can audit what the \
         rule did. Carry the counts into `redacted_entity_counts`.",
        unaudited.join("\n"),
    );

    // The exemption rots like every other list here.
    for handler in NO_USAGE_EVENT {
        let src = std::fs::read_to_string(dir.join(handler)).expect("handler is readable");
        assert!(
            !src.contains("UsageEvent {"),
            "NO_USAGE_EVENT lists `{handler}`, which now builds a UsageEvent — \
             drop the exemption and carry the counts into it"
        );
    }
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

/// The list itself rots BOTH ways, and only one of them was checked.
///
/// A deleted handler leaves an entry pointing at nothing — caught below. A
/// NEW handler is the direction that actually costs something: the module
/// doc asks you to add the file when you add the endpoint, and nothing made
/// anyone. That is not hypothetical here — `jobs.rs` sat outside this list
/// while resolving a Model, IP-checking it and spending the caller's
/// rate-limit window, and both mechanisms it was missing went unnoticed
/// because the list it belonged in never mentioned it.
///
/// Membership is decided by behaviour: calling the shared resolver is what
/// makes a file one of these.
#[test]
fn the_handler_list_matches_what_is_on_disk() {
    let dir = handlers_dir();
    for handler in MODEL_DISPATCH_HANDLERS {
        assert!(
            dir.join(handler).exists(),
            "MODEL_DISPATCH_HANDLERS lists `{handler}`, which no longer exists"
        );
    }

    /// Resolves a Model but is not one of these, with the reason.
    const NOT_A_RESPONSE_HANDLER: &[(&str, &str)] = &[(
        "realtime.rs",
        "answers with a WebSocket upgrade, not a response an SDK reads: the \
         shared mechanisms here are about shaping that response (rate-limit \
         headers, cache-hit guarding, a mask over caller text in a body it \
         does not have).",
    )];

    let mut unlisted: Vec<String> = Vec::new();
    let mut resolvers = 0usize;
    let mut stack = vec![dir.clone()];
    let mut files: Vec<PathBuf> = Vec::new();
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("proxy src is readable") {
            let path = e.expect("readable entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("utf-8 name")
            .to_string();
        let src = std::fs::read_to_string(path).expect("readable");
        // A call, not a mention: `semantic.rs` discusses the resolver in a
        // comment and resolves nothing.
        if !src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| l.contains("model_resolve::resolve_model("))
        {
            continue;
        }
        resolvers += 1;
        if MODEL_DISPATCH_HANDLERS.contains(&name.as_str())
            || NOT_A_RESPONSE_HANDLER.iter().any(|(f, _)| *f == name)
        {
            continue;
        }
        unlisted.push(format!("  {name}"));
    }

    assert!(
        resolvers >= 10,
        "found only {resolvers} callers of the shared resolver — the marker \
         changed and this guard is checking almost nothing"
    );
    assert!(
        unlisted.is_empty(),
        "these resolve a Model and are in neither list, so every rule in this \
         file skips them silently:\n{}\n\nAdd them to MODEL_DISPATCH_HANDLERS, \
         or to NOT_A_RESPONSE_HANDLER with the reason they are not one.",
        unlisted.join("\n"),
    );

    for (file, _) in NOT_A_RESPONSE_HANDLER {
        assert!(
            dir.join(file).exists(),
            "NOT_A_RESPONSE_HANDLER lists `{file}`, which no longer exists"
        );
    }
}

/// A handler that SPENDS the caller's rate-limit window must also REPORT it.
///
/// `quota::enforce` reserves the request-level layers — the api_key's own
/// `rate_limit` among them — so every surface that calls it draws down the
/// same per-key bucket `/v1/chat/completions` draws down. Reporting that
/// bucket is a separate call, and five surfaces spent it without ever making
/// that call: the caller receives no `x-ratelimit-*` headers and backs off
/// blindly into 429s, and `aisix_rate_limit_remaining` keeps publishing the
/// value from the last chat request while the key is in fact near its
/// ceiling. Nothing errors either way, which is why this stayed unnoticed.
///
/// Scoped by *behaviour*, not by a hand-drawn endpoint list, because that is
/// exactly what the list above missed: `MODEL_DISPATCH_HANDLERS` excuses the
/// jobs surface on the grounds that it "resolves no Model", and
/// `jobs.rs` has resolved one — and IP-checked it — the whole time.
#[test]
fn a_handler_that_spends_the_rate_limit_window_also_reports_it() {
    /// Calls that reserve the request-level layers (api_key / team / member).
    const SPENDS_THE_WINDOW: &[&str] = &[
        "quota::enforce(",
        "quota::enforce_rate_limit(",
        "quota::enforce_mcp(",
    ];

    /// Surfaces that spend the window and deliberately do not report it, each
    /// for a reason that is about THAT surface — not "we didn't get to it".
    /// A reason that stops being true is a bug: re-check them when the
    /// surface changes shape.
    const EXEMPT: &[(&str, &str)] = &[
        (
            "passthrough_route.rs",
            "relays every non-hop-by-hop upstream response header verbatim \
             (`copy_safe_headers`), so the caller may already be holding the \
             UPSTREAM's own `x-ratelimit-*`. `inject_ratelimit_headers` uses \
             `insert`, so publishing here would overwrite a truthful upstream \
             number with the gateway's — worse than saying nothing.",
        ),
        (
            "mcp.rs",
            "resolves no Model, and the remaining-quota gauge is labelled by \
             one. Reporting would mean either a synthetic model label (a \
             permanent junk series) or splitting the call so the two halves \
             can drift — the failure this file exists to prevent.",
        ),
        (
            "a2a.rs",
            "resolves no Model; same gauge-label problem as `mcp.rs`.",
        ),
        (
            "realtime.rs",
            "answers with a WebSocket upgrade. Headers on a 101 reach no \
             client API, and the session's token spend lands after the \
             handshake, so a window sampled at upgrade time would be stale \
             before the caller could read it.",
        ),
    ];

    let dir = handlers_dir();
    let mut silent: Vec<String> = Vec::new();
    let mut spends_the_window: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("proxy src is readable");
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();

    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("source file has a utf-8 name")
            .to_string();
        let src = std::fs::read_to_string(path).expect("handler is readable");
        // A mention inside a comment is not a call: `lib.rs` discusses
        // `quota::enforce` in three places and reserves nothing.
        let spends = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| SPENDS_THE_WINDOW.iter().any(|m| l.contains(m)));
        if !spends {
            continue;
        }
        spends_the_window.push(name.clone());
        if src.contains("publish_rate_limit_window") {
            continue;
        }
        if EXEMPT.iter().any(|(f, _)| *f == name) {
            continue;
        }
        silent.push(format!("  {name}"));
    }

    assert!(
        !spends_the_window.is_empty(),
        "the scan found no caller of {SPENDS_THE_WINDOW:?} — the marker was \
         renamed and this guard is now checking nothing"
    );

    assert!(
        silent.is_empty(),
        "these surfaces draw down the caller's rate-limit window and never \
         report it:\n{}\n\nCall `request_metrics::publish_rate_limit_window` \
         on the success path, as `/v1/chat/completions` does — or add the \
         file to EXEMPT with the reason it cannot.",
        silent.join("\n"),
    );

    // The exemptions rot the same way the handler list does: a surface that
    // stopped reserving, or was renamed, leaves a reason nobody re-reads.
    for (file, _) in EXEMPT {
        assert!(
            spends_the_window.iter().any(|f| f == file),
            "EXEMPT lists `{file}`, which no longer reserves the caller's \
             rate-limit window — drop the exemption"
        );
    }
}
