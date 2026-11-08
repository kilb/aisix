//! Provider groups as data: the routing name a provider key and a key
//! document carry (immutable, operator-only), the display name and
//! description tenants see, and the group rates it charges
//! ([`crate::pricing::rates`]).
//!
//! A group is never deleted. Retiring one refuses `409 group_in_use` while
//! any tenant, the tenant defaults, a live key's group or override, or a
//! live provider key still names it, and the answer counts each. A retired
//! group accepts no rate, leaves every picker, and keeps its row and its
//! name for the history that references it.
//!
//! At most one group is designated the default (`is_default`): the one an
//! upgrade gives every internal key still without a group. It cannot be
//! retired while it is designated, and `make-default` moves the flag.
//!
//! # Locks
//!
//! The lock order is tenant row, then its billing account, then group rows
//! (ascending id), then `RESOURCES_LOCK_KEY` (design D20). A path that names
//! a group takes its row `FOR SHARE` ([`lock_one`], as a rate write does);
//! `make-default` takes it `FOR UPDATE`, so two designations serialise.
//!
//! A retire takes its group row `FOR UPDATE` and then also
//! `RESOURCES_LOCK_KEY`, and counts the references under both. That closes
//! the two ways a name could land after the count:
//! - a tenant, a key or a rate naming the group waits on the row itself (a
//!   key's own `group_id` row is not locked, but assigning a group to a
//!   tenant and writing a rate both are, and a key is created under its
//!   tenant's row);
//! - a provider-key document is written under `RESOURCES_LOCK_KEY` only, so
//!   the retire holds that lock for its count and its update. A provider-key
//!   write that gets the lock second is refused at `/groups` for naming a
//!   retired group ([`crate::resources::write`]); one that gets it first has
//!   committed before the count reads it, and the retire refuses with
//!   `group_in_use`.
//!
//! So a group is never retired while a live provider key names it, and a
//! provider key never names a retired group. Task 12's provider-key write
//! additionally takes its groups `FOR SHARE` before the resources lock,
//! which orders the two writes' waits on the group row rather than on the
//! resources lock; the invariant holds with either lock order.
//!
//! # References are counted, never copied
//!
//! Tenants and key overrides hold the group's name in a text array, and a
//! provider key's document holds it in `groups`; [`references`] is how a
//! retire learns whether anything still names the group. A key may name it
//! by id instead, in `tenant_keys.group_id`, which the same count reads.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::keys::KeyGroup;
use crate::pricing::rates::{self, NewGroupRate, RatePlan};
use crate::pricing::store;
use crate::resources::{refs, Kind};
use crate::tenants::{self, TenantRow};
use crate::AppState;

/// The longest a routing name may be, in characters: the same bound every
/// group name in a tenant's list has (`tenants::MAX_GROUP_CHARS`).
const MAX_NAME_CHARS: usize = 64;

/// The most characters a display name and a description take.
const MAX_DISPLAY_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 500;

/// The group `raw` names; `404` for an id that names nothing, as every id in
/// a path does.
fn group_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::not_found())
}

/// The one refusal every `/group_id` a tenant may not name shares: an id
/// that names nothing to it, a retired group and another tenant's group are
/// the same answer, so nothing leaks which groups exist.
fn group_unavailable() -> ApiError {
    store::refused("/group_id", "分组不存在或不可用")
}

