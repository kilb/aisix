//! Redis-backed shared counter store — the fix for api7/#798.
//!
//! Every DP replica points at the same Redis, so one global window is
//! enforced across the cluster instead of one-per-replica. The counter
//! math mirrors [`super::local::LocalStore`] / [`crate::window`] exactly
//! (wall-clock-aligned fixed windows: `window_start = now - now % window`),
//! so swapping `memory ↔ redis` doesn't change observable limits — only
//! whether the count is shared.
//!
//! Key layout (hash-tagged so every key for a bucket shares one Redis
//! Cluster slot, keeping the per-bucket Lua atomic):
//! - `aisix:rl:{<bucket>}:<rps|rpm|rph|rpd|tpm|tph|tpd>:<window_start>` — plain
//!   `INCR`/`GET` counters, `EXPIRE = window + grace`.
//! - `aisix:rl:{<bucket>}:conc` — a ZSET (`member → last-refresh time`) acting
//!   as a crash-safe distributed semaphore: acquire prunes stale entries then
//!   counts, so a slot leaked by a crashed/hung replica is reclaimed after its
//!   `conc_ttl` (with at most one extra second for rolling-upgrade safety).
//!   Live streams renew their member while their `StreamConcurrencyGuard`
//!   exists, so a deliberately long SSE response is not mistaken for a
//!   crashed replica.
//!
//! `now` is read from `redis.call('TIME')` inside every script so window
//! boundaries are identical across replicas regardless of host clock skew.
//! Redis returns seconds plus microseconds, which the concurrency scripts
//! retain at millisecond precision:
//! <https://redis.io/docs/latest/commands/time/>. Expiry pruning deliberately
//! uses the inclusive deadline semantics of `ZREMRANGEBYSCORE`:
//! <https://redis.io/docs/latest/commands/zremrangebyscore/>. Renewal uses
//! `ZADD XX`, which updates an existing member but never creates one:
//! <https://redis.io/docs/latest/commands/zadd/>.
//!
//! On any Redis error the store fails **open** to a per-process
//! [`LocalStore`] (logged once): traffic keeps flowing with per-replica
//! enforcement during an outage instead of being blocked. Counts may
//! diverge from Redis until it recovers — availability over strict global
//! enforcement, matching the cache's "Redis error → proceed" stance.
//!
//! The connection is the shared [`aisix_redis::RedisConn`], so `single`,
//! `cluster`, and `sentinel` topologies all work. Because every script
//! touches several keys, each `EVAL` declares the bucket key (carrying the
//! `{<bucket>}` hash tag) so Redis Cluster routes it to the slot that owns
//! every key the script reads/writes; on single/sentinel the declared key
//! is simply ignored by the script body, which still reads the prefix from
//! `ARGV[1]`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use aisix_core::{RateLimit, RedisConnConfig};
use aisix_redis::{ConnSlot, FailurePolicy};
use async_trait::async_trait;
use dashmap::DashMap;
use redis::Script;

use super::{local::LocalStore, token_dims, Dim, RateStore, StreamLeaseRefresh};
use crate::error::{LimitDetail, RateLimitError};
use crate::limiter::RateLimitStatus;

/// Default key namespace, kept distinct from `aisix:cache` so the same
/// Redis can back both the response cache and the rate limiter.
pub const DEFAULT_PREFIX: &str = "aisix:rl";
/// Default concurrency-slot lifetime ceiling. A slot not released within
/// this many seconds (crashed replica, hung upstream) is pruned from the
/// count. Generous enough to cover a long streaming response.
pub const DEFAULT_CONC_TTL_SECS: u64 = 300;
/// Extra seconds added to each window counter's TTL so a counter doesn't
/// expire a hair before its window mathematically closes.
pub const DEFAULT_GRACE_SECS: u64 = 5;
/// Depth of the post-stream accounting queue.
///
/// Bounded on purpose. Token accounting is after-the-fact billing and must
/// never back-pressure a request, so the serving path offers an update and
/// returns; if Redis is wedged long enough to fill this queue the update is
/// shed with a warning rather than stalling the stream-completion callback —
/// which runs on the serving thread, and in thread-per-core mode that thread
/// is the whole core's event loop.
const POST_STREAM_QUEUE_DEPTH: usize = 8192;
/// Updates in flight against Redis at once. Past it the worker stops taking from
/// the queue, so a slow or hung Redis backs the queue up until it sheds, which is
/// what the queue's bound and the shed metric are there for, instead of growing
/// a task per update.
const POST_STREAM_IN_FLIGHT: usize = 256;
/// How often a gateway that started without its shared backend re-states
/// that it is still counting per replica. Long enough that a Redis which
/// is merely late says nothing beyond the boot WARN, short enough that a
/// permanent misconfiguration is greppable in any retained log window.
const DEGRADED_REMINDER: std::time::Duration = std::time::Duration::from_secs(300);

// Result codes returned by the acquire script's first element.
/// Dimension name reported when the script names an index this build
/// no longer knows — only reachable if the script and the caller ever
/// disagree on the dimension list, which they cannot today (both come
/// from the same `request_dims` / `token_dims` call).
const UNKNOWN_DIMENSION: &str = "unknown";

const CODE_OK: i64 = 0;
const CODE_CONCURRENCY: i64 = 1;
const CODE_TOKENS: i64 = 2;
const CODE_REQUESTS: i64 = 3;

/// What this process knows about one live concurrency member. A Redis command
/// can fail after the server applies its Lua mutation but before the client
/// receives a reply, so "not confirmed" is not always the same as "local
/// only".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaseOwnership {
    ConfirmedRedis,
    LocalOnly,
    Unknown,
}

