//! Credentials in stored documents: sealed at rest, redacted on every read,
//! and kept when a client sends the redacted placeholder back.
//!
//! A slot is every value `aisix_core::credentials::visit_credentials` finds
//! (the walk the gateway's own Admin API redacts with), plus the former
//! spelling of a credential field the schema still accepts
//! ([`super::normalize::schema_aliases`]). Normalisation renames the former
//! spelling before anything is stored, so the second set only matters for a
//! document that skipped it; it is covered all the same, so that no spelling
//! the gateway accepts is ever stored, shown or logged in plaintext.

use std::collections::HashMap;
use std::sync::LazyLock;

use aisix_core::credentials::{visit_credentials, CredentialSlot, REDACTED};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use uuid::Uuid;

use super::normalize::schema_aliases;
use super::Kind;
use crate::crypto::{CryptoError, MasterKey};
use crate::error::Detail;

/// What a sealed value starts with; the rest is
/// `base64url(nonce || ciphertext || tag)` without padding.
pub const SEALED_PREFIX: &str = "aisixsealed.v1.";

/// The additional authenticated data a slot is sealed with, which binds the
/// sealed value to this slot of this row.
fn aad(kind: Kind, id: Uuid, field_key: &str) -> String {
    format!("gw:{}:{id}:{field_key}", kind.segment())
}

/// The former field names of `kind` whose canonical name is a credential.
fn credential_aliases(kind: Kind) -> &'static [String] {
    static FORMER: LazyLock<HashMap<Kind, Vec<String>>> = LazyLock::new(|| {
        Kind::ALL
            .into_iter()
            .filter_map(|kind| Some((kind, kind.credential_kind()?)))
            .map(|(kind, credential_kind)| {
                let former = schema_aliases(kind)
                    .iter()
                    .filter(|(canonical, _)| {
                        let mut probe = json!({});
                        probe[canonical.as_str()] = json!("probe");
                        let mut is_slot = false;
                        visit_credentials(credential_kind, &mut probe, &mut |slot, _| {
                            is_slot |= slot.field_key == *canonical;
                        });
                        is_slot
                    })
                    .map(|(_, former)| former.clone())
                    .collect();
                (kind, former)
            })
            .collect()
    });
    FORMER.get(&kind).map_or(&[], Vec::as_slice)
}

/// Calls `visit` on every credential slot of `doc`.
fn for_each_slot(kind: Kind, doc: &mut Value, visit: &mut dyn FnMut(&CredentialSlot, &mut Value)) {
    let Some(credential_kind) = kind.credential_kind() else {
        return;
    };
    visit_credentials(credential_kind, doc, visit);
    for former in credential_aliases(kind) {
        if let Some(value) = doc
            .get_mut(former.as_str())
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        {
            let slot = CredentialSlot {
                field_key: former.clone(),
                label: former.clone(),
            };
            visit(&slot, value);
        }
    }
}

/// Seals every credential slot of `doc`, the document of row `id`, in
/// place. A value already sealed is left as it is.
pub fn seal(mk: &MasterKey, kind: Kind, id: Uuid, doc: &mut Value) {
    for_each_slot(kind, doc, &mut |slot, value| {
        let Some(plain) = value.as_str() else {
            return;
        };
        if plain.starts_with(SEALED_PREFIX) {
            return;
        }
        let sealed = mk.seal(aad(kind, id, &slot.field_key).as_bytes(), plain.as_bytes());
        *value = Value::String(format!("{SEALED_PREFIX}{}", URL_SAFE_NO_PAD.encode(sealed)));
    });
}

