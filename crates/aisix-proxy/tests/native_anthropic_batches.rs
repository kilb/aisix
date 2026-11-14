//! 批次持久恢复组件测试；本地真实 HTTP 不代表提供商端到端验收。
use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_proxy::{platform::PlatformSettings, ProxyState};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";
#[derive(Default)]
struct Registry {
    pending: Mutex<Vec<NativeRecord>>,
    complete: Mutex<Vec<CompleteRequest>>,
    postponed: Mutex<Vec<LeaseRequest>>,
    claim_filters: Mutex<Vec<(Option<NativeProtocol>, Option<NativeObjectKind>)>>,
}
macro_rules! unused {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}
impl NativeObjectRegistry for Registry {
    unused!(begin, BeginRequest);
    unused!(bind, BindRequest);
    unused!(lookup, LookupRequest);
    unused!(owned, OwnedRequest);
    unused!(tombstone, TombstoneRequest);
    fn claim(
        &self,
        request: ClaimRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.claim_filters
            .lock()
            .unwrap()
            .push((request.protocol, request.kind));
        let mut reply = NativeReply::new("leased");
        reply.records = std::mem::take(&mut *self.pending.lock().unwrap());
        Box::pin(async move { Ok(reply) })
    }
    fn renew(&self, _: LeaseRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Ok(NativeReply::new("leased")) })
    }
    fn reschedule(
        &self,
        request: LeaseRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.postponed.lock().unwrap().push(request);
        Box::pin(async { Ok(NativeReply::new("pending")) })
    }
    fn complete(
        &self,
        request: CompleteRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.complete.lock().unwrap().push(request);
        Box::pin(async { Ok(NativeReply::new("completed")) })
    }
}
fn state(base: &str, registry: Arc<Registry>, entries: Value) -> ProxyState {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"native","secret":"upstream-secret","provider":"anthropic",
        "api_base":base,"adapter":"anthropic"
    }))
    .unwrap();
    let platform = PlatformSettings::from_secret("batches-test-boundary");
    let boundary = configured_native_origin(
        &platform.id_key,
        Uuid::parse_str(PROVIDER).unwrap(),
        &provider,
        NativeProtocol::Anthropic,
        NativeObjectKind::MessageBatch,
    )
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"native-model","provider":"anthropic","model_name":"upstream-model",
        "provider_key_id":PROVIDER
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
    let record: NativeRecord = serde_json::from_value(json!({
        "record_id":Uuid::new_v4(),"operation_id":Uuid::new_v4(),"requested_model_id":MODEL,
        "requested_model":"native-model","upstream_model":"upstream-model",
        "protocol":"anthropic","kind":"message_batch","raw_id":"msgbatch_owned",
        "origin":{"model_id":MODEL,"provider_key_id":PROVIDER,"boundary":boundary},
        "lease_generation":3,"lease_expires_at":chrono::Utc::now()+chrono::Duration::seconds(30),
        "observation_headers":{"anthropic-version":"2026-10-21","anthropic-beta":"custom-beta"},
        "entries":entries
    }))
    .unwrap();
    registry.pending.lock().unwrap().push(record);
    ProxyState::new(SnapshotHandle::new(snapshot), Arc::new(Hub::new()), &config)
        .with_platform(platform)
        .with_native_objects(registry)
}
fn entry(id: &str, tier: &str) -> Value {
    json!({"entry_id":id,"requested_model_id":MODEL,
    "model_id":MODEL,"requested_model":"native-model","upstream_model":"upstream-model",
    "requested_service_tier":tier})
}
async fn metadata(upstream: &MockServer, status: &str, results: Option<&str>) {
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_owned"))
        .and(header("x-api-key", "upstream-secret"))
        .and(header("anthropic-version", "2026-10-21"))
        .and(header("anthropic-beta", "custom-beta"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id":"msgbatch_owned",
            "type":"message_batch","processing_status":status,"results_url":results,
            "ended_at":"2026-11-12T04:05:06Z"})),
        )
        .mount(upstream)
        .await;
}

// 按结果行号匹配、丢失零命中存在性或采用返回模型计价都会使断言失败。
#[tokio::test]
async fn recovered_batch_accounts_every_custom_id_and_upstream_cache_shape_once() {
    let upstream = MockServer::builder().start().await;
    metadata(
        &upstream,
        "ended",
        Some("https://untrusted.example/never-follow"),
    )
    .await;
    let success = json!({"custom_id":"fast","result":{"type":"succeeded","message":{
        "id":"msg_success","model":"reported-other-model","usage":{"input_tokens":100,
        "output_tokens":9,"cache_read_input_tokens":0,"cache_creation_input_tokens":20,
        "cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":17},
        "speed":"fast","service_tier":"batch"}}}});
    let failed = json!({"custom_id":"failed","result":{"type":"errored","error":{
        "type":"error","error":{"type":"invalid_request_error","message":"bad param upstream-secret"}}}});
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_owned/results"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{failed}\n{success}\n")))
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let state = state(
        &upstream.uri(),
        registry.clone(),
        json!([entry("fast", "fast"), entry("failed", "standard")]),
    );
    assert_eq!(
        aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
            .await
            .unwrap(),
        0
    );
    let completed = registry.complete.lock().unwrap().clone();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].terminal_status, "ended");
    assert_eq!(completed[0].usage.len(), 2);
    let fast = completed[0]
        .usage
        .iter()
        .find(|u| u.entry_id == "fast")
        .unwrap();
    assert_eq!(
        (
            fast.prompt_tokens,
            fast.completion_tokens,
            fast.cached_prompt_tokens,
            fast.cache_read_tokens,
            fast.cache_creation_tokens
        ),
        (100, 9, 0, 0, 20)
    );
    assert_eq!(fast.cache_usage_reported, Some(true));
    assert_eq!(fast.total_tokens, 0);
    assert_eq!(
        (fast.cache_creation_5m_tokens, fast.cache_creation_1h_tokens),
        (Some(3), Some(17))
    );
    assert_eq!(fast.requested_service_tier.as_deref(), Some("fast"));
    let failed = completed[0]
        .usage
        .iter()
        .find(|u| u.entry_id == "failed")
        .unwrap();
    assert_eq!(failed.status_code, 400);
    assert_eq!(failed.cache_usage_reported, Some(false));
    let wire = serde_json::to_value(failed).unwrap();
    assert_eq!(wire["error_class"], "upstream_status");
    assert!(wire["error_message"]
        .as_str()
        .unwrap()
        .contains("bad param"));
    assert!(!wire["error_message"]
        .as_str()
        .unwrap()
        .contains("upstream-secret"));
    assert!(registry.claim_filters.lock().unwrap().iter().all(|f| *f
        == (
            Some(NativeProtocol::Anthropic),
            Some(NativeObjectKind::MessageBatch)
        )));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}
