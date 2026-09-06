//! Connection-layer settings and error rendering for upstream HTTP calls.
//!
//! Every provider bridge talks to its upstream through a `reqwest::Client`.
//! Two things live here because they must be identical across all of them:
//!
//! - [`client_builder`] — a `ClientBuilder` pre-loaded with the process-wide
//!   connection settings ([`UpstreamHttpConfig`]). reqwest's own defaults
//!   leave TCP keepalive off, impose no connect timeout, and keep idle
//!   pooled connections for 90s — longer than the idle timeout of a typical
//!   LB/NAT/proxy hop in front of a provider, so a pooled connection can be
//!   reaped upstream and still be handed out here, failing the next request.
//! - [`transport_error_message`] — renders a `reqwest::Error` with its full
//!   `source()` chain. The top-level `Display` is only ever
//!   "error sending request for url (…)", which is the same string for a DNS
//!   failure, a TCP reset, a TLS handshake error, and a stale pooled
//!   connection. The chain is what tells them apart.

use std::sync::OnceLock;
use std::time::Duration;

use crate::bridge::BridgeError;
use crate::upstream_tls::TlsSettings;

/// Suffixes marking a query parameter whose value is a credential and must
/// be redacted out of logged URLs. Vertex/Gemini accept `?key=` and
/// `?access_token=`, and an operator can put either directly in a
/// ProviderKey `api_base`, so a URL echoed into a log line can carry live
/// credentials.
///
/// Matched as a **suffix** of the parameter name after lowercasing and
/// stripping `-`/`_`, which covers the vendor-prefixed and punctuation
/// variants without enumerating them: `api-key` / `api_key` / `apiKey` all
/// end in `key`; `client_secret` in `secret`; SigV4's `X-Amz-Signature`,
/// `X-Amz-Security-Token`, and `X-Amz-Credential` in `signature`, `token`,
/// and `credential`. Over-redacting an unrelated parameter costs a little
/// diagnostic detail; under-redacting leaks a live key into a log store.
const SENSITIVE_PARAM_SUFFIXES: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "credential",
    "sig",
    "signature",
];

/// Whether a query parameter's value is credential material.
fn is_sensitive_param(name: &str) -> bool {
    let normalized: String = name
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect();
    SENSITIVE_PARAM_SUFFIXES
        .iter()
        .any(|s| normalized.ends_with(s))
}

/// Cap on how many `source()` links are walked. Real reqwest/hyper chains
/// are 3-5 deep; the bound just keeps a pathological cycle from running away.
const MAX_SOURCE_DEPTH: usize = 8;

/// Connection-layer settings shared by every upstream provider client.
///
/// Defaults follow the same reasoning LiteLLM applies to its own upstream
/// pool: bound the connect phase, keep the kernel probing so a NAT/LB hop
/// can't silently reap a connection while a slow model is still thinking,
/// and expire pooled connections well before a typical upstream idle
/// timeout would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamHttpConfig {
    /// Max time for DNS + TCP + TLS before the attempt fails. Without it a
    /// black-holed upstream is only bounded by the model's overall timeout.
    pub connect_timeout: Option<Duration>,
    /// Idle time before the kernel sends the first TCP keepalive probe.
    /// Keeps a long wait for a slow first token from being reaped by a NAT
    /// or LB idle timer.
    pub tcp_keepalive: Option<Duration>,
    /// Interval between subsequent keepalive probes.
    pub tcp_keepalive_interval: Option<Duration>,
    /// Unacknowledged probes before the kernel drops the connection.
    pub tcp_keepalive_retries: Option<u32>,
    /// How long an idle connection may sit in the pool before it is
    /// discarded. Must stay below the shortest idle timeout on the path to
    /// the provider, or the pool will hand out connections the far end has
    /// already closed.
    pub pool_idle_timeout: Option<Duration>,
    /// Cap on idle connections kept per upstream host. `None` leaves
    /// reqwest's default (unbounded).
    pub pool_max_idle_per_host: Option<usize>,
    /// Trust material for the TLS handshake — see [`TlsSettings`]. Lives
    /// here rather than beside each client so the private-CA / mTLS /
    /// verification decision is made once and reaches every outbound
    /// stack, not just the ones someone remembered to wire.
    pub tls: TlsSettings,
}

