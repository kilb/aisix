//! Evals 的真实路由组件合同；账号服务与持久注册表替身不构成真实提供商 E2E。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use aisix_core::native_metering::*;
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
    deny_lookup_after_bind: Mutex<HashSet<String>>,
    completed: Mutex<Vec<CompleteRequest>>,
    postponed: Mutex<Vec<LeaseRequest>>,
    typed_completed: Mutex<Vec<TypedCompleteRequest>>,
    reconciliations: Mutex<Vec<ReconciliationRequest>>,
}

impl NativeObjectRegistry for Registry {
    fn complete_typed(
        &self,
        request: TypedCompleteRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let MeteringTarget::Object { lease } = &request.target else {
                return Err(NativeRegistryError::InvalidRequest);
            };
            let mut records = self.records.lock().unwrap();
            let (_, record) = records
                .values_mut()
                .find(|(_, record)| record.record_id == lease.record_id)
                .ok_or(NativeRegistryError::NotFound)?;
            if record.lease_generation != Some(lease.generation) {
                return Err(NativeRegistryError::Conflict);
            }
            if !record.completed {
                record.completed = true;
                self.typed_completed.lock().unwrap().push(request);
            }
            Ok(NativeReply::new("completed"))
        })
    }
    fn mark_reconciliation(
        &self,
        request: ReconciliationRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            self.reconciliations.lock().unwrap().push(request);
            Ok(NativeReply::new("reconciliation"))
        })
    }

    fn begin_related(
        &self,
        request: BeginRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            {
                let records = self.records.lock().unwrap();
                for reference in &request.references {
                    if !records.values().any(|(owner, record)| {
                        *owner == request.submission.api_key_id
                            && record.record_id == reference.record_id
                            && !record.deleted
                    }) {
                        return Err(NativeRegistryError::NotFound);
                    }
                }
                if let Some(parent) = request.owner_parent_record_id {
                    if !records.values().any(|(owner, record)| {
                        *owner == request.submission.api_key_id
                            && record.record_id == parent
                            && !record.deleted
                    }) {
                        return Err(NativeRegistryError::NotFound);
                    }
                }
            }
            self.begin(request.submission).await
        })
    }
    fn bind_related(
        &self,
        request: BindRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.bind(request.binding)
    }
    fn reserve_delete(
        &self,
        request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let records = self.records.lock().unwrap();
            let (_, record) = records
                .values()
                .find(|(owner, record)| {
                    *owner == request.api_key_id
                        && record.record_id == request.record_id
                        && !record.deleted
                })
                .ok_or(NativeRegistryError::NotFound)?;
            if (record.kind == Some(NativeObjectKind::EvalRun) && !record.completed)
                || records.values().any(|(_, child)| {
                    child.parent_id == Some(record.record_id)
                        && child.kind == Some(NativeObjectKind::EvalRun)
                        && !child.completed
                })
            {
                return Err(NativeRegistryError::Conflict);
            }
            Ok(NativeReply::new("reserved"))
        })
    }
    fn commit_delete(
        &self,
        request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.tombstone(TombstoneRequest {
            api_key_id: request.api_key_id,
            record_id: request.record_id,
            version: request.version,
        })
    }
    fn claim(
        &self,
        request: ClaimRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        assert_eq!(request.protocol, Some(NativeProtocol::Openai));

        let operations = self.operations.lock().unwrap();
        let mut records = self.records.lock().unwrap();
        let mut reply = NativeReply::new("leased");
        for (_, record) in records.values_mut() {
            if record.kind != request.kind
                || record.completed
                || record
                    .lease_expires_at
                    .is_some_and(|expires| expires > chrono::Utc::now())
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
        for (_, record) in self.records.lock().unwrap().values_mut() {
            if record.record_id == request.lease.record_id
                && record.lease_generation == Some(request.lease.generation)
            {
                record.lease_expires_at = None;
            }
        }
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
                    && record.parent_id == request.parent_id
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
                "raw_id": request.raw_id, "parent_raw_id":operation.parent_id.and_then(|parent|self.records.lock().unwrap().values().find(|(_,record)|record.record_id==parent).map(|(_,record)|record.raw_id.clone())), "api_key_id":operation.api_key_id, "observation_headers":operation.observation_headers, "version": 1, "entries":operation.entries, "input_file_id":operation.input_file_id,"operation":operation.operation
            })).unwrap();
            self.records.lock().unwrap().insert(
                record.raw_id.clone(),
                (operation.api_key_id, record.clone()),
            );
            if self
                .deny_lookup_after_bind
                .lock()
                .unwrap()
                .contains(&record.raw_id)
            {
                *self.lookup_error.lock().unwrap() = Some(NativeRegistryError::NotFound);
            }
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

