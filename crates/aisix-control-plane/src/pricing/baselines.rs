//! Baselines: each model's official list price in USD, one full rate card
//! per model, which a tenant pays times its group's rate.
//!
//! A baseline records where its card came from (`source_kind` and
//! `source`):
//! - `catalogue`: the list prices built into this control plane, with the
//!   catalogue's version, the entry's key and date, the provider's page it
//!   was read from and the day it was read;
//! - `third_party`: a public price dataset an operator uploaded, written only
//!   once an operator has attested the price against the provider's page;
//! - `manual`: an operator's own entry, with a note saying where it comes
//!   from.
//!
//! Baselines are append-only like every price: a trigger refuses `UPDATE`,
//! `DELETE` and `TRUNCATE` (migration `0025`), a baseline starts now or later
//! (`store::starts_now_or_later`), and each write bumps `pricing_version`
//! and writes its audit row in its caller's transaction.
//!
//! The baseline keys on the requested model, the entry the caller addresses,
//! whichever upstream serves it.
//!
//! # Following the catalogue
//!
//! A baseline never follows the catalogue file by itself, so a control plane
//! upgraded to a newer catalogue charges what operators confirmed, and two
//! replicas on different releases price alike. `GET /admin/pricing/baselines`
//! shows instead, for a model whose baseline in force came from the catalogue,
//! a `catalogue_update` when the catalogue this control plane runs with now
//! says something else for that entry key (see [`catalogue_update`]);
//! confirming it is an ordinary write from that key.

use std::collections::HashMap;

use aisix_pricing::{Catalogue, CatalogueEntry, RateCard, ENGINE_VERSION};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::types::Json as JsonColumn;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::api::PriceWarning;
use super::store::{bump_pricing_version, created_by, starts_now_or_later};
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::resources::Kind;
use crate::AppState;

/// The most characters a manual baseline's note holds.
const MAX_NOTE_CHARS: usize = 500;

/// A `422` at `path`.
fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

/// UTC midnight of a catalogue date: when an entry dated that day starts.
fn day_start(day: NaiveDate) -> DateTime<Utc> {
    day.and_time(NaiveTime::MIN).and_utc()
}

/// An operator as a revision names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperatorRef {
    pub id: Uuid,
    pub email: String,
}

/// Where a baseline's card came from, stored as `source_kind` (the variant)
/// and `source` (its fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source_kind", content = "source", rename_all = "snake_case")]
pub enum BaselineSource {
    /// An entry of the built-in catalogue.
    Catalogue {
        catalogue_version: String,
        /// The entry's key, never an alias it was named by.
        entry_key: String,
        /// The entry's own date; `None` for an undated entry.
        entry_effective_from: Option<NaiveDate>,
        source_url: String,
        as_of: NaiveDate,
    },
    /// A confirmed proposal of an uploaded dataset.
    ThirdParty {
        dataset: String,
        /// The dataset's revision, as the operator gave it.
        dataset_ref: String,
        /// The provider's pricing page the dataset cites.
        pricing_url: Option<String>,
        /// When the dataset says the price was last checked.
        prices_checked: Option<NaiveDate>,
        import_id: Uuid,
        proposal_id: Uuid,
        /// The operator who attested the price against the provider's page.
        attested_by: Uuid,
        attested_at: DateTime<Utc>,
    },
    /// An operator's own card.
    Manual { note: String },
}

impl BaselineSource {
    /// `(source_kind, source)`, as the table stores them.
    fn columns(&self) -> Result<(String, Value), ApiError> {
        let tagged = serde_json::to_value(self).map_err(ApiError::internal)?;
        match tagged {
            Value::Object(mut fields) => {
                match (fields.remove("source_kind"), fields.remove("source")) {
                    (Some(Value::String(kind)), Some(source)) => Ok((kind, source)),
                    _ => Err(ApiError::internal(
                        "a baseline source serialized without its kind or fields",
                    )),
                }
            }
            _ => Err(ApiError::internal(
                "a baseline source did not serialize as an object",
            )),
        }
    }
}

/// A baseline to write.
#[derive(Debug, Clone)]
pub struct NewBaseline {
    pub model_id: Uuid,
    pub effective_from: DateTime<Utc>,
    pub card: RateCard,
    pub source: BaselineSource,
}

/// One `pricing_baselines` row, with the email of the operator who wrote it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BaselineRevision {
    pub revision_id: i64,
    pub engine_version: i64,
    pub model_id: Uuid,
    pub effective_from: DateTime<Utc>,
    /// As stored, so a card this engine cannot read is still shown.
    pub card: Value,
    pub source_kind: String,
    pub source: Value,
    /// `None` when the control plane itself wrote it.
    pub created_by: Option<OperatorRef>,
    pub created_at: DateTime<Utc>,
}

impl BaselineRevision {
    /// Its source as typed, `None` when it does not decode as one.
    fn typed_source(&self) -> Option<BaselineSource> {
        serde_json::from_value(json!({
            "source_kind": self.source_kind,
            "source": self.source,
        }))
        .ok()
    }
}

