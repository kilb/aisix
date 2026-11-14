//! 原生无状态端点组件测试，真实路由与本地 HTTP；不声称提供商端到端。
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn app(base: &str) -> axum::Router {
    configured_app(base, |_| {}).0
}

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";
const KEY: &str = "33333333-3333-4333-8333-333333333333";
const BODY_MODEL: &str = "44444444-4444-4444-8444-444444444444";
const ENDPOINTS: [(&str, &str); 3] = [
    ("/v1/moderations", "moderations"),
    ("/v1/content_provenance_checks", "content_provenance_checks"),
    ("/v1/images/variations", "images"),
];

fn configured_app(
    base: &str,
    configure: impl FnOnce(&AisixSnapshot),
) -> (
    axum::Router,
    SnapshotHandle<AisixSnapshot>,
    Receiver<UsageEvent>,
) {
    let (router, snapshot, events, _) = configured_app_with_metrics(base, configure);
    (router, snapshot, events)
}

fn configured_app_with_metrics(
    base: &str,
    configure: impl FnOnce(&AisixSnapshot),
) -> (
    axum::Router,
    SnapshotHandle<AisixSnapshot>,
    Receiver<UsageEvent>,
    Arc<aisix_obs::Metrics>,
) {
    let snapshot = AisixSnapshot::new();
    let provider:ProviderKey=serde_json::from_value(json!({"display_name":"native","secret":"upstream-key","provider":"openai","adapter":"openai","api_base":base})).unwrap();
    snapshot.provider_keys.insert(ResourceEntry::new(
        "22222222-2222-4222-8222-222222222222",
        provider,
        1,
    ));
    let model:Model=serde_json::from_value(json!({"display_name":"native-model","provider":"openai","model_name":"provider-model","provider_key_id":"22222222-2222-4222-8222-222222222222"})).unwrap();
    snapshot.models.insert(ResourceEntry::new(
        "11111111-1111-4111-8111-111111111111",
        model,
        1,
    ));
    let key: ApiKey = serde_json::from_value(
        json!({"key_hash":ApiKey::hash_bearer("caller"),"allowed_models":["native-model"]}),
    )
    .unwrap();
    snapshot.apikeys.insert(ResourceEntry::new(
        "33333333-3333-4333-8333-333333333333",
        key,
        1,
    ));
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .unwrap();
    configure(&snapshot);
    let snapshot = SnapshotHandle::new(snapshot);
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let state = ProxyState::new(snapshot.clone(), Arc::new(Hub::new()), &config)
        .without_cache()
        .with_guardrail_index(guardrails)
        .with_usage_sink(UsageSink::new(tx));
    let metrics = state.metrics.clone();
    (build_router(state), snapshot, rx, metrics)
}
async fn response_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1048576).await.unwrap()).unwrap()
}
#[tokio::test]
async fn moderation_preserves_multimodal_input_extensions_and_native_reply() {
    let upstream = MockServer::builder().start().await;
    let answer = json!({"id":"modr-real-shape","model":"omni-moderation-latest","results":[{"flagged":false,"categories":{"violence":false},"category_scores":{"violence":0.01},"category_applied_input_types":{"violence":["text","image"]}}],"future":17});
    Mock::given(method("POST"))
        .and(path("/v1/moderations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
        .expect(1)
        .mount(&upstream)
        .await;
    let input = json!({"model":"native-model","input":[{"type":"text","text":"inspect"},{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}],"future":{"keep":true}});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/moderations?x=a%2Fb&x=two")
        .header("authorization", "Bearer caller")
        .header("content-type", "application/json")
        .body(Body::from(input.to_string()))
        .unwrap();
    let response = app(&upstream.uri()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await, answer);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["input"], input["input"]);
    assert_eq!(sent["future"], input["future"]);
    assert_eq!(sent["model"], "provider-model");
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer upstream-key"
    );
    assert_eq!(requests[0].url.query(), Some("x=a%2Fb&x=two"));
}
#[tokio::test]
async fn model_less_moderation_needs_explicit_selector_without_inventing_upstream_model() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/moderations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"modr","model":"omni-moderation-latest","results":[]})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let request = || {
        Request::builder()
            .method("POST")
            .uri("/v1/moderations")
            .header("authorization", "Bearer caller")
            .header("content-type", "application/json")
    };
    let rejected = app(&upstream.uri())
        .oneshot(
            request()
                .body(Body::from(r#"{"input":"inspect"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let accepted = app(&upstream.uri())
        .oneshot(
            request()
                .header("x-aisix-model", "native-model")
                .body(Body::from(r#"{"input":"inspect"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(sent.get("model").is_none());
}
#[tokio::test]
async fn provenance_preserves_multipart_bytes_filename_query_and_native_result() {
    let upstream = MockServer::builder().start().await;
    let answer = json!({"id":"cpc","object":"content_provenance_check","results":[{"signal":"c2pa","status":"not_detected"}],"future":true});
    Mock::given(method("POST"))
        .and(path("/v1/content_provenance_checks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
        .expect(1)
        .mount(&upstream)
        .await;
    let body=b"--n\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\nPNG\x00\xffsecret-looking-sk-test\r\n--n--\r\n";
    let req = Request::builder()
        .method("POST")
        .uri("/v1/content_provenance_checks?model=native-model&beta=true")
        .header("authorization", "Bearer caller")
        .header("content-type", "multipart/form-data; boundary=n")
        .body(Body::from(body.as_slice()))
        .unwrap();
    let response = app(&upstream.uri()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await, answer);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests[0].url.query(), Some("beta=true"));
    assert!(requests[0]
        .body
        .windows(b"PNG\x00\xffsecret-looking-sk-test".len())
        .any(|w| w == b"PNG\x00\xffsecret-looking-sk-test"));
    assert!(String::from_utf8_lossy(&requests[0].body).contains("filename=\"image.png\""));
}
#[tokio::test]
async fn variations_is_a_native_legacy_relay_and_preserves_upstream_rejection() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/v1/images/variations")).respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"legacy image variations is unavailable","type":"invalid_request_error","code":"model_not_found"}}))).expect(1).mount(&upstream).await;
    let body="--n\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nnative-model\r\n--n\r\nContent-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n--n--\r\n";
    let req = Request::builder()
        .method("POST")
        .uri("/v1/images/variations")
        .header("authorization", "Bearer caller")
        .header("content-type", "multipart/form-data; boundary=n")
        .body(Body::from(body))
        .unwrap();
    let response = app(&upstream.uri()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value = response_json(response).await;
    assert_eq!(value["error"]["code"], "model_not_found");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

fn edit_key(snapshot: &AisixSnapshot, edit: impl FnOnce(&mut ApiKey)) {
    let entry = snapshot.apikeys.get_by_id(KEY).unwrap();
    let mut key = (*entry.value).clone();
    edit(&mut key);
    snapshot
        .apikeys
        .insert(ResourceEntry::new(KEY, key, entry.revision + 1));
}

fn edit_model(snapshot: &AisixSnapshot, edit: impl FnOnce(&mut Model)) {
    let entry = snapshot.models.get_by_id(MODEL).unwrap();
    let mut model = (*entry.value).clone();
    edit(&mut model);
    snapshot
        .models
        .insert(ResourceEntry::new(MODEL, model, entry.revision + 1));
}

fn edit_provider(snapshot: &AisixSnapshot, edit: impl FnOnce(&mut ProviderKey)) {
    let entry = snapshot.provider_keys.get_by_id(PROVIDER).unwrap();
    let mut provider = (*entry.value).clone();
    edit(&mut provider);
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, entry.revision + 1));
}

fn native_request(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", "Bearer caller")
        .extension(ConnectInfo(SocketAddr::from(([192, 0, 2, 7], 9000))));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if uri.split('?').next() == Some("/v1/moderations") {
        request
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"input":[],"future":{"keep":true}}).to_string(),
            ))
            .unwrap()
    } else {
        let field = if uri.starts_with("/v1/images/variations") {
            "image"
        } else {
            "file"
        };
        let mut body = format!(
            "--native-test\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(b"PNG\x00\xffnative-file-marker");
        body.extend_from_slice(b"\r\n--native-test--\r\n");
        request
            .header("content-type", "multipart/form-data; boundary=native-test")
            .body(Body::from(body))
            .unwrap()
    }
}

// 应答形状来自官方 SDK；本地协议服务不承诺已退役端点仍接受真实请求。
fn native_answer(endpoint: &str) -> Value {
    match endpoint {
        "/v1/moderations" => {
            json!({"id":"modr_native","model":"omni-moderation-latest","results":[]})
        }
        "/v1/content_provenance_checks" => json!({
            "created_at":1730000000,"object":"content_provenance_check",
            "results":[{"type":"c2pa","outcome":"not_detected","validation_state":"not_present"}]
        }),
        "/v1/images/variations" => {
            json!({"created":1730000000,"data":[{"b64_json":"bmF0aXZlLWltYWdl"}]})
        }
        _ => panic!("unexpected test endpoint"),
    }
}

async fn mount_native_answer(upstream: &MockServer, endpoint: &str, answer: Value) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer))
        .expect(1)
        .mount(upstream)
        .await;
}

