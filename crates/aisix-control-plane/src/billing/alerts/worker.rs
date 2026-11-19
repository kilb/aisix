use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use reqwest::Url;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::{validate_webhook, webhook_aad};
use crate::auth::{
    settings::{self, SecretScope},
    verification::send_configured_email,
};
use crate::billing::money::cny_string;
use crate::db::DbError;
use crate::AppState;

const TICK: Duration = Duration::from_secs(5);
const BATCH_SIZE: i64 = 32;
const PARALLEL_TENANTS: usize = 4;
const RESERVE_SQL: &str = include_str!("reserve.sql");

#[derive(sqlx::FromRow, Serialize)]
pub struct Alert {
    pub kind: String,
    pub observed_micro_cny: i64,
    pub threshold_micro_cny: i64,
    pub window_start: Option<DateTime<Utc>>,
    pub delivery_id: Uuid,
    pub measured_at: DateTime<Utc>,
    #[serde(skip)]
    pub channel: String,
    #[serde(skip)]
    attempts: i32,
}

/// 评估并认领一个组织的通知。只持有通知配置行的锁，归还连接后再发送。
pub async fn reserve(
    state: &AppState,
    tenant_id: Uuid,
    email_available: bool,
) -> Result<Vec<Alert>, DbError> {
    match reserve_signals(state, tenant_id, email_available, true).await {
        Err(DbError::Db(sqlx::Error::Database(error)))
            if error.code().as_deref() == Some("57014") =>
        {
            // 大组织的消费聚合达到时限时，余额和预算通知仍能投递。
            tracing::warn!(%tenant_id, "预算通知的消费聚合超时");
            reserve_signals(state, tenant_id, email_available, false).await
        }
        result => result,
    }
}

async fn reserve_signals(
    state: &AppState,
    tenant_id: Uuid,
    email_available: bool,
    measure_spend: bool,
) -> Result<Vec<Alert>, DbError> {
    let mut tx = state.db.pool().begin().await?;
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(state.cfg.db_statement_timeout_ms.min(2_000).to_string())
        .execute(&mut *tx)
        .await?;
    let alerts = sqlx::query_as(RESERVE_SQL)
        .bind(tenant_id)
        .bind(email_available)
        .bind(measure_spend)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(alerts)
}

/// 在工作进程中有界扫过已启用的组织，每个副本独立扫描，认领由数据库决定。
pub fn spawn(mut state: AppState, mut stop: watch::Receiver<bool>) -> JoinHandle<()> {
    state.db = state.db.dedicated_within(2, Duration::from_secs(2));
    tokio::spawn(async move {
        let mut cursor = Uuid::nil();
        let mut ticks = tokio::time::interval(TICK);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = crate::stopped(&mut stop) => return,
                _ = ticks.tick() => {}
            }
            tokio::select! {
                biased;
                _ = crate::stopped(&mut stop) => return,
                result = scan_batch(&state, &mut cursor) => {
                    if result.is_err() {
                        tracing::warn!("预算通知无法读取下一批组织");
                    }
                }
            }
        }
    })
}

async fn scan_batch(state: &AppState, cursor: &mut Uuid) -> Result<(), DbError> {
    let tenants: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM billing_alert_settings
         WHERE tenant_id > $1
           AND (low_balance_micro_cny IS NOT NULL OR budget_percent IS NOT NULL OR spend_spike_percent IS NOT NULL)
           AND (email IS NOT NULL OR sealed_webhook IS NOT NULL)
         ORDER BY tenant_id LIMIT $2",
    )
    .bind(*cursor)
    .bind(BATCH_SIZE)
    .fetch_all(state.db.pool())
    .await?;
    *cursor = if tenants.len() == BATCH_SIZE as usize {
        *tenants.last().unwrap_or(&Uuid::nil())
    } else {
        Uuid::nil()
    };
    if tenants.is_empty() {
        return Ok(());
    }
    let email_available = settings::load(state, SecretScope::Flags)
        .await
        .is_ok_and(|settings| settings.email.enabled);
    stream::iter(tenants)
        .map(|tenant_id| process_tenant(state, tenant_id, email_available))
        .buffer_unordered(PARALLEL_TENANTS)
        .for_each(|()| async {})
        .await;
    Ok(())
}

