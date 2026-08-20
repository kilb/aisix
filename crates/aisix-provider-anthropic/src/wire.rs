//! Anthropic `/v1/messages` wire shapes.
//!
//! Reference: <https://docs.anthropic.com/en/api/messages>
//!
//! Key differences from OpenAI that this module handles:
//!
//! - System prompt is a top-level `system` field, not a message with
//!   `role: "system"` — we collapse all leading system messages into one
//!   string (or an array of text blocks when the caller sent typed
//!   blocks, so `cache_control` markers survive) and forward it there.
//! - Only `user` and `assistant` roles on the wire. `tool` messages from
//!   ChatFormat are rejected at the bridge boundary rather than being
//!   silently re-classified.
//! - Content is an array of blocks — we emit a single `{"type":"text",…}`
//!   block per message (typed caller blocks forward their text blocks
//!   verbatim) and read the concatenation of text blocks on the way back.
//! - `max_tokens` is required by Anthropic. We default to a safe ceiling
//!   when the client didn't set one, but log the fallback so operators
//!   can tune the default if desired.
//! - Streaming events are typed (`message_start`, `content_block_delta`,
//!   …). We only emit a `ChatChunk` when a delta carries content or a
//!   stop reason — other events just advance internal state.

use aisix_gateway::{
    BridgeError, ChatChunk, ChatDelta, ChatFormat, ChatMessage, ChatResponse, FinishReason, Role,
    UsageStats,
};
use serde::{Deserialize, Serialize};

/// Anthropic requires a non-zero `max_tokens`. Clients that omit it get
/// this ceiling — generous enough to cover normal completions, conservative
/// enough that a runaway prompt doesn't burn tokens silently.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// The top-level `system` field — Anthropic accepts a plain string or an
/// array of text content blocks, documented as equivalent shapes
/// (<https://docs.anthropic.com/en/api/messages#parameter-system>).
///
/// The gateway emits the string form whenever every system message came
/// in as plain text (byte-identical to what it has always sent), and
/// switches to the block-array form only when a caller's system message
/// itself carried typed blocks — whose block-level fields (notably
/// `cache_control` prompt-cache markers) must survive translation
/// (#1110 Gap A).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AnthropicSystem {
    Text(String),
    Blocks(Vec<serde_json::Value>),
}

impl AnthropicSystem {
    /// Attach a `cache_control` marker to the last system block,
    /// promoting the string form to a single text block so the marker
    /// has somewhere to live. Skips an empty/whitespace-only last block
    /// (a blank system prompt promotes to `{"type":"text","text":""}`),
    /// which Anthropic rejects with a 400 — see [`is_empty_text_block`].
    fn mark_last_block(&mut self, marker: serde_json::Value) {
        if let AnthropicSystem::Text(s) = self {
            let text = std::mem::take(s);
            *self =
                AnthropicSystem::Blocks(vec![serde_json::json!({"type": "text", "text": text})]);
        }
        if let AnthropicSystem::Blocks(blocks) = self {
            if let Some(obj) = blocks
                .last_mut()
                .filter(|b| !is_empty_text_block(b))
                .and_then(|b| b.as_object_mut())
            {
                obj.insert("cache_control".into(), marker);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<AnthropicMessage<'a>>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<AnthropicSystem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    pub stream: bool,
    /// Tools spec translated from the caller's OpenAI-shape `tools`
    /// (when present in `extra`). The gateway emits Anthropic's
    /// shape per <https://docs.anthropic.com/en/api/messages>:
    /// `{name, description, input_schema}`. `None` when the caller
    /// didn't request tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    /// `tool_choice` translated from OpenAI's shape per
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-tool_choice>
    /// to Anthropic's per
    /// <https://docs.anthropic.com/en/api/messages#parameter-tool_choice>:
    ///   "auto"|"none"|"required"           → `{type:<same>}` ("required" → "any")
    ///   {type:"function",function:{name}}  → `{type:"tool", name}`
    /// Forwarding the OpenAI shape verbatim would 400 the upstream.
    /// `None` when the caller didn't set tool_choice (and we strip
    /// it from `extra` to avoid double-emit / shape mismatch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    /// Caller's other extra fields (excluding `tools`, which is
    /// translated above). Anthropic-incompatible OpenAI-only fields
    /// here would cause a 400 upstream — operators are expected to
    /// configure their gateway client to send shape-appropriate
    /// extras. Trade-off: forward-compatibility with new Anthropic
    /// fields > strict filtering.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicMessage<'a> {
    pub role: &'a str,
    /// Polymorphic content blocks — text and `tool_result` blocks
    /// emit different shapes per
    /// <https://docs.anthropic.com/en/api/messages>. Stored as
    /// owned `Value` so OpenAI `Role::Tool` messages can be
    /// translated into Anthropic `{type:"tool_result", tool_use_id,
    /// content}` without lifetime gymnastics.
    pub content: Vec<serde_json::Value>,
    #[serde(skip)]
    _lifetime: std::marker::PhantomData<&'a ()>,
}

impl<'a> AnthropicMessage<'a> {
    /// Single-text-block message (the common case for
    /// system/user/assistant turns without tool use).
    pub(crate) fn text(role: &'a str, text: &'a str) -> Self {
        Self {
            role,
            content: vec![serde_json::json!({"type": "text", "text": text})],
            _lifetime: std::marker::PhantomData,
        }
    }

    /// Anthropic tool_result block per
    /// <https://docs.anthropic.com/en/api/messages#example-of-tool-use>.
    /// Translates the OpenAI `{role:"tool", tool_call_id, content}`
    /// turn so agent-loop round-trips work — without this, the
    /// caller's tool-result reply 400s at the Anthropic upstream.
    pub(crate) fn tool_result(tool_use_id: &str, content: &str) -> Self {
        Self {
            role: "user",
            content: vec![serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            })],
            _lifetime: std::marker::PhantomData,
        }
    }

    /// Message content built from a caller's typed content blocks
    /// ([`ChatMessage::content_blocks`]): text blocks map 1:1 — OpenAI
    /// and Anthropic share the `{type:"text", text}` shape, and the
    /// block-level fields Anthropic accepts (notably `cache_control`
    /// prompt-cache markers) ride along in place. Flattening through the
    /// concatenated-text path would silently strip them (#1110
    /// Gap A). Image parts map to Anthropic `image` / `document` blocks in
    /// their original position; see [`anthropic_content_blocks`].
    /// Degrades to a single empty text block when nothing survives, so the
    /// turn is not dropped from the conversation. An empty `content` array
    /// is the shape being avoided; Anthropic's reference documents no
    /// minimum length for either `content` or a text block's `text`, so
    /// this is a conservative choice rather than a documented requirement.
    /// The one empty-text rule that IS documented — `cache_control` cannot
    /// be set on such a block — is handled by [`is_empty_text_block`],
    /// which the breakpoint injector consults so this degrade never
    /// acquires a marker upstream would reject.
    pub(crate) fn from_blocks(role: &'a str, blocks: &[serde_json::Value]) -> Self {
        let mut content = anthropic_content_blocks(blocks);
        if content.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": ""}));
        }
        Self {
            role,
            content,
            _lifetime: std::marker::PhantomData,
        }
    }

    /// Assistant turn replayed from conversation history, carrying its
    /// text (when any) plus any OpenAI-shape `tool_calls` translated into
    /// Anthropic `tool_use` blocks. An agent loop replays the assistant's
    /// prior tool calls before sending the matching tool results; without
    /// translating `tool_calls` here the following `tool_result` would
    /// reference a `tool_use` the upstream never saw and 400. When the
    /// caller sent typed content blocks, their TEXT blocks map 1:1
    /// (preserving `cache_control` markers) instead of the flattened text —
    /// media parts are not carried on this role, see the note below. Empty
    /// content with no tool calls degrades to an empty text block so the
    /// message isn't dropped — the same conservative degrade
    /// [`AnthropicMessage::from_blocks`] applies, and with the same caveat
    /// about what upstream actually documents.
    pub(crate) fn assistant(
        text: &str,
        blocks: Option<&[serde_json::Value]>,
        tool_calls: Option<&[serde_json::Value]>,
    ) -> Self {
        // Text only, deliberately: an Anthropic assistant turn carries what
        // the model produced (text / tool_use / thinking), and `image` is not
        // among those shapes. Mapping a media part into one — as the user
        // turn does — would turn a replay that previously worked into an
        // upstream 400.
        let mut content: Vec<serde_json::Value> = match blocks {
            Some(blocks) => anthropic_text_blocks(blocks),
            None => Vec::new(),
        };
        if content.is_empty() && !text.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": text}));
        }
        if let Some(tcs) = tool_calls {
            content.extend(tool_use_blocks_from_openai(tcs));
        }
        if content.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": ""}));
        }
        Self {
            role: "assistant",
            content,
            _lifetime: std::marker::PhantomData,
        }
    }
}

/// Forward the `{type:"text", …}` blocks of an OpenAI-shape content
/// array onto the Anthropic wire. Two guarantees per block:
///
/// * **Marker fidelity** — the fields Anthropic accepts on a text block
///   (`text`, `cache_control`, `citations`) forward unchanged, so a
///   caller's prompt-cache markers survive translation with their
///   block positions intact.
/// * **Clean-block guarantee** — everything else is dropped, matching
///   what the flattened-text path always guaranteed: stray caller
///   metadata (e.g. an OpenAI streaming `index` replayed from assembled
///   history) 400s at Anthropic's strict request validator, and
///   degenerate blocks (missing or whitespace-only `text`) carry nothing
///   worth forwarding. Both previously vanished in the flatten, so
///   forwarding them now would change requests that already worked —
///   that, not a documented upstream rejection, is why they are dropped:
///   the reference states no minimum length for a text block's `text`.
///
/// Non-text blocks are dropped HERE because both callers are roles that
/// Anthropic defines without media: the top-level `system` field, which is
/// text-only, and the replayed assistant turn, which carries only what a
/// model can produce. User message content goes through
/// [`anthropic_content_blocks`], which carries images and documents.
fn anthropic_text_blocks(blocks: &[serde_json::Value]) -> Vec<serde_json::Value> {
    const KEEP: &[&str] = &["type", "text", "cache_control", "citations"];
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter(|b| {
            b.get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| !t.trim().is_empty())
        })
        .map(|b| {
            let kept: serde_json::Map<String, serde_json::Value> = b
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(k, _)| KEEP.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            serde_json::Value::Object(kept)
        })
        .collect()
}

/// Anthropic image media types, per
/// <https://platform.claude.com/docs/en/api/messages>. Anything else is not
/// an image to Anthropic, so a part carrying it cannot become an `image`
/// block.
const ANTHROPIC_IMAGE_MEDIA_TYPES: &[&str] =
    &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Media type implied by the first bytes of a base64 payload.
///
/// Anthropic validates the DECLARED `media_type` against the actual bytes
/// and rejects a mismatch outright, so a caller whose data URI says `png`
/// over JPEG bytes would lose the whole request. The magic numbers are read
/// off the base64 prefix rather than the decoded image: base64 maps each
/// 3 input bytes onto 4 output chars, so the first four characters pin the
/// first three bytes without decoding — and without allocating a copy of an
/// image that may be megabytes.
///
/// `None` means "no confident opinion", and the caller keeps what the data
/// URI declared. Guessing beyond these four would be worse than trusting
/// the caller.
fn media_type_from_base64_prefix(data: &str) -> Option<&'static str> {
    // 0x89 'P' 'N' 'G' → "iVBO"; 0xFF 0xD8 0xFF → "/9j/"; "GIF8" → "R0lG";
    // "%PDF" → "JVBE".
    match data.as_bytes().get(..4)? {
        b"iVBO" => Some("image/png"),
        b"/9j/" => Some("image/jpeg"),
        b"R0lG" => Some("image/gif"),
        b"JVBE" => Some("application/pdf"),
        _ => None,
    }
}

/// Split a `data:` URI into `(media_type, base64_payload)`.
///
/// Only the base64 form is recognised; a percent-encoded `data:` URI is not
/// something either provider's SDK emits for an image, and decoding one
/// here would be inventing a shape neither side documents.
fn split_data_uri(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    (!media_type.is_empty() && !payload.is_empty()).then_some((media_type, payload))
}

/// Translate an OpenAI `image_url` content part into the Anthropic block
/// that carries the same bytes — the inverse of
/// [`openai_media_part_from_anthropic`], which has mapped the other
/// direction since #722.
///
/// Without this an OpenAI-shape caller dispatched to an Anthropic upstream
/// lost every image: the turn degraded to text, and the model answered
/// about a picture it was never shown. That failure is invisible from the
/// outside — a plausible answer to a question the model could not actually
/// see.
///
/// Two source forms, matching what Anthropic accepts:
///
///   - a `data:` URI becomes a `base64` source, with the media type taken
///     from the URI unless the payload's own magic bytes contradict it;
///   - an `http(s)` URL becomes a `url` source, forwarded as a URL.
///
/// The gateway deliberately does NOT fetch a remote image to re-encode it.
/// Anthropic accepts URLs natively, base64 inflates the payload by a third
/// against a per-image size limit, and a gateway that downloads caller-named
/// URLs takes on egress, latency and an SSRF surface that belong to the
/// provider. This is the mistake the ecosystem already made and walked back.
///
/// `detail` has no Anthropic equivalent and is dropped: it is a
/// quality/cost hint about how the image is processed, so losing it changes
/// the bill, never the answer's subject.
fn anthropic_media_block_from_openai(part: &serde_json::Value) -> Option<serde_json::Value> {
    let url = part
        .get("image_url")
        .and_then(|i| i.get("url"))
        .and_then(serde_json::Value::as_str)?;

    let source = match split_data_uri(url) {
        Some((declared, payload)) => {
            let media_type = media_type_from_base64_prefix(payload).unwrap_or(declared);
            serde_json::json!({
                "type": "base64",
                "media_type": media_type,
                "data": payload,
            })
        }
        // A non-`data:` URL. Only http(s) is forwarded — anything else
        // (`file:`, `gs:`, a bare path) is not something Anthropic fetches,
        // and passing it on would trade a dropped image for a 400.
        None if url.starts_with("http://") || url.starts_with("https://") => {
            serde_json::json!({"type": "url", "url": url})
        }
        _ => return None,
    };

    // The block type follows the media type: Anthropic carries a PDF as a
    // `document`, not an `image`, and rejects an `image` block whose media
    // type is not one of the four it renders.
    let media_type = source
        .get("media_type")
        .and_then(serde_json::Value::as_str)
        // A `url` source declares no media type; Anthropic sniffs it, and an
        // image is the only thing a caller can name through `image_url`.
        .unwrap_or("image/*");
    let block_type = if media_type == "application/pdf" {
        "document"
    } else if media_type == "image/*" || ANTHROPIC_IMAGE_MEDIA_TYPES.contains(&media_type) {
        "image"
    } else {
        return None;
    };

    let mut block = serde_json::Map::new();
    block.insert("type".into(), block_type.into());
    block.insert("source".into(), source);
    // A prompt-cache marker on the part rides along, same as on a text
    // block — Anthropic accepts `cache_control` on image and document
    // blocks too.
    if let Some(cc) = part.get("cache_control") {
        block.insert("cache_control".into(), cc.clone());
    }
    Some(serde_json::Value::Object(block))
}

/// Message content for the Anthropic wire: the text blocks
/// [`anthropic_text_blocks`] keeps, plus the media parts
/// [`anthropic_media_block_from_openai`] can carry, in the caller's
/// original order.
///
/// Order matters and is why this is one pass rather than two: "what is in
/// this image?" before the image and after it are different prompts.
///
/// Only for MESSAGE content. The top-level `system` field takes text blocks
/// alone, so `build_system` stays on the text-only filter.
fn anthropic_content_blocks(blocks: &[serde_json::Value]) -> Vec<serde_json::Value> {
    blocks
        .iter()
        .filter_map(|b| match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => anthropic_text_blocks(std::slice::from_ref(b)).pop(),
            Some("image_url") => {
                let mapped = anthropic_media_block_from_openai(b);
                if mapped.is_none() {
                    tracing::warn!(
                        "dropping image part with an unusable source on cross-provider \
                         dispatch: the model answers without having seen it",
                    );
                }
                mapped
            }
            other => {
                tracing::debug!(
                    block_type = ?other,
                    "dropping unsupported OpenAI content part on cross-provider dispatch",
                );
                None
            }
        })
        .collect()
}

/// Translate an array of OpenAI-shape `tool_calls`
/// (`{id, type:"function", function:{name, arguments}}`, `arguments` a
/// JSON string) into Anthropic `tool_use` content blocks
/// (`{type:"tool_use", id, name, input}`, `input` the parsed arguments
/// object). Entries missing an id or name are skipped; arguments that
/// don't parse to an object degrade to `{}`. Shared by the request-history
/// path ([`AnthropicMessage::assistant`]) and the response path
/// ([`chat_response_into_anthropic_json`]).
fn tool_use_blocks_from_openai(tool_calls: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tool_calls
        .iter()
        .filter_map(|tc| {
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .filter(|s| !s.is_empty())?;
            let input = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .filter(|v| v.is_object())
                .unwrap_or(serde_json::json!({}));
            Some(serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }))
        })
        .collect()
}

/// Merge adjacent messages that share a role by concatenating their
/// content blocks. Anthropic requires strictly alternating user/assistant
/// turns; a multi-turn tool loop (or parallel tool calls) produces
/// consecutive same-role turns — e.g. several `tool_result` replies, each
/// a `user` turn — that the upstream rejects with "messages: roles must
/// alternate" unless folded into one message.
fn merge_consecutive_roles(messages: Vec<AnthropicMessage<'_>>) -> Vec<AnthropicMessage<'_>> {
    let mut merged: Vec<AnthropicMessage<'_>> = Vec::with_capacity(messages.len());
    for msg in messages {
        match merged.last_mut() {
            Some(last) if last.role == msg.role => last.content.extend(msg.content),
            _ => merged.push(msg),
        }
    }
    merged
}

#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error("tool message missing tool_call_id field")]
    MissingToolCallId,
}

