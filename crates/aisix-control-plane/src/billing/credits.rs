//! Operator credits: an admin operator credits a tenant directly, without a
//! tenant request, to record an offline payment or a compensation.
//!
//! A credit is a top-up inserted already approved (`origin 'operator'`, no
//! requesting member, with its `credit_reason`), so the tenant reads it in its
//! own top-up list beside its requests. In one transaction under the
//! tenant's billing lock (`tenants::lock_billing`), it posts one `topup`
//! ledger entry, settles the balance mark and is audited: when the answer
//! arrives the gateway's document already says what the new balance means.
//! Credits are positive only, and never count toward the pending-request
//! cap.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::ledger::{self, Posting, Source};
use super::marks;
use super::topups::{self, Origin};
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::resources::write::WriteCtx;
use crate::tenants::{self, TenantStatus};
use crate::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCredit {
    amount_cny: String,
    #[serde(default)]
    operation_id: Option<Uuid>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    reference: Option<String>,
}

/// `POST /admin/tenants/{id}/credits` with `{amount_cny, reason,
/// reference?, operation_id?}` → `201 AdminTopup`, posted and settled before the answer.
/// 同一租户重送同一操作返回初次回执。金额、原因或凭证号不同时返回 409。
/// `422` at `/amount_cny` unless it is yuan and fen above 0 and at most
/// ¥99,999,999.99, at `/reason` unless it is 1 to 500 characters, not blank and
/// free of control characters, and at `/reference` when it is over 128
/// characters or holds a control character; `404 not_found` for no such
/// tenant or a pending one.
pub async fn create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<NewCredit>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Admin)?;
    let tenant_id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let reference = req.reference.unwrap_or_default();
    let mut problems = Vec::new();
    let amount = topups::topup_amount(&req.amount_cny).unwrap_or_else(|mut d| {
        problems.append(&mut d);
        0
    });
    let reason = topups::operator_reason(req.reason, "请填写入账原因（1–500 个字符）")
        .unwrap_or_else(|d| {
            problems.push(d);
            String::new()
        });
    problems.extend(topups::reference_problem(&reference));
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let ctx = WriteCtx::of(&state);
    let receipt = state
        .db
        .tx(move |conn| {
            async move {
                // The billing lock: the credit posts to the ledger and moves
                // the balance mark.
                let (tenant, _) = tenants::lock_billing(&mut *conn, tenant_id).await?;
                if tenant.status == TenantStatus::Pending {
                    return Err(ApiError::not_found());
                }
                if let Some(operation_id) = req.operation_id {
                    // 租户锁已经串行化同一租户的所有入账，读到的都是已提交回执。
                    let receipt: Option<Value> = sqlx::query_scalar(
                        "SELECT receipt FROM billing_credit_receipts WHERE tenant_id = $1 AND operation_id = $2",
                    )
                    .bind(tenant_id)
                    .bind(operation_id)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                    if let Some(receipt) = receipt {
                        if receipt["amount_micro_cny"].as_i64() == Some(amount)
                            && receipt["credit_reason"].as_str() == Some(reason.as_str())
                            && receipt["reference"].as_str() == Some(reference.as_str())
                        {
                            return Ok(receipt);
                        }
                        return Err(ApiError::conflict(
                            "credit_operation_conflict",
                            "该入账操作的金额、原因或凭证号与首次提交不一致",
                        ));
                    }
                }
                let row = topups::insert_approved(
                    &mut *conn,
                    tenant_id,
                    amount,
                    Origin::Operator,
                    None,
                    Some(&reason),
                    &reference,
                    auth.id,
                )
                .await?;
                ledger::post(
                    &mut *conn,
                    &[Posting {
                        tenant_id,
                        amount_micro_cny: amount,
                        source: Source::Topup { topup_id: row.id },
                    }],
                )
                .await?;
                marks::settle_locked(&mut *conn, &ctx, tenant_id).await?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "billing.topup.credit",
                        target_kind: "topup",
                        target_id: Some(row.id.to_string()),
                        tenant_id: Some(tenant_id),
                        detail: json!({ "amount_micro_cny": amount, "reason": reason }),
                    },
                )
                .await?;
                let mut receipt = topups::to_admin(&row);
                if let Some(operation_id) = req.operation_id {
                    receipt["operation_id"] = json!(operation_id);
                    sqlx::query(
                        "INSERT INTO billing_credit_receipts (tenant_id, operation_id, topup_id, receipt) \
                         VALUES ($1, $2, $3, $4)",
                    )
                    .bind(tenant_id)
                    .bind(operation_id)
                    .bind(row.id)
                    .bind(&receipt)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                }
                Ok::<_, ApiError>(receipt)
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(receipt)))
}
