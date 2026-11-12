//! `AzureOpenAiBridge` — family Bridge for [`Adapter::AzureOpenai`].
//!
//! Wire shape is OpenAI chat-completions (parsers reused from
//! `aisix-provider-openai::wire`). Azure differs on three axes:
//!
//! 1. **URL pattern** — deployment-keyed:
//!    `https://<resource>.openai.azure.com/openai/deployments/<deployment>/chat/completions?api-version=<version>`
//! 2. **Auth header** — `api-key: <secret>` (NOT `Authorization: Bearer`)
//! 3. **Response extension** — Azure adds `prompt_filter_results` /
//!    `content_filter_results` blocks; the reused OpenAI parsers
//!    tolerate them via serde's default-deny-on-known behavior.
//!
//! Override apply pipeline (request body + headers) mirrors
//! `OpenAiBridge` and reuses the helpers from
//! `aisix_provider_openai::overrides`.

use aisix_core::{RequestOverrides, ResponseOverrides, StreamDoneMarker};
use aisix_gateway::{
    apply_request_headers, Bridge, BridgeContext, BridgeError, ChatChunk, ChatChunkStream,
    ChatFormat, ChatResponse, EmbeddingRequest, EmbeddingResponse, SseDecoder, SseEvent,
    UpstreamHeaderContext,
};
use async_trait::async_trait;
use futures::StreamExt;
use http::{
    header::{HeaderName, HeaderValue},
    HeaderMap,
};
use reqwest::{header, Client, StatusCode};
use serde_json::Value;
use std::time::{Duration, Instant};

use aisix_provider_openai::close_strict_response_format_schema;
use aisix_provider_openai::overrides::{
    apply_content_list_to_string, apply_default_body_fields, apply_param_constraints,
    apply_param_renames, apply_stream_done_marker_policy, extract_reasoning_field,
    validate_content_safe_request_overrides, StreamDoneOutcome,
};
use aisix_provider_openai::reasoning::{is_reasoning_model, ReasoningFamily};
use aisix_provider_openai::wire::{
    build_request, embed_request_body, embed_response_into, messages_from,
    response_into_chat_response, stream_chunk_into_chat_chunks, DeveloperRoleMode,
    OpenAiEmbedResponse, OpenAiResponse, OpenAiStreamChunk, ToolCallIndexer,
};

use crate::aad_token_mint::TokenMinter;
use crate::wire;

use std::sync::Arc;

/// The bridge pins Azure API version `2024-10-21`, whose request-message
/// union does not define `developer`. Azure documents developer instructions
/// as functionally equivalent to system instructions, so both chat paths map
/// them to the supported `system` wire role.
const AZURE_DEVELOPER_ROLE_MODE: DeveloperRoleMode = DeveloperRoleMode::MapToSystem;

/// Family Bridge for Azure OpenAI Service.
pub struct AzureOpenAiBridge {
    client: Client,
    /// Static `name()` returned to the Hub. Distinct from `"openai"`
    /// so dashboards can split Azure traffic from canonical OpenAI
    /// traffic in metrics.
    name: &'static str,
    /// In-process AAD token cache + minter. Used only when the
    /// inbound `ProviderKey.api_key` parses to the AAD branch; the
    /// api-key branch bypasses this entirely. `Arc` so the bridge
    /// remains cheaply clonable for callers that share it across
    /// Hub registrations.
    token_minter: Arc<TokenMinter>,
    /// Test-only POST URL override. When set, [`Bridge::chat`] /
    /// [`Bridge::chat_stream`] still run resolve / validation /
    /// header / body building against the real `AzureUpstreamRef`,
    /// but the final HTTP request goes to this URL instead of
    /// `<resource>.openai.azure.com`. Lets wiremock cover the
    /// full bridge entry point (not only the helper sub-fns).
    #[cfg(test)]
    url_override: Option<String>,
}

impl AzureOpenAiBridge {
    /// Construct an Azure OpenAI bridge with the canonical name
    /// `"azure-openai"`. The Hub looks this up via [`Bridge::name`]
    /// when emitting per-request metrics (provider label).
    pub fn new() -> Self {
        Self::with_client(default_client())
    }

    /// Construct an Azure OpenAI bridge with a caller-supplied
    /// [`reqwest::Client`]. Useful when downstream callers want to
    /// share a connection pool with other bridges or pin custom
    /// timeouts. Public surface — not test-only.
    pub fn with_client(client: Client) -> Self {
        Self::with_clients(client.clone(), client)
    }

    /// 分别指定模型连接和凭据交换连接；凭据端点不继承模型的 TLS 或地址覆盖。
    pub fn with_clients(client: Client, credential_client: Client) -> Self {
        Self {
            token_minter: Arc::new(TokenMinter::new(credential_client)),
            client,
            name: "azure-openai",
            #[cfg(test)]
            url_override: None,
        }
    }

    /// 返回这次调用可能连接的模型和凭据地址，供调用方使用同一出站策略预检。
    pub fn outbound_urls(
        &self,
        pk: &aisix_core::ProviderKey,
        deployment: &str,
        embedding: bool,
    ) -> Result<(String, Option<String>), BridgeError> {
        let upstream = AzureUpstreamRef::resolve(deployment, pk.api_base.as_deref())?;
        let model = if embedding {
            upstream.embeddings_url()
        } else {
            upstream.chat_completions_url()
        };
        let credential = match AzureSecret::parse(&pk.api_key)? {
            AzureSecret::ApiKey(_) => None,
            AzureSecret::Aad(creds) => Some(self.token_minter.resolve_token_endpoint(&creds)),
        };
        Ok((model, credential))
    }

    /// The client this dispatch runs on: the bridge's shared one, unless
    /// the resolved Provider Key carries its own connection overrides
    /// (TLS trust settings, a name-resolution address). The token
    /// minter deliberately keeps the shared client — it talks to the
    /// identity provider, not to the key's `api_base`, and an override
    /// declared for the model endpoint says nothing about that host.
    fn client_for(&self, ctx: &BridgeContext) -> Client {
        aisix_gateway::upstream_tls::client_for_provider_key(
            &self.client,
            ctx.provider_key.upstream_connection().as_ref(),
        )
    }

    /// Test-only seam: replace the AAD token endpoint host on the
    /// internal minter (without touching the chat-completions URL
    /// override on `url_override`). Used by AAD-flow tests so
    /// `client_credentials` POSTs land on a wiremock instance.
    #[cfg(test)]
    pub(crate) fn with_aad_token_endpoint_override(mut self, url: impl Into<String>) -> Self {
        self.token_minter =
            Arc::new(TokenMinter::new(self.client.clone()).with_token_endpoint_override(url));
        self
    }

    /// Resolve the URL the bridge will POST to. Returns
    /// `upstream.chat_completions_url()` in production; tests can
    /// override via [`Self::with_url_override`].
    ///
    /// Cached per (Provider Key, deployment): the URL embeds the
    /// deployment name, so a key fronting several deployments needs a row
    /// each — keying only by Provider Key would make every deployment
    /// invalidate the previous one's row and rebuild on every request.
    /// The fingerprint is the raw `api_base`, the one input
    /// `AzureUpstreamRef::resolve` derives the rest from; the api-version
    /// is a pinned constant.
    fn resolve_url(
        &self,
        ctx: &BridgeContext,
        upstream: &AzureUpstreamRef,
    ) -> aisix_gateway::url_cache::EndpointUrl {
        self.resolve_route_url(ctx, upstream, "azure/chat", || {
            upstream.chat_completions_url()
        })
    }

    /// Cached URL for one Azure route on one deployment. The cache key
    /// carries the route, so the chat and embeddings URLs for the same
    /// deployment cannot overwrite each other.
    fn resolve_route_url(
        &self,
        ctx: &BridgeContext,
        upstream: &AzureUpstreamRef,
        cache_key: &'static str,
        build: impl FnOnce() -> String,
    ) -> aisix_gateway::url_cache::EndpointUrl {
        #[cfg(test)]
        if let Some(u) = &self.url_override {
            // Uncached, and handed over as a raw string so reqwest
            // parses it exactly as it always did: the override exists so
            // wiremock can stand in for a canonical Azure host, and each
            // test server is a different port under one `api_base`.
            return aisix_gateway::url_cache::EndpointUrl::Unparsed(u.clone());
        }
        aisix_gateway::url_cache::cached_model_endpoint_url(
            &ctx.provider_key_id,
            cache_key,
            &upstream.deployment,
            &[ctx.provider_key.api_base.as_deref().unwrap_or("")],
            || Ok::<_, std::convert::Infallible>(build()),
        )
        .unwrap_or_else(|never| match never {})
    }

    /// Test-only seam: rewrite the POST URL so wiremock can stand
    /// in for `<resource>.openai.azure.com`. Header / body / resolve
    /// / validation paths still run normally against the canonical
    /// api_base configured on the ProviderKey.
    #[cfg(test)]
    pub(crate) fn with_url_override(mut self, url: impl Into<String>) -> Self {
        self.url_override = Some(url.into());
        self
    }

    /// Resolve the per-request auth pair from the provider key's
    /// secret. Returns either an `api_key` (verbatim resource-key)
    /// or a `bearer_token` (freshly minted-or-cached AAD access
    /// token). Token-mint failures (e.g. AAD 401 invalid_client)
    /// surface as `Err` here so `chat()` / `chat_stream()` short-
    /// circuit BEFORE attempting the upstream Azure OpenAI call. The
    /// mint runs under the request's deadline counted from `started`, the
    /// attempt's start: waiting on a stalled token endpoint must not outlast
    /// it, the upstream call gets only what the mint left, and the timeout is
    /// what lets the request fail over.
    async fn resolve_auth(
        &self,
        ctx: &BridgeContext,
        started: Instant,
    ) -> Result<AzureAuth, BridgeError> {
        match AzureSecret::parse(&ctx.provider_key.api_key)? {
            AzureSecret::ApiKey(key) => Ok(AzureAuth {
                api_key: Some(key),
                bearer_token: None,
            }),
            AzureSecret::Aad(creds) => {
                let token =
                    with_deadline(ctx.deadline, started, self.token_minter.get_token(&creds))
                        .await?;
                Ok(AzureAuth {
                    api_key: None,
                    bearer_token: Some(token),
                })
            }
        }
    }
}

impl Default for AzureOpenAiBridge {
    fn default() -> Self {
        Self::new()
    }
}

fn default_client() -> Client {
    aisix_gateway::build_client_or_default(aisix_gateway::client_builder(), "azure-openai bridge")
}

/// Parsed Azure upstream reference resolved from a provider_key's
/// `api_base` + the request's upstream model id.
///
/// Azure's chat-completions URL pattern (per
/// <https://learn.microsoft.com/en-us/azure/ai-services/openai/reference>):
///
/// ```text
/// https://<resource>.openai.azure.com/openai/deployments/<deployment>/chat/completions?api-version=<version>
/// ```
///
/// - `resource` — the Azure resource name, e.g. `acme-prod-west-us`
/// - `deployment` — operator-named deployment, e.g. `gpt4o-prod`
/// - `api_version` — Azure's date-stamped API version, e.g. `2024-10-21`
///
/// The resolver is intentionally cautious — any missing piece produces
/// a clear `BridgeError::Config` so an operator can fix the
/// registration before traffic ever hits Azure.
/// **Construction**: marked `#[non_exhaustive]` so future field
/// additions don't break downstream crates. External callers should
/// always go through [`Self::resolve`]; in-crate tests can use
/// `..Default::default()`-style struct-literal completion (no
/// `Default` impl is offered yet, but the `Debug` impl lets test
/// failures dump the shape).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AzureUpstreamRef {
    pub resource: String,
    pub deployment: String,
    pub api_version: String,
    /// Operator-supplied verbatim upstream base URL, used when
    /// `ProviderKey.api_base` does NOT match the canonical
    /// `<resource>.openai.azure.com` host shape. Set for corporate
    /// proxies, private-VPC endpoints, mock services. When `Some(_)`,
    /// `resource` is empty and `chat_completions_url()` builds the
    /// URL off this override instead of `<resource>.openai.azure.com`.
    ///
    /// Fixes api7/ai-gateway#391 — pre-fix builds rejected any
    /// `api_base` whose host didn't end in `.openai.azure.com`
    /// with a `Config` error, blocking BYO override deployments.
    ///
    /// **Typo caveat**: this override mode accepts any HTTP/HTTPS
    /// URL. A typo like `https://acme.openai.azur.com` (missing
    /// `e`) no longer errors at resolve time — it silently activates
    /// override mode and routes traffic to wherever that domain
    /// resolves. Operators are responsible for double-checking the
    /// URL. Same risk model as the OpenAI / Anthropic / Bedrock
    /// bridges, which already accept arbitrary operator-supplied
    /// `api_base`. The trade-off is intentional: enabling corporate-
    /// proxy / private-VPC / mock deployments requires accepting
    /// arbitrary hosts.
    ///
    /// **Path preservation**: an `api_base` with an internal path
    /// segment (e.g. `https://corp-proxy/azure-passthrough`) is
    /// preserved verbatim. The bridge appends `/openai/deployments/<d>/...`
    /// to whatever the operator supplied, including any internal
    /// prefix.
    ///
    /// **Userinfo**: `https://user:pass@host` is accepted and used as
    /// configured. The value is therefore never displayed as it stands —
    /// the `Debug` impl below reduces it to its origin.
    pub upstream_override: Option<String>,
}

// Hand-written so a password in the override URL cannot reach a log through
// `{:?}`.
impl std::fmt::Debug for AzureUpstreamRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureUpstreamRef")
            .field("resource", &self.resource)
            .field("deployment", &self.deployment)
            .field("api_version", &self.api_version)
            .field(
                "upstream_override",
                &self
                    .upstream_override
                    .as_deref()
                    .map(aisix_gateway::redact_url_for_log),
            )
            .finish()
    }
}

impl AzureUpstreamRef {
    /// The GA REST API version every request is pinned to: `api_base` cannot
    /// carry a query, so nothing overrides it. Azure deprecates older versions
    /// on a published schedule:
    /// <https://learn.microsoft.com/en-us/azure/ai-services/openai/api-version-deprecation>.
    ///
    /// Pinned at a GA shape (`YYYY-MM-DD`, no `-preview` suffix) so a
    /// future bump can't silently re-introduce a preview default.
    pub const DEFAULT_API_VERSION: &'static str = "2024-10-21";

