//! `gw_resources`: the stored gateway documents.
//!
//! Every write bumps `version`, takes the next `seq` and notifies
//! `gw_resources_dirty`, which is what the projector follows. Only
//! [`super::write::write_resource`] calls the writes here: it holds the
//! resources lock, so `seq` order is commit order.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::Kind;
use crate::db::DbError;
use crate::error::ApiError;

/// The unique index on a kind's live identities.
const IDENTITY_INDEX: &str = "gw_resources_identity";

/// One stored document, live or a tombstone.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRow {
    pub kind: Kind,
    pub id: Uuid,
    /// With its credentials sealed.
    pub doc: Value,
    pub version: i64,
    /// Who derives the row (such as `tenant:<id>`); `None` for a row
    /// operators write directly.
    pub managed_by: Option<String>,
    pub deleted: bool,
    /// The version the projector last wrote to kine; the row is due for
    /// projection while this differs from `version`.
    pub projected_version: Option<i64>,
    /// The kine revision of the last projection, once projected.
    pub kine_revision: Option<i64>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no live row")]
    NotFound,
    #[error("the row is at version {current}")]
    VersionMismatch { current: i64 },
    /// Another live row of the kind has the same identity, or, on an
    /// insert, a live row has the same id.
    #[error("the identity is taken")]
    IdentityTaken,
    /// An insert would revive the tombstone of a row another owner derived
    /// (or an operator wrote).
    #[error("the row belongs to another owner")]
    NotOwner,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> StoreError {
        match DbError::from(e) {
            DbError::UniqueViolation { constraint } if constraint == IDENTITY_INDEX => {
                StoreError::IdentityTaken
            }
            e => StoreError::Db(e),
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> ApiError {
        match e {
            StoreError::NotFound => ApiError::not_found(),
            StoreError::VersionMismatch { current } => ApiError::version_mismatch(current),
            StoreError::IdentityTaken => {
                ApiError::conflict("name_taken", "同类资源中已有相同名称或标识的资源")
            }
            StoreError::NotOwner => super::managed_resource(),
            StoreError::Db(e) => e.into(),
        }
    }
}

type RawRow = (
    Value,
    i64,
    Option<String>,
    bool,
    Option<i64>,
    Option<i64>,
    DateTime<Utc>,
);

const COLUMNS: &str =
    "doc, version, managed_by, deleted, projected_version, kine_revision, updated_at";

fn row(kind: Kind, id: Uuid, raw: RawRow) -> ResourceRow {
    let (doc, version, managed_by, deleted, projected_version, kine_revision, updated_at) = raw;
    ResourceRow {
        kind,
        id,
        doc,
        version,
        managed_by,
        deleted,
        projected_version,
        kine_revision,
        updated_at,
    }
}

fn identity(kind: Kind, doc: &Value) -> Option<String> {
    kind.identity_field()
        .and_then(|field| doc.get(field))
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn notify(conn: &mut PgConnection) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_notify('gw_resources_dirty', '')")
        .execute(conn)
        .await?;
    Ok(())
}

/// Inserts a new row, or revives the tombstone of `id`: live again, with
/// the new document, at the next version. Only the tombstone's own owner
/// revives it (`NotOwner` otherwise), as only a live row's owner changes it.
pub async fn insert(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
    doc: &Value,
    managed_by: Option<&str>,
) -> Result<ResourceRow, StoreError> {
    let raw: Option<RawRow> = sqlx::query_as(&format!(
        "INSERT INTO gw_resources (kind, id, doc, identity, managed_by) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (kind, id) DO UPDATE SET \
           doc = EXCLUDED.doc, identity = EXCLUDED.identity, \
           managed_by = EXCLUDED.managed_by, deleted = false, \
           version = gw_resources.version + 1, seq = nextval('gw_resources_seq'), \
           updated_at = now() \
         WHERE gw_resources.deleted \
           AND gw_resources.managed_by IS NOT DISTINCT FROM EXCLUDED.managed_by \
         RETURNING {COLUMNS}"
    ))
    .bind(kind.segment())
    .bind(id)
    .bind(doc)
    .bind(identity(kind, doc))
    .bind(managed_by)
    .fetch_optional(&mut *conn)
    .await?;
    // No row back: `id` is live, and is not created twice, or it is
    // another owner's tombstone.
    let Some(raw) = raw else {
        let tombstone: Option<bool> =
            sqlx::query_scalar("SELECT deleted FROM gw_resources WHERE kind = $1 AND id = $2")
                .bind(kind.segment())
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
        return Err(if tombstone == Some(true) {
            StoreError::NotOwner
        } else {
            StoreError::IdentityTaken
        });
    };
    notify(conn).await?;
    Ok(row(kind, id, raw))
}

/// Why a conditional write of `kind`/`id` matched no row: there is no live
/// row, or it is at another version.
async fn refusal(conn: &mut PgConnection, kind: Kind, id: Uuid) -> StoreError {
    let found: Result<Option<(i64, bool)>, sqlx::Error> =
        sqlx::query_as("SELECT version, deleted FROM gw_resources WHERE kind = $1 AND id = $2")
            .bind(kind.segment())
            .bind(id)
            .fetch_optional(conn)
            .await;
    match found {
        Ok(Some((current, false))) => StoreError::VersionMismatch { current },
        Ok(_) => StoreError::NotFound,
        Err(e) => e.into(),
    }
}

/// Replaces the live row's document, when it is at `expected_version` (any
/// version when `None`). The owner is kept.
pub async fn update(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
    expected_version: Option<i64>,
    doc: &Value,
) -> Result<ResourceRow, StoreError> {
    let raw: Option<RawRow> = sqlx::query_as(&format!(
        "UPDATE gw_resources SET doc = $3, identity = $4, version = version + 1, \
           seq = nextval('gw_resources_seq'), updated_at = now() \
         WHERE kind = $1 AND id = $2 AND NOT deleted \
           AND ($5::bigint IS NULL OR version = $5) \
         RETURNING {COLUMNS}"
    ))
    .bind(kind.segment())
    .bind(id)
    .bind(doc)
    .bind(identity(kind, doc))
    .bind(expected_version)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(raw) = raw else {
        return Err(refusal(conn, kind, id).await);
    };
    notify(conn).await?;
    Ok(row(kind, id, raw))
}

/// Turns the live row into a tombstone, when it is at `expected_version`
/// (any version when `None`). The document stays, for the projector.
pub async fn mark_deleted(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
    expected_version: Option<i64>,
) -> Result<ResourceRow, StoreError> {
    let raw: Option<RawRow> = sqlx::query_as(&format!(
        "UPDATE gw_resources SET deleted = true, version = version + 1, \
           seq = nextval('gw_resources_seq'), updated_at = now() \
         WHERE kind = $1 AND id = $2 AND NOT deleted \
           AND ($3::bigint IS NULL OR version = $3) \
         RETURNING {COLUMNS}"
    ))
    .bind(kind.segment())
    .bind(id)
    .bind(expected_version)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(raw) = raw else {
        return Err(refusal(conn, kind, id).await);
    };
    notify(conn).await?;
    Ok(row(kind, id, raw))
}

/// The row of `kind`/`id`, a tombstone included.
pub async fn get(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
) -> Result<Option<ResourceRow>, DbError> {
    let raw: Option<RawRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM gw_resources WHERE kind = $1 AND id = $2"
    ))
    .bind(kind.segment())
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(raw.map(|raw| row(kind, id, raw)))
}