/// The group a key of tenant `t` should name, given the `group_id` its body
/// carries (`None` when it left it out). Takes the group's row `FOR SHARE`,
/// so the caller must hold `t`'s lock and must call this before
/// `write_resource`, as [`crate::keys::create_key`] does.
///
/// - An id: the live group it names. For a customer it must be one of `t`'s
///   assigned groups; the platform's own tenant may name any live group.
/// - Omitted: the one live group `t` is assigned, when it is assigned
///   exactly one. A customer with several, and the platform's own tenant,
///   which has no assignment list, must choose.
///
/// Refused `422 /group_id`: "分组不存在或不可用" for an id that names
/// nothing, a retired group or another tenant's, and "请选择分组" when the
/// id was omitted and no single group applies.
pub async fn key_group(
    conn: &mut PgConnection,
    t: &TenantRow,
    raw: Option<&str>,
) -> Result<KeyGroup, ApiError> {
    let Some(raw) = raw else {
        let only = match t.allowed_provider_groups.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        };
        // The platform's own tenant has no assignment list, so it never has
        // a single group to fall back on: its keys always choose.
        let only = match only {
            Some(name) if !t.is_internal => Some(name),
            _ => None,
        };
        let Some(name) = only else {
            return Err(store::refused("/group_id", "请选择分组"));
        };
        let row: Option<GroupRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM provider_groups \
             WHERE name = $1 AND retired_at IS NULL FOR SHARE"
        ))
        .bind(&name)
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::from)?;
        let row = row.ok_or_else(group_unavailable)?;
        return Ok(row.key_group());
    };
    let id = Uuid::parse_str(raw).map_err(|_| group_unavailable())?;
    let row = sqlx::query_as::<_, GroupRow>(&format!(
        "SELECT {COLUMNS} FROM provider_groups WHERE id = $1 FOR SHARE"
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?
    .ok_or_else(group_unavailable)?;
    if row.retired_at.is_some() {
        return Err(group_unavailable());
    }
    if !t.is_internal && !t.allowed_provider_groups.contains(&row.name) {
        return Err(group_unavailable());
    }
    Ok(row.key_group())
}

impl GroupRow {
    /// The group as a key records it: its id, its routing name and what a
    /// tenant sees.
    fn key_group(&self) -> KeyGroup {
        KeyGroup {
            id: self.id,
            name: self.name.clone(),
            display_name: self.display_name.clone(),
        }
    }
}

/// `GET /api/groups` → `{items: [{id, display_name, description}]}`: the
/// live groups `t` assigns, by display name, which is what a tenant chooses
/// its keys' group from. Never a routing `name`, and never a group its tenant
/// is not assigned.
pub async fn tenant_groups(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ])?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let tenant = tenants::find(&mut conn, auth.tenant_id).await?;
    let items: Vec<Value> = groups_of(&mut conn, &tenant.allowed_provider_groups)
        .await?
        .into_iter()
        .map(|row| {
            json!({"id": row.id, "display_name": row.display_name, "description": row.description})
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// The live groups `names` name, each locked `FOR SHARE` in ascending id
/// order; `422` at `{at}/{i}` for every name that is not one.
///
/// This is the one check behind "every write that names a group names a
/// live one": a name with no row and a retired group are the same refusal,
/// so nothing leaks which groups exist. The share lock is what makes an
/// assignment and a retire of the same group serialise — the retire takes
/// the row `FOR UPDATE`, so it lands either before (and the assigner then
/// finds the group retired) or after (and then counts the assignment).
///
/// Runs after any tenant lock and before [`crate::resources::write::
/// write_resource`], which takes `RESOURCES_LOCK_KEY`: a provider-key write
/// then waits on the group row rather than on the resources lock, and a
/// retire, which takes both in that order, cannot be overtaken through the
/// resources lock.
pub async fn lock_named(
    conn: &mut PgConnection,
    names: &[String],
    at: &str,
) -> Result<Vec<GroupRow>, ApiError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<GroupRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM provider_groups \
         WHERE name = ANY($1) AND retired_at IS NULL ORDER BY id FOR SHARE"
    ))
    .bind(names)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let found: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    let problems: Vec<Detail> = names
        .iter()
        .enumerate()
        .filter(|(_, name)| !found.contains(&name.as_str()))
        .map(|(i, _)| Detail {
            path: format!("{at}/{i}"),
            message: "分组不存在或已停用".to_string(),
        })
        .collect();
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    Ok(rows)
}

