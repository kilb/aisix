//! `Model` entity — the routing target users reference from API requests.
//!
//! A Model has a user-chosen unique `display_name`, an open vendor
//! string `provider` (e.g. `"openai"`, `"xai"`), an upstream
//! `model_name` (e.g. `"gpt-4o"`), and a `provider_key_id` referencing
//! a [`ProviderKey`] entry that supplies the secret + optional
//! `api_base` override.
//!
//! Routing models — virtual routers that pick a target Model per request
//! — set `routing` instead of `provider`/`model_name`/`provider_key_id`.
//! See [`Model::is_routing`].
//!
//! etcd path: `{prefix}/models/{uuid}`. Secondary index on `display_name`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::embedding::EmbeddingConfig;
use super::ensemble::EnsembleConfig;
use super::rate_limit::RateLimit;
use super::routing::Routing;
use super::semantic::Semantic;
use crate::resource::Resource;

// `Provider` enum removed as part of #302 Phase A clean cut. Vendor
// identity is an open string on `ProviderKey.provider` /
// `Model.provider` — DP no longer enumerates vendors at compile time.
// Code paths that need vendor-aware dispatch (rerank, messages
// cross-provider routing) compare the string directly.

/// Upstream API protocol family used for provider dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Adapter {
    Openai,
    Anthropic,
    Bedrock,
    Vertex,
    AzureOpenai,
}

/// Per-token cost for budget tracking. Every value is in USD per 1,000 tokens.
///
/// Prompt-cached tokens are priced separately because providers charge a
/// different rate for them, in both directions: a cache read is cheaper than
/// fresh input, while writing a prompt into the cache costs more. Priced with
/// `input_per_1k` alone, cached traffic is mis-billed either way.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct ModelCost {
    /// Prompt token cost in USD per 1,000 tokens.
    #[schemars(range(min = 0.0))]
    pub input_per_1k: f64,
    /// Completion token cost in USD per 1,000 tokens.
    #[schemars(range(min = 0.0))]
    pub output_per_1k: f64,
    /// Cost in USD per 1,000 prompt tokens served from the provider's prompt
    /// cache. Omitted, cache reads are charged at `input_per_1k`.
    ///
    /// An absolute price rather than a discount factor, because the discount
    /// is not the same across providers: one charges a tenth of the input
    /// rate for a cache read, another about half.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub cached_input_per_1k: Option<f64>,
    /// Cost in USD per 1,000 prompt tokens written INTO the provider's cache.
    /// Omitted, cache writes are charged at `input_per_1k`.
    ///
    /// Writes are the direction that costs more than plain input — one
    /// provider prices a five-minute write at 1.25x its input rate and a
    /// one-hour write at 2x. A deployment that caches heavily and prices
    /// writes at the plain input rate under-reports its spend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub cache_write_per_1k: Option<f64>,
}

/// The three input-token buckets pricing needs, guaranteed DISJOINT.
///
/// This type exists because the two upstream shapes report cached tokens
/// incompatibly, and the difference is invisible at the call site:
///
/// - Anthropic reports `cache_creation_input_tokens` and
///   `cache_read_input_tokens` as counters SEPARATE from `input_tokens`
///   (<https://platform.claude.com/docs/en/docs/build-with-claude/prompt-caching>:
///   "input_tokens: Number of input tokens which were not read from or used
///   to create a cache").
/// - OpenAI reports `prompt_tokens_details.cached_tokens` as tokens "present
///   in the prompt" — a SUBSET of `prompt_tokens`
///   (`openai-python: src/openai/types/completion_usage.py`).
///
/// Subtracting on the first shape under-counts input; not subtracting on the
/// second double-charges the cached half. Neither mistake fails a request or
/// shows up in a test that only checks a total, so the shape is named at the
/// constructor instead of being re-derived per handler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputTokens {
    /// Fresh prompt tokens, neither read from nor written to the cache.
    pub uncached: u64,
    /// Prompt tokens served from the cache.
    pub cache_read: u64,
    /// Prompt tokens written into the cache.
    pub cache_write: u64,
}

impl InputTokens {
    /// Anthropic-shaped usage: the three counters are already disjoint, so
    /// they are taken as given.
    pub fn from_disjoint_counters(input: u64, cache_read: u64, cache_write: u64) -> Self {
        Self {
            uncached: input,
            cache_read,
            cache_write,
        }
    }

    /// OpenAI-shaped usage: `cached` and `cache_write` are subsets of
    /// `prompt`, so the fresh portion is what remains after both are removed.
    /// Saturating, because a provider that reports a cached count exceeding
    /// its own prompt total must not wrap into a huge charge.
    pub fn from_prompt_superset(prompt: u64, cached: u64, cache_write: u64) -> Self {
        Self {
            uncached: prompt.saturating_sub(cached).saturating_sub(cache_write),
            cache_read: cached,
            cache_write,
        }
    }

    /// No cache involved: every prompt token is fresh.
    pub fn uncached_only(prompt: u64) -> Self {
        Self {
            uncached: prompt,
            ..Self::default()
        }
    }

    /// Total prompt tokens across all three buckets.
    pub fn total(&self) -> u64 {
        self.uncached + self.cache_read + self.cache_write
    }
}

impl ModelCost {
    /// Calculate USD cost, charging every input bucket at its own rate.
    pub fn calculate_with_cache(&self, input: InputTokens, output_tokens: u64) -> f64 {
        // Absent cache rates fall back to the plain input rate, which is what
        // this priced before the fields existed — an existing `cost` block
        // keeps reporting exactly what it reported yesterday.
        let cached_rate = self.cached_input_per_1k.unwrap_or(self.input_per_1k);
        let write_rate = self.cache_write_per_1k.unwrap_or(self.input_per_1k);
        (self.input_per_1k * (input.uncached as f64)
            + cached_rate * (input.cache_read as f64)
            + write_rate * (input.cache_write as f64)
            + self.output_per_1k * (output_tokens as f64))
            / 1000.0
    }