fn attach_unconditional_block(snapshot: &AisixSnapshot, hook: &str) {
    let script = match hook {
        "input" => {
            "export function checkInput() { return { action: 'block', reason_code: 'blocked' }; }"
        }
        "output" => {
            "export function checkOutput() { return { action: 'block', reason_code: 'blocked' }; }"
        }
        _ => panic!("unexpected guardrail hook"),
    };
    attach_script(snapshot, hook, script, false);
}

fn attach_script(snapshot: &AisixSnapshot, hook: &str, script: &str, fail_open: bool) {
    let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
        "name":"block-all","enabled":true,"kind":"custom","hook_point":hook,
        "fail_open":fail_open,"timeout_ms":5000,"script":script
    }))
    .unwrap();
    snapshot
        .guardrails
        .insert(ResourceEntry::new("native-stateless-guard", guardrail, 1));
    let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
        "guardrail_id":"native-stateless-guard","scope_type":"env","priority":100
    }))
    .unwrap();
    snapshot.guardrail_attachments.insert(ResourceEntry::new(
        "native-stateless-attachment",
        attachment,
        1,
    ));
}

async fn next_event(events: &mut Receiver<UsageEvent>) -> UsageEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
}

fn assert_request_metrics(metrics: &aisix_obs::Metrics, endpoint: &str, statuses: &[u16]) {
    let scrape = metrics.render();
    let rows: Vec<_> = scrape
        .lines()
        .filter(|line| line.starts_with("aisix_proxy_requests_total{"))
        .collect();
    assert_eq!(rows.len(), statuses.len(), "{scrape}");
    for status in statuses {
        let matching: Vec<_> = rows
            .iter()
            .filter(|line| {
                let (series, _) = line.rsplit_once(' ').unwrap();
                let (_, labels) = series.split_once('{').unwrap();
                labels
                    .trim_end_matches('}')
                    .split(',')
                    .any(|label| label == format!("status=\"{status}\""))
            })
            .collect();
        assert_eq!(matching.len(), 1, "{scrape}");
        let (series, count) = matching[0].rsplit_once(' ').unwrap();
        let (_, labels) = series.split_once('{').unwrap();
        let labels: Vec<_> = labels.trim_end_matches('}').split(',').collect();
        assert!(
            labels.contains(&format!("endpoint=\"{endpoint}\"").as_str()),
            "{scrape}"
        );
        assert!(labels.contains(&"inbound_protocol=\"openai\""), "{scrape}");
        assert_eq!(count, "1", "{scrape}");
    }
}

