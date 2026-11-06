//! Context affinity for `/v1/responses` served through a routing model group.
//!
//! A follow-up turn can carry context only the upstream that produced it can
//! read: a reasoning item's `encrypted_content` (stateless clients echo it
//! back) and `previous_response_id` (the upstream stored the response). Sent
//! to another deployment or account, the first is rejected with
//! `invalid_encrypted_content` and the second is not found.
//!
//! So the gateway tags what a group's target produced with that target's
//! model id, in the values the client echoes back unchanged:
//!
//! - `encrypted_content` → `aisix_enc_v1:<base64url(target id)>;<original>`
//! - response `id` → `resp_` + base64url(`aisix:v1;<target id>;<original>`)
//!
//! On every request the tags are removed before anything else reads the
//! body, so a tagged value never reaches an upstream; for a group, the
//! decoded origin pins the attempt order ([`crate::routing`]) and every
//! attempt whose target cannot read an item's ciphertext gets the item
//! without it ([`ContextAffinity::body_for_target`]).
//!
//! Two targets can read each other's context when they share a provider key
//! (same credentials, same endpoint) — that is the [`Boundary`].
//!
//! In platform mode both tags are sealed instead ([`TenantSeal`]): encrypted
//! and authenticated under a key only the gateway holds, and bound to the
//! tenant the response was handed to. A response id is tagged on every
//! target, not only a group's, so that it names neither the target nor the
//! upstream's own id, and a continuation from it is admitted only for that
//! tenant ([`ContextAffinity::admit_continuation`]).

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use ring::aead;
use serde_json::Value;

use crate::json_splice::PathSeg;

const ENC_PREFIX: &str = "aisix_enc_v1:";
const RESP_ID_PREFIX: &str = "resp_";
const RESP_ID_TAG: &str = "aisix:v1;";

/// Platform mode's ciphertext tag: `aisix_enc_v2:<sealed origin>;<original>`.
const ENC_PREFIX_V2: &str = "aisix_enc_v2:";
/// The head of every plaintext platform mode seals.
const SEALED_TAG: &str = "aisix:v2;";
/// Associated data of a sealed response id.
const RESP_ID_AAD: &[u8] = b"aisix-responses-v2";
/// Associated data of a sealed ciphertext tag, so neither kind of seal opens
/// as the other.
const ENC_AAD: &[u8] = b"aisix-encrypted-content-v2";
/// The bucket a sealed plaintext's head is padded to: 128 bytes holds the
/// tag, an owner that names a team or key by UUID and a target id of up to
/// 75 bytes, so every target of a deployment (UUIDs, in managed and file
/// mode alike) seals to one length.
const HEAD_BUCKET: usize = 2 * crate::opaque_id::SEAL_BUCKET;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// The key platform mode seals response ids and ciphertext tags under:
/// derived from the platform `id_key` under its own label, so it is neither
/// that key nor the one output item ids are sealed under.
pub(crate) fn response_id_key(id_key: &[u8; 32]) -> [u8; 32] {
    crate::opaque_id::derive_key(id_key, b"resp-v2")
}

/// A response id sealed to `owner`: `resp_` + base64url(`nonce || ciphertext
/// || tag`), the AES-256-GCM encryption of
/// `aisix:v2;<owner>;<target_id>;<upstream_id>` under a random nonce. The
/// head (`aisix:v2;<owner>;<target_id>;`) is padded to a bucket of its own
/// ([`HEAD_BUCKET`]), and the upstream id to the bucket item ids use
/// ([`crate::opaque_id::pad`]), so the id's length tells neither which
/// target served it nor one upstream's id format from another's.
///
/// The one-off form, under a key set up for this call; a request seals under
/// the cipher the platform settings set up once ([`TenantSeal`]).
#[cfg(test)]
pub(crate) fn seal_response_id(
    key: &[u8; 32],
    owner: &str,
    target_id: &str,
    upstream_id: &str,
) -> String {
    seal_response_id_with(&cipher(key), owner, target_id, upstream_id)
}

fn seal_response_id_with(
    cipher: &aead::LessSafeKey,
    owner: &str,
    target_id: &str,
    upstream_id: &str,
) -> String {
    format!(
        "{RESP_ID_PREFIX}{}",
        seal(cipher, RESP_ID_AAD, owner, target_id, upstream_id)
    )
}

/// `(target id, upstream id)` of a [`seal_response_id`] value; `None` for an
/// id that was not sealed under `key` to `owner`.
#[cfg(test)]
pub(crate) fn open_response_id(
    key: &[u8; 32],
    owner: &str,
    sealed: &str,
) -> Option<(String, String)> {
    open_response_id_with(&cipher(key), owner, sealed)
}

fn open_response_id_with(
    cipher: &aead::LessSafeKey,
    owner: &str,
    sealed: &str,
) -> Option<(String, String)> {
    open(
        cipher,
        RESP_ID_AAD,
        owner,
        sealed.strip_prefix(RESP_ID_PREFIX)?,
    )
}

/// The AES-256-GCM cipher under `key`.
pub(crate) fn cipher(key: &[u8; 32]) -> aead::LessSafeKey {
    aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, key)
            .expect("a 32-byte key is the length AES-256-GCM takes"),
    )
}

/// An owner or target id as it is written into a plaintext: `%` and `;`
/// escaped, so a field whose value extends another's past a `;` cannot pass
/// for it.
fn escaped(field: &str) -> Cow<'_, str> {
    if field.contains(['%', ';']) {
        Cow::Owned(field.replace('%', "%25").replace(';', "%3B"))
    } else {
        Cow::Borrowed(field)
    }
}

/// The field [`escaped`] wrote; `None` for anything it could not have.
fn unescaped(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let code = rest.get(at + 1..at + 3)?;
        out.push(match code {
            "25" => '%',
            "3B" => ';',
            _ => return None,
        });
        rest = &rest[at + 3..];
    }
    out.push_str(rest);
    Some(out)
}

fn seal(
    cipher: &aead::LessSafeKey,
    aad: &[u8],
    owner: &str,
    target_id: &str,
    value: &str,
) -> String {
    let mut sealed = crate::opaque_id::pad_to(
        &format!("{SEALED_TAG}{};{};", escaped(owner), escaped(target_id)),
        HEAD_BUCKET,
    );
    sealed.extend_from_slice(&crate::opaque_id::pad(value));
    let mut nonce = [0u8; aead::NONCE_LEN];
    // The operating system's random source; there is no id to hand out
    // without it.
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
        .expect("the system random source is readable");
    cipher
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad),
            &mut sealed,
        )
        .expect("a sealed value is far below the AES-GCM message limit");
    let mut payload = Vec::with_capacity(aead::NONCE_LEN + sealed.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&sealed);
    b64().encode(payload)
}

