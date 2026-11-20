use std::sync::Arc;
use std::time::Duration;

use aisix_core::usage_io::{TextMeasurement, UsageIoAudit};
use aisix_gateway::{
    response_tap, Bridge, BridgeContext, BridgeError, ChatFormat, ChatMessage, EmbeddingRequest,
    NativeMessages, ResponseHeaderTap,
};
use aisix_provider_vertex::VertexBridge;
use futures::TryStreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn context(base: &str, model_name: &str, gemini_instruction: bool) -> BridgeContext {
    let model = serde_json::from_value(json!({
        "display_name":"io-audit-vertex", "provider":"vertex",
        "model_name":model_name, "provider_key_id":"pk-io-audit"
    }))
    .unwrap();
    let mut key = json!({
        "display_name":"io-audit-key", "api_base":base,
        "api_key":r#"{"access_token":"synthetic-token","project":"io-project","region":"us-central1"}"#
    });
    if gemini_instruction {
        key["request"] =
            json!({"default_body_fields":{"systemInstruction":{"parts":[{"text":"x"}]}}});
    }
    BridgeContext::new(
        "req-io-audit-vertex",
        Arc::new(model),
        Arc::new(serde_json::from_value(key).unwrap()),
    )
}

fn tap() -> Arc<ResponseHeaderTap> {
    Arc::new(ResponseHeaderTap::new(
        vec!["x-goog-request-id".into()],
        false,
        "req-io-audit-vertex".into(),
    ))
}

async fn upstream(body: &[u8], stream: bool) -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-goog-request-id", "vertex-upstream-id")
                .insert_header(
                    "content-type",
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .set_body_bytes(body),
        )
        .mount(&server)
        .await;
    server
}

