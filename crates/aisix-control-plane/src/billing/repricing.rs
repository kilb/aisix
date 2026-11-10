//! Re-pricing: correcting past charges at the prices in force now.
//!
//! A charge is written once and never updated, so a price entered late or
//! wrong is corrected by a run. An operator previews a run over a filter of
//! stored charges ([`RepriceFilter`]): each selected row is priced again by
//! the engine at the prices in force at the run's instant, at the plan of the
//! tenant the charge is filed under ([`reprice_one`]), and the run is stored
//! with its totals by tenant. A preview posts nothing. An admin then applies
//! it, in one transaction:
//!
//! 1. the billing locks of the tenants the preview changes, as the first
//!    locks of the transaction (`tenants::lock_many_billing`);
//! 2. the run's row, which must still be `previewed` (`409 run_state`);
//! 3. `pricing_version`, read `FOR SHARE`, so no price write commits until
//!    apply ends; it must be the preview's (`409 stale_preview`);
//! 4. the totals, computed again, which must equal the preview's
//!    (`409 stale_preview`);
//! 5. one `pricing_adjustments` row per changed charge, one `adjustment`
//!    ledger entry per tenant whose deltas do not sum to 0, the deltas
//!    counted against cumulative quotas at each charge's own price instant
//!    (`budgets::count_charges` in `Adjustment` mode), and every changed
//!    tenant's account flagged for the settler.
//!
//! Once it commits, each changed tenant's marks are settled in a transaction
//! of its own (`marks::settle`), so the answer already stands in the
//! gateway's documents; one that fails is left to the settler, which the
//! flag wakes. Settling them inside the apply would hold every changed
//! tenant's usage intake, and the resources lock from the first mark that
//! moves, for as long as all of them take.
//!
//! A charge's effective amount is its `charge_micro_cny` (0 when it has
//! none) plus its adjustments, and a run's delta for a row is the new charge
//! (0 unless the engine charges it) minus that. Two kinds of row are counted
//! and left as they are:
//! - a row the engine still refuses: `still_unpriced`;
//! - a row filed under no tenant, or one intake refused to attribute
//!   (`unknown_api_key`, `tenant_mismatch`, `undecodable_usage`):
//!   `skipped_unattributed`. Intake left these unbilled on purpose, and a run
//!   never bills them.
//!
//! Prices cannot be backdated, so the price a run uses is always one in
//! force at the run's instant: a window spanning a later price change is
//! priced at the newest one, and the operator chooses the window. The tenant
//! a row is priced for is the one its charge is filed under, the tenant of
//! the control plane's key record, never the row's own claim.
//!
//! The lock order is the order of every other money path: the tenants and
//! their accounts, then the run, then `pricing_version` (a price write
//! key-shares a tenant before it bumps the version, so the version is taken
//! after the tenants). The apply takes no resources lock. Intake for those
//! tenants waits on their accounts while an apply runs, which
//! [`MAX_REPRICE_EVENTS`] and [`MAX_REPRICE_TENANTS`] bound; each settle after
//! it holds one tenant's account at a time.

use std::collections::{BTreeMap, HashMap};

use aisix_pricing::{
    price_event, BillableUsage, Outcome, Payer, PriceBook, PricedEvent, ENGINE_VERSION,
};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use super::budgets::{self, ChargedRow, CountMode};
use super::ledger::{self, Posting, Source};
use super::marks;
use super::topups;
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::pricing::store;
use crate::resources::write::WriteCtx;
use crate::tenants;
use crate::usage::{page, page_limit, query_rejected};
use crate::AppState;

/// The most charges one run may select. An apply holds its tenants' usage
/// intake for as long as it runs, so a run stays one bounded transaction.
pub const MAX_REPRICE_EVENTS: i64 = 10_000;

/// The most tenants one run may change. An apply locks each tenant it changes
/// for its whole transaction, a few round trips per tenant beside the
/// charges it prices, and then settles each one's marks in a transaction of
/// its own, about nine round trips plus one or more per live quota, holding
/// that tenant's account alone. The cap bounds how long the apply holds every
/// changed tenant's intake, and how long its answer takes.
pub const MAX_REPRICE_TENANTS: usize = 200;

/// The charge statuses a run may select by.
const CHARGE_STATUSES: [&str; 4] = ["charged", "exempt", "nothing_to_bill", "unpriced"];

