//! 存储 Chat 使用原生标识，所有读写都以持久归属和当前账号边界准入。
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use crate::request_json::Json;
use aisix_core::native_objects::{
    configured_native_origin, BeginRequest, BindRequest, InferenceStage, NativeObjectKind,
    NativeProtocol, NativeRecord, OriginBinding, OwnedRequest, TombstoneRequest,
};
use aisix_core::{Model, ProviderKey};
use aisix_gateway::{BridgeError, ChatChunk, ChatChunkStream, ChatDelta, ChatFormat};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use uuid::Uuid;

use super::openai::{self, Audit, Body};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::reject::AisixPath;
use crate::state::ProxyState;

fn not_found() -> ProxyError {
    ProxyError::JobResourceNotFound("chat_completion".into())
}
fn dependency() -> ProxyError {
    ProxyError::Bridge(BridgeError::DependencyUnavailable)
}
fn invalid_response() -> ProxyError {
    ProxyError::Bridge(BridgeError::UpstreamDecode(
        "stored chat identity is inconsistent".into(),
    ))
}

pub(crate) fn requested(req: &ChatFormat) -> bool {
    req.extra.get("store") == Some(&Value::Bool(true))
}

pub(crate) struct CreationBinding {
    requested_model_id: Uuid,
    requested_model: String,
    origin: OriginBinding,
    upstream_model: String,
    upstream_provider: String,
}

/// 取实际派发使用的不可变配置，不能在回答后重读同 ID 下已轮换的账号。
#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_creation(
    state: &ProxyState,
    req: &ChatFormat,
    requested_model_id: &str,
    target_model_id: &str,
    target: &Model,
    provider_key_id: &str,
    provider: &ProviderKey,
) -> Option<Result<CreationBinding, ProxyError>> {
    if state.platform().is_none()
        || !requested(req)
        || provider.provider.trim().eq_ignore_ascii_case("cohere")
        || !(provider.provider == "openai" || provider.adapter == Some(aisix_core::Adapter::Openai))
        || crate::dispatch::resolve_bridge(&state.hub, provider)
            .is_none_or(|bridge| bridge.wire_protocol() != "openai")
    {
        return None;
    }
    Some((|| {
        openai::registry(state)?;
        let platform = state.platform().ok_or_else(dependency)?;
        let provider_key_id = Uuid::parse_str(provider_key_id).map_err(|_| dependency())?;
        Ok(CreationBinding {
            requested_model_id: Uuid::parse_str(requested_model_id).map_err(|_| dependency())?,
            requested_model: req.model.clone(),
            origin: OriginBinding {
                surface: None,
                model_id: Uuid::parse_str(target_model_id).map_err(|_| dependency())?,
                provider_key_id,
                boundary: configured_native_origin(
                    &platform.id_key,
                    provider_key_id,
                    provider,
                    NativeProtocol::Openai,
                    NativeObjectKind::ChatCompletion,
                )
                .map_err(|_| dependency())?,
            },
            upstream_model: target.upstream_model().unwrap_or_default().to_owned(),
            upstream_provider: provider.provider.clone(),
        })
    })())
}

