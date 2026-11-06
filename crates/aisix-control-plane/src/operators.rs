//! Operator accounts: login, logout, the session, password changes, and the
//! admin's operator management.
//!
//! The first operator is created on the host with
//! `aisix-control-plane create-operator` ([`create_first`]); there is no
//! default password.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::password::{self, normalize_email, LOGIN_COOLDOWN, MIN_PASSWORD_LEN};
use crate::auth::session::{self, Principal, ADMIN_COOKIE, OPERATOR_SESSION_TTL};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::{Db, DbError};
use crate::error::{ApiError, Detail};
use crate::http::{ApiJson, API_CONTRACT};
use crate::AppState;

/// Serialises the operator-wide invariants (at least one enabled admin), so
/// two concurrent demotions cannot each count the other as the remaining
/// admin. It touches no tenant row.
pub const OPERATORS_LOCK_KEY: i64 = 0x6169_7369_7863_7030;

/// The longest display name accepted.
const MAX_DISPLAY_NAME_CHARS: usize = 128;

const TARGET_KIND: &str = "operator";

/// `Operator` in the API: `{id, email, display_name, role}`.
#[derive(Debug, Clone, Serialize)]
pub struct OperatorView {
    pub id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub role: OperatorRole,
}

/// A row of `operators`, without the password hash.
struct OperatorRow {
    id: Uuid,
    email: String,
    display_name: Option<String>,
    role: OperatorRole,
    disabled: bool,
    created_at: DateTime<Utc>,
}

type RawOperatorRow = (Uuid, String, Option<String>, String, bool, DateTime<Utc>);

const OPERATOR_COLUMNS: &str = "id, email, display_name, role, disabled, created_at";

impl OperatorRow {
    fn from_raw(raw: RawOperatorRow) -> Result<OperatorRow, ApiError> {
        let (id, email, display_name, role, disabled, created_at) = raw;
        Ok(OperatorRow {
            id,
            email,
            display_name,
            role: role.parse().map_err(ApiError::internal)?,
            disabled,
            created_at,
        })
    }

    fn view(&self) -> OperatorView {
        OperatorView {
            id: self.id,
            email: self.email.clone(),
            display_name: self.display_name.clone(),
            role: self.role,
        }
    }
}

fn detail(path: &str, message: impl Into<String>) -> Detail {
    Detail {
        path: path.to_string(),
        message: message.into(),
    }
}

fn password_too_short(path: &str) -> Detail {
    detail(path, format!("密码至少需要 {MIN_PASSWORD_LEN} 个字符"))
}

/// The display name to store: trimmed, `None` when empty, or a detail at
/// `/display_name` when too long.
fn display_name(raw: Option<&str>) -> Result<Option<String>, Detail> {
    let Some(name) = raw.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(detail(
            "/display_name",
            format!("显示名称不能超过 {MAX_DISPLAY_NAME_CHARS} 个字符"),
        ));
    }
    Ok(Some(name.to_string()))
}

fn is_email_taken(e: &DbError) -> bool {
    matches!(e, DbError::UniqueViolation { constraint } if constraint == "operators_email_key")
}

/// `429 busy` with `Retry-After`, for an email that is cooling down.
fn cooling_down() -> Response {
    (
        [(header::RETRY_AFTER, LOGIN_COOLDOWN.as_secs().to_string())],
        ApiError::busy(),
    )
        .into_response()
}

fn email_taken() -> ApiError {
    ApiError::conflict("email_taken", "该邮箱已被使用")
}

/// An operator to create, already checked.
struct NewOperator {
    email: String,
    display_name: Option<String>,
    role: OperatorRole,
}

/// Inserts `new` with `password_hash`, and its `operator.create` row.
async fn insert(
    conn: &mut PgConnection,
    actor: Actor<'_>,
    new: &NewOperator,
    password_hash: &str,
) -> Result<OperatorView, DbError> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO operators (id, email, password_hash, display_name, role) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(&new.email)
    .bind(password_hash)
    .bind(&new.display_name)
    .bind(new.role.as_str())
    .execute(&mut *conn)
    .await?;
    audit::record(
        conn,
        AuditEntry {
            actor,
            action: "operator.create",
            target_kind: TARGET_KIND,
            target_id: Some(id.to_string()),
            tenant_id: None,
            detail: json!({
                "email": new.email,
                "role": new.role,
                "display_name": new.display_name,
            }),
        },
    )
    .await?;
    Ok(OperatorView {
        id,
        email: new.email.clone(),
        display_name: new.display_name.clone(),
        role: new.role,
    })
}

