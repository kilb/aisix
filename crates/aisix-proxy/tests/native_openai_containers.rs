//! 容器组件合同：真实路由与HTTP；受控上游和归属替身不代替提供商或控制面E2E。
use std::collections::{HashMap, HashSet};
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
use futures::future::BoxFuture;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "c1111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "c2222222-2222-4222-8222-222222222222";
const KEY: &str = "c3333333-3333-4333-8333-333333333333";
const OTHER: &str = "c4444444-4444-4444-8444-444444444444";

#[derive(Default)]
struct Registry {
    operations: Mutex<HashMap<Uuid, BeginRequest>>,
    records: Mutex<HashMap<String, NativeRecord>>,
    failures: Mutex<HashSet<String>>,
    reservations: Mutex<HashMap<Uuid, ReserveRelationsRequest>>,
    memberships: Mutex<Vec<NativeMembership>>,
    deletions: Mutex<HashMap<Uuid, ObjectDeletionRequest>>,
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
    fn reserve_delete(
        &self,
        request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let result = (|| {
            let records = self.records.lock().unwrap();
            let parent = records
                .values()
                .find(|record| {
                    record.record_id == request.record_id
                        && record.api_key_id == Some(request.api_key_id)
                        && record.version == request.version
                        && !record.deleted
                })
                .ok_or(NativeRegistryError::NotFound)?;
            if self
                .reservations
                .lock()
                .unwrap()
                .values()
                .any(|reservation| reservation.parent_record_id == parent.record_id)
            {
                return Err(NativeRegistryError::Conflict);
            }
            let mut reply = NativeReply::new("reserved");
            reply.operation_id = Some(request.operation_id);
            reply.record = Some(parent.clone());
            self.deletions
                .lock()
                .unwrap()
                .insert(request.operation_id, request);
            Ok(reply)
        })();
        Box::pin(async move { result })
    }
    fn commit_delete(
        &self,
        request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let result = (|| {
            let mut deletions = self.deletions.lock().unwrap();
            let deletion = deletions
                .get(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?;
            if deletion.record_id != request.record_id || deletion.api_key_id != request.api_key_id
            {
                return Err(NativeRegistryError::NotFound);
            }
            let mut records = self.records.lock().unwrap();
            let parent = records
                .values_mut()
                .find(|record| {
                    record.record_id == request.record_id && record.version == request.version
                })
                .ok_or(NativeRegistryError::Conflict)?;
            parent.deleted = true;
            self.memberships
                .lock()
                .unwrap()
                .retain(|membership| membership.parent_record_id != parent.record_id);
            deletions.remove(&request.operation_id);
            Ok(NativeReply::new("deleted"))
        })();
        Box::pin(async move { result })
    }
    fn begin_related(
        &self,
        request: BeginRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.begin(request.submission)
    }
    fn bind_related(
        &self,
        request: BindRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.bind(request.binding)
    }
    fn reserve_relations(
        &self,
        request: ReserveRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let result = (|| {
            let records = self.records.lock().unwrap();
            let parent = records
                .values()
                .find(|record| {
                    record.record_id == request.parent_record_id
                        && record.api_key_id == Some(request.api_key_id)
                        && !record.deleted
                })
                .ok_or(NativeRegistryError::NotFound)?;
            if parent.version != request.expected_parent_version {
                return Err(NativeRegistryError::Conflict);
            }
            let mut reservations = self.reservations.lock().unwrap();
            if reservations
                .values()
                .any(|pending| pending.parent_record_id == parent.record_id)
            {
                return Err(NativeRegistryError::Conflict);
            }
            reservations.insert(request.operation_id, request.clone());
            let mut reply = NativeReply::new("reserved");
            reply.operation_id = Some(request.operation_id);
            reply.record = Some(parent.clone());
            Ok(reply)
        })();
        Box::pin(async move { result })
    }
    fn commit_relations(
        &self,
        request: CommitRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let result = (|| {
            let mut reservations = self.reservations.lock().unwrap();
            let reservation = reservations
                .get(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?
                .clone();
            if reservation.api_key_id != request.api_key_id {
                return Err(NativeRegistryError::NotFound);
            }
            let mut records = self.records.lock().unwrap();
            let parent = records
                .values()
                .find(|record| record.record_id == reservation.parent_record_id && !record.deleted)
                .ok_or(NativeRegistryError::NotFound)?
                .clone();
            if parent.version != reservation.expected_parent_version {
                return Err(NativeRegistryError::Conflict);
            }
            if request
                .objects
                .iter()
                .any(|object| self.failures.lock().unwrap().contains(&object.raw_id))
            {
                return Err(NativeRegistryError::Unavailable);
            }
            let mut memberships = self.memberships.lock().unwrap();
            let mut reply = NativeReply::new("committed");
            reply.operation_id = Some(request.operation_id);
            for object in &request.objects {
                let child = if let Some(existing) = records.get(&object.raw_id) {
                    if existing.api_key_id != parent.api_key_id
                        || existing.origin != parent.origin
                        || existing.kind != Some(object.kind)
                    {
                        return Err(NativeRegistryError::Conflict);
                    }
                    existing.clone()
                } else {
                    let mut child = parent.clone();
                    child.record_id = Uuid::new_v4();
                    child.raw_id = object.raw_id.clone();
                    child.kind = Some(object.kind);
                    child.parent_id = Some(parent.record_id);
                    child.version = 1;
                    records.insert(child.raw_id.clone(), child.clone());
                    child
                };
                if !memberships.iter().any(|membership| {
                    membership.parent_record_id == parent.record_id
                        && membership.child_record_id == child.record_id
                        && membership.relation == reservation.relation
                }) {
                    memberships.push(NativeMembership {
                        membership_id: Uuid::new_v4(),
                        parent_record_id: parent.record_id,
                        child_record_id: child.record_id,
                        relation: reservation.relation,
                        generation: 1,
                        version: 1,
                    });
                }
                reply.records.push(child);
            }
            if reservation.action == RelationAction::Detach {
                memberships.retain(|membership| {
                    !(membership.parent_record_id == parent.record_id
                        && membership.relation == reservation.relation
                        && reservation
                            .child_record_ids
                            .contains(&membership.child_record_id))
                });
            }
            let mut updated = parent;
            updated.version += 1;
            records.insert(updated.raw_id.clone(), updated.clone());
            reply.memberships = memberships
                .iter()
                .filter(|membership| membership.parent_record_id == updated.record_id)
                .cloned()
                .collect();
            reply.record = Some(updated);
            reservations.remove(&request.operation_id);
            Ok(reply)
        })();
        Box::pin(async move { result })
    }
    fn lookup_relations(
        &self,
        request: LookupRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let result = (|| {
            let records = self.records.lock().unwrap();
            let parent = records
                .values()
                .find(|record| {
                    record.record_id == request.parent_record_id
                        && record.api_key_id == Some(request.api_key_id)
                        && !record.deleted
                })
                .ok_or(NativeRegistryError::NotFound)?;
            let mut reply = NativeReply::new("related");
            reply.record = Some(parent.clone());
            for membership in self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .filter(|membership| {
                    membership.parent_record_id == parent.record_id
                        && membership.relation == request.relation
                })
            {
                if let Some(child) = records.values().find(|record| {
                    record.record_id == membership.child_record_id
                        && !record.deleted
                        && request.raw_ids.contains(&record.raw_id)
                }) {
                    reply.records.push(child.clone());
                    reply.memberships.push(membership.clone());
                }
            }
            Ok(reply)
        })();
        Box::pin(async move { result })
    }
    fn resolve_reservation(
        &self,
        request: ResolveReservationRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        if request.upstream_rejected {
            self.reservations
                .lock()
                .unwrap()
                .remove(&request.operation_id);
            self.deletions.lock().unwrap().remove(&request.operation_id);
        }
        Box::pin(async { Ok(NativeReply::new("resolved")) })
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
        let result = (|| {
            if self.failures.lock().unwrap().contains(&request.raw_id) {
                return Err(NativeRegistryError::Unavailable);
            }
            let operations = self.operations.lock().unwrap();
            let op = operations
                .get(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?;
            let mut records = self.records.lock().unwrap();
            if let Some(existing) = records.get(&request.raw_id) {
                if existing.api_key_id != Some(request.api_key_id)
                    || existing.origin.as_ref() != Some(&op.origin)
                {
                    return Err(NativeRegistryError::Conflict);
                }
            }
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id":request.existing_record_id.unwrap_or_else(Uuid::new_v4),
                "operation_id":op.operation_id,"api_key_id":op.api_key_id,
                "requested_model_id":op.requested_model_id,"requested_model":op.requested_model,
                "upstream_model":op.upstream_model,"origin":op.origin,
                "protocol":op.protocol,"kind":op.kind,"raw_id":request.raw_id,
                "parent_id":op.parent_id,"version":1
            }))
            .unwrap();
            records.insert(record.raw_id.clone(), record.clone());
            let mut reply = NativeReply::new("bound");
            reply.record = Some(record);
            Ok(reply)
        })();
        Box::pin(async move { result })
    }
    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let result = self
            .records
            .lock()
            .unwrap()
            .get(&request.locator.raw_id)
            .filter(|record| {
                record.api_key_id == Some(request.api_key_id)
                    && !record.deleted
                    && record.protocol == Some(request.locator.protocol)
                    && record.kind == Some(request.locator.kind)
                    && request
                        .locator
                        .parent_id
                        .is_none_or(|parent| record.parent_id == Some(parent))
                    && request
                        .locator
                        .origin
                        .as_ref()
                        .is_none_or(|origin| record.origin.as_ref() == Some(origin))
            })
            .cloned()
            .ok_or(NativeRegistryError::NotFound)
            .map(|record| {
                let mut reply = NativeReply::new("found");
                reply.record = Some(record);
                reply
            });
        Box::pin(async move { result })
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
            .filter(|record| {
                record.api_key_id == Some(request.api_key_id)
                    && !record.deleted
                    && record.protocol == Some(request.protocol)
                    && record.kind == Some(request.kind)
                    && record.origin.as_ref() == Some(&request.origin)
                    && request
                        .parent_id
                        .is_none_or(|parent| record.parent_id == Some(parent))
            })
            .cloned()
            .collect();
        Box::pin(async move { Ok(reply) })
    }
    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut records = self.records.lock().unwrap();
        let result = records
            .values_mut()
            .find(|record| {
                record.record_id == request.record_id
                    && record.api_key_id == Some(request.api_key_id)
                    && record.version == request.version
            })
            .ok_or(NativeRegistryError::NotFound)
            .map(|record| {
                record.deleted = true;
                NativeReply::new("deleted")
            });
        Box::pin(async move { result })
    }
}

