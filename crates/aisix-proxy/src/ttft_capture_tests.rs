// 验证真实桥接链的计时边界：早到的协议首帧不能被后到的正文替换。
// 首帧与正文故意隔开；删除桥对首帧的观测必须令 chat / responses 用例失败。
// 协议顺序：https://platform.claude.com/docs/en/build-with-claude/streaming#event-types

const BODY_DELAY: std::time::Duration = std::time::Duration::from_millis(1_200);

async fn early_start_late_text(endpoint: &str) -> aisix_obs::UsageEvent {
    use aisix_provider_anthropic::AnthropicBridge;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/messages",
                axum::routing::post(|| async {
                    let bytes = async_stream::stream! {
                        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_first_frame\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-haiku-20241022\",\"stop_reason\":null,\"usage\":{\"input_tokens\":13,\"output_tokens\":0}}}\n\n",
                        ));
                        tokio::time::sleep(BODY_DELAY).await;
                        yield Ok(bytes::Bytes::from_static(
                            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                        ));
                    };
                    axum::http::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(bytes))
                        .unwrap()
                }),
            ),
        )
        .await
        .unwrap();
    });

    let snapshot = AisixSnapshot::new();
    snapshot
        .provider_keys
        .insert(matrix_anthropic_pk(&format!("http://{address}")));
    snapshot.models.insert(anthropic_model_entry("first-frame"));
    snapshot
        .apikeys
        .insert(apikey_entry("sk-caller", &["first-frame"]));
    let hub = Arc::new(Hub::new());
    hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let app =
        build_router(build_state(snapshot, hub).with_usage_sink(aisix_obs::UsageSink::new(tx)));
    let body = if endpoint == "/v1/responses" {
        serde_json::json!({
            "model":"first-frame", "input":"hi", "stream":true,
            "max_output_tokens":16
        })
    } else {
        serde_json::json!({
            "model":"first-frame", "messages":[{"role":"user","content":"hi"}],
            "max_tokens":16, "stream":true
        })
    };
    let response = app
        .oneshot(cancellable_request(endpoint, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = String::from_utf8(
        to_bytes(response.into_body(), 65_536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let event = next_event(&mut rx).await;
    upstream.abort();

    assert!(
        delivered.contains("answer"),
        "正文必须完整交付：{delivered}"
    );
    assert_eq!(event.status_code, 200);
    assert!(event.is_final_attempt);
    assert_eq!(event.prompt_tokens, 13);
    assert_eq!(event.completion_tokens, 5);
    assert!(rx.try_recv().is_err(), "一次请求只能结算一次");

    let first_content = event
        .first_token_latency_ms
        .expect("真正交付了非空正文，首内容时刻必须已测量");
    assert!(
        first_content >= 1_200,
        "首内容不能借用早到的元数据帧：{first_content}ms"
    );
    assert!(event.upstream_latency_ms >= 1_200);
    assert!(event.upstream_ttft_ms > 0, "首帧确实已经到达");
    assert!(
        event.upstream_ttft_ms < 600,
        "{endpoint}: 首协议帧已早到，不能等到正文才停表：{}ms，首内容 {first_content}ms",
        event.upstream_ttft_ms
    );
    event
}

#[tokio::test]
async fn native_messages_first_frame_precedes_the_first_content() {
    early_start_late_text("/v1/messages").await;
}

#[tokio::test]
async fn bridged_chat_first_frame_precedes_the_first_content() {
    early_start_late_text("/v1/chat/completions").await;
}

#[tokio::test]
async fn bridged_responses_first_frame_precedes_the_first_content() {
    early_start_late_text("/v1/responses").await;
}

async fn buffered_structured_output_has_no_upstream_ttft(endpoint: &str) {
    use aisix_provider_anthropic::AnthropicBridge;

    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_partial_json(
            serde_json::json!({"stream":false}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(BODY_DELAY)
                .set_body_json(serde_json::json!({
                    "id":"msg_buffered", "type":"message", "role":"assistant",
                    "model":"claude-3-5-haiku-20241022",
                    "content":[{"type":"tool_use","id":"toolu_json", "name":"json_tool_call",
                        "input":{"answer":"ok"}}],
                    "stop_reason":"tool_use", "usage":{"input_tokens":13,"output_tokens":5}
                })),
        )
        .mount(&upstream)
        .await;
    let snapshot = AisixSnapshot::new();
    snapshot
        .provider_keys
        .insert(matrix_anthropic_pk(&upstream.uri()));
    snapshot
        .models
        .insert(anthropic_model_entry("buffered-clock"));
    snapshot
        .apikeys
        .insert(apikey_entry("sk-caller", &["buffered-clock"]));
    let hub = Arc::new(Hub::new());
    hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let state = build_state(snapshot, hub).with_usage_sink(aisix_obs::UsageSink::new(tx));
    let metrics = state.metrics.clone();
    let schema = serde_json::json!({"type":"object","properties":{"answer":{"type":"string"}}});
    let body = if endpoint == "/v1/responses" {
        serde_json::json!({
            "model":"buffered-clock", "input":"hi", "stream":true,
            "text":{"format":{"type":"json_schema","name":"answer","schema":schema}}
        })
    } else {
        serde_json::json!({
            "model":"buffered-clock", "messages":[{"role":"user","content":"hi"}],
            "stream":true,
            "response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":schema}}
        })
    };
    let response = build_router(state)
        .oneshot(cancellable_request(endpoint, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = String::from_utf8(
        to_bytes(response.into_body(), 65_536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let event = next_event(&mut rx).await;
    assert!(
        delivered.contains("ok"),
        "完整结构化输出不能丢失：{delivered}"
    );
    assert_eq!(event.status_code, 200);
    assert_eq!((event.prompt_tokens, event.completion_tokens), (13, 5));
    assert!(event
        .first_token_latency_ms
        .is_some_and(|value| value >= 1_200));
    assert!(event.upstream_latency_ms >= 1_200);
    assert_eq!(
        event.upstream_ttft_ms, 0,
        "非流式上游没有可测首流帧，不能拿完整耗时冒充"
    );
    assert!(!serde_json::to_value(&event)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("upstream_ttft_ms"));
    assert!(rx.try_recv().is_err(), "一次请求只能结算一次");
    let rendered = metrics.render();
    assert!(
        !rendered.lines().any(|line| {
            line.starts_with("aisix_request_ttft_seconds_count{")
                && line.contains(&format!("endpoint=\"{endpoint}\""))
                && line.contains("side=\"upstream\"")
        }),
        "非流式上游不得产生首流帧指标：{rendered}"
    );
}

#[tokio::test]
async fn buffered_chat_output_does_not_report_complete_latency_as_upstream_ttft() {
    buffered_structured_output_has_no_upstream_ttft("/v1/chat/completions").await;
}

#[tokio::test]
async fn buffered_responses_output_does_not_report_complete_latency_as_upstream_ttft() {
    buffered_structured_output_has_no_upstream_ttft("/v1/responses").await;
}

// 删除原生模拟流的计时豁免必须因伪造首帧值或指标样本变红；真实 HTTP 交付仍须完整计费。
#[tokio::test]
async fn native_messages_synthetic_json_has_no_upstream_ttft() {
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream_listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        axum::serve(
            upstream_listener,
            Router::new().fallback(
                |uri: axum::http::Uri, axum::Json(body): axum::Json<serde_json::Value>| async move {
                    assert!(uri.path().ends_with(":rawPredict"), "{uri}");
                    assert_ne!(body["stream"], true, "上游必须实际非流式：{body}");
                    assert_eq!(body["tools"][0]["name"], "json_tool_call");
                    tokio::time::sleep(BODY_DELAY).await;
                    axum::Json(serde_json::json!({
                        "id":"msg_native_buffered", "type":"message", "role":"assistant",
                        "model":"claude-3-5-haiku@20241022",
                        "content":[{"type":"tool_use","id":"toolu_json","name":"json_tool_call",
                            "input":{"answer":"ok"}}],
                        "stop_reason":"tool_use", "stop_sequence":null,
                        "usage":{"input_tokens":13,"output_tokens":5,
                            "cache_read_input_tokens":7,"cache_creation_input_tokens":11}
                    }))
                },
            ),
        )
        .await
        .unwrap();
    });

    let snapshot = AisixSnapshot::new();
    snapshot.provider_keys.insert(ResourceEntry::new(
        "pk-native-buffered",
        serde_json::from_value::<aisix_core::ProviderKey>(serde_json::json!({
            "display_name":"native-buffered", "provider":"google", "adapter":"vertex",
            "api_base":format!("http://{upstream_address}"),
            "secret":serde_json::json!({"access_token":"local-test-token",
                "project":"local-test-project","region":"us-east5"}).to_string()
        }))
        .unwrap(),
        1,
    ));
    snapshot.models.insert(ResourceEntry::new(
        "model-native-buffered",
        serde_json::from_value::<Model>(serde_json::json!({
            "display_name":"native-buffered", "provider":"google",
            "model_name":"claude-3-5-haiku@20241022", "provider_key_id":"pk-native-buffered",
            "cost":{"input_per_1k":10.0,"output_per_1k":20.0,
                "cached_input_per_1k":1.0,"cache_write_per_1k":15.0}
        }))
        .unwrap(),
        1,
    ));
    snapshot
        .apikeys
        .insert(apikey_entry("sk-caller", &["native-buffered"]));
    let hub = Arc::new(Hub::new());
    hub.register_family(
        aisix_core::Adapter::Vertex,
        Arc::new(aisix_provider_vertex::VertexBridge::with_client(
            aisix_gateway::client_builder().no_proxy().build().unwrap(),
        )),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut config = cfg();
    config.expose_debug_headers = false;
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_platform(crate::platform::PlatformSettings::from_secret(
            "native-buffered-test",
        ))
        .with_usage_sink(aisix_obs::UsageSink::new(tx));
    let metrics = state.metrics.clone();
    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_address = gateway_listener.local_addr().unwrap();
    let gateway = tokio::spawn(async move {
        axum::serve(gateway_listener, build_router(state))
            .await
            .unwrap();
    });

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let started = std::time::Instant::now();
    let response = client
        .post(format!("http://{gateway_address}/v1/messages"))
        .bearer_auth("sk-caller")
        .header("anthropic-version", "2023-06-01")
        .timeout(std::time::Duration::from_secs(10))
        .json(&serde_json::json!({
            "model":"native-buffered", "stream":true, "max_tokens":32,
            "messages":[{"role":"user","content":"hi"}],
            "output_config":{"format":{"type":"json_schema",
                "schema":{"type":"object","properties":{"answer":{"type":"string"}},
                    "required":["answer"],"additionalProperties":false}}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let mut body = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut payloads = Vec::<serde_json::Value>::new();
    let mut first_content = None;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.unwrap();
        for frame in decoder.feed(chunk.as_ref()).unwrap() {
            let SseEvent::Data(data) = frame else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&data).unwrap();
            if first_content.is_none()
                && value["type"] == "content_block_delta"
                && value["delta"]["type"] == "text_delta"
                && value["delta"]["text"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            {
                first_content = Some(started.elapsed());
            }
            payloads.push(value);
        }
    }
    let event = next_event(&mut rx).await;
    gateway.abort();
    upstream.abort();
    let first_content = first_content.expect("真实 HTTP 客户端必须收到非空内容");
    assert!(first_content >= BODY_DELAY);
    assert_eq!(payloads.first().unwrap()["type"], "message_start");
    assert_eq!(payloads.last().unwrap()["type"], "message_stop");
    let text: String = payloads
        .iter()
        .filter_map(|value| value["delta"]["text"].as_str())
        .collect();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        serde_json::json!({"answer":"ok"})
    );
    assert!(payloads
        .iter()
        .any(|value| value["delta"]["stop_reason"] == "end_turn"));
    assert_eq!(event.status_code, 200);
    assert!(event.is_final_attempt);
    assert_eq!((event.prompt_tokens, event.completion_tokens), (13, 5));
    assert_eq!(
        (event.cache_read_tokens, event.cache_creation_tokens),
        (7, 11)
    );
    assert_eq!(event.cached_prompt_tokens, 0);
    // 13×10/1000 + 5×20/1000 + 7×1/1000 + 11×15/1000 = 0.402 美元。
    assert!((event.cost_usd - 0.402).abs() < 1e-9, "{}", event.cost_usd);
    assert!(event
        .first_token_latency_ms
        .is_some_and(|value| value >= 1_200));
    assert!(event.downstream_latency_ms >= 1_200);
    assert!(event.upstream_latency_ms >= 1_200);
    assert_eq!(event.upstream_ttft_ms, 0, "模拟流不能伪造上游首帧");
    assert!(!serde_json::to_value(&event)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("upstream_ttft_ms"));
    assert!(rx.try_recv().is_err(), "一次请求只能结算一次");
    let rendered = metrics.render();
    assert!(
        !rendered.lines().any(|line| {
            line.starts_with("aisix_request_ttft_seconds_count{")
                && line.contains("endpoint=\"/v1/messages\"")
                && line.contains("side=\"upstream\"")
        }),
        "非流式上游不得产生首流帧指标：{rendered}"
    );
}

const METADATA_FRAME: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_boundary\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-haiku-20241022\",\"stop_reason\":null,\"usage\":{\"input_tokens\":13,\"output_tokens\":0}}}\n\n";
const COMPLETE_TEXT_FRAMES: &[u8] = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

fn boundary_body(endpoint: &str, input: &str) -> serde_json::Value {
    if endpoint == "/v1/responses" {
        serde_json::json!({
            "model":"boundary-clock", "input":input, "stream":true,
            "max_output_tokens":16
        })
    } else {
        serde_json::json!({
            "model":"boundary-clock", "messages":[{"role":"user","content":input}],
            "max_tokens":16, "stream":true
        })
    }
}

fn boundary_snapshot(api_base: &str, peek: bool) -> AisixSnapshot {
    let snapshot = AisixSnapshot::new();
    snapshot.provider_keys.insert(matrix_anthropic_pk(api_base));
    let mut model = anthropic_model_entry("boundary-clock");
    let value = Arc::make_mut(&mut model.value);
    value.retries = Some(0);
    if peek {
        // 只打开已有的首 chunk 预读分支；测试由通道放行结束，不靠超时终止。
        value.stream_timeout = Some(10_000);
    }
    snapshot.models.insert(model);
    snapshot
        .apikeys
        .insert(apikey_entry("sk-caller", &["boundary-clock"]));
    snapshot
}

async fn assert_usage_channel_finished(
    rx: &mut tokio::sync::mpsc::Receiver<aisix_obs::UsageEvent>,
) {
    let extra = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("所有请求 guard 释放后 usage 通道必须关闭");
    assert!(extra.is_none(), "一次请求只能结算一次：{extra:?}");
}

async fn slow_upload_keeps_the_arrival_and_handler_clocks_distinct(endpoint: &str) {
    use aisix_provider_anthropic::AnthropicBridge;

    let capture = crate::test_log::Capture::install();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let upstream = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/messages",
                axum::routing::post(move |body: bytes::Bytes| {
                    let request_tx = request_tx.clone();
                    async move {
                        request_tx.send(body).unwrap();
                        let mut complete = METADATA_FRAME.to_vec();
                        complete.extend_from_slice(COMPLETE_TEXT_FRAMES);
                        axum::http::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(Body::from(complete))
                            .unwrap()
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let hub = Arc::new(Hub::new());
    hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let state = build_state(boundary_snapshot(&format!("http://{address}"), false), hub)
        .with_usage_sink(aisix_obs::UsageSink::new(tx));
    // 保留原客户端字节，包括空白与多字节字符；io_audit v2 则统计转换后的上游实体。
    let original = format!(" \n{}\n ", boundary_body(endpoint, "A汉😀"));
    let original_len = original.len() as u64;
    let wire = bytes::Bytes::from(original);
    let split = wire.len() / 2;
    let first = wire.slice(..split);
    let rest = wire.slice(split..);
    let mut request = cancellable_request(endpoint, serde_json::Value::Null);
    *request.body_mut() = Body::from_stream(async_stream::stream! {
        yield Ok::<_, std::io::Error>(first);
        tokio::time::sleep(BODY_DELAY).await;
        yield Ok(rest);
    });
    let response = build_router(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = String::from_utf8(
        to_bytes(response.into_body(), 65_536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let event = next_event(&mut rx).await;
    let received = requests.try_recv().expect("真实 HTTP 上游必须收到请求");
    upstream.abort();
    let _ = upstream.await;

    let upstream_request: serde_json::Value = serde_json::from_slice(&received).unwrap();
    assert!(
        upstream_request["messages"][0]["content"]
            .to_string()
            .contains("A汉😀"),
        "慢上传后必须完整派发正文：{upstream_request}"
    );
    assert!(
        delivered.contains("answer"),
        "完整响应必须交付：{delivered}"
    );
    assert_eq!(event.status_code, 200);
    assert!(event.is_final_attempt);
    assert_eq!((event.prompt_tokens, event.completion_tokens), (13, 5));
    assert!(event.upstream_ttft_ms > 0, "真实上游首帧必须已测量");
    assert_usage_channel_finished(&mut rx).await;
    assert!(requests.try_recv().is_err(), "一次请求只能派发一次");
    let first_content = event.first_token_latency_ms.expect("已交付非空正文");
    assert!(first_content >= 1_200, "首内容时延必须包含上传：{event:?}");
    assert!(
        first_content.saturating_sub(event.downstream_latency_ms) >= 1_100,
        "handler 时钟不得吞入 1.2 秒上传等待：{event:?}"
    );
    let line = capture.only("慢上传完整交付");
    assert_eq!(line.num("request_body_bytes"), Some(original_len));
    let audit = event.io_audit.expect("上游交换的独立字节审计必须存在");
    assert_eq!(audit.version, 2);
    assert_eq!(audit.boundary, "upstream_attempt");
    assert_eq!(audit.request_body_bytes, Some(received.len() as u64));
    assert_eq!(audit.request_complete, Some(true));
    assert!(audit.response_complete);
}

#[tokio::test]
async fn slow_upload_messages_preserves_the_two_latency_origins() {
    slow_upload_keeps_the_arrival_and_handler_clocks_distinct("/v1/messages").await;
}

#[tokio::test]
async fn slow_upload_chat_preserves_the_two_latency_origins() {
    slow_upload_keeps_the_arrival_and_handler_clocks_distinct("/v1/chat/completions").await;
}

#[tokio::test]
async fn slow_upload_responses_preserves_the_two_latency_origins() {
    slow_upload_keeps_the_arrival_and_handler_clocks_distinct("/v1/responses").await;
}

// 只暴露真实 provider 已有的观测句柄；既不代写首帧/用量，也不改动协议字节和解析器。
struct CheckpointAnthropicBridge {
    inner: aisix_provider_anthropic::AnthropicBridge,
    observers: tokio::sync::mpsc::UnboundedSender<aisix_gateway::response_tap::StreamUsageObserver>,
}

#[async_trait::async_trait]
impl aisix_gateway::Bridge for CheckpointAnthropicBridge {
    fn name(&self) -> &'static str {
        aisix_gateway::Bridge::name(&self.inner)
    }

    async fn chat(
        &self,
        req: &aisix_gateway::ChatFormat,
        ctx: &aisix_gateway::BridgeContext,
    ) -> Result<aisix_gateway::ChatResponse, aisix_gateway::BridgeError> {
        aisix_gateway::Bridge::chat(&self.inner, req, ctx).await
    }

    async fn chat_stream(
        &self,
        req: &aisix_gateway::ChatFormat,
        ctx: &aisix_gateway::BridgeContext,
    ) -> Result<aisix_gateway::ChatChunkStream, aisix_gateway::BridgeError> {
        let stream = aisix_gateway::Bridge::chat_stream(&self.inner, req, ctx).await?;
        self.observers
            .send(
                aisix_gateway::response_tap::stream_usage_observer()
                    .expect("真实派发必须有请求观测上下文"),
            )
            .unwrap();
        Ok(stream)
    }
}

#[derive(Clone, Copy, Debug)]
enum MetadataEnding {
    Error,
    Eof,
    Cancel,
}

async fn metadata_without_canonical_chunk_keeps_its_clock(
    endpoint: &str,
    peek: bool,
    ending: MetadataEnding,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, released) = tokio::sync::oneshot::channel();
    let released = Arc::new(tokio::sync::Mutex::new(Some(released)));
    let upstream = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/messages",
                axum::routing::post(move || {
                    let released = released.clone();
                    async move {
                        let released = released.lock().await.take().expect("只能派发一次");
                        let stream = async_stream::stream! {
                            yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(METADATA_FRAME));
                            let _ = released.await;
                            if matches!(ending, MetadataEnding::Error) {
                                // 官方允许在 message_start 后发送 error，overloaded_error 对应 529。
                                // https://platform.claude.com/docs/en/build-with-claude/streaming#error-events
                                yield Ok(bytes::Bytes::from_static(
                                    b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"ttft-metadata-boundary-overload\"}}\n\n",
                                ));
                            }
                            // EOF 没有 message_stop，不能把截断的回答记作成功。
                        };
                        axum::http::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(Body::from_stream(stream))
                            .unwrap()
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let (observer_tx, mut observers) = tokio::sync::mpsc::unbounded_channel();
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "anthropic",
        Arc::new(CheckpointAnthropicBridge {
            inner: aisix_provider_anthropic::AnthropicBridge::new(),
            observers: observer_tx,
        }),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let state = build_state(boundary_snapshot(&format!("http://{address}"), peek), hub)
        .with_usage_sink(aisix_obs::UsageSink::new(tx));
    let request = cancellable_request(endpoint, boundary_body(endpoint, "hi"));
    // 非 peek 在响应 body 中消费，peek 在 handler 中消费；两者均由同一个任务持续 poll。
    let consumer = tokio::spawn(async move {
        let response = build_router(state).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 65_536).await.unwrap();
        (status, bytes)
    });
    let observer = tokio::time::timeout(std::time::Duration::from_secs(5), observers.recv())
        .await
        .expect("真实 bridge 没有派发")
        .expect("bridge 没有交出 observer");
    // 真实 parser 的首帧时刻与 input_tokens 同时可读才放行；不以固定睡眠猜测已解析。
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if observer.first_frame_at().is_some()
                && observer
                    .get()
                    .is_some_and(|usage| usage.stats.prompt_tokens == 13)
            {
                break;
            }
            assert!(!consumer.is_finished(), "元数据后必须等待测试放行");
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("真实 provider 没有解析首帧及其用量");
    assert!(rx.try_recv().is_err(), "元数据不是终态，不能提前结算");
    // 首帧已由 checkpoint 证实；再拉开终止时刻，防止用错误/取消发生时刻冒充首帧。
    tokio::time::sleep(BODY_DELAY).await;

    if matches!(ending, MetadataEnding::Cancel) {
        consumer.abort();
        assert!(consumer.await.unwrap_err().is_cancelled());
        drop(release);
    } else {
        release.send(()).unwrap();
        let (status, bytes) = tokio::time::timeout(std::time::Duration::from_secs(5), consumer)
            .await
            .expect("上游终止后客户端没有结束")
            .unwrap();
        if peek {
            assert!(
                status.is_server_error(),
                "预读阶段失败必须是 HTTP 错误：{status}"
            );
            if matches!(ending, MetadataEnding::Eof) {
                assert_eq!(status, StatusCode::BAD_GATEWAY);
            }
        } else {
            assert_eq!(status, StatusCode::OK, "响应头已在上游终态前交付");
        }
        let delivered = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            !delivered.contains("answer"),
            "测试不能生成正文：{delivered}"
        );
        if matches!(ending, MetadataEnding::Error) {
            assert!(
                delivered.contains("ttft-metadata-boundary-overload"),
                "{delivered}"
            );
        }
    }
    let event = next_event(&mut rx).await;
    upstream.abort();
    let _ = upstream.await;
    assert!(event.is_final_attempt, "{event:?}");
    assert_eq!((event.prompt_tokens, event.completion_tokens), (13, 0));
    assert_eq!(
        event.first_token_latency_ms, None,
        "没有正文可交付：{event:?}"
    );
    assert!(
        !serde_json::to_value(&event)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("first_token_latency_ms"),
        "未测到的首内容必须在 wire 上缺席"
    );
    assert!(
        event.upstream_ttft_ms > 0,
        "已解析首帧，终止不得丢失时刻：{event:?}"
    );
    assert!(
        event
            .upstream_latency_ms
            .saturating_sub(event.upstream_ttft_ms)
            >= 1_100,
        "首帧时刻必须早于后到的错误、EOF 或取消：{event:?}"
    );
    if matches!(ending, MetadataEnding::Cancel) {
        assert_eq!(event.status_code, 499);
    } else {
        assert!(
            event.status_code >= 400,
            "错误或截断不能结算成功：{event:?}"
        );
    }
    assert!(!event.error_class.is_empty(), "终态必须命名失败：{event:?}");
    assert!(
        !event.error_message.is_empty(),
        "终态必须解释失败：{event:?}"
    );
    if matches!(ending, MetadataEnding::Error) {
        assert!(
            event
                .error_message
                .contains("ttft-metadata-boundary-overload"),
            "{event:?}"
        );
    }
    assert_usage_channel_finished(&mut rx).await;
    assert!(
        observers.try_recv().is_err(),
        "已收到上游首帧后不得再次派发"
    );
}

macro_rules! metadata_boundary_test {
    ($name:ident, $endpoint:literal, $peek:literal, $ending:ident) => {
        #[tokio::test]
        async fn $name() {
            metadata_without_canonical_chunk_keeps_its_clock(
                $endpoint,
                $peek,
                MetadataEnding::$ending,
            )
            .await;
        }
    };
}

metadata_boundary_test!(
    chat_metadata_then_error_keeps_first_frame,
    "/v1/chat/completions",
    false,
    Error
);
metadata_boundary_test!(
    chat_metadata_then_eof_keeps_first_frame,
    "/v1/chat/completions",
    false,
    Eof
);
metadata_boundary_test!(
    chat_metadata_then_cancel_keeps_first_frame,
    "/v1/chat/completions",
    false,
    Cancel
);
metadata_boundary_test!(
    responses_metadata_then_error_keeps_first_frame,
    "/v1/responses",
    false,
    Error
);
metadata_boundary_test!(
    responses_metadata_then_eof_keeps_first_frame,
    "/v1/responses",
    false,
    Eof
);
metadata_boundary_test!(
    responses_metadata_then_cancel_keeps_first_frame,
    "/v1/responses",
    false,
    Cancel
);
metadata_boundary_test!(
    chat_peek_metadata_then_error_keeps_first_frame,
    "/v1/chat/completions",
    true,
    Error
);
metadata_boundary_test!(
    chat_peek_metadata_then_eof_keeps_first_frame,
    "/v1/chat/completions",
    true,
    Eof
);
metadata_boundary_test!(
    chat_peek_metadata_then_cancel_keeps_first_frame,
    "/v1/chat/completions",
    true,
    Cancel
);
metadata_boundary_test!(
    responses_peek_metadata_then_error_keeps_first_frame,
    "/v1/responses",
    true,
    Error
);
metadata_boundary_test!(
    responses_peek_metadata_then_eof_keeps_first_frame,
    "/v1/responses",
    true,
    Eof
);
metadata_boundary_test!(
    responses_peek_metadata_then_cancel_keeps_first_frame,
    "/v1/responses",
    true,
    Cancel
);

#[test]
fn repeated_first_frame_observation_keeps_the_original_sample() {
    let observer = aisix_gateway::response_tap::StreamUsageObserver::default();
    assert_eq!(observer.first_frame_at(), None);
    observer.note_first_frame();
    let first = observer.first_frame_at().expect("首样本必须存在");
    observer.note_first_frame();
    observer.clone().note_first_frame();
    assert_eq!(observer.first_frame_at(), Some(first));
}
