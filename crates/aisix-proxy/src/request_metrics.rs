//! The one chokepoint for the per-request outcome metrics every handler
//! emits once dispatch has produced a response.
//!
//! # What `elapsed` measures, and why it is not end-to-end
//!
//! Handlers call [`record`] on their way out, so for a **streamed** response
//! the `elapsed` they pass is time to response START, not the full
//! generation — the SSE body has not been polled yet. Every duration series
//! fed from here therefore mixes two scopes: full request time for
//! non-streamed traffic, time-to-response-start for streamed. `chat.rs`
//! guards the SLO histogram against exactly this
//! (`record_request_e2e_latency` is called with the stream's own duration at
//! completion instead); nothing guards the three families below.
//!
//! Read a streaming p99 off `aisix_request_e2e_latency_seconds`, which is
//! recorded at stream completion. Do not read one off
//! `aisix_llm_request_duration_seconds` and expect end-to-end.
//!
//! Three families ride on a single call:
//!
//! - `aisix_requests_total` / `aisix_request_duration_seconds` — the legacy
//!   compatibility series, four labels, every endpoint.
//! - `aisix_proxy_requests_total` / `aisix_proxy_failed_requests_total` /
//!   `aisix_proxy_request_duration_seconds` — the detailed series over ALL
//!   proxied traffic.
//! - `aisix_llm_requests_total` / `aisix_llm_request_duration_seconds` — the
//!   subset of the above that is a model-inference call, per
//!   [`LLM_ENDPOINTS`].
//!
//! Splitting the two tiers is the point: an MCP tool call, a batch-file
//! upload and a 413 are all proxy requests, but counting them as LLM
//! requests would corrupt every per-request token/cost average and the LLM
//! success rate. What is NOT a judgement call is that both tiers must cover
//! every endpoint — before #1234 only chat + messages emitted the
//! detailed families at all, so ten endpoints were absent from the
//! success-rate and request-count queries built on them while still showing
//! up in the legacy series.
//!
//! [`record_usage`] is the companion emit for what the request consumed —
//! the token and spend families, which had the same coverage problem, in
//! three different shapes (see its own docs).
//!
//! Handlers call [`record`] and [`record_usage`] instead of touching
//! `Metrics` directly, and the tier is decided from the endpoint rather than
//! by the caller, so a new endpoint cannot land with a half-wired label set —
//! the same anti-drift move `usage_attr` makes for the UsageEvent side.
//!
//! Two endpoints are model inference but report no tokens by nature —
//! `/v1/audio/speech` (billed per input character) and `/v1/videos` (per
//! video). They count as requests and contribute nothing to the token
//! families, so aggregate tokens-per-request is only meaningful per
//! `endpoint`, never summed across all of them.

use std::time::Duration;

use aisix_obs::{LlmUsage, RequestLabels, RequestOutcome, UsageLabels};

use crate::auth::AuthenticatedKey;
use crate::state::ProxyState;
use crate::usage_attr::PkLabels;

/// Label value every `RequestLabels` field falls back to when the path
/// never resolved it. Matches `RequestLabels::default()`.
const UNKNOWN: &str = "unknown";

/// Caller identity for the detailed label set.
#[derive(Clone, Copy)]
pub(crate) struct Caller<'a> {
    pub api_key_id: &'a str,
    pub team_id: &'a str,
    pub user_id: &'a str,
    pub user_name: &'a str,
}

impl<'a> Caller<'a> {
    pub(crate) fn new(auth: &'a AuthenticatedKey) -> Self {
        let key = auth.key();
        Self {
            api_key_id: &auth.entry.id,
            team_id: key.team_id.as_deref().unwrap_or(UNKNOWN),
            user_id: key.user_id.as_deref().unwrap_or(UNKNOWN),
            user_name: key.user_name.as_deref().unwrap_or(UNKNOWN),
        }
    }