    /// Calculate USD cost treating every prompt token as fresh input.
    ///
    /// Kept for the surfaces with no prompt cache at all (embeddings, audio,
    /// rerank, images). A text-completion path should use
    /// [`Self::calculate_with_cache`] so cached traffic is priced correctly.
    pub fn calculate(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        self.calculate_with_cache(InputTokens::uncached_only(input_tokens), output_tokens)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct BackgroundModelCheck {
    /// Whether background health checks are enabled for this model.
    pub enabled: bool,
    /// Seconds between background health checks. Minimum: 5.
    #[schemars(range(min = 5))]
    pub interval_seconds: u64,
    /// Request timeout in seconds for each background health check. Minimum: 1.
    #[schemars(range(min = 1))]
    pub timeout_seconds: u64,
    /// Prompt sent to the model during each background health check.
    #[schemars(length(min = 1))]
    pub prompt: String,
    /// Maximum completion tokens requested during each background health check.
    #[schemars(range(min = 1))]
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(inner(range(min = 100, max = 599)))]
    /// Upstream status codes to ignore when evaluating background check failures.
    pub ignore_statuses: Vec<u16>,
    /// Seconds after which the last completed background check is considered stale.
    #[schemars(range(min = 1))]
    pub stale_after_seconds: u64,
}

/// Request-path cooldown settings for a direct model after retryable upstream failures.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Default)]
pub struct CooldownConfig {
    /// Whether cooldown is active for this model. Set to `false` to keep the model in rotation regardless of upstream failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Cooldown TTL in seconds when the upstream did not supply a `Retry-After` header or `honor_retry_after` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_seconds: Option<u64>,
    /// Upper bound on cooldown TTL when `Retry-After` is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_seconds: Option<u64>,
    /// Whether to use the upstream's `Retry-After` header as the cooldown TTL when it contains seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub honor_retry_after: Option<bool>,
    /// Status codes that trigger cooldown, covering authentication failures, rate limits, and transient server errors. Caller-side validation errors such as `400`, `403`, and `422` are excluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(inner(range(min = 100, max = 599)))]
    pub trigger_statuses: Option<Vec<u16>>,
    /// Whether request-path timeouts trigger cooldown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_on_timeout: Option<bool>,
    /// Whether transport, decode, or stream-abort errors trigger cooldown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_on_transport: Option<bool>,
}

/// Default cooldown trigger statuses applied when the operator does
/// not override `trigger_statuses` on a direct model.
pub const DEFAULT_COOLDOWN_TRIGGER_STATUSES: &[u16] = &[401, 408, 429, 500, 502, 503, 504];

const DEFAULT_COOLDOWN_SECONDS: u64 = 30;
const DEFAULT_COOLDOWN_MAX_SECONDS: u64 = 600;

impl CooldownConfig {
    pub fn enabled_or_default(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn default_seconds_or_default(&self) -> u64 {
        self.default_seconds.unwrap_or(DEFAULT_COOLDOWN_SECONDS)
    }

    pub fn max_seconds_or_default(&self) -> u64 {
        self.max_seconds.unwrap_or(DEFAULT_COOLDOWN_MAX_SECONDS)
    }

    pub fn honor_retry_after_or_default(&self) -> bool {
        self.honor_retry_after.unwrap_or(true)
    }

    /// Effective trigger-status list — operator override OR built-in
    /// default. Returned as `Cow` so callers can avoid copies on the
    /// default path.
    pub fn effective_trigger_statuses(&self) -> std::borrow::Cow<'_, [u16]> {
        match &self.trigger_statuses {
            Some(list) => std::borrow::Cow::Borrowed(list.as_slice()),
            None => std::borrow::Cow::Borrowed(DEFAULT_COOLDOWN_TRIGGER_STATUSES),
        }
    }

    pub fn trigger_on_timeout_or_default(&self) -> bool {
        self.trigger_on_timeout.unwrap_or(true)
    }

    pub fn trigger_on_transport_or_default(&self) -> bool {
        self.trigger_on_transport.unwrap_or(true)
    }
}

/// Cache lifetime for gateway-injected prompt-cache breakpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    /// The value emitted on the upstream `cache_control.ttl` field.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            CacheTtl::FiveMinutes => "5m",
            CacheTtl::OneHour => "1h",
        }
    }
}

/// Automatic prompt-cache breakpoint injection for a direct Anthropic
/// model. When enabled, the gateway adds cache-control markers to
/// requests that carry none of their own, so callers get provider-side
/// prompt-cache discounts without changing their requests. Requests that
/// already set their own cache-control markers are forwarded unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct AutoPromptCaching {
    /// Whether automatic prompt-cache injection is active for this model.
    pub enabled: bool,
    /// Cache lifetime for injected breakpoints: `5m` (default when omitted) or `1h`. A `1h` cache write costs 2x the base input rate versus 1.25x for `5m`, so it pays off only when the cached prefix is reused across a longer session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTtl>,
}

impl AutoPromptCaching {
    /// Effective TTL — operator override or the built-in 5-minute default.
    pub fn ttl_or_default(&self) -> CacheTtl {
        self.ttl.unwrap_or(CacheTtl::FiveMinutes)
    }
}

/// Lazily-parsed form of a resource's `allowed_cidrs`, so the per-request IP gate
/// does not re-parse the operator's strings on every check — `routing.rs`
/// calls it once per target, so a group multiplies the cost.
///
/// Cloning deliberately produces an EMPTY cache rather than sharing the
/// parsed vector. A `Model` is cloned exactly where it may then be edited
/// (the admin update path, tests), and a parse cache that outlived an edit
/// would let a security gate answer from the previous allowlist. Rebuilding
/// costs one parse on the copy's first check; being wrong costs an incorrect
/// allow or deny.
#[derive(Debug, Default)]
pub struct ParsedCidrCache(std::sync::OnceLock<Vec<ipnet::IpNet>>);

