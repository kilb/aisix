//! The routes of both listeners, and the role table they are checked
//! against: the browser listener's ([`browser_router`]) and the gateway
//! listener's ([`dp_router`]).
//!
//! Rules for every handler, checked by the tests at the end of this file:
//! - Each route has a [`ROUTE_ROLES`] row, and its handler takes the
//!   identity extractor the row implies and calls `require` with the role
//!   it names.
//! - A request body is taken as [`ApiJson`], never as `axum::Json`, so a
//!   body that cannot be read answers in the error envelope.
//! - Routes are added with `.route(...)` only, before the router's listing
//!   layer: `only_listed_routes` on the browser router, which admits the
//!   rows of every role but [`MinRole::Dp`], and `only_listed_dp_routes` on
//!   the gateway router, which admits only those. Neither listener serves
//!   the other's routes.

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRequest, MatchedPath, Request};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, patch, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::auth::{MemberRole, OperatorRole};
use crate::error::{ApiError, Detail};
use crate::{
    audit, dp, gateways, keys, members, operators, projector, resources, rollout, tenants, usage,
    AppState,
};

/// The largest request body either listener reads.
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
    /// A member with one of these roles (`MemberAuth`), of an active tenant
    /// unless the route is one of [`INACTIVE_TENANT_ROUTES`].
    Member(&'static [MemberRole]),
    /// A gateway, by its client certificate (`DpIdentity`). Only the
    /// gateway listener serves these rows.
    Dp,
}

/// Every route's minimum role, as `(method, path, minimum)`. The method
/// `ANY` stands for every method.
///
/// `every_route_checks_identity` fails when a mounted route has no row, a
/// row names no mounted route, or a handler lacks the extractor its row
/// implies or the role check its row names; `tests/roles.rs` sends every
/// operator route as the role just below its minimum, and every member
/// route as each role its row does not name.
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
    (
        "GET",
        "/admin/environment",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "GET",
        "/admin/gateways/credentials",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "POST",
        "/admin/gateways/credentials",
        MinRole::Op(OperatorRole::Admin),
    ),
    (
        "POST",
        "/admin/gateways/credentials/{dp_id}/revoke",
        MinRole::Op(OperatorRole::Admin),
    ),
    (
        "GET",
        "/admin/gateways/instances",
        MinRole::Op(OperatorRole::Viewer),
    ),
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
    // The kine projection, and how far the gateways have it.
    ("GET", "/admin/rollout", MinRole::Op(OperatorRole::Viewer)),
    (
        "POST",
        "/admin/rollout/reconcile",
        MinRole::Op(OperatorRole::Admin),
    ),
    (
        "POST",
        "/admin/rollout/ownership",
        MinRole::Op(OperatorRole::Admin),
    ),
    // Tenants, their defaults, and the provider groups they can be given.
    ("GET", "/admin/tenants", MinRole::Op(OperatorRole::Viewer)),
    (
        "POST",
        "/admin/tenants",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "GET",
        "/admin/tenants/{id}",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "PATCH",
        "/admin/tenants/{id}",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "POST",
        "/admin/tenants/{id}/approve",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "POST",
        "/admin/tenants/{id}/suspend",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "POST",
        "/admin/tenants/{id}/reactivate",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "GET",
        "/admin/settings/tenant-defaults",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "PUT",
        "/admin/settings/tenant-defaults",
        MinRole::Op(OperatorRole::Admin),
    ),
    (
        "GET",
        "/admin/provider-groups",
        MinRole::Op(OperatorRole::Viewer),
    ),
    // Tenant members, as operators see them.
    (
        "GET",
        "/admin/tenants/{id}/members",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "POST",
        "/admin/tenants/{id}/invites",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "PATCH",
        "/admin/members/{id}",
        MinRole::Op(OperatorRole::Operator),
    ),
    // Tenant keys, as operators see them.
    (
        "GET",
        "/admin/tenants/{id}/keys",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "POST",
        "/admin/tenants/{id}/keys",
        MinRole::Op(OperatorRole::Operator),
    ),
    ("GET", "/admin/keys/{id}", MinRole::Op(OperatorRole::Viewer)),
    (
        "PATCH",
        "/admin/keys/{id}",
        MinRole::Op(OperatorRole::Operator),
    ),
    (
        "DELETE",
        "/admin/keys/{id}",
        MinRole::Op(OperatorRole::Operator),
    ),
    // The tenant API.
    ("POST", "/api/signup", MinRole::Public),
    ("POST", "/api/login", MinRole::Public),
    ("POST", "/api/logout", MinRole::Public),
    ("GET", "/api/session", MinRole::Public),
    ("POST", "/api/invites/accept", MinRole::Public),
    ("POST", "/api/password", MinRole::Member(EVERY_MEMBER)),
    ("GET", "/api/org", MinRole::Member(EVERY_MEMBER)),
    ("GET", "/api/models", MinRole::Member(EVERY_MEMBER)),
    ("GET", "/api/members", MinRole::Member(MANAGERS)),
    ("GET", "/api/members/invites", MinRole::Member(MANAGERS)),
    ("POST", "/api/members/invites", MinRole::Member(MANAGERS)),
    (
        "DELETE",
        "/api/members/invites/{id}",
        MinRole::Member(MANAGERS),
    ),
    ("PATCH", "/api/members/{id}", MinRole::Member(MANAGERS)),
    ("DELETE", "/api/members/{id}", MinRole::Member(MANAGERS)),
    ("GET", "/api/keys", MinRole::Member(KEY_HOLDERS)),
    ("POST", "/api/keys", MinRole::Member(KEY_HOLDERS)),
    ("GET", "/api/keys/{id}", MinRole::Member(KEY_HOLDERS)),
    ("PATCH", "/api/keys/{id}", MinRole::Member(KEY_HOLDERS)),
    ("DELETE", "/api/keys/{id}", MinRole::Member(KEY_HOLDERS)),
    // Stored usage.
    (
        "GET",
        "/admin/usage/events",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "GET",
        "/admin/usage/batches",
        MinRole::Op(OperatorRole::Viewer),
    ),
    (
        "GET",
        "/admin/usage/rejects",
        MinRole::Op(OperatorRole::Viewer),
    ),
    // Gateways, on the gateway listener.
    ("POST", "/dp/heartbeat", MinRole::Dp),
    ("ANY", "/dp/store/v1/{*rest}", MinRole::Dp),
    ("POST", "/dp/telemetry", MinRole::Dp),
];