    /// Resolve from the deployment name + an optional pre-parsed
    /// `api_base`.
    ///
    /// Both `deployment` and the resolved `resource` are validated to
    /// match a strict `[A-Za-z0-9_-]+` shape: Azure resource names
    /// and deployment names are constrained to that set per the
    /// portal, and a URL-injection vector via `?`, `#`, `/`, or
    /// whitespace would let an operator-supplied default redirect
    /// the dispatch to an attacker-pinned API version.
    pub fn resolve(deployment: &str, api_base: Option<&str>) -> Result<Self, BridgeError> {
        validate_url_token("deployment name", deployment)?;

        let base = api_base.unwrap_or_default().trim();
        if base.is_empty() {
            return Err(BridgeError::InvalidUpstreamConfig(
                "azure provider_key has no api_base — \
                 expected https://<resource>.openai.azure.com, a bare resource name, \
                 or a verbatim override URL (https://<host>[:<port>])"
                    .into(),
            ));
        }

        if let Some(rest) = base
            .strip_prefix("https://")
            .or_else(|| base.strip_prefix("http://"))
        {
            // Canonical form: split off the leading host segment
            // before the first `.`. If the remainder of the host is
            // `openai.azure.com` and the segment is a resource name,
            // extract the resource as today. Otherwise (another host, or
            // a URL whose authority carries more than a resource name)
            // fall through to the verbatim-override path.
            if let Some((host_resource, host_tail)) = rest.split_once('.') {
                let host_tail_trimmed = host_tail.trim_end_matches('/');
                let host_tail_core = host_tail_trimmed
                    .split_once('/')
                    .map(|(host, _path)| host)
                    .unwrap_or(host_tail_trimmed);
                if host_tail_core == "openai.azure.com"
                    && validate_url_token("resource name", host_resource).is_ok()
                {
                    return Ok(Self {
                        resource: host_resource.to_string(),
                        deployment: deployment.to_string(),
                        api_version: Self::DEFAULT_API_VERSION.to_string(),
                        upstream_override: None,
                    });
                }
            }

            // Verbatim-override branch — corporate proxy / private
            // endpoint / mock service. Defence-in-depth checks mirror
            // the Vertex sibling fix (#390): reject query and fragment
            // because each opens an injection / api-version-downgrade
            // vector. Scheme is already constrained to `http://` or
            // `https://` by the outer `strip_prefix` chain.
            // The value is not quoted back: a query string is where an
            // operator pastes a key (`?subscription-key=…`), and this error
            // reaches the caller.
            if base.contains('?') {
                return Err(BridgeError::InvalidUpstreamConfig(
                    "azure api_base must not contain a query string \
                     (the bridge appends `?api-version=…`; an operator-supplied \
                     query would either merge or override the pinned api-version)"
                        .to_string(),
                ));
            }
            if base.contains('#') {
                return Err(BridgeError::InvalidUpstreamConfig(
                    "azure api_base must not contain a fragment".to_string(),
                ));
            }
            let override_base = base.trim_end_matches('/').to_string();
            return Ok(Self {
                resource: String::new(),
                deployment: deployment.to_string(),
                api_version: Self::DEFAULT_API_VERSION.to_string(),
                upstream_override: Some(override_base),
            });
        }

        // Bare-resource shorthand (`acme-east` → canonical Azure URL).
        validate_url_token("api_base resource name", base)?;
        Ok(Self {
            resource: base.to_string(),
            deployment: deployment.to_string(),
            api_version: Self::DEFAULT_API_VERSION.to_string(),
            upstream_override: None,
        })
    }

    /// Build the chat-completions URL for this Azure upstream.
    ///
    /// Two shapes:
    ///   - Canonical (no override): `https://<resource>.openai.azure.com/openai/deployments/<deployment>/chat/completions?api-version=<v>`
    ///   - Verbatim override: `<override>/openai/deployments/<deployment>/chat/completions?api-version=<v>`
    ///
    /// The deployment path + api-version are always appended by the
    /// bridge — operators set just the host root in `api_base`,
    /// never a full chat-completions URL (the `?` / `#` rejection
    /// in `resolve()` enforces this).
    pub fn chat_completions_url(&self) -> String {
        self.deployment_url("chat/completions")
    }

    /// `POST {base}/openai/deployments/{deployment}/images/generations?api-version=…`
    /// — Azure's image-generation route for a DALL·E deployment.
    /// <https://learn.microsoft.com/en-us/azure/ai-services/openai/reference#image-generation>
    pub fn images_url(&self) -> String {
        self.deployment_url("images/generations")
    }

    /// `POST {base}/openai/deployments/{deployment}/embeddings?api-version=…`
    /// — Azure's embeddings route, which differs from OpenAI's only in
    /// carrying the deployment in the path instead of the model in the body.
    /// <https://learn.microsoft.com/en-us/azure/ai-services/openai/reference#embeddings>
    pub fn embeddings_url(&self) -> String {
        self.deployment_url("embeddings")
    }

    /// Shared path builder. Both routes hang off the same
    /// `/openai/deployments/{deployment}` prefix and carry the same pinned
    /// `api-version`, so a new route cannot accidentally drop either.
    fn deployment_url(&self, route: &str) -> String {
        let base = self
            .upstream_override
            .clone()
            .unwrap_or_else(|| format!("https://{}.openai.azure.com", self.resource));
        format!(
            "{}/openai/deployments/{}/{}?api-version={}",
            base, self.deployment, route, self.api_version,
        )
    }
}

/// Reject URL-control characters in operator/customer-supplied tokens
/// that end up in the Azure URL path. Azure resource names and
/// deployment names are documented as `[A-Za-z0-9_-]+`, so anything
/// outside that set is either a misconfig or a URL-injection attempt
/// (e.g. `?api-version=evil` to override the bridge's version pin).
fn validate_url_token(name: &str, value: &str) -> Result<(), BridgeError> {
    if value.is_empty() {
        return Err(BridgeError::InvalidUpstreamConfig(format!(
            "azure {name} is empty (expected an identifier matching [A-Za-z0-9_-]+)"
        )));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        // 这条报错是调用方读得到的 400。裸资源名这一支的值没有 scheme，算不上
        // URL，`user:pass@res` 会一路走到这里，所以回显前要去掉 userinfo。
        let shown = aisix_gateway::redact_userinfo(value);
        return Err(BridgeError::InvalidUpstreamConfig(format!(
            "azure {name} {shown:?} contains URL-control characters — \
             must match [A-Za-z0-9_-]+ (no spaces, slashes, dots, query params, or hash)"
        )));
    }
    Ok(())
}

/// Discriminated auth scheme. Today's resource-key deployments keep
/// the verbatim-string secret shape (backward-compat); the AAD path
/// is opted into by encoding the secret as JSON
/// `{tenant_id, client_id, client_secret}`.
///
/// Detection is by the leading character of the trimmed secret —
/// `{` triggers JSON parse, anything else is treated as a literal
/// api-key. Real Azure api-keys are URL-safe base64 strings, never
/// starting with `{`, so the heuristic is unambiguous in practice.
#[derive(Debug)]
pub(crate) enum AzureSecret {
    ApiKey(String),
    Aad(crate::aad_token_mint::AadCredentials),
}

impl AzureSecret {
    /// Parse the inbound `ProviderKey.api_key` into either the api-key
    /// or AAD branch.
    ///
    /// **Audit-aware:** error messages MUST NOT echo raw secret
    /// bytes. The branch heuristic is `starts_with('{')` after
    /// trimming — that test does not panic, returns Bool, no leaks.
    /// The JSON parse error message is fixed (not interpolated from
    /// serde) so partial secret contents can't surface in the error.
    pub(crate) fn parse(secret: &str) -> Result<Self, BridgeError> {
        let trimmed = secret.trim();
        if trimmed.is_empty() {
            return Err(BridgeError::InvalidUpstreamCredentials(
                "provider_key.api_key is empty".into(),
            ));
        }
        if trimmed.starts_with('{') {
            let creds: crate::aad_token_mint::AadCredentials = serde_json::from_str(trimmed)
                .map_err(|_e| {
                    BridgeError::InvalidUpstreamCredentials(
                        "azure provider_key.api_key looks JSON-shaped but failed to parse \
                         as AAD client_credentials \
                         {tenant_id, client_id, client_secret}"
                            .into(),
                    )
                })?;
            creds.validate()?;
            Ok(AzureSecret::Aad(creds))
        } else {
            Ok(AzureSecret::ApiKey(trimmed.to_string()))
        }
    }
}

/// The credential a request built outside the bridge sends to an Azure
/// endpoint, resolved from the Provider Key secret exactly as the bridge
/// resolves it: a verbatim key goes out as the `api-key` header, the JSON
/// client-credentials shape as an Entra ID bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AzureCredential {
    ApiKey(String),
    Bearer(String),
}

/// Process-wide token cache for the callers of [`resolve_credential`].
/// Kept apart from each bridge's own minter; both key their cache by the
/// whole credential, so sharing would only save a mint per process.
static SHARED_TOKEN_MINTER: std::sync::LazyLock<TokenMinter> =
    std::sync::LazyLock::new(|| TokenMinter::new(default_client()));

/// Resolve an Azure Provider Key secret into the credential to send, minting
/// (or reusing a cached) Entra ID token for the client-credentials shape.
pub async fn resolve_credential(secret: &str) -> Result<AzureCredential, BridgeError> {
    match AzureSecret::parse(secret)? {
        AzureSecret::ApiKey(key) => Ok(AzureCredential::ApiKey(key)),
        AzureSecret::Aad(creds) => SHARED_TOKEN_MINTER
            .get_token(&creds)
            .await
            .map(AzureCredential::Bearer),
    }
}

/// Resolved per-request auth header pair the bridge writes onto the
/// outbound request. Exactly ONE of `api_key` / `bearer_token` is
/// `Some`; the other is `None`.
pub(crate) struct AzureAuth {
    pub api_key: Option<String>,
    pub bearer_token: Option<String>,
}

/// Pull the upstream deployment name off the BridgeContext. Azure
/// deployment names (operator-defined in the Azure portal, e.g.
/// `gpt4o-prod`) live on Model.model_name. `req.model` is the
/// customer-facing display name and must NOT be used here.
fn upstream_model(ctx: &BridgeContext) -> Result<&str, BridgeError> {
    ctx.model
        .model_name
        .as_deref()
        .ok_or_else(|| BridgeError::InvalidUpstreamConfig("model.model_name missing".into()))
}

/// Map an Azure HTTP error response to a [`BridgeError::UpstreamStatus`]
/// carrying Azure's own error message.
///
/// The envelope is parsed whatever the `Content-Type`: Azure answers a
/// missing deployment with a JSON body and no such header.
async fn map_http_error(status: StatusCode, resp: reqwest::Response) -> BridgeError {
    aisix_gateway::capture_upstream_error_http(
        status,
        resp,
        aisix_gateway::UpstreamWire::AzureOpenAI,
        parse_azure_error_envelope,
    )
    .await
}

/// The OpenAI envelope, with Azure's code in both `kind` and `code`: Azure's
/// `error.type` is not what the translation table keys on, so the renderer can
/// override the code with a derived one (`DeploymentNotFound`) or pass it
/// through (`rate_limit_exceeded`). `code` stays Azure's own outer code, which
/// a relayed error carries as sent.
///
/// Azure-specific quirk: content-policy violations nest under
/// `error.inner_error.code` *or* `error.innererror.code` (Azure emits
/// both casings depending on endpoint).
/// `"ResponsibleAIPolicyViolation"` on the inner code becomes `kind` so the
/// gateway can recognise the policy hit even when the outer code is the
/// generic `invalid_request_error`.
fn parse_azure_error_envelope(body: &[u8]) -> Option<aisix_gateway::UpstreamErrorView> {
    #[derive(serde::Deserialize)]
    struct Outer {
        error: Inner,
    }
    #[derive(serde::Deserialize)]
    struct Inner {
        inner_error: Option<InnerInner>,
        innererror: Option<InnerInner>,
    }
    #[derive(serde::Deserialize)]
    struct InnerInner {
        code: Option<String>,
    }
    const POLICY_VIOLATION: &str = "ResponsibleAIPolicyViolation";
    let mut view = aisix_provider_openai::parse_openai_error_envelope(body)?;
    let inner_code = serde_json::from_slice::<Outer>(body).ok().and_then(|o| {
        o.error
            .inner_error
            .or(o.error.innererror)
            .and_then(|i| i.code)
    });
    view.kind = if inner_code.as_deref() == Some(POLICY_VIOLATION) {
        inner_code
    } else {
        view.code.clone()
    };
    Some(view)
}

/// Wrap a future in the optional deadline, counted from `started`: a wait
/// that follows another on the same attempt gets only what is left of it.
/// `None` → no timeout.
async fn with_deadline<T, F>(
    deadline: Option<Duration>,
    started: Instant,
    fut: F,
) -> Result<T, BridgeError>
where
    F: std::future::Future<Output = Result<T, BridgeError>>,
{
    match deadline {
        None => fut.await,
        Some(d) => match tokio::time::timeout(d.saturating_sub(started.elapsed()), fut).await {
            Ok(r) => r,
            Err(_) => Err(BridgeError::Timeout {
                elapsed_ms: started.elapsed().as_millis() as u64,
                cause: String::new(),
            }),
        },
    }
}

/// Apply RequestOverrides + ResponseOverrides flag-driven body
/// transforms before sending. Mirrors `OpenAiBridge::prepare_outbound_body`.
fn prepare_outbound_body<T: serde::Serialize>(
    typed: &T,
    gateway_strict_schema: bool,
    reasoning_model: bool,
    request: Option<&RequestOverrides>,
    response: Option<&ResponseOverrides>,
) -> Result<Value, BridgeError> {
    let mut body = serde_json::to_value(typed)
        .map_err(|e| BridgeError::Config(format!("serialize request body: {e}")))?;
    // Azure serves the OpenAI wire, so it owes the same strict-mode
    // schema closing the OpenAI edge applies — one function, not a
    // second copy, because these two bodies have to stay identical.
    close_strict_response_format_schema(&mut body, gateway_strict_schema);
    // Before `param_renames`, so an operator's explicit rename wins.
    if reasoning_model {
        aisix_provider_openai::reasoning::apply_reasoning_token_cap(&mut body);
    }
    if let Some(r) = request {
        validate_content_safe_request_overrides(r).map_err(BridgeError::Config)?;
        apply_param_renames(&mut body, &r.param_renames);
        if let Some(constraints) = &r.param_constraints {
            apply_param_constraints(&mut body, constraints);
        }
        // A default `max_tokens` fills a cap the caller did not send, so a
        // reasoning model needs it converted as well; a `max_tokens` already
        // present here was put back by the operator's own rename and stays.
        let had_max_tokens = body.get("max_tokens").is_some();
        apply_default_body_fields(&mut body, &r.default_body_fields);
        if reasoning_model && !had_max_tokens {
            aisix_provider_openai::reasoning::apply_reasoning_token_cap(&mut body);
        }
    }
    if response.is_some_and(|r| r.content_list_to_string) {
        apply_content_list_to_string(&mut body);
    }
    Ok(body)
}