impl Clone for ParsedCidrCache {
    fn clone(&self) -> Self {
        Self(std::sync::OnceLock::new())
    }
}

/// The cache is derived state, so two resources are equal when their
/// `allowed_cidrs` are — whether either has parsed them yet says nothing
/// about the configuration. Without this, a resource carrying the cache
/// could not derive `PartialEq`, and the equality checks that decide whether
/// a watch event actually changed anything would have to be hand-written.
impl PartialEq for ParsedCidrCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for ParsedCidrCache {}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Model {
    /// Operator-facing unique label. Surfaces on `/v1/models`,
    /// `req.model` on chat completions, `ApiKey.allowed_models`, and
    /// the dashboard model list. `Resource::name()` returns this.
    #[schemars(length(min = 1))]
    pub display_name: String,

    /// Upstream vendor identity used for dispatch, compatibility checks, telemetry, and access logs. Routing and ensemble models leave this field unset.
    //
    // `provider` is the open vendor identity (models.dev catalog id —
    // e.g. `openai`, `xai`, `wafer.ai`). The pattern accepts the dot
    // character because at least one real models.dev id (`wafer.ai`)
    // contains it; rejecting `.` would re-create the #417 bug class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = "^[a-z0-9][a-z0-9._-]*$"), length(min = 1, max = 64))]
    pub provider: Option<String>,

    /// Upstream model identifier sent in provider requests. Routing and ensemble models leave this field unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub model_name: Option<String>,

    /// Provider key resource ID used to authenticate upstream requests. Routing and ensemble models leave this field unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub provider_key_id: Option<String>,

    /// End-to-end timeout in milliseconds for non-streaming upstream calls. Absent falls back to the group's `timeout`, then to the deployment-wide `upstream.timeout_ms` default. `0` disables the non-streaming timeout for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,

    /// Maximum gap in milliseconds between upstream streaming chunks. `0` or absent falls back to the group's `stream_timeout`, then to the model's (or group's) `timeout`, then to the deployment-wide `upstream.stream_timeout_ms` / `timeout_ms` defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_timeout: Option<u64>,

    /// Retry attempts against this model after a retryable upstream failure, before the request gives up (or, inside a model group, fails over to the next target). Absent falls back to the group's `routing.retries`, then to the deployment-wide `upstream.retries` default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,

    /// Request, token, and concurrency limits for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,

    /// Client IP allowlist in CIDR notation. Empty or absent allows all clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(inner(length(min = 1)))]
    pub allowed_cidrs: Option<Vec<String>>,

    /// Parse cache for [`Model::allowed_cidrs`]. Never serialized and never
    /// part of the schema — derived state, rebuilt on demand.
    #[serde(skip)]
    #[schemars(skip)]
    pub allowed_cidrs_parsed: ParsedCidrCache,

    /// Virtual routing configuration. When set, the gateway selects a target
    /// from `routing.targets` and uses that target model's `provider`,
    /// `model_name`, and `provider_key_id` fields for upstream dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,

    /// Ensemble configuration for panel calls and judge synthesis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ensemble: Option<EnsembleConfig>,

    /// Semantic-routing configuration. When set, the gateway embeds the
    /// request and dispatches to the route whose examples it matches best,
    /// using that route's target Model for upstream dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<Semantic>,

    /// Embedding-modality metadata. Present on direct Models that serve an
    /// OpenAI-compatible `/v1/embeddings` endpoint (and can be referenced
    /// by a semantic router's `embedding_model`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingConfig>,

    /// Per-token cost for budget tracking. Omit it when cost tracking is not needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,

    /// Direct-model-only background health-check configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_model_check: Option<BackgroundModelCheck>,

    /// Direct-model-only request-path cooldown configuration. Omit this field to use the built-in cooldown behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<CooldownConfig>,

    /// Automatic prompt-cache breakpoint injection for direct Anthropic models. Omit to leave injection off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_prompt_caching: Option<AutoPromptCaching>,

    /// Non-schema runtime id. Not part of the JSON payload — filled in by
    /// the snapshot loader from the etcd key path. Kept here so `Resource`
    /// can return a `&str` id.
    #[serde(skip)]
    pub(crate) runtime_id: String,
}

impl Model {
    /// Whether this Model is a virtual router (proxy walks `routing.targets`
    /// instead of dispatching its own upstream config).
    pub fn is_routing(&self) -> bool {
        self.routing.is_some()
    }

    /// Whether this Model is an ensemble (fans out to a panel + judge
    /// instead of dispatching a single upstream).
    pub fn is_ensemble(&self) -> bool {
        self.ensemble.is_some()
    }

    /// Whether this Model is a semantic router (picks a target by the
    /// meaning of the request instead of dispatching its own upstream).
    pub fn is_semantic(&self) -> bool {
        self.semantic.is_some()
    }

    /// Whether this Model is an embedding-modality model (a direct model
    /// that also carries embedding metadata).
    pub fn is_embedding(&self) -> bool {
        self.embedding.is_some()
    }

    /// Convenience: borrow the upstream model id if this Model is a
    /// direct (non-routing) entry.
    pub fn upstream_model(&self) -> Option<&str> {
        self.model_name.as_deref()
    }

