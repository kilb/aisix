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
//! Two rules bound what a card may be made of. A price field carries at most
//! [`MAX_TIERS_PER_FIELD`] tiers, since a card's bands are the union of every
//! field's tier starts; a field over the cap is left off and flagged
//! `unsupported_field`. And a map with no output-side price is no card
//! (`invalid_card`), because a chat model with a missing output price is a
//! gap in the dataset rather than a free completion; the one exception is a
//! model in [`FREE_OUTPUT`] whose `price_comments` say in so many words that
//! its output is free, which becomes `output: 0`.
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

/// The most tiers one price field may carry. A card's bands are the union of
/// every field's tier starts, so a field's cost grows with its tier count;
/// the real dataset's largest is 2, and a body up to [`MAX_IMPORT_BYTES`]
/// could otherwise ask for hundreds of thousands of bands in one card.
pub const MAX_TIERS_PER_FIELD: usize = 64;

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
    /// The model's display name in the dataset, if it gives one.
    pub name: Option<String>,
    /// The other names the dataset's own `match` says the model is called by
    /// exactly ([`aliases`]), never the id itself.
    pub aliases: Vec<String>,
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
        // The page is shown as a link beside the attestation. Anything but a
        // page on the web is dropped rather than stored: a `javascript:` or
        // `data:` URL there would run in the operator's session.
        let pricing_url = provider
            .pricing_urls
            .iter()
            .find(|url| is_web_page(url))
            .cloned();
        for model in &provider.models {
            let entries = conditionals(model.prices).ok_or_else(unrecognised)?;
            let variants = model.price_variants.as_deref().unwrap_or_default();
            let comments = model.price_comments.as_deref();
            let entry = (provider.id.as_str(), model.id.as_str());
            let aliases = aliases(&model.id, model.matches.as_ref());
            for (effective_from, card, flags) in
                model_proposals(&entries, variants, today, entry, comments)
            {
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
                    name: model.name.clone(),
                    aliases: aliases.clone(),
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
    /// Every stored flag by its name, one this build does not know
    /// included ([`stored_flags`]).
    pub flags: Vec<String>,
    /// The provider's first cited pricing page.
    pub pricing_url: Option<String>,
    pub prices_checked: Option<NaiveDate>,
    /// The model a confirmation would price: the public model a channel
    /// model serves the entry's upstream name through, or a hand-made live
    /// direct model whose provider and model name are the entry's
    /// ([`suggest_models`]).
    pub suggested_model_id: Option<Uuid>,
    /// `pending`, `confirmed`, `rejected` or `superseded`.
    pub status: String,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub reject_reason: Option<String>,
    /// The operator who attested a confirmation's price, written once.
    pub attested_by: Option<Uuid>,
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
                                p.attested_by, p.baseline_revision_id";

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
            flags: stored_flags(flags),
            pricing_url: row.try_get("pricing_url")?,
            prices_checked: row.try_get("prices_checked")?,
            suggested_model_id: row.try_get("suggested_model_id")?,
            status: row.try_get("status")?,
            decided_by: row.try_get("decided_by")?,
            decided_at: row.try_get("decided_at")?,
            reject_reason: row.try_get("reject_reason")?,
            attested_by: row.try_get("attested_by")?,
            baseline_revision_id: row.try_get("baseline_revision_id")?,
            current_baseline: None,
            catalogue_entry: None,
            same_as_current: false,
        })
    }
}

