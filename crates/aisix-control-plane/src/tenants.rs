//! Tenants: the platform's customers, as operators manage them.
//!
//! The gateways see a tenant as a team, `teams/<tenant id>`, named after it,
//! once it is approved: a pending tenant gives them nothing (see `steps`).
//! A tenant reaches the global model catalog through its **model scope**: all
//! public models ([`ModelScope::All`], what every tenant starts on) or the
//! list it holds ([`ModelScope::Selected`], `allowed_model_ids`). "All" is
//! computed at read time ([`scope_model_ids`]) rather than stored as one
//! row per tenant per model, so a public model created later reaches every
//! all-scope tenant without an edit, and retiring one needs no withdrawal.
//! A key's own selection narrows within that scope, never past it. Its
//! default protection (`limits`) is projected,
//! while it is active, as one conditional rate-limit policy over the team,
//! written under an id minted once (`limits_policy_id`). Both documents are
//! the tenant's own (`managed_by = "tenant:<id>"`), so only the code here
//! changes them.
//!
//! Every tenant-scoped read-then-write starts by locking the tenant rows
//! ([`lock`], [`lock_many`]) and only then reaches `write_resource`, which
//! takes the resources lock: the lock order everywhere. A write that moves
//! money or decides a mark (the ledger, a credit limit, a quota, a balance
//! document) takes [`lock_billing`] instead, which adds the tenant's billing
//! account between the two. Every other edit leaves the account alone, so it
//! never holds up the tenant's usage intake, which locks accounts only.
//! A write that names provider groups takes their rows `FOR SHARE` straight
//! after the tenant's and before the resources lock
//! ([`crate::groups::lock_named`]), so it and a retire of the same group
//! serialise on the group's own row.
//!
//! A lookup by id answers `404 not_found` for an id that names no tenant,
//! the same answer whatever the reason, so the tenant routes built on these
//! functions can answer another tenant's id exactly as a random one.

use std::collections::BTreeMap;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::billing::{ledger, marks};
use crate::compat;
use crate::db::{Db, DbError};
use crate::error::{ApiError, BootError, Detail};
use crate::gateways::stale_window_secs;
use crate::http::ApiJson;
use crate::resources::api::if_match;
use crate::resources::store::{self, ResourceRow};
use crate::resources::write::{
    write_resource, ResourceWrite, WriteCtx, WriteOp, RESOURCES_LOCK_KEY,
};
use crate::resources::Kind;
use crate::rollout::{self, Fleet, ResourceRollout, RolloutInput};
use crate::settings::{self, EnvSettings, TenantDefaults, BOOT_LOCK_KEY};
use crate::AppState;
use crate::{keys, members};

/// The platform's own tenant, which operator keys belong to.
pub const INTERNAL_TENANT_NAME: &str = "平台内部";

/// The longest tenant name accepted.
const MAX_NAME_CHARS: usize = 128;

/// The longest provider group name accepted.
const MAX_GROUP_CHARS: usize = 64;

const TARGET_KIND: &str = "tenant";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantStatus {
    /// Signed up, waiting for an operator's approval.
    Pending,
    Active,
    Suspended,
}

impl TenantStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TenantStatus::Pending => "pending",
            TenantStatus::Active => "active",
            TenantStatus::Suspended => "suspended",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<TenantStatus> {
        [
            TenantStatus::Pending,
            TenantStatus::Active,
            TenantStatus::Suspended,
        ]
        .into_iter()
        .find(|status| status.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BillingMode {
    #[default]
    Prepaid,
    Postpaid,
}

impl BillingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BillingMode::Prepaid => "prepaid",
            BillingMode::Postpaid => "postpaid",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<BillingMode> {
        [BillingMode::Prepaid, BillingMode::Postpaid]
            .into_iter()
            .find(|mode| mode.as_str() == s)
    }
}

/// What a tenant may reach: every public model, or the list it holds.
///
/// [`ModelScope::All`] is the default, for a new tenant of either origin and
/// for a stored row written before the scope existed. It resolves to the
/// live public models — the tenant-facing names an operator creates in the
/// public model catalogue and assigns to upstream accounts — and is
/// **computed** whenever it is read ([`scope_model_ids`], and beside the row
/// in [`TenantRow::effective_model_ids`]), never written into a row. A hand-made model in the model catalogue is reachable only
/// through [`ModelScope::Selected`], which keeps the stored
/// `allowed_model_ids` and its existing meaning: the listed models, and an
/// empty list grants none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelScope {
    #[default]
    All,
    Selected,
}

impl ModelScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelScope::All => "all",
            ModelScope::Selected => "selected",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<ModelScope> {
        [ModelScope::All, ModelScope::Selected]
            .into_iter()
            .find(|scope| scope.as_str() == s)
    }
}

/// A tenant's default protection. The first three bound the whole tenant
/// (every key together); the `key_*_max` fields cap what one key of the
/// tenant may be given. Unset means unbounded.
///
/// Read leniently, since an instance of an older release may read a row a
/// newer one wrote; request bodies are checked for unknown fields by
/// [`limits_from`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_concurrency_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_rpm_max: Option<u32>,
}

const LIMIT_FIELDS: [&str; 5] = [
    "concurrency",
    "rpm",
    "tpm",
    "key_concurrency_max",
    "key_rpm_max",
];

