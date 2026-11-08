//! [`write_resource`]: the one way a gateway document is created, replaced
//! or deleted. Every writer goes through it: the operators' generic API and
//! the code that derives documents from tenants, members and keys.

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::normalize::normalize;
use super::store::{self, ResourceRow};
use super::validate::validate_strict;
use super::{managed_resource, refs, secrets, Kind};
use crate::compat;
use crate::crypto::MasterKey;
use crate::error::{ApiError, Detail};
use crate::gateways::stale_window_secs;
use crate::settings::EnvSettings;
use crate::AppState;

/// Serialises every gateway-resource write. Held to the end of the writing
/// transaction, so `seq` order is commit order and a reference check cannot
/// race the delete of what it found. Lock order: tenant rows first, then
/// this, never the other way round.
pub const RESOURCES_LOCK_KEY: i64 = 0x6169_7369_7863_7032;

/// The largest document stored, serialized: kine refuses a request much
/// over 2 MiB, and one document kine never accepts stalls every write
/// projected after it.
const MAX_DOC_BYTES: usize = 1 << 20;

/// What a write needs besides the database.
pub struct WriteCtx {
    pub mk: MasterKey,
    pub env: EnvSettings,
    /// The version this control plane reports (`CpConfig::cp_version`),
    /// which the gateway-version gate judges [`compat::Since::ThisRelease`]
    /// by.
    pub cp_version: String,
    /// `CP_INSTANCE_STALE_SECS` as applied: a gateway heard from within it
    /// is online.
    pub stale_secs: f64,
}

impl WriteCtx {
    pub fn of(state: &AppState) -> WriteCtx {
        WriteCtx {
            mk: MasterKey(state.cfg.master_key.0),
            env: state.env.clone(),
            cp_version: state.cfg.cp_version.clone(),
            stale_secs: stale_window_secs(&state.cfg),
        }
    }
}

pub struct ResourceWrite<'a> {
    pub kind: Kind,
    pub id: Uuid,
    pub op: WriteOp<'a>,
    /// Replace and delete only when the row is at this version; `None`
    /// skips the check.
    pub expected_version: Option<i64>,
    /// Who derives the row. A create stores it; a replace or a delete is
    /// refused unless it names the row's own owner (`None` for a row an
    /// operator writes directly), so a derived row stays its owner's to
    /// change and an operator's row stays the operators'.
    pub managed_by: Option<&'a str>,
}

pub enum WriteOp<'a> {
    Create(&'a Value),
    Replace(&'a Value),
    Delete,
}

/// Writes one gateway document, in the caller's transaction, in this
/// order: the resources lock; the size check (`413`); normalisation
/// (aliases first, so a redacted credential under its former name is
/// restored under the canonical one); restoring redacted credentials; the
/// size check again, on the document as it will be projected; the
/// gateway's strict validation; the platform rules ([`platform_rules`]: a
/// cache policy is never `env`-scoped, and a missing scope becomes `team`);
/// the reference check; the gateway-version
/// gate ([`compat::gate`], `422 gateway_too_old`); sealing; the store. A
/// delete is refused while a live document references the row (`409
/// in_use`), except the referrers [`non_holding_referrers`] names.
pub async fn write_resource(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    w: ResourceWrite<'_>,
) -> Result<ResourceRow, ApiError> {
    super::refs::lock_resources(conn).await?;
    // A singleton lives at the environment's id and nowhere else.
    if w.kind.is_singleton() && w.id != ctx.env.env_id {
        return Err(ApiError::not_found());
    }
    match w.op {
        WriteOp::Create(doc) => {
            check_size(doc)?;
            let doc = prepare(conn, ctx, &w, doc, &Value::Null).await?;
            Ok(store::insert(conn, w.kind, w.id, &doc, w.managed_by).await?)
        }
        WriteOp::Replace(doc) => {
            check_size(doc)?;
            let current = writable_row(conn, &w).await?;
            let doc = prepare(conn, ctx, &w, doc, &current.doc).await?;
            Ok(store::update(conn, w.kind, w.id, w.expected_version, &doc).await?)
        }
        WriteOp::Delete => {
            writable_row(conn, &w).await?;
            let (except, except_unowned) = non_holding_referrers(w.kind);
            let referrers = refs::incoming(conn, w.kind, w.id, except, except_unowned).await?;
            if !referrers.is_empty() {
                return Err(ApiError::in_use(referrers));
            }
            Ok(store::mark_deleted(conn, w.kind, w.id, w.expected_version).await?)
        }
    }
}

/// The references that do not keep a row of `kind` from being deleted: of
/// the first kinds every row, of the second only the rows operators wrote
/// (no `managed_by`).
///
/// - A model: the documents derived from tenants. Its delete withdraws it
///   from every grant and key selection first (`tenants::on_model_deleted`,
///   ruling 13); configuration that uses it still holds it.
/// - A row derived from a tenant, a member or a key: the configuration
///   operators scope to one. Once the team, member or key is gone, no
///   request reaches a gateway through it, so a scope left naming it
///   governs nothing that still exists; holding the delete would let an
///   operator's guardrail or cache scope block a key revoke. A derived
///   referrer (a key naming its team or member, or a derived document of
///   one of those kinds) still holds it: the code deriving both deletes the
///   referrer first.
fn non_holding_referrers(kind: Kind) -> (&'static [Kind], &'static [Kind]) {
    match kind {
        Kind::Models => (&[Kind::ApiKeys, Kind::Teams, Kind::Users], &[]),
        Kind::Teams | Kind::Users | Kind::ApiKeys => (
            &[],
            &[
                Kind::CachePolicies,
                Kind::GuardrailAttachments,
                Kind::RateLimitPolicies,
            ],
        ),
        _ => (&[], &[]),
    }
}

/// Refuses an operator's rate-limit policy named the way a tenant's limits
/// policy is (`tenant-<id>-limits`): names are unique among live policies,
/// so one taken by an operator would keep the tenant's own from being
/// written when the tenant is approved or reactivated.
fn refuse_reserved_name(w: &ResourceWrite<'_>, doc: &Value) -> Result<(), ApiError> {
    if w.kind != Kind::RateLimitPolicies || w.managed_by.is_some() {
        return Ok(());
    }
    let reserved = doc
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| name.strip_prefix("tenant-"))
        .and_then(|rest| rest.strip_suffix("-limits"))
        .is_some_and(|id| Uuid::parse_str(id).is_ok());
    if reserved {
        return Err(ApiError::validation(vec![Detail {
            path: "/name".to_string(),
            message: "该名称留给租户的默认限流策略使用，请换一个名称".to_string(),
        }]));
    }
    Ok(())
}

