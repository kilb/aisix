//! What operators see of prices: every live model against every live group,
//! with the CNY per million that group charges now and the ones scheduled
//! after it (`GET /admin/pricing/models`), and one tenant's discount
//! (`GET /admin/pricing/tenants/{id}`).
//!
//! A model's price in a group is its baseline in force times that group's
//! rate for the model in force (its own override, else the group's default),
//! before any tenant discount: this is the operators' view, and a tenant's
//! discount applies on top of it at charging time (R12). A model with no
//! baseline, or a group with no rate in force, shows no per-token figure and
//! is listed first, which is the alert the price list exists for.
//!
//! "In force" is the price book's own rule: of one key's revisions, the one
//! with the greatest `effective_from` at or before the instant, ties going to
//! the greatest `revision_id`. The instant is the database's `now()`, the
//! clock every price write is checked against, so a row priced now is charged
//! at the revision named here. A revision this engine cannot use (a newer
//! `engine_version`) is shown where it stands, since the book keeps its slot
//! too and the rows it would price are refused, not priced at an older
//! revision.
//!
//! An upcoming revision is one starting later than now. Of several starting
//! at the same instant only the greatest `revision_id` is listed: it is the
//! one that will apply, and the others never will.
//!
//! These views only read the revision tables. Each answer is read in one
//! transaction, so its current and upcoming revisions share one `now()`.

use std::collections::HashMap;

use aisix_pricing::{cny_per_million, CnyPerUsd, RateCard, ServiceTier};
use axum::extract::{Path, State};
use axum::Json;
use futures::FutureExt;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::money::CURRENCY;
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::ApiError;
use crate::pricing::baselines::{current_baselines, upcoming_baselines, BaselineRevision};
use crate::pricing::rates::rate_in_force;
use crate::pricing::store::DiscountRevision;
use crate::resources::Kind;
use crate::AppState;

/// The columns a [`DiscountRevision`] is read from.
const DISCOUNT_COLUMNS: &str =
    "revision_id, engine_version, tenant_id, effective_from, discount_bps, created_by, created_at";

/// The discount revision in force now for `tenant_id`; `None` when none has
/// started.
pub async fn current_discount(
    conn: &mut PgConnection,
    tenant_id: Uuid,
) -> Result<Option<DiscountRevision>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {DISCOUNT_COLUMNS} FROM pricing_discount_revisions \
         WHERE tenant_id = $1 AND effective_from <= now() \
         ORDER BY effective_from DESC, revision_id DESC LIMIT 1"
    ))
    .bind(tenant_id)
    .fetch_optional(conn)
    .await?)
}

/// The discount revisions of `tenant_id` that start later than now, at most
/// one per instant, by start.
async fn upcoming_discounts(
    conn: &mut PgConnection,
    tenant_id: Uuid,
) -> Result<Vec<DiscountRevision>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT DISTINCT ON (effective_from) {DISCOUNT_COLUMNS} \
         FROM pricing_discount_revisions \
         WHERE tenant_id = $1 AND effective_from > now() \
         ORDER BY effective_from, revision_id DESC"
    ))
    .bind(tenant_id)
    .fetch_all(conn)
    .await?)
}

/// One live group, as the price list names it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    display_name: String,
}

/// Every live group, by display name, then id.
async fn live_groups(conn: &mut PgConnection) -> Result<Vec<GroupRow>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id, display_name FROM provider_groups WHERE retired_at IS NULL \
         ORDER BY display_name, id",
    )
    .fetch_all(conn)
    .await?)
}

/// The `default` tier's base rates of `card`, in CNY per million at `cny`.
/// `None` when the card carries no default tier.
fn per_million(card: &RateCard, cny: CnyPerUsd) -> Option<(String, String)> {
    let base = &card.0.get(&ServiceTier::default_tier())?.base;
    Some((
        cny_per_million(base.input, cny, 0)?,
        cny_per_million(base.output, cny, 0)?,
    ))
}

/// What one group charges for one model.
#[derive(Debug, Serialize)]
struct GroupPrice {
    group_id: Uuid,
    display_name: String,
    /// The rate in force, as the decimal string it was typed as; `null` when
    /// the group has no rate for the model.
    cny_per_usd: Option<String>,
    /// `model` when the model's own override priced it, `default` when the
    /// group's default did, `null` when neither is in force.
    source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_per_million_cny: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_per_million_cny: Option<String>,
}

/// One model's baseline and every live group's price for it.
#[derive(Debug, Serialize)]
struct ModelPriceRow {
    model_id: Uuid,
    name: String,
    baseline: BaselinePlan,
    groups: Vec<GroupPrice>,
}

/// A model's baseline in force and the ones scheduled after it.
#[derive(Debug, Serialize)]
struct BaselinePlan {
    current: Option<BaselineRevision>,
    /// By ascending `effective_from`.
    upcoming: Vec<BaselineRevision>,
}

