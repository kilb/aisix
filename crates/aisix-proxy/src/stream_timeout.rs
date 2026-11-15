//! Per-chunk read-timeout combinators for streaming upstreams (#554).
//!
//! Per-chunk streaming read-timeout: the deadline bounds the
//! wait for EACH chunk — the first one and every inter-chunk gap — and
//! resets after each successful read. A *first-chunk* timeout lets the
//! caller fail over before any bytes reach the client (issue AC2); a
//! *mid-stream* timeout terminates the stream like any other upstream
//! error, because once the `200` is committed a clean fallback is no
//! longer possible.
//!
//! Two flavours:
//! - [`with_read_timeout`] for the typed [`ChatChunkStream`] path
//!   (`/v1/chat/completions`, cross-provider `/v1/messages`): a read
//!   timeout surfaces as [`BridgeError::Timeout`], which the SSE pump
//!   already renders as an error frame.
//! - [`with_read_timeout_bytes`] for the raw byte passthroughs
//!   (`/v1/responses`, native-Anthropic `/v1/messages`): a read timeout
//!   remains typed so the first-byte path can fail over and the mid-stream
//!   path can distinguish truncation from clean EOF. There is no in-band
//!   error frame to inject into an opaque passthrough.
//!
//! [`send_with_deadline`] bounds the connect phase of a raw passthrough so
//! a slow upstream that never returns response headers also fails over.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_gateway::{BridgeError, ChatChunkStream, CompletionByteStream};
use bytes::Bytes;
use futures::{Stream, StreamExt};

