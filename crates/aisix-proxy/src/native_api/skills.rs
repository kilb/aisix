//! Skills 保留提供商原始对象和版本；私有对象的每次操作都重新证明归属与账号。
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Instant;

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{
    configured_native_origin_at, AcceptedReference, InferenceStage, NativeObjectKind,
    NativeOriginSurface, NativeProtocol, NativeRecord, ObjectDeletionRequest, OriginBinding,
    OwnedRequest, ResolveReservationRequest,
};
use aisix_core::AisixSnapshot;
use axum::extract::{FromRequest, Multipart, RawQuery, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::registry::{NativeObjectRegistry, NativeRegistryError};
use super::{anthropic, openai};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::jobs::JobTarget;
use crate::reject::AisixPath;
use crate::state::ProxyState;

pub(crate) const SURFACE: crate::operation::Surface = crate::operation::Surface {
    handler: "skills",
    operation: "skills",
};

pub(crate) fn not_found() -> ProxyError {
    ProxyError::JobResourceNotFound("native_object".into())
}

pub(crate) fn same_account(left: &OriginBinding, right: &OriginBinding) -> bool {
    left.provider_key_id == right.provider_key_id && left.boundary == right.boundary
}

fn protocol(headers: &HeaderMap) -> NativeProtocol {
    if anthropic::is_anthropic(headers) {
        NativeProtocol::Anthropic
    } else {
        NativeProtocol::Openai
    }
}

pub(crate) enum Audit {
    Openai(openai::Audit),
    Anthropic(anthropic::Audit),
}

impl Audit {
    pub(crate) fn new(protocol: NativeProtocol) -> Self {
        match protocol {
            NativeProtocol::Openai => Self::Openai(openai::Audit::default()),
            NativeProtocol::Anthropic => Self::Anthropic(anthropic::Audit::default()),
        }
    }

    pub(crate) fn resolved(&mut self, target: &JobTarget) {
        match self {
            Self::Openai(audit) => audit.resolved(target),
            Self::Anthropic(audit) => audit.resolved(target),
        }
    }

    pub(crate) fn registered(&mut self, registration: &Registration) {
        match (self, registration) {
            (Self::Openai(audit), Registration::Openai(registration)) => {
                audit.registered(registration)
            }
            (Self::Anthropic(audit), Registration::Anthropic(registration)) => {
                audit.registered(registration)
            }
            _ => {}
        }
    }

    pub(crate) fn deleting(&mut self, operation_id: Uuid) {
        match self {
            Self::Openai(audit) => {
                audit.registered_operation(operation_id, InferenceStage::Management)
            }
            Self::Anthropic(audit) => {
                audit.registered_operation(operation_id, InferenceStage::Management)
            }
        }
    }

    async fn input(
        &mut self,
        state: &ProxyState,
        snapshot: &AisixSnapshot,
        auth: &AuthenticatedKey,
        target: &JobTarget,
        bytes: &[u8],
        mask: bool,
    ) -> Result<Option<Vec<u8>>, ProxyError> {
        match self {
            Self::Openai(audit) => {
                audit
                    .input(state, snapshot, auth, target, bytes, mask)
                    .await
            }
            Self::Anthropic(audit) => {
                audit
                    .input(state, snapshot, auth, target, bytes, mask)
                    .await
            }
        }
    }

    pub(crate) async fn output(
        &mut self,
        state: &ProxyState,
        auth: &AuthenticatedKey,
        target: &JobTarget,
        bytes: &[u8],
    ) -> Result<(), ProxyError> {
        match self {
            Self::Openai(audit) => audit.output(state, auth, target, bytes).await,
            Self::Anthropic(audit) => audit.output(state, auth, target, bytes).await,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn finish(
        self,
        state: &ProxyState,
        snapshot: &AisixSnapshot,
        surface: crate::operation::Surface,
        method: Method,
        path: &str,
        auth: &AuthenticatedKey,
        client: &ClientContext,
        started: Instant,
        result: Result<(Response, JobTarget), ProxyError>,
        operation_id: Option<Uuid>,
    ) -> Response {
        match self {
            Self::Openai(audit) => {
                openai::finish(
                    state,
                    snapshot,
                    surface,
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
            Self::Anthropic(audit) => {
                anthropic::finish(
                    state,
                    snapshot,
                    surface,
                    method,
                    path,
                    auth,
                    client,
                    started,
                    result,
                    audit,
                    InferenceStage::Management,
                    operation_id,
                )
                .await
            }
        }
    }
}

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    headers: HeaderMap,
    bytes: Bytes,
}

pub(crate) struct Payload {
    pub(crate) value: Value,
    json: Option<Bytes>,
    parts: Option<Vec<Part>>,
}

pub(crate) enum Body {
    Empty,
    Json(Bytes),
    Multipart(reqwest::multipart::Form),
}

impl Payload {
    pub(crate) async fn parse(
        state: &ProxyState,
        request: Request,
        path: &str,
    ) -> Result<Self, ProxyError> {
        // 无体方法不解析也不转发调用方正文；后续 screen 仍以空输入咨询 guardrail 链。
        if matches!(
            request.method(),
            &Method::GET | &Method::HEAD | &Method::DELETE
        ) {
            return Ok(Self {
                value: Value::Null,
                json: None,
                parts: None,
            });
        }
        let multipart = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("multipart/form-data"))
            });
        if !multipart {
            let bytes = Bytes::from_request(request, state).await.map_err(|error| {
                crate::error::proxy_error_from_bytes_rejection(
                    error,
                    state.request_body_limit_for(path),
                )
            })?;
            let value = if bytes.is_empty() {
                Value::Null
            } else {
                crate::attribution::measure_json_decode(false, || serde_json::from_slice(&bytes))
                    .map_err(|_| ProxyError::InvalidRequest("invalid JSON body".into()))?
            };
            return Ok(Self {
                value,
                json: (!bytes.is_empty()).then_some(bytes),
                parts: None,
            });
        }
        let mut multipart = Multipart::from_request(request, state)
            .await
            .map_err(|_| ProxyError::InvalidRequest("invalid multipart form data".into()))?;
        let mut parts = Vec::new();
        let mut values = Map::new();
        while let Some(field) = multipart.next_field().await.map_err(|error| {
            crate::error::proxy_error_from_multipart(
                error,
                state.request_body_limit_for(path),
                "native multipart",
            )
        })? {
            let name = field.name().unwrap_or_default().to_owned();
            let filename = field.file_name().map(str::to_owned);
            let content_type = field.content_type().map(str::to_owned);
            let headers = field.headers().clone();
            let bytes = field.bytes().await.map_err(|error| {
                crate::error::proxy_error_from_multipart(
                    error,
                    state.request_body_limit_for(path),
                    "native multipart",
                )
            })?;
            if filename.is_none() {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    let text = Value::String(text.to_owned());
                    match values.get_mut(&name) {
                        Some(Value::Array(previous)) => previous.push(text),
                        Some(previous) => *previous = Value::Array(vec![previous.clone(), text]),
                        None => {
                            values.insert(name.clone(), text);
                        }
                    }
                }
            }
            parts.push(Part {
                name,
                filename,
                content_type,
                headers,
                bytes,
            });
        }
        Ok(Self {
            value: Value::Object(values),
            json: None,
            parts: Some(parts),
        })
    }

    pub(crate) async fn screen(
        &mut self,
        state: &ProxyState,
        snapshot: &AisixSnapshot,
        auth: &AuthenticatedKey,
        client: &ClientContext,
        target: &JobTarget,
        audit: &mut Audit,
    ) -> Result<(), ProxyError> {
        crate::usage_attr::hold_ahead_of_chain(state, snapshot, auth, Some(target.display_name()));
        if let Some(parts) = &self.parts {
            // 没有文本也咨询链；不能借二进制上传跳过无条件 custom verdict。
            if parts.is_empty() {
                audit
                    .input(state, snapshot, auth, target, b"", false)
                    .await?;
            }
            for part in parts {
                let masked = audit
                    .input(state, snapshot, auth, target, &part.bytes, true)
                    .await?;
                if masked
                    .as_ref()
                    .is_some_and(|masked| masked.as_slice() != part.bytes.as_ref())
                {
                    return Err(crate::error::guardrail_block_error("request", None, None));
                }
            }
        } else {
            let bytes = self.json.as_deref().unwrap_or_default();
            if bytes.is_empty() {
                audit
                    .input(state, snapshot, auth, target, bytes, false)
                    .await?;
            } else {
                if let Some(masked) = audit
                    .input(state, snapshot, auth, target, bytes, true)
                    .await?
                {
                    self.value = serde_json::from_slice(&masked)
                        .map_err(|_| crate::error::guardrail_block_error("request", None, None))?;
                    self.json = Some(Bytes::from(masked));
                }
            }
        }
        crate::usage_attr::hold_ahead_of_chain(state, snapshot, auth, Some(target.display_name()));
        crate::usage_attr::note_screened_request(snapshot, || self.capture());
        let _ = client;
        Ok(())
    }

    fn capture(&self) -> String {
        match &self.parts {
            Some(parts) => openai::captured_multipart_fields(
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
                &["files", "files[]", "audio_sample", "recording"],
            ),
            None => self
                .json
                .as_ref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default(),
        }
    }

    pub(crate) fn into_body(self) -> Result<Body, ProxyError> {
        if let Some(parts) = self.parts {
            let mut form = reqwest::multipart::Form::new();
            for part in parts {
                let mut value =
                    reqwest::multipart::Part::bytes(part.bytes.to_vec()).headers(part.headers);
                if let Some(filename) = part.filename {
                    value = value.file_name(filename);
                }
                if let Some(content_type) = part.content_type {
                    value = value.mime_str(&content_type).map_err(|_| {
                        ProxyError::InvalidRequest("invalid multipart content type".into())
                    })?;
                }
                form = form.part(part.name, value);
            }
            Ok(Body::Multipart(form))
        } else {
            Ok(self.json.map_or(Body::Empty, Body::Json))
        }
    }
}

