use super::*;
use aisix_core::ResourceEntry;
use serde_json::json;
use std::sync::Arc;

fn group(
    strategy: RoutingStrategy,
    targets: Vec<RoutingTarget>,
    fallback: Option<u32>,
) -> Arc<Model> {
    Arc::new(
        serde_json::from_value(json!({
            "display_name": "group",
            "routing": {"strategy": strategy, "targets": targets, "max_fallbacks": fallback},
        }))
        .unwrap(),
    )
}

fn put(snapshot: &AisixSnapshot, id: &str, name: &str, cost: f64) {
    let model = serde_json::from_value(json!({
        "display_name": name, "provider": "openai", "model_name": name,
        "provider_key_id": "pk", "cost": {"input_per_1k": cost, "output_per_1k": 0.0},
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(id, model, 1));
}

fn resolve(
    snapshot: &AisixSnapshot,
    status: &crate::ModelRuntimeStatusTracker,
    model: &Arc<Model>,
) -> Vec<AttemptModel> {
    resolve_attempt_models(
        &RoutingRegistry::new(),
        status,
        &LivePricingIndex::new(),
        snapshot,
        "group",
        "group-id",
        model,
        RoutingRequest::default(),
    )
    .unwrap()
}

#[test]
fn priority_order_preserves_declaration_ties_before_limiting_fallbacks() {
    // 破坏优先级方向、同层声明顺序，或提前截断候选集，都会改变这份预期。
    for count in [8usize, 128, 1024] {
        let targets: Vec<_> = (0..count)
            .map(|i| RoutingTarget::new(format!("target-{i}")).with_priority((i % 7) as i32 - 3))
            .collect();
        let mut expected: Vec<_> = (0..count).collect();
        expected.sort_by_key(|i| std::cmp::Reverse((*i % 7) as i32 - 3));
        let model = group(RoutingStrategy::Failover, targets, Some(4));
        let routing = model.routing.as_ref().unwrap();
        assert_eq!(
            RoutingRegistry::new().pick_targets("group", routing, &routing.targets, ""),
            expected[..5]
                .iter()
                .map(|i| format!("target-{i}"))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn filtered_targets_do_not_consume_fallbacks_and_first_reference_keeps_its_priority() {
    // ID 引用跟随模型更名；同一模型用名字重复引用时不能覆盖第一次的优先级。
    let snapshot = AisixSnapshot::new();
    put(&snapshot, "id-a", "a", 1.0);
    put(&snapshot, "id-b", "b-renamed", 1.0);
    put(&snapshot, "id-c", "c", 1.0);
    put(&snapshot, "id-d", "d", 1.0);
    let status = crate::ModelRuntimeStatusTracker::new();
    status.mark_cooldown("id-a", Duration::from_secs(60), "test");
    let model = group(
        RoutingStrategy::Failover,
        vec![
            RoutingTarget::new("missing").with_priority(100),
            RoutingTarget::new("a").with_priority(20),
            RoutingTarget::new("d").with_priority(2),
            RoutingTarget::by_id("id-b").with_priority(1),
            RoutingTarget::new("b-renamed").with_priority(99),
            RoutingTarget::new("c").with_priority(1),
        ],
        Some(1),
    );
    let actual = resolve(&snapshot, &status, &model);
    assert_eq!(
        actual.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        ["id-d", "id-b"]
    );
    assert_eq!(actual[1].model.display_name, "b-renamed");
    assert_eq!(actual[1].priority, 1);
}

#[test]
fn metric_routing_ranks_within_priority_and_keeps_equal_cost_declaration_order() {
    // 便宜的低层目标不能越过高层；相同价格必须继续遵守声明顺序。
    let snapshot = AisixSnapshot::new();
    for (id, name, cost) in [
        ("id-low", "low", 0.01),
        ("id-high-a", "high-a", 2.0),
        ("id-high-b", "high-b", 2.0),
        ("id-high-c", "high-c", 1.0),
    ] {
        put(&snapshot, id, name, cost);
    }
    let model = group(
        RoutingStrategy::LeastCost,
        vec![
            RoutingTarget::new("low").with_priority(-1),
            RoutingTarget::new("high-a").with_priority(5),
            RoutingTarget::by_id("id-high-b").with_priority(5),
            RoutingTarget::new("high-c").with_priority(5),
        ],
        Some(2),
    );
    let actual = resolve(&snapshot, &crate::ModelRuntimeStatusTracker::new(), &model);
    assert_eq!(
        actual.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        ["id-high-c", "id-high-a", "id-high-b"]
    );
}

#[test]
fn consistent_hash_keeps_configured_weights_and_priority_when_candidates_are_filtered() {
    // 删除一个候选不应重映射原本落在幸存者上的键；低优先级不能抢占高层。
    let targets = vec![
        RoutingTarget::new("low-a").with_priority(-5).with_weight(3),
        RoutingTarget::new("heavy").with_priority(5).with_weight(90),
        RoutingTarget::new("light").with_priority(5).with_weight(10),
        RoutingTarget::new("medium")
            .with_priority(5)
            .with_weight(30),
        RoutingTarget::new("low-b").with_priority(-5).with_weight(7),
    ];
    let model = group(RoutingStrategy::ConsistentHash, targets.clone(), None);
    let routing = model.routing.as_ref().unwrap();
    let filtered: Vec<_> = targets
        .iter()
        .filter(|t| t.model != "light")
        .cloned()
        .collect();
    let reversed: Vec<_> = targets.iter().rev().cloned().collect();
    let registry = RoutingRegistry::new();
    let mut counts = [0usize; 3];
    for i in 0..4096 {
        let key = format!("session-{i:06}");
        let full = registry.pick_targets("full", routing, &targets, &key);
        let kept = registry.pick_targets("filtered", routing, &filtered, &key);
        let shuffled = registry.pick_targets("reversed", routing, &reversed, &key);
        assert_eq!(full.len(), 5);
        assert_eq!(kept.len(), 4);
        assert!(full[..3].iter().all(|name| !name.starts_with("low-")));
        assert!(full[3..].iter().all(|name| name.starts_with("low-")));
        assert!(kept[..2].iter().all(|name| !name.starts_with("low-")));
        assert!(kept[2..].iter().all(|name| name.starts_with("low-")));
        assert_eq!(full[0], shuffled[0]);
        if full[0] != "light" {
            assert_eq!(full[0], kept[0], "surviving winner moved for {key}");
        }
        let slot = match full[0].as_str() {
            "heavy" => 0,
            "light" => 1,
            "medium" => 2,
            name => panic!("low-priority target selected first: {name}"),
        };
        counts[slot] += 1;
    }
    assert!(
        counts[0] > counts[1] * 4,
        "90:10 share was lost: {counts:?}"
    );
    assert!(
        counts[0] > counts[2] * 2,
        "90:30 share was lost: {counts:?}"
    );
}

#[test]
fn routing_resolution_scaling_probe() {
    // 计时仅供前后比较，语义由独立断言固定，不用机器相关的墙钟阈值判定通过。
    for count in [8usize, 64, 256, 1024] {
        for filtered in [false, true] {
            let snapshot = AisixSnapshot::new();
            let status = crate::ModelRuntimeStatusTracker::new();
            let mut targets = Vec::with_capacity(count);
            for i in 0..count {
                let name = format!("model-{i:06}");
                let id = format!("id-{i:06}");
                put(&snapshot, &id, &name, i as f64 + 1.0);
                targets.push(RoutingTarget::new(name));
                if filtered && i % 2 == 0 {
                    status.mark_cooldown(&id, Duration::from_secs(600), "probe");
                }
            }
            let model = group(RoutingStrategy::LeastCost, targets, Some(0));
            let registry = RoutingRegistry::new();
            let pricing = LivePricingIndex::new();
            let repetitions = (8192 / count).max(8);
            let started = std::time::Instant::now();
            for _ in 0..repetitions {
                let result = resolve_attempt_models(
                    &registry,
                    &status,
                    &pricing,
                    &snapshot,
                    "group",
                    "group-id",
                    &model,
                    RoutingRequest::default(),
                )
                .unwrap();
                assert_eq!(result.len(), 1);
                assert_eq!(
                    result[0].id,
                    if filtered { "id-000001" } else { "id-000000" }
                );
                std::hint::black_box(result);
            }
            eprintln!(
                "routing_resolution_scaling_probe count={count} filtered={filtered} reps={repetitions} ns_per_call={}",
                started.elapsed().as_nanos() / repetitions as u128
            );
        }
    }
}

#[test]
fn forwarding_small_routing_scaling_probe() {
    // 此代码追加到冻结的 routing::performance_contract_tests 模块，复用其 fixture。
    // 预期只依赖配置契约；计时之外检查完整候选语义，避免断言开销掩盖常数回退。
    let repetitions = std::env::var("AISIX_SMALL_BENCH_REPS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(256);
    for count in [1usize, 2, 8] {
        let snapshot = AisixSnapshot::new();
        let targets: Vec<_> = (0..count)
            .map(|i| {
                let name = format!("small-{i}");
                put(&snapshot, &format!("small-id-{i}"), &name, i as f64 + 1.0);
                RoutingTarget::new(name)
            })
            .collect();
        for (strategy, weighted) in [
            (RoutingStrategy::Failover, false),
            (RoutingStrategy::RoundRobin, false),
            (RoutingStrategy::ConsistentHash, false),
            (RoutingStrategy::LeastCost, false),
            (RoutingStrategy::LeastLatency, false),
            (RoutingStrategy::LeastBusy, false),
            (RoutingStrategy::RoundRobin, true),
            (RoutingStrategy::ConsistentHash, true),
        ] {
            for max_fallbacks in [Some(0), None] {
                for filtered in [false, true] {
                    if filtered && count == 1 {
                        continue;
                    }
                    let configured = targets
                        .iter()
                        .enumerate()
                        .map(|(i, target)| {
                            if weighted {
                                target.clone().with_weight((i + 1) as u32)
                            } else {
                                target.clone()
                            }
                        })
                        .collect();
                    let model = group(strategy, configured, max_fallbacks);
                    let registry = RoutingRegistry::new();
                    let pricing = LivePricingIndex::new();
                    let status = crate::ModelRuntimeStatusTracker::new();
                    if filtered {
                        status.mark_cooldown("small-id-0", Duration::from_secs(600), "probe");
                    }
                    let available = count - usize::from(filtered);
                    let expected = if max_fallbacks == Some(0) {
                        1
                    } else {
                        available
                    };
                    let invoke = || {
                        resolve_attempt_models(
                            &registry,
                            &status,
                            &pricing,
                            &snapshot,
                            "small-group",
                            "small-group-id",
                            &model,
                            RoutingRequest::default(),
                        )
                        .unwrap()
                    };
                    let warm = invoke();
                    assert_eq!(warm.len(), expected);
                    let unique: std::collections::HashSet<_> = warm.iter().map(|a| &a.id).collect();
                    assert_eq!(unique.len(), expected);
                    assert!(warm.iter().all(|a| !filtered || a.id != "small-id-0"));
                    let mut samples = Vec::new();
                    for _ in 0..7 {
                        let started = std::time::Instant::now();
                        let mut resolved = 0;
                        for _ in 0..repetitions {
                            resolved += std::hint::black_box(invoke()).len();
                        }
                        let elapsed = started.elapsed().as_secs_f64() * 1e9 / repetitions as f64;
                        assert_eq!(resolved, repetitions * expected);
                        samples.push(elapsed);
                    }
                    let mut ordered = samples.clone();
                    ordered.sort_by(f64::total_cmp);
                    eprintln!(
                        "{}",
                        json!({
                            "case":"forwarding_small_routing", "targets":count,
                            "strategy":strategy, "nonuniform_weights":weighted, "max_fallbacks":max_fallbacks,
                            "filtered":filtered, "repetitions_per_sample":repetitions,
                            "median_ns_per_call":ordered[3], "samples_ns_per_call":samples,
                        })
                    );
                }
            }
        }
    }
}

#[test]
fn nonuniform_round_robin_preserves_weight_share_inside_priority() {
    // 忽略权重、全局轮转或低层抢占高层，都会破坏 3:1 的整周期份额。
    let model = group(
        RoutingStrategy::RoundRobin,
        vec![
            RoutingTarget::new("backup")
                .with_priority(-1)
                .with_weight(100),
            RoutingTarget::new("heavy").with_priority(2).with_weight(3),
            RoutingTarget::new("light").with_priority(2).with_weight(1),
        ],
        Some(0),
    );
    let routing = model.routing.as_ref().unwrap();
    let registry = RoutingRegistry::new();
    let mut counts = [0usize; 2];
    for _ in 0..160 {
        let picked = registry.pick_targets("weighted", routing, &routing.targets, "");
        assert_eq!(picked.len(), 1);
        match picked[0].as_str() {
            "heavy" => counts[0] += 1,
            "light" => counts[1] += 1,
            name => panic!("lower priority target selected: {name}"),
        }
    }
    assert_eq!(counts, [120, 40]);
}

#[test]
fn stored_weighted_shapes_keep_their_nonuniform_weights_after_loading() {
    // 存量 weighted 先迁移到当前策略；不能因基准覆盖新枚举而丢失旧配置权重。
    for (sticky, strategy) in [
        (false, RoutingStrategy::RoundRobin),
        (true, RoutingStrategy::ConsistentHash),
    ] {
        let mut document = json!({"display_name":"stored", "routing":{
            "strategy":"weighted", "sticky":sticky,
            "targets":[{"model":"heavy", "weight":3}, {"model":"light", "weight":1}],
        }});
        aisix_core::models::upgrade_stored_routing(&mut document);
        let loaded: Model = serde_json::from_value(document).unwrap();
        let routing = loaded.routing.as_ref().unwrap();
        assert_eq!(routing.strategy, strategy);
        assert_eq!(routing.targets[0].weight_or_default(), 3);
        assert_eq!(routing.targets[1].weight_or_default(), 1);
        let registry = RoutingRegistry::new();
        let mut counts = [0usize; 2];
        for index in 0..4096 {
            let picked =
                registry.pick_targets("stored", routing, &routing.targets, &format!("key-{index}"));
            assert_eq!(picked.len(), 2);
            match picked[0].as_str() {
                "heavy" => counts[0] += 1,
                "light" => counts[1] += 1,
                name => panic!("unknown loaded target: {name}"),
            }
        }
        assert!(
            counts[0] > counts[1] * 2,
            "3:1 weighted share lost: {counts:?}"
        );
        assert!(counts[1] > 0, "light target disappeared: {counts:?}");
    }
}

#[test]
fn routing_priority_partition_scaling_probe() {
    // 同层与多层都走真实 resolver；时间没有通过阈值，配置构造不计入样本。
    for count in [1usize, 2, 8, 64, 256, 1024] {
        for distinct_priorities in [false, true] {
            let snapshot = AisixSnapshot::new();
            let targets: Vec<_> = (0..count)
                .map(|index| {
                    let name = format!("tier-{index:06}");
                    put(&snapshot, &format!("tier-id-{index:06}"), &name, 1.0);
                    RoutingTarget::new(name).with_priority(if distinct_priorities {
                        index as i32
                    } else {
                        0
                    })
                })
                .collect();
            for strategy in [RoutingStrategy::Failover, RoutingStrategy::ConsistentHash] {
                let model = group(strategy, targets.clone(), None);
                let registry = RoutingRegistry::new();
                let pricing = LivePricingIndex::new();
                let status = crate::ModelRuntimeStatusTracker::new();
                let invoke = || {
                    resolve_attempt_models(
                        &registry,
                        &status,
                        &pricing,
                        &snapshot,
                        "tier-group",
                        "tier-group-id",
                        &model,
                        RoutingRequest::default(),
                    )
                    .unwrap()
                };
                let warm = invoke();
                assert_eq!(warm.len(), count);
                let unique: std::collections::HashSet<_> =
                    warm.iter().map(|attempt| &attempt.id).collect();
                assert_eq!(unique.len(), count);
                if distinct_priorities {
                    assert_eq!(warm[0].id, format!("tier-id-{:06}", count - 1));
                    assert_eq!(warm[count - 1].id, "tier-id-000000");
                }
                let repetitions = (2048 / count).max(4);
                let mut samples = Vec::new();
                for _ in 0..7 {
                    let started = std::time::Instant::now();
                    let mut resolved = 0;
                    for _ in 0..repetitions {
                        resolved += std::hint::black_box(invoke()).len();
                    }
                    samples.push(started.elapsed().as_secs_f64() * 1e9 / repetitions as f64);
                    assert_eq!(resolved, repetitions * count);
                }
                eprintln!(
                    "{}",
                    json!({"case":"routing_priority_partition", "targets":count,
                    "strategy":strategy, "distinct_priorities":distinct_priorities,
                    "repetitions_per_sample":repetitions, "samples_ns_per_call":samples})
                );
            }
        }
    }
}
