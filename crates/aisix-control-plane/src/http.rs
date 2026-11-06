//! The browser listener's routes, and the role table they are checked
//! against.

use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::auth::{MemberRole, OperatorRole};
use crate::{audit, operators, AppState};

/// The API contract version `/admin/session` reports. The Dashboard refuses
/// to edit when it was built against another one.
pub const API_CONTRACT: u32 = 1;

/// Who may call a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinRole {
    /// No identity needed.
    Public,
    /// An operator with this role or a higher one (`OperatorAuth`).
    Op(OperatorRole),
    /// A member of an active tenant with one of these roles (`MemberAuth`).
    Member(&'static [MemberRole]),
    /// A gateway, by its client certificate (`DpIdentity`).
    Dp,
}

/// Every route's minimum role, as `(method, path, minimum)`.
///
/// `every_route_checks_identity` fails when a mounted route has no row, a
/// row names no mounted route, or a handler lacks the extractor its row
/// implies or the role check its row names; `tests/roles.rs` sends every
/// operator route as the role just below its minimum.
pub const ROUTE_ROLES: &[(&str, &str, MinRole)] = &[
    ("GET", "/healthz", MinRole::Public),
    ("POST", "/admin/login", MinRole::Public),
    ("POST", "/admin/logout", MinRole::Public),
    ("GET", "/admin/session", MinRole::Public),
    ("POST", "/admin/password", MinRole::Op(OperatorRole::Viewer)),
    ("GET", "/admin/operators", MinRole::Op(OperatorRole::Admin)),
    ("POST", "/admin/operators", MinRole::Op(OperatorRole::Admin)),
    (
        "PATCH",
        "/admin/operators/{id}",
        MinRole::Op(OperatorRole::Admin),
    ),
    ("GET", "/admin/audit", MinRole::Op(OperatorRole::Viewer)),
];

/// `/healthz` (no session), `/admin/*` and, as they are added, `/api/*`.
pub fn browser_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/admin/login", post(operators::login))
        .route("/admin/logout", post(operators::logout))
        .route("/admin/session", get(operators::session))
        .route("/admin/password", post(operators::change_password))
        .route(
            "/admin/operators",
            get(operators::list).post(operators::create),
        )
        .route("/admin/operators/{id}", patch(operators::update))
        .route("/admin/audit", get(audit::list))
        .with_state(state)
}

/// Liveness: the process is up and serving. It reads nothing, so a
/// database outage does not take the listener out of a load balancer.
async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{MinRole, ROUTE_ROLES};

    /// The part of `src` before its test module.
    fn production(src: &str) -> &str {
        src.split_once("\n#[cfg(test)]")
            .map_or(src, |(before, _)| before)
    }

    /// `src` without its comment lines, so a route in a doc comment does not
    /// count as mounted.
    fn without_comment_lines(src: &str) -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The index of the `)` that closes the `(` at `open`.
    fn closing_paren(s: &str, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (i, c) in s[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(open + i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Every `(METHOD, path, handler)` that a `.route(...)` call in `src`
    /// mounts.
    fn mounted_routes(src: &str) -> Vec<(String, String, String)> {
        let mut routes = Vec::new();
        for (at, _) in src.match_indices(".route(") {
            let open = at + ".route".len();
            let close = closing_paren(src, open).expect("a closed .route( call");
            let call = &src[open + 1..close];
            let path = call.split('"').nth(1).expect("a path literal").to_string();
            for verb in ["get", "post", "put", "patch", "delete"] {
                let needle = format!("{verb}(");
                for (v, _) in call.match_indices(&needle) {
                    // `get(` must not be the tail of a longer name.
                    let before = call[..v].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    let handler: String = call[v + needle.len()..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                        .collect();
                    assert!(!handler.is_empty(), "{verb} on {path} names no handler");
                    routes.push((verb.to_ascii_uppercase(), path.clone(), handler));
                }
            }
        }
        routes
    }

    /// The signature and the body of the top-level handler `handler`
    /// (`module::name`, or a bare name for this file), from the file the
    /// module path names.
    fn handler_source(handler: &str) -> (String, String) {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let (module, name) = handler.rsplit_once("::").unwrap_or(("http", handler));
        let base = module.split("::").fold(src_dir, |dir, part| dir.join(part));
        let file = [base.with_extension("rs"), base.join("mod.rs")]
            .into_iter()
            .find(|f| f.is_file())
            .unwrap_or_else(|| panic!("no source file for the handler {handler}"));
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let text = without_comment_lines(production(&text));
        let needle = format!("\npub async fn {name}(");
        let hits: Vec<usize> = text.match_indices(&needle).map(|(i, _)| i).collect();
        assert_eq!(
            hits.len(),
            1,
            "{}: the handler {handler} must be one top-level `pub async fn {name}(`",
            file.display()
        );
        let rest = &text[hits[0] + needle.len()..];
        let (signature, rest) = rest
            .split_once("{\n")
            .unwrap_or_else(|| panic!("the handler {handler} has no body"));
        let body = rest
            .split_once("\n}")
            .map_or(rest, |(body, _)| body)
            .to_string();
        (signature.to_string(), body)
    }

    /// Whether `signature` takes `extractor` itself (not an `Option` of it).
    fn takes(signature: &str, extractor: &str) -> bool {
        [",", ")", "\n"]
            .iter()
            .any(|end| signature.contains(&format!(": {extractor}{end}")))
    }

    /// Every route is in `ROUTE_ROLES`, every row is a mounted route, and
    /// each handler takes the identity its row implies and checks the role
    /// its row names.
    ///
    /// The identity check is an extractor in the handler's signature, not a
    /// router layer: a handler that forgets it still compiles and still
    /// serves. This test is what notices.
    #[test]
    fn every_route_checks_identity() {
        let routes = mounted_routes(&without_comment_lines(production(include_str!("http.rs"))));
        assert!(
            routes.len() >= 9,
            "found only {} routes: {routes:?}",
            routes.len()
        );

        for (i, (method, path, _)) in ROUTE_ROLES.iter().enumerate() {
            assert!(
                !ROUTE_ROLES[..i]
                    .iter()
                    .any(|(m, p, _)| m == method && p == path),
                "{method} {path} has two ROUTE_ROLES rows"
            );
            assert!(
                routes.iter().any(|(m, p, _)| m == method && p == path),
                "{method} {path} has a ROUTE_ROLES row but is not mounted"
            );
        }

        for (method, path, handler) in &routes {
            let Some((_, _, min)) = ROUTE_ROLES
                .iter()
                .find(|(m, p, _)| m == method && p == path)
            else {
                panic!("{method} {path} has no ROUTE_ROLES row: add one with its minimum role");
            };
            let extractor = match min {
                MinRole::Public => continue,
                MinRole::Op(_) => "OperatorAuth",
                MinRole::Member(_) => "MemberAuth",
                MinRole::Dp => "DpIdentity",
            };
            let (signature, body) = handler_source(handler);
            assert!(
                takes(&signature, extractor),
                "{method} {path}: {handler} must take {extractor}, as its ROUTE_ROLES row implies"
            );
            if let MinRole::Op(role) = min {
                let check = format!(".require(OperatorRole::{role:?})");
                assert!(
                    body.contains(&check),
                    "{method} {path}: {handler} must call {check}, the role its ROUTE_ROLES row names"
                );
            }
        }
    }
}