impl<'r> sqlx::FromRow<'r, PgRow> for BaselineRevision {
    fn from_row(row: &'r PgRow) -> Result<BaselineRevision, sqlx::Error> {
        let created_by: Option<Uuid> = row.try_get("created_by")?;
        let email: Option<String> = row.try_get("created_by_email")?;
        Ok(BaselineRevision {
            revision_id: row.try_get("revision_id")?,
            engine_version: row.try_get("engine_version")?,
            model_id: row.try_get("model_id")?,
            effective_from: row.try_get("effective_from")?,
            card: row.try_get("card")?,
            source_kind: row.try_get("source_kind")?,
            source: row.try_get("source")?,
            created_by: created_by
                .zip(email)
                .map(|(id, email)| OperatorRef { id, email }),
            created_at: row.try_get("created_at")?,
        })
    }
}

/// The columns a [`BaselineRevision`] is read from, over `pricing_baselines
/// b` joined to `operators o` on `created_by`.
const COLUMNS: &str = "b.revision_id, b.engine_version, b.model_id, b.effective_from, b.card, \
                       b.source_kind, b.source, b.created_by, o.email AS created_by_email, \
                       b.created_at";

/// The baseline in force now for each of `model_ids` that has one: of its
/// revisions starting at or before now, the latest start, ties going to the
/// greatest `revision_id`, as the price book reads them. In no particular
/// order.
pub async fn current_baselines(
    conn: &mut PgConnection,
    model_ids: &[Uuid],
) -> Result<Vec<BaselineRevision>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT DISTINCT ON (b.model_id) {COLUMNS} \
         FROM pricing_baselines b LEFT JOIN operators o ON o.id = b.created_by \
         WHERE b.model_id = ANY($1) AND b.effective_from <= now() \
         ORDER BY b.model_id, b.effective_from DESC, b.revision_id DESC"
    ))
    .bind(model_ids)
    .fetch_all(conn)
    .await?)
}

/// The baselines of `model_ids` that start later than now, by model and
/// then by start. Of several starting at one instant only the greatest
/// `revision_id` is listed: it is the one that will apply.
pub async fn upcoming_baselines(
    conn: &mut PgConnection,
    model_ids: &[Uuid],
) -> Result<Vec<BaselineRevision>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT DISTINCT ON (b.model_id, b.effective_from) {COLUMNS} \
         FROM pricing_baselines b LEFT JOIN operators o ON o.id = b.created_by \
         WHERE b.model_id = ANY($1) AND b.effective_from > now() \
         ORDER BY b.model_id, b.effective_from, b.revision_id DESC"
    ))
    .bind(model_ids)
    .fetch_all(conn)
    .await?)
}

/// Writes a baseline starting at `row.effective_from`, refusing with `422`
/// one that starts in the past (`/effective_from`), a card that does not
/// validate (`/card`), and a model that is not live or is a channel's
/// generated direct document, which is priced through its public model
/// (`/model_id`). The revision, the `pricing_version` bump and the audit row
/// `pricing.baseline.create` go in the caller's transaction.
///
/// Each baseline of the same model already there that starts later takes
/// over from it then; each is named in a `superseded_later` warning, and the
/// write still succeeds.
pub async fn insert_baseline(
    conn: &mut PgConnection,
    row: &NewBaseline,
    actor: Actor<'_>,
) -> Result<(BaselineRevision, Vec<PriceWarning>), ApiError> {
    let revision = write_row(&mut *conn, row, actor).await?;
    let warnings = superseded_later(
        &mut *conn,
        row.model_id,
        row.effective_from,
        &[row.effective_from],
    )
    .await
    .map_err(ApiError::from)?;
    Ok((revision, warnings))
}

/// Writes `row` and nothing else: its checks, the revision, the
/// `pricing_version` bump and the audit row. [`insert_baseline`] adds the
/// `superseded_later` warnings; [`create`] computes them once, over every
/// baseline the request wrote.
async fn write_row(
    conn: &mut PgConnection,
    row: &NewBaseline,
    actor: Actor<'_>,
) -> Result<BaselineRevision, ApiError> {
    starts_now_or_later(&mut *conn, row.effective_from).await?;
    row.card
        .validate()
        .map_err(|e| refused("/card", format!("价目卡不合法：{e}")))?;
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM gw_resources WHERE kind = $1 AND id = $2 AND NOT deleted)",
    )
    .bind(Kind::Models.segment())
    .bind(row.model_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if !live {
        return Err(refused("/model_id", "模型不存在"));
    }
    // A channel's generated direct document is priced through its public
    // model: charges key on the public model's id, so a baseline written on
    // the direct document would price nothing.
    if let Some(public) = crate::channels::generated_direct_of(&mut *conn, row.model_id).await? {
        return Err(refused(
            "/model_id",
            format!("该模型由公共模型 {public} 生成，请为公共模型 {public} 设置基准价"),
        ));
    }
    let (source_kind, source) = row.source.columns()?;
    let revision: BaselineRevision = sqlx::query_as(&format!(
        "WITH b AS ( \
           INSERT INTO pricing_baselines \
           (engine_version, model_id, effective_from, card, source_kind, source, created_by) \
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *) \
         SELECT {COLUMNS} FROM b LEFT JOIN operators o ON o.id = b.created_by"
    ))
    .bind(i64::from(ENGINE_VERSION))
    .bind(row.model_id)
    .bind(row.effective_from)
    .bind(JsonColumn(&row.card))
    .bind(&source_kind)
    .bind(&source)
    .bind(created_by(actor))
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    bump_pricing_version(&mut *conn).await?;
    audit::record(
        &mut *conn,
        AuditEntry {
            actor,
            action: "pricing.baseline.create",
            target_kind: "pricing_baseline",
            target_id: Some(revision.revision_id.to_string()),
            tenant_id: None,
            detail: json!({
                "model_id": row.model_id,
                "effective_from": row.effective_from,
                "source_kind": source_kind,
            }),
        },
    )
    .await?;
    Ok(revision)
}