fn app(
    base: &str,
    registry: Arc<Registry>,
) -> (
    axum::Router,
    SnapshotHandle<AisixSnapshot>,
    tokio::sync::mpsc::Receiver<UsageEvent>,
) {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({"display_name":"container-account",
        "secret":"provider-secret","provider":"openai","api_base":format!("{base}/v1"),"adapter":"openai"})).unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model =
        serde_json::from_value(json!({"display_name":"container-model","provider":"openai",
        "model_name":"provider-model","provider_key_id":PROVIDER}))
        .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    for (id, bearer) in [(KEY, "caller"), (OTHER, "other")] {
        let key: ApiKey = serde_json::from_value(json!({"key_hash":ApiKey::hash_bearer(bearer),
            "allowed_models":["container-model"]}))
        .unwrap();
        snapshot.apikeys.insert(ResourceEntry::new(id, key, 1));
    }
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .unwrap();
    let snapshot = SnapshotHandle::new(snapshot);
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let (tx, rx) = tokio::sync::mpsc::channel(128);
    let state = ProxyState::new(snapshot.clone(), hub, &config)
        .without_cache()
        .with_guardrail_index(guardrails)
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "native-container-test",
        ))
        .with_native_objects(registry)
        .with_usage_sink(UsageSink::new(tx));
    (build_router(state), snapshot, rx)
}

