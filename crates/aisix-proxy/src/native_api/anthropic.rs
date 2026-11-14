//! 原生文件保持提供商线格式；所有对象在释放标识前固定持久归属和账号边界。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{
    configured_native_origin, native_file_consumer_allowed, BeginRequest, BindRequest,
    InferenceStage, LookupRequest, NativeEntry, NativeObjectKind, NativeProtocol, NativeRecord,
    ObjectLocator, OriginBinding, OwnedRequest, TombstoneRequest,
};
use aisix_core::{Adapter, AisixSnapshot};
use aisix_obs::UsageEvent;
use axum::extract::Multipart;
use axum::extract::{FromRef, FromRequestParts};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;
use uuid::Uuid;

use super::registry::{NativeObjectRegistry, NativeRegistryError};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::jobs::JobTarget;
use crate::state::ProxyState;

const MAX_NATIVE_BODY_BYTES: usize = 512 * 1024 * 1024;

pub(crate) fn is_anthropic(headers: &HeaderMap) -> bool {
    headers.contains_key("anthropic-version") || headers.contains_key("anthropic-beta")
}

pub(crate) struct FilesAuthenticatedKey(pub AuthenticatedKey);
#[axum::async_trait]
impl<S> FromRequestParts<S> for FilesAuthenticatedKey
where
    S: Send + Sync,
    ProxyState: FromRef<S>,
{
    type Rejection = Response;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let anthropic = is_anthropic(&parts.headers);
        AuthenticatedKey::from_request_parts(parts, state)
            .await
            .map(Self)
            .map_err(|error| {
                if anthropic {
                    error.into_anthropic_response()
                } else {
                    error.into_response()
                }
            })
    }
}

pub(crate) fn registry(state: &ProxyState) -> Result<Arc<dyn NativeObjectRegistry>, ProxyError> {
    state.platform().ok_or(ProxyError::AccountUnavailable)?;
    state
        .native_objects
        .clone()
        .ok_or(ProxyError::AccountUnavailable)
}

pub(crate) fn api_key_id(auth: &AuthenticatedKey) -> Result<Uuid, ProxyError> {
    Uuid::parse_str(&auth.entry.id).map_err(|_| ProxyError::AccountUnavailable)
}

fn not_found() -> ProxyError {
    ProxyError::JobResourceNotFound("file".into())
}
fn registry_error(error: NativeRegistryError) -> ProxyError {
    match error {
        NativeRegistryError::NotFound => not_found(),
        _ => ProxyError::AccountUnavailable,
    }
}
fn invalid_identity() -> ProxyError {
    ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamDecode(
        "native object identity is missing or inconsistent".into(),
    ))
}
fn dependency() -> ProxyError {
    ProxyError::Bridge(aisix_gateway::BridgeError::DependencyUnavailable)
}

/// 无模型字段的协议必须显式选择账号；后续操作只使用持久化的原账号。
pub(crate) fn resolve_target(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    wanted: &str,
    client: &ClientContext,
    kind: NativeObjectKind,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    resolve_target_for_request(state, snapshot, auth, wanted, wanted, client, kind)
}

fn resolve_target_for_request(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    wanted: &str,
    requested: &str,
    client: &ClientContext,
    kind: NativeObjectKind,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    let platform = state.platform().ok_or(ProxyError::AccountUnavailable)?;
    let model_entry = crate::model_resolve::resolve_model(snapshot, wanted)
        .ok_or_else(|| ProxyError::ModelNotFound(wanted.into()))?;
    if !auth.key().can_access(snapshot, requested) {
        return Err(ProxyError::ModelForbidden(requested.into()));
    }
    let model = &model_entry.value;
    crate::model_resolve::refuse_ensemble_to_grouped_key(
        wanted,
        model,
        &auth.key().allowed_provider_groups,
    )?;
    if model.is_routing() || model.is_semantic() || model.is_ensemble() {
        return Err(ProxyError::InvalidRequest(
            "native stored objects require a direct model".into(),
        ));
    }
    if !crate::routing::target_in_groups(snapshot, model, &auth.key().allowed_provider_groups) {
        return Err(crate::routing::no_target_in_groups(wanted));
    }
    crate::dispatch::check_ip_access(model, &client.source_ip)?;
    let pk_entry = crate::dispatch::resolve_provider_key(snapshot, model)?;
    crate::budget::gate(state, snapshot).check_provider_key(&pk_entry.id)?;
    let provider_key_id =
        Uuid::parse_str(&pk_entry.id).map_err(|_| ProxyError::AccountUnavailable)?;
    let origin = OriginBinding {
        model_id: Uuid::parse_str(&model_entry.id).map_err(|_| ProxyError::AccountUnavailable)?,
        provider_key_id,
        boundary: configured_native_origin(
            &platform.id_key,
            provider_key_id,
            &pk_entry.value,
            NativeProtocol::Anthropic,
            kind,
        )
        .map_err(|_| ProxyError::AccountUnavailable)?,
    };
    let secret = crate::dispatch::require_api_key(&pk_entry.value, model)?.to_owned();
    let context = crate::dispatch::upstream_header_ctx(
        &pk_entry.value,
        &pk_entry.id,
        model,
        &model_entry.id,
        client,
    );
    let extra_headers = aisix_gateway::resolve_default_headers(&context);
    let forwarded_client = aisix_gateway::ForwardedClientHeaders::resolve(&context);
    // workspace 会改变存储账号。调用方只能重复已经纳入边界的静态配置值。
    if let Some(workspace) = client.headers.get("anthropic-workspace-id") {
        if !extra_headers
            .iter()
            .any(|(name, value)| name == "anthropic-workspace-id" && value == workspace)
        {
            return Err(ProxyError::InvalidRequest(
                "anthropic-workspace-id does not match the configured workspace".into(),
            ));
        }
    }
    let requested_id = crate::model_resolve::resolve_model(snapshot, requested)
        .ok_or_else(|| ProxyError::ModelNotFound(requested.into()))?
        .id
        .clone();
    crate::attribution::note_requested_model(requested, &requested_id);
    crate::attribution::note_resolved_entry(&model_entry.id);
    crate::attribution::note_target(model, &pk_entry.id);
    Ok((
        JobTarget {
            model_entry,
            pk_entry,
            secret,
            adapter: Adapter::Anthropic,
            extra_headers,
            forwarded_client,
        },
        origin,
    ))
}

