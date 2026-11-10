//! Pluggable observability-sink framework — the capability-typed adapter
//! contract that the shared delivery pipeline drives.
//!
//! One implementation per sink *family* (`http_batch`, `object_store`,
//! `warehouse_stream`, `otlp`); per-vendor behaviour (Aliyun SLS, Datadog,
//! …) is configuration plus pluggable encoder/signer traits, not a new
//! sink. The shared pipeline owns batching, retry/backoff, backpressure and
//! per-sink delivery state; a sink only encodes a batch and reports the
//! outcome.
//!
//! This module is the framework foundation (#692, phase F1): the
//! trait + capability matrix + idempotency types. The shared pipeline (F2)
//! and the concrete sinks (SLS, …) build on it.

mod capabilities;
mod datadog;
mod manager;
mod object_store;
mod pipeline;
mod record;
mod redact;
mod sls;
mod truncate;

pub use capabilities::{
    BatchUnit, ChannelKey, IdempotencyMarker, IdempotencyScheme, OrderingScope, SinkCapabilities,
};
pub use datadog::{resolve_datadog_credential, DatadogSink};
pub use manager::ExporterPipelines;
pub use object_store::{build_object_store_sink, ObjectStoreSink};
pub use pipeline::{PipelineConfig, SinkHandle, SinkPipeline, SinkStatsSnapshot};
pub(crate) use record::is_mcp;
pub use record::{CapturedContent, EventBatch, SinkContent, SinkRecord, SCHEMA_VERSION};
pub use redact::ErrorRedactor;
pub use sls::{resolve_sls_credential, AliyunSlsSink};
pub use truncate::{omit_base64_media, omit_base64_media_in_json};

use std::time::Duration;

use async_trait::async_trait;

/// Health snapshot a sink reports for the circuit-breaker and the dashboard.
#[derive(Debug, Clone)]
pub struct SinkHealth {
    pub healthy: bool,
    /// Masked, human-readable reason when unhealthy. Never contains secrets.
    pub detail: Option<String>,
}

impl SinkHealth {
    pub fn healthy() -> Self {
        Self {
            healthy: true,
            detail: None,
        }
    }

    pub fn unhealthy(detail: impl Into<String>) -> Self {
        Self {
            healthy: false,
            detail: Some(detail.into()),
        }
    }
}

/// Acknowledgement returned by a successful (possibly partial) delivery.
#[derive(Debug, Clone, Default)]
pub struct SinkAck {
    /// Number of records the sink accepted on this call.
    pub accepted: usize,
    /// For partial-success sinks, the index of the first record that failed
    /// — the pipeline retries from there. `None` = whole batch accepted.
    pub first_failed: Option<usize>,
    /// The idempotency marker now durably committed, if any (offset-token /
    /// file-sequence sinks). `None` for at-least-once sinks.
    pub committed: Option<IdempotencyMarker>,
    /// What the receiver said it did not keep, or warned about, while
    /// accepting the delivery. `None` = it kept everything and said nothing.
    pub partial: Option<PartialSuccess>,
}

/// A receiver that accepted a delivery and reported dropping part of it, or
/// attached a warning. Never retried: the receiver answered, and resending
/// would duplicate what it did keep.
#[derive(Debug, Clone, Default)]
pub struct PartialSuccess {
    /// Items the receiver rejected, in its own unit. An OTLP receiver counts
    /// spans, and one record expands to several, so this is not a record
    /// count. `0` means a warning on an otherwise full success.
    pub rejected: u64,
    /// The receiver's account of it, as the sink renders it.
    pub detail: String,
}

