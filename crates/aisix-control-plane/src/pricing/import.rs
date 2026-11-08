//! The third-party price import: an operator uploads a public price dataset,
//! and each priced entry of it, per date it takes effect, becomes a proposal
//! labelled third-party. Nothing here is a price until an operator confirms a
//! proposal.
//!
//! Every price is read from its decimal text and never through a binary
//! float: a number that is not plain digits with at most six decimals is
//! flagged, never rounded or truncated.
//!
//! # How an entry becomes proposals
//!
//! The dataset is an array of providers, each with its `pricing_urls` and
//! its `models`. A model's `prices` is a price map, or a list of maps each
//! with an optional `constraint`; its `price_variants` are partial maps
//! selected by a `when` condition, each with an optional `constraint`. The
//! dataset's own resolution, as its reference package resolves a model's
//! prices for a request and prices a tiered field (read at the commit the
//! test snapshot was taken from), is followed exactly:
//!
//! - **Dates.** In a list, the last entry whose `start_date` (UTC day) has
//!   come, or that has no constraint, is the standard price; if none, the
//!   first. One proposal is made for the moment of upload and one for each
//!   later `start_date` of an entry or a variant, each priced as the dataset
//!   would price that day. A date already past makes nothing of its own.
//! - **Variants.** `{processing: batch}` is the `batch` tier and
//!   `{service_tier: <name>}` that tier. A variant overrides only the prices
//!   it names; every other price of its tier is the standard one. For each
//!   price the first active variant of the tier wins. A `default` variant
//!   that repeats the standard prices changes nothing; one that differs, a
//!   name the rate model refuses as a tier, and any other condition are
//!   flagged `unsupported_variant`.
//! - **Tiers.** `{base, tiers: [{start, price}]}` applies a tier's price to
//!   every token once the prompt is strictly longer than `start`, as a
//!   long-context band does. Each distinct `start` across the card is one
//!   band, and in it each price is its tier with the greatest `start` at or
//!   below the band's threshold, or its base.
//! - **Fields** map to card dimensions one to one; `web_searches_kcount` is
//!   priced per thousand searches and becomes `web_search` per million. Any
//!   other field is flagged `unsupported_field` and left off the card.
//! - **Time windows** (`start_time`, `end_time`) anywhere in a model flag
//!   every proposal of it `time_of_day`; the price without the window is the
//!   card.
//!
//! A proposal with a `bad_number` has no card, and neither does one whose
//! prices do not make a card [`RateCard::validate`] accepts (`invalid_card`).
//! Any other flag leaves the card in place for the comparison, but a flagged
//! proposal is never confirmable.
//!
//! # Uploads and proposals
//!
//! `POST /admin/pricing/imports` writes the batch and its proposals in one
//! transaction, and supersedes every open batch, with its pending proposals,
//! first: a newer upload is what an operator reviews. `GET
//! /admin/pricing/imports` lists the newest [`LISTED_BATCHES`], and `GET
//! /admin/pricing/imports/{id}` a batch with its proposals and, computed on
//! read, each proposal's suggested model's [`BaselineRevision`] in force, the
//! built-in catalogue's entry for the same provider and model, and whether
//! the card already equals that baseline. A proposal with a model of its own
//! is suggested by the live direct model whose document carries its
//! `provider` and `model_name`; confirmation, which is what turns one into a
//! price, is another route's.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use aisix_pricing::{
    CatalogueEntry, Dimension, LongContextBand, Rate, RateCard, Rates, ServiceTier, TierRates,
};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::types::Json as JsonColumn;
use sqlx::{PgConnection, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use super::baselines::{self, BaselineRevision, OperatorRef};
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::resources::Kind;
use crate::usage::query_rejected;
use crate::AppState;

/// The dataset this import reads, recorded on every batch and on every
/// baseline confirmed from it. The one place its name appears in code.
pub const DATASET: &str = "pydantic/genai-prices";

/// The largest upload `POST /admin/pricing/imports` reads.
pub const MAX_IMPORT_BYTES: usize = 8 << 20;

/// The most proposals one upload may make.
pub const MAX_IMPORT_PROPOSALS: usize = 10_000;

/// Why a proposal cannot be confirmed as it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// A price that is not plain digits with at most six decimals.
    BadNumber,
    /// A price for a quantity no card dimension meters.
    UnsupportedField,
    /// A price under a condition a card cannot express.
    UnsupportedVariant,
    /// A price that holds only at some times of day.
    TimeOfDay,
    /// The prices do not make a card the rate model accepts.
    InvalidCard,
}

/// One dataset entry at one date: the card it comes to, or the flags that
/// say why it comes to none.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedProposal {
    /// The dataset's provider id.
    pub provider: String,
    /// The dataset's model id.
    pub model: String,
    /// `None`: in effect at upload. Otherwise the UTC midnight of the date it
    /// takes effect.
    pub effective_from: Option<DateTime<Utc>>,
    pub card: Option<RateCard>,
    pub flags: Vec<Flag>,
    /// The provider's first cited pricing page.
    pub pricing_url: Option<String>,
    /// The date the dataset says it checked the model's prices, if it says.
    pub prices_checked: Option<NaiveDate>,
}

/// The most batches `GET /admin/pricing/imports` lists.
const LISTED_BATCHES: usize = 20;

/// A `422` at `path`.
fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

/// Every proposal `data` makes at `now`, in the dataset's order of
/// providers and models, each model's in date order. See the module
/// documentation.
///
/// Refused at `/data`: a body that is not an array of providers with their
/// models, one that prices nothing, and one that makes more than
/// [`MAX_IMPORT_PROPOSALS`] proposals.
pub fn parse(data: &RawValue, now: DateTime<Utc>) -> Result<Vec<ParsedProposal>, ApiError> {
    let unrecognised = || refused("/data", "不是可识别的价格数据集");
    let providers: Vec<Provider<'_>> =
        serde_json::from_str(data.get()).map_err(|_| unrecognised())?;
    let today = now.date_naive();
    let mut proposals = Vec::new();
    for provider in &providers {
        let pricing_url = provider.pricing_urls.first().cloned();
        for model in &provider.models {
            let entries = conditionals(model.prices).ok_or_else(unrecognised)?;
            let variants = model.price_variants.as_deref().unwrap_or_default();
            for (effective_from, card, flags) in model_proposals(&entries, variants, today) {
                if proposals.len() == MAX_IMPORT_PROPOSALS {
                    return Err(refused(
                        "/data",
                        format!("价格数据集产生的提议超过 {MAX_IMPORT_PROPOSALS} 条"),
                    ));
                }
                proposals.push(ParsedProposal {
                    provider: provider.id.clone(),
                    model: model.id.clone(),
                    effective_from,
                    card,
                    flags,
                    pricing_url: pricing_url.clone(),
                    prices_checked: model.prices_checked,
                });
            }
        }
    }
    if proposals.is_empty() {
        return Err(unrecognised());
    }
    Ok(proposals)
}

/// One upload of a dataset.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportBatch {
    pub id: Uuid,
    pub dataset: String,
    pub dataset_ref: String,
    /// `open`, or `superseded` once a newer upload has taken over.
    pub status: String,
    /// How many proposals it made.
    pub proposals: i64,
    /// How many of them are flagged.
    pub flagged: i64,
    pub created_by: OperatorRef,
    pub created_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, PgRow> for ImportBatch {
    fn from_row(row: &'r PgRow) -> Result<ImportBatch, sqlx::Error> {
        Ok(ImportBatch {
            id: row.try_get("id")?,
            dataset: row.try_get("dataset")?,
            dataset_ref: row.try_get("dataset_ref")?,
            status: row.try_get("status")?,
            proposals: row.try_get("proposals")?,
            flagged: row.try_get("flagged")?,
            created_by: OperatorRef {
                id: row.try_get("created_by")?,
                email: row.try_get("created_by_email")?,
            },
            created_at: row.try_get("created_at")?,
        })
    }
}

