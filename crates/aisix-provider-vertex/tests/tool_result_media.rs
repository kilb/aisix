//! 本地协议组件测试：真实 HTTP 媒体下载和桥接请求，不代表供应商线上 E2E。

use std::sync::Arc;

use aisix_gateway::{Bridge, BridgeContext, ChatFormat};
use aisix_provider_vertex::VertexBridge;
use futures::StreamExt;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn context(base: &str, model: &str) -> BridgeContext {
    BridgeContext::new(
        "tool-media-request",
        Arc::new(serde_json::from_value(json!({
            "display_name": "tool-media", "provider": "vertex",
            "model_name": model, "provider_key_id": "tool-media-key"
        })).unwrap()),
        Arc::new(serde_json::from_value(json!({
            "display_name": "tool-media-key", "api_base": base,
            "api_key": r#"{"access_token":"synthetic-token","project":"test-project","region":"us-central1"}"#
        })).unwrap()),
    )
}

fn request(base: &str) -> ChatFormat {
    serde_json::from_value(json!({
        "model": "tool-media",
        "messages": [
            {"role": "user", "content": "Read the tool attachments."},
            {"role": "assistant", "tool_calls": [{
                "id": "call-file", "type": "function",
                "function": {"name": "get_files", "arguments": "{}"}
            }]},
            {"role": "tool", "tool_call_id": "call-file", "content": [
                {"type": "text", "text": "tool facts"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PNG}")}},
                {"type": "file", "file": {"file_id": format!("{base}/tool-note.txt"), "format": "text/plain"}},
                {"type": "file", "file": {"file_id": "gs://test-bucket/tool.pdf", "format": "application/pdf"}}
            ]}
        ]
    })).unwrap()
}

// 忘记开启原生 Gemini 能力时，Messages 翻译会在到达桥接前丢掉附件。
#[test]
fn native_gemini_keeps_tool_result_media() {
    let bridge = VertexBridge::new();
    let request = request("https://example.com");
    assert!(bridge.reads_tool_result_media(&request, &context("", "gemini-3-flash-preview")));
    assert!(!bridge.reads_tool_result_media(&request, &context("", "llama-3-70b-instruct-maas")));
}

// 合法媒体不能遮住后续非法块；对照纯文本历史时也必须定位当前块。
#[tokio::test]
async fn unsupported_tool_part_names_its_own_index_after_valid_media() {
    let server = MockServer::builder().start().await;
    for media in [true, false] {
        let mut request = request(&server.uri());
        let blocks = request.messages[2].content_blocks.as_mut().unwrap();
        blocks.truncate(if media { 2 } else { 1 });
        blocks.push(json!({"type": "unsupported_part"}));
        let error = VertexBridge::new()
            .chat(&request, &context(&server.uri(), "gemini-3-flash-preview"))
            .await
            .unwrap_err();
        let expected = if media {
            "messages[2].content[2]: unsupported content part \"unsupported_part\""
        } else {
            "messages[2].content[1]: unsupported content part \"unsupported_part\""
        };
        assert!(
            matches!(&error, aisix_gateway::BridgeError::InvalidRequest(message) if message == expected),
            "{error:?}"
        );
    }
}

// 删除嵌套 parts 或任一下载入口的 tool-result 支持都会丢失附件并使本例失败。
async fn tool_result_exchange(streaming: bool) {
    aisix_gateway::ssrf::init(&aisix_core::config::SsrfProtectionConfig {
        enabled: true,
        allowed_hosts: vec!["127.0.0.1".into()],
    })
    .unwrap();
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/tool-note.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("tool note", "text/plain"))
        .expect(1)
        .mount(&server)
        .await;
    let answer = json!({
        "candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "attachments read"}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 2, "totalTokenCount": 6}
    });
    let action = if streaming {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let response = if streaming {
        ResponseTemplate::new(200).set_body_raw(format!("data: {answer}\n\n"), "text/event-stream")
    } else {
        ResponseTemplate::new(200).set_body_json(answer)
    };
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/test-project/locations/us-central1/publishers/google/models/gemini-3-flash-preview:{action}")))
        .respond_with(response)
        .expect(1)
        .mount(&server).await;

    let bridge = VertexBridge::new();
    let request = request(&server.uri());
    let context = context(&server.uri(), "gemini-3-flash-preview");
    if streaming {
        let mut chunks = bridge.chat_stream(&request, &context).await.unwrap();
        let mut text = String::new();
        while let Some(chunk) = chunks.next().await {
            text.push_str(chunk.unwrap().delta.content.as_deref().unwrap_or(""));
        }
        assert_eq!(text, "attachments read");
    } else {
        assert_eq!(
            bridge
                .chat(&request, &context)
                .await
                .unwrap()
                .message
                .content_str(),
            "attachments read"
        );
    }
    let requests = server.received_requests().await.unwrap();
    let sent = requests
        .iter()
        .find(|request| request.method == "POST")
        .unwrap();
    let sent: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        sent["contents"][2]["parts"],
        json!([{
            "functionResponse": {
                "id": "call-file", "name": "get_files", "response": {"content": "tool facts"},
                "parts": [
                    {"inlineData": {"mimeType": "image/png", "data": PNG}},
                    {"inlineData": {"mimeType": "text/plain", "data": "dG9vbCBub3Rl"}},
                    {"fileData": {"mimeType": "application/pdf", "fileUri": "gs://test-bucket/tool.pdf"}}
                ]
            }
        }])
    );
}

#[tokio::test]
async fn buffered_tool_result_media_reaches_native_function_response() {
    tool_result_exchange(false).await;
}

#[tokio::test]
async fn streamed_tool_result_media_reaches_native_function_response() {
    tool_result_exchange(true).await;
}
