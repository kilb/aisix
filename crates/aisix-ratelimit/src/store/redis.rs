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
//! - `aisix:rl:{<bucket>}:conc` — a ZSET (`member → score=server_time_ms`)
//!   acting as a
//!   crash-safe distributed semaphore: acquire prunes entries older than
//!   `conc_ttl` then counts, so a slot leaked by a crashed/hung replica is
//!   reclaimed within `conc_ttl`. (LiteLLM's latest tracks parallel
//!   requests as a window-TTL counter; we use a ZSET with a request-
//!   lifetime ttl because our streaming requests can outlive a 60s window
//!   — the same reason `StreamConcurrencyGuard`/#450 exists.)
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
use std::time::Duration;

use aisix_core::{RateLimit, RedisConnConfig};
use aisix_redis::RedisConn;
use async_trait::async_trait;
use redis::Script;

use super::{local::LocalStore, token_dims, Dim, RateStore};
use crate::error::RateLimitError;
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
/// Post-stream accounting runs from synchronous stream-finalization callbacks.
/// Bound Redis work so a half-open server cannot pin the worker's own runtime.
const POST_STREAM_REDIS_TIMEOUT: Duration = Duration::from_secs(1);
/// Depth of the post-stream accounting queue.
///
/// Bounded on purpose. Token accounting is after-the-fact billing and must
/// never back-pressure a request, so the serving path offers an update and
/// returns; if Redis is wedged long enough to fill this queue the update is
/// shed with a warning rather than stalling the stream-completion callback —
/// which runs on the serving thread, and in thread-per-core mode that thread
/// is the whole core's event loop.
const POST_STREAM_QUEUE_DEPTH: usize = 8192;
/// How long `connect` waits for the accounting worker to report its Redis
/// connection. Boot-time only — no request path ever waits on this.
const POST_STREAM_WORKER_READY_TIMEOUT: Duration = Duration::from_millis(1_250);

// Result codes returned by the acquire script's first element.
const CODE_OK: i64 = 0;
const CODE_CONCURRENCY: i64 = 1;
const CODE_TOKENS: i64 = 2;
const CODE_REQUESTS: i64 = 3;

/// Atomic per-bucket acquire: concurrency gate + token check-only +
/// request check-and-increment, all-or-nothing. See module docs for the
/// key layout. Returns `{code, retry_after}`.
const ACQUIRE_LUA: &str = r#"
local prefix = ARGV[1]
local member = ARGV[2]
local conc_max = tonumber(ARGV[3])
local conc_ttl = tonumber(ARGV[4])
local grace = tonumber(ARGV[5])
local t = redis.call('TIME')
local now = tonumber(t[1])
local now_ms = now * 1000 + math.floor(tonumber(t[2]) / 1000)

local conc_key = prefix .. ':conc'
if conc_max >= 0 then
  redis.call('ZREMRANGEBYSCORE', conc_key, 0, now_ms - conc_ttl * 1000)
  if redis.call('ZCARD', conc_key) >= conc_max then
    return {1, 0}
  end
end

local idx = 6
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
  if cur >= limit then
    local retry = window - (now - ws); if retry < 1 then retry = 1 end
    return {2, retry}
  end
end

for i = 1, nreq do
  local name, window, limit = req[i][1], req[i][2], req[i][3]
  local ws = now - (now % window)
  local cur = tonumber(redis.call('GET', prefix .. ':' .. name .. ':' .. ws) or '0')
  if cur + 1 > limit then
    local retry = window - (now - ws); if retry < 1 then retry = 1 end
    return {3, retry}
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
  redis.call('ZADD', conc_key, now_ms, member)
  redis.call('EXPIRE', conc_key, conc_ttl)
end
return {0, 0}
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

/// Refresh an existing concurrency member without recreating one that a
/// racing completion already released. ARGV: prefix, member, conc_ttl.
const RENEW_CONCURRENCY_LUA: &str = r#"
local prefix = ARGV[1]
local member = ARGV[2]
local conc_ttl = tonumber(ARGV[3])
local conc_key = prefix .. ':conc'
if not redis.call('ZSCORE', conc_key, member) then
  return 0