/// 文件消费沿用真实调用面的权限，账号与模型证明仍使用文件自身的原生边界。
#[allow(clippy::too_many_arguments)]
fn resolve_target_for_consumer(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    wanted: &str,
    requested: &str,
    client: &ClientContext,
    kind: NativeObjectKind,
    consumer: EndpointFamily,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    if !native_file_consumer_allowed(NativeProtocol::Anthropic, consumer) {
        return Err(not_found());
    }
    let current = super::openai::current_auth(snapshot, auth)?;
    if !current.key().can_call(consumer) {
        return Err(not_found());
    }
    let requested_entry =
        crate::model_resolve::resolve_model(snapshot, requested).ok_or_else(not_found)?;
    crate::dispatch::check_ip_access(&requested_entry.value, &client.source_ip)?;
    let (target, origin) =
        resolve_target_for_request(state, snapshot, &current, wanted, requested, client, kind)?;
    let native = crate::dispatch::resolve_bridge(&state.hub, &target.pk_entry.value).map_or_else(
        || aisix_gateway::hub::upstream_protocol(&target.pk_entry.value) == "anthropic",
        |bridge| bridge.wire_protocol() == "anthropic",
    );
    if !native {
        return Err(not_found());
    }
    Ok((target, origin))
}

pub(crate) enum Body {
    Empty,
    Json(Bytes),
    Multipart(reqwest::multipart::Form),
}

pub(crate) fn strip_model_query(query: Option<&str>) -> String {
    query
        .unwrap_or("")
        .split('&')
        .filter(|pair| {
            url::form_urlencoded::parse(pair.as_bytes())
                .next()
                .is_none_or(|(key, _)| key != "model")
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn outgoing_headers(target: &JobTarget, client: &ClientContext) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-version",
        crate::messages::client_anthropic_version(client),
    );
    for (name, value) in &target.extra_headers {
        if !headers.contains_key(name) {
            headers.insert(name.clone(), value.clone());
        }
    }
    target.forwarded_client.apply(&mut headers);
    crate::messages::forward_client_anthropic_beta(&mut headers, client);
    if !headers.contains_key("anthropic-user-profile-id") {
        if let Some(value) = client.headers.get("anthropic-user-profile-id") {
            headers.insert("anthropic-user-profile-id", value.clone());
        }
    }
    headers
}

fn observation_headers(
    target: &JobTarget,
    client: &ClientContext,
) -> Result<BTreeMap<String, String>, ProxyError> {
    let headers = outgoing_headers(target, client);
    ["anthropic-version", "anthropic-beta"]
        .into_iter()
        .filter_map(|name| {
            let values = headers
                .get_all(name)
                .iter()
                .map(|value| value.to_str())
                .collect::<Result<Vec<_>, _>>();
            match values {
                Ok(values) if values.is_empty() => None,
                Ok(values) => Some(Ok((name.to_owned(), values.join(",")))),
                Err(_) => Some(Err(ProxyError::InvalidRequest(format!(
                    "{name} must be text"
                )))),
            }
        })
        .collect()
}

async fn read_success_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Bytes, ProxyError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(invalid_identity());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ProxyError::Bridge(aisix_gateway::upstream_body_read_error(&error)))?
    {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(invalid_identity());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(bytes))
}

