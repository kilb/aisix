//! 对账控制环。
//!
//! 每轮做三件事：读这段时间的消费 → 入账 → 余额归零则停用该用户的密钥。
//!
//! # 为什么按时间窗查增量，而不是在累计值上做差
//!
//! 花费指标是 counter，而且**每个网关副本各自暴露一份**。初版想记「已计了多少
//! 钱」、每轮扣差值 —— 那是错的：任一副本重启都会让 `sum` 下陷，看起来就像
//! counter 重置，按累计额做差就会把水位线重新对齐到低点，那一刻起未入账的消费
//! 永久丢失，且毫无信号 —— 用户白得推理，账面看不出异常。
//!
//! 改成记「已经计到哪个时刻」，每轮查 `increase(...[自上次至今])`。`increase()`
//! 是**逐时间序列**处理重置后再求和的，跨副本天然安全，也没有缺口或重叠。
//!
//! # 两个不变量
//!
//! - **水位线只在入账成功后前进。** 读取失败就原地不动 —— 若此时把它推到当前
//!   时刻，这段窗口的消费就被跳过了，又是一次静默白送。
//! - **扣款与推进水位线在同一个事务里。** 中间崩了会重复扣或漏扣。这是本期
//!   唯一真正需要事务的地方（见 `ledger.rs` 里的事务边界说明）。

use chrono::{DateTime, Utc};

use crate::ledger::{Ledger, Source};
use crate::store::{Store, StoreError};

/// 默认轮询周期。超支上界 ≈ 本周期内的消费 + 在途请求，而按设计文档 §3.2，
/// 真正压住超支的是速率上限，不是这个数。
pub const DEFAULT_TICK_SECS: u64 = 15;

/// 实际周期，允许用 `PORTAL_TICK_SECS` 覆盖。
///
/// e2e 需要这个：一条完整的用例要跨「启用 → 消费 → 停用 → 补额启用」四个状态，
/// 每个都要等一轮对账。15 秒一轮的话整条链路就是一分钟起，几个等待叠起来把
/// 超时预算吃光 —— 而那种失败长得跟真 bug 一样。调快周期是让测试**更**确定，
/// 不是放宽断言。
///
/// 下限 1 秒：0 会让 `tokio::time::interval` 忙转。
pub fn tick_secs() -> u64 {
    std::env::var("PORTAL_TICK_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.max(1))
        .unwrap_or(DEFAULT_TICK_SECS)
}

/// 一轮的结果，供调用方与测试观察。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// 入账的用户数。
    pub debited: usize,
    /// 因余额耗尽被停用的用户数。
    pub disabled: usize,
    /// 因补额被重新启用的用户数。
    pub reenabled: usize,
    /// 读取失败、水位线未前进的用户数。
    pub read_failures: usize,
}

/// 消费读取器。抽成 trait 只为把 Prometheus 换成测试替身，生产实现只有一个。
#[allow(async_fn_in_trait)]
pub trait ConsumptionSource {
    /// `[from, to)` 窗口内该用户的花费（micro-USD）。`None` = 读不到。
    async fn spend_in_window(
        &self,
        user_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Option<u64>;
}

pub struct Sweeper<S: ConsumptionSource> {
    store: Store,
    ledger: Ledger,
    source: S,
    resources: crate::resources::Writer,
}

impl<S: ConsumptionSource> Sweeper<S> {
    pub fn new(store: Store, source: S, resources: crate::resources::Writer) -> Self {
        Self {
            ledger: Ledger::new(store.clone()),
            store,
            source,
            resources,
        }
    }