/// `GET /admin/provider-groups` → `{items: [Group]}`: every live group, by
/// name, including ones no upstream account is in. A retired group is not
/// listed.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let rows: Vec<GroupRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM provider_groups WHERE retired_at IS NULL ORDER BY name, id"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(view_json(&mut conn, &row).await?);
    }
    Ok(Json(json!({ "items": items })))
}

/// The live groups `names` assigns, by display name then id.
async fn groups_of(conn: &mut PgConnection, names: &[String]) -> Result<Vec<GroupRow>, DbError> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM provider_groups \
         WHERE name = ANY($1) AND retired_at IS NULL ORDER BY display_name, id"
    ))
    .bind(names)
    .fetch_all(conn)
    .await
    .map_err(DbError::from)
}

/// The decimal-string message a rate or a description of one is refused
/// with.
const RATE_MESSAGE: &str = "换算率应为 0 到 100 之间、最多四位小数的数字";

/// One `provider_groups` row.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct GroupRow {
    pub id: Uuid,
    /// The routing name: immutable, never shown to tenants.
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// The designated default group: the one an upgrade gives every internal
    /// key still without a group.
    pub is_default: bool,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

/// How a path takes a group row: `Share` to name it, `Update` to change or
/// retire it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Share,
    Update,
}

/// The group `id`, locked as `mode` says; `404` when no group has that id.
pub async fn lock_one(
    conn: &mut PgConnection,
    id: Uuid,
    mode: LockMode,
) -> Result<GroupRow, ApiError> {
    let lock = match mode {
        LockMode::Share => "FOR SHARE",
        LockMode::Update => "FOR UPDATE",
    };
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM provider_groups WHERE id = $1 {lock}"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?
    .ok_or_else(ApiError::not_found)
}

/// The group `id` as it stands, taking no lock.
async fn read_one(conn: &mut PgConnection, id: Uuid) -> Result<GroupRow, ApiError> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM provider_groups WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?
    .ok_or_else(ApiError::not_found)
}

/// The columns a [`GroupRow`] is read from.
const COLUMNS: &str =
    "id, name, display_name, description, is_default, created_by, created_at, retired_at";

/// What still names a group: how many of each. Every count must be 0 before
/// the group can be retired.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct GroupUse {
    /// Tenants whose `allowed_provider_groups` names it.
    pub tenants: i64,
    /// 1 when the stored tenant defaults name it, else 0.
    pub defaults: i64,
    /// Live keys that name it: a key's `group_id`, or its own override. A
    /// key with neither counts for nothing, so an absent override never
    /// reads as naming the group.
    pub keys: i64,
    /// Live provider-key documents that list it.
    pub provider_keys: i64,
    /// 1 when it is the designated default group, else 0.
    pub default_group: i64,
}

impl GroupUse {
    /// Whether anything still names the group.
    #[must_use]
    pub fn any(self) -> bool {
        [
            self.tenants,
            self.defaults,
            self.keys,
            self.provider_keys,
            self.default_group,
        ]
        .into_iter()
        .any(|count| count > 0)
    }
}

/// What still names `g`, read on `conn`: a retire's own `FOR UPDATE` lock on
/// the row is what keeps these counts true until its update.
pub async fn references(conn: &mut PgConnection, g: &GroupRow) -> Result<GroupUse, DbError> {
    let name = &g.name;
    let tenants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenants WHERE $1::text = ANY(allowed_provider_groups)",
    )
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    // One `tenant_defaults` row at most, so the count is 0 or 1; a database
    // that has stored none resolves to the first-boot defaults, so nothing
    // names the group there.
    let defaults: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cp_settings \
         WHERE key = 'tenant_defaults' \
           AND jsonb_typeof(value->'default_provider_groups') = 'array' \
           AND value->'default_provider_groups' ? $1",
    )
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    let keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenant_keys \
         WHERE revoked_at IS NULL \
           AND (group_id = $2 OR $1::text = ANY(allowed_provider_groups))",
    )
    .bind(name)
    .bind(g.id)
    .fetch_one(&mut *conn)
    .await?;
    let provider_keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources \
         WHERE kind = 'provider_keys' AND NOT deleted AND doc->'groups' ? $1",
    )
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    Ok(GroupUse {
        tenants,
        defaults,
        keys,
        provider_keys,
        default_group: i64::from(g.is_default),
    })
}