fn container(id: &str) -> Value {
    json!({"id":id,"object":"container","name":"report workspace","created_at":1730000000,
        "status":"running","expires_after":{"anchor":"last_active_at","minutes":20},
        "last_active_at":1730000001,"memory_limit":"1g","future":{"kept":true}})
}
fn file(id: &str, parent: &str) -> Value {
    json!({"id":id,"object":"container.file","container_id":parent,"bytes":4,
        "created_at":1730000001,"path":"/mnt/data/output.bin","source":"assistant","future":17})
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
async fn multipart(router: axum::Router, route: &str, bearer: &str) -> axum::response::Response {
    let mut body=b"--container-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"input.bin\"\r\nContent-Type: application/octet-stream\r\nX-Future-Part: keep\r\n\r\n".to_vec();
    body.extend_from_slice(&[255, 0, 1, 2]);
    body.extend_from_slice(b"\r\n--container-test\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nuser_data\r\n--container-test--\r\n");
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(route)
                .header("authorization", format!("Bearer {bearer}"))
                .header(
                    "content-type",
                    "multipart/form-data; boundary=container-test",
                )
                .header("x-aisix-model", "container-model")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn value(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"invalid_response":String::from_utf8_lossy(&bytes)}))
}
async fn create(router: axum::Router, upstream: &MockServer, id: &str, caller: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/containers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(container(id)))
        .expect(1)
        .mount(upstream)
        .await;
    let response = request(
        router,
        "POST",
        "/v1/containers?model=container-model",
        caller,
        Some(json!({"name":"report workspace"})),
    )
    .await;
    let status = response.status();
    let body = value(response).await;
    assert_eq!(status, StatusCode::OK, "create: {body}");
    assert_eq!(body, container(id));
}

