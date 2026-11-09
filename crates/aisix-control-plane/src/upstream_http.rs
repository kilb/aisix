//! The one outbound HTTP client the control plane has: the model list of an
//! upstream account (`GET /admin/provider-keys/{id}/upstream-models`).
//!
//! `api_base` is operator-set, so this is the first place the control plane
//! dials a URL a person supplied. `outbound.ssrf_protection` applies, through
//! the gateway's own [`SsrfProtection`]: the name is resolved here and pinned,
//! so a rebinding name cannot answer differently on a second lookup, and a
//! blocked or unresolvable host is refused before anything is dialled. The
//! environment proxy is not used: it would resolve the name a second time,
//! which this check cannot pin. Redirects are never followed — a `3xx` is
//! answered as a bad response, so the check cannot be bypassed by pointing a
//! redirect at an address it refused.
//!
//! The credential is used for one header, is noted for the error scrubber
//! before it is sent ([`response_tap::note_sent_credential`]), and never
//! reaches an answer, a log or an audit row.

use std::time::Duration;

use aisix_core::config::SsrfProtectionConfig;
use aisix_core::models::{Adapter, ProviderKey};
use aisix_gateway::response_tap::{self, ResponseHeaderTap};
use aisix_gateway::ssrf::{Refusal, Route, SsrfProtection};
use aisix_gateway::upstream_error_scrub::scrub_upstream_error_text;
use chrono::{DateTime, Utc};
use reqwest::Url;

use crate::error::ApiError;

/// The most of one upstream answer that is read. A larger body is refused
/// rather than buffered further.
pub const MAX_BODY_BYTES: usize = 1 << 20;

/// The most model ids one answer keeps. The rest are counted in `warnings`.
pub const MAX_IDS: usize = 5_000;

/// How long the whole call may take, and how long connecting may.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The most of a scrubbed detail an operator sees, counted after scrubbing:
/// truncate after, never before, or a cut could leave half a credential that
/// no longer matches.
const MAX_DETAIL_CHARS: usize = 500;

/// The Anthropic wire version the pinned constant names
/// (`aisix_provider_anthropic::ANTHROPIC_VERSION`).
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The model list one upstream account answered.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// The ids, deduplicated and sorted.
    pub ids: Vec<String>,
    /// Whether the answer held more ids than [`MAX_IDS`].
    pub truncated: bool,
    /// What an operator should know about the answer, in Chinese.
    pub warnings: Vec<String>,
    pub fetched_at: DateTime<Utc>,
}

/// The two upstream shapes a model list can be asked for in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Openai,
    Anthropic,
}

/// The OpenAI-compatible endpoints an operator may have pasted whole into
/// `api_base`; the bridges strip the same list
/// (`aisix-provider-openai/src/bridge.rs::OPENAI_ENDPOINT_SUFFIXES`).
const OPENAI_ENDPOINT_SUFFIXES: &[&str] = &[
    "/chat/completions",
    "/completions",
    "/embeddings",
    "/images/generations",
    "/audio/transcriptions",
    "/audio/translations",
    "/audio/speech",
];

/// The Anthropic endpoint suffixes, stripped the way its bridge does
/// (`aisix-provider-anthropic/src/bridge.rs::ANTHROPIC_ENDPOINT_SUFFIXES`).
const ANTHROPIC_ENDPOINT_SUFFIXES: &[&str] = &["/v1/messages", "/v1"];

/// The OpenAI-compatible vendor whose default base this module knows.
const OPENAI_VENDOR: &str = "openai";
/// The default base for the OpenAI form, which carries its version segment.
const OPENAI_DEFAULT_BASE: &str = "https://api.openai.com/v1";
/// The Anthropic form's default base: a bare host.
const ANTHROPIC_VENDOR: &str = "anthropic";
const ANTHROPIC_DEFAULT_BASE: &str = "https://api.anthropic.com";

/// The URLs this process may dial, and the clients they go out on. Built once
/// at boot.
pub struct Client {
    /// Whether `outbound.ssrf_protection` is on. Off, nothing is checked, and
    /// the unguarded client is the right one for every URL.
    guarded: bool,
    ssrf: SsrfProtection,
    /// Direct, through the resolver that refuses a non-public address and
    /// connects only to the addresses it checked.
    pinned: reqwest::Client,
    /// Direct with nothing to check: the target is allowed, an address
    /// literal that is public, or protection is off.
    direct: reqwest::Client,
}

