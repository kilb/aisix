//! The realised margin: what the platform earned against what its upstreams
//! cost it (D17).
//!
//! No exchange rate is stored anywhere in this platform. A charge is in CNY
//! and an upstream cost is in USD, so margin is shown as the **realised
//! rate**: the CNY a group charged per USD of upstream cost, over the rows
//! whose cost is known ([`realised`]). An operator compares it with what the
//! platform pays for its dollars; when the cost is the official list price,
//! the realised rate equals the configured rate, which is the self-check.
//!
//! A row's cost is known only when it names the upstream that served it and
//! a cost price covers that pair: a row that names neither carries no cost,
//! and a cost nobody knows never enters the cost side of a ratio. Such rows
//! are counted apart (`rows_without_cost`) rather than dropped silently, so
//! an operator can see how much of a period's charge the cost side does not
//! cover — and their money is still in `charged_micro_cny`, because they were
//! charged.
//!
//! This API names upstream costs, so it is for operators only and is never
//! projected to tenants.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::billing::range::{checked_range, Group};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::usage::query_rejected;
use crate::AppState;

/// The scale `realised` shows: four decimal places.
const PLACES: i128 = 10_000;

/// The CNY a group charged per USD of upstream cost, as four decimal places:
/// `charged × 10⁴ / cost`, rounded half up. `None` when `cost` is at or below
/// zero, where no ratio exists.
///
/// Integer arithmetic in `i128` throughout: a charge is at most `i64::MAX`,
/// so `× 10_000` and the doubling below stay inside `i128`, and no float ever
/// decides an amount.
pub fn realised(charged_micro_cny: i64, cost_micro_usd: i64) -> Option<String> {
    if cost_micro_usd <= 0 {
        return None;
    }
    let numerator = i128::from(charged_micro_cny) * PLACES;
    let cost = i128::from(cost_micro_usd);
    // Round half away from zero: |n|/d, ties going away from zero, then the
    // sign back. `2|n| + d` over `2d` is exactly that for a positive `d`.
    let magnitude = (numerator.abs() * 2 + cost) / (cost * 2);
    let rounded = if numerator < 0 { -magnitude } else { magnitude };
    let sign = if rounded < 0 { "-" } else { "" };
    let magnitude = rounded.abs();
    Some(format!(
        "{sign}{}.{:04}",
        magnitude / PLACES,
        magnitude % PLACES
    ))
}

#[derive(Deserialize)]
pub struct MarginQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    group_id: Option<String>,
    model_id: Option<String>,
}

/// A UUID filter: `422` at `path` for a malformed one, which names no group
/// or model either way.
fn uuid_filter(raw: Option<&str>, path: &str) -> Result<Option<Uuid>, ApiError> {
    match raw {
        None => Ok(None),
        Some(raw) => Uuid::parse_str(raw).map(Some).map_err(|_| {
            ApiError::validation(vec![Detail {
                path: path.to_string(),
                message: "标识不合法".to_string(),
            }])
        }),
    }
}

