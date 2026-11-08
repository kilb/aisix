//! Who a key is, for the lookups: its tenant, its member, and what the
//! key's holder may see of the organization's money.
//!
//! One query decides every lookup's "unknown or revoked key" (`None`, which
//! the routes answer as `404`), and reads the key's member's CURRENT role in
//! the same statement, so a role change shows at the next lookup.
//!
//! The gateway routes (`GET /dp/v1/keys/{key_id}/{quota,prices,requests}`)
//! run on a pool of their own ([`crate::AppState::lookups`]), of
//! [`LOOKUP_POOL_SIZE`] connections: a gateway asks on its callers' behalf,
//! as often as they ask, and a lookup on the shared pool would take the
//! connections usage intake needs. A lookup that finds no connection free
//! within [`LOOKUP_ACQUIRE_TIMEOUT`] answers `503`, which a gateway already
//! reads as the control plane being unavailable.

use std::time::Duration;

use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::MemberRole;
use crate::db::DbError;

/// Connections of the gateway lookups' own pool.
pub const LOOKUP_POOL_SIZE: u32 = 3;

/// How long a gateway lookup waits for a connection of its pool before it
/// answers `503`.
pub const LOOKUP_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);

/// How much of the organization a lookup shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// The organization's balance or credit and its quotas.
    Full,
    /// The member's own and the key's quotas; the organization only through
    /// the effective remaining.
    Own,
}

/// The visibility of a holder with `role`: owners, admins and billing members
/// see the organization, members see their own.
pub fn session_visibility(role: MemberRole) -> Visibility {
    match role {
        MemberRole::Owner | MemberRole::Admin | MemberRole::Billing => Visibility::Full,
        MemberRole::Member => Visibility::Own,
    }
}

/// A live key as the control plane's own record says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyAccess {
    pub key_id: Uuid,
    pub tenant_id: Uuid,
    pub member_id: Option<Uuid>,
    pub is_internal: bool,
    /// From the key's member's current role; a key bound to no member sees
    /// what a member's key sees.
    pub visibility: Visibility,
}

/// Key `key_id`; `None` for an unknown or revoked one.
pub async fn key_access(
    conn: &mut PgConnection,
    key_id: Uuid,
) -> Result<Option<KeyAccess>, DbError> {
    let row: Option<(Uuid, Option<Uuid>, bool, Option<String>)> = sqlx::query_as(
        "SELECT k.tenant_id, k.member_id, t.is_internal, m.role \
         FROM tenant_keys k JOIN tenants t ON t.id = k.tenant_id \
         LEFT JOIN members m ON m.id = k.member_id \
         WHERE k.id = $1 AND k.revoked_at IS NULL",
    )
    .bind(key_id)
    .fetch_optional(conn)
    .await?;
    Ok(
        row.map(|(tenant_id, member_id, is_internal, role)| KeyAccess {
            key_id,
            tenant_id,
            member_id,
            is_internal,
            visibility: role
                .and_then(|r| r.parse::<MemberRole>().ok())
                .map_or(Visibility::Own, session_visibility),
        }),
    )
}