#[tokio::test]
async fn every_stateless_endpoint_runs_unconditional_input_guardrail_without_text() {
    for (endpoint, _) in ENDPOINTS {
        let upstream = MockServer::builder().start().await;
        let (router, _, mut events) = configured_app(&upstream.uri(), |snapshot| {
            attach_unconditional_block(snapshot, "input");
        });
        let response = router
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{endpoint}"
        );
        assert_eq!(
            response_json(response).await["error"]["type"],
            "content_filter"
        );
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "{endpoint}"
        );
        let event = next_event(&mut events).await;
        assert!(event.guardrail_blocked, "{endpoint}");
        assert_eq!(event.upstream_dispatched, Some(false), "{endpoint}");
        assert_eq!(event.status_code, 422, "{endpoint}");
        assert_eq!(
            (
                event.prompt_tokens,
                event.completion_tokens,
                event.total_tokens
            ),
            (0, 0, 0)
        );
        assert_eq!(event.cost_usd, 0.0);
        assert!(!event.guardrail_enforced_hits.is_empty(), "{endpoint}");
        assert!(!event.applied_guardrails.is_empty(), "{endpoint}");
        assert!(
            events.try_recv().is_err(),
            "one terminal event per request: {endpoint}"
        );
    }
}

#[tokio::test]
async fn every_stateless_endpoint_retains_served_usage_when_output_guardrail_blocks() {
    for (endpoint, _) in ENDPOINTS {
        let upstream = MockServer::builder().start().await;
        let mut answer = native_answer(endpoint);
        // 兼容上游扩展的合成计数，仅验证已派发应答不会因输出拒绝丢失计费。
        answer["usage"] = json!({
            "prompt_tokens":37,"completion_tokens":5,"total_tokens":42,
            "prompt_tokens_details":{"cached_tokens":7}
        });
        answer["future"] = json!("upstream-content-must-not-be-released");
        mount_native_answer(&upstream, endpoint, answer).await;
        let (router, _, mut events) = configured_app(&upstream.uri(), |snapshot| {
            attach_unconditional_block(snapshot, "output");
            edit_model(snapshot, |model| {
                let mut value = serde_json::to_value(&*model).unwrap();
                value["cost"] = json!({
                    "input_per_1k":2.0,"output_per_1k":4.0,"cached_input_per_1k":1.0
                });
                *model = serde_json::from_value(value).unwrap();
            });
        });
        let response = router
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{endpoint}"
        );
        let error = response_json(response).await;
        assert_eq!(error["error"]["type"], "content_filter");
        assert!(!error
            .to_string()
            .contains("upstream-content-must-not-be-released"));
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "{endpoint}"
        );
        let event = next_event(&mut events).await;
        assert_eq!(event.upstream_dispatched, Some(true), "{endpoint}");
        assert!(event.guardrail_blocked, "{endpoint}");
        assert_eq!(event.status_code, 422, "{endpoint}");
        assert_eq!(
            (
                event.prompt_tokens,
                event.completion_tokens,
                event.total_tokens
            ),
            (37, 5, 42)
        );
        assert_eq!(event.cached_prompt_tokens, 7);
        assert!(
            (event.cost_usd - 0.087).abs() < 1e-9,
            "{endpoint}: {}",
            event.cost_usd
        );
        assert_eq!(event.provider_key_id.as_deref(), Some(PROVIDER));
        assert_eq!(event.model_id, MODEL);
        assert!(!event.guardrail_enforced_hits.is_empty());
        assert!(!event.applied_guardrails.is_empty());
        assert!(
            events.try_recv().is_err(),
            "one terminal event per request: {endpoint}"
        );
    }
}