impl Client {
    /// The client for `cfg`, which production takes from
    /// [`SsrfProtectionConfig::default`] — on, with no allowed hosts.
    pub fn new(cfg: &SsrfProtectionConfig) -> Result<Client, String> {
        let ssrf = SsrfProtection::from_config(cfg)?;
        let pinned = Self::builder()
            .dns_resolver(aisix_gateway::ssrf::direct_resolver(true))
            .build()
            .map_err(|e| format!("the pinned outbound client: {e}"))?;
        let direct = Self::builder()
            .dns_resolver(aisix_gateway::ssrf::direct_resolver(false))
            .build()
            .map_err(|e| format!("the direct outbound client: {e}"))?;
        Ok(Client {
            guarded: cfg.enabled,
            ssrf,
            pinned,
            direct,
        })
    }

    /// The settings every outbound request takes: no proxy, both timeouts,
    /// and no redirect followed.
    fn builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .no_proxy()
            .user_agent(format!("aisix/{}", &*aisix_core::BUILD_VERSION))
    }

    /// The client `url` goes out on, or why it may not be dialled.
    async fn route(&self, url: &Url) -> Result<&reqwest::Client, ApiError> {
        // Protection off checks nothing, so there is nothing for a guarded
        // resolver to enforce.
        if !self.guarded {
            return Ok(&self.direct);
        }
        match self.ssrf.route(url).await {
            // A public address literal, or a host the policy allows: both
            // already decided, and nothing is left to pin.
            Ok(Route::Direct) => Ok(&self.direct),
            // A name whose addresses were checked and pinned. `Proxied` is
            // the same case read through the environment's proxy matcher,
            // which never applies to these clients (`no_proxy`): the only
            // path that reaches here with `allowed_hosts` empty — which the
            // route's own policy always is — is a name checked as wholly
            // public, so the pin is what has to hold.
            Ok(Route::Pinned | Route::Proxied) => Ok(&self.pinned),
            Err(Refusal::Unresolvable) => {
                Err(ApiError::upstream_unreachable("该上游的域名无法解析"))
            }
            // The one English text in this route's 中文 answers, and
            // deliberately: the hint is the gateway's own `blocked_hint`,
            // which names the startup-config key an operator has to edit
            // (`outbound.ssrf_protection.allowed_hosts`). Restating it here
            // would be a second copy of the authority that drifts from it.
            Err(_) => Err(ApiError::upstream_unreachable(Refusal::blocked_hint(Some(
                url,
            )))),
        }
    }
}

/// The wire `pk` speaks, or `None` for a platform that cannot list models.
///
/// Read exactly as [`aisix_gateway::hub::upstream_protocol`] reads it: the
/// vendor decides first, because a key naming `openai` or `anthropic` speaks
/// that wire whatever `adapter` says, and `adapter` decides for every other
/// vendor. `adapter` is not a required field, so an older or hand-written
/// document that names only the vendor is a case this must serve.
fn form(pk: &ProviderKey) -> Option<Form> {
    match pk.provider.as_str() {
        OPENAI_VENDOR => Some(Form::Openai),
        ANTHROPIC_VENDOR => Some(Form::Anthropic),
        _ => match pk.adapter {
            Some(Adapter::Openai) => Some(Form::Openai),
            Some(Adapter::Anthropic) => Some(Form::Anthropic),
            Some(Adapter::Bedrock | Adapter::Vertex | Adapter::AzureOpenai) | None => None,
        },
    }
}

/// A `422 /api_base` naming the base as the field at fault.
fn bad_api_base(message: &str) -> ApiError {
    ApiError::validation(vec![crate::error::Detail {
        path: "/api_base".to_string(),
        message: message.to_string(),
    }])
}

