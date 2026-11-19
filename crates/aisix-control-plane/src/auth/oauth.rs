//! 租户门户的 OAuth 授权码登录。绑定由已登录成员确认密码后发起。
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::FutureExt;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use rand_core::RngCore;
use reqwest::Url;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::session::{self, MEMBER_SESSION_TTL, PORTAL_COOKIE};
use super::settings::{self, AuthSettings, OAuthSettings, SecretScope};
use super::verification::verify_turnstile;
use super::MemberAuth;
use crate::audit::{self, Actor, AuditEntry};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::{ApiJson, EVERY_MEMBER};
use crate::{members, AppState};

const FLOW_TTL_SECS: u64 = 600;
const FLOW_LIMIT: i64 = 4_000;
const FLOW_LOCK_KEY: i64 = 0x6169_7369_786f_6175;
const RESPONSE_LIMIT: usize = 64 * 1024;

static GOOGLE_KEYS: Mutex<Option<(Instant, JwkSet)>> = Mutex::const_new(None);

#[derive(Clone, Copy)]
enum Provider {
    Github,
    Google,
}

impl Provider {
    fn parse(raw: &str) -> Result<Self, ApiError> {
        match raw {
            "github" => Ok(Self::Github),
            "google" => Ok(Self::Google),
            _ => Err(ApiError::not_found()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Google => "google",
        }
    }

    fn settings(self, cfg: &AuthSettings) -> &OAuthSettings {
        match self {
            Self::Github => &cfg.github,
            Self::Google => &cfg.google,
        }
    }

    fn secret_scope(self) -> SecretScope {
        match self {
            Self::Github => SecretScope::Github,
            Self::Google => SecretScope::Google,
        }
    }

    fn cookie_name(self, origin: &str) -> String {
        let prefix = if origin.starts_with("https://") {
            "__Host-"
        } else {
            ""
        };
        format!("{prefix}aisix_oauth_{}", self.name())
    }

    fn cookie(self, origin: &str, token: &str, seconds: u64) -> String {
        let name = self.cookie_name(origin);
        let secure = if origin.starts_with("https://") {
            "; Secure"
        } else {
            ""
        };
        format!("{name}={token}; HttpOnly{secure}; SameSite=Lax; Path=/; Max-Age={seconds}")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Login,
    Link,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    mode: Mode,
    password: Option<String>,
    turnstile_token: Option<String>,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnlinkRequest {
    password: String,
}

#[derive(sqlx::FromRow)]
struct Flow {
    verifier_sealed: String,
    client_id: String,
    redirect_uri: String,
    session_sha256: Option<Vec<u8>>,
    link_member_id: Option<Uuid>,
    reauth_sha256: Option<Vec<u8>>,
}

fn token() -> String {
    let mut bytes = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn valid_token(text: &str) -> bool {
    text.len() == 43
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn digest(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

fn nonce(state: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&format!("oidc-nonce:{state}")))
}

fn verifier_aad(state: &AppState, flow_token: &str) -> String {
    format!("portal-oauth:{}:{flow_token}", state.env.env_id)
}

// 供应商协议固定。GitHub 授权与 PKCE：
// https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps
// Google OIDC 授权、令牌及签名密钥：
// https://developers.google.com/identity/openid-connect/openid-connect
fn authorization_url(
    provider: Provider,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    verifier: &str,
) -> Result<Url, ApiError> {
    let mut url = Url::parse(match provider {
        Provider::Github => "https://github.com/login/oauth/authorize",
        Provider::Google => "https://accounts.google.com/o/oauth2/v2/auth",
    })
    .map_err(ApiError::internal)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut query = url.query_pairs_mut();
    query
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    match provider {
        Provider::Github => {
            query
                .append_pair("scope", "")
                .append_pair("prompt", "select_account");
        }
        Provider::Google => {
            query
                .append_pair("scope", "openid")
                .append_pair("response_type", "code")
                .append_pair("nonce", &nonce(state))
                .append_pair("prompt", "select_account");
        }
    }
    drop(query);
    Ok(url)
}

pub async fn start(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<StartRequest>,
) -> Result<Response, ApiError> {
    let provider = Provider::parse(&raw)?;
    let login = matches!(req.mode, Mode::Login);
    let scope = if login {
        SecretScope::Turnstile
    } else {
        SecretScope::Flags
    };
    let cfg = settings::load(&state, scope).await?;
    let config = provider.settings(&cfg);
    if !config.enabled {
        return Err(ApiError::not_found());
    }
    // 同源浏览器请求携带 Origin；命令行请求可以缺省。
    if headers
        .get(header::ORIGIN)
        .is_some_and(|origin| origin.as_bytes() != cfg.portal_origin.as_bytes())
    {
        return Err(ApiError::forbidden());
    }
    let carried = members::carried_token(&headers).map(|t| digest(&t));
    let (link_member, reauth) = match req.mode {
        Mode::Login => {
            verify_turnstile(&state, &cfg, req.turnstile_token.as_deref(), "oauth_login").await?;
            (None, None)
        }
        Mode::Link => {
            let auth = MemberAuth::from_session(&headers, &state)
                .await?
                .ok_or_else(ApiError::unauthenticated)?;
            auth.require(EVERY_MEMBER)?;
            let password = req
                .password
                .as_deref()
                .ok_or_else(ApiError::invalid_credentials)?;
            let hash = members::verify_member_password(&state, auth.id, password).await?;
            (Some(auth.id), Some(digest(&hash)))
        }
    };
    let flow_token = token();
    let browser = token();
    let verifier = token();
    let redirect_uri = settings::callback_url(&state, &cfg.portal_origin, provider.name());
    let url = authorization_url(
        provider,
        &config.client_id,
        &redirect_uri,
        &flow_token,
        &verifier,
    )?;
    let verifier_sealed = state
        .cfg
        .master_key
        .seal_text(verifier_aad(&state, &flow_token).as_bytes(), &verifier);
    let flow_sha = digest(&flow_token);
    let browser_sha = digest(&browser);
    let client_id = config.client_id.clone();
    let turnstile_snapshot = login.then(|| {
        (
            cfg.turnstile.enabled,
            cfg.turnstile.site_key.clone(),
            cfg.turnstile.secret_key.clone(),
        )
    });
    let app = state.clone();

    // 登录可以在 Turnstile 停用时公开发起，限制短期状态表的占用。
    state.db.tx(move |conn| {
        async move {
            let current = settings::load_locked(&app, conn, scope).await?;
            let provider_config = provider.settings(&current);
            if !provider_config.enabled
                || provider_config.client_id != client_id
                || settings::callback_url(&app, &current.portal_origin, provider.name()) != redirect_uri
            {
                return Err(ApiError::not_found());
            }
            if turnstile_snapshot.is_some_and(|(enabled, site_key, secret)| {
                enabled != current.turnstile.enabled
                    || site_key != current.turnstile.site_key
                    || secret != current.turnstile.secret_key
            }) {
                return Err(ApiError::conflict("auth_settings_changed", "验证设置已更新，请刷新后重试"));
            }
            let reserved: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(FLOW_LOCK_KEY)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
            if !reserved {
                return Err(ApiError::busy());
            }
            let active: i64 = sqlx::query_scalar("SELECT count(*) FROM portal_oauth_flows WHERE expires_at > now()")
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
            if active >= FLOW_LIMIT {
                return Err(ApiError::busy());
            }
            sqlx::query(
                "INSERT INTO portal_oauth_flows \
                 (state_sha256, browser_sha256, provider, verifier_sealed, client_id, redirect_uri, \
                  expires_at, session_sha256, link_member_id, reauth_sha256) \
                 VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7), $8, $9, $10)",
            )
            .bind(flow_sha)
            .bind(browser_sha)
            .bind(provider.name())
            .bind(verifier_sealed)
            .bind(client_id)
            .bind(redirect_uri)
            .bind(FLOW_TTL_SECS as f64)
            .bind(carried)
            .bind(link_member)
            .bind(reauth)
            .execute(conn)
            .await
            .map_err(DbError::from)?;
            Ok::<_, ApiError>(())
        }
        .boxed()
    }).await?;

    Ok((
        [
            (
                header::SET_COOKIE,
                provider.cookie(&cfg.portal_origin, &browser, FLOW_TTL_SECS),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(json!({"url": url.as_str()})),
    )
        .into_response())
}

async fn consume(
    state: &AppState,
    provider: Provider,
    flow_token: &str,
    browser: &str,
) -> Result<Option<Flow>, ApiError> {
    // 同一个浏览器的授权回调也只允许一次，不因兑换令牌失败而恢复。
    Ok(sqlx::query_as(
        "DELETE FROM portal_oauth_flows \
         WHERE state_sha256 = $1 AND browser_sha256 = $2 AND provider = $3 AND expires_at > now() \
         RETURNING verifier_sealed, client_id, redirect_uri, session_sha256, link_member_id, reauth_sha256",
    )
    .bind(digest(flow_token))
    .bind(digest(browser))
    .bind(provider.name())
    .fetch_optional(state.db.pool())
    .await
    .map_err(DbError::from)?)
}

fn exchange_error() -> ApiError {
    ApiError::upstream_bad_response("第三方身份验证失败")
}

async fn public_request(
    state: &AppState,
    method: reqwest::Method,
    endpoint: &str,
) -> Result<reqwest::RequestBuilder, ApiError> {
    let url = Url::parse(endpoint).map_err(ApiError::internal)?;
    let client = state.upstream.public_https_client(&url).await?;
    Ok(client.request(method, url))
}

async fn read_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, ApiError> {
    if !response.status().is_success() {
        return Err(exchange_error());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| exchange_error())? {
        if body.len() + chunk.len() > RESPONSE_LIMIT {
            return Err(exchange_error());
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| exchange_error())
}

async fn google_decoding_key(state: &AppState, kid: &str) -> Result<DecodingKey, ApiError> {
    let mut keys = GOOGLE_KEYS.lock().await;
    if let Some((expires, cache)) = keys.as_ref() {
        if *expires > Instant::now() {
            if let Some(key) = cache.find(kid) {
                return DecodingKey::from_jwk(key).map_err(|_| exchange_error());
            }
        }
    }
    let response = public_request(
        state,
        reqwest::Method::GET,
        "https://www.googleapis.com/oauth2/v3/certs",
    )
    .await?
    .send()
    .await
    .map_err(|_| exchange_error())?;
    let max_age = response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .split(',')
                .map(str::trim)
                .find_map(|part| part.strip_prefix("max-age=")?.parse::<u64>().ok())
        })
        .unwrap_or_default()
        .min(86_400);
    let cache: JwkSet = read_json(response).await?;
    let key = cache
        .find(kid)
        .and_then(|jwk| DecodingKey::from_jwk(jwk).ok());
    *keys = Some((Instant::now() + Duration::from_secs(max_age), cache));
    key.ok_or_else(exchange_error)
}

#[derive(Deserialize)]
struct GoogleClaims {
    sub: String,
    nonce: String,
    aud: String,
    azp: Option<String>,
}

async fn google_subject(
    state: &AppState,
    id_token: &str,
    client_id: &str,
    flow_token: &str,
) -> Result<String, ApiError> {
    let header = decode_header(id_token).map_err(|_| exchange_error())?;
    if header.alg != Algorithm::RS256 {
        return Err(exchange_error());
    }
    let key = google_decoding_key(state, header.kid.as_deref().ok_or_else(exchange_error)?).await?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[client_id]);
    validation.set_issuer(&["https://accounts.google.com", "accounts.google.com"]);
    validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
    validation.leeway = 0;
    let claims = decode::<GoogleClaims>(id_token, &key, &validation)
        .map_err(|_| exchange_error())?
        .claims;
    if claims.aud != client_id
        || claims.azp.as_deref().is_some_and(|azp| azp != client_id)
        || claims.nonce != nonce(flow_token)
        || claims.sub.is_empty()
        || claims.sub.len() > 255
        || !claims.sub.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(exchange_error());
    }
    Ok(claims.sub)
}

async fn provider_subject(
    state: &AppState,
    provider: Provider,
    flow: &Flow,
    code: &str,
    secret: &str,
    verifier: &str,
    flow_token: &str,
) -> Result<String, ApiError> {
    let exchange = [
        ("client_id", flow.client_id.as_str()),
        ("client_secret", secret),
        ("code", code),
        ("redirect_uri", flow.redirect_uri.as_str()),
        ("code_verifier", verifier),
    ];
    match provider {
        Provider::Github => {
            #[derive(Deserialize)]
            struct TokenAnswer {
                access_token: String,
            }
            #[derive(Deserialize)]
            struct UserAnswer {
                id: u64,
            }
            let token: TokenAnswer = read_json(
                public_request(
                    state,
                    reqwest::Method::POST,
                    "https://github.com/login/oauth/access_token",
                )
                .await?
                .header(header::ACCEPT, "application/json")
                .form(&exchange)
                .send()
                .await
                .map_err(|_| exchange_error())?,
            )
            .await?;
            // 只读取授权账号稳定 ID，不申请邮箱或私有账号信息。
            let user: UserAnswer = read_json(
                public_request(state, reqwest::Method::GET, "https://api.github.com/user")
                    .await?
                    .header(header::ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2026-03-10")
                    .bearer_auth(token.access_token)
                    .send()
                    .await
                    .map_err(|_| exchange_error())?,
            )
            .await?;
            Ok(user.id.to_string())
        }
        Provider::Google => {
            #[derive(Deserialize)]
            struct TokenAnswer {
                id_token: String,
            }
            let mut fields = exchange.to_vec();
            fields.push(("grant_type", "authorization_code"));
            let token: TokenAnswer = read_json(
                public_request(
                    state,
                    reqwest::Method::POST,
                    "https://oauth2.googleapis.com/token",
                )
                .await?
                .form(&fields)
                .send()
                .await
                .map_err(|_| exchange_error())?,
            )
            .await?;
            google_subject(state, &token.id_token, &flow.client_id, flow_token).await
        }
    }
}

async fn bind_or_login(
    state: &AppState,
    provider: Provider,
    cfg: &AuthSettings,
    flow: Flow,
    subject: String,
) -> Result<Option<String>, ApiError> {
    let state = state.clone();
    let previous_secret = provider.settings(cfg).client_secret.clone();
    state.db.clone().tx(move |conn| {
        async move {
            // 在网络兑换之后再次读取。后台关闭提供方和会话签发都在事务中线性化。
            let current = settings::load_locked(&state, conn, provider.secret_scope()).await?;
            let settings = provider.settings(&current);
            if !settings.enabled
                || settings.client_id != flow.client_id
                || settings.client_secret != previous_secret
                || settings::callback_url(&state, &current.portal_origin, provider.name()) != flow.redirect_uri
            {
                return Err(ApiError::not_found());
            }

            if let Some(id) = flow.link_member_id {
                let (hash, email, tenant_id) = live_member(conn, id).await?;
                if flow.reauth_sha256.as_deref() != Some(digest(&hash).as_slice()) {
                    return Err(ApiError::invalid_credentials());
                }
                let session = flow.session_sha256.as_deref().ok_or_else(ApiError::unauthenticated)?;
                session::require_live_member(conn, id, session).await?;
                let inserted = sqlx::query(
                    "INSERT INTO member_oauth_identities (member_id, provider, subject) \
                     VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                )
                .bind(id).bind(provider.name()).bind(&subject)
                .execute(&mut *conn).await.map_err(DbError::from)?.rows_affected();
                if inserted == 0 {
                    require_identity(conn, id, provider, &subject).await?;
                } else {
                    record(conn, id, &email, tenant_id, "member.oauth.link", provider).await?;
                }
                return Ok(None);
            }

            let id: Uuid = sqlx::query_scalar(
                "SELECT member_id FROM member_oauth_identities WHERE provider = $1 AND subject = $2",
            )
            .bind(provider.name()).bind(&subject)
            .fetch_optional(&mut *conn).await.map_err(DbError::from)?
            .ok_or_else(ApiError::invalid_credentials)?;
            // 成员禁用、移除，以及解绑都可能在兑换令牌期间发生；锁成员后再检查绑定。
            live_member(conn, id).await?;
            if let Some(old_session) = flow.session_sha256 {
                sqlx::query("DELETE FROM sessions WHERE token_sha256 = $1 AND principal_kind = 'member'")
                    .bind(old_session).execute(&mut *conn).await.map_err(DbError::from)?;
            }
            require_identity(conn, id, provider, &subject).await?;
            let session = members::start_session(conn, id, None).await?;
            Ok(Some(session))
        }
        .boxed()
    }).await
}

async fn live_member(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<(String, String, Uuid), ApiError> {
    sqlx::query_as(
        "SELECT password_hash, email, tenant_id FROM members \
         WHERE id = $1 AND NOT disabled AND removed_at IS NULL FOR SHARE",
    )
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?
    .ok_or_else(ApiError::invalid_credentials)
}

async fn require_identity(
    conn: &mut PgConnection,
    id: Uuid,
    provider: Provider,
    subject: &str,
) -> Result<(), ApiError> {
    let identity: Option<bool> = sqlx::query_scalar(
        "SELECT true FROM member_oauth_identities \
         WHERE member_id = $1 AND provider = $2 AND subject = $3 FOR SHARE",
    )
    .bind(id)
    .bind(provider.name())
    .bind(subject)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?;
    identity
        .map(|_| ())
        .ok_or_else(ApiError::invalid_credentials)
}

async fn record(
    conn: &mut PgConnection,
    id: Uuid,
    email: &str,
    tenant_id: Uuid,
    action: &'static str,
    provider: Provider,
) -> Result<(), ApiError> {
    Ok(audit::record(
        conn,
        AuditEntry {
            actor: Actor::Member { id, email },
            action,
            target_kind: "member",
            target_id: Some(id.to_string()),
            tenant_id: Some(tenant_id),
            detail: json!({"provider": provider.name()}),
        },
    )
    .await?)
}

fn landing(prefix: &str, status: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, format!("{prefix}/?auth={status}")),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
        ],
    )
        .into_response()
}

pub async fn callback(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let provider = Provider::parse(&raw)?;
    let cfg = settings::load(&state, provider.secret_scope()).await?;
    let config = provider.settings(&cfg);
    let base = settings::portal_base(&state, &cfg.portal_origin);
    let prefix = base
        .strip_prefix(&cfg.portal_origin)
        .filter(|path| path.starts_with('/') && !path.starts_with("//"))
        .unwrap_or_default();
    let refused = || landing(prefix, "error");
    let Ok(Query(query)) = query else {
        return Ok(refused());
    };
    let (Some(flow_token), Some(browser)) = (
        query.state.as_deref().filter(|s| valid_token(s)),
        session::token_from_headers(&headers, &provider.cookie_name(&cfg.portal_origin))
            .filter(|s| valid_token(s)),
    ) else {
        return Ok(refused());
    };
    let Some(flow) = consume(&state, provider, flow_token, browser).await? else {
        return Ok(refused());
    };
    let result = async {
        let code = query
            .code
            .as_deref()
            .filter(|code| !code.is_empty() && code.len() <= 4096)
            .ok_or_else(ApiError::invalid_credentials)?;
        if query.error.is_some()
            || !config.enabled
            || flow.redirect_uri
                != settings::callback_url(&state, &cfg.portal_origin, provider.name())
            || flow.client_id != config.client_id
        {
            return Err(ApiError::invalid_credentials());
        }
        let secret = config
            .client_secret
            .as_deref()
            .ok_or_else(ApiError::invalid_credentials)?;
        let verifier = state
            .cfg
            .master_key
            .open_text(
                verifier_aad(&state, flow_token).as_bytes(),
                &flow.verifier_sealed,
            )
            .map_err(ApiError::internal)?;
        let subject =
            provider_subject(&state, provider, &flow, code, secret, &verifier, flow_token).await?;
        bind_or_login(&state, provider, &cfg, flow, subject).await
    }
    .await;
    let mut response = match result {
        Ok(Some(token)) => {
            let mut response = landing(prefix, "success");
            let cookie = session::cookie(PORTAL_COOKIE, &token, MEMBER_SESSION_TTL);
            response.headers_mut().append(
                header::SET_COOKIE,
                HeaderValue::from_str(&cookie).map_err(ApiError::internal)?,
            );
            response
        }
        Ok(None) => landing(prefix, "linked"),
        Err(error) => {
            // 日志仅记内部分类，避免记录回调参数、令牌或上游错误正文。
            tracing::debug!(
                provider = provider.name(),
                code = error.code,
                "portal OAuth callback refused"
            );
            refused()
        }
    };
    let cleared = provider.cookie(&cfg.portal_origin, "", 0);
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cleared).map_err(ApiError::internal)?,
    );
    Ok(response)
}

pub async fn list(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<Value>), ApiError> {
    auth.require(EVERY_MEMBER)?;
    let links: Vec<String> =
        sqlx::query_scalar("SELECT provider FROM member_oauth_identities WHERE member_id = $1")
            .bind(auth.id)
            .fetch_all(state.db.pool())
            .await
            .map_err(DbError::from)?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "github": links.iter().any(|p| p == "github"),
            "google": links.iter().any(|p| p == "google"),
        })),
    ))
}

