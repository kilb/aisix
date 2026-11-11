use super::{count_completion_prompt, extract_completion_usage, CompletionUsage};
use aisix_gateway::{
    BridgeError, ChatMessage, ChatResponse, CompletionByteStream, FinishReason, UsageStats,
};
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompletionStreamFailure {
    Timeout,
    Decode,
    /// The upstream reported an error inside the stream; the typed error is
    /// [`CompletionSseAccumulator::in_band_error`].
    InBand,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompletionStreamOutcome {
    CleanEof,
    UpstreamError {
        status: u16,
        failure: CompletionStreamFailure,
        error_class: &'static str,
    },
    DownstreamDrop,
}

pub(super) struct CompletionOutputObservation {
    pub(super) monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    pub(super) capture_safe: bool,
}

impl CompletionOutputObservation {
    fn unguarded() -> Self {
        Self {
            monitor_hits: Vec::new(),
            capture_safe: true,
        }
    }

    fn uninspected() -> Self {
        Self {
            monitor_hits: Vec::new(),
            capture_safe: false,
        }
    }
}

/// Side-channel parser for legacy completion SSE. It keeps only one bounded
/// partial frame plus bounded text channels; forwarded bytes themselves are
/// never retained on the live path.
const MAX_COMPLETION_CHOICES: usize = 128;

pub(super) struct CompletionSseAccumulator {
    frame_buf: Vec<u8>,
    boundary: SseBoundaryDetector,
    dropping_oversized_frame: bool,
    text_by_choice: BTreeMap<u64, String>,
    text_bytes: usize,
    frame_cap: usize,
    text_cap: usize,
    /// Some of the text did not fit under `text_cap`.
    text_truncated: bool,
    /// All the generated text the frames carried, whether or not it fit.
    generated_bytes: usize,
    pub(super) saw_done: bool,
    /// Where, in the bytes of the [`Self::push`] that found it, the `[DONE]`
    /// frame began — `0` when it began in an earlier push.
    pub(super) done_frame_start: usize,
    pub(super) malformed_data: bool,
    /// The first error the upstream reported inside the stream (vLLM ends a
    /// failed generation with `data: {"error":…}` and then `[DONE]`). It
    /// decides how the stream ended, whatever `[DONE]` or a disconnect says
    /// after it.
    pub(super) in_band_error: Option<BridgeError>,
    pub(super) usage: Option<CompletionUsage>,
    pub(super) provider_request_id: String,
}

impl Default for CompletionSseAccumulator {
    fn default() -> Self {
        Self {
            frame_buf: Vec::new(),
            boundary: SseBoundaryDetector::default(),
            dropping_oversized_frame: false,
            text_by_choice: BTreeMap::new(),
            text_bytes: 0,
            frame_cap: aisix_gateway::sse::MAX_SSE_FRAME_BYTES,
            text_cap: crate::token_estimate::OUTPUT_ACCUMULATION_CAP,
            text_truncated: false,
            generated_bytes: 0,
            saw_done: false,
            done_frame_start: 0,
            malformed_data: false,
            in_band_error: None,
            usage: None,
            provider_request_id: String::new(),
        }
    }
}

impl CompletionSseAccumulator {
    /// Build an accumulator for a fully held security scan. Unlike the live
    /// telemetry path, it must inspect the entire configured hold-back window;
    /// otherwise forbidden text after the telemetry cap could pass unchecked.
    pub(super) fn with_security_cap(cap: usize) -> Self {
        Self {
            frame_cap: cap,
            text_cap: cap,
            ..Self::default()
        }
    }

    /// Observe bytes until the first `[DONE]` event and return the wire prefix
    /// ending at that event. Bytes after the semantic terminal belong to no
    /// completion and must neither affect telemetry nor reach the caller.
    pub(super) fn push(&mut self, bytes: &[u8]) -> usize {
        if self.saw_done {
            return 0;
        }
        // Where the frame being read began in `bytes`: 0 for one carried over
        // from an earlier push.
        let mut frame_start = 0;
        for (offset, byte) in bytes.iter().enumerate() {
            loop {
                match self.boundary.feed(*byte) {
                    SseBoundary::None => {
                        self.push_frame_byte(*byte);
                    }
                    SseBoundary::EndAfterByte => {
                        self.push_frame_byte(*byte);
                        self.finish_frame();
                        if self.saw_done {
                            self.done_frame_start = frame_start;
                            return offset + 1;
                        }
                        frame_start = offset + 1;
                    }
                    SseBoundary::EndBeforeByte => {
                        self.finish_frame();
                        if self.saw_done {
                            self.done_frame_start = frame_start;
                            return offset;
                        }
                        frame_start = offset;
                        continue;
                    }
                }
                break;
            }
        }
        bytes.len()
    }

    /// What `max_buffer_bytes` caps on this stream: the generated text, not
    /// the frames around it (see [`crate::held_content`]).
    pub(super) fn generated_bytes(&self) -> usize {
        self.generated_bytes
    }

    /// EOF terminates the final SSE event even when the provider omitted its
    /// blank-line separator. Observe that frame before making policy or usage
    /// decisions.
    pub(super) fn finish(&mut self) {
        if self.dropping_oversized_frame || !self.frame_buf.is_empty() {
            self.finish_frame();
        }
    }

    fn push_frame_byte(&mut self, byte: u8) {
        if self.dropping_oversized_frame {
            return;
        }
        if self.frame_buf.len() < self.frame_cap {
            self.frame_buf.push(byte);
            return;
        }
        tracing::warn!(
            cap = self.frame_cap,
            "completions stream SSE frame exceeded parsing cap; dropping telemetry frame"
        );
        self.frame_buf.clear();
        self.dropping_oversized_frame = true;
        self.malformed_data = true;
    }

    fn finish_frame(&mut self) {
        self.boundary = SseBoundaryDetector::default();
        if self.dropping_oversized_frame {
            self.dropping_oversized_frame = false;
            self.frame_buf.clear();
            return;
        }
        let frame = std::mem::take(&mut self.frame_buf);
        self.observe_frame(&frame);
    }

    fn observe_frame(&mut self, frame: &[u8]) {
        if crate::redact::is_sse_done_event(frame) {
            self.saw_done = true;
            return;
        }
        let (json, malformed) = crate::redact::parse_sse_json_event(frame);
        self.malformed_data |= malformed;
        let Some(json) = json else {
            return;
        };
        if self.in_band_error.is_none() {
            self.in_band_error = aisix_gateway::capture_in_band_error_value(
                &json,
                aisix_gateway::UpstreamWire::OpenAI,
            );
        }
        if self.provider_request_id.is_empty() {
            self.provider_request_id = crate::usage_attr::provider_response_id(&json);
        }
        if let Some(usage) = extract_completion_usage(&json) {
            self.usage = Some(usage);
        }
        if let Some(choices) = json.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let Some(text) = choice.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                self.generated_bytes = self.generated_bytes.saturating_add(text.len());
                let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(channel) = self.text_by_choice.get_mut(&index) {
                    let appended =
                        push_capped(channel, text, self.text_cap.saturating_sub(self.text_bytes));
                    self.text_bytes += appended;
                    self.text_truncated |= appended < text.len();
                    continue;
                }
                if self.text_by_choice.len() >= MAX_COMPLETION_CHOICES {
                    self.malformed_data = true;
                    continue;
                }
                let separator = usize::from(!self.text_by_choice.is_empty());
                let remaining = self.text_cap.saturating_sub(self.text_bytes);
                if remaining <= separator {
                    self.text_truncated = true;
                    continue;
                }
                let mut channel = String::new();
                let appended = push_capped(&mut channel, text, remaining - separator);
                self.text_truncated |= appended < text.len();
                if appended > 0 {
                    self.text_bytes += separator + appended;
                    self.text_by_choice.insert(index, channel);
                }
            }
        }
    }

    pub(super) fn output_text(&self) -> String {
        self.text_by_choice
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Finish the telemetry parser after a fail-closed wire-cap overflow.
    ///
    /// A provider SSE event can be split across transport chunks. If the
    /// cap-triggering chunk ends in the middle of that event, strict JSON
    /// parsing cannot recover its `choices[].text`, but those bytes were
    /// still generated and must contribute to usage accounting. Preserve the
    /// bounded incomplete frame as a conservative estimation fallback while
    /// keeping already-complete frames in their parsed text form.
    pub(super) fn finish_for_overflow_estimate(&mut self) -> String {
        let partial = if self.frame_buf.is_empty()
            || crate::redact::parse_sse_json_event(&self.frame_buf)
                .0
                .is_some()
        {
            None
        } else {
            Some(self.frame_buf.clone())
        };
        self.finish();
        let mut output = self.output_text();
        if let Some(partial) = partial {
            if !output.is_empty() {
                let remaining =
                    crate::token_estimate::OUTPUT_ACCUMULATION_CAP.saturating_sub(output.len());
                push_capped(&mut output, "\n", remaining);
            }
            let remaining =
                crate::token_estimate::OUTPUT_ACCUMULATION_CAP.saturating_sub(output.len());
            let partial = String::from_utf8_lossy(&partial);
            push_capped(&mut output, &partial, remaining);
        }
        output
    }
}

