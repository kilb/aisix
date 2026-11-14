use std::sync::Arc;

use aisix_core::usage_io::{TextMeasurement, UsageIoAudit};
use aisix_gateway::{
    Bridge, BridgeContext, ChatFormat, ChatMessage, EmbeddingRequest, NativeMessages,
    ResponseHeaderTap,
};
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use futures::StreamExt;
use serde_json::{json, Value};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::BedrockBridge;

fn context(model: &str) -> BridgeContext {
    let model = serde_json::from_value(json!({
        "display_name": "io-model", "provider": "bedrock", "model_name": model,
        "provider_key_id": "11111111-1111-1111-1111-111111111111"
    }))
    .unwrap();
    let key = serde_json::from_value(json!({
        "display_name": "io-key",
        "api_key": r#"{"access_key_id":"AKIA-test","secret_access_key":"sk-test","region":"us-west-2"}"#
    })).unwrap();
    BridgeContext::new("io-request", Arc::new(model), Arc::new(key))
}

fn tap() -> Arc<ResponseHeaderTap> {
    Arc::new(ResponseHeaderTap::new(Vec::new(), false, String::new()))
}

fn audit(tap: &ResponseHeaderTap) -> UsageIoAudit {
    tap.capture()
        .io
        .expect("real SDK exchange registered")
        .snapshot()
}

fn text(bytes: u64, scalars: u64, complete: bool) -> Option<TextMeasurement> {
    Some(TextMeasurement {
        utf8_bytes: bytes,
        unicode_scalars: scalars,
        complete,
    })
}

