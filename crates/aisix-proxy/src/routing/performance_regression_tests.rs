use super::*;
use aisix_core::ResourceEntry;
use dashmap::try_result::TryResult;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::sync::{Arc, mpsc};

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
    static METRIC_READS: Cell<MetricReads> = Cell::new(MetricReads::default());
    static AFFINITY_PROBES: Cell<usize> = const { Cell::new(0) };
    static WRR_PAUSE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

pub(crate) fn note_metric_read(kind: MetricRead) {
    METRIC_READS.with(|slot| {
        let mut reads = slot.get();
        match kind {
            MetricRead::Busy => reads.busy += 1,
            MetricRead::Latency => reads.latency += 1,
            MetricRead::Penalty => reads.penalty += 1,
        }
        slot.set(reads);
    });
}

pub(crate) fn note_affinity_probe() {
    AFFINITY_PROBES.with(|slot| slot.set(slot.get() + 1));
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
fn affinity_restoration_has_linear_membership_work() {
    // 恢复逐项线性查重会把计数变成约 N²/2；错误地删掉回补则选不到最后的来源。
    for count in [8usize, 64, 128, 256] {
        let (snapshot, model) = fixture(count, RoutingStrategy::Failover, Some(0));
        AFFINITY_PROBES.with(|slot| slot.set(0));
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
        let probes = AFFINITY_PROBES.with(Cell::get);
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
            METRIC_READS.with(|slot| slot.set(MetricReads::default()));
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
            let reads = METRIC_READS.with(Cell::get);
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
fn wrr_warm_colliding_groups_progress_independently() {
    // A 真正进入 WRR lookup 后暂停；暖组 B 若仍需该 shard 写锁就不能先完成。
    let registry = Arc::new(RoutingRegistry {
        wrr: DashMap::with_shard_amount(2),
        rings: DashMap::new(),
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
