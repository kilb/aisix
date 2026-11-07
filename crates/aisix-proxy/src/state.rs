//! Axum state shared across every proxy handler.
//!
//! `ProxyState` holds:
//! - the lock-free `SnapshotHandle<AisixSnapshot>` for looking up
//!   Models and ApiKeys on every request
//! - the `Hub` for resolving a `Provider` to the Bridge that serves it
//! - the per-key [`Limiter`] — queried before each upstream call and
//!   finalised after the response completes
//! - an `Arc<Metrics>` shared with the admin `/metrics` endpoint
//! - the [`CacheBackends`] consulted before bridge dispatch (None disables
//!   caching for that ProxyState; tests use this to keep the cache off
//!   the hot path when they don't care about it)
//! - the configured request-body size limit
//!
//! Cheap to clone: every field is either an `Arc` or a small Copy scalar.

use std::sync::atomic::{AtomicBool, Ordering};

use aisix_cache::{Cache, MemoryCache, MemorySemanticCache, SemanticCacheStore};
use aisix_core::models::CacheBackend;
use aisix_core::models::{LiveMcpServerIndex, LivePricingIndex};
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ProxyConfig};
use aisix_gateway::Hub;
use aisix_guardrails::LiveGuardrailIndex;
use aisix_obs::{ClientTypeClassifier, Metrics, OtlpHttpFanOut, UsageSink};
use aisix_ratelimit::Limiter;
use dashmap::DashSet;
use std::sync::Arc;

use crate::client_ip::ResolvedRealIp;
use crate::health::{HealthTracker, LivezState, ModelRuntimeStatusTracker};
use crate::routing::RoutingRegistry;

/// The cache instances a DP deployment has available, selected per
/// request by the matched `CachePolicy.backend` (#519 B.8).
///
/// The memory cache is always built (in-process, no config needed);
/// the redis cache exists iff the boot config carries `cache.redis`.
/// A policy that asks for `redis` on a deployment without one gets NO
/// caching for its requests (`cache_status = disabled`) — never a
/// silent fallback to node-local memory, which would lie about the
/// sharing semantics the operator picked.
/// The write-once home of the `backend: redis` semantic store.
///
/// A `OnceLock` rather than a swap cell because that is the whole
/// contract: the vector-search question is answered once, against the
/// first live connection, and the answer never changes for the life of
/// the process.
pub type SemanticRedisCell = std::sync::OnceLock<Arc<dyn SemanticCacheStore>>;

#[derive(Clone)]
pub struct CacheBackends {
    memory: Arc<dyn Cache>,
    redis: Option<Arc<dyn Cache>>,
    /// Semantic (L2) store for `backend: memory` policies. Always
    /// built — in-process, no config needed, zero cost until a policy
    /// with a `semantic` block matches a request.
    semantic_memory: Arc<dyn SemanticCacheStore>,
    /// Semantic (L2) store for `backend: redis` policies. Wired only
    /// when `cache.redis` is configured, is not cluster mode, AND the
    /// server passed the vector-search capability probe — so its absence
    /// here IS the degradation signal, and a policy that asks for
    /// semantic matching is told once and then served exact-only with no
    /// embedding call and no Redis round trip.
    ///
    /// Swappable because the probe can only run against a live
    /// connection, and `cache.redis` may be unreachable when the gateway
    /// starts: the answer is then not "no vector search" but "not asked
    /// yet", and the background attach fills it in when it gets there.
    /// It is written once, by that attach, and never cleared.
    semantic_redis: Arc<SemanticRedisCell>,
    /// Policy ids already warned about an unavailable redis backend,
    /// so the gate logs once per policy instead of once per request.
    redis_warned: Arc<DashSet<String>>,
    /// Policy ids already warned about the redis semantic layer being
    /// unavailable (same warn-once discipline as `redis_warned`). The
    /// line has to name both reasons the cell can be empty, because
    /// warn-once means it is never corrected: a store published by the
    /// background attach simply stops the gate reaching this branch.
    semantic_redis_warned: Arc<DashSet<String>>,
    /// Policy ids already warned about a stable semantic config error
    /// (missing / non-embedding `embedding_model`). The per-request
    /// metric keeps counting; only the log line is deduplicated.
    semantic_resolve_warned: Arc<DashSet<String>>,
    /// Set while the redis cache is failing, so the gate reports an
    /// outage once rather than once per request. Re-armed by the next
    /// success, so a second outage is reported again.
    ///
    /// A cache Redis can now be unreachable from boot and stay that way
    /// (the connection attaches in the background instead of the process
    /// exiting), and every cached-policy request produces both a read and
    /// a write failure — so an unthrottled line is two per request for as
    /// long as the outage lasts, which buries every other line in the
    /// log. How hard and how long it is failing is
    /// `aisix_redis_failures_total{operation}`; the log says that it
    /// started.
    ///
    /// Two latches, not one: the exact-KV and vector-search halves fail
    /// and recover independently, and only one of them costs an
    /// embedding call.
    exact_degraded: Arc<AtomicBool>,
    semantic_degraded: Arc<AtomicBool>,
}

