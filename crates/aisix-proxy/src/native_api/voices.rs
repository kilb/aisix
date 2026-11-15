//! 自定义 Voice 与 Consent 是项目内原生对象；消费引用不授予管理其他对象的权限。
use std::time::Instant;

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{
    configured_native_origin_at, AcceptedReference, NativeObjectKind, NativeOriginSurface,
    NativeProtocol, OriginBinding,
};
use axum::extract::{RawQuery, Request, State};
use axum::http::Method;
use axum::response::Response;
use serde_json::Value;

use super::{openai, skills};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::reject::AisixPath;
use crate::state::ProxyState;

pub(crate) const VOICES_SURFACE: crate::operation::Surface = crate::operation::Surface {
    handler: "audio",
    operation: "voices",
};
pub(crate) const CONSENTS_SURFACE: crate::operation::Surface = crate::operation::Surface {
    handler: "audio",
    operation: "voice_consents",
};

enum Action {
    Consents,
    Consent(String),
    CreateVoice,
    DeleteVoice(String),
}

impl Action {
    fn kind(&self) -> NativeObjectKind {
        match self {
            Self::Consents | Self::Consent(_) => NativeObjectKind::VoiceConsent,
            Self::CreateVoice | Self::DeleteVoice(_) => NativeObjectKind::Voice,
        }
    }
    fn id(&self) -> Option<&str> {
        match self {
            Self::Consent(id) | Self::DeleteVoice(id) => Some(id),
            _ => None,
        }
    }
    fn path(&self) -> String {
        match self {
            Self::Consents => "/audio/voice_consents".into(),
            Self::Consent(id) => format!("/audio/voice_consents/{id}"),
            Self::CreateVoice => "/audio/voices".into(),
            Self::DeleteVoice(id) => format!("/audio/voices/{id}"),
        }
    }
}

pub(crate) async fn consents(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(state, auth, client, query, request, Action::Consents).await
}

pub(crate) async fn consent(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(state, auth, client, query, request, Action::Consent(id)).await
}

pub(crate) async fn create(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(state, auth, client, query, request, Action::CreateVoice).await
}

