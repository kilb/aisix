//! Quotas: spend caps at three levels, each its own `budgets` document.
//!
//! | Level | Scope, `scope_ref` | `set_by` |
//! |---|---|---|
//! | Organization quota | `team`, the tenant id | `operator` |
//! | Member quota | `member`, the member id | `org_admin` |
//! | Key quota | `api_key`, the key id | `org_admin` or `member` |
//!
//! Who may write each level is the routes' to enforce. No write compares
//! one level with another: a child may exceed its parent, and the gateway
//! refuses a request when any quota that applies to it is exhausted, so the
//! smallest remaining one binds.
//!
//! A quota is periodic, over a UTC day, week or month, or cumulative,
//! counting every charge priced from its creation (`counted_from`) on. A
//! charge counts by its `price_instant`, in the period
//! [`BudgetPeriod::start_of`] gives it, the function the gateway judges a
//! mark's period with. Spend is kept per quota and period in
//! `billing_budget_spend`; a cumulative quota has one row, at
//! `counted_from`.
//!
//! Usage intake counts each charged row ([`count_charges`]) while it holds
//! the batch's accounts, and before it posts: the tenants whose quotas
//! crossed are flagged in the posting's one update of each account, so
//! intake never updates an account twice and never waits on a tenant row.
//! The settler then writes the documents (`marks::project`, which calls
//! [`project`]). The domain writes take the tenant's billing lock and settle
//! inline, so their answer already stands in the gateway's documents.

use std::collections::{BTreeMap, HashMap};

use aisix_core::models::BudgetPeriod;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::marks;
use super::money::usd_string;
use crate::audit::{self, Actor, AuditEntry};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::resources::write::WriteCtx;
use crate::resources::Kind as ResourceKind;
use crate::tenants::{self, TenantRow};

/// The most quotas a tenant's own members may keep at once (`set_by`
/// `org_admin` or `member`); an operator's quotas do not count.
pub const MAX_BUDGETS_PER_TENANT: i64 = 100;

/// What a quota caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    /// Every key of the tenant.
    Team,
    /// Every key bound to the member.
    Member,
    /// One key.
    ApiKey,
}

/// The level that set a quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetBy {
    /// A platform operator: the organization's quota.
    Operator,
    /// The tenant's owner or an admin.
    OrgAdmin,
    /// The key's own member.
    Member,
}

/// How a quota counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Per UTC day, week or month.
    Periodic,
    /// Everything from its creation on.
    Cumulative,
}

impl BudgetScope {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetScope::Team => "team",
            BudgetScope::Member => "member",
            BudgetScope::ApiKey => "api_key",
        }
    }

    fn parse(s: &str) -> Option<BudgetScope> {
        [BudgetScope::Team, BudgetScope::Member, BudgetScope::ApiKey]
            .into_iter()
            .find(|v| v.as_str() == s)
    }
}

impl SetBy {
    pub fn as_str(self) -> &'static str {
        match self {
            SetBy::Operator => "operator",
            SetBy::OrgAdmin => "org_admin",
            SetBy::Member => "member",
        }
    }

    fn parse(s: &str) -> Option<SetBy> {
        [SetBy::Operator, SetBy::OrgAdmin, SetBy::Member]
            .into_iter()
            .find(|v| v.as_str() == s)
    }
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Periodic => "periodic",
            Kind::Cumulative => "cumulative",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        [Kind::Periodic, Kind::Cumulative]
            .into_iter()
            .find(|v| v.as_str() == s)
    }
}

fn parse_period(s: &str) -> Option<BudgetPeriod> {
    [BudgetPeriod::Day, BudgetPeriod::Week, BudgetPeriod::Month]
        .into_iter()
        .find(|v| v.as_str() == s)
}

