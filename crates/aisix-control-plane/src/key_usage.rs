//! 当前可见密钥的批量净费用；不读取凭据，也不为已计价事件展开大 JSON。

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::auth::{MemberAuth, KEY_HOLDERS};
use crate::billing::calls::ADJUSTED;
use crate::billing::range::{checked_range, Group};
use crate::db::DbError;
use crate::error::ApiError;
use crate::usage::query_rejected;
use crate::AppState;

#[derive(Deserialize)]
pub struct UsageQuery {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

#[derive(sqlx::FromRow, Serialize)]
struct KeyUsage {
    api_key_id: Uuid,
    charged_micro_cny: String,
    unpriced_requests: i64,
    /// pending 包含 unpriced；当前两者相等，不能相加。
    pending_requests: i64,
}

/// 同一条语句共享快照；当前密钥集合补零，不把历史或外租户的编号带回列表。
async fn summarize(
    state: &AppState,
    tenant: Uuid,
    member: Option<Uuid>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<KeyUsage>, ApiError> {
    // charged 或有修正的行必定可见；其他行的净费用为零，因此金额不需要读取事件。
    // 只有未修正的 unpriced 行需要判定最终尝试；CASE 避免为已计价行解包事件。
    let sql = format!(
        "WITH visible_keys AS MATERIALIZED (\
           SELECT id, created_at FROM tenant_keys \
           WHERE tenant_id = $1 AND revoked_at IS NULL \
             AND ($4::uuid IS NULL OR member_id = $4)\
         ), scoped AS MATERIALIZED (\
           SELECT pc.api_key_id, pc.event_id, pc.status, pc.charge_micro_cny, \
             {ADJUSTED} AS adjusted \
           FROM pricing_charges_cny pc JOIN visible_keys k ON pc.api_key_id = k.id::text \
           WHERE pc.tenant_id = $1::text AND pc.occurred_at >= $2 AND pc.occurred_at < $3 \
             AND ($4::uuid IS NULL OR pc.user_id = $4::text)\
         ), totals AS (\
           SELECT api_key_id, \
             (COALESCE(sum(CASE WHEN status = 'charged' THEN charge_micro_cny ELSE 0 END)::numeric, 0) \
               + COALESCE(sum(adjusted)::numeric, 0))::text AS charged_micro_cny, \
             (count(*) FILTER (WHERE CASE WHEN status = 'unpriced' AND adjusted IS NULL \
               THEN (SELECT ue.event->'is_final_attempt' = 'true'::jsonb \
                     FROM usage_events ue WHERE ue.event_id = scoped.event_id) \
               ELSE FALSE END))::bigint AS pending_requests \
           FROM scoped GROUP BY api_key_id\
         ) \
         SELECT k.id AS api_key_id, COALESCE(t.charged_micro_cny, '0') AS charged_micro_cny, \
           COALESCE(t.pending_requests, 0) AS unpriced_requests, \
           COALESCE(t.pending_requests, 0) AS pending_requests \
         FROM visible_keys k LEFT JOIN totals t ON t.api_key_id = k.id::text \
         ORDER BY k.created_at, k.id"
    );
    sqlx::query_as::<_, KeyUsage>(&sql)
        .bind(tenant)
        .bind(from)
        .bind(to)
        .bind(member)
        .fetch_all(state.db.pool())
        .await
        .map_err(DbError::from)
        .map_err(ApiError::from)
}

/// `GET /api/usage/keys`；与密钥列表相同，待审核和停用的组织仍可读取。
pub async fn get(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<UsageQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(KEY_HOLDERS)?;
    let Query(q) = query.map_err(query_rejected)?;
    let (from, to) = checked_range(q.from, q.to, Some(Group::Day))?;
    let own_member = (!auth.can_manage_all_keys()).then_some(auth.id);
    let items = summarize(&state, auth.tenant_id, own_member, from, to).await?;
    Ok(Json(
        serde_json::json!({ "from": from, "to": to, "items": items }),
    ))
}
