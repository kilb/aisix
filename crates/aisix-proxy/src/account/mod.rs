//! The account lookups: routes on which a caller reads its own account with
//! its API key, answered from the control plane.
//!
//! These are the one place the gateway asks the control plane something on a
//! caller's behalf while the caller waits. Model traffic never does, and never
//! shares anything with them: [`AccountClient`] holds a client and connection
//! pool of its own (built by the server from the gateway's mTLS bundle), every
//! request is bounded by [`ACCOUNT_TIMEOUT`], answers are cached for a few
//! seconds in caches bounded at [`CACHE_CAPACITY`] entries, and a control
//! plane that cannot answer fails only the lookup, with `503`
//! ([`crate::ProxyError::AccountUnavailable`]).
//!
//! Without a control plane (a gateway reading a resources file, or a managed
//! gateway with no certificate bundle) there is no client, and every lookup
//! answers `404`.
//!
//! Every answer decodes loosely: the control plane may be many releases newer
//! than this gateway, so no answer type denies unknown fields, and every field
//! but the one the lookup hinges on has a default. A route renders its own
//! client shape from what it decoded, so a field a newer control plane adds
//! reaches no caller until the gateway names it.

pub(crate) mod key;

use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::de::DeserializeOwned;

/// How long one lookup waits for the control plane, connecting included.
pub const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a key's quota answer is served from the cache.
pub const QUOTA_TTL: Duration = Duration::from_secs(3);
/// How long the control plane's "no such key" is served from the cache.
pub const NOT_FOUND_TTL: Duration = Duration::from_secs(1);
/// The most entries any one lookup cache holds.
pub const CACHE_CAPACITY: u64 = 10_000;

/// Why a lookup has no answer to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountError {
    /// The control plane timed out, could not be reached, failed, or answered
    /// with something that does not decode.
    Unavailable,
}

/// The gateway's client for the account lookups.
pub struct AccountClient {
    http: reqwest::Client,
    /// The control plane's origin, as configured, without a trailing slash.
    base: String,
    quotas: key::QuotaCache,
}

impl AccountClient {
    /// `http` must be a client of its own, never one model traffic or another
    /// control-plane channel uses; `cp_base_url` is the control plane's
    /// gateway listener.
    pub fn new(http: reqwest::Client, cp_base_url: &str) -> AccountClient {
        AccountClient {
            http,
            base: cp_base_url.trim_end_matches('/').to_string(),
            quotas: key::quota_cache(),
        }
    }

    /// `GET <control plane><path>?<query>`, decoded as `T`.
    ///
    /// `Ok(None)` is the control plane's `404`. Every other failure (a
    /// timeout, a connection that fails, any other status, a `2xx` that does
    /// not decode as `T`) is [`AccountError::Unavailable`]. Query values are
    /// encoded by the client; `path` must already be encoded.
    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Option<T>, AccountError> {
        let mut request = self
            .http
            .get(format!("{}{path}", self.base))
            .timeout(timeout);
        if !query.is_empty() {
            request = request.query(query);
        }
        // The URL is left out of every line logged here: a configured
        // control-plane URL may carry a password.
        let response = request.send().await.map_err(|error| {
            tracing::warn!(
                error = %aisix_gateway::transport_error_message(&error.without_url()),
                "account lookup: the control plane could not be reached",
            );
            AccountError::Unavailable
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            tracing::warn!(%status, "account lookup: the control plane answered with a failure");
            return Err(AccountError::Unavailable);
        }
        let body = response.bytes().await.map_err(|error| {
            aisix_gateway::log_body_read_causes(&error.without_url());
            AccountError::Unavailable
        })?;
        serde_json::from_slice(&body).map(Some).map_err(|error| {
            tracing::warn!(%error, "account lookup: the control plane's answer does not decode");
            AccountError::Unavailable
        })
    }
}

/// Characters a key id keeps when written into a URL path.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// The control plane's path for lookup `what` of key `key_id`.
pub(crate) fn key_path(key_id: &str, what: &str) -> String {
    format!(
        "/dp/v1/keys/{}/{what}",
        utf8_percent_encode(key_id, PATH_SEGMENT)
    )
}