/// The columns an [`ImportBatch`] is read from, over
/// `pricing_import_batches b` joined to `operators o` on `created_by`.
const BATCH_COLUMNS: &str = "b.id, b.dataset, b.dataset_ref, b.status, b.proposals, b.flagged, \
                             b.created_by, o.email AS created_by_email, b.created_at";

/// One proposal, as stored, with what the review compares it to, computed on
/// read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    pub id: Uuid,
    pub batch_id: Uuid,
    /// The dataset's provider id.
    pub provider: String,
    /// The dataset's model id.
    pub model: String,
    /// `None`: in effect at upload.
    pub effective_from: Option<DateTime<Utc>>,
    /// The card it comes to, when its prices made one.
    pub card: Option<Value>,
    pub flags: Vec<Flag>,
    /// The provider's first cited pricing page.
    pub pricing_url: Option<String>,
    pub prices_checked: Option<NaiveDate>,
    /// A live direct model whose provider and model name are the entry's.
    pub suggested_model_id: Option<Uuid>,
    /// `pending`, `confirmed`, `rejected` or `superseded`.
    pub status: String,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub reject_reason: Option<String>,
    /// The baseline a confirmation wrote.
    pub baseline_revision_id: Option<i64>,
    /// The suggested model's baseline in force now, if it has one.
    pub current_baseline: Option<BaselineRevision>,
    /// The built-in catalogue's entry for the dataset's provider and model,
    /// in effect at the proposal's start.
    pub catalogue_entry: Option<CatalogueEntryView>,
    /// Whether the card equals the current baseline's: nothing to confirm.
    pub same_as_current: bool,
}

/// The built-in catalogue's entry for one model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogueEntryView {
    pub key: String,
    pub card: RateCard,
    pub source_url: String,
    pub as_of: NaiveDate,
}

impl CatalogueEntryView {
    fn of(entry: &CatalogueEntry) -> CatalogueEntryView {
        CatalogueEntryView {
            key: entry.key.clone(),
            card: entry.card.clone(),
            source_url: entry.source_url.clone(),
            as_of: entry.as_of,
        }
    }
}

/// The columns a [`Proposal`] is read from, over
/// `pricing_import_proposals p`.
const PROPOSAL_COLUMNS: &str = "p.id, p.batch_id, p.provider, p.model, p.effective_from, p.card, \
                                p.flags, p.pricing_url, p.prices_checked, p.suggested_model_id, \
                                p.status, p.decided_by, p.decided_at, p.reject_reason, \
                                p.baseline_revision_id";

impl<'r> sqlx::FromRow<'r, PgRow> for Proposal {
    /// The stored row; what it is compared to is added by [`read_proposals`].
    fn from_row(row: &'r PgRow) -> Result<Proposal, sqlx::Error> {
        let flags: Value = row.try_get("flags")?;
        Ok(Proposal {
            id: row.try_get("id")?,
            batch_id: row.try_get("batch_id")?,
            provider: row.try_get("provider")?,
            model: row.try_get("model")?,
            effective_from: row.try_get("effective_from")?,
            card: row.try_get("card")?,
            // A stored flag this build does not know reads as none, so the
            // card still shows.
            flags: serde_json::from_value(flags).unwrap_or_default(),
            pricing_url: row.try_get("pricing_url")?,
            prices_checked: row.try_get("prices_checked")?,
            suggested_model_id: row.try_get("suggested_model_id")?,
            status: row.try_get("status")?,
            decided_by: row.try_get("decided_by")?,
            decided_at: row.try_get("decided_at")?,
            reject_reason: row.try_get("reject_reason")?,
            baseline_revision_id: row.try_get("baseline_revision_id")?,
            current_baseline: None,
            catalogue_entry: None,
            same_as_current: false,
        })
    }
}

/// Reads the stored proposals the query `build` writes (selecting
/// [`PROPOSAL_COLUMNS`]), and adds to each what the review shows it against:
/// its suggested model's baseline in force, whether the card equals that
/// baseline's, and the built-in catalogue's entry for the dataset's provider
/// and model at the proposal's own start. On `conn`, so every proposal of one
/// read sees one `now()`.
async fn read_proposals(
    conn: &mut PgConnection,
    build: impl FnOnce(&mut QueryBuilder<'_, Postgres>),
    catalogue: &aisix_pricing::Catalogue,
) -> Result<Vec<Proposal>, DbError> {
    let mut sql = QueryBuilder::<Postgres>::new("");
    build(&mut sql);
    let mut proposals: Vec<Proposal> = sql.build_query_as().fetch_all(&mut *conn).await?;
    let models: Vec<Uuid> = proposals
        .iter()
        .filter_map(|p| p.suggested_model_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *conn)
        .await?;
    let current: HashMap<Uuid, BaselineRevision> =
        baselines::current_baselines(&mut *conn, models.as_slice())
            .await?
            .into_iter()
            .map(|r| (r.model_id, r))
            .collect();
    let card = |v: &Value| serde_json::from_value::<RateCard>(v.clone()).ok();
    for proposal in &mut proposals {
        proposal.catalogue_entry = catalogue
            .get(
                &proposal.provider,
                &proposal.model,
                proposal.effective_from.unwrap_or(now),
            )
            .map(CatalogueEntryView::of);
        let baseline = proposal.suggested_model_id.and_then(|m| current.get(&m));
        // A stored card this engine cannot read equals nothing.
        proposal.same_as_current = match (baseline, proposal.card.as_ref()) {
            (Some(baseline), Some(proposed)) => {
                card(&baseline.card).is_some_and(|b| card(proposed) == Some(b))
            }
            _ => false,
        };
        proposal.current_baseline = baseline.cloned();
    }
    Ok(proposals)
}

/// `POST /admin/pricing/imports`: one upload of the dataset.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadBody {
    /// The dataset's revision: a tag, a commit or a file name.
    #[serde(default)]
    dataset_ref: Option<String>,
    /// The dataset itself, read as its own text: every price is a number
    /// whose decimal text is the price, and a binary float would lose it.
    #[serde(default)]
    data: Option<Box<RawValue>>,
}