#[tokio::test]
async fn native_sdk_audit_matches_final_body_and_original_response_before_rewrite() {
    let server = MockServer::builder().start().await;
    let response = r#"{ "id":"up-1", "type":"message", "role":"assistant", "content":[{"type":"text","text":"raw汉"}], "stop_reason":"end_turn", "usage":{"input_tokens":7,"output_tokens":3} }"#;
    Mock::given(method("POST"))
        .and(path_regex(r"^/model/anthropic\..+/invoke$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "application/json"))
        .expect(1)
        .mount(&server)
        .await;
    let bridge = BedrockBridge::new().with_endpoint_override(server.uri());
    let tap = tap();
    let input = json!({"model":"client-name", "stream":false, "max_tokens":20,
        "messages":[{"role":"user","content":"A汉😀"}]});
    let mut output = aisix_gateway::response_tap::scope(
        tap.clone(),
        bridge.native_messages(
            input.clone(),
            &context("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        ),
    )
    .await
    .unwrap();
    let requests = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(sent.get("model").is_none());
    assert!(sent.get("stream").is_none());
    assert_eq!(sent["anthropic_version"], "bedrock-2023-05-31");
    assert_ne!(requests[0].body, serde_json::to_vec(&input).unwrap());
    assert!(requests[0].headers["authorization"]
        .to_str()
        .unwrap()
        .starts_with("AWS4-HMAC-SHA256"));
    let before = audit(&tap);
    assert_eq!(
        before.request_body_bytes,
        Some(requests[0].body.len() as u64)
    );
    assert_eq!(before.response_body_bytes, Some(response.len() as u64));
    assert_eq!(before.input_text, text(8, 3, true));
    assert_eq!(before.output_text, text(6, 4, true));
    assert_eq!(before.request_complete, Some(true));
    assert!(before.response_complete);
    let NativeMessages::Json(ref mut output) = output else {
        panic!("nonstream")
    };
    assert_eq!(output["usage"]["input_tokens"], 7);
    assert_eq!(output["usage"]["output_tokens"], 3);
    output["content"] = json!([{"type":"text", "text":"client rewritten answer"}]);
    assert_eq!(audit(&tap), before);
}

#[tokio::test]
async fn converse_sdk_observes_stable_serialization_and_native_output() {
    let server = MockServer::builder().start().await;
    let response = r#"{ "output":{"message":{"role":"assistant","content":[{"text":"raw汉"}]}}, "stopReason":"end_turn", "usage":{"inputTokens":7,"outputTokens":3,"totalTokens":10}, "metrics":{"latencyMs":1} }"#;
    Mock::given(method("POST"))
        .and(path_regex(r"^/model/meta\..+/converse$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "application/json"))
        .expect(1)
        .mount(&server)
        .await;
    let bridge = BedrockBridge::new().with_endpoint_override(server.uri());
    let mut req = ChatFormat::new("client-model", vec![ChatMessage::user("A汉😀")]);
    req.temperature = Some(0.7);
    let tap = tap();
    let result = aisix_gateway::response_tap::scope(
        tap.clone(),
        bridge.chat(&req, &context("meta.llama3-3-70b-instruct-v1:0")),
    )
    .await
    .unwrap();
    assert_eq!(result.usage.prompt_tokens, 7);
    assert_eq!(result.usage.completion_tokens, 3);
    let requests = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["inferenceConfig"]["temperature"].as_f64(), Some(0.7));
    assert_eq!(sent["messages"][0]["content"][0]["text"], "A汉😀");
    let audit = audit(&tap);
    assert_eq!(
        audit.request_body_bytes,
        Some(requests[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(response.len() as u64));
    assert_eq!(audit.input_text, text(8, 3, true));
    assert_eq!(audit.output_text, text(6, 4, true));
    assert!(audit.response_complete);
}

#[tokio::test]
async fn titan_sdk_sums_two_actual_exchanges_with_unchanged_usage() {
    let server = MockServer::builder().start().await;
    let response = r#"{"embedding":[0.25,0.5],"inputTextTokenCount":4}"#;
    Mock::given(method("POST"))
        .and(path("/model/amazon.titan-embed-text-v1/invoke"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "application/json"))
        .expect(2)
        .mount(&server)
        .await;
    let req = EmbeddingRequest {
        extra: Default::default(),
        token_input: None,
        model: "client-model".into(),
        input: vec!["A汉😀".into(), "z".into()],
        input_was_single: false,
        encoding_format: None,
        dimensions: None,
        input_type: None,
        embedding_types: None,
    };
    let tap = tap();
    let bridge = BedrockBridge::new().with_endpoint_override(server.uri());
    let result = aisix_gateway::response_tap::scope(
        tap.clone(),
        bridge.embed(&req, &context("amazon.titan-embed-text-v1")),
    )
    .await
    .unwrap();
    assert_eq!(result.usage.prompt_tokens, 8);
    assert_eq!(result.data.len(), 2);
    let requests = server.received_requests().await.unwrap();
    let audit = audit(&tap);
    assert_eq!(audit.exchange_count, Some(2));
    assert_eq!(
        audit.request_body_bytes,
        Some(requests.iter().map(|r| r.body.len() as u64).sum())
    );
    assert_eq!(audit.response_body_bytes, Some((2 * response.len()) as u64));
    assert_eq!(audit.input_text, text(9, 4, true));
    assert_eq!(audit.output_text, text(0, 0, true));
    assert_eq!(audit.request_complete, Some(true));
    assert!(audit.response_complete);
}

fn frame(event: &str, payload: Value) -> Vec<u8> {
    let message = Message::new(serde_json::to_vec(&payload).unwrap())
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String(event.to_owned().into()),
        ))
        .add_header(Header::new(
            ":content-type",
            HeaderValue::String("application/json".into()),
        ));
    let mut bytes = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&message, &mut bytes).unwrap();
    bytes
}

#[tokio::test]
async fn count_tokens_observes_model_input_inside_actual_sdk_base64_envelope() {
    use base64::Engine as _;
    let server = MockServer::builder().start().await;
    let response = r#"{"inputTokens":77}"#;
    Mock::given(method("POST"))
        .and(path_regex(r"^/model/anthropic\..+/count-tokens$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "application/json"))
        .expect(1)
        .mount(&server)
        .await;
    let bridge = BedrockBridge::new().with_endpoint_override(server.uri());
    let tap = tap();
    let answer = aisix_gateway::response_tap::scope(
        tap.clone(),
        bridge.native_count_tokens(
            json!({"model":"client-model", "messages":[{"role":"user","content":"A汉😀"}]}),
            &context("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(answer["input_tokens"], 77);
    let requests = server.received_requests().await.unwrap();
    let envelope: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let encoded = envelope["input"]["invokeModel"]["body"].as_str().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let actual: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        actual["messages"][0]["content"],
        json!([{"type":"text","text":"A汉😀"}])
    );
    assert_eq!(actual["anthropic_version"], "bedrock-2023-05-31");
    let audit = audit(&tap);
    assert_eq!(
        audit.request_body_bytes,
        Some(requests[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(response.len() as u64));
    assert_eq!(audit.input_text, text(8, 3, true));
    assert_eq!(audit.exchange_count, Some(1));
}

#[tokio::test]
async fn concurrent_sdk_calls_keep_separate_attempt_observers() {
    let server = MockServer::builder().start().await;
    let response = r#"{"id":"up-1","type":"message","role":"assistant","content":[{"type":"text","text":"raw"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}}"#;
    Mock::given(method("POST"))
        .and(path_regex(r"^/model/anthropic\..+/invoke$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "application/json"))
        .expect(2)
        .mount(&server)
        .await;
    let bridge = BedrockBridge::new().with_endpoint_override(server.uri());
    let ctx = context("anthropic.claude-3-5-sonnet-20241022-v2:0");
    let first = tap();
    let second = tap();
    let (a, b) = tokio::join!(
        aisix_gateway::response_tap::scope(
            first.clone(),
            bridge.native_messages(
                json!({"max_tokens":20,"messages":[{"role":"user","content":"A汉😀"}]}),
                &ctx
            )
        ),
        aisix_gateway::response_tap::scope(
            second.clone(),
            bridge.native_messages(
                json!({"max_tokens":20,"messages":[{"role":"user","content":"b"}]}),
                &ctx
            )
        ),
    );
    assert!(a.is_ok());
    assert!(b.is_ok());
    assert_eq!(audit(&first).input_text, text(8, 3, true));
    assert_eq!(audit(&second).input_text, text(1, 1, true));
    assert_eq!(audit(&first).exchange_count, Some(1));
    assert_eq!(audit(&second).exchange_count, Some(1));
    assert_eq!(
        audit(&first).response_body_bytes,
        Some(response.len() as u64)
    );
    assert_eq!(
        audit(&second).response_body_bytes,
        Some(response.len() as u64)
    );
    assert_ne!(
        audit(&first).request_body_bytes,
        audit(&second).request_body_bytes
    );
}

#[tokio::test]
async fn native_binary_stream_observes_payload_once_before_sse_rendering() {
    use base64::Engine as _;
    let server = MockServer::builder().start().await;
    let mut wire = Vec::new();
    for event in [
        json!({"type":"message_start","message":{"id":"up-1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A汉😀"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ] {
        wire.extend(frame("chunk", json!({"bytes":base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&event).unwrap())})));
    }
    Mock::given(method("POST"))
        .and(path_regex(
            r"^/model/anthropic\..+/invoke-with-response-stream$",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(wire.clone(), "application/vnd.amazon.eventstream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let bridge = BedrockBridge::new().with_endpoint_override(server.uri());
    let tap = tap();
    let frames = aisix_gateway::response_tap::scope(tap.clone(), async {
        let answer = bridge.native_messages(json!({"stream":true,"max_tokens":20,"messages":[{"role":"user","content":"input"}]}),
            &context("anthropic.claude-3-5-sonnet-20241022-v2:0")).await.unwrap();
        let NativeMessages::Stream(mut stream) = answer else { panic!("stream") };
        let mut frames = Vec::new();
        while let Some(frame) = stream.next().await { frames.extend_from_slice(&frame.unwrap()); }
        frames
    }).await;
    assert!(std::str::from_utf8(&frames).unwrap().contains("A汉😀"));
    let audit = audit(&tap);
    assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
    assert_ne!(audit.response_body_bytes, Some(frames.len() as u64));
    assert_eq!(audit.output_text, text(8, 3, true));
    assert!(audit.response_complete);
}

#[tokio::test]
async fn converse_binary_body_is_counted_separately_from_decoded_model_text() {
    let server = MockServer::builder().start().await;
    let mut wire = frame("messageStart", json!({"role":"assistant"}));
    wire.extend(frame(
        "contentBlockDelta",
        json!({"contentBlockIndex":0,"delta":{"text":"A汉😀"}}),
    ));
    wire.extend(frame("messageStop", json!({"stopReason":"end_turn"})));
    wire.extend(frame("metadata", json!({"usage":{"inputTokens":7,"outputTokens":3,"totalTokens":10}, "metrics":{"latencyMs":1}})));
    Mock::given(method("POST"))
        .and(path_regex(r"^/model/meta\..+/converse-stream$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(wire.clone(), "application/vnd.amazon.eventstream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let bridge = BedrockBridge::new().with_endpoint_override(server.uri());
    let req = ChatFormat::new("client-model", vec![ChatMessage::user("input")]);
    let tap = tap();
    let chunks = aisix_gateway::response_tap::scope(tap.clone(), async {
        let mut stream = bridge
            .chat_stream(&req, &context("meta.llama3-3-70b-instruct-v1:0"))
            .await
            .unwrap();
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk.unwrap());
        }
        chunks
    })
    .await;
    assert!(chunks
        .iter()
        .any(|c| c.usage.as_ref().is_some_and(|u| u.completion_tokens == 3)));
    let audit = audit(&tap);
    assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
    assert_eq!(audit.output_text, text(8, 3, true));
    assert!(audit.response_complete);
}

#[tokio::test]
async fn dropping_binary_sdk_stream_keeps_observed_prefix_incomplete_without_drain() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut prefix = frame("messageStart", json!({"role":"assistant"}));
    prefix.extend(frame(
        "contentBlockDelta",
        json!({"contentBlockIndex":0,"delta":{"text":"A汉😀"}}),
    ));
    let expected = prefix.len() as u64;
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let count = socket.read(&mut buf).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buf[..count]);
            if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&request[..end]).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        socket
            .write_all(format!("{:x}\r\n", prefix.len()).as_bytes())
            .await
            .unwrap();
        socket.write_all(&prefix).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
        let _ = released.await;
    });
    let bridge = BedrockBridge::new().with_endpoint_override(endpoint);
    let req = ChatFormat::new("client-model", vec![ChatMessage::user("input")]);
    let tap = tap();
    aisix_gateway::response_tap::scope(tap.clone(), async {
        let mut stream = bridge
            .chat_stream(&req, &context("meta.llama3-3-70b-instruct-v1:0"))
            .await
            .unwrap();
        loop {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if chunk.delta.content.as_deref() == Some("A汉😀") {
                break;
            }
        }
        drop(stream);
    })
    .await;
    let audit = audit(&tap);
    assert_eq!(audit.response_body_bytes, Some(expected));
    assert_eq!(audit.output_text, text(8, 3, false));
    assert!(!audit.response_complete);
    let _ = release.send(());
    server.await.unwrap();
}
