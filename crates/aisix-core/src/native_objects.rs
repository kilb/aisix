//! 托管原生对象的内部归属契约；不属于用户资源或公开原生协议。

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use ring::hmac;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::models::{Adapter, EndpointFamily, ProviderKey};
use crate::usage_io::{TextMeasurement, UsageIoAudit};

/// 平台密钥按配置文本派生；控制面和网关必须使用相同规则，不能先解码十六进制。
pub fn platform_id_key_from_secret(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeProtocol {
    Openai,
    Anthropic,
}

/// 消费只借用实际入口权限；不授予该对象的管理权限。
pub fn native_object_consumer_allowed(
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    family: EndpointFamily,
) -> bool {
    use EndpointFamily as F;
    use NativeObjectKind as K;
    match (protocol, kind) {
        (NativeProtocol::Openai, K::File) => matches!(
            family,
            F::ChatCompletions
                | F::Responses
                | F::Batches
                | F::Containers
                | F::VectorStores
                | F::FineTuning
                | F::Evals
                | F::Videos
                | F::Conversations
        ),
        (NativeProtocol::Anthropic, K::File) => {
            matches!(family, F::Messages | F::CountTokens | F::Batches)
        }
        (NativeProtocol::Openai, K::ResponseItem | K::Conversation) => {
            matches!(family, F::Responses | F::Conversations)
        }
        (NativeProtocol::Openai, K::ResponseContainer) => matches!(
            family,
            F::Responses | F::Containers | F::Conversations | F::Evals
        ),
        (NativeProtocol::Openai, K::VectorStore) => matches!(
            family,
            F::Responses | F::VectorStores | F::Conversations | F::Evals
        ),
        (NativeProtocol::Openai, K::Skill) => {
            matches!(family, F::Responses | F::Containers | F::Conversations)
        }
        (NativeProtocol::Anthropic, K::Skill | K::MessageContainer) => family == F::Messages,
        (NativeProtocol::Openai, K::Voice) => {
            matches!(family, F::Audio | F::ChatCompletions | F::Realtime)
        }
        (NativeProtocol::Openai, K::VoiceConsent) => family == F::Audio,
        (NativeProtocol::Openai, K::Video | K::VideoCharacter) => family == F::Videos,
        (NativeProtocol::Openai, K::Response) => matches!(family, F::Responses | F::Evals),
        _ => false,
    }
}

pub fn native_file_consumer_allowed(protocol: NativeProtocol, family: EndpointFamily) -> bool {
    native_object_consumer_allowed(protocol, NativeObjectKind::File, family)
}

/// 推理生命周期独立于 HTTP 请求完成；管理调用和受理不代表推理成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceStage {
    Attempt,
    Accepted,
    Management,
    Terminal,
}

impl NativeProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeObjectKind {
    Response,
    ResponseItem,
    ResponseContainer,
    File,
    MessageBatch,
    ChatCompletion,
    Upload,
    Conversation,
    ContainerFile,
    VectorStore,
    VectorStoreFileBatch,
    Skill,
    Voice,
    VoiceConsent,
    MessageContainer,
    Eval,
    EvalRun,
    EvalOutputItem,
    FineTuningJob,
    FineTuningCheckpoint,
    FineTunedModel,
    Video,
    VideoCharacter,
    RealtimeCall,
}

impl NativeObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::ResponseItem => "response_item",
            Self::ResponseContainer => "response_container",
            Self::File => "file",
            Self::MessageBatch => "message_batch",
            Self::ChatCompletion => "chat_completion",
            Self::Upload => "upload",
            Self::Conversation => "conversation",
            Self::ContainerFile => "container_file",
            Self::VectorStore => "vector_store",
            Self::VectorStoreFileBatch => "vector_store_file_batch",
            Self::Skill => "skill",
            Self::Voice => "voice",
            Self::VoiceConsent => "voice_consent",
            Self::MessageContainer => "message_container",
            Self::Eval => "eval",
            Self::EvalRun => "eval_run",
            Self::EvalOutputItem => "eval_output_item",
            Self::FineTuningJob => "fine_tuning_job",
            Self::FineTuningCheckpoint => "fine_tuning_checkpoint",
            Self::FineTunedModel => "fine_tuned_model",
            Self::Video => "video",
            Self::VideoCharacter => "video_character",
            Self::RealtimeCall => "realtime_call",
        }
    }

    pub fn endpoint_family(self) -> EndpointFamily {
        match self {
            Self::Response | Self::ResponseItem => EndpointFamily::Responses,
            Self::File => EndpointFamily::Files,
            Self::MessageBatch => EndpointFamily::Batches,
            Self::ChatCompletion => EndpointFamily::ChatCompletions,
            Self::Upload => EndpointFamily::Files,
            Self::Conversation => EndpointFamily::Conversations,
            Self::ResponseContainer | Self::ContainerFile => EndpointFamily::Containers,
            Self::VectorStore | Self::VectorStoreFileBatch => EndpointFamily::VectorStores,
            Self::Skill => EndpointFamily::Skills,
            Self::Voice | Self::VoiceConsent => EndpointFamily::Audio,
            Self::MessageContainer => EndpointFamily::Messages,
            Self::Eval | Self::EvalRun | Self::EvalOutputItem => EndpointFamily::Evals,
            Self::FineTuningJob | Self::FineTuningCheckpoint | Self::FineTunedModel => {
                EndpointFamily::FineTuning
            }
            Self::Video | Self::VideoCharacter => EndpointFamily::Videos,
            Self::RealtimeCall => EndpointFamily::Realtime,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OriginFingerprint(String);

impl fmt::Debug for OriginFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OriginFingerprint(<redacted>)")
    }
}

