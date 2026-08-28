//! 余额与流水。
//!
//! 余额不是一个字段，而是流水的和。这样账永远能重算，也不存在「余额字段与
//! 流水对不上」这种无法收拾的状态。
//!
//! **事务边界说明。** 计划里写的是「写入在 BEGIN IMMEDIATE 事务内」，实施时
//! 判断不需要：一次记账就是一条 INSERT，单条语句本身原子，套事务只是仪式。
//! 真正需要原子的是控制环那一步 —— 「扣款」与「推进水位线」必须一起成或一起
//! 不成，中间崩了就会重复扣或漏扣；那个由 [`Ledger::debit_and_mark`] 承担。
//!
//! 金额一律 micro-USD 整数。浮点做钱会累积误差，而这个产品的花费到千分之一
//! 美分。

use crate::store::{Store, StoreError};

/// 一条流水。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub delta_micro_usd: i64,
    pub source: String,
    pub note: Option<String>,
}

/// 记账来源。枚举而不是裸字符串，四期接支付时加一个变体即可，credit 路径
/// 不用改（spec §8 未决问题 1）。
///
/// `Payment` 有意**现在不加**：四期不在本计划内，提前放一个永远构造不到的
/// 变体只会挂着一条 dead_code 警告。要用的时候再加。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    AdminGrant,
    Consumption,
    /// 线下充值单被管理员确认。真接支付后这条来源换成回调触发，账本不用改。
    Topup,
    /// 管理员把总额度**设定**成某个值。
    ///
    /// 账本仍然只追加：设定记的是「与当前总额的差」，可正可负。这样「谁在
    /// 什么时候把额度改成了多少」全部留痕，而余额与总额都还能从流水重算出来。
    AdminSet,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AdminGrant => "admin_grant",
            Self::Consumption => "consumption",
            Self::Topup => "topup",
            Self::AdminSet => "admin_set",
        }
    }
}

#[derive(Clone)]
pub struct Ledger {
    store: Store,
}

impl Ledger {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// 入账。金额取 `u64`，所以「发放负数」在类型上就不可能 —— 调用方从
    /// JSON 拿到负数时必须自己先挡掉（见管理端 API）。
    pub async fn credit(
        &self,
        user_id: &str,
        micro_usd: u64,
        source: Source,
        note: Option<&str>,
    ) -> Result<(), StoreError> {
        self.insert(
            user_id,
            i64::try_from(micro_usd).unwrap_or(i64::MAX),
            source,
            note,
        )
        .await
    }

    async fn insert(
        &self,
        user_id: &str,
        delta: i64,
        source: Source,
        note: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut c = self.store.pool().acquire().await?;
        insert_entry(&mut *c, user_id, delta, source, note).await
    }

    /// 余额 = 流水之和。
    ///
    /// 每次读都扫一遍该用户的流水（走 `ledger_user` 索引）。一期的量级下这没
    /// 问题；真要长到扫不动了再物化，那时也仍然以流水为准、物化值只是缓存。
    pub async fn balance(&self, user_id: &str) -> Result<i64, StoreError> {
        let (sum,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(delta_micro_usd), 0) FROM ledger WHERE user_id = ?1",
        )
        .bind(user_id)
        .fetch_one(self.store.pool())
        .await?;
        Ok(sum)
    }

    /// 总额度 —— 除消费之外的一切之和。
    ///
    /// 按**来源**而不是按正负判断，这一点是必须的：管理员调减额度会记一条负数
    /// 流水，按正负筛的话它会被漏掉，总额只涨不落，网关那边的闸就跟着降不下来。
    ///
    /// 这是下推给网关的那个数。网关那边只累加消费、从不重置，所以「还剩多少」
    /// 是两个量的差，不需要任何对账去校正。
    pub async fn total_granted(&self, user_id: &str) -> Result<i64, StoreError> {
        let mut c = self.store.pool().acquire().await?;
        total_granted_on(&mut *c, user_id).await
    }

    /// 把总额度设定成 `target`，记一条差额流水。返回实际写入的差额。
    ///
    /// 目标低于已消费是允许的：那就是「我把你的额度调到这个数」，网关会立刻
    /// 拒绝后续请求。不允许的是负的目标值 —— 那没有意义。
    /// 读当前总额、算差额、记流水 —— **一个事务**。分开做的话两个并发的设定
    /// 会各自读到同一份旧值，各写一条差额，最终总额既不是这个目标也不是那个
    /// （设 8 与设 2 同时发生，结果停在原来的 5）。
    pub async fn set_total_granted(
        &self,
        user_id: &str,
        target: i64,
        note: Option<&str>,
    ) -> Result<i64, StoreError> {
        let user_id = user_id.to_string();
        let note = note.map(str::to_string);
        self.store
            .immediate_tx(move |conn| {
                Box::pin(async move {
                    let current = total_granted_on(&mut *conn, &user_id).await?;
                    // `target - current` 会溢出（target 接近 i64::MAX 而 current
                    // 为负时）。release 下回绕成一个符号相反的差额，账本从此对不上。
                    let Some(delta) = target.checked_sub(current) else {
                        return Err(StoreError::OutOfRange);
                    };
                    if delta != 0 {
                        insert_entry(
                            &mut *conn,
                            &user_id,
                            delta,
                            Source::AdminSet,
                            note.as_deref(),
                        )
                        .await?;
                    }
                    Ok(delta)
                })
            })
            .await
    }

    /// 按记账顺序返回**最近** `limit` 条流水。
    ///
    /// 必须有上限。对账环给每个有消费的用户每轮写一条，15 秒一轮就是一天约
    /// 5760 条；不限的话，几周之后一次余额请求要序列化十几万条、浏览器再把它们
    /// 全渲染出来 —— 页面先卡死，接口本身也成了自己打自己的那种慢查询。
    ///
    /// 余额不受影响：那是 `SUM` 出来的，永远算全部流水。这里限的只是「给人看的
    /// 那几条」。
    pub async fn entries(&self, user_id: &str, limit: i64) -> Result<Vec<Entry>, StoreError> {
        // 倒序取最近 N 条，再翻回正序 —— 正序 + LIMIT 会取到最老的那几条。
        let mut rows: Vec<(i64, i64, String, Option<String>)> = sqlx::query_as(
            "SELECT id, delta_micro_usd, source, note FROM ledger
             WHERE user_id = ?1 ORDER BY id DESC LIMIT ?2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(self.store.pool())
        .await?;
        rows.reverse();
        Ok(rows
            .into_iter()
            .map(|(id, delta_micro_usd, source, note)| Entry {
                id,
                delta_micro_usd,
                source,
                note,
            })
            .collect())
    }
}