/// `GET /admin/pricing/models` → `{items: [ModelPriceRow]}`, in the design's
/// charge-list shape: every live model, the ones with no baseline or no rated
/// group first, each group by name.
pub async fn models(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let mut rows = state
        .db
        .tx(|conn| {
            async move {
                let models: Vec<(Uuid, String)> = sqlx::query_as(
                    "SELECT id, COALESCE(doc->>'display_name', '') FROM gw_resources \
                     WHERE kind = $1 AND NOT deleted ORDER BY 2, 1",
                )
                .bind(Kind::Models.segment())
                .fetch_all(&mut *conn)
                .await?;
                let ids: Vec<Uuid> = models.iter().map(|(id, _)| *id).collect();
                let groups = live_groups(&mut *conn).await?;
                let mut current: HashMap<Uuid, BaselineRevision> =
                    current_baselines(&mut *conn, &ids)
                        .await?
                        .into_iter()
                        .map(|r| (r.model_id, r))
                        .collect();
                let mut upcoming: HashMap<Uuid, Vec<BaselineRevision>> = HashMap::new();
                for r in upcoming_baselines(&mut *conn, &ids).await? {
                    upcoming.entry(r.model_id).or_default().push(r);
                }
                let mut rows = Vec::with_capacity(models.len());
                for (model_id, name) in models {
                    let baseline_current = current.remove(&model_id);
                    let card = baseline_current
                        .as_ref()
                        .and_then(|b| serde_json::from_value::<RateCard>(b.card.clone()).ok());
                    let mut prices = Vec::with_capacity(groups.len());
                    for group in &groups {
                        let in_force = rate_in_force(&mut *conn, group.id, Some(model_id)).await?;
                        prices.push(group_price(group, in_force, card.as_ref()));
                    }
                    rows.push(ModelPriceRow {
                        model_id,
                        name,
                        baseline: BaselinePlan {
                            current: baseline_current,
                            upcoming: upcoming.remove(&model_id).unwrap_or_default(),
                        },
                        groups: prices,
                    });
                }
                Ok::<_, DbError>(rows)
            }
            .boxed()
        })
        .await?;
    // Stable, so each group keeps the name order: a model with no baseline in
    // force comes first, since that is the one nobody can price. A model
    // whose baseline exists but whose groups have no rate is listed where its
    // name puts it: the missing rate is the group's, and the group's row says
    // so.
    rows.sort_by_key(|r| r.baseline.current.is_some());
    Ok(Json(json!({"currency": CURRENCY, "items": rows})))
}

/// What `group` charges for the model, at the rate in force and the card it
/// is given. A group with no rate for the model shows none, and one whose
/// baseline is missing shows no per-token figure: the group's row is always
/// listed, so an operator can see which group has no rate.
fn group_price(
    group: &GroupRow,
    in_force: Option<(CnyPerUsd, i64, bool)>,
    card: Option<&RateCard>,
) -> GroupPrice {
    let rates = match (card, in_force) {
        (Some(card), Some((rate, _, _))) => per_million(card, rate),
        _ => None,
    };
    GroupPrice {
        group_id: group.id,
        display_name: group.display_name.clone(),
        cny_per_usd: in_force.map(|(rate, _, _)| rate.to_string()),
        source: in_force.map(|(_, _, overridden)| if overridden { "model" } else { "default" }),
        input_per_million_cny: rates.as_ref().map(|r| r.0.clone()),
        output_per_million_cny: rates.as_ref().map(|r| r.1.clone()),
    }
}

/// `TenantPlan`: a tenant's discount.
#[derive(Debug, Serialize)]
pub struct DiscountPlan {
    current: Option<DiscountRevision>,
    /// By ascending `effective_from`.
    upcoming: Vec<DiscountRevision>,
}

/// `TenantPlan`: a tenant's discount, in basis points. A tenant's price is its
/// key's group's, so there are no per-tenant prices.
#[derive(Debug, Serialize)]
pub struct TenantPlan {
    tenant_id: Uuid,
    discount: DiscountPlan,
}

/// `GET /admin/pricing/tenants/{id}` → `TenantPlan`. An unknown or malformed
/// tenant id is `404`.
pub async fn tenant(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TenantPlan>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let tenant_id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let plan = state
        .db
        .tx(move |conn| {
            async move {
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1)")
                        .bind(tenant_id)
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                if !exists {
                    return Err(ApiError::not_found());
                }
                Ok(TenantPlan {
                    tenant_id,
                    discount: DiscountPlan {
                        current: current_discount(&mut *conn, tenant_id).await?,
                        upcoming: upcoming_discounts(&mut *conn, tenant_id).await?,
                    },
                })
            }
            .boxed()
        })
        .await?;
    Ok(Json(plan))
}