pub(crate) enum Registration {
    Openai(openai::Registration),
    Anthropic(anthropic::Registration),
}

impl Registration {
    pub(crate) fn operation_id(&self) -> Uuid {
        match self {
            Self::Openai(value) => value.operation_id,
            Self::Anthropic(value) => value.operation_id,
        }
    }
    pub(crate) async fn bind(&self, value: &Value) -> Result<NativeRecord, ProxyError> {
        match self {
            Self::Openai(registration) => registration.bind(value).await,
            Self::Anthropic(registration) => registration.bind(value).await,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn begin(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    origin: OriginBinding,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    operation: &str,
    references: Vec<AcceptedReference>,
) -> Result<Registration, ProxyError> {
    match protocol {
        NativeProtocol::Openai => openai::begin_related_operation(
            state,
            auth,
            target,
            origin,
            kind,
            None,
            client,
            operation,
            InferenceStage::Management,
            references,
            None,
        )
        .await
        .map(Registration::Openai),
        NativeProtocol::Anthropic => anthropic::begin_related_operation(
            state,
            auth,
            target,
            origin,
            kind,
            None,
            client,
            operation,
            InferenceStage::Management,
            references,
            None,
        )
        .await
        .map(Registration::Anthropic),
    }
}

pub(crate) fn resolve(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    wanted: &str,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
) -> Result<(JobTarget, OriginBinding), ProxyError> {
    match protocol {
        NativeProtocol::Openai => {
            openai::resolve_management(state, snapshot, auth, client, wanted, kind)
        }
        NativeProtocol::Anthropic => {
            anthropic::resolve_management(state, snapshot, auth, client, wanted, kind)
        }
    }
}

pub(crate) async fn lookup(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
) -> Result<(JobTarget, OriginBinding, NativeRecord), ProxyError> {
    match protocol {
        NativeProtocol::Openai => {
            openai::lookup_management(state, auth, client, raw, kind, None).await
        }
        NativeProtocol::Anthropic => {
            anthropic::lookup_management(state, auth, client, raw, kind, None).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn send(
    state: &ProxyState,
    target: &JobTarget,
    client: &ClientContext,
    origin: &OriginBinding,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    method: Method,
    path: &str,
    query: &str,
    body: Body,
) -> Result<openai::Reply, ProxyError> {
    match protocol {
        NativeProtocol::Openai => {
            let body = match body {
                Body::Empty => openai::Body::Empty,
                Body::Json(value) => openai::Body::Json(value),
                Body::Multipart(value) => openai::Body::Multipart(value),
            };
            openai::send_at_origin(
                state, target, client, origin, kind, method, path, query, body,
            )
            .await
        }
        NativeProtocol::Anthropic => {
            let body = match body {
                Body::Empty => anthropic::Body::Empty,
                Body::Json(value) => anthropic::Body::Json(value),
                Body::Multipart(value) => anthropic::Body::Multipart(value),
            };
            let (status, headers, bytes) = anthropic::send_at_origin(
                state, target, client, origin, kind, method, path, query, body,
            )
            .await?;
            Ok(openai::Reply {
                status,
                headers,
                bytes,
            })
        }
    }
}

pub(crate) fn response(protocol: NativeProtocol, reply: openai::Reply) -> Response {
    match protocol {
        NativeProtocol::Openai => openai::raw_body(reply.status, &reply.headers, reply.bytes),
        NativeProtocol::Anthropic => anthropic::raw_body(reply.status, &reply.headers, reply.bytes),
    }
}

/// 派发前持久占住对象；明确拒绝可释放，未知结果必须保留到受控对账。
pub(crate) struct Deletion {
    registry: Arc<dyn NativeObjectRegistry>,
    request: ObjectDeletionRequest,
}

fn deletion_error(error: NativeRegistryError) -> ProxyError {
    if error == NativeRegistryError::Conflict {
        ProxyError::NativeOperationConflict
    } else {
        openai::registry_error(error)
    }
}

impl Deletion {
    pub(crate) async fn reserve(
        state: &ProxyState,
        auth: &AuthenticatedKey,
        record: &NativeRecord,
    ) -> Result<Self, ProxyError> {
        let registry = openai::registry(state)?;
        let request = ObjectDeletionRequest {
            api_key_id: openai::api_key_id(auth)?,
            operation_id: Uuid::new_v4(),
            record_id: record.record_id,
            version: record.version,
        };
        let reply = registry
            .reserve_delete(request.clone())
            .await
            .map_err(deletion_error)?;
        if reply.state != "reserved"
            || reply.operation_id != Some(request.operation_id)
            || !reply.record.as_ref().is_some_and(|reserved| {
                reserved.record_id == request.record_id
                    && reserved.version == request.version
                    && !reserved.deleted
            })
        {
            return Err(openai::dependency());
        }
        Ok(Self { registry, request })
    }

    pub(crate) fn operation_id(&self) -> Uuid {
        self.request.operation_id
    }

    pub(crate) async fn commit(&self) -> Result<(), ProxyError> {
        let reply = self
            .registry
            .commit_delete(self.request.clone())
            .await
            .map_err(deletion_error)?;
        if reply.state != "deleted" {
            return Err(openai::dependency());
        }
        Ok(())
    }

    pub(crate) fn upstream_rejected(error: &ProxyError) -> bool {
        matches!(
            error,
            ProxyError::Bridge(
                aisix_gateway::BridgeError::UpstreamStatus { status, .. }
                | aisix_gateway::BridgeError::UpstreamBodyUnread { status, .. }
            ) if (400..500).contains(status) && *status != 408
        )
    }

    pub(crate) async fn resolve(&self, upstream_rejected: bool) -> Result<(), ProxyError> {
        let reply = self
            .registry
            .resolve_reservation(ResolveReservationRequest {
                api_key_id: self.request.api_key_id,
                operation_id: self.request.operation_id,
                upstream_rejected,
            })
            .await
            .map_err(deletion_error)?;
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
}

pub(crate) async fn filter_owned(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    origin: &OriginBinding,
    value: &mut Value,
    shared_skills: bool,
) -> Result<(), ProxyError> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(openai::invalid_identity)?;
    let ids = data
        .iter()
        .map(|value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(openai::invalid_identity)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let registry = openai::registry(state)?;
    let current = openai::current_auth(&state.snapshot.load(), auth)?;
    let mut owned = HashSet::new();
    // 空页也必须在注册表重验当前调用人，不能把“没有对象”当授权成功。
    let chunks = if ids.is_empty() {
        vec![&[][..]]
    } else {
        ids.chunks(100).collect::<Vec<_>>()
    };
    for chunk in chunks {
        let reply = registry
            .owned(OwnedRequest {
                api_key_id: openai::api_key_id(&current)?,
                protocol,
                kind,
                origin: origin.clone(),
                parent_id: None,
                raw_ids: chunk.to_vec(),
            })
            .await
            .map_err(openai::registry_error)?;
        if reply.state != "owned" {
            return Err(openai::dependency());
        }
        for record in reply.records {
            if record.deleted
                || record.protocol != Some(protocol)
                || record.kind != Some(kind)
                || record.origin.as_ref() != Some(origin)
                || !chunk.contains(&record.raw_id)
            {
                return Err(openai::dependency());
            }
            owned.insert(record.raw_id);
        }
    }
    let data = value
        .get_mut("data")
        .and_then(Value::as_array_mut)
        .ok_or_else(openai::invalid_identity)?;
    data.retain(|value| {
        value
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| owned.contains(id))
            || (shared_skills && is_official_anthropic_skill(value))
    });
    for (field, first) in [("first_id", true), ("last_id", false)] {
        if value.get(field).is_some() {
            value[field] = value
                .get("data")
                .and_then(Value::as_array)
                .and_then(|data| if first { data.first() } else { data.last() })
                .and_then(|value| value.get("id"))
                .cloned()
                .unwrap_or(Value::Null);
        }
    }
    Ok(())
}

fn is_official_anthropic_skill(value: &Value) -> bool {
    value.get("source").and_then(|source| {
        source
            .as_str()
            .or_else(|| source.get("type").and_then(Value::as_str))
    }) == Some("anthropic")
}

fn page_sealer(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    origin: &OriginBinding,
    parent: Option<&str>,
    query: &str,
) -> Result<crate::opaque_id::Sealer, ProxyError> {
    let filter = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key != "page")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let owner = serde_json::to_string(&(
        "native_page_v12",
        &auth.entry.id,
        protocol,
        kind,
        origin,
        parent,
        filter,
    ))
    .map_err(|_| openai::dependency())?;
    let key = state.native_origin_key().ok_or_else(openai::dependency)?;
    Ok(crate::opaque_id::Sealer::from_native_key(&key, &owner))
}

fn replace_query(query: &str, key: &str, value: &str) -> String {
    let mut result = url::form_urlencoded::Serializer::new(String::new());
    for (name, item) in
        url::form_urlencoded::parse(query.as_bytes()).filter(|(name, _)| name != key)
    {
        result.append_pair(&name, &item);
    }
    result.append_pair(key, value).finish()
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn list_owned(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    target: &JobTarget,
    origin: &OriginBinding,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    path: &str,
    query: &str,
    parent: Option<&str>,
) -> Result<openai::Reply, ProxyError> {
    let sealer = page_sealer(state, auth, protocol, kind, origin, parent, query)?;
    let mut query = query.to_owned();
    if protocol == NativeProtocol::Anthropic {
        if let Some(page) = url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "page")
            .map(|(_, value)| value.into_owned())
        {
            let raw = sealer
                .unseal(crate::opaque_id::IdKind::Item, &page)
                .ok_or_else(not_found)?;
            query = replace_query(&query, "page", &raw);
        }
    } else if parent.is_none() {
        if let Some(after) = url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "after")
            .map(|(_, value)| value.into_owned())
        {
            let (_, actual, _) = lookup(state, auth, client, &after, protocol, kind).await?;
            if !same_account(origin, &actual) {
                return Err(not_found());
            }
        }
    }
    let mut seen = BTreeSet::new();
    loop {
        let mut reply = send(
            state,
            target,
            client,
            origin,
            protocol,
            kind,
            Method::GET,
            path,
            &query,
            Body::Empty,
        )
        .await?;
        if !reply.status.is_success() {
            return Ok(reply);
        }
        let mut value: Value =
            serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
        let cursor = value
            .get("last_id")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .get("data")
                    .and_then(Value::as_array)
                    .and_then(|data| data.last())
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);
        if parent.is_none() {
            filter_owned(
                state,
                auth,
                protocol,
                kind,
                origin,
                &mut value,
                protocol == NativeProtocol::Anthropic,
            )
            .await?;
        } else {
            validate_versions(&value, parent.ok_or_else(openai::invalid_identity)?)?;
        }
        if protocol == NativeProtocol::Anthropic {
            if let Some(cursor) = value.get("next_page").and_then(Value::as_str) {
                value["next_page"] = sealer.seal(crate::opaque_id::IdKind::Item, cursor).into();
            }
        } else if value
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
            && value.get("has_more").and_then(Value::as_bool) == Some(true)
        {
            let cursor = cursor
                .filter(|cursor| seen.insert(cursor.clone()))
                .ok_or_else(openai::invalid_identity)?;
            query = replace_query(&query, "after", &cursor);
            continue;
        }
        reply.bytes =
            Bytes::from(serde_json::to_vec(&value).map_err(|_| openai::invalid_identity())?);
        return Ok(reply);
    }
}

fn validate_version(value: &Value, skill_id: &str) -> Result<(), ProxyError> {
    if value
        .get("skill_id")
        .and_then(Value::as_str)
        .is_some_and(|id| id != skill_id)
    {
        return Err(openai::invalid_identity());
    }
    Ok(())
}

fn validate_versions(value: &Value, skill_id: &str) -> Result<(), ProxyError> {
    for version in value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(openai::invalid_identity)?
    {
        validate_version(version, skill_id)?;
    }
    Ok(())
}

enum Action {
    Collection,
    Skill(String),
    Content(String),
    Versions(String),
    Version(String, String),
    VersionContent(String, String),
}

impl Action {
    fn skill_id(&self) -> Option<&str> {
        match self {
            Self::Collection => None,
            Self::Skill(id)
            | Self::Content(id)
            | Self::Versions(id)
            | Self::Version(id, _)
            | Self::VersionContent(id, _) => Some(id),
        }
    }
    fn path(&self) -> String {
        match self {
            Self::Collection => "/skills".into(),
            Self::Skill(id) => format!("/skills/{id}"),
            Self::Content(id) => format!("/skills/{id}/content"),
            Self::Versions(id) => format!("/skills/{id}/versions"),
            Self::Version(id, version) => format!("/skills/{id}/versions/{version}"),
            Self::VersionContent(id, version) => format!("/skills/{id}/versions/{version}/content"),
        }
    }
}

pub(crate) async fn collection(
    State(state): State<ProxyState>,
    anthropic::FilesAuthenticatedKey(auth): anthropic::FilesAuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(state, auth, client, query, request, Action::Collection).await
}
pub(crate) async fn addressed(
    State(state): State<ProxyState>,
    anthropic::FilesAuthenticatedKey(auth): anthropic::FilesAuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(state, auth, client, query, request, Action::Skill(id)).await
}
pub(crate) async fn content(
    State(state): State<ProxyState>,
    anthropic::FilesAuthenticatedKey(auth): anthropic::FilesAuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(state, auth, client, query, request, Action::Content(id)).await
}
pub(crate) async fn versions(
    State(state): State<ProxyState>,
    anthropic::FilesAuthenticatedKey(auth): anthropic::FilesAuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(state, auth, client, query, request, Action::Versions(id)).await
}
pub(crate) async fn version(
    State(state): State<ProxyState>,
    anthropic::FilesAuthenticatedKey(auth): anthropic::FilesAuthenticatedKey,
    client: ClientContext,
    AisixPath((id, version)): AisixPath<(String, String)>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(
        state,
        auth,
        client,
        query,
        request,
        Action::Version(id, version),
    )
    .await
}
pub(crate) async fn version_content(
    State(state): State<ProxyState>,
    anthropic::FilesAuthenticatedKey(auth): anthropic::FilesAuthenticatedKey,
    client: ClientContext,
    AisixPath((id, version)): AisixPath<(String, String)>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(
        state,
        auth,
        client,
        query,
        request,
        Action::VersionContent(id, version),
    )
    .await
}

async fn handle(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    query: Option<String>,
    request: Request,
    action: Action,
) -> Response {
    let started = Instant::now();
    crate::attribution::note_handler_started(started);
    let protocol = protocol(request.headers());
    let method = request.method().clone();
    let path = action.path();
    let endpoint = format!("/v1{path}");
    let snapshot = state.snapshot.load();
    let mut audit = Audit::new(protocol);
    let mut operation_id = None;
    let mut deletion = None;
    let mut result = async {
        if protocol == NativeProtocol::Anthropic
            && (matches!(action, Action::Content(_))
                || matches!(action, Action::Skill(_)) && method == Method::POST)
        {
            return Err(not_found());
        }
        if let Some(id) = action.skill_id() {
            crate::jobs::require_safe_upstream_id(id).map_err(|_| not_found())?;
        }
        if let Action::Version(_, version) | Action::VersionContent(_, version) = &action {
            crate::jobs::require_safe_upstream_id(version).map_err(|_| not_found())?;
        }
        let mut payload = Payload::parse(&state, request, &endpoint).await?;
        let (target, origin, record) = if let Some(id) = action.skill_id() {
            match lookup(
                &state,
                &auth,
                &client,
                id,
                protocol,
                NativeObjectKind::Skill,
            )
            .await
            {
                Ok((target, origin, record)) => (target, origin, Some(record)),
                Err(error)
                    if protocol == NativeProtocol::Anthropic
                        && method == Method::GET
                        && error.status() == StatusCode::NOT_FOUND =>
                {
                    let wanted =
                        openai::model_hint(query.as_deref(), &client).ok_or_else(|| {
                            ProxyError::InvalidRequest(
                                "select a direct model with x-aisix-model or ?model=".into(),
                            )
                        })?;
                    let (target, origin) = resolve(
                        &state,
                        &snapshot,
                        &auth,
                        &client,
                        &wanted,
                        protocol,
                        NativeObjectKind::Skill,
                    )?;
                    audit.resolved(&target);
                    let reply = send(
                        &state,
                        &target,
                        &client,
                        &origin,
                        protocol,
                        NativeObjectKind::Skill,
                        Method::GET,
                        &format!("/skills/{id}"),
                        "",
                        Body::Empty,
                    )
                    .await?;
                    if !reply.status.is_success() {
                        return Err(not_found());
                    }
                    let value: Value = serde_json::from_slice(&reply.bytes)
                        .map_err(|_| openai::invalid_identity())?;
                    if value.get("id").and_then(Value::as_str) != Some(id)
                        || !is_official_anthropic_skill(&value)
                    {
                        return Err(not_found());
                    }
                    if matches!(action, Action::Skill(_)) {
                        audit.output(&state, &auth, &target, &reply.bytes).await?;
                        return Ok((response(protocol, reply), target));
                    }
                    (target, origin, None)
                }
                Err(error) => return Err(error),
            }
        } else {
            let wanted = openai::model_hint(query.as_deref(), &client).ok_or_else(|| {
                ProxyError::InvalidRequest(
                    "select a direct model with x-aisix-model or ?model=".into(),
                )
            })?;
            let (target, origin) = resolve(
                &state,
                &snapshot,
                &auth,
                &client,
                &wanted,
                protocol,
                NativeObjectKind::Skill,
            )?;
            (target, origin, None)
        };
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        payload
            .screen(&state, &snapshot, &auth, &client, &target, &mut audit)
            .await?;
        let query = openai::strip_model_query(query.as_deref());
        let registration = if matches!(action, Action::Collection) && method == Method::POST {
            let registration = begin(
                &state,
                &auth,
                &client,
                &target,
                origin.clone(),
                protocol,
                NativeObjectKind::Skill,
                "skills",
                Vec::new(),
            )
            .await?;
            operation_id = Some(registration.operation_id());
            audit.registered(&registration);
            Some(registration)
        } else {
            None
        };
        if matches!(action, Action::Skill(_)) && method == Method::DELETE {
            let reserved = Deletion::reserve(
                &state,
                &auth,
                record.as_ref().ok_or_else(openai::dependency)?,
            )
            .await?;
            operation_id = Some(reserved.operation_id());
            audit.deleting(reserved.operation_id());
            deletion = Some(reserved);
        }
        let reply = if method == Method::GET
            && matches!(action, Action::Collection | Action::Versions(_))
        {
            list_owned(
                &state,
                &auth,
                &client,
                &target,
                &origin,
                protocol,
                NativeObjectKind::Skill,
                &path,
                &query,
                action.skill_id(),
            )
            .await?
        } else {
            send(
                &state,
                &target,
                &client,
                &origin,
                protocol,
                NativeObjectKind::Skill,
                method.clone(),
                &path,
                &query,
                payload.into_body()?,
            )
            .await?
        };
        if reply.status.is_success() {
            let binary = matches!(action, Action::Content(_) | Action::VersionContent(_, _));
            if !binary {
                let value: Value =
                    serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
                if let Some(registration) = &registration {
                    if protocol == NativeProtocol::Openai
                        && value
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(curated_openai_skill)
                    {
                        return Err(openai::invalid_identity());
                    }
                    if protocol == NativeProtocol::Anthropic && is_official_anthropic_skill(&value)
                    {
                        return Err(openai::invalid_identity());
                    }
                    registration.bind(&value).await?;
                }
                if matches!(action, Action::Version(_, _) | Action::Versions(_))
                    && method != Method::DELETE
                {
                    if let Some(id) = action.skill_id() {
                        if matches!(action, Action::Versions(_)) && method == Method::GET {
                            validate_versions(&value, id)?;
                        } else {
                            validate_version(&value, id)?;
                        }
                    }
                }
                if let Some(reserved) = &deletion {
                    reserved.commit().await?;
                    deletion = None;
                }
            }
            audit.output(&state, &auth, &target, &reply.bytes).await?;
        }
        Ok((response(protocol, reply), target))
    }
    .await;
    let upstream_rejected = result.as_ref().err().map(Deletion::upstream_rejected);
    if let (Some(upstream_rejected), Some(reserved)) = (upstream_rejected, deletion) {
        if let Err(error) = reserved.resolve(upstream_rejected).await {
            result = Err(error);
        }
    }
    audit
        .finish(
            &state,
            &snapshot,
            SURFACE,
            method,
            &endpoint,
            &auth,
            &client,
            started,
            result,
            operation_id,
        )
        .await
}

/// 官方精确标识是公开能力，不能把任意相同前缀的私有对象当公开对象。
pub(crate) fn curated_openai_skill(id: &str) -> bool {
    // https://developers.openai.com/api/docs/guides/tools-skills#curated-skills
    id == "openai-spreadsheets"
}

fn collect_skills<'a>(body: &'a Value, consumer: EndpointFamily, output: &mut Vec<&'a Value>) {
    fn skills<'a>(value: &'a Value, output: &mut Vec<&'a Value>) {
        if let Some(items) = value.get("skills").and_then(Value::as_array) {
            output.extend(items.iter().filter(|value| {
                value.get("type").and_then(Value::as_str) == Some("skill_reference")
            }));
        }
    }
    fn tool<'a>(value: &'a Value, output: &mut Vec<&'a Value>) {
        if value.get("type").and_then(Value::as_str) == Some("shell") {
            if let Some(environment) = value.get("environment").filter(|environment| {
                environment.get("type").and_then(Value::as_str) == Some("container_auto")
            }) {
                skills(environment, output);
            }
        }
    }
    fn input<'a>(value: &'a Value, output: &mut Vec<&'a Value>) {
        let Some(values) = value.as_array() else {
            return;
        };
        for value in values {
            match value.get("type").and_then(Value::as_str) {
                Some("additional_tools" | "tool_search_output") => {
                    if let Some(tools) = value.get("tools").and_then(Value::as_array) {
                        for value in tools {
                            tool(value, output);
                        }
                    }
                }
                Some("shell_call") => {
                    if let Some(environment) = value.get("environment").filter(|environment| {
                        environment.get("type").and_then(Value::as_str) == Some("container_auto")
                    }) {
                        skills(environment, output);
                    }
                }
                _ => {}
            }
        }
    }
    if consumer == EndpointFamily::Containers {
        skills(body, output);
        return;
    }
    if consumer == EndpointFamily::Conversations {
        if let Some(items) = body.get("items") {
            input(items, output);
        }
        return;
    }
    if consumer != EndpointFamily::Responses {
        return;
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for value in tools {
            tool(value, output);
        }
    }
    if let Some(choice) = body
        .get("tool_choice")
        .filter(|choice| choice.get("type").and_then(Value::as_str) == Some("allowed_tools"))
    {
        if let Some(tools) = choice.get("tools").and_then(Value::as_array) {
            for value in tools {
                tool(value, output);
            }
        }
    }
    if let Some(items) = body.get("input") {
        input(items, output);
    }
}

