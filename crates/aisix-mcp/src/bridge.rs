//! The upstream MCP client, behind the [`McpBridge`] trait.
//!
//! A bridge owns one live MCP session to a single upstream server (Streamable
//! HTTP transport) and exposes just the two operations the gateway needs in
//! this first cut: enumerate the server's tools, and invoke one. Aggregating
//! many bridges into the downstream-facing `/mcp` endpoint, tool namespacing,
//! and wiring into the shared guardrail/quota pipeline come in later steps —
//! this layer only proves a governed tunnel to one real upstream.
//!
//! All `rmcp` types are converted to this crate's own DTOs at the boundary so
//! the rest of the data plane never depends on the SDK directly. That keeps
//! rmcp's still-moving API contained to this file.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aisix_core::{McpAuthType, McpServer};
use aisix_gateway::redact_url_for_log;
use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt};
use http::{HeaderName, HeaderValue};
use rmcp::model::{CallToolRequestParams, CallToolResponse, ClientConfig, ProtocolVersion};
use rmcp::service::{ClientInitializeError, RoleClient, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClientTransportConfig, StreamableHttpError,
};
use rmcp::transport::{DynamicTransportError, StreamableHttpClientTransport};
use rmcp::ClientCacheConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt};

use crate::error::McpError;
use crate::token_exchange::{SubjectToken, TokenExchangeAuth, TokenExchangeConfig};
use crate::user_oauth::{UserOAuthManager, UserOAuthServer, UserSubject};

/// Default deadline for a single upstream operation (connect / list / call).
/// rmcp's high-level client sets no request timeout and reqwest has no default
/// one, so without this a hung or slow upstream pins the gateway request task
/// indefinitely. Overridable per upstream via [`McpUpstream::with_timeout`].
pub const DEFAULT_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared HTTP client for every rmcp streamable-http transport, with the
/// deployment's `upstream.*` connection settings applied.
///
/// rmcp pins its own reqwest line (0.13 — see Cargo.toml), so the shared
/// `aisix_gateway::client_builder()` (workspace reqwest) cannot be handed
/// to it directly; this is the sanctioned second construction site that
/// applies the SAME `upstream_http::config()` values to rmcp's reqwest.
///
/// What this changes vs rmcp's `default_http_client()`:
/// - a connect timeout exists at all (rmcp sets none, so a black-holed
///   upstream was bounded only by the coarse per-operation deadline —
///   the actual bug this fixes);
/// - connection POOLING turns ON. rmcp deliberately disables idle
///   pooling (`pool_max_idle_per_host(0)`) to dodge ~40 ms delayed-ACK
///   stalls on reused connections; here reuse wins — every other
///   outbound path pools under `upstream.*` management, and per-call
///   TCP+TLS handshakes to a remote MCP server cost far more than the
///   stall rmcp avoids. An operator can restore rmcp's behaviour with
///   `upstream.pool_max_idle_per_host: 0`;
/// - the TCP keepalive triple moves from reqwest's default 15 s/15 s/3
///   to the deployment's 60 s/30 s/5;
/// - the deployment's `upstream.tls` trust decision applies here too. An
///   MCP server behind an enterprise CA is exactly as common as a model
///   endpoint behind one, and the PEM has to be re-parsed rather than
///   reused because rmcp's `Certificate` is a different crate version's
///   type than the one `upstream_tls` caches for the workspace line.
///
/// One client for all MCP upstreams = one shared pool, matching how the
/// provider bridges share theirs (auth is injected per-request by the
/// transport, never client-wide).
fn shared_http_client() -> rmcp_reqwest::Client {
    static CLIENT: std::sync::OnceLock<rmcp_reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| build_http_client(false)).clone()
}

/// Build an MCP upstream client from `upstream.*`.
///
/// `disable_env_proxy` is for tests only: an egress proxy is a real
/// deployment shape, so the shared client stays proxy-aware. A dial that
/// lands on a proxy connects immediately and the wait moves to the proxy,
/// where `connect_timeout` no longer applies — so a test measuring that
/// timeout has to opt out of ambient `HTTP_PROXY`.
fn build_http_client(disable_env_proxy: bool) -> rmcp_reqwest::Client {
    let cfg = aisix_gateway::upstream_http::config();
    let mut b = rmcp_reqwest::Client::builder()
        // Same policy, same reason, as `aisix_gateway::client_builder` and as
        // this crate's own `openapi.rs` / `oauth.rs` clients: reqwest follows
        // up to ten redirects and strips only the four standard auth headers
        // cross-host. This client sends the gateway-held upstream key as
        // `API_KEY_HEADER` — `x-api-key`, a custom header — so an MCP server
        // answering `302 Location: http://elsewhere/` had that credential
        // re-sent to whatever it named.
        //
        // This builder mirrors `client_builder` field for field because rmcp
        // vendors its own reqwest and cannot share ours; the redirect policy
        // is part of what has to mirror.
        .redirect(rmcp_reqwest::redirect::Policy::none())
        // rmcp pins its own reqwest major, so this client cannot be built
        // from `client_builder()` — but it resolves through the same
        // process-wide cache, via that crate version's own resolver trait.
        .dns_resolver(std::sync::Arc::new(CachedResolver))
        .pool_idle_timeout(cfg.pool_idle_timeout)
        .tcp_keepalive(cfg.tcp_keepalive);
    if let Some(d) = cfg.connect_timeout {
        b = b.connect_timeout(d);
    }
    if let Some(d) = cfg.tcp_keepalive_interval {
        b = b.tcp_keepalive_interval(d);
    }
    if let Some(n) = cfg.tcp_keepalive_retries {
        b = b.tcp_keepalive_retries(n);
    }
    if let Some(n) = cfg.pool_max_idle_per_host {
        b = b.pool_max_idle_per_host(n);
    }
    if let Some(pem) = &cfg.tls.extra_ca_pem {
        // Already validated at boot by `TlsSettings::load`; a
        // failure here would mean rmcp's reqwest disagrees with
        // ours about the same bytes, which is worth a loud line
        // rather than a silently untrusting client.
        match rmcp_reqwest::Certificate::from_pem_bundle(pem) {
            Ok(roots) => {
                for root in roots {
                    b = b.add_root_certificate(root);
                }
            }
            Err(e) => tracing::error!(
                error = %aisix_gateway::error_with_causes(&e),
                "upstream.tls.ca_file not applied to MCP upstream connections"
            ),
        }
    }
    if let Some(configured) = &cfg.tls.client_identity {
        match rmcp_reqwest::Identity::from_pem(&configured.joined()) {
            Ok(identity) => b = b.identity(identity),
            Err(e) => tracing::error!(
                error = %aisix_gateway::error_with_causes(&e),
                "upstream.tls client identity not applied to MCP upstream connections"
            ),
        }
    }
    if !cfg.tls.verify {
        b = b.danger_accept_invalid_certs(true);
    }
    if disable_env_proxy {
        b = b.no_proxy();
    }
    // 同上面那两个分支：失败要出声。这一个丢掉的还不止一项 —— 默认客户端把
    // 上面配的 CA、客户端证书、`no_proxy` 全都没有了，于是连接可能改走环境里
    // 的代理、或者对内部 CA 签发的证书连不上，而运维看不到任何线索。
    //
    // 这里不返回 Err：调用方拿不到可处理的失败，一个不带自定义设置的客户端
    // 仍然比没有客户端强。丢掉的那些设置里，`verify: false` 也一并丢掉了，
    // 所以在 TLS 这一侧确实只会更严。
    //
    // 但**重定向策略是例外**，而且方向相反：`Client::new()` 拿的是 reqwest
    // 的默认策略，会跟最多十跳，跨主机只剥掉四个标准认证头 —— 上面那段注释
    // 说明了为什么这对本客户端是个洞：它带的是 `API_KEY_HEADER`
    // （`x-api-key`，自定义头），一台 MCP 服务器回 `302 Location:` 就能把网关
    // 持有的上游密钥送到它指定的任何地方。所以兜底客户端必须自己重新声明这
    // 条策略，不能靠“默认更严”。
    b.build().unwrap_or_else(|e| {
        tracing::error!(
            error = %aisix_gateway::error_with_causes(&e),
            "upstream.tls settings not applied to MCP upstream connections: the \
             configured client could not be built, falling back to a default one \
             (no custom CA, no client identity, no proxy opt-out)"
        );
        rmcp_reqwest::Client::builder()
            .redirect(rmcp_reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| rmcp_reqwest::Client::new())
    })
}

