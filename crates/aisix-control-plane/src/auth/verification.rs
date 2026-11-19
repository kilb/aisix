//! 租户注册邮箱验证码和 Turnstile 人机校验。

use axum::extract::State;
use axum::http::StatusCode;
use futures::FutureExt;
use rand_core::RngCore;
use reqwest::Url;
use ring::hmac;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::auth::password::normalize_email;
use crate::auth::settings::{self, AuthSettings, SecretScope};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::AppState;

const EMAIL_SEND_LOCK: i64 = 0x6169_7369_7861_656d;
const EMAIL_SENDS_PER_HOUR: i64 = 5;
const GLOBAL_EMAIL_SENDS_PER_MINUTE: i64 = 60;
const GLOBAL_EMAIL_SENDS_PER_HOUR: i64 = 500;
const MAX_CODE_ATTEMPTS: i16 = 5;
const CODE_SPACE: u32 = 1_000_000;
const TURNSTILE_VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
const TURNSTILE_TEST_SECRET: &str = "1x0000000000000000000000000000000AA";
const MAX_TURNSTILE_REPLY: usize = 16 * 1024;
static EMAIL_REQUESTS: Semaphore = Semaphore::const_new(8);
static TURNSTILE_REQUESTS: Semaphore = Semaphore::const_new(32);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailCodeRequest {
    email: String,
    turnstile_token: Option<String>,
}

#[derive(Deserialize)]
struct TurnstileReply {
    success: bool,
    hostname: Option<String>,
    action: Option<String>,
    #[serde(rename = "error-codes", default)]
    errors: Vec<String>,
}

fn invalid(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_owned(),
        message: message.to_owned(),
    }])
}

fn invalid_code() -> ApiError {
    invalid("/email_code", "邮箱验证码错误、已使用或已过期")
}

fn invalid_challenge() -> ApiError {
    invalid("/turnstile_token", "请重新完成人机验证")
}

fn mail_unavailable() -> ApiError {
    ApiError {
        message: "暂时无法发送验证码，请稍后重试".to_owned(),
        ..ApiError::unavailable()
    }
}

fn safe_recipient(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    // 发信服务只应收到一个裸邮箱地址，避免接受地址列表或邮件头语法。
    !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&c))
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

fn generate_code() -> Result<String, ApiError> {
    // 拒绝高位余数，避免取模造成少数验证码更容易被抽中。
    let limit = u32::MAX - u32::MAX % CODE_SPACE;
    loop {
        let mut bytes = [0; 4];
        rand_core::OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(ApiError::internal)?;
        let random = u32::from_le_bytes(bytes);
        if random < limit {
            return Ok(format!("{:06}", random % CODE_SPACE));
        }
    }
}

fn recipient_mac(key: &hmac::Key, email: &str) -> hmac::Tag {
    let mut mac = hmac::Context::with_key(key);
    mac.update(b"aisix.portal.signup.recipient.v1\0");
    mac.update(email.as_bytes());
    mac.sign()
}

fn code_material(settings: &AuthSettings, id: Uuid, code: &str) -> Vec<u8> {
    [
        b"aisix.portal.signup.code.v1\0".as_slice(),
        id.as_bytes(),
        code.as_bytes(),
        settings.email.endpoint.as_bytes(),
        b"\0",
        settings.email.from.as_bytes(),
        b"\0",
        settings
            .email
            .api_key
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
    ]
    .concat()
}

async fn reserve_code(
    state: &AppState,
    checked: AuthSettings,
    id: Uuid,
    recipient: hmac::Tag,
    code: hmac::Tag,
) -> Result<AuthSettings, ApiError> {
    let state = state.clone();
    let db = state.db.clone();
    db.tx(move |conn| {
        async move {
            let current =
                settings::load_locked(&state, conn, SecretScope::EmailAndTurnstile).await?;
            if !same_turnstile(&checked, &current) || !same_email(&checked, &current) {
                return Err(settings_changed());
            }
            let admitted: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(EMAIL_SEND_LOCK)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
            if !admitted {
                return Err(ApiError::busy());
            }
            let (minute, hour, mailbox, cooling): (i64, i64, i64, bool) = sqlx::query_as(
                "SELECT
                       (SELECT count(*) FROM portal_email_codes
                        WHERE issued_at > statement_timestamp() - interval '1 minute'),
                       (SELECT count(*) FROM portal_email_codes
                        WHERE issued_at > statement_timestamp() - interval '1 hour'),
                       (SELECT count(*) FROM portal_email_codes
                        WHERE recipient_mac = $1
                        AND issued_at > statement_timestamp() - interval '1 hour'),
                       EXISTS (SELECT 1 FROM portal_email_codes
                               WHERE recipient_mac = $1
                               AND issued_at > statement_timestamp() - interval '1 minute')",
            )
            .bind(recipient.as_ref())
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::from)?;
            if cooling
                || minute >= GLOBAL_EMAIL_SENDS_PER_MINUTE
                || hour >= GLOBAL_EMAIL_SENDS_PER_HOUR
                || mailbox >= EMAIL_SENDS_PER_HOUR
            {
                return Err(ApiError::busy());
            }
            sqlx::query(
                "INSERT INTO portal_email_codes (id, recipient_mac, code_mac) VALUES ($1, $2, $3)",
            )
            .bind(id)
            .bind(recipient.as_ref())
            .bind(code.as_ref())
            .execute(conn)
            .await
            .map_err(DbError::from)?;
            Ok(checked)
        }
        .boxed()
    })
    .await
}

