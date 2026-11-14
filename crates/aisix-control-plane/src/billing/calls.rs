//! A tenant's call log and usage summary: the priced usage rows of its own
//! keys, shown without anything of the platform's channel.
//!
//! - `GET /api/usage/calls`: the call log, newest first; an owner, admin or
//!   billing member sees every call of the tenant, a member only the calls
//!   of the keys bound to them (the charge's `user_id`, which is the key's
//!   member as the control plane recorded it).
//! - `GET /api/usage/summary`: requests, charges and billed quantities by
//!   Beijing calendar day or month and model, over the same rows.
//! - `GET /admin/tenants/{id}/usage/summary`: the same summary for operators.
//!
//! Rows are found by `pricing_charges.tenant_id`, the tenant of the control
//! plane's own key record, never by the event's `team_id`. That column and
//! `user_id` and `api_key_id` are `text`, so every comparison casts the uuid
//! side.
//!
//! A tenant never learns the channel. Nothing is read out of
//! `usage_events.event` beyond the whitelist in [`EVENT_FIELDS`]; a status
//! and an error are shown through the fixed vocabulary of [`tenant_outcome`]
//! and [`tenant_error`], so an upstream's own status, the recorded error
//! class and its message never reach an answer, and a model the key may not
//! use reads exactly like one that does not exist. A request's failed
//! failover attempts are hidden unless the upstream billed them or a
//! re-pricing run adjusted them ([`visible_to_tenant`]).
//!
//! A charge is written once; a re-pricing run corrects it with adjustments
//! (`pricing_adjustments`). A call shows their sum beside its charge
//! (`adjusted_micro_cny`), and a summary's `charged_micro_cny` is the
//! corrected charge: the charges plus their adjustments. What a corrected
//! charge bills is the latest run's: a call's lines, rate, discount and tier,
//! and a summary's quantities, are the ones that run priced it at
//! ([`EFFECTIVE`]).

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use aisix_core::usage_io::UsageIoAudit;
use aisix_pricing::{cny_per_million, CnyPerUsd, Rate};

use super::range::{checked_range, Group};
use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::resources::Kind;
use crate::usage::{page, page_limit, query_rejected, CallFilters};
use crate::AppState;

/// What a tenant sees of a call's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    Succeeded,
    Retried,
    Failed,
    Unknown,
}

/// The `error_class` a tenant sees for a recorded one. A model the key may
/// not use reads as an unknown model: the gateway answers it `403` outside
/// platform mode, and the tenant must not learn a model exists.
pub fn tenant_error(error_class: &str, error_message: &str) -> &'static str {
    if error_class == "permission_denied"
        && error_message.starts_with("API key is not allowed to use model")
    {
        return "model_not_found";
    }
    TENANT_ERRORS
        .iter()
        .find_map(|(recorded, shown)| (*recorded == error_class).then_some(*shown))
        .unwrap_or("failed")
}

// 显示和筛选共用同一词汇，避免用筛选探测不对租户公开的错误分类。
const TENANT_ERRORS: &[(&str, &str)] = &[
    ("model_not_found", "model_not_found"),
    ("client_disconnected", "client_disconnected"),
    ("invalid_request_error", "invalid_request"),
    ("rate_limit_exceeded", "rate_limited"),
    ("billing_error", "budget_exceeded"),
    ("content_filter", "content_filtered"),
    ("provider_unavailable", "upstream_unavailable"),
    ("all_candidates_unavailable", "upstream_unavailable"),
    ("timeout", "upstream_unavailable"),
    ("upstream_status", "upstream_unavailable"),
    ("upstream_decode", "upstream_unavailable"),
    ("upstream_in_band", "upstream_unavailable"),
    ("transport", "upstream_unavailable"),
    ("stream_aborted", "upstream_unavailable"),
    ("invalid_credentials", "upstream_unavailable"),
    ("config", "upstream_unavailable"),
    ("invalid_config", "upstream_unavailable"),
];

pub(crate) fn is_tenant_error(value: &str) -> bool {
    value == "failed" || TENANT_ERRORS.iter().any(|(_, shown)| *shown == value)
}

pub(crate) fn tenant_error_sql() -> String {
    let mut sql = String::from(
        "CASE WHEN ue.event->>'error_class' = 'permission_denied' \
         AND starts_with(COALESCE(ue.event->>'error_message', ''), \
         'API key is not allowed to use model') THEN 'model_not_found' \
         ELSE CASE ue.event->>'error_class'",
    );
    for (recorded, shown) in TENANT_ERRORS {
        sql.push_str(&format!(" WHEN '{recorded}' THEN '{shown}'"));
    }
    sql.push_str(" ELSE 'failed' END END");
    sql
}

/// `(status, status_code shown, error shown)` of one recorded attempt. Only
/// a final success shows its HTTP status; a failure shows the fixed
/// vocabulary only; an attempt that was not the request's last shows
/// neither. A final row that recorded an error class is a failure even when
/// its status is 2xx (a stream that broke after its first byte).
/// 缺失终态标记或有效 HTTP 状态的记录保持未知，不补造失败或重试。
pub fn tenant_outcome(
    status_code: Option<u16>,
    error_class: &str,
    error_message: &str,
    is_final_attempt: Option<bool>,
) -> (TenantStatus, Option<u16>, Option<&'static str>) {
    if is_final_attempt == Some(false) {
        return (TenantStatus::Retried, None, None);
    }
    if is_final_attempt.is_none() {
        return (TenantStatus::Unknown, None, None);
    }
    let status_code = status_code.filter(|code| (100..600).contains(code));
    if error_class.is_empty() {
        match status_code {
            Some(code) if (200..300).contains(&code) => {
                return (TenantStatus::Succeeded, Some(code), None);
            }
            None => return (TenantStatus::Unknown, None, None),
            _ => {}
        }
    }
    (
        TenantStatus::Failed,
        None,
        Some(tenant_error(error_class, error_message)),
    )
}

