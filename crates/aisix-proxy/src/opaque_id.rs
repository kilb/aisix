//! Platform mode: response and tool-call ids that name no upstream channel.
//!
//! An upstream's own ids can say which channel served a response: a cloud
//! platform that resells a model often marks the ids it hands out. In
//! platform mode none of them reaches a tenant, and multi-turn tool use keeps
//! working on every protocol:
//!
//! - A Chat Completions or Messages response id is minted fresh
//!   ([`mint_response_id`]), one per response, shared by every frame of a
//!   stream.
//! - A Chat Completions or Messages tool-call id is mapped one way
//!   ([`map_tool_call_id`]). Both protocols carry the whole conversation on
//!   every request, and an upstream pairs a tool call with its result only
//!   inside that one request, so the mapped id travels upstream unchanged on
//!   the next turn and still pairs.
//! - A Responses output item's ids are sealed reversibly ([`seal_id`]) and
//!   unsealed on their way back in ([`unseal_responses_input`]): that protocol
//!   cannot take a substitute id, see there.
//!
//! The usage event keeps the upstream's own id as `provider_request_id`;
//! every rewrite here runs after telemetry has read the response.

use base64::Engine as _;
use ring::{aead, hmac};
use serde_json::Value;

use crate::json_splice::{self, PathSeg};

/// The client-facing protocols whose ids are rewritten one way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    Chat,
    Messages,
}

impl Surface {
    /// Domain separation for [`map_tool_call_id`].
    fn label(self) -> &'static [u8] {
        match self {
            Surface::Chat => b"chat",
            Surface::Messages => b"messages",
        }
    }

    fn response_id_prefix(self) -> &'static str {
        match self {
            Surface::Chat => "chatcmpl-",
            Surface::Messages => "msg_",
        }
    }

    fn tool_call_id_prefix(self) -> &'static str {
        match self {
            Surface::Chat => "call_",
            Surface::Messages => "toolu_",
        }
    }
}