/// One live `billing_budgets` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub scope: BudgetScope,
    pub scope_ref: Uuid,
    pub set_by: SetBy,
    pub kind: Kind,
    /// Set exactly for a periodic quota.
    pub period: Option<BudgetPeriod>,
    /// Set exactly for a cumulative quota: charges priced from here on
    /// count.
    pub counted_from: Option<DateTime<Utc>>,
    pub limit_micro_usd: i64,
    /// Periodic: the period the projected document marks exhausted.
    pub marked_period_start: Option<DateTime<Utc>>,
    /// Cumulative: whether the projected document is exhausted; `None`
    /// when none has been projected.
    pub marked_exhausted: Option<bool>,
    pub created_by_kind: String,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct RawRow {
    id: Uuid,
    tenant_id: Uuid,
    scope: String,
    scope_ref: Uuid,
    set_by: String,
    kind: String,
    period: Option<String>,
    counted_from: Option<DateTime<Utc>>,
    limit_micro_usd: i64,
    marked_period_start: Option<DateTime<Utc>>,
    marked_exhausted: Option<bool>,
    created_by_kind: String,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, tenant_id, scope, scope_ref, set_by, kind, period, counted_from, \
     limit_micro_usd, marked_period_start, marked_exhausted, created_by_kind, created_by, \
     created_at";

fn decode(raw: RawRow) -> Result<BudgetRow, DbError> {
    let bad = |what: &str, value: &str| {
        DbError::Db(sqlx::Error::Decode(
            format!("quota {}: {what} {value:?}", raw.id).into(),
        ))
    };
    let period = match &raw.period {
        Some(p) => Some(parse_period(p).ok_or_else(|| bad("period", p))?),
        None => None,
    };
    Ok(BudgetRow {
        scope: BudgetScope::parse(&raw.scope).ok_or_else(|| bad("scope", &raw.scope))?,
        set_by: SetBy::parse(&raw.set_by).ok_or_else(|| bad("set_by", &raw.set_by))?,
        kind: Kind::parse(&raw.kind).ok_or_else(|| bad("kind", &raw.kind))?,
        period,
        id: raw.id,
        tenant_id: raw.tenant_id,
        scope_ref: raw.scope_ref,
        counted_from: raw.counted_from,
        limit_micro_usd: raw.limit_micro_usd,
        marked_period_start: raw.marked_period_start,
        marked_exhausted: raw.marked_exhausted,
        created_by_kind: raw.created_by_kind,
        created_by: raw.created_by,
        created_at: raw.created_at,
    })
}

/// A quota to create.
#[derive(Debug, Clone, Copy)]
pub struct NewBudget {
    pub scope: BudgetScope,
    pub scope_ref: Uuid,
    pub set_by: SetBy,
    pub kind: Kind,
    /// Required for a periodic quota, refused for a cumulative one.
    pub period: Option<BudgetPeriod>,
    pub limit_micro_usd: i64,
    pub created_by_kind: &'static str,
    pub created_by: Option<Uuid>,
}

/// Which quotas a count moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountMode {
    /// Usage intake: every quota.
    Usage,
    /// A re-pricing correction: cumulative quotas only, since a periodic
    /// quota's period has usually passed.
    Adjustment,
}

/// One charge to count: `amount` µ$ (negative only for an `Adjustment`),
/// priced at `price_instant`, of key `api_key_id` of tenant `tenant_id`,
/// bound to `member_id`, all as the control plane's key record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargedRow {
    pub tenant_id: Uuid,
    pub member_id: Option<Uuid>,
    pub api_key_id: Uuid,
    pub price_instant: DateTime<Utc>,
    pub amount: i64,
}

/// Whether a charge of `row` counts against `b` at all.
fn applies(b: &BudgetRow, row: &ChargedRow) -> bool {
    b.tenant_id == row.tenant_id
        && match b.scope {
            BudgetScope::Team => b.scope_ref == row.tenant_id,
            BudgetScope::Member => row.member_id == Some(b.scope_ref),
            BudgetScope::ApiKey => b.scope_ref == row.api_key_id,
        }
}

/// The spend row of `b` a charge priced at `at` counts in: its period's
/// for a periodic quota; `counted_from` for a cumulative one, unless the
/// charge was priced before it, which counts nowhere.
fn counts_in(b: &BudgetRow, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match b.period {
        Some(period) => Some(period.start_of(at)),
        None => b.counted_from.filter(|from| at >= *from),
    }
}

/// The spend row holding `b`'s current spend at `now`.
fn current(b: &BudgetRow, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match b.period {
        Some(period) => Some(period.start_of(now)),
        None => b.counted_from,
    }
}