fn open(
    cipher: &aead::LessSafeKey,
    aad: &[u8],
    owner: &str,
    payload: &str,
) -> Option<(String, String)> {
    let raw = b64().decode(payload).ok()?;
    if raw.len() < aead::NONCE_LEN + HEAD_BUCKET + crate::opaque_id::SEAL_BUCKET + aead::MAX_TAG_LEN
    {
        return None;
    }
    let (nonce, sealed) = raw.split_at(aead::NONCE_LEN);
    let mut buf = sealed.to_vec();
    let plain = cipher
        .open_in_place(
            aead::Nonce::try_assume_unique_for_key(nonce).ok()?,
            aead::Aad::from(aad),
            &mut buf,
        )
        .ok()?;
    // The head ends at the `;` after the target, and its padding at the
    // bucket that follows; the upstream id is padded on its own after it.
    let owner_head = format!("{SEALED_TAG}{};", escaped(owner));
    let rest = plain.strip_prefix(owner_head.as_bytes())?;
    let target_len = rest.iter().position(|&b| b == b';')?;
    let head_len = owner_head.len() + target_len + 1;
    let padded_head = (head_len + 1).div_ceil(HEAD_BUCKET) * HEAD_BUCKET;
    let (head, value) = (plain.get(..padded_head)?, plain.get(padded_head..)?);
    if crate::opaque_id::unpad(head)?.len() != head_len {
        return None;
    }
    let target = unescaped(std::str::from_utf8(&rest[..target_len]).ok()?)?;
    let value = std::str::from_utf8(crate::opaque_id::unpad(value)?).ok()?;
    (!target.is_empty()).then(|| (target, value.to_owned()))
}

/// Platform mode: the key response ids and ciphertext tags are sealed under,
/// and the tenant they are sealed to — the caller key's team, or the key
/// itself when it has none.
#[derive(Clone)]
pub(crate) struct TenantSeal {
    /// The cipher the platform settings set up once.
    cipher: Arc<aead::LessSafeKey>,
    owner: String,
}

impl std::fmt::Debug for TenantSeal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantSeal")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl TenantSeal {
    pub(crate) fn new(platform: &crate::platform::PlatformSettings, owner: &str) -> Self {
        Self {
            cipher: platform.response_cipher().clone(),
            owner: owner.to_string(),
        }
    }

    fn mark_encrypted_content(&self, target_id: &str, original: &str) -> String {
        format!(
            "{ENC_PREFIX_V2}{};{original}",
            seal(&self.cipher, ENC_AAD, &self.owner, target_id, "")
        )
    }

    /// A ciphertext tag as this tenant reads it. A tag sealed to anyone else
    /// does not decode, and neither does the readable kind platform mode
    /// never hands out: anyone can write one, naming any target.
    fn decode_encrypted_content(&self, value: &str) -> EncryptedContent {
        let Some(rest) = value.strip_prefix(ENC_PREFIX_V2) else {
            return if value.starts_with(ENC_PREFIX) {
                EncryptedContent::Undecodable
            } else {
                EncryptedContent::Plain
            };
        };
        let opened = rest.split_once(';').and_then(|(tag, original)| {
            let (origin, _) = open(&self.cipher, ENC_AAD, &self.owner, tag)?;
            Some((origin, original.to_string()))
        });
        match opened {
            Some((origin, original)) => EncryptedContent::Marked { origin, original },
            None => EncryptedContent::Undecodable,
        }
    }
}

/// Platform mode: the one sealed id a response carries, made on first use.
/// Its nonce is random, so every frame of the response must reuse it.
#[derive(Debug)]
struct SealedResponseId {
    tenant: TenantSeal,
    /// The target that served the response.
    target_id: String,
    /// `(upstream id, sealed id)`.
    sealed: OnceLock<(String, String)>,
}

impl SealedResponseId {
    fn seal(&self, upstream_id: &str) -> String {
        let fresh = || {
            seal_response_id_with(
                &self.tenant.cipher,
                &self.tenant.owner,
                &self.target_id,
                upstream_id,
            )
        };
        let (first, sealed) = self
            .sealed
            .get_or_init(|| (upstream_id.to_string(), fresh()));
        if first == upstream_id {
            sealed.clone()
        } else {
            fresh()
        }
    }
}

pub(crate) fn mark_encrypted_content(target_id: &str, original: &str) -> String {
    format!("{ENC_PREFIX}{};{original}", b64().encode(target_id))
}

pub(crate) fn mark_response_id(target_id: &str, original: &str) -> String {
    format!(
        "{RESP_ID_PREFIX}{}",
        b64().encode(format!("{RESP_ID_TAG}{target_id};{original}"))
    )
}

enum EncryptedContent {
    /// Not a gateway tag: forwarded as the client sent it.
    Plain,
    Marked {
        origin: String,
        original: String,
    },
    /// Carries the tag prefix but does not decode.
    Undecodable,
}

fn decode_encrypted_content(value: &str) -> EncryptedContent {
    let Some(rest) = value.strip_prefix(ENC_PREFIX) else {
        return EncryptedContent::Plain;
    };
    let decoded = rest.split_once(';').and_then(|(origin, original)| {
        let origin = String::from_utf8(b64().decode(origin).ok()?).ok()?;
        (!origin.is_empty()).then(|| (origin, original.to_string()))
    });
    match decoded {
        Some((origin, original)) => EncryptedContent::Marked { origin, original },
        None => EncryptedContent::Undecodable,
    }
}

/// `(origin target id, original id)` of a tagged response id; `None` for any
/// other id, which is forwarded unchanged.
fn decode_response_id(value: &str) -> Option<(String, String)> {
    let raw = b64().decode(value.strip_prefix(RESP_ID_PREFIX)?).ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (origin, original) = text.strip_prefix(RESP_ID_TAG)?.split_once(';')?;
    (!origin.is_empty()).then(|| (origin.to_string(), original.to_string()))
}

/// The targets that can read context produced by one origin target: the
/// origin itself, and every target on the same provider key. The key is read
/// from the origin's own model document, so it resolves even when the origin
/// is not a member of the group the request addressed; an origin that no
/// longer exists has an empty boundary.
#[derive(Debug, Clone)]
pub(crate) struct Boundary {
    origin_id: String,
    provider_key_id: Option<String>,
}

impl Boundary {
    pub(crate) fn of(snapshot: &aisix_core::AisixSnapshot, origin_id: &str) -> Self {
        Self {
            origin_id: origin_id.to_string(),
            provider_key_id: snapshot
                .models
                .get_by_id(origin_id)
                .and_then(|m| m.value.provider_key_id.clone()),
        }
    }

    pub(crate) fn is_origin(&self, target_id: &str) -> bool {
        self.origin_id == target_id
    }

    pub(crate) fn contains(&self, target_id: &str, target_provider_key_id: Option<&str>) -> bool {
        self.is_origin(target_id)
            || (self.provider_key_id.is_some()
                && self.provider_key_id.as_deref() == target_provider_key_id)
    }
}

