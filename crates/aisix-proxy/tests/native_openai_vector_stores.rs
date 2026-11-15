//! Vector Stores 的真实路由 HTTP 契约；归属替身不代替真实控制面验收。
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
    related_submissions: Mutex<HashMap<Uuid, BeginRelatedRequest>>,
    memberships: Mutex<Vec<NativeMembership>>,
    reservations: Mutex<HashMap<Uuid, ReserveRelationsRequest>>,
    detached: Mutex<HashSet<(Uuid, Uuid)>>,
    deletion_reservations: Mutex<HashMap<Uuid, ObjectDeletionRequest>>,
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
        Box::pin(async move {
            if !self
                .records
                .lock()
                .unwrap()
                .values()
                .any(|(owner, parent)| {
                    *owner == request.api_key_id
                        && parent.record_id == request.record_id
                        && parent.version == request.version
                        && !parent.deleted
                })
            {
                return Err(NativeRegistryError::NotFound);
            }
            if self
                .reservations
                .lock()
                .unwrap()
                .values()
                .any(|reservation| reservation.parent_record_id == request.record_id)
            {
                return Err(NativeRegistryError::Conflict);
            }
            let mut reply = NativeReply::new("reserved");
            reply.operation_id = Some(request.operation_id);
            self.deletion_reservations
                .lock()
                .unwrap()
                .insert(request.operation_id, request);
            Ok(reply)
        })
    }
    fn commit_delete(
        &self,
        request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let reserved = self
                .deletion_reservations
                .lock()
                .unwrap()
                .remove(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?;
            if reserved.record_id != request.record_id || reserved.api_key_id != request.api_key_id
            {
                return Err(NativeRegistryError::NotFound);
            }
            self.tombstone(TombstoneRequest {
                api_key_id: request.api_key_id,
                record_id: request.record_id,
                version: request.version,
            })
            .await
        })
    }
    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let mut records = self.records.lock().unwrap();
            let (_, record) = records
                .values_mut()
                .find(|(owner, record)| {
                    *owner == request.api_key_id
                        && record.record_id == request.record_id
                        && !record.deleted
                        && record.version == request.version
                })
                .ok_or(NativeRegistryError::NotFound)?;
            record.deleted = true;
            self.memberships
                .lock()
                .unwrap()
                .retain(|membership| membership.parent_record_id != request.record_id);
            Ok(NativeReply::new("deleted"))
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
            }
            self.related_submissions
                .lock()
                .unwrap()
                .insert(request.submission.operation_id, request.clone());
            self.begin(request.submission).await
        })
    }
    fn bind_related(
        &self,
        request: BindRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let accepted = self
                .related_submissions
                .lock()
                .unwrap()
                .get(&request.binding.operation_id)
                .cloned()
                .ok_or(NativeRegistryError::NotFound)?;
            let reply = self.bind(request.binding).await?;
            let root = reply.record.as_ref().unwrap();
            let records = self.records.lock().unwrap();
            let selector = |selector: &ArtifactSelector| -> Result<Uuid, NativeRegistryError> {
                match selector {
                    ArtifactSelector::Root => Ok(root.record_id),
                    ArtifactSelector::Record { record_id }
                        if accepted
                            .references
                            .iter()
                            .any(|r| r.record_id == *record_id) =>
                    {
                        Ok(*record_id)
                    }
                    ArtifactSelector::Observed { raw_id, .. } => records
                        .get(raw_id)
                        .map(|(_, r)| r.record_id)
                        .ok_or(NativeRegistryError::NotFound),
                    _ => Err(NativeRegistryError::NotFound),
                }
            };
            let mut memberships = self.memberships.lock().unwrap();
            for relation in request.relations {
                let parent = selector(&relation.parent)?;
                let child = selector(&relation.child)?;
                if !memberships.iter().any(|m| {
                    m.parent_record_id == parent
                        && m.child_record_id == child
                        && m.relation == relation.relation
                }) {
                    memberships.push(NativeMembership {
                        membership_id: Uuid::new_v4(),
                        parent_record_id: parent,
                        child_record_id: child,
                        relation: relation.relation,
                        generation: 1,
                        version: 1,
                    });
                }
            }
            Ok(reply)
        })
    }
    fn reserve_relations(
        &self,
        request: ReserveRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let records = self.records.lock().unwrap();
            if !records.values().any(|(owner, parent)| {
                *owner == request.api_key_id
                    && parent.record_id == request.parent_record_id
                    && !parent.deleted
                    && parent.version == request.expected_parent_version
            }) {
                return Err(NativeRegistryError::NotFound);
            }
            for id in &request.child_record_ids {
                if !records.values().any(|(owner, child)| {
                    *owner == request.api_key_id && child.record_id == *id && !child.deleted
                }) {
                    return Err(NativeRegistryError::NotFound);
                }
            }
            let mut reservations = self.reservations.lock().unwrap();
            if reservations
                .values()
                .any(|r| r.parent_record_id == request.parent_record_id)
            {
                return Err(NativeRegistryError::Conflict);
            }
            let mut reply = NativeReply::new("reserved");
            reply.operation_id = Some(request.operation_id);
            reservations.insert(request.operation_id, request);
            Ok(reply)
        })
    }
    fn commit_relations(
        &self,
        request: CommitRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let reservation = self
                .reservations
                .lock()
                .unwrap()
                .remove(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?;
            if reservation.api_key_id != request.api_key_id {
                return Err(NativeRegistryError::NotFound);
            }
            let mut memberships = self.memberships.lock().unwrap();
            for child in reservation.child_record_ids {
                if reservation.action == RelationAction::Detach {
                    memberships.retain(|m| {
                        !(m.parent_record_id == reservation.parent_record_id
                            && m.child_record_id == child
                            && m.relation == reservation.relation)
                    });
                    self.detached
                        .lock()
                        .unwrap()
                        .insert((reservation.parent_record_id, child));
                } else if !memberships.iter().any(|m| {
                    m.parent_record_id == reservation.parent_record_id
                        && m.child_record_id == child
                        && m.relation == reservation.relation
                }) {
                    memberships.push(NativeMembership {
                        membership_id: Uuid::new_v4(),
                        parent_record_id: reservation.parent_record_id,
                        child_record_id: child,
                        relation: reservation.relation,
                        generation: 1,
                        version: 1,
                    });
                }
            }
            for (_, parent) in self.records.lock().unwrap().values_mut() {
                if parent.record_id == reservation.parent_record_id {
                    parent.version += 1;
                }
            }
            Ok(NativeReply::new("committed"))
        })
    }
    fn resolve_reservation(
        &self,
        request: ResolveReservationRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            if !request.upstream_rejected {
                return Ok(NativeReply::new("uncertain"));
            }
            let relation = self
                .reservations
                .lock()
                .unwrap()
                .remove(&request.operation_id);
            let deletion = self
                .deletion_reservations
                .lock()
                .unwrap()
                .remove(&request.operation_id);
            if relation.is_none() && deletion.is_none() {
                return Err(NativeRegistryError::NotFound);
            }
            Ok(NativeReply::new("rejected"))
        })
    }
    fn lookup_relations(
        &self,
        request: LookupRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let records = self.records.lock().unwrap();
            if !records.values().any(|(owner, parent)| {
                *owner == request.api_key_id
                    && parent.record_id == request.parent_record_id
                    && !parent.deleted
            }) {
                return Err(NativeRegistryError::NotFound);
            }
            let mut reply = NativeReply::new("related");
            let memberships = self.memberships.lock().unwrap();
            for membership in memberships.iter().filter(|m| {
                m.parent_record_id == request.parent_record_id && m.relation == request.relation
            }) {
                if let Some((_, record)) = records.values().find(|(owner, r)| {
                    *owner == request.api_key_id
                        && !r.deleted
                        && r.record_id == membership.child_record_id
                        && request.raw_ids.contains(&r.raw_id)
                }) {
                    reply.records.push(record.clone());
                    reply.memberships.push(membership.clone());
                }
            }
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
            } else if let Some((owner, record)) = records.get(&request.raw_id) {
                if *owner != request.api_key_id
                    || record.operation_id != Some(request.operation_id)
                {
                    return Err(NativeRegistryError::Conflict);
                }
                record.record_id
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
        "display_name":"vector-account", "secret":"upstream-key", "provider":"openai",
        "api_base":format!("{}/v1",base.trim_end_matches('/').trim_end_matches("/v1")), "adapter":"openai"
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"vector-model", "provider":"openai", "model_name":"provider-model",
        "provider_key_id":PROVIDER
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    for (id, caller) in [(KEY, "caller"), (OTHER_KEY, "other-caller")] {
        let key: ApiKey = serde_json::from_value(json!({
            "key_hash":ApiKey::hash_bearer(caller), "allowed_models":["vector-model"]
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
            "native-vector-test",
        ))
        .with_native_objects(registry);
    (build_router(state), snapshot)
}
fn vector_store(id: &str) -> Value {
    json!({"id":id,"object":"vector_store","created_at":1730000000i64,
        "name":"Knowledge","status":"completed","usage_bytes":54,
        "file_counts":{"in_progress":0,"completed":1,"failed":0,"cancelled":0,"total":1},
        "metadata":{"topic":"support"},"future":{"retained":true}})
}
async fn request(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    caller: &str,
    hint: bool,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {caller}"));
    if hint {
        request = request.header("x-aisix-model", "vector-model");
    }
    let body = match body {
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

#[tokio::test]
async fn vector_create_requires_a_gateway_selector() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let answer = request(
        router,
        "POST",
        "/v1/vector_stores",
        Some(json!({"name":"Knowledge"})),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::BAD_REQUEST);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

fn block_vector_input(snapshot: &AisixSnapshot) {
    let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
        "name":"block-all-vector-input","enabled":true,"kind":"custom",
        "hook_point":"input","fail_open":false,"timeout_ms":5000,
        "script":"export function checkInput() { return { action: 'block', reason_code: 'blocked' }; }"
    })).unwrap();
    snapshot
        .guardrails
        .insert(ResourceEntry::new("vector-input-block", guardrail, 1));
    let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
        "guardrail_id":"vector-input-block","scope_type":"env","priority":100
    }))
    .unwrap();
    snapshot.guardrail_attachments.insert(ResourceEntry::new(
        "vector-input-block-attachment",
        attachment,
        1,
    ));
}