/// A charge's status as a tenant reads it: a row not priced yet is pending,
/// and one a re-pricing run adjusted (`repriced`) is charged, whatever it was
/// stored as: a run adjusts only a row it charges.
pub fn tenant_charge_status(status: &str, repriced: bool) -> &'static str {
    if repriced {
        return "charged";
    }
    match status {
        "charged" => "charged",
        "exempt" => "exempt",
        "nothing_to_bill" => "nothing_to_bill",
        _ => "pending",
    }
}

/// Whether a tenant sees the row: every final attempt, and a non-final one
/// only when it is billed: its charge is `charged` (the upstream billed it),
/// or a re-pricing run adjusted it (`repriced`). The listing applies the
/// same rule in SQL ([`VISIBLE`]) so a page is never short.
pub fn visible_to_tenant(is_final_attempt: bool, charge_status: &str, repriced: bool) -> bool {
    is_final_attempt || charge_status == "charged" || repriced
}

/// [`visible_to_tenant`] over the joined `pc` (charge) and `ue` (usage row).
pub(crate) const VISIBLE: &str =
    "(ue.event->'is_final_attempt' = 'true'::jsonb OR pc.status = 'charged' \
     OR EXISTS (SELECT 1 FROM pricing_adjustments_cny pa WHERE pa.event_id = pc.event_id))";

/// The sum of the adjustments of the charge `pc`, `NULL` when it has none.
pub(crate) const ADJUSTED: &str =
    "(SELECT SUM(pa.delta_micro_cny)::bigint FROM pricing_adjustments_cny pa \
     WHERE pa.event_id = pc.event_id)";

/// The latest re-pricing adjustment of the charge `pc`, joined as `eff`
/// (` LEFT JOIN LATERAL …`). A charge is written once, so one a run
/// corrected is shown with that run's lines, rate, discount and tier
/// ([`EFF_LINES`] and its siblings), the ones its corrected amount was
/// computed from — the **latest** adjustment's, whichever release wrote it.
/// The selection is by `created_at` alone and never by whether a detail was
/// recorded: an adjustment a release before `0038` wrote carries none, and
/// filtering those out would show an earlier run's rate beside the latest
/// run's amount. Two adjustments of one charge can land in the same
/// microsecond (a re-pricing retry beside a release that wrote no detail), so
/// the order is broken by `run_id` — part of the table's own key, and
/// therefore the same row for every query that reads it. `eff` is all NULL
/// only for a charge no run adjusted.
pub(crate) const EFFECTIVE: &str = " LEFT JOIN LATERAL (\
    SELECT pa.lines, pa.cny_per_usd_e4, pa.discount_bps, pa.service_tier, pa.created_at \
    FROM pricing_adjustments_cny pa WHERE pa.event_id = pc.event_id \
    ORDER BY pa.created_at DESC, pa.run_id DESC LIMIT 1) eff ON true";

/// The charge's lines as [`EFFECTIVE`] corrects them; `NULL` when the latest
/// adjustment recorded none, which a caller shows as unknown rather than
/// substituting an earlier run's.
pub(crate) const EFF_LINES: &str =
    "(CASE WHEN eff.created_at IS NULL THEN pc.lines WHEN eff.lines IS NULL THEN NULL \
      ELSE eff.lines END)";

/// The charge's group rate as [`EFFECTIVE`] corrects it.
const EFF_RATE: &str = "(CASE WHEN eff.created_at IS NULL THEN pc.cny_per_usd_e4 \
      WHEN eff.lines IS NULL THEN NULL ELSE eff.cny_per_usd_e4 END)";

/// The charge's discount as [`EFFECTIVE`] corrects it.
const EFF_DISCOUNT: &str = "(CASE WHEN eff.created_at IS NULL THEN COALESCE(pc.discount_bps, 0) \
      WHEN eff.lines IS NULL THEN NULL ELSE COALESCE(eff.discount_bps, 0) END)";

/// The charge's served tier as [`EFFECTIVE`] corrects it.
const EFF_TIER: &str = "(CASE WHEN eff.created_at IS NULL THEN pc.service_tier \
      WHEN eff.lines IS NULL THEN NULL ELSE eff.service_tier END)";

/// The only keys read out of a usage row's event.
pub(super) const EVENT_FIELDS: &str = "jsonb_build_object(\
    'requested_model', ue.event->'requested_model', \
    'operation', ue.event->'operation', \
    'status_code', ue.event->'status_code', \
    'error_class', ue.event->'error_class', \
    'error_message', ue.event->'error_message', \
    'is_final_attempt', ue.event->'is_final_attempt', \
    'inference_stage', ue.event->'inference_stage', \
    'cache_status', ue.event->'cache_status', \
    'guardrail_blocked', ue.event->'guardrail_blocked', \
    'downstream_latency_ms', ue.event->'downstream_latency_ms', \
    'first_token_latency_ms', ue.event->'first_token_latency_ms', \
    'upstream_latency_ms', ue.event->'upstream_latency_ms', \
    'upstream_tpot_ms', ue.event->'upstream_tpot_ms', \
    'upstream_dispatched', ue.event->'upstream_dispatched', \
    'io_audit', ue.event->'io_audit', \
    'usage_estimated', ue.event->'usage_estimated', \
    'usage', jsonb_build_object(\
        'prompt_tokens', ue.event->'prompt_tokens', \
        'completion_tokens', ue.event->'completion_tokens', \
        'cached_prompt_tokens', ue.event->'cached_prompt_tokens', \
        'cache_write_tokens', ue.event->'cache_write_tokens', \
        'cache_creation_tokens', ue.event->'cache_creation_tokens', \
        'cache_creation_1h_tokens', ue.event->'cache_creation_1h_tokens', \
        'cache_read_tokens', ue.event->'cache_read_tokens', \
        'reasoning_tokens', ue.event->'reasoning_tokens', \
        'web_search_requests', ue.event->'web_search_requests', \
        'total_tokens', ue.event->'total_tokens'))";

/// 对应版本边界内已观测文本的未校准参考；不参与价格、配额或用量改写。
#[derive(Debug, Serialize)]
pub struct IoTokenReference {
    method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
}