impl CacheBackends {
    pub fn new(memory: Arc<dyn Cache>, redis: Option<Arc<dyn Cache>>) -> Self {
        Self {
            memory,
            redis,
            semantic_memory: Arc::new(MemorySemanticCache::new()),
            semantic_redis: Arc::new(SemanticRedisCell::new()),
            redis_warned: Arc::new(DashSet::new()),
            semantic_redis_warned: Arc::new(DashSet::new()),
            semantic_resolve_warned: Arc::new(DashSet::new()),
            exact_degraded: Arc::new(AtomicBool::new(false)),
            semantic_degraded: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Entries held by the in-process exact-match response cache.
    pub fn response_cache_entries(&self) -> u64 {
        self.memory.local_entry_count().unwrap_or(0)
    }

    /// Entries held by the in-process semantic cache.
    pub fn semantic_cache_entries(&self) -> u64 {
        self.semantic_memory.local_entry_count().unwrap_or(0)
    }

    /// True the first time the exact-KV cache fails in an outage, false
    /// for the rest of it — the caller logs only when it is true.
    pub fn note_exact_failure(&self) -> bool {
        !self.exact_degraded.swap(true, Ordering::Relaxed)
    }

    /// Re-arm [`Self::note_exact_failure`] after a successful operation.
    ///
    /// Read before write: this runs on every cache MISS, the commonest
    /// branch there is, and an unconditional store is a cross-core line
    /// invalidation per request under thread-per-core serving. The
    /// healthy path only ever reads.
    pub fn note_exact_success(&self) {
        if self.exact_degraded.load(Ordering::Relaxed) {
            self.exact_degraded.store(false, Ordering::Relaxed);
        }
    }

    /// [`Self::note_exact_failure`] for the vector-search half.
    pub fn note_semantic_failure(&self) -> bool {
        !self.semantic_degraded.swap(true, Ordering::Relaxed)
    }

    /// Re-arm [`Self::note_semantic_failure`]. Read before write, for
    /// the reason on [`Self::note_exact_success`].
    pub fn note_semantic_success(&self) {
        if self.semantic_degraded.load(Ordering::Relaxed) {
            self.semantic_degraded.store(false, Ordering::Relaxed);
        }
    }

    /// Attach the shared semantic store for `backend: redis` policies.
    /// Callers reach this only after the capability probe passed.
    pub fn with_semantic_redis(self, store: Arc<dyn SemanticCacheStore>) -> Self {
        let _ = self.semantic_redis.set(store);
        self
    }

    /// The cell [`Self::with_semantic_redis`] writes, for a bootstrap
    /// that has to attach the store later than it builds the backends —
    /// a `cache.redis` that was unreachable at startup.
    pub fn semantic_redis_cell(&self) -> Arc<SemanticRedisCell> {
        Arc::clone(&self.semantic_redis)
    }

    /// True the FIRST time `policy_id` reports a stable semantic config
    /// error, so the gate logs once per policy instead of per request.
    pub fn semantic_resolve_warn_once(&self, policy_id: &str) -> bool {
        self.semantic_resolve_warned.insert(policy_id.to_string())
    }

    /// Memory cache only — the default for self-hosted dev and tests.
    pub fn memory_only() -> Self {
        Self::new(Arc::new(MemoryCache::with_defaults()), None)
    }

    /// Resolve the cache instance for a matched policy's `backend`.
    ///
    /// `Memory` always resolves. `Redis` resolves iff the deployment
    /// configured one; otherwise caching is inactive for the request
    /// and we warn once per policy id.
    pub fn for_policy_backend(
        &self,
        backend: CacheBackend,
        policy_id: &str,
        policy_name: &str,
    ) -> Option<&Arc<dyn Cache>> {
        match backend {
            CacheBackend::Memory => Some(&self.memory),
            CacheBackend::Redis => {
                let redis = self.redis.as_ref();
                if redis.is_none() && self.redis_warned.insert(policy_id.to_string()) {
                    tracing::warn!(
                        target: "aisix::cache",
                        policy_id = %policy_id,
                        policy_name = %policy_name,
                        "cache policy requests backend=redis but this DP has no \
                         redis cache configured; caching is disabled for matching \
                         requests (set `cache.redis` in the gateway config)"
                    );
                }
                redis
            }
        }
    }

    /// Resolve the semantic (L2) store for a matched policy's
    /// `backend`. Same never-fall-back discipline as
    /// [`Self::for_policy_backend`]: a policy whose backend has no
    /// semantic store gets NO semantic matching (exact matching still
    /// works) rather than a silent per-node stand-in with different
    /// sharing semantics.
    pub fn semantic_for_policy_backend(
        &self,
        backend: CacheBackend,
        policy_id: &str,
        policy_name: &str,
    ) -> Option<&Arc<dyn SemanticCacheStore>> {
        match backend {
            CacheBackend::Memory => Some(&self.semantic_memory),
            CacheBackend::Redis => {
                let store = self.semantic_redis.get();
                if store.is_none() && self.semantic_redis_warned.insert(policy_id.to_string()) {
                    tracing::warn!(
                        target: "aisix::cache",
                        policy_id = %policy_id,
                        policy_name = %policy_name,
                        "cache policy configures semantic matching on backend=redis but \
                         no vector-search store is available: either the configured \
                         cache.redis has no vector-search support (requires Redis 8+ or \
                         the search module; cluster mode is not supported yet), or it \
                         was unreachable at startup and has not been probed yet — a \
                         background attach publishes the store if the probe then \
                         passes. Requests fall back to exact matching only"
                    );
                }
                store
            }
        }
    }
}

/// How long a completed batch's id stays in the process-local dedup map.
/// As long as a provider keeps a finished batch: a list keeps showing it for
/// that long, and an expired entry costs one more download of its output.
/// Short enough that the map cannot grow for the life of the process.
pub(crate) const BILLED_BATCH_DEDUP_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(31 * 24 * 3600);

/// A batch poll a minute: results arrive hours apart, and a charge landing a
/// minute late changes nothing.
const DEFAULT_BATCH_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Record that this process has attributed `batch_id`, returning `false` when
/// it was already recorded inside the dedup window. An entry older than `ttl`
/// is refreshed and counts as a first sighting.
///
/// It touches the one entry only: a listing re-observes every batch on its
/// page on the request path, so a walk of the whole map here would cost that
/// much per listed batch. Entries no batch is looked up by again are
/// reclaimed by [`sweep_billed_batches`], on the server's upkeep timer. The
/// set is an optimization, not the correctness mechanism — the deterministic
/// `request_id = "batch-<id>"` on the emitted events is what keeps
/// re-emission idempotent across a restart (#720) — so expiring an entry
/// costs at worst one redundant download and one redundant, already-idempotent
/// emit.
///
/// That covers the event and nothing else. The spend counters the attribution
/// also feeds are plain counters: re-crediting them charges the batch again.
/// They are guarded separately by a `RateStore::claim_once` marker, which
/// lives beside the counters it protects and so survives exactly as long as
/// they do. Do not extend this set's job to them — an in-process map cannot
/// guard state that outlives the process.
pub(crate) fn remember_billed_batch(
    seen: &dashmap::DashMap<String, std::time::Instant>,
    batch_id: &str,
    now: std::time::Instant,
    ttl: std::time::Duration,
) -> bool {
    match seen.entry(batch_id.to_string()) {
        dashmap::mapref::entry::Entry::Occupied(mut recorded) => {
            if now.duration_since(*recorded.get()) <= ttl {
                return false;
            }
            recorded.insert(now);
            true
        }
        dashmap::mapref::entry::Entry::Vacant(slot) => {
            slot.insert(now);
            true
        }
    }
}

/// Drop the entries of [`remember_billed_batch`]'s map that are older than
/// `ttl`, so the map cannot grow for the life of the process.
pub(crate) fn sweep_billed_batches(
    seen: &dashmap::DashMap<String, std::time::Instant>,
    now: std::time::Instant,
    ttl: std::time::Duration,
) {
    seen.retain(|_, recorded| now.duration_since(*recorded) <= ttl);
}

#[derive(Clone)]
pub struct ProxyState {
    // Axum clones state for several layers on every request. Keep the many
    // shared handles behind one refcount so each clone/drop uses one atomic.
    inner: Arc<ProxyStateInner>,
}

#[derive(Clone)]
pub struct ProxyStateInner {
    pub snapshot: SnapshotHandle<AisixSnapshot>,
    pub hub: Arc<Hub>,
    pub limiter: Arc<Limiter>,
    pub(crate) policy_index: Arc<crate::policy_index::LivePolicyIndex>,
    pub(crate) jwt_bindings: Arc<crate::jwt_index::LiveJwtBindings>,
    pub metrics: Arc<Metrics>,
    pub cache: Option<CacheBackends>,
    pub routing: Arc<RoutingRegistry>,
    /// Per-instance cache of semantic-router example embeddings, populated
    /// lazily on first use and reused across requests so a steady-state
    /// logical embedding batch contains only the prompt.
    pub semantic_cache: Arc<crate::semantic::SemanticVectorCache>,
    /// Per-request guardrail index. Resolves the applicable chain from
    /// attachment scope + priority on each request. Rebuilds lazily
    /// when the snapshot version changes. Default is an empty index
    /// (no-op); the server bootstrap wires a live handle at startup.
    pub guardrail_index: Arc<LiveGuardrailIndex>,
    /// Prices by `pricing_key`, derived from the two pricing tables and
    /// rebuilt only when one of them changes. Every reader of a model's
    /// price goes through it — `least_cost` ranking and the `cost_usd` on
    /// the usage events — so ranking and billing cannot disagree about
    /// what a model costs.
    pub pricing: Arc<LivePricingIndex>,
    /// Registered MCP servers by name → resource id, rebuilt only when the
    /// `mcp_servers` table changes. Both MCP gates that a key can address by
    /// server id read it — the tool ACL and the per-server rate limit — so
    /// the two resolve a rename at the same instant.
    pub mcp_servers: Arc<LiveMcpServerIndex>,
    /// Per-user tokens of `oauth2_authorization_code` MCP servers.
    /// [`ProxyState::with_mcp_oauth_store`] picks where they are kept; until
    /// then, this process alone.
    pub(crate) mcp_oauth: Arc<aisix_mcp::UserOAuthManager>,
    /// The `budgets` documents indexed for the per-request budget gate,
    /// rebuilt only when that table changes.
    pub(crate) budgets: Arc<crate::budget::LiveBudgetIndex>,
    /// Per-model health tracker. Updated on every upstream call outcome;
    /// read by `GET /admin/v1/health`.
    pub health: Arc<HealthTracker>,
    /// Public liveness state served on `GET /livez`.
    pub livez: Arc<LivezState>,
    /// Runtime model-status tracker keyed by resolved direct-model id.
    /// Used for request-path cooldown/background health exclusion and
    /// surfaced by `GET /admin/v1/models/status`.
    pub runtime_status: Arc<ModelRuntimeStatusTracker>,
    /// CP-side usage telemetry sink. Backed by an mpsc channel into the
    /// sender worker spawned in aisix-server (see `telemetry::spawn`).
    /// Defaults to a no-op sink when running outside managed mode so
    /// chat handlers don't have to special-case `Option`.
    pub usage_sink: UsageSink,
    /// Per-env OTLP/HTTP fan-out — POSTs one OTLP-encoded span per
    /// chat request to every enabled `ObservabilityExporter` in the
    /// snapshot. Cheap clonable handle holding a shared
    /// `reqwest::Client` connection pool. Always present (the
    /// no-exporters case = empty snapshot table = no spawned tasks).
    pub otlp_fan_out: OtlpHttpFanOut,
    /// `0` is unlimited; `n` applies `n` globally.
    pub request_body_limit_bytes: usize,
    /// Pre-parsed `proxy.real_ip` config for resolving the downstream
    /// client IP on each request (#492). Default = trust nothing → the
    /// logged source IP is the immediate TCP peer.
    pub real_ip: Arc<ResolvedRealIp>,
    /// Pre-parsed `proxy.request_id.accept_headers`: the inbound headers a
    /// caller may supply its own request id in, in priority order
    /// (#1288). Default = `[x-aisix-request-id]`.
    pub request_id_accept: Arc<[axum::http::HeaderName]>,
    /// Boot-compiled `proxy.url_rewrites` rules, applied in order to every
    /// request before routing (first match wins). Empty = layer no-ops.
    pub url_rewrites: Arc<[crate::rewrite::CompiledRewrite]>,
    /// `proxy.expose_debug_headers`: whether chat completions carry the
    /// `x-aisix-call-id` / `-served-by` / `-route` / `-cache*` headers. In
    /// platform mode it governs the other endpoints' `-cache*` headers too.
    pub expose_debug_headers: bool,
    /// Config-freshness probe for `GET /readyz`: returns the time since the
    /// config source last applied a snapshot (`None` = never applied). Every
    /// constructor installs a fail-closed probe; server bootstrap must replace
    /// it with its etcd/file source signal before serving traffic (#803).
    pub config_apply_age: Option<Arc<dyn Fn() -> Option<std::time::Duration> + Send + Sync>>,
    /// Batch ids whose completed output has already been attributed to
    /// UsageEvents by THIS process (#720). Process-local dedup only — the
    /// deterministic `request_id = "batch-<id>"` on the emitted events is
    /// what keeps cross-restart re-emission idempotent on the control plane side.
    pub billed_batches: Arc<dashmap::DashMap<String, std::time::Instant>>,
    /// Permits for a batch attribution's output download, so only
    /// [`crate::jobs::BATCH_ATTRIBUTION_DOWNLOADS`] run at once.
    pub(crate) batch_attribution_permits: Arc<tokio::sync::Semaphore>,
    /// How often [`crate::jobs`] re-reads a batch it created until it ends.
    pub batch_watch_interval: std::time::Duration,
    /// Boot-compiled User-Agent → `client_type` classifier: operator
    /// rules from `observability.metrics.client_type_rules` first, then
    /// the built-in allowlist (#1045). Default = built-ins
    /// only; the server bootstrap swaps in the compiled config rules.
    pub client_classifier: Arc<ClientTypeClassifier>,
    /// Deployment-wide retry budget (`upstream.retries`) — the floor every
    /// dispatch falls back to when neither the target Model nor its model
    /// group sets one. See `routing::effective_retries`.
    pub default_retries: u32,
    /// Deployment-wide timeout defaults (`upstream.timeout_ms` /
    /// `upstream.stream_timeout_ms`) — the floor every dispatch falls back
    /// to when neither the target Model nor its model group sets one. See
    /// `routing::effective_timeouts`.
    pub default_timeouts: crate::routing::TimeoutDefaults,
    /// Platform mode settings; `None` while `proxy.platform.enabled` is off.
    platform: Option<Arc<crate::platform::PlatformSettings>>,
}

impl std::ops::Deref for ProxyState {
    type Target = ProxyStateInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for ProxyState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.inner)
    }
}

