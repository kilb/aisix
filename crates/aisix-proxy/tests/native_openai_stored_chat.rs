//! 存储 Chat 组件合同：真实路由和 HTTP，归属服务替身不冒充提供商 E2E。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::{future::BoxFuture, StreamExt};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "71111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "72222222-2222-4222-8222-222222222222";
const KEY: &str = "73333333-3333-4333-8333-333333333333";
const OTHER_KEY: &str = "74444444-4444-4444-8444-444444444444";

#[derive(Default)]
struct Registry {
    operations: Mutex<HashMap<Uuid, BeginRequest>>,
    records: Mutex<HashMap<String, (Uuid, NativeRecord)>>,
    fail_bind: bool,
    bindings: Mutex<Vec<BindRequest>>,
    bind_gate: Option<Arc<tokio::sync::Notify>>,
}
macro_rules! unused {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}
impl NativeObjectRegistry for Registry {
    unused!(claim, ClaimRequest);
    unused!(renew, LeaseRequest);
    unused!(reschedule, LeaseRequest);
    unused!(complete, CompleteRequest);
    fn begin(
        &self,
        request: BeginRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut reply = NativeReply::new("pending_submission");
        reply.operation_id = Some(request.operation_id);
        reply.origin = Some(request.origin.clone());
        self.operations
            .lock()
            .unwrap()
            .insert(request.operation_id, request);
        Box::pin(async move { Ok(reply) })
    }
    fn bind(
        &self,
        request: BindRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            self.bindings.lock().unwrap().push(request.clone());
            if self.fail_bind {
                return Err(NativeRegistryError::Unavailable);
            }
            if let Some(gate) = &self.bind_gate {
                gate.notified().await;
            }
            let operations = self.operations.lock().unwrap();
            let operation = &operations[&request.operation_id];
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id":Uuid::new_v4(),"operation_id":request.operation_id,
                "requested_model_id":operation.requested_model_id,
                "requested_model":operation.requested_model,"origin":operation.origin,
                "protocol":"openai","kind":"chat_completion","raw_id":request.raw_id,"version":1
            }))
            .unwrap();
            self.records
                .lock()
                .unwrap()
                .insert(record.raw_id.clone(), (request.api_key_id, record.clone()));
            let mut reply = NativeReply::new("bound");
            reply.record = Some(record);
            Ok(reply)
        })
    }
    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let record = self
            .records
            .lock()
            .unwrap()
            .get(&request.locator.raw_id)
            .filter(|(owner, record)| {
                *owner == request.api_key_id
                    && !record.deleted
                    && request
                        .locator
                        .origin
                        .as_ref()
                        .is_none_or(|origin| record.origin.as_ref() == Some(origin))
            })
            .map(|(_, record)| record.clone());
        Box::pin(async move {
            let mut reply = NativeReply::new("found");
            reply.record = Some(record.ok_or(NativeRegistryError::NotFound)?);
            Ok(reply)
        })
    }
    fn owned(
        &self,
        request: OwnedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let records = self.records.lock().unwrap();
        let mut reply = NativeReply::new("owned");
        reply.records = request
            .raw_ids
            .iter()
            .filter_map(|id| records.get(id))
            .filter(|(owner, record)| {
                *owner == request.api_key_id
                    && !record.deleted
                    && record.origin.as_ref() == Some(&request.origin)
            })
            .map(|(_, record)| record.clone())
            .collect();
        Box::pin(async move { Ok(reply) })
    }
    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        for (owner, record) in self.records.lock().unwrap().values_mut() {
            if *owner == request.api_key_id && record.record_id == request.record_id {
                record.deleted = true;
            }
        }
        Box::pin(async { Ok(NativeReply::new("deleted")) })
    }
}

fn completion(id: &str) -> Value {
    json!({"id":id,"object":"chat.completion","created":1794528000,
    "model":"provider-model","choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],
    "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12},"metadata":{"kept":"yes"}})
}

fn app(
    base: &str,
    registry: Arc<Registry>,
) -> (
    axum::Router,
    SnapshotHandle<AisixSnapshot>,
    tokio::sync::mpsc::Receiver<UsageEvent>,
) {
    app_with_registry(base, Some(registry))
}

fn app_with_registry(
    base: &str,
    registry: Option<Arc<Registry>>,
) -> (
    axum::Router,
    SnapshotHandle<AisixSnapshot>,
    tokio::sync::mpsc::Receiver<UsageEvent>,
) {
    app_configured(base, registry, |_| {})
}

fn app_configured(
    base: &str,
    registry: Option<Arc<Registry>>,
    configure: impl FnOnce(&AisixSnapshot),
) -> (
    axum::Router,
    SnapshotHandle<AisixSnapshot>,
    tokio::sync::mpsc::Receiver<UsageEvent>,
) {
    let snapshot = AisixSnapshot::new();
    let provider:ProviderKey=serde_json::from_value(json!({"display_name":"native","secret":"upstream-key","provider":"openai","api_base":format!("{base}/v1"),"adapter":"openai"})).unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model:Model=serde_json::from_value(json!({"display_name":"native-model","provider":"openai","model_name":"provider-model","provider_key_id":PROVIDER})).unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    for (id, bearer) in [(KEY, "caller"), (OTHER_KEY, "other")] {
        let key: ApiKey = serde_json::from_value(
            json!({"key_hash":ApiKey::hash_bearer(bearer),"allowed_models":["native-model"]}),
        )
        .unwrap();
        snapshot.apikeys.insert(ResourceEntry::new(id, key, 1));
    }
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1024*1024}))
            .unwrap();
    configure(&snapshot);
    let snapshot = SnapshotHandle::new(snapshot);
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    hub.register_family(
        aisix_core::Adapter::Openai,
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let state = ProxyState::new(snapshot.clone(), hub, &config)
        .without_cache()
        .with_guardrail_index(guardrails)
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "native-chat-test",
        ))
        .with_usage_sink(UsageSink::new(tx));
    let state = match registry {
        Some(registry) => state.with_native_objects(registry),
        None => state,
    };
    (build_router(state), snapshot, rx)
}
async fn request(
    router: axum::Router,
    verb: &str,
    route: &str,
    bearer: &str,
    value: Option<Value>,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(verb)
        .uri(route)
        .header("authorization", format!("Bearer {bearer}"));
    let body = if let Some(value) = value {
        request = request.header("content-type", "application/json");
        Body::from(value.to_string())
    } else {
        Body::empty()
    };
    router.oneshot(request.body(body).unwrap()).await.unwrap()
}
async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
}
async fn create(upstream: &MockServer, router: axum::Router, id: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion(id)))
        .mount(upstream)
        .await;
    let response=request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true}))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["id"], id);
}