/// The workspace DNS cache, behind rmcp's reqwest major's resolver trait.
struct CachedResolver;

impl rmcp_reqwest::dns::Resolve for CachedResolver {
    fn resolve(&self, name: rmcp_reqwest::dns::Name) -> rmcp_reqwest::dns::Resolving {
        let cache = aisix_gateway::dns_cache::shared();
        Box::pin(async move {
            let addrs = cache.lookup(name.as_str()).await?;
            Ok(
                Box::new(addrs.iter().copied().collect::<Vec<_>>().into_iter())
                    as rmcp_reqwest::dns::Addrs,
            )
        })
    }
}

/// Header carrying the gateway-held key for `api_key` upstream auth.
const API_KEY_HEADER: &str = "x-api-key";

/// How the gateway authenticates to an upstream MCP server. Every variant but
/// [`McpAuth::Delegate`] is a gateway-held credential — a Bearer, an API key,
/// or an OAuth token the gateway mints or exchanges itself — and none of them
/// is ever exposed to the calling agent.
///
/// Relaying the CALLER's own credentials is a separate, opt-in mechanism
/// ([`McpUpstream::forwarded_client_headers`], from the server's
/// `forward_client_headers`), never a property of this enum. The normative
/// rule it has to respect is the audience one — a server "MUST only accept
/// tokens specifically intended for themselves" (MCP authorization,
/// 2025-06-18 and later) — so relaying is for an internal server that reads
/// the claims of a token its own identity provider issued, not for one that
/// validates `aud` against itself. There is no normative prohibition on the
/// relay itself; the "token passthrough is forbidden" wording lives in a
/// non-normative security best-practices guide.
#[derive(Clone)]
pub enum McpAuth {
    /// No upstream auth — the server is reachable as-is.
    None,
    /// Send `Authorization: Bearer <token>` on every upstream request. The
    /// token is the raw value, without the `Bearer ` prefix.
    Bearer(String),
    /// Send `x-api-key: <key>` on every upstream request.
    ApiKey(String),
    /// OAuth 2.0 client credentials (RFC 6749 §4.4): mint an access token at
    /// the configured token endpoint and send it as `Authorization: Bearer
    /// <access_token>`. The token is gateway-minted and gateway-held — never
    /// the caller's credential (see [`crate::oauth`]).
    OAuth2(OAuthClientConfig),
    /// Relay the caller's own `Authorization` value unchanged — the one the
    /// caller did NOT authenticate to the gateway with. `None` reaches the
    /// upstream without credentials.
    Delegate(Option<HeaderValue>),
    /// Exchange the caller's verified token for an upstream access token and
    /// send it as `Authorization: Bearer <access_token>` (see
    /// [`crate::token_exchange`]).
    TokenExchange(TokenExchangeAuth),
    /// Send the access token the caller obtained by signing in to this server
    /// as `Authorization: Bearer <access_token>` (see [`crate::user_oauth`]).
    UserOAuth(UserOAuthAuth),
}

/// The per-request halves of [`McpAuth::UserOAuth`].
#[derive(Clone)]
pub struct UserOAuthAuth {
    pub(crate) server: UserOAuthServer,
    pub(crate) caller: Option<UserOAuthCaller>,
}

/// What a request contributes to [`McpAuth::UserOAuth`]: where tokens are
/// kept, and whose to use. A caller the gateway could not name a user for has
/// no `subject`.
#[derive(Clone)]
pub struct UserOAuthCaller {
    pub manager: Arc<UserOAuthManager>,
    pub subject: Option<UserSubject>,
}

impl UserOAuthAuth {
    fn resolved(&self) -> Result<(&Arc<UserOAuthManager>, &UserSubject), McpError> {
        let caller = self.caller.as_ref();
        let subject = caller.and_then(|c| c.subject.as_ref());
        match (caller, subject) {
            (Some(caller), Some(subject)) => Ok((&caller.manager, subject)),
            _ => Err(crate::error::UserOAuthFailure::NoSubject.into()),
        }
    }

    pub(crate) async fn token(&self) -> Result<String, McpError> {
        let (manager, subject) = self.resolved()?;
        Ok(manager.access_token(&self.server, subject).await?)
    }
}

/// The calling agent's own credentials, resolved per request by the proxy,
/// for the auth types that act for the caller rather than with a
/// gateway-held credential.
#[derive(Clone, Default)]
pub struct CallerCredentials {
    /// The caller's `Authorization` header when it was not what the gateway
    /// admitted the request with — the upstream token an `oauth_delegate`
    /// server receives.
    pub delegated_authorization: Option<HeaderValue>,
    /// The bearer token the gateway verified through a JWT trust provider on
    /// this request — the only token an `oauth2_token_exchange` server
    /// exchanges.
    pub subject_token: Option<Arc<str>>,
    /// The token store and the signed-in user an
    /// `oauth2_authorization_code` server is called as.
    pub user_oauth: Option<UserOAuthCaller>,
}

impl std::fmt::Debug for CallerCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallerCredentials")
            .field(
                "delegated_authorization",
                &self
                    .delegated_authorization
                    .as_ref()
                    .map(|_| "***redacted***"),
            )
            .field(
                "subject_token",
                &self.subject_token.as_ref().map(|_| "***redacted***"),
            )
            .field(
                "user_oauth_subject",
                &self
                    .user_oauth
                    .as_ref()
                    .and_then(|c| c.subject.as_ref())
                    .map(UserSubject::id),
            )
            .finish()
    }
}

/// Client-credentials parameters for [`McpAuth::OAuth2`].
#[derive(Clone)]
pub struct OAuthClientConfig {
    /// OAuth client identifier (non-secret).
    pub client_id: String,
    /// OAuth client secret. Redacted from `Debug` like every other
    /// gateway-held credential in this module.
    pub client_secret: String,
    /// Token endpoint URL the credentials are exchanged at (non-secret).
    pub token_url: String,
    /// Scopes to request, joined with spaces into the `scope` parameter.
    pub scopes: Vec<String>,
}

// Hand-written for the same reason as `McpAuth`'s: the client secret must
// never land in logs via `{:?}`. The non-secret fields stay visible — they
// are what an operator needs to identify the token exchange being logged —
// with `token_url` reduced to its origin, since a URL can carry a
// credential of its own.
impl std::fmt::Debug for OAuthClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClientConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"***redacted***")
            .field("token_url", &redact_url_for_log(&self.token_url))
            .field("scopes", &self.scopes)
            .finish()
    }
}

// Hand-written so the gateway-held token never lands in logs via `{:?}`. This
// crate is the credential holder; a derived `Debug` would print the bearer in
// plaintext the moment any caller logs an upstream.
impl std::fmt::Debug for McpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpAuth::None => f.write_str("None"),
            McpAuth::Bearer(_) => f.write_str("Bearer(***redacted***)"),
            McpAuth::ApiKey(_) => f.write_str("ApiKey(***redacted***)"),
            // Delegates to the redacting `OAuthClientConfig` impl above.
            McpAuth::OAuth2(cfg) => f.debug_tuple("OAuth2").field(cfg).finish(),
            McpAuth::Delegate(value) => f
                .debug_tuple("Delegate")
                .field(&value.as_ref().map(|_| "***redacted***"))
                .finish(),
            McpAuth::TokenExchange(auth) => f.debug_tuple("TokenExchange").field(auth).finish(),
            McpAuth::UserOAuth(auth) => {
                f.debug_tuple("UserOAuth").field(&auth.server.name).finish()
            }
        }
    }
}

/// The MCP protocol revision the bridge opens an upstream session with.
///
/// Explicit, never probed: a server that does not speak the configured
/// revision produces a visible connect failure instead of a silent
/// cross-generation downgrade. (Automatic fallback is how version and
/// session context ends up crossing the protocol boundary — the bug class
/// sibling gateways are currently working through.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpProtocol {
    /// Open with the `initialize` handshake and negotiate among the
    /// pre-2026 protocol revisions. Works with every legacy server and with
    /// `2026-07-28` servers that keep backward compatibility.
    #[default]
    LegacyHandshake,
    /// The stateless MCP `2026-07-28` revision: handshake-free
    /// `server/discover` startup, self-contained per-request metadata.
    /// Required for servers that no longer answer `initialize`.
    V20260728,
}

