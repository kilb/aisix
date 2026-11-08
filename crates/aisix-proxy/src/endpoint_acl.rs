//! The per-key endpoint allow-list, `ApiKey::allowed_endpoints`.
//!
//! A key that names the endpoint families it may call is refused on every
//! other client-facing route, with `403 permission_denied` and one fixed
//! message that names neither the route's family nor any the key may call.
//! The decision itself is [`ApiKey::can_call`]; this module maps a route to
//! its family and is called from every place a route resolves its caller:
//!
//! - the `AuthenticatedKey` extractor, once the key has resolved, for every
//!   route that authenticates through it, `/mcp/{server}/oauth/token` in the
//!   relay sign-in mode included. The refusal takes the extractor's own `Err`
//!   arm, which writes the access-log line and the request metrics;
//! - `/v1/realtime`, before the session is prepared;
//! - passthrough routes, as [`EndpointFamily::Passthrough`] whatever path or
//!   host matched the route, since a host-matched route can carry any path;
//! - the key an MCP sign-in session resolves to, at sign-in, at the token
//!   step and on every `/mcp/{server}` request that presents the session
//!   (`mcp_oauth_login::session_principal`), none of which passes through the
//!   extractor;
//! - the bound principal of anonymous MCP access and of an anonymous
//!   passthrough route, where a principal that may not call the family closes
//!   the entry: the caller is answered `401`, as by an entry no anonymous
//!   caller may use, rather than refused for an API key it never sent.
//!
//! A route's family is read off the same route template the request metrics
//! label it with ([`crate::normalize_endpoint_label`]), so the two cannot
//! disagree about which route a path is. The census in this module's tests
//! ties that map to the routes `build_router` mounts: a new route fails it
//! until it declares a family, or a reason it needs none.
//!
//! The account routes ([`ACCOUNT_ROUTES`]) are open to every key whatever
//! its list: they read the caller's own key and reach no upstream, so a key
//! confined to the model endpoints can still read its own quota.

use aisix_core::models::EndpointFamily;
use aisix_core::ApiKey;

use crate::error::ProxyError;

/// The endpoint family of a client-facing route, by its request path.
///
/// `None` for the routes that take no API key (probes, OAuth discovery and
/// the MCP sign-in pages) and for paths no typed route serves: a passthrough
/// route may own such a path, and checks [`EndpointFamily::Passthrough`]
/// itself. The extractor reads `None` as a family nothing grants, so a route
/// that takes a key and is missing here refuses a restricted key rather than
/// skipping the check.
pub(crate) fn endpoint_family(path: &str) -> Option<EndpointFamily> {
    use EndpointFamily as F;
    Some(match crate::normalize_endpoint_label(path) {
        "/v1/chat/completions" => F::ChatCompletions,
        "/v1/completions" => F::Completions,
        "/v1/embeddings" => F::Embeddings,
        "/v1/messages" => F::Messages,
        "/v1/messages/count_tokens" => F::CountTokens,
        "/v1/responses" => F::Responses,
        "/v1/models" | "/v1/models/:id" => F::Models,
        "/v1/images/generations" | "/v1/images/edits" => F::Images,
        "/v1/audio/transcriptions" | "/v1/audio/translations" | "/v1/audio/speech" => F::Audio,
        "/v1/videos" | "/v1/videos/:id" => F::Videos,
        "/v1/rerank" => F::Rerank,
        "/v1/decisions" | "/v1/systemone" => F::Decisions,
        "/v1/realtime" => F::Realtime,
        "/v1/files" | "/v1/files/:id" => F::Files,
        "/v1/batches" | "/v1/batches/:id" => F::Batches,
        "/v1/fine_tuning/jobs" | "/v1/fine_tuning/jobs/:id" => F::FineTuning,
        "/mcp"
        | "/mcp/{server}"
        | "/mcp/{server}/oauth/token"
        | "/mcp/{server}/oauth/credential" => F::Mcp,
        "/a2a" | "/a2a/{agent}/.well-known/agent-card.json" => F::A2a,
        _ => return None,
    })
}

/// The routes every key may call whatever its endpoint list, by route
/// template, each with why. The extractor consults this before
/// [`endpoint_family`].
pub(crate) const ACCOUNT_ROUTES: &[(&str, &str)] = &[
    ("/v1/key", "reads the caller's own key"),
    ("/v1/generation", "reads the caller's own request"),
];

/// Whether `path` is one of [`ACCOUNT_ROUTES`].
pub(crate) fn is_account_route(path: &str) -> bool {
    let route = crate::normalize_endpoint_label(path);
    ACCOUNT_ROUTES.iter().any(|(r, _)| *r == route)
}

