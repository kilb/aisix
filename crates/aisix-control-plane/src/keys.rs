//! Tenant API keys: what a key holds, the gateway document it projects, how
//! one is minted, and the key routes of both APIs.
//!
//! A key is a row of `tenant_keys` and one gateway document,
//! `api_keys/<key id>`, owned by `key:<id>` ([`owner`]). The document is
//! derived from the key, its tenant and its member ([`build_key_doc`]), and
//! written again in the transaction that changes any of them:
//! - the tenant's status, grants, groups or limits: `tenants::project` calls
//!   [`reproject_tenant`] after it writes the limits policy and before it
//!   retires it, so the limit brackets the keys;
//! - a member disabled or enabled again: [`reproject_member`];
//! - a member removed: [`revoke_member`], before their `users` document
//!   goes, since a key document names its user;
//! - a model deleted: `tenants::on_model_deleted` withdraws it from every
//!   key selection and re-projects.
//!
//! Every write to `tenant_keys` holds the key's tenant's lock
//! ([`tenants::lock`]) and only then reaches `write_resource`, which takes
//! the resources lock.
//!
//! The plaintext is returned once, by the request that mints it; only its
//! SHA-256 (`key_hash`) is stored, and no log line, audit row or error
//! carries either. On `/api/*` a key is looked up within the caller's tenant
//! (and, for a member who does not manage keys, among their own) before
//! anything else, so another tenant's key answers exactly as an id that
//! names nothing; tenant views name no provider group and no endpoint
//! family.

use std::collections::{HashMap, HashSet};

use aisix_core::models::ApiKey;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use rand_core::RngCore;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{MemberAuth, OperatorAuth, OperatorRole, KEY_HOLDERS};
use crate::compat;
use crate::db::{Db, DbError};
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::members;
use crate::resources::api::if_match;
use crate::resources::write::WriteCtx;
use crate::resources::Kind;
use crate::tenants::{self, TenantRow, TenantStatus};
use crate::AppState;

/// The endpoint families a customer's key may call in the first phase:
/// chat, messages and their token count, responses, embeddings and the
/// model list, named as `aisix_core::models::EndpointFamily` names them.
pub const FIRST_PHASE_ENDPOINTS: [&str; 6] = [
    "chat_completions",
    "messages",
    "count_tokens",
    "responses",
    "embeddings",
    "models",
];

/// What every minted key starts with.
const KEY_PREFIX: &str = "sk-aisix-";

/// How much of a key's plaintext is kept to show it by (`key_prefix`).
const PREFIX_CHARS: usize = 12;

/// The longest label accepted.
const MAX_LABEL_CHARS: usize = 64;

/// Keys one member may mint per minute, counted from their `key.create`
/// audit rows of the last minute, so the bound holds whichever instance
/// took them. A member mints under their tenant's lock, so two mints of
/// one member cannot both count the tenth. Operators are not bounded.
pub const MINTS_PER_MINUTE: i64 = 10;

const MINT_WINDOW_SECS: f64 = 60.0;

const TARGET_KIND: &str = "api_key";

/// A key's own limits. Unset means the tenant's per-key cap, or no limit
/// without one. Read leniently, since an instance of an older release may
/// read a row a newer one wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
}

/// Who minted a key: `kind` is `member` or `operator`, `id` theirs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCreator {
    pub kind: String,
    pub id: Option<Uuid>,
}

/// One row of `tenant_keys`.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// The member the key belongs to; `None` for a key of the tenant as a
    /// whole.
    pub member_id: Option<Uuid>,
    pub label: String,
    pub key_hash: String,
    pub key_prefix: String,
    /// The models selected from the tenant's grants; `None` follows every
    /// grant.
    pub model_ids: Option<Vec<Uuid>>,
    /// Model name patterns, for the platform's own keys only.
    pub allowed_models: Option<Vec<String>>,
    /// The groups set for this key alone; `None` takes the tenant's.
    pub allowed_provider_groups: Option<Vec<String>>,
    pub rate_limit: KeyLimits,
    /// Switched off on the tenant's side (by a member who may change the
    /// key), whatever the tenant's or the member's state.
    pub disabled: bool,
    /// Switched off by an operator. Only an operator switches it on again.
    pub operator_disabled: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub version: i64,
    pub created_at: DateTime<Utc>,
    pub created_by: KeyCreator,
}

type RawKey = (
    Uuid,
    Uuid,
    Option<Uuid>,
    String,
    String,
    String,
    Option<Vec<Uuid>>,
    Option<Vec<String>>,
    Option<Vec<String>>,
    Value,
    bool,
    bool,
    Option<DateTime<Utc>>,
    i64,
    DateTime<Utc>,
    Value,
);

// `created_by` as one value, so the row stays within the 16 columns a
// tuple decodes.
const COLUMNS: &str = "id, tenant_id, member_id, label, key_hash, key_prefix, model_ids, \
     allowed_models, allowed_provider_groups, rate_limit, disabled, operator_disabled, expires_at, \
     version, created_at, jsonb_build_object('kind', created_by_kind, 'id', created_by)";

impl KeyRow {
    fn from_raw(raw: RawKey) -> Result<KeyRow, ApiError> {
        let (
            id,
            tenant_id,
            member_id,
            label,
            key_hash,
            key_prefix,
            model_ids,
            allowed_models,
            allowed_provider_groups,
            rate_limit,
            disabled,
            operator_disabled,
            expires_at,
            version,
            created_at,
            created_by,
        ) = raw;
        Ok(KeyRow {
            id,
            tenant_id,
            member_id,
            label,
            key_hash,
            key_prefix,
            model_ids,
            allowed_models,
            allowed_provider_groups,
            rate_limit: serde_json::from_value(rate_limit).map_err(ApiError::internal)?,
            disabled,
            operator_disabled,
            expires_at,
            version,
            created_at,
            created_by: serde_json::from_value(created_by).map_err(ApiError::internal)?,
        })
    }
}

/// Who owns the document derived from key `id`.
pub fn owner(id: Uuid) -> String {
    format!("key:{id}")
}

/// A new key: `(plaintext, key_hash)`, the plaintext `sk-aisix-` and 32
/// random bytes in hex, hashed as the gateway hashes a bearer token
/// ([`ApiKey::hash_bearer`]). The randomness is plenty, so a plain SHA-256
/// is the right hash; argon2 is for passwords people choose.
pub fn mint() -> Result<(String, String), ApiError> {
    let mut bytes = [0u8; 32];
    rand_core::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(ApiError::internal)?;
    let plaintext = format!("{KEY_PREFIX}{}", hex::encode(bytes));
    let key_hash = ApiKey::hash_bearer(&plaintext);
    Ok((plaintext, key_hash))
}

/// The part of a plaintext kept to show the key by.
fn prefix_of(plaintext: &str) -> &str {
    plaintext.get(..PREFIX_CHARS).unwrap_or(plaintext)
}

/// The models a key of `t` reaches by id: its selection within the
/// tenant's grants, or every grant without one; sorted, and empty rather
/// than absent when there is none.
fn granted_model_ids(t: &TenantRow, k: &KeyRow) -> Vec<String> {
    let mut ids: Vec<Uuid> = match &k.model_ids {
        Some(selected) => selected
            .iter()
            .filter(|id| t.allowed_model_ids.contains(id))
            .copied()
            .collect(),
        None => t.allowed_model_ids.clone(),
    };
    ids.sort();
    ids.dedup();
    ids.iter().map(Uuid::to_string).collect()
}

/// The provider groups a key of `t` may route to: its own when it sets
/// some, the tenant's otherwise. An empty list here means every upstream.
fn effective_groups(t: &TenantRow, k: &KeyRow) -> Vec<String> {
    match &k.allowed_provider_groups {
        Some(own) if !own.is_empty() => own.clone(),
        _ => t.allowed_provider_groups.clone(),
    }
}

/// A key's limit within the tenant's per-key cap: its own, bounded by the
/// cap, or the cap when it sets none.
fn capped(own: Option<u32>, cap: Option<u32>) -> Option<u32> {
    match (own, cap) {
        (Some(own), Some(cap)) => Some(own.min(cap)),
        (own, cap) => own.or(cap),
    }
}