    /// Recover the caller from an api-key id alone.
    ///
    /// The streaming emits run from a detached task or a Drop guard that was
    /// handed an `api_key_id: &str` rather than the key itself, and threading
    /// the team / user / name triple down every dispatch signature to reach
    /// them would be a lot of plumbing for three labels. The id resolves back
    /// to the same row the auth extractor matched, so the labels come out
    /// identical to [`Caller::new`]; an id that no longer resolves (the key
    /// was deleted mid-stream) degrades to `unknown` rather than dropping the
    /// sample.
    pub(crate) fn from_api_key_id(
        snap: &aisix_core::AisixSnapshot,
        api_key_id: &'a str,
    ) -> Owned<'a> {
        let entry = snap.apikeys.get_by_id(api_key_id);
        let key = entry.as_ref().map(|e| &*e.value);
        Owned {
            api_key_id,
            team_id: key.and_then(|k| k.team_id.clone()),
            user_id: key.and_then(|k| k.user_id.clone()),
            user_name: key.and_then(|k| k.user_name.clone()),
        }
    }

    /// A path that gave up before it could attribute the request to a team
    /// or user — the pre-dispatch rejections. `api_key_id` is `Some` once
    /// the auth extractor has run and `None` for the middleware
    /// short-circuits that precede it (see `reject`).
    pub(crate) fn unattributed(api_key_id: Option<&'a str>) -> Self {
        Self {
            api_key_id: api_key_id.unwrap_or(UNKNOWN),
            team_id: UNKNOWN,
            user_id: UNKNOWN,
            user_name: UNKNOWN,
        }
    }
}

/// Owning form of [`Caller`], for the snapshot lookup whose strings cannot
/// outlive the guard. Call [`Owned::as_caller`] at the emit.
pub(crate) struct Owned<'a> {
    api_key_id: &'a str,
    team_id: Option<String>,
    user_id: Option<String>,
    user_name: Option<String>,
}

impl<'a> Owned<'a> {
    pub(crate) fn as_caller(&'a self) -> Caller<'a> {
        Caller {
            api_key_id: self.api_key_id,
            team_id: self.team_id.as_deref().unwrap_or(UNKNOWN),
            user_id: self.user_id.as_deref().unwrap_or(UNKNOWN),
            user_name: self.user_name.as_deref().unwrap_or(UNKNOWN),
        }
    }
}

/// What the handler resolved about the upstream it reached, or tried to.
/// [`Upstream::default()`] is the shape of a request that failed before
/// resolution; a handler fills in only the fields its endpoint has.
#[derive(Clone, Copy)]
pub(crate) struct Upstream<'a> {
    pub provider: &'a str,
    /// MUST be bounded: a name that already resolved against the snapshot,
    /// or `usage_attr::metric_model_label()` output on any path that can
    /// fire before resolution. The raw client-supplied `model` is
    /// attacker-controlled cardinality (#451).
    pub model: &'a str,
    pub upstream_model: &'a str,
    /// The attempt's ProviderKey id AND its readable name, resolved
    /// together by `usage_attr::ResolvedPk` (#941). Taking the pair rather
    /// than a bare id is deliberate: the name used to be looked up inside
    /// each emit, so a request paid one snapshot read per emit and a new
    /// call site could not tell it was doing so.
    pub pk: PkLabels<'a>,
    pub stream: bool,
    pub is_fallback: bool,
}

impl Default for Upstream<'_> {
    fn default() -> Self {
        Self {
            provider: UNKNOWN,
            model: UNKNOWN,
            upstream_model: UNKNOWN,
            pk: PkLabels::default(),
            stream: false,
            is_fallback: false,
        }
    }
}

/// Endpoints whose requests belong in the `aisix_llm_*` families on top of
/// the `aisix_proxy_*` ones — the model-inference routes.
///
/// Values are `normalize_endpoint_label` outputs; `llm_endpoints_are_reachable`
/// pins that, because a typo here fails silently (the entry simply never
/// matches, and the endpoint quietly drops out of every LLM query).
///
/// Deliberately absent, and why:
/// - `/mcp`, `/mcp/{server}`, `/a2a` — tool and agent calls, no model.
/// - `/passthrough_route` — an opaque relay; even a `protocol`-aware route
///   resolves no configured Model to attribute.
/// - `/v1/files`, `/v1/batches`, `/v1/fine_tuning/jobs` — management calls.
///
/// `/v1/realtime` was in that list until the token families reached it too.
/// It was held out because it fed none of them, so counting it here would
/// have inflated the denominator of every tokens-per-request query; now that
/// a session reports its tokens and cost, that reason is gone and it belongs
/// with the rest.
const LLM_ENDPOINTS: &[&str] = &[
    "/v1/chat/completions",
    "/v1/completions",
    "/v1/embeddings",
    "/v1/images/generations",
    "/v1/messages",
    "/v1/messages/count_tokens",
    "/v1/rerank",
    "/v1/responses",
    "/v1/audio/transcriptions",
    "/v1/audio/translations",
    "/v1/audio/speech",
    "/v1/videos",
    "/v1/videos/:id",
    "/v1/realtime",
];

