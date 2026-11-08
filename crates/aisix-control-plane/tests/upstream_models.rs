//! `GET /admin/provider-keys/{id}/upstream-models`: an operator asks an
//! upstream account for the model list it serves, and the control plane
//! answers with the ids it can offer in a picker.
//!
//! Every case here talks to a real fixture server on an ephemeral port
//! ([`Fixture`]), never a mock inside the process: the thing under test is
//! the request the control plane builds — its path, its headers, its bounds —
//! and a mock could not see any of it.

mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aisix_control_plane::auth::OperatorRole;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{routing::any, Router};
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The credential every fixture account carries.
const KEY: &str = "sk-upstream-7c1-2f4b";

/// One request the fixture received.
#[derive(Debug, Clone)]
struct Hit {
    method: String,
    path: String,
    query: String,
    headers: HeaderMap,
}

/// What the fixture answers: a status, a body and how long to wait first.
#[derive(Clone)]
struct Answer {
    status: StatusCode,
    body: Vec<u8>,
    delay: Duration,
    location: Option<String>,
}

impl Answer {
    fn new(status: StatusCode, body: impl Into<Vec<u8>>) -> Answer {
        Answer {
            status,
            body: body.into(),
            delay: Duration::ZERO,
            location: None,
        }
    }

    fn slow(mut self, delay: Duration) -> Answer {
        self.delay = delay;
        self
    }
}

#[derive(Clone)]
struct FixtureState {
    hits: Arc<Mutex<Vec<Hit>>>,
    answer: Arc<dyn Fn() -> Answer + Send + Sync>,
}

/// A real HTTP server on an ephemeral port, answering whatever [`Answer`]
/// says and recording every request it received.
struct Fixture {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<Hit>>>,
    stop: tokio::sync::watch::Sender<bool>,
    served: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(answer: impl Fn() -> Answer + Send + Sync + 'static) -> Fixture {
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let state = FixtureState {
            hits: hits.clone(),
            answer: Arc::new(answer),
        };
        let router = Router::new().fallback(any(fixture)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fixture");
        let addr = listener.local_addr().expect("the fixture's address");
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let served = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stopped.wait_for(|stop| *stop).await;
                })
                .await;
        });
        Fixture {
            addr,
            hits,
            stop,
            served,
        }
    }

    /// `http://127.0.0.1:<port>`.
    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// How many requests reached it.
    fn hits(&self) -> usize {
        self.hits.lock().expect("the fixture's hits").len()
    }

    /// The last request it received.
    fn last(&self) -> Hit {
        self.hits
            .lock()
            .expect("the fixture's hits")
            .last()
            .expect("the fixture was reached")
            .clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.served.abort();
    }
}

async fn fixture(
    State(state): State<FixtureState>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    _body: Bytes,
) -> Response {
    state.hits.lock().expect("the fixture's hits").push(Hit {
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().unwrap_or_default().to_string(),
        headers,
    });
    let answer = (state.answer)();
    if !answer.delay.is_zero() {
        tokio::time::sleep(answer.delay).await;
    }
    let mut response = (answer.status, answer.body).into_response();
    if let Some(location) = answer.location {
        response.headers_mut().insert(
            axum::http::header::LOCATION,
            location.parse().expect("an ASCII Location"),
        );
    }
    response
}

// ---------------------------------------------------------------------------
// An account on the fixture.
// ---------------------------------------------------------------------------

/// A provider key pointing at `fixture`, made as `admin` with `extra` written
/// over its document; its id.
async fn account(cp: &TestCp, admin: &OperatorSession, fixture: &Fixture, extra: Value) -> Uuid {
    let mut doc = json!({
        "display_name": format!("acct-{}", Uuid::new_v4().simple()),
        "provider": "openai",
        "adapter": "openai",
        "api_key": KEY,
        "api_base": fixture.base(),
        "groups": ["shared"],
    });
    for (k, v) in extra.as_object().expect("an object") {
        doc[k] = v.clone();
    }
    let r = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({ "doc": doc })),
        )
        .await;
    assert_eq!(r.status, 201, "create the account: {r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid")
}

