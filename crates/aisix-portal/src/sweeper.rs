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
    /// 配置写入失败的次数。
    ///
    /// 单独记一格，因为这几步原本用 `let _ =` 把错误全丢了：写前校验一旦拒绝
    /// 这次改动（比如库里有条记录指向已不存在的密钥），对账环会照常报成功，
    /// 而额度从此再也推不下去 —— 没有任何东西会说出来。
    pub write_failures: usize,
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

        // 把累计发放总额下推给网关。
        //
        // 这才是真正的闸：网关按 `granted_micro_usd` 比对自己那个只增不减的
        // 消费计数器，精确、跨天不续杯、无需对账。下面停用密钥的那一段退居
        // **兜底** —— 它有轮询周期的滞后，而这一条没有。
        let mut grants: Vec<(String, i64)> = Vec::new();
        for u in &users {
            grants.push((u.id.clone(), self.ledger.total_granted(&u.id).await?));
        }
        if let Err(e) = self.apply_allowances(&grants).await {
            eprintln!("下推额度失败（网关仍在用旧值）: {e}");
            r.write_failures += 1;
        }

        // 密钥级额度。用户把自己的总额分配到各把密钥上，网关按 `scope: api_key`
        // 单独收口每一把 —— 用户级那条策略仍然在，所以「总花销不超过用户额度」
        // 由它保证，密钥级只是在此之下再切分。
        if let Err(e) = self.apply_key_allowances().await {
            eprintln!("下推密钥额度失败（网关仍在用旧值）: {e}");
            r.write_failures += 1;
        }

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
        match self.apply_key_state(&wanted).await {
            Ok((off, on)) => {
                r.disabled = off;
                r.reenabled = on;
            }
            Err(e) => {
                eprintln!("停用/启用密钥写入失败（网关仍在放行）: {e}");
                r.write_failures += 1;
            }
        }
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
    async fn apply_key_allowances(&self) -> Result<(), String> {
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
        let snapshot = self.quotas().await?;
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
                    self.store
                        .drop_key_quota(uid, name)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
        }

        let quotas = self.quotas().await?;
        self.resources
            .edit(move |doc| {
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
            })
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// 额度记录。折成文字错误：调用方只用它记日志与计数，不按类型分支。
    async fn quotas(&self) -> Result<Vec<(String, String, i64)>, String> {
        self.store.all_key_quotas().await.map_err(|e| e.to_string())
    }

    /// 把每个用户的累计发放总额写成一条 `scope: member` 的策略。
    ///
    /// 策略名按用户派生，所以重复运行是幂等的：已经一致就不写盘、不发 SIGHUP。
    /// 无条件重写会让文件 mtime 一直跳、网关白重载。
    ///
    /// 只写 `granted_micro_usd`，不写窗口 —— 这条策略表达的是「总共给了多少」，
    /// 给它一个窗口会让读配置的人以为额度按那个周期刷新。
    async fn apply_allowances(&self, grants: &[(String, i64)]) -> Result<(), String> {
        let grants = grants.to_vec();
        self.resources
            .edit(move |doc| {
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
                for (uid, granted) in &grants {
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
            })
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// 把期望的启停状态落到 `resources.yaml`。返回 (新停用数, 新启用数)。
    ///
    /// 走共用的 [`crate::resources::Writer`]：铸密钥也写这个文件，两条路径不
    /// 串行就会互相覆盖 —— 后写的那次带着自己读到的旧全文，把中间那次改动整段
    /// 抹掉。改动用泛型 `Value`，不经过窄结构体（那会抹掉门户不认识的字段）。
    async fn apply_key_state(&self, wanted: &[(String, bool)]) -> Result<(usize, usize), String> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // 原子计数而不是捕获可变引用：闭包是 FnMut、可能被重试调用多次，
        // 而这个 future 会被 tokio::spawn 送到别的线程上。
        let off = Arc::new(AtomicUsize::new(0));
        let on = Arc::new(AtomicUsize::new(0));
        let (o1, o2) = (off.clone(), on.clone());
        let wanted = wanted.to_vec();

        let wrote = self
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

        // 这一步落的是杀手闸。写失败还报「改了 N 把」的话，账面显示已停用、
        // 实际仍然在放行 —— 最不该被吞掉的一次失败。
        wrote.map_err(|e| e.to_string())?;
        Ok((off.load(Ordering::SeqCst), on.load(Ordering::SeqCst)))
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
        plausible_spend(crate::usage::scalar_from_prom(&body)?)
    }
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

    /// 一次被问过的窗口：(用户, from, to)。
    type Window = (String, DateTime<Utc>, DateTime<Utc>);

    /// 记录被问过的窗口，并按「时间窗 → 增量」应答 —— 正是 `increase()` 的语义。
    #[derive(Default)]
    pub(super) struct FakeSource {
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
        pub(super) fn set_increase(&self, v: u64) {
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
        let path = std::env::temp_dir()
            .join(format!("aisix-sweeper-{}.yaml", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string();
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
        let mut perm = std::fs::metadata(path).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(path, perm).unwrap();
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
            .split_once("async fn apply_key_allowances(")
            .and_then(|(_, rest)| rest.split_once("\n    }\n"))
            .map(|(b, _)| b)
            .expect("找不到 apply_key_allowances");
        let snapshot = body
            .find("let snapshot = self.quotas()")
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