/// 消费准入只读取官方引用槽，函数的 schema、工具参数和内联包不被解释成对象 ID。
pub(crate) async fn collect_openai_skill_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    origin: &OriginBinding,
    consumer: EndpointFamily,
) -> Result<Vec<AcceptedReference>, ProxyError> {
    let mut skills = Vec::new();
    collect_skills(body, consumer, &mut skills);
    if !skills.is_empty() {
        if !aisix_core::native_objects::native_object_consumer_allowed(
            NativeProtocol::Openai,
            NativeObjectKind::Skill,
            consumer,
        ) {
            return Err(not_found());
        }
        let current = openai::current_auth(&state.snapshot.load(), auth)?;
        if !current.key().can_call(consumer) {
            return Err(not_found());
        }
    }
    let mut references = Vec::new();
    let mut seen = BTreeSet::new();
    for skill in skills {
        let id = skill
            .get("skill_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ProxyError::InvalidRequest("skill_id must be a nonempty string".into())
            })?;
        if curated_openai_skill(id) {
            continue;
        }
        if !seen.insert(id) {
            continue;
        }
        let (_, actual, record) = crate::attribution::detached(openai::lookup_reference(
            state,
            auth,
            client,
            id,
            NativeObjectKind::Skill,
            None,
            consumer,
        ))
        .await?;
        if !same_account(origin, &actual) {
            return Err(not_found());
        }
        references.push(AcceptedReference {
            record_id: record.record_id,
            consumer_family: consumer,
        });
    }
    Ok(references)
}