/// Frozen `unix_secs` for unit-test limiters — an arbitrary mid-window
/// instant; the exact value only shapes reported retry-after seconds.
#[cfg(test)]
const TEST_RATE_LIMIT_CLOCK_SECS: u64 = 1_763_000_000;

/// Reclaim runtime state for config rows the snapshot no longer carries.
///
/// Health counters, per-target runtime status and round-robin cursors are all
/// keyed by configuration rather than by traffic, so nothing bounds them: a
/// row deleted from the control plane leaves its state behind until the
/// process restarts. The publication hook is the natural place to notice —
/// it already drives exporter reconciliation — and it is the only signal that
/// says *which* rows still exist.
fn reconcile_runtime_state(
    snapshot: &SnapshotHandle<AisixSnapshot>,
    health: Arc<crate::health::HealthTracker>,
    runtime_status: Arc<ModelRuntimeStatusTracker>,
    routing: Arc<RoutingRegistry>,
) {
    snapshot.subscribe_before_publish(move |view| {
        let models = &view.snapshot.models;
        // Health and routing cursors key on the operator-facing name;
        // per-target runtime status keys on the row id.
        health.retain_configured(&|name| models.get_by_name(name).is_some());
        routing.retain_configured(&|name| models.get_by_name(name).is_some());
        runtime_status.retain_configured(&|id| models.get_by_id(id).is_some());
    });
}

