//! Request extractors that resolve the session cookie into an identity.
//!
//! The identity is checked per request against the database: a session
//! issued before its account was disabled stops working at once, and any
//! instance honours a session another one issued.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use uuid::Uuid;

use super::session::{self, PrincipalKind, ADMIN_COOKIE, PORTAL_COOKIE};
use super::{MemberRole, OperatorRole};
use crate::db::DbError;
use crate::error::ApiError;
use crate::tenants::TenantStatus;
use crate::AppState;

/// The operator a request comes from, read from the `aisix_cp_admin` cookie
/// only. Extracting it answers `401 unauthenticated` without a live session
/// of an enabled operator; the handler then calls [`OperatorAuth::require`]
/// with the role its `http::ROUTE_ROLES` row names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorAuth {
    pub id: Uuid,
    pub email: String,
    pub role: OperatorRole,
}

impl OperatorAuth {
    /// `403 forbidden` unless this operator's role is at least `min`.
    pub fn require(&self, min: OperatorRole) -> Result<(), ApiError> {
        if self.role >= min {
            Ok(())
        } else {
            Err(ApiError::forbidden())
        }
    }

    /// The operator of the request's session, or `None` when it carries no
    /// live session of an enabled operator.
    pub async fn from_session(
        headers: &HeaderMap,
        state: &AppState,
    ) -> Result<Option<OperatorAuth>, ApiError> {
        let Some(token) = session::token_from_headers(headers, ADMIN_COOKIE) else {
            return Ok(None);
        };
        let Some(id) = session::resolve(&state.db, token, PrincipalKind::Operator).await? else {
            return Ok(None);
        };
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT email, role FROM operators WHERE id = $1 AND NOT disabled")
                .bind(id)
                .fetch_optional(state.db.pool())
                .await
                .map_err(DbError::from)?;
        let Some((email, role)) = row else {
            return Ok(None);
        };
        let role = role.parse().map_err(ApiError::internal)?;
        Ok(Some(OperatorAuth { id, email, role }))
    }
}

impl FromRequestParts<AppState> for OperatorAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<OperatorAuth, ApiError> {
        Self::from_session(&parts.headers, state)
            .await?
            .ok_or_else(ApiError::unauthenticated)
    }
}

/// The tenant member a request comes from, read from the `aisix_cp_portal`
/// cookie only. Extracting it answers `401 unauthenticated` without a live
/// session of a member who is neither disabled nor removed. The handler
/// then calls [`MemberAuth::require_active`] (unless its route is one of
/// `http::INACTIVE_TENANT_ROUTES`) and [`MemberAuth::require`] with the
/// roles its `http::ROUTE_ROLES` row names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberAuth {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub email: String,
    pub role: MemberRole,
    /// The member's tenant's status when the member was read.
    pub tenant_status: TenantStatus,
}

impl MemberAuth {
    /// `403 tenant_not_active` unless the member's tenant is active.
    pub fn require_active(&self) -> Result<(), ApiError> {
        if self.tenant_status == TenantStatus::Active {
            Ok(())
        } else {
            Err(ApiError::tenant_not_active())
        }
    }

    /// `403 forbidden` unless this member's role is one of `allowed`.
    pub fn require(&self, allowed: &[MemberRole]) -> Result<(), ApiError> {
        if allowed.contains(&self.role) {
            Ok(())
        } else {
            Err(ApiError::forbidden())
        }
    }

    /// Whether the member may invite, change and remove members.
    pub fn can_manage_members(&self) -> bool {
        matches!(self.role, MemberRole::Owner | MemberRole::Admin)
    }

    /// Whether the member may see and change every key of the tenant, not
    /// only their own.
    pub fn can_manage_all_keys(&self) -> bool {
        matches!(self.role, MemberRole::Owner | MemberRole::Admin)
    }

    /// Whether the member may hold and use keys at all.
    pub fn can_use_keys(&self) -> bool {
        matches!(
            self.role,
            MemberRole::Owner | MemberRole::Admin | MemberRole::Member
        )
    }

    /// The member of the request's session, or `None` when it carries no
    /// live session of a member who is neither disabled nor removed.
    pub async fn from_session(
        headers: &HeaderMap,
        state: &AppState,
    ) -> Result<Option<MemberAuth>, ApiError> {
        let Some(token) = session::token_from_headers(headers, PORTAL_COOKIE) else {
            return Ok(None);
        };
        let Some(id) = session::resolve(&state.db, token, PrincipalKind::Member).await? else {
            return Ok(None);
        };
        Self::fetch(state.db.pool(), id).await
    }

    /// Member `id` as they stand now, read with `executor`, or `None` when
    /// they are disabled or removed. Read under their tenant's lock, it is
    /// what a change that started from an older read is checked against.
    pub async fn fetch<'e>(
        executor: impl sqlx::PgExecutor<'e>,
        id: Uuid,
    ) -> Result<Option<MemberAuth>, ApiError> {
        let row: Option<(Uuid, String, String, String)> = sqlx::query_as(
            "SELECT m.tenant_id, m.email, m.role, t.status \
             FROM members m JOIN tenants t ON t.id = m.tenant_id \
             WHERE m.id = $1 AND NOT m.disabled AND m.removed_at IS NULL",
        )
        .bind(id)
        .fetch_optional(executor)
        .await
        .map_err(DbError::from)?;
        let Some((tenant_id, email, role, status)) = row else {
            return Ok(None);
        };
        Ok(Some(MemberAuth {
            id,
            tenant_id,
            email,
            role: role.parse().map_err(ApiError::internal)?,
            tenant_status: TenantStatus::parse(&status).ok_or_else(|| {
                ApiError::internal(format!("tenant {tenant_id}: status {status}"))
            })?,
        }))
    }
}

impl FromRequestParts<AppState> for MemberAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<MemberAuth, ApiError> {
        Self::from_session(&parts.headers, state)
            .await?
            .ok_or_else(ApiError::unauthenticated)
    }
}
