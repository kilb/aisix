//! 全量 API 指标：最终事件计请求，可见的计费尝试计 token 和费用；不按客户端 request_id 去重。
//! 所有分组、百分位和金额求和均由同一数据库快照完成，Rust 只补齐空时间桶。
//! 模型维度使用调用者请求的入口：direct / embedding 为自身，routing / semantic /
//! ensemble 为父入口，wildcard 为解析到的配置行；不按尝试派发的上游目标分组。

use std::collections::{BTreeMap, HashMap};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::billing::calls::{ModelRef, VISIBLE};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::usage::query_rejected;
use crate::AppState;

const MAX_BUCKETS: i64 = 1500;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Minute,
    Hour,
    Day,
}

impl Bucket {
    fn seconds(self) -> i64 {
        match self {
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86400,
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }

    fn floor(self, at: DateTime<Utc>) -> Result<DateTime<Utc>, ApiError> {
        DateTime::from_timestamp(
            at.timestamp().div_euclid(self.seconds()) * self.seconds(),
            0,
        )
        .ok_or_else(|| refused("/from", "时间不合法"))
    }
}

#[derive(Default, Deserialize)]
pub struct MetricsQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    bucket: Option<Bucket>,
    tenant_id: Option<Uuid>,
    member_id: Option<Uuid>,
    model_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
}

#[derive(Default, Deserialize, Serialize)]
pub struct LatencyMetrics {
    pub sample_count: i64,
    /// 首 token / 响应指标的基数是所有最终事件，包含非流式与旧事件；尝试耗时基数是可见尝试。
    pub missing_count: i64,
    pub avg: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
}

#[derive(Default, Deserialize, Serialize)]
pub struct TokenMetrics {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub reasoning_tokens: i64,
    pub cached_prompt_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub input_tokens: i64,
    pub cache_hit_tokens: i64,
    /// 已记录缓存 token 占输入比例：(subset cached + additive read) / (prompt + read + creation)。
    /// 旧遥测省略零计数，不能由此区分上游明确报告零与上游未报告缓存。
    pub cache_hit_rate: Option<f64>,
    pub known_usage_attempts: i64,
    pub unknown_usage_attempts: i64,
}

#[derive(Default, Deserialize, Serialize)]
pub struct ResponseCacheMetrics {
    pub hit_requests: i64,
    pub miss_requests: i64,
    pub bypass_requests: i64,
    pub disabled_requests: i64,
    pub unknown_requests: i64,
    /// 只有实际查缓存的 hit + miss 进入分母；没有合格请求时为 null。
    pub hit_rate: Option<f64>,
}

#[derive(Deserialize, Serialize)]
pub struct Metrics {
    pub requests: i64,
    pub succeeded_requests: i64,
    pub failed_requests: i64,
    pub known_outcome_requests: i64,
    pub unknown_outcome_requests: i64,
    pub unknown_final_attempts: i64,
    /// 只以 succeeded + failed 为分母，结果未知的请求由独立计数呈现。
    pub success_rate: Option<f64>,
    /// 包括免费、未定价的最终事件，以及已计费或重定价的非最终尝试。
    pub billing_attempts: i64,
    pub unpriced_attempts: i64,
    pub charged_micro_cny: String,
    pub adjusted_micro_cny: String,
    /// 只读真实首个生成内容交付时间；0 也是实测值，不从首帧或完整耗时推算。
    pub first_token_latency_ms: LatencyMetrics,
    /// 请求至首个可用数据，流式不是完整请求耗时。
    pub response_latency_ms: LatencyMetrics,
    /// 每次上游尝试的完整耗时，不拼成客户端完整请求耗时。
    pub attempt_duration_ms: LatencyMetrics,
    pub tokens: TokenMetrics,
    pub response_cache: ResponseCacheMetrics,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            requests: 0,
            succeeded_requests: 0,
            failed_requests: 0,
            known_outcome_requests: 0,
            unknown_outcome_requests: 0,
            unknown_final_attempts: 0,
            success_rate: None,
            billing_attempts: 0,
            unpriced_attempts: 0,
            charged_micro_cny: "0".into(),
            adjusted_micro_cny: "0".into(),
            first_token_latency_ms: LatencyMetrics::default(),
            response_latency_ms: LatencyMetrics::default(),
            attempt_duration_ms: LatencyMetrics::default(),
            tokens: TokenMetrics::default(),
            response_cache: ResponseCacheMetrics::default(),
        }
    }
}

#[derive(Serialize)]
pub struct MetricsPeriod {
    pub period_start: DateTime<Utc>,
    #[serde(flatten)]
    pub metrics: Metrics,
}