fn effective_limits(t: &TenantRow, k: &KeyRow) -> KeyLimits {
    KeyLimits {
        concurrency: capped(k.rate_limit.concurrency, t.limits.key_concurrency_max),
        rpm: capped(k.rate_limit.rpm, t.limits.key_rpm_max),
    }
}

/// Whether the gateway refuses a key of `t`: switched off on the tenant's
/// side or by an operator, its tenant not active, or its member no longer
/// holding keys (`member_active` false). A customer key with no provider
/// group would reach every upstream; no customer can be saved so, and one
/// found so stays closed.
fn effective_disabled(t: &TenantRow, k: &KeyRow, member_active: bool) -> bool {
    k.disabled
        || k.operator_disabled
        || t.status != TenantStatus::Active
        || !member_active
        || (!t.is_internal && effective_groups(t, k).is_empty())
}

/// The shape of the documents [`build_key_doc`] writes. A release that
/// changes what a key document carries raises it. Every write of a key's
/// document records the version it was written in (`tenant_keys.doc_version`),
/// and [`catch_up`] rewrites the keys behind it: nothing else rewrites a key
/// whose own state did not change. Kept per row, so a key an older replica
/// writes during a rolling upgrade, or one written before a rollback and a
/// second upgrade, is caught up too.
pub const KEY_DOC_VERSION: i32 = 1;

/// The gateway `api_keys` document of key `k` of tenant `t`, whose member,
/// if it has one, is `member_active`: neither disabled nor removed, and in
/// a role that holds keys ([`KEY_HOLDERS`]; a billing member holds none).
///
/// - `key_hash`, `display_name` (the label), `team_id` (the tenant) and
///   `user_id` (the member, when there is one).
/// - A customer's key names its models by id, always with the list written
///   (`[]` grants none, where an absent list would fall back to names), and
///   may call [`FIRST_PHASE_ENDPOINTS`] only.
/// - The platform's own key names models by its patterns when it has some,
///   and by id otherwise, and calls every endpoint.
/// - `allowed_provider_groups`: the key's own, else the tenant's; left out
///   when there are none, never written empty.
/// - `rate_limit`: the key's limits within the tenant's per-key caps; left
///   out when neither sets any.
/// - `disabled` per [`effective_disabled`].
/// - Never `allowed_agents`, `allowed_routes` or `mcp_access`: a key grants
///   no A2A agent, passthrough route or MCP tool.
pub fn build_key_doc(t: &TenantRow, k: &KeyRow, member_active: bool) -> Value {
    let mut doc = Map::new();
    doc.insert("key_hash".to_string(), json!(k.key_hash));
    doc.insert("display_name".to_string(), json!(k.label));
    doc.insert("team_id".to_string(), json!(t.id.to_string()));
    if let Some(member) = k.member_id {
        doc.insert("user_id".to_string(), json!(member.to_string()));
    }
    match (&k.allowed_models, t.is_internal) {
        (Some(patterns), true) => doc.insert("allowed_models".to_string(), json!(patterns)),
        _ => doc.insert(
            "allowed_model_ids".to_string(),
            json!(granted_model_ids(t, k)),
        ),
    };
    if !t.is_internal {
        doc.insert(
            "allowed_endpoints".to_string(),
            json!(FIRST_PHASE_ENDPOINTS),
        );
    }
    let groups = effective_groups(t, k);
    if !groups.is_empty() {
        doc.insert("allowed_provider_groups".to_string(), json!(groups));
    }
    let limits = effective_limits(t, k);
    if limits != KeyLimits::default() {
        doc.insert("rate_limit".to_string(), json!(limits));
    }
    if let Some(at) = k.expires_at {
        doc.insert("expires_at".to_string(), json!(at));
    }
    doc.insert(
        "disabled".to_string(),
        json!(effective_disabled(t, k, member_active)),
    );
    Value::Object(doc)
}

/// The live keys (not revoked) matching every filter given: the key `id`,
/// the tenant, the member; oldest first.
async fn select(
    conn: &mut PgConnection,
    id: Option<Uuid>,
    tenant_id: Option<Uuid>,
    member_id: Option<Uuid>,
) -> Result<Vec<KeyRow>, ApiError> {
    let raw: Vec<RawKey> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM tenant_keys WHERE revoked_at IS NULL \
           AND ($1::uuid IS NULL OR id = $1) AND ($2::uuid IS NULL OR tenant_id = $2) \
           AND ($3::uuid IS NULL OR member_id = $3) \
         ORDER BY created_at, id"
    ))
    .bind(id)
    .bind(tenant_id)
    .bind(member_id)
    .fetch_all(conn)
    .await
    .map_err(DbError::from)?;
    raw.into_iter().map(KeyRow::from_raw).collect()
}

/// The live key `id`, within the tenant and among the member's keys when
/// they are given; `404 not_found` otherwise, whatever the reason.
async fn live_key(
    conn: &mut PgConnection,
    id: Uuid,
    tenant_id: Option<Uuid>,
    member_id: Option<Uuid>,
) -> Result<KeyRow, ApiError> {
    select(conn, Some(id), tenant_id, member_id)
        .await?
        .pop()
        .ok_or_else(ApiError::not_found)
}

/// The roles of [`KEY_HOLDERS`], as stored.
fn holder_roles() -> Vec<&'static str> {
    KEY_HOLDERS.iter().map(|role| role.as_str()).collect()
}

/// The members of `tenant_id` whose keys do not work: disabled, removed, or
/// in a role that holds no keys.
async fn inactive_members(
    conn: &mut PgConnection,
    tenant_id: Uuid,
) -> Result<HashSet<Uuid>, ApiError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM members WHERE tenant_id = $1 \
           AND (disabled OR removed_at IS NOT NULL OR NOT role = ANY($2))",
    )
    .bind(tenant_id)
    .bind(holder_roles())
    .fetch_all(conn)
    .await
    .map_err(DbError::from)?;
    Ok(ids.into_iter().collect())
}

/// Whether a key bound to `member_id` follows a member whose keys work
/// (neither disabled nor removed, and in a role that holds keys): true for
/// a key bound to nobody.
async fn member_active(conn: &mut PgConnection, member_id: Option<Uuid>) -> Result<bool, ApiError> {
    let Some(member_id) = member_id else {
        return Ok(true);
    };
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT NOT disabled AND removed_at IS NULL AND role = ANY($2) FROM members WHERE id = $1",
    )
    .bind(member_id)
    .bind(holder_roles())
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?;
    Ok(active.unwrap_or(false))
}

/// Writes key `k`'s document as it should now be (nothing when it already
/// is), and records that it is written in shape `version`.
async fn project(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    t: &TenantRow,
    k: &KeyRow,
    member_active: bool,
    version: i32,
) -> Result<(), ApiError> {
    let doc = build_key_doc(t, k, member_active);
    tenants::put(&mut *conn, ctx, Kind::ApiKeys, k.id, &doc, &owner(k.id)).await?;
    sqlx::query("UPDATE tenant_keys SET doc_version = $2 WHERE id = $1 AND doc_version <> $2")
        .bind(k.id)
        .bind(version)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(())
}

/// Rewrites the document of every live key of `tenant_id` from the tenant's
/// row as it now stands. Called by `tenants::project`, at its key step, in
/// the transaction of the change, which holds the tenant's lock.
pub async fn reproject_tenant(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant_id: Uuid,
) -> Result<(), ApiError> {
    reproject_tenant_at(conn, ctx, tenant_id, KEY_DOC_VERSION).await
}

/// [`reproject_tenant`], recording the documents as written in shape
/// `version`.
async fn reproject_tenant_at(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant_id: Uuid,
    version: i32,
) -> Result<(), ApiError> {
    let tenant = tenants::lock(&mut *conn, tenant_id).await?;
    let inactive = inactive_members(&mut *conn, tenant_id).await?;
    for key in select(&mut *conn, None, Some(tenant_id), None).await? {
        let active = key.member_id.is_none_or(|m| !inactive.contains(&m));
        project(&mut *conn, ctx, &tenant, &key, active, version).await?;
    }
    Ok(())
}