/// Connection parameters for a single upstream MCP server.
#[derive(Clone)]
pub struct McpUpstream {
    /// The server's Streamable HTTP MCP endpoint, e.g.
    /// `https://api.example.com/mcp`.
    pub url: String,
    /// Upstream authentication, held gateway-side.
    pub auth: McpAuth,
    /// Per-operation deadline. Defaults to [`DEFAULT_UPSTREAM_TIMEOUT`].
    pub timeout: Duration,
    /// Protocol revision the session is opened with. Defaults to the
    /// legacy `initialize` handshake.
    pub protocol: McpProtocol,
    /// Inbound client headers this server's `forward_client_headers`
    /// admits, resolved per request against the calling agent's own
    /// request. Empty when the server configures none, or when nothing the
    /// agent sent matched.
    ///
    /// Independent of `auth`, which stays the gateway's own credential —
    /// except when both name the same header, where the forwarded value
    /// wins and the gateway's is not sent at all.
    pub forwarded_client_headers: Vec<(HeaderName, HeaderValue)>,
}

// Manual so a `Bearer` token cannot leak through `McpUpstream`'s `Debug`
// (delegates to the redacting `McpAuth` impl above), nor a credential the
// URL itself carries.
impl std::fmt::Debug for McpUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpUpstream")
            .field("url", &redact_url_for_log(&self.url))
            .field("auth", &self.auth)
            .field("timeout", &self.timeout)
            .field("protocol", &self.protocol)
            .field(
                // Names only: a forwarded header may be the caller's own
                // credential.
                "forwarded_client_headers",
                &self
                    .forwarded_client_headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl McpUpstream {
    /// Build an unauthenticated upstream with the default timeout.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            auth: McpAuth::None,
            timeout: DEFAULT_UPSTREAM_TIMEOUT,
            protocol: McpProtocol::default(),
            forwarded_client_headers: Vec::new(),
        }
    }

    /// Deliver the client headers this server forwards, as already
    /// resolved against the inbound request.
    pub fn with_forwarded_client_headers(
        mut self,
        forwarded: Vec<(HeaderName, HeaderValue)>,
    ) -> Self {
        self.forwarded_client_headers = forwarded;
        self
    }

    /// Select the protocol revision the session is opened with.
    pub fn with_protocol(mut self, protocol: McpProtocol) -> Self {
        self.protocol = protocol;
        self
    }

    /// Set Bearer auth (raw token, no `Bearer ` prefix).
    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        self.auth = McpAuth::Bearer(token.into());
        self
    }

    /// Set API-key auth (sent as `x-api-key: <key>`).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.auth = McpAuth::ApiKey(key.into());
        self
    }

    /// Set OAuth 2.0 client-credentials auth.
    pub fn with_oauth2(mut self, config: OAuthClientConfig) -> Self {
        self.auth = McpAuth::OAuth2(config);
        self
    }

    /// Fill the caller-held halves of [`McpAuth::Delegate`] and
    /// [`McpAuth::TokenExchange`] from this request's caller. Other auth
    /// types are gateway-held and unaffected.
    pub fn with_caller_credentials(mut self, caller: &CallerCredentials) -> Self {
        match &mut self.auth {
            McpAuth::Delegate(value) => *value = caller.delegated_authorization.clone(),
            McpAuth::TokenExchange(auth) => {
                auth.subject = caller.subject_token.clone().map(SubjectToken);
            }
            McpAuth::UserOAuth(auth) => auth.caller = caller.user_oauth.clone(),
            _ => {}
        }
        self
    }

    /// Override the per-operation deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// One tool advertised by an upstream server, normalised off the wire shape.
///
/// Minimal for this step: tool annotations (`readOnlyHint` / `destructiveHint`)
/// and `output_schema` are dropped here and will be carried when the per-tool
/// ACL / guardrail layer (DP-4) needs them.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// The tool's name, as the upstream advertises it (no gateway prefix yet).
    pub name: String,
    /// Human-readable description, if the server provides one.
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments, as a JSON object.
    pub input_schema: serde_json::Value,
}

/// The outcome of a `tools/call`, normalised off the wire shape.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolResult {
    /// The content blocks the tool returned, as a JSON array (text, images,
    /// resource links, …). Left as raw JSON here; the downstream endpoint
    /// shapes it for the agent.
    pub content: serde_json::Value,
    /// The tool's structured result, when it returns one (MCP `structuredContent`).
    /// A tool may return only structured content with an empty `content` array.
    pub structured_content: Option<serde_json::Value>,
    /// Whether the upstream flagged this result as a tool-level error.
    pub is_error: bool,
}

/// The gateway's view of one upstream MCP server. Implemented by
/// [`RmcpBridge`]; kept as a trait so the rest of the data plane depends on
/// this surface rather than on `rmcp`, and so the upstream can be stubbed in
/// higher-layer tests.
#[async_trait]
pub trait McpBridge: Send + Sync {
    /// List the tools the upstream currently exposes.
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError>;

    /// Invoke a tool by name with the given JSON arguments. `arguments` must
    /// be a JSON object or `null` (no arguments); anything else is rejected.
    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, McpError>;

    /// Obtain, without running any MCP operation, the upstream credential an
    /// `oauth2_token_exchange` server needs for the current caller (see
    /// `McpGateway::preflight_upstream_auth`). Every other bridge has nothing
    /// to check.
    async fn preflight_auth(&self) -> Result<(), McpError> {
        Ok(())
    }
}

/// `rmcp`-backed [`McpBridge`]: holds one running client session to the
/// upstream. Dropping it tears the session down.
pub struct RmcpBridge {
    running: RunningService<RoleClient, ClientConfig>,
    timeout: Duration,
}

/// Base transport configuration for one upstream connection. One rmcp
/// default is deliberately overridden, to keep "one inbound call = one
/// upstream execution" true:
///
/// - `reinit_on_expired_session = false`. On a session-expired 404 the SDK
///   default transparently re-runs `initialize` and RE-SENDS the in-flight
///   request — for `tools/call` that is a silent second execution of a
///   possibly side-effectful tool, outside quota and budget accounting
///   (the session-layer sibling of the MRTR auto-retry disabled in
///   [`McpBridge::call_tool`]). The gateway surfaces the failure instead;
///   [`EphemeralBridge`] opens a fresh session on the next operation anyway.
///
/// The per-event SSE cap stays at rmcp's 16 MiB default. rmcp also caps a
/// non-streaming JSON response at 16 MiB (fixed, RUSTSEC-2026-0462), so an
/// upstream answer over 16 MiB fails the same way in either framing. Lifting
/// the SSE cap alone would let an upstream that never ends an event grow the
/// gateway's memory until the operation deadline.
fn transport_config(url: &str) -> StreamableHttpClientTransportConfig {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    config.reinit_on_expired_session = false;
    config
}

impl RmcpBridge {
    /// Open a session to `upstream`: build the Streamable HTTP transport
    /// (injecting gateway-held auth — for `oauth2` this mints or reuses a
    /// cached access token first) and run the startup lifecycle selected by
    /// [`McpUpstream::protocol`] — the `initialize` handshake by default,
    /// or handshake-free `server/discover` for `2026-07-28` upstreams. The
    /// whole sequence, token minting included, is bounded by the upstream's
    /// timeout.
    ///
    /// An upstream `401` drops a cached minted or exchanged token; an
    /// exchanged one is then exchanged afresh and the connect retried once.
    pub async fn connect(upstream: &McpUpstream) -> Result<Self, McpError> {
        match Self::connect_once(upstream).await {
            Err(ConnectFailure::Unauthorized(error)) => match &upstream.auth {
                McpAuth::OAuth2(cfg) => {
                    crate::oauth::invalidate(cfg);
                    Err(error)
                }
                McpAuth::TokenExchange(auth) => {
                    auth.invalidate();
                    Self::connect_once(upstream).await.map_err(|failure| {
                        // A fresh token rejected too is not kept either.
                        if matches!(failure, ConnectFailure::Unauthorized(_)) {
                            auth.invalidate();
                        }
                        failure.into_error().after_upstream_contact()
                    })
                }
                _ => Err(error),
            },
            other => other.map_err(ConnectFailure::into_error),
        }
    }

