//! 操作者查看真实计量及待对账状态；不返回产物 ID、提交正文或 IO 审计内容。

use aisix_core::native_metering::{ObservedMeter, ObservedUsage};
use aisix_core::native_objects::NativeObjectKind;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::error::{ApiError, Detail};
use crate::AppState;

const MAX_LIMIT: u32 = 100;
const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeteringQuery {
    state: Option<String>,
    reason: Option<String>,
    kind: Option<String>,
    limit: Option<u32>,
    after: Option<Uuid>,
}

fn invalid(path: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_owned(),
        message: "计量查询参数不合法".into(),
    }])
}

impl MeteringQuery {
    fn validate(&self) -> Result<u32, ApiError> {
        if self.state.as_deref().is_some_and(|state| {
            !matches!(
                state,
                "unknown" | "outbox_pending" | "awaiting_reconciliation" | "settled"
            )
        }) {
            return Err(invalid("/state"));
        }
        if self.reason.as_deref().is_some_and(|reason| {
            !matches!(
                reason,
                "missing_usage"
                    | "unknown_usage_shape"
                    | "unpriced_meter"
                    | "incomplete_observation"
                    | "uncertain_submission"
            )
        }) {
            return Err(invalid("/reason"));
        }
        if self
            .kind
            .as_ref()
            .is_some_and(|kind| serde_json::from_value::<NativeObjectKind>(json!(kind)).is_err())
        {
            return Err(invalid("/kind"));
        }
        let limit = self.limit.unwrap_or(50);
        if limit == 0 || limit > MAX_LIMIT {
            return Err(invalid("/limit"));
        }
        Ok(limit)
    }
}

