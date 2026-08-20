//! Application helpers for PII redaction (#932 / #932).
//!
//! `aisix-guardrails` owns detection and the text→text rewrite
//! ([`Guardrail::redact_input_text`] / [`Guardrail::redact_output_text`]);
//! this module owns WHERE the rewrite is applied on each wire shape:
//!
//! - request side: the normalised [`ChatFormat`] (chat/completions), the
//!   Anthropic-native `/v1/messages` body, the `/v1/responses` body, the
//!   legacy completions `prompt`, and embeddings `input` — message text
//!   only, mirroring the scan surface of `check_input`;
//! - response side: [`ChatResponse`] content + tool-call arguments, the
//!   Anthropic-native response JSON, and buffered streamed chunks
//!   (channel-reassembly: a masked span can cross chunk boundaries, so
//!   each content channel is concatenated, rewritten once, and the full
//!   rewritten text re-emitted on the channel's first chunk).
//!
//! Every helper returns per-detector match counts (detector names only,
//! never values) which callers merge into `usage_events
//! .redacted_entity_counts`.

use std::collections::BTreeMap;

use aisix_gateway::{ChatChunk, ChatFormat, ChatResponse};
use aisix_guardrails::Guardrail;
use serde_json::Value;

/// detector name → masked-span count. Mirrors
/// `UsageEvent::redacted_entity_counts`.
pub type RedactionCounts = BTreeMap<String, u32>;

/// Merge `from` into `into` (repeated small helper; counts are tiny maps).
pub fn merge_counts(into: &mut RedactionCounts, from: RedactionCounts) {
    for (k, v) in from {
        *into.entry(k).or_insert(0) += v;
    }
}

/// Which side's redactor to run. The two sides can be configured
/// independently (`hook_point`), so every JSON-walking helper takes the
/// direction rather than hardcoding one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

fn redact_str(
    chain: &dyn Guardrail,
    dir: Direction,
    text: &str,
) -> Option<aisix_guardrails::Redaction> {
    match dir {
        Direction::Input => chain.redact_input_text(text),
        Direction::Output => chain.redact_output_text(text),
    }
}

// ─── Remote segment moderation (kind=bedrock mask write-back) ────────────────
//
// A Bedrock guardrail whose PII action is ANONYMIZE returns the masked
// replacement text from the SAME `ApplyGuardrail` call that yields the
// verdict — an async, whole-request rewrite that can't implement the sync
// per-field redact contract above. The bridge works in three walker
// passes over one wire body, all using the SAME wire-shape walker so slot
// enumeration order is identical by construction:
//
//   1. collect: a probe guardrail records every text slot the walker
//      offers (rewriting nothing);
//   2. one remote call: the chain's segment fold sends the slots as one
//      content block each and returns verdict + positionally-aligned
//      masked texts;
//   3. apply: a second probe guardrail replaces slot i with masked[i].
//
// Call sites pair this with `check_*_non_segment` so a segment-moderating
// member is consulted exactly once per hook. Families without a wire
// walker (embeddings, rerank, images, audio, passthrough, MCP) keep the
// plain `check_*` path, where an ANONYMIZE disposition still maps to
// Block — there is no write-back channel, and releasing the un-masked
// content would defeat the operator's policy.

/// Marker count key [`SegmentApplier`] attaches to each rewritten slot.
/// Several walkers discard a rewrite whose counts are empty (their
/// "did anything change" gate); the marker makes those gates fire. It is
/// never surfaced: [`moderate_body`] discards the apply-walk's returned
/// counts and reports the provider's entity counts instead.
const SEGMENT_APPLY_MARKER: &str = "__segment_apply__";

/// Pass-1 probe: records every text slot the walker offers. Never
/// rewrites, so the body is bit-identical after the collect walk.
#[derive(Default)]
struct SegmentCollector {
    texts: std::sync::Mutex<Vec<String>>,
}

impl SegmentCollector {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.texts.lock().expect("collector poisoned"))
    }

    fn record(&self, text: &str) -> Option<aisix_guardrails::Redaction> {
        self.texts
            .lock()
            .expect("collector poisoned")
            .push(text.to_owned());
        None
    }
}

impl Guardrail for SegmentCollector {
    fn name(&self) -> &'static str {
        "segment-collector"
    }
    fn redacts_input(&self) -> bool {
        true
    }
    fn redacts_output(&self) -> bool {
        true
    }
    fn redact_input_text(&self, text: &str) -> Option<aisix_guardrails::Redaction> {
        self.record(text)
    }
    fn redact_output_text(&self, text: &str) -> Option<aisix_guardrails::Redaction> {
        self.record(text)
    }
}

/// Pass-3 probe: replaces the i-th offered slot with `masked[i]`.
/// Positional by construction — the walker offers slots in the same
/// order the collector recorded them (same walker, same body state).
struct SegmentApplier {
    state: std::sync::Mutex<ApplierState>,
}

struct ApplierState {
    masked: Vec<String>,
    originals: Vec<String>,
    cursor: usize,
    mismatched_text: bool,
}

impl SegmentApplier {
    fn new(masked: Vec<String>, originals: Vec<String>) -> Self {
        Self {
            state: std::sync::Mutex::new(ApplierState {
                masked,
                originals,
                cursor: 0,
                mismatched_text: false,
            }),
        }
    }

    fn apply(&self, original: &str) -> Option<aisix_guardrails::Redaction> {
        let mut st = self.state.lock().expect("applier poisoned");
        let i = st.cursor;
        st.cursor += 1;
        if st.originals.get(i).map(String::as_str) != Some(original) {
            st.mismatched_text = true;
            return None;
        }
        match st.masked.get(i) {
            Some(m) if m != original => Some(aisix_guardrails::Redaction {
                text: m.clone(),
                counts: std::iter::once((SEGMENT_APPLY_MARKER.to_owned(), 1)).collect(),
            }),
            _ => None,
        }
    }

    fn is_aligned(&self) -> bool {
        let st = self.state.lock().expect("applier poisoned");
        !st.mismatched_text && st.cursor == st.masked.len() && st.originals.len() == st.masked.len()
    }
}

impl Guardrail for SegmentApplier {
    fn name(&self) -> &'static str {
        "segment-applier"
    }
    fn redacts_input(&self) -> bool {
        true
    }
    fn redacts_output(&self) -> bool {
        true
    }
    fn redact_input_text(&self, text: &str) -> Option<aisix_guardrails::Redaction> {
        self.apply(text)
    }
    fn redact_output_text(&self, text: &str) -> Option<aisix_guardrails::Redaction> {
        self.apply(text)
    }
}

/// Complete one hook's moderation over a wire body: fold the already-run
/// `check_*_non_segment` verdict with the remote segment pass. The
/// segment pass is skipped when the check already blocked (the request
/// is dead — don't burn a provider call) or when the chain has no
/// segment-moderating member (zero overhead for non-Bedrock chains).
/// Masked replacements are written back through `walk`; the provider's
/// entity counts merge into `counts_out` (they feed
/// `redacted_entity_counts`, names only — #932 no-leak).
pub struct BodyModeration {
    pub verdict: aisix_guardrails::GuardrailVerdict,
    /// Safe to attach the walked body to a full-content exporter. False when
    /// a remote segment block or collect/apply drift left the body without a
    /// complete rewrite.
    pub capture_safe: bool,
}

fn segment_walk_drift_verdict() -> aisix_guardrails::GuardrailVerdict {
    aisix_guardrails::GuardrailVerdict::block(
        "segment apply walk drifted from collect walk; blocking to avoid releasing unmasked text",
    )
}

pub async fn moderate_body(
    chain: &dyn Guardrail,
    dir: Direction,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
    mut walk: impl FnMut(&dyn Guardrail) -> RedactionCounts,
) -> BodyModeration {
    if non_segment_verdict.is_block() || !chain.moderates_segments() {
        // A block-only detector has no rewrite to remove the matched secret.
        // Never attach that rejected input/output to a full-content exporter.
        let capture_safe = !non_segment_verdict.is_block() && !non_segment_verdict.is_bypass();
        return BodyModeration {
            verdict: non_segment_verdict,
            capture_safe,
        };
    }
    let collector = SegmentCollector::default();
    walk(&collector);
    let texts = collector.take();
    if texts.is_empty() {
        return BodyModeration {
            capture_safe: !non_segment_verdict.is_bypass(),
            verdict: non_segment_verdict,
        };
    }
    let mut outcome = match dir {
        Direction::Input => chain.moderate_input_segments(&texts).await,
        Direction::Output => chain.moderate_output_segments(&texts).await,
    };
    monitor_hits_out.append(&mut outcome.monitor_hits);
    let segment_capture_safe = !outcome.verdict.is_bypass();
    if outcome.verdict.is_block() {
        return BodyModeration {
            verdict: non_segment_verdict.merged_with(outcome.verdict),
            capture_safe: false,
        };
    }
    if let Some(masked) = outcome.masked {
        // Re-run the same walker without mutating anything and compare both
        // order and count before applying positional masks. A mismatch is an
        // internal boundary failure: retaining any original would violate the
        // fail-closed DLP contract.
        //
        // Belt-and-braces, deliberately: `SegmentApplier` already compares
        // each slot against its recorded original and `is_aligned()` already
        // catches a short, long or reordered walk, so drift is blocked either
        // way. What this pass buys is that the body is left UNTOUCHED on
        // drift rather than half-masked — worth one extra traversal on the
        // segment-moderating path only. Don't remove it without deciding that
        // a partially-rewritten (but blocked, so never forwarded) body is
        // acceptable.
        let verifier = SegmentCollector::default();
        let _ = walk(&verifier);
        if verifier.take() != texts || masked.len() != texts.len() {
            return BodyModeration {
                verdict: non_segment_verdict.merged_with(segment_walk_drift_verdict()),
                capture_safe: false,
            };
        }
        let applier = SegmentApplier::new(masked, texts);
        // Marker counts are plumbing (see SEGMENT_APPLY_MARKER) — discard
        // them; the provider counts below are the real ones.
        let _ = walk(&applier);
        if !applier.is_aligned() {
            return BodyModeration {
                verdict: non_segment_verdict.merged_with(segment_walk_drift_verdict()),
                capture_safe: false,
            };
        }
        merge_counts(counts_out, outcome.counts);
    }
    let verdict = non_segment_verdict.merged_with(outcome.verdict);
    BodyModeration {
        capture_safe: segment_capture_safe && !verdict.is_bypass(),
        verdict,
    }
}

/// Rewrite one owned text field in place. No-op (and no allocation) when
/// nothing matches.
fn apply_to_string(
    chain: &dyn Guardrail,
    dir: Direction,
    field: &mut String,
    counts: &mut RedactionCounts,
) {
    if field.is_empty() {
        return;
    }
    if let Some(r) = redact_str(chain, dir, field) {
        *field = r.text;
        merge_counts(counts, r.counts);
    }
}

/// Rewrite a `Value::String` in place (helper for JSON-tree walking).
fn apply_to_value_string(
    chain: &dyn Guardrail,
    dir: Direction,
    v: &mut Value,
    counts: &mut RedactionCounts,
) {
    if let Value::String(s) = v {
        if !s.is_empty() {
            if let Some(r) = redact_str(chain, dir, s) {
                *s = r.text;
                merge_counts(counts, r.counts);
            }
        }
    }
}

/// Recursively rewrite every string VALUE in a JSON tree (object values,
/// array elements). Keys and non-string scalars are untouched, so the
/// tree stays structurally valid — a phone number stored as a JSON number
/// is out of scope by design (rewriting it to a mask token would corrupt
/// the document).
pub fn redact_value_strings(
    chain: &dyn Guardrail,
    dir: Direction,
    v: &mut Value,
    counts: &mut RedactionCounts,
) {
    match v {
        Value::String(_) => apply_to_value_string(chain, dir, v, counts),
        Value::Array(items) => {
            for item in items {
                redact_value_strings(chain, dir, item, counts);
            }
        }
        Value::Object(map) => {
            for (_, val) in map.iter_mut() {
                redact_value_strings(chain, dir, val, counts);
            }
        }
        _ => {}
    }
}

/// Mask-rewrite an already-assembled OUTPUT text buffer in place — the
/// content-capture accumulator a streaming hold-back path hands to
/// content-capturing exporters (#932 × #947). The wire-side
/// SSE/chunk redaction rewrites only the held bytes released to the client;
/// the capture accumulator collects raw deltas, so without this the exported
/// content would carry PII the client never saw. Counts are deliberately
/// discarded — the wire-side redaction already tallied them, and tallying
/// the same matches again would double-count.
pub fn redact_captured_output(chain: &dyn Guardrail, text: &mut String) {
    let mut discard = RedactionCounts::new();
    apply_to_string(chain, Direction::Output, text, &mut discard);
}

/// Rewrite a JSON-*encoded* string (OpenAI `function.arguments`): parse,
/// walk the string values, re-serialise — so a mask token can't corrupt
/// the embedded document (e.g. a phone number as a JSON number value
/// stays untouched rather than becoming invalid JSON). Falls back to a
/// raw text rewrite when the payload doesn't parse (a provider emitted
/// malformed/partial args — best effort beats leaking).
#[cfg(test)]
pub fn redact_json_encoded(
    chain: &dyn Guardrail,
    dir: Direction,
    encoded: &mut String,
    counts: &mut RedactionCounts,
) {
    let _ = redact_json_encoded_structured(chain, dir, encoded, counts, false);
}

fn redact_json_encoded_structured(
    chain: &dyn Guardrail,
    dir: Direction,
    encoded: &mut String,
    counts: &mut RedactionCounts,
    detect_keys: bool,
) -> bool {
    if encoded.is_empty() {
        return false;
    }
    match serde_json::from_str::<Value>(encoded) {
        Ok(mut v) => {
            let unrewritable_tool_key = if detect_keys {
                let mut key_counts = RedactionCounts::new();
                let found = detect_unrewritable_object_keys(chain, dir, &v, &mut key_counts);
                merge_counts(counts, key_counts);
                found
            } else {
                false
            };

            let mut value_counts = RedactionCounts::new();
            redact_value_strings(chain, dir, &mut v, &mut value_counts);
            if !value_counts.is_empty() {
                if let Ok(s) = serde_json::to_string(&v) {
                    *encoded = s;
                    merge_counts(counts, value_counts);
                }
            }
            unrewritable_tool_key
        }
        Err(_) => {
            apply_to_string(chain, dir, encoded, counts);
            false
        }
    }
}

fn redact_tool_arguments_value(
    chain: &dyn Guardrail,
    dir: Direction,
    arguments: &mut Value,
    counts: &mut RedactionCounts,
    detect_keys: bool,
) -> bool {
    match arguments {
        Value::String(encoded) => {
            let mut owned = std::mem::take(encoded);
            let unrewritable =
                redact_json_encoded_structured(chain, dir, &mut owned, counts, detect_keys);
            *encoded = owned;
            unrewritable
        }
        Value::Object(_) | Value::Array(_) => {
            let unrewritable =
                detect_keys && detect_unrewritable_object_keys(chain, dir, arguments, counts);
            redact_value_strings(chain, dir, arguments, counts);
            unrewritable
        }
        // `null` is the wire encoding for an absent optional field, not a
        // shape we failed to understand: it carries no text, so there is
        // nothing to fail closed over. Matches `message_scan_text` and the
        // OpenAI response ingress, which both skip it.
        Value::Null => false,
        _ => detect_keys,
    }
}

// ─── Request side ────────────────────────────────────────────────────────────

/// Mask the request messages of a normalised [`ChatFormat`] in place:
/// the flat `content` string and the `text` field of typed content
/// blocks — the same surface `check_input` scans (`message_scan_text`).
/// Tool-call arguments replayed in history are covered too (they reach
/// the upstream verbatim). Returns the merged counts (empty = untouched).
fn redact_chat_format_impl(
    chain: &dyn Guardrail,
    req: &mut ChatFormat,
    detect_keys: bool,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_input() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    let mut unrewritable_tool_key = false;
    for msg in &mut req.messages {
        if detect_keys {
            if let Some(name) = msg.name.as_deref() {
                unrewritable_tool_key |= detect_unrewritable_value_strings(
                    chain,
                    Direction::Input,
                    &Value::String(name.to_owned()),
                    &mut counts,
                );
            }
            if let Some(tool_call_id) = msg.tool_call_id.as_deref() {
                unrewritable_tool_key |= detect_unrewritable_value_strings(
                    chain,
                    Direction::Input,
                    &Value::String(tool_call_id.to_owned()),
                    &mut counts,
                );
            }
        }
        if let Some(content) = msg.content.as_mut() {
            apply_to_string(chain, Direction::Input, content, &mut counts);
        }
        if let Some(blocks) = msg.content_blocks.as_mut() {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get_mut("text") {
                        apply_to_value_string(chain, Direction::Input, text, &mut counts);
                    }
                }
            }
        }
        // History-replay tool calls: arguments travel to the upstream
        // verbatim through `extra`, so mask them like fresh content.
        if let Some(tool_calls) = msg.extra.get_mut("tool_calls") {
            unrewritable_tool_key |= redact_tool_call_arguments(
                chain,
                Direction::Input,
                tool_calls,
                &mut counts,
                detect_keys,
            );
        }
        if let Some(function_call) = msg.extra.get_mut("function_call") {
            unrewritable_tool_key |= redact_legacy_function_call(
                chain,
                Direction::Input,
                function_call,
                &mut counts,
                detect_keys,
            );
        }
        if let Some(refusal) = msg.extra.get_mut("refusal") {
            apply_to_value_string(chain, Direction::Input, refusal, &mut counts);
        }
        // Fail closed rather than mask: the transcript travels with opaque
        // audio the redactor cannot reach, so rewriting only the text would
        // leave the same PII spoken in `audio.data` and hand the operator a
        // masked transcript as false assurance. Pinned by
        // `chat_audio_transcript_is_unrewritable_sensitive_output`.
        if let Some(transcript) = msg
            .extra
            .get_mut("audio")
            .and_then(|audio| audio.get_mut("transcript"))
        {
            if detect_keys {
                unrewritable_tool_key |= detect_unrewritable_value_strings(
                    chain,
                    Direction::Input,
                    transcript,
                    &mut counts,
                );
            } else {
                apply_to_value_string(chain, Direction::Input, transcript, &mut counts);
            }
        }
    }
    // Tool descriptions and JSON-schema examples/defaults are prompt text
    // forwarded through `ChatFormat::extra`. Structural fields (for example
    // function names and schema types) participate in DLP inspection but are
    // never rewritten because changing them would corrupt the tool contract.
    for field in ["tools", "functions"] {
        if let Some(definitions) = req.extra.get_mut(field) {
            unrewritable_tool_key |= redact_tool_definitions(
                chain,
                Direction::Input,
                definitions,
                &mut counts,
                detect_keys,
            );
        }
    }
    if let Some(response_format) = req.extra.get_mut("response_format") {
        unrewritable_tool_key |= redact_tool_definitions(
            chain,
            Direction::Input,
            response_format,
            &mut counts,
            detect_keys,
        );
    }
    if detect_keys {
        for field in ["tool_choice", "function_call"] {
            if let Some(selector) = req.extra.get(field) {
                unrewritable_tool_key |=
                    detect_unrewritable_object_keys(chain, Direction::Input, selector, &mut counts);
                unrewritable_tool_key |= detect_unrewritable_value_strings(
                    chain,
                    Direction::Input,
                    selector,
                    &mut counts,
                );
            }
        }
        for field in ["user", "safety_identifier", "prompt_cache_key"] {
            if let Some(value) = req.extra.get(field) {
                unrewritable_tool_key |=
                    detect_unrewritable_value_strings(chain, Direction::Input, value, &mut counts);
            }
        }
        if let Some(metadata) = req.extra.get("metadata") {
            unrewritable_tool_key |=
                detect_unrewritable_object_keys(chain, Direction::Input, metadata, &mut counts);
            unrewritable_tool_key |=
                detect_unrewritable_value_strings(chain, Direction::Input, metadata, &mut counts);
        }
    }
    if let Some(content) = req
        .extra
        .get_mut("prediction")
        .and_then(|prediction| prediction.get_mut("content"))
    {
        redact_responses_text_value(chain, Direction::Input, content, &mut counts);
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

#[cfg(test)]
pub fn redact_chat_format(chain: &dyn Guardrail, req: &mut ChatFormat) -> RedactionCounts {
    redact_chat_format_impl(chain, req, false).counts
}

pub fn redact_chat_format_structured(
    chain: &dyn Guardrail,
    req: &mut ChatFormat,
) -> AnthropicRequestRedaction {
    redact_chat_format_impl(chain, req, true)
}

pub async fn moderate_chat_format_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    req: &mut ChatFormat,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Input,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_chat_format_structured(guardrail, req),
    )
    .await
}

