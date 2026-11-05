//! End-of-stream output-guardrail observation for the live-forward
//! streaming paths (AISIX-Cloud#1010).
//!
//! A live-forwarded stream has already put its bytes on the wire, so an
//! output-hook chain can no longer block it — which is exactly why only
//! the chains that *can't* block reach here: a block-capable chain
//! resolves to a hold-back [`aisix_guardrails::StreamOutputPolicy`] and
//! takes its endpoint's buffered branch instead. What remains is the
//! monitor-only chain, whose whole purpose is to observe without changing
//! delivery — so it must still get its scan, and must never hold the
//! stream back to get it.
//!
//! Shared by `/v1/responses` (#808) and the streamed audio transcription
//! relay (#998); both assemble the response text as it streams and hand
//! it here once the upstream ends.
//!
//! The text a live stream keeps for that scan is memory like any other, so
//! every live route bounds it here: [`live_scan_cap`], past which the scan reads
//! what was collected and the request records the rest as unscanned
//! ([`record_live_scan_outgrown`]). A route that assembles into one buffer of
//! its own bounds that buffer by the cap; one that fills several shares a
//! [`LiveScanBudget`].

use std::sync::Arc;

use aisix_gateway::{ChatMessage, ChatResponse, FinishReason, UsageStats};

/// Build the minimal internal `ChatResponse` an output guardrail needs to
/// scan: the assistant text in `message.content`. Only the text is read by
/// `check_output` (via `guardrail_output_text`); the other fields are
/// placeholders and never reach the client.
pub(crate) fn synth_chat_response(model: &str, text: String) -> ChatResponse {
    ChatResponse {
        logprobs: None,
        more_choices: Vec::new(),
        id: String::new(),
        model: model.to_string(),
        message: ChatMessage::assistant(text),
        finish_reason: FinishReason::Stop,
        usage: UsageStats::default(),
        service_tier: None,
    }
}

/// End-of-stream output observation for a live-forward path. Reachable
/// only when the output-hook chain's resolved streaming policy is
/// `EndOfStreamCheck` — today that is exactly the monitor-only chains,
/// which can never block. Runs the same two-phase scan as the buffered
/// branch (blob check + segment pass) so would-block / would-mask hits
/// reach telemetry; the bytes are already on the wire, so a `Block`
/// verdict (unreachable for monitor members) is logged, not enforced.
pub(crate) struct EosOutputScan {
    chain: Arc<aisix_guardrails::GuardrailChain>,
    upstream_model: String,
}

impl EosOutputScan {
    pub(crate) fn new(
        chain: Arc<aisix_guardrails::GuardrailChain>,
        upstream_model: String,
    ) -> Self {
        Self {
            chain,
            upstream_model,
        }
    }

    /// The most generated text the stream keeps for [`Self::observe`].
    pub(crate) fn text_cap(&self) -> usize {
        live_scan_cap(self.chain.as_ref())
    }

    /// The text outgrew [`Self::text_cap`]; see [`record_live_scan_outgrown`].
    pub(crate) fn record_outgrown(&self) {
        record_live_scan_outgrown(self.chain.as_ref());
    }

    /// `local_segments` are the slots the buffered branch's mask walker
    /// would rewrite, when the caller can rebuild them; the local kinds
    /// judge those instead of the flattened text (#1027).
    pub(crate) async fn observe(
        self,
        text: &str,
        local_segments: Option<Vec<aisix_guardrails::ScanSegment>>,
    ) -> Vec<aisix_core::GuardrailMonitorHit> {
        // Everything the stream kept, as chat and `/v1/messages` scan it.
        let synth = synth_chat_response(&self.upstream_model, text.to_string());
        let (verdict, mut hits) = aisix_guardrails::Guardrail::check_output_non_segment_observed(
            self.chain.as_ref(),
            &synth,
        )
        .await;
        // Segment pass (bedrock/lakera/presidio members): offer the flattened
        // text as one segment so monitor-mode segment moderators record their
        // observations too. Masks are suppressed in monitor mode, and nothing
        // could be rewritten anyway — the counts are discarded.
        let mut seg_counts = crate::redact::RedactionCounts::new();
        let verdict = crate::redact::moderate_body_local_given(
            self.chain.as_ref(),
            crate::redact::Direction::Output,
            None,
            verdict,
            &mut seg_counts,
            &mut hits,
            local_segments,
            |g| {
                let _ = g.redact_output_text(text);
                crate::redact::RedactionCounts::new()
            },
        )
        .await
        .verdict;
        if let aisix_guardrails::GuardrailVerdict::Block {
            reason,
            guardrail_name,
            ..
        } = verdict
        {
            tracing::warn!(
                guardrail_name = guardrail_name_field(guardrail_name.as_deref()),
                guardrail_hook = "output",
                model = %self.upstream_model,
                reason = %reason,
                "output guardrail returned a block after live forward; \
                 response already sent (EndOfStreamCheck policy)",
            );
        }
        hits
    }
}