/// Every row, live or a tombstone, of any kind, derived by one of `owners`
/// (`managed_by`), in write order.
pub async fn owned_by(
    conn: &mut PgConnection,
    owners: &[String],
) -> Result<Vec<ResourceRow>, DbError> {
    type OwnedRow = (
        String,
        Uuid,
        Value,
        i64,
        Option<String>,
        bool,
        Option<i64>,
        Option<i64>,
        DateTime<Utc>,
    );
    let raw: Vec<OwnedRow> = sqlx::query_as(&format!(
        "SELECT kind, id, {COLUMNS} FROM gw_resources WHERE managed_by = ANY($1) ORDER BY seq"
    ))
    .bind(owners)
    .fetch_all(conn)
    .await?;
    Ok(raw
        .into_iter()
        .filter_map(
            |(
                kind,
                id,
                doc,
                version,
                managed_by,
                deleted,
                projected_version,
                kine_revision,
                updated_at,
            )| {
                let kind = Kind::from_segment(&kind)?;
                Some(row(
                    kind,
                    id,
                    (
                        doc,
                        version,
                        managed_by,
                        deleted,
                        projected_version,
                        kine_revision,
                        updated_at,
                    ),
                ))
            },
        )
        .collect())
}

/// Every live row of `kind`, by identity and then id.
pub async fn list(conn: &mut PgConnection, kind: Kind) -> Result<Vec<ResourceRow>, DbError> {
    type ListedRow = (
        Uuid,
        Value,
        i64,
        Option<String>,
        bool,
        Option<i64>,
        Option<i64>,
        DateTime<Utc>,
    );
    let raw: Vec<ListedRow> = sqlx::query_as(&format!(
        "SELECT id, {COLUMNS} FROM gw_resources WHERE kind = $1 AND NOT deleted \
             ORDER BY identity, id"
    ))
    .bind(kind.segment())
    .fetch_all(conn)
    .await?;
    Ok(raw
        .into_iter()
        .map(
            |(
                id,
                doc,
                version,
                managed_by,
                deleted,
                projected_version,
                kine_revision,
                updated_at,
            )| {
                row(
                    kind,
                    id,
                    (
                        doc,
                        version,
                        managed_by,
                        deleted,
                        projected_version,
                        kine_revision,
                        updated_at,
                    ),
                )
            },
        )
        .collect())
}