async fn assert_exchange(
    tap: &ResponseHeaderTap,
    server: &MockServer,
    response: &[u8],
    input_bytes: u64,
    input_scalars: u64,
    require_eof: bool,
) -> UsageIoAudit {
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let capture = tap.capture();
    assert_eq!(capture.request_id, "vertex-upstream-id");
    let audit = capture
        .io
        .expect("actual Vertex send must be observed")
        .snapshot();
    assert_eq!(audit.version, 2);
    assert_eq!(audit.boundary, "upstream_attempt");
    assert_eq!(audit.exchange_count, Some(1));
    assert_eq!(audit.request_complete, Some(true));
    assert_eq!(
        audit.request_body_bytes,
        Some(received[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(response.len() as u64));
    if require_eof {
        assert!(audit.response_complete);
    }
    assert_eq!(
        audit.input_text,
        Some(TextMeasurement {
            utf8_bytes: input_bytes,
            unicode_scalars: input_scalars,
            complete: true,
        })
    );
    audit
}

fn response(protocol: &str, stream: bool) -> Vec<u8> {
    match (protocol, stream) {
        ("gemini", false) => serde_json::to_vec(&json!({"candidates":[{"content":{"role":"model","parts":[{"text":"A汉😀"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}})).unwrap(),
        ("gemini", true) => format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"text":"A汉😀"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}})).into_bytes(),
        ("messages", false) => serde_json::to_vec(&json!({"id":"msg-vertex","type":"message","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"A汉😀"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}})).unwrap(),
        ("messages", true) => format!(
            "event: message_start\ndata: {}\n\nevent: content_block_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\nevent: content_block_stop\ndata: {}\n\nevent: message_delta\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
            json!({"type":"message_start","message":{"id":"msg-vertex","type":"message","model":"claude-sonnet-4-5","role":"assistant","content":[],"usage":{"input_tokens":3,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A汉😀"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ).into_bytes(),
        (_, false) => serde_json::to_vec(&json!({"id":"chat-vertex","object":"chat.completion","model":"vertex-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"A汉😀"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})).unwrap(),
        (_, true) => format!("data: {}\n\ndata: [DONE]\n\n", json!({"id":"chat-vertex","object":"chat.completion.chunk","model":"vertex-model",
            "choices":[{"index":0,"delta":{"content":"A汉😀"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})).into_bytes(),
    }
}

#[tokio::test]
async fn upstream_io_vertex_all_publisher_chat_rails_observe_their_actual_protocol() {
    for (model, protocol) in [
        ("gemini-2.5-pro", "gemini"),
        ("claude-sonnet-4-5", "messages"),
        ("llama-3.3-70b-instruct-maas", "chat"),
        ("mistral-large-2411", "chat"),
        ("jamba-1.5-large", "chat"),
    ] {
        for stream in [false, true] {
            let body = response(protocol, stream);
            let server = upstream(&body, stream).await;
            let gemini = protocol == "gemini";
            let ctx = context(&server.uri(), model, gemini);
            let req = ChatFormat::new("caller-alias", vec![ChatMessage::user("A汉😀")]);
            let bridge = VertexBridge::new();
            let tap = tap();
            response_tap::scope(tap.clone(), async {
                if stream {
                    let chunks: Vec<_> = bridge
                        .chat_stream(&req, &ctx)
                        .await
                        .unwrap()
                        .try_collect()
                        .await
                        .unwrap();
                    assert!(!chunks.is_empty());
                } else {
                    assert_eq!(
                        bridge.chat(&req, &ctx).await.unwrap().message.content_str(),
                        "A汉😀"
                    );
                }
            })
            .await;
            let audit = assert_exchange(
                &tap,
                &server,
                &body,
                if gemini { 9 } else { 8 },
                if gemini { 4 } else { 3 },
                !(protocol == "messages" && stream),
            )
            .await;
            assert_eq!(
                audit.output_text,
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    // 语义终态可以早于 HTTP EOF，正文观察的完整性必须传递给文本。
                    complete: audit.response_complete
                }),
                "{model} stream={stream}"
            );
            if gemini {
                let received = server.received_requests().await.unwrap();
                let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
                assert_eq!(sent["systemInstruction"]["parts"][0]["text"], "x");
            }
        }
    }
}

#[tokio::test]
async fn upstream_io_vertex_native_messages_observe_json_and_untranslated_sse() {
    for stream in [false, true] {
        let body = response("messages", stream);
        let server = upstream(&body, stream).await;
        let ctx = context(&server.uri(), "claude-sonnet-4-5", false);
        let bridge = VertexBridge::new();
        let tap = tap();
        response_tap::scope(tap.clone(), async {
            let result = bridge
                .anthropic_messages(
                    json!({"model":"caller-alias","stream":stream,"max_tokens":32,
                "messages":[{"role":"user","content":"A汉😀"}]}),
                    &ctx,
                )
                .await
                .unwrap();
            match result {
                NativeMessages::Json(json) => assert_eq!(json["content"][0]["text"], "A汉😀"),
                NativeMessages::Stream(frames) => {
                    let chunks: Vec<_> = frames.try_collect().await.unwrap();
                    assert_eq!(chunks.concat(), body);
                }
            }
        })
        .await;
        let audit = assert_exchange(&tap, &server, &body, 8, 3, true).await;
        assert_eq!(
            audit.output_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true
            })
        );
    }
}

#[tokio::test]
async fn upstream_io_vertex_count_tokens_observes_only_the_counting_exchange() {
    let body = serde_json::to_vec(&json!({"input_tokens":9})).unwrap();
    let server = upstream(&body, false).await;
    let ctx = context(&server.uri(), "claude-sonnet-4-5", false);
    let bridge = VertexBridge::new();
    let tap = tap();
    response_tap::scope(tap.clone(), async {
        assert_eq!(
            bridge
                .anthropic_count_tokens(
                    json!({"messages":[{"role":"user","content":"A汉😀"}]}),
                    &ctx
                )
                .await
                .unwrap()["input_tokens"],
            9
        );
    })
    .await;
    let audit = assert_exchange(&tap, &server, &body, 8, 3, true).await;
    assert!(audit.output_text.is_none());
}

#[tokio::test]
async fn upstream_io_vertex_predict_measures_native_embedding_input() {
    let body = serde_json::to_vec(&json!({"predictions":[{"embeddings":{"values":[0.1,0.2],"statistics":{"token_count":3}}}]})).unwrap();
    let server = upstream(&body, false).await;
    let ctx = context(&server.uri(), "text-embedding-005", false);
    let bridge = VertexBridge::new();
    let tap = tap();
    response_tap::scope(tap.clone(), async {
        assert_eq!(
            bridge
                .embed(&embedding_request(), &ctx)
                .await
                .unwrap()
                .usage
                .prompt_tokens,
            3
        );
    })
    .await;
    let audit = assert_exchange(&tap, &server, &body, 8, 3, true).await;
    assert_eq!(
        audit.output_text,
        Some(TextMeasurement {
            utf8_bytes: 0,
            unicode_scalars: 0,
            complete: true,
        })
    );
}

fn embedding_request() -> EmbeddingRequest {
    EmbeddingRequest {
        model: "caller-alias".into(),
        input: vec!["A汉😀".into()],
        token_input: None,
        input_was_single: true,
        encoding_format: None,
        dimensions: None,
        input_type: None,
        embedding_types: None,
        extra: Default::default(),
    }
}

async fn assert_vertex_buffered_deadline(route: &str, model: &str, send_head: bool) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind local upstream");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    let upstream = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.expect("receive Vertex request");
        assert_ne!(conn.read(&mut [0; 4096]).await.expect("request bytes"), 0);
        if send_head {
            conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-goog-request-id: vertex-upstream-id\r\ncontent-length: 1024\r\n\r\n{")
                .await
                .expect("send head and incomplete body");
        }
        std::future::pending::<()>().await;
    });

    let ctx = context(&base, model, false).with_deadline(Duration::from_millis(500));
    let bridge = VertexBridge::new();
    let tap = tap();
    let result = response_tap::scope(tap.clone(), async {
        let messages = || json!({"messages":[{"role":"user","content":"hello"}]});
        match route {
            "messages" | "messages-json-fallback" => {
                let mut body = messages();
                body["max_tokens"] = json!(32);
                body["stream"] = json!(route == "messages-json-fallback");
                bridge.anthropic_messages(body, &ctx).await.map(|_| ())
            }
            "count_tokens" => bridge
                .anthropic_count_tokens(messages(), &ctx)
                .await
                .map(|_| ()),
            "embeddings" => bridge.embed(&embedding_request(), &ctx).await.map(|_| ()),
            _ => {
                let request = ChatFormat::new("caller-alias", vec![ChatMessage::user("hello")]);
                bridge.chat(&request, &ctx).await.map(|_| ())
            }
        }
    })
    .await;
    upstream.abort();

    if send_head {
        assert!(
            matches!(
                result,
                Err(BridgeError::UpstreamBodyUnread { status: 200, .. })
            ),
            "{route} {model}: {result:?}"
        );
        let captured = tap.capture();
        assert_eq!(captured.request_id, "vertex-upstream-id");
        let audit = captured.io.expect("capture the actual response").snapshot();
        assert_eq!(audit.exchange_count, Some(1));
        assert!(!audit.response_complete);
    } else {
        assert!(
            matches!(result, Err(BridgeError::Timeout { .. })),
            "{route} {model}: {result:?}"
        );
    }
}

#[tokio::test]
async fn vertex_buffered_deadlines_keep_the_accepted_upstream_status() {
    futures::future::join_all(
        [
            ("chat", "gemini-2.5-pro"),
            ("chat", "claude-sonnet-4-5"),
            ("chat", "llama-3.3-70b-instruct-maas"),
            ("chat", "mistral-large-2411"),
            ("chat", "jamba-1.5-large"),
            ("embeddings", "text-embedding-005"),
            ("messages", "claude-sonnet-4-5"),
            ("messages-json-fallback", "claude-sonnet-4-5"),
            ("count_tokens", "claude-sonnet-4-5"),
        ]
        .into_iter()
        .map(|(route, model)| assert_vertex_buffered_deadline(route, model, true)),
    )
    .await;
}

#[tokio::test]
async fn vertex_buffered_deadline_before_headers_is_a_timeout() {
    assert_vertex_buffered_deadline("chat", "gemini-2.5-pro", false).await;
}