impl TenantLimits {
    fn by_name(&self) -> [(&'static str, Option<u32>); 5] {
        [
            ("concurrency", self.concurrency),
            ("rpm", self.rpm),
            ("tpm", self.tpm),
            ("key_concurrency_max", self.key_concurrency_max),
            ("key_rpm_max", self.key_rpm_max),
        ]
    }

    /// A detail at `<at>/<field>` for each limit of zero, which would admit
    /// nothing at all.
    fn problems(&self, at: &str) -> Vec<Detail> {
        self.by_name()
            .into_iter()
            .filter(|(_, value)| *value == Some(0))
            .map(|(name, _)| {
                detail(
                    &format!("{at}/{name}"),
                    "限制必须大于 0；不限制请省略该字段",
                )
            })
            .collect()
    }
}

/// One row of `tenants`.
#[derive(Debug, Clone, PartialEq)]
pub struct TenantRow {
    pub id: Uuid,
    pub name: String,
    pub status: TenantStatus,
    pub is_internal: bool,
    pub billing_mode: BillingMode,
    pub model_scope: ModelScope,
    /// The stored selection, whose meaning `model_scope` decides.
    pub allowed_model_ids: Vec<Uuid>,
    /// What the tenant reaches, resolved when the row was read: every live
    /// public model under [`ModelScope::All`], the stored list under
    /// [`ModelScope::Selected`]. Readable by every consumer that has the
    /// row, so no model-keyed mechanism has to know which scope it is
    /// looking at. Never the place a decision is made about the *stored*
    /// list: `allowed_model_ids` stays what an operator last selected.
    pub effective_model_ids: Vec<Uuid>,
    pub allowed_provider_groups: Vec<String>,
    pub limits: TenantLimits,
    pub limits_policy_id: Option<Uuid>,
    pub version: i64,
    pub created_at: DateTime<Utc>,
    pub approved_at: Option<DateTime<Utc>>,
}

type RawTenant = (
    Uuid,
    String,
    String,
    bool,
    String,
    String,
    Vec<Uuid>,
    Vec<String>,
    Value,
    Option<Uuid>,
    i64,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

const COLUMNS: &str = "id, name, status, is_internal, billing_mode, model_scope, \
     allowed_model_ids, allowed_provider_groups, limits, limits_policy_id, version, created_at, \
     approved_at";

impl TenantRow {
    /// The row with the scope it stored, and **not** yet with what that
    /// scope resolves to ([`TenantRow::effective_model_ids`, filled by
    /// [`load`] and the lock helpers, the only readers): a row built by hand
    /// has no catalogue read behind it.
    fn from_raw(raw: RawTenant) -> Result<TenantRow, ApiError> {
        let (
            id,
            name,
            status,
            is_internal,
            billing_mode,
            model_scope,
            allowed_model_ids,
            allowed_provider_groups,
            limits,
            limits_policy_id,
            version,
            created_at,
            approved_at,
        ) = raw;
        Ok(TenantRow {
            id,
            name,
            status: TenantStatus::parse(&status)
                .ok_or_else(|| ApiError::internal(format!("tenant {id}: status {status}")))?,
            is_internal,
            billing_mode: BillingMode::parse(&billing_mode).ok_or_else(|| {
                ApiError::internal(format!("tenant {id}: billing mode {billing_mode}"))
            })?,
            model_scope: ModelScope::parse(&model_scope).ok_or_else(|| {
                ApiError::internal(format!("tenant {id}: model scope {model_scope}"))
            })?,
            allowed_model_ids,
            effective_model_ids: Vec::new(),
            allowed_provider_groups,
            limits: serde_json::from_value(limits).map_err(ApiError::internal)?,
            limits_policy_id,
            version,
            created_at,
            approved_at,
        })
    }
}

fn detail(path: &str, message: &str) -> Detail {
    Detail {
        path: path.to_string(),
        message: message.to_string(),
    }
}

/// Who owns the documents derived from tenant `id`.
pub fn owner(id: Uuid) -> String {
    format!("tenant:{id}")
}

/// The tenant's `teams` document.
pub fn team_doc(name: &str) -> Value {
    json!({ "name": name })
}

/// The rate-limit policy that holds tenant `id` to the tenant-wide part of
/// `limits`, one bucket over every key of its team; `None` when that part
/// sets nothing.
pub fn limits_policy_doc(id: Uuid, limits: &TenantLimits) -> Option<Value> {
    let caps: Map<String, Value> = [
        ("concurrency", limits.concurrency),
        ("rpm", limits.rpm),
        ("tpm", limits.tpm),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|v| (name.to_string(), json!(v))))
    .collect();
    if caps.is_empty() {
        return None;
    }
    Some(json!({
        "name": format!("tenant-{id}-limits"),
        "conditions": [{"dimension": "team", "operator": "in", "value": [id.to_string()]}],
        "limits": caps,
    }))
}

/// [`TenantRow`] with the scope it resolves to (`effective_model_ids`)
/// filled in: the one place a read becomes a row every consumer can use.
///
/// Under [`ModelScope::All`] that is the live public models as they stand
/// now. It is read here, beside the row, rather than per key document, so
/// every key of one tenant in one transaction resolves the same set. A
/// retire cannot slip in between: a retire takes the public model's row and
/// then the resources lock, and the paths that write a key document take
/// the tenant's row first, so either the retire commits before this row is
/// read and the set already lacks the model, or it waits and finds the key
/// documents already narrowed.
async fn load(conn: &mut PgConnection, raw: RawTenant) -> Result<TenantRow, ApiError> {
    let mut tenant = TenantRow::from_raw(raw)?;
    tenant.effective_model_ids = scope_model_ids(&mut *conn, tenant.id)
        .await
        .map_err(ApiError::from)?;
    Ok(tenant)
}

/// Tenant `id`, locked for the rest of the transaction; `404 not_found`
/// when there is none. Locking a row the transaction already holds is a
/// no-op. Its billing account is not locked: see [`lock_billing`].
pub async fn lock(conn: &mut PgConnection, id: Uuid) -> Result<TenantRow, ApiError> {
    let raw: Option<RawTenant> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM tenants WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    load(conn, raw.ok_or_else(ApiError::not_found)?).await
}

/// Tenant `id` and then its billing account ([`ledger::lock_account`]),
/// both locked for the rest of the transaction, in the lock order; `404
/// not_found` when there is no such tenant. Every path that posts to the
/// ledger, changes a credit limit or a quota, or projects a balance or quota
/// mark takes this rather than [`lock`]: the account lock is what keeps it
/// from racing the tenant's usage intake on the balance, which it holds off
/// for as long as the transaction runs.
pub async fn lock_billing(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<(TenantRow, ledger::Account), ApiError> {
    let tenant = lock(&mut *conn, id).await?;
    let account = ledger::lock_account(conn, id).await?;
    Ok((tenant, account))
}

/// Tenant `id` as it stands, without a lock; `404 not_found` when there is
/// none.
pub(crate) async fn find(conn: &mut PgConnection, id: Uuid) -> Result<TenantRow, ApiError> {
    let raw: Option<RawTenant> =
        sqlx::query_as(&format!("SELECT {COLUMNS} FROM tenants WHERE id = $1"))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(DbError::from)?;
    load(conn, raw.ok_or_else(ApiError::not_found)?).await
}

/// Every tenant as it stands, in id order, each with the scope it resolves
/// to, taking no lock: what an operator view counts reach from
/// (`channels::view`). Nothing written may be decided from it.
pub(crate) async fn all(conn: &mut PgConnection) -> Result<Vec<TenantRow>, ApiError> {
    let raws: Vec<RawTenant> =
        sqlx::query_as(&format!("SELECT {COLUMNS} FROM tenants ORDER BY id"))
            .fetch_all(&mut *conn)
            .await
            .map_err(DbError::from)?;
    let mut rows = Vec::with_capacity(raws.len());
    for raw in raws {
        rows.push(load(&mut *conn, raw).await?);
    }
    Ok(rows)
}

/// Fences a tenant's move onto [`ModelScope::All`] against a write that
/// changes what every all-scope tenant reaches. Such a write takes it
/// shared ([`lock_all_scope`]) before it reads which tenants are on `all`,
/// and holds it to its commit; a move onto `all` takes it exclusive right
/// after the tenant's own row (and account), before it projects the
/// tenant's keys from the catalogue. So a tenant that joins `all` is either
/// in the writer's census, its keys rewritten by the writer, or projected
/// after the writer committed, from a catalogue that holds the writer's
/// change. Without it a tenant could join between the census and the
/// write and be rewritten by neither.
///
/// A tenant's creation is a move onto `all` too (every customer starts
/// there): [`create`] takes it exclusive as its first lock, since there is
/// no tenant row yet.
///
/// The writer takes it before any tenant row, the move after its tenant's
/// row, and neither order can close a wait cycle: a writer waits only on
/// rows of tenants already on `all`, and a move waits for the fence only
/// while its tenant is not. A create holds nothing a writer waits on while
/// it waits for the fence, and takes only the default groups' rows and the
/// resources lock after it, which no writer holds while it waits for the
/// fence.
pub const SCOPE_LOCK_KEY: i64 = 0x6169_7369_7863_7034;

/// Locks every tenant on [`ModelScope::All`], in ascending id order, so a
/// write that changes what those tenants reach (`channels::regenerate`, a
/// public model's retire) can reproject their key documents in the same
/// transaction, under the lock order: the tenant rows first, then the
/// public model's row, then the resources lock. [`SCOPE_LOCK_KEY`] is taken
/// shared first, so no tenant joins `all` between the census and the
/// commit.
///
/// A selected-scope tenant holds no reference to a public model as a whole,
/// so it is not taken here; a change in the public model catalogue reaches
/// it only through an operator's edit of its list.
pub async fn lock_all_scope(conn: &mut PgConnection) -> Result<Vec<TenantRow>, ApiError> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(SCOPE_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let mut ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM tenants WHERE model_scope = 'all' ORDER BY id")
            .fetch_all(&mut *conn)
            .await
            .map_err(DbError::from)?;
    lock_many(conn, &mut ids).await
}

/// Locks the tenants `ids`, in ascending id order (which `ids` is sorted
/// into, without repeats), so that two transactions locking overlapping
/// sets cannot deadlock.
pub async fn lock_many(
    conn: &mut PgConnection,
    ids: &mut Vec<Uuid>,
) -> Result<Vec<TenantRow>, ApiError> {
    ids.sort();
    ids.dedup();
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids.iter() {
        rows.push(lock(&mut *conn, *id).await?);
    }
    Ok(rows)
}

/// [`lock_billing`] for several tenants: each tenant row and then its
/// billing account, tenant by tenant in ascending id order (which `ids` is
/// sorted into, without repeats), so that two transactions locking
/// overlapping sets cannot deadlock and usage intake, which locks accounts
/// alone in the same order, cannot either. A path that posts to several
/// tenants' ledgers takes this as the first locks of its transaction.
pub async fn lock_many_billing(
    conn: &mut PgConnection,
    ids: &mut Vec<Uuid>,
) -> Result<Vec<(TenantRow, ledger::Account)>, ApiError> {
    ids.sort();
    ids.dedup();
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids.iter() {
        rows.push(lock_billing(&mut *conn, *id).await?);
    }
    Ok(rows)
}

/// What a new tenant row holds.
struct NewTenant<'a> {
    name: &'a str,
    status: TenantStatus,
    is_internal: bool,
    billing_mode: BillingMode,
    groups: &'a [String],
    limits: &'a TenantLimits,
    approved_by: Option<Uuid>,
}

/// Inserts the tenant row `new` and its billing account, and writes its
/// documents.
async fn insert(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    new: NewTenant<'_>,
) -> Result<TenantRow, ApiError> {
    let id = Uuid::new_v4();
    let limits = serde_json::to_value(new.limits).map_err(ApiError::internal)?;
    sqlx::query(
        "INSERT INTO tenants (id, name, status, is_internal, billing_mode, model_scope, \
           allowed_provider_groups, limits, approved_at, approved_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
           CASE WHEN $3 = 'active' THEN now() END, $9)",
    )
    .bind(id)
    .bind(new.name)
    .bind(new.status.as_str())
    .bind(new.is_internal)
    .bind(new.billing_mode.as_str())
    // The platform's own tenant stays `selected` with its (empty) list: its
    // keys are the platform's, and `all` would widen them from the hand-made
    // models an operator grants by id to every public model. A customer
    // starts on every public model, the owner's default.
    .bind(if new.is_internal {
        ModelScope::Selected
    } else {
        ModelScope::All
    }
    .as_str())
    .bind(new.groups)
    .bind(limits)
    .bind(new.approved_by)
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;
    sqlx::query("INSERT INTO billing_accounts (tenant_id) VALUES ($1)")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    project(&mut *conn, ctx, id).await?;
    lock(conn, id).await
}

/// Creates an ordinary tenant in `status`, with a copy of the current
/// tenant defaults (its limits and its provider groups, ruling 27), opens
/// its billing account and writes its documents. An active tenant counts as
/// approved by `actor`. Recording it in the audit log is the caller's: signup
/// and the operator API record it differently.
///
/// The default groups are taken `FOR SHARE` ([`locked_defaults`]) before the
/// tenant row names them, so a retire of one either commits first, and is
/// then not copied, or waits for this create and counts the new tenant.
///
/// A customer starts on [`ModelScope::All`], so a create is a move onto
/// `all` and takes [`SCOPE_LOCK_KEY`] exclusive first, before any other lock
/// (there is no tenant row yet): a catalogue write that counted the
/// all-scope tenants without this one has committed before the tenant
/// exists, and one that starts later waits for this commit and counts it.
/// Without it a key minted for the new tenant while such a write is in
/// flight would miss the model the write makes callable, and the write
/// would rewrite only the tenants it counted. A pending tenant is fenced
/// too: it is approved, and its keys minted, outside the writer's census.
pub async fn create(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    name: &str,
    status: TenantStatus,
    billing_mode: BillingMode,
    actor: Actor<'_>,
) -> Result<TenantRow, ApiError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SCOPE_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let defaults = locked_defaults(&mut *conn).await?;
    let approved_by = match (status, actor) {
        (TenantStatus::Active, Actor::Operator(op)) => Some(op.id),
        _ => None,
    };
    insert(
        conn,
        ctx,
        NewTenant {
            name,
            status,
            is_internal: false,
            billing_mode,
            groups: &defaults.default_provider_groups,
            limits: &defaults.limits(),
            approved_by,
        },
    )
    .await
}