/// The database's clock: within a transaction, its start.
async fn db_now(conn: &mut PgConnection) -> Result<DateTime<Utc>, DbError> {
    Ok(sqlx::query_scalar("SELECT now()").fetch_one(conn).await?)
}

/// The live quotas of `tenant_ids`, oldest first.
async fn live_rows(
    conn: &mut PgConnection,
    tenant_ids: &[Uuid],
) -> Result<Vec<BudgetRow>, DbError> {
    let raws: Vec<RawRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM billing_budgets \
         WHERE tenant_id = ANY($1) AND deleted_at IS NULL ORDER BY created_at, id"
    ))
    .bind(tenant_ids)
    .fetch_all(conn)
    .await?;
    raws.into_iter().map(decode).collect()
}

/// The current spend at `now` of each of `rows` that has spent anything.
async fn spends(
    conn: &mut PgConnection,
    rows: &[BudgetRow],
    now: DateTime<Utc>,
) -> Result<HashMap<Uuid, i64>, DbError> {
    let (ids, starts): (Vec<Uuid>, Vec<DateTime<Utc>>) = rows
        .iter()
        .filter_map(|b| current(b, now).map(|start| (b.id, start)))
        .unzip();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let found: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT s.budget_id, s.spent_micro_usd FROM billing_budget_spend s \
         JOIN unnest($1::uuid[], $2::timestamptz[]) AS k(budget_id, period_start) \
           ON s.budget_id = k.budget_id AND s.period_start = k.period_start",
    )
    .bind(ids)
    .bind(starts)
    .fetch_all(conn)
    .await?;
    Ok(found.into_iter().collect())
}

/// Live quota `id` of tenant `tenant_id`; `None` for a deleted one, another
/// tenant's, or none.
pub async fn get(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<Option<BudgetRow>, DbError> {
    let raw: Option<RawRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM billing_budgets \
         WHERE id = $1 AND tenant_id = $2 AND deleted_at IS NULL"
    ))
    .bind(id)
    .bind(tenant_id)
    .fetch_optional(conn)
    .await?;
    raw.map(decode).transpose()
}

/// Every live quota of tenant `tenant_id`, oldest first, each with its
/// current spend: the current UTC period's for a periodic quota, since
/// `counted_from` for a cumulative one.
pub async fn list(
    conn: &mut PgConnection,
    tenant_id: Uuid,
) -> Result<Vec<(BudgetRow, i64)>, DbError> {
    let rows = live_rows(&mut *conn, &[tenant_id]).await?;
    let now = db_now(&mut *conn).await?;
    let spends = spends(conn, &rows, now).await?;
    Ok(rows
        .into_iter()
        .map(|b| {
            let spent = spends.get(&b.id).copied().unwrap_or(0);
            (b, spent)
        })
        .collect())
}

/// Quota `b`'s current spend, as [`list`] gives it.
pub async fn spent(conn: &mut PgConnection, b: &BudgetRow) -> Result<i64, DbError> {
    let now = db_now(&mut *conn).await?;
    Ok(spends(conn, std::slice::from_ref(b), now)
        .await?
        .get(&b.id)
        .copied()
        .unwrap_or(0))
}

/// Quota `row` as the API shows it (`Budget`): its target with
/// `target_label`, and its spend `spent`, which is the current UTC
/// period's for a periodic quota, whose `period_start` is that period's.
pub fn to_api(row: &BudgetRow, spent: i64, target_label: &str, target_live: bool) -> Value {
    json!({
        "id": row.id,
        "scope": row.scope,
        "target": {"id": row.scope_ref, "label": target_label},
        "target_live": target_live,
        "set_by": row.set_by,
        "kind": row.kind,
        "period": row.period.map(BudgetPeriod::as_str),
        "limit_micro_usd": row.limit_micro_usd,
        "spent_micro_usd": spent,
        "remaining_micro_usd": row.limit_micro_usd.saturating_sub(spent),
        "period_start": row.period.map(|p| p.start_of(Utc::now())),
        "counted_from": row.counted_from,
        "exhausted": spent >= row.limit_micro_usd,
        "created_at": row.created_at,
    })
}