pub(crate) async fn validate_openai_skill_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    origin: &OriginBinding,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    collect_openai_skill_references(state, auth, client, body, origin, consumer)
        .await
        .map(|_| ())
}

/// 入口先查私有归属，真实 request 的目标账号在派发点再次证明。
pub(crate) async fn admit_openai_skill_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    checked: &mut Value,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    let mut found = Vec::new();
    collect_skills(body, consumer, &mut found);
    for skill in found {
        let id = skill
            .get("skill_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ProxyError::InvalidRequest("skill_id must be a nonempty string".into())
            })?;
        if !curated_openai_skill(id) {
            crate::attribution::detached(openai::lookup_reference(
                state,
                auth,
                client,
                id,
                NativeObjectKind::Skill,
                None,
                consumer,
            ))
            .await?;
        }
    }
    fn skills(value: &mut Value) {
        if let Some(skills) = value.get_mut("skills").and_then(Value::as_array_mut) {
            skills.retain(|skill| {
                skill.get("type").and_then(Value::as_str) != Some("skill_reference")
            });
        }
    }
    fn environment(value: &mut Value) {
        if value.get("type").and_then(Value::as_str) == Some("container_auto") {
            skills(value);
        }
    }
    fn tools(value: &mut Value) {
        if let Some(tools) = value.as_array_mut() {
            for tool in tools
                .iter_mut()
                .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("shell"))
            {
                if let Some(value) = tool.get_mut("environment") {
                    environment(value);
                }
            }
        }
    }
    if consumer == EndpointFamily::Containers {
        skills(checked);
        return Ok(());
    }
    if consumer == EndpointFamily::Responses {
        if let Some(value) = checked.get_mut("tools") {
            tools(value);
        }
        if let Some(value) = checked
            .get_mut("tool_choice")
            .filter(|choice| choice.get("type").and_then(Value::as_str) == Some("allowed_tools"))
        {
            if let Some(value) = value.get_mut("tools") {
                tools(value);
            }
        }
    }
    let field = if consumer == EndpointFamily::Conversations {
        "items"
    } else {
        "input"
    };
    for field in [field] {
        if let Some(items) = checked.get_mut(field).and_then(Value::as_array_mut) {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("additional_tools" | "tool_search_output") => {
                        if let Some(value) = item.get_mut("tools") {
                            tools(value);
                        }
                    }
                    Some("shell_call") => {
                        if let Some(value) = item.get_mut("environment") {
                            environment(value);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// 派发点传入实际捕获的 provider 配置；不能用重新选路的账号冒充本次上游。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn validate_openai_skills_for_target(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    model_id: &str,
    provider_key_id: &str,
    provider_key: &aisix_core::ProviderKey,
    surface: NativeOriginSurface,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    let mut references = Vec::new();
    collect_skills(body, consumer, &mut references);
    if references.is_empty() {
        return Ok(());
    }
    let key = state.native_origin_key().ok_or_else(openai::dependency)?;
    let provider_key_id = openai::resource_id(state, "provider_keys", provider_key_id)?;
    let origin = OriginBinding {
        model_id: openai::resource_id(state, "models", model_id)?,
        provider_key_id,
        surface: Some(surface),
        boundary: configured_native_origin_at(
            &key,
            provider_key_id,
            provider_key,
            NativeProtocol::Openai,
            NativeObjectKind::Skill,
            Some(surface),
        )
        .map_err(|_| openai::dependency())?,
    };
    validate_openai_skill_references(state, auth, client, body, &origin, consumer).await
}

pub(crate) async fn collect_anthropic_skill_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    origin: &OriginBinding,
) -> Result<Vec<AcceptedReference>, ProxyError> {
    let Some(skills) = body.pointer("/container/skills").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let snapshot = state.snapshot.load();
    let mut references = Vec::new();
    let mut seen = BTreeSet::new();
    for skill in skills {
        let id = skill
            .get("skill_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ProxyError::InvalidRequest(
                    "container.skills[].skill_id must be a nonempty string".into(),
                )
            })?;
        match skill.get("type").and_then(Value::as_str) {
            Some("custom") => {
                let (_, actual, record) =
                    crate::attribution::detached(anthropic::lookup_reference(
                        state,
                        auth,
                        client,
                        id,
                        NativeObjectKind::Skill,
                        None,
                        EndpointFamily::Messages,
                    ))
                    .await?;
                if !same_account(origin, &actual) {
                    return Err(not_found());
                }
                if seen.insert(record.record_id) {
                    references.push(AcceptedReference {
                        record_id: record.record_id,
                        consumer_family: EndpointFamily::Messages,
                    });
                }
            }
            Some("anthropic") => {
                crate::jobs::require_safe_upstream_id(id).map_err(|_| not_found())?;
                let requested = body
                    .get("model")
                    .and_then(Value::as_str)
                    .ok_or_else(not_found)?;
                let (target, actual) = crate::attribution::detached(async {
                    anthropic::resolve_reference_for_requested(
                        state,
                        &snapshot,
                        auth,
                        client,
                        &openai::model_entry_by_id(state, &snapshot, origin.model_id)?
                            .value
                            .display_name,
                        requested,
                        NativeObjectKind::Skill,
                        EndpointFamily::Messages,
                    )
                })
                .await?;
                if !same_account(origin, &actual) {
                    return Err(not_found());
                }
                let reply = crate::attribution::detached(send(
                    state,
                    &target,
                    client,
                    &actual,
                    NativeProtocol::Anthropic,
                    NativeObjectKind::Skill,
                    Method::GET,
                    &format!("/skills/{id}"),
                    "",
                    Body::Empty,
                ))
                .await?;
                let value: Value =
                    serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
                if !reply.status.is_success()
                    || value.get("id").and_then(Value::as_str) != Some(id)
                    || !is_official_anthropic_skill(&value)
                {
                    return Err(not_found());
                }
            }
            _ => {}
        }
    }
    Ok(references)
}

