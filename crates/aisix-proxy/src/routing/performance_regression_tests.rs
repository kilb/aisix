use super::*;
use aisix_core::ResourceEntry;
use aisix_response_reference_test_alloc::AllocationScope;
use dashmap::try_result::TryResult;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::sync::{mpsc, Arc};

#[derive(Clone, Copy, Default, Debug)]
struct MetricReads {
    busy: usize,
    latency: usize,
    penalty: usize,
}

pub(crate) enum MetricRead {
    Busy,
    Latency,
    Penalty,
}

thread_local! {
    static METRIC_READS: Cell<Option<MetricReads>> = const { Cell::new(None) };
    static AFFINITY_PROBES: Cell<Option<usize>> = const { Cell::new(None) };
    static WRR_PAUSE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    static RING_BUILDS: Cell<Option<usize>> = const { Cell::new(None) };
    static CONFIGURED_ID_LOOKUPS: Cell<Option<usize>> = const { Cell::new(None) };
}

pub(crate) fn note_configured_id_lookup() {
    CONFIGURED_ID_LOOKUPS.with(|slot| {
        if let Some(lookups) = slot.get() {
            slot.set(Some(lookups + 1));
        }
    });
}

pub(crate) fn note_ring_build() {
    RING_BUILDS.with(|slot| {
        if let Some(builds) = slot.get() {
            slot.set(Some(builds + 1));
        }
    });
}

pub(crate) fn note_metric_read(kind: MetricRead) {
    METRIC_READS.with(|slot| {
        let Some(mut reads) = slot.get() else { return };
        match kind {
            MetricRead::Busy => reads.busy += 1,
            MetricRead::Latency => reads.latency += 1,
            MetricRead::Penalty => reads.penalty += 1,
        }
        slot.set(Some(reads));
    });
}

pub(crate) fn note_affinity_probe() {
    AFFINITY_PROBES.with(|slot| {
        if let Some(probes) = slot.get() {
            slot.set(Some(probes + 1));
        }
    });
}

pub(crate) fn pause_wrr_after_lookup() {
    let pause = WRR_PAUSE.with(|slot| slot.borrow_mut().take());
    if let Some(pause) = pause {
        pause();
    }
}

fn put(snapshot: &AisixSnapshot, index: usize) {
    let model = serde_json::from_value(json!({
        "display_name": format!("target-{index}"),
        "provider": "openai", "model_name": "upstream",
        "provider_key_id": format!("pk-{index}"),
    }))
    .unwrap();
    snapshot
        .models
        .insert(ResourceEntry::new(format!("id-{index}"), model, 1));
}

fn fixture(
    count: usize,
    strategy: RoutingStrategy,
    limit: Option<u32>,
) -> (AisixSnapshot, Arc<Model>) {
    let snapshot = AisixSnapshot::new();
    for index in 0..count {
        put(&snapshot, index);
    }
    let targets: Vec<_> = (0..count)
        .map(|index| RoutingTarget::new(format!("target-{index}")))
        .collect();
    let model = serde_json::from_value(json!({
        "display_name": "group",
        "routing": {"strategy": strategy, "targets": targets, "max_fallbacks": limit},
    }))
    .unwrap();
    (snapshot, Arc::new(model))
}

