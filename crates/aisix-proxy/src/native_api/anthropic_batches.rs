//! 原生批次的路由、归属与结果读取；创建后账号固定，管理操作不重复计推理。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::request_json::Json;
use aisix_core::native_objects::{
    InferenceStage, NativeEntry, NativeObjectKind, NativeProtocol, OwnedRequest, TombstoneRequest,
};
use axum::extract::{Path, RawQuery, State};
use axum::http::Method;
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use super::anthropic::{self, Audit, Body};
use crate::auth::{AnthropicAuthenticatedKey, AuthenticatedKey};
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::state::ProxyState;

const KIND: NativeObjectKind = NativeObjectKind::MessageBatch;
const MAX_ENTRIES: usize = 1000;
const MAX_LIST_SCAN: Duration = Duration::from_secs(30);

fn invalid(message: &str) -> ProxyError {
    ProxyError::InvalidRequest(message.into())
}
fn dependency() -> ProxyError {
    ProxyError::Bridge(aisix_gateway::BridgeError::DependencyUnavailable)
}
fn decode() -> ProxyError {
    ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamDecode(
        "native batch response is inconsistent".into(),
    ))
}

fn model_hint(query: Option<&str>, client: &ClientContext) -> Option<String> {
    query
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(name, value)| name == "model" && !value.is_empty())
                .map(|(_, value)| value.into_owned())
        })
        .or_else(|| {
            client
                .headers
                .get("x-aisix-model")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

fn local_results_url(value: &mut Value) {
    if value.get("results_url").is_some_and(|url| !url.is_null()) {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            value["results_url"] = Value::String(format!("/v1/messages/batches/{id}/results"));
        }
    }
}

pub(crate) async fn create(
    State(state): State<ProxyState>,
    AnthropicAuthenticatedKey(auth): AnthropicAuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, crate::request_json::JsonRejection>,
) -> Response {
    let started = Instant::now();
    let mut body = match body {
        Ok(Json(body)) => body,
        Err(error) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                "/v1/messages/batches",
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::Anthropic,
                crate::error::proxy_error_from_json_rejection(
                    error,
                    state.request_body_limit_for("/v1/messages/batches"),
                ),
            )
        }
    };
    let snapshot = state.snapshot.load();
    crate::usage_attr::note_failure_request(&snapshot, || body.to_string());
    let mut audit = Audit::default();
    let mut operation_id = None;
    let mut stage = InferenceStage::Management;
    let result = async {
        anthropic::registry(&state)?;
        let requests = body
            .get_mut("requests")
            .and_then(Value::as_array_mut)
            .filter(|requests| !requests.is_empty() && requests.len() <= MAX_ENTRIES)
            .ok_or_else(|| invalid("requests must contain 1 to 1000 batch entries"))?;
        let mut ids = HashSet::new();
        let mut entries = Vec::with_capacity(requests.len());
        let mut primary = None;
        let mut reservations = Vec::new();
        for request in requests {
            let id = request
                .get("custom_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .ok_or_else(|| invalid("requests[].custom_id must be a non-empty string"))?
                .to_owned();
            if !ids.insert(id.clone()) {
                return Err(invalid("requests[].custom_id must be unique"));
            }
            let params = request
                .get_mut("params")
                .filter(|params| params.is_object())
                .ok_or_else(|| invalid("requests[].params must be an object"))?;
            let requested = params
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .ok_or_else(|| invalid("requests[].params.model is required"))?
                .to_owned();
            crate::usage_attr::hold_ahead_of_chain(&state, &snapshot, &auth, Some(&requested));
            let (target, origin) =
                anthropic::resolve_target(&state, &snapshot, &auth, &requested, &client, KIND)?;
            audit.resolved(&target);
            if let Some((_, first)) = &primary {
                let first: &aisix_core::native_objects::OriginBinding = first;
                if first.provider_key_id != origin.provider_key_id
                    || first.boundary != origin.boundary
                {
                    return Err(invalid(
                        "all batch entries must use the same upstream account",
                    ));
                }
            }
            let reservation = if reservations.is_empty() {
                crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?
            } else {
                // 请求层只预留一次；每项仍检查自己模型的限制，不能借第一项绕过。
                crate::quota::reserve_model_only(
                    &state,
                    &snapshot,
                    &auth,
                    &requested,
                    &target.model_entry.id,
                    &target.model_entry.value,
                    None,
                )
                .await?
            };
            reservations.push(reservation);
            audit
                .input(
                    &state,
                    &snapshot,
                    &auth,
                    &target,
                    params.to_string().as_bytes(),
                    false,
                )
                .await?;
            anthropic::refuse_messages_references(
                &state,
                &snapshot,
                &auth,
                &client,
                params,
                aisix_core::models::EndpointFamily::Batches,
            )
            .await?;
            let upstream = target
                .model_entry
                .value
                .upstream_model()
                .ok_or(ProxyError::AccountUnavailable)?
                .to_owned();
            let requested_tier = aisix_gateway::chat::anthropic_service_tier(
                params.get("speed").and_then(Value::as_str),
                params.get("service_tier").and_then(Value::as_str),
            );
            params["model"] = Value::String(upstream.clone());
            let io = aisix_gateway::upstream_io::UpstreamIoObserver::new(
                aisix_gateway::upstream_io::Protocol::Messages,
            );
            io.observe_request_json(params);
            entries.push(NativeEntry {
                upstream_input_text: io.snapshot().input_text,
                endpoint: String::new(),
                entry_id: id,
                requested_model: requested.clone(),
                requested_service_tier: requested_tier,
                requested_model_id: super::openai::resource_id(
                    &state,
                    "models",
                    &target.model_entry.id,
                )?,
                model_id: origin.model_id,
                upstream_model: upstream.clone(),
                upstream_provider: target.pk_entry.value.provider.clone(),
            });
            if primary.is_none() {
                primary = Some((target, origin));
            }
        }
        let (target, origin) = primary.ok_or_else(|| invalid("requests is required"))?;
        let registration =
            anthropic::begin(&state, &auth, &target, origin, KIND, entries, &client).await?;
        operation_id = Some(registration.operation_id);
        let bytes =
            Bytes::from(serde_json::to_vec(&body).map_err(|_| invalid("invalid batch request"))?);
        let (status, headers, bytes) = anthropic::send(
            &state,
            &target,
            &client,
            Method::POST,
            "/messages/batches",
            &anthropic::strip_model_query(query.as_deref()),
            Body::Json(bytes),
        )
        .await?;
        if !status.is_success() {
            return Ok((anthropic::json_body(status, &headers, bytes), target));
        }
        stage = InferenceStage::Accepted;
        let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| decode())?;
        registration.bind(&value).await?;
        local_results_url(&mut value);
        Ok((
            anthropic::json_body(
                status,
                &headers,
                Bytes::from(serde_json::to_vec(&value).map_err(|_| decode())?),
            ),
            target,
        ))
    }
    .await;
    anthropic::finish(
        &state,
        &snapshot,
        crate::operation::BATCHES,
        Method::POST,
        "/v1/messages/batches",
        &auth,
        &client,
        started,
        result,
        audit,
        stage,
        operation_id,
    )
    .await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Retrieve,
    Delete,
    Cancel,
    Results,
}
impl Operation {
    fn method(self) -> Method {
        match self {
            Self::Delete => Method::DELETE,
            Self::Cancel => Method::POST,
            _ => Method::GET,
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            Self::Cancel => "/cancel",
            Self::Results => "/results",
            _ => "",
        }
    }
}