// 删除任一个输入链调用，必须在相应403或上游触达计数断言处变红。
#[tokio::test]
async fn unconditional_input_guardrail_blocks_all_sixteen_vector_store_operations() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_guardrail", json!({})).await;
    let upload = b"--vector-input\r\nContent-Disposition: form-data; name=\"file\"; filename=\"input.txt\"\r\nContent-Type: text/plain\r\n\r\ndata\r\n--vector-input\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n--vector-input--\r\n";
    for id in ["file_guardrail", "file_batch_guardrail"] {
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":id,"object":"file","bytes":4,"created_at":1730000000i64,
                "filename":"input.txt","purpose":"assistants","status":"processed"
            })))
            .expect(1)
            .up_to_n_times(1)
            .mount(&upstream)
            .await;
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/files?model=vector-model")
                    .header("authorization", "Bearer caller")
                    .header("x-aisix-model", "vector-model")
                    .header("content-type", "multipart/form-data; boundary=vector-input")
                    .body(Body::from(upload.as_slice()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let answer = value(response).await;
        assert_eq!(status, StatusCode::OK, "bootstrap source upload: {answer}");
        assert_eq!(answer["id"], id);
    }
    attach(router.clone(), &upstream, "vs_guardrail", "file_guardrail").await;
    Mock::given(method("POST"))
        .and(path("/v1/vector_stores/vs_guardrail/file_batches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(file_batch(
            "vsfb_guardrail",
            "vs_guardrail",
            "in_progress",
        )))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router,
        "POST",
        "/v1/vector_stores/vs_guardrail/file_batches",
        Some(json!({"file_ids":["file_batch_guardrail"]})),
        "caller",
        false,
    )
    .await;
    let status = response.status();
    let answer = value(response).await;
    assert_eq!(status, StatusCode::OK, "bootstrap ingestion: {answer}");
    assert_eq!(answer["id"], "vsfb_guardrail");
    let accepted_calls = upstream.received_requests().await.unwrap().len();
    assert_eq!(accepted_calls, 5);
    let (router, _) = app_configured(&upstream.uri(), registry, block_vector_input);
    for (verb, route, body, hint) in [
        ("POST", "/v1/vector_stores", Some(json!({})), true),
        ("GET", "/v1/vector_stores", None, true),
        ("GET", "/v1/vector_stores/vs_guardrail", None, false),
        (
            "POST",
            "/v1/vector_stores/vs_guardrail",
            Some(json!({"metadata":{}})),
            false,
        ),
        ("DELETE", "/v1/vector_stores/vs_guardrail", None, false),
        (
            "POST",
            "/v1/vector_stores/vs_guardrail/search",
            Some(json!({"query":""})),
            false,
        ),
        (
            "POST",
            "/v1/vector_stores/vs_guardrail/files",
            Some(json!({"file_id":"file_guardrail"})),
            false,
        ),
        ("GET", "/v1/vector_stores/vs_guardrail/files", None, false),
        (
            "GET",
            "/v1/vector_stores/vs_guardrail/files/file_guardrail",
            None,
            false,
        ),
        (
            "POST",
            "/v1/vector_stores/vs_guardrail/files/file_guardrail",
            Some(json!({"attributes":{}})),
            false,
        ),
        (
            "DELETE",
            "/v1/vector_stores/vs_guardrail/files/file_guardrail",
            None,
            false,
        ),
        (
            "GET",
            "/v1/vector_stores/vs_guardrail/files/file_guardrail/content",
            None,
            false,
        ),
        (
            "POST",
            "/v1/vector_stores/vs_guardrail/file_batches",
            Some(json!({"file_ids":["file_batch_guardrail"]})),
            false,
        ),
        (
            "GET",
            "/v1/vector_stores/vs_guardrail/file_batches/vsfb_guardrail",
            None,
            false,
        ),
        (
            "POST",
            "/v1/vector_stores/vs_guardrail/file_batches/vsfb_guardrail/cancel",
            None,
            false,
        ),
        (
            "GET",
            "/v1/vector_stores/vs_guardrail/file_batches/vsfb_guardrail/files",
            None,
            false,
        ),
    ] {
        let response = request(router.clone(), verb, route, body, "caller", hint).await;
        let status = response.status();
        let answer = value(response).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{verb} {route}: {answer}");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            accepted_calls,
            "blocked {verb} {route} reached the provider"
        );
    }
}

