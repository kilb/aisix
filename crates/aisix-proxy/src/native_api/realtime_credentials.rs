//! 临时凭据只授予对应实时入口；供应商凭据不离开网关，当前权限在每次连接重新读取。

use std::time::Instant;

use aisix_core::native_objects::{InferenceStage, NativeObjectKind};
use aisix_core::native_realtime::{
    MintCredentialRequest, NativeCredential, RealtimePurpose, RedeemCredentialRequest,
};
use axum::extract::State;
use axum::http::Method;
use axum::response::Response;
use base64::Engine as _;
use bytes::Bytes;
use rand::RngCore as _;
use serde_json::Value;

use super::openai::{self, Audit, Body};
use super::registry::{LiveUsageAuthority, NativeRegistryError};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::request_json::{Json, JsonRejection};
use crate::state::ProxyState;

const PREFIX: &str = "aisix_rt_";
const KIND: NativeObjectKind = NativeObjectKind::RealtimeCall;

#[derive(Clone, Copy)]
enum Mint {
    Realtime,
    Translation,
}

impl Mint {
    fn path(self) -> &'static str {
        match self {
            Self::Realtime => "/realtime/client_secrets",
            Self::Translation => "/realtime/translations/client_secrets",
        }
    }

    fn purpose(self, body: &Value) -> RealtimePurpose {
        match self {
            Self::Translation => RealtimePurpose::Translation,
            Self::Realtime
                if body.pointer("/session/type").and_then(Value::as_str)
                    == Some("transcription") =>
            {
                RealtimePurpose::Transcription
            }
            _ => RealtimePurpose::Realtime,
        }
    }
}

pub(crate) async fn client_secrets(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    mint(state, auth, client, body, Mint::Realtime).await
}

pub(crate) async fn translation_client_secrets(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    client: ClientContext,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    mint(state, auth, client, body, Mint::Translation).await
}

fn model_pointer(body: &Value, action: Mint) -> &'static str {
    match action {
        Mint::Realtime if action.purpose(body) == RealtimePurpose::Transcription => {
            "/session/audio/input/transcription/model"
        }
        _ => "/session/model",
    }
}

// 只剥离协议凭据槽；工具参数 Schema 可合法声明 value/token 等同名属性。
fn strip_credential_slots(value: &mut Value) {
    if let Some(fields) = value.as_object_mut() {
        for name in [
            "value",
            "client_secret",
            "api_key",
            "authorization",
            "access_token",
            "refresh_token",
        ] {
            fields.remove(name);
        }
    }
}