#[tokio::test]
async fn stored_chat_create_retrieve_messages_update_delete_preserve_native_fields() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _, mut usage) = app(&upstream.uri(), registry.clone());
    create(&upstream, router.clone(), "chatcmpl_owned").await;
    assert_eq!(
        registry.bindings.lock().unwrap()[0]
            .provider_created_at
            .map(|at| at.timestamp()),
        Some(1794528000)
    );
    assert!(registry
        .records
        .lock()
        .unwrap()
        .contains_key("chatcmpl_owned"));
    Mock::given(method("GET"))
        .and(path("/v1/chat/completions/chatcmpl_owned"))
        .and(query_param("include", "extra"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("chatcmpl_owned")))
        .expect(1)
        .mount(&upstream)
        .await;
    let got = request(
        router.clone(),
        "GET",
        "/v1/chat/completions/chatcmpl_owned?include=extra",
        "caller",
        None,
    )
    .await;
    assert_eq!(got.status(), 200);
    let got = json_body(got).await;
    assert_eq!(got["metadata"], json!({"kept":"yes"}));
    assert_eq!(got["model"], "native-model");
    let messages = json!({"object":"list","data":[{"id":"msg_owned","role":"user","content":"hello"}],"has_more":false,"first_id":"msg_owned","last_id":"msg_owned"});
    Mock::given(method("GET"))
        .and(path("/v1/chat/completions/chatcmpl_owned/messages"))
        .and(query_param("order", "desc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&messages))
        .expect(1)
        .mount(&upstream)
        .await;
    assert_eq!(
        json_body(
            request(
                router.clone(),
                "GET",
                "/v1/chat/completions/chatcmpl_owned/messages?order=desc",
                "caller",
                None
            )
            .await
        )
        .await,
        messages
    );
    let mut updated = completion("chatcmpl_owned");
    updated["metadata"] = json!({"changed":"yes"});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/chatcmpl_owned"))
        .and(body_json(json!({"metadata":{"changed":"yes"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(updated))
        .expect(1)
        .mount(&upstream)
        .await;
    assert_eq!(
        json_body(
            request(
                router.clone(),
                "POST",
                "/v1/chat/completions/chatcmpl_owned",
                "caller",
                Some(json!({"metadata":{"changed":"yes"}}))
            )
            .await
        )
        .await["metadata"],
        json!({"changed":"yes"})
    );
    Mock::given(method("DELETE"))
        .and(path("/v1/chat/completions/chatcmpl_owned"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id":"chatcmpl_owned","object":"chat.completion.deleted","deleted":true}),
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    assert_eq!(
        request(
            router.clone(),
            "DELETE",
            "/v1/chat/completions/chatcmpl_owned",
            "caller",
            None
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        request(
            router,
            "GET",
            "/v1/chat/completions/chatcmpl_owned",
            "caller",
            None
        )
        .await
        .status(),
        404
    );
    let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
    assert_eq!(
        rows.iter().filter(|r| r.prompt_tokens > 0).count(),
        1,
        "管理查询不能重复计费"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn unknown_foreign_and_rotated_account_ids_never_probe_upstream() {
    let upstream = MockServer::builder().start().await;
    let (router, snapshot, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    create(&upstream, router.clone(), "chatcmpl_owned").await;
    for (id, bearer) in [("chatcmpl_owned", "other"), ("chatcmpl_unknown", "caller")] {
        assert_eq!(
            request(
                router.clone(),
                "GET",
                &format!("/v1/chat/completions/{id}"),
                bearer,
                None
            )
            .await
            .status(),
            404
        );
    }
    let mut provider = (*snapshot
        .load()
        .provider_keys
        .get_by_id(PROVIDER)
        .unwrap()
        .value)
        .clone();
    provider.api_key = "rotated-key".into();
    snapshot
        .load()
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 2));
    assert!(!request(
        router,
        "GET",
        "/v1/chat/completions/chatcmpl_owned",
        "caller",
        None
    )
    .await
    .status()
    .is_success());
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn stored_chat_list_filters_account_rows_and_rejects_foreign_cursor() {
    let upstream = MockServer::builder().start().await;
    let (router, _, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    create(&upstream, router.clone(), "chatcmpl_owned").await;
    Mock::given(method("GET")).and(path("/v1/chat/completions"))
        .and(query_param("model", "provider-model"))
        .and(query_param("metadata[k]", "v"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[completion("chatcmpl_foreign"),completion("chatcmpl_owned")],"has_more":false,"first_id":"chatcmpl_foreign","last_id":"chatcmpl_owned"}))).expect(1).mount(&upstream).await;
    let response = request(
        router.clone(),
        "GET",
        "/v1/chat/completions?model=native-model&metadata[k]=v",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let body = json_body(response).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["first_id"], "chatcmpl_owned");
    assert_eq!(body["last_id"], "chatcmpl_owned");
    assert!(!body.to_string().contains("chatcmpl_foreign"));
    assert_eq!(
        request(
            router.clone(),
            "GET",
            "/v1/chat/completions?model=native-model&after=chatcmpl_foreign",
            "caller",
            None
        )
        .await
        .status(),
        404
    );
    assert_eq!(
        request(
            router,
            "GET",
            "/v1/chat/completions?model=native-model&after=chatcmpl_owned&after=chatcmpl_foreign",
            "caller",
            None
        )
        .await
        .status(),
        404
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    upstream.verify().await;
}

#[tokio::test]
async fn failed_creation_binding_hides_id_without_replaying_or_losing_usage() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry {
        fail_bind: true,
        ..Default::default()
    });
    let (router, _, mut usage) = app(&upstream.uri(), registry);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("chatcmpl_secret")))
        .expect(1)
        .mount(&upstream)
        .await;
    let response=request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true}))).await;
    assert!(!response.status().is_success());
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("chatcmpl_secret"));
    let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (10, 2));
    upstream.verify().await;
}

