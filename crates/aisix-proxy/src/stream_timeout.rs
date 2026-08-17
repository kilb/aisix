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
    #[error(transparent)]
    Upstream(#[from] reqwest::Error),
    #[error("upstream stream read timed out after {elapsed_ms} ms")]
    Timeout { elapsed_ms: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawStreamFailure {
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
        }
    }

    pub(crate) fn into_bridge(self, started: Instant) -> BridgeError {
        match self {
            Self::Timeout { elapsed_ms } => BridgeError::Timeout {
                elapsed_ms,
                cause: String::new(),
            },
            Self::Upstream(error) => crate::dispatch::reqwest_error_to_bridge(&error, started),
        }
    }
}

impl RawStreamFailure {
    pub(crate) fn bridge_error(self) -> BridgeError {
        match self {
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
) -> StreamTerminalStatus {
    if let Some(error) = failure {
        let status = error.http_status();
        let error_class = error.error_type().to_string();
        if status >= 500 {
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
        runtime_status.mark_healthy(model_id);
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
    match deadline {
        Some(d) => match tokio::time::timeout(d, req.send()).await {
            Ok(res) => res.map_err(|e| crate::dispatch::reqwest_error_to_bridge(&e, started)),
            Err(_) => Err(BridgeError::Timeout {
                elapsed_ms: started.elapsed().as_millis() as u64,
                cause: String::new(),
            }),
        },
        None => req
            .send()
            .await
            .map_err(|e| crate::dispatch::reqwest_error_to_bridge(&e, started)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            &health, &runtime, "model", "model-id", None, None, false, 200,
        );
        assert_eq!(dropped.status, crate::CLIENT_CLOSED_REQUEST);
        assert_eq!(health.level("model"), crate::health::HealthLevel::Degraded);

        let completed = finish_bridge_stream(
            &health, &runtime, "model", "model-id", None, None, true, 200,
        );
        assert_eq!(completed.status, 200);
        assert_eq!(health.level("model"), crate::health::HealthLevel::Healthy);
    }

    #[test]
    fn gateway_completed_stream_uses_its_terminal_status() {
        let health = crate::health::HealthTracker::new();
        let runtime = crate::health::ModelRuntimeStatusTracker::new();

        let completed = finish_bridge_stream(
            &health, &runtime, "model", "model-id", None, None, true, 422,
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
