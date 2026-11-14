//! 原生 Files/Batches 的路由与恢复组件回归；持久归属替身不代替真实控制面验收。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";
const KEY: &str = "33333333-3333-4333-8333-333333333333";
const OTHER_KEY: &str = "44444444-4444-4444-8444-444444444444";

#[derive(Default)]
struct Registry {
    operations: Mutex<HashMap<Uuid, BeginRequest>>,
    records: Mutex<HashMap<String, (Uuid, NativeRecord)>>,
    fail_bind_for: Mutex<HashSet<String>>,
    lookup_error: Mutex<Option<NativeRegistryError>>,
    completed: Mutex<Vec<CompleteRequest>>,
    postponed: Mutex<Vec<LeaseRequest>>,
}

impl NativeObjectRegistry for Registry {
    fn claim(
        &self,
        request: ClaimRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        assert_eq!(request.protocol, Some(NativeProtocol::Openai));
        assert_eq!(request.kind, Some(NativeObjectKind::MessageBatch));
        let operations = self.operations.lock().unwrap();
        let mut records = self.records.lock().unwrap();
        let mut reply = NativeReply::new("leased");
        for (_, record) in records.values_mut() {
            if record.kind != request.kind
                || record.completed
                || request.record_id.is_some_and(|id| id != record.record_id)
            {
                continue;
            }
            if !operations
                .get(&record.operation_id.unwrap())
                .unwrap()
                .asynchronous
            {
                continue;
            }
            record.lease_generation = Some(record.lease_generation.unwrap_or(0) + 1);
            record.lease_expires_at = Some(chrono::Utc::now() + chrono::Duration::seconds(30));
            reply.records.push(record.clone());
        }
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
        let mut records = self.records.lock().unwrap();
        let (_, record) = records
            .values_mut()
            .find(|(_, record)| record.record_id == request.lease.record_id)
            .unwrap();
        if !record.completed {
            record.completed = true;
            self.completed.lock().unwrap().push(request);
        }
        Box::pin(async { Ok(NativeReply::new("completed")) })
    }
    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut records = self.records.lock().unwrap();
        for (key, record) in records.values_mut() {
            if *key == request.api_key_id && record.record_id == request.record_id {
                record.deleted = true;
            }
        }
        Box::pin(async { Ok(NativeReply::new("deleted")) })
    }
    fn owned(
        &self,
        request: OwnedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        if request.raw_ids.len() > 100 {
            return Box::pin(async { Err(NativeRegistryError::InvalidRequest) });
        }
        let mut reply = NativeReply::new("owned");
        for id in request.raw_ids {
            if let Some((key, record)) = self.records.lock().unwrap().get(&id) {
                if *key == request.api_key_id
                    && !record.deleted
                    && record.protocol == Some(request.protocol)
                    && record.kind == Some(request.kind)
                    && record.origin.as_ref().is_some_and(|origin| {
                        origin.provider_key_id == request.origin.provider_key_id
                            && origin.boundary == request.origin.boundary
                    })
                {
                    reply.records.push(record.clone());
                }
            }
        }
        Box::pin(async move { Ok(reply) })
    }
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
            if self.fail_bind_for.lock().unwrap().contains(&request.raw_id) {
                return Err(NativeRegistryError::Unavailable);
            }
            let operations = self.operations.lock().unwrap();
            let operation = operations
                .get(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?;
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id": Uuid::new_v4(), "operation_id": request.operation_id,
                "requested_model_id": operation.requested_model_id,
                "requested_model": operation.requested_model, "upstream_model": operation.upstream_model,
                "origin": operation.origin, "protocol": operation.protocol,
                "kind": operation.kind, "parent_id": operation.parent_id,
                "raw_id": request.raw_id, "version": 1, "entries":operation.entries, "input_file_id":operation.input_file_id,"operation":operation.operation
            })).unwrap();
            self.records.lock().unwrap().insert(
                record.raw_id.clone(),
                (operation.api_key_id, record.clone()),
            );
            let mut reply = NativeReply::new("bound");
            reply.record = Some(record);
            Ok(reply)
        })
    }
    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        if let Some(error) = *self.lookup_error.lock().unwrap() {
            return Box::pin(async move { Err(error) });
        }
        let stored = self
            .records
            .lock()
            .unwrap()
            .get(&request.locator.raw_id)
            .cloned();
        Box::pin(async move {
            let (owner, record) = stored.ok_or(NativeRegistryError::NotFound)?;
            if owner != request.api_key_id
                || record.deleted
                || record.protocol != Some(request.locator.protocol)
                || record.kind != Some(request.locator.kind)
                || request
                    .locator
                    .parent_id
                    .is_some_and(|parent| record.parent_id != Some(parent))
                || request
                    .locator
                    .origin
                    .as_ref()
                    .is_some_and(|origin| record.origin.as_ref() != Some(origin))
            {
                return Err(NativeRegistryError::NotFound);
            }
            let mut reply = NativeReply::new("found");
            reply.record = Some(record);
            Ok(reply)
        })
    }
}