/// The URL `pk`'s models are listed at, or why this upstream has none.
///
/// `api_base` is used as configured, userinfo included. With no `api_base`, a
/// vendor default applies only when the key's own vendor is that vendor, so a
/// vendor's credential never reaches another vendor's host. Every way the base
/// can be unusable is the operator's own configuration and answers
/// `422 /api_base`.
pub fn base_url(pk: &ProviderKey) -> Result<Url, ApiError> {
    let wire = form(pk).ok_or_else(ApiError::upstream_models_unsupported)?;
    let base = match pk
        .api_base
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        Some(base) => base.to_string(),
        None => default_base(wire, &pk.provider)?,
    };
    // The endpoint path is appended to this base, so a query or a fragment
    // would end up in the middle of the shaped URL. The bridges' own
    // `validate_api_base` refuses both for the same reason.
    if base.contains(['?', '#']) {
        return Err(bad_api_base("基础地址不能带查询参数或锚点"));
    }
    let shaped = match wire {
        Form::Openai => format!("{}/models", normalize_openai(&base)),
        Form::Anthropic => format!("{}/v1/models?limit=1000", normalize_anthropic(&base)),
    };
    let url = Url::parse(&shaped).map_err(|_| bad_api_base("基础地址不是一个合法的 URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad_api_base("基础地址必须使用 http 或 https"));
    }
    Ok(url)
}

/// The vendor default base for `form`, refused for any other vendor.
fn default_base(form: Form, provider: &str) -> Result<String, ApiError> {
    match (form, provider) {
        (Form::Openai, OPENAI_VENDOR) => Ok(OPENAI_DEFAULT_BASE.to_string()),
        (Form::Anthropic, ANTHROPIC_VENDOR) => Ok(ANTHROPIC_DEFAULT_BASE.to_string()),
        _ => Err(bad_api_base("该上游需要填写基础地址")),
    }
}

/// Strip a pasted endpoint suffix and the trailing slash, the way the
/// OpenAI-compatible bridge does, then give the canonical host its `/v1`.
fn normalize_openai(base: &str) -> String {
    let stripped = strip_suffixes(base, OPENAI_ENDPOINT_SUFFIXES);
    match stripped {
        "https://api.openai.com" | "http://api.openai.com" => format!("{stripped}/v1"),
        other => other.to_string(),
    }
}

/// Strip a pasted `/v1`, `/v1/messages` or trailing slash, the way the
/// Anthropic bridge does; the caller appends `/v1/models`.
fn normalize_anthropic(base: &str) -> String {
    strip_suffixes(base, ANTHROPIC_ENDPOINT_SUFFIXES).to_string()
}

fn strip_suffixes<'a>(base: &'a str, suffixes: &[&str]) -> &'a str {
    let trimmed = base.trim_end_matches('/');
    for suffix in suffixes {
        if let Some(rest) = trimmed.strip_suffix(suffix) {
            return rest.trim_end_matches('/');
        }
    }
    trimmed
}

/// Ask `pk`'s upstream for its own model list.
///
/// The credential is noted for the scrubber and the whole call runs inside a
/// `response_tap` scope, so every text that reaches an error has passed
/// through the one scrubber.
pub async fn fetch_models(
    client: &Client,
    pk: &ProviderKey,
    api_key: &str,
) -> Result<Fetched, ApiError> {
    let wire = form(pk).ok_or_else(ApiError::upstream_models_unsupported)?;
    let url = base_url(pk)?;
    let tap = std::sync::Arc::new(ResponseHeaderTap::new(
        Vec::new(),
        false,
        uuid::Uuid::new_v4().to_string(),
    ));
    response_tap::scope(tap, async move {
        // Before the request goes out: an upstream that quotes the key back
        // in its error must have it taken out of the relayed text.
        response_tap::note_sent_credential(api_key);
        // A password in `api_base`'s userinfo rides the request too.
        response_tap::note_url_userinfo(url.as_str());
        let target = client.route(&url).await?;
        let request = match wire {
            Form::Openai => target.get(url.clone()).bearer_auth(api_key),
            Form::Anthropic => target
                .get(url.clone())
                .header("x-api-key", api_key)
                .bearer_auth(api_key)
                .header("anthropic-version", ANTHROPIC_VERSION),
        };
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Err(transport_failure(&error, &url)),
        };
        // The answer's own status is read here and nowhere else: a non-2xx
        // becomes the fixed `502`, so no status the upstream chose reaches
        // the caller. Its words do, scrubbed.
        if !response.status().is_success() {
            let detail = match read_body(response).await {
                Ok(body) => scrub_upstream_error_text(String::from_utf8_lossy(&body).trim()),
                // A body too large to read carries no text to relay; say why
                // rather than leave the detail empty.
                Err(error) => error
                    .details
                    .first()
                    .map(|detail| detail.message.clone())
                    .unwrap_or(error.message),
            };
            return Err(ApiError::upstream_bad_response(detail_capped(detail)));
        }
        let body = read_body(response).await?;
        parse(wire, &body)
    })
    .await
}