/// Rewrites the document of every live key of member `member_id`, after
/// the member was disabled or enabled again, in that transaction. The
/// caller holds the member's tenant's lock.
pub async fn reproject_member(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    member_id: Uuid,
) -> Result<(), ApiError> {
    let tenant_id: Uuid = sqlx::query_scalar("SELECT tenant_id FROM members WHERE id = $1")
        .bind(member_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::from)?
        .ok_or_else(ApiError::not_found)?;
    let tenant = tenants::lock(&mut *conn, tenant_id).await?;
    let active = member_active(&mut *conn, Some(member_id)).await?;
    for key in select(&mut *conn, None, Some(tenant_id), Some(member_id)).await? {
        project(&mut *conn, ctx, &tenant, &key, active, KEY_DOC_VERSION).await?;
    }
    Ok(())
}

/// Revokes every live key of member `member_id`, who is being removed: each
/// is marked revoked and its document deleted, in the removal's transaction
/// and before the member's `users` document, which the key documents name.
/// The caller holds the member's tenant's lock. The keys revoked, in id
/// order.
pub async fn revoke_member(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    member_id: Uuid,
) -> Result<Vec<Uuid>, ApiError> {
    let mut revoked: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE tenant_keys SET revoked_at = now(), version = version + 1 \
         WHERE member_id = $1 AND revoked_at IS NULL RETURNING id",
    )
    .bind(member_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    revoked.sort();
    for id in &revoked {
        tenants::retire(&mut *conn, ctx, Kind::ApiKeys, *id, &owner(*id)).await?;
    }
    Ok(revoked)
}

/// What one [`catch_up`] did, by tenant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatchUp {
    /// Tenants whose keys were rewritten.
    pub rewritten: usize,
    /// Tenants the gateway-version gate refused, or that stayed busy past
    /// the lock timeout, left for a later pass.
    pub deferred: usize,
    /// Tenants whose rewrite failed for another reason, each logged with
    /// it, in the order they were tried; a later pass tries them again.
    pub failed: Vec<Uuid>,
}

/// Rewrites the documents of every tenant that holds a live key whose
/// document is behind `version` (its `doc_version`; the running release
/// passes [`KEY_DOC_VERSION`]). Runs at boot and on every housekeeping
/// tick, on every instance; while nothing is behind it costs one indexed
/// query.
///
/// Each tenant is one transaction of its own (its lock, then the resources
/// lock for each write), so a pass holds no lock across tenants, and a
/// write anywhere else waits for one tenant's rewrite at most. Two
/// instances running a pass together each take a tenant in turn, and the
/// second finds it caught up.
///
/// A tenant the gateway-version gate refuses (an older gateway is still
/// registered, as during every upgrade) keeps its documents and its
/// `doc_version`, is logged, and is rewritten by a later pass once that
/// gateway is gone; so is one held by another transaction for longer than
/// the lock timeout. A tenant that fails otherwise is logged, listed as
/// failed, and tried again by every later pass. Only the first query's
/// failure is an error.
pub async fn catch_up(db: &Db, ctx: &WriteCtx, version: i32) -> Result<CatchUp, ApiError> {
    let behind: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT tenant_id FROM tenant_keys \
         WHERE revoked_at IS NULL AND doc_version < $1 ORDER BY tenant_id",
    )
    .bind(version)
    .fetch_all(db.pool())
    .await
    .map_err(DbError::from)?;
    let mut outcome = CatchUp::default();
    for tenant_id in behind {
        let ctx = WriteCtx {
            mk: crate::crypto::MasterKey(ctx.mk.0),
            env: ctx.env.clone(),
            cp_version: ctx.cp_version.clone(),
            stale_secs: ctx.stale_secs,
        };
        let rewritten = db
            .tx(move |conn| {
                async move {
                    tenants::lock(&mut *conn, tenant_id).await?;
                    reproject_tenant_at(&mut *conn, &ctx, tenant_id, version).await
                }
                .boxed()
            })
            .await;
        match rewritten {
            Ok(()) => outcome.rewritten += 1,
            Err(e) if compat::refused(&e) => {
                outcome.deferred += 1;
                tracing::warn!(
                    tenant = %tenant_id,
                    "a tenant's key documents were not rewritten: a registered gateway is too \
                     old for them; a later pass tries again"
                );
            }
            // Busy past the lock timeout: contention, not a fault.
            Err(e) if e.status == StatusCode::SERVICE_UNAVAILABLE => {
                outcome.deferred += 1;
                tracing::warn!(
                    tenant = %tenant_id,
                    "a tenant's key documents were not rewritten: the tenant stayed busy; a \
                     later pass tries again"
                );
            }
            Err(e) => {
                outcome.failed.push(tenant_id);
                tracing::warn!(
                    tenant = %tenant_id,
                    code = e.code,
                    reason = %e.message,
                    details = ?e.details,
                    "a tenant's key documents could not be rewritten"
                );
            }
        }
    }
    Ok(outcome)
}

fn detail(path: &str, message: impl Into<String>) -> Detail {
    Detail {
        path: path.to_string(),
        message: message.into(),
    }
}

/// The label to store: trimmed, 1 to 64 characters, no control characters.
fn checked_label(raw: &str) -> Result<String, Detail> {
    let label = raw.trim();
    if label.is_empty() {
        return Err(detail("/label", "名称不能为空"));
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(detail(
            "/label",
            format!("名称不能超过 {MAX_LABEL_CHARS} 个字符"),
        ));
    }
    if label.chars().any(char::is_control) {
        return Err(detail("/label", "名称不能包含控制字符"));
    }
    Ok(label.to_string())
}

