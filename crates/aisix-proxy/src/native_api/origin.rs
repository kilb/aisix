//! 原生 Response 在标识符离开网关之前固定归属，成功创建后不得再次分派。

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;

use aisix_core::native_objects::{
    configured_native_origin, BeginRequest, BindRequest, LookupRequest, NativeChildObject,
    NativeObjectKind, NativeProtocol, NativeRecord, ObjectLocator, OriginBinding,
};
use aisix_core::{AisixSnapshot, ProviderKey, ResourceEntry};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;
use uuid::Uuid;

use super::registry::{proxy_error, NativeObjectRegistry};
use crate::auth::AuthenticatedKey;
use crate::error::ProxyError;
use crate::state::ProxyState;
use crate::stream_timeout::RawStreamError;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, RawStreamError>> + Send>>;

pub(crate) struct OriginRegistration {
    registry: Arc<dyn NativeObjectRegistry>,
    operation_id: Uuid,
    api_key_id: Uuid,
    existing_record_id: Option<Uuid>,
    origin: OriginBinding,
    bound_id: Option<String>,
    record: Option<NativeRecord>,
    bound_objects: BTreeSet<(String, String)>,
}

fn invalid_response() -> ProxyError {
    ProxyError::Bridge(aisix_gateway::BridgeError::UpstreamDecode(
        "native response identity is missing or inconsistent".into(),
    ))
}

/// 原始请求身份和账号边界先持久化；不使用调用方 request_id 作为幂等键。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn begin_response(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    api_key_id: &str,
    requested_model: &str,
    model_id: &str,
    provider: &ResourceEntry<ProviderKey>,
    upstream_model: &str,
    body: &Value,
    request_id: &str,
    upstream_input_text: Option<aisix_core::usage_io::TextMeasurement>,
) -> Result<Option<OriginRegistration>, ProxyError> {
    let Some(registry) = state.native_objects.as_ref() else {
        return Ok(None);
    };
    let platform = state.platform().ok_or(ProxyError::AccountUnavailable)?;
    let api_key_id = Uuid::parse_str(api_key_id).map_err(|_| ProxyError::AccountUnavailable)?;
    let model_id = Uuid::parse_str(model_id).map_err(|_| ProxyError::AccountUnavailable)?;
    let provider_key_id =
        Uuid::parse_str(&provider.id).map_err(|_| ProxyError::AccountUnavailable)?;
    let requested = crate::model_resolve::resolve_model(snapshot, requested_model)
        .ok_or(ProxyError::AccountUnavailable)?;
    let requested_model_id =
        Uuid::parse_str(&requested.id).map_err(|_| ProxyError::AccountUnavailable)?;
    let origin = OriginBinding {
        surface: None,
        model_id,
        provider_key_id,
        boundary: configured_native_origin(
            &platform.id_key,
            provider_key_id,
            &provider.value,
            NativeProtocol::Openai,
            NativeObjectKind::Response,
        )
        .map_err(|_| ProxyError::AccountUnavailable)?,
    };
    let operation_id = Uuid::new_v4();
    let reply = registry
        .begin(BeginRequest {
            upstream_input_text,
            input_file_id: None,
            operation_id,
            api_key_id,
            protocol: NativeProtocol::Openai,
            kind: NativeObjectKind::Response,
            origin: origin.clone(),
            parent_id: None,
            requested_model_id,
            requested_model: requested_model.to_owned(),
            upstream_model: upstream_model.to_owned(),
            upstream_provider: String::new(),
            operation: "responses".to_owned(),
            request_id: request_id.to_owned(),
            asynchronous: body
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            requested_service_tier: body
                .get("service_tier")
                .and_then(Value::as_str)
                .map(str::to_owned),
            entries: Vec::new(),
            observation_headers: Default::default(),
        })
        .await
        .map_err(proxy_error)?;
    if reply.operation_id != Some(operation_id)
        || reply.origin.as_ref() != Some(&origin)
        || reply.state != "pending_submission"
    {
        return Err(ProxyError::AccountUnavailable);
    }
    Ok(Some(OriginRegistration {
        registry: Arc::clone(registry),
        operation_id,
        api_key_id,
        existing_record_id: None,
        origin,
        bound_id: None,
        record: None,
        bound_objects: BTreeSet::new(),
    }))
}

