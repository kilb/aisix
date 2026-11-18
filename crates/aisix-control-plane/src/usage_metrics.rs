//! 全量 API 指标：最终事件计请求，可见的计费尝试计 token 和费用；不按客户端 request_id 去重。
//! 所有分组、百分位和金额求和均由同一数据库快照完成，Rust 只补齐空时间桶。
//! 模型维度使用调用者请求的入口：direct / embedding 为自身，routing / semantic /
//! ensemble 为父入口，wildcard 为解析到的配置行；不按尝试派发的上游目标分组。
//! 独立的运维直连监测读取实际尝试目标，并保留未知派发与结果。

use std::collections::{BTreeMap, HashMap};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::billing::calls::{ModelRef, VISIBLE};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::usage::query_rejected;
use crate::usage_metrics_projection::{CHARGE_METRIC_INPUT, METRIC_FIELDS, METRIC_INPUT};
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
        // 北京日界是 UTC 前一天16点；小时和分钟边界在东八区恰好不变。
        let offset = 8 * 3600;
        DateTime::from_timestamp(
            (at.timestamp() + offset).div_euclid(self.seconds()) * self.seconds() - offset,
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
    view: Option<String>,
    tenant_id: Option<Uuid>,
    member_id: Option<Uuid>,
    model_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
}

#[derive(Default, Deserialize, Serialize)]
pub struct LatencyMetrics {
    pub sample_count: i64,
    /// 请求指标以最终事件为基数；直连监测以实际派发尝试为基数，缺失单独计数。
    pub missing_count: i64,
    pub avg: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
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
pub struct MetricsPeriod<T = Metrics> {
    pub period_start: DateTime<Utc>,
    #[serde(flatten)]
    pub metrics: T,
}

#[derive(Serialize)]
pub struct MetricsModel {
    pub model: ModelRef,
    #[serde(flatten)]
    pub metrics: Metrics,
}

#[derive(Serialize)]
pub struct MetricsResponse<T = Metrics, P = T> {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub bucket: Bucket,
    pub totals: T,
    pub series: Vec<MetricsPeriod<P>>,
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

/// 无效历史数值在版本化投影中保留 NULL；摄入与缺失行回退共用同一函数。
fn counter(field: &str) -> String {
    format!("ep.u32_{field}")
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
      'p95', percentile_cont(0.95) WITHIN GROUP (ORDER BY {field}) FILTER (WHERE {eligible}), \
      'p99', percentile_cont(0.99) WITHIN GROUP (ORDER BY {field}) FILTER (WHERE {eligible}))",
        count(&sample),
        count(&format!("{eligible} AND {field} IS NULL"))
    )
}

fn token_sum(field: &str) -> String {
    format!("COALESCE(sum(CASE WHEN visible AND known_usage THEN {field} ELSE 0 END)::bigint, 0)")
}

fn request_and_charge_fields() -> String {
    let requests = count("final");
    let succeeded = count("final AND status_code BETWEEN 200 AND 299 AND NOT has_error");
    let failed = count("final AND (has_error OR status_code NOT BETWEEN 200 AND 299)");
    format!(
        "'requests', {requests}, 'succeeded_requests', {succeeded}, 'failed_requests', {failed}, \
         'unknown_outcome_requests', {}, 'unknown_final_attempts', {}, \
         'known_outcome_requests', {succeeded} + {failed}, \
         'success_rate', {succeeded}::double precision / NULLIF({succeeded} + {failed}, 0), \
         'unpriced_attempts', {}, 'billing_attempts', {}, 'charged_micro_cny', \
         (COALESCE(sum(CASE WHEN charge_status = 'charged' THEN charge_micro_cny ELSE 0 END)::numeric, 0) \
         + COALESCE(sum(adjusted)::numeric, 0))::text",
        count("final AND status_code IS NULL AND NOT has_error"),
        count("unknown_final"),
        count("visible AND charge_status = 'unpriced' AND adjusted IS NULL"),
        count("visible")
    )
}

