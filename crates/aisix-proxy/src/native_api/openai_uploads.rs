//! 原生分段上传保持线格式；上传、分块与完成文件都受同一持久账号归属约束。

use std::time::Instant;

use crate::request_json::Json;
use crate::request_json::JsonRejection;
use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{InferenceStage, NativeObjectKind, NativeRecord};
use aisix_core::AisixSnapshot;
use axum::extract::multipart::MultipartRejection;
use axum::extract::{FromRequest, Multipart, RawQuery, Request, State};
use axum::http::{header, HeaderMap, Method};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use super::openai::{self, Audit, Body};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::reject::AisixPath;
use crate::state::ProxyState;

fn invalid_identity() -> ProxyError {
    ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamDecode(
        "native upload identity is missing or inconsistent".into(),
    ))
}

fn model_hint(query: Option<&str>, client: &ClientContext, body: &mut Value) -> Option<String> {
    // model 只用于网关选路；官方 Upload 创建体没有这个字段。
    let body_hint = body
        .as_object_mut()
        .and_then(|body| body.remove("model"))
        .and_then(|model| model.as_str().map(str::to_owned));
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find_map(|(name, value)| (name == "model").then(|| value.into_owned()))
        .or_else(|| {
            client
                .headers
                .get("x-aisix-model")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
        .or(body_hint)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

fn validate_upload(value: &Value, expected: Option<&str>, status: &str) -> Result<(), ProxyError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(invalid_identity)?;
    crate::jobs::require_safe_upstream_id(id).map_err(|_| invalid_identity())?;
    if value.get("object").and_then(Value::as_str) != Some("upload")
        || expected.is_some_and(|expected| expected != id)
        || value.get("status").and_then(Value::as_str) != Some(status)
    {
        return Err(invalid_identity());
    }
    validate_timestamp(value, "created_at")?;
    validate_timestamp(value, "expires_at")?;
    Ok(())
}

fn validate_timestamp(value: &Value, field: &str) -> Result<(), ProxyError> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|_| ())
        .ok_or_else(invalid_identity)
}

/// 已登记文件只对同一原生账号开放；请求和文件各自仍须通过当前模型权限。
async fn admit_files(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    wanted: &str,
    kind: NativeObjectKind,
    mut ids: Vec<String>,
) -> Result<(), ProxyError> {
    if ids.is_empty() || state.platform().is_none() {
        return Ok(());
    }
    let consumer = match kind {
        NativeObjectKind::ChatCompletion => EndpointFamily::ChatCompletions,
        NativeObjectKind::Response => EndpointFamily::Responses,
        _ => {
            return Err(ProxyError::InvalidRequest(
                "unsupported native file consumer".into(),
            ))
        }
    };
    // 带文件引用时不能在路由组或翻译桥中猜测最终账号。
    let (target, expected) = openai::resolve(state, snapshot, auth, client, wanted, kind)?;
    ids.sort();
    ids.dedup();
    for id in ids {
        // 文件归属查询不是推理派发，不能覆盖本次请求已确定的模型归属。
        let (_, actual, _) = crate::attribution::detached(openai::lookup_file_reference(
            state, auth, client, &id, consumer,
        ))
        .await?;
        if actual.provider_key_id != expected.provider_key_id
            || actual.boundary != expected.boundary
        {
            return Err(ProxyError::JobResourceNotFound("file".into()));
        }
    }
    // 计费和审计仍归本次请求选择的模型。
    crate::attribution::note_requested_model(wanted, &target.model_entry.id);
    crate::attribution::note_resolved_entry(&target.model_entry.id);
    crate::attribution::note_target(&target.model_entry.value, &target.pk_entry.id);
    Ok(())
}

fn take_file_id(value: &mut Value, ids: &mut Vec<String>) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if let Some(id) = object.get("file_id").and_then(Value::as_str) {
        if !id.is_empty() {
            ids.push(id.to_owned());
            object.remove("file_id");
        }
    }
}