/// Split the gateway's flat ChatFormat into Anthropic's (system, messages)
/// shape. Consecutive plain-text system messages at the head are
/// concatenated with a blank line into the string form, matching how
/// users typically compose multi-paragraph system prompts in the OpenAI
/// format; when any head system message carried typed content blocks,
/// the whole system prompt switches to Anthropic's block-array form so
/// block-level fields (`cache_control` prompt-cache markers) survive —
/// see [`AnthropicSystem`].
///
/// Role::Tool turns translate to Anthropic's `{role:"user", content:
/// [{type:"tool_result", tool_use_id, content}]}` shape per
/// <https://docs.anthropic.com/en/api/messages> so agent-loop turn 2
/// (caller sends the tool's output back to the model) round-trips.
pub fn split_system<'a>(
    req: &'a ChatFormat,
) -> Result<(Option<AnthropicSystem>, Vec<AnthropicMessage<'a>>), TranslateError> {
    let mut system_msgs: Vec<&'a ChatMessage> = Vec::new();
    let mut messages: Vec<AnthropicMessage<'a>> = Vec::new();
    let mut seen_non_system = false;

    for m in &req.messages {
        match m.role {
            Role::System => {
                if seen_non_system {
                    // System messages interleaved with user/assistant
                    // turns don't map cleanly; append as a user turn to
                    // preserve semantics without silently dropping them.
                    messages.push(user_turn(m));
                } else {
                    system_msgs.push(m);
                }
            }
            Role::User => {
                seen_non_system = true;
                messages.push(user_turn(m));
            }
            Role::Assistant => {
                seen_non_system = true;
                let tool_calls = m
                    .extra
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice);
                messages.push(AnthropicMessage::assistant(
                    m.content_str(),
                    m.content_blocks.as_deref(),
                    tool_calls,
                ));
            }
            Role::Tool => {
                seen_non_system = true;
                let tool_use_id = m
                    .tool_call_id
                    .as_deref()
                    .ok_or(TranslateError::MissingToolCallId)?;
                messages.push(AnthropicMessage::tool_result(tool_use_id, m.content_str()));
            }
        }
    }

    // Fold consecutive same-role turns so the alternating-role invariant
    // Anthropic enforces holds for multi-turn tool loops and parallel
    // tool calls.
    Ok((
        build_system(&system_msgs),
        merge_consecutive_roles(messages),
    ))
}

/// A user-role turn: forward typed content blocks verbatim when the
/// caller sent them (preserving `cache_control` markers), else the
/// single-text-block shape from the flattened text.
fn user_turn(m: &ChatMessage) -> AnthropicMessage<'_> {
    match m.content_blocks.as_deref() {
        Some(blocks) => AnthropicMessage::from_blocks("user", blocks),
        None => AnthropicMessage::text("user", m.content_str()),
    }
}

/// Collapse the leading system messages into the top-level `system`
/// field. All-plain-text input keeps the historical `"\n\n"`-joined
/// string form byte-for-byte; any block-carrying message switches the
/// whole prompt to the block-array form, plain parts becoming their own
/// text blocks in order. Falls back to the string form when no text
/// block survives (e.g. image-only blocks), matching the flattened-text
/// behavior for that degenerate input.
fn build_system(system_msgs: &[&ChatMessage]) -> Option<AnthropicSystem> {
    if system_msgs.is_empty() {
        return None;
    }
    let joined = || {
        system_msgs
            .iter()
            .map(|m| m.content_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    if system_msgs.iter().all(|m| m.content_blocks.is_none()) {
        return Some(AnthropicSystem::Text(joined()));
    }
    let blocks: Vec<serde_json::Value> = system_msgs
        .iter()
        .flat_map(|m| match m.content_blocks.as_deref() {
            Some(blocks) => anthropic_text_blocks(blocks),
            None => vec![serde_json::json!({"type": "text", "text": m.content_str()})],
        })
        .collect();
    if blocks.is_empty() {
        return Some(AnthropicSystem::Text(joined()));
    }
    Some(AnthropicSystem::Blocks(blocks))
}

pub fn build_request<'a>(
    req: &'a ChatFormat,
    upstream_model: &'a str,
    system: Option<AnthropicSystem>,
    messages: Vec<AnthropicMessage<'a>>,
    stream: bool,
) -> AnthropicRequest<'a> {
    // Pull `tools` and `tool_choice` out of the caller's extras and
    // translate to Anthropic shape; everything else passes through
    // verbatim. Forwarding the OpenAI tool_choice shape would 400
    // upstream — the field is removed from `extra` even when the
    // translation returns None (e.g. unrecognised value), to avoid
    // a shape-mismatch double-emit.
    let mut extras = req.extra.clone();
    let tools = extras
        .remove("tools")
        .and_then(translate_openai_tools_to_anthropic);
    let tool_choice = extras
        .remove("tool_choice")
        .and_then(translate_openai_tool_choice_to_anthropic);
    AnthropicRequest {
        model: upstream_model,
        messages,
        max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system,
        temperature: req.temperature,
        top_p: req.top_p,
        stream,
        tools,
        tool_choice,
        extra: extras,
    }
}

/// Inject a pair of prompt-cache breakpoints into a request that carries
/// none of its own — one on the last system block, one on the last
/// content block of the final message — so the stable tools+system
/// prefix and the whole conversation prefix are cached
/// (<https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>).
///
/// Standing down entirely when the caller already supplied any
/// `cache_control` marker (on system, messages, or tools) is deliberate:
/// it never overrides the caller's own strategy, and it is what keeps the
/// request within Anthropic's four-breakpoint cap — this function adds at
/// most two markers, and only to a request that had zero.
///
/// `ttl` is the wire value for the injected markers (`5m` or `1h`); `5m`
/// is emitted as the bare `{"type":"ephemeral"}` Anthropic treats as the
/// five-minute default. A *below-minimum-length* prompt is a documented
/// silent no-op upstream, so no length gate is applied — but an *empty*
/// text block is a different case Anthropic rejects with a 400
/// (`cache_control cannot be set for empty text blocks`), so the marker
/// is never attached to one. The empty-text degrade shape is reachable
/// on the final turn (an image-only user message flattens to a single
/// `{"type":"text","text":""}` block) and on a blank system prompt.
pub fn inject_cache_breakpoints(req: &mut AnthropicRequest<'_>, ttl: &str) {
    if request_has_cache_control(req) {
        return;
    }
    let marker = cache_control_marker(ttl);
    if let Some(system) = req.system.as_mut() {
        system.mark_last_block(marker.clone());
    }
    if let Some(block) = req
        .messages
        .last_mut()
        .and_then(|m| m.content.last_mut())
        .filter(|b| !is_empty_text_block(b))
        .and_then(|b| b.as_object_mut())
    {
        block.insert("cache_control".into(), marker);
    }
}

/// Whether a content block is a text block with empty or whitespace-only
/// text. Anthropic rejects `cache_control` on such a block with a 400
/// (`cache_control cannot be set for empty text blocks`), so the
/// injector must skip it — it cannot be cached anyway.
fn is_empty_text_block(block: &serde_json::Value) -> bool {
    block.get("type").and_then(|t| t.as_str()) == Some("text")
        && block
            .get("text")
            .and_then(|t| t.as_str())
            .is_none_or(|t| t.trim().is_empty())
}

/// The marker written by [`inject_cache_breakpoints`]. `1h` carries the
/// explicit `ttl`; `5m` (and any other value) uses the bare ephemeral
/// form that defaults to five minutes.
fn cache_control_marker(ttl: &str) -> serde_json::Value {
    if ttl == "1h" {
        serde_json::json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        serde_json::json!({"type": "ephemeral"})
    }
}

/// Whether the request already carries any caller-supplied `cache_control`
/// marker on a system block, a message content block, or a tool
/// definition — the stand-down signal for [`inject_cache_breakpoints`].
/// The string-form system prompt cannot carry a marker, so only the
/// block form is scanned.
fn request_has_cache_control(req: &AnthropicRequest<'_>) -> bool {
    let system_marked = matches!(&req.system, Some(AnthropicSystem::Blocks(b)) if b.iter().any(block_has_cache_control));
    system_marked
        || req
            .messages
            .iter()
            .any(|m| m.content.iter().any(block_has_cache_control))
        || req
            .tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(block_has_cache_control))
}

fn block_has_cache_control(block: &serde_json::Value) -> bool {
    block.get("cache_control").is_some()
}

/// Translate the caller's OpenAI-shape `tools` array into
/// Anthropic's tools-spec shape on the outbound axis. Field mapping
/// per <https://platform.openai.com/docs/api-reference/chat/create#chat-create-tools>
/// and <https://docs.anthropic.com/en/api/messages#parameter-tools>:
///
///   OpenAI                                    Anthropic
///   {type: "function",                        {name,
///    function: {name, description,             description,
///               parameters}}                   input_schema}
///
/// Two kinds of entry survive:
///
///   - `type: "function"`, translated field-by-field as above;
///   - an entry ALREADY in Anthropic's own shape, forwarded verbatim.
///     Anthropic's server tools are declared as
///     `{"type": "web_search_20250305", "name": "web_search", …}` — a
///     versioned type plus a name, with no `function` wrapper
///     (<https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>),
///     and its client tools as `{name, description, input_schema}`. A
///     caller that declares one is not asking for a translation; it is
///     already speaking the target's language, and passing it through is
///     lossless.
///
/// Nothing is invented across vendors. An OpenAI built-in tool
/// (`code_interpreter`, `file_search`, the Responses `web_search`) is NOT
/// rewritten into an Anthropic server tool: the two run different indexes
/// and return different citation shapes, so a mapping would look
/// successful while answering from somewhere else. Those entries are
/// dropped — and, unlike before, the drop is logged, because a request
/// that loses a tool declaration gets an answer the model could not
/// actually support.
///
/// Returns `None` when the input isn't an array or when no entries
/// survived — keeping the field absent from the upstream wire shape so
/// Anthropic doesn't reject for empty-tools.
/// Recognise a tool entry the caller already wrote in Anthropic's shape and
/// forward it unchanged.
///
/// Two accepted forms, both from
/// <https://platform.claude.com/docs/en/api/messages>:
///
///   - a server tool — a `type` that is not one of OpenAI's own tool kinds,
///     plus a `name` (e.g. `web_search_20250305` / `web_search`);
///   - a client tool — a `name` plus an `input_schema`, with no `type`.
///
/// `type: "custom"` is OpenAI's freeform-tool kind, not an Anthropic one, so
/// it is excluded explicitly rather than mistaken for a server tool.
///
/// Forwarding verbatim is deliberate: these declarations carry optional
/// fields the gateway has no reason to model (`max_uses`, `allowed_domains`,
/// `user_location`, `allowed_callers`, …), and a re-serialising translation
/// would silently drop whichever ones it did not know about — the failure
/// this whole function exists to remove.
fn anthropic_native_tool(tool: &serde_json::Value) -> Option<serde_json::Value> {
    let obj = tool.as_object()?;
    let named = obj
        .get("name")
        .and_then(|n| n.as_str())
        .is_some_and(|n| !n.is_empty());
    if !named {
        tracing::warn!(
            tool_type = ?obj.get("type").and_then(|v| v.as_str()),
            "dropping unrecognised tool declaration on cross-provider dispatch",
        );
        return None;
    }
    let native = match obj.get("type").and_then(|v| v.as_str()) {
        // A versioned Anthropic server tool (`web_search_20250305`,
        // `web_fetch_20250910`, `code_execution_20260120`, …).
        Some(t) => t != "custom" && !obj.contains_key("function"),
        // No `type` at all: an Anthropic client tool is `{name,
        // description, input_schema}`.
        None => obj.contains_key("input_schema"),
    };
    if !native {
        tracing::warn!(
            tool_type = ?obj.get("type").and_then(|v| v.as_str()),
            "dropping tool with no Anthropic equivalent on cross-provider dispatch: \
             the model answers without it",
        );
        return None;
    }
    Some(tool.clone())
}

pub fn translate_openai_tools_to_anthropic(
    tools: serde_json::Value,
) -> Option<Vec<serde_json::Value>> {
    let arr = tools.as_array()?;
    let translated: Vec<serde_json::Value> = arr
        .iter()
        .filter_map(|t| {
            // OpenAI: `{type: "function", function: {name, description,
            // parameters}}`.
            if t.get("type").and_then(|v| v.as_str()) != Some("function") {
                return anthropic_native_tool(t);
            }
            let function = t.get("function")?.as_object()?;
            let name = function.get("name")?.as_str()?;
            let mut anthropic_tool = serde_json::Map::new();
            anthropic_tool.insert("name".into(), name.into());
            if let Some(desc) = function.get("description") {
                anthropic_tool.insert("description".into(), desc.clone());
            }
            // OpenAI's `parameters` (JSON Schema) maps to Anthropic's
            // `input_schema` verbatim — both are JSON Schema.
            if let Some(params) = function.get("parameters") {
                anthropic_tool.insert("input_schema".into(), params.clone());
            }
            // Anthropic tools accept a `cache_control` prompt-cache
            // marker on the tool entry
            // (<https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>).
            // OpenAI-shape callers attach it at the tool's top level or
            // inside `function`; forward either spelling — tool
            // definitions sit first in the prompt-cache prefix
            // hierarchy, so a stripped marker silently disables the
            // caller's whole caching strategy (#1110 Gap A).
            if let Some(cc) = t
                .get("cache_control")
                .or_else(|| function.get("cache_control"))
            {
                anthropic_tool.insert("cache_control".into(), cc.clone());
            }
            Some(serde_json::Value::Object(anthropic_tool))
        })
        .collect();
    if translated.is_empty() {
        None
    } else {
        Some(translated)
    }
}

/// Translate the caller's OpenAI-shape `tool_choice` to Anthropic's.
///
///   OpenAI                              Anthropic
///   "auto"                          →   {"type":"auto"}
///   "none"                          →   {"type":"none"}
///   "required"                      →   {"type":"any"}    (Anthropic's name for "must call something")
///   {type:"function",                   {"type":"tool",
///    function:{name:"X"}}           →    "name":"X"}
///
/// Returns None for unrecognised shapes — caller's value is discarded
/// rather than forwarded verbatim, since the OpenAI shape would 400
/// the Anthropic upstream.
pub fn translate_openai_tool_choice_to_anthropic(
    v: serde_json::Value,
) -> Option<serde_json::Value> {
    match v {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" | "none" => Some(serde_json::json!({"type": s})),
            "required" => Some(serde_json::json!({"type": "any"})),
            _ => None,
        },
        serde_json::Value::Object(o) => {
            if o.get("type").and_then(|t| t.as_str()) != Some("function") {
                return None;
            }
            let name = o
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())?;
            Some(serde_json::json!({"type": "tool", "name": name}))
        }
        _ => None,
    }
}

/// Translate Anthropic-shape `tools` array into OpenAI's tools-spec shape.
///
///   Anthropic                                 OpenAI
///   {name,                                    {type: "function",
///    description,                              function: {name, description,
///    input_schema}                                        parameters}}
///
/// Returns `None` when the input isn't an array or when no entries
/// translated — keeping the field absent from the outbound request.
pub fn translate_anthropic_tools_to_openai(tools: serde_json::Value) -> Option<serde_json::Value> {
    let arr = tools.as_array()?;
    let translated: Vec<serde_json::Value> = arr
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            let mut function = serde_json::Map::new();
            function.insert("name".into(), name.into());
            if let Some(desc) = t.get("description") {
                function.insert("description".into(), desc.clone());
            }
            if let Some(schema) = t.get("input_schema") {
                function.insert("parameters".into(), schema.clone());
            }
            Some(serde_json::json!({
                "type": "function",
                "function": serde_json::Value::Object(function),
            }))
        })
        .collect();
    if translated.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(translated))
    }
}

/// Translate Anthropic-shape `tool_choice` to OpenAI's.
///
///   Anthropic                              OpenAI
///   {"type":"auto"}                    →   "auto"
///   {"type":"none"}                    →   "none"  (Anthropic doesn't officially
///                                          document this but clients may send it)
///   {"type":"any"}                     →   "required"
///   {"type":"tool", "name":"X"}        →   {type:"function", function:{name:"X"}}
///
/// Returns `None` for unrecognised shapes.
pub fn translate_anthropic_tool_choice_to_openai(
    v: serde_json::Value,
) -> Option<serde_json::Value> {
    let obj = v.as_object()?;
    let typ = obj.get("type").and_then(|t| t.as_str())?;
    match typ {
        "auto" | "none" => Some(serde_json::Value::String(typ.to_string())),
        "any" => Some(serde_json::Value::String("required".to_string())),
        "tool" => {
            let name = obj.get("name").and_then(|n| n.as_str())?;
            Some(serde_json::json!({
                "type": "function",
                "function": {"name": name}
            }))
        }
        _ => None,
    }
}