/// 创建/管理只发送一次；成功头之后的本地失败不能触发重试或上游熔断。
pub(crate) async fn send(
    state: &ProxyState,
    target: &JobTarget,
    client: &ClientContext,
    method: Method,
    path: &str,
    query: &str,
    body: Body,
) -> Result<(StatusCode, HeaderMap, Bytes), ProxyError> {
    let key = &target.pk_entry.value;
    let base = if path.starts_with("/messages/batches") {
        crate::dispatch::resolve_base_url_for(key, aisix_core::ApiSurface::Messages)?
    } else {
        key.api_base
            .as_deref()
            .map(str::trim)
            .filter(|base| !base.is_empty())
            .unwrap_or("https://api.anthropic.com")
            .trim_end_matches('/')
            .to_owned()
    };
    // 路径只剥离已知协议端点；配置中的 userinfo、query 保留给 HTTP 客户端。
    let mut url = url::Url::parse(&base).map_err(|_| ProxyError::AccountUnavailable)?;
    let root = url.path().trim_end_matches('/');
    let root = root
        .strip_suffix("/v1/messages")
        .or_else(|| root.strip_suffix("/v1"))
        .unwrap_or(root);
    url.set_path(&format!("{root}/v1{path}"));
    if !query.is_empty() {
        let configured = url
            .query()
            .filter(|query| !query.is_empty())
            .map(str::to_owned);
        url.set_query(Some(&configured.map_or_else(
            || query.to_owned(),
            |configured| format!("{configured}&{query}"),
        )));
    }
    let mut headers = outgoing_headers(target, client);
    headers.insert(
        "x-api-key",
        HeaderValue::from_str(&target.secret).map_err(|_| ProxyError::AccountUnavailable)?,
    );
    if let Ok(value) = HeaderValue::from_str(&client.request_id) {
        headers.insert("x-aisix-request-id", value);
    }
    if let Some(accept) = client.headers.get(header::ACCEPT) {
        headers.insert(header::ACCEPT, accept.clone());
    }
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::CONTENT_TYPE);
    aisix_gateway::response_tap::note_url_userinfo(url.as_str());
    let http = crate::http_client::client_for(key.upstream_connection().as_ref());
    let mut request = http.request(method.clone(), url).headers(headers);
    request = match body {
        Body::Empty => request,
        Body::Json(bytes) => request
            .header(header::CONTENT_TYPE, "application/json")
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
    let response = match aisix_gateway::upstream_io::send(
        request,
        aisix_gateway::upstream_io::Protocol::Unknown,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            crate::attribution::note_upstream_wait_end(false);
            let error = aisix_gateway::BridgeError::Transport(
                aisix_gateway::transport_error_message(&error),
            );
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
    aisix_gateway::note_upstream_response(&headers);
    crate::attempt::RoutingTelemetry::record_deployment(
        state,
        &target.model_entry.id,
        status.as_u16(),
    );
    if !status.is_success() {
        let error = aisix_provider_anthropic::map_http_error(status, response).await;
        crate::attribution::note_upstream_wait_end(true);
        return Err(ProxyError::Bridge(error));
    }
    // 成功的二进制不能走错误正文清理器；超限明确失败，绝不截断后伪装成功。
    let bytes = read_success_body(response, MAX_NATIVE_BODY_BYTES).await;
    crate::attribution::note_upstream_wait_end(bytes.is_ok());
    let bytes = bytes?;
    Ok((status, headers, bytes))
}

pub(crate) struct Registration {
    pub(crate) operation_id: Uuid,
    api_key_id: Uuid,
    origin: OriginBinding,
    kind: NativeObjectKind,
    registry: Arc<dyn NativeObjectRegistry>,
}

pub(crate) async fn begin(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    origin: OriginBinding,
    kind: NativeObjectKind,
    entries: Vec<NativeEntry>,
    client: &ClientContext,
) -> Result<Registration, ProxyError> {
    begin_for_requested(state, auth, target, origin, kind, entries, client, None).await
}

#[allow(clippy::too_many_arguments)]
async fn begin_for_requested(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    target: &JobTarget,
    origin: OriginBinding,
    kind: NativeObjectKind,
    entries: Vec<NativeEntry>,
    client: &ClientContext,
    requested: Option<(Uuid, String)>,
) -> Result<Registration, ProxyError> {
    let registry = registry(state)?;
    let api_key_id = api_key_id(auth)?;
    let operation_id = Uuid::new_v4();
    let reply = registry
        .begin(BeginRequest {
            upstream_input_text: None,
            input_file_id: None,
            operation_id,
            api_key_id,
            protocol: NativeProtocol::Anthropic,
            kind,
            origin: origin.clone(),
            parent_id: None,
            requested_model_id: requested.as_ref().map_or(origin.model_id, |(id, _)| *id),
            requested_model: requested.as_ref().map_or_else(
                || target.model_entry.value.display_name.clone(),
                |(_, name)| name.clone(),
            ),
            upstream_model: target
                .model_entry
                .value
                .upstream_model()
                .unwrap_or_default()
                .to_owned(),
            upstream_provider: target.pk_entry.value.provider.clone(),
            operation: if kind == NativeObjectKind::File {
                "files"
            } else {
                "batch_completion"
            }
            .into(),
            request_id: client.request_id.clone(),
            asynchronous: kind == NativeObjectKind::MessageBatch,
            requested_service_tier: None,
            entries,
            observation_headers: observation_headers(target, client)?,
        })
        .await
        .map_err(registry_error)?;
    if reply.state != "pending_submission"
        || reply.operation_id != Some(operation_id)
        || reply.origin.as_ref() != Some(&origin)
    {
        return Err(ProxyError::AccountUnavailable);
    }
    Ok(Registration {
        operation_id,
        api_key_id,
        origin,
        kind,
        registry,
    })
}

impl Registration {
    pub(crate) async fn bind(&self, value: &Value) -> Result<NativeRecord, ProxyError> {
        let raw_id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(invalid_identity)?;
        crate::jobs::require_safe_upstream_id(raw_id).map_err(|_| invalid_identity())?;
        let timestamp = |field| {
            value
                .get(field)
                .and_then(Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|time| time.with_timezone(&chrono::Utc))
        };
        let reply = self
            .registry
            .bind(BindRequest {
                existing_record_id: None,
                objects: Vec::new(),
                operation_id: self.operation_id,
                api_key_id: self.api_key_id,
                raw_id: raw_id.into(),
                provider_created_at: timestamp("created_at"),
                provider_expires_at: timestamp("expires_at"),
            })
            .await
            .map_err(|_| dependency())?;
        reply
            .record
            .filter(|record| {
                reply.state == "bound"
                    && record.operation_id == Some(self.operation_id)
                    && record.raw_id == raw_id
                    && record.origin.as_ref() == Some(&self.origin)
                    && record.protocol == Some(NativeProtocol::Anthropic)
                    && record.kind == Some(self.kind)
                    && !record.deleted
            })
            .ok_or_else(dependency)
    }
}

pub(crate) async fn lookup(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    id: &str,
    kind: NativeObjectKind,
) -> Result<(JobTarget, OriginBinding, NativeRecord), ProxyError> {
    lookup_at_origin(state, auth, client, id, kind, None, None).await
}

#[allow(clippy::too_many_arguments)]
async fn lookup_at_origin(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    id: &str,
    kind: NativeObjectKind,
    hint: Option<OriginBinding>,
    consumer_family: Option<EndpointFamily>,
) -> Result<(JobTarget, OriginBinding, NativeRecord), ProxyError> {
    crate::jobs::require_safe_upstream_id(id).map_err(|_| not_found())?;
    let reply = registry(state)?
        .lookup(LookupRequest {
            consumer_family,
            api_key_id: api_key_id(auth)?,
            locator: ObjectLocator {
                protocol: NativeProtocol::Anthropic,
                kind,
                raw_id: id.into(),
                origin: hint,
                parent_id: None,
            },
        })
        .await
        .map_err(registry_error)?;
    let record = reply
        .record
        .filter(|record| {
            reply.state == "found"
                && !record.deleted
                && record.raw_id == id
                && record.protocol == Some(NativeProtocol::Anthropic)
                && record.kind == Some(kind)
        })
        .ok_or_else(not_found)?;
    let origin = record
        .origin
        .clone()
        .ok_or(ProxyError::AccountUnavailable)?;
    let snapshot = state.snapshot.load();
    let requested = snapshot
        .models
        .get_by_id(
            &record
                .requested_model_id
                .ok_or(ProxyError::AccountUnavailable)?
                .to_string(),
        )
        .ok_or_else(not_found)?;
    let direct = snapshot
        .models
        .get_by_id(&origin.model_id.to_string())
        .ok_or_else(not_found)?;
    let (target, actual) = if let Some(consumer) = consumer_family {
        if kind != NativeObjectKind::File {
            return Err(not_found());
        }
        resolve_target_for_consumer(
            state,
            &snapshot,
            auth,
            &direct.value.display_name,
            &requested.value.display_name,
            client,
            kind,
            consumer,
        )?
    } else {
        resolve_target_for_request(
            state,
            &snapshot,
            auth,
            &direct.value.display_name,
            &requested.value.display_name,
            client,
            kind,
        )?
    };
    if actual != origin {
        return Err(ProxyError::AccountUnavailable);
    }
    Ok((target, origin, record))
}

/// 只验证协议定义的文件槽，工具参数里的任意 JSON 不被解释成网关引用。
pub(crate) async fn refuse_messages_references(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    if state.platform().is_none() {
        return crate::stored_refs::refuse_in_messages(None, body);
    }
    fn scrub(node: &mut Value, ids: &mut Vec<String>) -> Result<(), ProxyError> {
        match node {
            Value::Array(items) => {
                for item in items {
                    scrub(item, ids)?;
                }
            }
            Value::Object(object) => {
                if object
                    .get("source")
                    .and_then(|source| source.get("type"))
                    .and_then(Value::as_str)
                    == Some("file")
                {
                    let id = object
                        .get("source")
                        .and_then(|source| source.get("file_id"))
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            ProxyError::InvalidRequest(
                                "source.file_id must be a nonempty string".into(),
                            )
                        })?;
                    ids.push(id.to_owned());
                    object.remove("source");
                }
                if object.get("type").and_then(Value::as_str) == Some("container_upload") {
                    let id = object
                        .get("file_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            ProxyError::InvalidRequest("file_id must be a nonempty string".into())
                        })?;
                    ids.push(id.to_owned());
                    object.remove("file_id");
                }
                let skip_input = object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.ends_with("tool_use"));
                for (key, value) in object {
                    if key != "input" || !skip_input {
                        scrub(value, ids)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut sanitized = body.clone();
    let mut ids = Vec::new();
    if let Some(messages) = sanitized.get_mut("messages") {
        scrub(messages, &mut ids)?;
    }
    crate::stored_refs::refuse_in_messages(state.platform(), &sanitized)?;
    if ids.is_empty() {
        return Ok(());
    }
    let wanted = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ProxyError::InvalidRequest("model field missing".into()))?;
    let (_, message_origin) = resolve_target_for_consumer(
        state,
        snapshot,
        auth,
        wanted,
        wanted,
        client,
        NativeObjectKind::MessageBatch,
        consumer,
    )?;
    ids.sort();
    ids.dedup();
    for id in ids {
        let (_, file_origin, _) = crate::attribution::detached(lookup_at_origin(
            state,
            auth,
            client,
            &id,
            NativeObjectKind::File,
            None,
            Some(consumer),
        ))
        .await?;
        if file_origin.provider_key_id != message_origin.provider_key_id
            || file_origin.boundary != message_origin.boundary
        {
            return Err(not_found());
        }
    }
    Ok(())
}

fn generated_file_ids(value: &Value) -> Vec<String> {
    fn block(value: &Value, ids: &mut Vec<String>) {
        match value.get("type").and_then(Value::as_str) {
            Some("container_upload" | "code_execution_output" | "bash_code_execution_output") => {
                if let Some(id) = value.get("file_id").and_then(Value::as_str) {
                    ids.push(id.to_owned());
                }
            }
            Some(
                "code_execution_tool_result"
                | "bash_code_execution_tool_result"
                | "code_execution_result"
                | "bash_code_execution_result",
            ) => {
                if let Some(content) = value.get("content") {
                    match content {
                        Value::Array(items) => items.iter().for_each(|item| block(item, ids)),
                        _ => block(content, ids),
                    }
                }
            }
            _ => {}
        }
    }
    let mut ids = Vec::new();
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for item in content {
            block(item, &mut ids);
        }
    }
    if let Some(content) = value.get("content_block") {
        block(content, &mut ids);
    }
    ids.sort();
    ids.dedup();
    ids
}

