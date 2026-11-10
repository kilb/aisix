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
//!
//! The walkers are also the scan surface of the local guardrail kinds
//! (`keyword`, `pii` — #1027): [`moderate_body`] collects what a walker
//! offers and those kinds judge each slot on its own. A slot a walker does
//! not offer is a slot they never judge, so a new scanned field is offered
//! here — rewritable, or through `Guardrail::observe_unrewritable` when it
//! cannot be rewritten.

use std::collections::BTreeMap;
use std::ops::Range;

use aisix_gateway::{ChatChunk, ChatFormat, ChatResponse, Role};
use aisix_guardrails::Guardrail;
use serde_json::Value;

/// detector name → masked-span count. Mirrors
/// `UsageEvent::redacted_entity_counts`.
pub type RedactionCounts = BTreeMap<String, u32>;

/// How many spans `counts` says were masked: compared before and after a
/// walk of one choice, it says whether that walk rewrote anything. A choice
/// that was rewritten loses its `logprobs`, whose tokens are the answer's text
/// in the clear and which no mask can rewrite and leave meaningful.
fn masked_spans(counts: &RedactionCounts) -> u64 {
    counts.values().map(|n| u64::from(*n)).sum()
}

/// Merge `from` into `into` (repeated small helper; counts are tiny maps).
pub fn merge_counts(into: &mut RedactionCounts, from: RedactionCounts) {
    for (k, v) in from {
        *into.entry(k).or_insert(0) += v;
    }
}

/// Which side's redactor to run, and — on the request side — which part of
/// the conversation the text belongs to. The two sides can be configured
/// independently (`hook_point`), so every JSON-walking helper takes this
/// rather than hardcoding one.
///
/// The request side splits because a guardrail row may carry
/// `input_messages: latest_turn`, which narrows it to the part of the
/// conversation the model has not answered yet. Carrying the split in this
/// type rather than in a parallel argument is what makes it total: every
/// slot a walker offers has to name which half it is in, and the compiler
/// asks the question at each new one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Request text inside the latest-turn window: this turn's user
    /// message and the tool results answering it. Every input guardrail
    /// reads it. Also the direction a caller names for a whole-request
    /// pass, and the only one the single-input surfaces ever use.
    Input,
    /// Request text the model has already replied to, plus every system
    /// message. Only rows left on `input_messages: all` read it.
    InputHistory,
    Output,
    /// Response text restating a slot the walk already offered — a
    /// Responses stream's aggregate events repeat its deltas. Rewritten
    /// like [`Direction::Output`]; a collector records it as
    /// [`aisix_guardrails::SegmentRole::Echo`] so it is counted once.
    OutputEcho,
}

impl Direction {
    /// Request side, either window.
    pub fn is_input(self) -> bool {
        matches!(self, Direction::Input | Direction::InputHistory)
    }

    /// The request-side direction for a slot, given whether it sits inside
    /// the latest-turn window.
    fn input_window(in_latest_turn: bool) -> Self {
        if in_latest_turn {
            Direction::Input
        } else {
            Direction::InputHistory
        }
    }
}

fn redact_str(
    chain: &dyn Guardrail,
    dir: Direction,
    text: &str,
) -> Option<aisix_guardrails::Redaction> {
    match dir {
        Direction::Input => chain.redact_input_text_in_turn(text, true),
        Direction::InputHistory => chain.redact_input_text_in_turn(text, false),
        Direction::Output => chain.redact_output_text(text),
        Direction::OutputEcho => chain.redact_output_echo(text),
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

/// Whether a collected slot goes to the remote segment pass, and so comes back
/// through the apply walk. A label or a scan-only slot is judged by the local
/// pass only; the walker reports a structural field it needs a remote verdict on
/// through a rewritable slot, and refuses a mask of it.
fn reaches_remote_pass(role: aisix_guardrails::SegmentRole) -> bool {
    use aisix_guardrails::SegmentRole;
    matches!(role, SegmentRole::Rewritable | SegmentRole::Echo)
}

/// Pass-1 probe: records every text slot the walker offers, with the
/// latest-turn window each one belongs to, plus the slots the walker
/// judges without rewriting (object keys, tool names). Never rewrites, so
/// the body is bit-identical after the collect walk.
#[derive(Default)]
struct SegmentCollector {
    segments: std::sync::Mutex<Vec<aisix_guardrails::ScanSegment>>,
    /// Where each conversation message began, counted in the slots the remote
    /// pass reads (see [`Self::newest_from`]).
    message_starts: std::sync::Mutex<Vec<usize>>,
    /// How many collected slots the remote pass reads.
    remote_slots: std::sync::atomic::AtomicUsize,
}

impl SegmentCollector {
    /// Everything collected, in walk order.
    fn take(&self) -> Vec<aisix_guardrails::ScanSegment> {
        std::mem::take(&mut *self.segments.lock().expect("collector poisoned"))
    }

    /// The slots the remote pass reads, in walk order. Also what an inspection
    /// text is built from: a label or a scan-only slot is judged, not part of
    /// the text.
    fn take_rewritable_texts(&self) -> Vec<String> {
        self.take()
            .into_iter()
            .filter(|segment| reaches_remote_pass(segment.role))
            .map(|segment| segment.text)
            .collect()
    }

    /// First slot of the newest conversation message that produced any, when
    /// the walker marked message boundaries, as an index into
    /// [`Self::take_rewritable_texts`]. Read before taking.
    fn newest_from(&self) -> Option<usize> {
        let len = self.remote_slots.load(std::sync::atomic::Ordering::Relaxed);
        self.message_starts
            .lock()
            .expect("collector poisoned")
            .iter()
            .rev()
            .copied()
            .find(|&start| start < len)
    }

    fn push(&self, text: &str, role: aisix_guardrails::SegmentRole, in_latest_turn: bool) {
        if reaches_remote_pass(role) {
            self.remote_slots
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.segments
            .lock()
            .expect("collector poisoned")
            .push(aisix_guardrails::ScanSegment {
                text: text.to_owned(),
                role,
                in_latest_turn,
            });
    }

    fn record(&self, text: &str, in_latest_turn: bool) -> Option<aisix_guardrails::Redaction> {
        self.push(
            text,
            aisix_guardrails::SegmentRole::Rewritable,
            in_latest_turn,
        );
        None
    }
}

impl Guardrail for SegmentCollector {
    fn name(&self) -> &'static str {
        "segment-collector"
    }
    fn segment_message_start(&self) {
        let len = self.remote_slots.load(std::sync::atomic::Ordering::Relaxed);
        self.message_starts
            .lock()
            .expect("collector poisoned")
            .push(len);
    }
    fn redacts_input(&self) -> bool {
        true
    }
    fn redacts_output(&self) -> bool {
        true
    }
    fn redact_input_text(&self, text: &str) -> Option<aisix_guardrails::Redaction> {
        self.record(text, true)
    }
    // The window arrives here and nowhere else: `redact_str` routes every
    // request-side slot through this method, so overriding the plain one
    // alone would collect the text and lose which half it came from.
    fn redact_input_text_in_turn(
        &self,
        text: &str,
        in_latest_turn: bool,
    ) -> Option<aisix_guardrails::Redaction> {
        self.record(text, in_latest_turn)
    }
    fn redact_output_text(&self, text: &str) -> Option<aisix_guardrails::Redaction> {
        self.record(text, true)
    }
    fn observe_unrewritable(
        &self,
        text: &str,
        role: aisix_guardrails::SegmentRole,
        in_latest_turn: bool,
    ) {
        self.push(text, role, in_latest_turn);
    }
    fn redact_output_echo(&self, text: &str) -> Option<aisix_guardrails::Redaction> {
        self.push(text, aisix_guardrails::SegmentRole::Echo, true);
        None
    }
}

/// The segments `walk` offers, collected without rewriting anything — for
/// a call site with no held body to hand [`moderate_body`], which builds
/// the slots itself and runs the local pass on them directly.
pub fn collect_segments(walk: impl FnOnce(&dyn Guardrail)) -> Vec<aisix_guardrails::ScanSegment> {
    let collector = SegmentCollector::default();
    walk(&collector);
    collector.take()
}

/// Offer a slot the walker judges but never rewrites. Empty text is
/// skipped, like an empty rewritable slot.
fn offer_unrewritable(
    chain: &dyn Guardrail,
    dir: Direction,
    text: &str,
    role: aisix_guardrails::SegmentRole,
) {
    if !text.is_empty() {
        chain.observe_unrewritable(text, role, dir != Direction::InputHistory);
    }
}

/// A tool name is structural — masking it would break the call it
/// identifies — so it is judged and forwarded: a block rule refuses, a
/// mask hit passes.
fn offer_tool_name(chain: &dyn Guardrail, dir: Direction, name: Option<&Value>) {
    if let Some(name) = name.and_then(Value::as_str) {
        offer_unrewritable(chain, dir, name, aisix_guardrails::SegmentRole::ScanOnly);
    }
}

/// A field holding a tool-call handle: a chat call's or an Anthropic block's
/// `id`, a Responses `call_id`, the result-side back-references, an approval's
/// `approval_request_id`. A handle links two items and carries no content.
fn is_call_handle_field(field: &str) -> bool {
    field == "approval_request_id" || aisix_gateway::tool_calls::TOOL_CALL_ID_KEYS.contains(&field)
}

/// Offer a call handle to the local pass only: a keyword rule still refuses it
/// and a mask hit blocks, since a handle cannot be rewritten. A remote
/// moderation service never reads one, the same as the inspection text: Gemini
/// ids carry hundreds of bytes of thought signature, and generated ids are
/// random strings a service can match by chance.
fn offer_call_handle(chain: &dyn Guardrail, dir: Direction, handle: &Value) {
    match handle {
        Value::String(text) => {
            offer_unrewritable(chain, dir, text, aisix_guardrails::SegmentRole::Label)
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| offer_call_handle(chain, dir, item)),
        Value::Object(map) => map
            .values()
            .for_each(|child| offer_call_handle(chain, dir, child)),
        _ => {}
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

/// What one hook's moderation decided about a wire body.
pub struct BodyModeration {
    pub verdict: aisix_guardrails::GuardrailVerdict,
    /// Safe to attach the walked body to a full-content exporter. False when
    /// a block, a bypass, or collect/apply drift left the body without a
    /// complete rewrite, or when rewriting a structural key would change the
    /// tool contract. Blocked output is never capture-safe because output
    /// callers intentionally omit it.
    pub capture_safe: bool,
}

/// The name the structured request and response walks use.
pub type AnthropicModeration = BodyModeration;

/// Refusal when the apply walk did not visit what the collect walk did. A
/// failed mask splice carries the tag the proxy's other mask refusals carry.
fn segment_walk_drift_verdict() -> aisix_guardrails::GuardrailVerdict {
    aisix_guardrails::GuardrailVerdict::block_unavailable(
        "segment apply walk drifted from collect walk; blocking to avoid releasing unmasked text",
        crate::error::TAG_MASK_WRITEBACK_FAILED,
    )
}

/// Complete one hook's moderation over a wire body: fold the already-run
/// `check_*_non_segment` verdict with the two segment passes, both fed by
/// ONE collect walk of `walk`:
///
/// - the local pass (`keyword`, `pii`, #1027) judges each collected slot on
///   its own — the same slots the sync mask pass rewrites, so a block, a
///   monitor `would_mask` and an enforced mask cannot disagree;
/// - the remote segment pass (Bedrock, Presidio, Lakera, Aliyun AI,
///   custom) moderates the rewritable slots in one provider call and its
///   masked replacements are written back through `walk`; the provider's
///   entity counts merge into `counts_out` (names only — #932 no-leak).
///
/// Nothing runs when the check already blocked (the request is dead —
/// don't burn a provider call), and each pass is skipped when the chain
/// has no member for it.
///
/// `model` is the model the request addressed, exactly as the check pass
/// saw it in `ChatFormat::model`, so a segment member that exposes it (a
/// `custom` script's `ctx.model`) reads the same value on either pass.
/// Input only: the output hook passes `None`.
///
/// `walk` reports a structural field a mask would change through
/// [`AnthropicRequestRedaction::unrewritable_tool_key`]; a walk that returns
/// bare counts has none. Such a field is a refusal, never a rewrite.
pub async fn moderate_body<R: Into<AnthropicRequestRedaction>>(
    chain: &dyn Guardrail,
    dir: Direction,
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
    walk: impl FnMut(&dyn Guardrail) -> R,
) -> BodyModeration {
    moderate_body_inner(
        chain,
        dir,
        model,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        Vec::new(),
        LocalSegments::Walk(Vec::new()),
        walk,
    )
    .await
}

/// [`moderate_body`] plus text that must be SCANNED but must never be
/// written back.
///
/// The walker is both the collect pass and the apply pass, so a slot it
/// offers is a slot the provider may rewrite. That makes it the wrong place
/// for a signed Anthropic `thinking` block: the block has to reach the
/// screening call (an operator's block rule must still fire on it) and must
/// come back byte-identical (#1104). `scan_only` goes ahead of the
/// collected slots and is dropped from the masked reply before the apply
/// walk, so those texts are judged and never rewritten. Each text carries
/// whether it sits in the latest-turn window.
///
/// Without it the segment kinds — `bedrock`, `presidio`, `lakera`,
/// `aliyun_ai_guardrail`, `custom` — never saw thinking content at all on
/// `/v1/messages`: they answer `Allow` from `check_input_non_segment` by
/// design and take their real verdict from this pass, so a block policy on
/// any of them was bypassable by moving the text into a thinking block.
#[allow(clippy::too_many_arguments)]
pub async fn moderate_body_scanning<R: Into<AnthropicRequestRedaction>>(
    chain: &dyn Guardrail,
    dir: Direction,
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
    scan_only: Vec<(String, bool)>,
    walk: impl FnMut(&dyn Guardrail) -> R,
) -> BodyModeration {
    moderate_body_inner(
        chain,
        dir,
        model,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        scan_only,
        LocalSegments::Walk(Vec::new()),
        walk,
    )
    .await
}

/// [`moderate_body`] plus segments only the LOCAL pass judges — text the
/// walk cannot offer because no held body carries it: frames a held-back
/// stream excised and will never release (#1100), or a live-forwarded
/// stream's accumulated text. The `check_*_non_segment` text already
/// carries them for every other kind, and the remote segment pass never
/// saw them.
#[allow(clippy::too_many_arguments)]
pub async fn moderate_body_local_extra<R: Into<AnthropicRequestRedaction>>(
    chain: &dyn Guardrail,
    dir: Direction,
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
    local_extra: Vec<aisix_guardrails::ScanSegment>,
    walk: impl FnMut(&dyn Guardrail) -> R,
) -> BodyModeration {
    moderate_body_inner(
        chain,
        dir,
        model,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        Vec::new(),
        LocalSegments::Walk(local_extra),
        walk,
    )
    .await
}

/// [`moderate_body`] with the local pass judging `segments`, when given,
/// INSTEAD of what `walk` offers — for a live-forwarded stream, where the
/// walk can only offer the flattened text the remote pass has always
/// read, while the call site can rebuild the slots the buffered branch's
/// mask walker would rewrite.
#[allow(clippy::too_many_arguments)]
pub async fn moderate_body_local_given<R: Into<AnthropicRequestRedaction>>(
    chain: &dyn Guardrail,
    dir: Direction,
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
    segments: Option<Vec<aisix_guardrails::ScanSegment>>,
    walk: impl FnMut(&dyn Guardrail) -> R,
) -> BodyModeration {
    moderate_body_inner(
        chain,
        dir,
        model,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        Vec::new(),
        segments.map_or(LocalSegments::Walk(Vec::new()), LocalSegments::Given),
        walk,
    )
    .await
}

/// Where the local pass takes its segments from.
enum LocalSegments {
    /// The collect walk, plus these.
    Walk(Vec<aisix_guardrails::ScanSegment>),
    /// These alone.
    Given(Vec<aisix_guardrails::ScanSegment>),
}

#[allow(clippy::too_many_arguments)]
async fn moderate_body_inner<R: Into<AnthropicRequestRedaction>>(
    chain: &dyn Guardrail,
    dir: Direction,
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
    scan_only: Vec<(String, bool)>,
    local: LocalSegments,
    mut walk: impl FnMut(&dyn Guardrail) -> R,
) -> BodyModeration {
    use aisix_guardrails::{ScanSegment, SegmentRole};

    let has_local = chain.checks_local_segments();
    let remote = chain.moderates_segments();
    if non_segment_verdict.is_block() || !(has_local || remote) {
        // A block-only detector has no rewrite to remove the matched secret.
        // Never attach that rejected input/output to a full-content exporter.
        let capture_safe = !non_segment_verdict.is_block() && !non_segment_verdict.is_bypass();
        return BodyModeration {
            verdict: non_segment_verdict,
            capture_safe,
        };
    }
    let collector = SegmentCollector::default();
    let _ = walk(&collector);
    let newest_from = collector.newest_from();
    let collected = collector.take();
    let mut verdict = non_segment_verdict;
    // Both passes run even with zero collected slots. "Nothing to scan" is
    // not "nothing to decide": a segment-moderating member may hold a
    // verdict that does not depend on the text (a `kind: custom` policy
    // script), and skipping the pass turned an operator's block rule into
    // a silent allow on any request whose scannable slots were all empty.
    // Every member that needs a remote call short-circuits empty input
    // itself (bedrock/lakera/presidio/aliyun all refuse empty content), so
    // consulting the chain here costs no provider round-trip.
    if has_local {
        let segments = match local {
            LocalSegments::Given(segments) => segments,
            LocalSegments::Walk(extra) => {
                let mut segments = collected.clone();
                segments.extend(scan_only.iter().map(|(text, in_latest_turn)| ScanSegment {
                    text: text.clone(),
                    role: SegmentRole::ScanOnly,
                    in_latest_turn: *in_latest_turn,
                }));
                segments.extend(extra);
                segments
            }
        };
        let (local_verdict, mut hits) = chain.check_local_segments(&segments, dir.is_input());
        monitor_hits_out.append(&mut hits);
        verdict = verdict.merged_with(local_verdict);
        if verdict.is_block() {
            return BodyModeration {
                verdict,
                capture_safe: false,
            };
        }
    }
    if !remote {
        return BodyModeration {
            capture_safe: !verdict.is_bypass(),
            verdict,
        };
    }
    // The scan-only texts go FIRST, so the newest message stays last and
    // `newest_from` still names it. A moderator that has to bound how much it
    // sends covers `newest_from..` in full: appended, a long signed thinking
    // block replayed from an old turn would count as part of the newest
    // message and could get an ordinary request refused as too long.
    let scan_only_len = scan_only.len();
    let mut texts = Vec::with_capacity(scan_only_len + collected.len());
    let mut in_latest_turn = Vec::with_capacity(scan_only_len + collected.len());
    for (text, in_turn) in scan_only {
        texts.push(text);
        in_latest_turn.push(in_turn);
    }
    let mut originals = Vec::with_capacity(collected.len());
    for segment in collected {
        if reaches_remote_pass(segment.role) {
            originals.push(segment.text.clone());
            texts.push(segment.text);
            in_latest_turn.push(segment.in_latest_turn);
        }
    }
    let newest_from = newest_from.map(|start| start + scan_only_len);
    let mut outcome = match dir {
        Direction::Input | Direction::InputHistory => {
            chain
                .moderate_input_segments_in_turn(&texts, &in_latest_turn, newest_from, model)
                .await
        }
        Direction::Output | Direction::OutputEcho => chain.moderate_output_segments(&texts).await,
    };
    monitor_hits_out.append(&mut outcome.monitor_hits);
    let segment_capture_safe = !outcome.verdict.is_bypass();
    if outcome.verdict.is_block() {
        return BodyModeration {
            verdict: verdict.merged_with(outcome.verdict),
            capture_safe: false,
        };
    }
    let mut unrewritable_tool_key = false;
    if let Some(mut masked) = outcome.masked {
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
        if verifier.take_rewritable_texts() != originals || masked.len() != texts.len() {
            return BodyModeration {
                verdict: verdict.merged_with(segment_walk_drift_verdict()),
                capture_safe: false,
            };
        }
        // Drop the scan-only head. The walk never offers those texts back, so
        // a signed block stays byte-identical, and the applier is positional.
        masked.drain(..scan_only_len);
        let applier = SegmentApplier::new(masked, originals);
        // Marker counts are plumbing (see SEGMENT_APPLY_MARKER) — discard
        // them; the provider counts below are the real ones.
        let applied: AnthropicRequestRedaction = walk(&applier).into();
        unrewritable_tool_key = applied.unrewritable_tool_key;
        if !applier.is_aligned() {
            return BodyModeration {
                verdict: verdict.merged_with(segment_walk_drift_verdict()),
                capture_safe: false,
            };
        }
        merge_counts(counts_out, outcome.counts);
    }
    let verdict = if unrewritable_tool_key {
        unrewritable_tool_key_verdict()
    } else {
        verdict.merged_with(outcome.verdict)
    };
    BodyModeration {
        capture_safe: !verdict.is_block() && !verdict.is_bypass() && segment_capture_safe,
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
/// the document). Each object key and each number is offered as a
/// [`aisix_guardrails::SegmentRole::Label`]: judged, never rewritten.
///
/// `offer_keys` is false for a walker that already ran
/// [`detect_unrewritable_object_keys`] over this value: it judged every key
/// through that, and offering it again would count one key twice.
fn walk_value_strings(
    chain: &dyn Guardrail,
    dir: Direction,
    v: &mut Value,
    counts: &mut RedactionCounts,
    offer_keys: bool,
) {
    match v {
        Value::String(_) => apply_to_value_string(chain, dir, v, counts),
        Value::Array(items) => {
            for item in items {
                walk_value_strings(chain, dir, item, counts, offer_keys);
            }
        }
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if offer_keys {
                    offer_unrewritable(chain, dir, key, aisix_guardrails::SegmentRole::Label);
                }
                walk_value_strings(chain, dir, val, counts, offer_keys);
            }
        }
        Value::Number(n) => {
            offer_unrewritable(
                chain,
                dir,
                &n.to_string(),
                aisix_guardrails::SegmentRole::Label,
            );
        }
        _ => {}
    }
}

/// 批处理 / 微调上传的 JSONL 文件经 [`redact_jsonl_upload`] 改写后的结果。
pub(crate) struct JsonlUploadRedaction {
    /// 要转发的全文：逐行重新序列化的那份，也就是被检查过的那份。
    pub text: String,
    pub counts: RedactionCounts,
    /// 某个协议字段（`custom_id` / `method` / `url` / `model`）命中了 mask 规则。
    /// 它既不能改写（两行会变成同一个 `custom_id`，结果再也对不回请求；改了
    /// `url` / `model` 请求就打错地方），也不能原样放行，调用方应拒绝上传。
    /// 协议字段以外某个对象的键命中时也一样：改了键，上游读到的就是另一个字段。
    /// 文件有的行是 JSON 对象、有的行不是时也是这样（此时 `text` 为空）：逐行
    /// 改写不了，当纯文本改写又会改到协议字段和键、看不见 JSON 转义写出的内容。
    pub unrewritable: bool,
}

/// 一行 JSONL 里不能改写的协议字段。
const JSONL_PROTOCOL_FIELDS: &[&str] = &["custom_id", "method", "url", "model"];

/// 按行解析 JSONL 上传，改写协议字段以外的每个字符串：batch 行的 `body`
/// （跳过 `body.model`）与顶层其余字段，微调行除协议字段外的整行。
///
/// 在原始字节上做替换会改到协议字段、可能弄坏 JSON 结构，还会漏掉 JSON 转义
/// 写出的内容（`\u0040` 解码后就是 `@`）；逐行解析后按值改写，三者都不会发生。
///
/// 每一行都以解析后的值重新序列化再转发，改没改都一样：检查的必须就是转发
/// 的。原样转发原始字节时，一个重复键（解析器只保留最后一个值）就能把带 PII
/// 的那一份藏在被检查的那一份后面送出去。文件开头的 UTF-8 BOM 不属于第一行，
/// 解析时跳过、转发时原样放回。返回 `None` 表示没有一行是 JSON 对象，调用方按
/// 纯文本处理；有的行是、有的行不是时报告为改写不了。
pub(crate) fn redact_jsonl_upload(
    chain: &dyn Guardrail,
    text: &str,
) -> Option<JsonlUploadRedaction> {
    // U+FEFF 不是空白，`trim` 去不掉它。
    let (bom, text) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    };
    let mut parsed = Vec::new();
    let mut other_lines = false;
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            parsed.push((raw, None));
            continue;
        }
        match serde_json::from_str::<Value>(line)
            .ok()
            .filter(Value::is_object)
        {
            Some(value) => parsed.push((raw, Some(value))),
            None => other_lines = true,
        }
    }
    if other_lines {
        return parsed
            .iter()
            .any(|(_, value)| value.is_some())
            .then(|| JsonlUploadRedaction {
                text: String::new(),
                counts: RedactionCounts::new(),
                unrewritable: true,
            });
    }

    let mut counts = RedactionCounts::new();
    let mut unrewritable = false;
    let mut out = Vec::with_capacity(parsed.len());
    for (raw, value) in parsed {
        let Some(mut value) = value else {
            out.push(raw.to_string());
            continue;
        };
        unrewritable |= detect_named_structural_fields(
            chain,
            Direction::Input,
            &value,
            JSONL_PROTOCOL_FIELDS,
            &mut counts,
        );
        // 改写只动值、不动键：整行每一层对象的键 —— 协议字段的值里的也算，它们
        // 不是字符串时同样原样送到上游 —— 命中 mask 规则就报告改写不了（与 chat
        // 对改写不了的键的处理一致）。
        unrewritable |=
            detect_unrewritable_object_keys(chain, Direction::Input, &value, &mut counts);
        if let Some(map) = value.as_object_mut() {
            for (key, slot) in map.iter_mut() {
                if JSONL_PROTOCOL_FIELDS.contains(&key.as_str()) {
                    continue;
                }
                match slot {
                    // batch 行的请求体：`model` 是协议字段，其余照常改写。
                    Value::Object(body) if key == "body" => {
                        unrewritable |= body.get("model").is_some_and(|model| {
                            detect_unrewritable_value_strings(
                                chain,
                                Direction::Input,
                                model,
                                &mut counts,
                            )
                        });
                        for (field, inner) in body.iter_mut() {
                            if field != "model" {
                                walk_value_strings(
                                    chain,
                                    Direction::Input,
                                    inner,
                                    &mut counts,
                                    false,
                                );
                            }
                        }
                    }
                    _ => walk_value_strings(chain, Direction::Input, slot, &mut counts, false),
                }
            }
        }
        let mut line = serde_json::to_string(&value).ok()?;
        if raw.ends_with('\r') {
            line.push('\r');
        }
        out.push(line);
    }

    Some(JsonlUploadRedaction {
        text: format!("{bom}{}", out.join("\n")),
        counts,
        unrewritable,
    })
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

/// Builds a JSON tree like `serde_json::Value`'s own deserializer does,
/// except that an object naming a key more than once keeps every value of
/// that key, in document order, as an array under the key, and reports that
/// it did.
///
/// `serde_json` keeps only the last of a repeated key, while the encoded
/// string a request or a response carries keeps them all, and RFC 8259 §4
/// leaves which one a reader takes to the reader. A scan of the last-wins
/// tree therefore reads one value and lets the others through unread.
struct EveryValue<'a> {
    repeated: &'a mut bool,
}

impl<'de> serde::de::DeserializeSeed<'de> for EveryValue<'_> {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for EveryValue<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }
    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(EveryValue {
            repeated: &mut *self.repeated,
        })? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        let mut repeated_keys: Vec<String> = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(EveryValue {
                repeated: &mut *self.repeated,
            })?;
            let Some(slot) = out.get_mut(&key) else {
                out.insert(key, value);
                continue;
            };
            *self.repeated = true;
            if repeated_keys.contains(&key) {
                if let Value::Array(all) = slot {
                    all.push(value);
                }
            } else {
                let first = std::mem::take(slot);
                *slot = Value::Array(vec![first, value]);
                repeated_keys.push(key);
            }
        }
        Ok(Value::Object(out))
    }
}

