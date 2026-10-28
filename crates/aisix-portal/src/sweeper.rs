//! 对账控制环。
//!
//! 每轮做三件事：读消费 → 入账 → 余额归零则停用该用户的密钥。
//!
//! # 怎么读消费：逐条序列的游标，不是时间窗
//!
//! 每轮读该用户每条花费序列（`aisix_llm_spend_micro_usd_total`，按完整标签区分，
//! 含 `instance`）的**原始累计值**，与库里记下的上次读数做差，差之和就是这一轮
//! 要扣的钱。
//!
//! 之前按时间窗查 `increase(...[自上次至今])`。那在首尾相接的窗口上算不对：窗口
//! 里少于两个采样点就返回空、被当成 0，而且只取窗口内首末两个采样之差，窗口之间
//! 的那段增量永远算不到。抓取间隔不比对账周期短时几乎什么都扣不到 —— 用真
//! Prometheus（抓取 15s、对账 5s）实测，$5 的消费扣了 $0。
//!
//! 也不能对**总和**做差：花费指标每个网关副本各自暴露一份，任一副本重启都会让
//! 总和下陷，看起来像整体重置，水位线会被拉回低点、那段消费永久丢失。游标按单条
//! 序列记就没有这个问题。
//!
//! 每条序列每轮都带着「上次读数之后重置过几次」（Prometheus 的 `resets()`），与原始
//! 读数出自同一次查询、同一个求值时刻，所以彼此一致：
//!
//! - 重置过 —— `resets()` 数到了，或者这条序列所在网关进程的启动时刻
//!   （`process_start_time_seconds`）变了：这一轮的花费是重置之后的累计，即当前
//!   读数，不论它比上次大还是小。光靠 `resets()` 不够：它只认得「值变小了」，而
//!   重启后在第一次抓取前就花得比重启前还多时，值只是涨了 —— 用真 Prometheus 实测，
//!   这种重启下 $5 的消费只扣到 $3。启动时刻与值无关。
//!   只在读数变小时才查重置的话，重启后又涨过上次读数的序列只会被扣 `当前 − 上次`，
//!   重启前那段累计被整个吞掉。
//! - 没重置过而读数变小：读到的是旧值（高可用的两个 Prometheus 副本抓取时刻不同、
//!   查询缓存）。不扣，游标不倒退 —— 当成重置的话会把整段累计值再扣一遍。
//! - 没重置过而读数变大：扣差值。
//!
//! 重置前、上次读数之后的那一小段花费（从上次读数到进程退出）拿不到，不计；它不超过
//! 一个对账周期的消费。

//! # 三个不变量
//!
//! - **游标只在入账成功后前进。** 读取失败就原地不动，下一轮的读数照样包含这段
//!   时间的消费。
//! - **扣款、游标、密钥花费在同一个事务里。** 中间崩了会重复扣或漏扣。事务里
//!   先核对算差额所依据的游标还是原样，被别的门户实例动过就这一轮不入账。
//! - **新序列从 0 算起，读到多少就是花了多少。** 新用户也一样：他的计数器生来
//!   就是 0，第一次被读到之前的消费照扣 —— 否则注册、拿额度、铸密钥、用掉都赶在
//!   第一轮对账之前，这段就白用了。唯一的例外是从按时间窗计费的版本升级上来的
//!   老用户：那一轮只记下当前读数，不把历史当成新序列重扣。

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

/// 一条序列的游标多久没被读到就剪掉。
///
/// 游标按完整标签记，`instance` 在其中：Kubernetes 里每次发版，每个 pod 都是一个
/// 新的 IP:端口，也就是一批新序列，旧的那批不会再出现。不剪的话每个用户的游标随
/// 发版只增不减，而每一轮都要把它们全部读出来。
///
/// 剪掉不会造成重扣。网关进程活着就一直暴露它的每条序列（回收指标句柄也不删
/// 序列），所以一条序列整整三十天不在读数里，暴露它的进程早已退出；同样的标签再
/// 出现，是一个新进程 —— 新 pod 恰好拿到同一个地址，或者网关重启了 —— 计数器从 0
/// 起，启动时刻比游标里记的晚。按重启的规则这时本来就按当前读数整段扣，与没有
/// 游标的新序列扣的是同一个数，所以游标在不在都一样。不暴露启动时刻的旧网关上，
/// 留着游标反而更糟：重置早落在 `resets()` 的窗口之外，新进程的计数器会被拿去跟
/// 旧进程的读数比，少扣。
///
/// 唯一的例外是一个网关进程一直活着、却连续三十天没被抓到，之后又被抓到：它在那
/// 之前已经扣过的累计会再扣一次（那三十天里的消费在此之前同样一直没扣到）。
const CURSOR_RETENTION: chrono::TimeDelta = chrono::TimeDelta::days(30);

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
    /// 配置写入失败的次数。
    ///
    /// 单独记一格，因为这几步原本用 `let _ =` 把错误全丢了：写前校验一旦拒绝
    /// 这次改动（比如库里有条记录指向已不存在的密钥），对账环会照常报成功，
    /// 而额度从此再也推不下去 —— 没有任何东西会说出来。
    pub write_failures: usize,
}

/// 一条花费序列的当前读数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendSeries {
    /// 序列的完整标签（排序后拼成一串）。游标按它记。
    pub key: String,
    /// 这条序列的 `api_key_id` 标签：把花费归到具体某把密钥上。
    pub api_key_id: Option<String>,
    /// 当前累计值（micro-USD）。
    pub value: u64,
    /// 从上次读数（`since`）到这次之间，这条序列的计数器重置过几次。
    pub resets_since: u64,
    /// 这条序列所在网关进程的启动时刻（毫秒）。旧网关不暴露时为 `None`。
    pub started_ms: Option<i64>,
}

/// 消费读取器。抽成 trait 只为把 Prometheus 换成测试替身，生产实现只有一个。
#[allow(async_fn_in_trait)]
pub trait ConsumptionSource {
    /// `at` 时刻该用户名下每条花费序列的累计值，以及自 `since`（上次读数的时刻）
    /// 以来各自重置过几次。`None` = 读不到；`Some(空)` = 这个人还没有任何花费序列。
    ///
    /// 两样东西必须出自同一次读：分开查的话，高可用的两个 Prometheus 副本各答
    /// 一半，读数与重置次数可以互相矛盾。用户级扣款与密钥级花费也都从这一次读数
    /// 里算 —— 两边用的是同一组数，账才对得上。
    async fn spend_series(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> Option<Vec<SpendSeries>>;
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
        // 花超自己额度的密钥。在用户循环里就地收集 —— 那里才拿得到真正的窗口。
        let mut exhausted: Vec<String> = Vec::new();

        for u in &users {
            // 这一轮的差额全按这份依据算；入账时在事务里再核对一遍（见 `settle`）。
            let basis = self.store.billing_basis(&u.id).await?;
            let since = basis.counted_through().unwrap_or(now);
            let Some(series) = self.source.spend_series(&u.id, since, now).await else {
                // 读不到就原地不动：游标不前进，下一轮的读数照样包含这段消费。
                // 密钥级也一并跳过 —— 它们出自同一次读数。
                r.read_failures += 1;
                continue;
            };
            if basis.needs_seeding() {
                // 升级上来的老用户：只记下当前读数，不把历史当成新序列重扣。
                self.settle(&u.id, &basis, 0, &series, &[], now).await?;
                continue;
            }

            let cursors: std::collections::HashMap<String, (u64, Option<i64>)> = basis
                .cursors
                .iter()
                .map(|(k, &(v, started))| (k.clone(), (v.max(0) as u64, started)))
                .collect();
            let mut spent = 0u64;
            let mut by_key: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
            // 这一轮之后每条序列的游标。通常就是读数；只有「变小却没重置」时留在原处。
            let mut next = Vec::with_capacity(series.len());
            for s in &series {
                // (这一轮扣多少, 新游标, 游标记下的进程启动时刻)
                let (delta, cursor, started) = match (cursors.get(&s.key).copied(), s.started_ms) {
                    // 新序列（新副本、新密钥、新用户）：从 0 算起。
                    (None, now) => (s.value, s.value, now),
                    // 两边都知道进程启动时刻：由它判断，`resets()` 不参与 —— 计数器在
                    // 一个进程里只增不减。只认「更晚」为重启：落后的 Prometheus 副本
                    // 还在报重启前的进程，那是旧读数；若按「不相等」算重启，它会把
                    // 旧进程的整段累计再扣一遍，下一次读到新进程又扣一遍。
                    (Some((prev, Some(was))), Some(now)) => {
                        if now > was {
                            (s.value, s.value, Some(now))
                        } else if now < was || s.value < prev {
                            // 更早的进程，或同一进程却变小：旧读数。不扣，游标与启动
                            // 时刻都不倒退 —— 倒退的话，下一轮会把这段差额再扣一次。
                            (0, prev, Some(was))
                        } else {
                            (s.value - prev, s.value, Some(was))
                        }
                    }
                    // 至少一边不知道启动时刻（旧网关）：退回按 `resets()` 判断。重置
                    // 之后记下这次读到的启动时刻（可能为空），免得之后补上启动时刻
                    // 时把同一次重启再认一遍。
                    (Some(_), now) if s.resets_since > 0 => (s.value, s.value, now),
                    (Some((prev, was)), now) if s.value >= prev => {
                        (s.value - prev, s.value, now.or(was))
                    }
                    (Some((prev, was)), _) => (0, prev, was),
                };
                spent = spent.saturating_add(delta);
                if let Some(id) = s.api_key_id.as_deref() {
                    *by_key.entry(id).or_default() += delta;
                }
                next.push(SpendSeries {
                    value: cursor,
                    started_ms: started,
                    ..s.clone()
                });
            }

            // 只给设了额度的密钥累计 —— 那是持久兜底要比的数。
            let quotas: Vec<(String, i64)> = self
                .store
                .key_quotas(&u.id)
                .await?
                .into_iter()
                .filter(|(_, quota)| *quota > 0)
                .collect();
            let key_spend: Vec<(String, i64)> = quotas
                .iter()
                .filter_map(|(name, _)| {
                    let id = aisix_core::filesource::derive_id("api_keys", name);
                    by_key
                        .get(id.as_str())
                        .filter(|v| **v > 0)
                        .map(|v| (name.clone(), i64::try_from(*v).unwrap_or(i64::MAX)))
                })
                .collect();

            let settled = self
                .settle(&u.id, &basis, spent, &next, &key_spend, now)
                .await?;
            if settled && spent > 0 {
                r.debited += 1;
            }

            // 花超自己额度的密钥。比的是库里的累计花费，不是这一轮的增量。
            //
            // 这一轮让给了别的实例（`settled` 为假）也照样比：库里的累计花费已经含着
            // 那个实例刚入的账。跳过的话，这把密钥不在 `exhausted` 里，下面那次写盘
            // 会把那个实例刚停掉的它重新启用。
            let totals: std::collections::HashMap<String, i64> =
                self.store.key_spend(&u.id).await?.into_iter().collect();
            for (name, quota) in quotas {
                if totals.get(&name).copied().unwrap_or(0) >= quota {
                    exhausted.push(name);
                }
            }
        }

        // ── 配置改动：算齐之后**只写一次盘** ───────────────────────────
        //
        // 这三段以前各自 `edit()` 一次，于是一轮对账最多写三次文件、发三次
        // SIGHUP，网关跟着重建三遍快照（生产日志里 `generation` 一路跳）。合成
        // 一次之后网关每轮最多重载一次，而且中间不再出现「额度推了、开关还没
        // 落」这种半成品状态。
        //
        // 顺序无关：三段各改文档的不同部分（策略表 / api_keys 的 disabled）。

        // 累计发放总额 —— 这才是真正的闸：网关按 `granted_micro_usd` 比对自己
        // 那个只增不减的消费计数器，精确、跨天不续杯、无需对账。
        let mut grants: Vec<(String, i64)> = Vec::new();
        for u in &users {
            grants.push((u.id.clone(), self.ledger.total_granted(&u.id).await?));
        }

        // 密钥级额度。用户把自己的总额分配到各把密钥上，网关按 `scope: api_key`
        // 单独收口每一把 —— 用户级那条策略仍然在，所以「总花销不超过用户额度」
        // 由它保证，密钥级只是在此之下再切分。
        //
        // 剪残留只动库，得在读配置之前做完。
        // 用 `?` 而不是记进 `write_failures`：那个计数的含义是「网关正在用旧
        // 配置」，而剪残留失败不会造成那个后果。库出问题跟这一轮里其它数据库
        // 调用一样，让整轮失败（计入 `reconcile_errors`）。
        self.prune_orphan_quotas().await?;
        let quotas = self.store.all_key_quotas().await?;

        // 余额与账号状态共同决定密钥开关。放在入账之后，用的是这一轮之后的余额。
        //
        // **账号被停用也要停密钥。** `users.disabled` 原本只在登录处检查：那个人
        // 登不进门户了，可他名下的密钥在网关那边照常放行 —— 而「停用这个用户」
        // 恰恰是要切断他的场景。控制台还把这种账号显示成「已停用」，等于告诉运维
        // 人已经断了。
        let mut wanted: Vec<(String, bool)> = Vec::new();
        for u in &users {
            let balance = self.ledger.balance(&u.id).await?;
            wanted.push((u.id.clone(), u.disabled || balance <= 0));
        }

        let off = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let on = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (o1, o2) = (off.clone(), on.clone());
        let wrote = self
            .resources
            .edit(move |doc| {
                let mut changed = apply_key_allowances_to(doc, &quotas);
                changed |= apply_allowances_to(doc, &grants);
                changed |= apply_key_state_to(doc, &wanted, &exhausted, &o1, &o2);
                changed
            })
            .await;
        match wrote {
            Ok(_) => {
                use std::sync::atomic::Ordering;
                r.disabled = off.load(Ordering::SeqCst);
                r.reenabled = on.load(Ordering::SeqCst);
            }
            Err(e) => {
                // 这一次写盘里就有杀手闸。失败还报「改了 N 把」的话，账面显示
                // 已停用、实际仍然在放行 —— 最不该被吞掉的一次失败。
                eprintln!("对账写配置失败（网关仍在用旧值）: {e}");
                r.write_failures += 1;
            }
        }
        Ok(r)
    }

