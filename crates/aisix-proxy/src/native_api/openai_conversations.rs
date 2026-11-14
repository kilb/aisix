//! Conversations 保留原始协议；父集合、引用及新项目的归属均由持久 authority 证明。

use std::collections::BTreeSet;
use std::time::Instant;

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{
    AcceptedReference, CommitRelationsRequest, InferenceStage, LookupRelationsRequest,
    NativeChildObject, NativeObjectKind, NativeProtocol, NativeRecord, NativeRelation,
    ObjectDeletionRequest, OriginBinding, RelationAction, ReserveRelationsRequest,
    ResolveReservationRequest,
};
use aisix_core::AisixSnapshot;
use axum::extract::{RawQuery, State};
use axum::http::Method;
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

const FAMILY: EndpointFamily = EndpointFamily::Conversations;
const KIND: NativeObjectKind = NativeObjectKind::Conversation;
const RELATION: NativeRelation = NativeRelation::ConversationItem;

fn missing() -> ProxyError {
    ProxyError::JobResourceNotFound("conversation".into())
}

fn safe_id(value: &Value) -> Result<&str, ProxyError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(openai::invalid_identity)?;
    crate::jobs::require_safe_upstream_id(id).map_err(|_| openai::invalid_identity())?;
    Ok(id)
}

fn validate_conversation(value: &Value, expected: Option<&str>) -> Result<(), ProxyError> {
    let id = safe_id(value)?;
    if value.get("object").and_then(Value::as_str) != Some("conversation")
        || expected.is_some_and(|expected| expected != id)
    {
        return Err(openai::invalid_identity());
    }
    Ok(())
}