/// `metrics` is what puts each exporter's drops and hard delivery failures on
/// the Prometheus scrape.
fn live_exporter_fan_out(
    snapshot: &SnapshotHandle<AisixSnapshot>,
    metrics: Metrics,
) -> OtlpHttpFanOut {
    let fan_out = OtlpHttpFanOut::with_metrics(metrics);
    let reconciler = fan_out.clone();
    snapshot.subscribe_before_publish(move |view| {
        let exporters = view.snapshot.observability_exporters.entries();
        reconciler.reconcile(view.version, exporters.iter().map(|entry| &*entry.value));
    });
    fan_out
}

/// The embedding dispatcher `kind: "semantic"` guardrail rows compile
/// against. Free function rather than a method because the three
/// `ProxyState` constructors need it before `self` exists.
fn guardrail_embedder_slot(
    hub: &Arc<Hub>,
    snapshot: &SnapshotHandle<AisixSnapshot>,
    cache: &Arc<crate::semantic::SemanticVectorCache>,
) -> aisix_guardrails::GuardrailEmbedderSlot {
    aisix_guardrails::GuardrailEmbedderSlot::new(Arc::new(
        crate::guardrail_embedder::ProxyGuardrailEmbedder::new(
            Arc::clone(hub),
            snapshot.clone(),
            Arc::clone(cache),
        ),
    ))
}

impl ProxyState {
    /// This state's embedding dispatcher, for callers that rebuild the
    /// guardrail index and must not drop the embedder while doing so.
    pub fn guardrail_embedder(&self) -> aisix_guardrails::GuardrailEmbedderSlot {
        guardrail_embedder_slot(&self.hub, &self.snapshot, &self.semantic_cache)
    }

    /// The upkeep job that reclaims billed-batch dedup entries past
    /// [`BILLED_BATCH_DEDUP_WINDOW`] ([`sweep_billed_batches`]). The server
    /// runs it on a timer, so the request path never walks the map.
    pub fn billed_batch_sweep(&self) -> impl Fn() + Send + 'static {
        let seen = Arc::clone(&self.billed_batches);
        move || sweep_billed_batches(&seen, std::time::Instant::now(), BILLED_BATCH_DEDUP_WINDOW)
    }
}