    /// 一轮对账的入账：扣款、所有序列的游标、密钥花费、对账时刻，一个事务。
    ///
    /// `debit` 为 0 时不写流水，但游标照样更新 —— 建游标的那一轮就是这样。
    ///
    /// 返回 `false` = 没入账：算这些差额所依据的 `basis` 在事务里已经对不上了。
    async fn settle(
        &self,
        user_id: &str,
        basis: &crate::store::BillingBasis,
        debit: u64,
        series: &[SpendSeries],
        key_spend: &[(String, i64)],
        through: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let user_id = user_id.to_string();
        let basis = basis.clone();
        let delta = -i64::try_from(debit).unwrap_or(i64::MAX);
        let cursors: Vec<(String, i64, Option<i64>)> = series
            .iter()
            .map(|s| {
                (
                    s.key.clone(),
                    i64::try_from(s.value).unwrap_or(i64::MAX),
                    s.started_ms,
                )
            })
            .collect();
        let key_spend = key_spend.to_vec();
        let through = through.to_rfc3339();
        // 同样用 `BEGIN IMMEDIATE`：这个事务里有多条写，多个门户实例或多轮对账
        // 重叠时，deferred 事务会在升级成写的那一刻当场失败而不是等待。下面的核对
        // 也靠它：写锁在核对之前就拿到了，核对与写之间插不进别人的入账。
        self.store
            .immediate_tx(move |conn| {
                Box::pin(async move {
                    // **先核对依据，再写。** 差额是在事务之外按 `basis` 算的，而多个
                    // 门户实例共用一个库时，两边的读数会落在同一段消费上。依据变了，
                    // 说明另一个实例在我们读完之后已经入过账 —— 照这里的差额再扣一遍
                    // 就是重复扣款。这一轮让给它：不扣、不动游标，下一轮按新的依据
                    // 重算，这段消费一分不少。
                    //
                    // 此时什么都还没写，直接返回等同回滚：提交的是一个空事务。
                    if crate::store::billing_basis_on(&mut *conn, &user_id).await? != basis {
                        return Ok(false);
                    }
                    let wall = Utc::now();
                    let now = wall.to_rfc3339();
                    let stale_before = (wall - CURSOR_RETENTION).to_rfc3339();
                    if delta != 0 {
                        crate::ledger::insert_entry(
                            &mut *conn,
                            &user_id,
                            delta,
                            Source::Consumption,
                            None,
                        )
                        .await?;
                    }
                    for (series, value, started) in &cursors {
                        sqlx::query(
                            "INSERT INTO spend_cursor
                               (user_id, series, value_micro_usd, instance_started_ms, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5)
                             ON CONFLICT(user_id, series)
                             DO UPDATE SET value_micro_usd = ?3, instance_started_ms = ?4,
                                           updated_at = ?5",
                        )
                        .bind(&user_id)
                        .bind(series)
                        .bind(value)
                        .bind(started)
                        .bind(&now)
                        .execute(&mut *conn)
                        .await?;
                    }
                    // 剪掉久未出现的游标（见 `CURSOR_RETENTION`）。这一轮读数里的每条
                    // 序列上面都刚写成 `updated_at = now`，所以按时间筛出来的只会是这次
                    // 读数里没有的。
                    sqlx::query("DELETE FROM spend_cursor WHERE user_id = ?1 AND updated_at < ?2")
                        .bind(&user_id)
                        .bind(&stale_before)
                        .execute(&mut *conn)
                        .await?;
                    for (key_name, micro) in &key_spend {
                        sqlx::query(
                            "INSERT INTO key_spend (user_id, key_name, spent_micro_usd, updated_at)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(user_id, key_name)
                             DO UPDATE SET spent_micro_usd = spent_micro_usd + ?3, updated_at = ?4",
                        )
                        .bind(&user_id)
                        .bind(key_name)
                        .bind(micro)
                        .bind(&now)
                        .execute(&mut *conn)
                        .await?;
                    }
                    sqlx::query(
                        "INSERT INTO consumption_mark (user_id, counted_through, updated_at, cursors_seeded)
                         VALUES (?1, ?2, ?3, 1)
                         ON CONFLICT(user_id)
                         DO UPDATE SET counted_through = ?2, updated_at = ?3, cursors_seeded = 1",
                    )
                    .bind(&user_id)
                    .bind(&through)
                    .bind(&now)
                    .execute(&mut *conn)
                    .await?;
                    Ok(true)
                })
            })
            .await
    }

    /// 把每把密钥的额度写成一条 `scope: api_key` 的策略。
    ///
    /// `scope_ref` 必须是密钥的 **entry id**（`ConditionInput::api_key` 比的是
    /// `auth.entry.id`），而文件模式下那个 id 是
    /// `uuid5(命名空间, "api_keys/<display_name>")`。派生直接调网关自己的函数 ——
    /// 自己抄一份跟上游漂开之后，策略会挂在一个不存在的身份上：配了、加载了、
    /// 永不命中，而这种失败不报错。
    ///
    /// 用户级那条策略不受影响：两条同时生效，所以密钥再怎么分配，总花销都过不了
    /// 用户级那道闸。
    /// 剪掉「密钥已经不在了、额度记录还留着」的残留。**只动库，不写配置。**
    async fn prune_orphan_quotas(&self) -> Result<(), StoreError> {
        // 先把「密钥已经不在了、额度记录还留着」这种残留剪掉。
        //
        // 不剪的话那份额度永远占着用户的可分配额：`allocated_to_keys` 照样把它
        // 算进去，于是用户看着自己有额度却一分也分不出去，而界面上没有任何东西
        // 解释为什么。密钥可以从门户之外消失（运维在控制台删掉了它）。
        //
        // **额度记录必须先于配置文件读取。** 反过来的话，在两次读取之间新铸并
        // 设了额度的那把密钥会既不在旧的文件快照里、又已经在新的额度表里，于是
        // 被当成残留删掉 —— 用户刚设的额度悄无声息地没了。按这个顺序，晚于快照
        // 出现的记录压根不在候选集里。
        let snapshot = self.store.all_key_quotas().await?;
        // `all_key_names` 读不动时返回 `None`，此时**不剪** —— 把一次读取失败
        // 当成「一把密钥都没有」会把所有人的分配一次清空。
        if let Some(present) = self
            .resources
            .read()
            .await
            .ok()
            .as_deref()
            .and_then(crate::resources::all_key_names)
        {
            for (uid, name, _) in &snapshot {
                if !present.contains(name) {
                    self.store.drop_key_quota(uid, name).await?;
                    // 累计花费一起剪。留着的话这把密钥的名字将来若被重新用到
                    // （运维手建同名），它一上来就带着别人的花费。
                    self.store.drop_key_spend(uid, name).await?;
                }
            }
        }

        Ok(())
    }
}