#[tokio::test]
async fn duplicate_or_missing_results_never_complete_partial_accounting() {
    for lines in [
        "{\"custom_id\":\"a\",\"result\":{\"type\":\"expired\"}}\n",
        "{\"custom_id\":\"a\",\"result\":{\"type\":\"expired\"}}\n{\"custom_id\":\"a\",\"result\":{\"type\":\"expired\"}}\n",
    ] {
        let upstream=MockServer::builder().start().await;metadata(&upstream,"ended",Some("/results")).await;
        Mock::given(method("GET")).and(path("/v1/messages/batches/msgbatch_owned/results"))
            .respond_with(ResponseTemplate::new(200).set_body_string(lines)).mount(&upstream).await;
        let registry=Arc::new(Registry::default());
        let state=state(&upstream.uri(),registry.clone(),json!([entry("a","standard"),entry("b","standard")]));
        assert_eq!(aisix_proxy::native_api::anthropic_batch_worker::run_once(&state).await.unwrap(),0);
        assert!(registry.complete.lock().unwrap().is_empty());
        assert_eq!(registry.postponed.lock().unwrap().len(),1);
    }
}
#[tokio::test]
async fn canceling_is_pending_and_canceled_expired_entries_are_terminal() {
    let upstream = MockServer::builder().start().await;
    metadata(&upstream, "canceling", None).await;
    let registry = Arc::new(Registry::default());
    let state = state(
        &upstream.uri(),
        registry.clone(),
        json!([entry("a", "standard")]),
    );
    assert_eq!(
        aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
            .await
            .unwrap(),
        0
    );
    assert!(registry.complete.lock().unwrap().is_empty());
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}
#[tokio::test]
async fn changed_provider_credential_never_polls_the_new_account() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let state = state(
        &upstream.uri(),
        registry.clone(),
        json!([entry("a", "standard")]),
    );
    let mut provider = state
        .snapshot
        .load()
        .provider_keys
        .get_by_id(PROVIDER)
        .unwrap()
        .value
        .as_ref()
        .clone();
    provider.api_key = "new-account-secret".into();
    state
        .snapshot
        .load()
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 2));
    assert_eq!(
        aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
            .await
            .unwrap(),
        0
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(registry.complete.lock().unwrap().is_empty());
}

// 创建路径必须在原始条目和 custom_id 上固定身份；不能改成聚合一条计费。
#[tokio::test]
async fn native_batch_creation_preserves_params_and_does_not_expose_an_unbound_id() {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"msgbatch_new","type":"message_batch","processing_status":"in_progress",
            "created_at":"2026-11-12T00:00:00Z","expires_at":"2026-11-13T00:00:00Z",
            "results_url":null
        })))
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let state = state(
        &upstream.uri(),
        registry.clone(),
        json!([entry("a", "standard")]),
    );
    let key: aisix_core::ApiKey = serde_json::from_value(json!({
        "key_hash":aisix_core::ApiKey::hash_bearer("caller"),"allowed_models":["native-model"]
    }))
    .unwrap();
    state.snapshot.load().apikeys.insert(ResourceEntry::new(
        "33333333-3333-4333-8333-333333333333",
        key,
        1,
    ));
    let router = aisix_proxy::build_router(state);
    let body = json!({"requests":[{"custom_id":"client-1","params":{
        "model":"native-model","max_tokens":32,"messages":[{"role":"user","content":"hi"}],
        "speed":"fast","metadata":{"user_id":"preserve"}
    }}]});
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/batches")
                .header("x-api-key", "caller")
                .header("anthropic-version", "2026-10-21")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    // 该注册器故意不支持 begin；必须在接触上游前失败，而不是返回提供方对象。
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    let answer: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(answer["type"], "error");
    assert!(!answer.to_string().contains("msgbatch_new"));
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

const MODEL_TWO: &str = "44444444-4444-4444-8444-444444444444";
const KEY: &str = "33333333-3333-4333-8333-333333333333";