impl ProxyState {
    /// Resolve the configured body cap. The path is retained at call sites so
    /// a future explicit per-endpoint mode can be added without another
    /// handler sweep; omission remains unlimited for backwards compatibility.
    pub(crate) fn request_body_limit_for(&self, _path: &str) -> usize {
        self.request_body_limit_bytes
    }

    pub fn new(snapshot: SnapshotHandle<AisixSnapshot>, hub: Arc<Hub>, cfg: &ProxyConfig) -> Self {
        let metrics = Arc::new(Metrics::new(false));
        let semantic_cache = Arc::new(crate::semantic::SemanticVectorCache::default());
        let guardrail_index = LiveGuardrailIndex::new_with_sink(
            snapshot.clone(),
            None,
            Some(metrics.clone()),
            guardrail_embedder_slot(&hub, &snapshot, &semantic_cache),
        );
        let otlp_fan_out = live_exporter_fan_out(&snapshot, (*metrics).clone());
        // Unit tests get a frozen rate-limit clock: on the wall clock, any
        // "the next request 429s" assertion silently races the fixed-window
        // minute boundary — a test that straddles :00 lands its two requests
        // in different windows and the 429 never comes (seen flaking in the
        // mcp per-server-limit tests under a loaded runner). Freezing the
        // clock puts every request of a test in one window by construction.
        // Only this crate's own test build is affected; other crates calling
        // `ProxyState::new` (e.g. aisix-admin's playground) compile the
        // system-clock arm.
        #[cfg(test)]
        let limiter = Arc::new(Limiter::local_with_clock(aisix_ratelimit::TestClock::new(
            TEST_RATE_LIMIT_CLOCK_SECS,
        )));
        #[cfg(not(test))]
        let limiter = Arc::new(Limiter::new());
        Self::from_inner(ProxyStateInner {
            snapshot,
            hub,
            limiter,
            policy_index: Arc::new(crate::policy_index::LivePolicyIndex::default()),
            jwt_bindings: Arc::new(crate::jwt_index::LiveJwtBindings::default()),
            metrics,
            cache: Some(CacheBackends::memory_only()),
            routing: Arc::new(RoutingRegistry::new()),
            semantic_cache,
            guardrail_index,
            pricing: Arc::new(LivePricingIndex::new()),
            mcp_servers: Arc::new(LiveMcpServerIndex::new()),
            mcp_oauth: Arc::new(aisix_mcp::UserOAuthManager::new(Arc::new(
                aisix_mcp::MemoryStore::default(),
            ))),
            budgets: Arc::new(crate::budget::LiveBudgetIndex::default()),
            health: Arc::new(HealthTracker::new()),
            livez: Arc::new(LivezState::new()),
            config_apply_age: Some(Arc::new(|| None)),
            runtime_status: Arc::new(ModelRuntimeStatusTracker::new()),
            usage_sink: UsageSink::disabled(),
            otlp_fan_out,
            request_body_limit_bytes: cfg.request_body_limit_bytes,
            real_ip: Arc::new(ResolvedRealIp::from_config(&cfg.real_ip)),
            request_id_accept: cfg
                .request_id
                .parse_accept_headers()
                .unwrap_or_default()
                .into(),
            url_rewrites: crate::rewrite::compile(&cfg.url_rewrites),
            expose_debug_headers: cfg.expose_debug_headers,
            billed_batches: Arc::new(dashmap::DashMap::new()),
            batch_attribution_permits: Arc::new(tokio::sync::Semaphore::new(
                crate::jobs::BATCH_ATTRIBUTION_DOWNLOADS,
            )),
            batch_watch_interval: DEFAULT_BATCH_WATCH_INTERVAL,
            client_classifier: Arc::new(ClientTypeClassifier::builtin()),
            default_retries: aisix_core::config::DEFAULT_UPSTREAM_RETRIES,
            default_timeouts: crate::routing::TimeoutDefaults::default(),
            platform: None,
        })
    }

    /// Alternative constructor for callers that want to share a preexisting
    /// limiter (e.g. tests with a deterministic clock).
    pub fn with_limiter(
        snapshot: SnapshotHandle<AisixSnapshot>,
        hub: Arc<Hub>,
        limiter: Arc<Limiter>,
        cfg: &ProxyConfig,
    ) -> Self {
        let metrics = Arc::new(Metrics::new(false));
        let semantic_cache = Arc::new(crate::semantic::SemanticVectorCache::default());
        let guardrail_index = LiveGuardrailIndex::new_with_sink(
            snapshot.clone(),
            None,
            Some(metrics.clone()),
            guardrail_embedder_slot(&hub, &snapshot, &semantic_cache),
        );
        let otlp_fan_out = live_exporter_fan_out(&snapshot, (*metrics).clone());
        Self::from_inner(ProxyStateInner {
            snapshot,
            hub,
            limiter,
            policy_index: Arc::new(crate::policy_index::LivePolicyIndex::default()),
            jwt_bindings: Arc::new(crate::jwt_index::LiveJwtBindings::default()),
            metrics,
            cache: Some(CacheBackends::memory_only()),
            routing: Arc::new(RoutingRegistry::new()),
            semantic_cache,
            guardrail_index,
            pricing: Arc::new(LivePricingIndex::new()),
            mcp_servers: Arc::new(LiveMcpServerIndex::new()),
            mcp_oauth: Arc::new(aisix_mcp::UserOAuthManager::new(Arc::new(
                aisix_mcp::MemoryStore::default(),
            ))),
            budgets: Arc::new(crate::budget::LiveBudgetIndex::default()),
            health: Arc::new(HealthTracker::new()),
            livez: Arc::new(LivezState::new()),
            config_apply_age: Some(Arc::new(|| None)),
            runtime_status: Arc::new(ModelRuntimeStatusTracker::new()),
            usage_sink: UsageSink::disabled(),
            otlp_fan_out,
            request_body_limit_bytes: cfg.request_body_limit_bytes,
            real_ip: Arc::new(ResolvedRealIp::from_config(&cfg.real_ip)),
            request_id_accept: cfg
                .request_id
                .parse_accept_headers()
                .unwrap_or_default()
                .into(),
            url_rewrites: crate::rewrite::compile(&cfg.url_rewrites),
            expose_debug_headers: cfg.expose_debug_headers,
            billed_batches: Arc::new(dashmap::DashMap::new()),
            batch_attribution_permits: Arc::new(tokio::sync::Semaphore::new(
                crate::jobs::BATCH_ATTRIBUTION_DOWNLOADS,
            )),
            batch_watch_interval: DEFAULT_BATCH_WATCH_INTERVAL,
            client_classifier: Arc::new(ClientTypeClassifier::builtin()),
            default_retries: aisix_core::config::DEFAULT_UPSTREAM_RETRIES,
            default_timeouts: crate::routing::TimeoutDefaults::default(),
            platform: None,
        })
    }