/// Atomic per-bucket acquire: concurrency gate + token check-only +
/// request check-and-increment, all-or-nothing. See module docs for the
/// key layout. Returns `{code, retry_after}`.
const ACQUIRE_LUA: &str = r#"
local prefix = ARGV[1]
local member = ARGV[2]
local conc_max = tonumber(ARGV[3])
local conc_ttl = tonumber(ARGV[4])
local conc_key_ttl = tonumber(ARGV[5])
local grace = tonumber(ARGV[6])
local t = redis.call('TIME')
local now = tonumber(t[1])
-- Keep the score in seconds so rolling upgrades still understand existing
-- integer-second members, but retain Redis TIME's sub-second precision for
-- a live lease.
local conc_now = now + tonumber(t[2]) / 1000000
-- Old nodes wrote integer-second scores. Prune at a whole-second boundary so
-- a new node cannot mistake an old member acquired late in that second for a
-- stale member; the safe cost is at most one extra second of retention.
local conc_prune_before = math.floor(conc_now) - conc_ttl

local conc_key = prefix .. ':conc'
if conc_max >= 0 then
  redis.call('ZREMRANGEBYSCORE', conc_key, 0, '(' .. conc_prune_before)
  -- 合并 upstream 之前本仓库的分数是毫秒（约 1.7e12），落不进上面按秒算的范围，
  -- 崩溃副本留下的旧成员就永远占着一个并发位。按毫秒再清一遍；秒制分数要到公元
  -- 5138 年才到 1e11，碰不到新格式的成员。
  redis.call('ZREMRANGEBYSCORE', conc_key, 100000000000, conc_prune_before * 1000)
  local in_flight = redis.call('ZCARD', conc_key)
  if in_flight >= conc_max then
    return {1, 0, 0, conc_max, in_flight}
  end
end

local idx = 7
local nreq = tonumber(ARGV[idx]); idx = idx + 1
local req = {}
for i = 1, nreq do
  req[i] = {ARGV[idx], tonumber(ARGV[idx+1]), tonumber(ARGV[idx+2])}
  idx = idx + 3
end
local ntok = tonumber(ARGV[idx]); idx = idx + 1
local tok = {}
for i = 1, ntok do
  tok[i] = {ARGV[idx], tonumber(ARGV[idx+1]), tonumber(ARGV[idx+2])}
  idx = idx + 3
end

for i = 1, ntok do
  local name, window, limit = tok[i][1], tok[i][2], tok[i][3]
  local ws = now - (now % window)
  local cur = tonumber(redis.call('GET', prefix .. ':' .. name .. ':' .. ws) or '0')
  -- `>=`, matching the local backend: a token window whose committed
  -- usage has reached the cap is spent, not still admissible (#950).
  if cur >= limit then
    local retry = window - (now - ws); if retry < 1 then retry = 1 end
    return {2, retry, i, limit, cur}
  end
end

for i = 1, nreq do
  local name, window, limit = req[i][1], req[i][2], req[i][3]
  local ws = now - (now % window)
  local cur = tonumber(redis.call('GET', prefix .. ':' .. name .. ':' .. ws) or '0')
  if cur + 1 > limit then
    local retry = window - (now - ws); if retry < 1 then retry = 1 end
    return {3, retry, i, limit, cur}
  end
end

for i = 1, nreq do
  local name, window = req[i][1], req[i][2]
  local ws = now - (now % window)
  local k = prefix .. ':' .. name .. ':' .. ws
  if redis.call('INCR', k) == 1 then
    redis.call('EXPIRE', k, window + grace)
  end
end
if conc_max >= 0 then
  redis.call('ZADD', conc_key, conc_now, member)
  redis.call('EXPIRE', conc_key, conc_key_ttl)
end
return {0, 0, 0, 0, 0}
"#;

/// Refresh one live streaming member's concurrency lease. The member must
/// already exist: refresh racing stream teardown must not resurrect a member
/// after `release` removed it. ARGV: prefix, member, conc_key_ttl.
const REFRESH_CONCURRENCY_LUA: &str = r#"
local prefix = ARGV[1]
local member = ARGV[2]
local conc_key_ttl = tonumber(ARGV[3])
local conc_key = prefix .. ':conc'
if not redis.call('ZSCORE', conc_key, member) then
  return 0
end
local t = redis.call('TIME')
local conc_now = tonumber(t[1]) + tonumber(t[2]) / 1000000
redis.call('ZADD', conc_key, 'XX', conc_now, member)
redis.call('EXPIRE', conc_key, conc_key_ttl)
return 1
"#;

/// Post-deduct: add `tokens` to the tpm/tph/tpd windows AND release the
/// concurrency slot held by `member`. Every token window is always
/// touched (matching the local backend); an unread counter just
/// expires. ARGV: prefix, member, tokens, grace.
const COMMIT_LUA: &str = r#"
local prefix = ARGV[1]
local member = ARGV[2]
local tokens = tonumber(ARGV[3])
local grace = tonumber(ARGV[4])
local t = redis.call('TIME')
local now = tonumber(t[1])
if tokens > 0 then
  for _, d in ipairs({{'tpm', 60}, {'tph', 3600}, {'tpd', 86400}}) do
    local ws = now - (now % d[2])
    local k = prefix .. ':' .. d[1] .. ':' .. ws
    if redis.call('INCRBY', k, tokens) == tokens then
      redis.call('EXPIRE', k, d[2] + grace)
    end
  end
end
redis.call('ZREM', prefix .. ':conc', member)
return 1
"#;

/// Post-stream token add only (no concurrency change). Touches the same
/// token windows as `COMMIT_LUA` — the two must stay in step, or a streamed
/// request would move a different set of counters than a unary one. ARGV:
/// prefix, tokens, grace.
const ADD_TOKENS_LUA: &str = r#"
local prefix = ARGV[1]
local tokens = tonumber(ARGV[2])
local grace = tonumber(ARGV[3])
local t = redis.call('TIME')
local now = tonumber(t[1])
if tokens > 0 then
  for _, d in ipairs({{'tpm', 60}, {'tph', 3600}, {'tpd', 86400}}) do
    local ws = now - (now % d[2])
    local k = prefix .. ':' .. d[1] .. ':' .. ws
    if redis.call('INCRBY', k, tokens) == tokens then
      redis.call('EXPIRE', k, d[2] + grace)
    end
  end
end
return 1
"#;

/// Read-only snapshot for headers: current-minute rpm/tpm counts +
/// pruned concurrency count + seconds-to-minute-reset. ARGV: prefix,
/// conc_ttl. Returns {rpm_used, tpm_used, in_flight, minute_reset}.
const PEEK_LUA: &str = r#"
local prefix = ARGV[1]
local conc_ttl = tonumber(ARGV[2])
local t = redis.call('TIME')
local now = tonumber(t[1])
local conc_now = now + tonumber(t[2]) / 1000000
-- See ACQUIRE_LUA: use the same conservative boundary so header reads do not
-- prune a live member while a rolling upgrade is in progress.
local conc_prune_before = math.floor(conc_now) - conc_ttl
local ws = now - (now % 60)
local rpm = tonumber(redis.call('GET', prefix .. ':rpm:' .. ws) or '0')
local tpm = tonumber(redis.call('GET', prefix .. ':tpm:' .. ws) or '0')
redis.call('ZREMRANGEBYSCORE', prefix .. ':conc', 0, '(' .. conc_prune_before)
-- See ACQUIRE_LUA: the millisecond scores written before this format.
redis.call('ZREMRANGEBYSCORE', prefix .. ':conc', 100000000000, conc_prune_before * 1000)
local inflight = redis.call('ZCARD', prefix .. ':conc')
return {rpm, tpm, inflight, 60 - (now % 60)}
"#;

pub struct RedisStore {
    conn: ConnSlot,
    post_stream_tx: tokio::sync::mpsc::Sender<PostStreamTokenAdd>,
    prefix: String,
    conc_ttl: u64,
    grace: u64,
    /// Per-process fallback used when Redis is unreachable (fail-open).
    local: Arc<LocalStore>,
    /// Per-reservation ownership provenance for stream lease refreshes.
    /// Only a `ConfirmedRedis` member may turn a healthy ZSET miss into a
    /// terminal signal; a timeout after dispatch is deliberately `Unknown`.
    lease_members: DashMap<(String, String), LeaseOwnership>,
    /// One-shot guard so the degradation warning is logged once, not per
    /// request, while Redis stays down. Shared with the boot attach task,
    /// which starts it already set (the boot WARN said it) and clears it
    /// on attach so the NEXT outage still warns even if no store
    /// operation succeeded in between.
    degraded_logged: Arc<AtomicBool>,
    shed_logged: AtomicBool,
    /// Optional observability seam. Failing open to per-replica counting is
    /// the event an operator alerts on, and until this existed the store
    /// could only log it — `aisix-ratelimit` sits below `aisix-obs`, so the
    /// metric that exists for this could never be emitted from here.
    metrics: Option<Arc<dyn aisix_core::RateLimitMetricsSink>>,
}

struct PostStreamTokenAdd {
    prefix: String,
    tokens: u64,
    /// 随命令带上：worker 在 `with_metrics` 之前就已经起来了。
    metrics: Option<Arc<dyn aisix_core::RateLimitMetricsSink>>,
}

/// What happened to one update offered to the accounting queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enqueued {
    Accepted,
    /// Queue full or worker gone. Dropped deliberately: the request path
    /// never waits on billing.
    Dropped,
}

