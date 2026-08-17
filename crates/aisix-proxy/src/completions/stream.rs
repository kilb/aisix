use super::{count_completion_prompt, extract_completion_usage, CompletionUsage};
use aisix_gateway::{
    BridgeError, ChatMessage, ChatResponse, CompletionByteStream, FinishReason, UsageStats,
};
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) enum PreparedCompletionStream {
    Buffered(Vec<u8>),
    Live {
        stream: CompletionByteStream,
        /// True when hold-back overflow deliberately switched to wire
        /// fail-open. The uninspected prefix may reach the caller but must
        /// never enter a full-content exporter.
        capture_bypassed: bool,
    },
    BufferExceeded {
        accumulator: CompletionSseAccumulator,
        output_text: String,
    },
    Failed {
        error: BridgeError,
        accumulator: CompletionSseAccumulator,
        output_text: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompletionStreamFailure {
    Timeout,
    Decode,
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
    first_frame: bool,
    pub(super) saw_done: bool,
    pub(super) malformed_data: bool,
    pub(super) usage: Option<CompletionUsage>,
    pub(super) provider_request_id: String,
    observed_bytes: bool,
}

impl Default for CompletionSseAccumulator {
    fn default() -> Self {
        Self {
            frame_buf: Vec::new(),
            boundary: SseBoundaryDetector::default(),
            dropping_oversized_frame: false,
            text_by_choice: BTreeMap::new(),
            text_bytes: 0,
            frame_cap: crate::messages::MAX_SSE_FRAME_BUF_BYTES,
            text_cap: crate::token_estimate::OUTPUT_ACCUMULATION_CAP,
            first_frame: true,
            saw_done: false,
            malformed_data: false,
            usage: None,
            provider_request_id: String::new(),
            observed_bytes: false,
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
        self.observed_bytes |= !bytes.is_empty();
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
                            return offset + 1;
                        }
                    }
                    SseBoundary::EndBeforeByte => {
                        self.finish_frame();
                        if self.saw_done {
                            return offset;
                        }
                        continue;
                    }
                }
                break;
            }
        }
        bytes.len()
    }

    pub(super) fn has_observed_bytes(&self) -> bool {
        self.observed_bytes
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
            self.first_frame = false;
            return;
        }
        let frame = std::mem::take(&mut self.frame_buf);
        self.observe_frame(&frame);
    }

    fn observe_frame(&mut self, frame: &[u8]) {
        let first_frame = self.first_frame;
        let saw_done = crate::redact::is_sse_done_event(frame, first_frame);
        self.first_frame = false;
        if saw_done {
            self.saw_done = true;
            return;
        }
        let (json, malformed) = crate::redact::parse_sse_json_event(frame, first_frame);
        self.malformed_data |= malformed;
        let Some(json) = json else {
            return;
        };
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
                let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(channel) = self.text_by_choice.get_mut(&index) {
                    let appended =
                        push_capped(channel, text, self.text_cap.saturating_sub(self.text_bytes));
                    self.text_bytes += appended;
                    continue;
                }
                if self.text_by_choice.len() >= MAX_COMPLETION_CHOICES {
                    self.malformed_data = true;
                    continue;
                }
                let separator = usize::from(!self.text_by_choice.is_empty());
                let remaining = self.text_cap.saturating_sub(self.text_bytes);
                if remaining <= separator {
                    continue;
                }
                let mut channel = String::new();
                let appended = push_capped(&mut channel, text, remaining - separator);
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
            || crate::redact::parse_sse_json_event(&self.frame_buf, self.first_frame)
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
            complete(accumulator, self.drop_outcome, observation);
        }
    }
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
        let mut guard = CompletionStreamGuard {
            slot: Some((on_complete, CompletionSseAccumulator::default())),
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
                    outcome = CompletionStreamOutcome::UpstreamError {
                        status: error.http_status(),
                        failure: match error {
                            BridgeError::Timeout { .. } => CompletionStreamFailure::Timeout,
                            BridgeError::UpstreamDecode(_) => CompletionStreamFailure::Decode,
                            _ => CompletionStreamFailure::Other,
                        },
                        error_class: error.error_type(),
                    };
                    guard.drop_outcome = outcome;
                    if let Some((_, accumulator)) = guard.slot.as_mut() {
                        accumulator.finish();
                    }
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
                    if let Some((complete, accumulator)) = guard.slot.take() {
                        complete(
                            accumulator,
                            failure,
                            CompletionOutputObservation::uninspected(),
                        );
                    }
                    yield Err(BridgeError::UpstreamDecode(
                        "malformed SSE data before [DONE]".into(),
                    ));
                    return;
                }
                guard.drop_outcome = CompletionStreamOutcome::CleanEof;
                let observation = match output_observer.as_ref() {
                    Some(chain) => {
                        let text = guard
                            .slot
                            .as_ref()
                            .map(|(_, accumulator)| accumulator.output_text())
                            .unwrap_or_default();
                        observe_completion_stream_output(chain.as_ref(), &upstream_model, &text)
                            .await
                    }
                    None => CompletionOutputObservation::unguarded(),
                };
                if let Some((complete, accumulator)) = guard.slot.take() {
                    complete(accumulator, CompletionStreamOutcome::CleanEof, observation);
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
                let text = guard
                    .slot
                    .as_ref()
                    .map(|(_, accumulator)| accumulator.output_text())
                    .unwrap_or_default();
                observe_completion_stream_output(chain.as_ref(), &upstream_model, &text).await
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
) -> CompletionOutputObservation {
    if text.is_empty() {
        return CompletionOutputObservation {
            monitor_hits: Vec::new(),
            capture_safe: true,
        };
    }
    let mut synth = ChatResponse {
        id: String::new(),
        model: upstream_model.to_string(),
        message: ChatMessage::assistant(text.to_string()),
        finish_reason: FinishReason::Stop,
        usage: UsageStats::default(),
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
        capture_safe: moderation.capture_safe,
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
}
