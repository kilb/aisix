//! 两条免费计数接口的固定账号路由；推理请求不读取此配置。

use crate::error::ProxyError;
use aisix_core::models::TokenCounting;
use aisix_core::resource::ResourceEntry;
use aisix_core::{AisixSnapshot, ApiSurface, Model, ProviderKey};
use std::sync::Arc;

/// 持有快照行，映射表按配置解码时建立的索引读取，计数请求不复制整表。
pub(crate) struct Configuration {
    row: Arc<ResourceEntry<TokenCounting>>,
    surface: ApiSurface,
}

/// 源模型仍负责策略；只持有实际计数账号和选中的短模型名。
pub(crate) struct ResolvedCounter {
    pub key: Arc<ResourceEntry<ProviderKey>>,
    pub model_name: String,
    /// 无显式映射时沿用源模型选择；映射指定的模型名由 operator 负责。
    pub model_name_from_source: bool,
}

pub(crate) fn configuration(
    snapshot: &AisixSnapshot,
    surface: ApiSurface,
) -> Result<Option<Configuration>, ProxyError> {
    let row = match snapshot.token_counting.len() {
        0 => return Ok(None),
        1 => snapshot
            .token_counting
            .get_by_name("token_counting")
            .ok_or(ProxyError::ProviderUnavailable)?,
        _ => {
            return Err(ProxyError::UnusableTarget(
                "duplicate token counting configuration".into(),
            ))
        }
    };
    let configured = match surface {
        ApiSurface::Messages => row.messages_count_tokens.is_some(),
        ApiSurface::Responses => row.responses_input_tokens.is_some(),
    };
    Ok(configured.then_some(Configuration { row, surface }))
}

impl Configuration {
    /// 请求入口可为路由组，但映射身份必须是它实际选中的直接模型 ID。
    /// 通配符派生模型可保留实际模型名；语义分类、ensemble 和 embedding 不参与计数。
    pub(crate) fn resolve(
        &self,
        snapshot: &AisixSnapshot,
        source_id: &str,
        source: &Model,
    ) -> Result<ResolvedCounter, ProxyError> {
        if source.is_routing()
            || source.is_semantic()
            || source.is_ensemble()
            || source.is_embedding()
        {
            return Err(ProxyError::TargetRefused(
                "token counting requires a resolved generation model".into(),
            ));
        }
        let target = match self.surface {
            ApiSurface::Messages => self.row.messages_count_tokens.as_ref(),
            ApiSurface::Responses => self.row.responses_input_tokens.as_ref(),
        }
        .ok_or(ProxyError::ProviderUnavailable)?;
        if !target.mappings_valid() {
            return Err(ProxyError::UnusableTarget(
                "invalid token counting model mapping".into(),
            ));
        }
        let key = snapshot
            .provider_keys
            .get_by_id(&target.provider_key_id)
            .ok_or_else(|| {
                ProxyError::UnusableTarget("token counting account is unavailable".into())
            })?;
        if !aisix_core::models::token_count_provider_serves(&key.value, self.surface) {
            return Err(ProxyError::UnusableTarget(
                "token counting account does not serve this protocol".into(),
            ));
        }
        let source_name = crate::dispatch::require_upstream_model(source)?;
        let (model_name, model_name_from_source) = if let Some(mapping) =
            target.mapping_for(source_id)
        {
            let exact_source = snapshot.models.get_by_id(source_id).is_some_and(|row| {
                !row.display_name.contains('*')
                    && row.model_name.as_deref() == Some(mapping.source_model_name.as_str())
                    && !mapping.source_model_name.contains('*')
            });
            if !exact_source || source_name != mapping.source_model_name {
                return Err(ProxyError::UnusableTarget("token counting model mapping no longer matches the source model; update the mapping".into()));
            }
            (mapping.model_name.clone(), false)
        } else {
            (source_name.to_owned(), true)
        };
        Ok(ResolvedCounter {
            key,
            model_name,
            model_name_from_source,
        })
    }
}

/// 复用协议槽遍历；普通工具调用 ID 和工具参数不属于持久对象引用。
pub(crate) fn responses_inline(body: &serde_json::Value) -> bool {
    body.get("previous_response_id")
        .is_none_or(serde_json::Value::is_null)
        && crate::stored_refs::responses_reference(body).is_none()
        && !crate::native_api::response_refs::ReferenceScan::collect(body).needs_review_copy
        && !body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    // 未加网关标签的密文仍绑定原账号；只检查正式顶层条目，不读取工具参数。
                    crate::opaque_id::is_item_reference(item)
                        || (matches!(
                            item.get("type").and_then(serde_json::Value::as_str),
                            Some("reasoning" | "compaction")
                        ) && item
                            .get("encrypted_content")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|content| !content.is_empty()))
                })
            })
}