#[tokio::test]
async fn every_stateless_endpoint_enforces_its_current_endpoint_acl_before_upstream() {
    for (endpoint, family) in ENDPOINTS {
        let upstream = MockServer::builder().start().await;
        mount_native_answer(&upstream, endpoint, native_answer(endpoint)).await;
        let (router, snapshot, mut events, metrics) =
            configured_app_with_metrics(&upstream.uri(), |snapshot| {
                edit_key(snapshot, |key| {
                    key.allowed_endpoints = Some(serde_json::from_value(json!([family])).unwrap());
                });
            });
        let allowed = router
            .clone()
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK, "{endpoint}");
        response_json(allowed).await;
        assert_eq!(
            next_event(&mut events).await.upstream_dispatched,
            Some(true)
        );

        edit_key(&snapshot.load(), |key| {
            key.allowed_endpoints =
                Some(serde_json::from_value(json!(["chat_completions"])).unwrap());
        });
        let denied = router
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{endpoint}");
        assert_eq!(
            response_json(denied).await["error"]["type"],
            "permission_denied"
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "{endpoint}"
        );
        assert_request_metrics(&metrics, endpoint, &[200, 403]);
        assert!(
            matches!(
                events.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ),
            "early ACL rejection must not emit a usage event: {endpoint}"
        );
    }
}

#[tokio::test]
async fn every_stateless_endpoint_honors_current_model_access() {
    for (endpoint, _) in ENDPOINTS {
        let upstream = MockServer::builder().start().await;
        mount_native_answer(&upstream, endpoint, native_answer(endpoint)).await;
        let (router, snapshot, mut events) = configured_app(&upstream.uri(), |_| {});
        let allowed = router
            .clone()
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        response_json(allowed).await;
        next_event(&mut events).await;

        edit_key(&snapshot.load(), |key| key.allowed_models.clear());
        let denied = router
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{endpoint}");
        assert_eq!(
            response_json(denied).await["error"]["type"],
            "permission_denied"
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "{endpoint}"
        );
        assert_eq!(
            next_event(&mut events).await.upstream_dispatched,
            Some(false)
        );
    }
}

#[tokio::test]
async fn every_stateless_endpoint_honors_current_provider_group_restrictions() {
    for (endpoint, _) in ENDPOINTS {
        let upstream = MockServer::builder().start().await;
        mount_native_answer(&upstream, endpoint, native_answer(endpoint)).await;
        let (router, snapshot, mut events) = configured_app(&upstream.uri(), |snapshot| {
            edit_key(snapshot, |key| {
                key.allowed_provider_groups = vec!["tenant-group".into()]
            });
            edit_provider(snapshot, |provider| {
                provider.groups = vec!["tenant-group".into()]
            });
        });
        let allowed = router
            .clone()
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        response_json(allowed).await;
        next_event(&mut events).await;

        edit_provider(&snapshot.load(), |provider| {
            provider.groups = vec!["other-group".into()]
        });
        let denied = router
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(
            denied.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{endpoint}"
        );
        let error = response_json(denied).await;
        assert!(!error.to_string().contains("other-group"));
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "{endpoint}"
        );
        assert_eq!(
            next_event(&mut events).await.upstream_dispatched,
            Some(false)
        );
    }
}