#[tokio::test]
async fn streamed_binding_failure_keeps_final_usage_and_releases_no_content_or_id() {
    let upstream = MockServer::builder().start().await;
    let (router, _, mut usage) = app(
        &upstream.uri(),
        Arc::new(Registry {
            fail_bind: true,
            ..Default::default()
        }),
    );
    let frames = [
        json!({"id":"chatcmpl_secret","model":"provider-model","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"secret answer"},"finish_reason":null}]}),
        json!({"id":"chatcmpl_secret","model":"provider-model","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"chatcmpl_secret","model":"provider-model","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}),
    ];
    let sse = frames
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response=request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true,"stream":true}))).await;
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("chatcmpl_secret"));
    assert!(!text.contains("secret answer"));
    assert!(text.contains("error"));
    let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (10, 2));
    upstream.verify().await;
}

#[tokio::test]
async fn credential_rotation_while_create_is_pending_cannot_rebind_to_a_new_account() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, snapshot, mut usage) = app(&upstream.uri(), registry.clone());
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            let mut provider = (*snapshot
                .load()
                .provider_keys
                .get_by_id(PROVIDER)
                .unwrap()
                .value)
                .clone();
            provider.api_key = "new-account-credential".into();
            snapshot
                .load()
                .provider_keys
                .insert(ResourceEntry::new(PROVIDER, provider, 2));
            ResponseTemplate::new(200).set_body_json(completion("chatcmpl_old_account"))
        })
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true}))).await;
    assert!(!response.status().is_success());
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("chatcmpl_old_account"));
    assert!(!text.contains("answer"));
    assert!(registry.records.lock().unwrap().is_empty());
    let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (10, 2));
    upstream.verify().await;
}