#[derive(Default)]
struct ManagedRegistry {
    operations: Mutex<std::collections::HashMap<Uuid, BeginRequest>>,
    records: Mutex<std::collections::HashMap<String, (Uuid, NativeRecord)>>,
    completions: Mutex<Vec<CompleteRequest>>,
    fail_bind: bool,
}
impl NativeObjectRegistry for ManagedRegistry {
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
            if self.fail_bind {
                return Err(NativeRegistryError::Unavailable);
            }
            let operations = self.operations.lock().unwrap();
            let operation = operations
                .get(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?;
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id":Uuid::new_v4(), "operation_id":request.operation_id,
                "requested_model_id":operation.requested_model_id,
                "requested_model":operation.requested_model, "upstream_model":operation.upstream_model,
                "origin":operation.origin, "protocol":"anthropic", "kind":operation.kind,
                "raw_id":request.raw_id, "version":1, "completed":false,
                "entries":operation.entries, "observation_headers":operation.observation_headers,
                "lease_generation":3, "lease_expires_at":chrono::Utc::now()+chrono::Duration::seconds(30)
            })).unwrap();
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
            .filter(|(key, record)| *key == request.api_key_id && !record.deleted)
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
            .filter(|(key, record)| {
                *key == request.api_key_id
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
        let mut records = self.records.lock().unwrap();
        let record = records.values_mut().find(|(key, record)| {
            *key == request.api_key_id && record.record_id == request.record_id
        });
        let result = match record {
            Some((_, record)) if record.version == request.version => {
                record.deleted = true;
                Ok(NativeReply::new("deleted"))
            }
            _ => Err(NativeRegistryError::NotFound),
        };
        Box::pin(async move { result })
    }
    fn claim(
        &self,
        request: ClaimRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut reply = NativeReply::new("leased");
        reply.records = self
            .records
            .lock()
            .unwrap()
            .values()
            .map(|(_, record)| record)
            .filter(|record| {
                !record.deleted
                    && !record.completed
                    && request.record_id.is_none_or(|id| record.record_id == id)
            })
            .take(request.limit as usize)
            .cloned()
            .collect();
        Box::pin(async move { Ok(reply) })
    }
    fn renew(&self, _: LeaseRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Ok(NativeReply::new("leased")) })
    }
    fn reschedule(
        &self,
        _: LeaseRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Ok(NativeReply::new("pending")) })
    }
    fn complete(
        &self,
        request: CompleteRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut records = self.records.lock().unwrap();
        let (_, record) = records
            .values_mut()
            .find(|(_, record)| record.record_id == request.lease.record_id)
            .unwrap();
        if !record.completed {
            record.completed = true;
            record.version += 1;
            self.completions.lock().unwrap().push(request);
        }
        Box::pin(async { Ok(NativeReply::new("completed")) })
    }
}

fn managed_state(
    base: &str,
    registry: Arc<ManagedRegistry>,
    blocked: bool,
) -> (ProxyState, SnapshotHandle<AisixSnapshot>) {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey =
        serde_json::from_value(json!({"display_name":"native","secret":"upstream-secret",
        "provider":"anthropic","adapter":"anthropic","api_base":base}))
        .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    for (id, name, upstream) in [
        (MODEL, "native-model", "upstream-model"),
        (MODEL_TWO, "second-model", "second-upstream"),
    ] {
        let model: Model =
            serde_json::from_value(json!({"display_name":name,"provider":"anthropic",
            "model_name":upstream,"provider_key_id":PROVIDER}))
            .unwrap();
        snapshot.models.insert(ResourceEntry::new(id, model, 1));
    }
    let key:aisix_core::ApiKey=serde_json::from_value(json!({
        "key_hash":aisix_core::ApiKey::hash_bearer("caller"),"allowed_models":["native-model","second-model"]
    })).unwrap();
    snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 1));
    if blocked {
        let guardrail:aisix_core::Guardrail=serde_json::from_value(json!({"name":"block-all","enabled":true,
            "kind":"custom","hook_point":"input","fail_open":false,
            "script":"export function checkInput() { return { action: 'block', reason_code: 'blocked' }; }",
            "timeout_ms":5000})).unwrap();
        snapshot
            .guardrails
            .insert(ResourceEntry::new("batch-guard", guardrail, 1));
        let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
            "guardrail_id":"batch-guard","scope_type":"env","priority":100}))
        .unwrap();
        snapshot.guardrail_attachments.insert(ResourceEntry::new(
            "batch-attachment",
            attachment,
            1,
        ));
    }
    let snapshot = SnapshotHandle::new(snapshot);
    let index = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .unwrap();
    let state = ProxyState::new(snapshot.clone(), Arc::new(Hub::new()), &config)
        .with_guardrail_index(index)
        .with_platform(PlatformSettings::from_secret("batch-route-test"))
        .with_native_objects(registry);
    (state, snapshot)
}
fn batch_metadata(id: &str, status: &str) -> Value {
    json!({"id":id,"type":"message_batch","processing_status":status,
        "request_counts":{"processing":0,"succeeded":2,"errored":0,"canceled":0,"expired":0},
        "created_at":"2026-11-12T00:00:00Z","expires_at":"2026-11-13T00:00:00Z",
        "ended_at":if status=="ended" {Some("2026-11-12T04:05:06Z")}else{None},
        "results_url":if status=="ended" {Some(format!("https://private-account.example/v1/messages/batches/{id}/results"))}else{None}})
}
fn batch_create_body() -> Value {
    json!({"requests":[
        {"custom_id":"first","params":{"model":"native-model","max_tokens":32,"speed":"fast",
            "messages":[{"role":"user","content":"hello"}],"metadata":{"user_id":"keep-me"}}},
        {"custom_id":"second","params":{"model":"second-model","max_tokens":64,"service_tier":"standard_only",
            "messages":[{"role":"user","content":"second"}],"temperature":0.7}}
    ]})
}
async fn call_batch(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> axum::response::Response {
    use tower::ServiceExt;
    let bytes = body.map(|body| body.to_string()).unwrap_or_default();
    router
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", "caller")
                .header("anthropic-version", "2026-10-21")
                .header("anthropic-beta", "custom-beta")
                .header("anthropic-user-profile-id", "profile-client")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(bytes))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn response_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
async fn create_managed(
    router: axum::Router,
    upstream: &MockServer,
    registry: &ManagedRegistry,
) -> Value {
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(batch_metadata("msgbatch_new", "in_progress")),
        )
        .mount(upstream)
        .await;
    let response = call_batch(
        router,
        "POST",
        "/v1/messages/batches",
        Some(batch_create_body()),
    )
    .await;
    assert_eq!(response.status(), 200);
    let value = response_json(response).await;
    assert_eq!(value["id"], "msgbatch_new");
    assert_eq!(registry.operations.lock().unwrap().len(), 1);
    value
}

