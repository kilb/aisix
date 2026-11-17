//! 已存调用的请求级只读投影；组内身份与费用均不改写原始记录。

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::db::DbError;
use crate::error::{ApiError, Detail};

#[derive(Debug, Default, Deserialize)]
pub(crate) struct GroupQuery {
    pub group_requests: Option<bool>,
    pub include_attempts: Option<bool>,
    pub attempts_after: Option<i64>,
    pub attempt_limit: Option<i64>,
}

impl GroupQuery {
    pub(crate) fn enabled(&self) -> bool {
        self.group_requests == Some(true)
    }

    pub(crate) fn validate(&self, event_id: Option<&str>) -> Result<(), ApiError> {
        let refused = |path: &str, message: &str| {
            ApiError::validation(vec![Detail {
                path: path.to_owned(),
                message: message.to_owned(),
            }])
        };
        if self.include_attempts == Some(true)
            && (!self.enabled() || event_id.is_none_or(str::is_empty))
        {
            return Err(refused(
                "/include_attempts",
                "尝试详情需要请求分组和事件标识",
            ));
        }
        if (self.attempts_after.is_some() || self.attempt_limit.is_some())
            && self.include_attempts != Some(true)
        {
            return Err(refused("/include_attempts", "尝试分页需要启用尝试详情"));
        }
        if self.attempts_after.is_some_and(|seq| seq < 0) {
            return Err(refused("/attempts_after", "尝试游标不能为负数"));
        }
        Ok(())
    }
}

/// 调用方可复用请求编号；没有有效追踪身份的历史行只能独立展示。
fn trace_predicate(ue: &str) -> String {
    format!(
        "({ue}.request_id IS NOT NULL AND {ue}.request_id <> '' \
         AND jsonb_typeof({ue}.event->'trace_id') = 'string' \
         AND {ue}.event->>'trace_id' ~ '^[0-9a-f]{{32}}$' \
         AND {ue}.event->>'trace_id' <> '00000000000000000000000000000000')"
    )
}

/// 两个互斥分支使历史行走事件主键，追踪行复用既有请求索引。
/// CP记录的租户、成员和密钥共同限定授权范围，不相信事件的team_id。
fn members_sql() -> String {
    members_sql_for("pc_anchor", "ue_anchor")
}

fn members_sql_for(pc_anchor: &str, ue_anchor: &str) -> String {
    let anchor_traced = format!("({} IS TRUE)", trace_predicate(ue_anchor));
    let member_traced = trace_predicate("ue_member");
    format!(
        "SELECT {pc_anchor}.*, {ue_anchor}.event AS group_event \
         WHERE NOT {anchor_traced} \
         UNION ALL \
         SELECT pc_member.*, ue_member.event AS group_event \
         FROM usage_events ue_member \
         JOIN pricing_charges_cny pc_member ON pc_member.event_id = ue_member.event_id \
         WHERE {anchor_traced} AND {member_traced} \
           AND ue_member.request_id = {ue_anchor}.request_id \
           AND ue_member.dp_id = {ue_anchor}.dp_id \
           AND ue_member.event->>'trace_id' = {ue_anchor}.event->>'trace_id' \
           AND pc_member.tenant_id = {pc_anchor}.tenant_id \
           AND pc_member.api_key_id = {pc_anchor}.api_key_id \
           AND pc_member.user_id = {pc_anchor}.user_id"
    )
}

/// 先选实际终态，再选最高尝试编号；晚交付的旧失败不能覆盖最终成功。
const REPRESENTATIVE_ORDER: &str = "CASE \
    WHEN member.group_event->'is_final_attempt' = 'true'::jsonb \
      AND jsonb_typeof(member.group_event->'status_code') = 'number' \
      AND member.group_event->>'status_code' ~ '^2[0-9]{2}$' \
      AND (jsonb_typeof(member.group_event->'error_class') IS DISTINCT FROM 'string' \
           OR member.group_event->>'error_class' = '') THEN 2 \
    WHEN member.group_event->'is_final_attempt' = 'true'::jsonb THEN 1 \
    ELSE 0 END DESC, \
    CASE WHEN jsonb_typeof(member.group_event->'attempt_index') = 'number' \
              AND member.group_event->>'attempt_index' ~ '^[0-9]{1,10}$' \
         THEN (member.group_event->>'attempt_index')::bigint END DESC NULLS LAST, \
    member.occurred_at DESC, member.seq DESC";

/// 列表先筛选pc/ue候选，再确认候选是组的代表，避免先为全史选代表。
/// 布尔代表校验避免跨子查询等值的稀有匹配误估，保留按序有界候选。
/// 游标取代表行pc.seq，晚交付的旧失败不能改变已完成请求的分页位置。
/// 指定任意组内事件时保留该锚点，详情不会因它不是最新行而消失。
pub(crate) fn from_sql(anchored: bool, extra_joins: &str) -> String {
    if !anchored {
        let members = members_sql_for("pc", "ue");
        return format!(
            " FROM pricing_charges_cny pc \
             JOIN usage_events ue ON ue.event_id = pc.event_id \
             {extra_joins} \
             JOIN LATERAL (SELECT member.event_id FROM ({members}) member \
                 ORDER BY {REPRESENTATIVE_ORDER} LIMIT 1) representative \
                 ON CASE WHEN representative.event_id = pc.event_id THEN true ELSE false END \
             WHERE true"
        );
    }
    let members = members_sql();
    format!(
        " FROM pricing_charges_cny pc_anchor \
         LEFT JOIN usage_events ue_anchor ON ue_anchor.event_id = pc_anchor.event_id \
         JOIN LATERAL (SELECT member.event_id FROM ({members}) member \
             ORDER BY {REPRESENTATIVE_ORDER} LIMIT 1) representative ON true \
         JOIN pricing_charges_cny pc ON pc.event_id = representative.event_id \
         LEFT JOIN usage_events ue ON ue.event_id = pc.event_id \
         {extra_joins} WHERE true"
    )
}