/// Why a delivery failed. Drives the pipeline's retry/backoff/drop logic.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    /// Transient — worth retrying with backoff (network, timeout, 5xx,
    /// throttle/429).
    #[error("transient sink error: {0}")]
    Transient(String),
    /// Transient, and the sink said how long to wait: a `Retry-After` on a
    /// 429 or a 503. Retrying sooner than asked is what turns a throttle
    /// into a longer one, so the pipeline waits the stated delay instead
    /// of its own backoff (bounded by `max_backoff` — a sink asking for
    /// an hour does not get to stall the queue behind it that long).
    #[error("transient sink error (retry after {retry_after:?}): {detail}")]
    Throttled {
        retry_after: Duration,
        detail: String,
    },
    /// Permanent for this batch — retrying it unchanged will fail again
    /// (auth/403, malformed payload, oversize). The pipeline stops hammering
    /// and surfaces a masked health error instead.
    #[error("permanent sink error: {0}")]
    Permanent(String),
}

impl SinkError {
    /// Whether the pipeline should retry this batch with backoff.
    pub fn is_transient(&self) -> bool {
        matches!(self, SinkError::Transient(_) | SinkError::Throttled { .. })
    }

    /// The delay the sink itself asked for, if it did.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            SinkError::Throttled { retry_after, .. } => Some(*retry_after),
            _ => None,
        }
    }
}

/// Whether a receiver's HTTP status is worth retrying: 5xx, 408 and 429.
/// Every other failure status is a config / auth / payload error that the
/// same batch meets again on retry. Shared by every HTTP-speaking sink so
/// the family cannot drift apart again.
pub(crate) fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// The [`SinkError`] for a non-success HTTP response: [`SinkError::Throttled`]
/// when a retryable status carried a `Retry-After`, [`SinkError::Transient`]
/// for any other retryable status, [`SinkError::Permanent`] otherwise.
pub(crate) fn http_status_error(
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    detail: String,
) -> SinkError {
    if !is_retryable_status(status) {
        return SinkError::Permanent(detail);
    }
    match retry_after {
        Some(retry_after) => SinkError::Throttled {
            retry_after,
            detail,
        },
        None => SinkError::Transient(detail),
    }
}

/// The `Retry-After` on a throttled or overloaded response, as the HTTP
/// spec allows it: delta-seconds, or an HTTP date to wait until.
///
/// Read only for 429 and 503, the two statuses where it means "come back
/// later" rather than something about the resource. A value that is
/// absent, unparseable or already in the past yields `None`, leaving the
/// pipeline on its own backoff.
pub(crate) fn retry_after_of(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Option<Duration> {
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS
        && status != reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        return None;
    }
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(secs) = raw.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let until = chrono::DateTime::parse_from_rfc2822(raw.trim()).ok()?;
    (until.timestamp() - chrono::Utc::now().timestamp())
        .try_into()
        .ok()
        .map(Duration::from_secs)
}

/// How much of a receiver's response body a sink reads.
///
/// A sink only ever keeps a short excerpt of an error body, and an OTLP
/// success body is a few dozen bytes. Reading the body whole first meant the
/// excerpt's cap bounded what was shown, not what was held: a receiver (or
/// anything in between) answering with a large or endless body inside the
/// request timeout had all of it buffered — decompressed, since the shared
/// client decodes gzip. Large enough for any envelope the sinks parse.
pub(crate) const RESPONSE_BODY_LIMIT: usize = 16 * 1024;

/// The first `limit` bytes of `resp`'s body, as text (lossily decoded).
///
/// Reads chunk by chunk and stops at the cap: the rest of the body is never
/// read, and dropping the response closes the connection. The cap counts the
/// bytes after decompression, so a compressed body cannot expand past it
/// either. A body that fails part-way yields what arrived before the failure.
pub(crate) async fn read_body_prefix(mut resp: reqwest::Response, limit: usize) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(limit.min(4096));
    while buf.len() < limit {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(limit - buf.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Render an error together with its full `source()` chain.
///
/// `Display` on a transport error usually names only the outermost layer
/// ("error sending request", "Error performing PUT <url>") while the
/// actionable cause — a DNS failure, a TLS verification error — sits levels
/// deeper. The scheduled objstore smoke burned five weeks on a detail that
/// ended at the request URL before anyone saw the underlying
/// "failed to lookup address information". Layers whose text the outer
/// message already embeds are skipped, so wrappers that interpolate their
/// source don't repeat it.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(src) = cur {
        let text = src.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cur = src.source();
    }
    out
}