async fn process_tenant(state: &AppState, tenant_id: Uuid, email_available: bool) {
    let alerts = match reserve(state, tenant_id, email_available).await {
        Ok(alerts) => alerts,
        Err(_) => {
            tracing::warn!(%tenant_id, "预算通知评估未完成");
            return;
        }
    };
    for alert in alerts {
        let Some(outcome) = deliver(state, tenant_id, &alert).await else {
            continue;
        };
        let updated = sqlx::query(
            "UPDATE billing_alert_states SET outcome = $4,
              next_attempt_at = CASE WHEN $4 = 'failed' THEN statement_timestamp() + interval '5 minutes'
                                ELSE next_attempt_at END
             WHERE tenant_id = $1 AND kind = $2 AND channel = $3 AND delivery_id = $5
               AND attempts = $6 AND outcome = 'claimed'",
        )
        .bind(tenant_id)
        .bind(&alert.kind)
        .bind(&alert.channel)
        .bind(outcome)
        .bind(alert.delivery_id)
        .bind(alert.attempts)
        .execute(state.db.pool())
        .await;
        if outcome == "failed" || updated.is_err() {
            // 地址、发信凭据以及响应体可能包含秘密，仅记录系统生成的标识。
            tracing::warn!(%tenant_id, delivery_id = %alert.delivery_id, channel = %alert.channel, "预算通知发送或记录未完成");
        }
    }
}

async fn deliver(state: &AppState, tenant_id: Uuid, alert: &Alert) -> Option<&'static str> {
    let mail = if alert.channel == "email" {
        match settings::load(state, SecretScope::EmailAndTurnstile).await {
            Ok(settings) => Some(settings.email),
            Err(_) => return Some("failed"),
        }
    } else {
        None
    };
    // 拨号前确认尝试仍归当前工作进程，并重新检查组织状态、设置和收件人权限。
    let target: Result<Option<(Option<String>,)>, _> = sqlx::query_as(
        "SELECT (
           SELECT CASE WHEN h.channel = 'email' THEN s.email ELSE s.sealed_webhook END
           FROM billing_alert_settings s JOIN tenants t ON t.id = s.tenant_id
           WHERE s.tenant_id = h.tenant_id AND s.version = h.version AND t.status = 'active'
             AND (h.channel <> 'email' OR EXISTS (
               SELECT 1 FROM members m WHERE m.tenant_id = s.tenant_id AND m.email = s.email
               AND m.role IN ('owner', 'admin') AND NOT m.disabled AND m.removed_at IS NULL
             ))
         )
         FROM billing_alert_states h
         WHERE h.tenant_id = $1 AND h.kind = $2 AND h.channel = $3 AND h.delivery_id = $4
           AND h.attempts = $5 AND h.breached AND h.outcome = 'claimed'
           AND h.next_attempt_at > statement_timestamp()",
    )
    .bind(tenant_id)
    .bind(&alert.kind)
    .bind(&alert.channel)
    .bind(alert.delivery_id)
    .bind(alert.attempts)
    .fetch_optional(state.db.pool())
    .await;
    let target = match target {
        Ok(Some((Some(target),))) => target,
        Ok(Some((None,))) => return Some("skipped"),
        Ok(None) => return None,
        Err(_) => return Some("failed"),
    };
    let sent = match alert.channel.as_str() {
        "email" => {
            let Some(mail) = mail.filter(|mail| mail.enabled && mail.api_key.is_some()) else {
                return Some("skipped");
            };
            let (subject, headline) = match alert.kind.as_str() {
                "low_balance" => ("Routio 可用额度提醒", "组织可用额度已达到或低于设定值。"),
                "budget_threshold" => ("Routio 组织预算提醒", "组织预算使用率已达到设定值。"),
                _ => (
                    "Routio 消费突增提醒",
                    "今日消费已达到最近七日基准的设定倍数。",
                ),
            };
            let text = format!(
                "{headline}\n组织 ID：{tenant_id}\n当前金额：¥{}\n触发金额：¥{}",
                cny_string(alert.observed_micro_cny),
                cny_string(alert.threshold_micro_cny),
            );
            send_configured_email(
                state,
                &mail,
                &target,
                subject,
                &text,
                Some(alert.delivery_id),
            )
            .await
            .is_ok()
        }
        "webhook" => send_webhook(state, tenant_id, &target, alert).await,
        _ => false,
    };
    Some(if sent { "sent" } else { "failed" })
}

async fn send_webhook(state: &AppState, tenant_id: Uuid, sealed: &str, alert: &Alert) -> bool {
    let Ok(raw) = state
        .cfg
        .master_key
        .open_text(&webhook_aad(tenant_id), sealed)
    else {
        return false;
    };
    if validate_webhook(&raw).is_err() {
        return false;
    }
    let Ok(url) = Url::parse(&raw) else {
        return false;
    };
    let Ok(client) = state.upstream.public_https_client(&url).await else {
        return false;
    };
    #[derive(Serialize)]
    struct Payload<'a> {
        tenant_id: Uuid,
        #[serde(flatten)]
        alert: &'a Alert,
    }
    client
        .post(url)
        .json(&Payload { tenant_id, alert })
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}
