//! 组织预算通知在控制面异步读取账务数据，摄入与网关不会等待评估或发送。

mod worker;

use axum::extract::State;
use axum::http::{header, HeaderName};
use axum::Json;
use futures::FutureExt;
use reqwest::Url;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{password::normalize_email, verification::safe_recipient, MemberAuth};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::{ApiJson, MANAGERS};
use crate::{tenants, AppState};

pub use worker::{reserve, spawn};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertSettingsInput {
    low_balance_micro_cny: Option<i64>,
    budget_percent: Option<i32>,
    spend_spike_percent: Option<i32>,
    email: Option<String>,
    #[serde(default, deserialize_with = "webhook_replacement")]
    webhook_url: Option<Option<String>>,
}

// 字段缺失保留现有密封地址；显式 null 移除它。
fn webhook_replacement<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Default, Serialize, sqlx::FromRow)]
pub struct AlertSettings {
    low_balance_micro_cny: Option<i64>,
    budget_percent: Option<i32>,
    spend_spike_percent: Option<i32>,
    email: Option<String>,
    webhook_configured: bool,
}

type SettingsResponse = ([(HeaderName, &'static str); 1], Json<AlertSettings>);

fn response(settings: AlertSettings) -> SettingsResponse {
    ([(header::CACHE_CONTROL, "no-store")], Json(settings))
}

fn invalid(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_owned(),
        message: message.to_owned(),
    }])
}

fn validate_webhook(raw: &str) -> Result<(), ApiError> {
    let invalid_url = || invalid("/webhook_url", "请输入使用默认端口的公网 HTTPS 域名地址");
    if raw.len() > 2048 || raw.chars().any(char::is_whitespace) {
        return Err(invalid_url());
    }
    let url = Url::parse(raw).map_err(|_| invalid_url())?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
        || !url
            .domain()
            .is_some_and(|host| host.trim_end_matches('.').contains('.'))
    {
        return Err(invalid_url());
    }
    Ok(())
}

fn webhook_aad(tenant_id: Uuid) -> Vec<u8> {
    [
        b"aisix.billing.alerts.webhook.v1".as_slice(),
        tenant_id.as_bytes(),
    ]
    .concat()
}