/// Offer one post-stream update to the worker without ever blocking.
fn offer_post_stream(
    tx: &tokio::sync::mpsc::Sender<PostStreamTokenAdd>,
    command: PostStreamTokenAdd,
) -> Enqueued {
    match tx.try_send(command) {
        Ok(()) => Enqueued::Accepted,
        Err(_) => Enqueued::Dropped,
    }
}

impl std::fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStore")
            .field("prefix", &self.prefix)
            .field("conc_ttl", &self.conc_ttl)
            .finish_non_exhaustive()
    }
}

impl RedisStore {
    /// Connect using the operator's `ratelimit.redis` config. The topology
    /// (`single` / `cluster` / `sentinel`) is selected by `mode`; see
    /// [`aisix_redis::connect`].
    pub async fn connect(cfg: &RedisConnConfig) -> Result<Self, redis::RedisError> {
        let conn = aisix_redis::connect(cfg).await?;
        Self::with_slot(ConnSlot::filled(conn), Arc::new(AtomicBool::new(false)))
    }

    /// Connect for the boot path: bounded by `ratelimit.redis.timeout_secs`
    /// and never fatal.
    ///
    /// The gateway must bind its listeners whether or not Redis is up —
    /// the node-reboot case has Redis coming up *after* the gateway, and
    /// a gateway that refuses to serve until its
    /// rate-limit counters are shareable trades an availability outage for
    /// an accuracy one. So a connection that does not land inside the
    /// budget leaves the store in its ordinary degraded state (per-replica
    /// in-memory counting) and a background task attaches the shared
    /// backend the moment Redis answers. The returned error is the boot
    /// diagnostic, not a failure: it is `Some` exactly when the store
    /// started degraded.
    ///
    /// Note what this does NOT do: fall back to the `memory` backend. That
    /// would be permanent, and the operator asked for cluster-wide
    /// counting.
    ///
    /// The outer `Err` is the one failure that is still fatal: a config
    /// the driver can never use, which no amount of retrying fixes and
    /// which the operator needs told loudly rather than degraded around.
    /// A process that cannot start the accounting worker's thread ends up
    /// here too: without it spend never reaches the shared counters.
    pub async fn connect_or_attach_later(
        cfg: &RedisConnConfig,
    ) -> Result<(Self, Option<redis::RedisError>), redis::RedisError> {
        let policy = FailurePolicy::new(cfg);
        match aisix_redis::connect_bounded(cfg, &policy).await {
            Ok(conn) => Ok((
                Self::with_slot(ConnSlot::filled(conn), Arc::new(AtomicBool::new(false)))?,
                None,
            )),
            // Only configuration the process rejected by itself. A
            // server that answered and refused is degraded around like
            // an outage: it can be corrected on the server side while
            // this gateway keeps serving, and the re-attach below picks
            // that up without a restart.
            Err(e) if aisix_redis::is_boot_fatal(&e) => Err(e),
            Err(e) => {
                let slot = ConnSlot::empty();
                // Already "logged": the caller WARNs this failure itself,
                // naming the endpoint, so the first degraded request must
                // not repeat it.
                let degraded_logged = Arc::new(AtomicBool::new(true));
                let store = Self::with_slot(slot.clone(), Arc::clone(&degraded_logged))?;
                spawn_attach(slot, degraded_logged, cfg.clone(), policy);
                Ok((store, Some(e)))
            }
        }
    }

