//! 租户口令找回。限流记录与邮件投递分开，入口响应不随账号状态变化。

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use futures::FutureExt;
use ring::hmac;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::password;
use crate::auth::session::{self, Principal};
use crate::auth::settings::{self, AuthSettings, SecretScope};
use crate::auth::verification::{self, safe_recipient};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::AppState;

const REQUEST_LOCK: i64 = 0x6169_7369_7861_7270;
const ACTIVATION_LOCK: i64 = 0x6169_7369_7861_7271;
const MAX_ATTEMPTS: i16 = 5;
const SENDS_PER_MAILBOX_HOUR: i64 = 5;
const SENDS_PER_MINUTE: i64 = 60;
const SENDS_PER_HOUR: i64 = 500;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
static DELIVERIES: Semaphore = Semaphore::const_new(16);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgotRequest {
    email: String,
    turnstile_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetRequest {
    email: String,
    email_code: String,
    new_password: String,
}

fn invalid(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_owned(),
        message: message.to_owned(),
    }])
}

fn invalid_code() -> ApiError {
    invalid("/email_code", "验证码错误、已失效或已过期")
}

fn email_address(raw: &str) -> Result<String, ApiError> {
    password::normalize_email(raw)
        .filter(|address| safe_recipient(address))
        .ok_or_else(|| invalid("/email", "请输入有效邮箱地址"))
}

fn enabled(config: &AuthSettings) -> bool {
    config.password_recovery_enabled == Some(true)
        && config.email.enabled
        && config.email.api_key.is_some()
}

fn policy_current(checked: &AuthSettings, current: &AuthSettings) -> bool {
    checked.version == current.version && enabled(current)
}

fn key(state: &AppState) -> hmac::Key {
    hmac::Key::new(hmac::HMAC_SHA256, &state.cfg.master_key.0)
}

fn recipient_mac(key: &hmac::Key, email: &str) -> hmac::Tag {
    let mut mac = hmac::Context::with_key(key);
    mac.update(b"aisix.portal.recovery.recipient.v1\0");
    mac.update(email.as_bytes());
    mac.sign()
}

/// 绑定认证配置版本、成员及其当前口令。变更口令或认证设置后，已发送的旧码失效。
fn code_material(config: &AuthSettings, id: Uuid, member: Uuid, hash: &str, code: &str) -> Vec<u8> {
    [
        b"aisix.portal.recovery.code.v1\0".as_slice(),
        id.as_bytes(),
        member.as_bytes(),
        &config.version.to_be_bytes(),
        code.as_bytes(),
        hash.as_bytes(),
    ]
    .concat()
}

