//! Cost rates: what each upstream account (a provider key) costs the
//! platform, in CNY per USD of a model's baseline, per model, with the
//! account's model-less row as its default.
//!
//! The same unit, range and validation as a group rate, and the same table
//! shape (`pricing_cost_rates`, migration `0032`), read by the same code
//! ([`rates::plan`], [`rates::superseded_later`], [`rates::priceable_model`]).
//! A charge's cost is its own baseline lines at the serving account's cost
//! rate in force at its price instant (`aisix_pricing::price_event`, step
//! 14); a row costed at no rate has an unknown cost, never 0.
//!
//! A cost rate is append-only like every price: a trigger refuses `UPDATE`,
//! `DELETE` and `TRUNCATE`, a revision starts now or later
//! (`store::starts_now_or_later`), and each write bumps `pricing_version` and
//! writes its audit row in its caller's transaction. A model row with no
//! rate ends that model's override from its start on, so the account's
//! default applies again. Only the default row must carry a rate.
//!
//! # What a cost rate keys on
//!
//! The account a usage row names (`provider_key_id`, the account that
//! served it) and, for an override, the requested model: the entry the
//! caller addresses, which is the model the baseline and the group rate key
//! on. For a channel's generated direct document that entry is its public
//! model, so an override is written on the public model and refused on the
//! direct document, exactly as a group rate is.
//!
//! # Locks
//!
//! A write takes no row lock and never the resources lock: it reads that
//! the account is live, and a revision for an account deleted meanwhile
//! never applies. The `pricing_version` bump serializes price writes, as it
//! does for every other price table.

use aisix_pricing::{CnyPerUsd, ENGINE_VERSION};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::api::PriceWarning;
use super::baselines::OperatorRef;
use super::rates::{self, RatePlan, RateTable};
use super::store::{bump_pricing_version, created_by, refused, starts_now_or_later};
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::resources::Kind;
use crate::AppState;

/// The most characters a cost rate's note holds, as a group rate's.
const MAX_NOTE_CHARS: usize = 500;

/// The accounts' cost rates.
const COST_RATES: RateTable = RateTable {
    table: "pricing_cost_rates",
    owner: "provider_key_id",
};

/// A cost-rate revision to write.
#[derive(Debug, Clone)]
pub struct NewCostRate {
    pub provider_key_id: Uuid,
    /// `None` is the account's default.
    pub model_id: Option<Uuid>,
    pub effective_from: DateTime<Utc>,
    /// `None` only on a model row, where it ends that model's override.
    pub cny_per_usd: Option<CnyPerUsd>,
    pub note: String,
}

/// One `pricing_cost_rates` row, with the email of the operator who wrote
/// it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostRateRevision {
    pub revision_id: i64,
    pub engine_version: i64,
    pub provider_key_id: Uuid,
    pub model_id: Option<Uuid>,
    pub effective_from: DateTime<Utc>,
    /// As the decimal string it was typed as; `null` on a model row that
    /// ends an override.
    pub cny_per_usd: Option<String>,
    pub note: String,
    /// `None` when the control plane itself wrote it.
    pub created_by: Option<OperatorRef>,
    pub created_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, PgRow> for CostRateRevision {
    fn from_row(row: &'r PgRow) -> Result<CostRateRevision, sqlx::Error> {
        let created_by: Option<Uuid> = row.try_get("created_by")?;
        let email: Option<String> = row.try_get("created_by_email")?;
        Ok(CostRateRevision {
            revision_id: row.try_get("revision_id")?,
            engine_version: row.try_get("engine_version")?,
            provider_key_id: row.try_get("provider_key_id")?,
            model_id: row.try_get("model_id")?,
            effective_from: row.try_get("effective_from")?,
            cny_per_usd: rates::rate_text(row.try_get("cny_per_usd_e4")?),
            note: row.try_get("note")?,
            created_by: created_by
                .zip(email)
                .map(|(id, email)| OperatorRef { id, email }),
            created_at: row.try_get("created_at")?,
        })
    }
}

/// The cost-rate plan of `provider_key_id` for `model_id` (`None`: the
/// account's default), read as the price book reads it.
pub async fn cost_rate_plan(
    conn: &mut PgConnection,
    provider_key_id: Uuid,
    model_id: Option<Uuid>,
) -> Result<RatePlan<CostRateRevision>, DbError> {
    rates::plan(conn, COST_RATES, provider_key_id, model_id).await
}