#[derive(Serialize)]
pub struct MetricsModel {
    pub model: ModelRef,
    #[serde(flatten)]
    pub metrics: Metrics,
}

#[derive(Serialize)]
pub struct MetricsResponse {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub bucket: Bucket,
    pub totals: Metrics,
    pub series: Vec<MetricsPeriod>,
    /// 全部分组，不截断；未知或租户无权限的模型合并为空模型。
    pub models: Vec<MetricsModel>,
}

fn refused(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.into(),
        message: message.into(),
    }])
}

fn checked_window(q: &MetricsQuery) -> Result<(DateTime<Utc>, DateTime<Utc>, Bucket), ApiError> {
    let to = q.to.unwrap_or_else(Utc::now);
    let from = match q.from {
        Some(from) => from,
        None => to
            .checked_sub_signed(TimeDelta::days(1))
            .ok_or_else(|| refused("/from", "时间不合法"))?,
    };
    if to <= from || to - from > TimeDelta::days(92) {
        return Err(refused("/to", "时间范围须大于零且不超过 92 天"));
    }
    let bucket = q.bucket.unwrap_or_else(|| {
        if to - from <= TimeDelta::hours(6) {
            Bucket::Minute
        } else if to - from <= TimeDelta::days(3) {
            Bucket::Hour
        } else {
            Bucket::Day
        }
    });
    let start = bucket.floor(from)?;
    let end = bucket.floor(to)?;
    let buckets = (end - start).num_seconds() / bucket.seconds() + i64::from(to != end);
    if buckets > MAX_BUCKETS {
        return Err(refused("/bucket", "时间桶过多，请选择更粗的粒度"));
    }
    Ok((from, to, bucket))
}

/// 新增遥测字段是可选的；错误类型或超出 u32 的值同缺失处理，不让历史 JSON 使汇总失败。
fn counter(field: &str) -> String {
    format!(
        "CASE WHEN jsonb_typeof(ue.event->'{field}') = 'number' \
        AND (ue.event->>'{field}') ~ '^[0-9]{{1,10}}$' \
        THEN CASE WHEN (ue.event->>'{field}')::numeric <= 4294967295 \
          THEN (ue.event->>'{field}')::bigint END END"
    )
}

fn count(condition: &str) -> String {
    format!("count(*) FILTER (WHERE {condition})")
}

fn latency(field: &str, eligible: &str) -> String {
    let sample = format!("{eligible} AND {field} IS NOT NULL");
    format!(
        "jsonb_build_object('sample_count', {}, 'missing_count', {}, \
      'avg', avg({field}) FILTER (WHERE {eligible}), \
      'p50', percentile_cont(0.5) WITHIN GROUP (ORDER BY {field}) FILTER (WHERE {eligible}), \
      'p95', percentile_cont(0.95) WITHIN GROUP (ORDER BY {field}) FILTER (WHERE {eligible}))",
        count(&sample),
        count(&format!("{eligible} AND {field} IS NULL"))
    )
}

fn token_sum(field: &str) -> String {
    format!("COALESCE(sum(CASE WHEN visible AND known_usage THEN {field} ELSE 0 END)::bigint, 0)")
}

