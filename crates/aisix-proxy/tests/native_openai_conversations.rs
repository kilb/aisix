//! 原生 Conversations 的路由与 HTTP 组件回归；持久归属替身不代替真实控制面验收。
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
    relations: Mutex<HashMap<(Uuid, String), NativeMembership>>,
    relation_reservations: Mutex<HashMap<Uuid, ReserveRelationsRequest>>,
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
    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut records = self.records.lock().unwrap();
        let record = records
            .values_mut()
            .find(|(_, record)| record.record_id == request.record_id);
        let result = match record {
            Some((owner, record))
                if *owner == request.api_key_id && record.version == request.version =>
            {
                record.deleted = true;
                record.version += 1;
                Ok(NativeReply::new("deleted"))
            }
            _ => Err(NativeRegistryError::NotFound),
        };
        Box::pin(async move { result })
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
        let records = self.records.lock().unwrap();
        let found = records.values().any(|(owner, record)| {
            *owner == request.api_key_id
                && record.record_id == request.record_id
                && !record.deleted
                && record.version == request.version
        });
        let mut reply = NativeReply::new("reserved");
        reply.operation_id = Some(request.operation_id);
        if found {
            self.deletions
                .lock()
                .unwrap()
                .insert(request.operation_id, request);
        }
        Box::pin(async move {
            if found {
                Ok(reply)
            } else {
                Err(NativeRegistryError::NotFound)
            }
        })
    }
    fn commit_delete(
        &self,
        request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let reserved = self.deletions.lock().unwrap().remove(&request.operation_id);
        let mut records = self.records.lock().unwrap();
        let result = match (
            reserved,
            records
                .values_mut()
                .find(|(_, record)| record.record_id == request.record_id),
        ) {
            (Some(_), Some((owner, record)))
                if *owner == request.api_key_id && record.version == request.version =>
            {
                record.deleted = true;
                record.version += 1;
                let mut reply = NativeReply::new("deleted");
                reply.operation_id = Some(request.operation_id);
                reply.record = Some(record.clone());
                Ok(reply)
            }
            _ => Err(NativeRegistryError::NotFound),
        };
        Box::pin(async move { result })
    }
    fn reserve_relations(
        &self,
        request: ReserveRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let records = self.records.lock().unwrap();
        let parent = records
            .values()
            .find(|(owner, record)| {
                *owner == request.api_key_id
                    && record.record_id == request.parent_record_id
                    && record.version == request.expected_parent_version
                    && !record.deleted
            })
            .map(|(_, record)| record.clone());
        let mut reservations = self.relation_reservations.lock().unwrap();
        let busy = reservations
            .values()
            .any(|other| other.parent_record_id == request.parent_record_id);
        let mut reply = NativeReply::new("reserved");
        reply.operation_id = Some(request.operation_id);
        reply.record = parent.clone();
        if parent.is_some() && !busy {
            reservations.insert(request.operation_id, request);
        }
        Box::pin(async move {
            if parent.is_some() && !busy {
                Ok(reply)
            } else {
                Err(NativeRegistryError::Conflict)
            }
        })
    }
    fn commit_relations(
        &self,
        request: CommitRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let reservation = self
                .relation_reservations
                .lock()
                .unwrap()
                .remove(&request.operation_id)
                .ok_or(NativeRegistryError::NotFound)?;
            if reservation.api_key_id != request.api_key_id {
                return Err(NativeRegistryError::NotFound);
            }
            let mut records = self.records.lock().unwrap();
            let (owner, parent) = records
                .values()
                .find(|(_, record)| record.record_id == reservation.parent_record_id)
                .cloned()
                .ok_or(NativeRegistryError::NotFound)?;
            if parent.version != reservation.expected_parent_version || parent.deleted {
                return Err(NativeRegistryError::Conflict);
            }
            let mut relations = self.relations.lock().unwrap();
            let mut reply = NativeReply::new("committed");
            reply.operation_id = Some(request.operation_id);
            for object in request.objects {
                let child = if let Some((existing_owner, record)) = records.get(&object.raw_id) {
                    if *existing_owner != owner
                        || record.deleted
                        || record.kind != Some(object.kind)
                    {
                        return Err(NativeRegistryError::Conflict);
                    }
                    record.clone()
                } else {
                    let mut child = parent.clone();
                    child.record_id = Uuid::new_v4();
                    child.operation_id = Some(request.operation_id);
                    child.raw_id = object.raw_id;
                    child.kind = Some(object.kind);
                    child.parent_id = None;
                    child.version = 1;
                    records.insert(child.raw_id.clone(), (owner, child.clone()));
                    child
                };
                let key = (parent.record_id, child.raw_id.clone());
                if reservation.action == RelationAction::Detach {
                    relations.remove(&key);
                } else {
                    let membership = relations
                        .entry(key)
                        .or_insert_with(|| NativeMembership {
                            membership_id: Uuid::new_v4(),
                            parent_record_id: parent.record_id,
                            child_record_id: child.record_id,
                            relation: reservation.relation,
                            generation: 1,
                            version: 1,
                        })
                        .clone();
                    reply.memberships.push(membership);
                }
                reply.records.push(child);
            }
            let parent = records
                .values_mut()
                .find(|(_, record)| record.record_id == reservation.parent_record_id)
                .map(|(_, record)| record)
                .unwrap();
            parent.version += 1;
            reply.record = Some(parent.clone());
            Ok(reply)
        })
    }
    fn lookup_relations(
        &self,
        request: LookupRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let records = self.records.lock().unwrap();
        let parent = records.values().find(|(owner, record)| {
            *owner == request.api_key_id
                && record.record_id == request.parent_record_id
                && !record.deleted
        });
        let mut reply = NativeReply::new("related");
        if parent.is_some() {
            for ((parent_id, raw), membership) in self.relations.lock().unwrap().iter() {
                if *parent_id == request.parent_record_id
                    && membership.relation == request.relation
                    && request.raw_ids.contains(raw)
                {
                    if let Some((owner, record)) = records.get(raw) {
                        if *owner == request.api_key_id && !record.deleted {
                            reply.records.push(record.clone());
                            reply.memberships.push(membership.clone());
                        }
                    }
                }
            }
        }
        let present = parent.is_some();
        Box::pin(async move {
            if present {
                Ok(reply)
            } else {
                Err(NativeRegistryError::NotFound)
            }
        })
    }
    fn resolve_reservation(
        &self,
        request: ResolveReservationRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        if request.upstream_rejected {
            self.relation_reservations
                .lock()
                .unwrap()
                .remove(&request.operation_id);
        }
        let reply = NativeReply::new(if request.upstream_rejected {
            "rejected"
        } else {
            "uncertain"
        });
        Box::pin(async move { Ok(reply) })
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
        "display_name":"conversations-account", "secret":"upstream-key", "provider":"openai",
        "api_base":format!("{}/v1",base.trim_end_matches('/').trim_end_matches("/v1")), "adapter":"openai"
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"conversation-model", "provider":"openai", "model_name":"provider-model",
        "provider_key_id":PROVIDER
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    for (id, caller) in [(KEY, "caller"), (OTHER_KEY, "other-caller")] {
        let key: ApiKey = serde_json::from_value(json!({
            "key_hash":ApiKey::hash_bearer(caller), "allowed_models":["conversation-model"]
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
            "native-conversations-test",
        ))
        .with_native_objects(registry);
    (build_router(state), snapshot)
}

fn conversation(id: &str) -> Value {
    json!({"id":id,"object":"conversation","created_at":1730000000i64,
        "metadata":{"tag":"retained"},"future_extension":{"retained":true}})
}
async fn request(
    router: axum::Router,
    method_name: &str,
    uri: &str,
    value: Option<Value>,
    caller: &str,
    hint: bool,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method_name)
        .uri(uri)
        .header("authorization", format!("Bearer {caller}"));
    if hint {
        request = request.header("x-aisix-model", "conversation-model");
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

// 删除路由、改写原始ID、丢字段或把网关selector发给上游，任一变更都应使此用例失败。
#[tokio::test]
async fn conversation_create_preserves_original_wire_and_removes_only_routing_selector() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/conversations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(conversation("conv_original")))
        .expect(1)
        .mount(&upstream)
        .await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let body = json!({"metadata":{"tag":"retained"},"items":[{"role":"user","content":"hello"}],"future_parameter":{"keep":true}});
    let response = request(
        router,
        "POST",
        "/v1/conversations?model=conversation-model&x=a%2Fb&x=two",
        Some(body.clone()),
        "caller",
        false,
    )
    .await;
    let status = response.status();
    let answer = value(response).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer, conversation("conv_original"));
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.query(), Some("x=a%2Fb&x=two"));
    assert_eq!(requests[0].body_json::<Value>().unwrap(), body);
    assert_eq!(requests[0].headers["authorization"], "Bearer upstream-key");
}