/// 只拿走官方消息内容部件中的文件槽；音频和其他持久对象仍走原有拒绝。
pub(crate) async fn admit_chat_files(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    req: &aisix_gateway::ChatFormat,
) -> Result<(), ProxyError> {
    if state.platform().is_none() {
        return crate::stored_refs::refuse_in_chat(state.platform(), req);
    }
    let mut checked = req.clone();
    let mut ids = Vec::new();
    for message in &mut checked.messages {
        for part in message.content_blocks.iter_mut().flatten() {
            if part.get("type").and_then(Value::as_str) == Some("file") {
                if let Some(file) = part.get_mut("file") {
                    take_file_id(file, &mut ids);
                }
            }
        }
    }
    crate::stored_refs::refuse_in_chat(state.platform(), &checked)?;
    admit_files(
        state,
        snapshot,
        auth,
        client,
        &req.model,
        NativeObjectKind::ChatCompletion,
        ids,
    )
    .await
}

fn take_content_file_ids(content: &mut Value, ids: &mut Vec<String>) {
    if let Some(parts) = content.as_array_mut() {
        for part in parts {
            if matches!(
                part.get("type").and_then(Value::as_str),
                Some("input_file" | "input_image")
            ) {
                take_file_id(part, ids);
            }
        }
    }
}

fn take_file_ids(value: &mut Value, ids: &mut Vec<String>) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(files) = object.get("file_ids").and_then(Value::as_array) else {
        return;
    };
    // 类型不符仍由原拒绝链处理，不能让一部分有效 ID 掩盖另一部分引用。
    if files.iter().all(Value::is_string) {
        ids.extend(
            files
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
        );
        object.remove("file_ids");
    }
}

fn take_shell_file_ids(value: &mut Value, ids: &mut Vec<String>) {
    if value.get("type").and_then(Value::as_str) == Some("container_auto") {
        // 只拿文件槽；同一环境里的 skill_reference 或 container_id 没有获得授权。
        take_file_ids(value, ids);
    }
}

fn take_tool_file_ids(tool: &mut Value, ids: &mut Vec<String>) {
    match tool.get("type").and_then(Value::as_str) {
        Some("image_generation") => {
            if let Some(mask) = tool.get_mut("input_image_mask") {
                take_file_id(mask, ids);
            }
        }
        Some("code_interpreter") => {
            if let Some(container) = tool.get_mut("container") {
                if container.get("type").and_then(Value::as_str) == Some("auto") {
                    take_file_ids(container, ids);
                }
            }
        }
        Some("shell") => {
            if let Some(environment) = tool.get_mut("environment") {
                take_shell_file_ids(environment, ids);
            }
        }
        _ => {}
    }
}

fn take_tools_file_ids(tools: &mut Value, ids: &mut Vec<String>) {
    if let Some(tools) = tools.as_array_mut() {
        for tool in tools {
            take_tool_file_ids(tool, ids);
        }
    }
}