#[tokio::test]
async fn every_stateless_endpoint_honors_current_ip_allowlist_and_ignores_untrusted_forwarding() {
    for (endpoint, _) in ENDPOINTS {
        let upstream = MockServer::builder().start().await;
        mount_native_answer(&upstream, endpoint, native_answer(endpoint)).await;
        let (router, snapshot, mut events) = configured_app(&upstream.uri(), |snapshot| {
            edit_model(snapshot, |model| {
                model.allowed_cidrs = Some(vec!["192.0.2.0/24".into()])
            });
        });
        let allowed = router
            .clone()
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        response_json(allowed).await;
        next_event(&mut events).await;

        edit_model(&snapshot.load(), |model| {
            model.allowed_cidrs = Some(vec!["198.51.100.0/24".into()])
        });
        let denied = router
            .oneshot(native_request(
                endpoint,
                &[
                    ("x-aisix-model", "native-model"),
                    ("x-forwarded-for", "198.51.100.7"),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{endpoint}");
        assert_eq!(
            response_json(denied).await["error"]["code"],
            "ip_restricted"
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "{endpoint}"
        );
        let event = next_event(&mut events).await;
        assert_eq!(event.upstream_dispatched, Some(false));
        assert_eq!(event.client_source_ip, "192.0.2.7");
    }
}

#[tokio::test]
async fn stateless_account_headers_must_match_current_configured_account() {
    for (endpoint, _) in ENDPOINTS {
        let upstream = MockServer::builder().start().await;
        mount_native_answer(&upstream, endpoint, native_answer(endpoint)).await;
        let (router, snapshot, mut events) = configured_app(&upstream.uri(), |snapshot| {
            edit_provider(snapshot, |provider| {
                provider.request = Some(serde_json::from_value(json!({
                    "default_headers":{"openai-organization":"org-configured","openai-project":"proj-configured"}
                })).unwrap());
            });
        });
        let allowed = router
            .clone()
            .oneshot(native_request(
                endpoint,
                &[
                    ("x-aisix-model", "native-model"),
                    ("openai-organization", "org-configured"),
                    ("openai-project", "proj-configured"),
                ],
            ))
            .await
            .unwrap();
        let status = allowed.status();
        let answer = response_json(allowed).await;
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{endpoint}: body={answer}; received={requests:?}"
        );
        assert_eq!(answer, native_answer(endpoint));
        next_event(&mut events).await;
        assert_eq!(requests[0].headers["openai-organization"], "org-configured");
        assert_eq!(requests[0].headers["openai-project"], "proj-configured");
        assert_eq!(requests[0].headers["authorization"], "Bearer upstream-key");
        for name in ["openai-organization", "openai-project"] {
            let denied = router
                .clone()
                .oneshot(native_request(
                    endpoint,
                    &[("x-aisix-model", "native-model"), (name, "other-account")],
                ))
                .await
                .unwrap();
            assert_eq!(
                denied.status(),
                StatusCode::BAD_REQUEST,
                "{endpoint}: {name}"
            );
            assert_eq!(
                response_json(denied).await["error"]["type"],
                "invalid_request_error"
            );
            assert_eq!(
                next_event(&mut events).await.upstream_dispatched,
                Some(false)
            );
        }
        // 配置撤回后，原先匹配的账号头也不能沿用旧配置。
        edit_provider(&snapshot.load(), |provider| provider.request = None);
        let denied = router
            .oneshot(native_request(
                endpoint,
                &[
                    ("x-aisix-model", "native-model"),
                    ("openai-project", "proj-configured"),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::BAD_REQUEST, "{endpoint}");
        response_json(denied).await;
        assert_eq!(
            next_event(&mut events).await.upstream_dispatched,
            Some(false)
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn model_less_header_and_query_selectors_do_not_change_native_payloads() {
    for (endpoint, _) in ENDPOINTS {
        for header in [false, true] {
            let upstream = MockServer::builder().start().await;
            mount_native_answer(&upstream, endpoint, native_answer(endpoint)).await;
            let router = app(&upstream.uri());
            let rejected = router
                .clone()
                .oneshot(native_request(endpoint, &[]))
                .await
                .unwrap();
            assert_eq!(rejected.status(), StatusCode::BAD_REQUEST, "{endpoint}");
            assert!(response_json(rejected).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("model"));
            assert!(upstream.received_requests().await.unwrap().is_empty());
            let uri = if header {
                format!("{endpoint}?x=a%2Fb&x=two")
            } else {
                format!("{endpoint}?x=a%2Fb&model=native-model&x=two")
            };
            let headers = if header {
                vec![("x-aisix-model", "native-model")]
            } else {
                vec![]
            };
            let response = router
                .oneshot(native_request(&uri, &headers))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{endpoint}, header={header}"
            );
            assert_eq!(response_json(response).await, native_answer(endpoint));
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url.query(), Some("x=a%2Fb&x=two"));
            assert!(requests[0].headers.get("x-aisix-model").is_none());
            if endpoint == "/v1/moderations" {
                let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
                assert!(sent.get("model").is_none());
                assert_eq!(sent["input"], json!([]));
                assert_eq!(sent["future"], json!({"keep":true}));
            } else {
                let sent = String::from_utf8_lossy(&requests[0].body);
                assert!(!sent.contains("name=\"model\""));
                assert!(sent.contains("filename=\"image.png\""));
                assert!(requests[0]
                    .body
                    .windows(b"PNG\x00\xffnative-file-marker".len())
                    .any(|part| part == b"PNG\x00\xffnative-file-marker"));
            }
        }
    }
}

#[tokio::test]
async fn malformed_moderation_json_returns_native_error_without_upstream_dispatch() {
    for payload in [
        "{",
        "{\"input\":]",
        "{\"model\":\"native-model\",\"input\":\"x\"} trailing",
    ] {
        let upstream = MockServer::builder().start().await;
        let (router, _, mut events, metrics) = configured_app_with_metrics(&upstream.uri(), |_| {});
        let mut request = native_request("/v1/moderations", &[("x-aisix-model", "native-model")]);
        *request.body_mut() = Body::from(payload);
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{payload}");
        let value = response_json(response).await;
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(!value["error"]["message"].as_str().unwrap().is_empty());
        assert!(upstream.received_requests().await.unwrap().is_empty());
        assert_request_metrics(&metrics, "/v1/moderations", &[400]);
        assert!(
            matches!(
                events.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ),
            "malformed JSON must not emit a usage event"
        );
    }
}

#[tokio::test]
async fn stateless_relays_preserve_upstream_rate_limit_headers_outside_platform_mode() {
    let headers = [
        ("x-ratelimit-limit-requests", "9999"),
        ("x-ratelimit-remaining-requests", "9876"),
        ("x-ratelimit-reset-requests", "1s"),
        ("x-ratelimit-account-sentinel", "shared-account"),
    ];
    for (endpoint, _) in ENDPOINTS {
        let upstream = MockServer::builder().start().await;
        let answer = native_answer(endpoint);
        let mut template = ResponseTemplate::new(200).set_body_json(answer.clone());
        for (name, value) in headers {
            template = template.insert_header(name, value);
        }
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(template)
            .expect(1)
            .mount(&upstream)
            .await;
        // 此夹具未开启平台模式，调用方应继续读到上游的原始限流头。
        let response = app(&upstream.uri())
            .oneshot(native_request(
                endpoint,
                &[("x-aisix-model", "native-model")],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{endpoint}");
        for (name, expected) in headers {
            assert_eq!(
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok()),
                Some(expected),
                "{endpoint}: {name}"
            );
        }
        assert_eq!(response_json(response).await, answer);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
}

fn add_full_content_exporter(snapshot: &AisixSnapshot, collector: &MockServer) {
    let exporter: aisix_core::ObservabilityExporter = serde_json::from_value(json!({
        "name":"stateless-full-content","enabled":true,"kind":"otlp_http",
        "endpoint":format!("{}/v1/traces", collector.uri()),"headers":{},"content_mode":"full"
    }))
    .unwrap();
    snapshot.observability_exporters.insert(ResourceEntry::new(
        "stateless-content-exporter",
        exporter,
        1,
    ));
}

async fn exported_request(collector: &MockServer, request_id: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let exported: String = collector
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body).into_owned())
            .collect();
        if exported.contains(request_id) && exported.contains("gen_ai.operation.name") {
            return exported;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "request {request_id} never reached the exporter: {exported}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn stateless_upstream_failures_export_screened_requests_and_safe_binary_summaries() {
    const PROMPT: &str = "stateless-captured-input-marker-f521";
    const UPSTREAM_ERROR: &str = "stateless-upstream-rejected-marker-a983";
    let binary_hash = format!("{:x}", Sha256::digest(b"PNG\x00\xffnative-file-marker"));
    for (endpoint, _) in ENDPOINTS {
        for (script, input_exported) in [
            (None, true),
            (
                Some("export function checkInput() { return { action: 'none' }; }"),
                true,
            ),
            (
                Some("export function checkInput() { throw new Error('failed-open'); }"),
                false,
            ),
        ] {
            let upstream = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .and(path(endpoint))
                .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                    "error":{"message":UPSTREAM_ERROR,"type":"invalid_request_error","code":"invalid_value"}
                })))
                .expect(1)
                .mount(&upstream)
                .await;
            let collector = MockServer::builder().start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&collector)
                .await;
            let (router, _, _) = configured_app(&upstream.uri(), |snapshot| {
                add_full_content_exporter(snapshot, &collector);
                if let Some(script) = script {
                    attach_script(snapshot, "input", script, true);
                }
            });
            let mut request = native_request(endpoint, &[("x-aisix-model", "native-model")]);
            if endpoint == "/v1/moderations" {
                *request.body_mut() = Body::from(json!({"input":PROMPT}).to_string());
            }
            let response = router.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let request_id = response.headers()["x-aisix-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let answer = response_json(response).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{endpoint}: {answer}");
            assert!(answer["error"]["message"]
                .as_str()
                .unwrap()
                .contains(UPSTREAM_ERROR));
            assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
            let exported = exported_request(&collector, &request_id).await;
            assert!(exported.contains(UPSTREAM_ERROR), "{endpoint}: {exported}");
            if endpoint == "/v1/moderations" {
                assert_eq!(
                    exported.contains(PROMPT),
                    input_exported,
                    "{endpoint}: {exported}"
                );
            } else {
                let field = if endpoint == "/v1/images/variations" {
                    "image_sha256"
                } else {
                    "file_sha256"
                };
                assert_eq!(
                    exported.contains(field),
                    input_exported,
                    "{endpoint}: {exported}"
                );
                assert_eq!(
                    exported.contains(&binary_hash),
                    input_exported,
                    "{endpoint}: {exported}"
                );
                assert!(
                    !exported.contains("native-file-marker"),
                    "{endpoint}: {exported}"
                );
                assert!(!exported.contains("image.png"), "{endpoint}: {exported}");
            }
        }
    }
}

#[tokio::test]
async fn moderation_pii_mask_rewrites_every_official_text_shape_without_changing_control_fields() {
    let image = json!({"type":"image_url","image_url":{"url":"https://example.com/image.png"}});
    for (shape, input, masked, escaped) in [
        (
            "string",
            json!("release version: 9.9.9"),
            json!("release version: ***"),
            false,
        ),
        (
            "strings",
            json!(["release version: 9.9.9", "unrelated"]),
            json!(["release version: ***", "unrelated"]),
            false,
        ),
        (
            "multimodal",
            json!([{"type":"text","text":"release version: 9.9.9"}, image.clone()]),
            json!([{"type":"text","text":"release version: ***"}, image]),
            false,
        ),
        (
            "unicode-escaped",
            json!("release version: 9.9.9"),
            json!("release version: ***"),
            true,
        ),
    ] {
        for body_model in ["native-model", "control-model"] {
            let upstream = MockServer::builder().start().await;
            let answer = native_answer("/v1/moderations");
            mount_native_answer(&upstream, "/v1/moderations", answer.clone()).await;
            let (router, _, mut events) = configured_app(&upstream.uri(), |snapshot| {
                let control: Model = serde_json::from_value(json!({
                    "display_name":"control-model","provider":"openai","model_name":"control-model",
                    "provider_key_id":PROVIDER
                }))
                .unwrap();
                snapshot.models.insert(ResourceEntry::new(
                    "55555555-5555-4555-8555-555555555555",
                    control,
                    1,
                ));
                edit_key(snapshot, |key| {
                    key.allowed_models.push("control-model".into())
                });
                let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
                    "name":"eda-mask","enabled":true,"kind":"pii","hook_point":"input",
                    "detectors":[],"custom_patterns":[{
                        "name":"eda_version","regex":r"version\s*:\s*(\d+(?:\.\d+)+)",
                        "action":"mask","replacement":"***"
                    }]
                }))
                .unwrap();
                snapshot.guardrails.insert(ResourceEntry::new(
                    "native-stateless-guard",
                    guardrail,
                    1,
                ));
                let attachment: aisix_core::models::GuardrailAttachment =
                    serde_json::from_value(json!({
                        "guardrail_id":"native-stateless-guard","scope_type":"env","priority":100
                    }))
                    .unwrap();
                snapshot.guardrail_attachments.insert(ResourceEntry::new(
                    "native-stateless-attachment",
                    attachment,
                    1,
                ));
            });
            let extension = json!({"model":"extension-model","keep":[17,true,null]});
            let body = json!({"model":body_model,"input":input,"future":extension});
            let mut encoded = body.to_string();
            if escaped {
                encoded = encoded.replace("9.9.9", r"\u0039.\u0039.\u0039");
                assert!(encoded.contains(r"\u0039"));
            }
            let mut request =
                native_request("/v1/moderations", &[("x-aisix-model", "native-model")]);
            *request.body_mut() = Body::from(encoded);
            let response = router.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{shape}: {body_model}");
            assert_eq!(response_json(response).await, answer);
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(sent["input"], masked, "{shape}: {body_model}");
            let expected_model = if body_model == "native-model" {
                "provider-model"
            } else {
                "control-model"
            };
            assert_eq!(sent["model"], expected_model);
            assert_eq!(sent["future"], extension);
            assert_eq!(requests[0].headers["authorization"], "Bearer upstream-key");
            let event = next_event(&mut events).await;
            assert_eq!(event.status_code, 200);
            assert_eq!(event.upstream_dispatched, Some(true));
            assert!(!event.guardrail_blocked);
            assert_eq!(
                event.redacted_entity_counts.get("eda_version").copied(),
                Some(1)
            );
            let masked_hit = event.guardrail_enforced_hits.iter().find(|hit| {
                hit.guardrail_name == "eda-mask" && hit.hook == "input" && hit.action == "masked"
            });
            assert_eq!(
                masked_hit
                    .and_then(|hit| hit.counts.get("eda_version"))
                    .copied(),
                Some(1),
                "{shape}: {event:?}"
            );
            assert!(
                event
                    .guardrail_enforced_hits
                    .iter()
                    .all(|hit| hit.action != "mask_unsupported"),
                "{shape}: {event:?}"
            );
            assert!(event
                .applied_guardrails
                .iter()
                .any(|guardrail| guardrail.kind == "pii" && guardrail.hook == "input"));
            assert!(!serde_json::to_string(&event).unwrap().contains("9.9.9"));
        }
    }
}

