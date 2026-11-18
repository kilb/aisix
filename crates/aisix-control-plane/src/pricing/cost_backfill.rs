//! 管理员用本次操作开始时生效的账户成本率补填历史空值；原收费与余额保持原样。

use aisix_pricing::{cost_of_lines, ChargeLine, PriceBook, ENGINE_VERSION};
use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{cost_rates, store};
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::billing::budgets::db_now;
use crate::crypto::MasterKey;
use crate::db::DbError;
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::AppState;

const PAGE_SIZE: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FillBody {
    after: Option<String>,
    #[serde(default)]
    prepare: bool,
}

#[derive(Serialize)]
pub struct FillResult {
    examined: usize,
    filled: u64,
    next_after: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    operation_id: Uuid,
    after_seq: i64,
    through_seq: i64,
    as_of: DateTime<Utc>,
    pricing_version: i64,
}

impl Cursor {
    fn decode(raw: &str, key: &MasterKey, aad: &[u8]) -> Result<Self, ApiError> {
        let invalid = || store::refused("/after", "补填游标无效，请从头开始");
        if raw.len() > 8192 {
            return Err(invalid());
        }
        let json = key.open_text(aad, raw).map_err(|_| invalid())?;
        serde_json::from_str(&json).map_err(|_| invalid())
    }

    fn encode(&self, key: &MasterKey, aad: &[u8]) -> Result<String, ApiError> {
        let json = serde_json::to_string(self).map_err(ApiError::internal)?;
        Ok(key.seal_text(aad, &json))
    }
}

/// 分页签名绑定账户、操作时点与费率版本。翻页期间有新价格写入时，重新开始只补仍为空的行。
pub async fn fill(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    ApiJson(body): ApiJson<FillBody>,
) -> Result<Json<FillResult>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let id = Uuid::parse_str(&raw).map_err(|_| ApiError::not_found())?;
    if body.prepare && body.after.is_some() {
        return Err(store::refused("/after", "准备新的补填操作时无需游标"));
    }
    let aad = format!("pricing.cost_backfill:{id}");
    let cfg = state.cfg;
    let cursor = body
        .after
        .as_deref()
        .map(|s| Cursor::decode(s, &cfg.master_key, aad.as_bytes()))
        .transpose()?;
    state
        .db
        .tx(move |conn| {
            async move {
                cost_rates::live_account(&mut *conn, id).await?;
                let (version, book) = state.prices.current_with_version(&mut *conn).await?;
                if version == 0
                    || cursor
                        .as_ref()
                        .is_some_and(|c| c.pricing_version != version)
                {
                    return Err(ApiError::conflict(
                        "cost_backfill_stale",
                        "费率版本已变更或暂不可用，请从头开始补填",
                    ));
                }
                let mut cursor = match cursor {
                    Some(cursor) => cursor,
                    None => Cursor {
                        operation_id: Uuid::new_v4(),
                        after_seq: 0,
                        // seq 是分页边界；重叠的接收事务若稍后提交，下一次补录会重新从头检查。
                        through_seq: sqlx::query_scalar(
                            "SELECT COALESCE(max(seq), 0) FROM usage_events",
                        )
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(DbError::from)?,
                        as_of: db_now(&mut *conn).await?,
                        pricing_version: version,
                    },
                };
                // 浏览器先获取初始游标，首批写入后即使回执丢失，仍能重放同一页。
                let (examined, filled, next) = if body.prepare {
                    (0, 0, Some(0))
                } else {
                    fill_page(&mut *conn, id, &auth, &book, &cursor).await?
                };
                let next_after = next
                    .map(|seq| {
                        cursor.after_seq = seq;
                        cursor.encode(&cfg.master_key, aad.as_bytes())
                    })
                    .transpose()?;
                Ok(Json(FillResult {
                    examined,
                    filled,
                    next_after,
                }))
            }
            .boxed()
        })
        .await
}

#[derive(sqlx::FromRow)]
struct Candidate {
    operation_id: Option<Uuid>,
    seq: i64,
    event_id: String,
    requested_model_id: Option<String>,
    lines: Option<Value>,
}