/// Refuse `key` on a route of `family` unless the key may call it.
pub(crate) fn check_endpoint(key: &ApiKey, family: EndpointFamily) -> Result<(), ProxyError> {
    if key.can_call(family) {
        Ok(())
    } else {
        Err(ProxyError::EndpointForbidden(family))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail_coverage::{mounted_surfaces, FALLBACK_SURFACE};
    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ProxyConfig};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;
    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use EndpointFamily as F;

    /// Every family a route can be refused under: all but the catch-all.
    const ROUTABLE_FAMILIES: [EndpointFamily; 19] = [
        F::ChatCompletions,
        F::Completions,
        F::Embeddings,
        F::Messages,
        F::CountTokens,
        F::Responses,
        F::Models,
        F::Images,
        F::Audio,
        F::Videos,
        F::Rerank,
        F::Decisions,
        F::Realtime,
        F::Files,
        F::Batches,
        F::FineTuning,
        F::Mcp,
        F::A2a,
        F::Passthrough,
    ];

    /// What the endpoint check owes each route `build_router` mounts.
    enum Expect {
        Family(EndpointFamily),
        /// No family, and why: the route needs no key, or checks itself.
        Exempt(&'static str),
        /// No family: every key may call it ([`ACCOUNT_ROUTES`]).
        Account,
    }

    use Expect::{Account, Exempt, Family};

    const NO_KEY: &str = "answers without an API key";

    /// One row per mounted route. Checked against the routing table parsed
    /// out of `build_router`, so a new route fails the census until it
    /// declares what it is.
    const ROUTES: &[(&str, Expect)] = &[
        ("/livez", Exempt("liveness probe")),
        ("/readyz", Exempt("readiness probe")),
        ("/v1/models", Family(F::Models)),
        ("/v1/models/:id", Family(F::Models)),
        ("/v1/key", Account),
        ("/v1/generation", Account),
        ("/v1/chat/completions", Family(F::ChatCompletions)),
        ("/v1/completions", Family(F::Completions)),
        ("/v1/embeddings", Family(F::Embeddings)),
        ("/v1/images/generations", Family(F::Images)),
        ("/v1/images/edits", Family(F::Images)),
        ("/v1/messages", Family(F::Messages)),
        ("/v1/messages/count_tokens", Family(F::CountTokens)),
        ("/v1/rerank", Family(F::Rerank)),
        ("/v1/systemone", Family(F::Decisions)),
        ("/v1/decisions", Family(F::Decisions)),
        ("/v1/responses", Family(F::Responses)),
        ("/v1/audio/transcriptions", Family(F::Audio)),
        ("/v1/audio/translations", Family(F::Audio)),
        ("/v1/audio/speech", Family(F::Audio)),
        ("/v1/videos", Family(F::Videos)),
        ("/v1/videos/:id", Family(F::Videos)),
        ("/v1/videos/:id/content", Family(F::Videos)),
        ("/v1/realtime", Family(F::Realtime)),
        ("/v1/files", Family(F::Files)),
        ("/v1/files/:id", Family(F::Files)),
        ("/v1/files/:id/content", Family(F::Files)),
        ("/v1/batches", Family(F::Batches)),
        ("/v1/batches/:id", Family(F::Batches)),
        ("/v1/batches/:id/cancel", Family(F::Batches)),
        ("/v1/fine_tuning/jobs", Family(F::FineTuning)),
        ("/v1/fine_tuning/jobs/:id", Family(F::FineTuning)),
        ("/v1/fine_tuning/jobs/:id/cancel", Family(F::FineTuning)),
        ("/.well-known/oauth-protected-resource", Exempt(NO_KEY)),
        ("/.well-known/oauth-protected-resource/mcp", Exempt(NO_KEY)),
        (
            "/.well-known/oauth-protected-resource/mcp/:server",
            Exempt(NO_KEY),
        ),
        (
            "/.well-known/oauth-authorization-server/mcp/:server",
            Exempt(NO_KEY),
        ),
        (
            "/mcp/:server/.well-known/openid-configuration",
            Exempt(NO_KEY),
        ),
        ("/mcp/:server/.well-known/jwks.json", Exempt(NO_KEY)),
        ("/mcp/:server/oauth/register", Exempt(NO_KEY)),
        ("/mcp/:server/oauth/authorize", Exempt(NO_KEY)),
        // In the relay sign-in mode the token request authenticates with the
        // caller's key and redeems an upstream code for it; in the
        // interactive mode it takes no key and never reaches the check.
        ("/mcp/:server/oauth/token", Family(F::Mcp)),
        // Authenticated with the key, unlike its siblings: it removes the
        // caller's stored MCP credential, which is the MCP surface's own.
        ("/mcp/:server/oauth/credential", Family(F::Mcp)),
        ("/mcp-oauth/callback", Exempt(NO_KEY)),
        ("/mcp-oauth/login/callback", Exempt(NO_KEY)),
        ("/mcp-oauth/consent", Exempt(NO_KEY)),
        ("/mcp", Family(F::Mcp)),
        ("/mcp/", Family(F::Mcp)),
        ("/mcp/:server", Family(F::Mcp)),
        ("/a2a/:agent", Family(F::A2a)),
        ("/a2a/:agent/.well-known/agent-card.json", Family(F::A2a)),
        (
            FALLBACK_SURFACE,
            Exempt(
                "passthrough routes match on a configured prefix or Host, whatever the path, \
                 and check the `passthrough` family themselves",
            ),
        ),
    ];

    /// A concrete path for a route template: every `:param` becomes `x`.
    fn concrete(route: &str) -> String {
        route
            .split('/')
            .map(|seg| if seg.starts_with(':') { "x" } else { seg })
            .collect::<Vec<_>>()
            .join("/")
    }

    fn key_with(endpoints: Option<Vec<EndpointFamily>>) -> ApiKey {
        let mut doc = serde_json::json!({"key_hash": "h", "allowed_models": ["*"]});
        if let Some(endpoints) = endpoints {
            doc["allowed_endpoints"] = serde_json::to_value(endpoints).unwrap();
        }
        serde_json::from_value(doc).unwrap()
    }

    #[test]
    fn a_key_without_an_endpoint_list_may_call_every_family() {
        let k = key_with(None);
        assert!(ROUTABLE_FAMILIES.iter().all(|f| k.can_call(*f)));
        assert!(ROUTABLE_FAMILIES
            .iter()
            .all(|f| check_endpoint(&k, *f).is_ok()));
    }

    #[test]
    fn a_key_with_a_list_may_call_only_the_listed_families() {
        let k = key_with(Some(vec![F::ChatCompletions, F::Models]));
        assert!(check_endpoint(&k, F::ChatCompletions).is_ok());
        assert!(check_endpoint(&k, F::Models).is_ok());
        assert!(matches!(
            check_endpoint(&k, F::Images),
            Err(ProxyError::EndpointForbidden(_))
        ));
        for family in ROUTABLE_FAMILIES {
            let listed = matches!(family, F::ChatCompletions | F::Models);
            assert_eq!(check_endpoint(&k, family).is_ok(), listed, "{family:?}");
        }
    }

    #[test]
    fn an_empty_list_refuses_every_family() {
        assert!(!key_with(Some(vec![])).can_call(F::Models));
        let k = key_with(Some(vec![]));
        assert!(ROUTABLE_FAMILIES
            .iter()
            .all(|f| check_endpoint(&k, *f).is_err()));
    }

    /// A key whose list names only families this gateway does not know is
    /// refused everywhere, never left unrestricted.
    #[test]
    fn a_list_of_only_unknown_families_refuses_every_family() {
        let k: ApiKey = serde_json::from_value(serde_json::json!({
            "key_hash": "h",
            "allowed_models": ["*"],
            "allowed_endpoints": ["video_editing_v9", "speech_to_speech"],
        }))
        .unwrap();
        for family in ROUTABLE_FAMILIES {
            assert!(
                matches!(
                    check_endpoint(&k, family),
                    Err(ProxyError::EndpointForbidden(_))
                ),
                "{family:?}"
            );
        }
    }

    #[test]
    fn count_tokens_and_systemone_map_to_their_families() {
        assert_eq!(
            endpoint_family("/v1/messages/count_tokens"),
            Some(F::CountTokens)
        );
        assert_eq!(endpoint_family("/v1/messages"), Some(F::Messages));
        assert_eq!(endpoint_family("/v1/systemone"), Some(F::Decisions));
        assert_eq!(
            endpoint_family("/v1/files/file-abc/content"),
            Some(F::Files)
        );
        // A path no typed route serves names no family: a passthrough route
        // may own it, and checks itself.
        assert_eq!(endpoint_family("/passthrough/openai/v1/embeddings"), None);
        assert_eq!(endpoint_family("/definitely/not/a/route"), None);
    }

    /// Census: every route `build_router` mounts maps to the family its row
    /// names, or is exempt with a reason; and no row outlives its route.
    #[test]
    fn every_authenticated_route_declares_an_endpoint_family() {
        let mounted = mounted_surfaces();
        assert!(mounted.len() > 20, "the routing-table parse found too few");
        let declared: std::collections::BTreeSet<String> =
            ROUTES.iter().map(|(r, _)| (*r).to_string()).collect();
        let undeclared: Vec<_> = mounted.difference(&declared).collect();
        assert!(
            undeclared.is_empty(),
            "routes mounted in build_router with no endpoint family declared here: \
             {undeclared:?}. Map each in `endpoint_family` and add its row, or exempt it \
             with the reason it needs no check.",
        );
        let stale: Vec<_> = declared.difference(&mounted).collect();
        assert!(
            stale.is_empty(),
            "rows for routes no longer mounted: {stale:?}"
        );

        for (route, expect) in ROUTES {
            let path = concrete(route);
            let got = endpoint_family(&path);
            // Open to every key exactly when the row says so.
            assert_eq!(
                is_account_route(&path),
                matches!(expect, Account),
                "{route}"
            );
            match expect {
                Family(family) => assert_eq!(got, Some(*family), "{route}"),
                Exempt(reason) => {
                    assert!(
                        !reason.trim().is_empty(),
                        "{route} is exempt without a reason"
                    );
                    assert_eq!(got, None, "{route} is exempt yet maps to a family");
                }
                Account => {
                    assert_eq!(
                        got, None,
                        "{route} is an account route yet maps to a family"
                    );
                    let (_, reason) = ACCOUNT_ROUTES
                        .iter()
                        .find(|(r, _)| r == route)
                        .expect("an account route is listed in ACCOUNT_ROUTES");
                    assert!(
                        !reason.trim().is_empty(),
                        "{route} is open without a reason"
                    );
                }
            }
        }
        // And no account route outlives its row.
        for (route, _) in ACCOUNT_ROUTES {
            assert!(
                ROUTES
                    .iter()
                    .any(|(r, expect)| r == route && matches!(expect, Account)),
                "{route} is in ACCOUNT_ROUTES without an Account row"
            );
        }
    }

    /// The hand-written family list is the enum, as the write schema names
    /// it; and every family is reached somewhere: by a mounted route, or by
    /// the passthrough routes that check their own.
    #[test]
    fn every_family_is_routable_and_the_list_is_the_enum() {
        let strict = aisix_core::models::schema::resource_root_schema("api_key", true);
        let written: std::collections::BTreeSet<String> = strict
            .pointer("/definitions/EndpointFamily/enum")
            .and_then(serde_json::Value::as_array)
            .expect("the write schema closes the family enum")
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        let listed: std::collections::BTreeSet<String> = ROUTABLE_FAMILIES
            .iter()
            .map(|f| {
                serde_json::to_value(f)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(listed, written);

        let reached: std::collections::BTreeSet<EndpointFamily> = ROUTES
            .iter()
            .filter_map(|(_, expect)| match expect {
                Family(f) => Some(*f),
                Exempt(_) | Account => None,
            })
            .chain([F::Passthrough])
            .collect();
        for family in ROUTABLE_FAMILIES {
            assert!(
                reached.contains(&family),
                "{family:?} is reached by no route"
            );
        }
    }

    const ALLOWED: &str = "sk-endpoint-acl-chat-only";
    const OPEN: &str = "sk-endpoint-acl-open";
    const PK_ID: &str = "11111111-1111-1111-1111-111111111111";

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            addr: "127.0.0.1:0".into(),
            request_body_limit_bytes: 1_048_576,
            real_ip: Default::default(),
            request_id: Default::default(),
            url_rewrites: Vec::new(),
            expose_debug_headers: false,
            platform: Default::default(),
            tls: None,
            listeners: Vec::new(),
            thread_per_core: None,
            workers: None,
        }
    }

    /// A router whose one model is served by `upstream`, with the key
    /// `ALLOWED` limited to `endpoints` and the key `OPEN` unrestricted.
    fn router_with_key(upstream: &MockServer, endpoints: Vec<EndpointFamily>) -> axum::Router {
        let snap = AisixSnapshot::new();
        let pk: aisix_core::ProviderKey = serde_json::from_value(serde_json::json!({
            "display_name": "acl-openai",
            "secret": "sk-upstream",
            "api_base": format!("{}/v1", upstream.uri()),
            "provider": "openai",
            "adapter": "openai",
        }))
        .unwrap();
        snap.provider_keys.insert(ResourceEntry::new(PK_ID, pk, 1));
        let model: aisix_core::Model = serde_json::from_value(serde_json::json!({
            "display_name": "acl-model",
            "provider": "openai",
            "model_name": "text-embedding-3-small",
            "provider_key_id": PK_ID,
        }))
        .unwrap();
        snap.models.insert(ResourceEntry::new("m-acl", model, 1));
        for (id, plaintext, endpoints) in [
            ("k-allowed", ALLOWED, Some(endpoints)),
            ("k-open", OPEN, None),
        ] {
            let mut key = key_with(endpoints);
            key.key_hash = ApiKey::hash_bearer(plaintext);
            snap.apikeys.insert(ResourceEntry::new(id, key, 1));
        }
        let hub = Arc::new(aisix_gateway::Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        crate::build_router(
            crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg()).without_cache(),
        )
    }

    async fn embeddings_upstream() -> MockServer {
        let upstream = MockServer::builder().start().await;
        Mock::given(wm_method("POST"))
            .and(wm_path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"object": "embedding", "index": 0, "embedding": [0.5]}],
                "model": "text-embedding-3-small",
                "usage": {"prompt_tokens": 1, "total_tokens": 1},
            })))
            .mount(&upstream)
            .await;
        upstream
    }

    fn post(uri: &str, bearer: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    const REFUSAL: &str = "API key is not allowed to call this endpoint.";
    fn embed_body() -> serde_json::Value {
        serde_json::json!({"model": "acl-model", "input": "hello"})
    }

    #[tokio::test]
    async fn a_refused_endpoint_answers_403_before_any_upstream_call() {
        let upstream = embeddings_upstream().await;
        let router = router_with_key(&upstream, vec![F::ChatCompletions]);

        let resp = router
            .clone()
            .oneshot(post("/v1/embeddings", ALLOWED, embed_body()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = json(resp).await;
        assert_eq!(body["error"]["type"], "permission_denied");
        assert_eq!(body["error"]["message"], REFUSAL);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 0);

        // The same request on an unrestricted key reaches the upstream, so
        // the zero above is the check's doing and not a dead upstream.
        let resp = router
            .oneshot(post("/v1/embeddings", OPEN, embed_body()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }

    /// A key confined to chat may still read its own key: `GET /v1/key` is
    /// answered as the route answers without a control plane (`404`), never
    /// refused, while the same key is refused the model list.
    #[tokio::test]
    async fn the_key_route_is_open_to_a_restricted_key() {
        let upstream = embeddings_upstream().await;
        let router = router_with_key(&upstream, vec![F::ChatCompletions]);
        let get = |uri: &str| {
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", format!("Bearer {ALLOWED}"))
                .body(Body::empty())
                .unwrap()
        };

        let resp = router.clone().oneshot(get("/v1/key")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = json(resp).await;
        assert_eq!(body["error"]["code"], "not_found", "{body}");

        let resp = router.oneshot(get("/v1/models")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(json(resp).await["error"]["message"], REFUSAL);
    }

    /// A key confined to chat may still look up what its own request cost:
    /// `GET /v1/generation` is answered as the route answers without a
    /// control plane (`404`), never refused.
    #[tokio::test]
    async fn the_generation_route_is_open_to_a_restricted_key() {
        let upstream = embeddings_upstream().await;
        let router = router_with_key(&upstream, vec![F::ChatCompletions]);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/generation?id=req-1")
            .header("authorization", format!("Bearer {ALLOWED}"))
            .body(Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = json(resp).await;
        assert_eq!(body["error"]["code"], "not_found", "{body}");
    }

    /// `/v1/messages/count_tokens` is its own family, and its refusal comes
    /// in the Anthropic shape the route speaks.
    #[tokio::test]
    async fn count_tokens_is_refused_to_a_key_that_lists_only_messages() {
        let upstream = embeddings_upstream().await;
        let router = router_with_key(&upstream, vec![F::Messages]);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages/count_tokens")
            .header("x-api-key", ALLOWED)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": "acl-model",
                    "messages": [{"role": "user", "content": "hi"}],
                })
                .to_string(),
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = json(resp).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "permission_error");
        assert_eq!(body["error"]["message"], REFUSAL);
    }

    /// The refusal names neither the family refused nor any the key may
    /// call, with platform mode off and on.
    #[test]
    fn a_refusal_names_no_family() {
        let err = ProxyError::EndpointForbidden(F::Embeddings);
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.kind(), "permission_denied");
        let off = serde_json::to_value(err.envelope()).unwrap();
        let on = crate::error::tests::render_platform(&err);
        for body in [&off, &on] {
            assert_eq!(body["error"]["message"], REFUSAL, "{body}");
            assert_eq!(body["error"]["type"], "permission_denied", "{body}");
            let text = body.to_string();
            for name in ROUTABLE_FAMILIES {
                let name = serde_json::to_value(name).unwrap();
                let name = name.as_str().unwrap();
                assert!(!text.contains(name), "{name} in {text}");
            }
        }
    }
}