// 删掉任一个9路由、包装ID、丢字段或JSON化二进制，都会使这个旅程失败。
#[tokio::test]
async fn containers_all_nine_routes_preserve_raw_ids_json_binary_and_empty_deletes() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, _, mut usage) = app(&upstream.uri(), registry.clone());
    create(router.clone(), &upstream, "cntr_owned", "caller").await;
    for (verb, route, body) in [
        ("GET", "/v1/containers/cntr_owned", container("cntr_owned")),
        (
            "GET",
            "/v1/containers",
            json!({"object":"list","data":[container("cntr_owned")],"first_id":"cntr_owned","last_id":"cntr_owned","has_more":false}),
        ),
        (
            "POST",
            "/v1/containers/cntr_owned/files",
            file("cfile_created", "cntr_owned"),
        ),
        (
            "GET",
            "/v1/containers/cntr_owned/files",
            json!({"object":"list","data":[file("cfile_generated","cntr_owned")],"first_id":"cfile_generated","last_id":"cfile_generated","has_more":false}),
        ),
        (
            "GET",
            "/v1/containers/cntr_owned/files/cfile_generated",
            file("cfile_generated", "cntr_owned"),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&upstream)
            .await;
        let route = if route == "/v1/containers" {
            format!("{route}?model=container-model")
        } else {
            route.to_owned()
        };
        let response = if verb == "POST" {
            multipart(router.clone(), &route, "caller").await
        } else {
            request(router.clone(), verb, &route, "caller", None).await
        };
        let status = response.status();
        let answer = value(response).await;
        assert_eq!(status, StatusCode::OK, "{verb} {route}: {answer}");
        assert_eq!(answer, body, "{verb} {route}");
    }
    Mock::given(method("GET"))
        .and(path(
            "/v1/containers/cntr_owned/files/cfile_generated/content",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(vec![255, 0, 1, 2]),
        )
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "GET",
        "/v1/containers/cntr_owned/files/cfile_generated/content",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
        &[255, 0, 1, 2]
    );
    for route in [
        "/v1/containers/cntr_owned/files/cfile_generated",
        "/v1/containers/cntr_owned",
    ] {
        Mock::given(method("DELETE"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&upstream)
            .await;
        let response = request(router.clone(), "DELETE", route, "caller", None).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }
    let records = registry.records.lock().unwrap();
    assert!(records["cntr_owned"].deleted);
    drop(records);
    let mut events = Vec::new();
    while let Ok(event) = usage.try_recv() {
        events.push(event);
    }
    assert!(!events.is_empty(), "管理调用必须有遥测");
    assert!(events.iter().all(
        |event| event.inference_stage == Some(InferenceStage::Management)
            && event.prompt_tokens == 0
            && event.completion_tokens == 0
    ));
    let sent = upstream.received_requests().await.unwrap();
    let uploaded = sent
        .iter()
        .find(|request| request.method.as_str() == "POST" && request.url.path().ends_with("/files"))
        .unwrap();
    assert!(uploaded
        .body
        .windows(4)
        .any(|bytes| bytes == [255, 0, 1, 2]));
    assert!(String::from_utf8_lossy(&uploaded.body)
        .to_ascii_lowercase()
        .contains("x-future-part: keep"));
}