/// Build the base outbound `HeaderMap` for Azure. The auth header
/// depends on the resolved auth scheme:
///
///   - api-key scheme → `api-key: <secret>` (Azure docs:
///     <https://learn.microsoft.com/en-us/azure/ai-services/openai/reference>;
///     the literal lowercase-hyphenated `api-key`, NOT
///     `Authorization: Bearer`).
///   - AAD scheme → `Authorization: Bearer <access_token>` (the
///     industry-standard Bearer header; matches how Azure's own
///     Python SDK sets the header on Entra ID auth).
///
/// In both branches the bridge also sets `Content-Type: application/json`,
/// `x-aisix-request-id: <ctx.request_id>`, and (for streaming)
/// `Accept: text/event-stream`. Bridge-owned headers are inserted before
/// `apply_request_headers`, which is what stops a `default_headers` entry
/// displacing them — that merge is skip-if-present. Note this bridge fills
/// exactly ONE credential slot per auth mode, so the OTHER slot is not
/// bridge-owned and operator config reaches it: on `api-key` auth an
/// `authorization` entry is honoured, which is how an Azure-shaped
/// upstream behind its own bearer-authenticated hop is reached. A
/// forwarded client header displaces even the filled slot, deliberately —
/// see `apply_request_headers`.
fn build_request_headers(
    auth: &AzureAuth,
    request_id: &str,
    sse: bool,
    hdr: &UpstreamHeaderContext<'_>,
) -> Result<HeaderMap, BridgeError> {
    let mut headers = HeaderMap::new();
    if let Some(credential) = auth.api_key.as_ref().or(auth.bearer_token.as_ref()) {
        aisix_gateway::note_sent_credential(credential);
    }
    match (&auth.api_key, &auth.bearer_token) {
        (Some(key), None) => {
            let value = HeaderValue::from_str(key).map_err(|e| {
                BridgeError::InvalidUpstreamCredentials(format!(
                    "api key contains invalid header chars: {e}"
                ))
            })?;
            headers.insert(HeaderName::from_static("api-key"), value);
        }
        (None, Some(token)) => {
            // Bearer token from AAD. Same byte-validation pattern as
            // api-key. Note: the validation error MUST NOT echo the
            // token bytes — `http::InvalidHeaderValue`'s Display is
            // opaque, but a future change could include byte
            // position; rebind to a fixed message to be safe.
            let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                BridgeError::Config(
                    "azure aad access_token contains invalid header characters".into(),
                )
            })?;
            headers.insert(header::AUTHORIZATION, value);
        }
        // parse() / resolve_auth() ensure exactly one is Some — keep
        // explicit guard for defense in depth.
        _ => {
            return Err(BridgeError::Config(
                "internal: AzureAuth must set exactly one of api_key / bearer_token".into(),
            ))
        }
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let rid = HeaderValue::from_str(request_id).map_err(|e| {
        BridgeError::Config(format!("request_id contains invalid header chars: {e}"))
    })?;
    headers.insert(HeaderName::from_static("x-aisix-request-id"), rid);
    if sse {
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
    }
    apply_request_headers(&mut headers, hdr);
    Ok(headers)
}

#[async_trait]
impl Bridge for AzureOpenAiBridge {
    fn name(&self) -> &'static str {
        self.name
    }

    fn wire_protocol(&self) -> &'static str {
        aisix_core::Adapter::AzureOpenai.wire_protocol()
    }

    async fn chat(
        &self,
        req: &ChatFormat,
        ctx: &BridgeContext,
    ) -> Result<ChatResponse, BridgeError> {
        self.chat_cow(std::borrow::Cow::Borrowed(req), ctx).await
    }

    async fn chat_cow(
        &self,
        req: std::borrow::Cow<'_, ChatFormat>,
        ctx: &BridgeContext,
    ) -> Result<ChatResponse, BridgeError> {
        let req = aisix_gateway::tool_calls::chat_without_thought_signatures(req);
        let deployment = upstream_model(ctx)?;
        let upstream = AzureUpstreamRef::resolve(deployment, ctx.provider_key.api_base.as_deref())?;
        // Keep reserved-config helpers reachable from the public surface
        // so a future override-validation PR can wire them in without
        // re-exposing private state.
        let _ = wire::reserved_query_params();
        let _ = wire::reserved_auth_headers();

        // Resolve auth BEFORE entering the request future so AAD
        // mint failures surface as a direct Err return rather than
        // a transport timeout. api-key path is a no-op string copy.
        let started = Instant::now();
        let auth = self.resolve_auth(ctx, started).await?;
        // Azure expects the deployment name in the URL path; the JSON
        // body's `model` field is ignored by Azure (or echoed back).
        // We still set it to the deployment name for log-trace clarity
        // and to mirror the upstream OpenAI SDK convention.
        let body = {
            let messages = messages_from(&req, AZURE_DEVELOPER_ROLE_MODE);
            let typed = build_request(&req, deployment, &messages, false);
            aisix_gateway::json_body(&prepare_outbound_body(
                &typed,
                req.gateway_strict_schema,
                is_reasoning_model(ReasoningFamily::AzureOpenai, deployment),
                ctx.provider_key.request.as_ref(),
                ctx.provider_key.response.as_ref(),
            )?)
            .map_err(|e| BridgeError::Config(format!("serialize request body: {e}")))?
        };
        // The owned request is not needed past its wire bytes.
        drop(req);
        let headers = build_request_headers(&auth, &ctx.request_id, false, &ctx.header_ctx())?;
        let url = self.resolve_url(ctx, &upstream);
        let client = self.client_for(ctx);

        with_deadline(ctx.deadline, started, async move {
            let resp = url
                .post_on(&client)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(aisix_gateway::send_error)?;

            aisix_gateway::note_upstream_response(resp.headers());
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(status, resp).await);
            }

            // Azure injects `prompt_filter_results` /
            // `content_filter_results` blocks. OpenAiResponse uses
            // `#[serde(default)]` on optional fields and does NOT set
            // `deny_unknown_fields`, so the extension fields pass
            // through transparently without breaking deserialization.
            let parsed: OpenAiResponse =
                aisix_gateway::decode_success_body(resp, aisix_gateway::UpstreamWire::AzureOpenAI)
                    .await?;
            Ok(response_into_chat_response(parsed))
        })
        .await
    }

    async fn chat_stream(
        &self,
        req: &ChatFormat,
        ctx: &BridgeContext,
    ) -> Result<ChatChunkStream, BridgeError> {
        self.chat_stream_cow(std::borrow::Cow::Borrowed(req), ctx)
            .await
    }

    async fn chat_stream_cow(
        &self,
        req: std::borrow::Cow<'_, ChatFormat>,
        ctx: &BridgeContext,
    ) -> Result<ChatChunkStream, BridgeError> {
        let req = aisix_gateway::tool_calls::chat_without_thought_signatures(req);
        let deployment = upstream_model(ctx)?;
        let upstream = AzureUpstreamRef::resolve(deployment, ctx.provider_key.api_base.as_deref())?;
        let _ = wire::reserved_query_params();
        let _ = wire::reserved_auth_headers();

        // See chat() — resolve auth before the request future.
        let started = Instant::now();
        let auth = self.resolve_auth(ctx, started).await?;
        let body = {
            let messages = messages_from(&req, AZURE_DEVELOPER_ROLE_MODE);
            let typed = build_request(&req, deployment, &messages, true);
            aisix_gateway::json_body(&prepare_outbound_body(
                &typed,
                req.gateway_strict_schema,
                is_reasoning_model(ReasoningFamily::AzureOpenai, deployment),
                ctx.provider_key.request.as_ref(),
                ctx.provider_key.response.as_ref(),
            )?)
            .map_err(|e| BridgeError::Config(format!("serialize request body: {e}")))?
        };
        // The owned request is not needed past its wire bytes.
        drop(req);
        let headers = build_request_headers(&auth, &ctx.request_id, true, &ctx.header_ctx())?;
        let url = self.resolve_url(ctx, &upstream);
        let client = self.client_for(ctx);

        let resp = with_deadline(ctx.deadline, started, async move {
            url.post_on(&client)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(aisix_gateway::send_error)
        })
        .await?;

        aisix_gateway::note_upstream_response(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            return Err(
                aisix_gateway::upstream_error_within(ctx.deadline, resp, |resp| {
                    map_http_error(status, resp)
                })
                .await,
            );
        }

        // Snapshot the response-side override knobs onto the stream
        // closure so it can run after `ctx` drops.
        let reasoning_path = ctx
            .provider_key
            .response
            .as_ref()
            .and_then(|r| r.reasoning_field.clone());
        let done_marker_policy = ctx
            .provider_key
            .response
            .as_ref()
            .and_then(|r| r.stream_done_marker);
        let bridge_name = self.name;
        let request_id_for_log = ctx.request_id.clone();

        // Audit H2: thread the budget into the streaming loop so a slow /
        // hanging upstream body can't wedge the connection after headers
        // arrive. It bounds the wait for EACH chunk and resets after every
        // successful read — `stream_timeout` is defined as the maximum gap
        // *between* chunks, so a long-but-healthy response must not be cut
        // off just because its total duration exceeds one gap's budget.
        let per_chunk_timeout = ctx.deadline;

        let byte_stream = resp.bytes_stream();
        let stream = build_chunk_stream(
            byte_stream,
            reasoning_path,
            done_marker_policy,
            bridge_name,
            request_id_for_log,
            per_chunk_timeout,
        );
        Ok(Box::pin(stream))
    }

    /// Azure speaks OpenAI's embeddings wire shape — the deployment name
    /// moves from the body to the URL path, and everything else is
    /// identical, so the request/response structs are the OpenAI ones.
    ///
    /// Without this, an Azure-backed model on `/v1/embeddings` hit the
    /// trait's default and came back "this provider does not support
    /// embeddings" as a 501: an operator whose whole estate is Azure could
    /// serve chat and nothing else, from a Model row that looked no
    /// different from a working one.
    async fn embed(
        &self,
        req: &EmbeddingRequest,
        ctx: &BridgeContext,
    ) -> Result<EmbeddingResponse, BridgeError> {
        let deployment = upstream_model(ctx)?;
        let upstream = AzureUpstreamRef::resolve(deployment, ctx.provider_key.api_base.as_deref())?;
        // Resolve auth BEFORE the request future so an AAD mint failure
        // surfaces as itself rather than as a transport timeout.
        let started = Instant::now();
        let auth = self.resolve_auth(ctx, started).await?;
        // The deployment is the model as far as Azure is concerned; the
        // body's `model` is ignored, and is set to the deployment name for
        // log-trace clarity, matching `chat()`.
        let body = aisix_gateway::json_body(&prepare_outbound_body(
            &embed_request_body(req, deployment),
            false,
            false,
            ctx.provider_key.request.as_ref(),
            ctx.provider_key.response.as_ref(),
        )?)
        .map_err(|e| BridgeError::Config(format!("serialize request body: {e}")))?;
        let headers = build_request_headers(&auth, &ctx.request_id, false, &ctx.header_ctx())?;
        let url = self.resolve_route_url(ctx, &upstream, "azure/embeddings", || {
            upstream.embeddings_url()
        });
        let client = self.client_for(ctx);

        with_deadline(ctx.deadline, started, async move {
            let resp = url
                .post_on(&client)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(aisix_gateway::send_error)?;

            aisix_gateway::note_upstream_response(resp.headers());
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(status, resp).await);
            }

            let parsed: OpenAiEmbedResponse =
                aisix_gateway::decode_success_body(resp, aisix_gateway::UpstreamWire::AzureOpenAI)
                    .await?;
            Ok(embed_response_into(parsed))
        })
        .await
    }

    /// Azure's image-generation route for a DALL·E deployment. Same body and
    /// response as OpenAI's, with the deployment in the path — so, like
    /// `embed`, this is a routing difference rather than a wire difference.
    async fn generate_image(
        &self,
        body: &Value,
        ctx: &BridgeContext,
    ) -> Result<Value, BridgeError> {
        let deployment = upstream_model(ctx)?;
        let upstream = AzureUpstreamRef::resolve(deployment, ctx.provider_key.api_base.as_deref())?;
        let started = Instant::now();
        let auth = self.resolve_auth(ctx, started).await?;
        // Azure reads the deployment from the URL and ignores the body's
        // `model`; it is set to the deployment name for log-trace clarity,
        // matching `chat()` and `embed()`.
        let outbound = {
            let mut outbound = body.clone();
            if let Some(obj) = outbound.as_object_mut() {
                obj.insert("model".to_string(), Value::String(deployment.to_string()));
            }
            aisix_gateway::json_body(&prepare_outbound_body(
                &outbound,
                false,
                false,
                ctx.provider_key.request.as_ref(),
                ctx.provider_key.response.as_ref(),
            )?)
            .map_err(|e| BridgeError::Config(format!("serialize request body: {e}")))?
        };
        let headers = build_request_headers(&auth, &ctx.request_id, false, &ctx.header_ctx())?;
        let url = self.resolve_route_url(ctx, &upstream, "azure/images", || upstream.images_url());
        let client = self.client_for(ctx);

        with_deadline(ctx.deadline, started, async move {
            let resp = url
                .post_on(&client)
                .headers(headers)
                .body(outbound)
                .send()
                .await
                .map_err(aisix_gateway::send_error)?;

            aisix_gateway::note_upstream_response(resp.headers());
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(status, resp).await);
            }

            aisix_gateway::decode_success_body(resp, aisix_gateway::UpstreamWire::AzureOpenAI).await
        })
        .await
    }
}