fn insert_body_model(snapshot: &AisixSnapshot, limited: bool) {
    let mut value = json!({
        "display_name":"body-model","provider":"openai","model_name":"provider-model-b",
        "provider_key_id":PROVIDER,"cost":{"input_per_1k":2.0,"output_per_1k":4.0}
    });
    if limited {
        value["rate_limit"] = json!({"rpd":1});
    }
    let model: Model = serde_json::from_value(value).unwrap();
    snapshot
        .models
        .insert(ResourceEntry::new(BODY_MODEL, model, 1));
}

fn conflicting_model_request(endpoint: &str, query_hint: bool) -> Request<Body> {
    let uri = if query_hint {
        format!("{endpoint}?model=native-model&keep=1")
    } else {
        endpoint.to_owned()
    };
    let headers = if query_hint {
        vec![]
    } else {
        vec![("x-aisix-model", "native-model")]
    };
    let mut request = native_request(&uri, &headers);
    let body = if endpoint == "/v1/moderations" {
        json!({"model":"body-model","input":"inspect body-selected model","future":{"kept":true}})
            .to_string()
    } else {
        "--native-test\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nbody-model\r\n--native-test\r\nContent-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\nPNG-native-model-authority\r\n--native-test--\r\n".to_owned()
    };
    *request.body_mut() = Body::from(body);
    request
}