async fn mint(
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    body: Result<Json<Value>, JsonRejection>,
    action: Mint,
) -> Response {
    crate::attribution::note_requested_stream(Some(false));
    let started = Instant::now();
    let endpoint = format!("/v1{}", action.path());
    let Json(mut body) = match body {
        Ok(body) => body,
        Err(error) => {
            return crate::reject::reject_before_dispatch(
                &state,
                "POST",
                &endpoint,
                &client.request_id,
                Some(&auth.entry.id),
                started,
                crate::reject::Envelope::OpenAi,
                crate::error::proxy_error_from_json_rejection(
                    error,
                    state.request_body_limit_for(&endpoint),
                ),
            )
        }
    };
    let snapshot = state.snapshot.load();
    let mut audit = Audit::default();
    let result = async {
        let pointer = model_pointer(&body, action);
        let wanted = body
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| openai::model_hint(None, &client))
            .ok_or_else(|| ProxyError::InvalidRequest(format!("{pointer} is required")))?;
        let purpose = action.purpose(&body);
        let (target, origin) = openai::resolve(&state, &snapshot, &auth, &client, &wanted, KIND)?;
        audit.resolved(&target);
        let reservation = crate::quota::enforce(
            &state,
            &snapshot,
            &auth,
            Some(&crate::quota::ModelRateLimit::from_model(
                &target.model_entry.value.display_name,
                &target.model_entry.id,
                &target.model_entry.value,
            )),
        )
        .await?;
        let original = serde_json::to_vec(&body).map_err(|_| openai::dependency())?;
        if let Some(masked) = audit
            .input(&state, &snapshot, &auth, &target, &original, true)
            .await?
        {
            body = serde_json::from_slice(&masked).map_err(|_| openai::dependency())?;
        }
        // 临时凭据必须由支持实时生命周期的后端托管，不能先消费供应商 mint 再发现缺实现。
        let registry = openai::registry(&state)?;
        if registry.live_usage_authority() == LiveUsageAuthority::Unsupported {
            return Err(openai::dependency());
        }
        // 本地默认保留调用方模型别名，重连时仍走相同本地准入；只改供应商请求的实际模型槽。
        let mut configuration = body.get("session").cloned().unwrap_or(Value::Null);
        // model 在创建时的合法位置改为已授权的上游模型，其他原生字段保留。
        let slot = body
            .pointer_mut(pointer)
            .ok_or_else(|| ProxyError::InvalidRequest(format!("{pointer} is required")))?;
        *slot = Value::String(
            crate::dispatch::require_upstream_model(&target.model_entry.value)?.to_owned(),
        );
        let registration = openai::begin(
            &state,
            &auth,
            &target,
            origin,
            KIND,
            None,
            &client,
            "realtime_credential",
        )
        .await?;
        audit.registered(&registration);
        let reply = openai::send_for_kind(
            &state,
            &target,
            &client,
            KIND,
            Method::POST,
            action.path(),
            "",
            Body::Json(Bytes::from(
                serde_json::to_vec(&body).map_err(|_| openai::dependency())?,
            )),
        )
        .await?;
        let mut envelope: Value =
            serde_json::from_slice(&reply.bytes).map_err(|_| openai::invalid_identity())?;
        // GA client_secret_create_response 的凭据位于顶层，供应商值从不发给调用方。
        let credential = Some(&envelope)
            .filter(|value| value.get("value").and_then(Value::as_str).is_some())
            .ok_or_else(openai::invalid_identity)?;
        let expires_at = credential
            .get("expires_at")
            .and_then(Value::as_i64)
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .filter(|expiry| *expiry > chrono::Utc::now())
            .ok_or_else(openai::invalid_identity)?;
        let mut returned = envelope
            .get("session")
            .filter(|session| session.is_object())
            .cloned()
            .ok_or_else(openai::invalid_identity)?;
        strip_credential_slots(&mut returned);
        if let Some(session) = returned.get_mut("session") {
            strip_credential_slots(session);
        }
        strip_credential_slots(&mut configuration);
        // 仅把 SDK 定义为会话配置的响应成员补作默认；未知响应元数据保留在响应中。
        // realtime_session_create_request_param.py / beta session_create_params.py。
        if let (Some(config), Some(response)) =
            (configuration.as_object_mut(), returned.as_object())
        {
            for name in [
                "type",
                "audio",
                "include",
                "instructions",
                "max_output_tokens",
                "model",
                "output_modalities",
                "parallel_tool_calls",
                "prompt",
                "reasoning",
                "tool_choice",
                "tools",
                "tracing",
                "truncation",
                "modalities",
                "voice",
                "input_audio_format",
                "output_audio_format",
                "input_audio_transcription",
                "input_audio_noise_reduction",
                "turn_detection",
                "temperature",
                "max_response_output_tokens",
                "speed",
            ] {
                if let Some(value) = response.get(name) {
                    config
                        .entry(name.to_owned())
                        .or_insert_with(|| value.clone());
                }
            }
        }
        let defaults = serde_json::json!({"session":configuration,"legacy":false});
        let mut random = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut random)
            .map_err(|_| openai::dependency())?;
        let token = format!(
            "{PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
        );
        // 配置扫描不含凭据；新 capability 也不发送给远端 guardrail。
        audit
            .output(
                &state,
                &auth,
                &target,
                &serde_json::to_vec(&defaults).map_err(|_| openai::dependency())?,
            )
            .await?;
        let api_key_id = openai::api_key_id(&auth)?;
        let stored = registry
            .mint_credential(MintCredentialRequest {
                operation_id: registration.operation_id,
                api_key_id,
                token_hash: aisix_core::ApiKey::hash_bearer(&token),
                purpose,
                expires_at,
                defaults,
            })
            .await
            .map_err(openai::registry_error)?;
        if stored.state != "credential"
            || !stored.credential.as_ref().is_some_and(|credential| {
                credential.usable_at(purpose, chrono::Utc::now())
                    && credential.api_key_id == api_key_id
                    && credential.expires_at == Some(expires_at)
            })
        {
            return Err(openai::dependency());
        }
        envelope["value"] = Value::String(token);
        drop(reservation);
        Ok((
            openai::raw_body(
                reply.status,
                &reply.headers,
                Bytes::from(serde_json::to_vec(&envelope).map_err(|_| openai::dependency())?),
            ),
            target,
        ))
    }
    .await;
    openai::finish(
        &state,
        &snapshot,
        crate::operation::REALTIME,
        Method::POST,
        &endpoint,
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

pub(crate) fn is_credential(token: &str) -> bool {
    token.starts_with(PREFIX)
}

pub(crate) fn initial_event(credential: &NativeCredential) -> Result<Value, ProxyError> {
    let mut session = credential
        .defaults
        .get("session")
        .filter(|session| session.is_object())
        .cloned()
        .ok_or_else(openai::dependency)?;
    let fields = session.as_object_mut().ok_or_else(openai::dependency)?;
    // model 只在连接 URL 指定；官方 session.update 明确不能更新它。
    for name in ["id", "object", "expires_at", "model", "client_secret"] {
        fields.remove(name);
    }
    // Translation 的更新只携带 audio；创建响应的 type 不能作为更新请求字段重放。
    if credential.purpose == RealtimePurpose::Translation {
        fields.remove("type");
    }
    let legacy_transcription = credential.purpose == RealtimePurpose::Transcription
        && credential.defaults.get("legacy").and_then(Value::as_bool) == Some(true);
    Ok(serde_json::json!({
        "type":if legacy_transcription {"transcription_session.update"} else {"session.update"},
        "session":session
    }))
}

pub(crate) async fn redeem(
    state: &ProxyState,
    token: &str,
    purpose: RealtimePurpose,
    source_ip: &str,
) -> Result<(AuthenticatedKey, NativeCredential), ProxyError> {
    let registry = openai::registry(state).map_err(|_| ProxyError::InvalidApiKey)?;
    let reply = registry
        .redeem_credential(RedeemCredentialRequest {
            token_hash: aisix_core::ApiKey::hash_bearer(token),
            purpose,
            client_ip: source_ip
                .parse::<std::net::IpAddr>()
                .ok()
                .map(|ip| ip.to_string()),
        })
        .await
        .map_err(|error| match error {
            NativeRegistryError::Unavailable => openai::dependency(),
            NativeRegistryError::NotFound
            | NativeRegistryError::Conflict
            | NativeRegistryError::InvalidRequest => ProxyError::InvalidApiKey,
        })?;
    let credential = reply
        .credential
        .filter(|credential| {
            reply.state == "credential" && credential.usable_at(purpose, chrono::Utc::now())
        })
        .ok_or(ProxyError::InvalidApiKey)?;
    let snapshot = state.snapshot.load();
    let mut matching = snapshot.apikeys.entries().into_iter().filter(|entry| {
        registry.resource_id("apikeys", &entry.id).ok() == Some(credential.api_key_id)
    });
    let entry = matching.next().ok_or(ProxyError::InvalidApiKey)?;
    if matching.next().is_some()
        || entry.value.disabled
        || entry.value.is_expired_at(chrono::Utc::now())
    {
        return Err(ProxyError::InvalidApiKey);
    }
    crate::endpoint_acl::check_endpoint(
        &entry.value,
        aisix_core::models::EndpointFamily::Realtime,
    )?;
    let auth = AuthenticatedKey::new(&snapshot, entry, None, false);
    crate::attribution::note_authenticated(&auth);
    state.metrics.record_auth_decision("api_key", true, "");
    Ok((auth, credential))
}