/// 总额度的那条查询，可挂在任意 executor 上 —— 事务内外共用同一份语句。
pub(crate) async fn total_granted_on<'e, E>(exec: E, user_id: &str) -> Result<i64, StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let (sum,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(delta_micro_usd), 0) FROM ledger
         WHERE user_id = ?1 AND source != 'consumption'",
    )
    .bind(user_id)
    .fetch_one(exec)
    .await?;
    Ok(sum)
}

/// 在 sweeper 里再抄一份，记账语句从此有两个真相。
pub(crate) async fn insert_entry<'e, E>(
    exec: E,
    user_id: &str,
    delta: i64,
    source: Source,
    note: Option<&str>,
) -> Result<(), StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO ledger (user_id, delta_micro_usd, source, note, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(user_id)
    .bind(delta)
    .bind(source.as_str())
    .bind(note)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(exec)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 出账。走的是生产路径 `insert_entry` —— 保留一个生产上没人调的
    /// `Ledger::debit` 才是仓库规矩点名的那类隐形死码：pub 方法不会触发死码
    /// 分析，单测调它照样通过，唯一的症状是它在真实流程里从未被执行。
    async fn debit(l: &Ledger, user: &str, micro: u64) {
        let mut c = l.store.pool().acquire().await.unwrap();
        insert_entry(
            &mut *c,
            user,
            -i64::try_from(micro).unwrap(),
            Source::Consumption,
            None,
        )
        .await
        .unwrap();
    }

    async fn ledger() -> Ledger {
        let store = Store::open_memory().await.unwrap();
        // 流水有外键指向 users，先把用户建出来。
        for id in ["u1", "u2"] {
            store
                .insert_user(id, &format!("{id}@b.c"), "h", None)
                .await
                .unwrap();
        }
        Ledger::new(store)
    }

    #[tokio::test]
    async fn 余额是流水的和() {
        let l = ledger().await;
        l.credit("u1", 5_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        debit(&l, "u1", 1_500_000).await;
        assert_eq!(l.balance("u1").await.unwrap(), 3_500_000);
    }

    #[tokio::test]
    async fn 没有流水时余额为零_而不是报错() {
        let l = ledger().await;
        assert_eq!(l.balance("u1").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 并发的充值与扣减不丢钱() {
        let l = ledger().await;
        l.credit("u1", 1_000_000, Source::AdminGrant, None)
            .await
            .unwrap();

        // 50 笔入账与 50 笔出账同时打进去。任何一笔丢失或重复，最终余额都对
        // 不上 —— 这是「记账必须原子」的实测，而不是对实现的信任。
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..50 {
            let a = l.clone();
            set.spawn(async move { a.credit("u1", 1_000, Source::AdminGrant, None).await });
            let b = l.clone();
            set.spawn(async move {
                debit(&b, "u1", 400).await;
                Ok::<(), StoreError>(())
            });
        }
        while let Some(r) = set.join_next().await {
            r.unwrap().unwrap();
        }

        assert_eq!(
            l.balance("u1").await.unwrap(),
            1_000_000 + 50 * 1_000 - 50 * 400
        );
        assert_eq!(l.entries("u1", 1_000).await.unwrap().len(), 101);
    }

    #[tokio::test]
    async fn 扣到负数仍然入账_不能因余额不足而丢弃这笔消费() {
        let l = ledger().await;
        l.credit("u1", 1_000, Source::AdminGrant, None)
            .await
            .unwrap();
        // 消费已经发生了，钱已经花出去了。按直觉写成「余额不足则拒绝」，这笔
        // 就永远不入账 —— 又是一次静默白送。
        debit(&l, "u1", 2_500).await;
        assert_eq!(l.balance("u1").await.unwrap(), -1_500);
    }

    #[tokio::test]
    async fn 流水只追加_扣减不改写既有行() {
        let l = ledger().await;
        l.credit("u1", 1_000, Source::AdminGrant, None)
            .await
            .unwrap();
        let before = l.entries("u1", 1_000).await.unwrap();
        debit(&l, "u1", 400).await;
        let after = l.entries("u1", 1_000).await.unwrap();
        // 账要能重算，历史行不得被动过。
        assert_eq!(&after[..before.len()], &before[..]);
        assert_eq!(after.len(), before.len() + 1);
    }

    #[tokio::test]
    async fn 一个用户的流水不会算进另一个用户的余额() {
        let l = ledger().await;
        l.credit("u1", 5_000_000, Source::AdminGrant, None)
            .await
            .unwrap();
        debit(&l, "u2", 3_000_000).await;
        assert_eq!(l.balance("u1").await.unwrap(), 5_000_000);
        assert_eq!(l.balance("u2").await.unwrap(), -3_000_000);
    }
}