/// 把每把密钥的额度写成 `api_key` 域的策略。返回文档是否被改过。
///
/// 纯函数：一轮对账里三段改动共用**同一次**写盘（见 [`Sweeper::tick`]），所以
/// 它们都只改内存里的文档，由调用方决定什么时候落盘。
fn apply_key_allowances_to(
    doc: &mut serde_yaml_ng::Value,
    quotas: &[(String, String, i64)],
) -> bool {
    use serde_yaml_ng::Value;
    let Some(map) = doc.as_mapping_mut() else {
        return false;
    };
    // 文档里现存的密钥名。只为它们下推策略 —— 一条指着不存在的密钥
    // 的策略会让网关**整份拒收**配置，而这里的额度记录来自门户自己的
    // 库，两者可能不同步（比如运维从控制台删掉了一把带额度的密钥）。
    // 不筛的话，那一条记录会让门户此后每次写入都被拒。
    let present: Vec<String> = map
        .get(Value::from("api_keys"))
        .and_then(Value::as_sequence)
        .map(|s| {
            s.iter()
                .filter_map(|k| {
                    k.get("display_name")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();

    let key = Value::from("rate_limit_policies");
    if !map.get(&key).map(Value::is_sequence).unwrap_or(false) {
        map.insert(key.clone(), Value::Sequence(Vec::new()));
    }
    let Some(list) = map.get_mut(&key).and_then(Value::as_sequence_mut) else {
        return false;
    };

    // `scope_ref` 写密钥的**名字**，不是算好的 id。
    //
    // 文件模式里这一层是「糖」：网关自己把名字解析成派生 id。写成
    // 派生 id 的话它成了一个「不存在的名字」，网关**整份配置**加载
    // 失败、静默保留旧快照 —— 连「余额耗尽就停用」那个闸也一起失效，
    // 而配置文件看起来完全正常。
    //
    // 这也正是仓库里记下的那条分歧轴：声明式文件里用名字，走线协议
    // 上才用 UUID。
    let wanted: Vec<(String, String, i64)> = quotas
        .iter()
        .filter(|(_, name, v)| *v > 0 && present.iter().any(|p| p == name))
        .map(|(_, name, v)| (format!("portal-key-{name}"), name.clone(), *v))
        .collect();

    let mut changed = false;
    // 额度被清掉或密钥被吊销的，对应策略要一并移除 —— 留着会挂在
    // 一个不存在的身份上，白占配置。
    let before = list.len();
    list.retain(|p| {
        let n = p.get("name").and_then(Value::as_str).unwrap_or("");
        !n.starts_with("portal-key-") || wanted.iter().any(|(w, _, _)| w == n)
    });
    if list.len() != before {
        changed = true;
    }

    for (pname, entry_id, granted) in &wanted {
        let existing = list
            .iter_mut()
            .find(|p| p.get("name").and_then(Value::as_str) == Some(pname.as_str()));
        match existing {
            Some(p) => {
                let cur = p
                    .get("granted_micro_usd")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if cur == *granted {
                    continue;
                }
                if let Some(m) = p.as_mapping_mut() {
                    m.insert(Value::from("granted_micro_usd"), Value::from(*granted));
                    changed = true;
                }
            }
            None => {
                let mut m = serde_yaml_ng::Mapping::new();
                m.insert(Value::from("name"), Value::from(pname.clone()));
                m.insert(Value::from("scope"), Value::from("api_key"));
                m.insert(Value::from("scope_ref"), Value::from(entry_id.clone()));
                m.insert(Value::from("granted_micro_usd"), Value::from(*granted));
                list.push(Value::Mapping(m));
                changed = true;
            }
        }
    }
    changed
}

/// 把每个用户的累计发放总额写成一条 `scope: member` 的策略。返回是否改过。
///
/// 策略名按用户派生，所以重复运行是幂等的：已经一致就不写盘、不发 SIGHUP。
/// 无条件重写会让文件 mtime 一直跳、网关白重载。
///
/// 只写 `granted_micro_usd`，不写窗口 —— 这条策略表达的是「总共给了多少」，
/// 给它一个窗口会让读配置的人以为额度按那个周期刷新。
fn apply_allowances_to(doc: &mut serde_yaml_ng::Value, grants: &[(String, i64)]) -> bool {
    use serde_yaml_ng::Value;
    let map = match doc.as_mapping_mut() {
        Some(m) => m,
        None => return false,
    };
    let key = Value::from("rate_limit_policies");
    if !map.get(&key).map(Value::is_sequence).unwrap_or(false) {
        map.insert(key.clone(), Value::Sequence(Vec::new()));
    }
    let Some(list) = map.get_mut(&key).and_then(Value::as_sequence_mut) else {
        return false;
    };
    let mut changed = false;
    // 先撤掉不该再存在的：额度归零、或者这个用户已经不在库里了
    // （直接删过库行）。留着的话它挂在一个谁也对不上的身份上，白占
    // 配置，而且下一个读配置的人会以为那个人还有额度。
    //
    // 只认自己写的那个前缀 —— 运维手写的策略不能碰。
    let keep: Vec<String> = grants
        .iter()
        .filter(|(_, g)| *g > 0)
        .map(|(uid, _)| format!("portal-allowance-{uid}"))
        .collect();
    let before = list.len();
    list.retain(|p| {
        let n = p.get("name").and_then(Value::as_str).unwrap_or("");
        !n.starts_with("portal-allowance-") || keep.iter().any(|k| k == n)
    });
    if list.len() != before {
        changed = true;
    }

    for (uid, granted) in grants {
        if *granted <= 0 {
            continue;
        }
        let name = format!("portal-allowance-{uid}");
        let existing = list
            .iter_mut()
            .find(|p| p.get("name").and_then(Value::as_str) == Some(name.as_str()));
        match existing {
            Some(p) => {
                let cur = p
                    .get("granted_micro_usd")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if cur == *granted {
                    continue;
                }
                if let Some(m) = p.as_mapping_mut() {
                    m.insert(Value::from("granted_micro_usd"), Value::from(*granted));
                    changed = true;
                }
            }
            None => {
                let mut m = serde_yaml_ng::Mapping::new();
                m.insert(Value::from("name"), Value::from(name));
                m.insert(Value::from("scope"), Value::from("member"));
                m.insert(Value::from("scope_ref"), Value::from(uid.clone()));
                m.insert(Value::from("granted_micro_usd"), Value::from(*granted));
                list.push(Value::Mapping(m));
                changed = true;
            }
        }
    }
    changed
}

/// 把期望的启停状态落到文档里。返回是否改过；改了几把记在 `off` / `on` 里。
///
/// `wanted` 是按**用户**算出来的期望状态；`exhausted` 是按**密钥**算出来的那几把
/// 已经花超自己额度的。两者取更严的一侧：用户没问题但这把超了，也停。
///
/// 走共用的写盘：铸密钥也写这个文件，两条路径不串行就会互相覆盖。改动用泛型
/// `Value`，不经过窄结构体（那会抹掉门户不认识的字段）。
fn apply_key_state_to(
    doc: &mut serde_yaml_ng::Value,
    wanted: &[(String, bool)],
    exhausted: &[String],
    o1: &std::sync::atomic::AtomicUsize,
    o2: &std::sync::atomic::AtomicUsize,
) -> bool {
    use std::sync::atomic::Ordering;

    // 每次重试从零开始数。
    o1.store(0, Ordering::SeqCst);
    o2.store(0, Ordering::SeqCst);
    let keys = crate::resources::api_keys_mut(doc);
    for k in keys.iter_mut() {
        let Some(uid) = k.get("user_id").and_then(serde_yaml_ng::Value::as_str) else {
            // 没有主人的密钥是运维自己用的，绝不能碰。
            continue;
        };
        let Some((_, user_want)) = wanted.iter().find(|(id, _)| id == uid) else {
            continue;
        };
        // 这把密钥自己有没有花超。取更严的一侧 —— 用户还有余额，但
        // 这把的子额度用完了，同样要停。
        let name = k
            .get("display_name")
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or("");
        let want = &(*user_want || exhausted.iter().any(|n| n == name));
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
    async fn spend_series(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> Option<Vec<SpendSeries>> {
        // 原始计数器、两段 `resets()`、各网关进程的启动时刻放进**同一个**查询，用
        // `aisix_part` 标签区分：同一个 Prometheus、同一个求值时刻答出来的，彼此
        // 不会矛盾。
        //
        // 两段重置都从 `since - LOOKBACK` 起，相减就是 `since` 之后才发生的重置，
        // 包括 `since` 之前最后一个采样与之后第一个采样之间的那一次（前一段含它、
        // 后一段不含）。LOOKBACK 取 Prometheus 默认的查询回看窗口：`since` 时刻的
        // 读数就是这个窗口里的最后一个采样。
        const LOOKBACK_SECS: i64 = 300;
        let sel = format!(
            "aisix_llm_spend_micro_usd_total{{user_id=\"{}\"}}",
            crate::usage::escape_label(user_id)
        );
        // 按整秒算：查询在 `at` 的整秒求值，窗口两端都落在整秒上，否则
        // `since` 的小数部分会让窗口短一秒，那一秒里的重置两次读数都看不到。
        let gap = (at.timestamp() - since.timestamp()).max(0);
        let span = gap + LOOKBACK_SECS;
        let offset = if gap > 0 {
            format!(" offset {gap}s")
        } else {
            String::new()
        };
        let q = format!(
            "label_replace({sel}, \"aisix_part\", \"value\", \"\", \"\") \
             or label_replace(resets({sel}[{span}s]), \"aisix_part\", \"resets_upto\", \"\", \"\") \
             or label_replace(resets({sel}[{LOOKBACK_SECS}s]{offset}), \"aisix_part\", \"resets_before\", \"\", \"\") \
             or label_replace(process_start_time_seconds and on(job, instance) {sel}, \"aisix_part\", \"started\", \"\", \"\")"
        );
        parse_spend_series(&self.query(&q, at).await?)
    }
}

impl PromSource {
    /// 在 `at` 时刻求值的即时查询。读不到、或回答不是 success，都是 `None`。
    async fn query(&self, q: &str, at: DateTime<Utc>) -> Option<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{}/api/v1/query", self.base))
            .query(&[("query", q), ("time", &at.timestamp().to_string())])
            .send()
            .await
            .ok()?;
        resp.json().await.ok()
    }
}

/// 解析 [`PromSource::spend_series`] 那个合并查询的结果向量，按序列归并读数与两段
/// 重置次数。
///
/// 回答本身不对（不是 success、没有结果向量）才整体当成读不到。某一条序列看不懂
/// 只跳过它：它的游标不动，下一轮仍能补上；若因为一条坏序列让整个人读不到，只要
/// 它一直在，这个人的消费就一直扣不到。
fn parse_spend_series(body: &serde_json::Value) -> Option<Vec<SpendSeries>> {
    #[derive(Default)]
    struct Parts {
        job_instance: (String, String),
        api_key_id: Option<String>,
        value: Option<u64>,
        resets_upto: u64,
        resets_before: u64,
    }
    if body.get("status")?.as_str()? != "success" {
        return None;
    }
    let result = body.pointer("/data/result")?.as_array()?;
    let mut by_series: std::collections::BTreeMap<String, Parts> =
        std::collections::BTreeMap::new();
    // (job, instance) → 那个网关进程的启动时刻（毫秒）。
    let mut started: std::collections::HashMap<(String, String), i64> =
        std::collections::HashMap::new();
    let label = |labels: &serde_json::Map<String, serde_json::Value>, name: &str| {
        labels
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    for item in result {
        let Some(labels) = item.get("metric").and_then(serde_json::Value::as_object) else {
            continue;
        };
        if labels.get("aisix_part").and_then(serde_json::Value::as_str) == Some("started") {
            if let Some(secs) = item
                .pointer("/value/1")
                .and_then(serde_json::Value::as_str)
                .and_then(|raw| raw.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
            {
                started.insert(
                    (label(labels, "job"), label(labels, "instance")),
                    (secs * 1000.0).round() as i64,
                );
            }
            continue;
        }
        // 键里不带 `__name__` 与查询自己加的 `aisix_part`：函数（`resets()`）的结果
        // 没有前者，三段的同一条序列必须落到同一个键上。
        //
        // 这个串会落库（`spend_cursor.series`）。改它的格式，库里所有游标就都对
        // 不上了 —— 每条序列都被当成新序列、整段累计值重扣一遍。要改，先写迁移
        // 把旧键换成新键。
        let Some(mut pairs) = labels
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "__name__" | "aisix_part"))
            .map(|(k, v)| v.as_str().map(|v| (k.as_str(), v)))
            .collect::<Option<Vec<(&str, &str)>>>()
        else {
            continue;
        };
        pairs.sort_unstable();
        let key = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v:?}"))
            .collect::<Vec<_>>()
            .join(",");
        let Some(number) = item
            .pointer("/value/1")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| raw.parse::<f64>().ok())
            .and_then(plausible_spend)
        else {
            continue;
        };
        let parts = by_series.entry(key).or_default();
        match labels.get("aisix_part").and_then(serde_json::Value::as_str) {
            Some("resets_upto") => parts.resets_upto = number,
            Some("resets_before") => parts.resets_before = number,
            _ => {
                parts.value = Some(number);
                parts.job_instance = (label(labels, "job"), label(labels, "instance"));
                parts.api_key_id = labels
                    .get("api_key_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
            }
        }
    }
    // 没有当前读数的序列（已经停止上报，只在重置窗口里还有采样）不参与这一轮。
    Some(
        by_series
            .into_iter()
            .filter_map(|(key, p)| {
                Some(SpendSeries {
                    key,
                    started_ms: started.get(&p.job_instance).copied(),
                    api_key_id: p.api_key_id,
                    value: p.value?,
                    resets_since: p.resets_upto.saturating_sub(p.resets_before),
                })
            })
            .collect(),
    )
}

/// 把一个读数变成可入账的金额。**读数坏了要当成读不到，不能当成花了多少。**
///
/// 两个坏值都是真会出现的：
///
/// * `NaN` —— 旧写法 `v.max(0.0)` 会把它变成 0，于是水位线照常推进，而那段窗口
///   里真实发生的消费被永久跳过，账面上什么异常都看不出来。
/// * `+Inf` 或大得离谱的有限值 —— `as u64` 是饱和转换，会得到 `u64::MAX`，接着
///   给这个用户记一笔 `i64::MAX` 的欠款：余额深度为负、密钥永久停用，而管理员
///   没有任何数能把它填平。
///
/// 返回 `None` 就是走「读不到」那条既有分支：水位线原地不动，下一轮用更长的窗口
/// 重来，`increase()` 在更长的窗口上照样正确。
///
/// 负数仍然按 0 处理：`increase()` 在计数器上不该为负，一点点负值是外推的舍入，
/// 当成「这段没花钱」比卡住水位线更合适。
fn plausible_spend(v: f64) -> Option<u64> {
    if !v.is_finite() || v > i64::MAX as f64 {
        return None;
    }
    Some(v.max(0.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use std::sync::{Arc, Mutex};

    /// 一条模拟的花费序列：累计值，以及每次被读之前长多少（两次读之间的流量）。
    #[derive(Default)]
    struct Counter {
        value: u64,
        per_read: u64,
        /// 这条序列至今被重置过几次。
        resets: u64,
        /// 这条序列所在网关进程的启动时刻（毫秒）；每次重启都变。
        started_ms: i64,
    }

    /// (用户, `api_key_id`, 网关实例)。
    type SeriesId = (String, Option<String>, String);

    /// 按 Prometheus **即时查询**的语义应答：返回每条序列的原始累计值。不替门户
    /// 算任何差 —— 上一版替身自己按「本次减上次」算好了增量，于是门户那边窗口
    /// 算法的错被它整个盖住了。
    #[derive(Default)]
    pub(super) struct FakeSource {
        /// 每个用户那条未归到密钥的序列，每次被读之前长多少。对所有用户生效。
        increase: Mutex<u64>,
        counters: Mutex<std::collections::HashMap<SeriesId, Counter>>,
        /// 每次读取时各序列的重置次数：(读取时刻, 序列, 重置次数)。
        history: Mutex<Vec<(DateTime<Utc>, SeriesId, u64)>>,
        /// 下一次读取时，该用户那条未归到密钥的序列少报多少 —— 从一个落后的
        /// Prometheus 副本读到的旧值。真实累计值不变。
        lag_next: Mutex<std::collections::HashMap<String, u64>>,
        /// 下一次读取改报的 (累计值, 进程启动时刻)，见 `stale_replica_once`。
        stale_next: Mutex<std::collections::HashMap<String, (u64, i64)>>,
        /// 指定让哪个用户的读取失败。写成按用户而不是「下一次」，因为
        /// 「下一次」会让测试依赖清扫器先访问谁 —— 那种测试无论答案如何都是
        /// 脆的（第一版就是这么挂的）。
        fail_for: Mutex<Option<String>>,
    }

    impl FakeSource {
        /// 每个用户每次被读之前都花掉 `v`（未归到密钥）。
        pub(super) fn set_increase(&self, v: u64) {
            *self.increase.lock().unwrap() = v;
        }
        /// 某把密钥（按名字）每次被读之前都花掉 `v`，记在其主人名下。
        pub(super) fn set_key_increase(&self, user_id: &str, key_name: &str, v: u64) {
            let id = aisix_core::filesource::derive_id("api_keys", key_name);
            self.counters
                .lock()
                .unwrap()
                .entry((user_id.to_string(), Some(id), "gw-1".into()))
                .or_default()
                .per_read = v;
        }
        /// 不经读取直接累计 `v` —— 门户没在对账的那段时间里发生的消费。
        pub(super) fn add_spend(&self, user_id: &str, v: u64) {
            self.series(user_id, "gw-1", v);
        }
        /// 某个网关实例上的那条序列累计 `v`；实例第一次出现就是一条新序列。
        pub(super) fn series(&self, user_id: &str, instance: &str, v: u64) {
            self.counters
                .lock()
                .unwrap()
                .entry((user_id.to_string(), None, instance.into()))
                .or_default()
                .value += v;
        }
        /// 这个网关实例上的序列从此不再上报 —— 那个 pod 没了。
        pub(super) fn drop_instance(&self, user_id: &str, instance: &str) {
            self.counters
                .lock()
                .unwrap()
                .retain(|(u, _, i), _| !(u == user_id && i == instance));
        }
        /// 这个用户名下所有序列归零 —— 网关副本重启。
        pub(super) fn reset(&self, user_id: &str) {
            for ((u, _, _), c) in self.counters.lock().unwrap().iter_mut() {
                if u == user_id {
                    c.value = 0;
                    c.resets += 1;
                    c.started_ms += 1;
                }
            }
        }
        /// 网关副本重启了，但 `resets()` 看不出来：重启后、下一次抓取之前花得比
        /// 重启前还多，抓到的值只是涨了，没有变小过。只有进程启动时刻变了。
        pub(super) fn restart_unseen_by_resets(&self, user_id: &str) {
            for ((u, _, _), c) in self.counters.lock().unwrap().iter_mut() {
                if u == user_id {
                    c.value = 0;
                    c.started_ms += 1;
                }
            }
        }
        /// 下一次读取来自一个还没抓到重启的 Prometheus 副本：报的是重启前那个
        /// 进程（启动时刻 `started_ms`）的累计 `value`，也没看到重置。
        pub(super) fn stale_replica_once(&self, user_id: &str, value: u64, started_ms: i64) {
            self.stale_next
                .lock()
                .unwrap()
                .insert(user_id.to_string(), (value, started_ms));
        }
        /// 下一次读取少报 `v`，计数器本身没有重置。
        pub(super) fn lag_once(&self, user_id: &str, v: u64) {
            self.lag_next.lock().unwrap().insert(user_id.to_string(), v);
        }

        pub(super) fn fail_for(&self, user_id: &str) {
            *self.fail_for.lock().unwrap() = Some(user_id.to_string());
        }
    }

    impl ConsumptionSource for Arc<FakeSource> {
        async fn spend_series(
            &self,
            user_id: &str,
            since: DateTime<Utc>,
            at: DateTime<Utc>,
        ) -> Option<Vec<SpendSeries>> {
            let inc = *self.increase.lock().unwrap();
            let mut counters = self.counters.lock().unwrap();
            // 两次读之间的流量先发生 —— 读失败不会让消费不发生。
            if inc > 0 {
                counters
                    .entry((user_id.to_string(), None, "gw-1".into()))
                    .or_default()
                    .value += inc;
            }
            for ((u, _, _), c) in counters.iter_mut() {
                if u == user_id {
                    c.value += c.per_read;
                }
            }
            let mut history = self.history.lock().unwrap();
            for (id, c) in counters.iter().filter(|((u, _, _), _)| u == user_id) {
                history.push((at, id.clone(), c.resets));
            }
            if self.fail_for.lock().unwrap().as_deref() == Some(user_id) {
                return None;
            }
            // 某条序列在时刻 t 之前最后一次被读时的重置次数 —— Prometheus 按采样
            // 数重置，这里按读数近似。
            let resets_at = |id: &SeriesId, t: DateTime<Utc>| {
                history
                    .iter()
                    .rev()
                    .find(|(when, sid, _)| sid == id && *when <= t)
                    .map_or(0, |(_, _, n)| *n)
            };
            let lag = self.lag_next.lock().unwrap().remove(user_id).unwrap_or(0);
            let stale = self.stale_next.lock().unwrap().remove(user_id);
            Some(
                counters
                    .iter()
                    .filter(|((u, _, _), _)| u == user_id)
                    .map(|(id, c)| SpendSeries {
                        key: series_key(id),
                        api_key_id: id.1.clone(),
                        value: if id.1.is_none() && id.2 == "gw-1" {
                            c.value.saturating_sub(lag)
                        } else {
                            c.value
                        },
                        resets_since: if since < at {
                            resets_at(id, at) - resets_at(id, since)
                        } else {
                            0
                        },
                        started_ms: Some(c.started_ms),
                    })
                    .map(|mut s| {
                        if let (Some((value, started)), true) = (
                            stale,
                            s.api_key_id.is_none() && s.key.contains("instance=gw-1"),
                        ) {
                            s.value = value;
                            s.started_ms = Some(started);
                            s.resets_since = 0;
                        }
                        s
                    })
                    .collect(),
            )
        }
    }

    fn series_key((u, key, instance): &SeriesId) -> String {
        format!("api_key_id={key:?},instance={instance},user_id={u}")
    }

    pub(super) struct Fx {
        pub(super) sw: Sweeper<Arc<FakeSource>>,
        pub(super) src: Arc<FakeSource>,
        pub(super) ledger: Ledger,
        pub(super) store: Store,
        pub(super) path: String,
        pub(super) uid: String,
        pub(super) other: String,
    }

    pub(super) fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    pub(super) async fn fx() -> Fx {
        let store = Store::open_memory().await.unwrap();
        let uid = "u1".to_string();
        let other = "u2".to_string();
        for id in [&uid, &other] {
            store
                .insert_user(id, &format!("{id}@b.c"), "h", None)
                .await
                .unwrap();
        }
        // 各自一个目录：失败注入要 chmod 目录（见 make_read_only），扔在 /tmp
        // 根下就只能去动 /tmp 本身。
        let dir = std::env::temp_dir().join(format!("aisix-sweeper-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resources.yaml").to_string_lossy().to_string();
        // 这份配置必须是网关**真能加载**的。第一版随手写了个残缺文档，结果
        // 每个用例走的都是「文件本来就坏、照写」那条降级路径 —— 写前校验那道闸
        // 在测试里完全没被执行过，而生产上它是开着的。
        std::fs::write(
            &path,
            format!(
                r#"_format_version: "1"
provider_keys:
  - display_name: stub
    provider: openai
    api_key: sk-stub
models:
  - display_name: keep-me
    provider: openai
    provider_key: stub
    model_name: keep-me
api_keys:
  - display_name: a
    key_hash: aa
    user_id: {uid}
    allowed_models: ["*"]
  - display_name: b
    key_hash: bb
    user_id: {other}
    allowed_models: ["*"]
  - display_name: ops
    key_hash: cc
    allowed_models: ["*"]
"#
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
    async fn 读数增长才扣_没有增长时不重复扣() {
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
    async fn 门户停摆期间的消费_恢复后一次入账() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        // 停摆十分钟，期间照常有消费。恢复后的第一轮必须把这一整段都扣上，
        // 否则停摆期间的消费就永远不入账了。
        f.src.add_spend(&f.uid, 7_000_000);
        f.sw.tick(t("2026-08-26T10:10:00Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 3_000_000);
    }

    #[tokio::test]
    async fn 读取失败时水位线不前进() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();

        // 每轮 2_000 的流量；失败那一轮也照常发生。
        f.src.set_increase(2_000);
        f.src.fail_for(&f.uid);
        let r = f.sw.tick(t("2026-08-26T10:05:00Z")).await.unwrap();
        assert_eq!(r.read_failures, 1);
        // 若此时把水位线推到当前时刻，这五分钟的消费就被跳过了 —— 又是一次
        // 静默白送。所以它必须还停在原处。
        assert_eq!(
            f.store.counted_through(&f.uid).await.unwrap(),
            Some(t("2026-08-26T10:00:00Z"))
        );

        // 读失败的那一轮消费照常发生；下一轮的读数包含它，一分不少。
        *f.src.fail_for.lock().unwrap() = None;
        f.sw.tick(t("2026-08-26T10:06:00Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 10_000_000 - 4_000);
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

    #[tokio::test]
    async fn 网关副本重启后计数从零重来_重启后的消费照扣不误() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        f.src.add_spend(&f.uid, 3_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 7_000_000);

        // 重启：这条序列从 0 重新累计，重启后又花了 $1。读数比游标小 —— 既不能
        // 当成负数，也不能当成「没花」。
        f.src.reset(&f.uid);
        f.src.add_spend(&f.uid, 1_000_000);
        f.sw.tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 6_000_000);
    }

    #[tokio::test]
    async fn 从落后的副本读到较小的值_不当成重置_也不扣() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        f.src.add_spend(&f.uid, 3_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 7_000_000);

        // 这一轮读到的是一个落后的 Prometheus 副本：比上次少 $1，但计数器没有
        // 重置过。当成重置的话，这条序列的全部 $2 会被再扣一遍。
        f.src.lag_once(&f.uid, 1_000_000);
        f.sw.tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 7_000_000);

        // 游标没有跟着倒退：之后只扣真正新增的那 $1。
        f.src.add_spend(&f.uid, 1_000_000);
        f.sw.tick(t("2026-08-26T10:00:45Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 6_000_000);
    }

    #[tokio::test]
    async fn 重启后又涨过上次读数_仍按重启后的累计扣() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        f.src.add_spend(&f.uid, 3_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 7_000_000);

        // 重启之后、下一次读数之前就花了 $5 —— 读数 $5 比上次的 $3 大。只在读数
        // 变小时才查重置的话，这里只会扣 $5 − $3 = $2，$3 白用。
        f.src.reset(&f.uid);
        f.src.add_spend(&f.uid, 5_000_000);
        f.sw.tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 2_000_000);
    }

    #[tokio::test]
    async fn 新副本的序列从零算起_第一笔就扣() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        // 扩容出来的副本：一条门户从没见过的序列，第一次被读到时已经是 $2。
        // 按时间窗算的话这一笔会被丢掉（新序列在窗口里只有一个采样点）。
        f.src.series(&f.uid, "gw-2", 2_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 8_000_000);
    }

    #[tokio::test]
    async fn 新用户第一次被读到之前的消费照扣() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        // 注册、拿额度、铸密钥、用掉 $4，全赶在门户第一轮对账之前。新用户的计数器
        // 生来就是 0，读到的 $4 全是他花的；按「第一次只建游标」处理就白用了。
        f.src.add_spend(&f.uid, 4_000_000);
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 6_000_000);

        // 之后照常按增量扣，不重复。
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 6_000_000);
    }

    #[tokio::test]
    async fn 从按时间窗计费的版本升级上来_第一轮不把历史当新序列扣() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        // 升级前的库：有对账时刻，没有游标。
        f.store
            .set_counted_through(&f.uid, t("2026-08-26T09:59:45Z"))
            .await
            .unwrap();
        f.src.add_spend(&f.uid, 9_000_000);
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        // 每条序列在游标表里都没有记录；若按「新序列」处理，整段历史 $9 会被
        // 一次扣掉。
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 10_000_000);

        f.src.add_spend(&f.uid, 1_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 9_000_000);
    }

    #[tokio::test]
    async fn 重启没被resets看出来_靠进程启动时刻认出_重启前后的消费都扣() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.src.add_spend(&f.uid, 1_000_000);
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 9_000_000);

        // 重启前又花 $1，还没被读到，副本就重启了；重启后花了 $3，比重启前的
        // 累计 $2 大，抓到的值只是从 2 涨到 3。
        f.src.add_spend(&f.uid, 1_000_000);
        f.src.restart_unseen_by_resets(&f.uid);
        f.src.add_spend(&f.uid, 3_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        // 重启前没读到的那 $1 计数器已经带走了，读不回来；重启后的 $3 一分不少。
        // 按「值涨了就扣差」只会扣 $3 - $1 = $2。
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 6_000_000);

        f.src.add_spend(&f.uid, 500_000);
        f.sw.tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 5_500_000);
    }

    #[tokio::test]
    async fn 落后的副本跨过一次重启_报的是旧进程_不当成重启也不重复扣() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 20_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.src.add_spend(&f.uid, 5_000_000);
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 15_000_000);

        // 网关重启（启动时刻 0 → 1），重启后花 $2，读到的是已经抓到新进程的副本。
        f.src.reset(&f.uid);
        f.src.add_spend(&f.uid, 2_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 13_000_000);

        // 下一轮落到另一个副本：它还没抓到重启，报的是旧进程的 $5。
        f.src.stale_replica_once(&f.uid, 5_000_000, 0);
        f.sw.tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 13_000_000);

        // 回到新副本，又花了 $2：只扣这 $2。
        f.src.add_spend(&f.uid, 2_000_000);
        f.sw.tick(t("2026-08-26T10:00:45Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 11_000_000);
    }

    #[tokio::test]
    async fn 升级前记下的游标没有启动时刻_不当成重启() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.src.add_spend(&f.uid, 1_000_000);
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 9_000_000);
        // 旧网关不暴露启动时刻时写下的游标。
        sqlx::query("UPDATE spend_cursor SET instance_started_ms = NULL")
            .execute(f.store.pool())
            .await
            .unwrap();

        f.src.add_spend(&f.uid, 1_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();
        // 当成重启就会把累计 $2 整个再扣一遍。
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 8_000_000);
    }

    #[tokio::test]
    async fn 三十天没再出现的序列_游标被剪掉_仍在读数里的与刚消失的不剪() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        // 三个副本各花了 $1。
        for gw in ["gw-1", "gw-gone", "gw-recent"] {
            f.src.series(&f.uid, gw, 1_000_000);
        }
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 7_000_000);

        // gw-1 与 gw-gone 的游标三十多天没更新过，gw-recent 的是刚写的；之后
        // gw-gone 与 gw-recent 的 pod 没了，读数里只剩 gw-1。
        let long_ago = (Utc::now() - chrono::TimeDelta::days(31)).to_rfc3339();
        sqlx::query("UPDATE spend_cursor SET updated_at = ?1 WHERE series NOT LIKE '%gw-recent%'")
            .bind(&long_ago)
            .execute(f.store.pool())
            .await
            .unwrap();
        f.src.drop_instance(&f.uid, "gw-gone");
        f.src.drop_instance(&f.uid, "gw-recent");
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();

        let key = |gw: &str| series_key(&(f.uid.clone(), None, gw.to_string()));
        let mut left: Vec<String> = f
            .store
            .billing_basis(&f.uid)
            .await
            .unwrap()
            .cursors
            .into_keys()
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![key("gw-1"), key("gw-recent")],
            "只该剪掉既久未更新、又不在这次读数里的那条"
        );
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 7_000_000);

        // 同一个地址上后来起了一个新进程（新 pod 拿到了同一个 IP:端口）：它的计数器
        // 从 0 起，只扣它自己的 $0.4，旧进程那 $1 不会再扣一遍。
        f.src.series(&f.uid, "gw-gone", 400_000);
        f.sw.tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 6_600_000);
    }

    /// 读完之后在栅栏上等另一个实例也读完：两边都在对方入账之前拿到同一份依据。
    struct BothReadFirst {
        inner: Arc<FakeSource>,
        gate: Arc<tokio::sync::Barrier>,
    }

    impl ConsumptionSource for BothReadFirst {
        async fn spend_series(
            &self,
            user_id: &str,
            since: DateTime<Utc>,
            at: DateTime<Utc>,
        ) -> Option<Vec<SpendSeries>> {
            let read = self.inner.spend_series(user_id, since, at).await;
            self.gate.wait().await;
            read
        }
    }

    #[tokio::test]
    async fn 两个门户实例交错对账_同一段消费只扣一次() {
        let f = fx().await;
        // 两个实例：同一个库文件、各自的连接池 —— 两个进程就是这样共用一个库的。
        let dir = std::path::Path::new(&f.path)
            .parent()
            .unwrap()
            .to_path_buf();
        let db = format!("sqlite:{}", dir.join("portal.db").display());
        let a = Store::open(&db).await.unwrap();
        let b = Store::open(&db).await.unwrap();
        a.insert_user(&f.uid, "u1@b.c", "h", None).await.unwrap();
        let ledger = Ledger::new(a.clone());
        ledger
            .credit(&f.uid, 10_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        let plain = |store: &Store| {
            Sweeper::new(
                store.clone(),
                f.src.clone(),
                crate::resources::Writer::new(f.path.clone()),
            )
        };

        // 先对一轮账，建好游标。
        f.src.add_spend(&f.uid, 1_000_000);
        plain(&a).tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        assert_eq!(ledger.balance(&f.uid).await.unwrap(), 9_000_000);

        // 又花了 $3。两个实例都在对方入账之前读完：同一份游标、同一个读数。
        f.src.add_spend(&f.uid, 3_000_000);
        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let gated = |store: &Store| {
            Sweeper::new(
                store.clone(),
                BothReadFirst {
                    inner: f.src.clone(),
                    gate: gate.clone(),
                },
                crate::resources::Writer::new(f.path.clone()),
            )
        };
        let (sa, sb) = (gated(&a), gated(&b));
        let at = t("2026-08-26T10:00:15Z");
        let (ra, rb) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            tokio::join!(sa.tick(at), sb.tick(at))
        })
        .await
        .expect("两个实例互相等住了");
        let (ra, rb) = (ra.unwrap(), rb.unwrap());

        // 各按自己算出的差额入账的话，这 $3 被扣两次。
        assert_eq!(ledger.balance(&f.uid).await.unwrap(), 6_000_000);
        assert_eq!(
            ra.debited + rb.debited,
            1,
            "只该有一个实例入账: {ra:?} / {rb:?}"
        );

        // 让出去的那个实例留下的游标照样对：之后只扣新增的那 $0.5。
        f.src.add_spend(&f.uid, 500_000);
        plain(&b).tick(t("2026-08-26T10:00:30Z")).await.unwrap();
        assert_eq!(ledger.balance(&f.uid).await.unwrap(), 5_500_000);
    }

    #[test]
    fn 合并查询的各段按序列归并() {
        let body = serde_json::json!({
            "status": "success",
            "data": {"resultType": "vector", "result": [
                {"metric": {"__name__": "aisix_llm_spend_micro_usd_total", "aisix_part": "value",
                            "user_id": "u1", "instance": "b:9090", "api_key_id": "k1"},
                 "value": [1, "1500"]},
                {"metric": {"aisix_part": "resets_upto", "user_id": "u1", "instance": "b:9090",
                            "api_key_id": "k1"},
                 "value": [1, "3"]},
                {"metric": {"aisix_part": "resets_before", "user_id": "u1", "instance": "b:9090",
                            "api_key_id": "k1"},
                 "value": [1, "2"]},
                {"metric": {"__name__": "aisix_llm_spend_micro_usd_total", "aisix_part": "value",
                            "job": "aisix", "instance": "a:9090", "user_id": "u1"},
                 "value": [1, "20"]},
                {"metric": {"__name__": "process_start_time_seconds", "aisix_part": "started",
                            "job": "aisix", "instance": "a:9090"},
                 "value": [1, "1760436000.25"]},
                // 别的 job 上同名实例的启动时刻不能串到这条序列上。
                {"metric": {"__name__": "process_start_time_seconds", "aisix_part": "started",
                            "job": "other", "instance": "b:9090"},
                 "value": [1, "1760430000"]},
            ]},
        });
        let series = parse_spend_series(&body).expect("可解析");
        assert_eq!(
            series,
            vec![
                SpendSeries {
                    key: r#"api_key_id="k1",instance="b:9090",user_id="u1""#.into(),
                    api_key_id: Some("k1".into()),
                    value: 1500,
                    resets_since: 1,
                    started_ms: None,
                },
                SpendSeries {
                    key: r#"instance="a:9090",job="aisix",user_id="u1""#.into(),
                    api_key_id: None,
                    value: 20,
                    resets_since: 0,
                    started_ms: Some(1_760_436_000_250),
                },
            ]
        );
    }

    #[test]
    fn 回答本身不对才整体读不到_坏的单条只跳过它() {
        assert_eq!(
            parse_spend_series(&serde_json::json!({"status": "error", "error": "bad query"})),
            None
        );
        let empty = serde_json::json!({"status": "success", "data": {"result": []}});
        assert_eq!(parse_spend_series(&empty), Some(Vec::new()));

        // 一条值是 NaN 的序列不能拖着同一个人的其它序列一起扣不到。
        let body = serde_json::json!({"status": "success", "data": {"result": [
            {"metric": {"aisix_part": "value", "user_id": "u1", "instance": "a"},
             "value": [1, "NaN"]},
            {"metric": {"aisix_part": "value", "user_id": "u1", "instance": "b"},
             "value": [1, "7"]},
        ]}});
        let series = parse_spend_series(&body).expect("整体可读");
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].value, 7);
    }
}

