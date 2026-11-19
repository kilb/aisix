//! 租户通行密钥。浏览器只接收挑战选项，WebAuthn 验证状态留在数据库。

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::types::Json as DbJson;
use sqlx::PgConnection;
use tokio::sync::Semaphore;
use uuid::Uuid;
use webauthn_rs::fake::{FakePasskeyDistribution, WebauthnFakeCredentialGenerator};
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Webauthn, WebauthnBuilder,
};

use super::{password::normalize_email, session, settings, verification, MemberAuth};
use crate::audit::{self, Actor, AuditEntry};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::{ApiJson, EVERY_MEMBER};
use crate::{members, AppState};

const LOGIN_BINDING_COOKIE: &str = "__Host-aisix_passkey";
const TTL: Duration = Duration::from_secs(300);
const MAX_PASSKEYS: i64 = 12;
const MAX_CHALLENGES_PER_PURPOSE: i64 = 16;
const MAX_NAME: usize = 64;
static LOGIN_STARTS: Semaphore = Semaphore::const_new(8);

#[derive(Serialize, sqlx::FromRow)]
pub struct PasskeyView {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct PasskeyList {
    passkeys: Vec<PasskeyView>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterStart {
    name: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterFinish {
    challenge_id: Uuid,
    credential: RegisterPublicKeyCredential,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remove {
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginStart {
    email: String,
    turnstile_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginFinish {
    challenge_id: Uuid,
    credential: PublicKeyCredential,
}

#[derive(Serialize, Deserialize)]
struct RegistrationState {
    ceremony: PasskeyRegistration,
    name: String,
    password_hash_digest: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct LoginState {
    ceremony: PasskeyAuthentication,
    revisions: Vec<(Uuid, i64)>,
    turnstile_digest: Option<[u8; 32]>,
}

fn invalid_challenge() -> ApiError {
    ApiError {
        code: "invalid_passkey",
        message: "通行密钥验证失败，请重新开始".to_string(),
        ..ApiError::invalid_credentials()
    }
}

fn passkey_limit() -> ApiError {
    ApiError::conflict("passkey_limit", "已达到通行密钥数量上限，请先移除旧密钥")
}

fn relying_party(config: &settings::AuthSettings) -> Result<(Webauthn, String), ApiError> {
    if !config.passkey.enabled {
        return Err(ApiError::not_found());
    }
    let origin = Url::parse(&config.portal_origin).map_err(ApiError::internal)?;
    let rp_id = origin
        .domain()
        .ok_or_else(|| ApiError::internal("invalid WebAuthn RP ID"))?;
    let webauthn = WebauthnBuilder::new(rp_id, &origin)
        .map_err(ApiError::internal)?
        .rp_name("AISIX")
        .timeout(TTL)
        .build()
        .map_err(ApiError::internal)?;
    Ok((webauthn, rp_id.to_string()))
}

fn challenge_response(id: Uuid, options: impl Serialize) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"challenge_id": id, "options": options})),
    )
        .into_response()
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn turnstile_digest(config: &settings::AuthSettings) -> Result<Option<[u8; 32]>, ApiError> {
    config
        .turnstile
        .enabled
        .then(|| {
            serde_json::to_vec(&config.turnstile)
                .map(|encoded| Sha256::digest(encoded).into())
                .map_err(ApiError::internal)
        })
        .transpose()
}

fn login_cookie(headers: &HeaderMap) -> Option<&str> {
    session::token_from_headers(headers, LOGIN_BINDING_COOKIE)
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

async fn store_challenge<T: Serialize + Send + Sync + 'static>(
    state: &AppState,
    id: Uuid,
    member_id: Uuid,
    purpose: &'static str,
    origin: &str,
    binding: [u8; 32],
    verification: T,
) -> Result<(), ApiError> {
    let origin = origin.to_string();
    state
        .db
        .tx(move |conn| {
            async move {
                // 同一成员的存入操作串行，换浏览器或副本也不能无限累积挑战。
                let available: Option<bool> = sqlx::query_scalar(
                    "SELECT true FROM members WHERE id = $1 AND NOT disabled \
                     AND removed_at IS NULL FOR NO KEY UPDATE",
                )
                .bind(member_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if available.is_none() {
                    return Ok(());
                }
                // 同一浏览器或门户会话重新发起时，替换此前尚未完成的同类挑战。
                sqlx::query(
                    "INSERT INTO portal_passkey_challenges \
                     (id, member_id, purpose, origin, binding_sha256, verification_state, expires_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7)) \
                     ON CONFLICT (member_id, purpose, binding_sha256) DO UPDATE SET id = EXCLUDED.id, \
                     origin = EXCLUDED.origin, verification_state = EXCLUDED.verification_state, \
                     expires_at = EXCLUDED.expires_at",
                )
                .bind(id)
                .bind(member_id)
                .bind(purpose)
                .bind(origin)
                .bind(binding.as_slice())
                .bind(DbJson(verification))
                .bind(TTL.as_secs_f64())
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                sqlx::query(
                    "DELETE FROM portal_passkey_challenges WHERE id IN (\
                         SELECT id FROM portal_passkey_challenges \
                         WHERE member_id = $1 AND purpose = $2 AND id <> $3 \
                         ORDER BY expires_at DESC, id DESC OFFSET $4)",
                )
                .bind(member_id)
                .bind(purpose)
                .bind(id)
                .bind(MAX_CHALLENGES_PER_PURPOSE - 1)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                Ok(())
            }
            .boxed()
        })
        .await
}

async fn take_challenge<T: serde::de::DeserializeOwned + Send + Unpin + 'static>(
    state: &AppState,
    id: Uuid,
    purpose: &str,
    origin: &str,
    binding: [u8; 32],
    member: Option<Uuid>,
) -> Result<(Uuid, T), ApiError> {
    // 独立提交删除。验证失败、并发重试或后续写入回滚都不能复用挑战。
    let row: Option<(Uuid, DbJson<T>)> = sqlx::query_as(
        "DELETE FROM portal_passkey_challenges \
         WHERE id = $1 AND purpose = $2 AND origin = $3 AND binding_sha256 = $4 \
           AND expires_at > now() AND ($5::uuid IS NULL OR member_id = $5) \
         RETURNING member_id, verification_state",
    )
    .bind(id)
    .bind(purpose)
    .bind(origin)
    .bind(binding.as_slice())
    .bind(member)
    .fetch_optional(state.db.pool())
    .await
    .map_err(DbError::from)?;
    row.map(|(member_id, state)| (member_id, state.0))
        .ok_or_else(invalid_challenge)
}

async fn audit_passkey(
    conn: &mut PgConnection,
    auth: &MemberAuth,
    action: &'static str,
    id: Uuid,
) -> Result<(), DbError> {
    audit::record(
        conn,
        AuditEntry {
            actor: Actor::Member {
                id: auth.id,
                email: &auth.email,
            },
            action,
            target_kind: "passkey",
            target_id: Some(id.to_string()),
            tenant_id: Some(auth.tenant_id),
            detail: json!({}),
        },
    )
    .await
}

pub async fn list(
    State(state): State<AppState>,
    auth: MemberAuth,
) -> Result<Json<PasskeyList>, ApiError> {
    auth.require(EVERY_MEMBER)?;
    let passkeys = sqlx::query_as(
        "SELECT id, name, created_at, last_used_at FROM portal_passkeys \
         WHERE member_id = $1 ORDER BY created_at DESC, id DESC",
    )
    .bind(auth.id)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    Ok(Json(PasskeyList { passkeys }))
}

pub async fn register_start(
    State(state): State<AppState>,
    auth: MemberAuth,
    headers: HeaderMap,
    ApiJson(body): ApiJson<RegisterStart>,
) -> Result<Response, ApiError> {
    auth.require(EVERY_MEMBER)?;
    let config = settings::load(&state, settings::SecretScope::Flags).await?;
    let (webauthn, rp_id) = relying_party(&config)?;
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(ApiError::validation(vec![Detail {
            path: "/name".to_string(),
            message: format!("请输入 1 至 {MAX_NAME} 个字符的通行密钥名称"),
        }]));
    }
    let token = members::carried_token(&headers).ok_or_else(ApiError::unauthenticated)?;
    let password_hash = members::verify_member_password(&state, auth.id, &body.password).await?;
    let existing: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT credential_id FROM portal_passkeys WHERE member_id = $1 AND rp_id = $2 LIMIT $3",
    )
    .bind(auth.id)
    .bind(&rp_id)
    .bind(MAX_PASSKEYS)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    if existing.len() as i64 >= MAX_PASSKEYS {
        return Err(passkey_limit());
    }
    let excluded = existing.into_iter().map(Into::into).collect();
    let (options, ceremony) = webauthn
        .start_passkey_registration(auth.id, &auth.email, &auth.email, Some(excluded))
        .map_err(ApiError::internal)?;
    let id = Uuid::new_v4();
    store_challenge(
        &state,
        id,
        auth.id,
        "register",
        &config.portal_origin,
        digest(&token),
        RegistrationState {
            ceremony,
            name: name.to_string(),
            password_hash_digest: digest(&password_hash),
        },
    )
    .await?;
    Ok(challenge_response(id, options))
}

pub async fn register_finish(
    State(state): State<AppState>,
    auth: MemberAuth,
    headers: HeaderMap,
    ApiJson(body): ApiJson<RegisterFinish>,
) -> Result<(StatusCode, Json<PasskeyView>), ApiError> {
    auth.require(EVERY_MEMBER)?;
    let config = settings::load(&state, settings::SecretScope::Flags).await?;
    let (webauthn, rp_id) = relying_party(&config)?;
    let token = members::carried_token(&headers).ok_or_else(ApiError::unauthenticated)?;
    let binding = digest(&token);
    let (_, registration): (_, RegistrationState) = take_challenge(
        &state,
        body.challenge_id,
        "register",
        &config.portal_origin,
        binding,
        Some(auth.id),
    )
    .await?;
    let credential = webauthn
        .finish_passkey_registration(&body.credential, &registration.ceremony)
        .map_err(|_| invalid_challenge())?;
    let db = state.db.clone();
    let view = db
        .tx(move |conn| {
            async move {
                let current =
                    settings::load_locked(&state, conn, settings::SecretScope::Flags).await?;
                if !current.passkey.enabled || current.portal_origin != config.portal_origin {
                    return Err(invalid_challenge());
                }
                // 在成员行串行化注册，同时重查密码是否已经重置、成员是否停用。
                let hash: Option<String> = sqlx::query_scalar(
                    "SELECT password_hash FROM members \
                     WHERE id = $1 AND NOT disabled AND removed_at IS NULL FOR NO KEY UPDATE",
                )
                .bind(auth.id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if !hash.is_some_and(|hash| digest(&hash) == registration.password_hash_digest) {
                    return Err(invalid_challenge());
                }
                session::require_live_member(conn, auth.id, binding.as_slice()).await?;
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM portal_passkeys WHERE member_id = $1 AND rp_id = $2",
                )
                .bind(auth.id)
                .bind(&rp_id)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if count >= MAX_PASSKEYS {
                    return Err(passkey_limit());
                }
                let id = Uuid::new_v4();
                let view: Option<PasskeyView> = sqlx::query_as(
                    "INSERT INTO portal_passkeys \
                     (id, member_id, rp_id, credential_id, public_credential, name) \
                     VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (rp_id, credential_id) DO NOTHING \
                     RETURNING id, name, created_at, last_used_at",
                )
                .bind(id)
                .bind(auth.id)
                .bind(rp_id)
                .bind(credential.cred_id().as_slice())
                .bind(DbJson(&credential))
                .bind(registration.name)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let view =
                    view.ok_or_else(|| ApiError::conflict("passkey_exists", "这把通行密钥已注册"))?;
                audit_passkey(conn, &auth, "member.passkey.add", id).await?;
                Ok(view)
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(view)))
}

pub async fn remove(
    State(state): State<AppState>,
    auth: MemberAuth,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    ApiJson(body): ApiJson<Remove>,
) -> Result<StatusCode, ApiError> {
    auth.require(EVERY_MEMBER)?;
    let token = members::carried_token(&headers).ok_or_else(ApiError::unauthenticated)?;
    let binding = digest(&token);
    let password_hash = members::verify_member_password(&state, auth.id, &body.password).await?;
    state
        .db
        .tx(move |conn| {
            async move {
                let valid: Option<bool> = sqlx::query_scalar(
                    "SELECT true FROM members WHERE id = $1 AND password_hash = $2 \
                     AND NOT disabled AND removed_at IS NULL FOR NO KEY UPDATE",
                )
                .bind(auth.id)
                .bind(password_hash)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if valid.is_none() {
                    return Err(ApiError::invalid_credentials());
                }
                session::require_live_member(conn, auth.id, binding.as_slice()).await?;
                let removed =
                    sqlx::query("DELETE FROM portal_passkeys WHERE id = $1 AND member_id = $2")
                        .bind(id)
                        .bind(auth.id)
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?
                        .rows_affected();
                if removed == 0 {
                    return Err(ApiError::not_found());
                }
                audit_passkey(conn, &auth, "member.passkey.remove", id).await?;
                Ok(())
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn login_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<LoginStart>,
) -> Result<Response, ApiError> {
    let _permit = LOGIN_STARTS.try_acquire().map_err(|_| ApiError::busy())?;
    let config = settings::load(&state, settings::SecretScope::Turnstile).await?;
    let (webauthn, rp_id) = relying_party(&config)?;
    let email = normalize_email(&body.email).ok_or_else(invalid_challenge)?;
    verification::verify_turnstile(
        &state,
        &config,
        body.turnstile_token.as_deref(),
        "passkey_login",
    )
    .await?;

    let entries: Vec<(Uuid, Uuid, i64, DbJson<Passkey>)> = sqlx::query_as(
        "SELECT m.id, p.id, p.revision, p.public_credential \
         FROM members m JOIN portal_passkeys p ON p.member_id = m.id \
         WHERE m.email = $1 AND NOT m.disabled AND m.removed_at IS NULL AND p.rp_id = $2 \
         ORDER BY p.created_at, p.id LIMIT $3",
    )
    .bind(&email)
    .bind(&rp_id)
    .bind(MAX_PASSKEYS)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let member = entries.first().map(|row| row.0);
    let (revisions, credentials): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .map(|(_, id, revision, DbJson(cred))| ((id, revision), cred))
        .unzip();
    let (mut options, ceremony) = webauthn
        .start_passkey_authentication(&credentials)
        .map_err(ApiError::internal)?;
    for credential in &mut options.public_key.allow_credentials {
        credential.transports = None;
    }

    let id = Uuid::new_v4();
    let marker = login_cookie(&headers)
        .map(str::to_string)
        .unwrap_or_else(|| {
            let mut bytes = [0; 32];
            rand_core::OsRng.fill_bytes(&mut bytes);
            hex::encode(bytes)
        });
    let mut options = serde_json::to_value(options).map_err(ApiError::internal)?;
    if let Some(member) = member {
        store_challenge(
            &state,
            id,
            member,
            "login",
            &config.portal_origin,
            digest(&marker),
            LoginState {
                ceremony,
                revisions,
                turnstile_digest: turnstile_digest(&config)?,
            },
        )
        .await?;
    } else {
        // 账号不存在、停用或没有通行密钥时仍返回挑战，减少账号枚举信息。
        let fake_key = ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &state.cfg.master_key.0),
            b"cp/portal/passkey/fake-credential/v1",
        );
        let generator =
            WebauthnFakeCredentialGenerator::<FakePasskeyDistribution>::new(fake_key.as_ref())
                .map_err(ApiError::internal)?;
        let decoys = generator
            .generate(email.as_bytes())
            .map_err(ApiError::internal)?;
        options["publicKey"]["allowCredentials"] = Value::Array(
            decoys
                .into_iter()
                .map(|id| json!({"type": "public-key", "id": id}))
                .collect(),
        );
    }
    let response = challenge_response(id, options);
    Ok((
        [(
            header::SET_COOKIE,
            session::cookie(LOGIN_BINDING_COOKIE, &marker, TTL),
        )],
        response,
    )
        .into_response())
}

pub async fn login_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<LoginFinish>,
) -> Result<Response, ApiError> {
    let config = settings::load(&state, settings::SecretScope::Flags).await?;
    let (webauthn, rp_id) = relying_party(&config)?;
    let marker = login_cookie(&headers).ok_or_else(invalid_challenge)?;
    let (member, login): (_, LoginState) = take_challenge(
        &state,
        body.challenge_id,
        "login",
        &config.portal_origin,
        digest(marker),
        None,
    )
    .await?;
    if body
        .credential
        .get_user_unique_id()
        .is_some_and(|handle| handle != member.as_bytes())
    {
        return Err(invalid_challenge());
    }
    let verified = webauthn
        .finish_passkey_authentication(&body.credential, &login.ceremony)
        .map_err(|_| invalid_challenge())?;
    if !verified.user_verified() {
        return Err(invalid_challenge());
    }
    let carried = members::carried_token(&headers);
    let db = state.db.clone();
    let (token, view) = db
        .tx(move |conn| {
            async move {
                let current =
                    settings::load_locked(&state, conn, settings::SecretScope::Turnstile).await?;
                if !current.passkey.enabled
                    || current.portal_origin != config.portal_origin
                    || turnstile_digest(&current)? != login.turnstile_digest
                {
                    return Err(invalid_challenge());
                }
                // 与停用成员、撤销通行密钥及并发签名计数更新有明确的提交顺序。
                let active: Option<bool> = sqlx::query_scalar(
                    "SELECT true FROM members WHERE id = $1 AND NOT disabled \
                     AND removed_at IS NULL FOR SHARE",
                )
                .bind(member)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if active.is_none() {
                    return Err(invalid_challenge());
                }
                let row: Option<(Uuid, i64, DbJson<Passkey>)> = sqlx::query_as(
                    "SELECT id, revision, public_credential FROM portal_passkeys \
                     WHERE member_id = $1 AND rp_id = $2 AND credential_id = $3 FOR UPDATE",
                )
                .bind(member)
                .bind(rp_id)
                .bind(verified.cred_id().as_slice())
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let Some((id, revision, DbJson(mut credential))) = row else {
                    return Err(invalid_challenge());
                };
                if !login.revisions.contains(&(id, revision)) {
                    return Err(invalid_challenge());
                }
                let changed = credential
                    .update_credential(&verified)
                    .ok_or_else(invalid_challenge)?;
                sqlx::query(
                    "UPDATE portal_passkeys SET last_used_at = now(), \
                     public_credential = coalesce($2, public_credential), revision = revision + $3 \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(changed.then_some(DbJson(credential)))
                .bind(i64::from(changed))
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let token = members::start_session(conn, member, carried.as_deref()).await?;
                let view = members::session_view(conn, member).await?;
                Ok((token, view))
            }
            .boxed()
        })
        .await?;
    Ok(members::with_cookie(StatusCode::OK, &token, view))
}