fn same_account(a: &OriginBinding, b: &OriginBinding) -> bool {
    a.provider_key_id == b.provider_key_id && a.boundary == b.boundary
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

/// 只访问官方协议节点；内联结果、参数、模式和文本不凭同名字段取得存储权限。
pub(crate) fn take_references(value: &mut Value, refs: &mut Vec<(NativeObjectKind, String)>) {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if matches!(
        kind.as_str(),
        "function" | "custom" | "tool_search_call" | "mcp_list_tools"
    ) {
        return;
    }
    let field = match kind.as_str() {
        "item_reference" => Some(("id", NativeObjectKind::ResponseItem)),
        "input_file" | "input_image" | "computer_screenshot" => {
            Some(("file_id", NativeObjectKind::File))
        }
        "code_interpreter_call" | "container_reference" => {
            Some(("container_id", NativeObjectKind::ResponseContainer))
        }
        "skill_reference" => Some(("skill_id", NativeObjectKind::Skill)),
        _ => None,
    };
    if let Some((field, object_kind)) = field {
        if let Some(raw) = value.get(field).and_then(Value::as_str) {
            refs.push((object_kind, raw.to_owned()));
            if matches!(kind.as_str(), "container_reference" | "skill_reference") {
                *value = Value::Null;
                return;
            }
            value[field] = Value::Null;
        }
    }
    if kind == "code_interpreter" {
        if let Some(raw) = value.get("container").and_then(Value::as_str) {
            refs.push((NativeObjectKind::ResponseContainer, raw.to_owned()));
            value["container"] = Value::Null;
        } else if let Some(container) = value.get_mut("container") {
            take_file_array(container, refs);
        }
    }
    if kind == "file_search" {
        if let Some(ids) = value.get("vector_store_ids").and_then(Value::as_array) {
            for id in ids.iter().filter_map(Value::as_str) {
                refs.push((NativeObjectKind::VectorStore, id.to_owned()));
            }
            value["vector_store_ids"] = Value::Null;
        }
    }
    if kind == "container_auto" {
        take_file_array(value, refs);
    }
    if kind == "image_generation" {
        if let Some(mask) = value.get_mut("input_image_mask") {
            if let Some(raw) = mask.get("file_id").and_then(Value::as_str) {
                refs.push((NativeObjectKind::File, raw.to_owned()));
                mask["file_id"] = Value::Null;
            }
        }
    }
    match value {
        Value::Array(items) => {
            for item in items {
                take_references(item, refs);
            }
        }
        Value::Object(fields) => {
            let children: &[&str] = match kind.as_str() {
                "" | "message" => &["content"],
                "function_call_output" | "custom_tool_call_output" | "computer_call_output" => {
                    &["output"]
                }
                "shell" | "shell_call" => &["environment"],
                "container_auto" => &["skills"],
                "additional_tools" | "tool_search_output" | "allowed_tools" => &["tools"],
                _ => &[],
            };
            for (name, value) in fields {
                if children.contains(&name.as_str()) && (value.is_array() || value.is_object()) {
                    take_references(value, refs);
                }
            }
        }
        _ => {}
    }
}

fn take_file_array(value: &mut Value, refs: &mut Vec<(NativeObjectKind, String)>) {
    if let Some(ids) = value.get("file_ids").and_then(Value::as_array) {
        for id in ids.iter().filter_map(Value::as_str) {
            refs.push((NativeObjectKind::File, id.to_owned()));
        }
        value["file_ids"] = Value::Null;
    }
}

pub(crate) async fn admit_items(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    expected: &OriginBinding,
) -> Result<Vec<AcceptedReference>, ProxyError> {
    let mut checked = json!({"input":body.get("items").cloned().unwrap_or(Value::Null)});
    let mut references = Vec::new();
    take_references(&mut checked["input"], &mut references);
    crate::stored_refs::refuse_in_responses_with_background(state.platform(), &checked, true)
        .map_err(|error| match error {
            ProxyError::StoredObjectReference(path) => {
                ProxyError::StoredObjectReference(path.replacen("input", "items", 1))
            }
            ProxyError::ModelSelectorRefused(path) => {
                ProxyError::ModelSelectorRefused(path.replacen("input", "items", 1))
            }
            error => error,
        })?;
    let mut accepted = Vec::new();
    let mut seen = BTreeSet::new();
    for (kind, raw) in references {
        if kind == NativeObjectKind::Skill && super::skills::curated_openai_skill(&raw) {
            continue;
        }
        if !seen.insert((kind.as_str(), raw.clone())) {
            continue;
        }
        let (_, origin, record) = crate::attribution::detached(openai::lookup_reference(
            state, auth, client, &raw, kind, None, FAMILY,
        ))
        .await?;
        if !same_account(expected, &origin) {
            return Err(missing());
        }
        accepted.push(AcceptedReference {
            record_id: record.record_id,
            consumer_family: FAMILY,
        });
    }
    Ok(accepted)
}

/// 只从审查副本取走已声明的会话引用；原始请求仍保持客户发送的形状。
pub(crate) fn take_response_conversation(body: &mut Value) -> Option<String> {
    let conversation = body.get("conversation")?;
    let raw = conversation
        .as_str()
        .or_else(|| conversation.get("id").and_then(Value::as_str))?
        .to_owned();
    body["conversation"] = Value::Null;
    Some(raw)
}

pub(crate) async fn admit_response_conversation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
) -> Result<(OriginBinding, NativeRecord), ProxyError> {
    let (_, origin, record) = crate::attribution::detached(openai::lookup_reference(
        state,
        auth,
        client,
        raw,
        KIND,
        None,
        EndpointFamily::Responses,
    ))
    .await?;
    Ok((origin, record))
}

async fn check_selector(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    query: Option<&str>,
    expected: &OriginBinding,
) -> Result<(), ProxyError> {
    if let Some(wanted) = openai::model_hint(query, client) {
        let snapshot = state.snapshot.load();
        let (_, actual) = crate::attribution::detached(async {
            openai::resolve_management(state, &snapshot, auth, client, &wanted, KIND)
        })
        .await?;
        if !same_account(expected, &actual) {
            return Err(missing());
        }
    }
    Ok(())
}

pub(crate) async fn create(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let body = match body {
        Ok(Json(body)) => body,
        Err(error) => {
            return reject_json(&state, &auth, &client, started, "/v1/conversations", error)
        }
    };
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let wanted = openai::model_hint(query.as_deref(), &client).ok_or_else(|| {
            ProxyError::InvalidRequest("select a direct model with x-aisix-model or ?model=".into())
        })?;
        let (target, origin) =
            openai::resolve_management(&state, &snapshot, &auth, &client, &wanted, KIND)?;
        audit.resolved(&target);
        let references = admit_items(&state, &auth, &client, &body, &origin).await?;
        let bytes = Bytes::from(body.to_string());
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        audit
            .input(&state, &snapshot, &auth, &target, &bytes, false)
            .await?;
        let registration = openai::begin_related_operation(
            &state,
            &auth,
            &target,
            origin.clone(),
            KIND,
            None,
            &client,
            "conversations",
            InferenceStage::Management,
            references,
            None,
        )
        .await?;
        audit.registered(&registration);
        let reply = openai::send_at_origin(
            &state,
            &target,
            &client,
            &origin,
            KIND,
            Method::POST,
            "/conversations",
            &openai::strip_model_query(query.as_deref()),
            Body::Json(bytes),
        )
        .await?;
        let value: Value =
            serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
        validate_conversation(&value, None)?;
        let raw = safe_id(&value)?;
        registration.bind_related(&value, Vec::new()).await?;
        // 已受理结果先持久化；释放正文前再检查当前授权，撤权不抹掉归属。
        openai::lookup_management(&state, &auth, &client, raw, KIND, None).await?;
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
        Method::POST,
        "/v1/conversations",
        &auth,
        &client,
        started,
        result,
        audit,
    )
    .await
}