pub(crate) async fn validate_anthropic_skill_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    origin: &OriginBinding,
) -> Result<(), ProxyError> {
    collect_anthropic_skill_references(state, auth, client, body, origin)
        .await
        .map(|_| ())
}

/// 只读取官方容器引用槽；未知字段里的同名字符串不授予消费意图。
pub(crate) fn message_reference_ids(body: &Value) -> BTreeSet<(&'static str, String)> {
    let mut ids = BTreeSet::new();
    if let Some(id) = body.get("container").and_then(|container| {
        container
            .as_str()
            .or_else(|| container.get("id").and_then(Value::as_str))
    }) {
        ids.insert(("container", id.to_owned()));
    }
    if let Some(skills) = body.pointer("/container/skills").and_then(Value::as_array) {
        for id in skills
            .iter()
            .filter_map(|skill| skill.get("skill_id").and_then(Value::as_str))
        {
            ids.insert(("skill", id.to_owned()));
        }
    }
    ids
}

/// 提供商覆盖可以调整普通容器配置，不能引入调用人未在官方槽中声明的对象。
pub(crate) fn validate_message_reference_overrides(
    canonical: &BTreeSet<(&'static str, String)>,
    outbound: &Value,
) -> Result<(), ProxyError> {
    if message_reference_ids(outbound).is_subset(canonical) {
        return Ok(());
    }
    Err(ProxyError::Bridge(
        aisix_gateway::BridgeError::InvalidUpstreamConfig(
            "request overrides cannot introduce native object references".into(),
        ),
    ))
}

pub(crate) fn message_container_requested(body: &Value) -> bool {
    body.get("container").is_some_and(|value| !value.is_null())
        || body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| {
                tools.iter().any(|tool| {
                    tool.get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind.starts_with("code_execution_"))
                })
            })
}