#[tokio::test]
async fn explicit_stateless_body_model_controls_access_mapping_quota_and_billing_audit() {
    for endpoint in ["/v1/moderations", "/v1/images/variations"] {
        for query_hint in [false, true] {
            let upstream = MockServer::builder().start().await;
            let mut answer = native_answer(endpoint);
            // 本地上游报告的合成用量，只用于核对选中模型的计费归属。
            answer["usage"] = json!({"prompt_tokens":10,"completion_tokens":2,"total_tokens":12});
            mount_native_answer(&upstream, endpoint, answer.clone()).await;
            let (router, snapshot, mut events) = configured_app(&upstream.uri(), |snapshot| {
                insert_body_model(snapshot, true);
            });
            let denied = router
                .clone()
                .oneshot(conflicting_model_request(endpoint, query_hint))
                .await
                .unwrap();
            assert_eq!(
                denied.status(),
                StatusCode::FORBIDDEN,
                "{endpoint}, query={query_hint}"
            );
            assert_eq!(
                response_json(denied).await["error"]["type"],
                "permission_denied"
            );
            assert!(
                upstream.received_requests().await.unwrap().is_empty(),
                "{endpoint}, query={query_hint}"
            );
            let denied_event = next_event(&mut events).await;
            assert_eq!(denied_event.upstream_dispatched, Some(false));

            edit_key(&snapshot.load(), |key| {
                key.allowed_models.push("body-model".into())
            });
            let allowed = router
                .clone()
                .oneshot(conflicting_model_request(endpoint, query_hint))
                .await
                .unwrap();
            assert_eq!(
                allowed.status(),
                StatusCode::OK,
                "{endpoint}, query={query_hint}"
            );
            assert_eq!(response_json(allowed).await, answer);
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            if endpoint == "/v1/moderations" {
                let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
                assert_eq!(sent["model"], "provider-model-b");
                assert_eq!(sent["input"], "inspect body-selected model");
                assert_eq!(sent["future"], json!({"kept":true}));
            } else {
                let sent = String::from_utf8_lossy(&requests[0].body);
                assert!(
                    sent.contains("name=\"model\"\r\n\r\nprovider-model-b\r\n"),
                    "{sent}"
                );
                assert!(!sent.contains("\r\n\r\nbody-model\r\n"));
                assert!(!sent.contains("\r\n\r\nprovider-model\r\n"));
                assert!(sent.contains("PNG-native-model-authority"));
            }
            let event = next_event(&mut events).await;
            assert_eq!(event.model_id, BODY_MODEL);
            assert_eq!(event.requested_model, "body-model");
            assert_eq!(event.requested_model_id, BODY_MODEL);
            assert_eq!(event.provider_key_id.as_deref(), Some(PROVIDER));
            assert_eq!(event.upstream_dispatched, Some(true));
            assert_eq!(
                (
                    event.prompt_tokens,
                    event.completion_tokens,
                    event.total_tokens
                ),
                (10, 2, 12)
            );
            assert!((event.cost_usd - 0.028).abs() < 1e-9, "{event:?}");

            let limited = router
                .oneshot(conflicting_model_request(endpoint, query_hint))
                .await
                .unwrap();
            assert_eq!(
                limited.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "{endpoint}, query={query_hint}"
            );
            response_json(limited).await;
            assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
            let event = next_event(&mut events).await;
            assert_eq!(event.upstream_dispatched, Some(false));
            assert_eq!(event.model_id, BODY_MODEL);
            assert_eq!(event.requested_model, "body-model");
        }
    }
}