impl Default for UpstreamHttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Some(Duration::from_secs(5)),
            tcp_keepalive: Some(Duration::from_secs(60)),
            tcp_keepalive_interval: Some(Duration::from_secs(30)),
            tcp_keepalive_retries: Some(5),
            pool_idle_timeout: Some(Duration::from_secs(30)),
            pool_max_idle_per_host: None,
            tls: TlsSettings::default(),
        }
    }
}

static CONFIG: OnceLock<UpstreamHttpConfig> = OnceLock::new();

/// Install the process-wide upstream connection settings. Called once
/// during boot, before any bridge builds its client. Later calls are
/// ignored — the pools are already built, so a second set would silently
/// not apply.
///
/// Fails when the configured TLS material does not parse, which is the
/// point at which a wrong `upstream.tls.ca_file` should stop the boot
/// rather than become a transport error on the first upstream call.
pub fn init(cfg: UpstreamHttpConfig) -> Result<(), String> {
    crate::upstream_tls::init_reqwest_material(&cfg.tls)?;
    let _ = CONFIG.set(cfg);
    Ok(())
}

/// The active settings, defaulting when [`init`] was never called (tests,
/// embedded uses).
pub fn config() -> &'static UpstreamHttpConfig {
    CONFIG.get_or_init(UpstreamHttpConfig::default)
}

/// Finish a [`client_builder`], or say loudly why the settings were lost.
///
/// A failed `build()` yields a client with **none** of the deployment's
/// outbound TLS trust: no private CA, no client identity, and no
/// `verify: false` opt-out either. Connections then fail against an
/// internally-signed upstream, or quietly stop presenting the certificate
/// the upstream expects — and swallowing the error leaves nothing pointing
/// at the cause.
///
/// Callers that would rather not start at all keep using `expect`; this is
/// for the ones on a request path, where the choice is between a degraded
/// client and a panic per request. `what` names the caller in the log.
pub fn build_client_or_default(builder: reqwest::ClientBuilder, what: &str) -> reqwest::Client {
    match builder.build() {
        Ok(client) => client,
        Err(e) => {
            tracing::error!(
                error = %e,
                client = what,
                "upstream.tls settings not applied: the configured HTTP client                  could not be built, falling back to a default one (no custom CA,                  no client identity, no verify opt-out)"
            );
            reqwest::Client::new()
        }
    }
}

/// A `reqwest::ClientBuilder` with the connection settings **and the
/// deployment's outbound TLS trust** applied. Callers add their own
/// `user_agent` and `build()`.
pub fn client_builder() -> reqwest::ClientBuilder {
    let cfg = config();
    let mut b = reqwest::Client::builder()
        // Do not follow redirects. reqwest's default follows up to ten, and
        // on a cross-host hop it strips only `Authorization`, `Cookie`,
        // `cookie2`, `Proxy-Authorization` and `WWW-Authenticate`
        // (reqwest 0.12 `redirect.rs::remove_sensitive_headers`). The two
        // credential headers this gateway sends most are neither: Anthropic
        // takes `x-api-key` and Azure takes `api-key`, both custom, both
        // forwarded verbatim. So an upstream answering `302 Location:
        // http://elsewhere/` had the operator's provider credential — and the
        // caller's prompt — re-sent to whatever it named.
        //
        // This client is shared by every outbound purpose: provider dispatch,
        // the video and file content routes, remote guardrails, A2A, OTLP. The
        // content routes are the only ones where following could plausibly be
        // wanted, and this repo's own notes say those endpoints return bytes
        // rather than a redirect (`videos.rs`: the OpenAI job "carries no
        // downloadable URL, so the gateway streams the bytes back"); the video
        // provider that DOES hand back a URL 302s the caller instead of
        // fetching it. A passthrough route relaying a 3xx to its caller is
        // likewise more faithful than silently resolving it here.
        //
        // Same policy, and the same reasoning, as the console's and portal's
        // outbound clients — which have asserted it for longer than this one
        // has had it. If an upstream ever does need a redirect followed, the
        // symptom is a visible 3xx rather than a leaked credential.
        .redirect(reqwest::redirect::Policy::none())
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
    apply_tls(b, &cfg.tls)
}