    /// Full constructor used by the server bootstrap — lets the same
    /// Metrics handle be shared with the admin `/metrics` endpoint and
    /// lets the caller supply the configured cache backends.
    pub fn with_components(
        snapshot: SnapshotHandle<AisixSnapshot>,
        hub: Arc<Hub>,
        limiter: Arc<Limiter>,
        metrics: Arc<Metrics>,
        cache: Option<CacheBackends>,
        cfg: &ProxyConfig,
    ) -> Self {
        let semantic_cache = Arc::new(crate::semantic::SemanticVectorCache::default());
        let guardrail_index = LiveGuardrailIndex::new_with_sink(
            snapshot.clone(),
            None,
            Some(metrics.clone()),
            guardrail_embedder_slot(&hub, &snapshot, &semantic_cache),
        );
        let otlp_fan_out = live_exporter_fan_out(&snapshot, (*metrics).clone());
        // The bootstrap constructor is the one place the tracker gets a
        // metrics sink + snapshot handle, so cooldown transitions emit
        // `aisix_deployment_*`. Clone both before they are moved into the
        // struct below. Both trackers consult one shared BookkeepingFlags
        // so the "does any configured consumer read this?" answer can't
        // drift between them.
        let bookkeeping_flags = crate::health::BookkeepingFlags::new(snapshot.clone());
        let runtime_status = Arc::new(ModelRuntimeStatusTracker::with_observability(
            metrics.clone(),
            snapshot.clone(),
            Arc::clone(&bookkeeping_flags),
        ));
        let health = Arc::new(HealthTracker::with_flags(bookkeeping_flags));
        let routing = Arc::new(RoutingRegistry::new());
        // Bootstrap only: the lightweight constructors publish no config and
        // are dropped with their process, so nothing accrues there.
        reconcile_runtime_state(
            &snapshot,
            Arc::clone(&health),
            Arc::clone(&runtime_status),
            Arc::clone(&routing),
        );
        Self::from_inner(ProxyStateInner {
            snapshot,
            hub,
            limiter,
            policy_index: Arc::new(crate::policy_index::LivePolicyIndex::default()),
            jwt_bindings: Arc::new(crate::jwt_index::LiveJwtBindings::default()),
            metrics,
            cache,
            routing,
            semantic_cache,
            guardrail_index,
            pricing: Arc::new(LivePricingIndex::new()),
            mcp_servers: Arc::new(LiveMcpServerIndex::new()),
            mcp_oauth: Arc::new(aisix_mcp::UserOAuthManager::new(Arc::new(
                aisix_mcp::MemoryStore::default(),
            ))),
            budgets: Arc::new(crate::budget::LiveBudgetIndex::default()),
            health,
            livez: Arc::new(LivezState::new()),
            config_apply_age: Some(Arc::new(|| None)),
            runtime_status,
            usage_sink: UsageSink::disabled(),
            otlp_fan_out,
            request_body_limit_bytes: cfg.request_body_limit_bytes,
            real_ip: Arc::new(ResolvedRealIp::from_config(&cfg.real_ip)),
            request_id_accept: cfg
                .request_id
                .parse_accept_headers()
                .unwrap_or_default()
                .into(),
            url_rewrites: crate::rewrite::compile(&cfg.url_rewrites),
            expose_debug_headers: cfg.expose_debug_headers,
            billed_batches: Arc::new(dashmap::DashMap::new()),
            batch_attribution_permits: Arc::new(tokio::sync::Semaphore::new(
                crate::jobs::BATCH_ATTRIBUTION_DOWNLOADS,
            )),
            batch_watch_interval: DEFAULT_BATCH_WATCH_INTERVAL,
            client_classifier: Arc::new(ClientTypeClassifier::builtin()),
            default_retries: aisix_core::config::DEFAULT_UPSTREAM_RETRIES,
            default_timeouts: crate::routing::TimeoutDefaults::default(),
            platform: None,
        })
    }

    /// Platform mode settings, `None` when platform mode is off.
    pub fn platform(&self) -> Option<&crate::platform::PlatformSettings> {
        self.platform.as_deref()
    }

    /// Install the platform mode settings built at startup.
    pub fn with_platform(mut self, settings: crate::platform::PlatformSettings) -> Self {
        self.platform = Some(Arc::new(settings));
        self
    }

    fn from_inner(inner: ProxyStateInner) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Disable caching on an existing state. Used by tests that need
    /// every request to reach wiremock.
    pub fn without_cache(mut self) -> Self {
        Arc::make_mut(&mut self.inner).cache = None;
        self
    }

    /// Replace the guardrail index. Used by the server bootstrap to
    /// wire a live snapshot-backed index; tests can substitute a
    /// deterministic one via `LiveGuardrailIndex::new(stub_handle, None)`.
    pub fn with_guardrail_index(mut self, index: Arc<LiveGuardrailIndex>) -> Self {
        Arc::make_mut(&mut self.inner).guardrail_index = index;
        self
    }