/// Rewrite `extra` (as filled by [`parse_inbound_request`], i.e. raw
/// Anthropic `/v1/messages` top-level fields) into the OpenAI chat shape
/// that non-Anthropic bridges expect. Whitelist-translate what maps
/// cleanly; drop everything else — Anthropic-only fields flattened onto
/// an OpenAI-compatible upstream request are rejected as unknown
/// parameters, e.g. 400 "Unknown parameter: 'context_management'"
/// (#953). Mirrors the `/v1/responses` bridge's
/// whitelist-and-drop policy (#825) in the opposite direction.
///
/// Translations (matching LiteLLM's Anthropic→OpenAI adapter):
///   tools / tool_choice   → OpenAI shapes (existing helpers)
///   stop_sequences        → stop
///   metadata.user_id      → user
///   thinking              → reasoning_effort
pub fn translate_extras_to_openai_shape(extra: &mut serde_json::Map<String, serde_json::Value>) {
    let anthropic = std::mem::take(extra);
    for (key, value) in anthropic {
        match key.as_str() {
            "tools" => {
                if let Some(translated) = translate_anthropic_tools_to_openai(value) {
                    extra.insert("tools".to_string(), translated);
                }
            }
            "tool_choice" => {
                if let Some(translated) = translate_anthropic_tool_choice_to_openai(value) {
                    extra.insert("tool_choice".to_string(), translated);
                }
            }
            "stop_sequences" => {
                extra.insert("stop".to_string(), value);
            }
            "metadata" => {
                if let Some(user_id) = value.get("user_id").and_then(|v| v.as_str()) {
                    extra.insert("user".to_string(), user_id.into());
                }
            }
            "thinking" => {
                if let Some(effort) = reasoning_effort_from_thinking(&value) {
                    extra.insert("reasoning_effort".to_string(), effort.into());
                }
            }
            _ => {
                tracing::debug!(
                    field = %key,
                    "dropping Anthropic-only request field on cross-provider dispatch"
                );
            }
        }
    }
}

/// Bucket Anthropic `thinking` into an OpenAI `reasoning_effort` label.
/// Thresholds match LiteLLM's `reasoning_effort_from_thinking_budget`
/// (budget_tokens ≥ 4096 → high, ≥ 2048 → medium, ≥ 1024 → low, below →
/// minimal; `adaptive` → medium; `disabled`/unrecognised → None).
fn reasoning_effort_from_thinking(thinking: &serde_json::Value) -> Option<&'static str> {
    match thinking.get("type").and_then(|t| t.as_str())? {
        "enabled" => {
            let budget = thinking
                .get("budget_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(match budget {
                b if b >= 4096 => "high",
                b if b >= 2048 => "medium",
                b if b >= 1024 => "low",
                _ => "minimal",
            })
        }
        "adaptive" => Some("medium"),
        _ => None,
    }
}

/// Non-streaming response shape from `/v1/messages`.
#[derive(Debug, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub content: Vec<AnthropicResponseBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    /// Anthropic's `tool_use` content block. The model is asking to
    /// invoke a tool: `id` is the call id, `name` is the tool name,
    /// and `input` is a JSON object with the tool's arguments. Per
    /// docs §6 outbound-axis table ("tool_use ↔ tool_calls"), the
    /// gateway translates this into OpenAI's `tool_calls` shape on
    /// the response so OpenAI-SDK callers (and every agent framework
    /// built on that shape) work transparently against Anthropic
    /// upstreams.
    /// <https://docs.anthropic.com/en/api/messages#example-of-tool-use>
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Future content-block types (e.g. `image` on output, `thinking`
    /// for reasoning models). Not surfaced today; accepted so unknown
    /// block types don't fail the whole response parse.
    #[serde(other)]
    Other,
}

// `#[serde(default)]` at the container level so a response missing any
// token counter deserializes to 0 rather than failing the whole body
// decode (same bug class as OpenAI usage, #474).

/// Server-side tool invocations the provider ran and reports separately
/// from tokens, per
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
/// and its web-fetch sibling.
///
/// Both counters are recorded, though only one costs money: web search is
/// billed per search on top of tokens, while web fetch carries no
/// additional charge. The free one is still worth carrying — it says the
/// model reached out to a URL, which is the signal an operator wants when
/// the tool's own documentation calls it an exfiltration vector.
#[derive(Debug, Default, Deserialize)]
pub struct AnthropicServerToolUse {
    #[serde(default)]
    pub web_search_requests: u32,
    #[serde(default)]
    pub web_fetch_requests: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Server tools the provider ran for this request. Absent on a
    /// request that declared none.
    #[serde(default)]
    pub server_tool_use: Option<AnthropicServerToolUse>,
    /// Tokens written to the prompt cache (1.25× input rate). Optional
    /// — present only on requests with cache_control segments.
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    /// Tokens served from the prompt cache (0.10× input rate).
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

pub fn response_into_chat_response(raw: AnthropicResponse) -> ChatResponse {
    let mut saw_text_block = false;
    let text = raw
        .content
        .iter()
        .filter_map(|b| match b {
            AnthropicResponseBlock::Text { text } => {
                saw_text_block = true;
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    // #395: when an Anthropic upstream returns only `tool_use` blocks
    // (no text block at all), surface `content: null` on the OpenAI
    // shape rather than `""` — same wire-shape parity fix as the OpenAI
    // passthrough. An explicit empty text block (`text: ""`) is distinct
    // and preserved as `Some("")`.
    let content = saw_text_block.then_some(text);

    // Translate Anthropic `tool_use` content blocks into OpenAI's
    // `message.tool_calls` shape so OpenAI-SDK callers see a
    // standard tool-call response. Field mapping per
    // <https://docs.anthropic.com/en/api/messages> and
    // <https://platform.openai.com/docs/api-reference/chat/object#chat-create-tool_calls>:
    //
    //   Anthropic                  OpenAI
    //   id          (string)   →   tool_calls[].id
    //   name        (string)   →   tool_calls[].function.name
    //   input       (object)   →   tool_calls[].function.arguments  (JSON-encoded string)
    //   (implicit)             →   tool_calls[].type: "function"
    //
    // `arguments` MUST be a JSON-encoded STRING in OpenAI's shape
    // (not the parsed object) so SDK consumers round-trip via
    // `JSON.parse(toolCall.function.arguments)`.
    let tool_calls: Vec<serde_json::Value> = raw
        .content
        .iter()
        .filter_map(|b| match b {
            AnthropicResponseBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    // OpenAI emits `"{}"` (empty object) for no-args
                    // tool calls, not `"null"`. Normalise here so SDK
                    // consumers doing `JSON.parse(args)` get an
                    // object back even when Anthropic's `input`
                    // field is absent / null.
                    "arguments": match input {
                        serde_json::Value::Null => "{}".to_string(),
                        other => serde_json::to_string(other)
                            .unwrap_or_else(|_| "{}".to_string()),
                    },
                },
            })),
            _ => None,
        })
        .collect();
    let mut extra = serde_json::Map::new();
    if !tool_calls.is_empty() {
        extra.insert(
            "tool_calls".to_string(),
            serde_json::Value::Array(tool_calls),
        );
    }

    let usage = raw
        .usage
        .map(|u| {
            // Anthropic bills cache_creation / cache_read as input classes
            // *on top of* input_tokens, so `total_tokens` must fold them
            // in — `input + output` alone under-counts (#906). Cache
            // counters stay separate; Anthropic doesn't use OpenAI's
            // cached-prompt / reasoning taxonomy (those default to 0).
            let mut usage = UsageStats::with_cache(
                u.input_tokens,
                u.output_tokens,
                u.cache_creation_input_tokens,
                u.cache_read_input_tokens,
            );
            // Server-tool counters are NOT token classes and deliberately
            // stay out of `total_tokens`: web search bills per search on
            // top of tokens, so folding it into a token total would both
            // inflate the token figure and hide the charge.
            if let Some(stu) = u.server_tool_use.as_ref() {
                usage.web_search_requests = stu.web_search_requests;
                usage.web_fetch_requests = stu.web_fetch_requests;
            }
            usage
        })
        .unwrap_or_default();

    ChatResponse {
        id: raw.id,
        model: raw.model,
        message: ChatMessage {
            role: Role::Assistant,
            content,
            content_blocks: None,
            name: None,
            tool_call_id: None,
            extra,
        },
        finish_reason: map_stop_reason(raw.stop_reason.as_deref()),
        usage,
    }
}

fn map_stop_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some("end_turn") | Some("stop_sequence") | None => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some(other) => FinishReason::Other(other.to_string()),
    }
}

/// Streaming events from Anthropic. Only variants that can yield user-
/// visible output or terminate the stream are modeled here; the rest are
/// quietly dropped by the Bridge.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart {
        message: AnthropicStreamStartMessage,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { delta: AnthropicStreamDelta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicStreamMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicStreamUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    /// In-band error event — Anthropic reports mid-stream failures
    /// (`overloaded_error`, `api_error`, …) as an `event: error` frame
    /// inside the committed 200 stream instead of an HTTP status.
    /// Without this variant the frame fell into [`Self::Other`] and was
    /// silently swallowed: the connection then closed and the truncated
    /// stream looked like a clean completion (#1222).
    #[serde(rename = "error")]
    Error { error: AnthropicStreamErrorBody },
    /// Catch-all for content_block_start / content_block_stop / ping /
    /// unknown event types — we don't need their state for chunk emission.
    #[serde(other)]
    Other,
}

/// Body of an in-band `event: error` frame:
/// `{"type":"error","error":{"type":"overloaded_error","message":"…"}}`.
#[derive(Debug, Deserialize)]
pub struct AnthropicStreamErrorBody {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// The HTTP status Anthropic documents for each error type — in-band
/// stream errors carry only the type token, and this is the same
/// mapping the HTTP (non-2xx) surface uses for those errors.
/// Reference: <https://docs.anthropic.com/en/api/errors>.
/// Unknown / absent types return `None`; the proxy treats a
/// status-less in-band error as a transient upstream fault.
pub fn anthropic_error_kind_status(kind: Option<&str>) -> Option<u16> {
    match kind? {
        "invalid_request_error" => Some(400),
        "authentication_error" => Some(401),
        "permission_error" => Some(403),
        "not_found_error" => Some(404),
        "request_too_large" => Some(413),
        "rate_limit_error" => Some(429),
        "api_error" => Some(500),
        "overloaded_error" => Some(529),
        _ => None,
    }
}

/// Convert an in-band `event: error` body into the typed
/// [`BridgeError::UpstreamInBand`]. Shared by the Anthropic bridge and
/// the Vertex Claude (`:streamRawPredict`) path — both speak the
/// Anthropic Messages wire, so the Anthropic taxonomy applies to the
/// envelope translation either way.
pub fn stream_error_into_bridge_error(err: &AnthropicStreamErrorBody) -> BridgeError {
    let cap = aisix_gateway::MAX_UPSTREAM_ERROR_MESSAGE_BYTES;
    let status = anthropic_error_kind_status(err.kind.as_deref());
    let kind = err
        .kind
        .as_deref()
        .map(|k| aisix_gateway::truncate_lossy(k, cap));
    let message = err
        .message
        .as_deref()
        .map(|m| aisix_gateway::truncate_lossy(m, cap));
    BridgeError::UpstreamInBand {
        status,
        message: message
            .clone()
            .unwrap_or_else(|| "upstream reported a stream error".to_string()),
        parsed: Some(Box::new(aisix_gateway::UpstreamErrorView {
            kind,
            message,
            code: None,
            param: None,
        })),
        wire: aisix_gateway::UpstreamWire::Anthropic,
    }
}

#[derive(Debug, Deserialize)]
pub struct AnthropicStreamStartMessage {
    pub id: String,
    pub model: String,
    /// `message_start` carries the prompt token count in `usage.input_tokens`.
    /// Anthropic only sends it on this first event, so we must capture it here
    /// or prompt tokens are lost for the whole stream (TPM/budget/telemetry).
    #[serde(default)]
    pub usage: Option<AnthropicStreamStartUsage>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicStreamStartUsage {
    #[serde(default)]
    pub input_tokens: Option<u32>,
    /// Cache write / read counters ride on `message_start` alongside
    /// `input_tokens` and are sent only there — capture them here or
    /// they're lost for the whole stream on the cross-protocol bridge
    /// path (#906).
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicStreamDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicStreamMessageDelta {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicStreamUsage {
    #[serde(default)]
    pub output_tokens: Option<u32>,
    /// Server-tool counters ride the terminal `message_delta` alongside
    /// the final token counts.
    #[serde(default)]
    pub server_tool_use: Option<AnthropicServerToolUse>,
    /// Cumulative input/cache counts on the terminal `message_delta` —
    /// newer Anthropic wire sends them there too, and for some relay
    /// backends it is the ONLY frame that carries them (#952:
    /// `message_start` shipped no usable usage, so prompt tokens
    /// recorded as 0).
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
}

/// Rolling state the Bridge carries across a stream so chunks can be
/// tagged with the message id/model even though only the first event
/// carries them.
#[derive(Debug, Default)]
pub struct StreamState {
    pub id: String,
    pub model: String,
    /// Prompt tokens captured from `message_start`; folded into the usage
    /// emitted on the terminal `message_delta` so the final `UsageStats`
    /// carries both prompt and completion (and a correct total).
    pub input_tokens: u32,
    /// Cache write / read counters captured from `message_start`, carried
    /// onto the terminal usage so the bridge doesn't drop them (#906).
    pub cache_creation_tokens: u32,
    pub cache_read_tokens: u32,
}

impl StreamState {
    pub fn update(&mut self, event: &AnthropicStreamEvent) {
        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                self.id = message.id.clone();
                self.model = message.model.clone();
                // Reset on every message_start so a later message_start without
                // usage can't leave a stale prompt-token count from a prior one.
                self.input_tokens = message
                    .usage
                    .as_ref()
                    .and_then(|u| u.input_tokens)
                    .unwrap_or(0);
                self.cache_creation_tokens = message
                    .usage
                    .as_ref()
                    .and_then(|u| u.cache_creation_input_tokens)
                    .unwrap_or(0);
                self.cache_read_tokens = message
                    .usage
                    .as_ref()
                    .and_then(|u| u.cache_read_input_tokens)
                    .unwrap_or(0);
            }
            // #952: harvest cumulative input/cache counts from
            // the terminal message_delta too (max-wins) — some backends
            // report them only there. Runs before to_chunk() for the same
            // event, so the emitted UsageStats picks these up.
            AnthropicStreamEvent::MessageDelta {
                usage: Some(usage), ..
            } => {
                if let Some(t) = usage.input_tokens {
                    self.input_tokens = self.input_tokens.max(t);
                }
                if let Some(t) = usage.cache_creation_input_tokens {
                    self.cache_creation_tokens = self.cache_creation_tokens.max(t);
                }
                if let Some(t) = usage.cache_read_input_tokens {
                    self.cache_read_tokens = self.cache_read_tokens.max(t);
                }
            }
            _ => {}
        }
    }

    /// Translate one event into an optional chunk to yield upstream.
    pub fn to_chunk(&self, event: &AnthropicStreamEvent) -> Option<ChatChunk> {
        match event {
            AnthropicStreamEvent::ContentBlockDelta {
                delta: AnthropicStreamDelta::TextDelta { text },
            } => Some(ChatChunk {
                id: self.id.clone(),
                model: self.model.clone(),
                delta: ChatDelta {
                    role: None,
                    content: Some(text.clone()),
                    tool_calls: None,
                    function_call: None,
                    reasoning_content: None,
                },
                finish_reason: None,
                usage: None,
            }),
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                let finish = delta
                    .stop_reason
                    .as_deref()
                    .map(|r| map_stop_reason(Some(r)));
                let usage = usage.as_ref().and_then(|u| {
                    u.output_tokens.map(|n| {
                        let mut stats = UsageStats::with_cache(
                            self.input_tokens,
                            n,
                            self.cache_creation_tokens,
                            self.cache_read_tokens,
                        );
                        // Server-tool counters ride this terminal frame, the
                        // same one that carries the final token counts.
                        if let Some(stu) = u.server_tool_use.as_ref() {
                            stats.web_search_requests = stu.web_search_requests;
                            stats.web_fetch_requests = stu.web_fetch_requests;
                        }
                        stats
                    })
                });
                if finish.is_none() && usage.is_none() {
                    return None;
                }
                Some(ChatChunk {
                    id: self.id.clone(),
                    model: self.model.clone(),
                    delta: ChatDelta::default(),
                    finish_reason: finish,
                    usage,
                })
            }
            _ => None,
        }
    }

    pub fn is_terminal(event: &AnthropicStreamEvent) -> bool {
        matches!(event, AnthropicStreamEvent::MessageStop)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Inbound translation — Anthropic protocol  →  internal ChatFormat.
//
// Used by the proxy's /v1/messages handler when the Model targeted by
// the request points at a non-Anthropic upstream: we accept the
// Anthropic-shaped body, translate to ChatFormat, and dispatch through
// the Hub. The reverse direction (ChatFormat → Anthropic wire request)
// is handled by `split_system` + `build_request` for the
// Anthropic-upstream case above.
//
// Content-block coverage (#722, matching LiteLLM's
// `LiteLLMAnthropicMessagesAdapter.translate_anthropic_messages_to_openai`):
// `text`, `image` (base64 + url), `document` (→ image_url data URL, the
// LiteLLM mapping), assistant `tool_use` (→ OpenAI `tool_calls`), and
// `tool_result` (→ a `role:"tool"` message; string / single-text /
// multi-block content forms). `thinking` / `redacted_thinking` history
// blocks are dropped: the OpenAI chat wire cannot replay another
// vendor's signed reasoning blocks — LiteLLM's OpenAI provider
// transform discards them the same way (the top-level `thinking`
// config key still maps to `reasoning_effort`, see
// `translate_extras_to_openai_shape`).

#[derive(Debug, thiserror::Error)]
pub enum AnthropicInboundError {
    #[error("body is not a JSON object")]
    NotAnObject,
    #[error("missing or non-string `model` field")]
    MissingModel,
    #[error("missing or non-array `messages` field")]
    MissingMessages,
    #[error("messages[{idx}] missing `role`")]
    MessageMissingRole { idx: usize },
    #[error("messages[{idx}] role {role:?} is not 'user', 'assistant' or 'system'")]
    UnsupportedRole { idx: usize, role: String },
    #[error("messages[{idx}].content must be a string or an array of content blocks")]
    UnsupportedContent { idx: usize },
    #[error("`system` field must be a string or an array of text blocks")]
    UnsupportedSystem,
}

/// Parse an Anthropic `POST /v1/messages` JSON body into the gateway's
/// internal [`ChatFormat`]. The `system` field is folded into a leading
/// system message. Message content blocks translate to their OpenAI
/// equivalents (see the module comment above for the per-block map);
/// a user message whose blocks include `tool_result`s expands into the
/// preceding `role:"tool"` messages OpenAI expects. Unrecognized
/// top-level keys (`metadata`, `tools`, `tool_choice`, etc.) flow into
/// `ChatFormat::extra` for `translate_extras_to_openai_shape`.
pub fn parse_inbound_request(
    body: &serde_json::Value,
) -> Result<ChatFormat, AnthropicInboundError> {
    use serde_json::Value;
    let obj = body.as_object().ok_or(AnthropicInboundError::NotAnObject)?;

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or(AnthropicInboundError::MissingModel)?
        .to_string();

    let raw_messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(AnthropicInboundError::MissingMessages)?;

    let mut messages: Vec<ChatMessage> = Vec::with_capacity(raw_messages.len() + 1);

    // `system`: prepend as leading system message. Anthropic accepts
    // string OR array of text blocks; we accept both shapes.
    if let Some(system) = obj.get("system") {
        let system_text = match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => {
                let mut parts = Vec::new();
                for block in blocks {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        parts.push(text);
                    }
                }
                parts.join("\n")
            }
            Value::Null => String::new(),
            _ => return Err(AnthropicInboundError::UnsupportedSystem),
        };
        if !system_text.is_empty() {
            messages.push(ChatMessage::system(system_text));
        }
    }

