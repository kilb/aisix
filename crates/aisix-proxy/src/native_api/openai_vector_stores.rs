//! 原生向量库保持上游标识；文件归属与各集合成员关系分别持久授权。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{
    AcceptedReference, ArtifactSelector, BeginRelatedRequest, BeginRequest, BindRelatedRequest,
    BindRequest, BoundRelation, CommitRelationsRequest, InferenceStage, LookupRelationsRequest,
    NativeChildObject, NativeObjectKind, NativeProtocol, NativeRecord, NativeRelation,
    ObjectDeletionRequest, OriginBinding, OwnedRequest, RelationAction, ReserveRelationsRequest,
    ResolveReservationRequest,
};
use axum::extract::{RawQuery, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{json, Value};
use uuid::Uuid;

use super::openai::{self, Audit, Body};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::jobs::JobTarget;
use crate::reject::AisixPath;
use crate::request_json::{Json, JsonRejection};
use crate::state::ProxyState;

fn not_found() -> ProxyError {
    ProxyError::JobResourceNotFound("vector_store".into())
}
fn invalid_response() -> ProxyError {
    openai::invalid_identity()
}
fn same_account(left: &OriginBinding, right: &OriginBinding) -> bool {
    left.provider_key_id == right.provider_key_id && left.boundary == right.boundary
}
fn hint(client: &ClientContext, query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find_map(|(key, value)| (key == "model").then(|| value.into_owned()))
        .or_else(|| {
            client
                .headers
                .get("x-aisix-model")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        })
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
fn raw_id(value: &Value) -> Result<&str, ProxyError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(invalid_response)?;
    crate::jobs::require_safe_upstream_id(id).map_err(|_| invalid_response())?;
    Ok(id)
}
fn validate(
    value: &Value,
    expected: Option<&str>,
    object: &str,
    store: Option<&str>,
) -> Result<(), ProxyError> {
    let id = raw_id(value)?;
    if expected.is_some_and(|expected| id != expected)
        || value.get("object").and_then(Value::as_str) != Some(object)
        || store.is_some_and(|store| {
            value.get("vector_store_id").and_then(Value::as_str) != Some(store)
        })
    {
        return Err(invalid_response());
    }
    Ok(())
}
fn file_ids(value: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = value.get("file_id").and_then(Value::as_str) {
        ids.push(id.to_owned());
    }
    if let Some(files) = value.get("file_ids").and_then(Value::as_array) {
        ids.extend(files.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    if let Some(files) = value.get("files").and_then(Value::as_array) {
        ids.extend(
            files
                .iter()
                .filter_map(|file| file.get("file_id").and_then(Value::as_str))
                .map(str::to_owned),
        );
    }
    ids.sort();
    ids.dedup();
    ids
}
async fn admit_files(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    ids: &[String],
    expected: &OriginBinding,
) -> Result<Vec<NativeRecord>, ProxyError> {
    let mut records = Vec::new();
    for id in ids {
        let (_, actual, record) = crate::attribution::detached(openai::lookup_file_reference(
            state,
            auth,
            client,
            id,
            EndpointFamily::VectorStores,
        ))
        .await?;
        if !same_account(&actual, expected) {
            return Err(not_found());
        }
        records.push(record);
    }
    Ok(records)
}
async fn selector_matches(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    query: Option<&str>,
    origin: &OriginBinding,
) -> Result<(), ProxyError> {
    if let Some(wanted) = hint(client, query) {
        let snapshot = state.snapshot.load();
        let (_, actual) = crate::attribution::detached(async {
            openai::resolve_management(
                state,
                &snapshot,
                auth,
                client,
                &wanted,
                NativeObjectKind::VectorStore,
            )
        })
        .await?;
        if !same_account(&actual, origin) {
            return Err(not_found());
        }
    }
    Ok(())
}
async fn reserve(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    parent: &NativeRecord,
    relation: NativeRelation,
    action: RelationAction,
    children: &[NativeRecord],
) -> Result<Uuid, ProxyError> {
    let operation_id = Uuid::new_v4();
    let reply = openai::registry(state)?
        .reserve_relations(ReserveRelationsRequest {
            api_key_id: openai::api_key_id(auth)?,
            operation_id,
            parent_record_id: parent.record_id,
            expected_parent_version: parent.version,
            relation,
            action,
            child_record_ids: children.iter().map(|record| record.record_id).collect(),
            references: Vec::new(),
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state != "reserved" || reply.operation_id != Some(operation_id) {
        return Err(openai::dependency());
    }
    Ok(operation_id)
}
async fn commit(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    operation_id: Uuid,
    objects: Vec<NativeChildObject>,
) -> Result<(), ProxyError> {
    let reply = openai::registry(state)?
        .commit_relations(CommitRelationsRequest {
            api_key_id: openai::api_key_id(auth)?,
            operation_id,
            objects,
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state != "committed" {
        return Err(openai::dependency());
    }
    Ok(())
}
async fn resolve_reservation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    operation_id: Uuid,
    upstream_rejected: bool,
) -> Result<(), ProxyError> {
    let reply = openai::registry(state)?
        .resolve_reservation(ResolveReservationRequest {
            api_key_id: openai::api_key_id(auth)?,
            operation_id,
            upstream_rejected,
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state
        != if upstream_rejected {
            "rejected"
        } else {
            "uncertain"
        }
    {
        return Err(openai::dependency());
    }
    Ok(())
}
async fn related(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    parent: &NativeRecord,
    relation: NativeRelation,
    ids: &[String],
    expected: &OriginBinding,
) -> Result<HashMap<String, NativeRecord>, ProxyError> {
    let reply = openai::registry(state)?
        .lookup_relations(LookupRelationsRequest {
            api_key_id: openai::api_key_id(auth)?,
            parent_record_id: parent.record_id,
            relation,
            raw_ids: ids.to_vec(),
            consumer_family: Some(EndpointFamily::VectorStores),
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state != "related" {
        return Err(openai::dependency());
    }
    let memberships: HashSet<_> = reply
        .memberships
        .iter()
        .filter(|membership| {
            membership.parent_record_id == parent.record_id && membership.relation == relation
        })
        .map(|membership| membership.child_record_id)
        .collect();
    let mut records = HashMap::new();
    for record in reply.records {
        if record.deleted
            || !memberships.contains(&record.record_id)
            || record
                .origin
                .as_ref()
                .is_none_or(|origin| !same_account(origin, expected))
            || !ids.contains(&record.raw_id)
        {
            return Err(openai::dependency());
        }
        // 关系查询之外仍执行 DP 当前模型、来源与 IP 准入，避免旧快照放行。
        let kind = record.kind.ok_or_else(openai::dependency)?;
        let (_, origin, current) = if kind.endpoint_family() == EndpointFamily::VectorStores {
            crate::attribution::detached(openai::lookup_management(
                state,
                auth,
                client,
                &record.raw_id,
                kind,
                None,
            ))
            .await?
        } else {
            crate::attribution::detached(openai::lookup_reference(
                state,
                auth,
                client,
                &record.raw_id,
                kind,
                None,
                EndpointFamily::VectorStores,
            ))
            .await?
        };
        if !same_account(&origin, expected) || current.record_id != record.record_id {
            return Err(not_found());
        }
        records.insert(record.raw_id.clone(), record);
    }
    Ok(records)
}
async fn require_related(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    parent: &NativeRecord,
    relation: NativeRelation,
    id: &str,
    expected: &OriginBinding,
) -> Result<NativeRecord, ProxyError> {
    related(
        state,
        auth,
        client,
        parent,
        relation,
        &[id.to_owned()],
        expected,
    )
    .await?
    .remove(id)
    .ok_or_else(not_found)
}

async fn begin_creation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    origin: &OriginBinding,
    parent: Option<&NativeRecord>,
    files: &[NativeRecord],
) -> Result<Uuid, ProxyError> {
    let operation_id = Uuid::new_v4();
    let mut references: Vec<_> = files
        .iter()
        .map(|file| AcceptedReference {
            record_id: file.record_id,
            consumer_family: EndpointFamily::VectorStores,
        })
        .collect();
    if let Some(parent) = parent {
        references.push(AcceptedReference {
            record_id: parent.record_id,
            consumer_family: EndpointFamily::VectorStores,
        });
    }
    let requested = if let Some(parent) = parent {
        (
            parent.requested_model.clone(),
            parent.requested_model_id.ok_or_else(openai::dependency)?,
        )
    } else {
        (
            target.model_entry.value.display_name.clone(),
            origin.model_id,
        )
    };
    let reply = openai::registry(state)?
        .begin_related(BeginRelatedRequest {
            entry_references: Default::default(),
            lifecycle_context: None,
            submission: BeginRequest {
                upstream_input_text: None,
                input_file_id: None,
                operation_id,
                api_key_id: openai::api_key_id(auth)?,
                protocol: NativeProtocol::Openai,
                kind: if parent.is_some() {
                    NativeObjectKind::VectorStoreFileBatch
                } else {
                    NativeObjectKind::VectorStore
                },
                origin: origin.clone(),
                parent_id: parent.map(|p| p.record_id),
                requested_model_id: requested.1,
                requested_model: requested.0,
                upstream_model: target
                    .model_entry
                    .value
                    .upstream_model()
                    .unwrap_or_default()
                    .to_owned(),
                upstream_provider: target.pk_entry.value.provider.clone(),
                operation: "vector_stores".into(),
                request_id: client.request_id.clone(),
                asynchronous: false,
                requested_service_tier: None,
                entries: Vec::new(),
                observation_headers: Default::default(),
            },
            references,
            owner_parent_record_id: parent.map(|p| p.record_id),
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state != "pending_submission"
        || reply.operation_id != Some(operation_id)
        || reply.origin.as_ref() != Some(origin)
    {
        return Err(openai::dependency());
    }
    Ok(operation_id)
}
async fn bind_creation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    operation_id: Uuid,
    origin: &OriginBinding,
    parent: Option<&NativeRecord>,
    files: &[NativeRecord],
    value: &Value,
) -> Result<NativeRecord, ProxyError> {
    let mut relations = Vec::new();
    if let Some(parent) = parent {
        relations.push(BoundRelation {
            parent: ArtifactSelector::Record {
                record_id: parent.record_id,
            },
            child: ArtifactSelector::Root,
            relation: NativeRelation::VectorStoreBatch,
        });
    }
    for file in files {
        relations.push(BoundRelation {
            parent: ArtifactSelector::Root,
            child: ArtifactSelector::Record {
                record_id: file.record_id,
            },
            relation: if parent.is_some() {
                NativeRelation::VectorStoreBatchFile
            } else {
                NativeRelation::VectorStoreFile
            },
        });
        if let Some(parent) = parent {
            relations.push(BoundRelation {
                parent: ArtifactSelector::Record {
                    record_id: parent.record_id,
                },
                child: ArtifactSelector::Record {
                    record_id: file.record_id,
                },
                relation: NativeRelation::VectorStoreFile,
            });
        }
    }
    let reply = openai::registry(state)?
        .bind_related(BindRelatedRequest {
            binding: BindRequest {
                existing_record_id: None,
                objects: Vec::new(),
                operation_id,
                api_key_id: openai::api_key_id(auth)?,
                raw_id: raw_id(value)?.to_owned(),
                provider_created_at: value
                    .get("created_at")
                    .and_then(Value::as_i64)
                    .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
                provider_expires_at: value
                    .get("expires_at")
                    .and_then(Value::as_i64)
                    .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
            },
            relations,
        })
        .await
        .map_err(openai::registry_error)?;
    reply
        .record
        .filter(|record| {
            reply.state == "bound"
                && !record.deleted
                && record.raw_id == raw_id(value).unwrap_or_default()
                && record.origin.as_ref() == Some(origin)
                && record.protocol == Some(NativeProtocol::Openai)
                && record.kind
                    == Some(if parent.is_some() {
                        NativeObjectKind::VectorStoreFileBatch
                    } else {
                        NativeObjectKind::VectorStore
                    })
        })
        .ok_or_else(openai::dependency)
}

fn reject_json(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    started: Instant,
    path: &str,
    error: JsonRejection,
) -> Response {
    crate::reject::reject_before_dispatch(
        state,
        "POST",
        path,
        &client.request_id,
        Some(&auth.entry.id),
        started,
        crate::reject::Envelope::OpenAi,
        crate::error::proxy_error_from_json_rejection(error, state.request_body_limit_for(path)),
    )
}

#[derive(Clone, Copy)]
enum Action {
    Create,
    List,
    Get,
    Update,
    Delete,
    Search,
    Attach,
    ListFiles,
    GetFile,
    UpdateFile,
    Detach,
    Content,
    CreateBatch,
    GetBatch,
    CancelBatch,
    BatchFiles,
}
impl Action {
    fn method(self) -> Method {
        match self {
            Self::Create
            | Self::Update
            | Self::Search
            | Self::Attach
            | Self::UpdateFile
            | Self::CreateBatch
            | Self::CancelBatch => Method::POST,
            Self::Delete | Self::Detach => Method::DELETE,
            _ => Method::GET,
        }
    }
    fn path(self, store: &str, child: &str) -> String {
        let root = format!("/vector_stores/{store}");
        match self {
            Self::Create | Self::List => "/vector_stores".into(),
            Self::Get | Self::Update | Self::Delete => root,
            Self::Search => format!("{root}/search"),
            Self::Attach | Self::ListFiles => format!("{root}/files"),
            Self::GetFile | Self::UpdateFile | Self::Detach => format!("{root}/files/{child}"),
            Self::Content => format!("{root}/files/{child}/content"),
            Self::CreateBatch => format!("{root}/file_batches"),
            Self::GetBatch => format!("{root}/file_batches/{child}"),
            Self::CancelBatch => format!("{root}/file_batches/{child}/cancel"),
            Self::BatchFiles => format!("{root}/file_batches/{child}/files"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    query: Option<String>,
    action: Action,
    store: String,
    child: String,
    body: Option<Value>,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let path = action.path(&store, &child);
    let public_path = format!("/v1{path}");
    let mut audit = Audit::default();
    let mut pending_operation = None;
    let result = async {
        if !store.is_empty() {
            crate::jobs::require_safe_upstream_id(&store).map_err(|_| not_found())?;
        }
        if !child.is_empty() {
            crate::jobs::require_safe_upstream_id(&child).map_err(|_| not_found())?;
        }
        let (target, origin, parent) = if matches!(action, Action::Create | Action::List) {
            if let Some(wanted) = hint(&client, query.as_deref()) {
                let (target, origin) = openai::resolve_management(
                    &state,
                    &snapshot,
                    &auth,
                    &client,
                    &wanted,
                    NativeObjectKind::VectorStore,
                )?;
                (target, origin, None)
            } else if matches!(action, Action::List) {
                let cursor =
                    url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
                        .find_map(|(key, value)| {
                            matches!(key.as_ref(), "after" | "before").then(|| value.into_owned())
                        })
                        .ok_or_else(|| {
                            ProxyError::InvalidRequest(
                                "pass model or x-aisix-model to select a direct model".into(),
                            )
                        })?;
                let (target, origin, _) = openai::lookup_management(
                    &state,
                    &auth,
                    &client,
                    &cursor,
                    NativeObjectKind::VectorStore,
                    None,
                )
                .await?;
                (target, origin, None)
            } else {
                return Err(ProxyError::InvalidRequest(
                    "pass model or x-aisix-model to select a direct model".into(),
                ));
            }
        } else {
            let (target, origin, record) = openai::lookup_management(
                &state,
                &auth,
                &client,
                &store,
                NativeObjectKind::VectorStore,
                None,
            )
            .await?;
            selector_matches(&state, &auth, &client, query.as_deref(), &origin).await?;
            (target, origin, Some(record))
        };
        audit.resolved(&target);
        let parent_ref = || parent.as_ref().ok_or_else(openai::dependency);
        let mut member = None;
        if matches!(
            action,
            Action::GetFile | Action::UpdateFile | Action::Detach | Action::Content
        ) {
            member = Some(
                require_related(
                    &state,
                    &auth,
                    &client,
                    parent_ref()?,
                    NativeRelation::VectorStoreFile,
                    &child,
                    &origin,
                )
                .await?,
            );
        }
        if matches!(
            action,
            Action::GetBatch | Action::CancelBatch | Action::BatchFiles
        ) {
            member = Some(
                require_related(
                    &state,
                    &auth,
                    &client,
                    parent_ref()?,
                    NativeRelation::VectorStoreBatch,
                    &child,
                    &origin,
                )
                .await?,
            );
        }
        let ids = body.as_ref().map(file_ids).unwrap_or_default();
        let files = if matches!(
            action,
            Action::Create | Action::Attach | Action::CreateBatch
        ) {
            admit_files(&state, &auth, &client, &ids, &origin).await?
        } else {
            Vec::new()
        };
        // 引用查验的归属不可覆盖本次管理请求的计量和审计目标。
        audit.resolved(&target);
        // 新增存储检查提供商预算，已存对象的读取、元数据编辑和清理仍走管理授权。
        if matches!(action, Action::Attach | Action::CreateBatch)
            || matches!(action, Action::Create) && !files.is_empty()
        {
            crate::budget::gate(&state, &snapshot).check_provider_key(&target.pk_entry.id)?;
        }
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let bytes = body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| invalid_response())?
            .unwrap_or_default();
        if body.is_some() {
            crate::usage_attr::note_failure_request(&snapshot, || {
                String::from_utf8_lossy(&bytes).into_owned()
            });
            crate::usage_attr::hold_ahead_of_chain(
                &state,
                &snapshot,
                &auth,
                Some(target.display_name()),
            );
        }
        audit
            .input(&state, &snapshot, &auth, &target, &bytes, false)
            .await?;
        if matches!(
            action,
            Action::List | Action::ListFiles | Action::BatchFiles
        ) {
            let bytes = list_owned(
                &state,
                &auth,
                &client,
                &target,
                &origin,
                &path,
                query.as_deref(),
                parent.as_ref(),
                member.as_ref(),
            )
            .await?;
            if let Some(parent) = parent.as_ref() {
                crate::attribution::detached(openai::lookup_management(
                    &state,
                    &auth,
                    &client,
                    &parent.raw_id,
                    NativeObjectKind::VectorStore,
                    None,
                ))
                .await?;
                if let Some(batch) = member.as_ref() {
                    require_related(
                        &state,
                        &auth,
                        &client,
                        parent,
                        NativeRelation::VectorStoreBatch,
                        &batch.raw_id,
                        &origin,
                    )
                    .await?;
                }
            } else {
                let current = state.snapshot.load();
                let (_,actual)=openai::resolve_management(&state,&current,&auth,&client,&target.model_entry.value.display_name,NativeObjectKind::VectorStore)?;
                if !same_account(&actual,&origin){return Err(not_found());}
            }
            audit.output(&state, &auth, &target, &bytes.2).await?;
            return Ok((openai::raw_body(bytes.0, &bytes.1, bytes.2), target));
        }
        let relation_operation = match action {
            Action::Attach => Some(
                reserve(
                    &state,
                    &auth,
                    parent_ref()?,
                    NativeRelation::VectorStoreFile,
                    RelationAction::Attach,
                    &files,
                )
                .await?,
            ),
            Action::Detach => Some(
                reserve(
                    &state,
                    &auth,
                    parent_ref()?,
                    NativeRelation::VectorStoreFile,
                    RelationAction::Detach,
                    std::slice::from_ref(member.as_ref().ok_or_else(openai::dependency)?),
                )
                .await?,
            ),
            _ => None,
        };
        let deletion = if matches!(action, Action::Delete) {
            let parent = parent_ref()?;
            let request = ObjectDeletionRequest {
                api_key_id: openai::api_key_id(&auth)?,
                operation_id: Uuid::new_v4(),
                record_id: parent.record_id,
                version: parent.version,
            };
            let reserved = openai::registry(&state)?
                .reserve_delete(request.clone())
                .await
                .map_err(openai::registry_error)?;
            if reserved.state != "reserved" || reserved.operation_id != Some(request.operation_id) {
                return Err(openai::dependency());
            }
            Some(request)
        } else {
            None
        };
        let registration = if matches!(action, Action::Create | Action::CreateBatch) {
            Some(
                begin_creation(
                    &state,
                    &auth,
                    &client,
                    &target,
                    &origin,
                    parent.as_ref(),
                    &files,
                )
                .await?,
            )
        } else {
            None
        };
        pending_operation=relation_operation.or_else(||deletion.as_ref().map(|request|request.operation_id)).or(registration);
        if let Some(operation_id)=pending_operation {audit.registered_operation(operation_id,InferenceStage::Management);}
        let sent = openai::send_at_origin(
            &state,
            &target,
            &client,
            &origin,
            NativeObjectKind::VectorStore,
            action.method(),
            &path,
            &openai::strip_model_query(query.as_deref()),
            if body.is_some() {
                Body::Json(Bytes::from(bytes))
            } else {
                Body::Empty
            },
        )
        .await;
        let reply=match sent {
            Ok(reply)=>reply,
            Err(error)=> {
                let rejected=matches!(&error,ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamStatus {status,..}|aisix_gateway::BridgeError::UpstreamBodyUnread {status,..}) if (400..500).contains(status)&&*status!=408);
                if let Some(operation_id)=pending_operation {
                    resolve_reservation(&state,&auth,operation_id,rejected).await?;
                    pending_operation=None;
                }
                return Err(error);
            }
        };
        let value: Value = serde_json::from_slice(&reply.bytes).map_err(|_| invalid_response())?;
        match action {
            Action::Create => {
                validate(&value, None, "vector_store", None)?;
                bind_creation(
                    &state,
                    &auth,
                    registration.ok_or_else(openai::dependency)?,
                    &origin,
                    None,
                    &files,
                    &value,
                )
                .await?;
            }
            Action::Get | Action::Update => validate(&value, Some(&store), "vector_store", None)?,
            Action::Delete => {
                validate(&value, Some(&store), "vector_store.deleted", None)?;
                if value.get("deleted").and_then(Value::as_bool) != Some(true) {
                    return Err(invalid_response());
                }
                let reply = openai::registry(&state)?
                    .commit_delete(deletion.ok_or_else(openai::dependency)?)
                    .await
                    .map_err(openai::registry_error)?;
                if reply.state != "deleted" {
                    return Err(openai::dependency());
                }
            }
            Action::Attach | Action::GetFile | Action::UpdateFile => {
                let expected = if matches!(action, Action::Attach) {
                    ids.first().map(String::as_str)
                } else {
                    Some(child.as_str())
                };
                validate(&value, expected, "vector_store.file", Some(&store))?;
                if let Some(operation) = relation_operation {
                    commit(&state, &auth, operation, Vec::new()).await?;
                }
            }
            Action::Detach => {
                validate(&value, Some(&child), "vector_store.file.deleted", None)?;
                if value.get("deleted").and_then(Value::as_bool) != Some(true) {
                    return Err(invalid_response());
                }
                commit(
                    &state,
                    &auth,
                    relation_operation.ok_or_else(openai::dependency)?,
                    Vec::new(),
                )
                .await?;
            }
            Action::Search => {
                let data = value
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or_else(invalid_response)?;
                let ids = data
                    .iter()
                    .map(|item| {
                        item.get("file_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .ok_or_else(invalid_response)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let records = related(
                    &state,
                    &auth,
                    &client,
                    parent_ref()?,
                    NativeRelation::VectorStoreFile,
                    &ids,
                    &origin,
                )
                .await?;
                if ids.iter().any(|id| !records.contains_key(id)) {
                    return Err(invalid_response());
                }
            }
            Action::Content => {
                value
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or_else(invalid_response)?;
            }
            Action::CreateBatch => {
                validate(&value, None, "vector_store.files_batch", Some(&store))?;
                // 受理的输入文件、根 batch 及两层成员关系在一个权威事务提交。
                bind_creation(
                    &state,
                    &auth,
                    registration.ok_or_else(openai::dependency)?,
                    &origin,
                    parent.as_ref(),
                    &files,
                    &value,
                )
                .await?;
            }
            Action::GetBatch | Action::CancelBatch => validate(
                &value,
                Some(&child),
                "vector_store.files_batch",
                Some(&store),
            )?,
            Action::List | Action::ListFiles | Action::BatchFiles => {
                return Err(openai::dependency())
            }
        }
        pending_operation=None;
        // 已接受的产物先持久化，释放内容前再以最新授权检查一次。
        if !matches!(action, Action::Delete) {
            let id = if matches!(action, Action::Create) {
                raw_id(&value)?
            } else {
                &store
            };
            crate::attribution::detached(openai::lookup_management(
                &state,
                &auth,
                &client,
                id,
                NativeObjectKind::VectorStore,
                None,
            ))
            .await?;
        } else {
            let current = state.snapshot.load();
            openai::current_auth(&current, &auth)?;
            let (_, actual) = openai::resolve_management(
                &state,
                &current,
                &auth,
                &client,
                &target.model_entry.value.display_name,
                NativeObjectKind::VectorStore,
            )?;
            if !same_account(&actual, &origin) {
                return Err(not_found());
            }
        }
        if matches!(
            action,
            Action::Attach | Action::GetFile | Action::UpdateFile | Action::Content
        ) {
            let id = if matches!(action, Action::Attach) {
                raw_id(&value)?
            } else {
                &child
            };
            require_related(
                &state,
                &auth,
                &client,
                parent_ref()?,
                NativeRelation::VectorStoreFile,
                id,
                &origin,
            )
            .await?;
        }
        if matches!(
            action,
            Action::GetBatch | Action::CancelBatch | Action::CreateBatch
        ) {
            let id = if matches!(action, Action::CreateBatch) {
                raw_id(&value)?
            } else {
                &child
            };
            require_related(
                &state,
                &auth,
                &client,
                parent_ref()?,
                NativeRelation::VectorStoreBatch,
                id,
                &origin,
            )
            .await?;
        }
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    let result = if let Some(operation_id) = pending_operation {
        match resolve_reservation(&state, &auth, operation_id, false).await {
            Ok(()) => result,
            Err(error) => Err(error),
        }
    } else {
        result
    };
    openai::finish(
        &state,
        &snapshot,
        crate::operation::VECTOR_STORES,
        action.method(),
        &public_path,
        &auth,
        &client,
        started,
        result,
        audit,
        InferenceStage::Management,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn list_owned(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    origin: &OriginBinding,
    path: &str,
    query: Option<&str>,
    parent: Option<&NativeRecord>,
    batch: Option<&NativeRecord>,
) -> Result<(StatusCode, axum::http::HeaderMap, Bytes), ProxyError> {
    let mut pairs: Vec<(String, String)> =
        url::form_urlencoded::parse(openai::strip_model_query(query).as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    let limit = pairs
        .iter()
        .find(|(key, _)| key == "limit")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let backwards = pairs.iter().any(|(key, _)| key == "before");
    for (_, id) in pairs
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "after" | "before"))
    {
        if let Some(parent) = parent {
            require_related(
                state,
                auth,
                client,
                parent,
                NativeRelation::VectorStoreFile,
                id,
                origin,
            )
            .await?;
            if let Some(batch) = batch {
                require_related(
                    state,
                    auth,
                    client,
                    batch,
                    NativeRelation::VectorStoreBatchFile,
                    id,
                    origin,
                )
                .await?;
            }
        } else {
            let (_, actual, _) = crate::attribution::detached(openai::lookup_management(
                state,
                auth,
                client,
                id,
                NativeObjectKind::VectorStore,
                None,
            ))
            .await?;
            if !same_account(&actual, origin) {
                return Err(not_found());
            }
        }
    }
    let mut visible = Vec::new();
    let mut seen = HashSet::new();
    let mut envelope = None;
    let mut remaining_pages = 512;
    let (response_status, response_headers) = loop {
        // 无法在总页数内确认可见窗口时失败关闭，不伪造一个已经结束的空页。
        if remaining_pages == 0 {
            return Err(openai::dependency());
        }
        remaining_pages -= 1;
        let query: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&pairs)
            .finish();
        let reply = openai::send_at_origin(
            state,
            target,
            client,
            origin,
            NativeObjectKind::VectorStore,
            Method::GET,
            path,
            &query,
            Body::Empty,
        )
        .await?;
        if !reply.status.is_success() {
            return Ok((reply.status, reply.headers, reply.bytes));
        }
        let mut value: Value =
            serde_json::from_slice(&reply.bytes).map_err(|_| invalid_response())?;
        let data = value
            .get_mut("data")
            .and_then(Value::as_array_mut)
            .ok_or_else(invalid_response)?;
        let ids = data
            .iter()
            .map(|item| raw_id(item).map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?;
        let allowed: HashSet<String> = if let Some(parent) = parent {
            let stores = related(
                state,
                auth,
                client,
                parent,
                NativeRelation::VectorStoreFile,
                &ids,
                origin,
            )
            .await?;
            if let Some(batch) = batch {
                let batches = related(
                    state,
                    auth,
                    client,
                    batch,
                    NativeRelation::VectorStoreBatchFile,
                    &ids,
                    origin,
                )
                .await?;
                stores
                    .into_keys()
                    .filter(|id| batches.contains_key(id))
                    .collect()
            } else {
                stores.into_keys().collect()
            }
        } else {
            let reply = openai::registry(state)?
                .owned(OwnedRequest {
                    api_key_id: openai::api_key_id(auth)?,
                    protocol: NativeProtocol::Openai,
                    kind: NativeObjectKind::VectorStore,
                    origin: origin.clone(),
                    parent_id: None,
                    raw_ids: ids.clone(),
                })
                .await
                .map_err(openai::registry_error)?;
            if reply.state != "owned" {
                return Err(openai::dependency());
            }
            let mut allowed = HashSet::new();
            for record in reply.records {
                if !record.deleted
                    && record.kind == Some(NativeObjectKind::VectorStore)
                    && record.protocol == Some(NativeProtocol::Openai)
                    && record
                        .origin
                        .as_ref()
                        .is_some_and(|actual| same_account(actual, origin))
                    && ids.contains(&record.raw_id)
                {
                    match crate::attribution::detached(openai::lookup_management(
                        state,
                        auth,
                        client,
                        &record.raw_id,
                        NativeObjectKind::VectorStore,
                        None,
                    ))
                    .await
                    {
                        Ok((_, actual, _)) if same_account(&actual, origin) => {
                            allowed.insert(record.raw_id);
                        }
                        Ok(_) | Err(ProxyError::JobResourceNotFound(_)) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            allowed
        };
        // before 的每页仍保持请求顺序；先从靠近游标的一端收集最近的可见项。
        if backwards {
            data.reverse();
        }
        for item in data.drain(..) {
            if allowed.contains(raw_id(&item)?) {
                validate(
                    &item,
                    None,
                    if parent.is_some() {
                        "vector_store.file"
                    } else {
                        "vector_store"
                    },
                    parent.map(|p| p.raw_id.as_str()),
                )?;
                visible.push(item);
            }
            if visible.len() > limit {
                break;
            }
        }
        let has_more = value
            .get("has_more")
            .and_then(Value::as_bool)
            .ok_or_else(invalid_response)?;
        let next = if backwards { ids.first() } else { ids.last() }.cloned();
        if envelope.is_none() {
            envelope = Some(value);
        }
        if visible.len() > limit || !has_more {
            break (reply.status, reply.headers);
        }
        let next = next.ok_or_else(invalid_response)?;
        if !seen.insert(next.clone()) {
            return Err(invalid_response());
        }
        pairs.retain(|(key, _)| key != "after" && key != "before");
        pairs.push((if backwards { "before" } else { "after" }.into(), next));
    };
    let has_more = visible.len() > limit;
    visible.truncate(limit);
    if backwards {
        visible.reverse();
    }
    let first = visible
        .first()
        .and_then(|item| item.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    let last = visible
        .last()
        .and_then(|item| item.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut value = envelope.ok_or_else(invalid_response)?;
    value["data"] = Value::Array(visible);
    value["first_id"] = first;
    value["last_id"] = last;
    value["has_more"] = json!(has_more);
    Ok((
        response_status,
        response_headers,
        Bytes::from(serde_json::to_vec(&value).map_err(|_| invalid_response())?),
    ))
}

macro_rules! root_read {
    ($name:ident,$action:ident) => {
        pub(crate) async fn $name(
            State(state): State<ProxyState>,
            auth: AuthenticatedKey,
            client: ClientContext,
            RawQuery(query): RawQuery,
        ) -> Response {
            run(
                state,
                auth,
                client,
                query,
                Action::$action,
                String::new(),
                String::new(),
                None,
            )
            .await
        }
    };
}
macro_rules! store_read {
    ($name:ident,$action:ident) => {
        pub(crate) async fn $name(
            State(state): State<ProxyState>,
            auth: AuthenticatedKey,
            client: ClientContext,
            AisixPath(id): AisixPath<String>,
            RawQuery(query): RawQuery,
        ) -> Response {
            run(
                state,
                auth,
                client,
                query,
                Action::$action,
                id,
                String::new(),
                None,
            )
            .await
        }
    };
}
macro_rules! child_read {
    ($name:ident,$action:ident) => {
        pub(crate) async fn $name(
            State(state): State<ProxyState>,
            auth: AuthenticatedKey,
            client: ClientContext,
            AisixPath((id, child)): AisixPath<(String, String)>,
            RawQuery(query): RawQuery,
        ) -> Response {
            run(state, auth, client, query, Action::$action, id, child, None).await
        }
    };
}
macro_rules! store_write {
    ($name:ident,$action:ident) => {
        pub(crate) async fn $name(
            State(state): State<ProxyState>,
            auth: AuthenticatedKey,
            client: ClientContext,
            AisixPath(id): AisixPath<String>,
            RawQuery(query): RawQuery,
            body: Result<Json<Value>, JsonRejection>,
        ) -> Response {
            let started = Instant::now();
            let body = match body {
                Ok(Json(value)) => value,
                Err(error) => {
                    return reject_json(
                        &state,
                        &auth,
                        &client,
                        started,
                        &format!("/v1{}", Action::$action.path(&id, "")),
                        error,
                    )
                }
            };
            run(
                state,
                auth,
                client,
                query,
                Action::$action,
                id,
                String::new(),
                Some(body),
            )
            .await
        }
    };
}
root_read!(list, List);
store_read!(get, Get);
store_read!(delete, Delete);
store_read!(list_files, ListFiles);
store_write!(update, Update);
store_write!(search, Search);
store_write!(attach_file, Attach);
store_write!(create_batch, CreateBatch);
child_read!(get_file, GetFile);
child_read!(detach_file, Detach);
child_read!(file_content, Content);
child_read!(get_batch, GetBatch);
child_read!(cancel_batch, CancelBatch);
child_read!(batch_files, BatchFiles);
pub(crate) async fn create(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let body = match body {
        Ok(Json(value)) => value,
        Err(error) => {
            return reject_json(&state, &auth, &client, started, "/v1/vector_stores", error)
        }
    };
    run(
        state,
        auth,
        client,
        query,
        Action::Create,
        String::new(),
        String::new(),
        Some(body),
    )
    .await
}
pub(crate) async fn update_file(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath((id, child)): AisixPath<(String, String)>,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let body = match body {
        Ok(Json(value)) => value,
        Err(error) => {
            return reject_json(
                &state,
                &auth,
                &client,
                started,
                &format!("/v1/vector_stores/{id}/files/{child}"),
                error,
            )
        }
    };
    run(
        state,
        auth,
        client,
        query,
        Action::UpdateFile,
        id,
        child,
        Some(body),
    )
    .await
}

/// 只收集官方工具定义槽；历史结果和引文是内联内容，不产生存储解引用。
pub(crate) fn take_file_search_tool_refs(tool: &mut Value, ids: &mut Vec<String>) {
    if tool.get("type").and_then(Value::as_str) != Some("file_search") {
        return;
    }
    if let Some(stores) = tool.get("vector_store_ids").and_then(Value::as_array) {
        if stores.iter().all(Value::is_string) {
            ids.extend(stores.iter().filter_map(Value::as_str).map(str::to_owned));
            if let Some(object) = tool.as_object_mut() {
                object.remove("vector_store_ids");
            }
        }
    }
}
fn take_tools(value: &mut Value, ids: &mut Vec<String>) {
    if let Some(tools) = value.as_array_mut() {
        for tool in tools {
            take_file_search_tool_refs(tool, ids);
        }
    }
}
pub(crate) fn take_vector_store_references(body: &mut Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(tools) = body.get_mut("tools") {
        take_tools(tools, &mut ids);
    }
    if let Some(choice) = body.get_mut("tool_choice") {
        if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
            if let Some(tools) = choice.get_mut("tools") {
                take_tools(tools, &mut ids);
            }
        }
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in items {
            if matches!(
                item.get("type").and_then(Value::as_str),
                Some("additional_tools" | "tool_search_output")
            ) {
                if let Some(tools) = item.get_mut("tools") {
                    take_tools(tools, &mut ids);
                }
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}
pub(crate) async fn admit_vector_store_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    ids: &[String],
    expected: &OriginBinding,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    for id in ids {
        let (_, actual, _) = crate::attribution::detached(openai::lookup_reference(
            state,
            auth,
            client,
            id,
            NativeObjectKind::VectorStore,
            None,
            consumer,
        ))
        .await?;
        if !same_account(&actual, expected) {
            return Err(not_found());
        }
    }
    Ok(())
}

/// 已授权向量库将真正推理固定到同一账号；管理 family 不从消费行为继承。
pub(crate) async fn admit_responses_affinity(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    affinity: &mut crate::responses_affinity::ContextAffinity,
) -> Result<(), ProxyError> {
    if state.platform().is_none() {
        return Ok(());
    }
    let ids = take_vector_store_references(&mut body.clone());
    for id in ids {
        let (_, origin, _) = crate::attribution::detached(openai::lookup_reference(
            state,
            auth,
            client,
            &id,
            NativeObjectKind::VectorStore,
            None,
            EndpointFamily::Responses,
        ))
        .await?;
        affinity.admit_object_origin(origin)?;
    }
    Ok(())
}