pub async fn unlink(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<UnlinkRequest>,
) -> Result<StatusCode, ApiError> {
    auth.require(EVERY_MEMBER)?;
    let provider = Provider::parse(&raw)?;
    let token = members::carried_token(&headers).ok_or_else(ApiError::unauthenticated)?;
    if req.password.is_empty() {
        return Err(ApiError::validation(vec![Detail {
            path: "/password".to_string(),
            message: "请输入当前密码".to_string(),
        }]));
    }
    let verified = members::verify_member_password(&state, auth.id, &req.password).await?;
    state
        .db
        .tx(move |conn| {
            async move {
                let (hash, email, tenant_id) = live_member(conn, auth.id).await?;
                if hash != verified {
                    return Err(ApiError::invalid_credentials());
                }
                session::require_live_member(conn, auth.id, &digest(&token)).await?;
                let deleted = sqlx::query(
                    "DELETE FROM member_oauth_identities WHERE member_id = $1 AND provider = $2",
                )
                .bind(auth.id)
                .bind(provider.name())
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?
                .rows_affected();
                if deleted > 0 {
                    record(
                        conn,
                        auth.id,
                        &email,
                        tenant_id,
                        "member.oauth.unlink",
                        provider,
                    )
                    .await?;
                }
                Ok::<_, ApiError>(StatusCode::NO_CONTENT)
            }
            .boxed()
        })
        .await
}