/// 页内所有请求一次读出完整组的摘要；金额用numeric求和并以字符串返回。
pub(crate) async fn summaries(
    conn: &mut PgConnection,
    ids: &[String],
    tenant: bool,
) -> Result<HashMap<String, Value>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let members = members_sql();
    let sql = format!(
        "SELECT pc_anchor.event_id, jsonb_build_object(\
             'attempt_count', COUNT(*)::bigint, \
             'failover_count', COUNT(*) FILTER (WHERE member.group_event->>'attempt_kind' = 'fallback')::bigint, \
             'retry_count', COUNT(*) FILTER (WHERE member.group_event->>'attempt_kind' = 'retry')::bigint, \
             'complete', BOOL_OR(member.group_event->'is_final_attempt' = 'true'::jsonb) IS TRUE, \
             'charge_micro_cny', CASE WHEN BOOL_AND(member.status <> 'unpriced' OR adjusted.amount IS NOT NULL) \
                 THEN (SUM(COALESCE(member.charge_micro_cny, 0)::numeric + COALESCE(adjusted.amount, 0))::numeric)::text END, \
             'adjusted_micro_cny', (SUM(COALESCE(adjusted.amount, 0))::numeric)::text, \
             'cost_micro_cny', CASE WHEN BOOL_AND(member.cost_micro_cny IS NOT NULL \
                    OR member.status IN ('exempt', 'nothing_to_bill')) \
                 THEN (SUM(COALESCE(member.cost_micro_cny, 0))::numeric)::text END \
         ) AS summary \
         FROM pricing_charges_cny pc_anchor \
         LEFT JOIN usage_events ue_anchor ON ue_anchor.event_id = pc_anchor.event_id \
         CROSS JOIN LATERAL ({members}) member \
         LEFT JOIN LATERAL (SELECT SUM(pa.delta_micro_cny)::numeric AS amount \
             FROM pricing_adjustments_cny pa WHERE pa.event_id = member.event_id) adjusted ON true \
         WHERE pc_anchor.event_id = ANY($1) \
         GROUP BY pc_anchor.event_id"
    );
    let rows: Vec<(String, Value)> = sqlx::query_as(&sql).bind(ids).fetch_all(conn).await?;
    Ok(rows
        .into_iter()
        .map(|(id, mut summary)| {
            if tenant {
                if let Some(fields) = summary.as_object_mut() {
                    fields.remove("cost_micro_cny");
                }
            }
            (id, summary)
        })
        .collect())
}

/// 尝试详情按存储序号续读，不在组列表的页边界截断。
pub(crate) async fn attempt_page(
    conn: &mut PgConnection,
    event_id: &str,
    after: Option<i64>,
    limit: Option<i64>,
) -> Result<(Vec<String>, Option<i64>), DbError> {
    let limit = crate::usage::page_limit(Some(limit.unwrap_or(500)));
    let members = members_sql();
    let sql = format!(
        "SELECT member.seq, member.event_id \
         FROM pricing_charges_cny pc_anchor \
         LEFT JOIN usage_events ue_anchor ON ue_anchor.event_id = pc_anchor.event_id \
         CROSS JOIN LATERAL ({members}) member \
         WHERE pc_anchor.event_id = $1 AND member.seq > $2 \
         ORDER BY member.seq ASC LIMIT $3"
    );
    let mut rows: Vec<(i64, String)> = sqlx::query_as(&sql)
        .bind(event_id)
        .bind(after.unwrap_or(0))
        .bind(limit + 1)
        .fetch_all(conn)
        .await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next = more.then(|| rows.last().map(|row| row.0)).flatten();
    Ok((rows.into_iter().map(|(_, id)| id).collect(), next))
}

/// 管理员详情批量读取保存事件，沿用单事件接口的脱敏与字段契约。
pub(crate) async fn stored_events(
    conn: &mut PgConnection,
    ids: &[String],
) -> Result<HashMap<String, Value>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    type StoredEvent = (
        i64,
        String,
        DateTime<Utc>,
        DateTime<Utc>,
        Uuid,
        Uuid,
        Option<String>,
        Option<String>,
        Option<String>,
        Value,
    );
    let rows: Vec<StoredEvent> = sqlx::query_as(
        "SELECT seq, event_id, occurred_at, received_at, dp_id, batch_id, \
             team_id, api_key_id, request_id, event \
         FROM usage_events WHERE event_id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                seq,
                id,
                occurred_at,
                received_at,
                dp_id,
                batch_id,
                team_id,
                api_key_id,
                request_id,
                mut event,
            )| {
                crate::usage::admin_event_view(&mut event);
                let stored = json!({
                    "seq": seq, "event_id": id, "occurred_at": occurred_at,
                    "received_at": received_at, "dp_id": dp_id, "batch_id": batch_id,
                    "team_id": team_id, "api_key_id": api_key_id, "request_id": request_id,
                    "event": event,
                });
                (id, stored)
            },
        )
        .collect())
}