/// One live provider key that lists the group, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderKeyRef {
    pub id: Uuid,
    pub display_name: String,
}

/// A group as the API answers it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupView {
    pub id: Uuid,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub is_default: bool,
    pub default_rate: RatePlan,
    pub tenants: i64,
    pub defaults: i64,
    pub keys: i64,
    /// The live provider keys that list it, by display name.
    pub provider_keys: Vec<ProviderKeyRef>,
    pub retired_at: Option<DateTime<Utc>>,
}

/// The live provider keys that list `name`, by display name.
async fn provider_keys_of(
    conn: &mut PgConnection,
    name: &str,
) -> Result<Vec<ProviderKeyRef>, DbError> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, COALESCE(doc->>'display_name', '') FROM gw_resources \
         WHERE kind = $1 AND NOT deleted AND doc->'groups' ? $2 \
         ORDER BY 2, 1",
    )
    .bind(Kind::ProviderKeys.segment())
    .bind(name)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, display_name)| ProviderKeyRef { id, display_name })
        .collect())
}

/// The API's `Group`: the row, what still names it, its provider keys and
/// its default rate plan.
pub async fn view(conn: &mut PgConnection, row: &GroupRow) -> Result<GroupView, ApiError> {
    let uses = references(&mut *conn, row).await?;
    let default_rate = rates::rate_plan(&mut *conn, row.id, None).await?;
    let provider_keys = provider_keys_of(&mut *conn, &row.name).await?;
    Ok(GroupView {
        id: row.id,
        name: row.name.clone(),
        display_name: row.display_name.clone(),
        description: row.description.clone(),
        is_default: row.is_default,
        default_rate,
        tenants: uses.tenants,
        defaults: uses.defaults,
        keys: uses.keys,
        provider_keys,
        retired_at: row.retired_at,
    })
}

/// The answer body of the group routes.
async fn view_json(conn: &mut PgConnection, row: &GroupRow) -> Result<Value, ApiError> {
    serde_json::to_value(view(&mut *conn, row).await?).map_err(ApiError::internal)
}

/// Whether `name` is a routing name a new group may take:
/// `^[a-z0-9][a-z0-9_-]{0,63}$`.
fn name_is_well_formed(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    name.chars().count() <= MAX_NAME_CHARS
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A display name as given: 1 to 64 characters.
fn checked_display_name(raw: &str) -> Result<String, Detail> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(Detail {
            path: "/display_name".to_string(),
            message: "显示名称不能为空".to_string(),
        });
    }
    if name.chars().count() > MAX_DISPLAY_CHARS {
        return Err(Detail {
            path: "/display_name".to_string(),
            message: format!("显示名称不能超过 {MAX_DISPLAY_CHARS} 个字符"),
        });
    }
    Ok(name.to_string())
}

/// A description as given: at most 500 characters.
fn checked_description(raw: &str) -> Result<String, Detail> {
    if raw.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(Detail {
            path: "/description".to_string(),
            message: format!("说明最多 {MAX_DESCRIPTION_CHARS} 个字符"),
        });
    }
    Ok(raw.trim().to_string())
}

/// A rate as given, at `path`: a decimal string of at most four decimals, 0
/// to 100. `null`, an absent field and a JSON number are all refused, so the
/// two routes that take a rate answer the same input the same way.
fn checked_rate(path: &str, raw: Option<&Value>) -> Result<aisix_pricing::CnyPerUsd, Detail> {
    raw.and_then(Value::as_str)
        .and_then(|s| aisix_pricing::CnyPerUsd::parse(s).ok())
        .ok_or_else(|| Detail {
            path: path.to_string(),
            message: RATE_MESSAGE.to_string(),
        })
}