struct SseBoundaryDetector {
    line_empty: bool,
    pending_cr: Option<bool>,
}

impl Default for SseBoundaryDetector {
    fn default() -> Self {
        Self {
            line_empty: true,
            pending_cr: None,
        }
    }
}

enum SseBoundary {
    None,
    EndBeforeByte,
    EndAfterByte,
}

impl SseBoundaryDetector {
    fn feed(&mut self, byte: u8) -> SseBoundary {
        if let Some(ended_empty_line) = self.pending_cr.take() {
            if byte == b'\n' {
                if ended_empty_line {
                    return SseBoundary::EndAfterByte;
                }
                self.line_empty = true;
                return SseBoundary::None;
            }
            if ended_empty_line {
                return SseBoundary::EndBeforeByte;
            }
            self.line_empty = true;
        }

        match byte {
            b'\r' => self.pending_cr = Some(self.line_empty),
            b'\n' if self.line_empty => return SseBoundary::EndAfterByte,
            b'\n' => self.line_empty = true,
            _ => self.line_empty = false,
        }
        SseBoundary::None
    }
}

impl CompletionStreamOutcome {
    /// How a stream ended whose upstream reported `error` inside it.
    fn in_band(error: &BridgeError) -> Self {
        Self::UpstreamError {
            status: error.http_status(),
            failure: CompletionStreamFailure::InBand,
            error_class: error.error_type(),
        }
    }
}