/// 对每个邮箱以相同方式核对限额，预留提交后才异步查账号和发信。
pub async fn forgot(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<ForgotRequest>,
) -> Result<StatusCode, ApiError> {
    let config = settings::load(&state, SecretScope::EmailAndTurnstile).await?;
    if !enabled(&config) {
        return Err(ApiError::not_found());
    }
    let email = email_address(&req.email)?;
    verification::verify_turnstile(
        &state,
        &config,
        req.turnstile_token.as_deref(),
        "password_recovery",
    )
    .await?;
    let permit = DELIVERIES.try_acquire().map_err(|_| ApiError::busy())?;
    let id = Uuid::new_v4();
    let recipient = recipient_mac(&key(&state), &email);
    let tx_state = state.clone();
    let reserved = state
        .db
        .tx(move |conn| {
            async move {
                let current =
                    settings::load_locked(&tx_state, conn, SecretScope::EmailAndTurnstile).await?;
                if !policy_current(&config, &current) {
                    return Err(ApiError::conflict(
                        "authentication_settings_changed",
                        "认证设置已更新，请刷新后重新提交",
                    ));
                }
                Ok(reserve(conn, id, recipient.as_ref())
                    .await?
                    .then_some(current))
            }
            .boxed()
        })
        .await?;

    if let Some(config) = reserved {
        let deadline = tokio::time::Instant::now() + DELIVERY_TIMEOUT;
        tokio::spawn(async move {
            let _permit = permit;
            match tokio::time::timeout_at(deadline, deliver(&state, config, id, &email)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(code = %error.code, "未能投递口令找回邮件"),
                Err(_) => tracing::warn!("口令找回邮件投递超时"),
            }
            // 占用发送容量的时长固定，繁忙响应不会反映该邮箱是否存在或投递结果。
            tokio::time::sleep_until(deadline).await;
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn reserve(conn: &mut PgConnection, id: Uuid, recipient: &[u8]) -> Result<bool, ApiError> {
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(REQUEST_LOCK)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
    if !locked {
        return Err(ApiError::busy());
    }
    let (minute, hour, mailbox, cooling): (i64, i64, i64, bool) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM portal_password_recovery
             WHERE issued_at > statement_timestamp() - interval '1 minute'),
            (SELECT count(*) FROM portal_password_recovery
             WHERE issued_at > statement_timestamp() - interval '1 hour'),
            (SELECT count(*) FROM portal_password_recovery
             WHERE recipient_mac = $1 AND issued_at > statement_timestamp() - interval '1 hour'),
            EXISTS (SELECT 1 FROM portal_password_recovery
                    WHERE recipient_mac = $1 AND issued_at > statement_timestamp() - interval '1 minute')",
    )
    .bind(recipient)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if cooling
        || mailbox >= SENDS_PER_MAILBOX_HOUR
        || minute >= SENDS_PER_MINUTE
        || hour >= SENDS_PER_HOUR
    {
        return Ok(false);
    }
    sqlx::query("INSERT INTO portal_password_recovery (id, recipient_mac) VALUES ($1, $2)")
        .bind(id)
        .bind(recipient)
        .execute(conn)
        .await
        .map_err(DbError::from)?;
    Ok(true)
}

async fn deliver(
    state: &AppState,
    config: AuthSettings,
    id: Uuid,
    email: &str,
) -> Result<(), ApiError> {
    // 与成员口令登录一致，待审核或暂停组织的在职成员仍可恢复口令。
    let member: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, password_hash FROM members WHERE email = $1 AND NOT disabled AND removed_at IS NULL",
    )
    .bind(email)
    .fetch_optional(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let Some((member, hash)) = member else {
        return Ok(());
    };
    let current = settings::load(state, SecretScope::EmailAndTurnstile).await?;
    if !policy_current(&config, &current) {
        return Ok(());
    }
    let code = verification::generate_code()?;
    let material = code_material(&config, id, member, &hash, &code);
    let key = key(state);
    let recipient = recipient_mac(&key, email);
    let proof = hmac::sign(&key, &material);
    verification::send_configured_email(
        state,
        &config.email,
        email,
        "Routio 口令找回验证码",
        &format!("您的 Routio 口令找回验证码为 {code}，10 分钟内有效。如果您没有请求找回口令，可忽略此邮件。"),
        None,
    )
    .await?;

    // 发信失败不会覆盖前一封。迟到的确认也不能覆盖由更新请求发出的验证码。
    let state = state.clone();
    let db = state.db.clone();
    db.tx(move |conn| {
        async move {
            let current =
                settings::load_locked(&state, conn, SecretScope::EmailAndTurnstile).await?;
            if !policy_current(&config, &current) {
                return Ok(());
            }
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(ACTIVATION_LOCK)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
            let can_activate: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1 FROM portal_password_recovery AS request
                    WHERE request.id = $1 AND NOT request.closed
                    AND request.issued_at > statement_timestamp() - interval '10 minutes'
                    AND NOT EXISTS (
                        SELECT 1 FROM portal_password_recovery AS newer
                        WHERE newer.recipient_mac = $2 AND newer.code_mac IS NOT NULL
                        AND newer.issued_at > request.issued_at
                    )
                )",
            )
            .bind(id)
            .bind(recipient.as_ref())
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::from)?;
            if !can_activate {
                return Ok(());
            }
            sqlx::query(
                "UPDATE portal_password_recovery SET closed = true
                 WHERE recipient_mac = $1 AND code_mac IS NOT NULL AND NOT closed",
            )
            .bind(recipient.as_ref())
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
            sqlx::query(
                "UPDATE portal_password_recovery SET member_id = $2, code_mac = $3 WHERE id = $1",
            )
            .bind(id)
            .bind(member)
            .bind(proof.as_ref())
            .execute(conn)
            .await
            .map_err(DbError::from)?;
            Ok(())
        }
        .boxed()
    })
    .await
}