/// Every tenant role.
const EVERY_MEMBER: &[MemberRole] = &[
    MemberRole::Owner,
    MemberRole::Admin,
    MemberRole::Member,
    MemberRole::Billing,
];

/// The tenant roles that manage members.
const MANAGERS: &[MemberRole] = &[MemberRole::Owner, MemberRole::Admin];

/// The tenant roles that hold keys: a member their own, the managers every
/// key of the tenant.
const KEY_HOLDERS: &[MemberRole] = &[MemberRole::Owner, MemberRole::Admin, MemberRole::Member];

/// The member routes a member of a tenant that is not active (pending
/// approval, or suspended) may still call, to see the tenant's state and
/// keep their own account; on every other member route they get `403
/// tenant_not_active`. `every_route_checks_identity` holds each handler to
/// this list.
pub const INACTIVE_TENANT_ROUTES: &[(&str, &str)] =
    &[("POST", "/api/password"), ("GET", "/api/org")];

/// `/healthz` (no session), `/admin/*` and `/api/*`.
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
        .route("/admin/environment", get(gateways::environment))
        .route(
            "/admin/gateways/credentials",
            get(gateways::list_credentials).post(gateways::issue_credential),
        )
        .route(
            "/admin/gateways/credentials/{dp_id}/revoke",
            post(gateways::revoke_credential),
        )
        .route("/admin/gateways/instances", get(gateways::list_instances))
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
        .route("/admin/rollout", get(rollout::summary))
        .route("/admin/rollout/reconcile", post(projector::post_reconcile))
        .route("/admin/rollout/ownership", post(projector::post_ownership))
        .route(
            "/admin/tenants",
            get(tenants::list).post(tenants::create_tenant),
        )
        .route(
            "/admin/tenants/{id}",
            get(tenants::get).patch(tenants::update),
        )
        .route("/admin/tenants/{id}/approve", post(tenants::approve))
        .route("/admin/tenants/{id}/suspend", post(tenants::suspend))
        .route("/admin/tenants/{id}/reactivate", post(tenants::reactivate))
        .route(
            "/admin/settings/tenant-defaults",
            get(tenants::get_defaults).put(tenants::put_defaults),
        )
        .route("/admin/provider-groups", get(tenants::provider_groups))
        .route("/admin/tenants/{id}/members", get(members::admin_list))
        .route("/admin/tenants/{id}/invites", post(members::admin_invite))
        .route("/admin/members/{id}", patch(members::admin_update))
        .route(
            "/admin/tenants/{id}/keys",
            get(keys::admin_list).post(keys::admin_create),
        )
        .route(
            "/admin/keys/{id}",
            get(keys::admin_get)
                .patch(keys::admin_update)
                .delete(keys::admin_revoke),
        )
        .route("/api/signup", post(members::signup))
        .route("/api/login", post(members::login))
        .route("/api/logout", post(members::logout))
        .route("/api/session", get(members::session))
        .route("/api/invites/accept", post(members::accept_invite))
        .route("/api/password", post(members::change_password))
        .route("/api/org", get(members::org))
        .route("/api/models", get(members::models))
        .route("/api/members", get(members::list))
        .route(
            "/api/members/invites",
            get(members::list_invites).post(members::invite),
        )
        .route("/api/members/invites/{id}", delete(members::revoke_invite))
        .route(
            "/api/members/{id}",
            patch(members::update).delete(members::remove),
        )
        .route("/api/keys", get(keys::list).post(keys::create))
        .route(
            "/api/keys/{id}",
            get(keys::get).patch(keys::update).delete(keys::revoke),
        )
        .route("/admin/usage/events", get(usage::list_events))
        .route("/admin/usage/batches", get(usage::list_batches))
        .route("/admin/usage/rejects", get(usage::list_rejects))
        .route_layer(axum::middleware::from_fn(only_listed_routes))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// `/dp/*`, for gateways: served on the gateway listener, which attaches