/// 生成请求的真实直连目标确定后再固定归属，不能拿虚拟模型重新选一个账号。
pub(crate) async fn register_created(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    binding: &CreationBinding,
    raw_id: &str,
    created: Option<i64>,
) -> Result<(), ProxyError> {
    crate::jobs::require_safe_upstream_id(raw_id).map_err(|_| invalid_response())?;
    let snapshot = state.snapshot.load();
    let auth = openai::current_auth(&snapshot, auth)?;
    let requested = snapshot
        .models
        .get_by_id(&binding.requested_model_id.to_string())
        .ok_or_else(dependency)?;
    let target = snapshot
        .models
        .get_by_id(&binding.origin.model_id.to_string())
        .ok_or_else(dependency)?;
    let provider = crate::dispatch::resolve_provider_key(&snapshot, &target.value)?;
    let platform = state.platform().ok_or_else(dependency)?;
    let current = configured_native_origin(
        &platform.id_key,
        binding.origin.provider_key_id,
        &provider.value,
        NativeProtocol::Openai,
        NativeObjectKind::ChatCompletion,
    )
    .map_err(|_| dependency())?;
    if provider.id != binding.origin.provider_key_id.to_string()
        || current != binding.origin.boundary
        || target.value.upstream_model().unwrap_or_default() != binding.upstream_model
        || !auth
            .key()
            .can_call(NativeObjectKind::ChatCompletion.endpoint_family())
        || !auth
            .key()
            .can_access(&snapshot, &requested.value.display_name)
        || !crate::routing::target_in_groups(
            &snapshot,
            &target.value,
            &auth.key().allowed_provider_groups,
        )
    {
        return Err(dependency());
    }
    crate::dispatch::check_ip_access(&requested.value, &client.source_ip)?;
    crate::dispatch::check_ip_access(&target.value, &client.source_ip)?;
    let origin = &binding.origin;
    let registry = openai::registry(state)?;
    let operation_id = Uuid::new_v4();
    let api_key_id = openai::api_key_id(&auth)?;
    let reply = registry
        .begin(BeginRequest {
            upstream_input_text: None,
            input_file_id: None,
            operation_id,
            api_key_id,
            protocol: NativeProtocol::Openai,
            kind: NativeObjectKind::ChatCompletion,
            origin: origin.clone(),
            parent_id: None,
            requested_model_id: binding.requested_model_id,
            requested_model: binding.requested_model.clone(),
            upstream_model: binding.upstream_model.clone(),
            upstream_provider: binding.upstream_provider.clone(),
            operation: "chat".into(),
            request_id: client.request_id.clone(),
            asynchronous: false,
            requested_service_tier: None,
            entries: Vec::new(),
            observation_headers: BTreeMap::new(),
        })
        .await
        .map_err(|_| dependency())?;
    if reply.state != "pending_submission"
        || reply.operation_id != Some(operation_id)
        || reply.origin.as_ref() != Some(origin)
    {
        return Err(dependency());
    }
    let reply = registry
        .bind(BindRequest {
            existing_record_id: None,
            objects: Vec::new(),
            operation_id,
            api_key_id,
            raw_id: raw_id.to_owned(),
            provider_created_at: created
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
            provider_expires_at: None,
        })
        .await
        .map_err(|_| dependency())?;
    if reply.state != "bound"
        || !reply.record.as_ref().is_some_and(|record| {
            !record.deleted
                && record.raw_id == raw_id
                && record.operation_id == Some(operation_id)
                && record.origin.as_ref() == Some(origin)
                && record.protocol == Some(NativeProtocol::Openai)
                && record.kind == Some(NativeObjectKind::ChatCompletion)
                && record.parent_id.is_none()
                && record.requested_model_id == Some(binding.requested_model_id)
        })
    {
        return Err(dependency());
    }
    Ok(())
}

