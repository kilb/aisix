//! #451, held by behaviour instead of by discipline.
//!
//! `Upstream::model` is documented "MUST be bounded", and every emit site
//! honours that today by passing either a name that already resolved against
//! the snapshot or a `usage_attr::metric_model_label` sentinel. But the
//! chokepoint is collapse-only on purpose — `metric_model_label_pair` returns
//! an unresolvable name verbatim — so the invariant lives entirely in the
//! call sites. There are more than twenty of them across ten handlers, and
//! nothing fails when a new one passes the raw `model` field through.
//!
//! Prometheus never reclaims a series. A caller who can mint one label per
//! request can grow the gateway's own memory and every scrape it feeds until
//! one or the other falls over, without exceeding a single rate limit — the
//! request is REJECTED each time, which is exactly why the error paths are
//! the ones that matter here.
//!
//! So: drive an attacker-shaped name at every endpoint that takes one and
//! read `/metrics`. The canary must appear in no label, on any of them.
//!
//! What mutation testing showed while this was written, worth knowing before
//! trusting a green run: for a WILDCARD-served name the two collapses —
//! `metric_model_label` at the handler and `metric_model_label_pair` inside
//! `request_metrics::record` — are redundant, and each masks the other. Break
//! either one alone and this test still passes, because the survivor folds
//! the name back to the row. Break both and both canaries leak. That
//! redundancy is a good property, not a gap; the point of writing it down is
//! that a green run here does NOT mean every collapse is still wired, only
//! that at least one is. The unresolvable canary has no such second net:
//! removing `metric_model_label` alone fails this test on its own.

use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig};
use aisix_gateway::Hub;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

/// A caller-minted model name that resolves to NOTHING. Distinctive enough
/// that a substring search over the whole exposition cannot match anything
/// else.
const CANARY: &str = "zz-canary-9f2b1e-unbounded";

/// A caller-minted name the wildcard row DOES serve. The interesting half:
/// resolution succeeds, the request dispatches, and BOTH label halves —
/// `model` and `upstream_model` — are derived from this string, so the
/// collapse to the row's own `(display_name, model_name)` is the only thing
/// between a caller and a fresh series per request. `CANARY` never reaches
/// that code path, because it matches no row at all.
const WILD_CANARY: &str = "openai/zz-canary-9f2b1e-served";

/// sha256("sk-canary-caller")
const CALLER_HASH: &str = "78a15ec738637626cda26ac621605f1220d7dbe72e3e8c8bc2b4532048b080a1";

fn snapshot() -> AisixSnapshot {
    let snap = AisixSnapshot::new();
    let pk: ProviderKey = serde_json::from_str(
        r#"{"display_name":"pk","secret":"sk-up","api_base":"http://127.0.0.1:1/v1","provider":"openai","adapter":"openai"}"#,
    )
    .unwrap();
    snap.provider_keys.insert(ResourceEntry::new("pk-1", pk, 1));
    let m: Model = serde_json::from_str(
        r#"{"display_name":"real-model","provider":"openai","model_name":"gpt-4o","provider_key_id":"pk-1"}"#,
    )
    .unwrap();
    snap.models.insert(ResourceEntry::new("m-1", m, 1));
    // A wildcard row too: it is the one shape whose resolution SUCCEEDS on a
    // caller-minted name, so without it the run would only exercise the
    // "nothing resolved" half of the guard.
    let w: Model = serde_json::from_str(
        r#"{"display_name":"openai/*","provider":"openai","model_name":"*","provider_key_id":"pk-1"}"#,
    )
    .unwrap();
    snap.models.insert(ResourceEntry::new("m-w", w, 1));
    let k: ApiKey = serde_json::from_str(&format!(
        r#"{{"key_hash":"{CALLER_HASH}","allowed_models":["*"]}}"#
    ))
    .unwrap();
    snap.apikeys.insert(ResourceEntry::new("k-1", k, 1));
    snap
}

fn cfg() -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1:0".into(),
        request_body_limit_bytes: Some(1_048_576),
        real_ip: Default::default(),
        request_id: Default::default(),
        url_rewrites: Vec::new(),
        tls: None,
        thread_per_core: None,
        workers: None,
    }
}

/// Every model-addressed route, with a body that names the canary. The
/// requests are expected to FAIL — an unresolvable model is a 404, and the
/// wildcard-served one cannot reach its unroutable `api_base` — because the
/// pre-dispatch error paths are where a raw name would be emitted.
fn requests() -> Vec<(&'static str, &'static str, serde_json::Value)> {
    let mut out = requests_for(CANARY);
    out.extend(requests_for(WILD_CANARY));
    out
}

