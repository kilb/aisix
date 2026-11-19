//! 租户 CSV 导出。仅导出允许公开的列，完整下载超限时返回可重试的筛选错误。

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::{Postgres, QueryBuilder, Transaction};
use tokio::sync::SemaphorePermit;
use uuid::Uuid;

use super::{calls, statements, topups};
use crate::auth::{MemberAuth, MemberRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::usage::query_rejected;
use crate::AppState;

pub(crate) const MAX_IN_FLIGHT: u32 = 2;
pub(super) const MAX_ROWS: i64 = 5_000;
const MAX_BYTES: usize = 8 * 1024 * 1024;

fn narrow_window() -> ApiError {
    ApiError::validation(vec![Detail {
        path: "query".to_owned(),
        message: "导出范围过大，请缩短日期范围或增加筛选条件".to_owned(),
    }])
}

pub(super) fn check_rows(len: usize) -> Result<(), ApiError> {
    if len > MAX_ROWS as usize {
        return Err(narrow_window());
    }
    Ok(())
}

pub(super) fn read_error(error: impl Into<DbError>) -> ApiError {
    let error = error.into();
    if let DbError::Db(sqlx::Error::Database(db)) = &error {
        if db.code().as_deref() == Some("57014") {
            return narrow_window();
        }
    }
    error.into()
}

pub(super) enum Cell<'a> {
    Empty,
    Text(&'a str),
    Number(i128),
    Time(DateTime<Utc>),
}

impl<'a> Cell<'a> {
    pub(super) fn text(value: Option<&'a str>) -> Self {
        value.map_or(Self::Empty, Self::Text)
    }

    pub(super) fn number(value: Option<impl Into<i128>>) -> Self {
        value.map_or(Self::Empty, |value| Self::Number(value.into()))
    }
}

pub(super) struct Csv(String);

impl Csv {
    pub(super) fn new(header: &str) -> Self {
        Self(format!("\u{feff}{header}\r\n"))
    }

    pub(super) fn row<const N: usize>(&mut self, cells: [Cell<'_>; N]) -> Result<(), ApiError> {
        for (index, cell) in cells.into_iter().enumerate() {
            if index > 0 {
                self.0.push(',');
            }
            match cell {
                Cell::Empty => {}
                Cell::Text(text) => self.push_text(text)?,
                Cell::Number(number) => self.0.push_str(&number.to_string()),
                Cell::Time(time) => self
                    .0
                    .push_str(&time.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
            }
        }
        self.0.push_str("\r\n");
        if self.0.len() > MAX_BYTES {
            return Err(narrow_window());
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str) -> Result<(), ApiError> {
        let trimmed = text.trim_start_matches(|c: char| {
            c.is_whitespace() || c.is_control() || matches!(c, '\u{feff}' | '\u{200b}')
        });
        let formula = trimmed.starts_with(['=', '+', '-', '@', '＝', '＋', '－', '＠'])
            || text.starts_with(['\t', '\r', '\n', '\0']);
        let escaped_len = text.len()
            + text.bytes().filter(|&byte| byte == b'"').count()
            + 2
            + usize::from(formula);
        if escaped_len > MAX_BYTES.saturating_sub(self.0.len()) {
            return Err(narrow_window());
        }
        self.0.push('"');
        if formula {
            self.0.push('\'');
        }
        for part in text.split_inclusive('"') {
            self.0.push_str(part);
            if part.ends_with('"') {
                self.0.push('"');
            }
        }
        self.0.push('"');
        Ok(())
    }

    fn download(self, disposition: &'static str) -> Response {
        (
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (header::CONTENT_DISPOSITION, disposition),
                (header::CACHE_CONTROL, "private, no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            self.0,
        )
            .into_response()
    }
}

async fn snapshot(
    state: &AppState,
) -> Result<(SemaphorePermit<'_>, Transaction<'_, Postgres>), ApiError> {
    let permit = state
        .billing_exports
        .try_acquire()
        .map_err(|_| ApiError::busy())?;
    let mut conn = state
        .billing_export_db
        .pool()
        .begin()
        .await
        .map_err(|error| {
            if matches!(error, sqlx::Error::PoolTimedOut) {
                return ApiError::busy();
            }
            read_error(error)
        })?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *conn)
        .await
        .map_err(read_error)?;
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(state.cfg.db_statement_timeout_ms.min(5_000).to_string())
        .execute(&mut *conn)
        .await
        .map_err(read_error)?;
    Ok((permit, conn))
}

/// GET /api/usage/calls/export
pub async fn calls(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<calls::CallsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    auth.require_active()?;
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ])?;
    let Query(selection) = query.map_err(query_rejected)?;
    let (_permit, mut conn) = snapshot(&state).await?;
    let csv = calls::export_rows(&mut conn, &auth, selection).await?;
    conn.commit().await.map_err(read_error)?;
    Ok(csv.download("attachment; filename=\"usage-calls.csv\""))
}

/// GET /api/billing/ledger/export
pub async fn ledger(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<statements::LedgerFilters>, QueryRejection>,
) -> Result<Response, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Billing])?;
    let Query(filters) = query.map_err(query_rejected)?;
    filters.validate()?;
    let (_permit, mut conn) = snapshot(&state).await?;
    let rows =
        statements::ledger_rows(&mut conn, auth.tenant_id, filters, None, MAX_ROWS + 1, true)
            .await
            .map_err(read_error)?;
    check_rows(rows.len())?;
    let model_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|row| row.8.as_deref().and_then(|id| Uuid::parse_str(id).ok()))
        .collect();
    let models = calls::Models::load(&mut conn, auth.tenant_id, &model_ids)
        .await
        .map_err(read_error)?;
    conn.commit().await.map_err(read_error)?;
    let mut csv = Csv::new(
        "seq,posted_at_utc,kind,amount_micro_cny,balance_before_micro_cny,balance_after_micro_cny,event_id,topup_id,model",
    );
    for (seq, posted, kind, amount, balance, event, topup, _, model_id, event_name) in rows {
        let model = event.is_some().then(|| {
            calls::call_model(
                &models,
                model_id.as_deref().unwrap_or_default(),
                event_name.as_deref().unwrap_or_default(),
            )
        });
        let topup = topup.map(|id| id.to_string());
        csv.row([
            Cell::Number(seq.into()),
            Cell::Time(posted),
            Cell::Text(&kind),
            Cell::Number(amount.into()),
            Cell::Number(i128::from(balance) - i128::from(amount)),
            Cell::Number(balance.into()),
            Cell::text(event.as_deref()),
            Cell::text(topup.as_deref()),
            Cell::text(model.as_ref().map(|model| {
                if model.name.is_empty() {
                    statements::UNKNOWN_MODEL
                } else {
                    model.name.as_str()
                }
            })),
        ])?;
    }
    Ok(csv.download("attachment; filename=\"billing-ledger.csv\""))
}

