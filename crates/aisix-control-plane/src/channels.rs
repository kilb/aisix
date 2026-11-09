//! Public models and channel models: the channel-style way of authoring a
//! model.
//!
//! A **public model** is the name a tenant sends (`pub-x`). Its row's id is
//! the gateway's model id for the routing document generated from it, minted
//! once and never changed: grants (`tenants.allowed_model_ids`), baselines
//! and group rates all key on it, so a new id would lose every one of them.
//! A rename is therefore a new public model; the old name is retired, never
//! repointed.
//!
//! A **channel model** says which upstream account serves a public model,
//! under which upstream name, at which priority and weight. The operator
//! ticks a model on an account, and this module writes the documents that
//! implies:
//!
//! - one **direct** model per enabled channel model, at that row's
//!   `direct_model_id`, named `<public name>#<account display name>` so it
//!   never collides with the public model's own name;
//! - one **routing** model per public model, at the public model's id, whose
//!   targets name the direct models by id.
//!
//! Every generated document is owned by [`owner`]
//! (`public:<public_model_id>`) and written through
//! [`write_resource`], so a hand edit answers `409 managed_resource` and the
//! next regeneration cannot silently overwrite it.
//!
//! # No serving account is not a deletion
//!
//! Disabling the last serving account leaves the public model listed: the
//! routing document stays, with no target, and a request for the name
//! answers the gateway's `upstream_unavailable` `503` — the same answer
//! every upstream being down gets (a routing model whose targets all resolve
//! to nothing is refused with it). Grants are **not** withdrawn, and only the
//! direct documents of the disabled rows are removed. An operator who wants
//! the name gone retires the public model.
//!
//! # Locks
//!
//! A channel edit is one transaction: the public model's row `FOR UPDATE`
//! ([`lock_public`]), then [`write_resource`], which takes
//! `RESOURCES_LOCK_KEY` itself. A public model's row sits after the tenant
//! rows and before the resources lock, which is the documented order; this
//! module never withdraws a grant, so it takes no tenant row at all
//! (`crate/AGENTS.md`, "Lock order").
//!
//! A retire takes the same two in the same order, but takes the resources
//! lock itself ([`refs::lock_resources`]) because its reference counts are
//! plain `SELECT`s: a grant commits under that lock, so a count taken before
//! it could miss one ([`retire`]).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::resources::store::{self, ResourceRow};
use crate::resources::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use crate::resources::{refs, Kind};
use crate::AppState;

/// The longest a public model's name may be, in characters.
const MAX_NAME_CHARS: usize = 128;
/// The longest an upstream model name may be, in characters.
const MAX_UPSTREAM_CHARS: usize = 255;
/// The largest weight a channel model takes, as `RoutingTarget::weight` does.
const MAX_WEIGHT: i32 = 65535;

/// `422` at `path`.
fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

/// Who owns the documents derived from public model `public_model_id`.
pub fn owner(public_model_id: Uuid) -> String {
    format!("public:{public_model_id}")
}

/// One `public_models` row.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct PublicModelRow {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

/// The columns a [`PublicModelRow`] is read from.
const PUBLIC_COLUMNS: &str = "id, name, enabled, created_by, created_at, retired_at";

/// One `channel_models` row.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct ChannelModelRow {
    pub id: Uuid,
    pub provider_key_id: Uuid,
    pub public_model_id: Uuid,
    pub upstream_model: Option<String>,
    pub priority: i32,
    pub weight: i32,
    pub enabled: bool,
    pub direct_model_id: Uuid,
}

/// The columns a [`ChannelModelRow`] is read from.
const CHANNEL_COLUMNS: &str = "id, provider_key_id, public_model_id, upstream_model, priority, \
                              weight, enabled, direct_model_id";

/// The display name of the direct document for `public_name` served by the
/// account named `provider_key_display_name`: `<public name>#<account>`.
///
/// Never the public name itself — the routing document holds that, and two
/// live model documents cannot share a name. `#` cannot appear in either
/// half (a public model's name is refused one at [`checked_name`]), so the
/// pair is unique per (public model, account).
pub fn direct_display_name(public_name: &str, provider_key_display_name: &str) -> String {
    format!("{public_name}#{provider_key_display_name}")
}