#[derive(Deserialize)]
pub struct LoginRequest {
    email: String,
    password: String,
}

/// `POST /admin/login`. A wrong password, an unknown email and a disabled
/// account all answer the same `401 invalid_credentials`, after the same
/// work (one argon2 verification and one audit row). A body that cannot be
/// read answers the same 401.
pub async fn login(
    State(state): State<AppState>,
    body: Result<ApiJson<LoginRequest>, ApiError>,
) -> Result<Response, ApiError> {
    let Ok(ApiJson(req)) = body else {
        return Err(ApiError::invalid_credentials());
    };
    // No account has an email of another shape, and refusing it here keeps
    // it out of the cooldown table.
    let Some(email) = normalize_email(&req.email) else {
        return Err(ApiError::invalid_credentials());
    };
    // Before any argon2 work: guessing must not get to spend 19 MiB a try.
    if state.operator_logins.in_cooldown(&email) {
        return Ok(cooling_down());
    }
    let found: Option<(Uuid, String, String, bool)> =
        sqlx::query_as("SELECT id, password_hash, role, disabled FROM operators WHERE email = $1")
            .bind(&email)
            .fetch_optional(state.db.pool())
            .await
            .map_err(DbError::from)?;
    let matched = state
        .passwords
        .verify(found.as_ref().map(|f| f.1.as_str()), &req.password)
        .await?;
    let target = found.as_ref().map(|f| f.0);
    let live = found.filter(|f| matched && !f.3);
    let Some((id, verified_hash, role, _)) = live else {
        state.operator_logins.note_failure(&email);
        state
            .db
            .tx(move |conn| {
                async move {
                    // The row is written either way, so an unknown email
                    // costs what a known one does; only an email that names
                    // an account is stored, not text a caller made up.
                    let actor = match target {
                        Some(_) => Actor::Anonymous { email: &email },
                        None => Actor::Unidentified,
                    };
                    audit::record(
                        conn,
                        AuditEntry {
                            actor,
                            action: "operator.login_failed",
                            target_kind: TARGET_KIND,
                            target_id: target.map(|id| id.to_string()),
                            tenant_id: None,
                            detail: json!({}),
                        },
                    )
                    .await
                    .map_err(ApiError::from)
                }
                .boxed()
            })
            .await?;
        return Err(ApiError::invalid_credentials());
    };
    state.operator_logins.clear(&email);
    let auth = OperatorAuth {
        id,
        email,
        role: role.parse().map_err(ApiError::internal)?,
    };
    let (token, view) = state
        .db
        .tx(move |conn| {
            async move {
                // The account was read and verified outside this transaction,
                // which can take seconds. A password change or a disable
                // that committed meanwhile has already ended the account's
                // sessions, before this one existed. So the row is read
                // again, still with the hash just verified and enabled,
                // under a share lock: those changes update the row before
                // ending its sessions, so either this waits for such a
                // change to commit and then finds no row, or the change
                // waits for this session to commit and then ends it too.
                let unchanged: Option<bool> = sqlx::query_scalar(
                    "SELECT true FROM operators \
                     WHERE id = $1 AND password_hash = $2 AND NOT disabled FOR SHARE",
                )
                .bind(auth.id)
                .bind(&verified_hash)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if unchanged.is_none() {
                    return Err(ApiError::invalid_credentials());
                }
                let token = session::create(
                    &mut *conn,
                    Principal::Operator(auth.id),
                    OPERATOR_SESSION_TTL,
                )
                .await?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "operator.login",
                        target_kind: TARGET_KIND,
                        target_id: Some(auth.id.to_string()),
                        tenant_id: None,
                        detail: json!({}),
                    },
                )
                .await?;
                let view = load(conn, auth.id).await?.view();
                Ok::<_, ApiError>((token, view))
            }
            .boxed()
        })
        .await?;
    let cookie = session::cookie(ADMIN_COOKIE, &token, OPERATOR_SESSION_TTL);
    Ok(([(header::SET_COOKIE, cookie)], Json(view)).into_response())
}

/// `POST /admin/logout`: ends the request's session, if any, and clears the
/// cookie.
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = session::token_from_headers(&headers, ADMIN_COOKIE) {
        session::revoke(&state.db, token).await?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session::cleared_cookie(ADMIN_COOKIE))],
    )
        .into_response())
}