/// Opens every sealed slot of `doc`, the stored document of row `id`, in
/// place: the plaintext the gateway is sent. A value that does not open
/// fails the whole document.
pub fn unseal(mk: &MasterKey, kind: Kind, id: Uuid, doc: &mut Value) -> Result<(), CryptoError> {
    let mut failed = None;
    for_each_slot(kind, doc, &mut |slot, value| {
        if failed.is_some() {
            return;
        }
        let Some(sealed) = value.as_str().and_then(|v| v.strip_prefix(SEALED_PREFIX)) else {
            return;
        };
        let opened = URL_SAFE_NO_PAD
            .decode(sealed)
            .map_err(|_| CryptoError::Open)
            .and_then(|bytes| mk.open(aad(kind, id, &slot.field_key).as_bytes(), &bytes))
            .and_then(|plain| String::from_utf8(plain).map_err(|_| CryptoError::Open));
        match opened {
            Ok(plain) => *value = Value::String(plain),
            Err(e) => failed = Some(e),
        }
    });
    failed.map_or(Ok(()), Err)
}

/// Replaces every credential slot of `doc` with [`REDACTED`], for a reader.
pub fn redact(kind: Kind, doc: &mut Value) {
    for_each_slot(kind, doc, &mut |_, value| {
        *value = Value::String(REDACTED.to_string());
    });
}

/// The fields that decide where a kind's credentials are sent: the host,
/// the address it is reached at, the trust placed in it, the protocol, and
/// for a custom guardrail the script that holds them. From the
/// `provider_key`, `guardrail` and `observability_exporter` schemas.
fn destination_fields(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::ProviderKeys => &[
            "provider",
            "adapter",
            "api_base",
            "apis",
            "resolve_addresses",
            "tls",
        ],
        Kind::Guardrails => &[
            "kind",
            "endpoint",
            "analyzer_url",
            "anonymizer_url",
            "region",
            "script",
        ],
        Kind::ObservabilityExporters => &["kind", "endpoint"],
        _ => &[],
    }
}

/// Whether `incoming` would send credentials anywhere `stored` does not.
/// Both sides are compared redacted, so the password in a URL does not
/// count and the rest of the URL does; an explicit `null` is an absent
/// field.
fn changed_destination(kind: Kind, incoming: &Value, stored: &Value) -> bool {
    let destination = |doc: &Value| {
        let mut doc = doc.clone();
        redact(kind, &mut doc);
        destination_fields(kind)
            .iter()
            .map(|field| doc.get(*field).filter(|v| !v.is_null()).cloned())
            .collect::<Vec<_>>()
    };
    destination(incoming) != destination(stored)
}

