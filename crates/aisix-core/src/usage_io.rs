//! 版本化的交换边界长度观察；与供应商 Token 和计费字段相互独立。

use serde::{Deserialize, Serialize};

/// 请求入口的实际阶段测量；缺失表示未观测，零表示窗口短于一毫秒。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestTiming {
    /// 首次读取正文到 EOF、读错误或取消的窗口，未必已读完整。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_read_ms: Option<u32>,
    /// 入口既有首次 JSON 解码的耗时；不包含后续转换或再次读取。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_decode_ms: Option<u32>,
    /// middleware 起点到最终 extractor 完成；原始 Request 路由复用其处理起点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_handler_ms: Option<u32>,
    /// 已实际读取的入口字节；只有完整标志为真时才代表全部正文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body_bytes: Option<u64>,
    /// 来自入口正文计数器的完成状态，不由 Content-Length 推断。
    #[serde(default)]
    pub request_body_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMeasurement {
    pub utf8_bytes: u64,
    pub unicode_scalars: u64,
    pub complete: bool,
}

impl TextMeasurement {
    pub fn is_valid(&self) -> bool {
        self.unicode_scalars <= self.utf8_bytes
            && self.utf8_bytes <= self.unicode_scalars.saturating_mul(4)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageIoAudit {
    pub version: u8,
    pub boundary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_complete: Option<bool>,
    pub response_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_text: Option<TextMeasurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_text: Option<TextMeasurement>,
}

impl UsageIoAudit {
    pub fn is_supported(&self) -> bool {
        matches!(
            (self.version, self.boundary.as_str()),
            (1, "client_exchange") | (2, "upstream_attempt")
        )
    }

    pub fn validate(&self) -> bool {
        self.is_supported()
            && self
                .input_text
                .as_ref()
                .is_none_or(TextMeasurement::is_valid)
            && self
                .output_text
                .as_ref()
                .is_none_or(TextMeasurement::is_valid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_zero_survives_while_unknown_is_omitted() {
        let audit = UsageIoAudit {
            version: 1,
            boundary: "client_exchange".into(),
            transport: None,
            request_body_bytes: Some(0),
            response_body_bytes: None,
            request_complete: None,
            response_complete: false,
            exchange_count: None,
            input_text: Some(TextMeasurement {
                utf8_bytes: 0,
                unicode_scalars: 0,
                complete: true,
            }),
            output_text: None,
        };
        let wire = serde_json::to_value(&audit).unwrap();
        assert_eq!(wire["request_body_bytes"].as_u64(), Some(0));
        assert!(wire.get("response_body_bytes").is_none());
        assert!(wire.get("input_text").is_some());
        assert!(wire.get("output_text").is_none());
        assert_eq!(serde_json::from_value::<UsageIoAudit>(wire).unwrap(), audit);
    }

    #[test]
    fn version_and_scalar_bounds_are_validated_without_overflow() {
        let mut audit = UsageIoAudit {
            version: 1,
            boundary: "client_exchange".into(),
            transport: None,
            request_body_bytes: None,
            response_body_bytes: None,
            request_complete: None,
            response_complete: true,
            exchange_count: None,
            input_text: Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true,
            }),
            output_text: None,
        };
        assert!(audit.validate());
        audit.version = 2;
        assert!(!audit.validate());
        audit.version = 1;
        audit.boundary = "upstream".into();
        assert!(!audit.validate());
        assert!(!TextMeasurement {
            utf8_bytes: 0,
            unicode_scalars: 1,
            complete: true
        }
        .is_valid());
        assert!(!TextMeasurement {
            utf8_bytes: 5,
            unicode_scalars: 1,
            complete: false
        }
        .is_valid());
        assert!(TextMeasurement {
            utf8_bytes: u64::MAX,
            unicode_scalars: u64::MAX,
            complete: false
        }
        .is_valid());
    }

    #[test]
    fn only_matching_legacy_and_upstream_version_boundary_pairs_are_supported() {
        for (version, boundary, supported) in [
            (1, "client_exchange", true),
            (2, "upstream_attempt", true),
            (1, "upstream_attempt", false),
            (2, "client_exchange", false),
            (255, "upstream_attempt", false),
            (2, "upstream", false),
        ] {
            let audit: UsageIoAudit = serde_json::from_value(serde_json::json!({
                "version": version,
                "boundary": boundary,
                "response_complete": false
            }))
            .unwrap();
            assert_eq!(audit.is_supported(), supported, "{version}/{boundary}");
            assert_eq!(audit.validate(), supported, "{version}/{boundary}");
        }
    }

    #[test]
    fn upstream_completeness_and_exchange_count_preserve_zero_false_and_unknown() {
        let wire = serde_json::json!({
            "version": 2, "boundary": "upstream_attempt",
            "request_body_bytes": 0, "response_complete": false,
            "request_complete": false, "exchange_count": 0, "transport": "http_entity"
        });
        let audit: UsageIoAudit = serde_json::from_value(wire.clone()).unwrap();
        assert!(audit.validate());
        assert_eq!(serde_json::to_value(audit).unwrap(), wire);

        let legacy = serde_json::json!({
            "version": 1, "boundary": "client_exchange", "response_complete": true
        });
        let audit: UsageIoAudit = serde_json::from_value(legacy.clone()).unwrap();
        assert!(audit.validate());
        assert_eq!(serde_json::to_value(audit).unwrap(), legacy);
    }

    #[test]
    fn upstream_transport_metadata_is_preserved_without_reinterpreting_future_names() {
        for transport in [
            "http_entity",
            "websocket_payload",
            "mixed",
            "future_transport",
        ] {
            let wire = serde_json::json!({
                "version": 2, "boundary": "upstream_attempt",
                "response_complete": false, "transport": transport
            });
            let audit: UsageIoAudit = serde_json::from_value(wire.clone()).unwrap();
            assert!(audit.validate());
            assert_eq!(serde_json::to_value(audit).unwrap(), wire);
        }
        assert!(serde_json::from_value::<UsageIoAudit>(serde_json::json!({
            "version": 2, "boundary": "upstream_attempt",
            "response_complete": false, "transport": 7
        }))
        .is_err());
    }
}