fn requests_for(model: &'static str) -> Vec<(&'static str, &'static str, serde_json::Value)> {
    let msgs = serde_json::json!([{"role": "user", "content": "hi"}]);
    vec![
        (
            "/v1/chat/completions",
            "POST",
            serde_json::json!({"model": model, "messages": msgs}),
        ),
        (
            "/v1/messages",
            "POST",
            serde_json::json!({"model": model, "max_tokens": 16, "messages": msgs}),
        ),
        (
            "/v1/messages/count_tokens",
            "POST",
            serde_json::json!({"model": model, "messages": msgs}),
        ),
        (
            "/v1/responses",
            "POST",
            serde_json::json!({"model": model, "input": "hi"}),
        ),
        (
            "/v1/completions",
            "POST",
            serde_json::json!({"model": model, "prompt": "hi"}),
        ),
        (
            "/v1/embeddings",
            "POST",
            serde_json::json!({"model": model, "input": "hi"}),
        ),
        (
            "/v1/rerank",
            "POST",
            serde_json::json!({"model": model, "query": "hi", "documents": ["a"]}),
        ),
        (
            "/v1/images/generations",
            "POST",
            serde_json::json!({"model": model, "prompt": "hi"}),
        ),
        (
            "/v1/videos",
            "POST",
            serde_json::json!({"model": model, "prompt": "hi"}),
        ),
        (
            "/v1/audio/speech",
            "POST",
            serde_json::json!({"model": model, "input": "hi", "voice": "alloy"}),
        ),
        (
            "/v1/batches",
            "POST",
            serde_json::json!({
                "model": model,
                "input_file_id": "file-x",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            }),
        ),
        (
            "/v1/fine_tuning/jobs",
            "POST",
            serde_json::json!({"model": model, "training_file": "file-x"}),
        ),
    ]
}

#[tokio::test]
async fn no_endpoint_lets_a_caller_mint_a_metric_label() {
    let hub = Arc::new(Hub::new());
    let state =
        aisix_proxy::ProxyState::new(SnapshotHandle::new(snapshot()), hub, &cfg()).without_cache();
    let metrics = state.metrics.clone();
    let app = aisix_proxy::build_router(state);

    let mut drove = 0usize;
    for (path, method, body) in requests() {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer sk-canary-caller")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        // A path that is not mounted 404s at the ROUTER with an EMPTY body,
        // never reaching a handler — it would prove nothing, and would let
        // this guard rot silently the day a route is renamed. A handler's own
        // "no such model" 404 carries an error envelope, which is exactly the
        // pre-dispatch path being checked.
        assert!(
            !(status == StatusCode::NOT_FOUND && body.is_empty()),
            "{path} is not mounted under {method} — fix the request, \
             otherwise this endpoint is not actually being checked"
        );
        drove += 1;
    }
    assert_eq!(drove, requests().len());
    assert_eq!(
        drove,
        2 * requests_for(CANARY).len(),
        "both canaries must run"
    );

    let rendered = metrics.render();
    let offenders: Vec<&str> = rendered
        .lines()
        .filter(|l| l.contains(CANARY) || l.contains(WILD_CANARY))
        .take(5)
        .collect();
    assert!(
        offenders.is_empty(),
        "a caller-supplied model name reached a metric label — every request \
         with a fresh name mints a series Prometheus never reclaims \
         (#451):\n{}",
        offenders.join("\n")
    );

    // "The canary is absent" is also satisfied by an endpoint that emitted
    // NOTHING — a request shape this test got wrong, or a route that started
    // rejecting earlier than the emit. Require each one to show up in the
    // exposition under its own endpoint label, so the coverage is measured
    // rather than assumed.
    let mut unmeasured: Vec<&str> = Vec::new();
    for (path, _, _) in requests_for(CANARY) {
        let label = format!(r#"endpoint="{}""#, endpoint_label(path));
        if !rendered
            .lines()
            .any(|l| l.starts_with("aisix_proxy_requests_total{") && l.contains(&label))
        {
            unmeasured.push(path);
        }
    }
    assert!(
        unmeasured.is_empty(),
        "these endpoints emitted no request metric, so the canary search \
         above proved nothing about them:\n  {}\n\nfull exposition:\n{rendered}",
        unmeasured.join("\n  "),
    );
}

/// The bounded route template `normalize_endpoint_label` collapses each path
/// to. Written out rather than imported: a guard that reads its expected
/// values from the code under test cannot catch that code being wrong.
fn endpoint_label(path: &str) -> &str {
    match path {
        "/v1/fine_tuning/jobs" => "/v1/fine_tuning/jobs",
        other => other,
    }
}