pub async fn reset(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<ResetRequest>,
) -> Result<StatusCode, ApiError> {
    let config = settings::load(&state, SecretScope::EmailAndTurnstile).await?;
    if !enabled(&config) {
        return Err(ApiError::not_found());
    }
    let email = email_address(&req.email)?;
    if !password::long_enough(&req.new_password) {
        return Err(invalid(
            "/new_password",
            &format!("密码至少需要 {} 个字符", password::MIN_PASSWORD_LEN),
        ));
    }
    let code = &req.email_code;
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_code());
    }

    // 猜测次数单独提交，校验错误或后续事务回滚都不会恢复尝试次数。
    let key = key(&state);
    let recipient = recipient_mac(&key, &email);
    let candidate: Option<(Uuid, Vec<u8>, Uuid, Uuid, String)> = sqlx::query_as(
        "UPDATE portal_password_recovery AS recovery SET attempts = recovery.attempts + 1
         FROM members AS member
         WHERE recovery.recipient_mac = $1 AND NOT recovery.closed
         AND recovery.issued_at > statement_timestamp() - interval '10 minutes' AND recovery.attempts < $2
         AND member.id = recovery.member_id AND member.email = $3
         AND NOT member.disabled AND member.removed_at IS NULL
         RETURNING recovery.id, recovery.code_mac, member.id, member.tenant_id, member.password_hash",
    )
    .bind(recipient.as_ref())
    .bind(MAX_ATTEMPTS)
    .bind(&email)
    .fetch_optional(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let (id, proof, member, tenant, old_hash) = candidate.ok_or_else(invalid_code)?;
    hmac::verify(
        &key,
        &code_material(&config, id, member, &old_hash, code),
        &proof,
    )
    .map_err(|_| invalid_code())?;
    let new_hash = state.passwords.hash(&req.new_password).await?;

    let tx_state = state.clone();
    let tx_email = email.clone();
    state.db.tx(move |conn| {
        async move {
            let current = settings::load_locked(&tx_state, conn, SecretScope::EmailAndTurnstile).await?;
            if !policy_current(&config, &current) {
                return Err(invalid_code());
            }
            let updated = sqlx::query(
                "UPDATE members SET password_hash = $2
                 WHERE id = $1 AND password_hash = $3 AND email = $4 AND NOT disabled AND removed_at IS NULL",
            )
            .bind(member)
            .bind(new_hash)
            .bind(old_hash)
            .bind(&tx_email)
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
            if updated.rows_affected() != 1 {
                return Err(invalid_code());
            }
            let consumed = sqlx::query(
                "UPDATE portal_password_recovery SET closed = true
                 WHERE id = $1 AND NOT closed AND issued_at > statement_timestamp() - interval '10 minutes'",
            )
            .bind(id)
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
            if consumed.rows_affected() != 1 {
                return Err(invalid_code());
            }
            session::revoke_all(&mut *conn, Principal::Member(member), None).await?;
            audit::record(
                conn,
                AuditEntry {
                    actor: Actor::Member { id: member, email: &tx_email },
                    action: "member.password_recovered",
                    target_kind: "member",
                    target_id: Some(member.to_string()),
                    tenant_id: Some(tenant),
                    detail: json!({}),
                },
            )
            .await?;
            Ok::<_, ApiError>(())
        }
        .boxed()
    }).await?;
    state.portal.logins.clear(&email);
    Ok(StatusCode::NO_CONTENT)
}
