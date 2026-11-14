//! 原生 Responses 子操作复用推理路径的准入、审查和流生命周期。
use crate::request_json::Json;
use aisix_core::native_objects::{NativeRecord, OriginBinding, TombstoneRequest};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::time::Instant;

use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::reject::AisixPath;
use crate::state::ProxyState;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseOperationKind {
    Create,
    Retrieve,
    Delete,
    Cancel,
    InputItems,
    Compact,
    InputTokens,
}

#[derive(Clone)]
pub(crate) struct NativeResponseOperation {
    pub kind: ResponseOperationKind,
    pub raw_id: String,
    pub query: String,
    pub origin: Option<OriginBinding>,
    pub record: Option<NativeRecord>,
    pub item_owner: Option<String>,
}

impl Default for NativeResponseOperation {
    fn default() -> Self {
        Self::new(ResponseOperationKind::Create)
    }
}

impl NativeResponseOperation {
    pub fn new(kind: ResponseOperationKind) -> Self {
        Self {
            kind,
            raw_id: String::new(),
            query: String::new(),
            origin: None,
            record: None,
            item_owner: None,
        }
    }

    pub fn method(&self) -> Method {
        match self.kind {
            ResponseOperationKind::Retrieve | ResponseOperationKind::InputItems => Method::GET,
            ResponseOperationKind::Delete => Method::DELETE,
            _ => Method::POST,
        }
    }

    pub fn path(&self) -> &'static str {
        match self.kind {
            ResponseOperationKind::Create => "/v1/responses",
            ResponseOperationKind::Retrieve | ResponseOperationKind::Delete => "/v1/responses/:id",
            ResponseOperationKind::Cancel => "/v1/responses/:id/cancel",
            ResponseOperationKind::InputItems => "/v1/responses/:id/input_items",
            ResponseOperationKind::Compact => "/v1/responses/compact",
            ResponseOperationKind::InputTokens => "/v1/responses/input_tokens",
        }
    }

    pub fn upstream_path(&self) -> String {
        match self.kind {
            ResponseOperationKind::Create => "/responses".into(),
            ResponseOperationKind::Compact => "/responses/compact".into(),
            ResponseOperationKind::InputTokens => "/responses/input_tokens".into(),
            ResponseOperationKind::Retrieve | ResponseOperationKind::Delete => {
                format!("/responses/{}", self.raw_id)
            }
            ResponseOperationKind::Cancel => format!("/responses/{}/cancel", self.raw_id),
            ResponseOperationKind::InputItems => format!("/responses/{}/input_items", self.raw_id),
        }
    }

    pub fn is_create(&self) -> bool {
        self.kind == ResponseOperationKind::Create
    }
    pub fn sends_body(&self) -> bool {
        matches!(
            self.kind,
            ResponseOperationKind::Create
                | ResponseOperationKind::Compact
                | ResponseOperationKind::InputTokens
        )
    }
    pub fn bills_tokens(&self) -> bool {
        matches!(
            self.kind,
            ResponseOperationKind::Create | ResponseOperationKind::Compact
        )
    }

    pub fn validate_target(
        &self,
        state: &ProxyState,
        snapshot: &aisix_core::AisixSnapshot,
        model_id: &str,
        model: &aisix_core::Model,
    ) -> Result<(), ProxyError> {
        let Some(origin) = &self.origin else {
            return Ok(());
        };
        if origin.model_id.to_string() != model_id
            || model.provider_key_id.as_deref() != Some(origin.provider_key_id.to_string().as_str())
        {
            return Err(ProxyError::ProviderUnavailable);
        }
        let platform = state.platform().ok_or(ProxyError::ProviderUnavailable)?;
        let pk = snapshot
            .provider_keys
            .get_by_id(&origin.provider_key_id.to_string())
            .ok_or(ProxyError::ProviderUnavailable)?;
        let boundary = aisix_core::native_objects::configured_native_origin(
            &platform.id_key,
            origin.provider_key_id,
            &pk.value,
            aisix_core::native_objects::NativeProtocol::Openai,
            aisix_core::native_objects::NativeObjectKind::Response,
        )
        .map_err(|_| ProxyError::ProviderUnavailable)?;
        if boundary != origin.boundary {
            return Err(ProxyError::ProviderUnavailable);
        }
        Ok(())
    }
}

pub(crate) async fn compact(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, crate::request_json::JsonRejection>,
) -> Response {
    stateless(
        state,
        auth,
        client,
        query,
        body,
        ResponseOperationKind::Compact,
    )
    .await
}
pub(crate) async fn input_tokens(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    body: Result<Json<Value>, crate::request_json::JsonRejection>,
) -> Response {
    stateless(
        state,
        auth,
        client,
        query,
        body,
        ResponseOperationKind::InputTokens,
    )
    .await
}

async fn stateless(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    query: Option<String>,
    body: Result<Json<Value>, crate::request_json::JsonRejection>,
    kind: ResponseOperationKind,
) -> Response {
    let started = Instant::now();
    let mut operation = NativeResponseOperation::new(kind);
    let mut body = match body {
        Ok(Json(body)) => body,
        Err(error) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                operation.path(),
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_json_rejection(
                    error,
                    state.request_body_limit_for(operation.path()),
                ),
            )
        }
    };
    if body.get("model").is_none() {
        if let Some(model) = model_hint(query.as_deref(), &client.headers) {
            if let Some(object) = body.as_object_mut() {
                object.insert("model".into(), Value::String(model));
            }
        }
    }
    operation.query = strip_model_query(query.as_deref());
    crate::responses::responses_operation(state, auth, client, body, operation).await
}

