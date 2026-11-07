//! What operators see of prices: each live model's platform price in force
//! now and the ones scheduled after it (`GET /admin/pricing/models`), and one
//! tenant's plan, its discount and its overrides
//! (`GET /admin/pricing/tenants/{id}`).
//!
//! "In force" is the price book's own rule: of one model's revisions in one
//! scope (the platform's, or one tenant's overrides), the one with the
//! greatest `effective_from` at or before the instant, ties going to the
//! greatest `revision_id`. The instant is the database's `now()`, the clock
//! every price write is checked against, so a row priced now is charged at
//! the revision [`current_revision`] names. A revision this engine cannot use
//! (a newer `engine_version`) is shown where it stands, since the book keeps
//! its slot too and the rows it would price are refused, not priced at an
//! older revision.
//!
//! An upcoming revision is one starting later than now. Of several starting
//! at the same instant only the greatest `revision_id` is listed: it is the
//! one that will apply, and the others never will.
//!
//! These views only read P2a's revision tables. Each answer is read in one
//! transaction, so its current and upcoming revisions share one `now()`.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::Json;
use futures::FutureExt;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::ApiError;
use crate::pricing::store::{DiscountRevision, SellRevision};
use crate::resources::Kind;
use crate::AppState;

/// The columns a [`SellRevision`] is read from.
const SELL_COLUMNS: &str = "revision_id, engine_version, model_id, tenant_id, effective_from, \
                            card, source, created_by, created_at";

/// The columns a [`DiscountRevision`] is read from.
const DISCOUNT_COLUMNS: &str =
    "revision_id, engine_version, tenant_id, effective_from, discount_bps, created_by, created_at";

/// The sell revision in force now for `model_id`: the platform's price when
/// `tenant_id` is `None`, otherwise that tenant's override (one whose `card`
/// is null ended the override). `None` when no revision of that scope has
/// started.
pub async fn current_revision(
    conn: &mut PgConnection,
    model_id: Uuid,
    tenant_id: Option<Uuid>,
) -> Result<Option<SellRevision>, DbError> {
    Ok(current_revisions(conn, &[model_id], tenant_id).await?.pop())
}

/// [`current_revision`] for each of `model_ids` at once, one revision per
/// model that has one, in no particular order.
pub async fn current_revisions(
    conn: &mut PgConnection,
    model_ids: &[Uuid],
    tenant_id: Option<Uuid>,
) -> Result<Vec<SellRevision>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT DISTINCT ON (model_id) {SELL_COLUMNS} FROM pricing_sell_revisions \
         WHERE model_id = ANY($1) AND tenant_id IS NOT DISTINCT FROM $2 \
           AND effective_from <= now() \
         ORDER BY model_id, effective_from DESC, revision_id DESC"
    ))
    .bind(model_ids)
    .bind(tenant_id)
    .fetch_all(conn)
    .await?)
}

/// The sell revisions of `model_ids` in one scope that start later than
/// now, at most one per instant, by model and then by start.
async fn upcoming_revisions(
    conn: &mut PgConnection,
    model_ids: &[Uuid],
    tenant_id: Option<Uuid>,
) -> Result<Vec<SellRevision>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT DISTINCT ON (model_id, effective_from) {SELL_COLUMNS} \
         FROM pricing_sell_revisions \
         WHERE model_id = ANY($1) AND tenant_id IS NOT DISTINCT FROM $2 \
           AND effective_from > now() \
         ORDER BY model_id, effective_from, revision_id DESC"
    ))
    .bind(model_ids)
    .bind(tenant_id)
    .fetch_all(conn)
    .await?)
}

/// The discount revision in force now for `tenant_id`, by the same rule as
/// [`current_revision`]; `None` when none has started.
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

/// `PriceRow`: one model's price in one scope.
#[derive(Debug, Serialize)]
pub struct PriceRow {
    model_id: Uuid,
    name: String,
    current: Option<SellRevision>,
    /// By ascending `effective_from`.
    upcoming: Vec<SellRevision>,
}

