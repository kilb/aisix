//! 容器保留上游原始标识；文件成员关系在返回之前由持久归属服务确认。

use std::collections::HashSet;
use std::time::Instant;

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{
    AcceptedReference, BeginRelatedRequest, BeginRequest, BindRelatedRequest, BindRequest,
    CommitRelationsRequest, InferenceStage, LookupRelationsRequest, NativeChildObject,
    NativeObjectKind, NativeProtocol, NativeRecord, NativeRelation, ObjectDeletionRequest,
    OriginBinding, OwnedRequest, RelationAction, ReserveRelationsRequest,
    ResolveReservationRequest,
};
use aisix_core::AisixSnapshot;
use axum::extract::{FromRequest, Multipart, RawQuery, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;
use uuid::Uuid;

use super::openai::{self, Audit, Body};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::jobs::JobTarget;
use crate::reject::AisixPath;
use crate::request_json::{Json, JsonRejection};
use crate::state::ProxyState;

const CONTAINER: NativeObjectKind = NativeObjectKind::ResponseContainer;
const FILE: NativeObjectKind = NativeObjectKind::ContainerFile;
const MAX_LIST_PAGES: usize = 256;

fn missing() -> ProxyError {
    ProxyError::JobResourceNotFound("container".into())
}

fn same_account(left: &OriginBinding, right: &OriginBinding) -> bool {
    left.provider_key_id == right.provider_key_id && left.boundary == right.boundary
}

fn identity<'a>(
    value: &'a Value,
    object: &str,
    expected: Option<&str>,
) -> Result<&'a str, ProxyError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(openai::invalid_identity)?;
    crate::jobs::require_safe_upstream_id(id).map_err(|_| openai::invalid_identity())?;
    if value.get("object").and_then(Value::as_str) != Some(object)
        || expected.is_some_and(|expected| expected != id)
    {
        return Err(openai::invalid_identity());
    }
    Ok(id)
}

fn file_identity<'a>(
    value: &'a Value,
    parent: &str,
    expected: Option<&str>,
) -> Result<&'a str, ProxyError> {
    let id = identity(value, "container.file", expected)?;
    if value.get("container_id").and_then(Value::as_str) != Some(parent) {
        return Err(openai::invalid_identity());
    }
    Ok(id)
}

fn decode(bytes: &[u8]) -> Result<Value, ProxyError> {
    serde_json::from_slice(bytes).map_err(|_| openai::invalid_identity())
}

fn selector(
    query: Option<&str>,
    client: &ClientContext,
    body: Option<&mut Value>,
) -> Result<String, ProxyError> {
    // 官方容器创建体没有model；该扩展字段只作为网关选路提示。
    let body_hint = body
        .and_then(Value::as_object_mut)
        .and_then(|body| body.remove("model"))
        .and_then(|value| value.as_str().map(str::to_owned));
    openai::model_hint(query, client)
        .or(body_hint)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            ProxyError::InvalidRequest(
                "pass model or x-aisix-model to select a direct model".into(),
            )
        })
}

fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

fn page_query(query: Option<&str>, after: Option<&str>) -> String {
    let mut pairs: Vec<&str> = query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            url::form_urlencoded::parse(pair.as_bytes())
                .next()
                .is_none_or(|(name, _)| name != "model" && name != "after")
        })
        .collect();
    let cursor = after.map(|after| {
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("after", after)
            .finish()
    });
    if let Some(cursor) = cursor.as_deref() {
        pairs.push(cursor);
    }
    pairs.join("&")
}

#[allow(clippy::too_many_arguments)]
async fn finish(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    started: Instant,
    method: Method,
    route: &str,
    result: Result<(Response, JobTarget), ProxyError>,
    audit: Audit,
) -> Response {
    openai::finish(
        state,
        snapshot,
        crate::operation::CONTAINERS,
        method,
        route,
        auth,
        client,
        started,
        result,
        audit,
        InferenceStage::Management,
        None,
    )
    .await
}

fn reject_json(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    started: Instant,
    route: &str,
    error: JsonRejection,
) -> Response {
    crate::reject::reject_before_dispatch(
        state,
        "POST",
        route,
        &client.request_id,
        Some(&auth.entry.id),
        started,
        crate::reject::Envelope::OpenAi,
        crate::error::proxy_error_from_json_rejection(error, state.request_body_limit_for(route)),
    )
}