/// The `superseded_later` warnings of a write of model `model_id` starting
/// at `starts`: each baseline of the model already there that starts later
/// than `starts` and at none of `own` (the instants this write covered with
/// a revision of its own, which no older revision supersedes) takes over
/// from it then.
async fn superseded_later(
    conn: &mut PgConnection,
    model_id: Uuid,
    starts: DateTime<Utc>,
    own: &[DateTime<Utc>],
) -> Result<Vec<PriceWarning>, DbError> {
    let later: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT revision_id, effective_from FROM pricing_baselines \
         WHERE model_id = $1 AND effective_from > $2 AND effective_from <> ALL($3) \
         ORDER BY effective_from, revision_id",
    )
    .bind(model_id)
    .bind(starts)
    .bind(own)
    .fetch_all(&mut *conn)
    .await?;
    Ok(later
        .into_iter()
        .map(|(revision_id, effective_from)| PriceWarning {
            code: "superseded_later",
            message: format!(
                "已有一条更晚生效的基准价（修订 {revision_id}），将从 {} 起取代这次写入的基准价",
                effective_from.to_rfc3339()
            ),
            revision_id: Some(revision_id),
            effective_from: Some(effective_from),
        })
        .collect())
}

/// Whether an entry's list price holds only at some times of day: its notes
/// speak of peak hours. Such a price is the off-peak one.
fn has_time_of_day_price(entry: &CatalogueEntry) -> bool {
    entry
        .notes
        .as_deref()
        .is_some_and(|n| n.to_ascii_lowercase().contains("peak"))
}

/// The entries a catalogue write takes, each with when its baseline starts,
/// by start.
pub type CataloguePlan<'c> = Vec<(DateTime<Utc>, &'c CatalogueEntry)>;

/// The baselines writing catalogue `key` (`<provider>/<model>`, or an alias)
/// at `effective_from` comes to, each with when it starts:
/// - the entry in effect at `effective_from`, starting then;
/// - each later-dated entry of the same key, starting at its own date, so a
///   price change the catalogue announces carries into the baseline.
///
/// An unknown key is refused at `/catalogue_key`, and a key with no entry in
/// effect yet at `/effective_from`. An entry whose list price holds only off
/// peak is written all the same, with a `time_of_day_price` warning.
pub fn catalogue_plan<'c>(
    catalogue: &'c Catalogue,
    key: &str,
    effective_from: DateTime<Utc>,
) -> Result<(CataloguePlan<'c>, Vec<PriceWarning>), ApiError> {
    let unknown = || refused("/catalogue_key", "价目目录中没有这个模型");
    let (provider, model) = key.split_once('/').ok_or_else(unknown)?;
    // Known at all: some entry of it is in effect at the end of time.
    catalogue
        .get(provider, model, DateTime::<Utc>::MAX_UTC)
        .ok_or_else(unknown)?;
    let first = catalogue
        .get(provider, model, effective_from)
        .ok_or_else(|| refused("/effective_from", "该目录价格在此生效时间之前还没有生效"))?;
    let mut later: Vec<(DateTime<Utc>, &CatalogueEntry)> = catalogue
        .entries()
        .iter()
        .filter(|e| e.key == first.key)
        .filter_map(|e| e.effective_from.map(|d| (day_start(d), e)))
        .filter(|(starts, _)| *starts > effective_from)
        .collect();
    later.sort_by_key(|(starts, _)| *starts);
    let plan: CataloguePlan<'c> = std::iter::once((effective_from, first))
        .chain(later)
        .collect();
    let mut warnings: Vec<PriceWarning> = plan
        .iter()
        .filter(|(_, e)| has_time_of_day_price(e))
        .map(|(_, e)| PriceWarning {
            code: "time_of_day_price",
            message: format!(
                "{} 的目录价格是非高峰时段价格，高峰时段上游按更高价格收费；基准价按非高峰价格计",
                e.key
            ),
            revision_id: None,
            effective_from: None,
        })
        .collect();
    warnings.dedup();
    Ok((plan, warnings))
}

/// The baseline of model `model_id` starting at `starts` from catalogue
/// entry `entry`, under `catalogue`'s provenance.
fn catalogue_row(
    catalogue: &Catalogue,
    model_id: Uuid,
    starts: DateTime<Utc>,
    entry: &CatalogueEntry,
) -> NewBaseline {
    NewBaseline {
        model_id,
        effective_from: starts,
        card: entry.card.clone(),
        source: BaselineSource::Catalogue {
            catalogue_version: catalogue.version().to_string(),
            entry_key: entry.key.clone(),
            entry_effective_from: entry.effective_from,
            source_url: entry.source_url.clone(),
            as_of: entry.as_of,
        },
    }
}