fn aggregates() -> String {
    let hit = count("final AND cache_status = 'hit'");
    let miss = count("final AND cache_status = 'miss'");
    let input = token_sum(
        "prompt_tokens + COALESCE(cache_read_tokens, 0) + COALESCE(cache_creation_tokens, 0)",
    );
    let cached = token_sum("COALESCE(cached_prompt_tokens, 0) + COALESCE(cache_read_tokens, 0)");
    let mut fields = vec![
        request_and_charge_fields(),
        "'adjusted_micro_cny', COALESCE(sum(adjusted)::numeric, 0)::text".into(),
        format!(
            "'first_token_latency_ms', {}, 'response_latency_ms', {}, 'attempt_duration_ms', {}",
            latency("ttft", "final"),
            latency("response_latency", "final"),
            latency("attempt_duration", "visible")
        ),
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

async fn metric_model_names(
    conn: &mut PgConnection,
    tenant: Option<Uuid>,
) -> Result<HashMap<String, String>, ApiError> {
    let allowed = if let Some(tenant) = tenant {
        Some(crate::tenants::scope_model_ids(conn, tenant).await?)
    } else {
        None
    };
    let model_rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, COALESCE(doc->>'display_name', '') FROM gw_resources \
         WHERE kind = 'models' AND NOT deleted AND ($1::uuid[] IS NULL OR id = ANY($1))",
    )
    .bind(allowed)
    .fetch_all(conn)
    .await
    .map_err(DbError::from)?;
    Ok(model_rows
        .into_iter()
        .map(|(id, name)| (id.to_string(), name))
        .collect())
}

async fn summarize(
    state: &AppState,
    q: MetricsQuery,
    tenant: Option<Uuid>,
    own_member: Option<Uuid>,
) -> Result<Response, ApiError> {
    let (from, to, bucket) = checked_window(&q)?;
    let mut tx = state.db.pool().begin().await.map_err(DbError::from)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if let Some(tenant) = tenant {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1)")
                .bind(tenant)
                .fetch_one(&mut *tx)
                .await
                .map_err(DbError::from)?;
        if !exists {
            return Err(ApiError::not_found());
        }
    }
    let overview = q.view.as_deref() == Some("overview");
    let names = if overview {
        HashMap::new()
    } else {
        metric_model_names(&mut tx, tenant).await?
    };
    let ids: Vec<String> = names.keys().cloned().collect();

    let mut sql = QueryBuilder::<Postgres>::new(
        "WITH charge_scope AS NOT MATERIALIZED (SELECT pc.* FROM pricing_charges_cny pc \
         WHERE pc.occurred_at >= ",
    );
    sql.push_bind(from)
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
    // 修正按当前范围一次聚合；单消费者明细不物化，避免宽表扫描及排序屏障。
    sql.push(
        "), adjusted AS (SELECT pa.event_id, sum(pa.delta_micro_cny)::numeric AS adjusted \
         FROM pricing_adjustments_cny pa JOIN charge_scope scope ON scope.event_id = pa.event_id \
         GROUP BY pa.event_id), raw AS NOT MATERIALIZED (SELECT ",
    );
    let visible = VISIBLE.replace("ue.event->'is_final_attempt'", "ep.json_is_final_attempt");
    sql.push("date_trunc(")
        .push_bind(bucket.unit())
        .push(", pc.occurred_at, 'Asia/Shanghai') AS period_start, ");
    sql.push("CASE WHEN pc.requested_model_id = ANY(")
        .push_bind(ids)
        .push(") THEN pc.requested_model_id ELSE '' END AS model_id, ");
    sql.push(format!("ep.json_is_final_attempt = 'true'::jsonb AS final, \
        (ep.json_is_final_attempt IS NULL OR jsonb_typeof(ep.json_is_final_attempt) <> 'boolean') AS unknown_final, \
        {visible} AS visible, pc.status AS charge_status, pc.charge_micro_cny, \
        adjusted.adjusted, \
        COALESCE(ep.text_error_class, '') <> '' AS has_error, ep.text_cache_status AS cache_status, "));
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
            "ep.json_cache_write_tokens = 'null'::jsonb OR "
        } else {
            ""
        };
        format!(
            "(NOT (ep.has_{field}) OR {nullable}({}) IS NOT NULL)",
            counter(field)
        )
    })
    .collect::<Vec<_>>()
    .join(" AND ");
    columns.push(format!("({valid_optional}) AS valid_optional_usage"));
    sql.push(columns.join(", "))
        .push(" FROM charge_scope pc LEFT JOIN adjusted ON adjusted.event_id = pc.event_id")
        .push(CHARGE_METRIC_INPUT)
        .push(METRIC_FIELDS);
    sql.push(
        "), scoped AS (SELECT *, \
        CASE WHEN raw_status BETWEEN 100 AND 599 THEN raw_status END AS status_code, \
        prompt_tokens IS NOT NULL AND completion_tokens IS NOT NULL AND valid_optional_usage \
          AND COALESCE(cached_prompt_tokens, 0) + COALESCE(cache_write_tokens, 0) <= prompt_tokens \
          AS known_usage FROM raw)",
    );
    if overview {
        return summarize_overview(sql, tx, from, to, bucket).await;
    }
    sql.push(" SELECT \
        CASE WHEN GROUPING(period_start) = 0 THEN 1 WHEN GROUPING(model_id) = 0 THEN 2 ELSE 0 END AS grouping, \
        period_start, model_id, ").push(aggregates()).push(" AS metrics FROM scoped \
        GROUP BY GROUPING SETS ((), (period_start), (model_id)) ORDER BY grouping, period_start, model_id");
    let rows: Vec<AggregateRow> = sql
        .build_query_as()
        // 窗口选择性由本次参数决定，不让持久语句转用误估宽窗口的通用计划。
        .persistent(false)
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
    Ok(Json(MetricsResponse {
        from,
        to,
        bucket,
        totals,
        series: fill_periods(from, to, bucket, periods, Metrics::default)?,
        models,
    })
    .into_response())
}