async fn create_conversation(router: axum::Router, upstream: &MockServer, id: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/conversations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(conversation(id)))
        .expect(1)
        .up_to_n_times(1)
        .mount(upstream)
        .await;
    let response = request(
        router,
        "POST",
        "/v1/conversations",
        Some(json!({"metadata":{"tag":"retained"}})),
        "caller",
        true,
    )
    .await;
    let status = response.status();
    let answer = value(response).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["id"], id);
}

#[tokio::test]
async fn conversation_management_keeps_wire_and_deleted_conversation_cannot_be_read() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    create_conversation(router.clone(), &upstream, "conv_lifecycle").await;
    Mock::given(method("GET"))
        .and(path("/v1/conversations/conv_lifecycle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(conversation("conv_lifecycle")))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "GET",
        "/v1/conversations/conv_lifecycle?include=extra",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await, conversation("conv_lifecycle"));
    let updated = json!({"id":"conv_lifecycle","object":"conversation","created_at":1730000000i64,"metadata":{"tag":"updated"},"future_extension":{"updated":true}});
    Mock::given(method("POST"))
        .and(path("/v1/conversations/conv_lifecycle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(updated.clone()))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "POST",
        "/v1/conversations/conv_lifecycle",
        Some(json!({"metadata":{"tag":"updated"}})),
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await, updated);
    let deleted = json!({"id":"conv_lifecycle","object":"conversation.deleted","deleted":true,"future_delete":"retained"});
    Mock::given(method("DELETE"))
        .and(path("/v1/conversations/conv_lifecycle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(deleted.clone()))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "DELETE",
        "/v1/conversations/conv_lifecycle",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await, deleted);
    let response = request(
        router,
        "GET",
        "/v1/conversations/conv_lifecycle",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn foreign_conversation_and_revoked_model_do_not_reach_shared_upstream() {
    let upstream = MockServer::builder().start().await;
    let (router, snapshot) = app(&upstream.uri(), Arc::new(Registry::default()));
    create_conversation(router.clone(), &upstream, "conv_private").await;
    for method_name in ["GET", "POST", "DELETE"] {
        let body = (method_name == "POST").then(|| json!({"metadata":{"tag":"stolen"}}));
        let response = request(
            router.clone(),
            method_name,
            "/v1/conversations/conv_private",
            body,
            "other-caller",
            false,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "foreign {method_name}"
        );
    }
    let entry = snapshot.load().apikeys.get_by_id(KEY).unwrap();
    let mut key = entry.value.as_ref().clone();
    key.allowed_models.clear();
    snapshot
        .load()
        .apikeys
        .insert(ResourceEntry::new(KEY, key, 2));
    let response = request(
        router,
        "GET",
        "/v1/conversations/conv_private",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn conversation_bind_failure_hides_unregistered_id_without_recreating() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/conversations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(conversation("conv_unregistered_private")),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    registry
        .fail_bind_for
        .lock()
        .unwrap()
        .insert("conv_unregistered_private".into());
    let (router, _) = app(&upstream.uri(), registry);
    let response = request(
        router,
        "POST",
        "/v1/conversations",
        Some(json!({"metadata":{}})),
        "caller",
        true,
    )
    .await;
    assert!(!response.status().is_success());
    assert!(!value(response)
        .await
        .to_string()
        .contains("conv_unregistered_private"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

fn item(id: &str) -> Value {
    json!({"id":id,"type":"message","role":"user","status":"completed",
        "content":[{"type":"input_text","text":"hello"}],"future_item":{"keep":true}})
}
fn page(items: Vec<Value>, more: bool) -> Value {
    json!({"object":"list","first_id":items.first().and_then(|item|item.get("id")),
        "last_id":items.last().and_then(|item|item.get("id")),"data":items,"has_more":more,
        "future_page":{"keep":true}})
}

#[tokio::test]
async fn conversation_items_append_list_retrieve_detach_preserve_original_contract() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    create_conversation(router.clone(), &upstream, "conv_items").await;
    let appended = page(vec![item("msg_original")], false);
    Mock::given(method("POST"))
        .and(path("/v1/conversations/conv_items/items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(appended.clone()))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "POST",
        "/v1/conversations/conv_items/items?include=message.output_text.logprobs",
        Some(json!({"items":[{"role":"user","content":"hello"}]})),
        "caller",
        false,
    )
    .await;
    let status = response.status();
    let answer = value(response).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer, appended);
    Mock::given(method("GET"))
        .and(path("/v1/conversations/conv_items/items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(appended.clone()))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "GET",
        "/v1/conversations/conv_items/items?limit=1&order=asc",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await, appended);
    Mock::given(method("GET"))
        .and(path("/v1/conversations/conv_items/items/msg_original"))
        .respond_with(ResponseTemplate::new(200).set_body_json(item("msg_original")))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "GET",
        "/v1/conversations/conv_items/items/msg_original",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await, item("msg_original"));
    // SDK items.delete返回Conversation；没有deleted字段也必须成功。
    Mock::given(method("DELETE"))
        .and(path("/v1/conversations/conv_items/items/msg_original"))
        .respond_with(ResponseTemplate::new(200).set_body_json(conversation("conv_items")))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router.clone(),
        "DELETE",
        "/v1/conversations/conv_items/items/msg_original",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await, conversation("conv_items"));
    // 解除集合关系不能删除独立item；仍能在自己新会话中引用它。
    Mock::given(method("POST"))
        .and(path("/v1/conversations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(conversation("conv_reused")))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router,
        "POST",
        "/v1/conversations",
        Some(json!({"items":[{"type":"item_reference","id":"msg_original"}]})),
        "caller",
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await["id"], "conv_reused");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 6);
    assert_eq!(
        requests[1].url.query(),
        Some("include=message.output_text.logprobs")
    );
}

#[tokio::test]
async fn conversation_initial_automatic_items_are_discovered_from_owned_parent_after_router_restart(
) {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_conversation(router, &upstream, "conv_initial").await;
    Mock::given(method("GET"))
        .and(path("/v1/conversations/conv_initial/items"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![item("msg_auto")], true)))
        .expect(1)
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    let (router, _) = app(&upstream.uri(), registry.clone());
    let response = request(
        router.clone(),
        "GET",
        "/v1/conversations/conv_initial/items?limit=1&order=asc",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await, page(vec![item("msg_auto")], true));
    Mock::given(method("GET"))
        .and(path("/v1/conversations/conv_initial/items"))
        .and(wiremock::matchers::query_param("after", "msg_auto"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![], false)))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router,
        "GET",
        "/v1/conversations/conv_initial/items?after=msg_auto&limit=1&order=asc",
        None,
        "caller",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await, page(vec![], false));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn conversation_nested_foreign_file_and_item_references_never_reach_upstream() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry);
    create_conversation(router.clone(), &upstream, "conv_secure").await;
    for body in [
        json!({"items":[{"role":"user","content":[{"type":"input_text","text":"hello"},{"type":"input_file","file_id":"file_foreign"}]}]}),
        json!({"items":[{"type":"item_reference","id":"msg_foreign"}]}),
        json!({"items":[{"type":"code_interpreter_call","id":"ci_inline","container_id":"cntr_foreign","code":"print(1)","status":"completed","outputs":[]}]}),
        json!({"items":[{"type":"additional_tools","tools":[{"type":"file_search","vector_store_ids":["vs_foreign"]}]}]}),
    ] {
        let response = request(
            router.clone(),
            "POST",
            "/v1/conversations/conv_secure/items",
            Some(body),
            "caller",
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn conversation_inline_tool_history_does_not_claim_or_dereference_citation_ids() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/conversations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(conversation("conv_history")))
        .expect(1)
        .mount(&upstream)
        .await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let body = json!({"items":[
        {"type":"function_call_output","call_id":"call_history","output":"{\"type\":\"input_file\",\"file_id\":\"file_business\"}"},
        {"type":"file_search_call","id":"fs_history","queries":["q"],"status":"completed","results":[{"file_id":"file_citation","filename":"history.txt","score":0.8,"text":"inline history"}]},
        {"type":"additional_tools","tools":[{"type":"function","name":"business","parameters":{"type":"object","properties":{"file_id":{"const":"file_business"}}}}]}
    ]});
    let response = request(
        router,
        "POST",
        "/v1/conversations",
        Some(body.clone()),
        "caller",
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(value(response).await["id"], "conv_history");
    assert_eq!(
        upstream.received_requests().await.unwrap()[0]
            .body_json::<Value>()
            .unwrap(),
        body
    );
}

fn block_conversation_input(snapshot: &AisixSnapshot) {
    let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
        "name":"block-all-conversation-input","enabled":true,"kind":"custom",
        "hook_point":"input","fail_open":false,"timeout_ms":5000,
        "script":"export function checkInput() { return { action: 'block', reason_code: 'blocked' }; }"
    }))
    .unwrap();
    snapshot
        .guardrails
        .insert(ResourceEntry::new("conversation-input-block", guardrail, 1));
    let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
        "guardrail_id":"conversation-input-block","scope_type":"env","priority":100
    }))
    .unwrap();
    snapshot.guardrail_attachments.insert(ResourceEntry::new(
        "conversation-input-block-attachment",
        attachment,
        1,
    ));
}