/// `catalogue_update`: what the catalogue now says for an entry key, from
/// the entry in effect now on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogueUpdate {
    pub catalogue_version: String,
    pub entry_key: String,
    pub entries: Vec<CatalogueUpdateEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogueUpdateEntry {
    /// The entry's own date; `None` for an undated entry.
    pub effective_from: Option<NaiveDate>,
    pub card: RateCard,
}

/// The catalogue's word on the baseline in force, `current`, when it differs
/// from what the baselines say; `None` when it agrees, when `current` did not
/// come from the catalogue, and when the catalogue no longer has its key in
/// effect at `now` (there is nothing to confirm then).
///
/// The two price schedules from `now` on are compared as what they charge at
/// each instant either of them changes: every start of the catalogue's
/// ([`catalogue_plan`] at `now`) and of the baselines written from the same
/// entry key (`current`, then each of `upcoming`). A schedule this engine
/// cannot read agrees with nothing at its instants, so it is an update too.
///
/// So a different card, a new dated entry, a dated entry moved either way
/// and a dated entry dropped all show as an update; a newer catalogue that
/// charges the same, or a re-read date (`as_of`), does not. A baseline
/// written from the update agrees again.
///
/// An instant where a baseline from *another* entry key (or a manual or
/// third-party one) starts is not judged: the operator departed from the
/// catalogue there, and the update's confirm would write over that choice.
pub fn catalogue_update(
    catalogue: &Catalogue,
    current: &BaselineRevision,
    upcoming: &[BaselineRevision],
    now: DateTime<Utc>,
) -> Option<CatalogueUpdate> {
    /// The entry key and date a revision was written from, when it was
    /// written from the catalogue.
    fn origin(r: &BaselineRevision) -> Option<(String, Option<NaiveDate>)> {
        match r.typed_source()? {
            BaselineSource::Catalogue {
                entry_key,
                entry_effective_from,
                ..
            } => Some((entry_key, entry_effective_from)),
            _ => None,
        }
    }
    let (key, _) = origin(current)?;
    let (provider, model) = key.split_once('/')?;
    // `None` when the key has no entry in effect now.
    let (plan, _) = catalogue_plan(catalogue, &key, now).ok()?;
    // What the baselines of this key charge, by start.
    let mut ours: Vec<(DateTime<Utc>, Option<RateCard>)> = std::iter::once(current)
        .chain(upcoming)
        .filter(|r| origin(r).is_some_and(|(k, _)| k == key))
        .map(|r| {
            // A card this engine cannot read agrees with no entry.
            let card = serde_json::from_value(r.card.clone()).ok();
            (r.effective_from, card)
        })
        .collect();
    ours.sort_by_key(|(starts, _)| *starts);
    let masked: std::collections::HashSet<DateTime<Utc>> = upcoming
        .iter()
        .filter(|r| origin(r).is_none_or(|(k, _)| k != key))
        .map(|r| r.effective_from)
        .collect();
    let mut instants: Vec<DateTime<Utc>> = std::iter::once(now)
        .chain(plan.iter().map(|(starts, _)| *starts))
        .chain(ours.iter().map(|(starts, _)| *starts))
        .filter(|t| *t >= now && !masked.contains(t))
        .collect();
    instants.sort();
    instants.dedup();
    let differs = instants.into_iter().any(|t| {
        let says = catalogue.get(provider, model, t).map(|e| e.card.clone());
        let written = ours
            .iter()
            .rfind(|(starts, _)| *starts <= t)
            .and_then(|(_, card)| card.clone());
        says != written
    });
    if !differs {
        return None;
    }
    Some(CatalogueUpdate {
        catalogue_version: catalogue.version().to_string(),
        entry_key: key,
        entries: plan
            .into_iter()
            .map(|(_, e)| CatalogueUpdateEntry {
                effective_from: e.effective_from,
                card: e.card.clone(),
            })
            .collect(),
    })
}

/// `BaselineItem`: one live model's baselines.
#[derive(Debug, Serialize)]
struct BaselineItem {
    model_id: Uuid,
    name: String,
    current: Option<BaselineRevision>,
    /// By ascending `effective_from`.
    upcoming: Vec<BaselineRevision>,
    catalogue_update: Option<CatalogueUpdate>,
}