fn test_state(base: &str, registry: Arc<Registry>) -> ProxyState {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"uploads-account", "secret":"upstream-key", "provider":"openai",
        "api_base":base, "adapter":"openai"
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"upload-model", "provider":"openai", "model_name":"provider-model",
        "provider_key_id":PROVIDER
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    for (id, caller) in [(KEY, "caller"), (OTHER_KEY, "other-caller")] {
        let key: ApiKey = serde_json::from_value(json!({
            "key_hash":ApiKey::hash_bearer(caller), "allowed_models":["upload-model"]
        }))
        .unwrap();
        snapshot.apikeys.insert(ResourceEntry::new(id, key, 1));
    }
    let config: ProxyConfig = serde_json::from_value(json!({
        "addr":"127.0.0.1:0", "request_body_limit_bytes":1024*1024
    }))
    .unwrap();
    let snapshot = SnapshotHandle::new(snapshot);
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    ProxyState::new(snapshot.clone(), hub, &config)
        .without_cache()
        .with_guardrail_index(guardrails)
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "native-jobs-test",
        ))
        .with_native_objects(registry)
}

async fn call(
    state: &ProxyState,
    method: &str,
    uri: &str,
    caller: &str,
    body: Option<Value>,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {caller}"));
    let body = if let Some(body) = body {
        request = request.header("content-type", "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    build_router(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}
async fn upload_file(state: &ProxyState, content: &str) -> Value {
    let boundary = "native-batch-input-v7";
    let body=format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nupload-model\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"requests.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{content}\r\n--{boundary}--\r\n");
    let request = Request::builder()
        .method("POST")
        .uri("/v1/files")
        .header("authorization", "Bearer caller")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}
async fn input_upstream(upstream: &MockServer) {
    let files = Arc::new(Mutex::new(HashMap::<String, (String, Vec<u8>)>::new()));
    let saved = files.clone();
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(move |request: &wiremock::Request| {
            let body = std::str::from_utf8(&request.body).unwrap();
            let file = body
                .split_once("filename=")
                .unwrap()
                .1
                .split_once("\r\n\r\n")
                .unwrap()
                .1
                .split_once("\r\n--")
                .unwrap()
                .0
                .as_bytes()
                .to_vec();
            let purpose = body
                .split_once("name=\"purpose\"")
                .unwrap()
                .1
                .split_once("\r\n\r\n")
                .unwrap()
                .1
                .split_once("\r\n--")
                .unwrap()
                .0
                .to_owned();
            let native = std::str::from_utf8(&file)
                .unwrap()
                .contains("\"model\":\"provider-model\"");
            let id = if native { "file_derived" } else { "file_input" };
            let bytes = file.len();
            saved
                .lock()
                .unwrap()
                .insert(id.into(), (purpose.clone(), file));
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":id,"object":"file","purpose":purpose,
            "filename":"requests.jsonl","created_at":1793843000,"bytes":bytes}))
        })
        .mount(upstream)
        .await;
    let saved = files.clone();
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(
            "^/v1/files/file_(input|derived)$",
        ))
        .respond_with(move |request: &wiremock::Request| {
            let id = request.url.path().rsplit('/').next().unwrap();
            let saved = saved.lock().unwrap();
            let Some((purpose, bytes)) = saved.get(id) else {
                return ResponseTemplate::new(404);
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":id,"object":"file","purpose":purpose,
                "filename":"requests.jsonl","created_at":1793843000,"bytes":bytes.len()}))
        })
        .mount(upstream)
        .await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(
            "^/v1/files/file_(input|derived)/content$",
        ))
        .respond_with(move |request: &wiremock::Request| {
            let id = request.url.path().split('/').rev().nth(1).unwrap();
            let saved = files.lock().unwrap();
            let Some((_, bytes)) = saved.get(id) else {
                return ResponseTemplate::new(404);
            };
            ResponseTemplate::new(200).set_body_bytes(bytes.clone())
        })
        .mount(upstream)
        .await;
}

fn input_line(id: &str, tier: &str) -> Value {
    json!({"custom_id":id,"method":"POST","url":"/v1/chat/completions",
    "body":{"model":"upload-model","service_tier":tier,"messages":[{"role":"user","content":"Classify this text"}]}})
}