/// The direct model document for one channel model.
///
/// `provider` and `provider_key_id` come from the account's own document;
/// `model_name` is `upstream_model`, or the public name when the row sets
/// none. No `cost`, `pricing_key`, `retries`, `cooldown` or
/// `background_model_check`: the control plane never prices a target, and
/// per-account timeouts are not part of this shape.
pub fn direct_doc(
    channel: &ChannelModelRow,
    public: &PublicModelRow,
    provider_key_display_name: &str,
    provider: &str,
) -> Value {
    json!({
        "display_name": direct_display_name(&public.name, provider_key_display_name),
        "provider": provider,
        "model_name": channel
            .upstream_model
            .as_deref()
            .unwrap_or(public.name.as_str()),
        "provider_key_id": channel.provider_key_id.to_string(),
    })
}

/// The routing model document for `public` over `channels`.
///
/// `round_robin`, **not** `failover`: `failover` accepts a target's `weight`
/// and does not use it, so a weight would be silently inert. `round_robin` is
/// smooth weighted round-robin inside a tier and walks forward on failure,
/// and tiers concatenate best-first, so "priority tiers, weighted inside a
/// tier, falling over down the tiers" is exactly this strategy with
/// `priority` and `weight` set on the targets.
///
/// Targets are ordered by `priority` descending, then `weight` descending,
/// then `direct_model_id` ascending, so the document is a pure function of
/// the rows and two regenerations of unchanged rows are byte-identical.
///
/// An empty `channels` writes `targets: []`: the public model keeps its
/// routing document, stays listed, and answers `upstream_unavailable` to a
/// request, which is what the owner asked for when every serving account is
/// disabled.
pub fn routing_doc(public: &PublicModelRow, channels: &[ChannelModelRow]) -> Value {
    let mut ordered: Vec<&ChannelModelRow> = channels.iter().collect();
    ordered.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(b.weight.cmp(&a.weight))
            .then(a.direct_model_id.cmp(&b.direct_model_id))
    });
    let targets: Vec<Value> = ordered
        .iter()
        .map(|c| {
            json!({
                "model_id": c.direct_model_id.to_string(),
                "priority": c.priority,
                "weight": c.weight,
            })
        })
        .collect();
    json!({
        "display_name": public.name,
        "routing": {
            "strategy": "round_robin",
            "targets": targets,
        },
    })
}