#[tokio::test]
async fn vector_create_persists_raw_id_and_preserves_official_wire() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/vector_stores"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vector_store("vs_owned")))
        .expect(1)
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    let body = json!({"name":"Knowledge","expires_after":{"anchor":"last_active_at","days":7},"metadata":{"topic":"support"},"future_parameter":true});
    let answer = request(
        router,
        "POST",
        "/v1/vector_stores?model=vector-model&x=a%2Fb&x=two",
        Some(body.clone()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, vector_store("vs_owned"));
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body_json::<Value>().unwrap(), body);
    assert_eq!(requests[0].url.query(), Some("x=a%2Fb&x=two"));
    assert_eq!(requests[0].headers["authorization"], "Bearer upstream-key");
    let records = registry.records.lock().unwrap();
    let record = &records["vs_owned"].1;
    assert_eq!(
        serde_json::to_value(record.kind).unwrap(),
        json!("vector_store")
    );
    assert!(record.parent_id.is_none());
}

async fn create_store(router: axum::Router, upstream: &MockServer, id: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/vector_stores"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vector_store(id)))
        .expect(1)
        .up_to_n_times(1)
        .mount(upstream)
        .await;
    let answer = request(
        router,
        "POST",
        "/v1/vector_stores",
        Some(body),
        "caller",
        true,
    )
    .await;
    let status = answer.status();
    let body = value(answer).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], id);
}
fn vector_file(id: &str, store: &str) -> Value {
    json!({"id":id,"object":"vector_store.file","created_at":1730000001i64,"vector_store_id":store,
        "status":"completed","usage_bytes":54,"attributes":{"language":"en"},"last_error":null,
        "chunking_strategy":{"type":"static","static":{"max_chunk_size_tokens":800,"chunk_overlap_tokens":400}},"future_file":"keep"})
}
fn file_batch(id: &str, store: &str, status: &str) -> Value {
    json!({"id":id,"object":"vector_store.files_batch","created_at":1730000002i64,"vector_store_id":store,"status":status,
        "file_counts":{"in_progress":1,"completed":0,"failed":0,"cancelled":0,"total":1},"future_batch":7})
}
fn page(data: Vec<Value>, more: bool) -> Value {
    json!({"object":"list","first_id":data.first().and_then(|v|v.get("id")),"last_id":data.last().and_then(|v|v.get("id")),"data":data,"has_more":more,"future_page":"keep"})
}
async fn seed_file(registry: &Registry, store: &str, id: &str, owner: Option<Uuid>) {
    let source = registry.records.lock().unwrap()[store].1.clone();
    let mut file = source;
    file.record_id = Uuid::new_v4();
    file.raw_id = id.to_owned();
    file.kind = Some(NativeObjectKind::File);
    file.operation = "files".into();
    file.parent_id = None;
    registry.records.lock().unwrap().insert(
        id.to_owned(),
        (owner.unwrap_or(Uuid::parse_str(KEY).unwrap()), file),
    );
}
async fn attach(router: axum::Router, upstream: &MockServer, store: &str, file: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/vector_stores/{store}/files")))
        .respond_with(ResponseTemplate::new(200).set_body_json(vector_file(file, store)))
        .expect(1)
        .mount(upstream)
        .await;
    let answer = request(
        router,
        "POST",
        &format!("/v1/vector_stores/{store}/files"),
        Some(json!({"file_id":file,"attributes":{"language":"en"},"future_attach":"keep"})),
        "caller",
        false,
    )
    .await;
    let status = answer.status();
    let body = value(answer).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, vector_file(file, store));
}