pub(crate) async fn retrieve(
    State(state): State<ProxyState>,
    AnthropicAuthenticatedKey(auth): AnthropicAuthenticatedKey,
    client: ClientContext,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(state, auth, client, id, query, Operation::Retrieve).await
}
pub(crate) async fn delete(
    State(state): State<ProxyState>,
    AnthropicAuthenticatedKey(auth): AnthropicAuthenticatedKey,
    client: ClientContext,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(state, auth, client, id, query, Operation::Delete).await
}
pub(crate) async fn cancel(
    State(state): State<ProxyState>,
    AnthropicAuthenticatedKey(auth): AnthropicAuthenticatedKey,
    client: ClientContext,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(state, auth, client, id, query, Operation::Cancel).await
}
pub(crate) async fn results(
    State(state): State<ProxyState>,
    AnthropicAuthenticatedKey(auth): AnthropicAuthenticatedKey,
    client: ClientContext,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(state, auth, client, id, query, Operation::Results).await
}

async fn addressed(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    id: String,
    query: Option<String>,
    operation: Operation,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let path = format!("/v1/messages/batches/{id}{}", operation.suffix());
    let result = async {
        let (target, _, mut record) = anthropic::lookup(&state, &auth, &client, &id, KIND).await?;
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        audit
            .input(&state, &snapshot, &auth, &target, &[], false)
            .await?;
        if operation == Operation::Delete {
            super::anthropic_batch_worker::settle_before_delete(&state, &record)
                .await
                .map_err(|_| dependency())?;
            // 完成记账会推进对象版本；墓碑不能继续使用结算前的乐观锁版本。
            record = anthropic::lookup(&state, &auth, &client, &id, KIND)
                .await?
                .2;
        }
        let (status, headers, bytes) = anthropic::send(
            &state,
            &target,
            &client,
            operation.method(),
            &format!("/messages/batches/{id}{}", operation.suffix()),
            &anthropic::strip_model_query(query.as_deref()),
            Body::Empty,
        )
        .await?;
        let response = if status.is_success() && operation == Operation::Results {
            let bytes =
                rewrite_results(&state, &auth, &client, &record, &bytes, &mut audit).await?;
            anthropic::raw_body(status, &headers, bytes)
        } else if status.is_success() {
            let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| decode())?;
            if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(decode());
            }
            local_results_url(&mut value);
            if operation == Operation::Delete {
                let reply = anthropic::registry(&state)?
                    .tombstone(TombstoneRequest {
                        api_key_id: anthropic::api_key_id(&auth)?,
                        record_id: record.record_id,
                        version: record.version,
                    })
                    .await
                    .map_err(|_| dependency())?;
                if reply.state != "deleted" {
                    return Err(dependency());
                }
            }
            anthropic::json_body(
                status,
                &headers,
                Bytes::from(serde_json::to_vec(&value).map_err(|_| decode())?),
            )
        } else {
            anthropic::json_body(status, &headers, bytes)
        };
        Ok((response, target))
    }
    .await;
    anthropic::finish(
        &state,
        &snapshot,
        crate::operation::BATCHES,
        operation.method(),
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

async fn rewrite_results(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    record: &aisix_core::native_objects::NativeRecord,
    bytes: &[u8],
    audit: &mut Audit,
) -> Result<Bytes, ProxyError> {
    let entries: std::collections::BTreeMap<_, _> = record
        .entries
        .iter()
        .map(|entry| (entry.entry_id.as_str(), entry))
        .collect();
    if entries.is_empty() {
        return Err(dependency());
    }
    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(bytes.len());
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        if line.len() > aisix_gateway::sse::MAX_SSE_FRAME_BYTES {
            return Err(decode());
        }
        let mut value: Value = serde_json::from_slice(line).map_err(|_| decode())?;
        let id = value
            .get("custom_id")
            .and_then(Value::as_str)
            .ok_or_else(decode)?
            .to_owned();
        let entry = entries.get(id.as_str()).ok_or_else(decode)?;
        if !seen.insert(id) {
            return Err(decode());
        }
        match value.pointer("/result/type").and_then(Value::as_str) {
            Some("succeeded") => {
                let message = value.pointer_mut("/result/message").ok_or_else(decode)?;
                let model = message.get_mut("model").ok_or_else(decode)?;
                *model = entry.requested_model.clone().into();
                let snapshot = state.snapshot.load();
                let direct = super::openai::model_entry_by_id(state, &snapshot, entry.model_id)
                    .map_err(|_| dependency())?;
                let requested =
                    super::openai::model_entry_by_id(state, &snapshot, entry.requested_model_id)
                        .map_err(|_| dependency())?;
                let (entry_target, entry_origin) = anthropic::resolve_target(
                    state,
                    &snapshot,
                    auth,
                    &requested.value.display_name,
                    client,
                    KIND,
                )?;
                let persisted = record.origin.as_ref().ok_or_else(dependency)?;
                if super::openai::resource_id(state, "models", &entry_target.model_entry.id)?
                    != entry.model_id
                    || entry_origin.provider_key_id != persisted.provider_key_id
                    || entry_origin.boundary != persisted.boundary
                {
                    return Err(dependency());
                }
                // 每项输出按自己的模型链审查；空内容也必须执行无条件裁决。
                audit
                    .output(
                        state,
                        auth,
                        &entry_target,
                        &serde_json::to_vec(message).map_err(|_| decode())?,
                    )
                    .await?;
                anthropic::register_output_files_at_origin(
                    state,
                    auth,
                    client,
                    &entry.requested_model,
                    &direct.value.display_name,
                    Some(&entry_origin),
                    message,
                )
                .await?;
            }
            Some("errored") => {
                let error = value.pointer_mut("/result/error").ok_or_else(decode)?;
                let raw = Bytes::from(serde_json::to_vec(error).map_err(|_| decode())?);
                let kind = error
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .ok_or_else(decode)?;
                let message = error
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .ok_or_else(decode)?;
                let parsed = aisix_gateway::UpstreamErrorView {
                    kind: Some(kind.to_owned()),
                    message: Some(message.to_owned()),
                    code: None,
                    param: None,
                };
                let upstream = aisix_gateway::upstream_status_of_body(
                    super::anthropic_batch_worker::error_status(kind),
                    &raw,
                    None,
                    aisix_gateway::UpstreamWire::Anthropic,
                    |_| Some(parsed),
                );
                // 错误项和直接 HTTP 错误共用平台隔离规则；原协议请求错误保留可修复文本。
                *error = if crate::platform_errors::active()
                    && crate::platform_errors::relayed_status(&upstream).is_none()
                {
                    serde_json::from_str(&crate::error::anthropic_platform_body(
                        &crate::platform_errors::classify_bridge(&upstream),
                    ))
                    .map_err(|_| decode())?
                } else {
                    serde_json::from_slice(&aisix_gateway::scrub_upstream_error_body(&raw))
                        .map_err(|_| decode())?
                };
            }
            Some("canceled" | "expired") => {}
            _ => return Err(decode()),
        }
        serde_json::to_writer(&mut output, &value).map_err(|_| decode())?;
        output.push(b'\n');
    }
    if seen.len() != entries.len() {
        return Err(decode());
    }
    Ok(Bytes::from(output))
}