    for (idx, m) in raw_messages.iter().enumerate() {
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .ok_or(AnthropicInboundError::MessageMissingRole { idx })?;

        match (role, m.get("content")) {
            ("user", Some(Value::String(s))) => messages.push(ChatMessage::user(s.clone())),
            ("assistant", Some(Value::String(s))) => {
                messages.push(ChatMessage::assistant(s.clone()))
            }
            // Not in the Anthropic spec, but Claude Code/cc-switch send it
            // (#597). Keep it as a system message so OpenAI-compatible
            // upstreams receive it natively instead of a 400 here.
            ("system", Some(Value::String(s))) => messages.push(ChatMessage::system(s.clone())),
            ("user", Some(Value::Array(blocks))) => {
                translate_user_blocks(blocks, &mut messages);
            }
            ("assistant", Some(Value::Array(blocks))) => {
                messages.push(translate_assistant_blocks(blocks));
            }
            ("system", Some(Value::Array(blocks))) => {
                messages.push(ChatMessage::system(concat_text_blocks(blocks)));
            }
            ("user" | "assistant" | "system", _) => {
                return Err(AnthropicInboundError::UnsupportedContent { idx })
            }
            (other, _) => {
                return Err(AnthropicInboundError::UnsupportedRole {
                    idx,
                    role: other.to_string(),
                })
            }
        }
    }

    let mut chat = ChatFormat::new(model, messages);

    if let Some(t) = obj.get("temperature").and_then(Value::as_f64) {
        chat.temperature = Some(t as f32);
    }
    if let Some(t) = obj.get("top_p").and_then(Value::as_f64) {
        chat.top_p = Some(t as f32);
    }
    if let Some(t) = obj.get("max_tokens").and_then(Value::as_u64) {
        // Saturate rather than wrap: `as u32` turns a caller's oversized
        // `max_tokens` into a small valid-looking number, so a request the
        // upstream would have rejected instead runs with a silently
        // different budget.
        chat.max_tokens = Some(u32::try_from(t).unwrap_or(u32::MAX));
    }
    if let Some(s) = obj.get("stream").and_then(Value::as_bool) {
        chat.stream = Some(s);
    }

    // Pass remaining keys through `extra` so future bridges can use
    // them. We deliberately don't whitelist — bridges that don't
    // understand a key just ignore it.
    for (key, value) in obj {
        if !matches!(
            key.as_str(),
            "model" | "messages" | "system" | "temperature" | "top_p" | "max_tokens" | "stream"
        ) {
            chat.extra.insert(key.clone(), value.clone());
        }
    }

    Ok(chat)
}

/// OpenAI function-name length cap; LiteLLM truncates the same way
/// (`truncate_tool_name`).
const OPENAI_TOOL_NAME_MAX: usize = 64;

fn truncate_tool_name(name: &str) -> &str {
    match name.char_indices().nth(OPENAI_TOOL_NAME_MAX) {
        Some((byte_idx, _)) => &name[..byte_idx],
        None => name,
    }
}

fn concat_text_blocks(blocks: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
            out.push_str(text);
        }
    }
    out
}

/// Translate one Anthropic `image` or `document` block into the OpenAI
/// `image_url` content part. Base64 sources become `data:` URLs; URL
/// sources pass through. Documents map to `image_url` as well — the
/// LiteLLM `_translate_anthropic_image_to_openai` behavior.
fn openai_media_part_from_anthropic(block: &serde_json::Value) -> Option<serde_json::Value> {
    let source = block.get("source")?;
    let url = match source.get("type").and_then(serde_json::Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("application/octet-stream");
            let data = source.get("data").and_then(serde_json::Value::as_str)?;
            format!("data:{media_type};base64,{data}")
        }
        Some("url") => source
            .get("url")
            .and_then(serde_json::Value::as_str)?
            .to_string(),
        _ => return None,
    };
    Some(serde_json::json!({"type": "image_url", "image_url": {"url": url}}))
}

/// Translate one Anthropic assistant `tool_use` block into an OpenAI
/// `tool_calls[]` entry (`arguments` is the JSON-*encoded* input).
fn tool_call_from_tool_use(block: &serde_json::Value) -> Option<serde_json::Value> {
    let id = block.get("id").and_then(serde_json::Value::as_str)?;
    let name = block.get("name").and_then(serde_json::Value::as_str)?;
    let input = block
        .get("input")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    Some(serde_json::json!({
        "id": id,
        "type": "function",
        "function": {
            "name": truncate_tool_name(name),
            "arguments": input.to_string(),
        }
    }))
}

/// Translate one Anthropic `tool_result` block into the OpenAI
/// `role:"tool"` message. Content forms (LiteLLM parity): absent → "",
/// string → string, single-text array → collapsed string, multi-block
/// array → combined text+image content parts on ONE tool message.
fn tool_message_from_tool_result(block: &serde_json::Value) -> ChatMessage {
    use serde_json::Value;
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let (content, content_blocks) = match block.get("content") {
        Some(Value::String(s)) => (Some(s.clone()), None),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            let mut text = String::new();
            let mut non_text = false;
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                            parts.push(serde_json::json!({"type": "text", "text": t}));
                        }
                    }
                    Some("image") => {
                        if let Some(p) = openai_media_part_from_anthropic(item) {
                            parts.push(p);
                            non_text = true;
                        }
                    }
                    other => {
                        tracing::debug!(
                            block_type = ?other,
                            "dropping unsupported tool_result item on cross-provider dispatch",
                        );
                    }
                }
            }
            if non_text {
                (Some(text), Some(parts))
            } else {
                // All-text (or empty) collapses to a plain string.
                (Some(text), None)
            }
        }
        _ => (Some(String::new()), None),
    };

    ChatMessage {
        role: Role::Tool,
        content,
        content_blocks,
        name: None,
        tool_call_id: (!tool_use_id.is_empty()).then_some(tool_use_id),
        extra: serde_json::Map::new(),
    }
}

/// Expand one Anthropic user message's content blocks. `tool_result`
/// blocks become individual `role:"tool"` messages emitted BEFORE the
/// user turn (OpenAI requires tool messages to directly follow the
/// assistant `tool_calls` turn); the remaining text/image/document
/// blocks form the user message itself.
fn translate_user_blocks(blocks: &[serde_json::Value], out: &mut Vec<ChatMessage>) {
    use serde_json::Value;
    let mut tool_messages: Vec<ChatMessage> = Vec::new();
    let mut parts: Vec<Value> = Vec::new();
    let mut text = String::new();
    let mut non_text = false;

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                    parts.push(serde_json::json!({"type": "text", "text": t}));
                }
            }
            Some("image") | Some("document") => {
                if let Some(p) = openai_media_part_from_anthropic(block) {
                    parts.push(p);
                    non_text = true;
                } else {
                    tracing::debug!(
                        "dropping image/document block with unsupported source on \
                         cross-provider dispatch",
                    );
                }
            }
            Some("tool_result") => tool_messages.push(tool_message_from_tool_result(block)),
            other => {
                tracing::debug!(
                    block_type = ?other,
                    "dropping unsupported Anthropic content block on cross-provider dispatch",
                );
            }
        }
    }

    let had_tool_messages = !tool_messages.is_empty();
    out.append(&mut tool_messages);

    if !parts.is_empty() {
        let mut msg = ChatMessage::user(text);
        if non_text {
            // Mixed/multimodal content rides the raw OpenAI parts array;
            // `content` keeps the concatenated text for guardrail scans
            // and non-block bridges.
            msg.content_blocks = Some(parts);
        }
        out.push(msg);
    } else if !had_tool_messages {
        // Preserve the pre-#722 behavior for a message whose blocks all
        // fell through: an empty user turn (rather than dropping the
        // message and shifting the conversation structure).
        out.push(ChatMessage::user(String::new()));
    }
}

/// Collapse one Anthropic assistant message's content blocks into a
/// ChatMessage: text concatenates, `tool_use` becomes OpenAI
/// `tool_calls`, thinking blocks drop (non-replayable on the OpenAI
/// wire — see the module comment).
fn translate_assistant_blocks(blocks: &[serde_json::Value]) -> ChatMessage {
    use serde_json::Value;
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                if let Some(tc) = tool_call_from_tool_use(block) {
                    tool_calls.push(tc);
                } else {
                    tracing::debug!(
                        "dropping malformed tool_use block (missing id/name) on \
                         cross-provider dispatch",
                    );
                }
            }
            Some("thinking") | Some("redacted_thinking") => {
                tracing::debug!(
                    "dropping thinking block on cross-provider dispatch (not replayable \
                     on the OpenAI wire)",
                );
            }
            other => {
                tracing::debug!(
                    block_type = ?other,
                    "dropping unsupported Anthropic content block on cross-provider dispatch",
                );
            }
        }
    }

    let mut msg = ChatMessage::assistant(text);
    if !tool_calls.is_empty() {
        if msg.content.as_deref() == Some("") {
            // OpenAI's canonical history shape for a pure tool-call turn
            // is `content: null`.
            msg.content = None;
        }
        msg.extra.insert(
            "tool_calls".to_string(),
            serde_json::Value::Array(tool_calls),
        );
    }
    msg
}

// ─────────────────────────────────────────────────────────────────────
// Outbound translation — internal ChatResponse  →  Anthropic JSON.

/// Render an internal [`ChatResponse`] as the JSON an Anthropic
/// `/v1/messages` client expects. The reverse of
/// `response_into_chat_response`. `model_display_name` is the
/// operator-facing model name the client requested — we echo it back
/// rather than leaking the actual upstream id (e.g. `gpt-4o`) when
/// the underlying provider isn't Anthropic.
pub fn chat_response_into_anthropic_json(
    resp: &ChatResponse,
    model_display_name: &str,
) -> serde_json::Value {
    let stop_reason = match &resp.finish_reason {
        FinishReason::Stop => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ContentFilter => "stop_sequence",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Other(_) => "end_turn",
    };

    let mut content: Vec<serde_json::Value> = Vec::new();

    if let Some(text) = resp.message.content.as_deref().filter(|s| !s.is_empty()) {
        content.push(serde_json::json!({"type": "text", "text": text}));
    }

    // Translate OpenAI-shape tool_calls from message.extra into
    // Anthropic tool_use content blocks so Anthropic clients see
    // the tool invocations the model requested.
    if let Some(tool_calls) = resp
        .message
        .extra
        .get("tool_calls")
        .and_then(|v| v.as_array())
    {
        content.extend(tool_use_blocks_from_openai(tool_calls));
    }

    if content.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": ""}));
    }

    serde_json::json!({
        "id": resp.id,
        "type": "message",
        "role": "assistant",
        "model": model_display_name,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": serde_json::Value::Null,
        "usage": {
            "input_tokens": resp.usage.prompt_tokens,
            "output_tokens": resp.usage.completion_tokens,
        },
    })
}

// ─────────────────────────────────────────────────────────────────────
// Streaming SSE encoder — internal ChatChunk stream  →  Anthropic
// SSE events.
//
// State machine:
//   1. First chunk that carries content or a finish_reason → emit
//      `message_start`. If it carries content, also emit
//      `content_block_start` + `content_block_delta`.
//   2. Mid-stream chunks with content → `content_block_delta`.
//   3. Chunk carrying `finish_reason` → emit `content_block_stop`
//      (only if a content block was opened), `message_delta` (with
//      stop_reason + final usage), then `message_stop`. After
//      `finished` flips true the encoder is silent.
//
// Reference: https://docs.anthropic.com/en/api/streaming

/// One Anthropic SSE event, ready to be written to the wire as
/// `event: {event}\ndata: {data}\n\n`.
#[derive(Debug, Clone)]
pub struct AnthropicSseEvent {
    pub event: &'static str,
    pub data: serde_json::Value,
}

impl AnthropicSseEvent {
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).expect("serde_json::Value always serializes"),
        )
    }
}

/// Per-tool-call accumulator used by the SSE encoder to track which
/// tool_use blocks have been started and at which content-block index.
#[derive(Debug)]
struct ToolCallState {
    id: String,
    name: String,
    content_block_index: usize,
    started: bool,
}

/// State machine for re-encoding a stream of internal `ChatChunk`s as
/// Anthropic SSE events.
#[derive(Debug)]
pub struct AnthropicSseEncoder {
    message_id: String,
    model_display_name: String,
    initial_input_tokens: u32,
    sent_message_start: bool,
    /// Index assigned to the text content block (if any).
    text_block_index: Option<usize>,
    finished: bool,
    /// Next content-block index to assign (shared across text + tool_use blocks).
    next_block_index: usize,
    /// Per-OpenAI-delta-index tool call state.
    tool_calls: std::collections::BTreeMap<u64, ToolCallState>,
    /// Stop reason captured at the `finish_reason` chunk while the
    /// closing `message_delta`/`message_stop` pair is withheld. With
    /// `stream_options.include_usage` (#790) an OpenAI
    /// upstream sends its only `usage` frame AFTER the stop chunk;
    /// emitting the pair at the stop chunk would hand the client
    /// `output_tokens: 0` and drop the usage frame unread.
    pending_stop_reason: Option<&'static str>,
    /// Best-known cumulative usage across all chunks. Max semantics —
    /// robust to providers that double-emit usage.
    seen_input_tokens: u32,
    seen_output_tokens: u32,
    usage_seen: bool,
}

