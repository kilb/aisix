//! The browser listener's routes, and the role table they are checked
//! against.
//!
//! Rules for every handler, checked by the tests at the end of this file:
//! - Each route has a [`ROUTE_ROLES`] row, and its handler takes the
//!   identity extractor the row implies and calls `require` with the role
//!   it names.
//! - A request body is taken as [`ApiJson`], never as `axum::Json`, so a
//!   body that cannot be read answers in the error envelope.
//! - Routes are added with `.route(...)` only, before the router's
//!   `only_listed_routes` layer.

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRequest, MatchedPath, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::auth::{MemberRole, OperatorRole};
use crate::error::{ApiError, Detail};
use crate::{audit, operators, resources, AppState};

/// The largest request body the browser listener reads.
const MAX_BODY_BYTES: usize = 1 << 20;

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
    // Gateway resources.
    (
        "GET",
        "/admin/schemas/{resource}",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "GET",
        "/admin/resources/{kind}",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "POST",
        "/admin/resources/{kind}",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "GET",
        "/admin/resources/{kind}/{id}",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "PUT",
        "/admin/resources/{kind}/{id}",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "DELETE",
        "/admin/resources/{kind}/{id}",
        MinRole::Op(OperatorRole::Operator),
    ),
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
        .route("/admin/schemas/{resource}", get(resources::api::schema))
        .route(
            "/admin/resources/{kind}",
            get(resources::api::list).post(resources::api::create),
        )
        .route(
            "/admin/resources/{kind}/{id}",
            get(resources::api::get)
                .put(resources::api::replace)
                .delete(resources::api::delete),
        )
        .route_layer(axum::middleware::from_fn(only_listed_routes))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Answers `404 not_found` for a route with no [`ROUTE_ROLES`] row: the
/// run-time backstop for a route added in a way the census does not read.
/// A HEAD request is served by the GET handler, so it goes by the GET row.
async fn only_listed_routes(request: Request, next: Next) -> Response {
    let listed = {
        let method = if request.method() == Method::HEAD {
            "GET"
        } else {
            request.method().as_str()
        };
        request
            .extensions()
            .get::<MatchedPath>()
            .is_some_and(|path| {
                ROUTE_ROLES
                    .iter()
                    .any(|(m, p, _)| *m == method && *p == path.as_str())
            })
    };
    if listed {
        next.run(request).await
    } else {
        ApiError::not_found().into_response()
    }
}

/// A JSON request body. A body that cannot be read (not JSON, no
/// `Content-Type: application/json`, a missing or mistyped field) answers
/// `422 validation_failed`, and one over the size limit `413
/// payload_too_large`, in the error envelope. The rejection's own text is
/// not passed on: it is English and can quote the body.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<ApiJson<T>, ApiError> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(body_rejection(&rejection)),
        }
    }
}

fn body_rejection(rejection: &JsonRejection) -> ApiError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::too_large();
    }
    ApiError::validation(vec![Detail {
        path: String::new(),
        message: "请求体必须是符合接口定义的 JSON（Content-Type: application/json）".to_string(),
    }])
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
        let hits: Vec<(usize, usize)> = [
            format!("\npub async fn {name}("),
            format!("\nasync fn {name}("),
        ]
        .iter()
        .flat_map(|needle| {
            text.match_indices(needle.as_str())
                .map(|(i, n)| (i, n.len()))
        })
        .collect();
        assert_eq!(
            hits.len(),
            1,
            "{}: the handler {handler} must be one top-level `async fn {name}(`",
            file.display()
        );
        let rest = &text[hits[0].0 + hits[0].1..];
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
            let (signature, body) = handler_source(handler);
            let params = &signature[..closing_paren(&format!("({signature}"), 0)
                .expect("a closed parameter list")
                - 1];
            assert!(
                !params.replace("ApiJson<", "").contains("Json<"),
                "{method} {path}: {handler} must take its body as ApiJson, so a body it cannot \
                 read answers in the error envelope"
            );
            let extractor = match min {
                MinRole::Public => continue,
                MinRole::Op(_) => "OperatorAuth",
                MinRole::Member(_) => "MemberAuth",
                MinRole::Dp => "DpIdentity",
            };
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

    /// Routes reach the routers only through `.route(...)`, which the census
    /// reads, and every router ends its routes with the layer that refuses a
    /// route without a row at run time.
    #[test]
    fn routes_are_added_only_in_a_way_the_census_reads() {
        let src = without_comment_lines(production(include_str!("http.rs")));
        for hidden in [
            ".merge(",
            ".nest(",
            ".nest_service(",
            ".route_service(",
            ".fallback(",
        ] {
            assert!(
                !src.contains(hidden),
                "http.rs uses {hidden}, which every_route_checks_identity cannot read: \
                 extend the census first"
            );
        }
        let layer = ".route_layer(axum::middleware::from_fn(only_listed_routes))";
        let routers: Vec<&str> = src.split("Router::new()").skip(1).collect();
        assert!(!routers.is_empty(), "no router found");
        for router in routers {
            let router = router.split_once(".with_state(").map_or(router, |(r, _)| r);
            let guarded = router
                .find(layer)
                .expect("a router without the only_listed_routes layer");
            // A route layer covers only the routes added before it.
            assert!(
                router.rfind(".route(").is_some_and(|last| last < guarded),
                "a route is added after the only_listed_routes layer"
            );
        }
    }

    #[tokio::test]
    async fn a_route_without_a_row_answers_404_at_run_time() {
        use axum::routing::get;
        use axum::Router;

        let app = Router::new()
            .route("/healthz", get(|| async { "up" }))
            .route("/admin/unlisted-7c1", get(|| async { "leaked" }))
            .route_layer(axum::middleware::from_fn(super::only_listed_routes));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("an address");
        tokio::spawn(async move { axum::serve(listener, app).await });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("a client");
        let get = |path: &str| client.get(format!("http://{addr}{path}")).send();

        let listed = get("/healthz").await.expect("send");
        assert_eq!(listed.status(), 200);
        let unlisted = get("/admin/unlisted-7c1").await.expect("send");
        assert_eq!(unlisted.status(), 404);
        let body: serde_json::Value = unlisted.json().await.expect("a JSON body");
        assert_eq!(body["code"], "not_found");
        // HEAD is served by the GET handler, so it goes by the GET row.
        let head = client
            .head(format!("http://{addr}/healthz"))
            .send()
            .await
            .expect("send");
        assert_eq!(head.status(), 200);
    }
}