#[derive(sqlx::FromRow)]
struct MeteringListRow {
    operation_id: Uuid,
    object_id: Option<Uuid>,
    target_kind: String,
    tenant_id: Uuid,
    api_key_id: Uuid,
    member_id: Option<Uuid>,
    kind: String,
    protocol: String,
    state: String,
    terminal_status: Option<String>,
    reason: Option<String>,
    observed_usage: Value,
    receipt_version: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn quantities(usage: Value) -> Result<Value, ApiError> {
    let observations: Vec<ObservedUsage> =
        serde_json::from_value(usage).map_err(super::store::unavailable)?;
    let mut quantities = Vec::with_capacity(observations.len());
    for observation in observations {
        let meter = match observation.meter {
            ObservedMeter::Tokens { usage } => json!({
                "kind":"tokens", "prompt_tokens":usage.prompt_tokens,
                "completion_tokens":usage.completion_tokens,
                "cached_prompt_tokens":usage.cached_prompt_tokens,
                "cache_usage_reported":usage.cache_usage_reported,
                "cache_write_tokens":usage.cache_write_tokens,
                "cache_write_30m_tokens":usage.cache_write_30m_tokens,
                "cache_write_12h_tokens":usage.cache_write_12h_tokens,
                "cache_creation_tokens":usage.cache_creation_tokens,
                "cache_read_tokens":usage.cache_read_tokens,
                "cache_creation_5m_tokens":usage.cache_creation_5m_tokens,
                "cache_creation_1h_tokens":usage.cache_creation_1h_tokens,
                "reasoning_tokens":usage.reasoning_tokens,"total_tokens":usage.total_tokens,
                "prompt_audio_tokens":usage.prompt_audio_tokens,
                "prompt_image_tokens":usage.prompt_image_tokens,
                "completion_audio_tokens":usage.completion_audio_tokens,
                "completion_image_tokens":usage.completion_image_tokens,
                "web_search_requests":usage.web_search_requests,
                "usage_estimated":usage.usage_estimated,
            }),
            meter => serde_json::to_value(meter).map_err(super::store::unavailable)?,
        };
        quantities.push(json!({
            "entry_id":observation.entry_id, "upstream_model":observation.upstream_model,
            "occurred_at":observation.occurred_at, "meter":meter,
        }));
    }
    Ok(Value::Array(quantities))
}

/// GET /admin/native-metering：按接受时间稳定分页，当前 receipt 与账务状态独立展示。
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<MeteringQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(query) = query.map_err(|_| invalid("query"))?;
    let limit = query.validate()?;
    let after_time: Option<DateTime<Utc>> = if let Some(after) = query.after {
        Some(sqlx::query_scalar(
            "SELECT created_at FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2",
        ).bind(state.env.env_id).bind(after).fetch_optional(state.native_objects.pool()).await
            .map_err(super::store::unavailable)?.ok_or_else(|| invalid("/after"))?)
    } else {
        None
    };
    let rows: Vec<MeteringListRow> = sqlx::query_as(
        "SELECT meter.operation_id,meter.object_id,meter.target_kind,meter.tenant_id,meter.api_key_id,\
         operation.member_id,operation.submission->>'kind' AS kind,operation.submission->>'protocol' AS protocol,\
         meter.state,meter.terminal_status,meter.reason,meter.observed_usage,meter.receipt_version,\
         meter.created_at,meter.updated_at FROM native_metering_records meter \
         JOIN native_operations operation USING(environment_id,operation_id) \
         WHERE meter.environment_id=$1 AND ($2::text IS NULL OR meter.state=$2) \
         AND ($3::text IS NULL OR meter.reason=$3) AND ($4::text IS NULL OR operation.submission->>'kind'=$4) \
         AND ($5::timestamptz IS NULL OR (meter.created_at,meter.operation_id)>($5,$6::uuid)) \
         ORDER BY meter.created_at,meter.operation_id LIMIT $7",
    ).bind(state.env.env_id).bind(query.state).bind(query.reason).bind(query.kind)
        .bind(after_time).bind(query.after).bind(i64::from(limit)+1)
        .fetch_all(state.native_objects.pool()).await.map_err(super::store::unavailable)?;
    let mut has_more = rows.len() > limit as usize;
    let mut items = Vec::new();
    let mut next_after = None;
    let mut bytes = 0usize;
    for row in rows.into_iter().take(limit as usize) {
        let item = json!({
            "operation_id":row.operation_id,"object_id":row.object_id,"target_kind":row.target_kind,
            "owner":{"tenant_id":row.tenant_id,"api_key_id":row.api_key_id,"member_id":row.member_id},
            "kind":row.kind,"protocol":row.protocol,"metering_state":row.state,
            "terminal_status":row.terminal_status,"reconciliation_reason":row.reason,
            "quantities":quantities(row.observed_usage)?, "receipt_version":row.receipt_version,
            "created_at":row.created_at,"updated_at":row.updated_at,
        });
        let size = serde_json::to_vec(&item)
            .map_err(super::store::unavailable)?
            .len();
        if size > MAX_REPLY_BYTES.saturating_sub(bytes) {
            if items.is_empty() {
                return Err(ApiError::unavailable());
            }
            has_more = true;
            break;
        }
        bytes += size;
        next_after = Some(row.operation_id);
        items.push(item);
    }
    Ok(Json(
        json!({"items":items,"has_more":has_more,"next_after":has_more.then_some(next_after).flatten()}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_filters_are_closed_and_bounded() {
        for (field, value) in [
            ("state", json!("no_charge")),
            ("reason", json!("other")),
            ("kind", json!("unknown")),
            ("limit", json!(0)),
            ("limit", json!(101)),
        ] {
            let mut query = json!({});
            query[field] = value;
            let query: MeteringQuery = serde_json::from_value(query).unwrap();
            assert!(query.validate().is_err(), "{field}");
        }
        for state in [
            "unknown",
            "outbox_pending",
            "awaiting_reconciliation",
            "settled",
        ] {
            let query: MeteringQuery =
                serde_json::from_value(json!({"state":state,"limit":100})).unwrap();
            assert_eq!(query.validate().unwrap(), 100);
        }
        assert!(serde_json::from_value::<MeteringQuery>(json!({"body":"secret"})).is_err());
    }

    #[test]
    fn operator_quantities_omit_error_and_audit_content() {
        let reported = json!([{
            "entry_id":"entry", "upstream_model":"provider-model", "occurred_at":"2026-11-14T00:00:00Z",
            "meter":{"kind":"tokens","usage":{
                "entry_id":"entry","occurred_at":"2026-11-14T00:00:00Z","status_code":200,
                "prompt_tokens":4,"completion_tokens":2,"total_tokens":6,
                "cache_write_tokens":4,"cache_write_30m_tokens":0,"cache_write_12h_tokens":4,
                "error_message":"request-body-secret","error_class":"private-provider-error"
            }}
        }]);
        let visible = quantities(reported).unwrap();
        assert_eq!(visible.pointer("/0/meter/prompt_tokens"), Some(&json!(4)));
        assert_eq!(visible.pointer("/0/meter/total_tokens"), Some(&json!(6)));
        assert_eq!(
            visible.pointer("/0/meter/cache_write_30m_tokens"),
            Some(&json!(0))
        );
        assert_eq!(
            visible.pointer("/0/meter/cache_write_12h_tokens"),
            Some(&json!(4))
        );
        assert!(!visible.to_string().contains("request-body-secret"));
        assert!(!visible.to_string().contains("private-provider-error"));
        assert!(visible.pointer("/0/meter/io_audit").is_none());
    }
}