/// 发信失败时仍有发送记录，只有服务商接收后才替换已有的有效验证码。
async fn activate_code(
    state: &AppState,
    checked: AuthSettings,
    id: Uuid,
    recipient: hmac::Tag,
) -> Result<(), ApiError> {
    let state = state.clone();
    let db = state.db.clone();
    db.tx(move |conn| {
        async move {
            let current =
                settings::load_locked(&state, conn, SecretScope::EmailAndTurnstile).await?;
            if !same_email(&checked, &current) {
                return Err(settings_changed());
            }
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(EMAIL_SEND_LOCK)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
            // 迟到的确认不得覆盖已经送达或使用的更新一封。
            let current_dispatch: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1 FROM portal_email_codes AS candidate
                    WHERE candidate.id = $1
                    AND candidate.issued_at > statement_timestamp() - interval '10 minutes'
                    AND NOT EXISTS (
                        SELECT 1 FROM portal_email_codes AS newer
                        WHERE newer.recipient_mac = $2 AND newer.delivered
                        AND newer.issued_at > candidate.issued_at
                    )
                )",
            )
            .bind(id)
            .bind(recipient.as_ref())
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::from)?;
            if !current_dispatch {
                return Err(mail_unavailable());
            }
            sqlx::query(
                "UPDATE portal_email_codes SET closed = true
                 WHERE recipient_mac = $1 AND delivered AND NOT closed",
            )
            .bind(recipient.as_ref())
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
            sqlx::query("UPDATE portal_email_codes SET delivered = true WHERE id = $1")
                .bind(id)
                .execute(conn)
                .await
                .map_err(DbError::from)?;
            Ok(())
        }
        .boxed()
    })
    .await
}

