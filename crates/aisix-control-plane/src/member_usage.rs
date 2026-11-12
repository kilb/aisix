//! 管理员一次读取组织成员的用量；记账归属来自控制面的 charge，而非客户端事件。

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::auth::{MemberAuth, MemberRole};
use crate::billing::calls::{ADJUSTED, EFFECTIVE, EFF_LINES, VISIBLE};
use crate::billing::range::{checked_range, Group};
use crate::db::DbError;
use crate::error::ApiError;
use crate::usage::query_rejected;
use crate::AppState;

#[derive(Default, Deserialize)]
pub struct UsageQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct StoredUsage {
    member_id: String,
    requests: i64,
    charged_micro_cny: String,
    estimated_requests: i64,
    quantities: Value,
    pending_requests: i64,
    unpriced_requests: i64,
    unknown_quantity_requests: i64,
}

#[derive(Serialize)]
struct MemberUsage {
    member_id: String,
    requests: i64,
    charged_micro_cny: String,
    estimated_requests: i64,
    quantities: Value,
    /// pending 包含 unpriced，与现有调用记录的 pending 状态一致，不能相加。
    pending_requests: i64,
    unpriced_requests: i64,
    unknown_quantity_requests: i64,
}

/// 单条语句共享同一快照；数量单独聚合，避免一笔请求因多条计费维度而重复计数。
async fn summarize(
    state: &AppState,
    tenant: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<MemberUsage>, ApiError> {
    let sql = format!(
        "WITH scoped AS MATERIALIZED (\
           SELECT pc.user_id, pc.status, pc.charge_micro_cny, pc.estimated, \
                  {ADJUSTED} AS adjusted, eff.created_at AS corrected_at, {EFF_LINES} AS lines \
           FROM pricing_charges_cny pc JOIN usage_events ue ON ue.event_id = pc.event_id \
           {EFFECTIVE} \
           WHERE pc.tenant_id = $1::text AND pc.user_id <> '' \
             AND pc.occurred_at >= $2 AND pc.occurred_at < $3 AND {VISIBLE}\
         ), totals AS (\
           SELECT user_id, count(*)::bigint AS requests, \
             (COALESCE(SUM(CASE WHEN status = 'charged' THEN charge_micro_cny ELSE 0 END)::numeric, 0) \
               + COALESCE(sum(adjusted)::numeric, 0))::text AS charged_micro_cny, \
             (count(*) FILTER (WHERE estimated))::bigint AS estimated_requests, \
             (count(*) FILTER (WHERE status = 'unpriced' AND corrected_at IS NULL))::bigint AS unpriced_requests, \
             (count(*) FILTER (WHERE lines IS NULL))::bigint AS unknown_quantity_requests \
           FROM scoped GROUP BY user_id\
         ), dimensions AS (\
           SELECT user_id, l->>'dimension' AS dimension, sum((l->>'quantity')::bigint)::bigint AS quantity \
           FROM scoped CROSS JOIN LATERAL jsonb_array_elements(lines) l GROUP BY 1, 2\
         ), quantities AS (\
           SELECT user_id, jsonb_object_agg(dimension, quantity) AS quantities FROM dimensions GROUP BY user_id\
         ), member_ids AS (\
           SELECT id::text AS user_id FROM members WHERE tenant_id = $1::uuid AND removed_at IS NULL \
           UNION SELECT user_id FROM totals\
         ) \
         SELECT ids.user_id AS member_id, COALESCE(t.requests, 0) AS requests, \
           COALESCE(t.charged_micro_cny, '0') AS charged_micro_cny, \
           COALESCE(t.estimated_requests, 0) AS estimated_requests, \
           COALESCE(q.quantities, '{{}}'::jsonb) AS quantities, \
           COALESCE(t.unpriced_requests, 0) AS pending_requests, \
           COALESCE(t.unpriced_requests, 0) AS unpriced_requests, \
           COALESCE(t.unknown_quantity_requests, 0) AS unknown_quantity_requests \
         FROM member_ids ids LEFT JOIN totals t ON t.user_id = ids.user_id \
         LEFT JOIN quantities q ON q.user_id = ids.user_id ORDER BY ids.user_id"
    );
    let rows = sqlx::query_as::<_, StoredUsage>(&sql)
        .bind(tenant)
        .bind(from)
        .bind(to)
        .fetch_all(state.db.pool())
        .await
        .map_err(DbError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| MemberUsage {
            member_id: row.member_id,
            requests: row.requests,
            charged_micro_cny: row.charged_micro_cny,
            estimated_requests: row.estimated_requests,
            quantities: row.quantities,
            pending_requests: row.pending_requests,
            unpriced_requests: row.unpriced_requests,
            unknown_quantity_requests: row.unknown_quantity_requests,
        })
        .collect())
}

/// 统计月按北京时间月初开始，与预算实际采用的 UTC 结算周期无关。
fn month_start_beijing(now: DateTime<Utc>) -> DateTime<Utc> {
    let beijing = now + chrono::TimeDelta::hours(8);
    let month_start = beijing.date_naive() - chrono::TimeDelta::days(i64::from(beijing.day0()));
    month_start.and_time(chrono::NaiveTime::MIN).and_utc() - chrono::TimeDelta::hours(8)
}

/// `GET /api/members/usage`；默认北京时间月初到现在，范围最多 92 天，左闭右开。
pub async fn get(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<UsageQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin])?;
    let Query(q) = query.map_err(query_rejected)?;
    let now = Utc::now();
    let default_from = month_start_beijing(now);
    let (from, to) = checked_range(
        q.from.unwrap_or(default_from),
        q.to.unwrap_or(now),
        Some(Group::Day),
    )?;
    let items = summarize(&state, auth.tenant_id, from, to).await?;
    Ok(Json(
        serde_json::json!({"from": from, "to": to, "items": items}),
    ))
}

#[cfg(test)]
mod timezone_tests {
    use super::*;

    #[test]
    fn beijing_month_start_changes_at_sixteen_utc_across_year_and_leap_day() {
        for (now, expected) in [
            ("2025-12-31T15:59:59Z", "2025-11-30T16:00:00Z"),
            ("2025-12-31T16:00:00Z", "2025-12-31T16:00:00Z"),
            ("2024-02-28T16:00:00Z", "2024-01-31T16:00:00Z"),
            ("2024-02-29T16:00:00Z", "2024-02-29T16:00:00Z"),
        ] {
            let at = |value: &str| {
                DateTime::parse_from_rfc3339(value)
                    .expect("时间")
                    .with_timezone(&Utc)
            };
            assert_eq!(month_start_beijing(at(now)), at(expected), "{now}");
        }
    }
}
