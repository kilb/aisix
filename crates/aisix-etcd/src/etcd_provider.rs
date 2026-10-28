//! Real [`ConfigProvider`] backed by `etcd-client`.
//!
//! Connection sequence (spec §2):
//! - Fixed-interval retry on initial connect: 5s × up to 5 attempts
//! - On success, `get` with prefix to bootstrap
//! - `watch` with `start_revision = range_revision + 1` to avoid a gap
//! - Compaction errors map to [`ProviderError::Compacted`] so the
//!   supervisor can trigger a full resync

use async_trait::async_trait;
use etcd_client::{
    Client, ConnectOptions, Error as EtcdError, EventType, GetOptions, WatchOptions,
};
use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::error::Error as StdError;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::Mutex;

/// Flatten an error and its source chain into a single readable line.
/// Without this, tonic surfaces opaque strings like "dns error" while
/// the real cause (`getaddrinfo: Name or service not known`, TLS
/// handshake reason, …) hides in `.source()`. The supervisor logs
/// the returned string, so CI triage gets the full picture.
fn format_error_chain(err: &(dyn StdError + 'static)) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(next) = cur {
        let s = next.to_string();
        if !s.is_empty() && !out.ends_with(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        cur = next.source();
    }
    out
}

use crate::provider::{ConfigProvider, ProviderError, RawEntry, WatchEvent};

/// Fixed-interval retry: 5s × 5 attempts (spec §2).
pub const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(5);
pub const CONNECT_MAX_ATTEMPTS: u32 = 5;

/// Retry policy used on the initial connect. Exposed for tests so they
/// can shrink the interval; production uses [`ConnectPolicy::default`].
#[derive(Debug, Clone, Copy)]
pub struct ConnectPolicy {
    pub interval: Duration,
    pub attempts: u32,
    /// Deadline applied to each unary request the resulting provider makes.
    /// See [`EtcdConfigProvider::request_timeout`].
    pub request_timeout: Option<Duration>,
}

impl Default for ConnectPolicy {
    fn default() -> Self {
        Self {
            interval: CONNECT_RETRY_INTERVAL,
            attempts: CONNECT_MAX_ATTEMPTS,
            request_timeout: None,
        }
    }
}

pub struct EtcdConfigProvider {
    /// The etcd client itself is `Clone`-cheap (internally Arc'd), but we
    /// still serialise access for watches through a Mutex because the
    /// underlying channel is not Sync at construction time.
    client: Mutex<Client>,
    prefix: String,
    /// Deadline for one unary request (`etcd.request_timeout_ms`).
    ///
    /// Applied HERE and not through `ConnectOptions::with_timeout`, because
    /// etcd-client turns that into `Endpoint::timeout` — a tonic
    /// channel-level deadline on every request INCLUDING the streaming
    /// watch, which would tear the config watch down on a fixed schedule.
    /// `request_timeout` describes a request; the watch is not one.
    ///
    /// `None` leaves the read unbounded, which is what every caller got
    /// before the knob was wired: `dial_timeout_ms` and `request_timeout_ms`
    /// have been accepted, defaulted and documented since the config struct
    /// existed, with no reader anywhere.
    request_timeout: Option<Duration>,
}

impl std::fmt::Debug for EtcdConfigProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EtcdConfigProvider")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl EtcdConfigProvider {
    /// Connect with the spec §2 default retry policy.
    pub async fn connect(
        endpoints: &[String],
        prefix: impl Into<String>,
        options: Option<ConnectOptions>,
    ) -> Result<Self, ProviderError> {
        Self::connect_with_policy(endpoints, prefix, options, ConnectPolicy::default()).await
    }

    /// Connect with a caller-chosen retry policy. Returns the last-seen
    /// error on failure to surface useful context in the bootstrap logs.
    pub async fn connect_with_policy(
        endpoints: &[String],
        prefix: impl Into<String>,
        options: Option<ConnectOptions>,
        policy: ConnectPolicy,
    ) -> Result<Self, ProviderError> {
        let prefix = prefix.into();
        let mut last_err: Option<EtcdError> = None;
        for attempt in 1..=policy.attempts {
            match Client::connect(endpoints, options.clone()).await {
                Ok(client) => {
                    tracing::info!(attempt, prefix = %prefix, "etcd connected");
                    return Ok(Self {
                        client: Mutex::new(client),
                        prefix,
                        request_timeout: policy.request_timeout,
                    });
                }
                Err(err) => {
                    tracing::warn!(
                        attempt,
                        max = policy.attempts,
                        error = %format_error_chain(&err),
                        "etcd connect failed — retrying",
                    );
                    last_err = Some(err);
                    if attempt < policy.attempts {
                        tokio::time::sleep(policy.interval).await;
                    }
                }
            }
        }
        Err(ProviderError::Connect(
            last_err
                .as_ref()
                .map(|e| format_error_chain(e))
                .unwrap_or_else(|| "exhausted retries".to_string()),
        ))
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

/// 全量加载每页的 key 数。每页是一次普通范围读，受 `request_timeout` 约束；
/// 配置再大也只是页数多，不会因为总量超出一次请求的预算而加载失败。
const LOAD_PAGE_SIZE: i64 = 500;

/// 全量加载在第一页之后遇到压缩时，从头重来的最多次数；用完就把压缩错误交给
/// supervisor，由它按退避再试。
const LOAD_COMPACTION_RESTARTS: u32 = 3;

/// etcd 对已压缩修订号的范围读返回的错误（`ErrGRPCCompacted`）：
/// <https://github.com/etcd-io/etcd/blob/v3.5.18/api/v3rpc/rpctypes/error.go>
const COMPACTED_REVISION: &str = "required revision has been compacted";

/// 前缀范围的结束键：最后一个小于 0xff 的字节加一，其后截断（与 etcd 客户端
/// 的 `GetPrefixRangeEnd` 相同）。全是 0xff 时取到 key 空间末尾。
fn prefix_range_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return end;
        }
    }
    vec![0]
}