/// A field's value when the body gave it, `None` when it was absent: so
/// `null` and an absent field differ.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `POST /admin/provider-groups`: a routing name, what tenants see, and the
/// group's default rate.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    name: String,
    display_name: String,
    #[serde(default)]
    description: Option<String>,
    /// A decimal string: a JSON number is refused, as everywhere a rate is
    /// typed.
    default_cny_per_usd: Value,
    #[serde(default)]
    effective_from: Option<DateTime<Utc>>,
}

/// `PATCH /admin/provider-groups/{id}`: what tenants see. A field it does
/// not give, or gives as `null`, stays as it is.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateBody {
    #[serde(default, deserialize_with = "present")]
    display_name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    description: Option<Value>,
}

/// `POST /admin/provider-groups` → `201 Group`: the group and its default
/// rate, in one transaction, audited as `provider_group.create` and
/// `pricing.group_rate.create`.
///
/// The default rate creating a group takes no `note`: it is the rate the
/// group starts at, not a hand-entered revision, and a later one may be
/// written with its own note through `POST /admin/provider-groups/{id}/rates`.
pub async fn create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let CreateBody {
        name,
        display_name,
        description,
        default_cny_per_usd,
        effective_from,
    } = body;
    let mut problems: Vec<Detail> = Vec::new();
    if !name_is_well_formed(&name) {
        problems.push(Detail {
            path: "/name".to_string(),
            message: "分组名称须为 1 到 64 个字符，只含小写字母、数字、下划线和连字符，并且以字母或数字开头"
                .to_string(),
        });
    }
    let display_name = match checked_display_name(&display_name) {
        Ok(name) => Some(name),
        Err(d) => {
            problems.push(d);
            None
        }
    };
    let description = match description.as_deref().map(checked_description).transpose() {
        Ok(description) => Some(description.unwrap_or_default()),
        Err(d) => {
            problems.push(d);
            None
        }
    };
    let rate = match checked_rate("/default_cny_per_usd", Some(&default_cny_per_usd)) {
        Ok(rate) => Some(rate),
        Err(d) => {
            problems.push(d);
            None
        }
    };
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let (display_name, description, rate) = (
        display_name.expect("a display name, the problems being empty"),
        description.expect("a description, the problems being empty"),
        rate.expect("a rate, the problems being empty"),
    );
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                let id: Uuid = sqlx::query_scalar(
                    "INSERT INTO provider_groups (name, display_name, description, created_by) \
                     VALUES ($1, $2, $3, $4) RETURNING id",
                )
                .bind(&name)
                .bind(&display_name)
                .bind(&description)
                .bind(Some(auth.id))
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| match &e {
                    // The routing name is unique, retired groups included: a
                    // retired group never frees its name.
                    sqlx::Error::Database(db)
                        if db.code().as_deref() == Some("23505")
                            && db.constraint() == Some("provider_groups_name_key") =>
                    {
                        store::refused("/name", "分组名称已存在")
                    }
                    _ => ApiError::from(DbError::from(e)),
                })?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "provider_group.create",
                        target_kind: "provider_group",
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: json!({"name": name, "display_name": display_name}),
                    },
                )
                .await?;
                // The group's default rate, in the same transaction: a group
                // never exists without one.
                let effective_from = match effective_from {
                    Some(at) => at,
                    None => sqlx::query_scalar("SELECT now()")
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(DbError::from)?,
                };
                rates::insert_group_rate(
                    &mut *conn,
                    &NewGroupRate {
                        group_id: id,
                        model_id: None,
                        effective_from,
                        cny_per_usd: Some(rate),
                        note: String::new(),
                    },
                    Actor::Operator(&auth),
                )
                .await?;
                let row = read_one(&mut *conn, id).await?;
                view_json(&mut *conn, &row).await
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(answer)))
}