#[tokio::test]
async fn files_batches_owned_routes_and_worker_recover_without_caller_or_model() {
    let upstream = MockServer::builder().start().await;
    input_upstream(&upstream).await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    let input = format!(
        "{}\n{}\n",
        input_line("slow", "flex"),
        input_line("fast", "priority")
    );
    let file = upload_file(&state, &input).await;
    assert_eq!(file["id"], "file_input");
    let submitted = registry.records.lock().unwrap()["file_input"]
        .1
        .entries
        .clone();
    assert_eq!(submitted.len(), 2);
    assert_eq!(submitted[1].entry_id, "fast");
    assert_eq!(
        submitted[1].requested_service_tier.as_deref(),
        Some("priority")
    );
    let request = upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|request| request.method == "POST")
        .unwrap();
    let body = String::from_utf8(request.body).unwrap();
    assert!(body.contains("\"model\":\"upload-model\""));
    assert!(!body.contains("\"model\":\"provider-model\""));
    let original = call(
        &state,
        "GET",
        "/v1/files/file_input/content",
        "caller",
        None,
    )
    .await;
    assert_eq!(original.status(), StatusCode::OK);
    assert_eq!(
        &to_bytes(original.into_body(), 1024 * 1024).await.unwrap()[..],
        input.as_bytes(),
        "the uploaded File remains byte exact"
    );
    Mock::given(method("POST")).and(path("/v1/batches")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"batch_owned","object":"batch","status":"in_progress","input_file_id":"file_derived","created_at":1793843000}))).mount(&upstream).await;
    let response=call(&state,"POST","/v1/batches","caller",Some(json!({"input_file_id":"file_input","endpoint":"/v1/chat/completions","completion_window":"24h"}))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(created["input_file_id"], "file_input");
    let requests = upstream.received_requests().await.unwrap();
    let wire_request = requests
        .iter()
        .find(|request| request.method == "POST" && request.url.path() == "/v1/batches")
        .unwrap();
    let wire_submission: Value = serde_json::from_slice(&wire_request.body).unwrap();
    assert_eq!(wire_submission["input_file_id"], "file_derived");
    let derived = requests
        .iter()
        .filter(|request| request.method == "POST" && request.url.path() == "/v1/files")
        .map(|request| String::from_utf8(request.body.clone()).unwrap())
        .find(|body| body.contains("\"model\":\"provider-model\""))
        .unwrap();
    assert!(derived.contains("\"model\":\"provider-model\""));
    assert!(!derived.contains("\"model\":\"upload-model\""));
    Mock::given(method("GET")).and(path("/v1/files")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list",
        "data":[{"id":"file_derived","filename":"PRIVATE_STAGE"},{"id":"file_input","filename":"requests.jsonl"}],"has_more":false}))).mount(&upstream).await;
    let listed = call(
        &state,
        "GET",
        "/v1/files?model=upload-model",
        "caller",
        None,
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed: Value =
        serde_json::from_slice(&to_bytes(listed.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["id"], "file_input");
    let before = upstream.received_requests().await.unwrap().len();
    for (method, path) in [
        ("GET", "/v1/files/file_derived"),
        ("GET", "/v1/files/file_derived/content"),
        ("DELETE", "/v1/files/file_derived"),
        ("GET", "/v1/files?model=upload-model&after=file_derived"),
    ] {
        assert_eq!(
            call(&state, method, path, "caller", None).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    for (path, body) in [
        (
            "/v1/batches",
            json!({"input_file_id":"file_derived","endpoint":"/v1/chat/completions","completion_window":"24h"}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"upload-model","messages":[{"role":"user","content":[{"type":"file","file":{"file_id":"file_derived"}}]}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"upload-model","input":[{"role":"user","content":[{"type":"input_file","file_id":"file_derived"}]}]}),
        ),
    ] {
        assert_eq!(
            call(&state, "POST", path, "caller", Some(body))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "internal stage IDs stop before every public provider path"
    );
    let batch = registry.records.lock().unwrap()["batch_owned"].1.clone();
    assert_eq!(batch.entries, submitted);
    assert_eq!(
        batch.parent_id, None,
        "deleting an input file cannot invalidate an accepted batch"
    );
    assert_eq!(batch.input_file_id.as_deref(), Some("file_input"));
    let before = upstream.received_requests().await.unwrap().len();
    for path in [
        "/v1/files/file_input/content",
        "/v1/batches/batch_owned",
        "/v1/files/file_unknown",
    ] {
        let response = call(&state, "GET", path, "other-caller", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "unknown and foreign IDs never probe the account"
    );
    Mock::given(method("GET")).and(path("/v1/batches/batch_owned")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"batch_owned","object":"batch","status":"completed","completed_at":1793843100,"output_file_id":"file_output","error_file_id":"file_error"}))).mount(&upstream).await;
    let fast = json!({"custom_id":"fast","response":{"status_code":200,"body":{"model":"provider-returned", "service_tier":"priority",
        "usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":0}}}}});
    Mock::given(method("GET"))
        .and(path("/v1/files/file_output/content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{fast}\n")))
        .mount(&upstream)
        .await;
    let slow = json!({"custom_id":"slow","response":{"status_code":400,"body":{"error":{"message":"invalid parameter"}}}});
    Mock::given(method("GET"))
        .and(path("/v1/files/file_error/content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{slow}\n")))
        .mount(&upstream)
        .await;
    let restarted = test_state(&upstream.uri(), registry.clone());
    let snapshot = restarted.snapshot.load();
    snapshot.models.remove(MODEL);
    snapshot.apikeys.remove(KEY);
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&restarted)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&restarted)
            .await
            .unwrap(),
        0
    );
    let complete = registry.completed.lock().unwrap();
    assert_eq!(complete.len(), 1);
    assert_eq!(
        complete[0].output_file_ids,
        vec!["file_error", "file_output"]
    );
    let fast = complete[0]
        .usage
        .iter()
        .find(|row| row.entry_id == "fast")
        .unwrap();
    assert_eq!(
        (
            fast.prompt_tokens,
            fast.completion_tokens,
            fast.total_tokens
        ),
        (100, 20, 120)
    );
    assert_eq!(fast.cache_usage_reported, Some(true));
    assert_eq!(fast.cached_prompt_tokens, 0);
    assert_eq!(fast.requested_service_tier.as_deref(), Some("priority"));
    assert_eq!(
        complete[0]
            .usage
            .iter()
            .find(|row| row.entry_id == "slow")
            .unwrap()
            .status_code,
        400
    );
}