end
local t = redis.call('TIME')
local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
-- XX is a second, command-level guarantee that a future refactor cannot
-- recreate a member already removed by commit/release. The enclosing Lua
-- script is atomic, so the ZSCORE result cannot change before this update.
redis.call('ZADD', conc_key, 'XX', now_ms, member)
redis.call('EXPIRE', conc_key, conc_ttl)
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
local now_ms = now * 1000 + math.floor(tonumber(t[2]) / 1000)
local ws = now - (now % 60)
local rpm = tonumber(redis.call('GET', prefix .. ':rpm:' .. ws) or '0')
local tpm = tonumber(redis.call('GET', prefix .. ':tpm:' .. ws) or '0')
redis.call('ZREMRANGEBYSCORE', prefix .. ':conc', 0, now_ms - conc_ttl * 1000)
local inflight = redis.call('ZCARD', prefix .. ':conc')
return {rpm, tpm, inflight, 60 - (now % 60)}
"#;

pub struct RedisStore {
    conn: RedisConn,
    post_stream_tx: tokio::sync::mpsc::Sender<PostStreamTokenAdd>,
    prefix: String,
    conc_ttl: u64,
    grace: u64,
    /// Per-process fallback used when Redis is unreachable (fail-open).
    local: Arc<LocalStore>,
    /// One-shot guard so the degradation warning is logged once, not per
    /// request, while Redis stays down.
    degraded_logged: AtomicBool,
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
        let post_stream_tx = spawn_post_stream_worker(cfg.clone(), DEFAULT_GRACE_SECS)?;
        Ok(Self {
            conn,
            post_stream_tx,
            prefix: DEFAULT_PREFIX.into(),
            conc_ttl: DEFAULT_CONC_TTL_SECS,
            grace: DEFAULT_GRACE_SECS,
            local: Arc::new(LocalStore::new()),
            degraded_logged: AtomicBool::new(false),
            shed_logged: AtomicBool::new(false),
            metrics: None,
        })
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

    /// Log the fail-open transition once per outage.
    /// Report shed post-stream updates. Deliberately not an error on the
    /// request path: billing is decoupled, so a wedged Redis costs accuracy
    /// on this replica's shared counters, never request latency. Logged once
    /// per outage so it cannot be mistaken for silence.
    fn note_shed_post_stream(&self, dropped: usize) {
        if !self.shed_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "aisix::ratelimit",
                dropped,
                queue_depth = POST_STREAM_QUEUE_DEPTH,
                "post-stream token accounting queue full; shedding updates rather than \
                 delaying requests (shared token counters undercount until it drains)"
            );
        }
    }

    /// Publish this store's degradations to `metrics`. Wired by the server
    /// bootstrap; absent in tests.
    pub fn with_metrics(mut self, metrics: Arc<dyn aisix_core::RateLimitMetricsSink>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    fn warn_degraded(&self, op: &str, err: &redis::RedisError) {
        // Counted on every failure, not once per outage: the warn below is
        // deduplicated for log volume, but a rate needs every sample.
        if let Some(m) = &self.metrics {
            m.record_redis_failure(op);
        }
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

fn spawn_post_stream_worker(
    cfg: RedisConnConfig,
    grace: u64,
) -> Result<tokio::sync::mpsc::Sender<PostStreamTokenAdd>, redis::RedisError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| post_stream_worker_error(err.to_string()))?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PostStreamTokenAdd>(POST_STREAM_QUEUE_DEPTH);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("aisix-ratelimit-commit".to_string())
        .spawn(move || {
            runtime.block_on(async move {
                let conn = match tokio::time::timeout(
                    POST_STREAM_REDIS_TIMEOUT,
                    aisix_redis::connect(&cfg),
                )
                .await
                {
                    Ok(Ok(conn)) => {
                        let _ = ready_tx.send(Ok(()));
                        conn
                    }
                    Ok(Err(err)) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                    Err(_) => {
                        let _ = ready_tx.send(Err(post_stream_worker_error(
                            "Redis post-stream worker connection timed out",
                        )));
                        return;
                    }
                };
                while let Some(command) = rx.recv().await {
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let result = match tokio::time::timeout(
                            POST_STREAM_REDIS_TIMEOUT,
                            add_tokens_remote(&conn, &command.prefix, command.tokens, grace),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => {
                                conn.note_error().await;
                                Err(post_stream_worker_error(
                                    "Redis post-stream token update timed out",
                                ))
                            }
                        };
                        // Nobody is waiting on this: report it here or it is
                        // invisible.
                        if let Err(err) = result {
                            tracing::warn!(
                                target: "aisix::ratelimit",
                                error = %err,
                                "post-stream token accounting failed; the local counter still \
                                 reflects it but this replica's Redis total does not"
                            );
                        }
                    });
                }
            });
        })
        .map_err(|err| post_stream_worker_error(err.to_string()))?;
    match ready_rx
        .recv_timeout(POST_STREAM_WORKER_READY_TIMEOUT)
        .unwrap_or_else(|err| Err(post_stream_worker_error(err.to_string())))
    {
        Ok(()) => Ok(tx),
        Err(err) => Err(err),
    }
}