#[test]
fn alternating_filtered_hash_subsets_do_not_rebuild_warm_rings() {
    // 恢复按请求子集覆盖缓存会在热身后重建 1000 次；删掉实际建环也会让热身计数失败。
    for count in [4usize, 16, 128] {
        let (snapshot, mut model) = fixture(count, RoutingStrategy::ConsistentHash, None);
        let routing = Arc::make_mut(&mut model).routing.as_mut().unwrap();
        for (index, target) in routing.targets.iter_mut().enumerate() {
            target.tags = Some(vec![if index % 2 == 0 { "eu" } else { "us" }.into()]);
        }
        let registry = RoutingRegistry::new();
        let status = crate::ModelRuntimeStatusTracker::new();
        let pricing = LivePricingIndex::new();
        let tags = [vec!["eu".to_string()], vec!["us".to_string()]];
        let invoke = |side: usize| {
            resolve_attempt_models(
                &registry,
                &status,
                &pricing,
                &snapshot,
                "group",
                "group-id",
                &model,
                RoutingRequest {
                    tags: &tags[side],
                    api_key_id: "stable-caller",
                    ..Default::default()
                },
            )
            .unwrap()
        };
        RING_BUILDS.with(|slot| slot.set(Some(0)));
        let warm = [invoke(0), invoke(1)];
        assert!(RING_BUILDS.with(|slot| slot.get().unwrap()) > 0);
        RING_BUILDS.with(|slot| slot.set(Some(0)));
        for request in 0..1000 {
            let side = request % 2;
            let actual = invoke(side);
            assert_eq!(actual.len(), count / 2);
            let ids: Vec<_> = actual.iter().map(|attempt| attempt.id.as_str()).collect();
            let expected: Vec<_> = warm[side]
                .iter()
                .map(|attempt| attempt.id.as_str())
                .collect();
            assert_eq!(ids, expected);
            assert!(ids.iter().all(|id| {
                id.strip_prefix("id-").unwrap().parse::<usize>().unwrap() % 2 == side
            }));
        }
        assert_eq!(
            RING_BUILDS.with(|slot| slot.take().unwrap()),
            0,
            "count={count}: warm filtered subsets rebuilt their rings"
        );
    }
}

#[test]
fn warm_hash_subsets_do_not_resolve_configured_model_ids_again() {
    // 恢复每次完整配置 ID 解析会产生 D×requests 次真实 lookup；热身计数也必须为正。
    for count in [4usize, 8, 128] {
        let (snapshot, mut model) = fixture(count, RoutingStrategy::ConsistentHash, None);
        for (index, target) in Arc::make_mut(&mut model)
            .routing
            .as_mut()
            .unwrap()
            .targets
            .iter_mut()
            .enumerate()
        {
            target.model.clear();
            target.model_id = Some(format!("id-{index}"));
            target.tags = Some(vec![if index >= 4 {
                "other"
            } else if index % 2 == 0 {
                "eu"
            } else {
                "us"
            }
            .into()]);
        }
        let registry = RoutingRegistry::new();
        let status = crate::ModelRuntimeStatusTracker::new();
        let pricing = LivePricingIndex::new();
        let tags = [vec!["eu".to_string()], vec!["us".to_string()]];
        let invoke = |side: usize| {
            resolve_attempt_models(
                &registry,
                &status,
                &pricing,
                &snapshot,
                "lookup-group",
                "lookup-id",
                &model,
                RoutingRequest {
                    tags: &tags[side],
                    api_key_id: "same-key",
                    ..Default::default()
                },
            )
            .unwrap()
        };
        CONFIGURED_ID_LOOKUPS.with(|slot| slot.set(Some(0)));
        assert_eq!(invoke(0).len(), 2);
        assert_eq!(invoke(1).len(), 2);
        assert!(CONFIGURED_ID_LOOKUPS.with(|slot| slot.get().unwrap()) > 0);
        for alternating in [false, true] {
            CONFIGURED_ID_LOOKUPS.with(|slot| slot.set(Some(0)));
            for request in 0..32 {
                let side = usize::from(alternating && request % 2 == 1);
                let actual = invoke(side);
                assert_eq!(actual.len(), 2);
                assert!(actual.iter().all(|attempt| {
                    attempt
                        .id
                        .strip_prefix("id-")
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                        % 2
                        == side
                }));
            }
            assert_eq!(
                CONFIGURED_ID_LOOKUPS.with(|slot| slot.take().unwrap()),
                0,
                "count={count} alternating={alternating}: warm routing re-resolved configured IDs"
            );
        }
        // 模型表更新但引用名字不变时，只核对一次，随后仍为零 lookup，也不重建点。
        put(&snapshot, count + 1000);
        CONFIGURED_ID_LOOKUPS.with(|slot| slot.set(Some(0)));
        RING_BUILDS.with(|slot| slot.set(Some(0)));
        assert_eq!(invoke(0).len(), 2);
        assert!(CONFIGURED_ID_LOOKUPS.with(|slot| slot.take().unwrap()) > 0);
        assert_eq!(RING_BUILDS.with(|slot| slot.take().unwrap()), 0);
        CONFIGURED_ID_LOOKUPS.with(|slot| slot.set(Some(0)));
        assert_eq!(invoke(1).len(), 2);
        assert_eq!(CONFIGURED_ID_LOOKUPS.with(|slot| slot.take().unwrap()), 0);
    }
}