#[tokio::test]
async fn native_batch_routes_preserve_each_model_tier_and_complete_before_delete() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, _) = managed_state(&upstream.uri(), registry.clone(), false);
    let router = aisix_proxy::build_router(state);
    create_managed(router.clone(), &upstream, &registry).await;
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let expected = batch_create_body();
    let mut mapped = expected.clone();
    mapped["requests"][0]["params"]["model"] = "upstream-model".into();
    mapped["requests"][1]["params"]["model"] = "second-upstream".into();
    assert_eq!(sent, mapped);
    assert_eq!(requests[0].headers["anthropic-version"], "2026-10-21");
    assert_eq!(requests[0].headers["anthropic-beta"], "custom-beta");
    assert_eq!(
        requests[0].headers["anthropic-user-profile-id"],
        "profile-client"
    );
    let operations = registry.operations.lock().unwrap().clone();
    let operation = operations.values().next().unwrap();
    assert_eq!(operation.entries.len(), 2);
    assert_eq!(operation.entries[0].requested_model, "native-model");
    assert_eq!(
        operation.entries[1].requested_model_id.to_string(),
        MODEL_TWO
    );
    assert_eq!(
        operation.entries[0].requested_service_tier.as_deref(),
        Some("fast")
    );
    assert_eq!(
        operation.observation_headers["anthropic-beta"],
        "custom-beta"
    );
    drop(operations);
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_new"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(batch_metadata("msgbatch_new", "ended")),
        )
        .mount(&upstream)
        .await;
    let result = json!({"custom_id":"second","result":{"type":"succeeded","message":{
        "id":"msg_second","type":"message","model":"second-upstream","content":[],
        "usage":{"input_tokens":11,"output_tokens":2,"cache_read_input_tokens":0}}}});
    let first = json!({"custom_id":"first","result":{"type":"succeeded","message":{
        "id":"msg_first","type":"message","model":"upstream-model","content":[],
        "usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":0,"speed":"fast"}}}});
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_new/results"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(format!("{result}\n{first}\n")),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches/msgbatch_new/cancel"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(batch_metadata("msgbatch_new", "canceling")),
        )
        .mount(&upstream)
        .await;
    for (method, path) in [
        ("GET", "/v1/messages/batches/msgbatch_new"),
        ("POST", "/v1/messages/batches/msgbatch_new/cancel"),
    ] {
        let response = call_batch(router.clone(), method, path, None).await;
        assert_eq!(response.status(), 200);
        let value = response_json(response).await;
        if value["processing_status"] == "ended" {
            assert_eq!(
                value["results_url"],
                "/v1/messages/batches/msgbatch_new/results"
            );
        }
        assert!(!value.to_string().contains("private-account"));
    }
    let response = call_batch(
        router.clone(),
        "GET",
        "/v1/messages/batches/msgbatch_new/results",
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let lines: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["custom_id"], "second");
    assert_eq!(lines[0]["result"]["message"]["model"], "second-model");
    assert_eq!(lines[1]["result"]["message"]["model"], "native-model");
    assert!(
        registry.completions.lock().unwrap().is_empty(),
        "读取结果不作为第二次结算"
    );
    Mock::given(method("DELETE"))
        .and(path("/v1/messages/batches/msgbatch_new"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"msgbatch_new","type":"message_batch_deleted"})),
        )
        .mount(&upstream)
        .await;
    let response = call_batch(
        router.clone(),
        "DELETE",
        "/v1/messages/batches/msgbatch_new",
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response_json(response).await["type"],
        "message_batch_deleted"
    );
    let completed = registry.completions.lock().unwrap().clone();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].usage.len(), 2);
    assert_eq!(completed[0].usage[0].entry_id, "second");
    assert_eq!(
        completed[0].usage[0].terminal_status.as_deref(),
        Some("completed")
    );
    drop(completed);
    let before = upstream.received_requests().await.unwrap().len();
    assert_eq!(
        call_batch(router, "GET", "/v1/messages/batches/msgbatch_new", None)
            .await
            .status(),
        404
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn native_batch_bind_failure_hides_id_after_exactly_one_create() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry {
        fail_bind: true,
        ..Default::default()
    });
    let (state, _) = managed_state(&upstream.uri(), registry, false);
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(batch_metadata("msgbatch_not_bound", "in_progress")),
        )
        .mount(&upstream)
        .await;
    let response = call_batch(
        aisix_proxy::build_router(state),
        "POST",
        "/v1/messages/batches",
        Some(batch_create_body()),
    )
    .await;
    assert_eq!(response.status(), 503);
    assert!(!response_json(response)
        .await
        .to_string()
        .contains("msgbatch_not_bound"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn native_batch_zstd_create_preserves_the_real_json_body() {
    use tokio::io::AsyncReadExt;
    use tower::ServiceExt;
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, _) = managed_state(&upstream.uri(), registry.clone(), false);
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(batch_metadata("msgbatch_zstd", "in_progress")),
        )
        .mount(&upstream)
        .await;
    let original = serde_json::to_vec(&batch_create_body()).unwrap();
    let mut encoder = async_compression::tokio::bufread::ZstdEncoder::new(original.as_slice());
    let mut compressed = Vec::new();
    encoder.read_to_end(&mut compressed).await.unwrap();
    assert_ne!(compressed, original);
    let response = aisix_proxy::build_router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/messages/batches")
                .header("x-api-key", "caller")
                .header("anthropic-version", "2026-10-21")
                .header("content-type", "application/json")
                .header("content-encoding", "zstd")
                .body(axum::body::Body::from(compressed))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response_json(response).await["id"], "msgbatch_zstd");
    assert_eq!(
        registry
            .operations
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .entries
            .len(),
        2
    );
    let sent = upstream.received_requests().await.unwrap();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].headers.get("content-encoding").is_none());
    let value: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        value["requests"][0]["params"]["metadata"]["user_id"],
        "keep-me"
    );
}