/// `micro` as the dollars a person set, with two decimals: every limit
/// typed is whole cents. One that is not is written exactly, never
/// rounded.
fn dollars(micro: i64) -> String {
    let exact = usd_string(micro);
    if micro % 10_000 == 0 {
        exact[..exact.len() - 4].to_string()
    } else {
        exact
    }
}

/// Quota `b`'s document, with `spent` its current spend at `now`.
///
/// It is `{scope, scope_ref, period}`, with `period` the quota's own for a
/// periodic quota and the placeholder `month` for a cumulative one: the
/// gateway requires one, and a value it does not know would make it skip
/// the document. Once `spent` reaches the limit it carries an `exhausted`
/// mark with the limit and the spend. A periodic mark names the period it
/// holds in (`period_start`), so the gateway lifts it at the next period on
/// its own; a cumulative one names none and holds until the control plane
/// lifts it, with `"period": "total"` in its reason.
pub fn budget_doc(b: &BudgetRow, spent: i64, now: DateTime<Utc>) -> Value {
    let period = b.period.map_or("month", BudgetPeriod::as_str);
    let mut doc = json!({"scope": b.scope.as_str(), "scope_ref": b.scope_ref, "period": period});
    if spent < b.limit_micro_usd {
        return doc;
    }
    let limit = dollars(b.limit_micro_usd);
    let (message, reason_period) = match b.period {
        Some(p) => {
            let every = match p {
                BudgetPeriod::Day => "daily",
                BudgetPeriod::Week => "weekly",
                BudgetPeriod::Month => "monthly",
            };
            (
                format!("Budget exceeded: the {every} limit of {limit} USD is reached."),
                p.as_str(),
            )
        }
        None => (
            format!("Quota exhausted: the allocation of {limit} USD is used up."),
            "total",
        ),
    };
    let mut exhausted = json!({"reason": {
        "message": message,
        "scope": b.scope.as_str(),
        "scope_ref": b.scope_ref,
        "limit_usd": usd_string(b.limit_micro_usd),
        "spent_usd": usd_string(spent),
        "remaining_usd": usd_string(b.limit_micro_usd.saturating_sub(spent)),
        "period": reason_period,
    }});
    if let Some(p) = b.period {
        exhausted["period_start"] =
            json!(p.start_of(now).to_rfc3339_opts(SecondsFormat::Secs, true));
    }
    doc["exhausted"] = exhausted;
    doc
}

/// Counts `rows` against every live quota each falls under (its tenant's,
/// its member's and its key's, from every setter), in one upsert of the
/// spend rows, ordered by quota and period. `Adjustment` counts against
/// cumulative quotas only.
///
/// Returns the tenants whose quotas the count crossed, ascending: a
/// periodic quota that reached its limit in the current period and whose
/// document does not mark that period yet, or a cumulative quota whose
/// reached-or-not differs from what its document marks. The caller holds
/// the accounts of `rows`' tenants, so no quota write runs alongside, and
/// flags those tenants in its one update of each account
/// (`ledger::post_flagging`), then notifies the settler: a second update of
/// an account here would have the foreign key's check key-share its tenant
/// row, which intake must never wait on.
pub async fn count_charges(
    conn: &mut PgConnection,
    rows: &[ChargedRow],
    mode: CountMode,
) -> Result<Vec<Uuid>, DbError> {
    let mut tenant_ids: Vec<Uuid> = rows.iter().map(|row| row.tenant_id).collect();
    tenant_ids.sort();
    tenant_ids.dedup();
    if tenant_ids.is_empty() {
        return Ok(Vec::new());
    }
    let quotas: Vec<BudgetRow> = live_rows(&mut *conn, &tenant_ids)
        .await?
        .into_iter()
        .filter(|b| mode == CountMode::Usage || b.period.is_none())
        .collect();
    let mut sums: BTreeMap<(Uuid, DateTime<Utc>), i64> = BTreeMap::new();
    for row in rows {
        for b in quotas.iter().filter(|b| applies(b, row)) {
            if let Some(start) = counts_in(b, row.price_instant) {
                let sum = sums.entry((b.id, start)).or_default();
                *sum = sum.saturating_add(row.amount);
            }
        }
    }
    if sums.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::with_capacity(sums.len());
    let mut starts = Vec::with_capacity(sums.len());
    let mut amounts = Vec::with_capacity(sums.len());
    for ((id, start), amount) in sums {
        ids.push(id);
        starts.push(start);
        amounts.push(amount);
    }
    let totals: Vec<(Uuid, DateTime<Utc>, i64, DateTime<Utc>)> = sqlx::query_as(
        "INSERT INTO billing_budget_spend (budget_id, period_start, spent_micro_usd) \
         SELECT k.budget_id, k.period_start, k.amount \
         FROM unnest($1::uuid[], $2::timestamptz[], $3::bigint[]) \
           WITH ORDINALITY AS k(budget_id, period_start, amount, n) \
         ORDER BY k.n \
         ON CONFLICT (budget_id, period_start) DO UPDATE \
           SET spent_micro_usd = billing_budget_spend.spent_micro_usd + EXCLUDED.spent_micro_usd \
         RETURNING budget_id, period_start, spent_micro_usd, now()",
    )
    .bind(ids)
    .bind(starts)
    .bind(amounts)
    .fetch_all(conn)
    .await?;
    let mut flag: Vec<Uuid> = totals
        .into_iter()
        .filter_map(|(id, start, spent, now)| {
            let b = quotas.iter().find(|b| b.id == id)?;
            let reached = spent >= b.limit_micro_usd;
            let crossed = match b.period {
                Some(period) => {
                    reached && start == period.start_of(now) && b.marked_period_start != Some(start)
                }
                None => reached != b.marked_exhausted.unwrap_or(false),
            };
            crossed.then_some(b.tenant_id)
        })
        .collect();
    flag.sort();
    flag.dedup();
    Ok(flag)
}