/// The codes of rows intake refused to attribute to the tenant they are
/// filed under, which a run never bills.
const UNATTRIBUTED: [&str; 3] = ["unknown_api_key", "tenant_mismatch", "undecodable_usage"];

/// Which charges a run selects: those that occurred in `[from, to)`, with
/// `to` no later than the run's instant, narrowed to one model, one tenant
/// (the tenant a charge is filed under), one group and one charge status when
/// given. The group filter matches the coalesced group ([`BILLED_GROUP`]),
/// the one a re-pricing prices the row at, not the column alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepriceFilter {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub model_id: Option<Uuid>,
    pub tenant_id: Option<Uuid>,
    pub group_id: Option<Uuid>,
    pub status: Option<String>,
}

/// Prices the stored usage row `event` again for `payer` (the tenant its
/// charge is filed under, never the row's own `team_id`, and the group it is
/// billed at) at the prices in force at `at`: the row is read as the engine
/// reads it, with its `team_id` and `occurred_at` replaced, so its price
/// instant is `at`. `None` when the engine refuses it, or the row does not
/// decode.
pub fn reprice_one(
    book: &PriceBook,
    event: &Value,
    payer: &Payer,
    at: DateTime<Utc>,
) -> Option<PricedEvent> {
    let mut usage = BillableUsage::deserialize(event).ok()?;
    usage.team_id = payer.tenant_id.clone();
    usage.occurred_at = at;
    price_event(book, &usage, payer, at).ok()
}

/// One tenant's part of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantTotals {
    pub tenant_id: Uuid,
    /// The selected charges filed under the tenant, skipped ones included.
    pub events: i64,
    pub changed: i64,
    pub delta_micro_cny: i64,
}

/// What a run comes to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Totals {
    events: i64,
    changed: i64,
    still_unpriced: i64,
    skipped_unattributed: i64,
    delta_micro_cny: i64,
    /// Ascending tenant id.
    by_tenant: Vec<TenantTotals>,
}

/// One charge a run changes.
struct Change {
    event_id: String,
    tenant_id: Uuid,
    member_id: Option<Uuid>,
    api_key_id: Uuid,
    /// The instant the charge was priced at, or would have been: a quota
    /// counts the correction there.
    price_instant: DateTime<Utc>,
    delta: i64,
    baseline_revision_id: Option<i64>,
    rate_revision_id: Option<i64>,
}

/// One selected charge with its usage row.
#[derive(sqlx::FromRow)]
struct Selected {
    event_id: String,
    tenant_id: String,
    api_key_id: String,
    user_id: String,
    charge_micro_cny: Option<i64>,
    error_code: Option<String>,
    price_instant: DateTime<Utc>,
    event: Value,
    /// The sum of the charge's earlier adjustments.
    adjusted: i64,
    /// The group the charge is billed at: the one recorded when it was priced,
    /// or, when it was never priced, the key's (`BILLED_GROUP`).
    group_id: Option<Uuid>,
}

/// The group a re-pricing prices the charge `pc` at (R11): the group recorded
/// on the charge when it was priced, else the group of its key. A key's group
/// is set at most once and never changes after, so the two agree whenever both
/// are set; the fallback is what lets a row intake stored `no_group` be
/// charged once its key has taken a group. The subquery runs only for a charge
/// with no recorded group.
pub(super) const BILLED_GROUP: &str =
    "COALESCE(pc.group_id, (SELECT k.group_id FROM tenant_keys k \
     WHERE k.id::text = pc.api_key_id))";

/// ` WHERE …` over the charge `pc` for `f`.
fn push_where(sql: &mut QueryBuilder<'_, Postgres>, f: &RepriceFilter) {
    sql.push(" WHERE pc.occurred_at >= ")
        .push_bind(f.from)
        .push(" AND pc.occurred_at < ")
        .push_bind(f.to);
    if let Some(model) = f.model_id {
        sql.push(" AND pc.requested_model_id = ")
            .push_bind(model)
            .push("::text");
    }
    if let Some(tenant) = f.tenant_id {
        sql.push(" AND pc.tenant_id = ")
            .push_bind(tenant)
            .push("::text");
    }
    if let Some(group) = f.group_id {
        sql.push(" AND ")
            .push(BILLED_GROUP)
            .push(" = ")
            .push_bind(group);
    }
    if let Some(status) = &f.status {
        sql.push(" AND pc.status = ").push_bind(status.clone());
    }
}