/// `GET /admin/pricing/margin?from&to&group_id&model_id` → `{items:
/// [MarginRow]}`, grouped by (group, model) and ordered by the group's
/// display name (a null group last), then the group's id, then the model's
/// id.
///
/// **The group is the one the charge was billed at**, not the column alone:
/// `COALESCE(c.group_id, the key's group_id)`, the same expression a
/// re-pricing run prices a row at (`repricing::BILLED_GROUP`). Intake stores
/// a row it could not price with `group_id` NULL; a later run bills it at its
/// key's group and writes the money to `pricing_adjustments_cny`, while the
/// charge row is never updated. Grouping on the column would then drop that
/// money from every item. A row whose key names no group still belongs to
/// none: it comes back as its own item with `group` `null`, so it is visible
/// and countable rather than absent. The invariant this preserves, and
/// `the_margin_items_account_for_every_charged_row` pins, is that over any
/// range the sum of `charged_micro_cny` over every item equals the charges
/// plus their adjustments — whatever the grouping.
///
/// **The rate divides one population by itself.** `charged_micro_cny` is the
/// group's whole corrected charge: every row's `charge_micro_cny` plus every
/// adjustment to it. `cost_micro_usd` and `charged_with_cost_micro_cny` cover
/// only the rows whose cost is known, and `realised_cny_per_usd` divides the
/// second by the first — a repriced row whose cost price records nothing
/// contributes money to the total but no cost, and dividing that money by a
/// cost it never had would inflate the rate into a false check. So the answer
/// carries both figures: `charged_without_cost_micro_cny` is the part of the
/// total with no cost behind it (`charged_micro_cny` less
/// `charged_with_cost_micro_cny`), and `rows_without_cost` counts its rows.
/// A reader who trusts the rate can see exactly how much charge it does not
/// describe.
///
/// The filters are bounded by the range: `model_id` alone has no index of
/// its own besides `pricing_charges_cny_occurred` (0027), and the range cap
/// of 731 days (`billing::range`) is what bounds that scan; a group and a
/// model together use `pricing_charges_cny_margin`, and `group_id` alone
/// uses its leading column.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<MarginQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let (from, to) = match (q.from, q.to) {
        (Some(from), Some(to)) => checked_range(from, to, Some(Group::Month))?,
        _ => {
            return Err(ApiError::validation(vec![Detail {
                path: "/from".to_string(),
                message: "请给出开始和结束时间".to_string(),
            }]))
        }
    };
    let group_id = uuid_filter(q.group_id.as_deref(), "/group_id")?;
    let model_id = uuid_filter(q.model_id.as_deref(), "/model_id")?;
    let model_filter = model_id.map(|id| id.to_string());
    let rows: Vec<MarginRow> = sqlx::query_as(
        "SELECT COALESCE(c.group_id, k.group_id) AS billed_group_id, \
                g.display_name AS group_display_name, \
                c.requested_model_id AS requested, \
                CASE WHEN r.id IS NULL THEN NULL \
                     ELSE COALESCE(r.doc->>'display_name', '') END AS model_name, \
                (COALESCE(SUM(c.charge_micro_cny), 0)::bigint \
                 + COALESCE(SUM((SELECT COALESCE(SUM(pa.delta_micro_cny)::bigint, 0) \
                                 FROM pricing_adjustments_cny pa \
                                 WHERE pa.event_id = c.event_id)), 0)::bigint) \
                  AS charged, \
                (COALESCE(SUM(c.charge_micro_cny) \
                          FILTER (WHERE c.cost_micro_usd IS NOT NULL), 0)::bigint \
                 + COALESCE(SUM((SELECT COALESCE(SUM(pa.delta_micro_cny)::bigint, 0) \
                                 FROM pricing_adjustments_cny pa \
                                 WHERE pa.event_id = c.event_id)) \
                            FILTER (WHERE c.cost_micro_usd IS NOT NULL), 0)::bigint) \
                  AS charged_with_cost, \
                COALESCE(SUM(c.cost_micro_usd), 0)::bigint AS cost, \
                (COUNT(*) FILTER (WHERE c.cost_micro_usd IS NOT NULL))::bigint AS rows, \
                (COUNT(*) FILTER (WHERE c.cost_micro_usd IS NULL))::bigint \
                  AS rows_without_cost \
         FROM pricing_charges_cny c \
         LEFT JOIN tenant_keys k ON k.id::text = c.api_key_id \
         LEFT JOIN provider_groups g ON g.id = COALESCE(c.group_id, k.group_id) \
         LEFT JOIN gw_resources r ON r.kind = 'models' AND NOT r.deleted \
                                  AND r.id::text = c.requested_model_id \
         WHERE c.occurred_at >= $1 AND c.occurred_at < $2 \
           AND ($3::uuid IS NULL OR COALESCE(c.group_id, k.group_id) = $3) \
           AND ($4::text IS NULL OR c.requested_model_id = $4) \
         GROUP BY COALESCE(c.group_id, k.group_id), g.display_name, c.requested_model_id, \
                  r.id, r.doc->>'display_name' \
         ORDER BY g.display_name NULLS LAST, COALESCE(c.group_id, k.group_id), \
                  c.requested_model_id",
    )
    .bind(from)
    .bind(to)
    .bind(group_id)
    .bind(model_filter)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            json!({
                // A null group is a charge whose key names no group: it is
                // real money with no group to report it under.
                "group": match row.billed_group_id {
                    Some(id) => json!({"id": id, "display_name": row.group_display_name}),
                    None => json!({"id": null, "display_name": null}),
                },
                "model": {
                    "id": row.model_id(),
                    // `null` says the model is gone (deleted, or never a row);
                    // `""` would only say it has no display name.
                    "name": row.model_name,
                },
                "charged_micro_cny": row.charged,
                "charged_with_cost_micro_cny": row.charged_with_cost,
                "charged_without_cost_micro_cny": row.charged - row.charged_with_cost,
                "cost_micro_usd": row.cost,
                // The rate divides the charge that has a cost behind it by
                // that cost: the two populations must match.
                "realised_cny_per_usd": realised(row.charged_with_cost, row.cost),
                "rows": row.rows,
                "rows_without_cost": row.rows_without_cost,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// One (group, model) of a margin answer, as the query selects it.
#[derive(Debug, sqlx::FromRow)]
struct MarginRow {
    /// The group the charge was billed at: `None` when its key names none.
    billed_group_id: Option<Uuid>,
    /// That group's display name; `None` with `billed_group_id`.
    group_display_name: Option<String>,
    /// `pricing_charges_cny.requested_model_id`, a text column.
    requested: String,
    /// The model's display name; `None` when no live model row has its id.
    model_name: Option<String>,
    charged: i64,
    /// [`Self::charged`] restricted to the rows whose cost is known.
    charged_with_cost: i64,
    cost: i64,
    rows: i64,
    rows_without_cost: i64,
}

impl MarginRow {
    /// The requested model's id, while its column still holds one: a charge
    /// written before the gateway resolved a model has none.
    fn model_id(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.requested).ok()
    }
}
