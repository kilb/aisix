//! Control-plane settings (`cp_settings`), and the one environment a
//! deployment is pinned to.

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::db::{Db, DbError};
use crate::error::BootError;

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
