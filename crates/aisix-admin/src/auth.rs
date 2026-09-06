//! Admin-key bearer auth. Distinct from the proxy's API-key auth:
//!
//! - Admin keys come from `config.admin.admin_keys` (static, bootstrap
//!   config), not the `ApiKey` table in etcd.
//! - Presented as `Authorization: Bearer <key>` with an `x-api-key`
//!   fallback.
//!
//! This extractor short-circuits with an `AdminError::Unauthorized`
//! envelope before any handler runs.

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;

use crate::error::AdminError;
use crate::state::AdminState;

/// Marker yielded by the extractor once an admin key has been verified.
/// Handlers don't need the key itself — just proof that the caller
/// supplied a valid one — so the type is empty by design.
#[derive(Debug, Clone, Copy)]
pub struct AdminAuth;

#[axum::async_trait]
impl<S> FromRequestParts<S> for AdminAuth
where
    S: Send + Sync,
    AdminState: FromRef<S>,
{
    type Rejection = AdminError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let admin_state = AdminState::from_ref(state);
        if !is_admin_authorized(&parts.headers, &admin_state.admin_keys) {
            return Err(AdminError::Unauthorized);
        }
        Ok(AdminAuth)
    }
}

/// Header-level admin-key check shared by the extractor above and by
/// router-layer middleware (which runs *before* per-handler extractors
/// and therefore cannot use `AdminAuth` directly). True iff the request
/// carries a valid admin key.
pub(crate) fn is_admin_authorized(headers: &axum::http::HeaderMap, admin_keys: &[String]) -> bool {
    match extract_bearer(headers) {
        // Compare digests, not the secrets themselves, and compare them
        // without short-circuiting. `String::eq` stops at the first differing
        // byte, so comparing plaintext admin keys leaks a prefix oracle in its
        // timing; the proxy's own API-key path already avoids this by looking
        // the caller up under `hash_bearer`, and this is the same idea. The
        // fold keeps the digest comparison itself constant-time.
        Ok(token) => {
            let presented = aisix_core::ApiKey::hash_bearer(&token);
            admin_keys.iter().fold(false, |seen, k| {
                seen | digests_match(&presented, &aisix_core::ApiKey::hash_bearer(k))
            })
        }
        Err(_) => false,
    }
}