#[tokio::test]
async fn native_batch_empty_content_still_runs_unconditional_input_guardrail() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, _) = managed_state(&upstream.uri(), registry.clone(), true);
    let body = json!({"requests":[{"custom_id":"empty","params":{"model":"native-model","max_tokens":32,"messages":[]}}]});
    let response = call_batch(
        aisix_proxy::build_router(state),
        "POST",
        "/v1/messages/batches",
        Some(body),
    )
    .await;
    assert_eq!(response.status(), 422);
    let value = response_json(response).await;
    assert_eq!(value["error"]["code"], "content_filter");
    assert!(registry.operations.lock().unwrap().is_empty());
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn native_batch_list_omits_foreign_rows_cursors_and_result_urls() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, _) = managed_state(&upstream.uri(), registry.clone(), false);
    let router = aisix_proxy::build_router(state);
    create_managed(router.clone(), &upstream, &registry).await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[
            batch_metadata("msgbatch_foreign","ended"),batch_metadata("msgbatch_new","ended")],
            "has_more":false,"first_id":"msgbatch_foreign","last_id":"msgbatch_new"})))
        .mount(&upstream)
        .await;
    let response = call_batch(
        router.clone(),
        "GET",
        "/v1/messages/batches?model=native-model&limit=10",
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let value = response_json(response).await;
    assert_eq!(value["data"].as_array().unwrap().len(), 1);
    assert_eq!(value["first_id"], "msgbatch_new");
    assert_eq!(value["last_id"], "msgbatch_new");
    assert!(!value.to_string().contains("msgbatch_foreign"));
    assert!(!value.to_string().contains("private-account"));
    let before = upstream.received_requests().await.unwrap().len();
    assert_eq!(
        call_batch(router, "GET", "/v1/messages/batches/msgbatch_foreign", None)
            .await
            .status(),
        404
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn recovered_batch_canceled_and_expired_results_keep_distinct_outcomes() {
    let upstream = MockServer::builder().start().await;
    metadata(&upstream, "ended", Some("/results")).await;
    let canceled = json!({"custom_id":"cancel","result":{"type":"canceled"}});
    let expired = json!({"custom_id":"expire","result":{"type":"expired"}});
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_owned/results"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{expired}\n{canceled}")))
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let state = state(
        &upstream.uri(),
        registry.clone(),
        json!([entry("cancel", "standard"), entry("expire", "standard")]),
    );
    assert_eq!(
        aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
            .await
            .unwrap(),
        1
    );
    let completed = registry.complete.lock().unwrap();
    assert_eq!(completed.len(), 1);
    let canceled = completed[0]
        .usage
        .iter()
        .find(|usage| usage.entry_id == "cancel")
        .unwrap();
    assert_eq!(canceled.status_code, 499);
    assert_eq!(canceled.terminal_status.as_deref(), Some("cancelled"));
    let expired = completed[0]
        .usage
        .iter()
        .find(|usage| usage.entry_id == "expire")
        .unwrap();
    assert_eq!(expired.status_code, 504);
    assert_eq!(expired.terminal_status.as_deref(), Some("expired"));
    assert_eq!(expired.error_class, "timeout");
    for usage in &completed[0].usage {
        assert_eq!(usage.occurred_at.to_rfc3339(), "2026-11-12T04:05:06+00:00");
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens
            ),
            (0, 0, 0)
        );
        assert_eq!(usage.cache_usage_reported, Some(false));
    }
}

#[tokio::test]
async fn recovered_batch_unknown_or_invalid_success_never_commits_a_partial_result() {
    let invalid = [
        json!({"custom_id":"a","result":{"type":"succeeded","message":{"usage":{"input_tokens":100}}}}),
        json!({"custom_id":"a","result":{"type":"succeeded","message":{"usage":{"input_tokens":-1,"output_tokens":2}}}}),
        json!({"custom_id":"foreign","result":{"type":"expired"}}),
        json!({"custom_id":"a","result":{"type":"future_status"}}),
    ];
    for line in invalid {
        let upstream = MockServer::builder().start().await;
        metadata(&upstream, "ended", Some("/results")).await;
        Mock::given(method("GET"))
            .and(path("/v1/messages/batches/msgbatch_owned/results"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{line}\n")))
            .mount(&upstream)
            .await;
        let registry = Arc::new(Registry::default());
        let state = state(
            &upstream.uri(),
            registry.clone(),
            json!([entry("a", "standard")]),
        );
        assert_eq!(
            aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
                .await
                .unwrap(),
            0
        );
        assert!(registry.complete.lock().unwrap().is_empty());
        assert_eq!(registry.postponed.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn recovered_batch_redirect_never_sends_account_credentials_to_another_host() {
    let upstream = MockServer::builder().start().await;
    let foreign = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_owned"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/steal", foreign.uri())),
        )
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let state = state(
        &upstream.uri(),
        registry.clone(),
        json!([entry("a", "standard")]),
    );
    assert_eq!(
        aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
            .await
            .unwrap(),
        0
    );
    assert!(registry.complete.lock().unwrap().is_empty());
    assert!(foreign.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn native_batch_results_keep_request_errors_and_hide_platform_account_errors() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, _) = managed_state(&upstream.uri(), registry.clone(), false);
    let router = aisix_proxy::build_router(state);
    create_managed(router.clone(), &upstream, &registry).await;
    let first = json!({"custom_id":"first","result":{"type":"errored","error":{"type":"error","error":{
        "type":"invalid_request_error","message":"Fix max_tokens upstream-secret"}}}});
    let second = json!({"custom_id":"second","result":{"type":"errored","error":{"type":"error","error":{
        "type":"api_error","message":"private-account provider production quota exhausted"}}}});
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_new/results"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(format!("{first}\n{second}\n")),
        )
        .mount(&upstream)
        .await;
    let response = call_batch(
        router,
        "GET",
        "/v1/messages/batches/msgbatch_new/results",
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let lines: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0]["result"]["error"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Fix max_tokens"));
    assert!(!std::str::from_utf8(&bytes)
        .unwrap()
        .contains("upstream-secret"));
    assert!(!std::str::from_utf8(&bytes)
        .unwrap()
        .contains("private-account"));
    assert!(!std::str::from_utf8(&bytes)
        .unwrap()
        .contains("production quota"));
    assert_eq!(
        lines[1]["result"]["error"]["error"]["type"],
        "overloaded_error"
    );
}

#[tokio::test]
async fn native_batch_checks_later_models_without_recounting_the_request_gate() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, snapshot) = managed_state(&upstream.uri(), registry.clone(), false);
    let mut second = snapshot
        .load()
        .models
        .get_by_id(MODEL_TWO)
        .unwrap()
        .value
        .as_ref()
        .clone();
    second.rate_limit = Some(serde_json::from_value(json!({"rpd":1})).unwrap());
    snapshot
        .load()
        .models
        .insert(ResourceEntry::new(MODEL_TWO, second, 2));
    let router = aisix_proxy::build_router(state);
    create_managed(router.clone(), &upstream, &registry).await;
    let response = call_batch(
        router,
        "POST",
        "/v1/messages/batches",
        Some(batch_create_body()),
    )
    .await;
    assert_eq!(response.status(), 429);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert_eq!(registry.operations.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn recovered_batch_unicode_errors_fit_the_durable_byte_limit() {
    let upstream = MockServer::builder().start().await;
    metadata(&upstream, "ended", Some("/results")).await;
    let line = json!({"custom_id":"a","result":{"type":"errored","error":{"type":"error","error":{
        "type":"invalid_request_error","message":"错误".repeat(3000)}}}});
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_owned/results"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{line}\n")))
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let state = state(
        &upstream.uri(),
        registry.clone(),
        json!([entry("a", "standard")]),
    );
    assert_eq!(
        aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
            .await
            .unwrap(),
        1
    );
    let completed = registry.complete.lock().unwrap();
    let text = &completed[0].usage[0].error_message;
    assert!(!text.is_empty());
    assert!(text.len() <= 4096, "{} UTF-8 bytes", text.len());
}

