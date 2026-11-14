//! 冻结的能力对照 v1.1 DTO；只保存合成题、脱敏证据及允许的请求参数。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

macro_rules! enums {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}
enums!(Mode { Quick, Full });
enums!(Dimension {
    Reasoning,
    Code,
    Context,
    Tool,
    Protocol
});
enums!(RunState {
    Planned,
    Queued,
    Running,
    Completed,
    Cancelled,
    Interrupted
});
enums!(CompletionReason {
    AllScheduled,
    Cancelled,
    Deadline,
    Budget,
    ConfigurationChanged,
    OwnerLost
});
enums!(Verdict {
    WeakerOnSuite,
    NoDirectionalEvidence,
    Insufficient
});
enums!(Role {
    Reference,
    Candidate
});
enums!(DispatchState {
    NotSent,
    Reserved,
    Sent,
    Finished,
    CancelledBeforeSend,
    AbortedAfterSend,
    InterruptedUnknown
});
enums!(SampleResult {
    Pass,
    Fail,
    Error,
    Unsupported,
    NotRun
});
enums!(ErrorKind {
    Auth,
    RateLimit,
    UpstreamHttp,
    Timeout,
    Transport,
    Decode,
    Truncated,
    ConfigurationChanged,
    Cancelled
});
enums!(ReferenceSourceKind {
    DirectProvider,
    VerifiedChannel
});
enums!(CostStatus { Estimated, Unknown });

impl RunState {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Interrupted)
    }
}
impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Candidate => "candidate",
        }
    }
}
impl Mode {
    pub fn limits(self) -> Limits {
        Limits {
            max_requests: if self == Self::Quick { 20 } else { 160 },
            max_output_tokens_per_request: if self == Self::Quick { 1024 } else { 2048 },
            max_input_bytes_per_request: 65536,
            max_response_bytes_per_request: 65536,
            request_timeout_seconds: 30,
            run_timeout_seconds: if self == Self::Quick { 180 } else { 1800 },
            concurrency: 2,
            per_account_concurrency: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetInput {
    pub model_id: Uuid,
    pub model_version: String,
    pub provider_key_version: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceSource {
    pub kind: ReferenceSourceKind,
    pub note: String,
    pub verified_at: DateTime<Utc>,
    pub independence_confirmed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    pub reference: TargetInput,
    pub candidate: TargetInput,
    pub reference_source: ReferenceSource,
    pub mode: Mode,
    pub suite_id: String,
    pub suite_version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub client_request_id: Uuid,
    pub accept_unknown_price: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeTargetSnapshot {
    pub model_id: Uuid,
    pub model_version: String,
    pub provider_key_version: String,
    pub model_name: String,
    pub account_id: Uuid,
    pub account_name: String,
    pub provider: String,
    pub adapter: String,
    pub configured_model: String,
    pub endpoint_display: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteSnapshot {
    pub id: String,
    pub version: String,
    pub manifest_hash: String,
    pub generator_version: String,
    pub grader_version: String,
    pub seed: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    pub max_requests: u32,
    pub max_output_tokens_per_request: u32,
    pub max_input_bytes_per_request: u32,
    pub max_response_bytes_per_request: u32,
    pub request_timeout_seconds: u32,
    pub run_timeout_seconds: u32,
    pub concurrency: u32,
    pub per_account_concurrency: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimatedCost {
    pub currency: String,
    pub amount_decimal: String,
    pub price_revision: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessmentRunSummary {
    pub id: Uuid,
    pub state: RunState,
    pub mode: Mode,
    pub reference: SafeTargetSnapshot,
    pub candidate: SafeTargetSnapshot,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub reserved_requests: u32,
    pub sent_requests: u32,
    pub unknown_requests: u32,
    pub finished_requests: u32,
    pub planned_requests: u32,
    pub verdict: Verdict,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessmentRun {
    #[serde(flatten)]
    pub summary: AssessmentRunSummary,
    pub schema_version: u32,
    pub actor_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub suite: SuiteSnapshot,
    pub reference_source: ReferenceSource,
    pub limits: Limits,
    pub estimated_cost: Option<EstimatedCost>,
    pub cost_status: CostStatus,
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub completion_reason: Option<CompletionReason>,
    pub reason_codes: Vec<String>,
    pub dimensions: Vec<DimensionReport>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionReport {
    pub dimension: Dimension,
    pub planned_clusters: u32,
    pub valid_clusters: u32,
    pub valid_pairs: u32,
    pub paired_coverage: f64,
    pub reference_score: Option<f64>,
    pub candidate_score: Option<f64>,
    pub delta: Option<f64>,
    pub reference_wins: u32,
    pub candidate_wins: u32,
    pub ties: u32,
    pub p_one_sided: Option<f64>,
    pub p_threshold: f64,
    pub verdict: Verdict,
    pub reason_codes: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub upstream_total_tokens: Option<u64>,
    pub cached_prompt_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceCheck {
    pub name: String,
    pub passed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub checks: Vec<EvidenceCheck>,
    pub expected_hash: String,
    pub answer_preview: Option<String>,
    pub answer_scrubbed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveRequest {
    pub native_protocol: String,
    pub adapter_build: String,
    pub stream: bool,
    pub output_cap: u32,
    pub sampling_policy: String,
    pub semantic_configuration_hash: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessmentSample {
    pub id: Uuid,
    pub ordinal: u32,
    pub pair_id: String,
    pub cluster_id: String,
    pub dimension: Dimension,
    pub role: Role,
    pub request_semantic_hash: String,
    pub task_hash: String,
    pub input_bytes: u32,
    pub dispatch_state: DispatchState,
    pub result: SampleResult,
    pub error_kind: Option<ErrorKind>,
    pub owner_generation: Option<Uuid>,
    pub dispatch_deadline_at: Option<DateTime<Utc>>,
    pub sent_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub http_status: Option<u16>,
    pub finish_reason: Option<String>,
    pub score: Option<u8>,
    pub usage: Usage,
    pub duration_ms: Option<f64>,
    pub output_bytes: Option<u32>,
    pub evidence: Evidence,
    pub effective_request: EffectiveRequest,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraderConfig {
    pub id: String,
    pub version: String,
    pub build: String,
    pub config: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportTask {
    pub pair_id: String,
    pub cluster_id: String,
    pub task_hash: String,
    pub normalized_task: Value,
    pub expected: Value,
    pub grader_config: GraderConfig,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub dimension: Dimension,
    pub request_semantic_hash: String,
    pub input_bytes: u32,
    #[serde(flatten)]
    pub task: ExportTask,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedToolCall {
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NormalizedAnswer {
    pub text: String,
    pub tool_calls: Vec<NormalizedToolCall>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSample {
    #[serde(flatten)]
    pub sample: AssessmentSample,
    pub normalized_answer: Option<NormalizedAnswer>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessmentExport {
    pub schema_version: u32,
    pub interface_version: String,
    pub snapshot_at: DateTime<Utc>,
    pub partial: bool,
    pub run: AssessmentRun,
    pub tasks: Vec<ExportTask>,
    pub samples: Vec<ExportSample>,
}