/// Byte-compare two hex digests without an early exit.
fn digests_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn extract_bearer(headers: &axum::http::HeaderMap) -> Result<String, AdminError> {
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
        let s = auth.to_str().map_err(|_| AdminError::Unauthorized)?;
        if let Some(rest) = s.strip_prefix("Bearer ") {
            let rest = rest.trim();
            if rest.is_empty() {
                return Err(AdminError::Unauthorized);
            }
            return Ok(rest.to_string());
        }
        return Err(AdminError::Unauthorized);
    }
    if let Some(raw) = headers.get("x-api-key") {
        let s = raw.to_str().map_err(|_| AdminError::Unauthorized)?;
        let s = s.trim();
        if s.is_empty() {
            return Err(AdminError::Unauthorized);
        }
        return Ok(s.to_string());
    }
    Err(AdminError::Unauthorized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, Request};

    fn parts_with(headers: HeaderMap) -> Parts {
        let mut req = Request::builder().uri("/").body(()).unwrap();
        *req.headers_mut() = headers;
        req.into_parts().0
    }

    /// The admin key is a secret compared against attacker-supplied input.
    /// A short-circuiting compare of the plaintext leaks a prefix oracle in
    /// its timing, so the check goes through a digest and does not exit
    /// early — the same shape the proxy's API-key lookup already has.
    #[test]
    fn admin_key_check_matches_exactly_and_rejects_prefixes() {
        fn authorized(token: &str, keys: &[String]) -> bool {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
            is_admin_authorized(&h, keys)
        }

        let keys = vec!["admin-secret".to_string(), "second-key".to_string()];
        assert!(authorized("admin-secret", &keys));
        assert!(
            authorized("second-key", &keys),
            "every configured key works"
        );

        assert!(!authorized("admin-secre", &keys), "a prefix is not the key");
        assert!(!authorized("admin-secretX", &keys), "nor is a superstring");
        assert!(!authorized("", &keys));
        assert!(
            !authorized("admin-secret", &[]),
            "no keys configured = deny"
        );
    }

    #[test]
    fn digests_match_is_length_safe() {
        assert!(digests_match("abcd", "abcd"));
        assert!(!digests_match("abcd", "abce"));
        assert!(!digests_match("abc", "abcd"));
    }

    #[test]
    fn extract_bearer_reads_authorization_header() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer admin-secret"),
        );
        assert_eq!(
            extract_bearer(&parts_with(h).headers).unwrap(),
            "admin-secret"
        );
    }

    #[test]
    fn extract_bearer_accepts_x_api_key_fallback() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("admin-secret"));
        assert_eq!(
            extract_bearer(&parts_with(h).headers).unwrap(),
            "admin-secret"
        );
    }

    #[test]
    fn extract_bearer_rejects_missing_and_wrong_scheme() {
        assert!(matches!(
            extract_bearer(&parts_with(HeaderMap::new()).headers),
            Err(AdminError::Unauthorized)
        ));

        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic Zm9v"),
        );
        assert!(matches!(
            extract_bearer(&parts_with(h).headers),
            Err(AdminError::Unauthorized)
        ));
    }

    /// Every admin handler must take the [`AdminAuth`] extractor.
    ///
    /// Authentication here is an extractor, not a router layer, so a handler
    /// that simply omits the parameter compiles, routes, and serves the
    /// configuration surface to anyone who can reach the listener. Nothing
    /// about that reads as wrong at the call site — the route registration
    /// looks identical either way.
    ///
    /// The exception list is explicit and carries its reason; anything else
    /// new has to be added here deliberately, which is the point.
    #[test]
    fn every_admin_handler_takes_the_auth_extractor() {
        /// Handlers deliberately outside admin authentication.
        ///
        /// `playground_chat_completions` forwards into the proxy router,
        /// where the caller's *proxy* key is authenticated by the proxy's own
        /// middleware — an admin key is the wrong credential for it and
        /// requiring one would lock out the surface's only intended callers.
        const NOT_ADMIN_AUTHENTICATED: &[&str] = &["playground_chat_completions"];

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        let mut unguarded: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read src/") {
            let path = entry.expect("dir entry").path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if !name.contains("handler") || !name.ends_with(".rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read handler file");
            // 只看生产段：测试里会自己拼请求，不经过提取器。
            let production = src.split_once("#[cfg(test)]").map_or(&src[..], |(a, _)| a);
            for (idx, _) in production.match_indices("pub async fn ") {
                let rest = &production[idx + "pub async fn ".len()..];
                let Some(paren) = rest.find('(') else {
                    continue;
                };
                let fname = rest[..paren].trim().to_string();
                let Some(close) = rest.find(')') else {
                    continue;
                };
                let args = &rest[paren..close];
                checked += 1;
                if !args.contains("AdminAuth") && !NOT_ADMIN_AUTHENTICATED.contains(&fname.as_str())
                {
                    unguarded.push(format!("{name}::{fname}"));
                }
            }
        }
        assert!(
            checked >= 20,
            "只扫到 {checked} 个处理器，抽取多半坏了（实际有三十几个）"
        );
        assert!(
            unguarded.is_empty(),
            "这些 admin 处理器没有取 AdminAuth —— 它们对任何能连到这个监听口的\
             人开放配置面：{unguarded:?}"
        );
    }

    #[test]
    fn is_admin_authorized_checks_key_membership() {
        let keys = vec!["admin-secret".to_string()];
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer admin-secret"),
        );
        assert!(is_admin_authorized(&h, &keys));

        let mut wrong = HeaderMap::new();
        wrong.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer nope"),
        );
        assert!(!is_admin_authorized(&wrong, &keys));
        assert!(!is_admin_authorized(&HeaderMap::new(), &keys));
    }
}