/// The stored tenant defaults, their groups taken `FOR SHARE` through
/// [`crate::groups::lock_named`], in the lock order: after any tenant row
/// (a create has none yet) and before `RESOURCES_LOCK_KEY`, which
/// [`project`] takes.
///
/// A default group the lock finds retired was retired after the defaults
/// were read, and a retire refuses while the defaults name the group, so
/// the defaults changed in between: they are read once more. Refused a
/// second time, they name a group that is not live, which the defaults API
/// never stores, and nothing is created.
async fn locked_defaults(conn: &mut PgConnection) -> Result<TenantDefaults, ApiError> {
    let mut first = true;
    loop {
        let defaults = settings::tenant_defaults(&mut *conn).await?;
        // A customer with no group is served by every upstream. The defaults
        // API refuses to store none, so a stored row without one was written
        // some other way; creating nothing is the closed failure.
        if defaults.default_provider_groups.is_empty() {
            return Err(ApiError::internal(
                "the stored tenant defaults name no provider group; refusing to create a tenant \
                 that every upstream would serve",
            ));
        }
        // The first pass is released before the second: the defaults read
        // again can name a group of a lower id than one the first pass
        // still held, which would lock out of id order.
        sqlx::query("SAVEPOINT tenant_defaults")
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
        match crate::groups::lock_named(
            &mut *conn,
            &defaults.default_provider_groups,
            "/default_provider_groups",
        )
        .await
        {
            Ok(_) => {
                sqlx::query("RELEASE SAVEPOINT tenant_defaults")
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                return Ok(defaults);
            }
            Err(e) if e.status == StatusCode::UNPROCESSABLE_ENTITY && first => {
                sqlx::query("ROLLBACK TO SAVEPOINT tenant_defaults")
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                first = false;
            }
            Err(e) if e.status == StatusCode::UNPROCESSABLE_ENTITY => {
                return Err(ApiError::internal(
                    "the stored tenant defaults name a provider group that is not live; \
                     refusing to create a tenant",
                ))
            }
            Err(e) => return Err(e),
        }
    }
}

/// One write [`project`] makes.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// The tenant's `teams` document.
    Team(Value),
    /// The `users` document of every live member that has none live.
    Users,
    /// The limits policy, written (or revived) with this document.
    Policy(Value),
    /// The tenant's balance mark (`marks::project`).
    Billing,
    /// The tenant's keys, re-projected from its row.
    Keys,
    /// The limits policy deleted, if it is live.
    RetirePolicy,
}

/// The writes [`project`] makes for `tenant`, in the order it makes them.
///
/// A pending tenant makes none: signup is anonymous, and a tenant nobody
/// has approved must not grow what every gateway loads. It has no keys
/// (an operator cannot mint one before the approval), and its members'
/// `users` documents wait too, so the approval writes the team and every
/// member's user ahead of everything that names them.
///
/// The projector writes kine in commit order and stops at the first write
/// it cannot make, so a projection can stall between any two of these. The
/// order is therefore chosen so that no stall leaves a key enabled and
/// unlimited: the tenant's limit is in place before its keys are opened (an
/// approval or a reactivation enables them), and is lifted only after they
/// are closed (a suspension disables them). Its balance mark is written right
/// before the keys in every case, so a tenant out of money is blocked before
/// any key of it opens. The team and the users come first, since the keys
/// name them.
fn steps(tenant: &TenantRow) -> Vec<Step> {
    let policy = match tenant.status {
        TenantStatus::Pending => return Vec::new(),
        TenantStatus::Active => limits_policy_doc(tenant.id, &tenant.limits),
        TenantStatus::Suspended => None,
    };
    let mut steps = vec![Step::Team(team_doc(&tenant.name)), Step::Users];
    match policy {
        Some(doc) => steps.extend([Step::Policy(doc), Step::Billing, Step::Keys]),
        None => steps.extend([Step::Billing, Step::Keys, Step::RetirePolicy]),
    }
    steps
}

/// Whether a projection runs its billing step, which writes the tenant's
/// balance and quota marks (`marks::project`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BillingStep {
    /// Run it: the status, the billing mode or the money moved, and the
    /// caller holds the tenant's billing lock ([`lock_billing`]).
    Run,
    /// Skip it: nothing a mark is decided from moved, so the marks stand,
    /// and the caller holds the tenant's row alone ([`lock`]), leaving its
    /// account to usage intake.
    Skip,
}

/// Writes the documents tenant `id` derives, as its row now stands, in the
/// order [`steps`] gives: the team, its members' users, its balance mark,
/// its keys, and its limits policy, which is written while the tenant is
/// active and limits it tenant-wide and deleted otherwise; nothing while it
/// is pending. A document already as it should be is not written again. The
/// caller holds the tenant's billing lock ([`lock_billing`]), taken before
/// anything that takes the resources lock, since the balance mark is decided
/// on the account; the transaction creating the tenant holds its new account
/// already, and the internal tenant has no mark. A caller that moves nothing
/// a mark depends on calls [`project_without_marks`] instead.
pub async fn project(conn: &mut PgConnection, ctx: &WriteCtx, id: Uuid) -> Result<(), ApiError> {
    project_with(conn, ctx, id, BillingStep::Run).await
}

/// [`project`] without the billing step, for a caller holding only the
/// tenant's row: an edit that changes neither the status nor the billing
/// mode nor any money (a rename, a grant, limits, groups), and a model
/// delete. A groups edit can reopen keys, but no mark depends on whether a
/// key is open: a mark is the tenant's or a quota's, so the marks already
/// written stand.
async fn project_without_marks(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    id: Uuid,
) -> Result<(), ApiError> {
    project_with(conn, ctx, id, BillingStep::Skip).await
}

async fn project_with(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    id: Uuid,
    billing: BillingStep,
) -> Result<(), ApiError> {
    let tenant = lock(&mut *conn, id).await?;
    let owner = owner(id);
    for step in steps(&tenant) {
        match step {
            Step::Team(doc) => put(&mut *conn, ctx, Kind::Teams, id, &doc, &owner).await?,
            Step::Users => members::put_missing_users(&mut *conn, ctx, id).await?,
            Step::Billing => {
                if billing == BillingStep::Run {
                    marks::project(&mut *conn, ctx, &tenant).await?
                }
            }
            Step::Policy(doc) => {
                let policy_id = match tenant.limits_policy_id {
                    Some(policy_id) => policy_id,
                    None => mint_policy_id(&mut *conn, id).await?,
                };
                put(
                    &mut *conn,
                    ctx,
                    Kind::RateLimitPolicies,
                    policy_id,
                    &doc,
                    &owner,
                )
                .await?;
            }
            // Here and nowhere else, so that the limit brackets the keys as
            // `steps` describes.
            Step::Keys => keys::reproject_tenant(&mut *conn, ctx, id).await?,
            Step::RetirePolicy => {
                if let Some(policy_id) = tenant.limits_policy_id {
                    retire(&mut *conn, ctx, Kind::RateLimitPolicies, policy_id, &owner).await?;
                }
            }
        }
    }
    Ok(())
}

/// The id tenant `id`'s limits policy is written under from now on.
async fn mint_policy_id(conn: &mut PgConnection, id: Uuid) -> Result<Uuid, ApiError> {
    let policy_id = Uuid::new_v4();
    sqlx::query("UPDATE tenants SET limits_policy_id = $2 WHERE id = $1")
        .bind(id)
        .bind(policy_id)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(policy_id)
}

/// Makes the document `kind`/`id` live with `doc`: unchanged when it
/// already is, replaced when it is live, created (or its tombstone
/// revived) otherwise.
pub(crate) async fn put(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    kind: Kind,
    id: Uuid,
    doc: &Value,
    owner: &str,
) -> Result<(), ApiError> {
    let current = store::get(&mut *conn, kind, id).await?;
    let op = match &current {
        Some(row) if !row.deleted && row.doc == *doc => return Ok(()),
        Some(row) if !row.deleted => WriteOp::Replace(doc),
        _ => WriteOp::Create(doc),
    };
    write_resource(
        conn,
        ctx,
        ResourceWrite {
            kind,
            id,
            op,
            expected_version: None,
            managed_by: Some(owner),
        },
    )
    .await?;
    Ok(())
}

/// Deletes the document `kind`/`id` if it is live.
pub(crate) async fn retire(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    kind: Kind,
    id: Uuid,
    owner: &str,
) -> Result<(), ApiError> {
    let live = store::get(&mut *conn, kind, id)
        .await?
        .is_some_and(|row| !row.deleted);
    if live {
        write_resource(
            conn,
            ctx,
            ResourceWrite {
                kind,
                id,
                op: WriteOp::Delete,
                expected_version: None,
                managed_by: Some(owner),
            },
        )
        .await?;
    }
    Ok(())
}

/// The platform's own tenant (`is_internal`, active, named
/// [`INTERNAL_TENANT_NAME`], with no limits and no groups), created by the
/// first boot; later boots find it and bring its documents up to date,
/// unless the gateway-version gate refuses that, which is logged and left
/// for a later boot ([`settle_boot_projection`]).
/// The same boot writes the first tenant defaults (ruling 14).
///
/// Runs under the boot lock, so instances booting together create it once;
/// the unique index `tenants_one_internal` holds that even without the lock.
pub async fn ensure_internal_tenant(db: &Db, ctx: &WriteCtx) -> Result<TenantRow, BootError> {
    let ctx = WriteCtx {
        mk: crate::crypto::MasterKey(ctx.mk.0),
        env: ctx.env.clone(),
        cp_version: ctx.cp_version.clone(),
        stale_secs: ctx.stale_secs,
    };
    db.tx(move |conn| {
        async move {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(BOOT_LOCK_KEY)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
            settings::seed_tenant_defaults(&mut *conn).await?;
            let existing: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
            match existing {
                Some(id) => {
                    lock(&mut *conn, id).await?;
                    boot_savepoint(&mut *conn).await?;
                    let outcome = project(&mut *conn, &ctx, id).await;
                    if !settle_boot_projection(&mut *conn, outcome).await? {
                        tracing::warn!(
                            tenant = %id,
                            "the platform's own tenant was not re-projected: a registered \
                             gateway is too old for its documents; the next boot tries again"
                        );
                    }
                    lock(conn, id).await
                }
                None => {
                    insert(
                        conn,
                        &ctx,
                        NewTenant {
                            name: INTERNAL_TENANT_NAME,
                            status: TenantStatus::Active,
                            is_internal: true,
                            // The platform funds its own tenant postpaid (R2):
                            // never blocked, and its consumption is its balance.
                            billing_mode: BillingMode::Postpaid,
                            groups: &[],
                            limits: &TenantLimits::default(),
                            approved_by: None,
                        },
                    )
                    .await
                }
            }
        }
        .boxed()
    })
    .await
    .map_err(|e: ApiError| BootError::InternalTenant(format!("{}: {}", e.code, e.message)))
}