impl OriginFingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 内部摘要输入有明确边界；摘要及其密钥都不得写入日志。
pub fn native_origin_digest(
    platform_id_key: &[u8; 32],
    provider_key_id: Uuid,
    protocol: NativeProtocol,
    effective_adapter: &str,
    canonical_endpoint: &str,
    credential_material: &str,
    account_headers: &BTreeMap<String, String>,
) -> OriginFingerprint {
    let key = hmac::Key::new(hmac::HMAC_SHA256, platform_id_key);
    let mut context = hmac::Context::with_key(&key);
    context.update(b"aisix-native-origin-v1\0");
    let provider_key_id = provider_key_id.to_string();
    for field in [
        provider_key_id.as_str(),
        protocol.as_str(),
        effective_adapter,
        canonical_endpoint,
        credential_material,
    ] {
        context.update(&(field.len() as u64).to_be_bytes());
        context.update(field.as_bytes());
    }
    context.update(&(account_headers.len() as u64).to_be_bytes());
    for (name, value) in account_headers {
        for field in [name.as_str(), value.as_str()] {
            context.update(&(field.len() as u64).to_be_bytes());
            context.update(field.as_bytes());
        }
    }
    OriginFingerprint(format!("v1:{}", hex::encode(context.sign().as_ref())))
}

#[derive(Debug, thiserror::Error)]
#[error("the native account boundary cannot be established")]
pub struct NativeOriginError;

/// 与聊天桥共用原始字符串规则；不替自定义主机添加版本前缀。
pub fn strip_openai_chat_endpoint_suffix(base: &str) -> &str {
    let trimmed = base.trim_end_matches('/');
    for suffix in [
        "/chat/completions",
        "/completions",
        "/embeddings",
        "/images/generations",
        "/audio/transcriptions",
        "/audio/translations",
        "/audio/speech",
    ] {
        if let Some(rest) = trimmed.strip_suffix(suffix) {
            return rest.trim_end_matches('/');
        }
    }
    trimmed
}

/// 只补全两个既有规范主机字符串；大小写、用户信息、端口和查询保持桥的原义。
pub fn normalize_openai_chat_base(base: &str) -> String {
    let stripped = strip_openai_chat_endpoint_suffix(base);
    if matches!(stripped, "https://api.openai.com" | "http://api.openai.com") {
        format!("{stripped}/v1")
    } else {
        stripped.to_owned()
    }
}

/// 控制面和执行器共用配置边界；不接受调用方提供的凭据或账号头。
pub fn configured_native_origin(
    platform_id_key: &[u8; 32],
    provider_key_id: Uuid,
    key: &ProviderKey,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
) -> Result<OriginFingerprint, NativeOriginError> {
    let (adapter, endpoint, headers) = configured_native_account(key, protocol, kind)?;
    Ok(native_origin_digest(
        platform_id_key,
        provider_key_id,
        protocol,
        adapter,
        &endpoint,
        &key.api_key,
        &headers,
    ))
}

/// 返回与账号指纹完全相同的规范基址；实际派发必须使用此值。
pub fn configured_native_endpoint(
    key: &ProviderKey,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
) -> Result<String, NativeOriginError> {
    configured_native_account(key, protocol, kind).map(|(_, endpoint, _)| endpoint)
}