fn build_chunk_stream<S>(
    byte_stream: S,
    reasoning_path: Option<String>,
    done_marker_policy: Option<StreamDoneMarker>,
    bridge_name: &'static str,
    request_id: String,
    per_chunk_timeout: Option<Duration>,
) -> impl futures::Stream<Item = Result<ChatChunk, BridgeError>> + Send
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
{
    async_stream::try_stream! {
        let mut decoder = SseDecoder::new();
        let mut tool_call_indexer = ToolCallIndexer::default();
        let mut stream = Box::pin(byte_stream);
        let mut done_marker_seen = false;
        // A finish reason ends one choice; the answer is whole once every
        // choice the stream carried has finished.
        let mut choices = aisix_gateway::ChoiceFinishCensus::default();
        'outer: loop {
            // Audit H2: bound each individual chunk wait. `tokio::time::timeout`
            // (not `timeout_at`) so the budget restarts on every chunk — the
            // configured value is the maximum inter-chunk gap, not a ceiling on
            // the response as a whole. `None` disables the timeout.
            let next = match per_chunk_timeout {
                Some(d) => match tokio::time::timeout(d, stream.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        Err(BridgeError::Timeout {
                            elapsed_ms: d.as_millis() as u64,
                            cause: String::new(),
                        })?;
                        unreachable!()
                    }
                },
                None => stream.next().await,
            };
            let Some(next) = next else { break 'outer; };
            let chunk = match next {
                Ok(chunk) => chunk,
                Err(_) if choices.all_finished() => return,
                Err(e) => {
                    Err(BridgeError::Transport(aisix_gateway::transport_error_message(&e)))?
                }
            };
            for event in decoder
                .feed(chunk.as_ref())
                .map_err(|e| BridgeError::UpstreamDecode(e.to_string()))?
            {
                match event {
                    SseEvent::Done => {
                        done_marker_seen = true;
                        break 'outer;
                    }
                    SseEvent::Data(payload) => {
                        let parsed = parse_stream_chunk(&payload, reasoning_path.as_deref())?;
                        for chunk in stream_chunk_into_chat_chunks(parsed, &mut tool_call_indexer) {
                            choices.observe(&chunk);
                            yield chunk;
                        }
                    }
                }
            }
        }
        match decoder
            .finish()
            .map_err(|e| BridgeError::UpstreamDecode(e.to_string()))?
        {
            Some(SseEvent::Done) => {
                done_marker_seen = true;
            }
            Some(SseEvent::Data(payload)) => {
                let parsed = parse_stream_chunk(&payload, reasoning_path.as_deref())?;
                for chunk in stream_chunk_into_chat_chunks(parsed, &mut tool_call_indexer) {
                    choices.observe(&chunk);
                    yield chunk;
                }
            }
            None => {}
        }
        // Issue #302 §5 `response.stream_done_marker` — violations
        // are logged (operator diagnostic) but never error the
        // request: customer chunks have already been delivered.
        if let Some(policy) = done_marker_policy {
            match apply_stream_done_marker_policy(policy, done_marker_seen) {
                StreamDoneOutcome::Ok => {}
                StreamDoneOutcome::MissingDoneMarker => {
                    tracing::warn!(
                        bridge = bridge_name,
                        request_id = %request_id,
                        "upstream stream ended without [DONE] marker (policy=Required)"
                    );
                }
                StreamDoneOutcome::UnexpectedDoneMarker => {
                    tracing::warn!(
                        bridge = bridge_name,
                        request_id = %request_id,
                        "upstream emitted [DONE] marker (policy=None)"
                    );
                }
            }
        }
        if !done_marker_seen && !choices.all_finished() {
            Err(BridgeError::StreamAborted)?;
        }
    }
}