/// Starts the savepoint a projection made at boot runs in; it is ended by
/// [`settle_boot_projection`].
pub(crate) async fn boot_savepoint(conn: &mut PgConnection) -> Result<(), ApiError> {
    sqlx::query("SAVEPOINT boot_projection")
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}

/// Ends a projection made at boot, begun with [`boot_savepoint`], by its
/// `outcome`: kept when it succeeded (`true`); undone when the
/// gateway-version gate refused it (`false`), since every upgrade boots the
/// control plane while older gateways are still registered and the boot
/// must not fail for that; any other error is returned, and fails the boot.
pub(crate) async fn settle_boot_projection(
    conn: &mut PgConnection,
    outcome: Result<(), ApiError>,
) -> Result<bool, ApiError> {
    let (statement, kept) = match outcome {
        Ok(()) => ("RELEASE SAVEPOINT boot_projection", true),
        Err(e) if compat::refused(&e) => ("ROLLBACK TO SAVEPOINT boot_projection", false),
        Err(e) => return Err(e),
    };
    sqlx::query(statement)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(kept)
}

/// Every live public model: the tenant-facing model names, by id, sorted.
///
/// A public model counts once it is **callable** — its row is live and its
/// routing document is live. A newly created one has no document until an
/// operator assigns it to an upstream account (`channels::create` writes the
/// row alone), so it reaches no tenant before there is something to call,
/// and a model whose every serving account is disabled keeps its document
/// (with no target) and stays listed, answering `upstream_unavailable`,
/// exactly as a grant of it would.
pub async fn public_model_ids(conn: &mut PgConnection) -> Result<Vec<Uuid>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT p.id FROM public_models p \
         WHERE p.retired_at IS NULL \
           AND EXISTS (SELECT 1 FROM gw_resources r \
                       WHERE r.kind = $1 AND r.id = p.id AND NOT r.deleted) \
         ORDER BY p.id",
    )
    .bind(Kind::Models.segment())
    .fetch_all(conn)
    .await?)
}

/// The model ids tenant `id` reaches, sorted: every live public model under
/// [`ModelScope::All`], and its stored list under [`ModelScope::Selected`].
/// The tenant's scope is read from its row, so this answers on a connection
/// that has no use for a whole [`TenantRow`] — the gateway's price lookups
/// on their own pool.
///
/// This is the one computation of what a tenant's scope gives it, read by
/// the key documents, the gateway's price lookups and the tenant-facing
/// model list. "All" is computed here rather than materialised as one row
/// per tenant per model, so creating a public model reaches every all-scope
/// tenant at once and retiring one withdraws nothing: the set is what the
/// catalogue holds at the moment it is read, and there is no row to keep in
/// step with it, to delete on a retire, or to backfill for a tenant created
/// between two ticks.
///
/// Under [`ModelScope::Selected`] the stored ids are returned as they are,
/// without re-checking each document: a grant is validated when it is
/// written, under the resources lock, and a model delete withdraws it
/// (`on_model_deleted`).
pub async fn scope_model_ids(
    conn: &mut PgConnection,
    tenant_id: Uuid,
) -> Result<Vec<Uuid>, DbError> {
    // A tenant row the caller holds is there; one that is not is answered as
    // `RowNotFound`, which the price lookup renders as the `404` it answers an
    // unknown key with.
    let stored: (String, Vec<Uuid>) =
        sqlx::query_as("SELECT model_scope, allowed_model_ids FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
    match ModelScope::parse(&stored.0) {
        Some(ModelScope::Selected) => Ok(stored.1),
        _ => public_model_ids(conn).await,
    }
}

/// The tenants whose grants, or whose live keys' selections, include
/// `model_id`, in id order.
async fn holders_of(conn: &mut PgConnection, model_id: Uuid) -> Result<Vec<Uuid>, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM tenants WHERE $1 = ANY(allowed_model_ids) \
         UNION SELECT tenant_id FROM tenant_keys WHERE revoked_at IS NULL AND $1 = ANY(model_ids) \
         ORDER BY 1",
    )
    .bind(model_id)
    .fetch_all(conn)
    .await
    .map_err(DbError::from)?)
}