pub(crate) fn message_overrides_affect_container(key: &aisix_core::ProviderKey) -> bool {
    key.request.as_ref().is_some_and(|request| {
        request
            .param_renames
            .iter()
            .any(|(from, to)| from == "container" || to == "container")
            || request.default_body_fields.contains_key("container")
    })
}

/// 缓存命中没有选中的目标；有潜在引用注入的配置走正常派发及逐目标拒绝。
pub(crate) fn messages_overrides_allow_cache(
    snapshot: &AisixSnapshot,
    model: &aisix_core::Model,
    body: &Value,
) -> bool {
    let canonical = message_reference_ids(body);
    let safe = |model: &aisix_core::Model| {
        let Ok(key) = crate::dispatch::resolve_provider_key(snapshot, model) else {
            return true;
        };
        let Some(request) = key
            .value
            .request
            .as_ref()
            .filter(|_| message_overrides_affect_container(&key.value))
        else {
            return true;
        };
        let mut outbound = body.clone();
        aisix_provider_openai::overrides::apply_param_renames(
            &mut outbound,
            &request.param_renames,
        );
        aisix_provider_openai::overrides::apply_default_body_fields(
            &mut outbound,
            &request.default_body_fields,
        );
        validate_message_reference_overrides(&canonical, &outbound).is_ok()
    };
    match model.routing.as_ref() {
        Some(routing) => routing
            .targets
            .iter()
            .filter_map(|target| snapshot.models.get_by_name(&target.model_ref(snapshot)))
            .all(|entry| safe(&entry.value)),
        None => safe(model),
    }
}