async fn add_tokens_remote(
    conn: &RedisConn,
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
                self.warn_degraded("acquire", &e);
                return self.local.acquire(key, limits, member).await;
            }
        };
        match invocation.invoke_async::<Vec<i64>>(&mut conn).await {
            Ok(reply) => {
                self.mark_ok();
                let code = reply.first().copied().unwrap_or(CODE_OK);
                let retry = reply.get(1).copied().unwrap_or(0).max(0) as u64;
                match code {
                    CODE_OK => Ok(()),
                    CODE_CONCURRENCY => Err(RateLimitError::Concurrency),
                    CODE_TOKENS => Err(RateLimitError::Tokens {
                        scope: aisix_core::RateLimitScope::Tokens,
                        retry_after_secs: retry,
                    }),
                    CODE_REQUESTS => Err(RateLimitError::Requests {
                        scope: aisix_core::RateLimitScope::Requests,
                        retry_after_secs: retry,
                    }),
                    _ => Ok(()),
                }
            }
            Err(e) => {
                self.warn_degraded("acquire", &e);
                self.conn.note_error().await;
                self.local.acquire(key, limits, member).await
            }
        }
    }

    async fn commit(&self, key: &str, tokens: u64, member: &str) {
        let prefix = self.bucket_prefix(key);
        let mut conn = match self.conn.acquire().await {
            Ok(c) => c,
            Err(e) => {
                self.warn_degraded("commit", &e);
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
            Ok(_) => self.mark_ok(),
            Err(e) => {
                self.warn_degraded("commit", &e);
                self.conn.note_error().await;
                self.local.commit(key, tokens, member).await;
            }
        }
    }

    fn concurrency_lease_renewal_interval(&self) -> Option<Duration> {
        let millis = self.conc_ttl.saturating_mul(1_000) / 3;
        Some(Duration::from_millis(millis.max(100)))
    }

    async fn renew_concurrency_lease(&self, key: &str, member: &str) {
        let prefix = self.bucket_prefix(key);
        let mut conn = match self.conn.acquire().await {
            Ok(c) => c,
            Err(e) => {
                self.warn_degraded("renew_concurrency_lease", &e);
                return;
            }
        };
        let res: Result<i64, redis::RedisError> = Script::new(RENEW_CONCURRENCY_LUA)
            .key(&prefix)
            .arg(&prefix)
            .arg(member)
            .arg(self.conc_ttl)
            .invoke_async(&mut conn)
            .await;
        match res {
            Ok(_) => self.mark_ok(),
            Err(e) => {
                self.warn_degraded("renew_concurrency_lease", &e);
                self.conn.note_error().await;
            }
        }
    }

    fn release(&self, key: &str, member: &str) {
        // Drop the local slot first (a cheap no-op when the bucket was
        // never acquired locally); covers the degraded-acquire case.
        self.local.release(key, member);
        let conc_key = format!("{}:conc", self.bucket_prefix(key));
        let conn = self.conn.clone();
        let member = member.to_string();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // ZREM's first arg is the key, so Redis Cluster routes it.
                let Ok(mut c) = conn.acquire().await else {
                    return;
                };
                let res: Result<(), redis::RedisError> = redis::cmd("ZREM")
                    .arg(&conc_key)
                    .arg(&member)
                    .query_async(&mut c)
                    .await;
                if res.is_err() {
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

    async fn peek(&self, key: &str, limits: &RateLimit) -> Option<RateLimitStatus> {
        if limits.is_unrestricted() {
            return None;
        }
        let prefix = self.bucket_prefix(key);
        let mut conn = match self.conn.acquire().await {
            Ok(c) => c,
            Err(e) => {
                self.warn_degraded("peek", &e);
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
                self.warn_degraded("peek", &e);
                self.conn.note_error().await;
                self.local.peek(key, limits).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(prefix: &str) -> PostStreamTokenAdd {
        PostStreamTokenAdd {
            prefix: prefix.to_string(),
            tokens: 10,
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

    #[test]
    fn offering_post_stream_work_sheds_when_the_worker_is_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        drop(rx);
        assert_eq!(offer_post_stream(&tx, add("a")), Enqueued::Dropped);
    }
}