#[cfg(test)]
mod tests {
    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ProxyConfig};
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SOURCE: &str = "11111111-1111-4111-8111-111111111111";
    const COUNTER: &str = "22222222-2222-4222-8222-222222222222";
    const MODEL: &str = "33333333-3333-4333-8333-333333333333";

    #[derive(Clone, Copy)]
    enum Wire {
        Messages,
        Responses,
    }
    impl Wire {
        fn path(self) -> &'static str {
            match self {
                Self::Messages => "/v1/messages/count_tokens",
                Self::Responses => "/v1/responses/input_tokens",
            }
        }
        fn block(self) -> &'static str {
            match self {
                Self::Messages => "messages_count_tokens",
                Self::Responses => "responses_input_tokens",
            }
        }
        fn provider(self) -> &'static str {
            match self {
                Self::Messages => "anthropic",
                Self::Responses => "openai",
            }
        }
        fn body(self) -> Value {
            match self {
                Self::Messages => {
                    json!({"model":"default", "messages":[{"role":"user", "content":"hi"}]})
                }
                Self::Responses => json!({"model":"default", "input":"hi"}),
            }
        }
    }

    fn configure(snapshot: &AisixSnapshot, wire: Wire, source_version: &str) {
        snapshot.token_counting.insert(ResourceEntry::new("count-config",
            serde_json::from_value::<aisix_core::models::TokenCounting>(json!({wire.block(): {
                "provider_key_id":COUNTER,
                "model_mappings":[{"model_id":MODEL,"source_model_name":source_version,"model_name":"counter-version"}]
            }})).unwrap(), 1));
    }

    fn snapshot(wire: Wire, source: &str, counter: &str) -> AisixSnapshot {
        let snapshot = AisixSnapshot::new();
        for (id, base, secret, groups) in [
            (SOURCE, source, "source-secret", vec!["inference"]),
            (COUNTER, counter, "counter-secret", vec!["count-only"]),
        ] {
            snapshot.provider_keys.insert(ResourceEntry::new(
                id,
                serde_json::from_value(json!({
                    "display_name":id,"provider":wire.provider(),"adapter":wire.provider(),
                    "api_key":secret,"api_base":base,"groups":groups
                }))
                .unwrap(),
                1,
            ));
        }
        snapshot.models.insert(ResourceEntry::new(MODEL, serde_json::from_value(json!({
            "display_name":"default","provider":wire.provider(),"provider_key_id":SOURCE,"model_name":"source-version"
        })).unwrap(), 1));
        snapshot.apikeys.insert(ResourceEntry::new(
            "caller",
            serde_json::from_value(json!({
                "key_hash":"8b6712790a2089c67aa97a2d80022df18cc65c7814350e33baebe79aab508891",
                "allowed_models":["default"],"allowed_provider_groups":["inference"]
            }))
            .unwrap(),
            1,
        ));
        snapshot
    }

    fn state(
        snapshot: AisixSnapshot,
    ) -> (
        crate::ProxyState,
        tokio::sync::mpsc::Receiver<aisix_obs::UsageEvent>,
    ) {
        let cfg = ProxyConfig {
            addr: "127.0.0.1:0".into(),
            request_body_limit_bytes: 1_048_576,
            real_ip: Default::default(),
            request_id: Default::default(),
            url_rewrites: vec![],
            expose_debug_headers: false,
            platform: Default::default(),
            tls: None,
            listeners: vec![],
            thread_per_core: None,
            workers: None,
        };
        let handle = SnapshotHandle::new(snapshot);
        let index = aisix_guardrails::LiveGuardrailIndex::new(handle.clone(), None);
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        (
            crate::ProxyState::new(handle, Arc::new(aisix_gateway::Hub::new()), &cfg)
                .without_cache()
                .with_guardrail_index(index)
                .with_usage_sink(aisix_obs::UsageSink::new(tx)),
            rx,
        )
    }
    fn app(
        snapshot: AisixSnapshot,
    ) -> (
        axum::Router,
        tokio::sync::mpsc::Receiver<aisix_obs::UsageEvent>,
    ) {
        let (state, rx) = state(snapshot);
        (crate::build_router(state), rx)
    }
    fn request(wire: Wire, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(wire.path())
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-caller")
            .header("x-api-key", "sk-caller")
            .body(Body::from(body.to_string()))
            .unwrap()
    }
    async fn upstream(status: u16) -> MockServer {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status).set_body_json(if status == 200 {
                    json!({"input_tokens":73})
                } else {
                    json!({"error":{"message":"counter failure","type":"server_error"}})
                }),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn fixed_counter_outside_inference_groups_uses_mapped_model_once_and_zero_ledger() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            snapshot.budgets.insert(ResourceEntry::new(
                "counter-spend",
                serde_json::from_value(json!({
                    "scope":"provider_key","scope_ref":COUNTER,"period":"month",
                    "exhausted":{"reason":{"message":"spent","scope":"provider_key"}}
                }))
                .unwrap(),
                1,
            ));
            let (app, mut events) = app(snapshot);
            let response = app.oneshot(request(wire, wire.body())).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{}", wire.path());
            let answer: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap())
                    .unwrap();
            assert_eq!(answer["input_tokens"], 73);
            assert!(source.received_requests().await.unwrap().is_empty());
            let requests = counter.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url.path(), wire.path());
            assert_eq!(
                requests[0].body_json::<Value>().unwrap()["model"],
                "counter-version"
            );
            let (header, expected) = match wire {
                Wire::Messages => ("x-api-key", "counter-secret"),
                Wire::Responses => ("authorization", "Bearer counter-secret"),
            };
            assert_eq!(requests[0].headers.get(header).unwrap(), expected);
            let event = events.try_recv().unwrap();
            assert_eq!(event.model_id, MODEL);
            assert_eq!(event.status_code, 200);
            if matches!(wire, Wire::Responses) {
                assert_eq!(
                    event.inference_stage,
                    Some(aisix_core::native_objects::InferenceStage::Management)
                );
            }
            assert_eq!(event.provider_key_id.as_deref(), Some(COUNTER));
            assert_eq!(
                (
                    event.prompt_tokens,
                    event.completion_tokens,
                    event.total_tokens
                ),
                (0, 0, 0)
            );
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn fixed_counter_failure_never_retries_or_falls_back_to_source() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(200).await;
            let counter = upstream(503).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            let (app, _) = app(snapshot);
            let response = app.oneshot(request(wire, wire.body())).await.unwrap();
            assert!(response.status().is_server_error());
            assert!(source.received_requests().await.unwrap().is_empty());
            assert_eq!(counter.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn fixed_counter_stale_mapping_and_missing_target_fail_closed() {
        for wire in [Wire::Messages, Wire::Responses] {
            for stale in [true, false] {
                let source = upstream(200).await;
                let counter = upstream(200).await;
                let snapshot = snapshot(wire, &source.uri(), &counter.uri());
                configure(
                    &snapshot,
                    wire,
                    if stale {
                        "old-version"
                    } else {
                        "source-version"
                    },
                );
                if !stale {
                    snapshot.provider_keys.remove(COUNTER);
                }
                let (app, _) = app(snapshot);
                assert!(!app
                    .oneshot(request(wire, wire.body()))
                    .await
                    .unwrap()
                    .status()
                    .is_success());
                assert!(source.received_requests().await.unwrap().is_empty());
                assert!(counter.received_requests().await.unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn fixed_counter_keeps_source_acl_and_input_guardrail() {
        for wire in [Wire::Messages, Wire::Responses] {
            for guardrail in [false, true] {
                let source = upstream(200).await;
                let counter = upstream(200).await;
                let snapshot = snapshot(wire, &source.uri(), &counter.uri());
                configure(&snapshot, wire, "source-version");
                if guardrail {
                    crate::seed_env_scoped_guardrail(&snapshot,ResourceEntry::new("block",serde_json::from_value(json!({
                        "name":"block-hi","kind":"keyword","hook_point":"input","patterns":[{"kind":"literal","value":"hi"}]
                    })).unwrap(),1));
                } else {
                    let existing = snapshot.apikeys.get_by_id("caller").unwrap();
                    let mut key = (*existing.value).clone();
                    key.allowed_models = vec!["another-model".into()];
                    snapshot
                        .apikeys
                        .insert(ResourceEntry::new("caller", key, 2));
                }
                let (app, _) = app(snapshot);
                let response = app.oneshot(request(wire, wire.body())).await.unwrap();
                assert_eq!(
                    response.status(),
                    if guardrail {
                        StatusCode::UNPROCESSABLE_ENTITY
                    } else {
                        StatusCode::FORBIDDEN
                    }
                );
                assert!(source.received_requests().await.unwrap().is_empty());
                assert!(counter.received_requests().await.unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn unconfigured_count_preserves_source_404() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let (app, _) = app(snapshot(wire, &source.uri(), &counter.uri()));
            assert_eq!(
                app.oneshot(request(wire, wire.body()))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(source.received_requests().await.unwrap().len(), 1);
            assert!(counter.received_requests().await.unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn fixed_counter_does_not_change_generation_or_compaction_permissions() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            let (app, _) = app(snapshot);
            let paths: &[&str] = match wire {
                Wire::Messages => &["/v1/messages"],
                Wire::Responses => &["/v1/responses", "/v1/responses/compact"],
            };
            for path in paths {
                for stream in [false, true] {
                    let mut body = wire.body();
                    body["stream"] = json!(stream);
                    body["max_tokens"] = json!(16);
                    let mut req = request(wire, body);
                    *req.uri_mut() = path.parse().unwrap();
                    let response = app.clone().oneshot(req).await.unwrap();
                    assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
                }
            }
            assert_eq!(
                source.received_requests().await.unwrap().len(),
                paths.len() * 2
            );
            assert!(counter.received_requests().await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn fixed_counter_leaves_stored_file_requests_on_source_account() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(200).await;
            let counter = upstream(503).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            let (app, _) = app(snapshot);
            let mut body = wire.body();
            match wire {
                Wire::Messages => {
                    body["messages"] = json!([{"role":"user","content":[{"type":"document","source":{"type":"file","file_id":"file-account-owned"}}]}])
                }
                Wire::Responses => {
                    body["input"] = json!([{"role":"user","content":[{"type":"input_file","file_id":"file-account-owned"}]}])
                }
            }
            assert_eq!(
                app.oneshot(request(wire, body)).await.unwrap().status(),
                StatusCode::OK
            );
            assert_eq!(source.received_requests().await.unwrap().len(), 1);
            assert!(counter.received_requests().await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn fixed_counter_treats_tool_call_ids_as_inline_history() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            let (app, _) = app(snapshot);
            let mut body = wire.body();
            match wire {
                Wire::Messages => {
                    body["messages"] = json!([
                    {"role":"assistant","content":[{"type":"tool_use","id":"call-inline","name":"f","input":{"file_id":"not-a-reference"}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-inline","content":"done"}]}])
                }
                Wire::Responses => {
                    body["input"] = json!([
                    {"type":"function_call","call_id":"call-inline","name":"f","arguments":"{}"},
                    {"type":"function_call_output","call_id":"call-inline","output":"done"}])
                }
            }
            assert_eq!(
                app.oneshot(request(wire, body)).await.unwrap().status(),
                StatusCode::OK
            );
            assert!(source.received_requests().await.unwrap().is_empty());
            assert_eq!(counter.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn fixed_counter_without_mapping_preserves_actual_source_version() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            snapshot.token_counting.insert(ResourceEntry::new(
                "count-config",
                serde_json::from_value(json!({
                    wire.block():{"provider_key_id":COUNTER}
                }))
                .unwrap(),
                1,
            ));
            let (app, _) = app(snapshot);
            assert_eq!(
                app.oneshot(request(wire, wire.body()))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
            assert!(source.received_requests().await.unwrap().is_empty());
            let requests = counter.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].body_json::<Value>().unwrap()["model"],
                "source-version"
            );
        }
    }

    #[tokio::test]
    async fn fixed_counter_maps_distinct_model_ids_with_identical_upstream_names() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            let original = snapshot.models.get_by_id(MODEL).unwrap();
            let mut another = (*original.value).clone();
            another.display_name = "another-default".into();
            snapshot
                .models
                .insert(ResourceEntry::new("other-model", another, 1));
            for (id, name) in [(MODEL, "chosen"), ("other-model", "chosen-other")] {
                snapshot.models.insert(ResourceEntry::new(name,serde_json::from_value(json!({
                    "display_name":name,"routing":{"strategy":"failover","targets":[{"model_id":id}]}
                })).unwrap(),1));
            }
            let key = snapshot.apikeys.get_by_id("caller").unwrap();
            let mut key = (*key.value).clone();
            key.allowed_models = vec!["chosen".into(), "chosen-other".into()];
            snapshot
                .apikeys
                .insert(ResourceEntry::new("caller", key, 2));
            snapshot.token_counting.insert(ResourceEntry::new("count-config",serde_json::from_value(json!({wire.block():{
                "provider_key_id":COUNTER,"model_mappings":[
                    {"model_id":MODEL,"source_model_name":"source-version","model_name":"counter-version"},
                    {"model_id":"other-model","source_model_name":"source-version","model_name":"counter-other-version"}
                ]
            }})).unwrap(),1));
            let (app, _) = app(snapshot);
            for name in ["chosen", "chosen-other"] {
                let mut body = wire.body();
                body["model"] = json!(name);
                assert_eq!(
                    app.clone()
                        .oneshot(request(wire, body))
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::OK
                );
            }
            assert!(source.received_requests().await.unwrap().is_empty());
            let requests = counter.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0].body_json::<Value>().unwrap()["model"],
                "counter-version"
            );
            assert_eq!(
                requests[1].body_json::<Value>().unwrap()["model"],
                "counter-other-version"
            );
        }
    }

    #[tokio::test]
    async fn fixed_counter_platform_mode_masks_input_and_reports_counter_identity() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            crate::seed_env_scoped_guardrail(&snapshot,ResourceEntry::new("mask",serde_json::from_value(json!({
                "name":"mask-email","kind":"pii","hook_point":"input","detectors":[{"type":"email","action":"mask"}]
            })).unwrap(),1));
            let (state, mut events) = state(snapshot);
            let app = crate::build_router(state.with_platform(
                crate::platform::PlatformSettings::from_secret("isolated-count-test"),
            ));
            let mut body = wire.body();
            match wire {
                Wire::Messages => {
                    body["messages"][0]["content"] = json!("write to alice@example.com")
                }
                Wire::Responses => body["input"] = json!("write to alice@example.com"),
            }
            let response = app.oneshot(request(wire, body)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = to_bytes(response.into_body(), 65536).await.unwrap();
            assert!(source.received_requests().await.unwrap().is_empty());
            let requests = counter.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert!(!String::from_utf8_lossy(&requests[0].body).contains("alice@example.com"));
            let event = events.try_recv().unwrap();
            assert_eq!(event.provider_key_id.as_deref(), Some(COUNTER));
            assert_eq!(event.model_id, MODEL);
            assert_eq!(
                (
                    event.prompt_tokens,
                    event.completion_tokens,
                    event.total_tokens
                ),
                (0, 0, 0)
            );
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn fixed_counter_keeps_source_budget_endpoint_ip_and_rate_limits() {
        for wire in [Wire::Messages, Wire::Responses] {
            for gate in ["budget", "endpoint", "ip", "rpm"] {
                let source = upstream(200).await;
                let counter = upstream(200).await;
                let snapshot = snapshot(wire, &source.uri(), &counter.uri());
                configure(&snapshot, wire, "source-version");
                match gate {
                    "budget" => {
                        snapshot.budgets.insert(ResourceEntry::new(
                            "source-spend",
                            serde_json::from_value(json!({
                                "scope":"provider_key","scope_ref":SOURCE,"period":"month",
                                "exhausted":{"reason":{"message":"spent","scope":"provider_key"}}
                            }))
                            .unwrap(),
                            1,
                        ));
                    }
                    "endpoint" | "rpm" => {
                        let key = snapshot.apikeys.get_by_id("caller").unwrap();
                        let mut doc = serde_json::to_value(&*key.value).unwrap();
                        if gate == "endpoint" {
                            doc["allowed_endpoints"] = json!(["embeddings"]);
                        } else {
                            doc["rate_limit"] = json!({"rpm":1});
                        }
                        snapshot.apikeys.insert(ResourceEntry::new(
                            "caller",
                            serde_json::from_value(doc).unwrap(),
                            2,
                        ));
                    }
                    "ip" => {
                        let model = snapshot.models.get_by_id(MODEL).unwrap();
                        let mut model = (*model.value).clone();
                        model.allowed_cidrs = Some(vec!["192.0.2.0/24".into()]);
                        snapshot.models.insert(ResourceEntry::new(MODEL, model, 2));
                    }
                    _ => unreachable!(),
                }
                let (app, _) = app(snapshot);
                if gate == "rpm" {
                    assert_eq!(
                        app.clone()
                            .oneshot(request(wire, wire.body()))
                            .await
                            .unwrap()
                            .status(),
                        StatusCode::OK
                    );
                }
                let status = app
                    .oneshot(request(wire, wire.body()))
                    .await
                    .unwrap()
                    .status();
                assert_eq!(
                    status,
                    if gate == "budget" || gate == "rpm" {
                        StatusCode::TOO_MANY_REQUESTS
                    } else {
                        StatusCode::FORBIDDEN
                    },
                    "{gate}"
                );
                assert!(source.received_requests().await.unwrap().is_empty());
                assert_eq!(
                    counter.received_requests().await.unwrap().len(),
                    usize::from(gate == "rpm")
                );
            }
        }
    }

    #[tokio::test]
    async fn fixed_counter_snapshot_replacement_and_removal_are_live() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(200).await;
            let counter = upstream(200).await;
            let initial = snapshot(wire, &source.uri(), &counter.uri());
            configure(&initial, wire, "source-version");
            let (state, _) = state(initial);
            let handle = state.snapshot.clone();
            let app = crate::build_router(state);
            for configured in [true, false, true] {
                let replacement = snapshot(wire, &source.uri(), &counter.uri());
                if configured {
                    configure(&replacement, wire, "source-version");
                }
                handle.store(replacement);
                let response = app
                    .clone()
                    .oneshot(request(wire, wire.body()))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let _ = to_bytes(response.into_body(), 65536).await.unwrap();
            }
            assert_eq!(source.received_requests().await.unwrap().len(), 1);
            assert_eq!(counter.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn fixed_counter_duplicate_singleton_or_wrong_protocol_fails_closed() {
        for wire in [Wire::Messages, Wire::Responses] {
            for duplicate in [false, true] {
                let source = upstream(200).await;
                let counter = upstream(200).await;
                let snapshot = snapshot(wire, &source.uri(), &counter.uri());
                configure(&snapshot, wire, "source-version");
                if duplicate {
                    let row = snapshot.token_counting.get_by_id("count-config").unwrap();
                    snapshot.token_counting.insert(ResourceEntry::new(
                        "second-config",
                        (*row.value).clone(),
                        1,
                    ));
                } else {
                    let pk = snapshot.provider_keys.get_by_id(COUNTER).unwrap();
                    let mut pk = (*pk.value).clone();
                    pk.adapter = Some(aisix_core::Adapter::Bedrock);
                    snapshot
                        .provider_keys
                        .insert(ResourceEntry::new(COUNTER, pk, 2));
                }
                let (app, _) = app(snapshot);
                assert!(!app
                    .oneshot(request(wire, wire.body()))
                    .await
                    .unwrap()
                    .status()
                    .is_success());
                assert!(source.received_requests().await.unwrap().is_empty());
                assert!(counter.received_requests().await.unwrap().is_empty());
            }
        }
    }
    #[tokio::test]
    async fn fixed_counter_supplies_native_capability_for_non_native_source() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            let other_provider = match wire {
                Wire::Messages => "openai",
                Wire::Responses => "anthropic",
            };
            let pk = snapshot.provider_keys.get_by_id(SOURCE).unwrap();
            let mut pk = (*pk.value).clone();
            pk.provider = other_provider.into();
            pk.adapter = Some(serde_json::from_value(json!(other_provider)).unwrap());
            snapshot
                .provider_keys
                .insert(ResourceEntry::new(SOURCE, pk, 2));
            let model = snapshot.models.get_by_id(MODEL).unwrap();
            let mut model = (*model.value).clone();
            model.provider = Some(other_provider.into());
            snapshot.models.insert(ResourceEntry::new(MODEL, model, 2));
            let (app, mut events) = app(snapshot);
            let response = app.oneshot(request(wire, wire.body())).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{}", wire.path());
            let _ = to_bytes(response.into_body(), 65536).await.unwrap();
            assert!(source.received_requests().await.unwrap().is_empty());
            let requests = counter.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url.path(), wire.path());
            assert_eq!(
                requests[0].body_json::<Value>().unwrap()["model"],
                "counter-version"
            );
            let event = events.try_recv().unwrap();
            assert_eq!(event.provider_key_id.as_deref(), Some(COUNTER));
            assert_eq!(
                (
                    event.prompt_tokens,
                    event.completion_tokens,
                    event.total_tokens
                ),
                (0, 0, 0)
            );
        }
    }
    #[tokio::test]
    async fn fixed_counter_never_updates_source_generation_health_or_latency() {
        for wire in [Wire::Messages, Wire::Responses] {
            for status in [200, 503] {
                let source = upstream(404).await;
                let counter = MockServer::builder().start().await;
                Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)
                    .set_delay(std::time::Duration::from_millis(250))
                    .set_body_json(if status==200 {json!({"input_tokens":73})} else {json!({"error":{"message":"count unavailable","type":"server_error"}})}))
                    .mount(&counter).await;
                let snapshot = snapshot(wire, &source.uri(), &counter.uri());
                configure(&snapshot, wire, "source-version");
                let model = snapshot.models.get_by_id(MODEL).unwrap();
                let mut model = (*model.value).clone();
                model.cooldown = Some(
                    serde_json::from_value(
                        json!({"enabled":true,"default_seconds":60,"trigger_statuses":[503]}),
                    )
                    .unwrap(),
                );
                snapshot.models.insert(ResourceEntry::new(MODEL, model, 2));
                let (state, _) = state(snapshot);
                let health = state.health.clone();
                let runtime = state.runtime_status.clone();
                if status == 200 {
                    runtime.mark_cooldown(
                        MODEL,
                        std::time::Duration::from_secs(60),
                        "existing generation failure",
                    );
                    runtime.record_latency(MODEL, 1234);
                }
                let before_status = runtime.status(MODEL).status;
                let before_latency = runtime.latency_ewma_ms(MODEL);
                let app = crate::build_router(state);
                let task = tokio::spawn(app.oneshot(request(wire, wire.body())));
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while counter.received_requests().await.unwrap().is_empty() {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                assert!(!task.is_finished());
                assert_eq!(
                    runtime.in_flight(MODEL),
                    0,
                    "helper must not occupy source generation busy slots"
                );
                let response = task.await.unwrap().unwrap();
                assert_eq!(response.status().is_success(), status == 200);
                let _ = to_bytes(response.into_body(), 65536).await.unwrap();
                assert!(source.received_requests().await.unwrap().is_empty());
                assert!(health.all_levels().is_empty());
                assert_eq!(runtime.latency_ewma_ms(MODEL), before_latency);
                assert_eq!(runtime.status(MODEL).status, before_status);
            }
        }
    }
    #[tokio::test]
    async fn fixed_counter_preserves_existing_persistent_context_paths() {
        let cases = [
            (Wire::Messages, json!({"container":"container-owned"})),
            (Wire::Messages, json!({"thread":"thread-owned"})),
            (
                Wire::Responses,
                json!({"previous_response_id":"resp_owned"}),
            ),
            (Wire::Responses, json!({"conversation":"conv_owned"})),
            (
                Wire::Responses,
                json!({"input":[{"type":"item_reference","id":"item_owned"}]}),
            ),
            (
                Wire::Responses,
                json!({"tools":[{"type":"code_interpreter","container":"container-owned"}]}),
            ),
        ];
        for (wire, extra) in cases {
            let source = upstream(200).await;
            let counter = upstream(503).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            let (app, _) = app(snapshot);
            let mut body = wire.body();
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let response = app.oneshot(request(wire, body)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{extra}");
            assert_eq!(
                source.received_requests().await.unwrap().len(),
                1,
                "{extra}"
            );
            assert!(
                counter.received_requests().await.unwrap().is_empty(),
                "{extra}"
            );
        }
    }

    #[tokio::test]
    async fn fixed_counter_never_runs_embedding_semantic_ensemble_or_nested_routing() {
        for wire in [Wire::Messages, Wire::Responses] {
            for kind in ["embedding", "semantic", "ensemble", "nested"] {
                let source = upstream(200).await;
                let counter = upstream(200).await;
                let snapshot = snapshot(wire, &source.uri(), &counter.uri());
                configure(&snapshot, wire, "source-version");
                let mut doc = match kind {
                    "embedding" => {
                        serde_json::to_value(&*snapshot.models.get_by_id(MODEL).unwrap().value)
                            .unwrap()
                    }
                    _ => json!({"display_name":"default"}),
                };
                match kind {
                    "embedding" => doc["embedding"] = json!({"dimensions":8}),
                    "semantic" => {
                        doc["semantic"] = json!({"embedding_model":"embed","default":"member","match":{"threshold":0.9},"routes":[{"name":"r","target":"member","examples":["hi"]}]})
                    }
                    "ensemble" => {
                        doc["ensemble"] =
                            json!({"panel":[{"model":"member"}],"judge":{"model":"member"}})
                    }
                    "nested" => {
                        snapshot.models.insert(ResourceEntry::new("nested-group",serde_json::from_value(json!({"display_name":"nested","routing":{"targets":[{"model":"missing-member"}]}})).unwrap(),1));
                        doc["routing"] = json!({"targets":[{"model_id":"nested-group"}]});
                    }
                    _ => unreachable!(),
                }
                snapshot.models.insert(ResourceEntry::new(
                    MODEL,
                    serde_json::from_value(doc).unwrap(),
                    2,
                ));
                let key = snapshot.apikeys.get_by_id("caller").unwrap();
                let mut key = (*key.value).clone();
                key.allowed_provider_groups.clear();
                snapshot
                    .apikeys
                    .insert(ResourceEntry::new("caller", key, 2));
                let (app, _) = app(snapshot);
                assert!(
                    !app.oneshot(request(wire, wire.body()))
                        .await
                        .unwrap()
                        .status()
                        .is_success(),
                    "{kind}"
                );
                assert!(
                    source.received_requests().await.unwrap().is_empty(),
                    "{kind}"
                );
                assert!(
                    counter.received_requests().await.unwrap().is_empty(),
                    "{kind}"
                );
            }
        }
    }

    #[tokio::test]
    async fn fixed_counter_uses_wildcard_derived_version_without_fixed_mapping() {
        for wire in [Wire::Messages, Wire::Responses] {
            let source = upstream(404).await;
            let counter = upstream(200).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            let row = snapshot.models.get_by_id(MODEL).unwrap();
            let mut model = (*row.value).clone();
            model.display_name = "service/*".into();
            model.model_name = Some("version-*".into());
            snapshot.models.insert(ResourceEntry::new(MODEL, model, 2));
            let key = snapshot.apikeys.get_by_id("caller").unwrap();
            let mut key = (*key.value).clone();
            key.allowed_models = vec!["service/*".into()];
            snapshot
                .apikeys
                .insert(ResourceEntry::new("caller", key, 2));
            snapshot.token_counting.insert(ResourceEntry::new(
                "count-config",
                serde_json::from_value(json!({wire.block():{"provider_key_id":COUNTER}})).unwrap(),
                1,
            ));
            let resolved =
                crate::model_resolve::resolve_model(&snapshot, "service/2026-exact").unwrap();
            let surface = match wire {
                Wire::Messages => aisix_core::ApiSurface::Messages,
                Wire::Responses => aisix_core::ApiSurface::Responses,
            };
            let resolved_counter = super::configuration(&snapshot, surface)
                .unwrap()
                .unwrap()
                .resolve(&snapshot, MODEL, &resolved.value)
                .unwrap();
            assert!(resolved_counter.model_name_from_source);
            let (app, _) = app(snapshot);
            let mut body = wire.body();
            body["model"] = json!("service/2026-exact");
            assert_eq!(
                app.oneshot(request(wire, body)).await.unwrap().status(),
                StatusCode::OK
            );
            assert!(source.received_requests().await.unwrap().is_empty());
            let requests = counter.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].body_json::<Value>().unwrap()["model"],
                "version-2026-exact"
            );
        }
    }

    #[test]
    fn fixed_counter_resolution_allocations_do_not_scale_with_model_body_or_mapping_table() {
        use aisix_response_reference_test_alloc::AllocationScope;
        for (mapping_count, payload_bytes, metadata_bytes) in
            [(1, 16, 0), (10_000, 25 * 1024 * 1024, 8 * 1024 * 1024)]
        {
            let snapshot = snapshot(Wire::Responses, "http://127.0.0.1:1", "http://127.0.0.1:2");
            let source = snapshot.models.get_by_id(MODEL).unwrap();
            let mut source = (*source.value).clone();
            source.background_model_check =
                Some(serde_json::from_value(json!({"enabled":false,"interval_seconds":60,"timeout_seconds":10,"max_tokens":1,"stale_after_seconds":120,"prompt":"m".repeat(metadata_bytes)})).unwrap());
            snapshot.models.insert(ResourceEntry::new(MODEL, source, 2));
            let mappings:Vec<Value>=(0..mapping_count).map(|i|json!({"model_id":if i+1==mapping_count {MODEL.to_owned()} else {format!("other-{i}")},"source_model_name":"source-version","model_name":"counter-version"})).collect();
            snapshot.token_counting.insert(ResourceEntry::new("count-config",serde_json::from_value(json!({"responses_input_tokens":{"provider_key_id":COUNTER,"model_mappings":mappings}})).unwrap(),1));
            let source = snapshot.models.get_by_id(MODEL).unwrap();
            let body = json!({"model":"default","input":"x".repeat(payload_bytes)});
            let _ = super::configuration(&snapshot, aisix_core::ApiSurface::Responses)
                .unwrap()
                .unwrap()
                .resolve(&snapshot, MODEL, &source.value)
                .unwrap();
            let mut nanos = Vec::new();
            let mut max_calls = 0;
            let mut max_bytes = 0;
            for _ in 0..128 {
                let allocation = AllocationScope::start();
                let started = std::time::Instant::now();
                assert!(super::responses_inline(&body));
                let config = super::configuration(&snapshot, aisix_core::ApiSurface::Responses)
                    .unwrap()
                    .unwrap();
                let selected = config.resolve(&snapshot, MODEL, &source.value).unwrap();
                let elapsed = started.elapsed().as_nanos();
                let allocation = allocation.finish();
                assert_eq!(selected.model_name, "counter-version");
                assert!(!selected.model_name_from_source);
                assert!(allocation.bytes <= 512, "{allocation:?}");
                max_calls = max_calls.max(allocation.calls);
                max_bytes = max_bytes.max(allocation.bytes);
                nanos.push(elapsed);
            }
            nanos.sort_unstable();
            println!(
                "{}",
                json!({"probe":"count-classify-and-resolve","mapping_count":mapping_count,"payload_bytes":payload_bytes,"source_metadata_bytes":metadata_bytes,"samples":nanos.len(),"p50_ns":nanos[64],"p95_ns":nanos[121],"max_allocation_calls":max_calls,"max_allocation_bytes":max_bytes})
            );
        }
    }
    #[tokio::test]
    async fn fixed_counter_keeps_raw_reasoning_and_compaction_ciphertext_on_source() {
        for kind in ["reasoning", "compaction"] {
            let wire = Wire::Responses;
            let source = upstream(200).await;
            let counter = upstream(503).await;
            let snapshot = snapshot(wire, &source.uri(), &counter.uri());
            configure(&snapshot, wire, "source-version");
            let (app, mut events) = app(snapshot);
            let mut body = wire.body();
            body["input"] = json!([{"type":kind,"id":"opaque-item-id","encrypted_content":"raw-account-bound-ciphertext"}]);
            let response = app.oneshot(request(wire, body)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{kind}");
            let answer: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap())
                    .unwrap();
            assert_eq!(answer["input_tokens"], 73);
            let requests = source.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1, "{kind}");
            assert_eq!(requests[0].url.path(), "/v1/responses/input_tokens");
            assert_eq!(
                requests[0].headers.get("authorization").unwrap(),
                "Bearer source-secret"
            );
            let forwarded = requests[0].body_json::<Value>().unwrap();
            assert_eq!(forwarded["model"], "source-version");
            assert_eq!(
                forwarded["input"][0]["encrypted_content"],
                "raw-account-bound-ciphertext"
            );
            assert!(
                counter.received_requests().await.unwrap().is_empty(),
                "{kind}"
            );
            let event = events.try_recv().unwrap();
            assert_eq!(event.provider_key_id.as_deref(), Some(SOURCE));
            assert_eq!(
                (
                    event.prompt_tokens,
                    event.completion_tokens,
                    event.total_tokens
                ),
                (0, 0, 0)
            );
        }
    }
}