#[cfg(test)]
mod allowance_pushdown_tests {
    use super::tests::{fx, t};
    use crate::ledger::Source;

    fn policy_granted(path: &str, uid: &str) -> Option<i64> {
        let doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(path).unwrap()).ok()?;
        doc.get("rate_limit_policies")?
            .as_sequence()?
            .iter()
            .find(|p| {
                p.get("name").and_then(serde_yaml_ng::Value::as_str)
                    == Some(&format!("portal-allowance-{uid}"))
            })?
            .get("granted_micro_usd")?
            .as_i64()
    }

    #[tokio::test]
    async fn 累计发放总额被写成一条策略() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        assert_eq!(policy_granted(&f.path, &f.uid), Some(5_000_000));
    }

    #[tokio::test]
    async fn 补额之后策略跟着涨_而不是新增一条() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        f.ledger
            .credit(&f.uid, 3_000_000, Source::Topup, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();

        assert_eq!(policy_granted(&f.path, &f.uid), Some(8_000_000));
        let doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
        // 策略名按用户派生，所以只该有一条 —— 每轮新增一条会让配置无限膨胀，
        // 而且哪条生效变得无法预料。
        assert_eq!(doc["rate_limit_policies"].as_sequence().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn 消费不影响下推的数字() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        f.src.set_increase(2_000_000);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();

        // 下推的是**累计发放**，不是余额。网关那边自己累计消费，两个单调量
        // 相减才是剩余 —— 把余额推下去会在网关再减一次，等于扣两遍。
        assert_eq!(policy_granted(&f.path, &f.uid), Some(5_000_000));
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 3_000_000);
    }

    #[tokio::test]
    async fn 没发放过的用户不生成策略() {
        let f = fx().await;
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        assert_eq!(policy_granted(&f.path, &f.uid), None);
    }

    #[tokio::test]
    async fn 无变化时不重写配置() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-26T10:00:00Z")).await.unwrap();
        let after_first = std::fs::read_to_string(&f.path).unwrap();

        let mtime_first = std::fs::metadata(&f.path).unwrap().modified().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        f.src.set_increase(0);
        f.sw.tick(t("2026-08-26T10:00:15Z")).await.unwrap();

        assert_eq!(std::fs::read_to_string(&f.path).unwrap(), after_first);
        // 比 mtime 而不是比内容：无条件重写写回去的字节是一样的，只有写这个
        // 动作本身能被看见 —— 而它每次都会让网关白重载一遍。
        assert_eq!(
            std::fs::metadata(&f.path).unwrap().modified().unwrap(),
            mtime_first,
            "无变化也重写了配置 —— 网关会因此白重载",
        );
    }
}

