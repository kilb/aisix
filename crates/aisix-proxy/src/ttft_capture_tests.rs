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