fn aggregates() -> String {
    let requests = count("final");
    let succeeded = count("final AND status_code BETWEEN 200 AND 299 AND NOT has_error");
    let failed = count("final AND (has_error OR status_code NOT BETWEEN 200 AND 299)");
    let hit = count("final AND cache_status = 'hit'");
    let miss = count("final AND cache_status = 'miss'");
    let input = token_sum(
        "prompt_tokens + COALESCE(cache_read_tokens, 0) + COALESCE(cache_creation_tokens, 0)",
    );
    let cached = token_sum("COALESCE(cached_prompt_tokens, 0) + COALESCE(cache_read_tokens, 0)");
    let mut fields = vec![
        format!("'requests', {requests}, 'succeeded_requests', {succeeded}, 'failed_requests', {failed}"),
        format!("'unknown_outcome_requests', {}, 'unknown_final_attempts', {}", count("final AND status_code IS NULL AND NOT has_error"), count("unknown_final")),
        format!("'known_outcome_requests', {succeeded} + {failed}, 'success_rate', {succeeded}::double precision / NULLIF({succeeded} + {failed}, 0)"),
        format!("'unpriced_attempts', {}, 'adjusted_micro_cny', COALESCE(sum(adjusted)::numeric, 0)::text", count("visible AND charge_status = 'unpriced' AND adjusted IS NULL")),
        format!("'billing_attempts', {}, 'charged_micro_cny', \
          (COALESCE(sum(CASE WHEN charge_status = 'charged' THEN charge_micro_cny ELSE 0 END)::numeric, 0) \
           + COALESCE(sum(adjusted)::numeric, 0))::text", count("visible")),
        format!("'first_token_latency_ms', {}, 'response_latency_ms', {}, 'attempt_duration_ms', {}", latency("ttft", "final"), latency("response_latency", "final"), latency("attempt_duration", "visible")),
    ];
    let mut tokens: Vec<String> = [
        "prompt_tokens",
        "completion_tokens",
        "reasoning_tokens",
        "cached_prompt_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
    ]
    .iter()
    .map(|field| format!("'{field}', {}", token_sum(field)))
    .collect();
    tokens.push(format!(
        "'input_tokens', {input}, 'cache_hit_tokens', {cached}, \
        'cache_hit_rate', {cached}::double precision / NULLIF({input}, 0), \
        'known_usage_attempts', {}, 'unknown_usage_attempts', {}",
        count("visible AND known_usage"),
        count("visible AND NOT known_usage")
    ));
    fields.push(format!(
        "'tokens', jsonb_build_object({})",
        tokens.join(", ")
    ));
    fields.push(format!("'response_cache', jsonb_build_object('hit_requests', {hit}, 'miss_requests', {miss}, \
        'bypass_requests', {}, 'disabled_requests', {}, 'unknown_requests', {}, \
        'hit_rate', {hit}::double precision / NULLIF({hit} + {miss}, 0))",
        count("final AND cache_status = 'bypass'"), count("final AND cache_status = 'disabled'"),
        count("final AND (cache_status IS NULL OR cache_status NOT IN ('hit', 'miss', 'bypass', 'disabled'))")));
    format!("jsonb_build_object({})", fields.join(", "))
}

#[derive(sqlx::FromRow)]
struct AggregateRow {
    grouping: i32,
    period_start: Option<DateTime<Utc>>,
    model_id: Option<String>,
    metrics: sqlx::types::Json<Metrics>,
}