#[tokio::test]
async fn vector_root_crud_rechecks_owner_and_keeps_unknown_fields() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(
        router.clone(),
        &upstream,
        "vs_owned",
        json!({"name":"Knowledge"}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores/vs_owned"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vector_store("vs_owned")))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores/vs_owned",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, vector_store("vs_owned"));
    for verb in ["GET", "POST", "DELETE"] {
        let answer = request(
            router.clone(),
            verb,
            "/v1/vector_stores/vs_owned",
            (verb == "POST").then(|| json!({"metadata":{"changed":"yes"}})),
            "other-caller",
            false,
        )
        .await;
        assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    }
    let mut updated = vector_store("vs_owned");
    updated["metadata"] = json!({"changed":"yes"});
    Mock::given(method("POST"))
        .and(path("/v1/vector_stores/vs_owned"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&updated))
        .expect(1)
        .mount(&upstream)
        .await;
    let change = json!({"metadata":{"changed":"yes"},"expires_after":{"anchor":"last_active_at","days":2},"future_update":[1,2]});
    let answer = request(
        router.clone(),
        "POST",
        "/v1/vector_stores/vs_owned",
        Some(change.clone()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, updated);
    let deleted = json!({"id":"vs_owned","object":"vector_store.deleted","deleted":true,"future_delete":true});
    Mock::given(method("DELETE"))
        .and(path("/v1/vector_stores/vs_owned"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&deleted))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "DELETE",
        "/v1/vector_stores/vs_owned",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, deleted);
    let answer = request(
        router,
        "GET",
        "/v1/vector_stores/vs_owned",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[2].body_json::<Value>().unwrap(), change);
}

#[tokio::test]
async fn one_file_can_belong_to_two_stores_and_detach_keeps_the_source() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_a", json!({"name":"A"})).await;
    create_store(router.clone(), &upstream, "vs_b", json!({"name":"B"})).await;
    seed_file(&registry, "vs_a", "file_shared", None).await;
    attach(router.clone(), &upstream, "vs_a", "file_shared").await;
    attach(router.clone(), &upstream, "vs_b", "file_shared").await;
    let update = json!({"attributes":{"language":"zh"},"future_update":true});
    let mut updated = vector_file("file_shared", "vs_b");
    updated["attributes"] = json!({"language":"zh"});
    Mock::given(method("POST"))
        .and(path("/v1/vector_stores/vs_b/files/file_shared"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&updated))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "POST",
        "/v1/vector_stores/vs_b/files/file_shared",
        Some(update.clone()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, updated);
    Mock::given(method("DELETE"))
        .and(path("/v1/vector_stores/vs_a/files/file_shared"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id":"file_shared","object":"vector_store.file.deleted","deleted":true}),
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "DELETE",
        "/v1/vector_stores/vs_a/files/file_shared",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores/vs_a/files/file_shared",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores/vs_b/files/file_shared"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&updated))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router,
        "GET",
        "/v1/vector_stores/vs_b/files/file_shared",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, updated);
    let records = registry.records.lock().unwrap();
    assert!(!records["file_shared"].1.deleted);
    assert!(records["file_shared"].1.parent_id.is_none());
    let calls = upstream.received_requests().await.unwrap();
    assert_eq!(calls.len(), 7);
    assert_eq!(calls[4].body_json::<Value>().unwrap(), update);
}