/// A fresh response id in the surface's own shape.
pub(crate) fn mint_response_id(surface: Surface) -> String {
    format!(
        "{}{}",
        surface.response_id_prefix(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The id a tenant sees for an upstream tool-call id: the first 16 bytes of
/// `HMAC-SHA256(key, surface || 0x00 || upstream_id)` in hex, behind the
/// surface's tool-call prefix. The same input always maps to the same id.
pub(crate) fn map_tool_call_id(key: &[u8; 32], surface: Surface, upstream_id: &str) -> String {
    let tag = hmac_parts(key, &[surface.label(), &[0], upstream_id.as_bytes()]);
    let mut id = String::with_capacity(surface.tool_call_id_prefix().len() + 32);
    id.push_str(surface.tool_call_id_prefix());
    for byte in &tag.as_ref()[..16] {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// Whether `id` has the shape of an id this module mints or maps on `surface`.
#[cfg(test)]
pub(crate) fn is_platform_id(surface: Surface, id: &str) -> bool {
    let hex32 = |rest: &str| {
        rest.len() == 32
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    [surface.response_id_prefix(), surface.tool_call_id_prefix()]
        .iter()
        .any(|prefix| id.strip_prefix(prefix).is_some_and(hex32))
}

/// Map the `id` of every call in an OpenAI-shape `tool_calls` list. A
/// streamed continuation carries no id, and an empty one is left empty.
pub(crate) fn map_tool_calls(key: &[u8; 32], surface: Surface, calls: &mut [Value]) {
    for call in calls {
        if let Some(Value::String(id)) = call.get_mut("id") {
            if !id.is_empty() {
                *id = map_tool_call_id(key, surface, id);
            }
        }
    }
}

fn hmac_parts(key: &[u8], parts: &[&[u8]]) -> hmac::Tag {
    let mut ctx = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, key));
    for part in parts {
        ctx.update(part);
    }
    ctx.sign()
}

fn derive_key(key: &[u8], label: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(hmac_parts(key, &[label]).as_ref());
    out
}

/// The key [`seal_id`] takes, derived from the platform `id_key` under its
/// own label so the seal never shares a key with [`map_tool_call_id`].
pub(crate) fn seal_key(id_key: &[u8; 32]) -> [u8; 32] {
    derive_key(id_key, b"seal-v1")
}

/// A seal key that never prints.
#[derive(Clone, Copy)]
pub(crate) struct SealKey([u8; 32]);

impl SealKey {
    pub(crate) fn of(platform: &crate::platform::PlatformSettings) -> Self {
        SealKey(seal_key(&platform.id_key))
    }

    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SealKey(***)")
    }
}

/// The longest leading `<letters and digits>_` a sealed id keeps in the clear.
const MAX_KEPT_PREFIX: usize = 16;

/// The part of an id kept in front of its sealed form: everything up to and
/// including the first `_`, when that is a short run of ASCII letters and
/// digits (`fc_`, `rs_`, `msg_`, `call_`). Anything else keeps nothing, so a
/// path-like id cannot carry a project name into the clear.
fn kept_prefix(id: &str) -> &str {
    match id.find('_') {
        Some(end)
            if (1..=MAX_KEPT_PREFIX).contains(&end)
                && id.as_bytes()[..end].iter().all(u8::is_ascii_alphanumeric) =>
        {
            &id[..=end]
        }
        _ => "",
    }
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Deterministic authenticated encryption of ids under one seal key: the
/// nonce is synthetic, `HMAC-SHA256(key, "seal" || 0x00 || id)[..12]`, so the
/// same id always seals to the same value — a stream's `item_id` on every
/// delta matches the item it belongs to, and a replayed answer matches the
/// first. The AES-256-GCM key is derived from the seal key under its own
/// label, so the nonce function and the cipher never share key material.
struct Sealer {
    nonce_key: hmac::Key,
    cipher: aead::LessSafeKey,
}

impl Sealer {
    fn new(key: &[u8; 32]) -> Self {
        let cipher = aead::UnboundKey::new(&aead::AES_256_GCM, &derive_key(key, b"seal-aead"))
            .expect("a 32-byte key is the length AES-256-GCM takes");
        Self {
            nonce_key: hmac::Key::new(hmac::HMAC_SHA256, key),
            cipher: aead::LessSafeKey::new(cipher),
        }
    }

    fn nonce(&self, id: &str) -> [u8; aead::NONCE_LEN] {
        let mut ctx = hmac::Context::with_key(&self.nonce_key);
        ctx.update(b"seal");
        ctx.update(&[0]);
        ctx.update(id.as_bytes());
        let mut nonce = [0u8; aead::NONCE_LEN];
        nonce.copy_from_slice(&ctx.sign().as_ref()[..aead::NONCE_LEN]);
        nonce
    }

    /// `<kept prefix>` + base64url(nonce || ciphertext || tag), with the kept
    /// prefix as associated data so it cannot be swapped for another.
    fn seal(&self, id: &str) -> String {
        let prefix = kept_prefix(id);
        let nonce = self.nonce(id);
        let mut payload = Vec::with_capacity(aead::NONCE_LEN + id.len() + aead::MAX_TAG_LEN);
        payload.extend_from_slice(&nonce);
        let mut sealed = id.as_bytes().to_vec();
        self.cipher
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(prefix.as_bytes()),
                &mut sealed,
            )
            .expect("an id is far below the AES-GCM message limit");
        payload.extend_from_slice(&sealed);
        format!("{prefix}{}", b64().encode(payload))
    }

    fn unseal(&self, id: &str) -> Option<String> {
        // The payload's alphabet includes `_`, so an id sealed without a
        // prefix is also tried whole.
        id.find('_')
            .and_then(|end| self.open(&id[..=end], &id[end + 1..]))
            .or_else(|| self.open("", id))
    }

    fn open(&self, prefix: &str, payload: &str) -> Option<String> {
        let raw = b64().decode(payload).ok()?;
        if raw.len() <= aead::NONCE_LEN + aead::MAX_TAG_LEN {
            return None;
        }
        let (nonce, sealed) = raw.split_at(aead::NONCE_LEN);
        let mut buf = sealed.to_vec();
        let plain = self
            .cipher
            .open_in_place(
                aead::Nonce::try_assume_unique_for_key(nonce).ok()?,
                aead::Aad::from(prefix.as_bytes()),
                &mut buf,
            )
            .ok()?;
        let plain = String::from_utf8(plain.to_vec()).ok()?;
        (kept_prefix(&plain) == prefix && self.nonce(&plain) == nonce).then_some(plain)
    }
}

/// Seal an upstream id reversibly. The id's leading `<prefix>_` stays in the
/// clear (see [`kept_prefix`]) so the sealed id still passes an upstream's
/// prefix check; the rest names nothing.
pub(crate) fn seal_id(key: &[u8; 32], upstream_id: &str) -> String {
    Sealer::new(key).seal(upstream_id)
}

/// The upstream id a [`seal_id`] value was made from; `None` for anything
/// this key did not seal, which the caller passes through untouched.
pub(crate) fn unseal_id(key: &[u8; 32], id: &str) -> Option<String> {
    Sealer::new(key).unseal(id)
}

/// The fields of a Responses output item that name an item or a call — the
/// values a client echoes back in a later request's `input[]`. A `caller`
/// object's `caller_id` names a call too.
const RESPONSES_ITEM_ID_FIELDS: [&str; 3] = ["id", "call_id", "approval_request_id"];

fn item_id_slots(item: &mut Value) -> impl Iterator<Item = &mut String> {
    let mut slots = Vec::new();
    if let Value::Object(map) = item {
        for (field, value) in map.iter_mut() {
            match value {
                Value::String(s) if RESPONSES_ITEM_ID_FIELDS.contains(&field.as_str()) => {
                    slots.push(s);
                }
                Value::Object(caller) if field == "caller" => {
                    if let Some(Value::String(s)) = caller.get_mut("caller_id") {
                        slots.push(s);
                    }
                }
                _ => {}
            }
        }
    }
    slots.into_iter()
}

/// Seal every item id of a Responses `Response` object, and drop the usage
/// counter the gateway adds beside OpenAI's (an Anthropic-shape cache write,
/// already counted in `input_tokens`). The response-level `id` is not an item
/// id and is left alone.
pub(crate) fn seal_responses_body(key: &[u8; 32], body: &mut Value) {
    if let Some(items) = body.get_mut("output").and_then(Value::as_array_mut) {
        for item in items {
            seal_item(key, item);
        }
    }
    if let Some(details) = body
        .pointer_mut("/usage/input_tokens_details")
        .and_then(Value::as_object_mut)
    {
        details.remove("cache_creation_tokens");
    }
}

fn seal_item(key: &[u8; 32], item: &mut Value) {
    for slot in item_id_slots(item) {
        if !slot.is_empty() {
            *slot = seal_id(key, slot);
        }
    }
}

/// [`seal_responses_body`] for one parsed Responses stream event: the item of
/// `response.output_item.*`, the `item_id` of every per-item event, and the
/// Response object of the snapshot events.
pub(crate) fn seal_responses_event(key: &[u8; 32], event: &mut Value) {
    if let Some(item) = event.get_mut("item") {
        seal_item(key, item);
    }
    if let Some(Value::String(id)) = event.get_mut("item_id") {
        if !id.is_empty() {
            *id = seal_id(key, id);
        }
    }
    if let Some(response) = event.get_mut("response") {
        seal_responses_body(key, response);
    }
}

/// The paths [`seal_responses_event`] seals, for a byte splice over a
/// Responses stream event.
pub(crate) fn responses_event_id_path(path: &[PathSeg]) -> bool {
    fn item_field(rest: &[PathSeg]) -> bool {
        match rest {
            [PathSeg::Key(field)] => RESPONSES_ITEM_ID_FIELDS.contains(&field.as_str()),
            [PathSeg::Key(caller), PathSeg::Key(field)] => {
                caller == "caller" && field == "caller_id"
            }
            _ => false,
        }
    }
    match path {
        [PathSeg::Key(key)] => key == "item_id",
        [PathSeg::Key(item), rest @ ..] if item == "item" => item_field(rest),
        [PathSeg::Key(response), PathSeg::Key(output), PathSeg::Index(_), rest @ ..]
            if response == "response" && output == "output" =>
        {
            item_field(rest)
        }
        _ => false,
    }
}

/// Restore the upstream's own ids in a `/v1/responses` request's `input[]`
/// before anything reads it, on the native and the translated path alike;
/// an id this key did not seal — one the client made — passes untouched.
///
/// Responses cannot take a substitute id the way Chat Completions and
/// Messages can, for two reasons, neither stated in the API reference but
/// both in the API's own error text:
///
/// - A continuation through `previous_response_id` pairs a
///   `function_call_output` with the call the upstream stored, by `call_id`
///   ("The unique ID of the function tool call generated by the model",
///   openai-python `src/openai/types/responses/response_input_item_param.py`
///   lines 183-184 at c6fc2646). An id it never issued is refused: "No tool
///   call found for function call output with call_id …"
///   (<https://community.openai.com/t/1142327>), and a stored item is looked
///   up by its id: "Item with id 'rs_…' not found."
///   (<https://community.openai.com/t/1339944>).
/// - An item id is checked by prefix: "Invalid 'input[3].id': 'abc'.
///   Expected an ID that begins with 'fs'."
///   (<https://community.openai.com/t/1373183>). That is why a sealed id
///   keeps the original's `<prefix>_`.
pub(crate) fn unseal_responses_input(key: &[u8; 32], body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        for slot in item_id_slots(item) {
            if let Some(original) = unseal_id(key, slot) {
                *slot = original;
            }
        }
    }
}

/// The top-level member a cloud platform adds to a relayed Messages answer:
/// its own invocation metrics, named after the platform.
pub(crate) const CHANNEL_METRICS_KEY: &str = "amazon-bedrock-invocationMetrics";

/// `doc` without a top-level [`CHANNEL_METRICS_KEY`] member, byte-preserving;
/// `None` when it has none or cannot be read, and the caller keeps `doc`.
pub(crate) fn strip_channel_metrics(doc: &[u8]) -> Option<Vec<u8>> {
    let key = CHANNEL_METRICS_KEY.as_bytes();
    if !doc.windows(key.len()).any(|w| w == key) {
        return None;
    }
    match json_splice::remove_top_level_member(doc, CHANNEL_METRICS_KEY) {
        Ok(stripped) => stripped,
        Err(error) => {
            tracing::debug!(%error, "channel metrics removal skipped; forwarding verbatim");
            None
        }
    }
}

/// Per-response id state for an Anthropic Messages answer in platform mode:
/// the one message id the whole response carries, and the key its tool-use
/// ids are mapped under. Serves the native relay and the translated path.
pub(crate) struct MessagesIds {
    key: [u8; 32],
    message_id: String,
}

#[derive(Clone, Copy)]
enum MessagesSlot {
    Model,
    MessageId,
    BlockType,
    BlockId,
}

impl MessagesIds {
    pub(crate) fn new(id_key: &[u8; 32]) -> Self {
        Self {
            key: *id_key,
            message_id: mint_response_id(Surface::Messages),
        }
    }

    fn map_block(&self, block: &mut Value) {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return;
        }
        if let Some(Value::String(id)) = block.get_mut("id") {
            *id = map_tool_call_id(&self.key, Surface::Messages, id);
        }
    }

    /// A buffered Messages answer: its `id`, each `tool_use` block's `id`,
    /// and no channel metrics.
    pub(crate) fn apply_to_body(&self, body: &mut Value) {
        if let Some(Value::String(id)) = body.get_mut("id") {
            id.clone_from(&self.message_id);
        }
        if let Some(blocks) = body.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                self.map_block(block);
            }
        }
        if let Some(map) = body.as_object_mut() {
            map.remove(CHANNEL_METRICS_KEY);
        }
    }

    /// One parsed stream event: `message_start.message.id` and a `tool_use`
    /// `content_block_start`'s block id.
    pub(crate) fn apply_to_event(&self, event: &mut Value) {
        if let Some(Value::String(id)) = event.pointer_mut("/message/id") {
            id.clone_from(&self.message_id);
        }
        if let Some(block) = event.get_mut("content_block") {
            self.map_block(block);
        }
        if let Some(map) = event.as_object_mut() {
            map.remove(CHANNEL_METRICS_KEY);
        }
    }

    /// One relayed stream event's JSON, byte-preserving: the caller-facing
    /// `message.model` and the ids [`Self::apply_to_event`] rewrites, in one
    /// splice pass, then any channel metrics. `None` when nothing changed or
    /// the payload cannot be read, and the caller forwards it as it came.
    pub(crate) fn rewrite_event_json(
        &self,
        payload: &[u8],
        client_facing_model: &str,
    ) -> Option<Vec<u8>> {
        let spliced = match self.splice_event(payload, client_facing_model, true) {
            // A block id was mapped before its block's `type` said it is
            // not a tool_use: splice again without it.
            Ok((_, true)) => self.splice_event(payload, client_facing_model, false),
            other => other,
        };
        let spliced = match spliced {
            Ok((spliced, _)) => spliced,
            Err(error) => {
                tracing::debug!(%error, "platform id rewrite skipped; forwarding verbatim");
                return None;
            }
        };
        strip_channel_metrics(spliced.as_deref().unwrap_or(payload)).or(spliced)
    }

    /// One splice pass; the flag says a block id was mapped on a block whose
    /// `type` is not `tool_use`.
    fn splice_event(
        &self,
        payload: &[u8],
        client_facing_model: &str,
        map_block_id: bool,
    ) -> Result<(Option<Vec<u8>>, bool), json_splice::SpliceError> {
        let slot = std::cell::Cell::new(None);
        let mut block_type: Option<String> = None;
        let mut block_id_mapped = false;
        let spliced = json_splice::rewrite_string_values(
            payload,
            |path| {
                let picked = match path {
                    [PathSeg::Key(parent), PathSeg::Key(field)] => {
                        match (parent.as_str(), field.as_str()) {
                            ("message", "model") => Some(MessagesSlot::Model),
                            ("message", "id") => Some(MessagesSlot::MessageId),
                            ("content_block", "type") => Some(MessagesSlot::BlockType),
                            ("content_block", "id") if map_block_id => Some(MessagesSlot::BlockId),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                slot.set(picked);
                picked.is_some()
            },
            |value| match slot.get()? {
                MessagesSlot::Model => Some(client_facing_model.to_string()),
                MessagesSlot::MessageId => Some(self.message_id.clone()),
                MessagesSlot::BlockType => {
                    block_type = Some(value.to_string());
                    None
                }
                MessagesSlot::BlockId => {
                    block_id_mapped = true;
                    Some(map_tool_call_id(&self.key, Surface::Messages, value))
                }
            },
        )?;
        Ok((
            spliced,
            block_id_mapped && block_type.as_deref() != Some("tool_use"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const K: [u8; 32] = [7u8; 32];

    #[test]
    fn a_tool_call_id_maps_the_same_way_every_time() {
        let k = [7u8; 32];
        assert_eq!(
            map_tool_call_id(&k, Surface::Messages, "toolu_bdrk_01ABC"),
            map_tool_call_id(&k, Surface::Messages, "toolu_bdrk_01ABC")
        );
    }

    #[test]
    fn a_mapped_id_hides_the_channel_marker() {
        let id = map_tool_call_id(&[7u8; 32], Surface::Messages, "toolu_bdrk_01ABC");
        assert!(id.starts_with("toolu_") && !id.contains("bdrk"), "{id}");
    }

    #[test]
    fn different_upstream_ids_map_to_different_ids() {
        let k = [7u8; 32];
        assert_ne!(
            map_tool_call_id(&k, Surface::Chat, "call_a"),
            map_tool_call_id(&k, Surface::Chat, "call_b")
        );
    }

    #[test]
    fn mapped_ids_carry_the_surface_prefix_and_depend_on_the_key() {
        let chat = map_tool_call_id(&K, Surface::Chat, "toolu_vrtx_01");
        let messages = map_tool_call_id(&K, Surface::Messages, "toolu_vrtx_01");
        assert!(chat.starts_with("call_"), "{chat}");
        assert!(is_platform_id(Surface::Chat, &chat), "{chat}");
        assert!(is_platform_id(Surface::Messages, &messages), "{messages}");
        assert_ne!(
            chat.trim_start_matches("call_"),
            messages.trim_start_matches("toolu_"),
            "each surface maps under its own label"
        );
        assert_ne!(
            chat,
            map_tool_call_id(&[8u8; 32], Surface::Chat, "toolu_vrtx_01")
        );
    }

    #[test]
    fn minted_response_ids_carry_the_surface_prefix_and_never_repeat() {
        let a = mint_response_id(Surface::Chat);
        let b = mint_response_id(Surface::Chat);
        assert!(
            a.starts_with("chatcmpl-") && is_platform_id(Surface::Chat, &a),
            "{a}"
        );
        assert_ne!(a, b);
        let m = mint_response_id(Surface::Messages);
        assert!(
            m.starts_with("msg_") && is_platform_id(Surface::Messages, &m),
            "{m}"
        );
        assert!(!is_platform_id(Surface::Messages, "msg_bdrk_01X"));
        assert!(!is_platform_id(Surface::Chat, "chatcmpl-up"));
    }

    #[test]
    fn a_sealed_id_keeps_its_prefix_hides_the_rest_and_unseals() {
        for upstream in [
            "fc_bdrk_68af0c1e2b",
            "call_bdrk_01Y",
            "rs_0123456789abcdef",
            "msg_bdrk_01X",
            "toolu_vrtx_01ABC",
        ] {
            let sealed = seal_id(&K, upstream);
            let prefix = &upstream[..=upstream.find('_').unwrap()];
            assert!(sealed.starts_with(prefix), "{sealed} keeps {prefix}");
            assert!(
                !sealed.contains("bdrk") && !sealed.contains("vrtx"),
                "{sealed}"
            );
            assert_ne!(sealed, upstream);
            assert_eq!(unseal_id(&K, &sealed).as_deref(), Some(upstream));
        }
    }

    #[test]
    fn sealing_is_deterministic() {
        assert_eq!(seal_id(&K, "fc_bdrk_1"), seal_id(&K, "fc_bdrk_1"));
        assert_ne!(seal_id(&K, "fc_bdrk_1"), seal_id(&K, "fc_bdrk_2"));
        assert_ne!(seal_id(&K, "fc_bdrk_1"), seal_id(&[8u8; 32], "fc_bdrk_1"));
    }

    /// An id with no plain `<letters>_` head keeps no prefix at all, so
    /// nothing in front of the payload can name anything — and the payload
    /// alone still unseals, even when its own alphabet contains `_`.
    #[test]
    fn an_id_without_a_plain_prefix_seals_whole_and_unseals() {
        for upstream in [
            "0f8e2c1d",
            "projects/p-123/locations/x_y",
            "a-b_c",
            "_leading",
            "averyveryverylongprefix_x",
        ] {
            let sealed = seal_id(&K, upstream);
            assert!(
                !sealed.contains("p-123") && !sealed.contains("averyvery"),
                "{sealed}"
            );
            assert_eq!(
                unseal_id(&K, &sealed).as_deref(),
                Some(upstream),
                "{upstream}"
            );
        }
        // Enough samples that some payload carries a `_` of its own.
        for i in 0..64 {
            let upstream = format!("{i:08x}");
            assert_eq!(
                unseal_id(&K, &seal_id(&K, &upstream)).as_deref(),
                Some(upstream.as_str())
            );
        }
    }

    #[test]
    fn unseal_answers_none_for_anything_it_did_not_seal() {
        let sealed = seal_id(&K, "fc_bdrk_1");
        for foreign in [
            "call_abc",
            "fc_68af0c1e2b",
            "",
            "_",
            "msg_",
            // The same payload under another prefix.
            &sealed.replacen("fc_", "msg_", 1),
            // One flipped payload byte.
            &format!(
                "{}{}",
                &sealed[..sealed.len() - 1],
                if sealed.ends_with('A') { 'B' } else { 'A' }
            ),
        ] {
            assert_eq!(unseal_id(&K, foreign), None, "{foreign}");
        }
        assert_eq!(unseal_id(&[8u8; 32], &sealed), None, "another key");
    }

    #[test]
    fn the_seal_key_is_not_the_id_key() {
        assert_ne!(seal_key(&K), K);
        assert_eq!(seal_key(&K), seal_key(&K));
    }

    #[test]
    fn a_response_body_seals_every_item_id_and_drops_gateway_usage_extensions() {
        let mut body = json!({
            "id": "resp_up",
            "output": [
                {"type": "reasoning", "id": "rs_bdrk_1", "summary": []},
                {"type": "function_call", "id": "fc_bdrk_1", "call_id": "call_bdrk_1",
                 "name": "f", "arguments": "{}", "caller": {"type": "program", "caller_id": "call_prog_1"}},
                {"type": "mcp_call", "id": "mcp_bdrk_1", "approval_request_id": "mcpr_bdrk_1"},
            ],
            "usage": {"input_tokens": 9, "input_tokens_details": {"cached_tokens": 1, "cache_creation_tokens": 3}},
        });
        seal_responses_body(&K, &mut body);
        let text = body.to_string();
        assert!(!text.contains("bdrk") && !text.contains("prog_1"), "{text}");
        // The response-level id is not an item id.
        assert_eq!(body["id"], "resp_up");
        assert_eq!(body["output"][1]["call_id"], seal_id(&K, "call_bdrk_1"));
        assert_eq!(
            body["output"][1]["caller"]["caller_id"],
            seal_id(&K, "call_prog_1")
        );
        assert_eq!(
            body["output"][2]["approval_request_id"],
            seal_id(&K, "mcpr_bdrk_1")
        );
        // Every other field survives as it was.
        assert_eq!(body["output"][1]["name"], "f");
        assert_eq!(
            body["usage"]["input_tokens_details"],
            json!({"cached_tokens": 1})
        );
    }

    #[test]
    fn a_stream_event_seals_the_ids_a_body_would() {
        let mut added = json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "function_call", "id": "fc_bdrk_1", "call_id": "call_bdrk_1"}});
        let mut delta = json!({"type": "response.function_call_arguments.delta", "item_id": "fc_bdrk_1", "delta": "{"});
        let mut done = json!({"type": "response.completed", "response": {"id": "resp_up",
            "output": [{"type": "function_call", "id": "fc_bdrk_1", "call_id": "call_bdrk_1"}],
            "usage": {"input_tokens_details": {"cached_tokens": 0, "cache_creation_tokens": 2}}}});
        for event in [&mut added, &mut delta, &mut done] {
            seal_responses_event(&K, event);
        }
        let item_id = seal_id(&K, "fc_bdrk_1");
        assert_eq!(added["item"]["id"], item_id);
        assert_eq!(delta["item_id"], item_id);
        assert_eq!(done["response"]["output"][0]["id"], item_id);
        assert_eq!(
            done["response"]["output"][0]["call_id"],
            seal_id(&K, "call_bdrk_1")
        );
        assert_eq!(done["response"]["id"], "resp_up");
        assert_eq!(
            done["response"]["usage"]["input_tokens_details"],
            json!({"cached_tokens": 0})
        );
    }

    /// The byte splice and the parsed walk pick the same fields.
    #[test]
    fn the_event_path_predicate_selects_the_fields_the_value_walk_seals() {
        let event = br#"{"type":"response.completed","item_id":"fc_bdrk_1","item":{"id":"fc_bdrk_1","call_id":"call_bdrk_1","caller":{"caller_id":"call_p"}},"response":{"id":"resp_up","output":[{"id":"fc_bdrk_1","call_id":"call_bdrk_1","approval_request_id":"mcpr_1","name":"keep"}]}}"#;
        let mut seen = Vec::new();
        crate::json_splice::rewrite_string_values(event, responses_event_id_path, |v| {
            seen.push(v.to_owned());
            None
        })
        .unwrap();
        assert_eq!(
            seen,
            [
                "fc_bdrk_1",
                "fc_bdrk_1",
                "call_bdrk_1",
                "call_p",
                "fc_bdrk_1",
                "call_bdrk_1",
                "mcpr_1"
            ]
        );
    }

    #[test]
    fn inbound_input_items_get_the_upstreams_own_ids_back() {
        let mut body = json!({
            "model": "m",
            "previous_response_id": "resp_up",
            "input": [
                {"type": "function_call", "id": seal_id(&K, "fc_bdrk_1"),
                 "call_id": seal_id(&K, "call_bdrk_1"), "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": seal_id(&K, "call_bdrk_1"), "output": "ok"},
                {"type": "mcp_approval_response", "approval_request_id": seal_id(&K, "mcpr_1"), "approve": true},
                {"type": "function_call_output", "call_id": "call_client_made", "output": "x",
                 "caller": {"type": "program", "caller_id": seal_id(&K, "call_p")}},
                {"type": "message", "role": "user", "content": "hi", "id": "msg_client"},
            ],
        });
        unseal_responses_input(&K, &mut body);
        assert_eq!(body["input"][0]["id"], "fc_bdrk_1");
        assert_eq!(body["input"][0]["call_id"], "call_bdrk_1");
        assert_eq!(body["input"][1]["call_id"], "call_bdrk_1");
        assert_eq!(body["input"][2]["approval_request_id"], "mcpr_1");
        assert_eq!(body["input"][3]["call_id"], "call_client_made");
        assert_eq!(body["input"][3]["caller"]["caller_id"], "call_p");
        assert_eq!(body["input"][4]["id"], "msg_client");
        assert_eq!(body["previous_response_id"], "resp_up");

        // A string input has no items to restore.
        let mut plain = json!({"model": "m", "input": "hi"});
        unseal_responses_input(&K, &mut plain);
        assert_eq!(plain, json!({"model": "m", "input": "hi"}));
    }

    #[test]
    fn a_messages_body_gets_a_new_id_mapped_tool_use_ids_and_no_channel_metrics() {
        let ids = MessagesIds::new(&K);
        let mut body = json!({
            "id": "msg_bdrk_01X",
            "type": "message",
            "content": [
                {"type": "text", "text": "calling"},
                {"type": "tool_use", "id": "toolu_bdrk_01Y", "name": "f", "input": {}},
                {"type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search", "input": {}},
            ],
            "usage": {"input_tokens": 3, "output_tokens": 1},
            "amazon-bedrock-invocationMetrics": {"inputTokenCount": 3},
        });
        ids.apply_to_body(&mut body);
        assert!(
            is_platform_id(Surface::Messages, body["id"].as_str().unwrap()),
            "{body}"
        );
        assert_eq!(
            body["content"][1]["id"],
            map_tool_call_id(&K, Surface::Messages, "toolu_bdrk_01Y")
        );
        // Only a tool_use block's id is a tool-call id the client echoes.
        assert_eq!(body["content"][2]["id"], "srvtoolu_01");
        assert!(body.get("amazon-bedrock-invocationMetrics").is_none());
        assert_eq!(
            body["usage"],
            json!({"input_tokens": 3, "output_tokens": 1})
        );
    }

    #[test]
    fn a_messages_stream_shares_one_id_and_maps_tool_use_blocks() {
        let ids = MessagesIds::new(&K);
        let start = br#"{"type":"message_start","message":{"id":"msg_bdrk_01X","model":"up","usage":{"input_tokens":1e2}}}"#;
        let out = String::from_utf8(ids.rewrite_event_json(start, "alias").unwrap()).unwrap();
        assert_eq!(
            out,
            format!(
                r#"{{"type":"message_start","message":{{"id":"{}","model":"alias","usage":{{"input_tokens":1e2}}}}}}"#,
                ids.message_id
            )
        );

        let block = br#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_bdrk_01Y","name":"f","input":{}}}"#;
        let out = String::from_utf8(ids.rewrite_event_json(block, "alias").unwrap()).unwrap();
        let mapped = map_tool_call_id(&K, Surface::Messages, "toolu_bdrk_01Y");
        assert!(out.contains(&format!(r#""id":"{mapped}""#)), "{out}");
        // The id ahead of the type is still read against it.
        let id_first = br#"{"type":"content_block_start","index":1,"content_block":{"id":"toolu_bdrk_01Y","type":"tool_use"}}"#;
        assert!(
            String::from_utf8(ids.rewrite_event_json(id_first, "alias").unwrap())
                .unwrap()
                .contains(&mapped)
        );

        // Another block type's id is not a tool-call id, so the frame is left alone.
        let server = br#"{"type":"content_block_start","index":2,"content_block":{"id":"srvtoolu_01","type":"server_tool_use"}}"#;
        assert_eq!(ids.rewrite_event_json(server, "alias"), None);

        let stop = br#"{"type":"message_stop","amazon-bedrock-invocationMetrics":{"inputTokenCount":3,"outputTokenCount":1}}"#;
        assert_eq!(
            ids.rewrite_event_json(stop, "alias").as_deref(),
            Some(&br#"{"type":"message_stop"}"#[..])
        );
        let delta = br#"{"type":"content_block_delta","index":0,"delta":{"text":"hi"}}"#;
        assert_eq!(ids.rewrite_event_json(delta, "alias"), None);
    }

    #[test]
    fn a_messages_encoder_event_gets_the_same_treatment() {
        let ids = MessagesIds::new(&K);
        let mut start =
            json!({"type": "message_start", "message": {"id": "msg_gw", "content": []}});
        let mut block = json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "call_up_1", "name": "f", "input": {}}});
        ids.apply_to_event(&mut start);
        ids.apply_to_event(&mut block);
        assert_eq!(start["message"]["id"], ids.message_id.as_str());
        assert_eq!(
            block["content_block"]["id"],
            map_tool_call_id(&K, Surface::Messages, "call_up_1")
        );
    }

    #[test]
    fn openai_shape_tool_calls_map_in_place() {
        let mut calls = vec![
            json!({"index": 0, "id": "toolu_bdrk_01Y", "type": "function", "function": {"name": "f", "arguments": ""}}),
            json!({"index": 0, "function": {"arguments": "{}"}}),
        ];
        map_tool_calls(&K, Surface::Chat, &mut calls);
        assert_eq!(
            calls[0]["id"],
            map_tool_call_id(&K, Surface::Chat, "toolu_bdrk_01Y")
        );
        assert!(
            calls[1].get("id").is_none(),
            "a continuation delta carries no id"
        );
        assert_eq!(calls[1]["function"]["arguments"], "{}");
    }
}
