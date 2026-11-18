//! 版本化的交换边界长度观察；与供应商 Token 和计费字段相互独立。

use serde::{Deserialize, Serialize};

/// 同一 attempt 单调起点的毫秒偏移；相邻已知时点作差可加和，缺失不补零。
/// HTTP 发送起点表示交给客户端执行前，不代表内核或网卡实际写出。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamTiming {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_send_started_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_response_byte_ms: Option<u32>,
    /// 既有 SSE 解析器首次看到完整 data 帧；非 SSE 或未闭合帧保持未知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_protocol_frame_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_canonical_chunk_ms: Option<u32>,
    /// 上游正文被既有解析器消费为完整非空生成内容帧的时点，不是 socket 到达。
    /// 与首 canonical 片段无先后约束；该片段可能只是元数据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_first_content_ms: Option<u32>,
    /// 既有首内容观察器把非空内容交给 HTTP server 的时点，不是客户端 ACK。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_content_ms: Option<u32>,
}

impl UpstreamTiming {
    pub fn validate(&self) -> bool {
        let mut previous = 0;
        for current in [
            self.http_send_started_ms,
            self.response_headers_ms,
            self.first_response_byte_ms,
            self.first_protocol_frame_ms,
            self.first_canonical_chunk_ms,
            self.first_content_ms,
        ]
        .into_iter()
        .flatten()
        {
            if current < previous {
                return false;
            }
            previous = current;
        }
        if let Some(content) = self.upstream_first_content_ms {
            if [
                self.http_send_started_ms,
                self.response_headers_ms,
                self.first_response_byte_ms,
                self.first_protocol_frame_ms,
            ]
            .into_iter()
            .flatten()
            .any(|before| before > content)
                || self.first_content_ms.is_some_and(|after| after < content)
            {
                return false;
            }
        }
        true
    }
}

/// 请求入口的实际阶段测量；缺失表示未观测，零表示窗口短于一毫秒。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestTiming {
    /// 首次读取正文到 EOF、读错误或取消的窗口，未必已读完整。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_read_ms: Option<u32>,
    /// 入口既有 JSON 解码块的耗时，包含错误分类回退，不包含后续转换。
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
    fn raw_content_is_optional_and_ordered_independently_of_the_first_canonical_chunk() {
        let mut wire = serde_json::json!({
            "http_send_started_ms":0,"response_headers_ms":1,"first_response_byte_ms":2,
            "first_protocol_frame_ms":3,"first_canonical_chunk_ms":4,
            "upstream_first_content_ms":5,"first_content_ms":8
        });
        let timing: UpstreamTiming = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(timing).unwrap()["upstream_first_content_ms"],
            5
        );
        assert!(timing.validate());
        wire["first_canonical_chunk_ms"] = serde_json::json!(6);
        assert!(serde_json::from_value::<UpstreamTiming>(wire.clone())
            .unwrap()
            .validate());
        for reversed in [2, 9] {
            wire["upstream_first_content_ms"] = serde_json::json!(reversed);
            assert!(!serde_json::from_value::<UpstreamTiming>(wire.clone())
                .unwrap()
                .validate());
        }
        let old: UpstreamTiming =
            serde_json::from_value(serde_json::json!({"first_content_ms":8})).unwrap();
        assert!(serde_json::to_value(old)
            .unwrap()
            .get("upstream_first_content_ms")
            .is_none());
        let zero: UpstreamTiming =
            serde_json::from_value(serde_json::json!({"upstream_first_content_ms":0})).unwrap();
        assert_eq!(
            serde_json::to_value(zero)
                .unwrap()
                .get("upstream_first_content_ms"),
            Some(&serde_json::json!(0))
        );
    }

    #[test]
    fn upstream_timing_rejects_reversed_known_samples_without_inventing_missing_ones() {
        let mut timing = UpstreamTiming {
            http_send_started_ms: Some(0),
            first_response_byte_ms: Some(5),
            first_content_ms: Some(12),
            ..Default::default()
        };
        assert!(timing.validate());
        timing.response_headers_ms = Some(6);
        assert!(!timing.validate());
        timing.response_headers_ms = Some(5);
        assert!(timing.validate());
    }

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