/// A public model's name as given: trimmed, 1 to [`MAX_NAME_CHARS`]
/// characters, no control characters, and neither `*` (a wildcard alias
/// cannot be granted) nor `#` (it separates a direct document's name from
/// the public one).
fn checked_name(raw: &str) -> Result<String, Detail> {
    let name = raw.trim();
    let bad = |message: String| Detail {
        path: "/name".to_string(),
        message,
    };
    if name.is_empty() {
        return Err(bad("模型名称不能为空".to_string()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(bad(format!("模型名称不能超过 {MAX_NAME_CHARS} 个字符")));
    }
    if name.chars().any(char::is_control) {
        return Err(bad("模型名称不能包含控制字符".to_string()));
    }
    if name.contains('*') {
        return Err(bad(
            "模型名称不能包含 *（通配模型不能作为公共模型）".to_string()
        ));
    }
    if name.contains('#') {
        return Err(bad(
            "模型名称不能包含 #（它用于生成直连模型的名称）".to_string()
        ));
    }
    Ok(name.to_string())
}

/// An upstream model name as given: `None` for `null`/absent, otherwise 1
/// to [`MAX_UPSTREAM_CHARS`] characters with no control characters.
fn checked_upstream(raw: Option<&str>) -> Result<Option<String>, Detail> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let name = raw.trim();
    let bad = |message: String| Detail {
        path: "/upstream_model".to_string(),
        message,
    };
    if name.is_empty() {
        return Err(bad(
            "上游模型名称不能为空（不填写表示与公共模型同名）".to_string()
        ));
    }
    if name.chars().count() > MAX_UPSTREAM_CHARS {
        return Err(bad(format!(
            "上游模型名称不能超过 {MAX_UPSTREAM_CHARS} 个字符"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(bad("上游模型名称不能包含控制字符".to_string()));
    }
    Ok(Some(name.to_string()))
}

/// The public model `id` as it stands, taking no lock; `404` for an id that
/// names nothing, as every id in a path does.
async fn read_public(conn: &mut PgConnection, id: Uuid) -> Result<PublicModelRow, ApiError> {
    sqlx::query_as(&format!(
        "SELECT {PUBLIC_COLUMNS} FROM public_models WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?
    .ok_or_else(ApiError::not_found)
}

/// [`read_public`], for the rest of the transaction: a regeneration of one
/// public model serialises on this row.
async fn lock_public(conn: &mut PgConnection, id: Uuid) -> Result<PublicModelRow, ApiError> {
    sqlx::query_as(&format!(
        "SELECT {PUBLIC_COLUMNS} FROM public_models WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?
    .ok_or_else(ApiError::not_found)
}

/// A retired public model, as every write of one answers.
fn retired() -> ApiError {
    ApiError::conflict("public_model_retired", "该公共模型已停用")
}

/// The account's `(display_name, provider)`, from its live document; `None`
/// when no live provider key has that id.
async fn provider_key_of(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<(String, String)>, ApiError> {
    let row = store::get(&mut *conn, Kind::ProviderKeys, id).await?;
    Ok(row.filter(|row| !row.deleted).map(|row| {
        let text = |field: &str| {
            row.doc
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        (text("display_name"), text("provider"))
    }))
}

/// Regenerates every document `public_model_id` implies, in the caller's
/// transaction, from the `channel_models` rows as they now stand.
///
/// Locks the public model's row ([`lock_public`]) and lets
/// [`write_resource`] take `RESOURCES_LOCK_KEY`; the enabled rows are read
/// after that lock, so no channel edit can be half-applied into a
/// regeneration. With at least one enabled channel whose account still
/// exists, writes each direct document and then the routing document —
/// referents before referrers. With none, writes a routing document with no
/// target and removes every direct document this public model owns that no
/// enabled row names: the public model stays listed and answers
/// `upstream_unavailable`, and its grants stay (`tenants::on_model_deleted`
/// is deliberately not called).
///
/// A retired public model is `409 public_model_retired`, so nothing
/// regenerates one.
pub async fn regenerate(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    public_model_id: Uuid,
) -> Result<(), ApiError> {
    let public = lock_public(&mut *conn, public_model_id).await?;
    if public.retired_at.is_some() {
        return Err(retired());
    }
    let channels: Vec<ChannelModelRow> = sqlx::query_as(&format!(
        "SELECT {CHANNEL_COLUMNS} FROM channel_models \
         WHERE public_model_id = $1 AND enabled \
         ORDER BY priority DESC, weight DESC, direct_model_id ASC"
    ))
    .bind(public_model_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;

    // An enabled row whose account was deleted is reported rather than
    // generated: a direct document naming a provider key that does not exist
    // is refused by the reference check, so the row cannot serve until an
    // operator repoints or removes it.
    let mut served: Vec<(ChannelModelRow, String, String)> = Vec::new();
    for channel in channels {
        match provider_key_of(&mut *conn, channel.provider_key_id).await? {
            Some((display_name, provider)) => served.push((channel, display_name, provider)),
            None => tracing::warn!(
                public_model_id = %public_model_id,
                provider_key_id = %channel.provider_key_id,
                "a channel model names a provider key that no longer exists; \
                 its direct document is not generated"
            ),
        }
    }

    let owner = owner(public_model_id);
    // Direct documents first: the routing document names them by id, and a
    // reference must be live when the referrer is written.
    for (channel, display_name, provider) in &served {
        crate::tenants::put(
            conn,
            ctx,
            Kind::Models,
            channel.direct_model_id,
            &direct_doc(channel, &public, display_name, provider),
            &owner,
        )
        .await?;
    }
    let channels: Vec<ChannelModelRow> = served.into_iter().map(|(c, _, _)| c).collect();
    crate::tenants::put(
        conn,
        ctx,
        Kind::Models,
        public_model_id,
        &routing_doc(&public, &channels),
        &owner,
    )
    .await?;

    // Retraction, after the routing document no longer names them: every
    // live model document this public model owns whose id is neither the
    // routing document nor a direct document of an enabled row.
    let keep: Vec<Uuid> = std::iter::once(public_model_id)
        .chain(channels.iter().map(|c| c.direct_model_id))
        .collect();
    for row in store::owned_by(&mut *conn, std::slice::from_ref(&owner)).await? {
        if row.kind != Kind::Models || row.deleted || keep.contains(&row.id) {
            continue;
        }
        write_resource(
            conn,
            ctx,
            ResourceWrite {
                kind: Kind::Models,
                id: row.id,
                op: WriteOp::Delete,
                expected_version: None,
                managed_by: Some(&owner),
            },
        )
        .await?;
    }
    Ok(())
}

/// Refuses the delete of a provider key any channel model names, `409
/// in_use` with `counts {public_models}`, answering in the terms the
/// operator used. `refs::incoming` already holds a key some direct document
/// names; this covers the rest, a key whose channel rows are all disabled
/// included. A caller runs it before the generic delete, in the same
/// transaction.
///
/// Only rows of a **live** public model count. [`retire`] deletes a retired
/// model's rows, so the join is belt and braces for a row that outlived its
/// model by another route: a retired model pins no account.
pub async fn refuse_if_served(
    conn: &mut PgConnection,
    provider_key_id: Uuid,
) -> Result<(), ApiError> {
    let serving: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT cm.public_model_id)::bigint FROM channel_models cm \
         JOIN public_models p ON p.id = cm.public_model_id AND p.retired_at IS NULL \
         WHERE cm.provider_key_id = $1",
    )
    .bind(provider_key_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if serving == 0 {
        return Ok(());
    }
    Err(ApiError::provider_key_served(
        json!({ "public_models": serving }),
    ))
}

/// What still holds a public model from being retired.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PublicModelUse {
    /// Live tenants whose grants name it.
    pub tenants: i64,
    /// Live keys whose model selection names it.
    pub keys: i64,
    /// Its enabled channel models.
    pub channels: i64,
}

impl PublicModelUse {
    fn any(&self) -> bool {
        self.tenants > 0 || self.keys > 0 || self.channels > 0
    }
}

/// What still names `id`, as [`PublicModelUse`].
async fn references(conn: &mut PgConnection, id: Uuid) -> Result<PublicModelUse, ApiError> {
    let tenants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM tenants \
         WHERE status <> 'deleted' AND $1 = ANY(allowed_model_ids)",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let keys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM tenant_keys \
         WHERE revoked_at IS NULL AND $1 = ANY(model_ids)",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let channels: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM channel_models WHERE public_model_id = $1 AND enabled",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    Ok(PublicModelUse {
        tenants,
        keys,
        channels,
    })
}

/// One account that serves a public model, as `GET /admin/public-models/{id}`
/// lists it.
#[derive(Debug, Clone, Serialize)]
pub struct ServedBy {
    pub provider_key_id: Uuid,
    pub display_name: String,
    pub groups: Vec<String>,
    pub upstream_model: Option<String>,
    pub priority: i32,
    pub weight: i32,
    pub enabled: bool,
}

/// The channels of `public_model_id`, joined to each account's live document.
async fn served_by(
    conn: &mut PgConnection,
    public_model_id: Uuid,
) -> Result<Vec<ServedBy>, ApiError> {
    let rows: Vec<ChannelModelRow> = sqlx::query_as(&format!(
        "SELECT {CHANNEL_COLUMNS} FROM channel_models WHERE public_model_id = $1 \
         ORDER BY priority DESC, weight DESC, direct_model_id ASC"
    ))
    .bind(public_model_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let mut served = Vec::with_capacity(rows.len());
    for row in rows {
        let account = store::get(&mut *conn, Kind::ProviderKeys, row.provider_key_id).await?;
        let doc = account.filter(|r| !r.deleted).map(|r| r.doc);
        let text = |field: &str| {
            doc.as_ref()
                .and_then(|d| d.get(field))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let groups = doc
            .as_ref()
            .and_then(|d| d.get("groups"))
            .and_then(Value::as_array)
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        served.push(ServedBy {
            provider_key_id: row.provider_key_id,
            display_name: text("display_name"),
            groups,
            upstream_model: row.upstream_model,
            priority: row.priority,
            weight: row.weight,
            enabled: row.enabled,
        });
    }
    Ok(served)
}

/// The API's `PublicModel`.
#[derive(Debug, Clone, Serialize)]
pub struct PublicModelView {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub served_by: Vec<ServedBy>,
    pub tenants: i64,
    pub keys: i64,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

async fn view(conn: &mut PgConnection, row: &PublicModelRow) -> Result<PublicModelView, ApiError> {
    let uses = references(conn, row.id).await?;
    Ok(PublicModelView {
        id: row.id,
        name: row.name.clone(),
        enabled: row.enabled,
        served_by: served_by(conn, row.id).await?,
        tenants: uses.tenants,
        keys: uses.keys,
        created_at: row.created_at,
        retired_at: row.retired_at,
    })
}

async fn view_json(conn: &mut PgConnection, row: &PublicModelRow) -> Result<Value, ApiError> {
    serde_json::to_value(view(conn, row).await?).map_err(ApiError::internal)
}

/// The API's `ChannelModel`: one row per public model this account serves or
/// could serve.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelModelView {
    pub public_model_id: Uuid,
    pub public_model_name: String,
    pub upstream_model: Option<String>,
    pub priority: i32,
    pub weight: i32,
    pub enabled: bool,
    /// Whether this account serves the public model at all.
    pub served: bool,
}

/// `GET /admin/public-models` → `{items: [PublicModel]}`.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let items = state
        .db
        .tx(move |conn| {
            async move {
                let rows: Vec<PublicModelRow> = sqlx::query_as(&format!(
                    "SELECT {PUBLIC_COLUMNS} FROM public_models ORDER BY name"
                ))
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let mut items = Vec::with_capacity(rows.len());
                for row in &rows {
                    items.push(view_json(&mut *conn, row).await?);
                }
                Ok::<_, ApiError>(Value::Array(items))
            }
            .boxed()
        })
        .await?;
    Ok(Json(json!({ "items": items })))
}

/// `POST /admin/public-models` → `201 PublicModel`: the row only. No
/// document exists until a channel model does.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    name: String,
}

pub async fn create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let name = checked_name(&body.name).map_err(|d| ApiError::validation(vec![d]))?;
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                // A live model of any kind carrying the name keeps it: a
                // public model cannot share a name with a hand-made one. The
                // reverse side is the identity index, which refuses a
                // hand-made model named after a public model's routing
                // document. The check holds the resources lock, as every
                // model write does, so a model written meanwhile is either
                // seen here or written after this commits; the path takes no
                // tenant, group or public-model row, so the order holds.
                refs::lock_resources(&mut *conn).await?;
                let taken: Option<Uuid> = sqlx::query_scalar(
                    "SELECT id FROM gw_resources \
                     WHERE kind = $1 AND identity = $2 AND NOT deleted LIMIT 1",
                )
                .bind(Kind::Models.segment())
                .bind(&name)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if taken.is_some() {
                    return Err(ApiError::public_model_name_taken());
                }
                let id: Uuid = sqlx::query_scalar(
                    "INSERT INTO public_models (name, created_by) VALUES ($1, $2) RETURNING id",
                )
                .bind(&name)
                .bind(Some(auth.id))
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| match &e {
                    // The name is unique among retired rows too: a retired
                    // public model keeps its id, so its name stays taken.
                    sqlx::Error::Database(db)
                        if db.code().as_deref() == Some("23505")
                            && db.constraint() == Some("public_models_name_key") =>
                    {
                        ApiError::public_model_name_taken()
                    }
                    _ => ApiError::from(DbError::from(e)),
                })?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "public_model.create",
                        target_kind: "public_model",
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: json!({"name": name}),
                    },
                )
                .await?;
                let row = read_public(&mut *conn, id).await?;
                view_json(&mut *conn, &row).await
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(answer)))
}

/// `GET /admin/public-models/{id}` → `PublicModel`.
pub async fn get(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = path_id(&raw)?;
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                let row = read_public(&mut *conn, id).await?;
                view_json(&mut *conn, &row).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `POST /admin/public-models/{id}/retire` → `200 PublicModel`: sets
/// `retired_at` once nothing live names it; `409 public_model_in_use` with
/// the counts while something does, and `409 public_model_retired` for a
/// model already retired.
///
/// Retiring removes the documents, the routing document first since it names
/// the direct ones, and the model's channel rows with them: the row names a
/// public model nobody can restore, so it holds nothing worth keeping.
///
/// The counts are read under `RESOURCES_LOCK_KEY`, and the delete and the
/// update land while it is held. A grant is the reference that can arrive
/// concurrently — `tenants::update` takes its tenant row, then that lock,
/// then validates `allowed_model_ids` against live documents — so a grant
/// that commits first is counted here and refused, and one that gets the
/// lock second finds the model retired. The public model's row is held
/// before the resources lock, which is the documented order; retire takes no
/// tenant row (`groups::retire` is the same shape).
pub async fn retire(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(_): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = path_id(&raw)?;
    let ctx = WriteCtx::of(&state);
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                let row = lock_public(&mut *conn, id).await?;
                if row.retired_at.is_some() {
                    return Err(retired());
                }
                refs::lock_resources(&mut *conn).await?;
                let uses = references(&mut *conn, row.id).await?;
                if uses.any() {
                    return Err(ApiError::public_model_in_use(
                        serde_json::to_value(uses).map_err(ApiError::internal)?,
                    ));
                }
                let owner = owner(id);
                // Referrers before referents: the routing document names
                // every direct document by id.
                let live: Vec<ResourceRow> =
                    store::owned_by(&mut *conn, std::slice::from_ref(&owner)).await?;
                let mut models: Vec<Uuid> = live
                    .iter()
                    .filter(|r| r.kind == Kind::Models && !r.deleted)
                    .map(|r| r.id)
                    .collect();
                models.sort_by_key(|m| if *m == id { 0 } else { 1 });
                for model in models {
                    write_resource(
                        &mut *conn,
                        &ctx,
                        ResourceWrite {
                            kind: Kind::Models,
                            id: model,
                            op: WriteOp::Delete,
                            expected_version: None,
                            managed_by: Some(&owner),
                        },
                    )
                    .await?;
                }
                sqlx::query("DELETE FROM channel_models WHERE public_model_id = $1")
                    .bind(id)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                sqlx::query(
                    "UPDATE public_models SET retired_at = now(), enabled = false WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "public_model.retire",
                        target_kind: "public_model",
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: json!({"name": row.name}),
                    },
                )
                .await?;
                let row = read_public(&mut *conn, id).await?;
                view_json(&mut *conn, &row).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `GET /admin/provider-keys/{id}/channel-models` → `{items: [ChannelModel]}`,
/// one per live public model — served or not — with `served` saying which.
pub async fn list_channels(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let provider_key_id = path_id(&raw)?;
    let items = state
        .db
        .tx(move |conn| {
            async move {
                if provider_key_of(&mut *conn, provider_key_id)
                    .await?
                    .is_none()
                {
                    return Err(ApiError::not_found());
                }
                let channels: Vec<ChannelModelRow> = sqlx::query_as(&format!(
                    "SELECT {CHANNEL_COLUMNS} FROM channel_models WHERE provider_key_id = $1",
                ))
                .bind(provider_key_id)
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let publics: Vec<PublicModelRow> = sqlx::query_as(&format!(
                    "SELECT {PUBLIC_COLUMNS} FROM public_models WHERE retired_at IS NULL \
                     ORDER BY name"
                ))
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let items: Vec<Value> = publics
                    .iter()
                    .map(|public| {
                        let row = channels.iter().find(|c| c.public_model_id == public.id);
                        serde_json::to_value(ChannelModelView {
                            public_model_id: public.id,
                            public_model_name: public.name.clone(),
                            upstream_model: row.and_then(|c| c.upstream_model.clone()),
                            priority: row.map(|c| c.priority).unwrap_or(0),
                            weight: row.map(|c| c.weight).unwrap_or(1),
                            enabled: row.map(|c| c.enabled).unwrap_or(false),
                            served: row.is_some(),
                        })
                        .map_err(ApiError::internal)
                    })
                    .collect::<Result<_, _>>()?;
                Ok::<_, ApiError>(Value::Array(items))
            }
            .boxed()
        })
        .await?;
    Ok(Json(json!({ "items": items })))
}

/// `PUT /admin/provider-keys/{id}/channel-models/{public_model_id}`: creates
/// the channel model or updates it, then regenerates every document it
/// implies, in one transaction.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutBody {
    /// Absent keeps the row's value, `null` clears it (the public name is
    /// sent upstream), a string sets it. Read as a `Value` because serde
    /// cannot tell an absent `Option<Option<T>>` from a `null` one.
    #[serde(default, deserialize_with = "present")]
    upstream_model: Option<Value>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    weight: Option<i32>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// A field's value when the body gave it, `None` when it was absent: so
/// `null` and an absent field differ.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `upstream_model` as given: absent keeps the row's value, `null` clears
/// it, a string is the new name.
#[derive(Debug, Clone)]
enum GivenUpstream {
    Unchanged,
    Cleared,
    Named(String),
}

/// [`GivenUpstream`] for the body's field, `422 /upstream_model` for
/// anything else.
fn given_upstream(raw: Option<&Value>) -> Result<GivenUpstream, Vec<Detail>> {
    match raw {
        None => Ok(GivenUpstream::Unchanged),
        Some(Value::Null) => Ok(GivenUpstream::Cleared),
        Some(Value::String(name)) => checked_upstream(Some(name))
            .map(|name| GivenUpstream::Named(name.unwrap_or_default()))
            .map_err(|d| vec![d]),
        Some(_) => Err(vec![Detail {
            path: "/upstream_model".to_string(),
            message: "上游模型名称应为字符串或不填写".to_string(),
        }]),
    }
}

pub async fn put_channel(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((raw_key, raw_public)): Path<(String, String)>,
    ApiJson(body): ApiJson<PutBody>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let provider_key_id = path_id(&raw_key)?;
    let public_model_id = path_id(&raw_public)?;
    let PutBody {
        upstream_model,
        priority,
        weight,
        enabled,
    } = body;
    let mut problems = Vec::new();
    let given_upstream = match given_upstream(upstream_model.as_ref()) {
        Ok(given) => given,
        Err(d) => {
            problems.extend(d);
            GivenUpstream::Unchanged
        }
    };
    if let Some(weight) = weight {
        if !(0..=MAX_WEIGHT).contains(&weight) {
            problems.push(Detail {
                path: "/weight".to_string(),
                message: format!("权重应在 0 到 {MAX_WEIGHT} 之间"),
            });
        }
    }
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let ctx = WriteCtx::of(&state);
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                let Some((display_name, provider)) =
                    provider_key_of(&mut *conn, provider_key_id).await?
                else {
                    return Err(refused("/provider_key_id", "该上游账号不存在或已删除"));
                };
                let public = lock_public(&mut *conn, public_model_id).await?;
                if public.retired_at.is_some() {
                    return Err(retired());
                }
                let existing: Option<ChannelModelRow> = sqlx::query_as(&format!(
                    "SELECT {CHANNEL_COLUMNS} FROM channel_models \
                     WHERE provider_key_id = $1 AND public_model_id = $2 FOR UPDATE"
                ))
                .bind(provider_key_id)
                .bind(public_model_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let created = existing.is_none();
                let row: ChannelModelRow = match &existing {
                    Some(row) => {
                        let upstream = match &given_upstream {
                            GivenUpstream::Unchanged => row.upstream_model.clone(),
                            GivenUpstream::Cleared => None,
                            GivenUpstream::Named(name) => Some(name.clone()),
                        };
                        sqlx::query_as(&format!(
                            "UPDATE channel_models SET upstream_model = $2, priority = $3, \
                               weight = $4, enabled = $5, updated_at = now() \
                             WHERE id = $1 RETURNING {CHANNEL_COLUMNS}"
                        ))
                        .bind(row.id)
                        .bind(upstream)
                        .bind(priority.unwrap_or(row.priority))
                        .bind(weight.unwrap_or(row.weight))
                        .bind(enabled.unwrap_or(row.enabled))
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(DbError::from)?
                    }
                    None => sqlx::query_as(&format!(
                        "INSERT INTO channel_models \
                         (provider_key_id, public_model_id, upstream_model, priority, weight, \
                          enabled, direct_model_id, created_by) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {CHANNEL_COLUMNS}"
                    ))
                    .bind(provider_key_id)
                    .bind(public_model_id)
                    .bind(match &given_upstream {
                        GivenUpstream::Named(name) => Some(name.clone()),
                        _ => None,
                    })
                    .bind(priority.unwrap_or(0))
                    .bind(weight.unwrap_or(1))
                    .bind(enabled.unwrap_or(true))
                    .bind(Uuid::new_v4())
                    .bind(Some(auth.id))
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(DbError::from)?,
                };
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "channel_model.put",
                        target_kind: "public_model",
                        target_id: Some(public_model_id.to_string()),
                        tenant_id: None,
                        detail: json!({
                            "provider_key_id": provider_key_id,
                            "provider_key_display_name": display_name,
                            "provider": provider,
                            "upstream_model": row.upstream_model,
                            "priority": row.priority,
                            "weight": row.weight,
                            "enabled": row.enabled,
                            "created": created,
                        }),
                    },
                )
                .await?;
                regenerate(&mut *conn, &ctx, public_model_id).await?;
                let warnings: Vec<&str> = if provider_key_of(&mut *conn, provider_key_id)
                    .await?
                    .is_none()
                {
                    vec!["provider_key_missing"]
                } else {
                    Vec::new()
                };
                Ok::<_, ApiError>(json!({
                    "public_model_id": row.public_model_id,
                    "public_model_name": public.name,
                    "upstream_model": row.upstream_model,
                    "priority": row.priority,
                    "weight": row.weight,
                    "enabled": row.enabled,
                    "served": true,
                    "warnings": warnings,
                }))
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `DELETE /admin/provider-keys/{id}/channel-models/{public_model_id}` →
/// `204`, after regenerating the documents the removal implies.
pub async fn delete_channel(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((raw_key, raw_public)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let provider_key_id = path_id(&raw_key)?;
    let public_model_id = path_id(&raw_public)?;
    let ctx = WriteCtx::of(&state);
    state
        .db
        .tx(move |conn| {
            async move {
                let public = lock_public(&mut *conn, public_model_id).await?;
                if public.retired_at.is_some() {
                    return Err(retired());
                }
                let removed = sqlx::query(
                    "DELETE FROM channel_models \
                     WHERE provider_key_id = $1 AND public_model_id = $2",
                )
                .bind(provider_key_id)
                .bind(public_model_id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if removed.rows_affected() == 0 {
                    return Err(ApiError::not_found());
                }
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "channel_model.delete",
                        target_kind: "public_model",
                        target_id: Some(public_model_id.to_string()),
                        tenant_id: None,
                        detail: json!({"provider_key_id": provider_key_id}),
                    },
                )
                .await?;
                regenerate(&mut *conn, &ctx, public_model_id).await
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// An id in a path; `404` for anything that is not one, as every id in a
/// path does.
fn path_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::not_found())
}
