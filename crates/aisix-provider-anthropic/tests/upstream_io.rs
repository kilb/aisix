//! 上游正文与工具解包后的客户端输出分别观察；流离开请求作用域后仍绑定原 attempt。

use aisix_core::usage_io::{TextMeasurement, UsageIoAudit};
use aisix_core::{Model, ProviderKey};
use aisix_gateway::response_tap::{self, ResponseHeaderTap};
use aisix_gateway::{Bridge, BridgeContext, ChatFormat};
use aisix_provider_anthropic::AnthropicBridge;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn context(server: &MockServer, model_name: &str) -> BridgeContext {
    let model: Model = serde_json::from_value(json!({
        "display_name":"upstream-io", "provider":"anthropic", "model_name":model_name,
        "provider_key_id":"11111111-1111-1111-1111-111111111111"
    }))
    .unwrap();
    let key: ProviderKey = serde_json::from_value(json!({
        "display_name":"upstream-io", "provider":"anthropic", "api_key":"test-key",
        "api_base":server.uri(), "request":{"default_body_fields":{"top_k":7}}
    }))
    .unwrap();
    BridgeContext::new("upstream-io", Arc::new(model), Arc::new(key))
}

fn tap() -> Arc<ResponseHeaderTap> {
    Arc::new(ResponseHeaderTap::new(vec![], false, "upstream-io".into()))
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
async fn translated_messages_measure_final_system_and_request_overrides() {
    let server = MockServer::builder().start().await;
    let raw_response = json!({
        "id":"msg_io", "type":"message", "role":"assistant", "model":"claude-sonnet-4-5",
        "content":[{"type":"text","text":"A汉😀"}],
        "stop_reason":"end_turn", "usage":{"input_tokens":7,"output_tokens":5}
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(raw_response.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let req: ChatFormat = serde_json::from_value(json!({
        "model":"client-alias", "max_tokens":64,
        "messages":[{"role":"system","content":"S"},{"role":"user","content":"A汉😀"}]
    }))
    .unwrap();
    let ctx = context(&server, "claude-sonnet-4-5");
    let tap = tap();
    let reply = response_tap::scope(tap.clone(), AnthropicBridge::new().chat(&req, &ctx))
        .await
        .unwrap();
    assert_eq!(reply.message.content.as_deref(), Some("A汉😀"));
    assert_eq!(reply.usage.prompt_tokens, 7);
    assert_eq!(reply.usage.completion_tokens, 5);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(sent["system"], "S");
    assert_eq!(sent["top_k"], 7);
    let audit = audit(&tap);
    assert_eq!(
        audit.request_body_bytes,
        Some(requests[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(raw_response.len() as u64));
    assert!(audit.response_complete);
    assert_eq!(audit.input_text, text(9, 4));
    assert_eq!(audit.output_text, text(8, 3));
}

fn event(payload: Value) -> String {
    format!(
        "event: {}\ndata: {payload}\n\n",
        payload["type"].as_str().unwrap()
    )
}

#[tokio::test]
async fn message_stream_keeps_io_and_early_usage_bound_after_scope_exit() {
    let server = MockServer::builder().start().await;
    let raw_response: String = [
        json!({"type":"message_start","message":{
            "id":"msg_stream_io","type":"message","role":"assistant","model":"claude-sonnet-4-5",
            "content":[],"stop_reason":null,"usage":{"input_tokens":37,"output_tokens":0}
        }}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A汉😀"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]
    .into_iter()
    .map(event)
    .collect();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(raw_response.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let req: ChatFormat = serde_json::from_value(json!({
        "model":"client-alias","max_tokens":64,"messages":[{"role":"user","content":"A汉😀"}]
    }))
    .unwrap();
    let ctx = context(&server, "claude-sonnet-4-5");
    let tap = tap();
    let bridge = AnthropicBridge::new();
    let mut stream = response_tap::scope(tap.clone(), bridge.chat_stream(&req, &ctx))
        .await
        .unwrap();
    let mut output = String::new();
    while let Some(chunk) = stream.next().await {
        if let Some(content) = chunk.unwrap().delta.content {
            output.push_str(&content);
        }
    }
    assert_eq!(output, "A汉😀");
    let usage = tap.observed_stream_usage().unwrap();
    assert!(usage.prompt_reported);
    assert!(usage.completion_reported);
    assert_eq!(usage.stats.prompt_tokens, 37);
    assert_eq!(usage.stats.completion_tokens, 5);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let audit = audit(&tap);
    assert_eq!(
        audit.request_body_bytes,
        Some(requests[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(raw_response.len() as u64));
    assert_eq!(audit.input_text, text(8, 3));
    assert_eq!(audit.output_text, text(8, 3));
}

#[tokio::test]
async fn fake_stream_keeps_original_output_before_synthetic_tool_unwrap() {
    let server = MockServer::builder().start().await;
    let arguments = json!({"name":"A汉😀"});
    let raw_response = json!({
        "id":"msg_tool_io", "type":"message", "role":"assistant", "model":"claude-3-5-haiku-20241022",
        "content":[
            {"type":"text","text":"discarded"},
            {"type":"tool_use","id":"tool_io","name":"json_tool_call","input":arguments}
        ],
        "stop_reason":"tool_use", "usage":{"input_tokens":7,"output_tokens":5}
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(raw_response.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let req: ChatFormat = serde_json::from_value(json!({
        "model":"client-alias", "max_tokens":64, "stream":true,
        "messages":[{"role":"user","content":"A汉😀"}],
        "response_format":{"type":"json_schema","json_schema":{
            "name":"person","schema":{"type":"object","properties":{"name":{"type":"string"}}}
        }}
    }))
    .unwrap();
    let ctx = context(&server, "claude-3-5-haiku-20241022");
    let tap = tap();
    let bridge = AnthropicBridge::new();
    let mut stream = response_tap::scope(tap.clone(), bridge.chat_stream(&req, &ctx))
        .await
        .unwrap();
    let mut output = String::new();
    while let Some(chunk) = stream.next().await {
        if let Some(content) = chunk.unwrap().delta.content {
            output.push_str(&content);
        }
    }
    assert_eq!(output, arguments.to_string());
    assert!(!output.contains("discarded"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["top_k"], 7);
    assert!(sent["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "json_tool_call"));
    let audit = audit(&tap);
    assert_eq!(
        audit.request_body_bytes,
        Some(requests[0].body.len() as u64)
    );
    assert_eq!(audit.response_body_bytes, Some(raw_response.len() as u64));
    assert!(audit.response_complete);
    let raw_output = format!("discarded{arguments}");
    assert_eq!(
        audit.output_text,
        text(raw_output.len() as u64, raw_output.chars().count() as u64)
    );
}