/// Layer the outbound trust decision onto a builder. Split out so the
/// per-ProviderKey clients get byte-for-byte the same treatment as the
/// shared one.
pub(crate) fn apply_tls(
    mut b: reqwest::ClientBuilder,
    tls: &TlsSettings,
) -> reqwest::ClientBuilder {
    let material = crate::upstream_tls::reqwest_material();
    for root in &material.roots {
        b = b.add_root_certificate(root.clone());
    }
    if let Some(identity) = &material.identity {
        b = b.identity(identity.clone());
    }
    if !tls.verify {
        b = b.danger_accept_invalid_certs(true);
    }
    b
}

/// Render a `reqwest::Error` as a single diagnostic line: the top-level
/// message followed by every distinct `source()` cause, with credentials
/// stripped from any embedded URL.
///
/// reqwest's own `Display` stops at "error sending request for url (…)",
/// which is identical for a DNS failure, a refused connection, a TLS
/// handshake error, and a pooled connection the far end already closed.
/// The causes below it are what name the actual fault, e.g.
/// `… : client error (Connect): tcp connect error: Connection refused (os error 111)`.
pub fn transport_error_message(err: &reqwest::Error) -> String {
    let mut msg = err.to_string();
    if let Some(url) = err.url() {
        let raw = url.as_str();
        if msg.contains(raw) {
            msg = msg.replace(raw, &redact_url(url));
        }
    }
    append_source_chain(&mut msg, err);
    msg
}

/// Classify a `reqwest` **send** failure into its [`BridgeError`].
///
/// reqwest reports a *builder* error when the request could not even be
/// constructed. In practice that is an `api_base` that does not parse as a
/// URL: [`crate::url_cache::EndpointUrl::Unparsed`] deliberately hands the
/// raw string to the request builder so the message stays exactly what it
/// always was, and the parse failure then surfaces here at `send()` with
/// `is_builder()` set and no URL attached.
///
/// Nothing was sent, so this is customer-fixable upstream config — the same
/// class as a *missing* `api_base`, which already maps to
/// [`BridgeError::InvalidUpstreamConfig`] — rather than a transport failure.
/// Calling it `Transport` would report a 502 for an operator's typo, retry
/// a URL that can never parse, and (via
/// [`BridgeError::reached_upstream`]) count it against the target's
/// `aisix_deployment_*` health even though no provider was contacted.
///
/// Use at `send()` sites only. A failure reading an already-open response
/// body or stream is never a builder error and stays [`BridgeError::Transport`].
pub fn send_error(err: reqwest::Error) -> BridgeError {
    if err.is_builder() {
        BridgeError::InvalidUpstreamConfig(transport_error_message(&err))
    } else {
        BridgeError::Transport(transport_error_message(&err))
    }
}

