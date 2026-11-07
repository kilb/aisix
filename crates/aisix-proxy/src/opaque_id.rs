//! Platform mode: Responses ids that name no upstream channel and belong to
//! one tenant.
//!
//! A Responses output item's ids are sealed reversibly ([`Sealer`]) and
//! unsealed on their way back in ([`unseal_responses_input`]): that protocol
//! cannot take a substitute id, see there. An item id keeps its upstream's
//! prefix, and a call id is shown behind a fixed `call_` ([`IdKind`]). The
//! seal is what keeps one tenant's ids from opening for another; every id is
//! restored before the upstream sees the request, so it gets back exactly
//! what it issued.
//!
//! Chat Completions and Messages ids pass through as the upstream issued
//! them, in platform mode as without it: an upstream that signs its reasoning
//! must get back exactly the ids and blocks it issued.
//!
//! The usage event keeps the upstream's own id as `provider_request_id`;
//! every rewrite here runs after telemetry has read the response.

use base64::Engine as _;
use ring::{aead, hmac};
use serde_json::Value;

use crate::json_splice::{PathSeg, Retain};

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
/// own label so the seal never uses the id key itself.
pub(crate) fn seal_key(id_key: &[u8; 32]) -> [u8; 32] {
    derive_key(id_key, b"seal-v1")
}

/// The longest leading `<letters and digits>_` a sealed id keeps in the clear.
const MAX_KEPT_PREFIX: usize = 16;

/// The prefix every sealed call id is shown behind.
const CALL_PREFIX: &str = "call_";

/// What a Responses id names, which decides the prefix its seal is shown
/// behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdKind {
    /// An item's `id`, and an `approval_request_id`, which is the `id` of
    /// the approval request item it answers. The seal keeps the upstream's
    /// own prefix, because the upstream checks an item id's prefix.
    Item,
    /// A field that names a call: `call_id`, and a `caller`'s `caller_id`,
    /// which is "the call ID of the program item that produced this tool
    /// call" (openai-python `types/responses/response_function_tool_call.py`).
    /// The seal is shown behind a fixed `call_` whatever the upstream wrote:
    /// a translated answer's call id is the tool-call id of the Chat
    /// Completions or Messages upstream that served it, and that id's prefix
    /// (`tooluse_`, `toolu_`, `call_`) names the channel.
    Call,
}

impl IdKind {
    /// The byte that separates the two kinds in a seal's nonce and its
    /// associated data, so a seal of one kind never opens as the other.
    fn label(self) -> u8 {
        match self {
            IdKind::Item => b'i',
            IdKind::Call => b'c',
        }
    }

    /// The prefix a seal of `id` is shown behind.
    fn shown_prefix(self, id: &str) -> &str {
        match self {
            IdKind::Item => kept_prefix(id),
            IdKind::Call => CALL_PREFIX,
        }
    }
}

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
    pad_to(id, SEAL_BUCKET)
}

/// [`pad`] to a multiple of `bucket` rather than of [`SEAL_BUCKET`].
pub(crate) fn pad_to(data: &str, bucket: usize) -> Vec<u8> {
    let padded_len = (data.len() + 1).div_ceil(bucket) * bucket;
    let mut out = Vec::with_capacity(padded_len + aead::MAX_TAG_LEN);
    out.extend_from_slice(data.as_bytes());
    out.push(0x80);
    out.resize(padded_len, 0);
    out
}

/// The bytes [`pad`] or [`pad_to`] padded; `None` when they do not end in
/// the marker and zeros.
pub(crate) fn unpad(padded: &[u8]) -> Option<&[u8]> {
    let marker = padded.iter().rposition(|&b| b != 0)?;
    (padded[marker] == 0x80).then(|| &padded[..marker])
}

/// Deterministic authenticated encryption of ids under one seal key, bound
/// to one tenant: the nonce is synthetic,
/// `HMAC-SHA256(key, "seal" || 0x00 || owner || kind || id)[..12]`, so the
/// same id always seals to the same value for one tenant — a stream's
/// `item_id` on every delta matches the item it belongs to, and a replayed
/// answer matches the first. The kind ([`IdKind`]) and the prefix the seal is
/// shown behind are its associated data, so no seal opens as another kind or
/// behind another prefix. The AES-256-GCM key is derived from the seal key under its own
/// label, so the nonce function and the cipher never share key material. The
/// plaintext is padded to [`SEAL_BUCKET`] first.
///
/// The owner is the tenant the ids are handed to
/// ([`crate::platform::tenant_owner`]), written length-prefixed into both the
/// nonce and the associated data. The upstream account is shared by every
/// tenant, so an id only its own tenant can unseal is what keeps one tenant
/// from echoing another's id and having the upstream resolve what it stored
/// for that other tenant: a seal made for anyone else does not open, and
/// passes through as the opaque string it is.
///
/// The keys are set up once, in [`crate::platform::PlatformSettings`]
/// ([`SealKeys`]); a request builds one sealer for its owner and seals or
/// unseals every id it handles with it.
pub(crate) struct Sealer {
    keys: std::sync::Arc<SealKeys>,
    /// The owner, length-prefixed: the bytes the nonce and the associated
    /// data carry, so no owner's bytes are another owner's followed by more.
    owner: Vec<u8>,
    /// The last id sealed, as `(kind, id, sealed)`: a stream names the same
    /// item on every delta, and this seals it once.
    last: std::sync::Mutex<Option<(IdKind, String, String)>>,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(***)")
    }
}

