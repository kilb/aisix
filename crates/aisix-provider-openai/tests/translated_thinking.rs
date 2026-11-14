//! 原生历史仅在原协议回放；Chat 请求保留可表示内容与工具往返。

use aisix_core::{Model, ProviderKey};
use aisix_gateway::{Bridge, BridgeContext, ChatFormat};
use aisix_provider_anthropic::wire::parse_inbound_request;
use aisix_provider_openai::wire::{build_request, messages_from, DeveloperRoleMode};
use aisix_provider_openai::OpenAiBridge;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn upstream(req: &ChatFormat) -> Value {
    let messages = messages_from(req, DeveloperRoleMode::Preserve);
    serde_json::to_value(build_request(req, "gpt-target", &messages, false)).unwrap()
}

#[test]
fn translated_thinking_keeps_text_and_tool_round_trip_without_native_carrier() {
    let body = json!({
        "model":"claude-entry","max_tokens":64,
        "messages":[
            {"role":"user","content":"Find it"},
            {"role":"assistant","content":[
                {"type":"text","text":"Before."},
                {"type":"thinking","thinking":"Readable chain","signature":"signed-sentinel"},
                {"type":"redacted_thinking","data":"ciphertext-sentinel"},
                {"type":"text","text":"After."},
                {"type":"tool_use","id":"tool-1","name":"search","input":{"query":"exact"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tool-1","content":"Found."},
                {"type":"text","text":"Use it."},
                {"type":"image","source":{"type":"url","url":"https://example.test/reference.png"}}
            ]}
        ]
    });
    let req = parse_inbound_request(&body).unwrap();
    let before = serde_json::to_value(&req).unwrap();
    let wire = upstream(&req);
    let messages = wire["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[1]["content"],
        json!([
            {"type":"text","text":"Before."},
            {"type":"text","text":"After."}
        ])
    );
    assert_eq!(messages[1]["reasoning_content"], "Readable chain");
    assert_eq!(
        messages[1]["tool_calls"],
        json!([
            {"id":"tool-1","type":"function","function":{"name":"search","arguments":"{\"query\":\"exact\"}"}}
        ])
    );
    assert_eq!(
        messages[2],
        json!({"role":"tool","tool_call_id":"tool-1","content":"Found."})
    );
    assert_eq!(
        messages[3]["content"],
        json!([
            {"type":"text","text":"Use it."},
            {"type":"image_url","image_url":{"url":"https://example.test/reference.png"}}
        ])
    );
    assert!(!wire.to_string().contains("signed-sentinel"));
    assert!(!wire.to_string().contains("ciphertext-sentinel"));
    assert_eq!(serde_json::to_value(&req).unwrap(), before);
    assert_eq!(
        before["messages"][1]["content_blocks"],
        body["messages"][1]["content"]
    );
}

#[test]
fn translated_thinking_only_tool_turn_stays_null_with_no_duplicate_call() {
    let req = parse_inbound_request(&json!({
        "model":"entry","max_tokens":64,"messages":[
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"Plan","signature":"signed"},
                {"type":"redacted_thinking","data":"opaque"},
                {"type":"tool_use","id":"tool-1","name":"lookup","input":{}}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"ok"}]}
        ]
    }))
    .unwrap();
    let wire = upstream(&req);
    assert_eq!(wire["messages"][0].get("content"), Some(&Value::Null));
    assert_eq!(
        wire["messages"][0]["tool_calls"].as_array().unwrap().len(),
        1
    );
    assert_eq!(wire["messages"][1]["tool_call_id"], "tool-1");
    assert_eq!(wire["messages"][1]["content"], "ok");
    assert_eq!(wire["messages"][0]["reasoning_content"], "Plan");
}

#[test]
fn translated_redacted_only_carrier_preserves_text_without_inventing_reasoning() {
    let req = parse_inbound_request(&json!({
        "model":"entry","max_tokens":64,"messages":[
            {"role":"assistant","content":[
                {"type":"redacted_thinking","data":"opaque-only"},
                {"type":"text","text":"Answer"}
            ]},
            {"role":"user","content":"Continue"}
        ]
    }))
    .unwrap();
    let wire = upstream(&req);
    assert_eq!(
        wire["messages"][0]["content"],
        json!([{"type":"text","text":"Answer"}])
    );
    assert!(wire["messages"][0].get("reasoning_content").is_none());
    assert!(!wire.to_string().contains("opaque-only"));
    assert_eq!(
        req.messages[0].content_blocks.as_ref().unwrap()[0]["data"],
        "opaque-only"
    );
}