#[tokio::test]
async fn vector_search_is_post_only_and_parsed_content_is_json() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_search", json!({})).await;
    seed_file(&registry, "vs_search", "file_search", None).await;
    attach(router.clone(), &upstream, "vs_search", "file_search").await;
    let search = json!({"object":"vector_store.search_results.page","search_query":["refund"],"data":[{"file_id":"file_search","filename":"policy.txt","score":0.93,"attributes":{"topic":"billing"},"content":[{"type":"text","text":"Refunds within thirty days."}]}],"has_more":false,"next_page":null,"future_search":"keep"});
    Mock::given(method("POST"))
        .and(path("/v1/vector_stores/vs_search/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&search))
        .expect(1)
        .mount(&upstream)
        .await;
    let query = json!({"query":["refund"],"filters":{"key":"topic","type":"eq","value":"billing"},"ranking_options":{"ranker":"auto","score_threshold":0.4},"rewrite_query":true,"max_num_results":4,"future_search_parameter":true});
    let answer = request(
        router.clone(),
        "POST",
        "/v1/vector_stores/vs_search/search",
        Some(query.clone()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, search);
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores/vs_search/search",
        None,
        "caller",
        false,
    )
    .await;
    assert!(matches!(
        answer.status(),
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
    ));
    let content = json!({"object":"list","data":[{"type":"text","text":"Refunds within thirty days."}],"future_content":true});
    Mock::given(method("GET"))
        .and(path(
            "/v1/vector_stores/vs_search/files/file_search/content",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(&content))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores/vs_search/files/file_search/content",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, content);
    let answer = request(
        router,
        "GET",
        "/v1/vector_stores/vs_search/files/file_foreign/content",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    let calls = upstream.received_requests().await.unwrap();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[2].body_json::<Value>().unwrap(), query);
    assert_eq!(calls[2].headers["openai-beta"], "assistants=v2");
}

#[tokio::test]
async fn vector_file_batch_is_ingestion_with_exact_parent_and_batch_membership() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_batch", json!({})).await;
    create_store(router.clone(), &upstream, "vs_wrong", json!({})).await;
    seed_file(&registry, "vs_batch", "file_batch", None).await;
    seed_file(&registry, "vs_batch", "file_other", None).await;
    attach(router.clone(), &upstream, "vs_batch", "file_other").await;
    let accepted = file_batch("vsfb_owned", "vs_batch", "in_progress");
    Mock::given(method("POST"))
        .and(path("/v1/vector_stores/vs_batch/file_batches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&accepted))
        .expect(1)
        .mount(&upstream)
        .await;
    let submission = json!({"files":[{"file_id":"file_batch","attributes":{"department":"finance"},"chunking_strategy":{"type":"auto"}}],"future_batch_parameter":true});
    let answer = request(
        router.clone(),
        "POST",
        "/v1/vector_stores/vs_batch/file_batches",
        Some(submission.clone()),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, accepted);
    let completed = file_batch("vsfb_owned", "vs_batch", "completed");
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores/vs_batch/file_batches/vsfb_owned"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&completed))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores/vs_batch/file_batches/vsfb_owned",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, completed);
    let files = page(
        vec![
            vector_file("file_other", "vs_batch"),
            vector_file("file_batch", "vs_batch"),
        ],
        false,
    );
    Mock::given(method("GET"))
        .and(path(
            "/v1/vector_stores/vs_batch/file_batches/vsfb_owned/files",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(&files))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores/vs_batch/file_batches/vsfb_owned/files",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    let answer = value(answer).await;
    assert_eq!(
        answer["data"],
        json!([vector_file("file_batch", "vs_batch")])
    );
    assert_eq!(answer["first_id"], "file_batch");
    assert_eq!(answer["last_id"], "file_batch");
    let cancelled = file_batch("vsfb_owned", "vs_batch", "cancelled");
    Mock::given(method("POST"))
        .and(path(
            "/v1/vector_stores/vs_batch/file_batches/vsfb_owned/cancel",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(&cancelled))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "POST",
        "/v1/vector_stores/vs_batch/file_batches/vsfb_owned/cancel",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(value(answer).await, cancelled);
    for uri in [
        "/v1/vector_stores/vs_wrong/file_batches/vsfb_owned",
        "/v1/vector_stores/vs_wrong/file_batches/vsfb_owned/files",
        "/v1/vector_stores/vs_batch/file_batches/vsfb_owned/files?after=file_other",
    ] {
        let answer = request(router.clone(), "GET", uri, None, "caller", false).await;
        assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    }
    let calls = upstream.received_requests().await.unwrap();
    assert_eq!(calls.len(), 7);
    assert_eq!(calls[3].body_json::<Value>().unwrap(), submission);
    let operations = registry.operations.lock().unwrap();
    assert!(operations
        .values()
        .all(|operation| !operation.asynchronous && operation.entries.is_empty()));
}

#[tokio::test]
async fn vector_lists_fill_across_foreign_pages_and_never_publish_their_cursors() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    for id in ["vs_owned_a", "vs_owned_b", "vs_owned_c"] {
        create_store(router.clone(), &upstream, id, json!({})).await;
    }
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores"))
        .and(wiremock::matchers::query_param_is_missing("after"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![vector_store("vs_foreign_a"), vector_store("vs_owned_a")],
            true,
        )))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores"))
        .and(wiremock::matchers::query_param("after", "vs_owned_a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![vector_store("vs_foreign_b"), vector_store("vs_foreign_c")],
            true,
        )))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores"))
        .and(wiremock::matchers::query_param("after", "vs_foreign_c"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![vector_store("vs_owned_b"), vector_store("vs_owned_c")],
            false,
        )))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores?limit=2&model=vector-model",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    let answer = value(answer).await;
    assert_eq!(
        answer["data"],
        json!([vector_store("vs_owned_a"), vector_store("vs_owned_b")])
    );
    assert_eq!(answer["first_id"], "vs_owned_a");
    assert_eq!(answer["last_id"], "vs_owned_b");
    assert_eq!(answer["has_more"], true);
    assert!(!answer.to_string().contains("foreign"));
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores"))
        .and(wiremock::matchers::query_param("after", "vs_owned_b"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(page(vec![vector_store("vs_owned_c")], false)),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores?limit=2&after=vs_owned_b",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    let answer = value(answer).await;
    assert_eq!(answer["data"], json!([vector_store("vs_owned_c")]));
    assert_eq!(answer["has_more"], false);
    let answer = request(
        router,
        "GET",
        "/v1/vector_stores?after=vs_foreign_a",
        None,
        "caller",
        true,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 7);
}

#[tokio::test]
async fn create_store_admits_every_source_before_upstream_and_persists_accepted_membership() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_seed", json!({})).await;
    seed_file(&registry, "vs_seed", "file_owned", None).await;
    seed_file(
        &registry,
        "vs_seed",
        "file_foreign",
        Some(Uuid::parse_str(OTHER_KEY).unwrap()),
    )
    .await;
    let answer = request(
        router.clone(),
        "POST",
        "/v1/vector_stores",
        Some(json!({"file_ids":["file_owned","file_foreign"]})),
        "caller",
        true,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    create_store(
        router.clone(),
        &upstream,
        "vs_created",
        json!({"file_ids":["file_owned"]}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores/vs_created/files"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(page(vec![vector_file("file_owned", "vs_created")], false)),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router,
        "GET",
        "/v1/vector_stores/vs_created/files",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(
        value(answer).await["data"],
        json!([vector_file("file_owned", "vs_created")])
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn revoke_or_rotate_account_prevents_all_nested_reads_before_upstream() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, snapshot) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_owned", json!({})).await;
    seed_file(&registry, "vs_owned", "file_owned", None).await;
    attach(router.clone(), &upstream, "vs_owned", "file_owned").await;
    let old = snapshot.load().provider_keys.get_by_id(PROVIDER).unwrap();
    let mut changed = old.value.as_ref().clone();
    changed.api_key = "rotated-upstream-account".into();
    snapshot
        .load()
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, changed, 2));
    for (verb, uri, body) in [
        ("GET", "/v1/vector_stores/vs_owned", None),
        (
            "GET",
            "/v1/vector_stores/vs_owned/files/file_owned/content",
            None,
        ),
        (
            "POST",
            "/v1/vector_stores/vs_owned/search",
            Some(json!({"query":"secret"})),
        ),
        (
            "DELETE",
            "/v1/vector_stores/vs_owned/files/file_owned",
            None,
        ),
    ] {
        let answer = request(router.clone(), verb, uri, body, "caller", false).await;
        assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    }
    snapshot.load().provider_keys.insert(ResourceEntry::new(
        PROVIDER,
        old.value.as_ref().clone(),
        3,
    ));
    let old = snapshot.load().apikeys.get_by_id(KEY).unwrap();
    let mut revoked = old.value.as_ref().clone();
    revoked.allowed_models = vec!["unavailable".into()];
    snapshot
        .load()
        .apikeys
        .insert(ResourceEntry::new(KEY, revoked, 2));
    let answer = request(
        router,
        "POST",
        "/v1/vector_stores/vs_owned/search",
        Some(json!({"query":"secret"})),
        "caller",
        false,
    )
    .await;
    assert!(!answer.status().is_success());
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn binding_failure_never_replays_creation_or_releases_an_unbound_id() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    registry
        .fail_bind_for
        .lock()
        .unwrap()
        .insert("vs_unbound".into());
    let (router, _) = app(&upstream.uri(), registry);
    Mock::given(method("POST"))
        .and(path("/v1/vector_stores"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vector_store("vs_unbound")))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router,
        "POST",
        "/v1/vector_stores",
        Some(json!({})),
        "caller",
        true,
    )
    .await;
    assert!(!answer.status().is_success());
    assert!(!value(answer).await.to_string().contains("vs_unbound"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

fn native_response() -> Value {
    json!({"id":"resp_search","object":"response","created_at":1730000003i64,"status":"completed","error":null,"incomplete_details":null,
        "model":"provider-model","output":[
            {"id":"fs_search","type":"file_search_call","status":"completed","queries":["refund"],"results":[{"file_id":"file_history_only","filename":"policy.txt","score":0.9,"text":"Thirty days."}]},
            {"id":"msg_search","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Thirty days.","annotations":[{"type":"file_citation","file_id":"file_history_only","filename":"policy.txt","index":0}]}]}
        ],"usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13}})
}
fn native_responses(snapshot: &AisixSnapshot) {
    let old = snapshot.provider_keys.get_by_id(PROVIDER).unwrap();
    let mut provider = old.value.as_ref().clone();
    provider.apis = Some(serde_json::from_value(json!({"responses":{}})).unwrap());
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, old.revision + 1));
}
fn responses_only(snapshot: &AisixSnapshot) {
    let old = snapshot.apikeys.get_by_id(KEY).unwrap();
    let mut key = old.value.as_ref().clone();
    key.allowed_endpoints = Some(vec![aisix_core::models::EndpointFamily::Responses]);
    snapshot
        .apikeys
        .insert(ResourceEntry::new(KEY, key, old.revision + 1));
}

#[tokio::test]
async fn responses_file_search_uses_current_consumer_authority_without_management_grant() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, snapshot) = app_configured(&upstream.uri(), registry.clone(), native_responses);
    create_store(router.clone(), &upstream, "vs_response", json!({})).await;
    responses_only(&snapshot.load());
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(native_response()))
        .expect(1)
        .mount(&upstream)
        .await;
    let body = json!({"model":"vector-model","input":"What is the refund policy?","tools":[{"type":"file_search","vector_store_ids":["vs_response"],"max_num_results":3,"filters":{"key":"topic","type":"eq","value":"billing"}}],"include":["file_search_call.results"]});
    let answer = request(
        router.clone(),
        "POST",
        "/v1/responses",
        Some(body.clone()),
        "caller",
        false,
    )
    .await;
    let status = answer.status();
    let answer = value(answer).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["output"][0]["results"],
        native_response()["output"][0]["results"]
    );
    assert_eq!(
        answer["output"][1]["content"],
        native_response()["output"][1]["content"]
    );
    let answer = request(
        router.clone(),
        "GET",
        "/v1/vector_stores/vs_response",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::FORBIDDEN);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let sent = requests[1].body_json::<Value>().unwrap();
    assert_eq!(sent["tools"], body["tools"]);
    assert_eq!(sent["include"], body["include"]);
    assert!(!registry
        .records
        .lock()
        .unwrap()
        .contains_key("file_history_only"));
    assert!(registry
        .lookups
        .lock()
        .unwrap()
        .iter()
        .any(|lookup| lookup.locator.raw_id == "vs_response"
            && lookup.consumer_family == Some(aisix_core::models::EndpointFamily::Responses)));
}