async fn fill_page(
    conn: &mut PgConnection,
    id: Uuid,
    auth: &OperatorAuth,
    book: &PriceBook,
    cursor: &Cursor,
) -> Result<(usize, u64, Option<i64>), ApiError> {
    let account = id.to_string();
    // 先按账户索引取一页，再看原收费和补录。即使只有极少的空值，一页的检查量仍有上限。
    let mut page: Vec<Candidate> = sqlx::query_as(
        "WITH page AS MATERIALIZED (
           SELECT ue.seq, ue.event_id FROM usage_events ue
           WHERE length(ue.event->>'provider_key_id') = 36
             AND ue.event->>'provider_key_id' = $1 AND ue.seq > $2 AND ue.seq <= $3
           ORDER BY ue.seq LIMIT $4
         )
         SELECT p.seq, p.event_id, b.operation_id, c.requested_model_id,
                CASE WHEN c.status = 'charged' AND c.baseline_revision_id IS NOT NULL THEN c.lines
                     WHEN first_bill.engine_version <= $5 AND first_bill.baseline_revision_id IS NOT NULL
                         THEN first_bill.lines END AS lines FROM page p
         LEFT JOIN pricing_cost_backfills b ON b.event_id = p.event_id
         LEFT JOIN pricing_charges_cny c ON c.event_id = p.event_id AND b.event_id IS NULL
           AND c.status IN ('charged','unpriced') AND c.engine_version <= $5
           AND c.cost_micro_cny IS NULL AND c.cost_micro_usd IS NULL
         LEFT JOIN LATERAL (
           SELECT a.engine_version, a.baseline_revision_id, a.lines
           FROM pricing_adjustments_cny a WHERE c.status = 'unpriced' AND a.event_id = c.event_id
             AND a.delta_micro_cny > 0 AND a.created_at <= $6
           ORDER BY a.created_at, a.run_id LIMIT 1
         ) first_bill ON true
         ORDER BY p.seq",
    )
    .bind(&account)
    .bind(cursor.after_seq)
    .bind(cursor.through_seq)
    .bind(PAGE_SIZE as i64 + 1)
    .bind(i64::from(ENGINE_VERSION))
    .bind(cursor.as_of)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let next = (page.len() > PAGE_SIZE).then(|| page[PAGE_SIZE - 1].seq);
    page.truncate(PAGE_SIZE);
    let examined = page.len();
    let already_filled = page
        .iter()
        .filter(|row| row.operation_id == Some(cursor.operation_id))
        .count() as u64;
    let mut ids = Vec::new();
    let mut revisions = Vec::new();
    let mut amounts = Vec::new();
    for row in page {
        let (Some(model), Some(raw)) = (row.requested_model_id, row.lines) else {
            continue;
        };
        let Ok(lines) = serde_json::from_value::<Vec<ChargeLine>>(raw) else {
            continue;
        };
        if lines.is_empty() {
            continue;
        }
        if let Some(cost) = cost_of_lines(book, &account, &model, &lines, cursor.as_of) {
            ids.push(row.event_id);
            revisions.push(cost.rate.revision_id);
            amounts.push(cost.amount.0);
        }
    }
    if ids.is_empty() {
        return Ok((examined, already_filled, next));
    }
    let inserted = sqlx::query(
        "INSERT INTO pricing_cost_backfills
           (event_id, provider_key_id, rate_revision_id, cost_micro_cny, created_by, operation_id)
         SELECT event_id, $1, revision, amount, $2, $6
         FROM unnest($3::text[], $4::bigint[], $5::bigint[]) AS row(event_id, revision, amount)
         ORDER BY event_id ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(id)
    .bind(auth.id)
    .bind(&ids)
    .bind(revisions)
    .bind(amounts)
    .bind(cursor.operation_id)
    .execute(&mut *conn)
    .await
    .map_err(DbError::from)?
    .rows_affected();
    if inserted > 0 {
        audit::record(
            &mut *conn,
            AuditEntry {
                actor: Actor::Operator(auth),
                action: "pricing.cost_backfill",
                target_kind: "provider_key",
                target_id: Some(account),
                tenant_id: None,
                detail: json!({ "examined": examined, "filled": inserted, "operation_id": cursor.operation_id, "rate_at": cursor.as_of }),
            },
        )
        .await?;
    }
    // 并发重放可能在唯一键上等到同一操作的另一事务提交；此处按操作归属确认本页全部写入。
    let confirmed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pricing_cost_backfills WHERE event_id = ANY($1) AND operation_id = $2",
    ).bind(&ids).bind(cursor.operation_id).fetch_one(conn).await.map_err(DbError::from)?;
    Ok((examined, already_filled + confirmed as u64, next))
}