/// 检索可能首次返回后台任务的项目，继续在原记录下登记，不创建新推理或用量。
pub(crate) fn resume_response(
    state: &ProxyState,
    api_key_id: &str,
    record: &NativeRecord,
) -> Result<Option<OriginRegistration>, ProxyError> {
    let Some(registry) = state.native_objects.as_ref() else {
        return Ok(None);
    };
    if record.protocol != Some(NativeProtocol::Openai)
        || record.kind != Some(NativeObjectKind::Response)
        || record.deleted
    {
        return Err(ProxyError::ResponseNotFound);
    }
    Ok(Some(OriginRegistration {
        registry: Arc::clone(registry),
        operation_id: record.operation_id.ok_or(ProxyError::AccountUnavailable)?,
        api_key_id: Uuid::parse_str(api_key_id).map_err(|_| ProxyError::AccountUnavailable)?,
        existing_record_id: Some(record.record_id),
        origin: record
            .origin
            .clone()
            .ok_or(ProxyError::AccountUnavailable)?,
        bound_id: Some(record.raw_id.clone()),
        record: Some(record.clone()),
        bound_objects: BTreeSet::new(),
    }))
}

impl OriginRegistration {
    pub(crate) fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    pub(crate) fn bound_record(&self) -> Option<&NativeRecord> {
        self.record.as_ref()
    }

    pub(crate) async fn bind_response(&mut self, response: &Value) -> Result<(), ProxyError> {
        self.bind_objects(response, response_objects(response))
            .await
    }

    pub(crate) async fn bind_input_items(&mut self, body: &Value) -> Result<(), ProxyError> {
        let id = self.bound_id.clone().ok_or_else(invalid_response)?;
        let items = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(invalid_response)?;
        self.bind_response(&serde_json::json!({"id":id,"output":items}))
            .await
    }

    async fn bind_objects(
        &mut self,
        response: &Value,
        objects: Vec<NativeChildObject>,
    ) -> Result<(), ProxyError> {
        let raw_id = self.response_id(response)?;
        let objects: Vec<_> = objects
            .into_iter()
            .filter(|object| {
                !self
                    .bound_objects
                    .contains(&(object.kind.as_str().into(), object.raw_id.clone()))
            })
            .collect();
        if self.bound_id.is_some() && objects.is_empty() {
            return Ok(());
        }
        let reply = self
            .registry
            .bind(BindRequest {
                existing_record_id: self.existing_record_id,
                objects: objects.clone(),
                operation_id: self.operation_id,
                api_key_id: self.api_key_id,
                raw_id: raw_id.to_owned(),
                provider_created_at: response
                    .get("created_at")
                    .and_then(Value::as_i64)
                    .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
                provider_expires_at: response
                    .get("expires_at")
                    .and_then(Value::as_i64)
                    .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
            })
            .await
            .map_err(|_| ProxyError::Bridge(aisix_gateway::BridgeError::DependencyUnavailable))?;
        let record = reply
            .record
            .filter(|record| {
                reply.state == "bound"
                    && record.operation_id == Some(self.operation_id)
                    && record.raw_id == raw_id
                    && record.origin.as_ref() == Some(&self.origin)
                    && record.protocol == Some(NativeProtocol::Openai)
                    && record.kind == Some(NativeObjectKind::Response)
                    && self
                        .record
                        .as_ref()
                        .is_none_or(|previous| previous.record_id == record.record_id)
                    && !record.deleted
            })
            .ok_or(ProxyError::Bridge(
                aisix_gateway::BridgeError::DependencyUnavailable,
            ))?;
        for object in objects {
            if !reply.records.iter().any(|child| {
                child.raw_id == object.raw_id
                    && child.kind == Some(object.kind)
                    && child.protocol == Some(NativeProtocol::Openai)
                    && child.origin.as_ref() == Some(&self.origin)
                    && child.parent_id.is_some()
                    && !child.deleted
            }) {
                return Err(ProxyError::Bridge(
                    aisix_gateway::BridgeError::DependencyUnavailable,
                ));
            }
            self.bound_objects
                .insert((object.kind.as_str().into(), object.raw_id));
        }
        self.bound_id = Some(raw_id.to_owned());
        self.record = Some(record);
        Ok(())
    }