/// Clone a chat request for non-segment input inspection and append tool
/// declarations as prompt text. The original request remains the rewrite
/// target for the structured moderation pass.
pub fn chat_request_for_inspection(req: &ChatFormat) -> ChatFormat {
    let mut inspection = req.clone();
    for message in &req.messages {
        for text in [
            message.extra.get("refusal").and_then(Value::as_str),
            message
                .extra
                .get("audio")
                .and_then(|audio| audio.get("transcript"))
                .and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        {
            inspection
                .messages
                .push(aisix_gateway::ChatMessage::user(text.to_owned()));
        }
    }
    for field in [
        "tools",
        "functions",
        "tool_choice",
        "function_call",
        "response_format",
        "prediction",
        "user",
        "safety_identifier",
        "prompt_cache_key",
        "metadata",
    ] {
        if let Some(value) = req.extra.get(field) {
            let text = serde_json::to_string(value).expect("serde_json::Value always serializes");
            if text != "null" && text != "[]" {
                inspection
                    .messages
                    .push(aisix_gateway::ChatMessage::user(text));
            }
        }
    }
    inspection
}

/// Mask `function.arguments` (JSON-encoded string) on each element of an
/// OpenAI-shaped `tool_calls` array. Names/ids are structural: offer them to
/// DLP inspection, but fail closed instead of rewriting them.
fn redact_tool_call_arguments(
    chain: &dyn Guardrail,
    dir: Direction,
    tool_calls: &mut Value,
    counts: &mut RedactionCounts,
    detect_keys: bool,
) -> bool {
    let mut unrewritable_tool_key = false;
    if tool_calls.is_null() {
        return false;
    }
    let Some(items) = tool_calls.as_array_mut() else {
        return detect_keys;
    };
    for tc in items {
        if !tc.is_object() {
            unrewritable_tool_key |= detect_keys;
            continue;
        }
        if detect_keys {
            unrewritable_tool_key |=
                detect_named_structural_fields(chain, dir, tc, &["id", "type"], counts);
            if let Some(function) = tc.get("function") {
                unrewritable_tool_key |=
                    detect_named_structural_fields(chain, dir, function, &["name"], counts);
            }
        }
        let Some(function) = tc.get_mut("function") else {
            continue;
        };
        if !function.is_object() {
            unrewritable_tool_key |= detect_keys;
            continue;
        }
        if let Some(arguments) = function.get_mut("arguments") {
            unrewritable_tool_key |=
                redact_tool_arguments_value(chain, dir, arguments, counts, detect_keys);
        }
    }
    unrewritable_tool_key
}

/// Rewrite the JSON-encoded arguments of OpenAI's deprecated single
/// `function_call` shape. The function name is a stable protocol identifier:
/// inspect it, but fail closed rather than changing it.
fn redact_legacy_function_call(
    chain: &dyn Guardrail,
    dir: Direction,
    function_call: &mut Value,
    counts: &mut RedactionCounts,
    detect_keys: bool,
) -> bool {
    if function_call.is_null() {
        return false;
    }
    if !function_call.is_object() {
        return detect_keys;
    }
    let mut unrewritable = false;
    if detect_keys {
        unrewritable |=
            detect_named_structural_fields(chain, dir, function_call, &["name"], counts);
    }
    if let Some(arguments) = function_call.get_mut("arguments") {
        unrewritable |= redact_tool_arguments_value(chain, dir, arguments, counts, detect_keys);
    }
    unrewritable
}

/// Result of rewriting an Anthropic request. JSON object keys cannot be
/// renamed safely because doing so changes the tool contract; a sensitive key
/// therefore requires the caller to reject the request before dispatch.
pub struct AnthropicRequestRedaction {
    pub counts: RedactionCounts,
    pub unrewritable_tool_key: bool,
}

fn detect_unrewritable_object_keys(
    chain: &dyn Guardrail,
    dir: Direction,
    value: &Value,
    counts: &mut RedactionCounts,
) -> bool {
    match value {
        Value::Array(items) => {
            let mut found = false;
            for item in items {
                found |= detect_unrewritable_object_keys(chain, dir, item, counts);
            }
            found
        }
        Value::Object(map) => {
            let mut found = false;
            for (key, child) in map {
                if let Some(redaction) = redact_str(chain, dir, key) {
                    found |= redaction.text != *key;
                    merge_counts(counts, redaction.counts);
                }
                found |= detect_unrewritable_object_keys(chain, dir, child, counts);
            }
            found
        }
        _ => false,
    }
}

fn detect_unrewritable_value_strings(
    chain: &dyn Guardrail,
    dir: Direction,
    value: &Value,
    counts: &mut RedactionCounts,
) -> bool {
    match value {
        Value::String(text) => redact_str(chain, dir, text).is_some_and(|redaction| {
            let changed = redaction.text != *text;
            merge_counts(counts, redaction.counts);
            changed
        }),
        Value::Array(items) => items.iter().fold(false, |found, item| {
            detect_unrewritable_value_strings(chain, dir, item, counts) || found
        }),
        Value::Object(map) => map.values().fold(false, |found, child| {
            detect_unrewritable_value_strings(chain, dir, child, counts) || found
        }),
        _ => false,
    }
}

fn detect_named_structural_fields(
    chain: &dyn Guardrail,
    dir: Direction,
    value: &Value,
    fields: &[&str],
    counts: &mut RedactionCounts,
) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    fields.iter().fold(false, |found, field| {
        map.get(*field)
            .is_some_and(|slot| detect_unrewritable_value_strings(chain, dir, slot, counts))
            || found
    })
}

fn is_tool_contract_field(field: &str) -> bool {
    matches!(
        field,
        "name"
            | "id"
            | "type"
            | "call_id"
            | "tool_call_id"
            | "tool_use_id"
            | "item_id"
            | "approval_request_id"
            | "server_label"
            | "connector_id"
            | "required"
            | "enum"
            | "const"
            | "format"
            | "pattern"
            | "$ref"
            | "$schema"
    )
}

/// Rewrite prompt-bearing tool-definition values without changing protocol or
/// JSON-schema identifiers. Object keys and structural string fields are
/// inspected as unrewritable slots so local and remote masks fail closed.
fn redact_tool_definitions(
    chain: &dyn Guardrail,
    dir: Direction,
    value: &mut Value,
    counts: &mut RedactionCounts,
    detect_structural: bool,
) -> bool {
    match value {
        Value::String(_) => {
            apply_to_value_string(chain, dir, value, counts);
            false
        }
        Value::Array(items) => items.iter_mut().fold(false, |found, item| {
            redact_tool_definitions(chain, dir, item, counts, detect_structural) || found
        }),
        Value::Object(map) => {
            let mut found = false;
            for (key, child) in map {
                if detect_structural {
                    if let Some(redaction) = redact_str(chain, dir, key) {
                        found |= redaction.text != *key;
                        merge_counts(counts, redaction.counts);
                    }
                }
                if detect_structural && is_tool_contract_field(key) {
                    found |= detect_unrewritable_value_strings(chain, dir, child, counts);
                } else {
                    found |= redact_tool_definitions(chain, dir, child, counts, detect_structural);
                }
            }
            found
        }
        _ => false,
    }
}

/// Mask an Anthropic-native `/v1/messages` request body in place:
/// `system` (string or text blocks) and `messages[].content` (string or
/// blocks — `text` blocks and nested `tool_result` content). `tool_use`
/// input objects in history are walked as JSON strings. Tool object keys are
/// inspected but never renamed; `unrewritable_tool_key` tells handlers to
/// fail closed when a configured mask would change one.
pub fn redact_anthropic_request(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_input() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    let mut unrewritable_tool_key = false;
    if let Some(system) = body.get_mut("system") {
        unrewritable_tool_key |=
            redact_anthropic_content(chain, Direction::Input, system, &mut counts);
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages {
            if let Some(content) = msg.get_mut("content") {
                unrewritable_tool_key |=
                    redact_anthropic_content(chain, Direction::Input, content, &mut counts);
            }
        }
    }
    // Tool descriptions and JSON-schema examples/defaults are prompt text the
    // provider receives. Structural fields participate in inspection but are
    // never renamed or rewritten.
    if let Some(tools) = body.get_mut("tools") {
        unrewritable_tool_key |=
            redact_tool_definitions(chain, Direction::Input, tools, &mut counts, true);
    }
    if let Some(format) = body
        .get_mut("output_config")
        .and_then(|config| config.get_mut("format"))
    {
        unrewritable_tool_key |=
            redact_tool_definitions(chain, Direction::Input, format, &mut counts, true);
    }
    if let Some(format) = body.get_mut("output_format") {
        unrewritable_tool_key |=
            redact_tool_definitions(chain, Direction::Input, format, &mut counts, true);
    }
    if let Some(tool_choice) = body.get("tool_choice") {
        unrewritable_tool_key |=
            detect_unrewritable_object_keys(chain, Direction::Input, tool_choice, &mut counts);
        unrewritable_tool_key |=
            detect_unrewritable_value_strings(chain, Direction::Input, tool_choice, &mut counts);
    }
    if let Some(metadata) = body.get("metadata") {
        unrewritable_tool_key |= detect_named_structural_fields(
            chain,
            Direction::Input,
            metadata,
            &["user_id"],
            &mut counts,
        );
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

/// Run the remote segment pass over the Anthropic request walker. Object keys
/// participate in the same positional moderation call as rewriteable values,
/// but a changed key becomes a fail-closed verdict instead of being renamed.
pub struct AnthropicModeration {
    pub verdict: aisix_guardrails::GuardrailVerdict,
    /// Safe to attach the walked body to a full-content exporter. False when
    /// remote segment moderation blocked before a rewrite pass completed, or
    /// when rewriting a structural key would change the tool contract. A
    /// local-only input block stays safe because input callers run their
    /// synchronous rewrite before exporting the failure body. Blocked output
    /// is never capture-safe because output callers intentionally omit it.
    pub capture_safe: bool,
}

async fn moderate_structured_body(
    chain: &dyn Guardrail,
    dir: Direction,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
    mut walk: impl FnMut(&dyn Guardrail) -> AnthropicRequestRedaction,
) -> AnthropicModeration {
    if non_segment_verdict.is_block() {
        return AnthropicModeration {
            verdict: non_segment_verdict,
            capture_safe: false,
        };
    }
    if !chain.moderates_segments() {
        return AnthropicModeration {
            capture_safe: !non_segment_verdict.is_bypass(),
            verdict: non_segment_verdict,
        };
    }

    let collector = SegmentCollector::default();
    let _ = walk(&collector);
    let texts = collector.take();
    if texts.is_empty() {
        return AnthropicModeration {
            capture_safe: !non_segment_verdict.is_bypass(),
            verdict: non_segment_verdict,
        };
    }
    let mut outcome = match dir {
        Direction::Input => chain.moderate_input_segments(&texts).await,
        Direction::Output => chain.moderate_output_segments(&texts).await,
    };
    monitor_hits_out.append(&mut outcome.monitor_hits);
    let segment_capture_safe = !outcome.verdict.is_bypass();
    if outcome.verdict.is_block() {
        return AnthropicModeration {
            verdict: non_segment_verdict.merged_with(outcome.verdict),
            capture_safe: false,
        };
    }

    let mut unrewritable_tool_key = false;
    if let Some(masked) = outcome.masked {
        let verifier = SegmentCollector::default();
        let _ = walk(&verifier);
        if verifier.take() != texts || masked.len() != texts.len() {
            return AnthropicModeration {
                verdict: non_segment_verdict.merged_with(segment_walk_drift_verdict()),
                capture_safe: false,
            };
        }
        let applier = SegmentApplier::new(masked, texts);
        let result = walk(&applier);
        unrewritable_tool_key = result.unrewritable_tool_key;
        if !applier.is_aligned() {
            return AnthropicModeration {
                verdict: non_segment_verdict.merged_with(segment_walk_drift_verdict()),
                capture_safe: false,
            };
        }
        merge_counts(counts_out, outcome.counts);
    }
    let verdict = if unrewritable_tool_key {
        unrewritable_tool_key_verdict()
    } else {
        non_segment_verdict.merged_with(outcome.verdict)
    };
    AnthropicModeration {
        capture_safe: !verdict.is_block() && !verdict.is_bypass() && segment_capture_safe,
        verdict,
    }
}

pub fn unrewritable_tool_key_verdict() -> aisix_guardrails::GuardrailVerdict {
    aisix_guardrails::GuardrailVerdict::block(
        "data-loss prevention matched a tool structural field that cannot be safely rewritten",
    )
}

pub async fn moderate_anthropic_request(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Input,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_anthropic_request(guardrail, body),
    )
    .await
}

/// Normalize an Anthropic request for input inspection and append its raw tool
/// declarations as one synthetic message. The provider adapter intentionally
/// keeps tools in `ChatFormat::extra`, while guardrails inspect messages; this
/// projection closes that otherwise invisible provider-forwarded text surface.
pub fn anthropic_request_for_inspection(body: &Value) -> Result<aisix_gateway::ChatFormat, ()> {
    let mut chat = aisix_provider_anthropic::parse_inbound_request(body).map_err(|_| ())?;
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for content in messages.iter().filter_map(|message| message.get("content")) {
            let Some(additional) = anthropic_additional_content_for_inspection(content) else {
                continue;
            };
            let text = anthropic_content_inspection_text_capped(&additional, usize::MAX).0;
            if !text.is_empty() {
                chat.messages.push(aisix_gateway::ChatMessage::user(text));
            }
        }
    }
    for field in [
        "tools",
        "tool_choice",
        "output_config",
        "output_format",
        "metadata",
    ] {
        if let Some(value) = body.get(field) {
            let text = serde_json::to_string(value).map_err(|_| ())?;
            if text != "null" && text != "[]" {
                chat.messages.push(aisix_gateway::ChatMessage::user(text));
            }
        }
    }
    Ok(chat)
}

/// Keep only Anthropic message surfaces that the provider adapter does not
/// already materialize in `ChatFormat`. Ordinary strings, text blocks,
/// tool-use payloads, and textual client tool results are already covered by
/// the canonical adapter projection and must not be sent to guardrails twice.
fn anthropic_additional_content_for_inspection(content: &Value) -> Option<Value> {
    match content {
        Value::String(_) => None,
        Value::Array(items) => {
            let additional: Vec<_> = items
                .iter()
                .filter_map(anthropic_additional_content_for_inspection)
                .collect();
            (!additional.is_empty()).then_some(Value::Array(additional))
        }
        Value::Object(map) => match map.get("type").and_then(Value::as_str) {
            Some("text") => map.get("citations").cloned(),
            Some("tool_use") => map.get("caller").cloned(),
            Some("image") => None,
            Some("tool_result") => match map.get("content") {
                Some(Value::Array(items)) => {
                    let additional: Vec<_> = items
                        .iter()
                        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                            Some("text" | "image") => None,
                            _ => Some(item.clone()),
                        })
                        .collect();
                    (!additional.is_empty()).then_some(Value::Array(additional))
                }
                _ => None,
            },
            _ => Some(content.clone()),
        },
        _ => None,
    }
}

const ANTHROPIC_STRUCTURAL_FIELDS: &[&str] = &[
    "id",
    "name",
    "tool_use_id",
    "tool_id",
    "tool_name",
    "file_id",
    "server_name",
    "caller_id",
    "custom_tool_use_id",
    "mcp_tool_use_id",
    "session_thread_id",
    "url",
];

const ANTHROPIC_PROTOCOL_FIELDS: &[&str] = &[
    "type",
    "cache_control",
    "caller",
    "text",
    "thinking",
    "content",
    "source",
    "data",
    "media_type",
    "title",
    "context",
    "input",
    "input_schema",
    "description",
    "output",
    "stdout",
    "stderr",
    "error",
    "error_message",
    "message",
    "is_error",
    "citations",
    "cited_text",
    "document_title",
    "tool_references",
];

