// 恢复无条件的全候选检查会拒绝健康主目标；删掉逐目标检查则会向备用目标泄露私有引用。
#[cfg(test)]
mod chat_voice_cache_tests {
    use super::*;
    use serde_json::{json, Value};
    use wiremock::matchers::body_partial_json;

    const PRIMARY_KEY: &str = "43400000-0000-4000-8000-000000000001";
    const FALLBACK_KEY: &str = "43400000-0000-4000-8000-000000000002";

    fn provider(name: &str, upstream: &str, unsafe_override: bool) -> aisix_core::ProviderKey {
        let mut doc = json!({
            "display_name": name, "provider": "openai", "adapter": "openai",
            "api_base": upstream, "api_key": "synthetic-upstream-key"
        });
        if unsafe_override {
            doc["request"] = json!({"default_body_fields": {"audio": {"voice": {"id": "voice_foreign"}, "format": "wav"}}});
        }
        // 控制面 ProviderKeys 的正式写入使用同一 strict validator 与类型解码。
        aisix_core::models::schema::validate_provider_key(&doc).unwrap();
        serde_json::from_value(doc).unwrap()
    }

    fn model(doc: Value) -> Model {
        aisix_core::models::schema::validate_model(&doc).unwrap();
        serde_json::from_value(doc).unwrap()
    }

    fn payload(streaming: bool) -> Value {
        json!({
            "model": "voice-route", "stream": streaming,
            "messages": [{"role": "user", "content": "Read the marker"}]
        })
    }