    /// 一轮对账。抽成公开方法而不是藏在定时器里，测试才能直接驱动它。
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<TickReport, StoreError> {
        let mut r = TickReport::default();
        let users = self.store.all_users().await?;

        for u in &users {
            let from = match self.store.counted_through(&u.id).await? {
                Some(t) => t,
                // 第一次见到这个用户：从现在起算，不去追溯注册之前的历史。
                None => {
                    self.store.set_counted_through(&u.id, now).await?;
                    continue;
                }
            };
            if now <= from {
                continue;
            }

            match self.source.spend_in_window(&u.id, from, now).await {
                Some(0) => {
                    // 没有增量也要推进水位线，否则窗口会越来越长。
                    self.store.set_counted_through(&u.id, now).await?;
                }
                Some(micro) => {
                    // 扣款与推进水位线必须一起成或一起不成。
                    self.debit_and_mark(&u.id, micro, now).await?;
                    r.debited += 1;
                }
                None => {
                    // 读不到就原地不动。宁可下一轮窗口更长（increase 照样正确），
                    // 也不能把这段跳过去。
                    r.read_failures += 1;
                }
            }
        }

        // 余额决定密钥开关。放在入账之后，用的是这一轮之后的余额。
        let mut wanted: Vec<(String, bool)> = Vec::new();
        for u in &users {
            let balance = self.ledger.balance(&u.id).await?;
            wanted.push((u.id.clone(), balance <= 0));
        }
        let changed = self.apply_key_state(&wanted).await;
        r.disabled = changed.0;
        r.reenabled = changed.1;
        Ok(r)
    }