/// `PATCH /admin/provider-groups/{id}` → `200 Group`: the display name and
/// the description, audited as `provider_group.update` when either changed.
pub async fn update(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(body): ApiJson<UpdateBody>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = group_id(&raw)?;
    let mut problems: Vec<Detail> = Vec::new();
    let display_name = match body.display_name {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => match checked_display_name(&raw) {
            Ok(name) => Some(name),
            Err(d) => {
                problems.push(d);
                None
            }
        },
        Some(_) => {
            problems.push(Detail {
                path: "/display_name".to_string(),
                message: "显示名称必须是字符串".to_string(),
            });
            None
        }
    };
    let description = match body.description {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => match checked_description(&raw) {
            Ok(description) => Some(description),
            Err(d) => {
                problems.push(d);
                None
            }
        },
        Some(_) => {
            problems.push(Detail {
                path: "/description".to_string(),
                message: "说明必须是字符串".to_string(),
            });
            None
        }
    };
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let mut fields: Vec<&str> = Vec::new();
    if display_name.is_some() {
        fields.push("display_name");
    }
    if description.is_some() {
        fields.push("description");
    }
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                let row = lock_one(&mut *conn, id, LockMode::Update).await?;
                if !fields.is_empty() {
                    sqlx::query(
                        "UPDATE provider_groups SET \
                           display_name = COALESCE($2, display_name), \
                           description = COALESCE($3, description) \
                         WHERE id = $1",
                    )
                    .bind(id)
                    .bind(display_name)
                    .bind(description)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                    audit::record(
                        &mut *conn,
                        AuditEntry {
                            actor: Actor::Operator(&auth),
                            action: "provider_group.update",
                            target_kind: "provider_group",
                            target_id: Some(id.to_string()),
                            tenant_id: None,
                            detail: json!({"fields": fields, "name": row.name}),
                        },
                    )
                    .await?;
                }
                let row = read_one(&mut *conn, id).await?;
                view_json(&mut *conn, &row).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `POST /admin/provider-groups/{id}/retire` → `200 Group`: sets
/// `retired_at` once nothing names the group; `409 group_in_use` with the
/// counts while something does. Retiring a retired group changes nothing and
/// answers its view.
///
/// The counts are read under the group's own `FOR UPDATE` lock and under
/// `RESOURCES_LOCK_KEY`, and the update lands while both are held. A
/// provider-key write waits on the same resources lock: one that gets it
/// first is counted, and one that gets it second is refused at `/groups`
/// because the group is retired by then
/// (`a_provider_key_write_and_a_retire_never_end_with_a_retired_group_named`).
pub async fn retire(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(_): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = group_id(&raw)?;
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                let row = lock_one(&mut *conn, id, LockMode::Update).await?;
                if row.retired_at.is_none() {
                    // The provider-key half of `references` counts documents
                    // a `write_resource` writes under this lock, so holding
                    // it across the count and the update is what makes the
                    // refusal true for them. The group row is already held,
                    // which is the documented order: group rows, then this.
                    refs::lock_resources(&mut *conn).await?;
                    let uses = references(&mut *conn, &row).await?;
                    if uses.any() {
                        return Err(ApiError::group_in_use(&uses));
                    }
                    sqlx::query("UPDATE provider_groups SET retired_at = now() WHERE id = $1")
                        .bind(id)
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                    audit::record(
                        &mut *conn,
                        AuditEntry {
                            actor: Actor::Operator(&auth),
                            action: "provider_group.retire",
                            target_kind: "provider_group",
                            target_id: Some(id.to_string()),
                            tenant_id: None,
                            detail: json!({"name": row.name}),
                        },
                    )
                    .await?;
                }
                let row = read_one(&mut *conn, id).await?;
                view_json(&mut *conn, &row).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `POST /admin/provider-groups/{id}/make-default` → `200 Group`: moves the
/// designated default to this group. Both rows are taken `FOR UPDATE` in
/// ascending id, so two operators designating different groups serialise. A
/// retired group is `409 group_retired`.
pub async fn make_default(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(_): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = group_id(&raw)?;
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                // Ascending id: whichever two operators pick, the lower id is
                // locked first.
                let rows: Vec<GroupRow> = sqlx::query_as(&format!(
                    "SELECT {COLUMNS} FROM provider_groups WHERE is_default OR id = $1 \
                     ORDER BY id FOR UPDATE"
                ))
                .bind(id)
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let Some(row) = rows.iter().find(|r| r.id == id) else {
                    return Err(ApiError::not_found());
                };
                if row.retired_at.is_some() {
                    return Err(ApiError::group_retired());
                }
                if !row.is_default {
                    let previous = rows.iter().find(|r| r.is_default).map(|r| r.id);
                    sqlx::query("UPDATE provider_groups SET is_default = false WHERE is_default")
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                    sqlx::query("UPDATE provider_groups SET is_default = true WHERE id = $1")
                        .bind(id)
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                    audit::record(
                        &mut *conn,
                        AuditEntry {
                            actor: Actor::Operator(&auth),
                            action: "provider_group.make_default",
                            target_kind: "provider_group",
                            target_id: Some(id.to_string()),
                            tenant_id: None,
                            detail: json!({"name": row.name, "previous": previous}),
                        },
                    )
                    .await?;
                }
                let row = read_one(&mut *conn, id).await?;
                view_json(&mut *conn, &row).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `GET /admin/provider-groups/{id}/rates` → `{group, default, models:
/// [{model_id, name, rate}]}`, over every live model.
pub async fn list_rates(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = group_id(&raw)?;
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                // One transaction, so every plan below reads the same
                // `now()`.
                let row = lock_one(&mut *conn, id, LockMode::Share).await?;
                let group = view(&mut *conn, &row).await?;
                let default = rates::rate_plan(&mut *conn, id, None).await?;
                let models: Vec<(Uuid, String)> = sqlx::query_as(
                    "SELECT id, COALESCE(doc->>'display_name', '') FROM gw_resources \
                     WHERE kind = $1 AND NOT deleted ORDER BY 2, 1",
                )
                .bind(Kind::Models.segment())
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let mut items = Vec::with_capacity(models.len());
                for (model_id, name) in models {
                    let rate = rates::rate_plan(&mut *conn, id, Some(model_id)).await?;
                    items.push(json!({"model_id": model_id, "name": name, "rate": rate}));
                }
                Ok::<_, ApiError>(json!({
                    "group": group,
                    "default": default,
                    "models": items,
                }))
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `POST /admin/provider-groups/{id}/rates`: a model's override, or the
/// group default.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateBody {
    #[serde(default)]
    model_id: Option<Uuid>,
    /// A decimal string; `null` or absent on a model row ends its override.
    /// Parsed like the group's own default rate, so a JSON number answers
    /// `422 /cny_per_usd` here too.
    #[serde(default)]
    cny_per_usd: Option<Value>,
    effective_from: DateTime<Utc>,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /admin/provider-groups/{id}/rates` → `201 {revision, warnings}`. A
/// default row (`model_id` null) with no rate is `422 /cny_per_usd`: only a
/// model's override ends.
pub async fn create_rate(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(body): ApiJson<RateBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = group_id(&raw)?;
    let rate = body
        .cny_per_usd
        .as_ref()
        .map(|raw| {
            checked_rate("/cny_per_usd", Some(raw)).map_err(|d| ApiError::validation(vec![d]))
        })
        .transpose()?;
    let row = NewGroupRate {
        group_id: id,
        model_id: body.model_id,
        effective_from: body.effective_from,
        cny_per_usd: rate,
        note: body.note.unwrap_or_default(),
    };
    let (revision, warnings) = state
        .db
        .tx(move |conn| {
            async move { rates::insert_group_rate(&mut *conn, &row, Actor::Operator(&auth)).await }
                .boxed()
        })
        .await?;
    let revision = serde_json::to_value(revision).map_err(ApiError::internal)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"revision": revision, "warnings": warnings})),
    ))
}