/// Withdraws `model_id` from every tenant grant and every live key's
/// selection, and re-projects each tenant it was withdrawn from with its
/// keys, ahead of the model's own delete in the same transaction (ruling
/// 13). A selection that loses its last model is left empty, which grants
/// none, never cleared, which would follow every grant. Runs before
/// `write_resource` deletes the model, so the tenant locks are taken before
/// the resources lock.
///
/// A grant is only ever added under the resources lock (see [`update`]),
/// and a key selects only from its tenant's grants, under the tenant's
/// lock. The holders are looked up before the resources lock can be taken,
/// so they are looked up again once it is held: one that appeared in
/// between, which cannot be locked now without breaking the lock order,
/// fails the delete as `503 unavailable`, to be retried.
///
/// A model delete moves no money and opens no key, so each holder is
/// projected without its marks ([`project_without_marks`]) and its billing
/// account is not taken: its usage intake goes on while the delete runs.
pub async fn on_model_deleted(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    model_id: Uuid,
) -> Result<(), ApiError> {
    let mut holders = holders_of(&mut *conn, model_id).await?;
    lock_many(&mut *conn, &mut holders).await?;
    resources_lock(&mut *conn).await?;
    if holders_of(&mut *conn, model_id).await? != holders {
        tracing::warn!(%model_id, "a grant of the model was added while it was being deleted");
        return Err(ApiError::unavailable());
    }
    for id in holders {
        sqlx::query(
            "UPDATE tenants SET allowed_model_ids = array_remove(allowed_model_ids, $2), \
               version = version + 1 \
             WHERE id = $1 AND $2 = ANY(allowed_model_ids)",
        )
        .bind(id)
        .bind(model_id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
        sqlx::query(
            "UPDATE tenant_keys SET model_ids = array_remove(model_ids, $2), \
               version = version + 1 \
             WHERE tenant_id = $1 AND revoked_at IS NULL AND $2 = ANY(model_ids)",
        )
        .bind(id)
        .bind(model_id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
        project_without_marks(&mut *conn, ctx, id).await?;
    }
    Ok(())
}

/// What one [`replace_generated_grants`] pass did, in counts only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeneratedGrantsReplaced {
    /// Tenants changed, each with its own audit row.
    pub tenants: u64,
    /// Tenant grants of a generated direct model replaced by its public
    /// model (or dropped where the tenant already holds the public model).
    pub grants_replaced: u64,
    /// Tenant grants withdrawn, the public model being retired.
    pub grants_withdrawn: u64,
    /// Live keys whose model selection changed.
    pub keys_changed: u64,
    /// Tenants the gateway-version gate refused, or that stayed busy past
    /// the lock timeout, left for the next pass.
    pub deferred: u64,
    /// Tenants whose change failed for another reason; the next pass tries
    /// them again.
    pub failed: u64,
}

/// One list of model ids with every channel's generated direct model in it
/// replaced by its public model, sorted and without repeats, as grants and
/// selections are stored: `(list, replaced, withdrawn)`. `live` maps each
/// generated direct model the list may name to its public model, `None`
/// when that public model is retired.
fn replace_generated(
    ids: &[Uuid],
    live: &BTreeMap<Uuid, Option<Uuid>>,
) -> (Vec<Uuid>, Vec<(Uuid, Uuid)>, Vec<Uuid>) {
    let mut out = Vec::with_capacity(ids.len());
    let (mut replaced, mut withdrawn) = (Vec::new(), Vec::new());
    for id in ids {
        match live.get(id) {
            None => out.push(*id),
            Some(Some(public)) => {
                replaced.push((*id, *public));
                out.push(*public);
            }
            Some(None) => withdrawn.push(*id),
        }
    }
    out.sort();
    out.dedup();
    (out, replaced, withdrawn)
}

/// Replaces every tenant grant and live key selection that names a
/// channel's generated direct model, which no tenant may hold
/// ([`checked_grants`]) but a release before that refusal could store, and
/// an older replica can still write during a rolling upgrade. The grant was
/// of that public model through one account, so it becomes a grant of the
/// public model; a key that selected the direct model selects the public
/// one, where its tenant then holds it. Where the public model is retired,
/// or the channel was removed since (no channel row and no live document),
/// the grant is withdrawn. Run at boot and by every housekeeping tick.
///
/// One transaction per tenant, so a tenant that fails holds back no other,
/// and takes the locks of a grant edit and no more: the tenant's row, then
/// the public models' rows `FOR SHARE` (a retire takes them `FOR UPDATE`, so
/// none is retired while the replacement is decided), then the resources
/// lock in the re-projection, which skips the billing step. The tenant's row
/// serialises it with every grant and selection write and with a peer
/// running the same pass, which then finds nothing to change. Each tenant
/// changed gets one audit row, `tenant.generated_grants_replaced`, naming
/// the model ids replaced and withdrawn, on the tenant and on each key.
///
/// A tenant the gateway-version gate refuses, or that stays busy, is left
/// for the next pass, and so is one that fails otherwise, logged with its
/// id: its grant stays as it was, which is what the gateways already serve.
/// A tenant whose grants name no such model is not written at all.
pub async fn replace_generated_grants(
    db: &Db,
    ctx: &WriteCtx,
) -> Result<GeneratedGrantsReplaced, ApiError> {
    // A holder names a channel's direct model, or the direct model of a
    // channel removed since: no channel row and no live document, but the
    // tombstone of a document a public model generated as a target (owned by
    // `public:<id>` at another id than the public model's own). That is the
    // grant `replace_generated_in` withdraws; without it, a tenant whose last
    // such channel is gone would never be found. The tombstone outlives the
    // delete's projection by a day (`housekeeping::TOMBSTONE_RETENTION`),
    // and the pass runs at boot and on every tick, so it is found well
    // within that. An id with no such trace is no evidence of a grant this
    // repair is for, and is not looked for.
    let holders: Vec<Uuid> = sqlx::query_scalar(
        "WITH named(id) AS ( \
           SELECT u.id FROM tenants t, unnest(t.allowed_model_ids) AS u(id) \
           UNION SELECT u.id FROM tenant_keys k, unnest(k.model_ids) AS u(id) \
             WHERE k.revoked_at IS NULL \
         ), gone(id) AS ( \
           SELECT n.id FROM named n \
           WHERE NOT EXISTS (SELECT 1 FROM gw_resources r \
                             WHERE r.kind = $1 AND r.id = n.id AND NOT r.deleted) \
             AND NOT EXISTS (SELECT 1 FROM channel_models cm WHERE cm.direct_model_id = n.id) \
             AND EXISTS (SELECT 1 FROM gw_resources r \
                         WHERE r.kind = $1 AND r.id = n.id AND r.deleted \
                           AND r.managed_by LIKE 'public:%' \
                           AND r.managed_by <> 'public:' || r.id::text) \
         ) \
         SELECT t.id FROM tenants t \
         WHERE EXISTS (SELECT 1 FROM channel_models cm WHERE cm.direct_model_id = ANY(t.allowed_model_ids)) \
            OR EXISTS (SELECT 1 FROM gone g WHERE g.id = ANY(t.allowed_model_ids)) \
         UNION SELECT k.tenant_id FROM tenant_keys k \
         WHERE k.revoked_at IS NULL \
           AND (EXISTS (SELECT 1 FROM channel_models cm WHERE cm.direct_model_id = ANY(k.model_ids)) \
             OR EXISTS (SELECT 1 FROM gone g WHERE g.id = ANY(k.model_ids))) \
         ORDER BY 1",
    )
    .bind(Kind::Models.segment())
    .fetch_all(db.pool())
    .await
    .map_err(DbError::from)?;
    let mut outcome = GeneratedGrantsReplaced::default();
    for tenant in holders {
        let ctx = WriteCtx {
            mk: crate::crypto::MasterKey(ctx.mk.0),
            env: ctx.env.clone(),
            cp_version: ctx.cp_version.clone(),
            stale_secs: ctx.stale_secs,
        };
        let changed = db
            .tx(move |conn| async move { replace_generated_in(conn, &ctx, tenant).await }.boxed())
            .await;
        match changed {
            Ok(None) => {}
            Ok(Some((replaced, withdrawn, keys))) => {
                outcome.tenants += 1;
                outcome.grants_replaced += replaced;
                outcome.grants_withdrawn += withdrawn;
                outcome.keys_changed += keys;
            }
            Err(e) if compat::refused(&e) || e.status == StatusCode::SERVICE_UNAVAILABLE => {
                outcome.deferred += 1;
            }
            Err(e) => {
                outcome.failed += 1;
                tracing::warn!(
                    tenant = %tenant,
                    code = e.code,
                    "a tenant's grants of channels' generated direct models could not be replaced"
                );
            }
        }
    }
    Ok(outcome)
}

/// [`replace_generated_grants`] for one tenant, in the caller's
/// transaction: `Some((grants replaced, grants withdrawn, keys changed))`
/// when anything changed, `None` when nothing named such a model.
async fn replace_generated_in(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant_id: Uuid,
) -> Result<Option<(u64, u64, u64)>, ApiError> {
    let tenant = lock(&mut *conn, tenant_id).await?;
    let selections: Vec<(Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT id, model_ids FROM tenant_keys \
         WHERE tenant_id = $1 AND revoked_at IS NULL AND model_ids IS NOT NULL ORDER BY id",
    )
    .bind(tenant_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let named: Vec<Uuid> = tenant
        .allowed_model_ids
        .iter()
        .chain(selections.iter().flat_map(|(_, ids)| ids.iter()))
        .copied()
        .collect();
    let generated: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT direct_model_id, public_model_id FROM channel_models \
         WHERE direct_model_id = ANY($1)",
    )
    .bind(&named)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    // An id that names no live model and no channel row is the direct model
    // of a channel an operator removed since: an untick, or a retire of its
    // public model, deletes the row and the document and leaves the grants.
    // It reaches nothing, and a key document naming it fails the reference
    // check, so it is withdrawn with the rest. It is not replaced: the
    // operator stopped serving the model through that account. Read before
    // anything returns early: a tenant whose every such channel is gone
    // names nothing else here.
    let dead: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT u.id FROM unnest($1::uuid[]) AS u(id) \
         WHERE NOT EXISTS (SELECT 1 FROM gw_resources r \
                           WHERE r.kind = $2 AND r.id = u.id AND NOT r.deleted) \
           AND NOT EXISTS (SELECT 1 FROM channel_models cm WHERE cm.direct_model_id = u.id)",
    )
    .bind(&named)
    .bind(Kind::Models.segment())
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if generated.is_empty() && dead.is_empty() {
        return Ok(None);
    }
    let mut publics: Vec<Uuid> = generated.iter().map(|(_, p)| *p).collect();
    publics.sort();
    publics.dedup();
    // Held until commit, so no public model kept here is retired before the
    // grant naming it commits. One whose routing document is gone is as
    // good as retired: it could not be granted.
    let live: Vec<Uuid> = sqlx::query_scalar(
        "SELECT p.id FROM public_models p \
         WHERE p.id = ANY($1) AND p.retired_at IS NULL \
           AND EXISTS (SELECT 1 FROM gw_resources r \
                       WHERE r.kind = $2 AND r.id = p.id AND NOT r.deleted) \
         ORDER BY p.id FOR SHARE OF p",
    )
    .bind(&publics)
    .bind(Kind::Models.segment())
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let mut map: BTreeMap<Uuid, Option<Uuid>> = generated
        .into_iter()
        .map(|(direct, public)| (direct, live.contains(&public).then_some(public)))
        .collect();
    map.extend(dead.into_iter().map(|id| (id, None)));

    let (grants, replaced, withdrawn) = replace_generated(&tenant.allowed_model_ids, &map);
    if !replaced.is_empty() || !withdrawn.is_empty() {
        sqlx::query(
            "UPDATE tenants SET allowed_model_ids = $2, version = version + 1 WHERE id = $1",
        )
        .bind(tenant_id)
        .bind(&grants)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    }
    // A key's selection becomes the public model only where its tenant holds
    // that model after the pass; otherwise the entry goes, so no key reaches
    // a model its tenant's grants would not give it.
    let key_map: BTreeMap<Uuid, Option<Uuid>> = map
        .iter()
        .map(|(id, public)| (*id, public.filter(|p| grants.contains(p))))
        .collect();
    let mut keys = Vec::new();
    for (key_id, ids) in &selections {
        let (selection, key_replaced, key_withdrawn) = replace_generated(ids, &key_map);
        if key_replaced.is_empty() && key_withdrawn.is_empty() {
            continue;
        }
        sqlx::query("UPDATE tenant_keys SET model_ids = $2, version = version + 1 WHERE id = $1")
            .bind(key_id)
            .bind(&selection)
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
        keys.push(json!({
            "key_id": key_id,
            "replaced": pairs(&key_replaced),
            "withdrawn": key_withdrawn,
        }));
    }
    if replaced.is_empty() && withdrawn.is_empty() && keys.is_empty() {
        return Ok(None);
    }
    project_without_marks(&mut *conn, ctx, tenant_id).await?;
    audit::record(
        &mut *conn,
        AuditEntry {
            actor: Actor::System,
            action: "tenant.generated_grants_replaced",
            target_kind: TARGET_KIND,
            target_id: Some(tenant_id.to_string()),
            tenant_id: Some(tenant_id),
            detail: json!({
                "replaced": pairs(&replaced),
                "withdrawn": withdrawn,
                "keys": keys,
            }),
        },
    )
    .await?;
    Ok(Some((
        replaced.len() as u64,
        withdrawn.len() as u64,
        keys.len() as u64,
    )))
}

/// `(generated direct model, public model)` pairs as audit detail.
fn pairs(replaced: &[(Uuid, Uuid)]) -> Vec<Value> {
    replaced
        .iter()
        .map(|(direct, public)| json!({"model_id": direct, "public_model_id": public}))
        .collect()
}

async fn resources_lock(conn: &mut PgConnection) -> Result<(), ApiError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}

/// Whether a model document is one ordinary tenants may not be granted
/// (ruling 15), and the path that makes it so: a wildcard row reaches past
/// the catalog, and ensemble models are experimental.
fn ungrantable(doc: &Value) -> Option<(&'static str, &'static str)> {
    if doc
        .get("display_name")
        .and_then(Value::as_str)
        .is_some_and(|name| name.contains('*'))
    {
        return Some(("/display_name", "通配模型"));
    }
    if doc.get("ensemble").is_some_and(|e| !e.is_null()) {
        return Some(("/ensemble", "集成（ensemble）模型"));
    }
    None
}