#[tokio::test]
async fn native_batch_filtered_empty_backward_pages_keep_the_before_direction() {
    use wiremock::matchers::query_param;
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, _) = managed_state(&upstream.uri(), registry.clone(), false);
    let router = aisix_proxy::build_router(state);
    create_managed(router.clone(), &upstream, &registry).await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches"))
        .and(query_param("before_id", "msgbatch_new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[
            batch_metadata("msgbatch_first","ended"),batch_metadata("msgbatch_last","ended")],
            "has_more":true,"first_id":"msgbatch_first","last_id":"msgbatch_last"})))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches"))
        .and(query_param("before_id", "msgbatch_first"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[batch_metadata("msgbatch_new","ended")],
            "has_more":false,"first_id":"msgbatch_new","last_id":"msgbatch_new"})),
        )
        .mount(&upstream)
        .await;
    let response = call_batch(
        router,
        "GET",
        "/v1/messages/batches?model=native-model&before_id=msgbatch_new&limit=2",
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let value = response_json(response).await;
    assert_eq!(value["data"].as_array().unwrap().len(), 1);
    assert_eq!(value["first_id"], "msgbatch_new");
    assert!(!value.to_string().contains("msgbatch_first"));
    let calls = upstream.received_requests().await.unwrap();
    assert_eq!(calls.len(), 3);
    assert!(calls[2]
        .url
        .query_pairs()
        .any(|(name, value)| name == "before_id" && value == "msgbatch_first"));
    assert!(!calls[2]
        .url
        .query_pairs()
        .any(|(name, _)| name == "after_id"));
}

#[tokio::test]
async fn native_batch_generated_files_are_bound_before_the_result_is_released() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, _) = managed_state(&upstream.uri(), registry.clone(), false);
    let router = aisix_proxy::build_router(state);
    create_managed(router.clone(), &upstream, &registry).await;
    let first = json!({"custom_id":"first","result":{"type":"succeeded","message":{
        "id":"msg_first","type":"message","model":"upstream-model","usage":{"input_tokens":1,"output_tokens":1},
        "content":[{"type":"bash_code_execution_tool_result","tool_use_id":"tool-1","content":{
            "type":"bash_code_execution_result","stdout":"saved","stderr":"","return_code":0,
            "content":[{"type":"bash_code_execution_output","file_id":"file_generated"}]}}]}}});
    let second = json!({"custom_id":"second","result":{"type":"expired"}});
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_new/results"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(format!("{first}\n{second}\n")),
        )
        .mount(&upstream)
        .await;
    let response = call_batch(
        router.clone(),
        "GET",
        "/v1/messages/batches/msgbatch_new/results",
        None,
    )
    .await;
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    assert!(std::str::from_utf8(&bytes)
        .unwrap()
        .contains("file_generated"));
    let operations = registry.operations.lock().unwrap().clone();
    assert_eq!(operations.len(), 2);
    let file = operations
        .values()
        .find(|operation| operation.kind == NativeObjectKind::File)
        .unwrap();
    assert_eq!(file.requested_model, "native-model");
    assert_eq!(file.requested_model_id.to_string(), MODEL);
    drop(operations);
    let records = registry.records.lock().unwrap().clone();
    assert_eq!(
        records["file_generated"].1.kind,
        Some(NativeObjectKind::File)
    );
    drop(records);
    Mock::given(method("GET")).and(path("/v1/files/file_generated"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"file_generated","type":"file",
            "created_at":"2026-11-12T00:00:00Z","filename":"result.txt","mime_type":"text/plain","size_bytes":5})))
        .mount(&upstream).await;
    let response = call_batch(router, "GET", "/v1/files/file_generated", None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response_json(response).await["filename"], "result.txt");
}

