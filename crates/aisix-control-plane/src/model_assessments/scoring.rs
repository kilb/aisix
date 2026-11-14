//! 只评分完整答案；执行失败不等同于能力错题。

use super::contracts::*;
use super::suite::SuitePlan;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

const P_THRESHOLD: f64 = 0.002;
const MIN_DELTA: f64 = -0.10;
const DIMENSIONS: [Dimension; 5] = [
    Dimension::Reasoning,
    Dimension::Code,
    Dimension::Context,
    Dimension::Tool,
    Dimension::Protocol,
];

pub type CollectedAnswer = NormalizedAnswer;

#[derive(Debug, Clone)]
pub struct SampleGrade {
    pub result: SampleResult,
    pub score: Option<u8>,
    pub checks: Vec<EvidenceCheck>,
    pub expected_hash: String,
}

#[derive(Debug, Clone)]
pub struct RunEvaluationContext {
    pub mode: Mode,
    pub plan: SuitePlan,
    pub state: RunState,
    pub completion_reason: Option<CompletionReason>,
    pub configuration_comparable: bool,
    pub independence_confirmed: bool,
    pub ever_stopped: bool,
}

pub fn grade(task: &TaskSnapshot, answer: &CollectedAnswer) -> SampleGrade {
    let expected_hash = super::suite::hash_json(&task.task.expected);
    let check = |name: &str, passed| EvidenceCheck {
        name: name.into(),
        passed,
    };
    if task.task.grader_config.version != "1" {
        return SampleGrade {
            result: SampleResult::Unsupported,
            score: None,
            checks: vec![check("known_grader_version", false)],
            expected_hash,
        };
    }
    if answer.text.trim().is_empty() && answer.tool_calls.is_empty() {
        return SampleGrade {
            result: SampleResult::Error,
            score: None,
            checks: vec![check("answer_present", false)],
            expected_hash,
        };
    }
    let checks = match task.task.grader_config.id.as_str() {
        "exact_json" => {
            let parsed = serde_json::from_str::<StrictJson>(&answer.text);
            vec![
                check("no_tool_calls", answer.tool_calls.is_empty()),
                check("complete_json_without_duplicate_keys", parsed.is_ok()),
                check(
                    "exact_structure_and_values",
                    parsed.is_ok_and(|value| value.0 == task.task.expected),
                ),
            ]
        }
        "exact_text" => vec![
            check("no_tool_calls", answer.tool_calls.is_empty()),
            check(
                "exact_text",
                task.task.expected.as_str() == Some(answer.text.as_str()),
            ),
        ],
        "tool_calls" => {
            let expected = task
                .task
                .expected
                .get("tool_calls")
                .and_then(Value::as_array);
            let same_names = expected.is_some_and(|expected| {
                expected.len() == answer.tool_calls.len()
                    && expected.iter().zip(&answer.tool_calls).all(|(a, b)| {
                        a.get("name").and_then(Value::as_str) == Some(b.name.as_str())
                    })
            });
            let same_args = expected.is_some_and(|expected| {
                expected.len() == answer.tool_calls.len()
                    && expected
                        .iter()
                        .zip(&answer.tool_calls)
                        .all(|(a, b)| a.get("arguments") == Some(&b.arguments))
            });
            vec![
                check(
                    "exact_text",
                    task.task.expected.get("text").and_then(Value::as_str)
                        == Some(answer.text.as_str()),
                ),
                check(
                    "tool_call_count",
                    expected.is_some_and(|calls| calls.len() == answer.tool_calls.len()),
                ),
                check("tool_names_and_order", same_names),
                check("tool_arguments_and_order", same_args),
            ]
        }
        _ => {
            return SampleGrade {
                result: SampleResult::Unsupported,
                score: None,
                checks: vec![check("known_grader", false)],
                expected_hash,
            }
        }
    };
    let passed = checks.iter().all(|check| check.passed);
    SampleGrade {
        result: if passed {
            SampleResult::Pass
        } else {
            SampleResult::Fail
        },
        score: Some(u8::from(passed)),
        checks,
        expected_hash,
    }
}

/// 固定最多 16 个非平簇，整数精确求二项上尾；平簇不进入分母。
pub fn exact_sign_test(reference_wins: u32, candidate_wins: u32) -> Option<f64> {
    let n = reference_wins.checked_add(candidate_wins)?;
    if n == 0 || n > 16 {
        return None;
    }
    let mut coefficient = 1u64;
    let mut tail = 0u64;
    for j in 0..=n {
        if j >= reference_wins {
            tail += coefficient;
        }
        if j < n {
            coefficient = coefficient * u64::from(n - j) / u64::from(j + 1);
        }
    }
    Some(tail as f64 / (1u64 << n) as f64)
}