/// The two keys [`Sealer`] seals under, set up once from the seal key
/// ([`seal_key`]): the nonce function's HMAC key, and the AES-256-GCM cipher
/// under a key derived from it under its own label.
pub(crate) struct SealKeys {
    nonce_key: hmac::Key,
    cipher: aead::LessSafeKey,
}

impl SealKeys {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        #[cfg(test)]
        tests::SEAL_KEYS_BUILT.with(|n| n.set(n.get() + 1));
        let cipher = aead::UnboundKey::new(&aead::AES_256_GCM, &derive_key(key, b"seal-aead"))
            .expect("a 32-byte key is the length AES-256-GCM takes");
        Self {
            nonce_key: hmac::Key::new(hmac::HMAC_SHA256, key),
            cipher: aead::LessSafeKey::new(cipher),
        }
    }
}

impl Sealer {
    /// The sealer of platform mode for `owner`'s ids, under the keys the
    /// settings set up once.
    pub(crate) fn of(platform: &crate::platform::PlatformSettings, owner: &str) -> Self {
        Self::with_keys(platform.seal_keys().clone(), owner)
    }

    /// A sealer under keys set up from `key` for this one sealer.
    #[cfg(test)]
    pub(crate) fn new(key: &[u8; 32], owner: &str) -> Self {
        Self::with_keys(std::sync::Arc::new(SealKeys::new(key)), owner)
    }

    fn with_keys(keys: std::sync::Arc<SealKeys>, owner: &str) -> Self {
        #[cfg(test)]
        tests::SEALERS_BUILT.with(|n| n.set(n.get() + 1));
        let mut owner_bytes = Vec::with_capacity(8 + owner.len());
        owner_bytes.extend_from_slice(&(owner.len() as u64).to_be_bytes());
        owner_bytes.extend_from_slice(owner.as_bytes());
        Self {
            keys,
            owner: owner_bytes,
            last: std::sync::Mutex::new(None),
        }
    }

    /// The associated data of a seal: the owner, the kind, then the prefix
    /// the seal is shown behind, so none of them can be swapped for another.
    fn aad(&self, kind: IdKind, prefix: &str) -> Vec<u8> {
        [self.owner.as_slice(), &[kind.label()], prefix.as_bytes()].concat()
    }

    fn nonce(&self, kind: IdKind, id: &str) -> [u8; aead::NONCE_LEN] {
        let mut ctx = hmac::Context::with_key(&self.keys.nonce_key);
        ctx.update(b"seal");
        ctx.update(&[0]);
        ctx.update(&self.owner);
        ctx.update(&[kind.label()]);
        ctx.update(id.as_bytes());
        let mut nonce = [0u8; aead::NONCE_LEN];
        nonce.copy_from_slice(&ctx.sign().as_ref()[..aead::NONCE_LEN]);
        nonce
    }

    /// `<shown prefix>` + base64url(nonce || ciphertext || tag), the
    /// ciphertext of the padded id, with the owner, the kind and the shown
    /// prefix as associated data.
    pub(crate) fn seal(&self, kind: IdKind, id: &str) -> String {
        // A poisoned memo is only a lost shortcut.
        if let Ok(last) = self.last.lock() {
            if let Some((k, plain, sealed)) = last.as_ref() {
                if *k == kind && plain == id {
                    return sealed.clone();
                }
            }
        }
        let sealed = self.seal_fresh(kind, id);
        if let Ok(mut last) = self.last.lock() {
            *last = Some((kind, id.to_string(), sealed.clone()));
        }
        sealed
    }

    fn seal_fresh(&self, kind: IdKind, id: &str) -> String {
        #[cfg(test)]
        tests::SEALS_MADE.with(|n| n.set(n.get() + 1));
        let prefix = kind.shown_prefix(id);
        let nonce = self.nonce(kind, id);
        let mut sealed = pad(id);
        let mut payload = Vec::with_capacity(aead::NONCE_LEN + sealed.len() + aead::MAX_TAG_LEN);
        payload.extend_from_slice(&nonce);
        self.keys
            .cipher
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(self.aad(kind, prefix)),
                &mut sealed,
            )
            .expect("an id is far below the AES-GCM message limit");
        payload.extend_from_slice(&sealed);
        format!("{prefix}{}", b64().encode(payload))
    }

    /// The id [`Self::seal`] was given for `kind`; `None` for anything this
    /// sealer did not seal as that kind, another tenant's seal included.
    pub(crate) fn unseal(&self, kind: IdKind, id: &str) -> Option<String> {
        if id.len() < MIN_SEALED_LEN {
            return None;
        }
        match kind {
            // The payload's alphabet includes `_`, so an id sealed without a
            // prefix is also tried whole.
            IdKind::Item => id
                .find('_')
                .and_then(|end| self.open(kind, &id[..=end], &id[end + 1..]))
                .or_else(|| self.open(kind, "", id)),
            IdKind::Call => self.open(kind, CALL_PREFIX, id.strip_prefix(CALL_PREFIX)?),
        }
    }

    fn open(&self, kind: IdKind, prefix: &str, payload: &str) -> Option<String> {
        let raw = b64().decode(payload).ok()?;
        if raw.len() < aead::NONCE_LEN + SEAL_BUCKET + aead::MAX_TAG_LEN {
            return None;
        }
        let (nonce, sealed) = raw.split_at(aead::NONCE_LEN);
        let mut buf = sealed.to_vec();
        let plain = self
            .keys
            .cipher
            .open_in_place(
                aead::Nonce::try_assume_unique_for_key(nonce).ok()?,
                aead::Aad::from(self.aad(kind, prefix)),
                &mut buf,
            )
            .ok()?;
        let plain = String::from_utf8(unpad(plain)?.to_vec()).ok()?;
        (kind.shown_prefix(&plain) == prefix && self.nonce(kind, &plain) == nonce).then_some(plain)
    }
}