const ANTHROPIC_METADATA_FIELDS: &[&str] = &[
    "document_index",
    "enabled",
    "end_block_index",
    "end_char_index",
    "end_page_number",
    "error_code",
    "file_type",
    "from_",
    "is_file_update",
    "lines",
    "new_lines",
    "new_start",
    "num_lines",
    "old_lines",
    "old_start",
    "page_age",
    "processed_at",
    "retrieved_at",
    "return_code",
    "search_result_index",
    "start_block_index",
    "start_char_index",
    "start_line",
    "start_page_number",
    "stop_reason",
    "to",
    "total_lines",
    "trigger",
    "ttl",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnthropicKeyContext {
    Protocol,
    UserDefined,
    ServerToolInput,
}

fn redact_anthropic_value(
    chain: &dyn Guardrail,
    dir: Direction,
    value: &mut Value,
    counts: &mut RedactionCounts,
    key_context: AnthropicKeyContext,
) -> bool {
    match value {
        Value::String(_) => {
            apply_to_value_string(chain, dir, value, counts);
            false
        }
        Value::Array(items) => items.iter_mut().fold(false, |found, item| {
            redact_anthropic_value(chain, dir, item, counts, key_context) || found
        }),
        Value::Object(map) => {
            let object_type = map
                .get("type")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if key_context == AnthropicKeyContext::Protocol
                && matches!(
                    object_type.as_deref(),
                    Some("thinking" | "redacted_thinking")
                )
            {
                return false;
            }
            let mut unrewritable = false;
            for (field, child) in map {
                if key_context == AnthropicKeyContext::Protocol && field == "type" {
                    continue;
                }
                let opaque = key_context == AnthropicKeyContext::Protocol
                    && (matches!(
                        field.as_str(),
                        "signature" | "encrypted_content" | "encrypted_index" | "encrypted_stdout"
                    ) || (field == "data"
                        && matches!(
                            object_type.as_deref(),
                            Some("base64" | "base64_pdf" | "redacted_thinking")
                        ))
                        || (field == "url"
                            && matches!(
                                object_type.as_deref(),
                                Some("url" | "url_pdf" | "image" | "document")
                            )));
                if opaque {
                    continue;
                }
                if key_context == AnthropicKeyContext::Protocol
                    && ANTHROPIC_METADATA_FIELDS.contains(&field.as_str())
                {
                    continue;
                }
                // Protocol field names are not model content. Unknown keys
                // inside tool inputs/results remain in scope because callers
                // control those object keys and a mask cannot safely rename
                // them without changing the tool contract.
                if key_context == AnthropicKeyContext::UserDefined
                    || (key_context == AnthropicKeyContext::Protocol
                        && !ANTHROPIC_PROTOCOL_FIELDS.contains(&field.as_str())
                        && !ANTHROPIC_STRUCTURAL_FIELDS.contains(&field.as_str()))
                {
                    if let Some(redaction) = redact_str(chain, dir, field) {
                        unrewritable |= redaction.text != *field;
                        merge_counts(counts, redaction.counts);
                    }
                }
                if key_context == AnthropicKeyContext::Protocol
                    && (ANTHROPIC_STRUCTURAL_FIELDS.contains(&field.as_str())
                        || (field == "source" && matches!(child, Value::String(_))))
                {
                    unrewritable |= detect_unrewritable_value_strings(chain, dir, child, counts);
                    continue;
                }
                let child_context = match (key_context, field.as_str()) {
                    (AnthropicKeyContext::UserDefined, _) => AnthropicKeyContext::UserDefined,
                    (AnthropicKeyContext::ServerToolInput, _) => {
                        AnthropicKeyContext::ServerToolInput
                    }
                    (_, "input") if object_type.as_deref() == Some("server_tool_use") => {
                        AnthropicKeyContext::ServerToolInput
                    }
                    (_, "input" | "input_schema") => AnthropicKeyContext::UserDefined,
                    (_, "content")
                        if object_type.as_deref() == Some("tool_result") && child.is_object() =>
                    {
                        AnthropicKeyContext::UserDefined
                    }
                    _ => AnthropicKeyContext::Protocol,
                };
                unrewritable |= redact_anthropic_value(chain, dir, child, counts, child_context);
            }
            unrewritable
        }
        _ => false,
    }
}

/// Anthropic `content` is either a bare string or an array of typed blocks.
/// Every official text-bearing request/result block is walked; opaque image,
/// PDF, signature, and encrypted-thinking payloads are deliberately skipped.
fn redact_anthropic_content(
    chain: &dyn Guardrail,
    dir: Direction,
    content: &mut Value,
    counts: &mut RedactionCounts,
) -> bool {
    redact_anthropic_value(chain, dir, content, counts, AnthropicKeyContext::Protocol)
}

pub(crate) fn anthropic_content_inspection_text_capped(
    content: &Value,
    cap: usize,
) -> (String, bool) {
    let mut content = content.clone();
    let collector = SegmentCollector::default();
    let mut counts = RedactionCounts::new();
    let _ = redact_anthropic_content(&collector, Direction::Input, &mut content, &mut counts);
    let mut output = String::new();
    let mut truncated = false;
    for text in collector.take() {
        if text.is_empty() {
            continue;
        }
        if !output.is_empty() {
            truncated |= crate::token_estimate::push_capped_to(&mut output, "\n", cap);
        }
        truncated |= crate::token_estimate::push_capped_to(&mut output, &text, cap);
    }
    (output, truncated)
}

/// Mask an Anthropic-native `/v1/messages` RESPONSE body in place (the
/// non-streaming passthrough JSON): top-level `content` blocks (`text` +
/// `tool_use` input).
pub fn redact_anthropic_response(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_output() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    let unrewritable_tool_key = body.get_mut("content").is_some_and(|content| {
        redact_anthropic_content(chain, Direction::Output, content, &mut counts)
    });
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

pub async fn moderate_anthropic_response(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Output,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_anthropic_response(guardrail, body),
    )
    .await
}

/// Mask a `/v1/responses` request body in place: `instructions` and
/// `input` (bare string, or item list whose `message` items carry
/// `content` as a string or `input_text` parts). Function-call outputs
/// replayed as `function_call_output` items are walked too.
fn redact_responses_request_impl(
    chain: &dyn Guardrail,
    body: &mut Value,
    detect_keys: bool,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_input() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    if let Some(instructions) = body.get_mut("instructions") {
        apply_to_value_string(chain, Direction::Input, instructions, &mut counts);
    }
    let mut unrewritable_tool_key = false;
    match body.get_mut("input") {
        Some(v @ Value::String(_)) => {
            apply_to_value_string(chain, Direction::Input, v, &mut counts)
        }
        Some(Value::Array(items)) => {
            for item in items {
                unrewritable_tool_key |= redact_responses_item_structured(
                    chain,
                    Direction::Input,
                    item,
                    &mut counts,
                    detect_keys,
                );
            }
        }
        _ => {}
    }
    if let Some(tools) = body.get_mut("tools") {
        unrewritable_tool_key |=
            redact_tool_definitions(chain, Direction::Input, tools, &mut counts, detect_keys);
    }
    if let Some(text) = body.get_mut("text").and_then(|text| text.get_mut("format")) {
        unrewritable_tool_key |=
            redact_tool_definitions(chain, Direction::Input, text, &mut counts, detect_keys);
    }
    if let Some(prompt) = body.get_mut("prompt") {
        unrewritable_tool_key |= redact_responses_prompt(chain, prompt, &mut counts, detect_keys);
    }
    if detect_keys {
        if let Some(tool_choice) = body.get("tool_choice") {
            unrewritable_tool_key |=
                detect_unrewritable_object_keys(chain, Direction::Input, tool_choice, &mut counts);
            unrewritable_tool_key |= detect_unrewritable_value_strings(
                chain,
                Direction::Input,
                tool_choice,
                &mut counts,
            );
        }
        unrewritable_tool_key |= detect_named_structural_fields(
            chain,
            Direction::Input,
            body,
            &["user", "safety_identifier", "prompt_cache_key"],
            &mut counts,
        );
        if let Some(metadata) = body.get("metadata") {
            unrewritable_tool_key |=
                detect_unrewritable_object_keys(chain, Direction::Input, metadata, &mut counts);
            unrewritable_tool_key |=
                detect_unrewritable_value_strings(chain, Direction::Input, metadata, &mut counts);
        }
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

fn redact_responses_prompt(
    chain: &dyn Guardrail,
    prompt: &mut Value,
    counts: &mut RedactionCounts,
    detect_keys: bool,
) -> bool {
    let mut unrewritable = false;
    if detect_keys {
        unrewritable |= detect_named_structural_fields(
            chain,
            Direction::Input,
            prompt,
            &["id", "version"],
            counts,
        );
    }
    if let Some(variables) = prompt.get_mut("variables").and_then(Value::as_object_mut) {
        for (name, value) in variables {
            if detect_keys {
                if let Some(redaction) = redact_str(chain, Direction::Input, name) {
                    unrewritable |= redaction.text != *name;
                    merge_counts(counts, redaction.counts);
                }
            }
            match value {
                Value::String(_) => apply_to_value_string(chain, Direction::Input, value, counts),
                Value::Object(part)
                    if part.get("type").and_then(Value::as_str) == Some("input_text") =>
                {
                    if let Some(text) = part.get_mut("text") {
                        apply_to_value_string(chain, Direction::Input, text, counts);
                    }
                }
                _ => {}
            }
        }
    }
    unrewritable
}

pub(crate) fn responses_prompt_inspection_text_capped(
    prompt: &Value,
    cap: usize,
) -> (String, bool) {
    let mut prompt = prompt.clone();
    let collector = SegmentCollector::default();
    let mut counts = RedactionCounts::new();
    let _ = redact_responses_prompt(&collector, &mut prompt, &mut counts, true);
    let mut output = String::new();
    let mut truncated = false;
    for text in collector.take() {
        if text.is_empty() {
            continue;
        }
        if !output.is_empty() {
            truncated |= crate::token_estimate::push_capped_to(&mut output, "\n", cap);
        }
        truncated |= crate::token_estimate::push_capped_to(&mut output, &text, cap);
    }
    (output, truncated)
}

#[cfg(test)]
pub fn redact_responses_request(chain: &dyn Guardrail, body: &mut Value) -> RedactionCounts {
    redact_responses_request_impl(chain, body, false).counts
}

pub fn redact_responses_request_structured(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    redact_responses_request_impl(chain, body, true)
}

pub async fn moderate_responses_request_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Input,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_responses_request_structured(guardrail, body),
    )
    .await
}

fn redact_responses_text_value(
    chain: &dyn Guardrail,
    dir: Direction,
    value: &mut Value,
    counts: &mut RedactionCounts,
) {
    match value {
        Value::String(_) => apply_to_value_string(chain, dir, value, counts),
        Value::Array(parts) => {
            for part in parts {
                for field in ["text", "refusal"] {
                    if let Some(text) = part.get_mut(field) {
                        apply_to_value_string(chain, dir, text, counts);
                    }
                }
            }
        }
        _ => {}
    }
}

const RESPONSES_STRUCTURAL_FIELDS: &[&str] = &[
    "id",
    "call_id",
    "name",
    "approval_request_id",
    "server_label",
    "connector_id",
    "fingerprint",
    "caller_id",
    "file_id",
    "container_id",
    "created_by",
    "namespace",
    "path",
];

const RESPONSES_ENUM_FIELDS: &[&str] = &["type", "status", "role", "outcome"];

const RESPONSES_OPAQUE_FIELDS: &[&str] = &[
    "encrypted_content",
    "file_data",
    "file_url",
    "image_url",
    "screenshot",
    "signature",
];

fn responses_field_is_opaque(
    item_type: Option<&str>,
    field: &str,
    parent_type: Option<&str>,
) -> bool {
    RESPONSES_OPAQUE_FIELDS.contains(&field)
        || (item_type == Some("image_generation_call") && field == "result")
        || (parent_type == Some("code_interpreter_call") && field == "url")
}

#[derive(Clone, Copy)]
struct ResponsesWalkMode {
    detect_keys: bool,
    inspect_structural_values: bool,
}

fn redact_responses_tree(
    chain: &dyn Guardrail,
    dir: Direction,
    value: &mut Value,
    counts: &mut RedactionCounts,
    mode: ResponsesWalkMode,
    item_type: Option<&str>,
    parent_type: Option<&str>,
) -> bool {
    match value {
        Value::String(_) => {
            apply_to_value_string(chain, dir, value, counts);
            false
        }
        Value::Array(items) => items.iter_mut().fold(false, |found, item| {
            redact_responses_tree(chain, dir, item, counts, mode, item_type, parent_type) || found
        }),
        Value::Object(map) => {
            let object_type = map
                .get("type")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let mut unrewritable = false;
            for (field, child) in map {
                if responses_field_is_opaque(item_type, field, parent_type) {
                    continue;
                }
                if RESPONSES_ENUM_FIELDS.contains(&field.as_str()) {
                    continue;
                }
                if mode.detect_keys {
                    if let Some(redaction) = redact_str(chain, dir, field) {
                        unrewritable |= redaction.text != *field;
                        merge_counts(counts, redaction.counts);
                    }
                }
                if RESPONSES_STRUCTURAL_FIELDS.contains(&field.as_str()) {
                    if mode.inspect_structural_values {
                        unrewritable |=
                            detect_unrewritable_value_strings(chain, dir, child, counts);
                    }
                    continue;
                }
                if matches!(field.as_str(), "arguments" | "result" | "output")
                    && matches!(child, Value::String(_))
                {
                    unrewritable |= redact_responses_payload_value(
                        chain,
                        dir,
                        child,
                        counts,
                        mode.detect_keys,
                        true,
                    );
                    continue;
                }
                unrewritable |= redact_responses_tree(
                    chain,
                    dir,
                    child,
                    counts,
                    mode,
                    item_type,
                    object_type.as_deref().or(parent_type),
                );
            }
            unrewritable
        }
        _ => false,
    }
}

/// The complete Responses item projection used by non-segment input/output
/// checks and stream capture. Keep this field table shared with
/// [`redact_responses_item_structured`] so a newly supported item cannot be
/// inspected on one path but silently skipped by mask/segment handling.
pub(crate) fn responses_item_inspection_text_capped(item: &Value, cap: usize) -> (String, bool) {
    let mut item = item.clone();
    let collector = SegmentCollector::default();
    let mut counts = RedactionCounts::new();
    let _ = redact_responses_item_structured(
        &collector,
        Direction::Input,
        &mut item,
        &mut counts,
        true,
    );
    let mut output = String::new();
    let mut truncated = false;
    for text in collector.take() {
        if text.is_empty() {
            continue;
        }
        if !output.is_empty() {
            truncated |= crate::token_estimate::push_capped_to(&mut output, "\n", cap);
        }
        truncated |= crate::token_estimate::push_capped_to(&mut output, &text, cap);
    }
    (output, truncated)
}

/// User-visible/billed item text without protocol object-key names. Stable
/// identifiers remain included because tool names and call ids are part of
/// the model exchange, while schema field names such as `content` and `text`
/// are not assistant output and must not pollute capture or token estimates.
pub(crate) fn responses_item_content_text_capped(item: &Value, cap: usize) -> (String, bool) {
    let mut item = item.clone();
    let collector = SegmentCollector::default();
    let mut counts = RedactionCounts::new();
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let _ = redact_responses_tree(
        &collector,
        Direction::Input,
        &mut item,
        &mut counts,
        ResponsesWalkMode {
            detect_keys: false,
            inspect_structural_values: true,
        },
        item_type.as_deref(),
        item_type.as_deref(),
    );
    let mut output = String::new();
    let mut truncated = false;
    for text in collector.take() {
        if text.is_empty() {
            continue;
        }
        if !output.is_empty() {
            truncated |= crate::token_estimate::push_capped_to(&mut output, "\n", cap);
        }
        truncated |= crate::token_estimate::push_capped_to(&mut output, &text, cap);
    }
    (output, truncated)
}

fn redact_responses_payload_value(
    chain: &dyn Guardrail,
    dir: Direction,
    value: &mut Value,
    counts: &mut RedactionCounts,
    detect_keys: bool,
    json_encoded: bool,
) -> bool {
    if json_encoded {
        if let Value::String(encoded) = value {
            let mut owned = std::mem::take(encoded);
            let unrewritable =
                redact_json_encoded_structured(chain, dir, &mut owned, counts, detect_keys);
            *encoded = owned;
            return unrewritable;
        }
    }
    let unrewritable = detect_keys && detect_unrewritable_object_keys(chain, dir, value, counts);
    redact_value_strings(chain, dir, value, counts);
    unrewritable
}

/// One `/v1/responses` input/output item. This walks the same prose, tool
/// payload, program payload, and structural identifier slots as the inspection
/// projection above; JSON-encoded arguments and program results stay valid JSON.
fn redact_responses_item_structured(
    chain: &dyn Guardrail,
    dir: Direction,
    item: &mut Value,
    counts: &mut RedactionCounts,
    detect_keys: bool,
) -> bool {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    redact_responses_tree(
        chain,
        dir,
        item,
        counts,
        ResponsesWalkMode {
            detect_keys,
            inspect_structural_values: detect_keys,
        },
        item_type.as_deref(),
        item_type.as_deref(),
    )
}

/// Mask a `/v1/responses` non-streaming RESPONSE body in place: every
/// item in `output` (message `output_text` parts, `function_call`
/// arguments) — the same surface the output check scans.
#[cfg(test)]
pub fn redact_responses_response(chain: &dyn Guardrail, body: &mut Value) -> RedactionCounts {
    redact_responses_response_impl(chain, body, false).counts
}

fn redact_responses_response_impl(
    chain: &dyn Guardrail,
    body: &mut Value,
    detect_keys: bool,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_output() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    let mut unrewritable_tool_key = false;
    if let Some(Value::Array(items)) = body.get_mut("output") {
        for item in items {
            unrewritable_tool_key |= redact_responses_item_structured(
                chain,
                Direction::Output,
                item,
                &mut counts,
                detect_keys,
            );
        }
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

pub fn redact_responses_response_structured(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    redact_responses_response_impl(chain, body, true)
}

pub async fn moderate_responses_response_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Output,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_responses_response_structured(guardrail, body),
    )
    .await
}

/// Mask the mutable legacy completion text slots and report a matched
/// structural `user` identifier without rewriting it.
pub fn redact_completions_request_structured(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_input() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    match body.get_mut("prompt") {
        Some(v @ Value::String(_)) => {
            apply_to_value_string(chain, Direction::Input, v, &mut counts)
        }
        Some(Value::Array(items)) => {
            for item in items {
                if item.is_string() {
                    apply_to_value_string(chain, Direction::Input, item, &mut counts);
                }
            }
        }
        _ => {}
    }
    if let Some(suffix) = body.get_mut("suffix") {
        apply_to_value_string(chain, Direction::Input, suffix, &mut counts);
    }
    let unrewritable_tool_key =
        detect_named_structural_fields(chain, Direction::Input, body, &["user"], &mut counts);
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

pub async fn moderate_completions_request_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Input,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_completions_request_structured(guardrail, body),
    )
    .await
}

/// Mask caller-authored `/v1/rerank` text and report document object keys
/// that cannot be renamed safely.
pub fn redact_rerank_request_structured(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_input() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    let mut unrewritable_tool_key = false;
    if let Some(q) = body.get_mut("query") {
        apply_to_value_string(chain, Direction::Input, q, &mut counts);
    }
    if let Some(Value::Array(docs)) = body.get_mut("documents") {
        for doc in docs {
            match doc {
                Value::String(_) => {
                    apply_to_value_string(chain, Direction::Input, doc, &mut counts)
                }
                Value::Object(_) => {
                    unrewritable_tool_key |=
                        detect_unrewritable_object_keys(chain, Direction::Input, doc, &mut counts);
                    redact_value_strings(chain, Direction::Input, doc, &mut counts);
                }
                _ => {}
            }
        }
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

#[cfg(test)]
pub fn redact_rerank_request(chain: &dyn Guardrail, body: &mut Value) -> RedactionCounts {
    redact_rerank_request_structured(chain, body).counts
}

pub async fn moderate_rerank_request_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Input,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_rerank_request_structured(guardrail, body),
    )
    .await
}

/// Mask an image-generation prompt and report the stable `user` identifier
/// when a DLP rule would change it.
pub fn redact_images_request_structured(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_input() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    if let Some(p) = body.get_mut("prompt") {
        apply_to_value_string(chain, Direction::Input, p, &mut counts);
    }
    let unrewritable_tool_key =
        detect_named_structural_fields(chain, Direction::Input, body, &["user"], &mut counts);
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

#[cfg(test)]
pub fn redact_images_request(chain: &dyn Guardrail, body: &mut Value) -> RedactionCounts {
    redact_images_request_structured(chain, body).counts
}

pub async fn moderate_images_request_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Input,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_images_request_structured(guardrail, body),
    )
    .await
}

/// Mask the two prompt-bearing `/v1/audio/speech` fields.
pub fn redact_speech_request_structured(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_input() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    for field in ["input", "instructions"] {
        if let Some(value) = body.get_mut(field) {
            apply_to_value_string(chain, Direction::Input, value, &mut counts);
        }
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key: false,
    }
}

#[cfg(test)]
pub fn redact_speech_request(chain: &dyn Guardrail, body: &mut Value) -> RedactionCounts {
    redact_speech_request_structured(chain, body).counts
}

pub async fn moderate_speech_request_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Input,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_speech_request_structured(guardrail, body),
    )
    .await
}

/// Mask the textual part of an image-generation response.
pub fn redact_images_response_structured(
    chain: &dyn Guardrail,
    body: &mut Value,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if chain.redacts_output() {
        if let Some(Value::Array(items)) = body.get_mut("data") {
            for item in items {
                if let Some(prompt) = item.get_mut("revised_prompt") {
                    apply_to_value_string(chain, Direction::Output, prompt, &mut counts);
                }
            }
        }
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key: false,
    }
}

pub async fn moderate_images_response_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Output,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_images_response_structured(guardrail, body),
    )
    .await
}

/// Mask an audio transcription/translation RESPONSE in place (#696). The
/// wire body is either JSON (`json` / `verbose_json` response_format:
/// top-level `text` + per-segment `segments[].text`) or raw text
/// (`text` / `srt` / `vtt` formats). Returns the rewritten bytes + counts,
/// or `None` when nothing matched (caller keeps the original body). A
/// non-UTF-8 body is left untouched.
pub fn redact_transcription_response(
    chain: &dyn Guardrail,
    body: &[u8],
) -> Option<(Vec<u8>, RedactionCounts)> {
    if !chain.redacts_output() {
        return None;
    }
    let mut counts = RedactionCounts::new();
    if let Ok(mut json) = serde_json::from_slice::<Value>(body) {
        if let Some(text) = json.get_mut("text") {
            apply_to_value_string(chain, Direction::Output, text, &mut counts);
        }
        if let Some(Value::Array(segments)) = json.get_mut("segments") {
            for seg in segments {
                if let Some(text) = seg.get_mut("text") {
                    apply_to_value_string(chain, Direction::Output, text, &mut counts);
                }
            }
        }
        if counts.is_empty() {
            return None;
        }
        return serde_json::to_vec(&json).ok().map(|b| (b, counts));
    }
    let text = std::str::from_utf8(body).ok()?;
    let r = chain.redact_output_text(text)?;
    Some((r.text.into_bytes(), r.counts))
}

/// Mask a legacy `/v1/completions` RESPONSE body in place: `choices[].text`.
pub fn redact_completions_response(chain: &dyn Guardrail, body: &mut Value) -> RedactionCounts {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_output() {
        return counts;
    }
    if let Some(Value::Array(choices)) = body.get_mut("choices") {
        for choice in choices {
            if let Some(text) = choice.get_mut("text") {
                apply_to_value_string(chain, Direction::Output, text, &mut counts);
            }
        }
    }
    counts
}

// ─── Response side (non-streaming) ───────────────────────────────────────────

/// Mask a normalised [`ChatResponse`] in place: assistant `content` plus
/// `tool_calls` function arguments (the same surface
/// `guardrail_output_text` scans). Reasoning content is excluded from
/// guardrail scope by design and stays untouched.
fn redact_chat_response_impl(
    chain: &dyn Guardrail,
    resp: &mut ChatResponse,
    detect_keys: bool,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_output() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    if let Some(content) = resp.message.content.as_mut() {
        apply_to_string(chain, Direction::Output, content, &mut counts);
    }
    if let Some(refusal) = resp.message.extra.get_mut("refusal") {
        apply_to_value_string(chain, Direction::Output, refusal, &mut counts);
    }
    let mut unrewritable_tool_key = false;
    if let Some(transcript) = resp
        .message
        .extra
        .get_mut("audio")
        .and_then(|audio| audio.get_mut("transcript"))
    {
        if detect_keys {
            unrewritable_tool_key |= detect_unrewritable_value_strings(
                chain,
                Direction::Output,
                transcript,
                &mut counts,
            );
        } else {
            apply_to_value_string(chain, Direction::Output, transcript, &mut counts);
        }
    }
    unrewritable_tool_key |= resp
        .message
        .extra
        .get_mut("tool_calls")
        .is_some_and(|tool_calls| {
            redact_tool_call_arguments(
                chain,
                Direction::Output,
                tool_calls,
                &mut counts,
                detect_keys,
            )
        });
    if let Some(function_call) = resp.message.extra.get_mut("function_call") {
        unrewritable_tool_key |= redact_legacy_function_call(
            chain,
            Direction::Output,
            function_call,
            &mut counts,
            detect_keys,
        );
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

#[cfg(test)]
pub fn redact_chat_response(chain: &dyn Guardrail, resp: &mut ChatResponse) -> RedactionCounts {
    redact_chat_response_impl(chain, resp, false).counts
}

/// Mask a normalised response before converting it to Anthropic JSON. Tool
/// argument object keys cannot be renamed safely; a matching key therefore
/// requires the Messages handler to fail closed.
pub fn redact_chat_response_structured(
    chain: &dyn Guardrail,
    resp: &mut ChatResponse,
) -> AnthropicRequestRedaction {
    redact_chat_response_impl(chain, resp, true)
}

pub async fn moderate_chat_response_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    resp: &mut ChatResponse,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Output,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_chat_response_structured(guardrail, resp),
    )
    .await
}

// ─── Response side (streamed, buffered) ──────────────────────────────────────