/// An upstream account as the cost-rate routes name it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Account {
    pub id: Uuid,
    pub display_name: String,
    pub provider: String,
}

/// The columns an [`Account`] is read from, over `gw_resources`.
const ACCOUNT_COLUMNS: &str = "id, COALESCE(doc->>'display_name', '') AS display_name, \
                               COALESCE(doc->>'provider', '') AS provider";

/// The live account `id`; `404` when no live provider key has that id.
async fn live_account(conn: &mut PgConnection, id: Uuid) -> Result<Account, ApiError> {
    sqlx::query_as(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM gw_resources WHERE kind = $1 AND id = $2 AND NOT deleted"
    ))
    .bind(Kind::ProviderKeys.segment())
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?
    .ok_or_else(ApiError::not_found)
}

/// Writes a cost-rate revision starting at `row.effective_from`, refusing
/// with:
/// `404` for an account that is not live;
/// `422 /effective_from` for one that starts in the past;
/// `422 /note` for a note over 500 characters;
/// `422 /model_id` for a model that is not live, and for a channel's
/// generated direct document, which is costed through its public model;
/// `422 /cny_per_usd` for a default row with no rate.
///
/// The revision, the `pricing_version` bump and the audit row
/// `pricing.cost_rate.create` (or `pricing.cost_rate.end_override`) go in the
/// caller's transaction. Each revision of the same (account, model) already
/// there that starts later takes over from it then; each is named in a
/// `superseded_later` warning, and the write still succeeds.
pub async fn insert_cost_rate(
    conn: &mut PgConnection,
    row: &NewCostRate,
    actor: Actor<'_>,
) -> Result<(CostRateRevision, Vec<PriceWarning>), ApiError> {
    live_account(&mut *conn, row.provider_key_id).await?;
    starts_now_or_later(&mut *conn, row.effective_from).await?;
    if row.note.chars().count() > MAX_NOTE_CHARS {
        return Err(refused("/note", "说明最多 500 个字符"));
    }
    if let Some(model_id) = row.model_id {
        rates::priceable_model(&mut *conn, model_id, "成本换算率").await?;
    } else if row.cny_per_usd.is_none() {
        return Err(refused("/cny_per_usd", "默认成本换算率不能为空"));
    }
    let revision: CostRateRevision = sqlx::query_as(&format!(
        "WITH r AS ( \
           INSERT INTO pricing_cost_rates \
           (engine_version, provider_key_id, model_id, effective_from, cny_per_usd_e4, note, \
            created_by) \
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *) \
         SELECT {} FROM r LEFT JOIN operators o ON o.id = r.created_by",
        COST_RATES.columns()
    ))
    .bind(i64::from(ENGINE_VERSION))
    .bind(row.provider_key_id)
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
                "pricing.cost_rate.create"
            } else {
                "pricing.cost_rate.end_override"
            },
            target_kind: "pricing_cost_rate",
            target_id: Some(revision.revision_id.to_string()),
            tenant_id: None,
            detail: json!({
                "provider_key_id": row.provider_key_id,
                "model_id": row.model_id,
                "effective_from": row.effective_from,
                "cny_per_usd_e4": row.cny_per_usd.map(|c| c.0),
            }),
        },
    )
    .await?;
    let warnings = rates::superseded_later(
        &mut *conn,
        COST_RATES,
        row.provider_key_id,
        row.model_id,
        row.effective_from,
    )
    .await?;
    Ok((revision, warnings))
}

/// The models account `id` is costed on, as `(id, display name)` by name
/// then id: every live model a price can be written on
/// (`channels::priceable_models`' test) that the account serves — a public
/// model with a channel on it, or a hand-written model naming it — or that
/// already carries an override of the account's.
async fn account_models(conn: &mut PgConnection, id: Uuid) -> Result<Vec<(Uuid, String)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT r.id, COALESCE(r.doc->>'display_name', '') FROM gw_resources r \
         WHERE r.kind = $1 AND NOT r.deleted \
           AND NOT EXISTS (SELECT 1 FROM channel_models cm WHERE cm.direct_model_id = r.id) \
           AND (EXISTS (SELECT 1 FROM channel_models cm \
                        WHERE cm.public_model_id = r.id AND cm.provider_key_id = $2) \
                OR r.doc->>'provider_key_id' = $2::text \
                OR EXISTS (SELECT 1 FROM pricing_cost_rates c \
                           WHERE c.provider_key_id = $2 AND c.model_id = r.id)) \
         ORDER BY 2, 1",
    )
    .bind(Kind::Models.segment())
    .bind(id)
    .fetch_all(conn)
    .await?)
}