    /// Swap in the classifier compiled from
    /// `observability.metrics.client_type_rules` (#1045).
    /// Default is built-ins only.
    pub fn with_client_classifier(mut self, classifier: Arc<ClientTypeClassifier>) -> Self {
        Arc::make_mut(&mut self.inner).client_classifier = classifier;
        self
    }

    /// Apply the deployment-wide `upstream.retries` budget. Default is
    /// [`aisix_core::config::DEFAULT_UPSTREAM_RETRIES`].
    pub fn with_default_retries(mut self, retries: u32) -> Self {
        Arc::make_mut(&mut self.inner).default_retries = retries;
        self
    }

    /// Apply the deployment-wide `upstream.timeout_ms` /
    /// `upstream.stream_timeout_ms` defaults, with `0` meaning "no
    /// default at that slot".
    pub fn with_default_timeouts(mut self, timeout_ms: u64, stream_timeout_ms: u64) -> Self {
        Arc::make_mut(&mut self.inner).default_timeouts = crate::routing::TimeoutDefaults {
            request: (timeout_ms > 0).then(|| std::time::Duration::from_millis(timeout_ms)),
            stream: (stream_timeout_ms > 0)
                .then(|| std::time::Duration::from_millis(stream_timeout_ms)),
        };
        self
    }

    /// Keep per-user MCP tokens in `store` rather than in this process.
    pub fn with_mcp_oauth_store(mut self, store: Arc<dyn aisix_mcp::SharedStore>) -> Self {
        Arc::make_mut(&mut self.inner).mcp_oauth =
            Arc::new(aisix_mcp::UserOAuthManager::new(store));
        self
    }

    /// Attach a CP-side usage telemetry sink. Default is `disabled()`;
    /// the server bootstrap calls this in managed mode after spawning
    /// the sender worker.
    pub fn with_usage_sink(mut self, sink: UsageSink) -> Self {
        Arc::make_mut(&mut self.inner).usage_sink = sink;
        self
    }
    /// Poll a created batch this often instead of once a minute.
    #[cfg(test)]
    pub(crate) fn with_batch_watch_interval(mut self, every: std::time::Duration) -> Self {
        Arc::make_mut(&mut self.inner).batch_watch_interval = every;
        self
    }
    /// Wire the config-freshness probe used by `GET /readyz` (#591). The
    /// closure returns the time since the etcd watch last applied config.
    pub fn with_config_apply_age(
        mut self,
        probe: Arc<dyn Fn() -> Option<std::time::Duration> + Send + Sync>,
    ) -> Self {
        Arc::make_mut(&mut self.inner).config_apply_age = Some(probe);
        self
    }