#[tokio::test]
async fn all_addressed_methods_require_current_model_entitlement_before_any_provider_probe() {
    let upstream = MockServer::builder().start().await;
    let (router, snapshot, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    create(&upstream, router.clone(), "chatcmpl_owned").await;
    let mut key = (*snapshot.load().apikeys.get_by_id(KEY).unwrap().value).clone();
    key.allowed_models = vec!["a-different-model".into()];
    snapshot
        .load()
        .apikeys
        .insert(ResourceEntry::new(KEY, key, 2));
    for (verb, suffix, body) in [
        ("GET", "", None),
        ("GET", "/messages", None),
        ("POST", "", Some(json!({"metadata":{"x":"y"}}))),
        ("DELETE", "", None),
    ] {
        let response = request(
            router.clone(),
            verb,
            &format!("/v1/chat/completions/chatcmpl_owned{suffix}"),
            "caller",
            body,
        )
        .await;
        assert!(!response.status().is_success(), "{verb} {suffix}");
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn all_unknown_or_foreign_lifecycle_methods_fail_without_provider_probes() {
    let upstream = MockServer::builder().start().await;
    let (router, _, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    create(&upstream, router.clone(), "chatcmpl_owned").await;
    for (id, bearer) in [("chatcmpl_unknown", "caller"), ("chatcmpl_owned", "other")] {
        for (verb, suffix, body) in [
            ("GET", "", None),
            ("GET", "/messages", None),
            ("POST", "", Some(json!({"metadata":{"x":"y"}}))),
            ("DELETE", "", None),
        ] {
            let response = request(
                router.clone(),
                verb,
                &format!("/v1/chat/completions/{id}{suffix}"),
                bearer,
                body,
            )
            .await;
            assert_eq!(response.status(), 404, "{verb} {suffix}");
        }
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn unsuccessful_native_delete_never_tombstones_a_live_binding() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _, _) = app(&upstream.uri(), registry.clone());
    create(&upstream, router.clone(), "chatcmpl_owned").await;
    Mock::given(method("DELETE"))
        .and(path("/v1/chat/completions/chatcmpl_owned"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id":"chatcmpl_owned","object":"chat.completion.deleted","deleted":false}),
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router,
        "DELETE",
        "/v1/chat/completions/chatcmpl_owned",
        "caller",
        None,
    )
    .await;
    assert!(!response.status().is_success());
    assert!(!registry.records.lock().unwrap()["chatcmpl_owned"].1.deleted);
    upstream.verify().await;
}

fn stored_stream() -> String {
    let frames = [
        json!({"id":"chatcmpl_stream","created":1794528000,"model":"provider-model","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"held answer"},"finish_reason":null}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}),
        json!({"id":"chatcmpl_stream","created":1794528000,"model":"provider-model","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ];
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

#[tokio::test]
async fn streamed_id_and_content_wait_for_durable_binding() {
    let upstream = MockServer::builder().start().await;
    let gate = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(Registry {
        bind_gate: Some(gate.clone()),
        ..Default::default()
    });
    let (router, _, mut usage) = app(&upstream.uri(), registry.clone());
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stored_stream()),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true,"stream":true}))).await;
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .is_err(),
        "持久绑定之前不能发出 ID、正文或结束帧"
    );
    assert_eq!(registry.bindings.lock().unwrap().len(), 1);
    assert!(registry.records.lock().unwrap().is_empty());
    gate.notify_one();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        assert!(registry
            .records
            .lock()
            .unwrap()
            .contains_key("chatcmpl_stream"));
        bytes.extend_from_slice(&chunk.unwrap());
    }
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("chatcmpl_stream"));
    assert!(text.contains("held answer"));
    assert_eq!(
        registry.bindings.lock().unwrap()[0]
            .provider_created_at
            .map(|at| at.timestamp()),
        Some(1794528000)
    );
    let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (10, 2));
    upstream.verify().await;
}

#[tokio::test]
async fn cancellation_while_binding_keeps_already_reported_paid_usage() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry {
        bind_gate: Some(Arc::new(tokio::sync::Notify::new())),
        ..Default::default()
    });
    let (router, _, mut usage) = app(&upstream.uri(), registry.clone());
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stored_stream()),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true,"stream":true}))).await;
    let mut stream = response.into_body().into_data_stream();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .is_err()
    );
    assert_eq!(registry.bindings.lock().unwrap().len(), 1);
    drop(stream);
    let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (10, 2));
    assert!(registry.records.lock().unwrap().is_empty());
    upstream.verify().await;
}

#[tokio::test]
async fn list_rechecks_each_rows_original_requested_model_ip_policy() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, snapshot, _) = app(&upstream.uri(), registry.clone());
    create(&upstream, router.clone(), "chatcmpl_owned").await;
    let restricted_id = "75555555-5555-4555-8555-555555555555";
    let mut restricted = (*snapshot.load().models.get_by_id(MODEL).unwrap().value).clone();
    restricted.display_name = "restricted-model".into();
    restricted.allowed_cidrs = Some(vec!["192.0.2.0/24".into()]);
    snapshot
        .load()
        .models
        .insert(ResourceEntry::new(restricted_id, restricted, 1));
    let mut key = (*snapshot.load().apikeys.get_by_id(KEY).unwrap().value).clone();
    key.allowed_models = vec!["native-model".into(), "restricted-model".into()];
    snapshot
        .load()
        .apikeys
        .insert(ResourceEntry::new(KEY, key, 2));
    {
        let mut records = registry.records.lock().unwrap();
        let record = &mut records.get_mut("chatcmpl_owned").unwrap().1;
        record.requested_model_id = Some(Uuid::parse_str(restricted_id).unwrap());
        record.requested_model = "restricted-model".into();
    }
    Mock::given(method("GET")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[completion("chatcmpl_owned")],"has_more":false,"first_id":"chatcmpl_owned","last_id":"chatcmpl_owned"})))
        .expect(1).mount(&upstream).await;
    let response = request(
        router,
        "GET",
        "/v1/chat/completions?model=native-model",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let body = json_body(response).await;
    assert_eq!(body["data"], json!([]));
    assert!(body["first_id"].is_null());
    assert!(body["last_id"].is_null());
    assert!(!body.to_string().contains("chatcmpl_owned"));
    upstream.verify().await;
}

fn block_all(snapshot: &AisixSnapshot, hook: &str) {
    let script = if hook == "input" {
        "export function checkInput() { return { action: 'block', reason_code: 'blocked' }; }"
    } else {
        "export function checkOutput() { return { action: 'block', reason_code: 'blocked' }; }"
    };
    let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
        "name":"stored-chat-block-all","enabled":true,"kind":"custom","hook_point":hook,
        "fail_open":false,"script":script,"timeout_ms":5000
    }))
    .unwrap();
    let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
        "guardrail_id":"stored-chat-guardrail","scope_type":"env","priority":100
    }))
    .unwrap();
    snapshot
        .guardrails
        .insert(ResourceEntry::new("stored-chat-guardrail", guardrail, 1));
    snapshot.guardrail_attachments.insert(ResourceEntry::new(
        "stored-chat-attachment",
        attachment,
        1,
    ));
}