async fn summarize(
    state: &AppState,
    q: MetricsQuery,
    tenant: Option<Uuid>,
    own_member: Option<Uuid>,
) -> Result<Json<MetricsResponse>, ApiError> {
    let (from, to, bucket) = checked_window(&q)?;
    let mut tx = state.db.pool().begin().await.map_err(DbError::from)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let allowed = if let Some(tenant) = tenant {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1)")
                .bind(tenant)
                .fetch_one(&mut *tx)
                .await
                .map_err(DbError::from)?;
        if !exists {
            return Err(ApiError::not_found());
        }
        Some(crate::tenants::scope_model_ids(&mut tx, tenant).await?)
    } else {
        None
    };
    let model_rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, COALESCE(doc->>'display_name', '') FROM gw_resources \
         WHERE kind = 'models' AND NOT deleted AND ($1::uuid[] IS NULL OR id = ANY($1))",
    )
    .bind(allowed)
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::from)?;
    let names: HashMap<String, String> = model_rows
        .into_iter()
        .map(|(id, name)| (id.to_string(), name))
        .collect();
    let ids: Vec<String> = names.keys().cloned().collect();

    let mut sql = QueryBuilder::<Postgres>::new("WITH raw AS MATERIALIZED (SELECT ");
    sql.push("date_trunc(")
        .push_bind(bucket.unit())
        .push(", pc.occurred_at, 'UTC') AS period_start, ");
    sql.push("CASE WHEN pc.requested_model_id = ANY(")
        .push_bind(ids)
        .push(") THEN pc.requested_model_id ELSE '' END AS model_id, ");
    sql.push(format!("ue.event->'is_final_attempt' = 'true'::jsonb AS final, \
        (ue.event->'is_final_attempt' IS NULL OR jsonb_typeof(ue.event->'is_final_attempt') <> 'boolean') AS unknown_final, \
        {VISIBLE} AS visible, pc.status AS charge_status, pc.charge_micro_cny, \
        (SELECT sum(pa.delta_micro_cny)::numeric FROM pricing_adjustments_cny pa WHERE pa.event_id = pc.event_id) AS adjusted, \
        COALESCE(ue.event->>'error_class', '') <> '' AS has_error, ue.event->>'cache_status' AS cache_status, "));
    let mut columns: Vec<String> = [
        "prompt_tokens",
        "completion_tokens",
        "reasoning_tokens",
        "cached_prompt_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
    ]
    .iter()
    .map(|field| format!("{} AS {field}", counter(field)))
    .collect();
    columns.extend([
        format!("{} AS cache_write_tokens", counter("cache_write_tokens")),
        format!("{} AS raw_status", counter("status_code")),
        format!("{} AS ttft", counter("first_token_latency_ms")),
        format!(
            "NULLIF({}, 0) AS response_latency",
            counter("downstream_latency_ms")
        ),
        format!(
            "NULLIF({}, 0) AS attempt_duration",
            counter("upstream_latency_ms")
        ),
    ]);
    let valid_optional = [
        "reasoning_tokens",
        "cached_prompt_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
        "cache_write_tokens",
    ]
    .iter()
    .map(|field| {
        let nullable = if *field == "cache_write_tokens" {
            "ue.event->'cache_write_tokens' = 'null'::jsonb OR "
        } else {
            ""
        };
        format!(
            "(NOT (ue.event ? '{field}') OR {nullable}({}) IS NOT NULL)",
            counter(field)
        )
    })
    .collect::<Vec<_>>()
    .join(" AND ");
    columns.push(format!("({valid_optional}) AS valid_optional_usage"));
    sql.push(columns.join(", "))
        .push(
            " FROM pricing_charges_cny pc JOIN usage_events ue ON ue.event_id = pc.event_id \
        WHERE pc.occurred_at >= ",
        )
        .push_bind(from)
        .push(" AND pc.occurred_at < ")
        .push_bind(to);
    for (column, value) in [
        ("tenant_id", tenant),
        ("user_id", own_member),
        ("user_id", q.member_id),
        ("requested_model_id", q.model_id),
        ("api_key_id", q.api_key_id),
    ] {
        if let Some(value) = value {
            sql.push(format!(" AND pc.{column} = "))
                .push_bind(value)
                .push("::text");
        }
    }
    sql.push("), scoped AS (SELECT *, \
        CASE WHEN raw_status BETWEEN 100 AND 599 THEN raw_status END AS status_code, \
        prompt_tokens IS NOT NULL AND completion_tokens IS NOT NULL AND valid_optional_usage \
          AND COALESCE(cached_prompt_tokens, 0) + COALESCE(cache_write_tokens, 0) <= prompt_tokens \
          AS known_usage FROM raw) SELECT \
        CASE WHEN GROUPING(period_start) = 0 THEN 1 WHEN GROUPING(model_id) = 0 THEN 2 ELSE 0 END AS grouping, \
        period_start, model_id, ").push(aggregates()).push(" AS metrics FROM scoped \
        GROUP BY GROUPING SETS ((), (period_start), (model_id)) ORDER BY grouping, period_start, model_id");
    let rows: Vec<AggregateRow> = sql
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    let mut totals = Metrics::default();
    let mut periods = BTreeMap::new();
    let mut models = Vec::new();
    for row in rows {
        match row.grouping {
            0 => totals = row.metrics.0,
            1 => {
                if let Some(period) = row.period_start {
                    periods.insert(period, row.metrics.0);
                }
            }
            2 => {
                let raw = row.model_id.unwrap_or_default();
                models.push(MetricsModel {
                    model: ModelRef {
                        id: Uuid::parse_str(&raw).ok(),
                        name: names.get(&raw).cloned().unwrap_or_default(),
                    },
                    metrics: row.metrics.0,
                });
            }
            _ => {}
        }
    }
    let mut series = Vec::new();
    let mut period = bucket.floor(from)?;
    while period < to {
        series.push(MetricsPeriod {
            period_start: period,
            metrics: periods.remove(&period).unwrap_or_default(),
        });
        period = period
            .checked_add_signed(TimeDelta::seconds(bucket.seconds()))
            .ok_or_else(|| refused("/to", "时间不合法"))?;
    }
    Ok(Json(MetricsResponse {
        from,
        to,
        bucket,
        totals,
        series,
        models,
    }))
}

/// 活跃租户所有角色可读；普通成员的过滤条件只会在自身历史归属上继续收窄。
pub async fn tenant(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<MetricsQuery>, QueryRejection>,
) -> Result<Json<MetricsResponse>, ApiError> {
    auth.require_active()?;
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ])?;
    let Query(q) = query.map_err(query_rejected)?;
    if q.tenant_id.is_some() {
        return Err(refused("/tenant_id", "租户范围由当前会话决定"));
    }
    summarize(
        &state,
        q,
        Some(auth.tenant_id),
        (auth.role == MemberRole::Member).then_some(auth.id),
    )
    .await
}

pub async fn admin(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<MetricsQuery>, QueryRejection>,
) -> Result<Json<MetricsResponse>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let tenant = q.tenant_id;
    summarize(&state, q, tenant, None).await
}

pub async fn admin_tenant(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<MetricsQuery>, QueryRejection>,
) -> Result<Json<MetricsResponse>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let tenant = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let Query(q) = query.map_err(query_rejected)?;
    if q.tenant_id.is_some_and(|id| id != tenant) {
        return Err(refused("/tenant_id", "租户范围与路径不一致"));
    }
    summarize(&state, q, Some(tenant), None).await
}