/// Result of one [`ObservabilitySink::append_batch`] call.
pub type SinkResult = Result<SinkAck, SinkError>;

/// A pluggable delivery target for observability events.
///
/// Implementors are held as `Arc<dyn ObservabilitySink>` and driven by the
/// shared pipeline. The trait is deliberately small: encode-and-deliver one
/// batch, plus the hooks streaming sinks need (committed-marker resume) and
/// everyone needs (health). HTTP/object-store sinks get sensible defaults so
/// they stay simple.
#[async_trait]
pub trait ObservabilitySink: Send + Sync + 'static {
    /// Stable name used in logs/metrics labels and health reporting.
    fn name(&self) -> &str;

    /// How this sink wants to be driven (idempotency, ordering, batch
    /// sizing, partial-success).
    fn capabilities(&self) -> SinkCapabilities;

    /// Deliver one batch. The pipeline retains ownership of `batch` and
    /// retries on [`SinkError::Transient`]; `marker` is the idempotency
    /// marker for this batch (or [`IdempotencyMarker::None`] for
    /// at-least-once sinks).
    async fn append_batch(&self, batch: &EventBatch, marker: &IdempotencyMarker) -> SinkResult;

    /// On startup, the last marker this channel durably committed, so the
    /// pipeline can resume after a restart. Defaults to `None` —
    /// at-least-once sinks (every `http_batch` / `object_store` target)
    /// don't track one.
    async fn last_committed_marker(&self, _channel: &ChannelKey) -> Option<IdempotencyMarker> {
        None
    }

    /// Cheap liveness/connectivity probe for the circuit-breaker and the
    /// control-plane "test connection" affordance.
    async fn healthcheck(&self) -> SinkHealth;

    /// The configured URLs and secrets the pipeline scrubs from this sink's
    /// delivery errors before logging or reporting them. A sink configured
    /// with a URL or a credential must declare it here.
    fn error_redactor(&self) -> ErrorRedactor {
        ErrorRedactor::default()
    }
}

/// A receiver that answers every request with a 500 whose body never ends,
/// for the sinks' bounded-read cases.
#[cfg(test)]
pub(crate) mod endless_body {
    use std::io::Write;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// What `serve` writes as the body, chunk after chunk.
    pub(crate) const FILL: u8 = b'x';

    /// Past this many body bytes the receiver stops writing but keeps the
    /// body open, so a reader that waits for its end still never gets one,
    /// without a test of the broken reader holding gigabytes.
    const SENT_CAP: usize = 64 << 20;