#[test]
fn filtered_hash_stable_and_alternating_resolution_probe() {
    // 记录真实 resolver 的时间和分配；比较固定子集与交错子集，不能只报告后者的收益。
    let by_id = std::env::var("AISIX_FILTERED_HASH_BENCH_BY_ID").as_deref() == Ok("1");
    let repetitions = std::env::var("AISIX_FILTERED_HASH_BENCH_REPS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(16);
    for count in [4usize, 8, 128] {
        for sparse in [false, true] {
            if sparse && count == 4 {
                continue;
            }
            for alternating in [false, true] {
                for weighted in [false, true] {
                    for limit in [Some(0), None] {
                        let (snapshot, mut model) =
                            fixture(count, RoutingStrategy::ConsistentHash, limit);
                        let routing = Arc::make_mut(&mut model).routing.as_mut().unwrap();
                        for (index, target) in routing.targets.iter_mut().enumerate() {
                            if by_id {
                                target.model.clear();
                                target.model_id = Some(format!("id-{index}"));
                            }
                            target.tags = Some(vec![if sparse && index >= 4 {
                                "other"
                            } else if index % 2 == 0 {
                                "eu"
                            } else {
                                "us"
                            }
                            .into()]);
                            if weighted {
                                target.weight = Some((index % 7 + 1) as u32);
                            }
                        }
                        let registry = RoutingRegistry::new();
                        let status = crate::ModelRuntimeStatusTracker::new();
                        let pricing = LivePricingIndex::new();
                        let tags = [vec!["eu".to_string()], vec!["us".to_string()]];
                        let invoke = |side: usize| {
                            resolve_attempt_models(
                                &registry,
                                &status,
                                &pricing,
                                &snapshot,
                                "probe-group",
                                "probe-id",
                                &model,
                                RoutingRequest {
                                    tags: &tags[side],
                                    api_key_id: "probe-key",
                                    ..Default::default()
                                },
                            )
                            .unwrap()
                        };
                        let survivors = if sparse { 2 } else { count / 2 };
                        let expected = if limit == Some(0) { 1 } else { survivors };
                        assert_eq!(invoke(1).len(), expected);
                        assert_eq!(invoke(0).len(), expected);
                        let scope = AllocationScope::start();
                        let measured = invoke(usize::from(alternating));
                        let allocated = scope.finish();
                        assert_eq!(measured.len(), expected);
                        let mut samples = Vec::new();
                        for _ in 0..7 {
                            let started = std::time::Instant::now();
                            let mut resolved = 0;
                            for request in 0..repetitions {
                                let side = usize::from(alternating && request % 2 == 1);
                                resolved += std::hint::black_box(invoke(side)).len();
                            }
                            samples
                                .push(started.elapsed().as_secs_f64() * 1e9 / repetitions as f64);
                            assert_eq!(resolved, repetitions * expected);
                        }
                        eprintln!(
                            "{}",
                            json!({
                                "case":"filtered_hash_resolution", "configured":count,
                            "model_id_references":by_id,
                                "survivors":survivors, "alternating":alternating,
                                "weighted":weighted, "max_fallbacks":limit,
                                "repetitions_per_sample":repetitions, "samples_ns_per_call":samples,
                                "allocation_calls":allocated.calls, "allocated_bytes":allocated.bytes,
                            })
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn affinity_restoration_has_linear_membership_work() {
    // 恢复逐项线性查重会把计数变成约 N²/2；错误地删掉回补则选不到最后的来源。
    for count in [8usize, 64, 128, 256] {
        let (snapshot, model) = fixture(count, RoutingStrategy::Failover, Some(0));
        AFFINITY_PROBES.with(|slot| slot.set(Some(0)));
        let origin = format!("id-{}", count - 1);
        let attempts = resolve_attempt_models(
            &RoutingRegistry::new(),
            &crate::ModelRuntimeStatusTracker::new(),
            &LivePricingIndex::new(),
            &snapshot,
            "group",
            "group-id",
            &model,
            RoutingRequest {
                context_origin: Some(&origin),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, origin);
        let probes = AFFINITY_PROBES.with(|slot| slot.take().unwrap());
        assert!(probes > 0, "亲和性恢复计数没有覆盖实际工作");
        assert!(
            probes <= 2 * count,
            "affinity membership count={count} probes={probes}"
        );
    }
}

#[test]
fn metric_ranking_reads_each_runtime_key_at_most_once() {
    // 相异乱序指标不能走全相等快路；恢复比较器内读状态时，真实 getter 计数会超界。
    for strategy in [RoutingStrategy::LeastLatency, RoutingStrategy::LeastBusy] {
        for count in [2usize, 8, 128] {
            let (snapshot, model) = fixture(count, strategy, None);
            let status = crate::ModelRuntimeStatusTracker::new();
            let mut holds = Vec::new();
            for index in 0..count {
                let load = index * 37 % count;
                let id = format!("id-{index}");
                status.record_latency(&id, load as u32 + 1);
                for _ in 0..load {
                    holds.push(status.count_in_flight(&id));
                }
            }
            METRIC_READS.with(|slot| slot.set(Some(MetricReads::default())));
            let attempts = resolve_attempt_models(
                &RoutingRegistry::new(),
                &status,
                &LivePricingIndex::new(),
                &snapshot,
                "group",
                "group-id",
                &model,
                RoutingRequest::default(),
            )
            .unwrap();
            // 37 在 2/8/128 上的逆元分别为 1/5/45，独立于排序实现推导。
            let inverse = match count {
                2 => 1,
                8 => 5,
                128 => 45,
                _ => unreachable!(),
            };
            let expected: Vec<_> = (0..count)
                .map(|load| format!("id-{}", load * inverse % count))
                .collect();
            let ids: Vec<_> = attempts.iter().map(|a| a.id.as_str()).collect();
            assert_eq!(ids, expected);
            let reads = METRIC_READS.with(|slot| slot.take().unwrap());
            match strategy {
                RoutingStrategy::LeastLatency => {
                    assert_eq!(reads.penalty, count, "count={count}, reads={reads:?}");
                    assert_eq!(reads.latency, count, "count={count}, reads={reads:?}");
                    assert_eq!(reads.busy, 0);
                }
                RoutingStrategy::LeastBusy => {
                    assert_eq!(reads.busy, count, "count={count}, reads={reads:?}");
                    assert_eq!(reads.penalty + reads.latency, 0);
                }
                _ => unreachable!(),
            }
            drop(holds);
        }
    }
}

#[test]
fn small_metric_ranking_adds_no_heap_allocations() {
    // 把小组也改成缓存键 Vec 会触发真实 allocator 计数，不能由大组收益掩盖。
    for strategy in [RoutingStrategy::LeastLatency, RoutingStrategy::LeastBusy] {
        for count in [1usize, 2, 8] {
            let (snapshot, _) = fixture(count, strategy, None);
            let status = crate::ModelRuntimeStatusTracker::new();
            let mut attempts: Vec<_> = (0..count)
                .map(|index| AttemptModel {
                    id: format!("id-{index}"),
                    model: snapshot
                        .models
                        .get_by_id(&format!("id-{index}"))
                        .unwrap()
                        .value
                        .clone(),
                    priority: 0,
                    weight: 1,
                })
                .collect();
            let pricing = LivePricingIndex::new();
            let scope = AllocationScope::start();
            order_attempts_by_metric(strategy, &mut attempts, &status, &snapshot, &pricing);
            let allocated = scope.finish();
            assert_eq!(
                allocated.calls, 0,
                "strategy={strategy:?} count={count} allocations={allocated:?}"
            );
            let expected: Vec<_> = (0..count).map(|index| format!("id-{index}")).collect();
            let ids: Vec<_> = attempts.iter().map(|a| a.id.as_str()).collect();
            assert_eq!(ids, expected);
        }
    }
}

#[test]
fn latency_ranking_preserves_unknown_and_penalized_ties() {
    // 两条分支都必须保留未采样探测优先、惩罚末位和两类 tie 的声明顺序。
    for (count, expected) in [
        (
            8usize,
            vec![
                "id-0", "id-1", "id-4", "id-6", "id-7", "id-5", "id-3", "id-2",
            ],
        ),
        (
            16usize,
            vec![
                "id-0", "id-1", "id-4", "id-6", "id-7", "id-8", "id-11", "id-12", "id-13", "id-14",
                "id-15", "id-9", "id-5", "id-3", "id-2", "id-10",
            ],
        ),
    ] {
        let (snapshot, model) = fixture(count, RoutingStrategy::LeastLatency, None);
        let status = crate::ModelRuntimeStatusTracker::new();
        status.record_latency("id-3", 90);
        status.record_latency("id-5", 60);
        status.penalize_latency("id-2");
        if count == 16 {
            status.record_latency("id-9", 30);
            status.penalize_latency("id-10");
        }
        let attempts = resolve_attempt_models(
            &RoutingRegistry::new(),
            &status,
            &LivePricingIndex::new(),
            &snapshot,
            "group",
            "group-id",
            &model,
            RoutingRequest::default(),
        )
        .unwrap();
        let ids: Vec<_> = attempts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, expected);
    }
}

#[test]
fn wrr_warm_colliding_groups_progress_independently() {
    // A 真正进入 WRR lookup 后暂停；暖组 B 若仍需该 shard 写锁就不能先完成。
    let registry = Arc::new(RoutingRegistry {
        wrr: DashMap::with_shard_amount(2),
        rings: DashMap::new(),
        ..RoutingRegistry::default()
    });
    let targets = vec![RoutingTarget::new("a"), RoutingTarget::new("b")];
    for index in 0..128 {
        registry.wrr_pick(&format!("group-{index}"), 0, 7, targets.iter());
    }
    let name_a = "group-0".to_string();
    let key_a = (name_a.clone(), 0);
    let guard = registry.wrr.get_mut(&key_a).unwrap();
    let name_b = (1..128)
        .map(|index| format!("group-{index}"))
        .find(|name| matches!(registry.wrr.try_get(&(name.clone(), 0)), TryResult::Locked))
        .expect("两 shard 上的测试组没有找到碰撞");
    drop(guard);

    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let a_registry = Arc::clone(&registry);
    let a_targets = targets.clone();
    let worker_a = std::thread::spawn(move || {
        WRR_PAUSE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }));
        });
        a_registry.wrr_pick(&name_a, 0, 7, a_targets.iter())
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let worker_b = std::thread::spawn(move || {
        done_tx
            .send(registry.wrr_pick(&name_b, 0, 7, targets.iter()))
            .unwrap();
    });
    let completed = done_rx.recv_timeout(Duration::from_secs(2));
    // 先清理线程再断言，即使旧实现 RED 也不会留下挂起线程。
    release_tx.send(()).unwrap();
    assert_eq!(worker_a.join().unwrap(), 1);
    worker_b.join().unwrap();
    assert_eq!(completed, Ok(1), "不同组仍被 A 持有的 shard guard 串行化");
}