#[tokio::test]
async fn native_batch_submission_and_metadata_do_not_emit_inference_terminal_rows() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, _) = managed_state(&upstream.uri(), registry.clone(), false);
    let (tx, mut events) = tokio::sync::mpsc::channel(16);
    let router = aisix_proxy::build_router(state.with_usage_sink(aisix_obs::UsageSink::new(tx)));
    create_managed(router.clone(), &upstream, &registry).await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/msgbatch_new"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(batch_metadata("msgbatch_new", "in_progress")),
        )
        .mount(&upstream)
        .await;
    let response = call_batch(router, "GET", "/v1/messages/batches/msgbatch_new", None).await;
    assert_eq!(response.status(), 200);
    response_json(response).await;
    let accepted = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    let metadata = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(accepted.inference_stage, Some(InferenceStage::Accepted));
    assert!(accepted.native_operation_id.is_some());
    assert_eq!(metadata.inference_stage, Some(InferenceStage::Management));
    assert_eq!(accepted.operation, "batches");
    assert_eq!(metadata.operation, "batches");
    assert!(!accepted.is_final_attempt);
    assert!(metadata.is_final_attempt);
    for event in [accepted, metadata] {
        assert_eq!((event.prompt_tokens, event.completion_tokens), (0, 0));
        assert_eq!(event.upstream_dispatched, Some(true));
    }
    assert!(events.try_recv().is_err());
    assert!(registry.completions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_batch_results_enforce_each_models_output_guardrail_even_for_empty_content() {
    for second_content in [json!([]), json!([{"type":"text","text":"second answer"}])] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(ManagedRegistry::default());
        let (state, snapshot) = managed_state(&upstream.uri(), registry.clone(), false);
        let guardrail:aisix_core::Guardrail=serde_json::from_value(json!({
            "name":"block-second-model-output","enabled":true,"kind":"custom","hook_point":"output",
            "fail_open":false,"timeout_ms":5000,
            "script":"export function checkOutput() { return { action: 'block', reason_code: 'blocked' }; }"
        })).unwrap();
        snapshot
            .load()
            .guardrails
            .insert(ResourceEntry::new("batch-output-guard", guardrail, 1));
        let attachment:aisix_core::models::GuardrailAttachment=serde_json::from_value(json!({
            "guardrail_id":"batch-output-guard","scope_type":"model","scope_id":MODEL_TWO,"priority":100
        })).unwrap();
        snapshot
            .load()
            .guardrail_attachments
            .insert(ResourceEntry::new("batch-output-attachment", attachment, 1));
        let router = aisix_proxy::build_router(state);
        create_managed(router.clone(), &upstream, &registry).await;
        let first = json!({"custom_id":"first","result":{"type":"succeeded","message":{
            "id":"msg_do_not_release_first","type":"message","model":"upstream-model",
            "content":[{"type":"text","text":"first answer"}],"usage":{"input_tokens":1,"output_tokens":1}}}});
        let second = json!({"custom_id":"second","result":{"type":"succeeded","message":{
            "id":"msg_do_not_release_second","type":"message","model":"second-upstream",
            "content":second_content,"usage":{"input_tokens":1,"output_tokens":0}}}});
        Mock::given(method("GET"))
            .and(path("/v1/messages/batches/msgbatch_new/results"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/x-ndjson")
                    .set_body_string(format!("{first}\n{second}\n")),
            )
            .mount(&upstream)
            .await;
        let response = call_batch(
            router,
            "GET",
            "/v1/messages/batches/msgbatch_new/results",
            None,
        )
        .await;
        assert_eq!(response.status(), 422);
        let value = response_json(response).await;
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["code"], "content_filter");
        assert!(!value.to_string().contains("msg_do_not_release"));
        assert!(!value.to_string().contains("first answer"));
        assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn recovered_batch_settlement_survives_deleted_or_repointed_request_models() {
    for deleted in [true, false] {
        let upstream = MockServer::builder().start().await;
        metadata(&upstream, "ended", Some("/results")).await;
        let line = json!({"custom_id":"a","result":{"type":"succeeded","message":{
            "model":"upstream-model","usage":{"input_tokens":100,"output_tokens":5}}}});
        Mock::given(method("GET"))
            .and(path("/v1/messages/batches/msgbatch_owned/results"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{line}\n")))
            .mount(&upstream)
            .await;
        let registry = Arc::new(Registry::default());
        let state = state(
            &upstream.uri(),
            registry.clone(),
            json!([entry("a", "standard")]),
        );
        if deleted {
            state.snapshot.load().models.remove(MODEL);
        } else {
            let mut model = state
                .snapshot
                .load()
                .models
                .get_by_id(MODEL)
                .unwrap()
                .value
                .as_ref()
                .clone();
            model.provider_key_id = Some("99999999-9999-4999-8999-999999999999".into());
            state
                .snapshot
                .load()
                .models
                .insert(ResourceEntry::new(MODEL, model, 2));
        }
        assert_eq!(
            aisix_proxy::native_api::anthropic_batch_worker::run_once(&state)
                .await
                .unwrap(),
            1
        );
        let completed = registry.complete.lock().unwrap().clone();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].usage[0].prompt_tokens, 100);
        assert_eq!(completed[0].usage[0].completion_tokens, 5);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn rejected_native_batch_submissions_are_final_management_calls() {
    for rejected_upstream in [false, true] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(ManagedRegistry::default());
        let (state, _) = managed_state(&upstream.uri(), registry.clone(), false);
        let (tx, mut events) = tokio::sync::mpsc::channel(8);
        let router =
            aisix_proxy::build_router(state.with_usage_sink(aisix_obs::UsageSink::new(tx)));
        Mock::given(method("POST"))
            .and(path("/v1/messages/batches"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "type":"error","error":{"type":"invalid_request_error","message":"requests cannot be used for this batch"}
            })))
            .expect(u64::from(rejected_upstream))
            .mount(&upstream).await;
        let body = if rejected_upstream {
            batch_create_body()
        } else {
            json!({"requests":[]})
        };
        let response = call_batch(router, "POST", "/v1/messages/batches", Some(body)).await;
        assert_eq!(response.status(), 400);
        response_json(response).await;
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.inference_stage, Some(InferenceStage::Management));
        assert!(event.is_final_attempt);
        assert_eq!(event.upstream_dispatched, Some(rejected_upstream));
        assert_eq!(event.status_code, 400);
        assert!(!event.error_class.is_empty());
        assert_eq!((event.prompt_tokens, event.completion_tokens), (0, 0));
        assert!(registry.records.lock().unwrap().is_empty());
        assert!(events.try_recv().is_err());
    }
}

