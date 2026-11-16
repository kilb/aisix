//! 真实桥接与公共错误包的跨 crate 回归；提供方仅使用本地 HTTP 组件，不发付费推理。

use std::sync::Arc;

use aisix_gateway::{Bridge, BridgeContext, BridgeError};
use aisix_provider_bedrock::{init_mantle_endpoint, BedrockBridge};
use aisix_proxy::ProxyError;
use axum::body::to_bytes;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn mantle_count_fallback_defaults_ttl_and_filters_html_in_both_error_envelopes() {
    let server = MockServer::builder().start().await;
    init_mantle_endpoint(Some(server.uri()));
    Mock::given(method("POST"))
        .and(path("/model/anthropic.claude-opus-5-5/count-tokens"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("x-amzn-errortype", "ValidationException")
                .set_body_json(
                    json!({"message": "The provided model does not support counting tokens."}),
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages/count_tokens"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["messages"][0]["content"] == "HTML probe" {
                ResponseTemplate::new(502).set_body_raw(
                    "<!DOCTYPE html><html><title>relay.example 502</title></html>",
                    "text/html",
                )
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 77}))
            }
        })
        .mount(&server)
        .await;
    let model = serde_json::from_value(json!({
        "display_name": "native-claude", "provider": "bedrock", "provider_key_id": "pk",
        "model_name": "us.anthropic.claude-opus-5-5"
    }))
    .unwrap();
    let key = serde_json::from_value(json!({
        "display_name": "local-count-only", "api_base": server.uri(), "api_key":
        r#"{"auth_type":"api_key","api_key":"ABSK-local-count-test","region":"us-east-1"}"#
    }))
    .unwrap();
    let ctx = BridgeContext::new("local-count-regression", Arc::new(model), Arc::new(key));
    let bridge = BedrockBridge::new();
    let result = bridge
        .anthropic_count_tokens(
            json!({
                "system": [{"type": "text", "text": "prefix", "cache_control": {"type": "ephemeral"}}],
                "tools": [{"name": "f", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "ttl": "5m"}}]}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(result["input_tokens"], 77);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let sent: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        requests[0].url.path(),
        "/model/anthropic.claude-opus-5-5/count-tokens"
    );
    assert_eq!(
        requests[1].url.path(),
        "/anthropic/v1/messages/count_tokens"
    );

    let error = bridge
        .anthropic_count_tokens(
            json!({"messages": [{"role": "user", "content": "HTML probe"}]}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        BridgeError::UpstreamStatus {
            status: 502,
            parsed: Some(_),
            ..
        }
    ));
    assert!(error.to_string().contains("relay.example"));
    let error = ProxyError::Bridge(error);
    let openai = serde_json::to_value(error.envelope()).unwrap();
    let anthropic: Value = serde_json::from_slice(
        &to_bytes(error.into_anthropic_response().into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    eprintln!(
        "Mantle wire TTLs: system={}, tool={}, message={}; OpenAI={openai}; Anthropic={anthropic}",
        sent["system"][0]["cache_control"],
        sent["tools"][0]["cache_control"],
        sent["messages"][0]["content"][0]["cache_control"]
    );
    assert!(
        sent["system"][0]["cache_control"]["ttl"] == "5m"
            && sent["tools"][0]["cache_control"]["ttl"] == "1h"
            && sent["messages"][0]["content"][0]["cache_control"]["ttl"] == "5m"
            && openai["error"]["message"] == "upstream returned 502 Bad Gateway"
            && anthropic["error"]["message"] == "upstream returned 502 Bad Gateway",
        "runtime→Mantle default TTL or public HTML filtering contract failed"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests
        .iter()
        .all(|request| request.url.path().ends_with("count-tokens")
            || request.url.path().ends_with("count_tokens")));
}
