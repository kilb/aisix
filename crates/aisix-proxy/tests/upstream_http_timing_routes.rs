//! HTTP 分阶段计时的路由组件回归：真实 loopback 上游、router、TelemetryBody 与用量事件。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::usage_io::UpstreamTiming;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tower::ServiceExt;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const GAP: Duration = Duration::from_millis(60);
const ENTRY_WAIT: Duration = Duration::from_millis(300);
const DEADLINE: Duration = Duration::from_secs(10);
const PK: &str = "22222222-2222-4222-8222-222222222222";

#[derive(Clone, Copy, Debug)]
enum Family {
    Chat,
    Messages,
    ResponsesNative,
    ResponsesChatBridge,
}

impl Family {
    fn route(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Messages => "/v1/messages",
            Self::ResponsesNative | Self::ResponsesChatBridge => "/v1/responses",
        }
    }

    fn upstream_route(self) -> &'static str {
        match self {
            Self::ResponsesChatBridge => "/v1/chat/completions",
            _ => self.route(),
        }
    }

    fn provider(self) -> &'static str {
        match self {
            Self::Messages => "anthropic",
            Self::ResponsesChatBridge => "component-compatible",
            _ => "openai",
        }
    }

    fn request(self, stream: bool) -> Value {
        match self {
            Self::ResponsesNative | Self::ResponsesChatBridge => {
                json!({"model":"clock-model","input":"hello","stream":stream})
            }
            _ => {
                json!({"model":"clock-model","messages":[{"role":"user","content":"hello"}],"max_tokens":16,"stream":stream})
            }
        }
    }

    fn streaming_parts(self) -> (String, String, String) {
        match self {
            Self::Chat | Self::ResponsesChatBridge => (
                sse(
                    json!({"id":"chat-clock","object":"chat.completion.chunk","model":"upstream-clock","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}),
                ),
                sse(
                    json!({"id":"chat-clock","object":"chat.completion.chunk","model":"upstream-clock","choices":[{"index":0,"delta":{"content":"clock-output"},"finish_reason":null}]}),
                ),
                format!(
                    "{}data: [DONE]\n\n",
                    sse(
                        json!({"id":"chat-clock","object":"chat.completion.chunk","model":"upstream-clock","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})
                    )
                ),
            ),
            Self::Messages => (
                sse(
                    json!({"type":"message_start","message":{"id":"msg_clock","type":"message","role":"assistant","model":"upstream-clock","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
                ),
                format!(
                    "{}{}",
                    sse(
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
                    ),
                    sse(
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"clock-output"}})
                    )
                ),
                format!(
                    "{}{}{}",
                    sse(json!({"type":"content_block_stop","index":0})),
                    sse(
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}})
                    ),
                    sse(json!({"type":"message_stop"}))
                ),
            ),
            Self::ResponsesNative => (
                sse(
                    json!({"type":"response.created","response":{"id":"resp_clock","object":"response","model":"upstream-clock","status":"in_progress","output":[]}}),
                ),
                sse(
                    json!({"type":"response.output_text.delta","item_id":"msg_clock","output_index":0,"content_index":0,"delta":"clock-output"}),
                ),
                sse(
                    json!({"type":"response.completed","response":{"id":"resp_clock","object":"response","model":"upstream-clock","status":"completed","output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}}),
                ),
            ),
        }
    }

    fn buffered_reply(self) -> Value {
        match self {
            Self::Chat | Self::ResponsesChatBridge => {
                json!({"id":"chat-clock","object":"chat.completion","model":"upstream-clock","choices":[{"index":0,"message":{"role":"assistant","content":"clock-output"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})
            }
            Self::Messages => {
                json!({"id":"msg_clock","type":"message","role":"assistant","model":"upstream-clock","content":[{"type":"text","text":"clock-output"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}})
            }
            Self::ResponsesNative => {
                json!({"id":"resp_clock","object":"response","model":"upstream-clock","status":"completed","output":[{"type":"message","id":"msg_clock","role":"assistant","status":"completed","content":[{"type":"output_text","text":"clock-output","annotations":[]}]}],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}})
            }
        }
    }
}

fn sse(value: Value) -> String {
    match value.get("type").and_then(Value::as_str) {
        Some(kind) => format!("event: {kind}\ndata: {value}\n\n"),
        None => format!("data: {value}\n\n"),
    }
}

struct Reply {
    status: StatusCode,
    content_type: &'static str,
    header_delay: Duration,
    parts: Vec<(Duration, Bytes)>,
}

impl Reply {
    fn streaming(family: Family) -> Self {
        let (metadata, content, end) = family.streaming_parts();
        let cut = metadata.len() / 2;
        Self {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            header_delay: GAP,
            parts: vec![
                (GAP, Bytes::copy_from_slice(&metadata.as_bytes()[..cut])),
                (GAP, Bytes::copy_from_slice(&metadata.as_bytes()[cut..])),
                (GAP, Bytes::from(content)),
                (Duration::ZERO, Bytes::from(end)),
            ],
        }
    }

    fn buffered(family: Family) -> Self {
        let body = family.buffered_reply().to_string().into_bytes();
        let cut = body.len() / 2;
        Self {
            status: StatusCode::OK,
            content_type: "application/json",
            header_delay: GAP,
            parts: vec![
                (GAP, Bytes::copy_from_slice(&body[..cut])),
                (GAP, Bytes::copy_from_slice(&body[cut..])),
            ],
        }
    }
}

struct Upstream {
    base: String,
    requests: mpsc::Receiver<(String, Value)>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn upstream(replies: Vec<Reply>) -> TestResult<Upstream> {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let (seen, requests) = mpsc::channel(8);
    let router = axum::Router::new().fallback(move |request: Request<Body>| {
        let replies = Arc::clone(&replies);
        let seen = seen.clone();
        async move {
            let path = request.uri().path().to_owned();
            let bytes = to_bytes(request.into_body(), 1048576)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            let body = serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
            seen.send((path, body))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let reply = replies
                .lock()
                .await
                .pop_front()
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
            tokio::time::sleep(reply.header_delay).await;
            let body = futures::stream::iter(reply.parts).then(|(delay, bytes)| async move {
                tokio::time::sleep(delay).await;
                Ok::<_, std::io::Error>(bytes)
            });
            Response::builder()
                .status(reply.status)
                .header("content-type", reply.content_type)
                .body(Body::from_stream(body))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok(Upstream {
        base: format!("http://{address}/v1"),
        requests,
        task,
    })
}

fn app(
    base: &str,
    family: Family,
    retries: u32,
) -> TestResult<(axum::Router, mpsc::Receiver<UsageEvent>)> {
    let snapshot = AisixSnapshot::new();
    let provider = family.provider();
    let key: ProviderKey = serde_json::from_value(json!({
        "display_name":"clock component upstream","api_key":"component-only-upstream-key",
        "provider":provider,"adapter":if matches!(family, Family::Messages) { "anthropic" } else { "openai" },
        "api_base":base
    }))?;
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PK, key, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"clock-model","provider":provider,"model_name":"upstream-clock",
        "provider_key_id":PK,"retries":retries
    }))?;
    snapshot.models.insert(ResourceEntry::new(
        "11111111-1111-4111-8111-111111111111",
        model,
        1,
    ));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("clock-caller"),"allowed_models":["clock-model"]
    }))?;
    snapshot.apikeys.insert(ResourceEntry::new(
        "33333333-3333-4333-8333-333333333333",
        key,
        1,
    ));
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    hub.register_specialized(
        "anthropic",
        Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
    );
    // 与服务启动保持一致，按 adapter 为自定义供应商选择正式协议桥。
    hub.register_family(
        aisix_core::Adapter::Openai,
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    hub.register_family(
        aisix_core::Adapter::Anthropic,
        Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
    );
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"}))?;
    let (tx, events) = mpsc::channel(8);
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_usage_sink(UsageSink::new(tx));
    Ok((build_router(state), events))
}

async fn call(
    router: axum::Router,
    family: Family,
    stream: bool,
    entry_wait: Duration,
) -> TestResult<Vec<u8>> {
    let body = Bytes::from(family.request(stream).to_string());
    let request_body = futures::stream::once(async move {
        tokio::time::sleep(entry_wait).await;
        Ok::<_, std::io::Error>(body)
    });
    let request = Request::builder()
        .method("POST")
        .uri(family.route())
        .header("authorization", "Bearer clock-caller")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .extension(ConnectInfo("127.0.0.1:41000".parse::<SocketAddr>()?))
        .body(Body::from_stream(request_body))?;
    let response = tokio::time::timeout(DEADLINE, router.oneshot(request)).await??;
    let status = response.status();
    let returned =
        tokio::time::timeout(DEADLINE, to_bytes(response.into_body(), 1048576)).await??;
    assert_eq!(status, StatusCode::OK, "{family:?}: {:?}", returned);
    assert!(String::from_utf8_lossy(&returned).contains("clock-output"));
    Ok(returned.to_vec())
}

async fn event(events: &mut mpsc::Receiver<UsageEvent>) -> TestResult<UsageEvent> {
    tokio::time::timeout(DEADLINE, events.recv())
        .await?
        .ok_or_else(|| "missing usage event".into())
}

fn measured(value: Option<u32>, phase: &str) -> TestResult<u32> {
    value.ok_or_else(|| format!("missing {phase}").into())
}

fn raw_phases(timing: UpstreamTiming) -> TestResult<(u32, u32, u32)> {
    let send = measured(timing.http_send_started_ms, "HTTP send")?;
    let headers = measured(timing.response_headers_ms, "response headers")?;
    let byte = measured(timing.first_response_byte_ms, "first response byte")?;
    assert!(
        headers >= send + 45,
        "the delayed response head must be measured independently"
    );
    assert!(
        byte >= headers + 45,
        "the body pause after the head must stay visible"
    );
    Ok((send, headers, byte))
}

async fn streaming_case(family: Family) -> TestResult {
    let mut upstream = upstream(vec![Reply::streaming(family)]).await?;
    let (router, mut events) = app(&upstream.base, family, 0)?;
    call(router, family, true, ENTRY_WAIT).await?;
    let event = event(&mut events).await?;
    assert_eq!(event.status_code, 200);
    assert!(event.is_final_attempt);
    let timing = event
        .upstream_timing
        .ok_or("streaming event lacks HTTP timing")?;
    let (_, _, byte) = raw_phases(timing)?;
    let frame = measured(timing.first_protocol_frame_ms, "complete protocol frame")?;
    let canonical = measured(timing.first_canonical_chunk_ms, "canonical chunk")?;
    let content = measured(timing.first_content_ms, "first delivered content")?;
    assert!(
        frame >= byte + 45,
        "an incomplete SSE fragment must not stop the frame clock"
    );
    assert!(canonical >= frame);
    assert!(
        content >= canonical + 45,
        "metadata must not become generated content"
    );
    let first_token = event
        .first_token_latency_ms
        .ok_or("missing entry-to-content timing")?;
    assert!(
        first_token >= content + 250,
        "content offset must use the attempt origin, excluding the delayed caller body"
    );
    let pre_handler = event
        .request_timing
        .and_then(|timing| timing.pre_handler_ms)
        .ok_or("missing request timing")?;
    assert!(pre_handler >= 250);
    let (path, body) = tokio::time::timeout(DEADLINE, upstream.requests.recv())
        .await?
        .ok_or("missing upstream request")?;
    assert_eq!(path, family.upstream_route());
    assert_eq!(body.get("stream").and_then(Value::as_bool), Some(true));
    assert!(
        events.try_recv().is_err(),
        "one exchange must emit one token usage row"
    );
    Ok(())
}

// 移除任一兄弟路由的阶段观察、混用入口起点或忽略交付后的事件补全，都必须报红。
#[tokio::test]
async fn token_route_families_publish_all_six_http_stages_from_the_attempt_origin() -> TestResult {
    futures::future::try_join_all(
        [
            Family::Chat,
            Family::Messages,
            Family::ResponsesNative,
            Family::ResponsesChatBridge,
        ]
        .into_iter()
        .map(streaming_case),
    )
    .await?;
    Ok(())
}

async fn buffered_case(family: Family) -> TestResult {
    let mut upstream = upstream(vec![Reply::buffered(family)]).await?;
    let (router, mut events) = app(&upstream.base, family, 0)?;
    call(router, family, false, Duration::ZERO).await?;
    let event = event(&mut events).await?;
    let timing = event
        .upstream_timing
        .ok_or("buffered event lacks HTTP timing")?;
    raw_phases(timing)?;
    assert_eq!(
        timing.first_protocol_frame_ms, None,
        "JSON has no streamed protocol frame"
    );
    assert_eq!(timing.first_canonical_chunk_ms, None);
    assert_eq!(timing.first_content_ms, None);
    let (path, body) = tokio::time::timeout(DEADLINE, upstream.requests.recv())
        .await?
        .ok_or("missing upstream request")?;
    assert_eq!(path, family.upstream_route());
    assert_ne!(body.get("stream").and_then(Value::as_bool), Some(true));
    assert!(events.try_recv().is_err());
    Ok(())
}

// 非流式也必须保留 HTTP 分段，不能拿完整 JSON 的耗时伪造流式首帧。
#[tokio::test]
async fn buffered_token_routes_publish_raw_http_stages_without_stream_samples() -> TestResult {
    futures::future::try_join_all(
        [
            Family::Chat,
            Family::Messages,
            Family::ResponsesNative,
            Family::ResponsesChatBridge,
        ]
        .into_iter()
        .map(buffered_case),
    )
    .await?;
    Ok(())
}

// 首个失败交换故意慢很多；重试成功的行不能继承它的起点、head 或内容时点。
#[tokio::test]
async fn retry_usage_rows_keep_their_own_http_exchange_and_content_ownership() -> TestResult {
    let family = Family::ResponsesChatBridge;
    let failed = Reply {
        status: StatusCode::SERVICE_UNAVAILABLE,
        content_type: "application/json",
        header_delay: Duration::from_millis(800),
        parts: vec![(
            Duration::ZERO,
            Bytes::from(
                json!({"error":{"type":"server_error","message":"temporary component outage"}})
                    .to_string(),
            ),
        )],
    };
    let mut upstream = upstream(vec![failed, Reply::streaming(family)]).await?;
    let (router, mut events) = app(&upstream.base, family, 1)?;
    call(router, family, true, Duration::ZERO).await?;
    let failed = event(&mut events).await?;
    let won = event(&mut events).await?;
    assert_eq!(failed.attempt_index, 0);
    assert!(!failed.is_final_attempt);
    assert_eq!(failed.status_code, 502);
    assert_eq!(won.attempt_index, 1);
    assert!(won.is_final_attempt);
    assert_eq!(won.status_code, 200);
    let failed_timing = failed
        .upstream_timing
        .ok_or("failed attempt lacks HTTP timing")?;
    let failed_send = measured(failed_timing.http_send_started_ms, "failed send")?;
    let failed_headers = measured(failed_timing.response_headers_ms, "failed headers")?;
    assert!(failed_headers >= failed_send + 750);
    assert!(failed_timing.first_response_byte_ms.is_some());
    assert_eq!(failed_timing.first_protocol_frame_ms, None);
    assert_eq!(failed_timing.first_canonical_chunk_ms, None);
    assert_eq!(failed_timing.first_content_ms, None);
    let won_timing = won
        .upstream_timing
        .ok_or("retry winner lacks HTTP timing")?;
    raw_phases(won_timing)?;
    let won_frame = measured(won_timing.first_protocol_frame_ms, "retry frame")?;
    let won_chunk = measured(won_timing.first_canonical_chunk_ms, "retry canonical chunk")?;
    let won_content = measured(won_timing.first_content_ms, "retry content")?;
    assert!(won_chunk >= won_frame);
    assert!(won_content >= won_chunk + 45);
    assert!(
        won_content < failed_headers,
        "retry offsets must restart rather than include the first attempt's 800ms wait"
    );
    assert!(
        won.first_token_latency_ms
            .ok_or("missing request first content")?
            >= won_content + 750
    );
    for _ in 0..2 {
        let (path, _) = tokio::time::timeout(DEADLINE, upstream.requests.recv())
            .await?
            .ok_or("missing retry upstream request")?;
        assert_eq!(path, family.upstream_route());
    }
    assert!(events.try_recv().is_err());
    Ok(())
}