/// each connection's certificate identity to its requests.
pub fn dp_router(state: AppState) -> Router {
    Router::new()
        .route("/dp/heartbeat", post(dp::heartbeat::heartbeat))
        .route("/dp/store/v1/{*rest}", any(dp::store::not_found))
        // A batch may be larger than the other routes' bodies, and every
        // answer, the body limit's included, says batches are deduplicated.
        .route(
            "/dp/telemetry",
            post(dp::telemetry::telemetry)
                .layer(DefaultBodyLimit::max(dp::telemetry::MAX_BODY_BYTES))
                .layer(axum::middleware::map_response(dp::telemetry::mark_dedup)),
        )
        .route_layer(axum::middleware::from_fn(only_listed_dp_routes))
        .layer(axum::middleware::map_response(close_on_forbidden))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Every `403` on the gateway listener ends its connection: a credential it
/// refuses (revoked, expired, of another environment, speaking for another)
/// gets one answer, and then holds no connection open.
async fn close_on_forbidden(mut response: Response) -> Response {
    if response.status() == StatusCode::FORBIDDEN {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// Answers `404 not_found` on the browser listener for a route with no
/// [`ROUTE_ROLES`] row, or a gateway's row: the run-time backstop for a
/// route added in a way the census does not read.
async fn only_listed_routes(request: Request, next: Next) -> Response {
    if is_listed(&request, false) {
        next.run(request).await
    } else {
        ApiError::not_found().into_response()
    }
}

/// [`only_listed_routes`] for the gateway listener, which serves only the
/// gateways' rows.
async fn only_listed_dp_routes(request: Request, next: Next) -> Response {
    if is_listed(&request, true) {
        next.run(request).await
    } else {
        ApiError::not_found().into_response()
    }
}

/// Whether the request's route has a [`ROUTE_ROLES`] row, a gateway's row
/// when `dp` is set and any other otherwise. A HEAD request is served by
/// the GET handler, so it goes by the GET row.
fn is_listed(request: &Request, dp: bool) -> bool {
    let method = if request.method() == Method::HEAD {
        "GET"
    } else {
        request.method().as_str()
    };
    request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|path| {
            ROUTE_ROLES.iter().any(|(m, p, min)| {
                (*m == method || *m == "ANY")
                    && *p == path.as_str()
                    && matches!(min, MinRole::Dp) == dp
            })
        })
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

    use super::{MinRole, INACTIVE_TENANT_ROUTES, ROUTE_ROLES};

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
            for verb in ["get", "post", "put", "patch", "delete", "any"] {
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
            if let MinRole::Member(roles) = min {
                // Compared without whitespace, so rustfmt may wrap the list.
                let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
                let names: Vec<String> =
                    roles.iter().map(|r| format!("MemberRole::{r:?}")).collect();
                let check = format!(".require(&[{}])", names.join(","));
                assert!(
                    compact.replace(",])", "])").contains(&check),
                    "{method} {path}: {handler} must call {check}, the roles its ROUTE_ROLES row names"
                );
                let open = INACTIVE_TENANT_ROUTES.contains(&(method.as_str(), path.as_str()));
                assert_eq!(
                    body.contains(".require_active()"),
                    !open,
                    "{method} {path}: {handler} must call .require_active() unless the route is in \
                     INACTIVE_TENANT_ROUTES, and only then"
                );
            }
        }
        for (method, path) in INACTIVE_TENANT_ROUTES {
            assert!(
                ROUTE_ROLES.iter().any(|(m, p, min)| m == method
                    && p == path
                    && matches!(min, MinRole::Member(_))),
                "{method} {path} is in INACTIVE_TENANT_ROUTES but is no member route"
            );
        }
    }

    /// Routes reach the routers only through `.route(...)`, which the census
    /// reads; every router ends its routes with one of the layers that
    /// refuse a route without a row at run time; and each route is on the
    /// listener its row is for, the gateways' rows on the router with the
    /// gateway layer and every other row on the one with the browser layer.
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
        // Each layer, and whether it is the gateway listener's.
        let layers = [
            (
                ".route_layer(axum::middleware::from_fn(only_listed_routes))",
                false,
            ),
            (
                ".route_layer(axum::middleware::from_fn(only_listed_dp_routes))",
                true,
            ),
        ];
        let routers: Vec<&str> = src.split("Router::new()").skip(1).collect();
        assert!(routers.len() >= 2, "found {} routers", routers.len());
        let mut kinds = Vec::new();
        for router in routers {
            let router = router.split_once(".with_state(").map_or(router, |(r, _)| r);
            let found: Vec<(usize, bool)> = layers
                .iter()
                .filter_map(|(layer, dp)| router.find(layer).map(|at| (at, *dp)))
                .collect();
            let [(guarded, dp)] = found[..] else {
                panic!("a router must end its routes with exactly one listing layer: {found:?}");
            };
            // A route layer covers only the routes added before it.
            assert!(
                router.rfind(".route(").is_some_and(|last| last < guarded),
                "a route is added after the listing layer"
            );
            for (method, path, _) in mounted_routes(router) {
                let Some((_, _, min)) = ROUTE_ROLES
                    .iter()
                    .find(|(m, p, _)| *m == method && *p == path)
                else {
                    // every_route_checks_identity reports it.
                    continue;
                };
                assert_eq!(
                    matches!(min, MinRole::Dp),
                    dp,
                    "{method} {path} is mounted on the {} router, but its row is for the other \
                     listener",
                    if dp { "gateway" } else { "browser" }
                );
            }
            kinds.push(dp);
        }
        assert!(
            kinds.contains(&true) && kinds.contains(&false),
            "both listeners have a router: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn each_listener_admits_only_its_own_rows() {
        use axum::routing::{get, post};
        use axum::Router;

        // Rows of the other listener, mounted where they do not belong.
        let browser = Router::new()
            .route("/healthz", get(|| async { "up" }))
            .route("/dp/heartbeat", post(|| async { "leaked" }))
            .route_layer(axum::middleware::from_fn(super::only_listed_routes));
        let gateway = Router::new()
            .route(
                "/dp/store/v1/{*rest}",
                axum::routing::any(|| async { "store" }),
            )
            .route("/admin/session", get(|| async { "leaked" }))
            .route_layer(axum::middleware::from_fn(super::only_listed_dp_routes));
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("a client");
        let mut addrs = Vec::new();
        for app in [browser, gateway] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            addrs.push(listener.local_addr().expect("an address"));
            tokio::spawn(async move { axum::serve(listener, app).await });
        }
        let status = |addr, method: &str, path: &str| {
            let method = reqwest::Method::from_bytes(method.as_bytes()).expect("a method");
            let request = client.request(method, format!("http://{addr}{path}"));
            async move { request.send().await.expect("send").status().as_u16() }
        };
        assert_eq!(status(addrs[0], "GET", "/healthz").await, 200);
        assert_eq!(status(addrs[0], "POST", "/dp/heartbeat").await, 404);
        // `ANY` admits every method.
        for method in ["GET", "PUT", "DELETE", "POST", "HEAD"] {
            assert_eq!(
                status(addrs[1], method, "/dp/store/v1/ns/k").await,
                200,
                "{method}"
            );
        }
        assert_eq!(status(addrs[1], "GET", "/admin/session").await, 404);
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
