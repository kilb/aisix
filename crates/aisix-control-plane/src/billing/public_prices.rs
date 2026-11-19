//! 首页匿名模型价格。只展示面向新注册租户的标准售价，金额为人民币精确小数。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use aisix_pricing::{Dimension, ServiceTier, TierQuote};
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::lookup_prices::{valid_until, QuoteContext};
use super::money::CURRENCY;
use crate::db::DbError;
use crate::error::ApiError;
use crate::resources::Kind;
use crate::{groups, settings, AppState};

const MAX_AGE: Duration = Duration::from_secs(15);

#[derive(Serialize)]
struct PublicCatalogue {
    currency: &'static str,
    as_of: DateTime<Utc>,
    available: bool,
    models: Vec<PublicModelPrice>,
}

#[derive(Serialize)]
struct PublicModelPrice {
    id: Uuid,
    name: String,
    input_per_million: Option<String>,
    output_per_million: Option<String>,
    cache_read_per_million: Option<String>,
    prices_by_tier: Option<BTreeMap<ServiceTier, TierQuote>>,
}

impl PublicModelPrice {
    fn new(
        id: Uuid,
        name: String,
        prices_by_tier: Option<BTreeMap<ServiceTier, TierQuote>>,
    ) -> Self {
        let base = prices_by_tier
            .as_ref()
            .and_then(|tiers| tiers.get(&ServiceTier::default_tier()));
        let explicit = |dimension| {
            base?
                .base
                .iter()
                .find(|rate| rate.dimension == dimension && rate.fallback_from.is_none())?
                .price_cny_per_million
                .clone()
        };
        Self {
            id,
            name,
            input_per_million: explicit(Dimension::Input),
            output_per_million: explicit(Dimension::Output),
            cache_read_per_million: explicit(Dimension::CacheRead),
            prices_by_tier,
        }
    }
}

async fn signup_price_group(conn: &mut PgConnection) -> Result<Option<Uuid>, ApiError> {
    let defaults = settings::tenant_defaults(conn).await?;
    let groups = groups::groups_of(conn, &defaults.default_provider_groups).await?;
    if let [only] = groups.as_slice() {
        return Ok(Some(only.id));
    }
    // 只有已分配给新租户的分组可以发布价格。
    Ok(groups
        .into_iter()
        .find(|group| group.is_default)
        .map(|group| group.id))
}

pub async fn prices(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let started = Instant::now();
    let cache = state.prices.clone();
    let (body, until) = state
        .db
        .tx(move |conn| {
            async move {
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                let group_id = signup_price_group(conn).await?;
                let rows: Vec<(Uuid, String)> = sqlx::query_as(
                    "SELECT p.id, p.name FROM public_models p \
                 JOIN gw_resources r ON r.kind = $1 AND r.id = p.id AND NOT r.deleted \
                 WHERE p.enabled AND p.retired_at IS NULL \
                   AND r.managed_by = 'public:' || p.id::text \
                   AND COALESCE(r.doc->>'internal', 'false') <> 'true' \
                   AND position('*' in p.name) = 0 ORDER BY p.name, p.id",
                )
                .bind(Kind::Models.segment())
                .fetch_all(&mut *conn)
                .await
                .map_err(DbError::from)?;

                let (as_of, until, models) = if let Some(group_id) =
                    group_id.filter(|_| !rows.is_empty())
                {
                    let context = QuoteContext::load(conn, &cache).await?;
                    let ids: Vec<_> = rows.iter().map(|(id, _)| *id).collect();
                    let until = valid_until(conn, &ids, None, group_id, context.as_of).await?;
                    let group = group_id.to_string();
                    let models = rows
                        .into_iter()
                        .map(|(id, name)| {
                            let tiers =
                                context
                                    .book
                                    .public_quote(&id.to_string(), &group, context.as_of);
                            PublicModelPrice::new(id, name, tiers)
                        })
                        .collect();
                    (context.as_of, until, models)
                } else {
                    let as_of = sqlx::query_scalar("SELECT now()")
                        .fetch_one(conn)
                        .await
                        .map_err(DbError::from)?;
                    let models = rows
                        .into_iter()
                        .map(|(id, name)| PublicModelPrice::new(id, name, None))
                        .collect();
                    (as_of, None, models)
                };

                Ok::<_, ApiError>((
                    PublicCatalogue {
                        currency: CURRENCY,
                        as_of,
                        available: group_id.is_some(),
                        models,
                    },
                    until,
                ))
            }
            .boxed()
        })
        .await?;

    // 按整个请求已耗时扣减，缓存不能越过下一次计划生效的时间。
    let max_age = until.map_or(MAX_AGE, |until| {
        (until - body.as_of)
            .to_std()
            .unwrap_or_default()
            .saturating_sub(started.elapsed())
            .min(MAX_AGE)
    });
    let cache_control = format!("public, max-age={}, must-revalidate", max_age.as_secs());
    Ok(([(header::CACHE_CONTROL, cache_control)], Json(body)))
}
