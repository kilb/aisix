//! The margin: what the platform charged against what its upstream
//! accounts cost it, both in CNY.
//!
//! A charge is micro-CNY, and so is a cost: the charge's own baseline lines
//! at the serving account's cost rate (`pricing::cost_rates`). Profit is the
//! charge less the cost, and the margin is that profit as a percentage of
//! the charge, both over the rows whose cost is known ([`margin_pct`]).
//!
//! A row's cost is known only when it names the account that served it and
//! that account has a cost rate in force for the model; a row priced before
//! cost became a rate has no CNY cost either. A cost nobody knows never
//! enters the cost side, and is never read as 0: such rows are counted apart
//! (`rows_without_cost`) rather than dropped silently, so an operator can
//! see how much of a period's charge the cost side does not cover — and
//! their money is still in `charged_micro_cny`, because they were charged.
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

/// Hundredths of a percent in a whole: the margin shows two decimal places.
const PCT_PLACES: i128 = 10_000;

/// `profit` as a percentage of `charged`, to two decimal places:
/// `profit × 10⁴ / charged`, rounded half away from zero. `None` when
/// `charged` is at or below zero, where no percentage exists.
///
/// Integer arithmetic in `i128` throughout: an amount is at most
/// `i64::MAX`, so `× 10_000` and the doubling below stay inside `i128`, and
/// no float ever decides a figure.
pub fn margin_pct(profit_micro_cny: i64, charged_micro_cny: i64) -> Option<String> {
    if charged_micro_cny <= 0 {
        return None;
    }
    let numerator = i128::from(profit_micro_cny) * PCT_PLACES;
    let charged = i128::from(charged_micro_cny);
    // Round half away from zero: |n|/d, ties going away from zero, then the
    // sign back. `2|n| + d` over `2d` is exactly that for a positive `d`.
    let magnitude = (numerator.abs() * 2 + charged) / (charged * 2);
    let sign = if numerator < 0 && magnitude > 0 {
        "-"
    } else {
        ""
    };
    Some(format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100))
}

#[derive(Deserialize)]
pub struct MarginQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    group_id: Option<String>,
    model_id: Option<String>,
    provider_key_id: Option<String>,
}

/// A UUID filter: `422` at `path` for a malformed one, which names no group,
/// model or account either way.
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