fn fill_periods<T>(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bucket: Bucket,
    mut periods: BTreeMap<DateTime<Utc>, T>,
    empty: impl Fn() -> T,
) -> Result<Vec<MetricsPeriod<T>>, ApiError> {
    let mut series = Vec::new();
    let mut period = bucket.floor(from)?;
    while period < to {
        series.push(MetricsPeriod {
            period_start: period,
            metrics: periods.remove(&period).unwrap_or_else(&empty),
        });
        period = period
            .checked_add_signed(TimeDelta::seconds(bucket.seconds()))
            .ok_or_else(|| refused("/to", "时间不合法"))?;
    }
    Ok(series)
}

#[derive(Deserialize, Serialize)]
struct OverviewTotals {
    requests: i64,
    succeeded_requests: i64,
    failed_requests: i64,
    known_outcome_requests: i64,
    unknown_outcome_requests: i64,
    unknown_final_attempts: i64,
    success_rate: Option<f64>,
    billing_attempts: i64,
    unpriced_attempts: i64,
    charged_micro_cny: String,
    first_token_latency_ms: OverviewMedian,
}

#[derive(Deserialize, Serialize)]
struct OverviewMedian {
    p50: Option<f64>,
    sample_count: i64,
    missing_count: i64,
}

#[derive(Default, Deserialize, Serialize)]
struct OverviewPeriod {
    requests: i64,
    first_token_latency_ms: OverviewMean,
}

#[derive(Default, Deserialize, Serialize)]
struct OverviewMean {
    avg: Option<f64>,
}

/// 首页只需要总范围的精确 P50；分桶仅统计请求数与首 token 的实测平均值。
async fn summarize_overview(
    mut sql: QueryBuilder<'_, Postgres>,
    mut tx: Transaction<'_, Postgres>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bucket: Bucket,
) -> Result<Response, ApiError> {
    sql.push(", overview AS MATERIALIZED (SELECT period_start, final, unknown_final, status_code, \
        has_error, charge_status, adjusted, charge_micro_cny, visible, ttft FROM scoped) \
        SELECT NULL::timestamptz AS period_start, jsonb_build_object(")
        .push(request_and_charge_fields())
        .push(", 'first_token_latency_ms', jsonb_build_object( \
          'p50', percentile_cont(0.5) WITHIN GROUP (ORDER BY ttft) FILTER (WHERE final), \
          'sample_count', count(*) FILTER (WHERE final AND ttft IS NOT NULL), \
          'missing_count', count(*) FILTER (WHERE final AND ttft IS NULL))) FROM overview \
        UNION ALL SELECT period_start, jsonb_build_object('requests', count(*) FILTER (WHERE final), \
          'first_token_latency_ms', jsonb_build_object('avg', avg(ttft) FILTER (WHERE final))) \
        FROM overview GROUP BY period_start");
    let rows: Vec<(Option<DateTime<Utc>>, Value)> = sql
        .build_query_as()
        .persistent(false)
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    let mut totals = None;
    let mut periods = BTreeMap::new();
    for (period, metrics) in rows {
        if let Some(period) = period {
            periods.insert(
                period,
                serde_json::from_value(metrics).map_err(ApiError::internal)?,
            );
        } else {
            totals = Some(
                serde_json::from_value::<OverviewTotals>(metrics).map_err(ApiError::internal)?,
            );
        }
    }
    Ok(Json(MetricsResponse {
        from,
        to,
        bucket,
        totals: totals.ok_or_else(|| ApiError::internal("概览汇总缺失"))?,
        series: fill_periods(from, to, bucket, periods, OverviewPeriod::default)?,
        models: vec![],
    })
    .into_response())
}