/// GET /api/billing/topups/export
pub async fn topups(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<topups::TopupFilters>, QueryRejection>,
) -> Result<Response, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Billing])?;
    let Query(filters) = query.map_err(query_rejected)?;
    filters.validate()?;
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT p.id, p.requested_at, p.decided_at, p.origin, p.status, p.amount_micro_cny
         FROM billing_topups_cny p WHERE p.tenant_id = ",
    );
    sql.push_bind(auth.tenant_id);
    filters.push_page(&mut sql, None, MAX_ROWS);
    let (_permit, mut conn) = snapshot(&state).await?;
    type TopupExportRow = (
        Uuid,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
        String,
        String,
        i64,
    );
    let rows: Vec<TopupExportRow> = sql
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(read_error)?;
    conn.commit().await.map_err(read_error)?;
    check_rows(rows.len())?;
    let mut csv = Csv::new("id,requested_at_utc,decided_at_utc,origin,status,amount_micro_cny");
    for (id, requested, decided, origin, status, amount) in rows {
        csv.row([
            Cell::Text(&id.to_string()),
            Cell::Time(requested),
            decided.map_or(Cell::Empty, Cell::Time),
            Cell::Text(&origin),
            Cell::Text(&status),
            Cell::Number(amount.into()),
        ])?;
    }
    Ok(csv.download("attachment; filename=\"billing-topups.csv\""))
}