/// `GET /admin/pricing/cost-rates` → `{items: [{account, default}]}`: every
/// live upstream account by display name then id, each with its default
/// cost-rate plan (the rate in force and the ones scheduled).
pub async fn list_accounts(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let items = state
        .db
        .tx(move |conn| {
            async move {
                let accounts: Vec<Account> = sqlx::query_as(&format!(
                    "SELECT {ACCOUNT_COLUMNS} FROM gw_resources WHERE kind = $1 AND NOT deleted \
                     ORDER BY 2, 1"
                ))
                .bind(Kind::ProviderKeys.segment())
                .fetch_all(&mut *conn)
                .await?;
                let mut items = Vec::with_capacity(accounts.len());
                for account in accounts {
                    let default = cost_rate_plan(&mut *conn, account.id, None).await?;
                    items.push(json!({"account": account, "default": default}));
                }
                Ok::<_, DbError>(items)
            }
            .boxed()
        })
        .await?;
    Ok(Json(json!({ "items": items })))
}

/// `GET /admin/provider-keys/{id}/cost-rates` → `{account, default, models:
/// [{model_id, name, rate}]}`: the account's default cost-rate plan and each
/// of its models' override plan ([`account_models`]). `404` for an id that
/// names no live account.
pub async fn list_account(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = Uuid::parse_str(&raw).map_err(|_| ApiError::not_found())?;
    let answer = state
        .db
        .tx(move |conn| {
            async move {
                // One transaction, so every plan below reads the same `now()`.
                let account = live_account(&mut *conn, id).await?;
                let default = cost_rate_plan(&mut *conn, id, None).await?;
                let models = account_models(&mut *conn, id).await?;
                let mut items = Vec::with_capacity(models.len());
                for (model_id, name) in models {
                    let rate = cost_rate_plan(&mut *conn, id, Some(model_id)).await?;
                    items.push(json!({"model_id": model_id, "name": name, "rate": rate}));
                }
                Ok::<_, ApiError>(json!({
                    "account": account,
                    "default": default,
                    "models": items,
                }))
            }
            .boxed()
        })
        .await?;
    Ok(Json(answer))
}

/// `POST /admin/provider-keys/{id}/cost-rates`: a model's override, or the
/// account's default.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostRateBody {
    #[serde(default)]
    model_id: Option<Uuid>,
    /// A decimal string; `null` or absent on a model row ends its override.
    /// Parsed exactly as a group rate is, so a JSON number answers
    /// `422 /cny_per_usd`.
    #[serde(default)]
    cny_per_usd: Option<Value>,
    effective_from: DateTime<Utc>,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /admin/provider-keys/{id}/cost-rates` → `201 {revision,
/// warnings}`. A default row (`model_id` null) with no rate is
/// `422 /cny_per_usd`: only a model's override ends.
pub async fn create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(body): ApiJson<CostRateBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = Uuid::parse_str(&raw).map_err(|_| ApiError::not_found())?;
    let rate = body
        .cny_per_usd
        .as_ref()
        .map(|raw| {
            crate::groups::checked_rate("/cny_per_usd", Some(raw))
                .map_err(|d| ApiError::validation(vec![d]))
        })
        .transpose()?;
    let row = NewCostRate {
        provider_key_id: id,
        model_id: body.model_id,
        effective_from: body.effective_from,
        cny_per_usd: rate,
        note: body.note.unwrap_or_default(),
    };
    let (revision, warnings) = state
        .db
        .tx(move |conn| {
            async move { insert_cost_rate(&mut *conn, &row, Actor::Operator(&auth)).await }.boxed()
        })
        .await?;
    let revision = serde_json::to_value(revision).map_err(ApiError::internal)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"revision": revision, "warnings": warnings})),
    ))
}