    fn with_slot(
        conn: ConnSlot,
        degraded_logged: Arc<AtomicBool>,
    ) -> Result<Self, redis::RedisError> {
        let post_stream_tx = spawn_post_stream_worker(conn.clone(), DEFAULT_GRACE_SECS)?;
        Ok(Self {
            conn,
            post_stream_tx,
            prefix: DEFAULT_PREFIX.into(),
            conc_ttl: DEFAULT_CONC_TTL_SECS,
            grace: DEFAULT_GRACE_SECS,
            local: Arc::new(LocalStore::new()),
            lease_members: DashMap::new(),
            degraded_logged,
            shed_logged: AtomicBool::new(false),
            metrics: None,
        })
    }

    /// Publish this store's degradations to `metrics`. Wired by the server
    /// bootstrap; absent in tests.
    pub fn with_metrics(mut self, metrics: Arc<dyn aisix_core::RateLimitMetricsSink>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn with_conc_ttl(mut self, secs: u64) -> Self {
        self.conc_ttl = secs.max(1);
        self
    }

    /// Namespace the prefix by the DP's environment id. Most bucket keys are
    /// globally-unique CP UUIDs (api_key / policy ids), but the model inline
    /// limit buckets on the env-local model alias (`model:<name>`), so two
    /// environments sharing one (user-provided) Redis would otherwise merge
    /// their `model:<alias>` counters. Scoping the prefix to `<prefix>:<env_id>`
    /// isolates every bucket. Empty `env_id` (standalone / v2) leaves it
    /// unchanged. The env segment sits before the `{bucket}` hash tag, so
    /// Redis Cluster slot placement (by bucket) is unaffected.
    pub fn with_env_namespace(mut self, env_id: &str) -> Self {
        if !env_id.is_empty() {
            self.prefix = format!("{}:{}", self.prefix, env_id);
        }
        self
    }

    /// `aisix:rl:{<bucket>}` — the hash tag co-locates every key for the
    /// bucket on one Redis Cluster slot.
    fn bucket_prefix(&self, key: &str) -> String {
        format!("{}:{{{}}}", self.prefix, key)
    }

    /// Report shed post-stream updates. Deliberately not an error on the
    /// request path: billing is decoupled, so a wedged Redis costs accuracy
    /// on this replica's shared counters, never request latency.
    ///
    /// The log fires once per PROCESS, not once per outage — `shed_logged` is
    /// never reset — so it marks that shedding started and says nothing about
    /// whether it is still going. The counter is the signal to alert on.
    fn note_shed_post_stream(&self, dropped: usize) {
        // 计数每次都记，日志只打第一次。持续丢弃恰恰是「outage 开始时那一
        // 条日志」最没用的场景：它说不出现在还在丢。而且这个队列自花费上限
        // 本地化之后同时载着 token 和钱——丢一笔就是有人花的钱没记进预算。
        if let Some(metrics) = &self.metrics {
            metrics.record_post_stream_shed(dropped as u64);
        }
        if !self.shed_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "aisix::ratelimit",
                dropped,
                queue_depth = POST_STREAM_QUEUE_DEPTH,
                "post-stream accounting queue full; shedding updates rather than delaying \
                 requests. Shared token AND spend counters undercount until it drains, so \
                 a spend ceiling can be overrun by the shed amount. This logs once — watch \
                 aisix_ratelimit_post_stream_shed_total for whether it is still happening."
            );
        }
    }

    /// Count the failure and log the fail-open transition once per
    /// outage. Every failed operation counts; only the first logs.
    fn note_failure(&self, op: &str, err: &redis::RedisError) {
        record_failure(&self.metrics, op);
        self.warn_degraded(op, err);
    }

    /// Log the fail-open transition once per outage.
    fn warn_degraded(&self, op: &str, err: &redis::RedisError) {
        if !self.degraded_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "aisix::ratelimit",
                op,
                error = %err,
                "shared rate-limit Redis unavailable; failing open to per-replica \
                 in-memory counting until it recovers (cluster limits not enforced \
                 during the outage)"
            );
        }
    }

    /// Mark Redis healthy again after a successful op (re-arms the warn).
    fn mark_ok(&self) {
        self.degraded_logged.store(false, Ordering::Relaxed);
        self.shed_logged.store(false, Ordering::Relaxed);
    }
}

fn post_stream_worker_error(detail: impl Into<String>) -> redis::RedisError {
    redis::RedisError::from((
        redis::ErrorKind::IoError,
        "rate-limit post-stream worker unavailable",
        detail.into(),
    ))
}