/// 结果中的容器 ID 必须在离开网关前绑定；stream 的最终 message_delta 同样经过这里。
pub(crate) struct MessageContainerAdmission {
    registration: anthropic::Registration,
    bound_id: tokio::sync::Mutex<Option<String>>,
}

impl MessageContainerAdmission {
    pub(crate) async fn observe(&self, value: &Value) -> Result<(), ProxyError> {
        // 官方 SDK RawMessageDeltaEvent.Delta.container；message_start 则放在 message 中。
        let container = value
            .get("container")
            .or_else(|| value.pointer("/message/container"))
            .or_else(|| value.pointer("/delta/container"));
        let Some(container) = container.filter(|value| !value.is_null()) else {
            return Ok(());
        };
        let id = container
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(openai::invalid_identity)?;
        let mut bound = self.bound_id.lock().await;
        if let Some(previous) = bound.as_deref() {
            return if previous == id {
                Ok(())
            } else {
                Err(openai::invalid_identity())
            };
        }
        self.registration
            .bind_related(container, Vec::new())
            .await?;
        *bound = Some(id.to_owned());
        Ok(())
    }
}

/// 本次 dispatch 的真实直连目标已确定；准入引用与 accepted owner 在上游前持久化。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn admit_messages_dispatch(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    requested_model: &str,
    direct_model: &str,
    expected: Option<&OriginBinding>,
) -> Result<Option<Arc<MessageContainerAdmission>>, ProxyError> {
    if !message_container_requested(body) {
        return Ok(None);
    }
    let expected = expected.ok_or_else(openai::dependency)?;
    let snapshot = state.snapshot.load();
    let (target, origin) = crate::attribution::detached(async {
        anthropic::resolve_reference_for_requested(
            state,
            &snapshot,
            auth,
            client,
            direct_model,
            requested_model,
            NativeObjectKind::MessageContainer,
            EndpointFamily::Messages,
        )
    })
    .await?;
    if !same_account(expected, &origin) {
        return Err(not_found());
    }
    let mut checked = body.clone();
    checked["model"] = requested_model.into();
    let mut references =
        collect_anthropic_skill_references(state, auth, client, &checked, &origin).await?;
    let container = body.get("container");
    let existing_id = container
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("id").and_then(Value::as_str))
        })
        .filter(|id| !id.is_empty());
    let owner_parent_record_id = if let Some(id) = existing_id {
        let (_, actual, record) = crate::attribution::detached(anthropic::lookup_reference(
            state,
            auth,
            client,
            id,
            NativeObjectKind::MessageContainer,
            None,
            EndpointFamily::Messages,
        ))
        .await?;
        if !same_account(expected, &actual) {
            return Err(not_found());
        }
        references.push(AcceptedReference {
            record_id: record.record_id,
            consumer_family: EndpointFamily::Messages,
        });
        Some(record.record_id)
    } else {
        None
    };
    let registration = crate::attribution::detached(anthropic::begin_related_operation(
        state,
        auth,
        &target,
        origin,
        NativeObjectKind::MessageContainer,
        None,
        client,
        "messages",
        InferenceStage::Attempt,
        references,
        owner_parent_record_id,
    ))
    .await?;
    Ok(Some(Arc::new(MessageContainerAdmission {
        registration,
        bound_id: tokio::sync::Mutex::new(None),
    })))
}