/// What a request's tags said, recorded while removing them.
#[derive(Debug, Default)]
pub(crate) struct ContextAffinity {
    previous_origin: Option<String>,
    /// `(original, as the client sent it)` for a tagged `previous_response_id`,
    /// so the Response object can echo back the value the client sent.
    previous_echo: Option<(String, String)>,
    /// Index into `input[]` of each item whose ciphertext was tagged, with
    /// its origin. Items whose tag did not decode were already stripped.
    encrypted: Vec<(usize, String)>,
    /// Platform mode: the tenant this request's tags are sealed to.
    tenant: Option<TenantSeal>,
    /// Platform mode: the `previous_response_id` names no response this
    /// tenant was handed.
    previous_unknown: bool,
    /// Platform mode: the index into `input[]` of the first item reference
    /// whose id is not this tenant's seal
    /// ([`crate::opaque_id::unseal_responses_input`]).
    unopened_item_reference: Option<usize>,
}

impl ContextAffinity {
    /// Restore every tagged value in a `/v1/responses` request body to the
    /// upstream's original bytes, and strip ciphertext whose tag does not
    /// decode (its origin is unknowable, so no upstream can be trusted to
    /// read it).
    pub(crate) fn unwrap_request(body: &mut Value) -> Self {
        let mut affinity = Self::default();
        if let Some(slot) = body.get_mut("previous_response_id") {
            if let Some((origin, original)) = slot.as_str().and_then(decode_response_id) {
                affinity.restore_previous(slot, origin, original);
            }
        }
        affinity.unwrap_input(body, decode_encrypted_content);
        affinity
    }

    /// [`Self::unwrap_request`] in platform mode, for a request of `tenant`:
    /// only what was sealed to it is restored. A `previous_response_id` that
    /// does not open is left as sent and recorded, for
    /// [`Self::admit_continuation`] to refuse; a ciphertext tag that does not
    /// open is stripped like any undecodable one.
    pub(crate) fn unwrap_sealed_request(body: &mut Value, tenant: TenantSeal) -> Self {
        let mut affinity = Self::default();
        if let Some(slot) = body
            .get_mut("previous_response_id")
            .filter(|v| v.is_string())
        {
            match slot
                .as_str()
                .and_then(|id| open_response_id_with(&tenant.cipher, &tenant.owner, id))
            {
                Some((origin, original)) => affinity.restore_previous(slot, origin, original),
                None => affinity.previous_unknown = true,
            }
        }
        affinity.unwrap_input(body, |value| tenant.decode_encrypted_content(value));
        affinity.tenant = Some(tenant);
        affinity
    }

    fn restore_previous(&mut self, slot: &mut Value, origin: String, original: String) {
        let sent = std::mem::replace(slot, Value::String(original.clone()));
        if let Value::String(sent) = sent {
            self.previous_echo = Some((original, sent));
        }
        self.previous_origin = Some(origin);
    }

    fn unwrap_input(&mut self, body: &mut Value, decode: impl Fn(&str) -> EncryptedContent) {
        let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
            return;
        };
        if !input.iter().any(|item| {
            item.get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|s| s.starts_with(ENC_PREFIX) || s.starts_with(ENC_PREFIX_V2))
        }) {
            return;
        }
        let items = std::mem::take(input);
        for mut item in items {
            let decoded = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(&decode);
            match decoded {
                Some(EncryptedContent::Marked { origin, original }) => {
                    item["encrypted_content"] = Value::String(original);
                    self.encrypted.push((input.len(), origin));
                }
                Some(EncryptedContent::Undecodable) => match strip_ciphertext(item) {
                    Some(kept) => item = kept,
                    None => continue,
                },
                Some(EncryptedContent::Plain) | None => {}
            }
            input.push(item);
        }
    }

    /// Platform mode: record what [`crate::opaque_id::unseal_responses_input`]
    /// found, for [`Self::refuse_unopened_item_reference`].
    pub(crate) fn note_unopened_item_reference(&mut self, index: Option<usize>) {
        self.unopened_item_reference = index;
    }

    /// Platform mode: refuse a request whose `input[]` references an item by
    /// an id that is not this tenant's own seal: another tenant's, a forged
    /// one, or a bare upstream id. The upstream would resolve it against
    /// everything the shared account stored. It is refused like any other
    /// reference to an object stored upstream, naming the field and never
    /// the id, and nothing reaches an upstream.
    pub(crate) fn refuse_unopened_item_reference(&self) -> Result<(), crate::error::ProxyError> {
        match self.unopened_item_reference {
            Some(index) => Err(crate::error::ProxyError::StoredObjectReference(format!(
                "input[{index}].id"
            ))),
            None => Ok(()),
        }
    }

    /// Platform mode: whether this request may continue from the response
    /// its `previous_response_id` names. One that was not sealed to the
    /// caller's tenant is a response it was never handed; one served by a
    /// target the key's provider groups no longer hold is one the key cannot
    /// reach. Both are answered as a response that does not exist, before
    /// anything is sent upstream, so the refusal cannot tell which.
    ///
    /// Keys on the DISPATCHED target recorded in the id, never on the model
    /// this request addresses: the target of a direct or embedding model is
    /// the model itself, a wildcard alias's the row that served it, and a
    /// routing group's the member that answered. Semantic and ensemble
    /// parents never dispatch on this endpoint. A target that no longer
    /// exists cannot be shown to be in a group, so a grouped key is refused
    /// it; a key without groups is not held to them. Outside platform mode
    /// everything is admitted, as before.
    pub(crate) fn admit_continuation(
        &self,
        snapshot: &aisix_core::AisixSnapshot,
        allowed_groups: &[String],
    ) -> Result<(), crate::error::ProxyError> {
        if self.previous_unknown {
            tracing::debug!("previous_response_id was not issued to this tenant");
            return Err(crate::error::ProxyError::ResponseNotFound);
        }
        let (Some(_), Some(origin)) = (&self.tenant, self.previous_origin.as_deref()) else {
            return Ok(());
        };
        if allowed_groups.is_empty()
            || snapshot.models.get_by_id(origin).is_some_and(|entry| {
                crate::routing::target_in_groups(snapshot, &entry.value, allowed_groups)
            })
        {
            return Ok(());
        }
        tracing::debug!(
            origin,
            "previous response was served outside the API key's provider groups"
        );
        Err(crate::error::ProxyError::ResponseNotFound)
    }

    /// The target that produced this conversation's context, if a tag named
    /// one: the stored `previous_response_id` first, else the latest tagged
    /// ciphertext in `input[]`.
    pub(crate) fn origin(&self) -> Option<&str> {
        self.previous_origin
            .as_deref()
            .or_else(|| self.encrypted.last().map(|(_, o)| o.as_str()))
    }

    /// The body one routing-group attempt sends: `None` when every tagged
    /// ciphertext is readable by `target`, else a copy without the items
    /// it cannot read.
    pub(crate) fn body_for_target(
        &self,
        snapshot: &aisix_core::AisixSnapshot,
        body: &Value,
        target_id: &str,
        target_provider_key_id: Option<&str>,
    ) -> Option<Value> {
        let unreadable: Vec<usize> = self
            .encrypted
            .iter()
            .filter(|(_, origin)| {
                !Boundary::of(snapshot, origin).contains(target_id, target_provider_key_id)
            })
            .map(|(idx, _)| *idx)
            .collect();
        if unreadable.is_empty() {
            return None;
        }
        let mut body = body.clone();
        if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
            let items = std::mem::take(input);
            for (idx, item) in items.into_iter().enumerate() {
                if !unreadable.contains(&idx) {
                    input.push(item);
                } else if let Some(kept) = strip_ciphertext(item) {
                    input.push(kept);
                }
            }
        }
        Some(body)
    }

    /// How a response to this request is tagged. `target_id` is the target
    /// that served it; `in_group` says it was reached through a routing
    /// group, the only case outside platform mode that is tagged.
    pub(crate) fn response_mark(&self, target_id: &str, in_group: bool) -> ResponseMark {
        ResponseMark {
            target_id: in_group.then(|| target_id.to_string()),
            previous_echo: self.previous_echo.clone(),
            seal: None,
            tenant: self.tenant.clone().map(|tenant| {
                Arc::new(SealedResponseId {
                    tenant,
                    target_id: target_id.to_string(),
                    sealed: OnceLock::new(),
                })
            }),
        }
    }
}

