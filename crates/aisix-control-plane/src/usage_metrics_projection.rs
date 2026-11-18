//! 事件导出的窄统计输入；权限、计费与模型映射仍在原统计快照中读取。

use crate::db::{Db, DbError};

// 参数处先截断已就绪行；未知版本整行回退，不把合法 NULL 当成未投影。
pub(crate) const METRIC_INPUT: &str = r#"
 LEFT JOIN usage_metric_inputs ready
   ON ready.event_id = ue.event_id AND ready.projection_version = 1
 LEFT JOIN LATERAL (
   SELECT * FROM usage_metric_input_v1(CASE WHEN ready.event_id IS NULL THEN ue.event END)
   WHERE ready.event_id IS NULL OFFSET 0
 ) fallback ON true
"#;

// 请求统计已有核定计费归属；只有投影缺失时才访问宽事件表。
pub(crate) const CHARGE_METRIC_INPUT: &str = r#"
 LEFT JOIN usage_metric_inputs ready
   ON ready.event_id = pc.event_id AND ready.projection_version = 1
 LEFT JOIN LATERAL (
   SELECT projected.* FROM usage_events ue
   CROSS JOIN LATERAL usage_metric_input_v1(ue.event) projected
   WHERE ready.event_id IS NULL AND ue.event_id = pc.event_id OFFSET 0
 ) fallback ON true
"#;

// 纯 CASE 投影不设优化屏障，使范围读取和精确聚合仍可选择并行计划。
pub(crate) const METRIC_FIELDS: &str = r#"
 CROSS JOIN LATERAL (SELECT
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_prompt_tokens ELSE fallback.u32_prompt_tokens END AS u32_prompt_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_completion_tokens ELSE fallback.u32_completion_tokens END AS u32_completion_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_reasoning_tokens ELSE fallback.u32_reasoning_tokens END AS u32_reasoning_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_total_tokens ELSE fallback.u32_total_tokens END AS u32_total_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_cached_prompt_tokens ELSE fallback.u32_cached_prompt_tokens END AS u32_cached_prompt_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_cache_read_tokens ELSE fallback.u32_cache_read_tokens END AS u32_cache_read_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_cache_creation_tokens ELSE fallback.u32_cache_creation_tokens END AS u32_cache_creation_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_cache_write_tokens ELSE fallback.u32_cache_write_tokens END AS u32_cache_write_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_status_code ELSE fallback.u32_status_code END AS u32_status_code,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_first_token_latency_ms ELSE fallback.u32_first_token_latency_ms END AS u32_first_token_latency_ms,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_downstream_latency_ms ELSE fallback.u32_downstream_latency_ms END AS u32_downstream_latency_ms,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_upstream_ttft_ms ELSE fallback.u32_upstream_ttft_ms END AS u32_upstream_ttft_ms,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.u32_upstream_latency_ms ELSE fallback.u32_upstream_latency_ms END AS u32_upstream_latency_ms,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.tpot ELSE fallback.tpot END AS tpot,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_error_class ELSE fallback.text_error_class END AS text_error_class,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_cache_status ELSE fallback.text_cache_status END AS text_cache_status,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.json_is_final_attempt ELSE fallback.json_is_final_attempt END AS json_is_final_attempt,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.json_cache_write_tokens ELSE fallback.json_cache_write_tokens END AS json_cache_write_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.has_reasoning_tokens ELSE fallback.has_reasoning_tokens END AS has_reasoning_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.has_cached_prompt_tokens ELSE fallback.has_cached_prompt_tokens END AS has_cached_prompt_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.has_cache_read_tokens ELSE fallback.has_cache_read_tokens END AS has_cache_read_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.has_cache_creation_tokens ELSE fallback.has_cache_creation_tokens END AS has_cache_creation_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.has_cache_write_tokens ELSE fallback.has_cache_write_tokens END AS has_cache_write_tokens,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_model_id ELSE fallback.text_model_id END AS text_model_id,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_provider_key_id ELSE fallback.text_provider_key_id END AS text_provider_key_id,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_inference_stage ELSE fallback.text_inference_stage END AS text_inference_stage,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_native_terminal_status ELSE fallback.text_native_terminal_status END AS text_native_terminal_status,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_upstream_dispatched ELSE fallback.text_upstream_dispatched END AS text_upstream_dispatched,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_cache_usage_reported ELSE fallback.text_cache_usage_reported END AS text_cache_usage_reported,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.json_upstream_dispatched ELSE fallback.json_upstream_dispatched END AS json_upstream_dispatched,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.json_cache_usage_reported ELSE fallback.json_cache_usage_reported END AS json_cache_usage_reported,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.has_cache_usage_reported ELSE fallback.has_cache_usage_reported END AS has_cache_usage_reported,
  CASE WHEN ready.event_id IS NOT NULL THEN ready.text_service_tier ELSE fallback.text_service_tier END AS text_service_tier
 ) ep
"#;

const BACKFILL_BATCH: i64 = 256;
const BACKFILL_SQL: &str = include_str!("usage_metric_input_backfill.sql");

/// 一次短事务的进度；暂停调用或进程重启均从已提交的连续前缀恢复。
#[derive(Debug, Default)]
pub struct Backfilled {
    pub examined: i64,
    pub written: i64,
    pub deferred: i64,
    pub complete: bool,
}

/// 只补安装触发器前的有界历史；跳锁的洞阻止游标越过，完成后不再扫描历史。
pub async fn backfill_once(db: &Db) -> Result<Backfilled, DbError> {
    let mut tx = db.pool().begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '50ms'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '2s'")
        .execute(&mut *tx)
        .await?;
    let state: Option<(i32, i64, Option<i64>, bool)> = sqlx::query_as(
        "SELECT version, after_seq, high_seq, complete FROM usage_metric_input_backfill \
         WHERE version = (SELECT version FROM usage_metric_input_backfill WHERE version IN (1, 2) \
             ORDER BY complete, version LIMIT 1) FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some((version, after, high, complete)) = state else {
        tx.commit().await?;
        return Ok(Backfilled::default());
    };
    if complete {
        tx.commit().await?;
        return Ok(Backfilled {
            complete: true,
            ..Backfilled::default()
        });
    }
    let high = match high {
        Some(high) => high,
        None => {
            sqlx::query_scalar("SELECT COALESCE(max(seq), 0)::bigint FROM usage_events")
                .fetch_one(&mut *tx)
                .await?
        }
    };
    let (next, examined, locked, written): (i64, i64, i64, i64) = sqlx::query_as(BACKFILL_SQL)
        .bind(after)
        .bind(high)
        .bind(BACKFILL_BATCH)
        .fetch_one(&mut *tx)
        .await?;
    let complete = examined < BACKFILL_BATCH && locked == examined;
    let others_complete: bool = sqlx::query_scalar(
        "UPDATE usage_metric_input_backfill SET after_seq = $1, high_seq = $2, complete = $3 \
         WHERE version = $4 RETURNING NOT EXISTS (SELECT 1 FROM usage_metric_input_backfill \
             WHERE version IN (1, 2) AND version <> $4 AND NOT complete)",
    )
    .bind(if complete { high } else { next })
    .bind(high)
    .bind(complete)
    .bind(version)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Backfilled {
        examined,
        written,
        deferred: examined - locked,
        complete: complete && others_complete,
    })
}