/// 活跃租户所有角色可读；普通成员的过滤条件只会在自身历史归属上继续收窄。
pub async fn tenant(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<MetricsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
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
) -> Result<Response, ApiError> {
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
) -> Result<Response, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let tenant = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let Query(q) = query.map_err(query_rejected)?;
    if q.tenant_id.is_some_and(|id| id != tenant) {
        return Err(refused("/tenant_id", "租户范围与路径不一致"));
    }
    summarize(&state, q, Some(tenant), None).await
}

/// 直连模型监测读取每次尝试，与客户端请求和收费可见性口径独立。
#[derive(Default, Deserialize)]
pub struct DirectModelsQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    bucket: Option<Bucket>,
    tenant_id: Option<Uuid>,
    model_id: Option<Uuid>,
    provider_key_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    service_tier: Option<String>,
    outcome: Option<DirectOutcome>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectOutcome {
    Succeeded,
    Failed,
    Unknown,
}

impl DirectOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
pub struct DirectMetrics {
    pub attributed_attempts: i64,
    pub attempts: i64,
    pub excluded_attempts: i64,
    pub unmeasured_attempts: i64,
    pub succeeded_attempts: i64,
    pub failed_attempts: i64,
    pub unknown_outcome_attempts: i64,
    pub success_rate: Option<f64>,
    pub failure_rate: Option<f64>,
    pub upstream_ttft_ms: LatencyMetrics,
    pub first_token_latency_ms: LatencyMetrics,
    pub attempt_duration_ms: LatencyMetrics,
    pub tokens: DirectTokenMetrics,
    /// 有效输出 Token 合计除以完整上游耗时合计；百分位来自单次输出 TPS。
    pub throughput_tps: LatencyMetrics,
}

#[derive(Default, Deserialize, Serialize)]
pub struct DirectTokenMetrics {
    #[serde(flatten)]
    pub totals: TokenMetrics,
    /// 缓存率只使用明确上报缓存计数的样本，缺失不当作未命中。
    pub cache_measured_attempts: i64,
    pub cache_missing_attempts: i64,
    pub cache_measured_input_tokens: i64,
    pub cache_measured_hit_tokens: i64,
}

#[derive(Serialize)]
pub struct DirectMetricsPeriod {
    pub period_start: DateTime<Utc>,
    #[serde(flatten)]
    pub metrics: DirectMetrics,
}

#[derive(Serialize)]
pub struct DirectMetricsModel {
    pub model: ModelRef,
    pub provider_key: ModelRef,
    #[serde(flatten)]
    pub metrics: DirectMetrics,
}

#[derive(Serialize)]
pub struct DirectMetricsResponse {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub bucket: Bucket,
    pub totals: DirectMetrics,
    pub series: Vec<DirectMetricsPeriod>,
    pub models: Vec<DirectMetricsModel>,
}

/// TPOT 保留原 numeric 边界与正数语义，未知值仍为 NULL。
fn direct_tpot() -> &'static str {
    "ep.tpot"
}

fn direct_token_sum(field: &str) -> String {
    format!(
        "COALESCE(sum(CASE WHEN dispatched AND known_usage THEN {field} ELSE 0 END)::bigint, 0)"
    )
}

fn direct_cache_sum(field: &str) -> String {
    format!(
        "COALESCE(sum(CASE WHEN dispatched AND known_usage AND cache_reported \
         THEN {field} ELSE 0 END)::bigint, 0)"
    )
}