#[tokio::test]
async fn owned_file_pages_do_not_expose_foreign_ids_or_accept_foreign_cursors() {
    let upstream = MockServer::builder().start().await;
    input_upstream(&upstream).await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    upload_file(&state, &input_line("a", "default").to_string()).await;
    Mock::given(method("GET")).and(path("/v1/files")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "object":"list","data":[{"id":"file_foreign","object":"file","filename":"SECRET_OTHER_CUSTOMER"},
        {"id":"file_input","object":"file","filename":"requests.jsonl"}],"first_id":"file_foreign","last_id":"file_input","has_more":false}))).mount(&upstream).await;
    let response = call(
        &state,
        "GET",
        "/v1/files?model=upload-model&limit=1",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["data"].as_array().unwrap().len(), 1);
    assert_eq!(value["first_id"], "file_input");
    assert_eq!(value["last_id"], "file_input");
    assert_eq!(value["has_more"], false);
    assert!(!String::from_utf8(body.to_vec())
        .unwrap()
        .contains("file_foreign"));
    let before = upstream.received_requests().await.unwrap().len();
    for caller in ["caller", "other-caller"] {
        let response = call(
            &state,
            "GET",
            "/v1/files?model=upload-model&after=file_foreign",
            caller,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn batch_input_validates_every_model_and_origin_rotation_never_uses_new_credentials() {
    let upstream = MockServer::builder().start().await;
    input_upstream(&upstream).await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    upload_file(&state, &input_line("a", "default").to_string()).await;
    Mock::given(method("POST"))
        .and(path("/v1/batches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"batch_owned","object":"batch","status":"in_progress"})))
        .mount(&upstream)
        .await;
    assert_eq!(call(&state,"POST","/v1/batches","caller",Some(json!({"input_file_id":"file_input","endpoint":"/v1/responses","completion_window":"24h"}))).await.status(),StatusCode::BAD_REQUEST);
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        1,
        "endpoint mismatch never submits"
    );
    assert_eq!(call(&state,"POST","/v1/batches","caller",Some(json!({"input_file_id":"file_input","endpoint":"/v1/chat/completions","completion_window":"24h"}))).await.status(),StatusCode::OK);
    let snapshot = state.snapshot.load();
    let old = snapshot.provider_keys.get_by_id(PROVIDER).unwrap();
    let mut rotated = (*old.value).clone();
    rotated.api_key = "rotated-upstream-credential".into();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, rotated, 2));
    let before = upstream.received_requests().await.unwrap().len();
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&state)
            .await
            .unwrap(),
        0
    );
    assert!(registry.completed.lock().unwrap().is_empty());
    assert_eq!(registry.postponed.lock().unwrap().len(), 1);
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "rotation closes the bound account before HTTP"
    );
}