/// 详情共用的只读投影，保留缺失、真实零和网关估算来源。
#[derive(Debug, Serialize)]
pub struct CallUsage {
    first_token_latency_ms: Option<u32>,
    /// 本次上游尝试的完整耗时，不以客户端首帧耗时代替。
    attempt_duration_ms: Option<u32>,
    throughput_tps: Option<f64>,
    usage: Option<serde_json::Map<String, Value>>,
    usage_estimated: Option<bool>,
    usage_input_tokens: Option<u64>,
    io_audit: Option<UsageIoAudit>,
    io_token_reference: Option<IoTokenReference>,
}

impl CallUsage {
    pub(crate) fn from_event(ev: &Value) -> Self {
        let io_audit = ev
            .get("io_audit")
            .and_then(|raw| serde_json::from_value::<UsageIoAudit>(raw.clone()).ok())
            .filter(UsageIoAudit::validate);
        let io_token_reference = io_audit
            .as_ref()
            // 多次真实交换的聚合长度不能与某一次调用的记录 Token 作单次对照。
            .filter(|audit| audit.version != 2 || audit.exchange_count.is_none_or(|count| count <= 1))
            .map(|audit| {
                // 除法后再向上取整，避免 u64 上界的加法溢出。
                let estimate = |bytes: u64| bytes.div_ceil(4);
                IoTokenReference {
                    method: "utf8_bytes_div4_v1",
                    input_tokens: audit
                        .input_text
                        .as_ref()
                        .map(|text| estimate(text.utf8_bytes)),
                    output_tokens: audit
                        .output_text
                        .as_ref()
                        .map(|text| estimate(text.utf8_bytes)),
                }
            });
        Self {
            first_token_latency_ms: event_milliseconds(ev, "first_token_latency_ms"),
            attempt_duration_ms: event_milliseconds(ev, "upstream_latency_ms").filter(|ms| *ms > 0),
            throughput_tps: measured_throughput(ev),
            usage: ev.get("usage").and_then(Value::as_object).map(|usage| {
                usage
                    .iter()
                    .filter(|(_, value)| value.is_number())
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            }),
            usage_estimated: ev.get("usage_estimated").and_then(Value::as_bool),
            usage_input_tokens: stored_input_tokens(&ev["usage"]),
            io_audit,
            io_token_reference,
        }
    }
}

fn event_milliseconds(event: &Value, field: &str) -> Option<u32> {
    event
        .get(field)?
        .as_u64()
        .and_then(|ms| u32::try_from(ms).ok())
}

/// 复用监测中的 TPOT 口径；不从完整耗时、字节参考或缺失字段推算吞吐。
fn measured_throughput(event: &Value) -> Option<f64> {
    let status = event.get("status_code")?.as_u64()?;
    if !(200..300).contains(&status)
        || event
            .get("error_class")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        || event.get("cache_status").and_then(Value::as_str) == Some("hit")
        || event.get("upstream_dispatched").and_then(Value::as_bool) == Some(false)
    {
        return None;
    }
    let tpot = event.get("upstream_tpot_ms")?.as_f64()?;
    (1e-100..=1e100).contains(&tpot).then(|| 1000.0 / tpot)
}

fn stored_input_tokens(usage: &Value) -> Option<u64> {
    let prompt = usage.get("prompt_tokens")?.as_u64()?;
    let optional = |key: &str| match usage.get(key) {
        None | Some(Value::Null) => Some(0),
        Some(value) => value.as_u64(),
    };
    let cached = optional("cached_prompt_tokens")?;
    let written = optional("cache_write_tokens")?;
    let read = optional("cache_read_tokens")?;
    let created = optional("cache_creation_tokens")?;
    if cached.checked_add(written)? > prompt {
        return None;
    }
    cached.checked_add(read)?;
    written.checked_add(created)?;
    let input = aisix_core::input_tokens_for_pricing(prompt, cached, written, read, created);
    input
        .uncached
        .checked_add(input.cache_read)?
        .checked_add(input.cache_write)
}

/// A model as a tenant sees it: `id` only while the model is in the
/// tenant's current grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelRef {
    pub id: Option<Uuid>,
    pub name: String,
}

/// The models a page of rows names, with what the tenant may be told of
/// them.
pub struct Models {
    granted: HashSet<Uuid>,
    names: HashMap<Uuid, String>,
}

impl Models {
    /// Reads the model scope of `tenant` and the current names of `ids`.
    /// A model outside that scope is named without its id, as one the tenant
    /// does not reach; an all-scope tenant reaches every live public model.
    pub async fn load(
        conn: &mut PgConnection,
        tenant: Uuid,
        ids: &[Uuid],
    ) -> Result<Models, DbError> {
        let scope = crate::tenants::scope_model_ids(&mut *conn, tenant).await?;
        let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
            "SELECT r.id, COALESCE(r.doc->>'display_name', ''), r.id = ANY($2) \
             FROM gw_resources r \
             WHERE r.id = ANY($3) AND r.kind = $1 AND NOT r.deleted",
        )
        .bind(Kind::Models.segment())
        .bind(&scope)
        .bind(ids)
        .fetch_all(conn)
        .await?;
        let mut models = Models {
            granted: HashSet::new(),
            names: HashMap::new(),
        };
        for (id, name, granted) in rows {
            if granted {
                models.granted.insert(id);
            }
            models.names.insert(id, name);
        }
        Ok(models)
    }
}

/// A call's `model`: `id` only while the model is in the tenant's current
/// grants, `name` the one the caller sent (`event_name`), else the model's
/// current display name.
pub fn call_model(models: &Models, requested_model_id: &str, event_name: &str) -> ModelRef {
    let id = Uuid::parse_str(requested_model_id).ok();
    let name = if event_name.is_empty() {
        id.filter(|id| models.granted.contains(id))
            .and_then(|id| models.names.get(&id).cloned())
            .unwrap_or_default()
    } else {
        event_name.to_string()
    };
    ModelRef {
        id: id.filter(|id| models.granted.contains(id)),
        name,
    }
}