/// 原生执行结果中的文件先固定归属，再向调用方释放可用于下载的标识。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn register_output_files_at_origin(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    requested_model: &str,
    direct_model: &str,
    expected_origin: Option<&OriginBinding>,
    value: &Value,
) -> Result<(), ProxyError> {
    if state.platform().is_none() {
        return Ok(());
    }
    let ids = generated_file_ids(value);
    if ids.is_empty() {
        return Ok(());
    }
    let expected_origin = expected_origin.ok_or_else(dependency)?;
    let snapshot = state.snapshot.load();
    let (target, origin) = resolve_target_for_request(
        state,
        &snapshot,
        auth,
        direct_model,
        requested_model,
        client,
        NativeObjectKind::File,
    )?;
    let (_, message_origin) = resolve_target_for_request(
        state,
        &snapshot,
        auth,
        direct_model,
        requested_model,
        client,
        NativeObjectKind::MessageBatch,
    )?;
    // 应答沿用派发时捕获的账号；配置轮换后不能把旧账号文件登记到新账号。
    if expected_origin != &origin {
        return Err(dependency());
    }
    if origin.provider_key_id != message_origin.provider_key_id
        || origin.boundary != message_origin.boundary
    {
        return Err(dependency());
    }
    let registry = registry(state)?;
    for id in ids {
        crate::jobs::require_safe_upstream_id(&id).map_err(|_| invalid_identity())?;
        match registry
            .lookup(LookupRequest {
                consumer_family: None,
                api_key_id: api_key_id(auth)?,
                locator: ObjectLocator {
                    protocol: NativeProtocol::Anthropic,
                    kind: NativeObjectKind::File,
                    raw_id: id.clone(),
                    origin: Some(origin.clone()),
                    parent_id: None,
                },
            })
            .await
        {
            Ok(reply)
                if reply.state == "found"
                    && reply.record.as_ref().is_some_and(|record| {
                        !record.deleted
                            && record.raw_id == id
                            && record.protocol == Some(NativeProtocol::Anthropic)
                            && record.kind == Some(NativeObjectKind::File)
                            && record.origin.as_ref() == Some(&origin)
                    }) =>
            {
                continue
            }
            Err(NativeRegistryError::NotFound) => {}
            _ => return Err(dependency()),
        }
        let requested = crate::model_resolve::resolve_model(&snapshot, requested_model)
            .ok_or_else(not_found)?;
        let requested_id = Uuid::parse_str(&requested.id).map_err(|_| dependency())?;
        let registration = begin_for_requested(
            state,
            auth,
            &target,
            origin.clone(),
            NativeObjectKind::File,
            Vec::new(),
            client,
            Some((requested_id, requested.value.display_name.clone())),
        )
        .await?;
        registration.bind(&serde_json::json!({"id": id})).await?;
    }
    Ok(())
}

