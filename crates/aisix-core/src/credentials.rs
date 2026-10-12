//! Where each resource kind keeps live upstream credentials in its stored
//! document, and the one walk over them that every surface which must not
//! show them shares: `aisix export` replaces each with an `${ENV_VAR}`
//! placeholder, and the Admin API read routes with [`REDACTED`].
//!
//! The walk runs over the serialized document rather than the typed
//! resource, so a reader that redacts and a reader that re-emits YAML see
//! the same field paths, and a credential added to a kind is redacted on
//! both by adding it here once.

use serde_json::Value;

/// What the Admin API read routes return in place of a credential.
pub const REDACTED: &str = "[REDACTED]";

/// The resource kinds that store credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// `api_key`, plus every string value under `request.default_headers`
    /// (outbound headers, which can carry auth) and
    /// `request.default_body_fields` (which can carry a secondary
    /// credential), at any depth. Non-string values, such as a boolean
    /// flag, are kept.
    ProviderKey,
    /// `api_key`, `access_key_secret` and `secret_access_key` at any depth
    /// (the Bedrock secret sits under `aws_credentials`).
    Guardrail,
    /// `secret`.
    McpServer,
    /// `secret`.
    A2aAgent,
    /// Every value under `headers` (an OTLP exporter's request headers,
    /// which routinely carry a vendor auth token).
    ObservabilityExporter,
}

/// Guardrail fields that hold a credential wherever they appear.
const GUARDRAIL_SECRET_KEYS: &[&str] = &["api_key", "access_key_secret", "secret_access_key"];

/// One credential found by [`visit_credentials`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSlot {
    /// Stable, environment-variable-safe key for the field within its
    /// entry (`api_key`, `HEADER_X_HONEYCOMB_TEAM`, …).
    pub field_key: String,
    /// Human label for the field (`api_key`, `header x-honeycomb-team`, …).
    pub label: String,
}

/// Uppercase a label and replace every non-alphanumeric byte with `_` so
/// it is a legal, stable environment-variable fragment. Not collapsed or
/// trimmed — determinism matters more than prettiness, and the raw shape
/// stays recognizable (`openai-prod` → `OPENAI_PROD`).
pub fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Call `visit` on every credential in `doc`, a stored document of `kind`.
/// Only non-empty strings are visited: an absent or empty field has
/// nothing to hide.
pub fn visit_credentials(
    kind: CredentialKind,
    doc: &mut Value,
    visit: &mut dyn FnMut(&CredentialSlot, &mut Value),
) {
    match kind {
        CredentialKind::ProviderKey => {
            top_level(doc, "api_key", visit);
            if let Some(request) = doc.get_mut("request") {
                string_map(request, "default_headers", "default header", visit);
                string_map(request, "default_body_fields", "default body field", visit);
            }
        }
        CredentialKind::Guardrail => by_key(doc, GUARDRAIL_SECRET_KEYS, visit),
        CredentialKind::McpServer | CredentialKind::A2aAgent => top_level(doc, "secret", visit),
        CredentialKind::ObservabilityExporter => string_map(doc, "headers", "header", visit),
    }
}

/// Replace every credential in `doc` with [`REDACTED`].
pub fn redact_credentials(kind: CredentialKind, doc: &mut Value) {
    visit_credentials(kind, doc, &mut |_, value| {
        *value = Value::String(REDACTED.to_string());
    });
}

fn is_nonempty_str(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.is_empty())
}

/// A top-level string field.
fn top_level(doc: &mut Value, field: &str, visit: &mut dyn FnMut(&CredentialSlot, &mut Value)) {
    if let Some(slot) = doc.get_mut(field).filter(|v| is_nonempty_str(v)) {
        let found = CredentialSlot {
            field_key: field.to_string(),
            label: field.to_string(),
        };
        visit(&found, slot);
    }
}