/// The most generated text a live-forwarded stream keeps for its end-of-stream
/// scan: the limit that also caps the raw bytes a held-back stream keeps,
/// [`crate::held_content::RAW_HOLD_FACTOR`] times the chain's `max_buffer_bytes`
/// (32 MiB at the 256 KiB default).
///
/// Not the content cap itself. A monitor-only chain holds nothing back, so
/// nothing about the response waits on this text, and the scan reads all of an
/// ordinary response however long; the bound is what stands between a stream of
/// any length and the gateway's memory.
pub(crate) fn live_scan_cap(chain: &dyn aisix_guardrails::Guardrail) -> usize {
    chain
        .output_buffer_bytes()
        .saturating_mul(crate::held_content::RAW_HOLD_FACTOR)
}

/// Record that a live-forwarded stream outgrew [`live_scan_cap`]: its scan reads
/// what was kept, and the request carries the bypass for the rest. Once per
/// stream, since the metric counts events.
pub(crate) fn record_live_scan_outgrown(chain: &dyn aisix_guardrails::Guardrail) {
    tracing::warn!(
        guardrail_hook = "output",
        max_scan_bytes = live_scan_cap(chain),
        "streamed output outgrew what the end-of-stream scan keeps; the rest goes unscanned",
    );
    chain.record_output_bypass(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED);
}

/// [`live_scan_cap`] shared across the buffers a route assembles a live stream's
/// text into.
pub(crate) struct LiveScanBudget {
    chain: Arc<dyn aisix_guardrails::Guardrail>,
    remaining: usize,
    outgrown: bool,
}

impl LiveScanBudget {
    pub(crate) fn new(chain: Arc<dyn aisix_guardrails::Guardrail>) -> Self {
        let remaining = live_scan_cap(chain.as_ref());
        Self {
            chain,
            remaining,
            outgrown: false,
        }
    }

    /// The whole budget, for a copy that is bounded apart from the text.
    pub(crate) fn cap(&self) -> usize {
        live_scan_cap(self.chain.as_ref())
    }

    /// How much of `text` may be kept, cut on a character boundary.
    pub(crate) fn admit<'t>(&mut self, text: &'t str) -> &'t str {
        if text.len() <= self.remaining {
            self.remaining -= text.len();
            return text;
        }
        let mut cut = self.remaining;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        self.remaining = 0;
        self.note_outgrown();
        &text[..cut]
    }

    /// Whether a piece of `len` bytes that cannot be cut may be kept.
    pub(crate) fn admit_whole(&mut self, len: usize) -> bool {
        if len <= self.remaining {
            self.remaining -= len;
            return true;
        }
        self.remaining = 0;
        self.note_outgrown();
        false
    }

    fn note_outgrown(&mut self) {
        if !self.outgrown {
            self.outgrown = true;
            record_live_scan_outgrown(self.chain.as_ref());
        }
    }

    /// Whether some of the text was left out.
    pub(crate) fn outgrown(&self) -> bool {
        self.outgrown
    }
}

/// Release held-back output unscanned because it outgrew the chain's
/// `max_buffer_bytes` under `on_buffer_exceeded: fail_open`: records the
/// bypass and logs which row's cap was exceeded. Every relay's fail-open
/// branch goes through here; a fail-closed branch logs
/// [`output_buffer_owner`] itself.
pub(crate) fn release_output_buffer_unscanned(chain: &dyn aisix_guardrails::Guardrail) {
    tracing::warn!(
        guardrail_name = output_buffer_owner(chain),
        guardrail_hook = "output",
        max_buffer_bytes = chain.stream_output_policy().hold_cap().map(|(cap, _)| cap),
        fail_open = true,
        "streamed output exceeded the guardrail buffer cap; releasing it without an output scan",
    );
    chain.record_output_bypass(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED);
}

