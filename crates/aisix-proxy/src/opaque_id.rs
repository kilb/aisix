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

/// A 32-byte key derived from `key` under `label`: `HMAC-SHA256(key, label)`.
pub(crate) fn derive_key(key: &[u8], label: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(hmac_parts(key, &[label]).as_ref());
    out
}

/// The key [`seal_id`] takes, derived from the platform `id_key` under its
/// own label so the seal never shares a key with [`map_tool_call_id`].
pub(crate) fn seal_key(id_key: &[u8; 32]) -> [u8; 32] {
    derive_key(id_key, b"seal-v1")
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

/// The size a sealed plaintext is padded to: 64 bytes, or the next multiple
/// of 64 for a longer id. Every id up to 63 bytes — every upstream id seen in
/// practice — therefore seals to one length, so a sealed id's length cannot
/// tell one upstream's id format from another's.
pub(crate) const SEAL_BUCKET: usize = 64;

/// The shortest id [`Sealer::seal`] produces: no kept prefix, and the
/// base64url form of a nonce, one bucket and a tag. Anything shorter cannot
/// be one, so it is passed over before any decoding or key setup.
const MIN_SEALED_LEN: usize = ((aead::NONCE_LEN + SEAL_BUCKET + aead::MAX_TAG_LEN) * 4).div_ceil(3);

/// `id`, a `0x80` marker, then zeros up to the bucket (ISO/IEC 7816-4
/// padding): unambiguous for any id, of any length, at one byte's cost.
pub(crate) fn pad(id: &str) -> Vec<u8> {
    pad_after("", id)
}

/// [`pad`] for an id behind a fixed `head`: `head || id`, padded to a
/// multiple of [`SEAL_BUCKET`] that leaves the id a whole bucket of its own,
/// so every id up to 63 bytes behind the same head pads to one length. With
/// no head this is exactly [`pad`]'s bucket.
pub(crate) fn pad_after(head: &str, id: &str) -> Vec<u8> {
    let padded_len = (head.len() + (id.len() + 1).div_ceil(SEAL_BUCKET) * SEAL_BUCKET)
        .div_ceil(SEAL_BUCKET)
        * SEAL_BUCKET;
    let mut out = Vec::with_capacity(padded_len + aead::MAX_TAG_LEN);
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(id.as_bytes());
    out.push(0x80);
    out.resize(padded_len, 0);
    out
}

/// The bytes [`pad`] or [`pad_after`] padded; `None` when they do not end in
/// the marker and zeros.
pub(crate) fn unpad(padded: &[u8]) -> Option<&[u8]> {
    let marker = padded.iter().rposition(|&b| b != 0)?;
    (padded[marker] == 0x80).then(|| &padded[..marker])
}

/// Deterministic authenticated encryption of ids under one seal key: the
/// nonce is synthetic, `HMAC-SHA256(key, "seal" || 0x00 || id)[..12]`, so the
/// same id always seals to the same value — a stream's `item_id` on every
/// delta matches the item it belongs to, and a replayed answer matches the
/// first. The AES-256-GCM key is derived from the seal key under its own
/// label, so the nonce function and the cipher never share key material. The
/// plaintext is padded to [`SEAL_BUCKET`] first.
///
/// Building one sets up both keys, so a request builds one and seals or
/// unseals every id it handles with it.
#[derive(Clone)]
pub(crate) struct Sealer {
    nonce_key: hmac::Key,
    cipher: aead::LessSafeKey,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(***)")
    }
}

impl Sealer {
    /// The sealer of platform mode: its keys come from [`seal_key`].
    pub(crate) fn of(platform: &crate::platform::PlatformSettings) -> Self {
        Self::new(&seal_key(&platform.id_key))
    }

