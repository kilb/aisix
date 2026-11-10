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
//! A channel edit is one transaction: every all-scope tenant's row
//! ([`crate::tenants::lock_all_scope`]), the public model's row `FOR UPDATE`
//! ([`lock_public`]), then [`write_resource`], which takes
//! `RESOURCES_LOCK_KEY` itself. A tenant's row sits before a public model's,
//! which sits before the resources lock, which is the documented order. The
//! tenants' rows are taken because a public model that becomes callable is
//! in every all-scope tenant's scope at once, and their key documents, which
//! name the models by id, are rewritten in the same transaction
//! ([`crate::keys::reproject_tenants`]). This module still never withdraws a
//! grant: a selected tenant's list is an operator's.
//!
//! A retire takes the tenants' rows, then the public model's, then takes the
//! resources lock itself ([`refs::lock_resources`]) because its reference
//! counts are plain `SELECT`s: a grant commits under that lock, so a count
//! taken before it could miss one ([`retire`]). It then rewrites the
//! all-scope tenants' key documents before deleting the documents the
//! retired model owns, so no key document names a model that is going.
//!
//! A bulk apply ([`bulk_channels`]) holds several public models at once: it
//! takes every one it reuses in ascending id order, and only then the
//! resources lock, so two applies, or an apply and a channel edit, never
//! wait on each other out of order.

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
    /// The routing settings the operator set, without `targets`: those are
    /// read from `channel_models` on every write ([`routing_doc`]). A key
    /// absent here means the gateway's own default, and one equal to that
    /// default is dropped on write ([`checked_routing`]), so a default that
    /// moves in a later gateway release reaches every model that never
    /// overrode it.
    ///
    /// `{}` for every row written before `0037`: the document generated from
    /// it is what [`routing_doc`] built then, `strategy: round_robin` over
    /// the generated targets, so nothing changed for those models.
    pub routing: Value,
}

/// The columns a [`PublicModelRow`] is read from.
const PUBLIC_COLUMNS: &str = "id, name, enabled, created_by, created_at, retired_at, routing";

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

/// The name of the public model whose generated direct document `model_id`
/// is (`channel_models.direct_model_id`, live or not), or `None` for any
/// other model.
///
/// Such a document is granted and priced only through its public model, and
/// this is the one test for it: a tenant grant ([`crate::tenants`]), a
/// baseline ([`crate::pricing::baselines`]) and a group rate
/// ([`crate::pricing::rates`]) each refuse one, naming the public model to
/// use instead, and the price lists ([`priceable_models`]) leave it out. A
/// charge keys on the model the caller addressed, which is the public model,
/// so a price written on the direct document prices nothing; and its name
/// carries the account's.
pub async fn generated_direct_of(
    conn: &mut PgConnection,
    model_id: Uuid,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT p.name FROM channel_models cm JOIN public_models p ON p.id = cm.public_model_id \
         WHERE cm.direct_model_id = $1",
    )
    .bind(model_id)
    .fetch_optional(conn)
    .await?)
}

/// Whether `model_id` is a channel's generated direct document
/// ([`generated_direct_of`]).
pub async fn is_generated_direct(conn: &mut PgConnection, model_id: Uuid) -> Result<bool, DbError> {
    Ok(generated_direct_of(conn, model_id).await?.is_some())
}

/// Every live model a price can be written on, as `(id, display name)` by
/// name then id: every one but a channel's generated direct document, the
/// same test as [`generated_direct_of`] in SQL. The baseline list, a group's
/// rate list and the charge list read it.
pub async fn priceable_models(conn: &mut PgConnection) -> Result<Vec<(Uuid, String)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT r.id, COALESCE(r.doc->>'display_name', '') FROM gw_resources r \
         WHERE r.kind = $1 AND NOT r.deleted \
           AND NOT EXISTS (SELECT 1 FROM channel_models cm WHERE cm.direct_model_id = r.id) \
         ORDER BY 2, 1",
    )
    .bind(Kind::Models.segment())
    .fetch_all(conn)
    .await?)
}

/// The direct model document for one channel model.
///
/// `provider` and `provider_key_id` come from the account's own document;
/// `model_name` is `upstream_model`, or the public name when the row sets
/// none. No `cost`, `pricing_key`, `retries`, `cooldown` or
/// `background_model_check`: the control plane never prices a target, and
/// per-account timeouts are not part of this shape.
///
/// `internal: true` is what keeps this document a routing target and
/// nothing else: the gateway does not list it on `/v1/models` and refuses a
/// request that names it, so a key granted by pattern cannot pin one
/// account and bypass the public model's routing. The routing document
/// itself is not marked, because that is the entry callers address.
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
        "internal": true,
    })
}