#[tokio::test]
async fn derived_batch_result_files_hide_account_errors_and_models_while_input_files_stay_opaque() {
    let upstream = MockServer::builder().start().await;
    input_upstream(&upstream).await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    let input = format!(
        "{}\n{}\n{}\n",
        input_line("success", "default"),
        input_line("request", "flex"),
        input_line("account", "priority")
    );
    upload_file(&state, &input).await;
    let mut record = registry.records.lock().unwrap()["file_input"].1.clone();
    record.raw_id = "file_result".into();
    record.record_id = Uuid::new_v4();
    record.parent_id = Some(Uuid::new_v4());
    record.operation = "batch_result".into();
    registry.records.lock().unwrap().insert(
        record.raw_id.clone(),
        (Uuid::parse_str(KEY).unwrap(), record),
    );
    let success = json!({"id":"batch_req_success","custom_id":"success","response":{"status_code":200,"request_id":"req_success",
        "body":{"id":"chatcmpl_native","object":"chat.completion","model":"INTERNAL_PROVIDER_MODEL_SENTINEL",
        "choices":[{"index":0,"message":{"role":"assistant","content":"Completed classification"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}},"error":null});
    let request = json!({"id":"batch_req_request","custom_id":"request","response":{"status_code":400,"request_id":"req_request",
        "body":{"error":{"message":"CLIENT_PROMPT_SENTINEL prompt is too long upstream-key","type":"invalid_request_error",
        "code":"context_length_exceeded","param":"messages"}}},"error":null});
    let account = json!({"id":"batch_req_account","custom_id":"account","response":{"status_code":400,"request_id":"req_account",
        "body":{"error":{"message":"You exceeded your current quota ACCOUNT_CHANNEL_SENTINEL upstream-key",
        "type":"invalid_request_error","code":"billing_hard_limit_reached","param":null}}},"error":null});
    Mock::given(method("GET"))
        .and(path("/v1/files/file_result/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!("{account}\n{success}\n{request}\n")),
        )
        .mount(&upstream)
        .await;
    let response = call(
        &state,
        "GET",
        "/v1/files/file_result/content",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!body.contains("INTERNAL_PROVIDER_MODEL_SENTINEL"));
    assert!(!body.contains("ACCOUNT_CHANNEL_SENTINEL"));
    assert!(!body.contains("upstream-key"));
    let lines: Vec<Value> = body
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    let find = |id: &str| lines.iter().find(|line| line["custom_id"] == id).unwrap();
    assert_eq!(find("success")["response"]["body"]["model"], "upload-model");
    assert_eq!(find("success")["response"]["body"]["id"], "chatcmpl_native");
    assert_eq!(
        find("success")["response"]["body"]["choices"][0]["message"]["content"],
        "Completed classification"
    );
    assert_eq!(find("request")["response"]["status_code"], 400);
    assert_eq!(
        find("request")["response"]["body"]["error"]["code"],
        "context_length_exceeded"
    );
    assert_eq!(
        find("request")["response"]["body"]["error"]["param"],
        "messages"
    );
    assert!(find("request")["response"]["body"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("CLIENT_PROMPT_SENTINEL"));
    assert_eq!(find("account")["response"]["status_code"], 503);
    assert_eq!(
        find("account")["response"]["body"]["error"]["code"],
        "upstream_unavailable"
    );
    let original = format!("{account}\n{success}\n{request}\n");
    Mock::given(method("POST")).and(path("/v1/files")).and(wiremock::matchers::body_string_contains("user_data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"file_opaque","object":"file","purpose":"user_data","filename":"opaque.jsonl","created_at":1793843000,"bytes":original.len()})))
        .with_priority(1).mount(&upstream).await;
    let boundary = "opaque-file-boundary";
    let multipart=format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nupload-model\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nuser_data\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"opaque.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{original}\r\n--{boundary}--\r\n");
    let request = Request::builder()
        .method("POST")
        .uri("/v1/files")
        .header("authorization", "Bearer caller")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(multipart))
        .unwrap();
    let uploaded = build_router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(uploaded.status(), StatusCode::OK);
    Mock::given(method("GET"))
        .and(path("/v1/files/file_opaque/content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(original.clone()))
        .mount(&upstream)
        .await;
    let response = call(
        &state,
        "GET",
        "/v1/files/file_opaque/content",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        &bytes[..],
        original.as_bytes(),
        "only the immutable result marker admits rewriting opaque file content"
    );
}

#[tokio::test]
async fn owned_pages_cross_foreign_only_pages_and_return_only_owned_continuations() {
    let upstream = MockServer::builder().start().await;
    input_upstream(&upstream).await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    upload_file(&state, &input_line("a", "default").to_string()).await;
    let mut next = registry.records.lock().unwrap()["file_input"].1.clone();
    next.raw_id = "file_next".into();
    next.record_id = Uuid::new_v4();
    registry
        .records
        .lock()
        .unwrap()
        .insert(next.raw_id.clone(), (Uuid::parse_str(KEY).unwrap(), next));
    Mock::given(method("GET")).and(path("/v1/files")).respond_with(|request:&wiremock::Request|{
        let after=request.url.query_pairs().find(|(name,_)|name=="after").map(|(_,value)|value.into_owned());
        let (ids,more)=match after.as_deref(){
            None=>(vec!["file_foreign_one"],true),
            Some("file_foreign_one")=>(vec!["file_input","file_foreign_two"],true),
            Some("file_input")=>(vec!["file_foreign_two"],true),
            Some("file_foreign_two")=>(vec!["file_next"],false),
            other=>panic!("unexpected pagination cursor {other:?}"),
        };
        ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":ids.iter().map(|id|json!({"id":id,"object":"file"})).collect::<Vec<_>>(),
            "first_id":ids.first(),"last_id":ids.last(),"has_more":more}))
    }).mount(&upstream).await;
    let response = call(
        &state,
        "GET",
        "/v1/files?model=upload-model&limit=1&x-extra=a%2Fb&x-extra=two+words&empty=",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let page: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["first_id"], "file_input");
    assert_eq!(page["last_id"], "file_input");
    assert_eq!(page["has_more"], true);
    assert!(!String::from_utf8(bytes.to_vec())
        .unwrap()
        .contains("foreign"));
    let requests = upstream.received_requests().await.unwrap();
    let listed: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "GET")
        .collect();
    assert_eq!(
        listed.len(),
        3,
        "lookahead reaches the next owned row through private pages"
    );
    for request in listed {
        assert!(request
            .url
            .query()
            .unwrap()
            .contains("x-extra=a%2Fb&x-extra=two+words&empty="));
    }
    let response = call(
        &state,
        "GET",
        "/v1/files?after=file_input&limit=1",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let page: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(page["data"][0]["id"], "file_next");
    assert_eq!(page["last_id"], "file_next");
    assert_eq!(page["has_more"], false);
}

#[tokio::test]
async fn batches_only_key_uses_owned_input_without_acquiring_file_management() {
    let upstream = MockServer::builder().start().await;
    input_upstream(&upstream).await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    upload_file(&state, &input_line("a", "default").to_string()).await;
    let snapshot = state.snapshot.load();
    let entry = snapshot.apikeys.get_by_id(KEY).unwrap();
    let mut key = (*entry.value).clone();
    key.allowed_endpoints = Some(vec![aisix_core::models::EndpointFamily::Batches]);
    snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 2));
    Mock::given(method("POST"))
        .and(path("/v1/batches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"batch_narrow","object":"batch","input_file_id":"file_input","status":"in_progress"})))
        .mount(&upstream)
        .await;
    let response=call(&state,"POST","/v1/batches","caller",Some(json!({"input_file_id":"file_input","endpoint":"/v1/chat/completions","completion_window":"24h"}))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let before = upstream.received_requests().await.unwrap().len();
    assert_eq!(
        call(&state, "GET", "/v1/files/file_input", "caller", None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(call(&state,"POST","/v1/batches","other-caller",Some(json!({"input_file_id":"file_input","endpoint":"/v1/chat/completions","completion_window":"24h"}))).await.status(),StatusCode::NOT_FOUND);
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "consumer permission neither grants File management nor another key's data"
    );
}

#[tokio::test]
async fn files_with_101_account_rows_stay_inside_the_real_authority_page_bound() {
    let upstream = MockServer::builder().start().await;
    input_upstream(&upstream).await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry);
    upload_file(&state, &input_line("a", "default").to_string()).await;
    Mock::given(method("GET")).and(path("/v1/files")).respond_with(|request:&wiremock::Request|{
        let rows:Vec<_>=(0..100).map(|index|format!("file_foreign_{index}")).chain(std::iter::once("file_input".to_owned())).collect();
        let after=request.url.query_pairs().find(|(key,_)|key=="after").map(|(_,value)|value.into_owned());
        let start=after.as_ref().and_then(|id|rows.iter().position(|row|row==id)).map_or(0,|index|index+1);
        let limit=request.url.query_pairs().find(|(key,_)|key=="limit").unwrap().1.parse::<usize>().unwrap();
        let page:Vec<_>=rows.iter().skip(start).take(limit).collect();
        ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":page.iter().map(|id|json!({"id":id,"object":"file"})).collect::<Vec<_>>(),
            "has_more":start+page.len()<rows.len(),"first_id":page.first(),"last_id":page.last()}))
    }).mount(&upstream).await;
    let response = call(
        &state,
        "GET",
        "/v1/files?model=upload-model",
        "caller",
        None,
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the registry rejects any owned page larger than100"
    );
    let page: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["id"], "file_input");
    assert_eq!(page["has_more"], false);
}

#[tokio::test]
async fn lists_recheck_saved_primary_and_secondary_model_ip_rules() {
    let upstream = MockServer::builder().start().await;
    input_upstream(&upstream).await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    upload_file(&state, &input_line("a", "default").to_string()).await;
    let private = Uuid::parse_str("77777777-7777-4777-8777-777777777777").unwrap();
    let snapshot = state.snapshot.load();
    let model:Model=serde_json::from_value(json!({"display_name":"private-model","provider":"openai","model_name":"private-upstream","provider_key_id":PROVIDER,"allowed_cidrs":["10.0.0.0/8"]})).unwrap();
    snapshot
        .models
        .insert(ResourceEntry::new(private.to_string(), model, 1));
    let key = snapshot.apikeys.get_by_id(KEY).unwrap();
    let mut key = (*key.value).clone();
    key.allowed_models = vec!["upload-model".into(), "private-model".into()];
    snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 2));
    let source = registry.records.lock().unwrap()["file_input"].1.clone();
    let mut primary = source.clone();
    primary.raw_id = "file_private_primary".into();
    primary.record_id = Uuid::new_v4();
    primary.origin.as_mut().unwrap().model_id = private;
    primary.requested_model_id = Some(private);
    primary.requested_model = "private-model".into();
    primary.entries.clear();
    let mut secondary = source.clone();
    secondary.raw_id = "file_private_secondary".into();
    secondary.record_id = Uuid::new_v4();
    let mut extra = secondary.entries[0].clone();
    extra.entry_id = "private-entry".into();
    extra.model_id = private;
    extra.requested_model_id = private;
    extra.requested_model = "private-model".into();
    extra.upstream_model = "private-upstream".into();
    secondary.entries.push(extra);
    let mut batch = secondary.clone();
    batch.kind = Some(NativeObjectKind::MessageBatch);
    batch.raw_id = "batch_private_secondary".into();
    batch.record_id = Uuid::new_v4();
    for record in [primary, secondary, batch] {
        registry.records.lock().unwrap().insert(
            record.raw_id.clone(),
            (Uuid::parse_str(KEY).unwrap(), record),
        );
    }
    Mock::given(method("GET")).and(path("/v1/files")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list",
        "data":[{"id":"file_input"},{"id":"file_private_primary","filename":"PRIMARY_SECRET"},{"id":"file_private_secondary","filename":"SECONDARY_SECRET"}],"has_more":false}))).mount(&upstream).await;
    Mock::given(method("GET")).and(path("/v1/batches")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list",
        "data":[{"id":"batch_private_secondary","metadata":{"private":"SECONDARY_SECRET"}}],"has_more":false}))).mount(&upstream).await;
    let response = call(
        &state,
        "GET",
        "/v1/files?model=upload-model",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let page: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["id"], "file_input");
    assert!(!String::from_utf8(bytes.to_vec())
        .unwrap()
        .contains("SECRET"));
    let response = call(
        &state,
        "GET",
        "/v1/batches?model=upload-model",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert!(page["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn rejected_batch_submissions_emit_final_management_rows() {
    for scenario in [
        "missing_file",
        "endpoint_mismatch",
        "model_forbidden",
        "native_400",
        "native_503",
    ] {
        let upstream = MockServer::builder().start().await;
        input_upstream(&upstream).await;
        let registry = Arc::new(Registry::default());
        let state = test_state(&upstream.uri(), registry.clone());
        upload_file(&state, &input_line("a", "default").to_string()).await;
        let expected = match scenario {
            "model_forbidden" => {
                let snapshot = state.snapshot.load();
                let entry = snapshot.apikeys.get_by_id(KEY).unwrap();
                let mut key = (*entry.value).clone();
                key.allowed_models = vec!["a-different-model".into()];
                snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 2));
                StatusCode::NOT_FOUND
            }
            "native_503" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_REQUEST,
        };
        let native_status = if scenario == "native_503" { 503 } else { 400 };
        Mock::given(method("POST"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(native_status).set_body_json(json!({
                "error":{"type":"invalid_request_error","code":"invalid_value", "param":"input_file_id", "message":"input_file_id cannot be used for this batch"}
            })))
            .mount(&upstream)
            .await;
        let (tx, mut events) = tokio::sync::mpsc::channel(8);
        let state = state.with_usage_sink(aisix_obs::UsageSink::new(tx));
        let body = match scenario {
            "missing_file" => json!({"endpoint":"/v1/chat/completions","completion_window":"24h"}),
            "endpoint_mismatch" => {
                json!({"input_file_id":"file_input","endpoint":"/v1/responses","completion_window":"24h"})
            }
            _ => {
                json!({"input_file_id":"file_input","endpoint":"/v1/chat/completions","completion_window":"24h"})
            }
        };
        let response = call(&state, "POST", "/v1/batches", "caller", Some(body)).await;
        assert_eq!(response.status(), expected, "{scenario}");
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            event.status_code,
            if scenario == "native_503" {
                StatusCode::BAD_GATEWAY.as_u16()
            } else {
                expected.as_u16()
            },
            "{scenario}: {}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(
            event.inference_stage,
            Some(InferenceStage::Management),
            "{scenario}"
        );
        assert!(
            event.is_final_attempt,
            "{scenario}: rejection has no future terminal job"
        );
        assert_eq!(event.operation, "batches");
        assert_eq!((event.prompt_tokens, event.completion_tokens), (0, 0));
        assert!(!event.error_class.is_empty(), "{scenario}");
        let submitted = scenario.starts_with("native_");
        assert_eq!(event.upstream_dispatched, Some(submitted), "{scenario}");
        assert_eq!(
            upstream
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.method == "POST" && request.url.path() == "/v1/batches")
                .count(),
            usize::from(submitted),
            "{scenario}"
        );
        assert!(registry
            .records
            .lock()
            .unwrap()
            .values()
            .all(|(_, record)| record.kind != Some(NativeObjectKind::MessageBatch)));
        assert!(events.try_recv().is_err());
    }
}