/// Whether `dataset_ref` is `^[A-Za-z0-9._/-]{1,100}$`.
fn valid_dataset_ref(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 100
        && reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

/// The live direct model whose document's `provider` and `model_name` are
/// `provider` and `model`, for each pair given: the smallest id of several.
///
/// It keys on the document's own upstream fields, so a routing, ensemble or
/// semantic model (which carry neither) never matches, and an embedding
/// model, which is a direct model, can.
async fn suggest_models(
    conn: &mut PgConnection,
    proposals: &[ParsedProposal],
) -> Result<HashMap<(String, String), Uuid>, DbError> {
    let providers: Vec<String> = proposals
        .iter()
        .map(|p| p.provider.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let models: Vec<String> = proposals
        .iter()
        .map(|p| p.model.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let rows: Vec<(String, String, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT ON (doc->>'provider', doc->>'model_name') \
                doc->>'provider', doc->>'model_name', id \
         FROM gw_resources \
         WHERE kind = $1 AND NOT deleted AND doc ? 'provider' AND doc ? 'model_name' \
           AND doc->>'provider' = ANY($2) AND doc->>'model_name' = ANY($3) \
         ORDER BY doc->>'provider', doc->>'model_name', id",
    )
    .bind(Kind::Models.segment())
    .bind(&providers)
    .bind(&models)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(provider, model, id)| ((provider, model), id))
        .collect())
}

/// `POST /admin/pricing/imports` → `201 ImportBatch`. The body is
/// `{dataset_ref, data}`, `data` being the dataset file itself.
///
/// Refused with `422`: a `dataset_ref` outside `^[A-Za-z0-9._/-]{1,100}$`
/// (`/dataset_ref`), and a body that is not a dataset, or makes more
/// proposals than [`MAX_IMPORT_PROPOSALS`], (`/data`, see [`parse`]).
///
/// One transaction supersedes every open batch and every pending proposal of
/// an older one, writes this batch and its proposals with the model each
/// suggests, and records `pricing.import.upload`. Nothing here is a price.
pub async fn upload(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<UploadBody>,
) -> Result<(StatusCode, Json<ImportBatch>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let dataset_ref = body
        .dataset_ref
        .filter(|r| valid_dataset_ref(r))
        .ok_or_else(|| {
            refused(
                "/dataset_ref",
                "数据集版本只能包含字母、数字和 . _ / -，最长 100 个字符",
            )
        })?;
    let data = body
        .data
        .ok_or_else(|| refused("/data", "不是可识别的价格数据集"))?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(state.db.pool())
        .await
        .map_err(DbError::from)?;
    // Up to `MAX_IMPORT_BYTES` of JSON: parsed off the request workers.
    let parsed = tokio::task::spawn_blocking(move || parse(&data, now))
        .await
        .map_err(ApiError::internal)??;
    let batch = state
        .db
        .tx(move |conn| {
            async move {
                // Uploads one at a time: two at once would each supersede
                // only what the other had committed, leaving two open
                // batches. The mode conflicts with itself and with any
                // write of a batch, and with no read.
                sqlx::query("LOCK TABLE pricing_import_batches IN SHARE ROW EXCLUSIVE MODE")
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                // Every open batch, and with it every pending proposal of an
                // older one: a newer upload is what an operator reviews now.
                let superseded: u64 = sqlx::query(
                    "WITH older AS (
                       UPDATE pricing_import_batches SET status = 'superseded'
                       WHERE status = 'open' RETURNING id)
                     UPDATE pricing_import_proposals SET status = 'superseded'
                     WHERE status = 'pending' AND batch_id IN (SELECT id FROM older)",
                )
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?
                .rows_affected();
                let batch: ImportBatch = sqlx::query_as(&format!(
                    "WITH b AS (
                       INSERT INTO pricing_import_batches
                       (id, dataset, dataset_ref, status, proposals, flagged, created_by)
                       VALUES ($1, $2, $3, 'open', $4, $5, $6) RETURNING *)
                     SELECT {BATCH_COLUMNS} FROM b JOIN operators o ON o.id = b.created_by"
                ))
                .bind(Uuid::new_v4())
                .bind(DATASET)
                .bind(&dataset_ref)
                .bind(parsed.len() as i64)
                .bind(parsed.iter().filter(|p| !p.flags.is_empty()).count() as i64)
                .bind(auth.id)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let suggested = suggest_models(&mut *conn, &parsed).await?;
                for chunk in parsed.chunks(PROPOSALS_PER_INSERT) {
                    let mut sql = QueryBuilder::<Postgres>::new(
                        "INSERT INTO pricing_import_proposals \
                         (id, batch_id, provider, model, effective_from, card, flags, pricing_url, \
                          prices_checked, suggested_model_id, status) ",
                    );
                    sql.push_values(chunk, |mut row, p| {
                        row.push_bind(Uuid::new_v4())
                            .push_bind(batch.id)
                            .push_bind(p.provider.clone())
                            .push_bind(p.model.clone())
                            .push_bind(p.effective_from)
                            .push_bind(p.card.as_ref().map(JsonColumn))
                            .push_bind(JsonColumn(p.flags.clone()))
                            .push_bind(p.pricing_url.clone())
                            .push_bind(p.prices_checked)
                            .push_bind(
                                suggested
                                    .get(&(p.provider.clone(), p.model.clone()))
                                    .copied(),
                            )
                            .push_bind("pending");
                    });
                    sql.build()
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                }
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "pricing.import.upload",
                        target_kind: "pricing_import_batch",
                        target_id: Some(batch.id.to_string()),
                        tenant_id: None,
                        detail: serde_json::json!({
                            "dataset": DATASET,
                            "dataset_ref": dataset_ref,
                            "proposals": batch.proposals,
                            "flagged": batch.flagged,
                            "superseded_proposals": superseded,
                        }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(batch)
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(batch)))
}

/// `GET /admin/pricing/imports` → `{items: [ImportBatch]}`: the newest 20
/// uploads, superseded ones included, newest first.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let items: Vec<ImportBatch> = sqlx::query_as(&format!(
        "SELECT {BATCH_COLUMNS} FROM pricing_import_batches b \
         JOIN operators o ON o.id = b.created_by \
         ORDER BY b.seq DESC LIMIT $1"
    ))
    .bind(LISTED_BATCHES as i64)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    Ok(Json(serde_json::json!({ "items": items })))
}

/// The most proposals one `INSERT` writes: 11 columns each, so 1,000 rows
/// stay far below Postgres's 65,535 bound parameters.
const PROPOSALS_PER_INSERT: usize = 1_000;

/// `GET /admin/pricing/imports/{id}?all=` → one `ImportBatch` with its
/// `proposals`, oldest first. With `all=true` a pending proposal whose card
/// equals the suggested model's current baseline is listed too; by default
/// it is left out, as there is nothing to confirm.
///
/// A `404` for an id that is not a batch's.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailQuery {
    #[serde(default)]
    all: Option<bool>,
}

pub async fn detail(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<DetailQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let catalogue = state.prices.catalogue().clone();
    let all = q.all.unwrap_or(false);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let batch: Option<ImportBatch> = sqlx::query_as(&format!(
                    "SELECT {BATCH_COLUMNS} FROM pricing_import_batches b \
                     JOIN operators o ON o.id = b.created_by WHERE b.id = $1"
                ))
                .bind(id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let Some(batch) = batch else {
                    return Ok::<_, ApiError>(None);
                };
                let proposals = read_proposals(
                    &mut *conn,
                    |sql| {
                        sql.push(format!(
                            "SELECT {PROPOSAL_COLUMNS} FROM pricing_import_proposals p \
                             WHERE p.batch_id = "
                        ))
                        .push_bind(id)
                        .push(" ORDER BY p.effective_from NULLS FIRST, p.provider, p.model, p.id");
                    },
                    &catalogue,
                )
                .await?;
                let mut view = serde_json::to_value(&batch).map_err(ApiError::internal)?;
                let listed: Vec<Proposal> = proposals
                    .into_iter()
                    .filter(|p| all || p.status != "pending" || !p.same_as_current)
                    .collect();
                view["proposals"] = serde_json::to_value(listed).map_err(ApiError::internal)?;
                Ok(Some(view))
            }
            .boxed()
        })
        .await?;
    view.map(Json).ok_or_else(ApiError::not_found)
}

/// The most characters a rejection's reason holds.
const MAX_REASON_CHARS: usize = 500;

/// A proposal of batch `batch_id`, read `FOR UPDATE` with its batch's
/// `dataset_ref`: everything a decision needs of the row itself.
struct LockedProposal {
    provider: String,
    model: String,
    effective_from: Option<DateTime<Utc>>,
    card: Option<Value>,
    flags: Value,
    pricing_url: Option<String>,
    prices_checked: Option<NaiveDate>,
    status: String,
    dataset_ref: String,
}