#[test]
fn translated_thinking_does_not_filter_native_multimodal_or_extension_arrays() {
    let parts = json!([
        {"type":"text","text":"Inspect"},
        {"type":"image_url","image_url":{"url":"https://example.test/image.png","detail":"high"}},
        {"type":"future_part","payload":{"sentinel":true}},
        {"type":"thinking","thinking":[{"type":"text","text":"extension shape"}]}
    ]);
    let req: ChatFormat = serde_json::from_value(json!({
        "model":"native","messages":[{"role":"assistant","content":parts}]
    }))
    .unwrap();
    assert_eq!(upstream(&req)["messages"][0]["content"], parts);
}

#[test]
fn translated_thinking_keeps_representable_media_and_extension_parts_in_carrier() {
    let parts = json!([
        {"type":"thinking","thinking":"Plan","signature":"signed"},
        {"type":"text","text":"Answer"},
        {"type":"image_url","image_url":{"url":"https://example.test/image.png"}},
        {"type":"future_part","payload":"keep"},
        {"type":"thinking","thinking":[{"type":"text","text":"extension shape"}]}
    ]);
    let req: ChatFormat = serde_json::from_value(json!({
        "model":"entry","messages":[{"role":"assistant","content_blocks":parts,"reasoning_content":"Plan"}]
    })).unwrap();
    assert_eq!(
        upstream(&req)["messages"][0]["content"],
        json!([
            {"type":"text","text":"Answer"},
            {"type":"image_url","image_url":{"url":"https://example.test/image.png"}},
            {"type":"future_part","payload":"keep"},
            {"type":"thinking","thinking":[{"type":"text","text":"extension shape"}]}
        ])
    );
}

async fn translated_thinking_reaches_transport(streaming: bool) {
    let server = MockServer::builder().start().await;
    let response = if streaming {
        let frame = json!({
            "id":"reply-1","model":"gpt-target","choices":[{
                "index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":"stop"
            }]
        });
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(format!("data: {frame}\n\ndata: [DONE]\n\n"))
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id":"reply-1","model":"gpt-target","choices":[{
                "index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"
            }],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }))
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    let req = parse_inbound_request(&json!({
        "model":"entry","max_tokens":64,"messages":[
            {"role":"user","content":"Find it"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"Plan","signature":"transport-signed"},
                {"type":"redacted_thinking","data":"transport-opaque"},
                {"type":"text","text":"Looking."},
                {"type":"tool_use","id":"tool-1","name":"lookup","input":{"key":"exact"}}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"Found."}]}
        ]
    }))
    .unwrap();
    let model: Arc<Model> = Arc::new(
        serde_json::from_value(json!({
            "display_name":"target","provider":"openai","model_name":"gpt-target",
            "provider_key_id":"11111111-1111-1111-1111-111111111111"
        }))
        .unwrap(),
    );
    let key: Arc<ProviderKey> = Arc::new(
        serde_json::from_value(json!({
            "display_name":"test","secret":"sk-test","api_base":server.uri()
        }))
        .unwrap(),
    );
    let context = BridgeContext::new("translated-thinking", model, key);
    let bridge = OpenAiBridge::new();
    if streaming {
        let mut stream = bridge.chat_stream(&req, &context).await.unwrap();
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            if let Some(content) = chunk.unwrap().delta.content {
                text.push_str(&content);
            }
        }
        assert_eq!(text, "ok");
    } else {
        let reply = bridge.chat(&req, &context).await.unwrap();
        assert_eq!(reply.message.content.as_deref(), Some("ok"));
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(wire["stream"], streaming);
    assert_eq!(wire["model"], "gpt-target");
    assert_eq!(wire["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        wire["messages"][1]["content"],
        json!([{"type":"text","text":"Looking."}])
    );
    assert_eq!(wire["messages"][1]["reasoning_content"], "Plan");
    assert_eq!(
        wire["messages"][1]["tool_calls"],
        json!([{
            "id":"tool-1","type":"function",
            "function":{"name":"lookup","arguments":"{\"key\":\"exact\"}"}
        }])
    );
    assert_eq!(
        wire["messages"][2],
        json!({"role":"tool","tool_call_id":"tool-1","content":"Found."})
    );
    assert!(!wire.to_string().contains("transport-signed"));
    assert!(!wire.to_string().contains("transport-opaque"));
}

#[tokio::test]
async fn translated_thinking_nonstream_bridge_filters_carrier_on_wire() {
    translated_thinking_reaches_transport(false).await;
}

#[tokio::test]
async fn translated_thinking_stream_bridge_filters_carrier_on_wire() {
    translated_thinking_reaches_transport(true).await;
}