/// 入口只解除已归属的 private 引用；真正派发目标的同账号证明在 admit_messages_dispatch 完成。
pub(crate) async fn admit_anthropic_owned_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    checked: &mut Value,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    if consumer != EndpointFamily::Messages {
        return Ok(());
    }
    if let Some(skills) = body.pointer("/container/skills").and_then(Value::as_array) {
        for skill in skills
            .iter()
            .filter(|skill| skill.get("type").and_then(Value::as_str) == Some("custom"))
        {
            let id = skill
                .get("skill_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    ProxyError::InvalidRequest(
                        "container.skills[].skill_id must be a nonempty string".into(),
                    )
                })?;
            crate::attribution::detached(anthropic::lookup_reference(
                state,
                auth,
                client,
                id,
                NativeObjectKind::Skill,
                None,
                consumer,
            ))
            .await?;
        }
        if let Some(skills) = checked
            .pointer_mut("/container/skills")
            .and_then(Value::as_array_mut)
        {
            skills.retain(|skill| skill.get("type").and_then(Value::as_str) != Some("custom"));
        }
    }
    if let Some(id) = body
        .get("container")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("id").and_then(Value::as_str))
        })
        .filter(|id| !id.is_empty())
    {
        crate::attribution::detached(anthropic::lookup_reference(
            state,
            auth,
            client,
            id,
            NativeObjectKind::MessageContainer,
            None,
            consumer,
        ))
        .await?;
        if let Some(container) = checked.get_mut("container") {
            if let Some(object) = container.as_object_mut() {
                object.remove("id");
            } else {
                *container = Value::Null;
            }
        }
    }
    Ok(())
}
