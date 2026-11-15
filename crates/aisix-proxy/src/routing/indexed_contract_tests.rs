use super::*;
use aisix_core::ResourceEntry;
use aisix_response_reference_test_alloc::AllocationScope;
use serde_json::json;
use std::sync::Arc;

fn put(snapshot: &AisixSnapshot, id: &str, name: &str, cost: f64) {
    let model = serde_json::from_value(json!({
        "display_name":name,"provider":"openai","model_name":"upstream",
        "provider_key_id":"pk","cost":{"input_per_1k":cost,"output_per_1k":0.0}
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(id, model, 1));
}

fn group(strategy: RoutingStrategy, targets: Vec<RoutingTarget>, limit: Option<u32>) -> Arc<Model> {
    Arc::new(
        serde_json::from_value(json!({"display_name":"indexed-group","routing":{
            "strategy":strategy,"targets":targets,"max_fallbacks":limit
        }}))
        .unwrap(),
    )
}

fn invoke(
    snapshot: &AisixSnapshot,
    status: &crate::ModelRuntimeStatusTracker,
    registry: &RoutingRegistry,
    model: &Arc<Model>,
    origin: Option<&str>,
) -> Vec<AttemptModel> {
    resolve_attempt_models(
        registry,
        status,
        &LivePricingIndex::new(),
        snapshot,
        "indexed-group",
        "indexed-group-id",
        model,
        RoutingRequest {
            context_origin: origin,
            ..RoutingRequest::default()
        },
    )
    .unwrap()
}

fn filtered_fixture() -> (
    AisixSnapshot,
    crate::ModelRuntimeStatusTracker,
    Vec<RoutingTarget>,
) {
    let snapshot = AisixSnapshot::new();
    for (id, name, cost) in [
        ("id-a", "cooling", 0.0),
        ("id-b", "renamed", 9.0),
        ("id-c", "zero", 0.0),
        ("id-d", "peer", 1.0),
        ("id-e", "backup", 2.0),
    ] {
        put(&snapshot, id, name, cost);
    }
    let status = crate::ModelRuntimeStatusTracker::new();
    status.mark_cooldown("id-a", Duration::from_secs(600), "indexed-contract");
    let targets = vec![
        RoutingTarget::new("missing").with_priority(100),
        RoutingTarget::new("cooling").with_priority(99),
        RoutingTarget::by_id("id-b").with_priority(5).with_weight(7),
        RoutingTarget::new("zero").with_priority(50).with_weight(0),
        RoutingTarget::new("peer").with_priority(5).with_weight(3),
        RoutingTarget::new("backup").with_priority(-1),
        RoutingTarget::new("renamed").with_priority(999),
    ];
    (snapshot, status, targets)
}

#[test]
fn filtered_attempt_positions_keep_identity_priority_weight_and_order() {
    // 缺失、冷却、零权重和重复别名留下不同位置；错误下标会串用模型或首项属性。
    for strategy in [
        RoutingStrategy::Failover,
        RoutingStrategy::RoundRobin,
        RoutingStrategy::ConsistentHash,
        RoutingStrategy::LeastCost,
        RoutingStrategy::LeastLatency,
        RoutingStrategy::LeastBusy,
    ] {
        for limit in [None, Some(0)] {
            let (snapshot, status, targets) = filtered_fixture();
            let model = group(strategy, targets, limit);
            let attempts = invoke(&snapshot, &status, &RoutingRegistry::new(), &model, None);
            let weighted = matches!(
                strategy,
                RoutingStrategy::RoundRobin
                    | RoutingStrategy::ConsistentHash
                    | RoutingStrategy::LeastBusy
            );
            assert_eq!(
                attempts.len(),
                if limit.is_some() {
                    1
                } else if weighted {
                    3
                } else {
                    4
                }
            );
            for attempt in &attempts {
                let (priority, weight) = match attempt.id.as_str() {
                    "id-b" => (5, 7),
                    "id-c" if !weighted => (50, 0),
                    "id-d" => (5, 3),
                    "id-e" => (-1, 1),
                    id => panic!("excluded or unknown model selected: {id}"),
                };
                assert_eq!((attempt.priority, attempt.weight), (priority, weight));
                let stored = snapshot.models.get_by_id(&attempt.id).unwrap();
                assert!(Arc::ptr_eq(&attempt.model, &stored.value));
            }
            let ids: Vec<_> = attempts.iter().map(|attempt| attempt.id.as_str()).collect();
            if strategy == RoutingStrategy::ConsistentHash {
                assert!(matches!(ids[0], "id-b" | "id-d"));
                if limit.is_none() {
                    let mut unique = ids.clone();
                    unique.sort_unstable();
                    assert_eq!(unique, ["id-b", "id-d", "id-e"]);
                    assert_eq!(ids[2], "id-e");
                }
            } else {
                let expected: &[&str] = match strategy {
                    RoutingStrategy::RoundRobin | RoutingStrategy::LeastBusy => {
                        &["id-b", "id-d", "id-e"]
                    }
                    RoutingStrategy::LeastCost => &["id-c", "id-d", "id-b", "id-e"],
                    _ => &["id-c", "id-b", "id-d", "id-e"],
                };
                assert_eq!(ids, &expected[..attempts.len()]);
            }
        }
    }
}

#[test]
fn context_origin_restores_a_filtered_survivor_after_the_strategy_limit() {
    // 初选只留一个高层目标时，低层已接受上下文的真实来源仍须被恢复到首位。
    for strategy in [
        RoutingStrategy::Failover,
        RoutingStrategy::RoundRobin,
        RoutingStrategy::ConsistentHash,
        RoutingStrategy::LeastCost,
        RoutingStrategy::LeastLatency,
        RoutingStrategy::LeastBusy,
    ] {
        let (snapshot, status, targets) = filtered_fixture();
        let model = group(strategy, targets, Some(0));
        let attempts = invoke(
            &snapshot,
            &status,
            &RoutingRegistry::new(),
            &model,
            Some("id-e"),
        );
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, "id-e");
        assert_eq!((attempts[0].priority, attempts[0].weight), (-1, 1));
    }
}

fn allocated_resolution_bytes(count: usize, name_bytes: usize) -> usize {
    let snapshot = AisixSnapshot::new();
    let targets = (0..count)
        .map(|index| {
            let id = format!("allocation-id-{index}");
            let mut name = format!("target-{index}-");
            name.extend(std::iter::repeat_n('x', name_bytes - name.len()));
            put(&snapshot, &id, &name, 1.0);
            RoutingTarget::new(name)
        })
        .collect();
    let model = group(RoutingStrategy::Failover, targets, Some(0));
    let status = crate::ModelRuntimeStatusTracker::new();
    let registry = RoutingRegistry::new();
    assert_eq!(
        invoke(&snapshot, &status, &registry, &model, None)[0].id,
        "allocation-id-0"
    );
    let mut minimum = usize::MAX;
    for _ in 0..3 {
        let scope = AllocationScope::start();
        let attempts = invoke(&snapshot, &status, &registry, &model, None);
        let allocated = scope.finish();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, "allocation-id-0");
        minimum = minimum.min(allocated.bytes);
        std::hint::black_box(attempts);
    }
    minimum
}

#[test]
fn routing_resolution_does_not_copy_selected_model_names() {
    // 固定候选数和返回值，只增长模型标签；复制 target/name 会使累计分配随标签增长。
    for count in [1usize, 2, 8] {
        let short = allocated_resolution_bytes(count, 32);
        let long = allocated_resolution_bytes(count, 4096);
        eprintln!("ROUTING_NAME_ALLOCATION targets={count} short_bytes={short} long_bytes={long}");
        assert!(
            long <= short + 1024,
            "model-name copies remain: targets={count} short={short} long={long}"
        );
    }
}