#[tokio::test]
async fn all_stored_chat_lifecycle_methods_run_unconditional_input_guardrails() {
    for operation in ["list", "get", "messages", "update", "delete"] {
        let upstream = MockServer::builder().start().await;
        let (router, snapshot, mut usage) = app(&upstream.uri(), Arc::new(Registry::default()));
        create(&upstream, router.clone(), "chatcmpl_owned").await;
        block_all(&snapshot.load(), "input");
        let (verb, route, body) = match operation {
            "list" => ("GET", "/v1/chat/completions?model=native-model", None),
            "messages" => ("GET", "/v1/chat/completions/chatcmpl_owned/messages", None),
            "update" => (
                "POST",
                "/v1/chat/completions/chatcmpl_owned",
                Some(json!({"metadata":{}})),
            ),
            "delete" => ("DELETE", "/v1/chat/completions/chatcmpl_owned", None),
            _ => ("GET", "/v1/chat/completions/chatcmpl_owned", None),
        };
        let response = request(router, verb, route, "caller", body).await;
        assert_eq!(response.status(), 422, "{operation}");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "{operation}"
        );
        let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
        assert_eq!(rows.len(), 2, "{operation}");
        let management = rows
            .iter()
            .find(|row| row.inference_stage == Some(InferenceStage::Management))
            .unwrap();
        assert!(management.guardrail_blocked, "{operation}");
        assert_eq!(
            (management.prompt_tokens, management.completion_tokens),
            (0, 0)
        );
    }
}

#[tokio::test]
async fn all_stored_chat_lifecycle_methods_hold_output_for_unconditional_guardrails() {
    for operation in ["list", "get", "messages", "update", "delete"] {
        let upstream = MockServer::builder().start().await;
        let (router, snapshot, mut usage) = app(&upstream.uri(), Arc::new(Registry::default()));
        create(&upstream, router.clone(), "chatcmpl_owned").await;
        block_all(&snapshot.load(), "output");
        let (verb, upstream_path, route, body, response_body) = match operation {
            "list" => (
                "GET",
                "/v1/chat/completions",
                "/v1/chat/completions?model=native-model",
                None,
                json!({"object":"list","data":[completion("chatcmpl_owned")],"has_more":false,"first_id":"chatcmpl_owned","last_id":"chatcmpl_owned"}),
            ),
            "messages" => (
                "GET",
                "/v1/chat/completions/chatcmpl_owned/messages",
                "/v1/chat/completions/chatcmpl_owned/messages",
                None,
                json!({"object":"list","data":[{"id":"msg_owned","role":"user","content":"private message"}],"has_more":false}),
            ),
            "update" => (
                "POST",
                "/v1/chat/completions/chatcmpl_owned",
                "/v1/chat/completions/chatcmpl_owned",
                Some(json!({"metadata":{}})),
                completion("chatcmpl_owned"),
            ),
            "delete" => (
                "DELETE",
                "/v1/chat/completions/chatcmpl_owned",
                "/v1/chat/completions/chatcmpl_owned",
                None,
                json!({"id":"chatcmpl_owned","object":"chat.completion.deleted","deleted":true}),
            ),
            _ => (
                "GET",
                "/v1/chat/completions/chatcmpl_owned",
                "/v1/chat/completions/chatcmpl_owned",
                None,
                completion("chatcmpl_owned"),
            ),
        };
        Mock::given(method(verb))
            .and(path(upstream_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .expect(1)
            .mount(&upstream)
            .await;
        let response = request(router, verb, route, "caller", body).await;
        assert_eq!(response.status(), 422, "{operation}");
        let body = json_body(response).await.to_string();
        assert!(!body.contains("chatcmpl_owned"), "{operation}");
        assert!(!body.contains("private message"), "{operation}");
        let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
        assert_eq!(rows.len(), 2, "{operation}");
        let management = rows
            .iter()
            .find(|row| row.inference_stage == Some(InferenceStage::Management))
            .unwrap();
        assert!(management.guardrail_blocked, "{operation}");
        assert_eq!(
            (management.prompt_tokens, management.completion_tokens),
            (0, 0)
        );
        upstream.verify().await;
    }
}

#[tokio::test]
async fn owner_reassignment_while_create_is_pending_cannot_rebind_to_a_new_tenant() {
    let original_team = "76666666-6666-4666-8666-666666666666";
    let original_user = "77777777-7777-4777-8777-777777777777";
    let replacement_owner = "78888888-8888-4888-8888-888888888888";
    for changed_owner in ["team", "user"] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, snapshot, mut usage) = app(&upstream.uri(), registry.clone());
        let mut key = (*snapshot.load().apikeys.get_by_id(KEY).unwrap().value).clone();
        key.team_id = Some(original_team.into());
        key.user_id = Some(original_user.into());
        snapshot
            .load()
            .apikeys
            .insert(ResourceEntry::new(KEY, key, 2));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_: &wiremock::Request| {
                let mut key = (*snapshot.load().apikeys.get_by_id(KEY).unwrap().value).clone();
                if changed_owner == "team" {
                    key.team_id = Some(replacement_owner.into());
                } else {
                    key.user_id = Some(replacement_owner.into());
                }
                snapshot
                    .load()
                    .apikeys
                    .insert(ResourceEntry::new(KEY, key, 3));
                ResponseTemplate::new(200).set_body_json(completion("chatcmpl_original_tenant"))
            })
            .expect(1)
            .mount(&upstream)
            .await;
        let response = request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true}))).await;
        assert!(!response.status().is_success(), "{changed_owner}");
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("chatcmpl_original_tenant"),
            "{changed_owner}"
        );
        assert!(!text.contains("answer"), "{changed_owner}");
        assert!(
            registry.operations.lock().unwrap().is_empty(),
            "{changed_owner}"
        );
        assert!(
            registry.bindings.lock().unwrap().is_empty(),
            "{changed_owner}"
        );
        assert!(
            registry.records.lock().unwrap().is_empty(),
            "{changed_owner}"
        );
        let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
        assert_eq!(rows.len(), 1, "{changed_owner}");
        assert_eq!(
            (rows[0].prompt_tokens, rows[0].completion_tokens),
            (10, 2),
            "{changed_owner}"
        );
        assert_eq!(rows[0].total_tokens, 12, "{changed_owner}");
        assert_eq!(rows[0].team_id, original_team, "{changed_owner}");
        assert_eq!(rows[0].user_id, original_user, "{changed_owner}");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "{changed_owner}"
        );
        upstream.verify().await;
    }
}