#[async_trait]
impl ConfigProvider for EtcdConfigProvider {
    async fn load_all(&self) -> Result<(Vec<RawEntry>, i64), ProviderError> {
        let mut client = self.client.lock().await;
        // 分页读：`request_timeout` 约束的是一次请求，整份配置是一批数据。
        // 一次读完时，前缀一大、链路一慢就超出单次预算 —— 启动失败，watch
        // 重置后的重新同步次次超时。每页是一次普通范围读，各自受预算约束。
        let range_end = prefix_range_end(self.prefix.as_bytes());
        let mut start = self.prefix.as_bytes().to_vec();
        let mut revision = 0;
        let mut entries = Vec::new();
        let mut restarts = 0;
        loop {
            let mut options = GetOptions::new()
                .with_range(range_end.clone())
                .with_limit(LOAD_PAGE_SIZE);
            // 后续页钉在第一页的修订号上：整份快照必须来自同一个时刻。
            if revision > 0 {
                options = options.with_revision(revision);
            }
            let get = client.get(start.clone(), Some(options));
            let resp = match self.request_timeout {
                Some(d) => tokio::time::timeout(d, get).await.map_err(|_| {
                    ProviderError::Range(format!(
                        "etcd range request exceeded etcd.request_timeout_ms ({} ms)",
                        d.as_millis()
                    ))
                })?,
                None => get.await,
            };
            let resp = match resp {
                Ok(resp) => resp,
                // 第一页之后 etcd 压缩过了钉住的修订号：这个修订上的快照已经
                // 读不全了，从第一页、在新的修订号上重来。
                Err(e)
                    if restarts < LOAD_COMPACTION_RESTARTS
                        && e.to_string().contains(COMPACTED_REVISION) =>
                {
                    restarts += 1;
                    tracing::warn!(
                        revision,
                        restart = restarts,
                        max = LOAD_COMPACTION_RESTARTS,
                        "etcd compacted past the load's revision between pages — restarting the load",
                    );
                    start = self.prefix.as_bytes().to_vec();
                    revision = 0;
                    entries.clear();
                    continue;
                }
                Err(e) => return Err(ProviderError::Range(format_error_chain(&e))),
            };

            if revision == 0 {
                revision = resp.header().map(|h| h.revision()).unwrap_or(0);
            }
            entries.extend(resp.kvs().iter().map(|kv| RawEntry {
                key: String::from_utf8_lossy(kv.key()).into_owned(),
                value: kv.value().to_vec(),
                revision: kv.mod_revision(),
            }));
            match (resp.more(), resp.kvs().last()) {
                (true, Some(last)) => {
                    // 下一页从紧跟最后一个 key 的位置开始。
                    start = last.key().to_vec();
                    start.push(0);
                }
                _ => break,
            }
        }

        Ok((entries, revision))
    }