/// The price rows of `models` (`(id, name)`, in the order given) in one
/// scope.
async fn price_rows(
    conn: &mut PgConnection,
    models: Vec<(Uuid, String)>,
    tenant_id: Option<Uuid>,
) -> Result<Vec<PriceRow>, DbError> {
    let ids: Vec<Uuid> = models.iter().map(|(id, _)| *id).collect();
    let mut current: HashMap<Uuid, SellRevision> = current_revisions(&mut *conn, &ids, tenant_id)
        .await?
        .into_iter()
        .map(|r| (r.model_id, r))
        .collect();
    let mut upcoming: HashMap<Uuid, Vec<SellRevision>> = HashMap::new();
    // By start within each model, as read.
    for r in upcoming_revisions(&mut *conn, &ids, tenant_id).await? {
        upcoming.entry(r.model_id).or_default().push(r);
    }
    Ok(models
        .into_iter()
        .map(|(model_id, name)| PriceRow {
            model_id,
            name,
            current: current.remove(&model_id),
            upcoming: upcoming.remove(&model_id).unwrap_or_default(),
        })
        .collect())
}

/// Every live model as `(id, name)`, by name, then id.
async fn live_models(conn: &mut PgConnection) -> Result<Vec<(Uuid, String)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id, COALESCE(doc->>'display_name', '') FROM gw_resources \
         WHERE kind = $1 AND NOT deleted \
         ORDER BY 2, 1",
    )
    .bind(Kind::Models.segment())
    .fetch_all(conn)
    .await?)
}

/// `GET /admin/pricing/models` → `{items: [PriceRow]}`: every live model's
/// platform price, the models with no price in force first, each group by
/// name.
pub async fn models(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let mut rows = state
        .db
        .tx(|conn| {
            async move {
                let models = live_models(&mut *conn).await?;
                price_rows(conn, models, None).await
            }
            .boxed()
        })
        .await?;
    // Stable, so each group keeps the name order.
    rows.sort_by_key(|r| r.current.is_some());
    Ok(Json(json!({ "items": rows })))
}

/// `TenantPlan`'s `discount`.
#[derive(Debug, Serialize)]
pub struct DiscountPlan {
    current: Option<DiscountRevision>,
    /// By ascending `effective_from`.
    upcoming: Vec<DiscountRevision>,
}

/// `TenantPlan`: a tenant's discount, and every live model it has an
/// override revision for, by model name.
#[derive(Debug, Serialize)]
pub struct TenantPlan {
    tenant_id: Uuid,
    discount: DiscountPlan,
    overrides: Vec<PriceRow>,
}

/// `GET /admin/pricing/tenants/{id}` → `TenantPlan`. An override whose
/// `current.card` is null has ended, and the platform price applies. An
/// unknown or malformed tenant id is `404`.
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
                let discount = DiscountPlan {
                    current: current_discount(&mut *conn, tenant_id).await?,
                    upcoming: upcoming_discounts(&mut *conn, tenant_id).await?,
                };
                let overridden: Vec<(Uuid, String)> = sqlx::query_as(
                    "SELECT r.id, COALESCE(r.doc->>'display_name', '') FROM gw_resources r \
                     WHERE r.kind = $1 AND NOT r.deleted AND EXISTS ( \
                       SELECT 1 FROM pricing_sell_revisions s \
                       WHERE s.model_id = r.id AND s.tenant_id = $2) \
                     ORDER BY 2, 1",
                )
                .bind(Kind::Models.segment())
                .bind(tenant_id)
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let overrides = price_rows(conn, overridden, Some(tenant_id)).await?;
                Ok(TenantPlan {
                    tenant_id,
                    discount,
                    overrides,
                })
            }
            .boxed()
        })
        .await?;
    Ok(Json(plan))
}