    async fn mount_primary(upstream: &MockServer, failed: bool) {
        if failed {
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(502).set_body_json(json!({
                    "error": {"type": "server_error", "message": "synthetic primary failure"}
                })))
                .mount(upstream)
                .await;
            return;
        }
        let events = concat!(
            "data: {\"id\":\"chatcmpl_primary\",\"object\":\"chat.completion.chunk\",\"created\":1793491200,\"model\":\"provider-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"HEALTHY_PRIMARY\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_primary\",\"object\":\"chat.completion.chunk\",\"created\":1793491200,\"model\":\"provider-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(json!({"stream": true})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(events),
            )
            .with_priority(1)
            .mount(upstream)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl_primary", "object": "chat.completion", "created": 1793491200,
                "model": "provider-model",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "HEALTHY_PRIMARY"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .with_priority(2)
            .mount(upstream)
            .await;
    }

    async fn fixture(
        cache: bool,
        failed_primary: bool,
        unsafe_fallback: bool,
    ) -> (Router, ProxyState, MockServer, MockServer) {
        let primary = MockServer::builder().start().await;
        mount_primary(&primary, failed_primary).await;
        let fallback = MockServer::builder().start().await;
        mount_primary(&fallback, false).await;
        let snap = AisixSnapshot::new();
        for (id, name, upstream, unsafe_override) in [
            (PRIMARY_KEY, "primary-account", primary.uri(), false),
            (
                FALLBACK_KEY,
                "fallback-account",
                fallback.uri(),
                unsafe_fallback,
            ),
        ] {
            snap.provider_keys.insert(ResourceEntry::new(
                id,
                provider(name, &upstream, unsafe_override),
                1,
            ));
        }
        for (id, name, key) in [
            (
                "43400000-0000-4000-8000-000000000003",
                "primary",
                PRIMARY_KEY,
            ),
            (
                "43400000-0000-4000-8000-000000000004",
                "fallback",
                FALLBACK_KEY,
            ),
        ] {
            snap.models.insert(ResourceEntry::new(
                id,
                model(json!({
                    "display_name": name, "provider": "openai", "model_name": "provider-model",
                    "provider_key_id": key
                })),
                1,
            ));
        }
        snap.models.insert(ResourceEntry::new(
            "43400000-0000-4000-8000-000000000005",
            model(json!({
                "display_name": "voice-route", "routing": {
                    "strategy": "failover", "retries": 0, "max_fallbacks": 1,
                    "targets": [{"model": "primary"}, {"model": "fallback"}]
                }
            })),
            1,
        ));
        snap.apikeys
            .insert(apikey_entry("sk-caller", &["voice-route"]));
        if cache {
            let doc = json!({"name": "voice-cache", "enabled": true, "backend": "memory", "applies_to": "all"});
            aisix_core::models::schema::validate_cache_policy(&doc).unwrap();
            snap.cache_policies.insert(ResourceEntry::new(
                "43400000-0000-4000-8000-000000000006",
                serde_json::from_value(doc).unwrap(),
                1,
            ));
        }
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(openai_test_bridge()));
        let state = build_state_with_cache(snap, hub);
        (build_router(state.clone()), state, primary, fallback)
    }

    async fn call(app: Router, streaming: bool) -> (StatusCode, Option<String>, String) {
        call_with_cache_control(app, streaming, None).await
    }

    async fn call_with_cache_control(
        app: Router,
        streaming: bool,
        directive: Option<&str>,
    ) -> (StatusCode, Option<String>, String) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer sk-caller")
            .header("content-type", "application/json");
        if let Some(directive) = directive {
            request = request.header("cache-control", directive);
        }
        let response = run(
            app,
            request
                .body(Body::from(payload(streaming).to_string()))
                .unwrap(),
        )
        .await;
        let status = response.status();
        let cache = response
            .headers()
            .get("x-aisix-cache")
            .map(|value| value.to_str().unwrap().to_owned());
        let bytes = to_bytes(response.into_body(), 65_536).await.unwrap();
        (status, cache, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn healthy_primary(cache: bool, streaming: bool) {
        let (app, _state, primary, fallback) = fixture(cache, false, true).await;
        for _ in 0..2 {
            let (status, cache_status, body) = call(app.clone(), streaming).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(body.contains("HEALTHY_PRIMARY"), "{body}");
            assert_ne!(cache_status.as_deref(), Some("hit"), "不安全覆盖不能走缓存");
        }
        assert_eq!(primary.received_requests().await.unwrap().len(), 2);
        assert!(fallback.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn chat_voice_cache_json_without_cache_keeps_healthy_primary() {
        healthy_primary(false, false).await;
    }

    #[tokio::test]
    async fn chat_voice_cache_json_with_cache_keeps_healthy_primary() {
        healthy_primary(true, false).await;
    }

    #[tokio::test]
    async fn chat_voice_cache_stream_without_cache_keeps_healthy_primary() {
        healthy_primary(false, true).await;
    }

    #[tokio::test]
    async fn chat_voice_cache_stream_with_cache_keeps_healthy_primary() {
        healthy_primary(true, true).await;
    }

    async fn selected_fallback(streaming: bool) {
        for cache in [false, true] {
            let (_app, state, primary, fallback) = fixture(cache, true, true).await;
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let app = build_router(state.with_usage_sink(aisix_obs::UsageSink::new(tx)));
            let (status, _cache, body) = call(app, streaming).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let error: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(error["error"]["type"], "invalid_request_error");
            assert_eq!(error["error"]["message"], "invalid upstream configuration: request overrides cannot introduce a custom voice reference");
            assert!(!error["error"].as_object().unwrap().contains_key("code"));
            assert!(!body.contains("HEALTHY_PRIMARY"), "{body}");
            assert!(!body.contains("voice_foreign"), "{body}");
            assert_eq!(primary.received_requests().await.unwrap().len(), 1);
            assert!(fallback.received_requests().await.unwrap().is_empty());
            let mut events = Vec::new();
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            assert_eq!(
                events.len(),
                2,
                "真实失败的 A 与本地拒绝的 B 都须保留：{events:?}"
            );
            for (index, event) in events.iter().enumerate() {
                let wire = serde_json::to_value(event).unwrap();
                assert_eq!(wire.get("attempt_index"), Some(&json!(index)));
                assert_eq!(wire.get("is_final_attempt"), Some(&json!(index == 1)));
                assert_eq!(wire.get("prompt_tokens"), Some(&json!(0)));
                assert_eq!(wire.get("completion_tokens"), Some(&json!(0)));
                assert_eq!(wire.get("cost_usd"), Some(&json!(0.0)));
                assert_eq!(
                    event.provider_key_id.as_deref(),
                    Some(if index == 0 {
                        PRIMARY_KEY
                    } else {
                        FALLBACK_KEY
                    })
                );
                assert_eq!(
                    event.attempt_model,
                    if index == 0 { "primary" } else { "fallback" }
                );
                assert_eq!(
                    wire.get("status_code"),
                    Some(&json!(if index == 0 { 502 } else { 400 }))
                );
                assert_eq!(wire.get("upstream_dispatched"), Some(&json!(index == 0)));
            }
        }
    }

    #[tokio::test]
    async fn chat_voice_cache_selected_fallback_json_still_refuses_private_voice() {
        selected_fallback(false).await;
    }

    #[tokio::test]
    async fn chat_voice_cache_selected_fallback_stream_still_refuses_private_voice() {
        selected_fallback(true).await;
    }

    #[tokio::test]
    async fn chat_voice_cache_populated_entry_cannot_skip_changed_voice_override() {
        let (app, state, primary, fallback) = fixture(true, false, false).await;
        for expected in ["miss", "hit"] {
            let (status, cache, body) = call(app.clone(), false).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(cache.as_deref(), Some(expected));
            assert!(body.contains("HEALTHY_PRIMARY"), "{body}");
        }
        assert_eq!(primary.received_requests().await.unwrap().len(), 1);
        state
            .snapshot
            .load()
            .provider_keys
            .insert(ResourceEntry::new(
                FALLBACK_KEY,
                provider("fallback-account", &fallback.uri(), true),
                2,
            ));
        let (status, cache, body) = call(app, false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_ne!(cache.as_deref(), Some("hit"));
        assert!(body.contains("HEALTHY_PRIMARY"), "{body}");
        assert_eq!(primary.received_requests().await.unwrap().len(), 2);
        assert!(fallback.received_requests().await.unwrap().is_empty());
    }

    async fn unsafe_override_does_not_write_a_future_cache_hit(directive: Option<&str>) {
        let (app, state, primary, fallback) = fixture(true, false, true).await;
        let (status, cache, body) = call_with_cache_control(app.clone(), false, directive).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(cache.as_deref(), Some("bypass"));
        assert!(body.contains("HEALTHY_PRIMARY"), "{body}");
        state
            .snapshot
            .load()
            .provider_keys
            .insert(ResourceEntry::new(
                FALLBACK_KEY,
                provider("fallback-account", &fallback.uri(), false),
                2,
            ));
        primary.reset().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl_second", "object": "chat.completion", "created": 1793491200,
                "model": "provider-model",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "SECOND_PRIMARY"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .mount(&primary)
            .await;
        for expected in ["miss", "hit"] {
            let (status, cache, body) = call(app.clone(), false).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(cache.as_deref(), Some(expected));
            assert!(body.contains("SECOND_PRIMARY"), "{body}");
            assert!(!body.contains("HEALTHY_PRIMARY"), "{body}");
        }
        assert_eq!(primary.received_requests().await.unwrap().len(), 1);
        assert!(fallback.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn chat_voice_cache_unsafe_override_does_not_write_a_future_cache_hit() {
        unsafe_override_does_not_write_a_future_cache_hit(None).await;
    }

    #[tokio::test]
    async fn chat_voice_cache_no_cache_cannot_write_unsafe_override_response() {
        unsafe_override_does_not_write_a_future_cache_hit(Some("no-cache")).await;
    }
}
