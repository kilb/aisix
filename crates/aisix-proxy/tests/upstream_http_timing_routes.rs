//! HTTP 分阶段计时的路由组件回归：真实 loopback 上游、router、TelemetryBody 与用量事件。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::usage_io::UpstreamTiming;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{ConnectInfo, FromRequest, Multipart};
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
const MODEL_ID: &str = "11111111-1111-4111-8111-111111111111";

#[derive(Clone, Copy, Debug)]
enum Family {
    Chat,
    Completions,
    Messages,
    ResponsesNative,
    ResponsesChatBridge,
    Transcription,
}

impl Family {
    fn route(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Completions => "/v1/completions",
            Self::Messages => "/v1/messages",
            Self::ResponsesNative | Self::ResponsesChatBridge => "/v1/responses",
            Self::Transcription => "/v1/audio/transcriptions",
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
            Self::Transcription => json!({"model":"clock-model","stream":stream}),
            Self::Completions => {
                json!({"model":"clock-model","prompt":"hello","max_tokens":16,"stream":stream})
            }
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
            Self::Completions => (
                sse(
                    json!({"id":"cmpl-clock","object":"text_completion","model":"upstream-clock","choices":[{"index":0,"text":"","finish_reason":null}]}),
                ),
                sse(
                    json!({"id":"cmpl-clock","object":"text_completion","model":"upstream-clock","choices":[{"index":0,"text":"clock-output","finish_reason":null}]}),
                ),
                format!(
                    "{}data: [DONE]\n\n",
                    sse(
                        json!({"id":"cmpl-clock","object":"text_completion","model":"upstream-clock","choices":[{"index":0,"text":"","finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})
                    )
                ),
            ),
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
            Self::Transcription => (
                sse(json!({"type":"transcript.text.delta","delta":"clock-"})),
                sse(json!({"type":"transcript.text.delta","delta":"output"})),
                sse(json!({"type":"transcript.text.done","text":"clock-output"})),
            ),
        }
    }

    fn buffered_reply(self) -> Value {
        match self {
            Self::Completions => {
                json!({"id":"cmpl-clock","object":"text_completion","model":"upstream-clock","choices":[{"index":0,"text":"clock-output","finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})
            }
            Self::Chat | Self::ResponsesChatBridge => {
                json!({"id":"chat-clock","object":"chat.completion","model":"upstream-clock","choices":[{"index":0,"message":{"role":"assistant","content":"clock-output"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})
            }
            Self::Messages => {
                json!({"id":"msg_clock","type":"message","role":"assistant","model":"upstream-clock","content":[{"type":"text","text":"clock-output"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}})
            }
            Self::ResponsesNative => {
                json!({"id":"resp_clock","object":"response","model":"upstream-clock","status":"completed","output":[{"type":"message","id":"msg_clock","role":"assistant","status":"completed","content":[{"type":"output_text","text":"clock-output","annotations":[]}]}],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}})
            }
            Self::Transcription => json!({"text":"clock-output"}),
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
            let body = if path == Family::Transcription.route() {
                let bad_request = |_| StatusCode::BAD_REQUEST;
                let mut form = Multipart::from_request(request, &())
                    .await
                    .map_err(|_| StatusCode::BAD_REQUEST)?;
                let mut body = json!({});
                while let Some(field) = form.next_field().await.map_err(bad_request)? {
                    match field.name() {
                        Some("model") => {
                            body["model"] = json!(field.text().await.map_err(bad_request)?)
                        }
                        Some("stream") => {
                            body["stream"] =
                                json!(field.text().await.map_err(bad_request)? == "true")
                        }
                        Some("file") => {
                            body["file_bytes"] =
                                json!(field.bytes().await.map_err(bad_request)?.len())
                        }
                        _ => {}
                    }
                }
                body
            } else {
                let bytes = to_bytes(request.into_body(), 1048576)
                    .await
                    .map_err(|_| StatusCode::BAD_REQUEST)?;
                serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?
            };
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
    app_with_hold(base, family, retries, false)
}

fn app_with_hold(
    base: &str,
    family: Family,
    retries: u32,
    held: bool,
) -> TestResult<(axum::Router, mpsc::Receiver<UsageEvent>)> {
    let (state, events) = state_with_hold(base, family, retries, held)?;
    Ok((build_router(state), events))
}

fn state_with_hold(
    base: &str,
    family: Family,
    retries: u32,
    held: bool,
) -> TestResult<(ProxyState, mpsc::Receiver<UsageEvent>)> {
    let snapshot = AisixSnapshot::new();
    if held {
        let guardrail = serde_json::from_value(json!({
            "name":"clock-output-hold","kind":"keyword","hook_point":"output",
            "patterns":[{"kind":"literal","value":"never-match-clock-output"}]
        }))?;
        snapshot
            .guardrails
            .insert(ResourceEntry::new("clock-hold", guardrail, 1));
        let attachment = serde_json::from_value(json!({
            "guardrail_id":"clock-hold","scope_type":"env","priority":100
        }))?;
        snapshot.guardrail_attachments.insert(ResourceEntry::new(
            "clock-hold-attachment",
            attachment,
            1,
        ));
    }
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
        "display_name":"clock-model","provider":provider,
        "model_name":if matches!(family, Family::Transcription) { "gpt-4o-transcribe" } else { "upstream-clock" },
        "provider_key_id":PK,"retries":retries
    }))?;
    snapshot
        .models
        .insert(ResourceEntry::new(MODEL_ID, model, 1));
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
    Ok((state, events))
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

#[tokio::test]
async fn transcript_tool_input_and_bridged_thinking_are_timed_before_the_terminal_frame(
) -> TestResult {
    let chat = |delta| json!({"id":"chat-clock","choices":[{"index":0,"delta":delta}]});
    let cases = [
        (
            Family::Chat,
            chat(json!({"audio":{"transcript":"clock-output"}})),
        ),
        (
            Family::Chat,
            chat(
                json!({"tool_calls":[{"index":0,"type":"custom","id":"call_1","custom":{"name":"run","input":"clock-output"}}]}),
            ),
        ),
        (
            Family::Messages,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"lookup","input":{"q":"clock-output"}}}),
        ),
        (
            Family::ResponsesNative,
            json!({"type":"response.audio.transcript.delta","delta":"clock-output","sequence_number":1}),
        ),
        (
            Family::ResponsesChatBridge,
            chat(
                json!({"content":[{"type":"thinking","thinking":[{"type":"text","text":"clock-output"}]}]}),
            ),
        ),
    ];
    futures::future::try_join_all(cases.into_iter().map(|(family, content)| async move {
        let (metadata, _, end) = family.streaming_parts();
        let reply = Reply {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            header_delay: Duration::ZERO,
            parts: vec![
                (Duration::ZERO, Bytes::from(metadata)),
                (GAP, Bytes::from(sse(content))),
                (GAP, Bytes::from(end)),
            ],
        };
        let server = upstream(vec![reply]).await?;
        let (router, mut events) = app(&server.base, family, 0)?;
        call(router, family, true, Duration::ZERO).await?;
        let event = event(&mut events).await?;
        let timing = event.upstream_timing.ok_or("missing attempt timing")?;
        let metadata = measured(timing.first_protocol_frame_ms, "metadata")?;
        for observed in [
            measured(timing.upstream_first_content_ms, "upstream content")?,
            measured(timing.first_content_ms, "delivered content")?,
        ] {
            assert!(observed >= metadata + 45, "{family:?}: {timing:?}");
            assert!(
                event.upstream_latency_ms >= observed + 45,
                "{family:?}: {timing:?}"
            );
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }))
    .await?;
    Ok(())
}

struct GatewaySocket(tokio::task::JoinHandle<std::io::Result<()>>);

impl Drop for GatewaySocket {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn socket_content_case(family: Family, held: bool) -> TestResult {
    let (metadata, content, end) = family.streaming_parts();
    let reply = Reply {
        status: StatusCode::OK,
        content_type: "text/event-stream",
        header_delay: Duration::ZERO,
        parts: vec![
            (Duration::ZERO, Bytes::from(format!("{metadata}{content}"))),
            (Duration::from_millis(400), Bytes::from(end)),
        ],
    };
    let upstream = upstream(vec![reply]).await?;
    let (router, mut events) = app_with_hold(&upstream.base, family, 0, held)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let _server = GatewaySocket(tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    }));
    let started = Instant::now();
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(DEADLINE)
        .build()?
        .post(format!("http://{address}{}", family.route()))
        .bearer_auth("clock-caller")
        .header("anthropic-version", "2023-06-01")
        .json(&family.request(true))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let returned = tokio::time::timeout(DEADLINE, response.bytes()).await??;
    let completed = started.elapsed().as_millis() as u64;
    assert!(String::from_utf8_lossy(&returned).contains("clock-output"));
    let event = event(&mut events).await?;
    let wire = serde_json::to_value(&event)?;
    let raw = wire
        .pointer("/upstream_timing/upstream_first_content_ms")
        .and_then(Value::as_u64)
        .ok_or("missing raw upstream content clock")?;
    let delivered = wire
        .pointer("/upstream_timing/first_content_ms")
        .and_then(Value::as_u64)
        .ok_or("missing delivered content clock")?;
    assert!(delivered >= raw, "{family:?}: {wire}");
    if held {
        assert!(
            delivered >= raw + 250,
            "hold must remain distinct from upstream arrival: {wire}"
        );
    } else {
        assert!(
            delivered + 250 <= completed,
            "live content must precede upstream EOS: {wire}"
        );
    }
    assert!(events.try_recv().is_err(), "one stream emits one row");
    Ok(())
}

// 真实上游及网关 socket：立刻到达的内容与故意等待 EOS 的输出检查必须分开记时。
#[tokio::test]
async fn raw_upstream_content_precedes_delayed_delivery_on_every_token_route() -> TestResult {
    futures::future::try_join_all(
        [
            Family::Chat,
            Family::Completions,
            Family::Messages,
            Family::ResponsesNative,
            Family::ResponsesChatBridge,
        ]
        .into_iter()
        .flat_map(|family| [false, true].map(move |held| socket_content_case(family, held))),
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
    assert_eq!(timing.upstream_first_content_ms, None);
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

#[tokio::test]
async fn native_messages_stream_request_with_stalled_json_is_not_retried() -> TestResult {
    stream_request_with_stalled_json(Family::Messages).await
}

#[tokio::test]
async fn native_responses_stream_request_with_stalled_json_is_not_retried() -> TestResult {
    stream_request_with_stalled_json(Family::ResponsesNative).await
}

#[tokio::test]
async fn transcription_stream_request_with_stalled_json_is_not_retried() -> TestResult {
    stream_request_with_stalled_json(Family::Transcription).await
}

async fn stream_request_with_stalled_json(family: Family) -> TestResult {
    let mut stalled = Reply::buffered(family);
    stalled.header_delay = Duration::ZERO;
    stalled.parts[0].0 = Duration::ZERO;
    stalled.parts[1].0 = Duration::from_millis(650);
    let mut upstream = upstream(vec![stalled, Reply::buffered(family)]).await?;
    let (state, mut events) = state_with_hold(&upstream.base, family, 1, false)?;
    let snapshot = state.snapshot.load();
    let mut model = (*snapshot
        .models
        .get_by_id(MODEL_ID)
        .ok_or("missing model")?
        .value)
        .clone();
    model.timeout = Some(250);
    model.cooldown = Some(serde_json::from_value(
        json!({"enabled":true,"trigger_on_timeout":true}),
    )?);
    snapshot
        .models
        .insert(ResourceEntry::new(MODEL_ID, model, 2));
    let router = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let _server = GatewaySocket(tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    }));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(DEADLINE)
        .build()?;
    let request = client
        .post(format!("http://{address}{}", family.route()))
        .bearer_auth("clock-caller");
    let request = if matches!(family, Family::Transcription) {
        let wav = b"RIFF\x26\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x40\x1f\x00\x00\x80\x3e\x00\x00\x02\x00\x10\x00data\x02\x00\x00\x00\x00\x00";
        request.multipart(
            reqwest::multipart::Form::new()
                .text("model", "clock-model")
                .text("stream", "true")
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(wav.to_vec())
                        .file_name("sample.wav")
                        .mime_str("audio/wav")?,
                ),
        )
    } else {
        request
            .header("anthropic-version", "2023-06-01")
            .json(&family.request(true))
    };
    let response = request.send().await?;
    let status = response.status();
    let payload = response.text().await?;
    let first = event(&mut events).await?;
    let (path, body) = tokio::time::timeout(DEADLINE, upstream.requests.recv())
        .await?
        .ok_or("missing socket attempt")?;
    assert_eq!(path, family.upstream_route());
    assert_eq!(body["stream"], true);
    if matches!(family, Family::Transcription) {
        assert_eq!(body["model"], "gpt-4o-transcribe");
        assert_eq!(body["file_bytes"], 46);
    }
    let timing = first
        .upstream_timing
        .ok_or("missing upstream HTTP timing")?;
    measured(timing.response_headers_ms, "accepted response headers")?;
    measured(timing.first_response_byte_ms, "initial buffered body bytes")?;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{family:?}: {payload}");
    assert_eq!(first.status_code, 502);
    assert_eq!(first.error_class, "upstream_decode");
    assert!(
        first.error_message.contains("HTTP 200") && first.error_message.contains("timed out"),
        "{}",
        first.error_message
    );
    assert!(first.is_final_attempt);
    assert!(
        upstream.requests.try_recv().is_err(),
        "unexpected upstream replay"
    );
    assert!(events.try_recv().is_err(), "unexpected extra usage event");
    assert_eq!(state.runtime_status.status(MODEL_ID).cooldown_until, None);
    Ok(())
}

#[tokio::test]
async fn translated_buffered_messages_retry_only_before_an_accepted_response() -> TestResult {
    let delay = Duration::from_millis(650);
    for (primary_status, early_headers, timeout, expected_status, expected_attempts) in [
        (StatusCode::OK, true, Some(250), StatusCode::BAD_GATEWAY, 1),
        (StatusCode::OK, false, Some(250), StatusCode::OK, 2),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            true,
            Some(250),
            StatusCode::OK,
            2,
        ),
        (StatusCode::OK, true, None, StatusCode::OK, 1),
    ] {
        let body = Family::Chat.buffered_reply().to_string().into_bytes();
        let primary_body = if primary_status.is_success() {
            body.as_slice()
        } else {
            br#"{"error":{"message":"fixture unavailable"}}"#
        };
        let first = Reply {
            status: primary_status,
            content_type: "application/json",
            header_delay: if early_headers { Duration::ZERO } else { delay },
            parts: vec![
                (Duration::ZERO, Bytes::copy_from_slice(&primary_body[..10])),
                (
                    if early_headers { delay } else { Duration::ZERO },
                    Bytes::copy_from_slice(&primary_body[10..]),
                ),
            ],
        };
        let second = Reply {
            status: StatusCode::OK,
            content_type: "application/json",
            header_delay: Duration::ZERO,
            parts: vec![(Duration::ZERO, Bytes::from(body))],
        };
        let mut upstream = upstream(vec![first, second]).await?;
        let (state, mut events) = state_with_hold(&upstream.base, Family::Chat, 0, false)?;
        let snapshot = state.snapshot.load();
        let primary_id = MODEL_ID;
        let mut primary = (*snapshot
            .models
            .get_by_id(primary_id)
            .ok_or("missing primary")?
            .value)
            .clone();
        primary.display_name = "clock-primary".into();
        primary.model_name = Some("fixture-primary".into());
        let mut fallback = primary.clone();
        fallback.display_name = "clock-fallback".into();
        fallback.model_name = Some("fixture-fallback".into());
        primary.cooldown = Some(serde_json::from_value(json!({
            "enabled":true,"trigger_on_timeout":true
        }))?);
        snapshot
            .models
            .insert(ResourceEntry::new(primary_id, primary, 2));
        snapshot.models.insert(ResourceEntry::new(
            "44444444-4444-4444-8444-444444444444",
            fallback,
            2,
        ));
        let group: Model = serde_json::from_value(json!({
            "display_name":"clock-model","timeout":timeout,
            "routing":{"strategy":"failover","retries":0,"targets":[{"model":"clock-primary"},{"model":"clock-fallback"}]}
        }))?;
        snapshot.models.insert(ResourceEntry::new(
            "55555555-5555-4555-8555-555555555555",
            group,
            2,
        ));
        let router = build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let _server = GatewaySocket(tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        }));
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(DEADLINE)
            .build()?
            .post(format!("http://{address}/v1/messages"))
            .bearer_auth("clock-caller")
            .header("anthropic-version", "2023-06-01")
            .json(&Family::Messages.request(false))
            .send()
            .await?;
        let status = response.status();
        let payload = response.text().await?;
        assert_eq!(
            status, expected_status,
            "primary_status={primary_status}, early_headers={early_headers}, timeout={timeout:?}, body={payload}"
        );
        if status == StatusCode::OK {
            assert!(payload.contains("clock-output"), "{payload}");
        }
        let first = event(&mut events).await?;
        assert_eq!(first.attempt_index, 0);
        assert_eq!(first.model_id, primary_id);
        assert_eq!(first.is_final_attempt, expected_attempts == 1);
        let timing = first.upstream_timing.ok_or("missing primary HTTP timing")?;
        assert_eq!(timing.response_headers_ms.is_some(), early_headers);
        if expected_status == StatusCode::BAD_GATEWAY {
            assert_eq!(first.status_code, 502);
            assert_eq!(first.error_class, "upstream_decode");
            assert!(
                first.error_message.contains("HTTP 200")
                    && first.error_message.contains("timed out"),
                "{}",
                first.error_message
            );
        } else if expected_attempts == 2 {
            assert_eq!(first.status_code, 504);
            assert_eq!(first.error_class, "timeout");
            let second = event(&mut events).await?;
            assert_eq!(second.status_code, 200);
            assert_eq!(second.attempt_index, 1);
            assert_eq!(second.attempt_kind, "fallback");
            assert!(second.is_final_attempt);
        }
        for model in ["fixture-primary", "fixture-fallback"]
            .into_iter()
            .take(expected_attempts)
        {
            let (path, body) = tokio::time::timeout(DEADLINE, upstream.requests.recv())
                .await?
                .ok_or("missing socket attempt")?;
            assert_eq!(path, "/v1/chat/completions");
            assert_eq!(body["model"], model);
        }
        assert!(
            upstream.requests.try_recv().is_err(),
            "unexpected upstream replay"
        );
        assert!(events.try_recv().is_err(), "unexpected extra usage event");
        let runtime = state.runtime_status.status(primary_id);
        assert_eq!(runtime.cooldown_until.is_some(), expected_attempts == 2);
        if !early_headers {
            assert_eq!(runtime.status_reason.as_deref(), Some("request_timeout"));
        }
    }
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
    assert_eq!(failed_timing.upstream_first_content_ms, None);
    assert_eq!(failed_timing.first_content_ms, None);
    let won_timing = won
        .upstream_timing
        .ok_or("retry winner lacks HTTP timing")?;
    raw_phases(won_timing)?;
    let won_frame = measured(won_timing.first_protocol_frame_ms, "retry frame")?;
    let won_chunk = measured(won_timing.first_canonical_chunk_ms, "retry canonical chunk")?;
    let won_content = measured(won_timing.first_content_ms, "retry content")?;
    let won_raw = measured(won_timing.upstream_first_content_ms, "retry raw content")?;
    assert!(won_raw >= won_frame);
    assert!(won_content >= won_raw);
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

async fn completions_timing(parts: Vec<(Duration, Bytes)>) -> TestResult<UpstreamTiming> {
    let mut upstream = upstream(vec![Reply {
        status: StatusCode::OK,
        content_type: "text/event-stream",
        header_delay: GAP,
        parts,
    }])
    .await?;
    let (router, mut events) = app(&upstream.base, Family::Chat, 0)?;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/completions")
        .header("authorization", "Bearer clock-caller")
        .header("content-type", "application/json")
        .extension(ConnectInfo("127.0.0.1:41000".parse::<SocketAddr>()?))
        .body(Body::from(
            json!({"model":"clock-model","prompt":"hello","stream":true,"max_tokens":16})
                .to_string(),
        ))?;
    let response = tokio::time::timeout(DEADLINE, router.oneshot(request)).await??;
    // 完整消费真实 TelemetryBody；这里不将旧 completions 的状态分类作为新契约。
    let _ = tokio::time::timeout(DEADLINE, to_bytes(response.into_body(), 1048576)).await??;
    let event = event(&mut events).await?;
    let (path, body) = tokio::time::timeout(DEADLINE, upstream.requests.recv())
        .await?
        .ok_or("missing completions upstream request")?;
    assert_eq!(path, "/v1/completions");
    assert_eq!(body.get("prompt").and_then(Value::as_str), Some("hello"));
    assert_eq!(body.get("stream").and_then(Value::as_bool), Some(true));
    assert!(events.try_recv().is_err());
    event
        .upstream_timing
        .ok_or_else(|| "completions event lacks HTTP timing".into())
}

// 旧 completions 原样转发字节，不产生 canonical chunk；半帧不能被隐式 hook 计作一个。
#[tokio::test]
async fn opaque_completions_split_sse_has_a_raw_frame_without_a_canonical_chunk() -> TestResult {
    let frame = sse(json!({
        "id":"cmpl-clock","object":"text_completion","model":"upstream-clock",
        "choices":[{"index":0,"text":"clock-output","finish_reason":"stop"}],
        "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
    }));
    let cut = frame.len() / 2;
    let timing = completions_timing(vec![
        (GAP, Bytes::copy_from_slice(&frame.as_bytes()[..cut])),
        (GAP, Bytes::copy_from_slice(&frame.as_bytes()[cut..])),
        (Duration::ZERO, Bytes::from_static(b"data: [DONE]\n\n")),
    ])
    .await?;
    let (_, _, byte) = raw_phases(timing)?;
    let frame = measured(timing.first_protocol_frame_ms, "complete completions frame")?;
    assert!(
        frame >= byte + 45,
        "the raw frame must wait for the separately delivered terminator"
    );
    assert_eq!(
        timing.first_canonical_chunk_ms, None,
        "opaque byte forwarding must not manufacture a canonical chunk"
    );
    Ok(())
}

// EOF 前只有未闭合事件时，首字节仍可测，raw frame 与 canonical 均应未知。
#[tokio::test]
async fn opaque_completions_unterminated_sse_has_no_frame_or_canonical_chunk() -> TestResult {
    let timing = completions_timing(vec![(
        GAP,
        Bytes::from_static(
            b"data: {\"id\":\"cmpl-clock\",\"choices\":[{\"index\":0,\"text\":\"clock-output\"}]}",
        ),
    )])
    .await?;
    raw_phases(timing)?;
    assert_eq!(timing.first_protocol_frame_ms, None);
    assert_eq!(
        timing.first_canonical_chunk_ms, None,
        "an unterminated byte fragment must not become a canonical chunk"
    );
    Ok(())
}