#[tokio::test]
async fn accepted_batch_submission_remains_pending_after_post_acceptance_failures() {
    for scenario in ["accepted", "bind_unavailable", "output_blocked"] {
        let upstream = MockServer::builder().start().await;
        input_upstream(&upstream).await;
        let registry = Arc::new(Registry::default());
        let state = test_state(&upstream.uri(), registry.clone());
        upload_file(&state, &input_line("a", "default").to_string()).await;
        if scenario == "bind_unavailable" {
            registry
                .fail_bind_for
                .lock()
                .unwrap()
                .insert("batch_accepted".into());
        }
        if scenario == "output_blocked" {
            let snapshot = state.snapshot.load();
            let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
                "name":"block-batch-acceptance-output", "enabled":true, "kind":"custom", "hook_point":"output",
                "fail_open":false, "timeout_ms":5000,
                "script":"export function checkOutput() { return { action: 'block', reason_code: 'blocked' }; }"
            })).unwrap();
            snapshot
                .guardrails
                .insert(ResourceEntry::new("batch-acceptance-guard", guardrail, 1));
            let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
                "guardrail_id":"batch-acceptance-guard","scope_type":"model","scope_id":MODEL,"priority":100
            })).unwrap();
            snapshot.guardrail_attachments.insert(ResourceEntry::new(
                "batch-acceptance-attachment",
                attachment,
                1,
            ));
        }
        Mock::given(method("POST"))
            .and(path("/v1/batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"batch_accepted","object":"batch","input_file_id":"file_derived","status":"in_progress"
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let (tx, mut events) = tokio::sync::mpsc::channel(8);
        let state = state.with_usage_sink(aisix_obs::UsageSink::new(tx));
        let response = call(&state, "POST", "/v1/batches", "caller", Some(json!({
            "input_file_id":"file_input","endpoint":"/v1/chat/completions","completion_window":"24h"
        }))).await;
        let expected = match scenario {
            "bind_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            "output_blocked" => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::OK,
        };
        assert_eq!(response.status(), expected, "{scenario}");
        let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.status_code, expected.as_u16(), "{scenario}");
        assert_eq!(
            event.inference_stage,
            Some(InferenceStage::Accepted),
            "{scenario}"
        );
        assert!(
            !event.is_final_attempt,
            "{scenario}: accepted inference settles separately"
        );
        assert_eq!(event.guardrail_blocked, scenario == "output_blocked");
        assert_eq!(event.upstream_dispatched, Some(true));
        assert_eq!((event.prompt_tokens, event.completion_tokens), (0, 0));
        assert_eq!(
            registry
                .records
                .lock()
                .unwrap()
                .contains_key("batch_accepted"),
            scenario != "bind_unavailable"
        );
        assert!(registry.completed.lock().unwrap().is_empty());
        assert!(events.try_recv().is_err());
    }
}

