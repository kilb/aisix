//! 实时凭据和已受理会话的持久合同；这里不承载供应商凭据。

use crate::native_metering::ObservedUsage;
use crate::native_objects::{BeginRequest, NativeEntry, OriginBinding};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimePurpose {
    Realtime,
    Transcription,
    Translation,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeTransport {
    WebSocket,
    WebRtc,
    Sip,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCoverage {
    Partial,
    Complete,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintCredentialRequest {
    pub operation_id: Uuid,
    pub api_key_id: Uuid,
    /// 仅接收随机网关凭据的 SHA-256，明文只在网关生成和响应。
    pub token_hash: String,
    pub purpose: RealtimePurpose,
    pub expires_at: DateTime<Utc>,
    /// 供应商响应中的默认会话配置，已移除所有凭据槽。
    #[serde(default)]
    pub defaults: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemCredentialRequest {
    pub token_hash: String,
    pub purpose: RealtimePurpose,
    #[serde(default)]
    pub client_ip: Option<String>,
}

/// 新控制面可扩充响应；缺少身份、用途或过期时间必须由调用方拒绝。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeCredential {
    pub credential_id: Uuid,
    #[serde(default)]
    pub api_key_id: Uuid,
    #[serde(default)]
    pub purpose: RealtimePurpose,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub defaults: serde_json::Value,
    /// 默认来源只作审计，每次建立会话仍按最终模型重新路由授权。
    #[serde(default)]
    pub origin: Option<OriginBinding>,
    #[serde(default)]
    pub requested_model_id: Uuid,
    #[serde(default)]
    pub requested_model: String,
}

impl NativeCredential {
    pub fn usable_at(&self, purpose: RealtimePurpose, now: DateTime<Utc>) -> bool {
        !self.credential_id.is_nil()
            && !self.api_key_id.is_nil()
            && purpose != RealtimePurpose::Unknown
            && self.purpose == purpose
            && self.expires_at.is_some_and(|expiry| expiry > now)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateLiveSessionRequest {
    pub operation_id: Uuid,
    pub api_key_id: Uuid,
    pub purpose: RealtimePurpose,
    pub transport: RealtimeTransport,
    #[serde(default)]
    pub credential_id: Option<Uuid>,
    #[serde(default)]
    pub provider_session_id: Option<String>,
    /// 网关明确选择的生命周期截止时间，与临时凭据过期时间互相独立。
    pub deadline_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSessionLease {
    pub operation_id: Uuid,
    pub generation: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendLiveUsageRequest {
    pub operation_id: Uuid,
    pub api_key_id: Uuid,
    /// 真实供应商事件标识，或明确属于网关观察的唯一事件标识。
    pub event_id: String,
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub usage: Vec<ObservedUsage>,
    pub coverage: UsageCoverage,
    #[serde(default)]
    pub terminal: bool,
    /// 只允许用量槽；不能传入音频、文本、请求配置或完整事件。
    #[serde(default)]
    pub raw_usage: Option<serde_json::Value>,
    #[serde(default)]
    pub lease: Option<LiveSessionLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointLiveSessionRequest {
    pub lease: LiveSessionLease,
    #[serde(default)]
    pub terminal: bool,
    pub coverage: UsageCoverage,
    /// 转发新增的独立计费模型配置前，先保存当下授权的不可变条目。
    #[serde(default)]
    pub entries: Vec<NativeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimLiveSessionsRequest {
    pub limit: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SignedSipIntakeRequest {
    pub operation_id: Uuid,
    pub route_id: String,
    pub webhook_id: String,
    pub webhook_timestamp: String,
    pub webhook_signature: String,
    /// 验签时使用逐字节正文，正文不进入持久 journal。
    pub raw_body: String,
    pub deadline_at: DateTime<Utc>,
}

impl std::fmt::Debug for SignedSipIntakeRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignedSipIntakeRequest")
            .field("operation_id", &self.operation_id)
            .field("route_id", &self.route_id)
            .field("webhook_id", &self.webhook_id)
            .finish_non_exhaustive()
    }
}

/// 恢复只允许停止和对账，不表示可以重新派发已接受的会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveSessionRecord {
    pub operation_id: Uuid,
    #[serde(default)]
    pub api_key_id: Uuid,
    #[serde(default)]
    pub purpose: RealtimePurpose,
    #[serde(default)]
    pub transport: RealtimeTransport,
    #[serde(default)]
    pub provider_session_id: Option<String>,
    #[serde(default)]
    pub lease: Option<LiveSessionLease>,
    #[serde(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub deadline_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub coverage: UsageCoverage,
    #[serde(default)]
    pub accounting_state: String,
    #[serde(default)]
    pub requires_stop: bool,
    /// 恢复沿受理时的固定账号来源停止旧会话，不借新账号重派发。
    #[serde(default)]
    pub accepted: Option<BeginRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn incomplete_or_future_credential_shapes_do_not_authorize_a_session() {
        let identity = Uuid::new_v4();
        let credential: NativeCredential = serde_json::from_value(json!({
            "credential_id": identity, "future_field": "tolerated"
        }))
        .unwrap();
        assert!(!credential.usable_at(RealtimePurpose::Realtime, Utc::now()));
        let future: NativeCredential = serde_json::from_value(json!({
            "credential_id":identity, "api_key_id":Uuid::new_v4(),
            "purpose":"future_protocol", "expires_at":Utc::now()+chrono::Duration::seconds(10)
        }))
        .unwrap();
        assert!(!future.usable_at(RealtimePurpose::Realtime, Utc::now()));
    }

    #[test]
    fn a_reported_zero_and_a_missing_usage_keep_different_wire_shapes() {
        let missing: AppendLiveUsageRequest = serde_json::from_value(json!({
            "operation_id":Uuid::new_v4(),"api_key_id":Uuid::new_v4(),
            "event_id":"session.closed","coverage":"unknown","terminal":true
        }))
        .unwrap();
        assert!(missing.usage.is_empty());
        assert_eq!(missing.coverage, UsageCoverage::Unknown);
        let known: ObservedUsage = serde_json::from_value(json!({
            "entry_id":"asr","upstream_model":"asr-model","occurred_at":Utc::now(),
            "meter":{"kind":"audio_seconds","seconds":"0","basis":"input_audio_seconds","source":"supplier_reported"}
        }))
        .unwrap();
        assert_ne!(
            serde_json::to_value(known).unwrap(),
            serde_json::Value::Null
        );
    }
}