/// Error carried by raw upstream byte streams. Reqwest does not expose a
/// constructor for a timeout error, so collapsing the timeout to
/// `reqwest::Result` used to require silently ending the stream. Keeping a
/// small gateway-owned error preserves the terminal outcome without changing
/// the forwarded bytes.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RawStreamError {
    #[error("service temporarily unavailable")]
    DependencyUnavailable,
    #[error("upstream native response frame could not be decoded")]
    UpstreamDecode,
    #[error(transparent)]
    Upstream(#[from] reqwest::Error),
    #[error("upstream stream read timed out after {elapsed_ms} ms")]
    Timeout { elapsed_ms: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawStreamFailure {
    DependencyUnavailable,
    Timeout {
        elapsed_ms: u64,
    },
    Upstream,
    UpstreamDecode,
    UpstreamInBand {
        status: Option<u16>,
        wire: aisix_gateway::UpstreamWire,
    },
}

impl RawStreamError {
    pub(crate) fn failure(&self) -> RawStreamFailure {
        match self {
            Self::Timeout { elapsed_ms } => RawStreamFailure::Timeout {
                elapsed_ms: *elapsed_ms,
            },
            Self::Upstream(_) => RawStreamFailure::Upstream,
            Self::DependencyUnavailable => RawStreamFailure::DependencyUnavailable,
            Self::UpstreamDecode => RawStreamFailure::UpstreamDecode,
        }
    }

    pub(crate) fn into_bridge(self, started: Instant) -> BridgeError {
        self.as_bridge(started)
    }

    // 错误仍需向响应体转发时，统一保留其终态分类，不消费原始错误。
    pub(crate) fn as_bridge(&self, started: Instant) -> BridgeError {
        match self {
            Self::DependencyUnavailable => BridgeError::DependencyUnavailable,
            Self::UpstreamDecode => {
                BridgeError::UpstreamDecode("native response frame could not be decoded".into())
            }
            Self::Timeout { elapsed_ms } => BridgeError::Timeout {
                elapsed_ms: *elapsed_ms,
                cause: String::new(),
            },
            Self::Upstream(error) => crate::dispatch::reqwest_error_to_bridge(error, started),
        }
    }
}

impl RawStreamFailure {
    pub(crate) fn bridge_error(self) -> BridgeError {
        match self {
            Self::DependencyUnavailable => BridgeError::DependencyUnavailable,
            Self::Timeout { elapsed_ms } => BridgeError::Timeout {
                elapsed_ms,
                cause: "stream body".to_string(),
            },
            Self::Upstream => BridgeError::StreamAborted,
            Self::UpstreamDecode => {
                BridgeError::UpstreamDecode("malformed upstream SSE event".to_string())
            }
            Self::UpstreamInBand { status, wire } => BridgeError::UpstreamInBand {
                status,
                message: "upstream reported a stream error".to_string(),
                parsed: None,
                wire,
            },
        }
    }
}

/// Final status for a committed streaming response. Upstream-body failures
/// retain their gateway status, a clean or gateway-completed terminal outcome
/// marks the target healthy, and only a downstream cancellation becomes 499.
pub(crate) struct StreamTerminalStatus {
    pub(crate) status: u16,
    pub(crate) error_class: String,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_bridge_stream(
    health: &crate::health::HealthTracker,
    runtime_status: &crate::health::ModelRuntimeStatusTracker,
    model_display_name: &str,
    model_id: &str,
    cooldown: Option<&aisix_core::CooldownConfig>,
    failure: Option<BridgeError>,
    completed: bool,
    complete_status: u16,
    attempt_started: std::time::Instant,
) -> StreamTerminalStatus {
    if let Some(error) = failure {
        let status = error.http_status();
        let error_class = error.error_type().to_string();
        if status >= 500 && !matches!(error, BridgeError::DependencyUnavailable) {
            health.record_failure(model_display_name);
        }
        let _ = crate::cooldown::note_failure(runtime_status, model_id, cooldown, error);
        return StreamTerminalStatus {
            status,
            error_class,
        };
    }
    if completed {
        health.record_success(model_display_name);
        runtime_status.mark_healthy(model_id, attempt_started);
        return StreamTerminalStatus {
            status: complete_status,
            error_class: String::new(),
        };
    }
    StreamTerminalStatus {
        status: crate::CLIENT_CLOSED_REQUEST,
        error_class: String::new(),
    }
}

/// Wrap a [`ChatChunkStream`] so each `next()` is bounded by `per_chunk`.
/// On elapse, yield a single [`BridgeError::Timeout`] and end the stream.
/// `None` returns the stream unchanged (zero overhead on the hot path).
pub(crate) fn with_read_timeout(
    upstream: ChatChunkStream,
    per_chunk: Option<Duration>,
) -> ChatChunkStream {
    let Some(d) = per_chunk else {
        return upstream;
    };
    Box::pin(async_stream::stream! {
        // `ChatChunkStream` is a `Pin<Box<..>>`, hence `Unpin`; a plain
        // `mut` binding is enough to poll it via `StreamExt::next`.
        let mut upstream = upstream;
        loop {
            match tokio::time::timeout(d, upstream.next()).await {
                Ok(Some(item)) => yield item,
                Ok(None) => break,
                Err(_) => {
                    yield Err(BridgeError::Timeout {
                        elapsed_ms: d.as_millis() as u64,
                        cause: String::new(),
                    });
                    break;
                }
            }
        }
    })
}

/// Legacy-completions equivalent of [`with_read_timeout`]. The stream is
/// opaque bytes, but its error channel is already [`BridgeError`], so a
/// timeout remains typed and the completion stream's terminal telemetry can
/// distinguish an upstream timeout from a downstream disconnect.
pub(crate) fn with_read_timeout_completion(
    upstream: CompletionByteStream,
    per_chunk: Option<Duration>,
) -> CompletionByteStream {
    let Some(d) = per_chunk else {
        return upstream;
    };
    Box::pin(async_stream::stream! {
        let mut upstream = upstream;
        loop {
            match tokio::time::timeout(d, upstream.next()).await {
                Ok(Some(item)) => yield item,
                Ok(None) => break,
                Err(_) => {
                    yield Err(BridgeError::Timeout {
                        elapsed_ms: d.as_millis() as u64,
                        cause: String::new(),
                    });
                    break;
                }
            }
        }
    })
}

/// Wrap a raw byte stream (`reqwest::Response::bytes_stream()`) so each
/// `next()` is bounded by `per_chunk`. On elapse, yield one typed timeout and
/// end the stream. `None` still maps reqwest's error into the gateway-owned
/// error without adding a timer.
pub(crate) fn with_read_timeout_bytes<S>(
    upstream: S,
    per_chunk: Option<Duration>,
) -> impl Stream<Item = Result<Bytes, RawStreamError>> + Send
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    async_stream::stream! {
        let mut upstream = std::pin::pin!(upstream);
        loop {
            match per_chunk {
                Some(d) => match tokio::time::timeout(d, upstream.next()).await {
                    Ok(Some(item)) => yield item.map_err(RawStreamError::from),
                    Ok(None) => break,
                    Err(_) => {
                        yield Err(RawStreamError::Timeout {
                            elapsed_ms: d.as_millis() as u64,
                        });
                        break;
                    }
                },
                None => match upstream.next().await {
                    Some(item) => yield item.map_err(RawStreamError::from),
                    None => break,
                },
            }
        }
    }
}

/// Whether [`with_read_timeout_bytes_signalled`] ended its stream on a read
/// timeout. The truncated byte stream looks like a clean end to whoever
/// relays it, so this is how the relay's usage record learns the upstream
/// stalled.
#[derive(Clone, Default)]
pub(crate) struct ReadTimeoutSignal(Arc<AtomicU64>);

impl ReadTimeoutSignal {
    /// The elapsed read timeout, as the error it would have been on the
    /// typed path; `None` if the stream did not end on one.
    pub(crate) fn fired(&self) -> Option<BridgeError> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            elapsed_ms => Some(BridgeError::Timeout {
                elapsed_ms,
                cause: String::new(),
            }),
        }
    }
}