    /// 扣款 + 推进水位线，一个事务。
    async fn debit_and_mark(
        &self,
        user_id: &str,
        micro: u64,
        through: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let user_id = user_id.to_string();
        let delta = -i64::try_from(micro).unwrap_or(i64::MAX);
        let through = through.to_rfc3339();
        // 同样用 `BEGIN IMMEDIATE`：这个事务里有两条写，多个门户实例或多轮对账
        // 重叠时，deferred 事务会在升级成写的那一刻当场失败而不是等待。
        self.store
            .immediate_tx(move |conn| {
                Box::pin(async move {
                    crate::ledger::insert_entry(
                        &mut *conn,
                        &user_id,
                        delta,
                        Source::Consumption,
                        None,
                    )
                    .await?;
                    sqlx::query(
                        "INSERT INTO consumption_mark (user_id, counted_through, updated_at)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(user_id) DO UPDATE SET counted_through = ?2, updated_at = ?3",
                    )
                    .bind(&user_id)
                    .bind(&through)
                    .bind(Utc::now().to_rfc3339())
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
    }

    /// 把期望的启停状态落到 `resources.yaml`。返回 (新停用数, 新启用数)。
    ///
    /// 走共用的 [`crate::resources::Writer`]：铸密钥也写这个文件，两条路径不
    /// 串行就会互相覆盖 —— 后写的那次带着自己读到的旧全文，把中间那次改动整段
    /// 抹掉。改动用泛型 `Value`，不经过窄结构体（那会抹掉门户不认识的字段）。
    async fn apply_key_state(&self, wanted: &[(String, bool)]) -> (usize, usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // 原子计数而不是捕获可变引用：闭包是 FnMut、可能被重试调用多次，
        // 而这个 future 会被 tokio::spawn 送到别的线程上。
        let off = Arc::new(AtomicUsize::new(0));
        let on = Arc::new(AtomicUsize::new(0));
        let (o1, o2) = (off.clone(), on.clone());
        let wanted = wanted.to_vec();

        let _ = self
            .resources
            .edit(move |doc| {
                // 每次重试从零开始数。
                o1.store(0, Ordering::SeqCst);
                o2.store(0, Ordering::SeqCst);
                let keys = crate::resources::api_keys_mut(doc);
                for k in keys.iter_mut() {
                    let Some(uid) = k.get("user_id").and_then(serde_yaml_ng::Value::as_str) else {
                        // 没有主人的密钥是运维自己用的，绝不能碰。
                        continue;
                    };
                    let Some((_, want)) = wanted.iter().find(|(id, _)| id == uid) else {
                        continue;
                    };
                    let now = k
                        .get("disabled")
                        .and_then(serde_yaml_ng::Value::as_bool)
                        .unwrap_or(false);
                    if now == *want {
                        continue;
                    }
                    if let Some(m) = k.as_mapping_mut() {
                        m.insert(
                            serde_yaml_ng::Value::from("disabled"),
                            serde_yaml_ng::Value::from(*want),
                        );
                    }
                    if *want {
                        o1.fetch_add(1, Ordering::SeqCst);
                    } else {
                        o2.fetch_add(1, Ordering::SeqCst);
                    }
                }
                o1.load(Ordering::SeqCst) > 0 || o2.load(Ordering::SeqCst) > 0
            })
            .await;

        (off.load(Ordering::SeqCst), on.load(Ordering::SeqCst))
    }
}

/// 生产实现：向 Prometheus 查 `increase()`。
///
/// 用 `increase(...[Ns])` 而不是在累计值上做差 —— 见模块注释。窗口长度由
/// 水位线到当前的间隔算出，所以停摆之后窗口自动变长，不留缺口。
pub struct PromSource {
    base: String,
    http: reqwest::Client,
}

impl PromSource {
    pub fn new(base: String) -> Self {
        Self {
            base,
            http: crate::client::outbound(),
        }
    }
}

impl ConsumptionSource for PromSource {
    async fn spend_in_window(
        &self,
        user_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Option<u64> {
        let secs = (to - from).num_seconds();
        if secs <= 0 {
            return Some(0);
        }
        let q = format!(
            "sum(increase(aisix_llm_spend_micro_usd_total{{user_id=\"{}\"}}[{secs}s]))",
            crate::usage::escape_label(user_id)
        );
        let resp = self
            .http
            .get(format!("{}/api/v1/query", self.base))
            .query(&[("query", q.as_str()), ("time", &to.timestamp().to_string())])
            .send()
            .await
            .ok()?;
        let body: serde_json::Value = resp.json().await.ok()?;
        // 读不到与读到零必须分开：前者不推进水位线，后者推进。
        let v = crate::usage::scalar_from_prom(&body)?;
        Some(v.max(0.0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use std::sync::{Arc, Mutex};

    /// 一次被问过的窗口：(用户, from, to)。
    type Window = (String, DateTime<Utc>, DateTime<Utc>);

    /// 记录被问过的窗口，并按「时间窗 → 增量」应答 —— 正是 `increase()` 的语义。
    #[derive(Default)]
    struct FakeSource {
        increase: Mutex<u64>,
        /// 指定让哪个用户的读取失败。写成按用户而不是「下一次」，因为
        /// 「下一次」会让测试依赖清扫器先访问谁 —— 那种测试无论答案如何都是
        /// 脆的（第一版就是这么挂的）。
        fail_for: Mutex<Option<String>>,
        /// 按用户记录被问过的窗口。写成一条全局列表时，`last()` 会混进另一个
        /// 用户的窗口 —— 又是一次顺序依赖，第二版就是这么挂的。
        windows: Mutex<Vec<Window>>,
    }

    impl FakeSource {
        fn set_increase(&self, v: u64) {
            *self.increase.lock().unwrap() = v;
        }
        fn fail_for(&self, user_id: &str) {
            *self.fail_for.lock().unwrap() = Some(user_id.to_string());
        }
        fn last_window_for(&self, user_id: &str) -> (DateTime<Utc>, DateTime<Utc>) {
            self.windows
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(u, _, _)| u == user_id)
                .map(|(_, f, t)| (*f, *t))
                .expect("没有该用户的窗口记录")
        }
    }

    impl ConsumptionSource for Arc<FakeSource> {
        async fn spend_in_window(
            &self,
            user_id: &str,
            from: DateTime<Utc>,
            to: DateTime<Utc>,
        ) -> Option<u64> {
            if self.fail_for.lock().unwrap().as_deref() == Some(user_id) {
                return None;
            }
            self.windows
                .lock()
                .unwrap()
                .push((user_id.to_string(), from, to));
            Some(*self.increase.lock().unwrap())
        }
    }

    struct Fx {
        sw: Sweeper<Arc<FakeSource>>,
        src: Arc<FakeSource>,
        ledger: Ledger,
        store: Store,
        path: String,
        uid: String,
        other: String,
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    async fn fx() -> Fx {
        let store = Store::open_memory().await.unwrap();
        let uid = "u1".to_string();
        let other = "u2".to_string();
        for id in [&uid, &other] {
            store
                .insert_user(id, &format!("{id}@b.c"), "h", None)
                .await
                .unwrap();
        }
        let path = std::env::temp_dir()
            .join(format!("aisix-sweeper-{}.yaml", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string();
        std::fs::write(
            &path,
            format!(
                "models:\n- display_name: keep-me\n  provider: openai\n\
                 api_keys:\n\
                 - display_name: a\n  key_hash: aa\n  user_id: {uid}\n\
                 - display_name: b\n  key_hash: bb\n  user_id: {other}\n\
                 - display_name: ops\n  key_hash: cc\n"
            ),
        )
        .unwrap();
        let src = Arc::new(FakeSource::default());
        Fx {
            sw: Sweeper::new(
                store.clone(),
                src.clone(),
                crate::resources::Writer::new(path.clone()),
            ),
            src,
            ledger: Ledger::new(store.clone()),
            store,
            path,
            uid,
            other,
        }
    }

    fn read_disabled(path: &str, user_id: &str) -> bool {
        let doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        doc["api_keys"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|k| k.get("user_id").and_then(|v| v.as_str()) == Some(user_id))
            .and_then(|k| k.get("disabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn 按时间窗查增量_没有增量时不重复扣() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        // 第一轮只建水位线。
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();

        f.src.set_increase(1_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 4_000_000);

        // 这一轮窗口内没有新增量，不该再扣。
        f.src.set_increase(0);
        f.sw.tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 4_000_000);
    }

    #[tokio::test]
    async fn 查询窗口从水位线接到当前_不留缺口() {
        let f = fx().await;
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        // 停摆十分钟后再跑：窗口必须覆盖整段空档，否则停摆期间的消费就永远
        // 不入账了。
        f.src.set_increase(1);
        f.sw.tick(t("2026-08-26T10:10:00Z")).await.unwrap();
        let (from, to) = f.src.last_window_for(&f.uid);
        assert_eq!(from, t("2026-08-26T10:00:00Z"));
        assert_eq!(to, t("2026-08-26T10:10:00Z"));
    }

    #[tokio::test]
    async fn 读取失败时水位线不前进() {
        let f = fx().await;
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();

        f.src.fail_for(&f.uid);
        let r = f.sw.tick(t("2026-08-26T10:05:00Z")).await.unwrap();
        assert_eq!(r.read_failures, 1);
        // 若此时把水位线推到当前时刻，这五分钟的消费就被跳过了 —— 又是一次
        // 静默白送。所以它必须还停在原处。
        assert_eq!(
            f.store.counted_through(&f.uid).await.unwrap(),
            Some(t("2026-08-26T10:00:00Z"))
        );

        // 下一轮窗口因此更长，increase() 照样正确。
        *f.src.fail_for.lock().unwrap() = None;
        f.src.set_increase(2_000);
        f.sw.tick(t("2026-08-26T10:06:00Z")).await.unwrap();
        let (from, to) = f.src.last_window_for(&f.uid);
        assert_eq!(from, t("2026-08-26T10:00:00Z"));
        assert_eq!(to, t("2026-08-26T10:06:00Z"));
    }

    #[tokio::test]
    async fn 余额归零把该用户的密钥置_disabled_别人的不碰() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 1_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.ledger
            .credit(&f.other, 9_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();

        f.src.set_increase(1_200_000);
        let r = f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert!(f.ledger.balance(&f.uid).await.unwrap() < 0);
        assert_eq!(r.disabled, 1);
        assert!(read_disabled(&f.path, &f.uid));
        // 别人的密钥不许被碰 —— 他余额还够。
        assert!(!read_disabled(&f.path, &f.other));
    }

    #[tokio::test]
    async fn 补上余额后密钥被重新启用() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 1_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        f.src.set_increase(1_200_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert!(read_disabled(&f.path, &f.uid));

        f.ledger
            .credit(&f.uid, 5_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.src.set_increase(0);
        let r = f.sw.tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(r.reenabled, 1);
        assert!(!read_disabled(&f.path, &f.uid));
    }

    #[tokio::test]
    async fn 改写配置不会抹掉门户不认识的字段() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 1_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        f.src.set_increase(9_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();

        let after = std::fs::read_to_string(&f.path).unwrap();
        // 走窄结构体序列化会把整份网关配置削成门户认识的那几个字段。
        assert!(after.contains("keep-me"), "models 段被抹掉了:\n{after}");
        assert!(after.contains("ops"), "没有主人的密钥被抹掉了:\n{after}");
        assert!(after.contains("key_hash"), "key_hash 被抹掉了:\n{after}");
    }

    #[tokio::test]
    async fn 没有主人的密钥永不被停用() {
        let f = fx().await;
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        f.src.set_increase(0);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        let doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
        let ops = doc["api_keys"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|k| k.get("display_name").and_then(|v| v.as_str()) == Some("ops"))
            .unwrap();
        // 运维自己的密钥没有 user_id，不属于任何用户，余额与它无关。
        assert!(ops.get("disabled").is_none(), "运维密钥被动过了");
    }
}