/// The models `raw` selects, sorted without repeats, once each is one of
/// the tenant's grants; otherwise a detail at `/model_ids/<i>` for each that
/// is not. A model the tenant is not granted answers as an id that names
/// nothing.
fn checked_model_ids(t: &TenantRow, raw: &[String]) -> Result<Vec<Uuid>, Vec<Detail>> {
    let mut ids = Vec::with_capacity(raw.len());
    let mut problems = Vec::new();
    for (i, entry) in raw.iter().enumerate() {
        match Uuid::parse_str(entry) {
            Ok(id) if t.allowed_model_ids.contains(&id) => ids.push(id),
            _ => problems.push(detail(&format!("/model_ids/{i}"), "租户没有这个模型")),
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// A key's limits as a request body gives them.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsBody {
    concurrency: Option<u32>,
    rpm: Option<u32>,
}

/// `body` once each limit is above zero and within the tenant's per-key
/// cap; otherwise a detail at `/rate_limit/<field>` for each that is not.
fn checked_limits(t: &TenantRow, body: LimitsBody) -> Result<KeyLimits, Vec<Detail>> {
    let problems: Vec<Detail> = [
        (
            "concurrency",
            body.concurrency,
            t.limits.key_concurrency_max,
        ),
        ("rpm", body.rpm, t.limits.key_rpm_max),
    ]
    .into_iter()
    .filter_map(|(field, value, cap)| {
        let path = format!("/rate_limit/{field}");
        match (value, cap) {
            (Some(0), _) => Some(detail(&path, "限制必须大于 0；不限制请省略该字段")),
            (Some(v), Some(cap)) if v > cap => {
                Some(detail(&path, format!("不能超过租户的单密钥上限 {cap}")))
            }
            _ => None,
        }
    })
    .collect();
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(KeyLimits {
        concurrency: body.concurrency,
        rpm: body.rpm,
    })
}

/// The expiry `raw` names: an RFC 3339 time still to come.
fn checked_expiry(raw: &str) -> Result<DateTime<Utc>, Detail> {
    let at = DateTime::parse_from_rfc3339(raw)
        .map_err(|_| {
            detail(
                "/expires_at",
                "必须是 RFC 3339 时间，例如 2030-01-01T00:00:00Z",
            )
        })?
        .with_timezone(&Utc);
    if at <= Utc::now() {
        return Err(detail("/expires_at", "过期时间必须晚于当前时间"));
    }
    Ok(at)
}

/// What a create or an update sets, as its body gives it: `None` leaves a
/// field as it is, and for a field that can be cleared, `Some(None)`
/// clears it.
#[derive(Default)]
struct Changes {
    label: Option<String>,
    model_ids: Option<Option<Vec<String>>>,
    allowed_models: Option<Option<Vec<String>>>,
    allowed_provider_groups: Option<Option<Vec<String>>>,
    rate_limit: Option<Option<LimitsBody>>,
    /// The tenant's switch, set on `/api`.
    disabled: Option<bool>,
    /// The operators' switch, set on `/admin`.
    operator_disabled: Option<bool>,
    expires_at: Option<Option<String>>,
}

impl Changes {
    /// The fields given, by name, which is all an audit row records.
    fn fields(&self) -> Vec<&'static str> {
        [
            ("allowed_models", self.allowed_models.is_some()),
            (
                "allowed_provider_groups",
                self.allowed_provider_groups.is_some(),
            ),
            (
                "disabled",
                self.disabled.is_some() || self.operator_disabled.is_some(),
            ),
            ("expires_at", self.expires_at.is_some()),
            ("label", self.label.is_some()),
            ("model_ids", self.model_ids.is_some()),
            ("rate_limit", self.rate_limit.is_some()),
        ]
        .into_iter()
        .filter_map(|(field, given)| given.then_some(field))
        .collect()
    }

    /// What an audit row records of the changes: the fields given, and the
    /// value of the switch when it was set (the tenant's on `/api`, the
    /// operators' on `/admin`, as the actor tells).
    fn audit_detail(&self) -> Value {
        let mut detail = json!({ "fields": self.fields() });
        if let Some(disabled) = self.disabled.or(self.operator_disabled) {
            detail["disabled"] = json!(disabled);
        }
        detail
    }

    /// Writes the changes onto `key`, each checked against its tenant `t`:
    /// a `422` with a detail for every field that cannot be set so, and
    /// `key` unchanged then. Only the fields given are checked, so a key
    /// whose stored limits a lowered cap now bounds can still be renamed.
    fn apply(self, t: &TenantRow, key: &mut KeyRow) -> Result<(), ApiError> {
        let mut next = key.clone();
        let mut problems = Vec::new();
        if let Some(raw) = self.label {
            match checked_label(&raw) {
                Ok(label) => next.label = label,
                Err(d) => problems.push(d),
            }
        }
        if let Some(raw) = self.model_ids {
            match raw.map(|raw| checked_model_ids(t, &raw)).transpose() {
                Ok(ids) => next.model_ids = ids,
                Err(found) => problems.extend(found),
            }
        }
        if let Some(raw) = self.allowed_models {
            if raw.is_some() && !t.is_internal {
                problems.push(detail(
                    "/allowed_models",
                    "只有平台内部租户的密钥可以按名称模式授权模型",
                ));
            } else {
                next.allowed_models = raw;
            }
        }
        if let Some(raw) = self.allowed_provider_groups {
            let at = "/allowed_provider_groups";
            match raw
                .map(|groups| tenants::checked_groups(&groups, at))
                .transpose()
            {
                Ok(Some(groups)) if groups.is_empty() && !t.is_internal => {
                    problems.push(tenants::no_groups(at))
                }
                Ok(groups) => next.allowed_provider_groups = groups,
                Err(found) => problems.extend(found),
            }
        }
        if let Some(raw) = self.rate_limit {
            match raw.map(|body| checked_limits(t, body)).transpose() {
                Ok(limits) => next.rate_limit = limits.unwrap_or_default(),
                Err(found) => problems.extend(found),
            }
        }
        if let Some(disabled) = self.disabled {
            next.disabled = disabled;
        }
        if let Some(disabled) = self.operator_disabled {
            next.operator_disabled = disabled;
        }
        if let Some(raw) = self.expires_at {
            match raw.as_deref().map(checked_expiry).transpose() {
                Ok(at) => next.expires_at = at,
                Err(d) => problems.push(d),
            }
        }
        if !problems.is_empty() {
            return Err(ApiError::validation(problems));
        }
        *key = next;
        Ok(())
    }
}

/// For a field that can be cleared: present, with `null` read as
/// `Some(None)`. With `#[serde(default)]`, an absent field is `None`.
fn present<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The live member `raw` names in tenant `tenant_id`, in a role that holds
/// keys, for a key to be bound to; `422` at `/member_id` otherwise,
/// whatever the reason, so a member of another tenant answers as an id that
/// names nothing.
async fn bound_member(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    raw: &str,
) -> Result<Uuid, ApiError> {
    let refused = || {
        ApiError::validation(vec![detail(
            "/member_id",
            "成员不存在，或该成员的角色不能持有密钥",
        )])
    };
    let id = Uuid::parse_str(raw).map_err(|_| refused())?;
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM members \
         WHERE id = $1 AND tenant_id = $2 AND removed_at IS NULL AND role = ANY($3))",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(holder_roles())
    .fetch_one(conn)
    .await
    .map_err(DbError::from)?;
    if live {
        Ok(id)
    } else {
        Err(refused())
    }
}

/// `429 busy` once member `member_id` has minted [`MINTS_PER_MINUTE`] keys
/// in the last minute. Revoking a key gives no mint back.
async fn refuse_mint_flood(conn: &mut PgConnection, member_id: Uuid) -> Result<(), ApiError> {
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log \
         WHERE action = 'key.create' AND actor_kind = 'member' AND actor_id = $1 \
           AND at > now() - make_interval(secs => $2)",
    )
    .bind(member_id)
    .bind(MINT_WINDOW_SECS)
    .fetch_one(conn)
    .await
    .map_err(DbError::from)?;
    if recent >= MINTS_PER_MINUTE {
        return Err(ApiError {
            message: "创建密钥过于频繁，请稍后再试".to_string(),
            ..ApiError::busy()
        });
    }
    Ok(())
}

fn audit_entry<'a>(
    actor: Actor<'a>,
    action: &'static str,
    key: &KeyRow,
    detail: Value,
) -> AuditEntry<'a> {
    AuditEntry {
        actor,
        action,
        target_kind: TARGET_KIND,
        target_id: Some(key.id.to_string()),
        tenant_id: Some(key.tenant_id),
        detail,
    }
}

/// Mints a key of tenant `t`, bound to `member_id` (already checked to be a
/// live member of `t`), as `changes` set it, writes its document and
/// records it; the key, and the plaintext, which is shown this once. The
/// caller holds `t`'s lock.
async fn create_key(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    t: &TenantRow,
    member_id: Option<Uuid>,
    changes: Changes,
    by: Actor<'_>,
) -> Result<(KeyRow, String), ApiError> {
    let fields = changes.fields();
    let mut key = KeyRow {
        id: Uuid::new_v4(),
        tenant_id: t.id,
        member_id,
        label: String::new(),
        key_hash: String::new(),
        key_prefix: String::new(),
        model_ids: None,
        allowed_models: None,
        allowed_provider_groups: None,
        rate_limit: KeyLimits::default(),
        disabled: false,
        operator_disabled: false,
        expires_at: None,
        version: 1,
        created_at: Utc::now(),
        created_by: KeyCreator {
            kind: String::new(),
            id: None,
        },
    };
    changes.apply(t, &mut key)?;
    let (plaintext, key_hash) = mint()?;
    let (created_by_kind, created_by) = match by {
        Actor::Operator(op) => ("operator", Some(op.id)),
        Actor::Member { id, .. } => ("member", Some(id)),
        _ => ("system", None),
    };
    let rate_limit = serde_json::to_value(key.rate_limit).map_err(ApiError::internal)?;
    let raw: RawKey = sqlx::query_as(&format!(
        "INSERT INTO tenant_keys (id, tenant_id, member_id, label, key_hash, key_prefix, \
           model_ids, allowed_models, allowed_provider_groups, rate_limit, disabled, \
           operator_disabled, expires_at, created_by_kind, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         RETURNING {COLUMNS}"
    ))
    .bind(key.id)
    .bind(key.tenant_id)
    .bind(key.member_id)
    .bind(&key.label)
    .bind(&key_hash)
    .bind(prefix_of(&plaintext))
    .bind(&key.model_ids)
    .bind(&key.allowed_models)
    .bind(&key.allowed_provider_groups)
    .bind(rate_limit)
    .bind(key.disabled)
    .bind(key.operator_disabled)
    .bind(key.expires_at)
    .bind(created_by_kind)
    .bind(created_by)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let key = KeyRow::from_raw(raw)?;
    let active = member_active(&mut *conn, key.member_id).await?;
    project(&mut *conn, ctx, t, &key, active, KEY_DOC_VERSION).await?;
    audit::record(
        conn,
        audit_entry(
            by,
            "key.create",
            &key,
            json!({ "member_id": key.member_id, "fields": fields }),
        ),
    )
    .await?;
    Ok((key, plaintext))
}