    async fn watch(
        &self,
        start_revision: i64,
    ) -> Result<
        Box<dyn Stream<Item = Result<WatchEvent, ProviderError>> + Send + Unpin>,
        ProviderError,
    > {
        let mut client = self.client.lock().await;
        let opts = WatchOptions::new()
            .with_prefix()
            .with_start_revision(start_revision);
        let (watcher, stream) = client
            .watch(self.prefix.as_bytes(), Some(opts))
            .await
            .map_err(|e| ProviderError::Watch(format_error_chain(&e)))?;

        Ok(Box::new(EtcdWatchStream {
            inner: stream,
            _watcher: watcher,
            buf: VecDeque::new(),
        }))
    }
}

/// Adapter from `etcd-client`'s WatchStream to our typed [`WatchEvent`].
///
/// Each `WatchResponse` carries a batch of events; we flatten them
/// into individual `WatchEvent` items. A `VecDeque` buffer drains
/// multi-event responses across successive `poll_next` calls so that
/// no events are silently dropped.
pub struct EtcdWatchStream {
    inner: etcd_client::WatchStream,
    // Must outlive `inner` — dropping the Watcher closes the client→server
    // half of the gRPC stream, causing the server to tear down the watch.
    _watcher: etcd_client::Watcher,
    buf: VecDeque<WatchEvent>,
}

fn convert_event(ev: &etcd_client::Event) -> Option<WatchEvent> {
    match ev.event_type() {
        EventType::Put => ev.kv().map(|kv| {
            WatchEvent::Put(RawEntry {
                key: String::from_utf8_lossy(kv.key()).into_owned(),
                value: kv.value().to_vec(),
                revision: kv.mod_revision(),
            })
        }),
        EventType::Delete => ev.kv().map(|kv| WatchEvent::Delete {
            key: String::from_utf8_lossy(kv.key()).into_owned(),
            revision: kv.mod_revision(),
        }),
    }
}

impl Stream for EtcdWatchStream {
    type Item = Result<WatchEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Drain buffered events from a previous multi-event response first.
        if let Some(item) = self.buf.pop_front() {
            return Poll::Ready(Some(Ok(item)));
        }

        match self.inner.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => {
                let shallow = err.to_string();
                if shallow.contains("required revision has been compacted")
                    || shallow.contains("mvcc: required revision")
                {
                    Poll::Ready(Some(Err(ProviderError::Compacted)))
                } else {
                    Poll::Ready(Some(Err(ProviderError::Watch(format_error_chain(&err)))))
                }
            }
            Poll::Ready(Some(Ok(resp))) => {
                if resp.compact_revision() > 0 {
                    return Poll::Ready(Some(Err(ProviderError::Compacted)));
                }

                for ev in resp.events() {
                    if let Some(item) = convert_event(ev) {
                        self.buf.push_back(item);
                    }
                }

                if let Some(item) = self.buf.pop_front() {
                    return Poll::Ready(Some(Ok(item)));
                }

                // Empty response (e.g. header-only): tell the runtime
                // to poll us again rather than stalling.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_retry_constants_match_spec() {
        assert_eq!(CONNECT_RETRY_INTERVAL, Duration::from_secs(5));
        assert_eq!(CONNECT_MAX_ATTEMPTS, 5);
    }

    #[test]
    fn default_policy_matches_spec() {
        let p = ConnectPolicy::default();
        assert_eq!(p.interval, CONNECT_RETRY_INTERVAL);
        assert_eq!(p.attempts, CONNECT_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn connect_with_malformed_endpoint_returns_connect_error() {
        // Empty endpoint list is treated as a parse failure by etcd-client,
        // which lets us exercise the retry loop's error branch without
        // waiting on a real TCP timeout. A compressed policy keeps the
        // test sub-millisecond.
        let policy = ConnectPolicy {
            interval: Duration::from_millis(1),
            attempts: 1,
            ..Default::default()
        };
        let endpoints: Vec<String> = vec![];
        let err = EtcdConfigProvider::connect_with_policy(&endpoints, "/aisix", None, policy)
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Connect(_)));
    }

    #[test]
    fn format_error_chain_joins_sources_without_duplicating() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("Name or service not known")
            }
        }
        impl StdError for Inner {}

        #[derive(Debug)]
        struct Outer {
            inner: Inner,
        }
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("dns error")
            }
        }
        impl StdError for Outer {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                Some(&self.inner)
            }
        }

        let joined = format_error_chain(&Outer { inner: Inner });
        assert_eq!(joined, "dns error: Name or service not known");
    }

    #[test]
    fn format_error_chain_handles_empty_source() {
        let err = std::io::Error::other("bare");
        assert_eq!(format_error_chain(&err), "bare");
    }
}