async fn resolve_failed_reservation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    operation_id: Uuid,
) {
    if let (Ok(registry), Ok(api_key_id)) = (openai::registry(state), openai::api_key_id(auth)) {
        let rejected = crate::attribution::native_reply_headers()
            .is_some_and(|(status, _)| status.is_client_error() && status.as_u16() != 408);
        let _ = registry
            .resolve_reservation(ResolveReservationRequest {
                api_key_id,
                operation_id,
                upstream_rejected: rejected,
            })
            .await;
    }
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
            relation: RELATION,
            action,
            child_record_ids: children,
            references,
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
    parent: &NativeRecord,
    operation_id: Uuid,
    ids: &[String],
) -> Result<(), ProxyError> {
    let reply = openai::registry(state)?
        .commit_relations(CommitRelationsRequest {
            api_key_id: openai::api_key_id(auth)?,
            operation_id,
            objects: ids
                .iter()
                .map(|raw_id| NativeChildObject {
                    kind: NativeObjectKind::ResponseItem,
                    raw_id: raw_id.clone(),
                })
                .collect(),
        })
        .await
        .map_err(|_| openai::dependency())?;
    if reply.state != "committed" || reply.operation_id != Some(operation_id) {
        return Err(openai::dependency());
    }
    for id in ids {
        let child = reply
            .records
            .iter()
            .find(|record| {
                !record.deleted
                    && record.raw_id == *id
                    && record.kind == Some(NativeObjectKind::ResponseItem)
                    && record.protocol == Some(NativeProtocol::Openai)
                    && record
                        .origin
                        .as_ref()
                        .zip(parent.origin.as_ref())
                        .is_some_and(|(a, b)| same_account(a, b))
            })
            .ok_or_else(openai::dependency)?;
        if !reply.memberships.iter().any(|membership| {
            membership.parent_record_id == parent.record_id
                && membership.child_record_id == child.record_id
                && membership.relation == RELATION
        }) {
            return Err(openai::dependency());
        }
    }
    Ok(())
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
            relation: RELATION,
            raw_ids: vec![raw.into()],
            consumer_family: Some(FAMILY),
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state != "related" {
        return Err(openai::dependency());
    }
    let Some(record) = reply.records.into_iter().find(|record| {
        !record.deleted
            && record.raw_id == raw
            && record.kind == Some(NativeObjectKind::ResponseItem)
            && record.protocol == Some(NativeProtocol::Openai)
    }) else {
        return Ok(None);
    };
    if !reply.memberships.iter().any(|membership| {
        membership.parent_record_id == parent.record_id
            && membership.child_record_id == record.record_id
            && membership.relation == RELATION
    }) {
        return Err(openai::dependency());
    }
    let (_, actual, current) = crate::attribution::detached(openai::lookup_reference(
        state,
        auth,
        client,
        raw,
        NativeObjectKind::ResponseItem,
        None,
        FAMILY,
    ))
    .await?;
    if current.record_id != record.record_id
        || parent
            .origin
            .as_ref()
            .is_none_or(|expected| !same_account(expected, &actual))
    {
        return Err(missing());
    }
    Ok(Some(current))
}

