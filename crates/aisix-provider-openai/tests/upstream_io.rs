//! 在实际发送和读取正文的边界记录长度，不使用调用方原文代替上游请求。

use aisix_core::usage_io::{TextMeasurement, UsageIoAudit};
use aisix_core::{Model, ProviderKey};
use aisix_gateway::response_tap::{self, ResponseHeaderTap};
use aisix_gateway::{Bridge, BridgeContext, ChatFormat, EmbeddingRequest};
use aisix_provider_openai::OpenAiBridge;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn context(server: &MockServer, request: Value, response: Value) -> BridgeContext {
    let model: Model = serde_json::from_value(json!({
        "display_name":"upstream-io", "provider":"openai", "model_name":"gpt-target",
        "provider_key_id":"11111111-1111-1111-1111-111111111111"
    }))
    .unwrap();
    let key: ProviderKey = serde_json::from_value(json!({
        "display_name":"upstream-io", "provider":"openai", "api_key":"test-key",
        "api_base":server.uri(), "request":request, "response":response
    }))
    .unwrap();
    BridgeContext::new("upstream-io", Arc::new(model), Arc::new(key))
}

fn tap() -> Arc<ResponseHeaderTap> {
    Arc::new(ResponseHeaderTap::new(
        vec!["x-request-id".into()],
        false,
        "upstream-io".into(),
    ))
}

fn audit(tap: &ResponseHeaderTap) -> UsageIoAudit {
    let audit = tap.capture().io.unwrap().snapshot();
    assert_eq!(audit.version, 2);
    assert_eq!(audit.boundary, "upstream_attempt");
    assert_eq!(audit.exchange_count, Some(1));
    assert_eq!(audit.request_complete, Some(true));
    audit
}

fn text(bytes: u64, scalars: u64) -> Option<TextMeasurement> {
    Some(TextMeasurement {
        utf8_bytes: bytes,
        unicode_scalars: scalars,
        complete: true,
    })
}