/// The model ids `raw` names, sorted and without repeats, once each is a
/// live model a tenant of this kind may be granted; otherwise a detail at
/// `/allowed_model_ids/<i>` for each one that is not. The caller holds the
/// resources lock, so no model it checks can be deleted or reshaped before
/// the grant commits.
async fn checked_grants(
    conn: &mut PgConnection,
    is_internal: bool,
    raw: &[String],
) -> Result<Vec<Uuid>, ApiError> {
    let mut ids = Vec::with_capacity(raw.len());
    let mut problems = Vec::new();
    for (i, entry) in raw.iter().enumerate() {
        let path = format!("/allowed_model_ids/{i}");
        let Ok(id) = Uuid::parse_str(entry) else {
            problems.push(detail(&path, "不是有效的模型 id"));
            continue;
        };
        let doc: Option<Value> = sqlx::query_scalar(
            "SELECT doc FROM gw_resources WHERE kind = $1 AND id = $2 AND NOT deleted",
        )
        .bind(Kind::Models.segment())
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::from)?;
        match doc {
            None => problems.push(detail(&path, "模型不存在")),
            // No tenant may be granted a channel's generated direct document,
            // the platform's own included. Its name is `<public name>#<account
            // display name>`, so a customer granted it is shown which upstream
            // account serves the model, and its key calls that account alone,
            // past the public model's priorities and weights. Every tenant is
            // refused for two more reasons: the document has no price of its
            // own, so every call through it is stored unpriced, while the
            // platform's own tenant is priced like any other; and it lives and
            // dies with its channel row, which removes it without withdrawing
            // any grant (`channels` takes no tenant row), so a grant on it
            // would be left naming a deleted model. The public model is what
            // to grant instead.
            Some(_) if crate::channels::is_generated_direct(&mut *conn, id).await? => problems
                .push(detail(
                    &path,
                    if is_internal {
                        "公共模型生成的直连模型不能授权给平台内部租户，请授权该公共模型"
                    } else {
                        "公共模型生成的直连模型不能授权给客户租户，请授权该公共模型"
                    },
                )),
            Some(doc) => match ungrantable(&doc).filter(|_| !is_internal) {
                Some((_, what)) => {
                    problems.push(detail(&path, &format!("{what}不能授权给客户租户")))
                }
                None => ids.push(id),
            },
        }
    }
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Refuses a model write that makes the model `model_id`, which an ordinary
/// tenant holds, one it could not have been granted. Runs after the write,
/// under the resources lock it took, so it sees every committed grant and
/// no grant can be added before this transaction ends.
pub async fn refuse_ungrantable_reshape(
    conn: &mut PgConnection,
    model_id: Uuid,
    doc: &Value,
) -> Result<(), ApiError> {
    let Some((path, what)) = ungrantable(doc) else {
        return Ok(());
    };
    let held: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tenants WHERE NOT is_internal AND $1 = ANY(allowed_model_ids))",
    )
    .bind(model_id)
    .fetch_one(conn)
    .await
    .map_err(DbError::from)?;
    if held {
        return Err(ApiError::validation(vec![detail(
            path,
            &format!("该模型已授权给客户租户，不能改为{what}"),
        )]));
    }
    Ok(())
}

/// The tenant name to store: trimmed, 1 to 128 characters, no control
/// characters.
pub(crate) fn tenant_name(raw: &str) -> Result<String, Detail> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(detail("/name", "名称不能为空"));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(detail(
            "/name",
            &format!("名称不能超过 {MAX_NAME_CHARS} 个字符"),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(detail("/name", "名称不能包含控制字符"));
    }
    Ok(name.to_string())
}

/// The group names `groups` as they are stored, a name given twice kept
/// once (where it first appears); otherwise a detail at `<at>/<i>` for each
/// name that is blank, starts or ends with whitespace, carries a control
/// character, or is longer than 64 characters. Names are matched exactly
/// against provider keys' groups, so a stray space would match nothing; a
/// control character is refused for the reason `tenant_name` refuses one,
/// that Postgres rejects the parameter and the refusal would be a `500`
/// instead of a detail at the name's own path.
pub(crate) fn checked_groups(groups: &[String], at: &str) -> Result<Vec<String>, Vec<Detail>> {
    let problems: Vec<Detail> = groups
        .iter()
        .enumerate()
        .filter_map(|(i, group)| {
            let path = format!("{at}/{i}");
            if group.trim().is_empty() {
                Some(detail(&path, "分组名称不能为空"))
            } else if group.trim() != group {
                Some(detail(&path, "分组名称前后不能有空白"))
            } else if group.chars().any(char::is_control) {
                Some(detail(&path, "分组名称不能包含控制字符"))
            } else if group.chars().count() > MAX_GROUP_CHARS {
                Some(detail(
                    &path,
                    &format!("分组名称不能超过 {MAX_GROUP_CHARS} 个字符"),
                ))
            } else {
                None
            }
        })
        .collect();
    if !problems.is_empty() {
        return Err(problems);
    }
    let mut kept: Vec<String> = Vec::with_capacity(groups.len());
    for group in groups {
        if !kept.contains(group) {
            kept.push(group.clone());
        }
    }
    Ok(kept)
}

/// The refusal of an empty group list where a customer's is required: a
/// key with no groups is served by every upstream, so a customer reaching
/// every upstream lists every group instead.
pub(crate) fn no_groups(at: &str) -> Detail {
    detail(
        at,
        "至少需要一个分组：不设分组即可使用全部上游；如确需全部上游，请逐一列出各分组",
    )
}

/// A detail at `<at>/<field>` for each field of the object `value` that is
/// not one of `known`.
fn unknown_fields(value: &Value, known: &[&str], at: &str) -> Vec<Detail> {
    match value.as_object() {
        Some(fields) => fields
            .keys()
            .filter(|name| !known.contains(&name.as_str()))
            .map(|name| detail(&format!("{at}/{name}"), "不支持的字段"))
            .collect(),
        None => vec![detail(at, "必须是一个对象")],
    }
}

/// The limits a request body gives at `at`, checked.
fn limits_from(value: &Value, at: &str) -> Result<TenantLimits, Vec<Detail>> {
    let unknown = unknown_fields(value, &LIMIT_FIELDS, at);
    if !unknown.is_empty() {
        return Err(unknown);
    }
    let limits: TenantLimits = serde_json::from_value(value.clone())
        .map_err(|_| vec![detail(at, "每个限制必须是正整数")])?;
    let problems = limits.problems(at);
    if problems.is_empty() {
        Ok(limits)
    } else {
        Err(problems)
    }
}

/// `Tenant` in the API.
#[derive(Debug, Serialize)]
pub struct TenantView {
    id: Uuid,
    name: String,
    status: TenantStatus,
    is_internal: bool,
    billing_mode: BillingMode,
    /// `all` or `selected`; see [`ModelScope`].
    model_scope: ModelScope,
    /// The stored selection, which the scope decides the meaning of: under
    /// `all` it is kept but grants nothing on its own.
    allowed_model_ids: Vec<Uuid>,
    /// What the tenant reaches now, as [`scope_model_ids`] resolves it:
    /// every live public model under `all`, the stored selection under
    /// `selected`. A key's own selection is checked against this list, so it
    /// is what a form offers a key and what a key's models are judged by.
    effective_model_ids: Vec<Uuid>,
    allowed_provider_groups: Vec<String>,
    limits: TenantLimits,
    /// Members that are not removed, disabled ones included.
    member_count: i64,
    /// Keys that are not revoked.
    key_count: i64,
    version: i64,
    created_at: DateTime<Utc>,
    approved_at: Option<DateTime<Utc>>,
    /// The worst rollout among the documents the tenant and its keys
    /// derive.
    rollout: ResourceRollout,
}

/// The views of `tenants`, each with the rollout of its documents and its
/// keys' (live ones, and deletes still on their way) over the gateways of
/// `env`, online when heard from within `stale_secs`.
async fn views(
    conn: &mut PgConnection,
    env: &EnvSettings,
    stale_secs: f64,
    tenants: Vec<TenantRow>,
) -> Result<Vec<TenantView>, ApiError> {
    let fleet = Fleet::load(&mut *conn, env, stale_secs).await?;
    let ids: Vec<Uuid> = tenants.iter().map(|t| t.id).collect();
    // Each document's owner, and the tenant it belongs to: the tenant's
    // own, and its keys', revoked ones included while their delete may be
    // on its way.
    let mut tenant_of: BTreeMap<String, Uuid> =
        tenants.iter().map(|t| (owner(t.id), t.id)).collect();
    let tenant_keys: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, tenant_id FROM tenant_keys WHERE tenant_id = ANY($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(DbError::from)?;
    tenant_of.extend(
        tenant_keys
            .into_iter()
            .map(|(key, t)| (keys::owner(key), t)),
    );
    let owners: Vec<String> = tenant_of.keys().cloned().collect();
    let mut rows: BTreeMap<Uuid, Vec<ResourceRow>> = BTreeMap::new();
    for row in store::owned_by(&mut *conn, &owners).await? {
        if let Some(t) = row.managed_by.as_ref().and_then(|o| tenant_of.get(o)) {
            rows.entry(*t).or_default().push(row);
        }
    }
    let key_counts: BTreeMap<Uuid, i64> = sqlx::query_as::<_, (Uuid, i64)>(
        "SELECT tenant_id, count(*) FROM tenant_keys \
         WHERE tenant_id = ANY($1) AND revoked_at IS NULL GROUP BY tenant_id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?
    .into_iter()
    .collect();
    let member_counts: BTreeMap<Uuid, i64> = sqlx::query_as::<_, (Uuid, i64)>(
        "SELECT tenant_id, count(*) FROM members \
         WHERE tenant_id = ANY($1) AND removed_at IS NULL GROUP BY tenant_id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?
    .into_iter()
    .collect();
    // The live public models, read once for every all-scope tenant listed
    // rather than once per tenant; the rule is `scope_model_ids`'s own.
    let public = if tenants.iter().any(|t| t.model_scope == ModelScope::All) {
        public_model_ids(&mut *conn).await?
    } else {
        Vec::new()
    };
    Ok(tenants
        .into_iter()
        .map(|t| {
            let effective_model_ids = match t.model_scope {
                ModelScope::All => public.clone(),
                ModelScope::Selected => t.allowed_model_ids.clone(),
            };
            let states: Vec<ResourceRollout> = rows
                .get(&t.id)
                .map(|rows| {
                    rows.iter()
                        .map(|row| fleet.rollout(&RolloutInput::of_row(row)))
                        .collect()
                })
                .unwrap_or_default();
            TenantView {
                id: t.id,
                name: t.name,
                status: t.status,
                is_internal: t.is_internal,
                billing_mode: t.billing_mode,
                model_scope: t.model_scope,
                allowed_model_ids: t.allowed_model_ids,
                effective_model_ids,
                allowed_provider_groups: t.allowed_provider_groups,
                limits: t.limits,
                member_count: member_counts.get(&t.id).copied().unwrap_or(0),
                key_count: key_counts.get(&t.id).copied().unwrap_or(0),
                version: t.version,
                created_at: t.created_at,
                approved_at: t.approved_at,
                rollout: rollout::worst(&states),
            }
        })
        .collect())
}

async fn view(
    conn: &mut PgConnection,
    env: &EnvSettings,
    stale_secs: f64,
    tenant: TenantRow,
) -> Result<TenantView, ApiError> {
    views(conn, env, stale_secs, vec![tenant])
        .await?
        .pop()
        .ok_or_else(|| ApiError::internal("a tenant view went missing"))
}

/// A malformed id names no tenant.
fn tenant_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::not_found())
}

fn audit_entry<'a>(
    auth: &'a OperatorAuth,
    action: &'static str,
    id: Uuid,
    detail: Value,
) -> AuditEntry<'a> {
    AuditEntry {
        actor: Actor::Operator(auth),
        action,
        target_kind: TARGET_KIND,
        target_id: Some(id.to_string()),
        tenant_id: Some(id),
        detail,
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    status: Option<String>,
}