/// The routing model document for `public` over `channels`.
///
/// `round_robin`, **not** `failover`: `failover` accepts a target's `weight`
/// and does not use it, so a weight would be silently inert. `round_robin` is
/// smooth weighted round-robin inside a tier and walks forward on failure,
/// and tiers concatenate best-first, so "priority tiers, weighted inside a
/// tier, falling over down the tiers" is exactly this strategy with
/// `priority` and `weight` set on the targets. It is also the setting
/// [`default_routing`] names as the one a model that set no strategy means,
/// so an operator who picks `round_robin` explicitly stores nothing.
///
/// The operator's own settings ([`PublicModelRow::routing`]) are written
/// first and `targets` last, so the one field the generated document decides
/// is the one field no stored setting can carry: the target list is always a
/// function of `channel_models` and can never drift from the account rows. A
/// stored `strategy` replaces the default here.
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
    let strategy = public
        .routing
        .get("strategy")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| Value::String(DEFAULT_STRATEGY.to_string()));
    let mut routing = public.routing.as_object().cloned().unwrap_or_default();
    routing.insert("strategy".to_string(), strategy);
    routing.insert("targets".to_string(), Value::Array(targets));
    json!({
        "display_name": public.name,
        "routing": Value::Object(routing),
    })
}

/// The strategy a public model dispatches with when the operator set none.
pub const DEFAULT_STRATEGY: &str = "round_robin";

/// The strategy a public model cannot use: it orders targets by their price,
/// and no generated target carries one.
const UNPRICED_STRATEGY: &str = "least_cost";