pub(crate) async fn delete(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    AisixPath(id): AisixPath<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    handle(state, auth, client, query, request, Action::DeleteVoice(id)).await
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
    let method = request.method().clone();
    let path = action.path();
    let endpoint = format!("/v1{path}");
    let kind = action.kind();
    let surface = if kind == NativeObjectKind::Voice {
        VOICES_SURFACE
    } else {
        CONSENTS_SURFACE
    };
    let snapshot = state.snapshot.load();
    let mut audit = skills::Audit::new(NativeProtocol::Openai);
    let mut operation_id = None;
    let mut deletion = None;
    let mut result = async {
        let mut payload = skills::Payload::parse(&state, request, &endpoint).await?;
        let (target, origin, record) = if let Some(id) = action.id() {
            let (target, origin, record) =
                skills::lookup(&state, &auth, &client, id, NativeProtocol::Openai, kind).await?;
            (target, origin, Some(record))
        } else {
            // 官方 Voice 的 body.model 选择创建算法；账号选择只读网关自己的 hint。
            let wanted = openai::model_hint(query.as_deref(), &client).ok_or_else(|| {
                ProxyError::InvalidRequest(
                    "select a direct model with x-aisix-model or ?model=".into(),
                )
            })?;
            let (target, origin) = skills::resolve(
                &state,
                &snapshot,
                &auth,
                &client,
                &wanted,
                NativeProtocol::Openai,
                kind,
            )?;
            (target, origin, None)
        };
        audit.resolved(&target);
        let _reservation = crate::jobs::reserve_target(&state, &snapshot, &auth, &target).await?;
        payload
            .screen(&state, &snapshot, &auth, &client, &target, &mut audit)
            .await?;
        let mut references = Vec::new();
        if matches!(action, Action::CreateVoice)
            && payload.value.get("type").and_then(Value::as_str) != Some("prompt")
        {
            if let Some(consent) = payload.value.get("consent") {
                let consent = consent
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        ProxyError::InvalidRequest("consent must be a nonempty string".into())
                    })?;
                let (_, actual, record) = crate::attribution::detached(openai::lookup_reference(
                    &state,
                    &auth,
                    &client,
                    consent,
                    NativeObjectKind::VoiceConsent,
                    None,
                    EndpointFamily::Audio,
                ))
                .await?;
                if !skills::same_account(&origin, &actual) {
                    return Err(skills::not_found());
                }
                references.push(AcceptedReference {
                    record_id: record.record_id,
                    consumer_family: EndpointFamily::Audio,
                });
            }
        }
        let creating =
            method == Method::POST && matches!(action, Action::Consents | Action::CreateVoice);
        let registration = if creating {
            let registration = skills::begin(
                &state,
                &auth,
                &client,
                &target,
                origin.clone(),
                NativeProtocol::Openai,
                kind,
                surface.operation,
                references,
            )
            .await?;
            operation_id = Some(registration.operation_id());
            audit.registered(&registration);
            Some(registration)
        } else {
            None
        };
        if method == Method::DELETE {
            let reserved = skills::Deletion::reserve(
                &state,
                &auth,
                record.as_ref().ok_or_else(openai::dependency)?,
            )
            .await?;
            operation_id = Some(reserved.operation_id());
            audit.deleting(reserved.operation_id());
            deletion = Some(reserved);
        }
        let query = openai::strip_model_query(query.as_deref());
        let reply = if method == Method::GET && matches!(action, Action::Consents) {
            skills::list_owned(
                &state,
                &auth,
                &client,
                &target,
                &origin,
                NativeProtocol::Openai,
                kind,
                &path,
                &query,
                None,
            )
            .await?
        } else {
            skills::send(
                &state,
                &target,
                &client,
                &origin,
                NativeProtocol::Openai,
                kind,
                method.clone(),
                &path,
                &query,
                payload.into_body()?,
            )
            .await?
        };
        if reply.status.is_success() {
            if let Some(registration) = &registration {
                let value: Value =
                    serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
                let expected = if kind == NativeObjectKind::Voice {
                    "audio.voice"
                } else {
                    "audio.voice_consent"
                };
                if value.get("object").and_then(Value::as_str) != Some(expected) {
                    return Err(openai::invalid_identity());
                }
                registration.bind(&value).await?;
            } else if let Some(id) = action.id() {
                if !reply.bytes.is_empty() {
                    let value: Value = serde_json::from_slice(&reply.bytes)
                        .map_err(|_| openai::invalid_identity())?;
                    if value.get("id").and_then(Value::as_str) != Some(id) {
                        return Err(openai::invalid_identity());
                    }
                }
                if let Some(reserved) = &deletion {
                    reserved.commit().await?;
                    deletion = None;
                }
            }
            audit.output(&state, &auth, &target, &reply.bytes).await?;
        }
        Ok((skills::response(NativeProtocol::Openai, reply), target))
    }
    .await;
    let upstream_rejected = result
        .as_ref()
        .err()
        .map(skills::Deletion::upstream_rejected);
    if let (Some(upstream_rejected), Some(reserved)) = (upstream_rejected, deletion) {
        if let Err(error) = reserved.resolve(upstream_rejected).await {
            result = Err(error);
        }
    }
    audit
        .finish(
            &state,
            &snapshot,
            surface,
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

/// 内置名称是普通值，只有官方自定义 Voice 对象槽需要持久对象证明。
pub(crate) async fn validate_voice_reference(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    voice: &Value,
    origin: &OriginBinding,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    let Some(object) = voice.as_object() else {
        return Ok(());
    };
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ProxyError::InvalidRequest("voice.id must be a nonempty string".into()))?;
    let (_, actual, _) = crate::attribution::detached(openai::lookup_reference(
        state,
        auth,
        client,
        id,
        NativeObjectKind::Voice,
        None,
        consumer,
    ))
    .await?;
    if !skills::same_account(origin, &actual) {
        return Err(skills::not_found());
    }
    Ok(())
}

// 仅计算最终官方槽，不允许覆盖规则从其他字段或静态配置引入私有引用。
fn voice_override_affects_slot(key: &aisix_core::ProviderKey, field: &str) -> bool {
    key.request.as_ref().is_some_and(|request| {
        request
            .param_renames
            .iter()
            .any(|(from, to)| from == field || to == field)
            || request.default_body_fields.contains_key(field)
    })
}

fn voice_after_overrides(
    mut body: Value,
    original: Option<&Value>,
    key: &aisix_core::ProviderKey,
    field: &str,
    nested: bool,
) -> Result<Option<Value>, ProxyError> {
    if let Some(request) = &key.request {
        aisix_provider_openai::overrides::validate_content_safe_request_overrides(request)
            .map_err(|message| {
                ProxyError::Bridge(aisix_gateway::BridgeError::InvalidUpstreamConfig(message))
            })?;
        aisix_provider_openai::overrides::apply_param_renames(&mut body, &request.param_renames);
        if let Some(constraints) = &request.param_constraints {
            aisix_provider_openai::overrides::apply_param_constraints(&mut body, constraints);
        }
        aisix_provider_openai::overrides::apply_default_body_fields(
            &mut body,
            &request.default_body_fields,
        );
    }
    let effective = body.get(field).and_then(|value| {
        if nested {
            value.get("voice")
        } else {
            Some(value)
        }
    });
    if let Some(voice) = effective.filter(|voice| voice.is_object()) {
        if !original.is_some_and(Value::is_object)
            || voice.get("id") != original.and_then(|voice| voice.get("id"))
        {
            return Err(ProxyError::Bridge(
                aisix_gateway::BridgeError::InvalidUpstreamConfig(
                    "request overrides cannot introduce a custom voice reference".into(),
                ),
            ));
        }
    }
    Ok(effective.cloned())
}