    /// Strip the per-kind DEAD knobs from a loaded document, returning
    /// the stripped field names for the loader's partially-compatible
    /// report. The strict write path rejects these shapes outright
    /// ([`model_one_of_strict`]); rows stored before that keep loading —
    /// minus the field, so no code path can half-honor a knob the shape
    /// never resolved.
    pub fn strip_kind_inapplicable(&mut self) -> Vec<&'static str> {
        let mut stripped = Vec::new();
        if !(self.is_routing() || self.is_ensemble() || self.is_semantic()) {
            return stripped;
        }
        if self.auto_prompt_caching.take().is_some() {
            stripped.push("auto_prompt_caching");
        }
        if self.cost.take().is_some() {
            stripped.push("cost");
        }
        if (self.is_routing() || self.is_ensemble()) && self.retries.take().is_some() {
            stripped.push("retries");
        }
        // Pushed in lexicographic order so a pure-strip row's field list
        // is already sorted (the loader's merge path re-sorts a combined
        // unknown+inapplicable list, but a strip-only row bypasses that).
        if self.is_ensemble() {
            if self.stream_timeout.take().is_some() {
                stripped.push("stream_timeout");
            }
            if self.timeout.take().is_some() {
                stripped.push("timeout");
            }
        }
        stripped
    }

    /// This resource's own non-streaming deadline, as one level of the
    /// model → group → `upstream.timeout_ms` resolution performed by the
    /// proxy's `effective_timeouts`. Tri-state: `None` defers to the next
    /// level, `Some(None)` is an explicit `0` ("no deadline, stop
    /// resolving"), `Some(Some(d))` is a configured deadline.
    pub fn request_timeout_level(&self) -> Option<Option<std::time::Duration>> {
        self.timeout
            .map(|ms| (ms > 0).then(|| std::time::Duration::from_millis(ms)))
    }

    /// This resource's own streaming per-chunk deadline, as one level of
    /// the model → group → `upstream.stream_timeout_ms` → resolved
    /// `timeout` chain. Unlike [`Model::request_timeout_level`], `0` and
    /// absent both defer — `stream_timeout` has always used `0` as "fall
    /// back", not "disable".
    pub fn stream_read_timeout(&self) -> Option<std::time::Duration> {
        self.stream_timeout
            .filter(|&ms| ms > 0)
            .map(std::time::Duration::from_millis)
    }

    /// Whether a client at `source_ip` may access this model (#557).
    ///
    /// See [`cidr_allows`] for the rules — this is the model-scoped call of
    /// the one implementation every resource with an IP allowlist shares.
    pub fn ip_allowed(&self, source_ip: &str) -> bool {
        cidr_allows(
            self.allowed_cidrs.as_deref(),
            &self.allowed_cidrs_parsed,
            source_ip,
        )
    }
}

/// Whether a client at `source_ip` is inside a resource's CIDR allowlist.
///
/// THE implementation for every resource that carries one — Model, MCP
/// server, A2A agent. A security gate copied per resource drifts: the
/// IPv4-mapped-IPv6 canonicalisation below was a real bug fixed once, and a
/// second copy would still have it.
///
/// Returns `true` when no restriction is configured (the common case). When
/// one is set, returns `true` only if `source_ip` parses as an address
/// contained in at least one range. An empty or unparseable `source_ip`
/// against a configured restriction returns `false` — fail closed, so an
/// unattributable request can never slip past an allowlist.
pub fn cidr_allows(ranges: Option<&[String]>, cache: &ParsedCidrCache, source_ip: &str) -> bool {
    let ranges = match ranges {
        Some(r) if !r.is_empty() => r,
        _ => return true,
    };
    let ip: std::net::IpAddr = match source_ip.parse() {
        Ok(ip) => ip,
        Err(_) => return false,
    };
    // A dual-stack listener reports an IPv4 client as `::ffff:a.b.c.d`,
    // which no IPv4 CIDR contains — without this the same allowlist
    // rejects every IPv4 caller on `[::]` while working on `0.0.0.0`.
    // A no-op for genuine IPv6.
    let ip = ip.to_canonical();
    // An entry that does not parse is skipped, which narrows the allowlist
    // rather than widening it. The write-path validators reject that shape,
    // so this only tolerates rows stored by an older build.
    let nets = cache.0.get_or_init(|| {
        ranges
            .iter()
            .filter_map(|cidr| cidr.parse::<ipnet::IpNet>().ok())
            .collect()
    });
    nets.iter().any(|net| net.contains(&ip))
}

/// The one cross-field invariant the runtime schema enforces that
/// `schemars` cannot derive from the flat struct: a Model ships EXACTLY
/// one dispatch shape — a `routing` block, an `ensemble` block, a
/// `semantic` block, or the three direct upstream fields
/// (`provider`/`model_name`/`provider_key_id`) together. The `embedding`
/// block is modality metadata on the direct shape, so it is permitted only
/// alongside the direct triple, never on a virtual router.
/// [`crate::models::schema::model_root_schema`] injects this as a top-level
/// `oneOf` into the generated schema, so the published schema and the
/// runtime validator share this single definition.
pub fn model_one_of() -> Value {
    model_one_of_variant(false)
}

/// The write-path variant of [`model_one_of`]: additionally forbids the
/// per-kind DEAD knobs — fields the runtime never reads on that shape,
/// which the lenient read path keeps tolerating (loaded rows strip them
/// with a partially-compatible warning instead of dropping the row; see
/// [`Model::strip_kind_inapplicable`]). Kind policy (project decision):
/// generic call knobs (`timeout`/`stream_timeout`/`retries`) resolve
/// member → group → deployment default wherever a group slot exists;
/// model-specific knobs (`auto_prompt_caching`, `cost`) are direct-only.
pub fn model_one_of_strict() -> Value {
    model_one_of_variant(true)
}

fn model_one_of_variant(strict: bool) -> Value {
    let extend = |base: &mut Value, extra: &[&str]| {
        let list = base["not"]["anyOf"].as_array_mut().expect("anyOf array");
        for field in extra {
            list.push(json!({ "required": [field] }));
        }
    };
    let mut variants = model_one_of_base();
    if strict {
        let arr = variants.as_array_mut().expect("oneOf array");
        // routing: the group slot for timeouts is the top-level pair
        // (api7/aisix#844); retries' group slot is `routing.retries`, so a
        // top-level value is dead — as are the model-specific knobs.
        extend(&mut arr[0], &["retries", "auto_prompt_caching", "cost"]);
        // direct (arr[1]): every knob is live.
        // ensemble: sub-calls resolve member-level knobs only; the
        // parent-level deadline is `ensemble.timeout_ms`.
        extend(
            &mut arr[2],
            &[
                "timeout",
                "stream_timeout",
                "retries",
                "auto_prompt_caching",
                "cost",
            ],
        );
        // semantic: top-level timeout/stream_timeout/retries ARE the group
        // slots (no routing block to carry them); the model-specific knobs
        // stay direct-only.
        extend(&mut arr[3], &["auto_prompt_caching", "cost"]);
    }
    variants
}