/// Whether this endpoint's requests are model inference.
///
/// Keyed off the route, not the call site, so a request lands in the same
/// families however it ended — a 413 refused before dispatch has to sit in
/// the same denominator as the model-not-found 404 the handler itself
/// records, or a success rate over the endpoint silently omits one of them.
///
/// Anything unlisted is proxy-only, the safe default: a wrong `false` loses
/// a row from an LLM query, a wrong `true` corrupts every per-request token
/// and cost average built on these counters.
fn is_llm_endpoint(endpoint: &str) -> bool {
    LLM_ENDPOINTS.contains(&endpoint)
}

/// The one request-metric emit, shared by every handler.
///
/// Called on the handler's way out — see the module docs for why `elapsed`
/// is NOT the end-to-end figure on a streamed response.
///
/// `endpoint` must be a bounded route template — a literal for the fixed
/// routes, or [`crate::normalize_endpoint_label`] output for the `:param` /
/// wildcard ones. Never a raw request path (#451).
pub(crate) fn record(
    state: &ProxyState,
    endpoint: &'static str,
    caller: Caller<'_>,
    upstream: Upstream<'_>,
    status: u16,
    elapsed: Duration,
) {
    let outcome = RequestOutcome::from_status(status);
    // Emit-chokepoint label bounding (#451 class): success paths hand in
    // the caller's requested string, which for a wildcard-served alias
    // is caller-minted — collapse it to the configured row's name here
    // so no handler-family member can mint unbounded series.
    let snap = state.snapshot.load();
    let (model_label, upstream_label) =
        crate::usage_attr::metric_model_label_pair(&snap, upstream.model, upstream.upstream_model);
    let upstream = Upstream {
        model: model_label.as_ref(),
        upstream_model: upstream_label.as_ref(),
        ..upstream
    };
    state
        .metrics
        .record_request(upstream.provider, upstream.model, status, outcome, elapsed);
    let labels = RequestLabels {
        endpoint,
        // Derived from the endpoint rather than passed in, so the detailed
        // families can't disagree with `aisix_proxy_in_flight_requests`
        // about which protocol a route speaks.
        inbound_protocol: crate::inbound_protocol_for_endpoint(endpoint),
        provider: upstream.provider,
        model: upstream.model,
        upstream_model: upstream.upstream_model,
        provider_key_id: upstream.pk.id(),
        provider_key_name: upstream.pk.name(),
        api_key_id: caller.api_key_id,
        team_id: caller.team_id,
        user_id: caller.user_id,
        user_name: caller.user_name,
        stream: upstream.stream,
        is_fallback: upstream.is_fallback,
        status,
        outcome,
    };
    if is_llm_endpoint(endpoint) {
        state.metrics.record_proxy_and_llm_request(labels, elapsed);
    } else {
        state.metrics.record_proxy_request(labels, elapsed);
    }
}

/// What one request consumed. Every counter below no-ops on an all-zero
/// value, so the zero-token paths — a failed attempt, a 501, `/v1/files` —
/// cost nothing and create no series.
#[derive(Clone, Copy, Default)]
pub(crate) struct Tokens<'a> {
    pub input: u32,
    pub output: u32,
    /// The canonical, CACHE-INCLUSIVE total. Use
    /// `usage_attr::total_tokens_with_cache` wherever cache counters exist
    /// (#740/#1002) — a bare input+output silently undercounts cached
    /// traffic, and this value is what the by-client series reports as
    /// `token_type="total"`.
    pub total: u32,
    pub spend_usd: f64,
    /// Normalised inbound client for the by-client series
    /// (`state.client_classifier.classify(&client.user_agent)`).
    pub client_type: &'a str,
}