fn configured_native_account(
    key: &ProviderKey,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
) -> Result<(&'static str, String, BTreeMap<String, String>), NativeOriginError> {
    let (adapter, surface, default_base) = match protocol {
        NativeProtocol::Openai => {
            let surface = key.apis.as_ref().and_then(|apis| match kind {
                NativeObjectKind::Response
                | NativeObjectKind::ResponseItem
                | NativeObjectKind::ResponseContainer
                | NativeObjectKind::ContainerFile => apis.responses.as_ref(),
                _ => None,
            });
            if matches!(
                kind,
                NativeObjectKind::Response | NativeObjectKind::ResponseItem
            ) && key.apis.is_some()
                && surface.is_none()
            {
                return Err(NativeOriginError);
            }
            if key.provider != "openai"
                && key.adapter != Some(Adapter::Openai)
                && !(matches!(
                    kind,
                    NativeObjectKind::Response
                        | NativeObjectKind::ResponseItem
                        | NativeObjectKind::ResponseContainer
                        | NativeObjectKind::ContainerFile
                ) && surface.is_some())
            {
                return Err(NativeOriginError);
            }
            ("openai", surface, "https://api.openai.com/v1")
        }
        NativeProtocol::Anthropic => {
            if !matches!(
                kind,
                NativeObjectKind::File
                    | NativeObjectKind::MessageBatch
                    | NativeObjectKind::Skill
                    | NativeObjectKind::MessageContainer
            ) {
                return Err(NativeOriginError);
            }
            let surface = matches!(
                kind,
                NativeObjectKind::MessageBatch
                    | NativeObjectKind::Skill
                    | NativeObjectKind::MessageContainer
            )
            .then(|| key.apis.as_ref().and_then(|a| a.messages.as_ref()))
            .flatten();
            if key.provider != "anthropic"
                && key.adapter != Some(Adapter::Anthropic)
                && surface.is_none()
            {
                return Err(NativeOriginError);
            }
            ("anthropic", surface, "https://api.anthropic.com")
        }
    };
    let mut headers = BTreeMap::new();
    if let Some(request) = &key.request {
        // 可变头可能替换凭据或账号，不能把其归属冒充为配置账号。
        if !request.forward_client_headers.is_empty() {
            return Err(NativeOriginError);
        }
        for (name, value) in &request.default_headers {
            let name = name.to_ascii_lowercase();
            if value.contains("${") {
                return Err(NativeOriginError);
            }
            // 只忽略该协议已经填写的凭据槽；其他凭据默认头仍可能选择不同账号。
            if matches!(
                (protocol, name.as_str()),
                (NativeProtocol::Openai, "authorization")
                    | (NativeProtocol::Anthropic, "x-api-key")
            ) {
                continue;
            }
            if headers.insert(name, value.clone()).is_some() {
                return Err(NativeOriginError);
            }
        }
    }
    let raw_base = surface
        .and_then(|s| s.base.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            key.api_base
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });
    let chat_default = protocol == NativeProtocol::Openai
        && kind == NativeObjectKind::ChatCompletion
        && (key.provider.trim().is_empty() || key.provider.trim().eq_ignore_ascii_case("openai"));
    if raw_base.is_none() && key.provider != protocol.as_str() && !chat_default {
        return Err(NativeOriginError);
    }
    if protocol == NativeProtocol::Openai && kind == NativeObjectKind::ChatCompletion {
        // 该兼容接口不支持存储；普通聊天移除 store，不进入这个归属分支。
        if key.provider.trim().eq_ignore_ascii_case("cohere") {
            return Err(NativeOriginError);
        }
        let endpoint = normalize_openai_chat_base(raw_base.unwrap_or(default_base));
        let parsed = url::Url::parse(&endpoint).map_err(|_| NativeOriginError)?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(NativeOriginError);
        }
        return Ok((adapter, endpoint, headers));
    }
    let mut base =
        url::Url::parse(raw_base.unwrap_or(default_base)).map_err(|_| NativeOriginError)?;
    if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
        return Err(NativeOriginError);
    }
    // 与原生协议的基址约定一致，路径只剥离已知协议端点。
    let mut path = base.path().trim_end_matches('/').to_string();
    let suffixes: &[&str] = match protocol {
        NativeProtocol::Openai => &["/responses", "/chat/completions", "/completions"],
        NativeProtocol::Anthropic => &["/v1/messages", "/v1"],
    };
    for suffix in suffixes {
        if let Some(stripped) = path.strip_suffix(suffix) {
            path = stripped.to_string();
            break;
        }
    }
    if protocol == NativeProtocol::Openai && path.is_empty() {
        path = "/v1".to_string();
    }
    base.set_path(&path);
    let endpoint = if base.path() == "/" && base.query().is_none() && base.fragment().is_none() {
        base.as_str().strip_suffix('/').unwrap_or(base.as_str())
    } else {
        base.as_str()
    };
    Ok((adapter, endpoint.to_string(), headers))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeOriginSurface {
    Objects,
    Responses,
    Messages,
    ChatCompletions,
}

pub fn native_origin_surface(
    protocol: NativeProtocol,
    kind: NativeObjectKind,
) -> NativeOriginSurface {
    match (protocol, kind) {
        (
            NativeProtocol::Openai,
            NativeObjectKind::Response
            | NativeObjectKind::ResponseItem
            | NativeObjectKind::ResponseContainer
            | NativeObjectKind::ContainerFile,
        ) => NativeOriginSurface::Responses,
        (NativeProtocol::Openai, NativeObjectKind::ChatCompletion) => {
            NativeOriginSurface::ChatCompletions
        }
        (
            NativeProtocol::Anthropic,
            NativeObjectKind::MessageBatch
            | NativeObjectKind::Skill
            | NativeObjectKind::MessageContainer,
        ) => NativeOriginSurface::Messages,
        _ => NativeOriginSurface::Objects,
    }
}

pub fn configured_native_origin_at(
    platform_id_key: &[u8; 32],
    provider_key_id: Uuid,
    key: &ProviderKey,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    surface: Option<NativeOriginSurface>,
) -> Result<OriginFingerprint, NativeOriginError> {
    let representative = surface_kind(protocol, kind, surface)?;
    configured_native_origin(
        platform_id_key,
        provider_key_id,
        key,
        protocol,
        representative,
    )
}

pub fn configured_native_endpoint_at(
    key: &ProviderKey,
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    surface: Option<NativeOriginSurface>,
) -> Result<String, NativeOriginError> {
    configured_native_endpoint(key, protocol, surface_kind(protocol, kind, surface)?)
}

