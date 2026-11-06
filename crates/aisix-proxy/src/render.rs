//! Render the gateway's normalised `ChatResponse` / `ChatChunk` into the
//! OpenAI wire shape that clients expect on `/v1/chat/completions`.
//!
//! The structure is intentionally independent from the provider crates'
//! upstream types — those describe what we *received*, while these
//! describe what we *emit*. Keeping them separate means a client-facing
//! schema change doesn't ripple into every provider adapter.
//!
//! ## `model` field contract (#410)
//!
//! `response.model` is always the **customer-facing model name** the
//! caller put on the request — for a direct model that's the model's
//! display name; for a routing group that's the group name. It is
//! never the upstream provider's raw id (e.g. `gpt-4o`) and never the
//! routing target's display name. This keeps two contracts stable:
//!
//! 1. **Symmetric with direct models.** A request to `failover-group-X`
//!    sees `model: "failover-group-X"` back, exactly the same shape as
//!    a request to a direct model already produced before this PR.
//! 2. **Failover-stable and provider-agnostic.** Customer dashboards
//!    keyed on `response.model` don't flap when a target rotates out,
//!    and a cross-provider routing group never leaks `gpt-4o` vs
//!    `claude-3-5-sonnet` into the client's vocabulary.
//!
//! When the caller wants to know *which* target actually served the
//! request (cost attribution, "did my failover fire?", A/B analysis),
//! the proxy emits an `x-aisix-served-by` response header carrying
//! the winning target's display name, when `proxy.expose_debug_headers`
//! is enabled. See `chat::chat_completions`.

use aisix_gateway::{ChatChunk, ChatResponse, FinishReason, Role};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ChatCompletion {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<NonStreamChoice>,
    pub usage: Usage,
    /// The upstream's own `service_tier`, passed through only from an
    /// OpenAI-shape upstream and omitted when it sent none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NonStreamChoice {
    pub index: u32,
    pub message: RenderedMessage,
    pub finish_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct RenderedMessage {
    pub role: &'static str,
    /// `Option<String>` — NOT `#[serde(skip_serializing_if)]`. OpenAI
    /// emits an explicit `"content": null` on a `tool_calls` response
    /// (the assistant chose to call a tool, there is no text reply), so
    /// `None` MUST serialize as JSON `null`, not be omitted (#395).
    /// Contrast the streaming `RenderedDelta.content`, which correctly
    /// uses `skip_serializing_if` because OpenAI omits `delta.content`
    /// on tool-call chunks. An upstream that answered with an array of
    /// content parts has them relayed as the array.
    pub content: Option<RenderedContent>,
    /// Forward-compatible bag for OpenAI message-level fields the
    /// gateway doesn't model directly on `ChatMessage` (e.g.
    /// `tool_calls` for cross-provider tool-use translation,
    /// `refusal` for OpenAI's safety-classifier output, `audio` for
    /// realtime/4o audio models). Bridges populate this on the way
    /// back from the upstream; serde flatten emits each entry as a
    /// top-level field on the wire so OpenAI SDK clients see the
    /// standard shape.
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RenderedContent {
    Text(String),
    Parts(Vec<serde_json::Value>),
}

impl RenderedMessage {
    fn from_message(message: aisix_gateway::ChatMessage) -> Self {
        let content = match (message.content, message.content_blocks) {
            (Some(text), _) => Some(RenderedContent::Text(text)),
            (None, Some(parts)) => Some(RenderedContent::Parts(parts)),
            (None, None) => None,
        };
        Self {
            role: role_to_str(message.role),
            content,
            // Forward bridge-populated fields (`tool_calls`, `refusal`,
            // `annotations`, `audio`, …) through to the caller.
            extra: message.extra,
        }
    }
}

#[derive(Debug, Serialize, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// OpenAI-canonical prompt-token breakdown. Each counter is emitted
    /// when the upstream reported it — a reported `0` included, so an
    /// OpenAI-shape upstream's details reach the caller as it sent them —
    /// and the whole object is omitted when nothing was reported, so
    /// callers that branch on the field's presence behave like they do
    /// against api.openai.com. Bridged upstreams fill it through their
    /// existing mappings (`cached_tokens` from an Anthropic / Bedrock
    /// cache read or DeepSeek's native hit count, `cache_creation_tokens`
    /// from a cache write); nothing is invented for one that reports none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// OpenAI-canonical completion-token breakdown, under the same rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    /// DeepSeek-native cache-hit counter, passed through verbatim
    /// (#542/#465) alongside the normalized `prompt_tokens_details`
    /// above so DeepSeek-aware clients reading the native field name
    /// still work. Omitted for upstreams that don't emit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit_tokens: Option<u32>,
    /// DeepSeek-native cache-miss counter, passed through verbatim
    /// (#542/#465).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_miss_tokens: Option<u32>,
}

#[derive(Debug, Serialize, Default)]
pub struct PromptTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tokens: Option<u32>,
    /// Raw OpenAI cache-write counter, kept separate from additive cache creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
    /// Cache creation reported by an Anthropic-shape upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
}