/// 绑定失败后只取回上游已用的计数，禁止内容或原生 ID 在错误之前离开网关。
pub(crate) fn bind_stream(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    binding: Result<CreationBinding, ProxyError>,
    mut upstream: ChatChunkStream,
) -> ChatChunkStream {
    Box::pin(crate::request_id::in_request_span(async_stream::stream! {
        let mut bound: Option<String> = None;
        while let Some(item) = upstream.next().await {
            let chunk = match item { Ok(chunk) => chunk, Err(error) => { yield Err(error); return; } };
            if chunk.usage.as_ref().is_some_and(|usage| usage.total_input_tokens() > 0 && usage.completion_tokens > 0) {
                // 正数来自真实上游，两个计量分量均无需估算；取消结算不能再等冷分词表。
                crate::attribution::clear_table_needs();
            }
            if bound.is_none() {
                if let Some(usage) = chunk.usage.as_ref().or(chunk.accounting_usage.as_ref()) {
                    // 等待持久绑定之前交给流尾守卫，调用方此时离开也保留已见计量。
                    yield Ok(ChatChunk { id: chunk.id.clone(), model: chunk.model.clone(), choice_index: 0,
                        delta: ChatDelta::default(), finish_reason: None, logprobs: None, usage: None,
                        native_envelope: None, service_tier: chunk.service_tier.clone(), accounting_usage: Some(usage.clone()) });
                }
            }
            let registration = if let Some(id) = &bound {
                if chunk.id.is_empty() || &chunk.id == id { Ok(()) } else { Err(invalid_response()) }
            } else {
                match &binding {
                    Ok(binding) => register_created(&state, &auth, &client, binding, &chunk.id,
                        chunk.native_envelope.as_ref().and_then(|envelope| envelope.created)).await,
                    Err(_) => Err(dependency()),
                }
            };
            if registration.is_err() {
                let mut usage = chunk.usage.clone().or(chunk.accounting_usage.clone());
                let mut tier = chunk.service_tier.clone();
                let id = chunk.id.clone(); let model = chunk.model.clone();
                while let Some(item) = upstream.next().await {
                    match item {
                        Ok(chunk) => {
                            if let Some(reported) = chunk.usage.as_ref().or(chunk.accounting_usage.as_ref()) {
                                usage = Some(usage.as_ref().map_or_else(|| reported.clone(), |previous| previous.max_fieldwise(reported)));
                            }
                            if chunk.service_tier.is_some() { tier = chunk.service_tier; }
                        }
                        Err(_) => break,
                    }
                }
                if let Some(usage) = usage {
                    // accounting_usage 不编码给客户端，原有流尾仍按真实上游计量结算。
                    yield Ok(ChatChunk { id, model, choice_index: 0, delta: ChatDelta::default(),
                        finish_reason: None, logprobs: None, usage: None, native_envelope: None,
                        service_tier: tier, accounting_usage: Some(usage) });
                }
                yield Err(BridgeError::DependencyUnavailable);
                return;
            }
            if bound.is_none() { bound = Some(chunk.id.clone()); }
            yield Ok(chunk);
        }
    }))
}

fn display_completion(value: &mut Value, record: &NativeRecord) {
    if !record.requested_model.is_empty() && value.get("model").is_some() {
        value["model"] = Value::String(record.requested_model.clone());
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("system_fingerprint");
    }
}
fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}
fn query_string(pairs: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            pairs
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .finish()
}
fn selector(headers: &HeaderMap, pairs: &[(String, String)]) -> Option<String> {
    headers
        .get("x-aisix-model")
        .and_then(|value| value.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            pairs
                .iter()
                .find(|(key, value)| key == "model" && !value.trim().is_empty())
                .map(|(_, value)| value.clone())
        })
}