// 跳过任一管理操作的输入链（包括空 GET/DELETE），应暴露403或零上游触达断言失败。
#[tokio::test]
async fn unconditional_input_guardrail_blocks_all_eight_conversation_operations() {
    let upstream = MockServer::builder().start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    create_conversation(router.clone(), &upstream, "conv_guardrail").await;
    Mock::given(method("POST"))
        .and(path("/v1/conversations/conv_guardrail/items"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(page(vec![item("msg_guardrail")], false)),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = request(
        router,
        "POST",
        "/v1/conversations/conv_guardrail/items",
        Some(json!({"items":[{"role":"user","content":"hello"}]})),
        "caller",
        false,
    )
    .await;
    let status = response.status();
    let answer = value(response).await;
    assert_eq!(status, StatusCode::OK, "bootstrap append: {answer}");
    assert_eq!(answer["data"][0]["id"], "msg_guardrail");
    let accepted_calls = upstream.received_requests().await.unwrap().len();
    assert_eq!(accepted_calls, 2);
    let (router, _) = app_configured(&upstream.uri(), registry, block_conversation_input);
    for (verb, route, body, hint) in [
        (
            "POST",
            "/v1/conversations",
            Some(json!({"metadata":{}})),
            true,
        ),
        ("GET", "/v1/conversations/conv_guardrail", None, false),
        (
            "POST",
            "/v1/conversations/conv_guardrail",
            Some(json!({"metadata":{}})),
            false,
        ),
        ("DELETE", "/v1/conversations/conv_guardrail", None, false),
        (
            "POST",
            "/v1/conversations/conv_guardrail/items",
            Some(json!({"items":[{"type":"item_reference","id":"msg_guardrail"}]})),
            false,
        ),
        ("GET", "/v1/conversations/conv_guardrail/items", None, false),
        (
            "GET",
            "/v1/conversations/conv_guardrail/items/msg_guardrail",
            None,
            false,
        ),
        (
            "DELETE",
            "/v1/conversations/conv_guardrail/items/msg_guardrail",
            None,
            false,
        ),
    ] {
        let response = request(router.clone(), verb, route, body, "caller", hint).await;
        let status = response.status();
        let answer = value(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{verb} {route}: {answer}");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            accepted_calls,
            "blocked {verb} {route} reached the provider"
        );
    }
}