/// [`with_read_timeout_bytes`] for a relay that carries `reqwest` errors and
/// has no way to report a typed timeout in-band: an elapsed read timeout
/// truncates the stream and is recorded on `signal`.
pub(crate) fn with_read_timeout_bytes_signalled<S>(
    upstream: S,
    per_chunk: Option<Duration>,
    signal: ReadTimeoutSignal,
) -> impl Stream<Item = reqwest::Result<Bytes>> + Send
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    async_stream::stream! {
        // 此接口只接收 reqwest 错误，不能产生原生对象注册或 SSE 解析错误。
        let mut upstream = std::pin::pin!(upstream);
        loop {
            let item = match per_chunk {
                Some(budget) => match tokio::time::timeout(budget, upstream.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        let elapsed_ms = budget.as_millis().min(u128::from(u64::MAX)) as u64;
                        signal.0.store(elapsed_ms.max(1), Ordering::Relaxed);
                        break;
                    }
                },
                None => upstream.next().await,
            };
            let Some(item) = item else { break };
            yield item;
        }
    }
}

/// Send a raw-passthrough request, optionally bounding the connect phase
/// (everything up to and including response headers) by `deadline`. Maps
/// both reqwest's own timeout and the outer deadline to
/// [`BridgeError::Timeout`] so a slow connect fails over like the
/// Bridge-trait path. `started` anchors the reported elapsed time.
pub(crate) async fn send_with_deadline(
    req: reqwest::RequestBuilder,
    deadline: Option<Duration>,
    started: Instant,
) -> Result<reqwest::Response, BridgeError> {
    send_with_deadline_inner(req, deadline, started, None).await
}

/// 模型调用在最终 HTTP 正文边界观察；媒体下载等非模型请求继续使用原入口。
pub(crate) async fn send_with_deadline_observed(
    req: reqwest::RequestBuilder,
    deadline: Option<Duration>,
    started: Instant,
    protocol: aisix_gateway::upstream_io::Protocol,
) -> Result<reqwest::Response, BridgeError> {
    send_with_deadline_inner(req, deadline, started, Some(protocol)).await
}