/// Changes `key` of tenant `t` as `changes` set it, writes its document and
/// records it. The caller holds `t`'s lock and has checked the version.
async fn update_key(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    t: &TenantRow,
    mut key: KeyRow,
    changes: Changes,
    by: Actor<'_>,
) -> Result<KeyRow, ApiError> {
    let detail = changes.audit_detail();
    let limits_given = changes.rate_limit.is_some();
    changes.apply(t, &mut key)?;
    // Limits left out keep what is stored, including fields a newer release
    // added that this one does not read.
    let rate_limit = limits_given
        .then(|| serde_json::to_value(key.rate_limit))
        .transpose()
        .map_err(ApiError::internal)?;
    let raw: RawKey = sqlx::query_as(&format!(
        "UPDATE tenant_keys SET label = $2, model_ids = $3, allowed_models = $4, \
           allowed_provider_groups = $5, rate_limit = COALESCE($6, rate_limit), disabled = $7, \
           operator_disabled = $8, expires_at = $9, version = version + 1 \
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(key.id)
    .bind(&key.label)
    .bind(&key.model_ids)
    .bind(&key.allowed_models)
    .bind(&key.allowed_provider_groups)
    .bind(rate_limit)
    .bind(key.disabled)
    .bind(key.operator_disabled)
    .bind(key.expires_at)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let key = KeyRow::from_raw(raw)?;
    let active = member_active(&mut *conn, key.member_id).await?;
    project(&mut *conn, ctx, t, &key, active, KEY_DOC_VERSION).await?;
    audit::record(conn, audit_entry(by, "key.update", &key, detail)).await?;
    Ok(key)
}

/// Revokes `key`: marked revoked, so it leaves every list, and its document
/// deleted. The caller holds its tenant's lock and has checked the version.
async fn revoke_key(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    key: &KeyRow,
    by: Actor<'_>,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE tenant_keys SET revoked_at = now(), version = version + 1 WHERE id = $1")
        .bind(key.id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    tenants::retire(&mut *conn, ctx, Kind::ApiKeys, key.id, &owner(key.id)).await?;
    audit::record(conn, audit_entry(by, "key.revoke", key, json!({}))).await?;
    Ok(())
}

/// `412 version_mismatch` unless `expected` (what `If-Match` named, or why
/// it named nothing usable) is `key`'s version.
fn check_version(key: &KeyRow, expected: Result<i64, ApiError>) -> Result<(), ApiError> {
    if expected? != key.version {
        return Err(ApiError::version_mismatch(key.version));
    }
    Ok(())
}

/// A malformed id names no key.
fn key_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::not_found())
}

/// Who a tenant key belongs to, as a tenant sees it.
#[derive(Debug, Serialize)]
pub struct KeyOwner {
    id: Uuid,
    display_name: Option<String>,
}

/// `TenantKey` in the tenant API: no provider group, no endpoint family.
#[derive(Debug, Serialize)]
pub struct TenantKeyView {
    id: Uuid,
    label: String,
    prefix: String,
    owner: Option<KeyOwner>,
    model_ids: Option<Vec<Uuid>>,
    rate_limit: KeyLimits,
    /// The tenant's own switch, which `PATCH /api/keys/{id}` sets.
    disabled: bool,
    /// Switched off by the platform; only the platform switches it on.
    disabled_by_platform: bool,
    expires_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    version: i64,
}

async fn tenant_views(
    conn: &mut PgConnection,
    keys: Vec<KeyRow>,
) -> Result<Vec<TenantKeyView>, ApiError> {
    let members: Vec<Uuid> = keys.iter().filter_map(|k| k.member_id).collect();
    let names: HashMap<Uuid, Option<String>> = sqlx::query_as::<_, (Uuid, Option<String>)>(
        "SELECT id, display_name FROM members WHERE id = ANY($1)",
    )
    .bind(&members)
    .fetch_all(conn)
    .await
    .map_err(DbError::from)?
    .into_iter()
    .collect();
    Ok(keys
        .into_iter()
        .map(|k| TenantKeyView {
            id: k.id,
            label: k.label,
            prefix: k.key_prefix,
            owner: k.member_id.map(|id| KeyOwner {
                id,
                display_name: names.get(&id).cloned().flatten(),
            }),
            model_ids: k.model_ids,
            rate_limit: k.rate_limit,
            disabled: k.disabled,
            disabled_by_platform: k.operator_disabled,
            expires_at: k.expires_at,
            created_at: k.created_at,
            version: k.version,
        })
        .collect())
}

async fn tenant_view(conn: &mut PgConnection, key: KeyRow) -> Result<TenantKeyView, ApiError> {
    tenant_views(conn, vec![key])
        .await?
        .pop()
        .ok_or_else(|| ApiError::internal("a key view went missing"))
}

/// `AdminKey` in the operator API: the key as set, and what the gateway is
/// given for it (`effective_*`).
#[derive(Debug, Serialize)]
pub struct AdminKeyView {
    id: Uuid,
    tenant_id: Uuid,
    label: String,
    prefix: String,
    member_id: Option<Uuid>,
    model_ids: Option<Vec<Uuid>>,
    allowed_models: Option<Vec<String>>,
    allowed_provider_groups: Option<Vec<String>>,
    effective_provider_groups: Vec<String>,
    rate_limit: KeyLimits,
    /// The operators' switch, which `PATCH /admin/keys/{id}` sets: the key
    /// stays off while it is set, whatever the tenant does.
    disabled: bool,
    /// The tenant's own switch, which operators do not set.
    disabled_by_tenant: bool,
    effective_disabled: bool,
    expires_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    /// Who minted it, so a key a member minted before they were demoted or
    /// removed can be found.
    created_by: KeyCreator,
    version: i64,
}

fn admin_view(t: &TenantRow, k: KeyRow, member_active: bool) -> AdminKeyView {
    AdminKeyView {
        effective_provider_groups: effective_groups(t, &k),
        effective_disabled: effective_disabled(t, &k, member_active),
        id: k.id,
        tenant_id: k.tenant_id,
        label: k.label,
        prefix: k.key_prefix,
        member_id: k.member_id,
        model_ids: k.model_ids,
        allowed_models: k.allowed_models,
        allowed_provider_groups: k.allowed_provider_groups,
        rate_limit: k.rate_limit,
        disabled: k.operator_disabled,
        disabled_by_tenant: k.disabled,
        expires_at: k.expires_at,
        created_at: k.created_at,
        created_by: k.created_by,
        version: k.version,
    }
}

async fn one_admin_view(
    conn: &mut PgConnection,
    t: &TenantRow,
    key: KeyRow,
) -> Result<AdminKeyView, ApiError> {
    let active = member_active(&mut *conn, key.member_id).await?;
    Ok(admin_view(t, key, active))
}

/// The caller as they stand now, under their tenant's lock, as a member who
/// holds keys of an active tenant, and that tenant.
async fn holder(
    conn: &mut PgConnection,
    auth: &MemberAuth,
) -> Result<(MemberAuth, TenantRow), ApiError> {
    let auth = members::current(&mut *conn, auth, KEY_HOLDERS).await?;
    let tenant = tenants::lock(conn, auth.tenant_id).await?;
    Ok((auth, tenant))
}

/// The member whose keys alone `auth` may see and change, or `None` for one
/// who manages every key of the tenant.
fn own_only(auth: &MemberAuth) -> Option<Uuid> {
    (!auth.can_manage_all_keys()).then_some(auth.id)
}

fn member_actor(auth: &MemberAuth) -> Actor<'_> {
    Actor::Member {
        id: auth.id,
        email: &auth.email,
    }
}

/// An error as `/api` answers it: a refusal by the gateway-version gate is
/// a neutral `503 unavailable`, so a tenant learns nothing of the gateways.
/// Every `/api` route whose writes reach key documents maps its answer
/// through this.
pub(crate) fn on_api(e: ApiError) -> ApiError {
    if compat::refused(&e) {
        tracing::info!("a tenant key write waits for the gateways to be upgraded");
        return ApiError::unavailable();
    }
    e
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateKey {
    label: String,
    /// Left out: the caller. `null`: nobody. A member of the tenant: only
    /// an owner or an admin binds a key to someone else.
    #[serde(default, deserialize_with = "present")]
    member_id: Option<Option<String>>,
    model_ids: Option<Vec<String>>,
    rate_limit: Option<LimitsBody>,
    expires_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateKey {
    label: Option<String>,
    #[serde(default, deserialize_with = "present")]
    model_ids: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "present")]
    rate_limit: Option<Option<LimitsBody>>,
    disabled: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    expires_at: Option<Option<String>>,
}

