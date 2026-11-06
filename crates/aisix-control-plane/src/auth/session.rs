//! Browser sessions, stored in the database so that every control-plane
//! instance honours a session any of them issued.
//!
//! A token is 32 random bytes in hex. Only its SHA-256 is stored, so a
//! copy of the table does not log anyone in.

use std::time::Duration;

use axum::http::{header, HeaderMap};
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::db::{Db, DbError};

/// The operators' session cookie. The admin API reads only this one.
pub const ADMIN_COOKIE: &str = "aisix_cp_admin";

/// The tenant members' session cookie. The tenant API reads only this one.
pub const PORTAL_COOKIE: &str = "aisix_cp_portal";

pub const OPERATOR_SESSION_TTL: Duration = Duration::from_secs(8 * 3600);
pub const MEMBER_SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// Hex characters in a token.
const TOKEN_LEN: usize = 64;

/// Whose session it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    Operator(Uuid),
    Member(Uuid),
}

/// The kind of principal a session belongs to: a session of one kind never
/// resolves as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalKind {
    Operator,
    Member,
}

impl PrincipalKind {
    fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::Operator => "operator",
            PrincipalKind::Member => "member",
        }
    }
}

impl Principal {
    fn kind(self) -> PrincipalKind {
        match self {
            Principal::Operator(_) => PrincipalKind::Operator,
            Principal::Member(_) => PrincipalKind::Member,
        }
    }

    fn id(self) -> Uuid {
        match self {
            Principal::Operator(id) | Principal::Member(id) => id,
        }
    }
}

fn digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Whether `token` has the shape of one this module issues. Anything else
/// is refused without a query.
fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_LEN && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Starts a session for `principal`, valid for `ttl` from the database's
/// clock, and returns its token.
pub async fn create(
    conn: &mut PgConnection,
    principal: Principal,
    ttl: Duration,
) -> Result<String, DbError> {
    let mut bytes = [0u8; 32];
    // A failing system RNG panics here rather than issuing a guessable
    // token.
    rand_core::OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    sqlx::query(
        "INSERT INTO sessions (token_sha256, principal_kind, principal_id, expires_at) \
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(digest(&token))
    .bind(principal.kind().as_str())
    .bind(principal.id())
    .bind(ttl.as_secs_f64())
    .execute(conn)
    .await?;
    Ok(token)
}

/// The principal whose live session `token` is, if it is one of `kind`.
pub async fn resolve(db: &Db, token: &str, kind: PrincipalKind) -> Result<Option<Uuid>, DbError> {
    if !well_formed(token) {
        return Ok(None);
    }
    Ok(sqlx::query_scalar(
        "SELECT principal_id FROM sessions \
         WHERE token_sha256 = $1 AND principal_kind = $2 AND expires_at > now()",
    )
    .bind(digest(token))
    .bind(kind.as_str())
    .fetch_optional(db.pool())
    .await?)
}

/// Ends the session `token`, if there is one of `kind`, with `executor`
/// (the pool, or the connection of a transaction that starts a session in
/// its place). A token of the other kind is left alone, so neither API can
/// end the other's sessions.
pub async fn revoke<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    token: &str,
    kind: PrincipalKind,
) -> Result<(), DbError> {
    if !well_formed(token) {
        return Ok(());
    }
    sqlx::query("DELETE FROM sessions WHERE token_sha256 = $1 AND principal_kind = $2")
        .bind(digest(token))
        .bind(kind.as_str())
        .execute(executor)
        .await?;
    Ok(())
}

/// Ends every session of `principal`, except `except` when given.
pub async fn revoke_all(
    conn: &mut PgConnection,
    principal: Principal,
    except: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query(
        "DELETE FROM sessions WHERE principal_kind = $1 AND principal_id = $2 \
         AND ($3::bytea IS NULL OR token_sha256 <> $3)",
    )
    .bind(principal.kind().as_str())
    .bind(principal.id())
    .bind(except.map(digest))
    .execute(conn)
    .await?;
    Ok(())
}

/// The `Set-Cookie` value for a session. `HttpOnly` keeps it from scripts,
/// `SameSite=Strict` from other sites' requests (with JSON-only mutating
/// routes, that is the CSRF defence), and `Secure` from plain HTTP.
pub fn cookie(name: &str, token: &str, ttl: Duration) -> String {
    format!(
        "{name}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}",
        ttl.as_secs()
    )
}

/// The `Set-Cookie` value that removes the cookie `name`.
pub fn cleared_cookie(name: &str) -> String {
    format!("{name}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0")
}

/// The value of the cookie `name` in the request, if it carries one.
pub fn token_from_headers<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn the_cookie_is_found_by_its_exact_name() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("x_aisix_cp_admin=wrong; theme=dark"),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("aisix_cp_portal=other;aisix_cp_admin=abc"),
        );
        assert_eq!(token_from_headers(&headers, ADMIN_COOKIE), Some("abc"));
        assert_eq!(
            token_from_headers(&headers, "aisix_cp_portal"),
            Some("other")
        );
        assert_eq!(token_from_headers(&headers, "missing"), None);
    }

    #[test]
    fn only_tokens_of_the_issued_shape_are_looked_up() {
        assert!(well_formed(&"ab".repeat(32)));
        assert!(!well_formed(&"ab".repeat(31)));
        assert!(!well_formed(&"zz".repeat(32)));
    }
}