/// `GET /admin/tenants[?status=]` → `{items: [Tenant]}`, oldest first.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let bad_status = || ApiError::validation(vec![detail("status", "未知的租户状态")]);
    let Query(q) = query.map_err(|_| bad_status())?;
    let status = match q.status.as_deref() {
        None => None,
        Some(raw) => Some(TenantStatus::parse(raw).ok_or_else(bad_status)?),
    };
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let raw: Vec<RawTenant> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM tenants WHERE ($1::text IS NULL OR status = $1) \
         ORDER BY created_at, id"
    ))
    .bind(status.map(TenantStatus::as_str))
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let tenants = raw
        .into_iter()
        .map(TenantRow::from_raw)
        .collect::<Result<Vec<_>, _>>()?;
    let items = views(
        &mut conn,
        &state.env,
        stale_window_secs(&state.cfg),
        tenants,
    )
    .await?;
    Ok(Json(json!({ "items": items })))
}

/// `GET /admin/tenants/{id}` → `Tenant` and `ETag: "<version>"`.
pub async fn get(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = tenant_id(&id)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let raw: Option<RawTenant> =
        sqlx::query_as(&format!("SELECT {COLUMNS} FROM tenants WHERE id = $1"))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(DbError::from)?;
    let tenant = TenantRow::from_raw(raw.ok_or_else(ApiError::not_found)?)?;
    let etag = format!("\"{}\"", tenant.version);
    let view = view(&mut conn, &state.env, stale_window_secs(&state.cfg), tenant).await?;
    Ok(([(header::ETAG, etag)], Json(view)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTenant {
    name: String,
    #[serde(default)]
    billing_mode: BillingMode,
}

/// `POST /admin/tenants` with `{name, billing_mode?}` → `201 Tenant`. An
/// operator's tenant starts `active`.
pub async fn create_tenant(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<CreateTenant>,
) -> Result<(StatusCode, Json<TenantView>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let name = tenant_name(&req.name).map_err(|d| ApiError::validation(vec![d]))?;
    let ctx = WriteCtx::of(&state);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let tenant = create(
                    &mut *conn,
                    &ctx,
                    &name,
                    TenantStatus::Active,
                    req.billing_mode,
                    Actor::Operator(&auth),
                )
                .await?;
                audit::record(
                    &mut *conn,
                    audit_entry(
                        &auth,
                        "tenant.create",
                        tenant.id,
                        json!({ "billing_mode": tenant.billing_mode }),
                    ),
                )
                .await?;
                view(conn, &ctx.env, ctx.stale_secs, tenant).await
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(view)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTenant {
    name: Option<String>,
    billing_mode: Option<BillingMode>,
    /// `all` or `selected`. Sending `allowed_model_ids` without it means
    /// `selected`: a list is a selection, and accepting it under `all` would
    /// store a list that grants nothing. Sending `all` with a list is
    /// refused, for the same reason read the other way.
    model_scope: Option<ModelScope>,
    allowed_model_ids: Option<Vec<String>>,
    allowed_provider_groups: Option<Vec<String>>,
    /// Replaces every limit: a field left out is unset.
    limits: Option<Value>,
}

/// `PATCH /admin/tenants/{id}` with `If-Match` → `200 Tenant`. Each field
/// given replaces the stored one, and the tenant's documents follow.
pub async fn update(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<UpdateTenant>,
) -> Result<Json<TenantView>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = tenant_id(&id)?;
    let expected = if_match(&headers)?;
    let mut problems = Vec::new();
    let name = match req.name.as_deref().map(tenant_name) {
        Some(Err(d)) => {
            problems.push(d);
            None
        }
        Some(Ok(name)) => Some(name),
        None => None,
    };
    let groups = match req
        .allowed_provider_groups
        .as_deref()
        .map(|g| checked_groups(g, "/allowed_provider_groups"))
    {
        Some(Err(found)) => {
            problems.extend(found);
            None
        }
        Some(Ok(groups)) => Some(groups),
        None => None,
    };
    let limits = match req.limits.as_ref().map(|v| limits_from(v, "/limits")) {
        Some(Err(found)) => {
            problems.extend(found);
            None
        }
        Some(Ok(limits)) => Some(limits),
        None => None,
    };
    // A list under `all` would be stored and grant nothing, which looks like
    // a selection and is not one: refused rather than ignored.
    if req.model_scope == Some(ModelScope::All) && req.allowed_model_ids.is_some() {
        problems.push(detail(
            "/allowed_model_ids",
            "范围是全部模型时不能同时指定模型列表；要指定列表，请把范围改为 selected",
        ));
    }
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    // A list is a selection; `all` is the only scope that ignores it. Naming
    // neither leaves the scope as it is.
    let scope = req.model_scope.or_else(|| {
        req.allowed_model_ids
            .is_some()
            .then_some(ModelScope::Selected)
    });
    let mut fields: Vec<&'static str> = [
        ("name", req.name.is_some()),
        ("billing_mode", req.billing_mode.is_some()),
        ("model_scope", req.model_scope.is_some()),
        ("allowed_model_ids", req.allowed_model_ids.is_some()),
        (
            "allowed_provider_groups",
            req.allowed_provider_groups.is_some(),
        ),
        ("limits", req.limits.is_some()),
    ]
    .into_iter()
    .filter_map(|(field, given)| given.then_some(field))
    .collect();
    fields.sort_unstable();
    // A billing mode change moves the balance mark, so it takes the billing
    // lock and the projection writes the marks. Every other field moves no
    // money and decides no mark: the tenant's row alone, and the marks stand.
    let billing = if req.billing_mode.is_some() {
        BillingStep::Run
    } else {
        BillingStep::Skip
    };
    let ctx = WriteCtx::of(&state);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let current = match billing {
                    BillingStep::Run => lock_billing(&mut *conn, id).await?.0,
                    BillingStep::Skip => lock(&mut *conn, id).await?,
                };
                if current.version != expected {
                    return Err(ApiError::version_mismatch(current.version));
                }
                // A move onto `all` waits for every catalogue write that
                // counted the all-scope tenants without this one, and holds
                // off the next until it commits ([`SCOPE_LOCK_KEY`]).
                if scope == Some(ModelScope::All) && current.model_scope != ModelScope::All {
                    sqlx::query("SELECT pg_advisory_xact_lock($1)")
                        .bind(SCOPE_LOCK_KEY)
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                }
                if groups.as_ref().is_some_and(Vec::is_empty) && !current.is_internal {
                    return Err(ApiError::validation(vec![no_groups(
                        "/allowed_provider_groups",
                    )]));
                }
                // Every group the body names must be live, taken `FOR SHARE`
                // here: after the tenant's lock and before the resources
                // lock a grant change takes, so an assignment and a retire
                // of the same group serialise on the group's row.
                if let Some(names) = &groups {
                    crate::groups::lock_named(&mut *conn, names, "/allowed_provider_groups")
                        .await?;
                }
                let grants = match (&req.allowed_model_ids, scope) {
                    (Some(raw), _) => {
                        resources_lock(&mut *conn).await?;
                        Some(checked_grants(&mut *conn, current.is_internal, raw).await?)
                    }
                    // Narrowing to `selected` starts from what the tenant
                    // reaches now, so an operator narrows rather than
                    // silently drops everything. The current set is read
                    // under the resources lock the grant write takes, so a
                    // public model created meanwhile is either in it or
                    // written after this commits.
                    (None, Some(ModelScope::Selected))
                        if current.model_scope == ModelScope::All =>
                    {
                        resources_lock(&mut *conn).await?;
                        let mut ids = scope_model_ids(&mut *conn, id).await?;
                        ids.sort();
                        Some(ids)
                    }
                    _ => None,
                };
                // Switching to `all` keeps the stored list as it is: it is
                // what the tenant returns to if it is narrowed again, and
                // nothing reads it while the scope is `all`.
                let scope = scope.map(ModelScope::as_str);
                let limits = limits
                    .map(|l| serde_json::to_value(&l))
                    .transpose()
                    .map_err(ApiError::internal)?;
                // Only what the body carries is written: a field left out
                // keeps what is stored, including limit fields a newer
                // release added that this one does not read.
                sqlx::query(
                    "UPDATE tenants SET name = COALESCE($2, name), \
                       billing_mode = COALESCE($3, billing_mode), \
                       model_scope = COALESCE($7, model_scope), \
                       allowed_model_ids = COALESCE($4, allowed_model_ids), \
                       allowed_provider_groups = COALESCE($5, allowed_provider_groups), \
                       limits = COALESCE($6, limits), version = version + 1 \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(name)
                .bind(req.billing_mode.map(BillingMode::as_str))
                .bind(grants)
                .bind(groups)
                .bind(limits)
                .bind(scope)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                project_with(&mut *conn, &ctx, id, billing).await?;
                audit::record(
                    &mut *conn,
                    audit_entry(&auth, "tenant.update", id, json!({ "fields": fields })),
                )
                .await?;
                let tenant = lock(&mut *conn, id).await?;
                view(conn, &ctx.env, ctx.stale_secs, tenant).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(view))
}

/// The state changes an operator makes: each moves one state to one other.
#[derive(Debug, Clone, Copy)]
enum Transition {
    Approve,
    Suspend,
    Reactivate,
}

impl Transition {
    fn from(self) -> TenantStatus {
        match self {
            Transition::Approve => TenantStatus::Pending,
            Transition::Suspend => TenantStatus::Active,
            Transition::Reactivate => TenantStatus::Suspended,
        }
    }

    fn action(self) -> &'static str {
        match self {
            Transition::Approve => "tenant.approve",
            Transition::Suspend => "tenant.suspend",
            Transition::Reactivate => "tenant.reactivate",
        }
    }

    /// The update, with the tenant's id as `$1` and, for an approval, the
    /// approving operator's as `$2`.
    fn sql(self) -> &'static str {
        match self {
            Transition::Approve => {
                "UPDATE tenants SET status = 'active', approved_at = now(), approved_by = $2, \
                   version = version + 1 WHERE id = $1"
            }
            Transition::Suspend => {
                "UPDATE tenants SET status = 'suspended', suspended_at = now(), \
                   version = version + 1 WHERE id = $1"
            }
            Transition::Reactivate => {
                "UPDATE tenants SET status = 'active', suspended_at = NULL, \
                   version = version + 1 WHERE id = $1"
            }
        }
    }
}