async fn quota_usage(
    events: &mut tokio::sync::mpsc::Receiver<aisix_obs::UsageEvent>,
) -> aisix_obs::UsageEvent {
    tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
}

fn assert_quota_attribution(event: &aisix_obs::UsageEvent, model_id: &str, requested_model: &str) {
    assert_eq!(event.status_code, 429);
    assert_eq!(event.model_id, model_id);
    assert_eq!(event.requested_model, requested_model);
    assert_eq!(event.requested_model_id, model_id);
    assert_eq!(event.provider_key_id.as_deref(), Some(PROVIDER));
    assert_eq!(event.upstream_dispatched, Some(false));
    assert_eq!(
        (
            event.prompt_tokens,
            event.completion_tokens,
            event.total_tokens
        ),
        (0, 0, 0)
    );
    assert_eq!(event.cost_usd, 0.0);
    assert_eq!(event.inference_stage, Some(InferenceStage::Management));
    assert!(event.is_final_attempt);
    assert!(!event.error_class.is_empty());
}

#[tokio::test]
async fn native_batch_model_quota_rejections_keep_resolved_attribution_on_every_route() {
    for (verb, route) in [
        ("POST", "/v1/messages/batches"),
        ("GET", "/v1/messages/batches?model=native-model"),
        ("GET", "/v1/messages/batches/msgbatch_new"),
    ] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(ManagedRegistry::default());
        let (state, snapshot) = managed_state(&upstream.uri(), registry.clone(), false);
        // 无限额时先建立真实路由登记的批次，供列表和按 ID 读取使用。
        create_managed(
            aisix_proxy::build_router(state.clone()),
            &upstream,
            &registry,
        )
        .await;
        if verb == "GET" {
            let answer = if route.contains('?') {
                json!({"data":[batch_metadata("msgbatch_new", "in_progress")],"has_more":false,"first_id":"msgbatch_new","last_id":"msgbatch_new"})
            } else {
                batch_metadata("msgbatch_new", "in_progress")
            };
            Mock::given(method("GET"))
                .and(path(route.split('?').next().unwrap()))
                .respond_with(ResponseTemplate::new(200).set_body_json(answer))
                .expect(1)
                .mount(&upstream)
                .await;
        }
        let entry = snapshot.load().models.get_by_id(MODEL).unwrap();
        let mut limited = (*entry.value).clone();
        limited.rate_limit = Some(serde_json::from_value(json!({"rpd":1})).unwrap());
        snapshot
            .load()
            .models
            .insert(ResourceEntry::new(MODEL, limited, 2));
        let (tx, mut events) = tokio::sync::mpsc::channel(8);
        let router =
            aisix_proxy::build_router(state.with_usage_sink(aisix_obs::UsageSink::new(tx)));
        let value = (verb == "POST").then(batch_create_body);
        let allowed = call_batch(router.clone(), verb, route, value.clone()).await;
        assert_eq!(allowed.status(), 200, "{verb} {route}");
        response_json(allowed).await;
        let allowed_event = quota_usage(&mut events).await;
        assert_eq!(allowed_event.model_id, MODEL);
        assert_eq!(allowed_event.upstream_dispatched, Some(true));
        assert_eq!(upstream.received_requests().await.unwrap().len(), 2);

        let denied = call_batch(router, verb, route, value).await;
        assert_eq!(denied.status(), 429, "{verb} {route}");
        assert_eq!(response_json(denied).await["type"], "error");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            2,
            "quota rejection must not dispatch: {verb} {route}"
        );
        assert_quota_attribution(&quota_usage(&mut events).await, MODEL, "native-model");
        assert!(events.try_recv().is_err());
    }
}

#[tokio::test]
async fn native_batch_later_entry_quota_rejection_names_the_model_that_refused_it() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(ManagedRegistry::default());
    let (state, snapshot) = managed_state(&upstream.uri(), registry.clone(), false);
    let entry = snapshot.load().models.get_by_id(MODEL_TWO).unwrap();
    let mut limited = (*entry.value).clone();
    limited.rate_limit = Some(serde_json::from_value(json!({"rpd":1})).unwrap());
    snapshot
        .load()
        .models
        .insert(ResourceEntry::new(MODEL_TWO, limited, 2));
    let (tx, mut events) = tokio::sync::mpsc::channel(8);
    let router = aisix_proxy::build_router(state.with_usage_sink(aisix_obs::UsageSink::new(tx)));
    create_managed(router.clone(), &upstream, &registry).await;
    assert_eq!(
        quota_usage(&mut events).await.upstream_dispatched,
        Some(true)
    );

    let denied = call_batch(
        router,
        "POST",
        "/v1/messages/batches",
        Some(batch_create_body()),
    )
    .await;
    assert_eq!(denied.status(), 429);
    assert_eq!(response_json(denied).await["type"], "error");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert_eq!(registry.operations.lock().unwrap().len(), 1);
    assert_quota_attribution(&quota_usage(&mut events).await, MODEL_TWO, "second-model");
    assert!(events.try_recv().is_err());
}