/// `GET /admin/pricing/margin?from&to&group_id&model_id&provider_key_id` →
/// `{items: [MarginRow]}`, grouped by (group, model) and ordered by the
/// group's display name (a null group last), then the group's id, then the
/// model's id.
///
/// **The rows are the billed ones**: a charge with status `charged`, or one a
/// re-pricing run adjusted. An exempt row (a response-cache hit, agent or
/// tool traffic), a row that billed nothing, and an unpriced row nobody
/// corrected carry no charge, and a rate set later would never give them a
/// cost, so they are neither costed nor counted as unknown.
///
/// **`provider_key_id` keeps the rows one upstream account served**: the
/// account the usage row names (`provider_key_id` on the stored event). Two
/// accounts serving one public model in one group charge the same price and
/// cost what each costs, so this is how their margins are told apart
/// (design C7). Each item of a filtered answer names that account:
/// `provider_key_id`, and `provider_key_display_name` read from the
/// account's own live document, never from the event (`null` once the
/// account is deleted). An answer over every account names none: both are
/// `null`, and its items stay one per (group, model).
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
/// **Profit and margin set one population against itself.**
/// `charged_micro_cny` is the group's whole corrected charge: every row's
/// `charge_micro_cny` plus every adjustment to it. `cost_micro_cny` and
/// `charged_with_cost_micro_cny` cover only the rows whose cost is known;
/// `profit_micro_cny` is the second less the first, and `margin_pct` that
/// profit over the second ([`margin_pct`]) — a row with no known cost
/// contributes money to the total but no cost, and setting that money
/// against a cost it never had would inflate the profit into a false figure.
/// So the answer carries both: `charged_without_cost_micro_cny` is the part
/// of the total with no cost behind it (`charged_micro_cny` less
/// `charged_with_cost_micro_cny`), and `rows_without_cost` counts its rows.
/// A reader who trusts the profit can see exactly how much charge it does
/// not describe.
///
/// The filters are bounded by the range, not by an index: the window is read
/// through `pricing_charges_cny_occurred` (0027), and the range cap of 731
/// days (`billing::range`) is what bounds that scan. `group_id` is matched
/// on the billed group, `COALESCE(c.group_id, k.group_id)`, an expression
/// across a join that `pricing_charges_cny_margin` cannot serve; `model_id`
/// and `provider_key_id` are checked on each row in the window, the second
/// through the row's own usage event.
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
    let account = uuid_filter(q.provider_key_id.as_deref(), "/provider_key_id")?;
    let account_filter = account.map(|id| id.to_string());
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
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
                          FILTER (WHERE c.cost_micro_cny IS NOT NULL), 0)::bigint \
                 + COALESCE(SUM((SELECT COALESCE(SUM(pa.delta_micro_cny)::bigint, 0) \
                                 FROM pricing_adjustments_cny pa \
                                 WHERE pa.event_id = c.event_id)) \
                            FILTER (WHERE c.cost_micro_cny IS NOT NULL), 0)::bigint) \
                  AS charged_with_cost, \
                COALESCE(SUM(c.cost_micro_cny), 0)::bigint AS cost, \
                (COUNT(*) FILTER (WHERE c.cost_micro_cny IS NOT NULL))::bigint AS rows, \
                (COUNT(*) FILTER (WHERE c.cost_micro_cny IS NULL))::bigint \
                  AS rows_without_cost \
         FROM pricing_charges_cny c \
         LEFT JOIN tenant_keys k ON k.id::text = c.api_key_id \
         LEFT JOIN provider_groups g ON g.id = COALESCE(c.group_id, k.group_id) \
         LEFT JOIN gw_resources r ON r.kind = 'models' AND NOT r.deleted \
                                  AND r.id::text = c.requested_model_id \
         WHERE c.occurred_at >= $1 AND c.occurred_at < $2 \
           AND (c.status = 'charged' \
                OR EXISTS (SELECT 1 FROM pricing_adjustments_cny pa \
                           WHERE pa.event_id = c.event_id)) \
           AND ($3::uuid IS NULL OR COALESCE(c.group_id, k.group_id) = $3) \
           AND ($4::text IS NULL OR c.requested_model_id = $4) \
           AND ($5::text IS NULL OR EXISTS (SELECT 1 FROM usage_events ue \
                                            WHERE ue.event_id = c.event_id \
                                              AND ue.event->>'provider_key_id' = $5)) \
         GROUP BY COALESCE(c.group_id, k.group_id), g.display_name, c.requested_model_id, \
                  r.id, r.doc->>'display_name' \
         ORDER BY g.display_name NULLS LAST, COALESCE(c.group_id, k.group_id), \
                  c.requested_model_id",
    )
    .bind(from)
    .bind(to)
    .bind(group_id)
    .bind(model_filter)
    .bind(&account_filter)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    // The account's name as the control plane records it now.
    let account_name: Option<String> = match account {
        None => None,
        Some(id) => sqlx::query_scalar(
            "SELECT doc->>'display_name' FROM gw_resources \
             WHERE kind = 'provider_keys' AND id = $1 AND NOT deleted",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::from)?
        .flatten(),
    };
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
                "cost_micro_cny": row.cost,
                // Profit and margin set the charge that has a cost behind it
                // against that cost: the two populations must match.
                "profit_micro_cny": row.charged_with_cost - row.cost,
                "margin_pct": margin_pct(row.charged_with_cost - row.cost, row.charged_with_cost),
                "rows": row.rows,
                "rows_without_cost": row.rows_without_cost,
                "provider_key_id": account,
                "provider_key_display_name": account_name,
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
