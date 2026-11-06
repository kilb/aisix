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
use crate::crypto::MasterKey;
use crate::db::DbError;
use crate::error::{ApiError, Detail};
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
}

impl WriteCtx {
    pub fn of(state: &AppState) -> WriteCtx {
        WriteCtx {
            mk: MasterKey(state.cfg.master_key.0),
            env: state.env.clone(),
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
/// gateway's strict validation; the reference check; sealing; the store. A
/// delete is refused while a live document references the row (`409
/// in_use`).
pub async fn write_resource(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    w: ResourceWrite<'_>,
) -> Result<ResourceRow, ApiError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
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
            // Task 10 cascades a model's tenant grants and key selections
            // first, and then excludes those kinds here (ruling 13); until
            // then every referrer counts.
            let referrers = refs::incoming(conn, w.kind, w.id, &[]).await?;
            if !referrers.is_empty() {
                return Err(ApiError::in_use(referrers));
            }
            Ok(store::mark_deleted(conn, w.kind, w.id, w.expected_version).await?)
        }
    }
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
    refuse_self_reference(w, &doc)?;
    refs::check_outgoing(conn, w.kind, &doc).await?;
    // Task 9: the save-time gateway-version gate runs here.
    secrets::seal(&ctx.mk, w.kind, w.id, &mut doc);
    Ok(doc)
}
