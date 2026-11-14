//! 原生对象的持久归属、租约和终态发件箱，均由真实 Postgres 负责。

pub(crate) mod store;

use std::time::Duration;

use futures::FutureExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::AppState;

pub const POOL_SIZE: u32 = 3;
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);
pub const LEASE_SECONDS: i64 = 30;
pub const MAX_PAGE: usize = 100;
pub const MAX_ENTRIES: usize = 1000;

/// 发件箱确认和现有用量接收在同一事务，进程重启无需重新推断归属。
pub fn spawn(state: AppState, mut stopping: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = crate::stopped(&mut stopping) => break,
                _ = tick.tick() => {}
            }
            let state = state.clone();
            let db = state.native_objects.clone();
            let result = db
                .tx(move |conn| async move { store::deliver_one(conn, &state).await }.boxed())
                .await;
            if result.is_err() {
                tracing::warn!("native terminal delivery will be retried");
            }
        }
    })
}