impl AnthropicSseEncoder {
    /// `message_id` is echoed in `message_start.message.id`.
    /// `model_display_name` is the operator-facing model name the
    /// client originally sent in `req.model`.
    /// `initial_input_tokens` is the best-known-at-stream-open input
    /// token count; pass 0 if unknown.
    pub fn new(
        message_id: impl Into<String>,
        model_display_name: impl Into<String>,
        initial_input_tokens: u32,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            model_display_name: model_display_name.into(),
            initial_input_tokens,
            sent_message_start: false,
            text_block_index: None,
            finished: false,
            next_block_index: 0,
            tool_calls: std::collections::BTreeMap::new(),
            pending_stop_reason: None,
            seen_input_tokens: 0,
            seen_output_tokens: 0,
            usage_seen: false,
        }
    }

    /// Translate one chunk into the Anthropic SSE events to emit.
    /// Returns an empty Vec on no-op chunks.
    pub fn next_events(&mut self, chunk: &ChatChunk) -> Vec<AnthropicSseEvent> {
        if self.finished {
            return Vec::new();
        }

        if let Some(u) = chunk.usage.as_ref() {
            self.usage_seen = true;
            self.seen_input_tokens = self.seen_input_tokens.max(u.prompt_tokens);
            self.seen_output_tokens = self.seen_output_tokens.max(u.completion_tokens);
        }

        // Closing pair withheld at the stop chunk: only the trailing
        // usage frame releases it (stream end does too, via
        // `force_finish`). Post-stop chunks carry no renderable
        // content, so nothing else is emitted from here.
        if let Some(reason) = self.pending_stop_reason {
            if self.usage_seen {
                self.pending_stop_reason = None;
                return self.closing_pair(reason);
            }
            return Vec::new();
        }

        let mut events = Vec::new();

        let has_content = chunk
            .delta
            .content
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        let has_tool_calls = chunk
            .delta
            .tool_calls
            .as_ref()
            .is_some_and(|v| !v.is_empty());
        let has_finish = chunk.finish_reason.is_some();

        if !self.sent_message_start && (has_content || has_tool_calls || has_finish) {
            events.push(self.message_start_event());
            self.sent_message_start = true;
        }

        // ── Text content block ──
        if self.text_block_index.is_none() && has_content {
            let idx = self.next_block_index;
            self.next_block_index += 1;
            self.text_block_index = Some(idx);
            events.push(content_block_start_event(idx));
        }

        if has_content {
            let idx = self.text_block_index.unwrap_or(0);
            events.push(AnthropicSseEvent {
                event: "content_block_delta",
                data: serde_json::json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": {
                        "type": "text_delta",
                        "text": chunk.delta.content.clone().unwrap_or_default(),
                    },
                }),
            });
        }

        // ── Tool-use content blocks ──
        if let Some(tool_calls) = &chunk.delta.tool_calls {
            for tc in tool_calls {
                let oai_index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);

                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let arguments = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("");

                let state = self.tool_calls.entry(oai_index).or_insert_with(|| {
                    let block_idx = self.next_block_index;
                    self.next_block_index += 1;
                    ToolCallState {
                        id: String::new(),
                        name: String::new(),
                        content_block_index: block_idx,
                        started: false,
                    }
                });

                if !id.is_empty() {
                    state.id = id.to_string();
                }
                if !name.is_empty() {
                    state.name = name.to_string();
                }

                // Emit content_block_start once id and name are known.
                if !state.started && !state.id.is_empty() && !state.name.is_empty() {
                    state.started = true;
                    events.push(AnthropicSseEvent {
                        event: "content_block_start",
                        data: serde_json::json!({
                            "type": "content_block_start",
                            "index": state.content_block_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": state.id,
                                "name": state.name,
                                "input": {},
                            },
                        }),
                    });
                }

                if state.started && !arguments.is_empty() {
                    events.push(AnthropicSseEvent {
                        event: "content_block_delta",
                        data: serde_json::json!({
                            "type": "content_block_delta",
                            "index": state.content_block_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": arguments,
                            },
                        }),
                    });
                }
            }
        }

        // ── Finish ──
        if let Some(fr) = &chunk.finish_reason {
            // Close text block if open.
            if let Some(text_idx) = self.text_block_index {
                events.push(content_block_stop_event(text_idx));
            }
            // Close all open tool_use blocks.
            for state in self.tool_calls.values() {
                if state.started {
                    events.push(content_block_stop_event(state.content_block_index));
                }
            }

            let stop_reason = match fr {
                FinishReason::Stop => "end_turn",
                FinishReason::Length => "max_tokens",
                FinishReason::ContentFilter => "stop_sequence",
                FinishReason::ToolCalls => "tool_use",
                FinishReason::Other(_) => "end_turn",
            };
            if self.usage_seen {
                // Usage already known (provider attached it to the stop
                // chunk or earlier) — close out immediately.
                events.extend(self.closing_pair(stop_reason));
            } else {
                // OpenAI's `stream_options.include_usage` frame arrives
                // AFTER the stop chunk — withhold the closing pair so
                // it can carry real token counts.
                self.pending_stop_reason = Some(stop_reason);
            }
        }

        events
    }

    /// The closing `message_delta` + `message_stop` pair, carrying the
    /// best-known cumulative usage. `input_tokens` is included when
    /// known — on a translated stream `message_start` fires before any
    /// usage frame exists and always reports 0, so this is the only
    /// place the client can learn the prompt token count.
    fn closing_pair(&mut self, stop_reason: &'static str) -> Vec<AnthropicSseEvent> {
        let mut usage = serde_json::Map::new();
        if self.seen_input_tokens > 0 {
            usage.insert("input_tokens".into(), self.seen_input_tokens.into());
        }
        usage.insert("output_tokens".into(), self.seen_output_tokens.into());
        self.finished = true;
        vec![
            AnthropicSseEvent {
                event: "message_delta",
                data: serde_json::json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": stop_reason,
                        "stop_sequence": serde_json::Value::Null,
                    },
                    "usage": serde_json::Value::Object(usage),
                }),
            },
            AnthropicSseEvent {
                event: "message_stop",
                data: serde_json::json!({"type": "message_stop"}),
            },
        ]
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Force-close the stream when the upstream ended without
    /// releasing the closing pair — either no `finish_reason` chunk at
    /// all (closes with `end_turn`), or a stop chunk arrived but the
    /// trailing usage frame never did (flushes the withheld pair with
    /// the real stop reason). Usage carries the best-known counts.
    /// Idempotent.
    pub fn force_finish(&mut self) -> Vec<AnthropicSseEvent> {
        if self.finished {
            return Vec::new();
        }
        // A withheld closing pair (stop seen, but the upstream ignored
        // `stream_options` and never sent a usage frame): flush it with
        // the real stop reason. Content blocks were already closed at
        // the stop chunk.
        if let Some(reason) = self.pending_stop_reason.take() {
            return self.closing_pair(reason);
        }
        let mut events = Vec::new();
        if !self.sent_message_start {
            events.push(self.message_start_event());
            self.sent_message_start = true;
        }
        if let Some(text_idx) = self.text_block_index {
            events.push(content_block_stop_event(text_idx));
        }
        for state in self.tool_calls.values() {
            if state.started {
                events.push(content_block_stop_event(state.content_block_index));
            }
        }
        events.extend(self.closing_pair("end_turn"));
        events
    }

    fn message_start_event(&self) -> AnthropicSseEvent {
        AnthropicSseEvent {
            event: "message_start",
            data: serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model_display_name,
                    "stop_reason": serde_json::Value::Null,
                    "stop_sequence": serde_json::Value::Null,
                    "usage": {
                        "input_tokens": self.initial_input_tokens,
                        "output_tokens": 0,
                    },
                },
            }),
        }
    }
}

fn content_block_start_event(index: usize) -> AnthropicSseEvent {
    AnthropicSseEvent {
        event: "content_block_start",
        data: serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""},
        }),
    }
}