fn parse_stream_chunk(
    payload: &str,
    reasoning_path: Option<&str>,
) -> Result<OpenAiStreamChunk, BridgeError> {
    let parsed = match reasoning_path {
        Some(path) => serde_json::from_str::<Value>(payload)
            .map_err(|e| e.to_string())
            .and_then(|mut value| {
                extract_reasoning_field(&mut value, path);
                serde_json::from_value(value).map_err(|e| e.to_string())
            }),
        None => serde_json::from_str(payload).map_err(|e| e.to_string()),
    };
    parsed.map_err(|serde_err| {
        // A frame that isn't a chunk may be Azure reporting an error
        // inside the 200 stream (`{"error":{...}}`) — surface the
        // provider's own error instead of the serde failure.
        aisix_gateway::capture_in_band_error(payload, aisix_gateway::UpstreamWire::AzureOpenAI)
            .unwrap_or(BridgeError::UpstreamDecode(serde_err))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Azure serves the OpenAI wire, so an Anthropic-shaped
    /// `/v1/messages` request translated onto an Azure deployment must
    /// arrive with exactly the schema the OpenAI edge would send. Azure
    /// keeps its own copy of the outbound-body pipeline, which is
    /// precisely how it came to be missing the strict-mode closing.
    #[test]
    fn a_translated_messages_request_reaches_azure_byte_for_byte_as_before() {
        use aisix_gateway::{ChatFormat, ChatMessage};
        use aisix_provider_anthropic::wire::translate_extras_to_openai_shape;

        let mut extra = serde_json::json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "city": {"type": "string"},
                            "days": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {"high": {"type": "number"}},
                                },
                            },
                        },
                    },
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let gateway_strict =
            translate_extras_to_openai_shape(&mut extra, aisix_core::MappedEffort::AsWritten);
        assert!(gateway_strict);

        let mut req = ChatFormat::new("m", vec![ChatMessage::user("weather?")]);
        req.extra = extra;
        let messages = messages_from(&req, AZURE_DEVELOPER_ROLE_MODE);
        let typed = build_request(&req, "ci-chat", &messages, false);
        let body = prepare_outbound_body(&typed, gateway_strict, false, None, None).unwrap();

        assert_eq!(
            body["response_format"],
            serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "structured_output",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["city", "days"],
                        "properties": {
                            "city": {"type": "string"},
                            "days": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["high"],
                                    "properties": {"high": {"type": "number"}},
                                },
                            },
                        },
                    },
                },
            })
        );
    }

    /// #1222 scenario 3: an in-band `data: {"error":{...}}`
    /// frame inside the committed 200 stream surfaces as the typed
    /// in-band error, not a serde decode failure.
    #[test]
    fn parse_stream_chunk_captures_in_band_error_frame() {
        let err = parse_stream_chunk(
            r#"{"error":{"message":"stream failed","type":"server_error","code":"InternalServerError"}}"#,
            None,
        )
        .unwrap_err();
        match err {
            BridgeError::UpstreamInBand {
                status,
                message,
                parsed,
                wire,
            } => {
                assert_eq!(status, None);
                assert_eq!(message, "stream failed");
                assert_eq!(
                    parsed.expect("view").code.as_deref(),
                    Some("InternalServerError")
                );
                assert!(matches!(wire, aisix_gateway::UpstreamWire::AzureOpenAI));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Garbage keeps the historical decode classification.
        assert!(matches!(
            parse_stream_chunk("{not-json", None).unwrap_err(),
            BridgeError::UpstreamDecode(_)
        ));
    }

    #[test]
    fn resolve_accepts_canonical_https_resource() {
        let r = AzureUpstreamRef::resolve("gpt4o-prod", Some("https://acme-west.openai.azure.com"))
            .unwrap();
        assert_eq!(r.resource, "acme-west");
        assert_eq!(r.deployment, "gpt4o-prod");
        assert_eq!(r.api_version, AzureUpstreamRef::DEFAULT_API_VERSION);
    }

    #[test]
    fn resolve_accepts_bare_resource_name() {
        let r = AzureUpstreamRef::resolve("dep", Some("acme-east")).unwrap();
        assert_eq!(r.resource, "acme-east");
    }

    #[test]
    fn resolve_rejects_empty_deployment() {
        let err = AzureUpstreamRef::resolve("", Some("https://acme.openai.azure.com")).unwrap_err();
        match err {
            BridgeError::InvalidUpstreamConfig(msg) => {
                assert!(
                    msg.contains("deployment name is empty"),
                    "must call out empty deployment; got {msg}"
                );
            }
            other => panic!("expected InvalidUpstreamConfig error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_missing_api_base() {
        let err = AzureUpstreamRef::resolve("dep", None).unwrap_err();
        match err {
            BridgeError::InvalidUpstreamConfig(msg) => {
                assert!(
                    msg.contains("no api_base"),
                    "must call out missing api_base; got {msg}"
                );
            }
            other => panic!("expected InvalidUpstreamConfig error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_empty_api_base() {
        let err = AzureUpstreamRef::resolve("dep", Some("   ")).unwrap_err();
        match err {
            BridgeError::InvalidUpstreamConfig(msg) => {
                assert!(msg.contains("no api_base"));
            }
            other => panic!("expected InvalidUpstreamConfig error, got {other:?}"),
        }
    }

    #[test]
    fn chat_completions_url_matches_azure_api_path() {
        let r = AzureUpstreamRef {
            resource: "acme-west".into(),
            deployment: "gpt4o-prod".into(),
            api_version: "2024-10-21".into(),
            upstream_override: None,
        };
        assert_eq!(
            r.chat_completions_url(),
            "https://acme-west.openai.azure.com/openai/deployments/gpt4o-prod/chat/completions?api-version=2024-10-21",
        );
    }

    #[test]
    fn resolve_rejects_deployment_with_query_injection() {
        let err = AzureUpstreamRef::resolve("foo?api-version=evil", Some("acme-east")).unwrap_err();
        match err {
            BridgeError::InvalidUpstreamConfig(msg) => {
                assert!(msg.contains("URL-control characters"), "got {msg}");
            }
            other => panic!("expected InvalidUpstreamConfig error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_deployment_with_slash_injection() {
        let err = AzureUpstreamRef::resolve("foo/bar/chat", Some("acme")).unwrap_err();
        assert!(matches!(err, BridgeError::InvalidUpstreamConfig(_)));
    }

    #[test]
    fn resolve_rejects_deployment_with_hash_fragment() {
        let err = AzureUpstreamRef::resolve("foo#bar", Some("acme")).unwrap_err();
        assert!(matches!(err, BridgeError::InvalidUpstreamConfig(_)));
    }

    #[test]
    fn resolve_rejects_resource_with_query_injection() {
        let err = AzureUpstreamRef::resolve("dep", Some("acme?evil=1")).unwrap_err();
        assert!(matches!(err, BridgeError::InvalidUpstreamConfig(_)));
    }

    #[test]
    fn resolve_accepts_verbatim_override_for_non_canonical_host() {
        // Fixes api7/ai-gateway#391: a BYO operator pasting a
        // corporate-proxy / private-VPC / mock URL whose host does
        // NOT end in `.openai.azure.com` is now treated as a verbatim
        // override (resource is empty, `upstream_override` holds the
        // base). Pre-fix builds rejected this as a Config error,
        // blocking any non-canonical Azure deployment.
        //
        // Compatible with Bedrock's `endpoint_url` precedence — both
        // bridges now honor `ProviderKey.api_base` as a production
        // override path.
        let r = AzureUpstreamRef::resolve("gpt4o-prod", Some("https://acme.evil.com")).unwrap();
        assert_eq!(r.resource, "");
        assert_eq!(r.deployment, "gpt4o-prod");
        assert_eq!(
            r.upstream_override.as_deref(),
            Some("https://acme.evil.com")
        );
        assert_eq!(
            r.chat_completions_url(),
            "https://acme.evil.com/openai/deployments/gpt4o-prod/chat/completions?api-version=2024-10-21",
        );
    }

    #[test]
    fn resolve_accepts_http_override_for_mock_endpoint() {
        // The canonical e2e use case: mock-llm at http://mock-llm:8000
        // in the compose bridge network. Same shape as the corporate-
        // proxy override, just over plain HTTP.
        let r = AzureUpstreamRef::resolve("gpt4o", Some("http://mock-llm:8000")).unwrap();
        assert_eq!(r.upstream_override.as_deref(), Some("http://mock-llm:8000"));
        assert_eq!(
            r.chat_completions_url(),
            "http://mock-llm:8000/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21",
        );
    }

    #[test]
    fn resolve_strips_trailing_slash_on_verbatim_override() {
        let r = AzureUpstreamRef::resolve("dep", Some("https://proxy.acme.internal/")).unwrap();
        assert_eq!(
            r.upstream_override.as_deref(),
            Some("https://proxy.acme.internal")
        );
        // Single-slash separator before /openai/... — no `//`.
        assert!(!r.chat_completions_url().contains("internal//openai"));
    }

    #[test]
    fn resolve_rejects_override_with_query_injection() {
        // Defence-in-depth: an operator-supplied query string would
        // either merge with or override the bridge's pinned
        // `api-version=…` append. Reject up-front rather than risk
        // a downstream api-version downgrade attack.
        let err =
            AzureUpstreamRef::resolve("dep", Some("https://proxy.acme.internal?api-version=evil"))
                .unwrap_err();
        match err {
            BridgeError::InvalidUpstreamConfig(msg) => {
                assert!(
                    msg.contains("query string"),
                    "must call out the query rejection; got {msg}"
                );
            }
            other => panic!("expected InvalidUpstreamConfig error, got {other:?}"),
        }
    }

    /// The rejection reaches the caller, and a query string is where an
    /// operator pastes a key: the value must not be quoted back.
    #[test]
    fn a_rejected_query_string_is_not_quoted_back() {
        let err = AzureUpstreamRef::resolve(
            "dep",
            Some("https://apim.corp.net?subscription-key=SECRET-9f2b"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("SECRET-9f2b"), "{msg}");
        let err = AzureUpstreamRef::resolve("dep", Some("https://apim.corp.net#SECRET-9f2b"))
            .unwrap_err();
        assert!(!err.to_string().contains("SECRET-9f2b"));
    }

    #[test]
    fn resolve_rejects_override_with_fragment() {
        let err = AzureUpstreamRef::resolve("dep", Some("https://proxy.acme.internal#fragment"))
            .unwrap_err();
        assert!(matches!(err, BridgeError::InvalidUpstreamConfig(_)));
    }

    #[test]
    fn resolve_passes_userinfo_through_verbatim() {
        for (base, expected) in [
            (
                "https://user:pw@proxy.acme.internal/",
                "https://user:pw@proxy.acme.internal",
            ),
            (
                "https://user:pw@acme.openai.azure.com",
                "https://user:pw@acme.openai.azure.com",
            ),
        ] {
            let r = AzureUpstreamRef::resolve("dep", Some(base)).unwrap();
            assert_eq!(r.upstream_override.as_deref(), Some(expected), "{base}");
        }
    }

    #[test]
    fn resolve_preserves_path_prefix_on_verbatim_override() {
        // PR #392 audit MEDIUM-1: an operator pasting a proxy URL
        // with an internal path prefix (e.g. corporate gateway that
        // routes `/azure-passthrough/*` to upstream Azure) gets the
        // prefix preserved verbatim, with the bridge's deployment
        // path appended after.
        let r =
            AzureUpstreamRef::resolve("dep", Some("http://corp-proxy/azure-passthrough/")).unwrap();
        assert_eq!(
            r.upstream_override.as_deref(),
            Some("http://corp-proxy/azure-passthrough"),
        );
        assert_eq!(
            r.chat_completions_url(),
            "http://corp-proxy/azure-passthrough/openai/deployments/dep/chat/completions?api-version=2024-10-21",
        );
    }

    #[test]
    fn resolve_accepts_canonical_https_with_trailing_slash() {
        let r =
            AzureUpstreamRef::resolve("gpt4o-prod", Some("https://acme-west.openai.azure.com/"))
                .unwrap();
        assert_eq!(r.resource, "acme-west");
    }

    #[test]
    fn resolve_accepts_canonical_https_with_pasted_endpoint_path() {
        let r = AzureUpstreamRef::resolve(
            "gpt4o-prod",
            Some("https://acme-west.openai.azure.com/openai/deployments/x/chat/completions"),
        )
        .unwrap();
        assert_eq!(r.resource, "acme-west");
    }

    #[test]
    fn default_api_version_is_ga_shape() {
        let v = AzureUpstreamRef::DEFAULT_API_VERSION;
        assert!(
            !v.contains("preview"),
            "default API version must be GA, not preview; got {v:?}"
        );
        assert_eq!(v.len(), 10, "must match YYYY-MM-DD; got {v:?}");
        assert_eq!(v.chars().nth(4), Some('-'), "{v:?}");
        assert_eq!(v.chars().nth(7), Some('-'), "{v:?}");
    }

    #[test]
    fn bridge_name_is_stable() {
        assert_eq!(AzureOpenAiBridge::new().name(), "azure-openai");
    }

    // ─── Dispatch tests (wiremock) ────────────────────────────────────

    use aisix_core::{Model, ProviderKey};
    use aisix_gateway::ChatMessage;
    use std::sync::Arc;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

    /// Build a `BridgeContext` that points at a wiremock server. The
    /// server pretends to be `acme-west.openai.azure.com` — to make the
    /// reqwest client send there instead of the real Azure host we
    /// patch `chat_completions_url()` by routing through the mock host
    /// directly (see [`bridge_pointed_at_mock`]).
    fn sample_model() -> Arc<Model> {
        Arc::new(
            serde_json::from_str(
                r#"{
                    "display_name": "my-azure-gpt4",
                    "provider": "openai",
                    "model_name": "gpt4o-prod",
                    "provider_key_id": "11111111-1111-1111-1111-111111111111"
                }"#,
            )
            .unwrap(),
        )
    }

    fn sample_pk(api_base: Option<&str>) -> Arc<ProviderKey> {
        let api_base_json = match api_base {
            Some(b) => format!(r#", "api_base": "{b}""#),
            None => String::new(),
        };
        Arc::new(
            serde_json::from_str(&format!(
                r#"{{"display_name": "azure-prod", "secret": "az-key"{api_base_json}}}"#
            ))
            .unwrap(),
        )
    }

    /// Build a `ProviderKey` whose `secret` is the JSON-encoded AAD
    /// credentials shape. Used by the D6.6 (Entra ID) tests; sidesteps
    /// the string-escaping awkwardness of embedding JSON inside JSON.
    fn sample_pk_with_aad_secret(api_base: &str) -> Arc<ProviderKey> {
        let aad_json = r#"{"tenant_id":"tenant-uuid-aaa","client_id":"client-uuid-bbb","client_secret":"aad-secret-rotation-managed"}"#;
        let pk = serde_json::from_value::<ProviderKey>(serde_json::json!({
            "display_name": "azure-aad-prod",
            "secret": aad_json,
            "api_base": api_base,
        }))
        .unwrap();
        Arc::new(pk)
    }

    fn sample_pk_with_overrides(api_base: &str, overrides_json: &str) -> Arc<ProviderKey> {
        Arc::new(
            serde_json::from_str(&format!(
                r#"{{"display_name": "azure-prod", "secret": "az-key", "api_base": "{api_base}", {overrides_json}}}"#
            ))
            .unwrap(),
        )
    }

    /// Build a `BridgeContext` configured for dispatch tests:
    /// `Model.model_name = "gpt4o-prod"` and `ProviderKey.api_base`
    /// pinned to the canonical `https://acme-west.openai.azure.com`
    /// (so `AzureUpstreamRef::resolve` succeeds against the strict
    /// host-suffix check). The actual POST URL is rewritten by
    /// [`AzureOpenAiBridge::with_url_override`] to point at the
    /// wiremock server, so the test exercises the full `chat()` /
    /// `chat_stream()` entry point.
    fn canonical_test_ctx() -> BridgeContext {
        BridgeContext::new(
            "req-azure-1",
            sample_model(),
            sample_pk(Some("https://acme-west.openai.azure.com")),
        )
    }

    /// Compute the URL the wiremock server should receive — mirrors
    /// what `AzureUpstreamRef::chat_completions_url()` would produce
    /// but rooted at the mock's URI. Pass to
    /// [`AzureOpenAiBridge::with_url_override`].
    /// Test helper: build an [`AzureAuth`] for the api-key branch.
    /// Mirrors the inbound shape the production parser produces for
    /// a verbatim-string secret.
    fn api_key_auth(key: &str) -> AzureAuth {
        AzureAuth {
            api_key: Some(key.to_string()),
            bearer_token: None,
        }
    }

    /// Test helper: build an [`AzureAuth`] for the AAD-bearer branch.
    fn bearer_auth(token: &str) -> AzureAuth {
        AzureAuth {
            api_key: None,
            bearer_token: Some(token.to_string()),
        }
    }

    fn mock_chat_url(mock_uri: &str, deployment: &str) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version=2024-10-21",
            mock_uri, deployment,
        )
    }

    fn model_for_deployment(deployment: &str) -> Arc<Model> {
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "display_name": "my-azure-gpt4",
                "provider": "openai",
                "model_name": deployment,
                "provider_key_id": "11111111-1111-1111-1111-111111111111",
            }))
            .unwrap(),
        )
    }

    /// One Provider Key fronting two deployments. The URL embeds the
    /// deployment name, so the endpoint URL cache has to hold a row per
    /// deployment: keying it by Provider Key alone makes each deployment
    /// invalidate the previous one's row — every request a miss, plus a
    /// window in which a stale row answers with the *other* deployment's
    /// URL.
    ///
    /// Deliberately built without `with_url_override`: that seam bypasses
    /// the cache, so a test using it proves nothing about this. The
    /// wiremock URI reaches the same code through
    /// `AzureUpstreamRef::resolve`'s verbatim-override branch.
    #[tokio::test]
    async fn url_cache_holds_a_row_per_deployment() {
        let server = MockServer::builder().start().await;
        for deployment in ["gpt4o-prod", "gpt4o-mini"] {
            Mock::given(method("POST"))
                .and(path(format!(
                    "/openai/deployments/{deployment}/chat/completions"
                )))
                .and(query_param("api-version", "2024-10-21"))
                .respond_with(CapturingResponder::default())
                .expect(3)
                .mount(&server)
                .await;
        }

        let bridge = AzureOpenAiBridge::new();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        for _ in 0..3 {
            for deployment in ["gpt4o-prod", "gpt4o-mini"] {
                let ctx = BridgeContext::new(
                    "req-azure-1",
                    model_for_deployment(deployment),
                    sample_pk(Some(&server.uri())),
                )
                .with_resource_ids("m-1", "pk-azure-rows");
                bridge.chat(&req, &ctx).await.unwrap();
            }
        }
        // Each `.expect(3)` is verified when the server drops; a row
        // keyed without the deployment cross-serves and fails both.
    }

    /// An edited `api_base` has to reach the deployment-keyed rows: a
    /// cached URL that outlives the edit keeps sending the key's
    /// credential to the host the operator just re-pointed away from.
    #[tokio::test]
    async fn url_cache_follows_an_api_base_edit_for_the_same_key_id() {
        async fn mount(server: &MockServer) {
            Mock::given(method("POST"))
                .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
                .respond_with(CapturingResponder::default())
                .expect(1)
                .mount(server)
                .await;
        }
        let first = MockServer::builder().start().await;
        let second = MockServer::builder().start().await;
        mount(&first).await;
        mount(&second).await;

        let bridge = AzureOpenAiBridge::new();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        for host in [&first, &second] {
            let ctx = BridgeContext::new(
                "req-azure-1",
                model_for_deployment("gpt4o-prod"),
                sample_pk(Some(&host.uri())),
            )
            .with_resource_ids("m-1", "pk-azure-repoint");
            bridge.chat(&req, &ctx).await.unwrap();
        }
    }

    #[test]
    fn build_request_headers_uses_api_key_not_bearer() {
        // Critical Azure-vs-OpenAI distinction: the auth header is
        // literally `api-key`, NOT `Authorization: Bearer`.
        let headers = build_request_headers(
            &api_key_auth("az-secret-key"),
            "req-1",
            false,
            &UpstreamHeaderContext::default(),
        )
        .unwrap();
        assert_eq!(headers.get("api-key").unwrap(), "az-secret-key");
        assert!(
            !headers.contains_key("authorization"),
            "must NOT set Authorization header for Azure api-key scheme"
        );
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("x-aisix-request-id").unwrap(), "req-1");
        assert!(
            !headers.contains_key("accept"),
            "Accept header must not be set for non-streaming requests"
        );
    }

    #[test]
    fn build_request_headers_sets_sse_accept_when_streaming() {
        let headers = build_request_headers(
            &api_key_auth("az-key"),
            "req-1",
            true,
            &UpstreamHeaderContext::default(),
        )
        .unwrap();
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
    }

    #[test]
    fn default_headers_cannot_displace_the_slot_this_bridge_filled() {
        // `default_headers` is a fallback, never an override: the bridge
        // fills its credential slot first, so an entry of the same name is
        // declined. The OTHER slot is a different question — this bridge
        // does not fill `authorization` under `api-key` auth, so an
        // operator entry reaches it, which is how an Azure-shaped upstream
        // behind its own bearer-authenticated hop is reached.
        use std::collections::HashMap;
        let mut default_headers = HashMap::new();
        default_headers.insert("api-key".to_string(), "OVERRIDE-KEY".to_string());
        default_headers.insert("authorization".to_string(), "Bearer hop-token".to_string());
        let request_overrides = RequestOverrides {
            default_headers,
            ..Default::default()
        };
        let headers = build_request_headers(
            &api_key_auth("legit-key"),
            "req-1",
            false,
            &UpstreamHeaderContext::from_overrides(Some(&request_overrides)),
        )
        .unwrap();
        assert_eq!(
            headers.get("api-key").unwrap(),
            "legit-key",
            "the bridge filled this slot first, so the entry is declined"
        );
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer hop-token",
            "a slot no auth mode fills is the operator's to set"
        );

        // AAD auth fills `authorization` instead, and then it is the
        // declined one — the rule is which slot the bridge filled, not
        // which name it is.
        let headers = build_request_headers(
            &bearer_auth("aad-token"),
            "req-1",
            false,
            &UpstreamHeaderContext::from_overrides(Some(&request_overrides)),
        )
        .unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer aad-token");
        assert_eq!(headers.get("api-key").unwrap(), "OVERRIDE-KEY");
    }

    #[test]
    fn build_request_headers_default_headers_allow_custom_non_reserved() {
        use std::collections::HashMap;
        let mut default_headers = HashMap::new();
        default_headers.insert("x-custom-trace".to_string(), "trace-123".to_string());
        let request_overrides = RequestOverrides {
            default_headers,
            ..Default::default()
        };
        let headers = build_request_headers(
            &api_key_auth("k"),
            "req-1",
            false,
            &UpstreamHeaderContext::from_overrides(Some(&request_overrides)),
        )
        .unwrap();
        assert_eq!(headers.get("x-custom-trace").unwrap(), "trace-123");
    }

    #[test]
    fn build_request_headers_rejects_invalid_api_key_chars() {
        // A secret with a newline would let an operator inject extra
        // headers via the api-key value.
        let err = build_request_headers(
            &api_key_auth("legit\nx-evil: 1"),
            "req-1",
            false,
            &UpstreamHeaderContext::default(),
        )
        .unwrap_err();
        assert!(matches!(err, BridgeError::InvalidUpstreamCredentials(_)));
        assert_eq!(err.http_status(), 401);
    }

    #[test]
    fn build_request_headers_rejects_invalid_request_id_chars() {
        let err = build_request_headers(
            &api_key_auth("legit"),
            "req\nbad",
            false,
            &UpstreamHeaderContext::default(),
        )
        .unwrap_err();
        assert!(matches!(err, BridgeError::Config(_)));
    }

    #[tokio::test]
    async fn chat_with_missing_api_base_errors_before_dispatch() {
        let bridge = AzureOpenAiBridge::new();
        let ctx = BridgeContext::new("req-1", sample_model(), sample_pk(None));
        let req = ChatFormat::new("customer-facing-name", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::InvalidUpstreamConfig(msg) => {
                assert!(msg.contains("no api_base"), "got {msg}");
            }
            other => panic!("expected InvalidUpstreamConfig error, got {other:?}"),
        }
    }

    /// Azure image generation: deployment in the path, pinned
    /// `api-version`, and the caller's body relayed with `model` set to the
    /// deployment. Same route family as chat and embeddings.
    #[tokio::test]
    async fn generate_image_uses_the_deployment_images_route() {
        let server = MockServer::builder().start().await;
        let responder = CapturingResponder::default().with_response(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "created": 0,
                "data": [{"url": "https://example.invalid/cat.png"}]
            })),
        );
        Mock::given(method("POST"))
            .and(path("/openai/deployments/dalle-prod/images/generations"))
            .and(query_param("api-version", "2024-10-21"))
            .and(header("api-key", "az-key"))
            .respond_with(responder.clone())
            .expect(1)
            .mount(&server)
            .await;

        let bridge = AzureOpenAiBridge::new().with_url_override(format!(
            "{}/openai/deployments/dalle-prod/images/generations?api-version=2024-10-21",
            server.uri()
        ));
        let ctx = BridgeContext::new(
            "req-azure-image",
            model_for_deployment("dalle-prod"),
            sample_pk(Some("https://acme-west.openai.azure.com")),
        );
        let out = bridge
            .generate_image(
                &serde_json::json!({"model": "ignored", "prompt": "a cat", "n": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["data"][0]["url"], "https://example.invalid/cat.png");

        let body = responder.captured_body.lock().unwrap().clone().unwrap();
        assert_eq!(
            body.get("model").and_then(|v| v.as_str()),
            Some("dalle-prod"),
            "the body's model carries the deployment name: {body}",
        );
        assert_eq!(body.get("prompt").and_then(|v| v.as_str()), Some("a cat"));
    }

    #[tokio::test]
    async fn chat_with_empty_secret_errors_before_dispatch() {
        let bridge = AzureOpenAiBridge::new();
        // Build a PK whose api_key is empty.
        let pk: Arc<ProviderKey> = Arc::new(
            serde_json::from_str(
                r#"{"display_name": "azure-prod", "secret": "", "api_base": "https://acme-west.openai.azure.com"}"#,
            )
            .unwrap(),
        );
        let ctx = BridgeContext::new("req-1", sample_model(), pk);
        let req = ChatFormat::new("customer-facing-name", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        assert_eq!(err.http_status(), 401);
        match err {
            BridgeError::InvalidUpstreamCredentials(msg) => {
                assert!(msg.contains("api_key is empty"), "got {msg}");
            }
            other => panic!("expected InvalidUpstreamCredentials error, got {other:?}"),
        }
    }

    /// Per-test responder that records both the inbound request body
    /// AND headers so tests can assert (a) the renamed key carries the
    /// original value (Audit M2), (b) `Authorization` is absent at the
    /// wire (Audit M3 — defense in depth atop the unit-tested
    /// `build_request_headers`), and (c) the full body shape (Audit M4).
    #[derive(Clone, Default)]
    struct CapturingResponder {
        captured_body: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
        captured_headers: std::sync::Arc<std::sync::Mutex<Option<http::HeaderMap>>>,
        response_template: std::sync::Arc<std::sync::Mutex<Option<ResponseTemplate>>>,
    }

    impl CapturingResponder {
        fn with_response(self, template: ResponseTemplate) -> Self {
            *self.response_template.lock().unwrap() = Some(template);
            self
        }
    }

    impl Respond for CapturingResponder {
        fn respond(&self, req: &MockRequest) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            *self.captured_body.lock().unwrap() = Some(body);
            *self.captured_headers.lock().unwrap() = Some(req.headers.clone());
            self.response_template
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "id": "x", "model": "gpt4o-prod", "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"
                        }], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }))
                })
        }
    }

    // ─── bridge.chat() end-to-end against wiremock via url_override ────
    //
    // These tests drive the **real** `AzureOpenAiBridge::chat()` /
    // `chat_stream()` entry point. The canonical api_base
    // `https://acme-west.openai.azure.com` lets `AzureUpstreamRef::resolve`
    // succeed (strict host-suffix check passes), and
    // [`AzureOpenAiBridge::with_url_override`] rewrites the POST URL
    // to the wiremock server. So everything the bridge does at runtime
    // (header building, body building, override apply, error mapping,
    // SSE decoding, deadline handling) is exercised; only the final
    // hostname is different from production.

    /// Azure embeddings dispatch: deployment in the path, pinned
    /// `api-version`, `api-key` header — and an OpenAI-shaped body/response.
    ///
    /// Before this the bridge had no `embed`, so the trait default answered
    /// "this provider does not support embeddings" and an Azure-only estate
    /// could not serve `/v1/embeddings` at all.
    #[tokio::test]
    async fn embed_dispatch_uses_the_deployment_embeddings_route() {
        let server = MockServer::builder().start().await;
        let responder = CapturingResponder::default().with_response(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "model": "text-embed-prod",
                "data": [{"object": "embedding", "index": 0, "embedding": [0.5, 0.25]}],
                "usage": {"prompt_tokens": 4, "total_tokens": 4}
            })),
        );
        Mock::given(method("POST"))
            .and(path("/openai/deployments/text-embed-prod/embeddings"))
            .and(query_param("api-version", "2024-10-21"))
            .and(header("api-key", "az-key"))
            .respond_with(responder.clone())
            .expect(1)
            .mount(&server)
            .await;

        let bridge = AzureOpenAiBridge::new().with_url_override(format!(
            "{}/openai/deployments/text-embed-prod/embeddings?api-version=2024-10-21",
            server.uri()
        ));
        let mut ctx = BridgeContext::new(
            "req-azure-embed",
            model_for_deployment("text-embed-prod"),
            sample_pk(Some("https://acme-west.openai.azure.com")),
        );
        ctx.model = model_for_deployment("text-embed-prod");
        let req = EmbeddingRequest {
            model: "ignored-by-azure".to_string(),
            input: vec!["hello".to_string()],
            input_was_single: true,
            encoding_format: None,
            dimensions: None,
            input_type: None,
            embedding_types: None,
            token_input: None,
            extra: Default::default(),
        };
        let out = bridge.embed(&req, &ctx).await.unwrap();
        assert_eq!(out.data.len(), 1);
        match &out.data[0].embedding {
            aisix_gateway::EmbeddingVector::Float(v) => assert_eq!(v, &vec![0.5_f32, 0.25_f32]),
            other => panic!("expected a float vector, got {other:?}"),
        }
        assert_eq!(out.usage.prompt_tokens, 4);

        // The caller sent a single string, so the wire body must carry a
        // single string — the same "both pass through" contract the OpenAI
        // bridge honours (#162).
        let body = responder.captured_body.lock().unwrap().clone().unwrap();
        assert_eq!(
            body.get("input").and_then(|v| v.as_str()),
            Some("hello"),
            "single-string input must not be array-wrapped on the wire: {body}",
        );
        assert_eq!(
            body.get("model").and_then(|v| v.as_str()),
            Some("text-embed-prod"),
            "the body's model carries the deployment name, matching chat()",
        );
    }

    #[tokio::test]
    async fn chat_dispatch_sends_api_key_header_and_deployment_url() {
        let server = MockServer::builder().start().await;
        // Mock asserts: POST + path with deployment + api-version
        // query + api-key header carrying the literal secret.
        let responder = CapturingResponder::default().with_response(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-azure-1",
                "model": "gpt4o-prod",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi from azure"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
            })),
        );
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .and(query_param("api-version", "2024-10-21"))
            .and(header("api-key", "az-key"))
            .and(header("content-type", "application/json"))
            .respond_with(responder.clone())
            .expect(1)
            .mount(&server)
            .await;

        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let chat = bridge.chat(&req, &ctx).await.unwrap();
        assert_eq!(chat.message.content_str(), "hi from azure");

        // Audit M3: assert `Authorization` is absent on the wire (not
        // just absent from the helper's output).
        let captured = responder.captured_headers.lock().unwrap().clone().unwrap();
        assert!(
            !captured.contains_key("authorization"),
            "Authorization must not be on the wire; headers={captured:?}"
        );
        assert_eq!(
            captured.get("api-key").and_then(|v| v.to_str().ok()),
            Some("az-key"),
            "api-key must reach the wire with the literal secret"
        );
    }

    #[tokio::test]
    async fn chat_body_full_shape_on_the_wire() {
        // Audit M4: assert the full body shape, not just the `model`
        // field — `messages` array present, `stream: false` for
        // non-streaming, content matches the request.
        let server = MockServer::builder().start().await;
        let responder = CapturingResponder::default();
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(responder.clone())
            .expect(1)
            .mount(&server)
            .await;

        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new(
            "my-azure-gpt4",
            vec![
                ChatMessage::developer("follow application instructions"),
                ChatMessage::user("hi"),
            ],
        );
        bridge.chat(&req, &ctx).await.unwrap();

        let body = responder.captured_body.lock().unwrap().clone().unwrap();
        assert_eq!(
            body.get("model").and_then(|v| v.as_str()),
            Some("gpt4o-prod"),
            "body.model must = deployment name; got body={body}"
        );
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 2, "exactly two messages; got body={body}");
        assert_eq!(
            messages[0].get("role").and_then(|v| v.as_str()),
            Some("system"),
            "Azure API version 2024-10-21 receives developer instructions as system"
        );
        assert_eq!(
            messages[0].get("content").and_then(|v| v.as_str()),
            Some("follow application instructions")
        );
        assert_eq!(
            messages[1].get("role").and_then(|v| v.as_str()),
            Some("user")
        );
        assert_eq!(
            messages[1].get("content").and_then(|v| v.as_str()),
            Some("hi")
        );
        assert_eq!(
            body.get("stream").and_then(|v| v.as_bool()),
            Some(false),
            "stream: false for chat (non-streaming); got body={body}"
        );
    }

    #[tokio::test]
    async fn chat_tolerates_content_filter_results_in_response() {
        // Azure-specific: responses include `prompt_filter_results`
        // and `content_filter_results` blocks. The reused OpenAi
        // wire parsers must not blow up on these extension fields —
        // they don't set `deny_unknown_fields`, so serde discards.
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-azure-cf",
                "model": "gpt4o-prod",
                "prompt_filter_results": [{
                    "prompt_index": 0,
                    "content_filter_results": {
                        "hate": {"filtered": false, "severity": "safe"},
                        "self_harm": {"filtered": false, "severity": "safe"},
                        "sexual": {"filtered": false, "severity": "safe"},
                        "violence": {"filtered": false, "severity": "safe"}
                    }
                }],
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "filtered ok"},
                    "finish_reason": "stop",
                    "content_filter_results": {
                        "hate": {"filtered": false, "severity": "safe"}
                    }
                }],
                "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let chat = bridge.chat(&req, &ctx).await.unwrap();
        assert_eq!(chat.message.content_str(), "filtered ok");
        assert_eq!(chat.usage.total_tokens, 7);
    }

    #[tokio::test]
    async fn chat_applies_param_renames_to_outbound_body() {
        // Audit M2: assert the renamed key carries the original VALUE,
        // not just that the key swap happened.
        let server = MockServer::builder().start().await;
        let responder = CapturingResponder::default();
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(responder.clone())
            .expect(1)
            .mount(&server)
            .await;

        let overrides_json = r#""request": {"param_renames": {"max_tokens": "max_completion_tokens"}, "param_constraints": null, "default_body_fields": {}, "default_headers": {}}"#;
        let pk = sample_pk_with_overrides("https://acme-west.openai.azure.com", overrides_json);
        let ctx = BridgeContext::new("r", sample_model(), pk);
        let req: ChatFormat = serde_json::from_str(
            r#"{"model": "my-azure-gpt4", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 100}"#,
        )
        .unwrap();
        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        bridge.chat(&req, &ctx).await.unwrap();

        let body = responder.captured_body.lock().unwrap().clone().unwrap();
        assert!(
            body.get("max_tokens").is_none(),
            "original max_tokens key must be gone; body={body}"
        );
        assert_eq!(
            body.get("max_completion_tokens").and_then(|v| v.as_u64()),
            Some(100),
            "renamed key must carry the original value of 100; body={body}"
        );
    }

    #[tokio::test]
    async fn chat_maps_upstream_400_to_the_upstream_message() {
        // The caller receives Azure's own error text.
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "The API deployment for 'gpt4o-prod' does not exist on resource 'acme-west'",
            ))
            .expect(1)
            .mount(&server)
            .await;
        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::UpstreamStatus {
                status, message, ..
            } => {
                assert_eq!(status, 400);
                assert_eq!(
                    message,
                    "The API deployment for 'gpt4o-prod' does not exist on resource 'acme-west'"
                );
            }
            other => panic!("expected UpstreamStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_maps_404_to_the_upstream_message() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_string("operator-internal: foo-bar"))
            .expect(1)
            .mount(&server)
            .await;
        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::UpstreamStatus {
                status, message, ..
            } => {
                assert_eq!(status, 404);
                assert_eq!(message, "operator-internal: foo-bar");
            }
            other => panic!("expected UpstreamStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_maps_429_with_retry_after_and_the_upstream_message() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_string("rate limited — quota for deployment gpt4o-prod exceeded"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::UpstreamStatus {
                status,
                retry_after,
                message,
                ..
            } => {
                assert_eq!(status, 429);
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(30)));
                assert_eq!(
                    message,
                    "rate limited — quota for deployment gpt4o-prod exceeded"
                );
            }
            other => panic!("expected UpstreamStatus with retry_after, got {other:?}"),
        }
    }

    /// Copilot review (PR #323): the translation table keys on Azure's
    /// `error.code`, not its `type`. For OpenAI-compatible tokens that Azure
    /// inherits unchanged (e.g. `rate_limit_exceeded`), the bridge
    /// must populate BOTH `parsed.kind` AND `parsed.code` from the
    /// upstream — otherwise the downstream OpenAI client receives
    /// `error.type=rate_limit_exceeded` but `error.code=null`, which
    /// is exactly the SDK-retry-logic break that issue #322 calls out.
    #[tokio::test]
    async fn chat_429_preserves_openai_compatible_code_for_sdk_retry() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_raw(
                br#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#.as_slice(),
                "application/json",
            ))
            .mount(&server)
            .await;
        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::UpstreamStatus { parsed, .. } => {
                let parsed = parsed.expect("envelope parsed");
                assert_eq!(parsed.kind.as_deref(), Some("rate_limit_exceeded"));
                assert_eq!(
                    parsed.code.as_deref(),
                    Some("rate_limit_exceeded"),
                    "OpenAI-compat code must flow through view.code for SDK retry"
                );
            }
            other => panic!("expected UpstreamStatus, got {other:?}"),
        }
    }

    /// A non-JSON body (HTML error page from Azure Front Door, etc.)
    /// yields no parsed view.
    #[tokio::test]
    async fn chat_400_non_json_body_has_no_parsed_view() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(
                b"<html><body>403 Forbidden by Front Door</body></html>".as_slice(),
                "text/html",
            ))
            .mount(&server)
            .await;
        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::UpstreamStatus { parsed, .. } => {
                assert!(
                    parsed.is_none(),
                    "non-JSON body must not produce a parsed view; got {parsed:?}"
                );
            }
            other => panic!("expected UpstreamStatus, got {other:?}"),
        }
    }

    /// Audit fix (PR #323 follow-up): the `inner_error` casing
    /// variant is what most Azure docs show, but Azure ALSO emits
    /// `innererror` (smushed) on some endpoints. Both must be
    /// recognised by the parser.
    #[tokio::test]
    async fn chat_400_with_innererror_smushed_casing_also_lifts_kind() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(
                br#"{"error":{"code":"invalid_request_error","message":"blocked","innererror":{"code":"ResponsibleAIPolicyViolation"}}}"#.as_slice(),
                "application/json",
            ))
            .mount(&server)
            .await;
        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::UpstreamStatus { parsed, .. } => {
                let parsed = parsed.expect("innererror parsed");
                assert_eq!(parsed.kind.as_deref(), Some("ResponsibleAIPolicyViolation"));
                // Azure's own outer code stays, for a relay to carry as sent.
                assert_eq!(parsed.code.as_deref(), Some("invalid_request_error"));
            }
            other => panic!("expected UpstreamStatus, got {other:?}"),
        }
    }

    /// Audit fix (PR #323 MEDIUM-2): structured-parse path —
    /// Azure-specific `inner_error.code = ResponsibleAIPolicyViolation`
    /// must surface as `parsed.kind`, not be flattened under the outer
    /// `error.code`. `wire: AzureOpenAI` must be set so the
    /// translation layer picks the Azure-aware code map.
    /// `parsed.message` stays `None` (deployment id leak).
    #[tokio::test]
    async fn chat_400_with_inner_error_responsible_ai_lifts_kind() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(
                br#"{"error":{"code":"invalid_request_error","message":"blocked","inner_error":{"code":"ResponsibleAIPolicyViolation"}}}"#.as_slice(),
                "application/json",
            ))
            .mount(&server)
            .await;
        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::UpstreamStatus {
                status,
                wire,
                parsed,
                ..
            } => {
                assert_eq!(status, 400);
                assert_eq!(wire, aisix_gateway::UpstreamWire::AzureOpenAI);
                let parsed = parsed.expect("inner_error parsed");
                assert_eq!(parsed.kind.as_deref(), Some("ResponsibleAIPolicyViolation"));
                assert_eq!(parsed.message.as_deref(), Some("blocked"));
            }
            other => panic!("expected UpstreamStatus, got {other:?}"),
        }
    }

    /// Audit L3: `chat_against_full_bridge_dispatch` calls the real
    /// Azure DNS (`acme-west.openai.azure.com`). Marked `#[ignore]`
    /// because (a) CI runners with corporate proxies may resolve it
    /// to unexpected hosts, (b) it takes wall-clock time to fail. Run
    /// manually with `cargo test -- --ignored` to sanity-check that
    /// the bridge reaches the network layer end-to-end.
    #[tokio::test]
    #[ignore = "calls real Azure DNS; run with `cargo test -- --ignored`"]
    async fn chat_against_real_azure_reaches_network() {
        let bridge = AzureOpenAiBridge::new();
        let ctx = BridgeContext::new(
            "req-1",
            sample_model(),
            sample_pk(Some("https://acme-west.openai.azure.com")),
        );
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::Transport(_) | BridgeError::UpstreamStatus { .. } => {
                // expected — we reached the network
            }
            other => panic!(
                "expected Transport or UpstreamStatus (proving we reached network); got {other:?}"
            ),
        }
    }

    /// D6 audit HIGH-1 regression (from #313 skeleton): dispatch must
    /// read the upstream deployment from `ctx.model.model_name`, NOT
    /// from `req.model`. `req.model` is the customer-typed display
    /// name; resolving off it would produce
    /// `/openai/deployments/customer-facing-name/...` — 404 from
    /// Azure every time.
    #[tokio::test]
    async fn chat_ignores_req_model_and_uses_ctx_model_name() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "x", "model": "gpt4o-prod", "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        // req.model is the customer-facing display name. The URL the
        // bridge dispatches to must use ctx.model.model_name
        // ("gpt4o-prod") not req.model ("customer-facing-name").
        let req = ChatFormat::new("customer-facing-name", vec![ChatMessage::user("hi")]);
        bridge.chat(&req, &ctx).await.unwrap();
        // Path matcher on `gpt4o-prod` proves dispatch used model_name.
    }

    #[tokio::test]
    async fn chat_stream_yields_chunks_until_done_marker_with_inline_content_filters() {
        // Audit H3: SSE stream chunks can carry `prompt_filter_results`
        // (top-level) and `content_filter_results` (per-choice) — the
        // reused OpenAiStreamChunk parsers must tolerate both without
        // breaking deserialization.
        let server = MockServer::builder().start().await;
        let sse_body = concat!(
            // chunk 1 — top-level prompt_filter_results + empty choices (Azure prelude)
            "data: {\"id\":\"x\",\"model\":\"gpt4o-prod\",\"prompt_filter_results\":[{\"prompt_index\":0,\"content_filter_results\":{\"hate\":{\"filtered\":false,\"severity\":\"safe\"}}}],\"choices\":[]}\n\n",
            // chunk 2 — content delta with per-choice content_filter_results
            "data: {\"id\":\"x\",\"model\":\"gpt4o-prod\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"content_filter_results\":{\"hate\":{\"filtered\":false,\"severity\":\"safe\"}},\"finish_reason\":null}]}\n\n",
            // chunk 3 — content delta with finish_reason
            "data: {\"id\":\"x\",\"model\":\"gpt4o-prod\",\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let responder = CapturingResponder::default().with_response(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        );
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .and(header("accept", "text/event-stream"))
            .respond_with(responder.clone())
            .expect(1)
            .mount(&server)
            .await;

        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let ctx = canonical_test_ctx();
        let req = ChatFormat::new(
            "my-azure-gpt4",
            vec![
                ChatMessage::developer("follow application instructions"),
                ChatMessage::user("hi"),
            ],
        );
        let mut stream = bridge.chat_stream(&req, &ctx).await.unwrap();
        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.unwrap());
        }
        // Audit H3 specifically: chunk 1 has empty choices — yielded
        // chunk's delta is the default. Chunk 2 carries "hello".
        // Chunk 3 carries " world" + finish_reason=stop.
        assert!(
            chunks.len() >= 3,
            "expected at least 3 chunks (incl. Azure prelude); got {chunks:?}"
        );
        let content_chunk = chunks
            .iter()
            .find(|c| c.delta.content.as_deref() == Some("hello"))
            .expect("must find a chunk with content=hello");
        assert_eq!(
            content_chunk.delta.role,
            Some(aisix_gateway::Role::Assistant)
        );
        let last = chunks.last().unwrap();
        assert!(
            last.finish_reason.is_some(),
            "last chunk must carry finish_reason"
        );
        let body = responder.captured_body.lock().unwrap().clone().unwrap();
        assert_eq!(
            body.get("messages"),
            Some(&serde_json::json!([
                {"role": "system", "content": "follow application instructions"},
                {"role": "user", "content": "hi"}
            ])),
            "streaming Azure requests map developer instructions to system: {body}"
        );
    }

    #[tokio::test]
    async fn chat_stream_enforces_per_chunk_deadline() {
        // Audit H2: a slow / hanging stream body must not wedge after
        // headers arrive. The bridge enforces the deadline on each
        // per-chunk wait, not just on the initial POST.
        let server = MockServer::builder().start().await;
        // 1s delay before the SSE body is emitted. With a 200ms
        // deadline, the bridge should surface a Timeout from the
        // per-chunk wait — the headers arrive instantly (mock
        // responds with 200), but the body delivery sits idle.
        Mock::given(method("POST"))
            .and(path("/openai/deployments/gpt4o-prod/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_delay(std::time::Duration::from_secs(2))
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&server)
            .await;

        let bridge =
            AzureOpenAiBridge::new().with_url_override(mock_chat_url(&server.uri(), "gpt4o-prod"));
        let mut ctx = canonical_test_ctx();
        ctx.deadline = Some(std::time::Duration::from_millis(200));
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        // The initial POST happens within deadline (mock 200 OK), but
        // wiremock's set_delay applies to the response *body*, so
        // either the initial with_deadline or the per-chunk timeout
        // fires. Either way the bridge must emit a Timeout.
        let result = bridge.chat_stream(&req, &ctx).await;
        match result {
            Ok(mut stream) => {
                let next = stream.next().await;
                match next {
                    Some(Err(BridgeError::Timeout { .. })) => {}
                    other => panic!("expected per-chunk Timeout, got {other:?}"),
                }
            }
            Err(BridgeError::Timeout { .. }) => {
                // Acceptable: initial POST already timed out.
            }
            Err(other) => panic!("expected Timeout, got {other:?}"),
        }
    }

    /// #1122: `stream_timeout` is defined as the maximum gap
    /// *between* chunks (see `Model::stream_timeout`), but the budget was
    /// applied as `timeout_at(started + d)` — one absolute instant reused
    /// for every chunk, i.e. a ceiling on the whole response. A long but
    /// perfectly healthy stream was cut off with a Timeout once its total
    /// duration passed a single gap's budget.
    ///
    /// Here every gap (80ms) is comfortably inside the budget (250ms)
    /// while the total (~320ms) is past it. Per-chunk semantics deliver
    /// the whole stream; the absolute deadline truncated it.
    #[tokio::test]
    async fn stream_budget_bounds_each_chunk_gap_not_the_whole_response() {
        use futures::stream::StreamExt as _;

        let gap = Duration::from_millis(80);
        let budget = Duration::from_millis(250);
        let frames: Vec<bytes::Bytes> = vec![
            bytes::Bytes::from(
                "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n",
            ),
            bytes::Bytes::from(
                "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"b\"},\"finish_reason\":null}]}\n\n",
            ),
            bytes::Bytes::from(
                "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"c\"},\"finish_reason\":\"stop\"}]}\n\n",
            ),
            bytes::Bytes::from("data: [DONE]\n\n"),
        ];
        let byte_stream = futures::stream::iter(frames).then(move |b| async move {
            tokio::time::sleep(gap).await;
            Ok::<bytes::Bytes, reqwest::Error>(b)
        });

        let started = Instant::now();
        let mut stream = Box::pin(build_chunk_stream(
            byte_stream,
            None,
            None,
            "azure-openai",
            "req-per-chunk".to_string(),
            Some(budget),
        ));

        let mut content = String::new();
        while let Some(item) = stream.next().await {
            let chunk = item.expect("no chunk may time out — every gap is within budget");
            if let Some(text) = chunk.delta.content.as_deref() {
                content.push_str(text);
            }
        }

        assert_eq!(content, "abc", "the whole stream must be delivered");
        assert!(
            started.elapsed() > budget,
            "the test is only meaningful if the total run outlasts one gap's \
             budget (elapsed {:?}, budget {budget:?})",
            started.elapsed()
        );
    }

    /// The same contract as the test above, but driven through the public
    /// `chat_stream` entry point instead of `build_chunk_stream`, so it
    /// also covers `chat_stream` actually forwarding `ctx.deadline` — the
    /// helper-level test alone would still pass if that wiring were
    /// dropped.
    ///
    /// Needs an upstream that emits frames on a schedule, which wiremock
    /// cannot do (`set_delay` delays the response as a whole, which is
    /// exactly the distinction under test), so this hand-rolls a one-shot
    /// SSE server: no `content-length`, `connection: close`, body framed
    /// by EOF.
    #[tokio::test]
    async fn chat_stream_delivers_a_long_stream_whose_gaps_stay_within_budget() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let gap = Duration::from_millis(80);
        let budget = Duration::from_millis(250);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the request headers so the client's write completes.
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            sock.flush().await.unwrap();
            for text in ["a", "b", "c"] {
                tokio::time::sleep(gap).await;
                let frame = format!(
                    "data: {{\"id\":\"x\",\"model\":\"m\",\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\n"
                );
                sock.write_all(frame.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
            tokio::time::sleep(gap).await;
            sock.write_all(b"data: [DONE]\n\n").await.unwrap();
            sock.flush().await.unwrap();
            sock.shutdown().await.unwrap();
        });

        let bridge = AzureOpenAiBridge::new()
            .with_url_override(mock_chat_url(&format!("http://{addr}"), "gpt4o-prod"));
        let mut ctx = canonical_test_ctx();
        ctx.deadline = Some(budget);
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);

        let started = Instant::now();
        let mut stream = bridge.chat_stream(&req, &ctx).await.expect("stream opens");
        let mut content = String::new();
        while let Some(item) = stream.next().await {
            let chunk = item.expect("no chunk may time out — every gap is within budget");
            if let Some(text) = chunk.delta.content.as_deref() {
                content.push_str(text);
            }
        }

        assert_eq!(content, "abc", "the whole stream must be delivered");
        assert!(
            started.elapsed() > budget,
            "the test is only meaningful if the total run outlasts one gap's \
             budget (elapsed {:?}, budget {budget:?})",
            started.elapsed()
        );
    }

    /// The counterpart that actually pins the wiring: response headers
    /// arrive immediately, so the connect-phase `with_deadline` is already
    /// satisfied, and only then does a single gap exceed the budget. The
    /// per-chunk timeout is therefore the only thing that can fire — so
    /// this test fails if `chat_stream` ever stops forwarding
    /// `ctx.deadline` into the stream, which the delivery test above
    /// (and a helper-level test) would not catch.
    #[tokio::test]
    async fn chat_stream_times_out_when_one_gap_exceeds_the_budget() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let budget = Duration::from_millis(150);
        let stall = Duration::from_millis(700);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            // Headers first and flushed, so the bridge commits the stream.
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            sock.flush().await.unwrap();
            // ...then stall well past the budget before the first frame.
            tokio::time::sleep(stall).await;
            let _ = sock.write_all(b"data: [DONE]\n\n").await;
        });

        let bridge = AzureOpenAiBridge::new()
            .with_url_override(mock_chat_url(&format!("http://{addr}"), "gpt4o-prod"));
        let mut ctx = canonical_test_ctx();
        ctx.deadline = Some(budget);
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);

        let mut stream = bridge
            .chat_stream(&req, &ctx)
            .await
            .expect("headers arrive within the budget, so the stream opens");
        match stream.next().await {
            Some(Err(BridgeError::Timeout { .. })) => {}
            other => panic!("expected a per-chunk Timeout, got {other:?}"),
        }
    }

    // ─── AAD (Entra ID) auth scheme (D6.6) ─────────────────────────

    /// JSON-shaped Azure secret triggering the AAD branch.
    fn aad_secret_json() -> String {
        r#"{
            "tenant_id": "tenant-uuid-aaa",
            "client_id": "client-uuid-bbb",
            "client_secret": "aad-secret-rotation-managed"
        }"#
        .to_string()
    }

    #[test]
    fn azure_secret_parses_verbatim_string_as_api_key() {
        // Backward compat: existing pre-D6.6 deployments encode the
        // resource api-key as a bare string.
        let parsed = AzureSecret::parse("legacy-api-key-string").unwrap();
        match parsed {
            AzureSecret::ApiKey(k) => assert_eq!(k, "legacy-api-key-string"),
            AzureSecret::Aad(_) => panic!("verbatim string must parse as api-key, not AAD"),
        }
    }

    #[test]
    fn azure_secret_parses_json_object_as_aad_credentials() {
        let parsed = AzureSecret::parse(&aad_secret_json()).unwrap();
        match parsed {
            AzureSecret::Aad(creds) => {
                assert_eq!(creds.tenant_id, "tenant-uuid-aaa");
                assert_eq!(creds.client_id, "client-uuid-bbb");
                assert_eq!(creds.client_secret, "aad-secret-rotation-managed");
            }
            AzureSecret::ApiKey(_) => panic!("JSON-shaped secret must parse as AAD, not api-key"),
        }
    }

    #[test]
    fn azure_secret_rejects_empty_secret() {
        let err = AzureSecret::parse("   ").unwrap_err();
        assert!(matches!(err, BridgeError::InvalidUpstreamCredentials(_)));
        assert_eq!(err.http_status(), 401);
    }

    #[test]
    fn azure_secret_rejects_json_missing_required_aad_fields() {
        let err = AzureSecret::parse(r#"{"tenant_id":"t"}"#).unwrap_err();
        assert_eq!(err.http_status(), 401);
        match err {
            BridgeError::InvalidUpstreamCredentials(msg) => {
                // Message must NOT echo raw secret bytes (audit-aware).
                assert!(msg.contains("looks JSON-shaped"));
                assert!(!msg.contains("tenant-uuid-aaa"));
            }
            other => panic!("expected InvalidUpstreamCredentials, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_with_aad_secret_mints_token_and_sets_authorization_bearer_header() {
        // End-to-end pin: AAD JSON secret → token mint via wiremock
        // AAD endpoint → chat POST to Azure OpenAI endpoint carrying
        // Authorization: Bearer <minted-token> (NOT api-key:).
        let aad_server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "aad.bearer.test-token",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&aad_server)
            .await;

        let azure_server = MockServer::builder().start().await;
        let captured_headers: std::sync::Arc<std::sync::Mutex<Option<http::HeaderMap>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_responder = captured_headers.clone();
        Mock::given(method("POST"))
            .respond_with(move |req: &wiremock::Request| {
                *captured_for_responder.lock().unwrap() = Some(req.headers.clone());
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "chatcmpl-aad-test",
                    "object": "chat.completion",
                    "created": 1700000000_i64,
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "hello via aad"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 4, "completion_tokens": 6, "total_tokens": 10}
                }))
            })
            .mount(&azure_server)
            .await;

        let bridge = AzureOpenAiBridge::new()
            .with_url_override(mock_chat_url(&azure_server.uri(), "gpt4o-prod"))
            .with_aad_token_endpoint_override(format!("{}/oauth2/v2.0/token", aad_server.uri()));
        let ctx = BridgeContext::new(
            "req-azure-1",
            sample_model(),
            sample_pk_with_aad_secret("https://acme-west.openai.azure.com"),
        );
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let resp = bridge.chat(&req, &ctx).await.unwrap();
        assert_eq!(resp.message.content_str(), "hello via aad");

        let headers = captured_headers
            .lock()
            .unwrap()
            .clone()
            .expect("headers captured");
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer aad.bearer.test-token",
            "AAD path must send Authorization: Bearer <minted-token>"
        );
        assert!(
            !headers.contains_key("api-key"),
            "AAD path must NOT send api-key: header (mutually exclusive auth schemes)"
        );
    }

    fn chunks_of(body: &str) -> impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> {
        futures::stream::iter(vec![Ok(bytes::Bytes::copy_from_slice(body.as_bytes()))])
    }

    /// EOF with no terminal is a failure; `[DONE]` or a finish reason is the
    /// terminal.
    #[tokio::test]
    async fn a_stream_that_ends_with_no_terminal_fails() {
        const CONTENT: &str =
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n";
        const FINISH: &str = "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        // `n: 2`: a finish reason ends one choice, so choice 1 still open
        // after choice 0 finished is a truncation.
        const SECOND: &str =
            "data: {\"id\":\"c\",\"choices\":[{\"index\":1,\"delta\":{\"content\":\"yo\"}}]}\n\n";
        const SECOND_FINISH: &str = "data: {\"id\":\"c\",\"choices\":[{\"index\":1,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        for (body, aborted) in [
            (CONTENT.to_string(), true),
            (format!("{CONTENT}{FINISH}"), false),
            (format!("{CONTENT}data: [DONE]\n\n"), false),
            (format!("{CONTENT}data: [DONE]\n"), false),
            (format!("{CONTENT}{SECOND}{FINISH}"), true),
            (format!("{CONTENT}{SECOND}{FINISH}{SECOND_FINISH}"), false),
            (format!("{CONTENT}{SECOND}{FINISH}data: [DONE]\n\n"), false),
        ] {
            let mut stream = Box::pin(build_chunk_stream(
                chunks_of(&body),
                None,
                None,
                "azure-openai",
                "req".to_string(),
                None,
            ));
            let mut failure = None;
            while let Some(item) = stream.next().await {
                if let Err(e) = item {
                    failure = Some(e);
                }
            }
            assert_eq!(
                matches!(failure, Some(BridgeError::StreamAborted)),
                aborted,
                "{body:?}: {failure:?}"
            );
        }
    }

    /// A connection that resets after the finish reason lost nothing the
    /// caller needs: the answer stands. One that resets before it is a failure.
    #[tokio::test]
    async fn a_reset_after_the_finish_reason_does_not_fail_the_stream() {
        const CONTENT: &str =
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n";
        const FINISH: &str = "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        // 一个返回半截 chunked 响应体就断开的上游，才能拿到真实的 reqwest 读错误。
        async fn relay(body: &'static [u8]) -> Option<BridgeError> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
                let chunk = format!("{:x}\r\n", body.len());
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(chunk.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
                socket.write_all(b"\r\n").await.unwrap();
                socket.flush().await.unwrap();
            });
            let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
            let mut stream = Box::pin(build_chunk_stream(
                resp.bytes_stream(),
                None,
                None,
                "azure-openai",
                "req".to_string(),
                None,
            ));
            let mut failure = None;
            while let Some(item) = stream.next().await {
                if let Err(e) = item {
                    failure = Some(e);
                }
            }
            failure
        }
        let whole: &'static str = Box::leak(format!("{CONTENT}{FINISH}").into_boxed_str());
        let answered = relay(whole.as_bytes()).await;
        assert!(answered.is_none(), "{answered:?}");
        let cut = relay(CONTENT.as_bytes()).await;
        assert!(matches!(cut, Some(BridgeError::Transport(_))), "{cut:?}");
    }

    /// A mint that answers in time still spends the attempt's deadline: the
    /// upstream call gets what is left of it, so the two together cannot take
    /// twice the target's timeout.
    #[tokio::test]
    async fn a_mint_and_its_upstream_call_share_one_deadline() {
        let delay = std::time::Duration::from_millis;
        // 两段各自都在期限内，加起来超了：第二段只能用第一段剩下的时间。
        for (mint, upstream, times_out) in [(350, 350, true), (50, 50, false)] {
            let aad_server = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .and(path("/oauth2/v2.0/token"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(delay(mint))
                        .set_body_json(
                            serde_json::json!({"access_token": "t", "expires_in": 3600}),
                        ),
                )
                .mount(&aad_server)
                .await;
            let azure_server = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(delay(upstream))
                        .set_body_json(serde_json::json!({
                            "id": "chatcmpl-1",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "hi"},
                                "finish_reason": "stop"
                            }]
                        })),
                )
                .mount(&azure_server)
                .await;
            let ctx = BridgeContext::new(
                "req-azure-1",
                sample_model(),
                sample_pk_with_aad_secret("https://acme-west.openai.azure.com"),
            )
            .with_deadline(delay(500));
            let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
            // A fresh bridge per call, so each one mints rather than reading the cache.
            let bridge = || {
                AzureOpenAiBridge::new()
                    .with_url_override(mock_chat_url(&azure_server.uri(), "gpt4o-prod"))
                    .with_aad_token_endpoint_override(format!(
                        "{}/oauth2/v2.0/token",
                        aad_server.uri()
                    ))
            };
            let buffered = bridge().chat(&req, &ctx).await;
            assert_eq!(
                matches!(buffered, Err(BridgeError::Timeout { .. })),
                times_out,
                "{mint}+{upstream}: {buffered:?}"
            );
            let streamed = bridge().chat_stream(&req, &ctx).await;
            assert_eq!(
                matches!(streamed, Err(BridgeError::Timeout { .. })),
                times_out,
                "{mint}+{upstream} streamed"
            );
        }
    }

    /// The request's deadline covers the token mint: a stalled token endpoint
    /// answers this attempt with a timeout it can fail over from, on the chat
    /// and the streaming path alike, and the next request mints in its turn.
    #[tokio::test]
    async fn a_stalled_token_mint_times_out_under_the_request_deadline() {
        let aad_server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(30))
                    .set_body_json(serde_json::json!({"access_token": "late", "expires_in": 3600})),
            )
            .up_to_n_times(2)
            .mount(&aad_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "next",
                "expires_in": 3600
            })))
            .mount(&aad_server)
            .await;
        let azure_server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi"},
                    "finish_reason": "stop"
                }]
            })))
            .mount(&azure_server)
            .await;
        let bridge = AzureOpenAiBridge::new()
            .with_url_override(mock_chat_url(&azure_server.uri(), "gpt4o-prod"))
            .with_aad_token_endpoint_override(format!("{}/oauth2/v2.0/token", aad_server.uri()));
        let ctx = BridgeContext::new(
            "req-azure-1",
            sample_model(),
            sample_pk_with_aad_secret("https://acme-west.openai.azure.com"),
        )
        .with_deadline(std::time::Duration::from_millis(200));
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let budget = std::time::Duration::from_secs(5);

        let buffered = tokio::time::timeout(budget, bridge.chat(&req, &ctx))
            .await
            .expect("the deadline ends the wait");
        assert!(
            matches!(buffered, Err(BridgeError::Timeout { .. })),
            "{buffered:?}"
        );
        let streamed = tokio::time::timeout(budget, bridge.chat_stream(&req, &ctx))
            .await
            .expect("the deadline ends the wait");
        assert!(matches!(streamed, Err(BridgeError::Timeout { .. })));

        bridge
            .chat(&req, &ctx)
            .await
            .expect("the gate was given back");
    }

    #[tokio::test]
    async fn chat_with_aad_secret_caches_minted_token_across_calls() {
        // Three back-to-back chats with the same AAD secret must
        // share the cache slot — only one token-mint trip.
        let aad_server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "aad.cached-token",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .expect(1) // critical: must be called EXACTLY ONCE across 3 chats
            .mount(&aad_server)
            .await;

        let azure_server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "x", "object": "chat.completion", "created": 1700000000_i64,
                "model": "gpt-4o",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .mount(&azure_server)
            .await;

        let bridge = AzureOpenAiBridge::new()
            .with_url_override(mock_chat_url(&azure_server.uri(), "gpt4o-prod"))
            .with_aad_token_endpoint_override(format!("{}/oauth2/v2.0/token", aad_server.uri()));
        let ctx = BridgeContext::new(
            "req-azure-1",
            sample_model(),
            sample_pk_with_aad_secret("https://acme-west.openai.azure.com"),
        );
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        for _ in 0..3 {
            bridge.chat(&req, &ctx).await.unwrap();
        }
        // wiremock's `expect(1)` on the AAD mock asserts on server
        // drop that only one mint trip happened across three chats.
    }

    #[tokio::test]
    async fn chat_with_aad_secret_aad_4xx_surfaces_before_azure_call() {
        // AAD 401 bubbles out as Config (operator-actionable) without
        // ever calling the Azure OpenAI endpoint.
        let aad_server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string(
                r#"{"error":"invalid_client","error_description":"AADSTS7000215"}"#,
            ))
            .mount(&aad_server)
            .await;

        let azure_server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0) // must NOT be called
            .mount(&azure_server)
            .await;

        let bridge = AzureOpenAiBridge::new()
            .with_url_override(mock_chat_url(&azure_server.uri(), "gpt4o-prod"))
            .with_aad_token_endpoint_override(format!("{}/oauth2/v2.0/token", aad_server.uri()));
        let ctx = BridgeContext::new(
            "req-azure-1",
            sample_model(),
            sample_pk_with_aad_secret("https://acme-west.openai.azure.com"),
        );
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let err = bridge.chat(&req, &ctx).await.unwrap_err();
        match err {
            BridgeError::Config(msg) => {
                assert!(msg.contains("invalid_client"));
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    /// Audit LOW (audit-aigw-388-azure-aad): chat_stream must also
    /// resolve auth via the AAD path. The streaming code calls the
    /// same `resolve_auth` helper as chat(), so this regression-
    /// guards a future refactor that accidentally skipped it.
    /// Mirrors the same gap noted (and addressed in follow-up) by
    /// audit-aigw-387 on the Vertex SA OAuth side.
    #[tokio::test]
    async fn chat_stream_with_aad_secret_sets_authorization_bearer_header() {
        let aad_server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "aad.stream-bearer",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&aad_server)
            .await;

        let azure_server = MockServer::builder().start().await;
        let captured_headers: std::sync::Arc<std::sync::Mutex<Option<http::HeaderMap>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_responder = captured_headers.clone();
        Mock::given(method("POST"))
            .respond_with(move |req: &wiremock::Request| {
                *captured_for_responder.lock().unwrap() = Some(req.headers.clone());
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: [DONE]\n\n")
            })
            .mount(&azure_server)
            .await;

        let bridge = AzureOpenAiBridge::new()
            .with_url_override(mock_chat_url(&azure_server.uri(), "gpt4o-prod"))
            .with_aad_token_endpoint_override(format!("{}/oauth2/v2.0/token", aad_server.uri()));
        let ctx = BridgeContext::new(
            "req-azure-1",
            sample_model(),
            sample_pk_with_aad_secret("https://acme-west.openai.azure.com"),
        );
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let mut stream = bridge.chat_stream(&req, &ctx).await.unwrap();
        while stream.next().await.is_some() {}

        let headers = captured_headers
            .lock()
            .unwrap()
            .clone()
            .expect("headers captured");
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer aad.stream-bearer",
            "chat_stream AAD path must send Authorization: Bearer <minted-token>"
        );
        assert!(
            !headers.contains_key("api-key"),
            "chat_stream AAD path must NOT send api-key: header"
        );
        assert_eq!(
            headers.get("accept").unwrap(),
            "text/event-stream",
            "chat_stream must request SSE via Accept header"
        );
    }

    /// 项目所有者的决定：`api_base` 里的 userinfo 照配置使用，但密码不能出现在
    /// 任何展示出来的地方。`Debug` 是其中之一，而 override 字段恰好原样存着它。
    #[test]
    fn an_accepted_userinfo_base_keeps_the_password_out_of_debug() {
        for base in [
            "https://user:sk-live-9f2b1e@azure.example",
            "https://user:sk-live-9f2b1e@myres.openai.azure.com",
        ] {
            let upstream = AzureUpstreamRef::resolve("dep", Some(base))
                .unwrap_or_else(|e| panic!("{base} is accepted: {e}"));
            assert!(
                upstream
                    .chat_completions_url()
                    .contains("user:sk-live-9f2b1e@"),
                "the URL is used as configured"
            );
            let debug = format!("{upstream:?}");
            assert!(!debug.contains("sk-live-9f2b1e"), "{debug}");
            assert!(debug.contains("https://"), "{debug}");
        }
    }

    /// `InvalidUpstreamConfig` renders as a 400 whose message the CALLER
    /// reads, so a shape that is still refused must not quote the credential
    /// it carries. A canonical host and a scheme-less resource name are handed
    /// to `validate_url_token`, whose message echoes the value, and a `://`
    /// inside a query must not fool the redaction about where the scheme ends.
    #[test]
    fn every_refused_api_base_shape_keeps_the_credential_out_of_the_rejection() {
        for base in [
            "user:sk-live-9f2b1e@myres",
            "user:sk-live-9f2b1e@myres/x?next=https://y",
            "https://user:sk-live-9f2b1e@azure.example?api-version=1",
            "https://user:sk-live-9f2b1e@azure.example#frag",
        ] {
            let err =
                AzureUpstreamRef::resolve("dep", Some(base)).expect_err("this shape stays refused");
            let msg = err.to_string();
            assert!(
                !msg.contains("sk-live-9f2b1e"),
                "{base}: the rejection quotes the credential back: {msg}"
            );
            assert!(
                msg.contains("api_base"),
                "{base}: the rejection must name the field: {msg}"
            );
        }
    }

    /// A request on a userinfo base goes out, and when the upstream cannot be
    /// reached the error — which reaches the usage event and the logs — does
    /// not name the password. reqwest turns the userinfo into a Basic header.
    #[tokio::test]
    async fn a_userinfo_base_reaches_the_upstream_and_failures_hide_the_password() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(CapturingResponder::default())
            .expect(1)
            .mount(&server)
            .await;
        let bridge = AzureOpenAiBridge::new();
        let req = ChatFormat::new("my-azure-gpt4", vec![ChatMessage::user("hi")]);
        let base = server
            .uri()
            .replace("http://", "http://user:sk-live-9f2b1e@");
        let ctx = BridgeContext::new("req", sample_model(), sample_pk(Some(&base)));
        bridge
            .chat(&req, &ctx)
            .await
            .expect("the request reaches the upstream");
        let received = server.received_requests().await.unwrap_or_default();
        assert!(
            received[0].headers.contains_key("authorization"),
            "the HTTP client turns the userinfo into a header"
        );
        drop(server);

        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let base = format!("http://user:sk-live-9f2b1e@127.0.0.1:{closed}");
        let ctx = BridgeContext::new("req", sample_model(), sample_pk(Some(&base)));
        let err = bridge
            .chat(&req, &ctx)
            .await
            .expect_err("nothing listens there");
        for rendered in [err.to_string(), format!("{err:?}")] {
            assert!(!rendered.contains("sk-live-9f2b1e"), "{rendered}");
        }
    }
}
