//! 本利汇算的有界后台投影；摄入只入队，不等待日汇总热行。

use std::time::Duration;

use sqlx::Connection;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::db::{Db, DbError};

const PROJECT_SQL: &str = include_str!("profit_project.sql");
const BACKFILL_BATCH: i64 = 512;

/// 无可用连接或另一个实例正在投影时直接跳过，不扩张控制面连接池。
pub async fn project_once(db: &Db) -> Result<usize, DbError> {
    let Some(mut conn) = db.pool().try_acquire() else {
        return Ok(0);
    };
    let work = async {
        let mut tx = conn.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '25ms'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout = '150ms'")
            .execute(&mut *tx)
            .await?;
        // 源表正被维护时立即让路，不占共享池连接等待表锁。
        sqlx::query(
            "LOCK TABLE pricing_charges_cny, pricing_adjustments_cny, pricing_cost_backfills \
             IN ACCESS SHARE MODE NOWAIT",
        )
        .execute(&mut *tx)
        .await?;
        let state: Option<(i64, Option<i64>, bool)> = sqlx::query_as(
            "SELECT after_seq, high_seq, complete FROM billing_profit_backfill \
             WHERE version = 1 FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some((after, high, complete)) = state else {
            tx.commit().await?;
            return Ok(0);
        };
        if !complete {
            let high = match high {
                Some(high) => high,
                None => {
                    sqlx::query_scalar(
                        "SELECT COALESCE(max(seq), 0)::bigint FROM pricing_charges_cny",
                    )
                    .fetch_one(&mut *tx)
                    .await?
                }
            };
            let (next, examined): (i64, i64) = sqlx::query_as(
                "WITH candidates AS MATERIALIZED ( \
                   SELECT seq, event_id FROM pricing_charges_cny \
                   WHERE seq > $1 AND seq <= $2 ORDER BY seq LIMIT $3 \
                 ), queued AS ( \
                   INSERT INTO billing_profit_pending(event_id) SELECT event_id FROM candidates \
                   ORDER BY event_id ON CONFLICT(event_id) DO NOTHING RETURNING event_id \
                 ) SELECT COALESCE(max(seq), $1)::bigint, count(*)::bigint FROM candidates",
            )
            .bind(after)
            .bind(high)
            .bind(BACKFILL_BATCH)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE billing_profit_backfill SET after_seq = $1, high_seq = $2, complete = $3 \
                 WHERE version = 1",
            )
            .bind(if examined < BACKFILL_BATCH {
                high
            } else {
                next
            })
            .bind(high)
            .bind(examined < BACKFILL_BATCH)
            .execute(&mut *tx)
            .await?;
        }
        // 队列锁与金额读取分用两条语句，后一快照覆盖锁前刚提交的调整。
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT event_id FROM billing_profit_pending ORDER BY event_id \
                 LIMIT 256 FOR UPDATE SKIP LOCKED",
        )
        .fetch_all(&mut *tx)
        .await?;
        let written: i64 = sqlx::query_scalar(PROJECT_SQL)
            .persistent(false)
            .bind(&ids)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok::<usize, DbError>(written as usize)
    };
    // 整笔事务也有期限；每条 SQL 的限时不能约束整个后台批次。
    match tokio::time::timeout(Duration::from_millis(250), work).await {
        Ok(Err(DbError::LockTimeout)) => Ok(0),
        Ok(result) => result,
        Err(_) => Ok(0),
    }
}

/// 与普通 housekeeping 独立运行；繁忙时让出共享连接，停止后回滚未提交批次。
pub fn spawn(db: Db, mut stop: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_millis(100));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = crate::stopped(&mut stop) => return,
                _ = every.tick() => {
                    match project_once(&db).await {
                        Ok(256) => every.reset_immediately(),
                        Ok(_) => {},
                        Err(error) => {
                            tracing::warn!(error = %error, "billing profit projection will retry");
                            every.reset_after(Duration::from_secs(10));
                        }
                    }
                }
            }
        }
    })
}