/// At most `limit` of the charges `f` selects, each with its usage row and
/// the sum of its adjustments. An unpriced charge has no price instant; it
/// gets the one intake would have priced it at, `min(occurred_at,
/// received_at + 5 min)`.
async fn select(
    conn: &mut PgConnection,
    f: &RepriceFilter,
    limit: i64,
) -> Result<Vec<Selected>, DbError> {
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT pc.event_id, pc.tenant_id, pc.api_key_id, pc.user_id, pc.charge_micro_cny, \
         pc.error_code, \
         COALESCE(pc.price_instant, LEAST(pc.occurred_at, ue.received_at + interval '5 minutes')) \
           AS price_instant, \
         ue.event, \
         COALESCE((SELECT SUM(pa.delta_micro_cny)::bigint FROM pricing_adjustments_cny pa \
                   WHERE pa.event_id = pc.event_id), 0) AS adjusted, ",
    );
    sql.push(BILLED_GROUP).push(" AS group_id");
    sql.push(" FROM pricing_charges_cny pc JOIN usage_events ue ON ue.event_id = pc.event_id");
    push_where(&mut sql, f);
    sql.push(" LIMIT ").push_bind(limit);
    Ok(sql.build_query_as().fetch_all(conn).await?)
}

/// How many charges `f` selects.
async fn count(conn: &mut PgConnection, f: &RepriceFilter) -> Result<i64, DbError> {
    let mut sql = QueryBuilder::<Postgres>::new("SELECT count(*) FROM pricing_charges_cny pc");
    push_where(&mut sql, f);
    Ok(sql.build_query_scalar().fetch_one(conn).await?)
}

/// The tenant, key and member a charge is billed under, as it is filed;
/// `None` for a row intake left unattributed.
fn attributed(row: &Selected) -> Option<(Uuid, Uuid, Option<Uuid>)> {
    if row
        .error_code
        .as_deref()
        .is_some_and(|code| UNATTRIBUTED.contains(&code))
    {
        return None;
    }
    let tenant = Uuid::parse_str(&row.tenant_id).ok()?;
    let key = Uuid::parse_str(&row.api_key_id).ok()?;
    Some((tenant, key, Uuid::parse_str(&row.user_id).ok()))
}

fn overflow() -> ApiError {
    ApiError::internal("a re-pricing amount leaves the bigint range")
}

/// Prices `rows` again at `at` and totals what changes.
fn compute(
    book: &PriceBook,
    rows: &[Selected],
    at: DateTime<Utc>,
) -> Result<(Totals, Vec<Change>), ApiError> {
    let mut totals = Totals {
        events: i64::try_from(rows.len()).map_err(|_| overflow())?,
        ..Totals::default()
    };
    let mut by_tenant: BTreeMap<Uuid, TenantTotals> = BTreeMap::new();
    let mut changes = Vec::new();
    for row in rows {
        if let Ok(filed) = Uuid::parse_str(&row.tenant_id) {
            by_tenant
                .entry(filed)
                .or_insert(TenantTotals {
                    tenant_id: filed,
                    events: 0,
                    changed: 0,
                    delta_micro_cny: 0,
                })
                .events += 1;
        }
        let Some((tenant_id, api_key_id, member_id)) = attributed(row) else {
            totals.skipped_unattributed += 1;
            continue;
        };
        // The tenant the charge is filed under and the group it is billed at,
        // both from the charge and its key, never from the row.
        let payer = Payer {
            tenant_id: tenant_id.to_string(),
            group_id: row.group_id.map(|g| g.to_string()),
        };
        let Some(priced) = reprice_one(book, &row.event, &payer, at) else {
            totals.still_unpriced += 1;
            continue;
        };
        let new = if priced.outcome == Outcome::Charged {
            priced.charge.0
        } else {
            0
        };
        let current = row
            .charge_micro_cny
            .unwrap_or(0)
            .checked_add(row.adjusted)
            .ok_or_else(overflow)?;
        let delta = new.checked_sub(current).ok_or_else(overflow)?;
        if delta == 0 {
            continue;
        }
        totals.changed += 1;
        totals.delta_micro_cny = totals
            .delta_micro_cny
            .checked_add(delta)
            .ok_or_else(overflow)?;
        if let Some(t) = by_tenant.get_mut(&tenant_id) {
            t.changed += 1;
            t.delta_micro_cny = t.delta_micro_cny.checked_add(delta).ok_or_else(overflow)?;
        }
        changes.push(Change {
            event_id: row.event_id.clone(),
            tenant_id,
            member_id,
            api_key_id,
            price_instant: row.price_instant,
            delta,
            baseline_revision_id: priced.baseline.map(|b| b.revision_id),
            rate_revision_id: priced.rate.map(|r| r.revision_id),
        });
    }
    totals.by_tenant = by_tenant.into_values().collect();
    Ok((totals, changes))
}