impl From<UpdateKey> for Changes {
    fn from(req: UpdateKey) -> Changes {
        Changes {
            label: req.label,
            model_ids: req.model_ids,
            rate_limit: req.rate_limit,
            disabled: req.disabled,
            expires_at: req.expires_at,
            ..Changes::default()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminCreateKey {
    label: String,
    /// Left out or `null`: nobody.
    member_id: Option<String>,
    model_ids: Option<Vec<String>>,
    /// The platform's own tenant only.
    allowed_models: Option<Vec<String>>,
    allowed_provider_groups: Option<Vec<String>>,
    rate_limit: Option<LimitsBody>,
    expires_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminUpdateKey {
    label: Option<String>,
    #[serde(default, deserialize_with = "present")]
    model_ids: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "present")]
    allowed_models: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "present")]
    allowed_provider_groups: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "present")]
    rate_limit: Option<Option<LimitsBody>>,
    disabled: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    expires_at: Option<Option<String>>,
}

impl From<AdminUpdateKey> for Changes {
    fn from(req: AdminUpdateKey) -> Changes {
        Changes {
            label: req.label,
            model_ids: req.model_ids,
            allowed_models: req.allowed_models,
            allowed_provider_groups: req.allowed_provider_groups,
            rate_limit: req.rate_limit,
            disabled: None,
            operator_disabled: req.disabled,
            expires_at: req.expires_at,
        }
    }
}

/// `GET /api/keys` → `{items: [TenantKey]}`: every live key of the tenant
/// for an owner or an admin, a member's own for a member.
pub async fn list(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(KEY_HOLDERS)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let keys = select(&mut conn, None, Some(auth.tenant_id), own_only(&auth)).await?;
    let items = tenant_views(&mut conn, keys).await?;
    Ok(Json(json!({ "items": items })))
}

/// `GET /api/keys/{id}` → `TenantKey` and `ETag: "<version>"`.
pub async fn get(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    auth.require_active()?;
    auth.require(KEY_HOLDERS)?;
    let id = key_id(&id)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let key = live_key(&mut conn, id, Some(auth.tenant_id), own_only(&auth)).await?;
    let etag = format!("\"{}\"", key.version);
    let view = tenant_view(&mut conn, key).await?;
    Ok(([(header::ETAG, etag)], Json(view)).into_response())
}

/// `POST /api/keys` with `{label, member_id?, model_ids?, rate_limit?,
/// expires_at?}` → `201 {key: TenantKey, plaintext}`. At most
/// [`MINTS_PER_MINUTE`] a minute per member (`429 busy`); `503
/// unavailable` while a registered gateway is too old for the key's
/// fields.
pub async fn create(
    auth: MemberAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<CreateKey>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require_active()?;
    auth.require(KEY_HOLDERS)?;
    let ctx = WriteCtx::of(&state);
    let (view, plaintext) = state
        .db
        .tx(move |conn| {
            async move {
                let (auth, tenant) = holder(&mut *conn, &auth).await?;
                refuse_mint_flood(&mut *conn, auth.id).await?;
                let member_id = match req.member_id {
                    None => Some(auth.id),
                    // A member mints for themselves only.
                    Some(chosen) if !auth.can_manage_all_keys() => {
                        let chosen = chosen.as_deref().and_then(|raw| Uuid::parse_str(raw).ok());
                        if chosen != Some(auth.id) {
                            return Err(ApiError::forbidden());
                        }
                        Some(auth.id)
                    }
                    Some(None) => None,
                    Some(Some(raw)) => Some(bound_member(&mut *conn, tenant.id, &raw).await?),
                };
                let changes = Changes {
                    label: Some(req.label),
                    model_ids: req.model_ids.map(Some),
                    rate_limit: req.rate_limit.map(Some),
                    expires_at: req.expires_at.map(Some),
                    ..Changes::default()
                };
                let (key, plaintext) = create_key(
                    &mut *conn,
                    &ctx,
                    &tenant,
                    member_id,
                    changes,
                    member_actor(&auth),
                )
                .await?;
                Ok::<_, ApiError>((tenant_view(conn, key).await?, plaintext))
            }
            .boxed()
        })
        .await
        .map_err(on_api)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "key": view, "plaintext": plaintext })),
    ))
}

/// `PATCH /api/keys/{id}` with `{label?, model_ids?, rate_limit?,
/// disabled?, expires_at?}` and `If-Match` → `200 TenantKey`. The key is
/// looked up before `If-Match` is read, so `428` and `412` answer only for
/// the caller's own keys. `disabled` is the tenant's own switch: switching
/// it back on while the platform's switch is set is `403 forbidden`, and
/// `false` for a switch already off is no change.
pub async fn update(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<UpdateKey>,
) -> Result<Json<TenantKeyView>, ApiError> {
    auth.require_active()?;
    auth.require(KEY_HOLDERS)?;
    let id = key_id(&id)?;
    let expected = if_match(&headers);
    let ctx = WriteCtx::of(&state);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let (auth, tenant) = holder(&mut *conn, &auth).await?;
                let key = live_key(&mut *conn, id, Some(tenant.id), own_only(&auth)).await?;
                check_version(&key, expected)?;
                // The platform's switch is the platform's to undo.
                // Only a change of the tenant's own switch is refused: one
                // already off is no change.
                if key.operator_disabled && key.disabled && req.disabled == Some(false) {
                    return Err(ApiError {
                        message: "该密钥已被平台停用，只有平台可以重新启用".to_string(),
                        ..ApiError::forbidden()
                    });
                }
                let key = update_key(
                    &mut *conn,
                    &ctx,
                    &tenant,
                    key,
                    req.into(),
                    member_actor(&auth),
                )
                .await?;
                tenant_view(conn, key).await
            }
            .boxed()
        })
        .await
        .map_err(on_api)?;
    Ok(Json(view))
}

/// `DELETE /api/keys/{id}` with `If-Match` → `204`: the key is revoked.
/// Open while the tenant is pending or suspended too: a leaked key must not
/// come back with a reactivation, which opens only the keys still live.
pub async fn revoke(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    auth.require(KEY_HOLDERS)?;
    let id = key_id(&id)?;
    let expected = if_match(&headers);
    let ctx = WriteCtx::of(&state);
    state
        .db
        .tx(move |conn| {
            async move {
                let auth = members::current_in_any_state(&mut *conn, &auth, KEY_HOLDERS).await?;
                let key = live_key(&mut *conn, id, Some(auth.tenant_id), own_only(&auth)).await?;
                check_version(&key, expected)?;
                revoke_key(conn, &ctx, &key, member_actor(&auth)).await
            }
            .boxed()
        })
        .await
        .map_err(on_api)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /admin/tenants/{id}/keys` → `{items: [AdminKey]}`.
pub async fn admin_list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let tenant_id = key_id(&id)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let tenant = tenants::find(&mut conn, tenant_id).await?;
    let inactive = inactive_members(&mut conn, tenant_id).await?;
    let items: Vec<AdminKeyView> = select(&mut conn, None, Some(tenant_id), None)
        .await?
        .into_iter()
        .map(|k| {
            let active = k.member_id.is_none_or(|m| !inactive.contains(&m));
            admin_view(&tenant, k, active)
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// `POST /admin/tenants/{id}/keys` with `{label, member_id?, model_ids?,
/// allowed_models?, allowed_provider_groups?, rate_limit?, expires_at?}` →
/// `201 {key: AdminKey, plaintext}`, in an active or a suspended tenant. A
/// pending tenant is `409 tenant_state`: it gives the gateways nothing until
/// it is approved, and a key would need its team and its member's user.
pub async fn admin_create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<AdminCreateKey>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let tenant_id = key_id(&id)?;
    let ctx = WriteCtx::of(&state);
    let (view, plaintext) = state
        .db
        .tx(move |conn| {
            async move {
                let tenant = tenants::lock(&mut *conn, tenant_id).await?;
                if tenant.status == TenantStatus::Pending {
                    return Err(ApiError::conflict(
                        "tenant_state",
                        "租户尚未通过审核，审核通过后才能创建密钥",
                    ));
                }
                let member_id = match req.member_id.as_deref() {
                    None => None,
                    Some(raw) => Some(bound_member(&mut *conn, tenant.id, raw).await?),
                };
                let changes = Changes {
                    label: Some(req.label),
                    model_ids: req.model_ids.map(Some),
                    allowed_models: req.allowed_models.map(Some),
                    allowed_provider_groups: req.allowed_provider_groups.map(Some),
                    rate_limit: req.rate_limit.map(Some),
                    expires_at: req.expires_at.map(Some),
                    ..Changes::default()
                };
                let (key, plaintext) = create_key(
                    &mut *conn,
                    &ctx,
                    &tenant,
                    member_id,
                    changes,
                    Actor::Operator(&auth),
                )
                .await?;
                Ok::<_, ApiError>((one_admin_view(conn, &tenant, key).await?, plaintext))
            }
            .boxed()
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "key": view, "plaintext": plaintext })),
    ))
}