/// Seal an upstream item id reversibly. The id's leading `<prefix>_` stays
/// in the clear (see [`kept_prefix`]) so the sealed id still passes an
/// upstream's prefix check; the rest names nothing, not even the id's length.
///
/// The one-off form of [`Sealer::seal`], for a caller with a single id; one
/// that handles several builds one [`Sealer`] and reuses it.
#[cfg(test)]
pub(crate) fn seal_id(key: &[u8; 32], owner: &str, upstream_id: &str) -> String {
    Sealer::new(key, owner).seal(IdKind::Item, upstream_id)
}

/// The upstream id a [`seal_id`] value was made from; `None` for anything
/// this key did not seal to `owner` as an item, which the caller passes
/// through untouched. The one-off form of [`Sealer::unseal`].
#[cfg(test)]
pub(crate) fn unseal_id(key: &[u8; 32], owner: &str, id: &str) -> Option<String> {
    Sealer::new(key, owner).unseal(IdKind::Item, id)
}

/// [`seal_id`] for an id that names a call: shown behind `call_`.
#[cfg(test)]
pub(crate) fn seal_call(key: &[u8; 32], owner: &str, upstream_id: &str) -> String {
    Sealer::new(key, owner).seal(IdKind::Call, upstream_id)
}

/// [`unseal_id`] for an id that names a call.
#[cfg(test)]
pub(crate) fn unseal_call(key: &[u8; 32], owner: &str, id: &str) -> Option<String> {
    Sealer::new(key, owner).unseal(IdKind::Call, id)
}

/// The kind of id a field of a Responses item holds, for the fields that
/// name an item or a call — the values a client echoes back in a later
/// request's `input[]`.
fn item_field_kind(field: &str) -> Option<IdKind> {
    match field {
        "id" | "approval_request_id" => Some(IdKind::Item),
        "call_id" => Some(IdKind::Call),
        _ => None,
    }
}

