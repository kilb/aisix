//! The price library: a stored copy of the public price dataset the import
//! reads ([`super::import`]), and the sync of baselines from it.
//!
//! # Filling it
//!
//! `POST /admin/pricing/library/fetch` reads the dataset from [`LIBRARY_URL`],
//! a constant of this build: no request field, configuration or environment
//! variable names another address, and only a caller of this library can
//! ([`Source::new`], as the tests do). The fetch is bounded: [`FETCH_TIMEOUT`]
//! for the whole call, [`MAX_LIBRARY_BYTES`] of body, and no redirect off the
//! address's own origin ([`crate::upstream_http::fixed_origin_client`]). Its
//! errors go through the shared scrubber. An upload of the dataset file
//! (`POST /admin/pricing/imports`) is the offline path and fills the same
//! library. Either way the text is read by the import's own parser
//! ([`import::parse`]) before it is stored, and a newer copy replaces the
//! older in one transaction ([`replace`]).
//!
//! The library is read at the moment of reading: each entry is the dataset's
//! price in effect then, by the dataset's own dates.
//!
//! # Matching a model to an entry
//!
//! A priceable model ([`crate::channels::priceable_models`]) is matched by
//! its name, the one tenants send ([`Index::find`]):
//!
//! 1. **Exactly**: an entry whose id is the name, or whose
//!    `<provider>/<id>` is.
//! 2. Only when nothing matched exactly, **loosely**: case-insensitively, an
//!    entry whose id or one of the exact names its own `match` clause lists
//!    ([`import::ParsedProposal::aliases`]) is the name, with or without the
//!    entry's own `<provider>/` in front; and an entry whose id or alias
//!    carries a prefix naming one of the library's providers (an
//!    aggregator's `mistral/…`) is found without it too. A prefix on the
//!    model's name is never taken off to reach another provider's entry: it
//!    says whose model it is.
//!
//! Of several candidates, the ones whose provider is the model's own (a
//! direct model's `provider`, the providers of the accounts serving a public
//! model, or a `<provider>/` prefix on its name) are kept when there are any.
//! When that leaves more than one, the ones whose provider is the protocol
//! family (`adapter`) of an account serving the model are kept when there
//! are any: an account under a provider id of the platform's own that speaks
//! `anthropic` finds the first-party entry, not the cloud and aggregator
//! copies of the same name ([`MatchedBy::Adapter`]). The model's own
//! provider is weighed first, so it wins over an adapter. One candidate left
//! is the match; more is ambiguous, and is synced only from the candidate an
//! operator chooses.
//!
//! # Choosing
//!
//! A sync that lists an ambiguous model under one of its candidates records
//! the choice (`pricing_library_bindings`, one row per model, audited as
//! `pricing.library.bind`). From then on the model matches the chosen entry
//! before anything its name finds ([`MatchedBy::Chosen`]), so later syncs
//! need no choice. A choice whose entry has left the library is ignored: the
//! model is matched by name again, and the preview says so (its `binding`,
//! `in_library: false`). `DELETE /admin/pricing/library/bindings/{model_id}`
//! forgets a choice ([`unbind`]).
//!
//! # Syncing
//!
//! `GET /admin/pricing/library/sync` previews every priceable model (or one,
//! with `model_id`): `create` (no baseline in force), `update` (the library's
//! card differs from it), `same`, `flagged` (the entry carries a price the
//! import would not confirm either), `unmatched` and `ambiguous`. `POST` of
//! the same route writes the `create` and `update` items the operator lists,
//! an ambiguous model's under the candidate chosen, and nothing else, in one
//! transaction, each as a `third_party` baseline starting now, under the
//! operator's attestation. See [`sync`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use aisix_gateway::upstream_error_scrub::scrub_upstream_error_text;
use aisix_pricing::{RateCard, ServiceTier};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
use futures::FutureExt;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::baselines::{self, BaselineRevision, BaselineSource, NewBaseline, OperatorRef};
use super::import::{self, Flag, ParsedProposal, ATTESTATION, DATASET};
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::resources::Kind;
use crate::usage::query_rejected;
use crate::AppState;

/// Where the dataset is published: the raw file of its current data, as the
/// dataset's README links it (`prices/new_data/v2/data.json`, the format the
/// import reads; the `prices/data.json` beside it is the frozen v1 file).
pub const LIBRARY_URL: &str =
    "https://raw.githubusercontent.com/pydantic/genai-prices/main/prices/new_data/v2/data.json";

/// The most of the dataset a fetch reads. The published file is under 1 MiB.
pub const MAX_LIBRARY_BYTES: usize = 10 << 20;

/// How long a whole fetch may take, the body included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The most models one sync writes.
///
/// Each one is a baseline write of about eight statements (its checks, the
/// row, the `pricing_version` bump, the superseded-later read, two audit
/// rows), all in one transaction that holds `pricing_version`'s row from the
/// first write until it commits — and every other price write waits on that
/// row, for up to the connection's `lock_timeout` (5 s by default) before it
/// answers `503`. A hundred models is some 800 statements: well under a
/// second on a database a millisecond away. A larger fleet syncs in more
/// than one confirmation; the preview says how many are left.
pub const MAX_SYNC_MODELS: usize = 100;

/// How many entries a browse lists by default, and at most.
const DEFAULT_LISTED: usize = 50;
const MAX_LISTED: usize = 200;

/// The most of a scrubbed detail an operator sees, counted after scrubbing.
const MAX_DETAIL_CHARS: usize = 300;

/// The address the library is fetched from, and the client it goes out on.
pub struct Source {
    url: Url,
    client: reqwest::Client,
}

impl Source {
    /// A source reading `url`. Production passes [`LIBRARY_URL`]; only a
    /// caller of this library can pass another (a test's stand-in).
    pub fn new(url: Url) -> Result<Source, String> {
        let client = crate::upstream_http::fixed_origin_client(&url, FETCH_TIMEOUT)?;
        Ok(Source { url, client })
    }

    /// The dataset's text, or why there is none.
    async fn fetch(&self) -> Result<String, ApiError> {
        let response = self
            .client
            .get(self.url.clone())
            .send()
            .await
            .map_err(|e| transport_failure(&e))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(ApiError::library_unreachable(
                "价格库地址跳转到了其他地址，未跟随",
            ));
        }
        if !status.is_success() {
            // The answer's words, scrubbed, as far as a short read gets.
            let text = read_body(response, 4096)
                .await
                .map(|body| String::from_utf8_lossy(&body).trim().to_string())
                .unwrap_or_default();
            return Err(ApiError::library_unreachable(capped(format!(
                "价格库地址返回 HTTP {}：{}",
                status.as_u16(),
                scrub_upstream_error_text(&text)
            ))));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_LIBRARY_BYTES as u64)
        {
            return Err(too_large());
        }
        let body = read_body(response, MAX_LIBRARY_BYTES).await?;
        String::from_utf8(body)
            .map_err(|_| ApiError::library_unreachable("下载的内容不是可识别的价格数据集"))
    }
}

fn too_large() -> ApiError {
    ApiError::library_unreachable(format!("价格库数据超过 {} MiB", MAX_LIBRARY_BYTES >> 20))
}

/// `text` cut to [`MAX_DETAIL_CHARS`], after every scrub has run.
fn capped(text: String) -> String {
    text.chars().take(MAX_DETAIL_CHARS).collect()
}