    fn validate_event(&self, event: &Value) -> Result<(), ProxyError> {
        let response = event.get("response").filter(|v| v.is_object());
        if let Some(response) = response {
            self.response_id(response)?;
        }
        if let Some(id) = event.get("response_id").and_then(Value::as_str) {
            if self
                .bound_id
                .as_deref()
                .or_else(|| response.and_then(|r| r.get("id")).and_then(Value::as_str))
                != Some(id)
            {
                return Err(invalid_response());
            }
        }
        Ok(())
    }

    async fn bind_event(&mut self, event: &Value) -> Result<(), ProxyError> {
        self.validate_event(event)?;
        let response = event.get("response").filter(|v| v.is_object());
        let mut objects = response.map(response_objects).unwrap_or_default();
        if let Some(item) = event.get("item") {
            item_objects(item, &mut objects);
        }
        if let Some(id) = event
            .get("item_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            objects.push(NativeChildObject {
                kind: NativeObjectKind::ResponseItem,
                raw_id: id.into(),
            });
        }
        if let Some(response) = response {
            self.bind_objects(response, objects).await
        } else if !objects.is_empty() {
            let id = self.bound_id.clone().ok_or_else(invalid_response)?;
            self.bind_objects(&serde_json::json!({"id":id}), objects)
                .await
        } else {
            Ok(())
        }
    }

    fn response_id<'a>(&self, response: &'a Value) -> Result<&'a str, ProxyError> {
        let raw_id = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| safe_id(id))
            .ok_or_else(invalid_response)?;
        if self
            .bound_id
            .as_deref()
            .is_some_and(|bound| bound != raw_id)
        {
            return Err(invalid_response());
        }
        Ok(raw_id)
    }
}

/// 只在完整事件绑定成功后释放原始字节；保留事件边界、注释和原始 JSON。
#[cfg(test)]
pub(crate) fn register_stream(
    registration: Option<OriginRegistration>,
    stream: ByteStream,
) -> ByteStream {
    register_stream_observed(registration, stream, |_| {})
}

/// 用量观察先于归属服务等待；观察不会释放未绑定的事件或正文。
pub(crate) fn register_stream_observed<F>(
    registration: Option<OriginRegistration>,
    stream: ByteStream,
    mut observe: F,
) -> ByteStream
where
    F: FnMut(&Value) + Send + 'static,
{
    let Some(mut registration) = registration else {
        return stream;
    };
    Box::pin(crate::request_id::in_request_span(async_stream::stream! {
        let mut stream = stream;
        let mut frames = aisix_gateway::sse::SseFrameSplitter::new(aisix_gateway::sse::MAX_SSE_FRAME_BYTES);
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk { Ok(chunk) => chunk, Err(error) => { yield Err(error); return; } };
            frames.push(&chunk);
            loop {
                let frame = match frames.next_frame() {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(_) => { yield Err(RawStreamError::UpstreamDecode); return; }
                };
                if frame.len() > aisix_gateway::sse::MAX_SSE_FRAME_BYTES {
                    yield Err(RawStreamError::UpstreamDecode); return;
                }
                let fields = aisix_gateway::sse::parse_frame(&frame);
                if let Some(data) = fields.data {
                    if let Ok(event) = serde_json::from_slice::<Value>(&data) {
                        if event.get("response").is_some_and(Value::is_object) {
                            if registration.validate_event(&event).is_err() {
                                yield Err(RawStreamError::UpstreamDecode);
                                return;
                            }
                            observe(&event);
                        }
                        if let Err(error) = registration.bind_event(&event).await {
                            yield Err(match error {
                                ProxyError::Bridge(aisix_gateway::BridgeError::DependencyUnavailable) => RawStreamError::DependencyUnavailable,
                                _ => RawStreamError::UpstreamDecode,
                            });
                            return;
                        }
                    }
                }
                yield Ok(Bytes::from(frame));
            }
        }
        // 未闭合的事件不属于可释放的完整 Response，交给原有解码错误分类。
        if !frames.buffered().is_empty() {
            yield Err(RawStreamError::UpstreamDecode);
        }
    }))
}