#[tokio::test]
async fn missing_registry_refuses_stored_creation_before_buffered_or_streamed_provider_dispatch() {
    for streaming in [false, true] {
        let upstream = MockServer::builder().start().await;
        let (router, _, mut usage) = app_with_registry(&upstream.uri(), None);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(completion("chatcmpl_must_not_run")),
            )
            .expect(0)
            .mount(&upstream)
            .await;
        let response = request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true,"stream":streaming}))).await;
        assert_eq!(response.status(), 503, "stream={streaming}");
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("chatcmpl_must_not_run"),
            "stream={streaming}"
        );
        assert!(!text.contains("answer"), "stream={streaming}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "stream={streaming}"
        );
        let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
        assert_eq!(rows.len(), 1, "stream={streaming}");
        assert_eq!(
            (
                rows[0].prompt_tokens,
                rows[0].completion_tokens,
                rows[0].total_tokens
            ),
            (0, 0, 0),
            "stream={streaming}"
        );
        upstream.verify().await;
    }
}

fn span_string_attribute(span: &Value, name: &str) -> Option<String> {
    span.get("attributes")?
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == name)?
        .get("value")?
        .get("stringValue")?
        .as_str()
        .map(str::to_owned)
}

#[tokio::test]
async fn metadata_update_upstream_error_exports_screened_request_and_native_error_body() {
    let upstream = MockServer::builder().start().await;
    let collector = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;
    let (router, _, mut usage) = app_configured(
        &upstream.uri(),
        Some(Arc::new(Registry::default())),
        |snapshot| {
            let exporter: aisix_core::ObservabilityExporter = serde_json::from_value(json!({
            "name":"stored-chat-full-content","enabled":true,"kind":"otlp_http",
            "endpoint":format!("{}/v1/traces",collector.uri()),"headers":{},"content_mode":"full"
        })).unwrap();
            snapshot.observability_exporters.insert(ResourceEntry::new(
                "stored-chat-exporter",
                exporter,
                1,
            ));
        },
    );
    create(&upstream, router.clone(), "chatcmpl_owned").await;
    let metadata = json!({"metadata":{"purpose":"stored-chat-metadata-capture-marker"}});
    let upstream_error = json!({"error":{"type":"invalid_request_error","code":"invalid_metadata","param":"metadata","message":"stored-chat-metadata-rejected"}});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/chatcmpl_owned"))
        .and(body_json(&metadata))
        .respond_with(ResponseTemplate::new(400).set_body_json(&upstream_error))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router,
        "POST",
        "/v1/chat/completions/chatcmpl_owned",
        "caller",
        Some(metadata.clone()),
    )
    .await;
    assert_eq!(response.status(), 400);
    let request_id = response.headers()["x-aisix-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(json_body(response).await["error"]["param"], "metadata");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let span = loop {
        let requests = collector.received_requests().await.unwrap();
        let mut found = None;
        for request in requests {
            let envelope: Value = serde_json::from_slice(&request.body).unwrap();
            for resource in envelope["resourceSpans"].as_array().into_iter().flatten() {
                for scope in resource["scopeSpans"].as_array().into_iter().flatten() {
                    for span in scope["spans"].as_array().into_iter().flatten() {
                        if span.to_string().contains(&request_id)
                            && span_string_attribute(span, "gen_ai.operation.name").is_some()
                        {
                            found = Some(span.clone());
                        }
                    }
                }
            }
        }
        if let Some(span) = found {
            break span;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "上游错误的完整内容事件未到达导出器"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    let captured_request = span_string_attribute(&span, "aisix.content.prompt")
        .expect("上游失败须保留已审查的 metadata 请求体");
    let captured_response = span_string_attribute(&span, "aisix.content.response")
        .expect("上游失败须保留该次原生错误体");
    assert_eq!(
        serde_json::from_str::<Value>(&captured_request).unwrap(),
        metadata
    );
    assert_eq!(
        serde_json::from_str::<Value>(&captured_response).unwrap(),
        upstream_error
    );
    let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
    let management = rows
        .iter()
        .find(|row| row.inference_stage == Some(InferenceStage::Management))
        .unwrap();
    assert_eq!(
        (management.prompt_tokens, management.completion_tokens),
        (0, 0)
    );
    upstream.verify().await;
}

fn app_at_chat_root(
    base: &str,
    registry: Arc<Registry>,
) -> (
    axum::Router,
    SnapshotHandle<AisixSnapshot>,
    tokio::sync::mpsc::Receiver<UsageEvent>,
) {
    app_configured(base, Some(registry), |snapshot| {
        let mut provider = (*snapshot.provider_keys.get_by_id(PROVIDER).unwrap().value).clone();
        provider.api_base = Some(base.to_owned());
        snapshot
            .provider_keys
            .insert(ResourceEntry::new(PROVIDER, provider, 2));
    })
}

async fn create_at_chat_root(
    upstream: &MockServer,
    router: axum::Router,
    streaming: bool,
) -> &'static str {
    let id = if streaming {
        "chatcmpl_stream"
    } else {
        "chatcmpl_root"
    };
    let reply = if streaming {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(stored_stream())
    } else {
        ResponseTemplate::new(200).set_body_json(completion(id))
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(reply)
        .expect(1)
        .mount(upstream)
        .await;
    let response=request(router,"POST","/v1/chat/completions","caller",Some(json!({"model":"native-model","messages":[{"role":"user","content":"hello"}],"store":true,"stream":streaming}))).await;
    assert_eq!(response.status(), 200, "stream={streaming}");
    if streaming {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains(id));
        assert!(text.contains("held answer"));
        assert!(text.contains("data: [DONE]"));
        assert!(!text.contains("event: error"));
    } else {
        assert_eq!(json_body(response).await["id"], id);
    }
    id
}