/// A stored `flags` column as the review lists it: each flag by its name,
/// a known one or not, so a proposal reads as flagged exactly when
/// [`check_confirmable`] refuses it. Decoding into [`Flag`] would lose a
/// flag a newer build wrote, and with it every known flag beside it.
fn stored_flags(flags: Value) -> Vec<String> {
    match flags {
        Value::Null => Vec::new(),
        Value::Array(flags) => flags
            .into_iter()
            .map(|flag| match flag {
                Value::String(name) => name,
                other => other.to_string(),
            })
            .collect(),
        other => vec![other.to_string()],
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
    // A model deleted after the upload keeps its baseline but can take no new
    // one, so a proposal naming it is not "nothing to confirm" — it is
    // something that can no longer be confirmed. Its baseline still shows.
    let live: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM gw_resources WHERE kind = $1 AND NOT deleted AND id = ANY($2)",
    )
    .bind(Kind::Models.segment())
    .bind(models.as_slice())
    .fetch_all(&mut *conn)
    .await?;
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
        // A stored card this engine cannot read equals nothing, and so does a
        // baseline its model can no longer be given.
        proposal.same_as_current = match (baseline, proposal.card.as_ref()) {
            (Some(baseline), Some(proposed)) => {
                live.contains(&baseline.model_id)
                    && card(&baseline.card).is_some_and(|b| card(proposed) == Some(b))
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

/// The model a confirmation of each pair `(provider, model)` should price:
/// the one tenants call for that upstream model.
///
/// A live direct document whose `provider` and `model_name` are the pair's
/// matches. One a channel model generated (`channel_models.direct_model_id`)
/// is never offered itself: tenants call its public model, whose id is the
/// one a baseline prices calls by, so the match resolves to that public
/// model. A public model is preferred over a hand-made direct model of the
/// same pair, then the smallest id of several.
///
/// It keys on the document's own upstream fields, so a routing, ensemble or
/// semantic model (which carry neither) never matches by itself, and an
/// embedding model, which is a direct model, can.
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
    // A generated document's public model is retired with its documents, so
    // a live generated document always resolves to a live public model.
    let rows: Vec<(String, String, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT ON (r.doc->>'provider', r.doc->>'model_name') \
                r.doc->>'provider', r.doc->>'model_name', COALESCE(cm.public_model_id, r.id) \
         FROM gw_resources r LEFT JOIN channel_models cm ON cm.direct_model_id = r.id \
         WHERE r.kind = $1 AND NOT r.deleted AND r.doc ? 'provider' AND r.doc ? 'model_name' \
           AND r.doc->>'provider' = ANY($2) AND r.doc->>'model_name' = ANY($3) \
         ORDER BY r.doc->>'provider', r.doc->>'model_name', cm.public_model_id IS NULL, \
                  COALESCE(cm.public_model_id, r.id)",
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
/// suggests, stores the dataset as the price library in place of the one in
/// use ([`super::library::replace`]), and records `pricing.import.upload`,
/// naming the library stored. Nothing here is a price.
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
    // Up to `MAX_IMPORT_BYTES` of JSON: parsed off the request workers. The
    // text comes back with the proposals, as the library stores it.
    let (parsed, data) = tokio::task::spawn_blocking(move || parse(&data, now).map(|p| (p, data)))
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
                // The upload is the library's offline path: the same text
                // becomes the copy a sync reads.
                let (library, replaced) = super::library::replace(
                    &mut *conn,
                    &super::library::NewLibrary {
                        origin: "upload",
                        source_url: None,
                        source_ref: Some(&dataset_ref),
                        data: data.get(),
                        entries: super::library::entry_count(&parsed),
                    },
                    &auth,
                )
                .await?;
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
                            // The same text became the price library.
                            "library_id": library.id,
                            "library_replaced": replaced,
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

/// What a confirmation attests, in the operator's own words: the checkbox's
/// text. It is the `422 /attested` answer and the audit row's record of what
/// was affirmed. A library sync attests the same sentence.
pub(crate) const ATTESTATION: &str = "已对照官方价格页核对";

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
    // The row is held by another transaction: an upload superseding this
    // batch, or another operator deciding the same proposal. Either way the
    // review page is showing a row that is about to change, which is what
    // `proposal_state` tells the caller. Still logged, as `error.rs`'s own
    // lock-timeout arm logs it: a run of them means something holds the row
    // too long, and without the line a stuck upload reads as an operator
    // double-clicking.
    .map_err(|e| match DbError::from(e) {
        e @ DbError::LockTimeout => {
            tracing::warn!(error = %e, "a proposal row was held past lock_timeout");
            ApiError::proposal_state()
        }
        e => ApiError::from(e),
    })?;
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
/// Refused in this order:
/// 0. `400`/`422` for a body that does not deserialize (an unknown member,
///    a missing `model_id` or `effective_from`), which answers before
///    anything below it is read.
/// 1. `422 /attested` unless `attested` is exactly `true`, with
///    "请确认已对照官方价格页核对". This is checked **before the proposal is
///    read**, so it is a bad body on a pid that names nothing that answers
///    `/attested` rather than `404`; the caller's own claim is settled first.
/// 2. `404` for a `pid` outside batch `{id}`, or an id that is not a UUID.
/// 3. `409 proposal_state` unless the proposal is still `pending`; a
///    superseded one counts as not pending.
/// 4. `422 /flags` for a proposal that carries any flag, or whose card this
///    engine cannot read (see [`check_confirmable`]).
/// 5. `422 /effective_from` for a start before the proposal's own, and for
///    one at or after the date a later-dated proposal of the same entry in
///    the same batch takes effect: the dataset itself says the price changed
///    by then, so that later proposal is the one to confirm, and the answer
///    names its date.
/// 6. `insert_baseline`'s own refusals, which answer last: `422 /model_id`
///    for a model that is not live or is a channel's generated direct
///    document, `422 /card` for a card it cannot apply, and its past-start
///    `/effective_from` check.
///
/// One transaction: the proposal row is taken `FOR UPDATE`, so of two
/// decisions on it exactly one commits, and the other answers
/// `409 proposal_state`. A row another writer holds past `lock_timeout` — an
/// in-flight upload's supersede sweep, or a concurrent decision — answers the
/// same `409`, not a `503`/`500`: the row is being decided, and the page that
/// sent the request is stale either way.
///
/// [`baselines::insert_baseline`] writes the revision labelled third-party,
/// carrying the batch's `dataset_ref`, the proposal's own page and checked
/// date, and this operator's attestation at `now()`; the proposal becomes
/// `confirmed`, naming the revision and the attesting operator, which
/// `0026`'s trigger permits only in this write. The confirmation's audit row
/// is `pricing.import.confirm`, holding the attester, `attested: true` and
/// the sentence affirmed, beside the baseline's own
/// `pricing.baseline.create`.
pub async fn confirm(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, pid)): Path<(String, String)>,
    ApiJson(body): ApiJson<ConfirmBody>,
) -> Result<(StatusCode, Json<BaselineRevision>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    if body.attested != Some(true) {
        return Err(refused("/attested", format!("请确认{ATTESTATION}")));
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
                // The latest of the entry's later-dated proposals that has
                // taken effect by the requested start, whatever its own
                // status: the dataset's word on the price does not change
                // with a decision on it.
                let later: Option<DateTime<Utc>> = sqlx::query_scalar(
                    "SELECT max(effective_from) FROM pricing_import_proposals \
                     WHERE batch_id = $1 AND provider = $2 AND model = $3 \
                       AND effective_from > COALESCE($4, '-infinity'::timestamptz) \
                       AND effective_from <= $5",
                )
                .bind(id)
                .bind(&proposal.provider)
                .bind(&proposal.model)
                .bind(proposal.effective_from)
                .bind(body.effective_from)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if let Some(later) = later {
                    return Err(refused(
                        "/effective_from",
                        format!(
                            "数据集中该模型自 {} 起已有更新的价格：生效时间须早于该日，或确认那一条",
                            later.format("%Y-%m-%d")
                        ),
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
                        import_id: Some(id),
                        proposal_id: Some(pid),
                        library_id: None,
                        library_entry: None,
                        attested_by: auth.id,
                        attested_at: now,
                    },
                };
                let (revision, _) =
                    super::baselines::insert_baseline(&mut *conn, &row, Actor::Operator(&auth))
                        .await?;
                // `attested_by` is written here and only here: `0026`'s
                // trigger allows the column to change only on a confirmation,
                // and refuses any update of a row already decided.
                let written = sqlx::query(
                    "UPDATE pricing_import_proposals \
                     SET status = 'confirmed', decided_by = $1, decided_at = $2, \
                         attested_by = $1, baseline_revision_id = $3 \
                     WHERE id = $4 AND status = 'pending'",
                )
                .bind(auth.id)
                .bind(now)
                .bind(revision.revision_id)
                .bind(pid)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                // The row was locked `pending` a moment ago, so this is
                // unreachable; should the lock ever stop carrying the
                // invariant, the caller is told to refresh rather than
                // answered as if it had confirmed.
                if written.rows_affected() != 1 {
                    return Err(ApiError::proposal_state());
                }
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
                            // The sentence the operator affirmed, so an
                            // attestation stays readable after a later
                            // release rewords the checkbox.
                            "attestation": ATTESTATION,
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
                let written = sqlx::query(
                    "UPDATE pricing_import_proposals \
                     SET status = 'rejected', reject_reason = $1, decided_by = $2, \
                         decided_at = now() \
                     WHERE id = $3 AND status = 'pending'",
                )
                .bind(&reason)
                .bind(auth.id)
                .bind(pid)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                // As in `confirm`: unreachable under the row lock, and a
                // refusal rather than a silent rewrite if that ever changes.
                if written.rows_affected() != 1 {
                    return Err(ApiError::proposal_state());
                }
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
    models: Vec<DatasetModel<'a>>,
}

#[derive(Deserialize)]
struct DatasetModel<'a> {
    id: String,
    #[serde(default)]
    name: Option<String>,
    /// The names the dataset's reference package matches the model by: a
    /// clause tree of `equals`, `starts_with`, `or` and the like.
    #[serde(default, rename = "match")]
    matches: Option<Value>,
    /// A price map, or a list of conditional ones ([`conditionals`]).
    #[serde(borrow)]
    prices: &'a RawValue,
    #[serde(default, borrow)]
    price_variants: Option<Vec<Variant<'a>>>,
    #[serde(default)]
    prices_checked: Option<NaiveDate>,
    /// The dataset's own remarks on the model's pricing; a few state that
    /// output is free, which no price field says.
    #[serde(default)]
    price_comments: Option<String>,
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

/// The exact names a model's `match` clause names besides its id: each
/// `equals` at the top of the clause or under an `or`, by its text, in the
/// clause's order and once each. A pattern (`starts_with`, `contains`,
/// `regex`) names no one model, and an `equals` under an `and` holds only
/// with the rest of its clause, so neither is an alias.
fn aliases(id: &str, clause: Option<&Value>) -> Vec<String> {
    fn walk(clause: &Value, out: &mut Vec<String>) {
        let Value::Object(fields) = clause else {
            return;
        };
        if let Some(Value::String(name)) = fields.get("equals") {
            out.push(name.clone());
        }
        if let Some(Value::Array(branches)) = fields.get("or") {
            for branch in branches {
                walk(branch, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(clause) = clause {
        walk(clause, &mut out);
    }
    let mut seen = BTreeSet::new();
    out.retain(|name| name != id && seen.insert(name.clone()));
    out
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
    entry: (&str, &str),
    comments: Option<&str>,
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
            let card = card(&tiers, entry, comments, &mut flags);
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

/// Why a field's value is not a price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unreadable {
    /// Not a number [`Rate::parse`] reads: digits, then at most six decimals,
    /// or a tiered price of such numbers with whole-number starts.
    Number,
    /// More tiers than [`MAX_TIERS_PER_FIELD`].
    Tiers,
}

impl Price {
    /// The value's text, multiplied by `per`.
    fn read(value: &RawValue, per: u64) -> Result<Price, Unreadable> {
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
            return rate(text, per).map(Price::Flat).ok_or(Unreadable::Number);
        }
        let tiered: Tiered<'_> = serde_json::from_str(text).map_err(|_| Unreadable::Number)?;
        if tiered.tiers.len() > MAX_TIERS_PER_FIELD {
            return Err(Unreadable::Tiers);
        }
        let mut tiers: Vec<(u64, Rate)> = Vec::with_capacity(tiered.tiers.len());
        for tier in &tiered.tiers {
            let start = start(tier.start.get()).ok_or(Unreadable::Number)?;
            let price = rate(tier.price.get(), per).ok_or(Unreadable::Number)?;
            tiers.push((start, price));
        }
        // Stable, so of two tiers with one start the later one wins, as in
        // the dataset's own resolution.
        tiers.sort_by_key(|(start, _)| *start);
        Ok(Price::Tiered(
            rate(tiered.base.get(), per).ok_or(Unreadable::Number)?,
            tiers,
        ))
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
            (Price::Tiered(base, tiers), Some(threshold)) => {
                // Tier starts rise, so this is the last one at or below the
                // threshold: a band's price is found without walking them.
                match tiers.partition_point(|(start, _)| *start <= threshold) {
                    0 => *base,
                    below => tiers[below - 1].1,
                }
            }
        }
    }
}

/// Whether `url` is an `http` or `https` page: a scheme, then a host, and
/// nothing in the authority that makes the host a browser dials differ from
/// the text the review prints: no userinfo (`https://a.example@b.example/`
/// goes to `b.example`), no backslash, which a browser reads as `/`, and no
/// control character or whitespace, since a browser drops a tab or a line
/// break from a URL before it reads the host
/// (`https://a.example\n.b.example/` goes to `a.example.b.example`).
fn is_web_page(url: &str) -> bool {
    let (scheme, rest) = match url.split_once("://") {
        Some(parts) => parts,
        None => return false,
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // The dataset's own entries all name a host; a scheme alone names none.
    !host.is_empty()
        && !host
            .chars()
            .any(|c| matches!(c, '@' | '\\') || c.is_control() || c.is_whitespace())
}

/// The dataset's models whose output price is a stated zero rather than a
/// missing one.
///
/// A Decisions-style model answers without billing its completion, and the
/// dataset publishes only its input price while `price_comments` says why.
/// These two are the entries this build knows that for. The list is by entry
/// on purpose: several other entries carry a comment that says "output is
/// free" and are ordinary chat models, so a statement alone decides nothing.
const FREE_OUTPUT: [(&str, &str); 2] = [
    ("perplexity", "pplx-decider-v1.2"),
    ("perplexity", "pplx-decider-flash-v1.2"),
];

/// Whether this provider's model states that its output is free, in its
/// `price_comments` as well as in [`FREE_OUTPUT`].
///
/// The statements are the phrases a pricing page writes for such a model:
/// "output tokens are free", "output free", "output is free". A comment about
/// a free *tier*, free allowances, or a free input or cache price says
/// nothing about output and does not match.
fn free_output(provider: &str, model: &str, comments: Option<&str>) -> bool {
    if !FREE_OUTPUT.contains(&(provider, model)) {
        return false;
    }
    let Some(comments) = comments else {
        return false;
    };
    let comments = comments.to_ascii_lowercase();
    const STATEMENTS: [&str; 3] = ["output tokens are free", "output free", "output is free"];
    // Sentence by sentence, so a statement owes nothing to its neighbours.
    comments.split(['.', ';']).any(|sentence| {
        let sentence = sentence.trim();
        STATEMENTS.iter().any(|statement| {
            sentence.starts_with(statement) || sentence.contains(&format!(" {statement}"))
        })
    })
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
    entry: (&str, &str),
    comments: Option<&str>,
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
                Ok(price) => {
                    prices.insert(dimension, price);
                }
                Err(Unreadable::Number) => {
                    flags.insert(Flag::BadNumber);
                }
                Err(Unreadable::Tiers) => {
                    // The value is a price this card cannot carry, not a
                    // malformed one: it is left off like any other field no
                    // dimension meters.
                    tracing::warn!(
                        field,
                        tiers = MAX_TIERS_PER_FIELD + 1,
                        limit = MAX_TIERS_PER_FIELD,
                        "a price field carries more tiers than a card may hold; it is left off"
                    );
                    flags.insert(Flag::UnsupportedField);
                }
            }
        }
        // A map with no output-side price is no card. A missing output price
        // on a chat model is a gap in the dataset, not a free completion, so
        // the one exception is a model this build names and the dataset says
        // in so many words bills its output at nothing. Both are required:
        // a comment that says "output is free" on an unnamed model changes
        // nothing.
        let names_output = fields.keys().any(|f| f.starts_with("output_"));
        if !names_output
            && prices.contains_key(&Dimension::Input)
            && !prices.contains_key(&Dimension::Output)
            && free_output(entry.0, entry.1, comments)
        {
            prices.insert(Dimension::Output, Price::Flat(Rate(0)));
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
               {"id":"m-zero-start","prices":{"input_mtok":{"base":1,"tiers":[{"start":0,"price":2}]},"output_mtok":1}},
               {"id":"m-empty","prices":{}},
               {"id":"m-unmappable","prices":{"audio_hours":3,"input_mchars":1,
                 "input_document_kpages":2}},
               {"id":"m-search-only","prices":{"web_searches_kcount":10}},
               {"id":"m-output-image","prices":{"input_mtok":1,"output_image_mtok":30}}"#,
        ));
        for (id, flags) in [
            // A chat model with no output price is a gap in the dataset, not
            // a free completion.
            ("m-input-only", vec![Flag::InvalidCard]),
            ("m-zero-start", vec![Flag::InvalidCard]),
            // No price at all.
            ("m-empty", vec![Flag::InvalidCard]),
            // Prices in units no card meters, and so no card.
            (
                "m-unmappable",
                vec![Flag::UnsupportedField, Flag::InvalidCard],
            ),
            // Neither side of a prompt, and no output.
            ("m-search-only", vec![Flag::InvalidCard]),
            // An output side the card has no slot for.
            (
                "m-output-image",
                vec![Flag::UnsupportedField, Flag::InvalidCard],
            ),
        ] {
            let p = current(&all, id);
            assert_eq!((p.card.as_ref(), &p.flags[..]), (None, &flags[..]), "{id}");
        }
    }

    #[test]
    fn output_stated_free_is_a_zero_and_never_inferred() {
        /// A dataset with one provider `p` whose models are `models`.
        fn provider(p: &str, models: &str) -> String {
            format!(r#"[{{"id":"{p}","models":[{models}]}}]"#)
        }
        let decider = r#"{"id":"pplx-decider-v1.2","prices":{"input_mtok":0.02},
             "price_comments":"Decisions API: output tokens are free and there is no per-request fee."}"#;
        let flash = r#"{"id":"pplx-decider-flash-v1.2","prices":{"input_mtok":0.01},
             "price_comments":"Decisions API: output tokens are free and there is no per-request fee."}"#;

        // The two entries the dataset states a free output for, at the
        // provider that states it.
        let all = proposals(&provider("perplexity", &format!("{decider},{flash}")));
        for (id, input) in [
            ("pplx-decider-v1.2", "0.02"),
            ("pplx-decider-flash-v1.2", "0.01"),
        ] {
            let p = current(&all, id);
            assert!(p.flags.is_empty(), "{id}: {:?}", p.flags);
            let base = default_base(p);
            assert_eq!(base.output, Rate(0), "{id}");
            // The input price is still the file's own.
            assert_eq!(base.input, Rate::parse(input).unwrap(), "{id}");
        }

        // The same two names under another provider: the statement is not
        // read there.
        let all = proposals(&provider("elsewhere", &format!("{decider},{flash}")));
        for id in ["pplx-decider-v1.2", "pplx-decider-flash-v1.2"] {
            assert_eq!(
                (
                    current(&all, id).card.as_ref(),
                    &current(&all, id).flags[..]
                ),
                (None, &[Flag::InvalidCard][..]),
                "{id} under another provider"
            );
        }

        // A named model whose comment says the output is free but whose
        // prices name output: the price stands.
        let priced = provider(
            "perplexity",
            r#"{"id":"pplx-decider-v1.2","prices":{"input_mtok":0.02,"output_mtok":1.5},
                 "price_comments":"Output is free for the first million tokens."}"#,
        );
        let all = proposals(&priced);
        assert!(current(&all, "pplx-decider-v1.2").flags.is_empty());
        assert_eq!(
            default_base(current(&all, "pplx-decider-v1.2")).output,
            Rate::parse("1.5").unwrap()
        );

        // A model the build does not name keeps its card-less flag, whatever
        // its comment says. Several real chat models carry "Output is free."
        let other = provider(
            "typesafe",
            r#"{"id":"jev-1.13","prices":{"input_mtok":0.042},
                 "price_comments":"Same $0.042 per million input tokens as TypeSafe. Output is free."}"#,
        );
        let all = proposals(&other);
        assert_eq!(
            (
                current(&all, "jev-1.13").card.as_ref(),
                &current(&all, "jev-1.13").flags[..]
            ),
            (None, &[Flag::InvalidCard][..])
        );

        // A free tier, a free cache read and a free input price say nothing
        // about output, and neither does the named model without a comment.
        let silent = provider(
            "perplexity",
            r#"{"id":"pplx-decider-v1.2","prices":{"input_mtok":0.02}},
               {"id":"m-free-cache","prices":{"input_mtok":0.02},
                 "price_comments":"Prompt cache reads are free on this model."},
               {"id":"m-free-input","prices":{"input_mtok":0.02},
                 "price_comments":"Input is free; output is billed per million tokens."},
               {"id":"m-free-tier","prices":{"input_mtok":0.99,"output_mtok":1.49},
                 "price_comments":"Developer tier pricing. Free tier: 65k context."}"#,
        );
        let all = proposals(&silent);
        for id in ["pplx-decider-v1.2", "m-free-cache", "m-free-input"] {
            assert_eq!(
                (
                    current(&all, id).card.as_ref(),
                    &current(&all, id).flags[..]
                ),
                (None, &[Flag::InvalidCard][..]),
                "{id}"
            );
        }
        // One that prices output keeps it.
        let tiered = current(&all, "m-free-tier");
        assert_eq!(default_base(tiered).output, Rate::parse("1.49").unwrap());
    }

    #[test]
    fn a_price_field_carries_a_bounded_number_of_tiers() {
        let tiers = |n: usize| {
            (1..=n)
                .map(|i| format!(r#"{{"start":{},"price":2}}"#, i * 1000))
                .collect::<Vec<_>>()
                .join(",")
        };
        let body = |n: usize| {
            dataset(&format!(
                r#"{{"id":"m-{n}","prices":{{"input_mtok":{{"base":1,"tiers":[{}]}},"output_mtok":1}}}}"#,
                tiers(n)
            ))
        };

        // The boundary is written out, not derived from the constant, so
        // moving the cap fails this test rather than moving its input too.
        assert_eq!(MAX_TIERS_PER_FIELD, 64);
        let (at_cap, over) = (64, 65);

        // At the cap, the card carries every band.
        let all = proposals(&body(at_cap));
        let p = current(&all, &format!("m-{at_cap}"));
        assert!(p.flags.is_empty(), "{:?}", p.flags);
        let card = p.card.as_ref().expect("a card");
        let tiered = &card.0[&ServiceTier::default_tier()];
        assert_eq!(tiered.long_context.len(), at_cap);
        assert_eq!(
            tiered.long_context.last().map(|b| b.above_input_tokens),
            Some(at_cap as u64 * 1000)
        );
        card.validate().expect("the card validates");

        // One past it, the field is left off. For an input price that costs
        // the card, since a card needs one.
        let all = proposals(&body(over));
        let p = current(&all, &format!("m-{over}"));
        assert_eq!(
            p.flags,
            vec![Flag::UnsupportedField, Flag::InvalidCard],
            "over the cap"
        );
        assert_eq!(p.card, None);

        // Left off, not fatal: a field the card can do without is dropped and
        // the card stands.
        let optional = dataset(&format!(
            r#"{{"id":"m-optional","prices":{{"input_mtok":1,"output_mtok":2,
                 "cache_read_mtok":{{"base":0.1,"tiers":[{}]}}}}}}"#,
            tiers(over)
        ));
        let all = proposals(&optional);
        let p = current(&all, "m-optional");
        assert_eq!(p.flags, vec![Flag::UnsupportedField]);
        let card = p
            .card
            .as_ref()
            .expect("a card without its cache-read price");
        let tiered = &card.0[&ServiceTier::default_tier()];
        assert!(tiered.long_context.is_empty(), "{tiered:?}");
        assert_eq!(tiered.base.cache_read, None, "left off, not rounded");
        card.validate().expect("the card validates");
    }

    #[test]
    fn provenance_is_the_providers_first_page_and_the_models_checked_date() {
        let all = proposals(
            r#"[{"id":"p","pricing_urls":["https://p.example/a","https://p.example/b"],"models":[
                  {"id":"m-checked","prices_checked":"2026-10-19","prices":{"input_mtok":1,"output_mtok":2}},
                  {"id":"m-unchecked","prices":{"input_mtok":1,"output_mtok":2}}]},
                {"id":"q","models":[{"id":"m-uncited","prices":{"input_mtok":1,"output_mtok":2}}]},
                {"id":"r","pricing_urls":["javascript:alert(1)","http://r.example/p"],
                 "models":[{"id":"m-untrusted","prices":{"input_mtok":1,"output_mtok":2}}]},
                {"id":"s","pricing_urls":["data:text/html,<script>alert(1)</script>","ftp://s.example/p"],
                 "models":[{"id":"m-no-page","prices":{"input_mtok":1,"output_mtok":2}}]},
                {"id":"t","pricing_urls":["https://openai.com@t.example/p","https://openai.com\\t.example/p",
                                          "https://openai.com\n.t.example/p","https://openai.com\t.t.example/p",
                                          "https://openai.com .t.example/p","https://t.example/pricing"],
                 "models":[{"id":"m-disguised","prices":{"input_mtok":1,"output_mtok":2}}]}]"#,
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
        // A page is a page: the first `http(s)` URL wins, and a provider
        // that cites none keeps none.
        assert_eq!(
            current(&all, "m-untrusted").pricing_url.as_deref(),
            Some("http://r.example/p")
        );
        assert_eq!(current(&all, "m-no-page").pricing_url, None);
        // A page whose printed text names one host and whose link goes to
        // another is no page: userinfo, a backslash a browser reads as `/`,
        // and a tab or line break a browser drops all put the real host
        // elsewhere.
        assert_eq!(
            current(&all, "m-disguised").pricing_url.as_deref(),
            Some("https://t.example/pricing")
        );
    }

    #[test]
    fn a_stored_flag_this_build_does_not_know_is_listed_beside_the_known_ones() {
        assert_eq!(
            stored_flags(json!(["unsupported_field", "new_flag"])),
            vec!["unsupported_field".to_string(), "new_flag".to_string()]
        );
        assert!(stored_flags(json!([])).is_empty());
        assert!(stored_flags(Value::Null).is_empty());
        // Whatever else the column holds still reads as flagged, as
        // `check_confirmable` reads it.
        assert_eq!(
            stored_flags(json!({"x": 1})),
            vec![r#"{"x":1}"#.to_string()]
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
