//! Group rates: the CNY-per-USD rate each provider group charges, per model,
//! with the group's model-less row as its default.
//!
//! A rate is append-only like every price: a trigger refuses `UPDATE`,
//! `DELETE` and `TRUNCATE` (migration `0023`), a revision starts now or
//! later (`store::starts_now_or_later`), and each write bumps
//! `pricing_version` and writes its audit row in its caller's transaction.
//!
//! A model row with no rate ends that model's override from its start on, so
//! the group default applies again. Only the default row must carry a rate:
//! a group that never had one leaves every row of its keys `missing_rate`.
//!
//! # What a rate keys on
//!
//! A rate keys on the provider group (`provider_groups.id`) and, for an
//! override, on the requested model, the entry the caller addresses. The
//! dispatched target plays no part: one model's rate applies whichever
//! upstream serves it.

use aisix_pricing::{CnyPerUsd, ENGINE_VERSION};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::api::PriceWarning;
use super::baselines::OperatorRef;
use super::store::{bump_pricing_version, created_by, refused, starts_now_or_later};
use crate::audit::{self, Actor, AuditEntry};
use crate::db::DbError;
use crate::error::ApiError;
use crate::groups::{self, LockMode};
use crate::resources::Kind;

/// The most characters a rate's note holds.
const MAX_NOTE_CHARS: usize = 500;

/// A rate revision to write.
#[derive(Debug, Clone)]
pub struct NewGroupRate {
    pub group_id: Uuid,
    /// `None` is the group default.
    pub model_id: Option<Uuid>,
    pub effective_from: DateTime<Utc>,
    /// `None` only on a model row, where it ends that model's override.
    pub cny_per_usd: Option<CnyPerUsd>,
    pub note: String,
}

/// One `pricing_group_rates` row, with the email of the operator who wrote
/// it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupRateRevision {
    pub revision_id: i64,
    pub engine_version: i64,
    pub group_id: Uuid,
    pub model_id: Option<Uuid>,
    pub effective_from: DateTime<Utc>,
    /// As stored, as the decimal string it was typed as; `null` on a model
    /// row that ends an override.
    pub cny_per_usd: Option<String>,
    pub note: String,
    /// `None` when the control plane itself wrote it.
    pub created_by: Option<OperatorRef>,
    pub created_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, PgRow> for GroupRateRevision {
    fn from_row(row: &'r PgRow) -> Result<GroupRateRevision, sqlx::Error> {
        let created_by: Option<Uuid> = row.try_get("created_by")?;
        let email: Option<String> = row.try_get("created_by_email")?;
        let e4: Option<i64> = row.try_get("cny_per_usd_e4")?;
        Ok(GroupRateRevision {
            revision_id: row.try_get("revision_id")?,
            engine_version: row.try_get("engine_version")?,
            group_id: row.try_get("group_id")?,
            model_id: row.try_get("model_id")?,
            effective_from: row.try_get("effective_from")?,
            cny_per_usd: e4.map(|e4| {
                // The column's CHECK bounds it to `CnyPerUsd::MAX`, so a
                // value outside that is corruption, not a rate that ended.
                let e4 = u32::try_from(e4)
                    .ok()
                    .filter(|e4| *e4 <= CnyPerUsd::MAX.0)
                    .expect("pricing_group_rates.cny_per_usd_e4 outside its CHECK");
                CnyPerUsd(e4).to_string()
            }),
            note: row.try_get("note")?,
            created_by: created_by
                .zip(email)
                .map(|(id, email)| OperatorRef { id, email }),
            created_at: row.try_get("created_at")?,
        })
    }
}

/// The columns a [`GroupRateRevision`] is read from, over
/// `pricing_group_rates r` joined to `operators o` on `created_by`.
const COLUMNS: &str = "r.revision_id, r.engine_version, r.group_id, r.model_id, r.effective_from, \
                       r.cny_per_usd_e4, r.note, r.created_by, o.email AS created_by_email, \
                       r.created_at";

/// When a rate applies: the revision in force now, and the ones already
/// scheduled.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RatePlan {
    pub current: Option<GroupRateRevision>,
    /// By ascending `effective_from`, one per instant: the greatest
    /// `revision_id` of those starting together, which is the one that will
    /// apply.
    pub upcoming: Vec<GroupRateRevision>,
}