/// Writes the document of every live quota of tenant `tenant_id` as its
/// spend stands now, owned by `marks::owner`, and records what each marks.
/// Called from `marks::project`, under the tenant's billing lock.
pub(crate) async fn project(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant_id: Uuid,
) -> Result<(), ApiError> {
    let rows = live_rows(&mut *conn, &[tenant_id]).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let now = db_now(&mut *conn).await?;
    let spends = spends(&mut *conn, &rows, now).await?;
    let owner = marks::owner(tenant_id);
    let mut ids = Vec::with_capacity(rows.len());
    let mut period_starts = Vec::with_capacity(rows.len());
    let mut exhausted = Vec::with_capacity(rows.len());
    for b in &rows {
        let spent = spends.get(&b.id).copied().unwrap_or(0);
        let doc = budget_doc(b, spent, now);
        tenants::put(&mut *conn, ctx, ResourceKind::Budgets, b.id, &doc, &owner).await?;
        let reached = spent >= b.limit_micro_usd;
        ids.push(b.id);
        match b.period {
            Some(period) => {
                period_starts.push(reached.then(|| period.start_of(now)));
                exhausted.push(None);
            }
            None => {
                period_starts.push(None);
                exhausted.push(Some(reached));
            }
        }
    }
    sqlx::query(
        "UPDATE billing_budgets AS b \
         SET marked_period_start = m.period_start, marked_exhausted = m.exhausted \
         FROM unnest($1::uuid[], $2::timestamptz[], $3::boolean[]) AS m(id, period_start, exhausted) \
         WHERE b.id = m.id",
    )
    .bind(ids)
    .bind(period_starts)
    .bind(exhausted)
    .execute(conn)
    .await
    .map_err(DbError::from)?;
    Ok(())
}

fn invalid(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.to_string(),
    }])
}

fn check_limit(limit_micro_usd: i64) -> Result<(), ApiError> {
    if limit_micro_usd <= 0 {
        return Err(invalid("/limit_usd", "额度必须大于 0"));
    }
    Ok(())
}

/// Whether `scope_ref` names a target of `tenant`'s own for `scope`: the
/// tenant itself, one of its live members or one of its live keys. The
/// internal tenant has none: it is never marked.
async fn target_is_own(
    conn: &mut PgConnection,
    tenant: &TenantRow,
    scope: BudgetScope,
    scope_ref: Uuid,
) -> Result<bool, DbError> {
    if tenant.is_internal {
        return Ok(false);
    }
    let sql = match scope {
        BudgetScope::Team => return Ok(scope_ref == tenant.id),
        BudgetScope::Member => {
            "SELECT EXISTS (SELECT 1 FROM members \
             WHERE id = $1 AND tenant_id = $2 AND removed_at IS NULL)"
        }
        BudgetScope::ApiKey => {
            "SELECT EXISTS (SELECT 1 FROM tenant_keys \
             WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL)"
        }
    };
    Ok(sqlx::query_scalar(sql)
        .bind(scope_ref)
        .bind(tenant.id)
        .fetch_one(conn)
        .await?)
}

