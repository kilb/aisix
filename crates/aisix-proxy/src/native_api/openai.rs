//! 原生 OpenAI 辅助调用共用账号边界、一次派发及终态遥测。
use super::registry::{NativeObjectRegistry, NativeRegistryError};
use crate::{
    auth::AuthenticatedKey, client_ip::ClientContext, error::ProxyError, jobs::JobTarget,
    state::ProxyState,
};
use aisix_core::native_objects::{
    configured_native_endpoint_at, configured_native_origin_at, native_object_consumer_allowed,
    native_origin_surface, AcceptedReference, BeginRelatedRequest, BeginRequest,
    BindRelatedRequest, BindRequest, BoundRelation, InferenceStage, LookupRequest,
    NativeObjectKind, NativeOriginSurface, NativeProtocol, NativeRecord, ObjectLocator,
    OriginBinding,
};
use aisix_core::{Adapter, AisixSnapshot};
use aisix_gateway::UsageStats;
use aisix_obs::UsageEvent;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const MAX_NATIVE_BODY_BYTES: usize = 512 * 1024 * 1024;
pub(crate) fn dependency() -> ProxyError {
    ProxyError::Bridge(aisix_gateway::BridgeError::DependencyUnavailable)
}
pub(crate) fn invalid_identity() -> ProxyError {
    ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamDecode(
        "native object identity is missing or inconsistent".into(),
    ))
}
fn not_found() -> ProxyError {
    ProxyError::JobResourceNotFound("native object".into())
}
pub(crate) fn registry_error(error: NativeRegistryError) -> ProxyError {
    if error == NativeRegistryError::NotFound {
        not_found()
    } else {
        ProxyError::AccountUnavailable
    }
}
pub(crate) fn api_key_id(auth: &AuthenticatedKey) -> Result<Uuid, ProxyError> {
    Ok(super::standalone_store::stable_resource_id(
        "apikeys",
        &auth.entry.id,
    ))
}
pub(crate) fn registry(state: &ProxyState) -> Result<Arc<dyn NativeObjectRegistry>, ProxyError> {
    state
        .native_objects
        .clone()
        .ok_or(ProxyError::AccountUnavailable)
}

pub(crate) fn resource_id(state: &ProxyState, kind: &str, raw: &str) -> Result<Uuid, ProxyError> {
    registry(state)?
        .resource_id(kind, raw)
        .map_err(registry_error)
}

/// 文件模式中的资源名不是 UUID；持久对象仍通过同一映射回到当前配置。
pub(crate) fn model_entry_by_id(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    id: Uuid,
) -> Result<Arc<aisix_core::ResourceEntry<aisix_core::Model>>, ProxyError> {
    let registry = registry(state)?;
    let mut matching = snapshot
        .models
        .entries()
        .into_iter()
        .filter(|entry| registry.resource_id("models", &entry.id).ok() == Some(id));
    let entry = matching.next().ok_or_else(not_found)?;
    if matching.next().is_some() {
        return Err(not_found());
    }
    Ok(entry)
}

/// 已接受任务只按稳定身份恢复原账号；同一身份存在多份当前配置时拒绝猜选。
pub(crate) fn provider_entry_by_id(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    id: Uuid,
) -> Result<Arc<aisix_core::ResourceEntry<aisix_core::ProviderKey>>, ProxyError> {
    let registry = registry(state)?;
    let mut matching = snapshot
        .provider_keys
        .entries()
        .into_iter()
        .filter(|entry| registry.resource_id("provider_keys", &entry.id).ok() == Some(id));
    let entry = matching.next().ok_or_else(not_found)?;
    if matching.next().is_some() {
        return Err(not_found());
    }
    Ok(entry)
}

fn native_protocol(state: &ProxyState, key: &aisix_core::ProviderKey) -> bool {
    crate::dispatch::resolve_bridge(&state.hub, key).map_or_else(
        || aisix_gateway::hub::upstream_protocol(key) == "openai",
        |bridge| bridge.wire_protocol() == "openai",
    )
}

pub(crate) fn current_auth(
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
) -> Result<AuthenticatedKey, ProxyError> {
    let entry = snapshot
        .apikeys
        .get_by_id(&auth.entry.id)
        .ok_or_else(not_found)?;
    if entry.value.key_hash != auth.entry.value.key_hash
        || entry.value.team_id != auth.entry.value.team_id
        || entry.value.user_id != auth.entry.value.user_id
        || entry.value.disabled
        || entry.value.is_expired_at(chrono::Utc::now())
    {
        return Err(not_found());
    }
    let mut current = auth.clone();
    current.entry = entry;
    Ok(current)
}

pub(crate) fn resolve_stateless(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    wanted: &str,
    family: aisix_core::models::EndpointFamily,
) -> Result<JobTarget, ProxyError> {
    let current = current_auth(snapshot, auth)?;
    if !current.key().can_call(family) {
        return Err(ProxyError::EndpointForbidden(family));
    }
    let target = crate::jobs::resolve_target(snapshot, &current, Some(wanted), client)?;
    if target.adapter != Adapter::Openai || !native_protocol(state, &target.pk_entry.value) {
        return Err(ProxyError::TargetRefused(
            "the selected account does not declare the native OpenAI protocol".into(),
        ));
    }
    crate::budget::gate(state, snapshot).check_provider_key(&target.pk_entry.id)?;
    check_account_headers(&target.extra_headers, client)?;
    crate::attribution::note_requested_model(wanted, &target.model_entry.id);
    crate::attribution::note_resolved_entry(&target.model_entry.id);
    crate::attribution::note_target(&target.model_entry.value, &target.pk_entry.id);
    Ok(target)
}
pub(crate) fn resolve(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    wanted: &str,
    kind: NativeObjectKind,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    resolve_for_requested(state, snapshot, auth, client, wanted, wanted, kind)
}
pub(crate) fn resolve_for_requested(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    direct: &str,
    requested: &str,
    kind: NativeObjectKind,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    resolve_for_requested_family(
        state, snapshot, auth, client, direct, requested, kind, None, true, None,
    )
}