/// Terminal token/spend emit, shared by every handler — the companion to
/// [`record`].
///
/// Three families ride along, and each had a DIFFERENT endpoint coverage
/// before #1234's follow-up: the `aisix_llm_*_tokens_total` and
/// `aisix_llm_spend_micro_usd_total` families were chat and messages only,
/// `aisix_llm_tokens_by_client_total` was chat, messages and responses, and
/// the legacy `aisix_tokens_consumed_total` was chat ALONE. A gateway that
/// billed a customer for `/v1/embeddings` reported none of those tokens.
///
/// Labels come from the same [`Caller`] / [`Upstream`] pair [`record`] uses,
/// so a query joining requests to tokens lines up by construction, and this
/// adds no series dimension the request families don't already carry.
pub(crate) fn record_usage(
    state: &ProxyState,
    endpoint: &'static str,
    caller: Caller<'_>,
    upstream: Upstream<'_>,
    tokens: Tokens<'_>,
) {
    // Same emit-chokepoint bounding as `record` — see the note there.
    let snap = state.snapshot.load();
    let (model_label, upstream_label) =
        crate::usage_attr::metric_model_label_pair(&snap, upstream.model, upstream.upstream_model);
    let upstream = Upstream {
        model: model_label.as_ref(),
        upstream_model: upstream_label.as_ref(),
        ..upstream
    };
    // Legacy compatibility series (provider × model).
    state
        .metrics
        .record_tokens(upstream.provider, upstream.model, u64::from(tokens.total));
    state.metrics.record_llm_usage(
        UsageLabels {
            endpoint,
            inbound_protocol: crate::inbound_protocol_for_endpoint(endpoint),
            provider: upstream.provider,
            model: upstream.model,
            upstream_model: upstream.upstream_model,
            provider_key_id: upstream.pk.id(),
            provider_key_name: upstream.pk.name(),
            api_key_id: caller.api_key_id,
            team_id: caller.team_id,
            user_id: caller.user_id,
            user_name: caller.user_name,
        },
        LlmUsage {
            input_tokens: tokens.input,
            output_tokens: tokens.output,
            total_tokens: tokens.total,
            spend_usd: tokens.spend_usd,
        },
    );
    // Deliberately NOT keyed on the labels above: this family is
    // client_type × model × token_type only, so the per-key dimensions
    // never multiply it (#890 req-4).
    state.metrics.record_llm_tokens_by_client(
        tokens.client_type,
        upstream.model,
        u64::from(tokens.input),
        u64::from(tokens.output),
        u64::from(tokens.total),
    );
}

/// Publish the caller's current rate-limit window: inject the
/// `x-ratelimit-*` response headers the OpenAI and Anthropic SDKs read to
/// schedule back-off, and record the remaining-quota gauges.
///
/// One function for both so an endpoint cannot ship the header without the
/// metric, or the metric with an unbounded label. `model` is collapsed
/// through [`crate::usage_attr::metric_model_label_pair`] here rather than at
/// the call site: a wildcard row serves arbitrary caller-minted names, every
/// distinct `(api_key_id, model)` pair registers a new Prometheus series, and
/// the recorder sets no idle timeout — so a raw request string is
/// attacker-controlled cardinality that is never reclaimed (#451).
///
/// No-op when the key has no limit configured: `peek` returns `None` and the
/// caller should not read an absent header as "unlimited".
#[allow(clippy::too_many_arguments)]
pub(crate) async fn publish_rate_limit_window(
    metrics: &aisix_obs::Metrics,
    limiter: &aisix_ratelimit::Limiter,
    snapshot: &aisix_core::AisixSnapshot,
    api_key_id: &str,
    limits: &aisix_core::RateLimit,
    requested_model: &str,
    upstream_model: &str,
    response: &mut axum::response::Response,
) {
    let Some(status) = limiter.peek(api_key_id, limits).await else {
        return;
    };
    crate::render::inject_ratelimit_headers(response, &status);
    let (model, _) =
        crate::usage_attr::metric_model_label_pair(snapshot, requested_model, upstream_model);
    metrics.set_rate_limit_remaining(
        api_key_id,
        model.as_ref(),
        status.rpm_remaining(),
        status.tpm_remaining(),
    );
}