/// The audit row of a quota write.
async fn record(
    conn: &mut PgConnection,
    actor: Actor<'_>,
    action: &'static str,
    b: &BudgetRow,
) -> Result<(), DbError> {
    audit::record(
        conn,
        AuditEntry {
            actor,
            action,
            target_kind: "budget",
            target_id: Some(b.id.to_string()),
            tenant_id: Some(b.tenant_id),
            detail: json!({
                "scope": b.scope,
                "set_by": b.set_by,
                "kind": b.kind,
                "period": b.period.map(BudgetPeriod::as_str),
                "limit_micro_usd": b.limit_micro_usd,
            }),
        },
    )
    .await
}

/// Creates quota `new` on `tenant`, settles its marks and records
/// `budget.create`.
///
/// Takes the tenant's billing lock first (a no-op when the caller holds
/// it), so its quotas and its account do not move underneath. Refused with
/// `422` at `/period` for a cumulative quota with a period or a periodic
/// one without, at `/limit_usd` for a limit below 1 µ$, and at `/scope_ref`
/// for a target that is not the tenant's own, exactly as for one that does
/// not exist; `409 budget_limit` once the tenant's own quotas reach
/// [`MAX_BUDGETS_PER_TENANT`], and `409 budget_exists` when the target has a
/// live quota of the same kind and period from the same setter.
///
/// A periodic quota starts with what its current period has already been
/// charged; a cumulative one counts from now (`counted_from`), and starts
/// with what has been charged at a price instant from then on, which is
/// nothing unless a charge priced at the very instant was recorded first.
pub async fn create(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant: &TenantRow,
    new: NewBudget,
    actor: Actor<'_>,
) -> Result<BudgetRow, ApiError> {
    let (tenant, _) = tenants::lock_billing(&mut *conn, tenant.id).await?;
    match (new.kind, new.period) {
        (Kind::Periodic, None) => return Err(invalid("/period", "周期额度需要指定周期")),
        (Kind::Cumulative, Some(_)) => return Err(invalid("/period", "累计额度不设周期")),
        _ => {}
    }
    check_limit(new.limit_micro_usd)?;
    if !target_is_own(&mut *conn, &tenant, new.scope, new.scope_ref).await? {
        return Err(invalid("/scope_ref", "额度对象不存在"));
    }
    if new.set_by != SetBy::Operator {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_budgets \
             WHERE tenant_id = $1 AND deleted_at IS NULL AND set_by <> 'operator'",
        )
        .bind(tenant.id)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
        if count >= MAX_BUDGETS_PER_TENANT {
            return Err(ApiError::conflict(
                "budget_limit",
                format!("额度数量已达上限（{MAX_BUDGETS_PER_TENANT} 个）"),
            ));
        }
    }
    let raw: RawRow = sqlx::query_as(&format!(
        "INSERT INTO billing_budgets \
         (id, tenant_id, scope, scope_ref, set_by, kind, period, counted_from, limit_micro_usd, \
          created_by_kind, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, CASE WHEN $6 = 'cumulative' THEN now() END, $8, \
          $9, $10) \
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(tenant.id)
    .bind(new.scope.as_str())
    .bind(new.scope_ref)
    .bind(new.set_by.as_str())
    .bind(new.kind.as_str())
    .bind(new.period.map(BudgetPeriod::as_str))
    .bind(new.limit_micro_usd)
    .bind(new.created_by_kind)
    .bind(new.created_by)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| match DbError::from(e) {
        DbError::UniqueViolation { constraint }
            if constraint == "billing_budgets_one_per_setter" =>
        {
            ApiError::conflict("budget_exists", "该对象已有同一设置方的同类额度")
        }
        e => e.into(),
    })?;
    let row = decode(raw)?;
    let now = db_now(&mut *conn).await?;
    let (from, until) = match (row.period, row.counted_from) {
        (Some(period), _) => {
            let start = period.start_of(now);
            (start, Some(period.next_start(start)))
        }
        (None, Some(from)) => (from, None),
        (None, None) => {
            return Err(ApiError::internal(
                "a cumulative quota without counted_from",
            ))
        }
    };
    // The target's column, compared as text as `pricing_charges` stores it.
    // A charge's price instant never follows its `occurred_at`, so the
    // bound on `occurred_at` changes nothing but lets the tenant's index
    // serve the scan.
    let column = match row.scope {
        BudgetScope::Team => "tenant_id",
        BudgetScope::Member => "user_id",
        BudgetScope::ApiKey => "api_key_id",
    };
    let so_far: i64 = sqlx::query_scalar(&format!(
        "SELECT COALESCE(SUM(charge_micro_usd), 0)::bigint FROM pricing_charges \
         WHERE tenant_id = $1::text AND {column} = $2::text AND status = 'charged' \
           AND occurred_at >= $3 AND price_instant >= $3 \
           AND ($4::timestamptz IS NULL OR price_instant < $4)"
    ))
    .bind(tenant.id)
    .bind(row.scope_ref)
    .bind(from)
    .bind(until)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    sqlx::query(
        "INSERT INTO billing_budget_spend (budget_id, period_start, spent_micro_usd) \
         VALUES ($1, $2, $3)",
    )
    .bind(row.id)
    .bind(from)
    .bind(so_far)
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;
    marks::settle_locked(&mut *conn, ctx, tenant.id).await?;
    record(conn, actor, "budget.create", &row).await?;
    Ok(row)
}