/// `GET /admin/provider-keys/{id}/upstream-models` as the caller of `who`.
async fn list(cp: &TestCp, who: &OperatorSession, id: Uuid) -> Resp {
    cp.send(
        "GET",
        &format!("/admin/provider-keys/{id}/upstream-models"),
        Some(who.cookie()),
        None,
    )
    .await
}

/// `method path` as `admin`, with `If-Match: "<version>"` and a JSON body.
async fn put_at(
    cp: &TestCp,
    admin: &OperatorSession,
    path: &str,
    version: i64,
    body: Value,
) -> Resp {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let response = client
        .put(cp.url(path))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&body)
        .send()
        .await
        .expect("send the request");
    let status = response.status().as_u16();
    let text = response.text().await.expect("read the body");
    Resp {
        status,
        body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
        set_cookie: None,
        headers: reqwest::header::HeaderMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Success, per adapter.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_openai_account_lists_its_models_by_bearer() {
    let fixture = Fixture::start(|| {
        Answer::new(
            StatusCode::OK,
            r#"{"object":"list","data":[{"id":"up-b-2"},{"id":"up-a-1"},{"id":"up-a-1"}]}"#,
        )
    })
    .await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(&cp, &admin, &fixture, json!({})).await;

    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["items"],
        json!([{"id": "up-a-1"}, {"id": "up-b-2"}]),
        "sorted and deduped"
    );
    assert_eq!(r.body["truncated"], false);
    assert_eq!(r.body["warnings"], json!([]));

    let hit = fixture.last();
    assert_eq!(hit.method, "GET");
    assert_eq!(hit.path, "/models");
    assert_eq!(
        hit.headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(
        hit.headers.get("x-api-key").is_none(),
        "the OpenAI form sends no x-api-key: {hit:?}"
    );
}

#[tokio::test]
async fn an_anthropic_account_lists_its_models_by_api_key() {
    let fixture = Fixture::start(|| {
        Answer::new(
            StatusCode::OK,
            r#"{"data":[{"id":"claude-x"}],"has_more":false}"#,
        )
    })
    .await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(
        &cp,
        &admin,
        &fixture,
        json!({"provider": "anthropic", "adapter": "anthropic"}),
    )
    .await;

    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["items"], json!([{"id": "claude-x"}]));

    let hit = fixture.last();
    assert_eq!(hit.path, "/v1/models");
    assert_eq!(hit.query, "limit=1000");
    assert_eq!(
        hit.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
        Some(KEY)
    );
    assert_eq!(
        hit.headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {KEY}").as_str()),
        "the documented curl's bearer form rides beside x-api-key"
    );
    assert_eq!(
        hit.headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some("2023-06-01")
    );
}

/// `adapter` is optional, so a document naming only the vendor is a real one.
/// The wire, the path and the header set all have to follow the vendor then,
/// or an Anthropic account answers its own URL with OpenAI-only headers.
#[tokio::test]
async fn a_vendor_with_no_adapter_is_still_listed_its_own_way() {
    let fixture = Fixture::start(|| {
        Answer::new(
            StatusCode::OK,
            r#"{"object":"list","data":[{"id":"same-host"}]}"#,
        )
    })
    .await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;

    // The OpenAI vendor, no adapter: `GET /models` by bearer.
    let id = account(&cp, &admin, &fixture, json!({"adapter": null})).await;
    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 200, "openai without an adapter: {r:?}");
    let hit = fixture.last();
    assert_eq!(hit.path, "/models");
    assert_eq!(
        hit.headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(hit.headers.get("x-api-key").is_none(), "{hit:?}");

    // The Anthropic vendor, no adapter: `GET /v1/models` with every
    // Anthropic header. This is the half a second copy of the form table
    // would get wrong, since 200 here is also what a wrong URL answers.
    let id = account(
        &cp,
        &admin,
        &fixture,
        json!({"provider": "anthropic", "adapter": null}),
    )
    .await;
    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 200, "anthropic without an adapter: {r:?}");
    let hit = fixture.last();
    assert_eq!(hit.path, "/v1/models");
    assert_eq!(hit.query, "limit=1000");
    assert_eq!(
        hit.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
        Some(KEY),
        "the Anthropic header set follows the vendor, not the missing adapter"
    );
    assert_eq!(
        hit.headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some("2023-06-01")
    );
}