fn directional_verdict(delta: f64, p: Option<f64>) -> Verdict {
    if delta <= MIN_DELTA && p.is_some_and(|p| p <= P_THRESHOLD) {
        Verdict::WeakerOnSuite
    } else {
        Verdict::NoDirectionalEvidence
    }
}

pub fn summarize(
    context: &RunEvaluationContext,
    samples: &[AssessmentSample],
) -> Vec<DimensionReport> {
    let mut global_reasons = Vec::new();
    if context.mode == Mode::Quick {
        global_reasons.push("quick_descriptive_only");
    }
    if context.state != RunState::Completed
        || context.completion_reason != Some(CompletionReason::AllScheduled)
        || context.ever_stopped
    {
        global_reasons.push("run_not_normally_completed");
    }
    if !context.configuration_comparable {
        global_reasons.push("configuration_incomparable");
    }
    if !context.independence_confirmed {
        global_reasons.push("independence_unconfirmed");
    }
    if !super::suite::matches_frozen_plan(&context.plan, context.mode) {
        global_reasons.push("frozen_plan_mismatch");
    }

    let mut ordinals = HashSet::new();
    let planned_count = context.plan.tasks.len() * 2;
    let mut complete = samples.len() == planned_count;
    let mut matched = true;
    for sample in samples {
        complete &= sample.dispatch_state == DispatchState::Finished
            && sample.result != SampleResult::NotRun
            && sample.error_kind != Some(ErrorKind::Cancelled);
        matched &= (sample.ordinal as usize) < planned_count && ordinals.insert(sample.ordinal);
        matched &= context.plan.tasks.iter().enumerate().any(|(index, task)| {
            sample.ordinal as usize / 2 == index && matches_task(sample, task)
        });
    }
    for task in &context.plan.tasks {
        for role in [Role::Reference, Role::Candidate] {
            matched &= samples
                .iter()
                .filter(|sample| {
                    sample.pair_id == task.task.pair_id
                        && sample.role == role
                        && matches_task(sample, task)
                })
                .count()
                == 1;
        }
    }
    if !complete {
        global_reasons.push("planned_samples_not_all_terminal");
    }
    if !matched {
        global_reasons.push("sample_plan_mismatch");
    }

    DIMENSIONS
        .into_iter()
        .map(|dimension| {
            let tasks: Vec<_> = context
                .plan
                .tasks
                .iter()
                .filter(|task| task.dimension == dimension)
                .collect();
            let mut reasons: Vec<String> = global_reasons
                .iter()
                .map(|reason| (*reason).into())
                .collect();
            let mut reference = 0u32;
            let mut candidate = 0u32;
            let mut valid = 0u32;
            let mut reference_wins = 0u32;
            let mut candidate_wins = 0u32;
            let mut ties = 0u32;
            let mut shared_failures = false;
            for task in &tasks {
                let rows = |role| {
                    samples
                        .iter()
                        .filter(|sample| sample.role == role && matches_task(sample, task))
                        .collect::<Vec<_>>()
                };
                let a = rows(Role::Reference);
                let b = rows(Role::Candidate);
                for sample in a.iter().chain(&b) {
                    if sample.usage.input_tokens.is_none() || sample.usage.output_tokens.is_none() {
                        add_reason(&mut reasons, "usage_missing");
                    }
                    match sample.result {
                        SampleResult::Error => add_reason(&mut reasons, "execution_error"),
                        SampleResult::Unsupported => add_reason(&mut reasons, "unsupported"),
                        SampleResult::NotRun => add_reason(&mut reasons, "not_run"),
                        _ => {}
                    }
                }
                if a.len() != 1 || b.len() != 1 {
                    add_reason(&mut reasons, "pair_incomplete_or_duplicated");
                    continue;
                }
                let (Some(a), Some(b)) = (valid_score(a[0]), valid_score(b[0])) else {
                    add_reason(&mut reasons, "pair_not_scorable");
                    continue;
                };
                valid += 1;
                reference += u32::from(a);
                candidate += u32::from(b);
                match b.cmp(&a) {
                    std::cmp::Ordering::Less => reference_wins += 1,
                    std::cmp::Ordering::Greater => candidate_wins += 1,
                    std::cmp::Ordering::Equal => {
                        ties += 1;
                        shared_failures |= a == 0;
                    }
                }
            }
            let planned = tasks.len() as u32;
            let coverage = if planned == 0 {
                0.0
            } else {
                f64::from(valid) / f64::from(planned)
            };
            let reference_score = (valid > 0).then(|| f64::from(reference) / f64::from(valid));
            let candidate_score = (valid > 0).then(|| f64::from(candidate) / f64::from(valid));
            let delta = (valid > 0)
                .then(|| (f64::from(candidate) - f64::from(reference)) / f64::from(valid));
            let mut quality = global_reasons.is_empty();
            if context.mode == Mode::Full && planned != 16 {
                add_reason(&mut reasons, "planned_cluster_count_invalid");
                quality = false;
            }
            if valid < 12 {
                add_reason(&mut reasons, "fewer_than_twelve_valid_clusters");
                quality = false;
            }
            if coverage < 0.90 {
                add_reason(&mut reasons, "paired_coverage_below_ninety_percent");
                quality = false;
            }
            if shared_failures {
                add_reason(&mut reasons, "shared_failures_do_not_establish_equivalence");
            }
            if reference_wins + candidate_wins == 0 {
                add_reason(&mut reasons, "no_discordant_clusters");
            }
            let p = if quality {
                exact_sign_test(reference_wins, candidate_wins)
            } else {
                None
            };
            let verdict = if quality {
                directional_verdict(delta.unwrap_or(0.0), p)
            } else {
                Verdict::Insufficient
            };
            DimensionReport {
                dimension,
                planned_clusters: planned,
                valid_clusters: valid,
                valid_pairs: valid,
                paired_coverage: coverage,
                reference_score,
                candidate_score,
                delta,
                reference_wins,
                candidate_wins,
                ties,
                p_one_sided: p,
                p_threshold: P_THRESHOLD,
                verdict,
                reason_codes: reasons,
            }
        })
        .collect()
}