    async fn connect_once(upstream: &McpUpstream) -> Result<Self, ConnectFailure> {
        // One deadline for the whole sequence. The upstream credential is
        // obtained outside `establish` so that a token exchange running out
        // of it is reported as the identity provider being unavailable, not
        // as the upstream failing to connect.
        let deadline = tokio::time::Instant::now() + upstream.timeout;
        // Gateway credential and forwarded client headers are
        // resolved into one header set rather than one arm each,
        // because they can name the same slot: an operator who lists
        // `authorization` in `forward_client_headers` is choosing the
        // caller's own credential over the gateway's, and the upstream
        // must receive exactly one of them.
        //
        // The claimed set is read before anything fills a slot, so
        // suppressing the gateway credential and delivering the
        // forwarded value cannot disagree.
        let forwarded = &upstream.forwarded_client_headers;
        let claims = |name: &str| forwarded.iter().any(|(n, _)| n.as_str() == name);
        let mut custom: HashMap<HeaderName, HeaderValue> = HashMap::new();
        let mut auth_header: Option<String> = None;
        match &upstream.auth {
            McpAuth::None => {}
            McpAuth::Bearer(token) => {
                if !claims("authorization") {
                    auth_header = Some(token.clone());
                }
            }
            McpAuth::ApiKey(key) => {
                if !claims(API_KEY_HEADER) {
                    // A key with non-header-safe bytes is a clean config error,
                    // not a panic — and the key itself never enters the message.
                    let mut value = HeaderValue::from_str(key).map_err(|_| {
                        McpError::Connect(
                            "upstream API key is not a valid HTTP header value".to_string(),
                        )
                    })?;
                    // Marks the value opaque to `Debug` formatting of the
                    // header map, mirroring this module's redaction posture.
                    value.set_sensitive(true);
                    custom.insert(HeaderName::from_static(API_KEY_HEADER), value);
                }
            }
            McpAuth::OAuth2(cfg) => {
                // Minted only when it will actually be sent: a token a
                // forwarded header is about to displace is a round trip
                // to the identity provider whose only possible effect is
                // failing a request that did not need it.
                if !claims("authorization") {
                    auth_header = Some(
                        tokio::time::timeout_at(deadline, crate::oauth::get_or_fetch(cfg))
                            .await
                            .map_err(|_| connect_timed_out())??,
                    );
                }
            }
            McpAuth::Delegate(value) => {
                // Relayed as the caller sent it, scheme included.
                if let (false, Some(value)) = (claims("authorization"), value) {
                    let mut value = value.clone();
                    value.set_sensitive(true);
                    custom.insert(http::header::AUTHORIZATION, value);
                }
            }
            McpAuth::TokenExchange(auth) => {
                if !claims("authorization") {
                    auth_header = Some(auth.token(deadline).await?);
                }
            }
            McpAuth::UserOAuth(auth) => {
                auth_header = Some(
                    tokio::time::timeout_at(deadline, auth.token())
                        .await
                        .map_err(|_| connect_timed_out())??,
                );
            }
        }
        for (name, value) in forwarded {
            custom.insert(name.clone(), value.clone());
        }
        let establish = async {
            // Through `with_client(shared_http_client(), ..)` so the
            // transport inherits the deployment's `upstream.*` connection
            // settings — `from_uri`/`from_config` would build rmcp's own
            // default client with none of them — and through
            // `transport_config` for the pinned transport defaults.
            let mut config = transport_config(&upstream.url);
            if let Some(token) = auth_header {
                config = config.auth_header(token);
            }
            if !custom.is_empty() {
                config = config.custom_headers(custom);
            }
            let transport =
                StreamableHttpClientTransport::with_client(shared_http_client(), config);
            // Lifecycle follows the configured protocol revision. The
            // handler is `ClientConfig::default()` on BOTH paths — identical
            // handshake bytes to the previous unit handler (whose default
            // `get_info()` returns exactly `ClientConfig::default()`), plus
            // the per-request metadata the Discover lifecycle needs.
            let lifecycle = match upstream.protocol {
                McpProtocol::LegacyHandshake => ClientLifecycleMode::Initialize,
                // Discover, NOT Auto: a server that answers `server/discover`
                // with an error must fail the connect visibly. `Auto` would
                // silently retry the legacy handshake, hiding real
                // misconfiguration and version drift.
                McpProtocol::V20260728 => ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                },
            };
            ClientConfig::default()
                .serve_with_lifecycle(transport, lifecycle)
                .await
                .map_err(|e| {
                    // Bound + sanitize: a bare-401 shape embeds the upstream's
                    // response body in the error text, which lands in gateway
                    // logs. An upstream that reflects request headers into its
                    // error body (or emits control characters for log injection)
                    // must not get either past this point verbatim. The
                    // request URL a transport failure names is redacted first,
                    // before truncation can cut it anywhere.
                    let (message, causes) = match &e {
                        ClientInitializeError::TransportError { error, .. } => (
                            redact_transport_url(e.to_string(), error),
                            transport_causes(error),
                        ),
                        _ => (e.to_string(), String::new()),
                    };
                    // Bounded apart: rmcp's own text can fill the bound by itself.
                    let error = McpError::Connect(format!(
                        "{}{}",
                        sanitize_error_message(&message),
                        sanitize_error_message(&causes)
                    ));
                    // An upstream 401 is handed to `connect`, which decides
                    // per auth type (drop a minted token, re-exchange, or
                    // surface it to the caller).
                    if init_error_is_unauthorized(&e) {
                        ConnectFailure::Unauthorized(error)
                    } else {
                        ConnectFailure::Other(error)
                    }
                })
        };
        let running = tokio::time::timeout_at(deadline, establish)
            .await
            .map_err(|_| connect_timed_out())??;
        // rmcp 3.x ships a client response cache that is ENABLED by default
        // (`ClientCacheConfig::default()`), keyed per peer and honoring the
        // server's `ttlMs`/`cacheScope` hints. A pooled or persistent bridge
        // is shared across every AISIX caller that reaches the same
        // upstream, so a cached upstream response would be served across
        // principals — a cross-tenant leak the SDK's own migration guide
        // warns about (`private_partition`). Today's production path
        // (`EphemeralBridge`) reconnects per operation, so this pins the
        // posture BEFORE the "connection pooling is a later optimization"
        // note above ever lands. The gateway's caching story is a wire-level
        // hint only (no cache engine), so the cache is disabled outright
        // rather than partitioned. Do not re-enable without keying the
        // partition to the calling principal.
        running
            .peer()
            .set_response_cache_config(ClientCacheConfig::disabled())
            .await;
        Ok(Self {
            running,
            timeout: upstream.timeout,
        })
    }
}

fn connect_timed_out() -> McpError {
    McpError::Connect("upstream MCP connect timed out".to_string())
}

/// A failed connect attempt, with an upstream `401` kept apart so
/// [`RmcpBridge::connect`] can act on it.
enum ConnectFailure {
    Unauthorized(McpError),
    Other(McpError),
}

impl ConnectFailure {
    fn into_error(self) -> McpError {
        match self {
            ConnectFailure::Unauthorized(error) | ConnectFailure::Other(error) => error,
        }
    }
}

impl From<McpError> for ConnectFailure {
    fn from(error: McpError) -> Self {
        ConnectFailure::Other(error)
    }
}

/// Bound an upstream-derived error message for logging: control characters
/// (log-injection vectors) are stripped and the text is truncated, since a
/// bare non-success response embeds the upstream's body verbatim.
pub(crate) fn sanitize_error_message(message: &str) -> String {
    const MAX_LEN: usize = 256;
    let cleaned: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= MAX_LEN {
        cleaned
    } else {
        let truncated: String = cleaned.chars().take(MAX_LEN).collect();
        format!("{truncated}…")
    }
}