// 自定义根基址沿用 SDK 相对路径语义，创建与全部生命周期请求必须到同一实际路径。
#[tokio::test]
async fn bare_root_stored_chat_buffered_and_streamed_creation_support_all_lifecycle_methods() {
    for streaming in [false, true] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, _, mut usage) = app_at_chat_root(&upstream.uri(), registry.clone());
        let id = create_at_chat_root(&upstream, router.clone(), streaming).await;
        assert!(registry.records.lock().unwrap().contains_key(id));
        let addressed = format!("/chat/completions/{id}");
        Mock::given(method("GET"))
            .and(path(&addressed))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion(id)))
            .expect(1)
            .mount(&upstream)
            .await;
        let retrieved = request(
            router.clone(),
            "GET",
            &format!("/v1/chat/completions/{id}"),
            "caller",
            None,
        )
        .await;
        assert_eq!(retrieved.status(), 200);
        let retrieved = json_body(retrieved).await;
        assert_eq!(retrieved["id"], id);
        assert_eq!(retrieved["model"], "native-model");
        assert_eq!(retrieved["metadata"], json!({"kept":"yes"}));
        Mock::given(method("GET")).and(path("/chat/completions"))
            .and(query_param("model","provider-model"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[completion(id)],"has_more":false,"first_id":id,"last_id":id})))
            .expect(1).mount(&upstream).await;
        let listed = request(
            router.clone(),
            "GET",
            "/v1/chat/completions?model=native-model",
            "caller",
            None,
        )
        .await;
        assert_eq!(listed.status(), 200);
        let listed = json_body(listed).await;
        assert_eq!(listed["data"].as_array().unwrap().len(), 1);
        assert_eq!(listed["data"][0]["id"], id);
        assert_eq!(listed["first_id"], id);
        assert_eq!(listed["last_id"], id);
        let update = json!({"metadata":{"changed":"root"}});
        let mut updated = completion(id);
        updated["metadata"] = update["metadata"].clone();
        Mock::given(method("POST"))
            .and(path(&addressed))
            .and(body_json(&update))
            .respond_with(ResponseTemplate::new(200).set_body_json(updated))
            .expect(1)
            .mount(&upstream)
            .await;
        let changed = request(
            router.clone(),
            "POST",
            &format!("/v1/chat/completions/{id}"),
            "caller",
            Some(update.clone()),
        )
        .await;
        assert_eq!(changed.status(), 200);
        assert_eq!(json_body(changed).await["metadata"], update["metadata"]);
        let messages = json!({"object":"list","data":[{"id":"msg_root","role":"user","content":"hello"}],"has_more":false,"first_id":"msg_root","last_id":"msg_root"});
        Mock::given(method("GET"))
            .and(path(format!("{addressed}/messages")))
            .and(query_param("order", "desc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&messages))
            .expect(1)
            .mount(&upstream)
            .await;
        let history = request(
            router.clone(),
            "GET",
            &format!("/v1/chat/completions/{id}/messages?order=desc"),
            "caller",
            None,
        )
        .await;
        assert_eq!(history.status(), 200);
        assert_eq!(json_body(history).await, messages);
        let deletion = json!({"id":id,"object":"chat.completion.deleted","deleted":true});
        Mock::given(method("DELETE"))
            .and(path(&addressed))
            .respond_with(ResponseTemplate::new(200).set_body_json(&deletion))
            .expect(1)
            .mount(&upstream)
            .await;
        let deleted = request(
            router,
            "DELETE",
            &format!("/v1/chat/completions/{id}"),
            "caller",
            None,
        )
        .await;
        assert_eq!(deleted.status(), 200);
        assert_eq!(json_body(deleted).await, deletion);
        assert!(registry.records.lock().unwrap()[id].1.deleted);
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 6, "stream={streaming}");
        assert!(requests
            .iter()
            .all(|request| !request.url.path().starts_with("/v1/")));
        let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
        let billed: Vec<_> = rows.iter().filter(|row| row.prompt_tokens > 0).collect();
        assert_eq!(billed.len(), 1);
        assert_eq!(
            (billed[0].prompt_tokens, billed[0].completion_tokens),
            (10, 2)
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.inference_stage == Some(InferenceStage::Management))
                .count(),
            5
        );
        upstream.verify().await;
    }
}