/// `text` cut to [`MAX_DETAIL_CHARS`] characters, by its caller after every
/// scrub has run: a cut before one could leave half a credential that no
/// longer matches, and a fit before one could let a scrub be skipped.
fn detail_capped(text: String) -> String {
    text.chars().take(MAX_DETAIL_CHARS).collect()
}

/// Read a response body to [`MAX_BODY_BYTES`], refusing a larger one rather
/// than buffering further.
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, ApiError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ApiError::upstream_bad_response(format!("读取该上游的回答失败：{}", transport_text(&error)))
    })? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(ApiError::upstream_bad_response(format!(
                "该上游的回答超过 {} KiB",
                MAX_BODY_BYTES / 1024
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `data[].id` as strings, deduplicated, sorted and capped.
///
/// The answer is complete only when the [`MAX_IDS`] cap was not passed **and**
/// the upstream did not say it had more: the Anthropic wire asks for one page
/// of 1 000 and does not page, so `has_more` is the only signal that the list
/// was cut. Either way the truncation is stated in `warnings`, never hidden.
fn parse(wire: Form, body: &[u8]) -> Result<Fetched, ApiError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::upstream_bad_response("该上游的回答不是有效的 JSON".to_string()))?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            ApiError::upstream_bad_response("该上游的回答不是有效的模型列表".to_string())
        })?;
    let mut ids: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut capped = false;
    for entry in data {
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        if ids.len() == MAX_IDS {
            capped = true;
            break;
        }
        ids.push(id.to_string());
    }
    ids.sort_unstable();
    // A page the upstream flagged, and a page this side cut, are both "there
    // are more". `last_id` rides along so an operator can say which model the
    // list stopped after.
    let more = wire == Form::Anthropic
        && value
            .get("has_more")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    let mut warnings = Vec::new();
    if capped {
        warnings.push(format!(
            "该上游的模型超过 {MAX_IDS} 个，只列出前 {MAX_IDS} 个，仍可直接填写名称"
        ));
    } else if more {
        let after = value
            .get("last_id")
            .and_then(serde_json::Value::as_str)
            .map(|id| format!("，最后一个是 {id}"))
            .unwrap_or_default();
        warnings.push(format!(
            "该上游的模型不止这一页，只列出前 {} 个{after}，仍可直接填写名称",
            ids.len()
        ));
    }
    Ok(Fetched {
        ids,
        truncated: capped || more,
        warnings,
        fetched_at: Utc::now(),
    })
}

/// A failed request, as one of the route's four answers.
fn transport_failure(error: &reqwest::Error, url: &Url) -> ApiError {
    if error.is_timeout() {
        return ApiError::upstream_timeout();
    }
    match Refusal::from_error(error) {
        Some(Refusal::Blocked(_)) => {
            ApiError::upstream_unreachable(Refusal::blocked_hint(Some(url)))
        }
        Some(Refusal::Unresolvable) => ApiError::upstream_unreachable("该上游的域名无法解析"),
        _ => ApiError::upstream_unreachable(format!("无法连接该上游：{}", transport_text(error))),
    }
}