/// A failed reqwest request as one log line: the origin of the URL it was
/// sent to ([`redact_url_for_log`]), reqwest's message, and the causes below
/// it. reqwest renders the URL into its own text — userinfo already moved into
/// a header, path and query intact — so the message is taken without it: the
/// path and query can carry a credential or a tool's arguments, and a long one
/// would push the cause past the log bound.
pub(crate) fn transport_failure_text(error: reqwest::Error) -> String {
    let origin = error.url().map(|url| redact_url_for_log(url.as_str()));
    let message = aisix_gateway::transport_error_message(&error.without_url());
    match origin {
        Some(origin) => format!("{origin}: {message}"),
        None => message,
    }
}

/// `message` with the request URL it names reduced to its origin
/// ([`redact_url_for_log`]). rmcp's transport text renders the URL it was
/// sending to, and the gateway logs these errors. `url` is the error's own
/// `url()`, i.e. exactly the text reqwest rendered.
fn redact_request_url(message: String, url: Option<&str>) -> String {
    match url {
        Some(url) if !url.is_empty() => message.replace(url, &redact_url_for_log(url)),
        _ => message,
    }
}

/// [`redact_request_url`] for an rmcp transport failure, which wraps the
/// reqwest error (rmcp's own reqwest, `rmcp_reqwest`) verbatim.
fn redact_transport_url(message: String, error: &DynamicTransportError) -> String {
    let url = match error
        .error
        .downcast_ref::<StreamableHttpError<rmcp_reqwest::Error>>()
    {
        Some(StreamableHttpError::Client(e)) => e.url().map(|u| u.as_str()),
        _ => None,
    };
    redact_request_url(message, url)
}

/// The log text of a failed MCP request on an open session. Bounded and
/// sanitized like a connect failure: a non-success response embeds the
/// upstream's whole body. The request URL is redacted first, before
/// truncation can cut it anywhere.
fn service_error_message(error: &ServiceError) -> String {
    let message = match error {
        ServiceError::TransportSend(transport) => {
            redact_transport_url(error.to_string(), transport)
        }
        _ => error.to_string(),
    };
    sanitize_error_message(&message)
}

/// Whether a failed `initialize` handshake was an upstream `401 Unauthorized`.
///
/// The reqwest transport surfaces a 401 in one of two stable shapes (rmcp is
/// pinned exactly, so these cannot drift silently): a 401 carrying a
/// `WWW-Authenticate` header becomes `StreamableHttpError::AuthRequired`, and
/// a bare 401 becomes `UnexpectedServerResponse("HTTP 401 …: <body>")` — a
/// JSON-RPC error in its body included, which rmcp keeps as the status rather
/// than as an in-band error. Both arrive here inside
/// `ClientInitializeError::TransportError` as the type-erased transport error;
/// the downcast names rmcp's own reqwest (`rmcp_reqwest`, the 0.13 line — not
/// the workspace 0.12) so the types match. Post-handshake operations don't
/// need this: [`EphemeralBridge`] reconnects per operation, so every request
/// replays the handshake and a rejected token always surfaces on this path.
fn init_error_is_unauthorized(error: &ClientInitializeError) -> bool {
    let ClientInitializeError::TransportError { error, .. } = error else {
        return false;
    };
    transport_error_is_unauthorized(error)
}

fn request_error(error: rmcp::ServiceError) -> McpError {
    match &error {
        rmcp::ServiceError::TransportSend(transport)
            if transport_error_is_unauthorized(transport) =>
        {
            McpError::Unauthorized(service_error_message(&error))
        }
        rmcp::ServiceError::TransportSend(transport) => {
            // Bounded apart, as on connect.
            McpError::Request(format!(
                "{}{}",
                service_error_message(&error),
                sanitize_error_message(&transport_causes(transport))
            ))
        }
        _ => McpError::Request(service_error_message(&error)),
    }
}

/// `": <causes>"` when the transport failed inside its HTTP client, otherwise
/// empty. rmcp's `StreamableHttpError` and the SSE decoder under it show the
/// client error's own `Display` and hide it from `source()`, so each is opened
/// by hand.
fn transport_causes(error: &rmcp::transport::DynamicTransportError) -> String {
    use std::error::Error as _;
    let cause = match error
        .error
        .downcast_ref::<StreamableHttpError<rmcp_reqwest::Error>>()
    {
        Some(StreamableHttpError::Client(client)) => client.source(),
        Some(StreamableHttpError::Sse(SseError::Body(body))) => body.source(),
        _ => None,
    };
    cause
        .map(|cause| format!(": {}", aisix_gateway::error_with_causes(cause)))
        .unwrap_or_default()
}

fn transport_error_is_unauthorized(error: &rmcp::transport::DynamicTransportError) -> bool {
    match error
        .error
        .downcast_ref::<StreamableHttpError<rmcp_reqwest::Error>>()
    {
        Some(StreamableHttpError::AuthRequired(_)) => true,
        Some(StreamableHttpError::UnexpectedServerResponse(message)) => {
            // Anchored to the prefix: the format is `HTTP <status>: <body>`,
            // so an upstream body can never fake a 401 here.
            message.starts_with("HTTP 401")
        }
        _ => false,
    }
}

/// Most `tools/list` pages read from one upstream. A server paginates at its
/// own page size, so this is far past any real inventory; it stops one that
/// keeps handing back a cursor from looping until the deadline.
const MAX_TOOL_LIST_PAGES: usize = 100;

/// Most tools kept from one upstream's `tools/list`, across its pages.
const MAX_LISTED_TOOLS: usize = 10_000;

#[async_trait]
impl McpBridge for RmcpBridge {
    /// Every page of the upstream's tool list, read under one deadline.
    ///
    /// `tools/list` is paginated: a result carrying `nextCursor` has more
    /// tools behind it, fetched by sending the cursor back
    /// (<https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/pagination>).
    /// The downstream list is one complete result, so the pages are joined
    /// here; an upstream past [`MAX_TOOL_LIST_PAGES`] or [`MAX_LISTED_TOOLS`]
    /// fails the list rather than serving part of it.
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        let list = async {
            let mut tools = Vec::new();
            let mut params = None;
            for _ in 0..MAX_TOOL_LIST_PAGES {
                let page = self
                    .running
                    .list_tools(params)
                    .await
                    .map_err(request_error)?;
                tools.extend(page.tools.into_iter().map(into_mcp_tool));
                if tools.len() > MAX_LISTED_TOOLS {
                    return Err(McpError::Request(format!(
                        "upstream tools/list returned more than {MAX_LISTED_TOOLS} tools"
                    )));
                }
                let Some(cursor) = page.next_cursor else {
                    return Ok(tools);
                };
                params =
                    Some(rmcp::model::PaginatedRequestParams::default().with_cursor(Some(cursor)));
            }
            Err(McpError::Request(format!(
                "upstream tools/list did not end within {MAX_TOOL_LIST_PAGES} pages"
            )))
        };
        tokio::time::timeout(self.timeout, list)
            .await
            .map_err(|_| McpError::Request("upstream tools/list timed out".to_string()))?
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, McpError> {
        let mut params = CallToolRequestParams::new(name.to_string());
        params = match arguments {
            serde_json::Value::Null => params,
            serde_json::Value::Object(map) => params.with_arguments(map),
            _ => {
                return Err(McpError::Request(
                    "tool arguments must be a JSON object or null".to_string(),
                ))
            }
        };
        // `call_tool_once`, NOT `call_tool`: the 3.x high-level helper drives
        // MRTR (SEP-2322) automatically — on an `input_required` result it
        // re-sends the request with client-fulfilled inputs, up to
        // `DEFAULT_MRTR_MAX_ROUNDS` (10) upstream round trips for ONE inbound
        // call. That would silently multiply upstream cost and defeat every
        // per-call quota, budget, and timeout assumption in the gateway. The
        // `_once` variant sends exactly one request; a non-final response is
        // surfaced as a clean tool failure instead of a hidden retry loop.
        let response = tokio::time::timeout(self.timeout, self.running.call_tool_once(params))
            .await
            .map_err(|_| McpError::Request("upstream tools/call timed out".to_string()))?
            .map_err(request_error)?;
        let result = match response {
            CallToolResponse::Complete(result) => result,
            CallToolResponse::InputRequired(_) => {
                return Err(McpError::Request(
                    "upstream tool requires additional interactive input (MRTR), which the \
                     gateway does not relay"
                        .to_string(),
                ))
            }
            CallToolResponse::Task(_) => {
                return Err(McpError::Request(
                    "upstream tool deferred to an asynchronous task, which the gateway does \
                     not support"
                        .to_string(),
                ))
            }
            // `CallToolResponse` is #[non_exhaustive]; treat future variants
            // as unsupported rather than mis-mapping them to a result. Kept
            // payload-free: the message lands in gateway logs verbatim.
            _ => {
                return Err(McpError::Request(
                    "upstream returned an unsupported tools/call response kind".to_string(),
                ))
            }
        };
        let content = serde_json::to_value(&result.content)
            .map_err(|e| McpError::Request(format!("failed to encode tool result: {e}")))?;
        Ok(McpToolResult {
            content,
            structured_content: result.structured_content,
            is_error: result.is_error.unwrap_or(false),
        })
    }
}

