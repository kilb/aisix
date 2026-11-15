//! 调度组件测试只控制归属接口的等待，不连接上游或数据库。

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ProxyConfig};
use aisix_gateway::Hub;
use futures::future::BoxFuture;
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::registry::{NativeObjectRegistry, NativeRegistryError};
use crate::state::ProxyState;

#[derive(Clone, Copy, Debug)]
enum Family {
    Responses,
    OpenaiBatch,
    AnthropicBatch,
}

const FAMILIES: [Family; 3] = [
    Family::Responses,
    Family::OpenaiBatch,
    Family::AnthropicBatch,
];

impl Family {
    fn protocol(self) -> NativeProtocol {
        match self {
            Self::AnthropicBatch => NativeProtocol::Anthropic,
            _ => NativeProtocol::Openai,
        }
    }

    fn kind(self) -> NativeObjectKind {
        match self {
            Self::Responses => NativeObjectKind::Response,
            _ => NativeObjectKind::MessageBatch,
        }
    }

    fn spawn(self, registry: Arc<Registry>) -> (watch::Sender<bool>, JoinHandle<()>) {
        let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
        let state = ProxyState::new(
            SnapshotHandle::new(AisixSnapshot::new()),
            Arc::new(Hub::new()),
            &config,
        )
        .with_native_objects(registry);
        let (cancel, receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            match self {
                Self::Responses => super::background::run(state, receiver).await,
                Self::OpenaiBatch => super::openai_batch_worker::run(state, receiver).await,
                Self::AnthropicBatch => super::anthropic_batch_worker::run(state, receiver).await,
            }
        });
        (cancel, task)
    }
}

fn id(index: usize) -> Uuid {
    Uuid::from_u128(index as u128)
}

struct Registry {
    pending: Mutex<VecDeque<NativeRecord>>,
    claims: Mutex<Vec<ClaimRequest>>,
    started: Mutex<BTreeSet<Uuid>>,
    finished: Mutex<BTreeSet<Uuid>>,
    held: BTreeSet<Uuid>,
    released: Mutex<BTreeSet<Uuid>>,
    poll_changes: watch::Sender<usize>,
    claim_release: watch::Sender<bool>,
    hold_later_claims: bool,
    fail_claims: bool,
    active: AtomicUsize,
    peak_active: AtomicUsize,
    claiming: AtomicUsize,
    peak_claiming: AtomicUsize,
}

impl Registry {
    fn new(family: Family, count: usize, held: impl IntoIterator<Item = usize>) -> Self {
        // 缺少原账号绑定会进入原有 reschedule 路径，调度测试不触发付费上游请求。
        let pending = (1..=count)
            .map(|index| {
                serde_json::from_value(json!({
                    "record_id":id(index), "raw_id":format!("native-{index}"),
                    "protocol":family.protocol(), "kind":family.kind(),
                    "lease_generation":1,
                    "lease_expires_at":chrono::Utc::now() + chrono::Duration::seconds(30)
                }))
                .unwrap()
            })
            .collect();
        Self {
            pending: Mutex::new(pending),
            claims: Mutex::new(Vec::new()),
            started: Mutex::new(BTreeSet::new()),
            finished: Mutex::new(BTreeSet::new()),
            held: held.into_iter().map(id).collect(),
            released: Mutex::new(BTreeSet::new()),
            poll_changes: watch::channel(0).0,
            claim_release: watch::channel(false).0,
            hold_later_claims: false,
            fail_claims: false,
            active: AtomicUsize::new(0),
            peak_active: AtomicUsize::new(0),
            claiming: AtomicUsize::new(0),
            peak_claiming: AtomicUsize::new(0),
        }
    }

    fn release(&self, index: usize) {
        self.released.lock().unwrap().insert(id(index));
        self.poll_changes.send_modify(|generation| *generation += 1);
    }

    fn claim_count(&self) -> usize {
        self.claims.lock().unwrap().len()
    }

    fn assert_claim(&self, index: usize, family: Family, limit: u32) {
        let claims = self.claims.lock().unwrap();
        assert!(
            claims.len() > index,
            "{family:?}: claim #{} was not started by the required tick; expected limit {limit}, observed {} claims",
            index + 1,
            claims.len()
        );
        let request = &claims[index];
        assert_eq!(request.limit, limit, "{family:?}");
        assert_eq!(request.protocol, Some(family.protocol()), "{family:?}");
        assert_eq!(request.kind, Some(family.kind()), "{family:?}");
        assert!(request.record_id.is_none(), "{family:?}");
    }
}

struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn new(active: &'a AtomicUsize, peak: &AtomicUsize) -> Self {
        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(count, Ordering::SeqCst);
        Self(active)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

macro_rules! unavailable {
    ($method:ident, $request:ty) => {
        fn $method(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for Registry {
    unavailable!(begin, BeginRequest);
    unavailable!(bind, BindRequest);
    unavailable!(lookup, LookupRequest);
    unavailable!(owned, OwnedRequest);
    unavailable!(tombstone, TombstoneRequest);
    unavailable!(renew, LeaseRequest);
    unavailable!(complete, CompleteRequest);

    fn claim(
        &self,
        request: ClaimRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let _in_flight = InFlight::new(&self.claiming, &self.peak_claiming);
            let mut released = self.claim_release.subscribe();
            let index = {
                let mut claims = self.claims.lock().unwrap();
                claims.push(request.clone());
                claims.len()
            };
            while self.hold_later_claims && index > 1 && !*released.borrow() {
                released.changed().await.unwrap();
            }
            if self.fail_claims {
                return Err(NativeRegistryError::Unavailable);
            }
            let mut reply = NativeReply::new("leased");
            let mut pending = self.pending.lock().unwrap();
            for _ in 0..request.limit {
                let Some(record) = pending.pop_front() else {
                    break;
                };
                reply.records.push(record);
            }
            Ok(reply)
        })
    }

    fn reschedule(
        &self,
        request: LeaseRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let _in_flight = InFlight::new(&self.active, &self.peak_active);
            let mut changes = self.poll_changes.subscribe();
            let record_id = request.lease.record_id;
            self.started.lock().unwrap().insert(record_id);
            while self.held.contains(&record_id)
                && !self.released.lock().unwrap().contains(&record_id)
            {
                changes.changed().await.unwrap();
            }
            self.finished.lock().unwrap().insert(record_id);
            Ok(NativeReply::new("pending"))
        })
    }
}

