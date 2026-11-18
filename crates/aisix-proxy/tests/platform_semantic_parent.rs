//! 语义父模型不支持的端点保持请求错误，不因密钥绑定供应商组变成临时不可用。

use std::sync::Arc;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn request(app: axum::Router, endpoint: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(endpoint)
                .header("authorization", "Bearer sk-caller")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), 65_536).await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn semantic_parent_request_errors_match_for_grouped_and_ungrouped_keys() {
    let upstream = MockServer::start().await;
    for (endpoint, body) in [
        ("/v1/messages/count_tokens", json!({"input_tokens": 7})),
        (
            "/v1/responses",
            json!({"id": "resp-positive", "object": "response", "status": "completed", "model": "gpt-4o-mini", "output": [], "usage": {"input_tokens": 1, "output_tokens": 0, "total_tokens": 1}}),
        ),
        (
            "/v1/embeddings",
            json!({"object": "list", "model": "embed", "data": [{"object": "embedding", "index": 0, "embedding": [0.25, 0.5]}], "usage": {"prompt_tokens": 1, "total_tokens": 1}}),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&upstream)
            .await;
    }
    let mut ungrouped_errors = std::collections::BTreeMap::new();
    for groups in [json!([]), json!(["group-one"])] {
        let snapshot = AisixSnapshot::new();
        let provider: ProviderKey = serde_json::from_value(json!({
            "display_name": "provider", "secret": "test-secret", "provider": "openai",
            "adapter": "openai", "api_base": format!("{}/v1", upstream.uri()),
            "groups": ["group-one"]
        }))
        .unwrap();
        snapshot
            .provider_keys
            .insert(ResourceEntry::new("pk-one", provider, 1));
        let target: Model = serde_json::from_value(json!({
            "display_name": "target", "provider": "openai", "model_name": "gpt-4o-mini",
            "provider_key_id": "pk-one"
        }))
        .unwrap();
        snapshot
            .models
            .insert(ResourceEntry::new("model-target", target, 1));
        let count_provider: ProviderKey = serde_json::from_value(json!({
            "display_name": "count-provider", "secret": "test-secret", "provider": "anthropic",
            "adapter": "anthropic", "api_base": upstream.uri(), "groups": ["group-one"]
        }))
        .unwrap();
        snapshot
            .provider_keys
            .insert(ResourceEntry::new("pk-count", count_provider, 1));
        let count_target: Model = serde_json::from_value(json!({
            "display_name": "count-target", "provider": "anthropic", "model_name": "claude-sonnet",
            "provider_key_id": "pk-count"
        }))
        .unwrap();
        snapshot
            .models
            .insert(ResourceEntry::new("model-count", count_target, 1));
        let parent: Model = serde_json::from_value(json!({
            "display_name": "router", "semantic": {
                "embedding_model": "embed", "default": "target",
                "routes": [{"name": "route", "target": "target", "examples": ["hello"]}],
                "match": {"threshold": 0.5}
            }
        }))
        .unwrap();
        snapshot
            .models
            .insert(ResourceEntry::new("model-router", parent, 1));
        let key: ApiKey = serde_json::from_value(json!({
            "key_hash": "8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891",
            "allowed_models": ["*"], "allowed_provider_groups": groups
        }))
        .unwrap();
        snapshot
            .apikeys
            .insert(ResourceEntry::new("key-one", key, 1));
        let config: ProxyConfig = serde_json::from_value(json!({"addr": "127.0.0.1:0"})).unwrap();
        let hub = Arc::new(Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        let app = build_router(
            ProxyState::new(SnapshotHandle::new(snapshot), hub, &config).without_cache(),
        );
        for (endpoint, mut body) in [
            (
                "/v1/messages/count_tokens",
                json!({"model": "router", "messages": [{"role": "user", "content": "hello"}]}),
            ),
            (
                "/v1/responses",
                json!({"model": "router", "input": "hello"}),
            ),
            (
                "/v1/embeddings",
                json!({"model": "router", "input": "hello"}),
            ),
        ] {
            let before = upstream.received_requests().await.unwrap().len();
            let (status, error) = request(app.clone(), endpoint, body.clone()).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{endpoint} groups={groups}: {error}"
            );
            assert_eq!(upstream.received_requests().await.unwrap().len(), before);
            assert_eq!(error["error"]["type"], "invalid_request_error");
            let message = error["error"]["message"].as_str().unwrap();
            assert!(
                message.contains("model") && message.contains("router"),
                "{endpoint}: {error}"
            );
            if groups == json!([]) {
                ungrouped_errors.insert(endpoint, error);
            } else {
                assert_eq!(error, ungrouped_errors[endpoint], "{endpoint}");
            }

            body["model"] = json!(if endpoint.ends_with("count_tokens") {
                "count-target"
            } else {
                "target"
            });
            let (status, answer) = request(app.clone(), endpoint, body).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{endpoint} groups={groups}: {answer}"
            );
            assert_eq!(
                upstream.received_requests().await.unwrap().len(),
                before + 1
            );
            assert!(answer.get("error").is_none(), "{endpoint}: {answer}");
        }
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 6);
}