/// Sets live quota `id` of `tenant` to `limit_micro_usd`, settles its marks
/// and records `budget.update`. Takes the tenant's billing lock first, like
/// [`create`]; `404` for a quota that is not the tenant's or is deleted,
/// `422 /limit_usd` for a limit below 1 µ$.
pub async fn set_limit(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant: &TenantRow,
    id: Uuid,
    limit_micro_usd: i64,
    actor: Actor<'_>,
) -> Result<BudgetRow, ApiError> {
    let (tenant, _) = tenants::lock_billing(&mut *conn, tenant.id).await?;
    check_limit(limit_micro_usd)?;
    let raw: Option<RawRow> = sqlx::query_as(&format!(
        "UPDATE billing_budgets SET limit_micro_usd = $3 \
         WHERE id = $1 AND tenant_id = $2 AND deleted_at IS NULL RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(tenant.id)
    .bind(limit_micro_usd)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let row = decode(raw.ok_or_else(ApiError::not_found)?)?;
    marks::settle_locked(&mut *conn, ctx, tenant.id).await?;
    record(conn, actor, "budget.update", &row).await?;
    Ok(row)
}

/// Deletes live quota `id` of `tenant`: sets `deleted_at`, retires its
/// document, settles the tenant's marks and records `budget.delete`. Takes
/// the tenant's billing lock first, like [`create`]; `404` for a quota that
/// is not the tenant's or is deleted.
pub async fn delete(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant: &TenantRow,
    id: Uuid,
    actor: Actor<'_>,
) -> Result<(), ApiError> {
    let (tenant, _) = tenants::lock_billing(&mut *conn, tenant.id).await?;
    let raw: Option<RawRow> = sqlx::query_as(&format!(
        "UPDATE billing_budgets SET deleted_at = now() \
         WHERE id = $1 AND tenant_id = $2 AND deleted_at IS NULL RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(tenant.id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let row = decode(raw.ok_or_else(ApiError::not_found)?)?;
    tenants::retire(
        &mut *conn,
        ctx,
        ResourceKind::Budgets,
        row.id,
        &marks::owner(tenant.id),
    )
    .await?;
    marks::settle_locked(&mut *conn, ctx, tenant.id).await?;
    record(conn, actor, "budget.delete", &row).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_reads_in_cents_and_is_never_rounded() {
        assert_eq!(dollars(10_000), "0.01");
        assert_eq!(dollars(12_340_000), "12.34");
        assert_eq!(dollars(1_000_000), "1.00");
        assert_eq!(dollars(10_001), "0.010001");
    }
}