/// One `billing_repricing_runs` row, with its operators' emails.
#[derive(sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    seq: i64,
    status: String,
    filter: Value,
    reason: String,
    pricing_version: i64,
    engine_version: i64,
    events: i64,
    changed: i64,
    still_unpriced: i64,
    skipped_unattributed: i64,
    delta_micro_cny: i64,
    by_tenant: Value,
    created_by: Uuid,
    creator_email: String,
    created_at: DateTime<Utc>,
    applied_by: Option<Uuid>,
    applier_email: Option<String>,
    applied_at: Option<DateTime<Utc>>,
}

const SELECT_RUN: &str = "SELECT r.id, r.seq, r.status, r.filter, r.reason, r.pricing_version, \
     r.engine_version, r.events, r.changed, r.still_unpriced, r.skipped_unattributed, \
     r.delta_micro_cny, r.by_tenant, r.created_by, c.email AS creator_email, r.created_at, \
     r.applied_by, a.email AS applier_email, r.applied_at \
     FROM billing_repricing_runs_cny r JOIN operators c ON c.id = r.created_by \
     LEFT JOIN operators a ON a.id = r.applied_by";

impl RunRow {
    fn filter(&self) -> Result<RepriceFilter, ApiError> {
        serde_json::from_value(self.filter.clone()).map_err(ApiError::internal)
    }

    fn by_tenant(&self) -> Result<Vec<TenantTotals>, ApiError> {
        serde_json::from_value(self.by_tenant.clone()).map_err(ApiError::internal)
    }

    fn totals(&self) -> Result<Totals, ApiError> {
        Ok(Totals {
            events: self.events,
            changed: self.changed,
            still_unpriced: self.still_unpriced,
            skipped_unattributed: self.skipped_unattributed,
            delta_micro_cny: self.delta_micro_cny,
            by_tenant: self.by_tenant()?,
        })
    }

    /// The run as the API shows it (`RepricingRun`), with each tenant's
    /// current name from `names`, its tenants by name.
    fn to_api(&self, names: &HashMap<Uuid, String>) -> Result<Value, ApiError> {
        let mut by_tenant: Vec<(String, Value)> = self
            .by_tenant()?
            .into_iter()
            .map(|t| {
                let name = names.get(&t.tenant_id).cloned().unwrap_or_default();
                let view = json!({
                    "tenant_id": t.tenant_id,
                    "tenant_name": name,
                    "events": t.events,
                    "changed": t.changed,
                    "delta_micro_cny": t.delta_micro_cny,
                });
                (name, view)
            })
            .collect();
        by_tenant.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(json!({
            "id": self.id,
            "status": self.status,
            "filter": self.filter()?,
            "reason": self.reason,
            "events": self.events,
            "changed": self.changed,
            "still_unpriced": self.still_unpriced,
            "skipped_unattributed": self.skipped_unattributed,
            "delta_micro_cny": self.delta_micro_cny,
            "by_tenant": by_tenant.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            "created_by": {"id": self.created_by, "email": self.creator_email},
            "created_at": self.created_at,
            "applied_by": self.applied_by.map(|id| json!({"id": id, "email": self.applier_email})),
            "applied_at": self.applied_at,
        }))
    }
}