async fn input(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    audit: &mut Audit,
    bytes: &[u8],
    requested: &str,
) -> Result<(), ProxyError> {
    crate::usage_attr::hold_ahead_of_chain(state, snapshot, auth, Some(requested));
    audit
        .input(state, snapshot, auth, target, bytes, false)
        .await?;
    Ok(())
}

async fn file_reference(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    origin: &OriginBinding,
) -> Result<AcceptedReference, ProxyError> {
    let (_, actual, record) = crate::attribution::detached(openai::lookup_file_reference(
        state,
        auth,
        client,
        raw,
        EndpointFamily::Containers,
    ))
    .await?;
    if !same_account(&actual, origin) {
        return Err(missing());
    }
    Ok(AcceptedReference {
        record_id: record.record_id,
        consumer_family: EndpointFamily::Containers,
    })
}

async fn create_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    origin: &OriginBinding,
) -> Result<Vec<AcceptedReference>, ProxyError> {
    let mut references = Vec::new();
    if let Some(ids) = body.get("file_ids").and_then(Value::as_array) {
        for id in ids.iter().filter_map(Value::as_str) {
            references.push(file_reference(state, auth, client, id, origin).await?);
        }
    }
    references.extend(
        super::skills::collect_openai_skill_references(
            state,
            auth,
            client,
            body,
            origin,
            EndpointFamily::Containers,
        )
        .await?,
    );
    references.sort_by_key(|reference| reference.record_id);
    references.dedup_by_key(|reference| reference.record_id);
    Ok(references)
}

pub(crate) async fn create_container(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let route = "/v1/containers";
    let mut body = match body {
        Ok(Json(body)) => body,
        Err(error) => return reject_json(&state, &auth, &client, started, route, error),
    };
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let wanted = selector(query.as_deref(), &client, Some(&mut body))?;
        let (target, origin) =
            openai::resolve_management(&state, &snapshot, &auth, &client, &wanted, CONTAINER)?;
        audit.resolved(&target);
        crate::budget::gate(&state, &snapshot).check_provider_key(&target.pk_entry.id)?;
        let references = create_references(&state, &auth, &client, &body, &origin).await?;
        let _quota = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let bytes = Bytes::from(body.to_string());
        crate::usage_attr::note_failure_request(&snapshot, || body.to_string());
        input(
            &state, &snapshot, &auth, &target, &mut audit, &bytes, &wanted,
        )
        .await?;
        let registry = openai::registry(&state)?;
        let api_key_id = openai::api_key_id(&auth)?;
        let operation_id = Uuid::new_v4();
        let begin = registry
            .begin_related(BeginRelatedRequest {
                lifecycle_context: None,
                entry_references: Default::default(),
                submission: BeginRequest {
                    usage_context: None,
                    upstream_input_text: None,
                    input_file_id: None,
                    operation_id,
                    api_key_id,
                    protocol: NativeProtocol::Openai,
                    kind: CONTAINER,
                    origin: origin.clone(),
                    parent_id: None,
                    requested_model_id: origin.model_id,
                    requested_model: wanted,
                    upstream_model: target
                        .model_entry
                        .value
                        .upstream_model()
                        .unwrap_or_default()
                        .into(),
                    upstream_provider: target.pk_entry.value.provider.clone(),
                    operation: "containers".into(),
                    request_id: client.request_id.clone(),
                    asynchronous: false,
                    requested_service_tier: None,
                    entries: Vec::new(),
                    observation_headers: Default::default(),
                },
                references,
                owner_parent_record_id: None,
            })
            .await
            .map_err(openai::registry_error)?;
        if begin.state != "pending_submission"
            || begin.operation_id != Some(operation_id)
            || begin.origin.as_ref() != Some(&origin)
        {
            return Err(openai::dependency());
        }
        audit.registered_operation(operation_id, InferenceStage::Management);
        let reply = openai::send_at_origin(
            &state,
            &target,
            &client,
            &origin,
            CONTAINER,
            Method::POST,
            "/containers",
            &openai::strip_model_query(query.as_deref()),
            Body::Json(bytes),
        )
        .await?;
        let value = decode(&reply.bytes)?;
        let raw = identity(&value, "container", None)?;
        let bound = registry
            .bind_related(BindRelatedRequest {
                binding: BindRequest {
                    existing_record_id: None,
                    objects: Vec::new(),
                    operation_id,
                    api_key_id,
                    raw_id: raw.into(),
                    provider_created_at: value
                        .get("created_at")
                        .and_then(Value::as_i64)
                        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
                    provider_expires_at: None,
                },
                relations: Vec::new(),
            })
            .await
            .map_err(|_| openai::dependency())?;
        if !bound.record.as_ref().is_some_and(|record| {
            bound.state == "bound"
                && record.operation_id == Some(operation_id)
                && record.raw_id == raw
                && record.kind == Some(CONTAINER)
                && record.protocol == Some(NativeProtocol::Openai)
                && record.origin.as_ref() == Some(&origin)
                && !record.deleted
        }) {
            return Err(openai::dependency());
        }
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    finish(
        &state,
        &snapshot,
        &auth,
        &client,
        started,
        Method::POST,
        route,
        result,
        audit,
    )
    .await
}