// 移除未知ID/owner/当前模型或account检查，应在禁止的上游调用计数处失败。
#[tokio::test]
async fn containers_unknown_foreign_and_revoked_objects_never_probe_provider() {
    for scenario in ["unknown", "foreign", "revoke", "account"] {
        let upstream = MockServer::start().await;
        let (router, snapshot, _) = app(&upstream.uri(), Arc::new(Registry::default()));
        if scenario != "unknown" {
            create(router.clone(), &upstream, "cntr_owned", "caller").await;
        }
        if scenario == "revoke" {
            let mut key = snapshot
                .load()
                .apikeys
                .get_by_id(KEY)
                .unwrap()
                .value
                .as_ref()
                .clone();
            key.allowed_models = vec!["unrelated".into()];
            snapshot
                .load()
                .apikeys
                .insert(ResourceEntry::new(KEY, key, 2));
        }
        if scenario == "account" {
            let mut provider = snapshot
                .load()
                .provider_keys
                .get_by_id(PROVIDER)
                .unwrap()
                .value
                .as_ref()
                .clone();
            provider.api_key = "changed-account".into();
            snapshot
                .load()
                .provider_keys
                .insert(ResourceEntry::new(PROVIDER, provider, 2));
        }
        let before = upstream.received_requests().await.unwrap().len();
        for (verb, route) in [
            ("GET", "/v1/containers/cntr_owned"),
            ("DELETE", "/v1/containers/cntr_owned"),
            ("GET", "/v1/containers/cntr_owned/files"),
            ("POST", "/v1/containers/cntr_owned/files"),
        ] {
            let response = request(
                router.clone(),
                verb,
                route,
                if scenario == "foreign" {
                    "other"
                } else {
                    "caller"
                },
                (verb == "POST").then(|| json!({"file_id":"file_other"})),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{scenario} {verb} {route}"
            );
        }
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            before,
            "{scenario}"
        );
    }
}