#[tokio::test]
async fn chat_measures_final_overrides_and_raw_unicode_response() {
    let server = MockServer::builder().start().await;
    let raw_response = json!({
        "id":"chat-io", "object":"chat.completion", "model":"gpt-target",
        "choices":[
            {"index":0,"message":{"role":"assistant","content":"A汉😀"},"finish_reason":"stop"},
            {"index":1,"message":{"role":"assistant","content":"旁"},"finish_reason":"stop"}
        ],
        "usage":{"prompt_tokens":7,"completion_tokens":5,"total_tokens":12}
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("x-request-id", "provider-io")
                .set_body_string(raw_response.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let req: ChatFormat = serde_json::from_value(json!({
        "model":"client-alias", "temperature":0.9,
        "messages":[{"role":"user","content":[
            {"type":"text","text":"A"},{"type":"text","text":"汉😀"}
        ]}]
    }))
    .unwrap();
    let ctx = context(
        &server,
        json!({"param_constraints":{"temperature_max":0.2},"default_body_fields":{"safe_prompt":true}}),
        json!({"content_list_to_string":true}),
    );
    let tap = tap();
    let reply = response_tap::scope(tap.clone(), OpenAiBridge::new().chat(&req, &ctx))
        .await
        .unwrap();
    assert_eq!(reply.message.content.as_deref(), Some("A汉😀"));
    assert_eq!(reply.more_choices.len(), 1);
    assert_eq!(reply.usage.prompt_tokens, 7);
    assert_eq!(reply.usage.completion_tokens, 5);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-target");
    assert_eq!(sent["temperature"], 0.2);
    assert_eq!(sent["safe_prompt"], true);
    assert_eq!(sent["messages"][0]["content"], "A汉😀");
    let audit = audit(&tap);
    assert_eq!(
        audit.request_body_bytes,
        Some(requests[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(raw_response.len() as u64));
    assert!(audit.response_complete);
    assert_eq!(audit.input_text, text(8, 3));
    assert_eq!(audit.output_text, text(11, 4));
    assert_eq!(tap.capture().request_id, "provider-io");
}

#[tokio::test]
async fn chat_stream_observer_stays_bound_when_consumed_outside_request_scope() {
    let server = MockServer::builder().start().await;
    let raw_response = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"stream-io","model":"gpt-target","choices":[
            {"index":0,"delta":{"role":"assistant","content":"A汉😀"},"finish_reason":"stop"},
            {"index":1,"delta":{"role":"assistant","content":"旁"},"finish_reason":"stop"}
        ]})
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(raw_response.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let req: ChatFormat = serde_json::from_value(json!({
        "model":"client-alias","messages":[{"role":"user","content":"A汉😀"}]
    }))
    .unwrap();
    let ctx = context(&server, json!({}), json!({}));
    let tap = tap();
    let bridge = OpenAiBridge::new();
    let mut stream = response_tap::scope(tap.clone(), bridge.chat_stream(&req, &ctx))
        .await
        .unwrap();
    let mut output = String::new();
    while let Some(chunk) = stream.next().await {
        if let Some(content) = chunk.unwrap().delta.content {
            output.push_str(&content);
        }
    }
    assert_eq!(output, "A汉😀旁");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let audit = audit(&tap);
    assert_eq!(
        audit.request_body_bytes,
        Some(requests[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(raw_response.len() as u64));
    assert_eq!(audit.input_text, text(8, 3));
    assert_eq!(audit.output_text, text(11, 4));
}

#[tokio::test]
async fn completions_measure_the_same_final_body_in_both_modes() {
    for streaming in [false, true] {
        let server = MockServer::builder().start().await;
        let payload = json!({
            "id":"completion-io","object":"text_completion","model":"gpt-target",
            "choices":[{"index":0,"text":"A汉😀","finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":3,"total_tokens":6}
        });
        let raw_response = if streaming {
            format!("data: {payload}\n\ndata: [DONE]\n\n")
        } else {
            payload.to_string()
        };
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "content-type",
                        if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )
                    .set_body_string(raw_response.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let ctx = context(
            &server,
            json!({"default_body_fields":{"echo":false}}),
            json!({}),
        );
        let tap = tap();
        let bridge = OpenAiBridge::new();
        let request = json!({"model":"client-alias","prompt":"A汉😀","stream":streaming});
        if streaming {
            let mut stream =
                response_tap::scope(tap.clone(), bridge.complete_stream(&request, &ctx))
                    .await
                    .unwrap();
            let mut returned = Vec::new();
            while let Some(frame) = stream.next().await {
                returned.extend_from_slice(&frame.unwrap());
            }
            assert_eq!(returned, raw_response.as_bytes());
        } else {
            let reply = response_tap::scope(tap.clone(), bridge.complete(&request, &ctx))
                .await
                .unwrap();
            assert_eq!(reply, payload);
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(sent["model"], "gpt-target");
        assert_eq!(sent["echo"], false);
        let audit = audit(&tap);
        assert_eq!(
            audit.request_body_bytes,
            Some(requests[0].body.len() as u64)
        );
        assert_eq!(audit.response_body_bytes, Some(raw_response.len() as u64));
        assert!(audit.response_complete);
        assert_eq!(audit.input_text, text(8, 3));
        assert_eq!(audit.output_text, text(8, 3));
    }
}

#[tokio::test]
async fn embeddings_and_images_use_the_target_protocol_text_slots() {
    let server = MockServer::builder().start().await;
    let embed_response = json!({
        "object":"list","model":"gpt-target",
        "data":[{"object":"embedding","index":0,"embedding":[0.25]}],
        "usage":{"prompt_tokens":3,"total_tokens":3}
    })
    .to_string();
    let image_response =
        json!({"created":1,"data":[{"url":"https://example.test/image.png"}]}).to_string();
    for (endpoint, body) in [
        ("/embeddings", &embed_response),
        ("/images/generations", &image_response),
    ] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let ctx = context(
        &server,
        json!({"default_body_fields":{"safe_prompt":true}}),
        json!({}),
    );
    let bridge = OpenAiBridge::new();
    let embedding: EmbeddingRequest = serde_json::from_value(json!({
        "model":"client-alias","input":["A汉😀"]
    }))
    .unwrap();
    let embed_tap = tap();
    let reply = response_tap::scope(embed_tap.clone(), bridge.embed(&embedding, &ctx))
        .await
        .unwrap();
    assert_eq!(reply.data.len(), 1);
    let image_tap = tap();
    let image_request = json!({"model":"client-alias","prompt":"A汉😀"});
    let image = response_tap::scope(
        image_tap.clone(),
        bridge.generate_image(&image_request, &ctx),
    )
    .await
    .unwrap();
    assert_eq!(image["data"][0]["url"], "https://example.test/image.png");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for (tap, endpoint, response) in [
        (&embed_tap, "/embeddings", &embed_response),
        (&image_tap, "/images/generations", &image_response),
    ] {
        let sent = requests
            .iter()
            .find(|request| request.url.path() == endpoint)
            .unwrap();
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["model"], "gpt-target");
        assert_eq!(body["safe_prompt"], true);
        let audit = audit(tap);
        assert_eq!(audit.request_body_bytes, Some(sent.body.len() as u64));
        assert_eq!(audit.response_body_bytes, Some(response.len() as u64));
        assert!(audit.response_complete);
        assert_eq!(audit.input_text, text(8, 3));
    }
}
