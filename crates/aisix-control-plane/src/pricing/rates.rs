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
//!
//! # One reader for both rate tables
//!
//! An account's cost rates ([`super::cost_rates`]) are the same table shape
//! keyed on the account, and are read by the same code: [`RateTable`] names
//! the table and its owner column, and [`plan`], [`superseded_later`] and
//! [`priceable_model`] serve both.

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
        Ok(GroupRateRevision {
            revision_id: row.try_get("revision_id")?,
            engine_version: row.try_get("engine_version")?,
            group_id: row.try_get("group_id")?,
            model_id: row.try_get("model_id")?,
            effective_from: row.try_get("effective_from")?,
            cny_per_usd: rate_text(row.try_get("cny_per_usd_e4")?),
            note: row.try_get("note")?,
            created_by: created_by
                .zip(email)
                .map(|(id, email)| OperatorRef { id, email }),
            created_at: row.try_get("created_at")?,
        })
    }
}

/// A stored `cny_per_usd_e4` as the decimal string it was typed as; `None`
/// on a model row that ends an override.
pub(crate) fn rate_text(e4: Option<i64>) -> Option<String> {
    e4.map(|e4| {
        // The column's CHECK bounds it to `CnyPerUsd::MAX`, so a value
        // outside that is corruption, not a rate that ended.
        let e4 = u32::try_from(e4)
            .ok()
            .filter(|e4| *e4 <= CnyPerUsd::MAX.0)
            .expect("cny_per_usd_e4 outside its CHECK");
        CnyPerUsd(e4).to_string()
    })
}

/// A table of rate revisions in `pricing_group_rates`' shape (0023): the
/// group rates, or the accounts' cost rates, keyed on `owner`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RateTable {
    pub(crate) table: &'static str,
    pub(crate) owner: &'static str,
}

/// The group rates.
pub(crate) const GROUP_RATES: RateTable = RateTable {
    table: "pricing_group_rates",
    owner: "group_id",
};

impl RateTable {
    /// The columns a revision is read from, over `<table> r` joined to
    /// `operators o` on `created_by`.
    pub(crate) fn columns(self) -> String {
        format!(
            "r.revision_id, r.engine_version, r.{}, r.model_id, r.effective_from, \
             r.cny_per_usd_e4, r.note, r.created_by, o.email AS created_by_email, r.created_at",
            self.owner
        )
    }
}

/// When a rate applies: the revision in force now, and the ones already
/// scheduled.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RatePlan<R = GroupRateRevision> {
    pub current: Option<R>,
    /// By ascending `effective_from`, one per instant: the greatest
    /// `revision_id` of those starting together, which is the one that will
    /// apply.
    pub upcoming: Vec<R>,
}

/// The rate plan of `group_id` for `model_id` (`None`: the group default),
/// read as the price book reads it: the latest revision starting at or
/// before now, ties going to the greatest `revision_id`.
pub async fn rate_plan(
    conn: &mut PgConnection,
    group_id: Uuid,
    model_id: Option<Uuid>,
) -> Result<RatePlan, DbError> {
    plan(conn, GROUP_RATES, group_id, model_id).await
}

/// The plan in `table` of `owner_id` for `model_id` (`None`: the owner's
/// default), read as the price book reads it: the latest revision starting
/// at or before now, ties going to the greatest `revision_id`.
pub(crate) async fn plan<R>(
    conn: &mut PgConnection,
    table: RateTable,
    owner_id: Uuid,
    model_id: Option<Uuid>,
) -> Result<RatePlan<R>, DbError>
where
    R: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    let (columns, name, owner) = (table.columns(), table.table, table.owner);
    // One instant for both halves of the plan: a revision landing exactly now
    // is `current`, never both and never neither.
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *conn)
        .await?;
    let current: Option<R> = sqlx::query_as(&format!(
        "SELECT {columns} FROM {name} r LEFT JOIN operators o ON o.id = r.created_by \
         WHERE r.{owner} = $1 AND r.model_id IS NOT DISTINCT FROM $2 AND r.effective_from <= $3 \
         ORDER BY r.effective_from DESC, r.revision_id DESC LIMIT 1"
    ))
    .bind(owner_id)
    .bind(model_id)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    let upcoming: Vec<R> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (r.effective_from) {columns} \
         FROM {name} r LEFT JOIN operators o ON o.id = r.created_by \
         WHERE r.{owner} = $1 AND r.model_id IS NOT DISTINCT FROM $2 AND r.effective_from > $3 \
         ORDER BY r.effective_from, r.revision_id DESC"
    ))
    .bind(owner_id)
    .bind(model_id)
    .bind(now)
    .fetch_all(&mut *conn)
    .await?;
    Ok(RatePlan { current, upcoming })
}