#[derive(Debug, Serialize)]
pub struct KeyRef {
    pub id: String,
    pub label: String,
    pub prefix: String,
}

#[derive(Debug, Serialize)]
pub struct MemberRef {
    pub id: Uuid,
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChargeView {
    pub status: &'static str,
    pub amount_micro_cny: Option<i64>,
    /// The group the charge was priced in, shown by its display name even
    /// once the group is retired; `null` for a row priced before its key
    /// took a group.
    pub group: Option<ChargeGroup>,
    /// The sum of the event's re-pricing adjustments; 0 when no run adjusted
    /// it.
    pub adjusted_micro_cny: i64,
    pub service_tier: Option<String>,
    pub estimated: bool,
    /// The discount the charge was priced at. `null` when the run that
    /// corrected it recorded none: the amount is the latest run's, and the
    /// rate it was computed from is not known.
    pub discount_bps: Option<i64>,
    /// The charge's lines, `null` under the same condition as
    /// `discount_bps`.
    pub lines: Option<Value>,
    pub calculation: ChargeCalculation,
}

/// 金额以十进制字符串保留完整存储精度，未知金额保持 null。
#[derive(Debug, Serialize)]
pub struct ChargeCalculation {
    pub original_micro_cny: Option<String>,
    pub adjusted_micro_cny: String,
    pub final_micro_cny: Option<String>,
    pub rounding: &'static str,
}

/// One call of the tenant.
#[derive(Debug, Serialize)]
pub struct Call {
    pub event_id: String,
    pub occurred_at: DateTime<Utc>,
    pub request_id: Option<String>,
    pub model: ModelRef,
    pub api_key: KeyRef,
    pub member: Option<MemberRef>,
    pub operation: String,
    pub status: TenantStatus,
    pub status_code: Option<u16>,
    pub error_class: Option<&'static str>,
    pub cache_hit: bool,
    pub guardrail_blocked: bool,
    pub latency_ms: Option<u32>,
    #[serde(flatten)]
    pub usage: CallUsage,
    pub charge: ChargeView,
}

#[derive(Deserialize)]
pub struct CallsQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    api_key_id: Option<String>,
    model_id: Option<String>,
    request_id: Option<String>,
    member_id: Option<String>,
    outcome: Option<String>,
    http_status: Option<u16>,
    error_class: Option<String>,
    cache_hit: Option<bool>,
    service_tier: Option<String>,
    operation: Option<String>,
    final_attempt: Option<bool>,
    limit: Option<i64>,
    /// Only calls with a smaller `seq`: the previous page's `next_before`.
    before: Option<i64>,
}

#[derive(Deserialize)]
pub struct SummaryQuery {
    group: Group,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    api_key_id: Option<String>,
    model_id: Option<String>,
}

fn refused(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.to_string(),
    }])
}

/// A filter id that must be a uuid; anything else is `422` at its field.
fn uuid_filter(raw: &Option<String>, path: &str) -> Result<Option<Uuid>, ApiError> {
    match raw {
        None => Ok(None),
        Some(raw) => Uuid::parse_str(raw)
            .map(Some)
            .map_err(|_| refused(path, "标识不合法")),
    }
}

/// Whose rows, and which of them.
struct Scope {
    tenant: Uuid,
    /// Only the calls of this member's keys.
    member: Option<Uuid>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    api_key_id: Option<Uuid>,
    model_id: Option<Uuid>,
    request_id: Option<String>,
    before: Option<i64>,
}

impl Scope {
    /// ` FROM ... WHERE ...` over the charge `pc` and its usage row `ue`,
    /// with the correction `eff` ([`EFFECTIVE`]), and one row per charge
    /// line `l` (the corrected lines) when `lines` is set.
    fn push_from(&self, sql: &mut QueryBuilder<'_, Postgres>, lines: bool) {
        sql.push(" FROM pricing_charges_cny pc JOIN usage_events ue ON ue.event_id = pc.event_id");
        // The display name only: a charge's group is shown by what tenants may
        // see, never by the routing name its provider keys carry.
        sql.push(" LEFT JOIN provider_groups g ON g.id = pc.group_id");
        sql.push(EFFECTIVE);
        if lines {
            sql.push(format!(
                " CROSS JOIN LATERAL jsonb_array_elements({EFF_LINES}) l"
            ));
        }
        sql.push(" WHERE pc.tenant_id = ");
        sql.push_bind(self.tenant).push("::text");
        sql.push(" AND ").push(VISIBLE);
        if let Some(member) = self.member {
            sql.push(" AND pc.user_id = ")
                .push_bind(member)
                .push("::text");
        }
        if let Some(from) = self.from {
            sql.push(" AND pc.occurred_at >= ").push_bind(from);
        }
        if let Some(to) = self.to {
            sql.push(" AND pc.occurred_at < ").push_bind(to);
        }
        if let Some(key) = self.api_key_id {
            sql.push(" AND pc.api_key_id = ")
                .push_bind(key)
                .push("::text");
        }
        if let Some(model) = self.model_id {
            sql.push(" AND pc.requested_model_id = ")
                .push_bind(model)
                .push("::text");
        }
        if let Some(request_id) = &self.request_id {
            sql.push(" AND pc.request_id = ")
                .push_bind(request_id.clone());
        }
        if let Some(before) = self.before {
            sql.push(" AND pc.seq < ").push_bind(before);
        }
    }
}

#[derive(sqlx::FromRow)]
struct CallRow {
    seq: i64,
    event_id: String,
    occurred_at: DateTime<Utc>,
    api_key_id: String,
    user_id: String,
    requested_model_id: String,
    status: String,
    charge_micro_cny: Option<i64>,
    service_tier: Option<String>,
    estimated: bool,
    discount_bps: Option<i64>,
    lines: Option<Value>,
    cny_per_usd_e4: Option<i64>,
    group_id: Option<Uuid>,
    group_name: Option<String>,
    /// The sum of the charge's adjustments, `None` when it has none.
    adjusted: Option<i64>,
    request_id: Option<String>,
    ev: Value,
}