/// Gives each [`REDACTED`] slot of `incoming` the value `stored` (the row's
/// sealed document, or `null` for a new row) holds in the same slot, so a
/// client can save back a document it read without sending the secret.
///
/// Refused, at the slot's label: a placeholder for a slot with nothing
/// stored; a placeholder in a document whose destination differs from the
/// stored one ([`destination_fields`]), since a kept secret must never
/// follow a new destination, so every secret is entered again then; and a
/// value shaped like a sealed one, which no client is ever shown and which
/// would otherwise be stored unsealed.
pub fn restore(kind: Kind, incoming: &mut Value, stored: &Value) -> Result<(), Detail> {
    let moved = changed_destination(kind, incoming, stored);
    // Keyed by field key and label: the label keeps apart names that
    // sanitize to one field key (`x-a` and `x_a`). `None` marks a key two
    // stored slots share, which cannot be told apart.
    let mut kept: HashMap<(String, String), Option<String>> = HashMap::new();
    let mut stored = stored.clone();
    for_each_slot(kind, &mut stored, &mut |slot, value| {
        let key = (slot.field_key.clone(), slot.label.clone());
        let value = value.as_str().map(str::to_string);
        kept.entry(key)
            .and_modify(|existing| *existing = None)
            .or_insert(value);
    });
    let mut problem = None;
    for_each_slot(kind, incoming, &mut |slot, value| {
        if problem.is_some() {
            return;
        }
        let Some(written) = value.as_str() else {
            return;
        };
        if written.starts_with(SEALED_PREFIX) {
            problem = Some(Detail {
                path: slot.label.clone(),
                message: format!("机密的值不能以 {SEALED_PREFIX} 开头"),
            });
            return;
        }
        if written != REDACTED {
            return;
        }
        match kept.get(&(slot.field_key.clone(), slot.label.clone())) {
            Some(Some(_)) if moved => {
                problem = Some(Detail {
                    path: slot.label.clone(),
                    message: "发送机密的目标已更改，请重新填写该机密".to_string(),
                })
            }
            Some(Some(previous)) => *value = Value::String(previous.clone()),
            Some(None) => {
                problem = Some(Detail {
                    path: slot.label.clone(),
                    message: "无法确定要保留哪一个已保存的机密，请重新填写".to_string(),
                })
            }
            None => {
                problem = Some(Detail {
                    path: slot.label.clone(),
                    message: "该机密没有已保存的值".to_string(),
                })
            }
        }
    });
    problem.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const KEY: MasterKey = MasterKey([7; 32]);

    #[test]
    fn a_sealed_value_opens_only_for_its_row_and_slot() {
        let id = Uuid::new_v4();
        let mut doc = json!({"display_name": "pk", "api_key": "sk-1",
            "request": {"default_headers": {"x-token": "sk-2"}}});
        seal(&KEY, Kind::ProviderKeys, id, &mut doc);
        let sealed = doc.clone();
        assert_eq!(doc["display_name"], "pk");
        for v in [
            &doc["api_key"],
            &doc["request"]["default_headers"]["x-token"],
        ] {
            assert!(
                v.as_str().is_some_and(|s| s.starts_with(SEALED_PREFIX)),
                "{doc}"
            );
        }
        // Sealing again changes nothing.
        seal(&KEY, Kind::ProviderKeys, id, &mut doc);
        assert_eq!(doc, sealed);

        unseal(&KEY, Kind::ProviderKeys, id, &mut doc).expect("unseal");
        assert_eq!(doc["api_key"], "sk-1");
        assert_eq!(doc["request"]["default_headers"]["x-token"], "sk-2");

        let mut other_row = sealed.clone();
        assert!(unseal(&KEY, Kind::ProviderKeys, Uuid::new_v4(), &mut other_row).is_err());
        // The api_key's sealed value moved into the header slot does not open.
        let mut moved = sealed.clone();
        moved["request"]["default_headers"]["x-token"] = sealed["api_key"].clone();
        assert!(unseal(&KEY, Kind::ProviderKeys, id, &mut moved).is_err());
        let mut other_key = sealed;
        assert!(unseal(&MasterKey([8; 32]), Kind::ProviderKeys, id, &mut other_key).is_err());
    }

    #[test]
    fn kinds_without_credentials_are_left_alone() {
        let mut doc = json!({"display_name": "m", "api_key": "not-a-slot-here"});
        seal(&KEY, Kind::Models, Uuid::new_v4(), &mut doc);
        redact(Kind::Models, &mut doc);
        assert_eq!(doc["api_key"], "not-a-slot-here");
    }

    #[test]
    fn restore_takes_the_stored_value_of_the_same_slot() {
        let id = Uuid::new_v4();
        let mut stored = json!({"api_key": "sk-1", "request": {"default_headers": {"x-a": "h-1"}}});
        seal(&KEY, Kind::ProviderKeys, id, &mut stored);

        let mut incoming = stored.clone();
        redact(Kind::ProviderKeys, &mut incoming);
        restore(Kind::ProviderKeys, &mut incoming, &stored).expect("restore");
        assert_eq!(incoming, stored);

        let mut new_slot = json!({"api_key": "[REDACTED]",
            "request": {"default_headers": {"x_a": "[REDACTED]"}}});
        let problem =
            restore(Kind::ProviderKeys, &mut new_slot, &stored).expect_err("x_a was never stored");
        assert_eq!(problem.path, "default header x_a");
    }
}