/// The Anthropic wire asks for one page and does not page. An account with
/// more models than fit must not be reported as a complete list.
#[tokio::test]
async fn an_anthropic_page_with_more_says_so() {
    let fixture = Fixture::start(|| {
        Answer::new(
            StatusCode::OK,
            r#"{"data":[{"id":"claude-x"}],"has_more":true,"first_id":"claude-x","last_id":"claude-x"}"#,
        )
    })
    .await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(
        &cp,
        &admin,
        &fixture,
        json!({"provider": "anthropic", "adapter": "anthropic"}),
    )
    .await;

    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["items"], json!([{"id": "claude-x"}]));
    assert_eq!(r.body["truncated"], true, "has_more was ignored");
    let warnings = r.body["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

// ---------------------------------------------------------------------------
// The cache.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_call_within_the_minute_does_not_dial() {
    let fixture =
        Fixture::start(|| Answer::new(StatusCode::OK, r#"{"data":[{"id":"up-1"}]}"#)).await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(&cp, &admin, &fixture, json!({})).await;

    assert_eq!(list(&cp, &admin, id).await.status, 200);
    assert_eq!(list(&cp, &admin, id).await.status, 200);
    assert_eq!(fixture.hits(), 1, "the second answer came from the cache");
}

#[tokio::test]
async fn editing_the_account_misses_the_cache() {
    let fixture =
        Fixture::start(|| Answer::new(StatusCode::OK, r#"{"data":[{"id":"up-1"}]}"#)).await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(&cp, &admin, &fixture, json!({})).await;
    assert_eq!(list(&cp, &admin, id).await.status, 200);

    // A new `api_base` on the same row: the version moves, the cache misses.
    let path = format!("/admin/resources/provider_keys/{id}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    let version = current.body["version"].as_i64().expect("a version");
    let mut doc = current.body["doc"].clone();
    doc["api_base"] = json!(fixture.base());
    let r = put_at(&cp, &admin, &path, version, json!({"doc": doc})).await;
    assert_eq!(r.status, 200, "update the account: {r:?}");

    assert_eq!(list(&cp, &admin, id).await.status, 200);
    assert_eq!(fixture.hits(), 2, "the edit reached the upstream again");
}

// ---------------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unsupported_adapter_never_dials() {
    let fixture = Fixture::start(|| Answer::new(StatusCode::OK, r#"{"data":[]}"#)).await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(
        &cp,
        &admin,
        &fixture,
        json!({"provider": "aws", "adapter": "bedrock"}),
    )
    .await;

    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "upstream_models_unsupported");
    assert_eq!(fixture.hits(), 0, "an unsupported adapter must not dial");
}

#[tokio::test]
async fn the_error_text_is_scrubbed_and_500_characters_at_most() {
    let tail = "y".repeat(2_000);
    let fixture = Fixture::start(move || {
        Answer::new(
            StatusCode::UNAUTHORIZED,
            format!(
                r#"{{"error":{{"message":"Incorrect API key provided: {KEY}. \
                   Another key sk-abcdefghijklmnopqrstuvwxyz was used. {tail}"}}}}"#
            ),
        )
    })
    .await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(&cp, &admin, &fixture, json!({})).await;

    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 502, "{r:?}");
    assert_eq!(r.body["code"], "upstream_bad_response");
    let detail = r.body["details"][0]["message"]
        .as_str()
        .expect("a scrubbed detail");
    assert!(
        !detail.contains(KEY),
        "the key reached the caller: {detail}"
    );
    assert!(
        !detail.contains("sk-"),
        "a key shape reached the caller: {detail}"
    );
    assert!(
        detail.chars().count() <= 500,
        "the detail is {} chars",
        detail.chars().count()
    );
    assert!(
        detail.contains("Incorrect API key provided"),
        "the upstream's own words survive: {detail}"
    );
    assert!(
        !detail.contains("401"),
        "the status the upstream chose reached the caller: {detail}"
    );
    assert!(
        !r.body.to_string().contains("401"),
        "the answer carries the upstream's status: {:?}",
        r.body
    );
}

#[tokio::test]
async fn a_slow_upstream_times_out_at_the_routes_bound() {
    let fixture = Fixture::start(|| {
        Answer::new(StatusCode::OK, r#"{"data":[]}"#).slow(Duration::from_secs(8))
    })
    .await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(&cp, &admin, &fixture, json!({})).await;

    let started = Instant::now();
    let r = list(&cp, &admin, id).await;
    let took = started.elapsed();
    assert_eq!(r.status, 504, "{r:?}");
    assert_eq!(r.body["code"], "upstream_timeout");
    assert!(took < Duration::from_secs(7), "answered after {took:?}");
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let second =
        Fixture::start(|| Answer::new(StatusCode::OK, r#"{"data":[{"id":"leak"}]}"#)).await;
    let first = {
        let target = format!("{}/models", second.base());
        Fixture::start(move || {
            let mut answer = Answer::new(StatusCode::FOUND, "");
            answer.location = Some(target.clone());
            answer
        })
        .await
    };
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(&cp, &admin, &first, json!({})).await;

    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 502, "{r:?}");
    assert_eq!(r.body["code"], "upstream_bad_response");
    assert_eq!(second.hits(), 0, "the redirect target was reached");
}

#[tokio::test]
async fn a_loopback_or_link_local_target_is_refused() {
    let fixture = Fixture::start(|| Answer::new(StatusCode::OK, r#"{"data":[]}"#)).await;
    // The production policy: what an operator gets with no configuration.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;

    for base in [
        format!("http://{}/v1", fixture.addr),
        "http://169.254.169.254/latest".to_string(),
    ] {
        let id = account(&cp, &admin, &fixture, json!({"api_base": base.clone()})).await;
        let r = list(&cp, &admin, id).await;
        assert_eq!(r.status, 502, "{base}: {r:?}");
        assert_eq!(r.body["code"], "upstream_unreachable", "{base}");
        let detail = r.body["details"][0]["message"].as_str().unwrap_or_default();
        assert!(
            detail.contains("outbound.ssrf_protection.allowed_hosts"),
            "{base}: the hint names what to change: {detail}"
        );
    }
    assert_eq!(fixture.hits(), 0, "no blocked target was dialed");
}

#[tokio::test]
async fn a_body_over_one_mebibyte_is_refused() {
    let fixture = Fixture::start(|| {
        let big = "z".repeat(2 << 20);
        Answer::new(StatusCode::OK, format!(r#"{{"data":[{{"id":"{big}"}}]}}"#))
    })
    .await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(&cp, &admin, &fixture, json!({})).await;

    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 502, "{r:?}");
    assert_eq!(r.body["code"], "upstream_bad_response");
}

#[tokio::test]
async fn the_answer_never_carries_the_credential_or_the_upstream_status() {
    let fixture =
        Fixture::start(|| Answer::new(StatusCode::OK, r#"{"data":[{"id":"up-1"}]}"#)).await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = account(&cp, &admin, &fixture, json!({})).await;

    let r = list(&cp, &admin, id).await;
    assert_eq!(r.status, 200, "{r:?}");
    let mut keys: Vec<&str> = r
        .body
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["fetched_at", "items", "truncated", "warnings"]);
    assert!(!r.body.to_string().contains(KEY), "{:?}", r.body);
}

#[tokio::test]
async fn a_missing_account_is_404_and_never_dials() {
    let fixture = Fixture::start(|| Answer::new(StatusCode::OK, r#"{"data":[]}"#)).await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let r = list(&cp, &admin, Uuid::new_v4()).await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(fixture.hits(), 0);
}

#[tokio::test]
async fn who_may_ask() {
    let fixture =
        Fixture::start(|| Answer::new(StatusCode::OK, r#"{"data":[{"id":"up-1"}]}"#)).await;
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let id = account(&cp, &admin, &fixture, json!({})).await;

    assert_eq!(
        list(&cp, &operator, id).await.status,
        200,
        "Operator may ask"
    );

    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let r = list(&cp, &viewer, id).await;
    assert_eq!(r.status, 403, "Viewer may not: {r:?}");
    assert_eq!(r.body["code"], "forbidden");
}