/// The rate plan of `group_id` for `model_id` (`None`: the group default),
/// read as the price book reads it: the latest revision starting at or
/// before now, ties going to the greatest `revision_id`.
pub async fn rate_plan(
    conn: &mut PgConnection,
    group_id: Uuid,
    model_id: Option<Uuid>,
) -> Result<RatePlan, DbError> {
    // One instant for both halves of the plan: a revision landing exactly now
    // is `current`, never both and never neither.
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *conn)
        .await?;
    let current: Option<GroupRateRevision> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM pricing_group_rates r LEFT JOIN operators o ON o.id = r.created_by \
         WHERE r.group_id = $1 AND r.model_id IS NOT DISTINCT FROM $2 AND r.effective_from <= $3 \
         ORDER BY r.effective_from DESC, r.revision_id DESC LIMIT 1"
    ))
    .bind(group_id)
    .bind(model_id)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    let upcoming: Vec<GroupRateRevision> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (r.effective_from) {COLUMNS} \
         FROM pricing_group_rates r LEFT JOIN operators o ON o.id = r.created_by \
         WHERE r.group_id = $1 AND r.model_id IS NOT DISTINCT FROM $2 AND r.effective_from > $3 \
         ORDER BY r.effective_from, r.revision_id DESC"
    ))
    .bind(group_id)
    .bind(model_id)
    .bind(now)
    .fetch_all(&mut *conn)
    .await?;
    Ok(RatePlan { current, upcoming })
}

/// Writes a rate revision starting at `row.effective_from`, refusing with:
/// `409 group_retired` for a retired group;
/// `422 /effective_from` for one that starts in the past;
/// `422 /model_id` for a model that is not live;
/// `422 /cny_per_usd` for a default row with no rate, and one above the cap
/// or with more than four decimals.
///
/// The group row is taken `FOR SHARE` first, so a retire answering that
/// nothing names the group cannot race this write. The revision, the
/// `pricing_version` bump and the audit row `pricing.group_rate.create` (or
/// `pricing.group_rate.end_override`) go in the caller's transaction.
///
/// Each revision of the same (group, model) already there that starts later
/// takes over from it then; each is named in a `superseded_later` warning,
/// and the write still succeeds.
pub async fn insert_group_rate(
    conn: &mut PgConnection,
    row: &NewGroupRate,
    actor: Actor<'_>,
) -> Result<(GroupRateRevision, Vec<PriceWarning>), ApiError> {
    let group = groups::lock_one(&mut *conn, row.group_id, LockMode::Share).await?;
    starts_now_or_later(&mut *conn, row.effective_from).await?;
    if group.retired_at.is_some() {
        return Err(ApiError::group_retired());
    }
    if row.note.chars().count() > MAX_NOTE_CHARS {
        return Err(refused("/note", "说明最多 500 个字符"));
    }
    if let Some(model_id) = row.model_id {
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gw_resources WHERE kind = $1 AND id = $2 AND NOT deleted)",
        )
        .bind(Kind::Models.segment())
        .bind(model_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
        if !live {
            return Err(refused("/model_id", "模型不存在"));
        }
    } else if row.cny_per_usd.is_none() {
        return Err(refused("/cny_per_usd", "默认换算率不能为空"));
    }
    let revision: GroupRateRevision = sqlx::query_as(&format!(
        "WITH r AS ( \
           INSERT INTO pricing_group_rates \
           (engine_version, group_id, model_id, effective_from, cny_per_usd_e4, note, created_by) \
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *) \
         SELECT {COLUMNS} FROM r LEFT JOIN operators o ON o.id = r.created_by"
    ))
    .bind(i64::from(ENGINE_VERSION))
    .bind(row.group_id)
    .bind(row.model_id)
    .bind(row.effective_from)
    .bind(row.cny_per_usd.map(|c| i64::from(c.0)))
    .bind(&row.note)
    .bind(created_by(actor))
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    bump_pricing_version(&mut *conn).await?;
    audit::record(
        &mut *conn,
        AuditEntry {
            actor,
            action: if row.cny_per_usd.is_some() {
                "pricing.group_rate.create"
            } else {
                "pricing.group_rate.end_override"
            },
            target_kind: "pricing_group_rate",
            target_id: Some(revision.revision_id.to_string()),
            tenant_id: None,
            detail: json!({
                "group_id": row.group_id,
                "model_id": row.model_id,
                "effective_from": row.effective_from,
                "cny_per_usd_e4": row.cny_per_usd.map(|c| c.0),
            }),
        },
    )
    .await?;
    let later: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT revision_id, effective_from FROM pricing_group_rates \
         WHERE group_id = $1 AND model_id IS NOT DISTINCT FROM $2 AND effective_from > $3 \
         ORDER BY effective_from, revision_id",
    )
    .bind(row.group_id)
    .bind(row.model_id)
    .bind(row.effective_from)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let warnings = later
        .into_iter()
        .map(|(revision_id, effective_from)| PriceWarning {
            code: "superseded_later",
            message: format!(
                "已有一条更晚生效的换算率（修订 {revision_id}），将从 {} 起取代这次写入的换算率",
                effective_from.to_rfc3339()
            ),
            revision_id: Some(revision_id),
            effective_from: Some(effective_from),
        })
        .collect();
    Ok((revision, warnings))
}
