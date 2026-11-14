//! 完整原生回答经假流再编码后仍保留各思考块的签名边界。
use aisix_gateway::{
    structured_output::response_into_fake_stream_chunks, ChatMessage, ChatResponse, FinishReason,
    UsageStats,
};
use aisix_provider_anthropic::AnthropicSseEncoder;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[test]
fn compat_anthropic_fake_stream_preserves_interleaved_signed_blocks() {
    let expected = json!([
        {"type":"thinking","thinking":"first","signature":"first/sig=="},
        {"type":"redacted_thinking","data":"cipher/data=="},
        {"type":"text","text":"between"},
        {"type":"thinking","thinking":"second","signature":"second/sig=="},
        {"type":"text","text":"answer"},
        {"type":"tool_use","id":"call1","name":"read","input":{"path":"notes"}}
    ]);
    let mut message = ChatMessage::assistant("betweenanswer");
    message.content_blocks = Some(expected.as_array().unwrap().clone());
    message
        .extra
        .insert("reasoning_content".into(), "first\nsecond".into());
    message.extra.insert("tool_calls".into(), json!([{"id":"call1","type":"function","function":{"name":"read","arguments":"{\"path\":\"notes\"}"}}]));
    let response = ChatResponse {
        native_envelope: None,
        id: "msg_signed".into(),
        model: "claude-test".into(),
        message,
        finish_reason: FinishReason::ToolCalls,
        logprobs: None,
        more_choices: vec![],
        usage: UsageStats::new(7, 3),
        service_tier: None,
    };
    let mut encoder = AnthropicSseEncoder::new("msg_signed", "alias", 0);
    let mut events = Vec::new();
    for chunk in response_into_fake_stream_chunks(response) {
        events.extend(encoder.next_events(&chunk));
    }
    events.extend(encoder.force_finish());
    let mut blocks = BTreeMap::<u64, Value>::new();
    for event in &events {
        let Some(index) = event.data.get("index").and_then(Value::as_u64) else {
            continue;
        };
        if event.event == "content_block_start" {
            assert!(blocks
                .insert(index, event.data["content_block"].clone())
                .is_none());
        } else if event.event == "content_block_delta" {
            let block = blocks.get_mut(&index).unwrap();
            let delta = &event.data["delta"];
            let field = match delta["type"].as_str().unwrap() {
                "thinking_delta" => "thinking",
                "signature_delta" => "signature",
                "text_delta" => "text",
                "input_json_delta" => "partial_json",
                other => panic!("unexpected delta {other}"),
            };
            let text = block
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
                + delta[field].as_str().unwrap();
            block[field] = text.into();
        }
    }
    let actual: Vec<_> = blocks
        .into_values()
        .map(|mut block| {
            if let Some(partial) = block.as_object_mut().unwrap().remove("partial_json") {
                block["input"] = serde_json::from_str(partial.as_str().unwrap()).unwrap();
            }
            block
        })
        .collect();
    assert_eq!(json!(actual), expected);
    let closing = events
        .iter()
        .find(|event| event.event == "message_delta")
        .unwrap();
    assert_eq!(closing.data["usage"]["input_tokens"], 7);
    assert_eq!(closing.data["usage"]["output_tokens"], 3);
}