/// `GET /admin/pricing/baselines` → `{items: [BaselineItem]}`: every live
/// model's baseline in force now, the ones scheduled after it, and the
/// catalogue's update to it, if any ([`catalogue_update`]). A channel's
/// generated direct document is left out: it is priced through its public
/// model. The models with no baseline in force come first, each group by
/// name. Read in one transaction, so every item shares one `now()`.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let catalogue = state.prices.catalogue().clone();
    let mut items = state
        .db
        .tx(move |conn| {
            async move {
                let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
                    .fetch_one(&mut *conn)
                    .await?;
                // A channel's generated direct document takes no baseline
                // (`write_row`), so it is not offered one.
                let models = crate::channels::priceable_models(&mut *conn).await?;
                let ids: Vec<Uuid> = models.iter().map(|(id, _)| *id).collect();
                let mut current: HashMap<Uuid, BaselineRevision> =
                    current_baselines(&mut *conn, &ids)
                        .await?
                        .into_iter()
                        .map(|r| (r.model_id, r))
                        .collect();
                let mut upcoming: HashMap<Uuid, Vec<BaselineRevision>> = HashMap::new();
                // By start within each model, as read.
                for r in upcoming_baselines(&mut *conn, &ids).await? {
                    upcoming.entry(r.model_id).or_default().push(r);
                }
                let items: Vec<BaselineItem> = models
                    .into_iter()
                    .map(|(model_id, name)| {
                        let current = current.remove(&model_id);
                        let upcoming = upcoming.remove(&model_id).unwrap_or_default();
                        let catalogue_update = current
                            .as_ref()
                            .and_then(|c| catalogue_update(&catalogue, c, &upcoming, now));
                        BaselineItem {
                            model_id,
                            name,
                            current,
                            upcoming,
                            catalogue_update,
                        }
                    })
                    .collect();
                Ok::<_, DbError>(items)
            }
            .boxed()
        })
        .await?;
    // Stable, so each group keeps the name order.
    items.sort_by_key(|i| i.current.is_some());
    Ok(Json(json!({ "items": items })))
}

/// `POST /admin/pricing/baselines`: exactly one of `card` (a rate card, with
/// its `note`) and `catalogue_key`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineBody {
    model_id: Uuid,
    effective_from: DateTime<Utc>,
    #[serde(default)]
    card: Option<Value>,
    #[serde(default)]
    catalogue_key: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /admin/pricing/baselines` → `201 {revision, later_revisions,
/// warnings}`. Two bodies:
/// - `{model_id, effective_from, card, note}`: an operator's own card, with
///   a note of 1 to 500 characters saying where it comes from;
/// - `{model_id, effective_from, catalogue_key}`: the catalogue's list price
///   (see [`catalogue_plan`]); `later_revisions` are the later-dated entries
///   it also wrote.
///
/// Both `card` and `catalogue_key`, or neither, is `422 /card`; a card
/// without a note, and a note beside a catalogue key, `422 /note`. A
/// proposal of an import is confirmed only through its own route. The write
/// is one transaction however many baselines it writes.
///
/// A catalogue write also re-asserts every later instant at which a baseline
/// from the same key is scheduled and the plan carries no row (see
/// [`reassert_later`]), so confirming an update that shows a moved or
/// dropped dated entry leaves the schedule agreeing with the catalogue, and
/// the update gone. `revision` is the one at `effective_from`.
pub async fn create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<BaselineBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let BaselineBody {
        model_id,
        effective_from,
        card,
        catalogue_key,
        note,
    } = body;
    // The rows the request writes by itself, and, for a catalogue key, the
    // catalogue and entry key the write re-asserts the later instants from.
    let (rows, mut warnings, reassert) = match (card, catalogue_key) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(refused(
                "/card",
                "必须且只能提供 card 或 catalogue_key 其中之一",
            ))
        }
        (Some(card), None) => {
            let note = note
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| refused("/note", "手动录入需要填写说明"))?;
            if note.chars().count() > MAX_NOTE_CHARS {
                return Err(refused("/note", "说明最多 500 个字符"));
            }
            let card: RateCard = serde_json::from_value(card)
                .map_err(|e| refused("/card", format!("价目卡不合法：{e}")))?;
            let row = NewBaseline {
                model_id,
                effective_from,
                card,
                source: BaselineSource::Manual { note },
            };
            (vec![row], Vec::new(), None)
        }
        (None, Some(key)) => {
            if note.is_some() {
                return Err(refused("/note", "说明只用于手动录入的价目卡"));
            }
            let catalogue = state.prices.catalogue();
            let (plan, warnings) = catalogue_plan(catalogue, &key, effective_from)?;
            let rows: Vec<NewBaseline> = plan
                .iter()
                .map(|(starts, e)| catalogue_row(catalogue, model_id, *starts, e))
                .collect();
            // The key the plan's entries carry, which the later instants'
            // re-assertion matches a scheduled baseline's provenance against.
            let entry_key = plan
                .first()
                .map(|(_, e)| e.key.clone())
                .ok_or_else(|| ApiError::internal("a catalogue plan with no entry"))?;
            (rows, warnings, Some((catalogue.clone(), entry_key)))
        }
    };
    let written = state
        .db
        .tx(move |conn| {
            async move {
                let actor = Actor::Operator(&auth);
                let mut rows = rows;
                if let Some((catalogue, entry_key)) = &reassert {
                    reassert_later(
                        &mut *conn,
                        catalogue,
                        entry_key,
                        model_id,
                        effective_from,
                        &mut rows,
                    )
                    .await?;
                }
                // The requested start first, then each other by start: a
                // re-asserted instant may well fall before one the plan
                // carried.
                rows.sort_by_key(|r| r.effective_from);
                let starts: Vec<DateTime<Utc>> = rows.iter().map(|r| r.effective_from).collect();
                let mut written = Vec::with_capacity(rows.len());
                for row in &rows {
                    written.push(write_row(&mut *conn, row, actor).await?);
                }
                // Once, over every instant the write covered: a baseline
                // that one of them starts at is superseded by ours, not by
                // an older one.
                let superseded =
                    superseded_later(&mut *conn, model_id, effective_from, &starts).await?;
                Ok::<_, ApiError>((written, superseded))
            }
            .boxed()
        })
        .await?;
    let (written, superseded) = written;
    warnings.extend(superseded);
    let mut written = written.into_iter();
    let revision = written
        .next()
        .ok_or_else(|| ApiError::internal("a baseline write wrote no revision"))?;
    let later: Vec<BaselineRevision> = written.collect();
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "revision": revision,
            "later_revisions": later,
            "warnings": warnings,
        })),
    ))
}