fn check_size(doc: &Value) -> Result<(), ApiError> {
    let bytes = serde_json::to_vec(doc).map_err(ApiError::internal)?.len();
    if bytes > MAX_DOC_BYTES {
        return Err(ApiError::too_large());
    }
    Ok(())
}

/// The live row `w` replaces or deletes, once the write may touch it.
async fn writable_row(
    conn: &mut PgConnection,
    w: &ResourceWrite<'_>,
) -> Result<ResourceRow, ApiError> {
    let row = store::get(conn, w.kind, w.id)
        .await?
        .filter(|row| !row.deleted)
        .ok_or_else(ApiError::not_found)?;
    if row.managed_by.as_deref() != w.managed_by {
        return Err(managed_resource());
    }
    if let Some(expected) = w.expected_version {
        if expected != row.version {
            return Err(ApiError::version_mismatch(row.version));
        }
    }
    Ok(row)
}

/// Refuses a document that references its own row, such as a routing,
/// semantic or ensemble model targeting itself: a group cannot dispatch to
/// itself. References between two rows are left to the gateway, which does
/// not follow a group's targets into further groups.
fn refuse_self_reference(w: &ResourceWrite<'_>, doc: &Value) -> Result<(), ApiError> {
    let own = w.id.to_string();
    let found: Vec<Detail> = refs::outgoing(w.kind, doc)
        .into_iter()
        .filter(|r| r.kind == w.kind && r.id == own)
        .map(|r| Detail {
            path: r.path,
            message: "不能引用资源自身".to_string(),
        })
        .collect();
    if found.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation(found))
    }
}

/// The document to store for a create or a replace of a row whose stored
/// document is `stored` (`null` when there is none).
async fn prepare(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    w: &ResourceWrite<'_>,
    incoming: &Value,
    stored: &Value,
) -> Result<Value, ApiError> {
    let mut doc = incoming.clone();
    normalize(conn, w.kind, &mut doc).await?;
    secrets::restore(w.kind, &mut doc, stored).map_err(|d| ApiError::validation(vec![d]))?;
    // A kept secret is back in the document at its stored size: what the
    // projector sends is the unsealed document, and kine must take it.
    let mut projected = doc.clone();
    secrets::unseal(&ctx.mk, w.kind, w.id, &mut projected).map_err(ApiError::internal)?;
    check_size(&projected)?;
    validate_strict(w.kind, &doc).map_err(ApiError::validation)?;
    platform_rules(w.kind, &mut doc)?;
    refuse_reserved_name(w, &doc)?;
    refuse_self_reference(w, &doc)?;
    refuse_retired_groups(&refs::retired_groups(conn, w.kind, &doc).await?)?;
    refs::check_outgoing(conn, w.kind, &doc).await?;
    compat::gate(conn, ctx, w.kind, &doc, stored).await?;
    secrets::seal(&ctx.mk, w.kind, w.id, &mut doc);
    Ok(doc)
}

/// Refuses a provider-key document that lists a retired group: a retired
/// group takes no rate, leaves every picker, and must not gain upstream
/// accounts it can never be used with. The check runs under
/// `RESOURCES_LOCK_KEY`, which `groups::retire` holds across its count and
/// its update, so this is the second half of "a group is never retired while
/// a live provider key names it".
fn refuse_retired_groups(retired: &[(String, String)]) -> Result<(), ApiError> {
    if retired.is_empty() {
        return Ok(());
    }
    let details: Vec<Detail> = retired
        .iter()
        .map(|(path, name)| Detail {
            path: path.clone(),
            message: format!("分组 {name} 已停用，不能加入新的上游账号"),
        })
        .collect();
    Err(ApiError::validation(details))
}

/// Rules only a platform deployment has, run after strict validation and
/// before the reference check and the version gate. A cache policy is
/// tenant-scoped: `env` would replay one tenant's answers to another, and a
/// missing scope is written as `team` so the gateway never applies its own
/// default.
fn platform_rules(kind: Kind, doc: &mut Value) -> Result<(), ApiError> {
    if kind != Kind::CachePolicies {
        return Ok(());
    }
    match doc.get("scope").and_then(Value::as_str) {
        Some("env") => Err(ApiError::validation(vec![Detail {
            path: "/scope".into(),
            message: "平台部署不提供 env 范围".into(),
        }])),
        Some(_) => Ok(()),
        None => {
            if let Some(map) = doc.as_object_mut() {
                map.insert("scope".into(), Value::String("team".into()));
            }
            Ok(())
        }
    }
}