// 根路径与显式 /v1 是两个账号边界；旧对象不能借相同主机和凭据探测新路径。
#[tokio::test]
async fn changing_bare_root_to_explicit_v1_invalidates_stored_chat_without_provider_probe() {
    for streaming in [false, true] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, snapshot, mut usage) = app_at_chat_root(&upstream.uri(), registry.clone());
        let id = create_at_chat_root(&upstream, router.clone(), streaming).await;
        assert!(registry.records.lock().unwrap().contains_key(id));
        let mut provider = (*snapshot
            .load()
            .provider_keys
            .get_by_id(PROVIDER)
            .unwrap()
            .value)
            .clone();
        provider.api_base = Some(format!("{}/v1", upstream.uri()));
        snapshot
            .load()
            .provider_keys
            .insert(ResourceEntry::new(PROVIDER, provider, 3));
        for (verb, route, payload) in [
            ("GET", format!("/v1/chat/completions/{id}"), None),
            (
                "GET",
                format!("/v1/chat/completions?model=native-model&after={id}"),
                None,
            ),
            (
                "POST",
                format!("/v1/chat/completions/{id}"),
                Some(json!({"metadata":{"changed":"must-not-run"}})),
            ),
            ("GET", format!("/v1/chat/completions/{id}/messages"), None),
            ("DELETE", format!("/v1/chat/completions/{id}"), None),
        ] {
            let response = request(router.clone(), verb, &route, "caller", payload).await;
            assert_eq!(response.status(), 404, "stream={streaming}: {verb} {route}");
        }
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "stream={streaming}"
        );
        assert!(!registry.records.lock().unwrap()[id].1.deleted);
        let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
        let billed: Vec<_> = rows.iter().filter(|row| row.prompt_tokens > 0).collect();
        assert_eq!(billed.len(), 1);
        assert_eq!(
            (billed[0].prompt_tokens, billed[0].completion_tokens),
            (10, 2)
        );
        upstream.verify().await;
    }
}

// 兼容接口不支持持久存储时仍正常生成，只省略两个不可表示的请求字段。
#[tokio::test]
async fn cohere_compatibility_chat_ignores_unsupported_storage_without_creating_native_records() {
    for streaming in [false, true] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, _, mut usage) =
            app_configured(&upstream.uri(), Some(registry.clone()), |snapshot| {
                let mut provider =
                    (*snapshot.provider_keys.get_by_id(PROVIDER).unwrap().value).clone();
                provider.provider = "cohere".into();
                provider.adapter = Some(aisix_core::Adapter::Openai);
                provider.api_base = Some(format!("{}/v1", upstream.uri()));
                snapshot
                    .provider_keys
                    .insert(ResourceEntry::new(PROVIDER, provider, 2));
                let mut model = (*snapshot.models.get_by_id(MODEL).unwrap().value).clone();
                model.provider = Some("cohere".into());
                snapshot.models.insert(ResourceEntry::new(MODEL, model, 2));
            });
        let id = if streaming {
            "chatcmpl_stream"
        } else {
            "chatcmpl_compatibility"
        };
        let reply = if streaming {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stored_stream())
        } else {
            let mut value = completion(id);
            value.as_object_mut().unwrap().remove("metadata");
            ResponseTemplate::new(200).set_body_json(value)
        };
        Mock::given(method("POST"))
            .and(path("/compatibility/v1/chat/completions"))
            .respond_with(reply)
            .expect(1)
            .mount(&upstream)
            .await;
        let messages = json!([{"role":"user","content":"ordinary compatible generation"}]);
        let extension = json!({"retained":true,"label":"ordinary-native-extension"});
        let response=request(router,"POST","/v1/chat/completions","caller",Some(json!({
            "model":"native-model","messages":messages,"store":true,"metadata":{"unsupported":"client-metadata"},
            "stream":streaming,"temperature":0.25,"top_p":0.75,"max_tokens":7,"custom_extension":extension
        }))).await;
        assert_eq!(response.status(), 200, "stream={streaming}");
        if streaming {
            let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(text.contains(id));
            assert!(text.contains("held answer"));
            assert!(text.contains("data: [DONE]"));
            assert!(!text.contains("event: error"));
        } else {
            let value = json_body(response).await;
            assert_eq!(value["id"], id);
            assert_eq!(value["choices"][0]["message"]["content"], "answer");
        }
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "stream={streaming}");
        assert_eq!(requests[0].url.path(), "/compatibility/v1/chat/completions");
        let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(sent.get("store").is_none(), "stream={streaming}: {sent}");
        assert!(sent.get("metadata").is_none(), "stream={streaming}: {sent}");
        assert_eq!(sent["model"], "provider-model");
        assert_eq!(sent["messages"], messages);
        assert_eq!(sent["stream"], streaming);
        assert_eq!(sent["temperature"], json!(0.25));
        assert_eq!(sent["top_p"], json!(0.75));
        assert_eq!(sent["max_tokens"], 7);
        assert_eq!(sent["custom_extension"], extension);
        assert!(registry.operations.lock().unwrap().is_empty());
        assert!(registry.bindings.lock().unwrap().is_empty());
        assert!(registry.records.lock().unwrap().is_empty());
        let rows: Vec<_> = std::iter::from_fn(|| usage.try_recv().ok()).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (10, 2));
        upstream.verify().await;
    }
}