/// `encoded` parsed as JSON, and whether an object in it repeats a key
/// (see [`EveryValue`]). Without a repeated key the tree is the one
/// `serde_json::from_str::<Value>` builds.
fn parse_every_value(encoded: &str) -> serde_json::Result<(Value, bool)> {
    use serde::de::DeserializeSeed;
    let mut repeated = false;
    let mut de = serde_json::Deserializer::from_str(encoded);
    let value = EveryValue {
        repeated: &mut repeated,
    }
    .deserialize(&mut de)?;
    de.end()?;
    Ok((value, repeated))
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
    match parse_every_value(encoded) {
        Ok((mut v, repeated)) => {
            let mut unrewritable_tool_key = if detect_keys {
                let mut key_counts = RedactionCounts::new();
                let found = detect_unrewritable_object_keys(chain, dir, &v, &mut key_counts);
                merge_counts(counts, key_counts);
                found
            } else {
                false
            };

            let mut value_counts = RedactionCounts::new();
            walk_value_strings(chain, dir, &mut v, &mut value_counts, !detect_keys);
            if !value_counts.is_empty() {
                if repeated {
                    // A document that repeats a key was scanned with every
                    // value it carries, and goes out as written when nothing
                    // matched. A match in it cannot be written back:
                    // re-serialising keeps one value per key, which changes
                    // what the document says, and which value a reader takes
                    // is the reader's choice. So it is reported the way an
                    // unrewritable key is, and the caller refuses rather than
                    // forward the unmasked bytes.
                    merge_counts(counts, value_counts);
                    unrewritable_tool_key = true;
                } else if let Ok(s) = serde_json::to_string(&v) {
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
            walk_value_strings(chain, dir, arguments, counts, !detect_keys);
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
/// Tool-call arguments and replayed `reasoning_content` are covered too
/// (both reach the upstream verbatim through `extra`), and so are
/// unsigned reasoning content parts (a signed one is judged through
/// [`chat_signed_reasoning_texts`] instead and forwarded unchanged).
/// Returns the merged counts (empty = untouched).
///
/// This list and `message_scan_text`'s must stay equal: a slot the scan
/// reads but this does not is a Mask rule that reports a hit and then
/// dispatches the match unmasked.
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
    // Request-level fields are walked BEFORE the conversation, so the
    // conversation's last turn is the last segment: a guardrail that has to
    // bound how much it sends checks the newest turn first and in full.
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
    let window_from = chat_latest_turn_start(&req.messages);
    for (i, msg) in req.messages.iter_mut().enumerate() {
        chain.segment_message_start();
        let dir = Direction::input_window(i >= window_from && !msg.role.is_instruction());
        if detect_keys {
            if let Some(name) = msg.name.as_deref() {
                unrewritable_tool_key |= detect_unrewritable_value_strings(
                    chain,
                    dir,
                    &Value::String(name.to_owned()),
                    &mut counts,
                );
            }
            if let Some(tool_call_id) = msg.tool_call_id.as_deref() {
                offer_call_handle(chain, dir, &Value::String(tool_call_id.to_owned()));
            }
        }
        if let Some(content) = msg.content.as_mut() {
            apply_to_string(chain, dir, content, &mut counts);
        }
        if let Some(blocks) = msg.content_blocks.as_mut() {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get_mut("text") {
                        apply_to_value_string(chain, dir, text, &mut counts);
                    }
                }
                // Replayed reasoning written as a content part is masked
                // like `reasoning_content` below — unless a signature
                // covers it: that slot must reach the upstream
                // byte-identical, as a signed Anthropic `thinking` block
                // does (#1104), so it is judged through
                // `chat_signed_reasoning_texts` and never offered here.
                for slot in aisix_gateway::chat::reasoning_part_slots_mut(block) {
                    if !slot.signed {
                        apply_to_value_string(chain, dir, slot.text, &mut counts);
                    }
                }
                for text in aisix_gateway::chat::unnamed_part_strings_mut(block) {
                    apply_to_value_string(chain, dir, text, &mut counts);
                }
            }
        }
        // History-replay tool calls: arguments travel to the upstream
        // verbatim through `extra`, so mask them like fresh content.
        if let Some(tool_calls) = msg.extra.get_mut("tool_calls") {
            unrewritable_tool_key |=
                redact_tool_call_arguments(chain, dir, tool_calls, &mut counts, detect_keys);
        }
        if let Some(function_call) = msg.extra.get_mut("function_call") {
            unrewritable_tool_key |=
                redact_legacy_function_call(chain, dir, function_call, &mut counts, detect_keys);
        }
        if let Some(refusal) = msg.extra.get_mut("refusal") {
            apply_to_value_string(chain, dir, refusal, &mut counts);
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
                unrewritable_tool_key |=
                    detect_unrewritable_value_strings(chain, dir, transcript, &mut counts);
            } else {
                apply_to_value_string(chain, dir, transcript, &mut counts);
            }
        }
        // Same reason for replayed reasoning: `reasoning_content` is the
        // canonical slot the provider overrides normalise every vendor
        // spelling onto, and it rides `extra` to the upstream untouched.
        // Nothing signs it on this wire, so masking it is safe.
        if let Some(reasoning @ Value::String(_)) = msg.extra.get_mut("reasoning_content") {
            apply_to_value_string(chain, dir, reasoning, &mut counts);
        }
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

/// The segment pass over the chat request walker, including the signed
/// reasoning parts that walk never offers ([`chat_signed_reasoning_texts`]).
pub async fn moderate_chat_format_structured(
    chain: &dyn Guardrail,
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    req: &mut ChatFormat,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    let scan_only = chat_signed_reasoning_texts(req);
    moderate_body_scanning(
        chain,
        Direction::Input,
        model,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        scan_only,
        |guardrail| redact_chat_format_structured(guardrail, req),
    )
    .await
}

/// The inspection request for an endpoint whose whole input is new content —
/// an embeddings `input` list, a rerank `query` and its `documents`, a legacy
/// completions request, a speech request, repeated transcription `prompt`
/// fields: the non-empty `parts` joined by newlines into ONE user message, or
/// no message when every part is empty.
///
/// A guardrail that bounds how much it sends checks the newest message in full
/// and older ones only while its budget lasts; a part kept as a message of its
/// own would count as older than the last one and could go unchecked. Every
/// guardrail that joins messages joins them with a newline too, so the text it
/// scans is the same either way.
pub(crate) fn newest_content_for_inspection<S: AsRef<str>>(
    model: &str,
    parts: impl IntoIterator<Item = S>,
) -> ChatFormat {
    let text = parts
        .into_iter()
        .filter(|part| !part.as_ref().is_empty())
        .map(|part| part.as_ref().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let messages = if text.is_empty() {
        Vec::new()
    } else {
        vec![aisix_gateway::ChatMessage::user(text)]
    };
    ChatFormat::new(model, messages)
}

/// Clone a chat request for non-segment input inspection and append tool
/// declarations as prompt text. The original request remains the rewrite
/// target for the structured moderation pass.
pub fn chat_request_for_inspection(req: &ChatFormat) -> ChatFormat {
    let mut inspection = req.clone();
    // The extra surfaces go BEFORE the conversation, so its last message
    // stays last: a guardrail that has to bound how much it sends checks
    // the newest turn first and in full, and request-level fields such as
    // `tools` or `metadata` must not take that place.
    let mut preamble = Vec::new();
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
            preamble.push(aisix_gateway::ChatMessage::user(text.to_owned()));
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
                preamble.push(aisix_gateway::ChatMessage::user(text));
            }
        }
    }
    inspection.messages.splice(0..0, preamble);
    inspection
}

/// Index of the first message inside the latest-turn window: one past the
/// last assistant message, or 0 when the request carries none.
///
/// The wire-level half of `aisix_guardrails::latest_turn_view`, which
/// answers the same question for the CHECK pass over the parsed
/// `ChatFormat`. Both are pinned per protocol by the e2e cases — this one
/// has to exist separately because the mask walkers rewrite raw slots and
/// have no parsed view to index against.
fn chat_latest_turn_start(messages: &[aisix_gateway::ChatMessage]) -> usize {
    let answered = messages
        .iter()
        .rposition(|m| !m.role.is_instruction())
        .unwrap_or(0);
    messages[..answered]
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .map_or(0, |i| i + 1)
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
            if let Some(id) = tc.get("id") {
                offer_call_handle(chain, dir, id);
            }
            unrewritable_tool_key |=
                detect_named_structural_fields(chain, dir, tc, &["type"], counts);
            if let Some(function) = tc.get("function") {
                unrewritable_tool_key |=
                    detect_named_structural_fields(chain, dir, function, &["name"], counts);
            }
        } else {
            offer_tool_name(chain, dir, tc.get("function").and_then(|f| f.get("name")));
        }
        // A custom tool's call: a name like a function's, and free-form text
        // rather than a JSON-encoded argument object.
        if detect_keys {
            if let Some(custom) = tc.get("custom") {
                unrewritable_tool_key |=
                    detect_named_structural_fields(chain, dir, custom, &["name"], counts);
            }
        } else {
            offer_tool_name(chain, dir, tc.get("custom").and_then(|c| c.get("name")));
        }
        if let Some(input) = tc.get_mut("custom").and_then(|c| c.get_mut("input")) {
            if input.is_string() {
                apply_to_value_string(chain, dir, input, counts);
            } else {
                unrewritable_tool_key |=
                    redact_tool_arguments_value(chain, dir, input, counts, detect_keys);
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
    } else {
        offer_tool_name(chain, dir, function_call.get("name"));
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

impl From<RedactionCounts> for AnthropicRequestRedaction {
    fn from(counts: RedactionCounts) -> Self {
        Self {
            counts,
            unrewritable_tool_key: false,
        }
    }
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
    // Request-level fields are walked BEFORE the conversation, so the
    // conversation's last turn is the last segment: a guardrail that has to
    // bound how much it sends checks the newest turn first and in full.
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
    // The top-level `system` prompt is never in the window.
    if let Some(system) = body.get_mut("system") {
        chain.segment_message_start();
        unrewritable_tool_key |=
            redact_anthropic_content(chain, Direction::InputHistory, system, &mut counts);
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        let window_from = anthropic_latest_turn_start(messages);
        for (i, msg) in messages.iter_mut().enumerate() {
            chain.segment_message_start();
            // A `role: "system"` entry is not in the Anthropic spec but
            // Claude Code sends it (#597); it is a system message wherever
            // it sits, so it stays out of the window like the field above.
            let is_system = msg.get("role").and_then(Value::as_str) == Some("system");
            let dir = Direction::input_window(i >= window_from && !is_system);
            if let Some(content) = msg.get_mut("content") {
                unrewritable_tool_key |= redact_anthropic_content(chain, dir, content, &mut counts);
            }
        }
    }
    AnthropicRequestRedaction {
        counts,
        unrewritable_tool_key,
    }
}

/// Index of the first `messages[]` entry inside the latest-turn window on
/// the Anthropic wire: one past the last `role: "assistant"` entry.
///
/// A tool-loop turn is an assistant message carrying `tool_use` blocks,
/// so the boundary lands where the parsed view puts it — the `tool_result`
/// blocks answering it ride the FINAL user message, which is in the window
/// whole. A TRAILING assistant message is a prefill and never closes the
/// window; see `aisix_guardrails::latest_turn_view`.
fn anthropic_latest_turn_start(messages: &[Value]) -> usize {
    let answered = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) != Some("system"))
        .unwrap_or(0);
    messages[..answered]
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map_or(0, |i| i + 1)
}

pub fn unrewritable_tool_key_verdict() -> aisix_guardrails::GuardrailVerdict {
    aisix_guardrails::GuardrailVerdict::block(
        "data-loss prevention matched a tool structural field that cannot be safely rewritten",
    )
}

/// The segment pass over the Anthropic request walker, including the signed
/// reasoning blocks that walk never offers ([`anthropic_signed_reasoning_texts`]).
pub async fn moderate_anthropic_request(
    chain: &dyn Guardrail,
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    let scan_only = anthropic_signed_reasoning_texts(body);
    moderate_body_scanning(
        chain,
        Direction::Input,
        model,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        scan_only,
        |guardrail| redact_anthropic_request(guardrail, body),
    )
    .await
}

/// Normalize an Anthropic request for input inspection and append its raw tool
/// declarations as one synthetic message. The provider adapter intentionally
/// keeps tools in `ChatFormat::extra`, while guardrails inspect messages; this
/// projection closes that otherwise invisible provider-forwarded text surface.
pub fn anthropic_request_for_inspection(body: &Value) -> Result<aisix_gateway::ChatFormat, ()> {
    let mut chat =
        aisix_provider_anthropic::parse_inbound_request_for_scan(body).map_err(|_| ())?;
    // Ahead of the conversation, so its last message stays last (see
    // `chat_request_for_inspection`).
    let mut preamble = Vec::new();
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for content in messages.iter().filter_map(|message| message.get("content")) {
            let Some(additional) = anthropic_additional_content_for_inspection(content) else {
                continue;
            };
            let text = anthropic_content_inspection_text_capped(&additional, usize::MAX).0;
            if !text.is_empty() {
                preamble.push(aisix_gateway::ChatMessage::user(text));
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
                preamble.push(aisix_gateway::ChatMessage::user(text));
            }
        }
    }
    chat.messages.splice(0..0, preamble);
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
                if key_context == AnthropicKeyContext::Protocol && is_call_handle_field(field) {
                    offer_call_handle(chain, dir, child);
                    continue;
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
        // A number in a caller-shaped object cannot be rewritten without
        // breaking the document, so it is judged and never rewritten.
        Value::Number(n) if key_context != AnthropicKeyContext::Protocol => {
            offer_unrewritable(
                chain,
                dir,
                &n.to_string(),
                aisix_guardrails::SegmentRole::Label,
            );
            false
        }
        _ => false,
    }
}

/// Anthropic `content` is either a bare string or an array of typed blocks.
/// Every official text-bearing request/result block is walked; opaque image,
/// PDF, signature, and encrypted-thinking payloads are deliberately skipped.
///
/// **Deliberate exception to "no write-back channel means block".** A
/// `thinking` / `redacted_thinking` block is signed by the provider, so it
/// can be neither rewritten (the signature would no longer verify and the
/// upstream rejects the replayed turn) nor, per the ruling on #1104,
/// blocked. A mask-action hit inside one is FORWARDED UNCHANGED, for every
/// guardrail kind — including the kinds that otherwise turn an unmaskable
/// result into a blocking verdict.
///
/// Why refusing it buys nothing: the same bytes already left the gateway.
/// Generated reasoning is out of the OUTPUT scope by design, so this block
/// is one the gateway itself handed to the client, unmasked, on the
/// previous turn. Blocking the replay protects nothing that was not already
/// disclosed, and it denies service permanently, because a client following
/// Anthropic's extended-thinking protocol re-sends the block every turn.
///
/// BLOCK-action rules are unaffected and still fire on thinking content —
/// the scan reads it (`parse_inbound_request_for_scan`); only the mask has
/// nowhere to write.
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
    for text in collector.take_rewritable_texts() {
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

/// The text of every signed reasoning content part in a chat request, for
/// SCAN-ONLY submission to the segment pass ([`moderate_body_scanning`]) —
/// the chat-wire counterpart of [`anthropic_signed_reasoning_texts`], and
/// the slots [`redact_chat_format`] leaves unoffered.
///
/// Each text carries whether its message sits in the latest-turn window,
/// judged as [`redact_chat_format`] judges the slots it does offer.
pub fn chat_signed_reasoning_texts(req: &ChatFormat) -> Vec<(String, bool)> {
    let window_from = chat_latest_turn_start(&req.messages);
    req.messages
        .iter()
        .enumerate()
        .flat_map(|(i, m)| {
            let in_turn = i >= window_from && !m.role.is_instruction();
            m.content_blocks
                .iter()
                .flatten()
                .flat_map(aisix_gateway::chat::reasoning_part_slots)
                .filter(|slot| slot.signed && !slot.text.is_empty())
                .map(move |slot| (slot.text.to_owned(), in_turn))
        })
        .collect()
}

/// The plaintext of every signed reasoning block in an Anthropic request,
/// for SCAN-ONLY submission to the segment pass
/// ([`moderate_body_scanning`]).
///
/// Deliberately not part of `redact_anthropic_content`'s walk: that walk is
/// also the write-back path, and a signed `thinking` block must be
/// forwarded byte-identical (#1104). Collecting it here is what lets a
/// segment-moderating kind — `bedrock`, `presidio`, `lakera`,
/// `aliyun_ai_guardrail`, `custom` — judge the text without being handed a
/// slot it could rewrite.
///
/// `redacted_thinking` carries only provider ciphertext under `data`, so
/// there is no plaintext in it to screen.
///
/// Every text is marked outside the latest-turn window, as it always has
/// been on this route.
pub fn anthropic_signed_reasoning_texts(body: &Value) -> Vec<(String, bool)> {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter()
                .filter_map(|m| m.get("content").and_then(Value::as_array))
                .flatten()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("thinking"))
                .filter_map(|b| b.get("thinking").and_then(Value::as_str))
                .filter(|t| !t.is_empty())
                .map(|t| (t.to_owned(), false))
                .collect()
        })
        .unwrap_or_default()
}