#[tokio::test]
async fn foreign_file_search_ids_are_rejected_in_every_official_tool_definition_slot() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app_configured(&upstream.uri(), registry.clone(), native_responses);
    create_store(router.clone(), &upstream, "vs_foreign", json!({})).await;
    let mut changed = registry.records.lock().unwrap()["vs_foreign"].clone();
    changed.0 = Uuid::parse_str(OTHER_KEY).unwrap();
    registry
        .records
        .lock()
        .unwrap()
        .insert("vs_foreign".into(), changed);
    let tool = json!({"type":"file_search","vector_store_ids":["vs_foreign"]});
    for extension in [
        json!({"tools":[tool.clone()]}),
        json!({"tool_choice":{"type":"allowed_tools","mode":"auto","tools":[tool.clone()]}}),
        json!({"input":[{"type":"additional_tools","role":"developer","tools":[tool.clone()]}]}),
        json!({"input":[{"type":"tool_search_output","execution":"client","call_id":"call_local","tools":[tool.clone()]}]}),
    ] {
        let mut body = json!({"model":"vector-model","input":"Question"});
        body.as_object_mut()
            .unwrap()
            .extend(extension.as_object().unwrap().clone());
        let answer = request(
            router.clone(),
            "POST",
            "/v1/responses",
            Some(body),
            "caller",
            false,
        )
        .await;
        assert!(!answer.status().is_success());
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn inline_file_search_history_and_citations_neither_dereference_nor_claim_files() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app_configured(&upstream.uri(), registry.clone(), native_responses);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(native_response()))
        .expect(1)
        .mount(&upstream)
        .await;
    let input = json!([
        {"type":"file_search_call","id":"fs_inline","status":"completed","queries":["refund"],"results":[{"file_id":"file_unregistered","filename":"policy.txt","score":0.9,"text":"Thirty days."}]},
        {"type":"message","id":"msg_inline","role":"assistant","content":[{"type":"output_text","text":"Thirty days.","annotations":[{"type":"file_citation","file_id":"file_unregistered","filename":"policy.txt","index":0}]}]}
    ]);
    let answer = request(
        router,
        "POST",
        "/v1/responses",
        Some(json!({"model":"vector-model","input":input})),
        "caller",
        false,
    )
    .await;
    let status = answer.status();
    let answer = value(answer).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(!registry
        .lookups
        .lock()
        .unwrap()
        .iter()
        .any(|lookup| lookup.locator.kind == NativeObjectKind::File));
    assert!(!registry
        .records
        .lock()
        .unwrap()
        .contains_key("file_unregistered"));
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent = requests[0].body_json::<Value>().unwrap();
    assert_eq!(sent["input"][0]["results"], input[0]["results"]);
    assert_eq!(sent["input"][1]["content"], input[1]["content"]);
}