/// The settings a public model's `routing` column may hold a non-default
/// value for: every property of the gateway's own `Routing` definition
/// except `targets`, which is generated.
///
/// Read from the strict model schema rather than copied, so a setting the
/// gateway adds is accepted here the day it lands, and one it retires is
/// refused with it.
fn settable_routing_keys() -> std::collections::BTreeSet<String> {
    aisix_core::models::schema::resource_root_schema(Kind::Models.schema_name(), true)
        .get("definitions")
        .and_then(|d| d.get("Routing"))
        .and_then(|d| d.get("properties"))
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .keys()
                .filter(|key| key.as_str() != "targets")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// `settings` as the `public_models.routing` column stores them: the keys the
/// gateway's `routing` block defines, minus the ones already at the
/// gateway's default, validated against the gateway's own schema.
///
/// The column holds what the operator changed, so a setting equal to the
/// default it is compared against here is not written: a default that moves
/// in a later gateway release then reaches the model instead of being pinned
/// by a value nobody chose. The three defaults are those of the gateway's
/// code (`Routing::*_or_default`, and [`DEFAULT_STRATEGY`] for the one this
/// module itself decides) — `retries` has none (it defers to the deployment's
/// `upstream.retries`), and `max_fallbacks`'s is derived from the target
/// count, so neither is ever treated as a default.
///
/// Refusals name the key: an unknown one, or `targets` itself, at
/// `/routing/<key>`; a value the schema rejects at `/routing/<path>`.
fn checked_routing(settings: &Value) -> Result<Value, Vec<Detail>> {
    let Some(settings) = settings.as_object() else {
        return Err(vec![Detail {
            path: "/routing".to_string(),
            message: "路由设置应为对象".to_string(),
        }]);
    };
    let allowed = settable_routing_keys();
    let mut problems = Vec::new();
    for key in settings.keys() {
        if !allowed.contains(key) {
            problems.push(Detail {
                path: format!("/routing/{key}"),
                message: if key == "targets" {
                    "route targets are generated from the accounts that serve the model and cannot be set here"
                        .to_string()
                } else {
                    format!("网关的路由模型没有「{key}」这一设置")
                },
            });
        }
    }
    // `least_cost` ranks targets by the price each one carries, and a
    // generated target carries none ([`direct_doc`]): every target would rank
    // as unpriced and the strategy would silently fall back to the order the
    // targets are listed in. Refused rather than accepted and inert.
    if settings.get("strategy").and_then(Value::as_str) == Some(UNPRICED_STRATEGY) {
        problems.push(Detail {
            path: "/routing/strategy".to_string(),
            message: format!(
                "公共模型的路由目标不带价格，「{UNPRICED_STRATEGY}」无法按价格排序；请选择其他选路策略"
            ),
        });
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let kept: serde_json::Map<String, Value> = settings
        .iter()
        .filter(|(key, value)| !at_default(key, value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    // Validated as a whole routing block with no targets: the schema then
    // decides the strategy enum, the status range and the hash chain's
    // shape, exactly as it does for a hand-written model.
    let mut probe_routing = kept.clone();
    probe_routing
        .entry("strategy".to_string())
        .or_insert_with(|| Value::String(DEFAULT_STRATEGY.to_string()));
    probe_routing.insert("targets".to_string(), json!([]));
    let probe = json!({
        "display_name": "routing",
        "routing": Value::Object(probe_routing),
    });
    if let Err(err) = aisix_core::models::schema::validate_model(&probe) {
        // The validator's path is an instance path from the probe's root, so
        // it already carries the `/routing` this request's body nests the
        // settings under; only a failure at the routing block itself (an
        // empty path, or one outside it) needs putting back there.
        let path = if err.path.starts_with("/routing") {
            err.path
        } else {
            format!("/routing{}", err.path)
        };
        return Err(vec![Detail {
            path,
            message: format!("不符合网关的路由模型定义：{}", err.message),
        }]);
    }
    Ok(Value::Object(kept))
}

/// Whether `key: value` is the gateway's default for that setting, and so
/// need not be stored.
fn at_default(key: &str, value: &Value) -> bool {
    match key {
        "strategy" => value.as_str() == Some(DEFAULT_STRATEGY),
        "retry_on_429" => value.as_bool() == Some(false),
        "when_all_unavailable" => value.as_str() == Some("fail"),
        "fallback_on_statuses" => value.as_array().is_some_and(Vec::is_empty),
        _ => false,
    }
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
    all_scope: &[crate::tenants::TenantRow],
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
    // Every all-scope tenant reaches this public model from now on — or, for
    // a model whose routing document this pass wrote without targets, still
    // reaches it, listed and answering `upstream_unavailable` — so its keys
    // are rewritten in the same transaction. A key's document names the
    // models by id, so a public model that became callable reaches an
    // all-scope tenant only once this has run; done here, it cannot be
    // later than the routing document it depends on. The tenants' rows are
    // held by the caller, before this public model's row.
    crate::keys::reproject_tenants(&mut *conn, ctx, all_scope).await
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
    /// Live tenants whose **selected** scope names it. A tenant on every
    /// public model is not a reference: its scope is what the catalogue
    /// holds, so the model simply stops being in it.
    pub tenants: i64,
    /// Live keys of **selected**-scope tenants that reach it: those whose
    /// model selection names it, and those with no selection whose tenant's
    /// list names it. A key of an all-scope tenant names no reference
    /// whichever its selection says: the tenant's scope is the catalogue,
    /// and the key's selection narrows within it.
    pub keys: i64,
    /// Its enabled channel models.
    pub channels: i64,
}

/// What still names `id`, as [`PublicModelUse`].
async fn references(conn: &mut PgConnection, id: Uuid) -> Result<PublicModelUse, ApiError> {
    let tenants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM tenants \
         WHERE status <> 'deleted' AND model_scope = 'selected' \
           AND $1 = ANY(allowed_model_ids)",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let keys: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM tenant_keys k \
         WHERE k.revoked_at IS NULL AND EXISTS (SELECT 1 FROM tenants t \
                WHERE t.id = k.tenant_id AND t.status <> 'deleted' \
                  AND t.model_scope = 'selected') \
           AND ($1 = ANY(k.model_ids) \
             OR (k.model_ids IS NULL AND EXISTS (SELECT 1 FROM tenants t \
                  WHERE t.id = k.tenant_id AND t.status <> 'deleted' \
                    AND t.model_scope = 'selected' \
                    AND $1 = ANY(t.allowed_model_ids))))",
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
    /// The routing settings the operator set, as stored; `{}` when none.
    /// `targets` is not part of it, and is read from `served_by`.
    pub routing: Value,
    /// [`PublicModelUse::tenants`]: what refuses a removal, which an
    /// all-scope tenant never does. Not who can call the model; that is
    /// `reachable_tenants`.
    pub tenants: i64,
    /// [`PublicModelUse::keys`], the same way; `reachable_keys` is who can
    /// call it.
    pub keys: i64,
    /// Every tenant whose model scope reaches the model now: an all-scope
    /// tenant, or a selected one whose list names it. `0` for a model with
    /// no live routing document (retired, or never served), which nothing
    /// can call.
    pub reachable_tenants: i64,
    /// Every live key whose grant reaches the model now, read as its
    /// document grants it ([`crate::keys::reaches`]): its selection within
    /// its tenant's scope, or the platform's own name patterns.
    pub reachable_keys: i64,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

/// Who can call public models as things stand, read once per answer so a
/// list of every model costs one read of the tenants and keys, not one per
/// model. Read without a lock: it is shown, never decided on.
struct Reach {
    /// The public models with a live routing document
    /// ([`crate::tenants::public_model_ids`]): the only ones a key can call.
    callable: Vec<Uuid>,
    tenants: Vec<crate::tenants::TenantRow>,
    keys: Vec<crate::keys::KeyRow>,
}

impl Reach {
    async fn read(conn: &mut PgConnection) -> Result<Reach, ApiError> {
        Ok(Reach {
            callable: crate::tenants::public_model_ids(&mut *conn)
                .await
                .map_err(ApiError::from)?,
            tenants: crate::tenants::all(&mut *conn).await?,
            keys: crate::keys::all_live(&mut *conn).await?,
        })
    }

    /// `(tenants, keys)` that reach `row`.
    fn of(&self, row: &PublicModelRow) -> (i64, i64) {
        if row.retired_at.is_some() || !self.callable.contains(&row.id) {
            return (0, 0);
        }
        let tenants = self
            .tenants
            .iter()
            .filter(|t| t.effective_model_ids.contains(&row.id))
            .count();
        let keys = self
            .keys
            .iter()
            .filter(|k| {
                self.tenants
                    .iter()
                    .find(|t| t.id == k.tenant_id)
                    .is_some_and(|t| crate::keys::reaches(t, k, row.id, &row.name))
            })
            .count();
        (
            i64::try_from(tenants).unwrap_or(i64::MAX),
            i64::try_from(keys).unwrap_or(i64::MAX),
        )
    }
}

async fn view(
    conn: &mut PgConnection,
    row: &PublicModelRow,
    reach: &Reach,
) -> Result<PublicModelView, ApiError> {
    let uses = references(conn, row.id).await?;
    let (reachable_tenants, reachable_keys) = reach.of(row);
    Ok(PublicModelView {
        id: row.id,
        name: row.name.clone(),
        enabled: row.enabled,
        served_by: served_by(conn, row.id).await?,
        routing: row.routing.clone(),
        tenants: uses.tenants,
        keys: uses.keys,
        reachable_tenants,
        reachable_keys,
        created_at: row.created_at,
        retired_at: row.retired_at,
    })
}

async fn view_json(conn: &mut PgConnection, row: &PublicModelRow) -> Result<Value, ApiError> {
    let reach = Reach::read(&mut *conn).await?;
    serde_json::to_value(view(conn, row, &reach).await?).map_err(ApiError::internal)
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
                let reach = Reach::read(&mut *conn).await?;
                let mut items = Vec::with_capacity(rows.len());
                for row in &rows {
                    items.push(
                        serde_json::to_value(view(&mut *conn, row, &reach).await?)
                            .map_err(ApiError::internal)?,
                    );
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

/// `PATCH /admin/public-models/{id}` with `{routing: {...}}` → `200
/// PublicModel`: sets the routing settings the public model's generated
/// routing document carries, and regenerates that document in the same
/// transaction.
///
/// The body is the whole set of settings, not a patch of them: a key it
/// omits is a key the model no longer sets, so it falls back to the gateway's
/// default. `targets` is not settable (`422 /routing/targets`): the targets
/// are generated from the accounts that serve the model, so removing one is
/// removing that account's mapping on the account page. A key the gateway's
/// `routing` block does not define is refused by name, and a value the
/// gateway's schema rejects is refused at its own path.
///
/// The write goes through [`regenerate`], the same path a channel edit takes,
/// so the generated document is written by the control plane's own
/// transaction rather than by a `PUT` on a document owned by the public model
/// (which answers `409 managed_resource`, and would be how a later
/// regeneration silently reverted a hand edit).
///
/// The locks are a channel edit's: the all-scope tenants' rows, then the
/// public model's, then the resources lock inside `write_resource`.
pub async fn patch(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(body): ApiJson<PatchBody>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = path_id(&raw)?;
    let routing = checked_routing(&body.routing).map_err(ApiError::validation)?;
    let ctx = WriteCtx::of(&state);
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                let all_scope = crate::tenants::lock_all_scope(&mut *conn).await?;
                let existing = lock_public(&mut *conn, id).await?;
                if existing.retired_at.is_some() {
                    return Err(retired());
                }
                sqlx::query("UPDATE public_models SET routing = $2 WHERE id = $1")
                    .bind(id)
                    .bind(&routing)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "public_model.routing",
                        target_kind: "public_model",
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: json!({"name": existing.name, "routing": routing}),
                    },
                )
                .await?;
                regenerate(&mut *conn, &ctx, id, &all_scope).await?;
                let row = read_public(&mut *conn, id).await?;
                view_json(&mut *conn, &row).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `PATCH /admin/public-models/{id}`: the routing settings, as a whole set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchBody {
    /// The non-target settings of the model's routing block. Required: the
    /// body is the whole set, so an absent one would be an ambiguous
    /// "clear everything" rather than a request anyone wrote on purpose.
    routing: Value,
}

/// What a public model's removal does with the accounts that serve it.
/// The two routes differ only here: a retire holds the model while an enabled
/// account serves it (the operator must mean to stop the name while its
/// mappings stay as they were), and a delete takes the mappings with it, which
/// is what an operator actually wants when they say the name should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Serving {
    /// An enabled channel row refuses the removal: `retire`.
    Holds,
    /// The channel rows are the removal's business, deleted with it: `delete`.
    Removed,
}

impl Serving {
    /// Whether the channel rows this model has refuse the removal.
    fn refuses(self, uses: &PublicModelUse) -> bool {
        self == Serving::Holds && uses.channels > 0
    }
}

/// The tail both removals run once `id`'s row is locked: the reference
/// count, the tombstone of the routing and direct documents, the channel
/// rows, the audit row, and the retired view.
///
/// The caller holds the all-scope tenants' rows, the public model's row and
/// `RESOURCES_LOCK_KEY`, in that order; this function takes no further lock.
/// A grant is the reference that can arrive concurrently — `tenants::update`
/// takes its tenant row, then the resources lock, then validates
/// `allowed_model_ids` against live documents — so a grant that commits first
/// is counted here and refused, and one that gets the lock second finds the
/// model retired.
async fn remove(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    auth: &OperatorAuth,
    row: &PublicModelRow,
    all_scope: &[crate::tenants::TenantRow],
    serving: Serving,
    action: &'static str,
) -> Result<Value, ApiError> {
    let id = row.id;
    let name = row.name.clone();
    let uses = references(&mut *conn, id).await?;
    if uses.tenants > 0 || uses.keys > 0 || serving.refuses(&uses) {
        return Err(ApiError::public_model_in_use(
            serde_json::to_value(uses).map_err(ApiError::internal)?,
        ));
    }
    // Retired first, so the scope an all-scope tenant's keys are rewritten
    // from no longer holds this model: a key document names the models by
    // id, and `write_resource` refuses to delete a model one still names.
    // All in one transaction, so no other reader sees the model retired but
    // still served.
    sqlx::query("UPDATE public_models SET retired_at = now(), enabled = false WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    crate::keys::reproject_tenants(&mut *conn, ctx, all_scope).await?;
    let owner = owner(id);
    // Referrers before referents: the routing document names every direct
    // document by id.
    let live: Vec<ResourceRow> = store::owned_by(&mut *conn, std::slice::from_ref(&owner)).await?;
    let mut models: Vec<Uuid> = live
        .iter()
        .filter(|r| r.kind == Kind::Models && !r.deleted)
        .map(|r| r.id)
        .collect();
    models.sort_by_key(|m| if *m == id { 0 } else { 1 });
    for model in models {
        write_resource(
            &mut *conn,
            ctx,
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
    // The channel rows go once, whichever route ran: a retire's own
    // refusal (`Serving::Holds`) has already seen them gone or disabled.
    sqlx::query("DELETE FROM channel_models WHERE public_model_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    // A retire's row is what it always was — the model's name — because
    // nothing about that path changes for an operator reading the audit log.
    // A delete is the attested action that removes mappings, so who and what
    // it removed is the record: the counts it refused on, or went through.
    let detail = match serving {
        Serving::Holds => json!({ "name": name }),
        Serving::Removed => json!({
            "name": name,
            "tenants": uses.tenants,
            "keys": uses.keys,
            "channels": uses.channels,
        }),
    };
    audit::record(
        &mut *conn,
        AuditEntry {
            actor: Actor::Operator(auth),
            action,
            target_kind: "public_model",
            target_id: Some(id.to_string()),
            tenant_id: None,
            detail,
        },
    )
    .await?;
    let row = read_public(&mut *conn, id).await?;
    view_json(&mut *conn, &row).await
}

/// `POST /admin/public-models/{id}/retire` → `200 PublicModel`: sets
/// `retired_at` once nothing live names it; `409 public_model_in_use` with
/// the counts while something does, and `409 public_model_retired` for a
/// model already retired.
///
/// Retiring removes the documents, the routing document first since it names
/// the direct ones, and the model's channel rows with them: a [`restore`]
/// brings the name back with no account serving it, so the rows hold
/// nothing worth keeping.
///
/// The counts are read under `RESOURCES_LOCK_KEY`, and the delete and the
/// update land while it is held. The public model's row is held before the
/// resources lock, which is the documented order; retire takes no tenant row
/// (`groups::retire` is the same shape).
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
                // An all-scope tenant's keys name this model while it is
                // callable and must stop naming it once it is retired, so
                // their rows are held from here, before the public model's.
                let all_scope = crate::tenants::lock_all_scope(&mut *conn).await?;
                let row = lock_public(&mut *conn, id).await?;
                if row.retired_at.is_some() {
                    return Err(retired());
                }
                refs::lock_resources(&mut *conn).await?;
                remove(
                    conn,
                    &ctx,
                    &auth,
                    &row,
                    &all_scope,
                    Serving::Holds,
                    "public_model.retire",
                )
                .await
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `POST /admin/public-models/{id}/delete` → `200 PublicModel`: the whole
/// job, in one attested action — every account's channel row for the model
/// goes, the routing and direct documents are tombstoned, and `retired_at`
/// is set.
///
/// It is `retire` with one question answered differently: an enabled serving
/// account does **not** refuse it. That is the point — a public model every
/// account serves could otherwise be retired only by unticking it on each
/// account in turn, and the tick was never what the operator wanted.
///
/// What still refuses it is a live tenant grant or a live key selection: the
/// model is named in a document this route does not rewrite, and it answers
/// `409 public_model_in_use` with `{tenants, keys, channels}`. It answers
/// `409 public_model_retired` for a model already retired, as `retire` does.
///
/// The locks are `retire`'s exactly: the all-scope tenants' rows, the public
/// model's row, then `RESOURCES_LOCK_KEY`. A refusal leaves everything as it
/// was — the reference count and the write share one transaction.
pub async fn delete(
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
                let all_scope = crate::tenants::lock_all_scope(&mut *conn).await?;
                let row = lock_public(&mut *conn, id).await?;
                if row.retired_at.is_some() {
                    return Err(retired());
                }
                refs::lock_resources(&mut *conn).await?;
                remove(
                    conn,
                    &ctx,
                    &auth,
                    &row,
                    &all_scope,
                    Serving::Removed,
                    "public_model.delete",
                )
                .await
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `POST /admin/public-models/{id}/restore` → `200 PublicModel`: brings a
/// retired public model back under the id it always had, so the grants,
/// baselines and group rates keyed on that id apply to it again.
///
/// It clears `retired_at`, switches the model on, and regenerates its
/// documents through [`regenerate`], the path a channel edit takes. A retire
/// removed every account's mapping, so the routing document comes back with
/// no target: the model is listed and callable again, and answers
/// `upstream_unavailable` until an account serves it. Every all-scope
/// tenant's keys are rewritten in the same transaction, exactly as when a
/// new public model first becomes callable; a selected tenant's list is an
/// operator's and is not touched. The routing settings the model had are
/// kept.
///
/// `409 public_model_not_retired` for a model that is not retired, `409
/// public_model_name_held` while a live model of any kind holds the name
/// (one created after the retire freed it), and `404` for an id that names
/// nothing.
///
/// The locks are `retire`'s exactly: the all-scope tenants' rows, the public
/// model's row, then `RESOURCES_LOCK_KEY`, taken here because the name check
/// is a plain `SELECT`, as [`create`]'s is.
pub async fn restore(
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
                let all_scope = crate::tenants::lock_all_scope(&mut *conn).await?;
                let row = lock_public(&mut *conn, id).await?;
                if row.retired_at.is_none() {
                    return Err(ApiError::conflict(
                        "public_model_not_retired",
                        "该公共模型没有停用，无需恢复",
                    ));
                }
                refs::lock_resources(&mut *conn).await?;
                let held: Option<Uuid> = sqlx::query_scalar(
                    "SELECT id FROM gw_resources \
                     WHERE kind = $1 AND identity = $2 AND NOT deleted AND id <> $3 LIMIT 1",
                )
                .bind(Kind::Models.segment())
                .bind(&row.name)
                .bind(id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if held.is_some() {
                    return Err(ApiError::conflict(
                        "public_model_name_held",
                        format!(
                            "已有同名的其他模型「{}」，不能恢复；请先删除或改名那个模型",
                            row.name
                        ),
                    ));
                }
                sqlx::query(
                    "UPDATE public_models SET retired_at = NULL, enabled = true WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "public_model.restore",
                        target_kind: "public_model",
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: json!({"name": row.name}),
                    },
                )
                .await?;
                regenerate(&mut *conn, &ctx, id, &all_scope).await?;
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
                // A channel edit can make a public model callable or
                // uncallable, which changes what every all-scope tenant
                // reaches: their rows are taken first, before the public
                // model's, in the documented order.
                let all_scope = crate::tenants::lock_all_scope(&mut *conn).await?;
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
                regenerate(&mut *conn, &ctx, public_model_id, &all_scope).await?;
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

/// The most upstream models one `POST
/// /admin/provider-keys/{id}/channel-models` applies.
///
/// The whole selection is one transaction that holds `RESOURCES_LOCK_KEY`
/// from its first collision check to its commit, and every other document
/// write — a tenant's key create included — queues behind it, failing with
/// `503` once it has waited `lock_timeout` (5 s by default). Each model
/// costs a public-model insert, a channel row, two audit rows and two
/// document writes, so 100 keeps the hold far inside that bound
/// (`a_full_selection_holds_the_resources_lock_briefly`), while still taking
/// a whole provider family from a typical list in one step.
pub const MAX_BULK_MODELS: usize = 100;

/// `POST /admin/provider-keys/{id}/channel-models`: maps upstream models of
/// this account to the public models of the same names, in one transaction.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkBody {
    upstream_models: Vec<String>,
}

/// What a bulk apply did with one upstream model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BulkOutcome {
    /// No public model had the name: one was created, and this account's
    /// row added to it.
    Created,
    /// The public model of that name existed, and this account's row was
    /// added to it.
    Added,
    /// This account already maps the upstream model, or already serves the
    /// public model of that name under another upstream name: its row is
    /// left exactly as it was.
    Unchanged,
}

/// One upstream model of a bulk apply, as the answer lists it.
#[derive(Debug, Clone, Serialize)]
struct BulkItem {
    upstream_model: String,
    public_model_id: Uuid,
    public_model_name: String,
    outcome: BulkOutcome,
}

/// The selection as given: each name trimmed and checked as a public
/// model's name ([`checked_name`]), the first of any repeat kept, with the
/// index it was sent at. `422` naming every name that cannot be one, at
/// `/upstream_models/<index>`; an empty or oversized selection is `422` at
/// `/upstream_models`.
fn checked_selection(raw: &[String]) -> Result<Vec<(usize, String)>, ApiError> {
    if raw.is_empty() {
        return Err(refused("/upstream_models", "请至少选择一个上游模型"));
    }
    let mut problems = Vec::new();
    let mut names: Vec<(usize, String)> = Vec::new();
    for (index, given) in raw.iter().enumerate() {
        match checked_name(given) {
            Ok(name) => {
                if !names.iter().any(|(_, n)| *n == name) {
                    names.push((index, name));
                }
            }
            Err(d) => problems.push(Detail {
                path: format!("/upstream_models/{index}"),
                message: format!("「{}」不能作为公共模型名称：{}", given.trim(), d.message),
            }),
        }
    }
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    if names.len() > MAX_BULK_MODELS {
        return Err(refused(
            "/upstream_models",
            format!("一次最多选择 {MAX_BULK_MODELS} 个上游模型"),
        ));
    }
    Ok(names)
}

/// What the apply decided for one name before it writes anything.
enum Plan {
    /// Create the public model and add this account's row.
    Create,
    /// Add this account's row to the live public model.
    Add(PublicModelRow),
    /// Leave everything as it is: the account already maps it.
    Keep {
        public_model_id: Uuid,
        public_name: String,
    },
}

/// `POST /admin/provider-keys/{id}/channel-models` with `{upstream_models:
/// [name]}` → `200 {items: [{upstream_model, public_model_id,
/// public_model_name, outcome}]}`, one item per distinct name in the order
/// sent.
///
/// For each upstream model: a live public model of the same name is reused,
/// or one is created; this account's channel row is added with the
/// upstream name set to the model, at the priority (0) and weight (1) a
/// ticked row starts with; and every document each affected public model
/// implies is regenerated. A model this account already maps — under any
/// public model, enabled or not — and a public model of that name this
/// account already serves under another upstream name are left as they are
/// (`unchanged`): nothing here edits or duplicates an existing row.
///
/// All or nothing. Every name is checked before the transaction, and every
/// collision under the resources lock before the first write; any refusal
/// is one `422` naming each offending model at `/upstream_models/<index>`,
/// with nothing written: a name that is not a valid public-model name, a
/// public model of that name that is retired, a live model of another kind
/// holding the name, and a public model of that name created while this
/// request waited for the lock (sent again, it is reused). Never more than
/// [`MAX_BULK_MODELS`] names.
///
/// Locks, in the documented order: the live public models it reuses, `FOR
/// UPDATE` in ascending id order, then `RESOURCES_LOCK_KEY`, which it takes
/// itself because its collision checks are plain `SELECT`s, as
/// [`create`]'s is. A public model it creates is inserted under that lock,
/// as [`create`] inserts one, so no other transaction can be waiting for its
/// row. No tenant, group or account row is taken. The account is read under
/// the resources lock, so a delete of it cannot commit between the check
/// and the rows this writes.
pub async fn bulk_channels(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(body): ApiJson<BulkBody>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let provider_key_id = path_id(&raw)?;
    let names = checked_selection(&body.upstream_models)?;
    let ctx = WriteCtx::of(&state);
    let items = state
        .db
        .tx(move |conn| {
            async move {
                let wanted: Vec<String> = names.iter().map(|(_, n)| n.clone()).collect();
                // Every all-scope tenant's row, before any public model's:
                // the rows this apply may make callable are taken after
                // them, in the documented order.
                let all_scope = crate::tenants::lock_all_scope(&mut *conn).await?;
                // The public models this reuses, held before the resources
                // lock. One that is retired while this waits drops out of
                // the result, and is refused below as a retired name.
                let live: Vec<PublicModelRow> = sqlx::query_as(&format!(
                    "SELECT {PUBLIC_COLUMNS} FROM public_models \
                     WHERE name = ANY($1) AND retired_at IS NULL ORDER BY id FOR UPDATE"
                ))
                .bind(&wanted)
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                refs::lock_resources(&mut *conn).await?;

                let Some((display_name, provider)) =
                    provider_key_of(&mut *conn, provider_key_id).await?
                else {
                    return Err(refused("/provider_key_id", "该上游账号不存在或已删除"));
                };
                // This account's rows of live public models, each with the
                // name it asks its upstream for.
                let mine: Vec<(Uuid, String, String)> = sqlx::query_as(
                    "SELECT cm.public_model_id, p.name, COALESCE(cm.upstream_model, p.name) \
                     FROM channel_models cm JOIN public_models p ON p.id = cm.public_model_id \
                     WHERE cm.provider_key_id = $1 AND p.retired_at IS NULL \
                     ORDER BY p.name, cm.public_model_id",
                )
                .bind(provider_key_id)
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                // Every other name a public model has, retired or created
                // since `live` was read, and every live model holding one.
                let other_publics: Vec<(String, bool)> = sqlx::query_as(
                    "SELECT name, retired_at IS NOT NULL FROM public_models \
                     WHERE name = ANY($1) AND NOT (id = ANY($2))",
                )
                .bind(&wanted)
                .bind(live.iter().map(|p| p.id).collect::<Vec<Uuid>>())
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let holders: Vec<(String, Uuid)> = sqlx::query_as(
                    "SELECT identity, id FROM gw_resources \
                     WHERE kind = $1 AND identity = ANY($2) AND NOT deleted",
                )
                .bind(Kind::Models.segment())
                .bind(&wanted)
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;

                let mut plans: Vec<(String, Plan)> = Vec::with_capacity(names.len());
                let mut problems = Vec::new();
                for (index, name) in &names {
                    if let Some((public_model_id, public_name, _)) =
                        mine.iter().find(|(_, _, upstream)| upstream == name)
                    {
                        plans.push((
                            name.clone(),
                            Plan::Keep {
                                public_model_id: *public_model_id,
                                public_name: public_name.clone(),
                            },
                        ));
                        continue;
                    }
                    let public = live.iter().find(|p| p.name == *name);
                    if let Some(public) = public {
                        if mine.iter().any(|(id, _, _)| *id == public.id) {
                            plans.push((
                                name.clone(),
                                Plan::Keep {
                                    public_model_id: public.id,
                                    public_name: public.name.clone(),
                                },
                            ));
                            continue;
                        }
                    }
                    let problem =
                        if let Some((_, retired)) = other_publics.iter().find(|(n, _)| n == name) {
                            Some(if *retired {
                                "同名公共模型已停用，该名称不能再用于公共模型"
                            } else {
                                "刚刚有同名公共模型被创建，请重新获取上游模型后再试"
                            })
                        } else if holders
                            .iter()
                            .any(|(n, id)| n == name && public.is_none_or(|p| p.id != *id))
                        {
                            Some("已有同名的其他模型，不能用作公共模型名称")
                        } else {
                            None
                        };
                    match problem {
                        Some(message) => problems.push(Detail {
                            path: format!("/upstream_models/{index}"),
                            message: format!("「{name}」{message}"),
                        }),
                        None => plans.push((
                            name.clone(),
                            match public {
                                Some(public) => Plan::Add(public.clone()),
                                None => Plan::Create,
                            },
                        )),
                    }
                }
                if !problems.is_empty() {
                    return Err(ApiError::validation(problems));
                }

                let mut items = Vec::with_capacity(plans.len());
                let mut touched: Vec<Uuid> = Vec::new();
                for (name, plan) in plans {
                    let (public_model_id, public_model_name, outcome) = match plan {
                        Plan::Keep {
                            public_model_id,
                            public_name,
                        } => {
                            items.push(BulkItem {
                                upstream_model: name,
                                public_model_id,
                                public_model_name: public_name,
                                outcome: BulkOutcome::Unchanged,
                            });
                            continue;
                        }
                        Plan::Add(public) => (public.id, public.name, BulkOutcome::Added),
                        Plan::Create => {
                            let id = insert_public(&mut *conn, &name, auth.id).await?;
                            audit::record(
                                &mut *conn,
                                AuditEntry {
                                    actor: Actor::Operator(&auth),
                                    action: "public_model.create",
                                    target_kind: "public_model",
                                    target_id: Some(id.to_string()),
                                    tenant_id: None,
                                    detail: json!({"name": name, "bulk": true}),
                                },
                            )
                            .await?;
                            (id, name.clone(), BulkOutcome::Created)
                        }
                    };
                    let row: ChannelModelRow = sqlx::query_as(&format!(
                        "INSERT INTO channel_models \
                         (provider_key_id, public_model_id, upstream_model, priority, weight, \
                          enabled, direct_model_id, created_by) \
                         VALUES ($1, $2, $3, 0, 1, true, $4, $5) RETURNING {CHANNEL_COLUMNS}"
                    ))
                    .bind(provider_key_id)
                    .bind(public_model_id)
                    .bind(&name)
                    .bind(Uuid::new_v4())
                    .bind(Some(auth.id))
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
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
                                "created": true,
                                "bulk": true,
                            }),
                        },
                    )
                    .await?;
                    touched.push(public_model_id);
                    items.push(BulkItem {
                        upstream_model: name,
                        public_model_id,
                        public_model_name,
                        outcome,
                    });
                }
                touched.sort();
                for public_model_id in touched {
                    regenerate(&mut *conn, &ctx, public_model_id, &all_scope).await?;
                }
                Ok::<_, ApiError>(items)
            }
            .boxed()
        })
        .await?;
    let items = serde_json::to_value(items).map_err(ApiError::internal)?;
    Ok(Json(json!({ "items": items })))
}

/// Inserts the public model `name`, created by `operator`; its id. The
/// caller holds `RESOURCES_LOCK_KEY` and has checked the name, as [`create`]
/// does; the unique index still answers a name taken meanwhile.
async fn insert_public(
    conn: &mut PgConnection,
    name: &str,
    operator: Uuid,
) -> Result<Uuid, ApiError> {
    sqlx::query_scalar("INSERT INTO public_models (name, created_by) VALUES ($1, $2) RETURNING id")
        .bind(name)
        .bind(Some(operator))
        .fetch_one(conn)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db)
                if db.code().as_deref() == Some("23505")
                    && db.constraint() == Some("public_models_name_key") =>
            {
                ApiError::public_model_name_taken()
            }
            _ => ApiError::from(DbError::from(e)),
        })
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
                let all_scope = crate::tenants::lock_all_scope(&mut *conn).await?;
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
                regenerate(&mut *conn, &ctx, public_model_id, &all_scope).await
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