    /// Serve on loopback and return `http://127.0.0.1:<port>`. `gzip` sends
    /// the body as a gzip stream, every chunk of which decompresses to a
    /// megabyte. Each connection ends when the client hangs up.
    pub(crate) async fn serve(gzip: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // Take the whole request first, so the client is reading
                    // the response rather than still writing its body.
                    let mut request = Vec::new();
                    let mut buf = [0u8; 8192];
                    loop {
                        let Ok(n) = conn.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    let encoding = if gzip {
                        "content-encoding: gzip\r\n"
                    } else {
                        ""
                    };
                    let head = format!(
                        "HTTP/1.1 500 Internal Server Error\r\ncontent-type: text/plain\r\n\
                         {encoding}transfer-encoding: chunked\r\n\r\n"
                    );
                    if conn.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let plain = vec![FILL; 1 << 20];
                    let mut encoder =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                    let mut sent = 0;
                    loop {
                        if sent >= SENT_CAP {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                            return;
                        }
                        sent += if gzip { plain.len() } else { 64 * 1024 };
                        let chunk = if gzip {
                            encoder.write_all(&plain).unwrap();
                            encoder.flush().unwrap();
                            std::mem::take(encoder.get_mut())
                        } else {
                            plain[..64 * 1024].to_vec()
                        };
                        let framed = [
                            format!("{:x}\r\n", chunk.len()).into_bytes(),
                            chunk,
                            b"\r\n".to_vec(),
                        ]
                        .concat();
                        if conn.write_all(&framed).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageEvent;
    use std::sync::Arc;

    /// An error body is read only as far as its excerpt needs, compressed
    /// or not: a receiver that never stops sending must not be buffered.
    #[tokio::test]
    async fn a_response_body_is_read_only_up_to_the_cap() {
        for gzip in [false, true] {
            let base = endless_body::serve(gzip).await;
            let resp = reqwest::Client::new()
                .post(format!("{base}/"))
                .body("{}")
                .send()
                .await
                .unwrap();
            let read = tokio::time::timeout(
                Duration::from_secs(10),
                read_body_prefix(resp, RESPONSE_BODY_LIMIT),
            )
            .await
            .unwrap_or_else(|_| panic!("gzip={gzip}: the read never stopped"));
            assert_eq!(read.len(), RESPONSE_BODY_LIMIT, "gzip={gzip}");
            assert!(
                read.bytes().all(|b| b == endless_body::FILL),
                "gzip={gzip}: the cap counts decoded bytes"
            );
        }
    }

    #[test]
    fn http_status_error_retries_only_5xx_408_429() {
        use reqwest::StatusCode;
        for s in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(
                http_status_error(s, None, String::new()).is_transient(),
                "{s} should be retried"
            );
        }
        for s in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::CONFLICT,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            assert!(
                !http_status_error(s, None, String::new()).is_transient(),
                "{s} should be permanent"
            );
        }
        let throttled = http_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            Some(Duration::from_secs(7)),
            String::new(),
        );
        assert_eq!(throttled.retry_after(), Some(Duration::from_secs(7)));
    }

    /// A trivial at-least-once sink that records what it was handed —
    /// proves the trait is object-safe (`Arc<dyn ObservabilitySink>`) and
    /// usable with the default `last_committed_marker`.
    struct CountingSink {
        accepted: parking_lot::Mutex<usize>,
    }

    #[async_trait]
    impl ObservabilitySink for CountingSink {
        fn name(&self) -> &str {
            "counting"
        }

        fn capabilities(&self) -> SinkCapabilities {
            SinkCapabilities {
                idempotency: IdempotencyScheme::None,
                ordering: OrderingScope::None,
                batch_unit: BatchUnit::Both,
                max_batch_bytes: Some(10 * 1024 * 1024),
                supports_partial_batch: false,
                supports_streaming_ingest: false,
            }
        }

        async fn append_batch(
            &self,
            batch: &EventBatch,
            _marker: &IdempotencyMarker,
        ) -> SinkResult {
            *self.accepted.lock() += batch.len();
            Ok(SinkAck {
                accepted: batch.len(),
                ..SinkAck::default()
            })
        }

        async fn healthcheck(&self) -> SinkHealth {
            SinkHealth::healthy()
        }
    }

    #[tokio::test]
    async fn dyn_sink_accepts_a_batch_and_defaults_marker_to_none() {
        let sink: Arc<dyn ObservabilitySink> = Arc::new(CountingSink {
            accepted: parking_lot::Mutex::new(0),
        });
        let batch = EventBatch::new(vec![Arc::new(SinkRecord::metadata_only(
            UsageEvent::default(),
        ))]);

        let ack = sink
            .append_batch(&batch, &IdempotencyMarker::None)
            .await
            .expect("delivery succeeds");
        assert_eq!(ack.accepted, 1);

        let channel = ChannelKey {
            org_id: "o".into(),
            env_id: "e".into(),
            worker_idx: 0,
            dp_node_uid: "node-1".into(),
            target: "request-events".into(),
        };
        assert_eq!(sink.last_committed_marker(&channel).await, None);
        assert!(sink.healthcheck().await.healthy);
    }