/// 仅真正由上游读取存储对象的槽位查归属；完整内联工具历史不要求预登记。
pub(crate) async fn admit_input_references(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    body: &mut Value,
    affinity: &mut crate::responses_affinity::ContextAffinity,
) -> Result<(), ProxyError> {
    let Some(platform) = state.platform() else {
        return Ok(());
    };
    let owner = crate::platform::tenant_owner(auth);
    let sealer = crate::opaque_id::Sealer::of(platform, &owner);
    if let Some(items) = body.get("input").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            if !crate::opaque_id::is_item_reference(item) {
                continue;
            }
            let refused = || ProxyError::StoredObjectReference(format!("input[{index}].id"));
            let raw = item.get("id").and_then(Value::as_str).ok_or_else(refused)?;
            // 旧封装只恢复查找键，不能代替当前成员、模型和账号的持久归属证明。
            let original = sealer.unseal(crate::opaque_id::IdKind::Item, raw);
            let raw = original.as_deref().unwrap_or(raw);
            let reference =
                match resolve_object_reference(state, auth, raw, NativeObjectKind::ResponseItem)
                    .await
                {
                    Ok(reference) => reference,
                    // 已知原生历史仍固定原账号；可内联的文本没有登记时由实际目标决定。
                    Err(ProxyError::ResponseNotFound | ProxyError::AccountUnavailable)
                        if inline_reasoning_reference(item) =>
                    {
                        affinity.defer_reasoning_authority();
                        continue;
                    }
                    Err(ProxyError::ResponseNotFound) => return Err(refused()),
                    Err(error) => return Err(error),
                };
            affinity.admit_object_origin(reference.origin.ok_or(ProxyError::ResponseNotFound)?)?;
        }
    }
    let mut paths = Vec::new();
    for root in ["input", "tools", "tool_choice"] {
        if let Some(value) = body.get(root) {
            container_paths(value, &format!("/{root}"), &mut paths);
        }
    }
    for pointer in paths {
        let raw = body
            .pointer(&pointer)
            .and_then(Value::as_str)
            .ok_or(ProxyError::ResponseNotFound)?
            .to_owned();
        let original = sealer
            .unseal(crate::opaque_id::IdKind::Container, &raw)
            .unwrap_or(raw);
        let reference =
            resolve_object_reference(state, auth, &original, NativeObjectKind::ResponseContainer)
                .await?;
        affinity.admit_object_origin(reference.origin.ok_or(ProxyError::ResponseNotFound)?)?;
        if let Some(value) = body.pointer_mut(&pointer) {
            *value = Value::String(original);
        }
        let checked_pointer = pointer
            .strip_suffix("/container_id")
            .filter(|parent| {
                body.pointer(parent)
                    .and_then(|value| value.get("type"))
                    .and_then(Value::as_str)
                    == Some("container_reference")
            })
            .unwrap_or(&pointer)
            .to_owned();
        affinity.note_owned_container(checked_pointer);
    }
    // 已验证的存储引用与完整内联历史继续恢复协议 ID，二者不会互相授予权限。
    let _ = crate::opaque_id::unseal_responses_input(platform, &owner, body);
    affinity.note_unopened_item_reference(None);
    Ok(())
}

// 可见 reasoning 既能内联翻译，也能在原生协议中读取存储状态；选定目标后再决定。
fn inline_reasoning_reference(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("reasoning")
        && crate::opaque_id::is_item_reference(item)
        && crate::responses_bridge::replayed_reasoning_text(item).is_some()
}