/// An item without its ciphertext and the item id the ciphertext belongs to;
/// `None` when nothing readable is left to send.
fn strip_ciphertext(mut item: Value) -> Option<Value> {
    let obj = item.as_object_mut()?;
    obj.remove("encrypted_content");
    obj.remove("id");
    let readable = ["summary", "content"].iter().any(|key| {
        obj.get(*key)
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|p| {
                    p.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| !t.is_empty())
                })
            })
    });
    readable.then_some(item)
}

/// The tags one response carries back to the client.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResponseMark {
    target_id: Option<String>,
    previous_echo: Option<(String, String)>,
    /// Platform mode: output item ids leave sealed
    /// ([`crate::opaque_id::seal_id`]), and channel metrics do not leave.
    seal: Option<std::sync::Arc<crate::opaque_id::Sealer>>,
    /// Platform mode: the response id leaves sealed to the tenant, and so
    /// does a group target's ciphertext tag.
    tenant: Option<Arc<SealedResponseId>>,
}

#[derive(Clone, Copy)]
enum Slot {
    /// The snapshot's `response.model`, restamped to the caller-facing name.
    Model,
    ResponseId,
    PreviousResponseId,
    EncryptedContent,
    /// An id platform mode seals, with the kind it holds.
    Id(crate::opaque_id::IdKind),
}

impl ResponseMark {
    fn is_noop(&self) -> bool {
        self.target_id.is_none()
            && self.previous_echo.is_none()
            && self.seal.is_none()
            && self.tenant.is_none()
    }

    /// Seal the response's item ids to `owner` when platform mode is on,
    /// with one [`crate::opaque_id::Sealer`] for every frame of the response.
    pub(crate) fn sealing_ids(
        mut self,
        platform: Option<&crate::platform::PlatformSettings>,
        owner: &str,
    ) -> Self {
        self.seal = platform.map(|p| std::sync::Arc::new(crate::opaque_id::Sealer::of(p, owner)));
        self
    }

    /// The sealer item ids are sealed with, for an encoder that builds the
    /// response itself; `None` outside platform mode.
    pub(crate) fn sealer(&self) -> Option<std::sync::Arc<crate::opaque_id::Sealer>> {
        self.seal.clone()
    }

    /// The `previous_response_id` as the client sent it, when it was tagged:
    /// what a Response object the gateway builds itself echoes.
    pub(crate) fn previous_response_id_sent(&self) -> Option<&str> {
        self.previous_echo.as_ref().map(|(_, sent)| sent.as_str())
    }

    /// A response id the gateway minted itself (the bridged path).
    pub(crate) fn response_id(&self, id: String) -> String {
        if let Some(tenant) = &self.tenant {
            return tenant.seal(&id);
        }
        match &self.target_id {
            Some(target) => mark_response_id(target, &id),
            None => id,
        }
    }

    fn rewrite(&self, slot: Slot, value: &str, model: Option<&str>) -> Option<String> {
        match slot {
            Slot::Model => model.map(str::to_string),
            Slot::ResponseId => match &self.tenant {
                Some(tenant) => (!value.is_empty()).then(|| tenant.seal(value)),
                None => self
                    .target_id
                    .as_deref()
                    .filter(|_| !value.is_empty())
                    .map(|t| mark_response_id(t, value)),
            },
            Slot::EncryptedContent => {
                self.target_id
                    .as_deref()
                    .filter(|_| !value.is_empty())
                    .map(|t| match &self.tenant {
                        Some(sealed) => sealed.tenant.mark_encrypted_content(t, value),
                        None => mark_encrypted_content(t, value),
                    })
            }
            Slot::PreviousResponseId => self
                .previous_echo
                .as_ref()
                .filter(|(original, _)| original == value)
                .map(|(_, sent)| sent.clone()),
            Slot::Id(kind) => self
                .seal
                .as_ref()
                .filter(|_| !value.is_empty())
                .map(|sealer| sealer.seal(kind, value)),
        }
    }

    /// Tag a parsed (non-streaming) Response object.
    pub(crate) fn apply_to_body(&self, body: &mut Value) {
        if self.is_noop() {
            return;
        }
        for (key, slot) in [
            ("id", Slot::ResponseId),
            ("previous_response_id", Slot::PreviousResponseId),
        ] {
            if let Some(v) = body.get_mut(key) {
                if let Some(new) = v.as_str().and_then(|s| self.rewrite(slot, s, None)) {
                    *v = Value::String(new);
                }
            }
        }
        if let Some(items) = body.get_mut("output").and_then(Value::as_array_mut) {
            for item in items {
                if let Some(v) = item.get_mut("encrypted_content") {
                    if let Some(new) = v
                        .as_str()
                        .and_then(|s| self.rewrite(Slot::EncryptedContent, s, None))
                    {
                        *v = Value::String(new);
                    }
                }
            }
        }
        if let Some(sealer) = &self.seal {
            crate::opaque_id::seal_responses_body(sealer, body);
            if let Some(map) = body.as_object_mut() {
                map.remove(crate::opaque_id::CHANNEL_METRICS_KEY);
            }
        }
    }

    /// Tag one Responses SSE event payload, byte-preserving: the Response
    /// object of the snapshot events (`response.created` / `.in_progress` /
    /// `.queued` / `.completed` / `.incomplete` / `.failed`) and the item of
    /// `response.output_item.added` / `.done`. In platform mode the same pass
    /// seals every item id, the `item_id` of a per-item event included.
    ///
    /// With `model`, the same pass restamps the snapshot's `response.model`
    /// to it ([`crate::model_echo::responses_snapshot_model`]).
    fn apply_to_event_json(&self, payload: &[u8], model: Option<&str>) -> Option<Vec<u8>> {
        let tagged = self.tag_event_json(payload, model);
        if self.seal.is_none() {
            return tagged;
        }
        crate::opaque_id::strip_channel_metrics(tagged.as_deref().unwrap_or(payload)).or(tagged)
    }