async fn settle_tasks() {
    // 暂停时钟下只让已唤醒任务执行，不用真实睡眠或自动推进租约超时。
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

async fn tick() {
    tokio::time::advance(Duration::from_secs(2)).await;
    settle_tasks().await;
}

async fn stop(cancel: watch::Sender<bool>, task: JoinHandle<()>, registry: &Registry) {
    cancel.send(true).unwrap();
    settle_tasks().await;
    assert!(task.is_finished(), "取消不能等待观察或 claim 返回");
    task.await.unwrap();
    assert_eq!(registry.active.load(Ordering::SeqCst), 0);
    assert_eq!(registry.claiming.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn native_workers_refill_free_slots_before_the_slowest_observation_finishes() {
    // 恢复整组 join 后，第九项会一直等第一项，这个断言必须失败。
    for family in FAMILIES {
        let registry = Arc::new(Registry::new(family, 9, [1]));
        let (cancel, task) = family.spawn(registry.clone());
        settle_tasks().await;
        assert_eq!(registry.started.lock().unwrap().len(), 8, "{family:?}");
        assert_eq!(registry.finished.lock().unwrap().len(), 7, "{family:?}");
        tick().await;
        assert!(
            registry.started.lock().unwrap().contains(&id(9)),
            "{family:?}"
        );
        assert!(
            !registry.finished.lock().unwrap().contains(&id(1)),
            "{family:?}"
        );
        registry.assert_claim(0, family, 8);
        registry.assert_claim(1, family, 7);
        assert!(registry.peak_active.load(Ordering::SeqCst) <= 8);
        stop(cancel, task, &registry).await;
    }
}

#[tokio::test(start_paused = true)]
async fn native_workers_poll_active_observations_while_one_claim_waits() {
    // 在 tick 分支直接 await claim 会让第一项的已唤醒观察无法结束。
    for family in FAMILIES {
        let mut registry = Registry::new(family, 9, [1]);
        registry.hold_later_claims = true;
        let registry = Arc::new(registry);
        let (cancel, task) = family.spawn(registry.clone());
        settle_tasks().await;
        tick().await;
        assert_eq!(registry.claim_count(), 2, "{family:?}");
        assert_eq!(registry.claiming.load(Ordering::SeqCst), 1, "{family:?}");
        registry.release(1);
        settle_tasks().await;
        assert!(
            registry.finished.lock().unwrap().contains(&id(1)),
            "{family:?}"
        );
        tick().await;
        tick().await;
        assert_eq!(registry.claim_count(), 2, "{family:?}");
        assert_eq!(registry.peak_claiming.load(Ordering::SeqCst), 1);
        stop(cancel, task, &registry).await;
        assert!(!registry.started.lock().unwrap().contains(&id(9)));
    }
}

#[tokio::test(start_paused = true)]
async fn native_workers_claim_only_free_slots_and_drop_all_observations_on_cancel() {
    // 补位仍固定领取八项会同时启动十五项，不能靠队列里等待消耗租期。
    for family in FAMILIES {
        let registry = Arc::new(Registry::new(family, 16, 1..=16));
        let (cancel, task) = family.spawn(registry.clone());
        settle_tasks().await;
        assert_eq!(registry.active.load(Ordering::SeqCst), 8, "{family:?}");
        tick().await;
        assert_eq!(registry.claim_count(), 1, "{family:?}");
        registry.release(1);
        settle_tasks().await;
        assert_eq!(registry.active.load(Ordering::SeqCst), 7, "{family:?}");
        tick().await;
        registry.assert_claim(1, family, 1);
        assert_eq!(registry.active.load(Ordering::SeqCst), 8, "{family:?}");
        assert_eq!(registry.started.lock().unwrap().len(), 9, "{family:?}");
        assert_eq!(registry.peak_active.load(Ordering::SeqCst), 8, "{family:?}");
        stop(cancel, task, &registry).await;
    }
}

#[tokio::test(start_paused = true)]
async fn native_workers_keep_the_poll_interval_after_empty_or_failed_claims() {
    // 空页或失败后立即重领会忙循环，下一次两秒 tick 前不得再次调用。
    for family in FAMILIES {
        for fail_claims in [false, true] {
            let mut registry = Registry::new(family, 0, []);
            registry.fail_claims = fail_claims;
            let registry = Arc::new(registry);
            let (cancel, task) = family.spawn(registry.clone());
            settle_tasks().await;
            assert_eq!(registry.claim_count(), 1, "{family:?}");
            tokio::time::advance(Duration::from_millis(1_999)).await;
            settle_tasks().await;
            assert_eq!(registry.claim_count(), 1, "{family:?}");
            tokio::time::advance(Duration::from_millis(1)).await;
            settle_tasks().await;
            assert_eq!(registry.claim_count(), 2, "{family:?}");
            registry.assert_claim(1, family, 8);
            stop(cancel, task, &registry).await;
        }
    }
}
