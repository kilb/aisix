//! What gateways reach the control plane for: the mTLS listener on
//! `CP_DP_ADDR` and the `/dp/*` endpoints behind it.
//!
//! A gateway is known by its client certificate alone, never by a header or
//! a field it sends. The listener reads the certificate's SANs once per
//! connection ([`ConnIdentity`]); [`DpIdentity`] checks them against this
//! environment and the credential list on every request, so a credential
//! revoked while its gateway is connected is refused from the next request
//! on.

pub mod heartbeat;
pub mod listener;
pub mod store;

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgListener, PgNotification};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::db::{Db, DbError};
use crate::error::ApiError;
use crate::gateways::CREDENTIALS_CHANNEL;
use crate::AppState;

/// How often the credential list is read again in full, whatever the
/// notifications say: the bound on how long a change missed while not
/// listening goes unseen.
const FULL_REFRESH: Duration = Duration::from_secs(10);

/// Connections of the credential cache's own pool: one held by the
/// notification listener, one for reads.
pub const CREDENTIALS_POOL_SIZE: u32 = 2;

/// Who a connection's client certificate names: read once per connection,
/// after the TLS verifier has checked the chain, and attached to each of its
/// requests. Not yet checked against anything else; [`DpIdentity`] does
/// that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnIdentity {
    pub env_id: String,
    pub dp_id: String,
    /// Lowercase hex SHA-256 of the certificate's DER.
    pub fingerprint: String,
}

/// The gateway credential a request comes from. Extracting it answers `403
/// forbidden` unless the connection's certificate names this environment
/// and a listed credential that is neither revoked nor expired and whose
/// certificate it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DpIdentity {
    pub dp_id: Uuid,
}

impl FromRequestParts<AppState> for DpIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<DpIdentity, ApiError> {
        // Only the gateway listener attaches one.
        let Some(conn) = parts.extensions.get::<ConnIdentity>() else {
            return Err(ApiError::forbidden());
        };
        if conn.env_id != state.env.env_id.to_string() {
            tracing::info!(env_id = %conn.env_id, dp_id = %conn.dp_id,
                "refused a gateway certificate for another environment");
            return Err(ApiError::forbidden());
        }
        let Ok(dp_id) = Uuid::parse_str(&conn.dp_id) else {
            return Err(ApiError::forbidden());
        };
        if !state.creds.check(dp_id, &conn.fingerprint).await? {
            tracing::info!(%dp_id, "refused a gateway credential that is revoked, expired or unlisted");
            return Err(ApiError::forbidden());
        }
        Ok(DpIdentity { dp_id })
    }
}

/// A credential as the cache holds it.
#[derive(Debug, Clone)]
struct Listed {
    fingerprint: String,
    not_after: DateTime<Utc>,
    revoked: bool,
}

/// The gateway credentials, held in memory so a request is checked without
/// a query. Cheap to clone: clones share one list.
///
/// It follows the database through [`CredentialCache::spawn_refresh`]: a
/// notification on [`CREDENTIALS_CHANNEL`] (sent when a credential is issued
/// or revoked, by any instance) reads the list again at once, and it is read
/// again in full every 10 seconds regardless.
#[derive(Clone)]
pub struct CredentialCache {
    listed: Arc<RwLock<HashMap<Uuid, Listed>>>,
    /// Every read of the cache's, and its listening, go through this pool.
    /// Given one of its own ([`CREDENTIALS_POOL_SIZE`]), a revoke is seen
    /// however busy request traffic keeps the main pool.
    db: Db,
}

impl CredentialCache {
    /// The cache, filled from the database through `db`, which it keeps.
    pub async fn load(db: &Db) -> Result<CredentialCache, DbError> {
        let cache = CredentialCache {
            listed: Arc::default(),
            db: db.clone(),
        };
        cache.reload().await?;
        Ok(cache)
    }

    /// Whether `dp_id` is a listed credential, not revoked, not expired, and
    /// issued with the certificate whose fingerprint is `fingerprint`.
    pub fn is_valid(&self, dp_id: &str, fingerprint: &str) -> bool {
        Uuid::parse_str(dp_id).is_ok_and(|dp_id| self.admits(dp_id, fingerprint))
    }