#[derive(Debug, Serialize, Default)]
pub struct CompletionTokensDetails {
    /// Reasoning tokens (subset of completion).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_prediction_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_prediction_tokens: Option<u32>,
}

impl PromptTokensDetails {
    fn is_empty(&self) -> bool {
        self.cached_tokens.is_none()
            && self.audio_tokens.is_none()
            && self.text_tokens.is_none()
            && self.image_tokens.is_none()
            && self.cache_write_tokens.is_none()
            && self.cache_creation_tokens.is_none()
    }
}

impl CompletionTokensDetails {
    fn is_empty(&self) -> bool {
        self.reasoning_tokens.is_none()
            && self.audio_tokens.is_none()
            && self.text_tokens.is_none()
            && self.image_tokens.is_none()
            && self.accepted_prediction_tokens.is_none()
            && self.rejected_prediction_tokens.is_none()
    }
}

impl Usage {
    /// Build the client-facing usage envelope from the gateway's
    /// canonical [`UsageStats`]. Centralises the normalize + native-
    /// passthrough policy (#542) so the non-streaming and streaming
    /// renderers stay in lock-step:
    ///
    /// - the input/total pair is PROJECTED into OpenAI accounting
    ///   (`UsageStats::openai_*`), never copied: an Anthropic-shape
    ///   upstream stores its cache counters BESIDE `prompt_tokens`,
    ///   and copying them through handed an OpenAI client a
    ///   `prompt_tokens` short of the input it paid for, with
    ///   `total_tokens != prompt + completion` (AISIX-Cloud#1447)
    /// - each `*_tokens_details` counter the upstream reported is emitted,
    ///   a reported `0` included; one it did not report is omitted, and a
    ///   details object with nothing in it is omitted entirely
    /// - `cached_tokens` is the projected hit (OpenAI's nested field,
    ///   DeepSeek's normalized native one, or an Anthropic / Bedrock
    ///   `cache_read_input_tokens`), emitted beside any cache write too
    /// - a cache WRITE is billed input, so it lands in `prompt_tokens`
    ///   and is named beside the hit rather than left as an unexplained
    ///   increase the caller cannot price
    /// - DeepSeek-native `prompt_cache_hit_tokens` /
    ///   `prompt_cache_miss_tokens` passed through verbatim when the
    ///   upstream sent them (Some), omitted otherwise
    fn from_stats(u: &aisix_gateway::UsageStats) -> Self {
        let reported = &u.openai_details;
        let cached_tokens = u.openai_cached_tokens();
        let cache_creation_tokens = u.anthropic_cache_creation_input_tokens();
        let cache_detail = cached_tokens > 0
            || reported.cached_tokens_reported
            || cache_creation_tokens > 0
            || u.cache_write_tokens.is_some();
        let prompt = PromptTokensDetails {
            cached_tokens: cache_detail.then_some(cached_tokens),
            audio_tokens: u.modality.prompt_audio_tokens,
            text_tokens: reported.prompt_text_tokens,
            image_tokens: u.modality.prompt_image_tokens,
            cache_write_tokens: u.cache_write_tokens,
            cache_creation_tokens: (cache_creation_tokens > 0).then_some(cache_creation_tokens),
        };
        let completion = CompletionTokensDetails {
            reasoning_tokens: (u.reasoning_tokens > 0 || reported.reasoning_tokens_reported)
                .then_some(u.reasoning_tokens),
            audio_tokens: u.modality.completion_audio_tokens,
            text_tokens: reported.completion_text_tokens,
            image_tokens: u.modality.completion_image_tokens,
            accepted_prediction_tokens: reported.accepted_prediction_tokens,
            rejected_prediction_tokens: reported.rejected_prediction_tokens,
        };
        Usage {
            prompt_tokens: u.openai_prompt_tokens(),
            completion_tokens: u.completion_tokens,
            total_tokens: u.openai_total_tokens(),
            prompt_tokens_details: (!prompt.is_empty()).then_some(prompt),
            completion_tokens_details: (!completion.is_empty()).then_some(completion),
            prompt_cache_hit_tokens: u.prompt_cache_hit_tokens,
            prompt_cache_miss_tokens: u.prompt_cache_miss_tokens,
        }
    }