/// Run `id`, without a lock; `None` when there is none.
async fn read(conn: &mut PgConnection, id: Uuid) -> Result<Option<RunRow>, DbError> {
    Ok(sqlx::query_as(&format!("{SELECT_RUN} WHERE r.id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await?)
}

/// The current names of the tenants `runs` name.
async fn tenant_names(
    conn: &mut PgConnection,
    runs: &[RunRow],
) -> Result<HashMap<Uuid, String>, ApiError> {
    let mut ids = Vec::new();
    for run in runs {
        ids.extend(run.by_tenant()?.into_iter().map(|t| t.tenant_id));
    }
    ids.sort();
    ids.dedup();
    let names: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM tenants WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(conn)
            .await
            .map_err(DbError::from)?;
    Ok(names.into_iter().collect())
}

/// Run `run` as the API shows it.
async fn view(conn: &mut PgConnection, run: RunRow) -> Result<Value, ApiError> {
    let runs = [run];
    let names = tenant_names(conn, &runs).await?;
    runs[0].to_api(&names)
}

fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

fn stale() -> ApiError {
    ApiError::conflict(
        "stale_preview",
        "预览之后价格或所选用量已变化，请重新预览后再应用",
    )
}

/// `422` at `/tenant_id` when a run changes more than
/// [`MAX_REPRICE_TENANTS`] tenants.
fn within_tenant_cap(changed: usize) -> Result<(), ApiError> {
    if changed > MAX_REPRICE_TENANTS {
        return Err(refused(
            "/tenant_id",
            format!(
                "本次重新定价涉及 {changed} 个租户，超过单次上限 {MAX_REPRICE_TENANTS} 个，\
                 请按租户筛选或缩小时间范围后分批预览"
            ),
        ));
    }
    Ok(())
}

/// How many tenants `totals` changes.
fn tenants_changed(totals: &Totals) -> usize {
    totals.by_tenant.iter().filter(|t| t.changed > 0).count()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRun {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    #[serde(default)]
    model_id: Option<Uuid>,
    #[serde(default)]
    tenant_id: Option<Uuid>,
    #[serde(default)]
    group_id: Option<Uuid>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /admin/billing/repricing` with `{from, to, model_id?, tenant_id?,
/// status?, reason}` → `201 RepricingRun`: the run, priced at the prices in
/// force now and stored as `previewed` with its totals by tenant. Nothing is
/// posted. `422` at `/reason` unless it is 1 to 500 characters, not blank
/// and free of control characters; at `/status` unless it is a charge
/// status; at `/to` unless `to` is after `from` and no later than now, or
/// when the filter selects more than [`MAX_REPRICE_EVENTS`] charges; at
/// `/tenant_id` when the run would change more than [`MAX_REPRICE_TENANTS`]
/// tenants.
pub async fn preview(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<NewRun>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let mut problems = Vec::new();
    let reason = topups::operator_reason(req.reason, "请填写重新定价原因（1–500 个字符）")
        .unwrap_or_else(|d| {
            problems.push(d);
            String::new()
        });
    if let Some(status) = &req.status {
        if !CHARGE_STATUSES.contains(&status.as_str()) {
            problems.push(Detail {
                path: "/status".to_string(),
                message: "状态必须是 charged、exempt、nothing_to_bill 或 unpriced".to_string(),
            });
        }
    }
    if req.to <= req.from {
        problems.push(Detail {
            path: "/to".to_string(),
            message: "结束时间必须晚于开始时间".to_string(),
        });
    }
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let filter = RepriceFilter {
        from: req.from,
        to: req.to,
        model_id: req.model_id,
        tenant_id: req.tenant_id,
        group_id: req.group_id,
        status: req.status,
    };
    let prices = state.prices.clone();
    let view = state
        .db
        .tx(move |conn| {
            async move {
                // One snapshot: the price version stored with the run is the
                // one the book and the rows were read at.
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                let at = budgets::db_now(&mut *conn).await?;
                if filter.to > at {
                    return Err(refused("/to", "结束时间不能晚于当前时间"));
                }
                let version = store::pricing_version(&mut *conn)
                    .await?
                    .ok_or_else(|| ApiError::internal("cp_settings.pricing_version is unreadable"))?;
                let book = prices.current(&mut *conn).await?;
                let rows = select(&mut *conn, &filter, MAX_REPRICE_EVENTS + 1).await?;
                if rows.len() as i64 > MAX_REPRICE_EVENTS {
                    let selected = count(&mut *conn, &filter).await?;
                    return Err(refused(
                        "/to",
                        format!(
                            "所选范围内有 {selected} 条用量，超过单次重新定价的上限 \
                             {MAX_REPRICE_EVENTS} 条，请缩小时间范围或增加筛选条件"
                        ),
                    ));
                }
                let (totals, _) = compute(&book, &rows, at)?;
                within_tenant_cap(tenants_changed(&totals))?;
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO billing_repricing_runs_cny \
                     (id, status, filter, reason, pricing_version, engine_version, events, changed, \
                      still_unpriced, skipped_unattributed, delta_micro_cny, by_tenant, created_by) \
                     VALUES ($1, 'previewed', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                )
                .bind(id)
                .bind(sqlx::types::Json(&filter))
                .bind(&reason)
                .bind(version)
                .bind(i64::from(ENGINE_VERSION))
                .bind(totals.events)
                .bind(totals.changed)
                .bind(totals.still_unpriced)
                .bind(totals.skipped_unattributed)
                .bind(totals.delta_micro_cny)
                .bind(sqlx::types::Json(&totals.by_tenant))
                .bind(auth.id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "billing.repricing.preview",
                        target_kind: "repricing_run",
                        target_id: Some(id.to_string()),
                        tenant_id: filter.tenant_id,
                        detail: json!({
                            "filter": filter,
                            "reason": reason,
                            "events": totals.events,
                            "changed": totals.changed,
                            "delta_micro_cny": totals.delta_micro_cny,
                        }),
                    },
                )
                .await?;
                let run = read(&mut *conn, id).await?.ok_or_else(ApiError::not_found)?;
                view(conn, run).await
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// `pricing_version`, read `FOR SHARE`: no price write, which bumps it,
/// commits until this transaction ends. `None` when it is missing or not a
/// whole number.
async fn hold_pricing_version(conn: &mut PgConnection) -> Result<Option<i64>, DbError> {
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT CASE WHEN jsonb_typeof(value) = 'number' AND value::text ~ '^-?[0-9]{1,18}$' \
                     THEN (value::text)::bigint END \
         FROM cp_settings WHERE key = 'pricing_version' FOR SHARE",
    )
    .fetch_optional(conn)
    .await?
    .flatten())
}

/// `POST /admin/billing/repricing/{id}/apply` with `{}` → `200
/// RepricingRun`: the run's corrections recorded, posted, counted and
/// flagged in one transaction, then each changed tenant's marks settled
/// (see the module docs), and the run `applied`.
/// `404 not_found` for no such run; `409 run_state` when it is no longer
/// `previewed`; `409 stale_preview` when a price, the engine or the selected
/// charges changed since the preview; `422` at `/tenant_id` when the run, as
/// stored or as computed again, changes more than [`MAX_REPRICE_TENANTS`]
/// tenants. Nothing is posted on any of these.
pub async fn apply(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let prices = state.prices.clone();
    let (view, settle) = state
        .db
        .tx(move |conn| {
            async move {
                // The run's filter and totals never change once stored, so
                // its tenants are read before any lock.
                let run = read(&mut *conn, id)
                    .await?
                    .ok_or_else(ApiError::not_found)?;
                let previewed = run.totals()?;
                let filter = run.filter()?;
                let mut changed_tenants: Vec<Uuid> = previewed
                    .by_tenant
                    .iter()
                    .filter(|t| t.changed > 0)
                    .map(|t| t.tenant_id)
                    .collect();
                // A run stored by a release without the cap is refused
                // before it locks a tenant.
                within_tenant_cap(changed_tenants.len())?;
                // The billing locks first: the run posts to these tenants'
                // ledgers and settles their marks. A tenant the computation
                // below changes is among them, or the totals differ.
                tenants::lock_many_billing(&mut *conn, &mut changed_tenants).await?;
                let status: String = sqlx::query_scalar(
                    "SELECT status FROM billing_repricing_runs_cny WHERE id = $1 FOR UPDATE",
                )
                .bind(id)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if status != "previewed" {
                    return Err(ApiError::conflict(
                        "run_state",
                        "该重新定价已应用，不能再次应用",
                    ));
                }
                let version = hold_pricing_version(&mut *conn).await?;
                if version != Some(run.pricing_version)
                    || run.engine_version != i64::from(ENGINE_VERSION)
                {
                    return Err(stale());
                }
                let book = prices.current(&mut *conn).await?;
                let at = budgets::db_now(&mut *conn).await?;
                let rows = select(&mut *conn, &filter, MAX_REPRICE_EVENTS + 1).await?;
                if rows.len() as i64 > MAX_REPRICE_EVENTS {
                    return Err(stale());
                }
                let (totals, changes) = compute(&book, &rows, at)?;
                // Rows that arrived since the preview can widen the run.
                within_tenant_cap(tenants_changed(&totals))?;
                if totals != previewed {
                    return Err(stale());
                }
                let mut postings = Vec::new();
                if !changes.is_empty() {
                    let mut event_ids = Vec::with_capacity(changes.len());
                    let mut deltas = Vec::with_capacity(changes.len());
                    let mut baselines = Vec::with_capacity(changes.len());
                    let mut rates = Vec::with_capacity(changes.len());
                    for c in &changes {
                        event_ids.push(c.event_id.as_str());
                        deltas.push(c.delta);
                        baselines.push(c.baseline_revision_id);
                        rates.push(c.rate_revision_id);
                    }
                    sqlx::query(
                        "INSERT INTO pricing_adjustments_cny \
                         (run_id, event_id, delta_micro_cny, baseline_revision_id, \
                          rate_revision_id, engine_version, reason, created_by) \
                         SELECT $1, a.event_id, a.delta, a.baseline, a.rate, $6, $7, $8 \
                         FROM unnest($2::text[], $3::bigint[], $4::bigint[], $5::bigint[]) \
                           AS a(event_id, delta, baseline, rate)",
                    )
                    .bind(id)
                    .bind(&event_ids)
                    .bind(&deltas)
                    .bind(&baselines)
                    .bind(&rates)
                    .bind(i64::from(ENGINE_VERSION))
                    .bind(&run.reason)
                    .bind(auth.id)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                    // Counted before the posting, so that the quota flags go
                    // into its one update of each account, as at intake.
                    let counted: Vec<ChargedRow> = changes
                        .iter()
                        .map(|c| ChargedRow {
                            tenant_id: c.tenant_id,
                            member_id: c.member_id,
                            api_key_id: c.api_key_id,
                            price_instant: c.price_instant,
                            amount: c.delta,
                        })
                        .collect();
                    let flag =
                        budgets::count_charges(&mut *conn, &counted, CountMode::Adjustment).await?;
                    for t in totals.by_tenant.iter().filter(|t| t.delta_micro_cny != 0) {
                        postings.push(Posting {
                            tenant_id: t.tenant_id,
                            amount_micro_cny: t
                                .delta_micro_cny
                                .checked_neg()
                                .ok_or_else(overflow)?,
                            source: Source::Repricing { run_id: id },
                        });
                    }
                    ledger::post_flagging(&mut *conn, &postings, &flag).await?;
                    // Every changed tenant is flagged, a tenant whose deltas
                    // sum to 0 included, which the posting does not reach.
                    // A second update of an account is safe here: this
                    // transaction holds each tenant's row, so the foreign
                    // key's key-share is its own.
                    sqlx::query(
                        "UPDATE billing_accounts SET marks_dirty = true \
                         WHERE tenant_id = ANY($1)",
                    )
                    .bind(&changed_tenants)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                    marks::notify(&mut *conn).await?;
                }
                sqlx::query(
                    "UPDATE billing_repricing_runs_cny \
                     SET status = 'applied', applied_by = $2, applied_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(auth.id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "billing.repricing.apply",
                        target_kind: "repricing_run",
                        target_id: Some(id.to_string()),
                        tenant_id: filter.tenant_id,
                        detail: json!({
                            "changed": totals.changed,
                            "delta_micro_cny": totals.delta_micro_cny,
                            "tenants_posted": postings.len(),
                        }),
                    },
                )
                .await?;
                let run = read(&mut *conn, id)
                    .await?
                    .ok_or_else(ApiError::not_found)?;
                Ok::<_, ApiError>((view(conn, run).await?, changed_tenants))
            }
            .boxed()
        })
        .await?;
    let ctx = WriteCtx::of(&state);
    for tenant in settle {
        if let Err(e) = marks::settle(&state.db, &ctx, tenant).await {
            tracing::warn!(
                %tenant,
                code = e.code,
                "a re-pricing run's tenant was not settled at once; the settler retries it"
            );
        }
    }
    Ok(Json(view))
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<i64>,
    /// Only runs with a smaller `seq`: the previous page's `next_before`.
    before: Option<i64>,
}

/// `GET /admin/billing/repricing?limit&before` → `{items: [RepricingRun],
/// next_before}`, newest first.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(format!("{SELECT_RUN} WHERE true"));
    if let Some(before) = q.before {
        sql.push(" AND r.seq < ").push_bind(before);
    }
    sql.push(" ORDER BY r.seq DESC LIMIT ").push_bind(limit + 1);
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let rows: Vec<RunRow> = sql
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let (rows, next_before) = page(rows, limit, |r| r.seq);
    let names = tenant_names(&mut conn, &rows).await?;
    let items = rows
        .iter()
        .map(|r| r.to_api(&names))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

/// `GET /admin/billing/repricing/{id}` → `RepricingRun`; `404 not_found`
/// for no such run.
pub async fn get(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let run = read(&mut conn, id).await?.ok_or_else(ApiError::not_found)?;
    Ok(Json(view(&mut conn, run).await?))
}

#[cfg(test)]
mod tests {
    use aisix_pricing::{BaselineRow, DiscountRow, GroupRateRow, PriceRows};
    use chrono::TimeDelta;

    use super::*;

    const GROUP: &str = "0b7e5a52-6a4f-4d3e-9c1b-2f6f4d1d9e10";

    /// A book pricing model `m` at input 1, output 2 and cache read 0.1 USD
    /// per million for every group at `7.2` CNY per USD, with tenant
    /// `discounted` on a 50 % discount.
    fn book(m: &str, discounted: &str, from: DateTime<Utc>) -> PriceBook {
        let card = json!({
            "default": {"base": {"input": "1", "output": "2", "cache_read": "0.1"}}
        });
        let rows = PriceRows {
            baselines: vec![BaselineRow {
                revision_id: 1,
                engine_version: ENGINE_VERSION,
                model_id: m.to_string(),
                effective_from: from,
                card: card.clone(),
            }],
            rates: vec![GroupRateRow {
                revision_id: 1,
                engine_version: ENGINE_VERSION,
                group_id: GROUP.to_string(),
                model_id: None,
                effective_from: from,
                cny_per_usd_e4: Some(72_000),
            }],
            cost_rates: vec![],
            discounts: vec![DiscountRow {
                revision_id: 1,
                engine_version: ENGINE_VERSION,
                tenant_id: discounted.to_string(),
                effective_from: from,
                discount_bps: 5_000,
            }],
        };
        PriceBook::from_rows(rows)
    }

    fn payer(tenant: &str) -> Payer {
        Payer {
            tenant_id: tenant.to_string(),
            group_id: Some(GROUP.to_string()),
        }
    }

    fn event(m: &str, team_id: &str, occurred_at: DateTime<Utc>) -> Value {
        json!({
            "event_id": "e1", "occurred_at": occurred_at, "team_id": team_id,
            "requested_model_id": m, "operation": "chat", "status_code": 200,
            "prompt_tokens": 1000, "cached_prompt_tokens": 800, "completion_tokens": 200,
        })
    }

    #[test]
    fn a_row_is_priced_for_its_filed_tenant_at_the_runs_instant() {
        let now = Utc::now();
        let book = book("m", "filed", now);
        // The row claims another tenant and occurred before any price.
        let row = event("m", "claimed", now - TimeDelta::days(30));
        // 680 µ$ at the baseline, times a rate of 7.2, less 50 %: 2448 µ¥.
        let priced = reprice_one(&book, &row, &payer("filed"), now).expect("priced");
        assert_eq!(priced.charge.0, 2_448);
        assert_eq!(priced.tenant_id, "filed");
        assert_eq!(priced.group_id.as_deref(), Some(GROUP));
        assert_eq!(priced.price_instant, now);
        // Undiscounted the same row is 4896 µ¥.
        assert_eq!(
            reprice_one(&book, &row, &payer("claimed"), now)
                .expect("priced")
                .charge
                .0,
            4_896
        );
        // Before the price starts, nothing is in force.
        assert!(reprice_one(&book, &row, &payer("filed"), now - TimeDelta::seconds(1)).is_none());
        // A row that does not decode is refused.
        assert!(reprice_one(&book, &json!({"event_id": "e2"}), &payer("filed"), now).is_none());
        // A payer whose key names no group is refused, never guessed at.
        let groupless = Payer {
            tenant_id: "filed".to_string(),
            group_id: None,
        };
        assert!(reprice_one(&book, &row, &groupless, now).is_none());
    }
}
