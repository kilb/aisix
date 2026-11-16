//! 运营方本利汇算：同一快照中的日汇总、有限脏差额和不足整日的原记录。

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::usage::query_rejected;
use crate::AppState;

const SUMMARY_SQL: &str = include_str!("profit_summary.sql");
const MAX_PENDING: usize = 4096;

#[derive(Deserialize)]
pub struct ProfitQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ProfitTotals {
    charged_micro_cny: String,
    adjusted_micro_cny: String,
    known_cost_micro_cny: String,
    charged_with_cost_micro_cny: String,
    rows_with_cost: String,
    rows_without_cost: String,
    unpriced_rows: String,
    rows: String,
}

#[derive(Serialize)]
pub struct ProfitResponse {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    totals: ProfitTotals,
}

fn range_error() -> ApiError {
    ApiError::validation(vec![Detail {
        path: "/from".into(),
        message: "请同时给出合法的开始和结束时间，且结束时间须晚于开始时间".into(),
    }])
}

fn catching_up() -> ApiError {
    let mut error = ApiError::unavailable();
    error.code = "profit_summary_catching_up";
    error.message = "本利汇算正在同步历史记录，请稍后刷新".into();
    error
}

fn day_floor(value: DateTime<Utc>) -> Result<DateTime<Utc>, ApiError> {
    DateTime::from_timestamp(value.timestamp().div_euclid(86_400) * 86_400, 0)
        .ok_or_else(range_error)
}

/// 无边界表示全部人民币会计历史；显式边界为任意跨度的 [from, to)。
pub async fn get(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<ProfitQuery>, QueryRejection>,
) -> Result<Json<ProfitResponse>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let (whole_from, whole_to) = match (q.from, q.to) {
        (None, None) => (None, None),
        (Some(from), Some(to)) if from < to => {
            let floor = day_floor(from)?;
            let start = if from == floor {
                floor
            } else {
                floor
                    .checked_add_signed(TimeDelta::days(1))
                    .ok_or_else(range_error)?
            };
            (Some(start), Some(day_floor(to)?))
        }
        _ => return Err(range_error()),
    };
    let mut tx = state.db.pool().begin().await.map_err(DbError::from)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let ready: bool =
        sqlx::query_scalar("SELECT complete FROM billing_profit_backfill WHERE version = 1")
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if !ready {
        return Err(catching_up());
    }
    let pending: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM billing_profit_pending ORDER BY event_id LIMIT 4097",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if pending.len() > MAX_PENDING {
        return Err(catching_up());
    }
    let totals = sqlx::query_as(SUMMARY_SQL)
        .bind(q.from)
        .bind(q.to)
        .bind(whole_from)
        .bind(whole_to)
        .bind(&pending)
        .persistent(false)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(Json(ProfitResponse {
        from: q.from,
        to: q.to,
        totals,
    }))
}