/// 按官方输入项的内容槽准入，不递归放行工具参数、JSON Schema 或任意扩展键。
pub(crate) async fn admit_responses_files(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    allow_background: bool,
) -> Result<(), ProxyError> {
    let mut checked = body.clone();
    let mut ids = Vec::new();
    if state.platform().is_some() {
        if let Some(tools) = checked.get_mut("tools") {
            take_tools_file_ids(tools, &mut ids);
        }
        if let Some(choice) = checked.get_mut("tool_choice") {
            if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
                if let Some(tools) = choice.get_mut("tools") {
                    take_tools_file_ids(tools, &mut ids);
                }
            }
        }
        if let Some(items) = checked.get_mut("input").and_then(Value::as_array_mut) {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") | None if item.get("role").is_some() => {
                        if let Some(content) = item.get_mut("content") {
                            take_content_file_ids(content, &mut ids);
                        }
                    }
                    Some("function_call_output" | "custom_tool_call_output") => {
                        if let Some(output) = item.get_mut("output") {
                            take_content_file_ids(output, &mut ids);
                        }
                    }
                    Some("computer_call_output") => {
                        if let Some(output) = item.get_mut("output") {
                            if output.get("type").and_then(Value::as_str)
                                == Some("computer_screenshot")
                            {
                                take_file_id(output, &mut ids);
                            }
                        }
                    }
                    Some("additional_tools" | "tool_search_output") => {
                        if let Some(tools) = item.get_mut("tools") {
                            take_tools_file_ids(tools, &mut ids);
                        }
                    }
                    Some("shell_call") => {
                        if let Some(environment) = item.get_mut("environment") {
                            take_shell_file_ids(environment, &mut ids);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    crate::opaque_id::with_own_containers(state.platform(), auth, &mut checked, |checked| {
        crate::stored_refs::refuse_in_responses_with_background(
            state.platform(),
            checked,
            allow_background,
        )
    })?;
    let wanted = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    admit_files(
        state,
        snapshot,
        auth,
        client,
        wanted,
        NativeObjectKind::Response,
        ids,
    )
    .await
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

pub(crate) async fn create_upload(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let mut body = match body {
        Ok(Json(body)) => body,
        Err(error) => return reject_json(&state, &auth, &client, started, "/v1/uploads", error),
    };
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let wanted = model_hint(query.as_deref(), &client, &mut body).ok_or_else(|| {
            ProxyError::InvalidRequest(
                "pass model or x-aisix-model to select a direct model".into(),
            )
        })?;
        crate::usage_attr::note_failure_request(&snapshot, || body.to_string());
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, Some(&wanted));
        let (target, origin) = openai::resolve(
            &state,
            &snapshot,
            &auth,
            &client,
            &wanted,
            NativeObjectKind::Upload,
        )?;
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let bytes = Bytes::from(body.to_string());
        audit
            .input(&state, &snapshot, &auth, &target, &bytes, false)
            .await?;
        let registration = openai::begin(
            &state,
            &auth,
            &target,
            origin,
            NativeObjectKind::Upload,
            None,
            &client,
            "uploads",
        )
        .await?;
        let reply = openai::send(
            &state,
            &target,
            &client,
            Method::POST,
            "/uploads",
            &openai::strip_model_query(query.as_deref()),
            Body::Json(bytes),
        )
        .await?;
        if reply.status.is_success() {
            let value: Value =
                serde_json::from_slice(&reply.bytes).map_err(|_| invalid_identity())?;
            validate_upload(&value, None, "pending")?;
            registration.bind(&value).await?;
            audit.output(&state, &auth, &target, &reply.bytes).await?;
        }
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    openai::finish(
        &state,
        &snapshot,
        crate::operation::UPLOADS,
        Method::POST,
        "/v1/uploads",
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

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    headers: HeaderMap,
    bytes: Bytes,
}

pub(crate) async fn create_part(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let started = Instant::now();
    let route = format!("/v1/uploads/{id}/parts");
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        // 先查归属再读分块，未知 ID 不能用来探测或消耗共享账号。
        let (target, origin, upload) =
            openai::lookup(&state, &auth, &client, &id, NativeObjectKind::Upload, None).await?;
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let mut multipart = multipart
            .map_err(|_| ProxyError::InvalidRequest("invalid multipart form data".into()))?;
        let mut parts = Vec::new();
        while let Some(field) = multipart.next_field().await.map_err(|error| {
            crate::error::proxy_error_from_multipart(
                error,
                state.request_body_limit_for(&route),
                "malformed multipart body",
            )
        })? {
            let name = field.name().unwrap_or_default().to_owned();
            let filename = field.file_name().map(str::to_owned);
            let content_type = field.content_type().map(str::to_owned);
            let mut headers = field.headers().clone();
            // 标准部件头由发送库按名称和文件名重建，扩展头逐值保留。
            headers.remove(header::CONTENT_DISPOSITION);
            headers.remove(header::CONTENT_TYPE);
            let bytes = field.bytes().await.map_err(|error| {
                crate::error::proxy_error_from_multipart(
                    error,
                    state.request_body_limit_for(&route),
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
            &["data"],
        );
        crate::usage_attr::note_failure_request(&snapshot, || captured.clone());
        crate::usage_attr::hold_ahead_of_chain(
            &state,
            &snapshot,
            &auth,
            Some(&upload.requested_model),
        );
        // 即使分块为空，也必须执行与文本无关的自定义拒绝规则。
        if parts.is_empty() {
            audit
                .input(&state, &snapshot, &auth, &target, &[], false)
                .await?;
        }
        let mut form = reqwest::multipart::Form::new();
        for part in parts {
            let masked = audit
                .input(
                    &state,
                    &snapshot,
                    &auth,
                    &target,
                    &part.bytes,
                    part.name == "data" || part.filename.is_some(),
                )
                .await?;
            crate::usage_attr::hold_ahead_of_chain(
                &state,
                &snapshot,
                &auth,
                Some(&upload.requested_model),
            );
            if masked
                .as_ref()
                .is_some_and(|masked| masked.as_slice() != part.bytes.as_ref())
            {
                // 字节总数与 MD5 由调用方预先声明，不能静默改写单个分块。
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
                outgoing = outgoing
                    .mime_str(&content_type)
                    .map_err(|_| ProxyError::InvalidRequest("invalid part content-type".into()))?;
            }
            form = form.part(part.name, outgoing.headers(part.headers));
        }
        if audit.input_bypassed() {
            crate::usage_attr::note_failure_request(&snapshot, String::new);
        } else {
            crate::usage_attr::note_failure_request(&snapshot, || captured);
        }
        crate::usage_attr::release_failure_request();
        let registration = openai::begin_from_record(
            &state,
            &auth,
            &target,
            origin,
            NativeObjectKind::Upload,
            Some(upload.record_id),
            &client,
            "uploads",
            &upload,
        )
        .await?;
        let reply = openai::send(
            &state,
            &target,
            &client,
            Method::POST,
            &format!("/uploads/{id}/parts"),
            &openai::strip_model_query(query.as_deref()),
            Body::Multipart(form),
        )
        .await?;
        if reply.status.is_success() {
            let value: Value =
                serde_json::from_slice(&reply.bytes).map_err(|_| invalid_identity())?;
            if value.get("object").and_then(Value::as_str) != Some("upload.part")
                || value.get("upload_id").and_then(Value::as_str) != Some(id.as_str())
            {
                return Err(invalid_identity());
            }
            validate_timestamp(&value, "created_at")?;
            registration.bind(&value).await?;
            audit.output(&state, &auth, &target, &reply.bytes).await?;
        }
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    openai::finish(
        &state,
        &snapshot,
        crate::operation::UPLOADS,
        Method::POST,
        &route,
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

async fn authorize_parts(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    upload: &NativeRecord,
    body: &Value,
) -> Result<(), ProxyError> {
    if let Some(parts) = body.get("part_ids").and_then(Value::as_array) {
        for id in parts.iter().filter_map(Value::as_str) {
            // 不同上传的分块即使同一租户、同一账号也不能混用。
            let (_, origin, _) = openai::lookup(
                state,
                auth,
                client,
                id,
                NativeObjectKind::Upload,
                Some(upload.record_id),
            )
            .await?;
            if upload.origin.as_ref() != Some(&origin) {
                return Err(ProxyError::JobResourceNotFound("upload".into()));
            }
        }
    }
    Ok(())
}

pub(crate) async fn complete_upload(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let route = format!("/v1/uploads/{id}/complete");
    let body = match body {
        Ok(Json(body)) => body,
        Err(error) => return reject_json(&state, &auth, &client, started, &route, error),
    };
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let (target, origin, upload) =
            openai::lookup(&state, &auth, &client, &id, NativeObjectKind::Upload, None).await?;
        audit.resolved(&target);
        authorize_parts(&state, &auth, &client, &upload, &body).await?;
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let bytes = Bytes::from(body.to_string());
        crate::usage_attr::note_failure_request(&snapshot, || body.to_string());
        crate::usage_attr::hold_ahead_of_chain(
            &state,
            &snapshot,
            &auth,
            Some(&upload.requested_model),
        );
        audit
            .input(&state, &snapshot, &auth, &target, &bytes, false)
            .await?;
        // 完成后的 File 可独立用于普通文件接口，不要求客户端携带上传父 ID。
        let (_, file_origin) = openai::resolve(
            &state,
            &snapshot,
            &auth,
            &client,
            &target.model_entry.value.display_name,
            NativeObjectKind::File,
        )?;
        if origin != file_origin {
            return Err(ProxyError::JobResourceNotFound("upload".into()));
        }
        let file_registration = openai::begin_from_record(
            &state,
            &auth,
            &target,
            file_origin,
            NativeObjectKind::File,
            None,
            &client,
            "files",
            &upload,
        )
        .await?;
        let reply = openai::send(
            &state,
            &target,
            &client,
            Method::POST,
            &format!("/uploads/{id}/complete"),
            &openai::strip_model_query(query.as_deref()),
            Body::Json(bytes),
        )
        .await?;
        if reply.status.is_success() {
            let value: Value =
                serde_json::from_slice(&reply.bytes).map_err(|_| invalid_identity())?;
            validate_upload(&value, Some(&id), "completed")?;
            let file = value.get("file").ok_or_else(invalid_identity)?;
            if file.get("object").and_then(Value::as_str) != Some("file") {
                return Err(invalid_identity());
            }
            validate_timestamp(file, "created_at")?;
            file_registration.bind(file).await?;
            audit.output(&state, &auth, &target, &reply.bytes).await?;
        }
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    openai::finish(
        &state,
        &snapshot,
        crate::operation::UPLOADS,
        Method::POST,
        &route,
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

pub(crate) async fn cancel_upload(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    let started = Instant::now();
    let route = format!("/v1/uploads/{id}/cancel");
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let (target, _, upload) =
            openai::lookup(&state, &auth, &client, &id, NativeObjectKind::Upload, None).await?;
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let bytes = <Bytes as FromRequest<ProxyState>>::from_request(request, &state)
            .await
            .map_err(|error| {
                crate::error::proxy_error_from_bytes_rejection(
                    error,
                    state.request_body_limit_for(&route),
                )
            })?;
        if !bytes.is_empty() {
            crate::usage_attr::note_failure_request(&snapshot, || {
                String::from_utf8_lossy(&bytes).into_owned()
            });
        }
        crate::usage_attr::hold_ahead_of_chain(
            &state,
            &snapshot,
            &auth,
            Some(&upload.requested_model),
        );
        audit
            .input(&state, &snapshot, &auth, &target, &bytes, false)
            .await?;
        let body = if bytes.is_empty() {
            Body::Empty
        } else {
            Body::Json(bytes)
        };
        let reply = openai::send(
            &state,
            &target,
            &client,
            Method::POST,
            &format!("/uploads/{id}/cancel"),
            &openai::strip_model_query(query.as_deref()),
            body,
        )
        .await?;
        if reply.status.is_success() {
            let value: Value =
                serde_json::from_slice(&reply.bytes).map_err(|_| invalid_identity())?;
            validate_upload(&value, Some(&id), "cancelled")?;
            // 取消没有删除上传；后续原生重试仍由同一账号判定 cancelled 状态。
            audit.output(&state, &auth, &target, &reply.bytes).await?;
        }
        Ok((
            openai::raw_body(reply.status, &reply.headers, reply.bytes),
            target,
        ))
    }
    .await;
    openai::finish(
        &state,
        &snapshot,
        crate::operation::UPLOADS,
        Method::POST,
        &route,
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