pub(crate) async fn retrieve(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(state, auth, client, id, query, Method::GET, false, None).await
}
pub(crate) async fn messages(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(state, auth, client, id, query, Method::GET, true, None).await
}
pub(crate) async fn delete(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(state, auth, client, id, query, Method::DELETE, false, None).await
}
pub(crate) async fn update(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, crate::request_json::JsonRejection>,
) -> Response {
    let started = Instant::now();
    match body {
        Ok(Json(body)) => {
            addressed(
                state,
                auth,
                client,
                id,
                query,
                Method::POST,
                false,
                Some(body),
            )
            .await
        }
        Err(error) => crate::reject::reject_before_dispatch(
            &state,
            "POST",
            "/v1/chat/completions/:id",
            &client.request_id,
            Some(&auth.entry.id),
            started,
            crate::reject::Envelope::OpenAi,
            crate::error::proxy_error_from_json_rejection(
                error,
                state.request_body_limit_for("/v1/chat/completions/:id"),
            ),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn addressed(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    id: String,
    query: Option<String>,
    method: Method,
    messages: bool,
    body: Option<Value>,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    if let Some(body) = &body {
        crate::usage_attr::note_failure_request(&snapshot, || body.to_string());
        crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, None);
    }
    let mut audit = Audit::default();
    let path = format!(
        "/v1/chat/completions/{id}{}",
        if messages { "/messages" } else { "" }
    );
    let result = async {
        crate::jobs::require_safe_upstream_id(&id).map_err(|_| not_found())?;
        let (target, _, record) = openai::lookup(
            &state,
            &auth,
            &client,
            &id,
            NativeObjectKind::ChatCompletion,
            None,
        )
        .await?;
        audit.resolved(&target);
        if body.is_some() {
            crate::usage_attr::hold_ahead_of_chain(
                &state,
                &snapshot,
                &auth,
                Some(target.display_name()),
            );
        }

        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        let bytes = body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| invalid_response())?
            .unwrap_or_default();
        let masked = audit
            .input(&state, &snapshot, &auth, &target, &bytes, body.is_some())
            .await?;
        let send_body = if body.is_some() {
            let bytes = masked.unwrap_or(bytes);
            crate::usage_attr::note_screened_request(&snapshot, || {
                if audit.bypass.is_empty() {
                    String::from_utf8_lossy(&bytes).into_owned()
                } else {
                    String::new()
                }
            });
            Body::Json(Bytes::from(bytes))
        } else {
            Body::Empty
        };
        let reply = openai::send(
            &state,
            &target,
            &client,
            method.clone(),
            &path[3..],
            query.as_deref().unwrap_or_default(),
            send_body,
        )
        .await?;
        let mut bytes = reply.bytes;
        if method == Method::DELETE && bytes.is_empty() {
            return Err(invalid_response());
        }
        if !bytes.is_empty() {
            let mut value: Value =
                serde_json::from_slice(&bytes).map_err(|_| invalid_response())?;
            if !messages && value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(invalid_response());
            }
            if method == Method::DELETE
                && value.get("deleted").and_then(Value::as_bool) != Some(true)
            {
                return Err(invalid_response());
            }
            if !messages {
                display_completion(&mut value, &record);
            }
            bytes = Bytes::from(serde_json::to_vec(&value).map_err(|_| invalid_response())?);
        }
        if method == Method::DELETE {
            let reply = openai::registry(&state)?
                .tombstone(TombstoneRequest {
                    api_key_id: openai::api_key_id(&auth)?,
                    record_id: record.record_id,
                    version: record.version,
                })
                .await
                .map_err(|_| dependency())?;
            if reply.state != "deleted" {
                return Err(dependency());
            }
        }
        audit.output(&state, &auth, &target, &bytes).await?;
        Ok((
            openai::raw_body(reply.status, &reply.headers, bytes),
            target,
        ))
    }
    .await;
    openai::finish(
        &state,
        &snapshot,
        crate::operation::CHAT,
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

pub(crate) async fn list(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let path = "/v1/chat/completions";
    let result = async {
        let mut pairs = query_pairs(query.as_deref());
        let cursors: Vec<_> = pairs
            .iter()
            .filter(|(key, _)| key == "after")
            .map(|(_, value)| value.clone())
            .collect();
        let (target, origin) = if let Some(wanted) = selector(&headers, &pairs) {
            openai::resolve(
                &state,
                &snapshot,
                &auth,
                &client,
                &wanted,
                NativeObjectKind::ChatCompletion,
            )?
        } else if let Some(cursor) = cursors.first() {
            let (target, origin, _) = openai::lookup(
                &state,
                &auth,
                &client,
                cursor,
                NativeObjectKind::ChatCompletion,
                None,
            )
            .await?;
            (target, origin)
        } else {
            return Err(ProxyError::InvalidRequest(
                "model or x-aisix-model must select the stored Chat account".into(),
            ));
        };
        audit.resolved(&target);
        for cursor in cursors {
            let (_, cursor_origin, _) = openai::lookup(
                &state,
                &auth,
                &client,
                &cursor,
                NativeObjectKind::ChatCompletion,
                None,
            )
            .await?;
            if cursor_origin != origin {
                return Err(not_found());
            }
        }
        if headers
            .get("x-aisix-model")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value.trim().is_empty())
        {
            for (key, value) in &mut pairs {
                if key == "model" {
                    *value = target
                        .model_entry
                        .value
                        .upstream_model()
                        .unwrap_or_default()
                        .to_owned();
                }
            }
        }
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        audit
            .input(&state, &snapshot, &auth, &target, &[], false)
            .await?;
        let registry = openai::registry(&state)?;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            let reply = openai::send(
                &state,
                &target,
                &client,
                Method::GET,
                "/chat/completions",
                &query_string(&pairs),
                Body::Empty,
            )
            .await?;
            let mut value: Value =
                serde_json::from_slice(&reply.bytes).map_err(|_| invalid_response())?;
            let data = value
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(invalid_response)?;
            let ids = data
                .iter()
                .map(|item| {
                    item.get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(invalid_response)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let owned = registry
                .owned(OwnedRequest {
                    api_key_id: openai::api_key_id(&auth)?,
                    protocol: NativeProtocol::Openai,
                    kind: NativeObjectKind::ChatCompletion,
                    origin: origin.clone(),
                    parent_id: None,
                    raw_ids: ids.clone(),
                })
                .await
                .map_err(|_| dependency())?;
            if owned.state != "owned" {
                return Err(dependency());
            }
            let records: HashMap<_, _> = owned
                .records
                .into_iter()
                .filter(|r| {
                    !r.deleted
                        && r.origin.as_ref() == Some(&origin)
                        && r.kind == Some(NativeObjectKind::ChatCompletion)
                        && r.protocol == Some(NativeProtocol::Openai)
                        && r.parent_id.is_none()
                        && r.requested_model_id
                            .and_then(|id| snapshot.models.get_by_id(&id.to_string()))
                            .is_some_and(|entry| {
                                auth.key().can_access(&snapshot, &entry.value.display_name)
                            })
                })
                .map(|record| (record.raw_id.clone(), record))
                .collect();
            let mut filtered = Vec::new();
            for item in data {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(owned_record) = records.get(id) else {
                    continue;
                };
                // 页内归属只筛候选；每一行仍须通过当前模型、分组、IP 与账号的完整证明。
                let (_, current_origin, current_record) = match openai::lookup(
                    &state,
                    &auth,
                    &client,
                    id,
                    NativeObjectKind::ChatCompletion,
                    None,
                )
                .await
                {
                    Ok(found) => found,
                    Err(ProxyError::JobResourceNotFound(_)) => continue,
                    Err(error) => return Err(error),
                };
                if current_origin != origin || current_record.record_id != owned_record.record_id {
                    return Err(dependency());
                }
                let mut item = item.clone();
                display_completion(&mut item, &current_record);
                filtered.push(item);
            }
            let has_more = value
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !filtered.is_empty() || !has_more {
                value["first_id"] = filtered
                    .first()
                    .and_then(|v| v.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                value["last_id"] = filtered
                    .last()
                    .and_then(|v| v.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                value["has_more"] = json!(has_more && !filtered.is_empty());
                value["data"] = Value::Array(filtered);
                let bytes =
                    Bytes::from(serde_json::to_vec(&value).map_err(|_| invalid_response())?);
                audit.output(&state, &auth, &target, &bytes).await?;
                return Ok((
                    openai::raw_body(reply.status, &reply.headers, bytes),
                    target,
                ));
            }
            let next = ids
                .last()
                .filter(|id| seen.insert((*id).clone()))
                .ok_or_else(invalid_response)?
                .clone();
            pairs.retain(|(key, _)| key != "after");
            pairs.push(("after".into(), next));
        }
        Err(dependency())
    }
    .await;
    openai::finish(
        &state,
        &snapshot,
        crate::operation::CHAT,
        Method::GET,
        path,
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
