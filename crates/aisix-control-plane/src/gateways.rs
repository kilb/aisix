//! Gateway credentials, the deployment bundle a gateway boots from, and the
//! environment view that bundle is built from.
//!
//! A credential is a client certificate of the control plane's CA (see
//! [`crate::pki`]), identified by its `dp_id`. Issuing one returns the
//! bundle: the certificate, its private key (this is the only time the key
//! leaves the control plane), the CA, and the environment variables the
//! official gateway image boots in managed mode with.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::config::CpConfig;
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::{ApiJson, API_CONTRACT};
use crate::pki::IssuedCert;
use crate::AppState;

/// Notified with the `dp_id` whenever a credential is issued or revoked, so
/// every instance's gateway listener sees the change at once.
pub const CREDENTIALS_CHANNEL: &str = "dp_credentials_changed";

/// Where the official image bakes its managed-mode configuration
/// (`Dockerfile`). The image's entrypoint exits without a file at
/// `AISIX_CONFIG_PATH`, which defaults to `/etc/aisix/config.yaml`.
const MANAGED_CONFIG_PATH: &str = "/etc/aisix/config.managed.yaml";

/// The variable the platform id key is passed in. Not an `AISIX_*` name:
/// those belong to the gateway's configuration loader.
const PLATFORM_ID_KEY_VAR: &str = "PLATFORM_ID_KEY";

const DEFAULT_VALIDITY_DAYS: u32 = 365;
const MAX_VALIDITY_DAYS: u32 = 825;
const MAX_NAME_CHARS: usize = 128;

const TARGET_KIND: &str = "gateway_credential";

/// Shown with every bundle. Replicas started from one bundle share its
/// `dp_id`, but each writes its own copy of the bundle and its own snapshot
/// cache, so each needs a volume of its own.
const BUNDLE_NOTE: &str = "私钥只在此处返回一次，控制面不保存，请立即妥善保管。\
    网关会把证书和最近一次的配置快照（含上游凭据明文）写入 /var/lib/aisix，\
    请为该目录挂载持久卷，并只允许网关进程读取。\
    多个网关副本可以共用同一份凭据，但每个网关实例都需要挂载自己独立的卷，不要在实例之间共享。";

/// `GatewayCredential` in the API.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CredentialView {
    pub dp_id: Uuid,
    pub name: String,
    pub fingerprint_sha256: String,
    pub not_after: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub revoked_at: Option<DateTime<Utc>>,
}

const CREDENTIAL_COLUMNS: &str =
    "dp_id, name, fingerprint_sha256, not_after, created_at, created_by, revoked_at";

/// The gateways' etcd endpoint, `host:port` of `CP_DP_BASE_URL`.
fn etcd_endpoint(cfg: &CpConfig) -> Result<String, ApiError> {
    cfg.dp_etcd_endpoint()
        .ok_or_else(|| ApiError::internal("CP_DP_BASE_URL names no host"))
}

/// `GET /admin/environment`: what every gateway of this deployment is
/// given. `dp_base_url` is shown reduced to its origin, as every configured
/// URL is; the bundle carries it as configured.
///
/// So this view is for display only: anything a gateway is started with,
/// such as a deployment snippet, is built from a bundle's `env`, never from
/// these values. A path or userinfo on `CP_DP_BASE_URL` is not here.
pub async fn environment(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    Ok(Json(json!({
        "env_id": state.env.env_id,
        "etcd_prefix": state.env.etcd_prefix,
        "dp_base_url": aisix_core::redact_url_for_log(&state.cfg.dp_base_url),
        "etcd_endpoint": etcd_endpoint(&state.cfg)?,
        "cp_version": &*aisix_core::HEARTBEAT_VERSION,
        "api_contract": API_CONTRACT,
    })))
}

/// The environment the official image boots a gateway of this deployment
/// in: the managed configuration file, this control plane's REST and etcd
/// endpoints, the credential, the key-space prefix, the snapshot cache (a
/// gateway restarting while the control plane is down serves its last
/// configuration), and platform mode with this deployment's id key.
///
/// No `AISIX_ETCD__USER` or `AISIX_ETCD__PASSWORD_ENV`: kine has no
/// authentication service, and a gateway that tries to log in fails its
/// boot.
fn bundle_env(
    state: &AppState,
    issued: &IssuedCert,
) -> Result<BTreeMap<&'static str, String>, ApiError> {
    let on = || "true".to_string();
    Ok(BTreeMap::from([
        ("AISIX_CONFIG_PATH", MANAGED_CONFIG_PATH.to_string()),
        ("AISIX_MANAGED__ENABLED", on()),
        ("AISIX_MANAGED__CP_BASE_URL", state.cfg.dp_base_url.clone()),
        (
            "AISIX_MANAGED__CP_ETCD_ENDPOINT",
            etcd_endpoint(&state.cfg)?,
        ),
        ("AISIX_MANAGED__CP_CERT_PEM", issued.cert_pem.clone()),
        ("AISIX_MANAGED__CP_KEY_PEM", issued.key_pem.clone()),
        ("AISIX_MANAGED__CP_CA_PEM", state.pki.ca_pem().to_string()),
        ("AISIX_ETCD__PREFIX", state.env.etcd_prefix.clone()),
        ("AISIX_MANAGED__SNAPSHOT_CACHE_ENABLED", on()),
        ("AISIX_PROXY__PLATFORM__ENABLED", on()),
        (
            "AISIX_PROXY__PLATFORM__ID_KEY_ENV",
            PLATFORM_ID_KEY_VAR.to_string(),
        ),
        (PLATFORM_ID_KEY_VAR, state.pki.platform_id_key_hex()),
    ]))
}