/// `GET /admin/keys/{id}` → `AdminKey` and `ETag: "<version>"`; a revoked
/// key is not found.
pub async fn admin_get(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = key_id(&id)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let key = live_key(&mut conn, id, None, None).await?;
    let tenant = tenants::find(&mut conn, key.tenant_id).await?;
    let etag = format!("\"{}\"", key.version);
    let view = one_admin_view(&mut conn, &tenant, key).await?;
    Ok(([(header::ETAG, etag)], Json(view)).into_response())
}

/// The live key `id`, under its tenant's lock, and that tenant.
async fn locked_key(conn: &mut PgConnection, id: Uuid) -> Result<(KeyRow, TenantRow), ApiError> {
    let tenant_id = live_key(&mut *conn, id, None, None).await?.tenant_id;
    let tenant = tenants::lock(&mut *conn, tenant_id).await?;
    // Read again under the lock: a revoke may have committed meanwhile.
    let key = live_key(conn, id, Some(tenant_id), None).await?;
    Ok((key, tenant))
}

/// `PATCH /admin/keys/{id}` with `{label?, model_ids?, allowed_models?,
/// allowed_provider_groups?, rate_limit?, disabled?, expires_at?}` and
/// `If-Match` → `200 AdminKey`. `disabled` is the operators' switch: it
/// keeps the key off whatever the tenant does, and clearing it leaves the
/// tenant's own switch as it is.
pub async fn admin_update(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<AdminUpdateKey>,
) -> Result<Json<AdminKeyView>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = key_id(&id)?;
    let expected = if_match(&headers)?;
    let ctx = WriteCtx::of(&state);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let (key, tenant) = locked_key(&mut *conn, id).await?;
                check_version(&key, Ok(expected))?;
                let key = update_key(
                    &mut *conn,
                    &ctx,
                    &tenant,
                    key,
                    req.into(),
                    Actor::Operator(&auth),
                )
                .await?;
                one_admin_view(conn, &tenant, key).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(view))
}

