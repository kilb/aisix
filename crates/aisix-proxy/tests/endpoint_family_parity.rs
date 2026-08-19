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