fn model_one_of_base() -> Value {
    json!([
        {
            "required": ["routing"],
            "not": { "anyOf": [
                { "required": ["provider"] },
                { "required": ["model_name"] },
                { "required": ["provider_key_id"] },
                { "required": ["background_model_check"] },
                { "required": ["cooldown"] },
                { "required": ["ensemble"] },
                { "required": ["semantic"] },
                { "required": ["embedding"] }
            ]}
        },
        {
            "required": ["provider", "model_name", "provider_key_id"],
            "not": { "anyOf": [
                { "required": ["routing"] },
                { "required": ["ensemble"] },
                { "required": ["semantic"] }
            ]}
        },
        {
            "required": ["ensemble"],
            "not": { "anyOf": [
                { "required": ["provider"] },
                { "required": ["model_name"] },
                { "required": ["provider_key_id"] },
                { "required": ["routing"] },
                { "required": ["background_model_check"] },
                { "required": ["cooldown"] },
                { "required": ["semantic"] },
                { "required": ["embedding"] }
            ]}
        },
        {
            "required": ["semantic"],
            "not": { "anyOf": [
                { "required": ["provider"] },
                { "required": ["model_name"] },
                { "required": ["provider_key_id"] },
                { "required": ["routing"] },
                { "required": ["ensemble"] },
                { "required": ["background_model_check"] },
                { "required": ["cooldown"] },
                { "required": ["embedding"] }
            ]}
        }
    ])
}

impl Resource for Model {
    fn id(&self) -> &str {
        &self.runtime_id
    }

    fn name(&self) -> &str {
        &self.display_name
    }

    fn kind() -> &'static str {
        "models"
    }
}

#[cfg(test)]
mod tests {
    use super::{InputTokens, ModelCost};

    /// Rates chosen so each bucket is distinguishable in the total: mixing
    /// two of them up cannot produce the same number by coincidence.
    fn cost() -> ModelCost {
        ModelCost {
            input_per_1k: 1.0,
            output_per_1k: 10.0,
            cached_input_per_1k: Some(0.1),
            cache_write_per_1k: Some(1.25),
        }
    }

    /// An existing `cost` block with no cache rates must price exactly as it
    /// did before those fields existed — upgrading the gateway must not move
    /// anyone's reported spend.
    #[test]
    fn absent_cache_rates_fall_back_to_the_input_rate() {
        let c = ModelCost {
            input_per_1k: 2.0,
            output_per_1k: 4.0,
            cached_input_per_1k: None,
            cache_write_per_1k: None,
        };
        // 1000 fresh + 1000 read + 1000 write, all at the input rate.
        let with_cache =
            c.calculate_with_cache(InputTokens::from_disjoint_counters(1000, 1000, 1000), 0);
        assert!((with_cache - 6.0).abs() < 1e-9, "got {with_cache}");
        // And the plain entry point is unchanged.
        assert!((c.calculate(1000, 500) - (2.0 + 2.0)).abs() < 1e-9);
    }

    /// Anthropic reports cache tokens as counters SEPARATE from
    /// `input_tokens`, so all three buckets are additive. Pricing them as a
    /// subset would subtract tokens the provider is charging for.
    #[test]
    fn anthropic_shaped_counters_are_additive() {
        let got = cost()
            .calculate_with_cache(InputTokens::from_disjoint_counters(1000, 2000, 4000), 1000);
        // 1000*1.0 + 2000*0.1 + 4000*1.25 + 1000*10.0, per 1k.
        let want = (1000.0 * 1.0 + 2000.0 * 0.1 + 4000.0 * 1.25 + 1000.0 * 10.0) / 1000.0;
        assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
    }

    /// OpenAI reports `cached_tokens` as a SUBSET of `prompt_tokens`. Failing
    /// to subtract it charges the cached half twice — once at the fresh rate
    /// and once at the cached rate.
    #[test]
    fn openai_shaped_cached_tokens_are_not_double_charged() {
        let input = InputTokens::from_prompt_superset(1000, 800, 0);
        assert_eq!(input.uncached, 200);
        assert_eq!(input.cache_read, 800);
        // The buckets must still add up to the prompt the provider reported.
        assert_eq!(input.total(), 1000);
        let got = cost().calculate_with_cache(input, 0);
        let want = (200.0 * 1.0 + 800.0 * 0.1) / 1000.0;
        assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
        // Charging the whole prompt at the fresh rate — today's behaviour —
        // is strictly more expensive, which is the bug being fixed.
        assert!(got < cost().calculate(1000, 0));
    }

    /// A provider reporting a cached count larger than its own prompt total
    /// must not wrap the fresh bucket into an astronomically large charge.
    #[test]
    fn an_impossible_cached_count_cannot_wrap_into_a_huge_charge() {
        let input = InputTokens::from_prompt_superset(100, 999, 0);
        assert_eq!(input.uncached, 0);
        assert!(cost().calculate_with_cache(input, 0) < 1.0);
    }

    /// Reading from the cache is cheaper than fresh input; writing to it is
    /// dearer. A configuration that got the two rates the wrong way round is
    /// the operator's business, but the arithmetic must keep them distinct.
    #[test]
    fn read_and_write_rates_are_applied_to_their_own_buckets() {
        let read_only =
            cost().calculate_with_cache(InputTokens::from_disjoint_counters(0, 1000, 0), 0);
        let write_only =
            cost().calculate_with_cache(InputTokens::from_disjoint_counters(0, 0, 1000), 0);
        let fresh_only =
            cost().calculate_with_cache(InputTokens::from_disjoint_counters(1000, 0, 0), 0);
        assert!(read_only < fresh_only, "cache read must be cheaper");
        assert!(write_only > fresh_only, "cache write must be dearer");
    }

