//! 两种调度路径均把平台模式带入共享头部选择点。

use std::sync::Arc;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn platform_account_header_rules_reach_bridge_and_direct_dispatches() {
    for (platform, exact) in [(false, false), (true, false), (true, true)] {
        let upstream = MockServer::start().await;
        for (endpoint, answer) in [
            (
                "/v1/chat/completions",
                json!({"id":"chat-test", "object":"chat.completion", "created":0,
                "model":"gpt-4o-mini", "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            ),
            (
                "/v1/messages",
                json!({"id":"msg-test", "type":"message", "role":"assistant",
                "model":"claude-sonnet", "content":[{"type":"text","text":"ok"}],
                "stop_reason":"end_turn", "usage":{"input_tokens":1,"output_tokens":1}}),
            ),
        ] {
            Mock::given(method("POST"))
                .and(path(endpoint))
                .respond_with(ResponseTemplate::new(200).set_body_json(answer))
                .mount(&upstream)
                .await;
        }
        let snapshot = AisixSnapshot::new();
        let forward = if exact {
            json!([
                "ANTHROPIC-WORKSPACE-ID",
                "anthropic-user-profile-id",
                "x-client-tag"
            ])
        } else {
            json!(["*"])
        };
        for (provider, model_name, base) in [
            ("openai", "gpt-4o-mini", format!("{}/v1", upstream.uri())),
            ("anthropic", "claude-sonnet", upstream.uri()),
        ] {
            let key: ProviderKey = serde_json::from_value(json!({
                "display_name": provider, "provider": provider, "adapter": provider,
                "secret": "test-secret", "api_base": base,
                "request": {"forward_client_headers": forward}
            }))
            .unwrap();
            snapshot
                .provider_keys
                .insert(ResourceEntry::new(format!("pk-{provider}"), key, 1));
            let model: Model = serde_json::from_value(json!({
                "display_name": provider, "provider": provider, "model_name": model_name,
                "provider_key_id": format!("pk-{provider}")
            }))
            .unwrap();
            snapshot
                .models
                .insert(ResourceEntry::new(format!("model-{provider}"), model, 1));
        }
        let key: ApiKey = serde_json::from_value(json!({
            "key_hash":"8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891", "allowed_models":["*"]
        })).unwrap();
        snapshot
            .apikeys
            .insert(ResourceEntry::new("key-caller", key, 1));
        let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
        let hub = Arc::new(Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        let mut state =
            ProxyState::new(SnapshotHandle::new(snapshot), hub, &config).without_cache();
        if platform {
            state =
                state.with_platform(aisix_proxy::platform::PlatformSettings::from_secret("test"));
        }
        let app = build_router(state);
        for (endpoint, model) in [
            ("/v1/chat/completions", "openai"),
            ("/v1/messages", "anthropic"),
        ] {
            let response = app.clone().oneshot(Request::builder()
                .method("POST").uri(endpoint)
                .header("authorization", "Bearer sk-caller")
                .header("content-type", "application/json")
                .header("anthropic-workspace-id", "caller-workspace")
                .header("anthropic-user-profile-id", "caller-profile")
                .header("x-client-tag", "visible-tag")
                .body(Body::from(json!({"model":model,"max_tokens":16,"messages":[{"role":"user","content":"hello"}]}).to_string()))
                .unwrap()).await.unwrap();
            let status = response.status();
            let body = to_bytes(response.into_body(), 65_536).await.unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{endpoint}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        let calls = upstream.received_requests().await.unwrap();
        assert_eq!(calls.len(), 2);
        for call in calls {
            for name in ["anthropic-workspace-id", "anthropic-user-profile-id"] {
                assert_eq!(
                    call.headers.contains_key(name),
                    !platform || exact,
                    "{} platform={platform} exact={exact} name={name}",
                    call.url.path()
                );
            }
            assert_eq!(call.headers["x-client-tag"], "visible-tag");
        }
    }
}