async fn send_with_deadline_inner(
    req: reqwest::RequestBuilder,
    deadline: Option<Duration>,
    started: Instant,
    protocol: Option<aisix_gateway::upstream_io::Protocol>,
) -> Result<reqwest::Response, BridgeError> {
    let send = async move {
        match protocol {
            Some(protocol) => aisix_gateway::upstream_io::send(req, protocol).await,
            None => req.send().await,
        }
    };
    let sent = match deadline {
        Some(d) => match tokio::time::timeout(d, send).await {
            Ok(res) => res.map_err(|e| crate::dispatch::reqwest_error_to_bridge(&e, started)),
            Err(_) => Err(BridgeError::Timeout {
                elapsed_ms: started.elapsed().as_millis() as u64,
                cause: String::new(),
            }),
        },
        None => send
            .await
            .map_err(|e| crate::dispatch::reqwest_error_to_bridge(&e, started)),
    };
    if let Ok(resp) = &sent {
        aisix_gateway::note_upstream_response(resp.headers());
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stall past the per-chunk budget ends the byte stream exactly as it
    /// always did, and now also says so, carrying the budget it hit.
    #[tokio::test]
    async fn a_read_timeout_is_signalled() {
        let signal = ReadTimeoutSignal::default();
        let stalled = futures::stream::once(async { Ok::<_, reqwest::Error>(Bytes::from("a")) })
            .chain(futures::stream::pending());
        let out: Vec<_> = with_read_timeout_bytes_signalled(
            stalled,
            Some(Duration::from_millis(20)),
            signal.clone(),
        )
        .collect()
        .await;
        assert_eq!(out.len(), 1);
        match signal.fired() {
            Some(BridgeError::Timeout { elapsed_ms, .. }) => assert_eq!(elapsed_ms, 20),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    /// A stream that ends on its own leaves the signal unset.
    #[tokio::test]
    async fn a_clean_end_is_not_a_read_timeout() {
        let signal = ReadTimeoutSignal::default();
        let done = futures::stream::iter([Ok::<_, reqwest::Error>(Bytes::from("a"))]);
        let _: Vec<_> =
            with_read_timeout_bytes_signalled(done, Some(Duration::from_secs(5)), signal.clone())
                .collect()
                .await;
        assert!(signal.fired().is_none());
    }

    fn delayed_completion_stream(first_immediate: bool) -> CompletionByteStream {
        Box::pin(async_stream::stream! {
            if first_immediate {
                yield Ok(Bytes::from_static(b"first"));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            yield Ok(Bytes::from_static(b"late"));
        })
    }

    #[tokio::test(start_paused = true)]
    async fn completion_timeout_surfaces_before_first_chunk() {
        let mut stream = with_read_timeout_completion(
            delayed_completion_stream(false),
            Some(Duration::from_secs(1)),
        );

        assert!(matches!(
            stream.next().await,
            Some(Err(BridgeError::Timeout {
                elapsed_ms: 1_000,
                ..
            }))
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn completion_timeout_resets_after_each_chunk() {
        let mut stream = with_read_timeout_completion(
            delayed_completion_stream(true),
            Some(Duration::from_secs(1)),
        );

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"first")
        );
        assert!(matches!(
            stream.next().await,
            Some(Err(BridgeError::Timeout {
                elapsed_ms: 1_000,
                ..
            }))
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn raw_timeout_is_typed_before_first_chunk() {
        let upstream = futures::stream::pending::<reqwest::Result<Bytes>>();
        let mut stream = Box::pin(with_read_timeout_bytes(
            upstream,
            Some(Duration::from_secs(1)),
        ));

        assert!(matches!(
            stream.next().await,
            Some(Err(RawStreamError::Timeout { elapsed_ms: 1_000 }))
        ));
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn raw_first_chunk_timeout_maps_to_retryable_504() {
        let error = RawStreamError::Timeout { elapsed_ms: 300 }.into_bridge(Instant::now());
        assert!(matches!(
            error,
            BridgeError::Timeout {
                elapsed_ms: 300,
                ..
            }
        ));
        assert_eq!(error.http_status(), 504);
        assert!(crate::routing::is_retryable(&error, false, &[]));
    }

    #[test]
    fn raw_dependency_and_decode_errors_keep_their_terminal_class() {
        let dependency = RawStreamError::DependencyUnavailable;
        assert_eq!(
            dependency.failure(),
            RawStreamFailure::DependencyUnavailable
        );
        assert!(matches!(
            dependency.as_bridge(Instant::now()),
            BridgeError::DependencyUnavailable
        ));
        assert!(matches!(
            dependency.into_bridge(Instant::now()),
            BridgeError::DependencyUnavailable
        ));
        assert!(matches!(
            RawStreamFailure::DependencyUnavailable.bridge_error(),
            BridgeError::DependencyUnavailable
        ));

        let decode = RawStreamError::UpstreamDecode;
        assert_eq!(decode.failure(), RawStreamFailure::UpstreamDecode);
        assert!(matches!(
            decode.as_bridge(Instant::now()),
            BridgeError::UpstreamDecode(_)
        ));
        assert!(matches!(
            decode.into_bridge(Instant::now()),
            BridgeError::UpstreamDecode(_)
        ));
    }

    #[test]
    fn terminal_dependency_failure_does_not_penalize_or_recover_upstream_health() {
        let health = crate::health::HealthTracker::new();
        let runtime = crate::health::ModelRuntimeStatusTracker::new();
        let cfg = aisix_core::CooldownConfig {
            enabled: Some(true),
            ..Default::default()
        };
        for failures in [0, 4] {
            let name = format!("model-{failures}");
            for _ in 0..failures {
                health.record_failure(&name);
            }
            let initial = health.level(&name);
            for _ in 0..8 {
                let terminal = finish_bridge_stream(
                    &health,
                    &runtime,
                    &name,
                    &name,
                    Some(&cfg),
                    Some(BridgeError::DependencyUnavailable),
                    false,
                    200,
                    Instant::now(),
                );
                assert_eq!(terminal.status, 503);
                assert!(!terminal.error_class.is_empty());
                assert_eq!(health.level(&name), initial);
                assert_ne!(
                    runtime.status(&name).status,
                    crate::health::RuntimeStatus::Cooldown
                );
            }
        }
    }

    #[test]
    fn terminal_timeout_accumulates_health_instead_of_resetting_it() {
        let health = crate::health::HealthTracker::new();
        let runtime = crate::health::ModelRuntimeStatusTracker::new();
        for _ in 0..3 {
            health.record_failure("model");
        }

        let terminal = finish_bridge_stream(
            &health,
            &runtime,
            "model",
            "model-id",
            None,
            Some(BridgeError::Timeout {
                elapsed_ms: 300,
                cause: String::new(),
            }),
            false,
            200,
            std::time::Instant::now(),
        );

        assert_eq!(terminal.status, 504);
        assert_eq!(terminal.error_class, "timeout");
        assert_eq!(health.level("model"), crate::health::HealthLevel::Degraded);
    }

    #[test]
    fn only_clean_upstream_end_resets_health() {
        let health = crate::health::HealthTracker::new();
        let runtime = crate::health::ModelRuntimeStatusTracker::new();
        for _ in 0..4 {
            health.record_failure("model");
        }

        let dropped = finish_bridge_stream(
            &health,
            &runtime,
            "model",
            "model-id",
            None,
            None,
            false,
            200,
            std::time::Instant::now(),
        );
        assert_eq!(dropped.status, crate::CLIENT_CLOSED_REQUEST);
        assert_eq!(health.level("model"), crate::health::HealthLevel::Degraded);

        let completed = finish_bridge_stream(
            &health,
            &runtime,
            "model",
            "model-id",
            None,
            None,
            true,
            200,
            std::time::Instant::now(),
        );
        assert_eq!(completed.status, 200);
        assert_eq!(health.level("model"), crate::health::HealthLevel::Healthy);
    }

    #[test]
    fn gateway_completed_stream_uses_its_terminal_status() {
        let health = crate::health::HealthTracker::new();
        let runtime = crate::health::ModelRuntimeStatusTracker::new();

        let completed = finish_bridge_stream(
            &health,
            &runtime,
            "model",
            "model-id",
            None,
            None,
            true,
            422,
            std::time::Instant::now(),
        );

        assert_eq!(completed.status, 422);
        assert_eq!(health.level("model"), crate::health::HealthLevel::Healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn raw_timeout_is_typed_after_a_delivered_chunk() {
        let upstream = async_stream::stream! {
            yield Ok(Bytes::from_static(b"first"));
            tokio::time::sleep(Duration::from_secs(2)).await;
            yield Ok(Bytes::from_static(b"late"));
        };
        let mut stream = Box::pin(with_read_timeout_bytes(
            upstream,
            Some(Duration::from_secs(1)),
        ));

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"first")
        );
        assert!(matches!(
            stream.next().await,
            Some(Err(RawStreamError::Timeout { elapsed_ms: 1_000 }))
        ));
        assert!(stream.next().await.is_none());
    }
}

#[cfg(test)]
mod upstream_io_tests {
    use super::*;
    use aisix_gateway::response_tap::{scope, ResponseHeaderTap};
    use aisix_gateway::upstream_io::Protocol;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // 计数必须跟随实际 multipart 编码，包括 boundary 和文件字节；响应仍保留原始头和正文。
    #[tokio::test]
    async fn observed_deadline_helper_counts_encoded_multipart_body(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::builder().start().await;
        let output = br#"{"data":[{"b64_json":"AAEC"}],"usage":{"total_tokens":17}}"#;
        Mock::given(method("POST"))
            .and(path("/images/edits"))
            .respond_with(
                ResponseTemplate::new(201)
                    .insert_header("content-type", "application/json")
                    .insert_header("x-request-id", "upstream-multipart")
                    .set_body_bytes(output.to_vec()),
            )
            .expect(2)
            .mount(&server)
            .await;
        for deadline in [None, Some(Duration::from_secs(5))] {
            let tap = Arc::new(ResponseHeaderTap::new(
                vec!["x-request-id".to_owned()],
                false,
                String::new(),
            ));
            let form = reqwest::multipart::Form::new()
                .text("prompt", "A汉😀")
                .part(
                    "image",
                    reqwest::multipart::Part::bytes(vec![0, 255, 1, 254]).file_name("sample.bin"),
                );
            let response = scope(
                tap.clone(),
                send_with_deadline_observed(
                    reqwest::Client::new()
                        .post(format!("{}/images/edits", server.uri()))
                        .multipart(form),
                    deadline,
                    Instant::now(),
                    Protocol::Images,
                ),
            )
            .await?;
            assert_eq!(response.status(), reqwest::StatusCode::CREATED);
            assert_eq!(response.headers()["x-request-id"], "upstream-multipart");
            let io = tap
                .upstream_io_observer()
                .ok_or("missing upstream observer")?;
            assert!(!io.snapshot().response_complete);
            assert_eq!(response.bytes().await?.as_ref(), output);
            let received = server
                .received_requests()
                .await
                .ok_or("missing upstream requests")?;
            let sent = received.last().ok_or("missing multipart request")?;
            assert!(sent.body.len() > "A汉😀".len() + 4);
            let audit = io.snapshot();
            assert_eq!(audit.boundary, "upstream_attempt");
            assert_eq!(audit.request_body_bytes, Some(sent.body.len() as u64));
            assert_eq!(audit.response_body_bytes, Some(output.len() as u64));
            assert_eq!(audit.request_complete, Some(true));
            assert!(audit.response_complete);
            assert_eq!(audit.exchange_count, Some(1));
        }
        Ok(())
    }

    // 非模型下载不能因复用截止时间工具而混入模型的上游交换。
    #[tokio::test]
    async fn unobserved_deadline_helper_keeps_download_outside_model_audit(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/video.mp4"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0, 255, 7]))
            .expect(1)
            .mount(&server)
            .await;
        let tap = Arc::new(ResponseHeaderTap::default());
        let response = scope(
            tap.clone(),
            send_with_deadline(
                reqwest::Client::new().get(format!("{}/video.mp4", server.uri())),
                Some(Duration::from_secs(5)),
                Instant::now(),
            ),
        )
        .await?;
        assert_eq!(response.bytes().await?.as_ref(), &[0, 255, 7]);
        assert!(tap.upstream_io_observer().is_none());
        Ok(())
    }

    // 截止时间仍返回原有可重试 Timeout；尚未收到正文不能伪造完整响应。
    #[tokio::test]
    async fn observed_deadline_helper_preserves_typed_connect_timeout(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(1))
                    .set_body_json(serde_json::json!({"content":[]})),
            )
            .mount(&server)
            .await;
        let tap = Arc::new(ResponseHeaderTap::default());
        let response = scope(
            tap.clone(),
            send_with_deadline_observed(
                reqwest::Client::new()
                    .post(format!("{}/messages", server.uri()))
                    .body(r#"{"messages":[{"role":"user","content":"A汉😀"}]}"#),
                Some(Duration::from_millis(20)),
                Instant::now(),
                Protocol::Messages,
            ),
        )
        .await;
        assert!(matches!(response, Err(BridgeError::Timeout { .. })));
        let audit = tap
            .upstream_io_observer()
            .ok_or("missing timeout observer")?
            .snapshot();
        assert!(!audit.response_complete);
        assert_eq!(audit.response_body_bytes, None);
        Ok(())
    }
}

#[cfg(test)]
mod physical_dispatch_boundary_tests {
    use super::*;
    use aisix_gateway::response_tap::{scope, ResponseHeaderTap};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // 发送事实与模型 IO 审计正交；下载未收到响应头即取消，也不能抹掉真实 GET。
    #[tokio::test]
    async fn unobserved_download_records_physical_http_without_model_io_before_response() {
        for deadline in [None, Some(Duration::from_secs(10))] {
            let rejected = Arc::new(ResponseHeaderTap::default());
            assert!(!rejected.http_dispatched());
            assert!(rejected.upstream_io_observer().is_none());
            let invalid = scope(
                rejected.clone(),
                send_with_deadline(
                    reqwest::Client::new().get("not a valid HTTP URL"),
                    deadline,
                    Instant::now(),
                ),
            )
            .await;
            assert!(
                invalid.is_err(),
                "invalid builder fails before transmission"
            );
            assert!(
                !rejected.http_dispatched(),
                "failed construction is not HTTP dispatch"
            );
            assert!(rejected.upstream_io_observer().is_none());

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let seen = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let (server_seen, server_release) = (seen.clone(), release.clone());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 8192);
                    socket.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                assert!(head.starts_with(b"GET /owned-content HTTP/1.1\r\n"));
                server_seen.notify_one();
                server_release.notified().await;
                let _ = socket.shutdown().await;
            });
            struct ProviderTask(tokio::task::JoinHandle<()>);
            impl Drop for ProviderTask {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let mut server = ProviderTask(server);
            let tap = Arc::new(ResponseHeaderTap::default());
            {
                let sending = scope(
                    tap.clone(),
                    send_with_deadline(
                        reqwest::Client::new().get(format!("http://{address}/owned-content")),
                        deadline,
                        Instant::now(),
                    ),
                );
                tokio::pin!(sending);
                tokio::select! {
                    result = &mut sending => panic!("provider withholds every response byte: {result:?}"),
                    result = tokio::time::timeout(Duration::from_secs(5), seen.notified()) => {
                        result.expect("local provider receives GET before the client future is dropped");
                    }
                }
                // 离开作用域会丢弃仍等待响应的正式发送 future。
            }
            assert!(
                tap.http_dispatched(),
                "received GET remains dispatched before any response header"
            );
            assert!(tap.capture().http_dispatched);
            assert!(
                tap.upstream_io_observer().is_none(),
                "download cannot manufacture a model IO exchange"
            );
            assert!(tap.capture().io.is_none());
            release.notify_one();
            tokio::time::timeout(Duration::from_secs(5), &mut server.0)
                .await
                .expect("held local provider finishes after release")
                .expect("local provider task completes without panic");
        }
    }
}