/// Same as [`transport_error_message`] for error types that aren't
/// `reqwest::Error` (websocket handshakes, SDK dispatch errors) — no URL
/// is available to redact, so only the cause chain is appended.
pub fn error_with_causes(err: &(dyn std::error::Error + 'static)) -> String {
    let mut msg = err.to_string();
    append_source_chain(&mut msg, err);
    msg
}

fn append_source_chain(msg: &mut String, err: &(dyn std::error::Error + 'static)) {
    let mut source = err.source();
    let mut depth = 0;
    while let Some(cause) = source {
        if depth >= MAX_SOURCE_DEPTH {
            break;
        }
        let text = cause.to_string();
        // hyper repeats the innermost message at several levels; only add
        // a cause that isn't already the tail of what we have.
        if !text.is_empty() && !msg.ends_with(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
        source = cause.source();
        depth += 1;
    }
}

/// Replace the values of credential-bearing query parameters with
/// `REDACTED`, leaving everything else (host, path, `api-version`, …)
/// intact for diagnosis.
fn redact_url(url: &reqwest::Url) -> String {
    if url.query().is_none() {
        return url.as_str().to_string();
    }
    let mut out = url.clone();
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| {
            if is_sensitive_param(&k) {
                (k.into_owned(), "REDACTED".to_string())
            } else {
                (k.into_owned(), v.into_owned())
            }
        })
        .collect();
    {
        let mut q = out.query_pairs_mut();
        q.clear();
        for (k, v) in &pairs {
            q.append_pair(k, v);
        }
    }
    out.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bound_connect_and_expire_idle_before_reqwest_would() {
        let cfg = UpstreamHttpConfig::default();
        assert!(cfg.connect_timeout.is_some(), "connect must be bounded");
        assert!(cfg.tcp_keepalive.is_some(), "keepalive must be on");
        // reqwest's own default is 90s; anything at or above that reopens
        // the stale-pooled-connection window this config exists to close.
        assert!(cfg.pool_idle_timeout.unwrap() < Duration::from_secs(90));
    }

    #[test]
    fn client_builder_applies_settings() {
        // Smoke: the builder must accept every configured knob.
        let client = client_builder().user_agent("aisix-test").build();
        assert!(client.is_ok(), "{:?}", client.err());
    }

    /// Every outbound HTTP client in the workspace must be built from
    /// [`client_builder`], or it silently keeps reqwest's defaults — no
    /// connect timeout, TCP keepalive off, and a 90s pooled-connection
    /// lifetime that outlives a typical hop's idle timeout. Nothing else
    /// catches that: such a client works fine until a load balancer
    /// starts reaping connections it still considers usable.
    ///
    /// The first pass of this (#1122) converted the provider
    /// bridges only, leaving the guardrail, MCP, A2A, telemetry, and
    /// exporter clients on the defaults — which is what this test exists
    /// to stop repeating (#1126).
    #[test]
    fn no_production_code_builds_a_bare_reqwest_client() {
        let crates_dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."));
        let mut offenders = Vec::new();
        for file in rust_sources(crates_dir) {
            let src = std::fs::read_to_string(&file).expect("read source");
            // Tests build throwaway clients on purpose; only the
            // production half of each file is in scope.
            let production = production_half(&src);
            // `shared_http_client()` in aisix-mcp is the sanctioned
            // counterpart of `client_builder()` for rmcp's own reqwest
            // line (rmcp pins 0.13; it gets the same
            // `upstream_http::config()` values applied). The exemption is
            // scoped to that one file so a bare rmcp client anywhere else
            // still gets flagged.
            let sanctioned_rmcp_site = file.ends_with("aisix-mcp/src/bridge.rs");
            // The console is a separate binary that never loads gateway
            // config, so `client_builder()` — which reads the global
            // `UpstreamHttpConfig` — has nothing to read there. It pins the
            // same connect timeout / keepalive / pool-idle values literally
            // instead; `console_client_is_not_left_on_reqwest_defaults`
            // below is what holds it to them.
            let sanctioned_console_site = file.ends_with("aisix-console/src/main.rs");
            // The portal is in the same position as the console: a separate
            // binary that never loads gateway config, so `client_builder()`
            // has no `UpstreamHttpConfig` to read. It pins the same values
            // literally in one place, and
            // `portal_client_is_not_left_on_reqwest_defaults` holds it to
            // them. Scoped to that one file so a bare client anywhere else
            // in the portal still gets flagged.
            let sanctioned_portal_site = file.ends_with("aisix-portal/src/client.rs");
            for (n, line) in production.lines().enumerate() {
                if sanctioned_rmcp_site && line.contains("rmcp_reqwest::Client::") {
                    continue;
                }
                if sanctioned_console_site || sanctioned_portal_site {
                    continue;
                }
                if line.contains("reqwest::Client::builder()")
                    || line.contains("reqwest::Client::new()")
                {
                    offenders.push(format!("{}:{}", file.display(), n + 1));
                }
            }
        }
        // This module is where the shared builder is defined.
        offenders.retain(|o| !o.contains("upstream_http.rs"));
        assert!(
            offenders.is_empty(),
            "these build a reqwest client directly instead of \
             `aisix_gateway::client_builder()`:\n{}",
            offenders.join("\n"),
        );
    }

    /// The gateway's OWN client is held to the rule its two siblings were
    /// already held to.
    ///
    /// The console's and portal's clients each had a test requiring
    /// `redirect(`, and the comments there name the exact hazard: reqwest
    /// follows up to ten redirects and strips only `Authorization`, `Cookie`,
    /// `cookie2`, `Proxy-Authorization` and `WWW-Authenticate` cross-host, so
    /// a custom credential header rides along. This client — the one that
    /// actually carries provider credentials on every request, `x-api-key`
    /// for Anthropic and `api-key` for Azure — was the one with no such test
    /// and no such setting.
    #[test]
    fn the_shared_upstream_client_does_not_follow_redirects() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/upstream_http.rs"))
                .expect("this module is readable");
        let builder = src
            .split_once("pub fn client_builder() -> reqwest::ClientBuilder {")
            .expect("client_builder is declared here")
            .1
            .split_once("\n}")
            .expect("client_builder is closed")
            .0;
        // Comment lines do not count: the explanation survives deleting the
        // call, and a guard satisfied by its own rationale guards nothing.
        assert!(
            builder
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains(".redirect(")),
            "`client_builder` sets no redirect policy, so reqwest's default \
             follows up to ten hops and forwards `x-api-key` / `api-key` — \
             the operator's provider credential — to whatever an upstream \
             names in a cross-host `Location`."
        );
    }

    /// The console is exempted from the scan above because it cannot call
    /// `client_builder()` — but an exemption with nothing behind it just
    /// removes it from the guard. This is what keeps it honest: its client
    /// must still set every default reqwest gets wrong, or the exemption is
    /// hiding exactly the bug the scan exists to catch.
    ///
    /// Asserted against the source rather than the built client because
    /// `reqwest::Client` exposes none of these settings for readback.
    #[test]
    fn console_client_is_not_left_on_reqwest_defaults() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../aisix-console/src/main.rs"
        ));
        let Ok(src) = std::fs::read_to_string(path) else {
            // The console is an optional member; absent is not a failure.
            return;
        };
        let production = production_half(&src);
        for setting in [
            "connect_timeout(",
            "tcp_keepalive(",
            "pool_idle_timeout(",
            // Redirects off: the console validates only the first hop
            // before dialling an operator-supplied address, so following a
            // 302 walks straight past that check — and reqwest strips only
            // the standard auth headers cross-host, not the `x-api-key` that
            // path sends.
            "redirect(",
        ] {
            assert!(
                production.contains(setting),
                "the console's outbound client does not set `{setting}` — \
                 reqwest's default there is what the workspace scan exists \
                 to prevent, and the console is exempt from that scan",
            );
        }
    }

    /// The portal's exemption from the scan above is conditional on it
    /// pinning the same values `client_builder()` would have applied. Same
    /// reasoning as the console: reqwest's defaults leave no connect
    /// timeout, TCP keepalive off, and a 90s pooled-connection lifetime that
    /// outlives a typical hop's idle timeout, which shows up as an
    /// occasional failure that succeeds on retry.
    #[test]
    fn portal_client_is_not_left_on_reqwest_defaults() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../aisix-portal/src/client.rs"
        ));
        let Ok(src) = std::fs::read_to_string(path) else {
            // The portal is an optional member; absent is not a failure.
            return;
        };
        let production = production_half(&src);
        for setting in [
            "connect_timeout(",
            "tcp_keepalive(",
            "pool_idle_timeout(",
            // Redirects off: the portal dials operator-configured addresses,
            // and following a 302 hands the choice of destination to whoever
            // answered.
            "redirect(",
        ] {
            assert!(
                production.contains(setting),
                "the portal's outbound client does not set `{setting}` —                  reqwest's default there is what the workspace scan exists                  to prevent, and the portal is exempt from that scan",
            );
        }
    }

    /// The outbound stacks that are *not* reqwest each have exactly one
    /// sanctioned construction site, and each of those sites is the only
    /// thing standing between `upstream.tls` and a client that quietly
    /// trusts the wrong set of roots.
    ///
    /// Nothing else catches a regression here: a client built without the
    /// shared trust material works perfectly against every public
    /// provider and fails only against the private CA the setting exists
    /// for — which is to say, only in the customer's environment.
    ///
    /// Each entry is (probe, what the file must also mention, why).
    #[test]
    fn every_non_reqwest_outbound_stack_applies_the_shared_tls_settings() {
        const RULES: &[(&str, &str, &str)] = &[
            (
                // Catches a *new* WebSocket call site: `connect_async`
                // builds its own connector over webpki roots only.
                "tokio_tungstenite::connect_async(",
                "rustls_client_config",
                "the Realtime WebSocket must dial through the shared rustls config \
                 (`connect_async` builds its own connector over webpki roots only)",
            ),
            (
                // Catches the existing call site being weakened — passing
                // `Connector::Plain` or `None` still compiles and still
                // connects, just to a different set of roots.
                "connect_async_tls_with_config",
                "rustls_client_config",
                "the Realtime WebSocket connector must come from \
                 `upstream_tls::rustls_client_config()`",
            ),
            (
                "aws_config::SdkConfig::builder()",
                "aws_http_client",
                "Bedrock SDK clients must be built on `upstream_tls::aws_http_client()`",
            ),
            (
                "AmazonS3Builder::",
                "tls_client_options",
                "object-store exporters must pass `tls_client_options()` as client options",
            ),
            (
                "MicrosoftAzureBuilder::",
                "tls_client_options",
                "object-store exporters must pass `tls_client_options()` as client options",
            ),
            (
                "GoogleCloudStorageBuilder::",
                "tls_client_options",
                "object-store exporters must pass `tls_client_options()` as client options",
            ),
        ];

        let crates_dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."));
        let mut offenders = Vec::new();
        for file in rust_sources(crates_dir) {
            let src = std::fs::read_to_string(&file).expect("read source");
            let production = production_half(&src);
            for (probe, required, why) in RULES {
                if production.contains(probe) && !production.contains(required) {
                    offenders.push(format!("{}: {}", file.display(), why));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these reach an external service without the deployment's outbound TLS \
             trust:\n{}",
            offenders.join("\n"),
        );
    }

    /// The part of a source file that is not the test module.
    ///
    /// Cuts at the top-level `#[cfg(test)] mod …` specifically, not at
    /// the first `#[cfg(test)]` anywhere: that attribute is also used on
    /// struct fields and match arms, several of which appear near the
    /// top of a file, and cutting there silently excused everything
    /// below from every scan in this module.
    fn production_half(src: &str) -> &str {
        const MARKER: &str = "\n#[cfg(test)]\nmod ";
        match src.find(MARKER) {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// The scans above are only worth their runtime if they actually see
    /// the whole file — an early `#[cfg(test)]` field attribute used to
    /// hide every call site under it.
    #[test]
    fn production_half_cuts_at_the_test_module_not_a_field_attribute() {
        let src = "struct S {\n    #[cfg(test)]\n    probe: bool,\n}\nfn f() {}\n\
                   #[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
        let production = production_half(src);
        assert!(production.contains("fn f()"), "{production}");
        assert!(!production.contains("fn t()"), "{production}");
    }

    /// On a thread-per-core worker every dispatch runs on that worker's
    /// own pool, and that one pool stands in for all of the clients
    /// below — so it is built with a single user agent
    /// (`upstream_tls::DISPATCH_USER_AGENT`).
    ///
    /// That substitution is only invisible while the clients it replaces
    /// agree on the user agent. Give one bridge its own and the header
    /// it sends changes depending on which serving mode the deployment
    /// runs, which is not something an upstream-facing identity is
    /// allowed to do. Whoever wants a distinct user agent has to give
    /// that client a distinct pool as well.
    #[test]
    fn every_dispatch_client_presents_the_same_user_agent() {
        let crates_dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."));
        // Only literals: the telemetry, heartbeat, and OTLP clients build
        // theirs from a version string or a named constant, and none of
        // them talks to a model provider or reaches the per-worker pool.
        const NEEDLE: &str = ".user_agent(\"";
        let mut offenders = Vec::new();
        for file in rust_sources(crates_dir) {
            let src = std::fs::read_to_string(&file).expect("read source");
            for (n, line) in production_half(&src).lines().enumerate() {
                let Some(rest) = line.split_once(NEEDLE).map(|(_, r)| r) else {
                    continue;
                };
                let Some((agent, _)) = rest.split_once('"') else {
                    continue;
                };
                if agent != crate::upstream_tls::DISPATCH_USER_AGENT {
                    offenders.push(format!("{}:{}: {agent}", file.display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these dispatch clients present a user agent the per-worker \
             pool would replace with `{}`:\n{}",
            crate::upstream_tls::DISPATCH_USER_AGENT,
            offenders.join("\n"),
        );
    }

    fn rust_sources(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "tests" || n == "target")
                {
                    continue;
                }
                out.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn redacts_credential_query_params_only() {
        let url = reqwest::Url::parse(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini:generateContent\
             ?key=AIzaSy-super-secret&alt=sse",
        )
        .unwrap();
        let out = redact_url(&url);
        assert!(!out.contains("AIzaSy-super-secret"), "{out}");
        assert!(out.contains("key=REDACTED"), "{out}");
        // Non-credential params stay readable — they're the diagnostic bit.
        assert!(out.contains("alt=sse"), "{out}");
    }

    #[test]
    fn redacts_access_token_case_insensitively() {
        let url =
            reqwest::Url::parse("https://example.com/v1/chat?Access_Token=ya29.live&x=1").unwrap();
        let out = redact_url(&url);
        assert!(!out.contains("ya29.live"), "{out}");
        assert!(out.contains("x=1"), "{out}");
    }

    /// Suffix matching exists so vendor-prefixed and punctuation variants
    /// don't have to be enumerated one by one — each of these would have
    /// slipped through an exact-name denylist.
    #[test]
    fn redacts_credential_parameter_aliases() {
        for name in [
            "api-key",
            "api_key",
            "apiKey",
            "client_secret",
            "client-secret",
            "X-Amz-Signature",
            "X-Amz-Security-Token",
            "X-Amz-Credential",
            "SIG",
            "refresh_token",
            "subscription-key",
        ] {
            let url = reqwest::Url::parse(&format!("https://h/p?{name}=live-secret-value&keep=1"))
                .unwrap();
            let out = redact_url(&url);
            assert!(
                !out.contains("live-secret-value"),
                "{name} was not redacted: {out}"
            );
            assert!(out.contains("keep=1"), "{name} over-redacted: {out}");
        }
    }

    /// The flip side: parameters that merely look credential-ish must stay
    /// readable, since they are the diagnostic content of the URL.
    #[test]
    fn keeps_non_credential_parameters_readable() {
        let url = reqwest::Url::parse(
            "https://h/p?api-version=2024-10-21&alt=sse&keyword=hello&signature_version=4",
        )
        .unwrap();
        let out = redact_url(&url);
        assert!(out.contains("api-version=2024-10-21"), "{out}");
        assert!(out.contains("alt=sse"), "{out}");
        assert!(out.contains("keyword=hello"), "{out}");
        assert!(out.contains("signature_version=4"), "{out}");
    }

    #[test]
    fn url_without_query_is_untouched() {
        let url = reqwest::Url::parse("https://api.openai.com/v1/chat/completions").unwrap();
        assert_eq!(
            redact_url(&url),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_ref()
                .map(|b| b.as_ref() as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn cause_chain_is_flattened_into_one_line() {
        let err = Layer(
            "error sending request",
            Some(Box::new(Layer(
                "client error (Connect)",
                Some(Box::new(Layer("tcp connect error: refused", None))),
            ))),
        );
        assert_eq!(
            error_with_causes(&err),
            "error sending request: client error (Connect): tcp connect error: refused"
        );
    }

    #[test]
    fn repeated_tail_cause_is_not_duplicated() {
        // hyper commonly restates the innermost message one level up.
        let err = Layer("outer: refused", Some(Box::new(Layer("refused", None))));
        assert_eq!(error_with_causes(&err), "outer: refused");
    }

    /// The whole point of `transport_error_message`, against a real
    /// `reqwest::Error` rather than a hand-built chain: reqwest's own
    /// `Display` is "error sending request for url (…)" for a refused
    /// connection, a DNS failure, a TLS error, and a stale pooled
    /// connection alike. Operators can't tell those apart, which is what
    /// made #1122 undiagnosable from the logs.
    #[tokio::test]
    async fn real_transport_error_names_the_root_cause() {
        // Bind an ephemeral loopback port and immediately release it, so the
        // connect is refused straight away without assuming any fixed port
        // is free (no timeout, no external network needed).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let client = client_builder().build().expect("client builds");
        let err = client
            .get(format!("http://{addr}/v1/chat/completions"))
            .send()
            .await
            .expect_err("connect to a closed port must fail");

        let top_level = err.to_string();
        let with_causes = transport_error_message(&err);

        assert!(
            !top_level.to_lowercase().contains("refused"),
            "reqwest's Display is expected to hide the cause; got: {top_level}"
        );
        assert!(
            with_causes.to_lowercase().contains("refused"),
            "the cause chain must name the actual fault; got: {with_causes}"
        );
        assert!(
            with_causes.len() > top_level.len(),
            "causes must add information"
        );
    }

    /// The distinction `send_error` exists to make. `EndpointUrl::Unparsed`
    /// hands a malformed `api_base` to the request builder verbatim, and
    /// reqwest reports the parse failure only here, at `send()`, as a
    /// builder error with no URL attached. Classifying it as `Transport`
    /// would 502 an operator's typo, retry a URL that can never parse, and
    /// count it against the target's upstream health.
    #[tokio::test]
    async fn builder_errors_are_upstream_config_not_transport() {
        let client = reqwest::Client::new();
        let builder_err = crate::url_cache::EndpointUrl::Unparsed("ht tp://not a url".to_string())
            .post_on(&client)
            .send()
            .await
            .expect_err("a malformed api_base cannot produce a response");
        assert!(builder_err.is_builder());
        assert!(matches!(
            send_error(builder_err),
            BridgeError::InvalidUpstreamConfig(_)
        ));

        // A real connection attempt to a closed port stays transport: we
        // did try to reach the provider, and that is upstream health.
        let io_err = client
            .post("http://127.0.0.1:1/v1/chat/completions")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert!(!io_err.is_builder());
        assert!(matches!(send_error(io_err), BridgeError::Transport(_)));
    }

    /// A `client_builder()` may not be finished by silently discarding the
    /// error.
    ///
    /// The fallback client carries none of the deployment's outbound TLS
    /// trust, so swallowing the failure turns a misconfiguration into
    /// connections that fail — or stop presenting a client certificate —
    /// with nothing naming the cause. There were seven of these across the
    /// bridges and the proxy, in three different spellings; they now go
    /// through [`build_client_or_default`], which says what was lost.
    ///
    /// `expect` stays allowed: refusing to start is a legitimate answer for
    /// a caller that is not on a request path.
    #[test]
    fn no_caller_finishes_a_client_builder_by_swallowing_the_error() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/")
            .to_path_buf();

        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>, seen: &mut usize) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out, seen);
                    continue;
                }
                if path.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let production = src
                    .split_once("\n#[cfg(test)]")
                    .map_or(&src[..], |(a, _)| a);
                for hit in production.match_indices("client_builder()") {
                    *seen += 1;
                    let tail = &production[hit.0..];
                    // 按字符切，不按字节 —— 注释里有中文，字节切会切在 UTF-8
                    // 码点中间直接 panic。
                    let window: String = tail.chars().take(400).collect();
                    let window = window.as_str();
                    // `build()` 之后紧跟的那一句决定了失败怎么处理。
                    if window.contains("unwrap_or_else(|_|")
                        || window.contains("unwrap_or_default()")
                    {
                        out.push((
                            path.display().to_string(),
                            window.lines().take(6).collect::<Vec<_>>().join(" | "),
                        ));
                    }
                }
            }
        }

        let mut offenders = Vec::new();
        let mut seen = 0usize;
        walk(&root, &mut offenders, &mut seen);
        // 「不许出现」型的断言，扫不到东西就恒绿 —— 所以先证明扫到了。
        assert!(
            seen >= 8,
            "只扫到 {seen} 处 client_builder() 调用，抽取多半坏了，这条守卫成了空转"
        );
        assert!(
            offenders.is_empty(),
            "这些地方把 client_builder() 的失败吞掉了 —— 换来的客户端没有部署配\
             的任何 TLS 设置，而且没有一行日志说明：{offenders:#?}"
        );
    }
}