/// A failed request as the fetch's answer: a timeout, or the transport's own
/// words, scrubbed.
fn transport_failure(error: &reqwest::Error) -> ApiError {
    if error.is_timeout() {
        return ApiError::library_timeout();
    }
    ApiError::library_unreachable(capped(format!(
        "无法连接价格库地址：{}",
        scrub_upstream_error_text(&aisix_gateway::transport_error_message(error))
    )))
}

/// The body, to `cap` bytes; a larger one is refused rather than buffered
/// further.
async fn read_body(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, ApiError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| transport_failure(&e))? {
        if body.len() + chunk.len() > cap {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The library in use, as its row describes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibraryMeta {
    pub id: Uuid,
    pub dataset: String,
    /// `fetch` or `upload`.
    pub origin: String,
    /// The address a fetch read.
    pub source_url: Option<String>,
    /// The revision the payload names, or the one the operator gave an
    /// upload.
    pub source_ref: Option<String>,
    pub sha256: String,
    pub bytes: i64,
    /// How many entries it held when it was stored: each model once.
    pub entries: i64,
    pub fetched_at: DateTime<Utc>,
    pub created_by: OperatorRef,
}

impl LibraryMeta {
    /// The revision a baseline synced from it names: the one the payload or
    /// the operator gave, else its content's digest.
    fn reference(&self) -> String {
        match &self.source_ref {
            Some(reference) => reference.clone(),
            None => format!("sha256:{}", &self.sha256[..self.sha256.len().min(12)]),
        }
    }
}

impl<'r> sqlx::FromRow<'r, PgRow> for LibraryMeta {
    fn from_row(row: &'r PgRow) -> Result<LibraryMeta, sqlx::Error> {
        Ok(LibraryMeta {
            id: row.try_get("id")?,
            dataset: row.try_get("dataset")?,
            origin: row.try_get("origin")?,
            source_url: row.try_get("source_url")?,
            source_ref: row.try_get("source_ref")?,
            sha256: row.try_get("sha256")?,
            bytes: row.try_get("bytes")?,
            entries: row.try_get("entries")?,
            fetched_at: row.try_get("fetched_at")?,
            created_by: OperatorRef {
                id: row.try_get("created_by")?,
                email: row.try_get("created_by_email")?,
            },
        })
    }
}

/// The columns a [`LibraryMeta`] is read from, over `pricing_library l`
/// joined to `operators o` on `created_by`.
const META_COLUMNS: &str = "l.id, l.dataset, l.origin, l.source_url, l.source_ref, l.sha256, \
                            l.bytes, l.entries, l.fetched_at, l.created_by, \
                            o.email AS created_by_email";

/// A copy of the dataset to store as the library.
pub(crate) struct NewLibrary<'a> {
    /// `fetch` or `upload`.
    pub origin: &'static str,
    pub source_url: Option<&'a str>,
    pub source_ref: Option<&'a str>,
    /// The dataset's text, already read by [`import::parse`].
    pub data: &'a str,
    /// How many priced entries it holds.
    pub entries: usize,
}

/// Stores `new` as the library, replacing the one in use, in the caller's
/// transaction: the library stored, and the ids of the copies it replaced,
/// which the caller's audit row names (a fetch's own
/// `pricing.library.update`, an upload's `pricing.import.upload`).
///
/// One replacement at a time: the table lock conflicts with itself and with
/// no read, and a sync holding the old row (`FOR UPDATE`) makes the delete
/// wait until it commits, so a sync never writes from a library that was
/// replaced under it.
pub(crate) async fn replace(
    conn: &mut PgConnection,
    new: &NewLibrary<'_>,
    auth: &OperatorAuth,
) -> Result<(LibraryMeta, Vec<Uuid>), ApiError> {
    sqlx::query("LOCK TABLE pricing_library IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let replaced: Vec<Uuid> = sqlx::query_scalar("DELETE FROM pricing_library RETURNING id")
        .fetch_all(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let sha256 = hex::encode(Sha256::digest(new.data.as_bytes()));
    let meta: LibraryMeta = sqlx::query_as(&format!(
        "WITH l AS (
           INSERT INTO pricing_library
           (id, dataset, origin, source_url, source_ref, sha256, bytes, entries, data, created_by)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *)
         SELECT {META_COLUMNS} FROM l JOIN operators o ON o.id = l.created_by"
    ))
    .bind(Uuid::new_v4())
    .bind(DATASET)
    .bind(new.origin)
    .bind(new.source_url)
    .bind(new.source_ref)
    .bind(&sha256)
    .bind(new.data.len() as i64)
    .bind(new.entries as i64)
    .bind(new.data)
    .bind(auth.id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    Ok((meta, replaced))
}

/// How many entries `proposals` holds: each model once, by its proposal in
/// effect at the parse, whether or not its prices made a card.
pub(crate) fn entry_count(proposals: &[ParsedProposal]) -> usize {
    proposals
        .iter()
        .filter(|p| p.effective_from.is_none())
        .count()
}

/// The library in use, with its text.
async fn current(conn: &mut PgConnection) -> Result<Option<(LibraryMeta, String)>, DbError> {
    let row: Option<PgRow> = sqlx::query(&format!(
        "SELECT {META_COLUMNS}, l.data FROM pricing_library l \
         JOIN operators o ON o.id = l.created_by ORDER BY l.fetched_at DESC LIMIT 1"
    ))
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let meta = <LibraryMeta as sqlx::FromRow<'_, PgRow>>::from_row(&row)?;
    let data: String = row.try_get("data")?;
    Ok(Some((meta, data)))
}

/// The library's entries at `now`: each one's price in effect then, read off
/// the request workers. A stored text the parser no longer reads is an
/// internal error: it was read before it was stored.
async fn entries_at(data: String, now: DateTime<Utc>) -> Result<Vec<ParsedProposal>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let raw = RawValue::from_string(data).map_err(ApiError::internal)?;
        let parsed = import::parse(&raw, now)
            .map_err(|e| ApiError::internal(format!("the stored library: {}", e.message)))?;
        Ok(parsed
            .into_iter()
            .filter(|p| p.effective_from.is_none())
            .collect())
    })
    .await
    .map_err(ApiError::internal)?
}

/// The database's `now()`.
async fn db_now(conn: &mut PgConnection) -> Result<DateTime<Utc>, DbError> {
    Ok(sqlx::query_scalar("SELECT now()").fetch_one(conn).await?)
}

/// One library entry as an answer shows it.
#[derive(Debug, Serialize)]
struct EntryView<'a> {
    provider: &'a str,
    model: &'a str,
    name: Option<&'a str>,
    aliases: &'a [String],
    /// `None` when its prices make no card.
    card: Option<&'a RateCard>,
    flags: &'a [Flag],
    pricing_url: Option<&'a str>,
    prices_checked: Option<NaiveDate>,
}

fn view(p: &ParsedProposal) -> EntryView<'_> {
    EntryView {
        provider: &p.provider,
        model: &p.model,
        name: p.name.as_deref(),
        aliases: &p.aliases,
        card: p.card.as_ref(),
        flags: &p.flags,
        pricing_url: p.pricing_url.as_deref(),
        prices_checked: p.prices_checked,
    }
}