fn definition() -> Value {
    json!({"name":"classification", "data_source_config":{"type":"custom","item_schema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}},
        "testing_criteria":[{"type":"string_check","name":"answer matches","input":"{{sample.output_text}}","reference":"{{item.answer}}","operation":"eq"}]})
}
fn run_body() -> Value {
    json!({"data_source":{"type":"completions","model":"upload-model","input_messages":{"type":"template","template":[{"role":"user","content":"Reply yes"}]},"source":{"type":"file_content","content":[{"item":{"answer":"yes"}}]}}})
}
async fn decoded(response: axum::response::Response, expected: StatusCode) -> Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(status, expected, "{}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}
async fn mount(upstream: &MockServer, verb: &str, route: &str, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(upstream)
        .await;
}
async fn create_eval(state: &ProxyState) -> Value {
    decoded(
        call(
            state,
            "POST",
            "/v1/evals?model=upload-model",
            "caller",
            Some(definition()),
        )
        .await,
        StatusCode::OK,
    )
    .await
}

#[tokio::test]
async fn exact_twelve_official_routes_preserve_raw_ids_and_manage_owned_tree() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    let eval =
        json!({"id":"eval_owned","object":"eval","name":"classification","created_at":1793843000});
    let run =
        json!({"id":"evalrun_owned","eval_id":"eval_owned","object":"eval.run","status":"queued"});
    let output = json!({"id":"item_owned","eval_id":"eval_owned","run_id":"evalrun_owned","object":"eval.run.output_item","sample":{"usage":{"prompt_tokens":99,"completion_tokens":8}}});
    for (verb, route, value) in [
        ("POST", "/v1/evals", eval.clone()),
        ("GET", "/v1/evals/eval_owned", eval.clone()),
        ("POST", "/v1/evals/eval_owned", eval.clone()),
        (
            "DELETE",
            "/v1/evals/eval_owned",
            json!({"eval_id":"eval_owned","deleted":true}),
        ),
        (
            "GET",
            "/v1/evals",
            json!({"object":"list","data":[eval],"has_more":false}),
        ),
        ("POST", "/v1/evals/eval_owned/runs", run.clone()),
        (
            "GET",
            "/v1/evals/eval_owned/runs/evalrun_owned",
            run.clone(),
        ),
        (
            "GET",
            "/v1/evals/eval_owned/runs",
            json!({"object":"list","data":[run],"has_more":false}),
        ),
        (
            "POST",
            "/v1/evals/eval_owned/runs/evalrun_owned/cancel",
            json!({"id":"evalrun_owned","eval_id":"eval_owned","status":"canceling"}),
        ),
        (
            "DELETE",
            "/v1/evals/eval_owned/runs/evalrun_owned",
            json!({"run_id":"evalrun_owned","deleted":true}),
        ),
        (
            "GET",
            "/v1/evals/eval_owned/runs/evalrun_owned/output_items",
            json!({"object":"list","data":[output.clone()],"has_more":false}),
        ),
        (
            "GET",
            "/v1/evals/eval_owned/runs/evalrun_owned/output_items/item_owned",
            output,
        ),
    ] {
        mount(&upstream, verb, route, value).await;
    }
    assert_eq!(create_eval(&state).await["id"], "eval_owned");
    for (verb, route, body) in [
        ("GET", "/v1/evals?model=upload-model", None),
        ("GET", "/v1/evals/eval_owned", None),
        (
            "POST",
            "/v1/evals/eval_owned",
            Some(json!({"name":"renamed"})),
        ),
        ("POST", "/v1/evals/eval_owned/runs", Some(run_body())),
        ("GET", "/v1/evals/eval_owned/runs", None),
        ("GET", "/v1/evals/eval_owned/runs/evalrun_owned", None),
        (
            "GET",
            "/v1/evals/eval_owned/runs/evalrun_owned/output_items",
            None,
        ),
        (
            "GET",
            "/v1/evals/eval_owned/runs/evalrun_owned/output_items/item_owned",
            None,
        ),
        (
            "POST",
            "/v1/evals/eval_owned/runs/evalrun_owned/cancel",
            None,
        ),
    ] {
        decoded(
            call(&state, verb, route, "caller", body).await,
            StatusCode::OK,
        )
        .await;
    }
    assert!(
        registry.completed.lock().unwrap().is_empty(),
        "cancel request and output sample never settle a queued run"
    );
    assert_eq!(
        registry
            .operations
            .lock()
            .unwrap()
            .values()
            .filter(|op| op.asynchronous)
            .count(),
        1,
        "only run submission enters durable inference observation"
    );
    let before = upstream.received_requests().await.unwrap().len();
    for route in [
        "/v1/evals/eval_owned",
        "/v1/evals/eval_owned/runs",
        "/v1/evals/eval_owned/runs/evalrun_owned",
        "/v1/evals/eval_owned/runs/evalrun_owned/output_items/item_owned",
    ] {
        assert_eq!(
            call(&state, "GET", route, "other-caller", None)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "foreign IDs must fail before account probing"
    );
    assert_eq!(
        call(
            &state,
            "DELETE",
            "/v1/evals/eval_owned/runs/evalrun_owned",
            "caller",
            None
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "active run deletion cannot erase the only remaining usage evidence"
    );
    registry
        .records
        .lock()
        .unwrap()
        .get_mut("evalrun_owned")
        .unwrap()
        .1
        .completed = true;
    decoded(
        call(
            &state,
            "DELETE",
            "/v1/evals/eval_owned/runs/evalrun_owned",
            "caller",
            None,
        )
        .await,
        StatusCode::OK,
    )
    .await;
    decoded(
        call(&state, "DELETE", "/v1/evals/eval_owned", "caller", None).await,
        StatusCode::OK,
    )
    .await;
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        12,
        "every official method/route must reach its intended provider route once"
    );
    let wire: Value = serde_json::from_slice(
        &requests
            .iter()
            .find(|req| req.method == "POST" && req.url.path().ends_with("/runs"))
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(wire["data_source"]["model"], "provider-model");
    assert!(
        requests
            .iter()
            .all(|req| !req.url.query_pairs().any(|(key, _)| key == "model")),
        "routing selector never enters provider query"
    );
}

#[tokio::test]
async fn list_crosses_foreign_only_page_and_rejects_foreign_cursor_before_dispatch() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry);
    mount(
        &upstream,
        "POST",
        "/v1/evals",
        json!({"id":"eval_owned","object":"eval"}),
    )
    .await;
    create_eval(&state).await;
    Mock::given(method("GET")).and(path("/v1/evals")).respond_with(|req:&wiremock::Request| {
        if req.url.query_pairs().any(|(k,v)|k=="after"&&v=="eval_foreign") {
            ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"eval_owned","name":"ours"}],"has_more":false}))
        } else { ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"eval_foreign","name":"TENANT_SECRET"}],"has_more":true,"last_id":"eval_foreign"})) }
    }).mount(&upstream).await;
    let list = decoded(
        call(
            &state,
            "GET",
            "/v1/evals?model=upload-model&limit=1",
            "caller",
            None,
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(list["data"], json!([{"id":"eval_owned","name":"ours"}]));
    assert_eq!(list["first_id"], "eval_owned");
    assert_eq!(list["last_id"], "eval_owned");
    assert!(!list.to_string().contains("eval_foreign"));
    let before = upstream.received_requests().await.unwrap().len();
    assert_eq!(
        call(
            &state,
            "GET",
            "/v1/evals?model=upload-model&after=eval_foreign",
            "caller",
            None
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn request_references_and_account_responses_sources_fail_before_submission() {
    let upstream = MockServer::start().await;
    let state = test_state(&upstream.uri(), Arc::new(Registry::default()));
    mount(
        &upstream,
        "POST",
        "/v1/evals",
        json!({"id":"eval_owned","object":"eval"}),
    )
    .await;
    create_eval(&state).await;
    let before = upstream.received_requests().await.unwrap().len();
    let mut body = run_body();
    body["data_source"]["source"] = json!({"type":"file_id","id":"file_foreign"});
    assert_eq!(
        call(
            &state,
            "POST",
            "/v1/evals/eval_owned/runs",
            "caller",
            Some(body)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let mut body = run_body();
    body["data_source"]["model"] = json!("private-model");
    assert_eq!(
        call(
            &state,
            "POST",
            "/v1/evals/eval_owned/runs",
            "caller",
            Some(body)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let body = json!({"data_source":{"type":"responses","source":{"type":"responses","metadata":{"tenant":"ours"}}}});
    let result = call(
        &state,
        "POST",
        "/v1/evals/eval_owned/runs",
        "caller",
        Some(body),
    )
    .await;
    let status = result.status();
    let bytes = to_bytes(result.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "unsafe account-level dataset projection must be explicitly unavailable"
    );
    assert!(String::from_utf8_lossy(&bytes).contains("unavailable"));
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "reference rejection and unsafe projection never call provider"
    );
}

#[tokio::test]
async fn same_owner_wrong_parent_and_foreign_output_do_not_probe_provider() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry);
    Mock::given(method("POST")).and(path("/v1/evals")).respond_with(|req:&wiremock::Request| {
        let body:Value=serde_json::from_slice(&req.body).unwrap();
        ResponseTemplate::new(200).set_body_json(json!({"id":if body["name"]=="second" {"eval_second"} else {"eval_owned"},"object":"eval"}))
    }).mount(&upstream).await;
    create_eval(&state).await;
    let mut second = definition();
    second["name"] = json!("second");
    decoded(
        call(
            &state,
            "POST",
            "/v1/evals?model=upload-model",
            "caller",
            Some(second),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    mount(
        &upstream,
        "POST",
        "/v1/evals/eval_owned/runs",
        json!({"id":"run_owned","eval_id":"eval_owned","object":"eval.run","status":"queued"}),
    )
    .await;
    decoded(
        call(
            &state,
            "POST",
            "/v1/evals/eval_owned/runs",
            "caller",
            Some(run_body()),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    let before = upstream.received_requests().await.unwrap().len();
    for (verb, route) in [
        ("GET", "/v1/evals/eval_second/runs/run_owned"),
        ("DELETE", "/v1/evals/eval_second/runs/run_owned"),
        ("POST", "/v1/evals/eval_second/runs/run_owned/cancel"),
        ("GET", "/v1/evals/eval_second/runs/run_owned/output_items"),
        (
            "GET",
            "/v1/evals/eval_owned/runs/run_owned/output_items/item_foreign",
        ),
    ] {
        assert_eq!(
            call(&state, verb, route, "caller", None).await.status(),
            StatusCode::NOT_FOUND,
            "{verb} {route}"
        );
    }
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "a readable sibling parent never authorizes someone else's child"
    );
}

#[tokio::test]
async fn grader_models_and_nested_input_files_are_authorized_in_their_actual_slots() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry);
    mount(
        &upstream,
        "POST",
        "/v1/evals",
        json!({"id":"eval_owned","object":"eval"}),
    )
    .await;
    let mut body = definition();
    body["testing_criteria"] = json!([{"type":"score_model","name":"quality","model":"private-model","input":[{"role":"user","content":"{{sample.output_text}}"}],"range":[0,1]}]);
    assert_eq!(
        call(
            &state,
            "POST",
            "/v1/evals?model=upload-model",
            "caller",
            Some(body)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        upstream.received_requests().await.unwrap().is_empty(),
        "grader model ACL is checked before definition registration reaches provider"
    );
    create_eval(&state).await;
    let before = upstream.received_requests().await.unwrap().len();
    let mut body = run_body();
    body["data_source"]["type"] = json!("responses");
    body["data_source"]["sampling_params"] = json!({"tools":[{"type":"code_interpreter","container":{"type":"auto","file_ids":["file_foreign"]}}]});
    assert_eq!(
        call(
            &state,
            "POST",
            "/v1/evals/eval_owned/runs",
            "caller",
            Some(body)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "File references inside sampling tools are checked before dispatch"
    );
}

fn seed_owned_file(registry: &Registry, raw_id: &str) {
    let (owner, mut record) = registry.records.lock().unwrap()["eval_owned"].clone();
    record.record_id = Uuid::new_v4();
    record.raw_id = raw_id.to_string();
    record.kind = Some(NativeObjectKind::File);
    record.parent_id = None;
    record.parent_raw_id = None;
    record.entries.clear();
    registry
        .records
        .lock()
        .unwrap()
        .insert(raw_id.to_string(), (owner, record));
}

#[tokio::test]
async fn explicit_item_trajectory_preserves_official_messages_and_opaque_dataset_fields() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry);
    mount(
        &upstream,
        "POST",
        "/v1/evals",
        json!({"id":"eval_owned","object":"eval"}),
    )
    .await;
    create_eval(&state).await;
    mount(
        &upstream,
        "POST",
        "/v1/evals/eval_owned/runs",
        json!({"id":"run_owned","eval_id":"eval_owned","object":"eval.run","status":"queued"}),
    )
    .await;
    let mut body = run_body();
    body["data_source"]["type"] = json!("responses");
    body["data_source"]["input_messages"] =
        json!({"type":"item_reference","item_reference":"item.input_trajectory"});
    body["data_source"]["source"] = json!({"type":"file_content","content":[
        {"item":{"answer":"yes","file_id":"file_foreign","input_trajectory":[{"role":"user","content":"Reply yes"}]}},
        {"item":{"answer":"yes","metadata":{"file_id":"file_foreign"},"input_trajectory":[{"role":"assistant","content":[{"type":"output_text","text":"yes"}]},{"role":"user","content":[{"type":"input_text","text":"Repeat yes"}]}]}}
    ]});
    decoded(
        call(
            &state,
            "POST",
            "/v1/evals/eval_owned/runs",
            "caller",
            Some(body.clone()),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    let requests = upstream.received_requests().await.unwrap();
    let wire: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    assert_eq!(
        wire["data_source"]["input_messages"],
        body["data_source"]["input_messages"]
    );
    assert_eq!(wire["data_source"]["source"], body["data_source"]["source"]);
    assert_eq!(requests.len(), 2, "opaque dataset fields are data; only definition creation and run submission may reach the provider");
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].url.path(), "/v1/evals/eval_owned/runs");
}

#[tokio::test]
async fn item_trajectory_uses_owned_jsonl_and_rejects_foreign_source_before_dispatch() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    mount(
        &upstream,
        "POST",
        "/v1/evals",
        json!({"id":"eval_owned","object":"eval"}),
    )
    .await;
    create_eval(&state).await;
    seed_owned_file(&registry, "file_dataset");
    mount(
        &upstream,
        "POST",
        "/v1/evals/eval_owned/runs",
        json!({"id":"run_owned","eval_id":"eval_owned","object":"eval.run","status":"queued"}),
    )
    .await;
    let mut body = run_body();
    body["data_source"]["type"] = json!("responses");
    body["data_source"]["input_messages"] =
        json!({"type":"item_reference","item_reference":"item.input_trajectory"});
    body["data_source"]["source"] = json!({"type":"file_id","id":"file_dataset"});
    decoded(
        call(
            &state,
            "POST",
            "/v1/evals/eval_owned/runs",
            "caller",
            Some(body.clone()),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "owned source files are admitted without reinterpreting their opaque dataset contents"
    );
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].url.path(), "/v1/evals/eval_owned/runs");
    let wire: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        wire["data_source"]["input_messages"],
        body["data_source"]["input_messages"]
    );
    assert_eq!(wire["data_source"]["source"], body["data_source"]["source"]);
    body["data_source"]["source"]["id"] = json!("file_foreign");
    assert_eq!(
        call(
            &state,
            "POST",
            "/v1/evals/eval_owned/runs",
            "caller",
            Some(body)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), requests.len(), "the protocol-defined source file slot remains subject to current ownership before dispatch");
}

#[tokio::test]
async fn accepted_run_bind_failure_records_uncertainty_without_repeating_or_releasing_the_id() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    mount(
        &upstream,
        "POST",
        "/v1/evals",
        json!({"id":"eval_owned","object":"eval"}),
    )
    .await;
    create_eval(&state).await;
    registry
        .fail_bind_for
        .lock()
        .unwrap()
        .insert("run_accepted".to_string());
    mount(
        &upstream,
        "POST",
        "/v1/evals/eval_owned/runs",
        json!({"id":"run_accepted","eval_id":"eval_owned","object":"eval.run","status":"queued"}),
    )
    .await;
    let response = call(
        &state,
        "POST",
        "/v1/evals/eval_owned/runs",
        "caller",
        Some(run_body()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("run_accepted"));
    let receipts = registry.reconciliations.lock().unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "a provider-accepted run must leave a durable reconciliation receipt"
    );
    assert_eq!(
        receipts[0].reason,
        ReconciliationReason::UncertainSubmission
    );
    assert!(matches!(
        receipts[0].target,
        MeteringTarget::Operation { .. }
    ));
    assert!(receipts[0].raw_usage.is_none());
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        2,
        "a post-accept authority failure must not resubmit inference"
    );
}

#[tokio::test]
async fn accepted_run_is_persisted_but_not_released_after_current_authority_is_withdrawn() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let state = test_state(&upstream.uri(), registry.clone());
    mount(
        &upstream,
        "POST",
        "/v1/evals",
        json!({"id":"eval_owned","object":"eval"}),
    )
    .await;
    create_eval(&state).await;
    registry
        .deny_lookup_after_bind
        .lock()
        .unwrap()
        .insert("run_accepted".to_string());
    mount(
        &upstream,
        "POST",
        "/v1/evals/eval_owned/runs",
        json!({"id":"run_accepted","eval_id":"eval_owned","object":"eval.run","status":"queued"}),
    )
    .await;
    let response = call(
        &state,
        "POST",
        "/v1/evals/eval_owned/runs",
        "caller",
        Some(run_body()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("run_accepted"));
    assert!(
        registry
            .records
            .lock()
            .unwrap()
            .contains_key("run_accepted"),
        "withdrawing current authority must retain the accepted run for observation"
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    assert!(
        registry.reconciliations.lock().unwrap().is_empty(),
        "a successfully bound run already has its durable observer identity"
    );
}