pub(crate) fn chat_voice_after_overrides(
    request: &aisix_gateway::ChatFormat,
    key: &aisix_core::ProviderKey,
) -> Result<Option<Value>, ProxyError> {
    let original = request
        .extra
        .get("audio")
        .and_then(|audio| audio.get("voice"));
    if !voice_override_affects_slot(key, "audio") {
        return Ok(original.cloned());
    }
    // 只有顶层参数覆盖可能改变此槽；模型名、消息投影与token cap不生成VoiceID。
    let body = serde_json::to_value(request).map_err(|_| openai::dependency())?;
    voice_after_overrides(body, original, key, "audio", true)
}

pub(crate) fn speech_voice_after_overrides(
    body: &Value,
    key: &aisix_core::ProviderKey,
) -> Result<Option<Value>, ProxyError> {
    let original = body.get("voice");
    if !voice_override_affects_slot(key, "voice") {
        return Ok(original.cloned());
    }
    voice_after_overrides(body.clone(), original, key, "voice", false)
}

/// 使用本次已经选定并捕获的配置，不根据模型别名再选一次账号。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn validate_voice_for_target(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    voice: &Value,
    model_id: &str,
    provider_key_id: &str,
    provider_key: &aisix_core::ProviderKey,
    surface: NativeOriginSurface,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    if !voice.is_object() {
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
            NativeObjectKind::Voice,
            Some(surface),
        )
        .map_err(|_| openai::dependency())?,
    };
    validate_voice_reference(state, auth, client, voice, &origin, consumer).await
}

pub(crate) async fn validate_voice_slot(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    origin: &OriginBinding,
    consumer: EndpointFamily,
) -> Result<(), ProxyError> {
    let voices = match consumer {
        EndpointFamily::Audio => body.get("voice").into_iter().collect::<Vec<_>>(),
        EndpointFamily::ChatCompletions => body.pointer("/audio/voice").into_iter().collect(),
        EndpointFamily::Realtime => [
            "/session/audio/output/voice",
            "/response/audio/output/voice",
            "/audio/output/voice",
            "/session/voice",
            "/response/voice",
            "/voice",
        ]
        .into_iter()
        .filter_map(|pointer| body.pointer(pointer))
        .collect(),
        _ => return Err(skills::not_found()),
    };
    for voice in voices {
        validate_voice_reference(state, auth, client, voice, origin, consumer).await?;
    }
    Ok(())
}

/// WS 只在官方事件槽解释自定义引用；JSON binary 与 text 使用同一当前授权。
#[cfg(test)]
pub(crate) async fn validate_realtime_frame(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    bytes: &[u8],
    origin: Option<&OriginBinding>,
    requested_model: &str,
) -> Result<(), ProxyError> {
    let frame = super::realtime_frame::Frame::new(bytes);
    validate_realtime_config(state, auth, client, &frame, origin, requested_model).await
}

pub(crate) async fn validate_realtime_config(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    frame: &super::realtime_frame::Frame<'_>,
    origin: Option<&OriginBinding>,
    requested_model: &str,
) -> Result<(), ProxyError> {
    use super::realtime_frame::Kind;
    let field = match frame.kind() {
        Kind::SessionUpdate => "session",
        Kind::ResponseCreate => "response",
        _ => return Ok(()),
    };
    let Some(config) = frame.json().and_then(|event| event.get(field)) else {
        return Ok(());
    };
    let voices = ["/audio/output/voice", "/voice"]
        .iter()
        .filter_map(|slot| config.pointer(slot))
        .filter(|voice| voice.is_object())
        .collect::<Vec<_>>();
    if voices.is_empty() {
        return Ok(());
    }
    let expected = origin.ok_or_else(openai::dependency)?;
    let snapshot = state.snapshot.load();
    let current = openai::current_auth(&snapshot, auth)?;
    let direct = openai::model_entry_by_id(state, &snapshot, expected.model_id)?;
    let (_, actual) = crate::attribution::detached(async {
        openai::resolve_reference_for_requested(
            state,
            &snapshot,
            &current,
            client,
            &direct.value.display_name,
            requested_model,
            NativeObjectKind::Voice,
            EndpointFamily::Realtime,
        )
    })
    .await?;
    if !skills::same_account(expected, &actual) {
        return Err(skills::not_found());
    }
    for voice in voices {
        validate_voice_reference(
            state,
            &current,
            client,
            voice,
            &actual,
            EndpointFamily::Realtime,
        )
        .await?;
    }
    Ok(())
}