/// Refuses an override for `model_id` with `422 /model_id`: a model that is
/// not live, and a channel's generated direct document, which is priced
/// through its public model. A charge reads the rate of the model the
/// caller addressed, which for a channel's generated direct document is its
/// public model, so an override written on the direct document would never
/// apply. `what` names the rate in the refusal.
pub(crate) async fn priceable_model(
    conn: &mut PgConnection,
    model_id: Uuid,
    what: &str,
) -> Result<(), ApiError> {
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
    if let Some(public) = crate::channels::generated_direct_of(&mut *conn, model_id).await? {
        return Err(refused(
            "/model_id",
            format!("该模型由公共模型 {public} 生成，请为公共模型 {public} 设置{what}"),
        ));
    }
    Ok(())
}

/// A `superseded_later` warning for each revision in `table` of the same
/// owner and model that starts after `effective_from`: it takes over from
/// the one just written then.
pub(crate) async fn superseded_later(
    conn: &mut PgConnection,
    table: RateTable,
    owner_id: Uuid,
    model_id: Option<Uuid>,
    effective_from: DateTime<Utc>,
) -> Result<Vec<PriceWarning>, DbError> {
    let later: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(&format!(
        "SELECT revision_id, effective_from FROM {} \
         WHERE {} = $1 AND model_id IS NOT DISTINCT FROM $2 AND effective_from > $3 \
         ORDER BY effective_from, revision_id",
        table.table, table.owner
    ))
    .bind(owner_id)
    .bind(model_id)
    .bind(effective_from)
    .fetch_all(&mut *conn)
    .await?;
    Ok(later
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
        .collect())
}

/// The rate in force for `model_id` in `group_id`: the model's own override
/// when one is in effect, else the group's default. `(cny, revision_id,
/// model_override)`; `None` when no rate is in force, or the revision in
/// force cannot be read by this engine (an override that ended falls through
/// to the default, as the price book reads it).
pub async fn rate_in_force(
    conn: &mut PgConnection,
    group_id: Uuid,
    model_id: Option<Uuid>,
) -> Result<Option<(CnyPerUsd, i64, bool)>, DbError> {
    let usable = |revision: &GroupRateRevision| -> Option<(CnyPerUsd, i64, bool)> {
        if u32::try_from(revision.engine_version).unwrap_or(u32::MAX) > ENGINE_VERSION {
            return None;
        }
        let cny = CnyPerUsd::parse(revision.cny_per_usd.as_deref()?).ok()?;
        Some((cny, revision.revision_id, revision.model_id.is_some()))
    };
    let Some(model_id) = model_id else {
        let plan = rate_plan(&mut *conn, group_id, None).await?;
        return Ok(plan.current.as_ref().and_then(usable));
    };
    let model = rate_plan(&mut *conn, group_id, Some(model_id)).await?;
    if let Some(revision) = model.current.as_ref() {
        if u32::try_from(revision.engine_version).unwrap_or(u32::MAX) > ENGINE_VERSION {
            // An override written by a newer engine prices nothing; it does
            // not give way to the default.
            return Ok(None);
        }
        if revision.cny_per_usd.is_some() {
            return Ok(usable(revision));
        }
        // An override that ended gives way to the group's default.
    }
    let default = rate_plan(&mut *conn, group_id, None).await?;
    Ok(default.current.as_ref().and_then(usable))
}

/// Writes a rate revision starting at `row.effective_from`, refusing with:
/// `409 group_retired` for a retired group;
/// `422 /effective_from` for one that starts in the past;
/// `422 /model_id` for a model that is not live, and for a channel's
/// generated direct document, which is priced through its public model;
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
        priceable_model(&mut *conn, model_id, "换算率").await?;
    } else if row.cny_per_usd.is_none() {
        return Err(refused("/cny_per_usd", "默认换算率不能为空"));
    }
    let revision: GroupRateRevision = sqlx::query_as(&format!(
        "WITH r AS ( \
           INSERT INTO pricing_group_rates \
           (engine_version, group_id, model_id, effective_from, cny_per_usd_e4, note, created_by) \
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *) \
         SELECT {} FROM r LEFT JOIN operators o ON o.id = r.created_by",
        GROUP_RATES.columns()
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
    let warnings = superseded_later(
        &mut *conn,
        GROUP_RATES,
        row.group_id,
        row.model_id,
        row.effective_from,
    )
    .await?;
    Ok((revision, warnings))
}