#[tokio::test]
async fn responses_stream_preserves_search_details_and_citations_before_terminal_close() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, snapshot) = app_configured(&upstream.uri(), registry.clone(), native_responses);
    create_store(router.clone(), &upstream, "vs_stream", json!({})).await;
    responses_only(&snapshot.load());
    let response = native_response();
    let events = [
        json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_search","object":"response","model":"provider-model","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":response["output"][0]}),
        json!({"type":"response.output_item.done","sequence_number":2,"output_index":1,"item":response["output"][1]}),
        json!({"type":"response.completed","sequence_number":3,"response":response}),
    ];
    let wire = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer=request(router,"POST","/v1/responses",Some(json!({"model":"vector-model","input":"Refunds?","stream":true,"tools":[{"type":"file_search","vector_store_ids":["vs_stream"]}],"include":["file_search_call.results"]})),"caller",false).await;
    assert_eq!(answer.status(), StatusCode::OK);
    let bytes = to_bytes(answer.into_body(), 1 << 20).await.unwrap();
    let frames = aisix_gateway::sse::SseDecoder::default()
        .feed(&bytes[..])
        .unwrap();
    let completed = frames
        .into_iter()
        .filter_map(|frame| match frame {
            aisix_gateway::sse::SseEvent::Data(data) => serde_json::from_str::<Value>(&data).ok(),
            _ => None,
        })
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    assert_eq!(
        completed["response"]["output"][0]["results"],
        native_response()["output"][0]["results"]
    );
    assert_eq!(
        completed["response"]["output"][1]["content"],
        native_response()["output"][1]["content"]
    );
    assert!(!registry
        .records
        .lock()
        .unwrap()
        .contains_key("file_history_only"));
}

