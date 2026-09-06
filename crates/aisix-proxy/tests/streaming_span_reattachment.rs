//! `crates/aisix-proxy/AGENTS.md`: a response-body stream must re-attach the
//! request span, and nothing errors when it does not.
//!
//! `request_id::ensure_request_id` opens the `request{request_id=…}` span that
//! puts the caller's `x-aisix-request-id` on every log line the request emits.
//! Hyper polls a response-body generator AFTER the middleware has returned, so
//! the generator inherits nothing: every `tracing::*` inside it — a guardrail
//! block, a buffer-cap trip, the terminal access log some endpoints emit from
//! there — comes out with no request field and cannot be joined back to the
//! request that produced it. The endpoint keeps working. The only symptom is
//! log lines an operator cannot correlate, which is indistinguishable from a
//! request that never happened.
//!
//! `passthrough_route.rs` shipped that way while all six of its siblings
//! wrapped theirs, and the three warnings inside its stream name a route and a
//! reason but no request. Found by reading, not by any failing test — hence
//! this one.

use std::path::{Path, PathBuf};

/// Modules whose `stream!` WRAPS an already-spanned stream rather than
/// originating a response body. Attaching a second span there is a no-op at
/// best: `in_request_span` captures `Span::current()`, and these run from the
/// polling task, not the handler's stack.
const WRAPPERS: &[&str] = &["sse_keepalive.rs", "stream_timeout.rs"];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every source file that ORIGINATES a streamed response body re-attaches the
/// span to it.
#[test]
fn every_streamed_body_re_attaches_the_request_span() {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![src_dir()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("proxy src is readable") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();

    let mut originates = 0usize;
    let mut unattached: Vec<String> = Vec::new();

    for path in &files {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("source file has a utf-8 name")
            .to_string();
        if WRAPPERS.contains(&name.as_str()) {
            continue;
        }
        let src = std::fs::read_to_string(path).expect("source is readable");
        // Only the non-test half: a `stream!` inside a test module builds a
        // fixture, not a response body.
        let body = match src.find("\n#[cfg(test)]") {
            Some(at) => &src[..at],
            None => &src[..],
        };
        if !body.contains("async_stream::stream!") && !body.contains("stream! {") {
            continue;
        }
        originates += 1;
        // Comment lines do not count. The first draft of this guard checked
        // the raw file text and was satisfied by the comment ABOVE the fix
        // explaining why the wrap is there — deleting the call while keeping
        // its explanation kept the test green, which is the precise shape of
        // rot these guards exist to prevent.
        let calls_it = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| l.contains("in_request_span("));
        if !calls_it {
            unattached.push(format!("  {name}"));
        }
    }

    assert!(
        originates >= 6,
        "found only {originates} files originating a streamed body — the \
         marker changed and this guard is checking almost nothing"
    );

    assert!(
        unattached.is_empty(),
        "these files build a response body from a generator and never \
         re-attach the request span:\n{}\n\nEvery `tracing::*` inside such a \
         generator loses its request id, so a guardrail block or a stream \
         error cannot be joined to the `x-aisix-request-id` the caller holds. \
         Wrap it in `request_id::in_request_span(…)` FROM THE HANDLER'S STACK \
         (it captures `Span::current()`), as the other streaming endpoints do \
         — or add the file to WRAPPERS with the reason it wraps rather than \
         originates.",
        unattached.join("\n"),
    );
}

/// The exemption list rots like every other one: a wrapper that is deleted,
/// renamed, or that grows into originating its own body leaves a name nobody
/// re-reads.
#[test]
fn the_wrapper_exemptions_still_wrap() {
    for name in WRAPPERS {
        let path = src_dir().join(name);
        assert!(
            path.exists(),
            "WRAPPERS lists `{name}`, which no longer exists"
        );
        let src = std::fs::read_to_string(&path).expect("source is readable");
        assert!(
            src.contains("async_stream::stream!") || src.contains("stream! {"),
            "WRAPPERS lists `{name}`, which no longer builds a stream — drop \
             the exemption"
        );
    }
}
