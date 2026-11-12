//! 手工直连测试的持久样本；只存有界脱敏结果及资源版本，不参与正式用量和计费。

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::{Db, DbError};
use crate::error::{ApiError, Detail};
use crate::model_probes::ProbeResult;
use crate::AppState;

/// 只记录资源身份和版本，不记录资源文档或凭据指纹。
pub(crate) struct Snapshot {
    pub model_id: Uuid,
    pub model_version: i64,
    pub model_name: String,
    pub provider_key_id: Option<Uuid>,
    pub provider_key_version: Option<i64>,
    pub account_name: Option<String>,
}

pub(crate) async fn begin(
    db: &Db,
    target: Snapshot,
    actor_id: Uuid,
    capability: bool,
    deadline: tokio::time::Instant,
) -> Result<i64, ApiError> {
    let insert = sqlx::query_scalar(
        "INSERT INTO model_probe_history \
         (model_id, model_version, model_name, provider_key_id, provider_key_version, \
          account_name, actor_id, mode) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id",
    )
    .bind(target.model_id)
    .bind(target.model_version)
    .bind(target.model_name)
    .bind(target.provider_key_id)
    .bind(target.provider_key_version)
    .bind(target.account_name)
    .bind(actor_id)
    .bind(if capability { "capability" } else { "baseline" })
    .fetch_one(db.pool());
    within_budget(deadline, insert, "测试记录保存超时，本次未调用上游").await
}

pub(crate) async fn save(
    db: &Db,
    id: i64,
    result: &ProbeResult,
    completed: bool,
    deadline: tokio::time::Instant,
) -> Result<(), ApiError> {
    // 入口只接受固定 DTO，不能把原始请求、答文或账号文档写进 JSON。
    let result =
        serde_json::to_value(result).map_err(|_| ApiError::internal("测试结果无法保存"))?;
    let update = sqlx::query(
        "UPDATE model_probe_history SET result = $2, \
         finished_at = CASE WHEN $3 THEN now() ELSE NULL END WHERE id = $1",
    )
    .bind(id)
    .bind(result)
    .bind(completed)
    .execute(db.pool());
    within_budget(deadline, update, "测试记录保存超时，请从历史查看已保存结果").await?;
    Ok(())
}

/// 所有保存共用调用方的绝对截止；零预算不 poll SQL，也不占连接等待。
pub(crate) async fn within_budget<T>(
    deadline: tokio::time::Instant,
    operation: impl std::future::Future<Output = Result<T, sqlx::Error>>,
    message: &'static str,
) -> Result<T, ApiError> {
    let remaining = deadline
        .checked_duration_since(tokio::time::Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| ApiError::internal(message))?;
    tokio::time::timeout(remaining.min(std::time::Duration::from_secs(5)), operation)
        .await
        .map_err(|_| ApiError::internal(message))?
        .map_err(DbError::from)
        .map_err(Into::into)
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Baseline,
    Capability,
}
impl Mode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Capability => "capability",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Available,
    Failed,
    Timeout,
    Unsupported,
}
impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    model_id: Option<Uuid>,
    mode: Option<Mode>,
    status: Option<Status>,
    before: Option<i64>,
    limit: Option<i64>,
    #[serde(default)]
    latest: bool,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

type Row = (
    i64,
    Uuid,
    Option<Uuid>,
    i64,
    Option<i64>,
    String,
    Option<String>,
    Uuid,
    Option<String>,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<Value>,
    bool,
);

/// 最新记录先按模式取每个目标的一轮，再筛基础状态；旧成功不能冒充最新结果。
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(|_| invalid_query())?;
    if q.before.is_some_and(|id| id < 1) || matches!((q.from, q.to), (Some(a), Some(b)) if a >= b) {
        return Err(invalid_query());
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let mut rows: Vec<Row> = sqlx::query_as(
        "WITH selected AS ( \
           SELECT h.*, row_number() OVER (PARTITION BY model_id ORDER BY id DESC) AS position \
           FROM model_probe_history h \
           WHERE ($1::uuid IS NULL OR model_id = $1) AND ($2::text IS NULL OR mode = $2) \
             AND ($3::timestamptz IS NULL OR started_at >= $3) \
             AND ($4::timestamptz IS NULL OR started_at < $4) \
         ) SELECT h.id, h.model_id, h.provider_key_id, h.model_version, h.provider_key_version, \
           h.model_name, h.account_name, h.actor_id, o.email, h.mode, h.started_at, h.finished_at, h.result, \
           (m.id IS NULL OR m.deleted OR m.version <> h.model_version \
             OR (h.provider_key_id IS NOT NULL AND (p.id IS NULL OR p.deleted \
               OR p.version IS DISTINCT FROM h.provider_key_version))) AS stale \
         FROM selected h LEFT JOIN operators o ON o.id = h.actor_id \
           LEFT JOIN gw_resources m ON m.kind = 'models' AND m.id = h.model_id \
           LEFT JOIN gw_resources p ON p.kind = 'provider_keys' AND p.id = h.provider_key_id \
         WHERE (NOT $5 OR h.position = 1) AND ($6::bigint IS NULL OR h.id < $6) \
           AND ($7::text IS NULL OR h.result->>'status' = $7) \
         ORDER BY h.id DESC LIMIT $8",
    )
    .bind(q.model_id).bind(q.mode.as_ref().map(Mode::as_str)).bind(q.from).bind(q.to)
    .bind(q.latest).bind(q.before).bind(q.status.as_ref().map(Status::as_str)).bind(limit + 1)
    .fetch_all(state.db.pool()).await.map_err(DbError::from)?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_before = if more {
        rows.last().map(|row| row.0.to_string())
    } else {
        None
    };
    let now = Utc::now();
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(
                id,
                model_id,
                provider_key_id,
                model_version,
                provider_key_version,
                model_name,
                account_name,
                actor_id,
                actor_email,
                mode,
                started_at,
                finished_at,
                result,
                stale,
            )| {
                let state = if finished_at.is_some() {
                    "completed"
                } else if now.signed_duration_since(started_at).num_seconds() > 60 {
                    "interrupted"
                } else {
                    "running"
                };
                json!({
                    "id": id.to_string(), "model_id": model_id, "provider_key_id": provider_key_id,
                    "model_version": model_version.to_string(),
                    "provider_key_version": provider_key_version.map(|v| v.to_string()),
                    "target": {"name": model_name, "account": account_name},
                    "actor": {"id": actor_id, "email": actor_email}, "mode": mode,
                    "started_at": started_at, "finished_at": finished_at,
                    "state": state, "stale": stale, "result": result,
                })
            },
        )
        .collect();
    Ok(Json(json!({"items": items, "next_before": next_before})))
}

fn invalid_query() -> ApiError {
    // 这个新查询端点约定非法查询返回 400；保留共用验证错误的结构化详情。
    ApiError {
        status: axum::http::StatusCode::BAD_REQUEST,
        ..ApiError::validation(vec![Detail {
            path: "query".to_string(),
            message: "测试历史查询参数不合法".to_string(),
        }])
    }
}
