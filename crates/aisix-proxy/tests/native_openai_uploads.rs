//! 原生 Uploads 的路由与 HTTP 组件回归；持久归属替身不代替真实控制面验收。
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
    lookups: Mutex<Vec<LookupRequest>>,
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
    unused!(tombstone, TombstoneRequest);
    fn owned(
        &self,
        request: OwnedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let records = self.records.lock().unwrap();
        let mut reply = NativeReply::new("owned");
        reply.records = request
            .raw_ids
            .iter()
            .filter_map(|raw| records.get(raw))
            .filter(|(owner, record)| {
                *owner == request.api_key_id
                    && !record.deleted
                    && record.protocol == Some(request.protocol)
                    && record.kind == Some(request.kind)
                    && record.origin.as_ref() == Some(&request.origin)
                    && request
                        .parent_id
                        .is_none_or(|parent| record.parent_id == Some(parent))
            })
            .map(|(_, record)| record.clone())
            .collect();
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
            let failures = self.fail_bind_for.lock().unwrap();
            if failures.contains(&request.raw_id)
                || request
                    .objects
                    .iter()
                    .any(|object| failures.contains(&object.raw_id))
            {
                return Err(NativeRegistryError::Unavailable);
            }
            drop(failures);
            let operations = self.operations.lock().unwrap();
            let operation = operations
                .get(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?;
            if operation.api_key_id != request.api_key_id {
                return Err(NativeRegistryError::NotFound);
            }
            let mut records = self.records.lock().unwrap();
            let record_id = if let Some(existing) = request.existing_record_id {
                let (owner, record) = records
                    .get(&request.raw_id)
                    .ok_or(NativeRegistryError::NotFound)?;
                if *owner != request.api_key_id || record.record_id != existing {
                    return Err(NativeRegistryError::NotFound);
                }
                existing
            } else {
                Uuid::new_v4()
            };
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id": record_id, "operation_id": request.operation_id,
                "api_key_id": request.api_key_id,
                "requested_model_id": operation.requested_model_id,
                "requested_model": operation.requested_model, "upstream_model": operation.upstream_model,
                "origin": operation.origin, "protocol": operation.protocol,
                "kind": operation.kind, "parent_id": operation.parent_id,
                "raw_id": request.raw_id, "version": 1,
                "entries": operation.entries, "input_file_id": operation.input_file_id
            })).unwrap();
            records.insert(
                record.raw_id.clone(),
                (operation.api_key_id, record.clone()),
            );
            let mut reply = NativeReply::new("bound");
            for object in request.objects {
                let mut child = record.clone();
                child.record_id = Uuid::new_v4();
                child.raw_id = object.raw_id;
                child.kind = Some(object.kind);
                child.parent_id = Some(record.record_id);
                records.insert(child.raw_id.clone(), (operation.api_key_id, child.clone()));
                reply.records.push(child);
            }
            reply.record = Some(record);
            Ok(reply)
        })
    }
    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.lookups.lock().unwrap().push(request.clone());
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

fn app(base: &str, registry: Arc<Registry>) -> (axum::Router, SnapshotHandle<AisixSnapshot>) {
    app_configured(base, registry, |_| {})
}

fn app_configured(
    base: &str,
    registry: Arc<Registry>,
    configure: impl FnOnce(&AisixSnapshot),
) -> (axum::Router, SnapshotHandle<AisixSnapshot>) {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"uploads-account", "secret":"upstream-key", "provider":"openai",
        "api_base":format!("{}/v1",base.trim_end_matches('/').trim_end_matches("/v1")), "adapter":"openai"
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
    configure(&snapshot);
    let snapshot = SnapshotHandle::new(snapshot);
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let state = ProxyState::new(snapshot.clone(), hub, &config)
        .without_cache()
        .with_guardrail_index(guardrails)
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "native-uploads-test",
        ))
        .with_native_objects(registry);
    (build_router(state), snapshot)
}
fn upload(id: &str, status: &str) -> Value {
    json!({"id":id,"object":"upload","bytes":4,"created_at":1730000000i64,
        "expires_at":4102444800i64,"filename":"notes.bin","purpose":"user_data", "status":status,
        "extension":{"retained":true}})
}
fn file() -> Value {
    json!({"id":"file_complete","object":"file","bytes":4,"created_at":1730000001i64,
        "filename":"notes.bin","purpose":"user_data","status":"processed"})
}
fn create_body() -> Value {
    json!({"bytes":4,"filename":"notes.bin","mime_type":"application/octet-stream","purpose":"user_data",
        "expires_after":{"anchor":"created_at","seconds":3600},"future_parameter":{"preserved":true}})
}
async fn json_request(
    router: axum::Router,
    uri: &str,
    value: Option<Value>,
    caller: &str,
    hint: bool,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {caller}"));
    if hint {
        request = request.header("x-aisix-model", "upload-model");
    }
    let body = match value {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    router.oneshot(request.body(body).unwrap()).await.unwrap()
}
async fn value(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
}
async fn create(router: axum::Router, upstream: &MockServer, id: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload(id, "pending")))
        .expect(1)
        .mount(upstream)
        .await;
    let answer = json_request(router, "/v1/uploads", Some(create_body()), "caller", true).await;
    let status = answer.status();
    let body = value(answer).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "create {id}: {body}; upstream requests: {:?}",
        upstream.received_requests().await.unwrap()
    );
    assert_eq!(body, upload(id, "pending"));
}
async fn part_request(router: axum::Router, id: &str, caller: &str) -> axum::response::Response {
    let mut body=b"--upload-test\r\nContent-Disposition: form-data; name=\"data\"; filename=\"part.bin\"\r\nContent-Type: application/octet-stream\r\nX-Future-Part: keep-header\r\n\r\n".to_vec();
    body.extend_from_slice(&[255, 0, 1, 2]);
    body.extend_from_slice(b"\r\n--upload-test\r\nContent-Disposition: form-data; name=\"future_option\"\r\n\r\nkeep-me\r\n--upload-test--\r\n");
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/uploads/{id}/parts?x=a%2Fb&x=two"))
                .header("authorization", format!("Bearer {caller}"))
                .header("content-type", "multipart/form-data; boundary=upload-test")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn native_upload_create_preserves_wire_and_explicit_routing_query() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload("upload_owned", "pending")))
        .expect(1)
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    let answer = json_request(
        router,
        "/v1/uploads?model=upload-model&x=a%2Fb&x=two",
        Some(create_body()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, upload("upload_owned", "pending"));
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.query(), Some("x=a%2Fb&x=two"));
    assert_eq!(requests[0].body_json::<Value>().unwrap(), create_body());
    assert_eq!(requests[0].headers["authorization"], "Bearer upstream-key");
    let records = registry.records.lock().unwrap();
    let record = &records["upload_owned"].1;
    assert_eq!(serde_json::to_value(record.kind).unwrap(), json!("upload"));
    assert!(record.parent_id.is_none());
}