/// The text field `key` of `ev`, `""` when it has none.
pub(super) fn text(ev: &Value, key: &str) -> String {
    ev.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// What a tenant is shown of one recorded attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TenantView {
    pub status: TenantStatus,
    /// Set for a final success only.
    pub status_code: Option<u16>,
    /// From the fixed vocabulary, for a final failure only.
    pub error: Option<&'static str>,
    /// The charge's status as a tenant reads it.
    pub charge_status: &'static str,
}

/// How a tenant sees the usage row `ev` (read through [`EVENT_FIELDS`])
/// whose charge is stored as `charge_status`, with `repriced` when a
/// re-pricing run adjusted it; `None` when the row is hidden from the tenant
/// ([`visible_to_tenant`]). Every tenant view of a row goes through here, so
/// they cannot disagree on what a row shows.
pub(super) fn tenant_view(ev: &Value, charge_status: &str, repriced: bool) -> Option<TenantView> {
    let is_final = ev.get("is_final_attempt").and_then(Value::as_bool);
    if !visible_to_tenant(is_final == Some(true), charge_status, repriced) {
        return None;
    }
    let status_code = ev
        .get("status_code")
        .and_then(Value::as_u64)
        .and_then(|c| u16::try_from(c).ok());
    // 后台受理尚无推理终态，不能将首次成功受理显示成已重试。
    let outcome_final = if is_final == Some(false)
        && matches!(
            ev.get("inference_stage").and_then(Value::as_str),
            Some("accepted" | "pending")
        ) {
        None
    } else {
        is_final
    };
    let (status, status_code, error) = tenant_outcome(
        status_code,
        &text(ev, "error_class"),
        &text(ev, "error_message"),
        outcome_final,
    );
    Some(TenantView {
        status,
        status_code,
        error,
        charge_status: tenant_charge_status(charge_status, repriced),
    })
}

/// A charge's stored `lines` as a tenant reads them: each line's dimension,
/// quantity and CNY rate per million, and nothing else a stored line may
/// carry. A stored line's `rate` is the baseline's in micro-USD per million;
/// the tenant is shown the price it was charged at, so the rate is converted
/// here at the rate recorded on the charge. The tenant's discount is NOT in
/// `rate_cny` (it is shown separately as `discount_bps`), so a discounted
/// tenant's `rate_cny × quantity` is more than the amount charged. A line
/// whose stored rate this
/// cannot read shows no rate rather than a wrong one.
fn tenant_lines(lines: &Value, cny_per_usd_e4: Option<i64>) -> Value {
    let cny = cny_per_usd_e4
        .and_then(|e4| u32::try_from(e4).ok())
        .and_then(|e4| CnyPerUsd::parse(&CnyPerUsd(e4).to_string()).ok());
    Value::Array(
        lines
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| {
                let line_rate = l
                    .get("rate")
                    .and_then(Value::as_str)
                    .and_then(|r| Rate::parse(r).ok());
                let rate_cny = match (line_rate, cny) {
                    (Some(rate), Some(cny)) => cny_per_million(rate, cny, 0),
                    _ => None,
                };
                json!({
                    "dimension": l.get("dimension"),
                    "quantity": l.get("quantity"),
                    "rate_cny": rate_cny,
                })
            })
            .collect(),
    )
}

/// The group a charge was priced in, as a tenant reads it: its display name,
/// never the routing name the platform's provider keys carry.
#[derive(Debug, Serialize)]
pub struct ChargeGroup {
    pub id: Uuid,
    pub display_name: String,
}