/// `DELETE /admin/keys/{id}` with `If-Match` → `204`: the key is revoked.
pub async fn admin_revoke(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = key_id(&id)?;
    let expected = if_match(&headers)?;
    let ctx = WriteCtx::of(&state);
    state
        .db
        .tx(move |conn| {
            async move {
                let (key, _) = locked_key(&mut *conn, id).await?;
                check_version(&key, Ok(expected))?;
                revoke_key(conn, &ctx, &key, Actor::Operator(&auth)).await
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use aisix_core::models::{ApiKey, EndpointFamily};
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::resources::validate::{validate_lenient, validate_strict};
    use crate::resources::Kind;
    use crate::tenants::{BillingMode, TenantLimits, TenantRow, TenantStatus};

    const M1: Uuid = Uuid::from_u128(0x0000_0001_0000_4000_8000_0000_0000_0001);
    const M2: Uuid = Uuid::from_u128(0x0000_0002_0000_4000_8000_0000_0000_0002);
    const M9: Uuid = Uuid::from_u128(0x0000_0009_0000_4000_8000_0000_0000_0009);

    fn tenant(grants: &[Uuid], groups: &[&str]) -> TenantRow {
        TenantRow {
            id: Uuid::new_v4(),
            name: "acme-7c1".to_string(),
            status: TenantStatus::Active,
            is_internal: false,
            billing_mode: BillingMode::Prepaid,
            allowed_model_ids: grants.to_vec(),
            allowed_provider_groups: groups.iter().map(|g| g.to_string()).collect(),
            limits: TenantLimits::default(),
            limits_policy_id: None,
            version: 1,
            created_at: Utc::now(),
            approved_at: None,
        }
    }

    fn internal(grants: &[Uuid]) -> TenantRow {
        TenantRow {
            is_internal: true,
            allowed_provider_groups: Vec::new(),
            ..tenant(grants, &[])
        }
    }

    fn key(model_ids: Option<&[Uuid]>) -> KeyRow {
        KeyRow {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            member_id: Some(Uuid::new_v4()),
            label: "ci-7c1".to_string(),
            key_hash: ApiKey::hash_bearer("sk-aisix-unit-7c1"),
            key_prefix: "sk-aisix-uni".to_string(),
            model_ids: model_ids.map(<[Uuid]>::to_vec),
            allowed_models: None,
            allowed_provider_groups: None,
            rate_limit: KeyLimits::default(),
            disabled: false,
            operator_disabled: false,
            expires_at: None,
            version: 1,
            created_at: Utc::now(),
            created_by: KeyCreator {
                kind: "member".to_string(),
                id: None,
            },
        }
    }

    fn ids(ids: &[Uuid]) -> serde_json::Value {
        let mut ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        ids.sort();
        json!(ids)
    }

    #[test]
    fn a_customer_key_is_confined_to_the_first_phase_endpoints_and_its_grants() {
        let t = tenant(&[M1, M2], &["grp-eu-5d2"]);
        let k = key(Some(&[M2, M9]));
        let d = build_key_doc(&t, &k, true);
        assert_eq!(d["allowed_endpoints"], json!(FIRST_PHASE_ENDPOINTS));
        assert_eq!(d["allowed_model_ids"], json!([M2.to_string()]));
        assert_eq!(d["allowed_provider_groups"], json!(["grp-eu-5d2"]));
        assert!(
            d.get("allowed_models").is_none()
                && d.get("allowed_routes").is_none()
                && d.get("mcp_access").is_none()
                && d.get("allowed_agents").is_none(),
            "{d}"
        );
        assert_eq!(d["key_hash"], json!(k.key_hash));
        assert_eq!(d["display_name"], "ci-7c1");
        assert_eq!(d["team_id"], json!(t.id.to_string()));
        assert_eq!(d["user_id"], json!(k.member_id.map(|m| m.to_string())));
        assert_eq!(d["disabled"], false);
        // Without a selection, the key follows every grant.
        let d = build_key_doc(&t, &key(None), true);
        assert_eq!(d["allowed_model_ids"], ids(&[M2, M1]));
        // A key bound to nobody names no user; an expiry is carried.
        let at = Utc
            .with_ymd_and_hms(2031, 1, 2, 3, 4, 5)
            .single()
            .expect("a time");
        let unbound = KeyRow {
            member_id: None,
            expires_at: Some(at),
            ..key(None)
        };
        let d = build_key_doc(&t, &unbound, true);
        assert!(d.get("user_id").is_none(), "{d}");
        assert_eq!(d["expires_at"], json!(at));
    }

    #[test]
    fn the_first_phase_endpoints_are_families_the_gateway_knows() {
        let families: Vec<EndpointFamily> = FIRST_PHASE_ENDPOINTS
            .iter()
            .map(|f| serde_json::from_value(json!(f)).expect("a family"))
            .collect();
        assert_eq!(
            families,
            [
                EndpointFamily::ChatCompletions,
                EndpointFamily::Messages,
                EndpointFamily::CountTokens,
                EndpointFamily::Responses,
                EndpointFamily::Embeddings,
                EndpointFamily::Models,
            ]
        );
    }

    #[test]
    fn a_key_with_no_granted_model_still_writes_an_empty_id_list() {
        for (t, k) in [
            (tenant(&[], &["shared"]), key(None)),
            (tenant(&[M1], &["shared"]), key(Some(&[M9]))),
            (tenant(&[M1], &["shared"]), key(Some(&[]))),
        ] {
            let d = build_key_doc(&t, &k, true);
            assert_eq!(d.get("allowed_model_ids"), Some(&json!([])), "{d}");
            assert!(d.get("allowed_models").is_none(), "{d}");
        }
    }

    #[test]
    fn a_suspended_tenant_or_inactive_member_disables_the_key() {
        let active = tenant(&[M1], &["shared"]);
        assert_eq!(build_key_doc(&active, &key(None), true)["disabled"], false);
        for status in [TenantStatus::Suspended, TenantStatus::Pending] {
            let t = TenantRow {
                status,
                ..active.clone()
            };
            assert_eq!(
                build_key_doc(&t, &key(None), true)["disabled"],
                true,
                "{status:?}"
            );
        }
        assert_eq!(build_key_doc(&active, &key(None), false)["disabled"], true);
        let off = KeyRow {
            disabled: true,
            ..key(None)
        };
        assert_eq!(build_key_doc(&active, &off, true)["disabled"], true);
        // An operator's switch closes it whatever the tenant's is.
        let by_platform = KeyRow {
            operator_disabled: true,
            ..key(None)
        };
        assert_eq!(build_key_doc(&active, &by_platform, true)["disabled"], true);
    }

    #[test]
    fn the_tenant_cap_bounds_the_keys_concurrency() {
        let capped = |concurrency, rpm| TenantRow {
            limits: TenantLimits {
                concurrency: Some(100),
                rpm: Some(6000),
                key_concurrency_max: concurrency,
                key_rpm_max: rpm,
                ..TenantLimits::default()
            },
            ..tenant(&[M1], &["shared"])
        };
        let asking = |concurrency, rpm| KeyRow {
            rate_limit: KeyLimits { concurrency, rpm },
            ..key(None)
        };
        for (t, k, want) in [
            (
                capped(Some(5), None),
                asking(Some(8), None),
                json!({"concurrency": 5}),
            ),
            (
                capped(Some(5), None),
                asking(Some(3), None),
                json!({"concurrency": 3}),
            ),
            (
                capped(Some(5), None),
                asking(None, None),
                json!({"concurrency": 5}),
            ),
            (
                capped(None, None),
                asking(Some(8), None),
                json!({"concurrency": 8}),
            ),
            (
                capped(Some(5), Some(60)),
                asking(Some(8), Some(120)),
                json!({"concurrency": 5, "rpm": 60}),
            ),
            (
                capped(None, Some(60)),
                asking(None, Some(30)),
                json!({"rpm": 30}),
            ),
        ] {
            assert_eq!(build_key_doc(&t, &k, true)["rate_limit"], want, "{k:?}");
        }
        // The tenant-wide limits are the tenant's policy, not the key's.
        let d = build_key_doc(&capped(None, None), &asking(None, None), true);
        assert!(d.get("rate_limit").is_none(), "{d}");
    }

    #[test]
    fn an_internal_key_may_carry_patterns_and_every_endpoint() {
        let patterned = KeyRow {
            member_id: None,
            allowed_models: Some(vec!["openai/*".to_string()]),
            ..key(Some(&[M1]))
        };
        let d = build_key_doc(&internal(&[M1]), &patterned, true);
        assert_eq!(d["allowed_models"], json!(["openai/*"]));
        for absent in [
            "allowed_model_ids",
            "allowed_endpoints",
            "allowed_provider_groups",
        ] {
            assert!(d.get(absent).is_none(), "{absent} in {d}");
        }
        assert_eq!(d["disabled"], false);
        // Without patterns an internal key names models by id, as a
        // customer's does.
        let d = build_key_doc(&internal(&[M1, M2]), &key(Some(&[M2, M9])), true);
        assert_eq!(d["allowed_model_ids"], json!([M2.to_string()]));
        assert!(d.get("allowed_endpoints").is_none(), "{d}");
        // A customer key never takes patterns, whatever its row holds.
        let d = build_key_doc(&tenant(&[M1], &["shared"]), &patterned, true);
        assert!(d.get("allowed_models").is_none(), "{d}");
        assert_eq!(d["allowed_model_ids"], json!([M1.to_string()]));
    }

    #[test]
    fn a_key_document_never_carries_an_empty_group_list() {
        let t = tenant(&[M1], &["grp-eu-5d2"]);
        let overridden = |groups: &[&str]| KeyRow {
            allowed_provider_groups: Some(groups.iter().map(|g| g.to_string()).collect()),
            ..key(None)
        };
        let d = build_key_doc(&t, &overridden(&["grp-dedicated-9f3"]), true);
        assert_eq!(d["allowed_provider_groups"], json!(["grp-dedicated-9f3"]));
        // An empty override is no override: the tenant's groups hold.
        let d = build_key_doc(&t, &overridden(&[]), true);
        assert_eq!(d["allowed_provider_groups"], json!(["grp-eu-5d2"]));
        // The platform's own keys may reach every upstream: the field is
        // left out, never written empty.
        let d = build_key_doc(&internal(&[]), &overridden(&[]), true);
        assert!(d.get("allowed_provider_groups").is_none(), "{d}");
        // A customer with no group cannot be saved; if one were found, its
        // keys stay closed rather than reach every upstream.
        let d = build_key_doc(&tenant(&[M1], &[]), &key(None), true);
        assert!(d.get("allowed_provider_groups").is_none(), "{d}");
        assert_eq!(d["disabled"], true);
    }

    #[test]
    fn every_built_key_document_loads_on_the_lenient_schema() {
        let customer = TenantRow {
            limits: TenantLimits {
                key_concurrency_max: Some(5),
                key_rpm_max: Some(60),
                ..TenantLimits::default()
            },
            ..tenant(&[M1, M2], &["grp-eu-5d2", "shared"])
        };
        let suspended = TenantRow {
            status: TenantStatus::Suspended,
            ..customer.clone()
        };
        let at = Utc
            .with_ymd_and_hms(2031, 1, 2, 3, 4, 5)
            .single()
            .expect("a time");
        let full = KeyRow {
            model_ids: Some(vec![M2]),
            allowed_provider_groups: Some(vec!["grp-dedicated-9f3".to_string()]),
            rate_limit: KeyLimits {
                concurrency: Some(3),
                rpm: Some(30),
            },
            expires_at: Some(at),
            disabled: true,
            ..key(None)
        };
        let patterned = KeyRow {
            member_id: None,
            allowed_models: Some(vec!["*".to_string()]),
            ..key(None)
        };
        let docs = [
            build_key_doc(&customer, &key(None), true),
            build_key_doc(&customer, &full, true),
            build_key_doc(&suspended, &key(Some(&[])), false),
            build_key_doc(&tenant(&[], &["shared"]), &key(None), true),
            build_key_doc(&internal(&[]), &patterned, true),
            build_key_doc(&internal(&[M1]), &key(None), true),
        ];
        for doc in docs {
            validate_strict(Kind::ApiKeys, &doc).unwrap_or_else(|e| panic!("strict: {e:?} {doc}"));
            validate_lenient(Kind::ApiKeys, &doc).unwrap_or_else(|e| panic!("lenient: {e} {doc}"));
            let loaded: ApiKey = serde_json::from_value(doc.clone()).expect("the gateway's type");
            assert!(
                loaded
                    .allowed_endpoints
                    .iter()
                    .flatten()
                    .all(|f| *f != EndpointFamily::Unknown),
                "{doc}"
            );
        }
    }

    #[test]
    fn a_minted_key_is_prefixed_hex_and_hashed_the_gateways_way() {
        let (plaintext, hash) = mint().expect("mint");
        let hex = plaintext.strip_prefix("sk-aisix-").expect("the prefix");
        assert_eq!(hex.len(), 64);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{plaintext}");
        assert_eq!(hash, ApiKey::hash_bearer(&plaintext));
        assert_eq!(prefix_of(&plaintext), &plaintext[..12]);
        let (again, _) = mint().expect("mint");
        assert_ne!(plaintext, again);
    }
}