    use super::*;

    fn sample_json() -> &'static str {
        r#"{
          "display_name": "my-gpt4",
          "provider": "openai",
          "model_name": "gpt-4o",
          "provider_key_id": "11111111-1111-1111-1111-111111111111",
          "timeout": 30000,
          "rate_limit": {"rpm": 100, "tpm": 100000}
        }"#
    }

    #[test]
    fn deserialises_spec_sample() {
        let m: Model = serde_json::from_str(sample_json()).unwrap();
        assert_eq!(m.display_name, "my-gpt4");
        assert_eq!(m.provider.as_deref(), Some("openai"));
        assert_eq!(m.model_name.as_deref(), Some("gpt-4o"));
        assert_eq!(
            m.provider_key_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(m.timeout, Some(30_000));
        assert_eq!(m.rate_limit.as_ref().unwrap().rpm, Some(100));
    }

    #[test]
    fn deserialises_stream_timeout_and_helpers_fold_zero() {
        let m: Model = serde_json::from_str(
            r#"{
              "display_name": "my-gpt4",
              "provider": "openai",
              "model_name": "gpt-4o",
              "provider_key_id": "pk-1",
              "timeout": 30000,
              "stream_timeout": 2500
            }"#,
        )
        .unwrap();
        assert_eq!(m.stream_timeout, Some(2_500));
        assert_eq!(
            m.request_timeout_level(),
            Some(Some(std::time::Duration::from_millis(30_000)))
        );
        assert_eq!(
            m.stream_read_timeout(),
            Some(std::time::Duration::from_millis(2_500))
        );

        // Absent → defer to the next resolution level.
        let none: Model = serde_json::from_str(
            r#"{"display_name":"x","provider":"openai","model_name":"g","provider_key_id":"pk-1"}"#,
        )
        .unwrap();
        assert_eq!(none.request_timeout_level(), None);
        assert_eq!(none.stream_read_timeout(), None);

        // Explicit `timeout: 0` resolves to "no deadline" and stops the
        // chain; explicit `stream_timeout: 0` defers like absent.
        let zero: Model = serde_json::from_str(
            r#"{"display_name":"x","provider":"openai","model_name":"g","provider_key_id":"pk-1","timeout":0,"stream_timeout":0}"#,
        )
        .unwrap();
        assert_eq!(zero.request_timeout_level(), Some(None));
        assert_eq!(zero.stream_read_timeout(), None);
    }

    #[test]
    fn tolerates_unknown_top_level_fields_for_forward_compat() {
        // cp-api may ship new fields ahead of the DP rolling out; serde must
        // accept them. The write path still rejects them via `validate_model`
        // in models/schema.rs.
        let m: Model = serde_json::from_str(
            r#"{
              "display_name":"x","provider":"openai","model_name":"g",
              "provider_key_id":"pk-1",
              "foo": 1
            }"#,
        )
        .unwrap();
        assert_eq!(m.display_name, "x");
    }

    #[test]
    fn strip_kind_inapplicable_per_kind() {
        let load = |v: serde_json::Value| -> Model { serde_json::from_value(v).unwrap() };
        // Routing parent: model-specific knobs + top-level retries strip;
        // the group-level timeout pair stays (it IS the group slot).
        let mut group = load(serde_json::json!({
            "display_name": "g",
            "routing": {"targets": [{"model": "m"}]},
            "retries": 2,
            "timeout": 1000,
            "cost": {"input_per_1k": 0.0, "output_per_1k": 0.0},
            "auto_prompt_caching": {"enabled": true}
        }));
        let mut stripped = group.strip_kind_inapplicable();
        stripped.sort_unstable();
        assert_eq!(stripped, ["auto_prompt_caching", "cost", "retries"]);
        assert!(group.retries.is_none() && group.cost.is_none());
        assert_eq!(group.timeout, Some(1000));
        // Semantic parent: timeout/retries are the group slots and stay.
        let mut sem = load(serde_json::json!({
            "display_name": "s",
            "semantic": {
                "embedding_model": "e",
                "routes": [{"name": "r", "target": "t", "examples": ["x"]}],
                "default": "d",
                "match": {"threshold": 0.5}
            },
            "retries": 2,
            "timeout": 1000,
            "cost": {"input_per_1k": 0.0, "output_per_1k": 0.0}
        }));
        assert_eq!(sem.strip_kind_inapplicable(), ["cost"]);
        assert_eq!(sem.retries, Some(2));
        assert_eq!(sem.timeout, Some(1000));
        // Direct: nothing strips.
        let mut direct = load(serde_json::json!({
            "display_name": "m",
            "provider": "openai",
            "model_name": "gpt-4o",
            "provider_key_id": "pk-1",
            "retries": 2,
            "cost": {"input_per_1k": 0.0, "output_per_1k": 0.0}
        }));
        assert!(direct.strip_kind_inapplicable().is_empty());
        assert_eq!(direct.retries, Some(2));
        // Ensemble parent: the whole generic set strips (its own
        // deadline knob is `ensemble.timeout_ms`).
        let mut ens = load(serde_json::json!({
            "display_name": "e",
            "ensemble": {"panel": [{"model": "m"}], "judge": {"model": "j"}},
            "timeout": 1000,
            "stream_timeout": 500,
            "retries": 1,
            "cost": {"input_per_1k": 0.0, "output_per_1k": 0.0}
        }));
        // Asserted WITHOUT a pre-sort: the strip output is already
        // lexicographic (a pure-strip loader row keeps the fields
        // "sorted" per PartialCompatRow's contract).
        let ens_stripped = ens.strip_kind_inapplicable();
        assert_eq!(
            ens_stripped,
            ["cost", "retries", "stream_timeout", "timeout"]
        );
    }

    /// The parse cache must never outlive the strings it was built from: a
    /// stale allowlist on a security gate is worse than re-parsing.
    #[test]
    fn ip_allowed_cache_cannot_go_stale_across_a_mutated_clone() {
        fn model_with(cidrs: &[&str]) -> Model {
            let mut m: Model = serde_json::from_str(sample_json()).unwrap();
            m.allowed_cidrs = Some(cidrs.iter().map(|c| (*c).to_string()).collect());
            m
        }

        let original = model_with(&["10.0.0.0/8"]);
        assert!(original.ip_allowed("10.1.2.3"));
        assert!(!original.ip_allowed("192.168.1.5"));

        // Warm cache, then clone and point the copy at a different range.
        let mut changed = original.clone();
        changed.allowed_cidrs = Some(vec!["192.168.1.0/24".to_string()]);
        assert!(
            changed.ip_allowed("192.168.1.5"),
            "the clone must honour its own allowed_cidrs, not the original's cache"
        );
        assert!(!changed.ip_allowed("10.1.2.3"));

        // The original is unaffected.
        assert!(original.ip_allowed("10.1.2.3"));
    }

    #[test]
    fn ip_allowed_matrix() {
        fn model_with(cidrs: Option<Vec<&str>>) -> Model {
            let mut m: Model = serde_json::from_str(sample_json()).unwrap();
            m.allowed_cidrs = cidrs.map(|c| c.into_iter().map(String::from).collect());
            m
        }

        // No restriction → everything allowed.
        let open = model_with(None);
        assert!(open.ip_allowed("203.0.113.7"));
        assert!(open.ip_allowed("")); // even an unresolved IP

        // Empty list behaves like no restriction.
        assert!(model_with(Some(vec![])).ip_allowed("203.0.113.7"));

        // IPv4 allowlist: in-range allowed, out-of-range denied.
        let v4 = model_with(Some(vec!["10.0.0.0/8", "192.168.1.0/24"]));
        assert!(v4.ip_allowed("10.1.2.3"));
        assert!(v4.ip_allowed("192.168.1.42"));
        assert!(!v4.ip_allowed("114.114.114.114"));
        assert!(!v4.ip_allowed("192.168.2.1"));

        // Dual-stack listener: an IPv4 client arrives as `::ffff:a.b.c.d`,
        // which must match the operator's IPv4 CIDR rather than 403.
        assert!(v4.ip_allowed("::ffff:10.1.2.3"));
        assert!(!v4.ip_allowed("::ffff:114.114.114.114"));
        assert!(model_with(Some(vec!["0.0.0.0/0"])).ip_allowed("::ffff:1.2.3.4"));

        // Fail closed: a restriction set but the client IP is empty/garbage.
        assert!(!v4.ip_allowed(""));
        assert!(!v4.ip_allowed("not-an-ip"));

        // IPv6 allowlist.
        let v6 = model_with(Some(vec!["2001:db8::/32"]));
        assert!(v6.ip_allowed("2001:db8::1"));
        assert!(!v6.ip_allowed("2001:db9::1"));

        // Malformed CIDR entries are skipped, valid ones still apply.
        let mixed = model_with(Some(vec!["garbage", "10.0.0.0/8"]));
        assert!(mixed.ip_allowed("10.0.0.1"));
        assert!(!mixed.ip_allowed("203.0.113.1"));
    }

    #[test]
    fn deserialises_allowed_cidrs() {
        let m: Model = serde_json::from_str(
            r#"{"display_name":"x","provider":"openai","model_name":"g","provider_key_id":"pk-1","allowed_cidrs":["10.0.0.0/8"]}"#,
        )
        .unwrap();
        assert_eq!(
            m.allowed_cidrs.as_deref(),
            Some(&["10.0.0.0/8".to_string()][..])
        );

        // Absent field → None → no restriction.
        let none: Model = serde_json::from_str(
            r#"{"display_name":"x","provider":"openai","model_name":"g","provider_key_id":"pk-1"}"#,
        )
        .unwrap();
        assert!(none.allowed_cidrs.is_none());
        assert!(none.ip_allowed("203.0.113.7"));
    }

    #[test]
    fn routing_form_has_no_provider_or_provider_key_id() {
        let m: Model = serde_json::from_str(
            r#"{
              "display_name": "router-1",
              "routing": {
                "strategy": "round_robin",
                "targets": [{"model": "my-gpt4"}, {"model": "my-claude"}]
              }
            }"#,
        )
        .unwrap();
        assert!(m.is_routing());
        assert!(m.provider.is_none());
        assert!(m.model_name.is_none());
        assert!(m.provider_key_id.is_none());
    }

    #[test]
    fn ensemble_form_has_no_provider_and_reports_is_ensemble() {
        let m: Model = serde_json::from_str(
            r#"{
              "display_name": "council",
              "ensemble": {
                "panel": [{"model": "my-gpt4"}, {"model": "my-claude"}],
                "judge": {"model": "my-opus"}
              }
            }"#,
        )
        .unwrap();
        assert!(m.is_ensemble());
        assert!(!m.is_routing());
        assert!(m.provider.is_none());
        assert!(m.model_name.is_none());
        assert!(m.provider_key_id.is_none());
    }

    #[test]
    fn resource_trait_routes_through_display_name() {
        let mut m: Model = serde_json::from_str(sample_json()).unwrap();
        m.runtime_id = "uuid-1".into();
        assert_eq!(<Model as Resource>::kind(), "models");
        assert_eq!(m.id(), "uuid-1");
        assert_eq!(m.name(), "my-gpt4");
    }

    #[test]
    fn cooldown_config_defaults_via_helpers() {
        let cfg = CooldownConfig::default();
        assert!(cfg.enabled_or_default());
        assert_eq!(cfg.default_seconds_or_default(), 30);
        assert_eq!(cfg.max_seconds_or_default(), 600);
        assert!(cfg.honor_retry_after_or_default());
        assert_eq!(
            cfg.effective_trigger_statuses().as_ref(),
            DEFAULT_COOLDOWN_TRIGGER_STATUSES,
        );
        assert!(cfg.trigger_on_timeout_or_default());
        assert!(cfg.trigger_on_transport_or_default());
    }

    #[test]
    fn cooldown_default_trigger_statuses_match_advertised_set() {
        // Lock the documented default so a future change has to update
        // both the constant and the test, surfaced as one diff.
        assert_eq!(
            DEFAULT_COOLDOWN_TRIGGER_STATUSES,
            &[401, 408, 429, 500, 502, 503, 504]
        );
    }

    #[test]
    fn cooldown_config_partial_override_keeps_other_defaults() {
        let cfg: CooldownConfig = serde_json::from_str(r#"{"default_seconds": 90}"#).unwrap();
        assert_eq!(cfg.default_seconds_or_default(), 90);
        // Other fields fall back to defaults.
        assert!(cfg.enabled_or_default());
        assert_eq!(cfg.max_seconds_or_default(), 600);
        assert!(cfg.honor_retry_after_or_default());
    }

    #[test]
    fn cooldown_config_disable_via_enabled_false() {
        let cfg: CooldownConfig = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert!(!cfg.enabled_or_default());
    }

    #[test]
    fn cooldown_config_override_trigger_statuses() {
        let cfg: CooldownConfig = serde_json::from_str(r#"{"trigger_statuses": [503]}"#).unwrap();
        assert_eq!(cfg.effective_trigger_statuses().as_ref(), &[503]);
    }

    #[test]
    fn direct_model_can_deserialize_cooldown_config() {
        let m: Model = serde_json::from_str(
            r#"{
              "display_name": "my-gpt4",
              "provider": "openai",
              "model_name": "gpt-4o",
              "provider_key_id": "11111111-1111-1111-1111-111111111111",
              "cooldown": {
                "enabled": true,
                "default_seconds": 45,
                "trigger_statuses": [429, 503]
              }
            }"#,
        )
        .unwrap();
        let cooldown = m.cooldown.unwrap();
        assert!(cooldown.enabled_or_default());
        assert_eq!(cooldown.default_seconds_or_default(), 45);
        assert_eq!(cooldown.effective_trigger_statuses().as_ref(), &[429, 503]);
    }

    #[test]
    fn direct_model_can_deserialize_background_check() {
        let m: Model = serde_json::from_str(
            r#"{
              "display_name": "my-gpt4",
              "provider": "openai",
              "model_name": "gpt-4o",
              "provider_key_id": "11111111-1111-1111-1111-111111111111",
              "background_model_check": {
                "enabled": true,
                "interval_seconds": 30,
                "timeout_seconds": 10,
                "prompt": "Respond with OK",
                "max_tokens": 8,
                "ignore_statuses": [408, 429],
                "stale_after_seconds": 90
              }
            }"#,
        )
        .unwrap();
        let bg = m.background_model_check.unwrap();
        assert!(bg.enabled);
        assert_eq!(bg.ignore_statuses, vec![408, 429]);
    }

    // `adapter_from_provider_covers_every_variant` removed alongside
    // the `From<Provider> for Adapter` impl — both are dead post-#302
    // Phase A. ProviderKey.adapter carries the Adapter directly.

    #[test]
    fn adapter_serializes_to_kebab_case_wire_strings() {
        // Pin each Adapter's wire form. AzureOpenai → "azure-openai"
        // is the load-bearing case for the kebab-case choice; the
        // others are pinned to lock the contract so a future
        // rename_all change is surfaced as a test failure.
        assert_eq!(
            serde_json::to_string(&Adapter::Openai).unwrap(),
            "\"openai\""
        );
        assert_eq!(
            serde_json::to_string(&Adapter::Anthropic).unwrap(),
            "\"anthropic\""
        );
        assert_eq!(
            serde_json::to_string(&Adapter::Bedrock).unwrap(),
            "\"bedrock\""
        );
        assert_eq!(
            serde_json::to_string(&Adapter::Vertex).unwrap(),
            "\"vertex\""
        );
        assert_eq!(
            serde_json::to_string(&Adapter::AzureOpenai).unwrap(),
            "\"azure-openai\""
        );
    }

    #[test]
    fn adapter_deserializes_from_kebab_case_wire_strings() {
        assert_eq!(
            serde_json::from_str::<Adapter>("\"openai\"").unwrap(),
            Adapter::Openai
        );
        assert_eq!(
            serde_json::from_str::<Adapter>("\"anthropic\"").unwrap(),
            Adapter::Anthropic
        );
        assert_eq!(
            serde_json::from_str::<Adapter>("\"bedrock\"").unwrap(),
            Adapter::Bedrock
        );
        assert_eq!(
            serde_json::from_str::<Adapter>("\"vertex\"").unwrap(),
            Adapter::Vertex
        );
        assert_eq!(
            serde_json::from_str::<Adapter>("\"azure-openai\"").unwrap(),
            Adapter::AzureOpenai
        );
    }

    #[test]
    fn adapter_rejects_unknown_variant_strings() {
        // Closed enum — any string outside the kebab-case wire set
        // must fail to deserialize so callers can't silently smuggle
        // in a typo or a legacy provider name.
        assert!(serde_json::from_str::<Adapter>("\"gemini\"").is_err());
        assert!(serde_json::from_str::<Adapter>("\"azureopenai\"").is_err());
        assert!(serde_json::from_str::<Adapter>("\"azure_openai\"").is_err());
    }

    // `every_provider_variant_has_as_str_and_adapter` removed —
    // the `Provider` enum it pinned no longer exists post-#302
    // Phase A. Vendor identity is now a free-form string on
    // `ProviderKey.provider` / `Model.provider`.
}