    fn admits(&self, dp_id: Uuid, fingerprint: &str) -> bool {
        self.read().get(&dp_id).is_some_and(|listed| {
            !listed.revoked && listed.not_after > Utc::now() && listed.fingerprint == fingerprint
        })
    }

    /// [`CredentialCache::is_valid`], reading `dp_id`'s row first when the
    /// cache holds none: a credential issued a moment ago may be in use
    /// before its notification has arrived. A credential the cache holds is
    /// judged from the cache.
    pub(crate) async fn check(&self, dp_id: Uuid, fingerprint: &str) -> Result<bool, DbError> {
        if !self.read().contains_key(&dp_id) {
            let row: Option<(String, DateTime<Utc>, bool)> = sqlx::query_as(
                "SELECT fingerprint_sha256, not_after, revoked_at IS NOT NULL \
                 FROM dp_credentials WHERE dp_id = $1",
            )
            .bind(dp_id)
            .fetch_optional(self.db.pool())
            .await?;
            if let Some((fingerprint, not_after, revoked)) = row {
                // A reload may have stored it meanwhile, read later than
                // this row was: that one stands.
                self.write().entry(dp_id).or_insert(Listed {
                    fingerprint,
                    not_after,
                    revoked,
                });
            }
        }
        Ok(self.admits(dp_id, fingerprint))
    }

    /// Replaces the cache with the credential list as it is now.
    async fn reload(&self) -> Result<(), DbError> {
        let rows: Vec<(Uuid, String, DateTime<Utc>, bool)> = sqlx::query_as(
            "SELECT dp_id, fingerprint_sha256, not_after, revoked_at IS NOT NULL \
             FROM dp_credentials",
        )
        .fetch_all(self.db.pool())
        .await?;
        let listed = rows
            .into_iter()
            .map(|(dp_id, fingerprint, not_after, revoked)| {
                (
                    dp_id,
                    Listed {
                        fingerprint,
                        not_after,
                        revoked,
                    },
                )
            })
            .collect();
        *self.write() = listed;
        Ok(())
    }

    /// Keeps the cache current until the task is aborted: the list is read
    /// again on each notification on [`CREDENTIALS_CHANNEL`], every 10
    /// seconds, and whenever listening (re)starts, which covers what was
    /// sent while nothing listened. Listening holds one pool connection.
    pub fn spawn_refresh(&self) -> JoinHandle<()> {
        let cache = self.clone();
        tokio::spawn(async move { cache.refresh().await })
    }

    async fn refresh(self) {
        let mut listener: Option<PgListener> = None;
        let mut every = tokio::time::interval(FULL_REFRESH);
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            if listener.is_none() {
                match listen(&self.db).await {
                    Ok(l) => listener = Some(l),
                    Err(e) => tracing::warn!(error = %e,
                        "cannot listen for gateway credential changes; reading them every 10 s"),
                }
                self.reload_logged().await;
            }
            tokio::select! {
                _ = every.tick() => {}
                received = next_notice(&mut listener) => {
                    // `Ok(None)`: the connection was lost and made again,
                    // and what was sent meanwhile is lost; the reload
                    // below covers it.
                    if let Err(e) = received {
                        tracing::warn!(error = %e, "lost the gateway credential notifications");
                        listener = None;
                        continue;
                    }
                }
            }
            self.reload_logged().await;
        }
    }

    async fn reload_logged(&self) {
        if let Err(e) = self.reload().await {
            tracing::warn!(error = %e, "cannot read the gateway credentials; keeping the last list");
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, HashMap<Uuid, Listed>> {
        // A panic while the lock was held cannot leave the map half
        // written: it is replaced whole or added to by one entry.
        self.listed.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<Uuid, Listed>> {
        self.listed.write().unwrap_or_else(PoisonError::into_inner)
    }
}

async fn listen(db: &Db) -> Result<PgListener, sqlx::Error> {
    let mut listener = PgListener::connect_with(db.pool()).await?;
    listener.listen(CREDENTIALS_CHANNEL).await?;
    Ok(listener)
}

/// The next notification, or never while not listening.
async fn next_notice(
    listener: &mut Option<PgListener>,
) -> Result<Option<PgNotification>, sqlx::Error> {
    match listener {
        Some(listener) => listener.try_recv().await,
        None => std::future::pending().await,
    }
}