/// Start the store-owned accounting worker.
///
/// The serving path only offers to its queue; the Redis I/O runs on this
/// thread's own runtime. It works through the store's own connection slot, so
/// a backend attached after boot serves accounting and the request path in
/// one step, and one outage trips one breaker. Each command is bounded by that
/// connection's command budget.
fn spawn_post_stream_worker(
    conn: ConnSlot,
    grace: u64,
) -> Result<tokio::sync::mpsc::Sender<PostStreamTokenAdd>, redis::RedisError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| post_stream_worker_error(err.to_string()))?;
    let (tx, rx) = tokio::sync::mpsc::channel::<PostStreamTokenAdd>(POST_STREAM_QUEUE_DEPTH);
    std::thread::Builder::new()
        .name("aisix-ratelimit-commit".to_string())
        .spawn(move || {
            runtime.block_on(async move {
                // 启动时没连上的 store 每笔记账都会失败，可能持续几个小时：
                // 计数每笔都记，日志一轮故障只打一条。
                let warned = Arc::new(AtomicBool::new(false));
                drain_post_stream(rx, POST_STREAM_IN_FLIGHT, move |command| {
                    let conn = conn.clone();
                    let warned = Arc::clone(&warned);
                    async move {
                        let result =
                            add_tokens_remote(&conn, &command.prefix, command.tokens, grace).await;
                        // Nobody is waiting on this: report it here or it is
                        // invisible.
                        match result {
                            Ok(()) => warned.store(false, Ordering::Relaxed),
                            Err(err) => {
                                record_failure(&command.metrics, "add_tokens");
                                if !warned.swap(true, Ordering::Relaxed) {
                                    tracing::warn!(
                                        target: "aisix::ratelimit",
                                        error = %err,
                                        "post-stream token accounting failed; the local counter \
                                         still reflects it but this replica's Redis total does not"
                                    );
                                }
                            }
                        }
                    }
                })
                .await;
            });
        })
        .map_err(|err| post_stream_worker_error(err.to_string()))?;
    Ok(tx)
}

/// Run `work` on each queued update, at most `limit` at a time. The next update
/// stays in the queue until one finishes.
async fn drain_post_stream<F, Fut>(
    mut rx: tokio::sync::mpsc::Receiver<PostStreamTokenAdd>,
    limit: usize,
    work: F,
) where
    F: Fn(PostStreamTokenAdd) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let in_flight = Arc::new(tokio::sync::Semaphore::new(limit));
    while let Some(command) = rx.recv().await {
        // 信号量从不关闭，拿不到许可只可能是它已被丢弃。
        let Ok(permit) = Arc::clone(&in_flight).acquire_owned().await else {
            return;
        };
        let job = work(command);
        tokio::spawn(async move {
            job.await;
            drop(permit);
        });
    }
}

async fn add_tokens_remote(
    conn: &ConnSlot,
    prefix: &str,
    tokens: u64,
    grace: u64,
) -> Result<(), redis::RedisError> {
    let mut connection = conn.acquire().await?;
    let result: Result<i64, redis::RedisError> = Script::new(ADD_TOKENS_LUA)
        .key(prefix)
        .arg(prefix)
        .arg(tokens)
        .arg(grace)
        .invoke_async(&mut connection)
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(err) => {
            conn.note_error().await;
            Err(err)
        }
    }
}

/// Keep trying the shared backend until it answers, then attach it.
///
/// The cadence is the configured budget, not the breaker's cool-off
/// window: the reason the request-path prober waits a whole window is
/// that a probe against a still-dead Redis costs a caller its budget,
/// and nothing here is billed to a caller. What it buys instead is the
/// case this exists for — Redis coming up seconds after the gateway on a
/// node reboot — recovering in seconds rather than half a minute.
///
/// One attempt per iteration, and `connect_bounded` bounds each one, so
/// an unreachable Redis costs one connect attempt per sleep-plus-budget
/// for as long as the outage lasts.
///
/// A permanent misconfiguration — a wrong host, a dead Redis, an
/// unreachable subnet — used to announce itself by killing the boot. Now
/// it is a gateway with a healthy `/livez` that quietly never enforces a
/// cluster-wide limit, and the only other trace is
/// `aisix_redis_failures_total`, which moves only while rate-limited
/// traffic actually arrives. So the retry re-states it at WARN on
/// [`DEGRADED_REMINDER`], leaving the individual attempts at debug.
fn spawn_attach(
    slot: ConnSlot,
    degraded_logged: Arc<AtomicBool>,
    cfg: RedisConnConfig,
    policy: FailurePolicy,
) {
    tokio::spawn(async move {
        let retry = std::time::Duration::from_secs(cfg.timeout_secs.max(1));
        let endpoint = aisix_redis::endpoint_label(&cfg);
        let mut last_reminder = std::time::Instant::now();
        loop {
            tokio::time::sleep(retry).await;
            match aisix_redis::connect_bounded(&cfg, &policy).await {
                Ok(conn) => {
                    slot.attach(conn);
                    // Re-arm the store's one-shot degradation warning:
                    // it was set at boot so the first degraded request
                    // would not repeat the boot WARN, and a LATER outage
                    // has to be able to warn again even if no store
                    // operation succeeded in between (which is what
                    // `mark_ok` would otherwise be relied on for).
                    degraded_logged.store(false, Ordering::Relaxed);
                    tracing::info!(
                        target: "aisix::ratelimit",
                        %endpoint,
                        "shared rate-limit backend attached; cluster-wide counting resumes"
                    );
                    return;
                }
                Err(e) => {
                    if last_reminder.elapsed() >= DEGRADED_REMINDER {
                        last_reminder = std::time::Instant::now();
                        // Same `reason` the boot line carried, so the two
                        // read as one story and one filter finds both. A
                        // server that answers and refuses is not an
                        // outage, and saying "unreachable" about it sends
                        // the operator to the network.
                        let reason = aisix_redis::failure_reason(&e);
                        if reason == "refused" {
                            tracing::warn!(
                                target: "aisix::ratelimit",
                                %endpoint,
                                %reason,
                                error = %e,
                                "shared rate-limit backend is still REFUSING the configured \
                                 connection settings; cluster-wide rate limits are NOT \
                                 enforced and counting stays per-replica"
                            );
                        } else {
                            tracing::warn!(
                                target: "aisix::ratelimit",
                                %endpoint,
                                %reason,
                                error = %e,
                                "shared rate-limit backend still unreachable; cluster-wide \
                                 rate limits are NOT enforced and counting stays per-replica"
                            );
                        }
                    } else {
                        tracing::debug!(
                            target: "aisix::ratelimit",
                            %endpoint,
                            error = %e,
                            retry_secs = retry.as_secs(),
                            "shared rate-limit backend still unreachable; per-replica \
                             counting continues"
                        );
                    }
                }
            }
        }
    });
}

