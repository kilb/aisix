//! Provider groups as data: the routing name a provider key and a key
//! document carry (immutable, operator-only), the display name and
//! description tenants see, and the group rates it charges
//! ([`crate::pricing::rates`]).
//!
//! A group is never deleted. Retiring one refuses `409 group_in_use` while
//! any tenant, the tenant defaults, a live key's override or a live provider
//! key still names it, and the answer counts each. A retired group accepts
//! no rate, leaves every picker, and keeps its row and its name for the
//! history that references it.
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
//! retiring and `make-default` take it `FOR UPDATE`, so no write can name a
//! group between retire's count and its update.
//!
//! # References are counted, never copied
//!
//! Tenants and key overrides hold the group's name in a text array, and a
//! provider key's document holds it in `groups`; [`references`] is how a
//! retire learns whether anything still names the group. A key's own
//! `group_id` column is Task 5's and is counted there.

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
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::pricing::rates::{self, NewGroupRate, RatePlan};
use crate::resources::{store, Kind};
use crate::AppState;

/// The longest a routing name may be, in characters: the same bound every
/// group name in a tenant's list has (`tenants::MAX_GROUP_CHARS`).
const MAX_NAME_CHARS: usize = 64;

/// The most characters a display name and a description take.
const MAX_DISPLAY_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 500;

/// A `422` at `path`.
fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

/// The group `raw` names; `404` for an id that names nothing, as every id in
/// a path does.
fn group_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::not_found())
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
    /// Live keys whose own override names it.
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
        self.tenants + self.defaults + self.keys + self.provider_keys + self.default_group > 0
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
         WHERE revoked_at IS NULL AND $1::text = ANY(allowed_provider_groups)",
    )
    .bind(name)
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
    let mut keys: Vec<ProviderKeyRef> = store::list(&mut *conn, Kind::ProviderKeys)
        .await?
        .into_iter()
        .filter(|row| {
            row.doc
                .get("groups")
                .and_then(Value::as_array)
                .is_some_and(|groups| groups.iter().any(|g| g.as_str() == Some(name)))
        })
        .map(|row| ProviderKeyRef {
            id: row.id,
            display_name: row
                .doc
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect();
    keys.sort_by(|a, b| a.display_name.cmp(&b.display_name).then(a.id.cmp(&b.id)));
    Ok(keys)
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

/// A rate as given: a decimal string of at most four decimals, 0 to 100.
fn checked_rate(raw: &Value) -> Result<aisix_pricing::CnyPerUsd, Detail> {
    raw.as_str()
        .and_then(|s| aisix_pricing::CnyPerUsd::parse(s).ok())
        .ok_or_else(|| Detail {
            path: "/default_cny_per_usd".to_string(),
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
    let rate = match checked_rate(&default_cny_per_usd) {
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
                        refused("/name", "分组名称已存在")
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
    #[serde(default)]
    cny_per_usd: Option<String>,
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
        .as_deref()
        .map(|raw| {
            aisix_pricing::CnyPerUsd::parse(raw).map_err(|_| refused("/cny_per_usd", RATE_MESSAGE))
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
