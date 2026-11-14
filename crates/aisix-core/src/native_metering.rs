//! 生命周期计量保留上游真实单位；缺失计量与缺价不能变成零费用。

use crate::native_objects::{LeaseToken, ObservedArtifactEntry, TerminalUsage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum MeteringTarget {
    Object {
        lease: LeaseToken,
    },
    Operation {
        operation_id: Uuid,
        api_key_id: Uuid,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementSource {
    SupplierReported,
    GatewayObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioMeasurementBasis {
    InputAudioSeconds,
    OutputAudioSeconds,
}

/// 接受时的来源与请求参数只作审计，不能替代最终报告的实际用量。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LifecycleContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_generation: Option<VideoGenerationContext>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoGenerationContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_video_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_reported_seconds: Option<String>,
    /// 请求新增长度从未被当作供应商实际生成长度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_segment_seconds: Option<String>,
}

impl LifecycleContext {
    pub fn validate(&self) -> Result<(), MeteringContractError> {
        if let Some(video) = &self.video_generation {
            for seconds in [
                &video.source_reported_seconds,
                &video.requested_segment_seconds,
            ]
            .into_iter()
            .flatten()
            {
                validate_reported_seconds(seconds)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservedMeter {
    Tokens {
        usage: TerminalUsage,
    },
    TrainingTokens {
        trained_tokens: u64,
    },
    /// 字符串保留供应商十进制精度；不从请求长度或拼接后总长推断新增量。
    GeneratedVideoSeconds {
        seconds: String,
        model: String,
        size: String,
    },
    AudioSeconds {
        seconds: String,
        basis: AudioMeasurementBasis,
        source: MeasurementSource,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedUsage {
    pub entry_id: String,
    pub upstream_model: String,
    pub occurred_at: DateTime<Utc>,
    pub meter: ObservedMeter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedCompleteRequest {
    pub target: MeteringTarget,
    pub terminal_status: String,
    pub usage: Vec<ObservedUsage>,
    #[serde(default)]
    pub artifacts: Vec<ObservedArtifactEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationReason {
    MissingUsage,
    UnknownUsageShape,
    UnpricedMeter,
    IncompleteObservation,
    UncertainSubmission,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationRequest {
    pub target: MeteringTarget,
    #[serde(default)]
    pub terminal_status: Option<String>,
    pub reason: ReconciliationReason,
    #[serde(default)]
    pub observed_usage: Vec<ObservedUsage>,
    /// 只保留供应商 usage 槽供受限对账，不接受请求正文、错误正文或凭据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_usage: Option<serde_json::Value>,
    #[serde(default)]
    pub artifacts: Vec<ObservedArtifactEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MeteringContractError {
    #[error("the reported duration is not a non-negative decimal")]
    InvalidDuration,
    #[error("the terminal measurement has no observed quantities")]
    MissingUsage,
    #[error("the terminal measurement has a duplicate entry")]
    DuplicateEntry,
    #[error("the token measurement names a different entry")]
    EntryMismatch,
    #[error("the video measurement is missing its model or size")]
    MissingVideoDimension,
}

/// 只验证计量契约，不据此猜费用、模型能力或未报告用量。
pub fn validate_observed_usage(usage: &[ObservedUsage]) -> Result<(), MeteringContractError> {
    if usage.is_empty() {
        return Err(MeteringContractError::MissingUsage);
    }
    let mut entries = std::collections::BTreeSet::new();
    for observation in usage {
        if !entries.insert(observation.entry_id.as_str()) {
            return Err(MeteringContractError::DuplicateEntry);
        }
        match &observation.meter {
            ObservedMeter::Tokens { usage } => {
                if usage.entry_id != observation.entry_id {
                    return Err(MeteringContractError::EntryMismatch);
                }
            }
            ObservedMeter::TrainingTokens { .. } => {}
            ObservedMeter::GeneratedVideoSeconds {
                seconds,
                model,
                size,
            } => {
                validate_reported_seconds(seconds)?;
                if model.is_empty() || size.is_empty() {
                    return Err(MeteringContractError::MissingVideoDimension);
                }
            }
            ObservedMeter::AudioSeconds { seconds, .. } => validate_reported_seconds(seconds)?,
        }
    }
    Ok(())
}

/// 接受精确十进制秒数，不接受负数、指数、NaN 或无穷值。
pub fn validate_reported_seconds(seconds: &str) -> Result<(), MeteringContractError> {
    let valid = match seconds.split_once('.') {
        Some((whole, fraction)) => {
            !whole.is_empty()
                && !fraction.is_empty()
                && whole.bytes().all(|byte| byte.is_ascii_digit())
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
        }
        None => !seconds.is_empty() && seconds.bytes().all(|byte| byte.is_ascii_digit()),
    };
    if valid {
        Ok(())
    } else {
        Err(MeteringContractError::InvalidDuration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn training(tokens: u64) -> ObservedUsage {
        ObservedUsage {
            entry_id: String::new(),
            upstream_model: "provider-base".into(),
            occurred_at: Utc::now(),
            meter: ObservedMeter::TrainingTokens {
                trained_tokens: tokens,
            },
        }
    }

    #[test]
    fn an_explicit_reported_zero_is_known_training_usage() {
        assert!(validate_observed_usage(&[training(0)]).is_ok());
        assert_eq!(
            validate_observed_usage(&[]),
            Err(MeteringContractError::MissingUsage)
        );
        assert!(
            serde_json::from_value::<ObservedMeter>(json!({"kind":"training_tokens"})).is_err()
        );
    }

    #[test]
    fn exact_decimal_duration_rejects_non_quantities() {
        for valid in [
            "0",
            "4",
            "4.125",
            "000.001",
            "12345678901234567890.123456789",
        ] {
            assert!(validate_reported_seconds(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "", "-1", "+1", "NaN", "inf", "1e3", " 4", "4 ", ".4", "4.", "1.2.3",
        ] {
            assert_eq!(
                validate_reported_seconds(invalid),
                Err(MeteringContractError::InvalidDuration)
            );
        }
    }

    #[test]
    fn duplicate_training_entries_cannot_be_settled_twice() {
        assert_eq!(
            validate_observed_usage(&[training(2), training(3)]),
            Err(MeteringContractError::DuplicateEntry)
        );
    }

    #[test]
    fn token_quantities_cannot_be_attributed_to_a_different_entry() {
        let usage: TerminalUsage = serde_json::from_value(json!({
            "entry_id":"accepted-model-a","occurred_at":Utc::now(),"status_code":200,
            "prompt_tokens":2,"completion_tokens":3
        }))
        .unwrap();
        let observation = ObservedUsage {
            entry_id: "accepted-model-b".into(),
            upstream_model: "provider-base".into(),
            occurred_at: Utc::now(),
            meter: ObservedMeter::Tokens { usage },
        };
        assert_eq!(
            validate_observed_usage(&[observation]),
            Err(MeteringContractError::EntryMismatch)
        );
    }

    #[test]
    fn video_duration_needs_reported_dimensions() {
        let mut usage = training(0);
        usage.meter = ObservedMeter::GeneratedVideoSeconds {
            seconds: "4".into(),
            model: "sora-compatible".into(),
            size: "1280x720".into(),
        };
        assert!(validate_observed_usage(&[usage.clone()]).is_ok());
        usage.meter = ObservedMeter::GeneratedVideoSeconds {
            seconds: "4".into(),
            model: String::new(),
            size: "1280x720".into(),
        };
        assert_eq!(
            validate_observed_usage(&[usage]),
            Err(MeteringContractError::MissingVideoDimension)
        );
    }

    #[test]
    fn measured_audio_does_not_claim_a_supplier_report() {
        let meter = ObservedMeter::AudioSeconds {
            seconds: "0.2".into(),
            basis: AudioMeasurementBasis::InputAudioSeconds,
            source: MeasurementSource::GatewayObserved,
        };
        let value = serde_json::to_value(meter).unwrap();
        assert_eq!(value["source"], "gateway_observed");
        assert_eq!(value["basis"], "input_audio_seconds");
        assert!(value.get("trained_tokens").is_none());
    }

    #[test]
    fn unknown_observation_does_not_create_a_zero_quantity() {
        let request: ReconciliationRequest = serde_json::from_value(json!({
            "target":{"target":"operation","operation_id":Uuid::new_v4(),"api_key_id":Uuid::new_v4()},
            "reason":"missing_usage"
        })).unwrap();
        assert!(request.observed_usage.is_empty());
        assert!(request.terminal_status.is_none());
        let value = serde_json::to_value(request).unwrap();
        assert!(value.get("usage").is_none());
    }

    #[test]
    fn requested_video_length_remains_context_and_not_an_observed_meter() {
        let context: LifecycleContext = serde_json::from_value(json!({
            "video_generation":{"source_video_id":"video-original","source_reported_seconds":"8","requested_segment_seconds":"4"}
        })).unwrap();
        assert!(context.validate().is_ok());
        let value = serde_json::to_value(context).unwrap();
        assert_eq!(value["video_generation"]["requested_segment_seconds"], "4");
        assert!(value.get("meter").is_none());
        let invalid: LifecycleContext = serde_json::from_value(json!({
            "video_generation":{"source_reported_seconds":"NaN"}
        }))
        .unwrap();
        assert_eq!(
            invalid.validate(),
            Err(MeteringContractError::InvalidDuration)
        );
    }

    #[test]
    fn unrecognized_supplier_usage_is_kept_separate_from_known_quantities() {
        let request: ReconciliationRequest = serde_json::from_value(json!({
            "target":{"target":"operation","operation_id":Uuid::new_v4(),"api_key_id":Uuid::new_v4()},
            "reason":"unknown_usage_shape",
            "raw_usage":{"provider-model":{"reported_units":17}}
        })).unwrap();
        assert_eq!(
            request.raw_usage.as_ref().unwrap()["provider-model"]["reported_units"],
            17
        );
        assert!(request.observed_usage.is_empty());
        let value = serde_json::to_value(request).unwrap();
        assert!(value.get("prompt_tokens").is_none());
        assert!(value.get("trained_tokens").is_none());
    }
}