fn direct_aggregates() -> String {
    let attempts = count("dispatched");
    let succeeded = count("dispatched AND outcome = 'succeeded'");
    let failed = count("dispatched AND outcome = 'failed'");
    let input = direct_token_sum(
        "prompt_tokens + COALESCE(cache_read_tokens, 0) + COALESCE(cache_creation_tokens, 0)",
    );
    let cached =
        direct_token_sum("COALESCE(cached_prompt_tokens, 0) + COALESCE(cache_read_tokens, 0)");
    let cache_input = direct_cache_sum(
        "prompt_tokens + COALESCE(cache_read_tokens, 0) + COALESCE(cache_creation_tokens, 0)",
    );
    let cache_hits =
        direct_cache_sum("COALESCE(cached_prompt_tokens, 0) + COALESCE(cache_read_tokens, 0)");
    let mut tokens: Vec<String> = [
        "prompt_tokens",
        "completion_tokens",
        "reasoning_tokens",
        "cached_prompt_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
    ]
    .iter()
    .map(|field| format!("'{field}', {}", direct_token_sum(field)))
    .collect();
    tokens.push(format!(
        "'input_tokens', {input}, 'cache_hit_tokens', {cached}, \
         'cache_hit_rate', {cache_hits}::double precision / NULLIF({cache_input}, 0), \
         'known_usage_attempts', {}, 'unknown_usage_attempts', {}, \
         'cache_measured_attempts', {}, 'cache_missing_attempts', {}, \
         'cache_measured_input_tokens', {cache_input}, 'cache_measured_hit_tokens', {cache_hits}",
        count("dispatched AND known_usage"),
        count("dispatched AND NOT known_usage"),
        count("dispatched AND known_usage AND cache_reported"),
        count("dispatched AND (NOT known_usage OR NOT cache_reported)")
    ));
    let throughput = "jsonb_build_object(\
        'sample_count', count(*) FILTER (WHERE tps IS NOT NULL), \
        'missing_count', count(*) FILTER (WHERE dispatched AND tps IS NULL), \
        'avg', 1000.0 * sum(CASE WHEN tps IS NOT NULL THEN output_tokens END)::numeric \
          / NULLIF(sum(CASE WHEN tps IS NOT NULL THEN attempt_duration END)::numeric, 0), \
        'p50', percentile_cont(0.5) WITHIN GROUP (ORDER BY tps), \
        'p95', percentile_cont(0.95) WITHIN GROUP (ORDER BY tps), \
        'p99', percentile_cont(0.99) WITHIN GROUP (ORDER BY tps))";
    format!(
        "jsonb_build_object(\
         'attributed_attempts', count(*), 'attempts', {attempts}, \
         'excluded_attempts', {}, 'unmeasured_attempts', {}, \
         'succeeded_attempts', {succeeded}, 'failed_attempts', {failed}, \
         'unknown_outcome_attempts', {}, \
         'success_rate', {succeeded}::double precision / NULLIF({succeeded} + {failed}, 0), \
         'failure_rate', {failed}::double precision / NULLIF({succeeded} + {failed}, 0), \
         'upstream_ttft_ms', {}, 'first_token_latency_ms', {}, 'attempt_duration_ms', {}, \
         'tokens', jsonb_build_object({}), 'throughput_tps', {throughput})",
        count("excluded"),
        count("NOT dispatched AND NOT excluded"),
        count("dispatched AND outcome = 'unknown'"),
        latency("upstream_ttft", "dispatched"),
        latency("ttft", "dispatched"),
        latency("attempt_duration", "dispatched"),
        tokens.join(", ")
    )
}

#[derive(sqlx::FromRow)]
struct DirectAggregateRow {
    grouping: i32,
    period_start: Option<DateTime<Utc>>,
    model_id: Option<String>,
    provider_key_id: Option<String>,
    metrics: sqlx::types::Json<DirectMetrics>,
}