/// `GET /admin/pricing/library`'s query.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseQuery {
    /// Matched, case-insensitively, against each entry's provider, id, name
    /// and aliases.
    #[serde(default)]
    q: Option<String>,
    /// 1 to 200; 50 when left out.
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /admin/pricing/library?q=&limit=` → `{library, total, items}`: the
/// library in use (`null` before the first fetch or upload), how many of its
/// entries match `q`, and the first `limit` of them in the dataset's order,
/// each with its card in effect now, its flags and its provider's pricing
/// page.
pub async fn browse(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<BrowseQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = q.limit.unwrap_or(DEFAULT_LISTED).clamp(1, MAX_LISTED);
    let needle =
        q.q.as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_lowercase();
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let Some((meta, data)) = current(&mut conn).await? else {
        return Ok(Json(json!({"library": null, "total": 0, "items": []})));
    };
    let now = db_now(&mut conn).await?;
    drop(conn);
    let entries = entries_at(data, now).await?;
    let found: Vec<&ParsedProposal> = entries
        .iter()
        .filter(|p| {
            needle.is_empty()
                || std::iter::once(&p.provider)
                    .chain(std::iter::once(&p.model))
                    .chain(p.name.as_ref())
                    .chain(&p.aliases)
                    .any(|text| text.to_lowercase().contains(&needle))
        })
        .collect();
    let items: Vec<EntryView<'_>> = found.iter().take(limit).copied().map(view).collect();
    Ok(Json(json!({
        "library": meta,
        "total": found.len(),
        "items": items,
    })))
}

/// `POST /admin/pricing/library/fetch` → `201 LibraryMeta`: reads the
/// dataset from its address, parses it as an upload is parsed, and stores it
/// as the library, replacing the one in use (`pricing.library.update`).
///
/// `502 library_fetch_failed` when the address cannot be reached, answers
/// with anything but a `2xx` (a redirect off its origin included), sends more
/// than [`MAX_LIBRARY_BYTES`] or sends something that is not the dataset;
/// `504 library_fetch_timeout` past [`FETCH_TIMEOUT`]. Either way nothing is
/// stored and the library in use stays.
pub async fn fetch(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<LibraryMeta>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let text = state.library.fetch().await?;
    let now = db_now(&mut *state.db.pool().acquire().await.map_err(DbError::from)?).await?;
    let (raw, entries) = tokio::task::spawn_blocking(move || {
        let not_dataset = || ApiError::library_unreachable("下载的内容不是可识别的价格数据集");
        let raw = RawValue::from_string(text).map_err(|_| not_dataset())?;
        let parsed = import::parse(&raw, now).map_err(|_| not_dataset())?;
        Ok::<_, ApiError>((raw, entry_count(&parsed)))
    })
    .await
    .map_err(ApiError::internal)??;
    let url = state.library.url.to_string();
    let meta = state
        .db
        .tx(move |conn| {
            async move {
                let (meta, replaced) = replace(
                    &mut *conn,
                    &NewLibrary {
                        origin: "fetch",
                        source_url: Some(&url),
                        source_ref: None,
                        data: raw.get(),
                        entries,
                    },
                    &auth,
                )
                .await?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "pricing.library.update",
                        target_kind: "pricing_library",
                        target_id: Some(meta.id.to_string()),
                        tenant_id: None,
                        detail: json!({
                            "dataset": DATASET,
                            "origin": "fetch",
                            "sha256": meta.sha256,
                            "bytes": meta.bytes,
                            "entries": meta.entries,
                            "replaced": replaced,
                        }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(meta)
            }
            .boxed()
        })
        .await?;
    tracing::info!(
        library = %meta.id,
        bytes = meta.bytes,
        entries = meta.entries,
        "fetched the price library"
    );
    Ok((StatusCode::CREATED, Json(meta)))
}

/// How a model's name found its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// The name is the entry's id, or its `<provider>/<id>`.
    Exact,
    /// Case-insensitively, by the id or an alias, a provider prefix aside.
    Alias,
    /// Of several candidates, the one whose provider is the protocol family
    /// (`adapter`) of an account serving the model.
    Adapter,
    /// The entry an operator chose for the model ([`Binding`]).
    Chosen,
}

/// What a name finds in the library: indexes into its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    None,
    One(usize, MatchedBy),
    /// Every candidate, none of which is picked.
    Many(Vec<usize>),
}

/// The library's entries, looked up by name. See the module documentation.
pub struct Index<'a> {
    entries: &'a [ParsedProposal],
    /// By id and by `<provider>/<id>`, as written.
    exact: HashMap<String, Vec<usize>>,
    /// Lower-cased: by id, alias, either with its provider's prefix, and
    /// either with a provider prefix of its own taken off.
    loose: HashMap<String, Vec<usize>>,
    /// Every provider of the library, lower-cased.
    providers: HashSet<String>,
}

/// `name` split at a leading `<provider>/` that names one of `providers`.
fn provider_prefixed<'n>(name: &'n str, providers: &HashSet<String>) -> Option<(&'n str, &'n str)> {
    let (prefix, rest) = name.split_once('/')?;
    (!rest.is_empty() && providers.contains(&prefix.to_lowercase())).then_some((prefix, rest))
}

impl<'a> Index<'a> {
    pub fn new(entries: &'a [ParsedProposal]) -> Index<'a> {
        let providers: HashSet<String> =
            entries.iter().map(|e| e.provider.to_lowercase()).collect();
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        let mut loose: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            exact.entry(e.model.clone()).or_default().push(i);
            exact
                .entry(format!("{}/{}", e.provider, e.model))
                .or_default()
                .push(i);
            let provider = e.provider.to_lowercase();
            for name in std::iter::once(&e.model).chain(&e.aliases) {
                let name = name.to_lowercase();
                if let Some((_, bare)) = provider_prefixed(&name, &providers) {
                    loose.entry(bare.to_string()).or_default().push(i);
                }
                loose
                    .entry(format!("{provider}/{name}"))
                    .or_default()
                    .push(i);
                loose.entry(name).or_default().push(i);
            }
        }
        Index {
            entries,
            exact,
            loose,
            providers,
        }
    }

    pub fn entry(&self, i: usize) -> &'a ParsedProposal {
        &self.entries[i]
    }

    /// The entry `provider`'s `model` names exactly, if the library holds it.
    pub fn keyed(&self, provider: &str, model: &str) -> Option<usize> {
        self.exact
            .get(&format!("{provider}/{model}"))?
            .iter()
            .copied()
            .find(|i| self.entries[*i].provider == provider && self.entries[*i].model == model)
    }

    /// What model `name`, served through `providers` (its own upstream
    /// providers, as written) by accounts speaking `adapters` (their
    /// protocol families), finds.
    pub fn find(
        &self,
        name: &str,
        providers: &BTreeSet<String>,
        adapters: &BTreeSet<String>,
    ) -> Match {
        let mut own: BTreeSet<String> = providers.iter().map(|p| p.to_lowercase()).collect();
        let folded = name.to_lowercase();
        if let Some((prefix, _)) = provider_prefixed(&folded, &self.providers) {
            own.insert(prefix.to_string());
        }
        let adapters: BTreeSet<String> = adapters.iter().map(|a| a.to_lowercase()).collect();
        if let Some(found) = self.exact.get(name) {
            return self.decide(found.clone(), &own, &adapters, MatchedBy::Exact);
        }
        let found = self.loose.get(&folded).cloned().unwrap_or_default();
        self.decide(found, &own, &adapters, MatchedBy::Alias)
    }

    /// One candidate; or the ones of the model's own providers when that
    /// leaves one; or, of those left, the ones of its accounts' adapters
    /// when that leaves one. Otherwise every candidate left, unpicked.
    fn decide(
        &self,
        mut found: Vec<usize>,
        own: &BTreeSet<String>,
        adapters: &BTreeSet<String>,
        by: MatchedBy,
    ) -> Match {
        found.sort_unstable();
        found.dedup();
        match found.as_slice() {
            [] => return Match::None,
            [one] => return Match::One(*one, by),
            _ => {}
        }
        let theirs = self.narrow(found, own);
        if let [one] = theirs.as_slice() {
            return Match::One(*one, by);
        }
        let spoken = self.narrow(theirs, adapters);
        match spoken.as_slice() {
            [one] => Match::One(*one, MatchedBy::Adapter),
            _ => Match::Many(spoken),
        }
    }

    /// The candidates whose provider is one of `providers` (lower-cased),
    /// or all of them when none is.
    fn narrow(&self, found: Vec<usize>, providers: &BTreeSet<String>) -> Vec<usize> {
        let kept: Vec<usize> = found
            .iter()
            .copied()
            .filter(|i| providers.contains(&self.entries[*i].provider.to_lowercase()))
            .collect();
        if kept.is_empty() {
            found
        } else {
            kept
        }
    }
}

/// A model a baseline can be written on, with the providers it is served
/// through and the protocol families of the accounts serving it.
#[derive(Debug, Clone)]
pub struct Priceable {
    pub id: Uuid,
    pub name: String,
    pub providers: BTreeSet<String>,
    /// The `adapter` of each live account serving it that names one.
    pub adapters: BTreeSet<String>,
}

/// Every priceable model, or those of `only`, by name: its own `provider`
/// (a hand-made direct model), and those of the live direct documents its
/// channels generated (a public model); and the `adapter` of each live
/// account those documents name.
async fn priceable(
    conn: &mut PgConnection,
    only: Option<&[Uuid]>,
) -> Result<Vec<Priceable>, DbError> {
    let mut models: Vec<Priceable> = crate::channels::priceable_models(&mut *conn)
        .await?
        .into_iter()
        .filter(|(id, _)| only.is_none_or(|only| only.contains(id)))
        .map(|(id, name)| Priceable {
            id,
            name,
            providers: BTreeSet::new(),
            adapters: BTreeSet::new(),
        })
        .collect();
    let ids: Vec<Uuid> = models.iter().map(|m| m.id).collect();
    let served: Vec<(Uuid, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT r.id, r.doc->>'provider', k.doc->>'adapter' FROM gw_resources r \
         LEFT JOIN gw_resources k ON k.kind = $3 AND NOT k.deleted \
           AND k.id::text = lower(r.doc->>'provider_key_id') \
         WHERE r.kind = $1 AND NOT r.deleted AND r.id = ANY($2) \
         UNION \
         SELECT cm.public_model_id, d.doc->>'provider', k.doc->>'adapter' FROM channel_models cm \
         JOIN gw_resources d ON d.kind = $1 AND d.id = cm.direct_model_id AND NOT d.deleted \
         LEFT JOIN gw_resources k ON k.kind = $3 AND k.id = cm.provider_key_id AND NOT k.deleted \
         WHERE cm.public_model_id = ANY($2)",
    )
    .bind(Kind::Models.segment())
    .bind(&ids)
    .bind(Kind::ProviderKeys.segment())
    .fetch_all(&mut *conn)
    .await?;
    let mut by_model: HashMap<Uuid, (BTreeSet<String>, BTreeSet<String>)> = HashMap::new();
    for (id, provider, adapter) in served {
        let (providers, adapters) = by_model.entry(id).or_default();
        providers.extend(provider);
        adapters.extend(adapter);
    }
    for model in &mut models {
        (model.providers, model.adapters) = by_model.remove(&model.id).unwrap_or_default();
    }
    Ok(models)
}

/// What a sync would do to one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// No baseline in force: the entry's card would be its first.
    Create,
    /// The entry's card differs from the baseline in force.
    Update,
    /// The entry's card is the baseline in force.
    Same,
    /// The entry carries a flag, or no card: as in the import, it is never
    /// written as it stands.
    Flagged,
    Unmatched,
    Ambiguous,
}

/// One model's place in a sync.
pub struct Planned<'a> {
    pub model: Priceable,
    pub found: Match,
    pub state: SyncState,
    pub current: Option<BaselineRevision>,
    /// The entry an operator chose for it, used only while the library holds
    /// that entry.
    pub binding: Option<Binding>,
    index: &'a Index<'a>,
}

impl Planned<'_> {
    /// The entry it matched, if one.
    pub fn entry(&self) -> Option<&ParsedProposal> {
        match self.found {
            Match::One(i, _) => Some(self.index.entry(i)),
            _ => None,
        }
    }

    /// The entry it would be synced from when listed under `provider`'s
    /// `model`: the one it matched, or the candidate of that name when it is
    /// ambiguous.
    fn listed_as(&self, provider: &str, model: &str) -> Option<&ParsedProposal> {
        let named = |e: &&ParsedProposal| e.provider == provider && e.model == model;
        match &self.found {
            Match::One(i, _) => Some(self.index.entry(*i)).filter(named),
            Match::Many(all) => all.iter().map(|i| self.index.entry(*i)).find(named),
            Match::None => None,
        }
    }

    fn answer(&self) -> Value {
        let (matched_by, candidates) = match &self.found {
            Match::One(_, by) => (Some(*by), Vec::new()),
            Match::Many(all) => (None, all.iter().map(|i| self.index.entry(*i)).collect()),
            Match::None => (None, Vec::new()),
        };
        json!({
            "model_id": self.model.id,
            "name": self.model.name,
            "state": self.state,
            "matched_by": matched_by,
            "entry": self.entry().map(view),
            "candidates": candidates
                .into_iter()
                .map(|c| CandidateView {
                    entry: view(c),
                    input: headline(c, |r| &r.input),
                    output: headline(c, |r| &r.output),
                    state: state_of(c, self.current.as_ref()),
                })
                .collect::<Vec<_>>(),
            "binding": self.binding.as_ref().map(|b| json!({
                "provider": b.provider,
                "model": b.model,
                "chosen_at": b.chosen_at,
                // Ignored once its entry has left the library.
                "in_library": matches!(self.found, Match::One(_, MatchedBy::Chosen)),
            })),
            "current": self.current,
        })
    }
}

/// One candidate of an ambiguous model, with everything choosing it takes:
/// the entry, its headline rates, and what a sync from it would do.
#[derive(Serialize)]
struct CandidateView<'a> {
    #[serde(flatten)]
    entry: EntryView<'a>,
    /// The `default` tier's base input rate, USD per million; `None` without
    /// a card.
    input: Option<String>,
    output: Option<String>,
    state: SyncState,
}

/// One base rate of `entry`'s `default` tier, as written.
fn headline(
    entry: &ParsedProposal,
    rate: impl Fn(&aisix_pricing::Rates) -> &aisix_pricing::Rate,
) -> Option<String> {
    let tier = entry.card.as_ref()?.0.get(&ServiceTier::default_tier())?;
    Some(rate(&tier.base).to_string())
}

/// What a sync would do to each of `models`, against the baselines in force
/// `current` and the entries chosen `bindings`.
pub fn plan<'a>(
    index: &'a Index<'a>,
    models: Vec<Priceable>,
    current: &HashMap<Uuid, BaselineRevision>,
    bindings: &HashMap<Uuid, Binding>,
) -> Vec<Planned<'a>> {
    models
        .into_iter()
        .map(|model| {
            let binding = bindings.get(&model.id).cloned();
            let found = match binding
                .as_ref()
                .and_then(|b| index.keyed(&b.provider, &b.model))
            {
                Some(i) => Match::One(i, MatchedBy::Chosen),
                None => index.find(&model.name, &model.providers, &model.adapters),
            };
            let current = current.get(&model.id).cloned();
            let state = match &found {
                Match::None => SyncState::Unmatched,
                Match::Many(_) => SyncState::Ambiguous,
                Match::One(i, _) => state_of(index.entry(*i), current.as_ref()),
            };
            Planned {
                model,
                found,
                state,
                current,
                binding,
                index,
            }
        })
        .collect()
}

/// Whether `entry`'s card would create, update or leave the baseline in
/// force `current`. A stored card this engine cannot read equals nothing.
fn state_of(entry: &ParsedProposal, current: Option<&BaselineRevision>) -> SyncState {
    let Some(card) = entry.card.as_ref().filter(|_| entry.flags.is_empty()) else {
        return SyncState::Flagged;
    };
    match current {
        None => SyncState::Create,
        Some(current) => {
            let stored = serde_json::from_value::<RateCard>(current.card.clone()).ok();
            if stored.as_ref() == Some(card) {
                SyncState::Same
            } else {
                SyncState::Update
            }
        }
    }
}

/// The baselines in force for `models`, by model.
async fn in_force(
    conn: &mut PgConnection,
    models: &[Priceable],
) -> Result<HashMap<Uuid, BaselineRevision>, DbError> {
    let ids: Vec<Uuid> = models.iter().map(|m| m.id).collect();
    Ok(baselines::current_baselines(conn, &ids)
        .await?
        .into_iter()
        .map(|r| (r.model_id, r))
        .collect())
}

/// The entry an operator chose for a model the library matched ambiguously,
/// as `pricing_library_bindings` holds it.
#[derive(Debug, Clone)]
pub struct Binding {
    pub provider: String,
    pub model: String,
    pub chosen_at: DateTime<Utc>,
}

/// The choices recorded for `models`, by model.
async fn bindings(
    conn: &mut PgConnection,
    models: &[Priceable],
) -> Result<HashMap<Uuid, Binding>, DbError> {
    let ids: Vec<Uuid> = models.iter().map(|m| m.id).collect();
    let rows: Vec<(Uuid, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT model_id, provider, model, chosen_at FROM pricing_library_bindings \
         WHERE model_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, provider, model, chosen_at)| {
            (
                id,
                Binding {
                    provider,
                    model,
                    chosen_at,
                },
            )
        })
        .collect())
}

/// `GET /admin/pricing/library/sync`'s query.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewQuery {
    /// One model only.
    #[serde(default)]
    model_id: Option<Uuid>,
}

/// `GET /admin/pricing/library/sync?model_id=` → `{library, cap, items}`:
/// every priceable model (or `model_id` alone) with what a sync would do to
/// it (`state`), the entry it matched and how (`entry`, `matched_by`), every
/// candidate of an ambiguous one with its headline rates and the state a
/// sync from it would have (`candidates`), the entry an operator chose for
/// it and whether the library still holds it (`binding`), and its baseline
/// in force (`current`). `cap` is the most one sync writes. With no library,
/// `library` is `null` and `items` empty. A `model_id` naming no priceable
/// model is `404`.
pub async fn preview(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<PreviewQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let library = current(&mut conn).await?;
    let only = q.model_id.map(|id| vec![id]);
    let (models, current_baselines, chosen, now) = {
        let models = priceable(&mut conn, only.as_deref()).await?;
        let current_baselines = in_force(&mut conn, &models).await?;
        let chosen = bindings(&mut conn, &models).await?;
        (models, current_baselines, chosen, db_now(&mut conn).await?)
    };
    drop(conn);
    if q.model_id.is_some() && models.is_empty() {
        return Err(ApiError::not_found());
    }
    let Some((meta, data)) = library else {
        return Ok(Json(json!({
            "library": null,
            "cap": MAX_SYNC_MODELS,
            "items": [],
        })));
    };
    let entries = entries_at(data, now).await?;
    let index = Index::new(&entries);
    let items: Vec<Value> = plan(&index, models, &current_baselines, &chosen)
        .iter()
        .map(Planned::answer)
        .collect();
    Ok(Json(json!({
        "library": meta,
        "cap": MAX_SYNC_MODELS,
        "items": items,
    })))
}

/// `POST /admin/pricing/library/sync`'s body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncBody {
    /// The library the preview was read from.
    library_id: Uuid,
    /// The operator's attestation that the prices were checked against each
    /// provider's page. Must be `true`.
    #[serde(default)]
    attested: Option<bool>,
    /// The models to write, each as the preview showed it.
    #[serde(default)]
    items: Vec<SyncItem>,
}

/// One model of a sync, as its preview showed it: the entry matched (or, for
/// an ambiguous model, the candidate chosen), and the card the operator
/// attested.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncItem {
    model_id: Uuid,
    provider: String,
    model: String,
    card: Value,
}

/// `POST /admin/pricing/library/sync`, body `{library_id, attested, items:
/// [{model_id, provider, model, card}]}` → `201 {revisions, warnings}`.
///
/// Refused in this order, each before anything is written:
/// 1. `422 /attested` unless `attested` is exactly `true`, with
///    "请确认已对照官方价格页核对", before anything is read.
/// 2. `422 /items` for no item or more than [`MAX_SYNC_MODELS`];
///    `422 /items/<n>/model_id` for a model listed twice;
///    `422 /items/<n>/card` for a card that is not one.
/// 3. `409 library_stale` when `library_id` is not the library in use, and
///    when any item no longer previews as listed: a model no longer
///    priceable, matched to another entry or none (an ambiguous model: a
///    `provider`/`model` that is none of its candidates), no longer `create`
///    or `update` from that entry, or whose entry's card is not the card
///    sent. `details` names each such item at `/items/<n>`.
///
/// Otherwise one transaction holds the library's row, writes each listed
/// item's card as a `third_party` baseline starting now, naming the library,
/// its entry, the dataset's pricing page and checked date, and this operator's
/// attestation, and records `pricing.library.sync` per model (the attester,
/// `attested: true` and the sentence affirmed) beside the baseline's own
/// `pricing.baseline.create`. An item that resolves an ambiguous model also
/// records the candidate chosen as the model's binding
/// (`pricing.library.bind`). A model not listed is never written, whatever
/// its preview says now.
pub async fn sync(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SyncBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    if body.attested != Some(true) {
        return Err(refused("/attested", format!("请确认{ATTESTATION}")));
    }
    if body.items.is_empty() {
        return Err(refused("/items", "请选择要同步的模型"));
    }
    if body.items.len() > MAX_SYNC_MODELS {
        return Err(refused(
            "/items",
            format!("一次最多同步 {MAX_SYNC_MODELS} 个模型"),
        ));
    }
    let mut seen = HashSet::new();
    let mut cards = Vec::with_capacity(body.items.len());
    for (n, item) in body.items.iter().enumerate() {
        if !seen.insert(item.model_id) {
            return Err(refused(
                &format!("/items/{n}/model_id"),
                "同一模型只能出现一次",
            ));
        }
        let card: RateCard = serde_json::from_value(item.card.clone())
            .map_err(|e| refused(&format!("/items/{n}/card"), format!("价目卡不合法：{e}")))?;
        cards.push(card);
    }
    let library = {
        let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
        let library = current(&mut conn).await?;
        let now = db_now(&mut conn).await?;
        library.map(|l| (l, now))
    };
    let Some(((meta, data), now)) = library.filter(|((l, _), _)| l.id == body.library_id) else {
        return Err(ApiError::library_stale(vec![Detail {
            path: "/library_id".to_string(),
            message: "价格库已更新".to_string(),
        }]));
    };
    let entries = entries_at(data, now).await?;
    let written = state
        .db
        .tx(move |conn| {
            async move {
                // Held to commit: a replacement of the library waits for
                // this sync, and a second sync of it waits too, then sees
                // what this one wrote.
                let held: Option<Uuid> =
                    sqlx::query_scalar("SELECT id FROM pricing_library WHERE id = $1 FOR UPDATE")
                        .bind(meta.id)
                        .fetch_optional(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                if held.is_none() {
                    return Err(ApiError::library_stale(vec![Detail {
                        path: "/library_id".to_string(),
                        message: "价格库已更新".to_string(),
                    }]));
                }
                let ids: Vec<Uuid> = body.items.iter().map(|i| i.model_id).collect();
                let models = priceable(&mut *conn, Some(&ids)).await?;
                let current_baselines = in_force(&mut *conn, &models).await?;
                let chosen = bindings(&mut *conn, &models).await?;
                let index = Index::new(&entries);
                let planned = plan(&index, models, &current_baselines, &chosen);
                let by_id: HashMap<Uuid, &Planned<'_>> =
                    planned.iter().map(|p| (p.model.id, p)).collect();
                let mut moved = Vec::new();
                let mut writes = Vec::with_capacity(body.items.len());
                for (n, (item, card)) in body.items.iter().zip(&cards).enumerate() {
                    let path = format!("/items/{n}");
                    let Some(p) = by_id.get(&item.model_id) else {
                        moved.push(Detail {
                            path,
                            message: "模型已不存在或不可定价".to_string(),
                        });
                        continue;
                    };
                    let Some(entry) = p.listed_as(&item.provider, &item.model) else {
                        moved.push(Detail {
                            path,
                            message: format!("{}：匹配的价格库条目已变化", p.model.name),
                        });
                        continue;
                    };
                    if !matches!(
                        state_of(entry, p.current.as_ref()),
                        SyncState::Create | SyncState::Update
                    ) {
                        moved.push(Detail {
                            path,
                            message: format!("{}：已无需同步", p.model.name),
                        });
                        continue;
                    }
                    if entry.card.as_ref() != Some(card) {
                        moved.push(Detail {
                            path,
                            message: format!("{}：价格库中的价格已变化", p.model.name),
                        });
                        continue;
                    }
                    let chooses = matches!(p.found, Match::Many(_));
                    writes.push((p.model.id, entry, card.clone(), chooses));
                }
                if !moved.is_empty() {
                    return Err(ApiError::library_stale(moved));
                }
                let now = db_now(&mut *conn).await?;
                let reference = meta.reference();
                let mut revisions = Vec::with_capacity(writes.len());
                let mut warnings = Vec::new();
                for (model_id, entry, card, chooses) in writes {
                    let library_entry = format!("{}/{}", entry.provider, entry.model);
                    let row = NewBaseline {
                        model_id,
                        effective_from: now,
                        card,
                        source: BaselineSource::ThirdParty {
                            dataset: DATASET.to_string(),
                            dataset_ref: reference.clone(),
                            pricing_url: entry.pricing_url.clone(),
                            prices_checked: entry.prices_checked,
                            import_id: None,
                            proposal_id: None,
                            library_id: Some(meta.id),
                            library_entry: Some(library_entry.clone()),
                            attested_by: auth.id,
                            attested_at: now,
                        },
                    };
                    let (revision, more) =
                        baselines::insert_baseline(&mut *conn, &row, Actor::Operator(&auth))
                            .await?;
                    audit::record(
                        &mut *conn,
                        AuditEntry {
                            actor: Actor::Operator(&auth),
                            action: "pricing.library.sync",
                            target_kind: "model",
                            target_id: Some(model_id.to_string()),
                            tenant_id: None,
                            detail: json!({
                                "model_id": model_id,
                                "library_id": meta.id,
                                "library_entry": library_entry,
                                "revision_id": revision.revision_id,
                                "attested": true,
                                // The sentence the operator affirmed, as the
                                // import's confirmation records it.
                                "attestation": ATTESTATION,
                            }),
                        },
                    )
                    .await?;
                    if chooses {
                        bind(&mut *conn, model_id, entry, &auth, now).await?;
                    }
                    revisions.push(revision);
                    warnings.extend(more);
                }
                Ok::<_, ApiError>((revisions, warnings))
            }
            .boxed()
        })
        .await?;
    let (revisions, warnings) = written;
    tracing::info!(
        models = revisions.len(),
        "synced baselines from the price library"
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({"revisions": revisions, "warnings": warnings})),
    ))
}