/// 管理已存对象仍校验当前授权，但不因推理预算耗尽阻止读取和清理。
pub(crate) fn resolve_management(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    wanted: &str,
    kind: NativeObjectKind,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    resolve_for_requested_family(
        state, snapshot, auth, client, wanted, wanted, kind, None, false, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_reference_for_requested(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    direct: &str,
    requested: &str,
    kind: NativeObjectKind,
    consumer_family: aisix_core::models::EndpointFamily,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    resolve_for_requested_family(
        state,
        snapshot,
        auth,
        client,
        direct,
        requested,
        kind,
        Some(consumer_family),
        false,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_file_reference_for_requested(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    direct: &str,
    requested: &str,
    consumer_family: aisix_core::models::EndpointFamily,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    resolve_reference_for_requested(
        state,
        snapshot,
        auth,
        client,
        direct,
        requested,
        NativeObjectKind::File,
        consumer_family,
    )
}

fn reference_family(
    kind: NativeObjectKind,
    consumer_family: Option<aisix_core::models::EndpointFamily>,
) -> Result<aisix_core::models::EndpointFamily, ProxyError> {
    match consumer_family {
        None => Ok(kind.endpoint_family()),
        Some(family) if native_object_consumer_allowed(NativeProtocol::Openai, kind, family) => {
            Ok(family)
        }
        Some(_) => Err(not_found()),
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_for_requested_family(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    direct: &str,
    requested: &str,
    kind: NativeObjectKind,
    consumer_family: Option<aisix_core::models::EndpointFamily>,
    check_provider_budget: bool,
    surface: Option<NativeOriginSurface>,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    let current = current_auth(snapshot, auth)?;
    let auth = &current;
    if !auth
        .key()
        .can_call(reference_family(kind, consumer_family)?)
    {
        return Err(not_found());
    }
    let origin_key = state
        .native_origin_key()
        .ok_or(ProxyError::AccountUnavailable)?;
    if !auth.key().can_access(snapshot, requested) {
        return Err(ProxyError::ModelForbidden(requested.into()));
    }
    let entry = crate::model_resolve::resolve_model(snapshot, direct).ok_or_else(not_found)?;
    let model = &entry.value;
    crate::model_resolve::refuse_ensemble_to_grouped_key(
        direct,
        model,
        &auth.key().allowed_provider_groups,
    )?;
    if model.is_routing() || model.is_ensemble() || model.is_semantic() {
        return Err(ProxyError::InvalidRequest(
            "native stored objects require a direct model".into(),
        ));
    }
    if !crate::routing::target_in_groups(snapshot, model, &auth.key().allowed_provider_groups) {
        return Err(crate::routing::no_target_in_groups(direct));
    }
    crate::dispatch::check_ip_access(model, &client.source_ip)?;
    let pk = crate::dispatch::resolve_provider_key(snapshot, model)?;
    let surface = surface.or(Some(native_origin_surface(NativeProtocol::Openai, kind)));
    let declared = surface == Some(NativeOriginSurface::Responses)
        && pk
            .value
            .apis
            .as_ref()
            .is_some_and(|apis| apis.responses.is_some())
        && !matches!(
            pk.value.adapter,
            Some(Adapter::Bedrock | Adapter::Vertex | Adapter::AzureOpenai)
        );
    if !declared && !native_protocol(state, &pk.value) {
        return Err(ProxyError::TargetRefused(
            "the selected account does not declare the native OpenAI protocol".into(),
        ));
    }
    if check_provider_budget {
        crate::budget::gate(state, snapshot).check_provider_key(&pk.id)?;
    }
    let provider_key_id = resource_id(state, "provider_keys", &pk.id)?;
    let origin = OriginBinding {
        surface,
        model_id: resource_id(state, "models", &entry.id)?,
        provider_key_id,
        boundary: configured_native_origin_at(
            &origin_key,
            provider_key_id,
            &pk.value,
            NativeProtocol::Openai,
            kind,
            surface,
        )
        .map_err(|_| ProxyError::AccountUnavailable)?,
    };
    let context = crate::dispatch::upstream_header_ctx(&pk.value, &pk.id, model, &entry.id, client);
    let extra_headers = aisix_gateway::resolve_default_headers(&context);
    let forwarded_client = aisix_gateway::ForwardedClientHeaders::resolve(&context);
    // 账号选择只能重复静态配置值，不能临时切换共享账号的组织或项目。
    for name in ["openai-organization", "openai-project"] {
        if let Some(value) = client.headers.get(name) {
            if !extra_headers.iter().any(|(n, v)| n == name && v == value) {
                return Err(ProxyError::InvalidRequest(format!(
                    "{name} does not match the configured account"
                )));
            }
        }
    }
    let secret = crate::dispatch::require_api_key(&pk.value, model)?.to_owned();
    let requested_entry =
        crate::model_resolve::resolve_model(snapshot, requested).ok_or_else(not_found)?;
    crate::dispatch::check_ip_access(&requested_entry.value, &client.source_ip)?;
    crate::attribution::note_requested_model(requested, &requested_entry.id);
    crate::attribution::note_resolved_entry(&entry.id);
    crate::attribution::note_target(model, &pk.id);
    Ok((
        JobTarget {
            model_entry: entry,
            pk_entry: pk,
            secret,
            adapter: Adapter::Openai,
            extra_headers,
            forwarded_client,
        },
        origin,
    ))
}

fn check_account_headers(
    headers: &[(axum::http::HeaderName, HeaderValue)],
    client: &ClientContext,
) -> Result<(), ProxyError> {
    for name in ["openai-organization", "openai-project"] {
        if client.headers.get(name).is_some_and(|value| {
            !headers
                .iter()
                .any(|(key, configured)| key == name && configured == value)
        }) {
            return Err(ProxyError::InvalidRequest(format!(
                "{name} does not match the configured account"
            )));
        }
    }
    Ok(())
}

pub(crate) enum Body {
    Empty,
    Json(Bytes),
    Raw {
        bytes: Bytes,
        content_type: HeaderValue,
    },
    Multipart(reqwest::multipart::Form),
}
pub(crate) struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: Bytes,
}
pub(crate) fn strip_model_query(query: Option<&str>) -> String {
    query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| {
            url::form_urlencoded::parse(pair.as_bytes())
                .next()
                .is_none_or(|(key, _)| key != "model")
        })
        .collect::<Vec<_>>()
        .join("&")
}
/// 成功状态之后不重派；正文逐块检查硬上限，二进制不走错误清理器。
pub(crate) async fn send(
    state: &ProxyState,
    target: &JobTarget,
    client: &ClientContext,
    method: Method,
    path: &str,
    query: &str,
    body: Body,
) -> Result<Reply, ProxyError> {
    send_for_kind(
        state,
        target,
        client,
        path_kind(path),
        method,
        path,
        query,
        body,
    )
    .await
}

fn path_kind(path: &str) -> NativeObjectKind {
    match path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default()
    {
        "responses" => NativeObjectKind::Response,
        "chat" => NativeObjectKind::ChatCompletion,
        "containers" => NativeObjectKind::ResponseContainer,
        "conversations" => NativeObjectKind::Conversation,
        "vector_stores" => NativeObjectKind::VectorStore,
        "skills" => NativeObjectKind::Skill,
        "videos" => NativeObjectKind::Video,
        "fine_tuning" => NativeObjectKind::FineTuningJob,
        "evals" => NativeObjectKind::Eval,
        "uploads" => NativeObjectKind::Upload,
        "audio" => NativeObjectKind::Voice,
        "batches" => NativeObjectKind::MessageBatch,
        _ => NativeObjectKind::File,
    }
}

fn native_url(
    target: &JobTarget,
    kind: NativeObjectKind,
    surface: Option<NativeOriginSurface>,
    path: &str,
    query: &str,
) -> Result<String, ProxyError> {
    let base = configured_native_endpoint_at(
        &target.pk_entry.value,
        NativeProtocol::Openai,
        kind,
        surface,
    )
    .map_err(|_| ProxyError::AccountUnavailable)?;
    let mut url = url::Url::parse(&base).map_err(|_| ProxyError::AccountUnavailable)?;
    url.set_path(&format!("{}{path}", url.path().trim_end_matches('/')));
    if !query.is_empty() {
        let combined = match url.query().filter(|configured| !configured.is_empty()) {
            Some(configured) => format!("{configured}&{query}"),
            None => query.to_owned(),
        };
        url.set_query(Some(&combined));
    }
    Ok(url.into())
}

/// 对象种类决定规范基址；不能用普通 API 基址替换已固定的 Responses 来源。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_for_kind(
    state: &ProxyState,
    target: &JobTarget,
    client: &ClientContext,
    kind: NativeObjectKind,
    method: Method,
    path: &str,
    query: &str,
    body: Body,
) -> Result<Reply, ProxyError> {
    send_for_surface(state, target, client, kind, None, method, path, query, body).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_at_origin(
    state: &ProxyState,
    target: &JobTarget,
    client: &ClientContext,
    origin: &OriginBinding,
    kind: NativeObjectKind,
    method: Method,
    path: &str,
    query: &str,
    body: Body,
) -> Result<Reply, ProxyError> {
    send_for_surface(
        state,
        target,
        client,
        kind,
        origin.surface,
        method,
        path,
        query,
        body,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_for_surface(
    state: &ProxyState,
    target: &JobTarget,
    client: &ClientContext,
    kind: NativeObjectKind,
    surface: Option<NativeOriginSurface>,
    method: Method,
    path: &str,
    query: &str,
    body: Body,
) -> Result<Reply, ProxyError> {
    let url = native_url(target, kind, surface, path, query)?;
    aisix_gateway::response_tap::note_url_userinfo(&url);
    let mut request = crate::jobs::upstream_request(
        target,
        method.clone(),
        &url,
        &client.request_id,
        !matches!(&body, Body::Empty),
    );
    // 官方 SDK 在向量库接口填默认 beta，调用方的显式值优先。
    if path == "/vector_stores" || path.starts_with("/vector_stores/") {
        let beta = client
            .headers
            .get("openai-beta")
            .cloned()
            .or_else(|| {
                target
                    .extra_headers
                    .iter()
                    .find(|(name, _)| name == "openai-beta")
                    .map(|(_, value)| value.clone())
            })
            .unwrap_or_else(|| HeaderValue::from_static("assistants=v2"));
        // RequestBuilder::header 会追加；先替换 map 中的同名默认头。
        let mut outgoing = target.clone();
        outgoing
            .extra_headers
            .retain(|(name, _)| name != "openai-beta");
        outgoing
            .extra_headers
            .push((axum::http::HeaderName::from_static("openai-beta"), beta));
        request = crate::jobs::upstream_request(
            &outgoing,
            method,
            &url,
            &client.request_id,
            !matches!(&body, Body::Empty),
        );
    }
    request = match body {
        Body::Empty => request,
        Body::Json(bytes) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes),
        Body::Raw {
            bytes,
            content_type,
        } => request
            .header(header::CONTENT_TYPE, content_type)
            .body(bytes),
        Body::Multipart(form) => request.multipart(form),
    };
    if let Some(timeout) =
        crate::routing::effective_timeouts(&target.model_entry.value, None, state.default_timeouts)
            .request
    {
        request = request.timeout(timeout);
    }
    aisix_gateway::note_sent_credential(&target.secret);
    crate::attribution::note_translated_dispatch(false);
    crate::attribution::note_upstream_wait_begin();
    let dispatched_at = Instant::now();
    let protocol = match path {
        "/images/variations" => aisix_gateway::upstream_io::Protocol::Images,
        "/moderations" => aisix_gateway::upstream_io::Protocol::Embeddings,
        _ => aisix_gateway::upstream_io::Protocol::Unknown,
    };
    let mut response = match aisix_gateway::upstream_io::send(request, protocol).await {
        Ok(response) => response,
        Err(error) => {
            crate::attribution::note_upstream_wait_end(false);
            let error = crate::dispatch::reqwest_error_to_bridge(&error, dispatched_at);
            crate::attempt::RoutingTelemetry::record_deployment(
                state,
                &target.model_entry.id,
                error.http_status(),
            );
            return Err(ProxyError::Bridge(crate::cooldown::note_failure(
                &state.runtime_status,
                &target.model_entry.id,
                target.model_entry.value.cooldown.as_ref(),
                error,
            )));
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    crate::attribution::note_native_reply_headers(status, &headers);
    aisix_gateway::note_upstream_response(&headers);
    crate::attempt::RoutingTelemetry::record_deployment(
        state,
        &target.model_entry.id,
        status.as_u16(),
    );
    if !status.is_success() {
        let error = aisix_provider_openai::held_route_http_error(response).await;
        crate::attribution::note_upstream_wait_end(true);
        return Err(ProxyError::Bridge(error));
    }
    let mut bytes = Vec::new();
    let read =
        async {
            if response
                .content_length()
                .is_some_and(|size| size > MAX_NATIVE_BODY_BYTES as u64)
            {
                return Err(invalid_identity());
            }
            while let Some(chunk) = response.chunk().await.map_err(|error| {
                ProxyError::Bridge(aisix_gateway::upstream_body_read_error(&error))
            })? {
                if chunk.len() > MAX_NATIVE_BODY_BYTES.saturating_sub(bytes.len()) {
                    return Err(invalid_identity());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(())
        }
        .await;
    crate::attribution::note_upstream_wait_end(read.is_ok());
    read?;
    Ok(Reply {
        status,
        headers,
        bytes: Bytes::from(bytes),
    })
}
pub(crate) struct Registration {
    pub operation_id: Uuid,
    pub inference_stage: InferenceStage,
    api_key_id: Uuid,
    origin: OriginBinding,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    registry: Arc<dyn NativeObjectRegistry>,
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn begin(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    origin: OriginBinding,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    client: &ClientContext,
    operation: &str,
) -> Result<Registration, ProxyError> {
    begin_operation(
        state,
        auth,
        target,
        origin,
        kind,
        parent_id,
        client,
        operation,
        InferenceStage::Management,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn begin_operation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    origin: OriginBinding,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    client: &ClientContext,
    operation: &str,
    stage: InferenceStage,
) -> Result<Registration, ProxyError> {
    let requested = requested_model(state, target, &origin)?;
    begin_requested(
        state,
        auth,
        target,
        origin,
        kind,
        parent_id,
        client,
        operation,
        requested,
        stage,
        false,
        Vec::new(),
        None,
    )
    .await
}

/// 创建前固化引用准入；来源父对象与集合成员关系是两份独立证明。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn begin_related_operation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    origin: OriginBinding,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    client: &ClientContext,
    operation: &str,
    stage: InferenceStage,
    references: Vec<AcceptedReference>,
    owner_parent_record_id: Option<Uuid>,
) -> Result<Registration, ProxyError> {
    let requested = requested_model(state, target, &origin)?;
    begin_requested(
        state,
        auth,
        target,
        origin,
        kind,
        parent_id,
        client,
        operation,
        requested,
        stage,
        true,
        references,
        owner_parent_record_id,
    )
    .await
}

fn requested_model(
    state: &ProxyState,
    target: &JobTarget,
    origin: &OriginBinding,
) -> Result<(String, Uuid), ProxyError> {
    let requested = crate::attribution::current();
    let (requested_model, requested_model_id) =
        match requested.filter(|r| !r.requested_model_id.is_empty()) {
            Some(requested) => (
                requested.requested_model,
                resource_id(state, "models", &requested.requested_model_id)?,
            ),
            None => (
                target.model_entry.value.display_name.clone(),
                origin.model_id,
            ),
        };
    Ok((requested_model, requested_model_id))
}

/// 子对象保留创建来源的原始模型名称；当前名称只用于本次权限检查。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn begin_from_record(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    origin: OriginBinding,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    client: &ClientContext,
    operation: &str,
    source: &NativeRecord,
) -> Result<Registration, ProxyError> {
    begin_from_record_operation(
        state,
        auth,
        target,
        origin,
        kind,
        parent_id,
        client,
        operation,
        InferenceStage::Management,
        source,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn begin_from_record_operation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    origin: OriginBinding,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    client: &ClientContext,
    operation: &str,
    stage: InferenceStage,
    source: &NativeRecord,
) -> Result<Registration, ProxyError> {
    begin_requested(
        state,
        auth,
        target,
        origin,
        kind,
        parent_id,
        client,
        operation,
        (
            source.requested_model.clone(),
            source.requested_model_id.ok_or_else(dependency)?,
        ),
        stage,
        false,
        Vec::new(),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn begin_requested(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    origin: OriginBinding,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    client: &ClientContext,
    operation: &str,
    requested: (String, Uuid),
    stage: InferenceStage,
    related: bool,
    references: Vec<AcceptedReference>,
    owner_parent_record_id: Option<Uuid>,
) -> Result<Registration, ProxyError> {
    let registry = registry(state)?;
    let api_key_id = api_key_id(auth)?;
    let operation_id = Uuid::new_v4();
    let (requested_model, requested_model_id) = requested;
    let submission = BeginRequest {
        upstream_input_text: None,
        input_file_id: None,
        operation_id,
        api_key_id,
        protocol: NativeProtocol::Openai,
        kind,
        origin: origin.clone(),
        parent_id,
        requested_model_id,
        requested_model,
        upstream_model: target
            .model_entry
            .value
            .upstream_model()
            .unwrap_or_default()
            .to_owned(),
        upstream_provider: target.pk_entry.value.provider.clone(),
        operation: operation.into(),
        request_id: client.request_id.clone(),
        asynchronous: stage == InferenceStage::Accepted,
        requested_service_tier: None,
        entries: Vec::new(),
        observation_headers: Default::default(),
    };
    // 明确选择关系事务时，即使没有引用也要在分派前固定其持久 manifest。
    let reply = if !related && references.is_empty() && owner_parent_record_id.is_none() {
        registry.begin(submission).await
    } else {
        registry
            .begin_related(BeginRelatedRequest {
                entry_references: Default::default(),
                lifecycle_context: None,
                submission,
                references,
                owner_parent_record_id,
            })
            .await
    }
    .map_err(registry_error)?;
    if reply.state != "pending_submission"
        || reply.operation_id != Some(operation_id)
        || reply.origin.as_ref() != Some(&origin)
    {
        return Err(dependency());
    }
    Ok(Registration {
        operation_id,
        inference_stage: stage,
        api_key_id,
        origin,
        kind,
        parent_id,
        registry,
    })
}
impl Registration {
    pub(crate) async fn bind(&self, value: &Value) -> Result<NativeRecord, ProxyError> {
        self.bind_with_relations(value, None).await
    }

    pub(crate) async fn bind_related(
        &self,
        value: &Value,
        relations: Vec<BoundRelation>,
    ) -> Result<NativeRecord, ProxyError> {
        self.bind_with_relations(value, Some(relations)).await
    }

    async fn bind_with_relations(
        &self,
        value: &Value,
        relations: Option<Vec<BoundRelation>>,
    ) -> Result<NativeRecord, ProxyError> {
        let raw = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(invalid_identity)?;
        crate::jobs::require_safe_upstream_id(raw).map_err(|_| invalid_identity())?;
        let timestamp = |field| {
            value
                .get(field)
                .and_then(Value::as_i64)
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        };
        let binding = BindRequest {
            existing_record_id: None,
            objects: Vec::new(),
            operation_id: self.operation_id,
            api_key_id: self.api_key_id,
            raw_id: raw.into(),
            provider_created_at: timestamp("created_at").or_else(|| timestamp("created")),
            provider_expires_at: timestamp("expires_at"),
        };
        let reply = match relations {
            None => self.registry.bind(binding).await,
            Some(relations) => {
                self.registry
                    .bind_related(BindRelatedRequest { binding, relations })
                    .await
            }
        }
        .map_err(|_| dependency())?;
        reply
            .record
            .filter(|record| {
                reply.state == "bound"
                    && record.operation_id == Some(self.operation_id)
                    && record.protocol == Some(NativeProtocol::Openai)
                    && record.kind == Some(self.kind)
                    && record.parent_id == self.parent_id
                    && record.origin.as_ref() == Some(&self.origin)
                    && record.raw_id == raw
                    && !record.deleted
            })
            .ok_or_else(dependency)
    }
}
pub(crate) async fn lookup(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
) -> Result<(JobTarget, OriginBinding, NativeRecord), ProxyError> {
    lookup_for_consumer(state, auth, client, raw, kind, parent_id, None, true).await
}

pub(crate) async fn lookup_management(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
) -> Result<(JobTarget, OriginBinding, NativeRecord), ProxyError> {
    lookup_for_consumer(state, auth, client, raw, kind, parent_id, None, false).await
}

/// 消费引用不授予管理权限，也不是一次新的收费派发。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn lookup_reference(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    consumer_family: aisix_core::models::EndpointFamily,
) -> Result<(JobTarget, OriginBinding, NativeRecord), ProxyError> {
    lookup_for_consumer(
        state,
        auth,
        client,
        raw,
        kind,
        parent_id,
        Some(consumer_family),
        false,
    )
    .await
}

/// 文件引用沿用通用对象消费校验；文件管理接口仍检查 Files。
pub(crate) async fn lookup_file_reference(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    consumer_family: aisix_core::models::EndpointFamily,
) -> Result<(JobTarget, OriginBinding, NativeRecord), ProxyError> {
    lookup_reference(
        state,
        auth,
        client,
        raw,
        NativeObjectKind::File,
        None,
        consumer_family,
    )
    .await
}

/// 只复用本请求已经查过持久归属的记录；本地校验本身不能建立对象归属权限。
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_reference_record(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    consumer_family: aisix_core::models::EndpointFamily,
    record: &NativeRecord,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    reference_family(kind, Some(consumer_family))?;
    crate::jobs::require_safe_upstream_id(raw).map_err(|_| not_found())?;
    validate_lookup_record(
        state,
        auth,
        client,
        raw,
        kind,
        parent_id,
        Some(consumer_family),
        false,
        record,
    )
}

#[allow(clippy::too_many_arguments)]
async fn lookup_for_consumer(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    consumer_family: Option<aisix_core::models::EndpointFamily>,
    check_provider_budget: bool,
) -> Result<(JobTarget, OriginBinding, NativeRecord), ProxyError> {
    reference_family(kind, consumer_family)?;
    crate::jobs::require_safe_upstream_id(raw).map_err(|_| not_found())?;
    let reply = registry(state)?
        .lookup(LookupRequest {
            consumer_family,
            api_key_id: api_key_id(auth)?,
            locator: ObjectLocator {
                protocol: NativeProtocol::Openai,
                kind,
                raw_id: raw.into(),
                origin: None,
                parent_id,
            },
        })
        .await
        .map_err(registry_error)?;
    let record = reply
        .record
        .filter(|_| reply.state == "found")
        .ok_or_else(not_found)?;
    let (target, origin) = validate_lookup_record(
        state,
        auth,
        client,
        raw,
        kind,
        parent_id,
        consumer_family,
        check_provider_budget,
        &record,
    )?;
    Ok((target, origin, record))
}

/// 远程查询和请求内复用共用原有的模型、分组、IP、消费端与账号检查，不复制记录。
#[allow(clippy::too_many_arguments)]
fn validate_lookup_record(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    kind: NativeObjectKind,
    parent_id: Option<Uuid>,
    consumer_family: Option<aisix_core::models::EndpointFamily>,
    check_provider_budget: bool,
    record: &NativeRecord,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    if record.deleted
        || record.raw_id != raw
        || record.protocol != Some(NativeProtocol::Openai)
        || record.kind != Some(kind)
        || (kind == NativeObjectKind::File && record.operation == "batch_input")
        || parent_id.is_some_and(|parent| record.parent_id != Some(parent))
    {
        return Err(not_found());
    }
    let origin = record.origin.clone().ok_or_else(dependency)?;
    let snapshot = state.snapshot.load();
    let requested = model_entry_by_id(
        state,
        &snapshot,
        record.requested_model_id.ok_or_else(dependency)?,
    )?;
    let direct = model_entry_by_id(state, &snapshot, origin.model_id)?;
    let (target, actual) = resolve_for_requested_family(
        state,
        &snapshot,
        auth,
        client,
        &direct.value.display_name,
        &requested.value.display_name,
        kind,
        consumer_family,
        check_provider_budget,
        origin.surface,
    )
    .map_err(|_| not_found())?;
    if actual != origin {
        return Err(not_found());
    }
    if kind == NativeObjectKind::File {
        for entry in &record.entries {
            let requested = model_entry_by_id(state, &snapshot, entry.requested_model_id)?;
            let direct = model_entry_by_id(state, &snapshot, entry.model_id)?;
            let (_, actual) = resolve_for_requested_family(
                state,
                &snapshot,
                auth,
                client,
                &direct.value.display_name,
                &requested.value.display_name,
                kind,
                consumer_family,
                check_provider_budget,
                origin.surface,
            )
            .map_err(|_| not_found())?;
            let mut expected = origin.clone();
            expected.model_id = entry.model_id;
            if actual != expected {
                return Err(not_found());
            }
        }
    }
    Ok((target, origin))
}
pub(crate) fn raw_body(status: StatusCode, headers: &HeaderMap, bytes: Bytes) -> Response {
    let mut response = (status, bytes).into_response();
    for name in [
        "content-type",
        "content-disposition",
        "x-request-id",
        "request-id",
        "retry-after",
        "retry-after-ms",
    ] {
        if crate::platform_errors::active()
            && matches!(name, "content-disposition" | "x-request-id" | "request-id")
        {
            continue;
        }
        if let Some(value) = headers.get(name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    if !crate::platform_errors::active() {
        for (name, value) in headers {
            if name.as_str().starts_with("x-ratelimit-") {
                response.headers_mut().append(name.clone(), value.clone());
            }
        }
    }
    response
}
#[derive(Default)]
pub(crate) struct Audit {
    target: Option<JobTarget>,
    operation_id: Option<Uuid>,
    stage: Option<InferenceStage>,
    pub(crate) monitor: Vec<aisix_core::GuardrailMonitorHit>,
    pub(crate) redactions: crate::redact::RedactionCounts,
    pub(crate) enforced: Vec<aisix_core::GuardrailEnforcedHit>,
    pub(crate) scores: Vec<aisix_core::GuardrailScore>,
    pub(crate) bypass: String,
    pub(crate) applied: Vec<aisix_core::AppliedGuardrail>,
}
impl Audit {
    pub(crate) fn registered(&mut self, registration: &Registration) {
        self.registered_operation(registration.operation_id, registration.inference_stage);
    }
    pub(crate) fn registered_operation(&mut self, operation_id: Uuid, stage: InferenceStage) {
        self.operation_id = Some(operation_id);
        self.stage = Some(stage);
        crate::attribution::note_native_context(Some(stage), Some(operation_id));
    }
    pub(crate) fn input_bypassed(&self) -> bool {
        !self.bypass.is_empty()
    }
    pub(crate) fn resolved(&mut self, target: &JobTarget) {
        self.target = Some(target.clone());
        crate::attribution::note_native_context(
            Some(self.stage.unwrap_or(InferenceStage::Management)),
            self.operation_id,
        );
    }

    pub(crate) async fn input(
        &mut self,
        state: &ProxyState,
        snapshot: &AisixSnapshot,
        auth: &AuthenticatedKey,
        target: &JobTarget,
        blob: &[u8],
        mask: bool,
    ) -> Result<Option<Vec<u8>>, ProxyError> {
        self.resolved(target);
        crate::jobs::scan_input_blob(
            state,
            snapshot,
            auth,
            target,
            blob,
            &mut self.monitor,
            &mut self.enforced,
            &mut self.scores,
            &mut self.bypass,
            &mut self.applied,
            mask.then_some(&mut self.redactions),
        )
        .await
    }
    pub(crate) async fn output(
        &mut self,
        state: &ProxyState,
        auth: &AuthenticatedKey,
        target: &JobTarget,
        blob: &[u8],
    ) -> Result<(), ProxyError> {
        self.resolved(target);
        crate::jobs::scan_output_blob(
            state,
            auth,
            target,
            blob,
            &mut self.monitor,
            &mut self.enforced,
            &mut self.scores,
            &mut self.bypass,
            &mut self.applied,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    surface: crate::operation::Surface,
    method: Method,
    path: &str,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    started: Instant,
    result: Result<(Response, JobTarget), ProxyError>,
    audit: Audit,
    stage: InferenceStage,
    usage: Option<UsageStats>,
) -> Response {
    let stage = audit.stage.unwrap_or(stage);
    let elapsed = started.elapsed();
    if let Err(error) = &result {
        crate::usage_attr::note_client_error_body(snapshot, || {
            serde_json::to_string(&error.envelope()).unwrap_or_default()
        });
    }
    let (status, target, error) = match &result {
        Ok((response, target)) => (response.status().as_u16(), Some(target), None),
        Err(error) => (error.status().as_u16(), audit.target.as_ref(), Some(error)),
    };
    crate::jobs::emit_access_log(
        &method,
        path,
        target,
        &auth.entry.id,
        status,
        elapsed,
        &client.request_id,
        error,
    );
    let provider = target
        .map(|target| target.pk_entry.value.provider.as_str())
        .unwrap_or_default();
    let model = target
        .map(|target| target.model_entry.value.display_name.as_str())
        .unwrap_or_default();
    let pk_id = target
        .map(|target| target.pk_entry.id.as_str())
        .unwrap_or_default();
    let pk = crate::usage_attr::ResolvedPk::resolve(snapshot, pk_id);
    let upstream_model = target
        .and_then(|target| target.model_entry.value.upstream_model())
        .unwrap_or_default();
    crate::request_metrics::record(
        state,
        crate::normalize_endpoint_label(path),
        crate::request_metrics::Caller::new(auth),
        crate::request_metrics::Upstream {
            provider,
            model,
            upstream_model,
            pk: pk.labels(),
            ..Default::default()
        },
        status,
        error
            .map(crate::attempt::error_source)
            .unwrap_or_else(|| crate::attempt::relayed_error_source(status)),
        elapsed,
    );
    let (error_class, error_message) = match error {
        Some(error) => (
            crate::attempt::error_class(error).to_string(),
            crate::attempt::gateway_error_message(error),
        ),
        None if crate::attempt::relayed_response_failed(status) => {
            let (class, message) = crate::attempt::relayed_failure(
                status,
                &crate::attribution::relayed_error_body().unwrap_or_default(),
            );
            (class.to_owned(), message)
        }
        None => Default::default(),
    };
    let mut event = UsageEvent {
        request_id: client.request_id.clone(),
        occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
        api_key_id: auth.entry.id.clone(),
        model_id: target
            .map(|target| target.model_entry.id.clone())
            .unwrap_or_default(),
        requested_model: model.to_owned(),
        requested_model_id: target
            .map(|target| target.model_entry.id.clone())
            .unwrap_or_default(),
        status_code: status,
        downstream_latency_ms: crate::attempt::measured_ms(elapsed),
        inbound_protocol: if surface == crate::operation::REALTIME {
            "realtime"
        } else {
            "openai"
        }
        .into(),
        inference_stage: Some(stage),
        native_operation_id: audit.operation_id,
        client_source_ip: client.source_ip.clone(),
        client_user_agent: client.user_agent.clone(),
        request_headers: client.request_headers.clone(),
        guardrail_monitor_hits: audit.monitor,
        redacted_entity_counts: audit.redactions,
        applied_guardrails: audit.applied,
        guardrail_enforced_hits: audit.enforced,
        guardrail_scores: audit.scores,
        guardrail_bypassed_reason: audit.bypass,
        error_class,
        error_message,
        guardrail_blocked: error.is_some_and(ProxyError::is_guardrail_block),
        ..Default::default()
    };
    if let Some(resolved) =
        crate::attribution::current().filter(|resolved| !resolved.requested_model_id.is_empty())
    {
        event.requested_model = resolved.requested_model;
        event.requested_model_id = resolved.requested_model_id;
    }
    if let Some(usage) = usage {
        if let Some(target) = target {
            answered_usage(snapshot, target, &usage).apply(&mut event);
            crate::request_metrics::record_usage(
                state,
                crate::normalize_endpoint_label(path),
                crate::request_metrics::Caller::new(auth),
                crate::request_metrics::Upstream {
                    provider,
                    model,
                    upstream_model,
                    pk: pk.labels(),
                    ..Default::default()
                },
                crate::request_metrics::Tokens {
                    input: usage.prompt_tokens,
                    output: usage.completion_tokens,
                    total: usage.total_tokens,
                    cached: usage.cached_prompt_tokens,
                    cache_read: usage.cache_read_tokens,
                    cache_creation: usage.cache_creation_tokens,
                    client_type: state.client_classifier.classify(&client.user_agent),
                },
            );
        }
    }
    crate::usage_attr::apply_pk_telemetry(&mut event, &pk);
    crate::usage_attr::apply_caller_identity(&mut event, auth.jwt.as_ref(), &auth.caller());
    let label = crate::usage_attr::usage_event_model_label(snapshot, model).into_owned();
    let dispatched = crate::attribution::translated_dispatch().is_some();
    crate::usage_attr::emit_usage_with_finality(
        state,
        snapshot,
        surface,
        event,
        crate::usage_attr::usage_event_labels(&label, &pk),
        None,
        client.trace.as_ref(),
        true,
        stage != InferenceStage::Accepted,
        dispatched,
    );
    let upstream_model = target
        .and_then(|target| target.model_entry.value.upstream_model())
        .unwrap_or_default()
        .to_owned();
    let model = model.to_owned();
    let mut response = match result {
        Ok((response, _)) => response,
        Err(error) => error.into_response(),
    };
    if let Some((status, headers)) = crate::attribution::native_reply_headers() {
        if response.status() == status {
            crate::jobs::relay_job_response_headers(&mut response, &headers, status);
        }
    }
    let limits = auth.key().rate_limit.clone().unwrap_or_default();
    crate::request_metrics::publish_rate_limit_window(
        &state.metrics,
        &state.limiter,
        snapshot,
        &auth.entry.id,
        &limits,
        &model,
        &upstream_model,
        &mut response,
    )
    .await;
    if let Ok(value) = HeaderValue::from_str(&client.request_id) {
        response.headers_mut().insert("x-aisix-request-id", value);
    }
    response
}

pub(crate) fn model_hint(query: Option<&str>, client: &ClientContext) -> Option<String> {
    client
        .headers
        .get("x-aisix-model")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            query.and_then(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(key, value)| key == "model" && !value.is_empty())
                    .map(|(_, value)| value.into_owned())
            })
        })
}
fn missing_model() -> ProxyError {
    ProxyError::InvalidRequest(
        "model is required; select a direct model with x-aisix-model or ?model=".into(),
    )
}
const MODERATIONS: crate::operation::Surface = crate::operation::MODERATIONS;
const PROVENANCE: crate::operation::Surface = crate::operation::CONTENT_PROVENANCE_CHECKS;
const VARIATIONS: crate::operation::Surface = crate::operation::IMAGE_VARIATION;

pub(crate) async fn moderations(
    axum::extract::State(state): axum::extract::State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: Result<crate::request_json::Json<Value>, crate::request_json::JsonRejection>,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let mut usage = None;
    let body = match body {
        Ok(crate::request_json::Json(body)) => body,
        Err(error) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                "/v1/moderations",
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_json_rejection(
                    error,
                    state.request_body_limit_for("/v1/moderations"),
                ),
            )
        }
    };
    let result = async {
        let mut body = body;
        crate::usage_attr::note_failure_request(&snapshot, || body.to_string());
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, None);
        let supplied = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        let wanted = supplied
            .clone()
            .or_else(|| model_hint(query.as_deref(), &client))
            .ok_or_else(missing_model)?;
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, Some(&wanted));
        let target = resolve_stateless(
            &state,
            &snapshot,
            &auth,
            &client,
            &wanted,
            aisix_core::models::EndpointFamily::Moderations,
        )?;
        audit.resolved(&target);
        let mut reservation =
            crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        // 路由别名映射为当前配置的上游模型；显式账号提示不补造可省略的协议字段。
        if supplied.as_deref() == Some(wanted.as_str()) {
            body["model"] = Value::String(
                target
                    .model_entry
                    .value
                    .upstream_model()
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| ProxyError::InvalidRequest("request is not JSON".into()))?;
        let rewritten = audit
            .input(&state, &snapshot, &auth, &target, &bytes, true)
            .await?;
        let bytes = rewritten.map_or_else(|| Bytes::from(bytes), Bytes::from);
        crate::usage_attr::note_screened_request(&snapshot, || {
            if audit.input_bypassed() {
                String::new()
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            }
        });
        let reply = send(
            &state,
            &target,
            &client,
            Method::POST,
            "/moderations",
            &strip_model_query(query.as_deref()),
            Body::Json(bytes),
        )
        .await?;
        if let Ok(value) = serde_json::from_slice::<Value>(&reply.bytes) {
            usage = reported_usage(&value);
            if let Some(stats) = usage.as_ref() {
                note_usage(&snapshot, &target, stats);
                reservation.owe_tokens(crate::usage_attr::total_tokens_with_cache(
                    stats.prompt_tokens,
                    stats.completion_tokens,
                    stats.cache_creation_tokens,
                    stats.cache_read_tokens,
                ));
            }
        }
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((raw_body(reply.status, &reply.headers, reply.bytes), target))
    }
    .await;
    finish(
        &state,
        &snapshot,
        MODERATIONS,
        Method::POST,
        "/v1/moderations",
        &auth,
        &client,
        started,
        result,
        audit,
        InferenceStage::Attempt,
        usage,
    )
    .await
}

/// 文件只保留校验和；普通字段保留审查后的文本，重复字段逐值保留。
pub(crate) fn captured_multipart_fields(
    fields: &[(String, Option<String>, Option<String>, Bytes)],
    binary_slots: &[&str],
) -> String {
    use sha2::Digest;
    let mut object = serde_json::Map::new();
    for (name, filename, _, bytes) in fields {
        let text = std::str::from_utf8(bytes).ok();
        let binary = filename.is_some() || binary_slots.contains(&name.as_str()) || text.is_none();
        let (key, value) = if binary {
            (
                format!("{name}_sha256"),
                format!("{:x}", sha2::Sha256::digest(bytes)),
            )
        } else {
            (name.clone(), text.unwrap_or_default().to_owned())
        };
        match object.get_mut(&key) {
            Some(Value::String(previous)) => {
                previous.push('\n');
                previous.push_str(&value);
            }
            _ => {
                object.insert(key, Value::String(value));
            }
        }
    }
    Value::Object(object).to_string()
}

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    headers: HeaderMap,
    bytes: Bytes,
}
fn captured_parts(parts: &[Part]) -> String {
    let fields = parts
        .iter()
        .map(|part| {
            (
                part.name.clone(),
                part.filename.clone(),
                part.content_type.clone(),
                part.bytes.clone(),
            )
        })
        .collect::<Vec<_>>();
    captured_multipart_fields(&fields, &["file", "image", "mask", "data"])
}
async fn multipart_parts(
    multipart: Result<axum::extract::Multipart, axum::extract::multipart::MultipartRejection>,
    limit: usize,
) -> Result<Vec<Part>, ProxyError> {
    let mut multipart =
        multipart.map_err(|_| ProxyError::InvalidRequest("invalid multipart form data".into()))?;
    let mut parts = Vec::new();
    while let Some(field) = multipart.next_field().await.map_err(|error| {
        crate::error::proxy_error_from_multipart(error, limit, "native multipart")
    })? {
        let name = field
            .name()
            .ok_or_else(|| ProxyError::InvalidRequest("multipart field name missing".into()))?
            .to_owned();
        let filename = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        let mut headers = field.headers().clone();
        for name in [
            header::CONTENT_DISPOSITION,
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
        ] {
            headers.remove(name);
        }
        let bytes = field.bytes().await.map_err(|error| {
            crate::error::proxy_error_from_multipart(error, limit, "native multipart")
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
fn reported_usage(value: &Value) -> Option<UsageStats> {
    let usage = value.get("usage")?;
    if usage.get("input_tokens").is_some() {
        crate::images::image_usage_stats(value)
    } else {
        serde_json::from_value(usage.clone())
            .ok()
            .map(aisix_provider_openai::wire::into_usage)
    }
}
fn note_usage(snapshot: &AisixSnapshot, target: &JobTarget, usage: &UsageStats) {
    crate::attribution::note_answered_usage(answered_usage(snapshot, target, usage));
}
fn answered_usage(
    snapshot: &AisixSnapshot,
    target: &JobTarget,
    usage: &UsageStats,
) -> crate::usage_attr::AnsweredUsage {
    let mut answered = crate::usage_attr::AnsweredUsage::from_stats(usage);
    answered.cost_usd = crate::usage_attr::request_cost_usd(
        snapshot,
        &target.model_entry.id,
        crate::usage_attr::input_tokens_from_counts(
            usage.prompt_tokens,
            usage.cached_prompt_tokens,
            usage.cache_write_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            usage.cache_creation_ttl,
        ),
        u64::from(usage.completion_tokens),
    );
    answered
}
pub(crate) async fn provenance(
    axum::extract::State(state): axum::extract::State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    multipart: Result<axum::extract::Multipart, axum::extract::multipart::MultipartRejection>,
) -> Response {
    multipart_call(
        state,
        auth,
        client,
        query,
        multipart,
        "/content_provenance_checks",
        PROVENANCE,
        false,
    )
    .await
}
pub(crate) async fn variations(
    axum::extract::State(state): axum::extract::State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    multipart: Result<axum::extract::Multipart, axum::extract::multipart::MultipartRejection>,
) -> Response {
    multipart_call(
        state,
        auth,
        client,
        query,
        multipart,
        "/images/variations",
        VARIATIONS,
        true,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn multipart_call(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    query: Option<String>,
    multipart: Result<axum::extract::Multipart, axum::extract::multipart::MultipartRejection>,
    path: &'static str,
    surface: crate::operation::Surface,
    body_model: bool,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let mut usage = None;
    let public_path = if body_model {
        "/v1/images/variations"
    } else {
        "/v1/content_provenance_checks"
    };
    let result = async {
        let mut parts =
            multipart_parts(multipart, state.request_body_limit_for(public_path)).await?;
        crate::usage_attr::note_failure_request(&snapshot, || captured_parts(&parts));
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, None);
        let supplied = body_model
            .then(|| {
                parts
                    .iter()
                    .find(|part| part.name == "model")
                    .and_then(|part| std::str::from_utf8(&part.bytes).ok())
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            })
            .flatten();
        let wanted = supplied
            .clone()
            .or_else(|| model_hint(query.as_deref(), &client))
            .ok_or_else(missing_model)?;
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, Some(&wanted));
        let family = if body_model {
            aisix_core::models::EndpointFamily::Images
        } else {
            aisix_core::models::EndpointFamily::ContentProvenanceChecks
        };
        let target = resolve_stateless(&state, &snapshot, &auth, &client, &wanted, family)?;
        audit.resolved(&target);
        let mut reservation =
            crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        if parts.is_empty() {
            audit
                .input(&state, &snapshot, &auth, &target, b"", false)
                .await?;
        }
        let mut form = reqwest::multipart::Form::new();
        for part in &mut parts {
            if body_model && part.name == "model" && supplied.as_deref() == Some(wanted.as_str()) {
                part.bytes = Bytes::from(
                    target
                        .model_entry
                        .value
                        .upstream_model()
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
            let masked = audit
                .input(&state, &snapshot, &auth, &target, &part.bytes, true)
                .await?;
            crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, Some(&wanted));
            if masked
                .as_ref()
                .is_some_and(|masked| masked.as_slice() != part.bytes.as_ref())
            {
                return Err(crate::error::guardrail_block_error("request", None, None));
            }
            let mut outbound =
                reqwest::multipart::Part::bytes(part.bytes.to_vec()).headers(part.headers.clone());
            if let Some(filename) = &part.filename {
                outbound = outbound.file_name(filename.clone());
            }
            if let Some(content_type) = &part.content_type {
                outbound = outbound.mime_str(content_type).map_err(|_| {
                    ProxyError::InvalidRequest("invalid multipart content type".into())
                })?;
            }
            form = form.part(part.name.clone(), outbound);
        }
        crate::usage_attr::note_screened_request(&snapshot, || {
            if audit.input_bypassed() {
                String::new()
            } else {
                captured_parts(&parts)
            }
        });
        let reply = send(
            &state,
            &target,
            &client,
            Method::POST,
            path,
            &strip_model_query(query.as_deref()),
            Body::Multipart(form),
        )
        .await?;
        if let Ok(value) = serde_json::from_slice::<Value>(&reply.bytes) {
            usage = reported_usage(&value);
            if let Some(stats) = usage.as_ref() {
                note_usage(&snapshot, &target, stats);
                reservation.owe_tokens(crate::usage_attr::total_tokens_with_cache(
                    stats.prompt_tokens,
                    stats.completion_tokens,
                    stats.cache_creation_tokens,
                    stats.cache_read_tokens,
                ));
            }
        }
        audit.output(&state, &auth, &target, &reply.bytes).await?;
        Ok((raw_body(reply.status, &reply.headers, reply.bytes), target))
    }
    .await;
    finish(
        &state,
        &snapshot,
        surface,
        Method::POST,
        public_path,
        &auth,
        &client,
        started,
        result,
        audit,
        InferenceStage::Attempt,
        usage,
    )
    .await
}

#[cfg(test)]
mod shared_dispatch_tests {
    use super::super::standalone_store::StandaloneNativeStore;
    use super::*;
    use aisix_core::{
        models::EndpointFamily, snapshot::SnapshotHandle, ApiKey, Model, ProviderKey, ProxyConfig,
        ResourceEntry,
    };
    use serde_json::json;
    use std::os::unix::fs::OpenOptionsExt;
    use wiremock::{
        matchers::{header as has_header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    // 使用真实本地归属数据库和 HTTP 协议组件；这些测试不声称是真实提供商 E2E。
    async fn fixture(
        protocol: &str,
        provider: Value,
    ) -> (ProxyState, AuthenticatedKey, tempfile::TempDir) {
        let snapshot = AisixSnapshot::new();
        let key: ApiKey = serde_json::from_value(
            json!({"key_hash":"test-hash","allowed_models":["native-model"]}),
        )
        .unwrap();
        snapshot
            .apikeys
            .insert(ResourceEntry::new("local-key", key, 1));
        let model: Model = serde_json::from_value(json!({
            "display_name":"native-model","provider":protocol,"model_name":"upstream-model","provider_key_id":"local-provider"
        })).unwrap();
        snapshot
            .models
            .insert(ResourceEntry::new("local-model", model, 1));
        snapshot.provider_keys.insert(ResourceEntry::new(
            "local-provider",
            serde_json::from_value::<ProviderKey>(provider).unwrap(),
            1,
        ));
        let auth = AuthenticatedKey::new(
            &snapshot,
            snapshot.apikeys.get_by_id("local-key").unwrap(),
            None,
            false,
        );
        let snapshot = SnapshotHandle::new(snapshot);
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("native.sqlite");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&database)
            .unwrap();
        let store = StandaloneNativeStore::open(&database, [19; 32], snapshot.clone())
            .await
            .unwrap();
        let hub = Arc::new(aisix_gateway::Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        hub.register_specialized(
            "anthropic",
            Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
        );
        let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
        (
            ProxyState::new(snapshot, hub, &config)
                .without_cache()
                .with_native_objects(Arc::new(store)),
            auth,
            directory,
        )
    }

    fn provider(protocol: &str, base: &str) -> Value {
        json!({"display_name":"native","provider":protocol,"adapter":protocol,"api_key":"upstream-key","api_base":base})
    }

    fn exhaust_provider(state: &ProxyState) {
        state.snapshot.load().budgets.insert(ResourceEntry::new("spent", serde_json::from_value::<aisix_core::models::Budget>(json!({
            "scope":"provider_key","scope_ref":"local-provider","period":"month",
            "exhausted":{"reason":{"message":"provider budget exhausted","scope":"provider_key"}}
        })).unwrap(), 1));
    }

    fn endpoints(state: &ProxyState, families: Vec<EndpointFamily>, disabled: bool) {
        let snapshot = state.snapshot.load();
        let old = snapshot.apikeys.get_by_id("local-key").unwrap();
        let mut key = old.value.as_ref().clone();
        key.allowed_endpoints = Some(families);
        key.disabled = disabled;
        snapshot
            .apikeys
            .insert(ResourceEntry::new("local-key", key, old.revision + 1));
    }

    #[tokio::test]
    async fn provider_exhaustion_allows_owned_file_consent_and_cleanup_but_refuses_paid_submission()
    {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/files/file_owned"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":"file_owned","object":"file"})),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/files/file_owned"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id":"file_owned","deleted":true})),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/audio/voice_consents/consent_owned"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":"consent_owned","object":"audio.voice_consent"})),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let (state, auth, _directory) = fixture(
            "openai",
            provider("openai", &format!("{}/v1", upstream.uri())),
        )
        .await;
        let client = ClientContext::default();
        let snapshot = state.snapshot.load();
        assert!(state.platform().is_none());
        assert!(resolve(
            &state,
            &snapshot,
            &auth,
            &client,
            "native-model",
            NativeObjectKind::FineTuningJob
        )
        .is_ok());
        let (target, origin) = resolve_management(
            &state,
            &snapshot,
            &auth,
            &client,
            "native-model",
            NativeObjectKind::File,
        )
        .unwrap();
        let file = begin_operation(
            &state,
            &auth,
            &target,
            origin.clone(),
            NativeObjectKind::File,
            None,
            &client,
            "files",
            InferenceStage::Management,
        )
        .await
        .unwrap()
        .bind(&json!({"id":"file_owned"}))
        .await
        .unwrap();
        begin_operation(
            &state,
            &auth,
            &target,
            origin.clone(),
            NativeObjectKind::VoiceConsent,
            None,
            &client,
            "voice_consents",
            InferenceStage::Management,
        )
        .await
        .unwrap()
        .bind(&json!({"id":"consent_owned"}))
        .await
        .unwrap();
        exhaust_provider(&state);
        assert!(matches!(
            resolve(
                &state,
                &snapshot,
                &auth,
                &client,
                "native-model",
                NativeObjectKind::FineTuningJob
            ),
            Err(ProxyError::BudgetExceeded(_))
        ));
        let (target, origin, found) = lookup_management(
            &state,
            &auth,
            &client,
            "file_owned",
            NativeObjectKind::File,
            None,
        )
        .await
        .unwrap();
        assert_eq!(found.record_id, file.record_id);
        let (consent_target, consent_origin, _) = lookup_reference(
            &state,
            &auth,
            &client,
            "consent_owned",
            NativeObjectKind::VoiceConsent,
            None,
            EndpointFamily::Audio,
        )
        .await
        .unwrap();
        let consent = send_at_origin(
            &state,
            &consent_target,
            &client,
            &consent_origin,
            NativeObjectKind::VoiceConsent,
            Method::GET,
            "/audio/voice_consents/consent_owned",
            "",
            Body::Empty,
        )
        .await
        .unwrap();
        assert_eq!(consent.status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<Value>(&consent.bytes).unwrap()["id"],
            "consent_owned"
        );
        for verb in [Method::GET, Method::DELETE] {
            let reply = send_at_origin(
                &state,
                &target,
                &client,
                &origin,
                NativeObjectKind::File,
                verb,
                "/files/file_owned",
                "",
                Body::Empty,
            )
            .await
            .unwrap();
            assert_eq!(reply.status, StatusCode::OK);
            let value: Value = serde_json::from_slice(&reply.bytes).unwrap();
            assert_eq!(value["id"], "file_owned");
        }
        registry(&state)
            .unwrap()
            .tombstone(aisix_core::native_objects::TombstoneRequest {
                api_key_id: api_key_id(&auth).unwrap(),
                record_id: file.record_id,
                version: file.version,
            })
            .await
            .unwrap();
        assert!(lookup_management(
            &state,
            &auth,
            &client,
            "file_owned",
            NativeObjectKind::File,
            None
        )
        .await
        .is_err());
        upstream.verify().await;
    }

    #[tokio::test]
    async fn object_consumption_does_not_grant_management_and_rechecks_current_key() {
        let (state, auth, _directory) =
            fixture("openai", provider("openai", "https://api.openai.com/v1")).await;
        let client = ClientContext::default();
        let (target, origin) = resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::Skill,
        )
        .unwrap();
        begin_operation(
            &state,
            &auth,
            &target,
            origin,
            NativeObjectKind::Skill,
            None,
            &client,
            "skills",
            InferenceStage::Management,
        )
        .await
        .unwrap()
        .bind(&json!({"id":"skill_owned"}))
        .await
        .unwrap();
        endpoints(&state, vec![EndpointFamily::Responses], false);
        assert!(lookup_reference(
            &state,
            &auth,
            &client,
            "skill_owned",
            NativeObjectKind::Skill,
            None,
            EndpointFamily::Responses
        )
        .await
        .is_ok());
        assert!(lookup_management(
            &state,
            &auth,
            &client,
            "skill_owned",
            NativeObjectKind::Skill,
            None
        )
        .await
        .is_err());
        assert!(lookup_reference(
            &state,
            &auth,
            &client,
            "skill_owned",
            NativeObjectKind::Skill,
            Some(Uuid::new_v4()),
            EndpointFamily::Responses
        )
        .await
        .is_err());
        endpoints(&state, vec![EndpointFamily::Responses], true);
        assert!(lookup_reference(
            &state,
            &auth,
            &client,
            "skill_owned",
            NativeObjectKind::Skill,
            None,
            EndpointFamily::Responses
        )
        .await
        .is_err());
        endpoints(&state, vec![EndpointFamily::Messages], false);
        assert!(lookup_reference(
            &state,
            &auth,
            &client,
            "skill_owned",
            NativeObjectKind::Skill,
            None,
            EndpointFamily::Messages
        )
        .await
        .is_err());
        let snapshot = state.snapshot.load();
        let mapped = resource_id(&state, "models", "local-model").unwrap();
        let mut collision = snapshot
            .models
            .get_by_id("local-model")
            .unwrap()
            .value
            .as_ref()
            .clone();
        collision.display_name = "collision-model".into();
        snapshot
            .models
            .insert(ResourceEntry::new(mapped.to_string(), collision, 1));
        assert!(model_entry_by_id(&state, &snapshot, mapped).is_err());
    }

    #[tokio::test]
    async fn stored_surface_selects_actual_endpoint_and_vector_sdk_beta_is_overridable() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/responses/v1/containers/cntr_owned"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"cntr_owned"})))
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/objects/v1/conversations/conv_owned/items/item_owned",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"item_owned"})))
            .expect(1)
            .mount(&upstream)
            .await;
        for beta in ["assistants=v2", "caller-beta"] {
            Mock::given(method("GET"))
                .and(path("/objects/v1/vector_stores"))
                .and(has_header("openai-beta", beta))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[]})),
                )
                .expect(1)
                .mount(&upstream)
                .await;
        }
        let mut key = provider("openai", &format!("{}/objects/v1", upstream.uri()));
        key["apis"] =
            json!({"responses":{"base":format!("{}/responses/v1/responses", upstream.uri())}});
        let (state, auth, _directory) = fixture("openai", key).await;
        let mut client = ClientContext::default();
        let (target, origin) = resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::ResponseContainer,
        )
        .unwrap();
        assert_eq!(
            send_at_origin(
                &state,
                &target,
                &client,
                &origin,
                NativeObjectKind::ResponseContainer,
                Method::GET,
                "/containers/cntr_owned",
                "",
                Body::Empty
            )
            .await
            .unwrap()
            .status,
            StatusCode::OK
        );
        let (_, objects) = resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::Conversation,
        )
        .unwrap();
        assert_ne!(origin.boundary, objects.boundary);
        assert_eq!(
            send_at_origin(
                &state,
                &target,
                &client,
                &objects,
                NativeObjectKind::ResponseItem,
                Method::GET,
                "/conversations/conv_owned/items/item_owned",
                "",
                Body::Empty
            )
            .await
            .unwrap()
            .status,
            StatusCode::OK
        );
        assert_eq!(
            send_for_kind(
                &state,
                &target,
                &client,
                NativeObjectKind::VectorStore,
                Method::GET,
                "/vector_stores",
                "",
                Body::Empty
            )
            .await
            .unwrap()
            .status,
            StatusCode::OK
        );
        Arc::make_mut(&mut client.headers)
            .insert("openai-beta", HeaderValue::from_static("caller-beta"));
        assert_eq!(
            send_for_kind(
                &state,
                &target,
                &client,
                NativeObjectKind::VectorStore,
                Method::GET,
                "/vector_stores",
                "",
                Body::Empty
            )
            .await
            .unwrap()
            .status,
            StatusCode::OK
        );
        let (_, unchanged) = resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::Conversation,
        )
        .unwrap();
        assert_eq!(objects.boundary, unchanged.boundary);
        upstream.verify().await;
    }

    #[tokio::test]
    async fn explicit_native_surface_does_not_require_the_default_chat_protocol() {
        let mut key = provider("anthropic", "https://default.example");
        key["apis"] = json!({"responses":{"base":"https://responses.example/v1"}});
        let (state, auth, _directory) = fixture("anthropic", key).await;
        let client = ClientContext::default();
        assert!(resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::ResponseContainer
        )
        .is_ok());
        assert!(resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::Conversation
        )
        .is_err());
        let snapshot = state.snapshot.load();
        let old = snapshot.provider_keys.get_by_id("local-provider").unwrap();
        let mut platform_key = old.value.as_ref().clone();
        platform_key.adapter = Some(Adapter::AzureOpenai);
        snapshot.provider_keys.insert(ResourceEntry::new(
            "local-provider",
            platform_key,
            old.revision + 1,
        ));
        assert!(resolve_management(
            &state,
            &snapshot,
            &auth,
            &client,
            "native-model",
            NativeObjectKind::ResponseContainer
        )
        .is_err());

        let mut key = provider("openai", "https://default.example/v1");
        key["apis"] = json!({"messages":{"base":"https://messages.example/v1/messages"}});
        let (state, auth, _directory) = fixture("openai", key).await;
        assert!(super::super::anthropic::resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::Skill
        )
        .is_ok());
        assert!(super::super::anthropic::resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::File
        )
        .is_err());
    }

    #[tokio::test]
    async fn message_container_reuse_preserves_the_accepted_root_operation() {
        let (state, auth, _directory) = fixture(
            "anthropic",
            provider("anthropic", "https://api.anthropic.com"),
        )
        .await;
        let client = ClientContext::default();
        let (target, origin) = super::super::anthropic::resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::MessageContainer,
        )
        .unwrap();
        let original = super::super::anthropic::begin_operation(
            &state,
            &auth,
            &target,
            origin.clone(),
            NativeObjectKind::MessageContainer,
            None,
            &client,
            "messages",
            InferenceStage::Management,
        )
        .await
        .unwrap();
        let record = original
            .bind(&json!({"id":"container_owned"}))
            .await
            .unwrap();
        let snapshot = state.snapshot.load();
        let mut second_model = snapshot
            .models
            .get_by_id("local-model")
            .unwrap()
            .value
            .as_ref()
            .clone();
        second_model.display_name = "second-model".into();
        snapshot
            .models
            .insert(ResourceEntry::new("second-model", second_model, 1));
        let old_key = snapshot.apikeys.get_by_id("local-key").unwrap();
        let mut key = old_key.value.as_ref().clone();
        key.allowed_models.push("second-model".into());
        snapshot
            .apikeys
            .insert(ResourceEntry::new("local-key", key, old_key.revision + 1));
        let (target, second_origin) = super::super::anthropic::resolve_management(
            &state,
            &snapshot,
            &auth,
            &client,
            "second-model",
            NativeObjectKind::MessageContainer,
        )
        .unwrap();
        assert_ne!(origin.model_id, second_origin.model_id);
        assert_eq!(origin.boundary, second_origin.boundary);
        let reuse = super::super::anthropic::begin_related_operation(
            &state,
            &auth,
            &target,
            second_origin,
            NativeObjectKind::MessageContainer,
            None,
            &client,
            "messages",
            InferenceStage::Management,
            vec![AcceptedReference {
                record_id: record.record_id,
                consumer_family: EndpointFamily::Messages,
            }],
            Some(record.record_id),
        )
        .await
        .unwrap();
        assert_ne!(reuse.operation_id, original.operation_id);
        let reused = reuse
            .bind_related(&json!({"id":"container_owned"}), Vec::new())
            .await
            .unwrap();
        assert_eq!(reused.record_id, record.record_id);
        assert_eq!(reused.operation_id, Some(original.operation_id));
        assert_eq!(reused.origin.as_ref().unwrap().model_id, origin.model_id);
    }

    #[tokio::test]
    async fn anthropic_skills_use_messages_origin_and_explicit_management_operation() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages-origin/v1/skills/skill_owned"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"skill_owned"})))
            .expect(1)
            .mount(&upstream)
            .await;
        let mut key = provider("anthropic", &format!("{}/files-origin", upstream.uri()));
        key["apis"] =
            json!({"messages":{"base":format!("{}/messages-origin/v1/messages", upstream.uri())}});
        let (state, auth, _directory) = fixture("anthropic", key).await;
        let client = ClientContext::default();
        let (target, origin) = super::super::anthropic::resolve_management(
            &state,
            &state.snapshot.load(),
            &auth,
            &client,
            "native-model",
            NativeObjectKind::Skill,
        )
        .unwrap();
        let registration = super::super::anthropic::begin_operation(
            &state,
            &auth,
            &target,
            origin.clone(),
            NativeObjectKind::Skill,
            None,
            &client,
            "skills",
            InferenceStage::Management,
        )
        .await
        .unwrap();
        let record = registration
            .bind(&json!({"id":"skill_owned"}))
            .await
            .unwrap();
        assert_eq!(record.operation, "skills");
        assert_eq!(registration.inference_stage, InferenceStage::Management);
        exhaust_provider(&state);
        endpoints(&state, vec![EndpointFamily::Messages], false);
        assert!(super::super::anthropic::lookup_reference(
            &state,
            &auth,
            &client,
            "skill_owned",
            NativeObjectKind::Skill,
            None,
            EndpointFamily::Messages
        )
        .await
        .is_ok());
        assert!(super::super::anthropic::lookup_management(
            &state,
            &auth,
            &client,
            "skill_owned",
            NativeObjectKind::Skill,
            None
        )
        .await
        .is_err());
        let (status, _, _) = super::super::anthropic::send_at_origin(
            &state,
            &target,
            &client,
            &origin,
            NativeObjectKind::Skill,
            Method::GET,
            "/skills/skill_owned",
            "",
            super::super::anthropic::Body::Empty,
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK);
        upstream.verify().await;
    }
}