/// 每把密钥自己的额度下推成 `api_key` 域的策略。
///
/// 用户域那条（`portal-allowance-<uid>`）管的是「这个人总共能花多少」，这里
/// 管的是「这把密钥能花多少」。两条各自成闸，网关取更严的那个。
#[cfg(test)]
mod key_allowance_tests {
    use super::tests::{fx, t};
    use serde_yaml_ng::Value;

    fn policies(path: &str) -> Vec<Value> {
        let doc: Value = serde_yaml_ng::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        doc.get("rate_limit_policies")
            .and_then(Value::as_sequence)
            .cloned()
            .unwrap_or_default()
    }

    fn policy(path: &str, name: &str) -> Option<Value> {
        policies(path)
            .into_iter()
            .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
    }

    #[tokio::test]
    async fn 密钥额度下推成_api_key_域的策略() {
        let f = fx().await;
        f.store.set_key_quota(&f.uid, "a", 3_000_000).await.unwrap();
        f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();

        let p = policy(&f.path, "portal-key-a").expect("没有下推 portal-key-a");
        assert_eq!(p.get("scope").and_then(Value::as_str), Some("api_key"));
        assert_eq!(
            p.get("granted_micro_usd").and_then(Value::as_i64),
            Some(3_000_000)
        );
        // scope_ref 是密钥的**名字**，网关自己去解析成 id。
        //
        // 这条断言原本写的是「等于派生 id」—— 实现和测试一起错，单测全绿，直到
        // 真网关拒收整份配置才暴露。所以这里额外钉一句：那串不能是 uuid 形状。
        assert_eq!(p.get("scope_ref").and_then(Value::as_str), Some("a"));
        assert!(
            uuid::Uuid::parse_str(p.get("scope_ref").and_then(Value::as_str).unwrap()).is_err(),
            "scope_ref 又写成了 uuid —— 网关会把整份配置当成引用了不存在的密钥而整体拒收",
        );
    }