#[tokio::test]
async fn rejected_mutation_releases_its_fence_and_does_not_delete_local_ownership() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_rejected", json!({})).await;
    seed_file(&registry, "vs_rejected", "file_rejected", None).await;
    Mock::given(method("POST")).and(path("/v1/vector_stores/vs_rejected/files"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"Unsupported attributes value.","type":"invalid_request_error","param":"attributes","code":"invalid_value"}})))
        .expect(1).up_to_n_times(1).mount(&upstream).await;
    let answer = request(
        router.clone(),
        "POST",
        "/v1/vector_stores/vs_rejected/files",
        Some(json!({"file_id":"file_rejected","attributes":{"x":[]}})),
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::BAD_REQUEST);
    assert!(registry.reservations.lock().unwrap().is_empty());
    attach(router.clone(), &upstream, "vs_rejected", "file_rejected").await;
    Mock::given(method("DELETE")).and(path("/v1/vector_stores/vs_rejected"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error":{"message":"Vector store is busy.","type":"invalid_request_error","code":"conflict"}})))
        .expect(1).mount(&upstream).await;
    let answer = request(
        router.clone(),
        "DELETE",
        "/v1/vector_stores/vs_rejected",
        None,
        "caller",
        false,
    )
    .await;
    assert!(!answer.status().is_success());
    assert!(!registry.records.lock().unwrap()["vs_rejected"].1.deleted);
    assert!(registry.deletion_reservations.lock().unwrap().is_empty());
    Mock::given(method("GET"))
        .and(path("/v1/vector_stores/vs_rejected"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vector_store("vs_rejected")))
        .expect(1)
        .mount(&upstream)
        .await;
    let answer = request(
        router,
        "GET",
        "/v1/vector_stores/vs_rejected",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 5);
}

#[tokio::test]
async fn uncertain_mutation_is_not_replayed_and_keeps_later_mutations_fenced() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_uncertain", json!({})).await;
    seed_file(&registry, "vs_uncertain", "file_uncertain", None).await;
    Mock::given(method("POST")).and(path("/v1/vector_stores/vs_uncertain/files"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error":{"message":"Internal server error.","type":"server_error","code":"server_error"}})))
        .expect(1).mount(&upstream).await;
    for _ in 0..2 {
        let answer = request(
            router.clone(),
            "POST",
            "/v1/vector_stores/vs_uncertain/files",
            Some(json!({"file_id":"file_uncertain"})),
            "caller",
            false,
        )
        .await;
        assert!(!answer.status().is_success());
    }
    assert_eq!(registry.reservations.lock().unwrap().len(), 1);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn search_never_releases_or_claims_an_unrelated_file_result() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_store(router.clone(), &upstream, "vs_search_guard", json!({})).await;
    Mock::given(method("POST")).and(path("/v1/vector_stores/vs_search_guard/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"vector_store.search_results.page","search_query":["secret"],"data":[{"file_id":"file_elsewhere","filename":"private.txt","score":1.0,"content":[{"type":"text","text":"Must stay private."}]}],"has_more":false,"next_page":null})))
        .expect(1).mount(&upstream).await;
    let answer = request(
        router,
        "POST",
        "/v1/vector_stores/vs_search_guard/search",
        Some(json!({"query":"secret"})),
        "caller",
        false,
    )
    .await;
    assert!(!answer.status().is_success());
    let answer = value(answer).await.to_string();
    assert!(!answer.contains("Must stay private."));
    assert!(!answer.contains("file_elsewhere"));
    assert!(!registry
        .records
        .lock()
        .unwrap()
        .contains_key("file_elsewhere"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn owned_file_search_definitions_work_in_every_official_slot_without_rewriting() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app_configured(&upstream.uri(), registry, native_responses);
    create_store(router.clone(), &upstream, "vs_slots", json!({})).await;
    let tool = json!({"type":"file_search","vector_store_ids":["vs_slots"],"ranking_options":{"ranker":"auto","score_threshold":0.2}});
    for (index,extension) in [
        json!({"tools":[tool.clone()]}),
        json!({"tools":[tool.clone()],"tool_choice":{"type":"allowed_tools","mode":"auto","tools":[tool.clone()]}}),
        json!({"input":[{"type":"additional_tools","role":"developer","tools":[tool.clone()]}]}),
        json!({"input":[{"type":"tool_search_output","execution":"client","call_id":"call_local","tools":[tool.clone()]}]}),
    ].into_iter().enumerate() {
        let mut response=native_response(); response["id"]=json!(format!("resp_slot_{index}"));
        response["output"][0]["id"]=json!(format!("fs_slot_{index}")); response["output"][1]["id"]=json!(format!("msg_slot_{index}"));
        Mock::given(method("POST")).and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response)).expect(1).up_to_n_times(1).mount(&upstream).await;
        let mut body=json!({"model":"vector-model","input":"Question","include":["file_search_call.results"]});
        body.as_object_mut().unwrap().extend(extension.as_object().unwrap().clone());
        let answer=request(router.clone(),"POST","/v1/responses",Some(body.clone()),"caller",false).await;
        let status=answer.status(); let answer=value(answer).await;
        assert_eq!(status,StatusCode::OK,"slot {index}: {answer}");
        let requests=upstream.received_requests().await.unwrap();
        let sent=requests.last().unwrap().body_json::<Value>().unwrap();
        for (key,expected) in extension.as_object().unwrap() {assert_eq!(&sent[key],expected,"slot {index}, {key}");}
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 5);
}
