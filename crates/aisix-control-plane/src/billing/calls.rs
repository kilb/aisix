//! A tenant's call log and usage summary: the priced usage rows of its own
//! keys, shown without anything of the platform's channel.
//!
//! - `GET /api/usage/calls`: the call log, newest first; an owner, admin or
//!   billing member sees every call of the tenant, a member only the calls
//!   of the keys bound to them (the charge's `user_id`, which is the key's
//!   member as the control plane recorded it).
//! - `GET /api/usage/summary`: requests, charges and billed quantities by
//!   UTC day or month and model, over the same rows.
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

use aisix_pricing::{cny_per_million, CnyPerUsd, Rate};

use super::range::{checked_range, Group};
use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::resources::Kind;
use crate::usage::{page, page_limit, query_rejected};
use crate::AppState;

/// What a tenant sees of a call's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    Succeeded,
    Retried,
    Failed,
}

/// The `error_class` a tenant sees for a recorded one. A model the key may
/// not use reads as an unknown model: the gateway answers it `403` outside
/// platform mode, and the tenant must not learn a model exists.
pub fn tenant_error(error_class: &str, error_message: &str) -> &'static str {
    match error_class {
        "model_not_found" => "model_not_found",
        "permission_denied" if error_message.starts_with("API key is not allowed to use model") => {
            "model_not_found"
        }
        "invalid_request_error" => "invalid_request",
        "rate_limit_exceeded" => "rate_limited",
        "billing_error" => "budget_exceeded",
        "content_filter" => "content_filtered",
        "provider_unavailable"
        | "all_candidates_unavailable"
        | "timeout"
        | "upstream_status"
        | "upstream_decode"
        | "upstream_in_band"
        | "transport"
        | "stream_aborted"
        | "invalid_credentials"
        | "config"
        | "invalid_config" => "upstream_unavailable",
        _ => "failed",
    }
}

/// `(status, status_code shown, error shown)` of one recorded attempt. Only
/// a final success shows its HTTP status; a failure shows the fixed
/// vocabulary only; an attempt that was not the request's last shows
/// neither. A final row that recorded an error class is a failure even when
/// its status is 2xx (a stream that broke after its first byte).
pub fn tenant_outcome(
    status_code: u16,
    error_class: &str,
    error_message: &str,
    is_final_attempt: bool,
) -> (TenantStatus, Option<u16>, Option<&'static str>) {
    if !is_final_attempt {
        return (TenantStatus::Retried, None, None);
    }
    if (200..300).contains(&status_code) && error_class.is_empty() {
        return (TenantStatus::Succeeded, Some(status_code), None);
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
pub(super) const VISIBLE: &str =
    "(ue.event->'is_final_attempt' = 'true'::jsonb OR pc.status = 'charged' \
     OR EXISTS (SELECT 1 FROM pricing_adjustments_cny pa WHERE pa.event_id = pc.event_id))";

/// The sum of the adjustments of the charge `pc`, `NULL` when it has none.
pub(super) const ADJUSTED: &str =
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
pub(super) const EFFECTIVE: &str = " LEFT JOIN LATERAL (\
    SELECT pa.lines, pa.cny_per_usd_e4, pa.discount_bps, pa.service_tier, pa.created_at \
    FROM pricing_adjustments_cny pa WHERE pa.event_id = pc.event_id \
    ORDER BY pa.created_at DESC, pa.run_id DESC LIMIT 1) eff ON true";

/// The charge's lines as [`EFFECTIVE`] corrects them; `NULL` when the latest
/// adjustment recorded none, which a caller shows as unknown rather than
/// substituting an earlier run's.
pub(super) const EFF_LINES: &str =
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
    'cache_status', ue.event->'cache_status', \
    'guardrail_blocked', ue.event->'guardrail_blocked', \
    'downstream_latency_ms', ue.event->'downstream_latency_ms')";

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
    pub charge: ChargeView,
}

#[derive(Deserialize)]
pub struct CallsQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    api_key_id: Option<String>,
    model_id: Option<String>,
    request_id: Option<String>,
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
    let is_final = ev.get("is_final_attempt").and_then(Value::as_bool) == Some(true);
    if !visible_to_tenant(is_final, charge_status, repriced) {
        return None;
    }
    let status_code = ev
        .get("status_code")
        .and_then(Value::as_u64)
        .and_then(|c| u16::try_from(c).ok())
        .unwrap_or(0);
    let (status, status_code, error) = tenant_outcome(
        status_code,
        &text(ev, "error_class"),
        &text(ev, "error_message"),
        is_final,
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
                charge: ChargeView {
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
        ", pc.occurred_at, 'UTC'), pc.requested_model_id, count(*)::bigint, \
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
        ", pc.occurred_at, 'UTC'), pc.requested_model_id, l->>'dimension', \
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
            tenant_outcome(200, "", "", true),
            (TenantStatus::Succeeded, Some(200), None)
        );
        assert_eq!(
            tenant_outcome(529, "upstream_status", "x", false),
            (TenantStatus::Retried, None, None)
        );
        assert_eq!(
            tenant_outcome(529, "upstream_status", "x", true),
            (TenantStatus::Failed, None, Some("upstream_unavailable"))
        );
        assert_eq!(
            tenant_outcome(500, "brand_new_class", "", true),
            (TenantStatus::Failed, None, Some("failed"))
        );
        // A stream that broke after a 200 is a failure, not a success.
        assert_eq!(
            tenant_outcome(200, "stream_aborted", "", true),
            (TenantStatus::Failed, None, Some("upstream_unavailable"))
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
}