/// Records `entry` as the one chosen for `model_id`, replacing an earlier
/// choice (one whose entry left the library), with its audit row, in the
/// sync's transaction.
async fn bind(
    conn: &mut PgConnection,
    model_id: Uuid,
    entry: &ParsedProposal,
    auth: &OperatorAuth,
    now: DateTime<Utc>,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO pricing_library_bindings (model_id, provider, model, chosen_by, chosen_at) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (model_id) DO UPDATE SET \
           provider = EXCLUDED.provider, model = EXCLUDED.model, \
           chosen_by = EXCLUDED.chosen_by, chosen_at = EXCLUDED.chosen_at",
    )
    .bind(model_id)
    .bind(&entry.provider)
    .bind(&entry.model)
    .bind(auth.id)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?;
    audit::record(
        &mut *conn,
        AuditEntry {
            actor: Actor::Operator(auth),
            action: "pricing.library.bind",
            target_kind: "model",
            target_id: Some(model_id.to_string()),
            tenant_id: None,
            detail: json!({
                "model_id": model_id,
                "provider": entry.provider,
                "model": entry.model,
            }),
        },
    )
    .await?;
    Ok(())
}

/// `DELETE /admin/pricing/library/bindings/{model_id}` → `204`: forgets the
/// entry chosen for the model (`pricing.library.unbind`), which is matched by
/// its name again from then on. `404` when no choice is recorded for it.
pub async fn unbind(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let model_id = Uuid::parse_str(&raw).map_err(|_| ApiError::not_found())?;
    state
        .db
        .tx(move |conn| {
            async move {
                let gone: Option<(String, String)> = sqlx::query_as(
                    "DELETE FROM pricing_library_bindings WHERE model_id = $1 \
                     RETURNING provider, model",
                )
                .bind(model_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let Some((provider, model)) = gone else {
                    return Err(ApiError::not_found());
                };
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "pricing.library.unbind",
                        target_kind: "model",
                        target_id: Some(model_id.to_string()),
                        tenant_id: None,
                        detail: json!({
                            "model_id": model_id,
                            "provider": provider,
                            "model": model,
                        }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(())
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A `422` at `path`.
fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    /// 2026-11-08 12:00 UTC.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 8, 12, 0, 0).unwrap()
    }

    /// The entries `providers` (JSON text: a dataset) holds now.
    fn entries(providers: &str) -> Vec<ParsedProposal> {
        let raw = RawValue::from_string(providers.to_string()).expect("JSON");
        import::parse(&raw, now())
            .expect("a dataset")
            .into_iter()
            .filter(|p| p.effective_from.is_none())
            .collect()
    }

    /// A model `id` priced at 1 and 2, with `matches` as its `match` clause.
    fn model(id: &str, matches: &str) -> String {
        format!(r#"{{"id":"{id}","match":{matches},"prices":{{"input_mtok":1,"output_mtok":2}}}}"#)
    }

    fn provider(id: &str, models: &[String]) -> String {
        format!(r#"{{"id":"{id}","models":[{}]}}"#, models.join(","))
    }

    fn dataset(providers: &[String]) -> String {
        format!("[{}]", providers.join(","))
    }

    fn own(providers: &[&str]) -> BTreeSet<String> {
        providers.iter().map(|p| p.to_string()).collect()
    }

    /// What `name` served through `providers` finds, as `provider/model`
    /// and how, or the candidates.
    fn found(entries: &[ParsedProposal], name: &str, providers: &[&str]) -> String {
        spoken(entries, name, providers, &[])
    }

    /// [`found`], by accounts speaking `adapters`.
    fn spoken(
        entries: &[ParsedProposal],
        name: &str,
        providers: &[&str],
        adapters: &[&str],
    ) -> String {
        let index = Index::new(entries);
        let key = |i: usize| format!("{}/{}", entries[i].provider, entries[i].model);
        match index.find(name, &own(providers), &own(adapters)) {
            Match::None => "none".to_string(),
            Match::One(i, by) => format!("{} by {by:?}", key(i)),
            Match::Many(all) => format!(
                "many: {}",
                all.into_iter().map(key).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    #[test]
    fn a_name_that_is_an_entrys_id_matches_it_exactly() {
        let e = entries(&dataset(&[
            provider("openai", &[model("gpt-x", r#"{"equals":"gpt-x"}"#)]),
            // Its alias is `gpt-x` too: the exact id wins, and the alias is
            // never weighed beside it.
            provider(
                "other",
                &[model("gpt-x-other", r#"{"or":[{"equals":"gpt-x"}]}"#)],
            ),
        ]));
        assert_eq!(found(&e, "gpt-x", &[]), "openai/gpt-x by Exact");
        assert_eq!(found(&e, "openai/gpt-x", &[]), "openai/gpt-x by Exact");
    }

    #[test]
    fn an_alias_the_datasets_match_lists_matches_case_insensitively() {
        let e = entries(&dataset(&[provider(
            "cohere",
            &[model(
                "command-a",
                r#"{"or":[{"equals":"command-a"},{"equals":"command-a-03-2025"},{"starts_with":"command-a-x"}]}"#,
            )],
        )]));
        assert_eq!(
            found(&e, "command-a-03-2025", &[]),
            "cohere/command-a by Alias"
        );
        assert_eq!(found(&e, "Command-A", &[]), "cohere/command-a by Alias");
        // A pattern names no one model.
        assert_eq!(found(&e, "command-a-x", &[]), "none");
        assert_eq!(found(&e, "command-a-xl", &[]), "none");
    }

    #[test]
    fn a_provider_prefix_on_either_side_may_be_left_off() {
        let e = entries(&dataset(&[
            provider(
                "deepseek",
                &[model("deepseek-chat", r#"{"equals":"deepseek-chat"}"#)],
            ),
            // An aggregator's id carries a provider's prefix of its own.
            provider(
                "openrouter",
                &[model(
                    "mistral/mistral-tiny",
                    r#"{"equals":"mistral/mistral-tiny"}"#,
                )],
            ),
            provider(
                "mistral",
                &[model("mistral-large", r#"{"equals":"mistral-large"}"#)],
            ),
            // A prefix that is no provider of the library is part of the name.
            provider(
                "together",
                &[model(
                    "meta-llama/Llama-3",
                    r#"{"equals":"meta-llama/Llama-3"}"#,
                )],
            ),
        ]));
        assert_eq!(
            found(&e, "DeepSeek/DeepSeek-Chat", &[]),
            "deepseek/deepseek-chat by Alias"
        );
        assert_eq!(
            found(&e, "mistral-tiny", &[]),
            "openrouter/mistral/mistral-tiny by Alias"
        );
        assert_eq!(found(&e, "llama-3", &[]), "none");
        // A prefix on the model's name says whose it is: it never reaches
        // another provider's entry of the same id.
        assert_eq!(found(&e, "mistral/deepseek-chat", &[]), "none");
    }

    #[test]
    fn several_candidates_are_never_picked_unless_one_is_the_models_own_providers() {
        let e = entries(&dataset(&[
            provider("openai", &[model("m", r#"{"equals":"m"}"#)]),
            provider("azure", &[model("m", r#"{"equals":"m"}"#)]),
            provider("github", &[model("m", r#"{"equals":"m"}"#)]),
        ]));
        assert_eq!(found(&e, "m", &[]), "many: openai/m, azure/m, github/m");
        // A provider the library does not know narrows nothing.
        assert_eq!(
            found(&e, "m", &["openai_compatible"]),
            "many: openai/m, azure/m, github/m"
        );
        assert_eq!(found(&e, "m", &["OpenAI"]), "openai/m by Exact");
        // Two of the model's own still leave a choice nobody made.
        assert_eq!(
            found(&e, "m", &["openai", "azure"]),
            "many: openai/m, azure/m"
        );
        // A provider prefix on the name is the model's own provider.
        assert_eq!(found(&e, "azure/m", &[]), "azure/m by Exact");
    }

    #[test]
    fn an_accounts_adapter_picks_among_candidates_its_own_provider_left() {
        let e = entries(&dataset(&[
            provider("anthropic", &[model("c", r#"{"equals":"c"}"#)]),
            provider("aws", &[model("c", r#"{"equals":"c"}"#)]),
            provider("openrouter", &[model("c", r#"{"equals":"c"}"#)]),
        ]));
        // A provider id of the platform's own narrows nothing; the protocol
        // its account speaks names the first-party entry.
        assert_eq!(
            found(&e, "c", &["matrix"]),
            "many: anthropic/c, aws/c, openrouter/c"
        );
        assert_eq!(
            spoken(&e, "c", &["matrix"], &["anthropic"]),
            "anthropic/c by Adapter"
        );
        assert_eq!(
            spoken(&e, "c", &["matrix"], &["Anthropic"]),
            "anthropic/c by Adapter"
        );
        // The model's own provider is weighed first, and wins over it.
        assert_eq!(spoken(&e, "c", &["aws"], &["anthropic"]), "aws/c by Exact");
        // Two of the model's own providers: the adapter picks between them.
        assert_eq!(
            spoken(&e, "c", &["aws", "anthropic"], &["anthropic"]),
            "anthropic/c by Adapter"
        );
        // An adapter no candidate's provider is narrows nothing, and two
        // adapters that each name a candidate still leave a choice.
        assert_eq!(
            spoken(&e, "c", &["matrix"], &["openai"]),
            "many: anthropic/c, aws/c, openrouter/c"
        );
        assert_eq!(
            spoken(&e, "c", &["matrix"], &["anthropic", "openrouter"]),
            "many: anthropic/c, openrouter/c"
        );
        // One candidate needs no narrowing, and keeps how its name found it.
        let one = entries(&dataset(&[provider(
            "aws",
            &[model("d", r#"{"equals":"d"}"#)],
        )]));
        assert_eq!(
            spoken(&one, "d", &["matrix"], &["anthropic"]),
            "aws/d by Exact"
        );
    }

    /// The committed snapshot of the real dataset, as names a platform
    /// actually carries find it.
    #[test]
    fn the_real_datasets_names_find_their_own_providers_entries() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/e2e/fixtures/pricing-import/genai-prices-v2-7de757d.json"
        ))
        .expect("the snapshot");
        let started = std::time::Instant::now();
        let e = entries(&text);
        let index = Index::new(&e);
        eprintln!(
            "{} entries ({} without a flag) read and indexed in {:?}",
            e.len(),
            e.iter()
                .filter(|p| p.card.is_some() && p.flags.is_empty())
                .count(),
            started.elapsed()
        );
        let key = |m: Match| match m {
            Match::One(i, by) => format!("{}/{} by {by:?}", e[i].provider, e[i].model),
            other => format!("{other:?}"),
        };
        // One provider carries it: found whoever serves it.
        assert_eq!(
            key(index.find("command-a", &own(&[]), &own(&[]))),
            "cohere/command-a by Exact"
        );
        // Two carry it: only the model's own provider decides.
        assert!(matches!(
            index.find("gpt-4o", &own(&[]), &own(&[])),
            Match::Many(_)
        ));
        assert_eq!(
            key(index.find("gpt-4o", &own(&["openai"]), &own(&[]))),
            "openai/gpt-4o by Exact"
        );
        assert_eq!(
            key(index.find("openai/gpt-4o", &own(&[]), &own(&[]))),
            "openai/gpt-4o by Exact"
        );
        // A dated snapshot name is one of the entry's own aliases.
        assert_eq!(
            key(index.find("gpt-4o-2024-08-06", &own(&["openai"]), &own(&[]))),
            "openai/gpt-4o by Alias"
        );
        // An account under the platform's own provider id, speaking a
        // first party's protocol, finds that party's entry among the cloud
        // and aggregator copies of the name.
        assert!(matches!(
            index.find("claude-opus-4-5", &own(&["matrix"]), &own(&[])),
            Match::Many(_)
        ));
        assert_eq!(
            key(index.find("claude-opus-4-5", &own(&["matrix"]), &own(&["anthropic"]))),
            "anthropic/claude-opus-4-5 by Adapter"
        );
        assert_eq!(
            key(index.find("gpt-4o", &own(&["matrix"]), &own(&["openai"]))),
            "openai/gpt-4o by Adapter"
        );
    }

    #[test]
    fn a_choice_wins_while_the_library_holds_its_entry() {
        let e = entries(&dataset(&[
            provider("openai", &[model("m", r#"{"equals":"m"}"#)]),
            provider("azure", &[model("m", r#"{"equals":"m"}"#)]),
        ]));
        let index = Index::new(&e);
        let model = Priceable {
            id: Uuid::new_v4(),
            name: "m".to_string(),
            providers: own(&["relay"]),
            adapters: BTreeSet::new(),
        };
        let chose = |provider: &str| {
            HashMap::from([(
                model.id,
                Binding {
                    provider: provider.to_string(),
                    model: "m".to_string(),
                    chosen_at: now(),
                },
            )])
        };
        let none = HashMap::new();
        let planned = plan(&index, vec![model.clone()], &none, &HashMap::new());
        assert_eq!(planned[0].found, Match::Many(vec![0, 1]));
        assert_eq!(planned[0].state, SyncState::Ambiguous);
        let planned = plan(&index, vec![model.clone()], &none, &chose("azure"));
        assert_eq!(planned[0].found, Match::One(1, MatchedBy::Chosen));
        assert_eq!(planned[0].state, SyncState::Create);
        assert_eq!(planned[0].answer()["binding"]["in_library"], json!(true));
        // Its entry left the library: ignored, and said so.
        let planned = plan(&index, vec![model.clone()], &none, &chose("github"));
        assert_eq!(planned[0].found, Match::Many(vec![0, 1]));
        assert_eq!(planned[0].answer()["binding"]["in_library"], json!(false));
    }

    #[test]
    fn each_candidate_carries_its_rates_page_card_and_state() {
        let e = entries(&dataset(&[
            r#"{"id":"openai","pricing_urls":["https://openai.example/pricing"],"models":[{"id":"m","match":{"equals":"m"},"prices":{"input_mtok":2.5,"output_mtok":10}}]}"#.to_string(),
            provider("azure", &[model("m", r#"{"equals":"m"}"#)]),
        ]));
        let index = Index::new(&e);
        let model = Priceable {
            id: Uuid::new_v4(),
            name: "m".to_string(),
            providers: BTreeSet::new(),
            adapters: BTreeSet::new(),
        };
        // In force: azure's own card, so syncing from it changes nothing.
        let azure = serde_json::to_value(e[1].card.as_ref().expect("a card")).expect("JSON");
        let current = HashMap::from([(model.id, baseline(azure))]);
        let answer = plan(&index, vec![model], &current, &HashMap::new())[0].answer();
        let candidates = answer["candidates"].as_array().expect("candidates");
        assert_eq!(candidates.len(), 2, "{answer}");
        let openai = &candidates[0];
        assert_eq!(openai["provider"], "openai");
        assert_eq!(openai["model"], "m");
        assert_eq!(openai["input"], "2.5");
        assert_eq!(openai["output"], "10");
        assert_eq!(openai["pricing_url"], "https://openai.example/pricing");
        assert_eq!(openai["card"]["default"]["base"]["input"], "2.5");
        assert_eq!(openai["state"], "update");
        assert_eq!(candidates[1]["provider"], "azure");
        assert_eq!(candidates[1]["state"], "same");
    }

    /// A baseline in force with `card`.
    fn baseline(card: Value) -> BaselineRevision {
        BaselineRevision {
            revision_id: 1,
            engine_version: 1,
            model_id: Uuid::nil(),
            effective_from: now(),
            card,
            source_kind: "manual".to_string(),
            source: json!({"note": "n"}),
            created_by: None,
            created_at: now(),
        }
    }

    #[test]
    fn a_baseline_equal_to_the_entrys_card_is_the_same_and_a_flag_is_never_synced() {
        let e = entries(&dataset(&[provider(
            "p",
            &[
                model("m", r#"{"equals":"m"}"#),
                r#"{"id":"odd","prices":{"input_mtok":1,"output_mtok":2,"storage_mtok":3}}"#
                    .to_string(),
            ],
        )]));
        let card = serde_json::to_value(e[0].card.as_ref().expect("a card")).expect("JSON");
        assert_eq!(state_of(&e[0], None), SyncState::Create);
        assert_eq!(state_of(&e[0], Some(&baseline(card))), SyncState::Same);
        assert_eq!(
            state_of(
                &e[0],
                Some(&baseline(
                    json!({"default": {"base": {"input": "1", "output": "3"}}})
                ))
            ),
            SyncState::Update
        );
        // A stored card this engine cannot read equals nothing.
        assert_eq!(
            state_of(&e[0], Some(&baseline(json!({"default": "?"})))),
            SyncState::Update
        );
        assert_eq!(state_of(&e[1], None), SyncState::Flagged);
    }
}