    /// Explicit opt-out for tests or embedded callers with no asynchronous
    /// config source. Production bootstrap should wire the real source probe.
    pub fn with_config_always_fresh(self) -> Self {
        self.with_config_apply_age(Arc::new(|| Some(std::time::Duration::ZERO)))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn platform_is_none_until_installed() {
        use super::*;
        let cfg: ProxyConfig = serde_json::from_str(r#"{"addr": "127.0.0.1:0"}"#).unwrap();
        let state = ProxyState::new(
            SnapshotHandle::new(AisixSnapshot::default()),
            Arc::new(Hub::new()),
            &cfg,
        );
        assert!(state.platform().is_none());
        let state = state.with_platform(crate::platform::PlatformSettings::from_secret("abc"));
        assert!(state.platform().is_some());
    }

    /// #720's dedup set is an optimization — the deterministic
    /// `request_id = "batch-<id>"` is what actually makes re-emission
    /// idempotent — so it must not grow for the life of the process. Entries
    /// past the dedup window are reclaimed by the sweep; a re-poll inside the
    /// window is still suppressed, and one past it attributes again.
    #[test]
    fn billed_batch_dedup_is_bounded_by_its_window() {
        let seen = dashmap::DashMap::new();
        let ttl = std::time::Duration::from_secs(3600);
        let t0 = std::time::Instant::now();

        assert!(
            remember_billed_batch(&seen, "batch-a", t0, ttl),
            "first sighting attributes"
        );
        assert!(
            !remember_billed_batch(&seen, "batch-a", t0, ttl),
            "a re-poll inside the window is suppressed"
        );
        assert!(remember_billed_batch(&seen, "batch-c", t0, ttl));

        let later = t0 + ttl + std::time::Duration::from_secs(1);
        assert!(
            remember_billed_batch(&seen, "batch-a", later, ttl),
            "a sighting past the window attributes again"
        );
        assert!(
            !remember_billed_batch(&seen, "batch-a", later, ttl),
            "and the refreshed entry suppresses the next one"
        );
        assert!(
            remember_billed_batch(&seen, "batch-b", later, ttl),
            "an unrelated batch still records"
        );
        // A listing re-observes every batch on its page from the request
        // path: a sighting touches its own entry and never walks the rest.
        assert!(
            seen.contains_key("batch-c"),
            "a sighting of another batch swept the map on the request path"
        );

        sweep_billed_batches(&seen, later, ttl);
        assert!(
            !seen.contains_key("batch-c"),
            "an entry past its window must be reclaimed, not kept forever"
        );
        assert!(seen.contains_key("batch-a") && seen.contains_key("batch-b"));
    }

    // The re-arm is the only direction of this latch that can cause
    // SILENCE — a latch stuck set means the NEXT outage reports at debug
    // and nobody hears about it — and it has already been got wrong once
    // (a semantic HIT did not re-arm). Without this test, deleting the
    // re-arm leaves every other test green.
    #[test]
    fn a_second_outage_is_reported_again() {
        let b = super::CacheBackends::memory_only();
        assert!(b.note_exact_failure(), "the first failure reports");
        assert!(!b.note_exact_failure(), "the rest of the outage is quiet");
        b.note_exact_success();
        assert!(b.note_exact_failure(), "a later outage must report again");
    }

    // The two halves fail and recover independently — one connection is
    // exact-KV and the other is vector search — so neither latch may
    // speak for the other.
    #[test]
    fn the_two_cache_halves_latch_independently() {
        let b = super::CacheBackends::memory_only();
        assert!(b.note_exact_failure());
        assert!(
            b.note_semantic_failure(),
            "the exact half's outage must not silence the semantic one"
        );
        b.note_exact_success();
        assert!(
            !b.note_semantic_failure(),
            "and recovering the exact half must not re-arm the semantic one"
        );
    }
    use super::{remember_billed_batch, sweep_billed_batches, ProxyState};
    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ProxyConfig};
    use aisix_gateway::Hub;
    use aisix_obs::UsageEvent;
    use std::sync::Arc;

    fn test_state_with_limit(request_body_limit_bytes: usize) -> ProxyState {
        ProxyState::new(
            SnapshotHandle::new(AisixSnapshot::new()),
            Arc::new(Hub::new()),
            &ProxyConfig {
                addr: "127.0.0.1:0".into(),
                request_body_limit_bytes,
                tls: None,
                listeners: Vec::new(),
                real_ip: Default::default(),
                request_id: Default::default(),
                thread_per_core: None,
                workers: None,
                url_rewrites: Vec::new(),
                expose_debug_headers: false,
                platform: Default::default(),
            },
        )
    }

    fn test_state() -> ProxyState {
        test_state_with_limit(1_048_576)
    }

    #[test]
    fn config_freshness_defaults_fail_closed_and_opt_out_is_explicit() {
        let state = test_state();
        let probe = state
            .config_apply_age
            .as_ref()
            .expect("constructors must install a config probe");
        assert_eq!(probe(), None);

        let always_fresh = state.with_config_always_fresh();
        assert_eq!(
            always_fresh
                .config_apply_age
                .as_ref()
                .expect("explicit opt-out must keep a probe")(),
            Some(std::time::Duration::ZERO)
        );
    }

    #[test]
    fn omitted_body_limit_preserves_unlimited_compatibility() {
        let omitted: ProxyConfig =
            serde_json::from_value(serde_json::json!({"addr": "127.0.0.1:0"})).unwrap();
        let omitted = ProxyState::new(
            SnapshotHandle::new(AisixSnapshot::new()),
            Arc::new(Hub::new()),
            &omitted,
        );
        assert_eq!(omitted.request_body_limit_for("/v1/messages"), 0);
        assert_eq!(omitted.request_body_limit_for("/v1/files"), 0);

        let explicit = test_state_with_limit(7);
        assert_eq!(explicit.request_body_limit_for("/v1/files"), 7);
        assert_eq!(explicit.request_body_limit_for("/v1/messages"), 7);
        let unlimited = test_state_with_limit(0);
        assert_eq!(unlimited.request_body_limit_for("/v1/messages"), 0);
    }

    #[test]
    fn proxy_state_clone_shares_one_inner_and_mutation_is_copy_on_write() {
        assert_eq!(
            std::mem::size_of::<ProxyState>(),
            std::mem::size_of::<Arc<()>>()
        );

        let original = test_state();
        assert_eq!(Arc::strong_count(&original.inner), 1);

        let mut cloned = original.clone();
        assert!(Arc::ptr_eq(&original.inner, &cloned.inner));
        assert_eq!(Arc::strong_count(&original.inner), 2);

        cloned.cache = None;
        assert!(!Arc::ptr_eq(&original.inner, &cloned.inner));
        assert!(original.cache.is_some());
        assert!(cloned.cache.is_none());

        let configured = original.clone().with_default_retries(99);
        assert!(!Arc::ptr_eq(&original.inner, &configured.inner));
        assert_ne!(original.default_retries, 99);
        assert_eq!(configured.default_retries, 99);
    }

    #[tokio::test]
    async fn snapshot_publication_revokes_exporter_without_request_traffic() {
        let initial = AisixSnapshot::new();
        let exporter = serde_json::from_value(serde_json::json!({
            "name": "revoked",
            "enabled": true,
            "kind": "otlp_http",
            "endpoint": "http://127.0.0.1:9/v1/traces"
        }))
        .unwrap();
        initial
            .observability_exporters
            .insert(ResourceEntry::new("exporter-id", exporter, 1));
        let handle = SnapshotHandle::new(initial);
        let state = ProxyState::new(
            handle.clone(),
            Arc::new(Hub::new()),
            &ProxyConfig {
                addr: "127.0.0.1:0".into(),
                request_body_limit_bytes: 0,
                tls: None,
                listeners: Vec::new(),
                real_ip: Default::default(),
                request_id: Default::default(),
                thread_per_core: None,
                workers: None,
                url_rewrites: Vec::new(),
                expose_debug_headers: false,
                platform: Default::default(),
            },
        );
        let view = handle.load_versioned();
        let exporters = view.snapshot.observability_exporters.entries();
        state.otlp_fan_out.fan_out(
            &UsageEvent::default(),
            None,
            None,
            None,
            view.version,
            exporters.iter().map(|entry| &*entry.value),
        );
        assert!(state.otlp_fan_out.exporter_stats().contains_key("revoked"));

        // No request/fan_out follows this publication. The snapshot listener
        // itself must synchronously abort the worker and discard its queue.
        handle.store(AisixSnapshot::new());
        assert!(state.otlp_fan_out.exporter_stats().is_empty());

        // A request may have captured the old view before the deletion and
        // finish afterward. Once the new generation is visible, the
        // pre-publication reconciliation watermark rejects that stale fanout
        // instead of recreating the revoked pipeline.
        state.otlp_fan_out.fan_out(
            &UsageEvent::default(),
            None,
            None,
            None,
            view.version,
            exporters.iter().map(|entry| &*entry.value),
        );
        assert!(state.otlp_fan_out.exporter_stats().is_empty());
        state.otlp_fan_out.shutdown().await;
    }
}