    /// [`Self::from_stats`] in platform mode: OpenAI's own fields only. The
    /// DeepSeek-native pair and the Anthropic-shape cache write would name the
    /// upstream's vendor; their numbers are already in `prompt_tokens` and
    /// `prompt_tokens_details.cached_tokens`.
    fn for_client(u: &aisix_gateway::UsageStats, platform: bool) -> Self {
        let mut usage = Self::from_stats(u);
        if platform {
            usage.prompt_cache_hit_tokens = None;
            usage.prompt_cache_miss_tokens = None;
            if let Some(details) = usage.prompt_tokens_details.as_mut() {
                details.cache_creation_tokens = None;
            }
            usage.prompt_tokens_details = usage.prompt_tokens_details.filter(|d| !d.is_empty());
        }
        usage
    }
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// See [`ChatCompletion::service_tier`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: RenderedDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Default)]
pub struct RenderedDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<serde_json::Value>,
    /// Reasoning text (DeepSeek-canonical `delta.reasoning_content`).
    /// Surfaced when the bridge applied
    /// [`response.reasoning_field`](aisix_core::ResponseOverrides::reasoning_field)
    /// — issue #302 §5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// OpenAI's `delta.audio`, as the upstream streamed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<serde_json::Value>,
    /// Every other delta field the upstream streamed (`refusal`, the
    /// legacy `function_call`, …).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn generated_completion_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
}

/// The id a stream's frames carry: the first id the upstream reports, from that
/// frame on, whatever later frames say. Empty frames ahead of it share one
/// generated id, and so does a whole stream with no upstream id. A generated id
/// is minted here rather than in a provider bridge because `ChatChunk::id` also
/// feeds the usage event's `provider_request_id`, which must stay the upstream's
/// own. In platform mode every frame carries the one generated id instead.
pub(crate) struct StreamId {
    upstream: Option<String>,
    generated: Option<String>,
    opaque: bool,
}

impl StreamId {
    /// `opaque`: platform mode, where no upstream id reaches the client.
    pub(crate) fn new(opaque: bool) -> Self {
        Self {
            upstream: None,
            generated: None,
            opaque,
        }
    }

    pub(crate) fn stamp(&mut self, id: &mut String) {
        if self.opaque {
            id.clone_from(self.generated.get_or_insert_with(|| {
                crate::opaque_id::mint_response_id(crate::opaque_id::Surface::Chat)
            }));
        } else if let Some(upstream) = &self.upstream {
            id.clone_from(upstream);
        } else if id.is_empty() {
            id.clone_from(self.generated.get_or_insert_with(generated_completion_id));
        } else {
            self.upstream = Some(id.clone());
        }
    }
}

/// `platform`: platform mode, where the response id is minted, each tool-call
/// id is mapped ([`crate::opaque_id`]) and usage keeps to OpenAI's fields.
pub fn render_response(
    created_unix_ts: i64,
    resp: ChatResponse,
    client_facing_model: &str,
    platform: Option<&crate::platform::PlatformSettings>,
) -> ChatCompletion {
    let message = |mut message: aisix_gateway::ChatMessage| {
        if let (Some(p), Some(serde_json::Value::Array(calls))) =
            (platform, message.extra.get_mut("tool_calls"))
        {
            crate::opaque_id::map_tool_calls(&p.id_key, crate::opaque_id::Surface::Chat, calls);
        }
        RenderedMessage::from_message(message)
    };
    // `resp.model` is whatever the upstream returned (`gpt-4o`,
    // `claude-3-5-sonnet-20241022`, etc.). The wire-level contract is
    // to echo the customer's requested name — see module-level docs.
    ChatCompletion {
        id: if platform.is_some() {
            crate::opaque_id::mint_response_id(crate::opaque_id::Surface::Chat)
        } else if resp.id.is_empty() {
            generated_completion_id()
        } else {
            resp.id
        },
        object: "chat.completion",
        created: created_unix_ts,
        model: client_facing_model.to_string(),
        choices: std::iter::once(NonStreamChoice {
            index: 0,
            message: message(resp.message),
            finish_reason: finish_to_str(&resp.finish_reason).to_string(),
            logprobs: resp.logprobs,
        })
        .chain(resp.more_choices.into_iter().map(|c| NonStreamChoice {
            index: c.index,
            message: message(c.message),
            finish_reason: finish_to_str(&c.finish_reason).to_string(),
            logprobs: c.logprobs,
        }))
        .collect(),
        usage: Usage::for_client(&resp.usage, platform.is_some()),
        service_tier: openai_service_tier(resp.service_tier.as_ref()),
    }
}