// 过滤data却泄露foreign游标、遇整页foreign即结束、或把unknown认领为自己，都会失败。
#[tokio::test]
async fn containers_owned_pagination_crosses_foreign_pages_and_keeps_visible_cursors() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, _, _) = app(&upstream.uri(), registry.clone());
    create(router.clone(), &upstream, "cntr_a", "caller").await;
    create(router.clone(), &upstream, "cntr_b", "caller").await;
    let pages = [
        (
            None,
            vec![container("cntr_foreign_1")],
            "cntr_foreign_1",
            true,
        ),
        (
            Some("cntr_foreign_1"),
            vec![container("cntr_foreign_2")],
            "cntr_foreign_2",
            true,
        ),
        (
            Some("cntr_foreign_2"),
            vec![container("cntr_a"), container("cntr_b")],
            "cntr_b",
            false,
        ),
        (Some("cntr_a"), vec![container("cntr_b")], "cntr_b", false),
    ];
    for (after, data, last, more) in pages {
        let mock = Mock::given(method("GET")).and(path("/v1/containers"));
        let mock = if let Some(after) = after {
            mock.and(query_param("after", after))
        } else {
            mock.and(|request: &wiremock::Request| {
                !request.url.query_pairs().any(|(name, _)| name == "after")
            })
        };
        mock.respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"object":"list","data":data,"first_id":last,"last_id":last,"has_more":more}),
        ))
        .mount(&upstream)
        .await;
    }
    let response = request(
        router.clone(),
        "GET",
        "/v1/containers?model=container-model&limit=1&name=report%20workspace",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = value(response).await;
    assert_eq!(body["data"], json!([container("cntr_a")]));
    assert_eq!(body["first_id"], "cntr_a");
    assert_eq!(body["last_id"], "cntr_a");
    assert_eq!(body["has_more"], true);
    let response = request(
        router,
        "GET",
        "/v1/containers?model=container-model&limit=1&after=cntr_a",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = value(response).await;
    assert_eq!(body["data"], json!([container("cntr_b")]));
    assert_eq!(body["has_more"], false);
    let records = registry.records.lock().unwrap();
    assert!(!records.contains_key("cntr_foreign_1"));
    assert!(!records.contains_key("cntr_foreign_2"));
}

#[tokio::test]
async fn containers_creation_binding_failure_hides_id_and_does_not_retry() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    registry
        .failures
        .lock()
        .unwrap()
        .insert("cntr_unbound".into());
    let (router, _, _) = app(&upstream.uri(), registry);
    Mock::given(method("POST"))
        .and(path("/v1/containers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(container("cntr_unbound")))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router,
        "POST",
        "/v1/containers?model=container-model",
        "caller",
        Some(json!({"name":"report workspace"})),
    )
    .await;
    assert!(!response.status().is_success());
    assert!(!value(response).await.to_string().contains("cntr_unbound"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

// 只查第一个引用、跳过跨调用者文件或误要求Files管理权限，都会失败。
#[tokio::test]
async fn containers_json_file_references_require_current_owner_and_only_consumer_permission() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, snapshot, _) = app(&upstream.uri(), registry);
    create(router.clone(), &upstream, "cntr_owned", "caller").await;
    for (id, caller) in [("file_source", "caller"), ("file_foreign", "other")] {
        Mock::given(method("POST")).and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":id,"object":"file","bytes":4,
                "created_at":1730000001,"filename":"input.bin","purpose":"user_data","status":"processed"})))
            .up_to_n_times(1).mount(&upstream).await;
        let response = multipart(router.clone(), "/v1/files", caller).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            value(response).await
        );
    }
    let mut key = snapshot
        .load()
        .apikeys
        .get_by_id(KEY)
        .unwrap()
        .value
        .as_ref()
        .clone();
    key.allowed_endpoints = Some(vec![aisix_core::models::EndpointFamily::Containers]);
    snapshot
        .load()
        .apikeys
        .insert(ResourceEntry::new(KEY, key, 2));
    Mock::given(method("POST"))
        .and(path("/v1/containers/cntr_owned/files"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(file("cfile_from_source", "cntr_owned")),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let payload = json!({"file_id":"file_source","future_option":{"retained":true}});
    let response = request(
        router.clone(),
        "POST",
        "/v1/containers/cntr_owned/files",
        "caller",
        Some(payload.clone()),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        value(response).await
    );
    let sent = upstream.received_requests().await.unwrap();
    assert_eq!(sent.last().unwrap().body_json::<Value>().unwrap(), payload);
    let before = sent.len();
    for payload in [
        json!({"file_id":"file_foreign"}),
        json!({"file_id":"file_missing"}),
    ] {
        let response = request(
            router.clone(),
            "POST",
            "/v1/containers/cntr_owned/files",
            "caller",
            Some(payload),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let response = request(
        router,
        "POST",
        "/v1/containers?model=container-model",
        "caller",
        Some(json!({"name":"unsafe","file_ids":["file_source","file_foreign"]})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn containers_wrong_parent_payload_is_not_discovered_and_delete_failure_keeps_owner() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, _, _) = app(&upstream.uri(), registry.clone());
    create(router.clone(), &upstream, "cntr_owned", "caller").await;
    Mock::given(method("GET"))
        .and(path("/v1/containers/cntr_owned/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"object":"list","data":[file("cfile_wrong","cntr_foreign")],"has_more":false}),
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "GET",
        "/v1/containers/cntr_owned/files",
        "caller",
        None,
    )
    .await;
    assert!(!response.status().is_success());
    assert!(!value(response).await.to_string().contains("cfile_wrong"));
    assert!(!registry.records.lock().unwrap().contains_key("cfile_wrong"));
    Mock::given(method("DELETE"))
        .and(path("/v1/containers/cntr_owned"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"error":{"message":"cannot delete now","type":"invalid_request_error"}}),
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router,
        "DELETE",
        "/v1/containers/cntr_owned",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!registry.records.lock().unwrap()["cntr_owned"].deleted);
}