/// `GET /api/usage/calls` → `{items: [Call], next_before}`, newest first.
pub async fn list_calls(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<CallsQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ])?;
    let Query(q) = query.map_err(query_rejected)?;
    let filters = CallFilters {
        outcome: q.outcome,
        http_status: q.http_status,
        error_class: q.error_class,
        cache_hit: q.cache_hit,
        service_tier: q.service_tier,
        operation: q.operation,
        final_attempt: q.final_attempt,
    };
    filters.validate(true)?;
    let member_filter = uuid_filter(&q.member_id, "/member_id")?;
    let (from, to) = match (q.from, q.to) {
        (Some(from), Some(to)) => {
            let (f, t) = checked_range(from, to, None)?;
            (Some(f), Some(t))
        }
        other => other,
    };
    let scope = Scope {
        tenant: auth.tenant_id,
        member: (auth.role == MemberRole::Member).then_some(auth.id),
        from,
        to,
        api_key_id: uuid_filter(&q.api_key_id, "/api_key_id")?,
        model_id: uuid_filter(&q.model_id, "/model_id")?,
        request_id: q.request_id,
        before: q.before,
    };
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(format!(
        "SELECT pc.seq, pc.event_id, pc.occurred_at, pc.api_key_id, pc.user_id, \
         pc.requested_model_id, pc.status, pc.charge_micro_cny, {EFF_TIER} AS service_tier, \
         pc.estimated, {EFF_DISCOUNT} AS discount_bps, {EFF_LINES} AS lines, \
         {EFF_RATE} AS cny_per_usd_e4, pc.group_id, g.display_name AS group_name, \
         ue.request_id, ",
    ));
    sql.push(ADJUSTED).push(" AS adjusted, ");
    sql.push(EVENT_FIELDS).push(" AS ev");
    scope.push_from(&mut sql, false);
    if let Some(member) = member_filter {
        sql.push(" AND pc.user_id = ")
            .push_bind(member)
            .push("::text");
    }
    filters.push(&mut sql, EFF_TIER, true);
    sql.push(" ORDER BY pc.seq DESC LIMIT ")
        .push_bind(limit + 1);
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let rows: Vec<CallRow> = sql
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let (rows, next_before) = page(rows, limit, |r| r.seq);

    let model_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| Uuid::parse_str(&r.requested_model_id).ok())
        .collect();
    let key_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| Uuid::parse_str(&r.api_key_id).ok())
        .collect();
    let member_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| Uuid::parse_str(&r.user_id).ok())
        .collect();
    let models = Models::load(&mut conn, auth.tenant_id, &model_ids).await?;
    let keys: HashMap<Uuid, (String, String)> = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT id, label, key_prefix FROM tenant_keys WHERE tenant_id = $1 AND id = ANY($2)",
    )
    .bind(auth.tenant_id)
    .bind(&key_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?
    .into_iter()
    .map(|(id, label, prefix)| (id, (label, prefix)))
    .collect();
    let members: HashMap<Uuid, Option<String>> = sqlx::query_as::<_, (Uuid, Option<String>)>(
        "SELECT id, display_name FROM members WHERE tenant_id = $1 AND id = ANY($2)",
    )
    .bind(auth.tenant_id)
    .bind(&member_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?
    .into_iter()
    .collect();

    let items: Vec<Call> = rows
        .into_iter()
        .filter_map(|r| {
            let view = tenant_view(&r.ev, &r.status, r.adjusted.is_some())?;
            let key = Uuid::parse_str(&r.api_key_id)
                .ok()
                .and_then(|id| keys.get(&id));
            let member = Uuid::parse_str(&r.user_id).ok().map(|id| MemberRef {
                id,
                display_name: members.get(&id).cloned().flatten(),
            });
            Some(Call {
                model: call_model(
                    &models,
                    &r.requested_model_id,
                    &text(&r.ev, "requested_model"),
                ),
                api_key: KeyRef {
                    id: r.api_key_id,
                    label: key.map(|k| k.0.clone()).unwrap_or_default(),
                    prefix: key.map(|k| k.1.clone()).unwrap_or_default(),
                },
                member,
                operation: text(&r.ev, "operation"),
                status: view.status,
                status_code: view.status_code,
                error_class: view.error,
                cache_hit: text(&r.ev, "cache_status") == "hit",
                guardrail_blocked: r.ev.get("guardrail_blocked").and_then(Value::as_bool)
                    == Some(true),
                latency_ms: r
                    .ev
                    .get("downstream_latency_ms")
                    .and_then(Value::as_u64)
                    .and_then(|ms| u32::try_from(ms).ok())
                    .filter(|ms| *ms > 0),
                usage: CallUsage::from_event(&r.ev),
                charge: ChargeView {
                    calculation: ChargeCalculation {
                        original_micro_cny: r.charge_micro_cny.map(|amount| amount.to_string()),
                        adjusted_micro_cny: r.adjusted.unwrap_or(0).to_string(),
                        final_micro_cny: (view.charge_status != "pending").then(|| {
                            (i128::from(r.charge_micro_cny.unwrap_or(0))
                                + i128::from(r.adjusted.unwrap_or(0)))
                            .to_string()
                        }),
                        rounding: "ceil_micro_cny",
                    },
                    status: view.charge_status,
                    amount_micro_cny: r.charge_micro_cny,
                    group: r
                        .group_id
                        .zip(r.group_name.clone())
                        .map(|(id, display_name)| ChargeGroup { id, display_name }),
                    adjusted_micro_cny: r.adjusted.unwrap_or(0),
                    service_tier: r.service_tier,
                    estimated: r.estimated,
                    discount_bps: r.discount_bps,
                    lines: r
                        .lines
                        .as_ref()
                        .map(|lines| tenant_lines(lines, r.cny_per_usd_e4)),
                },
                event_id: r.event_id,
                occurred_at: r.occurred_at,
                request_id: r.request_id,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

/// One period and model of a usage summary.
#[derive(Debug, Serialize)]
pub struct UsageSummaryRow {
    pub period_start: DateTime<Utc>,
    pub model: ModelRef,
    pub requests: i64,
    pub charged_micro_cny: i64,
    pub estimated_requests: i64,
    pub quantities: BTreeMap<String, i64>,
}

/// The most rows (a period and a model) a summary lists; the rest are rolled
/// into one `other` row, so the answer's size does not depend on how much
/// traffic the range holds.
pub const MAX_SUMMARY_ROWS: usize = 1000;

/// The name of the row that holds everything past [`MAX_SUMMARY_ROWS`].
const OTHER: &str = "other";

/// Runs `summarize` in one read-only repeatable-read transaction, so its
/// queries see the same rows.
async fn summarize_snapshot(
    state: &AppState,
    scope: &Scope,
    group: Group,
) -> Result<Vec<UsageSummaryRow>, ApiError> {
    let mut tx = state.db.pool().begin().await.map_err(DbError::from)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let rows = summarize(&mut tx, scope, group).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(rows)
}

/// Rows by period and model. A row is grouped by the model's id, which the
/// gateway resolved, never by the name a caller sent: calls whose model is
/// not in the tenant's grants (or never resolved) share one row per period
/// with `id` null and an empty name, and a granted model is named by its
/// current display name. `charged_micro_cny` is the corrected charge: the
/// charged rows' charges plus every row's re-pricing adjustments, in the
/// period the row occurred in.
async fn summarize(
    conn: &mut PgConnection,
    scope: &Scope,
    group: Group,
) -> Result<Vec<UsageSummaryRow>, ApiError> {
    let unit = match group {
        Group::Day => "day",
        Group::Month => "month",
    };

    let mut sql = QueryBuilder::<Postgres>::new("SELECT date_trunc(");
    sql.push_bind(unit).push(format!(
        ", pc.occurred_at, 'Asia/Shanghai'), pc.requested_model_id, count(*)::bigint, \
         COALESCE(sum(pc.charge_micro_cny) FILTER (WHERE pc.status = 'charged'), 0)::bigint \
           + sum(COALESCE({ADJUSTED}, 0))::bigint, \
         (count(*) FILTER (WHERE pc.estimated))::bigint"
    ));
    scope.push_from(&mut sql, false);
    sql.push(" GROUP BY 1, 2 ORDER BY 1, 2");
    let rows: Vec<(DateTime<Utc>, String, i64, i64, i64)> = sql
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(DbError::from)?;

    let mut sql = QueryBuilder::<Postgres>::new("SELECT date_trunc(");
    sql.push_bind(unit).push(
        ", pc.occurred_at, 'Asia/Shanghai'), pc.requested_model_id, l->>'dimension', \
         COALESCE(sum((l->>'quantity')::bigint), 0)::bigint",
    );
    scope.push_from(&mut sql, true);
    sql.push(" GROUP BY 1, 2, 3");
    let quantity_rows: Vec<(DateTime<Utc>, String, String, i64)> = sql
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(DbError::from)?;

    let model_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| Uuid::parse_str(&r.1).ok())
        .collect();
    let models = Models::load(conn, scope.tenant, &model_ids).await?;

    // The shown model of a stored id: granted ids by name, all others as one.
    let shown = |raw: &str| call_model(&models, raw, "");
    let mut merged: BTreeMap<(DateTime<Utc>, Option<Uuid>), UsageSummaryRow> = BTreeMap::new();
    for (period, raw, requests, charged, estimated) in rows {
        let model = shown(&raw);
        let row = merged
            .entry((period, model.id))
            .or_insert_with(|| UsageSummaryRow {
                period_start: period,
                model: ModelRef {
                    id: model.id,
                    name: model.name.clone(),
                },
                requests: 0,
                charged_micro_cny: 0,
                estimated_requests: 0,
                quantities: BTreeMap::new(),
            });
        row.requests += requests;
        row.charged_micro_cny += charged;
        row.estimated_requests += estimated;
    }
    for (period, raw, dimension, quantity) in quantity_rows {
        let model = shown(&raw);
        if let Some(row) = merged.get_mut(&(period, model.id)) {
            *row.quantities.entry(dimension).or_insert(0) += quantity;
        }
    }

    let mut items: Vec<UsageSummaryRow> = merged.into_values().collect();
    if items.len() > MAX_SUMMARY_ROWS {
        let rest = items.split_off(MAX_SUMMARY_ROWS);
        let mut other = UsageSummaryRow {
            period_start: rest[0].period_start,
            model: ModelRef {
                id: None,
                name: OTHER.to_string(),
            },
            requests: 0,
            charged_micro_cny: 0,
            estimated_requests: 0,
            quantities: BTreeMap::new(),
        };
        for row in rest {
            other.requests += row.requests;
            other.charged_micro_cny += row.charged_micro_cny;
            other.estimated_requests += row.estimated_requests;
            for (dimension, quantity) in row.quantities {
                *other.quantities.entry(dimension).or_insert(0) += quantity;
            }
        }
        items.push(other);
    }
    Ok(items)
}

fn summary_scope(tenant: Uuid, member: Option<Uuid>, q: &SummaryQuery) -> Result<Scope, ApiError> {
    let (from, to) = checked_range(q.from, q.to, Some(q.group))?;
    Ok(Scope {
        tenant,
        member,
        from: Some(from),
        to: Some(to),
        api_key_id: uuid_filter(&q.api_key_id, "/api_key_id")?,
        model_id: uuid_filter(&q.model_id, "/model_id")?,
        request_id: None,
        before: None,
    })
}

/// `GET /api/usage/summary` → `{items: [UsageSummaryRow]}`.
pub async fn tenant_summary(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<SummaryQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ])?;
    let Query(q) = query.map_err(query_rejected)?;
    let scope = summary_scope(
        auth.tenant_id,
        (auth.role == MemberRole::Member).then_some(auth.id),
        &q,
    )?;
    let items = summarize_snapshot(&state, &scope, q.group).await?;
    Ok(Json(json!({ "items": items })))
}

/// `GET /admin/tenants/{id}/usage/summary` → `{items: [UsageSummaryRow]}`;
/// `404` for a tenant that does not exist.
pub async fn admin_summary(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<SummaryQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let Query(q) = query.map_err(query_rejected)?;
    let scope = summary_scope(id, None, &q)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1)")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
    if !exists {
        return Err(ApiError::not_found());
    }
    drop(conn);
    let items = summarize_snapshot(&state, &scope, q.group).await?;
    Ok(Json(json!({ "items": items })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregated_upstream_exchanges_keep_lengths_and_usage_without_a_single_call_reference() {
        for (version, boundary, count, reference_available) in [
            (2, "upstream_attempt", None, true),
            (2, "upstream_attempt", Some(0), true),
            (2, "upstream_attempt", Some(1), true),
            (2, "upstream_attempt", Some(2), false),
            (2, "upstream_attempt", Some(u32::MAX), false),
            (1, "client_exchange", Some(2), true),
        ] {
            let mut audit = json!({"version":version,"boundary":boundary,
                "request_body_bytes":234,"response_body_bytes":156,
                "request_complete":true,"response_complete":true,
                "input_text":{"utf8_bytes":16,"unicode_scalars":6,"complete":true},
                "output_text":{"utf8_bytes":10,"unicode_scalars":4,"complete":true}});
            if let Some(count) = count {
                audit["exchange_count"] = json!(count);
            }
            let usage = json!({"prompt_tokens":1000,"cached_prompt_tokens":800,
                "completion_tokens":200,"total_tokens":1200});
            let view = serde_json::to_value(CallUsage::from_event(&json!({
                "io_audit":audit,"usage":usage,"usage_estimated":false
            })))
            .expect("序列化只读用量投影");
            assert_eq!(view.get("io_audit"), Some(&audit), "{view}");
            assert_eq!(view.get("usage"), Some(&usage), "{view}");
            assert_eq!(view.get("usage_input_tokens"), Some(&json!(1000)), "{view}");
            assert_eq!(view.get("usage_estimated"), Some(&json!(false)), "{view}");
            if reference_available {
                assert_eq!(
                    view.get("io_token_reference"),
                    Some(
                        &json!({"method":"utf8_bytes_div4_v1","input_tokens":4,"output_tokens":3})
                    ),
                    "{view}"
                );
            } else {
                assert_eq!(view.get("io_token_reference"), Some(&Value::Null), "{view}");
            }
        }
    }

    #[test]
    fn a_forbidden_model_row_reads_like_an_unknown_model() {
        assert_eq!(
            tenant_error(
                "permission_denied",
                "API key is not allowed to use model \"x\""
            ),
            "model_not_found"
        );
        assert_eq!(
            tenant_error("permission_denied", "client IP not allowed"),
            "failed"
        );
        assert_eq!(tenant_error("model_not_found", ""), "model_not_found");
    }

    #[test]
    fn the_error_vocabulary_is_fixed() {
        for (class, shown) in [
            ("invalid_request_error", "invalid_request"),
            ("rate_limit_exceeded", "rate_limited"),
            ("billing_error", "budget_exceeded"),
            ("content_filter", "content_filtered"),
            ("provider_unavailable", "upstream_unavailable"),
            ("all_candidates_unavailable", "upstream_unavailable"),
            ("timeout", "upstream_unavailable"),
            ("upstream_status", "upstream_unavailable"),
            ("upstream_decode", "upstream_unavailable"),
            ("upstream_in_band", "upstream_unavailable"),
            ("transport", "upstream_unavailable"),
            ("stream_aborted", "upstream_unavailable"),
            ("invalid_credentials", "upstream_unavailable"),
            ("config", "upstream_unavailable"),
            ("invalid_config", "upstream_unavailable"),
            ("brand_new_class", "failed"),
            ("", "failed"),
        ] {
            assert_eq!(tenant_error(class, ""), shown, "{class}");
        }
    }

    #[test]
    fn an_attempt_shows_no_internal_status() {
        assert_eq!(
            tenant_outcome(Some(200), "", "", Some(true)),
            (TenantStatus::Succeeded, Some(200), None)
        );
        assert_eq!(
            tenant_outcome(Some(529), "upstream_status", "x", Some(false)),
            (TenantStatus::Retried, None, None)
        );
        assert_eq!(
            tenant_outcome(Some(529), "upstream_status", "x", Some(true)),
            (TenantStatus::Failed, None, Some("upstream_unavailable"))
        );
        assert_eq!(
            tenant_outcome(Some(500), "brand_new_class", "", Some(true)),
            (TenantStatus::Failed, None, Some("failed"))
        );
        // A stream that broke after a 200 is a failure, not a success.
        assert_eq!(
            tenant_outcome(Some(200), "stream_aborted", "", Some(true)),
            (TenantStatus::Failed, None, Some("upstream_unavailable"))
        );
    }

    #[test]
    fn unknown_outcome_keeps_status_and_error_private() {
        for status_code in [None, Some(0), Some(99), Some(600), Some(u16::MAX)] {
            assert_eq!(
                tenant_outcome(status_code, "", "", Some(true)),
                (TenantStatus::Unknown, None, None),
                "{status_code:?}"
            );
        }
        for status_code in [None, Some(200), Some(503)] {
            for error_class in ["", "upstream_status"] {
                assert_eq!(
                    tenant_outcome(status_code, error_class, "private upstream error", None),
                    (TenantStatus::Unknown, None, None)
                );
            }
        }
        assert_eq!(
            tenant_outcome(None, "stream_aborted", "private upstream error", Some(true)),
            (TenantStatus::Failed, None, Some("upstream_unavailable"))
        );
        assert_eq!(
            tenant_outcome(None, "", "", Some(false)),
            (TenantStatus::Retried, None, None)
        );
        for status_code in [100, 199, 300, 599] {
            assert_eq!(
                tenant_outcome(Some(status_code), "", "", Some(true)),
                (TenantStatus::Failed, None, Some("failed"))
            );
        }
    }

    #[test]
    fn tenant_view_preserves_optional_recorded_outcome() {
        for event in [
            json!({"is_final_attempt": true}),
            json!({"is_final_attempt": true, "status_code": null}),
            json!({"is_final_attempt": true, "status_code": "200"}),
            json!({"is_final_attempt": true, "status_code": 200.5}),
            json!({"is_final_attempt": true, "status_code": 9999999999999999999_u64}),
            json!({"status_code": 200}),
            json!({"is_final_attempt": null, "status_code": 503}),
            json!({"is_final_attempt": "false", "status_code": 503}),
        ] {
            assert_eq!(
                tenant_view(&event, "charged", false),
                Some(TenantView {
                    status: TenantStatus::Unknown,
                    status_code: None,
                    error: None,
                    charge_status: "charged",
                }),
                "{event}"
            );
        }
        assert_eq!(
            tenant_view(&json!({"status_code": 200}), "pending", false),
            None
        );
        assert_eq!(
            tenant_view(&json!({"status_code": 200}), "unpriced", true),
            Some(TenantView {
                status: TenantStatus::Unknown,
                status_code: None,
                error: None,
                charge_status: "charged",
            })
        );
    }

    #[test]
    fn an_unpriced_row_is_pending_to_a_tenant() {
        assert_eq!(tenant_charge_status("unpriced", false), "pending");
        assert_eq!(tenant_charge_status("charged", false), "charged");
        assert_eq!(tenant_charge_status("exempt", false), "exempt");
        assert_eq!(
            tenant_charge_status("nothing_to_bill", false),
            "nothing_to_bill"
        );
        // A re-pricing run charged it.
        assert_eq!(tenant_charge_status("unpriced", true), "charged");
    }

    #[test]
    fn only_final_or_billed_attempts_are_visible() {
        assert!(visible_to_tenant(true, "unpriced", false));
        assert!(visible_to_tenant(false, "charged", false));
        assert!(!visible_to_tenant(false, "unpriced", false));
        assert!(!visible_to_tenant(false, "nothing_to_bill", false));
        // A run billed it.
        assert!(visible_to_tenant(false, "unpriced", true));
    }

    #[test]
    fn visible_nonterminal_background_work_is_not_projected_as_a_retry() {
        for stage in ["accepted", "pending"] {
            let event = json!({"inference_stage":stage,"is_final_attempt":false,"status_code":200});
            let view = tenant_view(&event, "charged", false).expect("已收费记录可见");
            assert_eq!(view.status, TenantStatus::Unknown, "{stage}");
            assert_eq!(view.status_code, None, "{stage}");
            assert_eq!(view.error, None, "{stage}");
        }
    }
}