#[allow(clippy::too_many_arguments)]
async fn root_call(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    id: String,
    query: Option<String>,
    method: Method,
    body: Option<Value>,
    started: Instant,
) -> Response {
    let route = format!("/v1/conversations/{id}");
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let (target, origin, parent) =
            openai::lookup_management(&state, &auth, &client, &id, KIND, None).await?;
        audit.resolved(&target);
        check_selector(&state, &auth, &client, query.as_deref(), &origin).await?;
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let bytes = body.as_ref().map(|body| Bytes::from(body.to_string()));
        audit
            .input(
                &state,
                &snapshot,
                &auth,
                &target,
                bytes.as_deref().unwrap_or_default(),
                false,
            )
            .await?;
        let deletion = ObjectDeletionRequest {
            api_key_id: openai::api_key_id(&auth)?,
            operation_id: Uuid::new_v4(),
            record_id: parent.record_id,
            version: parent.version,
        };
        let mutation = if method == Method::DELETE {
            let reply = openai::registry(&state)?
                .reserve_delete(deletion.clone())
                .await
                .map_err(openai::registry_error)?;
            if reply.state != "reserved" || reply.operation_id != Some(deletion.operation_id) {
                return Err(openai::dependency());
            }
            Some(deletion.operation_id)
        } else if method == Method::POST {
            Some(
                reserve(
                    &state,
                    &auth,
                    &parent,
                    RelationAction::Attach,
                    Vec::new(),
                    Vec::new(),
                )
                .await?,
            )
        } else {
            None
        };
        if let Some(operation) = mutation {
            audit.registered_operation(operation, InferenceStage::Management);
        }
        let sent = openai::send_at_origin(
            &state,
            &target,
            &client,
            &origin,
            KIND,
            method.clone(),
            &format!("/conversations/{id}"),
            &openai::strip_model_query(query.as_deref()),
            bytes.map_or(Body::Empty, Body::Json),
        )
        .await;
        let reply = match sent {
            Ok(reply) => reply,
            Err(error) => {
                if let Some(operation) = mutation {
                    resolve_failed_reservation(&state, &auth, operation).await;
                }
                return Err(error);
            }
        };
        let value: Value =
            serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
        if method == Method::DELETE {
            if safe_id(&value)? != id
                || value.get("object").and_then(Value::as_str) != Some("conversation.deleted")
                || value.get("deleted").and_then(Value::as_bool) != Some(true)
            {
                return Err(openai::invalid_identity());
            }
            let deleted = openai::registry(&state)?
                .commit_delete(deletion)
                .await
                .map_err(|_| openai::dependency())?;
            if deleted.state != "deleted" {
                return Err(openai::dependency());
            }
        } else {
            validate_conversation(&value, Some(&id))?;
            if let Some(operation) = mutation {
                commit(&state, &auth, &parent, operation, &[]).await?;
            }
            openai::lookup_management(&state, &auth, &client, &id, KIND, None).await?;
        }
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    finish(
        &state, &snapshot, method, &route, &auth, &client, started, result, audit,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn items_call(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    id: String,
    item_id: Option<String>,
    query: Option<String>,
    method: Method,
    body: Option<Value>,
    started: Instant,
) -> Response {
    let path = item_id.as_ref().map_or_else(
        || format!("/conversations/{id}/items"),
        |item| format!("/conversations/{id}/items/{item}"),
    );
    let route = format!("/v1{path}");
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let (target, origin, parent) =
            openai::lookup_management(&state, &auth, &client, &id, KIND, None).await?;
        audit.resolved(&target);
        check_selector(&state, &auth, &client, query.as_deref(), &origin).await?;
        let references = match &body {
            Some(body) => admit_items(&state, &auth, &client, body, &origin).await?,
            None => Vec::new(),
        };
        for (_, cursor) in
            url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
                .filter(|(name, _)| name == "after")
        {
            if member(&state, &auth, &client, &parent, &cursor)
                .await?
                .is_none()
            {
                return Err(missing());
            }
        }
        let child = match &item_id {
            Some(raw) => member(&state, &auth, &client, &parent, raw).await?,
            None => None,
        };
        if method == Method::DELETE && child.is_none() {
            return Err(missing());
        }
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let bytes = body.map(|body| Bytes::from(body.to_string()));
        audit
            .input(
                &state,
                &snapshot,
                &auth,
                &target,
                bytes.as_deref().unwrap_or_default(),
                false,
            )
            .await?;
        let action = if method == Method::POST {
            RelationAction::Attach
        } else if method == Method::DELETE {
            RelationAction::Detach
        } else {
            RelationAction::Discover
        };
        let mut admitted = Vec::new();
        // 读取的子项已在 member 中按当前权限验证；Discover 预留只接收空子集。
        if action != RelationAction::Discover {
            if let Some(child) = &child {
                admitted.push(child.record_id);
            }
        }
        let operation = reserve(&state, &auth, &parent, action, admitted, references).await?;
        audit.registered_operation(operation, InferenceStage::Management);
        let sent = openai::send_at_origin(
            &state,
            &target,
            &client,
            &origin,
            KIND,
            method.clone(),
            &path,
            &openai::strip_model_query(query.as_deref()),
            bytes.map_or(Body::Empty, Body::Json),
        )
        .await;
        let reply = match sent {
            Ok(reply) => reply,
            Err(error) => {
                resolve_failed_reservation(&state, &auth, operation).await;
                return Err(error);
            }
        };
        let value: Value =
            serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
        let ids = if method == Method::DELETE {
            // 官方返回更新后的Conversation，而非item.deleted或deleted布尔字段。
            validate_conversation(&value, Some(&id))?;
            vec![item_id.clone().ok_or_else(missing)?]
        } else if let Some(expected) = &item_id {
            let raw = safe_id(&value)?;
            if raw != expected {
                return Err(openai::invalid_identity());
            }
            vec![raw.to_owned()]
        } else {
            value
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(openai::invalid_identity)?
                .iter()
                .map(|item| safe_id(item).map(str::to_owned))
                .collect::<Result<Vec<_>, _>>()?
        };
        // Detach只移除本集合关系；独立item及别的会话关系继续存在。
        if method == Method::DELETE {
            let committed = openai::registry(&state)?
                .commit_relations(CommitRelationsRequest {
                    api_key_id: openai::api_key_id(&auth)?,
                    operation_id: operation,
                    objects: ids
                        .iter()
                        .map(|raw_id| NativeChildObject {
                            kind: NativeObjectKind::ResponseItem,
                            raw_id: raw_id.clone(),
                        })
                        .collect(),
                })
                .await
                .map_err(|_| openai::dependency())?;
            if committed.state != "committed" || committed.operation_id != Some(operation) {
                return Err(openai::dependency());
            }
        } else {
            commit(&state, &auth, &parent, operation, &ids).await?;
        }
        openai::lookup_management(&state, &auth, &client, &id, KIND, None).await?;
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    finish(
        &state, &snapshot, method, &route, &auth, &client, started, result, audit,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    method: Method,
    path: &str,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    started: Instant,
    result: Result<(Response, JobTarget), ProxyError>,
    audit: Audit,
) -> Response {
    openai::finish(
        state,
        snapshot,
        crate::operation::CONVERSATIONS,
        method,
        path,
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

pub(crate) async fn retrieve(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    root_call(
        state,
        auth,
        client,
        id,
        query,
        Method::GET,
        None,
        Instant::now(),
    )
    .await
}
pub(crate) async fn delete(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    root_call(
        state,
        auth,
        client,
        id,
        query,
        Method::DELETE,
        None,
        Instant::now(),
    )
    .await
}
pub(crate) async fn update(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    match body {
        Ok(Json(body)) => {
            root_call(
                state,
                auth,
                client,
                id,
                query,
                Method::POST,
                Some(body),
                started,
            )
            .await
        }
        Err(error) => reject_json(
            &state,
            &auth,
            &client,
            started,
            &format!("/v1/conversations/{id}"),
            error,
        ),
    }
}
pub(crate) async fn create_items(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    match body {
        Ok(Json(body)) => {
            items_call(
                state,
                auth,
                client,
                id,
                None,
                query,
                Method::POST,
                Some(body),
                started,
            )
            .await
        }
        Err(error) => reject_json(
            &state,
            &auth,
            &client,
            started,
            &format!("/v1/conversations/{id}/items"),
            error,
        ),
    }
}
pub(crate) async fn list_items(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    items_call(
        state,
        auth,
        client,
        id,
        None,
        query,
        Method::GET,
        None,
        Instant::now(),
    )
    .await
}
pub(crate) async fn retrieve_item(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath((id, item)): AisixPath<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    items_call(
        state,
        auth,
        client,
        id,
        Some(item),
        query,
        Method::GET,
        None,
        Instant::now(),
    )
    .await
}
pub(crate) async fn delete_item(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath((id, item)): AisixPath<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    items_call(
        state,
        auth,
        client,
        id,
        Some(item),
        query,
        Method::DELETE,
        None,
        Instant::now(),
    )
    .await
}