/// reqwest's error as one line, its URL reduced to the origin: it already
/// does exactly that, so a credential in a userinfo never reaches the caller.
fn transport_text(error: &reqwest::Error) -> String {
    aisix_gateway::transport_error_message(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body of `fn <signature>` in this file, without its comment
    /// lines: an explanation survives deleting the call it explains, and a
    /// guard satisfied by its own rationale guards nothing.
    fn body_of(signature: &str) -> String {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/upstream_http.rs"))
                .expect("this module is readable");
        src.split_once(signature)
            .unwrap_or_else(|| panic!("`{signature}` is declared here"))
            .1
            .split_once("\n    }")
            .expect("closed")
            .0
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What the gateway's shared client guarantees, held here by name: the
    /// control plane builds its client bare (the gateway's census exempts
    /// that one line for it), so nothing else would notice a setting lost.
    ///
    /// - No proxy: a proxy resolves the name again, which the SSRF pin cannot
    ///   cover.
    /// - Both timeouts, the route's own bounds.
    /// - No redirect followed: a `3xx` cannot steer the call to an address
    ///   the check refused, nor carry the account's credential there.
    /// - The `aisix/<version>` identity every gateway client sends.
    ///
    /// Source-level, as the gateway's own: `reqwest::Client` exposes none of
    /// these for readback.
    #[test]
    fn the_outbound_client_takes_no_proxy_both_timeouts_no_redirect_and_the_identity() {
        let builder = body_of("fn builder() -> reqwest::ClientBuilder {");
        for setting in [
            ".redirect(reqwest::redirect::Policy::none())",
            ".no_proxy()",
            ".connect_timeout(CONNECT_TIMEOUT)",
            ".timeout(TOTAL_TIMEOUT)",
            ".user_agent(format!(\"aisix/{}\", &*aisix_core::BUILD_VERSION))",
        ] {
            assert!(
                builder.contains(setting),
                "the control plane's outbound client does not set `{setting}`:\n{builder}"
            );
        }
        // Set once, so a later call cannot override what is pinned above,
        // and no proxy configured beside `no_proxy`.
        for once in [
            ".redirect(",
            ".timeout(",
            ".connect_timeout(",
            ".user_agent(",
        ] {
            assert_eq!(
                builder.matches(once).count(),
                1,
                "`{once}` is set once:\n{builder}"
            );
        }
        assert!(
            !builder.contains(".proxy("),
            "no proxy is configured:\n{builder}"
        );
        // Both clients the route dials through start from that builder.
        let new = body_of("pub fn new(cfg: &SsrfProtectionConfig) -> Result<Client, String> {");
        assert_eq!(
            new.matches("Self::builder()").count(),
            2,
            "`pinned` and `direct` are each built from `Self::builder()`:\n{new}"
        );
        assert!(
            !new.contains("reqwest::Client::"),
            "a client built beside `Self::builder()` skips its settings:\n{new}"
        );
        for setting in [
            ".redirect(",
            ".timeout(",
            ".connect_timeout(",
            ".user_agent(",
            ".proxy(",
        ] {
            assert!(
                !new.contains(setting),
                "`{setting}` set after `Self::builder()` overrides it:\n{new}"
            );
        }
    }

    fn key(adapter: Option<Adapter>, provider: &str, api_base: Option<&str>) -> ProviderKey {
        let mut doc = serde_json::json!({
            "display_name": "acct",
            "api_key": "sk-test",
            "provider": provider,
        });
        if let Some(adapter) = adapter {
            doc["adapter"] = serde_json::to_value(adapter).expect("an adapter");
        }
        if let Some(base) = api_base {
            doc["api_base"] = serde_json::json!(base);
        }
        serde_json::from_value(doc).expect("a key")
    }

    fn url(pk: &ProviderKey) -> String {
        base_url(pk).expect("a URL").to_string()
    }

    #[test]
    fn the_openai_form_keeps_its_version_segment_and_drops_a_pasted_endpoint() {
        assert_eq!(
            url(&key(
                Some(Adapter::Openai),
                "openai",
                Some("https://api.openai.com/v1")
            )),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            url(&key(
                Some(Adapter::Openai),
                "openai",
                Some("https://api.openai.com/v1/chat/completions")
            )),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            url(&key(
                Some(Adapter::Openai),
                "openai",
                Some("https://api.openai.com")
            )),
            "https://api.openai.com/v1/models",
            "the bare canonical host takes its version segment back"
        );
        assert_eq!(
            url(&key(
                Some(Adapter::Openai),
                "deepseek",
                Some("https://api.deepseek.com/v1/")
            )),
            "https://api.deepseek.com/v1/models",
            "another vendor's path is trusted as written"
        );
        assert_eq!(
            url(&key(Some(Adapter::Openai), "openai", None)),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn the_anthropic_form_strips_every_pasted_shape() {
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
            "https://api.anthropic.com/v1/messages",
        ] {
            assert_eq!(
                url(&key(Some(Adapter::Anthropic), "anthropic", Some(base))),
                "https://api.anthropic.com/v1/models?limit=1000",
                "{base}"
            );
        }
        assert_eq!(
            url(&key(Some(Adapter::Anthropic), "anthropic", None)),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
    }

    #[test]
    fn userinfo_rides_the_url_as_configured() {
        let shaped = url(&key(
            Some(Adapter::Openai),
            "openai",
            Some("https://user:p%40ss@proxy.example.com/v1"),
        ));
        assert_eq!(shaped, "https://user:p%40ss@proxy.example.com/v1/models");
    }

    #[test]
    fn the_wire_comes_from_the_vendor_first_and_the_adapter_second() {
        // Exactly `upstream_protocol`'s order: a key naming the vendor
        // speaks that wire whatever `adapter` says, which is the case a
        // hand-written or older document is in, since `adapter` is optional.
        for adapter in [None, Some(Adapter::Openai)] {
            let pk = key(adapter, "openai", None);
            assert_eq!(form(&pk), Some(Form::Openai), "{adapter:?}");
            assert_eq!(
                url(&pk),
                "https://api.openai.com/v1/models",
                "no adapter, the vendor decides: {adapter:?}"
            );
        }
        for adapter in [None, Some(Adapter::Anthropic)] {
            let pk = key(adapter, "anthropic", None);
            assert_eq!(form(&pk), Some(Form::Anthropic), "{adapter:?}");
            assert_eq!(
                url(&pk),
                "https://api.anthropic.com/v1/models?limit=1000",
                "no adapter, the vendor decides: {adapter:?}"
            );
        }
        // The vendor wins over a disagreeing adapter, as the gateway's own
        // dispatch does.
        assert_eq!(
            form(&key(Some(Adapter::Bedrock), "anthropic", None)),
            Some(Form::Anthropic)
        );
        // Any other vendor takes its adapter, and only those two adapters
        // have a list at all.
        assert_eq!(
            form(&key(Some(Adapter::Openai), "deepseek", None)),
            Some(Form::Openai)
        );
        assert_eq!(
            form(&key(Some(Adapter::Anthropic), "deepseek", None)),
            Some(Form::Anthropic)
        );
        for adapter in [
            None,
            Some(Adapter::Bedrock),
            Some(Adapter::Vertex),
            Some(Adapter::AzureOpenai),
        ] {
            assert_eq!(form(&key(adapter, "aws", None)), None, "{adapter:?}");
        }
    }

    #[test]
    fn an_unsupported_adapter_has_no_url_and_a_mismatched_vendor_has_no_default() {
        for adapter in [
            Some(Adapter::Bedrock),
            Some(Adapter::Vertex),
            Some(Adapter::AzureOpenai),
        ] {
            let error = base_url(&key(adapter, "aws", Some("https://x.example.com/v1")))
                .expect_err("no model list");
            assert_eq!(error.code, "upstream_models_unsupported", "{adapter:?}");
            assert_eq!(
                error.message, "该类型的上游不提供模型列表，请直接填写模型名称",
                "{adapter:?}"
            );
        }
        // No vendor, no adapter: nothing to guess from.
        let error = base_url(&key(None, "aws", Some("https://x.example.com/v1")))
            .expect_err("no model list");
        assert_eq!(error.code, "upstream_models_unsupported");

        // No base, and a vendor that is not the form's: the credential must
        // not reach another vendor's host. An adapter decides the wire for a
        // vendor it alone names, and still gets no default base from it.
        for (adapter, provider) in [
            (Some(Adapter::Openai), "deepseek"),
            (Some(Adapter::Anthropic), "deepseek"),
        ] {
            let error = base_url(&key(adapter, provider, None)).expect_err("no default base");
            assert_eq!(error.code, "validation_failed", "{adapter:?} {provider}");
            assert_eq!(error.details[0].path, "/api_base");
            assert_eq!(error.details[0].message, "该上游需要填写基础地址");
        }
        // The vendor party to the wire wins, so its own default applies
        // whatever a disagreeing adapter says.
        assert_eq!(
            url(&key(Some(Adapter::Bedrock), "openai", None)),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn a_base_that_is_not_usable_is_the_fields_fault() {
        for base in [
            "file:///etc/passwd",
            "not a url",
            "https://host/v1?x=1",
            "https://host#a",
        ] {
            let error = base_url(&key(Some(Adapter::Openai), "openai", Some(base)))
                .expect_err("not usable");
            assert_eq!(error.code, "validation_failed", "{base}");
            assert_eq!(error.details[0].path, "/api_base", "{base}");
        }
    }

    #[test]
    fn ids_are_deduped_sorted_and_capped() {
        let fetched = parse(
            Form::Openai,
            br#"{"object":"list","data":[{"id":"b"},{"id":"a"},{"id":"a"},{"nope":1}]}"#,
        )
        .expect("a list");
        assert_eq!(fetched.ids, ["a", "b"]);
        assert!(!fetched.truncated);
        assert!(fetched.warnings.is_empty());

        let many: Vec<String> = (0..MAX_IDS + 5)
            .map(|i| format!(r#"{{"id":"m{i:05}"}}"#))
            .collect();
        let body = format!(r#"{{"data":[{}]}}"#, many.join(","));
        let fetched = parse(Form::Openai, body.as_bytes()).expect("a long list");
        assert_eq!(fetched.ids.len(), MAX_IDS);
        assert!(fetched.truncated);
        assert_eq!(fetched.warnings.len(), 1);

        for bad in [&b"not json"[..], br#"{"nope":1}"#, br#"{"data":1}"#] {
            let error = parse(Form::Openai, bad).expect_err("not a list");
            assert_eq!(error.code, "upstream_bad_response");
        }
    }

    /// The Anthropic wire asks for one page and does not page, so `has_more`
    /// is the only sign that the list was cut — and a cut list reported as
    /// complete is what the design forbids.
    #[test]
    fn an_anthropic_page_that_has_more_is_truncated() {
        let page = br#"{"data":[{"id":"c-1"}],"has_more":true,"first_id":"c-1","last_id":"c-1"}"#;
        let fetched = parse(Form::Anthropic, page).expect("a page");
        assert!(fetched.truncated, "has_more was ignored");
        assert_eq!(fetched.warnings.len(), 1);
        assert!(
            fetched.warnings[0].contains("c-1"),
            "{:?}",
            fetched.warnings
        );

        let whole = br#"{"data":[{"id":"c-1"}],"has_more":false}"#;
        let fetched = parse(Form::Anthropic, whole).expect("a page");
        assert!(!fetched.truncated);
        assert!(fetched.warnings.is_empty());

        // The OpenAI wire documents no pagination, so its `has_more` is not
        // read: a stray field there must not invent a truncation.
        let openai =
            parse(Form::Openai, br#"{"data":[{"id":"a"}],"has_more":true}"#).expect("a list");
        assert!(!openai.truncated);
    }

    #[test]
    fn a_scrubbed_detail_is_cut_after_scrubbing_and_quotes_nothing_secret() {
        let long = "x".repeat(2_000);
        let detail = detail_capped(scrub_upstream_error_text(&format!(
            "bad key sk-abcdefghijklmnopqrstuvwxyz {long}"
        )));
        assert!(!detail.contains("sk-"), "{detail}");
        assert_eq!(detail.chars().count(), MAX_DETAIL_CHARS);

        // The cut runs after the scrub: a 500-character prefix of a longer
        // body is still scrubbed, and what the cut leaves is whole.
        let secret = format!("sk-{}", "a".repeat(40));
        let text = detail_capped(scrub_upstream_error_text(&format!(
            "{secret} {}",
            "z".repeat(4_000)
        )));
        assert!(!text[..].contains(&secret), "{text}");
    }
}