/// Every string whose object key is in `keys`, at any depth.
fn by_key(value: &mut Value, keys: &[&str], visit: &mut dyn FnMut(&CredentialSlot, &mut Value)) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if keys.contains(&key.as_str()) && is_nonempty_str(child) {
                    let found = CredentialSlot {
                        field_key: key.clone(),
                        label: key.clone(),
                    };
                    visit(&found, child);
                } else {
                    by_key(child, keys, visit);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                by_key(item, keys, visit);
            }
        }
        _ => {}
    }
}

/// Every string value of the object under `doc[field]`, at any depth. The `headers` map
/// keys its slots `HEADER_<NAME>`; any other map `<FIELD>_<NAME>`.
fn string_map(
    doc: &mut Value,
    field: &str,
    label: &str,
    visit: &mut dyn FnMut(&CredentialSlot, &mut Value),
) {
    let Some(Value::Object(map)) = doc.get_mut(field) else {
        return;
    };
    let prefix = if field == "headers" {
        "HEADER".to_string()
    } else {
        sanitize(field)
    };
    for (name, value) in map.iter_mut() {
        string_leaves(
            value,
            format!("{prefix}_{}", sanitize(name)),
            format!("{label} {name}"),
            visit,
        );
    }
}

/// Every string in `value`, at any depth: a body field can nest a
/// credential (`{"metadata": {"partner_token": ...}}`). A nested string's
/// key extends its parent's with the object key or array index.
fn string_leaves(
    value: &mut Value,
    field_key: String,
    label: String,
    visit: &mut dyn FnMut(&CredentialSlot, &mut Value),
) {
    match value {
        Value::String(_) if is_nonempty_str(value) => {
            visit(&CredentialSlot { field_key, label }, value);
        }
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let child_key = format!("{field_key}_{}", sanitize(key));
                string_leaves(child, child_key, format!("{label}.{key}"), visit);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                let child_key = format!("{field_key}_{index}");
                string_leaves(child, child_key, format!("{label}[{index}]"), visit);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn slots(kind: CredentialKind, mut doc: Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        visit_credentials(kind, &mut doc, &mut |slot, value| {
            out.push((slot.field_key.clone(), value.as_str().unwrap().to_string()));
        });
        out.sort();
        out
    }

    /// A resource property whose name suggests a credential must be either
    /// one this module redacts or named here as not secret, so a new
    /// credential field cannot reach the Admin API or `aisix export` in
    /// plaintext without someone deciding which it is.
    #[test]
    fn every_credential_looking_resource_property_is_classified() {
        // Walked by `visit_credentials` (`secret` is also the provider
        // key's former name for `api_key`, read back as `api_key`).
        const REDACTED: &[&str] = &[
            "api_key",
            "secret",
            "access_key_secret",
            "secret_access_key",
        ];
        const NOT_SECRET: &[&str] = &[
            "api_key_header",   // the name of a header, not its value
            "api_key_id",       // an identifier
            "auth_header_name", // the name of a header
            "auth_mode",
            "auth_type",
            "aws_credentials", // container; its secret is redacted
            "credential_mode",
            "credential_ref", // names an environment credential
            "max_tokens",
            "token_url",
        ];
        const HINTS: &[&str] = &[
            "secret",
            "password",
            "passwd",
            "token",
            "apikey",
            "api_key",
            "credential",
            "private",
            "auth",
            "bearer",
            "cookie",
        ];
        fn collect(node: &Value, out: &mut std::collections::BTreeSet<String>) {
            match node {
                Value::Object(map) => {
                    if let Some(Value::Object(props)) = map.get("properties") {
                        out.extend(props.keys().cloned());
                    }
                    map.values().for_each(|v| collect(v, out));
                }
                Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
                _ => {}
            }
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/resources");
        let mut names = std::collections::BTreeSet::new();
        for file in std::fs::read_dir(&dir).unwrap() {
            let path = file.unwrap().path();
            let schema: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            collect(&schema, &mut names);
        }
        assert!(
            names.contains("api_key"),
            "no schemas read from {}",
            dir.display()
        );
        let unclassified: Vec<_> = names
            .iter()
            .filter(|n| HINTS.iter().any(|h| n.to_ascii_lowercase().contains(h)))
            .filter(|n| !REDACTED.contains(&n.as_str()) && !NOT_SECRET.contains(&n.as_str()))
            .collect();
        assert!(
            unclassified.is_empty(),
            "classify each as redacted (extend visit_credentials) or not secret: {unclassified:?}",
        );
    }

    #[test]
    fn sanitize_uppercases_and_replaces_non_alphanumeric() {
        assert_eq!(sanitize("openai-prod"), "OPENAI_PROD");
        assert_eq!(sanitize("gpt-4o"), "GPT_4O");
        assert_eq!(sanitize("x.honeycomb/team"), "X_HONEYCOMB_TEAM");
    }

    #[test]
    fn provider_key_api_key_and_request_string_maps() {
        let found = slots(
            CredentialKind::ProviderKey,
            json!({
                "display_name": "pk",
                "api_key": "sk-1",
                "request": {
                    "default_headers": { "x-tenant-token": "tok-2", "x-empty": "" },
                    "default_body_fields": { "partner_token": "tok-3", "safe_prompt": true }
                }
            }),
        );
        assert_eq!(
            found,
            vec![
                ("DEFAULT_BODY_FIELDS_PARTNER_TOKEN".into(), "tok-3".into()),
                ("DEFAULT_HEADERS_X_TENANT_TOKEN".into(), "tok-2".into()),
                ("api_key".into(), "sk-1".into()),
            ],
        );
    }

    #[test]
    fn provider_key_body_fields_are_walked_at_any_depth() {
        let found = slots(
            CredentialKind::ProviderKey,
            json!({"request": {"default_body_fields": {
                "metadata": { "partner_token": "nested", "tier": 2 },
                "tags": ["in-array", true]
            }}}),
        );
        assert_eq!(
            found,
            vec![
                (
                    "DEFAULT_BODY_FIELDS_METADATA_PARTNER_TOKEN".into(),
                    "nested".into()
                ),
                ("DEFAULT_BODY_FIELDS_TAGS_0".into(), "in-array".into()),
            ],
        );
    }

    #[test]
    fn guardrail_secrets_at_any_depth_but_not_identifiers() {
        let found = slots(
            CredentialKind::Guardrail,
            json!({
                "name": "g",
                "api_key": "top",
                "aws_credentials": { "access_key_id": "AKIA", "secret_access_key": "nested" }
            }),
        );
        assert_eq!(
            found,
            vec![
                ("api_key".into(), "top".into()),
                ("secret_access_key".into(), "nested".into()),
            ],
        );
    }

    #[test]
    fn secret_and_header_kinds() {
        for kind in [CredentialKind::McpServer, CredentialKind::A2aAgent] {
            assert_eq!(
                slots(
                    kind,
                    json!({"name": "s", "secret": "s-1", "url": "https://x"})
                ),
                vec![("secret".into(), "s-1".into())],
            );
        }
        assert_eq!(
            slots(
                CredentialKind::ObservabilityExporter,
                json!({"endpoint": "https://x", "headers": {"x-honeycomb-team": "h-1"}}),
            ),
            vec![("HEADER_X_HONEYCOMB_TEAM".into(), "h-1".into())],
        );
    }

    #[test]
    fn redact_credentials_replaces_values_and_keeps_the_rest() {
        let mut doc = json!({
            "display_name": "pk",
            "api_key": "sk-1",
            "api_base": "https://api.example.com",
            "request": { "default_body_fields": { "partner_token": "tok", "safe_prompt": true } }
        });
        redact_credentials(CredentialKind::ProviderKey, &mut doc);
        assert_eq!(
            doc,
            json!({
                "display_name": "pk",
                "api_key": REDACTED,
                "api_base": "https://api.example.com",
                "request": { "default_body_fields": { "partner_token": REDACTED, "safe_prompt": true } }
            }),
        );
    }
}