fn surface_kind(
    protocol: NativeProtocol,
    kind: NativeObjectKind,
    surface: Option<NativeOriginSurface>,
) -> Result<NativeObjectKind, NativeOriginError> {
    match surface {
        None => Ok(kind),
        Some(surface) if surface == native_origin_surface(protocol, kind) => Ok(kind),
        Some(NativeOriginSurface::Objects) => Ok(NativeObjectKind::File),
        Some(NativeOriginSurface::Responses) if protocol == NativeProtocol::Openai => Ok(
            if matches!(
                kind,
                NativeObjectKind::Response | NativeObjectKind::ResponseItem
            ) {
                NativeObjectKind::Response
            } else {
                NativeObjectKind::ResponseContainer
            },
        ),
        Some(NativeOriginSurface::Messages) if protocol == NativeProtocol::Anthropic => {
            Ok(NativeObjectKind::MessageBatch)
        }
        Some(NativeOriginSurface::ChatCompletions) if protocol == NativeProtocol::Openai => {
            Ok(NativeObjectKind::ChatCompletion)
        }
        _ => Err(NativeOriginError),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginBinding {
    /// 仅供重建已选中的基址；不进入账号 HMAC，也不作为另一项授权。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<NativeOriginSurface>,
    pub model_id: Uuid,
    pub provider_key_id: Uuid,
    pub boundary: OriginFingerprint,
}

impl PartialEq for OriginBinding {
    fn eq(&self, other: &Self) -> bool {
        self.model_id == other.model_id
            && self.provider_key_id == other.provider_key_id
            && self.boundary == other.boundary
    }
}
impl Eq for OriginBinding {}

/// 准入时的纯观测值；不参与授权、来源证明或账务事实。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NativeInputAudit {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub applied_guardrails: Vec<crate::AppliedGuardrail>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub guardrail_bypassed_reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guardrail_monitor_hits: Vec<crate::GuardrailMonitorHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guardrail_enforced_hits: Vec<crate::GuardrailEnforcedHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guardrail_scores: Vec<crate::GuardrailScore>,
}

impl NativeInputAudit {
    pub fn is_empty(&self) -> bool {
        self.applied_guardrails.is_empty()
            && self.guardrail_bypassed_reason.is_empty()
            && self.guardrail_monitor_hits.is_empty()
            && self.guardrail_enforced_hits.is_empty()
            && self.guardrail_scores.is_empty()
    }
}

/// 请求身份的遥测投影不含租户、key 或成员标识；那些仍来自权威归属记录。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NativeUsageContext {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub api_key_name: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub jwt_subject: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub jwt_provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub jwt_claim_mapping: String,
    #[serde(flatten)]
    pub input_audit: NativeInputAudit,
}