/// Appends to `rows` what the catalogue says at each instant a scheduled
/// baseline written from catalogue entry `entry_key` starts, where this
/// write's plan carries no row of its own: the dated entry was moved or
/// dropped, and left alone the stale revision would bill its old card from
/// its date.
///
/// The row is written even when the schedule would already charge the same
/// from that instant (a change brought forward leaves the old date charging
/// the new price). Same card or not, it supersedes the stale revision of the
/// same instant, so the listed schedule names only what the catalogue says.
///
/// A scheduled baseline written from another key, or a manual or third-party
/// one, is the operator's own choice and is left alone.
async fn reassert_later(
    conn: &mut PgConnection,
    catalogue: &Catalogue,
    entry_key: &str,
    model_id: Uuid,
    effective_from: DateTime<Utc>,
    rows: &mut Vec<NewBaseline>,
) -> Result<(), ApiError> {
    let (provider, model) = entry_key
        .split_once('/')
        .ok_or_else(|| ApiError::internal("a catalogue entry key is not <provider>/<model>"))?;
    let ours: std::collections::HashSet<DateTime<Utc>> =
        rows.iter().map(|r| r.effective_from).collect();
    for stale in upcoming_baselines(&mut *conn, &[model_id]).await? {
        let from_key = matches!(
            stale.typed_source(),
            Some(BaselineSource::Catalogue { entry_key: k, .. }) if k == entry_key
        );
        if !from_key
            || stale.effective_from <= effective_from
            || ours.contains(&stale.effective_from)
        {
            continue;
        }
        let entry = catalogue
            .get(provider, model, stale.effective_from)
            .ok_or_else(|| {
                ApiError::internal(format!(
                    "{entry_key} has no entry in effect at {}, where a revision of it starts, \
                     though it has one at the end of time",
                    stale.effective_from
                ))
            })?;
        rows.push(catalogue_row(
            catalogue,
            model_id,
            stale.effective_from,
            entry,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use aisix_pricing::{Rate, ServiceTier};

    use super::*;

    /// A catalogue with an announced price change (`openai/m1` from
    /// 2026-12-01 and 2027-02-01), a key only priced from 2027-01-01, and an
    /// off-peak price with an alias.
    fn catalogue() -> Catalogue {
        let entry = |key: &str, extra: &str, input: &str| {
            format!(
                r#"{{"key":"{key}","source_url":"https://openai.com/api/pricing","as_of":"2026-11-01",{extra}"card":{{"default":{{"base":{{"input":"{input}","output":"2"}}}}}}}}"#
            )
        };
        let entries = [
            entry("openai/m1", "", "1"),
            entry("openai/m1", r#""effective_from":"2026-12-01","#, "3"),
            entry("openai/m1", r#""effective_from":"2027-02-01","#, "5"),
            entry("openai/soon", r#""effective_from":"2027-01-01","#, "1"),
            entry(
                "openai/offpeak",
                r#""aliases":["offpeak-alias"],"notes":"Standard (off-peak) price; peak windows bill double.","#,
                "1",
            ),
            entry(
                "openai/offpeak",
                r#""aliases":["offpeak-alias"],"notes":"Off-peak price again.","effective_from":"2026-12-01","#,
                "2",
            ),
        ];
        Catalogue::from_json(&format!(
            r#"{{"version":"2026-11-06","entries":[{}]}}"#,
            entries.join(",")
        ))
        .expect("the test catalogue loads")
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    fn day(s: &str) -> NaiveDate {
        s.parse().expect("a date")
    }

    fn input(card: &RateCard) -> Rate {
        card.0[&ServiceTier::default_tier()].base.input
    }

    /// `(starts, input rate, entry date)` of each planned baseline.
    fn summary(
        plan: &[(DateTime<Utc>, &CatalogueEntry)],
    ) -> Vec<(DateTime<Utc>, Rate, Option<NaiveDate>)> {
        plan.iter()
            .map(|(starts, e)| (*starts, input(&e.card), e.effective_from))
            .collect()
    }

    fn path_of(e: &ApiError) -> Vec<&str> {
        e.details.iter().map(|d| d.path.as_str()).collect()
    }

    #[test]
    fn a_plan_carries_every_announced_price_change() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        let (plan, warnings) = catalogue_plan(&c, "openai/m1", now).expect("a plan");
        assert_eq!(
            summary(&plan),
            vec![
                (now, Rate(1_000_000), None),
                (
                    at("2026-12-01T00:00:00Z"),
                    Rate(3_000_000),
                    Some(day("2026-12-01"))
                ),
                (
                    at("2027-02-01T00:00:00Z"),
                    Rate(5_000_000),
                    Some(day("2027-02-01"))
                ),
            ]
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_plan_starts_from_the_entry_in_effect_then() {
        let c = catalogue();
        // On the day of the change, the change itself is in effect.
        let (plan, _) =
            catalogue_plan(&c, "openai/m1", at("2026-12-01T00:00:00Z")).expect("a plan");
        assert_eq!(
            summary(&plan),
            vec![
                (
                    at("2026-12-01T00:00:00Z"),
                    Rate(3_000_000),
                    Some(day("2026-12-01"))
                ),
                (
                    at("2027-02-01T00:00:00Z"),
                    Rate(5_000_000),
                    Some(day("2027-02-01"))
                ),
            ]
        );
        let (plan, _) =
            catalogue_plan(&c, "openai/m1", at("2027-03-01T00:00:00Z")).expect("a plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(input(&plan[0].1.card), Rate(5_000_000));
    }

    #[test]
    fn a_plan_names_a_known_key_in_effect_at_its_start() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        for key in ["openai/nope", "nope", "anthropic/m1", "openai/"] {
            let e = catalogue_plan(&c, key, now).expect_err(key);
            assert_eq!(path_of(&e), ["/catalogue_key"], "{key}");
        }
        let e = catalogue_plan(&c, "openai/soon", now).expect_err("not in effect yet");
        assert_eq!(path_of(&e), ["/effective_from"]);
        let (plan, _) =
            catalogue_plan(&c, "openai/soon", at("2027-01-01T00:00:00Z")).expect("in effect");
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn an_off_peak_list_price_is_planned_with_one_warning() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        let (plan, warnings) = catalogue_plan(&c, "openai/offpeak-alias", now).expect("a plan");
        // The alias names the entry, whose key the plan carries.
        assert!(plan.iter().all(|(_, e)| e.key == "openai/offpeak"));
        assert_eq!(plan.len(), 2);
        assert_eq!(
            warnings,
            vec![PriceWarning {
                code: "time_of_day_price",
                message: "openai/offpeak 的目录价格是非高峰时段价格，高峰时段上游按更高价格收费；\
                          基准价按非高峰价格计"
                    .to_string(),
                revision_id: None,
                effective_from: None,
            }]
        );
    }

    #[test]
    fn every_builtin_deepseek_entry_warns_and_nothing_else_does() {
        let builtin = Catalogue::builtin().expect("the builtin catalogue");
        // DeepSeek lists an off-peak price; every entry of it must say so,
        // and a note reworded past the check fails here.
        let deepseek: Vec<&CatalogueEntry> = builtin
            .entries()
            .iter()
            .filter(|e| e.key.starts_with("deepseek/"))
            .collect();
        assert!(!deepseek.is_empty());
        for e in &deepseek {
            let (_, warnings) = catalogue_plan(&builtin, &e.key, at("2026-11-06T09:30:00Z"))
                .unwrap_or_else(|err| panic!("plan {}: {err:?}", e.key));
            let codes: Vec<&str> = warnings.iter().map(|w| w.code).collect();
            assert_eq!(codes, ["time_of_day_price"], "{}", e.key);
        }
        let warned: std::collections::BTreeSet<&str> = builtin
            .entries()
            .iter()
            .filter(|e| has_time_of_day_price(e))
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(
            warned,
            std::collections::BTreeSet::from([
                "deepseek/deepseek-flash",
                "deepseek/deepseek-v4-pro"
            ])
        );
    }

    /// A revision of a model's baseline starting at `starts` with an input
    /// of `input` USD per million, written from `source`.
    fn revision(starts: &str, input: &str, source: BaselineSource) -> BaselineRevision {
        let (source_kind, source) = source.columns().expect("a source");
        BaselineRevision {
            revision_id: 1,
            engine_version: i64::from(ENGINE_VERSION),
            model_id: Uuid::nil(),
            effective_from: at(starts),
            card: json!({"default": {"base": {"input": input, "output": "2"}}}),
            source_kind,
            source,
            created_by: None,
            created_at: at(starts),
        }
    }

    /// Written from entry `key` dated `dated` of catalogue `version`.
    fn from_catalogue(version: &str, key: &str, dated: Option<&str>) -> BaselineSource {
        BaselineSource::Catalogue {
            catalogue_version: version.to_string(),
            entry_key: key.to_string(),
            entry_effective_from: dated.map(day),
            source_url: "https://openai.com/api/pricing".to_string(),
            as_of: day("2026-11-01"),
        }
    }

    /// The entry dates `update` lists.
    fn dates(update: &CatalogueUpdate) -> Vec<Option<NaiveDate>> {
        update.entries.iter().map(|e| e.effective_from).collect()
    }

    #[test]
    fn a_catalogue_that_says_the_same_is_no_update() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        // Written now from openai/m1: its entry in force and its two changes.
        let current = revision(
            "2026-11-06T09:30:00Z",
            "1",
            from_catalogue("2026-11-01", "openai/m1", None),
        );
        let upcoming = [
            revision(
                "2026-12-01T00:00:00Z",
                "3",
                from_catalogue("2026-11-01", "openai/m1", Some("2026-12-01")),
            ),
            revision(
                "2027-02-01T00:00:00Z",
                "5",
                from_catalogue("2026-11-01", "openai/m1", Some("2027-02-01")),
            ),
        ];
        // Another version of the catalogue that says the same.
        assert_eq!(catalogue_update(&c, &current, &upcoming, now), None);
        // Once the first change took over.
        let later = at("2026-12-02T00:00:00Z");
        assert_eq!(
            catalogue_update(&c, &upcoming[0], &upcoming[1..], later),
            None
        );
        // The same entry written again for a later instant changes nothing.
        let again = revision(
            "2026-11-20T00:00:00Z",
            "1",
            from_catalogue("2026-11-06", "openai/m1", None),
        );
        let with_again = [again, upcoming[0].clone(), upcoming[1].clone()];
        assert_eq!(catalogue_update(&c, &current, &with_again, now), None);
        // A manual baseline scheduled in between is not the catalogue's.
        let manual = revision(
            "2026-11-20T00:00:00Z",
            "9",
            BaselineSource::Manual {
                note: "n".to_string(),
            },
        );
        let with_manual = [manual.clone(), upcoming[0].clone(), upcoming[1].clone()];
        assert_eq!(catalogue_update(&c, &current, &with_manual, now), None);
        // Nor is a baseline in force that did not come from the catalogue.
        assert_eq!(catalogue_update(&c, &manual, &upcoming, now), None);
        // Nor is an instant where the operator's own baseline starts: the
        // first change of openai/m1 is the catalogue's, and a manual
        // revision at that same instant leaves the rest of the schedule
        // agreeing with it.
        let at_first_change = revision(
            "2026-12-01T00:00:00Z",
            "9",
            BaselineSource::Manual {
                note: "n".to_string(),
            },
        );
        assert_eq!(
            catalogue_update(&c, &current, &[at_first_change, upcoming[1].clone()], now),
            None
        );
    }

    #[test]
    fn a_new_card_or_date_in_the_catalogue_is_an_update() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        let source = |dated| from_catalogue("2026-11-01", "openai/m1", dated);
        let current = revision("2026-11-06T09:30:00Z", "1", source(None));
        let first = revision("2026-12-01T00:00:00Z", "3", source(Some("2026-12-01")));
        let second = revision("2027-02-01T00:00:00Z", "5", source(Some("2027-02-01")));
        let expected = vec![None, Some(day("2026-12-01")), Some(day("2027-02-01"))];

        // An announced change the baselines never took.
        let update =
            catalogue_update(&c, &current, std::slice::from_ref(&first), now).expect("an update");
        assert_eq!(
            (
                update.catalogue_version.as_str(),
                update.entry_key.as_str(),
                dates(&update)
            ),
            ("2026-11-06", "openai/m1", expected.clone())
        );
        assert_eq!(input(&update.entries[2].card), Rate(5_000_000));
        // A card the catalogue no longer lists.
        let repriced = revision("2026-11-06T09:30:00Z", "1.5", source(None));
        let update = catalogue_update(&c, &repriced, &[first.clone(), second.clone()], now)
            .expect("an update");
        assert_eq!(dates(&update), expected);
        // A change dated a day the catalogue does not announce.
        let moved = revision("2026-12-02T00:00:00Z", "3", source(Some("2026-12-02")));
        assert!(catalogue_update(&c, &current, &[moved, second.clone()], now).is_some());
        // A card this engine cannot read.
        let mut unreadable = current.clone();
        unreadable.card = json!({"default": "?"});
        assert!(catalogue_update(&c, &unreadable, &[first, second], now).is_some());
    }

    #[test]
    fn a_key_the_catalogue_no_longer_has_in_effect_is_no_update() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        for key in ["openai/gone", "openai/soon"] {
            let current = revision(
                "2026-11-06T09:30:00Z",
                "1",
                from_catalogue("2026-11-01", key, None),
            );
            assert_eq!(catalogue_update(&c, &current, &[], now), None, "{key}");
        }
    }

    #[test]
    fn a_source_is_stored_as_its_kind_and_its_fields() {
        let (kind, fields) = BaselineSource::Manual {
            note: "n".to_string(),
        }
        .columns()
        .expect("columns");
        assert_eq!((kind.as_str(), fields), ("manual", json!({"note": "n"})));
        let (kind, fields) = from_catalogue("2026-11-06", "openai/m1", Some("2026-12-01"))
            .columns()
            .expect("columns");
        assert_eq!(kind, "catalogue");
        assert_eq!(
            fields,
            json!({
                "catalogue_version": "2026-11-06",
                "entry_key": "openai/m1",
                "entry_effective_from": "2026-12-01",
                "source_url": "https://openai.com/api/pricing",
                "as_of": "2026-11-01",
            })
        );
    }
}
