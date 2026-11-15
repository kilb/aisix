//! 空闲槽位在既有轮询节奏补位；领取中的请求不暂停其他对象的读取和续租。

use std::future::Future;
use std::time::Duration;

use aisix_core::native_objects::{ClaimRequest, NativeRecord, NativeReply};
use futures::future::{pending, BoxFuture};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::sync::watch;

use super::registry::{NativeObjectRegistry, NativeRegistryError};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(super) async fn run<F, P>(
    registry: &dyn NativeObjectRegistry,
    request: ClaimRequest,
    mut poll: F,
    mut cancel: watch::Receiver<bool>,
) where
    F: FnMut(NativeRecord) -> P,
    P: Future<Output = Result<bool, NativeRegistryError>>,
{
    if *cancel.borrow() {
        return;
    }
    let capacity = request.limit as usize;
    let mut active = FuturesUnordered::new();
    let mut claiming: Option<BoxFuture<'_, Result<NativeReply, NativeRegistryError>>> = None;
    let mut claimed_limit = 0;
    let mut ticks = tokio::time::interval(POLL_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return;
                }
            }
            Some(result) = active.next(), if !active.is_empty() => {
                if result.is_err() {
                    tracing::warn!("native observation will resume after its lease");
                }
            }
            result = async {
                match claiming.as_mut() {
                    Some(claim) => claim.await,
                    None => pending().await,
                }
            }, if claiming.is_some() => {
                claiming = None;
                match result {
                    Ok(reply) if reply.state == "leased" && reply.records.len() <= claimed_limit => {
                        for record in reply.records {
                            active.push(poll(record));
                        }
                    }
                    _ => tracing::warn!("native observation authority unavailable"),
                }
            }
            _ = ticks.tick() => {
                // 一个领取请求预留全部空位；它返回前只会释放槽位，不会再次领取。
                if claiming.is_none() && active.len() < capacity {
                    let mut next = request.clone();
                    claimed_limit = capacity - active.len();
                    next.limit = claimed_limit as u32;
                    claiming = Some(registry.claim(next));
                }
            }
        }
    }
}