impl NativeUsageContext {
    pub fn is_empty(&self) -> bool {
        self.api_key_name.is_empty()
            && self.tags.is_empty()
            && self.jwt_subject.is_empty()
            && self.jwt_provider.is_empty()
            && self.jwt_claim_mapping.is_empty()
            && self.input_audit.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_context: Option<Box<NativeInputAudit>>,
    /// 最终模型请求的文本标量；不持久化正文或推断未观察的传输字节。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_input_text: Option<TextMeasurement>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    pub entry_id: String,
    #[serde(default)]
    pub requested_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_service_tier: Option<String>,
    pub requested_model_id: Uuid,
    pub model_id: Uuid,
    pub upstream_model: String,
    #[serde(default)]
    pub upstream_provider: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeginRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_context: Option<Box<NativeUsageContext>>,
    /// 最终模型请求的文本标量；不持久化正文或推断未观察的传输字节。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_input_text: Option<TextMeasurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_file_id: Option<String>,
    pub operation_id: Uuid,
    pub api_key_id: Uuid,
    pub protocol: NativeProtocol,
    pub kind: NativeObjectKind,
    pub origin: OriginBinding,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    pub requested_model_id: Uuid,
    pub requested_model: String,
    pub upstream_model: String,
    #[serde(default)]
    pub upstream_provider: String,
    pub operation: String,
    pub request_id: String,
    pub asynchronous: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_service_tier: Option<String>,
    #[serde(default)]
    pub entries: Vec<NativeEntry>,
    /// 原生后台观察重放的协议头；凭据与账号头只来自已绑定的静态配置。
    #[serde(default)]
    pub observation_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeChildObject {
    pub kind: NativeObjectKind,
    pub raw_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindRequest {
    #[serde(default)]
    pub existing_record_id: Option<Uuid>,
    #[serde(default)]
    pub objects: Vec<NativeChildObject>,
    pub operation_id: Uuid,
    pub api_key_id: Uuid,
    pub raw_id: String,
    #[serde(default)]
    pub provider_created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub provider_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectLocator {
    pub protocol: NativeProtocol,
    pub kind: NativeObjectKind,
    pub raw_id: String,
    #[serde(default)]
    pub origin: Option<OriginBinding>,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LookupRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_family: Option<EndpointFamily>,
    pub api_key_id: Uuid,
    pub locator: ObjectLocator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedRequest {
    pub api_key_id: Uuid,
    pub protocol: NativeProtocol,
    pub kind: NativeObjectKind,
    pub origin: OriginBinding,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    pub raw_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TombstoneRequest {
    pub api_key_id: Uuid,
    pub record_id: Uuid,
    pub version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub limit: u32,
    #[serde(default)]
    pub record_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<NativeProtocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<NativeObjectKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseToken {
    pub record_id: Uuid,
    pub generation: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub lease: LeaseToken,
    #[serde(default)]
    pub poll_after_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_audit: Option<UsageIoAudit>,
    /// 批次每个条目独立结束，不能只用整批 ended 推断成功或取消。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_status: Option<String>,
    #[serde(default)]
    pub error_class: String,
    #[serde(default)]
    pub error_message: String,
    #[serde(default)]
    pub entry_id: String,
    pub occurred_at: DateTime<Utc>,
    pub status_code: u16,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    pub cached_prompt_tokens: u32,
    /// 上游明确报告缓存计数时为 true，未报告时为 false；缺省兼容旧执行器。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_usage_reported: Option<bool>,
    #[serde(default)]
    pub cache_write_tokens: Option<u32>,
    #[serde(default)]
    pub cache_creation_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_creation_5m_tokens: Option<u32>,
    #[serde(default)]
    pub cache_creation_1h_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub prompt_audio_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_image_tokens: Option<u32>,
    #[serde(default)]
    pub completion_audio_tokens: Option<u32>,
    #[serde(default)]
    pub completion_image_tokens: Option<u32>,
    #[serde(default)]
    pub web_search_requests: u32,
    #[serde(default)]
    pub usage_estimated: bool,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub requested_service_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRequest {
    #[serde(default)]
    pub output_file_ids: Vec<String>,
    pub lease: LeaseToken,
    pub terminal_status: String,
    pub usage: Vec<TerminalUsage>,
}

/// 根创建与引用准入在一个事务；被引用对象不能仅存在于 handler 内存。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedReference {
    pub record_id: Uuid,
    pub consumer_family: EndpointFamily,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeginRelatedRequest {
    #[serde(default)]
    pub lifecycle_context: Option<crate::native_metering::LifecycleContext>,
    pub submission: BeginRequest,
    #[serde(default)]
    pub references: Vec<AcceptedReference>,
    /// 每个已受理 Responses 批次条目只借用自己准入的存储引用。
    #[serde(default)]
    pub entry_references: BTreeMap<String, Vec<AcceptedReference>>,
    /// 仅允许 submission.parent_id 指定的已授权父对象；新产物继承父的 accepted owner。
    #[serde(default)]
    pub owner_parent_record_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ArtifactSelector {
    Root,
    Record {
        record_id: Uuid,
    },
    Observed {
        kind: NativeObjectKind,
        raw_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundRelation {
    pub parent: ArtifactSelector,
    pub child: ArtifactSelector,
    pub relation: NativeRelation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindRelatedRequest {
    pub binding: BindRequest,
    #[serde(default)]
    pub relations: Vec<BoundRelation>,
}

/// 关系是集合成员身份，不改变子对象的不可变来源和所有者。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeRelation {
    ConversationItem,
    ContainerFile,
    VectorStoreFile,
    VectorStoreBatch,
    VectorStoreBatchFile,
    ResponseOutput,
    EvalRun,
    EvalOutput,
    FineTuningCheckpoint,
    FineTuningResult,
    FineTunedModel,
    VideoSource,
    VideoCharacter,
}

impl NativeRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConversationItem => "conversation_item",
            Self::ContainerFile => "container_file",
            Self::VectorStoreFile => "vector_store_file",
            Self::VectorStoreBatch => "vector_store_batch",
            Self::VectorStoreBatchFile => "vector_store_batch_file",
            Self::ResponseOutput => "response_output",
            Self::EvalRun => "eval_run",
            Self::EvalOutput => "eval_output",
            Self::FineTuningCheckpoint => "fine_tuning_checkpoint",
            Self::FineTuningResult => "fine_tuning_result",
            Self::FineTunedModel => "fine_tuned_model",
            Self::VideoSource => "video_source",
            Self::VideoCharacter => "video_character",
        }
    }

    pub fn permits(
        self,
        protocol: NativeProtocol,
        parent: NativeObjectKind,
        child: NativeObjectKind,
    ) -> bool {
        use NativeObjectKind as K;
        protocol == NativeProtocol::Openai
            && match self {
                Self::ConversationItem => parent == K::Conversation && child == K::ResponseItem,
                Self::ContainerFile => parent == K::ResponseContainer && child == K::ContainerFile,
                Self::VectorStoreFile => parent == K::VectorStore && child == K::File,
                Self::VectorStoreBatch => {
                    parent == K::VectorStore && child == K::VectorStoreFileBatch
                }
                Self::VectorStoreBatchFile => parent == K::VectorStoreFileBatch && child == K::File,
                Self::ResponseOutput => {
                    parent == K::Response && matches!(child, K::ResponseItem | K::ResponseContainer)
                }
                Self::EvalRun => parent == K::Eval && child == K::EvalRun,
                Self::EvalOutput => parent == K::EvalRun && child == K::EvalOutputItem,
                Self::FineTuningCheckpoint => {
                    parent == K::FineTuningJob && child == K::FineTuningCheckpoint
                }
                Self::FineTuningResult => parent == K::FineTuningJob && child == K::File,
                Self::FineTunedModel => {
                    matches!(parent, K::FineTuningJob | K::FineTuningCheckpoint)
                        && child == K::FineTunedModel
                }
                Self::VideoSource => parent == K::Video && matches!(child, K::Video | K::File),
                Self::VideoCharacter => parent == K::Video && child == K::VideoCharacter,
            }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationAction {
    Attach,
    Detach,
    Discover,
}

/// 必须在首个上游副作用前保留；operation_id 是内部幂等身份，不是客户端对象 ID。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveRelationsRequest {
    pub api_key_id: Uuid,
    pub operation_id: Uuid,
    pub parent_record_id: Uuid,
    pub expected_parent_version: i64,
    pub relation: NativeRelation,
    pub action: RelationAction,
    #[serde(default)]
    pub child_record_ids: Vec<Uuid>,
    #[serde(default)]
    pub references: Vec<AcceptedReference>,
}

/// objects 只能来自已准入父端点的官方对象槽；已有引用须在 reserve 中逐项准入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitRelationsRequest {
    pub api_key_id: Uuid,
    pub operation_id: Uuid,
    #[serde(default)]
    pub objects: Vec<NativeChildObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectDeletionRequest {
    pub api_key_id: Uuid,
    pub operation_id: Uuid,
    pub record_id: Uuid,
    pub version: i64,
}

/// 明确上游拒绝才能释放写入栅栏；不确定结果保留到受控对账完成。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveReservationRequest {
    pub api_key_id: Uuid,
    pub operation_id: Uuid,
    pub upstream_rejected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LookupRelationsRequest {
    pub api_key_id: Uuid,
    pub parent_record_id: Uuid,
    pub relation: NativeRelation,
    #[serde(default)]
    pub raw_ids: Vec<String>,
    #[serde(default)]
    pub consumer_family: Option<EndpointFamily>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeMembership {
    pub membership_id: Uuid,
    pub parent_record_id: Uuid,
    pub child_record_id: Uuid,
    pub relation: NativeRelation,
    pub generation: i64,
    pub version: i64,
}

/// 后台产物按 accepted entry 派生；不能借当前 worker 身份重写原 caller。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedArtifactEntry {
    pub entry_id: String,
    pub root: NativeChildObject,
    #[serde(default)]
    pub relations: Vec<BoundRelation>,
    #[serde(default)]
    pub objects: Vec<NativeChildObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindObservedArtifactsRequest {
    pub lease: LeaseToken,
    pub entries: Vec<ObservedArtifactEntry>,
    /// 提供时，所有产物、终态和 outbox 在同一事务提交。
    #[serde(default)]
    pub complete: Option<CompleteRequest>,
}

// 控制面响应宽松解码；执行器必须检查决策所需字段，不得用缺省值授权。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeRecord {
    /// 最终模型请求的文本标量；不持久化正文或推断未观察的传输字节。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_input_text: Option<TextMeasurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_file_id: Option<String>,
    pub record_id: Uuid,
    #[serde(default)]
    pub api_key_id: Option<Uuid>,
    #[serde(default)]
    pub entries: Vec<NativeEntry>,
    #[serde(default)]
    pub observation_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub operation_id: Option<Uuid>,
    #[serde(default)]
    pub requested_model_id: Option<Uuid>,
    #[serde(default)]
    pub requested_model: String,
    #[serde(default)]
    pub upstream_model: String,
    #[serde(default)]
    pub upstream_provider: String,
    #[serde(default)]
    pub operation: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub requested_service_tier: Option<String>,
    #[serde(default)]
    pub origin: Option<OriginBinding>,
    #[serde(default)]
    pub protocol: Option<NativeProtocol>,
    #[serde(default)]
    pub kind: Option<NativeObjectKind>,
    #[serde(default)]
    pub raw_id: String,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    #[serde(default)]
    pub parent_raw_id: Option<String>,
    #[serde(default)]
    pub lifecycle_context: Option<crate::native_metering::LifecycleContext>,
    #[serde(default)]
    pub metering_state: Option<String>,
    #[serde(default)]
    pub reconciliation_reason: Option<String>,
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub lease_generation: Option<i64>,
    #[serde(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeReply {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<OriginBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<NativeRecord>,
    #[serde(default)]
    pub records: Vec<NativeRecord>,
    #[serde(default)]
    pub memberships: Vec<NativeMembership>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metering_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciliation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<crate::native_realtime::NativeCredential>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_session: Option<crate::native_realtime::LiveSessionRecord>,
    #[serde(default)]
    pub live_sessions: Vec<crate::native_realtime::LiveSessionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appended: Option<bool>,
}

impl NativeReply {
    pub fn new(state: &str) -> Self {
        Self {
            state: state.to_string(),
            operation_id: None,
            origin: None,
            record: None,
            records: Vec::new(),
            memberships: Vec::new(),
            metering_state: None,
            reconciliation_reason: None,
            credential: None,
            live_session: None,
            live_sessions: Vec::new(),
            appended: None,
        }
    }
}

#[cfg(test)]
mod platform_key_tests {
    use super::platform_id_key_from_secret;

    // 固定 SHA-256 向量约束既有网关派生，避免统一规则时改变已签发标识符的密钥。
    #[test]
    fn platform_identity_keeps_the_existing_text_sha256_derivation() {
        assert_eq!(
            platform_id_key_from_secret("abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }
}

#[cfg(test)]
mod chat_origin_tests {
    use super::*;

    #[test]
    fn chat_base_keeps_the_bridge_exact_string_and_endpoint_suffix_rules() {
        for (given, expected) in [
            ("https://proxy.example/", "https://proxy.example"),
            (
                "https://proxy.example/chat/completions/",
                "https://proxy.example",
            ),
            (
                "https://proxy.example/v2/embeddings",
                "https://proxy.example/v2",
            ),
            (
                "https://proxy.example/v2/responses",
                "https://proxy.example/v2/responses",
            ),
            ("https://api.openai.com", "https://api.openai.com/v1"),
            ("https://API.OPENAI.COM", "https://API.OPENAI.COM"),
            ("https://api.openai.com:443", "https://api.openai.com:443"),
            (
                "https://user:pass@api.openai.com",
                "https://user:pass@api.openai.com",
            ),
            (
                "https://proxy.example?route=one",
                "https://proxy.example?route=one",
            ),
        ] {
            assert_eq!(normalize_openai_chat_base(given), expected);
        }
    }

    #[test]
    fn chat_origin_distinguishes_custom_root_from_v1_without_changing_file_origin() {
        let key: ProviderKey = serde_json::from_value(serde_json::json!({
            "display_name":"native", "provider":"openai", "api_key":"test-key",
            "api_base":"https://proxy.example", "adapter":"openai"
        }))
        .unwrap();
        let mut versioned = key.clone();
        versioned.api_base = Some("https://proxy.example/v1".into());
        let id = Uuid::from_u128(1);
        let origin = |key: &ProviderKey, kind| {
            configured_native_origin(&[7; 32], id, key, NativeProtocol::Openai, kind).unwrap()
        };
        assert_ne!(
            origin(&key, NativeObjectKind::ChatCompletion),
            origin(&versioned, NativeObjectKind::ChatCompletion)
        );
        assert_eq!(
            origin(&key, NativeObjectKind::File),
            origin(&versioned, NativeObjectKind::File)
        );
        versioned.api_base = Some("https://proxy.example/chat/completions".into());
        assert_eq!(
            origin(&key, NativeObjectKind::ChatCompletion),
            origin(&versioned, NativeObjectKind::ChatCompletion)
        );
    }
}

#[cfg(test)]
mod authority_contract_tests {
    use super::*;
    use serde_json::json;

    fn split_account() -> ProviderKey {
        serde_json::from_value(json!({
            "display_name":"split-native", "provider":"openai", "api_key":"fixture-key",
            "api_base":"https://objects.example/v1",
            "apis":{"responses":{"base":"https://responses.example/v1"}}
        }))
        .unwrap()
    }

    #[test]
    fn an_inherited_item_surface_rebuilds_the_original_objects_account() {
        let provider = split_account();
        let origin = |surface| {
            configured_native_origin_at(
                &[7; 32],
                Uuid::from_u128(1),
                &provider,
                NativeProtocol::Openai,
                NativeObjectKind::ResponseItem,
                Some(surface),
            )
            .unwrap()
        };
        assert_eq!(
            configured_native_endpoint_at(
                &provider,
                NativeProtocol::Openai,
                NativeObjectKind::ResponseItem,
                Some(NativeOriginSurface::Objects)
            )
            .unwrap(),
            "https://objects.example/v1"
        );
        assert_eq!(
            configured_native_endpoint_at(
                &provider,
                NativeProtocol::Openai,
                NativeObjectKind::ResponseItem,
                Some(NativeOriginSurface::Responses)
            )
            .unwrap(),
            "https://responses.example/v1"
        );
        assert_ne!(
            origin(NativeOriginSurface::Objects),
            origin(NativeOriginSurface::Responses)
        );
    }

    #[test]
    fn adding_a_surface_discriminator_keeps_legacy_account_equality_and_hmac() {
        let provider = split_account();
        let boundary = configured_native_origin(
            &[7; 32],
            Uuid::from_u128(1),
            &provider,
            NativeProtocol::Openai,
            NativeObjectKind::Response,
        )
        .unwrap();
        let legacy:OriginBinding=serde_json::from_value(json!({"model_id":Uuid::from_u128(2),"provider_key_id":Uuid::from_u128(1),"boundary":boundary})).unwrap();
        assert_eq!(legacy.surface, None);
        let mut explicit = legacy.clone();
        explicit.surface = Some(NativeOriginSurface::Responses);
        assert_eq!(legacy, explicit);
        assert_eq!(
            configured_native_origin_at(
                &[7; 32],
                explicit.provider_key_id,
                &provider,
                NativeProtocol::Openai,
                NativeObjectKind::Response,
                explicit.surface
            )
            .unwrap(),
            legacy.boundary
        );
    }

    #[test]
    fn an_explicit_response_surface_cannot_enable_an_undeclared_native_response_api() {
        let provider:ProviderKey=serde_json::from_value(json!({"display_name":"objects", "provider":"openai", "api_key":"fixture-key", "api_base":"https://objects.example/v1", "apis":{}})).unwrap();
        assert!(configured_native_endpoint_at(
            &provider,
            NativeProtocol::Openai,
            NativeObjectKind::Response,
            Some(NativeOriginSurface::Responses)
        )
        .is_err());
        assert_eq!(
            configured_native_endpoint_at(
                &provider,
                NativeProtocol::Openai,
                NativeObjectKind::ResponseContainer,
                Some(NativeOriginSurface::Responses)
            )
            .unwrap(),
            "https://objects.example/v1"
        );
    }

    #[test]
    fn a_container_file_uses_its_containers_actual_surface() {
        let provider = split_account();
        assert_eq!(
            configured_native_endpoint(
                &provider,
                NativeProtocol::Openai,
                NativeObjectKind::ContainerFile
            )
            .unwrap(),
            "https://responses.example/v1"
        );
        assert_eq!(
            configured_native_origin(
                &[7; 32],
                Uuid::from_u128(1),
                &provider,
                NativeProtocol::Openai,
                NativeObjectKind::ContainerFile
            )
            .unwrap(),
            configured_native_origin(
                &[7; 32],
                Uuid::from_u128(1),
                &provider,
                NativeProtocol::Openai,
                NativeObjectKind::ResponseContainer
            )
            .unwrap()
        );
    }

    #[test]
    fn consumption_never_grants_management_or_cross_protocol_object_rights() {
        let key: crate::ApiKey = serde_json::from_value(
            json!({"key_hash":"fixture", "allowed_endpoints":["responses"]}),
        )
        .unwrap();
        assert!(native_object_consumer_allowed(
            NativeProtocol::Openai,
            NativeObjectKind::VectorStore,
            EndpointFamily::Responses
        ));
        assert!(key.can_call(EndpointFamily::Responses));
        assert!(!key.can_call(EndpointFamily::VectorStores));
        assert!(!native_object_consumer_allowed(
            NativeProtocol::Anthropic,
            NativeObjectKind::VectorStore,
            EndpointFamily::Responses
        ));
        assert!(!native_object_consumer_allowed(
            NativeProtocol::Openai,
            NativeObjectKind::Skill,
            EndpointFamily::Audio
        ));
        assert!(!native_object_consumer_allowed(
            NativeProtocol::Openai,
            NativeObjectKind::FineTunedModel,
            EndpointFamily::Models
        ));
    }
}

#[cfg(test)]
mod usage_context_tests {
    use super::*;
    use serde_json::json;

    fn old_entry() -> serde_json::Value {
        json!({
            "entry_id":"one", "requested_model_id":Uuid::from_u128(3),
            "model_id":Uuid::from_u128(4), "upstream_model":"original-upstream"
        })
    }

    fn old_submission() -> serde_json::Value {
        json!({
            "operation_id":Uuid::from_u128(1), "api_key_id":Uuid::from_u128(2),
            "protocol":"openai", "kind":"response",
            "origin":{"model_id":Uuid::from_u128(4),"provider_key_id":Uuid::from_u128(5),
                "boundary":"v1:0000000000000000000000000000000000000000000000000000000000000000"},
            "requested_model_id":Uuid::from_u128(3), "requested_model":"original-alias",
            "upstream_model":"original-upstream", "operation":"responses",
            "request_id":"original-request", "asynchronous":true
        })
    }

    #[test]
    fn old_submissions_and_entries_do_not_gain_empty_observation_fields() {
        let submission: BeginRequest = serde_json::from_value(old_submission()).unwrap();
        assert!(submission.usage_context.is_none());
        let wire = serde_json::to_value(submission).unwrap();
        assert!(wire.get("usage_context").is_none());
        let entry: NativeEntry = serde_json::from_value(old_entry()).unwrap();
        assert!(entry.usage_context.is_none());
        let wire = serde_json::to_value(entry).unwrap();
        assert!(wire.get("usage_context").is_none());
    }

    #[test]
    fn a_future_observation_field_cannot_become_authority_or_financial_data() {
        let mut wire = old_submission();
        wire["usage_context"] = json!({
            "api_key_name":"original-name", "future_observation":{"value":7},
            "api_key_id":Uuid::from_u128(999), "team_id":"other-tenant", "user_id":"other-member",
            "cost_usd":999, "prompt_tokens":999, "occurred_at":"2099-01-01T00:00:00Z",
            "request_headers":{"x-private":"must-stay-exporter-only"}
        });
        let submission: BeginRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(submission.api_key_id, Uuid::from_u128(2));
        let context = serde_json::to_value(submission.usage_context.unwrap()).unwrap();
        assert_eq!(context, json!({"api_key_name":"original-name"}));

        let mut wire = old_entry();
        wire["usage_context"] = json!({
            "applied_guardrails":[{"kind":"keyword","hook":"input"}],
            "api_key_name":"must-not-be-repeated-per-entry", "future_audit":true
        });
        let entry: NativeEntry = serde_json::from_value(wire).unwrap();
        let context = serde_json::to_value(entry.usage_context.unwrap()).unwrap();
        assert_eq!(
            context,
            json!({"applied_guardrails":[{"kind":"keyword","hook":"input"}]})
        );
    }

    #[test]
    fn native_record_reads_future_entry_observations_without_requiring_them() {
        let mut entry = old_entry();
        entry["usage_context"] = json!({
            "guardrail_bypassed_reason":"original-observed-tag", "future_audit":true
        });
        let record: NativeRecord = serde_json::from_value(json!({
            "record_id":Uuid::from_u128(9), "entries":[entry], "future_record_field":true
        }))
        .unwrap();
        assert_eq!(record.entries.len(), 1);
        assert_eq!(
            record.entries[0]
                .usage_context
                .as_deref()
                .unwrap()
                .guardrail_bypassed_reason,
            "original-observed-tag"
        );
    }
}