pub fn overall_verdict(dimensions: &[DimensionReport]) -> Verdict {
    if dimensions
        .iter()
        .any(|dimension| dimension.verdict == Verdict::WeakerOnSuite)
    {
        return Verdict::WeakerOnSuite;
    }
    if dimensions.len() != DIMENSIONS.len()
        || DIMENSIONS.iter().any(|dimension| {
            dimensions
                .iter()
                .filter(|report| report.dimension == *dimension)
                .count()
                != 1
        })
        || dimensions
            .iter()
            .any(|dimension| dimension.verdict == Verdict::Insufficient)
    {
        return Verdict::Insufficient;
    }
    Verdict::NoDirectionalEvidence
}

fn matches_task(sample: &AssessmentSample, task: &TaskSnapshot) -> bool {
    sample.pair_id == task.task.pair_id
        && sample.cluster_id == task.task.cluster_id
        && sample.dimension == task.dimension
        && sample.task_hash == task.task.task_hash
        && sample.request_semantic_hash == task.request_semantic_hash
        && sample.input_bytes == task.input_bytes
}

fn valid_score(sample: &AssessmentSample) -> Option<u8> {
    if sample.dispatch_state != DispatchState::Finished
        || sample.error_kind.is_some()
        || sample
            .http_status
            .is_some_and(|status| !(200..=299).contains(&status))
        || sample.output_bytes.is_some_and(|bytes| bytes > 65_536)
        || sample
            .finish_reason
            .as_deref()
            .is_some_and(|reason| matches!(reason, "length" | "max_tokens" | "MAX_TOKENS"))
    {
        return None;
    }
    match (sample.result, sample.score) {
        (SampleResult::Pass, Some(1)) => Some(1),
        (SampleResult::Fail, Some(0)) => Some(0),
        _ => None,
    }
}

fn add_reason(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|existing| existing == reason) {
        reasons.push(reason.into());
    }
}

// serde_json 的普通 Value 接受重复对象键；严格题目契约需要递归拒绝。
struct StrictJson(Value);
impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictJson;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a complete JSON value without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::Bool(value)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::from(value)))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::from(value)))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| StrictJson(Value::Number(number)))
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::String(value.into())))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::String(value)))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictJson>()? {
                    values.push(value.0);
                }
                Ok(StrictJson(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, StrictJson>()? {
                    if values.insert(key, value.0).is_some() {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                }
                Ok(StrictJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

#[cfg(test)]
#[path = "tests/scoring.rs"]
mod tests;
