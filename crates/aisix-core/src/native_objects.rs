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

/// 文件引用借用实际消费接口的权限，不授予独立文件管理权限。
pub fn native_file_consumer_allowed(protocol: NativeProtocol, family: EndpointFamily) -> bool {
    match protocol {
        NativeProtocol::Openai => matches!(
            family,
            EndpointFamily::ChatCompletions | EndpointFamily::Responses | EndpointFamily::Batches
        ),
        NativeProtocol::Anthropic => matches!(
            family,
            EndpointFamily::Messages | EndpointFamily::CountTokens | EndpointFamily::Batches
        ),
    }
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
        }
    }

    pub fn endpoint_family(self) -> EndpointFamily {
        match self {
            Self::Response | Self::ResponseItem | Self::ResponseContainer => {
                EndpointFamily::Responses
            }
            Self::File => EndpointFamily::Files,
            Self::MessageBatch => EndpointFamily::Batches,
            Self::ChatCompletion => EndpointFamily::ChatCompletions,
            Self::Upload => EndpointFamily::Files,
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
    let (adapter, surface, default_base) = match protocol {
        NativeProtocol::Openai => {
            let surface = key.apis.as_ref().and_then(|apis| match kind {
                NativeObjectKind::Response
                | NativeObjectKind::ResponseItem
                | NativeObjectKind::ResponseContainer => apis.responses.as_ref(),
                _ => None,
            });
            if matches!(
                kind,
                NativeObjectKind::Response
                    | NativeObjectKind::ResponseItem
                    | NativeObjectKind::ResponseContainer
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
                ) && surface.is_some())
            {
                return Err(NativeOriginError);
            }
            ("openai", surface, "https://api.openai.com/v1")
        }
        NativeProtocol::Anthropic => {
            if !matches!(
                kind,
                NativeObjectKind::File | NativeObjectKind::MessageBatch
            ) {
                return Err(NativeOriginError);
            }
            let surface = (kind == NativeObjectKind::MessageBatch)
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
        return Ok(native_origin_digest(
            platform_id_key,
            provider_key_id,
            protocol,
            adapter,
            &endpoint,
            &key.api_key,
            &headers,
        ));
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
    Ok(native_origin_digest(
        platform_id_key,
        provider_key_id,
        protocol,
        adapter,
        endpoint,
        &key.api_key,
        &headers,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginBinding {
    pub model_id: Uuid,
    pub provider_key_id: Uuid,
    pub boundary: OriginFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeEntry {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeginRequest {
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
}

impl NativeReply {
    pub fn new(state: &str) -> Self {
        Self {
            state: state.to_string(),
            operation_id: None,
            origin: None,
            record: None,
            records: Vec::new(),
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