#[cfg(test)]
mod tests {
    /// H-01 / L-11: the `x-ratelimit-*` headers and the remaining-quota
    /// gauges ship from one call so an endpoint cannot emit one without the
    /// other — and the gauge's `model` label is collapsed to the resolved
    /// row. A wildcard row serves arbitrary caller-minted names, and nothing
    /// evicts a Prometheus series, so labelling with the raw request string
    /// is unbounded cardinality (#451).
    #[tokio::test]
    async fn publish_rate_limit_window_bounds_the_model_label_and_sets_headers() {
        use aisix_core::resource::ResourceEntry;
        use aisix_core::snapshot::ResourceTable;

        let table = ResourceTable::default();
        let wildcard: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "openai/*",
            "provider": "openai",
            "model_name": "*",
            "provider_key_id": "pk-1",
        }))
        .unwrap();
        table.insert(ResourceEntry::new("m-star", wildcard, 1));
        let snapshot = aisix_core::AisixSnapshot {
            models: table,
            ..Default::default()
        };

        let metrics = aisix_obs::Metrics::new(false);
        let limiter = aisix_ratelimit::Limiter::new();
        let limits = aisix_core::RateLimit {
            rpm: Some(100),
            tpm: Some(10_000),
            ..Default::default()
        };

        // Two different caller-minted names the same wildcard row serves.
        for requested in ["openai/gpt-4o", "openai/anything-else"] {
            // Production peeks after the commit, so the bucket exists.
            let _reservation = limiter.pre_commit("key-1", &limits).await.unwrap();
            let mut response = axum::response::Response::new(axum::body::Body::empty());
            publish_rate_limit_window(
                &metrics,
                &limiter,
                &snapshot,
                "key-1",
                &limits,
                requested,
                "gpt-4o",
                &mut response,
            )
            .await;
            assert!(
                response
                    .headers()
                    .contains_key("x-ratelimit-limit-requests"),
                "the window must reach the client as headers too"
            );
        }

        let scrape = metrics.render();
        let series: Vec<&str> = scrape
            .lines()
            .filter(|line| line.starts_with("aisix_ratelimit_remaining_requests{"))
            .collect();
        assert_eq!(
            series.len(),
            1,
            "two caller-minted names served by one wildcard row must collapse \
             to a single series, got: {series:?}"
        );
        assert!(
            series[0].contains("model=\"openai/*\""),
            "label must be the resolved row, got: {}",
            series[0]
        );
        assert!(
            !scrape.contains("openai/gpt-4o"),
            "the raw caller-supplied model must never reach a metric label"
        );
    }

    use super::*;

    /// Every registered proxy route, as its raw request path. Adding a route
    /// to `build_router` without adding it here leaves the tests below
    /// unable to see it — which is the point: the two assertions that follow
    /// are what force a new endpoint's `endpoint` label and LLM-vs-proxy
    /// tier to be decided rather than defaulted.
    const ROUTES: &[&str] = &[
        "/v1/models",
        "/v1/models/gpt-4o",
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/embeddings",
        "/v1/images/generations",
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/v1/rerank",
        "/v1/responses",
        "/v1/audio/transcriptions",
        "/v1/audio/translations",
        "/v1/audio/speech",
        "/v1/videos",
        "/v1/videos/vid_abc123",
        "/v1/videos/vid_abc123/content",
        "/v1/realtime",
        "/v1/files",
        "/v1/files/file_abc123",
        "/v1/files/file_abc123/content",
        "/v1/batches",
        "/v1/batches/batch_abc123",
        "/v1/batches/batch_abc123/cancel",
        "/v1/fine_tuning/jobs",
        "/v1/fine_tuning/jobs/ft_abc123",
        "/v1/fine_tuning/jobs/ft_abc123/cancel",
        "/mcp",
        // 带尾斜杠的那条是单独注册的，归一化里也有专门的臂 —— 两者都要有样本，
        // 否则那个臂没有任何断言看着。
        "/mcp/",
        "/mcp/some-server",
        "/a2a/some-agent",
        "/a2a/some-agent/.well-known/agent-card.json",
        "/passthrough/openai/v1/anything",
    ];

    /// Routes deliberately outside the per-endpoint request families, with
    /// the reason. Everything else must have a sample in [`ROUTES`].
    ///
    /// The liveness and readiness probes are scraped continuously by the
    /// orchestrator, carry no caller identity, and answer from process state
    /// without reaching an upstream — a per-endpoint series for them would
    /// be pure scrape noise sharing labels with real traffic.
    const NOT_METERED: &[&str] = &["/livez", "/readyz"];

    /// Every protocol `inbound_protocol_for_endpoint` can return must survive
    /// the usage sink's label bounding unchanged.
    ///
    /// Two closed sets, in two crates, describing the same thing: the request
    /// and LLM families label a request with the first, and the usage-event
    /// families with the second. When they disagree the same request carries
    /// two different protocols and an operator cannot join the families on
    /// it — which is what happened to `a2a`, `realtime`, `passthrough` and
    /// `batch`, folded into `other` on one side while the other named them.
    #[test]
    fn inbound_protocol_label_set_covers_every_endpoint() {
        for route in ROUTES {
            let endpoint = crate::normalize_endpoint_label(route);
            let protocol = crate::inbound_protocol_for_endpoint(endpoint);
            assert_eq!(
                aisix_obs::bounded_inbound_protocol(protocol),
                protocol,
                "{endpoint} 的协议标签 {protocol:?} 会被用量事件那侧折成 \
                 {:?} —— 同一次请求在两个指标族里协议不同，join 不起来",
                aisix_obs::bounded_inbound_protocol(protocol),
            );
        }
    }

    /// Every `inbound_protocol` literal a handler writes onto a `UsageEvent`
    /// must survive the sink's label bounding unchanged.
    ///
    /// The sibling test above covers the values derived from the endpoint
    /// label. These are the ones handlers set directly on the event —
    /// `"batch"`, `"passthrough"`, `"realtime"` — and they are what the
    /// usage-event families label with. A handler adding a new one lands in
    /// `other` silently: the events still flow, they just stop being
    /// distinguishable, which looks exactly like that protocol having no
    /// traffic.
    ///
    /// Read off the tree rather than listed here, so a new handler is
    /// covered by existing.
    #[test]
    fn every_written_inbound_protocol_is_a_bounded_label() {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|x| x == "rs") {
                    let Ok(src) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    // 只看生产段：测试里会故意写越界的值来钉住收口本身。
                    let production = src.split_once("#[cfg(test)]").map_or(&src[..], |(a, _)| a);
                    for chunk in production.split("inbound_protocol: \"").skip(1) {
                        if let Some(end) = chunk.find('"') {
                            out.push((
                                path.file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into(),
                                chunk[..end].to_string(),
                            ));
                        }
                    }
                }
            }
        }

        let mut found = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut found,
        );
        assert!(
            found.len() >= 5,
            "只扫到 {} 个 inbound_protocol 字面量，抽取多半坏了",
            found.len()
        );
        for (file, protocol) in found {
            assert_eq!(
                aisix_obs::bounded_inbound_protocol(&protocol),
                protocol,
                "{file} 写的协议 {protocol:?} 会被用量事件那侧折成 {:?} —— \
                 它的用量事件从此和「没有流量」看起来一样",
                aisix_obs::bounded_inbound_protocol(&protocol),
            );
        }
    }

    /// Every route `build_router` registers must have a sample in [`ROUTES`]
    /// or an entry in [`NOT_METERED`].
    ///
    /// [`ROUTES`] is what makes the two assertions below able to see a route
    /// at all, and it was kept in step by eye: a route added to
    /// `build_router` and not here produced no failure, it just stopped
    /// being checked. That is the same invisibility that shipped `/v1/videos`
    /// reporting `endpoint="other"` (#1234) — the label was never decided,
    /// and nothing said so.
    ///
    /// The route table is read out of this crate's own source rather than
    /// duplicated: a third list would be a third thing to align by hand. The
    /// scan stops at the test module so routes declared by tests below do not
    /// count as shipped ones.
    #[test]
    fn every_registered_route_is_covered_by_a_sample() {
        const LIB_SRC: &str = include_str!("lib.rs");
        let production = LIB_SRC
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(LIB_SRC);

        // `.route("<pattern>"` — the literal may sit on the next line.
        let mut patterns: Vec<&str> = Vec::new();
        for chunk in production.split(".route(").skip(1) {
            let Some(open) = chunk.find('"') else {
                continue;
            };
            // 只接受紧跟其后的字面量：`.route(` 与它之间只能有空白，否则那是
            // 别的东西（比如注释里提到的 `.route(`）。
            if !chunk[..open].chars().all(char::is_whitespace) {
                continue;
            }
            let rest = &chunk[open + 1..];
            let Some(close) = rest.find('"') else {
                continue;
            };
            patterns.push(&rest[..close]);
        }
        patterns.sort_unstable();
        patterns.dedup();
        assert!(
            patterns.len() > 20,
            "只从源码里认出 {} 条路由，抽取多半坏了（真实数量是三十几条）",
            patterns.len()
        );

        /// 一个样本路径是否匹配一条路由模式。`:param` 段匹配任意一段。
        fn matches(pattern: &str, sample: &str) -> bool {
            let p: Vec<&str> = pattern.split('/').collect();
            let s: Vec<&str> = sample.split('/').collect();
            p.len() == s.len()
                && p.iter()
                    .zip(&s)
                    .all(|(pseg, sseg)| pseg.starts_with(':') || pseg == sseg)
        }

        for pattern in &patterns {
            if NOT_METERED.contains(pattern) {
                continue;
            }
            assert!(
                ROUTES.iter().any(|sample| matches(pattern, sample)),
                "路由 {pattern} 在 ROUTES 里没有样本，于是下面两条断言看不见它 —— \
                 它的 endpoint 标签和 LLM/proxy 分层都还没被决定过。加一个样本，\
                 或者连同理由加进 NOT_METERED。"
            );
        }
    }

    /// No proxy route may fall through to the `"other"` bucket. A route that
    /// does is invisible per-endpoint in every request series — which is how
    /// `/v1/videos` shipped (#1234): it was registered in
    /// `build_router` but missing from the normalizer's allowlist, so all
    /// video traffic reported `endpoint="other"`.
    #[test]
    fn every_route_has_its_own_endpoint_label() {
        for route in ROUTES {
            assert_ne!(
                crate::normalize_endpoint_label(route),
                "other",
                "route {route} is missing from normalize_endpoint_label"
            );
        }
    }

    /// Guards against a typo in [`LLM_ENDPOINTS`]. An entry that no route
    /// normalizes to can never match, and the failure is silent: the
    /// endpoint just stops appearing in `aisix_llm_requests_total`, which is
    /// indistinguishable from having no traffic.
    #[test]
    fn llm_endpoints_are_reachable() {
        let reachable: Vec<&str> = ROUTES
            .iter()
            .map(|r| crate::normalize_endpoint_label(r))
            .collect();
        for endpoint in LLM_ENDPOINTS {
            assert!(
                reachable.contains(endpoint),
                "no route normalizes to {endpoint} — dead entry in LLM_ENDPOINTS"
            );
        }
    }

    /// The tier split itself: the inference routes carry the LLM series, the
    /// tool / management / tunnel surfaces carry only the proxy series.
    #[test]
    fn tiers_split_inference_from_the_rest() {
        for route in [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages/count_tokens",
            "/v1/embeddings",
            "/v1/audio/speech",
            "/v1/videos/vid_abc123/content",
            // Moved in once realtime started reporting its tokens + cost.
            "/v1/realtime",
        ] {
            assert!(
                is_llm_endpoint(crate::normalize_endpoint_label(route)),
                "{route} should count as an LLM request"
            );
        }
        for route in [
            "/mcp/some-server",
            "/a2a/some-agent",
            "/v1/batches/batch_abc123",
            "/passthrough/openai/v1/anything",
            "/livez",
        ] {
            assert!(
                !is_llm_endpoint(crate::normalize_endpoint_label(route)),
                "{route} must not count as an LLM request"
            );
        }
    }
}