/// Moves tenant `id` along `t`, and its documents with it; `409
/// tenant_state` from any other state.
async fn transition(
    state: AppState,
    auth: OperatorAuth,
    id: &str,
    t: Transition,
) -> Result<Json<TenantView>, ApiError> {
    let id = tenant_id(id)?;
    let ctx = WriteCtx::of(&state);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                // The billing lock: the projection below writes the balance
                // mark before the keys open.
                let (current, _) = lock_billing(&mut *conn, id).await?;
                if current.is_internal && matches!(t, Transition::Suspend) {
                    return Err(ApiError::conflict(
                        "tenant_state",
                        "平台内部租户不能停用：停用会使全部运维密钥失效",
                    ));
                }
                if current.status != t.from() {
                    return Err(ApiError::conflict(
                        "tenant_state",
                        "租户当前的状态不能执行此操作",
                    ));
                }
                let mut update = sqlx::query(t.sql()).bind(id);
                if let Transition::Approve = t {
                    update = update.bind(auth.id);
                }
                update.execute(&mut *conn).await.map_err(DbError::from)?;
                project(&mut *conn, &ctx, id).await?;
                audit::record(&mut *conn, audit_entry(&auth, t.action(), id, json!({}))).await?;
                let tenant = lock(&mut *conn, id).await?;
                view(conn, &ctx.env, ctx.stale_secs, tenant).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(view))
}

/// `POST /admin/tenants/{id}/approve`: pending → active.
pub async fn approve(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TenantView>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    transition(state, auth, &id, Transition::Approve).await
}

/// `POST /admin/tenants/{id}/suspend`: active → suspended. The team stays;
/// the keys are disabled, and then the limits policy goes.
pub async fn suspend(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TenantView>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    transition(state, auth, &id, Transition::Suspend).await
}

/// `POST /admin/tenants/{id}/reactivate`: suspended → active, with the
/// limits policy written again under its old id.
pub async fn reactivate(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TenantView>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    transition(state, auth, &id, Transition::Reactivate).await
}

/// `GET /admin/settings/tenant-defaults` → `TenantDefaults`.
pub async fn get_defaults(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<TenantDefaults>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    Ok(Json(settings::tenant_defaults(&mut conn).await?))
}

/// `PUT /admin/settings/tenant-defaults` with a whole `TenantDefaults` →
/// `200 TenantDefaults`. Tenants created afterwards copy it; existing
/// tenants keep what they have.
pub async fn put_defaults(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<TenantDefaults>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let mut known = LIMIT_FIELDS.to_vec();
    known.push("default_provider_groups");
    let mut problems = unknown_fields(&body, &known, "");
    if problems.is_empty() && body.get("default_provider_groups").is_none() {
        problems.push(detail(
            "/default_provider_groups",
            "必须给出新租户的默认分组",
        ));
    }
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let mut defaults: TenantDefaults = serde_json::from_value(body).map_err(|_| {
        ApiError::validation(vec![detail("", "限制必须是正整数，分组必须是字符串列表")])
    })?;
    let mut problems = defaults.limits().problems("");
    match checked_groups(
        &defaults.default_provider_groups,
        "/default_provider_groups",
    ) {
        Ok(groups) if groups.is_empty() => problems.push(no_groups("/default_provider_groups")),
        Ok(groups) => defaults.default_provider_groups = groups,
        Err(found) => problems.extend(found),
    }
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let stored = defaults.clone();
    state
        .db
        .tx(move |conn| {
            async move {
                // Every group the defaults name must be live, taken `FOR
                // SHARE` in this transaction: the stored defaults are what
                // every tenant created afterwards copies.
                crate::groups::lock_named(
                    &mut *conn,
                    &stored.default_provider_groups,
                    "/default_provider_groups",
                )
                .await?;
                settings::set_tenant_defaults(&mut *conn, &stored).await?;
                let mut fields: Vec<&str> = stored
                    .limits()
                    .by_name()
                    .into_iter()
                    .filter_map(|(name, value)| value.map(|_| name))
                    .collect();
                fields.push("default_provider_groups");
                fields.sort_unstable();
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "settings.tenant_defaults",
                        target_kind: "settings",
                        target_id: Some("tenant_defaults".to_string()),
                        tenant_id: None,
                        detail: json!({ "fields": fields }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(())
            }
            .boxed()
        })
        .await?;
    Ok(Json(defaults))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(status: TenantStatus, limits: TenantLimits) -> TenantRow {
        TenantRow {
            id: Uuid::new_v4(),
            name: "order-7c1".to_string(),
            status,
            is_internal: false,
            billing_mode: BillingMode::Prepaid,
            model_scope: ModelScope::Selected,
            allowed_model_ids: Vec::new(),
            effective_model_ids: Vec::new(),
            allowed_provider_groups: vec!["shared".to_string()],
            limits,
            limits_policy_id: Some(Uuid::new_v4()),
            version: 1,
            created_at: Utc::now(),
            approved_at: None,
        }
    }

    fn at(steps: &[Step], wanted: fn(&Step) -> bool) -> usize {
        steps
            .iter()
            .position(wanted)
            .unwrap_or_else(|| panic!("missing from {steps:?}"))
    }

    /// The order `project` writes in: the tenant's limit is in place before
    /// its keys are opened, and lifted only after they are closed, so a
    /// projection that stalls between two writes never leaves a key enabled
    /// and unlimited.
    #[test]
    fn a_limit_is_in_place_before_the_keys_open_and_lifted_only_after_they_close() {
        let limited = TenantLimits {
            rpm: Some(60),
            ..TenantLimits::default()
        };
        let active = steps(&tenant(TenantStatus::Active, limited.clone()));
        assert!(matches!(active[0], Step::Team(_)), "{active:?}");
        assert!(
            at(&active, |s| *s == Step::Users) < at(&active, |s| *s == Step::Keys),
            "{active:?}"
        );
        assert!(
            at(&active, |s| matches!(s, Step::Policy(_))) < at(&active, |s| *s == Step::Keys),
            "{active:?}"
        );
        assert!(!active.contains(&Step::RetirePolicy), "{active:?}");

        for t in [
            tenant(TenantStatus::Suspended, limited.clone()),
            tenant(TenantStatus::Active, TenantLimits::default()),
        ] {
            let closing = steps(&t);
            assert!(matches!(closing[0], Step::Team(_)), "{closing:?}");
            assert!(
                at(&closing, |s| *s == Step::Users) < at(&closing, |s| *s == Step::Keys),
                "{closing:?}"
            );
            assert!(
                at(&closing, |s| *s == Step::Keys) < at(&closing, |s| *s == Step::RetirePolicy),
                "{closing:?}"
            );
            assert!(
                !closing.iter().any(|s| matches!(s, Step::Policy(_))),
                "{closing:?}"
            );
        }
    }

    /// The balance mark is written right before the keys whether they open
    /// or close, so a tenant out of money is blocked before a key of it
    /// opens.
    #[test]
    fn the_balance_mark_is_written_right_before_the_keys() {
        let limited = TenantLimits {
            rpm: Some(60),
            ..TenantLimits::default()
        };
        for t in [
            tenant(TenantStatus::Active, limited.clone()),
            tenant(TenantStatus::Active, TenantLimits::default()),
            tenant(TenantStatus::Suspended, limited),
        ] {
            let steps = steps(&t);
            assert_eq!(
                at(&steps, |s| *s == Step::Billing) + 1,
                at(&steps, |s| *s == Step::Keys),
                "{steps:?}"
            );
            assert!(
                at(&steps, |s| *s == Step::Users) < at(&steps, |s| *s == Step::Billing),
                "{steps:?}"
            );
        }
    }

    /// A tenant waiting for approval gives the gateways nothing: signup is
    /// anonymous, and what it creates must not grow what every gateway
    /// loads.
    #[test]
    fn a_pending_tenant_writes_nothing() {
        let limited = TenantLimits {
            rpm: Some(60),
            ..TenantLimits::default()
        };
        for limits in [limited, TenantLimits::default()] {
            assert_eq!(steps(&tenant(TenantStatus::Pending, limits)), []);
        }
    }
}