fn push_capped(buffer: &mut String, text: &str, cap: usize) -> usize {
    if cap == 0 {
        return 0;
    }
    if text.len() <= cap {
        buffer.push_str(text);
        return text.len();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    buffer.push_str(&text[..end]);
    end
}

/// 上游第一个非空字节块到达时，把它相对本次尝试起点的耗时写进 `slot`，只写一次。
/// 这就是上游 TTFT：首块是什么类型都算，和 chat / responses / messages 的口径一致。
///
/// 打在源头而不是某个消费者里：扣留路径在 walk 里边读边缓冲，配置了流超时的
/// 实时路径会先偷看首块再拼回去，扣留溢出转放行时转发的又是重新拼起来的前缀——
/// 只有源头在每条分支上看到的都是上游真正交付的时刻。
pub(super) fn stamp_first_chunk(
    upstream: CompletionByteStream,
    attempt_started: Instant,
    slot: Arc<OnceLock<Duration>>,
) -> CompletionByteStream {
    Box::pin(upstream.inspect(move |item| {
        if matches!(item, Ok(bytes) if !bytes.is_empty()) {
            slot.get_or_init(|| {
                crate::attribution::note_upstream_first_frame();
                attempt_started.elapsed()
            });
        }
    }))
}

/// Whether one SSE frame is the `include_usage` terminal frame: `usage` beside
/// an empty `choices`, which is what the gateway asked the upstream for on a
/// caller's behalf and the caller did not ask to see.
fn is_usage_only_frame(frame: &[u8]) -> bool {
    const KEY: &[u8] = b"\"usage\"";
    if !frame.windows(KEY.len()).any(|w| w == KEY) {
        return false;
    }
    crate::redact::parse_sse_json_event(frame)
        .0
        .is_some_and(|json| {
            json.get("usage").is_some_and(|usage| !usage.is_null())
                && json
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
        })
}

/// A relayed completion stream in whole frames, without the usage frame unless
/// the caller asked for it, and naming the model the caller addressed, as every
/// other stream does. Whole, because a heartbeat goes out on any silence:
/// one landing inside a frame an upstream paused halfway through would cut that
/// event in two. A frame that outgrows the splitter's bound is relayed as it
/// stands rather than held without limit.
pub(super) fn relay_whole_frames<S>(
    upstream: S,
    strip_usage: bool,
    client_facing_model: String,
) -> impl futures::Stream<Item = Result<bytes::Bytes, BridgeError>>
where
    S: futures::Stream<Item = Result<bytes::Bytes, BridgeError>>,
{
    async_stream::stream! {
        futures::pin_mut!(upstream);
        let mut splitter =
            aisix_gateway::sse::SseFrameSplitter::new(aisix_gateway::sse::MAX_SSE_FRAME_BYTES);
        while let Some(item) = upstream.next().await {
            let bytes = match item {
                Ok(bytes) => bytes,
                Err(error) => {
                    let rest = splitter.take_rest();
                    if !rest.is_empty() {
                        yield Ok(bytes::Bytes::from(rest));
                    }
                    yield Err(error);
                    continue;
                }
            };
            splitter.push(&bytes);
            let mut out = Vec::with_capacity(bytes.len());
            loop {
                match splitter.next_frame() {
                    Ok(Some(frame)) if strip_usage && is_usage_only_frame(&frame) => {}
                    // An error event goes out less the credentials this
                    // request sent.
                    Ok(Some(frame)) => {
                        let frame = restamp_completion_frame(frame, &client_facing_model);
                        // In platform mode an upstream's error event becomes
                        // the class of the error it reports.
                        match crate::platform_errors::relayed_openai_error(&frame) {
                            Some(class_frame) => out.extend_from_slice(&class_frame),
                            None => out.extend_from_slice(
                                &aisix_gateway::scrub_upstream_error_frame(&frame),
                            ),
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        out.extend_from_slice(&splitter.take_rest());
                        break;
                    }
                }
            }
            if !out.is_empty() {
                yield Ok(bytes::Bytes::from(out));
            }
        }
        let rest = splitter.take_rest();
        if !rest.is_empty() && (!strip_usage || !is_usage_only_frame(&rest)) {
            yield Ok(bytes::Bytes::from(restamp_completion_frame(rest, &client_facing_model)));
        }
    }
}

/// `frame` with its top-level `model` set to the name the caller addressed.
fn restamp_completion_frame(frame: Vec<u8>, client_facing_model: &str) -> Vec<u8> {
    crate::model_echo::restamp_sse_frame(
        &frame,
        client_facing_model,
        crate::model_echo::top_level_model,
    )
    .unwrap_or(frame)
}

pub(super) fn completion_usage_with_estimates(
    mut usage: CompletionUsage,
    upstream_model: &str,
    prompt: Option<&Value>,
    output_text: &str,
) -> CompletionUsage {
    if usage.prompt_tokens == 0 {
        let estimated = count_completion_prompt(upstream_model, prompt);
        if estimated > 0 {
            usage.prompt_tokens = estimated;
            usage.usage_estimated = true;
        }
    }
    if usage.completion_tokens == 0 {
        let estimated = crate::token_estimate::count_text(upstream_model, output_text);
        if estimated > 0 {
            usage.completion_tokens = estimated;
            usage.usage_estimated = true;
        }
    }
    usage
}

struct CompletionStreamGuard<
    F: FnOnce(CompletionSseAccumulator, CompletionStreamOutcome, CompletionOutputObservation),
> {
    slot: Option<(F, CompletionSseAccumulator)>,
    drop_outcome: CompletionStreamOutcome,
    output_guarded: bool,
}

impl<F: FnOnce(CompletionSseAccumulator, CompletionStreamOutcome, CompletionOutputObservation)> Drop
    for CompletionStreamGuard<F>
{
    fn drop(&mut self) {
        if let Some((complete, mut accumulator)) = self.slot.take() {
            accumulator.finish();
            let observation = if self.output_guarded {
                CompletionOutputObservation::uninspected()
            } else {
                CompletionOutputObservation::unguarded()
            };
            // The upstream already failed the request; a caller leaving
            // after that is not what ended it.
            let outcome = match (&self.drop_outcome, &accumulator.in_band_error) {
                (CompletionStreamOutcome::DownstreamDrop, Some(error)) => {
                    CompletionStreamOutcome::in_band(error)
                }
                _ => self.drop_outcome,
            };
            complete(accumulator, outcome, observation);
        }
    }
}

/// What a hold's verdict adds to the row, beyond what `on_complete` takes. Unset
/// on a hold that never reached one.
#[derive(Default)]
pub(super) struct HeldCompletionVerdict {
    pub(super) refusal: Option<crate::attempt::StreamRefusal>,
    /// Per-detector counts of the output mask.
    pub(super) redactions: crate::redact::RedactionCounts,
    pub(super) captured: Option<aisix_obs::CapturedContent>,
    /// What the estimator reads where the answer never parsed whole.
    pub(super) estimate_text: Option<String>,
    /// How long after the attempt began the caller got its first byte.
    pub(super) released_after: Option<Duration>,
}

/// The `event: error` frame a held completion ends on, the one a chat stream
/// sends for the same failure.
fn failure_frame(payload: &str) -> bytes::Bytes {
    bytes::Bytes::from(format!("event: error\ndata: {payload}\n\n"))
}

/// The frame that ends a stream on an upstream-side failure: the error's own
/// type and text, or in platform mode its class (`crate::platform_errors`).
fn upstream_failure_frame(error: &BridgeError) -> bytes::Bytes {
    failure_frame(
        &crate::platform_errors::openai_stream_payload(error).unwrap_or_else(|| {
            crate::chat::error_frame_payload(error.error_type(), &error.stream_frame_message())
        }),
    )
}

/// The body of a completion a block-capable output chain holds back. The head
/// has gone out by the time this runs and heartbeats keep the connection warm,
/// so a long hold survives whatever sits in front of the gateway; what the hold
/// can no longer do is answer with a status. A complete answer is scanned, then
/// released or replaced by a failure frame, which is what a refusal is on every
/// held stream. Past the chain's cap the hold follows its overflow policy:
/// refuse, or hand what it has to the live relay unscanned.
///
/// A caller who leaves before the verdict received nothing and leaves a 499
/// ([`CompletionStreamGuard`]), with the upstream's counts as far as the frames
/// gave them and an estimate for the rest: the provider bills the call either
/// way.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_held_completion_stream<F>(
    upstream: CompletionByteStream,
    chain: Arc<aisix_guardrails::GuardrailChain>,
    model_name: String,
    upstream_model: String,
    attempt_started: Instant,
    content_cap: Option<u32>,
    // `None` when the request's own text may not be exported.
    captured_prompt: Option<String>,
    verdict_out: Arc<OnceLock<HeldCompletionVerdict>>,
    on_complete: F,
) -> impl futures::Stream<Item = Result<bytes::Bytes, BridgeError>> + Send
where
    F: FnOnce(CompletionSseAccumulator, CompletionStreamOutcome, CompletionOutputObservation)
        + Send
        + 'static,
{
    crate::request_id::in_request_span(async_stream::stream! {
        let mut guard = CompletionStreamGuard {
            slot: Some((on_complete, CompletionSseAccumulator::default())),
            drop_outcome: CompletionStreamOutcome::DownstreamDrop,
            output_guarded: true,
        };
        // The chain's folded cap and overflow policy, whether it resolved to a
        // window or a whole-response hold: this relay holds the whole response.
        let (max_buffer_bytes, fail_open) =
            aisix_guardrails::Guardrail::stream_output_policy(chain.as_ref())
                .hold_cap()
                .unwrap_or((aisix_guardrails::DEFAULT_STREAM_OUTPUT_BUFFER_BYTES, false));
        let mut upstream = upstream;
        let mut buffer = Vec::new();
        // What `buffer` holds (#513): the generated text, which
        // `max_buffer_bytes` caps, and the raw bytes it bounds too.
        let mut held_content = crate::held_content::HeldBuffer::default();
        let mut buffer_held = crate::held_content::HeldBytes::default();
        let mut failure = None;
        while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            let Some((_, accumulator)) = guard.slot.as_mut() else {
                return;
            };
            // The cap-triggering bytes were generated (and may be
            // billed) even though fail-closed policy withholds them.
            // Feed a separately bounded parser before enforcing the
            // wire-buffer limit so usage/token estimates include the
            // complete observed prefix without retaining it all.
            let generated_before = accumulator.generated_bytes();
            let wire = accumulator.push(&chunk);
            let done = accumulator.saw_done;
            held_content.hold(accumulator.generated_bytes() - generated_before, chunk.len());
            if held_content.exceeds(max_buffer_bytes) {
                if fail_open {
                    tracing::warn!(
                        guardrail_hook = "output",
                        model = %model_name,
                        max_buffer_bytes,
                        "streaming /v1/completions output exceeded buffer cap; failing open"
                    );
                    aisix_guardrails::Guardrail::record_output_bypass(
                        chain.as_ref(),
                        crate::error::TAG_OUTPUT_BUFFER_EXCEEDED,
                    );
                    let Some((complete, _)) = guard.slot.take() else {
                        return;
                    };
                    let _ = verdict_out.set(HeldCompletionVerdict {
                        released_after: Some(attempt_started.elapsed()),
                        ..Default::default()
                    });
                    buffer_held.clear();
                    let prefix = futures::stream::iter([
                        Ok(bytes::Bytes::from(std::mem::take(&mut buffer))),
                        Ok(chunk),
                    ]);
                    let live = build_completion_passthrough_stream(
                        Box::pin(prefix.chain(upstream)),
                        Some(chain),
                        upstream_model,
                        complete,
                    );
                    futures::pin_mut!(live);
                    while let Some(item) = live.next().await {
                        yield item;
                    }
                    return;
                }
                tracing::warn!(
                    guardrail_hook = "output",
                    model = %model_name,
                    max_buffer_bytes,
                    "streaming /v1/completions output exceeded buffer cap; failing closed"
                );
                aisix_guardrails::Guardrail::record_output_buffer_exceeded(chain.as_ref());
                let owner = aisix_guardrails::Guardrail::output_buffer_owner(chain.as_ref());
                let estimate_text = accumulator.finish_for_overflow_estimate();
                let _ = verdict_out.set(HeldCompletionVerdict {
                    refusal: Some(crate::attempt::StreamRefusal::new(
                        owner,
                        Some(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED),
                    )),
                    estimate_text: Some(estimate_text),
                    ..Default::default()
                });
                // The refusal is the gateway's; how the upstream ended is still
                // the upstream's. An error it reported in the reads seen so far
                // ends the attempt as that error, as on every other path.
                guard.drop_outcome = match &accumulator.in_band_error {
                    Some(reported) => CompletionStreamOutcome::in_band(reported),
                    None => CompletionStreamOutcome::CleanEof,
                };
                let frame = crate::error::guardrail_block_frame_payload(
                    owner,
                    Some(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED),
                );
                drop(guard);
                yield Ok(failure_frame(&frame));
                return;
            }
            buffer.extend_from_slice(&chunk[..wire]);
            buffer_held.set(buffer.len());
            // The semantic terminal ends the response, as on every other relay:
            // bytes after it belong to no completion.
            if done {
                break;
            }
        }
        // A read that failed, or a frame nobody can parse: what was held is not
        // sent alongside the gateway's terminal error. Its usage is kept for
        // billing.
        if let Some(error) = failure {
            let Some((_, accumulator)) = guard.slot.as_mut() else {
                return;
            };
            let estimate_text = accumulator.finish_for_overflow_estimate();
            let _ = verdict_out.set(HeldCompletionVerdict {
                estimate_text: Some(estimate_text),
                ..Default::default()
            });
            guard.drop_outcome = match &accumulator.in_band_error {
                // What the upstream reported before the read failed is the cause.
                Some(reported) => CompletionStreamOutcome::in_band(reported),
                None => CompletionStreamOutcome::UpstreamError {
                    status: error.http_status(),
                    failure: match &error {
                        BridgeError::Timeout { .. } => CompletionStreamFailure::Timeout,
                        BridgeError::UpstreamDecode(_) => CompletionStreamFailure::Decode,
                        _ => CompletionStreamFailure::Other,
                    },
                    error_class: error.error_type(),
                },
            };
            drop(guard);
            yield Ok(upstream_failure_frame(&error));
            return;
        }
        let mut accumulator = CompletionSseAccumulator::with_security_cap(buffer.len());
        accumulator.push(&buffer);
        accumulator.finish();
        let truncated = !accumulator.saw_done;
        if accumulator.malformed_data || (truncated && buffer.is_empty()) {
            let error = if accumulator.malformed_data {
                BridgeError::UpstreamDecode(
                    "malformed legacy completion SSE data event".to_string(),
                )
            } else {
                BridgeError::StreamAborted
            };
            guard.drop_outcome = CompletionStreamOutcome::UpstreamError {
                status: error.http_status(),
                failure: if accumulator.malformed_data {
                    CompletionStreamFailure::Decode
                } else {
                    CompletionStreamFailure::Other
                },
                error_class: error.error_type(),
            };
            if let Some((_, held)) = guard.slot.as_mut() {
                *held = accumulator;
            }
            drop(guard);
            yield Ok(upstream_failure_frame(&error));
            return;
        }
        let output_text = accumulator.output_text();
        // The answer is whole, so the scan's parse speaks for it from here.
        if let Some((_, held)) = guard.slot.as_mut() {
            std::mem::swap(held, &mut accumulator);
        }
        let synth = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: String::new(),
            model: model_name.clone(),
            message: ChatMessage::assistant(output_text),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::default(),
            service_tier: None,
        };
        let (verdict, hits) = aisix_guardrails::Guardrail::check_output_non_segment_observed(
            chain.as_ref(),
            &synth,
        )
        .await;
        let mut monitor_hits = hits;
        let mut redactions = crate::redact::RedactionCounts::new();
        let moderation = crate::redact::moderate_body(
            chain.as_ref(),
            crate::redact::Direction::Output,
            None,
            verdict,
            &mut redactions,
            &mut monitor_hits,
            |guardrail| match crate::redact::redact_completions_sse(guardrail, &buffer) {
                Some((rewritten, counts)) => {
                    buffer = rewritten;
                    counts
                }
                None => crate::redact::RedactionCounts::new(),
            },
        )
        .await;
        let capture_safe = moderation.capture_safe && !truncated;
        let upstream_end = match (&accumulator.in_band_error, truncated) {
            // The upstream's own report of a failure ends it, `[DONE]` or not.
            (Some(error), _) => CompletionStreamOutcome::in_band(error),
            (None, true) => CompletionStreamOutcome::UpstreamError {
                status: 502,
                failure: CompletionStreamFailure::Other,
                error_class: "stream_aborted",
            },
            (None, false) => CompletionStreamOutcome::CleanEof,
        };
        if let aisix_guardrails::GuardrailVerdict::Block {
            reason,
            guardrail_name,
            unavailable,
        } = moderation.verdict
        {
            tracing::warn!(
                guardrail_name = crate::guardrail_stream::guardrail_name_field(guardrail_name.as_deref()),
                guardrail_hook = "output",
                model = %model_name,
                reason = %reason,
                "guardrail blocked streaming /v1/completions response"
            );
            let _ = verdict_out.set(HeldCompletionVerdict {
                refusal: Some(crate::attempt::StreamRefusal::new(
                    guardrail_name.as_deref(),
                    unavailable.as_deref(),
                )),
                ..Default::default()
            });
            // A refusal does not make a truncated answer the upstream's success.
            if let Some((complete, accumulator)) = guard.slot.take() {
                complete(
                    accumulator,
                    upstream_end,
                    CompletionOutputObservation {
                        monitor_hits,
                        capture_safe: false,
                    },
                );
            }
            yield Ok(failure_frame(&crate::error::guardrail_block_frame_payload(
                guardrail_name.as_deref(),
                unavailable.as_deref(),
            )));
            return;
        }
        if let Some((rewritten, counts)) =
            crate::redact::redact_completions_sse(chain.as_ref(), &buffer)
        {
            buffer = rewritten;
            crate::redact::merge_counts(&mut redactions, counts);
        }
        let captured = match (&captured_prompt, content_cap) {
            (Some(prompt), Some(cap)) if capture_safe => {
                let mut parsed = CompletionSseAccumulator::with_security_cap(buffer.len());
                parsed.push(&buffer);
                parsed.finish();
                Some(aisix_obs::CapturedContent::new(
                    prompt,
                    &parsed.output_text(),
                    cap as usize,
                ))
            }
            _ => None,
        };
        let _ = verdict_out.set(HeldCompletionVerdict {
            redactions,
            captured,
            released_after: Some(attempt_started.elapsed()),
            ..Default::default()
        });
        if let Some((complete, accumulator)) = guard.slot.take() {
            complete(
                accumulator,
                upstream_end,
                CompletionOutputObservation {
                    monitor_hits,
                    capture_safe,
                },
            );
        }
        buffer_held.clear();
        yield Ok(bytes::Bytes::from(buffer));
        // The upstream ended without its `[DONE]`: the scanned frames went out,
        // and the stream closes on the failure.
        if truncated {
            yield Ok(upstream_failure_frame(&BridgeError::StreamAborted));
        }
    })
}