/// Normalise an `rmcp` `Tool` into our [`McpTool`] DTO.
fn into_mcp_tool(tool: rmcp::model::Tool) -> McpTool {
    McpTool {
        name: tool.name.into_owned(),
        description: tool.description.map(|d| d.into_owned()),
        input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

/// Build the connection parameters for an upstream from its registered
/// [`McpServer`] resource: maps `auth_type` and its credential fields to
/// [`McpAuth`] and `timeout_ms` to the per-operation deadline.
///
/// Stays permissive on purpose: fields a mis-configured resource left unset
/// map to empty strings rather than erroring here. The credential exchange
/// then fails cleanly at connect time and that server degrades like any
/// unreachable upstream (its tools drop out of `tools/list`, the failure is
/// logged), instead of one bad row poisoning snapshot loading.
/// Whether a URL sends its traffic over cleartext HTTP. Matches how the
/// HTTP stack will actually treat it (the URL parser lowercases the scheme
/// and strips leading whitespace), so `HTTP://` and `" http://"` are
/// flagged too; `https://` never is.
fn is_cleartext(url: &str) -> bool {
    let trimmed = url.trim_start();
    trimmed
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
}

/// The cleartext-credential findings for a registered server: which
/// gateway-held secret travels over plain HTTP, and to which URL. Applies
/// to every credentialed auth type against an `http://` server URL —
/// `type: mcp` and `type: openapi` rows share these fields — plus, for
/// `oauth2`, a cleartext `token_url` (it carries the client secret; a
/// distinct finding even when `token_url` equals `url`). The URL is the
/// one logged, so it is reduced to its origin ([`redact_url_for_log`]):
/// userinfo, path and query can each carry a credential. Pure, so tests
/// pin the selection.
pub(crate) fn cleartext_findings(server: &McpServer) -> Vec<(&'static str, String)> {
    let mut findings = Vec::new();
    match server.auth_type {
        McpAuthType::None => {}
        McpAuthType::OauthDelegate => {
            if is_cleartext(&server.url) {
                findings.push((
                    "the caller's relayed upstream token",
                    redact_url_for_log(&server.url),
                ));
            }
        }
        _ => {
            if is_cleartext(&server.url) {
                findings.push((
                    "the gateway-held upstream credential",
                    redact_url_for_log(&server.url),
                ));
            }
        }
    }
    let token_url = server.token_url.as_deref().unwrap_or_default();
    match server.auth_type {
        McpAuthType::OAuth2 if is_cleartext(token_url) => {
            findings.push(("the OAuth client secret", redact_url_for_log(token_url)));
        }
        McpAuthType::OAuth2TokenExchange if is_cleartext(token_url) => {
            findings.push((
                "the OAuth client secret and the caller's token",
                redact_url_for_log(token_url),
            ));
        }
        McpAuthType::OAuth2AuthorizationCode if is_cleartext(token_url) => {
            findings.push((
                "the users' upstream OAuth tokens",
                redact_url_for_log(token_url),
            ));
        }
        _ => {}
    }
    findings
}

/// Warn — once per distinct finding per process — that a credential travels
/// unencrypted. Deliberately a warning, not a rejection: plain-HTTP
/// upstreams inside a private network are a lawful, common deployment, and
/// request behavior (including redirect handling) stays aligned with the
/// reference SDK baseline (#879). Deduped on the (server, finding, url)
/// tuple because the gateway is rebuilt from the snapshot on every request
/// — an undeduped warn would log per call.
pub(crate) fn warn_cleartext_credential(server: &McpServer) {
    use std::sync::{Mutex, OnceLock};
    type Warned = std::collections::HashSet<(String, &'static str, String)>;
    static WARNED: OnceLock<Mutex<Warned>> = OnceLock::new();
    for (what, url) in cleartext_findings(server) {
        let mut warned = WARNED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if warned.insert((server.name.clone(), what, url.clone())) {
            tracing::warn!(
                server = %server.name,
                url = %url,
                "{what} is sent over cleartext http; anyone on the network path can read \
                 it — serve this upstream over https"
            );
        }
    }
}

/// Header slots the MCP transport itself owns on this hop.
///
/// `mcp-session-id`, `mcp-protocol-version` and `last-event-id` name the
/// session the CALLER holds with this gateway, not the one the gateway
/// opens upstream, so a relayed copy identifies a session the upstream
/// never issued. rmcp refuses a custom `mcp-session-id` / `last-event-id`
/// outright — the connection then fails rather than degrades — and while
/// it does let `mcp-protocol-version` through, that one selects the
/// revision the session is opened at, which is the transport's decision
/// and not the caller's.
pub const MCP_PROTOCOL_HEADERS: &[&str] =
    &["last-event-id", "mcp-protocol-version", "mcp-session-id"];

/// The inbound client headers `server` forwards out of `client_headers`.
///
/// `None` — a gateway built from the snapshot alone, with no request
/// behind it — forwards nothing.
pub fn forwarded_client_headers(
    server: &McpServer,
    client_headers: Option<&http::HeaderMap>,
) -> Vec<(HeaderName, HeaderValue)> {
    let Some(client) = client_headers else {
        return Vec::new();
    };
    aisix_core::resolve_forwarded_client_headers(
        &server.forward_client_headers,
        client,
        MCP_PROTOCOL_HEADERS,
    )
}

pub fn upstream_from_mcp_server(server: &McpServer) -> McpUpstream {
    let auth = match server.auth_type {
        McpAuthType::None => McpAuth::None,
        McpAuthType::Bearer => McpAuth::Bearer(server.secret.clone().unwrap_or_default()),
        McpAuthType::ApiKey => McpAuth::ApiKey(server.secret.clone().unwrap_or_default()),
        McpAuthType::OAuth2 => McpAuth::OAuth2(OAuthClientConfig {
            client_id: server.client_id.clone().unwrap_or_default(),
            client_secret: server.secret.clone().unwrap_or_default(),
            token_url: server.token_url.clone().unwrap_or_default(),
            scopes: server.scopes.clone().unwrap_or_default(),
        }),
        // The caller-held halves are filled per request
        // (`McpUpstream::with_caller_credentials`).
        McpAuthType::OauthDelegate => McpAuth::Delegate(None),
        McpAuthType::OAuth2TokenExchange => McpAuth::TokenExchange(TokenExchangeAuth {
            config: token_exchange_config(server),
            subject: None,
        }),
        // The stored server id and the caller are filled per request
        // (`McpGateway`'s `bridge_for`).
        McpAuthType::OAuth2AuthorizationCode => McpAuth::UserOAuth(UserOAuthAuth {
            server: UserOAuthServer::new("", server),
            caller: None,
        }),
    };
    let timeout = server
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_UPSTREAM_TIMEOUT);
    // Dated spec revisions map one-to-one onto session lifecycles; absent
    // means the legacy handshake (which negotiates among the pre-2026
    // revisions on its own).
    let protocol = match server.protocol_version {
        None => McpProtocol::LegacyHandshake,
        Some(aisix_core::McpProtocolVersion::V20260728) => McpProtocol::V20260728,
    };
    McpUpstream {
        url: server.url.clone(),
        auth,
        timeout,
        protocol,
        // Per-request, so it is attached by the gateway that holds the
        // inbound headers (`McpGateway::from_snapshot_for_request`), not
        // derived from the stored server row here.
        forwarded_client_headers: Vec::new(),
    }
}

/// The exchange parameters of an `oauth2_token_exchange` server. A row
/// missing any of them fails at exchange time, like every other incomplete
/// credential set.
pub(crate) fn token_exchange_config(server: &McpServer) -> TokenExchangeConfig {
    TokenExchangeConfig {
        profile: server.token_exchange_profile,
        token_url: server.token_url.clone().unwrap_or_default(),
        client_id: server.client_id.clone().unwrap_or_default(),
        client_secret: server.secret.clone().unwrap_or_default(),
        scopes: server.scopes.clone().unwrap_or_default(),
        server: format!("{}\x1f{}", server.name, server.url),
    }
}

/// An [`McpBridge`] that opens a fresh upstream session for each operation and
/// drops it when done.
///
/// The downstream `/mcp` endpoint is stateless, so the gateway holds no
/// long-lived upstream connections: every `tools/list` / `tools/call` connects,
/// runs, and disconnects. Connection pooling is a later optimization; this keeps
/// the snapshot-sourced gateway free of connection-lifecycle state, so a
/// configuration change is picked up on the next request with nothing to
/// reconcile.
pub struct EphemeralBridge {
    upstream: McpUpstream,
}

impl EphemeralBridge {
    pub fn new(upstream: McpUpstream) -> Self {
        Self { upstream }
    }
}

#[async_trait]
impl McpBridge for EphemeralBridge {
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        if let McpAuth::UserOAuth(auth) = &self.upstream.auth {
            return self
                .as_signed_in_user(auth, |bridge| {
                    async move { bridge.list_tools().await }.boxed()
                })
                .await;
        }
        RmcpBridge::connect(&self.upstream)
            .await?
            .list_tools()
            .await
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, McpError> {
        if let McpAuth::UserOAuth(auth) = &self.upstream.auth {
            return self
                .as_signed_in_user(auth, |bridge| {
                    let arguments = arguments.clone();
                    async move { bridge.call_tool(name, arguments).await }.boxed()
                })
                .await;
        }
        RmcpBridge::connect(&self.upstream)
            .await?
            .call_tool(name, arguments)
            .await
    }

    async fn preflight_auth(&self) -> Result<(), McpError> {
        let claimed = self
            .upstream
            .forwarded_client_headers
            .iter()
            .any(|(name, _)| name == http::header::AUTHORIZATION);
        match &self.upstream.auth {
            McpAuth::TokenExchange(auth) if !claimed => auth
                .token(tokio::time::Instant::now() + self.upstream.timeout)
                .await
                .map(drop),
            McpAuth::UserOAuth(auth) => auth.token().await.map(drop),
            _ => Ok(()),
        }
    }
}

impl EphemeralBridge {
    /// Run `operation` with the caller's token. A `401` on an apparently
    /// valid token earns one forced refresh and one more try; that one being
    /// refused as well means the token endpoint and the server disagree, and
    /// only a new sign-in can settle it.
    ///
    /// The retry cannot repeat a tool's side effects: a server that answered
    /// `401` did not run it.
    async fn as_signed_in_user<'a, T>(
        &'a self,
        auth: &'a UserOAuthAuth,
        operation: impl Fn(RmcpBridge) -> BoxFuture<'a, Result<T, McpError>>,
    ) -> Result<T, McpError> {
        let (manager, subject) = auth.resolved()?;
        let token = auth.token().await?;
        let attempt = |token: String| {
            let upstream = McpUpstream {
                auth: McpAuth::Bearer(token),
                ..self.upstream.clone()
            };
            let operation = &operation;
            async move {
                let bridge = RmcpBridge::connect_once(&upstream).await?;
                operation(bridge).await.map_err(|error| match error {
                    McpError::Unauthorized(_) => ConnectFailure::Unauthorized(error),
                    other => ConnectFailure::Other(other),
                })
            }
        };
        let after_contact = |failure| {
            McpError::UserOAuth(crate::error::UserOAuthError {
                failure,
                reached_upstream: true,
            })
        };
        match attempt(token.clone()).await {
            Err(ConnectFailure::Unauthorized(_)) => {}
            other => return other.map_err(ConnectFailure::into_error),
        }
        let fresh = manager
            .refresh_rejected(&auth.server, subject, &token)
            .await
            .map_err(after_contact)?;
        match attempt(fresh.clone()).await {
            Err(ConnectFailure::Unauthorized(_)) => {
                manager
                    .discard_rejected(&auth.server, subject, &fresh)
                    .await;
                Err(after_contact(
                    crate::error::UserOAuthFailure::AuthorizationRequired,
                ))
            }
            other => other.map_err(ConnectFailure::into_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleartext_credential_detection() {
        let server = |auth_type: &str, url: &str, token_url: Option<&str>| -> McpServer {
            serde_json::from_value(serde_json::json!({
                "display_name": "s",
                "url": url,
                "auth_type": auth_type,
                "secret": "k",
                "client_id": "c",
                "token_url": token_url,
            }))
            .expect("valid server")
        };

        // A credentialed http:// URL is flagged; https and credential-less
        // http are not (https also starts with "http", so the scheme match
        // must include the separator). The URL parser lowercases the scheme
        // and strips leading whitespace, so those shapes flag too.
        for auth in ["bearer", "api_key"] {
            assert_eq!(
                cleartext_findings(&server(auth, "http://mcp.internal/mcp", None)).len(),
                1,
                "{auth} over http must be flagged"
            );
        }
        assert!(cleartext_findings(&server("bearer", "https://mcp.internal/mcp", None)).is_empty());
        assert!(cleartext_findings(&server("none", "http://mcp.internal/mcp", None)).is_empty());
        assert!(cleartext_findings(&server("bearer", "", None)).is_empty());
        assert_eq!(
            cleartext_findings(&server("bearer", "HTTP://mcp.internal/mcp", None)).len(),
            1
        );
        assert_eq!(
            cleartext_findings(&server("bearer", " http://mcp.internal/mcp", None)).len(),
            1
        );

        // oauth2: the server URL carries the minted token, the token URL the
        // client secret — two distinct findings, even at the same URL.
        let both = cleartext_findings(&server("oauth2", "http://x/mcp", Some("http://x/mcp")));
        assert_eq!(both.len(), 2, "token_url == url must still flag twice");
        let token_only = cleartext_findings(&server(
            "oauth2",
            "https://x/mcp",
            Some("http://idp.internal/token"),
        ));
        assert_eq!(token_only.len(), 1);
        assert_eq!(token_only[0].0, "the OAuth client secret");
    }

    /// The URL a cleartext finding carries is what the warning logs. A
    /// server or token URL can hold a credential of its own — userinfo, a
    /// tenant-secret path, a `token=`-style query — so only its origin may
    /// reach the log line.
    #[test]
    fn cleartext_findings_log_no_url_credentials() {
        let server: McpServer = serde_json::from_value(serde_json::json!({
            "display_name": "s",
            "url": "http://alice:url-pass@mcp.internal/url-path?token=url-query",
            "auth_type": "oauth2",
            "secret": "k",
            "client_id": "c",
            "token_url": "http://bob:idp-pass@idp.internal/idp-path?client_secret=idp-query",
        }))
        .expect("valid server");
        let findings = cleartext_findings(&server);
        assert_eq!(findings.len(), 2);
        for (_, logged) in &findings {
            for secret in [
                "url-pass",
                "url-path",
                "url-query",
                "idp-pass",
                "idp-path",
                "idp-query",
                "token=",
            ] {
                assert!(
                    !logged.contains(secret),
                    "logged URL leaked {secret}: {logged}"
                );
            }
        }
        // The origin stays, so the operator can tell which upstream it is.
        assert_eq!(findings[0].1, "http://mcp.internal/");
        assert_eq!(findings[1].1, "http://idp.internal/");
    }

    /// A failed `tools/list` / `tools/call` lands in the gateway log, and the
    /// transport error rmcp wraps names the request URL (reqwest moves
    /// userinfo into a header, but keeps path and query). Only the origin may
    /// appear. Built from a real failed send through rmcp's reqwest, so the
    /// text is exactly what a dropped upstream produces.
    #[tokio::test]
    async fn transport_failure_names_no_url_credentials() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let sent = rmcp_reqwest::Client::new()
            .post(format!(
                "http://alice:url-pass@127.0.0.1:{port}/url-path?token=url-query"
            ))
            .send()
            .await;
        let Err(reqwest_error) = sent else {
            panic!("a closed upstream must fail");
        };
        let error =
            ServiceError::TransportSend(DynamicTransportError::new::<
                StreamableHttpClientTransport<rmcp_reqwest::Client>,
                RoleClient,
            >(StreamableHttpError::Client(reqwest_error)));
        let logged = service_error_message(&error);
        for secret in ["url-pass", "url-path", "url-query", "token="] {
            assert!(!logged.contains(secret), "error leaked {secret}: {logged}");
        }
        assert!(
            logged.contains(&format!("http://127.0.0.1:{port}/")),
            "{logged}"
        );
    }

    /// Same rule for the `Debug` renderings, which exist to be logged.
    #[test]
    fn debug_redacts_url_credentials() {
        let mut upstream = McpUpstream::new("https://alice:url-pass@mcp.example.com/mcp?token=q1");
        upstream.auth = McpAuth::OAuth2(OAuthClientConfig {
            client_id: "cid".into(),
            client_secret: "cs".into(),
            token_url: "https://bob:idp-pass@idp.example.com/token?token=q2".into(),
            scopes: vec![],
        });
        let rendered = format!("{upstream:?}");
        for secret in ["url-pass", "idp-pass", "token=", "q1", "q2"] {
            assert!(
                !rendered.contains(secret),
                "Debug leaked {secret}: {rendered}"
            );
        }
        assert!(rendered.contains("https://mcp.example.com/"), "{rendered}");
        assert!(rendered.contains("https://idp.example.com/"), "{rendered}");
    }

    /// The dial to an upstream that swallows SYNs must be cut by the
    /// shared client's `upstream.connect_timeout_ms` (default 5 s) —
    /// before the transport ran on rmcp's default reqwest, which has no
    /// connect timeout, so the dial hung until the OUTER
    /// `upstream.timeout` (12 s here; kernel SYN retries run ≈127 s).
    /// 203.0.113.1 (TEST-NET-3) is reserved and unrouted, the standard
    /// black-hole address; on networks that answer with an immediate
    /// "unreachable" the call fails fast either way — the assertion is
    /// the upper bound, which only the connect timeout guarantees.
    /// The timing half, made independent of ambient proxy configuration.
    /// With `HTTP_PROXY` set, reqwest connects to the proxy — which accepts
    /// immediately — and the wait moves to the proxy, where `connect_timeout`
    /// does not apply; the dial then runs to the outer deadline and this
    /// assertion would fail for a reason that is not a product defect.
    #[tokio::test]
    async fn connect_timeout_bounds_a_direct_dial_to_a_black_holed_address() {
        let client = build_http_client(/* disable_env_proxy */ true);
        let started = std::time::Instant::now();
        let err = client
            .get("http://203.0.113.1:81/mcp")
            .timeout(Duration::from_secs(12))
            .send()
            .await
            .expect_err("dial to a black-holed upstream must fail");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "dial was not bounded by connect_timeout: took {:?} ({err})",
            started.elapsed()
        );
    }

    /// The wiring half: `connect` must dial through the shared client rather
    /// than rmcp's default reqwest, which has no connect timeout at all. Kept
    /// separate from the timing assertion above so an egress proxy in CI
    /// cannot turn a real regression into a red test, or hide one behind a
    /// silent skip.
    #[tokio::test]
    async fn connect_fails_fast_for_an_unreachable_upstream() {
        let mut upstream = McpUpstream::new("http://203.0.113.1:81/mcp");
        upstream.timeout = Duration::from_secs(12);
        let started = std::time::Instant::now();
        assert!(
            RmcpBridge::connect(&upstream).await.is_err(),
            "dial to a black-holed upstream must fail"
        );
        assert!(
            started.elapsed() < upstream.timeout + Duration::from_secs(2),
            "connect must be bounded by the upstream deadline: took {:?}",
            started.elapsed()
        );
    }

    /// The hand-written Debug impls are the only guard between a credential
    /// and the logs; pin them so a `#[derive(Debug)]` regression fails loudly
    /// for every secret-bearing variant.
    #[test]
    fn debug_redacts_every_credential_variant() {
        let oauth = McpAuth::OAuth2(OAuthClientConfig {
            client_id: "cid".into(),
            client_secret: "cs-LEAK".into(),
            token_url: "https://cid:basic@idp.example.com/token".into(),
            scopes: vec!["read".into()],
        });
        let upstream = McpUpstream {
            url: "https://svc:pw@mcp.example.com/mcp".into(),
            auth: McpAuth::None,
            timeout: Duration::from_secs(1),
            protocol: McpProtocol::default(),
            forwarded_client_headers: Vec::new(),
        };
        let exchange = McpAuth::TokenExchange(TokenExchangeAuth {
            config: TokenExchangeConfig {
                profile: Some(aisix_core::McpTokenExchangeProfile::EntraObo),
                token_url: "https://login.example.com/token".into(),
                client_id: "cid".into(),
                client_secret: "obo-secret-LEAK".into(),
                scopes: vec!["api://x/.default".into()],
                server: "s\x1fhttps://svc:url-LEAK@mcp.example.com/mcp".into(),
            },
            subject: Some(SubjectToken("subject-LEAK".into())),
        });
        let caller = CallerCredentials {
            delegated_authorization: Some(HeaderValue::from_static("Bearer user-LEAK")),
            subject_token: Some("jwt-LEAK".into()),
            user_oauth: None,
        };
        let rendered = format!(
            "{oauth:?} {:?} {:?} {upstream:?} {exchange:?} {caller:?} {:?}",
            McpAuth::ApiKey("key-LEAK".into()),
            McpAuth::Bearer("tok-LEAK".into()),
            McpAuth::Delegate(Some(HeaderValue::from_static("Bearer user-LEAK"))),
        );
        assert!(
            !rendered.contains("LEAK"),
            "credential leaked into Debug output: {rendered}"
        );
        // The non-secret fields stay visible for operability.
        assert!(rendered.contains("idp.example.com"));
        assert!(rendered.contains("cid"));
    }

    #[test]
    fn sanitize_error_message_strips_controls_and_truncates() {
        let injected = "HTTP 401: bad\r\n[FAKE LOG LINE] evil";
        let cleaned = sanitize_error_message(injected);
        assert!(
            !cleaned.contains('\n') && !cleaned.contains('\r'),
            "{cleaned}"
        );
        assert!(cleaned.starts_with("HTTP 401"));

        let long = format!("HTTP 502: {}", "x".repeat(1000));
        let truncated = sanitize_error_message(&long);
        assert!(truncated.chars().count() <= 257, "bounded output");
        assert!(truncated.ends_with('…'));
    }

    /// rmcp's text ends at reqwest's "error sending request"; the failed session
    /// has to reach the gateway's warn line with what actually went wrong.
    #[tokio::test]
    async fn a_failed_connect_names_its_cause() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let upstream = McpUpstream::new(format!("http://{addr}/mcp"));
        let err = RmcpBridge::connect(&upstream)
            .await
            .err()
            .expect("nothing listens");
        let text = err.to_string();
        assert!(text.to_lowercase().contains("refused"), "{text}");
    }

    /// After the session opened, a dead connection arrives two ways: the POST
    /// failing, and the response's event stream failing mid-body.
    #[tokio::test]
    async fn a_failed_request_names_its_cause() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let send = || async {
            rmcp_reqwest::get(format!("http://{addr}/mcp"))
                .await
                .expect_err("nothing listens")
        };
        let client = StreamableHttpError::<rmcp_reqwest::Error>::Client(send().await);
        let body =
            StreamableHttpError::<rmcp_reqwest::Error>::Sse(SseError::Body(Box::new(send().await)));
        for failure in [client, body] {
            let transport = rmcp::transport::DynamicTransportError::from_parts(
                "test",
                std::any::TypeId::of::<()>(),
                Box::new(failure),
            );
            let text = request_error(rmcp::ServiceError::TransportSend(transport)).to_string();
            assert!(text.to_lowercase().contains("refused"), "{text}");
        }
    }
}