pub(crate) async fn list(
    State(state): State<ProxyState>,
    AnthropicAuthenticatedKey(auth): AnthropicAuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let model = model_hint(query.as_deref(), &client)
            .ok_or_else(|| invalid("model is required to select a batch account"))?;
        let (target, origin) =
            anthropic::resolve_target(&state, &snapshot, &auth, &model, &client, KIND)?;
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        audit
            .input(&state, &snapshot, &auth, &target, &[], false)
            .await?;
        let mut query = anthropic::strip_model_query(query.as_deref());
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if matches!(name.as_ref(), "after_id" | "before_id") {
                let (_, actual, _) =
                    anthropic::lookup(&state, &auth, &client, &value, KIND).await?;
                if actual != origin {
                    return Err(invalid("batch cursor belongs to another account"));
                }
            }
        }
        let backwards =
            url::form_urlencoded::parse(query.as_bytes()).any(|(name, _)| name == "before_id");
        let mut scanned = HashSet::new();
        loop {
            if started.elapsed() > MAX_LIST_SCAN {
                return Err(dependency());
            }
            let (status, headers, bytes) = tokio::time::timeout(
                MAX_LIST_SCAN.saturating_sub(started.elapsed()),
                anthropic::send(
                    &state,
                    &target,
                    &client,
                    Method::GET,
                    "/messages/batches",
                    &query,
                    Body::Empty,
                ),
            )
            .await
            .map_err(|_| dependency())??;
            if !status.is_success() {
                return Ok((anthropic::json_body(status, &headers, bytes), target));
            }
            let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| decode())?;
            let data = value
                .get_mut("data")
                .and_then(Value::as_array_mut)
                .ok_or_else(decode)?;
            let cursor = if backwards { data.first() } else { data.last() }
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let ids = data
                .iter()
                .map(|item| {
                    item.get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(decode)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut owned = HashSet::new();
            for ids in ids.chunks(100) {
                let reply = anthropic::registry(&state)?
                    .owned(OwnedRequest {
                        api_key_id: anthropic::api_key_id(&auth)?,
                        protocol: NativeProtocol::Anthropic,
                        kind: KIND,
                        origin: origin.clone(),
                        parent_id: None,
                        raw_ids: ids.to_vec(),
                    })
                    .await
                    .map_err(|_| dependency())?;
                if reply.state != "owned" {
                    return Err(dependency());
                }
                for record in reply.records {
                    if record.origin.as_ref() != Some(&origin)
                        || record.kind != Some(KIND)
                        || record.protocol != Some(NativeProtocol::Anthropic)
                        || record.deleted
                        || !ids.contains(&record.raw_id)
                    {
                        return Err(dependency());
                    }
                    owned.insert(record.raw_id);
                }
            }
            data.retain(|item| {
                item.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| owned.contains(id))
            });
            for item in data.iter_mut() {
                local_results_url(item);
            }
            let first = data
                .first()
                .and_then(|item| item.get("id"))
                .cloned()
                .unwrap_or(Value::Null);
            let last_owned = data
                .last()
                .and_then(|item| item.get("id"))
                .cloned()
                .unwrap_or(Value::Null);
            let empty = data.is_empty();
            if empty && value.get("has_more").and_then(Value::as_bool) == Some(true) {
                let cursor = cursor
                    .filter(|id| scanned.insert(id.clone()))
                    .ok_or_else(decode)?;
                query = query
                    .split('&')
                    .filter(|pair| {
                        !url::form_urlencoded::parse(pair.as_bytes())
                            .next()
                            .is_some_and(|(name, _)| {
                                matches!(name.as_ref(), "after_id" | "before_id")
                            })
                    })
                    .collect::<Vec<_>>()
                    .join("&");
                let next = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair(if backwards { "before_id" } else { "after_id" }, &cursor)
                    .finish();
                if !query.is_empty() {
                    query.push('&');
                }
                query.push_str(&next);
                continue;
            }
            if value.get("first_id").is_some() {
                value["first_id"] = first;
            }
            if value.get("last_id").is_some() {
                value["last_id"] = last_owned;
            }
            return Ok((
                anthropic::json_body(
                    status,
                    &headers,
                    Bytes::from(serde_json::to_vec(&value).map_err(|_| decode())?),
                ),
                target,
            ));
        }
    }
    .await;
    anthropic::finish(
        &state,
        &snapshot,
        crate::operation::BATCHES,
        Method::GET,
        "/v1/messages/batches",
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
