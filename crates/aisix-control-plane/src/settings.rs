//! Control-plane settings (`cp_settings`), the one environment a
//! deployment is pinned to, and the defaults new tenants start with.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::db::{Db, DbError};
use crate::error::{ApiError, BootError};
use crate::tenants::TenantLimits;

/// Serialises first boots, so instances starting together on an empty
/// database agree on what it holds.
pub const BOOT_LOCK_KEY: i64 = 0x6169_7369_7863_7033;

/// Written once, by the first boot, and never updated.
const ENV_ID: &str = "env_id";
const ETCD_PREFIX: &str = "etcd_prefix";

/// The environment this deployment serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSettings {
    pub env_id: Uuid,
    pub etcd_prefix: String,
}

impl EnvSettings {
    /// `<prefix>/<env_id>/`: the environment's key space. The trailing slash
    /// is part of the contract with the gateway (`EtcdConfig::effective_prefix`
    /// in aisix-core): without it the prefix also matches every environment
    /// id that starts with this one.
    pub fn env_prefix(&self) -> String {
        format!(
            "{}/{}/",
            self.etcd_prefix.trim_end_matches('/'),
            self.env_id
        )
    }

    /// `<prefix>/global/`: keys shared by every environment.
    pub fn global_prefix(&self) -> String {
        format!("{}/global/", self.etcd_prefix.trim_end_matches('/'))
    }
}

/// Pins the database to one environment, in one transaction under
/// [`BOOT_LOCK_KEY`].
///
/// The first boot stores `seed` (or a fresh id) and `prefix`; every boot then
/// reads back what is stored. A `seed` that disagrees with the stored id, or
/// a different `prefix`, refuses the boot: either would move every resource
/// to a key space no gateway reads.
pub async fn ensure_env(
    db: &Db,
    seed: Option<Uuid>,
    prefix: &str,
) -> Result<EnvSettings, BootError> {
    let prefix = prefix.to_string();
    db.tx(move |conn| {
        Box::pin(async move {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(BOOT_LOCK_KEY)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
            let minted = seed.unwrap_or_else(Uuid::new_v4);
            sqlx::query(
                "INSERT INTO cp_settings (key, value) \
                 VALUES ($1, to_jsonb($2::text)), ($3, to_jsonb($4::text)) \
                 ON CONFLICT (key) DO NOTHING",
            )
            .bind(ENV_ID)
            .bind(minted.to_string())
            .bind(ETCD_PREFIX)
            .bind(&prefix)
            .execute(&mut *conn)
            .await
            .map_err(DbError::from)?;
            let (env_id, stored_prefix): (Uuid, String) = sqlx::query_as(
                "SELECT (e.value #>> '{}')::uuid, p.value #>> '{}' \
                 FROM cp_settings e, cp_settings p \
                 WHERE e.key = $1 AND p.key = $2",
            )
            .bind(ENV_ID)
            .bind(ETCD_PREFIX)
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::from)?;
            if let Some(seed) = seed {
                if seed != env_id {
                    return Err(BootError::EnvIdMismatch {
                        stored: env_id,
                        configured: seed,
                    });
                }
            }
            if stored_prefix != prefix {
                return Err(BootError::PrefixChanged {
                    stored: stored_prefix,
                    configured: prefix,
                });
            }
            Ok(EnvSettings {
                env_id,
                etcd_prefix: stored_prefix,
            })
        })
    })
    .await
}

/// Sets `key` to `value`, replacing any earlier value. Never used for the
/// keys [`ensure_env`] writes: those are written once.
pub async fn set(conn: &mut PgConnection, key: &str, value: &Value) -> Result<(), DbError> {
    debug_assert!(
        key != ENV_ID && key != ETCD_PREFIX,
        "{key} is written only by ensure_env"
    );
    sqlx::query(
        "INSERT INTO cp_settings (key, value) VALUES ($1, $2) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(key)
    .bind(value)
    .execute(conn)
    .await?;
    Ok(())
}

/// The value of `key`, if it is set.
pub async fn get(conn: &mut PgConnection, key: &str) -> Result<Option<Value>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT value FROM cp_settings WHERE key = $1")
            .bind(key)
            .fetch_optional(conn)
            .await?,
    )
}

const TENANT_DEFAULTS: &str = "tenant_defaults";

/// What a new ordinary tenant starts with (`/admin/settings/tenant-defaults`):
/// its limits, and the provider groups it is confined to (ruling 27). A
/// tenant copies them when it is created and keeps its copy; changing them
/// changes no existing tenant.
///
/// Stored defaults are read leniently, so an instance of an older release
/// reads what a newer one wrote; the write path (`PUT
/// /admin/settings/tenant-defaults`) refuses unknown fields, and refuses a
/// missing or empty `default_provider_groups`, since no groups at all opens
/// every upstream to new tenants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_concurrency_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_rpm_max: Option<u32>,
    pub default_provider_groups: Vec<String>,
}

impl TenantDefaults {
    /// What the first boot writes (ruling 14).
    pub fn first_boot() -> TenantDefaults {
        TenantDefaults {
            concurrency: Some(20),
            rpm: Some(1200),
            tpm: None,
            key_concurrency_max: Some(10),
            key_rpm_max: None,
            default_provider_groups: vec!["shared".to_string()],
        }
    }

    pub fn limits(&self) -> TenantLimits {
        TenantLimits {
            concurrency: self.concurrency,
            rpm: self.rpm,
            tpm: self.tpm,
            key_concurrency_max: self.key_concurrency_max,
            key_rpm_max: self.key_rpm_max,
        }
    }
}

/// The defaults for new tenants; [`TenantDefaults::first_boot`] on a
/// database no boot has written them to.
pub async fn tenant_defaults(conn: &mut PgConnection) -> Result<TenantDefaults, ApiError> {
    match get(conn, TENANT_DEFAULTS).await? {
        Some(value) => serde_json::from_value(value).map_err(ApiError::internal),
        None => Ok(TenantDefaults::first_boot()),
    }
}

pub async fn set_tenant_defaults(
    conn: &mut PgConnection,
    defaults: &TenantDefaults,
) -> Result<(), ApiError> {
    let value = serde_json::to_value(defaults).map_err(ApiError::internal)?;
    Ok(set(conn, TENANT_DEFAULTS, &value).await?)
}

/// Writes [`TenantDefaults::first_boot`] unless defaults are stored, so a
/// later release that changes the first-boot values changes no running
/// deployment.
pub(crate) async fn seed_tenant_defaults(conn: &mut PgConnection) -> Result<(), ApiError> {
    let value = serde_json::to_value(TenantDefaults::first_boot()).map_err(ApiError::internal)?;
    sqlx::query(
        "INSERT INTO cp_settings (key, value) VALUES ($1, $2) ON CONFLICT (key) DO NOTHING",
    )
    .bind(TENANT_DEFAULTS)
    .bind(value)
    .execute(conn)
    .await
    .map_err(DbError::from)?;
    Ok(())
}