impl<'r> sqlx::FromRow<'r, PgRow> for LockedProposal {
    fn from_row(row: &'r PgRow) -> Result<LockedProposal, sqlx::Error> {
        Ok(LockedProposal {
            provider: row.try_get("provider")?,
            model: row.try_get("model")?,
            effective_from: row.try_get("effective_from")?,
            card: row.try_get("card")?,
            flags: row.try_get("flags")?,
            pricing_url: row.try_get("pricing_url")?,
            prices_checked: row.try_get("prices_checked")?,
            status: row.try_get("status")?,
            dataset_ref: row.try_get("dataset_ref")?,
        })
    }
}

/// The proposal `pid` of batch `id`, locked for decision; `404` for a
/// proposal of another batch, and for one that does not exist.
async fn lock_proposal(
    conn: &mut PgConnection,
    id: Uuid,
    pid: Uuid,
) -> Result<LockedProposal, ApiError> {
    let proposal: Option<LockedProposal> = sqlx::query_as(
        "SELECT p.id, p.provider, p.model, p.effective_from, p.card, p.flags, p.pricing_url, \
                p.prices_checked, p.status, b.dataset_ref \
         FROM pricing_import_proposals p JOIN pricing_import_batches b ON b.id = p.batch_id \
         WHERE p.id = $1 AND p.batch_id = $2 \
         FOR UPDATE OF p",
    )
    .bind(pid)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    proposal.ok_or_else(ApiError::not_found)
}

/// Whether `proposal` may be decided at all: only while it is `pending`. A
/// superseded one is not, and neither is one already confirmed or rejected.
fn check_pending(proposal: &LockedProposal) -> Result<(), ApiError> {
    if proposal.status == "pending" {
        Ok(())
    } else {
        Err(ApiError::proposal_state())
    }
}

/// Whether `proposal` carries a card this engine can read and no flag at
/// all: the only kind a confirmation may write. A stored flag this build
/// does not know still counts, since the column holds whatever flagged it.
fn check_confirmable(proposal: &LockedProposal) -> Result<RateCard, ApiError> {
    let flagged = match &proposal.flags {
        Value::Array(flags) => !flags.is_empty(),
        other => !other.is_null(),
    };
    let card = proposal
        .card
        .as_ref()
        .and_then(|c| serde_json::from_value::<RateCard>(c.clone()).ok());
    match card {
        Some(card) if !flagged => Ok(card),
        _ => Err(refused("/flags", "该提议含无法自动换算的价格，请手动录入")),
    }
}

/// `POST /admin/pricing/imports/{id}/proposals/{pid}/confirm`, body
/// `{model_id, effective_from, attested}` → `201 BaselineRevision`.
///
/// Refused with `422`: an `attested` that is not `true` (`/attested`, "请
/// 确认已对照官方价格页核对"), a proposal that carries any flag or no card
/// (`/flags`), and an `effective_from` before the proposal's own
/// (`/effective_from`). Refused with `409 proposal_state` unless the proposal
/// is still `pending`; a superseded one counts as not pending. `404` for a
/// `pid` outside batch `{id}`.
///
/// One transaction: the proposal row is taken `FOR UPDATE`, so of two
/// decisions on it exactly one commits, and the other answers
/// `409 proposal_state`. [`baselines::insert_baseline`] writes the revision
/// labelled third-party, carrying the batch's `dataset_ref`, the proposal's
/// own page and checked date, and this operator's attestation at `now()`; the
/// proposal becomes `confirmed` and names the revision. The confirmation's
/// audit row is `pricing.import.confirm`, beside the baseline's own
/// `pricing.baseline.create`.
pub async fn confirm(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, pid)): Path<(String, String)>,
    ApiJson(body): ApiJson<ConfirmBody>,
) -> Result<(StatusCode, Json<BaselineRevision>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    if body.attested != Some(true) {
        return Err(refused("/attested", "请确认已对照官方价格页核对"));
    }
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let pid = Uuid::parse_str(&pid).map_err(|_| ApiError::not_found())?;
    let revision = state
        .db
        .tx(move |conn| {
            async move {
                let proposal = lock_proposal(&mut *conn, id, pid).await?;
                check_pending(&proposal)?;
                let card = check_confirmable(&proposal)?;
                if proposal
                    .effective_from
                    .is_some_and(|starts| body.effective_from < starts)
                {
                    return Err(refused(
                        "/effective_from",
                        "生效时间不能早于该提议自己的生效时间",
                    ));
                }
                let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                let row = super::baselines::NewBaseline {
                    model_id: body.model_id,
                    effective_from: body.effective_from,
                    card,
                    source: super::baselines::BaselineSource::ThirdParty {
                        dataset: DATASET.to_string(),
                        dataset_ref: proposal.dataset_ref.clone(),
                        pricing_url: proposal.pricing_url.clone(),
                        prices_checked: proposal.prices_checked,
                        import_id: id,
                        proposal_id: pid,
                        attested_by: auth.id,
                        attested_at: now,
                    },
                };
                let (revision, _) =
                    super::baselines::insert_baseline(&mut *conn, &row, Actor::Operator(&auth))
                        .await?;
                sqlx::query(
                    "UPDATE pricing_import_proposals \
                     SET status = 'confirmed', decided_by = $1, decided_at = $2, \
                         baseline_revision_id = $3 \
                     WHERE id = $4",
                )
                .bind(auth.id)
                .bind(now)
                .bind(revision.revision_id)
                .bind(pid)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "pricing.import.confirm",
                        target_kind: "pricing_import_proposal",
                        target_id: Some(pid.to_string()),
                        tenant_id: None,
                        detail: serde_json::json!({
                            "proposal_id": pid,
                            "model_id": body.model_id,
                            "attested": true,
                        }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(revision)
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(revision)))
}

/// `POST /admin/pricing/imports/{id}/proposals/{pid}/confirm`'s body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmBody {
    /// The model the baseline prices.
    model_id: Uuid,
    /// When it starts; no earlier than the proposal's own date.
    effective_from: DateTime<Utc>,
    /// The operator's attestation that the price was checked against the
    /// provider's page. Must be `true`.
    #[serde(default)]
    attested: Option<bool>,
}

/// `POST /admin/pricing/imports/{id}/proposals/{pid}/reject`, body
/// `{reason}` → `200 Proposal`.
///
/// The reason is 1 to 500 characters (`/reason`). The same `409
/// proposal_state` as a confirmation, and the same `404`; the row is taken
/// `FOR UPDATE`, so a decision made meanwhile is refused rather than
/// overwritten. Nothing is written but the decision: the proposal becomes
/// `rejected` with the reason, and the audit row is `pricing.import.reject`.
pub async fn reject(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, pid)): Path<(String, String)>,
    ApiJson(body): ApiJson<RejectBody>,
) -> Result<Json<Proposal>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if reason.is_empty() {
        return Err(refused("/reason", "请填写拒绝原因"));
    }
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(refused("/reason", "拒绝原因最多 500 个字符"));
    }
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let pid = Uuid::parse_str(&pid).map_err(|_| ApiError::not_found())?;
    let catalogue = state.prices.catalogue().clone();
    let proposal = state
        .db
        .tx(move |conn| {
            async move {
                let proposal = lock_proposal(&mut *conn, id, pid).await?;
                check_pending(&proposal)?;
                sqlx::query(
                    "UPDATE pricing_import_proposals \
                     SET status = 'rejected', reject_reason = $1, decided_by = $2, \
                         decided_at = now() \
                     WHERE id = $3",
                )
                .bind(&reason)
                .bind(auth.id)
                .bind(pid)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "pricing.import.reject",
                        target_kind: "pricing_import_proposal",
                        target_id: Some(pid.to_string()),
                        tenant_id: None,
                        detail: serde_json::json!({
                            "proposal_id": pid,
                            "provider": proposal.provider,
                            "model": proposal.model,
                        }),
                    },
                )
                .await?;
                // The decision, in the shape the review page lists it in.
                read_proposals(
                    &mut *conn,
                    |sql| {
                        sql.push(format!(
                            "SELECT {PROPOSAL_COLUMNS} FROM pricing_import_proposals p \
                             WHERE p.id = "
                        ))
                        .push_bind(pid);
                    },
                    &catalogue,
                )
                .await?
                .pop()
                .ok_or_else(|| ApiError::internal("the proposal just decided is gone"))
            }
            .boxed()
        })
        .await?;
    Ok(Json(proposal))
}