async fn summarize_direct_models(
    state: &AppState,
    q: DirectModelsQuery,
) -> Result<Json<DirectMetricsResponse>, ApiError> {
    let (from, to, bucket) = checked_window(&MetricsQuery {
        from: q.from,
        to: q.to,
        bucket: q.bucket,
        ..MetricsQuery::default()
    })?;
    if q.service_tier.as_ref().is_some_and(|s| s.len() > 128) {
        return Err(refused("/service_tier", "服务档位过长"));
    }
    let mut tx = state.db.pool().begin().await.map_err(DbError::from)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let configured: Vec<(Uuid, serde_json::Value, bool)> =
        sqlx::query_as("SELECT id, doc, deleted FROM gw_resources WHERE kind = 'models'")
            .fetch_all(&mut *tx)
            .await
            .map_err(DbError::from)?;
    let account_rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, COALESCE(doc->>'display_name', '') FROM gw_resources \
         WHERE kind = 'provider_keys' AND NOT deleted",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::from)?;
    let accounts: HashMap<String, String> = account_rows
        .into_iter()
        .map(|(id, name)| (id.to_string(), name))
        .collect();
    let mut names = HashMap::new();
    let mut known_ids = Vec::new();
    let mut direct_ids = Vec::new();
    let mut idle = BTreeMap::new();
    for (id, doc, deleted) in configured {
        let model: aisix_core::Model = serde_json::from_value(doc)
            .map_err(|_| ApiError::internal("存储模型格式无效，无法读取监测配置"))?;
        let id = id.to_string();
        known_ids.push(id.clone());
        if model.is_routing() || model.is_ensemble() || model.is_semantic() {
            continue;
        }
        direct_ids.push(id.clone());
        if deleted {
            continue;
        }
        names.insert(id.clone(), model.display_name);
        let account = model.provider_key_id.unwrap_or_default();
        if q.model_id.is_none_or(|filter| filter.to_string() == id)
            && q.provider_key_id
                .is_none_or(|filter| filter.to_string() == account)
        {
            idle.insert((id, account), DirectMetrics::default());
        }
    }

    let mut sql = QueryBuilder::<Postgres>::new("WITH raw AS MATERIALIZED (SELECT date_trunc(");
    sql.push_bind(bucket.unit()).push(
        ", ue.occurred_at, 'Asia/Shanghai') AS period_start, \
          ep.text_model_id AS model_id, \
          COALESCE(ep.text_provider_key_id, '') AS provider_key_id, \
          COALESCE(ep.text_error_class, '') AS error_class, \
          ep.text_cache_status AS cache_status, \
          ep.text_inference_stage AS inference_stage, \
          ep.text_native_terminal_status AS native_terminal_status, \
          CASE WHEN jsonb_typeof(ep.json_upstream_dispatched) = 'boolean' \
            THEN (ep.text_upstream_dispatched)::boolean END AS explicit_dispatch, \
          COALESCE(ready.usage_estimated, ue.event->'usage_estimated' = 'true'::jsonb, FALSE) AS usage_estimated, ",
    );
    let mut columns: Vec<String> = [
        "prompt_tokens",
        "completion_tokens",
        "reasoning_tokens",
        "total_tokens",
        "cached_prompt_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
        "cache_write_tokens",
    ]
    .iter()
    .map(|field| format!("{} AS {field}", counter(field)))
    .collect();
    columns.extend([
        format!("{} AS raw_status", counter("status_code")),
        format!("{} AS ttft", counter("first_token_latency_ms")),
        format!(
            "NULLIF({}, 0) AS upstream_ttft",
            counter("upstream_ttft_ms")
        ),
        format!(
            "NULLIF({}, 0) AS attempt_duration",
            counter("upstream_latency_ms")
        ),
        format!("{} AS tpot", direct_tpot()),
    ]);
    let optional_usage = [
        "reasoning_tokens",
        "cached_prompt_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
        "cache_write_tokens",
    ]
    .iter()
    .map(|field| {
        let nullable = if *field == "cache_write_tokens" {
            "ep.json_cache_write_tokens = 'null'::jsonb OR "
        } else {
            ""
        };
        format!(
            "(NOT (ep.has_{field}) OR {nullable}({}) IS NOT NULL)",
            counter(field)
        )
    })
    .collect::<Vec<_>>()
    .join(" AND ");
    columns.push(format!("({optional_usage}) AS valid_optional_usage"));
    columns.push("CASE WHEN jsonb_typeof(ep.json_cache_usage_reported) = 'boolean' THEN (ep.text_cache_usage_reported)::boolean WHEN ep.has_cache_usage_reported THEN FALSE END AS explicit_cache_reported".to_string());
    sql.push(columns.join(", "))
        .push(" FROM usage_events ue")
        .push(METRIC_INPUT)
        .push(METRIC_FIELDS)
        .push(" WHERE ue.occurred_at >= ")
        .push_bind(from)
        .push(" AND ue.occurred_at < ")
        .push_bind(to)
        .push(
            " AND ep.text_model_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' \
              AND (ep.text_model_id = ANY(",
        )
        .push_bind(direct_ids)
        .push(") OR (NOT (ep.text_model_id = ANY(")
        .push_bind(known_ids)
        .push(")) AND ep.text_provider_key_id ~* \
            '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'))");
    for (column, value) in [
        ("ep.text_model_id", q.model_id),
        ("ep.text_provider_key_id", q.provider_key_id),
        ("ue.api_key_id", q.api_key_id),
    ] {
        if let Some(value) = value {
            sql.push(format!(" AND {column} = "))
                .push_bind(value.to_string());
        }
    }
    if let Some(tenant) = q.tenant_id {
        // 每条摄入记录都有核定归属，无论最终与否、是否收费；不相信遥测自报的 team_id。
        sql.push(
            " AND EXISTS (SELECT 1 FROM pricing_charges_cny pc \
            WHERE pc.event_id = ue.event_id AND pc.tenant_id = ",
        )
        .push_bind(tenant.to_string())
        .push(")");
    }
    if let Some(tier) = q.service_tier {
        sql.push(" AND ep.text_service_tier = ").push_bind(tier);
    }
    sql.push(
        "), measured AS (SELECT *, \
         CASE WHEN raw_status BETWEEN 100 AND 599 THEN raw_status END AS status_code, \
         COALESCE(explicit_cache_reported, cached_prompt_tokens IS NOT NULL OR cache_read_tokens IS NOT NULL \
           OR cache_creation_tokens IS NOT NULL OR cache_write_tokens IS NOT NULL) AS cache_reported, \
         prompt_tokens IS NOT NULL AND completion_tokens IS NOT NULL AND valid_optional_usage \
           AND COALESCE(cached_prompt_tokens, 0) + COALESCE(cache_write_tokens, 0) <= prompt_tokens \
           AS known_usage, \
         CASE WHEN inference_stage IN ('accepted', 'management') THEN FALSE \
           WHEN cache_status = 'hit' THEN FALSE \
           WHEN explicit_dispatch IS NOT NULL THEN explicit_dispatch \
           WHEN attempt_duration IS NOT NULL OR upstream_ttft IS NOT NULL OR tpot IS NOT NULL \
             OR error_class IN ('timeout', 'upstream_status', 'upstream_decode', 'upstream_in_band', \
                'transport', 'stream_aborted') THEN TRUE ELSE FALSE END AS dispatched, \
         COALESCE(cache_status = 'hit', FALSE) AS cache_hit \
         FROM raw), classified AS (SELECT *, \
         NOT dispatched AND (COALESCE(inference_stage IN ('accepted', 'management'), FALSE) \
           OR COALESCE(explicit_dispatch = FALSE, FALSE) OR cache_hit OR error_class IN (\
           'invalid_request_error', 'config', 'invalid_config', 'invalid_credentials', \
           'invalid_api_key', 'authentication_error', 'permission_denied', 'model_not_found', \
           'resource_not_found', 'response_not_found', 'video_not_found', \
           'websocket_upgrade_required', 'provider_unavailable', 'all_candidates_unavailable', \
           'provider_groups_unavailable', \
           'semantic_decision_failed', 'content_filter', 'billing_error', 'rate_limit_exceeded', \
           'dependency_unavailable')) AS excluded, \
         CASE WHEN inference_stage = 'terminal' AND native_terminal_status = 'cancelled' THEN 'unknown' \
           WHEN inference_stage = 'terminal' AND native_terminal_status IN ('failed', 'expired') THEN 'failed' \
           WHEN error_class IN ('timeout', 'upstream_status', 'upstream_decode', \
              'upstream_in_band', 'transport', 'stream_aborted') THEN 'failed' \
           WHEN error_class <> '' THEN 'unknown' \
           WHEN status_code BETWEEN 200 AND 299 THEN 'succeeded' \
           WHEN status_code BETWEEN 300 AND 599 THEN 'failed' ELSE 'unknown' END AS outcome, \
         completion_tokens + CASE WHEN COALESCE(total_tokens, 0) > 0 AND COALESCE(reasoning_tokens, 0) > 0 \
           AND prompt_tokens + completion_tokens + reasoning_tokens = total_tokens \
           AND prompt_tokens + completion_tokens <> total_tokens \
           THEN reasoning_tokens ELSE 0 END AS output_tokens \
         FROM measured), scoped AS (SELECT *, \
         CASE WHEN dispatched AND outcome = 'succeeded' AND known_usage AND NOT usage_estimated \
           AND output_tokens > 0 AND attempt_duration > 0 \
           THEN 1000.0 * output_tokens / attempt_duration END AS tps \
         FROM classified",
    );
    if let Some(outcome) = q.outcome {
        sql.push(" WHERE dispatched AND outcome = ")
            .push_bind(outcome.as_str());
    }
    sql.push(
        ") SELECT CASE WHEN GROUPING(period_start) = 0 THEN 1 \
        WHEN GROUPING(model_id) = 0 THEN 2 ELSE 0 END AS grouping, \
        period_start, model_id, provider_key_id, ",
    )
    .push(direct_aggregates())
    .push(
        " AS metrics FROM scoped GROUP BY GROUPING SETS \
          ((), (period_start), (model_id, provider_key_id)) \
          ORDER BY grouping, period_start, model_id, provider_key_id",
    );
    let rows: Vec<DirectAggregateRow> = sql
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;

    let mut totals = DirectMetrics::default();
    let mut periods = BTreeMap::new();
    for row in rows {
        match row.grouping {
            0 => totals = row.metrics.0,
            1 => {
                if let Some(at) = row.period_start {
                    periods.insert(at, row.metrics.0);
                }
            }
            2 => {
                idle.insert(
                    (
                        row.model_id.unwrap_or_default(),
                        row.provider_key_id.unwrap_or_default(),
                    ),
                    row.metrics.0,
                );
            }
            _ => {}
        }
    }
    let mut models: Vec<DirectMetricsModel> = idle
        .into_iter()
        .map(|((id, account), metrics)| DirectMetricsModel {
            model: ModelRef {
                id: Uuid::parse_str(&id).ok(),
                name: names.get(&id).cloned().unwrap_or_default(),
            },
            provider_key: ModelRef {
                id: Uuid::parse_str(&account).ok(),
                name: accounts.get(&account).cloned().unwrap_or_default(),
            },
            metrics,
        })
        .collect();
    models.sort_by(|a, b| {
        a.model
            .name
            .cmp(&b.model.name)
            .then_with(|| a.model.id.cmp(&b.model.id))
            .then_with(|| a.provider_key.id.cmp(&b.provider_key.id))
    });
    let mut series = Vec::new();
    let mut period = bucket.floor(from)?;
    while period < to {
        series.push(DirectMetricsPeriod {
            period_start: period,
            metrics: periods.remove(&period).unwrap_or_default(),
        });
        period = period
            .checked_add_signed(TimeDelta::seconds(bucket.seconds()))
            .ok_or_else(|| refused("/to", "时间不合法"))?;
    }
    Ok(Json(DirectMetricsResponse {
        from,
        to,
        bucket,
        totals,
        series,
        models,
    }))
}