pub async fn get(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<SettingsResponse, ApiError> {
    auth.require_active()?;
    auth.require(MANAGERS)?;
    let settings = sqlx::query_as(
        "SELECT low_balance_micro_cny, budget_percent, spend_spike_percent, email,
                sealed_webhook IS NOT NULL AS webhook_configured
         FROM billing_alert_settings WHERE tenant_id = $1",
    )
    .bind(auth.tenant_id)
    .fetch_optional(state.db.pool())
    .await
    .map_err(DbError::from)?;
    Ok(response(settings.unwrap_or_default()))
}

pub async fn put(
    auth: MemberAuth,
    State(state): State<AppState>,
    ApiJson(input): ApiJson<AlertSettingsInput>,
) -> Result<SettingsResponse, ApiError> {
    auth.require_active()?;
    auth.require(MANAGERS)?;
    if input
        .low_balance_micro_cny
        .is_some_and(|value| !(0..=99_999_999_990_000).contains(&value) || value % 10_000 != 0)
    {
        return Err(invalid(
            "/low_balance_micro_cny",
            "请输入 0 到 99999999.99 元，最多两位小数",
        ));
    }
    if input
        .budget_percent
        .is_some_and(|value| !(1..=100).contains(&value))
    {
        return Err(invalid("/budget_percent", "请输入 1 到 100 的整数"));
    }
    if input
        .spend_spike_percent
        .is_some_and(|value| !(101..=1_000).contains(&value))
    {
        return Err(invalid("/spend_spike_percent", "请输入 101 到 1000 的整数"));
    }
    let email = input
        .email
        .as_deref()
        .map(|email| {
            normalize_email(email)
                .filter(|address| safe_recipient(address))
                .ok_or_else(|| invalid("/email", "请输入有效邮箱地址"))
        })
        .transpose()?;
    let sealed = match input.webhook_url.as_ref() {
        Some(Some(raw)) => {
            validate_webhook(raw)?;
            Some(
                state
                    .cfg
                    .master_key
                    .seal_text(&webhook_aad(auth.tenant_id), raw),
            )
        }
        _ => None,
    };

    let settings = state
        .db
        .tx(move |conn| async move { save(conn, auth, input, email, sealed).await }.boxed())
        .await?;
    Ok(response(settings))
}

async fn save(
    conn: &mut PgConnection,
    auth: MemberAuth,
    input: AlertSettingsInput,
    email: Option<String>,
    sealed: Option<String>,
) -> Result<AlertSettings, ApiError> {
    // 与成员角色、停用状态更新串行。通知配置从不锁定账务账户。
    tenants::lock(&mut *conn, auth.tenant_id).await?;
    let current = MemberAuth::fetch(&mut *conn, auth.id)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;
    current.require_active()?;
    current.require(MANAGERS)?;

    if let Some(email) = &email {
        let allowed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM members
             WHERE tenant_id = $1 AND email = $2 AND role IN ('owner', 'admin')
               AND NOT disabled AND removed_at IS NULL)",
        )
        .bind(auth.tenant_id)
        .bind(email)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
        if !allowed {
            return Err(invalid("/email", "请填写本组织仍启用的所有者或管理员邮箱"));
        }
    }

    let settings: AlertSettings = sqlx::query_as(
        "WITH previous AS MATERIALIZED (
           SELECT * FROM billing_alert_settings WHERE tenant_id = $1 FOR NO KEY UPDATE
         ), rearmed AS (
           UPDATE billing_alert_states a SET breached = false
           FROM previous s WHERE a.tenant_id = s.tenant_id AND a.breached
             AND (
               (a.kind = 'low_balance' AND s.low_balance_micro_cny IS DISTINCT FROM $2)
               OR (a.kind = 'budget_threshold' AND s.budget_percent IS DISTINCT FROM $3)
               OR (a.kind = 'spend_spike' AND s.spend_spike_percent IS DISTINCT FROM $4)
               OR (a.channel = 'email' AND s.email IS DISTINCT FROM $5)
               OR (a.channel = 'webhook' AND $7 AND s.sealed_webhook IS DISTINCT FROM $6)
             )
         )
         INSERT INTO billing_alert_settings
           (tenant_id, low_balance_micro_cny, budget_percent, spend_spike_percent, email, sealed_webhook)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (tenant_id) DO UPDATE SET
           version = billing_alert_settings.version + CASE WHEN
             (billing_alert_settings.low_balance_micro_cny, billing_alert_settings.budget_percent,
              billing_alert_settings.spend_spike_percent, billing_alert_settings.email)
             IS DISTINCT FROM (EXCLUDED.low_balance_micro_cny, EXCLUDED.budget_percent,
                               EXCLUDED.spend_spike_percent, EXCLUDED.email)
             OR ($7 AND billing_alert_settings.sealed_webhook IS DISTINCT FROM EXCLUDED.sealed_webhook)
             THEN 1 ELSE 0 END,
           low_balance_micro_cny = EXCLUDED.low_balance_micro_cny,
           budget_percent = EXCLUDED.budget_percent,
           spend_spike_percent = EXCLUDED.spend_spike_percent,
           email = EXCLUDED.email,
           sealed_webhook = CASE WHEN $7 THEN EXCLUDED.sealed_webhook ELSE billing_alert_settings.sealed_webhook END
         RETURNING low_balance_micro_cny, budget_percent, spend_spike_percent, email,
                   sealed_webhook IS NOT NULL AS webhook_configured",
    )
    .bind(auth.tenant_id)
    .bind(input.low_balance_micro_cny)
    .bind(input.budget_percent)
    .bind(input.spend_spike_percent)
    .bind(&email)
    .bind(sealed)
    .bind(input.webhook_url.is_some())
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;

    if (settings.low_balance_micro_cny.is_some()
        || settings.budget_percent.is_some()
        || settings.spend_spike_percent.is_some())
        && settings.email.is_none()
        && !settings.webhook_configured
    {
        return Err(invalid("/email", "启用通知时至少配置一种发送方式"));
    }
    audit::record(
        conn,
        AuditEntry {
            actor: Actor::Member {
                id: current.id,
                email: &current.email,
            },
            action: "billing.alerts.update",
            target_kind: "billing_alert_settings",
            target_id: Some(auth.tenant_id.to_string()),
            tenant_id: Some(auth.tenant_id),
            detail: json!({
                "low_balance_micro_cny": settings.low_balance_micro_cny,
                "budget_percent": settings.budget_percent,
                "spend_spike_percent": settings.spend_spike_percent,
                "email_configured": settings.email.is_some(),
                "webhook_configured": settings.webhook_configured,
            }),
        },
    )
    .await?;
    Ok(settings)
}