    pub(crate) fn new(key: &[u8; 32]) -> Self {
        #[cfg(test)]
        tests::SEALERS_BUILT.with(|n| n.set(n.get() + 1));
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

    /// `<kept prefix>` + base64url(nonce || ciphertext || tag), the ciphertext
    /// of the padded id, with the kept prefix as associated data so it cannot
    /// be swapped for another.
    pub(crate) fn seal(&self, id: &str) -> String {
        let prefix = kept_prefix(id);
        let nonce = self.nonce(id);
        let mut sealed = pad(id);
        let mut payload = Vec::with_capacity(aead::NONCE_LEN + sealed.len() + aead::MAX_TAG_LEN);
        payload.extend_from_slice(&nonce);
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

    /// The id [`Self::seal`] was given; `None` for anything this sealer did
    /// not seal.
    pub(crate) fn unseal(&self, id: &str) -> Option<String> {
        if id.len() < MIN_SEALED_LEN {
            return None;
        }
        // The payload's alphabet includes `_`, so an id sealed without a
        // prefix is also tried whole.
        id.find('_')
            .and_then(|end| self.open(&id[..=end], &id[end + 1..]))
            .or_else(|| self.open("", id))
    }

    fn open(&self, prefix: &str, payload: &str) -> Option<String> {
        let raw = b64().decode(payload).ok()?;
        if raw.len() < aead::NONCE_LEN + SEAL_BUCKET + aead::MAX_TAG_LEN {
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
        let plain = String::from_utf8(unpad(plain)?.to_vec()).ok()?;
        (kept_prefix(&plain) == prefix && self.nonce(&plain) == nonce).then_some(plain)
    }
}

/// Seal an upstream id reversibly. The id's leading `<prefix>_` stays in the
/// clear (see [`kept_prefix`]) so the sealed id still passes an upstream's
/// prefix check; the rest names nothing, not even the id's length.
///
/// The one-off form of [`Sealer::seal`], for a caller with a single id; one
/// that handles several builds one [`Sealer`] and reuses it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn seal_id(key: &[u8; 32], upstream_id: &str) -> String {
    Sealer::new(key).seal(upstream_id)
}

/// The upstream id a [`seal_id`] value was made from; `None` for anything
/// this key did not seal, which the caller passes through untouched. The
/// one-off form of [`Sealer::unseal`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn unseal_id(key: &[u8; 32], id: &str) -> Option<String> {
    Sealer::new(key).unseal(id)
}

/// The fields of a Responses output item that name an item or a call — the
/// values a client echoes back in a later request's `input[]`. A `caller`
/// object's `caller_id` names a call too.
const RESPONSES_ITEM_ID_FIELDS: [&str; 3] = ["id", "call_id", "approval_request_id"];

/// Run `visit` over every id field of one item.
fn for_each_item_id(item: &mut Value, mut visit: impl FnMut(&mut String)) {
    let Value::Object(map) = item else {
        return;
    };
    for (field, value) in map.iter_mut() {
        match value {
            Value::String(s) if RESPONSES_ITEM_ID_FIELDS.contains(&field.as_str()) => visit(s),
            Value::Object(caller) if field == "caller" => {
                if let Some(Value::String(s)) = caller.get_mut("caller_id") {
                    visit(s);
                }
            }
            _ => {}
        }
    }
}

/// Seal every item id of a Responses `Response` object. The response-level
/// `id` is not an item id and is left alone, and so is every other field.
pub(crate) fn seal_responses_body(sealer: &Sealer, body: &mut Value) {
    if let Some(items) = body.get_mut("output").and_then(Value::as_array_mut) {
        for item in items {
            seal_item(sealer, item);
        }
    }
}

fn seal_item(sealer: &Sealer, item: &mut Value) {
    for_each_item_id(item, |slot| {
        if !slot.is_empty() {
            *slot = sealer.seal(slot);
        }
    });
}

/// [`seal_responses_body`] for one parsed Responses stream event: the item of
/// `response.output_item.*`, the `item_id` of every per-item event, and the
/// Response object of the snapshot events.
pub(crate) fn seal_responses_event(sealer: &Sealer, event: &mut Value) {
    if let Some(item) = event.get_mut("item") {
        seal_item(sealer, item);
    }
    if let Some(Value::String(id)) = event.get_mut("item_id") {
        if !id.is_empty() {
            *id = sealer.seal(id);
        }
    }
    if let Some(response) = event.get_mut("response") {
        seal_responses_body(sealer, response);
    }
}

/// Drop the usage counter the Responses bridge writes beside OpenAI's own:
/// an Anthropic-shape cache write, which would name the upstream's vendor and
/// is already counted in `input_tokens`. Only for a Response object the
/// gateway built itself; a relayed one keeps every field its upstream wrote.
pub(crate) fn drop_bridged_usage_extensions(response: &mut Value) {
    if let Some(details) = response
        .pointer_mut("/usage/input_tokens_details")
        .and_then(Value::as_object_mut)
    {
        details.remove("cache_creation_tokens");
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
///
/// The request body has no size cap of its own, so this stays one walk: a
/// value too short to be a seal is passed over unread, and the one
/// [`Sealer`] the request needs is built only when a value could be one.
pub(crate) fn unseal_responses_input(
    platform: &crate::platform::PlatformSettings,
    body: &mut Value,
) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut sealer: Option<Sealer> = None;
    for item in items {
        for_each_item_id(item, |slot| {
            if slot.len() < MIN_SEALED_LEN {
                return;
            }
            let sealer = sealer.get_or_insert_with(|| Sealer::of(platform));
            if let Some(original) = sealer.unseal(slot) {
                *slot = original;
            }
        });
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
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    const K: [u8; 32] = [7u8; 32];

    thread_local! {
        /// How many [`Sealer`]s this test's thread has built.
        pub(crate) static SEALERS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn sealers_built() -> usize {
        SEALERS_BUILT.with(std::cell::Cell::get)
    }

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

    /// The length of a sealed id says nothing about the id it was made
    /// from: one upstream's short ids and another's long ones look alike.
    #[test]
    fn sealed_ids_are_one_length_whatever_the_upstream_id_was() {
        let short = "call_1";
        let long = format!("call_{}", "b".repeat(57));
        assert_eq!(long.len(), 62);
        assert_eq!(seal_id(&K, short).len(), seal_id(&K, &long).len());
        // The payload behind any kept prefix is one length too.
        let payload = |upstream: &str| {
            let sealed = seal_id(&K, upstream);
            sealed.len() - kept_prefix(upstream).len()
        };
        for upstream in [
            "",
            "x",
            "toolu_bdrk_01Y",
            "fc_68af0c1e2b5f8190b2c1d3e4f5a6b7c8d9e0f1a2b3c4d5e6f",
        ] {
            assert_eq!(payload(upstream), payload("call_1"), "{upstream}");
        }
        // Past the first bucket the next one is shared the same way.
        assert_eq!(
            seal_id(&K, &"a".repeat(64)).len(),
            seal_id(&K, &"a".repeat(127)).len()
        );
        assert!(seal_id(&K, &"a".repeat(64)).len() > seal_id(&K, &"a".repeat(63)).len());
    }

    #[test]
    fn every_length_round_trips_exactly() {
        for len in [0usize, 1, 63, 64, 65, 200] {
            for upstream in [
                "a".repeat(len),
                format!("fc_{}", "é".repeat(len.saturating_sub(3) / 2)),
                // Trailing zeros and a trailing marker byte value belong to the id.
                format!("{}\u{0}", "z".repeat(len.saturating_sub(1))),
            ] {
                assert_eq!(
                    unseal_id(&K, &seal_id(&K, &upstream)).as_deref(),
                    Some(upstream.as_str()),
                    "length {}",
                    upstream.len()
                );
            }
        }
    }

    #[test]
    fn a_truncated_or_tampered_seal_opens_to_nothing() {
        let sealed = seal_id(&K, "fc_bdrk_01Z");
        let payload = &sealed["fc_".len()..];
        let mut tampered = payload.as_bytes().to_vec();
        let middle = tampered.len() / 2;
        tampered[middle] = if tampered[middle] == b'A' { b'B' } else { b'A' };
        for broken in [
            sealed[..sealed.len() - 1].to_string(),
            sealed[..sealed.len() - 4].to_string(),
            sealed[..sealed.len() / 2].to_string(),
            format!("fc_{}", String::from_utf8(tampered).unwrap()),
            format!("{sealed}A"),
        ] {
            assert_eq!(unseal_id(&K, &broken), None, "{broken}");
        }
    }

    #[test]
    fn padding_is_read_back_only_as_it_was_written() {
        assert_eq!(unpad(&pad("ab")), Some(&b"ab"[..]));
        assert_eq!(pad("").len(), SEAL_BUCKET);
        assert_eq!(pad(&"a".repeat(63)).len(), SEAL_BUCKET);
        assert_eq!(pad(&"a".repeat(64)).len(), 2 * SEAL_BUCKET);
        // Behind a head, the id still gets a whole bucket of its own.
        let head = "h".repeat(83);
        assert_eq!(pad_after(&head, "").len(), 3 * SEAL_BUCKET);
        assert_eq!(pad_after(&head, &"a".repeat(63)).len(), 3 * SEAL_BUCKET);
        assert_eq!(pad_after(&head, &"a".repeat(64)).len(), 4 * SEAL_BUCKET);
        assert_eq!(
            unpad(&pad_after(&head, "ab")),
            Some(format!("{head}ab").as_bytes())
        );
        // No marker, or something after it, is not padding.
        assert_eq!(unpad(&[b'a', 0, 0]), None);
        assert_eq!(unpad(&[b'a', 0x80, 1, 0]), None);
        assert_eq!(unpad(&[0, 0]), None);
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
    fn a_response_body_seals_every_item_id_and_keeps_every_other_field() {
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
        seal_responses_body(&Sealer::new(&K), &mut body);
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
        // Every other field survives as it was, the upstream's own usage
        // counters included.
        assert_eq!(body["output"][1]["name"], "f");
        assert_eq!(
            body["usage"]["input_tokens_details"],
            json!({"cached_tokens": 1, "cache_creation_tokens": 3})
        );
    }

    #[test]
    fn a_bridged_answer_drops_only_the_usage_counter_the_bridge_added() {
        let mut body = json!({"id": "resp_gw", "usage": {"input_tokens": 9,
            "input_tokens_details": {"cached_tokens": 1, "cache_write_tokens": 2, "cache_creation_tokens": 3}}});
        drop_bridged_usage_extensions(&mut body);
        assert_eq!(
            body,
            json!({"id": "resp_gw", "usage": {"input_tokens": 9,
                "input_tokens_details": {"cached_tokens": 1, "cache_write_tokens": 2}}})
        );
        let mut no_usage = json!({"id": "resp_gw"});
        drop_bridged_usage_extensions(&mut no_usage);
        assert_eq!(no_usage, json!({"id": "resp_gw"}));
    }

    #[test]
    fn a_stream_event_seals_the_ids_a_body_would() {
        let mut added = json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "function_call", "id": "fc_bdrk_1", "call_id": "call_bdrk_1"}});
        let mut delta = json!({"type": "response.function_call_arguments.delta", "item_id": "fc_bdrk_1", "delta": "{"});
        let mut done = json!({"type": "response.completed", "response": {"id": "resp_up",
            "output": [{"type": "function_call", "id": "fc_bdrk_1", "call_id": "call_bdrk_1"}],
            "usage": {"input_tokens_details": {"cached_tokens": 0, "cache_creation_tokens": 2}}}});
        let sealer = Sealer::new(&K);
        for event in [&mut added, &mut delta, &mut done] {
            seal_responses_event(&sealer, event);
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
            json!({"cached_tokens": 0, "cache_creation_tokens": 2})
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
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let k = seal_key(&platform.id_key);
        let seal_id = |id: &str| seal_id(&k, id);
        let mut body = json!({
            "model": "m",
            "previous_response_id": "resp_up",
            "input": [
                {"type": "function_call", "id": seal_id("fc_bdrk_1"),
                 "call_id": seal_id("call_bdrk_1"), "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": seal_id("call_bdrk_1"), "output": "ok"},
                {"type": "mcp_approval_response", "approval_request_id": seal_id("mcpr_1"), "approve": true},
                {"type": "function_call_output", "call_id": "call_client_made", "output": "x",
                 "caller": {"type": "program", "caller_id": seal_id("call_p")}},
                {"type": "message", "role": "user", "content": "hi", "id": "msg_client"},
            ],
        });
        let before = sealers_built();
        unseal_responses_input(&platform, &mut body);
        assert_eq!(
            sealers_built() - before,
            1,
            "one sealer for the whole request"
        );
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
        unseal_responses_input(&platform, &mut plain);
        assert_eq!(plain, json!({"model": "m", "input": "hi"}));
    }

    /// A body of many items whose ids are too short to be seals is one walk:
    /// no key is set up and no value is decoded.
    #[test]
    fn inbound_ids_too_short_to_be_seals_cost_no_key_setup() {
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let items: Vec<Value> = (0..10_000)
            .map(|i| json!({"type": "function_call_output", "call_id": format!("c{i}"), "id": "x", "output": ""}))
            .collect();
        let mut body = json!({"model": "m", "input": items});
        let expected = body.clone();
        let before = sealers_built();
        unseal_responses_input(&platform, &mut body);
        assert_eq!(
            sealers_built(),
            before,
            "no sealer for a body with no candidate"
        );
        assert_eq!(body, expected);

        // The shortest seal there is clears the bar; one shorter cannot be one.
        let shortest = seal_id(&K, "");
        assert_eq!(shortest.len(), MIN_SEALED_LEN);
        let sealer = Sealer::new(&K);
        assert_eq!(sealer.unseal(&shortest).as_deref(), Some(""));
        assert_eq!(sealer.unseal(&shortest[1..]), None);
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