/// 翻译目标只消费可见文本；每个原生尝试必须先证明同一存储对象及账号边界。
pub(crate) async fn admit_native_reasoning_references(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    auth: &AuthenticatedKey,
    model: &aisix_core::Model,
    body: &Value,
) -> Result<(), ProxyError> {
    let Some(platform) = state.platform() else {
        return Ok(());
    };
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return Ok(());
    };
    for (index, item) in items.iter().enumerate() {
        if item.get("type").and_then(Value::as_str) != Some("reasoning")
            || !crate::opaque_id::is_item_reference(item)
        {
            continue;
        }
        let refused = || ProxyError::StoredObjectReference(format!("input[{index}].id"));
        let raw = item.get("id").and_then(Value::as_str).ok_or_else(refused)?;
        let reference = resolve_object_reference(state, auth, raw, NativeObjectKind::ResponseItem)
            .await
            .map_err(|error| {
                if matches!(error, ProxyError::ResponseNotFound) {
                    refused()
                } else {
                    error
                }
            })?;
        let origin = reference.origin.ok_or(ProxyError::AccountUnavailable)?;
        let provider = model
            .provider_key_id
            .as_deref()
            .and_then(|id| snapshot.provider_keys.get_by_id(id))
            .filter(|provider| provider.id == origin.provider_key_id.to_string())
            .ok_or_else(refused)?;
        let boundary = configured_native_origin(
            &platform.id_key,
            origin.provider_key_id,
            &provider.value,
            NativeProtocol::Openai,
            NativeObjectKind::Response,
        )
        .map_err(|_| ProxyError::AccountUnavailable)?;
        if boundary != origin.boundary {
            return Err(refused());
        }
    }
    Ok(())
}

fn container_paths(value: &Value, path: &str, paths: &mut Vec<String>) {
    let kind = value.get("type").and_then(Value::as_str);
    if matches!(
        kind,
        Some("function" | "custom" | "tool_search_call" | "mcp_list_tools")
    ) {
        return;
    }
    let field = match kind {
        Some("code_interpreter") => Some("container"),
        Some("code_interpreter_call" | "container_reference") => Some("container_id"),
        _ => None,
    };
    if let Some(field) = field {
        if value
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        {
            paths.push(format!("{path}/{field}"));
        }
    }
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                container_paths(item, &format!("{path}/{index}"), paths);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                // 模型和用户写出的工具参数、文本及模式不是协议对象树。
                if matches!(
                    key.as_str(),
                    "arguments" | "parameters" | "input" | "output" | "content" | "metadata"
                ) {
                    continue;
                }
                let key = key.replace('~', "~0").replace('/', "~1");
                container_paths(value, &format!("{path}/{key}"), paths);
            }
        }
        _ => {}
    }
}

fn item_objects(item: &Value, objects: &mut Vec<NativeChildObject>) {
    for (pointer, kind) in [
        ("/id", NativeObjectKind::ResponseItem),
        ("/approval_request_id", NativeObjectKind::ResponseItem),
        ("/container_id", NativeObjectKind::ResponseContainer),
        (
            "/environment/container_id",
            NativeObjectKind::ResponseContainer,
        ),
    ] {
        if let Some(id) = item
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            objects.push(NativeChildObject {
                kind,
                raw_id: id.to_owned(),
            });
        }
    }
}

fn response_objects(response: &Value) -> Vec<NativeChildObject> {
    let mut objects = Vec::new();
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        item_objects(item, &mut objects);
    }
    objects
}