    #[tokio::test]
    async fn 改了额度_策略里的数跟着改() {
        let f = fx().await;
        f.store.set_key_quota(&f.uid, "a", 3_000_000).await.unwrap();
        f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();
        f.store.set_key_quota(&f.uid, "a", 9_000_000).await.unwrap();
        f.sw.tick(t("2026-08-27T10:00:15Z")).await.unwrap();

        let p = policy(&f.path, "portal-key-a").unwrap();
        assert_eq!(
            p.get("granted_micro_usd").and_then(Value::as_i64),
            Some(9_000_000)
        );
        // 改额度不该多出一条同名策略 —— 重名会让加载方按第一条生效，改的那条
        // 永远不起作用。
        assert_eq!(
            policies(&f.path)
                .iter()
                .filter(|p| p.get("name").and_then(Value::as_str) == Some("portal-key-a"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn 额度清掉后_那条策略被撤掉() {
        let f = fx().await;
        f.store.set_key_quota(&f.uid, "a", 3_000_000).await.unwrap();
        f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();
        f.store.drop_key_quota(&f.uid, "a").await.unwrap();
        f.sw.tick(t("2026-08-27T10:00:15Z")).await.unwrap();

        assert!(
            policy(&f.path, "portal-key-a").is_none(),
            "额度已清掉，策略还挂在配置里"
        );
    }

    #[tokio::test]
    async fn 不碰用户域那条策略_也不碰运维手写的() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        f.store.set_key_quota(&f.uid, "a", 1_000_000).await.unwrap();
        f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();

        // 撤旧策略那一步按 `portal-key-` 前缀筛。筛错了就会把用户域的闸、
        // 或者运维自己写的策略一起删掉 —— 静默地把限流全放开。
        assert!(
            policy(&f.path, &format!("portal-allowance-{}", f.uid)).is_some(),
            "用户域的额度策略被误删了"
        );
        assert!(policy(&f.path, "portal-key-a").is_some());
    }

    /// 把配置文件设成只读：读得到，写不进去。
    fn make_read_only(path: &str) {
        // 改**目录**的权限：写盘走「同目录临时文件 + rename」，只把文件设成
        // 只读拦不住 —— 目录可写就照样能覆盖过去。
        let dir = std::path::Path::new(path).parent().unwrap();
        let mut perm = std::fs::metadata(dir).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(dir, perm).unwrap();
    }

    // 下面三条各自安排成「只有那一步需要写盘」，否则断言分不清是谁失败的 ——
    // 第一版就是这么写的：三步共用一个计数器，删掉任何一步的错误传递都照样绿。

    #[tokio::test]
    async fn 额度下推写不进去时被记下来() {
        let f = fx().await;
        // 有额度要推（这一步要写）；余额为正、密钥本来就是启用态（那一步不写）；
        // 没有密钥额度（那一步也不写）。
        //
        // **两个用户都要给额度。** 夹具里有 u2，漏掉它的话它余额为零、这一轮要
        // 被停用，于是停用那一步也写了盘 —— 计数变成 2，隔离就没了。
        for u in [&f.uid, &f.other] {
            f.ledger
                .credit(u, 5_000_000, crate::ledger::Source::AdminGrant, None)
                .await
                .unwrap();
        }
        make_read_only(&f.path);
        let r = f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();
        assert_eq!(r.write_failures, 1, "额度下推的写入失败没被记下来: {r:?}");
    }

    #[tokio::test]
    async fn 密钥额度下推写不进去时被记下来() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        f.store.set_key_quota(&f.uid, "a", 1_000_000).await.unwrap();
        // 先让用户级那条策略落地，这样本轮只剩密钥级那一步需要写。
        f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();
        f.store.set_key_quota(&f.uid, "a", 2_000_000).await.unwrap();
        make_read_only(&f.path);
        let r = f.sw.tick(t("2026-08-27T10:00:15Z")).await.unwrap();
        assert_eq!(
            r.write_failures, 1,
            "密钥额度下推的写入失败没被记下来: {r:?}"
        );
    }

    #[tokio::test]
    async fn 停用密钥写不进去时被记下来() {
        let f = fx().await;
        // 余额为零 → 这一轮必须把密钥写成停用，只有这一步需要写盘。
        make_read_only(&f.path);
        let r = f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();
        assert_eq!(r.write_failures, 1, "停用写入失败没被记下来: {r:?}");
        // 而且不能报成「停用了 N 把」—— 那会让账面显示已停用、实际仍在放行。
        assert_eq!(r.disabled, 0, "写盘失败却报了停用数: {r:?}");
    }

    /// 剪除残留时，额度表必须先于配置文件被读到。
    ///
    /// 顺序反了就有一个窗口：两次读取之间新铸并设了额度的密钥，既不在旧的文件
    /// 快照里、又已经在新的额度表里，于是被当成残留删掉。窗口只有一次数据库
    /// 往返，症状是「刚设的额度自己没了」，无从复现。
    ///
    /// 钉源码序而不是去撞那个竞态：那种测试要么永远绿，要么偶尔红，两种都没用。
    #[test]
    fn 剪除残留时先取额度表再读配置() {
        let src = include_str!("sweeper.rs");
        let body = src
            .split_once("async fn prune_orphan_quotas(")
            .and_then(|(_, rest)| rest.split_once("\n    }\n"))
            .map(|(b, _)| b)
            .expect("找不到 prune_orphan_quotas");
        let snapshot = body
            .find("let snapshot = self.store.all_key_quotas()")
            .expect("没有取额度表");
        let read = body
            .find(".resources\n            .read()")
            .expect("没有读配置");
        assert!(
            snapshot < read,
            "先读了配置才取额度表 —— 中间新设的额度会被当成残留删掉",
        );
    }

    #[test]
    fn 坏读数当成读不到_而不是零也不是天文数字() {
        assert_eq!(super::plausible_spend(1_500.0), Some(1_500));
        assert_eq!(super::plausible_spend(0.0), Some(0));
        // 负的按 0：increase() 不该为负，一点点负值是外推舍入。
        assert_eq!(super::plausible_spend(-3.0), Some(0));

        // NaN 当成 0 的话，水位线照推，那段窗口的真实消费永久丢失。
        assert_eq!(super::plausible_spend(f64::NAN), None, "NaN 被当成了花了 0");
        // +Inf / 超大值经 `as u64` 饱和成 u64::MAX，会给用户记一笔 i64::MAX
        // 的欠款 —— 余额深度为负、密钥永久停用，管理员填不平。
        assert_eq!(super::plausible_spend(f64::INFINITY), None, "Inf 被入账了");
        assert_eq!(super::plausible_spend(1e30), None, "天文数字被入账了");
    }

    #[tokio::test]
    async fn 额度归零或用户消失后_用户域策略被撤掉() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-28T10:00:00Z")).await.unwrap();
        let name = format!("portal-allowance-{}", f.uid);
        assert!(policy(&f.path, &name).is_some());

        // 直接删库行 —— 门户没有删用户的接口，运维只能这么做。
        sqlx::query("DELETE FROM consumption_mark WHERE user_id = ?1")
            .bind(&f.uid)
            .execute(f.store.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM ledger WHERE user_id = ?1")
            .bind(&f.uid)
            .execute(f.store.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = ?1")
            .bind(&f.uid)
            .execute(f.store.pool())
            .await
            .unwrap();
        f.sw.tick(t("2026-08-28T10:00:15Z")).await.unwrap();

        // 留着的话它挂在一个谁也对不上的身份上，下一个读配置的人会以为那个人
        // 还有额度。
        assert!(
            policy(&f.path, &name).is_none(),
            "用户已不存在，用户域额度策略还留着"
        );
    }

    #[tokio::test]
    async fn 不碰运维手写的用户域策略() {
        let f = fx().await;
        // 运维自己写了一条 member 域策略，名字不带门户前缀。
        let doc = std::fs::read_to_string(&f.path).unwrap();
        std::fs::write(
            &f.path,
            format!("{doc}rate_limit_policies:\n- name: ops-member-cap\n  scope: member\n  scope_ref: someone\n  granted_micro_usd: 1\n"),
        )
        .unwrap();
        f.ledger
            .credit(&f.uid, 5_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-28T10:00:00Z")).await.unwrap();

        assert!(
            policy(&f.path, "ops-member-cap").is_some(),
            "把运维手写的策略删了"
        );
        assert!(policy(&f.path, &format!("portal-allowance-{}", f.uid)).is_some());
    }

    /// 一轮对账**最多写一次配置**。
    ///
    /// 三段改动以前各自写一次，于是一轮最多三次写盘、三次 SIGHUP，网关跟着重建
    /// 三遍快照；中间还会出现「额度推了、开关还没落」这种半成品状态。这条测试
    /// 数的是 mtime 变了几次 —— 比数内容更直接，因为白写一次的字节可能一样。
    #[tokio::test]
    async fn 一轮对账最多写一次配置() {
        let f = fx().await;
        // 让三段都有事情要做：要推用户额度、要推密钥额度、要停 u2 的密钥。
        f.ledger
            .credit(&f.uid, 5_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        f.store.set_key_quota(&f.uid, "a", 1_000_000).await.unwrap();

        let before = std::fs::metadata(&f.path).unwrap().modified().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let doc_before = std::fs::read_to_string(&f.path).unwrap();

        f.sw.tick(t("2026-08-28T10:00:00Z")).await.unwrap();

        let after = std::fs::metadata(&f.path).unwrap().modified().unwrap();
        assert_ne!(after, before, "什么都没写？三段都该有事做");
        let doc_after = std::fs::read_to_string(&f.path).unwrap();
        // 三段的结果都在这一次写里。
        assert!(
            doc_after.contains(&format!("portal-allowance-{}", f.uid)),
            "{doc_after}"
        );
        assert!(doc_after.contains("portal-key-a"), "{doc_after}");
        assert!(doc_after.contains("disabled: true"), "{doc_after}");
        assert_ne!(doc_before, doc_after);

        // 再跑一轮，什么都没变 —— 不该再写盘（否则网关每轮白重载一次）。
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        f.sw.tick(t("2026-08-28T10:00:15Z")).await.unwrap();
        assert_eq!(
            std::fs::metadata(&f.path).unwrap().modified().unwrap(),
            after,
            "无变化也写了盘"
        );
    }

    /// 源码序守卫：`tick` 里只能有一次 `.edit(`。
    ///
    /// 上面那条测 mtime 的用例只能发现「同一轮写了两次」，而两次写之间若字节相同、
    /// 或者第二次恰好也在同一秒里，它就看不出来。这条直接数调用点。
    #[test]
    fn tick_里只有一次写盘调用() {
        let src = include_str!("sweeper.rs");
        let body = src
            .split_once("pub async fn tick(&self, now: DateTime<Utc>)")
            .and_then(|(_, rest)| rest.split_once("\n    }\n"))
            .map(|(b, _)| b)
            .expect("找不到 tick");
        assert_eq!(
            body.matches(".edit(").count(),
            1,
            "tick 里有不止一次写盘 —— 每一次都会让网关重载一遍",
        );
    }

    /// 密钥级查询用的窗口，必须跟用户级那一段用的是同一个、且不是空的。
    ///
    /// 窗口算错成零长度时，`increase(...[0s])` 恒为 0 —— 花费永远累计不到，而
    /// 一切看起来都在正常运转。这条直接看被问过的窗口，不看结果。
    /// 数据库出问题要让**整轮失败**，不能报成「配置写入失败」。
    ///
    /// 两者的处置完全不同：`config_write_failures` 的含义是「网关正在用旧配置」，
    /// 会据此报警去看网关；而库读不动是另一回事。混在一起，运维会照着错误的
    /// 方向查。
    #[tokio::test]
    async fn 库出问题让整轮失败_而不是报成配置写入失败() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 5_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        // 确定性地制造一次库失败：把表删掉。
        sqlx::query("DROP TABLE key_quotas")
            .execute(f.store.pool())
            .await
            .unwrap();

        let r = f.sw.tick(t("2026-08-28T10:00:00Z")).await;
        assert!(r.is_err(), "库读不动了，这一轮却报成功: {r:?}");
    }

    #[tokio::test]
    async fn 密钥花费与用户扣款出自同一次读数() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 50_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        f.store.set_key_quota(&f.uid, "a", 2_000_000).await.unwrap();
        // 第一轮只建游标。
        f.sw.tick(t("2026-08-28T10:00:00Z")).await.unwrap();
        f.src.set_key_increase(&f.uid, "a", 1_500_000);
        f.sw.tick(t("2026-08-28T10:00:30Z")).await.unwrap();

        // 同一笔花费，用户这边扣多少，密钥这边就记多少 —— 不一致的话两边的账
        // 对不上。
        assert_eq!(f.ledger.balance(&f.uid).await.unwrap(), 48_500_000);
        assert_eq!(
            f.store.key_spend(&f.uid).await.unwrap(),
            vec![("a".to_string(), 1_500_000)]
        );
    }

    #[tokio::test]
    async fn 读花费失败的那一轮_密钥级也不记() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 50_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        f.store.set_key_quota(&f.uid, "a", 2_000_000).await.unwrap();
        f.sw.tick(t("2026-08-28T10:00:00Z")).await.unwrap();

        // 这一轮用户级读失败 → 水位线不动。密钥级若照样记，同一段花费会被这轮
        // 和下一轮各记一次。
        f.src.fail_for(&f.uid);
        f.src.set_key_increase(&f.uid, "a", 5_000_000);
        f.sw.tick(t("2026-08-28T10:00:30Z")).await.unwrap();
        assert_eq!(
            f.store.key_spend(&f.uid).await.unwrap(),
            Vec::new(),
            "用户级读失败的那一轮，密钥级却记了账"
        );
    }

    #[tokio::test]
    async fn 某把密钥花超自己的额度后_只停这一把() {
        let f = fx().await;
        f.ledger
            .credit(&f.uid, 500_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        // a 有 $2 的子额度，b 不设限。两把都属于 u1？不是 —— 夹具里 b 属于 u2，
        // 所以这里再给 u2 也发额度，免得它因为余额为零被停用而混淆断言。
        f.ledger
            .credit(
                &f.other,
                500_000_000,
                crate::ledger::Source::AdminGrant,
                None,
            )
            .await
            .unwrap();
        f.store.set_key_quota(&f.uid, "a", 2_000_000).await.unwrap();
        f.sw.tick(t("2026-08-28T10:00:00Z")).await.unwrap();
        assert!(!key_disabled(&f.path, "a"), "一开始不该停");

        // a 花掉 $3，越过它自己的 $2。用户余额还有 $500 —— 所以停它的理由只能
        // 是密钥级那道闸。网关侧那个计数器重启就归零，这条路径是持久的兜底。
        f.src.set_key_increase(&f.uid, "a", 3_000_000);
        f.sw.tick(t("2026-08-28T10:00:15Z")).await.unwrap();

        assert!(key_disabled(&f.path, "a"), "花超自己额度的密钥没被停用");
        // b 不受影响 —— 这是「每把各自一份」的全部含义。
        assert!(!key_disabled(&f.path, "b"), "把别的密钥一起停了");
    }

    #[tokio::test]
    async fn 累计花费跨轮累加_不是每轮单独比() {
        let f = fx().await;
        for u in [&f.uid, &f.other] {
            f.ledger
                .credit(u, 500_000_000, crate::ledger::Source::AdminGrant, None)
                .await
                .unwrap();
        }
        f.store.set_key_quota(&f.uid, "a", 2_500_000).await.unwrap();
        f.sw.tick(t("2026-08-28T10:00:00Z")).await.unwrap();

        // 每轮花 $1，三轮共 $3 越过 $2.5。按轮单独比的话每轮都是 $1 < $2.5，
        // 永远不会停 —— 那正是「续杯」这个 bug 的形状。
        f.src.set_key_increase(&f.uid, "a", 1_000_000);
        f.sw.tick(t("2026-08-28T10:00:15Z")).await.unwrap();
        assert!(!key_disabled(&f.path, "a"), "$1 就停了？");
        f.sw.tick(t("2026-08-28T10:00:30Z")).await.unwrap();
        assert!(!key_disabled(&f.path, "a"), "$2 就停了？");
        f.sw.tick(t("2026-08-28T10:00:45Z")).await.unwrap();
        assert!(key_disabled(&f.path, "a"), "累计 $3 越过 $2.5 却没停");
    }

    #[tokio::test]
    async fn 账号被停用后_他的密钥也在网关侧被停掉() {
        let f = fx().await;
        // 余额充足 —— 只有账号状态这一个理由要停他。
        f.ledger
            .credit(&f.uid, 50_000_000, crate::ledger::Source::AdminGrant, None)
            .await
            .unwrap();
        f.sw.tick(t("2026-08-28T10:00:00Z")).await.unwrap();
        assert!(!key_disabled(&f.path, "a"), "余额充足时不该停用");

        sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?1")
            .bind(&f.uid)
            .execute(f.store.pool())
            .await
            .unwrap();
        let r = f.sw.tick(t("2026-08-28T10:00:15Z")).await.unwrap();

        // 只在登录处看这个字段的话，人登不进门户、密钥却照常放行 —— 而「停用
        // 这个用户」正是要切断他。控制台还把这种账号显示成「已停用」。
        assert!(
            key_disabled(&f.path, "a"),
            "账号已停用，他的密钥在网关侧仍然是启用的"
        );
        assert_eq!(r.disabled, 1);
    }

    /// 配置里这把密钥是不是停用态。
    fn key_disabled(path: &str, name: &str) -> bool {
        let doc: Value = serde_yaml_ng::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        doc.get("api_keys")
            .and_then(Value::as_sequence)
            .and_then(|s| {
                s.iter()
                    .find(|k| k.get("display_name").and_then(Value::as_str) == Some(name))
            })
            .and_then(|k| k.get("disabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn 密钥已不在配置里时不下推它的策略() {
        let f = fx().await;
        // 额度记录还在，密钥却已经从配置里消失了（比如运维从控制台删掉了它）。
        f.store
            .set_key_quota(&f.uid, "早就没了", 1_000_000)
            .await
            .unwrap();
        f.store.set_key_quota(&f.uid, "a", 1_000_000).await.unwrap();
        f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();

        // 指着不存在的密钥的策略会让网关**整份拒收**配置 —— 于是门户此后每次
        // 写入都被写前校验挡下，停用闸也跟着停摆。
        assert!(policy(&f.path, "portal-key-早就没了").is_none());
        // 同一轮里正常那把仍要照常下推，不能一竿子全停。
        assert!(policy(&f.path, "portal-key-a").is_some());
    }

    #[tokio::test]
    async fn 只给有额度的密钥下推() {
        let f = fx().await;
        f.store.set_key_quota(&f.uid, "a", 2_000_000).await.unwrap();
        // 额度是 0 的那条也不该下推：0 额度的闸不是「不限」，是「一分钱都
        // 不许花」—— 会把一把本该不受限的密钥直接锁死。
        f.store.set_key_quota(&f.uid, "b", 0).await.unwrap();
        f.sw.tick(t("2026-08-27T10:00:00Z")).await.unwrap();
        assert!(policy(&f.path, "portal-key-b").is_none());
        // 压根没有额度记录的密钥同样不该有策略。
        assert!(policy(&f.path, "portal-key-ops").is_none());
    }
}