/// 上游标识只向运维账号开放，租户继续使用请求入口维度的指标。
pub async fn admin_direct_models(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<DirectModelsQuery>, QueryRejection>,
) -> Result<Json<DirectMetricsResponse>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    summarize_direct_models(&state, q).await
}

#[cfg(test)]
mod timezone_tests {
    use super::*;

    #[test]
    fn beijing_day_bucket_floor_and_window_count_use_sixteen_utc() {
        for boundary in [
            "2026-11-08T16:00:00Z",
            "2025-12-31T16:00:00Z",
            "2024-02-28T16:00:00Z",
        ] {
            let boundary = DateTime::parse_from_rfc3339(boundary)
                .expect("北京时间午夜")
                .with_timezone(&Utc);
            assert_eq!(Bucket::Day.floor(boundary).expect("日桶"), boundary);
            assert_eq!(
                Bucket::Day
                    .floor(boundary - TimeDelta::nanoseconds(1))
                    .expect("前一日桶"),
                boundary - TimeDelta::days(1)
            );
            for bucket in [Bucket::Minute, Bucket::Hour] {
                assert_eq!(bucket.floor(boundary).expect("细粒度桶"), boundary);
            }
            let q = MetricsQuery {
                from: Some(boundary),
                to: Some(boundary + TimeDelta::days(1)),
                bucket: Some(Bucket::Day),
                ..MetricsQuery::default()
            };
            let (from, to, bucket) = checked_window(&q).expect("整北京日范围");
            assert_eq!(bucket.floor(from).expect("起点"), from);
            assert_eq!(bucket.floor(to).expect("终点"), to);
        }
    }
}