/// 请求一封注册验证码邮件。先在数据库预留发送配额，再拨发信服务。
pub async fn send_email_code(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<EmailCodeRequest>,
) -> Result<StatusCode, ApiError> {
    let _permit = EMAIL_REQUESTS.try_acquire().map_err(|_| ApiError::busy())?;
    let settings = settings::load(&state, SecretScope::EmailAndTurnstile).await?;
    if !settings.email.enabled {
        return Err(ApiError::not_found());
    }
    let email = normalize_email(&req.email)
        .filter(|email| safe_recipient(email))
        .ok_or_else(|| invalid("/email", "请输入有效邮箱地址"))?;
    verify_turnstile(
        &state,
        &settings,
        req.turnstile_token.as_deref(),
        "email_code",
    )
    .await?;

    let endpoint = Url::parse(&settings.email.endpoint).map_err(|_| mail_unavailable())?;
    let client = state.upstream.public_https_client(&endpoint).await?;
    let id = Uuid::new_v4();
    let code = generate_code()?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, &state.cfg.master_key.0);
    let code_mac = hmac::sign(&key, &code_material(&settings, id, &code));
    let settings =
        reserve_code(&state, settings, id, recipient_mac(&key, &email), code_mac).await?;
    let api_key = settings
        .email
        .api_key
        .as_deref()
        .ok_or_else(mail_unavailable)?;

    let response = client
        .post(endpoint)
        .bearer_auth(api_key)
        .json(&json!({
            "from": settings.email.from,
            "to": email,
            "subject": "Routio 注册验证码",
            "text": format!("您的 Routio 注册验证码为 {code}，10 分钟内有效。")
        }))
        .send()
        .await
        .map_err(|_| mail_unavailable())?;
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "发信接口拒绝了验证码邮件"
        );
        return Err(mail_unavailable());
    }
    activate_code(&state, settings, id, recipient_mac(&key, &email)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 先提交猜测次数，再核对摘要。即使随后的注册事务回滚，猜测也计数。
async fn check_signup_code(
    state: &AppState,
    settings: &AuthSettings,
    email: &str,
    code: Option<&str>,
) -> Result<Uuid, ApiError> {
    let code = code
        .filter(|code| code.len() == 6 && code.bytes().all(|c| c.is_ascii_digit()))
        .ok_or_else(invalid_code)?;
    if !safe_recipient(email) {
        return Err(invalid("/email", "请输入有效邮箱地址"));
    }
    let key = hmac::Key::new(hmac::HMAC_SHA256, &state.cfg.master_key.0);
    let hash = recipient_mac(&key, email);
    let result: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "UPDATE portal_email_codes SET attempts = attempts + 1
         WHERE recipient_mac = $1 AND NOT closed AND delivered
         AND issued_at > statement_timestamp() - interval '10 minutes' AND attempts < $2
         RETURNING id, code_mac",
    )
    .bind(hash.as_ref())
    .bind(MAX_CODE_ATTEMPTS)
    .fetch_optional(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let (id, code_mac) = result.ok_or_else(invalid_code)?;
    hmac::verify(&key, &code_material(settings, id, code), &code_mac)
        .map_err(|_| invalid_code())?;
    Ok(id)
}

/// 验证码只在对应注册事务提交时消耗，替换、超时和并发重放都会被拒绝。
async fn consume_signup_code(conn: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
    let applied = sqlx::query(
        "UPDATE portal_email_codes SET closed = true
         WHERE id = $1 AND NOT closed AND delivered
         AND issued_at > statement_timestamp() - interval '10 minutes'",
    )
    .bind(id)
    .execute(conn)
    .await
    .map_err(DbError::from)?;
    if applied.rows_affected() == 1 {
        Ok(())
    } else {
        Err(invalid_code())
    }
}

fn settings_changed() -> ApiError {
    ApiError::conflict(
        "authentication_settings_changed",
        "认证设置已更新，请刷新后重新提交",
    )
}

fn same_email(checked: &AuthSettings, current: &AuthSettings) -> bool {
    checked.email.enabled == current.email.enabled
        && (!current.email.enabled
            || (checked.email.endpoint == current.email.endpoint
                && checked.email.from == current.email.from
                && checked.email.api_key == current.email.api_key))
}

fn same_turnstile(checked: &AuthSettings, current: &AuthSettings) -> bool {
    checked.turnstile.enabled == current.turnstile.enabled
        && (!current.turnstile.enabled
            || (checked.portal_origin == current.portal_origin
                && checked.turnstile.site_key == current.turnstile.site_key
                && checked.turnstile.secret_key == current.turnstile.secret_key))
}

pub(crate) struct SignupProof {
    settings: AuthSettings,
    email_code: Option<Uuid>,
}

/// 在取密码哈希前完成第三方校验，避免在认证事务内等待外部服务。
pub(crate) async fn verify_signup(
    state: &AppState,
    email: &str,
    code: Option<&str>,
    challenge: Option<&str>,
) -> Result<SignupProof, ApiError> {
    let settings = settings::load(state, SecretScope::EmailAndTurnstile).await?;
    verify_turnstile(state, &settings, challenge, "signup").await?;
    let email_code = if settings.email.enabled {
        Some(check_signup_code(state, &settings, email, code).await?)
    } else {
        None
    };
    Ok(SignupProof {
        settings,
        email_code,
    })
}

impl SignupProof {
    pub(crate) async fn consume(
        self,
        state: &AppState,
        conn: &mut PgConnection,
    ) -> Result<(), ApiError> {
        let current = settings::load_locked(state, conn, SecretScope::EmailAndTurnstile).await?;
        if !same_turnstile(&self.settings, &current) || !same_email(&self.settings, &current) {
            return Err(settings_changed());
        }
        if let Some(id) = self.email_code {
            consume_signup_code(conn, id).await?;
        }
        Ok(())
    }
}

pub(crate) async fn ensure_turnstile_current(
    state: &AppState,
    conn: &mut PgConnection,
    checked: &AuthSettings,
) -> Result<(), ApiError> {
    let current = settings::load_locked(state, conn, SecretScope::Turnstile).await?;
    if same_turnstile(checked, &current) {
        Ok(())
    } else {
        Err(settings_changed())
    }
}

/// 每次提交在服务端调用 Siteverify，按门户主机名和当前操作验证返回值。
/// https://developers.cloudflare.com/turnstile/get-started/server-side-validation/
pub(crate) async fn verify_turnstile(
    state: &AppState,
    settings: &AuthSettings,
    token: Option<&str>,
    action: &str,
) -> Result<(), ApiError> {
    if !settings.turnstile.enabled {
        return Ok(());
    }
    let token = token
        .filter(|token| !token.trim().is_empty() && token.len() <= 2048)
        .ok_or_else(invalid_challenge)?;
    let secret = settings
        .turnstile
        .secret_key
        .as_deref()
        .ok_or_else(ApiError::unavailable)?;
    if secret == TURNSTILE_TEST_SECRET && !state.cfg.http_addr.ip().is_loopback() {
        return Err(ApiError::unavailable());
    }
    let portal = Url::parse(&settings.portal_origin).map_err(|_| ApiError::unavailable())?;
    let hostname = portal.host_str().ok_or_else(ApiError::unavailable)?;
    let endpoint = Url::parse(TURNSTILE_VERIFY_URL).map_err(ApiError::internal)?;
    let client = state.upstream.public_https_client(&endpoint).await?;
    let _permit = TURNSTILE_REQUESTS
        .try_acquire()
        .map_err(|_| ApiError::busy())?;
    let mut response = client
        .post(endpoint)
        .json(&json!({"secret": secret, "response": token}))
        .send()
        .await
        .map_err(|_| ApiError::unavailable())?;
    if !response.status().is_success() {
        return Err(ApiError::unavailable());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ApiError::unavailable())?
    {
        if chunk.len() > MAX_TURNSTILE_REPLY - body.len() {
            return Err(ApiError::unavailable());
        }
        body.extend_from_slice(&chunk);
    }
    let result: TurnstileReply =
        serde_json::from_slice(&body).map_err(|_| ApiError::unavailable())?;
    if result.errors.iter().any(|error| {
        matches!(
            error.as_str(),
            "internal-error" | "invalid-input-secret" | "missing-input-secret"
        )
    }) {
        return Err(ApiError::unavailable());
    }
    if challenge_matches(&result, secret, hostname, action) {
        Ok(())
    } else {
        Err(invalid_challenge())
    }
}

fn challenge_matches(result: &TurnstileReply, secret: &str, hostname: &str, action: &str) -> bool {
    // 官方测试密钥固定返回 example.com，且从不返回 action。仅回环门户接受该响应。
    // https://developers.cloudflare.com/turnstile/troubleshooting/testing/
    if secret == TURNSTILE_TEST_SECRET {
        return result.success
            && matches!(hostname, "localhost" | "127.0.0.1" | "[::1]")
            && result.hostname.as_deref() == Some("example.com")
            && result.action.is_none();
    }
    result.success
        && result.hostname.as_deref() == Some(hostname)
        && result.action.as_deref() == Some(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siteverify_response_must_match_the_host_and_action() {
        let result = TurnstileReply {
            success: true,
            hostname: Some("portal.example.com".to_owned()),
            action: Some("login".to_owned()),
            errors: Vec::new(),
        };
        assert!(challenge_matches(
            &result,
            "secret",
            "portal.example.com",
            "login"
        ));
        assert!(!challenge_matches(
            &result,
            "secret",
            "portal.example.com",
            "signup"
        ));
        assert!(!challenge_matches(
            &result,
            "secret",
            "different.example.com",
            "login"
        ));
    }

    #[test]
    fn siteverify_dummy_response_is_never_accepted_for_a_public_portal() {
        let result = TurnstileReply {
            success: true,
            hostname: Some("example.com".to_owned()),
            action: None,
            errors: Vec::new(),
        };
        assert!(challenge_matches(
            &result,
            TURNSTILE_TEST_SECRET,
            "localhost",
            "login"
        ));
        assert!(!challenge_matches(
            &result,
            TURNSTILE_TEST_SECRET,
            "example.com",
            "login"
        ));
        assert!(!challenge_matches(&result, "secret", "localhost", "login"));
    }

    #[test]
    fn email_recipient_cannot_be_interpreted_as_multiple_addresses_or_headers() {
        assert!(safe_recipient("tester+signup@example.com"));
        for invalid in [
            "one@example.com,two@example.com",
            "u@example.com\r\nCc:private@example.com",
            "Someone <user@example.com>",
            "u@-invalid-domain.example.com",
            "u@example..com",
        ] {
            assert!(!safe_recipient(invalid));
        }
    }
}