/// Run `visit` over every id field of one item, with the kind it holds. A
/// `caller` object's `caller_id` names a call, and so does the `id` of a
/// `local_shell_call_output`: that item has no `call_id`, and its `id` is the
/// call id of the `local_shell_call` it answers (openai-python 3.41.0
/// `types/responses/response_input_item_param.py`, `LocalShellCallOutput`).
fn for_each_item_id(item: &mut Value, mut visit: impl FnMut(IdKind, &mut String)) {
    let names_call_by_id =
        item.get("type").and_then(Value::as_str) == Some("local_shell_call_output");
    let Value::Object(map) = item else {
        return;
    };
    for (field, value) in map.iter_mut() {
        match value {
            Value::String(s) => {
                let kind = match field.as_str() {
                    "id" if names_call_by_id => Some(IdKind::Call),
                    field => item_field_kind(field),
                };
                if let Some(kind) = kind {
                    visit(kind, s);
                }
            }
            Value::Object(caller) if field == "caller" => {
                if let Some(Value::String(s)) = caller.get_mut("caller_id") {
                    visit(IdKind::Call, s);
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
    for_each_item_id(item, |kind, slot| {
        if !slot.is_empty() {
            *slot = sealer.seal(kind, slot);
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
            *id = sealer.seal(IdKind::Item, id);
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

/// The paths [`seal_responses_event`] seals, with the kind of id each holds,
/// for a byte splice over a Responses stream event.
pub(crate) fn responses_event_id_kind(path: &[PathSeg]) -> Option<IdKind> {
    fn item_field(rest: &[PathSeg]) -> Option<IdKind> {
        match rest {
            [PathSeg::Key(field)] => item_field_kind(field),
            [PathSeg::Key(caller), PathSeg::Key(field)]
                if caller == "caller" && field == "caller_id" =>
            {
                Some(IdKind::Call)
            }
            _ => None,
        }
    }
    match path {
        [PathSeg::Key(key)] if key == "item_id" => Some(IdKind::Item),
        [PathSeg::Key(item), rest @ ..] if item == "item" => item_field(rest),
        [PathSeg::Key(response), PathSeg::Key(output), PathSeg::Index(_), rest @ ..]
            if response == "response" && output == "output" =>
        {
            item_field(rest)
        }
        _ => None,
    }
}

/// Restore the upstream's own ids in a `/v1/responses` request's `input[]`
/// before anything reads it, on the native and the translated path alike;
/// an id this key did not seal to `owner` — one the client made, or one
/// sealed to another tenant — passes untouched.
///
/// Answers the index of the first `item_reference` whose `id` did not
/// unseal, for the handler to refuse ([`is_item_reference`]): an item
/// reference has the upstream resolve an item it stored, so one that names
/// anything but this tenant's own seal names an item of the shared account
/// the tenant was never handed.
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
    owner: &str,
    body: &mut Value,
) -> Option<usize> {
    let items = body.get_mut("input").and_then(Value::as_array_mut)?;
    let mut sealer: Option<Sealer> = None;
    let mut unopened_reference = None;
    for (index, item) in items.iter_mut().enumerate() {
        // A reference opens by its own `id` only: another id field beside it
        // that opens (a `call_id`, a `caller`) is no warrant for it.
        let reference_id = is_item_reference(item).then(|| item.get("id").cloned());
        for_each_item_id(item, |kind, slot| {
            if slot.len() < MIN_SEALED_LEN {
                return;
            }
            let sealer = sealer.get_or_insert_with(|| Sealer::of(platform, owner));
            if let Some(original) = sealer.unseal(kind, slot) {
                *slot = original;
            }
        });
        if let Some(before) = reference_id {
            let opened = matches!(
                (before, item.get("id")),
                (Some(Value::String(sent)), Some(Value::String(now))) if sent != *now
            );
            if !opened && unopened_reference.is_none() {
                unopened_reference = Some(index);
            }
        }
    }
    unopened_reference
}

/// Whether an `input[]` item is an item reference: `type` is
/// `item_reference`, or it has no `type` and no `role`, the one item whose
/// `type` is optional and that has no role (openai-python
/// `types/responses/response_input_item_param.py`, `ItemReference`: "`id`:
/// The ID of the item to reference", `type: Optional[Literal["item_reference"]]`).
/// A `reasoning` item with an `id` and no `encrypted_content` is one too:
/// the upstream looks its reasoning up by that id ("Item with id 'rs_…' not
/// found", see [`unseal_responses_input`]).
fn is_item_reference(item: &Value) -> bool {
    match item.get("type") {
        Some(Value::String(t)) if t == "reasoning" => {
            item.get("id").is_some()
                && item
                    .get("encrypted_content")
                    .is_none_or(|content| content.is_null())
        }
        Some(Value::String(t)) => t == "item_reference",
        None | Some(Value::Null) => item.get("role").is_none() && item.get("id").is_some(),
        Some(_) => false,
    }
}

// What platform mode keeps of a Response object it holds parsed: the
// members and usage counters the API's type definitions document. A member
// an upstream adds beside them is the channel's own, and nothing a caller can
// rely on, so it is left out. Every documented member is kept, even one only
// some channels write (`service_tier`): it carries what a caller may act on,
// and hiding the channel is best effort, never paid for with what the caller
// gets. Only an answer already parsed is trimmed, buffered or built by the
// gateway; a relayed stream frame goes out as the upstream wrote it apart
// from its ids and model, since trimming it would cost a pass per frame.
// Telemetry has read the upstream's answer before any of this runs.

/// A Responses `Response` object's top-level members, as openai-python
/// 3.41.0 documents them (`types/responses/response.py`).
fn response_member(key: &str) -> bool {
    matches!(
        key,
        "id" | "access_programs"
            | "created_at"
            | "error"
            | "incomplete_details"
            | "instructions"
            | "metadata"
            | "model"
            | "object"
            | "output"
            | "parallel_tool_calls"
            | "temperature"
            | "tool_choice"
            | "tools"
            | "top_p"
            | "background"
            | "completed_at"
            | "conversation"
            | "max_output_tokens"
            | "max_tool_calls"
            | "moderation"
            | "previous_response_id"
            | "prompt"
            | "prompt_cache_diagnostics"
            | "prompt_cache_key"
            | "prompt_cache_options"
            | "prompt_cache_retention"
            | "reasoning"
            | "safety_identifier"
            | "service_tier"
            | "status"
            | "text"
            | "tool_usage"
            | "top_logprobs"
            | "truncation"
            | "usage"
            | "user"
    )
}

/// `types/responses/response_usage.py`.
fn responses_usage_member(key: &str) -> bool {
    matches!(
        key,
        "input_tokens"
            | "input_tokens_details"
            | "output_tokens"
            | "output_tokens_details"
            | "total_tokens"
    )
}

/// What platform mode keeps of a Response object.
pub(crate) const RESPONSE_BODY: &[Retain<'static>] = &[
    Retain {
        path: &[],
        keep: response_member,
    },
    Retain {
        path: &["usage"],
        keep: responses_usage_member,
    },
    Retain {
        path: &["usage", "input_tokens_details"],
        keep: |k| matches!(k, "cached_tokens" | "cache_write_tokens"),
    },
    Retain {
        path: &["usage", "output_tokens_details"],
        keep: |k| k == "reasoning_tokens",
    },
];

/// `value` with every member `rules` refuses removed from the objects they
/// name.
pub(crate) fn retain_value(value: &mut Value, rules: &[Retain<'_>]) {
    for rule in rules {
        let mut node = Some(&mut *value);
        for step in rule.path {
            node = node.and_then(|n| n.get_mut(*step));
        }
        if let Some(Value::Object(map)) = node {
            map.retain(|key, _| (rule.keep)(key));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    const K: [u8; 32] = [7u8; 32];
    /// The tenant the tests' ids are sealed to.
    const T: &str = "team-a";

    thread_local! {
        /// How many [`Sealer`]s this test's thread has built.
        pub(crate) static SEALERS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// How many [`SealKeys`] this test's thread has set up.
        pub(crate) static SEAL_KEYS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// How many seals this test's thread has computed, memo hits aside.
        pub(crate) static SEALS_MADE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn sealers_built() -> usize {
        SEALERS_BUILT.with(std::cell::Cell::get)
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
            let sealed = seal_id(&K, T, upstream);
            let prefix = &upstream[..=upstream.find('_').unwrap()];
            assert!(sealed.starts_with(prefix), "{sealed} keeps {prefix}");
            assert!(
                !sealed.contains("bdrk") && !sealed.contains("vrtx"),
                "{sealed}"
            );
            assert_ne!(sealed, upstream);
            assert_eq!(unseal_id(&K, T, &sealed).as_deref(), Some(upstream));
        }
    }

    #[test]
    fn sealing_is_deterministic() {
        assert_eq!(seal_id(&K, T, "fc_bdrk_1"), seal_id(&K, T, "fc_bdrk_1"));
        assert_ne!(seal_id(&K, T, "fc_bdrk_1"), seal_id(&K, T, "fc_bdrk_2"));
        assert_ne!(
            seal_id(&K, T, "fc_bdrk_1"),
            seal_id(&[8u8; 32], T, "fc_bdrk_1")
        );
    }

    /// A translated answer's call id is its upstream's tool-call id, whose
    /// prefix names the channel. Sealed as a call it is shown behind `call_`
    /// whatever that prefix was, at one length, and unseals to the original.
    #[test]
    fn a_sealed_call_id_shows_call_whatever_the_upstream_wrote() {
        let converse = seal_call(&K, T, "tooluse_bdrk_01Y");
        let anthropic = seal_call(&K, T, "toolu_vrtx_01Y");
        let openai = seal_call(&K, T, "call_abc");
        for sealed in [&converse, &anthropic, &openai] {
            assert!(sealed.starts_with("call_"), "{sealed}");
            assert!(
                !sealed.contains("tooluse") && !sealed.contains("toolu_"),
                "{sealed}"
            );
        }
        assert_eq!(converse.len(), anthropic.len());
        assert_eq!(converse.len(), openai.len());
        assert_eq!(
            unseal_call(&K, T, &converse).as_deref(),
            Some("tooluse_bdrk_01Y")
        );
        assert_eq!(
            unseal_call(&K, T, &anthropic).as_deref(),
            Some("toolu_vrtx_01Y")
        );
        assert_eq!(unseal_call(&K, T, &openai).as_deref(), Some("call_abc"));
        // Bound to the tenant like every seal.
        assert_eq!(unseal_call(&K, "team-b", &converse), None);
    }

    /// The kind is part of a seal: an item seal never opens as a call, a
    /// call seal never opens as an item, and neither opens behind another
    /// prefix.
    #[test]
    fn a_seal_opens_only_as_the_kind_it_was_made_for() {
        let item = seal_id(&K, T, "fc_bdrk_1");
        let call = seal_call(&K, T, "tooluse_bdrk_1");
        assert_eq!(unseal_call(&K, T, &item), None);
        assert_eq!(unseal_call(&K, T, &item.replacen("fc_", "call_", 1)), None);
        assert_eq!(unseal_id(&K, T, &call), None);
        assert_eq!(
            unseal_id(&K, T, &call.replacen("call_", "tooluse_", 1)),
            None
        );
        // Even where the shown prefixes agree, the two kinds seal apart.
        let as_item = seal_id(&K, T, "call_x");
        let as_call = seal_call(&K, T, "call_x");
        assert_ne!(as_item, as_call);
        assert_eq!(unseal_call(&K, T, &as_item), None);
        assert_eq!(unseal_id(&K, T, &as_call), None);
    }

    /// The length of a sealed id says nothing about the id it was made
    /// from: one upstream's short ids and another's long ones look alike.
    #[test]
    fn sealed_ids_are_one_length_whatever_the_upstream_id_was() {
        let short = "call_1";
        let long = format!("call_{}", "b".repeat(57));
        assert_eq!(long.len(), 62);
        assert_eq!(seal_id(&K, T, short).len(), seal_id(&K, T, &long).len());
        // The payload behind any kept prefix is one length too.
        let payload = |upstream: &str| {
            let sealed = seal_id(&K, T, upstream);
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
            seal_id(&K, T, &"a".repeat(64)).len(),
            seal_id(&K, T, &"a".repeat(127)).len()
        );
        assert!(seal_id(&K, T, &"a".repeat(64)).len() > seal_id(&K, T, &"a".repeat(63)).len());
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
                    unseal_id(&K, T, &seal_id(&K, T, &upstream)).as_deref(),
                    Some(upstream.as_str()),
                    "length {}",
                    upstream.len()
                );
            }
        }
    }

    #[test]
    fn a_truncated_or_tampered_seal_opens_to_nothing() {
        let sealed = seal_id(&K, T, "fc_bdrk_01Z");
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
            assert_eq!(unseal_id(&K, T, &broken), None, "{broken}");
        }
    }

    #[test]
    fn padding_is_read_back_only_as_it_was_written() {
        assert_eq!(unpad(&pad("ab")), Some(&b"ab"[..]));
        assert_eq!(pad("").len(), SEAL_BUCKET);
        assert_eq!(pad(&"a".repeat(63)).len(), SEAL_BUCKET);
        assert_eq!(pad(&"a".repeat(64)).len(), 2 * SEAL_BUCKET);
        // Another bucket size pads the same way.
        assert_eq!(pad_to("", 128).len(), 128);
        assert_eq!(pad_to(&"h".repeat(127), 128).len(), 128);
        assert_eq!(pad_to(&"h".repeat(128), 128).len(), 256);
        assert_eq!(unpad(&pad_to("ab", 128)), Some(&b"ab"[..]));
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
            let sealed = seal_id(&K, T, upstream);
            assert!(
                !sealed.contains("p-123") && !sealed.contains("averyvery"),
                "{sealed}"
            );
            assert_eq!(
                unseal_id(&K, T, &sealed).as_deref(),
                Some(upstream),
                "{upstream}"
            );
        }
        // Enough samples that some payload carries a `_` of its own.
        for i in 0..64 {
            let upstream = format!("{i:08x}");
            assert_eq!(
                unseal_id(&K, T, &seal_id(&K, T, &upstream)).as_deref(),
                Some(upstream.as_str())
            );
        }
    }

    #[test]
    fn unseal_answers_none_for_anything_it_did_not_seal() {
        let sealed = seal_id(&K, T, "fc_bdrk_1");
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
            assert_eq!(unseal_id(&K, T, foreign), None, "{foreign}");
        }
        assert_eq!(unseal_id(&[8u8; 32], T, &sealed), None, "another key");
    }

    /// The upstream account is shared by every tenant: an id one tenant was
    /// handed must not open for another, or echoing it would have the
    /// upstream resolve what it stored for the first.
    #[test]
    fn a_seal_for_one_tenant_does_not_open_for_another() {
        let sealed = seal_id(&K, "team-a", "msg_bdrk_1");
        assert_eq!(
            unseal_id(&K, "team-a", &sealed).as_deref(),
            Some("msg_bdrk_1")
        );
        assert_eq!(unseal_id(&K, "team-b", &sealed), None);
        // One upstream id, one sealed form per tenant.
        assert_ne!(sealed, seal_id(&K, "team-b", "msg_bdrk_1"));
        // An owner whose name starts another's is still another owner.
        assert_eq!(unseal_id(&K, "team-", &sealed), None);
        assert_eq!(unseal_id(&K, "team-ab", &sealed), None);

        // Inbound, another tenant's seals stay the strings that tenant sent;
        // the tenant they were handed to gets the upstream's ids back.
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let k = seal_key(&platform.id_key);
        let mut body = json!({"model": "m", "input": [
            {"type": "item_reference", "id": seal_id(&k, "team-a", "msg_bdrk_1")},
            {"type": "function_call_output", "call_id": seal_call(&k, "team-a", "call_bdrk_1"),
             "output": "sunny"},
        ]});
        let sent = body.clone();
        unseal_responses_input(&platform, "team-b", &mut body);
        assert_eq!(body, sent);
        unseal_responses_input(&platform, "team-a", &mut body);
        assert_eq!(body["input"][0]["id"], "msg_bdrk_1");
        assert_eq!(body["input"][1]["call_id"], "call_bdrk_1");
    }

    /// The owner's length prefix is what keeps one owner's bytes from being
    /// another owner's followed by the start of an id or prefix.
    #[test]
    fn the_owner_is_length_prefixed_in_the_nonce_and_the_associated_data() {
        assert_ne!(
            Sealer::new(&K, "ab").aad(IdKind::Item, "c_"),
            Sealer::new(&K, "a").aad(IdKind::Item, "bc_")
        );
        assert_ne!(
            Sealer::new(&K, "ab").nonce(IdKind::Item, "c"),
            Sealer::new(&K, "a").nonce(IdKind::Item, "bc")
        );
        assert_eq!(
            Sealer::new(&K, "ab").aad(IdKind::Item, "c_"),
            [&2u64.to_be_bytes()[..], b"abic_"].concat()
        );
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
        seal_responses_body(&Sealer::new(&K, T), &mut body);
        let text = body.to_string();
        assert!(!text.contains("bdrk") && !text.contains("prog_1"), "{text}");
        // The response-level id is not an item id.
        assert_eq!(body["id"], "resp_up");
        assert_eq!(
            body["output"][1]["call_id"],
            seal_call(&K, T, "call_bdrk_1")
        );
        assert_eq!(
            body["output"][1]["caller"]["caller_id"],
            seal_call(&K, T, "call_prog_1")
        );
        assert_eq!(
            body["output"][2]["approval_request_id"],
            seal_id(&K, T, "mcpr_bdrk_1")
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
        let sealer = Sealer::new(&K, T);
        for event in [&mut added, &mut delta, &mut done] {
            seal_responses_event(&sealer, event);
        }
        let item_id = seal_id(&K, T, "fc_bdrk_1");
        assert_eq!(added["item"]["id"], item_id);
        assert_eq!(delta["item_id"], item_id);
        assert_eq!(done["response"]["output"][0]["id"], item_id);
        assert_eq!(
            done["response"]["output"][0]["call_id"],
            seal_call(&K, T, "call_bdrk_1")
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
        let kind = std::cell::Cell::new(None);
        let mut seen = Vec::new();
        crate::json_splice::rewrite_string_values(
            event,
            |path| {
                kind.set(responses_event_id_kind(path));
                kind.get().is_some()
            },
            |v| {
                seen.push((v.to_owned(), kind.get().unwrap()));
                None
            },
        )
        .unwrap();
        use IdKind::{Call, Item};
        assert_eq!(
            seen,
            [
                ("fc_bdrk_1".to_string(), Item),
                ("fc_bdrk_1".to_string(), Item),
                ("call_bdrk_1".to_string(), Call),
                ("call_p".to_string(), Call),
                ("fc_bdrk_1".to_string(), Item),
                ("call_bdrk_1".to_string(), Call),
                ("mcpr_1".to_string(), Item),
            ]
        );
    }

    #[test]
    fn inbound_input_items_get_the_upstreams_own_ids_back() {
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let k = seal_key(&platform.id_key);
        let seal_id = |id: &str| seal_id(&k, T, id);
        let seal_call = |id: &str| seal_call(&k, T, id);
        let mut body = json!({
            "model": "m",
            "previous_response_id": "resp_up",
            "input": [
                {"type": "function_call", "id": seal_id("fc_bdrk_1"),
                 "call_id": seal_call("call_bdrk_1"), "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": seal_call("call_bdrk_1"), "output": "ok"},
                {"type": "mcp_approval_response", "approval_request_id": seal_id("mcpr_1"), "approve": true},
                {"type": "function_call_output", "call_id": "call_client_made", "output": "x",
                 "caller": {"type": "program", "caller_id": seal_call("call_p")}},
                {"type": "message", "role": "user", "content": "hi", "id": "msg_client"},
            ],
        });
        let before = sealers_built();
        unseal_responses_input(&platform, T, &mut body);
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
        unseal_responses_input(&platform, T, &mut plain);
        assert_eq!(plain, json!({"model": "m", "input": "hi"}));
    }

    /// A typed `reasoning` item with an `id` and no `encrypted_content` has
    /// the upstream look the reasoning up by that id ("Item with id 'rs_…'
    /// not found"), so it is a reference to a stored item like any other:
    /// refused unless the id is the tenant's own seal. With its encrypted
    /// content it carries itself and is not a reference.
    #[test]
    fn a_reasoning_item_named_only_by_its_id_is_a_reference() {
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let k = seal_key(&platform.id_key);
        let run = |input: Value| {
            let mut body = json!({"model": "m", "input": input});
            unseal_responses_input(&platform, T, &mut body)
        };
        assert_eq!(
            run(json!([{"role": "user", "content": "hi"},
                       {"type": "reasoning", "id": "rs_bdrk_1", "summary": []}])),
            Some(1)
        );
        assert_eq!(
            run(
                json!([{"type": "reasoning", "id": seal_id(&k, "team-b", "rs_1"),
                        "summary": [{"type": "summary_text", "text": "why"}]}])
            ),
            Some(0)
        );
        assert_eq!(
            run(json!([{"type": "reasoning", "id": seal_id(&k, T, "rs_1"), "summary": []}])),
            None
        );
        assert_eq!(
            run(json!([{"type": "reasoning", "id": "rs_1", "summary": [],
                        "encrypted_content": "gAAAA"}])),
            None
        );
        assert_eq!(run(json!([{"type": "reasoning", "summary": []}])), None);
    }

    /// A `local_shell_call_output` names the call it answers by `id`: the
    /// call id the answer's `local_shell_call` carried as `call_id`
    /// (openai-python 3.41.0 `types/responses/response_input_item_param.py`,
    /// `LocalShellCallOutput`, which has no `call_id`). It is a call seal,
    /// and opens as one; every other item's `id` is an item seal.
    #[test]
    fn a_local_shell_call_output_id_opens_as_the_call_it_answers() {
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let k = seal_key(&platform.id_key);
        let call = seal_call(&k, T, "call_up_1");
        let mut body = json!({"input": [
            {"type": "local_shell_call", "id": seal_id(&k, T, "lsh_up_1"), "call_id": call,
             "action": {"type": "exec", "command": ["ls"], "env": {}}, "status": "completed"},
            {"type": "local_shell_call_output", "id": call, "output": "a.txt"},
        ]});
        unseal_responses_input(&platform, T, &mut body);
        assert_eq!(body["input"][0]["id"], "lsh_up_1");
        assert_eq!(body["input"][0]["call_id"], "call_up_1");
        assert_eq!(body["input"][1]["id"], "call_up_1");
        // An item seal in that slot is not the call's, and stays as sent.
        let item = seal_id(&k, T, "call_up_1");
        let mut body = json!({"input": [
            {"type": "local_shell_call_output", "id": item.clone(), "output": "a.txt"}]});
        unseal_responses_input(&platform, T, &mut body);
        assert_eq!(body["input"][0]["id"], item);
    }

    /// A stream names one item on every delta: the sealer seals it once and
    /// hands back the same value, and a different id is sealed afresh.
    #[test]
    fn a_repeated_id_is_sealed_once() {
        let made = || SEALS_MADE.with(std::cell::Cell::get);
        let sealer = Sealer::new(&K, T);
        let msg_2 = seal_id(&K, T, "msg_2");
        let before = made();
        let first = sealer.seal(IdKind::Item, "fc_bdrk_1");
        for _ in 0..50 {
            assert_eq!(sealer.seal(IdKind::Item, "fc_bdrk_1"), first);
        }
        assert_eq!(made() - before, 1);
        // Another id, or the same one as another kind, is not the memo's.
        let call = sealer.seal(IdKind::Call, "fc_bdrk_1");
        assert_ne!(call, first);
        assert_eq!(sealer.seal(IdKind::Item, "msg_2"), msg_2);
        assert_eq!(sealer.seal(IdKind::Item, "fc_bdrk_1"), first);
        assert_eq!(made() - before, 4);
    }

    /// An item reference has the upstream resolve an item it stored, so the
    /// walk reports the first one whose id is not this tenant's own seal:
    /// another tenant's, a bare upstream id, or one too short to be a seal,
    /// with or without its optional `type`.
    #[test]
    fn an_item_reference_that_does_not_unseal_is_reported() {
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let k = seal_key(&platform.id_key);
        let own = seal_id(&k, T, "msg_bdrk_1");
        let run = |input: Value| {
            let mut body = json!({"model": "m", "input": input});
            let found = unseal_responses_input(&platform, T, &mut body);
            (found, body)
        };

        // The tenant's own reference opens, typed or not.
        let (found, body) = run(json!([
            {"role": "user", "content": "hi"},
            {"type": "item_reference", "id": own},
            {"id": own},
        ]));
        assert_eq!(found, None);
        assert_eq!(body["input"][1]["id"], "msg_bdrk_1");
        assert_eq!(body["input"][2]["id"], "msg_bdrk_1");

        for (input, at) in [
            (
                json!([{"type": "item_reference", "id": seal_id(&k, "team-b", "msg_bdrk_1")}]),
                0,
            ),
            (
                json!([{"role": "user", "content": "hi"}, {"type": "item_reference", "id": "msg_bdrk_1"}]),
                1,
            ),
            (json!([{"id": "msg_bdrk_1"}]), 0),
            (json!([{"type": "item_reference", "id": ""}]), 0),
            // A call seal is not an item's.
            (
                json!([{"type": "item_reference", "id": seal_call(&k, T, "msg_bdrk_1")}]),
                0,
            ),
            // The first that does not open is the one named.
            (
                json!([{"type": "item_reference", "id": own}, {"id": "x"}, {"id": "y"}]),
                1,
            ),
            // Another id field beside the reference that opens is no warrant
            // for its own id.
            (
                json!([{"type": "item_reference", "id": "msg_bdrk_1",
                        "call_id": seal_call(&k, T, "call_bdrk_1")}]),
                0,
            ),
            (
                json!([{"id": "msg_bdrk_1", "caller": {"caller_id": seal_call(&k, T, "call_bdrk_1")}}]),
                0,
            ),
            (
                json!([{"type": "item_reference", "id": seal_id(&k, "team-b", "msg_bdrk_1"),
                        "approval_request_id": seal_id(&k, T, "mcpr_1")}]),
                0,
            ),
        ] {
            assert_eq!(run(input.clone()).0, Some(at), "{input}");
        }

        // Anything that is not a reference is not reported, whatever its id.
        let (found, _) = run(json!([
            {"type": "message", "role": "assistant", "id": "msg_client", "content": []},
            {"role": "user", "id": "msg_x", "content": "hi"},
            {"type": "function_call", "id": "fc_x", "call_id": "call_x", "name": "f", "arguments": "{}"},
        ]));
        assert_eq!(found, None);
        assert_eq!(run(json!("hi")).0, None);
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
        unseal_responses_input(&platform, T, &mut body);
        assert_eq!(
            sealers_built(),
            before,
            "no sealer for a body with no candidate"
        );
        assert_eq!(body, expected);

        // The shortest seal there is clears the bar; one shorter cannot be one.
        let shortest = seal_id(&K, T, "");
        assert_eq!(shortest.len(), MIN_SEALED_LEN);
        let sealer = Sealer::new(&K, T);
        assert_eq!(sealer.unseal(IdKind::Item, &shortest).as_deref(), Some(""));
        assert_eq!(sealer.unseal(IdKind::Item, &shortest[1..]), None);
    }
}