/// `POST /admin/pricing/imports/{id}/proposals/{pid}/reject`'s body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectBody {
    /// 1 to 500 characters, stored as given.
    #[serde(default)]
    reason: Option<String>,
}

/// A price map: each price field to its value, kept as the text the file
/// wrote.
type PriceMap<'a> = BTreeMap<String, &'a RawValue>;

#[derive(Deserialize)]
struct Provider<'a> {
    id: String,
    #[serde(default)]
    pricing_urls: Vec<String>,
    #[serde(borrow)]
    models: Vec<Model<'a>>,
}

#[derive(Deserialize)]
struct Model<'a> {
    id: String,
    /// A price map, or a list of conditional ones ([`conditionals`]).
    #[serde(borrow)]
    prices: &'a RawValue,
    #[serde(default, borrow)]
    price_variants: Option<Vec<Variant<'a>>>,
    #[serde(default)]
    prices_checked: Option<NaiveDate>,
}

/// One entry of a model's list of prices.
#[derive(Deserialize)]
struct Conditional<'a> {
    #[serde(default, borrow)]
    constraint: Option<PriceMap<'a>>,
    #[serde(borrow)]
    prices: PriceMap<'a>,
}

#[derive(Deserialize)]
struct Variant<'a> {
    when: BTreeMap<String, String>,
    #[serde(borrow)]
    prices: PriceMap<'a>,
    #[serde(default, borrow)]
    constraint: Option<PriceMap<'a>>,
}

/// When an entry or a variant applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Condition {
    Always,
    /// From this UTC day on.
    From(NaiveDate),
    TimeOfDay,
    /// A constraint of no shape the dataset documents.
    Other,
}

impl Condition {
    fn of(constraint: Option<&PriceMap<'_>>) -> Condition {
        let Some(c) = constraint else {
            return Condition::Always;
        };
        if c.contains_key("start_time") || c.contains_key("end_time") {
            return Condition::TimeOfDay;
        }
        match (c.len(), c.get("start_date")) {
            (1, Some(day)) => {
                serde_json::from_str(day.get()).map_or(Condition::Other, Condition::From)
            }
            _ => Condition::Other,
        }
    }

    /// Whether it applies on `day`. A time window or an unknown condition
    /// never does: the proposal is flagged instead.
    fn active(self, day: NaiveDate) -> bool {
        match self {
            Condition::Always => true,
            Condition::From(from) => from <= day,
            Condition::TimeOfDay | Condition::Other => false,
        }
    }
}

/// A model's prices as a list, a single map being a list of one entry with
/// no constraint. `None` for anything else, or an empty list.
fn conditionals(prices: &RawValue) -> Option<Vec<Conditional<'_>>> {
    let text = prices.get();
    if text.starts_with('[') {
        let list: Vec<Conditional<'_>> = serde_json::from_str(text).ok()?;
        return (!list.is_empty()).then_some(list);
    }
    let prices = serde_json::from_str(text).ok()?;
    Some(vec![Conditional {
        constraint: None,
        prices,
    }])
}

/// One proposal of a model: its start (`None` at upload), card and flags.
type Dated = (Option<DateTime<Utc>>, Option<RateCard>, Vec<Flag>);