/// `GET /admin/session`: `{authed, api_contract, operator?}`.
pub async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let Some(auth) = OperatorAuth::from_session(&headers, &state).await? else {
        return Ok(Json(
            json!({ "authed": false, "api_contract": API_CONTRACT }),
        ));
    };
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let operator = load(&mut conn, auth.id).await?.view();
    Ok(Json(json!({
        "authed": true,
        "api_contract": API_CONTRACT,
        "operator": operator,
    })))
}

/// The operator `id`, or `404 not_found`.
async fn load(conn: &mut PgConnection, id: Uuid) -> Result<OperatorRow, ApiError> {
    let raw: Option<RawOperatorRow> = sqlx::query_as(&format!(
        "SELECT {OPERATOR_COLUMNS} FROM operators WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?;
    OperatorRow::from_raw(raw.ok_or_else(ApiError::not_found)?)
}

#[derive(Deserialize)]
pub struct ChangePassword {
    current_password: String,
    new_password: String,
}

/// `POST /admin/password`: the caller's own password. Every other session
/// of the caller ends; the one making the change stays.
pub async fn change_password(
    auth: OperatorAuth,
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<ChangePassword>,
) -> Result<Response, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    if !password::long_enough(&req.new_password) {
        return Err(ApiError::validation(vec![password_too_short(
            "/new_password",
        )]));
    }
    // A stolen session must not make guessing the password any easier than
    // the login does: the same cooldown applies.
    if state.operator_logins.in_cooldown(&auth.email) {
        return Ok(cooling_down());
    }
    let stored: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM operators WHERE id = $1")
            .bind(auth.id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(DbError::from)?;
    let matched = state
        .passwords
        .verify(stored.as_deref(), &req.current_password)
        .await?;
    if !matched {
        state.operator_logins.note_failure(&auth.email);
        return Err(ApiError {
            message: "当前密码错误".to_string(),
            ..ApiError::invalid_credentials()
        });
    }
    state.operator_logins.clear(&auth.email);
    let hash = state.passwords.hash(&req.new_password).await?;
    let current = session::token_from_headers(&headers, ADMIN_COOKIE).map(str::to_string);
    state
        .db
        .tx(move |conn| {
            async move {
                sqlx::query("UPDATE operators SET password_hash = $2 WHERE id = $1")
                    .bind(auth.id)
                    .bind(&hash)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                session::revoke_all(&mut *conn, Principal::Operator(auth.id), current.as_deref())
                    .await?;
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "operator.password",
                        target_kind: TARGET_KIND,
                        target_id: Some(auth.id.to_string()),
                        tenant_id: None,
                        detail: json!({}),
                    },
                )
                .await?;
                Ok::<_, ApiError>(())
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /admin/operators`: `{items: [Operator & {disabled, created_at}]}`.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let raw: Vec<RawOperatorRow> = sqlx::query_as(&format!(
        "SELECT {OPERATOR_COLUMNS} FROM operators ORDER BY created_at, email"
    ))
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let mut items = Vec::with_capacity(raw.len());
    for row in raw {
        let row = OperatorRow::from_raw(row)?;
        let mut item = serde_json::to_value(row.view()).map_err(ApiError::internal)?;
        if let Value::Object(fields) = &mut item {
            fields.insert("disabled".to_string(), json!(row.disabled));
            fields.insert("created_at".to_string(), json!(row.created_at));
        }
        items.push(item);
    }
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct CreateOperator {
    email: String,
    display_name: Option<String>,
    role: OperatorRole,
    password: String,
}

/// `POST /admin/operators` → `201 Operator`; `409 email_taken`.
pub async fn create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<CreateOperator>,
) -> Result<(StatusCode, Json<OperatorView>), ApiError> {
    auth.require(OperatorRole::Admin)?;
    let mut problems = Vec::new();
    let email = normalize_email(&req.email);
    if email.is_none() {
        problems.push(detail("/email", "邮箱格式不正确"));
    }
    let name = display_name(req.display_name.as_deref()).unwrap_or_else(|d| {
        problems.push(d);
        None
    });
    if !password::long_enough(&req.password) {
        problems.push(password_too_short("/password"));
    }
    let (Some(email), true) = (email, problems.is_empty()) else {
        return Err(ApiError::validation(problems));
    };
    let new = NewOperator {
        email,
        display_name: name,
        role: req.role,
    };
    let hash = state.passwords.hash(&req.password).await?;
    let view = state
        .db
        .tx(move |conn| {
            async move {
                insert(conn, Actor::Operator(&auth), &new, &hash)
                    .await
                    .map_err(|e| {
                        if is_email_taken(&e) {
                            email_taken()
                        } else {
                            e.into()
                        }
                    })
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(view)))
}

#[derive(Deserialize)]
pub struct UpdateOperator {
    role: Option<OperatorRole>,
    disabled: Option<bool>,
    /// An empty name clears it.
    display_name: Option<String>,
}

/// `PATCH /admin/operators/{id}` → `200 Operator`. The last enabled admin
/// cannot be demoted or disabled (`409 last_admin`); disabling an operator
/// ends their sessions in the same transaction.
pub async fn update(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<UpdateOperator>,
) -> Result<Json<OperatorView>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let name = match req.display_name.as_deref() {
        None => None,
        Some(raw) => Some(display_name(Some(raw)).map_err(|d| ApiError::validation(vec![d]))?),
    };
    let view = state
        .db
        .tx(move |conn| {
            async move {
                sqlx::query("SELECT pg_advisory_xact_lock($1)")
                    .bind(OPERATORS_LOCK_KEY)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                let current = load(&mut *conn, id).await?;
                let role = req.role.unwrap_or(current.role);
                let disabled = req.disabled.unwrap_or(current.disabled);
                let new_name = name.unwrap_or_else(|| current.display_name.clone());
                let was_usable_admin = current.role == OperatorRole::Admin && !current.disabled;
                if was_usable_admin && (role != OperatorRole::Admin || disabled) {
                    // Under the lock, so no concurrent change can be
                    // counting this one as the admin that remains.
                    let others: i64 = sqlx::query_scalar(
                        "SELECT count(*) FROM operators \
                         WHERE role = 'admin' AND NOT disabled AND id <> $1",
                    )
                    .bind(id)
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                    if others == 0 {
                        return Err(ApiError::conflict(
                            "last_admin",
                            "至少要保留一个可用的管理员",
                        ));
                    }
                }
                let mut changed = Map::new();
                if role != current.role {
                    changed.insert("role".to_string(), json!(role));
                }
                if disabled != current.disabled {
                    changed.insert("disabled".to_string(), json!(disabled));
                }
                if new_name != current.display_name {
                    changed.insert("display_name".to_string(), json!(new_name));
                }
                let updated = OperatorRow {
                    role,
                    disabled,
                    display_name: new_name,
                    ..current
                };
                if changed.is_empty() {
                    return Ok(updated.view());
                }
                sqlx::query(
                    "UPDATE operators SET role = $2, disabled = $3, display_name = $4 WHERE id = $1",
                )
                .bind(id)
                .bind(updated.role.as_str())
                .bind(updated.disabled)
                .bind(&updated.display_name)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if changed.contains_key("disabled") && updated.disabled {
                    session::revoke_all(&mut *conn, Principal::Operator(id), None).await?;
                }
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "operator.update",
                        target_kind: TARGET_KIND,
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: Value::Object(changed),
                    },
                )
                .await?;
                Ok(updated.view())
            }
            .boxed()
        })
        .await?;
    Ok(Json(view))
}

/// Why `create-operator` refused.
#[derive(Debug, thiserror::Error)]
pub enum CreateFirstError {
    #[error("--email is not an email address")]
    Email,
    #[error("--display-name is longer than {MAX_DISPLAY_NAME_CHARS} characters")]
    DisplayName,
    #[error(
        "the password (the first line of stdin) must have at least {MIN_PASSWORD_LEN} characters"
    )]
    Password,
    #[error("an operator with this email already exists")]
    EmailTaken,
    #[error(transparent)]
    Hash(#[from] password::PasswordError),
    #[error("database (CP_DATABASE_URL): {0}")]
    Db(#[from] DbError),
}

/// `aisix-control-plane create-operator`: creates an operator on the host,
/// audited as the system.
pub async fn create_first(
    db: &Db,
    email: &str,
    role: OperatorRole,
    display: Option<&str>,
    password: &str,
) -> Result<OperatorView, CreateFirstError> {
    let email = normalize_email(email).ok_or(CreateFirstError::Email)?;
    let display_name = display_name(display).map_err(|_| CreateFirstError::DisplayName)?;
    if !password::long_enough(password) {
        return Err(CreateFirstError::Password);
    }
    let hash = password::hash(password)?;
    let new = NewOperator {
        email,
        display_name,
        role,
    };
    db.tx(move |conn| {
        async move {
            insert(conn, Actor::System, &new, &hash).await.map_err(|e| {
                if is_email_taken(&e) {
                    CreateFirstError::EmailTaken
                } else {
                    CreateFirstError::Db(e)
                }
            })
        }
        .boxed()
    })
    .await
}