/// `platform` as for [`render_response`]; the frame id itself is the
/// caller's, stamped through [`StreamId`].
pub fn render_chunk(
    created_unix_ts: i64,
    mut chunk: ChatChunk,
    client_facing_model: &str,
    platform: Option<&crate::platform::PlatformSettings>,
) -> ChatCompletionChunk {
    if let (Some(p), Some(calls)) = (platform, chunk.delta.tool_calls.as_mut()) {
        crate::opaque_id::map_tool_calls(&p.id_key, crate::opaque_id::Surface::Chat, calls);
    }
    // A frame that carries only usage is the `include_usage` terminal
    // frame, which OpenAI sends with an empty `choices` array.
    let usage_only = chunk.is_usage_only();
    let choices = if usage_only {
        Vec::new()
    } else {
        vec![StreamChoice {
            index: chunk.choice_index,
            delta: RenderedDelta {
                role: chunk.delta.role.map(role_to_str),
                content: chunk.delta.content,
                tool_calls: chunk.delta.tool_calls,
                function_call: chunk.delta.function_call,
                reasoning_content: chunk.delta.reasoning_content,
                audio: chunk.delta.audio,
                extra: chunk.delta.extra,
            },
            finish_reason: chunk
                .finish_reason
                .as_ref()
                .map(|f| finish_to_str(f).to_string()),
            logprobs: chunk.logprobs,
        }]
    };
    // Same contract as `render_response`: every chunk's `model` field
    // carries the customer's requested name, not the upstream's raw id.
    // Re-stamping has to happen per-chunk; missing one chunk leaks the
    // upstream id mid-stream.
    ChatCompletionChunk {
        id: chunk.id,
        object: "chat.completion.chunk",
        created: created_unix_ts,
        model: client_facing_model.to_string(),
        choices,
        usage: chunk
            .usage
            .as_ref()
            .map(|u| Usage::for_client(u, platform.is_some())),
        service_tier: openai_service_tier(chunk.service_tier.as_ref()),
    }
}

fn openai_service_tier(tier: Option<&aisix_gateway::ServiceTier>) -> Option<String> {
    tier.and_then(|t| t.openai_value()).map(str::to_owned)
}

/// Inject the `x-ratelimit-*` response headers that OpenAI SDK clients
/// read for back-pressure / progress reporting.
///
/// Only headers with a configured limit (non-`None`) are injected;
/// endpoints or keys that have no limit set don't emit anything — the
/// client should not assume absence means unlimited when it sees nothing.
pub fn inject_ratelimit_headers(
    response: &mut axum::response::Response,
    status: &aisix_ratelimit::RateLimitStatus,
) {
    use axum::http::HeaderValue;

    let headers = response.headers_mut();

    macro_rules! set_header {
        ($name:expr, $value:expr) => {
            if let Ok(v) = HeaderValue::try_from($value.to_string()) {
                headers.insert($name, v);
            }
        };
    }

    if let Some(lim) = status.rpm_limit {
        set_header!("x-ratelimit-limit-requests", lim);
        set_header!(
            "x-ratelimit-remaining-requests",
            status.rpm_remaining().unwrap_or(0)
        );
        set_header!(
            "x-ratelimit-reset-requests",
            format!("{}s", status.rpm_reset_secs)
        );
    }

    if let Some(lim) = status.tpm_limit {
        set_header!("x-ratelimit-limit-tokens", lim);
        set_header!(
            "x-ratelimit-remaining-tokens",
            status.tpm_remaining().unwrap_or(0)
        );
        set_header!(
            "x-ratelimit-reset-tokens",
            format!("{}s", status.tpm_reset_secs)
        );
    }

    if let Some(lim) = status.concurrency_limit {
        set_header!("x-ratelimit-limit-concurrent", lim);
        set_header!(
            "x-ratelimit-remaining-concurrent",
            lim.saturating_sub(status.in_flight)
        );
    }
}