fn content_block_stop_event(index: usize) -> AnthropicSseEvent {
    AnthropicSseEvent {
        event: "content_block_stop",
        data: serde_json::json!({"type": "content_block_stop", "index": index}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_system_merges_leading_system_messages() {
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::system("you are helpful"),
                ChatMessage::system("respond concisely"),
                ChatMessage::user("hi"),
            ],
        );
        let (system, msgs) = split_system(&req).unwrap();
        assert_eq!(
            system,
            Some(AnthropicSystem::Text(
                "you are helpful\n\nrespond concisely".into()
            ))
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn split_system_mid_conversation_becomes_user_turn() {
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::user("hi"),
                ChatMessage::system("forget everything"),
                ChatMessage::assistant("ok"),
            ],
        );
        let (system, msgs) = split_system(&req).unwrap();
        assert!(system.is_none());
        // The interleaved system message becomes a user turn and folds into
        // the adjacent user turn (alternating-role invariant): one user
        // message carrying both text blocks, then the assistant turn.
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content.len(), 2);
        assert_eq!(msgs[0].content[0]["text"], "hi");
        assert_eq!(msgs[0].content[1]["text"], "forget everything");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn split_system_rejects_tool_role_without_tool_call_id() {
        // Tool turn must carry a tool_call_id (the OpenAI shape
        // pairs tool_calls[i].id with the next turn's tool_call_id).
        // Without one, we can't construct Anthropic's tool_result
        // block — error rather than silently dropping the turn.
        let req = ChatFormat::new(
            "claude",
            vec![ChatMessage {
                role: Role::Tool,
                content: Some("x".into()),
                content_blocks: None,
                name: None,
                tool_call_id: None,
                extra: serde_json::Map::new(),
            }],
        );
        assert!(matches!(
            split_system(&req),
            Err(TranslateError::MissingToolCallId)
        ));
    }

    #[test]
    fn split_system_translates_tool_role_to_anthropic_tool_result() {
        // Agent-loop turn 2: caller sends back the tool's output via
        // {role:"tool", tool_call_id, content}; gateway must
        // translate to Anthropic's
        // {role:"user", content:[{type:"tool_result", tool_use_id, content}]}.
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::user("What's the weather in SF?"),
                tool_call_assistant("toolu_abc", "get_weather", "{\"city\":\"SF\"}"),
                ChatMessage {
                    role: Role::Tool,
                    content: Some("72F, sunny".into()),
                    content_blocks: None,
                    name: None,
                    tool_call_id: Some("toolu_abc".into()),
                    extra: serde_json::Map::new(),
                },
            ],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(msgs.len(), 3);
        // Tool turn became a user turn with a tool_result block.
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content.len(), 1);
        assert_eq!(msgs[2].content[0]["type"], "tool_result");
        assert_eq!(msgs[2].content[0]["tool_use_id"], "toolu_abc");
        assert_eq!(msgs[2].content[0]["content"], "72F, sunny");
    }

    /// Build an assistant ChatMessage replaying a single tool call, the
    /// OpenAI history shape an agent loop sends back.
    fn tool_call_assistant(id: &str, name: &str, arguments: &str) -> ChatMessage {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "tool_calls".into(),
            serde_json::json!([{
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            }]),
        );
        ChatMessage {
            role: Role::Assistant,
            content: None,
            content_blocks: None,
            name: None,
            tool_call_id: None,
            extra,
        }
    }

    #[test]
    fn split_system_translates_assistant_tool_calls_to_tool_use() {
        // Agent-loop turn 2: the caller replays the assistant's prior
        // tool call as OpenAI-shape `tool_calls` in message.extra. Without
        // translation the tool_use is dropped and the following
        // tool_result orphans → Anthropic 400.
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::user("weather in SF?"),
                tool_call_assistant("toolu_1", "get_weather", "{\"city\":\"SF\"}"),
                ChatMessage {
                    role: Role::Tool,
                    content: Some("72F".into()),
                    content_blocks: None,
                    name: None,
                    tool_call_id: Some("toolu_1".into()),
                    extra: serde_json::Map::new(),
                },
            ],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].role, "assistant");
        let block = &msgs[1].content[0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "toolu_1");
        assert_eq!(block["name"], "get_weather");
        assert_eq!(block["input"]["city"], "SF");
        // The tool result alternates back as a user turn.
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content[0]["type"], "tool_result");
    }

    #[test]
    fn split_system_merges_parallel_tool_results_into_one_user_turn() {
        // Parallel tool calls produce two consecutive tool_result turns;
        // they must fold into a single user message so roles still
        // alternate (assistant → user) for the upstream.
        let mut assistant_extra = serde_json::Map::new();
        assistant_extra.insert(
            "tool_calls".into(),
            serde_json::json!([
                {"id": "t1", "type": "function", "function": {"name": "a", "arguments": "{}"}},
                {"id": "t2", "type": "function", "function": {"name": "b", "arguments": "{}"}},
            ]),
        );
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::user("go"),
                ChatMessage {
                    role: Role::Assistant,
                    content: None,
                    content_blocks: None,
                    name: None,
                    tool_call_id: None,
                    extra: assistant_extra,
                },
                ChatMessage {
                    role: Role::Tool,
                    content: Some("r1".into()),
                    content_blocks: None,
                    name: None,
                    tool_call_id: Some("t1".into()),
                    extra: serde_json::Map::new(),
                },
                ChatMessage {
                    role: Role::Tool,
                    content: Some("r2".into()),
                    content_blocks: None,
                    name: None,
                    tool_call_id: Some("t2".into()),
                    extra: serde_json::Map::new(),
                },
            ],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        // user, assistant(2 tool_use), user(2 tool_result)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content.len(), 2);
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content.len(), 2);
        assert_eq!(msgs[2].content[0]["tool_use_id"], "t1");
        assert_eq!(msgs[2].content[1]["tool_use_id"], "t2");
    }

    /// Serialize a built request and return the JSON value of its
    /// `system` field (Value::Null when absent) — the wire shape the
    /// Anthropic upstream actually sees.
    fn wire_system(req: &ChatFormat) -> serde_json::Value {
        let (system, messages) = split_system(req).unwrap();
        let built = build_request(req, "claude-sonnet-4-5", system, messages, false);
        serde_json::to_value(&built).unwrap()["system"].clone()
    }

    /// ChatMessage carrying a typed content-block array, as produced by
    /// deserializing the OpenAI array-form `content` (blocks land in
    /// `content_blocks`, concatenated text in `content`).
    fn block_message(role: Role, blocks: serde_json::Value) -> ChatMessage {
        let raw = serde_json::json!({"role": role, "content": blocks});
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn split_system_plain_system_messages_stay_string_on_the_wire() {
        // Byte-stability pin: callers sending plain-string system
        // messages must keep getting the exact string form the gateway
        // has always emitted — the block-array form is reserved for
        // callers that themselves sent typed blocks.
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::system("you are helpful"),
                ChatMessage::system("respond concisely"),
                ChatMessage::user("hi"),
            ],
        );
        assert_eq!(
            wire_system(&req),
            serde_json::json!("you are helpful\n\nrespond concisely")
        );
    }

    #[test]
    fn split_system_preserves_cache_control_on_system_blocks() {
        // A caller marking its system prompt for provider-side prompt
        // caching sends array-form content with a `cache_control`
        // marker. The marker must reach the upstream — flattening to
        // the concatenated string silently strips it and the caller
        // pays full input price every turn (#1110 Gap A).
        let req = ChatFormat::new(
            "claude",
            vec![
                block_message(
                    Role::System,
                    serde_json::json!([
                        {"type": "text", "text": "big stable prefix",
                         "cache_control": {"type": "ephemeral"}},
                    ]),
                ),
                ChatMessage::user("hi"),
            ],
        );
        assert_eq!(
            wire_system(&req),
            serde_json::json!([
                {"type": "text", "text": "big stable prefix",
                 "cache_control": {"type": "ephemeral"}},
            ])
        );
    }

    #[test]
    fn split_system_mixed_plain_and_block_system_messages_become_blocks() {
        // One plain system message + one block-form system message:
        // the whole system prompt goes to array form, the plain part
        // becoming its own text block, order preserved.
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::system("you are helpful"),
                block_message(
                    Role::System,
                    serde_json::json!([
                        {"type": "text", "text": "cached tail",
                         "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                    ]),
                ),
                ChatMessage::user("hi"),
            ],
        );
        assert_eq!(
            wire_system(&req),
            serde_json::json!([
                {"type": "text", "text": "you are helpful"},
                {"type": "text", "text": "cached tail",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ])
        );
    }

    #[test]
    fn split_system_preserves_cache_control_on_user_content_blocks() {
        // Per-block structure must survive: a marker on block 2 of 2
        // means "cache through here" — concatenating the blocks into
        // one loses the position along with the marker.
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "text", "text": "conversation so far"},
                    {"type": "text", "text": "latest turn",
                     "cache_control": {"type": "ephemeral"}},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content.len(), 2);
        assert_eq!(msgs[0].content[0]["text"], "conversation so far");
        assert!(msgs[0].content[0].get("cache_control").is_none());
        assert_eq!(
            msgs[0].content[1]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn split_system_preserves_cache_control_on_assistant_content_blocks() {
        // Markers on replayed assistant history turns must survive too
        // — and tool_calls translation still appends its tool_use
        // blocks after the text blocks.
        let mut msg = block_message(
            Role::Assistant,
            serde_json::json!([
                {"type": "text", "text": "prior answer",
                 "cache_control": {"type": "ephemeral"}},
            ]),
        );
        msg.extra.insert(
            "tool_calls".into(),
            serde_json::json!([{
                "id": "t1", "type": "function",
                "function": {"name": "f", "arguments": "{}"},
            }]),
        );
        let req = ChatFormat::new("claude", vec![ChatMessage::user("hi"), msg]);
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content.len(), 2);
        assert_eq!(
            msgs[1].content[0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert_eq!(msgs[1].content[1]["type"], "tool_use");
    }

    #[test]
    fn user_message_with_no_mappable_block_degrades_to_empty_text_block() {
        // The message must still carry a non-empty content array —
        // Anthropic rejects `content: []`. An audio part has no Anthropic
        // equivalent, so it is the shape that still degrades now that
        // images map.
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "input_audio", "input_audio": {"data": "xyz", "format": "wav"}},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].content,
            vec![serde_json::json!({"type": "text", "text": ""})]
        );
    }

    /// An OpenAI-shape caller dispatched to an Anthropic upstream used to
    /// lose every image: the turn degraded to text and the model answered
    /// about a picture it was never shown — a plausible answer to a
    /// question it could not see, indistinguishable from a real one.
    #[test]
    fn image_parts_reach_anthropic_as_image_blocks_in_place() {
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "text", "text": "what is in"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                    {"type": "text", "text": "this picture?"},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        // Order is preserved: "what is in" BEFORE the image and
        // "this picture?" after it is a different prompt from any other
        // arrangement.
        assert_eq!(msgs[0].content.len(), 3);
        assert_eq!(msgs[0].content[0]["text"], "what is in");
        assert_eq!(msgs[0].content[2]["text"], "this picture?");
        assert_eq!(
            msgs[0].content[1],
            serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "iVBORw0KGgo="
                }
            })
        );
    }

    /// A remote image is forwarded as a URL, never fetched and re-encoded.
    /// Base64 inflates the payload by a third against a per-image size
    /// limit, and a gateway that downloads caller-named URLs takes on
    /// egress, latency and an SSRF surface that belong to the provider.
    #[test]
    fn a_remote_image_forwards_as_a_url_source() {
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "image_url", "image_url": {"url": "https://x.example/cat.png"}},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(
            msgs[0].content[0],
            serde_json::json!({
                "type": "image",
                "source": {"type": "url", "url": "https://x.example/cat.png"}
            })
        );
    }

    /// Anthropic validates the declared media type against the actual
    /// bytes and rejects a mismatch outright, so a caller whose data URI
    /// mislabels its payload would lose the whole request. The magic
    /// bytes win.
    #[test]
    fn a_mislabelled_data_uri_is_corrected_from_its_magic_bytes() {
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                // Declares PNG, carries JPEG bytes ("/9j/" is 0xFF 0xD8 0xFF).
                serde_json::json!([
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,/9j/4AAQ"}},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(
            msgs[0].content[0]["source"]["media_type"], "image/jpeg",
            "the declared type must yield to the bytes, or Anthropic 400s the request",
        );
    }

    /// A PDF is a `document` to Anthropic, not an `image` — an `image`
    /// block carrying `application/pdf` is rejected.
    #[test]
    fn a_pdf_data_uri_becomes_a_document_block() {
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "image_url",
                     "image_url": {"url": "data:application/pdf;base64,JVBERi0xLjQ="}},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(msgs[0].content[0]["type"], "document");
        assert_eq!(
            msgs[0].content[0]["source"]["media_type"],
            "application/pdf"
        );
    }

    /// A media type Anthropic does not render cannot become an `image`
    /// block — forwarding it would trade a dropped image for a 400 on the
    /// whole request.
    #[test]
    fn an_unrenderable_media_type_is_not_forwarded_as_an_image() {
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/tiff;base64,SUkqAA=="}},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(msgs[0].content.len(), 1);
        assert_eq!(msgs[0].content[0]["text"], "look");
    }

    /// The system prompt stays text-only: Anthropic defines the top-level
    /// `system` field as text blocks, so an image there must not be
    /// promoted into it.
    #[test]
    fn an_image_in_a_system_message_does_not_reach_the_system_field() {
        let req = ChatFormat::new(
            "claude",
            vec![
                block_message(
                    Role::System,
                    serde_json::json!([
                        {"type": "text", "text": "be brief"},
                        {"type": "image_url",
                         "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                    ]),
                ),
                ChatMessage::user("hi"),
            ],
        );
        let (system, _msgs) = split_system(&req).unwrap();
        let rendered = serde_json::to_string(&system).unwrap();
        assert!(
            !rendered.contains("image"),
            "the system field must carry text blocks only: {rendered}",
        );
    }

    #[test]
    fn split_system_drops_degenerate_text_blocks_but_keeps_marked_ones() {
        // Empty / missing-text segments are common from templating code
        // and vanished in the old flatten; Anthropic rejects them
        // per-block, so forwarding them would 400 requests that worked
        // before. They must be filtered while the marked block survives.
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "text", "text": ""},
                    {"type": "text"},
                    {"type": "text", "text": "   "},
                    {"type": "text", "text": "hi",
                     "cache_control": {"type": "ephemeral"}},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(
            msgs[0].content,
            vec![serde_json::json!({
                "type": "text", "text": "hi",
                "cache_control": {"type": "ephemeral"},
            })]
        );
    }

    #[test]
    fn split_system_strips_stray_fields_from_forwarded_blocks() {
        // Callers built against lax OpenAI-compatible upstreams replay
        // assembled history carrying per-part metadata (e.g. a
        // streaming `index`). Anthropic's strict validator rejects
        // unknown block fields, and the old flatten path absorbed them
        // — only the fields Anthropic accepts may forward.
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "text", "text": "hi", "index": 0,
                     "annotations": [],
                     "cache_control": {"type": "ephemeral"}},
                ]),
            )],
        );
        let (_system, msgs) = split_system(&req).unwrap();
        assert_eq!(
            msgs[0].content,
            vec![serde_json::json!({
                "type": "text", "text": "hi",
                "cache_control": {"type": "ephemeral"},
            })]
        );
    }

    #[test]
    fn translate_tools_preserves_cache_control_marker() {
        // Anthropic tools accept a `cache_control` marker on the tool
        // entry; OpenAI-shape callers attach it at the tool's top
        // level (or inside `function`). Both spellings must survive —
        // tool definitions sit first in the prompt-cache prefix
        // hierarchy, so a stripped marker silently disables the
        // caller's whole caching strategy.
        let tools = serde_json::json!([
            {"type": "function",
             "function": {"name": "a", "parameters": {"type": "object"}},
             "cache_control": {"type": "ephemeral"}},
            {"type": "function",
             "function": {"name": "b", "cache_control": {"type": "ephemeral", "ttl": "1h"}}},
            {"type": "function", "function": {"name": "c"}},
        ]);
        let translated = translate_openai_tools_to_anthropic(tools).unwrap();
        assert_eq!(
            translated[0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert_eq!(
            translated[1]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert!(translated[2].get("cache_control").is_none());
    }

    /// Web search is billed PER SEARCH on top of tokens
    /// (<https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>),
    /// so a response carrying searches costs more than any token counter
    /// accounts for. Without reading this the spend is invisible.
    #[test]
    fn server_tool_counters_are_read_off_a_non_streaming_response() {
        let raw: AnthropicResponse = serde_json::from_value(serde_json::json!({
            "id": "msg_1",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 4,
                "server_tool_use": {"web_search_requests": 3, "web_fetch_requests": 2}
            }
        }))
        .unwrap();
        let resp = response_into_chat_response(raw);
        assert_eq!(resp.usage.web_search_requests, 3);
        assert_eq!(resp.usage.web_fetch_requests, 2);
        // NOT a token class: folding a per-search charge into a token total
        // would both inflate the token figure and hide the charge.
        assert_eq!(
            resp.usage.total_tokens, 14,
            "server-tool counts must stay out of the token total",
        );
    }

    /// The overwhelming majority of requests declare no server tool, so an
    /// absent block is zero rather than a parse failure.
    #[test]
    fn an_absent_server_tool_block_reads_as_zero() {
        let raw: AnthropicResponse = serde_json::from_value(serde_json::json!({
            "id": "msg_1",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 4}
        }))
        .unwrap();
        let resp = response_into_chat_response(raw);
        assert_eq!(resp.usage.web_search_requests, 0);
        assert_eq!(resp.usage.web_fetch_requests, 0);
    }

    /// A caller that declares an Anthropic server tool is not asking for a
    /// translation — it already wrote the target's shape. Dropping it left
    /// the model answering without the tool, which for `web_search` means
    /// answering from stale training data while the caller believes it
    /// searched.
    #[test]
    fn an_anthropic_server_tool_is_forwarded_verbatim() {
        let tools = serde_json::json!([
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 5,
             "allowed_domains": ["example.com"]},
            {"type": "web_fetch_20250910", "name": "web_fetch"},
            {"type": "function", "function": {"name": "get_weather"}},
        ]);
        let translated = translate_openai_tools_to_anthropic(tools).unwrap();
        assert_eq!(translated.len(), 3);
        // Verbatim, including the optional fields the gateway does not
        // model — a re-serialising translation would drop whichever ones
        // it did not know about.
        assert_eq!(
            translated[0],
            serde_json::json!({
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": 5,
                "allowed_domains": ["example.com"]
            })
        );
        assert_eq!(translated[1]["type"], "web_fetch_20250910");
        // The function tool still translates into Anthropic's client-tool
        // shape alongside it.
        assert_eq!(translated[2]["name"], "get_weather");
    }

    /// An Anthropic client tool (`{name, description, input_schema}`, no
    /// `type`) is the other shape a caller may already have written.
    #[test]
    fn an_anthropic_client_tool_shape_is_forwarded_verbatim() {
        let tools = serde_json::json!([
            {"name": "lookup", "description": "d",
             "input_schema": {"type": "object", "properties": {}}},
        ]);
        let translated = translate_openai_tools_to_anthropic(tools).unwrap();
        assert_eq!(translated[0]["name"], "lookup");
        assert_eq!(translated[0]["input_schema"]["type"], "object");
    }

    /// The mirror: an OpenAI built-in tool is NOT rewritten into an
    /// Anthropic server tool. The two run different indexes and return
    /// different citation shapes, so a mapping would look successful while
    /// answering from somewhere else.
    #[test]
    fn an_openai_builtin_tool_is_not_invented_into_an_anthropic_one() {
        let tools = serde_json::json!([
            {"type": "code_interpreter", "container": {"type": "auto"}},
            {"type": "custom", "custom": {"name": "freeform"}},
            {"type": "function", "function": {"name": "kept"}},
        ]);
        let translated = translate_openai_tools_to_anthropic(tools).unwrap();
        assert_eq!(
            translated.len(),
            1,
            "only the function tool survives: {translated:?}",
        );
        assert_eq!(translated[0]["name"], "kept");
    }

    /// Build a request from `req`, run injection with `ttl`, and return
    /// the serialized wire body — what the upstream actually receives.
    fn injected_wire(req: &ChatFormat, ttl: &str) -> serde_json::Value {
        let (system, messages) = split_system(req).unwrap();
        let mut built = build_request(req, "claude-sonnet-4-5", system, messages, false);
        inject_cache_breakpoints(&mut built, ttl);
        serde_json::to_value(&built).unwrap()
    }

    #[test]
    fn inject_marks_last_system_block_and_final_message_block() {
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::system("big stable prefix"),
                ChatMessage::user("first"),
                ChatMessage::assistant("answer"),
                ChatMessage::user("latest"),
            ],
        );
        let wire = injected_wire(&req, "5m");
        // Plain-string system promotes to a one-block array carrying the
        // 5m marker (bare ephemeral form).
        assert_eq!(
            wire["system"],
            serde_json::json!([
                {"type": "text", "text": "big stable prefix",
                 "cache_control": {"type": "ephemeral"}},
            ])
        );
        // Only the final message's last block is marked; earlier turns
        // stay clean.
        let msgs = wire["messages"].as_array().unwrap();
        assert!(msgs[0]["content"][0].get("cache_control").is_none());
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(
            last["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn inject_one_hour_ttl_carries_explicit_ttl() {
        let req = ChatFormat::new("claude", vec![ChatMessage::user("hi")]);
        let wire = injected_wire(&req, "1h");
        let msgs = wire["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn inject_with_no_system_marks_only_the_trailing_message() {
        let req = ChatFormat::new("claude", vec![ChatMessage::user("hi")]);
        let wire = injected_wire(&req, "5m");
        assert!(wire.get("system").is_none() || wire["system"].is_null());
        assert_eq!(
            wire["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn inject_stands_down_when_client_marked_a_message_block() {
        // Client set its own marker → gateway injects nothing, anywhere.
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::system("prefix"),
                block_message(
                    Role::User,
                    serde_json::json!([
                        {"type": "text", "text": "hi",
                         "cache_control": {"type": "ephemeral"}},
                    ]),
                ),
            ],
        );
        let wire = injected_wire(&req, "5m");
        // System stayed the plain string form (never promoted / marked).
        assert_eq!(wire["system"], serde_json::json!("prefix"));
        // The one marker present is the client's; no second one added.
        assert_eq!(
            wire["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn inject_stands_down_when_client_marked_a_tool() {
        // A marker on a tool definition also suppresses injection — the
        // stand-down scan covers tools, not just messages/system.
        let mut req = ChatFormat::new("claude", vec![ChatMessage::user("hi")]);
        req.extra.insert(
            "tools".into(),
            serde_json::json!([{
                "type": "function",
                "function": {"name": "f", "parameters": {"type": "object"}},
                "cache_control": {"type": "ephemeral"},
            }]),
        );
        let wire = injected_wire(&req, "5m");
        assert!(wire["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn inject_skips_empty_text_final_block() {
        // A final user turn carrying nothing mappable degrades to a single
        // empty text block. Anthropic 400s on cache_control attached to an
        // empty text block, so the injector must leave it unmarked.
        // (Audio, not an image: images map to real blocks now.)
        let req = ChatFormat::new(
            "claude",
            vec![
                ChatMessage::system("prefix"),
                block_message(
                    Role::User,
                    serde_json::json!([
                        {"type": "input_audio", "input_audio": {"data": "xyz", "format": "wav"}},
                    ]),
                ),
            ],
        );
        let wire = injected_wire(&req, "5m");
        // System still gets its marker (non-empty), but the degraded
        // empty final block does not.
        assert_eq!(
            wire["system"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert_eq!(
            wire["messages"][0]["content"][0],
            serde_json::json!({"type": "text", "text": ""})
        );
    }

    #[test]
    fn inject_skips_blank_system_prompt() {
        // A whitespace-only system message promotes to an empty text
        // block; it must not be marked (same 400 hazard).
        let req = ChatFormat::new(
            "claude",
            vec![ChatMessage::system("   "), ChatMessage::user("hi")],
        );
        let wire = injected_wire(&req, "5m");
        // System, if emitted as blocks, carries no marker on the empty
        // block; the trailing user message still gets one.
        if let Some(sys) = wire.get("system").filter(|v| v.is_array()) {
            assert!(sys[0].get("cache_control").is_none());
        }
        assert_eq!(
            wire["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn inject_marks_only_the_last_block_of_a_multi_block_final_message() {
        // Final message with several content blocks: only the LAST is
        // marked (a marker means "cache through here"), earlier blocks
        // stay clean — guards against marking the first or every block.
        let req = ChatFormat::new(
            "claude",
            vec![block_message(
                Role::User,
                serde_json::json!([
                    {"type": "text", "text": "block A"},
                    {"type": "text", "text": "block B"},
                    {"type": "text", "text": "block C"},
                ]),
            )],
        );
        let wire = injected_wire(&req, "5m");
        let content = wire["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert!(content[0].get("cache_control").is_none());
        assert!(content[1].get("cache_control").is_none());
        assert_eq!(
            content[2]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn inject_appends_marker_to_client_block_form_system() {
        // Client sent a block-form system with NO marker of its own:
        // injection marks its last block in place (no string promotion).
        let req = ChatFormat::new(
            "claude",
            vec![
                block_message(
                    Role::System,
                    serde_json::json!([
                        {"type": "text", "text": "line one"},
                        {"type": "text", "text": "line two"},
                    ]),
                ),
                ChatMessage::user("hi"),
            ],
        );
        let wire = injected_wire(&req, "5m");
        let sys = wire["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert!(sys[0].get("cache_control").is_none());
        assert_eq!(
            sys[1]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn build_request_applies_default_max_tokens_when_unset() {
        let req = ChatFormat::new("claude", vec![ChatMessage::user("hi")]);
        let (_system, messages) = split_system(&req).unwrap();
        let built = build_request(&req, "claude-sonnet-4-5", None, messages, false);
        assert_eq!(built.max_tokens, DEFAULT_MAX_TOKENS);

        let req = ChatFormat {
            max_tokens: Some(256),
            ..ChatFormat::new("claude", vec![ChatMessage::user("hi")])
        };
        let (_system, messages) = split_system(&req).unwrap();
        let built = build_request(&req, "claude-sonnet-4-5", None, messages, false);
        assert_eq!(built.max_tokens, 256);
    }

    #[test]
    fn tool_use_block_translates_to_openai_tool_calls_in_extra() {
        // Anthropic Messages response with a tool_use content block
        // (the model decided to call a tool) — verbatim shape from
        // <https://docs.anthropic.com/en/api/messages#example-of-tool-use>.
        let body = r#"{
            "id": "msg_tool_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet-20241022",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": "get_weather",
                    "input": {"location": "San Francisco, CA", "unit": "celsius"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 12, "output_tokens": 8}
        }"#;
        let raw: AnthropicResponse = serde_json::from_str(body).unwrap();
        let out = response_into_chat_response(raw);

        // stop_reason "tool_use" → finish_reason ToolCalls.
        assert_eq!(out.finish_reason, FinishReason::ToolCalls);
        // #395: when only tool_use blocks are emitted (no text), the
        // OpenAI-shape content is `null`, not `""`.
        assert_eq!(out.message.content, None);

        // tool_calls translation lives in `message.extra` so the
        // proxy renderer flattens it onto the wire as a top-level
        // OpenAI-shape field.
        let tool_calls = out
            .message
            .extra
            .get("tool_calls")
            .expect("tool_calls populated in extra")
            .as_array()
            .expect("tool_calls is an array");
        assert_eq!(tool_calls.len(), 1);
        let tc = &tool_calls[0];
        assert_eq!(tc["id"], "toolu_abc");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "get_weather");
        // OpenAI's `arguments` is a JSON-encoded STRING, not the
        // parsed object — SDK consumers `JSON.parse` it.
        let args_str = tc["function"]["arguments"]
            .as_str()
            .expect("arguments is a string");
        let args: serde_json::Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(args["location"], "San Francisco, CA");
        assert_eq!(args["unit"], "celsius");
    }

    #[test]
    fn mixed_text_and_tool_use_blocks_both_surface() {
        // The model can emit text BEFORE invoking a tool. Both must
        // reach the OpenAI-SDK caller: text → message.content,
        // tool_use → message.extra["tool_calls"].
        let body = r#"{
            "id": "msg_mixed_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet-20241022",
            "content": [
                {"type": "text", "text": "Let me check the weather."},
                {"type": "tool_use", "id": "toolu_x", "name": "get_weather",
                 "input": {"location": "NYC"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 10}
        }"#;
        let raw: AnthropicResponse = serde_json::from_str(body).unwrap();
        let out = response_into_chat_response(raw);
        assert_eq!(out.message.content_str(), "Let me check the weather.");
        assert!(out.message.extra.get("tool_calls").is_some());
    }

    #[test]
    fn explicit_empty_text_block_stays_empty_string_not_null() {
        // #395 refinement: distinguish "no text block at all" (→ null)
        // from an explicit empty text block (→ ""). A response carrying
        // a `{"type":"text","text":""}` block must surface `Some("")`,
        // not `None`.
        let body = r#"{
            "id": "msg_empty_text_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet-20241022",
            "content": [{"type": "text", "text": ""}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 0}
        }"#;
        let raw: AnthropicResponse = serde_json::from_str(body).unwrap();
        let out = response_into_chat_response(raw);
        assert_eq!(out.message.content, Some(String::new()));
    }

    #[test]
    fn parallel_tool_use_blocks_emit_array_in_order() {
        // Anthropic supports parallel tool calls — multiple tool_use
        // blocks in one response. Each must produce a tool_calls
        // entry, in the same order as the upstream emitted them.
        let body = r#"{
            "id": "msg_parallel_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet-20241022",
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                 "input": {"location": "SF"}},
                {"type": "tool_use", "id": "toolu_2", "name": "get_time",
                 "input": {"timezone": "PST"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        }"#;
        let raw: AnthropicResponse = serde_json::from_str(body).unwrap();
        let out = response_into_chat_response(raw);
        let tool_calls = out
            .message
            .extra
            .get("tool_calls")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0]["id"], "toolu_1");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
        assert_eq!(tool_calls[1]["id"], "toolu_2");
        assert_eq!(tool_calls[1]["function"]["name"], "get_time");
    }

    #[test]
    fn tool_use_with_no_input_emits_empty_object_arguments() {
        // OpenAI emits `arguments: "{}"` for no-args tool calls, not
        // `"null"`. SDK consumers do `JSON.parse(arguments)` — `null`
        // yields a non-object, breaking idiomatic agent code.
        let body = r#"{
            "id": "msg_no_args",
            "type": "message",
            "role": "assistant",
            "model": "c",
            "content": [
                {"type": "tool_use", "id": "tu", "name": "noop"}
            ],
            "stop_reason": "tool_use"
        }"#;
        let raw: AnthropicResponse = serde_json::from_str(body).unwrap();
        let out = response_into_chat_response(raw);
        let tc = &out.message.extra["tool_calls"][0];
        assert_eq!(tc["function"]["arguments"], "{}");
    }

    #[test]
    fn tool_choice_string_forms_translate_to_anthropic_object_shape() {
        // OpenAI: "auto" | "none" | "required"
        // Anthropic: {"type":"auto"} | {"type":"none"} | {"type":"any"}
        assert_eq!(
            translate_openai_tool_choice_to_anthropic(serde_json::json!("auto")),
            Some(serde_json::json!({"type": "auto"})),
        );
        assert_eq!(
            translate_openai_tool_choice_to_anthropic(serde_json::json!("none")),
            Some(serde_json::json!({"type": "none"})),
        );
        // "required" → "any" (Anthropic's name for "must call something")
        assert_eq!(
            translate_openai_tool_choice_to_anthropic(serde_json::json!("required")),
            Some(serde_json::json!({"type": "any"})),
        );
    }

    #[test]
    fn tool_choice_specific_function_translates_to_anthropic_tool() {
        // OpenAI: {type:"function", function:{name:"X"}}
        // Anthropic: {type:"tool", name:"X"}
        let openai = serde_json::json!({
            "type": "function",
            "function": {"name": "get_weather"}
        });
        assert_eq!(
            translate_openai_tool_choice_to_anthropic(openai),
            Some(serde_json::json!({"type": "tool", "name": "get_weather"})),
        );
    }

    #[test]
    fn tool_choice_unrecognised_shape_drops_to_none() {
        // Strip the field rather than forwarding an OpenAI shape
        // Anthropic doesn't recognise.
        assert_eq!(
            translate_openai_tool_choice_to_anthropic(serde_json::json!("invalid_form")),
            None,
        );
        assert_eq!(
            translate_openai_tool_choice_to_anthropic(serde_json::json!(42)),
            None,
        );
    }

    // ─── Anthropic → OpenAI tool translation (#236) ──────────────

    #[test]
    fn anthropic_tools_translate_to_openai_function_shape() {
        let anthropic = serde_json::json!([
            {
                "name": "get_weather",
                "description": "Get current weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        ]);
        let result = translate_anthropic_tools_to_openai(anthropic).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "get_weather");
        assert_eq!(arr[0]["function"]["description"], "Get current weather");
        assert_eq!(arr[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn anthropic_tool_without_description_or_schema_still_translates() {
        let anthropic = serde_json::json!([{"name": "noop"}]);
        let result = translate_anthropic_tools_to_openai(anthropic).unwrap();
        let tool = &result.as_array().unwrap()[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "noop");
        assert!(tool["function"].get("description").is_none());
        assert!(tool["function"].get("parameters").is_none());
    }

    #[test]
    fn anthropic_tools_non_array_returns_none() {
        assert!(translate_anthropic_tools_to_openai(serde_json::json!("not_array")).is_none());
    }

    #[test]
    fn anthropic_tools_empty_array_returns_none() {
        assert!(translate_anthropic_tools_to_openai(serde_json::json!([])).is_none());
    }

    #[test]
    fn anthropic_tools_entries_without_name_are_skipped() {
        let anthropic = serde_json::json!([
            {"description": "no name field"},
            {"name": "valid", "description": "ok"}
        ]);
        let result = translate_anthropic_tools_to_openai(anthropic).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["function"]["name"], "valid");
    }

    #[test]
    fn anthropic_tool_choice_auto_translates() {
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(serde_json::json!({"type": "auto"})),
            Some(serde_json::json!("auto")),
        );
    }

    #[test]
    fn anthropic_tool_choice_any_translates_to_required() {
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(serde_json::json!({"type": "any"})),
            Some(serde_json::json!("required")),
        );
    }

    #[test]
    fn anthropic_tool_choice_none_translates() {
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(serde_json::json!({"type": "none"})),
            Some(serde_json::json!("none")),
        );
    }

    #[test]
    fn anthropic_tool_choice_specific_tool_translates() {
        let anthropic = serde_json::json!({"type": "tool", "name": "get_weather"});
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(anthropic),
            Some(serde_json::json!({"type": "function", "function": {"name": "get_weather"}})),
        );
    }

    #[test]
    fn anthropic_tool_choice_unrecognised_returns_none() {
        assert!(
            translate_anthropic_tool_choice_to_openai(serde_json::json!({"type": "unknown"}))
                .is_none()
        );
        assert!(translate_anthropic_tool_choice_to_openai(serde_json::json!("auto")).is_none());
        assert!(translate_anthropic_tool_choice_to_openai(serde_json::json!(42)).is_none());
    }

    #[test]
    fn build_request_strips_tool_choice_from_extra() {
        // Even when the value is unrecognised, tool_choice MUST NOT
        // leak into `extra` — forwarding the OpenAI shape would 400
        // the upstream.
        let req = ChatFormat {
            extra: {
                let mut m = serde_json::Map::new();
                m.insert("tool_choice".to_string(), serde_json::json!("auto"));
                m.insert("custom_field".to_string(), serde_json::json!("kept"));
                m
            },
            ..ChatFormat::new("c", vec![ChatMessage::user("hi")])
        };
        let (_system, messages) = split_system(&req).unwrap();
        let built = build_request(&req, "c-name", None, messages, false);
        // tool_choice translated and on the typed field.
        assert_eq!(built.tool_choice, Some(serde_json::json!({"type": "auto"})));
        // tool_choice removed from `extra`; other fields preserved.
        assert!(!built.extra.contains_key("tool_choice"));
        assert_eq!(
            built.extra.get("custom_field"),
            Some(&serde_json::json!("kept"))
        );
    }

    #[test]
    fn non_streaming_response_concatenates_text_blocks() {
        let body = r#"{
            "id": "msg_01A",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "text", "text": "hel"},
                {"type": "text", "text": "lo"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2}
        }"#;
        let raw: AnthropicResponse = serde_json::from_str(body).unwrap();
        let out = response_into_chat_response(raw);
        assert_eq!(out.id, "msg_01A");
        assert_eq!(out.message.content_str(), "hello");
        assert_eq!(out.finish_reason, FinishReason::Stop);
        assert_eq!(out.usage.total_tokens, 5);
    }

    #[test]
    fn cache_creation_and_read_counters_populate_when_present() {
        // Verified shape from
        // https://docs.anthropic.com/en/api/messages (usage object
        // with cache_creation_input_tokens + cache_read_input_tokens).
        let body = r#"{
            "id": "msg_cache_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 4,
                "cache_creation_input_tokens": 200,
                "cache_read_input_tokens": 800
            }
        }"#;
        let raw: AnthropicResponse = serde_json::from_str(body).unwrap();
        let out = response_into_chat_response(raw);
        assert_eq!(out.usage.prompt_tokens, 10);
        assert_eq!(out.usage.completion_tokens, 4);
        assert_eq!(out.usage.cache_creation_tokens, 200);
        assert_eq!(out.usage.cache_read_tokens, 800);
        // #906: cache_creation / cache_read are input classes on top of
        // input_tokens, so the honest total folds them in — not just
        // input + output (which would be 14 and under-count by 1000).
        assert_eq!(out.usage.total_tokens, 10 + 4 + 200 + 800);
        // Anthropic doesn't use OpenAI's cached_prompt / reasoning
        // taxonomy — these stay 0.
        assert_eq!(out.usage.cached_prompt_tokens, 0);
        assert_eq!(out.usage.reasoning_tokens, 0);
    }

    #[test]
    fn stop_reason_mappings_match_spec() {
        assert_eq!(map_stop_reason(Some("end_turn")), FinishReason::Stop);
        assert_eq!(map_stop_reason(Some("max_tokens")), FinishReason::Length);
        assert_eq!(map_stop_reason(Some("tool_use")), FinishReason::ToolCalls);
        assert_eq!(
            map_stop_reason(Some("exotic_reason")),
            FinishReason::Other("exotic_reason".into())
        );
        assert_eq!(map_stop_reason(None), FinishReason::Stop);
    }

    #[test]
    fn content_blocks_other_than_text_are_skipped() {
        // Tool-use blocks on a completion we're treating as plain text
        // should not break parsing; they're simply not surfaced yet.
        let body = r#"{
            "id": "msg_02",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": "search", "input": {}},
                {"type": "text", "text": "done"}
            ]
        }"#;
        let raw: AnthropicResponse = serde_json::from_str(body).unwrap();
        let out = response_into_chat_response(raw);
        assert_eq!(out.message.content_str(), "done");
    }

    #[test]
    fn stream_events_deserialise_into_typed_variants() {
        let start: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":1}}}"#,
        )
        .unwrap();
        assert!(matches!(start, AnthropicStreamEvent::MessageStart { .. }));

        let delta: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        )
        .unwrap();
        assert!(matches!(
            delta,
            AnthropicStreamEvent::ContentBlockDelta { .. }
        ));

        let msg_delta: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
        )
        .unwrap();
        assert!(matches!(
            msg_delta,
            AnthropicStreamEvent::MessageDelta { .. }
        ));

        let ping: AnthropicStreamEvent = serde_json::from_str(r#"{"type":"ping"}"#).unwrap();
        assert!(matches!(ping, AnthropicStreamEvent::Other));
    }

    #[test]
    fn stream_state_tracks_id_and_emits_text_delta() {
        let mut state = StreamState::default();
        let start: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"msg_9","model":"claude-sonnet-4-5","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":1}}}"#,
        )
        .unwrap();
        state.update(&start);
        assert_eq!(state.id, "msg_9");
        assert!(state.to_chunk(&start).is_none());

        let delta: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        )
        .unwrap();
        let chunk = state.to_chunk(&delta).unwrap();
        assert_eq!(chunk.id, "msg_9");
        assert_eq!(chunk.delta.content.as_deref(), Some("hi"));
    }

    #[test]
    fn stream_state_emits_finish_on_message_delta() {
        let state = StreamState {
            id: "msg".into(),
            model: "claude".into(),
            ..Default::default()
        };
        let end: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        )
        .unwrap();
        let chunk = state.to_chunk(&end).unwrap();
        assert_eq!(chunk.finish_reason, Some(FinishReason::Stop));
        assert_eq!(chunk.usage.unwrap().completion_tokens, 3);
    }

    #[test]
    fn stream_state_carries_message_start_input_tokens_into_final_usage() {
        // message_start input_tokens must survive into the usage emitted on
        // the terminal message_delta — otherwise prompt tokens are dropped
        // for the whole stream (TPM/budget/telemetry undercount). See #450.
        let mut state = StreamState::default();
        let start: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"m","model":"claude","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":37,"output_tokens":1}}}"#,
        )
        .unwrap();
        state.update(&start);
        assert_eq!(state.input_tokens, 37);

        let end: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":52}}"#,
        )
        .unwrap();
        let usage = state.to_chunk(&end).unwrap().usage.unwrap();
        assert_eq!(usage.prompt_tokens, 37);
        assert_eq!(usage.completion_tokens, 52);
        assert_eq!(usage.total_tokens, 89);
    }

    #[test]
    fn stream_state_carries_message_start_cache_tokens_into_final_usage() {
        // #906: Anthropic sends cache_creation / cache_read only on
        // message_start. The cross-protocol bridge must carry them onto
        // the terminal usage (and fold them into the total), else an
        // OpenAI-shape client streaming against an Anthropic upstream
        // loses cache tokens entirely — not just from the total.
        let mut state = StreamState::default();
        let start: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"id":"m","model":"claude","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1,"cache_creation_input_tokens":200,"cache_read_input_tokens":800}}}"#,
        )
        .unwrap();
        state.update(&start);
        assert_eq!(state.cache_creation_tokens, 200);
        assert_eq!(state.cache_read_tokens, 800);

        let end: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
        )
        .unwrap();
        let usage = state.to_chunk(&end).unwrap().usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(usage.cache_creation_tokens, 200);
        assert_eq!(usage.cache_read_tokens, 800);
        assert_eq!(usage.total_tokens, 10 + 4 + 200 + 800);
        // Option A: cache is neither folded into prompt_tokens nor
        // mapped onto cached_prompt_tokens — the latter would
        // double-count cost in the control plane's pricing formula, which bills
        // cache_read as its own term.
        assert_eq!(usage.cached_prompt_tokens, 0);
    }

    // ─── parse_inbound_request ────────────────────────────────────

    #[test]
    fn parse_inbound_minimal_user_only() {
        let body = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
        });
        let chat = parse_inbound_request(&body).unwrap();
        assert_eq!(chat.model, "claude-sonnet-4-5");
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, Role::User);
        assert_eq!(chat.messages[0].content_str(), "hi");
        assert_eq!(chat.max_tokens, Some(100));
    }

    #[test]
    fn parse_inbound_system_string_folds_to_leading_message() {
        let body = serde_json::json!({
            "model": "claude",
            "system": "you are helpful",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = parse_inbound_request(&body).unwrap();
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[0].role, Role::System);
        assert_eq!(chat.messages[0].content_str(), "you are helpful");
        assert_eq!(chat.messages[1].role, Role::User);
    }

    #[test]
    fn parse_inbound_system_block_array_concatenates_with_newline() {
        let body = serde_json::json!({
            "model": "claude",
            "system": [
                {"type": "text", "text": "line1"},
                {"type": "text", "text": "line2"},
            ],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = parse_inbound_request(&body).unwrap();
        assert_eq!(chat.messages[0].role, Role::System);
        assert_eq!(chat.messages[0].content_str(), "line1\nline2");
    }

    #[test]
    fn parse_inbound_content_block_array_concatenates_text_only() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello "},
                    {"type": "image", "source": {"type": "base64", "data": "xx"}},
                    {"type": "text", "text": "world"},
                ],
            }],
        });
        let chat = parse_inbound_request(&body).unwrap();
        // Text still concatenates into `content` (guardrail scans read it)…
        assert_eq!(chat.messages[0].content_str(), "hello world");
        // …and since #722 the image block is preserved as an OpenAI
        // `image_url` part instead of being silently dropped.
        let blocks = chat.messages[0].content_blocks.as_ref().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[1]["type"], "image_url");
    }

    #[test]
    fn parse_inbound_unknown_top_level_keys_flow_to_extra() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": "abc"},
            "tools": [{"name": "get_weather"}],
        });
        let chat = parse_inbound_request(&body).unwrap();
        assert!(chat.extra.contains_key("metadata"));
        assert!(chat.extra.contains_key("tools"));
        assert!(!chat.extra.contains_key("model"));
        assert!(!chat.extra.contains_key("messages"));
    }

    /// #597: Claude Code/cc-switch send `role: "system"` inside `messages[]`
    /// even though the Anthropic spec only allows user/assistant. Parse it
    /// as Role::System instead of rejecting the whole request with a 400.
    #[test]
    fn parse_inbound_accepts_system_role_in_messages() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": "respond in French"},
                {"role": "user", "content": "hello again"},
            ],
        });
        let chat = parse_inbound_request(&body).unwrap();
        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[0].role, Role::User);
        assert_eq!(chat.messages[1].role, Role::System);
        assert_eq!(chat.messages[1].content_str(), "respond in French");
        assert_eq!(chat.messages[2].role, Role::User);
    }

    #[test]
    fn parse_inbound_rejects_unknown_role() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{"role": "tool", "content": "x"}],
        });
        let err = parse_inbound_request(&body).unwrap_err();
        assert!(matches!(err, AnthropicInboundError::UnsupportedRole { .. }));
    }

    #[test]
    fn parse_inbound_rejects_missing_model() {
        let body = serde_json::json!({"messages": []});
        assert!(matches!(
            parse_inbound_request(&body).unwrap_err(),
            AnthropicInboundError::MissingModel,
        ));
    }

    // ─── translate_extras_to_openai_shape (#953) ───────

    #[test]
    fn extras_shape_drops_anthropic_only_fields() {
        let mut extra = serde_json::json!({
            "context_management": {"edits": [{"type": "clear_tool_uses_20250919"}]},
            "top_k": 40,
            "mcp_servers": [{"type": "url", "url": "https://example.com/mcp"}],
            "container": "container_abc",
            "service_tier": "standard_only",
            "betas": ["context-management-2025-06-27"],
            "anthropic_version": "2023-06-01",
        })
        .as_object()
        .unwrap()
        .clone();
        translate_extras_to_openai_shape(&mut extra);
        assert!(extra.is_empty(), "expected all dropped, got: {extra:?}");
    }

    #[test]
    fn extras_shape_translates_mappable_fields() {
        let mut extra = serde_json::json!({
            "stop_sequences": ["\n\nHuman:"],
            "metadata": {"user_id": "user-123"},
            "tools": [{"name": "get_time", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"},
        })
        .as_object()
        .unwrap()
        .clone();
        translate_extras_to_openai_shape(&mut extra);

        assert_eq!(extra.get("stop"), Some(&serde_json::json!(["\n\nHuman:"])));
        assert!(!extra.contains_key("stop_sequences"));
        assert_eq!(extra.get("user"), Some(&serde_json::json!("user-123")));
        assert!(!extra.contains_key("metadata"));
        assert_eq!(
            extra.get("tools").unwrap()[0]["function"]["name"],
            serde_json::json!("get_time")
        );
        assert_eq!(
            extra.get("tool_choice"),
            Some(&serde_json::json!("required"))
        );
    }

    #[test]
    fn extras_shape_metadata_without_user_id_is_dropped() {
        let mut extra = serde_json::json!({"metadata": {"foo": "bar"}})
            .as_object()
            .unwrap()
            .clone();
        translate_extras_to_openai_shape(&mut extra);
        assert!(extra.is_empty());
    }

    #[test]
    fn extras_shape_thinking_buckets_to_reasoning_effort() {
        for (thinking, expected) in [
            (
                serde_json::json!({"type": "enabled", "budget_tokens": 8000}),
                Some("high"),
            ),
            (
                serde_json::json!({"type": "enabled", "budget_tokens": 2048}),
                Some("medium"),
            ),
            (
                serde_json::json!({"type": "enabled", "budget_tokens": 1024}),
                Some("low"),
            ),
            (
                serde_json::json!({"type": "enabled", "budget_tokens": 100}),
                Some("minimal"),
            ),
            (serde_json::json!({"type": "adaptive"}), Some("medium")),
            (serde_json::json!({"type": "disabled"}), None),
        ] {
            let mut extra = serde_json::Map::new();
            extra.insert("thinking".to_string(), thinking.clone());
            translate_extras_to_openai_shape(&mut extra);
            assert_eq!(
                extra.get("reasoning_effort").and_then(|v| v.as_str()),
                expected,
                "thinking = {thinking}"
            );
            assert!(!extra.contains_key("thinking"));
        }
    }

    // ─── chat_response_into_anthropic_json ────────────────────────

    #[test]
    fn render_anthropic_response_basic_shape() {
        let resp = ChatResponse {
            id: "cmpl-1".into(),
            model: "gpt-4o".into(), // upstream — should NOT leak into output
            message: ChatMessage::assistant("hello"),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::new(7, 3),
        };
        let json = chat_response_into_anthropic_json(&resp, "my-claude-alias");
        assert_eq!(json["id"], "cmpl-1");
        assert_eq!(json["type"], "message");
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["model"], "my-claude-alias");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "hello");
        assert_eq!(json["stop_reason"], "end_turn");
        assert!(json["stop_sequence"].is_null());
        assert_eq!(json["usage"]["input_tokens"], 7);
        assert_eq!(json["usage"]["output_tokens"], 3);
    }

    #[test]
    fn render_anthropic_response_finish_reason_mappings() {
        let mk = |fr: FinishReason| {
            let resp = ChatResponse {
                id: "x".into(),
                model: "u".into(),
                message: ChatMessage::assistant(""),
                finish_reason: fr,
                usage: UsageStats::new(0, 0),
            };
            chat_response_into_anthropic_json(&resp, "m")["stop_reason"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(mk(FinishReason::Stop), "end_turn");
        assert_eq!(mk(FinishReason::Length), "max_tokens");
        assert_eq!(mk(FinishReason::ContentFilter), "stop_sequence");
        assert_eq!(mk(FinishReason::ToolCalls), "tool_use");
        assert_eq!(mk(FinishReason::Other("vendor".into())), "end_turn");
    }

    // ─── AnthropicSseEncoder ──────────────────────────────────────

    #[test]
    fn render_anthropic_response_translates_openai_tool_calls_to_tool_use() {
        let mut msg = ChatMessage::assistant("");
        msg.extra.insert(
            "tool_calls".to_string(),
            serde_json::json!([{
                "id": "call_abc",
                "type": "function",
                "function": {
                    "name": "get_time",
                    "arguments": "{\"timezone\":\"UTC\"}"
                }
            }]),
        );
        let resp = ChatResponse {
            id: "cmpl-tc".into(),
            model: "gpt-4o".into(),
            message: msg,
            finish_reason: FinishReason::ToolCalls,
            usage: UsageStats::new(10, 5),
        };
        let json = chat_response_into_anthropic_json(&resp, "my-model");
        assert_eq!(json["stop_reason"], "tool_use");
        let content = json["content"].as_array().unwrap();
        let tool_block = content.iter().find(|b| b["type"] == "tool_use");
        assert!(tool_block.is_some(), "tool_use block must be present");
        let tb = tool_block.unwrap();
        assert_eq!(tb["id"], "call_abc");
        assert_eq!(tb["name"], "get_time");
        assert_eq!(tb["input"]["timezone"], "UTC");
    }

    fn delta_chunk(text: &str) -> ChatChunk {
        ChatChunk {
            id: "cmpl-1".into(),
            model: "u".into(),
            delta: ChatDelta {
                role: None,
                content: Some(text.into()),
                tool_calls: None,
                function_call: None,
                reasoning_content: None,
            },
            finish_reason: None,
            usage: None,
        }
    }

    fn finish_chunk(out_tokens: u32) -> ChatChunk {
        ChatChunk {
            id: "cmpl-1".into(),
            model: "u".into(),
            delta: ChatDelta::default(),
            finish_reason: Some(FinishReason::Stop),
            usage: Some(UsageStats::new(0, out_tokens)),
        }
    }

    #[test]
    fn sse_encoder_first_content_chunk_emits_message_start_then_block_start_then_delta() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "claude-alias", 5);
        let events = enc.next_events(&delta_chunk("hello"));
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(
            kinds,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(
            events[0].data["message"]["usage"]["input_tokens"], 5,
            "initial input_tokens echoed in message_start"
        );
        assert_eq!(events[0].data["message"]["model"], "claude-alias");
        assert_eq!(events[2].data["delta"]["text"], "hello");
    }

    #[test]
    fn sse_encoder_subsequent_chunks_only_emit_deltas() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "alias", 0);
        let _ = enc.next_events(&delta_chunk("hel"));
        let events = enc.next_events(&delta_chunk("lo"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "content_block_delta");
        assert_eq!(events[0].data["delta"]["text"], "lo");
    }

    #[test]
    fn sse_encoder_finish_chunk_after_content_emits_stop_trio() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "alias", 0);
        let _ = enc.next_events(&delta_chunk("hi"));
        let events = enc.next_events(&finish_chunk(2));
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(
            kinds,
            vec!["content_block_stop", "message_delta", "message_stop"]
        );
        assert_eq!(events[1].data["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[1].data["usage"]["output_tokens"], 2);
        assert!(enc.is_finished());
        // Subsequent chunks are silent.
        assert!(enc.next_events(&delta_chunk("ignored")).is_empty());
    }

    /// #790: OpenAI's `stream_options.include_usage` frame arrives
    /// AFTER the stop chunk. The closing pair must wait for it so the
    /// client sees real token counts instead of `output_tokens: 0`.
    #[test]
    fn sse_encoder_holds_close_until_post_stop_usage_frame() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "alias", 0);
        let _ = enc.next_events(&delta_chunk("hi"));

        // Stop chunk with NO usage — only the content block closes.
        let stop_no_usage = ChatChunk {
            id: "cmpl-1".into(),
            model: "u".into(),
            delta: ChatDelta::default(),
            finish_reason: Some(FinishReason::Stop),
            usage: None,
        };
        let events = enc.next_events(&stop_no_usage);
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(kinds, vec!["content_block_stop"]);
        assert!(!enc.is_finished());

        // The trailing usage-only frame releases the closing pair,
        // carrying both input and output tokens.
        let usage_only = ChatChunk {
            id: "cmpl-1".into(),
            model: "u".into(),
            delta: ChatDelta::default(),
            finish_reason: None,
            usage: Some(UsageStats::new(17, 23)),
        };
        let events = enc.next_events(&usage_only);
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(kinds, vec!["message_delta", "message_stop"]);
        assert_eq!(events[0].data["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[0].data["usage"]["input_tokens"], 17);
        assert_eq!(events[0].data["usage"]["output_tokens"], 23);
        assert!(enc.is_finished());
    }

    /// An upstream that ignores `stream_options` never sends the usage
    /// frame — stream end (force_finish) must flush the withheld pair
    /// with the REAL stop reason, not `end_turn`.
    #[test]
    fn sse_encoder_force_finish_flushes_held_close_with_real_stop_reason() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "alias", 0);
        let _ = enc.next_events(&delta_chunk("hi"));
        let stop_no_usage = ChatChunk {
            id: "cmpl-1".into(),
            model: "u".into(),
            delta: ChatDelta::default(),
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        };
        let _ = enc.next_events(&stop_no_usage);
        assert!(!enc.is_finished());

        let events = enc.force_finish();
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(kinds, vec!["message_delta", "message_stop"]);
        assert_eq!(events[0].data["delta"]["stop_reason"], "tool_use");
        assert_eq!(events[0].data["usage"]["output_tokens"], 0);
        assert!(enc.is_finished());
    }

    #[test]
    fn sse_encoder_finish_only_chunk_skips_content_block_stop() {
        // Finish without prior content (e.g. blocked by guardrail) —
        // we still emit message_start + message_delta + message_stop
        // but NOT content_block_start/stop.
        let mut enc = AnthropicSseEncoder::new("msg_01", "alias", 0);
        let events = enc.next_events(&finish_chunk(0));
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(
            kinds,
            vec!["message_start", "message_delta", "message_stop"]
        );
    }

    #[test]
    fn sse_encoder_force_finish_after_content_emits_full_close() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "alias", 3);
        let _ = enc.next_events(&delta_chunk("hi"));
        let events = enc.force_finish();
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(
            kinds,
            vec!["content_block_stop", "message_delta", "message_stop"]
        );
        assert!(enc.is_finished());
    }

    #[test]
    fn sse_encoder_force_finish_on_empty_stream_emits_message_start_then_close() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "alias", 0);
        let events = enc.force_finish();
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(
            kinds,
            vec!["message_start", "message_delta", "message_stop"]
        );
    }

    #[test]
    fn sse_event_renders_as_event_data_pair() {
        let ev = AnthropicSseEvent {
            event: "content_block_delta",
            data: serde_json::json!({"x": 1}),
        };
        let s = ev.to_sse_string();
        assert_eq!(s, "event: content_block_delta\ndata: {\"x\":1}\n\n");
    }

    // ─── Streaming tool_calls ──────────────────────────────────────

    fn tool_call_chunk(index: u64, id: &str, name: &str, arguments: &str) -> ChatChunk {
        let mut tc = serde_json::json!({"index": index});
        if !id.is_empty() {
            tc["id"] = serde_json::json!(id);
            tc["type"] = serde_json::json!("function");
        }
        let mut func = serde_json::Map::new();
        if !name.is_empty() {
            func.insert("name".into(), serde_json::json!(name));
        }
        if !arguments.is_empty() {
            func.insert("arguments".into(), serde_json::json!(arguments));
        }
        if !func.is_empty() {
            tc["function"] = serde_json::Value::Object(func);
        }
        ChatChunk {
            id: "cmpl-1".into(),
            model: "u".into(),
            delta: ChatDelta {
                role: None,
                content: None,
                tool_calls: Some(vec![tc]),
                function_call: None,
                reasoning_content: None,
            },
            finish_reason: None,
            usage: None,
        }
    }

    fn tool_finish_chunk() -> ChatChunk {
        ChatChunk {
            id: "cmpl-1".into(),
            model: "u".into(),
            delta: ChatDelta::default(),
            finish_reason: Some(FinishReason::ToolCalls),
            usage: Some(UsageStats::new(10, 5)),
        }
    }

    #[test]
    fn sse_encoder_tool_call_emits_block_start_and_argument_deltas() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "m", 0);
        // First chunk: tool header with id+name and initial args.
        let events = enc.next_events(&tool_call_chunk(0, "call_1", "get_weather", "{\"loc"));
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(
            kinds,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
        // content_block_start should be tool_use
        assert_eq!(events[1].data["content_block"]["type"], "tool_use");
        assert_eq!(events[1].data["content_block"]["id"], "call_1");
        assert_eq!(events[1].data["content_block"]["name"], "get_weather");
        // content_block_delta should be input_json_delta
        assert_eq!(events[2].data["delta"]["type"], "input_json_delta");
        assert_eq!(events[2].data["delta"]["partial_json"], "{\"loc");
    }

    #[test]
    fn sse_encoder_tool_call_subsequent_args_emit_delta_only() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "m", 0);
        enc.next_events(&tool_call_chunk(0, "call_1", "get_weather", ""));
        let events = enc.next_events(&tool_call_chunk(0, "", "", "ation\"}"));
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(kinds, vec!["content_block_delta"]);
        assert_eq!(events[0].data["delta"]["partial_json"], "ation\"}");
    }

    #[test]
    fn sse_encoder_tool_finish_closes_all_blocks() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "m", 0);
        enc.next_events(&tool_call_chunk(0, "call_1", "fn_a", "{}"));
        enc.next_events(&tool_call_chunk(1, "call_2", "fn_b", "{}"));
        let events = enc.next_events(&tool_finish_chunk());
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        // Should close both tool blocks, then message_delta + message_stop
        assert_eq!(
            kinds,
            vec![
                "content_block_stop",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[2].data["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn sse_encoder_mixed_text_and_tool_call() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "m", 0);
        // Text first
        enc.next_events(&delta_chunk("thinking..."));
        // Then a tool call
        let events = enc.next_events(&tool_call_chunk(0, "call_1", "search", "{\"q\":\"x\"}"));
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(kinds, vec!["content_block_start", "content_block_delta"]);
        // Tool block should be at index 1 (text was 0)
        assert_eq!(events[0].data["index"], 1);
        // Finish
        let events = enc.next_events(&tool_finish_chunk());
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        // Close text block (0), tool block (1), then message_delta + stop
        assert_eq!(
            kinds,
            vec![
                "content_block_stop",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
    }

    #[test]
    fn sse_encoder_force_finish_closes_tool_blocks() {
        let mut enc = AnthropicSseEncoder::new("msg_01", "m", 0);
        enc.next_events(&tool_call_chunk(0, "call_1", "fn_a", "{}"));
        let events = enc.force_finish();
        let kinds: Vec<_> = events.iter().map(|e| e.event).collect();
        assert_eq!(
            kinds,
            vec!["content_block_stop", "message_delta", "message_stop"]
        );
    }
    // ─── #722: cross-provider content-block translation ─────────────

    #[test]
    fn inbound_image_base64_becomes_data_url_image_part() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this?"},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "aGk="
                    }},
                ],
            }],
        });
        let chat = parse_inbound_request(&body).unwrap();
        assert_eq!(chat.messages.len(), 1);
        let blocks = chat.messages[0].content_blocks.as_ref().unwrap();
        assert_eq!(
            blocks[0],
            serde_json::json!({"type": "text", "text": "what is this?"})
        );
        assert_eq!(
            blocks[1],
            serde_json::json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}})
        );
        assert_eq!(chat.messages[0].content_str(), "what is this?");
    }

    #[test]
    fn inbound_image_url_and_document_become_image_parts() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://x.example/cat.png"}},
                    {"type": "document", "source": {
                        "type": "base64", "media_type": "application/pdf", "data": "cGRm"
                    }},
                ],
            }],
        });
        let chat = parse_inbound_request(&body).unwrap();
        let blocks = chat.messages[0].content_blocks.as_ref().unwrap();
        assert_eq!(blocks[0]["image_url"]["url"], "https://x.example/cat.png");
        assert_eq!(
            blocks[1]["image_url"]["url"],
            "data:application/pdf;base64,cGRm"
        );
    }

    #[test]
    fn inbound_assistant_tool_use_becomes_openai_tool_calls() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "weather in SF?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                     "input": {"city": "SF"}},
                ]},
            ],
        });
        let chat = parse_inbound_request(&body).unwrap();
        let assistant = &chat.messages[1];
        assert_eq!(assistant.content_str(), "checking");
        let calls = assistant
            .extra
            .get("tool_calls")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "toolu_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        let args: serde_json::Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, serde_json::json!({"city": "SF"}));
    }

    #[test]
    fn inbound_pure_tool_use_turn_has_null_content() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}},
            ]}],
        });
        let chat = parse_inbound_request(&body).unwrap();
        // OpenAI's canonical pure-tool-call history turn is content: null.
        assert_eq!(chat.messages[0].content, None);
        assert!(chat.messages[0].extra.contains_key("tool_calls"));
    }

    #[test]
    fn inbound_tool_name_truncates_to_openai_64_char_cap() {
        let long = "x".repeat(80);
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": long, "input": {}},
            ]}],
        });
        let chat = parse_inbound_request(&body).unwrap();
        let calls = chat.messages[0].extra["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["function"]["name"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn inbound_tool_result_becomes_tool_message_before_user_turn() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "and now?"},
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny, 21C"},
                ]},
            ],
        });
        let chat = parse_inbound_request(&body).unwrap();
        // user, assistant(tool_calls), TOOL, user — the tool answer must
        // directly follow the assistant tool_calls turn.
        assert_eq!(chat.messages.len(), 4);
        assert_eq!(chat.messages[2].role, Role::Tool);
        assert_eq!(chat.messages[2].tool_call_id.as_deref(), Some("toolu_1"));
        assert_eq!(chat.messages[2].content_str(), "sunny, 21C");
        assert_eq!(chat.messages[3].role, Role::User);
        assert_eq!(chat.messages[3].content_str(), "and now?");
    }

    #[test]
    fn inbound_tool_result_single_text_block_collapses_to_string() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": [{"type": "text", "text": "42"}]},
            ]}],
        });
        let chat = parse_inbound_request(&body).unwrap();
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, Role::Tool);
        assert_eq!(chat.messages[0].content_str(), "42");
        assert!(chat.messages[0].content_blocks.is_none());
    }

    #[test]
    fn inbound_tool_result_with_image_keeps_combined_parts() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "screenshot:"},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "aWc="
                    }},
                ]},
            ]}],
        });
        let chat = parse_inbound_request(&body).unwrap();
        let tool_msg = &chat.messages[0];
        assert_eq!(tool_msg.role, Role::Tool);
        let parts = tool_msg.content_blocks.as_ref().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
    }

    #[test]
    fn inbound_thinking_blocks_drop_but_text_and_tools_survive() {
        let body = serde_json::json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret chain", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "answer"},
            ]}],
        });
        let chat = parse_inbound_request(&body).unwrap();
        assert_eq!(chat.messages[0].content_str(), "answer");
        assert!(!chat.messages[0].extra.contains_key("tool_calls"));
        // The thinking text must not leak into the translated content.
        assert!(!serde_json::to_string(&chat.messages[0])
            .unwrap()
            .contains("secret chain"));
    }
}