/// One model's proposals: for the moment of upload and for each later date.
fn model_proposals(
    entries: &[Conditional<'_>],
    variants: &[Variant<'_>],
    today: NaiveDate,
) -> Vec<Dated> {
    let entry_conditions: Vec<Condition> = entries
        .iter()
        .map(|e| Condition::of(e.constraint.as_ref()))
        .collect();
    let variant_conditions: Vec<Condition> = variants
        .iter()
        .map(|v| Condition::of(v.constraint.as_ref()))
        .collect();
    let mut model_flags = BTreeSet::new();
    let mut later = BTreeSet::new();
    for condition in entry_conditions.iter().chain(&variant_conditions) {
        match condition {
            Condition::TimeOfDay => {
                model_flags.insert(Flag::TimeOfDay);
            }
            Condition::Other => {
                model_flags.insert(Flag::UnsupportedVariant);
            }
            Condition::From(day) if *day > today => {
                later.insert(*day);
            }
            Condition::Always | Condition::From(_) => {}
        }
    }
    std::iter::once(None)
        .chain(later.into_iter().map(Some))
        .map(|from| {
            let day = from.unwrap_or(today);
            let mut flags = model_flags.clone();
            // The last entry in effect, else the first (there is one).
            let standard = entries
                .iter()
                .zip(&entry_conditions)
                .rev()
                .find(|(_, c)| c.active(day))
                .map_or(&entries[0].prices, |(e, _)| &e.prices);
            let mut overrides: BTreeMap<ServiceTier, PriceMap<'_>> = BTreeMap::new();
            for (variant, condition) in variants.iter().zip(&variant_conditions) {
                if !condition.active(day) {
                    continue;
                }
                let Some(tier) = variant_tier(&variant.when) else {
                    flags.insert(Flag::UnsupportedVariant);
                    continue;
                };
                let prices = overrides.entry(tier).or_default();
                for (field, value) in &variant.prices {
                    prices.entry(field.clone()).or_insert(value);
                }
            }
            let mut tiers = BTreeMap::from([(ServiceTier::default_tier(), standard.clone())]);
            for (tier, prices) in overrides {
                if tier == ServiceTier::default_tier() {
                    // The standard price is what a request naming no tier
                    // pays; a card cannot price the named default apart.
                    let differs = prices
                        .iter()
                        .any(|(f, v)| standard.get(f).map(|s| s.get()) != Some(v.get()));
                    if differs {
                        flags.insert(Flag::UnsupportedVariant);
                    }
                    continue;
                }
                let mut resolved = standard.clone();
                resolved.extend(prices);
                tiers.insert(tier, resolved);
            }
            let card = card(&tiers, &mut flags);
            (
                from.map(|d| d.and_time(NaiveTime::MIN).and_utc()),
                card,
                flags.into_iter().collect(),
            )
        })
        .collect()
}

/// The tier a variant's condition selects, or `None` when a card cannot
/// express it.
fn variant_tier(when: &BTreeMap<String, String>) -> Option<ServiceTier> {
    let mut conditions = when.iter();
    match (conditions.next(), conditions.next()) {
        (Some((key, value)), None) if key == "processing" && value == "batch" => {
            Some(ServiceTier::batch())
        }
        (Some((key, name)), None) if key == "service_tier" => tier_named(name),
        _ => None,
    }
}

/// `name` as a service tier, when the rate model accepts it as one: checked
/// by [`RateCard::validate`] itself, so the rule is the engine's and never a
/// copy of it.
fn tier_named(name: &str) -> Option<ServiceTier> {
    let tier: ServiceTier = serde_json::from_value(Value::String(name.to_owned())).ok()?;
    let flat = |r| BTreeMap::from([(Dimension::Input, r), (Dimension::Output, r)]);
    let rates = TierRates {
        base: rates(&flat(Rate(0)))?,
        long_context: Vec::new(),
    };
    let probe = RateCard(BTreeMap::from([
        (ServiceTier::default_tier(), rates.clone()),
        (tier.clone(), rates),
    ]));
    probe.validate().ok().map(|()| tier)
}

/// The card dimension a dataset price field fills, and what its number is
/// multiplied by to be a price per million units.
fn dimension(field: &str) -> Option<(Dimension, u64)> {
    Some(match field {
        "input_mtok" => (Dimension::Input, 1),
        "output_mtok" => (Dimension::Output, 1),
        "cache_read_mtok" => (Dimension::CacheRead, 1),
        "cache_write_mtok" => (Dimension::CacheWrite, 1),
        "cache_write_1h_mtok" => (Dimension::CacheWrite1h, 1),
        "input_audio_mtok" => (Dimension::AudioInput, 1),
        "output_audio_mtok" => (Dimension::AudioOutput, 1),
        "web_searches_kcount" => (Dimension::WebSearch, 1_000),
        _ => return None,
    })
}

/// One price field's value.
enum Price {
    Flat(Rate),
    /// The base, and each tier's start and price in rising order of start.
    Tiered(Rate, Vec<(u64, Rate)>),
}

impl Price {
    /// The value's text, multiplied by `per`. `None` for anything that is not
    /// a number [`Rate::parse`] reads (digits, then at most six decimals) or
    /// a tiered price of such numbers with whole-number starts.
    fn read(value: &RawValue, per: u64) -> Option<Price> {
        #[derive(Deserialize)]
        struct Tiered<'a> {
            #[serde(borrow)]
            base: &'a RawValue,
            #[serde(borrow)]
            tiers: Vec<Tier<'a>>,
        }
        #[derive(Deserialize)]
        struct Tier<'a> {
            #[serde(borrow)]
            start: &'a RawValue,
            #[serde(borrow)]
            price: &'a RawValue,
        }
        let text = value.get();
        if !text.starts_with('{') {
            return rate(text, per).map(Price::Flat);
        }
        let tiered: Tiered<'_> = serde_json::from_str(text).ok()?;
        let mut tiers = tiered
            .tiers
            .iter()
            .map(|t| Some((start(t.start.get())?, rate(t.price.get(), per)?)))
            .collect::<Option<Vec<_>>>()?;
        // Stable, so of two tiers with one start the later one wins, as in
        // the dataset's own resolution.
        tiers.sort_by_key(|(start, _)| *start);
        Some(Price::Tiered(rate(tiered.base.get(), per)?, tiers))
    }

    fn starts(&self) -> impl Iterator<Item = u64> + '_ {
        let tiers = match self {
            Price::Flat(_) => &[][..],
            Price::Tiered(_, tiers) => &tiers[..],
        };
        tiers.iter().map(|(start, _)| *start)
    }

    /// The price in the band above `threshold` input tokens, or the base
    /// for `None`.
    fn in_band(&self, threshold: Option<u64>) -> Rate {
        match (self, threshold) {
            (Price::Flat(rate), _) | (Price::Tiered(rate, _), None) => *rate,
            (Price::Tiered(base, tiers), Some(threshold)) => tiers
                .iter()
                .rev()
                .find(|(start, _)| *start <= threshold)
                .map_or(*base, |(_, rate)| *rate),
        }
    }
}

/// `text` as a rate, times `per`; `None` past [`Rate::MAX`].
fn rate(text: &str, per: u64) -> Option<Rate> {
    Rate::parse(text)
        .ok()?
        .0
        .checked_mul(per)
        .filter(|r| *r <= Rate::MAX.0)
        .map(Rate)
}