async fn reserve(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    parent: &NativeRecord,
    action: RelationAction,
    children: Vec<Uuid>,
    references: Vec<AcceptedReference>,
) -> Result<Uuid, ProxyError> {
    let operation_id = Uuid::new_v4();
    let reply = openai::registry(state)?
        .reserve_relations(ReserveRelationsRequest {
            api_key_id: openai::api_key_id(auth)?,
            operation_id,
            parent_record_id: parent.record_id,
            expected_parent_version: parent.version,
            relation: NativeRelation::ContainerFile,
            action,
            child_record_ids: children,
            references,
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state != "reserved"
        || reply.operation_id != Some(operation_id)
        || !reply.record.as_ref().is_some_and(|record| {
            record.record_id == parent.record_id
                && record.version == parent.version
                && record.origin == parent.origin
                && !record.deleted
        })
    {
        return Err(openai::dependency());
    }
    Ok(operation_id)
}

async fn rejected(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    operation_id: Uuid,
    error: &ProxyError,
) {
    if matches!(
        error,
        ProxyError::Bridge(
            aisix_gateway::BridgeError::UpstreamStatus { .. }
                | aisix_gateway::BridgeError::UpstreamBodyUnread { .. }
        )
    ) {
        if let Ok(registry) = openai::registry(state) {
            let _ = registry
                .resolve_reservation(ResolveReservationRequest {
                    api_key_id: match openai::api_key_id(auth) {
                        Ok(id) => id,
                        Err(_) => return,
                    },
                    operation_id,
                    upstream_rejected: true,
                })
                .await;
        }
    }
}

async fn commit(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    parent: &NativeRecord,
    operation_id: Uuid,
    ids: &[String],
) -> Result<(NativeRecord, Vec<NativeRecord>), ProxyError> {
    let reply = openai::registry(state)?
        .commit_relations(CommitRelationsRequest {
            api_key_id: openai::api_key_id(auth)?,
            operation_id,
            objects: ids
                .iter()
                .map(|raw| NativeChildObject {
                    kind: FILE,
                    raw_id: raw.clone(),
                })
                .collect(),
        })
        .await
        .map_err(|_| openai::dependency())?;
    let origin = parent.origin.as_ref().ok_or_else(openai::dependency)?;
    if reply.state != "committed" || reply.operation_id != Some(operation_id) {
        return Err(openai::dependency());
    }
    for id in ids {
        if !reply.records.iter().any(|record| {
            record.raw_id == *id
                && record.kind == Some(FILE)
                && !record.deleted
                && record.protocol == Some(NativeProtocol::Openai)
                && record
                    .origin
                    .as_ref()
                    .is_some_and(|actual| same_account(actual, origin))
                && reply.memberships.iter().any(|membership| {
                    membership.parent_record_id == parent.record_id
                        && membership.child_record_id == record.record_id
                        && membership.relation == NativeRelation::ContainerFile
                })
        }) {
            return Err(openai::dependency());
        }
    }
    let updated = reply
        .record
        .filter(|record| {
            record.record_id == parent.record_id
                && !record.deleted
                && record.origin.as_ref() == Some(origin)
                && parent.version.checked_add(1) == Some(record.version)
        })
        .ok_or_else(openai::dependency)?;
    Ok((updated, reply.records))
}

async fn member(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    parent: &NativeRecord,
    raw: &str,
) -> Result<Option<NativeRecord>, ProxyError> {
    crate::jobs::require_safe_upstream_id(raw).map_err(|_| missing())?;
    let reply = openai::registry(state)?
        .lookup_relations(LookupRelationsRequest {
            api_key_id: openai::api_key_id(auth)?,
            parent_record_id: parent.record_id,
            relation: NativeRelation::ContainerFile,
            raw_ids: vec![raw.into()],
            consumer_family: None,
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state != "related" {
        return Err(openai::dependency());
    }
    let Some(record) = reply
        .records
        .into_iter()
        .find(|record| record.raw_id == raw)
    else {
        return Ok(None);
    };
    let origin = parent.origin.as_ref().ok_or_else(openai::dependency)?;
    if record.deleted
        || record.kind != Some(FILE)
        || record.protocol != Some(NativeProtocol::Openai)
        || !record
            .origin
            .as_ref()
            .is_some_and(|actual| same_account(actual, origin))
        || !reply.memberships.iter().any(|membership| {
            membership.parent_record_id == parent.record_id
                && membership.child_record_id == record.record_id
                && membership.relation == NativeRelation::ContainerFile
        })
    {
        return Err(missing());
    }
    // 子对象自身的原始模型授权仍需当前快照验证；关系不是授权的替代物。
    crate::attribution::detached(openai::lookup_management(
        state,
        auth,
        client,
        raw,
        FILE,
        record.parent_id,
    ))
    .await?;
    Ok(Some(record))
}

pub(crate) async fn list_containers(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
) -> Response {
    list(state, auth, client, None, query).await
}

pub(crate) async fn list_files(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    list(state, auth, client, Some(id), query).await
}

async fn list(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    parent_id: Option<String>,
    query: Option<String>,
) -> Response {
    let started = Instant::now();
    crate::attribution::note_handler_started(started);
    let route = parent_id.as_ref().map_or_else(
        || "/v1/containers".into(),
        |id| format!("/v1/containers/{id}/files"),
    );
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let (target, origin, mut parent) = if let Some(id) = parent_id.as_deref() {
            let (target, origin, parent) =
                openai::lookup_management(&state, &auth, &client, id, CONTAINER, None).await?;
            (target, origin, Some(parent))
        } else {
            let wanted = selector(query.as_deref(), &client, None)?;
            let (target, origin) =
                openai::resolve_management(&state, &snapshot, &auth, &client, &wanted, CONTAINER)?;
            (target, origin, None)
        };
        audit.resolved(&target);
        let _quota = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        input(
            &state,
            &snapshot,
            &auth,
            &target,
            &mut audit,
            &[],
            &target.model_entry.value.display_name,
        )
        .await?;
        let mut after = query_value(query.as_deref(), "after");
        if let Some(raw) = after.as_deref() {
            if let Some(parent) = &parent {
                if member(&state, &auth, &client, parent, raw).await?.is_none() {
                    return Err(missing());
                }
            } else {
                let (_, actual, _) = crate::attribution::detached(openai::lookup_management(
                    &state, &auth, &client, raw, CONTAINER, None,
                ))
                .await?;
                if !same_account(&origin, &actual) {
                    return Err(missing());
                }
            }
        }
        let limit = query_value(query.as_deref(), "limit")
            .and_then(|limit| limit.parse::<usize>().ok())
            .filter(|limit| (1..=100).contains(limit))
            .unwrap_or(20);
        let upstream_path = parent_id.as_ref().map_or_else(
            || "/containers".into(),
            |id| format!("/containers/{id}/files"),
        );
        let mut seen = HashSet::new();
        if let Some(after) = &after {
            seen.insert(after.clone());
        }
        let mut visible = Vec::new();
        let mut row_ids = HashSet::new();
        let mut first_page = None;
        let mut headers = HeaderMap::new();
        let mut status = StatusCode::OK;
        let mut done = false;
        for _ in 0..MAX_LIST_PAGES {
            let reservation = if let Some(parent) = &parent {
                Some(
                    reserve(
                        &state,
                        &auth,
                        parent,
                        RelationAction::Discover,
                        Vec::new(),
                        Vec::new(),
                    )
                    .await?,
                )
            } else {
                None
            };
            if let Some(operation) = reservation {
                audit.registered_operation(operation, InferenceStage::Management);
            }
            let reply = match openai::send_at_origin(
                &state,
                &target,
                &client,
                &origin,
                CONTAINER,
                Method::GET,
                &upstream_path,
                &page_query(query.as_deref(), after.as_deref()),
                Body::Empty,
            )
            .await
            {
                Ok(reply) => reply,
                Err(error) => {
                    if let Some(operation) = reservation {
                        rejected(&state, &auth, operation, &error).await;
                    }
                    return Err(error);
                }
            };
            let parsed = (|| {
                let page = decode(&reply.bytes)?;
                if page.get("object").and_then(Value::as_str) != Some("list") {
                    return Err(openai::invalid_identity());
                }
                let rows = page
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or_else(openai::invalid_identity)?;
                let mut ids = Vec::new();
                for row in rows {
                    ids.push(
                        if let Some(id) = parent_id.as_deref() {
                            file_identity(row, id, None)?
                        } else {
                            identity(row, "container", None)?
                        }
                        .to_owned(),
                    );
                }
                if ids.iter().any(|id| !row_ids.insert(id.clone())) {
                    return Err(openai::invalid_identity());
                }
                let more = page
                    .get("has_more")
                    .and_then(Value::as_bool)
                    .ok_or_else(openai::invalid_identity)?;
                Ok::<_, ProxyError>((page.clone(), rows.clone(), ids, more))
            })();
            let (page, rows, ids, more) = match parsed {
                Ok(page) => page,
                Err(error) => {
                    if let (Some(operation), Some(parent)) = (reservation, parent.as_ref()) {
                        let _ = commit(&state, &auth, parent, operation, &[]).await;
                    }
                    return Err(error);
                }
            };
            let allowed: HashSet<String> = if let (Some(operation), Some(current)) =
                (reservation, parent.as_ref())
            {
                let (updated, records) = commit(&state, &auth, current, operation, &ids).await?;
                parent = Some(updated);
                let mut allowed = HashSet::new();
                for record in records {
                    match crate::attribution::detached(openai::lookup_management(
                        &state,
                        &auth,
                        &client,
                        &record.raw_id,
                        FILE,
                        record.parent_id,
                    ))
                    .await
                    {
                        Ok(_) => {
                            allowed.insert(record.raw_id);
                        }
                        Err(ProxyError::JobResourceNotFound(_) | ProxyError::ResponseNotFound) => {}
                        Err(error) => return Err(error),
                    }
                }
                allowed
            } else {
                let owned = openai::registry(&state)?
                    .owned(OwnedRequest {
                        api_key_id: openai::api_key_id(&auth)?,
                        protocol: NativeProtocol::Openai,
                        kind: CONTAINER,
                        origin: origin.clone(),
                        parent_id: None,
                        raw_ids: ids.clone(),
                    })
                    .await
                    .map_err(openai::registry_error)?;
                if owned.state != "owned" {
                    return Err(openai::dependency());
                }
                let mut allowed = HashSet::new();
                for record in owned.records {
                    if !ids.contains(&record.raw_id)
                        || record.deleted
                        || record.kind != Some(CONTAINER)
                        || record.protocol != Some(NativeProtocol::Openai)
                        || !record
                            .origin
                            .as_ref()
                            .is_some_and(|actual| same_account(actual, &origin))
                    {
                        return Err(openai::dependency());
                    }
                    match crate::attribution::detached(openai::lookup_management(
                        &state,
                        &auth,
                        &client,
                        &record.raw_id,
                        CONTAINER,
                        None,
                    ))
                    .await
                    {
                        Ok(_) => {
                            allowed.insert(record.raw_id);
                        }
                        Err(ProxyError::JobResourceNotFound(_) | ProxyError::ResponseNotFound) => {}
                        Err(error) => return Err(error),
                    }
                }
                allowed
            };
            if first_page.is_none() {
                first_page = Some(page);
                headers = reply.headers;
                status = reply.status;
            }
            visible.extend(rows.into_iter().filter(|row| {
                row.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| allowed.contains(id))
            }));
            if visible.len() > limit {
                done = true;
                break;
            }
            if !more {
                done = true;
                break;
            }
            let cursor = ids.last().cloned().ok_or_else(openai::invalid_identity)?;
            if !seen.insert(cursor.clone()) {
                return Err(openai::invalid_identity());
            }
            after = Some(cursor);
        }
        if !done {
            return Err(openai::dependency());
        }
        let has_more = visible.len() > limit;
        visible.truncate(limit);
        let mut page = first_page.ok_or_else(openai::invalid_identity)?;
        page["first_id"] = visible
            .first()
            .and_then(|row| row.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        page["last_id"] = visible
            .last()
            .and_then(|row| row.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        page["has_more"] = Value::Bool(has_more);
        page["data"] = Value::Array(visible);
        let bytes = Bytes::from(page.to_string());
        audit.output(&state, &auth, &target, &bytes).await?;
        Ok((openai::raw_body(status, &headers, bytes), target))
    }
    .await;
    finish(
        &state,
        &snapshot,
        &auth,
        &client,
        started,
        Method::GET,
        &route,
        result,
        audit,
    )
    .await
}

pub(crate) async fn retrieve_container(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    container_call(state, auth, client, id, query, false).await
}

pub(crate) async fn delete_container(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    container_call(state, auth, client, id, query, true).await
}

fn empty_delete(reply: &openai::Reply) -> Result<(), ProxyError> {
    if !matches!(reply.status, StatusCode::OK | StatusCode::NO_CONTENT) || !reply.bytes.is_empty() {
        return Err(openai::invalid_identity());
    }
    Ok(())
}

async fn container_call(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    id: String,
    query: Option<String>,
    delete: bool,
) -> Response {
    let started = Instant::now();
    crate::attribution::note_handler_started(started);
    let route = format!("/v1/containers/{id}");
    let method = if delete { Method::DELETE } else { Method::GET };
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let (target, origin, record) =
            openai::lookup_management(&state, &auth, &client, &id, CONTAINER, None).await?;
        audit.resolved(&target);
        let _quota = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        input(
            &state,
            &snapshot,
            &auth,
            &target,
            &mut audit,
            &[],
            &record.requested_model,
        )
        .await?;
        let deletion = if delete {
            let deletion = ObjectDeletionRequest {
                api_key_id: openai::api_key_id(&auth)?,
                operation_id: Uuid::new_v4(),
                record_id: record.record_id,
                version: record.version,
            };
            let reply = openai::registry(&state)?
                .reserve_delete(deletion.clone())
                .await
                .map_err(openai::registry_error)?;
            if reply.state != "reserved"
                || reply.operation_id != Some(deletion.operation_id)
                || !reply.record.as_ref().is_some_and(|record| {
                    record.record_id == deletion.record_id
                        && record.version == deletion.version
                        && !record.deleted
                })
            {
                return Err(openai::dependency());
            }
            Some(deletion)
        } else {
            None
        };
        if let Some(deletion) = &deletion {
            audit.registered_operation(deletion.operation_id, InferenceStage::Management);
        }
        let reply = match openai::send_at_origin(
            &state,
            &target,
            &client,
            &origin,
            CONTAINER,
            method.clone(),
            &format!("/containers/{id}"),
            &openai::strip_model_query(query.as_deref()),
            Body::Empty,
        )
        .await
        {
            Ok(reply) => reply,
            Err(error) => {
                if let Some(deletion) = deletion {
                    rejected(&state, &auth, deletion.operation_id, &error).await;
                }
                return Err(error);
            }
        };
        if let Some(deletion) = deletion {
            empty_delete(&reply)?;
            let committed = openai::registry(&state)?
                .commit_delete(deletion.clone())
                .await
                .map_err(|_| openai::dependency())?;
            if committed.state != "deleted" {
                return Err(openai::dependency());
            }
        } else {
            identity(&decode(&reply.bytes)?, "container", Some(&id))?;
        }
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    finish(
        &state, &snapshot, &auth, &client, started, method, &route, result, audit,
    )
    .await
}

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    headers: HeaderMap,
    bytes: Bytes,
}

async fn multipart(
    state: &ProxyState,
    route: &str,
    request: Request,
) -> Result<Vec<Part>, ProxyError> {
    let mut multipart = Multipart::from_request(request, state)
        .await
        .map_err(|_| ProxyError::InvalidRequest("invalid multipart form data".into()))?;
    let mut parts = Vec::new();
    while let Some(field) = multipart.next_field().await.map_err(|error| {
        crate::error::proxy_error_from_multipart(
            error,
            state.request_body_limit_for(route),
            "malformed multipart body",
        )
    })? {
        let name = field
            .name()
            .ok_or_else(|| ProxyError::InvalidRequest("multipart field name missing".into()))?
            .to_owned();
        let filename = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        let mut headers = field.headers().clone();
        for header in [
            header::CONTENT_DISPOSITION,
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
        ] {
            headers.remove(header);
        }
        let bytes = field.bytes().await.map_err(|error| {
            crate::error::proxy_error_from_multipart(
                error,
                state.request_body_limit_for(route),
                "malformed multipart field",
            )
        })?;
        parts.push(Part {
            name,
            filename,
            content_type,
            headers,
            bytes,
        });
    }
    Ok(parts)
}

pub(crate) async fn create_file(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    let started = Instant::now();
    crate::attribution::note_handler_started(started);
    let route = format!("/v1/containers/{id}/files");
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        // 父授权在读取上传正文之前，未知容器不能消耗共享账号或接收私有文件。
        let (target, origin, parent) =
            openai::lookup_management(&state, &auth, &client, &id, CONTAINER, None).await?;
        audit.resolved(&target);
        let _quota = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let mut references = Vec::new();
        let is_multipart = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("multipart/form-data"))
            });
        let body = if is_multipart {
            let parts = multipart(&state, &route, request).await?;
            let captured = openai::captured_multipart_fields(
                &parts
                    .iter()
                    .map(|part| {
                        (
                            part.name.clone(),
                            part.filename.clone(),
                            part.content_type.clone(),
                            part.bytes.clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
                &["file"],
            );
            crate::usage_attr::note_failure_request(&snapshot, || captured.clone());
            crate::usage_attr::hold_ahead_of_chain(
                &state,
                &snapshot,
                &auth,
                Some(&parent.requested_model),
            );
            if parts.is_empty() {
                input(
                    &state,
                    &snapshot,
                    &auth,
                    &target,
                    &mut audit,
                    &[],
                    &parent.requested_model,
                )
                .await?;
            }
            let mut form = reqwest::multipart::Form::new();
            for part in parts {
                if part.name == "file_id" {
                    let raw = std::str::from_utf8(&part.bytes)
                        .map_err(|_| ProxyError::InvalidRequest("file_id must be UTF-8".into()))?;
                    references.push(file_reference(&state, &auth, &client, raw, &origin).await?);
                }
                let masked = audit
                    .input(
                        &state,
                        &snapshot,
                        &auth,
                        &target,
                        &part.bytes,
                        part.name == "file" || part.filename.is_some(),
                    )
                    .await?;
                if masked
                    .as_ref()
                    .is_some_and(|masked| masked.as_slice() != part.bytes.as_ref())
                {
                    return Err(crate::error::guardrail_block_error("request", None, None));
                }
                let mut outgoing = reqwest::multipart::Part::stream_with_length(
                    part.bytes.clone(),
                    part.bytes.len() as u64,
                );
                if let Some(filename) = part.filename {
                    outgoing = outgoing.file_name(filename);
                }
                if let Some(content_type) = part.content_type {
                    outgoing = outgoing.mime_str(&content_type).map_err(|_| {
                        ProxyError::InvalidRequest("invalid part content-type".into())
                    })?;
                }
                form = form.part(part.name, outgoing.headers(part.headers));
            }
            if audit.input_bypassed() {
                crate::usage_attr::note_failure_request(&snapshot, String::new);
            } else {
                crate::usage_attr::note_failure_request(&snapshot, || captured);
            }
            crate::usage_attr::release_failure_request();
            Body::Multipart(form)
        } else {
            let Json(body) =
                Json::<Value>::from_request(request, &state)
                    .await
                    .map_err(|error| {
                        crate::error::proxy_error_from_json_rejection(
                            error,
                            state.request_body_limit_for(&route),
                        )
                    })?;
            if let Some(raw) = body.get("file_id").and_then(Value::as_str) {
                references.push(file_reference(&state, &auth, &client, raw, &origin).await?);
            }
            let bytes = Bytes::from(body.to_string());
            crate::usage_attr::note_failure_request(&snapshot, || body.to_string());
            input(
                &state,
                &snapshot,
                &auth,
                &target,
                &mut audit,
                &bytes,
                &parent.requested_model,
            )
            .await?;
            Body::Json(bytes)
        };
        let operation = reserve(
            &state,
            &auth,
            &parent,
            RelationAction::Attach,
            Vec::new(),
            references,
        )
        .await?;
        audit.registered_operation(operation, InferenceStage::Management);
        let reply = match openai::send_at_origin(
            &state,
            &target,
            &client,
            &origin,
            CONTAINER,
            Method::POST,
            &format!("/containers/{id}/files"),
            &openai::strip_model_query(query.as_deref()),
            body,
        )
        .await
        {
            Ok(reply) => reply,
            Err(error) => {
                rejected(&state, &auth, operation, &error).await;
                return Err(error);
            }
        };
        let value = decode(&reply.bytes)?;
        let raw = file_identity(&value, &id, None)?;
        commit(&state, &auth, &parent, operation, &[raw.into()]).await?;
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    finish(
        &state,
        &snapshot,
        &auth,
        &client,
        started,
        Method::POST,
        &route,
        result,
        audit,
    )
    .await
}

/// 未知生成文件仅能通过已授权父容器的准确metadata端点发现，二进制自身不能授予归属。
#[allow(clippy::too_many_arguments)]
async fn load_file(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    parent: &NativeRecord,
    id: &str,
    file: &str,
    query: &str,
    audit: &mut Audit,
) -> Result<openai::Reply, ProxyError> {
    let reservation = if member(state, auth, client, parent, file).await?.is_none() {
        Some(
            reserve(
                state,
                auth,
                parent,
                RelationAction::Discover,
                Vec::new(),
                Vec::new(),
            )
            .await?,
        )
    } else {
        None
    };
    if let Some(operation) = reservation {
        audit.registered_operation(operation, InferenceStage::Management);
    }
    let reply = match openai::send_at_origin(
        state,
        target,
        client,
        parent.origin.as_ref().ok_or_else(openai::dependency)?,
        CONTAINER,
        Method::GET,
        &format!("/containers/{id}/files/{file}"),
        query,
        Body::Empty,
    )
    .await
    {
        Ok(reply) => reply,
        Err(error) => {
            if let Some(operation) = reservation {
                rejected(state, auth, operation, &error).await;
            }
            return Err(error);
        }
    };
    let checked =
        decode(&reply.bytes).and_then(|value| file_identity(&value, id, Some(file)).map(|_| ()));
    if let Err(error) = checked {
        if let Some(operation) = reservation {
            let _ = commit(state, auth, parent, operation, &[]).await;
        }
        return Err(error);
    }
    if let Some(operation) = reservation {
        commit(state, auth, parent, operation, &[file.into()]).await?;
    }
    Ok(reply)
}

pub(crate) async fn retrieve_file(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath((id, file)): AisixPath<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    file_call(state, auth, client, id, file, query, false, false).await
}
pub(crate) async fn delete_file(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath((id, file)): AisixPath<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    file_call(state, auth, client, id, file, query, true, false).await
}
pub(crate) async fn file_content(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath((id, file)): AisixPath<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    file_call(state, auth, client, id, file, query, false, true).await
}

#[allow(clippy::too_many_arguments)]
async fn file_call(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    id: String,
    file: String,
    query: Option<String>,
    delete: bool,
    content: bool,
) -> Response {
    let started = Instant::now();
    crate::attribution::note_handler_started(started);
    let route = format!(
        "/v1/containers/{id}/files/{file}{}",
        if content { "/content" } else { "" }
    );
    let method = if delete { Method::DELETE } else { Method::GET };
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let (target, origin, parent) =
            openai::lookup_management(&state, &auth, &client, &id, CONTAINER, None).await?;
        audit.resolved(&target);
        let _quota = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        input(
            &state,
            &snapshot,
            &auth,
            &target,
            &mut audit,
            &[],
            &parent.requested_model,
        )
        .await?;
        let query = openai::strip_model_query(query.as_deref());
        let reply = if delete {
            let child = member(&state, &auth, &client, &parent, &file)
                .await?
                .ok_or_else(missing)?;
            let operation = reserve(
                &state,
                &auth,
                &parent,
                RelationAction::Detach,
                vec![child.record_id],
                Vec::new(),
            )
            .await?;
            audit.registered_operation(operation, InferenceStage::Management);
            let reply = match openai::send_at_origin(
                &state,
                &target,
                &client,
                &origin,
                CONTAINER,
                Method::DELETE,
                &format!("/containers/{id}/files/{file}"),
                &query,
                Body::Empty,
            )
            .await
            {
                Ok(reply) => reply,
                Err(error) => {
                    rejected(&state, &auth, operation, &error).await;
                    return Err(error);
                }
            };
            empty_delete(&reply)?;
            commit(&state, &auth, &parent, operation, &[]).await?;
            reply
        } else if content {
            if member(&state, &auth, &client, &parent, &file)
                .await?
                .is_none()
            {
                load_file(
                    &state, &auth, &client, &target, &parent, &id, &file, "", &mut audit,
                )
                .await?;
            }
            openai::send_at_origin(
                &state,
                &target,
                &client,
                &origin,
                CONTAINER,
                Method::GET,
                &format!("/containers/{id}/files/{file}/content"),
                &query,
                Body::Empty,
            )
            .await?
        } else {
            load_file(
                &state, &auth, &client, &target, &parent, &id, &file, &query, &mut audit,
            )
            .await?
        };
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    finish(
        &state, &snapshot, &auth, &client, started, method, &route, result, audit,
    )
    .await
}