pub(crate) async fn retrieve(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(
        state,
        auth,
        client,
        id,
        query,
        ResponseOperationKind::Retrieve,
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
    addressed(
        state,
        auth,
        client,
        id,
        query,
        ResponseOperationKind::Delete,
    )
    .await
}
pub(crate) async fn cancel(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(
        state,
        auth,
        client,
        id,
        query,
        ResponseOperationKind::Cancel,
    )
    .await
}
pub(crate) async fn input_items(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    addressed(
        state,
        auth,
        client,
        id,
        query,
        ResponseOperationKind::InputItems,
    )
    .await
}

async fn addressed(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    id: String,
    query: Option<String>,
    kind: ResponseOperationKind,
) -> Response {
    let started = Instant::now();
    let mut operation = NativeResponseOperation::new(kind);
    let resolved = match super::origin::resolve_reference(&state, &auth, &id).await {
        Ok(resolved) => resolved,
        Err(error) => {
            // 路径命名的资源缺失使用404；previous_response_id 的400恢复码只用于创建续接。
            let error = match error {
                ProxyError::ResponseNotFound => ProxyError::JobResourceNotFound("response".into()),
                other => other,
            };
            return crate::reject::reject_before_dispatch(
                &state,
                operation.method().as_str(),
                operation.path(),
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                error,
            );
        }
    };
    if let Err(error) = crate::jobs::require_safe_upstream_id(&resolved.raw_id) {
        return error.into_response();
    }
    let stream = query.as_deref().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes()).any(|(k, v)| k == "stream" && v == "true")
    });
    operation.item_owner = state
        .platform()
        .map(|_| crate::platform::tenant_owner(&auth));
    operation.raw_id = resolved.raw_id;
    operation.origin = resolved.origin;
    operation.record = resolved.record;
    operation.query = match item_query(&state, &auth, query.as_deref(), kind) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let body = serde_json::json!({"model":resolved.requested_model,"stream":stream});
    let record = operation.record.clone();
    let response =
        crate::responses::responses_operation(state.clone(), auth.clone(), client, body, operation)
            .await;
    if kind == ResponseOperationKind::Delete && response.status().is_success() {
        if let (Some(registry), Some(record)) = (&state.native_objects, record) {
            let Ok(api_key_id) = uuid::Uuid::parse_str(&auth.entry.id) else {
                return ProxyError::ProviderUnavailable.into_response();
            };
            let result = registry
                .tombstone(TombstoneRequest {
                    api_key_id,
                    record_id: record.record_id,
                    version: record.version,
                })
                .await;
            if !matches!(result, Ok(reply) if reply.state == "deleted") {
                return ProxyError::ProviderUnavailable.into_response();
            }
        }
    }
    response
}

fn model_hint(query: Option<&str>, headers: &HeaderMap) -> Option<String> {
    query
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, v)| k == "model" && !v.trim().is_empty())
                .map(|(_, v)| v.into_owned())
        })
        .or_else(|| {
            headers
                .get("x-aisix-model")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        })
}
fn strip_model_query(query: Option<&str>) -> String {
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
fn item_query(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    query: Option<&str>,
    kind: ResponseOperationKind,
) -> Result<String, ProxyError> {
    let query = strip_model_query(query);
    let Some(platform) = state
        .platform()
        .filter(|_| kind == ResponseOperationKind::InputItems)
    else {
        return Ok(query);
    };
    let sealer = crate::opaque_id::Sealer::of(platform, &crate::platform::tenant_owner(auth));
    query
        .split('&')
        .map(|pair| {
            let Some((key, value)) = url::form_urlencoded::parse(pair.as_bytes()).next() else {
                return Ok(pair.to_string());
            };
            if key != "after" && key != "before" {
                return Ok(pair.to_string());
            }
            // 父 Response 已逐次授权；游标只在其 input_items 集合内选择位置。
            let raw = sealer
                .unseal(crate::opaque_id::IdKind::Item, &value)
                .unwrap_or_else(|| value.into_owned());
            Ok(url::form_urlencoded::Serializer::new(String::new())
                .append_pair(&key, &raw)
                .finish())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|pairs| pairs.join("&"))
}

pub(crate) fn relay_headers(response: &mut Response, headers: &HeaderMap) {
    for name in [
        "content-type",
        "x-request-id",
        "retry-after",
        "openai-version",
        "openai-processing-ms",
    ] {
        for value in headers.get_all(name) {
            response
                .headers_mut()
                .append(axum::http::HeaderName::from_static(name), value.clone());
        }
    }
    for (name, value) in headers {
        if name.as_str().starts_with("x-ratelimit-") {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    response
        .headers_mut()
        .remove(axum::http::header::CONTENT_LENGTH);
}

pub(crate) async fn read_body(
    response: reqwest::Response,
    deadline: Option<std::time::Duration>,
    allow_empty: bool,
) -> Result<(Value, bytes::Bytes), ProxyError> {
    let read = response.bytes();
    let bytes = match deadline {
        Some(deadline) => tokio::time::timeout(deadline, read).await.map_err(|_| {
            ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamDecode(
                "accepted native response body timed out".into(),
            ))
        })?,
        None => read.await,
    }
    .map_err(|error| ProxyError::Bridge(aisix_gateway::upstream_body_read_error(&error)))?;
    let body = if bytes.is_empty() && allow_empty {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).map_err(|_| {
            ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamDecode(
                "native response body was not JSON".into(),
            ))
        })?
    };
    Ok((body, bytes))
}
