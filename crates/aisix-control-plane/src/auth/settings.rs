//! 控制面认证设置。公开选项只读取启用状态，凭据使用不同的加密槽位。

use aisix_core::config::SsrfProtectionConfig;
use aisix_core::redact_userinfo;
use aisix_gateway::ssrf::SsrfProtection;
use axum::extract::State;
use axum::http::{header, HeaderName};
use axum::Json;
use futures::FutureExt;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;

use super::{password::normalize_email, OperatorAuth, OperatorRole};
use crate::audit::{self, Actor, AuditEntry};
use crate::crypto::MasterKey;
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::{settings, AppState};

const KEY: &str = "authentication";
const LOCK_KEY: i64 = 0x6169_7369_7861_7574;
const EMAIL_KEY: &str = "/email/api_key";
const EMAIL_ENDPOINT: &str = "/email/endpoint";
const TURNSTILE_SECRET: &str = "/turnstile/secret_key";
const GITHUB_SECRET: &str = "/github/client_secret";
const GOOGLE_SECRET: &str = "/google/client_secret";
const MAX_KEY_LENGTH: usize = 4096;

pub(crate) type AuthSettings = Settings<String>;
pub(crate) type OAuthSettings = OAuth<String>;
type AuthSettingsResponse = ([(HeaderName, &'static str); 1], Json<Value>);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretScope {
    Flags,
    Turnstile,
    EmailAndTurnstile,
    Github { turnstile: bool },
    Google { turnstile: bool },
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings<S> {
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_recovery_enabled: Option<bool>,
    pub portal_origin: String,
    pub email: Email<S>,
    pub turnstile: Turnstile<S>,
    pub passkey: Passkey,
    pub github: OAuth<S>,
    pub google: OAuth<S>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Email<S> {
    pub enabled: bool,
    pub endpoint: String,
    pub from: String,
    pub api_key: Option<S>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Turnstile<S> {
    pub enabled: bool,
    pub site_key: String,
    pub secret_key: Option<S>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Passkey {
    pub enabled: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth<S> {
    pub enabled: bool,
    pub client_id: String,
    pub client_secret: Option<S>,
}

#[derive(Deserialize)]
pub struct SubmittedSecret(String);

#[derive(Default, Deserialize, Serialize)]
struct Sealed(String);

impl Sealed {
    fn new(mk: &MasterKey, slot: &str, value: &str) -> Self {
        Self(mk.seal_text(format!("aisix-cp:authentication{slot}").as_bytes(), value))
    }

    fn open(&self, mk: &MasterKey, slot: &str) -> Result<String, ApiError> {
        mk.open_text(format!("aisix-cp:authentication{slot}").as_bytes(), &self.0)
            .map_err(ApiError::internal)
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Stored {
    config: Settings<Sealed>,
    // 旧控制面读取外层时会忽略此字段，内层仍沿用原有严格格式。
    #[serde(default)]
    password_recovery_enabled: bool,
    // 管理 API 返回遮盖后的地址，原始地址加密保存，供真正发信时使用。
    email_endpoint_sealed: Option<Sealed>,
}

impl Stored {
    fn public(&self) -> Value {
        let c = &self.config;
        json!({
            "email_verification": c.email.enabled,
            "password_recovery": self.password_recovery_enabled && c.email.enabled && c.email.api_key.is_some(),
            "turnstile_site_key": c.turnstile.enabled.then_some(c.turnstile.site_key.as_str()),
            "passkey": c.passkey.enabled,
            "github": c.github.enabled,
            "google": c.google.enabled,
        })
    }

    fn admin(&self, state: &AppState) -> Value {
        let c = &self.config;
        json!({
            "version": c.version,
            "password_recovery_enabled": self.password_recovery_enabled,
            "portal_origin": c.portal_origin,
            "email": {
                "enabled": c.email.enabled,
                "endpoint": c.email.endpoint,
                "from": c.email.from,
                "api_key_configured": c.email.api_key.is_some(),
            },
            "turnstile": {
                "enabled": c.turnstile.enabled,
                "site_key": c.turnstile.site_key,
                "secret_key_configured": c.turnstile.secret_key.is_some(),
            },
            "passkey": { "enabled": c.passkey.enabled },
            "github": oauth_view(&c.github, callback_url(state, &c.portal_origin, "github")),
            "google": oauth_view(&c.google, callback_url(state, &c.portal_origin, "google")),
        })
    }

    fn open(self, mk: &MasterKey, scope: SecretScope) -> Result<AuthSettings, ApiError> {
        let Settings {
            version,
            portal_origin,
            email,
            turnstile,
            passkey,
            github,
            google,
            ..
        } = self.config;
        let open = |enabled: bool, sealed: Option<Sealed>, slot: &str| {
            if enabled {
                sealed.map(|value| value.open(mk, slot)).transpose()
            } else {
                Ok(None)
            }
        };
        let email_needed = email.enabled && scope == SecretScope::EmailAndTurnstile;
        let turnstile_needed = turnstile.enabled
            && matches!(
                scope,
                SecretScope::Turnstile
                    | SecretScope::EmailAndTurnstile
                    | SecretScope::Github { turnstile: true }
                    | SecretScope::Google { turnstile: true }
            );
        let email_endpoint = if email_needed {
            open(true, self.email_endpoint_sealed, EMAIL_ENDPOINT)?.unwrap_or(email.endpoint)
        } else {
            email.endpoint
        };
        let oauth = |provider: OAuth<Sealed>, slot: &str, needed: bool| {
            Ok::<_, ApiError>(OAuth {
                enabled: provider.enabled,
                client_id: provider.client_id,
                client_secret: open(provider.enabled && needed, provider.client_secret, slot)?,
            })
        };
        Ok(Settings {
            version,
            password_recovery_enabled: Some(self.password_recovery_enabled),
            portal_origin,
            email: Email {
                enabled: email.enabled,
                endpoint: email_endpoint,
                from: email.from,
                api_key: open(email_needed, email.api_key, EMAIL_KEY)?,
            },
            turnstile: Turnstile {
                enabled: turnstile.enabled,
                site_key: turnstile.site_key,
                secret_key: open(turnstile_needed, turnstile.secret_key, TURNSTILE_SECRET)?,
            },
            passkey,
            github: oauth(
                github,
                GITHUB_SECRET,
                matches!(scope, SecretScope::Github { .. }),
            )?,
            google: oauth(
                google,
                GOOGLE_SECRET,
                matches!(scope, SecretScope::Google { .. }),
            )?,
        })
    }
}

fn oauth_view(provider: &OAuth<Sealed>, callback_url: String) -> Value {
    json!({
        "enabled": provider.enabled,
        "client_id": provider.client_id,
        "client_secret_configured": provider.client_secret.is_some(),
        "callback_url": callback_url,
    })
}

pub(crate) fn callback_url(state: &AppState, origin: &str, name: &str) -> String {
    if origin.is_empty() {
        String::new()
    } else {
        let base = portal_base(state, origin);
        format!("{base}/api/auth/oauth/{name}/callback")
    }
}

pub(crate) fn portal_base(state: &AppState, origin: &str) -> String {
    let prefix = portal_prefix(state.cfg.portal_url.as_deref().unwrap_or(""), origin);
    format!("{origin}{prefix}")
}

// 前缀来自既有部署配置，同一门户源的回调和完成页都沿用该前缀。
fn portal_prefix<'a>(configured: &'a str, origin: &str) -> &'a str {
    let configured = configured.trim();
    let prefix = || {
        let url = Url::parse(configured).ok()?;
        if url.origin().ascii_serialization() != origin
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        // 用原始片段比对，避免 URL 解析器把点段、反斜杠或转义变成另一个路径。
        let (_, authority_and_path) = configured.split_once("://")?;
        let start = authority_and_path.find('/')?;
        let raw_path = &authority_and_path[start..];
        if raw_path != url.path() {
            return None;
        }
        let path = raw_path.strip_suffix('/').unwrap_or(raw_path);
        let segments = path.strip_prefix('/')?;
        segments
            .split('/')
            .all(|segment| {
                !matches!(segment, "" | "." | "..")
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~')
                    })
            })
            .then_some(path)
    };
    prefix().unwrap_or_default()
}

fn decode(state: &AppState, stored: Option<Value>) -> Result<Stored, ApiError> {
    if let Some(stored) = stored {
        return serde_json::from_value(stored)
            .map_err(|_| ApiError::internal("invalid authentication settings"));
    }
    let mut initial = Stored::default();
    // 部署可能已经为邀请链接配置了门户地址，只取它的浏览器源。
    if let Some(url) = state
        .cfg
        .portal_url
        .as_deref()
        .and_then(|url| Url::parse(url).ok())
    {
        let origin = url.origin().ascii_serialization();
        initial.config.portal_origin = normalized_origin(&origin).unwrap_or_default();
    }
    Ok(initial)
}

async fn stored(state: &AppState) -> Result<Stored, ApiError> {
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    decode(state, settings::get(&mut conn, KEY).await?)
}

pub(crate) async fn load(state: &AppState, scope: SecretScope) -> Result<AuthSettings, ApiError> {
    stored(state).await?.open(&state.cfg.master_key, scope)
}

/// 认证完成时再次检查设置，与可能同时提交的禁用或密钥轮换有明确顺序。
pub(crate) async fn load_locked(
    state: &AppState,
    conn: &mut PgConnection,
    scope: SecretScope,
) -> Result<AuthSettings, ApiError> {
    // 先锁写入边界，首次启用时不存在可加行锁的设置。后续语句等待后才取快照。
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let value = sqlx::query_scalar("SELECT value FROM cp_settings WHERE key = $1 FOR SHARE")
        .bind(KEY)
        .fetch_optional(conn)
        .await
        .map_err(DbError::from)?;
    decode(state, value)?.open(&state.cfg.master_key, scope)
}

pub async fn get(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<AuthSettingsResponse, ApiError> {
    auth.require(OperatorRole::Admin)?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(stored(&state).await?.admin(&state)),
    ))
}

pub async fn options(State(state): State<AppState>) -> Result<AuthSettingsResponse, ApiError> {
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(stored(&state).await?.public()),
    ))
}

pub async fn put(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(form): ApiJson<Settings<SubmittedSecret>>,
) -> Result<AuthSettingsResponse, ApiError> {
    auth.require(OperatorRole::Admin)?;
    // 保存前只校验 URL 字面和地址字面量，不在配置事务中发起 DNS 或 HTTP。
    let endpoint = form.email.endpoint.trim();
    let safe_endpoint = endpoint.is_empty() || safe_mail_endpoint(endpoint).await;
    let db = state.db.clone();
    let view = db
        .tx(move |conn| {
            async move {
                // 第一笔写入时设置行还不存在，需要先锁住写入边界。
                sqlx::query("SELECT pg_advisory_xact_lock($1)")
                    .bind(LOCK_KEY)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                let previous =
                    sqlx::query_scalar("SELECT value FROM cp_settings WHERE key = $1 FOR UPDATE")
                        .bind(KEY)
                        .fetch_optional(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                let previous = decode(&state, previous)?;
                if form.version != previous.config.version {
                    return Err(ApiError::version_mismatch(previous.config.version));
                }
                let next = validate(form, previous, &state.cfg.master_key, safe_endpoint)?;
                let value = serde_json::to_value(&next).map_err(ApiError::internal)?;
                settings::set(conn, KEY, &value).await?;
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "settings.authentication",
                        target_kind: "settings",
                        target_id: Some(KEY.to_string()),
                        tenant_id: None,
                        detail: json!({
                            "email": next.config.email.enabled,
                            "password_recovery_enabled": next.password_recovery_enabled,
                            "turnstile": next.config.turnstile.enabled,
                            "passkey": next.config.passkey.enabled,
                            "github": next.config.github.enabled,
                            "google": next.config.google.enabled,
                        }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(next.admin(&state))
            }
            .boxed()
        })
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(view)))
}

fn problem(problems: &mut Vec<Detail>, path: &str, message: &str) {
    problems.push(Detail {
        path: path.to_string(),
        message: message.to_string(),
    });
}

fn normalized_origin(origin: &str) -> Option<String> {
    let (_, authority_and_path) = origin.split_once("://")?;
    let authority = authority_and_path
        .strip_suffix('/')
        .unwrap_or(authority_and_path);
    if authority.contains(['/', '\\', '?', '#', '@']) || origin.len() > 512 {
        return None;
    }
    let url = Url::parse(origin).ok()?;
    let domain = url.domain()?;
    if !matches!(url.scheme(), "https" | "http")
        || (url.scheme() == "http" && domain != "localhost")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || domain.ends_with('.')
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

async fn safe_mail_endpoint(endpoint: &str) -> bool {
    let Ok(url) = Url::parse(endpoint) else {
        return false;
    };
    if endpoint.len() > 4096
        || endpoint.chars().any(char::is_control)
        || url.scheme() != "https"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    if url.domain().is_some_and(|domain| {
        let name = domain.trim_end_matches('.');
        !name.contains('.') || name.ends_with(".local") || name.ends_with(".localhost")
    }) {
        return false;
    }
    let Ok(guard) = SsrfProtection::from_config(&SsrfProtectionConfig::default()) else {
        return false;
    };
    guard.without_proxies().route(&url).await.is_ok()
}

fn valid_credential(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_KEY_LENGTH
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn merge_secret(
    problems: &mut Vec<Detail>,
    path: &str,
    submitted: Option<SubmittedSecret>,
    previous: Option<Sealed>,
    target_changed: bool,
    required: bool,
    mk: &MasterKey,
) -> Option<Sealed> {
    match submitted {
        Some(value) if valid_credential(value.0.trim()) => {
            Some(Sealed::new(mk, path, value.0.trim()))
        }
        Some(value) if !value.0.trim().is_empty() => {
            problem(problems, path, "密钥格式不正确");
            None
        }
        None if previous.is_some() && target_changed => {
            problem(
                problems,
                path,
                "配置目标变更后，请重新填写密钥，或关闭功能并清除旧密钥",
            );
            None
        }
        None if previous.is_some() => previous,
        _ => {
            if required {
                problem(problems, path, "启用时必须填写密钥");
            }
            None
        }
    }
}

fn validate_oauth(
    problems: &mut Vec<Detail>,
    provider: OAuth<SubmittedSecret>,
    previous: OAuth<Sealed>,
    id_path: &str,
    secret_path: &str,
    mk: &MasterKey,
) -> OAuth<Sealed> {
    let client_id = provider.client_id.trim().to_owned();
    if (provider.enabled || !client_id.is_empty()) && !valid_credential(&client_id) {
        problem(problems, id_path, "请填写有效的 Client ID");
    }
    let client_secret = merge_secret(
        problems,
        secret_path,
        provider.client_secret,
        previous.client_secret,
        client_id != previous.client_id,
        provider.enabled,
        mk,
    );
    OAuth {
        enabled: provider.enabled,
        client_id,
        client_secret,
    }
}

fn validate(
    form: Settings<SubmittedSecret>,
    previous: Stored,
    mk: &MasterKey,
    safe_endpoint: bool,
) -> Result<Stored, ApiError> {
    let mut problems = Vec::new();
    let origin_required = form.turnstile.enabled
        || form.passkey.enabled
        || form.github.enabled
        || form.google.enabled;
    let raw_origin = form.portal_origin.trim();
    let portal_origin = if raw_origin.is_empty() && !origin_required {
        String::new()
    } else if let Some(origin) = normalized_origin(raw_origin) {
        origin
    } else {
        problem(
            &mut problems,
            "/portal_origin",
            "请填写门户的 HTTPS 源地址，不带路径、查询参数或片段",
        );
        String::new()
    };

    let raw_endpoint = form.email.endpoint.trim();
    let unchanged_endpoint = raw_endpoint == previous.config.email.endpoint;
    let (endpoint, email_endpoint_sealed) = if unchanged_endpoint {
        (
            previous.config.email.endpoint,
            previous.email_endpoint_sealed,
        )
    } else {
        let display = redact_userinfo(raw_endpoint);
        let sealed =
            (display != raw_endpoint).then(|| Sealed::new(mk, EMAIL_ENDPOINT, raw_endpoint));
        (display.into_owned(), sealed)
    };
    if (form.email.enabled && endpoint.is_empty()) || (!safe_endpoint && !unchanged_endpoint) {
        problem(
            &mut problems,
            EMAIL_ENDPOINT,
            "请填写可公开访问的 HTTPS 发信接口地址，不带查询参数或片段",
        );
    }
    let from = form.email.from.trim().to_owned();
    if (form.email.enabled || !from.is_empty())
        && (normalize_email(&from).is_none()
            || from.chars().any(|c| c.is_whitespace() || c.is_control()))
    {
        problem(&mut problems, "/email/from", "请填写有效的发件邮箱地址");
    }
    let api_key = merge_secret(
        &mut problems,
        EMAIL_KEY,
        form.email.api_key,
        previous.config.email.api_key,
        !unchanged_endpoint,
        form.email.enabled,
        mk,
    );
    let site_key = form.turnstile.site_key.trim().to_owned();
    if (form.turnstile.enabled || !site_key.is_empty()) && !valid_credential(&site_key) {
        problem(
            &mut problems,
            "/turnstile/site_key",
            "请填写有效的 Turnstile Site Key",
        );
    }
    let secret_key = merge_secret(
        &mut problems,
        TURNSTILE_SECRET,
        form.turnstile.secret_key,
        previous.config.turnstile.secret_key,
        site_key != previous.config.turnstile.site_key,
        form.turnstile.enabled,
        mk,
    );
    if form.turnstile.enabled
        && Url::parse(&portal_origin).is_ok_and(|origin| origin.domain() != Some("localhost"))
    {
        if site_key.get(1..22) == Some("x00000000000000000000") {
            problem(
                &mut problems,
                "/turnstile/site_key",
                "Turnstile 测试密钥只能用于 localhost",
            );
        }
        if let Some(secret) = &secret_key {
            let secret = secret.open(mk, TURNSTILE_SECRET)?;
            if secret.get(1..22) == Some("x00000000000000000000") {
                problem(
                    &mut problems,
                    TURNSTILE_SECRET,
                    "Turnstile 测试密钥只能用于 localhost",
                );
            }
        }
    }
    let github = validate_oauth(
        &mut problems,
        form.github,
        previous.config.github,
        "/github/client_id",
        GITHUB_SECRET,
        mk,
    );
    let google = validate_oauth(
        &mut problems,
        form.google,
        previous.config.google,
        "/google/client_id",
        GOOGLE_SECRET,
        mk,
    );
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    Ok(Stored {
        config: Settings {
            version: form
                .version
                .checked_add(1)
                .ok_or_else(|| ApiError::internal("authentication settings version overflow"))?,
            password_recovery_enabled: None,
            portal_origin,
            email: Email {
                enabled: form.email.enabled,
                endpoint,
                from,
                api_key,
            },
            turnstile: Turnstile {
                enabled: form.turnstile.enabled,
                site_key,
                secret_key,
            },
            passkey: form.passkey,
            github,
            google,
        },
        password_recovery_enabled: form
            .password_recovery_enabled
            .unwrap_or(previous.password_recovery_enabled),
        email_endpoint_sealed,
    })
}