pub(crate) struct ResolvedReference {
    pub requested_model: String,
    pub origin: Option<OriginBinding>,
    pub record: Option<NativeRecord>,
    pub raw_id: String,
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

pub(crate) fn verify_origin(
    state: &ProxyState,
    snapshot: &AisixSnapshot,
    origin: &OriginBinding,
) -> Result<(), ProxyError> {
    let model = snapshot
        .models
        .get_by_id(&origin.model_id.to_string())
        .ok_or(ProxyError::AccountUnavailable)?;
    if model.value.provider_key_id.as_deref() != Some(origin.provider_key_id.to_string().as_str()) {
        return Err(ProxyError::AccountUnavailable);
    }
    let provider = snapshot
        .provider_keys
        .get_by_id(&origin.provider_key_id.to_string())
        .ok_or(ProxyError::AccountUnavailable)?;
    let platform = state.platform().ok_or(ProxyError::AccountUnavailable)?;
    let actual = configured_native_origin(
        &platform.id_key,
        origin.provider_key_id,
        &provider.value,
        NativeProtocol::Openai,
        NativeObjectKind::Response,
    )
    .map_err(|_| ProxyError::AccountUnavailable)?;
    if actual != origin.boundary {
        return Err(ProxyError::AccountUnavailable);
    }
    Ok(())
}

pub(crate) async fn resolve_reference(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    raw_id: &str,
) -> Result<ResolvedReference, ProxyError> {
    resolve_object_reference(state, auth, raw_id, NativeObjectKind::Response).await
}

pub(crate) async fn resolve_object_reference(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    raw_id: &str,
    kind: NativeObjectKind,
) -> Result<ResolvedReference, ProxyError> {
    let snapshot = state.snapshot.load();
    // 旧封装只是原始 ID 的别名；它没有凭据版本，不能代替持久账号归属授权。
    let legacy =
        crate::responses_affinity::legacy_response_reference(state.platform(), auth, raw_id)
            .filter(|_| kind == NativeObjectKind::Response);
    let raw_id = legacy
        .as_ref()
        .map(|(_, original)| original.as_str())
        .unwrap_or(raw_id);
    if !safe_id(raw_id) {
        return Err(ProxyError::ResponseNotFound);
    }
    let registry = state
        .native_objects
        .as_ref()
        .ok_or(ProxyError::ResponseNotFound)?;
    let api_key_id = Uuid::parse_str(&auth.entry.id).map_err(|_| ProxyError::ResponseNotFound)?;
    let reply = registry
        .lookup(LookupRequest {
            consumer_family: None,
            api_key_id,
            locator: ObjectLocator {
                protocol: NativeProtocol::Openai,
                kind,
                raw_id: raw_id.to_owned(),
                origin: None,
                parent_id: None,
            },
        })
        .await
        .map_err(proxy_error)?;
    let record = reply
        .record
        .filter(|r| {
            reply.state == "found"
                && !r.deleted
                && r.raw_id == raw_id
                && r.protocol == Some(NativeProtocol::Openai)
                && r.kind == Some(kind)
        })
        .ok_or(ProxyError::ResponseNotFound)?;
    let origin = record
        .origin
        .clone()
        .ok_or(ProxyError::AccountUnavailable)?;
    if legacy
        .as_ref()
        .is_some_and(|(model_id, _)| model_id != &origin.model_id.to_string())
    {
        return Err(ProxyError::ResponseNotFound);
    }
    verify_origin(state, &snapshot, &origin)?;
    let requested_id = record
        .requested_model_id
        .ok_or(ProxyError::AccountUnavailable)?;
    let requested_model = snapshot
        .models
        .get_by_id(&requested_id.to_string())
        .ok_or(ProxyError::ResponseNotFound)?;
    let target = snapshot
        .models
        .get_by_id(&origin.model_id.to_string())
        .ok_or(ProxyError::ResponseNotFound)?;
    if !auth
        .key()
        .can_access(&snapshot, &requested_model.value.display_name)
        || (!auth.key().allowed_provider_groups.is_empty()
            && !crate::routing::target_in_groups(
                &snapshot,
                &target.value,
                &auth.key().allowed_provider_groups,
            ))
    {
        return Err(ProxyError::ResponseNotFound);
    }
    Ok(ResolvedReference {
        requested_model: requested_model.value.display_name.clone(),
        origin: Some(origin),
        record: Some(record),
        raw_id: raw_id.to_owned(),
    })
}

#[cfg(test)]
#[path = "origin_tests.rs"]
mod tests;
