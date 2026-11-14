//! Evals 原始标识透传；定义、运行和输出项逐层固定归属。
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use aisix_core::models::EndpointFamily;
use aisix_core::native_metering::{MeteringTarget, ReconciliationReason, ReconciliationRequest};
use aisix_core::native_objects::*;
use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::Method;
use axum::response::Response;
use bytes::Bytes;
use serde_json::{json, Value};
use uuid::Uuid;

use super::openai::{self, Audit, Body};
use super::registry::NativeRegistryError;
use crate::{
    auth::AuthenticatedKey, client_ip::ClientContext, error::ProxyError, jobs::JobTarget,
    state::ProxyState,
};

fn missing() -> ProxyError {
    ProxyError::JobResourceNotFound("eval".into())
}
fn conflict() -> ProxyError {
    ProxyError::NativeOperationConflict
}

/// 官方十二个 method/path 共用入口；路由表只登记官方操作。
pub(crate) async fn handle(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    OriginalUri(uri): OriginalUri,
    method: Method,
    RawQuery(query): RawQuery,
    bytes: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let mut stage = InferenceStage::Management;
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => {
            return crate::reject::reject_before_dispatch(
                &state,
                method.as_str(),
                uri.path(),
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_bytes_rejection(
                    error,
                    state.request_body_limit_for(uri.path()),
                ),
            )
        }
    };
    let result = dispatch(
        &state,
        &auth,
        &client,
        &method,
        uri.path(),
        query.as_deref(),
        bytes,
        &mut audit,
        &mut stage,
    )
    .await;
    openai::finish(
        &state,
        &snapshot,
        crate::operation::EVALS,
        method,
        uri.path(),
        &auth,
        &client,
        started,
        result,
        audit,
        stage,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    method: &Method,
    path: &str,
    query: Option<&str>,
    bytes: Bytes,
    audit: &mut Audit,
    stage: &mut InferenceStage,
) -> Result<(Response, JobTarget), ProxyError> {
    let snapshot = state.snapshot.load();
    crate::usage_attr::hold_ahead_of_chain(state, &snapshot, auth, None);
    let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
    let eval_id = parts.get(2).copied();
    let run_id = parts.get(4).copied();
    let output_id = parts.get(6).copied();
    let kind = if parts.get(5) == Some(&"output_items") {
        NativeObjectKind::EvalOutputItem
    } else if parts.get(3) == Some(&"runs") {
        NativeObjectKind::EvalRun
    } else {
        NativeObjectKind::Eval
    };
    let creating = *method == Method::POST && (parts.len() == 2 || parts.len() == 4);
    let listing = *method == Method::GET && matches!(parts.len(), 2 | 4 | 6);
    let mut body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|_| ProxyError::InvalidRequest("request is not JSON".into()))?
    };
    crate::usage_attr::note_failure_request(&snapshot, || body.to_string());
    let mut parent = None;
    let mut record = None;
    let (target, origin) = if let Some(eval_id) = eval_id {
        let (target, origin, eval) =
            openai::lookup_management(state, auth, client, eval_id, NativeObjectKind::Eval, None)
                .await?;
        authorize_entries(state, auth, client, &eval, &origin)?;
        if kind == NativeObjectKind::Eval {
            record = Some(eval);
            (target, origin)
        } else {
            parent = Some(eval.clone());
            if let Some(run_id) = run_id {
                let (target, run_origin, run) = openai::lookup_management(
                    state,
                    auth,
                    client,
                    run_id,
                    NativeObjectKind::EvalRun,
                    Some(eval.record_id),
                )
                .await?;
                if run_origin != origin {
                    return Err(missing());
                }
                authorize_entries(state, auth, client, &run, &origin)?;
                if kind == NativeObjectKind::EvalRun {
                    record = Some(run);
                } else {
                    parent = Some(run.clone());
                    if let Some(output_id) = output_id {
                        let (_, item_origin, item) = openai::lookup_management(
                            state,
                            auth,
                            client,
                            output_id,
                            kind,
                            Some(run.record_id),
                        )
                        .await?;
                        if item_origin != origin {
                            return Err(missing());
                        }
                        record = Some(item);
                    }
                }
                (target, origin)
            } else {
                (target, origin)
            }
        }
    } else {
        let wanted = openai::model_hint(query, client).ok_or_else(|| {
            ProxyError::InvalidRequest("select an account with x-aisix-model or ?model=".into())
        })?;
        openai::resolve_management(state, &snapshot, auth, client, &wanted, kind)?
    };
    audit.resolved(&target);
    let model_rate = crate::quota::ModelRateLimit::from_model(
        target.display_name(),
        &target.model_entry.id,
        &target.model_entry.value,
    );
    let _reservation = if creating && kind == NativeObjectKind::EvalRun {
        crate::jobs::reserve_target(state, &snapshot, auth, &target).await?
    } else {
        crate::quota::enforce_rate_limit(state, &snapshot, auth, Some(&model_rate)).await?
    };
    if listing {
        audit
            .input(state, &snapshot, auth, &target, b"", false)
            .await?;
        let page = list(
            state,
            auth,
            client,
            &target,
            &origin,
            kind,
            parent.as_ref(),
            path,
            query,
            audit,
        )
        .await?;
        return Ok((page, target));
    }
    let mut entries = parent
        .as_ref()
        .map(|record| record.entries.clone())
        .unwrap_or_default();
    let mut references = Vec::new();
    if creating {
        if let Some(config) = body.get("data_source_config") {
            if matches!(
                config.get("type").and_then(Value::as_str),
                Some("logs" | "stored_completions")
            ) {
                return Err(openai::dependency());
            }
        }
        prepare_body(
            state,
            auth,
            client,
            &target,
            &origin,
            &mut body,
            &mut entries,
            &mut references,
        )
        .await?;
    }
    let wire = if body.is_null() {
        Bytes::new()
    } else {
        Bytes::from(
            serde_json::to_vec(&body)
                .map_err(|_| ProxyError::InvalidRequest("request is not JSON".into()))?,
        )
    };
    let masked = audit
        .input(state, &snapshot, auth, &target, &wire, false)
        .await?;
    if masked.is_some_and(|masked| masked.as_slice() != wire.as_ref()) {
        return Err(crate::error::guardrail_block_error("request", None, None));
    }
    crate::usage_attr::note_screened_request(&snapshot, || {
        if audit.input_bypassed() {
            String::new()
        } else {
            String::from_utf8_lossy(&wire).into_owned()
        }
    });
    let mut deletion = None;
    if *method == Method::DELETE {
        let record = record.as_ref().ok_or_else(missing)?;
        if kind == NativeObjectKind::EvalRun && !record.completed {
            return Err(openai::dependency());
        }
        let request = ObjectDeletionRequest {
            api_key_id: openai::api_key_id(auth)?,
            operation_id: Uuid::new_v4(),
            record_id: record.record_id,
            version: record.version,
        };
        let reply = openai::registry(state)?
            .reserve_delete(request.clone())
            .await
            .map_err(|error| {
                if error == NativeRegistryError::Conflict && kind == NativeObjectKind::Eval {
                    conflict()
                } else {
                    openai::registry_error(error)
                }
            })?;
        if reply.state != "reserved" {
            return Err(openai::dependency());
        }
        deletion = Some(request);
    }
    let mut registration = None;
    if creating {
        if kind == NativeObjectKind::EvalRun {
            // 只在真正受理付费运行时检查预算；管理读取不被冻结余额阻断。
            let requested = parent.as_ref().ok_or_else(missing)?;
            let live = snapshot
                .models
                .get_by_id(&origin.model_id.to_string())
                .ok_or_else(missing)?;
            let current_requested = snapshot
                .models
                .get_by_id(
                    &requested
                        .requested_model_id
                        .ok_or_else(missing)?
                        .to_string(),
                )
                .ok_or_else(missing)?;
            openai::resolve_for_requested(
                state,
                &snapshot,
                auth,
                client,
                &live.value.display_name,
                &current_requested.value.display_name,
                kind,
            )?;
        }
        registration = Some(
            begin(
                state,
                auth,
                client,
                &target,
                &origin,
                kind,
                parent.as_ref(),
                entries,
                references,
            )
            .await?,
        );
    }
    let upstream_path = path.strip_prefix("/v1").ok_or_else(missing)?;
    let reply = openai::send_for_kind(
        state,
        &target,
        client,
        kind,
        method.clone(),
        upstream_path,
        &openai::strip_model_query(query),
        if wire.is_empty() {
            Body::Empty
        } else {
            Body::Json(wire)
        },
    )
    .await?;
    let paid_operation = registration.filter(|_| kind == NativeObjectKind::EvalRun);
    if let Some(operation) = paid_operation {
        // 供应商已接受运行；本地后续失败不能把它登记成终态零消耗。
        *stage = InferenceStage::Accepted;
        audit.registered_operation(operation, *stage);
    }
    let value: Value = match serde_json::from_slice(&reply.bytes) {
        Ok(value) => value,
        Err(_) => {
            if let Some(operation) = paid_operation {
                submission_uncertain(state, auth, operation).await?;
            }
            return Err(openai::invalid_identity());
        }
    };
    let mut accepted_record = None;
    if *method == Method::DELETE {
        let identity_field = if kind == NativeObjectKind::Eval {
            "eval_id"
        } else {
            "run_id"
        };
        if value.get("deleted").and_then(Value::as_bool) != Some(true)
            || value.get(identity_field).and_then(Value::as_str)
                != record.as_ref().map(|record| record.raw_id.as_str())
        {
            return Err(openai::invalid_identity());
        }
    }
    if *method != Method::DELETE {
        if let Some(record) = record.as_ref() {
            check_identity(&value, kind, &record.raw_id, eval_id, run_id)?;
        }
        if let Some(operation) = registration {
            let binding = async {
                let raw = value
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(openai::invalid_identity)?;
                check_identity(&value, kind, raw, eval_id, run_id)?;
                bind(
                    state,
                    auth,
                    operation,
                    &origin,
                    kind,
                    parent.as_ref(),
                    &value,
                )
                .await
            }
            .await;
            match binding {
                Ok(record) if paid_operation.is_some() => accepted_record = Some(record),
                Ok(_) => {}
                Err(error) => {
                    if paid_operation.is_some() {
                        submission_uncertain(state, auth, operation).await?;
                    }
                    return Err(error);
                }
            }
        }
    }
    if let Some(request) = deletion {
        let reply = openai::registry(state)?
            .commit_delete(request)
            .await
            .map_err(openai::registry_error)?;
        if reply.state != "deleted" {
            return Err(openai::dependency());
        }
    }
    audit.output(state, auth, &target, &reply.bytes).await?;
    if let Some(bound) = accepted_record {
        // 已绑定运行仍交由后台观察；只有当前可见性再次通过后才能释放标识。
        let (_, current_origin, current) = openai::lookup_management(
            state,
            auth,
            client,
            &bound.raw_id,
            NativeObjectKind::EvalRun,
            bound.parent_id,
        )
        .await?;
        if current.record_id != bound.record_id || current_origin != origin {
            return Err(missing());
        }
        authorize_entries(state, auth, client, &current, &current_origin)?;
    }
    Ok((
        openai::raw_body(reply.status, &reply.headers, reply.bytes),
        target,
    ))
}