/// Mask a fully-buffered stream of normalised [`ChatChunk`]s in place —
/// the hold-back release path (BufferFull), where the whole response is
/// available before any byte reaches the wire.
///
/// A masked span can cross chunk boundaries, so per-chunk rewriting would
/// miss it. Instead each content channel (delta content, and each
/// tool-call's streamed `arguments`) is concatenated across the buffered
/// chunks, rewritten once, and the FULL rewritten text re-emitted on the
/// channel's first carrying chunk; later chunks in that channel become
/// empty deltas. The stream is already released en bloc at this point, so
/// chunk-size distribution is not client-observable. Non-content fields
/// (ids, usage, finish_reason, reasoning) are untouched.
fn redact_chat_chunks_impl(
    chain: &dyn Guardrail,
    chunks: &mut [ChatChunk],
    detect_keys: bool,
) -> AnthropicRequestRedaction {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_output() {
        return AnthropicRequestRedaction {
            counts,
            unrewritable_tool_key: false,
        };
    }
    let mut unrewritable_tool_key = false;

    // Content channel: all chunks stream one assistant message.
    let content_sites: Vec<usize> = chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| c.delta.content.as_deref().is_some_and(|t| !t.is_empty()))
        .map(|(i, _)| i)
        .collect();
    if !content_sites.is_empty() {
        let joined: String = content_sites
            .iter()
            .map(|&i| chunks[i].delta.content.as_deref().unwrap_or(""))
            .collect();
        if let Some(r) = chain.redact_output_text(&joined) {
            let mut first = true;
            for &i in &content_sites {
                chunks[i].delta.content = Some(if first {
                    first = false;
                    r.text.clone()
                } else {
                    String::new()
                });
            }
            merge_counts(&mut counts, r.counts);
        }
    }

    // Compatible upstreams sometimes emit already-decoded argument objects
    // even though OpenAI documents a JSON-encoded string. They are still
    // forwarded on the raw wire, so recursively rewrite them before building
    // the normal fragmented-string channels below.
    for chunk in chunks.iter_mut() {
        if let Some(tool_calls) = chunk.delta.tool_calls.as_mut() {
            for tool_call in tool_calls {
                if !tool_call.is_object() {
                    unrewritable_tool_key |= detect_keys;
                    continue;
                }
                let Some(function) = tool_call.get_mut("function") else {
                    continue;
                };
                if !function.is_object() {
                    unrewritable_tool_key |= detect_keys;
                    continue;
                }
                if let Some(arguments) = function.get_mut("arguments") {
                    if !arguments.is_string() {
                        unrewritable_tool_key |= redact_tool_arguments_value(
                            chain,
                            Direction::Output,
                            arguments,
                            &mut counts,
                            detect_keys,
                        );
                    }
                }
            }
        }
        if let Some(function_call) = chunk.delta.function_call.as_mut() {
            if !function_call.is_object() {
                unrewritable_tool_key |= detect_keys;
                continue;
            }
            if let Some(arguments) = function_call.get_mut("arguments") {
                if !arguments.is_string() {
                    unrewritable_tool_key |= redact_tool_arguments_value(
                        chain,
                        Direction::Output,
                        arguments,
                        &mut counts,
                        detect_keys,
                    );
                }
            }
        }
    }

    // Tool-call channels: fragments carry an `index` discriminator; the
    // concatenation of each channel's `function.arguments` strings is the
    // complete JSON-encoded argument document.
    let mut channels: BTreeMap<u64, Vec<(usize, usize)>> = BTreeMap::new();
    let mut structural_channels: BTreeMap<(u64, &'static str), Vec<(usize, usize)>> =
        BTreeMap::new();
    for (ci, chunk) in chunks.iter().enumerate() {
        if let Some(tcs) = chunk.delta.tool_calls.as_ref() {
            for (ti, tc) in tcs.iter().enumerate() {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                for field in ["id", "type", "name"] {
                    let value = match field {
                        "name" => tc.get("function").and_then(|function| function.get("name")),
                        _ => tc.get(field),
                    };
                    if value
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                    {
                        structural_channels
                            .entry((idx, field))
                            .or_default()
                            .push((ci, ti));
                    }
                }
                if tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                {
                    channels.entry(idx).or_default().push((ci, ti));
                }
            }
        }
    }
    for ((_, field), sites) in &structural_channels {
        let joined: String = sites
            .iter()
            .map(|&(ci, ti)| {
                let tc = &chunks[ci].delta.tool_calls.as_ref().unwrap()[ti];
                match *field {
                    "name" => tc.get("function").and_then(|function| function.get("name")),
                    _ => tc.get(*field),
                }
                .and_then(Value::as_str)
                .unwrap_or("")
            })
            .collect();
        unrewritable_tool_key |= detect_unrewritable_value_strings(
            chain,
            Direction::Output,
            &Value::String(joined),
            &mut counts,
        );
    }
    for sites in channels.values() {
        let joined: String = sites
            .iter()
            .map(|&(ci, ti)| {
                chunks[ci].delta.tool_calls.as_ref().unwrap()[ti]
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            })
            .collect();
        let mut rewritten = joined.clone();
        let mut local = RedactionCounts::new();
        unrewritable_tool_key |= redact_json_encoded_structured(
            chain,
            Direction::Output,
            &mut rewritten,
            &mut local,
            detect_keys,
        );
        if local.is_empty() {
            continue;
        }
        let mut first = true;
        for &(ci, ti) in sites {
            let args = chunks[ci].delta.tool_calls.as_mut().unwrap()[ti]
                .get_mut("function")
                .and_then(|f| f.get_mut("arguments"))
                .expect("site was selected for having arguments");
            *args = Value::String(if first {
                first = false;
                rewritten.clone()
            } else {
                String::new()
            });
        }
        merge_counts(&mut counts, local);
    }

    // Deprecated single-function-call channel. OpenAI streams `name` and
    // JSON-encoded `arguments` as fragments just like modern tool calls, but
    // without an index because only one call can be active.
    let legacy_name_sites: Vec<usize> = chunks
        .iter()
        .enumerate()
        .filter(|(_, chunk)| {
            chunk
                .delta
                .function_call
                .as_ref()
                .and_then(|call| call.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|name| !name.is_empty())
        })
        .map(|(index, _)| index)
        .collect();
    if !legacy_name_sites.is_empty() {
        let joined: String = legacy_name_sites
            .iter()
            .filter_map(|&index| {
                chunks[index]
                    .delta
                    .function_call
                    .as_ref()
                    .and_then(|call| call.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();
        unrewritable_tool_key |= detect_unrewritable_value_strings(
            chain,
            Direction::Output,
            &Value::String(joined),
            &mut counts,
        );
    }
    let legacy_argument_sites: Vec<usize> = chunks
        .iter()
        .enumerate()
        .filter(|(_, chunk)| {
            chunk
                .delta
                .function_call
                .as_ref()
                .and_then(|call| call.get("arguments"))
                .and_then(Value::as_str)
                .is_some_and(|arguments| !arguments.is_empty())
        })
        .map(|(index, _)| index)
        .collect();
    if !legacy_argument_sites.is_empty() {
        let joined: String = legacy_argument_sites
            .iter()
            .filter_map(|&index| {
                chunks[index]
                    .delta
                    .function_call
                    .as_ref()
                    .and_then(|call| call.get("arguments"))
                    .and_then(Value::as_str)
            })
            .collect();
        let mut rewritten = joined.clone();
        let mut local = RedactionCounts::new();
        unrewritable_tool_key |= redact_json_encoded_structured(
            chain,
            Direction::Output,
            &mut rewritten,
            &mut local,
            detect_keys,
        );
        if rewritten != joined {
            let mut first = true;
            for &index in &legacy_argument_sites {
                let arguments = chunks[index]
                    .delta
                    .function_call
                    .as_mut()
                    .and_then(|call| call.get_mut("arguments"))
                    .expect("site was selected for having function-call arguments");
                *arguments = Value::String(if first {
                    first = false;
                    rewritten.clone()
                } else {
                    String::new()
                });
            }
        }
        merge_counts(&mut counts, local);
    }

    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

#[cfg(test)]
pub fn redact_chat_chunks(chain: &dyn Guardrail, chunks: &mut [ChatChunk]) -> RedactionCounts {
    redact_chat_chunks_impl(chain, chunks, false).counts
}

/// Mask buffered OpenAI chunks before converting them to Anthropic SSE. Tool
/// argument object keys cannot be renamed safely; a matching key therefore
/// requires the Messages handler to fail closed.
pub fn redact_chat_chunks_structured(
    chain: &dyn Guardrail,
    chunks: &mut [ChatChunk],
) -> AnthropicRequestRedaction {
    redact_chat_chunks_impl(chain, chunks, true)
}

pub async fn moderate_chat_chunks_structured(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    chunks: &mut [ChatChunk],
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Output,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_chat_chunks_structured(guardrail, chunks),
    )
    .await
}

// ─── Anthropic-native SSE (passthrough) rewrite ──────────────────────────────

/// One parsed SSE frame from a buffered Anthropic-native byte stream.
struct SseFrame {
    /// Original frame bytes (no trailing separator). Emitted verbatim
    /// unless `data` was modified.
    raw: Vec<u8>,
    /// Parsed `data:` payload, when the frame carries one.
    data: Option<Value>,
    /// A non-empty, non-sentinel data payload that was not valid JSON.
    malformed_data: bool,
    /// True when the event's joined data payload is the `[DONE]` sentinel.
    sentinel: bool,
    /// Only the stream's first frame may ignore the UTF-8 BOM.
    initial_frame: bool,
    dirty: bool,
    /// Exact blank-line terminator. Empty only for the final frame when the
    /// upstream closes without a trailing SSE separator.
    separator: Vec<u8>,
}

impl SseFrame {
    /// Re-render the frame: the first `data` field is replaced with the
    /// re-serialised payload and subsequent data fields are removed; every
    /// other field/comment and its original line ending pass through.
    fn render(&self) -> Vec<u8> {
        if !self.dirty {
            return self.raw.clone();
        }
        let Some(data) = self.data.as_ref() else {
            return self.raw.clone();
        };
        let encoded = serde_json::to_vec(data).unwrap_or_default();
        let mut out = Vec::with_capacity(self.raw.len());
        let mut position = 0usize;
        let mut first_line = true;
        let mut data_written = false;
        while position < self.raw.len() {
            let (content_end, next) = next_sse_line(&self.raw, position);
            let (line, bom) = sse_field_line(
                &self.raw[position..content_end],
                self.initial_frame && first_line,
            );
            if sse_field_value(line, b"data").is_some() {
                if !data_written {
                    out.extend_from_slice(bom);
                    out.extend_from_slice(b"data: ");
                    out.extend_from_slice(&encoded);
                    out.extend_from_slice(&self.raw[content_end..next]);
                    data_written = true;
                }
            } else {
                out.extend_from_slice(&self.raw[position..next]);
            }
            first_line = false;
            if next == self.raw.len() {
                break;
            }
            position = next;
        }
        out
    }
}

const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";

/// Return the content end and the end of one physical SSE line. WHATWG SSE
/// accepts CRLF, lone CR, and lone LF as line endings.
fn next_sse_line(raw: &[u8], start: usize) -> (usize, usize) {
    let Some(offset) = raw[start..]
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
    else {
        return (raw.len(), raw.len());
    };
    let content_end = start + offset;
    let next = if raw[content_end] == b'\r'
        && raw.get(content_end + 1).is_some_and(|byte| *byte == b'\n')
    {
        content_end + 2
    } else {
        content_end + 1
    };
    (content_end, next)
}

fn sse_field_line(line: &[u8], strip_bom: bool) -> (&[u8], &[u8]) {
    if strip_bom && line.starts_with(UTF8_BOM) {
        (&line[UTF8_BOM.len()..], &line[..UTF8_BOM.len()])
    } else {
        (line, &[])
    }
}

/// Parse one SSE field according to the EventSource algorithm: the field name
/// is the bytes before the first colon and one optional leading space is
/// removed from the value.
fn sse_field_value<'a>(line: &'a [u8], expected: &[u8]) -> Option<&'a [u8]> {
    if line.starts_with(b":") {
        return None;
    }
    let (field, value) = match line.iter().position(|byte| *byte == b':') {
        Some(colon) => (&line[..colon], &line[colon + 1..]),
        None => (line, &[][..]),
    };
    if field != expected {
        return None;
    }
    Some(value.strip_prefix(b" ").unwrap_or(value))
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn parse_sse_frame(raw: &[u8], separator: &[u8], initial_frame: bool) -> SseFrame {
    let mut joined_data = Vec::new();
    let mut position = 0usize;
    let mut first_line = true;
    let mut has_data = false;
    while position < raw.len() {
        let (content_end, next) = next_sse_line(raw, position);
        let (line, _) = sse_field_line(&raw[position..content_end], initial_frame && first_line);
        if let Some(value) = sse_field_value(line, b"data") {
            if has_data {
                joined_data.push(b'\n');
            }
            joined_data.extend_from_slice(value);
            has_data = true;
        }
        first_line = false;
        if next == raw.len() {
            break;
        }
        position = next;
    }

    let payload = trim_ascii(&joined_data);
    let sentinel = payload == b"[DONE]";
    let parsed = if has_data && !payload.is_empty() && !sentinel {
        serde_json::from_slice::<Value>(payload).ok()
    } else {
        None
    };
    let malformed_data = has_data && !payload.is_empty() && !sentinel && parsed.is_none();
    SseFrame {
        raw: raw.to_vec(),
        data: parsed,
        malformed_data,
        sentinel,
        initial_frame,
        dirty: false,
        separator: separator.to_vec(),
    }
}

/// Locate the first complete SSE event and return `(frame_end, event_end)`.
/// `frame_end` excludes the blank-line separator; `event_end` includes it.
fn sse_event_boundary(raw: &[u8]) -> Option<(usize, usize)> {
    let mut position = 0usize;
    let mut previous_line_ending = None;
    while position < raw.len() {
        let line_start = position;
        let (content_end, next) = next_sse_line(raw, position);
        if next == content_end {
            return None;
        }
        if content_end == line_start {
            return Some((previous_line_ending.unwrap_or(line_start), next));
        }
        previous_line_ending = Some(content_end);
        position = next;
    }
    None
}

/// Number of buffered bytes through the first complete SSE event, including
/// its blank-line terminator.
pub(crate) fn first_sse_event_end(raw: &[u8]) -> Option<usize> {
    sse_event_boundary(raw).map(|(_, event_end)| event_end)
}

/// Parse one buffered event. The bool reports a non-empty data payload that
/// was neither `[DONE]` nor valid JSON.
pub(crate) fn parse_sse_json_event(raw: &[u8], initial_frame: bool) -> (Option<Value>, bool) {
    let frame = match sse_event_boundary(raw) {
        Some((frame_end, event_end)) if event_end == raw.len() => {
            parse_sse_frame(&raw[..frame_end], &raw[frame_end..event_end], initial_frame)
        }
        _ => parse_sse_frame(raw, &[], initial_frame),
    };
    (frame.data, frame.malformed_data)
}

/// Whether one complete (or EOF-terminated) SSE event carries `[DONE]`.
/// Uses the same WHATWG line joining, BOM, and whitespace rules as the JSON
/// side-channel parser so terminal detection cannot drift from parsing.
pub(crate) fn is_sse_done_event(raw: &[u8], initial_frame: bool) -> bool {
    let frame = match sse_event_boundary(raw) {
        Some((frame_end, event_end)) if event_end == raw.len() => {
            parse_sse_frame(&raw[..frame_end], &raw[frame_end..event_end], initial_frame)
        }
        _ => parse_sse_frame(raw, &[], initial_frame),
    };
    frame.sentinel
}

/// Split a complete buffered SSE byte stream on every standards-compliant
/// blank-line separator. A final unterminated frame is still parsed: EOF
/// terminates an SSE event, so forwarding it as opaque bytes would let its
/// payload bypass output inspection.
fn split_sse_frames(raw: &[u8]) -> Vec<SseFrame> {
    let mut frames = Vec::new();
    let mut start = 0usize;
    while start < raw.len() {
        let remaining = &raw[start..];
        let initial_frame = frames.is_empty();
        let Some((frame_end, event_end)) = sse_event_boundary(remaining) else {
            frames.push(parse_sse_frame(remaining, &[], initial_frame));
            break;
        };
        frames.push(parse_sse_frame(
            &remaining[..frame_end],
            &remaining[frame_end..event_end],
            initial_frame,
        ));
        start += event_end;
    }
    frames
}

/// Parse all JSON data events in a buffered SSE stream. The bool is true when
/// any non-empty data event was not valid JSON.
pub(crate) fn parse_sse_json_stream(raw: &[u8]) -> (Vec<Value>, bool) {
    let frames = split_sse_frames(raw);
    let malformed = frames.iter().any(|frame| frame.malformed_data);
    let events = frames.into_iter().filter_map(|frame| frame.data).collect();
    (events, malformed)
}

/// Mask a fully-buffered legacy `/v1/completions` SSE response. Choice text
/// is reassembled by `index` before redaction so a sensitive span split across
/// provider chunks cannot evade detection; the rewritten channel is emitted
/// on its first carrying frame and later fragments become empty strings.
pub fn redact_completions_sse(
    chain: &dyn Guardrail,
    raw: &[u8],
) -> Option<(Vec<u8>, RedactionCounts)> {
    if !chain.redacts_output() {
        return None;
    }
    let mut frames = split_sse_frames(raw);
    let mut channels: BTreeMap<u64, Vec<(usize, usize)>> = BTreeMap::new();
    for (frame_index, frame) in frames.iter().enumerate() {
        let Some(choices) = frame
            .data
            .as_ref()
            .and_then(|data| data.get("choices"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for (choice_index, choice) in choices.iter().enumerate() {
            if choice
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
            {
                channels
                    .entry(choice.get("index").and_then(Value::as_u64).unwrap_or(0))
                    .or_default()
                    .push((frame_index, choice_index));
            }
        }
    }

    let mut counts = RedactionCounts::new();
    for sites in channels.values() {
        let joined: String = sites
            .iter()
            .map(|&(frame_index, choice_index)| {
                frames[frame_index]
                    .data
                    .as_ref()
                    .and_then(|data| data.get("choices"))
                    .and_then(Value::as_array)
                    .and_then(|choices| choices.get(choice_index))
                    .and_then(|choice| choice.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            })
            .collect();
        let Some(redaction) = chain.redact_output_text(&joined) else {
            continue;
        };
        let mut first = true;
        for &(frame_index, choice_index) in sites {
            let text = frames[frame_index]
                .data
                .as_mut()
                .and_then(|data| data.get_mut("choices"))
                .and_then(Value::as_array_mut)
                .and_then(|choices| choices.get_mut(choice_index))
                .and_then(|choice| choice.get_mut("text"))
                .expect("site was selected for having choice text");
            *text = Value::String(if first {
                first = false;
                redaction.text.clone()
            } else {
                String::new()
            });
            frames[frame_index].dirty = true;
        }
        merge_counts(&mut counts, redaction.counts);
    }
    if counts.is_empty() {
        return None;
    }

    let mut out = Vec::with_capacity(raw.len());
    for frame in &frames {
        out.extend_from_slice(&frame.render());
        out.extend_from_slice(&frame.separator);
    }
    Some((out, counts))
}

/// Mask a fully-buffered Anthropic-native SSE response (the `/v1/messages`
/// passthrough hold-back). Text deltas are reassembled per content-block
/// `index` (a masked span can cross frame boundaries), masked once, and
/// the full masked text re-emitted on the channel's first frame;
/// `input_json_delta` (tool-use arguments) channels are masked as complete
/// JSON documents. `rewritten: None` means nothing was changed, so callers
/// can forward the original bytes byte-identically. Sensitive JSON object
/// keys are reported separately because renaming a tool argument would change
/// its contract; handlers must fail closed instead.
pub struct AnthropicSseRedaction {
    pub rewritten: Option<Vec<u8>>,
    pub counts: RedactionCounts,
    pub unrewritable_tool_key: bool,
}

pub fn redact_anthropic_sse(chain: &dyn Guardrail, raw: &[u8]) -> AnthropicSseRedaction {
    if !chain.redacts_output() {
        return AnthropicSseRedaction {
            rewritten: None,
            counts: RedactionCounts::new(),
            unrewritable_tool_key: false,
        };
    }
    let mut frames = split_sse_frames(raw);
    let mut counts = RedactionCounts::new();
    let mut unrewritable_tool_key = false;

    // channel key → ordered (frame_idx, kind) sites. Kind distinguishes the
    // JSON path to rewrite inside the frame payload.
    #[derive(Clone, Copy)]
    enum Site {
        DeltaText,
        DeltaPartialJson,
        BlockStartText,
    }
    let mut text_channels: BTreeMap<u64, Vec<(usize, Site)>> = BTreeMap::new();
    let mut json_channels: BTreeMap<u64, Vec<(usize, Site)>> = BTreeMap::new();

    for (fi, frame) in frames.iter().enumerate() {
        let Some(data) = frame.data.as_ref() else {
            continue;
        };
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        match data.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => {
                match data
                    .get("delta")
                    .and_then(|d| d.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("text_delta") => {
                        if data
                            .get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .is_some_and(|t| !t.is_empty())
                        {
                            text_channels
                                .entry(index)
                                .or_default()
                                .push((fi, Site::DeltaText));
                        }
                    }
                    Some("input_json_delta") => {
                        if data
                            .get("delta")
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .is_some_and(|t| !t.is_empty())
                        {
                            json_channels
                                .entry(index)
                                .or_default()
                                .push((fi, Site::DeltaPartialJson));
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_start") => {
                // A `text` block may open with non-empty initial text; it
                // belongs at the head of the same channel as its deltas.
                if data
                    .get("content_block")
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    text_channels
                        .entry(index)
                        .or_default()
                        .push((fi, Site::BlockStartText));
                }
            }
            _ => {}
        }
    }

    fn site_text(data: &Value, site: Site) -> &str {
        let path = match site {
            Site::DeltaText => data.get("delta").and_then(|d| d.get("text")),
            Site::DeltaPartialJson => data.get("delta").and_then(|d| d.get("partial_json")),
            Site::BlockStartText => data.get("content_block").and_then(|b| b.get("text")),
        };
        path.and_then(Value::as_str).unwrap_or("")
    }

    fn site_slot(data: &mut Value, site: Site) -> Option<&mut Value> {
        match site {
            Site::DeltaText => data.get_mut("delta").and_then(|d| d.get_mut("text")),
            Site::DeltaPartialJson => data
                .get_mut("delta")
                .and_then(|d| d.get_mut("partial_json")),
            Site::BlockStartText => data
                .get_mut("content_block")
                .and_then(|b| b.get_mut("text")),
        }
    }

    let rewrite = |frames: &mut Vec<SseFrame>, sites: &[(usize, Site)], new_text: String| {
        let mut first = true;
        for &(fi, site) in sites {
            let frame = &mut frames[fi];
            if let Some(slot) = frame.data.as_mut().and_then(|d| site_slot(d, site)) {
                *slot = Value::String(if first {
                    first = false;
                    new_text.clone()
                } else {
                    String::new()
                });
                frame.dirty = true;
            }
        }
    };

    for sites in text_channels.values() {
        let joined: String = sites
            .iter()
            .map(|&(fi, site)| site_text(frames[fi].data.as_ref().unwrap(), site))
            .collect();
        if let Some(r) = chain.redact_output_text(&joined) {
            rewrite(&mut frames, sites, r.text);
            merge_counts(&mut counts, r.counts);
        }
    }
    for sites in json_channels.values() {
        let joined: String = sites
            .iter()
            .map(|&(fi, site)| site_text(frames[fi].data.as_ref().unwrap(), site))
            .collect();
        let mut rewritten = joined.clone();
        let mut local = RedactionCounts::new();
        unrewritable_tool_key |= redact_json_encoded_structured(
            chain,
            Direction::Output,
            &mut rewritten,
            &mut local,
            true,
        );
        if rewritten != joined {
            rewrite(&mut frames, sites, rewritten);
        }
        merge_counts(&mut counts, local);
    }

    // A block start may carry complete text-bearing content (tool input,
    // document/search text, or a server-tool result) instead of later deltas.
    // Run the same official-block walker used by non-streaming Messages.
    for frame in &mut frames {
        let changed = {
            let Some(data) = frame.data.as_mut() else {
                continue;
            };
            if data.get("type").and_then(Value::as_str) != Some("content_block_start") {
                continue;
            }
            let Some(block) = data.get_mut("content_block") else {
                continue;
            };
            let before = block.clone();
            unrewritable_tool_key |= redact_anthropic_value(
                chain,
                Direction::Output,
                block,
                &mut counts,
                AnthropicKeyContext::Protocol,
            );
            *block != before
        };
        frame.dirty |= changed;
    }

    let rewritten = frames.iter().any(|frame| frame.dirty).then(|| {
        let mut out = Vec::with_capacity(raw.len());
        for frame in &frames {
            out.extend_from_slice(&frame.render());
            out.extend_from_slice(&frame.separator);
        }
        out
    });
    AnthropicSseRedaction {
        rewritten,
        counts,
        unrewritable_tool_key,
    }
}

pub async fn moderate_anthropic_sse(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    held: &mut Vec<u8>,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Output,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| {
            let result = redact_anthropic_sse(guardrail, held);
            if let Some(rewritten) = result.rewritten {
                *held = rewritten;
            }
            AnthropicRequestRedaction {
                counts: result.counts,
                unrewritable_tool_key: result.unrewritable_tool_key,
            }
        },
    )
    .await
}

/// The concatenated TEXT-channel content of a buffered Anthropic-native
/// SSE stream (per content-block `index` order, `content_block_start`
/// head text included). Used to rebuild the content-capture accumulator
/// after a segment (provider-side) mask rewrote the held bytes — the
/// sync redactor can't reproduce a provider mask (#932 × #947).
pub fn anthropic_sse_text(raw: &[u8]) -> String {
    let frames = split_sse_frames(raw);
    let mut channels: BTreeMap<u64, String> = BTreeMap::new();
    for frame in &frames {
        let Some(data) = frame.data.as_ref() else {
            continue;
        };
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        let text = match data.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => data
                .get("delta")
                .filter(|d| d.get("type").and_then(Value::as_str) == Some("text_delta"))
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str),
            Some("content_block_start") => data
                .get("content_block")
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str),
            _ => None,
        };
        if let Some(t) = text {
            channels.entry(index).or_default().push_str(t);
        }
    }
    channels.into_values().collect()
}

/// The concatenated `output_text` delta content of a buffered
/// `/v1/responses` SSE stream (channel order). Same capture-rebuild role
/// as [`anthropic_sse_text`].
pub fn responses_sse_text(raw: &[u8]) -> String {
    let frames = split_sse_frames(raw);
    // First-seen channel order (NOT key order): the rebuilt capture must
    // read in the order the client saw the channels emitted.
    let mut channels: Vec<(String, String)> = Vec::new();
    for frame in &frames {
        let Some(data) = frame.data.as_ref() else {
            continue;
        };
        if !matches!(
            data.get("type").and_then(Value::as_str),
            Some("response.output_text.delta" | "response.refusal.delta")
        ) {
            continue;
        }
        let Some(t) = data.get("delta").and_then(Value::as_str) else {
            continue;
        };
        let key = match data.get("item_id").and_then(Value::as_str) {
            Some(id) => format!(
                "{id}/{}",
                data.get("content_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
            None => format!(
                "{}/{}",
                data.get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                data.get("content_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
        };
        match channels.iter_mut().find(|(k, _)| *k == key) {
            Some((_, buf)) => buf.push_str(t),
            None => channels.push((key, t.to_owned())),
        }
    }
    channels.into_iter().map(|(_, text)| text).collect()
}

// ─── Responses-API SSE rewrite ───────────────────────────────────────────────

/// Mask a fully-buffered Responses-API SSE byte stream (the `/v1/responses`
/// verbatim hold-back and the cross-provider bridge release). Delta events
/// are reassembled per channel (`output_text.delta` by item, `function_call
/// _arguments.delta` by item), masked once, and re-emitted on the channel's
/// first frame; the aggregate events (`*.done`, `output_item.done`,
/// `response.completed`) carry complete texts and are masked directly —
/// deterministic masking keeps them consistent with the delta channels.
/// `None` = nothing matched, forward the original bytes byte-identical.
#[cfg(test)]
pub fn redact_responses_sse(
    chain: &dyn Guardrail,
    raw: &[u8],
) -> Option<(Vec<u8>, RedactionCounts)> {
    let result = redact_responses_sse_impl(chain, raw, false);
    result.rewritten.map(|rewritten| (rewritten, result.counts))
}

pub struct ResponsesSseRedaction {
    pub rewritten: Option<Vec<u8>>,
    pub counts: RedactionCounts,
    pub unrewritable_tool_key: bool,
}

fn redact_responses_sse_impl(
    chain: &dyn Guardrail,
    raw: &[u8],
    detect_keys: bool,
) -> ResponsesSseRedaction {
    if !chain.redacts_output() {
        return ResponsesSseRedaction {
            rewritten: None,
            counts: RedactionCounts::new(),
            unrewritable_tool_key: false,
        };
    }
    let mut frames = split_sse_frames(raw);
    let mut counts = RedactionCounts::new();
    let mut unrewritable_tool_key = false;

    // Delta channels: (event-type discriminant, channel key) → frame sites.
    let mut text_channels: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut args_channels: BTreeMap<(String, bool), Vec<usize>> = BTreeMap::new();

    fn channel_key(data: &Value) -> String {
        // item_id is the stable discriminator; fall back to output_index +
        // content_index for encoders that omit it.
        match data.get("item_id").and_then(Value::as_str) {
            Some(id) => format!(
                "{id}/{}",
                data.get("content_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
            None => format!(
                "{}/{}",
                data.get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                data.get("content_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
        }
    }

    for (fi, frame) in frames.iter().enumerate() {
        let Some(data) = frame.data.as_ref() else {
            continue;
        };
        match data.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta" | "response.refusal.delta") => {
                if data
                    .get("delta")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    text_channels.entry(channel_key(data)).or_default().push(fi);
                }
            }
            Some(
                ty @ ("response.function_call_arguments.delta"
                | "response.mcp_call_arguments.delta"
                | "response.custom_tool_call_input.delta"),
            ) => {
                if data
                    .get("delta")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    let json_encoded = ty != "response.custom_tool_call_input.delta";
                    args_channels
                        .entry((channel_key(data), json_encoded))
                        .or_default()
                        .push(fi);
                }
            }
            _ => {}
        }
    }

    // Rewrite the delta channels (first frame gets the full masked text).
    let rewrite_channel = |frames: &mut Vec<SseFrame>, sites: &[usize], new_text: String| {
        let mut first = true;
        for &fi in sites {
            let frame = &mut frames[fi];
            if let Some(slot) = frame.data.as_mut().and_then(|d| d.get_mut("delta")) {
                *slot = Value::String(if first {
                    first = false;
                    new_text.clone()
                } else {
                    String::new()
                });
                frame.dirty = true;
            }
        }
    };
    for sites in text_channels.values() {
        let joined: String = sites
            .iter()
            .map(|&fi| {
                frames[fi]
                    .data
                    .as_ref()
                    .and_then(|d| d.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            })
            .collect();
        if let Some(r) = chain.redact_output_text(&joined) {
            rewrite_channel(&mut frames, sites, r.text);
            merge_counts(&mut counts, r.counts);
        }
    }
    for ((_, json_encoded), sites) in &args_channels {
        let joined: String = sites
            .iter()
            .map(|&fi| {
                frames[fi]
                    .data
                    .as_ref()
                    .and_then(|d| d.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            })
            .collect();
        let mut rewritten = joined.clone();
        let mut local = RedactionCounts::new();
        if *json_encoded {
            unrewritable_tool_key |= redact_json_encoded_structured(
                chain,
                Direction::Output,
                &mut rewritten,
                &mut local,
                detect_keys,
            );
        } else {
            apply_to_string(chain, Direction::Output, &mut rewritten, &mut local);
        }
        if !local.is_empty() {
            rewrite_channel(&mut frames, sites, rewritten);
            merge_counts(&mut counts, local);
        }
    }

    // Aggregate events carry complete texts — mask them in place. Their
    // counts are NOT merged into the totals: they duplicate the delta
    // channels' matches (the audit count is per span served, not per
    // wire occurrence). Only count them when the delta channel was absent
    // (e.g. a `.done`-only encoder).
    for frame in frames.iter_mut() {
        let Some(data) = frame.data.as_mut() else {
            continue;
        };
        let ty = data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut local = RedactionCounts::new();
        match ty.as_str() {
            "response.output_text.done" => {
                if let Some(text) = data.get_mut("text") {
                    apply_to_value_string(chain, Direction::Output, text, &mut local);
                }
            }
            "response.refusal.done" => {
                if let Some(text) = data.get_mut("refusal") {
                    apply_to_value_string(chain, Direction::Output, text, &mut local);
                }
            }
            "response.content_part.done" => {
                if let Some(part) = data.get_mut("part") {
                    for field in ["text", "refusal"] {
                        if let Some(text) = part.get_mut(field) {
                            apply_to_value_string(chain, Direction::Output, text, &mut local);
                        }
                    }
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(Value::String(args)) = data.get_mut("arguments") {
                    let mut owned = std::mem::take(args);
                    unrewritable_tool_key |= redact_json_encoded_structured(
                        chain,
                        Direction::Output,
                        &mut owned,
                        &mut local,
                        detect_keys,
                    );
                    *args = owned;
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = data.get_mut("item") {
                    unrewritable_tool_key |= redact_responses_item_structured(
                        chain,
                        Direction::Output,
                        item,
                        &mut local,
                        detect_keys,
                    );
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(Value::Array(items)) =
                    data.get_mut("response").and_then(|r| r.get_mut("output"))
                {
                    for item in items {
                        unrewritable_tool_key |= redact_responses_item_structured(
                            chain,
                            Direction::Output,
                            item,
                            &mut local,
                            detect_keys,
                        );
                    }
                }
            }
            _ => {}
        }
        if !local.is_empty() {
            frame.dirty = true;
            if counts.is_empty() {
                merge_counts(&mut counts, local);
            }
        }
    }

    let any_dirty = frames.iter().any(|f| f.dirty);
    let rewritten = any_dirty.then(|| {
        let mut out = Vec::with_capacity(raw.len());
        for frame in &frames {
            out.extend_from_slice(&frame.render());
            out.extend_from_slice(&frame.separator);
        }
        out
    });
    ResponsesSseRedaction {
        rewritten,
        counts,
        unrewritable_tool_key,
    }
}

pub fn redact_responses_sse_structured(chain: &dyn Guardrail, raw: &[u8]) -> ResponsesSseRedaction {
    redact_responses_sse_impl(chain, raw, true)
}

pub async fn moderate_responses_sse(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    held: &mut Vec<u8>,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_structured_body(
        chain,
        Direction::Output,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| {
            let result = redact_responses_sse_structured(guardrail, held);
            if let Some(rewritten) = result.rewritten {
                *held = rewritten;
            }
            AnthropicRequestRedaction {
                counts: result.counts,
                unrewritable_tool_key: result.unrewritable_tool_key,
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_gateway::{ChatDelta, ChatMessage};
    use aisix_guardrails::{builtin_rule, GuardrailChain, PiiAction, PiiGuardrail};
    use serde_json::json;
    use std::sync::Arc;

    fn mask_chain(hook: aisix_core::models::GuardrailHookPoint) -> Arc<dyn Guardrail> {
        let g = PiiGuardrail::new(
            vec![
                builtin_rule("email", PiiAction::Mask).unwrap(),
                builtin_rule("china_mobile", PiiAction::Mask).unwrap(),
                builtin_rule("api_key", PiiAction::Mask).unwrap(),
            ],
            hook,
            262_144,
            false,
        );
        Arc::new(GuardrailChain::new(vec![Arc::new(g)]))
    }

    fn both() -> Arc<dyn Guardrail> {
        mask_chain(aisix_core::models::GuardrailHookPoint::Both)
    }

    /// `null` is OpenAI's documented "absent" encoding for the optional
    /// `tool_calls` / `function_call` message fields and for a tool call's
    /// `arguments`. `message_scan_text` and the OpenAI response ingress both
    /// already skip it, so the structured walker must not treat it as an
    /// un-inspectable shape and reject a request carrying no sensitive data.
    #[test]
    fn structured_chat_treats_explicit_null_tool_fields_as_absent() {
        let chain = both();
        for body in [
            json!({"model": "m", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi", "function_call": null}]}),
            json!({"model": "m", "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi", "tool_calls": null}]}),
            json!({"model": "m", "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "f", "arguments": null}}]}]}),
        ] {
            let mut req: ChatFormat = serde_json::from_value(body.clone()).unwrap();
            let redaction = redact_chat_format_structured(chain.as_ref(), &mut req);
            assert!(
                !redaction.unrewritable_tool_key,
                "an explicit null carries no text to fail closed over: {body}"
            );
            assert!(redaction.counts.is_empty(), "nothing was detected: {body}");
        }
    }

    #[test]
    fn chat_format_masks_content_blocks_and_history_tool_args() {
        let chain = both();
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "mail a@x.com"},
                {"role": "user", "content": "", "content_blocks": [
                    {"type": "text", "text": "call 13800138000"},
                    {"type": "image_url", "image_url": {"url": "http://x"}}
                ]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"index": 0, "function": {"name": "send", "arguments": "{\"to\":\"b@y.org\"}"}}
                ]},
                {"role": "assistant", "content": null, "function_call": {
                    "name": "legacy_send", "arguments": "{\"to\":\"c@z.io\"}"
                }}
            ]
        }))
        .unwrap();
        let counts = redact_chat_format(chain.as_ref(), &mut req);
        assert_eq!(
            req.messages[0].content.as_deref(),
            Some("mail [EMAIL_REDACTED]")
        );
        let blocks = req.messages[1].content_blocks.as_ref().unwrap();
        assert_eq!(
            blocks[0].get("text").unwrap().as_str().unwrap(),
            "call [CHINA_MOBILE_REDACTED]",
        );
        let args = req.messages[2].extra["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(args, "{\"to\":\"[EMAIL_REDACTED]\"}");
        assert_eq!(
            req.messages[3].extra["function_call"]["arguments"],
            "{\"to\":\"[EMAIL_REDACTED]\"}"
        );
        assert_eq!(counts.get("email"), Some(&3));
        assert_eq!(counts.get("china_mobile"), Some(&1));
    }

    #[test]
    fn chat_request_masks_legacy_functions_and_prediction_content() {
        let chain = both();
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "clean"}],
            "functions": [{
                "name": "lookup",
                "description": "contact a@x.com",
                "parameters": {"type": "object", "properties": {
                    "owner": {"type": "string", "default": "b@y.org"}
                }}
            }],
            "function_call": {"name": "c@z.io"},
            "prediction": {"type": "content", "content": [
                {"type": "text", "text": "cached d@w.dev"}
            ]}
        }))
        .unwrap();

        let inspection = chat_request_for_inspection(&req);
        assert!(inspection
            .messages
            .iter()
            .any(|message| message.content_str().contains("d@w.dev")));
        let redaction = redact_chat_format_structured(chain.as_ref(), &mut req);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(
            req.extra["functions"][0]["description"],
            "contact [EMAIL_REDACTED]"
        );
        assert_eq!(
            req.extra["functions"][0]["parameters"]["properties"]["owner"]["default"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(
            req.extra["prediction"]["content"][0]["text"],
            "cached [EMAIL_REDACTED]"
        );
        assert_eq!(req.extra["function_call"]["name"], "c@z.io");
    }

    #[test]
    fn input_only_chain_skips_output_and_vice_versa() {
        let input_only = mask_chain(aisix_core::models::GuardrailHookPoint::Input);
        let mut resp = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: ChatMessage::assistant("mail a@x.com"),
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };
        assert!(redact_chat_response(input_only.as_ref(), &mut resp).is_empty());
        assert_eq!(resp.message.content.as_deref(), Some("mail a@x.com"));

        let output_only = mask_chain(aisix_core::models::GuardrailHookPoint::Output);
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "mail a@x.com"}]
        }))
        .unwrap();
        assert!(redact_chat_format(output_only.as_ref(), &mut req).is_empty());
        assert_eq!(req.messages[0].content.as_deref(), Some("mail a@x.com"));
    }

    #[test]
    fn chat_response_masks_content_and_tool_args_json_safely() {
        let chain = both();
        let mut msg = ChatMessage::assistant("reach me at a@x.com");
        msg.extra.insert(
            "tool_calls".into(),
            json!([{
                "id": "call_1", "type": "function",
                // A number-typed phone stays untouched (JSON preserved);
                // the string email is masked.
                "function": {"name": "f", "arguments": "{\"phone\":13800138000,\"mail\":\"b@y.org\"}"}
            }]),
        );
        msg.extra
            .insert("refusal".into(), json!("cannot help c@z.io"));
        msg.extra.insert(
            "function_call".into(),
            json!({"name": "legacy", "arguments": "{\"mail\":\"d@w.dev\"}"}),
        );
        let mut resp = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: msg,
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };
        let counts = redact_chat_response(chain.as_ref(), &mut resp);
        assert_eq!(
            resp.message.content.as_deref(),
            Some("reach me at [EMAIL_REDACTED]")
        );
        let args = resp.message.extra["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: Value = serde_json::from_str(args).expect("args stay valid JSON");
        assert_eq!(parsed["phone"], json!(13800138000u64));
        assert_eq!(parsed["mail"], json!("[EMAIL_REDACTED]"));
        assert_eq!(
            resp.message.extra["refusal"],
            "cannot help [EMAIL_REDACTED]"
        );
        assert_eq!(
            resp.message.extra["function_call"]["arguments"],
            "{\"mail\":\"[EMAIL_REDACTED]\"}"
        );
        assert_eq!(counts.get("email"), Some(&4));
    }

    #[test]
    fn chat_audio_transcript_is_unrewritable_sensitive_output() {
        let chain = both();
        let mut msg = ChatMessage::assistant("");
        msg.extra.insert(
            "audio".into(),
            json!({"id": "audio_1", "data": "encoded", "transcript": "call a@x.com"}),
        );
        let mut resp = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: msg,
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };
        let redaction = redact_chat_response_structured(chain.as_ref(), &mut resp);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(resp.message.extra["audio"]["transcript"], "call a@x.com");
    }

    #[test]
    fn structured_chat_response_flags_tool_argument_keys() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let mut msg = ChatMessage::assistant("");
        msg.extra.insert(
            "tool_calls".into(),
            json!([{
                "id": "call_1", "type": "function",
                "function": {
                    "name": "lookup",
                    "arguments": format!(r#"{{"{sensitive_key}":"b@y.org"}}"#)
                }
            }]),
        );
        let mut resp = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: msg,
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };

        let redaction = redact_chat_response_structured(chain.as_ref(), &mut resp);
        let args = resp.message.extra["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(parsed[sensitive_key], "[EMAIL_REDACTED]");
        assert_eq!(redaction.counts.get("email"), Some(&2));
    }

    #[test]
    fn structured_chat_masks_decoded_argument_values_and_rejects_invalid_shapes() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let mut msg = ChatMessage::assistant("");
        msg.extra.insert(
            "tool_calls".into(),
            json!([{
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "lookup",
                    "arguments": {(sensitive_key): "b@y.org"}
                }
            }]),
        );
        msg.extra.insert(
            "function_call".into(),
            json!({"name": "legacy", "arguments": {"owner": "c@z.io"}}),
        );
        let mut response = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: msg,
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };
        let redaction = redact_chat_response_structured(chain.as_ref(), &mut response);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(
            response.message.extra["tool_calls"][0]["function"]["arguments"][sensitive_key],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(
            response.message.extra["function_call"]["arguments"]["owner"],
            "[EMAIL_REDACTED]"
        );

        response
            .message
            .extra
            .insert("tool_calls".into(), json!({}));
        response
            .message
            .extra
            .insert("function_call".into(), json!(42));
        let malformed = redact_chat_response_structured(chain.as_ref(), &mut response);
        assert!(malformed.unrewritable_tool_key);
    }

    #[test]
    fn structured_tool_identifiers_fail_closed_without_rewriting_contracts() {
        let chain = both();
        let secret = "sk-abcdefghijklmnopqrstuv";

        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": secret,
                    "type": "function",
                    "function": {"name": secret, "arguments": "{}"}
                }]
            }],
            "tools": [{
                "type": "function",
                "function": {"name": secret, "description": "mail a@x.com"}
            }],
            "tool_choice": {"type": "function", "function": {"name": secret}}
        }))
        .unwrap();
        let chat_redaction = redact_chat_format_structured(chain.as_ref(), &mut chat);
        assert!(chat_redaction.unrewritable_tool_key);
        assert_eq!(chat.extra["tools"][0]["function"]["name"], secret);
        assert_eq!(
            chat.extra["tools"][0]["function"]["description"],
            "mail [EMAIL_REDACTED]"
        );
        assert_eq!(
            chat.messages[0].extra["tool_calls"][0]["function"]["name"],
            secret
        );
        assert_eq!(chat.messages[0].extra["tool_calls"][0]["id"], secret);
        assert_eq!(chat.extra["tool_choice"]["function"]["name"], secret);

        let mut anthropic = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [{
                "type": "tool_use", "id": secret, "name": secret, "input": {}
            }]}],
            "tools": [{"name": secret, "description": "mail b@y.org"}],
            "tool_choice": {"type": "tool", "name": secret}
        });
        let anthropic_redaction = redact_anthropic_request(chain.as_ref(), &mut anthropic);
        assert!(anthropic_redaction.unrewritable_tool_key);
        assert_eq!(anthropic["tools"][0]["name"], secret);
        assert_eq!(anthropic["messages"][0]["content"][0]["name"], secret);
        assert_eq!(anthropic["tool_choice"]["name"], secret);

        let mut responses = json!({
            "model": "m",
            "input": [{
                "type": "function_call", "id": secret, "call_id": secret,
                "name": secret, "arguments": "{}"
            }, {
                "type": "mcp_approval_response", "approval_request_id": secret,
                "approve": true
            }],
            "tools": [{"type": "function", "name": secret, "description": "mail c@z.io"}],
            "tool_choice": {"type": "function", "name": secret}
        });
        let responses_redaction =
            redact_responses_request_structured(chain.as_ref(), &mut responses);
        assert!(responses_redaction.unrewritable_tool_key);
        assert_eq!(responses["tools"][0]["name"], secret);
        assert_eq!(responses["input"][0]["name"], secret);
        assert_eq!(responses["input"][1]["approval_request_id"], secret);
        assert_eq!(responses["tool_choice"]["name"], secret);
    }

    #[test]
    fn structured_output_tool_identifiers_fail_closed_across_wire_shapes() {
        let chain = both();
        let secret = "sk-abcdefghijklmnopqrstuv";

        let mut message = ChatMessage::assistant("");
        message.extra.insert(
            "tool_calls".into(),
            json!([{
                "id": secret,
                "type": "function",
                "function": {"name": secret, "arguments": "{}"}
            }]),
        );
        let mut response = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message,
            finish_reason: aisix_gateway::FinishReason::ToolCalls,
            usage: aisix_gateway::UsageStats::default(),
        };
        let redaction = redact_chat_response_structured(chain.as_ref(), &mut response);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(
            response.message.extra["tool_calls"][0]["function"]["name"],
            secret
        );

        let mut chunks = vec![
            ChatChunk {
                id: "c".into(),
                model: "m".into(),
                delta: ChatDelta {
                    tool_calls: Some(vec![json!({
                        "index": 0,
                        "id": "sk-abcdefghij",
                        "function": {"name": "sk-abcdefghij", "arguments": ""}
                    })]),
                    ..ChatDelta::default()
                },
                finish_reason: None,
                usage: None,
            },
            ChatChunk {
                id: "c".into(),
                model: "m".into(),
                delta: ChatDelta {
                    tool_calls: Some(vec![json!({
                        "index": 0,
                        "id": "klmnopqrstuv",
                        "function": {"name": "klmnopqrstuv", "arguments": "{}"}
                    })]),
                    ..ChatDelta::default()
                },
                finish_reason: None,
                usage: None,
            },
        ];
        let redaction = redact_chat_chunks_structured(chain.as_ref(), &mut chunks);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(
            chunks[0].delta.tool_calls.as_ref().unwrap()[0]["id"],
            "sk-abcdefghij"
        );

        let anthropic = format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{secret}\",\"name\":\"{secret}\",\"input\":{{}}}}}}\n\n"
        );
        let redaction = redact_anthropic_sse(chain.as_ref(), anthropic.as_bytes());
        assert!(redaction.unrewritable_tool_key);
        assert!(
            String::from_utf8_lossy(&redaction.rewritten.unwrap_or_else(|| anthropic.into()))
                .contains(secret)
        );

        let responses = format!(
            "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"{secret}\",\"arguments\":\"{{}}\"}}}}\n\n"
        );
        let redaction = redact_responses_sse_structured(chain.as_ref(), responses.as_bytes());
        assert!(redaction.unrewritable_tool_key);
    }

    #[test]
    fn anthropic_request_masks_system_text_blocks_and_tool_result() {
        let chain = both();
        let mut body = json!({
            "model": "claude",
            "system": [{"type": "text", "text": "user email a@x.com"}],
            "messages": [
                {"role": "user", "content": "call 13800138000"},
                {"role": "user", "content": [
                    {"type": "text", "text": "and b@y.org"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "result c@z.io"}
                    ]}
                ]}
            ]
        });
        let redaction = redact_anthropic_request(chain.as_ref(), &mut body);
        assert_eq!(body["system"][0]["text"], "user email [EMAIL_REDACTED]");
        assert_eq!(
            body["messages"][0]["content"],
            "call [CHINA_MOBILE_REDACTED]"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["text"],
            "and [EMAIL_REDACTED]"
        );
        assert_eq!(
            body["messages"][1]["content"][1]["content"][0]["text"],
            "result [EMAIL_REDACTED]",
        );
        assert_eq!(redaction.counts.get("email"), Some(&3));
        assert!(!redaction.unrewritable_tool_key);
    }

    #[test]
    fn anthropic_request_masks_tool_description_and_schema_values() {
        let chain = both();
        let mut body = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "clean"}],
            "tools": [{
                "name": "lookup",
                "description": "contact a@x.com",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "owner": {"type": "string", "default": "b@y.org"}
                    }
                }
            }]
        });
        let redaction = redact_anthropic_request(chain.as_ref(), &mut body);
        assert_eq!(body["tools"][0]["description"], "contact [EMAIL_REDACTED]");
        assert_eq!(
            body["tools"][0]["input_schema"]["properties"]["owner"]["default"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(redaction.counts.get("email"), Some(&2));
        assert!(!redaction.unrewritable_tool_key);
    }

    #[test]
    fn anthropic_request_flags_sensitive_tool_keys_without_renaming_them() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let mut body = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "clean"}],
            "tools": [{
                "name": "lookup",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        (sensitive_key): {"type": "string"}
                    }
                }
            }]
        });

        let redaction = redact_anthropic_request(chain.as_ref(), &mut body);
        assert!(redaction.unrewritable_tool_key);
        assert!(body["tools"][0]["input_schema"]["properties"]
            .get(sensitive_key)
            .is_some());
        assert_eq!(redaction.counts.get("email"), Some(&1));
    }

    #[test]
    fn anthropic_request_flags_historical_tool_input_keys_without_renaming_them() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let mut body = json!({
            "model": "claude",
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "lookup",
                    "input": {(sensitive_key): "safe"}
                }]
            }]
        });

        let redaction = redact_anthropic_request(chain.as_ref(), &mut body);
        assert!(redaction.unrewritable_tool_key);
        assert!(body["messages"][0]["content"][0]["input"]
            .get(sensitive_key)
            .is_some());
        assert_eq!(redaction.counts.get("email"), Some(&1));
    }

    #[test]
    fn anthropic_inspection_projection_includes_tools_and_rejects_bad_shape() {
        let body = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "clean"}],
            "tools": [{"name": "lookup", "description": "contact a@x.com"}]
        });
        let chat = anthropic_request_for_inspection(&body).unwrap();
        assert!(chat.messages.iter().any(|message| message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("a@x.com"))));
        assert!(anthropic_request_for_inspection(&json!({
            "model": "claude",
            "messages": "not-an-array"
        }))
        .is_err());
    }

    #[test]
    fn anthropic_inspection_does_not_duplicate_adapter_projected_text() {
        for content in [
            json!("ordinary text"),
            json!([{
                "type": "text",
                "text": "ordinary text"
            }]),
        ] {
            let body = json!({
                "model": "claude",
                "messages": [{"role": "user", "content": content}]
            });
            let chat = anthropic_request_for_inspection(&body).unwrap();
            let occurrences = chat
                .messages
                .iter()
                .filter_map(|message| message.content.as_deref())
                .map(|text| text.matches("ordinary text").count())
                .sum::<usize>();
            assert_eq!(occurrences, 1, "messages: {:?}", chat.messages);
        }

        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {
                    "type": "text",
                    "text": "ordinary text",
                    "citations": [{
                        "type": "char_location",
                        "cited_text": "citation-only marker",
                        "document_index": 0,
                        "document_title": "citation title",
                        "start_char_index": 0,
                        "end_char_index": 1
                    }]
                },
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "lookup",
                    "input": {},
                    "caller": {"type": "code_execution_20250825", "tool_id": "caller-only marker"}
                }
            ]}]
        });
        let chat = anthropic_request_for_inspection(&body).unwrap();
        let joined = chat
            .messages
            .iter()
            .filter_map(|message| message.content.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(joined.matches("ordinary text").count(), 1);
        assert_eq!(joined.matches("citation-only marker").count(), 1);
        assert_eq!(joined.matches("citation title").count(), 1);
        assert_eq!(joined.matches("caller-only marker").count(), 1);

        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [{
                "type": "web_fetch_tool_result",
                "tool_use_id": "toolu_fetch",
                "content": {
                    "type": "web_fetch_result",
                    "retrieved_at": "2026-08-18T00:00:00Z",
                    "url": "https://example.test/page",
                    "content": {
                        "type": "document",
                        "source": {"type": "text", "data": "server-only text"},
                        "title": "fetched page"
                    }
                }
            }]}]
        });
        let chat = anthropic_request_for_inspection(&body).unwrap();
        assert!(chat.messages.iter().any(|message| message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("server-only text"))));
    }

    #[test]
    fn anthropic_response_masks_text_and_tool_use_input() {
        let chain = both();
        let mut body = json!({
            "content": [
                {"type": "text", "text": "email a@x.com"},
                {"type": "tool_use", "id": "t", "name": "send",
                 "input": {"to": "b@y.org", "count": 3}}
            ]
        });
        let redaction = redact_anthropic_response(chain.as_ref(), &mut body);
        assert_eq!(body["content"][0]["text"], "email [EMAIL_REDACTED]");
        assert_eq!(body["content"][1]["input"]["to"], "[EMAIL_REDACTED]");
        assert_eq!(body["content"][1]["input"]["count"], 3);
        assert_eq!(redaction.counts.get("email"), Some(&2));
        assert!(!redaction.unrewritable_tool_key);
    }

    #[test]
    fn anthropic_walks_document_and_server_tool_result_text() {
        let chain = both();
        let mut request = json!({
            "messages": [{"role": "user", "content": [{
                "type": "document",
                "source": {"type": "text", "media_type": "text/plain", "data": "a@x.com"},
                "title": "owner b@y.org",
                "context": "contact c@z.io"
            }]}]
        });
        let redaction = redact_anthropic_request(chain.as_ref(), &mut request);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(
            request["messages"][0]["content"][0]["source"]["data"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(
            request["messages"][0]["content"][0]["title"],
            "owner [EMAIL_REDACTED]"
        );
        assert_eq!(
            request["messages"][0]["content"][0]["context"],
            "contact [EMAIL_REDACTED]"
        );

        let mut response = json!({"content": [{
            "type": "bash_code_execution_tool_result",
            "tool_use_id": "toolu_1",
            "content": {"type": "bash_code_execution_result", "stdout": "d@w.dev", "stderr": "e@v.net"}
        }]});
        let redaction = redact_anthropic_response(chain.as_ref(), &mut response);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(
            response["content"][0]["content"]["stdout"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(
            response["content"][0]["content"]["stderr"],
            "[EMAIL_REDACTED]"
        );
    }

    #[test]
    fn anthropic_projection_excludes_server_tool_protocol_metadata_and_ciphertext() {
        let content = json!([
            {
                "type": "web_search_tool_result",
                "caller": {"type": "code_execution_20250825", "tool_id": "srvtool_1"},
                "tool_use_id": "toolu_search",
                "content": [{
                    "type": "web_search_result",
                    "encrypted_content": "cipher-a@x.com",
                    "page_age": "1 day ago",
                    "title": "search result",
                    "url": "https://example.test/result"
                }]
            },
            {
                "type": "web_fetch_tool_result",
                "tool_use_id": "toolu_fetch",
                "content": {
                    "type": "web_fetch_result",
                    "retrieved_at": "2026-08-18T00:00:00Z",
                    "url": "https://example.test/page",
                    "content": {
                        "type": "document",
                        "source": {"type": "text", "data": "visible a@x.com"},
                        "title": "fetched page"
                    }
                }
            },
            {
                "type": "code_execution_tool_result",
                "tool_use_id": "toolu_code",
                "content": {
                    "type": "encrypted_code_execution_result",
                    "encrypted_stdout": "cipher-b@y.org",
                    "return_code": 0,
                    "stderr": "visible c@z.io",
                    "content": []
                }
            },
            {
                "type": "text",
                "text": "answer text",
                "cache_control": {"type": "ephemeral", "ttl": "5m"},
                "citations": [{
                    "type": "char_location",
                    "cited_text": "quoted h@s.dev",
                    "document_index": 0,
                    "document_title": "title i@r.net",
                    "start_char_index": 1,
                    "end_char_index": 2
                }]
            }
        ]);

        let (projection, truncated) =
            anthropic_content_inspection_text_capped(&content, usize::MAX);
        assert!(!truncated);
        assert!(projection.contains("visible a@x.com"));
        assert!(projection.contains("visible c@z.io"));
        assert!(projection.contains("quoted h@s.dev"));
        assert!(projection.contains("title i@r.net"));
        for excluded in [
            "caller",
            "tool_id",
            "page_age",
            "retrieved_at",
            "return_code",
            "encrypted_stdout",
            "cache_control",
            "ttl",
            "document_index",
            "document_title",
            "start_char_index",
            "end_char_index",
            "cipher-a@x.com",
            "cipher-b@y.org",
        ] {
            assert!(!projection.contains(excluded), "projection: {projection}");
        }
    }

    #[test]
    fn anthropic_protocol_field_names_remain_guarded_inside_tool_input() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let mut body = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "lookup",
                "input": {
                    (sensitive_key): "safe",
                    "error_code": "b@y.org"
                }
            }]
        });

        let redaction = redact_anthropic_response(chain.as_ref(), &mut body);
        assert!(redaction.unrewritable_tool_key);
        assert!(body["content"][0]["input"].get(sensitive_key).is_some());
        assert_eq!(
            body["content"][0]["input"]["error_code"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(redaction.counts.get("email"), Some(&2));
    }

    #[test]
    fn anthropic_server_tool_input_excludes_protocol_keys_but_scans_values() {
        let content = json!([{
            "type": "server_tool_use",
            "id": "srvtoolu_1",
            "name": "web_search",
            "input": {
                "query": "weather for a@x.com",
                "type": "request for b@y.org",
                "max_uses": 3,
                "options": {"domain": "example.test"}
            }
        }]);
        let (projection, truncated) =
            anthropic_content_inspection_text_capped(&content, usize::MAX);
        assert!(!truncated);
        assert!(projection.contains("weather for a@x.com"));
        assert!(projection.lines().all(|line| line != "query"));
        assert!(projection.lines().all(|line| line != "max_uses"));
        assert!(projection.lines().all(|line| line != "options"));
        assert!(projection.lines().all(|line| line != "domain"));

        let chain = both();
        let mut response = json!({"content": content});
        let redaction = redact_anthropic_response(chain.as_ref(), &mut response);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(
            response["content"][0]["input"]["query"],
            "weather for [EMAIL_REDACTED]"
        );
        assert_eq!(
            response["content"][0]["input"]["type"],
            "request for [EMAIL_REDACTED]"
        );
        assert_eq!(redaction.counts.get("email"), Some(&2));
    }

    #[test]
    fn anthropic_tool_payload_keys_do_not_inherit_official_opaque_exemptions() {
        let chain = both();
        let mut response = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "lookup",
                "input": {
                    "encrypted_content": "a@x.com",
                    "encrypted_stdout": "b@y.org",
                    "nested_thinking": {"type": "thinking", "owner": "c@z.io"},
                    "nested_url": {"type": "url", "data": "d@w.dev"},
                    "id": "e@v.net"
                }
            }]
        });
        let redaction = redact_anthropic_response(chain.as_ref(), &mut response);
        assert!(!redaction.unrewritable_tool_key);
        for field in [
            &response["content"][0]["input"]["encrypted_content"],
            &response["content"][0]["input"]["encrypted_stdout"],
            &response["content"][0]["input"]["nested_thinking"]["owner"],
            &response["content"][0]["input"]["nested_url"]["data"],
            &response["content"][0]["input"]["id"],
        ] {
            assert_eq!(field, "[EMAIL_REDACTED]");
        }
        assert_eq!(redaction.counts.get("email"), Some(&5));

        let collector = SegmentCollector::default();
        let mut segment_body = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_2",
                "name": "lookup",
                "input": {
                    "encrypted_content": "f@u.org",
                    "nested": {"type": "redacted_thinking", "owner": "g@t.io"}
                }
            }]
        });
        let result = redact_anthropic_response(&collector, &mut segment_body);
        assert!(!result.unrewritable_tool_key);
        let segments = collector.take();
        assert!(segments.iter().any(|segment| segment == "f@u.org"));
        assert!(segments.iter().any(|segment| segment == "g@t.io"));
    }

    #[test]
    fn anthropic_tool_schema_keys_do_not_inherit_official_opaque_exemptions() {
        let chain = both();
        let mut request = json!({
            "tools": [{
                "name": "lookup",
                "description": "clean",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "encrypted_content": {"type": "string", "default": "a@x.com"},
                        "thinking": {"type": "string", "default": "b@y.org"}
                    }
                }
            }]
        });
        let redaction = redact_anthropic_request(chain.as_ref(), &mut request);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(
            request["tools"][0]["input_schema"]["properties"]["encrypted_content"]["default"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(
            request["tools"][0]["input_schema"]["properties"]["thinking"]["default"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(redaction.counts.get("email"), Some(&2));
    }

    #[test]
    fn anthropic_sse_masks_server_tool_result_block_start() {
        let chain = both();
        let raw = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"bash_code_execution_tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":{\"type\":\"bash_code_execution_result\",\"stdout\":\"a@x.com\",\"stderr\":\"b@y.org\"}}}\n\n";
        let result = redact_anthropic_sse(chain.as_ref(), raw);
        assert!(!result.unrewritable_tool_key);
        let rendered = String::from_utf8(result.rewritten.unwrap()).unwrap();
        assert!(!rendered.contains("a@x.com"));
        assert!(!rendered.contains("b@y.org"));
        assert_eq!(result.counts.get("email"), Some(&2));
    }

    #[test]
    fn anthropic_response_flags_sensitive_tool_input_keys_without_renaming_them() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let mut body = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "lookup",
                "input": {(sensitive_key): "safe"}
            }]
        });

        let redaction = redact_anthropic_response(chain.as_ref(), &mut body);
        assert!(redaction.unrewritable_tool_key);
        assert!(body["content"][0]["input"].get(sensitive_key).is_some());
        assert_eq!(redaction.counts.get("email"), Some(&1));
    }

    #[test]
    fn responses_request_masks_string_and_item_forms() {
        let chain = both();
        let mut body = json!({
            "model": "m",
            "instructions": "never leak a@x.com",
            "input": [
                {"type": "message", "role": "user", "content": "call 13800138000"},
                {"role": "user", "content": [
                    {"type": "input_text", "text": "mail b@y.org"}
                ]},
                {"type": "function_call_output", "call_id": "c", "output": "from c@z.io"},
                {"type": "custom_tool_call_output", "call_id": "d", "output": [
                    {"type": "input_text", "text": "custom d@w.dev"}
                ]},
                {"type": "mcp_approval_response", "reason": "approve e@v.net"}
            ]
        });
        let counts = redact_responses_request(chain.as_ref(), &mut body);
        assert_eq!(body["instructions"], "never leak [EMAIL_REDACTED]");
        assert_eq!(body["input"][0]["content"], "call [CHINA_MOBILE_REDACTED]");
        assert_eq!(
            body["input"][1]["content"][0]["text"],
            "mail [EMAIL_REDACTED]"
        );
        assert_eq!(body["input"][2]["output"], "from [EMAIL_REDACTED]");
        assert_eq!(
            body["input"][3]["output"][0]["text"],
            "custom [EMAIL_REDACTED]"
        );
        assert_eq!(body["input"][4]["reason"], "approve [EMAIL_REDACTED]");
        assert_eq!(counts.get("email"), Some(&5));

        let mut simple = json!({"model": "m", "input": "mail a@x.com"});
        redact_responses_request(chain.as_ref(), &mut simple);
        assert_eq!(simple["input"], "mail [EMAIL_REDACTED]");
    }

    #[test]
    fn responses_request_masks_prompt_variables_without_touching_files() {
        let chain = both();
        let mut body = json!({
            "model": "m",
            "input": "clean",
            "prompt": {
                "id": "pmpt_a@x.com",
                "version": "v1",
                "variables": {
                    "recipient": "b@y.org",
                    "context": {"type": "input_text", "text": "mail c@z.io"},
                    "attachment": {"type": "input_file", "file_data": "a@x.com"}
                }
            }
        });
        let inspection = responses_prompt_inspection_text_capped(&body["prompt"], usize::MAX).0;
        assert!(inspection.contains("b@y.org"));
        assert!(!inspection.contains("file_data"));
        let redaction = redact_responses_request_structured(chain.as_ref(), &mut body);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(body["prompt"]["variables"]["recipient"], "[EMAIL_REDACTED]");
        assert_eq!(
            body["prompt"]["variables"]["context"]["text"],
            "mail [EMAIL_REDACTED]"
        );
        assert_eq!(
            body["prompt"]["variables"]["attachment"]["file_data"],
            "a@x.com"
        );
    }

    #[test]
    fn responses_walks_official_tool_result_text_fields() {
        let chain = both();
        let mut response = json!({"output": [
            {"type": "file_search_call", "id": "fs_1", "queries": ["mail a@x.com"],
             "results": [{"file_id": "f_1", "filename": "notes", "text": "b@y.org"}]},
            {"type": "code_interpreter_call", "id": "ci_1", "code": "print('c@z.io')",
             "outputs": [{"type": "logs", "logs": "d@w.dev"}, {"type": "image", "url": "https://e@v.net"}]},
            {"type": "shell_call_output", "call_id": "sh_1", "output": [
                {"stdout": "f@u.net", "stderr": "g@t.net", "outcome": {"type": "exit", "exit_code": 0}}
             ]},
            {"type": "mcp_list_tools", "id": "mcp_1", "error": "h@s.net", "tools": [{
                "name": "lookup", "description": "i@r.net",
                "input_schema": {"type": "object", "properties": {"owner": {"default": "j@q.net"}}}
             }]},
            {"type": "mcp_call", "id": "mc_1", "name": "lookup", "error": {"type": "http_error", "message": "k@p.net"}}
        ]});
        let redaction = redact_responses_response_structured(chain.as_ref(), &mut response);
        assert!(!redaction.unrewritable_tool_key);
        let rendered = response.to_string();
        for original in [
            "a@x.com", "b@y.org", "c@z.io", "d@w.dev", "f@u.net", "g@t.net", "h@s.net", "i@r.net",
            "j@q.net", "k@p.net",
        ] {
            assert!(
                !rendered.contains(original),
                "unmasked official field: {original}"
            );
        }
        assert!(
            rendered.contains("https://e@v.net"),
            "opaque generated image URL must not be rewritten"
        );
        let inspection =
            responses_item_inspection_text_capped(&response["output"][0], usize::MAX).0;
        assert!(inspection.contains("[EMAIL_REDACTED]"));
    }

    #[test]
    fn structured_requests_cover_tool_definition_values_and_keys() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let tools = json!([{
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "contact b@y.org",
                "parameters": {
                    "type": "object",
                    "properties": {
                        (sensitive_key): {"type": "string", "default": "c@z.io"}
                    }
                }
            }
        }]);

        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "clean"}],
            "tools": tools.clone()
        }))
        .unwrap();
        let inspection = chat_request_for_inspection(&chat);
        assert!(inspection
            .messages
            .last()
            .unwrap()
            .content_str()
            .contains(sensitive_key));
        let chat_redaction = redact_chat_format_structured(chain.as_ref(), &mut chat);
        assert!(chat_redaction.unrewritable_tool_key);
        assert_eq!(
            chat.extra["tools"][0]["function"]["description"],
            "contact [EMAIL_REDACTED]"
        );
        assert_eq!(
            chat.extra["tools"][0]["function"]["parameters"]["properties"][sensitive_key]
                ["default"],
            "[EMAIL_REDACTED]"
        );

        let mut responses = json!({
            "model": "m",
            "input": "clean",
            "tools": tools
        });
        let responses_redaction =
            redact_responses_request_structured(chain.as_ref(), &mut responses);
        assert!(responses_redaction.unrewritable_tool_key);
        assert_eq!(
            responses["tools"][0]["function"]["description"],
            "contact [EMAIL_REDACTED]"
        );
        assert_eq!(
            responses["tools"][0]["function"]["parameters"]["properties"][sensitive_key]["default"],
            "[EMAIL_REDACTED]"
        );
    }

    #[test]
    fn structured_output_schemas_are_inspected_without_renaming_properties() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let schema = json!({
            "type": "object",
            "properties": {
                (sensitive_key): {
                    "type": "string",
                    "description": "contact b@y.org",
                    "default": "c@z.io"
                }
            }
        });

        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "clean"}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "result", "schema": schema.clone()}
            }
        }))
        .unwrap();
        let chat_redaction = redact_chat_format_structured(chain.as_ref(), &mut chat);
        assert!(chat_redaction.unrewritable_tool_key);
        assert_eq!(
            chat.extra["response_format"]["json_schema"]["schema"]["properties"][sensitive_key]
                ["description"],
            "contact [EMAIL_REDACTED]"
        );

        let mut responses = json!({
            "model": "m",
            "input": "clean",
            "text": {"format": {"type": "json_schema", "name": "result", "schema": schema.clone()}}
        });
        let responses_redaction =
            redact_responses_request_structured(chain.as_ref(), &mut responses);
        assert!(responses_redaction.unrewritable_tool_key);
        assert_eq!(
            responses["text"]["format"]["schema"]["properties"][sensitive_key]["default"],
            "[EMAIL_REDACTED]"
        );

        let mut anthropic = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "clean"}],
            "output_config": {"format": {"type": "json_schema", "schema": schema}}
        });
        let anthropic_redaction = redact_anthropic_request(chain.as_ref(), &mut anthropic);
        assert!(anthropic_redaction.unrewritable_tool_key);
        assert_eq!(
            anthropic["output_config"]["format"]["schema"]["properties"][sensitive_key]
                ["description"],
            "contact [EMAIL_REDACTED]"
        );
    }

    #[test]
    fn structured_requests_flag_historical_tool_argument_keys() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "arguments": format!(r#"{{"{sensitive_key}":"safe"}}"#)
                    }
                }]
            }]
        }))
        .unwrap();
        let chat_redaction = redact_chat_format_structured(chain.as_ref(), &mut chat);
        assert!(chat_redaction.unrewritable_tool_key);

        let mut responses = json!({
            "model": "m",
            "input": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": format!(r#"{{"{sensitive_key}":"safe"}}"#)
            }]
        });
        let responses_redaction =
            redact_responses_request_structured(chain.as_ref(), &mut responses);
        assert!(responses_redaction.unrewritable_tool_key);
        assert!(responses["input"][0]["arguments"]
            .as_str()
            .unwrap()
            .contains(sensitive_key));
    }

    fn content_chunk(text: &str) -> ChatChunk {
        ChatChunk {
            id: "c".into(),
            model: "m".into(),
            delta: ChatDelta {
                content: Some(text.to_string()),
                ..ChatDelta::default()
            },
            finish_reason: None,
            usage: None,
        }
    }

    #[test]
    fn stream_chunks_mask_span_split_across_chunk_boundary() {
        let chain = both();
        // "a@x.com" split across three chunks — per-chunk masking would miss it.
        let mut chunks = vec![
            content_chunk("mail a@"),
            content_chunk("x.c"),
            content_chunk("om now"),
        ];
        let counts = redact_chat_chunks(chain.as_ref(), &mut chunks);
        assert_eq!(counts.get("email"), Some(&1));
        let reassembled: String = chunks
            .iter()
            .map(|c| c.delta.content.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(reassembled, "mail [EMAIL_REDACTED] now");
        // Full text lands on the first content chunk; the rest are empty.
        assert_eq!(
            chunks[0].delta.content.as_deref(),
            Some("mail [EMAIL_REDACTED] now")
        );
        assert_eq!(chunks[1].delta.content.as_deref(), Some(""));
    }

    #[test]
    fn stream_chunks_mask_tool_call_arguments_channel() {
        let chain = both();
        let mut chunks = vec![
            ChatChunk {
                id: "c".into(),
                model: "m".into(),
                delta: ChatDelta {
                    tool_calls: Some(vec![json!({
                        "index": 0, "id": "call_1", "type": "function",
                        "function": {"name": "send", "arguments": "{\"to\":\"a@"}
                    })]),
                    ..ChatDelta::default()
                },
                finish_reason: None,
                usage: None,
            },
            ChatChunk {
                id: "c".into(),
                model: "m".into(),
                delta: ChatDelta {
                    tool_calls: Some(vec![json!({
                        "index": 0,
                        "function": {"arguments": "x.com\"}"}
                    })]),
                    ..ChatDelta::default()
                },
                finish_reason: None,
                usage: None,
            },
        ];
        let counts = redact_chat_chunks(chain.as_ref(), &mut chunks);
        assert_eq!(counts.get("email"), Some(&1));
        let first_args = chunks[0].delta.tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .to_string();
        let second_args = chunks[1].delta.tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(first_args, "{\"to\":\"[EMAIL_REDACTED]\"}");
        assert_eq!(second_args, "");
    }

    #[test]
    fn stream_chunks_mask_legacy_function_call_arguments_channel() {
        let chain = both();
        let mut chunks = vec![
            ChatChunk {
                id: "c".into(),
                model: "m".into(),
                delta: ChatDelta {
                    function_call: Some(json!({
                        "name": "legacy_send",
                        "arguments": "{\"to\":\"a@"
                    })),
                    ..ChatDelta::default()
                },
                finish_reason: None,
                usage: None,
            },
            ChatChunk {
                id: "c".into(),
                model: "m".into(),
                delta: ChatDelta {
                    function_call: Some(json!({"arguments": "x.com\"}"})),
                    ..ChatDelta::default()
                },
                finish_reason: None,
                usage: None,
            },
        ];
        let redaction = redact_chat_chunks_structured(chain.as_ref(), &mut chunks);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(redaction.counts.get("email"), Some(&1));
        assert_eq!(
            chunks[0].delta.function_call.as_ref().unwrap()["arguments"],
            "{\"to\":\"[EMAIL_REDACTED]\"}"
        );
        assert_eq!(
            chunks[1].delta.function_call.as_ref().unwrap()["arguments"],
            ""
        );
    }

    #[test]
    fn stream_chunks_mask_decoded_modern_and_legacy_arguments() {
        let chain = both();
        let mut chunks = vec![ChatChunk {
            id: "c".into(),
            model: "m".into(),
            delta: ChatDelta {
                tool_calls: Some(vec![json!({
                    "index": 0,
                    "function": {"name": "send", "arguments": {"to": "a@x.com"}}
                })]),
                function_call: Some(json!({
                    "name": "legacy_send",
                    "arguments": {"to": "b@y.org"}
                })),
                ..ChatDelta::default()
            },
            finish_reason: None,
            usage: None,
        }];
        let redaction = redact_chat_chunks_structured(chain.as_ref(), &mut chunks);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(redaction.counts.get("email"), Some(&2));
        assert_eq!(
            chunks[0].delta.tool_calls.as_ref().unwrap()[0]["function"]["arguments"]["to"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(
            chunks[0].delta.function_call.as_ref().unwrap()["arguments"]["to"],
            "[EMAIL_REDACTED]"
        );
    }

    #[test]
    fn structured_stream_chunks_flag_split_tool_argument_keys() {
        let chain = both();
        let mut chunks = vec![
            ChatChunk {
                id: "c".into(),
                model: "m".into(),
                delta: ChatDelta {
                    tool_calls: Some(vec![json!({
                        "index": 0, "id": "call_1", "type": "function",
                        "function": {"name": "send", "arguments": "{\"owner-a@"}
                    })]),
                    ..ChatDelta::default()
                },
                finish_reason: None,
                usage: None,
            },
            ChatChunk {
                id: "c".into(),
                model: "m".into(),
                delta: ChatDelta {
                    tool_calls: Some(vec![json!({
                        "index": 0,
                        "function": {"arguments": "x.com\":\"b@y.org\"}"}
                    })]),
                    ..ChatDelta::default()
                },
                finish_reason: None,
                usage: None,
            },
        ];

        let redaction = redact_chat_chunks_structured(chain.as_ref(), &mut chunks);
        let args = chunks[0].delta.tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(parsed["owner-a@x.com"], "[EMAIL_REDACTED]");
        assert_eq!(redaction.counts.get("email"), Some(&2));
    }

    #[test]
    fn stream_chunks_untouched_when_nothing_matches() {
        let chain = both();
        let mut chunks = vec![content_chunk("hello "), content_chunk("world")];
        assert!(redact_chat_chunks(chain.as_ref(), &mut chunks).is_empty());
        assert_eq!(chunks[0].delta.content.as_deref(), Some("hello "));
        assert_eq!(chunks[1].delta.content.as_deref(), Some("world"));
    }

    #[test]
    fn completions_sse_masks_crlf_frames_and_preserves_separator() {
        let chain = both();
        let raw = concat!(
            "event: completion\r\ndata: {\"choices\":[{\"index\":0,\"text\":\"mail a@\"}]}\r\n\r\n",
            "event: completion\r\ndata: {\"choices\":[{\"index\":0,\"text\":\"x.com\"}]}\r\n\r\n",
        );
        let (out, counts) = redact_completions_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("mail [EMAIL_REDACTED]"), "out: {out}");
        assert!(!out.contains("a@x.com"), "out: {out}");
        assert!(out.contains("\r\n\r\n"), "CRLF separator changed: {out:?}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn completions_sse_masks_final_unterminated_frame() {
        let chain = both();
        let raw = "data: {\"choices\":[{\"index\":0,\"text\":\"mail a@x.com\"}]}";
        let (out, counts) = redact_completions_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "data: {\"choices\":[{\"index\":0,\"text\":\"mail [EMAIL_REDACTED]\"}]}"
        );
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn completions_sse_masks_bom_cr_mixed_and_multi_data_event() {
        let chain = both();
        let raw = concat!(
            "\u{feff}event: completion\r",
            "data: {\"choices\":\r\n",
            "data: [{\"index\":0,\"text\":\"mail a@x.com\"}]}\n\r",
        );
        let (events, malformed) = parse_sse_json_stream(raw.as_bytes());
        assert!(!malformed);
        assert_eq!(events[0]["choices"][0]["text"], "mail a@x.com");

        let (out, counts) = redact_completions_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with('\u{feff}'));
        assert!(out.contains("mail [EMAIL_REDACTED]"), "out: {out:?}");
        assert!(!out.contains("a@x.com"), "out: {out:?}");
        assert_eq!(out.matches("data:").count(), 1, "out: {out:?}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn sse_parser_flags_non_json_data_instead_of_treating_it_as_empty() {
        let (events, malformed) = parse_sse_json_stream(b"data: not-json\r\r");
        assert!(events.is_empty());
        assert!(malformed);
    }

    #[test]
    fn anthropic_sse_masks_text_delta_across_frames() {
        let chain = both();
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"mail a@\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x.com ok\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let redaction = redact_anthropic_sse(chain.as_ref(), raw.as_bytes());
        let out = redaction.rewritten.unwrap();
        let counts = redaction.counts;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("mail [EMAIL_REDACTED] ok"), "out: {out}");
        assert!(!out.contains("a@x.com"));
        // Second delta emptied; frame structure + unrelated frames intact.
        assert!(
            out.contains("{\"type\":\"text_delta\",\"text\":\"\"}")
                || out.contains("\"text\":\"\"")
        );
        assert!(out.contains("message_start"));
        assert!(out.contains("message_stop"));
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn anthropic_sse_masks_crlf_and_unterminated_final_frame() {
        let chain = both();
        let raw = concat!(
            "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"mail a@\"}}\r\n\r\n",
            "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x.com\"}}",
        );
        let redaction = redact_anthropic_sse(chain.as_ref(), raw.as_bytes());
        let out = redaction.rewritten.unwrap();
        let counts = redaction.counts;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("mail [EMAIL_REDACTED]"), "out: {out}");
        assert!(!out.contains("a@x.com"), "out: {out}");
        assert!(out.contains("\r\n\r\n"), "CRLF separator changed: {out:?}");
        assert!(!out.ends_with("\r\n\r\n"), "EOF terminator was invented");
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn anthropic_sse_masks_lone_cr_multi_data_event() {
        let chain = both();
        let raw = concat!(
            "event: content_block_delta\r",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\r",
            "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"a@x.com\"}}\r\r",
        );
        let redaction = redact_anthropic_sse(chain.as_ref(), raw.as_bytes());
        let out = redaction.rewritten.unwrap();
        let counts = redaction.counts;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out:?}");
        assert!(!out.contains("a@x.com"), "out: {out:?}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn anthropic_sse_masks_tool_use_input_json_channel() {
        let chain = both();
        let raw = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"send\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"to\\\":\\\"a@\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"x.com\\\"}\"}}\n\n",
        );
        let redaction = redact_anthropic_sse(chain.as_ref(), raw.as_bytes());
        let out = redaction.rewritten.unwrap();
        let counts = redaction.counts;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        assert!(!out.contains("a@"), "no split original fragments: {out}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn anthropic_sse_walks_nonempty_tool_use_start_input() {
        let chain = both();
        let raw = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"send\",\"input\":{\"owner-a@x.com\":\"safe\",\"to\":\"b@y.org\"}}}\n\n",
        );

        let redaction = redact_anthropic_sse(chain.as_ref(), raw.as_bytes());
        let out = String::from_utf8(redaction.rewritten.unwrap()).unwrap();
        assert!(redaction.unrewritable_tool_key);
        assert!(
            out.contains("owner-a@x.com"),
            "structural key changed: {out}"
        );
        assert!(
            out.contains("[EMAIL_REDACTED]"),
            "value was not masked: {out}"
        );
        assert!(!out.contains("b@y.org"), "raw value leaked: {out}");
        assert_eq!(redaction.counts.get("email"), Some(&2));
    }

    #[test]
    fn anthropic_sse_flags_sensitive_tool_input_key_split_across_frames() {
        let chain = both();
        let raw = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"send\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"owner-a@\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"x.com\\\":\\\"safe\\\"}\"}}\n\n",
        );

        let redaction = redact_anthropic_sse(chain.as_ref(), raw.as_bytes());
        assert!(redaction.unrewritable_tool_key);
        assert!(redaction.rewritten.is_none());
        assert_eq!(redaction.counts.get("email"), Some(&1));
    }

    #[test]
    fn responses_sse_masks_delta_channel_and_aggregate_events() {
        let chain = both();
        let raw = concat!(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"mail a@\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"x.com ok\"}\n\n",
            "event: response.output_text.done\ndata: {\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",\"text\":\"mail a@x.com ok\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"mail a@x.com ok\"}]}]}}\n\n",
        );
        let (out, counts) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "original must be gone: {out}");
        // Delta channel: full masked text on the first delta; done +
        // completed events masked consistently.
        assert!(
            out.contains("\"delta\":\"mail [EMAIL_REDACTED] ok\""),
            "out: {out}"
        );
        assert!(
            out.contains("\"text\":\"mail [EMAIL_REDACTED] ok\""),
            "out: {out}"
        );
        // Aggregate events don't double-count the same span.
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn responses_sse_masks_bom_and_lone_cr_event() {
        let chain = both();
        let raw = concat!(
            "\u{feff}event: response.output_text.delta\r",
            "data: {\"type\":\"response.output_text.delta\",\r",
            "data: \"item_id\":\"m\",\"delta\":\"a@x.com\"}\r\r",
        );
        let (out, counts) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with('\u{feff}'));
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out:?}");
        assert!(!out.contains("a@x.com"), "out: {out:?}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn responses_sse_masks_function_call_args_channel() {
        let chain = both();
        let raw = concat!(
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"to\\\":\\\"a@\"}\n\n",
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"x.com\\\"}\"}\n\n",
            "event: response.function_call_arguments.done\ndata: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{\\\"to\\\":\\\"a@x.com\\\"}\"}\n\n",
        );
        let (out, counts) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@"), "original fragments gone: {out}");
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn structured_responses_outputs_flag_function_argument_keys() {
        let chain = both();
        let sensitive_key = "owner-a@x.com";
        let mut response = json!({
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": format!(r#"{{"{sensitive_key}":"safe"}}"#)
            }]
        });
        let response_redaction =
            redact_responses_response_structured(chain.as_ref(), &mut response);
        assert!(response_redaction.unrewritable_tool_key);

        let raw = concat!(
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"owner-a@\"}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"x.com\\\":\\\"safe\\\"}\"}\n\n",
        );
        let stream_redaction = redact_responses_sse_structured(chain.as_ref(), raw.as_bytes());
        assert!(stream_redaction.unrewritable_tool_key);
        assert!(String::from_utf8(stream_redaction.rewritten.unwrap())
            .unwrap()
            .contains(sensitive_key));
    }

    #[test]
    fn responses_sse_returns_none_when_clean() {
        let chain = both();
        let raw = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"hello\"}\n\n";
        assert!(redact_responses_sse(chain.as_ref(), raw.as_bytes()).is_none());
    }

    #[test]
    fn responses_sse_masks_split_refusal_and_done_forms() {
        let chain = both();
        let raw = concat!(
            "event: response.refusal.delta\n",
            "data: {\"type\":\"response.refusal.delta\",\"item_id\":\"m\",\"content_index\":0,\"delta\":\"mail a@\"}\n\n",
            "event: response.refusal.delta\n",
            "data: {\"type\":\"response.refusal.delta\",\"item_id\":\"m\",\"content_index\":0,\"delta\":\"x.com\"}\n\n",
            "event: response.refusal.done\n",
            "data: {\"type\":\"response.refusal.done\",\"item_id\":\"m\",\"content_index\":0,\"refusal\":\"mail a@x.com\"}\n\n",
        );
        let (rewritten, counts) =
            redact_responses_sse(chain.as_ref(), raw.as_bytes()).expect("must rewrite");
        let text = String::from_utf8(rewritten).unwrap();
        assert!(!text.contains("a@x.com"));
        assert!(!text.contains("mail a@"));
        assert!(text.contains("[EMAIL_REDACTED]"));
        assert_eq!(counts.get("email"), Some(&1));
    }

    #[test]
    fn responses_response_json_masks_output_items() {
        let chain = both();
        let mut body = json!({
            "id": "resp_1",
            "output": [
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "mail a@x.com"},
                    {"type": "refusal", "refusal": "cannot help c@z.io"}
                ]},
                {"type": "function_call", "call_id": "c", "name": "send",
                 "arguments": "{\"to\":\"b@y.org\"}"}
            ]
        });
        let counts = redact_responses_response(chain.as_ref(), &mut body);
        assert_eq!(
            body["output"][0]["content"][0]["text"],
            "mail [EMAIL_REDACTED]"
        );
        assert_eq!(
            body["output"][0]["content"][1]["refusal"],
            "cannot help [EMAIL_REDACTED]"
        );
        assert_eq!(
            body["output"][1]["arguments"],
            "{\"to\":\"[EMAIL_REDACTED]\"}"
        );
        assert_eq!(counts.get("email"), Some(&3));
    }

    #[test]
    fn anthropic_sse_returns_none_when_clean() {
        let chain = both();
        let raw = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n";
        let redaction = redact_anthropic_sse(chain.as_ref(), raw.as_bytes());
        assert!(redaction.rewritten.is_none());
        assert!(!redaction.unrewritable_tool_key);
    }

    #[test]
    fn malformed_tool_args_fall_back_to_raw_text_masking() {
        let chain = both();
        let mut encoded = String::from("not json but has a@x.com inside");
        let mut counts = RedactionCounts::new();
        redact_json_encoded(chain.as_ref(), Direction::Output, &mut encoded, &mut counts);
        assert_eq!(encoded, "not json but has [EMAIL_REDACTED] inside");
        assert_eq!(counts.get("email"), Some(&1));
    }

    /// #696: rerank request masking covers `query` + both document shapes.
    #[test]
    fn rerank_request_masks_query_and_documents() {
        let chain = both();
        let mut body = json!({
            "model": "m",
            "query": "who is a@x.com",
            "documents": [
                "contact b@y.org",
                {"text": "reach c@z.io", "metadata": {"owner": "d@w.dev"}},
                42
            ]
        });
        let counts = redact_rerank_request(chain.as_ref(), &mut body);
        assert_eq!(body["query"], "who is [EMAIL_REDACTED]");
        assert_eq!(body["documents"][0], "contact [EMAIL_REDACTED]");
        assert_eq!(body["documents"][1]["text"], "reach [EMAIL_REDACTED]");
        assert_eq!(
            body["documents"][1]["metadata"]["owner"],
            "[EMAIL_REDACTED]"
        );
        assert_eq!(counts.get("email"), Some(&4));
    }

    /// #696: images request masking covers `prompt`.
    #[test]
    fn images_request_masks_prompt() {
        let chain = both();
        let mut body = json!({"model": "m", "prompt": "portrait of a@x.com"});
        let counts = redact_images_request(chain.as_ref(), &mut body);
        assert_eq!(body["prompt"], "portrait of [EMAIL_REDACTED]");
        assert_eq!(counts.get("email"), Some(&1));
    }

    /// #696: speech (TTS) request masking covers `input`.
    #[test]
    fn speech_request_masks_input() {
        let chain = both();
        let mut body = json!({
            "model": "m",
            "input": "read a@x.com aloud",
            "instructions": "whisper to b@y.org",
            "voice": "alloy"
        });
        let counts = redact_speech_request(chain.as_ref(), &mut body);
        assert_eq!(body["input"], "read [EMAIL_REDACTED] aloud");
        assert_eq!(body["instructions"], "whisper to [EMAIL_REDACTED]");
        assert_eq!(counts.get("email"), Some(&2));
    }

    #[test]
    fn images_response_masks_revised_prompt() {
        let chain = both();
        let mut body = json!({
            "data": [{"url": "https://example.test/image.png", "revised_prompt": "mail a@x.com"}]
        });
        let redaction = redact_images_response_structured(chain.as_ref(), &mut body);
        assert_eq!(body["data"][0]["revised_prompt"], "mail [EMAIL_REDACTED]");
        assert_eq!(redaction.counts.get("email"), Some(&1));
    }

    /// #696: transcription response masking rewrites the JSON `text` +
    /// `segments[].text` (verbose_json) and reports counts.
    #[test]
    fn transcription_response_masks_json_text_and_segments() {
        let chain = both();
        let body = json!({
            "text": "mail a@x.com",
            "segments": [{"id": 0, "text": "mail a@x.com"}]
        });
        let (rewritten, counts) =
            redact_transcription_response(chain.as_ref(), &serde_json::to_vec(&body).unwrap())
                .expect("must rewrite");
        let v: Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["text"], "mail [EMAIL_REDACTED]");
        assert_eq!(v["segments"][0]["text"], "mail [EMAIL_REDACTED]");
        assert_eq!(counts.get("email"), Some(&2));
    }

    /// #696: the raw-text response formats (`text` / `srt` / `vtt`) are
    /// masked as plain text; a clean body returns None (kept as-is).
    #[test]
    fn transcription_response_masks_raw_text_formats() {
        let chain = both();
        let (rewritten, counts) =
            redact_transcription_response(chain.as_ref(), b"speaker: a@x.com\n").expect("rewrite");
        assert_eq!(rewritten, b"speaker: [EMAIL_REDACTED]\n");
        assert_eq!(counts.get("email"), Some(&1));
        assert!(redact_transcription_response(chain.as_ref(), b"all clean\n").is_none());
    }

    // ── remote segment moderation (#932 bedrock follow-up) ──────────────

    use aisix_guardrails::{GuardrailVerdict, SegmentsOutcome};

    /// Stub of a Bedrock-style segment moderator: masks slot i to
    /// `"<M{i}:UPPER(text)>"` — index-stamped so a positional mix-up is
    /// unmissable — and reports a fixed entity count. `verdict` lets the
    /// block/bypass paths be exercised; `panic_if_called` pins the
    /// skip-when-already-blocked contract.
    struct StubSegments {
        verdict: GuardrailVerdict,
        mask: bool,
        panic_if_called: bool,
    }

    impl StubSegments {
        fn masker() -> Self {
            Self {
                verdict: GuardrailVerdict::Allow,
                mask: true,
                panic_if_called: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl Guardrail for StubSegments {
        fn name(&self) -> &'static str {
            "stub-segments"
        }
        fn moderates_segments(&self) -> bool {
            true
        }
        async fn moderate_input_segments(&self, texts: &[String]) -> SegmentsOutcome {
            self.moderate(texts)
        }
        async fn moderate_output_segments(&self, texts: &[String]) -> SegmentsOutcome {
            self.moderate(texts)
        }
    }

    impl StubSegments {
        fn moderate(&self, texts: &[String]) -> SegmentsOutcome {
            if self.panic_if_called {
                panic!("segment moderator must not be called on this path");
            }
            let mut counts = RedactionCounts::new();
            counts.insert("EMAIL".to_owned(), 1);
            SegmentsOutcome {
                verdict: self.verdict.clone(),
                masked: self.mask.then(|| {
                    texts
                        .iter()
                        .enumerate()
                        .map(|(i, t)| format!("<M{i}:{}>", t.to_uppercase()))
                        .collect()
                }),
                counts,
                monitor_hits: Vec::new(),
            }
        }
    }

    fn seg_chain(stub: StubSegments) -> GuardrailChain {
        GuardrailChain::new(vec![Arc::new(stub)])
    }

    /// The collect→call→apply round trip over the chat walker: every slot
    /// kind (flat content, text block, tool-call JSON argument) gets its
    /// OWN positionally-matched mask, and the provider counts — not the
    /// applier's plumbing marker — land in `counts_out`.
    #[tokio::test]
    async fn moderate_body_masks_chat_slots_positionally() {
        let chain = seg_chain(StubSegments::masker());
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "first slot"},
                {"role": "user", "content": "", "content_blocks": [
                    {"type": "text", "text": "second slot"}
                ]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"index": 0, "function": {"name": "send", "arguments": "{\"to\":\"third slot\"}"}}
                ]}
            ]
        }))
        .unwrap();
        let mut counts = RedactionCounts::new();
        let verdict = moderate_body(
            &chain,
            Direction::Input,
            GuardrailVerdict::Allow,
            &mut counts,
            &mut Vec::new(),
            |g| redact_chat_format(g, &mut req),
        )
        .await
        .verdict;
        assert_eq!(verdict, GuardrailVerdict::Allow);
        assert_eq!(
            req.messages[0].content.as_deref(),
            Some("<M0:FIRST SLOT>"),
            "flat content = slot 0",
        );
        assert_eq!(
            req.messages[1].content_blocks.as_ref().unwrap()[0]["text"],
            "<M1:SECOND SLOT>",
            "text block = slot 1",
        );
        assert_eq!(
            req.messages[2].extra["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
            "{\"to\":\"<M2:THIRD SLOT>\"}",
            "tool-arg inner string = slot 2 (marker counts must fire the \
             json-encoded rewrite gate)",
        );
        assert_eq!(counts.get("EMAIL"), Some(&1), "provider counts merged");
        assert!(
            !counts.keys().any(|k| k.starts_with("__")),
            "the applier's plumbing marker must never leak into telemetry counts",
        );
    }

    #[tokio::test]
    async fn segment_mask_of_anthropic_historical_tool_input_key_fails_closed() {
        let chain = seg_chain(StubSegments::masker());
        let mut body = json!({
            "model": "claude",
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "lookup",
                    "input": {"sensitive-key": "safe"}
                }]
            }]
        });
        let mut counts = RedactionCounts::new();
        let verdict = moderate_anthropic_request(
            &chain,
            GuardrailVerdict::Allow,
            &mut body,
            &mut counts,
            &mut Vec::new(),
        )
        .await;

        assert!(verdict.verdict.is_block());
        assert!(!verdict.capture_safe);
        assert!(body["messages"][0]["content"][0]["input"]
            .get("sensitive-key")
            .is_some());
    }

    #[tokio::test]
    async fn segment_mask_of_anthropic_response_tool_key_fails_closed() {
        let chain = seg_chain(StubSegments::masker());
        let mut body = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "lookup",
                "input": {"sensitive-key": "safe"}
            }]
        });
        let mut counts = RedactionCounts::new();
        let moderation = moderate_anthropic_response(
            &chain,
            GuardrailVerdict::Allow,
            &mut body,
            &mut counts,
            &mut Vec::new(),
        )
        .await;

        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
        assert!(body["content"][0]["input"].get("sensitive-key").is_some());
    }

    #[tokio::test]
    async fn segment_masks_of_structured_chat_tool_keys_fail_closed() {
        let mut msg = ChatMessage::assistant("");
        msg.extra.insert(
            "tool_calls".into(),
            json!([{
                "id": "call_1", "type": "function",
                "function": {
                    "name": "lookup",
                    "arguments": "{\"sensitive-key\":\"safe\"}"
                }
            }]),
        );
        let mut resp = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: msg,
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };
        let mut counts = RedactionCounts::new();
        let moderation = moderate_chat_response_structured(
            &seg_chain(StubSegments::masker()),
            GuardrailVerdict::Allow,
            &mut resp,
            &mut counts,
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);

        let mut chunks = vec![ChatChunk {
            id: "c".into(),
            model: "m".into(),
            delta: ChatDelta {
                tool_calls: Some(vec![json!({
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {
                        "name": "lookup",
                        "arguments": "{\"sensitive-key\":\"safe\"}"
                    }
                })]),
                ..ChatDelta::default()
            },
            finish_reason: None,
            usage: None,
        }];
        let moderation = moderate_chat_chunks_structured(
            &seg_chain(StubSegments::masker()),
            GuardrailVerdict::Allow,
            &mut chunks,
            &mut counts,
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
    }

    #[tokio::test]
    async fn segment_masks_of_responses_tool_keys_fail_closed() {
        let mut request = json!({
            "model": "m",
            "input": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{\"sensitive-key\":\"safe\"}"
            }]
        });
        let mut counts = RedactionCounts::new();
        let moderation = moderate_responses_request_structured(
            &seg_chain(StubSegments::masker()),
            GuardrailVerdict::Allow,
            &mut request,
            &mut counts,
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);

        let mut response = json!({
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{\"sensitive-key\":\"safe\"}"
            }]
        });
        let moderation = moderate_responses_response_structured(
            &seg_chain(StubSegments::masker()),
            GuardrailVerdict::Allow,
            &mut response,
            &mut counts,
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);

        let mut stream = b"event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"sensitive-key\\\":\\\"safe\\\"}\"}\n\n".to_vec();
        let moderation = moderate_responses_sse(
            &seg_chain(StubSegments::masker()),
            GuardrailVerdict::Allow,
            &mut stream,
            &mut counts,
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
    }

    #[test]
    fn responses_program_payloads_are_masked_and_identifiers_fail_closed() {
        let chain = both();
        let mut payloads = json!({
            "input": [
                {"type":"program","code":"text('a@x.com')"},
                {"type":"program_output","result":"{\"owner\":\"b@y.org\"}"},
                {"type":"computer_call","action":{"description":"contact c@z.io"}}
            ]
        });
        let redaction = redact_responses_request_structured(chain.as_ref(), &mut payloads);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(payloads["input"][0]["code"], "text('[EMAIL_REDACTED]')");
        assert_eq!(
            payloads["input"][1]["result"],
            "{\"owner\":\"[EMAIL_REDACTED]\"}"
        );
        assert_eq!(
            payloads["input"][2]["action"]["description"],
            "contact [EMAIL_REDACTED]"
        );

        let mut response = json!({
            "output": [
                {"type":"program","code":"text('a@x.com')"},
                {"type":"program_output","result":"{\"owner\":\"b@y.org\"}"},
                {"type":"computer_call","action":{"description":"contact c@z.io"}}
            ]
        });
        let redaction = redact_responses_response_structured(chain.as_ref(), &mut response);
        assert!(!redaction.unrewritable_tool_key);
        assert!(!response.to_string().contains("@x.com"));
        assert!(!response.to_string().contains("@y.org"));
        assert!(!response.to_string().contains("@z.io"));

        for item in [
            json!({"type":"program","fingerprint":"a@x.com"}),
            json!({"type":"function_call","caller":{"type":"program","caller_id":"a@x.com"}}),
        ] {
            let mut body = json!({"input":[item.clone()]});
            let redaction = redact_responses_request_structured(chain.as_ref(), &mut body);
            assert!(redaction.unrewritable_tool_key);
            assert!(body.to_string().contains("a@x.com"));

            let mut response = json!({"output":[item]});
            let redaction = redact_responses_response_structured(chain.as_ref(), &mut response);
            assert!(redaction.unrewritable_tool_key);
            assert!(response.to_string().contains("a@x.com"));
        }
    }

    #[tokio::test]
    async fn segment_masks_of_tool_identifiers_fail_closed_without_rewriting_them() {
        let chain = seg_chain(StubSegments::masker());

        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "clean"}],
            "tools": [{
                "type": "function",
                "function": {"name": "lookup", "description": "clean"}
            }]
        }))
        .unwrap();
        let moderation = moderate_chat_format_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut chat,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
        assert_eq!(chat.extra["tools"][0]["function"]["name"], "lookup");

        let mut anthropic = json!({
            "model": "claude",
            "metadata": {"user_id": "stable-user"},
            "messages": [{"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}
            }]}]
        });
        let moderation = moderate_anthropic_request(
            &chain,
            GuardrailVerdict::Allow,
            &mut anthropic,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert_eq!(anthropic["messages"][0]["content"][0]["name"], "lookup");
        assert_eq!(anthropic["metadata"]["user_id"], "stable-user");

        let mut responses = json!({
            "model": "m",
            "safety_identifier": "stable-user",
            "input": [{
                "type": "function_call", "call_id": "call_1",
                "name": "lookup", "arguments": "{}"
            }]
        });
        let moderation = moderate_responses_request_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut responses,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert_eq!(responses["input"][0]["name"], "lookup");
        assert_eq!(responses["safety_identifier"], "stable-user");

        let mut completions = json!({
            "model": "m",
            "prompt": "clean",
            "user": "alice@example.com"
        });
        let moderation = moderate_completions_request_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut completions,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
        assert_eq!(completions["user"], "alice@example.com");

        let mut images = json!({"model": "m", "prompt": "clean", "user": "stable-user"});
        let moderation = moderate_images_request_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut images,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
        assert_eq!(images["user"], "stable-user");
    }

    #[test]
    fn structural_request_metadata_is_detected_without_rewriting() {
        let chain = both();

        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "clean"}],
            "user": "chat@example.com",
            "metadata": {"tenant": "meta@example.com"},
            "prompt_cache_key": "cache@example.com"
        }))
        .unwrap();
        let redaction = redact_chat_format_structured(chain.as_ref(), &mut chat);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(chat.extra["user"], "chat@example.com");

        let mut messages = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "clean"}],
            "metadata": {"user_id": "messages@example.com"}
        });
        let redaction = redact_anthropic_request(chain.as_ref(), &mut messages);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(messages["metadata"]["user_id"], "messages@example.com");

        let mut responses = json!({
            "model": "m",
            "input": "clean",
            "safety_identifier": "safety@example.com",
            "metadata": {"tenant": "responses@example.com"}
        });
        let redaction = redact_responses_request_structured(chain.as_ref(), &mut responses);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(responses["safety_identifier"], "safety@example.com");

        let mut images = json!({"model": "m", "prompt": "clean", "user": "images@example.com"});
        let redaction = redact_images_request_structured(chain.as_ref(), &mut images);
        assert!(redaction.unrewritable_tool_key);
        assert_eq!(images["user"], "images@example.com");
    }

    #[tokio::test]
    async fn segment_masks_raw_json_auxiliary_text_and_image_output() {
        let chain = seg_chain(StubSegments::masker());
        let mut speech = json!({
            "model": "m",
            "input": "clean input",
            "instructions": "quiet instructions"
        });
        let moderation = moderate_speech_request_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut speech,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(!moderation.verdict.is_block());
        assert_eq!(speech["input"], "<M0:CLEAN INPUT>");
        assert_eq!(speech["instructions"], "<M1:QUIET INSTRUCTIONS>");

        let mut image = json!({
            "data": [{"url": "https://example.test/image.png", "revised_prompt": "new prompt"}]
        });
        let moderation = moderate_images_response_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut image,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(!moderation.verdict.is_block());
        assert_eq!(image["data"][0]["revised_prompt"], "<M0:NEW PROMPT>");
    }

    #[tokio::test]
    async fn prior_and_remote_blocks_mark_structured_capture_unsafe() {
        let panic_chain = seg_chain(StubSegments {
            verdict: GuardrailVerdict::Allow,
            mask: false,
            panic_if_called: true,
        });
        let mut body = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "raw a@x.com"}]
        });
        let moderation = moderate_anthropic_request(
            &panic_chain,
            GuardrailVerdict::block("already blocked"),
            &mut body,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
        assert_eq!(body["messages"][0]["content"], "raw a@x.com");

        let remote_block_chain = seg_chain(StubSegments {
            verdict: GuardrailVerdict::block("remote blocked"),
            mask: true,
            panic_if_called: false,
        });
        let moderation = moderate_anthropic_request(
            &remote_block_chain,
            GuardrailVerdict::Allow,
            &mut body,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
        assert_eq!(body["messages"][0]["content"], "raw a@x.com");
    }

    #[tokio::test]
    async fn local_prior_block_is_capture_unsafe_even_after_synchronous_mask_pass() {
        let chain = both();
        let mut body = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "raw a@x.com"}]
        });
        let moderation = moderate_anthropic_request(
            chain.as_ref(),
            GuardrailVerdict::block("local keyword blocked"),
            &mut body,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);

        let redaction = redact_anthropic_request(chain.as_ref(), &mut body);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(body["messages"][0]["content"], "raw [EMAIL_REDACTED]");
    }

    /// A Block from the segment pass leaves the body untouched (no mask
    /// write-back on a dead request) and propagates the verdict.
    #[tokio::test]
    async fn moderate_body_block_leaves_body_untouched() {
        let chain = seg_chain(StubSegments {
            verdict: GuardrailVerdict::block("pii blocked"),
            mask: true,
            panic_if_called: false,
        });
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "original"}]
        }))
        .unwrap();
        let mut counts = RedactionCounts::new();
        let verdict = moderate_body(
            &chain,
            Direction::Input,
            GuardrailVerdict::Allow,
            &mut counts,
            &mut Vec::new(),
            |g| redact_chat_format(g, &mut req),
        )
        .await;
        assert!(verdict.verdict.is_block());
        assert!(!verdict.capture_safe);
        assert_eq!(req.messages[0].content.as_deref(), Some("original"));
        assert!(counts.is_empty(), "no counts on a blocked request");
    }

    #[tokio::test]
    async fn segment_bypass_releases_body_but_marks_capture_unsafe() {
        let bypass = GuardrailVerdict::Bypass {
            reason: "remote segment service unavailable".to_string(),
        };
        let chain = seg_chain(StubSegments {
            verdict: bypass.clone(),
            mask: false,
            panic_if_called: false,
        });
        let mut response = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: ChatMessage::assistant("raw output"),
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };
        let moderation = moderate_chat_response_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut response,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;

        assert_eq!(moderation.verdict, bypass);
        assert!(!moderation.capture_safe);
        assert_eq!(response.message.content.as_deref(), Some("raw output"));

        let mut responses_body = json!({
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "raw output"}]}]
        });
        let moderation = moderate_responses_response_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut responses_body,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_bypass());
        assert!(!moderation.capture_safe);
        assert_eq!(
            responses_body["output"][0]["content"][0]["text"],
            "raw output"
        );

        let mut local_only_response = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: ChatMessage::assistant("raw output"),
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };
        let moderation = moderate_chat_response_structured(
            both().as_ref(),
            GuardrailVerdict::Bypass {
                reason: "non-segment service unavailable".to_string(),
            },
            &mut local_only_response,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_bypass());
        assert!(!moderation.capture_safe);
    }

    #[tokio::test]
    async fn moderate_body_blocks_when_collect_and_apply_walks_drift() {
        let chain = seg_chain(StubSegments::masker());
        let walk_number = std::cell::Cell::new(0usize);
        let mut counts = RedactionCounts::new();
        let moderation = moderate_body(
            &chain,
            Direction::Input,
            GuardrailVerdict::Allow,
            &mut counts,
            &mut Vec::new(),
            |guardrail| {
                let current = walk_number.get();
                walk_number.set(current + 1);
                let offered = if current == 0 { "original" } else { "drifted" };
                let _ = guardrail.redact_input_text(offered);
                RedactionCounts::new()
            },
        )
        .await;

        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
        assert!(counts.is_empty());
        assert_eq!(walk_number.get(), 2, "drift blocks before mutation");
    }

    #[tokio::test]
    async fn structured_moderation_blocks_when_collect_and_apply_walks_drift() {
        let chain = seg_chain(StubSegments::masker());
        let walk_number = std::cell::Cell::new(0usize);
        let mut counts = RedactionCounts::new();
        let moderation = moderate_structured_body(
            &chain,
            Direction::Input,
            GuardrailVerdict::Allow,
            &mut counts,
            &mut Vec::new(),
            |guardrail| {
                let current = walk_number.get();
                walk_number.set(current + 1);
                let offered = if current == 0 { "original" } else { "drifted" };
                let _ = guardrail.redact_input_text(offered);
                AnthropicRequestRedaction {
                    counts: RedactionCounts::new(),
                    unrewritable_tool_key: false,
                }
            },
        )
        .await;

        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
        assert!(counts.is_empty());
        assert_eq!(walk_number.get(), 2, "drift blocks before mutation");
    }

    /// An already-blocked prior verdict skips the remote call entirely
    /// (the request is dead — don't burn a provider call), and a chain
    /// with no segment member is a no-op.
    #[tokio::test]
    async fn moderate_body_skips_remote_when_blocked_or_absent() {
        let chain = seg_chain(StubSegments {
            verdict: GuardrailVerdict::Allow,
            mask: false,
            panic_if_called: true,
        });
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}]
        }))
        .unwrap();
        let mut counts = RedactionCounts::new();
        let verdict = moderate_body(
            &chain,
            Direction::Input,
            GuardrailVerdict::block("already blocked"),
            &mut counts,
            &mut Vec::new(),
            |g| redact_chat_format(g, &mut req),
        )
        .await;
        assert!(verdict.verdict.is_block(), "prior Block passes through");
        assert!(
            !verdict.capture_safe,
            "skipping an attached remote segment pass makes capture unsafe"
        );

        // A sync-only (non-segment) chain never enters the pass.
        let sync_only = both();
        let verdict = moderate_body(
            sync_only.as_ref(),
            Direction::Input,
            GuardrailVerdict::Allow,
            &mut counts,
            &mut Vec::new(),
            |_| panic!("walk must not run when no segment member exists"),
        )
        .await;
        assert_eq!(verdict.verdict, GuardrailVerdict::Allow);
    }

    /// The round trip through the Anthropic SSE walker: the masked
    /// channel text lands on the channel's first frame (later frames
    /// empty) even though the applier only returns marker counts — the
    /// gate that discards count-less SSE rewrites must fire on them.
    #[tokio::test]
    async fn moderate_body_masks_anthropic_sse_channels() {
        let chain = seg_chain(StubSegments::masker());
        let mut held: Vec<u8> = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n",
        )
        .as_bytes()
        .to_vec();
        let mut counts = RedactionCounts::new();
        let moderation = moderate_anthropic_sse(
            &chain,
            GuardrailVerdict::Allow,
            &mut held,
            &mut counts,
            &mut Vec::new(),
        )
        .await;
        assert_eq!(moderation.verdict, GuardrailVerdict::Allow);
        let out = String::from_utf8(held.clone()).unwrap();
        assert!(
            out.contains("<M0:HELLO WORLD>"),
            "channel text masked as one positional slot: {out}",
        );
        assert_eq!(counts.get("EMAIL"), Some(&1));
        // The capture-rebuild helper reads the masked channel back.
        assert_eq!(anthropic_sse_text(&held), "<M0:HELLO WORLD>");
    }

    #[tokio::test]
    async fn segment_mask_of_anthropic_sse_tool_key_fails_closed() {
        let chain = seg_chain(StubSegments::masker());
        let mut held = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"sensitive-\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"key\\\":\\\"safe\\\"}\"}}\n\n",
        )
        .as_bytes()
        .to_vec();
        let mut counts = RedactionCounts::new();
        let moderation = moderate_anthropic_sse(
            &chain,
            GuardrailVerdict::Allow,
            &mut held,
            &mut counts,
            &mut Vec::new(),
        )
        .await;

        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
    }

    #[tokio::test]
    async fn segment_mask_of_anthropic_sse_start_input_key_fails_closed() {
        let chain = seg_chain(StubSegments::masker());
        let mut held = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"send\",\"input\":{\"sensitive-key\":\"safe\"}}}\n\n",
        )
        .as_bytes()
        .to_vec();
        let moderation = moderate_anthropic_sse(
            &chain,
            GuardrailVerdict::Allow,
            &mut held,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;

        assert!(moderation.verdict.is_block());
        assert!(!moderation.capture_safe);
    }

    /// `responses_sse_text` assembles `output_text` deltas per channel —
    /// the capture-rebuild source after a segment mask.
    #[test]
    fn responses_sse_text_assembles_channels() {
        let raw = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"i1\",\"content_index\":0,\"delta\":\"foo \"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"i1\",\"content_index\":0,\"delta\":\"bar\"}\n\n",
        );
        assert_eq!(responses_sse_text(raw.as_bytes()), "foo bar");
    }

    /// Channels concatenate in first-seen (emission) order, not item-id
    /// lexicographic order — the rebuilt capture must read like the
    /// stream the client saw.
    #[test]
    fn responses_sse_text_preserves_emission_order() {
        let raw = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"zzz\",\"content_index\":0,\"delta\":\"first \"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"aaa\",\"content_index\":0,\"delta\":\"second\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"zzz\",\"content_index\":0,\"delta\":\"more\"}\n\n",
        );
        assert_eq!(responses_sse_text(raw.as_bytes()), "first moresecond");
    }
}