/// A tier's start: digits only.
fn start(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The rates of one band or base; `None` without an input or an output
/// price, which every card needs.
fn rates(prices: &BTreeMap<Dimension, Rate>) -> Option<Rates> {
    let get = |d| prices.get(&d).copied();
    Some(Rates {
        input: get(Dimension::Input)?,
        output: get(Dimension::Output)?,
        cache_read: get(Dimension::CacheRead),
        cache_write: get(Dimension::CacheWrite),
        cache_write_1h: get(Dimension::CacheWrite1h),
        reasoning: None,
        audio_input: get(Dimension::AudioInput),
        audio_output: get(Dimension::AudioOutput),
        image_input: None,
        image_output: None,
        web_search: get(Dimension::WebSearch),
    })
}

/// The card `tiers` (each tier's resolved price map) come to, adding to
/// `flags` what keeps them from one.
fn card(
    tiers: &BTreeMap<ServiceTier, PriceMap<'_>>,
    flags: &mut BTreeSet<Flag>,
) -> Option<RateCard> {
    let mut priced: BTreeMap<&ServiceTier, BTreeMap<Dimension, Price>> = BTreeMap::new();
    for (tier, fields) in tiers {
        let prices = priced.entry(tier).or_default();
        for (field, value) in fields {
            let Some((dimension, per)) = dimension(field) else {
                flags.insert(Flag::UnsupportedField);
                continue;
            };
            match Price::read(value, per) {
                Some(price) => {
                    prices.insert(dimension, price);
                }
                None => {
                    flags.insert(Flag::BadNumber);
                }
            }
        }
    }
    if flags.contains(&Flag::BadNumber) {
        return None;
    }
    let thresholds: BTreeSet<u64> = priced
        .values()
        .flat_map(|prices| prices.values().flat_map(Price::starts))
        .collect();
    let card = priced
        .into_iter()
        .map(|(tier, prices)| {
            let at = |threshold: Option<u64>| {
                rates(
                    &prices
                        .iter()
                        .map(|(d, p)| (*d, p.in_band(threshold)))
                        .collect(),
                )
            };
            let long_context = thresholds
                .iter()
                .map(|t| {
                    Some(LongContextBand {
                        above_input_tokens: *t,
                        rates: at(Some(*t))?,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some((
                tier.clone(),
                TierRates {
                    base: at(None)?,
                    long_context,
                },
            ))
        })
        .collect::<Option<BTreeMap<_, _>>>()
        .map(RateCard);
    match card {
        Some(card) if card.validate().is_ok() => Some(card),
        _ => {
            flags.insert(Flag::InvalidCard);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    /// 2026-11-08 12:00 UTC.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 8, 12, 0, 0).unwrap()
    }

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_string()).expect("valid JSON")
    }

    /// The proposals of `text`, a whole dataset, at [`now`].
    fn proposals(text: &str) -> Vec<ParsedProposal> {
        parse(&raw(text), now()).expect("a recognisable dataset")
    }

    /// One provider `p` with the models `models` (JSON text).
    fn dataset(models: &str) -> String {
        format!(
            r#"[{{"id":"p","pricing_urls":["https://p.example/pricing"],"models":[{models}]}}]"#
        )
    }

    /// The proposal of model `id` in effect at upload.
    fn current<'a>(all: &'a [ParsedProposal], id: &str) -> &'a ParsedProposal {
        all.iter()
            .find(|p| p.model == id && p.effective_from.is_none())
            .unwrap_or_else(|| panic!("no proposal for {id} in {all:?}"))
    }

    fn card_json(p: &ParsedProposal) -> Value {
        serde_json::to_value(p.card.as_ref().expect("a card")).expect("a card serializes")
    }

    fn default_base(p: &ParsedProposal) -> &aisix_pricing::Rates {
        &p.card.as_ref().expect("a card").0[&ServiceTier::default_tier()].base
    }

    #[test]
    fn a_price_is_read_from_its_decimal_text() {
        let all = proposals(&dataset(
            r#"{"id":"m-plain","prices":{"input_mtok":3,"output_mtok": 0.075 }},
               {"id":"m-exponent","prices":{"input_mtok":1.5e-05,"output_mtok":1}},
               {"id":"m-seventh","prices":{"input_mtok":0.0000001,"output_mtok":1}},
               {"id":"m-negative","prices":{"input_mtok":-1,"output_mtok":1}},
               {"id":"m-float-noise","prices":{"input_mtok":0.18000000000000002,"output_mtok":1}},
               {"id":"m-string","prices":{"input_mtok":"3","output_mtok":1}},
               {"id":"m-tier-start","prices":{"input_mtok":{"base":1,"tiers":[{"start":2e5,"price":2}]},"output_mtok":1}}"#,
        ));
        let plain = current(&all, "m-plain");
        assert_eq!(default_base(plain).input, Rate(3_000_000));
        assert_eq!(default_base(plain).output, Rate(75_000));
        assert!(plain.flags.is_empty(), "{plain:?}");
        for id in [
            "m-exponent",
            "m-seventh",
            "m-negative",
            "m-float-noise",
            "m-string",
            "m-tier-start",
        ] {
            let p = current(&all, id);
            assert_eq!(p.card, None, "{id}: {p:?}");
            assert_eq!(p.flags, vec![Flag::BadNumber], "{id}");
        }
    }

    #[test]
    fn dataset_fields_map_to_card_dimensions() {
        let all = proposals(&dataset(
            r#"{"id":"m-all","prices":{"input_mtok":1,"output_mtok":2,"cache_read_mtok":0.1,
                 "cache_write_mtok":1.25,"cache_write_1h_mtok":2.5,"input_audio_mtok":3,
                 "output_audio_mtok":4,"web_searches_kcount":10}},
               {"id":"m-requests","prices":{"input_mtok":1,"output_mtok":2,"requests_kcount":5}}"#,
        ));
        assert_eq!(
            card_json(current(&all, "m-all")),
            json!({"default": {"base": {
                "input": "1", "output": "2", "cache_read": "0.1", "cache_write": "1.25",
                "cache_write_1h": "2.5", "audio_input": "3", "audio_output": "4",
                "web_search": "10000",
            }}})
        );
        assert!(current(&all, "m-all").flags.is_empty());
        let requests = current(&all, "m-requests");
        assert_eq!(requests.flags, vec![Flag::UnsupportedField]);
        // What does map is still shown, for the comparison.
        assert_eq!(
            card_json(requests),
            json!({"default": {"base": {"input": "1", "output": "2"}}})
        );
    }

    #[test]
    fn web_searches_priced_per_thousand_are_priced_per_million() {
        let all = proposals(&dataset(
            r#"{"id":"m-ten","prices":{"input_mtok":1,"output_mtok":2,"web_searches_kcount":10}},
               {"id":"m-small","prices":{"input_mtok":1,"output_mtok":2,"web_searches_kcount":0.000001}},
               {"id":"m-seventh","prices":{"input_mtok":1,"output_mtok":2,"web_searches_kcount":0.0000001}}"#,
        ));
        assert_eq!(
            default_base(current(&all, "m-ten")).web_search,
            Some(Rate::parse("10000").unwrap())
        );
        assert_eq!(
            default_base(current(&all, "m-small")).web_search,
            Some(Rate::parse("0.001").unwrap())
        );
        let seventh = current(&all, "m-seventh");
        assert_eq!(
            (seventh.card.as_ref(), &seventh.flags[..]),
            (None, &[Flag::BadNumber][..])
        );
    }

    #[test]
    fn tiers_become_long_context_bands_with_every_rate_the_base_carries() {
        let all = proposals(&dataset(
            r#"{"id":"m","prices":{
                 "input_mtok":{"base":3,"tiers":[{"start":200000,"price":6}]},
                 "output_mtok":{"base":15,"tiers":[{"start":200000,"price":22.5}]},
                 "cache_read_mtok":0.3}}"#,
        ));
        let p = current(&all, "m");
        assert!(p.flags.is_empty(), "{p:?}");
        assert_eq!(
            card_json(p),
            json!({"default": {
                "base": {"input": "3", "output": "15", "cache_read": "0.3"},
                "long_context": [{"above_input_tokens": 200000,
                                  "rates": {"input": "6", "output": "22.5", "cache_read": "0.3"}}],
            }})
        );
        p.card
            .as_ref()
            .unwrap()
            .validate()
            .expect("the card validates");
    }

    #[test]
    fn fields_tiered_at_different_starts_give_one_band_per_start() {
        let all = proposals(&dataset(
            r#"{"id":"m","prices":{
                 "input_mtok":{"base":1,"tiers":[{"start":128000,"price":2}]},
                 "output_mtok":{"base":4,"tiers":[{"start":200000,"price":9}]}}}"#,
        ));
        let p = current(&all, "m");
        assert!(p.flags.is_empty(), "{p:?}");
        assert_eq!(
            card_json(p),
            json!({"default": {
                "base": {"input": "1", "output": "4"},
                "long_context": [
                    {"above_input_tokens": 128000, "rates": {"input": "2", "output": "4"}},
                    {"above_input_tokens": 200000, "rates": {"input": "2", "output": "9"}},
                ],
            }})
        );
    }

    #[test]
    fn a_batch_variant_becomes_the_batch_tier_and_a_service_tier_variant_its_tier() {
        let all = proposals(&dataset(
            r#"{"id":"m","prices":{"input_mtok":4,"output_mtok":20,"web_searches_kcount":10},
                "price_variants":[
                  {"when":{"service_tier":"default"},"prices":{}},
                  {"when":{"processing":"batch"},"prices":{"input_mtok":2,"output_mtok":10}},
                  {"when":{"service_tier":"flex"},"prices":{"input_mtok":1,"output_mtok":5}}]},
               {"id":"m-alias","prices":{"input_mtok":4,"output_mtok":20},
                "price_variants":[{"when":{"service_tier":"standard"},"prices":{"input_mtok":3}}]},
               {"id":"m-default-differs","prices":{"input_mtok":4,"output_mtok":20},
                "price_variants":[{"when":{"service_tier":"default"},"prices":{"input_mtok":3}}]}"#,
        ));
        let p = current(&all, "m");
        assert!(p.flags.is_empty(), "{p:?}");
        // A variant overrides the prices it names; the rest are the
        // standard prices, as the dataset resolves a variant.
        assert_eq!(
            card_json(p),
            json!({
                "default": {"base": {"input": "4", "output": "20", "web_search": "10000"}},
                "batch": {"base": {"input": "2", "output": "10", "web_search": "10000"}},
                "flex": {"base": {"input": "1", "output": "5", "web_search": "10000"}},
            })
        );
        // `standard` is no tier name a served tier selects.
        let alias = current(&all, "m-alias");
        assert_eq!(alias.flags, vec![Flag::UnsupportedVariant]);
        assert_eq!(
            card_json(alias),
            json!({"default": {"base": {"input": "4", "output": "20"}}})
        );
        // A named default tier priced apart from requests that name none.
        let differs = current(&all, "m-default-differs");
        assert_eq!(differs.flags, vec![Flag::UnsupportedVariant]);
        assert_eq!(
            card_json(differs),
            json!({"default": {"base": {"input": "4", "output": "20"}}})
        );
    }

    #[test]
    fn other_conditions_and_time_windows_are_flagged() {
        let all = proposals(&dataset(
            r#"{"id":"m-region","prices":{"input_mtok":1,"output_mtok":2},
                "price_variants":[{"when":{"region":"eu"},"prices":{"input_mtok":3}}]},
               {"id":"m-two-keys","prices":{"input_mtok":1,"output_mtok":2},
                "price_variants":[{"when":{"processing":"batch","service_tier":"flex"},"prices":{"input_mtok":3}}]},
               {"id":"m-window","prices":[
                 {"prices":{"input_mtok":1,"output_mtok":2}},
                 {"constraint":{"start_time":"00:30:00Z","end_time":"16:30:00Z"},
                  "prices":{"input_mtok":2,"output_mtok":4}}]},
               {"id":"m-window-variant","prices":{"input_mtok":1,"output_mtok":2},
                "price_variants":[{"when":{"processing":"batch"},"prices":{"input_mtok":0.5},
                  "constraint":{"end_time":"08:00:00Z","start_time":"01:00:00Z"}}]}"#,
        ));
        for id in ["m-region", "m-two-keys"] {
            let p = current(&all, id);
            assert_eq!(p.flags, vec![Flag::UnsupportedVariant], "{id}");
            assert_eq!(
                card_json(p),
                json!({"default": {"base": {"input": "1", "output": "2"}}}),
                "{id}"
            );
        }
        for id in ["m-window", "m-window-variant"] {
            let p = current(&all, id);
            assert_eq!(p.flags, vec![Flag::TimeOfDay], "{id}");
            assert_eq!(
                card_json(p),
                json!({"default": {"base": {"input": "1", "output": "2"}}}),
                "{id}"
            );
        }
    }

    #[test]
    fn dated_prices_give_one_proposal_per_date_from_now_on() {
        let all = proposals(&dataset(
            r#"{"id":"m-list","prices":[
                 {"prices":{"input_mtok":1,"output_mtok":2}},
                 {"constraint":{"start_date":"2026-01-01"},"prices":{"input_mtok":3,"output_mtok":4}},
                 {"constraint":{"start_date":"2027-01-01"},"prices":{"input_mtok":5,"output_mtok":6}}]},
               {"id":"m-variants","prices":{"input_mtok":4,"output_mtok":20},
                "price_variants":[
                  {"when":{"processing":"batch"},"prices":{"input_mtok":1},
                   "constraint":{"start_date":"2027-03-01"}},
                  {"when":{"processing":"batch"},"prices":{"input_mtok":2,"output_mtok":10},
                   "constraint":{"start_date":"2026-10-14"}}]},
               {"id":"m-today","prices":[
                 {"prices":{"input_mtok":1,"output_mtok":2}},
                 {"constraint":{"start_date":"2026-11-08"},"prices":{"input_mtok":7,"output_mtok":8}}]}"#,
        ));
        let dates = |id: &str| -> Vec<Option<DateTime<Utc>>> {
            all.iter()
                .filter(|p| p.model == id)
                .map(|p| p.effective_from)
                .collect()
        };
        let midnight = |y, m, d| Some(Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap());

        assert_eq!(dates("m-list"), vec![None, midnight(2027, 1, 1)]);
        let list: Vec<&ParsedProposal> = all.iter().filter(|p| p.model == "m-list").collect();
        assert_eq!(
            card_json(list[0]),
            json!({"default": {"base": {"input": "3", "output": "4"}}})
        );
        assert_eq!(
            card_json(list[1]),
            json!({"default": {"base": {"input": "5", "output": "6"}}})
        );

        // A dated variant takes effect at its own date; until then the one
        // already in effect stands.
        assert_eq!(dates("m-variants"), vec![None, midnight(2027, 3, 1)]);
        let variants: Vec<&ParsedProposal> =
            all.iter().filter(|p| p.model == "m-variants").collect();
        assert_eq!(
            card_json(variants[0]),
            json!({
                "default": {"base": {"input": "4", "output": "20"}},
                "batch": {"base": {"input": "2", "output": "10"}},
            })
        );
        assert_eq!(
            card_json(variants[1]),
            json!({
                "default": {"base": {"input": "4", "output": "20"}},
                "batch": {"base": {"input": "1", "output": "10"}},
            })
        );

        // A date that is today is in effect at upload.
        assert_eq!(dates("m-today"), vec![None]);
        assert_eq!(
            card_json(current(&all, "m-today")),
            json!({"default": {"base": {"input": "7", "output": "8"}}})
        );
    }

    #[test]
    fn a_card_the_rate_model_refuses_is_kept_flagged_without_a_card() {
        let all = proposals(&dataset(
            r#"{"id":"m-input-only","prices":{"input_mtok":0.02}},
               {"id":"m-zero-start","prices":{"input_mtok":{"base":1,"tiers":[{"start":0,"price":2}]},"output_mtok":1}}"#,
        ));
        for id in ["m-input-only", "m-zero-start"] {
            let p = current(&all, id);
            assert_eq!(
                (p.card.as_ref(), &p.flags[..]),
                (None, &[Flag::InvalidCard][..]),
                "{id}"
            );
        }
    }

    #[test]
    fn provenance_is_the_providers_first_page_and_the_models_checked_date() {
        let all = proposals(
            r#"[{"id":"p","pricing_urls":["https://p.example/a","https://p.example/b"],"models":[
                  {"id":"m-checked","prices_checked":"2026-10-19","prices":{"input_mtok":1,"output_mtok":2}},
                  {"id":"m-unchecked","prices":{"input_mtok":1,"output_mtok":2}}]},
                {"id":"q","models":[{"id":"m-uncited","prices":{"input_mtok":1,"output_mtok":2}}]}]"#,
        );
        let checked = current(&all, "m-checked");
        assert_eq!(checked.provider, "p");
        assert_eq!(checked.pricing_url.as_deref(), Some("https://p.example/a"));
        assert_eq!(
            checked.prices_checked,
            Some(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap())
        );
        assert_eq!(current(&all, "m-unchecked").prices_checked, None);
        let uncited = current(&all, "m-uncited");
        assert_eq!(
            (uncited.provider.as_str(), uncited.pricing_url.as_deref()),
            ("q", None)
        );
    }

    #[test]
    fn a_body_that_is_not_a_dataset_is_refused_at_data() {
        let too_many = dataset(
            &(0..=MAX_IMPORT_PROPOSALS)
                .map(|i| format!(r#"{{"id":"m-{i}","prices":{{"input_mtok":1,"output_mtok":2}}}}"#))
                .collect::<Vec<_>>()
                .join(","),
        );
        for text in [
            r#"{"a":1}"#,
            "[]",
            "[1]",
            r#"[{"id":"p"}]"#,
            r#"[{"id":"p","models":[{"id":"m","prices":null}]}]"#,
            too_many.as_str(),
        ] {
            let err = parse(&raw(text), now()).expect_err("refused");
            assert_eq!(err.status.as_u16(), 422, "{err:?}");
            assert_eq!(
                err.details
                    .iter()
                    .map(|d| d.path.as_str())
                    .collect::<Vec<_>>(),
                vec!["/data"],
                "{err:?}"
            );
        }
        // Exactly the limit is accepted.
        let at_limit = dataset(
            &(0..MAX_IMPORT_PROPOSALS)
                .map(|i| format!(r#"{{"id":"m-{i}","prices":{{"input_mtok":1,"output_mtok":2}}}}"#))
                .collect::<Vec<_>>()
                .join(","),
        );
        assert_eq!(proposals(&at_limit).len(), MAX_IMPORT_PROPOSALS);
    }
}
