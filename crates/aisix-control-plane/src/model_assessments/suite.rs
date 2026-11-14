//! 固定版本的合成题库；seed 只决定本地调度，不声明上游确定性。

use super::contracts::{Dimension, ExportTask, GraderConfig, Mode, SuiteSnapshot, TaskSnapshot};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MANIFEST: &[u8] = include_bytes!("suites/capability-v1/manifest.json");

#[derive(Serialize)]
struct BankTask {
    dimension: Dimension,
    cluster_id: &'static str,
    quick: bool,
    independence_rationale: &'static str,
    normalized_task: Value,
    expected: Value,
    grader_id: &'static str,
}

include!("suites/capability-v1/bank.rs");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuitePlan {
    pub metadata: SuiteSnapshot,
    pub tasks: Vec<TaskSnapshot>,
}

pub fn build(mode: Mode, seed: &str) -> SuitePlan {
    let mut tasks: Vec<_> = bank().into_iter().filter(|task| mode == Mode::Full || task.quick).map(|entry| {
        let grader_config = GraderConfig {
            id: entry.grader_id.into(), version: "1".into(), build: "capability-v1-grader-1".into(),
            config: match entry.grader_id {
                "exact_json" => json!({"whole_document":true,"duplicate_keys":"reject","extra_keys":"reject","tool_calls":"forbidden"}),
                "tool_calls" => json!({"text":"exact","call_order":"significant","arguments":"exact_json_values","extra_arguments":"reject"}),
                _ => json!({"whitespace":"significant","tool_calls":"forbidden"}),
            },
        };
        let task_hash = hash_json(&json!({
            "suite_id":"capability-v1","suite_version":"1","cluster_id":entry.cluster_id,
            "dimension":entry.dimension,"normalized_task":entry.normalized_task,
            "expected":entry.expected,"grader_config":grader_config,
        }));
        let request_semantic_hash = hash_json(&entry.normalized_task);
        let input_bytes = entry.normalized_task.to_string().len() as u32;
        TaskSnapshot { dimension: entry.dimension, request_semantic_hash, input_bytes, task: ExportTask {
            pair_id: pair_id(seed, entry.cluster_id), cluster_id: entry.cluster_id.into(), task_hash,
            normalized_task: entry.normalized_task, expected: entry.expected, grader_config,
        } }
    }).collect();
    tasks.sort_by_cached_key(|task| schedule_key(seed, &task.task.cluster_id));
    SuitePlan {
        metadata: SuiteSnapshot {
            id: "capability-v1".into(),
            version: "1".into(),
            manifest_hash: hex::encode(Sha256::digest(MANIFEST)),
            generator_version: "1".into(),
            grader_version: "1".into(),
            seed: seed.into(),
        },
        tasks,
    }
}

fn schedule_key(seed: &str, cluster: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"aisix.capability-v1.schedule\0");
    hash.update((seed.len() as u64).to_be_bytes());
    hash.update(seed.as_bytes());
    hash.update(cluster.as_bytes());
    hash.finalize().into()
}

fn pair_id(seed: &str, cluster: &str) -> String {
    let hash = schedule_key(seed, cluster);
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    // RFC 9562 的版本 8 用于应用自定义的确定性 UUID。
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

/// Hash 使用递归排序对象键、无空白的 UTF-8 JSON；数组顺序和数值类型保留。
pub fn hash_json(value: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut entries: Vec<_> = object.iter().collect();
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(key, value)| (key.clone(), canonical(value)))
                        .collect(),
                )
            }
            Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
            value => value.clone(),
        }
    }
    hex::encode(Sha256::digest(canonical(value).to_string().as_bytes()))
}

/// 只承认该版本审定的完整计划，防止换簇名或改数字制造独立性。
pub(super) fn matches_frozen_plan(plan: &SuitePlan, mode: Mode) -> bool {
    let frozen = build(mode, &plan.metadata.seed);
    let a = &plan.metadata;
    let b = &frozen.metadata;
    a.id == b.id
        && a.version == b.version
        && a.manifest_hash == b.manifest_hash
        && a.generator_version == b.generator_version
        && a.grader_version == b.grader_version
        && plan.tasks.len() == frozen.tasks.len()
        && plan.tasks.iter().zip(&frozen.tasks).all(|(a, b)| {
            a.dimension == b.dimension
                && a.request_semantic_hash == b.request_semantic_hash
                && a.input_bytes == b.input_bytes
                && a.task.pair_id == b.task.pair_id
                && a.task.cluster_id == b.task.cluster_id
                && a.task.task_hash == b.task.task_hash
                && a.task.normalized_task == b.task.normalized_task
                && a.task.expected == b.task.expected
                && a.task.grader_config.id == b.task.grader_config.id
                && a.task.grader_config.version == b.task.grader_config.version
                && a.task.grader_config.build == b.task.grader_config.build
                && a.task.grader_config.config == b.task.grader_config.config
        })
}

#[cfg(test)]
#[path = "tests/suite.rs"]
mod tests;