    fn tag_event_json(&self, payload: &[u8], model: Option<&str>) -> Option<Vec<u8>> {
        let slot = std::cell::Cell::new(Slot::ResponseId);
        let select = |path: &[PathSeg]| {
            let picked = match path {
                _ if model.is_some() && crate::model_echo::responses_snapshot_model(path) => {
                    Some(Slot::Model)
                }
                [PathSeg::Key(r), PathSeg::Key(k)] if r == "response" => match k.as_str() {
                    "id" => Some(Slot::ResponseId),
                    "previous_response_id" => Some(Slot::PreviousResponseId),
                    _ => None,
                },
                [PathSeg::Key(r), PathSeg::Key(o), PathSeg::Index(_), PathSeg::Key(e)]
                    if r == "response" && o == "output" && e == "encrypted_content" =>
                {
                    Some(Slot::EncryptedContent)
                }
                [PathSeg::Key(i), PathSeg::Key(e)] if i == "item" && e == "encrypted_content" => {
                    Some(Slot::EncryptedContent)
                }
                _ if self.seal.is_some() => {
                    crate::opaque_id::responses_event_id_kind(path).map(Slot::Id)
                }
                _ => None,
            };
            if let Some(s) = picked {
                slot.set(s);
            }
            picked.is_some()
        };
        match crate::json_splice::rewrite_string_values(payload, select, |v| {
            self.rewrite(slot.get(), v, model)
        }) {
            Ok(rewritten) => rewritten,
            Err(error) => {
                tracing::debug!(%error, "responses context tag skipped; forwarding verbatim");
                None
            }
        }
    }

    /// Tag one SSE frame; `None` forwards it unchanged.
    #[cfg(test)]
    pub(crate) fn apply_to_sse_frame(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if self.is_noop() {
            return None;
        }
        crate::model_echo::splice_sse_frame(frame, |payload| {
            self.apply_to_event_json(payload, None)
        })
    }

    /// Restamp one relayed SSE frame's `response.model` to
    /// `client_facing_model` and tag it, in one splice pass: what
    /// [`crate::model_echo::restamp_sse_frame`] then [`Self::apply_to_sse_frame`]
    /// do in two. `None` forwards it unchanged.
    pub(crate) fn restamp_and_tag_sse_frame(
        &self,
        frame: &[u8],
        client_facing_model: &str,
    ) -> Option<Vec<u8>> {
        crate::model_echo::splice_sse_frame(frame, |payload| {
            self.apply_to_event_json(payload, Some(client_facing_model))
        })
    }