    #[test]
    fn sink_error_transience_drives_retry() {
        assert!(SinkError::Transient("429".into()).is_transient());
        assert!(!SinkError::Permanent("403".into()).is_transient());
        // A throttle is a transient failure that came with instructions.
        let throttled = SinkError::Throttled {
            retry_after: Duration::from_secs(7),
            detail: "429".into(),
        };
        assert!(throttled.is_transient());
        assert_eq!(throttled.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(SinkError::Transient("429".into()).retry_after(), None);
    }

    /// `Retry-After` is what a throttling receiver knows and our backoff
    /// ladder does not. Read it in both forms the spec allows, and only
    /// where it means "come back later".
    #[test]
    fn retry_after_is_read_on_throttle_and_overload_only() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        use reqwest::StatusCode;

        let with = |raw: &str| {
            let mut h = HeaderMap::new();
            h.insert(RETRY_AFTER, HeaderValue::from_str(raw).unwrap());
            h
        };

        assert_eq!(
            retry_after_of(StatusCode::TOO_MANY_REQUESTS, &with("30")),
            Some(Duration::from_secs(30)),
            "delta-seconds",
        );
        let date = chrono::Utc::now() + chrono::Duration::seconds(45);
        let parsed = retry_after_of(
            StatusCode::SERVICE_UNAVAILABLE,
            &with(&date.format("%a, %d %b %Y %H:%M:%S GMT").to_string()),
        )
        .expect("an HTTP date is a legal Retry-After");
        assert!(
            parsed >= Duration::from_secs(43) && parsed <= Duration::from_secs(45),
            "an HTTP date resolves to the wait it implies: {parsed:?}",
        );

        // A date already past means "now", never a negative wait.
        let past = chrono::Utc::now() - chrono::Duration::seconds(60);
        assert_eq!(
            retry_after_of(
                StatusCode::SERVICE_UNAVAILABLE,
                &with(&past.format("%a, %d %b %Y %H:%M:%S GMT").to_string()),
            ),
            None,
        );
        assert_eq!(
            retry_after_of(StatusCode::TOO_MANY_REQUESTS, &with("whenever")),
            None,
            "an unparseable value leaves the pipeline on its own backoff",
        );
        assert_eq!(
            retry_after_of(StatusCode::INTERNAL_SERVER_ERROR, &with("30")),
            None,
            "on other statuses the header is not this instruction",
        );
        assert_eq!(
            retry_after_of(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new()),
            None,
        );
    }

    #[test]
    fn health_helpers_round_trip() {
        assert!(SinkHealth::healthy().healthy);
        let bad = SinkHealth::unhealthy("endpoint unreachable");
        assert!(!bad.healthy);
        assert_eq!(bad.detail.as_deref(), Some("endpoint unreachable"));
    }

    #[derive(Debug)]
    struct Layered {
        text: &'static str,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    }

    impl std::fmt::Display for Layered {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.text)
        }
    }

    impl std::error::Error for Layered {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source
                .as_deref()
                .map(|s| s as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn error_chain_appends_hidden_sources() {
        // The wrapper's Display names only itself — the shape reqwest and
        // object_store's retry error have, which is what buried the DNS
        // cause of the scheduled objstore failure.
        let e = Layered {
            text: "error sending request",
            source: Some(Box::new(Layered {
                text: "dns error: failed to lookup address information",
                source: None,
            })),
        };
        assert_eq!(
            error_chain(&e),
            "error sending request: dns error: failed to lookup address information"
        );
    }

    #[test]
    fn error_chain_skips_sources_already_interpolated() {
        // Wrappers that embed their source's Display (thiserror's
        // `#[error("outer: {0}")]` pattern) must not repeat it.
        let e = Layered {
            text: "outer: inner detail",
            source: Some(Box::new(Layered {
                text: "inner detail",
                source: Some(Box::new(Layered {
                    text: "root cause",
                    source: None,
                })),
            })),
        };
        assert_eq!(error_chain(&e), "outer: inner detail: root cause");
    }
}