/// Bump `aisix_redis_failures_total` for one failed rate-limit store
/// operation. The sink is told the store call; `Metrics`' implementation of
/// it qualifies the label (`ratelimit_acquire`), because a deployment points
/// the limiter and the caches at the same Redis, and a bare `acquire` could
/// have come from either.
fn record_failure(metrics: &Option<Arc<dyn aisix_core::RateLimitMetricsSink>>, op: &str) {
    if let Some(m) = metrics {
        m.record_redis_failure(op);
    }
}

fn push_dims(args: &mut Vec<String>, dims: &[Dim]) {
    args.push(dims.len().to_string());
    for d in dims {
        args.push(d.name.to_string());
        args.push(d.window_secs.to_string());
        args.push(d.limit.to_string());
    }
}

#[async_trait]
impl RateStore for RedisStore {
    async fn acquire(
        &self,
        key: &str,
        limits: &RateLimit,
        member: &str,
    ) -> Result<(), RateLimitError> {
        let prefix = self.bucket_prefix(key);
        let mut args = vec![
            prefix,
            member.to_string(),
            limits.concurrency.map(i64::from).unwrap_or(-1).to_string(),
            self.conc_ttl.to_string(),
            // Pruning retains a pre-upgrade integer-second member through its
            // boundary, so the key must survive for the same extra second.
            self.conc_ttl.saturating_add(1).to_string(),
            self.grace.to_string(),
        ];
        push_dims(&mut args, &super::request_dims(limits));
        push_dims(&mut args, &token_dims(limits));

        let script = Script::new(ACQUIRE_LUA);
        let mut invocation = script.prepare_invoke();
        // KEYS[1] carries the {bucket} hash tag for Redis Cluster routing;
        // args[0] is the same prefix the script reads from ARGV[1].
        invocation.key(&args[0]);
        for a in &args {
            invocation.arg(a);
        }
        let mut conn = match self.conn.acquire().await {
            Ok(c) => c,
            Err(e) => {
                self.note_failure("acquire", &e);
                let acquired = self.local.acquire(key, limits, member).await;
                if acquired.is_ok() && limits.concurrency.is_some() {
                    self.lease_members.insert(
                        (key.to_string(), member.to_string()),
                        LeaseOwnership::LocalOnly,
                    );
                }
                return acquired;
            }
        };
        match invocation.invoke_async::<Vec<i64>>(&mut conn).await {
            Ok(reply) => {
                self.mark_ok();
                let code = reply.first().copied().unwrap_or(CODE_OK);
                let retry = reply.get(1).copied().unwrap_or(0).max(0) as u64;
                // The script reports which dimension refused as its
                // 1-based index into the same `request_dims` /
                // `token_dims` vectors this call pushed into ARGV, so
                // the name never has to cross the wire.
                let refused = |dims: &[super::Dim]| {
                    let idx = reply.get(2).copied().unwrap_or(0);
                    let name = usize::try_from(idx)
                        .ok()
                        .and_then(|i| i.checked_sub(1))
                        .and_then(|i| dims.get(i))
                        .map(|d| d.name)
                        .unwrap_or(UNKNOWN_DIMENSION);
                    let limit = reply.get(3).copied().unwrap_or(0).max(0) as u64;
                    let used = reply.get(4).copied().unwrap_or(0).max(0) as u64;
                    LimitDetail::window(name, limit, used, retry)
                };
                match code {
                    CODE_OK => {
                        if limits.concurrency.is_some() {
                            self.lease_members.insert(
                                (key.to_string(), member.to_string()),
                                LeaseOwnership::ConfirmedRedis,
                            );
                        }
                        Ok(())
                    }
                    CODE_CONCURRENCY => {
                        let limit = reply.get(3).copied().unwrap_or(0).max(0) as u64;
                        let in_flight = reply.get(4).copied().unwrap_or(0).max(0) as u64;
                        Err(RateLimitError::Concurrency {
                            detail: LimitDetail::concurrency(limit, in_flight),
                        })
                    }
                    CODE_TOKENS => Err(RateLimitError::Tokens {
                        scope: aisix_core::RateLimitScope::Tokens,
                        detail: refused(&token_dims(limits)),
                    }),
                    CODE_REQUESTS => Err(RateLimitError::Requests {
                        scope: aisix_core::RateLimitScope::Requests,
                        detail: refused(&super::request_dims(limits)),
                    }),
                    _ => Ok(()),
                }
            }
            Err(e) => {
                self.note_failure("acquire", &e);
                self.conn.note_error().await;
                let acquired = self.local.acquire(key, limits, member).await;
                if acquired.is_ok() && limits.concurrency.is_some() {
                    // A reply can be lost after Redis has already applied the
                    // Lua acquire. Probe on refresh rather than assuming the
                    // local fallback was the only reservation.
                    self.lease_members.insert(
                        (key.to_string(), member.to_string()),
                        LeaseOwnership::Unknown,
                    );
                }
                acquired
            }
        }
    }

    async fn commit(&self, key: &str, tokens: u64, member: &str) {
        self.lease_members
            .remove(&(key.to_string(), member.to_string()));
        let prefix = self.bucket_prefix(key);
        let mut conn = match self.conn.acquire().await {
            Ok(c) => c,
            Err(e) => {
                self.note_failure("commit", &e);
                return self.local.commit(key, tokens, member).await;
            }
        };
        let res: Result<i64, redis::RedisError> = Script::new(COMMIT_LUA)
            .key(&prefix)
            .arg(&prefix)
            .arg(member)
            .arg(tokens)
            .arg(self.grace)
            .invoke_async(&mut conn)
            .await;
        match res {
            Ok(_) => {
                self.mark_ok();
                // 与 `add_tokens_all` 一样，这笔花费也记一份进本地备份：断线时的
                // 本地降级要接着用同一份 token 窗口。只记流式的话，非流式花掉的
                // 那部分在断线期间又能花一遍。
                self.local.add_tokens(key, tokens);
                // 这个 member 可能是断线时降级到本地占的位：Redis 侧的 commit
                // 不碰本地，不在这里还掉就永远留在本地，下次断线时本地并发上限
                // 少一个。本地没有这个桶时是空操作。
                self.local.release(key, member);
            }
            Err(e) => {
                self.note_failure("commit", &e);
                self.conn.note_error().await;
                self.local.commit(key, tokens, member).await;
            }
        }
    }