    /// Tag every frame of a complete, buffered SSE document.
    pub(crate) fn apply_to_sse_buffer(&self, buf: Vec<u8>) -> Vec<u8> {
        if self.is_noop() {
            return buf;
        }
        crate::model_echo::splice_sse_buffer(&buf, |payload| {
            self.apply_to_event_json(payload, None)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tags_round_trip_and_foreign_values_are_plain() {
        let enc = mark_encrypted_content("m-1", "gAAAA;x");
        assert!(enc.starts_with("aisix_enc_v1:"));
        assert!(matches!(
            decode_encrypted_content(&enc),
            EncryptedContent::Marked { origin, original } if origin == "m-1" && original == "gAAAA;x"
        ));
        assert!(matches!(
            decode_encrypted_content("gAAAA"),
            EncryptedContent::Plain
        ));
        assert!(matches!(
            decode_encrypted_content("aisix_enc_v1:!!;x"),
            EncryptedContent::Undecodable
        ));

        let id = mark_response_id("m-1", "resp_abc");
        assert!(id.starts_with("resp_"));
        assert_eq!(
            decode_response_id(&id),
            Some(("m-1".into(), "resp_abc".into()))
        );
        assert_eq!(decode_response_id("resp_abc"), None);
        assert_eq!(decode_response_id("resp_68af0c1e2b"), None);
    }

    #[test]
    fn unwrap_restores_tags_and_strips_undecodable_ciphertext() {
        let mut body = json!({
            "previous_response_id": mark_response_id("m-1", "resp_up"),
            "input": [
                {"type": "reasoning", "id": "rs_1", "summary": [],
                 "encrypted_content": "aisix_enc_v1:%%;junk"},
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "reasoning", "id": "rs_2", "summary": [],
                 "encrypted_content": mark_encrypted_content("m-2", "CT")},
            ]
        });
        let a = ContextAffinity::unwrap_request(&mut body);
        assert_eq!(body["previous_response_id"], "resp_up");
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert_eq!(body["input"][1]["encrypted_content"], "CT");
        assert_eq!(a.origin(), Some("m-1"));
        assert_eq!(a.encrypted, vec![(1, "m-2".to_string())]);
    }

    #[test]
    fn strip_keeps_readable_reasoning_without_its_ciphertext() {
        let kept = strip_ciphertext(json!({
            "type": "reasoning", "id": "rs_1", "encrypted_content": "CT",
            "summary": [{"type": "summary_text", "text": "why"}]
        }))
        .unwrap();
        assert_eq!(
            kept,
            json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "why"}]})
        );
        assert!(strip_ciphertext(
            json!({"type": "reasoning", "summary": [], "encrypted_content": "CT"})
        )
        .is_none());
    }

    #[test]
    fn sse_frame_tags_snapshot_and_item_events() {
        let mark = ResponseMark {
            target_id: Some("m-1".into()),
            previous_echo: Some(("resp_prev".into(), "resp_SENT".into())),
            seal: None,
            tenant: None,
        };
        let frame = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_up\",\"previous_response_id\":\"resp_prev\",\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"CT\"}]}}\n\n";
        let out = mark.apply_to_sse_frame(frame).unwrap();
        let payload: Value = serde_json::from_slice(
            crate::redact::frame_payload(&out)
                .unwrap()
                .trim()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            payload["response"]["id"],
            mark_response_id("m-1", "resp_up")
        );
        assert_eq!(payload["response"]["previous_response_id"], "resp_SENT");
        assert_eq!(
            payload["response"]["output"][0]["encrypted_content"],
            mark_encrypted_content("m-1", "CT")
        );
        let item = b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"CT\"}}\n\n";
        let out = mark.apply_to_sse_frame(item).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("aisix_enc_v1:"));
        let delta = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n";
        assert!(mark.apply_to_sse_frame(delta).is_none());
    }

    fn sealing_mark(target_id: Option<&str>) -> (ResponseMark, [u8; 32]) {
        let platform = crate::platform::PlatformSettings::from_secret("s");
        let key = crate::opaque_id::seal_key(&platform.id_key);
        let mark = ContextAffinity::default()
            .response_mark(target_id.unwrap_or("m-1"), target_id.is_some())
            .sealing_ids(Some(&platform), "team-a");
        (mark, key)
    }

    #[test]
    fn a_sealing_mark_seals_item_ids_beside_the_context_tags() {
        use crate::opaque_id::{seal_call, seal_id};
        let (mark, key) = sealing_mark(Some("m-1"));
        let mut body = json!({
            "id": "resp_up",
            "output": [{"type": "function_call", "id": "fc_bdrk_1", "call_id": "call_bdrk_1",
                        "name": "f", "arguments": "{}"},
                       {"type": "reasoning", "id": "rs_1", "encrypted_content": "CT", "summary": []}],
            "amazon-bedrock-invocationMetrics": {"inputTokenCount": 1},
            "usage": {"input_tokens": 9, "input_tokens_details": {"cached_tokens": 1, "cache_creation_tokens": 3}},
        });
        mark.apply_to_body(&mut body);
        // A relayed answer keeps every usage field its upstream wrote.
        assert_eq!(
            body["usage"],
            json!({"input_tokens": 9, "input_tokens_details": {"cached_tokens": 1, "cache_creation_tokens": 3}})
        );
        assert_eq!(body["id"], mark_response_id("m-1", "resp_up"));
        assert_eq!(
            body["output"][0]["id"],
            seal_id(&key, "team-a", "fc_bdrk_1")
        );
        assert_eq!(
            body["output"][0]["call_id"],
            seal_call(&key, "team-a", "call_bdrk_1")
        );
        assert_eq!(body["output"][1]["id"], seal_id(&key, "team-a", "rs_1"));
        assert_eq!(
            body["output"][1]["encrypted_content"],
            mark_encrypted_content("m-1", "CT")
        );
        assert!(body.get("amazon-bedrock-invocationMetrics").is_none());

        let frame = b"data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_bdrk_1\",\"delta\":\"{\"}\n\n";
        let out = String::from_utf8(mark.apply_to_sse_frame(frame).unwrap()).unwrap();
        assert_eq!(
            out,
            format!(
                "data: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"{}\",\"delta\":\"{{\"}}\n\n",
                seal_id(&key, "team-a", "fc_bdrk_1")
            )
        );
        let done = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_up\",\"output\":[{\"id\":\"fc_bdrk_1\",\"call_id\":\"call_bdrk_1\"}]},\"amazon-bedrock-invocationMetrics\":{\"n\":1}}\n\n";
        let out = String::from_utf8(mark.apply_to_sse_frame(done).unwrap()).unwrap();
        assert!(!out.contains("bdrk"), "{out}");
        assert!(out.contains(&mark_response_id("m-1", "resp_up")), "{out}");
        assert!(out.ends_with("]}}\n\n"), "{out}");
        let buffered = mark.apply_to_sse_buffer([&frame[..], &done[..]].concat());
        assert!(!String::from_utf8(buffered).unwrap().contains("bdrk"));
    }

    #[test]
    fn a_mark_without_platform_mode_leaves_item_ids_alone() {
        let mark = ContextAffinity::default()
            .response_mark("m-1", false)
            .sealing_ids(None, "team-a");
        let mut body = json!({"id": "resp_up", "output": [{"id": "fc_1", "call_id": "call_1"}],
                              "amazon-bedrock-invocationMetrics": {}});
        let before = body.clone();
        mark.apply_to_body(&mut body);
        assert_eq!(body, before);
        let frame =
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\"}}\n\n";
        assert!(mark.apply_to_sse_frame(frame).is_none());
        assert!(mark.sealer().is_none());

        // Platform mode alone, outside a routing group, still seals.
        let (mark, _) = sealing_mark(None);
        assert!(mark.apply_to_sse_frame(frame).is_some());
    }

    #[test]
    fn a_sealed_response_id_opens_for_its_owner() {
        let k = [3u8; 32];
        let id = seal_response_id(&k, "team-a", "model-1", "resp_up");
        assert_eq!(
            open_response_id(&k, "team-a", &id),
            Some(("model-1".into(), "resp_up".into()))
        );
    }

    #[test]
    fn another_tenants_response_id_is_not_found() {
        let k = [3u8; 32];
        let id = seal_response_id(&k, "team-a", "model-1", "resp_up");
        assert_eq!(open_response_id(&k, "team-b", &id), None);
    }

    #[test]
    fn a_sealed_id_reveals_nothing_when_decoded() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let id = seal_response_id(&[3u8; 32], "team-a", "model-1", "resp_up");
        let raw = URL_SAFE_NO_PAD
            .decode(id.trim_start_matches("resp_"))
            .unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("model-1"));
    }

    /// Platform mode off: the tags are the bytes they always were.
    #[test]
    fn without_platform_mode_the_tags_are_unchanged_byte_for_byte() {
        assert_eq!(
            mark_response_id("m-1", "resp_abc"),
            format!("resp_{}", b64().encode("aisix:v1;m-1;resp_abc"))
        );
        assert_eq!(
            mark_encrypted_content("m-1", "CT"),
            format!("aisix_enc_v1:{};CT", b64().encode("m-1"))
        );
    }

    #[test]
    fn a_sealed_response_id_opens_only_as_it_was_sealed() {
        let k = [3u8; 32];
        let id = seal_response_id(&k, "team-a", "model-1", "resp_up;with;semicolons");
        assert_eq!(
            open_response_id(&k, "team-a", &id),
            Some(("model-1".into(), "resp_up;with;semicolons".into()))
        );
        // Another key, a flipped byte, a cut, or no prefix: nothing opens.
        assert_eq!(open_response_id(&[4u8; 32], "team-a", &id), None);
        let mut flipped = id.clone().into_bytes();
        let last = flipped.len() - 1;
        flipped[last] = if flipped[last] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            open_response_id(&k, "team-a", &String::from_utf8(flipped).unwrap()),
            None
        );
        assert_eq!(open_response_id(&k, "team-a", &id[..id.len() - 4]), None);
        assert_eq!(open_response_id(&k, "team-a", &id["resp_".len()..]), None);
        // Neither an upstream's id nor a tag readable by anyone is one.
        assert_eq!(open_response_id(&k, "team-a", "resp_up"), None);
        assert_eq!(
            open_response_id(&k, "team-a", &mark_response_id("model-1", "resp_up")),
            None
        );
        // Random nonces: the same response sealed twice is two ids.
        assert_ne!(
            id,
            seal_response_id(&k, "team-a", "model-1", "resp_up;with;semicolons")
        );
    }

    /// An owner that extends another owner's name past a `;` is still
    /// another owner.
    #[test]
    fn an_owner_whose_name_extends_another_opens_nothing_of_it() {
        let k = [3u8; 32];
        let id = seal_response_id(&k, "team;model-x", "model-1", "resp_up");
        assert_eq!(open_response_id(&k, "team", &id), None);
        assert_eq!(
            open_response_id(&k, "team;model-x", &id),
            Some(("model-1".into(), "resp_up".into()))
        );
    }

    /// A target id is escaped like the owner, and read back as it was.
    #[test]
    fn a_target_id_with_separators_reads_back_as_it_was() {
        let k = [3u8; 32];
        for target in ["m;1", "m%3B1", "%", "m;%;x"] {
            let id = seal_response_id(&k, "team", target, "resp_up");
            assert_eq!(
                open_response_id(&k, "team", &id),
                Some((target.to_string(), "resp_up".to_string())),
                "{target}"
            );
        }
        assert_eq!(unescaped("a%3B%25b").as_deref(), Some("a;%b"));
        for broken in ["%", "%3", "%41", "a%2"] {
            assert_eq!(unescaped(broken), None, "{broken}");
        }
    }

    /// The length of a sealed id says nothing about the target that served
    /// it: a short target id and a UUID one, behind an owner of either kind,
    /// seal to one length, and every one opens to what was sealed.
    #[test]
    fn sealed_response_ids_are_one_length_whatever_the_target_id_was() {
        let k = [3u8; 32];
        let uuid = "5a9d2c1b-3e4f-4a6b-8c7d-9e0f1a2b3c4d";
        let mut lengths = Vec::new();
        for owner in [
            "team:0c0e5f8a-7a8f-4d2b-9a53-6d1e3f2b4c5d",
            "key:0c0e5f8a-7a8f-4d2b-9a53-6d1e3f2b4c5d",
            "team:t",
        ] {
            for target in ["m-1", uuid, "gpt-4o-mini-on-a-long-routing-target-name"] {
                let id = seal_response_id(&k, owner, target, "resp_1");
                assert_eq!(
                    open_response_id(&k, owner, &id),
                    Some((target.to_string(), "resp_1".to_string())),
                    "{owner} {target}"
                );
                lengths.push(id.len());
            }
        }
        assert!(lengths.windows(2).all(|w| w[0] == w[1]), "{lengths:?}");
        // The ciphertext tag carries the same head, and the same length.
        let tenant = tenant("team:0c0e5f8a-7a8f-4d2b-9a53-6d1e3f2b4c5d");
        assert_eq!(
            tenant.mark_encrypted_content("m-1", "CT").len(),
            tenant.mark_encrypted_content(uuid, "CT").len()
        );
    }

    /// The length of a sealed id says nothing about the upstream id inside:
    /// one channel's short ids and another's long ones look alike.
    #[test]
    fn sealed_response_ids_are_one_length_whatever_the_upstream_id_was() {
        let k = [3u8; 32];
        let owner = "0c0e5f8a-7a8f-4d2b-9a53-6d1e3f2b4c5d";
        let target = "5a9d2c1b-3e4f-4a6b-8c7d-9e0f1a2b3c4d";
        let short = seal_response_id(&k, owner, target, "resp_1");
        // A 53-byte id of one channel, and the longest that shares a bucket.
        for long in [
            "resp_67ccd2bed1ec8190b14f964abc0542670bb6a6b452d3795b".to_string(),
            format!("resp_{}", "b".repeat(58)),
        ] {
            assert_eq!(
                short.len(),
                seal_response_id(&k, owner, target, &long).len()
            );
            assert_eq!(
                open_response_id(&k, owner, &seal_response_id(&k, owner, target, &long)),
                Some((target.to_string(), long.clone()))
            );
        }
    }

    #[test]
    fn the_response_id_key_is_its_own() {
        let id_key = [9u8; 32];
        let key = response_id_key(&id_key);
        assert_ne!(key, id_key);
        assert_ne!(key, crate::opaque_id::seal_key(&id_key));
        assert_eq!(key, response_id_key(&id_key));
    }

    fn tenant(owner: &str) -> TenantSeal {
        TenantSeal::new(&crate::platform::PlatformSettings::from_secret("s"), owner)
    }

    fn sealed_for(owner: &str, target: &str, upstream: &str) -> String {
        let platform = crate::platform::PlatformSettings::from_secret("s");
        seal_response_id(&response_id_key(&platform.id_key), owner, target, upstream)
    }

    #[test]
    fn a_sealed_previous_response_id_is_restored_for_its_owner() {
        let sent = sealed_for("team-a", "m-1", "resp_up");
        let mut body = json!({"previous_response_id": sent, "input": "hi"});
        let a = ContextAffinity::unwrap_sealed_request(&mut body, tenant("team-a"));
        assert_eq!(body["previous_response_id"], "resp_up");
        assert_eq!(a.origin(), Some("m-1"));
        assert!(!a.previous_unknown);
        // The Response object echoes what the client sent.
        let mark = a.response_mark("m-1", false);
        assert_eq!(mark.previous_response_id_sent(), Some(sent.as_str()));
    }

    #[test]
    fn a_previous_response_id_that_does_not_open_is_unknown() {
        for foreign in [
            sealed_for("team-a", "m-1", "resp_up"),
            "resp_up".to_string(),
            mark_response_id("m-1", "resp_up"),
            String::new(),
        ] {
            let mut body = json!({"previous_response_id": foreign, "input": "hi"});
            let a = ContextAffinity::unwrap_sealed_request(&mut body, tenant("team-b"));
            assert!(a.previous_unknown, "{foreign}");
            assert_eq!(a.origin(), None);
        }
        // No previous response at all is not an unknown one.
        for body in [
            json!({"input": "hi"}),
            json!({"input": "hi", "previous_response_id": null}),
        ] {
            let mut body = body;
            let a = ContextAffinity::unwrap_sealed_request(&mut body, tenant("team-b"));
            assert!(!a.previous_unknown, "{body}");
        }
    }

    #[test]
    fn a_previous_response_id_is_admitted_only_inside_the_keys_groups() {
        use aisix_core::resource::ResourceEntry;
        let snapshot = aisix_core::AisixSnapshot::new();
        let pk: aisix_core::ProviderKey = serde_json::from_value(json!({
            "display_name": "pk-a", "secret": "s", "provider": "openai", "adapter": "openai",
            "groups": ["ga"],
        }))
        .unwrap();
        snapshot
            .provider_keys
            .insert(ResourceEntry::new("pk-1", pk, 1));
        let model: aisix_core::Model = serde_json::from_value(json!({
            "display_name": "m", "provider": "openai", "model_name": "gpt-4o",
            "provider_key_id": "pk-1",
        }))
        .unwrap();
        snapshot.models.insert(ResourceEntry::new("m-1", model, 1));
        let continuing = |target: &str| {
            let mut body = json!({"previous_response_id": sealed_for("team-a", target, "resp_up")});
            ContextAffinity::unwrap_sealed_request(&mut body, tenant("team-a"))
        };
        let groups = |g: &[&str]| g.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert!(continuing("m-1").admit_continuation(&snapshot, &[]).is_ok());
        assert!(continuing("m-1")
            .admit_continuation(&snapshot, &groups(&["ga"]))
            .is_ok());
        for (target, allowed) in [("m-1", groups(&["gb"])), ("m-gone", groups(&["ga"]))] {
            let err = continuing(target)
                .admit_continuation(&snapshot, &allowed)
                .unwrap_err();
            assert!(
                matches!(err, crate::error::ProxyError::ResponseNotFound),
                "{target}: {err:?}"
            );
        }
        // A key without groups is not held to them, even for a target that is gone.
        assert!(continuing("m-gone")
            .admit_continuation(&snapshot, &[])
            .is_ok());
        // An id that did not open is never admitted.
        let mut body = json!({"previous_response_id": "resp_up"});
        let unknown = ContextAffinity::unwrap_sealed_request(&mut body, tenant("team-a"));
        assert!(unknown.admit_continuation(&snapshot, &[]).is_err());
        // Platform mode off admits everything, as before.
        let mut body = json!({"previous_response_id": "resp_up"});
        let off = ContextAffinity::unwrap_request(&mut body);
        assert!(off.admit_continuation(&snapshot, &groups(&["gb"])).is_ok());
    }

    #[test]
    fn a_tenant_mark_seals_the_response_id_once_for_every_frame() {
        let sent = sealed_for("team-a", "m-0", "resp_prev");
        let mut req = json!({"previous_response_id": sent});
        let affinity = ContextAffinity::unwrap_sealed_request(&mut req, tenant("team-a"));
        let mark = affinity.response_mark("m-1", false);
        let key = response_id_key(&crate::platform::PlatformSettings::from_secret("s").id_key);

        let mut body = json!({"id": "resp_bdrk_up", "previous_response_id": "resp_prev",
                              "output": [{"type": "reasoning", "encrypted_content": "CT"}]});
        mark.apply_to_body(&mut body);
        let id = body["id"].as_str().unwrap().to_string();
        assert_eq!(
            open_response_id(&key, "team-a", &id),
            Some(("m-1".into(), "resp_bdrk_up".into()))
        );
        assert_eq!(body["previous_response_id"], sent.as_str());
        // Outside a routing group no ciphertext is tagged, as before.
        assert_eq!(body["output"][0]["encrypted_content"], "CT");

        let created = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_bdrk_up\",\"previous_response_id\":\"resp_prev\"}}\n\n";
        let completed = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_bdrk_up\"}}\n\n";
        for frame in [&created[..], &completed[..]] {
            let out = String::from_utf8(mark.apply_to_sse_frame(frame).unwrap()).unwrap();
            assert!(out.contains(&id), "{out}");
            assert!(!out.contains("resp_bdrk_up"), "{out}");
        }
        let buffered = mark.apply_to_sse_buffer([&created[..], &completed[..]].concat());
        assert_eq!(String::from_utf8(buffered).unwrap().matches(&id).count(), 2);
        // A response the gateway minted itself gets the same one id.
        assert_eq!(mark.response_id("resp_bdrk_up".into()), id);
        let minted = mark.response_id("resp_gw".into());
        assert_eq!(
            open_response_id(&key, "team-a", &minted),
            Some(("m-1".into(), "resp_gw".into()))
        );
    }

    #[test]
    fn a_tenant_mark_in_a_group_seals_the_ciphertext_tag_to_the_tenant() {
        let affinity = ContextAffinity::unwrap_sealed_request(&mut json!({}), tenant("team-a"));
        let mark = affinity.response_mark("m-1", true);
        let mut body = json!({"id": "resp_up", "output": [
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "why"}],
             "encrypted_content": "CT"}]});
        mark.apply_to_body(&mut body);
        let tag = body["output"][0]["encrypted_content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(tag.ends_with(";CT"), "{tag}");
        let decoded_head = tag
            .split_once(';')
            .and_then(|(head, _)| head.rsplit_once(':'))
            .and_then(|(_, payload)| b64().decode(payload).ok())
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&decoded_head).contains("m-1"),
            "{tag}"
        );
        assert!(!tag.starts_with(ENC_PREFIX), "{tag}");

        let echoed = || {
            json!({"input": [
                {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "why"}],
                 "encrypted_content": tag},
                {"type": "message", "role": "user", "content": "go on"},
            ]})
        };
        // The tenant it was handed to gets its origin back.
        let mut own = echoed();
        let a = ContextAffinity::unwrap_sealed_request(&mut own, tenant("team-a"));
        assert_eq!(own["input"][0]["encrypted_content"], "CT");
        assert_eq!(a.origin(), Some("m-1"));
        // Anyone else's request sends no upstream the ciphertext.
        let mut other = echoed();
        let b = ContextAffinity::unwrap_sealed_request(&mut other, tenant("team-b"));
        assert_eq!(b.origin(), None);
        assert!(
            other["input"][0].get("encrypted_content").is_none(),
            "{other}"
        );
        assert_eq!(other["input"][0]["summary"][0]["text"], "why");
        // A tag anyone can write names nothing in platform mode either.
        let mut forged = json!({"input": [{"type": "reasoning", "summary": [],
            "encrypted_content": mark_encrypted_content("m-1", "CT")}]});
        let f = ContextAffinity::unwrap_sealed_request(&mut forged, tenant("team-a"));
        assert_eq!(f.origin(), None);
        assert_eq!(forged["input"].as_array().unwrap().len(), 0);
    }

    /// The one splice pass a relayed frame takes writes exactly what the
    /// model restamp followed by the tag pass wrote, with platform mode on
    /// and off, inside a group and outside one.
    #[test]
    fn restamping_and_tagging_in_one_pass_matches_the_two_passes() {
        let frames: [&[u8]; 5] = [
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_up\",\"model\":\"gpt-4o-2024\",\"output\":[]}}\n\n",
            b"data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"call_id\":\"call_1\",\"type\":\"function_call\"}}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\"}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_up\",\"model\":\"gpt-4o-2024\",\"output\":[{\"id\":\"rs_1\",\"encrypted_content\":\"CT\"}]},\"amazon-bedrock-invocationMetrics\":{\"n\":1}}\n\n",
            b"data: [DONE]\n\n",
        ];
        let off_plain = ContextAffinity::default().response_mark("m-1", false);
        let off_group = ContextAffinity::default().response_mark("m-1", true);
        let (on_plain, _) = sealing_mark(None);
        let (on_group, _) = sealing_mark(Some("m-1"));
        for mark in [&off_plain, &off_group, &on_plain, &on_group] {
            for frame in frames {
                let restamped = crate::model_echo::restamp_sse_frame(
                    frame,
                    "alias",
                    crate::model_echo::responses_snapshot_model,
                );
                let after_restamp = restamped.as_deref().unwrap_or(frame);
                let two_passes = mark.apply_to_sse_frame(after_restamp);
                let two_passes = two_passes.as_deref().unwrap_or(after_restamp);
                let one_pass = mark.restamp_and_tag_sse_frame(frame, "alias");
                let one_pass = one_pass.as_deref().unwrap_or(frame);
                // The response id's nonce is random but reused within a
                // mark, so both runs see the same sealed id.
                assert_eq!(
                    String::from_utf8_lossy(one_pass),
                    String::from_utf8_lossy(two_passes)
                );
            }
        }
        // Off platform mode and outside a group, only the model moves.
        let out = off_plain
            .restamp_and_tag_sse_frame(frames[0], "alias")
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            String::from_utf8_lossy(frames[0]).replace("gpt-4o-2024", "alias")
        );
        assert!(off_plain
            .restamp_and_tag_sse_frame(frames[2], "alias")
            .is_none());
    }

    /// The keys are set up once per response, not once per frame or id.
    #[test]
    fn a_sealing_mark_builds_one_sealer_for_every_frame() {
        let built = || crate::opaque_id::tests::SEALERS_BUILT.with(std::cell::Cell::get);
        let before = built();
        let (mark, _) = sealing_mark(None);
        let frame = b"data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"x\"}\n\n";
        for _ in 0..50 {
            assert!(mark.apply_to_sse_frame(frame).is_some());
        }
        let mut body = json!({"output": [{"id": "fc_1", "call_id": "call_1"}]});
        mark.apply_to_body(&mut body);
        assert_eq!(built() - before, 1);
    }
}