pub(crate) fn raw_body(status: StatusCode, headers: &HeaderMap, bytes: Bytes) -> Response {
    if !status.is_success() {
        if let Some(class) = crate::platform_errors::relayed_error(status.as_u16(), &bytes) {
            return (
                class.status(),
                [(header::CONTENT_TYPE, "application/json")],
                crate::error::anthropic_platform_body(&class),
            )
                .into_response();
        }
    }
    let mut response = (status, bytes).into_response();
    for name in [
        "content-type",
        "content-disposition",
        "request-id",
        "retry-after",
        "retry-after-ms",
    ] {
        if let Some(value) = headers.get(name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    for (name, value) in headers {
        if name.as_str().starts_with("anthropic-ratelimit-") {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    response
}
pub(crate) fn json_body(status: StatusCode, headers: &HeaderMap, bytes: Bytes) -> Response {
    let mut response = raw_body(status, headers, bytes);
    if !response.headers().contains_key(header::CONTENT_TYPE) {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    response
}

#[derive(Default)]
pub(crate) struct Audit {
    target: Option<JobTarget>,
    monitor: Vec<aisix_core::GuardrailMonitorHit>,
    redactions: crate::redact::RedactionCounts,
    enforced: Vec<aisix_core::GuardrailEnforcedHit>,
    scores: Vec<aisix_core::GuardrailScore>,
    bypass: String,
    applied: Vec<aisix_core::AppliedGuardrail>,
}
impl Audit {
    pub(crate) fn resolved(&mut self, target: &JobTarget) {
        self.target = Some(target.clone());
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
        self.target = Some(target.clone());
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
        self.target = Some(target.clone());
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
    operation_id: Option<Uuid>,
) -> Response {
    let elapsed = started.elapsed();
    if let Err(error) = &result {
        crate::usage_attr::note_client_error_body(snapshot, || error.anthropic_body());
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
    crate::request_metrics::record(
        state,
        crate::normalize_endpoint_label(path),
        crate::request_metrics::Caller::new(auth),
        crate::request_metrics::Upstream {
            provider,
            model,
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
        inbound_protocol: "anthropic".into(),
        inference_stage: Some(stage),
        native_operation_id: operation_id,
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
    let pk_id = target
        .map(|target| target.pk_entry.id.as_str())
        .unwrap_or_default();
    let pk = crate::usage_attr::ResolvedPk::resolve(snapshot, pk_id);
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
        Err(error) => error.into_anthropic_response(),
    };
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

fn model_hint(params: &HashMap<String, String>, client: &ClientContext) -> Option<String> {
    params
        .get("model")
        .map(String::as_str)
        .or_else(|| {
            client
                .headers
                .get("x-aisix-model")
                .and_then(|value| value.to_str().ok())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    bytes: Bytes,
}

pub(crate) async fn create_file(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    params: HashMap<String, String>,
    query: Option<String>,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let mut operation_id = None;
    let result = async {
        let mut multipart = multipart
            .map_err(|_| ProxyError::InvalidRequest("invalid multipart form data".into()))?;
        let mut parts = Vec::new();
        let mut wanted = model_hint(&params, &client);
        while let Some(field) = multipart.next_field().await.map_err(|error| {
            crate::error::proxy_error_from_multipart(
                error,
                state.request_body_limit_for("/v1/files"),
                "malformed multipart body",
            )
        })? {
            let name = field.name().unwrap_or_default().to_owned();
            let filename = field.file_name().map(str::to_owned);
            let content_type = field.content_type().map(str::to_owned);
            let bytes = field.bytes().await.map_err(|error| {
                crate::error::proxy_error_from_multipart(
                    error,
                    state.request_body_limit_for("/v1/files"),
                    "malformed multipart field",
                )
            })?;
            if name == "model" {
                wanted = Some(
                    std::str::from_utf8(&bytes)
                        .map_err(|_| ProxyError::InvalidRequest("model must be UTF-8".into()))?
                        .trim()
                        .to_owned(),
                );
            } else {
                parts.push(Part {
                    name,
                    filename,
                    content_type,
                    bytes,
                });
            }
        }
        let wanted = wanted.filter(|name| !name.is_empty()).ok_or_else(|| {
            ProxyError::InvalidRequest(
                "pass model or x-aisix-model to select a direct model".into(),
            )
        })?;
        crate::usage_attr::note_failure_request(&snapshot, || {
            use sha2::Digest;
            Value::Array(parts.iter().map(|part| {
                if part.name == "file" {
                    serde_json::json!({"name":part.name,"sha256":format!("{:x}", sha2::Sha256::digest(&part.bytes))})
                } else {
                    serde_json::json!({"name":part.name,"value":String::from_utf8_lossy(&part.bytes)})
                }
            }).collect()).to_string()
        });
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, Some(&wanted));
        if !parts.iter().any(|part| part.name == "file") {
            return Err(ProxyError::InvalidRequest(
                "multipart body must include a file field".into(),
            ));
        }
        let (target, origin) = resolve_target(
            &state,
            &snapshot,
            &auth,
            &wanted,
            &client,
            NativeObjectKind::File,
        )?;
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let mut form = reqwest::multipart::Form::new();
        for part in parts {
            let bytes = if part.name == "file" {
                audit
                    .input(&state, &snapshot, &auth, &target, &part.bytes, true)
                    .await?
                    .map(Bytes::from)
                    .unwrap_or(part.bytes)
            } else {
                audit
                    .input(&state, &snapshot, &auth, &target, &part.bytes, false)
                    .await?;
                part.bytes
            };
            let length = bytes.len() as u64;
            let mut outgoing = reqwest::multipart::Part::stream_with_length(bytes, length);
            if let Some(filename) = part.filename {
                outgoing = outgoing.file_name(filename);
            }
            if let Some(content_type) = part.content_type {
                outgoing = outgoing
                    .mime_str(&content_type)
                    .map_err(|_| ProxyError::InvalidRequest("invalid file content-type".into()))?;
            }
            form = form.part(part.name, outgoing);
        }
        let registration = begin(
            &state,
            &auth,
            &target,
            origin,
            NativeObjectKind::File,
            Vec::new(),
            &client,
        )
        .await?;
        operation_id = Some(registration.operation_id);
        let (status, headers, bytes) = send(
            &state,
            &target,
            &client,
            Method::POST,
            "/files",
            &strip_model_query(query.as_deref()),
            Body::Multipart(form),
        )
        .await?;
        if status.is_success() {
            let value = serde_json::from_slice(&bytes).map_err(|_| invalid_identity())?;
            registration.bind(&value).await?;
            audit.output(&state, &auth, &target, &bytes).await?;
        }
        Ok((json_body(status, &headers, bytes), target))
    }
    .await;
    finish(
        &state,
        &snapshot,
        crate::operation::FILES,
        Method::POST,
        "/v1/files",
        &auth,
        &client,
        started,
        result,
        audit,
        InferenceStage::Management,
        operation_id,
    )
    .await
}

/// 页游标只在当前租户和原账号边界内有效，不能拿其他账号的页定位对象。
fn page_sealer(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    origin: &OriginBinding,
) -> Result<crate::opaque_id::Sealer, ProxyError> {
    let platform = state.platform().ok_or(ProxyError::AccountUnavailable)?;
    let owner = format!(
        "native:anthropic:file:page:{}:{}",
        crate::platform::tenant_owner(auth),
        serde_json::to_string(origin).map_err(|_| ProxyError::AccountUnavailable)?
    );
    Ok(crate::opaque_id::Sealer::of(platform, &owner))
}

pub(crate) async fn list_files(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    params: HashMap<String, String>,
    query: Option<String>,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let wanted = model_hint(&params, &client).ok_or_else(|| {
            ProxyError::InvalidRequest(
                "pass model or x-aisix-model to select a direct model".into(),
            )
        })?;
        let (target, origin) = resolve_target(
            &state,
            &snapshot,
            &auth,
            &wanted,
            &client,
            NativeObjectKind::File,
        )?;
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let sealer = page_sealer(&state, &auth, &origin)?;
        let mut pairs = Vec::new();
        for pair in strip_model_query(query.as_deref())
            .split('&')
            .filter(|pair| !pair.is_empty())
        {
            let Some((key, value)) = url::form_urlencoded::parse(pair.as_bytes()).next() else {
                continue;
            };
            if key == "page" {
                let raw = sealer
                    .unseal(crate::opaque_id::IdKind::Item, &value)
                    .ok_or_else(not_found)?;
                pairs.push(
                    url::form_urlencoded::Serializer::new(String::new())
                        .append_pair("page", &raw)
                        .finish(),
                );
            } else {
                pairs.push(pair.to_owned());
            }
        }
        audit
            .input(&state, &snapshot, &auth, &target, &[], false)
            .await?;
        let (status, headers, bytes) = send(
            &state,
            &target,
            &client,
            Method::GET,
            "/files",
            &pairs.join("&"),
            Body::Empty,
        )
        .await?;
        if !status.is_success() {
            return Ok((json_body(status, &headers, bytes), target));
        }
        let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid_identity())?;
        let data = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(invalid_identity)?;
        let raw_ids = data
            .iter()
            .map(|item| {
                item.get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(invalid_identity)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let registry = registry(&state)?;
        let mut owned = HashSet::new();
        if raw_ids.is_empty() {
            let reply = registry
                .owned(OwnedRequest {
                    api_key_id: api_key_id(&auth)?,
                    protocol: NativeProtocol::Anthropic,
                    kind: NativeObjectKind::File,
                    origin: origin.clone(),
                    parent_id: None,
                    raw_ids: Vec::new(),
                })
                .await
                .map_err(registry_error)?;
            if reply.state != "owned" || !reply.records.is_empty() {
                return Err(ProxyError::AccountUnavailable);
            }
        }
        for ids in raw_ids.chunks(100) {
            let reply = registry
                .owned(OwnedRequest {
                    api_key_id: api_key_id(&auth)?,
                    protocol: NativeProtocol::Anthropic,
                    kind: NativeObjectKind::File,
                    origin: origin.clone(),
                    parent_id: None,
                    raw_ids: ids.to_vec(),
                })
                .await
                .map_err(registry_error)?;
            if reply.state != "owned" {
                return Err(ProxyError::AccountUnavailable);
            }
            for record in reply.records {
                if record.deleted
                    || record.origin.as_ref() != Some(&origin)
                    || record.protocol != Some(NativeProtocol::Anthropic)
                    || record.kind != Some(NativeObjectKind::File)
                    || !ids.contains(&record.raw_id)
                {
                    return Err(ProxyError::AccountUnavailable);
                }
                let requested = record
                    .requested_model_id
                    .and_then(|id| snapshot.models.get_by_id(&id.to_string()));
                if requested.is_some_and(|model| {
                    auth.key().can_access(&snapshot, &model.value.display_name)
                }) {
                    owned.insert(record.raw_id);
                }
            }
        }
        let data = value
            .get_mut("data")
            .and_then(Value::as_array_mut)
            .ok_or_else(invalid_identity)?;
        data.retain(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| owned.contains(id))
        });
        let first = data
            .first()
            .and_then(|item| item.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        let last = data
            .last()
            .and_then(|item| item.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        if value.get("first_id").is_some() {
            value["first_id"] = first;
        }
        if value.get("last_id").is_some() {
            value["last_id"] = last;
        }
        if let Some(cursor) = value.get("next_page").and_then(Value::as_str) {
            value["next_page"] = sealer.seal(crate::opaque_id::IdKind::Item, cursor).into();
        }
        let bytes = Bytes::from(serde_json::to_vec(&value).map_err(|_| invalid_identity())?);
        audit.output(&state, &auth, &target, &bytes).await?;
        Ok((json_body(status, &headers, bytes), target))
    }
    .await;
    finish(
        &state,
        &snapshot,
        crate::operation::FILES,
        Method::GET,
        "/v1/files",
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

pub(crate) async fn addressed_file(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    id: String,
    query: Option<String>,
    method: Method,
    content: bool,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let path = if content {
        format!("/v1/files/{id}/content")
    } else {
        format!("/v1/files/{id}")
    };
    let result = async {
        let (target, _, record) =
            lookup(&state, &auth, &client, &id, NativeObjectKind::File).await?;
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        audit
            .input(&state, &snapshot, &auth, &target, &[], false)
            .await?;
        let upstream = if content {
            format!("/files/{id}/content")
        } else {
            format!("/files/{id}")
        };
        let (status, headers, bytes) = send(
            &state,
            &target,
            &client,
            method.clone(),
            &upstream,
            &strip_model_query(query.as_deref()),
            Body::Empty,
        )
        .await?;
        if status.is_success() {
            if content {
                audit.output(&state, &auth, &target, &bytes).await?;
            } else {
                let value: Value =
                    serde_json::from_slice(&bytes).map_err(|_| invalid_identity())?;
                if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                    return Err(invalid_identity());
                }
            }
            if method == Method::DELETE {
                let reply = registry(&state)?
                    .tombstone(TombstoneRequest {
                        api_key_id: api_key_id(&auth)?,
                        record_id: record.record_id,
                        version: record.version,
                    })
                    .await
                    .map_err(|_| dependency())?;
                if reply.state != "deleted" {
                    return Err(dependency());
                }
            }
            if !content {
                audit.output(&state, &auth, &target, &bytes).await?;
            }
        }
        Ok((
            if content {
                raw_body(status, &headers, bytes)
            } else {
                json_body(status, &headers, bytes)
            },
            target,
        ))
    }
    .await;
    finish(
        &state,
        &snapshot,
        crate::operation::FILES,
        method,
        &path,
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

#[cfg(test)]
mod tests {
    use super::generated_file_ids;
    use serde_json::json;

    #[test]
    fn generated_files_are_only_collected_from_official_output_slots() {
        let value = json!({"content":[
            {"type":"bash_code_execution_tool_result","content":{"type":"bash_code_execution_result","content":[
                {"type":"bash_code_execution_output","file_id":"file_a"}]}},
            {"type":"code_execution_tool_result","content":{"type":"code_execution_result","content":[
                {"type":"code_execution_output","file_id":"file_b"}]}},
            {"type":"container_upload","file_id":"file_a"},
            {"type":"tool_use","input":{"type":"container_upload","file_id":"file_not_output"}},
            {"type":"text","text":"file_text"}
        ]});
        assert_eq!(generated_file_ids(&value), ["file_a", "file_b"]);
        assert_eq!(
            generated_file_ids(&json!({"type":"content_block_start", "content_block":{
            "type":"container_upload", "file_id":"file_stream"}})),
            ["file_stream"]
        );
    }

    #[tokio::test]
    async fn successful_body_limit_never_truncates_binary_into_a_success() {
        let upstream = wiremock::MockServer::builder().start().await;
        let bytes = vec![0, 255, 1, 0];
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&upstream)
            .await;
        let client = reqwest::Client::new();
        let response = client.get(upstream.uri()).send().await.unwrap();
        assert_eq!(
            super::read_success_body(response, bytes.len())
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        let response = client.get(upstream.uri()).send().await.unwrap();
        assert!(super::read_success_body(response, bytes.len() - 1)
            .await
            .is_err());
    }
}
