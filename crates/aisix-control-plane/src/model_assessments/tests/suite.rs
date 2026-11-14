use super::*;
use crate::model_assessments::contracts::Dimension;
use std::collections::HashSet;

#[test]
fn truth_assignment_has_one_consistent_solution_matching_its_expected_answer() {
    // 从题面解析三个命题并独立穷举，不能把预设答案直接交回评分器自证。
    let task = bank()
        .into_iter()
        .find(|task| task.cluster_id == "reasoning.truth_assignment")
        .unwrap();
    let prompt = task.normalized_task["messages"][1]["content"]
        .as_str()
        .unwrap();
    let names = ["A", "B", "C"];
    let statements: Vec<_> = names
        .iter()
        .map(|speaker| {
            let marker = format!("{speaker} says \"");
            assert_eq!(prompt.matches(marker.as_str()).count(), 1);
            let statement = prompt
                .split_once(marker.as_str())
                .unwrap()
                .1
                .split_once('"')
                .unwrap()
                .0;
            let (subject, predicate) = statement.split_once(' ').unwrap();
            let index = names.iter().position(|name| *name == subject).unwrap();
            let positive = match predicate {
                "tells the truth" => true,
                "lies" => false,
                _ => panic!("未审定的命题谓词"),
            };
            (index, positive)
        })
        .collect();
    let solutions: Vec<_> = (1u8..8)
        .filter(|assignment| assignment.count_ones() == 1)
        .filter(|assignment| {
            statements
                .iter()
                .enumerate()
                .all(|(speaker, &(subject, positive))| {
                    let subject_truth = assignment & (1 << subject) != 0;
                    let speaker_truth = assignment & (1 << speaker) != 0;
                    speaker_truth == (subject_truth == positive)
                })
        })
        .collect();
    assert_eq!(solutions.len(), 1, "题目必须存在唯一一致赋值");
    let answer = names[solutions[0].trailing_zeros() as usize];
    assert_eq!(task.expected, serde_json::json!({"truth_teller":answer}));
}

#[test]
fn compiled_bank_matches_the_versioned_manifest_and_has_no_digit_only_variants() {
    let manifest: serde_json::Value = serde_json::from_slice(MANIFEST).unwrap();
    assert_eq!(manifest["tasks"], serde_json::to_value(bank()).unwrap());
    let mut masked = HashSet::new();
    for entry in bank() {
        assert!(!entry.independence_rationale.is_empty());
        let mut previous_digit = false;
        let shape: String = entry
            .normalized_task
            .to_string()
            .chars()
            .filter_map(|c| {
                let digit = c.is_ascii_digit();
                let keep = !digit || !previous_digit;
                previous_digit = digit;
                keep.then_some(if digit { '#' } else { c })
            })
            .collect();
        assert!(
            masked.insert(shape),
            "digit-only variant {}",
            entry.cluster_id
        );
    }
    let value = serde_json::json!({"b":[{"z":0,"a":true}],"a":"猫"});
    let reordered = serde_json::json!({"a":"猫","b":[{"a":true,"z":0}]});
    assert_eq!(hash_json(&value), hash_json(&reordered));
    assert_ne!(
        hash_json(&value),
        hash_json(&serde_json::json!({"b":[{"z":1,"a":true}],"a":"猫"}))
    );
}

#[test]
fn full_has_sixteen_distinct_clusters_in_each_of_five_dimensions() {
    // 若漏维度、少题或重复簇冒充独立样本，此断言必须失败。
    let plan = build(Mode::Full, "fixed-review-seed");
    assert_eq!(plan.tasks.len(), 80);
    let mut clusters = HashSet::new();
    let mut hashes = HashSet::new();
    for dimension in [
        Dimension::Reasoning,
        Dimension::Code,
        Dimension::Context,
        Dimension::Tool,
        Dimension::Protocol,
    ] {
        assert_eq!(
            plan.tasks
                .iter()
                .filter(|t| t.dimension == dimension)
                .count(),
            16
        );
    }
    for task in &plan.tasks {
        assert!(clusters.insert(&task.task.cluster_id));
        assert!(hashes.insert(&task.request_semantic_hash));
        assert!(task.input_bytes > 0 && task.input_bytes <= 65_536);
        assert!(uuid::Uuid::parse_str(&task.task.pair_id).is_ok());
        assert_eq!(task.task.task_hash.len(), 64);
        assert_eq!(task.request_semantic_hash.len(), 64);
        assert!(!task.task.expected.is_null());
        assert!(!task.task.grader_config.id.is_empty());
    }
    assert_eq!(plan.metadata.manifest_hash.len(), 64);
}

#[test]
fn quick_cluster_selection_is_fixed_while_seed_reproduces_order_and_pair_ids() {
    let quick = build(Mode::Quick, "seed-a");
    let repeated = build(Mode::Quick, "seed-a");
    let changed = build(Mode::Quick, "seed-b");
    let full = build(Mode::Full, "seed-a");
    assert_eq!(quick.tasks.len(), 10);
    assert_eq!(
        serde_json::to_value(&quick).ok(),
        serde_json::to_value(repeated).ok()
    );
    let ids = |plan: &SuitePlan| {
        plan.tasks
            .iter()
            .map(|t| t.task.cluster_id.clone())
            .collect::<HashSet<_>>()
    };
    assert_eq!(ids(&quick), ids(&changed));
    assert!(ids(&quick).is_subset(&ids(&full)));
    assert_ne!(
        quick.tasks.first().map(|t| &t.task.pair_id),
        changed.tasks.first().map(|t| &t.task.pair_id)
    );
    for dimension in [
        Dimension::Reasoning,
        Dimension::Code,
        Dimension::Context,
        Dimension::Tool,
        Dimension::Protocol,
    ] {
        assert_eq!(
            quick
                .tasks
                .iter()
                .filter(|t| t.dimension == dimension)
                .count(),
            2
        );
    }
    for task in &quick.tasks {
        let full_task = full
            .tasks
            .iter()
            .find(|t| t.task.cluster_id == task.task.cluster_id);
        assert!(full_task.is_some());
        assert_eq!(
            full_task.map(|t| &t.task.task_hash),
            Some(&task.task.task_hash)
        );
        assert!(task.task.normalized_task.get("temperature").is_none());
        assert!(task.task.normalized_task.get("top_p").is_none());
        assert!(task.task.normalized_task.get("seed").is_none());
        assert_eq!(
            changed
                .tasks
                .iter()
                .find(|t| t.task.cluster_id == task.task.cluster_id)
                .map(|t| &t.task.task_hash),
            Some(&task.task.task_hash)
        );
    }
}
