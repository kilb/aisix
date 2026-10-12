//! Read responses for the resource kinds that store credentials
//! (provider keys, guardrails, MCP servers, A2A agents, observability
//! exporters): every credential the kind stores is returned as
//! [`aisix_core::credentials::REDACTED`]. Which fields those are is
//! defined once, in [`aisix_core::credentials`], and shared with
//! `aisix export`.

use aisix_core::credentials::{redact_credentials, CredentialKind};
use aisix_core::resource::ResourceEntry;
use serde::Serialize;
use serde_json::Value;

use crate::error::AdminError;

/// `entry` as a read response, credentials redacted.
pub(crate) fn redacted<T: Serialize>(
    entry: ResourceEntry<T>,
    kind: CredentialKind,
) -> Result<ResourceEntry<Value>, AdminError> {
    let mut value = serde_json::to_value(&*entry.value)
        .map_err(|e| AdminError::Store(format!("serialize {}: {e}", entry.id)))?;
    redact_credentials(kind, &mut value);
    Ok(ResourceEntry::new(entry.id, value, entry.revision))
}

/// Every entry of a list response, credentials redacted.
pub(crate) fn redacted_all<T: Serialize>(
    entries: Vec<ResourceEntry<T>>,
    kind: CredentialKind,
) -> Result<Vec<ResourceEntry<Value>>, AdminError> {
    entries.into_iter().map(|e| redacted(e, kind)).collect()
}