/// Mask an Anthropic-native `/v1/messages` RESPONSE body in place (the
/// non-streaming passthrough JSON): top-level `content` blocks (`text` +
/// `tool_use` input, and the server-tool blocks: the same walk as the request
/// side). The block-only kinds that read the serialized content in
/// `messages.rs` see those blocks too.
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
    moderate_body(
        chain,
        Direction::Output,
        None,
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
    let mut unrewritable_tool_key = false;
    // Request-level fields are walked BEFORE the conversation, so the
    // conversation's last turn is the last segment: a guardrail that has to
    // bound how much it sends checks the newest turn first and in full.
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
    // `instructions` is this API's system prompt — never in the window.
    if let Some(instructions) = body.get_mut("instructions") {
        chain.segment_message_start();
        apply_to_value_string(chain, Direction::InputHistory, instructions, &mut counts);
    }
    match body.get_mut("input") {
        // A bare-string `input` is the whole current turn.
        Some(v @ Value::String(_)) => {
            chain.segment_message_start();
            apply_to_value_string(chain, Direction::Input, v, &mut counts)
        }
        Some(Value::Array(items)) => {
            let window_from = responses_latest_turn_start(items);
            for (i, item) in items.iter_mut().enumerate() {
                chain.segment_message_start();
                let dir =
                    Direction::input_window(i >= window_from && !responses_item_is_system(item));
                unrewritable_tool_key |=
                    redact_responses_item_structured(chain, dir, item, &mut counts, detect_keys);
            }
        }
        _ => {}
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
    for text in collector.take_rewritable_texts() {
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
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_body(
        chain,
        Direction::Input,
        model,
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
            // An `output_text` part's token list repeats its text one token at
            // a time, where a mask of each token misses a span split across
            // them. The part loses the list when its text was rewritten.
            let text_before = map
                .contains_key("logprobs")
                .then(|| map.get("text").cloned())
                .flatten();
            for (field, child) in map.iter_mut() {
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
                        if is_call_handle_field(field) {
                            offer_call_handle(chain, dir, child);
                        } else {
                            unrewritable |=
                                detect_unrewritable_value_strings(chain, dir, child, counts);
                        }
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
            if text_before.is_some_and(|before| map.get("text") != Some(&before)) {
                map.remove("logprobs");
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
#[cfg(test)]
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
    for text in collector.take_rewritable_texts() {
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
    for text in collector.take_rewritable_texts() {
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
    walk_value_strings(chain, dir, value, counts, !detect_keys);
    unrewritable
}

/// Index of the first `input[]` item inside the latest-turn window on the
/// Responses wire: one past the last assistant-side item.
///
/// Assistant-side is broader than `role: "assistant"` here, because this
/// API spells a model turn as a typed item with no role at all — see
/// [`responses_item_is_assistant`].
fn responses_latest_turn_start(items: &[Value]) -> usize {
    let answered = items
        .iter()
        .rposition(|i| !responses_item_is_system(i))
        .unwrap_or(0);
    items[..answered]
        .iter()
        .rposition(responses_item_is_assistant)
        .map_or(0, |i| i + 1)
}

/// Whether one `input[]` item is something the MODEL produced, and so a
/// boundary for the latest-turn window.
///
/// This API does not spell a model turn as a role: a tool call the model
/// asked for is a bare `function_call` item, and so is a `reasoning`
/// item. A tool RESULT is the caller answering that call and is not a
/// boundary.
///
/// Asks `responses::responses_item_role` rather than re-deriving the
/// answer, so this protocol has ONE role mapping and not two. Two of them
/// disagreed on `{"role": "user", "type": "reasoning"}` — nothing sends
/// that, but the whole point of the design is that the check pass and
/// these walkers cannot drift, and a second copy is where drift comes
/// from.
fn responses_item_is_assistant(item: &Value) -> bool {
    crate::responses::responses_item_role(item) == Role::Assistant
}

/// Whether one `input[]` item is a system-side message. `developer` is
/// this API's spelling of the same thing.
fn responses_item_is_system(item: &Value) -> bool {
    crate::responses::responses_item_role(item) == Role::System
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
    // Reasoning the model GENERATES is out of output-guardrail scope, so an
    // output walk leaves the item byte-identical. A reasoning item replayed on
    // the REQUEST is in scope: its text carries no provider signature, so
    // rewriting it is not replay-breaking.
    //
    // NOT closed: a reasoning item's `encrypted_content` is forwarded verbatim
    // and is neither scanned nor rewritten, so on a request that replays one
    // the masked `text` is a readable copy while the provider still receives
    // the original. Masking it is not possible (it is provider ciphertext).
    if !dir.is_input() && item_type.as_deref() == Some("reasoning") {
        return false;
    }
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
/// arguments) — the same surface the output check scans. Generated
/// reasoning stays out of it.
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
    moderate_body(
        chain,
        Direction::Output,
        None,
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
    // The whole request is the newest message: the prompt (every item of a
    // prompt list), then `suffix` and `user`, both small enough to cover with
    // it.
    chain.segment_message_start();
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
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_body(
        chain,
        Direction::Input,
        model,
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
    // The whole request is the newest message: the query and every document
    // are new content.
    chain.segment_message_start();
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
                    walk_value_strings(chain, Direction::Input, doc, &mut counts, false);
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
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_body(
        chain,
        Direction::Input,
        model,
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
    // The prompt and `user` are one newest message.
    chain.segment_message_start();
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
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_body(
        chain,
        Direction::Input,
        model,
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
    // `input` and `instructions` are one newest message.
    chain.segment_message_start();
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
    model: Option<&str>,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    body: &mut Value,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_body(
        chain,
        Direction::Input,
        model,
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
    moderate_body(
        chain,
        Direction::Output,
        None,
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
        // The tokens of the transcript as it was, which no mask can rewrite
        // and leave meaningful (see [`masked_spans`]).
        if let Some(json) = json.as_object_mut() {
            json.remove("logprobs");
        }
        return serde_json::to_vec(&json).ok().map(|b| (b, counts));
    }
    let text = std::str::from_utf8(body).ok()?;
    let r = chain.redact_output_text(text)?;
    Some((r.text.into_bytes(), r.counts))
}

/// Mask a streamed transcription (`stream: true`) held back for an output
/// guardrail, frame by frame, on the decoded text of each event: a JSON
/// escape (`@`) hides a match from the raw SSE bytes. The deltas of
/// `transcript.text.delta` are one channel, rewritten as one text on its
/// first frame (a match can span deltas); `transcript.text.done` and
/// `transcript.text.segment` carry complete texts and are masked in place;
/// an event of any other type has each of its string values but `type`
/// masked, and a `data:` payload that is not JSON (but the `[DONE]`
/// sentinel) is masked as plain text.
/// Once anything was masked, every frame loses its `logprobs`: their tokens
/// spell the transcript as it was, and the deltas' and the done event's
/// repeat one text, so a frame left with them gives the mask away. Everything
/// else in a frame (usage, ids) is left as written, and a stream nothing
/// matched in goes out byte for byte.
///
/// Also the scan surface: [`transcription_sse_text`] reads what this walk
/// offers, so a slot the scan reads is a slot the mask rewrites.
/// <https://platform.openai.com/docs/api-reference/audio/transcript-text-delta-event>
pub fn redact_transcription_sse(
    chain: &dyn Guardrail,
    raw: &[u8],
) -> Option<(Vec<u8>, RedactionCounts)> {
    if !chain.redacts_output() {
        return None;
    }
    let mut frames = split_sse_frames(raw);
    let mut counts = RedactionCounts::new();
    let event_type = |frame: &SseFrame| {
        frame
            .data
            .as_ref()
            .and_then(|d| d.get("type")?.as_str().map(str::to_owned))
    };

    let deltas: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| event_type(f).as_deref() == Some("transcript.text.delta"))
        .map(|(i, _)| i)
        .collect();
    let joined: String = deltas
        .iter()
        .filter_map(|&i| frames[i].data.as_ref()?.get("delta")?.as_str())
        .collect();
    if !joined.is_empty() {
        if let Some(r) = redact_str(chain, Direction::Output, &joined) {
            let mut first = true;
            for &i in &deltas {
                let frame = &mut frames[i];
                if let Some(slot) = frame.data.as_mut().and_then(|d| d.get_mut("delta")) {
                    if slot.is_string() {
                        *slot = Value::String(if first {
                            first = false;
                            r.text.clone()
                        } else {
                            String::new()
                        });
                        frame.dirty = true;
                    }
                }
            }
            merge_counts(&mut counts, r.counts);
        }
    }

    for frame in &mut frames {
        let kind = event_type(frame);
        match frame.data.as_mut() {
            Some(data) => {
                let before = masked_spans(&counts);
                match kind.as_deref() {
                    Some("transcript.text.delta") => {}
                    Some("transcript.text.done" | "transcript.text.segment") => {
                        if let Some(text) = data.get_mut("text") {
                            apply_to_value_string(chain, Direction::Output, text, &mut counts);
                        }
                    }
                    _ => {
                        if let Some(map) = data.as_object_mut() {
                            for (field, value) in map.iter_mut() {
                                if field != "type" {
                                    walk_value_strings(
                                        chain,
                                        Direction::Output,
                                        value,
                                        &mut counts,
                                        false,
                                    );
                                }
                            }
                        }
                    }
                }
                frame.dirty |= masked_spans(&counts) != before;
            }
            // A payload that is not JSON has no slots to read it by, so the
            // frame is read, and rewritten, as the text it is.
            None if frame_payload(&frame.raw).is_some_and(|p| p.trim() != "[DONE]") => {
                let text = String::from_utf8_lossy(&frame.raw).into_owned();
                if let Some(r) = redact_str(chain, Direction::Output, &text) {
                    frame.raw = r.text.into_bytes();
                    merge_counts(&mut counts, r.counts);
                }
            }
            None => {}
        }
    }

    if counts.is_empty() {
        return None;
    }
    for frame in &mut frames {
        if let Some(data) = frame.data.as_mut().and_then(Value::as_object_mut) {
            frame.dirty |= data.remove("logprobs").is_some();
        }
    }
    let mut out = Vec::with_capacity(raw.len());
    for frame in &frames {
        out.extend_from_slice(&frame.render());
        out.extend_from_slice(frame.term);
    }
    Some((out, counts))
}

/// The text a streamed transcription's output guardrail reads: every slot
/// [`redact_transcription_sse`] offers, decoded, once each (the done event
/// repeats the deltas).
pub fn transcription_sse_text(raw: &[u8]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut texts: Vec<String> = Vec::new();
    for segment in collect_segments(|g| {
        let _ = redact_transcription_sse(g, raw);
    }) {
        if seen.insert(segment.text.clone()) {
            texts.push(segment.text);
        }
    }
    texts.join("\n")
}

/// Mask a legacy `/v1/completions` RESPONSE body in place: `choices[].text`.
pub fn redact_completions_response(chain: &dyn Guardrail, body: &mut Value) -> RedactionCounts {
    let mut counts = RedactionCounts::new();
    if !chain.redacts_output() {
        return counts;
    }
    if let Some(Value::Array(choices)) = body.get_mut("choices") {
        for choice in choices {
            let before = masked_spans(&counts);
            if let Some(text) = choice.get_mut("text") {
                apply_to_value_string(chain, Direction::Output, text, &mut counts);
            }
            if masked_spans(&counts) != before {
                if let Some(choice) = choice.as_object_mut() {
                    choice.remove("logprobs");
                }
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
    let mut unrewritable_tool_key = false;
    for (message, logprobs) in resp.choices_mut() {
        let before = masked_spans(&counts);
        if let Some(content) = message.content.as_mut() {
            apply_to_string(chain, Direction::Output, content, &mut counts);
        } else if let Some(parts) = message.content_blocks.as_mut() {
            for part in parts.iter_mut() {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = part.get_mut("text") {
                        apply_to_value_string(chain, Direction::Output, text, &mut counts);
                    }
                }
            }
        }
        if let Some(refusal) = message.extra.get_mut("refusal") {
            apply_to_value_string(chain, Direction::Output, refusal, &mut counts);
        }
        if let Some(transcript) = message
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
        if let Some(tool_calls) = message.extra.get_mut("tool_calls") {
            unrewritable_tool_key |= redact_tool_call_arguments(
                chain,
                Direction::Output,
                tool_calls,
                &mut counts,
                detect_keys,
            );
        }
        if let Some(function_call) = message.extra.get_mut("function_call") {
            unrewritable_tool_key |= redact_legacy_function_call(
                chain,
                Direction::Output,
                function_call,
                &mut counts,
                detect_keys,
            );
        }
        if masked_spans(&counts) != before {
            *logprobs = None;
        }
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
    moderate_body(
        chain,
        Direction::Output,
        None,
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
/// (ids, usage, finish_reason, reasoning) are untouched, except that a choice
/// with anything rewritten loses its `logprobs` (see [`masked_spans`]).
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
    let mut rewritten_choices: std::collections::BTreeSet<u32> = Default::default();

    // Content channels: each choice streams one assistant message.
    let mut content_channels: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, c) in chunks.iter().enumerate() {
        if c.delta.content.as_deref().is_some_and(|t| !t.is_empty()) {
            content_channels.entry(c.choice_index).or_default().push(i);
        }
    }
    for (&choice, content_sites) in &content_channels {
        let joined: String = content_sites
            .iter()
            .map(|&i| chunks[i].delta.content.as_deref().unwrap_or(""))
            .collect();
        if let Some(r) = chain.redact_output_text(&joined) {
            rewritten_choices.insert(choice);
            let mut first = true;
            for &i in content_sites {
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

    // Refusal channels: a choice streams its refusal text beside its content.
    let mut refusal_channels: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, c) in chunks.iter().enumerate() {
        if c.delta
            .extra
            .get("refusal")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty())
        {
            refusal_channels.entry(c.choice_index).or_default().push(i);
        }
    }
    for (&choice, refusal_sites) in &refusal_channels {
        let joined: String = refusal_sites
            .iter()
            .filter_map(|&i| chunks[i].delta.extra.get("refusal").and_then(Value::as_str))
            .collect();
        if let Some(r) = chain.redact_output_text(&joined) {
            rewritten_choices.insert(choice);
            let mut first = true;
            for &i in refusal_sites {
                let text = if first {
                    first = false;
                    r.text.clone()
                } else {
                    String::new()
                };
                chunks[i]
                    .delta
                    .extra
                    .insert("refusal".into(), Value::String(text));
            }
            merge_counts(&mut counts, r.counts);
        }
    }

    // Transcript channels: judged and never rewritten, as on the buffered path
    // (`redact_chat_response_impl`), since the same words travel in audio the
    // redactor cannot reach. A mask hit therefore refuses.
    let mut transcripts: BTreeMap<u32, String> = BTreeMap::new();
    for c in chunks.iter() {
        if let Some(text) = c
            .delta
            .audio
            .as_ref()
            .and_then(|audio| audio.get("transcript"))
            .and_then(Value::as_str)
        {
            transcripts
                .entry(c.choice_index)
                .or_default()
                .push_str(text);
        }
    }
    for joined in transcripts.into_values() {
        unrewritable_tool_key |= detect_unrewritable_value_strings(
            chain,
            Direction::Output,
            &Value::String(joined),
            &mut counts,
        );
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
                        let before = masked_spans(&counts);
                        unrewritable_tool_key |= redact_tool_arguments_value(
                            chain,
                            Direction::Output,
                            arguments,
                            &mut counts,
                            detect_keys,
                        );
                        if masked_spans(&counts) != before {
                            rewritten_choices.insert(chunk.choice_index);
                        }
                    }
                }
            }
        }
        // Both places a bridge can put the deprecated call: the typed field the
        // OpenAI wire fills, and `extra`, which the legacy-functions reply path
        // writes.
        let legacy = match chunk.delta.function_call.as_mut() {
            Some(call) => Some(call),
            None => chunk.delta.extra.get_mut("function_call"),
        };
        if let Some(function_call) = legacy {
            if function_call.is_null() {
                continue;
            }
            if !function_call.is_object() {
                unrewritable_tool_key |= detect_keys;
                continue;
            }
            if let Some(arguments) = function_call.get_mut("arguments") {
                if !arguments.is_string() {
                    let before = masked_spans(&counts);
                    unrewritable_tool_key |= redact_tool_arguments_value(
                        chain,
                        Direction::Output,
                        arguments,
                        &mut counts,
                        detect_keys,
                    );
                    if masked_spans(&counts) != before {
                        rewritten_choices.insert(chunk.choice_index);
                    }
                }
            }
        }
    }

    // Tool-call channels: fragments carry an `index` discriminator within
    // their choice; the concatenation of each channel's
    // `function.arguments` strings is the complete JSON-encoded argument
    // document. A legacy `delta.function_call` is one more channel per
    // choice.
    // Keyed by (choice, tool-call index — `None` for the legacy call); each
    // site is (chunk, position in its `tool_calls` — `None` likewise).
    type Channels = BTreeMap<(u32, Option<u64>), Vec<(usize, Option<usize>)>>;
    let mut channels = Channels::new();
    // The structural fields of a tool call stream as fragments too, so a match
    // split across chunks is judged joined. Same keys and sites as `channels`.
    type StructuralChannels =
        BTreeMap<(u32, Option<u64>, &'static str), Vec<(usize, Option<usize>)>>;
    let mut structural_channels = StructuralChannels::new();
    // A custom tool's `input`: free-form text in fragments, one channel per
    // call of a choice.
    let mut custom_channels: BTreeMap<(u32, u64), Vec<(usize, usize)>> = BTreeMap::new();
    // What a call's name sits on: `function`, or `custom` on a custom tool's.
    fn callee(tc: &Value) -> Option<&Value> {
        tc.get("function").or_else(|| tc.get("custom"))
    }
    // The deprecated single call: the typed field the OpenAI wire fills, or
    // `extra`, which the legacy-functions reply path writes.
    fn legacy_call(chunk: &ChatChunk) -> Option<&Value> {
        chunk
            .delta
            .function_call
            .as_ref()
            .or_else(|| chunk.delta.extra.get("function_call"))
    }
    for (ci, chunk) in chunks.iter().enumerate() {
        if let Some(tcs) = chunk.delta.tool_calls.as_ref() {
            for (ti, tc) in tcs.iter().enumerate() {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                if detect_keys {
                    for field in ["id", "type", "name"] {
                        let value = match field {
                            "name" => callee(tc).and_then(|callee| callee.get("name")),
                            _ => tc.get(field),
                        };
                        if value
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                        {
                            structural_channels
                                .entry((chunk.choice_index, Some(idx), field))
                                .or_default()
                                .push((ci, Some(ti)));
                        }
                    }
                } else {
                    offer_tool_name(
                        chain,
                        Direction::Output,
                        callee(tc).and_then(|f| f.get("name")),
                    );
                }
                if tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                {
                    channels
                        .entry((chunk.choice_index, Some(idx)))
                        .or_default()
                        .push((ci, Some(ti)));
                }
                if tc
                    .get("custom")
                    .and_then(|c| c.get("input"))
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                {
                    custom_channels
                        .entry((chunk.choice_index, idx))
                        .or_default()
                        .push((ci, ti));
                }
            }
        }
        if let Some(call) = legacy_call(chunk) {
            if detect_keys {
                if call
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !name.is_empty())
                {
                    structural_channels
                        .entry((chunk.choice_index, None, "name"))
                        .or_default()
                        .push((ci, None));
                }
            } else {
                offer_tool_name(chain, Direction::Output, call.get("name"));
            }
            if call
                .get("arguments")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                channels
                    .entry((chunk.choice_index, None))
                    .or_default()
                    .push((ci, None));
            }
        }
    }
    fn function_of(chunk: &ChatChunk, ti: Option<usize>) -> Option<&Value> {
        match ti {
            Some(ti) => callee(chunk.delta.tool_calls.as_ref()?.get(ti)?),
            None => legacy_call(chunk),
        }
    }
    fn arguments_slot(chunk: &mut ChatChunk, ti: Option<usize>) -> Option<&mut Value> {
        let function = match ti {
            Some(ti) => chunk
                .delta
                .tool_calls
                .as_mut()?
                .get_mut(ti)?
                .get_mut("function")?,
            None => match chunk.delta.function_call.as_mut() {
                Some(call) => call,
                None => chunk.delta.extra.get_mut("function_call")?,
            },
        };
        function.get_mut("arguments")
    }
    for ((_, _, field), sites) in &structural_channels {
        let joined: String = sites
            .iter()
            .filter_map(|&(ci, ti)| {
                match (*field, ti) {
                    ("name", ti) => function_of(&chunks[ci], ti)?.get("name"),
                    (_, Some(ti)) => chunks[ci].delta.tool_calls.as_ref()?.get(ti)?.get(*field),
                    (_, None) => None,
                }
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
    for (&(choice, _), sites) in &channels {
        let joined: String = sites
            .iter()
            .filter_map(|&(ci, ti)| {
                function_of(&chunks[ci], ti)?
                    .get("arguments")
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
            rewritten_choices.insert(choice);
            let mut first = true;
            for &(ci, ti) in sites {
                if let Some(args) = arguments_slot(&mut chunks[ci], ti) {
                    *args = Value::String(if first {
                        first = false;
                        rewritten.clone()
                    } else {
                        String::new()
                    });
                }
            }
        }
        merge_counts(&mut counts, local);
    }
    for sites in custom_channels.values() {
        let input_of = |chunk: &ChatChunk, ti: usize| -> Option<String> {
            chunk.delta.tool_calls.as_ref()?.get(ti)?["custom"]["input"]
                .as_str()
                .map(str::to_owned)
        };
        let joined: String = sites
            .iter()
            .filter_map(|&(ci, ti)| input_of(&chunks[ci], ti))
            .collect();
        if let Some(r) = chain.redact_output_text(&joined) {
            let mut first = true;
            for &(ci, ti) in sites {
                if let Some(input) = chunks[ci]
                    .delta
                    .tool_calls
                    .as_mut()
                    .and_then(|calls| calls.get_mut(ti))
                    .and_then(|tc| tc.get_mut("custom"))
                    .and_then(|custom| custom.get_mut("input"))
                {
                    *input = Value::String(if first {
                        first = false;
                        r.text.clone()
                    } else {
                        String::new()
                    });
                }
            }
            merge_counts(&mut counts, r.counts);
        }
    }

    for chunk in chunks.iter_mut() {
        if rewritten_choices.contains(&chunk.choice_index) {
            chunk.logprobs = None;
        }
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
    moderate_body(
        chain,
        Direction::Output,
        None,
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
    /// The blank line that ended this frame, re-emitted as the upstream
    /// wrote it — a masked CRLF document must not come back LF-framed. Empty
    /// on a final frame the upstream never terminated.
    term: &'static [u8],
    dirty: bool,
}

impl SseFrame {
    /// Re-render the frame: the first `data:` line carries the whole
    /// re-serialised payload and the frame's other `data:` lines go with
    /// it; every other line passes through untouched. Collapsing them is
    /// not a loss — the payload is the lines joined with `\n`, and a
    /// compact JSON document holds none, so one line says the same thing.
    fn render(&self) -> Vec<u8> {
        if !self.dirty {
            return self.raw.clone();
        }
        let Some(data) = self.data.as_ref() else {
            return self.raw.clone();
        };
        let raw = &self.raw;
        let mut out = Vec::with_capacity(raw.len());
        // `raw[cursor..]` has not been copied yet.
        let mut cursor = 0usize;
        let mut data_written = false;
        // Where the previous line's content ended — its terminator follows.
        let mut prev_end = 0usize;
        for line in aisix_gateway::sse::lines(raw) {
            let is_data = aisix_gateway::sse::parse_field(&raw[line.start..line.end])
                .is_some_and(|(name, _)| name == b"data");
            if is_data && !data_written {
                // Keep the line's own terminator: the rest of the frame
                // passes through verbatim, so a CRLF (or CR) frame stays one.
                out.extend_from_slice(&raw[cursor..line.start]);
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(serde_json::to_string(data).unwrap_or_default().as_bytes());
                cursor = line.end;
                data_written = true;
            } else if is_data {
                // Drop the line together with the terminator BEFORE it, so
                // its own terminator ends whatever preceded it — and a
                // dropped last line leaves no stray line ending ahead of the
                // separator the caller re-adds.
                out.extend_from_slice(&raw[cursor..prev_end]);
                cursor = line.end;
            }
            prev_end = line.end;
        }
        out.extend_from_slice(&raw[cursor..]);
        out
    }
}

/// Offset and length of the first frame's separator in `raw`: the last
/// line's terminator plus the blank line after it, as the upstream wrote
/// them.
///
/// The ONE place this crate decides where an SSE frame ends. Everything
/// below derives from it, so the frame-based redaction pass and the
/// line-based scan passes cannot end up with two notions of framing —
/// which is exactly how an unterminated final frame slipped past one pass
/// while the other read it (#1091). It is the gateway's shared splitter,
/// the same one every streaming relay drains with, so a frame the relay
/// forwarded is a frame the scans read.
fn frame_terminator(raw: &[u8]) -> Option<(usize, usize)> {
    let (content_end, frame_end) = aisix_gateway::sse::split_frame_end(raw)?;
    Some((content_end, frame_end - content_end))
}

/// The separator `frame_terminator` found, as a `'static` slice: a frame
/// keeps the separator its upstream wrote, so a masked CRLF (or CR)
/// document does not come back LF-framed.
fn separator(bytes: &[u8]) -> &'static [u8] {
    const SEPARATORS: [&[u8]; 11] = [
        b"\n\n",
        b"\r\n\r\n",
        b"\r\r",
        b"\n\r\n",
        b"\r\n\n",
        b"\n\r",
        b"\r\r\n",
        b"\r\n\r",
        b"\n",
        b"\r",
        b"\r\n",
    ];
    SEPARATORS
        .into_iter()
        .find(|s| *s == bytes)
        .unwrap_or(b"\n\n")
}

/// Byte offset just past the LAST complete frame terminator in `raw`
/// (`0` when there is none, `raw.len()` when the body ends on one).
/// `raw[end..]` is therefore the unterminated trailing fragment.
pub(crate) fn last_frame_end(raw: &[u8]) -> usize {
    let mut end = 0usize;
    let mut rest = raw;
    while let Some((pos, len)) = frame_terminator(rest) {
        end += pos + len;
        rest = &rest[pos + len..];
    }
    end
}

/// A frame's `data:` payload, un-parsed (`None` = the frame carries no
/// `data:` line at all — a comment or keepalive).
///
/// A frame may carry SEVERAL `data:` lines, and the payload is all of them
/// joined with `\n`; one leading space after the colon belongs to the
/// framing, not the payload
/// (<https://html.spec.whatwg.org/multipage/server-sent-events.html#event-stream-interpretation>).
/// Reading only the first line truncates the payload silently, which is
/// how such a frame used to be invisible to the scan and rendered down to
/// its first line by the redactor (#1100).
pub(crate) fn frame_payload(frame_raw: &[u8]) -> Option<String> {
    let ranges = data_line_ranges(frame_raw);
    let (first, rest) = ranges.split_first()?;
    let mut payload = String::from_utf8_lossy(&frame_raw[first.clone()]).into_owned();
    for r in rest {
        payload.push('\n');
        payload.push_str(&String::from_utf8_lossy(&frame_raw[r.clone()]));
    }
    Some(payload)
}

/// Each `data:` line's payload, as a byte range into `frame_raw`, in order.
///
/// The same scan [`frame_payload`] joins, kept as offsets for the one
/// consumer that must write BACK into the frame rather than only read it:
/// `model_echo::restamp_sse_frame` splices the caller's alias in place and
/// needs to know which bytes to replace. Two readers of the same framing
/// would be free to disagree about where a payload starts, which is how
/// the restamp came to see only the first line of a multi-line frame
/// (#1105) while every other consumer saw all of it (#1100).
pub(crate) fn data_line_ranges(frame_raw: &[u8]) -> Vec<Range<usize>> {
    aisix_gateway::sse::data_ranges(frame_raw)
}

/// The `data:` payload of every frame in a buffered SSE body, in order,
/// plus a trailing unterminated fragment's if there is one. Frames with no
/// `data:` line are skipped.
///
/// The line-based block scans read this rather than raw lines, so the
/// payload they scan is the payload the redaction pass rewrites — the two
/// cannot disagree about what a frame carries (#1100).
pub(crate) fn sse_frame_payloads(raw: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some((pos, len)) = frame_terminator(rest) {
        if let Some(p) = frame_payload(&rest[..pos]) {
            out.push(p);
        }
        rest = &rest[pos + len..];
    }
    if let Some(p) = frame_payload(rest) {
        out.push(p);
    }
    out
}

/// Whether a payload is one the passes can read: one JSON document, or
/// nothing to read at all (empty, or the `[DONE]` sentinel — both are
/// skipped by every scan).
fn payload_scannable(payload: &str) -> bool {
    let t = payload.trim();
    t.is_empty() || t == "[DONE]" || serde_json::from_str::<Value>(t).is_ok()
}

/// Split a buffered SSE byte stream into frames on the blank-line
/// separator. A final unterminated frame is parsed like any other, with an
/// empty `term`: EOF terminates an SSE event, so forwarding it as opaque bytes
/// would let its payload bypass output inspection. Call [`seal_buffered_sse`]
/// first on anything released to a client, which completes that frame's
/// terminator.
fn split_sse_frames(raw: &[u8]) -> Vec<SseFrame> {
    fn frame(raw: &[u8], term: &'static [u8]) -> SseFrame {
        SseFrame {
            raw: raw.to_vec(),
            data: frame_payload(raw).and_then(|l| serde_json::from_str::<Value>(l.trim()).ok()),
            term,
            dirty: false,
        }
    }
    let mut frames = Vec::new();
    let mut rest = raw;
    while let Some((pos, len)) = frame_terminator(rest) {
        frames.push(frame(&rest[..pos], separator(&rest[pos..pos + len])));
        rest = &rest[pos + len..];
    }
    if !rest.is_empty() {
        frames.push(frame(rest, b""));
    }
    frames
}

/// What [`seal_buffered_sse`] did with a buffered body's trailing bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum SseTailSeal {
    /// The body already ended on a frame terminator; nothing to do.
    Terminated,
    /// A final frame the upstream never terminated, but which parses:
    /// the missing terminator bytes were appended.
    Completed { padded: usize },
    /// A final frame that cannot be parsed, and so cannot be scanned:
    /// it was cut from the buffer.
    Dropped { dropped: usize },
}

/// What [`seal_buffered_sse`] did to a buffered body.
#[derive(Debug)]
pub struct BufferedSseSeal {
    /// The trailing, unterminated fragment's disposition (#1091).
    pub tail: SseTailSeal,
    /// The payloads of terminated frames excised because they are not one
    /// JSON document (#1100) — plain text, or several `data:` lines that
    /// do not join into one.
    ///
    /// They are gone from the buffer, and the caller MUST fold them into
    /// the text it hands the block scan: nothing can mask such a payload,
    /// but a forbidden literal in one still has to block the response.
    pub excised: Vec<String>,
    /// How many bytes that excision removed (log detail).
    pub excised_bytes: usize,
}

impl BufferedSseSeal {
    /// Whether anything was removed from the buffer. A buffer left empty
    /// by a cut has nothing scanned in it and must be refused; one that
    /// was empty to begin with is just an empty upstream response.
    pub fn cut_anything(&self) -> bool {
        matches!(self.tail, SseTailSeal::Dropped { .. }) || !self.excised.is_empty()
    }
}

/// Seal a fully-buffered SSE body so a guardrail scan and the redaction
/// pass read the SAME frames, and so no frame reaches the client whose
/// payload neither of them could parse (#1091, #1100).
///
/// Two passes run over such a buffer and they used to disagree about what
/// it contains: the line-based block scan reads whatever `data:` lines it
/// finds and parses each one, while the frame-based redactor walks
/// terminator-delimited frames. Wherever they disagreed, something
/// unscanned reached the client. Both shapes of disagreement are resolved
/// here, BEFORE either pass runs:
///
/// - **The trailing fragment** (#1091) — a non-conformant upstream can end
///   mid-frame. If it parses, or carries nothing a scan could read (no
///   `data:` line at all, an empty payload, or the `[DONE]` sentinel), the
///   terminator it left off is completed and it becomes an ordinary frame
///   to both passes. If it does not, it is cut.
/// - **A terminated frame whose payload is not one JSON document** (#1100)
///   — plain text, or `data:` lines that do not join into one document.
///   Neither pass could read it: the scan `continue`s past a line it
///   cannot parse, and the redactor skips a frame with no parsed payload,
///   so it was released byte-for-byte having been seen by nothing. Nothing
///   can mask such a payload structurally, so it is excised and returned
///   in [`BufferedSseSeal::excised`] for the caller's block scan to read.
///
/// A caller whose buffer is left empty by either cut has nothing scanned
/// at all and must refuse (`unscannable_body`) rather than answer with an
/// empty 200.
///
/// What this does NOT establish, and must not be read as establishing:
/// that everything released was *understood*. The test here is whether a
/// payload PARSES, because that is the most this layer can decide without
/// knowing the route's event vocabulary. A frame carrying valid JSON of a
/// `type` no pass models still travels through unread; that predates #1100
/// and closing it would cut responses that reach clients today, so it is
/// deliberately left (see #1100 for the reasoning). Do not build on a
/// stronger invariant than the one stated above.
///
/// A 200 body that is not SSE at all — what an upstream ignoring
/// `stream: true` returns — used to reach here too, and was released with
/// a `\n\n` appended to it. It no longer does: `/v1/responses` and
/// `/v1/messages` choose their relay branch from the upstream's own
/// content type (`dispatch::upstream_body_is_sse`), so such a body takes
/// the non-streaming buffered scan+mask path instead of this one.
///
/// Note what is NOT cut: a frame carrying several `data:` lines that join
/// into one JSON document is a well-formed frame, and is scanned and
/// masked like any other. The payload is the lines joined with `\n` —
/// reading only the first is the truncation #1100 is about, not a reason
/// to drop the frame.
pub fn seal_buffered_sse(buf: &mut Vec<u8>) -> BufferedSseSeal {
    let tail = seal_sse_tail(buf);
    let (excised, excised_bytes) = excise_unscannable_frames(buf);
    BufferedSseSeal {
        tail,
        excised,
        excised_bytes,
    }
}

/// Resolve the unterminated trailing fragment (#1091). See
/// [`seal_buffered_sse`].
///
/// Pads by what is MISSING, not a fixed `\n\n`: a fragment that already
/// ends in half its terminator needs only the other half, and a full pad
/// would leave a stray blank line in the released bytes. The upstream's own
/// line ending is matched, so a CRLF (or CR) stream stays one.
fn seal_sse_tail(buf: &mut Vec<u8>) -> SseTailSeal {
    let end = last_frame_end(buf);
    if end == buf.len() {
        // A CRLF stream cut between the `\r` and the `\n` of its last blank
        // line: that `\r` already ends the frame, but a client that frames on
        // `\r\n\r\n` alone would never see it end.
        if buf.ends_with(b"\r\n\r") {
            buf.push(b'\n');
            return SseTailSeal::Completed { padded: 1 };
        }
        return SseTailSeal::Terminated;
    }
    // An empty payload and the `[DONE]` sentinel carry no text (the scan
    // passes skip both), so cutting one protects nothing and takes the
    // terminal event the client is waiting for with it.
    let scannable = frame_payload(&buf[end..]).is_none_or(|p| payload_scannable(&p));
    if !scannable {
        let dropped = buf.len() - end;
        buf.truncate(end);
        return SseTailSeal::Dropped { dropped };
    }
    // How much of its terminator the FRAGMENT wrote — read `buf` here and a
    // one-byte fragment borrows the previous frame's terminator to match a
    // longer pattern, so the pad comes up short and the seal leaves a tail
    // behind. A fragment that stopped part-way through its terminator says
    // which style it was writing; one that stopped right after its payload
    // says nothing, so the frames before it are the only evidence.
    let frag = &buf[end..];
    let crlf_stream = buf[..end].ends_with(b"\r\n\r\n");
    let cr_stream = buf[..end].ends_with(b"\r\r");
    let pad: &[u8] = if frag.ends_with(b"\r\n") {
        b"\r\n"
    } else if frag.ends_with(b"\r") && crlf_stream {
        b"\n\r\n"
    } else if frag.ends_with(b"\r") {
        b"\r"
    } else if frag.ends_with(b"\n") {
        b"\n"
    } else if crlf_stream {
        b"\r\n\r\n"
    } else if cr_stream {
        b"\r\r"
    } else {
        b"\n\n"
    };
    buf.extend_from_slice(pad);
    SseTailSeal::Completed { padded: pad.len() }
}

/// Cut every terminated frame whose payload is not one JSON document
/// (#1100), returning those payloads and the bytes removed. See
/// [`seal_buffered_sse`].
///
/// Runs AFTER the tail seal, so a fragment that was padded into a frame is
/// judged by the same rule as the frames before it.
fn excise_unscannable_frames(buf: &mut Vec<u8>) -> (Vec<String>, usize) {
    // A frame with no `data:` line at all is a comment or a keepalive:
    // there is nothing in it for a scan to read and nothing for a mask to
    // rewrite, so it is not a bypass.
    let unscannable =
        |frame: &[u8]| matches!(frame_payload(frame), Some(p) if !payload_scannable(&p));
    // Look before copying. Every buffered response reaches this, and almost
    // none of them has a frame to cut, so the common path must not rebuild
    // the buffer to discover that.
    let mut rest: &[u8] = buf;
    let mut any = false;
    while let Some((pos, len)) = frame_terminator(rest) {
        if unscannable(&rest[..pos]) {
            any = true;
            break;
        }
        rest = &rest[pos + len..];
    }
    if !any {
        return (Vec::new(), 0);
    }

    let mut excised: Vec<String> = Vec::new();
    let mut excised_bytes = 0usize;
    let mut kept: Vec<u8> = Vec::with_capacity(buf.len());
    let mut rest: &[u8] = buf;
    while let Some((pos, len)) = frame_terminator(rest) {
        match frame_payload(&rest[..pos]) {
            Some(p) if !payload_scannable(&p) => {
                excised.push(p);
                excised_bytes += pos + len;
            }
            _ => kept.extend_from_slice(&rest[..pos + len]),
        }
        rest = &rest[pos + len..];
    }
    // Empty after the tail seal, but appending it keeps this function
    // correct on its own terms rather than on its caller's.
    kept.extend_from_slice(rest);
    *buf = kept;
    (excised, excised_bytes)
}

/// Number of buffered bytes through the first complete SSE event, including
/// its blank-line terminator.
pub(crate) fn first_sse_event_end(raw: &[u8]) -> Option<usize> {
    aisix_gateway::sse::find_frame_end(raw)
}

/// One event read for a side channel of a stream (usage, first token, the
/// terminal `[DONE]`), which is stricter than the mask pass: a payload that is
/// not valid UTF-8 JSON is malformed, not read lossily.
struct SseEventRead {
    data: Option<Value>,
    /// A non-empty, non-sentinel data payload that was not valid JSON.
    malformed: bool,
    /// The event's joined data payload is the `[DONE]` sentinel.
    sentinel: bool,
}

fn read_sse_event(frame: &[u8]) -> SseEventRead {
    let fields = aisix_gateway::sse::parse_frame(frame);
    let payload = fields.data.as_deref().unwrap_or_default().trim_ascii();
    let sentinel = payload == b"[DONE]";
    let data = if payload.is_empty() || sentinel {
        None
    } else {
        serde_json::from_slice::<Value>(payload).ok()
    };
    SseEventRead {
        malformed: !payload.is_empty() && !sentinel && data.is_none(),
        data,
        sentinel,
    }
}

/// Parse one buffered event. The bool reports a non-empty data payload that
/// was neither `[DONE]` nor valid JSON.
pub(crate) fn parse_sse_json_event(raw: &[u8]) -> (Option<Value>, bool) {
    let event = read_sse_event(raw);
    (event.data, event.malformed)
}

/// Whether one complete (or EOF-terminated) SSE event carries `[DONE]`.
/// Uses the same line joining, BOM, and whitespace rules as the JSON
/// side-channel parser so terminal detection cannot drift from parsing.
pub(crate) fn is_sse_done_event(raw: &[u8]) -> bool {
    read_sse_event(raw).sentinel
}

/// Parse all JSON data events in a buffered SSE stream, a final unterminated
/// one included. The bool is true when any non-empty data event was not valid
/// JSON.
pub(crate) fn parse_sse_json_stream(raw: &[u8]) -> (Vec<Value>, bool) {
    let mut events = Vec::new();
    let mut malformed = false;
    let mut rest = raw;
    while !rest.is_empty() {
        let (frame, next) = match frame_terminator(rest) {
            Some((pos, len)) => (&rest[..pos], &rest[pos + len..]),
            None => (rest, &[][..]),
        };
        let event = read_sse_event(frame);
        malformed |= event.malformed;
        events.extend(event.data);
        rest = next;
    }
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
    let mut rewritten_choices: Vec<u64> = Vec::new();
    for (&index, sites) in &channels {
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
        rewritten_choices.push(index);
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
    // Every frame of a rewritten choice, not only the ones that carried text.
    for frame in &mut frames {
        let Some(Value::Array(choices)) = frame.data.as_mut().and_then(|d| d.get_mut("choices"))
        else {
            continue;
        };
        for choice in choices {
            let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
            if rewritten_choices.contains(&index) {
                if let Some(choice) = choice.as_object_mut() {
                    frame.dirty |= choice.remove("logprobs").is_some();
                }
            }
        }
    }

    let mut out = Vec::with_capacity(raw.len());
    for frame in &frames {
        out.extend_from_slice(&frame.render());
        out.extend_from_slice(frame.term);
    }
    Some((out, counts))
}

/// Mask a fully-buffered Anthropic-native SSE response (the `/v1/messages`
/// passthrough hold-back). Text deltas are reassembled per content-block
/// `index` (a masked span can cross frame boundaries), masked once, and
/// the full masked text re-emitted on the channel's first frame;
/// `input_json_delta` channels are masked as complete JSON documents — for
/// every block that streams one. `rewritten: None` means nothing was changed, so callers
/// can forward the original bytes byte-identically. Sensitive JSON object
/// keys are reported separately because renaming a tool argument would change
/// its contract; handlers must fail closed instead.
pub struct AnthropicSseRedaction {
    pub rewritten: Option<Vec<u8>>,
    pub counts: RedactionCounts,
    pub unrewritable_tool_key: bool,
}

impl AnthropicSseRedaction {
    /// Write the rewrite into `held` and report what the walk found: the shape
    /// a moderation walk returns.
    pub fn apply_to(self, held: &mut Vec<u8>) -> AnthropicRequestRedaction {
        if let Some(rewritten) = self.rewritten {
            *held = rewritten;
        }
        AnthropicRequestRedaction {
            counts: self.counts,
            unrewritable_tool_key: self.unrewritable_tool_key,
        }
    }
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

    // A text block's opening `text` already went through its channel above,
    // at the head of the joined deltas. The block pass below must not offer it
    // a second time: the collect walk would record it twice, and once the
    // channel rewrite lands in that slot the apply walk sees a text the
    // collect walk never did, which is a drift and refuses the answer.
    let channel_start_frames: std::collections::BTreeSet<usize> = text_channels
        .values()
        .flatten()
        .filter(|(_, site)| matches!(site, Site::BlockStartText))
        .map(|&(fi, _)| fi)
        .collect();

    // A block start may carry complete text-bearing content (tool input,
    // document/search text, or a server-tool result) instead of later deltas.
    // Run the same official-block walker used by non-streaming Messages.
    for (fi, frame) in frames.iter_mut().enumerate() {
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
            // Set aside for the walk and put back in place, so everything
            // else in the block (its citations) is still walked.
            let channel_text = channel_start_frames
                .contains(&fi)
                .then(|| {
                    block
                        .get_mut("text")
                        .map(|text| std::mem::replace(text, Value::String(String::new())))
                })
                .flatten();
            unrewritable_tool_key |= redact_anthropic_value(
                chain,
                Direction::Output,
                block,
                &mut counts,
                AnthropicKeyContext::Protocol,
            );
            if let (Some(text), Some(slot)) = (channel_text, block.get_mut("text")) {
                *slot = text;
            }
            *block != before
        };
        frame.dirty |= changed;
    }

    let rewritten = frames.iter().any(|frame| frame.dirty).then(|| {
        let mut out = Vec::with_capacity(raw.len());
        for frame in &frames {
            out.extend_from_slice(&frame.render());
            out.extend_from_slice(frame.term);
        }
        out
    });
    AnthropicSseRedaction {
        rewritten,
        counts,
        unrewritable_tool_key,
    }
}

#[cfg(test)]
pub async fn moderate_anthropic_sse(
    chain: &dyn Guardrail,
    non_segment_verdict: aisix_guardrails::GuardrailVerdict,
    held: &mut Vec<u8>,
    counts_out: &mut RedactionCounts,
    monitor_hits_out: &mut Vec<aisix_core::models::GuardrailMonitorHit>,
) -> AnthropicModeration {
    moderate_body(
        chain,
        Direction::Output,
        None,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_anthropic_sse(guardrail, held).apply_to(held),
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

/// The concatenated client-visible delta content of a buffered `/v1/responses`
/// SSE stream (channel order). Same capture-rebuild role as
/// [`anthropic_sse_text`].
pub fn responses_sse_text(raw: &[u8]) -> String {
    let frames = split_sse_frames(raw);
    // First-seen channel order (NOT key order): the rebuilt capture must
    // read in the order the client saw the channels emitted.
    let mut channels: Vec<(String, String)> = Vec::new();
    for frame in &frames {
        let Some(data) = frame.data.as_ref() else {
            continue;
        };
        let kind = match data.get("type").and_then(Value::as_str) {
            Some(kind @ ("response.output_text.delta" | "response.refusal.delta")) => kind,
            _ => continue,
        };
        let Some(t) = data.get("delta").and_then(Value::as_str) else {
            continue;
        };
        let key = match data.get("item_id").and_then(Value::as_str) {
            Some(id) => format!(
                "{kind}/{id}/{}",
                data.get("content_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
            None => format!(
                "{kind}/{}/{}",
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
/// are reassembled per channel (`output_text.delta`, `refusal.delta`,
/// `function_call_arguments.delta`, `mcp_call_arguments.delta`,
/// `custom_tool_call_input.delta`, each by item), masked once, and
/// re-emitted on the channel's first frame; the aggregate events carry
/// complete texts and are masked directly — each channel's own `*.done`
/// event by its own arm, and `output_item.done` / `response.completed`
/// (`.incomplete`, `.failed`) through [`redact_responses_item_structured`], so the
/// latter cover the same slots as [`redact_responses_response`] and no
/// more. Both `*.added` and `*.done` aggregate forms are authoritative;
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

/// The same report as the Anthropic stream's.
pub type ResponsesSseRedaction = AnthropicSseRedaction;

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
    let mut args_channels: BTreeMap<String, Vec<usize>> = BTreeMap::new();

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
            Some(ty @ ("response.output_text.delta" | "response.refusal.delta")) => {
                if data
                    .get("delta")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    text_channels
                        .entry(format!("{ty}/{}", channel_key(data)))
                        .or_default()
                        .push(fi);
                }
            }
            // MCP tool calls stream their JSON-encoded arguments on their
            // own event; the scan reads it, so the mask has to (#1027).
            Some(
                "response.function_call_arguments.delta" | "response.mcp_call_arguments.delta",
            ) => {
                if data
                    .get("delta")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    args_channels.entry(channel_key(data)).or_default().push(fi);
                }
            }
            // A custom tool's input is free-form text, not a JSON-encoded
            // argument object, so it masks through the plain text channel
            // rather than the args one. It needs a channel at all because
            // `redact_responses_item_structured` masks the same string on the
            // terminal item: without this the caller is handed the masked
            // text on `output_item.done` and the unmasked text on the
            // delta it assembled the input from.
            Some("response.custom_tool_call_input.delta") => {
                if data
                    .get("delta")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    text_channels.entry(channel_key(data)).or_default().push(fi);
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
            if let Some(data) = frame.data.as_mut().and_then(Value::as_object_mut) {
                data.remove("logprobs");
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
    for sites in args_channels.values() {
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
        unrewritable_tool_key |= redact_json_encoded_structured(
            chain,
            Direction::Output,
            &mut rewritten,
            &mut local,
            detect_keys,
        );
        if !local.is_empty() {
            rewrite_channel(&mut frames, sites, rewritten);
            merge_counts(&mut counts, local);
        }
    }

    // Aggregate events carry complete texts — mask them in place. Their
    // counts are NOT merged into the totals: they duplicate the delta
    // channels' matches (the audit count is per span served, not per
    // wire occurrence). Only count them when the delta channel was absent
    // (e.g. a `.done`-only encoder). A collector is told the same thing,
    // so a monitor preview counts what this pass counts.
    let aggregate = if text_channels.is_empty() && args_channels.is_empty() {
        Direction::Output
    } else {
        Direction::OutputEcho
    };
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
                    apply_to_value_string(chain, aggregate, text, &mut local);
                }
            }
            "response.refusal.done" => {
                if let Some(refusal) = data.get_mut("refusal") {
                    apply_to_value_string(chain, aggregate, refusal, &mut local);
                }
            }
            // The part union also carries `reasoning_text` (a reasoning
            // item's content), which is generated reasoning and out of the
            // output scope. Every other part's text is masked.
            "response.content_part.added" | "response.content_part.done" => {
                if let Some(part) = data.get_mut("part").filter(|part| {
                    part.get("type").and_then(Value::as_str) != Some("reasoning_text")
                }) {
                    for field in ["text", "refusal"] {
                        if let Some(text) = part.get_mut(field) {
                            apply_to_value_string(chain, aggregate, text, &mut local);
                        }
                    }
                    if !local.is_empty() {
                        if let Some(part) = part.as_object_mut() {
                            part.remove("logprobs");
                        }
                    }
                }
            }
            "response.function_call_arguments.done" | "response.mcp_call_arguments.done" => {
                if let Some(Value::String(args)) = data.get_mut("arguments") {
                    let mut owned = std::mem::take(args);
                    unrewritable_tool_key |= redact_json_encoded_structured(
                        chain,
                        aggregate,
                        &mut owned,
                        &mut local,
                        detect_keys,
                    );
                    *args = owned;
                }
            }
            "response.custom_tool_call_input.done" => {
                if let Some(input) = data.get_mut("input") {
                    apply_to_value_string(chain, aggregate, input, &mut local);
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = data.get_mut("item") {
                    unrewritable_tool_key |= redact_responses_item_structured(
                        chain,
                        aggregate,
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
                            aggregate,
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
            // `response.output_text.done` carries the part's token list beside
            // its text.
            if let Some(data) = data.as_object_mut() {
                data.remove("logprobs");
            }
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
            out.extend_from_slice(frame.term);
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
    moderate_body(
        chain,
        Direction::Output,
        None,
        non_segment_verdict,
        counts_out,
        monitor_hits_out,
        |guardrail| redact_responses_sse_structured(guardrail, held).apply_to(held),
    )
    .await
}

/// Token-id input through `chain`'s mask rules: an array whose decoded text a
/// rule rewrites is sent back as the ids of the masked text, in the encoding it
/// was read in, as a string input is sent masked. `decoded` is `arrays`'
/// text, in order.
pub(crate) fn mask_token_ids(
    chain: &aisix_guardrails::GuardrailChain,
    encoding: crate::token_estimate::TokenEncoding,
    arrays: &mut [Vec<u64>],
    decoded: &[String],
    redactions: &mut RedactionCounts,
) {
    if !aisix_guardrails::Guardrail::redacts_input(chain) {
        return;
    }
    for (ids, text) in arrays.iter_mut().zip(decoded) {
        if let Some(r) = aisix_guardrails::Guardrail::redact_input_text(chain, text) {
            *ids = encoding.encode(&r.text);
            merge_counts(redactions, r.counts);
        }
    }
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

    /// Without a repeated key the tree is the one `serde_json` builds, so the
    /// ordinary path is unchanged; with one, every value is kept.
    #[test]
    fn parse_every_value_is_serde_json_until_a_key_repeats() {
        for doc in [
            r#"{"a":1,"b":[1.5,-2,true,null,"s"],"c":{"d":"A"}}"#,
            r#"[{"x":{}},[],"",0,18446744073709551615,-9223372036854775808]"#,
            r#""plain""#,
            "3.25e10",
        ] {
            let (tree, repeated) = parse_every_value(doc).unwrap();
            assert!(!repeated, "{doc}");
            assert_eq!(tree, serde_json::from_str::<Value>(doc).unwrap(), "{doc}");
        }
        let (tree, repeated) =
            parse_every_value(r#"{"x":"a","y":{"z":1,"z":[2]},"x":"b","x":"c"}"#).unwrap();
        assert!(repeated);
        assert_eq!(tree, json!({"x": ["a", "b", "c"], "y": {"z": [1, [2]]}}));
        assert!(parse_every_value(r#"{"x":1} trailing"#).is_err());
    }

    /// A tool call's arguments string reaches the upstream as written, so a
    /// scan has to read every value a repeated key carries: the last-wins
    /// tree reads only the last one and forwards the others unscanned (R-070).
    /// A match there cannot be written back, so it is reported as a refusal,
    /// on every family that carries an encoded arguments string.
    #[test]
    fn a_repeated_argument_key_is_scanned_whole_and_refused_not_forwarded() {
        let chain = both();
        for args in [
            r#"{"x":"alice@example.com","x":"safe"}"#,
            r#"{"x":"safe","x":"alice@example.com"}"#,
            r#"{"x":"alice@example.com","x":"safe"}"#,
            r#"{"x":{"y":"alice@example.com"},"x":"safe"}"#,
        ] {
            let mut chat: ChatFormat = serde_json::from_value(json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": "clean"},
                    {"role": "assistant", "content": null, "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {"name": "send", "arguments": args}
                    }]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
                ]
            }))
            .unwrap();
            let redaction = redact_chat_format_structured(chain.as_ref(), &mut chat);
            assert!(redaction.unrewritable_tool_key, "chat request {args}");
            assert_eq!(
                chat.messages[1].extra["tool_calls"][0]["function"]["arguments"], args,
                "a refused body is left as written"
            );

            let mut body = json!({"input": [
                {"type": "function_call", "call_id": "c", "name": "send", "arguments": args}
            ]});
            let redaction = redact_responses_request_structured(chain.as_ref(), &mut body);
            assert!(redaction.unrewritable_tool_key, "responses request {args}");
            assert_eq!(body["input"][0]["arguments"], args);

            let mut resp = ChatResponse {
                logprobs: None,
                more_choices: Vec::new(),
                id: String::new(),
                model: "m".into(),
                message: ChatMessage::assistant(String::new()),
                finish_reason: aisix_gateway::FinishReason::ToolCalls,
                usage: aisix_gateway::UsageStats::new(0, 0),
                service_tier: None,
            };
            resp.message.extra.insert(
                "tool_calls".into(),
                json!([{"id": "call_1", "type": "function",
                        "function": {"name": "send", "arguments": args}}]),
            );
            let redaction = redact_chat_response_structured(chain.as_ref(), &mut resp);
            assert!(redaction.unrewritable_tool_key, "chat response {args}");
            assert_eq!(
                resp.message.extra["tool_calls"][0]["function"]["arguments"],
                args
            );
        }
    }

    /// Nothing matched: the repeated key is the caller's to send, so the
    /// string goes out byte for byte rather than refused or normalised.
    #[test]
    fn a_clean_repeated_argument_key_goes_out_as_written() {
        let chain = both();
        let args = r#"{"x":"a","x":"b"}"#;
        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "send", "arguments": args}
            }]}]
        }))
        .unwrap();
        let redaction = redact_chat_format_structured(chain.as_ref(), &mut chat);
        assert!(!redaction.unrewritable_tool_key);
        assert!(redaction.counts.is_empty());
        assert_eq!(
            chat.messages[0].extra["tool_calls"][0]["function"]["arguments"],
            args
        );
    }

    /// A block-action rule judges each value a repeated key carries, so the
    /// shadowed one is refused like any other (the local segment pass reads
    /// the same walk).
    #[tokio::test]
    async fn a_block_rule_sees_the_value_a_repeated_key_shadows() {
        let chain: Arc<dyn Guardrail> =
            Arc::new(GuardrailChain::new(vec![Arc::new(PiiGuardrail::new(
                vec![builtin_rule("email", PiiAction::Block).unwrap()],
                aisix_core::models::GuardrailHookPoint::Both,
                262_144,
                false,
            ))]));
        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "send",
                             "arguments": r#"{"x":"alice@example.com","x":"safe"}"#}
            }]}]
        }))
        .unwrap();
        let moderation = moderate_chat_format_structured(
            chain.as_ref(),
            None,
            aisix_guardrails::GuardrailVerdict::Allow,
            &mut chat,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(moderation.verdict.is_block());
    }

    /// `redact_anthropic_sse`, as the rewrite and its counts.
    fn anthropic_sse_rewrite(
        chain: &dyn Guardrail,
        raw: &[u8],
    ) -> Option<(Vec<u8>, RedactionCounts)> {
        let redaction = redact_anthropic_sse(chain, raw);
        redaction
            .rewritten
            .map(|rewritten| (rewritten, redaction.counts))
    }

    /// 批处理行顶层除协议字段外的任何字段都会随文件上传给上游，和 `body`
    /// 一样是要过 mask 的内容。
    #[test]
    fn jsonl_upload_masks_extra_top_level_fields() {
        let chain = both();
        let line = r#"{"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":"m","messages":[]},"note":"alice@example.com"}"#;
        let r = redact_jsonl_upload(chain.as_ref(), line).expect("a JSONL file");
        assert!(!r.unrewritable);
        let sent = &r.text;
        assert!(!sent.contains("alice@example.com"), "{sent}");
    }

    /// 转发的必须是检查过的那一份。重复键时解析器只看得见最后一个值，原样
    /// 转发原始字节会把被遮住的那个 —— 带着 PII —— 一起送出去。
    #[test]
    fn jsonl_upload_never_forwards_bytes_it_did_not_inspect() {
        let chain = both();
        let line = r#"{"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":"m","messages":[{"role":"user","content":"mail alice@example.com"}]},"body":{"model":"m","messages":[{"role":"user","content":"clean"}]}}"#;
        let r = redact_jsonl_upload(chain.as_ref(), line).expect("a JSONL file");
        let sent = &r.text;
        assert!(!sent.contains("alice@example.com"), "{sent}");
    }

    /// 对象的键改写不了：改了键，上游读到的就是另一个字段。键命中 mask 规则时，
    /// 原样转发等于规则白设，所以报告为改写不了，由调用方拒绝上传 —— 和 chat
    /// 对改写不了的键的处理一致。
    #[test]
    fn jsonl_upload_reports_an_object_key_a_mask_rule_would_change() {
        let chain = both();
        for line in [
            r#"{"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":"m","messages":[],"metadata":{"bob@example.com":1}}}"#,
            r#"{"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":"m","messages":[]},"bob@example.com":1}"#,
            r#"{"messages":[{"role":"user","content":"hi","extra":{"bob@example.com":"x"}}]}"#,
            // 协议字段的值不是字符串、而是带键的对象时，键同样会送到上游。
            r#"{"custom_id":{"bob@example.com":1},"method":"POST","url":"/v1/chat/completions","body":{"model":"m","messages":[]}}"#,
            r#"{"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":{"bob@example.com":"m"},"messages":[]}}"#,
            r#"{"custom_id":"r1","method":"POST","url":"/v1/chat/completions","model":[{"bob@example.com":true}],"body":{"messages":[]}}"#,
        ] {
            let r = redact_jsonl_upload(chain.as_ref(), line).expect("a JSONL file");
            assert!(r.unrewritable, "{line}");
        }
        // 键里没有要遮蔽的内容时照常改写，不拒绝。
        let line = r#"{"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":"m","messages":[],"metadata":{"owner":"bob@example.com"}}}"#;
        let r = redact_jsonl_upload(chain.as_ref(), line).expect("a JSONL file");
        assert!(!r.unrewritable);
        assert!(!r.text.contains("bob@example.com"), "{}", r.text);
    }

    /// 文件开头的 UTF-8 BOM 不是第一行 JSON 的一部分：照样逐行按值改写，BOM
    /// 原样留在转发的文件开头。
    #[test]
    fn jsonl_upload_reads_past_a_byte_order_mark_and_keeps_it() {
        let chain = both();
        let text = "\u{feff}{\"custom_id\":\"r1\",\"body\":{\"messages\":[{\"role\":\"user\",\"content\":\"mail bob\\u0040example.com\"}]}}\n";
        let r = redact_jsonl_upload(chain.as_ref(), text).expect("a JSONL file");
        assert!(!r.unrewritable);
        let line = r.text.strip_prefix('\u{feff}').expect("the BOM is kept");
        let value: Value = serde_json::from_str(line.trim()).expect("still JSON");
        assert_eq!(value["custom_id"], "r1");
        assert!(!r.text.contains("bob@example.com"), "{}", r.text);
        assert!(!r.text.contains("bob\\u0040"), "{}", r.text);
    }

    /// 有的行是 JSON 对象、有的行不是的文件，逐行改写不了；当纯文本改写又会改
    /// 到协议字段和键、看不见 JSON 转义写出的内容：报告为改写不了。一行 JSON
    /// 对象都没有的文件才是纯文本。
    #[test]
    fn jsonl_upload_reports_a_file_mixing_objects_with_other_lines() {
        let chain = both();
        let line = r#"{"custom_id":"r1","body":{"messages":[]}}"#;
        for other in ["null", "not json", "[1]", "\"text\""] {
            let r = redact_jsonl_upload(chain.as_ref(), &format!("{line}\n{other}\n"))
                .expect("a file with a JSON object line");
            assert!(r.unrewritable, "{other}");
        }
        assert!(
            redact_jsonl_upload(chain.as_ref(), "plain text\nnull\nalice@example.com\n").is_none()
        );
    }

    /// Every text slot of a rerank or speech request is new content, so the
    /// walk marks the newest message at the first segment: a moderator that
    /// bounds what it sends covers the whole request, not only the last
    /// document or the instructions.
    #[test]
    fn rerank_and_speech_walks_start_the_newest_message_at_the_first_segment() {
        let mut rerank =
            json!({"model": "m", "query": "q", "documents": ["a", {"text": "b"}, "c"]});
        let collector = SegmentCollector::default();
        let _ = redact_rerank_request_structured(&collector, &mut rerank);
        assert_eq!(collector.newest_from(), Some(0), "rerank");

        let mut speech = json!({"model": "m", "input": "say this", "instructions": "softly"});
        let collector = SegmentCollector::default();
        let _ = redact_speech_request_structured(&collector, &mut speech);
        assert_eq!(collector.newest_from(), Some(0), "speech");

        let mut completions = json!({"model": "m", "prompt": ["p1", "p2"], "suffix": "s"});
        let collector = SegmentCollector::default();
        let _ = redact_completions_request_structured(&collector, &mut completions);
        assert_eq!(collector.newest_from(), Some(0), "completions");
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

    // ── latest-turn window, wire level (AISIX-Cloud#1558) ────────────────
    //
    // The mask walkers rewrite raw slots and have no parsed `ChatFormat` to
    // index against, so each wire shape answers the boundary question for
    // itself. These pin the three answers against the one
    // `aisix_guardrails::latest_turn_view` gives the check pass.

    #[test]
    fn chat_window_starts_after_the_last_assistant_message() {
        let req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "s"},
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "fresh"},
                {"role": "tool", "content": "result", "tool_call_id": "c1"},
            ],
        }))
        .unwrap();
        assert_eq!(chat_latest_turn_start(&req.messages), 3);
    }

    /// The wire-level twin of
    /// `chain::tests::a_trailing_assistant_prefill_does_not_close_the_window`.
    #[test]
    fn a_trailing_assistant_message_does_not_close_the_window_on_any_wire() {
        let chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "answered"},
                {"role": "user", "content": "fresh"},
                {"role": "assistant", "content": "Sure, here is"},
            ],
        }))
        .unwrap();
        assert_eq!(chat_latest_turn_start(&chat.messages), 2);

        let anthropic = json!([
            {"role": "user", "content": "old"},
            {"role": "assistant", "content": "answered"},
            {"role": "user", "content": "fresh"},
            {"role": "assistant", "content": "Sure, here is"},
        ]);
        assert_eq!(
            anthropic_latest_turn_start(anthropic.as_array().unwrap()),
            2,
        );

        let responses = json!([
            {"role": "user", "content": "old"},
            {"role": "assistant", "type": "message",
             "content": [{"type": "output_text", "text": "answered"}]},
            {"role": "user", "content": "fresh"},
            {"type": "reasoning", "encrypted_content": "opaque"},
        ]);
        assert_eq!(
            responses_latest_turn_start(responses.as_array().unwrap()),
            2,
        );
    }

    /// The wire-level twin of
    /// `chain::tests::a_system_message_after_a_prefill_does_not_reopen_the_bypass`:
    /// a system message trailing the prefill must not make the prefill
    /// look answered on any of the three shapes.
    #[test]
    fn a_system_message_after_a_prefill_does_not_close_the_window() {
        let chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "secret"},
                {"role": "assistant", "content": "Sure, here is"},
                {"role": "system", "content": "trailing policy"},
            ],
        }))
        .unwrap();
        assert_eq!(chat_latest_turn_start(&chat.messages), 0);

        let chat_developer: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "secret"},
                {"role": "assistant", "content": "Sure, here is"},
                {"role": "developer", "content": "trailing policy"},
            ],
        }))
        .unwrap();
        assert_eq!(chat_latest_turn_start(&chat_developer.messages), 0);

        // The non-spec `role: "system"` entry Claude Code sends (#597).
        let anthropic = json!([
            {"role": "user", "content": "secret"},
            {"role": "assistant", "content": "Sure, here is"},
            {"role": "system", "content": "trailing policy"},
        ]);
        assert_eq!(
            anthropic_latest_turn_start(anthropic.as_array().unwrap()),
            0,
        );

        let responses = json!([
            {"role": "user", "content": "secret"},
            {"role": "assistant", "type": "message",
             "content": [{"type": "output_text", "text": "Sure, here is"}]},
            {"role": "developer", "content": "trailing policy"},
        ]);
        assert_eq!(
            responses_latest_turn_start(responses.as_array().unwrap()),
            0,
        );
    }

    /// A request that is nothing but one assistant prefill still has a
    /// window — the whole request.
    #[test]
    fn a_request_of_one_assistant_message_is_entirely_in_the_window() {
        let chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": "Sure, here is"}],
        }))
        .unwrap();
        assert_eq!(chat_latest_turn_start(&chat.messages), 0);
    }

    #[test]
    fn chat_window_with_no_assistant_message_is_the_whole_request() {
        let req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "s"},
                {"role": "user", "content": "only"},
            ],
        }))
        .unwrap();
        assert_eq!(chat_latest_turn_start(&req.messages), 0);
    }

    /// An Anthropic tool loop: the assistant turn carrying `tool_use` is
    /// the boundary, and the `tool_result` answering it rides the final
    /// user message, which is inside the window whole.
    #[test]
    fn anthropic_window_starts_after_the_tool_use_turn() {
        let messages = json!([
            {"role": "user", "content": "old"},
            {"role": "assistant", "content": [{"type": "text", "text": "reply"}]},
            {"role": "user", "content": "call it"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "result"}
            ]},
        ]);
        assert_eq!(anthropic_latest_turn_start(messages.as_array().unwrap()), 4,);
    }

    /// The Responses API spells a model turn as a typed item with no role
    /// at all, so `role: "assistant"` alone would put a whole agent tool
    /// loop inside the window.
    #[test]
    fn responses_window_treats_typed_model_items_as_the_boundary() {
        let items = json!([
            {"role": "user", "content": "old"},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "result"},
        ]);
        // The tool RESULT is the caller answering, not a boundary — the
        // window opens right after the call.
        assert_eq!(responses_latest_turn_start(items.as_array().unwrap()), 2);
    }

    #[test]
    fn responses_window_opens_after_a_replayed_assistant_message() {
        let items = json!([
            {"role": "user", "content": "old"},
            {"role": "assistant", "type": "message",
             "content": [{"type": "output_text", "text": "reply"}]},
            {"role": "user", "content": "fresh"},
        ]);
        assert_eq!(responses_latest_turn_start(items.as_array().unwrap()), 2);
    }

    /// The window is what a `latest_turn` row reads; a row left on `all`
    /// must keep rewriting the history, so the walkers cannot simply skip
    /// out-of-window slots.
    #[test]
    fn an_all_row_still_masks_history_after_the_window_split() {
        let chain = both();
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "old alice@example.com"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "fresh bob@example.com"},
            ],
        }))
        .unwrap();
        let counts = redact_chat_format(chain.as_ref(), &mut req);
        assert_eq!(counts.get("email"), Some(&2), "{counts:?}");
        assert!(!req.messages[0].content_str().contains("alice@example.com"));
    }

    /// `message_scan_text` reads `extra["reasoning_content"]`, so this
    /// must rewrite it or a Mask rule reports a hit on reasoning it then
    /// forwards to the upstream verbatim. Nothing on the OpenAI wire signs
    /// this slot, so rewriting it is safe — unlike the Anthropic `thinking`
    /// block below.
    #[test]
    fn chat_format_masks_replayed_reasoning_content() {
        let chain = both();
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "who?"},
                {
                    "role": "assistant",
                    "content": "checking",
                    "reasoning_content": "the user meant a@x.com I think"
                }
            ]
        }))
        .unwrap();
        let counts = redact_chat_format(chain.as_ref(), &mut req);
        assert_eq!(
            req.messages[1]
                .extra
                .get("reasoning_content")
                .and_then(Value::as_str),
            Some("the user meant [EMAIL_REDACTED] I think"),
        );
        assert!(!counts.is_empty());
    }

    /// Reasoning content parts are masked like `reasoning_content` when
    /// unsigned, and left byte-identical (but handed to the scan-only
    /// tail) when a signature covers them.
    #[test]
    fn chat_format_masks_unsigned_reasoning_parts_only() {
        let chain = both();
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "who?"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": [{"type": "text", "text": "mistral a@x.com"}]},
                        {"type": "thinking", "signature": "sig", "thinking": [{"type": "text", "text": "signed b@x.com"}]},
                        {"type": "thinking", "thinking": "anthropic e@x.com"},
                        {"type": "thinking", "thinking": "signed f@x.com", "signature": "sig"},
                        {"type": "reasoning", "summary": [
                            {"type": "summary_text", "text": "unsigned c@x.com", "signature": ""},
                            {"type": "summary_text", "text": "signed d@x.com", "signature": "sig"}
                        ]},
                        {"type": "text", "text": "answer"}
                    ]
                },
                {"role": "user", "content": "go on"}
            ]
        }))
        .unwrap();
        assert_eq!(
            chat_signed_reasoning_texts(&req),
            vec![
                ("signed b@x.com".to_string(), false),
                ("signed f@x.com".to_string(), false),
                ("signed d@x.com".to_string(), false)
            ],
        );
        let counts = redact_chat_format(chain.as_ref(), &mut req);
        assert!(!counts.is_empty());
        let blocks = req.messages[1].content_blocks.as_ref().unwrap();
        assert_eq!(blocks[0]["thinking"][0]["text"], "mistral [EMAIL_REDACTED]");
        assert_eq!(blocks[1]["thinking"][0]["text"], "signed b@x.com");
        assert_eq!(blocks[2]["thinking"], "anthropic [EMAIL_REDACTED]");
        assert_eq!(blocks[3]["thinking"], "signed f@x.com");
        assert_eq!(blocks[4]["summary"][0]["text"], "unsigned [EMAIL_REDACTED]");
        assert_eq!(blocks[4]["summary"][1]["text"], "signed d@x.com");
    }

    /// C1: the `/v1/responses` scan reads a `reasoning` item's
    /// `content[].text` AND `summary[].text`, so the request mask has to
    /// reach both — otherwise a Mask rule matching there reported a hit,
    /// returned Allow, and dispatched the body with the match intact.
    #[test]
    fn responses_request_masks_reasoning_content_and_summary() {
        let chain = both();
        let mut body = json!({
            "model": "m",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "ask a@x.com"}],
                    "content": [{"type": "reasoning_text", "text": "or call 13800138000"}]
                }
            ]
        });
        let counts = redact_responses_request(chain.as_ref(), &mut body);
        let item = &body["input"][1];
        assert_eq!(
            item["summary"][0]["text"].as_str(),
            Some("ask [EMAIL_REDACTED]")
        );
        assert_eq!(
            item["content"][0]["text"].as_str(),
            Some("or call [CHINA_MOBILE_REDACTED]")
        );
        assert!(!counts.is_empty());
    }

    /// The mirror of the test above, and the reason the arm is gated on
    /// `Direction::Input`: reasoning the model GENERATED is out of the
    /// output-guardrail scope, so the response mask must leave a
    /// `reasoning` item in `output[]` byte-identical.
    #[test]
    fn responses_response_leaves_generated_reasoning_alone() {
        let chain = both();
        let mut body = json!({
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "ask a@x.com"}],
                    "content": [{"type": "reasoning_text", "text": "or call 13800138000"}]
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "reply to b@y.org"}]
                }
            ]
        });
        let before = body["output"][0].clone();
        redact_responses_response(chain.as_ref(), &mut body);
        assert_eq!(
            body["output"][0], before,
            "generated reasoning is untouched"
        );
        assert_eq!(
            body["output"][1]["content"][0]["text"].as_str(),
            Some("reply to [EMAIL_REDACTED]"),
            "the message item is still masked",
        );
    }

    /// A mask-action hit inside a signed `thinking` / `redacted_thinking`
    /// block is FORWARDED UNCHANGED — the deliberate exception to "no
    /// write-back channel means block" (#1104). The block is byte-identical
    /// afterwards, and the ordinary block beside it still masks, so this
    /// pins an exception for the signed block rather than an exemption for
    /// the whole message.
    #[test]
    fn signed_reasoning_is_forwarded_unchanged_while_its_neighbours_mask() {
        let chain = both();
        let mut body = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "mail a@x.com", "signature": "sig-abc"},
                    {"type": "redacted_thinking", "data": "opaque a@x.com blob"},
                    {"type": "text", "text": "also mail a@x.com"}
                ]}
            ]
        });
        let before = body["messages"][1]["content"].clone();
        redact_anthropic_request(chain.as_ref(), &mut body);

        assert_eq!(
            body["messages"][1]["content"][0], before[0],
            "the signed thinking block is byte-identical",
        );
        assert_eq!(
            body["messages"][1]["content"][1], before[1],
            "so is the redacted_thinking block",
        );
        assert_eq!(
            body["messages"][1]["content"][2]["text"].as_str(),
            Some("also mail [EMAIL_REDACTED]"),
            "the mask is demonstrably running on this very request",
        );
    }

    /// The exception is for the MASK only. A block-action rule still fires
    /// on thinking content, because the scan reads it — otherwise the
    /// exception would have turned a signed block into a channel any
    /// forbidden text could travel through.
    #[test]
    fn a_block_rule_still_fires_on_thinking_content() {
        use aisix_guardrails::{KeywordBlocklist, KeywordRule};

        let chain: Arc<dyn Guardrail> = Arc::new(GuardrailChain::new(vec![Arc::new(
            KeywordBlocklist::input_only(vec![KeywordRule::literal("FORBIDDENTHOUGHT")]),
        )]));

        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "I will FORBIDDENTHOUGHT now", "signature": "s"}
            ]}]
        });
        let chat = aisix_provider_anthropic::parse_inbound_request_for_scan(&body).unwrap();
        let verdict = futures::executor::block_on(aisix_guardrails::Guardrail::check_input(
            chain.as_ref(),
            &chat,
        ));
        assert!(
            matches!(verdict, aisix_guardrails::GuardrailVerdict::Block { .. }),
            "a block rule must still reach thinking text, got {verdict:?}",
        );
    }

    /// The FIVE segment-moderating kinds — `bedrock`, `presidio`, `lakera`,
    /// `aliyun_ai_guardrail`, `custom` — answer `Allow` from
    /// `check_input_non_segment` by design and take their real verdict from
    /// the segment pass. That pass walks the redactor, which never offers a
    /// signed `thinking` block, so before the scan-only channel existed a
    /// block policy on any of those kinds was bypassable by moving the text
    /// into a thinking block. The keyword test above cannot see this: keyword
    /// is not a segment moderator.
    #[tokio::test]
    async fn a_segment_kind_sees_thinking_text_and_can_block_on_it() {
        use aisix_guardrails::{GuardrailVerdict, SegmentsOutcome};
        use std::sync::Mutex;

        /// Records what the segment pass was offered, and blocks on a
        /// literal — the shape a Presidio/Bedrock/custom block policy has.
        struct RecordingSegmenter {
            seen: Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl Guardrail for RecordingSegmenter {
            fn name(&self) -> &'static str {
                "recording-segmenter"
            }
            fn moderates_segments(&self) -> bool {
                true
            }
            async fn moderate_input_segments(
                &self,
                texts: &[String],
                _newest_from: Option<usize>,
            ) -> SegmentsOutcome {
                *self.seen.lock().unwrap() = texts.to_vec();
                let hit = texts.iter().any(|t| t.contains("FORBIDDENTHOUGHT"));
                SegmentsOutcome {
                    verdict: if hit {
                        GuardrailVerdict::block("segment block".to_string())
                    } else {
                        GuardrailVerdict::Allow
                    },
                    masked: None,
                    counts: Default::default(),
                    monitor_hits: Vec::new(),
                }
            }
        }

        let seg = Arc::new(RecordingSegmenter {
            seen: Mutex::new(Vec::new()),
        });
        let chain: Arc<dyn Guardrail> = Arc::new(GuardrailChain::new(vec![seg.clone()]));

        let mut body = json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "benign question"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "I will FORBIDDENTHOUGHT now", "signature": "s"}
                ]}
            ]
        });
        let before = body.clone();

        let mut counts = RedactionCounts::new();
        let mut hits = Vec::new();
        let verdict = moderate_body_scanning(
            chain.as_ref(),
            Direction::Input,
            None,
            GuardrailVerdict::Allow,
            &mut counts,
            &mut hits,
            anthropic_signed_reasoning_texts(&body),
            |g| redact_anthropic_request(g, &mut body),
        )
        .await
        .verdict;

        let seen = seg.seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|t| t.contains("FORBIDDENTHOUGHT")),
            "the segment pass must be offered the thinking text, got {seen:?}",
        );
        assert!(
            matches!(verdict, GuardrailVerdict::Block { .. }),
            "a segment kind's block rule must fire on thinking content, got {verdict:?}",
        );
        assert_eq!(body, before, "and the body is never rewritten");
    }

    /// The scan-only text is judged but never written back: a segment kind
    /// that masks every slot it was offered must not have that mask land on
    /// the signed block, and must still land on the ordinary ones. (What
    /// enforces that is the walk not offering the block — see the note on
    /// `masked.truncate`, which is about log truthfulness, not this.)
    #[tokio::test]
    async fn a_segment_masks_ordinary_slots_but_never_the_signed_block() {
        use aisix_guardrails::{GuardrailVerdict, SegmentsOutcome};

        struct MaskEverything;
        #[async_trait::async_trait]
        impl Guardrail for MaskEverything {
            fn name(&self) -> &'static str {
                "mask-everything"
            }
            fn moderates_segments(&self) -> bool {
                true
            }
            async fn moderate_input_segments(
                &self,
                texts: &[String],
                _newest_from: Option<usize>,
            ) -> SegmentsOutcome {
                SegmentsOutcome {
                    verdict: GuardrailVerdict::Allow,
                    masked: Some(texts.iter().map(|_| "MASKED".to_owned()).collect()),
                    counts: std::collections::BTreeMap::from([("X".to_owned(), 1)]),
                    monitor_hits: Vec::new(),
                }
            }
        }

        let chain: Arc<dyn Guardrail> =
            Arc::new(GuardrailChain::new(vec![Arc::new(MaskEverything)]));
        let mut body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret chain", "signature": "s"},
                {"type": "text", "text": "ordinary text"}
            ]}]
        });

        let mut counts = RedactionCounts::new();
        let mut hits = Vec::new();
        moderate_body_scanning(
            chain.as_ref(),
            Direction::Input,
            None,
            GuardrailVerdict::Allow,
            &mut counts,
            &mut hits,
            anthropic_signed_reasoning_texts(&body),
            |g| redact_anthropic_request(g, &mut body),
        )
        .await;

        let blocks = &body["messages"][0]["content"];
        assert_eq!(
            blocks[0]["thinking"].as_str(),
            Some("secret chain"),
            "the signed block is byte-identical even under a mask-everything segmenter",
        );
        assert_eq!(
            blocks[1]["text"].as_str(),
            Some("MASKED"),
            "…while the ordinary slot beside it is rewritten",
        );
    }

    #[test]
    fn a_text_block_array_message_counts_each_entity_once() {
        // The text of a block array is held once, in its blocks, so a
        // number in it is masked — and counted — once.
        let chain = both();
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "call 13800138000"},
                {"type": "image_url", "image_url": {"url": "http://x"}}
            ]}]
        }))
        .unwrap();
        let counts = redact_chat_format(chain.as_ref(), &mut req);
        assert_eq!(counts.get("china_mobile"), Some(&1));
        assert_eq!(
            req.messages[0].content_str(),
            "call [CHINA_MOBILE_REDACTED]"
        );
    }

    #[test]
    fn a_text_only_bridge_reads_the_blocks_masked_one_by_one_then_joined() {
        // A bridge that reads only the message text sees the blocks as the
        // block-forwarding bridges send them: each masked on its own, then
        // joined. A number split across two blocks is therefore not a match
        // on either path.
        let chain = both();
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "mail a@x.com "},
                {"type": "text", "text": "call 138001"},
                {"type": "text", "text": "38000"}
            ]}]
        }))
        .unwrap();
        let counts = redact_chat_format(chain.as_ref(), &mut req);
        assert_eq!(counts.get("email"), Some(&1));
        assert_eq!(counts.get("china_mobile"), None);
        assert_eq!(
            req.messages[0].content_str(),
            "mail [EMAIL_REDACTED] call 13800138000"
        );
    }

    /// A moderator that bounds what it sends covers `newest_from..` in full.
    /// The scan-only texts go ahead of the walked slots, so a long thinking
    /// block replayed from an old turn is not part of that range, and a label
    /// does not shift it: the index counts the slots the moderator is handed.
    #[tokio::test]
    async fn newest_from_names_the_newest_message_past_scan_only_and_label_slots() {
        use aisix_guardrails::SegmentsOutcome;
        use std::sync::Mutex;

        struct Recorder {
            seen: Mutex<Option<(Vec<String>, Option<usize>)>>,
        }
        #[async_trait::async_trait]
        impl Guardrail for Recorder {
            fn name(&self) -> &'static str {
                "recorder"
            }
            fn moderates_segments(&self) -> bool {
                true
            }
            async fn moderate_input_segments(
                &self,
                texts: &[String],
                newest_from: Option<usize>,
            ) -> SegmentsOutcome {
                *self.seen.lock().unwrap() = Some((texts.to_vec(), newest_from));
                SegmentsOutcome::allow()
            }
        }

        let recorder = Arc::new(Recorder {
            seen: Mutex::new(None),
        });
        let chain = GuardrailChain::new(vec![recorder.clone()]);
        let mut body = json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "old question"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "long reasoning", "signature": "s"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {"floor": 2}},
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "new question"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": "new result"},
                ]},
            ],
        });
        let mut counts = RedactionCounts::new();
        moderate_anthropic_request(
            &chain,
            None,
            GuardrailVerdict::Allow,
            &mut body,
            &mut counts,
            &mut Vec::new(),
        )
        .await;

        let (texts, newest) = recorder.seen.lock().unwrap().take().expect("pass ran");
        let newest = newest.expect("the walk marks messages");
        assert_eq!(texts[0], "long reasoning");
        assert_eq!(texts[newest], "new question", "{texts:?}");
        assert!(
            texts[newest..].contains(&"new result".to_owned()),
            "{texts:?}"
        );
        assert!(!texts[newest..].contains(&"long reasoning".to_owned()));
        assert!(!texts[newest..].contains(&"old question".to_owned()));
    }

    /// Each masked reply lands on its own slot past the scan-only head, and
    /// nothing lands on the signed block.
    #[tokio::test]
    async fn each_mask_lands_on_its_own_slot_past_the_scan_only_head() {
        use aisix_guardrails::SegmentsOutcome;

        struct MaskEach;
        #[async_trait::async_trait]
        impl Guardrail for MaskEach {
            fn name(&self) -> &'static str {
                "mask-each"
            }
            fn moderates_segments(&self) -> bool {
                true
            }
            async fn moderate_input_segments(
                &self,
                texts: &[String],
                _newest_from: Option<usize>,
            ) -> SegmentsOutcome {
                SegmentsOutcome {
                    verdict: GuardrailVerdict::Allow,
                    masked: Some(texts.iter().map(|t| format!("M:{t}")).collect()),
                    counts: std::collections::BTreeMap::from([("X".to_owned(), 1)]),
                    monitor_hits: Vec::new(),
                }
            }
        }

        let chain = GuardrailChain::new(vec![Arc::new(MaskEach)]);
        let mut body = json!({
            "model": "claude",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "signed", "signature": "s"},
                    {"type": "text", "text": "first"},
                ]},
                {"role": "user", "content": "second"},
            ],
        });
        let mut counts = RedactionCounts::new();
        let moderation = moderate_anthropic_request(
            &chain,
            None,
            GuardrailVerdict::Allow,
            &mut body,
            &mut counts,
            &mut Vec::new(),
        )
        .await;

        assert!(!moderation.verdict.is_block(), "{:?}", moderation.verdict);
        assert_eq!(body["messages"][0]["content"][0]["thinking"], "signed");
        assert_eq!(body["messages"][0]["content"][1]["text"], "M:first");
        assert_eq!(body["messages"][1]["content"], "M:second");
    }

    /// The legacy-functions reply path writes the deprecated call to
    /// `delta.extra`, the OpenAI wire to the typed field. A stream guard has to
    /// read both: its arguments are masked and its name is judged either way.
    #[test]
    fn chat_chunks_read_the_legacy_call_from_either_place() {
        let chain = both();
        let call = |name: &str, arguments: &str| json!({"name": name, "arguments": arguments});
        let chunk = |delta: ChatDelta| ChatChunk {
            id: "c".into(),
            model: "m".into(),
            choice_index: 0,
            delta,
            finish_reason: None,
            logprobs: None,
            usage: None,
            service_tier: None,
            accounting_usage: None,
        };
        let in_extra = |value: Value| {
            let mut delta = ChatDelta::default();
            delta.extra.insert("function_call".into(), value);
            delta
        };
        let typed = |value: Value| ChatDelta {
            function_call: Some(value),
            ..ChatDelta::default()
        };
        let deltas: [&dyn Fn(Value) -> ChatDelta; 2] = [&in_extra, &typed];
        for place in deltas {
            let mut chunks = vec![
                chunk(place(call("send", "{\"to\":\"a@"))),
                chunk(place(call("", "x.com\"}"))),
            ];
            let redaction = redact_chat_chunks_structured(chain.as_ref(), &mut chunks);
            assert!(!redaction.unrewritable_tool_key);
            assert_eq!(redaction.counts.get("email"), Some(&1));
            let rendered = serde_json::to_string(&chunks).unwrap();
            assert!(!rendered.contains("a@"), "{rendered}");
            assert!(rendered.contains("[EMAIL_REDACTED]"), "{rendered}");

            let mut named = vec![chunk(place(call("a@x.com", "{}")))];
            assert!(
                redact_chat_chunks_structured(chain.as_ref(), &mut named).unrewritable_tool_key
            );
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
            logprobs: None,
            more_choices: Vec::new(),
            id: "r".into(),
            model: "m".into(),
            message: ChatMessage::assistant("mail a@x.com"),
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
            service_tier: None,
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
            logprobs: None,
            more_choices: Vec::new(),
            id: "r".into(),
            model: "m".into(),
            message: msg,
            finish_reason: aisix_gateway::FinishReason::Stop,
            usage: aisix_gateway::UsageStats::new(0, 0),
            service_tier: None,
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
    fn a_custom_tool_call_input_is_masked_like_any_generated_text() {
        let chain = both();
        let mut msg = ChatMessage::assistant("");
        msg.extra.insert(
            "tool_calls".into(),
            json!([{
                "id": "call_1",
                "type": "custom",
                "custom": {"name": "run", "input": "mail a@x.com"},
            }]),
        );
        let mut resp = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            service_tier: None,
            id: "r".into(),
            model: "m".into(),
            message: msg,
            finish_reason: aisix_gateway::FinishReason::ToolCalls,
            usage: aisix_gateway::UsageStats::new(0, 0),
        };
        let redaction = redact_chat_response_structured(chain.as_ref(), &mut resp);
        assert!(!redaction.unrewritable_tool_key);
        assert_eq!(
            resp.message.extra["tool_calls"][0]["custom"]["input"],
            "mail [EMAIL_REDACTED]"
        );
    }

    /// Keyword and PII rows judge only what the walk offers, so a custom
    /// tool's fields have to be among it for a block rule to read them.
    #[tokio::test]
    async fn a_block_rule_reads_a_custom_tool_call_on_a_buffered_answer() {
        let chain: Arc<dyn Guardrail> = Arc::new(GuardrailChain::new(vec![Arc::new(
            aisix_guardrails::KeywordBlocklist::output_only(vec![
                aisix_guardrails::KeywordRule::literal("SECRET"),
            ]),
        )
            as Arc<dyn Guardrail>]));
        for call in [
            json!({"id": "c", "type": "custom", "custom": {"name": "run", "input": "the SECRET"}}),
            json!({"id": "c", "type": "custom", "custom": {"name": "SECRET", "input": "x"}}),
        ] {
            let mut msg = ChatMessage::assistant("");
            msg.extra.insert("tool_calls".into(), json!([call]));
            let mut resp = ChatResponse {
                logprobs: None,
                more_choices: Vec::new(),
                service_tier: None,
                id: "r".into(),
                model: "m".into(),
                message: msg,
                finish_reason: aisix_gateway::FinishReason::ToolCalls,
                usage: aisix_gateway::UsageStats::new(0, 0),
            };
            let (verdict, _) = chain.check_output_non_segment_observed(&resp).await;
            let mut counts = RedactionCounts::new();
            let mut hits = Vec::new();
            let moderation = moderate_chat_response_structured(
                chain.as_ref(),
                verdict,
                &mut resp,
                &mut counts,
                &mut hits,
            )
            .await;
            assert!(moderation.verdict.is_block());
        }
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
            logprobs: None,
            more_choices: Vec::new(),
            service_tier: None,
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
            logprobs: None,
            more_choices: Vec::new(),
            service_tier: None,
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
            logprobs: None,
            more_choices: Vec::new(),
            service_tier: None,
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

    /// A tool-call handle is not content: the remote pass never reads one, on
    /// any family, while a local rule still judges it. The call's name and
    /// arguments stay on the slots they were on.
    #[test]
    fn call_handles_reach_the_local_pass_and_not_the_remote_one() {
        use aisix_guardrails::SegmentRole;
        const HANDLE: &str = "HANDLE__thought__SIGNATURE";
        let judged = |segments: Vec<aisix_guardrails::ScanSegment>, what: &str| {
            let handles: Vec<_> = segments
                .iter()
                .filter(|s| s.text.contains(HANDLE))
                .collect();
            assert!(!handles.is_empty(), "{what}: no local rule saw the handle");
            for handle in handles {
                assert_eq!(handle.role, SegmentRole::Label, "{what}: {}", handle.text);
            }
            assert!(
                segments
                    .iter()
                    .any(|s| reaches_remote_pass(s.role) && s.text.contains("ARGTEXT")),
                "{what}: the arguments must still reach the remote pass"
            );
        };

        let mut chat: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": HANDLE, "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"ARGTEXT\"}"}
                }]},
                {"role": "tool", "tool_call_id": HANDLE, "content": "result"}
            ]
        }))
        .unwrap();
        judged(
            collect_segments(|g| {
                let _ = redact_chat_format_structured(g, &mut chat);
            }),
            "chat",
        );

        let mut anthropic = json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": [{
                    "type": "tool_use", "id": HANDLE, "name": "lookup",
                    "input": {"q": "ARGTEXT"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": HANDLE, "content": "result"
                }]}
            ]
        });
        judged(
            collect_segments(|g| {
                let _ = redact_anthropic_request(g, &mut anthropic);
            }),
            "anthropic",
        );

        let mut responses = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "id": HANDLE, "call_id": HANDLE,
                 "name": "lookup", "arguments": "{\"q\":\"ARGTEXT\"}"},
                {"type": "function_call_output", "call_id": HANDLE, "output": "result"},
                {"type": "mcp_approval_response", "approval_request_id": HANDLE, "approve": true}
            ]
        });
        judged(
            collect_segments(|g| {
                let _ = redact_responses_request_structured(g, &mut responses);
            }),
            "responses",
        );
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
            logprobs: None,
            more_choices: Vec::new(),
            service_tier: None,
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
                choice_index: 0,
                logprobs: None,
                service_tier: None,
                accounting_usage: None,
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
                choice_index: 0,
                logprobs: None,
                service_tier: None,
                accounting_usage: None,
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

    /// 收集一份载荷里所有仍然原样含 `mark` 的位置。
    fn unmasked_positions(v: &serde_json::Value, mark: &str) -> Vec<String> {
        fn walk(v: &serde_json::Value, path: String, mark: &str, out: &mut Vec<String>) {
            match v {
                serde_json::Value::String(s) if s.contains(mark) => out.push(path),
                serde_json::Value::Object(m) => {
                    for (k, vv) in m {
                        walk(vv, format!("{path}.{k}"), mark, out);
                    }
                }
                serde_json::Value::Array(a) => {
                    for (i, vv) in a.iter().enumerate() {
                        walk(vv, format!("{path}[{i}]"), mark, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = vec![];
        walk(v, String::from("$"), mark, &mut out);
        out.sort();
        out
    }

    /// walker 是「默认遮蔽 + 显式豁免」：任何字符串都会被遮，除非命中
    /// `opaque` 那几条判定。这个方向是对的——新加的字段自动受保护——
    /// 代价是**豁免表变成了唯一的泄露面**，而往里加一个名字是一行改动，
    /// 看起来无害，效果是那个字段从此原样送到上游和全量内容导出器。
    ///
    /// 逐字段的测试证明的是「这个字段被遮了」，看不住豁免表。这条测试
    /// 反过来钉住整个面：在每一种合法文本位置埋同一个值，跑完脱敏之后，
    /// **仍然原样幸存的位置必须恰好等于下面这份清单**。豁免面一旦变宽，
    /// 多出来的位置会直接出现在失败信息里；反过来，把某处刻意跳过改成
    /// 遮蔽（会损坏签名或二进制载荷）也同样会被发现。
    #[test]
    fn anthropic_request_masks_every_text_position_but_the_listed_ones() {
        let chain = both();
        let mark = "probe@example.com";
        let mut request = json!({
            "system": mark,
            "messages": [
                {"role": "user", "content": mark},
                {"role": "user", "content": [
                    {"type": "text", "text": mark},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": mark}},
                    {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": mark},
                     "title": mark, "context": mark},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {"q": mark}},
                    {"type": "tool_result", "tool_use_id": "t1", "content": mark},
                    {"type": "tool_result", "tool_use_id": "t2", "content": [{"type": "text", "text": mark}]},
                    {"type": "thinking", "thinking": mark, "signature": "sig"},
                    {"type": "redacted_thinking", "data": mark},
                    {"type": "search_result", "title": mark, "content": [{"type": "text", "text": mark}]},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": mark}]}
            ],
            "tools": [
                {"name": "f", "description": mark,
                 "input_schema": {"type": "object", "properties": {"q": {"type": "string", "description": mark}}}}
            ],
        });

        let out = redact_anthropic_request(chain.as_ref(), &mut request);
        assert!(
            !out.unrewritable_tool_key,
            "这份载荷里没有改写不了的结构字段"
        );

        assert_eq!(
            unmasked_positions(&request, mark),
            vec![
                // 二进制载荷：base64 图片。遮它会毁掉图片，而它也不是文本。
                // 注意同一层的 document `source.data` 是 `type: text`，被遮了
                // —— 区分的是来源类型，不是字段名。
                "$.messages[1].content[1].source.data",
                // 加密的思考载荷，本来就不可读。
                "$.messages[1].content[7].data",
                // 扩展思考块带签名，必须原样回传，改一个字符上游就会拒收。
                "$.messages[1].content[6].thinking",
            ]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
            "幸存位置和预期不符 —— 要么新增的内容形状没走 walker（漏遮），\
             要么刻意跳过的位置被改成了遮蔽（会损坏签名/二进制载荷）",
        );
    }

    /// 同一条性质，打到 Responses API 的形状。它和 Anthropic 是两套独立的
    /// walker，各有各的豁免表（`RESPONSES_OPAQUE_FIELDS` 对
    /// `ANTHROPIC_METADATA_FIELDS` 那组判定），所以必须各自钉住 —— 这个
    /// 仓库反复栽在「一个家族修了、另一个家族静默保持原状」上。
    ///
    /// 注意工具定义走的是另一条路径（`redact_tool_definitions`），不受
    /// `RESPONSES_OPAQUE_FIELDS` 管辖；这里把它一并埋了值，是为了它哪天
    /// 停止遮蔽时也能在这条测试里露出来。
    #[test]
    fn responses_request_masks_every_text_position_but_the_listed_ones() {
        let chain = both();
        let mark = "probe@example.com";
        let mut request = json!({
            "model": "gpt-4o",
            "instructions": mark,
            "input": [
                {"role": "user", "content": mark},
                {"role": "user", "content": [
                    {"type": "input_text", "text": mark},
                    {"type": "input_image", "image_url": mark},
                    {"type": "input_file", "filename": mark, "file_data": mark},
                ]},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": mark},
                {"type": "function_call_output", "call_id": "c1", "output": mark},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": mark}]},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": mark}]},
            ],
            "tools": [
                {"type": "function", "name": "f", "description": mark,
                 "parameters": {"type": "object", "properties": {"q": {"type": "string", "description": mark}}}}
            ],
            "text": {"format": {"type": "json_schema", "name": "s", "description": mark,
                                 "schema": {"type": "object"}}},
        });

        let out = redact_responses_request_structured(chain.as_ref(), &mut request);
        assert!(!out.unrewritable_tool_key);

        assert_eq!(
            unmasked_positions(&request, mark),
            vec![
                // 内联文件内容是 base64，遮了就损坏；同一个块里的
                // `filename` 是文本，被遮了。
                "$.input[1].content[2].file_data",
                // 图片地址：可能是 data: URI（二进制），也可能是外链 URL，
                // 两种改了都会指向别处或损坏。
                "$.input[1].content[1].image_url",
            ]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
            "幸存位置和预期不符",
        );
    }

    /// 调用方自定义的标识字段（Anthropic 的 `metadata.user_id`、OpenAI 家族
    /// 的 `metadata.*`）改写不了 —— 改了就换了一个人。所以规则命中时唯一
    /// 安全的处置是拦下整个请求，而不是原样放行。
    ///
    /// 三个家族必须给出同一个结论：同一条规则在不同端点上得出不同的安全
    /// 判断，是这个仓库最贵的一类 bug。
    #[test]
    fn a_matched_caller_identifier_blocks_on_every_family() {
        let chain = both();
        let mark = "probe@example.com";

        let mut anthropic = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": mark},
        });
        assert!(
            redact_anthropic_request(chain.as_ref(), &mut anthropic).unrewritable_tool_key,
            "/v1/messages 放行了一个改写不了的标识字段",
        );

        let mut responses = json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "hi"}],
            "metadata": {"trace": mark},
        });
        assert!(
            redact_responses_request_structured(chain.as_ref(), &mut responses)
                .unrewritable_tool_key,
            "/v1/responses 放行了一个改写不了的标识字段",
        );

        let mut chat: aisix_gateway::ChatFormat = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"trace": mark},
        }))
        .expect("chat body");
        assert!(
            redact_chat_format_structured(chain.as_ref(), &mut chat).unrewritable_tool_key,
            "/v1/chat/completions 放行了一个改写不了的标识字段",
        );
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
        let segments = collector.take_rewritable_texts();
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
            .iter()
            .any(|m| m.content_str().contains(sensitive_key)));
        // The tools go ahead of the conversation, whose last turn stays last.
        assert_eq!(inspection.messages.last().unwrap().content_str(), "clean");
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
            choice_index: 0,
            logprobs: None,
            id: "c".into(),
            model: "m".into(),
            delta: ChatDelta {
                content: Some(text.to_string()),
                ..ChatDelta::default()
            },
            finish_reason: None,
            usage: None,
            service_tier: None,
            accounting_usage: None,
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
                choice_index: 0,
                logprobs: None,
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
                service_tier: None,
                accounting_usage: None,
            },
            ChatChunk {
                choice_index: 0,
                logprobs: None,
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
                service_tier: None,
                accounting_usage: None,
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
                choice_index: 0,
                logprobs: None,
                service_tier: None,
                accounting_usage: None,
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
                choice_index: 0,
                logprobs: None,
                service_tier: None,
                accounting_usage: None,
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
            choice_index: 0,
            logprobs: None,
            service_tier: None,
            accounting_usage: None,
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
                choice_index: 0,
                logprobs: None,
                service_tier: None,
                accounting_usage: None,
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
                choice_index: 0,
                logprobs: None,
                service_tier: None,
                accounting_usage: None,
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

    /// What the scan reads of a content part, the mask reads too: a match it
    /// cannot rewrite would be forwarded as written.
    #[test]
    fn a_mask_reaches_every_part_the_scan_reads() {
        let chain = both();
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "refusal", "refusal": "mail a@x.com"},
                ]},
                {"role": "user", "content": [
                    {"type": "input_text", "text": "mail b@x.com"},
                    {"type": "text", "text": ["mail c@x.com"]},
                    {"type": "image_url", "image_url": {"url": "https://x/d@x.com"}},
                ]},
            ],
        }))
        .unwrap();
        let counts = redact_chat_format(chain.as_ref(), &mut req);
        assert_eq!(counts.get("email"), Some(&3));
        let wire = serde_json::to_string(&req.messages).unwrap();
        for address in ["a@x.com", "b@x.com", "c@x.com"] {
            assert!(!wire.contains(address), "{address}: {wire}");
        }
        assert!(wire.contains("d@x.com"), "media is opaque: {wire}");
        assert!(wire.contains("\"type\":\"input_text\""), "{wire}");
    }

    fn logprobs_of(token: &str) -> Value {
        json!({"content": [{"token": token, "logprob": -0.1, "bytes": null, "top_logprobs": []}]})
    }

    /// A choice's tokens are its text in the clear, and a mask cannot rewrite
    /// them. A choice nothing was masked in keeps them: the caller asked.
    #[test]
    fn a_masked_chat_choice_loses_its_logprobs_and_only_that_choice() {
        let chain = both();
        let mut resp: ChatResponse = serde_json::from_value(json!({
            "id": "c", "model": "m", "finish_reason": "stop",
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            "message": {"role": "assistant", "content": "mail a@x.com"},
            "logprobs": logprobs_of("a@x.com"),
            "more_choices": [
                {"index": 1, "finish_reason": "stop", "logprobs": logprobs_of("clean"),
                 "message": {"role": "assistant", "content": "clean"}},
                {"index": 2, "finish_reason": "stop", "logprobs": logprobs_of("b@x.com"),
                 "message": {"role": "assistant", "content": "mail b@x.com"}},
            ],
        }))
        .unwrap();
        redact_chat_response(chain.as_ref(), &mut resp);
        assert_eq!(
            resp.message.content.as_deref(),
            Some("mail [EMAIL_REDACTED]")
        );
        assert_eq!(resp.logprobs, None);
        assert_eq!(resp.more_choices[0].logprobs, Some(logprobs_of("clean")));
        assert_eq!(resp.more_choices[1].logprobs, None);
    }

    /// A segment moderator rewrites through the apply walk, whose rewrite is the
    /// same "something changed" as a local one.
    #[tokio::test]
    async fn a_segment_mask_drops_the_choice_logprobs_too() {
        let chain = seg_chain(StubSegments::masker());
        let mut resp: ChatResponse = serde_json::from_value(json!({
            "id": "c", "model": "m", "finish_reason": "stop",
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            "message": {"role": "assistant", "content": "secret"},
            "logprobs": logprobs_of("secret"),
        }))
        .unwrap();
        let moderation = moderate_chat_response_structured(
            &chain,
            GuardrailVerdict::Allow,
            &mut resp,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(!moderation.verdict.is_block(), "{:?}", moderation.verdict);
        assert_eq!(resp.message.content.as_deref(), Some("<M0:SECRET>"));
        assert_eq!(resp.logprobs, None);
    }

    #[test]
    fn a_masked_stream_choice_loses_its_logprobs_on_every_frame() {
        let chain = both();
        let chunk = |choice: u32, text: &str, token: Option<&str>| -> ChatChunk {
            serde_json::from_value(json!({
                "id": "c",
                "model": "m",
                "choice_index": choice,
                "delta": {"content": text},
                "logprobs": token.map(logprobs_of),
            }))
            .unwrap()
        };
        let mut chunks = vec![
            chunk(0, "mail a@", Some("a@")),
            chunk(1, "clean", Some("clean")),
            chunk(0, "x.com", Some("x.com")),
        ];
        redact_chat_chunks(chain.as_ref(), &mut chunks);
        assert_eq!(
            chunks[0].delta.content.as_deref(),
            Some("mail [EMAIL_REDACTED]")
        );
        assert_eq!(chunks[0].logprobs, None);
        assert_eq!(chunks[2].logprobs, None);
        assert_eq!(chunks[1].logprobs, Some(logprobs_of("clean")));
    }

    #[test]
    fn a_masked_completion_choice_loses_its_logprobs() {
        let chain = both();
        let mut body = json!({"choices": [
            {"index": 0, "text": "mail a@x.com", "logprobs": {"tokens": ["a@x.com"]}},
            {"index": 1, "text": "clean", "logprobs": {"tokens": ["clean"]}},
        ]});
        redact_completions_response(chain.as_ref(), &mut body);
        assert_eq!(body["choices"][0]["text"], "mail [EMAIL_REDACTED]");
        assert!(body["choices"][0].get("logprobs").is_none(), "{body}");
        assert_eq!(body["choices"][1]["logprobs"]["tokens"][0], "clean");

        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"text\":\"mail a@\",\"logprobs\":{\"tokens\":[\"a@\"]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"text\":\"x.com\",\"logprobs\":{\"tokens\":[\"x.com\"]}}]}\n\n",
            "data: {\"choices\":[{\"index\":1,\"text\":\"clean\",\"logprobs\":{\"tokens\":[\"clean\"]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"text\":\"\",\"logprobs\":{\"tokens\":[\"x.com\"]}}]}\n\n",
        );
        let (out, _) = redact_completions_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@"), "{out}");
        assert!(!out.contains("x.com"), "{out}");
        assert!(out.contains("\"tokens\":[\"clean\"]"), "{out}");
    }

    /// One token per string, so a span split across tokens is in no single one
    /// of them: the rewrite of the part's text is what says the list must go.
    #[test]
    fn a_masked_responses_part_loses_its_logprobs() {
        let chain = both();
        let split = json!([{"token": "a@", "bytes": [97, 64]}, {"token": "x.com", "bytes": [120]}]);
        let mut body = json!({"output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "a@x.com", "logprobs": split},
            {"type": "output_text", "text": "clean", "logprobs": [{"token": "clean"}]},
        ]}]});
        redact_responses_response(chain.as_ref(), &mut body);
        let parts = &body["output"][0]["content"];
        assert_eq!(parts[0]["text"], "[EMAIL_REDACTED]");
        assert!(parts[0].get("logprobs").is_none(), "{body}");
        assert_eq!(parts[1]["logprobs"][0]["token"], "clean");

        let raw = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"content_index\":0,\"delta\":\"a@\",\"logprobs\":[{\"token\":\"a@\"}]}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"content_index\":0,\"delta\":\"x.com\",\"logprobs\":[{\"token\":\"x.com\"}]}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"item_id\":\"m\",\"content_index\":0,\"text\":\"a@x.com\",\"logprobs\":[{\"token\":\"a@\"},{\"token\":\"x.com\"}]}\n\n",
            "data: {\"type\":\"response.content_part.done\",\"item_id\":\"m\",\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"a@x.com\",\"logprobs\":[{\"token\":\"a@\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"a@x.com\",\"logprobs\":[{\"token\":\"a@\"}]}]}]}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"n\",\"content_index\":0,\"delta\":\"clean\",\"logprobs\":[{\"token\":\"clean\"}]}\n\n",
        );
        let (out, _) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@"), "{out}");
        assert!(!out.contains("x.com"), "{out}");
        assert!(out.contains("\"token\":\"clean\""), "{out}");
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
    fn responses_sse_masks_refusal_delta_and_aggregate_events() {
        let chain = both();
        let raw = concat!(
            "event: response.refusal.delta\ndata: {\"type\":\"response.refusal.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"mail a@\"}\n\n",
            "event: response.refusal.delta\ndata: {\"type\":\"response.refusal.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"x.com now\"}\n\n",
            "event: response.refusal.done\ndata: {\"type\":\"response.refusal.done\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"refusal\":\"mail a@x.com now\"}\n\n",
            "event: response.content_part.done\ndata: {\"type\":\"response.content_part.done\",\"part\":{\"type\":\"refusal\",\"refusal\":\"mail a@x.com now\"}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"refusal\",\"refusal\":\"mail a@x.com now\"}]}]}}\n\n",
        );
        let (out, counts) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "original must be gone: {out}");
        assert!(
            out.contains("\"delta\":\"mail [EMAIL_REDACTED] now\""),
            "out: {out}"
        );
        assert!(
            out.contains("\"refusal\":\"mail [EMAIL_REDACTED] now\""),
            "out: {out}"
        );
        assert_eq!(
            responses_sse_text(out.as_bytes()),
            "mail [EMAIL_REDACTED] now"
        );
        assert_eq!(counts.get("email"), Some(&1));
    }

    /// #1027: a Responses stream restates each delta channel on its
    /// aggregate events. The mask pass counts a span once, so a collector
    /// must tell the restatements apart — or a monitor preview counts the
    /// same span three times.
    #[test]
    fn responses_sse_aggregates_are_collected_as_echoes() {
        use aisix_guardrails::SegmentRole;
        let raw = concat!(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"mail a@x.com ok\"}\n\n",
            "event: response.output_text.done\ndata: {\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",\"text\":\"mail a@x.com ok\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"mail a@x.com ok\"}]}]}}\n\n",
        );
        let segments = collect_segments(|g| {
            let _ = redact_responses_sse(g, raw.as_bytes());
        });
        let counted: Vec<&str> = segments
            .iter()
            .filter(|s| s.role == SegmentRole::Rewritable)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(counted, vec!["mail a@x.com ok"]);
        assert_eq!(
            segments
                .iter()
                .filter(|s| s.role == SegmentRole::Echo)
                .count(),
            2
        );
    }

    /// #1027: a tool call's arguments are offered value by value, each key
    /// and number as a label, and the tool name as a scan-only slot.
    #[test]
    fn chat_tool_call_arguments_are_offered_as_values_keys_and_a_name() {
        use aisix_guardrails::SegmentRole;
        let mut req: ChatFormat = serde_json::from_value(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "unlock", "arguments": "{\"pin\":\"1234\",\"door\":{\"side\":\"front\",\"floor\":2}}"}
                }]
            }]
        }))
        .unwrap();
        let segments = collect_segments(|g| {
            let _ = redact_chat_format(g, &mut req);
        });
        let got: Vec<(&str, SegmentRole)> =
            segments.iter().map(|s| (s.text.as_str(), s.role)).collect();
        for want in [
            ("unlock", SegmentRole::ScanOnly),
            ("pin", SegmentRole::Label),
            ("1234", SegmentRole::Rewritable),
            ("door", SegmentRole::Label),
            ("side", SegmentRole::Label),
            ("front", SegmentRole::Rewritable),
            ("floor", SegmentRole::Label),
            ("2", SegmentRole::Label),
        ] {
            assert!(got.contains(&want), "missing {want:?} in {got:?}");
        }
        assert_eq!(got.len(), 8, "{got:?}");
    }

    /// The bridged encoder now emits reasoning-summary frames alongside the
    /// message frames it always did. Generated reasoning is out of
    /// output-guardrail scope, so an output mask that rewrites the held
    /// stream has to leave those frames byte-identical while still masking
    /// the message beside them — the mirror of
    /// `responses_response_leaves_generated_reasoning_alone` at the wire
    /// level, which is the layer the hold-back release actually goes through.
    #[test]
    fn responses_sse_leaves_reasoning_summary_frames_alone() {
        let chain = both();
        let reasoning = concat!(
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"ask a@x.com\"}\n\n",
            "event: response.reasoning_summary_text.done\ndata: {\"type\":\"response.reasoning_summary_text.done\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"text\":\"ask a@x.com\"}\n\n",
            "event: response.reasoning_summary_part.done\ndata: {\"type\":\"response.reasoning_summary_part.done\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"part\":{\"type\":\"summary_text\",\"text\":\"ask a@x.com\"}}\n\n",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"status\":\"completed\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"ask a@x.com\"}]}}\n\n",
        );
        let raw = format!(
            "{reasoning}{}",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0,\"delta\":\"reply to b@y.org\"}\n\n",
        );
        let (out, _) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.starts_with(reasoning),
            "every reasoning frame must survive byte-identical: {out}"
        );
        assert!(
            out.contains("\"delta\":\"reply to [EMAIL_REDACTED]\""),
            "…and the message beside them is still masked: {out}"
        );
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

    /// Generated reasoning is out of the output scope. A native upstream
    /// that streams raw reasoning (gpt-oss behind a Responses endpoint)
    /// closes the reasoning item's `reasoning_text` part with a
    /// `response.content_part.done`, the same event a message's
    /// `output_text` part closes with — only the message part is masked.
    #[test]
    fn responses_sse_leaves_a_reasoning_text_content_part_alone() {
        let chain = both();
        let raw = concat!(
            "event: response.content_part.done\ndata: {\"type\":\"response.content_part.done\",\"item_id\":\"rs_1\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"reasoning_text\",\"text\":\"think a@x.com\"}}\n\n",
            "event: response.content_part.done\ndata: {\"type\":\"response.content_part.done\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"mail b@y.org\",\"annotations\":[]}}\n\n",
        );
        let (out, counts) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("think a@x.com"),
            "reasoning part was rewritten: {out}"
        );
        assert!(out.contains("mail [EMAIL_REDACTED]"), "{out}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    /// AISIX-Cloud#1621: an `mcp_call`'s arguments stream on their own
    /// event pair, and `redact_responses_item_structured` masks the same string on the
    /// terminal item — so the pair must be masked too, or one stream
    /// carries the raw value beside its masked copy.
    #[test]
    fn responses_sse_masks_the_mcp_call_arguments_channel() {
        let chain = both();
        let raw = concat!(
            "event: response.mcp_call_arguments.delta\ndata: {\"type\":\"response.mcp_call_arguments.delta\",\"item_id\":\"mcp_1\",\"output_index\":0,\"delta\":\"{\\\"to\\\":\\\"a@\"}\n\n",
            "event: response.mcp_call_arguments.delta\ndata: {\"type\":\"response.mcp_call_arguments.delta\",\"item_id\":\"mcp_1\",\"output_index\":0,\"delta\":\"x.com\\\"}\"}\n\n",
            "event: response.mcp_call_arguments.done\ndata: {\"type\":\"response.mcp_call_arguments.done\",\"item_id\":\"mcp_1\",\"output_index\":0,\"arguments\":\"{\\\"to\\\":\\\"a@x.com\\\"}\"}\n\n",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"mcp_call\",\"id\":\"mcp_1\",\"server_label\":\"s\",\"name\":\"send\",\"arguments\":\"{\\\"to\\\":\\\"a@x.com\\\"}\"}}\n\n",
        );
        let (out, counts) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            !out.contains("a@"),
            "an mcp_call argument fragment reached the client unmasked: {out}"
        );
        assert_eq!(out.matches("[EMAIL_REDACTED]").count(), 3, "out: {out}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    /// A custom tool's input streams on its own event pair, and the same
    /// string is masked on the terminal item by `redact_responses_item`.
    /// Leave the pair out of this walk and the caller reads the masked
    /// text on `output_item.done` and the raw match on the delta it
    /// assembled the input from — one stream carrying both.
    #[test]
    fn responses_sse_masks_the_custom_tool_input_channel() {
        let chain = both();
        let raw = concat!(
            "event: response.custom_tool_call_input.delta\ndata: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"ctc_1\",\"output_index\":0,\"delta\":\"mail a@x.com now\"}\n\n",
            "event: response.custom_tool_call_input.done\ndata: {\"type\":\"response.custom_tool_call_input.done\",\"item_id\":\"ctc_1\",\"output_index\":0,\"input\":\"mail a@x.com now\"}\n\n",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"custom_tool_call\",\"id\":\"ctc_1\",\"call_id\":\"c1\",\"name\":\"apply_patch\",\"input\":\"mail a@x.com now\",\"status\":\"completed\"}}\n\n",
        );
        let (out, counts) = redact_responses_sse(chain.as_ref(), raw.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            !out.contains("a@x.com"),
            "the custom tool input reached the client unmasked: {out}"
        );
        assert_eq!(out.matches("[EMAIL_REDACTED]").count(), 3, "out: {out}");
        assert_eq!(counts.get("email"), Some(&1));
    }

    // #1091: an upstream that ends mid-frame used to leave the two passes
    // reading different bytes — the line-based block scan saw the fragment,
    // the frame-based redactor appended it verbatim without masking it. The
    // seal is what makes both read the same frames, so these assert the
    // masking THROUGH it: drop the `seal_buffered_sse` call and the PII
    // literal comes back.
    #[test]
    fn responses_sse_masks_a_final_frame_the_upstream_never_terminated() {
        let chain = both();
        let mut raw = concat!(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"hello \"}\n\n",
            // Complete JSON, only the terminating blank line missing.
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"mail a@x.com\"}",
        )
        .as_bytes()
        .to_vec();
        assert_eq!(
            seal_buffered_sse(&mut raw).tail,
            SseTailSeal::Completed { padded: 2 }
        );
        let (out, counts) = redact_responses_sse(chain.as_ref(), &raw)
            .expect("the sealed tail must reach the redactor");
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "tail left unmasked: {out}");
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        assert_eq!(counts.get("email"), Some(&1));
        // And the client gets a frame it can actually parse.
        assert!(out.ends_with("\n\n"), "out: {out}");
    }

    #[test]
    fn anthropic_sse_masks_a_final_frame_the_upstream_never_terminated() {
        let chain = both();
        let mut raw = concat!(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"mail a@x.com\"}}",
        )
        .as_bytes()
        .to_vec();
        assert_eq!(
            seal_buffered_sse(&mut raw).tail,
            SseTailSeal::Completed { padded: 2 }
        );
        let (out, counts) = anthropic_sse_rewrite(chain.as_ref(), &raw)
            .expect("the sealed tail must reach the redactor");
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "tail left unmasked: {out}");
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        assert_eq!(counts.get("email"), Some(&1));
        assert!(out.ends_with("\n\n"), "out: {out}");
    }

    #[test]
    fn seal_leaves_a_terminated_body_byte_identical() {
        let raw = b"event: x\ndata: {\"a\":1}\n\n".to_vec();
        let mut sealed = raw.clone();
        assert_eq!(seal_buffered_sse(&mut sealed).tail, SseTailSeal::Terminated);
        assert_eq!(sealed, raw);
    }

    #[test]
    fn seal_pads_by_what_the_upstream_left_off() {
        // Nothing of the terminator written.
        let mut none = b"data: {\"a\":1}".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut none).tail,
            SseTailSeal::Completed { padded: 2 }
        );
        assert_eq!(none, b"data: {\"a\":1}\n\n");
        // Half of it written: one newline is what is missing, and padding a
        // fixed `\n\n` here would leave a stray blank line in the frames.
        let mut half = b"data: {\"a\":1}\n".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut half).tail,
            SseTailSeal::Completed { padded: 1 }
        );
        assert_eq!(half, b"data: {\"a\":1}\n\n");
        // A comment / keepalive carries nothing to scan, so it is kept.
        let mut comment = b"data: {\"a\":1}\n\n: ping".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut comment).tail,
            SseTailSeal::Completed { padded: 2 }
        );
    }

    #[test]
    fn crlf_framed_bodies_are_framed_and_masked_like_any_other() {
        let chain = both();
        // A CRLF-framed stream is ordinary text to the line-based scan, so
        // the redactor has to read the same frames out of it. Reading none
        // would leave it unmasked; reading ONE would drop every `data:` line
        // after the first when that frame is rewritten.
        let raw = concat!(
            "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"mail a@\"}\r\n\r\n",
            "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"x.com ok\"}\r\n\r\n",
            "event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{}}\r\n\r\n",
        )
        .as_bytes()
        .to_vec();
        let mut sealed = raw.clone();
        assert_eq!(seal_buffered_sse(&mut sealed).tail, SseTailSeal::Terminated);
        assert_eq!(sealed, raw, "a terminated CRLF body must not be repadded");
        let (out, counts) = redact_responses_sse(chain.as_ref(), &sealed).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "out: {out}");
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        assert_eq!(counts.get("email"), Some(&1));
        // Every frame survives the rewrite.
        assert_eq!(out.matches("data:").count(), 3, "out: {out}");
        assert!(out.contains("response.completed"), "out: {out}");
    }

    #[test]
    fn seal_matches_the_upstreams_own_line_ending() {
        let mut crlf = b"data: {\"a\":1}\r\n".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut crlf).tail,
            SseTailSeal::Completed { padded: 2 }
        );
        assert_eq!(crlf, b"data: {\"a\":1}\r\n\r\n");
        // Three quarters of a CRLF terminator written.
        let mut partial = b"data: {\"a\":1}\r\n\r".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut partial).tail,
            SseTailSeal::Completed { padded: 1 }
        );
        assert_eq!(partial, b"data: {\"a\":1}\r\n\r\n");
    }

    #[test]
    fn seal_keeps_a_crlf_stream_crlf_when_the_tail_has_no_terminator_bytes() {
        // The fragment itself carries no clue — it stops right after its
        // payload — so the completed frames before it settle the style.
        let mut raw = concat!(
            "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"hello \"}\r\n\r\n",
            "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"mail a@x.com\"}",
        )
        .as_bytes()
        .to_vec();
        assert_eq!(
            seal_buffered_sse(&mut raw).tail,
            SseTailSeal::Completed { padded: 4 }
        );
        assert!(raw.ends_with(b"\r\n\r\n"));
        // And the sealed frame is a frame to the redactor, like the first.
        let (out, _) = redact_responses_sse(both().as_ref(), &raw).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "out: {out}");
        assert_eq!(out.matches("data:").count(), 2, "out: {out}");
        // Half a payload line written: the CR is there, its LF is not.
        let mut half_line = b"data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut half_line).tail,
            SseTailSeal::Completed { padded: 3 }
        );
        assert!(half_line.ends_with(b"data: {\"b\":2}\r\n\r\n"));
    }

    #[test]
    fn seal_cuts_a_fragment_that_is_several_frames_run_together() {
        // An upstream that never wrote its blank lines. Padding this into
        // ONE frame would be worse than leaving it: the redactor writes only
        // the first `data:` line of a frame it rewrites, so a mask would
        // delete `KEEPME` and the terminal event with it.
        let mut raw = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"mail a@x.com\"}\n",
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\" and KEEPME\"}\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}",
        )
        .as_bytes()
        .to_vec();
        assert!(matches!(
            seal_buffered_sse(&mut raw).tail,
            SseTailSeal::Dropped { .. }
        ));
        // Nothing was scanned, so nothing is released — the caller refuses.
        assert!(raw.is_empty());
    }

    #[test]
    fn a_masked_crlf_document_stays_crlf_framed() {
        let chain = both();
        let raw = concat!(
            "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"mail a@x.com\"}\r\n\r\n",
            "event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{}}\r\n\r\n",
        )
        .as_bytes()
        .to_vec();
        let (out, _) = redact_responses_sse(chain.as_ref(), &raw).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        // The mask must not re-frame the document it passed through: every
        // terminator, and the rewritten line's own ending, stay CRLF.
        assert!(!out.contains("\n\n"), "LF-framed after masking: {out:?}");
        assert_eq!(out.matches("\r\n\r\n").count(), 2, "out: {out:?}");
        // `split`, not `lines`: the latter strips the very `\r` under test.
        let masked = out
            .split('\n')
            .find(|l| l.contains("[EMAIL_REDACTED]"))
            .expect("the masked line");
        assert!(
            masked.ends_with('\r'),
            "rewritten line lost its CR: {masked:?}"
        );
    }

    #[test]
    fn seal_always_leaves_the_buffer_ending_on_a_terminator() {
        // The invariant the whole fix rests on, and the one `split_sse_frames`
        // documents: after a seal there is no trailing fragment left for the
        // two passes to read differently. Exhaustive over the bytes that can
        // form a terminator plus one payload byte, so the boundary cases
        // (a fragment that is a lone `\r` after a CRLF frame, a `\n\n\n`
        // run, an empty buffer) are covered by construction rather than by
        // imagination.
        let alphabet = [b'a', b'\n', b'\r'];
        let mut words: Vec<Vec<u8>> = vec![Vec::new()];
        for _ in 0..7 {
            let mut next = Vec::new();
            for w in &words {
                for b in alphabet {
                    let mut w = w.clone();
                    w.push(b);
                    next.push(w);
                }
            }
            for w in &words {
                let mut buf = w.clone();
                let before = buf.clone();
                seal_buffered_sse(&mut buf);
                assert_eq!(
                    last_frame_end(&buf),
                    buf.len(),
                    "sealed {before:?} into {buf:?}, which still has a tail",
                );
                // And sealing is settled: a second pass changes nothing.
                let once = buf.clone();
                assert_eq!(seal_buffered_sse(&mut buf).tail, SseTailSeal::Terminated);
                assert_eq!(buf, once, "seal is not idempotent on {before:?}");
            }
            words = next;
        }
    }

    #[test]
    fn seal_keeps_a_final_frame_with_nothing_to_scan() {
        // Neither payload is JSON, and neither carries text any pass reads —
        // cutting them would only cost the client its terminal event.
        for tail in ["data: [DONE]", "data:"] {
            let mut raw = format!("data: {{\"a\":1}}\n\n{tail}").into_bytes();
            assert!(
                matches!(
                    seal_buffered_sse(&mut raw).tail,
                    SseTailSeal::Completed { .. }
                ),
                "cut {tail}",
            );
            assert!(String::from_utf8(raw)
                .unwrap()
                .ends_with(&format!("{tail}\n\n")));
        }
    }

    #[test]
    fn seal_cuts_a_final_frame_that_cannot_be_parsed() {
        // Truncated mid-JSON: nothing can extract its text, so nothing can
        // scan it, so it must not be released.
        let mut raw = b"data: {\"a\":1}\n\ndata: {\"text\":\"SECRET".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut raw).tail,
            SseTailSeal::Dropped { dropped: 21 },
        );
        assert_eq!(raw, b"data: {\"a\":1}\n\n");
        // When it is the WHOLE body the buffer is left empty — the caller
        // refuses rather than answering with an empty 200.
        let mut only = b"data: {\"text\":\"SECRET".to_vec();
        assert!(matches!(
            seal_buffered_sse(&mut only).tail,
            SseTailSeal::Dropped { .. }
        ));
        assert!(only.is_empty());
    }

    // #1100: the sibling of the fragment above, in the frames BEFORE it. A
    // terminated frame whose payload is not one JSON document was read by
    // neither pass — the scan `continue`d past a line it could not parse and
    // the redactor skipped a frame with no parsed payload — so it went out
    // byte-for-byte, seen by nothing. And a frame whose payload legitimately
    // spans several `data:` lines lost every line after the first when the
    // redactor rewrote it.

    #[test]
    fn a_frames_payload_is_all_of_its_data_lines_joined() {
        // Split mid-document, which is exactly what the spec's `\n` join is
        // for. Reading only the first line leaves neither half parseable.
        let frame = b"event: x\ndata: {\"type\":\"content_block_delta\",\ndata: \"index\":0}";
        assert_eq!(
            frame_payload(frame).as_deref(),
            Some("{\"type\":\"content_block_delta\",\n\"index\":0}"),
        );
        // One leading space belongs to the framing, the rest to the payload.
        assert_eq!(frame_payload(b"data:  x").as_deref(), Some(" x"));
        // So does the CR of a CRLF line ending — the payload is the text
        // between them. Only a continued payload can carry one: a frame's
        // last line hands its CR to the terminator.
        assert_eq!(
            frame_payload(b"data: one\r\ndata: two").as_deref(),
            Some("one\ntwo"),
        );
        // No `data:` line at all: a comment or keepalive, nothing to read.
        assert_eq!(frame_payload(b": ping"), None);
    }

    /// `data_line_ranges` is what the model restamp splices into, so an
    /// off-by-one in its arithmetic would rewrite the wrong bytes. The
    /// ranges are checked against the payload lines they must select on
    /// every framing variant a provider is allowed to emit.
    #[test]
    fn data_line_ranges_select_exactly_the_payload_lines() {
        for (frame, want) in [
            // Canonical: labelled event, LF terminators, one space after the colon.
            (
                &b"event: message_start\ndata: {\"a\":1}\n\n"[..],
                vec![&b"{\"a\":1}"[..]],
            ),
            // CRLF: the `\r` belongs to the framing, not the payload.
            (
                &b"event: x\r\ndata: {\"a\":1}\r\n\r\n"[..],
                vec![&b"{\"a\":1}"[..]],
            ),
            // No space after the colon — the spec makes it optional.
            (&b"data:{\"a\":1}\n\n"[..], vec![&b"{\"a\":1}"[..]]),
            // A comment/keepalive line ahead of the data line.
            (&b": ping\ndata: {\"a\":1}\n\n"[..], vec![&b"{\"a\":1}"[..]]),
            // Terminal sentinel.
            (&b"data: [DONE]\n\n"[..], vec![&b"[DONE]"[..]]),
            // Empty payload: a zero-width range, not a panic and not the tail.
            (&b"data:\n\n"[..], vec![&b""[..]]),
            // No data line at all.
            (&b"event: ping\n\n"[..], vec![]),
            // A value containing the delimiter bytes must not confuse the scan.
            (
                &b"event: e\ndata: {\"t\":\"a: b\"}\n\n"[..],
                vec![&b"{\"t\":\"a: b\"}"[..]],
            ),
            // One document over several lines: every line is selected, and a
            // non-data line between two of them is not (#1105).
            (
                &b"event: e\ndata: {\"a\":1,\nid: 7\ndata: \"b\":2}\n\n"[..],
                vec![&b"{\"a\":1,"[..], &b"\"b\":2}"[..]],
            ),
        ] {
            let ranges = data_line_ranges(frame);
            let got: Vec<&[u8]> = ranges.iter().map(|r| &frame[r.clone()]).collect();
            assert_eq!(
                got,
                want,
                "ranges select the payload lines for {:?}",
                String::from_utf8_lossy(frame),
            );
        }
    }

    #[test]
    fn anthropic_sse_masks_a_frame_whose_payload_spans_several_data_lines() {
        let chain = both();
        let mut raw = concat!(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
            // One frame, one JSON document, written over two `data:` lines.
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\n",
            "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"mail a@x.com\"}}\n\n",
        )
        .as_bytes()
        .to_vec();
        // Nothing to seal — every frame here is terminated — and nothing to
        // excise either: the joined payload IS one JSON document.
        let seal = seal_buffered_sse(&mut raw);
        assert_eq!(seal.tail, SseTailSeal::Terminated);
        assert!(seal.excised.is_empty(), "excised: {:?}", seal.excised);
        let (out, counts) = anthropic_sse_rewrite(chain.as_ref(), &raw)
            .expect("the whole payload must reach the redactor");
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "left unmasked: {out}");
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        assert_eq!(counts.get("email"), Some(&1));
        // The rewritten frame carries the whole document, re-serialised onto
        // one `data:` line — the payload is preserved, not truncated to its
        // first line.
        assert!(out.contains("\"index\":0"), "second line dropped: {out}");
        assert!(out.contains("\"text_delta\""), "second line dropped: {out}");
    }

    #[test]
    fn responses_sse_masks_a_frame_whose_payload_spans_several_data_lines() {
        let chain = both();
        let mut raw = concat!(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"hello \"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\n",
            "data: \"item_id\":\"m\",\"delta\":\"mail a@x.com\"}\n\n",
        )
        .as_bytes()
        .to_vec();
        let seal = seal_buffered_sse(&mut raw);
        assert!(seal.excised.is_empty(), "excised: {:?}", seal.excised);
        let (out, counts) = redact_responses_sse(chain.as_ref(), &raw)
            .expect("the whole payload must reach the redactor");
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "left unmasked: {out}");
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        assert_eq!(counts.get("email"), Some(&1));
        assert!(
            out.contains("\"item_id\":\"m\""),
            "second line dropped: {out}"
        );
    }

    #[test]
    fn seal_excises_a_terminated_frame_whose_payload_is_not_json() {
        // Plain text in the middle of an otherwise well-formed stream. No
        // pass could read it, so it used to be released having been scanned
        // by nothing.
        let mut raw = b"data: {\"a\":1}\n\ndata: my mail is a@x.com\n\ndata: [DONE]\n\n".to_vec();
        let seal = seal_buffered_sse(&mut raw);
        assert_eq!(seal.tail, SseTailSeal::Terminated);
        // Handed back for the caller's block scan: nothing can mask it, but
        // a forbidden literal in it still has to block the response.
        assert_eq!(seal.excised, vec!["my mail is a@x.com".to_string()]);
        assert_eq!(seal.excised_bytes, "data: my mail is a@x.com\n\n".len());
        assert!(seal.cut_anything());
        // ...and it is gone from what will be released, while its neighbours
        // — including the frames with nothing to scan — are untouched.
        assert_eq!(raw, b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
    }

    #[test]
    fn seal_excises_a_terminated_frame_whose_data_lines_do_not_join_into_one_document() {
        // Two complete events run together with no blank line between them:
        // not one frame, and the join is not one document either. Same rule
        // as the tail shape, now applied wherever it appears.
        let mut raw =
            b"data: {\"a\":1}\n\ndata: {\"b\":2}\ndata: {\"c\":3}\n\ndata: [DONE]\n\n".to_vec();
        let seal = seal_buffered_sse(&mut raw);
        assert_eq!(
            seal.excised,
            vec!["{\"b\":2}\n{\"c\":3}".to_string()],
            "raw: {}",
            String::from_utf8_lossy(&raw),
        );
        assert_eq!(raw, b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
    }

    #[test]
    fn seal_can_excise_the_whole_buffer() {
        // The caller refuses with `unscannable_body` rather than answering
        // with an empty 200 — `cut_anything` is what tells it apart from an
        // upstream that simply sent nothing.
        let mut raw = b"data: not json at all\n\n".to_vec();
        let seal = seal_buffered_sse(&mut raw);
        assert!(raw.is_empty());
        assert!(seal.cut_anything());

        let mut empty = Vec::new();
        let seal = seal_buffered_sse(&mut empty);
        assert!(!seal.cut_anything(), "an empty body is not a cut");
    }

    #[test]
    fn seal_keeps_frames_with_nothing_to_scan_and_crlf_frames() {
        // A comment, a keepalive and an empty payload carry no text any pass
        // reads, so excising them would cost the client its events and
        // protect nothing. Framed CRLF, since that is the form the splitter
        // used to miss entirely.
        let raw = b": ping\r\n\r\ndata:\r\n\r\ndata: [DONE]\r\n\r\n".to_vec();
        let mut sealed = raw.clone();
        let seal = seal_buffered_sse(&mut sealed);
        assert!(seal.excised.is_empty(), "excised: {:?}", seal.excised);
        assert_eq!(sealed, raw);
        // `sealed == raw` alone would hold even if the splitter saw one
        // unframed blob, since an empty excision returns the buffer
        // untouched. Assert the framing itself: three CRLF frames, and the
        // `\r` stripped from each payload rather than left in it.
        assert_eq!(
            sse_frame_payloads(&raw),
            vec![String::new(), "[DONE]".to_string()],
        );
    }

    #[test]
    fn seal_excises_a_crlf_frame_and_leaves_its_neighbours_crlf() {
        let mut crlf =
            b"data: {\"a\":1}\r\n\r\ndata: not json\r\n\r\ndata: [DONE]\r\n\r\n".to_vec();
        let seal = seal_buffered_sse(&mut crlf);
        assert_eq!(seal.excised, vec!["not json".to_string()]);
        assert_eq!(crlf, b"data: {\"a\":1}\r\n\r\ndata: [DONE]\r\n\r\n");
    }

    #[test]
    fn anthropic_sse_masks_a_crlf_frame_whose_payload_spans_several_data_lines() {
        // The CRLF half of the LF case above. The dropped `data:` line takes
        // its line ending with it, so without care the CR of the line BEFORE
        // the separator is left stranded and the client reads `...}\r` as
        // the payload.
        let chain = both();
        let mut raw = concat!(
            "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\r\n",
            "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"mail a@x.com\"}}\r\n\r\n",
        )
        .as_bytes()
        .to_vec();
        let seal = seal_buffered_sse(&mut raw);
        assert!(seal.excised.is_empty(), "excised: {:?}", seal.excised);
        let (out, _) = anthropic_sse_rewrite(chain.as_ref(), &raw)
            .expect("the whole payload must reach the redactor");
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "left unmasked: {out}");
        assert!(out.contains("[EMAIL_REDACTED]"), "out: {out}");
        // A masked CRLF frame stays a well-formed CRLF frame: the payload
        // ends at `}`, not at a stray CR.
        assert!(out.ends_with("}\r\n\r\n"), "stray CR: {out:?}");
        assert!(!out.contains("\r\r"), "stray CR: {out:?}");
    }

    /// The event-stream spec lets a line end in a bare `\r` too. A body
    /// framed that way is framed, scanned and masked like any other, and
    /// comes back framed the way it arrived.
    #[test]
    fn a_cr_framed_body_is_read_masked_and_left_cr_framed() {
        let chain = both();
        let raw = concat!(
            "event: response.output_text.delta\rdata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"delta\":\"mail a@x.com\"}\r\r",
            "event: response.completed\rdata: {\"type\":\"response.completed\",\"response\":{}}\r\r",
        )
        .as_bytes()
        .to_vec();
        assert_eq!(sse_frame_payloads(&raw).len(), 2);
        let mut sealed = raw.clone();
        assert_eq!(seal_buffered_sse(&mut sealed).tail, SseTailSeal::Terminated);
        assert_eq!(sealed, raw);
        let (out, counts) = redact_responses_sse(chain.as_ref(), &sealed).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "out: {out:?}");
        assert_eq!(counts.get("email"), Some(&1));
        assert!(!out.contains('\n'), "re-framed with LF: {out:?}");
        assert_eq!(out.matches("\r\r").count(), 2, "out: {out:?}");
    }

    #[test]
    fn a_masked_cr_frame_spanning_several_data_lines_keeps_one_line_and_its_framing() {
        let chain = both();
        let raw = concat!(
            "event: content_block_delta\rdata: {\"type\":\"content_block_delta\",\"index\":0,\r",
            "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"mail a@x.com\"}}\r\r",
        );
        let (out, _) = anthropic_sse_rewrite(chain.as_ref(), raw.as_bytes())
            .expect("the whole payload must reach the redactor");
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("a@x.com"), "left unmasked: {out:?}");
        assert!(
            out.starts_with("event: content_block_delta\rdata: {"),
            "out: {out:?}"
        );
        assert_eq!(out.matches("data:").count(), 1, "out: {out:?}");
        assert!(out.ends_with("}\r\r"), "out: {out:?}");
    }

    #[test]
    fn seal_pads_a_cr_stream_with_cr() {
        let mut raw = b"data: {\"a\":1}\r\rdata: {\"b\":2}".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut raw).tail,
            SseTailSeal::Completed { padded: 2 }
        );
        assert_eq!(raw, b"data: {\"a\":1}\r\rdata: {\"b\":2}\r\r");
        let mut raw = b"data: {\"a\":1}\r".to_vec();
        assert_eq!(
            seal_buffered_sse(&mut raw).tail,
            SseTailSeal::Completed { padded: 1 }
        );
        assert_eq!(raw, b"data: {\"a\":1}\r\r");
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
                    {"type": "refusal", "refusal": "mail c@z.io"}
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
            "mail [EMAIL_REDACTED]"
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

    /// A streamed transcription carries its text JSON-escaped, so the raw
    /// bytes hide a match: the scan and the mask read each event decoded
    /// (R-044). The delta channel is one text, the done event's text is
    /// masked in place, and everything else in a frame stays as written.
    #[test]
    fn transcription_sse_is_read_and_masked_decoded() {
        let chain = both();
        let raw = concat!(
            "data: {\"type\":\"transcript.text.delta\",\"delta\":\"mail alice\\u0040exam\"}\r\n\r\n",
            "data: {\"type\":\"transcript.text.delta\",\"delta\":\"ple.com now\"}\r\n\r\n",
            "data: {\"type\":\"transcript.text.done\",\"text\":\"mail alice\\u0040example.com now\",",
            "\"usage\":{\"type\":\"tokens\",\"input_tokens\":26,\"output_tokens\":12,\"total_tokens\":38}}\r\n\r\n",
            "data: [DONE]\r\n\r\n",
        )
        .as_bytes();
        assert!(!String::from_utf8_lossy(raw).contains("alice@example.com"));
        assert_eq!(
            transcription_sse_text(raw),
            "mail alice@example.com now",
            "the deltas and the done text read once, decoded",
        );

        let (rewritten, counts) = redact_transcription_sse(chain.as_ref(), raw).expect("rewrite");
        assert_eq!(counts.get("email"), Some(&2));
        let out = String::from_utf8(rewritten).unwrap();
        let events: Vec<Value> = out
            .split("\r\n\r\n")
            .filter_map(|f| f.strip_prefix("data: "))
            .filter(|p| *p != "[DONE]")
            .map(|p| serde_json::from_str(p).expect("each frame stays JSON"))
            .collect();
        assert_eq!(events.len(), 3);
        let deltas: String = events[..2]
            .iter()
            .map(|e| e["delta"].as_str().unwrap())
            .collect();
        assert_eq!(deltas, "mail [EMAIL_REDACTED] now");
        assert_eq!(events[2]["text"], "mail [EMAIL_REDACTED] now");
        assert_eq!(
            events[2]["usage"]["output_tokens"], 12,
            "usage stays as written"
        );
        assert!(out.ends_with("data: [DONE]\r\n\r\n"), "{out}");

        let clean = b"data: {\"type\":\"transcript.text.done\",\"text\":\"all clean\"}\n\n";
        assert!(redact_transcription_sse(chain.as_ref(), clean).is_none());
    }

    /// `include[]=logprobs` puts the transcript's tokens beside its text, on
    /// the JSON answer and on every streamed event. A masked transcript loses
    /// them wherever they are, or the address the mask took out could be read
    /// back off the tokens; an unmasked one keeps them as written.
    #[test]
    fn a_masked_transcription_loses_its_logprobs() {
        let chain = both();
        let tokens = |ts: &[&str]| -> Value {
            ts.iter()
                .map(|t| json!({"token": t, "logprob": -0.1, "bytes": t.as_bytes()}))
                .collect()
        };

        let body = json!({
            "text": "mail alice@example.com now",
            "logprobs": tokens(&["mail", " alice", "@", "example", ".com", " now"]),
            "usage": {"type": "tokens", "input_tokens": 26, "output_tokens": 12, "total_tokens": 38}
        });
        let (rewritten, _) =
            redact_transcription_response(chain.as_ref(), &serde_json::to_vec(&body).unwrap())
                .expect("must rewrite");
        let v: Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["text"], "mail [EMAIL_REDACTED] now");
        assert!(v.get("logprobs").is_none(), "{v}");
        assert_eq!(v["usage"]["output_tokens"], 12);

        let frame = |event: Value| format!("data: {event}\n\n");
        let raw = [
            frame(
                json!({"type": "transcript.text.delta", "delta": "mail alice@exam",
                         "logprobs": tokens(&["mail", " alice", "@", "exam"])}),
            ),
            frame(
                json!({"type": "transcript.text.delta", "delta": "ple.com now",
                         "logprobs": tokens(&["ple", ".com", " now"])}),
            ),
            frame(
                json!({"type": "transcript.text.done", "text": "mail alice@example.com now",
                         "logprobs": tokens(&["mail", " alice", "@", "example", ".com", " now"])}),
            ),
        ]
        .concat();
        let (rewritten, _) =
            redact_transcription_sse(chain.as_ref(), raw.as_bytes()).expect("must rewrite");
        let out = String::from_utf8(rewritten).unwrap();
        assert!(!out.contains("logprobs"), "{out}");
        for token in ["alice", "exam", "ple"] {
            assert!(!out.contains(token), "{token} survived in {out}");
        }

        let clean = frame(json!({"type": "transcript.text.done", "text": "all clean",
                                 "logprobs": tokens(&["all", " clean"])}));
        assert!(redact_transcription_sse(chain.as_ref(), clean.as_bytes()).is_none());
        let clean = json!({"text": "all clean", "logprobs": tokens(&["all", " clean"])});
        assert!(redact_transcription_response(
            chain.as_ref(),
            &serde_json::to_vec(&clean).unwrap()
        )
        .is_none());
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
        async fn moderate_input_segments(
            &self,
            texts: &[String],
            _newest_from: Option<usize>,
        ) -> SegmentsOutcome {
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
            None,
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
            None,
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
            logprobs: None,
            more_choices: Vec::new(),
            service_tier: None,
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
            choice_index: 0,
            logprobs: None,
            service_tier: None,
            accounting_usage: None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            logprobs: None,
            more_choices: Vec::new(),
            service_tier: None,
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
            logprobs: None,
            more_choices: Vec::new(),
            service_tier: None,
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
            None,
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
        let moderation = moderate_body(
            &chain,
            Direction::Input,
            None,
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
    /// with no segment or local member is a no-op.
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
            None,
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

        // A chain with neither a segment nor a local member never enters
        // the pass.
        let sync_only = aisix_guardrails::GuardrailChain::new(vec![]);
        let verdict = moderate_body(
            &sync_only,
            Direction::Input,
            None,
            GuardrailVerdict::Allow,
            &mut counts,
            &mut Vec::new(),
            |_| -> RedactionCounts { panic!("walk must not run when no segment member exists") },
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

    /// A text block may open with text of its own (the start event's union
    /// allows a non-empty one). That text heads its channel, so it is ONE
    /// slot: offered again by the block pass, the apply walk would meet the
    /// channel's rewrite where the collect walk saw the original and refuse
    /// the answer as a drift (R-071). Its citations are still walked.
    #[tokio::test]
    async fn a_non_empty_start_text_is_one_slot_of_its_channel() {
        let chain = seg_chain(StubSegments::masker());
        let mut held: Vec<u8> = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"hello \",\"citations\":[{\"type\":\"char_location\",\"cited_text\":\"quoted\"}]}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n",
        )
        .as_bytes()
        .to_vec();
        let collected = collect_segments(|g| {
            let _ = redact_anthropic_sse(g, &held);
        });
        let texts: Vec<&str> = collected.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["hello world", "quoted"], "each text offered once");

        let moderation = moderate_anthropic_sse(
            &chain,
            GuardrailVerdict::Allow,
            &mut held,
            &mut RedactionCounts::new(),
            &mut Vec::new(),
        )
        .await;
        assert_eq!(moderation.verdict, GuardrailVerdict::Allow);
        let out = String::from_utf8(held.clone()).unwrap();
        assert!(out.contains("<M0:HELLO WORLD>"), "{out}");
        assert!(
            out.contains("<M1:QUOTED>"),
            "the citation is masked too: {out}"
        );
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