async fn submission_uncertain(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    operation_id: Uuid,
) -> Result<(), ProxyError> {
    let reply = openai::registry(state)?
        .mark_reconciliation(ReconciliationRequest {
            target: MeteringTarget::Operation {
                operation_id,
                api_key_id: openai::api_key_id(auth)?,
            },
            terminal_status: None,
            reason: ReconciliationReason::UncertainSubmission,
            observed_usage: Vec::new(),
            raw_usage: None,
            artifacts: Vec::new(),
        })
        .await
        .map_err(openai::registry_error)?;
    if reply.state != "reconciliation" {
        return Err(openai::dependency());
    }
    Ok(())
}

fn authorize_entries(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    record: &NativeRecord,
    origin: &OriginBinding,
) -> Result<(), ProxyError> {
    let snapshot = state.snapshot.load();
    for entry in &record.entries {
        let direct = snapshot
            .models
            .get_by_id(&entry.model_id.to_string())
            .ok_or_else(missing)?;
        let requested = snapshot
            .models
            .get_by_id(&entry.requested_model_id.to_string())
            .ok_or_else(missing)?;
        let (_, actual) = openai::resolve_management(
            state,
            &snapshot,
            auth,
            client,
            &requested.value.display_name,
            NativeObjectKind::EvalRun,
        )
        .map_err(|_| missing())?;
        if actual.provider_key_id != origin.provider_key_id
            || actual.boundary != origin.boundary
            || actual.model_id.to_string() != direct.id
        {
            return Err(missing());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn prepare_body(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    origin: &OriginBinding,
    body: &mut Value,
    entries: &mut Vec<NativeEntry>,
    references: &mut Vec<AcceptedReference>,
) -> Result<(), ProxyError> {
    let snapshot = state.snapshot.load();
    let mut model_slots = Vec::new();
    if let Some(criteria) = body.get("testing_criteria").and_then(Value::as_array) {
        for (index, criterion) in criteria.iter().enumerate() {
            if criterion.get("model").is_some() {
                model_slots.push(format!("/testing_criteria/{index}/model"));
            }
        }
    }
    if body.pointer("/data_source/model").is_some() {
        model_slots.push("/data_source/model".into());
    }
    for pointer in model_slots {
        let Some(wanted) = body
            .pointer(&pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let (selected, actual) = openai::resolve_management(
            state,
            &snapshot,
            auth,
            client,
            &wanted,
            NativeObjectKind::EvalRun,
        )?;
        if actual.provider_key_id != origin.provider_key_id || actual.boundary != origin.boundary {
            return Err(missing());
        }
        let upstream = selected
            .model_entry
            .value
            .upstream_model()
            .unwrap_or_default()
            .to_owned();
        if entries
            .iter()
            .any(|entry| entry.upstream_model == upstream && entry.model_id != actual.model_id)
        {
            return Err(openai::dependency());
        }
        if !entries.iter().any(|entry| entry.upstream_model == upstream) {
            entries.push(NativeEntry {
                upstream_input_text: None,
                endpoint: "/v1/evals".into(),
                entry_id: upstream.clone(),
                requested_model: wanted,
                requested_service_tier: None,
                requested_model_id: actual.model_id,
                model_id: actual.model_id,
                upstream_model: upstream.clone(),
                upstream_provider: selected.pk_entry.value.provider.clone(),
            });
        }
        if let Some(slot) = body.pointer_mut(&pointer) {
            *slot = Value::String(upstream);
        }
    }
    if let Some(source) = body.pointer("/data_source/source") {
        if matches!(
            source.get("type").and_then(Value::as_str),
            Some("responses" | "stored_completions")
        ) {
            return Err(openai::dependency());
        }
        if source.get("type").and_then(Value::as_str) == Some("file_id") {
            let raw = source.get("id").and_then(Value::as_str).ok_or_else(|| {
                ProxyError::InvalidRequest("data_source.source.id is required".into())
            })?;
            reference(
                state,
                auth,
                client,
                origin,
                raw,
                NativeObjectKind::File,
                references,
            )
            .await?;
        }
    }
    if body
        .pointer("/data_source/input_messages/type")
        .and_then(Value::as_str)
        == Some("item_reference")
    {
        // 动态轨迹无法在当前投影里逐槽证明引用归属，不能交给共享账号解释。
        return Err(openai::dependency());
    }
    for pointer in [
        "/data_source/input_messages/template",
        "/data_source/sampling_params/tools",
    ] {
        if let Some(value) = body.pointer(pointer) {
            let mut slots = Vec::new();
            collect_references(value, &mut slots)?;
            for (kind, raw) in slots {
                reference(state, auth, client, origin, &raw, kind, references).await?;
            }
        }
    }
    let _ = target;
    Ok(())
}

fn collect_references(
    value: &Value,
    slots: &mut Vec<(NativeObjectKind, String)>,
) -> Result<(), ProxyError> {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_references(value, slots)?;
            }
        }
        Value::Object(object) => {
            // 函数参数和元数据是用户数据，不是协议对象引用槽。
            if object.get("type").and_then(Value::as_str) == Some("function") {
                return Ok(());
            }
            for (key, value) in object {
                let kind = match key.as_str() {
                    "file_id" | "file_ids" => Some(NativeObjectKind::File),
                    "vector_store_ids" => Some(NativeObjectKind::VectorStore),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if let Some(raw) = value.as_str() {
                        slots.push((kind, raw.into()));
                    } else if let Some(values) = value.as_array() {
                        for value in values {
                            if let Some(raw) = value.as_str() {
                                slots.push((kind, raw.into()));
                            }
                        }
                    }
                } else if key == "container" && value.is_string() {
                    slots.push((
                        NativeObjectKind::ResponseContainer,
                        value.as_str().unwrap_or_default().into(),
                    ));
                } else if !matches!(key.as_str(), "metadata" | "parameters" | "schema") {
                    collect_references(value, slots)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn reference(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    origin: &OriginBinding,
    raw: &str,
    kind: NativeObjectKind,
    references: &mut Vec<AcceptedReference>,
) -> Result<(), ProxyError> {
    if raw.contains("{{") {
        return Err(openai::dependency());
    }
    let (_, actual, record) =
        openai::lookup_reference(state, auth, client, raw, kind, None, EndpointFamily::Evals)
            .await?;
    if actual.provider_key_id != origin.provider_key_id || actual.boundary != origin.boundary {
        return Err(missing());
    }
    references.push(AcceptedReference {
        record_id: record.record_id,
        consumer_family: EndpointFamily::Evals,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn begin(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    origin: &OriginBinding,
    kind: NativeObjectKind,
    parent: Option<&NativeRecord>,
    entries: Vec<NativeEntry>,
    references: Vec<AcceptedReference>,
) -> Result<Uuid, ProxyError> {
    let operation_id = Uuid::new_v4();
    let requested_model_id = parent
        .and_then(|record| record.requested_model_id)
        .unwrap_or(origin.model_id);
    let requested_model = parent
        .map(|record| record.requested_model.clone())
        .unwrap_or_else(|| target.model_entry.value.display_name.clone());
    let reply = openai::registry(state)?
        .begin_related(BeginRelatedRequest {
            entry_references: Default::default(),
            submission: BeginRequest {
                upstream_input_text: None,
                input_file_id: None,
                operation_id,
                api_key_id: openai::api_key_id(auth)?,
                protocol: NativeProtocol::Openai,
                kind,
                origin: origin.clone(),
                parent_id: parent.map(|record| record.record_id),
                requested_model_id,
                requested_model,
                upstream_model: target
                    .model_entry
                    .value
                    .upstream_model()
                    .unwrap_or_default()
                    .into(),
                upstream_provider: target.pk_entry.value.provider.clone(),
                operation: "evals".into(),
                request_id: client.request_id.clone(),
                asynchronous: kind == NativeObjectKind::EvalRun,
                requested_service_tier: None,
                entries,
                observation_headers: BTreeMap::new(),
            },
            references,
            lifecycle_context: None,
            owner_parent_record_id: parent.map(|record| record.record_id),
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

async fn bind(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    operation_id: Uuid,
    origin: &OriginBinding,
    kind: NativeObjectKind,
    parent: Option<&NativeRecord>,
    value: &Value,
) -> Result<NativeRecord, ProxyError> {
    let raw = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(openai::invalid_identity)?;
    crate::jobs::require_safe_upstream_id(raw).map_err(|_| openai::invalid_identity())?;
    let relations = parent
        .map(|parent| {
            vec![BoundRelation {
                parent: ArtifactSelector::Record {
                    record_id: parent.record_id,
                },
                child: ArtifactSelector::Root,
                relation: if kind == NativeObjectKind::EvalRun {
                    NativeRelation::EvalRun
                } else {
                    NativeRelation::EvalOutput
                },
            }]
        })
        .unwrap_or_default();
    let reply = openai::registry(state)?
        .bind_related(BindRelatedRequest {
            binding: BindRequest {
                existing_record_id: None,
                objects: Vec::new(),
                operation_id,
                api_key_id: openai::api_key_id(auth)?,
                raw_id: raw.into(),
                provider_created_at: value
                    .get("created_at")
                    .and_then(Value::as_i64)
                    .and_then(|time| chrono::DateTime::from_timestamp(time, 0)),
                provider_expires_at: None,
            },
            relations,
        })
        .await
        .map_err(openai::registry_error)?;
    reply
        .record
        .filter(|record| {
            reply.state == "bound"
                && record.raw_id == raw
                && record.protocol == Some(NativeProtocol::Openai)
                && record.kind == Some(kind)
                && record.parent_id == parent.map(|parent| parent.record_id)
                && record.origin.as_ref() == Some(origin)
                && !record.deleted
        })
        .ok_or_else(openai::dependency)
}

fn check_identity(
    value: &Value,
    kind: NativeObjectKind,
    raw: &str,
    eval_id: Option<&str>,
    run_id: Option<&str>,
) -> Result<(), ProxyError> {
    crate::jobs::require_safe_upstream_id(raw).map_err(|_| openai::invalid_identity())?;
    if value.get("id").and_then(Value::as_str) != Some(raw)
        || (kind != NativeObjectKind::Eval
            && value.get("eval_id").and_then(Value::as_str) != eval_id)
        || (kind == NativeObjectKind::EvalOutputItem
            && value.get("run_id").and_then(Value::as_str) != run_id)
    {
        return Err(openai::invalid_identity());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn list(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    origin: &OriginBinding,
    kind: NativeObjectKind,
    parent: Option<&NativeRecord>,
    path: &str,
    query: Option<&str>,
    audit: &mut Audit,
) -> Result<Response, ProxyError> {
    let pairs = url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let limit = pairs
        .iter()
        .find(|(key, _)| key == "limit")
        .map(|(_, value)| value.parse::<usize>())
        .transpose()
        .map_err(|_| ProxyError::InvalidRequest("limit must be an integer".into()))?
        .unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(ProxyError::InvalidRequest(
            "limit must be between 1 and 100".into(),
        ));
    }
    let mut after = pairs
        .iter()
        .find(|(key, _)| key == "after")
        .map(|(_, value)| value.clone());
    if let Some(raw) = after.as_deref() {
        let (_, actual, _) = openai::lookup_management(
            state,
            auth,
            client,
            raw,
            kind,
            parent.map(|parent| parent.record_id),
        )
        .await?;
        if actual.provider_key_id != origin.provider_key_id || actual.boundary != origin.boundary {
            return Err(missing());
        }
    }
    let mut seen = BTreeSet::new();
    let mut visible = Vec::new();
    let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
    let eval_id = parts.get(2).copied();
    let run_id = parts.get(4).copied();
    let mut first_reply = None;
    loop {
        let encoded = {
            let mut encoded = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in &pairs {
                if !matches!(key.as_str(), "model" | "after" | "limit") {
                    encoded.append_pair(key, value);
                }
            }
            encoded.append_pair("limit", "100");
            if let Some(raw) = after.as_deref() {
                encoded.append_pair("after", raw);
            }
            encoded.finish()
        };
        let reply = openai::send_for_kind(
            state,
            target,
            client,
            kind,
            Method::GET,
            path.strip_prefix("/v1").ok_or_else(missing)?,
            &encoded,
            Body::Empty,
        )
        .await?;
        let page: Value =
            serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
        let data = page
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(openai::invalid_identity)?;
        let raw_ids = data
            .iter()
            .map(|value| {
                value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(openai::invalid_identity)
            })
            .collect::<Result<Vec<_>, _>>()?;
        for raw in &raw_ids {
            crate::jobs::require_safe_upstream_id(raw).map_err(|_| openai::invalid_identity())?;
        }
        if kind == NativeObjectKind::EvalOutputItem {
            for value in data {
                let raw = value
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(openai::invalid_identity)?;
                check_identity(value, kind, raw, eval_id, run_id)?;
                let parent = parent.ok_or_else(missing)?;
                match openai::lookup_management(
                    state,
                    auth,
                    client,
                    raw,
                    kind,
                    Some(parent.record_id),
                )
                .await
                {
                    Ok((_, actual, _)) => {
                        if actual != *origin {
                            return Err(missing());
                        }
                    }
                    Err(ProxyError::JobResourceNotFound(_)) => {
                        let operation = begin(
                            state,
                            auth,
                            client,
                            target,
                            origin,
                            kind,
                            Some(parent),
                            Vec::new(),
                            Vec::new(),
                        )
                        .await?;
                        bind(state, auth, operation, origin, kind, Some(parent), value).await?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let owned = openai::registry(state)?
            .owned(OwnedRequest {
                api_key_id: openai::api_key_id(auth)?,
                protocol: NativeProtocol::Openai,
                kind,
                origin: origin.clone(),
                parent_id: parent.map(|parent| parent.record_id),
                raw_ids: raw_ids.clone(),
            })
            .await
            .map_err(openai::registry_error)?;
        if owned.state != "owned" {
            return Err(openai::dependency());
        }
        let owned = owned
            .records
            .into_iter()
            .filter(|record| {
                record.kind == Some(kind)
                    && record.protocol == Some(NativeProtocol::Openai)
                    && record.parent_id == parent.map(|parent| parent.record_id)
                    && !record.deleted
                    && record.origin.as_ref().is_some_and(|actual| {
                        actual.provider_key_id == origin.provider_key_id
                            && actual.boundary == origin.boundary
                    })
            })
            .map(|record| record.raw_id)
            .collect::<BTreeSet<_>>();
        for value in data {
            let raw = value
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(openai::invalid_identity)?;
            if owned.contains(raw) {
                check_identity(value, kind, raw, eval_id, run_id)?;
                match openai::lookup_management(
                    state,
                    auth,
                    client,
                    raw,
                    kind,
                    parent.map(|parent| parent.record_id),
                )
                .await
                {
                    Ok((_, actual, record))
                        if actual.provider_key_id == origin.provider_key_id
                            && actual.boundary == origin.boundary =>
                    {
                        if authorize_entries(state, auth, client, &record, origin).is_ok() {
                            visible.push(value.clone());
                        }
                    }
                    Ok(_) => {}
                    Err(ProxyError::JobResourceNotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let more = page
            .get("has_more")
            .and_then(Value::as_bool)
            .ok_or_else(openai::invalid_identity)?;
        let last = raw_ids.last().cloned();
        if first_reply.is_none() {
            first_reply = Some((reply.status, reply.headers));
        }
        if visible.len() > limit || !more {
            break;
        }
        let last = last.ok_or_else(openai::invalid_identity)?;
        if !seen.insert(last.clone()) {
            return Err(openai::invalid_identity());
        }
        after = Some(last);
        if seen.len() > 10_000 {
            return Err(openai::dependency());
        }
    }
    let has_more = visible.len() > limit;
    visible.truncate(limit);
    let value = json!({"object":"list","first_id":visible.first().and_then(|value|value.get("id")).cloned(),"last_id":visible.last().and_then(|value|value.get("id")).cloned(),"data":visible,"has_more":has_more});
    let bytes = Bytes::from(serde_json::to_vec(&value).map_err(|_| openai::invalid_identity())?);
    audit.output(state, auth, target, &bytes).await?;
    let (status, headers) = first_reply.ok_or_else(openai::invalid_identity)?;
    Ok(openai::raw_body(status, &headers, bytes))
}