/// The row whose `max_buffer_bytes` held-back output outgrew, as a log
/// field value.
pub(crate) fn output_buffer_owner(
    chain: &dyn aisix_guardrails::Guardrail,
) -> Option<tracing::field::DisplayValue<&str>> {
    guardrail_name_field(chain.output_buffer_owner())
}

/// A guardrail's configured name as the `guardrail_name` log field,
/// rendered like every guardrail log line renders it (unquoted), and
/// omitted when no single guardrail is named.
pub(crate) fn guardrail_name_field(
    name: Option<&str>,
) -> Option<tracing::field::DisplayValue<&str>> {
    name.map(tracing::field::display)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_guardrails::{Guardrail, GuardrailAuditLog, GuardrailChain, GuardrailVerdict};
    use async_trait::async_trait;

    /// A row reading the output with the given `max_buffer_bytes`.
    struct Capped(usize);

    #[async_trait]
    impl Guardrail for Capped {
        fn name(&self) -> &'static str {
            "capped"
        }
        async fn check_input(&self, _req: &aisix_gateway::ChatFormat) -> GuardrailVerdict {
            GuardrailVerdict::Allow
        }
        async fn check_output(&self, _resp: &ChatResponse) -> GuardrailVerdict {
            GuardrailVerdict::Allow
        }
        fn output_buffer_bytes(&self) -> usize {
            self.0
        }
    }

    fn budget_over(cap: usize) -> (LiveScanBudget, Arc<GuardrailAuditLog>) {
        let audit = Arc::new(GuardrailAuditLog::default());
        let chain = GuardrailChain::new(vec![Arc::new(Capped(cap)) as Arc<dyn Guardrail>])
            .with_audit_log(Some(Arc::clone(&audit)));
        (LiveScanBudget::new(Arc::new(chain)), audit)
    }

    /// The bound is the one a held stream's raw bytes answer to, at the chain's
    /// tightest row, and the default reaches 32 MiB.
    #[test]
    fn the_cap_is_the_raw_hold_bound_of_the_tightest_row() {
        let chain = GuardrailChain::new(vec![
            Arc::new(Capped(4096)) as Arc<dyn Guardrail>,
            Arc::new(Capped(1024)) as Arc<dyn Guardrail>,
        ]);
        assert_eq!(
            live_scan_cap(&chain),
            1024 * crate::held_content::RAW_HOLD_FACTOR
        );
        assert_eq!(
            aisix_guardrails::DEFAULT_STREAM_OUTPUT_BUFFER_BYTES
                * crate::held_content::RAW_HOLD_FACTOR,
            32 * 1024 * 1024,
        );
    }

    /// What fits is kept, the crossing piece is cut on a character boundary and
    /// the bypass is recorded, and nothing is kept after that.
    #[test]
    fn a_budget_keeps_what_fits_and_records_the_rest_as_unscanned_once() {
        let (mut budget, audit) = budget_over(1);
        let cap = crate::held_content::RAW_HOLD_FACTOR;
        assert_eq!(budget.admit(&"a".repeat(cap - 2)).len(), cap - 2);
        assert!(!budget.outgrown());
        assert_eq!(audit.bypass_reason(), None);

        // `é` is two bytes: one byte of room cannot hold half of it.
        assert_eq!(budget.admit("bé"), "b");
        assert!(budget.outgrown());
        assert_eq!(
            audit.bypass_reason().as_deref(),
            Some(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED),
        );
        assert_eq!(budget.admit("more"), "");
        assert!(!budget.admit_whole(1));
    }

    #[test]
    fn a_piece_that_cannot_be_cut_is_left_out_whole() {
        let (mut budget, audit) = budget_over(1);
        let cap = crate::held_content::RAW_HOLD_FACTOR;
        assert!(budget.admit_whole(cap));
        assert!(!budget.outgrown());
        assert!(!budget.admit_whole(1));
        assert!(budget.outgrown());
        assert_eq!(
            audit.bypass_reason().as_deref(),
            Some(crate::error::TAG_OUTPUT_BUFFER_EXCEEDED),
        );
    }
}