#[tokio::test]
async fn management_quota_rejections_keep_the_resolved_model_without_upstream_dispatch() {
    for (uri, raw, kind) in [
        ("/v1/files/file_input", "file_input", NativeObjectKind::File),
        (
            "/v1/batches/batch_quota",
            "batch_quota",
            NativeObjectKind::MessageBatch,
        ),
    ] {
        let upstream = MockServer::builder().start().await;
        input_upstream(&upstream).await;
        let registry = Arc::new(Registry::default());
        let state = test_state(&upstream.uri(), registry.clone());
        upload_file(&state, &input_line("a", "default").to_string()).await;
        if kind == NativeObjectKind::MessageBatch {
            let mut record = registry.records.lock().unwrap()["file_input"].1.clone();
            record.record_id = Uuid::new_v4();
            record.raw_id = raw.into();
            record.kind = Some(kind);
            registry
                .records
                .lock()
                .unwrap()
                .insert(raw.into(), (Uuid::parse_str(KEY).unwrap(), record));
            Mock::given(method("GET"))
                .and(path("/v1/batches/batch_quota"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id":"batch_quota","object":"batch","status":"in_progress"
                })))
                .expect(1)
                .mount(&upstream)
                .await;
        }
        let snapshot = state.snapshot.load();
        let current = snapshot.models.get_by_id(MODEL).unwrap();
        let mut model = serde_json::to_value(&*current.value).unwrap();
        model["rate_limit"] = json!({"rpd":1});
        snapshot.models.insert(ResourceEntry::new(
            MODEL,
            serde_json::from_value::<Model>(model).unwrap(),
            2,
        ));
        let (tx, mut events) = tokio::sync::mpsc::channel(8);
        let state = state.with_usage_sink(aisix_obs::UsageSink::new(tx));
        let first = call(&state, "GET", uri, "caller", None).await;
        assert_eq!(first.status(), StatusCode::OK, "{uri}");
        let _ = to_bytes(first.into_body(), 1024 * 1024).await.unwrap();
        let first_event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_event.model_id, MODEL);
        assert_eq!(first_event.upstream_dispatched, Some(true));
        let before = upstream.received_requests().await.unwrap().len();
        let limited = call(&state, "GET", uri, "caller", None).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS, "{uri}");
        let _ = to_bytes(limited.into_body(), 1024 * 1024).await.unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.status_code, 429);
        assert_eq!(event.model_id, MODEL, "{uri}");
        assert_eq!(event.requested_model, "upload-model");
        assert_eq!(event.requested_model_id, MODEL);
        assert_eq!(event.provider_key_id.as_deref(), Some(PROVIDER));
        assert_eq!(event.upstream_dispatched, Some(false));
        assert_eq!(event.inference_stage, Some(InferenceStage::Management));
        assert!(event.is_final_attempt);
        assert_eq!((event.prompt_tokens, event.completion_tokens), (0, 0));
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            before,
            "{uri}"
        );
        assert!(events.try_recv().is_err());
    }
}
