//! Tenants: the platform's customers, as operators manage them.
//!
//! The gateways see a tenant as a team, `teams/<tenant id>`, named after it.
//! A tenant reaches the global model catalog only through its grants
//! (`allowed_model_ids`). Its default protection (`limits`) is projected,
//! while it is active, as one conditional rate-limit policy over the team,
//! written under an id minted once (`limits_policy_id`). Both documents are
//! the tenant's own (`managed_by = "tenant:<id>"`), so only the code here
//! changes them.
//!
//! Every tenant-scoped read-then-write starts by locking the tenant rows
//! ([`lock`], [`lock_many`]) and only then reaches `write_resource`, which
//! takes the resources lock: the lock order everywhere.
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
use crate::db::{Db, DbError};
use crate::error::{ApiError, BootError, Detail};
use crate::http::ApiJson;
use crate::resources::api::if_match;
use crate::resources::store::{self, ResourceRow};
use crate::resources::write::{
    write_resource, ResourceWrite, WriteCtx, WriteOp, RESOURCES_LOCK_KEY,
};
use crate::resources::Kind;
use crate::rollout::{self, ResourceRollout};
use crate::settings::{self, TenantDefaults, BOOT_LOCK_KEY};
use crate::AppState;

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

    fn parse(s: &str) -> Option<TenantStatus> {
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

    fn parse(s: &str) -> Option<BillingMode> {
        [BillingMode::Prepaid, BillingMode::Postpaid]
            .into_iter()
            .find(|mode| mode.as_str() == s)
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
    pub allowed_model_ids: Vec<Uuid>,
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
    Vec<Uuid>,
    Vec<String>,
    Value,
    Option<Uuid>,
    i64,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

const COLUMNS: &str = "id, name, status, is_internal, billing_mode, allowed_model_ids, \
     allowed_provider_groups, limits, limits_policy_id, version, created_at, approved_at";

impl TenantRow {
    fn from_raw(raw: RawTenant) -> Result<TenantRow, ApiError> {
        let (
            id,
            name,
            status,
            is_internal,
            billing_mode,
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
            allowed_model_ids,
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

/// Tenant `id`, locked for the rest of the transaction; `404 not_found`
/// when there is none. Locking a row the transaction already holds is a
/// no-op.
pub async fn lock(conn: &mut PgConnection, id: Uuid) -> Result<TenantRow, ApiError> {
    let raw: Option<RawTenant> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM tenants WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?;
    TenantRow::from_raw(raw.ok_or_else(ApiError::not_found)?)
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

async fn insert(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    new: NewTenant<'_>,
) -> Result<TenantRow, ApiError> {
    let id = Uuid::new_v4();
    let limits = serde_json::to_value(new.limits).map_err(ApiError::internal)?;
    sqlx::query(
        "INSERT INTO tenants (id, name, status, is_internal, billing_mode, \
           allowed_provider_groups, limits, approved_at, approved_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, \
           CASE WHEN $3 = 'active' THEN now() END, $8)",
    )
    .bind(id)
    .bind(new.name)
    .bind(new.status.as_str())
    .bind(new.is_internal)
    .bind(new.billing_mode.as_str())
    .bind(new.groups)
    .bind(limits)
    .bind(new.approved_by)
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;
    project(&mut *conn, ctx, id).await?;
    lock(conn, id).await
}

/// Creates an ordinary tenant in `status`, with a copy of the current
/// tenant defaults (its limits and its provider groups, ruling 27), and
/// writes its documents. An active tenant counts as approved by `actor`.
/// Recording it in the audit log is the caller's: signup and the operator
/// API record it differently.
pub async fn create(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    name: &str,
    status: TenantStatus,
    billing_mode: BillingMode,
    actor: Actor<'_>,
) -> Result<TenantRow, ApiError> {
    let defaults = settings::tenant_defaults(&mut *conn).await?;
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

/// One write [`project`] makes.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// The tenant's `teams` document.
    Team(Value),
    /// The limits policy, written (or revived) with this document.
    Policy(Value),
    /// The tenant's keys, re-projected from its row.
    Keys,
    /// The limits policy deleted, if it is live.
    RetirePolicy,
}

/// The writes [`project`] makes for `tenant`, in the order it makes them.
///
/// The projector writes kine in commit order and stops at the first write
/// it cannot make, so a projection can stall between any two of these. The
/// order is therefore chosen so that no stall leaves a key enabled and
/// unlimited: the tenant's limit is in place before its keys are opened (an
/// approval or a reactivation enables them), and is lifted only after they
/// are closed (a suspension disables them). The team comes first, since the
/// keys name it.
fn steps(tenant: &TenantRow) -> Vec<Step> {
    let mut steps = vec![Step::Team(team_doc(&tenant.name))];
    let policy = match tenant.status {
        TenantStatus::Active => limits_policy_doc(tenant.id, &tenant.limits),
        TenantStatus::Pending | TenantStatus::Suspended => None,
    };
    match policy {
        Some(doc) => steps.extend([Step::Policy(doc), Step::Keys]),
        None => steps.extend([Step::Keys, Step::RetirePolicy]),
    }
    steps
}

/// Writes the documents tenant `id` derives, as its row now stands, in the
/// order [`steps`] gives: the team, its keys, and its limits policy, which
/// is written while the tenant is active and limits it tenant-wide and
/// deleted otherwise. A document already as it should be is not written
/// again. The caller holds the tenant's lock.
pub async fn project(conn: &mut PgConnection, ctx: &WriteCtx, id: Uuid) -> Result<(), ApiError> {
    let tenant = lock(&mut *conn, id).await?;
    let owner = owner(id);
    for step in steps(&tenant) {
        match step {
            Step::Team(doc) => put(&mut *conn, ctx, Kind::Teams, id, &doc, &owner).await?,
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
            // Task 12: `keys::reproject_tenant(conn, ctx, id)` goes here, and
            // nowhere else, so that the limit brackets the keys as `steps`
            // describes.
            Step::Keys => {}
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
async fn put(
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
async fn retire(
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
/// first boot; later boots find it and bring its documents up to date.
/// The same boot writes the first tenant defaults (ruling 14).
///
/// Runs under the boot lock, so instances booting together create it once;
/// the unique index `tenants_one_internal` holds that even without the lock.
pub async fn ensure_internal_tenant(db: &Db, ctx: &WriteCtx) -> Result<TenantRow, BootError> {
    let ctx = WriteCtx {
        mk: crate::crypto::MasterKey(ctx.mk.0),
        env: ctx.env.clone(),
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
                    project(&mut *conn, &ctx, id).await?;
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
                            billing_mode: BillingMode::Prepaid,
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

/// The tenants whose grants include `model_id`, in id order.
async fn holders_of(conn: &mut PgConnection, model_id: Uuid) -> Result<Vec<Uuid>, ApiError> {
    Ok(
        sqlx::query_scalar("SELECT id FROM tenants WHERE $1 = ANY(allowed_model_ids) ORDER BY id")
            .bind(model_id)
            .fetch_all(conn)
            .await
            .map_err(DbError::from)?,
    )
}

/// Withdraws `model_id` from every tenant grant and re-projects each tenant
/// it was withdrawn from, ahead of the model's own delete in the same
/// transaction (ruling 13). Runs before `write_resource` deletes the model,
/// so the tenant locks are taken before the resources lock.
///
/// A grant is only ever added under the resources lock (see [`update`]),
/// and the holders are looked up before that lock can be taken. So the
/// holders are looked up again once it is held: one that appeared in
/// between, which cannot be locked now without breaking the lock order,
/// fails the delete as `503 unavailable`, to be retried.
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
             WHERE id = $1",
        )
        .bind(id)
        .bind(model_id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
        project(&mut *conn, ctx, id).await?;
    }
    Ok(())
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
fn tenant_name(raw: &str) -> Result<String, Detail> {
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
/// name that is blank, starts or ends with whitespace, or is longer than 64
/// characters. Names are matched exactly against provider keys' groups, so
/// a stray space would match nothing.
fn checked_groups(groups: &[String], at: &str) -> Result<Vec<String>, Vec<Detail>> {
    let problems: Vec<Detail> = groups
        .iter()
        .enumerate()
        .filter_map(|(i, group)| {
            let path = format!("{at}/{i}");
            if group.trim().is_empty() {
                Some(detail(&path, "分组名称不能为空"))
            } else if group.trim() != group {
                Some(detail(&path, "分组名称前后不能有空白"))
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
fn no_groups(at: &str) -> Detail {
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
    allowed_model_ids: Vec<Uuid>,
    allowed_provider_groups: Vec<String>,
    limits: TenantLimits,
    /// Members and keys arrive with the member and key tasks; until then a
    /// tenant has none.
    member_count: i64,
    key_count: i64,
    version: i64,
    created_at: DateTime<Utc>,
    approved_at: Option<DateTime<Utc>>,
    /// The worst rollout among the documents the tenant derives.
    rollout: ResourceRollout,
}

/// The views of `tenants`, each with the rollout of its documents (live
/// ones, and deletes still on their way).
async fn views(
    conn: &mut PgConnection,
    tenants: Vec<TenantRow>,
) -> Result<Vec<TenantView>, ApiError> {
    let owners: Vec<String> = tenants.iter().map(|t| owner(t.id)).collect();
    let mut rows: BTreeMap<String, Vec<ResourceRow>> = BTreeMap::new();
    for row in store::owned_by(conn, &owners).await? {
        if let Some(owner) = row.managed_by.clone() {
            rows.entry(owner).or_default().push(row);
        }
    }
    Ok(tenants
        .into_iter()
        .map(|t| {
            let states: Vec<ResourceRollout> = rows
                .get(&owner(t.id))
                .map(|rows| rows.iter().map(ResourceRollout::of_row).collect())
                .unwrap_or_default();
            TenantView {
                id: t.id,
                name: t.name,
                status: t.status,
                is_internal: t.is_internal,
                billing_mode: t.billing_mode,
                allowed_model_ids: t.allowed_model_ids,
                allowed_provider_groups: t.allowed_provider_groups,
                limits: t.limits,
                member_count: 0,
                key_count: 0,
                version: t.version,
                created_at: t.created_at,
                approved_at: t.approved_at,
                rollout: rollout::worst(&states),
            }
        })
        .collect())
}

async fn view(conn: &mut PgConnection, tenant: TenantRow) -> Result<TenantView, ApiError> {
    views(conn, vec![tenant])
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
    let items = views(&mut conn, tenants).await?;
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
    let view = view(&mut conn, tenant).await?;
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
                view(conn, tenant).await
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
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let mut fields: Vec<&'static str> = [
        ("name", req.name.is_some()),
        ("billing_mode", req.billing_mode.is_some()),
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
    let ctx = WriteCtx::of(&state);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let current = lock(&mut *conn, id).await?;
                if current.version != expected {
                    return Err(ApiError::version_mismatch(current.version));
                }
                if groups.as_ref().is_some_and(Vec::is_empty) && !current.is_internal {
                    return Err(ApiError::validation(vec![no_groups(
                        "/allowed_provider_groups",
                    )]));
                }
                let grants = match &req.allowed_model_ids {
                    Some(raw) => {
                        resources_lock(&mut *conn).await?;
                        Some(checked_grants(&mut *conn, current.is_internal, raw).await?)
                    }
                    None => None,
                };
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
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                project(&mut *conn, &ctx, id).await?;
                audit::record(
                    &mut *conn,
                    audit_entry(&auth, "tenant.update", id, json!({ "fields": fields })),
                )
                .await?;
                let tenant = lock(&mut *conn, id).await?;
                view(conn, tenant).await
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
                let current = lock(&mut *conn, id).await?;
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
                view(conn, tenant).await
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
/// the limits policy goes, and the keys stop working (the key task).
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

/// `GET /admin/provider-groups` → `{items: [{name, provider_keys: [{id,
/// display_name}]}]}`: every group a live provider key is in, by name, each
/// with its provider keys by display name.
pub async fn provider_groups(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let mut groups: BTreeMap<String, Vec<(String, Uuid)>> = BTreeMap::new();
    for row in store::list(&mut conn, Kind::ProviderKeys).await? {
        let name = row
            .doc
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let members = row
            .doc
            .get("groups")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str);
        for group in members {
            groups
                .entry(group.to_string())
                .or_default()
                .push((name.clone(), row.id));
        }
    }
    let items: Vec<Value> = groups
        .into_iter()
        .map(|(group, mut keys)| {
            keys.sort();
            keys.dedup();
            let keys: Vec<Value> = keys
                .into_iter()
                .map(|(display_name, id)| json!({"id": id, "display_name": display_name}))
                .collect();
            json!({"name": group, "provider_keys": keys})
        })
        .collect();
    Ok(Json(json!({ "items": items })))
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
            allowed_model_ids: Vec::new(),
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
            at(&active, |s| matches!(s, Step::Policy(_))) < at(&active, |s| *s == Step::Keys),
            "{active:?}"
        );
        assert!(!active.contains(&Step::RetirePolicy), "{active:?}");

        for t in [
            tenant(TenantStatus::Suspended, limited.clone()),
            tenant(TenantStatus::Pending, limited),
            tenant(TenantStatus::Active, TenantLimits::default()),
        ] {
            let closing = steps(&t);
            assert!(matches!(closing[0], Step::Team(_)), "{closing:?}");
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
}