#[tokio::test]
async fn explicit_stateless_body_model_selects_its_own_guardrail_over_conflicting_hints() {
    for endpoint in ["/v1/moderations", "/v1/images/variations"] {
        for query_hint in [false, true] {
            let upstream = MockServer::builder().start().await;
            mount_native_answer(&upstream, endpoint, native_answer(endpoint)).await;
            let (router, _, mut events) = configured_app(&upstream.uri(), |snapshot| {
                insert_body_model(snapshot, false);
                edit_key(snapshot, |key| key.allowed_models.push("body-model".into()));
                attach_unconditional_block(snapshot, "input");
                let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
                    "guardrail_id":"native-stateless-guard","scope_type":"model","scope_id":BODY_MODEL,"priority":100
                })).unwrap();
                snapshot.guardrail_attachments.insert(ResourceEntry::new(
                    "native-stateless-attachment",
                    attachment,
                    2,
                ));
            });
            let control = router
                .clone()
                .oneshot(native_request(
                    endpoint,
                    &[("x-aisix-model", "native-model")],
                ))
                .await
                .unwrap();
            assert_eq!(control.status(), StatusCode::OK);
            assert_eq!(response_json(control).await, native_answer(endpoint));
            assert_eq!(next_event(&mut events).await.model_id, MODEL);
            let blocked = router
                .oneshot(conflicting_model_request(endpoint, query_hint))
                .await
                .unwrap();
            assert_eq!(
                blocked.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{endpoint}, query={query_hint}"
            );
            assert_eq!(
                response_json(blocked).await["error"]["type"],
                "content_filter"
            );
            assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
            let event = next_event(&mut events).await;
            assert!(event.guardrail_blocked);
            assert_eq!(event.upstream_dispatched, Some(false));
            assert_eq!(event.model_id, BODY_MODEL);
            assert_eq!(event.requested_model, "body-model");
            assert!(event
                .guardrail_enforced_hits
                .iter()
                .any(|hit| hit.hook == "input" && hit.action == "blocked"));
        }
    }
}