#[tokio::test]
async fn native_upload_requires_selector_and_unknown_upload_never_probes_an_account() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let answer = json_request(
        router.clone(),
        "/v1/uploads",
        Some(create_body()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::BAD_REQUEST);
    let answer = part_request(router, "upload_missing", "caller").await;
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn native_upload_bind_failure_releases_no_id_and_never_resends_creation() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload("upload_private", "pending")))
        .expect(1)
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    registry
        .fail_bind_for
        .lock()
        .unwrap()
        .insert("upload_private".into());
    let (router, _) = app(&upstream.uri(), registry);
    let answer = json_request(router, "/v1/uploads", Some(create_body()), "caller", true).await;
    assert!(!answer.status().is_success());
    assert!(!value(answer).await.to_string().contains("upload_private"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn native_upload_parts_complete_and_result_file_follow_one_owned_origin() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create(router.clone(), &upstream, "upload_owned").await;
    let part = json!({"id":"part_owned","object":"upload.part","upload_id":"upload_owned","created_at":1730000001i64,"future":7});
    Mock::given(method("POST"))
        .and(path("/v1/uploads/upload_owned/parts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&part))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = part_request(router.clone(), "upload_owned", "caller").await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, part);
    let mut completed = upload("upload_owned", "completed");
    completed["file"] = file();
    Mock::given(method("POST"))
        .and(path("/v1/uploads/upload_owned/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&completed))
        .expect(1)
        .mount(&upstream)
        .await;
    let completion =
        json!({"part_ids":["part_owned"],"md5":"unchanged-md5","future_complete":true});
    let answer = json_request(
        router.clone(),
        "/v1/uploads/upload_owned/complete",
        Some(completion.clone()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, completed);
    let records = registry.records.lock().unwrap().clone();
    let parent = records["upload_owned"].1.record_id;
    assert_eq!(records["part_owned"].1.parent_id, Some(parent));
    assert_eq!(
        records["file_complete"].1.kind,
        Some(NativeObjectKind::File)
    );
    assert!(records["file_complete"].1.parent_id.is_none());
    drop(records);
    let requests = upstream.received_requests().await.unwrap();
    let sent_part = &requests[1];
    assert_eq!(sent_part.url.query(), Some("x=a%2Fb&x=two"));
    assert!(sent_part
        .body
        .windows(4)
        .any(|bytes| bytes == [255, 0, 1, 2]));
    assert!(String::from_utf8_lossy(&sent_part.body).contains("keep-me"));
    assert!(String::from_utf8_lossy(&sent_part.body)
        .to_ascii_lowercase()
        .contains("x-future-part: keep-header"));
    assert_eq!(requests[2].body_json::<Value>().unwrap(), completion);
    Mock::given(method("GET"))
        .and(path("/v1/files/file_complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(file()))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = router
        .oneshot(
            Request::builder()
                .uri("/v1/files/file_complete")
                .header("authorization", "Bearer caller")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, file());
}

#[tokio::test]
async fn native_upload_foreign_owner_and_foreign_part_never_reach_upstream() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create(router.clone(), &upstream, "upload_owned").await;
    let answer = part_request(router.clone(), "upload_owned", "other-caller").await;
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    let answer = json_request(
        router,
        "/v1/uploads/upload_owned/complete",
        Some(json!({"part_ids":["part_foreign"]})),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn native_upload_revocation_and_rotated_account_close_access_before_dispatch() {
    for rotate in [false, true] {
        let upstream = MockServer::builder().start().await;
        let (router, snapshot) = app(&upstream.uri(), Arc::new(Registry::default()));
        create(router.clone(), &upstream, "upload_owned").await;
        if rotate {
            let entry = snapshot.load().provider_keys.get_by_id(PROVIDER).unwrap();
            let mut changed = entry.value.as_ref().clone();
            changed.api_key = "new-account-key".into();
            snapshot
                .load()
                .provider_keys
                .insert(ResourceEntry::new(PROVIDER, changed, 2));
        } else {
            let mut key = snapshot
                .load()
                .apikeys
                .get_by_id(KEY)
                .unwrap()
                .value
                .as_ref()
                .clone();
            key.allowed_models = vec!["unrelated-model".into()];
            snapshot
                .load()
                .apikeys
                .insert(ResourceEntry::new(KEY, key, 2));
        }
        let answer = part_request(router, "upload_owned", "caller").await;
        assert_eq!(answer.status(), StatusCode::NOT_FOUND);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn native_upload_part_parent_mismatch_and_file_bind_failure_release_no_new_ids() {
    for wrong_parent in [true, false] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, _) = app(&upstream.uri(), registry.clone());
        create(router.clone(), &upstream, "upload_owned").await;
        let (route, request_body, answer_body) = if wrong_parent {
            (
                "/v1/uploads/upload_owned/parts",
                None,
                json!({"id":"part_private","object":"upload.part","upload_id":"upload_elsewhere","created_at":1730000001i64}),
            )
        } else {
            registry
                .fail_bind_for
                .lock()
                .unwrap()
                .insert("file_complete".into());
            let mut completed = upload("upload_owned", "completed");
            completed["file"] = file();
            (
                "/v1/uploads/upload_owned/complete",
                Some(json!({"part_ids":[]})),
                completed,
            )
        };
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer_body))
            .expect(1)
            .mount(&upstream)
            .await;
        let answer = if wrong_parent {
            part_request(router, "upload_owned", "caller").await
        } else {
            json_request(router, route, request_body, "caller", false).await
        };
        assert!(!answer.status().is_success());
        let answer = value(answer).await.to_string();
        assert!(!answer.contains("part_private"));
        assert!(!answer.contains("file_complete"));
        assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn native_upload_cancel_preserves_empty_body_and_native_terminal_response() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    create(router.clone(), &upstream, "upload_owned").await;
    Mock::given(method("POST"))
        .and(path("/v1/uploads/upload_owned/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload("upload_owned", "cancelled")))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = json_request(
        router,
        "/v1/uploads/upload_owned/cancel",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, upload("upload_owned", "cancelled"));
    let requests = upstream.received_requests().await.unwrap();
    assert!(requests[1].body.is_empty());
}

#[tokio::test]
async fn native_upload_terminal_success_requires_the_official_status_and_completed_file() {
    for scenario in [
        "create_status",
        "complete_status",
        "missing_file",
        "cancel_status",
    ] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, _) = app(&upstream.uri(), registry.clone());
        let (route, body, mut reply) = if scenario == "create_status" {
            (
                "/v1/uploads",
                Some(create_body()),
                upload("upload_private", "completed"),
            )
        } else {
            create(router.clone(), &upstream, "upload_owned").await;
            if scenario == "cancel_status" {
                (
                    "/v1/uploads/upload_owned/cancel",
                    None,
                    upload("upload_owned", "pending"),
                )
            } else {
                (
                    "/v1/uploads/upload_owned/complete",
                    Some(json!({"part_ids":[]})),
                    upload(
                        "upload_owned",
                        if scenario == "complete_status" {
                            "pending"
                        } else {
                            "completed"
                        },
                    ),
                )
            }
        };
        if scenario == "complete_status" {
            reply["file"] = file();
        }
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply))
            .expect(1)
            .mount(&upstream)
            .await;
        let answer = json_request(router, route, body, "caller", scenario == "create_status").await;
        assert!(!answer.status().is_success(), "scenario {scenario}");
        let answer = value(answer).await.to_string();
        assert!(!answer.contains("upload_private"));
        assert!(!answer.contains("file_complete"));
        assert!(!registry
            .records
            .lock()
            .unwrap()
            .contains_key("file_complete"));
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            if scenario == "create_status" { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn native_upload_every_addressed_operation_rechecks_owner_and_registry_availability() {
    for denied in [
        NativeRegistryError::NotFound,
        NativeRegistryError::Unavailable,
    ] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, _) = app(&upstream.uri(), registry.clone());
        create(router.clone(), &upstream, "upload_owned").await;
        *registry.lookup_error.lock().unwrap() = Some(denied);
        let part = part_request(router.clone(), "upload_owned", "caller").await;
        let complete = json_request(
            router.clone(),
            "/v1/uploads/upload_owned/complete",
            Some(json!({"part_ids":[]})),
            "caller",
            false,
        )
        .await;
        let cancel = json_request(
            router,
            "/v1/uploads/upload_owned/cancel",
            None,
            "caller",
            false,
        )
        .await;
        for answer in [part, complete, cancel] {
            assert_eq!(
                answer.status(),
                if denied == NativeRegistryError::NotFound {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            );
            assert!(!value(answer).await.to_string().contains("upload_owned"));
        }
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn native_upload_current_group_endpoint_and_credential_boundary_are_not_reusable() {
    for change in ["group", "base", "organization", "disabled", "model_account"] {
        let upstream = MockServer::builder().start().await;
        let (router, snapshot) = app(&upstream.uri(), Arc::new(Registry::default()));
        create(router.clone(), &upstream, "upload_owned").await;
        if change == "group" || change == "disabled" {
            let mut key = snapshot
                .load()
                .apikeys
                .get_by_id(KEY)
                .unwrap()
                .value
                .as_ref()
                .clone();
            if change == "group" {
                key.allowed_provider_groups = vec!["new-group".into()];
            } else {
                key.disabled = true;
            }
            snapshot
                .load()
                .apikeys
                .insert(ResourceEntry::new(KEY, key, 2));
        } else if change == "model_account" {
            let mut model = snapshot
                .load()
                .models
                .get_by_id(MODEL)
                .unwrap()
                .value
                .as_ref()
                .clone();
            model.provider_key_id = Some("55555555-5555-4555-8555-555555555555".into());
            let provider = snapshot
                .load()
                .provider_keys
                .get_by_id(PROVIDER)
                .unwrap()
                .value
                .as_ref()
                .clone();
            snapshot.load().provider_keys.insert(ResourceEntry::new(
                "55555555-5555-4555-8555-555555555555",
                provider,
                1,
            ));
            snapshot
                .load()
                .models
                .insert(ResourceEntry::new(MODEL, model, 2));
        } else {
            let mut provider = serde_json::to_value(
                &snapshot
                    .load()
                    .provider_keys
                    .get_by_id(PROVIDER)
                    .unwrap()
                    .value,
            )
            .unwrap();
            if change == "base" {
                provider["api_base"] = json!(format!("{}/changed-account", upstream.uri()));
            } else {
                provider["request"] =
                    json!({"default_headers":{"openai-organization":"changed-org"}});
            }
            snapshot.load().provider_keys.insert(ResourceEntry::new(
                PROVIDER,
                serde_json::from_value::<ProviderKey>(provider).unwrap(),
                2,
            ));
        }
        let part = part_request(router.clone(), "upload_owned", "caller").await;
        let complete = json_request(
            router.clone(),
            "/v1/uploads/upload_owned/complete",
            Some(json!({"part_ids":[]})),
            "caller",
            false,
        )
        .await;
        let cancel = json_request(
            router,
            "/v1/uploads/upload_owned/cancel",
            None,
            "caller",
            false,
        )
        .await;
        for answer in [part, complete, cancel] {
            assert_eq!(
                answer.status(),
                if change == "disabled" {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::NOT_FOUND
                },
                "change {change}"
            );
        }
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
}

fn block_all(snapshot: &AisixSnapshot, hook: &str) {
    let script = if hook == "input" {
        "export function checkInput() { return { action: 'block', reason_code: 'blocked' }; }"
    } else {
        "export function checkOutput() { return { action: 'block', reason_code: 'blocked' }; }"
    };
    let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
        "name":"upload-block-all", "enabled":true, "kind":"custom", "hook_point":hook,
        "fail_open":false, "script":script, "timeout_ms":5000
    }))
    .unwrap();
    let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
        "guardrail_id":"upload-guardrail", "scope_type":"env", "priority":100
    }))
    .unwrap();
    snapshot
        .guardrails
        .insert(ResourceEntry::new("upload-guardrail", guardrail, 1));
    snapshot
        .guardrail_attachments
        .insert(ResourceEntry::new("upload-attachment", attachment, 1));
}

#[tokio::test]
async fn native_upload_input_guardrails_cover_create_binary_parts_complete_and_empty_cancel() {
    for operation in ["create", "parts", "complete", "cancel"] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, snapshot) = app(&upstream.uri(), registry);
        if operation != "create" {
            create(router.clone(), &upstream, "upload_owned").await;
        }
        block_all(&snapshot.load(), "input");
        let answer = match operation {
            "create" => {
                json_request(router, "/v1/uploads", Some(create_body()), "caller", true).await
            }
            "parts" => part_request(router, "upload_owned", "caller").await,
            "complete" => {
                json_request(
                    router,
                    "/v1/uploads/upload_owned/complete",
                    Some(json!({"part_ids":[]})),
                    "caller",
                    false,
                )
                .await
            }
            _ => {
                json_request(
                    router,
                    "/v1/uploads/upload_owned/cancel",
                    None,
                    "caller",
                    false,
                )
                .await
            }
        };
        assert_eq!(
            answer.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "operation {operation}"
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            usize::from(operation != "create")
        );
    }
}

#[tokio::test]
async fn native_upload_output_guardrail_holds_the_registered_identity() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app_configured(&upstream.uri(), registry.clone(), |snapshot| {
        block_all(snapshot, "output")
    });
    Mock::given(method("POST"))
        .and(path("/v1/uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload("upload_private", "pending")))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = json_request(router, "/v1/uploads", Some(create_body()), "caller", true).await;
    assert_eq!(answer.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!value(answer).await.to_string().contains("upload_private"));
    assert!(registry
        .records
        .lock()
        .unwrap()
        .contains_key("upload_private"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

async fn completed_file(router: axum::Router, upstream: &MockServer) {
    create(router.clone(), upstream, "upload_owned").await;
    let mut completed = upload("upload_owned", "completed");
    completed["file"] = file();
    Mock::given(method("POST"))
        .and(path("/v1/uploads/upload_owned/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&completed))
        .expect(1)
        .mount(upstream)
        .await;
    let answer = json_request(
        router,
        "/v1/uploads/upload_owned/complete",
        Some(json!({"part_ids":[]})),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, completed);
}

fn inference_body(responses: bool, model: &str, file_id: &str) -> Value {
    if responses {
        json!({"model":model, "input":[{"role":"user","content":[{"type":"input_file","file_id":file_id}]}],"store":false})
    } else {
        json!({"model":model, "messages":[{"role":"user","content":[{"type":"file","file":{"file_id":file_id}}]}],"store":false,"stream":false})
    }
}

fn inference_reply(responses: bool) -> Value {
    if responses {
        json!({"id":"resp_used_file","object":"response","created_at":1730000010i64,"status":"completed",
            "model":"provider-model","output":[{"id":"msg_used_file","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"read the file","annotations":[]}]}],
            "usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}})
    } else {
        json!({"id":"chatcmpl_used_file","object":"chat.completion","created":1730000010i64,"model":"provider-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"read the file"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}})
    }
}

fn reader_model(snapshot: &AisixSnapshot, provider_id: &str) {
    let reader: Model =
        serde_json::from_value(json!({"display_name":"reader-model","provider":"openai",
        "model_name":"provider-model","provider_key_id":provider_id}))
        .unwrap();
    snapshot.models.insert(ResourceEntry::new(
        "66666666-6666-4666-8666-666666666666",
        reader,
        1,
    ));
    for key_id in [KEY, OTHER_KEY] {
        let mut key = snapshot
            .apikeys
            .get_by_id(key_id)
            .unwrap()
            .value
            .as_ref()
            .clone();
        key.allowed_models = vec!["upload-model".into(), "reader-model".into()];
        snapshot.apikeys.insert(ResourceEntry::new(key_id, key, 2));
    }
}

#[tokio::test]
async fn completed_upload_file_is_consumed_by_chat_and_responses_on_another_model_same_account() {
    for responses in [false, true] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, _) = app_configured(&upstream.uri(), registry, |snapshot| {
            reader_model(snapshot, PROVIDER)
        });
        completed_file(router.clone(), &upstream).await;
        let route = if responses {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(inference_reply(responses)))
            .expect(1)
            .mount(&upstream)
            .await;
        let answer = json_request(
            router,
            route,
            Some(inference_body(responses, "reader-model", "file_complete")),
            "caller",
            false,
        )
        .await;
        let status = answer.status();
        let body = value(answer).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "responses {responses}: {body}; upstream requests: {:?}",
            upstream.received_requests().await.unwrap()
        );
        assert!(body.to_string().contains("read the file"));
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let sent = requests[2].body_json::<Value>().unwrap();
        assert_eq!(sent["model"], "provider-model");
        assert_eq!(
            if responses {
                &sent["input"][0]["content"][0]["file_id"]
            } else {
                &sent["messages"][0]["content"][0]["file"]["file_id"]
            },
            "file_complete"
        );
    }
}

#[tokio::test]
async fn completed_upload_file_keeps_authority_scope_and_request_account_in_both_consumers() {
    for responses in [false, true] {
        for denied in [
            "foreign",
            "unknown",
            "revoked_source_model",
            "other_account",
            "authority_unavailable",
        ] {
            let upstream = MockServer::builder().start().await;
            let registry = Arc::new(Registry::default());
            let (router, snapshot) =
                app_configured(&upstream.uri(), registry.clone(), |snapshot| {
                    reader_model(snapshot, PROVIDER)
                });
            completed_file(router.clone(), &upstream).await;
            if denied == "revoked_source_model" {
                let mut key = snapshot
                    .load()
                    .apikeys
                    .get_by_id(KEY)
                    .unwrap()
                    .value
                    .as_ref()
                    .clone();
                key.allowed_models = vec!["reader-model".into()];
                snapshot
                    .load()
                    .apikeys
                    .insert(ResourceEntry::new(KEY, key, 3));
            } else if denied == "other_account" {
                let provider = snapshot
                    .load()
                    .provider_keys
                    .get_by_id(PROVIDER)
                    .unwrap()
                    .value
                    .as_ref()
                    .clone();
                let other = "77777777-7777-4777-8777-777777777777";
                snapshot
                    .load()
                    .provider_keys
                    .insert(ResourceEntry::new(other, provider, 1));
                reader_model(&snapshot.load(), other);
            } else if denied == "authority_unavailable" {
                *registry.lookup_error.lock().unwrap() = Some(NativeRegistryError::Unavailable);
            }
            let route = if responses {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            let file_id = if denied == "unknown" {
                "file_foreign"
            } else {
                "file_complete"
            };
            let caller = if denied == "foreign" {
                "other-caller"
            } else {
                "caller"
            };
            let answer = json_request(
                router,
                route,
                Some(inference_body(responses, "reader-model", file_id)),
                caller,
                false,
            )
            .await;
            assert_eq!(
                answer.status(),
                if denied == "authority_unavailable" {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::NOT_FOUND
                },
                "responses {responses}, denied {denied}"
            );
            assert!(!value(answer).await.to_string().contains(file_id));
            assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn admitting_a_file_does_not_admit_other_stored_objects_or_nested_extension_keys() {
    for responses in [false, true] {
        for extra in ["stored_object", "nested_extension"] {
            let upstream = MockServer::builder().start().await;
            let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
            completed_file(router.clone(), &upstream).await;
            let mut body = inference_body(responses, "upload-model", "file_complete");
            if responses {
                if extra == "stored_object" {
                    body["conversation"] = json!("conv_private");
                } else {
                    body["input"][0]["extension"] =
                        json!({"type":"input_file","file_id":"file_complete"});
                }
            } else if extra == "stored_object" {
                body["messages"][0]["audio"] = json!({"id":"audio_private"});
            } else {
                body["provider"] = json!("unapproved-provider");
            }
            let route = if responses {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            let answer = json_request(router, route, Some(body), "caller", false).await;
            assert_eq!(answer.status(), StatusCode::BAD_REQUEST);
            assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn renamed_upload_model_keeps_original_file_provenance_and_current_access() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, snapshot) = app(&upstream.uri(), registry.clone());
    create(router.clone(), &upstream, "upload_owned").await;
    let mut model = snapshot
        .load()
        .models
        .get_by_id(MODEL)
        .unwrap()
        .value
        .as_ref()
        .clone();
    model.display_name = "renamed-upload-model".into();
    snapshot
        .load()
        .models
        .insert(ResourceEntry::new(MODEL, model, 2));
    let mut key = snapshot
        .load()
        .apikeys
        .get_by_id(KEY)
        .unwrap()
        .value
        .as_ref()
        .clone();
    key.allowed_models = vec!["renamed-upload-model".into()];
    snapshot
        .load()
        .apikeys
        .insert(ResourceEntry::new(KEY, key, 2));
    let mut completed = upload("upload_owned", "completed");
    completed["file"] = file();
    Mock::given(method("POST"))
        .and(path("/v1/uploads/upload_owned/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&completed))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = json_request(
        router.clone(),
        "/v1/uploads/upload_owned/complete",
        Some(json!({"part_ids":[]})),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, completed);
    let records = registry.records.lock().unwrap().clone();
    assert_eq!(records["file_complete"].1.requested_model, "upload-model");
    assert_eq!(
        records["file_complete"].1.requested_model_id,
        Some(Uuid::parse_str(MODEL).unwrap())
    );
    assert_eq!(
        records["file_complete"].1.origin,
        records["upload_owned"].1.origin
    );
    drop(records);
    Mock::given(method("GET"))
        .and(path("/v1/files/file_complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(file()))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = router
        .oneshot(
            Request::builder()
                .uri("/v1/files/file_complete")
                .header("authorization", "Bearer caller")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, file());
}

#[tokio::test]
async fn native_upload_missing_expiry_never_registers_a_nonexpiring_upload() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    let mut reply = upload("upload_private", "pending");
    reply.as_object_mut().unwrap().remove("expires_at");
    Mock::given(method("POST"))
        .and(path("/v1/uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = json_request(router, "/v1/uploads", Some(create_body()), "caller", true).await;
    assert!(!answer.status().is_success());
    assert!(!value(answer).await.to_string().contains("upload_private"));
    assert!(registry.records.lock().unwrap().is_empty());
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn native_upload_part_obeys_the_configured_request_limit_before_native_send() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    create(router.clone(), &upstream, "upload_owned").await;
    let mut payload = b"--large-upload\r\nContent-Disposition: form-data; name=\"data\"; filename=\"part.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
    payload.resize(payload.len() + (2 << 20), 42);
    payload.extend_from_slice(b"\r\n--large-upload--\r\n");
    let answer = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/uploads/upload_owned/parts")
                .header("authorization", "Bearer caller")
                .header("content-type", "multipart/form-data; boundary=large-upload")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn completed_batch_upload_derives_authorized_manifest_and_keeps_public_input_id() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    let lines = [
        json!({"custom_id":"slow","method":"POST","url":"/v1/chat/completions",
            "body":{"model":"upload-model","service_tier":"flex","messages":[{"role":"user","content":"Classify first text"}]}}),
        json!({"custom_id":"fast","method":"POST","url":"/v1/chat/completions",
            "body":{"model":"upload-model","service_tier":"priority","messages":[{"role":"user","content":"Classify second text"}]}}),
    ];
    let manifest = format!("{}\n{}\n", lines[0], lines[1]);
    let mut created = upload("upload_batch", "pending");
    created["bytes"] = json!(manifest.len());
    created["purpose"] = json!("batch");
    created["filename"] = json!("requests.jsonl");
    let mut create_request = create_body();
    create_request["bytes"] = created["bytes"].clone();
    create_request["purpose"] = created["purpose"].clone();
    create_request["filename"] = created["filename"].clone();
    create_request["mime_type"] = json!("application/jsonl");
    Mock::given(method("POST"))
        .and(path("/v1/uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&created))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = json_request(
        router.clone(),
        "/v1/uploads",
        Some(create_request),
        "caller",
        true,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, created);
    Mock::given(method("POST"))
        .and(path("/v1/uploads/upload_batch/parts"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"part_batch","object":"upload.part",
            "upload_id":"upload_batch","created_at":1730000001i64})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let part_body = format!("--batch-upload\r\nContent-Disposition: form-data; name=\"data\"; filename=\"requests.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{manifest}\r\n--batch-upload--\r\n");
    let answer = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/uploads/upload_batch/parts")
                .header("authorization", "Bearer caller")
                .header("content-type", "multipart/form-data; boundary=batch-upload")
                .body(Body::from(part_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::OK);
    let mut source_file = file();
    source_file["id"] = json!("file_batch_source");
    source_file["bytes"] = created["bytes"].clone();
    source_file["filename"] = created["filename"].clone();
    source_file["purpose"] = created["purpose"].clone();
    let mut completed = created;
    completed["status"] = json!("completed");
    completed["file"] = source_file.clone();
    Mock::given(method("POST"))
        .and(path("/v1/uploads/upload_batch/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&completed))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = json_request(
        router.clone(),
        "/v1/uploads/upload_batch/complete",
        Some(json!({"part_ids":["part_batch"]})),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, completed);
    assert!(registry.records.lock().unwrap()["file_batch_source"]
        .1
        .entries
        .is_empty());

    Mock::given(method("GET"))
        .and(path("/v1/files/file_batch_source"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&source_file))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/files/file_batch_source/content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(&manifest))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut derived_file = source_file;
    derived_file["id"] = json!("file_batch_derived");
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&derived_file))
        .expect(1)
        .mount(&upstream)
        .await;
    let batch = json!({"id":"batch_from_upload","object":"batch","status":"in_progress",
        "input_file_id":"file_batch_derived","endpoint":"/v1/chat/completions","completion_window":"24h",
        "created_at":1730000010i64,"metadata":{"kept":"yes"}});
    Mock::given(method("POST"))
        .and(path("/v1/batches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&batch))
        .expect(1)
        .mount(&upstream)
        .await;
    let request = json!({"input_file_id":"file_batch_source","endpoint":"/v1/chat/completions",
        "completion_window":"24h","metadata":{"kept":"yes"},"future_batch":{"preserved":true}});
    let answer = json_request(
        router.clone(),
        "/v1/batches",
        Some(request.clone()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    let mut public_batch = batch.clone();
    public_batch["input_file_id"] = json!("file_batch_source");
    assert_eq!(value(answer).await, public_batch);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 7);
    let sent_file = String::from_utf8(requests[5].body.clone()).unwrap();
    assert!(sent_file.contains("name=\"purpose\"\r\n\r\nbatch"));
    assert!(!sent_file.contains("\"model\":\"upload-model\""));
    assert!(sent_file.contains("\"model\":\"provider-model\""));
    assert!(sent_file.contains("\"custom_id\":\"slow\""));
    assert!(sent_file.contains("\"custom_id\":\"fast\""));
    assert!(sent_file.contains("\"service_tier\":\"flex\""));
    assert!(sent_file.contains("\"service_tier\":\"priority\""));
    let mut native_request = request;
    native_request["input_file_id"] = json!("file_batch_derived");
    assert_eq!(requests[6].body_json::<Value>().unwrap(), native_request);
    let records = registry.records.lock().unwrap().clone();
    let source = &records["file_batch_source"].1;
    let derived = &records["file_batch_derived"].1;
    assert_eq!(derived.parent_id, Some(source.record_id));
    assert_eq!(derived.kind, Some(NativeObjectKind::File));
    assert_eq!(derived.origin, source.origin);
    assert_eq!(derived.entries.len(), 2);
    let batch_record = &records["batch_from_upload"].1;
    assert_eq!(
        batch_record.input_file_id.as_deref(),
        Some("file_batch_source")
    );
    assert_eq!(batch_record.entries, derived.entries);
    for (index, (id, tier)) in [("slow", "flex"), ("fast", "priority")]
        .into_iter()
        .enumerate()
    {
        let entry = &batch_record.entries[index];
        assert_eq!(entry.endpoint, "/v1/chat/completions");
        assert_eq!(entry.entry_id, id);
        assert_eq!(entry.requested_service_tier.as_deref(), Some(tier));
        assert_eq!(entry.requested_model, "upload-model");
        assert_eq!(entry.requested_model_id, Uuid::parse_str(MODEL).unwrap());
        assert_eq!(entry.model_id, Uuid::parse_str(MODEL).unwrap());
        assert_eq!(entry.upstream_model, "provider-model");
    }
    drop(records);
    Mock::given(method("GET"))
        .and(path("/v1/batches/batch_from_upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&batch))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/batches"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object":"list","data":[batch],"has_more":false,
            "first_id":"batch_from_upload","last_id":"batch_from_upload"})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    for (route, list) in [
        ("/v1/batches/batch_from_upload", false),
        ("/v1/batches?model=upload-model", true),
    ] {
        let answer = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(route)
                    .header("authorization", "Bearer caller")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::OK);
        let answer = value(answer).await;
        assert_eq!(
            if list {
                &answer["data"][0]["input_file_id"]
            } else {
                &answer["input_file_id"]
            },
            "file_batch_source"
        );
    }
}

fn response_tool_file_body(slot: &str, file_id: &str) -> Value {
    let mut body = json!({"model":"upload-model","input":"Use the uploaded file","store":false});
    let code = json!({"type":"code_interpreter","container":{"type":"auto","file_ids":[file_id],"memory_limit":"1g"}});
    let shell = json!({"type":"shell","environment":{"type":"container_auto","file_ids":[file_id],"memory_limit":"1g"}});
    let image = json!({"type":"image_generation","input_image_mask":{"file_id":file_id}});
    match slot {
        "image" => body["tools"] = json!([image]),
        "code" => body["tools"] = json!([code]),
        "shell" => body["tools"] = json!([shell]),
        "choice" => {
            body["tool_choice"] = json!({"type":"allowed_tools","mode":"auto","tools":[image]})
        }
        "additional" => body["input"] = json!([{"type":"additional_tools","tools":[code]}]),
        "search" => {
            body["input"] = json!([{"type":"tool_search_output","call_id":"search-call","status":"completed","tools":[image]}])
        }
        "shell_call" => {
            body["input"] = json!([{"type":"shell_call","id":"shell-call","call_id":"shell-call",
            "action":{"commands":["cat /mnt/data/input"]},"environment":shell["environment"]}])
        }
        _ => unreachable!(),
    }
    body
}

#[tokio::test]
async fn completed_upload_files_reach_only_documented_response_tool_slots_with_current_proof() {
    for slot in [
        "image",
        "code",
        "shell",
        "choice",
        "additional",
        "search",
        "shell_call",
    ] {
        let upstream = MockServer::builder().start().await;
        let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
        completed_file(router.clone(), &upstream).await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(inference_reply(true)))
            .expect(1)
            .mount(&upstream)
            .await;
        let body = response_tool_file_body(slot, "file_complete");
        let answer = json_request(
            router.clone(),
            "/v1/responses",
            Some(body.clone()),
            "caller",
            false,
        )
        .await;
        let status = answer.status();
        let result = value(answer).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "slot {slot}: {result}; upstream requests: {:?}",
            upstream.received_requests().await.unwrap()
        );
        assert!(result.to_string().contains("read the file"));
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let mut expected = body;
        expected["model"] = json!("provider-model");
        assert_eq!(requests[2].body_json::<Value>().unwrap(), expected);
        let answer = json_request(
            router.clone(),
            "/v1/responses",
            Some(response_tool_file_body(slot, "file_complete")),
            "other-caller",
            false,
        )
        .await;
        assert_eq!(answer.status(), StatusCode::NOT_FOUND, "slot {slot}");
        assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
    }
}

#[tokio::test]
async fn authorized_response_tool_files_do_not_authorize_a_container_or_skill() {
    for stored in ["container", "skill", "extension"] {
        let upstream = MockServer::builder().start().await;
        let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
        completed_file(router.clone(), &upstream).await;
        let mut body = response_tool_file_body("shell", "file_complete");
        match stored {
            "container" => {
                body["tools"][0]["environment"] =
                    json!({"type":"container_reference","container_id":"cntr_foreign"})
            }
            "skill" => {
                body["tools"][0]["environment"]["skills"] =
                    json!([{"type":"skill_reference","skill_id":"skill_foreign"}])
            }
            _ => {
                body["tools"][0]["extension"] = json!({"type":"image_generation","input_image_mask":{"file_id":"file_complete"}})
            }
        }
        let answer = json_request(router, "/v1/responses", Some(body), "caller", false).await;
        assert_eq!(answer.status(), StatusCode::BAD_REQUEST);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn upload_reply_rate_headers_report_only_the_callers_own_configured_window() {
    for limited in [false, true] {
        let upstream = MockServer::builder().start().await;
        let (router, _) =
            app_configured(&upstream.uri(), Arc::new(Registry::default()), |snapshot| {
                if limited {
                    let mut key = snapshot
                        .apikeys
                        .get_by_id(KEY)
                        .unwrap()
                        .value
                        .as_ref()
                        .clone();
                    key.rate_limit = Some(serde_json::from_value(json!({"rpm":10})).unwrap());
                    snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 2));
                }
            });
        Mock::given(method("POST"))
            .and(path("/v1/uploads"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(upload("upload_owned", "pending"))
                    .insert_header("x-ratelimit-limit-requests", "9999")
                    .insert_header("x-ratelimit-remaining-requests", "9876")
                    .insert_header("x-ratelimit-reset-requests", "1s")
                    .insert_header("x-ratelimit-account-sentinel", "shared-account"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let answer = json_request(router, "/v1/uploads", Some(create_body()), "caller", true).await;
        assert_eq!(answer.status(), StatusCode::OK);
        assert!(answer
            .headers()
            .get("x-ratelimit-account-sentinel")
            .is_none());
        if limited {
            assert_eq!(
                answer
                    .headers()
                    .get("x-ratelimit-limit-requests")
                    .and_then(|v| v.to_str().ok()),
                Some("10")
            );
            assert_eq!(
                answer
                    .headers()
                    .get("x-ratelimit-remaining-requests")
                    .and_then(|v| v.to_str().ok()),
                Some("9")
            );
            assert!(answer.headers().get("x-ratelimit-reset-requests").is_some());
        } else {
            assert!(answer
                .headers()
                .keys()
                .all(|name| !name.as_str().starts_with("x-ratelimit-")));
        }
        assert_eq!(value(answer).await, upload("upload_owned", "pending"));
    }
}

async fn usage_span(collector: &MockServer, request_id: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        for request in collector.received_requests().await.unwrap() {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            for resource in payload["resourceSpans"].as_array().into_iter().flatten() {
                for scope in resource["scopeSpans"].as_array().into_iter().flatten() {
                    for span in scope["spans"].as_array().into_iter().flatten() {
                        let attributes = span["attributes"].as_array().unwrap();
                        let own = attributes.iter().any(|attr| {
                            attr["key"] == "aisix.request_id"
                                && attr["value"]["stringValue"] == request_id
                        });
                        let operation = attributes
                            .iter()
                            .any(|attr| attr["key"] == "gen_ai.operation.name");
                        if own && operation {
                            return span.clone();
                        }
                    }
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "usage span absent for {request_id}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn span_string<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attr| attr["key"] == key)
        .and_then(|attr| attr["value"]["stringValue"].as_str())
}

#[tokio::test]
async fn uploads_failure_capture_keeps_screened_metadata_and_suppresses_failed_open_bodies() {
    for parts in [false, true] {
        for fail_open in [false, true] {
            let upstream = MockServer::builder().start().await;
            let collector = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&collector)
                .await;
            let (router, snapshot) = app_configured(
                &upstream.uri(),
                Arc::new(Registry::default()),
                |snapshot| {
                    let exporter: aisix_core::ObservabilityExporter = serde_json::from_value(json!({
                    "name":"upload-full-content", "enabled":true, "kind":"otlp_http",
                    "endpoint":format!("{}/v1/traces", collector.uri()), "headers":{}, "content_mode":"full"
                })).unwrap();
                    snapshot.observability_exporters.insert(ResourceEntry::new(
                        "upload-exporter",
                        exporter,
                        1,
                    ));
                },
            );
            create(router.clone(), &upstream, "upload_owned").await;
            if fail_open {
                let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
                    "name":"upload-unavailable", "enabled":true, "kind":"custom", "hook_point":"input",
                    "fail_open":true, "script":"export function checkInput() { throw new Error('unavailable'); }", "timeout_ms":5000
                })).unwrap();
                let attachment: aisix_core::models::GuardrailAttachment =
                    serde_json::from_value(json!({
                        "guardrail_id":"upload-unavailable", "scope_type":"env", "priority":100
                    }))
                    .unwrap();
                snapshot.load().guardrails.insert(ResourceEntry::new(
                    "upload-unavailable",
                    guardrail,
                    1,
                ));
                snapshot
                    .load()
                    .guardrail_attachments
                    .insert(ResourceEntry::new(
                        "upload-unavailable-attachment",
                        attachment,
                        1,
                    ));
            }
            let route = if parts {
                "/v1/uploads/upload_owned/parts"
            } else {
                "/v1/uploads/upload_owned/cancel"
            };
            Mock::given(method("POST")).and(path(route)).respond_with(ResponseTemplate::new(400)
                .set_body_json(json!({"error":{"type":"invalid_request_error","message":"Invalid request parameter","param":"future_option","code":"invalid_parameter"}})))
                .expect(1).mount(&upstream).await;
            let answer = if parts {
                part_request(router, "upload_owned", "caller").await
            } else {
                json_request(
                    router,
                    route,
                    Some(json!({"future_option":"keep-cancel-body"})),
                    "caller",
                    false,
                )
                .await
            };
            let request_id = answer
                .headers()
                .get("x-aisix-request-id")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            assert_eq!(answer.status(), StatusCode::BAD_REQUEST);
            value(answer).await;
            let span = usage_span(&collector, &request_id).await;
            let captured = span_string(&span, "aisix.content.prompt");
            if fail_open {
                assert!(captured.is_none_or(str::is_empty));
                assert!(!span.to_string().contains("keep-me"));
                assert!(!span.to_string().contains("keep-cancel-body"));
            } else {
                let captured: Value = serde_json::from_str(
                    captured.expect("screened failure retains full request capture"),
                )
                .unwrap();
                if parts {
                    assert_eq!(captured["future_option"], "keep-me");
                    let hash = captured["data_sha256"]
                        .as_str()
                        .expect("file bytes are represented by a digest");
                    assert_eq!(hash.len(), 64);
                    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
                    assert!(!captured.to_string().contains("part.bin"));
                } else {
                    assert_eq!(captured, json!({"future_option":"keep-cancel-body"}));
                }
            }
            assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn an_unknown_upload_cancel_never_polls_the_request_body() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = polls.clone();
    let mut sent = false;
    let body = futures::stream::poll_fn(move |_| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let next = if sent {
            None
        } else {
            sent = true;
            Some(Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                b"private-cancel-body",
            )))
        };
        std::task::Poll::Ready(next)
    });
    let answer = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/uploads/upload_missing/cancel")
                .header("authorization", "Bearer caller")
                .body(Body::from_stream(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!value(answer)
        .await
        .to_string()
        .contains("private-cancel-body"));
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn batch_derivation_keeps_upload_purpose_and_relays_the_native_purpose_rejection() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    completed_file(router.clone(), &upstream).await;
    Mock::given(method("GET"))
        .and(path("/v1/files/file_complete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(file()))
        .expect(1)
        .mount(&upstream)
        .await;
    let manifest = format!(
        "{}\n",
        json!({"custom_id":"not-a-batch-file","method":"POST","url":"/v1/chat/completions",
        "body":{"model":"upload-model","messages":[{"role":"user","content":"Classify this text"}]}})
    );
    Mock::given(method("GET"))
        .and(path("/v1/files/file_complete/content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(manifest))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut derived = file();
    derived["id"] = json!("file_user_data_derived");
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(derived))
        .expect(1)
        .mount(&upstream)
        .await;
    let error = json!({"error":{"type":"invalid_request_error","code":"invalid_file_purpose","param":"input_file_id",
        "message":"Files used for batch requests must have purpose 'batch'."}});
    Mock::given(method("POST"))
        .and(path("/v1/batches"))
        .respond_with(ResponseTemplate::new(400).set_body_json(&error))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = json_request(
        router,
        "/v1/batches",
        Some(json!({"input_file_id":"file_complete",
        "endpoint":"/v1/chat/completions","completion_window":"24h"})),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::BAD_REQUEST);
    assert_eq!(value(answer).await, error);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 6);
    let multipart = String::from_utf8(requests[4].body.clone()).unwrap();
    assert!(multipart.contains("name=\"purpose\"\r\n\r\nuser_data"));
    assert!(!multipart.contains("name=\"purpose\"\r\n\r\nbatch"));
    assert!(multipart.contains("\"model\":\"provider-model\""));
    assert_eq!(
        requests[5].body_json::<Value>().unwrap()["input_file_id"],
        "file_user_data_derived"
    );
    let records = registry.records.lock().unwrap();
    assert_eq!(
        records["file_user_data_derived"].1.parent_id,
        Some(records["file_complete"].1.record_id)
    );
    assert!(records
        .values()
        .all(|(_, record)| record.kind != Some(NativeObjectKind::MessageBatch)));
}

#[tokio::test]
async fn file_consumers_need_only_their_own_endpoint_grant_without_gaining_file_access() {
    for responses in [false, true] {
        let upstream = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (router, snapshot) = app(&upstream.uri(), registry.clone());
        completed_file(router.clone(), &upstream).await;
        let family = if responses {
            "responses"
        } else {
            "chat_completions"
        };
        for id in [KEY, OTHER_KEY] {
            let mut key = snapshot
                .load()
                .apikeys
                .get_by_id(id)
                .unwrap()
                .value
                .as_ref()
                .clone();
            key.allowed_endpoints = Some(serde_json::from_value(json!([family])).unwrap());
            snapshot
                .load()
                .apikeys
                .insert(ResourceEntry::new(id, key, 3));
        }
        registry.lookups.lock().unwrap().clear();
        let route = if responses {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(inference_reply(responses)))
            .expect(1)
            .mount(&upstream)
            .await;
        let body = inference_body(responses, "upload-model", "file_complete");
        let answer = json_request(router.clone(), route, Some(body.clone()), "caller", false).await;
        let status = answer.status();
        let result = value(answer).await;
        assert_eq!(status, StatusCode::OK, "consumer {family}: {result}");
        assert!(result.to_string().contains("read the file"));
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let mut expected = body.clone();
        expected["model"] = json!("provider-model");
        assert_eq!(requests[2].body_json::<Value>().unwrap(), expected);
        let lookups = registry.lookups.lock().unwrap().clone();
        let own_proof = lookups
            .iter()
            .find(|lookup| {
                lookup.api_key_id == Uuid::parse_str(KEY).unwrap()
                    && lookup.locator.raw_id == "file_complete"
            })
            .expect("file consumer must prove the File with the authority");
        let own_proof = serde_json::to_value(own_proof).unwrap();
        assert_eq!(own_proof["consumer_family"], family);
        assert_eq!(own_proof["locator"]["kind"], "file");
        let proof_count = lookups.len();
        drop(lookups);
        let answer = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/files/file_complete")
                    .header("authorization", "Bearer caller")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::FORBIDDEN);
        assert_eq!(registry.lookups.lock().unwrap().len(), proof_count);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
        let answer = json_request(router, route, Some(body), "other-caller", false).await;
        assert_eq!(answer.status(), StatusCode::NOT_FOUND);
        assert!(!value(answer).await.to_string().contains("file_complete"));
        assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
        let lookups = registry.lookups.lock().unwrap();
        let foreign_proof = lookups
            .iter()
            .find(|lookup| {
                lookup.api_key_id == Uuid::parse_str(OTHER_KEY).unwrap()
                    && lookup.locator.raw_id == "file_complete"
            })
            .expect("foreign caller reaches only its own authority lookup");
        assert_eq!(
            serde_json::to_value(foreign_proof).unwrap()["consumer_family"],
            family
        );
    }
}