    fn release(&self, key: &str, member: &str) {
        self.lease_members
            .remove(&(key.to_string(), member.to_string()));
        // Drop the local slot first (a cheap no-op when the bucket was
        // never acquired locally); covers the degraded-acquire case.
        self.local.release(key, member);
        let conc_key = format!("{}:conc", self.bucket_prefix(key));
        let conn = self.conn.clone();
        let member = member.to_string();
        let metrics = self.metrics.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // ZREM's first arg is the key, so Redis Cluster routes it.
                let mut c = match conn.acquire().await {
                    Ok(c) => c,
                    Err(_) => {
                        record_failure(&metrics, "release");
                        return;
                    }
                };
                let res: Result<(), redis::RedisError> = redis::cmd("ZREM")
                    .arg(&conc_key)
                    .arg(&member)
                    .query_async(&mut c)
                    .await;
                if res.is_err() {
                    // The detached tasks are the only Redis failures with
                    // no log at all — nothing awaits them, so the counter
                    // is their only trace.
                    record_failure(&metrics, "release");
                    conn.note_error().await;
                }
            });
        }
    }

    /// Redis expires its own buckets server-side; the local fallback map
    /// this store keeps for outages does not, so reap that.
    fn reap(&self, idle_for: std::time::Duration) {
        self.local.reap(idle_for);
    }

    fn add_tokens(&self, key: &str, tokens: u64) {
        let keys = [key.to_owned()];
        self.add_tokens_all(&keys, tokens);
    }

    fn add_tokens_all(&self, keys: &[String], tokens: u64) {
        if tokens == 0 || keys.is_empty() {
            return;
        }
        // Post-hoc billing, fully decoupled from the request: hand every
        // layer's update to the store-owned worker and return. Nothing here
        // waits on Redis, so a slow or half-open server cannot add latency to
        // the stream that just finished — nor to any other request sharing
        // this thread, which in thread-per-core mode is the whole core.
        let mut shed = 0usize;
        for key in keys {
            // Record locally too so the count is right if a later request
            // falls back during an outage.
            self.local.add_tokens(key, tokens);
            if offer_post_stream(
                &self.post_stream_tx,
                PostStreamTokenAdd {
                    prefix: self.bucket_prefix(key),
                    tokens,
                    metrics: self.metrics.clone(),
                },
            ) == Enqueued::Dropped
            {
                shed += 1;
            }
        }
        if shed > 0 {
            self.note_shed_post_stream(shed);
        }
    }

    fn stream_lease_refresh_interval(&self) -> Option<std::time::Duration> {
        let millis = self
            .conc_ttl
            .saturating_mul(1_000)
            .saturating_div(3)
            .clamp(100, 60_000);
        Some(std::time::Duration::from_millis(millis))
    }

    async fn refresh_stream_lease(&self, key: &str, member: &str) {
        let _ = self.refresh_stream_lease_outcome(key, member).await;
    }

    async fn refresh_stream_lease_outcome(&self, key: &str, member: &str) -> StreamLeaseRefresh {
        let ownership = self
            .lease_members
            .get(&(key.to_string(), member.to_string()))
            .map(|entry| *entry);
        let Some(ownership) = ownership else {
            // The member was already released, or this store never admitted
            // it. It is not a live shared lease that can be conclusively lost.
            return StreamLeaseRefresh::Unavailable;
        };
        if matches!(ownership, LeaseOwnership::LocalOnly) {
            // `conn.acquire` failed before a command could be dispatched.
            // This member exists solely in the local fail-open fallback.
            return StreamLeaseRefresh::Unavailable;
        }
        let prefix = self.bucket_prefix(key);
        let mut conn = match self.conn.acquire().await {
            Ok(c) => c,
            Err(e) => {
                self.note_failure("refresh", &e);
                // A post-dispatch I/O failure opens the shared Redis breaker.
                // While it short-circuits, Unknown stays fail-open; only a
                // later successful refresh can promote it to confirmed Redis
                // ownership.
                return StreamLeaseRefresh::Unavailable;
            }
        };
        let res: Result<i64, redis::RedisError> = Script::new(REFRESH_CONCURRENCY_LUA)
            .key(&prefix)
            .arg(&prefix)
            .arg(member)
            .arg(self.conc_ttl.saturating_add(1))
            .invoke_async(&mut conn)
            .await;
        match res {
            Ok(1) => {
                self.mark_ok();
                if matches!(ownership, LeaseOwnership::Unknown) {
                    self.lease_members.insert(
                        (key.to_string(), member.to_string()),
                        LeaseOwnership::ConfirmedRedis,
                    );
                }
                StreamLeaseRefresh::Renewed
            }
            Ok(0) => {
                // Redis did answer: the member itself is gone. This is not a
                // backend outage, so clear any prior degradation marker while
                // returning the definitive loss to the stream hold.
                self.mark_ok();
                if matches!(ownership, LeaseOwnership::ConfirmedRedis) {
                    StreamLeaseRefresh::Missing
                } else {
                    // A command may have failed before Redis ever saw it, so
                    // an Unknown member's absence is fail-open, not proof of
                    // an externally released live lease.
                    StreamLeaseRefresh::Unavailable
                }
            }
            Ok(_) => StreamLeaseRefresh::Unavailable,
            Err(e) => {
                self.note_failure("refresh", &e);
                self.conn.note_error().await;
                StreamLeaseRefresh::Unavailable
            }
        }
    }

    async fn peek(&self, key: &str, limits: &RateLimit) -> Option<RateLimitStatus> {
        if limits.is_unrestricted() {
            return None;
        }
        let prefix = self.bucket_prefix(key);
        let mut conn = match self.conn.acquire().await {
            Ok(c) => c,
            Err(e) => {
                self.note_failure("peek", &e);
                return self.local.peek(key, limits).await;
            }
        };
        let reply: Result<Vec<i64>, redis::RedisError> = Script::new(PEEK_LUA)
            .key(&prefix)
            .arg(&prefix)
            .arg(self.conc_ttl)
            .invoke_async(&mut conn)
            .await;
        match reply {
            Ok(v) => {
                self.mark_ok();
                Some(RateLimitStatus {
                    rpm_limit: limits.rpm,
                    rpm_used: v.first().copied().unwrap_or(0).max(0) as u64,
                    rpm_reset_secs: v.get(3).copied().unwrap_or(0).max(0) as u64,
                    tpm_limit: limits.tpm,
                    tpm_used: v.get(1).copied().unwrap_or(0).max(0) as u64,
                    tpm_reset_secs: v.get(3).copied().unwrap_or(0).max(0) as u64,
                    concurrency_limit: limits.concurrency,
                    in_flight: v.get(2).copied().unwrap_or(0).max(0) as u32,
                })
            }
            Err(e) => {
                self.note_failure("peek", &e);
                self.conn.note_error().await;
                self.local.peek(key, limits).await
            }
        }
    }

    fn claims_outlive_process(&self) -> bool {
        true
    }

    async fn claim_once(&self, key: &str, ttl: std::time::Duration) -> bool {
        // `SET … NX EX` 是原子的，所以并发的两个实例只有一个拿到标记。
        //
        // Redis 不可达时退回进程内的那份 —— 与这个存储其它方法同一个姿态
        // （`local` 兜底、fail-open）。退化的后果正好与退化的对象一致：那时
        // 计数器也只在本进程里，所以标记只在本进程里有效，两者仍然同寿。
        //
        // **但这里的「放行」和别处不是一个意思。** `acquire` 放行意味着可能超
        // 限；这里放行意味着可能**重复收费**（每个副本各记一次）。反过来失败
        // 时不放行则是漏记一笔 —— 而漏记还能从用量事件里补，多收不能。选择退
        // 回本地那份是取中间：单个副本在故障期间仍然只记一次，只有多副本同时
        // 跑才会各记一次。真要更保守，该做的是失败时不记并把它计成一个指标，
        // 而不是在这里静默改方向。
        let mut conn = match self.conn.acquire().await {
            Ok(c) => c,
            Err(e) => {
                self.note_failure("claim_once", &e);
                return self.local.claim_once(key, ttl).await;
            }
        };
        let marker = self.bucket_prefix(&format!("claim:{key}"));
        let res: redis::RedisResult<Option<String>> = redis::cmd("SET")
            .arg(&marker)
            .arg(1)
            .arg("NX")
            .arg("EX")
            .arg(ttl.as_secs().max(1))
            .query_async(&mut conn)
            .await;
        match res {
            Ok(reply) => {
                self.mark_ok();
                reply.is_some()
            }
            Err(e) => {
                self.note_failure("claim_once", &e);
                self.local.claim_once(key, ttl).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn add(prefix: &str) -> PostStreamTokenAdd {
        PostStreamTokenAdd {
            prefix: prefix.to_string(),
            tokens: 10,
            metrics: None,
        }
    }

    /// Billing is after-the-fact: offering an update must return at once
    /// whatever the worker or Redis is doing. A full queue sheds the update
    /// rather than making the serving thread wait for it.
    #[test]
    fn offering_post_stream_work_never_blocks_the_caller() {
        let (tx, _rx) = tokio::sync::mpsc::channel(2);
        assert_eq!(offer_post_stream(&tx, add("a")), Enqueued::Accepted);
        assert_eq!(offer_post_stream(&tx, add("b")), Enqueued::Accepted);

        let started = std::time::Instant::now();
        assert_eq!(
            offer_post_stream(&tx, add("c")),
            Enqueued::Dropped,
            "a full queue must shed, not wait"
        );
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "the request path must never wait on billing, waited {:?}",
            started.elapsed()
        );
    }

    /// With Redis hanging, updates back up in the queue until it sheds. A worker
    /// that spawned a task per update would empty the queue forever: nothing
    /// would ever shed and the tasks would pile up instead.
    #[tokio::test]
    async fn a_stuck_redis_backs_the_queue_up_until_it_sheds() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&started);
        tokio::spawn(drain_post_stream(rx, 2, move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            std::future::pending::<()>()
        }));

        let mut accepted = 0;
        let mut full = false;
        for _ in 0..64 {
            match tx.try_send(add("rate:k")) {
                Ok(()) => accepted += 1,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => full = true,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    panic!("the worker stopped")
                }
            }
            tokio::task::yield_now().await;
        }
        assert!(full, "the queue never filled, after {accepted} updates");
        assert_eq!(started.load(Ordering::Relaxed), 2);
        // 两个在跑、一个等许可、四个排队。
        assert_eq!(accepted, 7);
    }

    #[test]
    fn offering_post_stream_work_sheds_when_the_worker_is_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        drop(rx);
        assert_eq!(offer_post_stream(&tx, add("a")), Enqueued::Dropped);
    }
}

#[cfg(test)]
mod lua_invariants {
    use super::*;

    /// 一元请求走 `COMMIT_LUA`，流式请求走 `ADD_TOKENS_LUA`。两段脚本必须动到
    /// **同一组**计数器，否则同样的流量按流式还是一元发出去会记到不同的地方。
    ///
    /// 模块注释里本来就写着这条不变量，这里把它检查起来：给一边加计数器时
    /// 漏了另一边，这条会红。
    #[test]
    fn 两段计费脚本动的是同一组计数器() {
        let counters = |lua: &str| {
            let mut found: Vec<&str> = ["'tpm'", "'tph'", "'tpd'"]
                .into_iter()
                .filter(|c| lua.contains(c))
                .collect();
            found.sort_unstable();
            found
        };
        assert_eq!(
            counters(COMMIT_LUA),
            counters(ADD_TOKENS_LUA),
            "一元与流式的计费脚本动了不同的计数器",
        );
    }
}
