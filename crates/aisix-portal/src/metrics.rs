//! 门户自己的指标。
//!
//! 为什么需要：对账环那几个计数（入账多少人、写盘失败几次、读指标失败几次）
//! 原本只写到 stderr。仓库的规矩是「没有 e2e 能在 `/metrics` 里断言到的指标等于
//! 不存在」—— 一个只进日志的失败计数，在监控看来跟「一切正常」没有区别，而
//! 「额度从此推不下去」正是要靠它才能发现的那种故障。
//!
//! 为什么手写而不是引指标库：要暴露的就是几个单调计数器，没有标签、没有直方图。
//! 手写省掉一个依赖，也省掉「注册表在哪、谁来渲染」这套问题。
//!
//! 监听地址与门户主端口分开（`PORTAL_METRICS_ADDR`，默认 `127.0.0.1:8092`），
//! 这样它不经 nginx 暴露 —— 里面是运维数据，不该出现在租户能打到的地方。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// 单调计数器的集合。克隆共享同一份底层计数。
#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    ticks: AtomicU64,
    tick_errors: AtomicU64,
    debited: AtomicU64,
    keys_disabled: AtomicU64,
    keys_reenabled: AtomicU64,
    read_failures: AtomicU64,
    write_failures: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一轮成功的对账。
    pub fn record_tick(&self, r: &crate::sweeper::TickReport) {
        let i = &self.inner;
        i.ticks.fetch_add(1, Ordering::Relaxed);
        i.debited.fetch_add(r.debited as u64, Ordering::Relaxed);
        i.keys_disabled
            .fetch_add(r.disabled as u64, Ordering::Relaxed);
        i.keys_reenabled
            .fetch_add(r.reenabled as u64, Ordering::Relaxed);
        i.read_failures
            .fetch_add(r.read_failures as u64, Ordering::Relaxed);
        i.write_failures
            .fetch_add(r.write_failures as u64, Ordering::Relaxed);
    }

    /// 记一轮整体失败的对账（`tick` 自己返回了错）。
    pub fn record_tick_error(&self) {
        self.inner.tick_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Prometheus 文本格式。
    pub fn render(&self) -> String {
        let i = &self.inner;
        let mut out = String::new();
        for (name, help, value) in [
            (
                "aisix_portal_reconcile_ticks_total",
                "对账环跑完的轮数",
                i.ticks.load(Ordering::Relaxed),
            ),
            (
                "aisix_portal_reconcile_errors_total",
                "对账环整轮失败的次数",
                i.tick_errors.load(Ordering::Relaxed),
            ),
            (
                "aisix_portal_debited_users_total",
                "被记入消费的用户次数",
                i.debited.load(Ordering::Relaxed),
            ),
            (
                "aisix_portal_keys_disabled_total",
                "因额度用尽或账号停用而被停用的密钥次数",
                i.keys_disabled.load(Ordering::Relaxed),
            ),
            (
                "aisix_portal_keys_reenabled_total",
                "补额后被重新启用的密钥次数",
                i.keys_reenabled.load(Ordering::Relaxed),
            ),
            (
                "aisix_portal_metric_read_failures_total",
                "读花费指标失败、水位线未前进的次数",
                i.read_failures.load(Ordering::Relaxed),
            ),
            (
                "aisix_portal_config_write_failures_total",
                "写网关配置失败的次数（额度与启停都推不下去）",
                i.write_failures.load(Ordering::Relaxed),
            ),
        ] {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        }
        out
    }
}