pub(super) fn build_completion_passthrough_stream<F>(
    upstream: CompletionByteStream,
    output_observer: Option<Arc<aisix_guardrails::GuardrailChain>>,
    upstream_model: String,
    on_complete: F,
) -> impl futures::Stream<Item = Result<bytes::Bytes, BridgeError>>
where
    F: FnOnce(CompletionSseAccumulator, CompletionStreamOutcome, CompletionOutputObservation)
        + Send
        + 'static,
{
    crate::request_id::in_request_span(async_stream::stream! {
        let output_guarded = output_observer.is_some();
        // A monitor-only chain holds nothing back, so no hold cap applies: its
        // end-of-stream scan judges the generated text up to the live bound.
        let accumulator = CompletionSseAccumulator {
            text_cap: output_observer.as_ref().map_or(
                crate::token_estimate::OUTPUT_ACCUMULATION_CAP,
                |chain| crate::guardrail_stream::live_scan_cap(chain.as_ref()),
            ),
            ..CompletionSseAccumulator::default()
        };
        let mut guard = CompletionStreamGuard {
            slot: Some((on_complete, accumulator)),
            drop_outcome: CompletionStreamOutcome::DownstreamDrop,
            output_guarded,
        };
        futures::pin_mut!(upstream);
        let mut outcome = CompletionStreamOutcome::CleanEof;
        while let Some(item) = upstream.next().await {
            let mut wire_len = None;
            match &item {
                Ok(bytes) => {
                    if let Some((_, accumulator)) = guard.slot.as_mut() {
                        wire_len = Some(accumulator.push(bytes));
                    }
                }
                Err(error) => {
                    if let Some((_, accumulator)) = guard.slot.as_mut() {
                        accumulator.finish();
                    }
                    let reported = guard
                        .slot
                        .as_ref()
                        .and_then(|(_, accumulator)| accumulator.in_band_error.as_ref());
                    outcome = match reported {
                        // What the upstream reported before the read failed
                        // is the cause.
                        Some(reported) => CompletionStreamOutcome::in_band(reported),
                        None => CompletionStreamOutcome::UpstreamError {
                            status: error.http_status(),
                            failure: match error {
                                BridgeError::Timeout { .. } => CompletionStreamFailure::Timeout,
                                BridgeError::UpstreamDecode(_) => CompletionStreamFailure::Decode,
                                _ => CompletionStreamFailure::Other,
                            },
                            error_class: error.error_type(),
                        },
                    };
                    guard.drop_outcome = outcome;
                }
            }
            let item = match (item, wire_len) {
                (Ok(bytes), Some(len)) if len < bytes.len() => Ok(bytes.slice(..len)),
                (item, _) => item,
            };
            let reached_done = guard
                .slot
                .as_ref()
                .is_some_and(|(_, accumulator)| accumulator.saw_done);
            if reached_done {
                let malformed = guard
                    .slot
                    .as_ref()
                    .is_some_and(|(_, accumulator)| accumulator.malformed_data);
                if malformed {
                    let failure = CompletionStreamOutcome::UpstreamError {
                        status: 502,
                        failure: CompletionStreamFailure::Decode,
                        error_class: "upstream_decode_error",
                    };
                    guard.drop_outcome = failure;
                    let done_frame_start = guard
                        .slot
                        .as_ref()
                        .map_or(0, |(_, accumulator)| accumulator.done_frame_start);
                    if let Some((complete, accumulator)) = guard.slot.take() {
                        complete(
                            accumulator,
                            failure,
                            CompletionOutputObservation::uninspected(),
                        );
                    }
                    // What came before the `[DONE]` frame in this read is
                    // relayed as it would have been had it arrived on its
                    // own; only the success terminal is withheld. Otherwise
                    // what the caller saw would hang on how the upstream's
                    // writes were coalesced on the way.
                    if let Ok(bytes) = &item {
                        let before_done = done_frame_start.min(bytes.len());
                        if before_done > 0 {
                            yield Ok(bytes.slice(..before_done));
                        }
                    }
                    yield Err(BridgeError::UpstreamDecode(
                        "malformed SSE data before [DONE]".into(),
                    ));
                    return;
                }
                // `[DONE]` closes the stream, but an error the upstream
                // reported before it is how the request ended.
                let terminal = guard
                    .slot
                    .as_ref()
                    .and_then(|(_, accumulator)| accumulator.in_band_error.as_ref())
                    .map_or(CompletionStreamOutcome::CleanEof, CompletionStreamOutcome::in_band);
                guard.drop_outcome = terminal;
                let observation = match output_observer.as_ref() {
                    Some(chain) => {
                        let (text, truncated) = guard
                            .slot
                            .as_ref()
                            .map(|(_, accumulator)| {
                                (accumulator.output_text(), accumulator.text_truncated)
                            })
                            .unwrap_or_default();
                        observe_completion_stream_output(
                            chain.as_ref(),
                            &upstream_model,
                            &text,
                            truncated,
                        )
                        .await
                    }
                    None => CompletionOutputObservation::unguarded(),
                };
                if let Some((complete, accumulator)) = guard.slot.take() {
                    complete(accumulator, terminal, observation);
                }
                yield item;
                return;
            }
            yield item;
            if matches!(outcome, CompletionStreamOutcome::UpstreamError { .. }) {
                break;
            }
        }

        if let Some((_, accumulator)) = guard.slot.as_mut() {
            accumulator.finish();
        }
        if matches!(outcome, CompletionStreamOutcome::CleanEof) {
            let accumulator = &guard
                .slot
                .as_ref()
                .expect("completion stream guard is armed before terminal callback")
                .1;
            if accumulator.malformed_data {
                outcome = CompletionStreamOutcome::UpstreamError {
                    status: 502,
                    failure: CompletionStreamFailure::Decode,
                    error_class: "upstream_decode_error",
                };
            } else if let Some(error) = &accumulator.in_band_error {
                outcome = CompletionStreamOutcome::in_band(error);
            } else if !accumulator.saw_done {
                outcome = CompletionStreamOutcome::UpstreamError {
                    status: 502,
                    failure: CompletionStreamFailure::Other,
                    error_class: "stream_aborted",
                };
            }
        }
        // If the downstream disconnects while a terminal monitor observation
        // awaits, Drop must retain the already-known upstream outcome rather
        // than relabeling it as a client cancellation.
        guard.drop_outcome = outcome;
        let observation = match output_observer.as_ref() {
            Some(chain) => {
                let (text, truncated) = guard
                    .slot
                    .as_ref()
                    .map(|(_, accumulator)| (accumulator.output_text(), accumulator.text_truncated))
                    .unwrap_or_default();
                observe_completion_stream_output(chain.as_ref(), &upstream_model, &text, truncated)
                    .await
            }
            None => CompletionOutputObservation::unguarded(),
        };
        if let Some((complete, accumulator)) = guard.slot.take() {
            complete(accumulator, outcome, observation);
        }
    })
}