fn role_to_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Function => "function",
    }
}

fn finish_to_str(f: &FinishReason) -> &str {
    match f {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Other(s) => s.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_gateway::{ChatMessage, UsageStats};

    #[test]
    fn render_response_matches_openai_shape() {
        let r = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "cmpl-1".into(),
            model: "m".into(),
            message: ChatMessage::assistant("hello"),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::new(3, 2),
            service_tier: None,
        };
        let out = render_response(42, r, "m", None);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["created"], 42);
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert_eq!(json["choices"][0]["message"]["content"], "hello");
        assert_eq!(json["usage"]["total_tokens"], 5);
        // No cache / reasoning / native fields when the upstream
        // reported none — the nested objects must be ABSENT, not
        // emitted as empty `{}` (#542). OpenAI SDK clients branch on
        // presence.
        assert!(
            json["usage"].get("prompt_tokens_details").is_none(),
            "prompt_tokens_details must be omitted when no cache hit"
        );
        assert!(json["usage"].get("completion_tokens_details").is_none());
        assert!(json["usage"].get("prompt_cache_hit_tokens").is_none());
    }

    #[test]
    fn service_tier_passes_through_only_from_an_openai_shape_upstream() {
        use aisix_gateway::ServiceTier;
        let resp = |tier| ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "c".into(),
            model: "m".into(),
            message: ChatMessage::assistant("hi"),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::new(1, 1),
            service_tier: tier,
        };
        let body = |tier| serde_json::to_value(render_response(0, resp(tier), "m", None)).unwrap();
        assert_eq!(
            body(Some(ServiceTier::OpenAi("priority".into())))["service_tier"],
            "priority"
        );
        assert!(body(None).get("service_tier").is_none());
        // Another protocol's vocabulary is never written into this body.
        assert!(body(Some(ServiceTier::Native("standard".into())))
            .get("service_tier")
            .is_none());

        let chunk = |tier| ChatChunk {
            choice_index: 0,
            logprobs: None,
            id: "c".into(),
            model: "m".into(),
            delta: Default::default(),
            finish_reason: None,
            usage: None,
            service_tier: tier,
            accounting_usage: None,
        };
        let frame = |tier| serde_json::to_value(render_chunk(0, chunk(tier), "m", None)).unwrap();
        assert_eq!(
            frame(Some(ServiceTier::OpenAi("flex".into())))["service_tier"],
            "flex"
        );
        assert!(frame(None).get("service_tier").is_none());
        assert!(frame(Some(ServiceTier::Native("fast".into())))
            .get("service_tier")
            .is_none());
    }

    /// #395: a tool_calls response carries `content: null` from the
    /// upstream. The renderer MUST emit an explicit JSON `null` (the
    /// OpenAI documented shape) — not `""` and not an omitted field.
    #[test]
    fn render_response_emits_explicit_null_content_on_tool_calls() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "tool_calls".into(),
            serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": "{}"}
            }]),
        );
        let message = ChatMessage {
            role: Role::Assistant,
            content: None,
            content_blocks: None,
            name: None,
            tool_call_id: None,
            extra,
        };
        let r = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "cmpl-1".into(),
            model: "m".into(),
            message,
            finish_reason: FinishReason::ToolCalls,
            usage: UsageStats::new(3, 2),
            service_tier: None,
        };
        let out = render_response(0, r, "m", None);

        // Serialized JSON must contain `"content":null`, not `""` and
        // not an omitted key.
        let raw = serde_json::to_string(&out).unwrap();
        assert!(
            raw.contains("\"content\":null"),
            "tool_calls content must serialize as explicit null: {raw}"
        );
        assert!(!raw.contains("\"content\":\"\""));

        let json = serde_json::to_value(&out).unwrap();
        let msg = &json["choices"][0]["message"];
        assert!(msg.get("content").is_some(), "content key must be present");
        assert!(msg["content"].is_null());
        assert_eq!(msg["tool_calls"][0]["id"], "call_1");
        assert_eq!(json["choices"][0]["finish_reason"], "tool_calls");
    }

    /// Issue #542 (OpenAI half): when the gateway has a non-zero
    /// cache-hit / reasoning count, the client response must carry the
    /// OpenAI-canonical nested breakdown objects (pre-fix these were
    /// silently dropped by the renderer).
    #[test]
    fn render_response_emits_openai_canonical_usage_details() {
        let usage = UsageStats {
            prompt_tokens: 1000,
            completion_tokens: 500,
            total_tokens: 1500,
            cached_prompt_tokens: 800,
            reasoning_tokens: 200,
            ..UsageStats::default()
        };
        let r = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "cmpl-1".into(),
            model: "m".into(),
            message: ChatMessage::assistant("hi"),
            finish_reason: FinishReason::Stop,
            usage,
            service_tier: None,
        };
        let json = serde_json::to_value(render_response(0, r, "m", None)).unwrap();
        assert_eq!(json["usage"]["prompt_tokens_details"]["cached_tokens"], 800);
        assert_eq!(
            json["usage"]["completion_tokens_details"]["reasoning_tokens"],
            200
        );
        // OpenAI upstream → no DeepSeek-native fields.
        assert!(json["usage"].get("prompt_cache_hit_tokens").is_none());
    }

    /// Issue #542 (DeepSeek half, hybrid): the response must carry BOTH
    /// the normalized OpenAI-shape `prompt_tokens_details.cached_tokens`
    /// AND the DeepSeek-native `prompt_cache_hit_tokens` /
    /// `prompt_cache_miss_tokens` passed through verbatim. Matches the
    /// ecosystem's hybrid so clients reading either field name work.
    #[test]
    fn render_response_emits_deepseek_native_and_normalized_cache() {
        let usage = UsageStats {
            prompt_tokens: 1000,
            completion_tokens: 100,
            total_tokens: 1100,
            // Normalized from DeepSeek's prompt_cache_hit_tokens by the
            // bridge.
            cached_prompt_tokens: 768,
            prompt_cache_hit_tokens: Some(768),
            prompt_cache_miss_tokens: Some(232),
            ..UsageStats::default()
        };
        let r = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "cmpl-ds".into(),
            model: "deepseek-chat".into(),
            message: ChatMessage::assistant("hi"),
            finish_reason: FinishReason::Stop,
            usage,
            service_tier: None,
        };
        let json = serde_json::to_value(render_response(0, r, "ds-alias", None)).unwrap();
        // Normalized OpenAI-canonical shape
        assert_eq!(json["usage"]["prompt_tokens_details"]["cached_tokens"], 768);
        // Native passthrough
        assert_eq!(json["usage"]["prompt_cache_hit_tokens"], 768);
        assert_eq!(json["usage"]["prompt_cache_miss_tokens"], 232);
    }

    /// Issue #542: the streaming renderer (`render_chunk`) must apply
    /// the same usage policy as `render_response` — the terminal chunk's
    /// usage carries the nested details + native passthrough. A
    /// regression that only fixed the non-streaming path would let
    /// streaming DeepSeek/OpenAI clients lose cache visibility.
    #[test]
    fn render_chunk_emits_usage_details_on_terminal_chunk() {
        let usage = UsageStats {
            prompt_tokens: 50,
            completion_tokens: 10,
            total_tokens: 60,
            cached_prompt_tokens: 32,
            prompt_cache_hit_tokens: Some(32),
            prompt_cache_miss_tokens: Some(18),
            ..UsageStats::default()
        };
        let chunk = ChatChunk {
            choice_index: 0,
            logprobs: None,
            id: "c".into(),
            model: "m".into(),
            delta: aisix_gateway::ChatDelta {
                extra: Default::default(),
                role: None,
                content: None,
                tool_calls: None,
                function_call: None,
                reasoning_content: None,
                audio: None,
            },
            finish_reason: Some(FinishReason::Stop),
            usage: Some(usage),
            service_tier: None,
            accounting_usage: None,
        };
        let json = serde_json::to_value(render_chunk(0, chunk, "m", None)).unwrap();
        assert_eq!(json["usage"]["prompt_tokens_details"]["cached_tokens"], 32);
        assert_eq!(json["usage"]["prompt_cache_hit_tokens"], 32);
        assert_eq!(json["usage"]["prompt_cache_miss_tokens"], 18);
    }

    /// Pins the #410 contract: when the upstream returns one
    /// model id but the customer requested a different name (alias /
    /// routing-group name), `response.model` must echo the customer's
    /// requested name, not the upstream's raw id.
    #[test]
    fn render_response_uses_client_facing_model_not_upstream_raw() {
        let r = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "cmpl-1".into(),
            // Upstream raw — e.g. what OpenAI returned for a routing
            // target inside `failover-group-XXX`.
            model: "gpt-4o".into(),
            message: ChatMessage::assistant("hi"),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::default(),
            service_tier: None,
        };
        let out = render_response(0, r, "failover-group-XXX", None);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(
            json["model"], "failover-group-XXX",
            "wire `model` echoes the customer's requested name"
        );
    }

    #[test]
    fn render_chunk_omits_finish_reason_when_absent() {
        let chunk = ChatChunk {
            choice_index: 0,
            logprobs: None,
            id: "c".into(),
            model: "m".into(),
            delta: aisix_gateway::ChatDelta {
                extra: Default::default(),
                role: None,
                content: Some("hi".into()),
                tool_calls: None,
                function_call: None,
                reasoning_content: None,
                audio: None,
            },
            finish_reason: None,
            usage: None,
            service_tier: None,
            accounting_usage: None,
        };
        let out = render_chunk(1, chunk, "m", None);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["object"], "chat.completion.chunk");
        assert_eq!(json["choices"][0]["delta"]["content"], "hi");
        // finish_reason / usage must be absent (not null).
        assert!(json["choices"][0].get("finish_reason").is_none());
        assert!(json.get("usage").is_none());
    }

    /// Streaming counterpart of the #410 contract — re-stamp the
    /// client-facing name onto every chunk, not just the first.
    #[test]
    fn render_chunk_uses_client_facing_model_not_upstream_raw() {
        let chunk = ChatChunk {
            choice_index: 0,
            logprobs: None,
            id: "c".into(),
            model: "gpt-4o".into(),
            delta: aisix_gateway::ChatDelta {
                extra: Default::default(),
                role: None,
                content: Some("hi".into()),
                tool_calls: None,
                function_call: None,
                reasoning_content: None,
                audio: None,
            },
            finish_reason: None,
            usage: None,
            service_tier: None,
            accounting_usage: None,
        };
        let out = render_chunk(0, chunk, "failover-group-XXX", None);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["model"], "failover-group-XXX");
    }

    fn upstream_chunk(id: &str, tool_call_id: Option<&str>) -> ChatChunk {
        ChatChunk {
            choice_index: 0,
            logprobs: None,
            id: id.into(),
            model: "m".into(),
            delta: aisix_gateway::ChatDelta {
                tool_calls: tool_call_id.map(|tid| {
                    vec![serde_json::json!({
                        "index": 0, "id": tid, "type": "function",
                        "function": {"name": "f", "arguments": ""}
                    })]
                }),
                ..Default::default()
            },
            finish_reason: None,
            usage: None,
            service_tier: None,
            accounting_usage: None,
        }
    }

    fn deepseek_and_anthropic_extended_usage() -> UsageStats {
        UsageStats {
            prompt_tokens: 50,
            completion_tokens: 10,
            total_tokens: 60,
            cached_prompt_tokens: 32,
            cache_creation_tokens: 4,
            prompt_cache_hit_tokens: Some(32),
            prompt_cache_miss_tokens: Some(18),
            ..UsageStats::default()
        }
    }

    #[test]
    fn every_frame_of_a_stream_carries_one_opaque_id() {
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let mut stream_id = StreamId::new(true);
        // The upstream's id arrives on the second frame, as some do.
        let ids: Vec<String> = ["", "chatcmpl-up", "chatcmpl-up"]
            .into_iter()
            .map(|upstream| {
                let mut chunk = upstream_chunk(upstream, None);
                stream_id.stamp(&mut chunk.id);
                let frame = serde_json::to_value(render_chunk(0, chunk, "m", Some(&platform)));
                frame.unwrap()["id"].as_str().unwrap().to_owned()
            })
            .collect();
        assert!(ids.iter().all(|id| id == &ids[0]), "{ids:?}");
        assert!(ids[0].starts_with("chatcmpl-"), "{ids:?}");
        assert_ne!(ids[0], "chatcmpl-up");
        // A second stream gets an id of its own.
        let mut other = StreamId::new(true);
        let mut id = "chatcmpl-up".to_string();
        other.stamp(&mut id);
        assert_ne!(id, ids[0]);
    }

    #[test]
    fn without_platform_mode_ids_are_the_upstreams() {
        let mut stream_id = StreamId::new(false);
        let mut chunk = upstream_chunk("chatcmpl-up", Some("toolu_bdrk_01Y"));
        chunk.usage = Some(deepseek_and_anthropic_extended_usage());
        stream_id.stamp(&mut chunk.id);
        let frame = serde_json::to_value(render_chunk(0, chunk, "m", None)).unwrap();
        assert_eq!(frame["id"], "chatcmpl-up");
        assert_eq!(
            frame["choices"][0]["delta"]["tool_calls"][0]["id"],
            "toolu_bdrk_01Y"
        );
        assert_eq!(frame["usage"]["prompt_cache_hit_tokens"], 32);
        assert_eq!(
            frame["usage"]["prompt_tokens_details"]["cache_creation_tokens"],
            4
        );

        let mut message = ChatMessage::assistant("");
        message.extra.insert(
            "tool_calls".into(),
            serde_json::json!([{"id": "toolu_bdrk_01Y", "type": "function",
                "function": {"name": "f", "arguments": "{}"}}]),
        );
        let r = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "chatcmpl-up".into(),
            model: "m".into(),
            message,
            finish_reason: FinishReason::ToolCalls,
            usage: deepseek_and_anthropic_extended_usage(),
            service_tier: None,
        };
        let body = serde_json::to_value(render_response(0, r, "m", None)).unwrap();
        assert_eq!(body["id"], "chatcmpl-up");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["id"],
            "toolu_bdrk_01Y"
        );
        assert_eq!(body["usage"]["prompt_cache_miss_tokens"], 18);
    }

    #[test]
    fn platform_mode_maps_tool_call_ids_and_keeps_usage_to_the_openai_shape() {
        use crate::opaque_id::{map_tool_call_id, Surface};
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let mapped = map_tool_call_id(&platform.id_key, Surface::Chat, "toolu_bdrk_01Y");
        let calls = serde_json::json!([{"id": "toolu_bdrk_01Y", "type": "function",
            "function": {"name": "f", "arguments": "{}"}}]);
        let mut message = ChatMessage::assistant("");
        message.extra.insert("tool_calls".into(), calls.clone());
        let mut second = ChatMessage::assistant("");
        second.extra.insert("tool_calls".into(), calls);
        let r = ChatResponse {
            logprobs: None,
            more_choices: vec![aisix_gateway::chat::ChatChoice {
                index: 1,
                message: second,
                finish_reason: FinishReason::ToolCalls,
                logprobs: None,
            }],
            id: "msg_bdrk_01X".into(),
            model: "m".into(),
            message,
            finish_reason: FinishReason::ToolCalls,
            usage: deepseek_and_anthropic_extended_usage(),
            service_tier: None,
        };
        let body = serde_json::to_value(render_response(0, r, "m", Some(&platform))).unwrap();
        let id = body["id"].as_str().unwrap();
        assert!(id.starts_with("chatcmpl-") && !id.contains("bdrk"), "{id}");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["id"],
            mapped.as_str()
        );
        assert_eq!(
            body["choices"][1]["message"]["tool_calls"][0]["id"],
            mapped.as_str()
        );
        // The standard counters keep every number; the extensions are gone.
        assert_eq!(body["usage"]["prompt_tokens"], 54);
        assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 32);
        assert!(body["usage"].get("prompt_cache_hit_tokens").is_none());
        assert!(body["usage"].get("prompt_cache_miss_tokens").is_none());
        assert!(body["usage"]["prompt_tokens_details"]
            .get("cache_creation_tokens")
            .is_none());

        let mut chunk = upstream_chunk("chatcmpl-up", Some("toolu_bdrk_01Y"));
        chunk.usage = Some(deepseek_and_anthropic_extended_usage());
        let frame = serde_json::to_value(render_chunk(0, chunk, "m", Some(&platform))).unwrap();
        assert_eq!(
            frame["choices"][0]["delta"]["tool_calls"][0]["id"],
            mapped.as_str()
        );
        assert!(frame["usage"].get("prompt_cache_hit_tokens").is_none());
        assert!(frame["usage"]["prompt_tokens_details"]
            .get("cache_creation_tokens")
            .is_none());
    }

    #[test]
    fn finish_reason_other_serialises_verbatim() {
        let r = ChatResponse {
            logprobs: None,
            more_choices: Vec::new(),
            id: "cmpl".into(),
            model: "m".into(),
            message: ChatMessage::assistant(""),
            finish_reason: FinishReason::Other("weird".into()),
            usage: UsageStats::default(),
            service_tier: None,
        };
        let out = render_response(0, r, "m", None);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["choices"][0]["finish_reason"], "weird");
    }
}