/// `GET /admin/gateways/credentials`: `{items: [GatewayCredential]}`,
/// oldest first.
pub async fn list_credentials(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let items: Vec<CredentialView> = sqlx::query_as(&format!(
        "SELECT {CREDENTIAL_COLUMNS} FROM dp_credentials ORDER BY created_at, dp_id"
    ))
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct IssueCredential {
    name: String,
    /// Any integer, so an out-of-range one is answered at its path.
    validity_days: Option<i64>,
}

fn detail(path: &str, message: impl Into<String>) -> Detail {
    Detail {
        path: path.to_string(),
        message: message.into(),
    }
}

/// `POST /admin/gateways/credentials` → `201` with the bundle:
/// `{dp_id, name, not_after, cert_pem, key_pem, ca_pem, env, note}`. Served
/// with `Cache-Control: no-store`, as it carries a private key.
pub async fn issue_credential(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<IssueCredential>,
) -> Result<Response, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let mut problems = Vec::new();
    let name = req.name.trim().to_string();
    if name.is_empty() {
        problems.push(detail("/name", "名称不能为空"));
    } else if name.chars().count() > MAX_NAME_CHARS {
        problems.push(detail(
            "/name",
            format!("名称不能超过 {MAX_NAME_CHARS} 个字符"),
        ));
    }
    let days = match req.validity_days {
        None => Some(DEFAULT_VALIDITY_DAYS),
        Some(days) => u32::try_from(days)
            .ok()
            .filter(|d| (1..=MAX_VALIDITY_DAYS).contains(d)),
    };
    if days.is_none() {
        problems.push(detail(
            "/validity_days",
            format!("有效期必须在 1 到 {MAX_VALIDITY_DAYS} 天之间"),
        ));
    }
    let (Some(days), true) = (days, problems.is_empty()) else {
        return Err(ApiError::validation(problems));
    };

    let dp_id = Uuid::new_v4();
    let issued = state
        .pki
        .issue_client(dp_id, days)
        .map_err(ApiError::internal)?;
    let env = bundle_env(&state, &issued)?;
    let row = (
        name.clone(),
        issued.serial.clone(),
        issued.fingerprint_sha256.clone(),
        issued.not_before,
        issued.not_after,
    );
    state
        .db
        .tx(move |conn| {
            async move {
                let (name, serial, fingerprint, not_before, not_after) = row;
                sqlx::query(
                    "INSERT INTO dp_credentials \
                     (dp_id, name, serial, fingerprint_sha256, not_before, not_after, created_by) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(dp_id)
                .bind(&name)
                .bind(&serial)
                .bind(&fingerprint)
                .bind(not_before)
                .bind(not_after)
                .bind(auth.id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                notify(&mut *conn, dp_id).await?;
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "gateway_credential.issue",
                        target_kind: TARGET_KIND,
                        target_id: Some(dp_id.to_string()),
                        tenant_id: None,
                        detail: json!({
                            "name": name,
                            "fingerprint_sha256": fingerprint,
                            "not_after": not_after,
                        }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(())
            }
            .boxed()
        })
        .await?;
    let bundle = json!({
        "dp_id": dp_id,
        "name": name,
        "not_after": issued.not_after,
        "cert_pem": issued.cert_pem,
        "key_pem": issued.key_pem,
        "ca_pem": state.pki.ca_pem(),
        "env": env,
        "note": BUNDLE_NOTE,
    });
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(bundle),
    )
        .into_response())
}

/// `POST /admin/gateways/credentials/{dp_id}/revoke` → `200
/// GatewayCredential`. Revoking a revoked credential changes and records
/// nothing.
pub async fn revoke_credential(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(dp_id): Path<String>,
) -> Result<Json<CredentialView>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let dp_id = Uuid::parse_str(&dp_id).map_err(|_| ApiError::not_found())?;
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let revoked: Option<CredentialView> = sqlx::query_as(&format!(
                    "UPDATE dp_credentials SET revoked_at = now() \
                     WHERE dp_id = $1 AND revoked_at IS NULL RETURNING {CREDENTIAL_COLUMNS}"
                ))
                .bind(dp_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let Some(view) = revoked else {
                    let current: Option<CredentialView> = sqlx::query_as(&format!(
                        "SELECT {CREDENTIAL_COLUMNS} FROM dp_credentials WHERE dp_id = $1"
                    ))
                    .bind(dp_id)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                    return current.ok_or_else(ApiError::not_found);
                };
                notify(&mut *conn, dp_id).await?;
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "gateway_credential.revoke",
                        target_kind: TARGET_KIND,
                        target_id: Some(dp_id.to_string()),
                        tenant_id: None,
                        detail: json!({ "name": view.name }),
                    },
                )
                .await?;
                Ok(view)
            }
            .boxed()
        })
        .await?;
    Ok(Json(view))
}

/// Notifies [`CREDENTIALS_CHANNEL`] with `dp_id` when the transaction
/// commits.
async fn notify(conn: &mut sqlx::PgConnection, dp_id: Uuid) -> Result<(), DbError> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CREDENTIALS_CHANNEL)
        .bind(dp_id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}
