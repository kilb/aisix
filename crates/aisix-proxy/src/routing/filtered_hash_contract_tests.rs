use super::*;
use aisix_core::ResourceEntry;
use aisix_response_reference_test_alloc::AllocationScope;
use serde_json::json;
use std::sync::Arc;

// 冻结 ebd91bac 的子集环算法；不调用产品的点生成、权重缩放或偏好排序函数。
fn legacy_order(
    configured: &[RoutingTarget],
    survivors: &[RoutingTarget],
    key: &str,
) -> Vec<String> {
    fn fnv(mut value: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            value = (value ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        value
    }
    fn mix(mut value: u64) -> u64 {
        value ^= value >> 30;
        value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
    fn gcd(mut a: u64, mut b: u64) -> u64 {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    }
    let hash = mix(fnv(0xcbf2_9ce4_8422_2325, key.as_bytes()));
    let mut priorities: Vec<_> = survivors.iter().map(|t| t.priority_or_default()).collect();
    priorities.sort_unstable_by(|left, right| right.cmp(left));
    priorities.dedup();
    let mut order = Vec::new();
    for priority in priorities {
        let weights: Vec<_> = configured
            .iter()
            .filter(|target| target.priority_or_default() == priority)
            .map(|target| u64::from(target.weight_or_default().max(1)))
            .collect();
        let divisor = weights.iter().copied().fold(0, gcd).max(1);
        let sum: u64 = weights.iter().map(|weight| weight / divisor).sum();
        let tier: Vec<_> = survivors
            .iter()
            .filter(|target| target.priority_or_default() == priority)
            .collect();
        let mut points = Vec::new();
        for (index, target) in tier.iter().enumerate() {
            let units = (u64::from(target.weight_or_default().max(1)) / divisor).max(1);
            let units = if sum > 64 {
                (units * 64 / sum).max(1)
            } else {
                units
            };
            for point in 0..units as usize * 160 {
                let name = fnv(0xcbf2_9ce4_8422_2325, target.model.as_bytes());
                let name = fnv(name, &[0]);
                points.push((mix(fnv(name, &(point as u32).to_le_bytes())), index));
            }
        }
        points.sort_unstable();
        let start = points.partition_point(|(point, _)| *point < hash);
        let mut seen = vec![false; tier.len()];
        for offset in 0..points.len() {
            let index = points[(start + offset) % points.len()].1;
            if !seen[index] {
                seen[index] = true;
                order.push(tier[index].model.clone());
            }
        }
    }
    order
}

fn put(snapshot: &AisixSnapshot, id: &str, name: &str) {
    snapshot.models.insert(ResourceEntry::new(
        id,
        serde_json::from_value(json!({
            "display_name":name,"provider":"openai","model_name":"upstream",
            "provider_key_id":"pk"
        }))
        .unwrap(),
        1,
    ));
}

fn group(targets: Vec<RoutingTarget>, limit: Option<u32>) -> Arc<Model> {
    Arc::new(
        serde_json::from_value(json!({"display_name":"oracle-group","routing":{
            "strategy":"consistent_hash","targets":targets,"max_fallbacks":limit
        }}))
        .unwrap(),
    )
}

#[test]
fn filtered_hash_matches_old_complete_fallback_order_after_aliases_and_rename() {
    // 配置层提前去重、选错首次存活别名、重算子集权重或忘记 ID 更名都会改变 oracle 次序。
    let snapshot = AisixSnapshot::new();
    for name in ["a", "b", "zero", "backup"] {
        put(&snapshot, &format!("id-{name}"), name);
    }
    let tagged = |target: RoutingTarget, tags: &[&str]| {
        target.with_tags(tags.iter().map(|tag| (*tag).to_string()).collect())
    };
    let declarations = vec![
        tagged(
            RoutingTarget::by_id("id-a").with_weight(1).with_priority(2),
            &["eu"],
        ),
        tagged(
            RoutingTarget::new("a").with_weight(9).with_priority(1),
            &["us"],
        ),
        tagged(
            RoutingTarget::new("b").with_weight(3).with_priority(2),
            &["eu", "us"],
        ),
        tagged(
            RoutingTarget::by_id("id-b")
                .with_weight(90)
                .with_priority(99),
            &["eu", "us"],
        ),
        tagged(
            RoutingTarget::new("zero").with_weight(0).with_priority(2),
            &["eu", "us"],
        ),
        tagged(
            RoutingTarget::new("missing").with_priority(100),
            &["eu", "us"],
        ),
        tagged(
            RoutingTarget::new("backup").with_priority(-1),
            &["eu", "us"],
        ),
    ];
    let status = crate::ModelRuntimeStatusTracker::new();
    let pricing = LivePricingIndex::new();
    let registry = RoutingRegistry::new();
    for renamed in [false, true] {
        if renamed {
            put(&snapshot, "id-a", "a-renamed");
        }
        for side in [0usize, 1] {
            let tags = vec![if side == 0 { "eu" } else { "us" }.to_string()];
            let first = if side == 0 {
                RoutingTarget::new(if renamed { "a-renamed" } else { "a" })
                    .with_weight(1)
                    .with_priority(2)
            } else {
                RoutingTarget::new("a").with_weight(9).with_priority(1)
            };
            let mut survivors = if side == 1 && renamed {
                vec![]
            } else {
                vec![first]
            };
            survivors.push(RoutingTarget::new("b").with_weight(3).with_priority(2));
            survivors.push(RoutingTarget::new("backup").with_priority(-1));
            for partial in [false, true] {
                let serves = |model: &Model| model.display_name != "backup";
                let kept: Vec<_> = survivors
                    .iter()
                    .filter(|target| !partial || target.model != "backup")
                    .cloned()
                    .collect();
                for limit in [None, Some(0), Some(1)] {
                    let model = group(declarations.clone(), limit);
                    for index in 0..64 {
                        let key = format!("oracle-session-{index}");
                        let actual = resolve_attempt_models(
                            &registry,
                            &status,
                            &pricing,
                            &snapshot,
                            "oracle-group",
                            "oracle-id",
                            &model,
                            RoutingRequest {
                                tags: &tags,
                                api_key_id: &key,
                                serves: partial.then_some(&serves as &dyn Fn(&Model) -> bool),
                                ..Default::default()
                            },
                        )
                        .unwrap();
                        let mut expected = legacy_order(&declarations, &kept, &key);
                        expected.truncate(limit.map_or(usize::MAX, |value| value as usize + 1));
                        assert_eq!(
                            actual
                                .iter()
                                .map(|attempt| attempt.model.display_name.as_str())
                                .collect::<Vec<_>>(),
                            expected,
                            "renamed={renamed} side={side} partial={partial} limit={limit:?} key={key}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn filtered_hash_all_zero_candidates_keep_old_order_with_configured_positive_siblings() {
    // 零权重是否参与在请求存活集上判断；不能因完整配置有正权重就把全零子集删掉。
    let snapshot = AisixSnapshot::new();
    let declarations: Vec<_> = [0u32, 0, 99, 7]
        .iter()
        .enumerate()
        .map(|(index, weight)| {
            let name = format!("zero-case-{index}");
            put(&snapshot, &format!("zero-id-{index}"), &name);
            RoutingTarget::new(name)
                .with_weight(*weight)
                .with_tags(vec![if index < 2 { "zero" } else { "positive" }.into()])
        })
        .collect();
    let model = group(declarations.clone(), None);
    let tags = vec!["zero".to_string()];
    let registry = RoutingRegistry::new();
    let status = crate::ModelRuntimeStatusTracker::new();
    let pricing = LivePricingIndex::new();
    for index in 0..256 {
        let key = format!("all-zero-{index}");
        let actual = resolve_attempt_models(
            &registry,
            &status,
            &pricing,
            &snapshot,
            "zero-group",
            "zero-id",
            &model,
            RoutingRequest {
                tags: &tags,
                api_key_id: &key,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|attempt| attempt.model.display_name.as_str())
                .collect::<Vec<_>>(),
            legacy_order(&declarations, &declarations[..2], &key)
        );
    }
}

#[test]
fn filtered_hash_successors_preserve_equal_point_ties_and_wraparound() {
    // 相同点必须按原声明位置解 tie；跳过不可用声明后也不能按请求局部下标重新打散。
    let ring = HashRing::from_sorted_points(
        0,
        vec![
            (10, 0),
            (10, 1),
            (10, 2),
            (20, 2),
            (30, 0),
            (30, 1),
            (40, 2),
        ],
        3,
    );
    for (key, expected) in [
        (0, [0, 1]),
        (10, [0, 1]),
        (11, [1, 0]),
        (20, [1, 0]),
        (21, [0, 1]),
        (30, [0, 1]),
        (31, [1, 0]),
        (40, [1, 0]),
        (41, [0, 1]),
        (u64::MAX, [0, 1]),
    ] {
        assert_eq!(
            ring.subset_preference_order(key, &[0, 2]),
            expected,
            "key={key}"
        );
        assert_eq!(
            ring.subset_preference_order(key, &[1, 2]),
            expected,
            "key={key}"
        );
    }
    assert_eq!(ring.subset_preference_order(10, &[0, 1, 2]), [0, 1, 2]);
    assert_eq!(ring.subset_preference_order(11, &[0, 1, 2]), [2, 0, 1]);
}

#[test]
fn filtered_hash_matches_legacy_with_weighted_priority_tiers_and_cooldown() {
    // 子集权重域、健康过滤、try_anyway 或跨层 fallback 任一改变都必须逐项偏离旧环。
    for count in [8usize, 32] {
        let snapshot = AisixSnapshot::new();
        let declarations: Vec<_> = (0..count)
            .map(|index| {
                let name = format!("weighted-{index}");
                put(&snapshot, &format!("weighted-id-{index}"), &name);
                RoutingTarget::new(name)
                    .with_weight((index % 7 + 1) as u32)
                    .with_priority((index % 3) as i32 - 1)
                    .with_tags(vec![if index % 2 == 0 { "eu" } else { "us" }.into()])
            })
            .collect();
        let mut model = group(declarations.clone(), None);
        Arc::make_mut(&mut model)
            .routing
            .as_mut()
            .unwrap()
            .when_all_unavailable = Some(WhenAllUnavailablePolicy::TryAnyway);
        let registry = RoutingRegistry::new();
        let pricing = LivePricingIndex::new();
        for cooldown in [0usize, 1, count] {
            let status = crate::ModelRuntimeStatusTracker::new();
            for index in 0..cooldown {
                status.mark_cooldown(
                    &format!("weighted-id-{index}"),
                    Duration::from_secs(600),
                    "oracle-cooldown",
                );
            }
            for side in [0usize, 1] {
                let tags = vec![if side == 0 { "eu" } else { "us" }.to_string()];
                let kept: Vec<_> = declarations
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| {
                        index % 2 == side && (cooldown == count || *index >= cooldown)
                    })
                    .map(|(_, target)| target.clone())
                    .collect();
                for index in 0..64 {
                    let key = format!("weighted-key-{index}");
                    let actual = resolve_attempt_models(
                        &registry,
                        &status,
                        &pricing,
                        &snapshot,
                        "weighted-group",
                        "weighted-id",
                        &model,
                        RoutingRequest {
                            tags: &tags,
                            api_key_id: &key,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    assert_eq!(
                        actual
                            .iter()
                            .map(|attempt| attempt.model.display_name.as_str())
                            .collect::<Vec<_>>(),
                        legacy_order(&declarations, &kept, &key),
                        "count={count} cooldown={cooldown} side={side} key={key}"
                    );
                }
            }
        }
    }
}

#[test]
fn configured_hash_ring_storage_stays_bounded_by_declarations() {
    // 缓存改回按请求子集累积，或为每个点重复索引，都会突破配置域的点数/条目上界。
    let snapshot = AisixSnapshot::new();
    let declarations = (0..128)
        .map(|index| {
            let name = format!("bounded-{index}");
            put(&snapshot, &format!("bounded-id-{index}"), &name);
            RoutingTarget::new(name)
                .with_weight(index as u32 + 1)
                .with_tags(vec![format!("subset-{}", index % 16)])
        })
        .collect();
    let model = group(declarations, None);
    let registry = RoutingRegistry::new();
    let status = crate::ModelRuntimeStatusTracker::new();
    let pricing = LivePricingIndex::new();
    for subset in 0..16 {
        let tags = vec![format!("subset-{subset}")];
        let actual = resolve_attempt_models(
            &registry,
            &status,
            &pricing,
            &snapshot,
            "bounded-group",
            "bounded-id",
            &model,
            RoutingRequest {
                tags: &tags,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(actual.len(), 8);
    }
    assert_eq!(registry.rings.len(), 1);
    let ring = registry
        .rings
        .get(&("bounded-group".to_string(), 0))
        .unwrap();
    assert!(ring.points.len() <= 160 * (64 + 128));
    assert_eq!(ring.point_positions.len(), ring.points.len());
    assert_eq!(ring.declaration_offsets.len(), 129);
}

#[test]
fn small_filtered_hash_successor_queries_allocate_only_the_returned_order() {
    // 把 successor scratch 改成 Vec，会超过实际分配计数上限。
    let declarations: Vec<_> = (0..16)
        .map(|index| RoutingTarget::new(format!("alloc-{index}")))
        .collect();
    let ring = HashRing::build(0, &declarations, RingScale::of(declarations.iter()));
    for count in [1usize, 2, 8] {
        let subset: Vec<_> = (0..count).map(|index| index * 2).collect();
        let scope = AllocationScope::start();
        let order = ring.subset_preference_order(12345, &subset);
        let allocations = scope.finish();
        assert_eq!(order.len(), count);
        assert_eq!(allocations.calls, 1, "count={count}: {allocations:?}");
    }
}

#[test]
fn small_warm_hash_pickers_add_no_id_name_or_mapping_scratch_allocations() {
    // ID 名称 clone、声明映射 Vec 或 successor Vec 都会超过名称引用的实际分配预算。
    // 规范化 survivors 在计数范围外构造，范围只覆盖环查找、配置声明匹配和查询。
    for count in [1usize, 2, 8] {
        let snapshot = AisixSnapshot::new();
        let names: Vec<_> = (0..count * 2)
            .map(|index| {
                let name = format!("picker-{index}");
                put(&snapshot, &format!("picker-id-{index}"), &name);
                RoutingTarget::new(name)
            })
            .collect();
        let ids: Vec<_> = (0..count * 2)
            .map(|index| RoutingTarget::by_id(format!("picker-id-{index}")))
            .collect();
        let available: Vec<_> = names.iter().step_by(2).cloned().collect();
        let named_model = group(names, None);
        let id_model = group(ids, None);
        let registry = RoutingRegistry::new();
        let mut expected = Vec::new();
        let mut reference_calls = None;
        for (label, model) in [("names", named_model), ("ids", id_model)] {
            let routing = model.routing.as_ref().unwrap();
            let warm = registry.pick_configured_hash_indices(
                &snapshot,
                label,
                routing,
                &available,
                "allocator-key",
            );
            if label == "names" {
                expected = warm;
            } else {
                assert_eq!(warm, expected);
            }
            let scope = AllocationScope::start();
            let actual = registry.pick_configured_hash_indices(
                &snapshot,
                label,
                routing,
                &available,
                "allocator-key",
            );
            let allocated = scope.finish();
            assert_eq!(actual, expected);
            assert_eq!(actual.len(), count);
            let budget = if count == 1 { 1 } else { 5 };
            assert!(
                allocated.calls <= budget,
                "count={count} label={label} budget={budget}: {allocated:?}"
            );
            if let Some(calls) = reference_calls {
                assert_eq!(
                    allocated.calls, calls,
                    "count={count}: ID added allocations"
                );
            } else {
                reference_calls = Some(allocated.calls);
            }
        }
    }
}