async fn observe_completion_stream_output(
    chain: &aisix_guardrails::GuardrailChain,
    upstream_model: &str,
    text: &str,
    // `text` stopped at the live scan's bound, so the rest goes unread.
    truncated: bool,
) -> CompletionOutputObservation {
    if truncated {
        crate::guardrail_stream::record_live_scan_outgrown(chain);
    }
    if text.is_empty() {
        return CompletionOutputObservation {
            monitor_hits: Vec::new(),
            capture_safe: true,
        };
    }
    let mut synth = ChatResponse {
        logprobs: None,
        more_choices: Vec::new(),
        id: String::new(),
        model: upstream_model.to_string(),
        message: ChatMessage::assistant(text.to_string()),
        finish_reason: FinishReason::Stop,
        usage: UsageStats::default(),
        service_tier: None,
    };
    let (verdict, mut hits) =
        aisix_guardrails::Guardrail::check_output_non_segment_observed(chain, &synth).await;
    let mut counts = crate::redact::RedactionCounts::new();
    let moderation = crate::redact::moderate_chat_response_structured(
        chain,
        verdict,
        &mut synth,
        &mut counts,
        &mut hits,
    )
    .await;
    if let aisix_guardrails::GuardrailVerdict::Block { reason, .. } = moderation.verdict {
        tracing::warn!(
            guardrail_hook = "output",
            model = %upstream_model,
            reason = %reason,
            "output guardrail returned block after live completions forward"
        );
    }
    CompletionOutputObservation {
        monitor_hits: hits,
        capture_safe: moderation.capture_safe && !truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MonitorHit;

    #[async_trait::async_trait]
    impl aisix_guardrails::Guardrail for MonitorHit {
        fn name(&self) -> &'static str {
            "monitor-hit"
        }

        async fn check_output_observed(
            &self,
            _resp: &ChatResponse,
        ) -> (
            aisix_guardrails::GuardrailVerdict,
            Vec<aisix_core::GuardrailMonitorHit>,
        ) {
            (
                aisix_guardrails::GuardrailVerdict::Allow,
                vec![aisix_core::GuardrailMonitorHit {
                    guardrail_name: "monitor-hit".to_string(),
                    hook: "output".to_string(),
                    action: "would_block".to_string(),
                    reason: "test observation".to_string(),
                    error_type: String::new(),
                    counts: BTreeMap::new(),
                }],
            )
        }
    }

    fn frame(text: &str, terminated: bool) -> Vec<u8> {
        let suffix = if terminated { "\n\n" } else { "" };
        format!(
            "data: {{\"id\":\"cmpl-1\",\"choices\":[{{\"index\":0,\"text\":{}}}]}}{suffix}",
            serde_json::to_string(text).unwrap()
        )
        .into_bytes()
    }

    /// The usage frame the gateway asked for goes, in whichever pieces the
    /// bytes arrive; every other frame reaches the caller as it was sent.
    #[tokio::test]
    async fn a_caller_that_did_not_ask_is_not_sent_the_usage_frame() {
        let text = "data: {\"choices\":[{\"text\":\"usage\",\"index\":0}]}\n\n";
        let usage = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3}}\n\n";
        let done = "data: [DONE]\n\n";
        let whole = format!("{text}{usage}{done}");
        let expected = format!("{text}{done}");

        for cut in [1, 7, text.len() + 3, whole.len() - 2] {
            let (head, tail) = whole.as_bytes().split_at(cut);
            let parts = vec![
                Ok(bytes::Bytes::copy_from_slice(head)),
                Ok(bytes::Bytes::copy_from_slice(tail)),
            ];
            let out: Vec<_> =
                relay_whole_frames(futures::stream::iter(parts), true, "model".to_string())
                    .collect()
                    .await;
            let relayed: Vec<u8> = out
                .into_iter()
                .flat_map(|item| item.expect("no upstream error").to_vec())
                .collect();
            assert_eq!(relayed, expected.as_bytes(), "cut at {cut}");
        }
    }

    /// A streamed completion names the model the caller addressed, as the
    /// buffered one and every other stream do, whether a frame ends on its
    /// terminator or the upstream ends without one.
    #[tokio::test]
    async fn a_relayed_completion_names_the_model_the_caller_addressed() {
        let frame = |terminated: bool| {
            format!(
                "data: {{\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"model\":\"gpt-3.5-turbo-instruct-0914\",\"choices\":[{{\"index\":0,\"text\":\"hi\"}}]}}{}",
                if terminated { "\n\n" } else { "" }
            )
        };
        let parts = vec![
            Ok(bytes::Bytes::from(frame(true))),
            Ok(bytes::Bytes::from(frame(false))),
        ];
        let out: Vec<_> = relay_whole_frames(
            futures::stream::iter(parts),
            false,
            "my-instruct".to_string(),
        )
        .collect()
        .await;
        let relayed: Vec<u8> = out
            .into_iter()
            .flat_map(|item| item.expect("no upstream error").to_vec())
            .collect();
        let relayed = String::from_utf8(relayed).unwrap();
        assert_eq!(
            relayed.matches(r#""model":"my-instruct""#).count(),
            2,
            "{relayed}"
        );
        assert!(!relayed.contains("0914"), "{relayed}");
    }

    #[test]
    fn eof_observes_an_unterminated_final_frame() {
        let bytes = frame("tail text", false);
        let mut accumulator = CompletionSseAccumulator::default();
        accumulator.push(&bytes);
        assert!(accumulator.output_text().is_empty());
        accumulator.finish();
        assert_eq!(accumulator.output_text(), "tail text");
    }

    #[test]
    fn overflow_estimate_keeps_an_incomplete_trigger_frame() {
        let mut acc = CompletionSseAccumulator::default();
        acc.push(br#"data: {"choices":[{"index":0,"text":"billed trigger"}"#);

        let output = acc.finish_for_overflow_estimate();

        assert!(output.contains("billed trigger"));
        assert!(acc.malformed_data);
    }

    #[test]
    fn security_accumulator_scans_beyond_the_telemetry_cap() {
        let tail = "forbidden-tail";
        let text = format!(
            "{}{}",
            "x".repeat(crate::token_estimate::OUTPUT_ACCUMULATION_CAP + 16),
            tail
        );
        let bytes = frame(&text, true);
        let mut accumulator = CompletionSseAccumulator::with_security_cap(bytes.len());
        accumulator.push(&bytes);
        accumulator.finish();
        assert!(
            accumulator.output_text().ends_with(tail),
            "held output was truncated at the telemetry cap"
        );
    }

    #[test]
    fn accumulator_reassembles_lone_cr_delimiter_across_byte_chunks() {
        let raw = concat!(
            "\u{feff}event: completion\r",
            "data: {\"choices\":\r",
            "data: [{\"index\":0,\"text\":\"split text\"}]}\r\r",
        );
        let mut accumulator = CompletionSseAccumulator::default();
        for byte in raw.as_bytes() {
            accumulator.push(std::slice::from_ref(byte));
        }
        accumulator.finish();
        assert_eq!(accumulator.output_text(), "split text");
        assert!(!accumulator.malformed_data);
    }

    #[test]
    fn accumulator_surfaces_malformed_data_for_held_policy() {
        let mut accumulator = CompletionSseAccumulator::default();
        accumulator.push(b"data: not-json\n\n");
        assert!(accumulator.malformed_data);
        assert!(accumulator.output_text().is_empty());
    }

    #[test]
    fn complete_oversized_frame_never_exceeds_the_frame_cap() {
        let mut accumulator = CompletionSseAccumulator {
            frame_cap: 32,
            ..CompletionSseAccumulator::default()
        };
        accumulator.push(&frame(&"x".repeat(256), true));

        assert!(accumulator.frame_buf.len() <= accumulator.frame_cap);
        assert!(accumulator.malformed_data);
        assert!(accumulator.output_text().is_empty());
    }

    #[test]
    fn excessive_choice_channels_are_rejected_with_a_bounded_map() {
        let choices: Vec<_> = (0..=MAX_COMPLETION_CHOICES)
            .map(|index| serde_json::json!({"index": index, "text": "x"}))
            .collect();
        let bytes = format!(
            "data: {}\n\n",
            serde_json::json!({"id": "cmpl-1", "choices": choices})
        );
        let mut accumulator = CompletionSseAccumulator::default();
        accumulator.push(bytes.as_bytes());

        assert_eq!(accumulator.text_by_choice.len(), MAX_COMPLETION_CHOICES);
        assert!(accumulator.malformed_data);
    }

    #[test]
    fn completion_text_cap_is_global_across_choice_channels() {
        let bytes = format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "cmpl-1",
                "choices": [
                    {"index": 0, "text": "a".repeat(20)},
                    {"index": 1, "text": "b".repeat(20)},
                    {"index": 2, "text": "汉".repeat(20)}
                ]
            })
        );
        let mut accumulator = CompletionSseAccumulator {
            text_cap: 32,
            ..CompletionSseAccumulator::default()
        };
        accumulator.push(bytes.as_bytes());
        let output = accumulator.output_text();

        assert!(output.len() <= accumulator.text_cap);
        assert_eq!(output.len(), accumulator.text_bytes);
    }

    /// 首个非空字节块写下 TTFT，之后的块不再覆盖；空块不算首块。
    #[tokio::test]
    async fn first_non_empty_chunk_stamps_ttft_once() {
        let slot = Arc::new(OnceLock::new());
        let upstream: CompletionByteStream = Box::pin(futures::stream::iter([
            Ok(bytes::Bytes::new()),
            Ok(bytes::Bytes::from(frame("first", true))),
            Ok(bytes::Bytes::from(frame("second", true))),
        ]));
        let mut stream = stamp_first_chunk(upstream, Instant::now(), Arc::clone(&slot));

        assert!(stream.next().await.is_some());
        assert!(
            slot.get().is_none(),
            "an empty chunk is not the first frame"
        );
        assert!(stream.next().await.is_some());
        let first = *slot
            .get()
            .expect("the first non-empty chunk stamps the TTFT");
        while stream.next().await.is_some() {}
        assert_eq!(
            slot.get(),
            Some(&first),
            "later chunks never move the stamp"
        );
    }

    #[tokio::test]
    async fn upstream_item_error_reports_failure_not_clean_eof() {
        let upstream: CompletionByteStream = Box::pin(futures::stream::iter([
            Ok(bytes::Bytes::from(frame("partial", true))),
            Err(BridgeError::StreamAborted),
        ]));
        let outcome = Arc::new(std::sync::Mutex::new(None));
        let outcome_for_callback = Arc::clone(&outcome);
        let stream = build_completion_passthrough_stream(
            upstream,
            None,
            "model".to_string(),
            move |accumulator, terminal, _| {
                assert_eq!(accumulator.output_text(), "partial");
                *outcome_for_callback.lock().unwrap() = Some(terminal);
            },
        );
        futures::pin_mut!(stream);
        while stream.next().await.is_some() {}

        assert_eq!(
            *outcome.lock().unwrap(),
            Some(CompletionStreamOutcome::UpstreamError {
                status: 502,
                failure: CompletionStreamFailure::Other,
                error_class: "stream_aborted",
            })
        );
    }

    #[tokio::test]
    async fn upstream_timeout_preserves_its_usage_error_class() {
        let upstream: CompletionByteStream =
            Box::pin(futures::stream::iter([Err(BridgeError::Timeout {
                elapsed_ms: 300,
                cause: "stream body".into(),
            })]));
        let outcome = Arc::new(std::sync::Mutex::new(None));
        let outcome_for_callback = Arc::clone(&outcome);
        let stream = build_completion_passthrough_stream(
            upstream,
            None,
            "model".to_string(),
            move |_, terminal, _| *outcome_for_callback.lock().unwrap() = Some(terminal),
        );
        futures::pin_mut!(stream);
        while stream.next().await.is_some() {}

        assert_eq!(
            *outcome.lock().unwrap(),
            Some(CompletionStreamOutcome::UpstreamError {
                status: 504,
                failure: CompletionStreamFailure::Timeout,
                error_class: "timeout",
            })
        );
    }

    #[tokio::test]
    async fn eof_without_done_reports_stream_aborted() {
        let upstream: CompletionByteStream = Box::pin(futures::stream::iter([Ok(
            bytes::Bytes::from(frame("partial", true)),
        )]));
        let outcome = Arc::new(std::sync::Mutex::new(None));
        let outcome_for_callback = Arc::clone(&outcome);
        let stream = build_completion_passthrough_stream(
            upstream,
            None,
            "model".to_string(),
            move |_, terminal, _| *outcome_for_callback.lock().unwrap() = Some(terminal),
        );
        futures::pin_mut!(stream);
        while stream.next().await.is_some() {}

        assert_eq!(
            *outcome.lock().unwrap(),
            Some(CompletionStreamOutcome::UpstreamError {
                status: 502,
                failure: CompletionStreamFailure::Other,
                error_class: "stream_aborted",
            })
        );
    }

    #[tokio::test]
    async fn malformed_eof_reports_upstream_decode() {
        let upstream: CompletionByteStream = Box::pin(futures::stream::iter([Ok(
            bytes::Bytes::from_static(b"data: not-json\n\n"),
        )]));
        let outcome = Arc::new(std::sync::Mutex::new(None));
        let outcome_for_callback = Arc::clone(&outcome);
        let stream = build_completion_passthrough_stream(
            upstream,
            None,
            "model".to_string(),
            move |_, terminal, _| *outcome_for_callback.lock().unwrap() = Some(terminal),
        );
        futures::pin_mut!(stream);
        while stream.next().await.is_some() {}

        assert_eq!(
            *outcome.lock().unwrap(),
            Some(CompletionStreamOutcome::UpstreamError {
                status: 502,
                failure: CompletionStreamFailure::Decode,
                error_class: "upstream_decode_error",
            })
        );
    }

    /// What vLLM's completions server writes when generation fails after the
    /// head went out: the error envelope as a data event, then `[DONE]`
    /// (`vllm/entrypoints/openai/completion/serving.py`, the generator's
    /// `except` arm).
    const IN_BAND_ERROR_THEN_DONE: &[u8] = b"data: {\"error\":{\"message\":\"generation failed\",\
\"type\":\"BadRequestError\",\"param\":null,\"code\":400}}\n\ndata: [DONE]\n\n";

    fn in_band_outcome() -> Option<CompletionStreamOutcome> {
        Some(CompletionStreamOutcome::UpstreamError {
            status: 400,
            failure: CompletionStreamFailure::InBand,
            error_class: "upstream_in_band_error",
        })
    }

    /// An error the upstream reported inside the stream ends it as that error,
    /// `[DONE]` after it or not, and keeps the typed error for the row and the
    /// cooldown. The frames still reach the caller as the upstream sent them.
    #[tokio::test]
    async fn an_in_band_error_before_done_is_the_streams_failure() {
        for tail in [
            IN_BAND_ERROR_THEN_DONE,
            &IN_BAND_ERROR_THEN_DONE[..IN_BAND_ERROR_THEN_DONE.len() - 14],
        ] {
            let mut body = frame("partial", true);
            body.extend_from_slice(tail);
            let upstream: CompletionByteStream =
                Box::pin(futures::stream::iter([Ok(bytes::Bytes::from(body))]));
            let outcome = Arc::new(std::sync::Mutex::new(None));
            let outcome_for_callback = Arc::clone(&outcome);
            let stream = build_completion_passthrough_stream(
                upstream,
                None,
                "model".to_string(),
                move |accumulator, terminal, _| {
                    *outcome_for_callback.lock().unwrap() =
                        Some((terminal, accumulator.in_band_error.map(|e| e.to_string())));
                },
            );
            futures::pin_mut!(stream);
            let mut relayed = Vec::new();
            while let Some(item) = stream.next().await {
                relayed.extend_from_slice(&item.unwrap());
            }
            let relayed = String::from_utf8_lossy(&relayed);
            assert!(relayed.contains("generation failed"), "{relayed}");
            let (terminal, error) = outcome.lock().unwrap().take().expect("terminal fired");
            assert_eq!(
                Some(terminal),
                in_band_outcome(),
                "tail {:?}",
                String::from_utf8_lossy(tail)
            );
            assert!(error.is_some_and(|e| e.contains("generation failed")));
        }
    }

    /// A caller that leaves after the upstream reported its failure did not end
    /// the request: the upstream did.
    #[tokio::test]
    async fn a_drop_after_an_in_band_error_keeps_the_upstreams_failure() {
        let mut body = frame("partial", true);
        body.extend_from_slice(&IN_BAND_ERROR_THEN_DONE[..IN_BAND_ERROR_THEN_DONE.len() - 14]);
        let upstream: CompletionByteStream = Box::pin(
            futures::stream::iter([Ok(bytes::Bytes::from(body))]).chain(futures::stream::pending()),
        );
        let outcome = Arc::new(std::sync::Mutex::new(None));
        let outcome_for_callback = Arc::clone(&outcome);
        {
            let stream = build_completion_passthrough_stream(
                upstream,
                None,
                "model".to_string(),
                move |_, terminal, _| *outcome_for_callback.lock().unwrap() = Some(terminal),
            );
            futures::pin_mut!(stream);
            assert!(stream.next().await.unwrap().is_ok());
        }
        assert_eq!(*outcome.lock().unwrap(), in_band_outcome());
    }

    /// The held relay judges the same way: the error and `[DONE]` are released
    /// as the upstream sent them, and the stream ends as the upstream's error.
    #[tokio::test]
    async fn a_held_completion_with_an_in_band_error_ends_as_that_error() {
        struct Holds;
        #[async_trait::async_trait]
        impl aisix_guardrails::Guardrail for Holds {
            fn name(&self) -> &'static str {
                "holds"
            }
            fn stream_output_policy(&self) -> aisix_guardrails::StreamOutputPolicy {
                aisix_guardrails::StreamOutputPolicy::BufferFull {
                    max_buffer_bytes: 1 << 20,
                    on_exceeded_fail_open: false,
                }
            }
        }
        let mut body = frame("partial", true);
        body.extend_from_slice(IN_BAND_ERROR_THEN_DONE);
        let upstream: CompletionByteStream =
            Box::pin(futures::stream::iter([Ok(bytes::Bytes::from(body))]));
        let chain = Arc::new(aisix_guardrails::GuardrailChain::new(vec![
            Arc::new(Holds) as Arc<dyn aisix_guardrails::Guardrail>
        ]));
        let outcome = Arc::new(std::sync::Mutex::new(None));
        let outcome_in_callback = Arc::clone(&outcome);
        let stream = build_held_completion_stream(
            upstream,
            chain,
            "model".to_string(),
            "model".to_string(),
            Instant::now(),
            None,
            None,
            Arc::new(OnceLock::new()),
            move |_, terminal, _| *outcome_in_callback.lock().unwrap() = Some(terminal),
        );
        futures::pin_mut!(stream);
        let mut released = Vec::new();
        while let Some(item) = stream.next().await {
            released.extend_from_slice(&item.unwrap());
        }
        let released = String::from_utf8_lossy(&released);
        assert!(released.contains("generation failed"), "{released}");
        assert_eq!(*outcome.lock().unwrap(), in_band_outcome());
    }

    /// The read that crosses a fail-closed cap can carry the upstream's own
    /// report of a failure. The caller still gets the refusal and nothing held,
    /// and the attempt still ends as the upstream's error, not a clean end.
    #[tokio::test]
    async fn a_fail_closed_overflow_keeps_an_in_band_error_seen_in_the_same_read() {
        struct SmallHold;
        #[async_trait::async_trait]
        impl aisix_guardrails::Guardrail for SmallHold {
            fn name(&self) -> &'static str {
                "small-hold"
            }
            fn stream_output_policy(&self) -> aisix_guardrails::StreamOutputPolicy {
                aisix_guardrails::StreamOutputPolicy::BufferFull {
                    max_buffer_bytes: 8,
                    on_exceeded_fail_open: false,
                }
            }
        }
        for (tail, expected) in [
            (IN_BAND_ERROR_THEN_DONE, in_band_outcome()),
            (
                &b"data: [DONE]\n\n"[..],
                Some(CompletionStreamOutcome::CleanEof),
            ),
        ] {
            let mut body = frame(&"x".repeat(64), true);
            body.extend_from_slice(tail);
            let upstream: CompletionByteStream =
                Box::pin(futures::stream::iter([Ok(bytes::Bytes::from(body))]));
            let chain = Arc::new(aisix_guardrails::GuardrailChain::new(vec![
                Arc::new(SmallHold) as Arc<dyn aisix_guardrails::Guardrail>,
            ]));
            let verdict = Arc::new(OnceLock::new());
            let outcome = Arc::new(std::sync::Mutex::new(None));
            let outcome_in_callback = Arc::clone(&outcome);
            let stream = build_held_completion_stream(
                upstream,
                chain,
                "model".to_string(),
                "model".to_string(),
                Instant::now(),
                None,
                None,
                Arc::clone(&verdict),
                move |accumulator, terminal, _| {
                    *outcome_in_callback.lock().unwrap() =
                        Some((terminal, accumulator.in_band_error.map(|e| e.to_string())));
                },
            );
            futures::pin_mut!(stream);
            let mut released = Vec::new();
            while let Some(item) = stream.next().await {
                released.extend_from_slice(&item.unwrap());
            }
            let released = String::from_utf8_lossy(&released);
            assert!(released.starts_with("event: error\n"), "{released}");
            assert!(!released.contains("xxxx"), "{released}");
            assert!(!released.contains("generation failed"), "{released}");
            assert!(
                verdict.get().and_then(|v| v.refusal.as_ref()).is_some(),
                "the overflow is still the gateway's refusal"
            );
            let (terminal, error) = outcome.lock().unwrap().take().expect("terminal fired");
            assert_eq!(Some(terminal), expected);
            assert_eq!(
                error.is_some_and(|e| e.contains("generation failed")),
                expected == in_band_outcome()
            );
        }
    }

    /// An upstream that pauses inside a frame leaves the heartbeat something
    /// to interrupt. What leaves the relay is whole frames, so the comment
    /// lands between events however the upstream split them.
    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_never_lands_inside_a_relayed_frame() {
        let upstream: CompletionByteStream = Box::pin(async_stream::stream! {
            yield Ok(bytes::Bytes::from_static(b"data: {\"id\":\"cmpl-1\",\"choi"));
            tokio::time::sleep(Duration::from_secs(20)).await;
            yield Ok(bytes::Bytes::from_static(
                b"ces\":[{\"text\":\"hi\",\"index\":0}]}\n\ndata: [DONE]\n\n",
            ));
        });
        let parsed =
            build_completion_passthrough_stream(upstream, None, "model".to_string(), |_, _, _| {});
        let relayed = crate::sse_keepalive::with_heartbeat(
            relay_whole_frames(parsed, false, "model".to_string()),
            Some(Duration::from_secs(15)),
        );
        let mut wire = Vec::new();
        for item in relayed.collect::<Vec<_>>().await {
            wire.extend_from_slice(&item.unwrap());
        }
        let mut decoder = aisix_gateway::sse::SseDecoder::new();
        assert_eq!(
            decoder.feed(wire.as_slice()).unwrap(),
            vec![
                aisix_gateway::sse::SseEvent::Data(
                    r#"{"id":"cmpl-1","choices":[{"text":"hi","index":0}]}"#.to_string()
                ),
                aisix_gateway::sse::SseEvent::Done,
            ],
        );
    }

    /// The malformed frame and `[DONE]` arriving in ONE upstream read must be
    /// relayed the same way as when they arrive in two: everything before the
    /// `[DONE]` frame reaches the caller, then the decode failure. Dropping
    /// the whole read made what the caller saw depend on how the upstream's
    /// writes were coalesced on the way.
    #[tokio::test]
    async fn malformed_data_coalesced_with_done_is_still_relayed() {
        let upstream: CompletionByteStream = Box::pin(futures::stream::iter([
            Ok(bytes::Bytes::from(frame("partial", true))),
            Ok(bytes::Bytes::from_static(
                b"data: not-json\n\ndata: [DONE]\n\n",
            )),
        ]));
        let stream =
            build_completion_passthrough_stream(upstream, None, "model".to_string(), |_, _, _| {});
        futures::pin_mut!(stream);
        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();
        let terminal = stream.next().await.unwrap().unwrap_err();

        assert!(String::from_utf8_lossy(&first).contains("partial"));
        assert_eq!(&second[..], b"data: not-json\n\n");
        assert!(matches!(terminal, BridgeError::UpstreamDecode(_)));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn malformed_data_before_done_reports_decode_without_success_terminal() {
        let upstream: CompletionByteStream = Box::pin(futures::stream::iter([
            Ok(bytes::Bytes::from(frame("partial", true))),
            Ok(bytes::Bytes::from_static(b"data: not-json\n\n")),
            Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n")),
        ]));
        let completed = Arc::new(std::sync::Mutex::new(None));
        let completed_for_callback = Arc::clone(&completed);
        let stream = build_completion_passthrough_stream(
            upstream,
            None,
            "model".to_string(),
            move |accumulator, terminal, _| {
                *completed_for_callback.lock().unwrap() =
                    Some((terminal, accumulator.output_text()));
            },
        );
        futures::pin_mut!(stream);
        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();
        let terminal = stream.next().await.unwrap().unwrap_err();

        assert!(String::from_utf8_lossy(&first).contains("partial"));
        assert!(String::from_utf8_lossy(&second).contains("not-json"));
        assert!(matches!(terminal, BridgeError::UpstreamDecode(_)));
        assert!(stream.next().await.is_none());
        assert_eq!(
            *completed.lock().unwrap(),
            Some((
                CompletionStreamOutcome::UpstreamError {
                    status: 502,
                    failure: CompletionStreamFailure::Decode,
                    error_class: "upstream_decode_error",
                },
                "partial".to_string(),
            ))
        );
    }

    #[tokio::test]
    async fn done_event_completes_before_client_drops_without_polling_upstream_eof() {
        let upstream: CompletionByteStream = Box::pin(
            futures::stream::iter([
                Ok(bytes::Bytes::from(frame("complete", true))),
                Ok(bytes::Bytes::from_static(
                    b"data: [DONE]\r\n\r\ndata: not-json-after-done\n\n",
                )),
            ])
            .chain(futures::stream::pending()),
        );
        let observer = Arc::new(aisix_guardrails::GuardrailChain::new(vec![Arc::new(
            MonitorHit,
        )]));
        let completed = Arc::new(std::sync::Mutex::new(None));
        let completed_for_callback = Arc::clone(&completed);

        {
            let stream = build_completion_passthrough_stream(
                upstream,
                Some(observer),
                "model".to_string(),
                move |accumulator, terminal, hits| {
                    *completed_for_callback.lock().unwrap() =
                        Some((terminal, accumulator.output_text(), hits));
                },
            );
            futures::pin_mut!(stream);
            assert!(stream.next().await.unwrap().is_ok());
            let done = stream.next().await.unwrap().unwrap();
            let done = String::from_utf8_lossy(&done);
            assert!(done.contains("[DONE]"));
            assert!(!done.contains("not-json-after-done"));
            // Deliberately do not poll for EOF. OpenAI SDKs commonly stop as
            // soon as the DONE-bearing item has been consumed.
        }

        let completed = completed.lock().unwrap();
        let (terminal, output, hits) = completed.as_ref().expect("DONE finalizes telemetry");
        assert_eq!(*terminal, CompletionStreamOutcome::CleanEof);
        assert_eq!(output, "complete");
        assert_eq!(hits.monitor_hits.len(), 1);
        assert_eq!(hits.monitor_hits[0].guardrail_name, "monitor-hit");
        assert!(hits.capture_safe);
    }

    /// A held completion ends at its `[DONE]`, as every other relay does, rather
    /// than at an upstream that keeps the body open after it.
    #[tokio::test]
    async fn a_held_completion_is_released_at_its_done() {
        struct Holds;
        #[async_trait::async_trait]
        impl aisix_guardrails::Guardrail for Holds {
            fn name(&self) -> &'static str {
                "holds"
            }
            fn stream_output_policy(&self) -> aisix_guardrails::StreamOutputPolicy {
                aisix_guardrails::StreamOutputPolicy::BufferFull {
                    max_buffer_bytes: 1 << 20,
                    on_exceeded_fail_open: false,
                }
            }
        }
        let mut body = frame("hello", true);
        body.extend_from_slice(b"data: [DONE]\n\ndata: after the end\n\n");
        let upstream: CompletionByteStream = Box::pin(
            futures::stream::iter([Ok(bytes::Bytes::from(body))]).chain(futures::stream::pending()),
        );
        let chain = Arc::new(aisix_guardrails::GuardrailChain::new(vec![
            Arc::new(Holds) as Arc<dyn aisix_guardrails::Guardrail>
        ]));
        let outcome = Arc::new(std::sync::Mutex::new(None));
        let outcome_in_callback = Arc::clone(&outcome);
        let stream = build_held_completion_stream(
            upstream,
            chain,
            "model".to_string(),
            "model".to_string(),
            Instant::now(),
            None,
            None,
            Arc::new(OnceLock::new()),
            move |_, terminal, _| *outcome_in_callback.lock().unwrap() = Some(terminal),
        );
        futures::pin_mut!(stream);
        let released = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("released at its [DONE]")
            .unwrap()
            .unwrap();
        let released = String::from_utf8_lossy(&released);
        assert!(released.ends_with("data: [DONE]\n\n"), "{released}");
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(CompletionStreamOutcome::CleanEof)
        );
    }

    /// Past the live bound the scan reads what was kept, the capture is off,
    /// and the row and the metric's source both say the rest went unread.
    #[tokio::test]
    async fn a_monitored_stream_stops_collecting_at_the_live_bound() {
        struct Capped;
        #[async_trait::async_trait]
        impl aisix_guardrails::Guardrail for Capped {
            fn name(&self) -> &'static str {
                "capped"
            }
            fn output_buffer_bytes(&self) -> usize {
                1
            }
        }
        let audit = std::sync::Arc::new(aisix_guardrails::GuardrailAuditLog::default());
        let chain = Arc::new(
            aisix_guardrails::GuardrailChain::new(vec![
                Arc::new(Capped) as Arc<dyn aisix_guardrails::Guardrail>
            ])
            .with_audit_log(Some(Arc::clone(&audit))),
        );
        let cap = crate::held_content::RAW_HOLD_FACTOR;
        let mut body = frame(&"z".repeat(cap - 1), true);
        body.extend_from_slice(&frame("cut here", true));
        body.extend_from_slice(b"data: [DONE]\n\n");
        let upstream: CompletionByteStream =
            Box::pin(futures::stream::iter([Ok(bytes::Bytes::from(body))]));
        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen_in_callback = Arc::clone(&seen);
        let stream = build_completion_passthrough_stream(
            upstream,
            Some(chain),
            "model".to_string(),
            move |accumulator, _, observation| {
                *seen_in_callback.lock().unwrap() =
                    Some((accumulator.output_text(), observation.capture_safe));
            },
        );
        futures::pin_mut!(stream);
        while stream.next().await.is_some() {}

        let (text, capture_safe) = seen.lock().unwrap().take().expect("on_complete fired");
        assert_eq!(text.len(), cap);
        assert!(text.ends_with('c'), "cut at the bound");
        assert!(!capture_safe);
        assert_eq!(
            audit.bypass_reason().as_deref(),
            Some(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED),
        );
    }

    /// The end-of-stream scan of a monitored stream reads past the bound on the
    /// text kept for token estimation; an unguarded stream keeps that bound.
    #[tokio::test]
    async fn a_monitored_stream_is_scanned_past_the_estimation_cap() {
        const MARKER: &str = "late-marker";
        // Ordinary frames: one past `frame_cap` is dropped whatever the text cap.
        let piece = "z".repeat(1024);
        let pieces = crate::token_estimate::OUTPUT_ACCUMULATION_CAP / piece.len() + 1;
        for (observer, expect_whole) in [
            (
                Some(Arc::new(aisix_guardrails::GuardrailChain::new(vec![
                    Arc::new(MonitorHit) as Arc<dyn aisix_guardrails::Guardrail>,
                ]))),
                true,
            ),
            (None, false),
        ] {
            let mut body = frame(&piece, true).repeat(pieces);
            body.extend_from_slice(&frame(MARKER, true));
            body.extend_from_slice(b"data: [DONE]\n\n");
            let upstream: CompletionByteStream =
                Box::pin(futures::stream::iter([Ok(bytes::Bytes::from(body))]));
            let output = Arc::new(std::sync::Mutex::new(None));
            let output_for_callback = Arc::clone(&output);
            let stream = build_completion_passthrough_stream(
                upstream,
                observer,
                "model".to_string(),
                move |accumulator, _, _| {
                    *output_for_callback.lock().unwrap() = Some(accumulator.output_text());
                },
            );
            futures::pin_mut!(stream);
            while stream.next().await.is_some() {}

            let output = output.lock().unwrap().take().expect("on_complete fired");
            assert_eq!(output.contains(MARKER), expect_whole);
        }
    }
}
