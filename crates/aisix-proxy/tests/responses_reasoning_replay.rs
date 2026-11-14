//! 两轮历史重放组件合同：正式路由连接本地协议上游，不代替供应商端到端验收。
use std::sync::Arc;

use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig};
use aisix_gateway::Hub;
use aisix_provider_openai::OpenAiBridge;
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn state(base: &str, native: bool) -> ProxyState {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"replay", "secret":"upstream-key", "provider":"openai",
        "api_base":base, "adapter":"openai",
        "apis":if native { json!({"responses":{}}) } else { json!({}) }
    }))
    .unwrap();
    snapshot.provider_keys.insert(ResourceEntry::new(
        "22222222-2222-4222-8222-222222222222",
        provider,
        1,
    ));
    let model: Model = serde_json::from_value(json!({
        "display_name":"replay-model", "provider":"openai", "model_name":"provider-model",
        "provider_key_id":"22222222-2222-4222-8222-222222222222"
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(
        "11111111-1111-4111-8111-111111111111",
        model,
        1,
    ));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("caller"), "allowed_models":["replay-model"]
    }))
    .unwrap();
    snapshot.apikeys.insert(ResourceEntry::new(
        "33333333-3333-4333-8333-333333333333",
        key,
        1,
    ));
    let hub = Arc::new(Hub::new());
    hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
    let config = ProxyConfig {
        addr: "127.0.0.1:0".into(),
        request_body_limit_bytes: 1024 * 1024,
        real_ip: Default::default(),
        request_id: Default::default(),
        url_rewrites: Vec::new(),
        expose_debug_headers: false,
        platform: Default::default(),
        tls: None,
        listeners: Vec::new(),
        thread_per_core: None,
        workers: None,
    };
    ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "replay-test",
        ))
}

fn router(base: &str, native: bool) -> axum::Router {
    build_router(state(base, native))
}

fn routing_router(base: &str, native_first: bool) -> axum::Router {
    let state = state(base, false);
    let snapshot = state.snapshot.load();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"native", "secret":"upstream-key", "provider":"openai",
        "api_base":base, "adapter":"openai", "apis":{"responses":{}}
    }))
    .unwrap();
    snapshot.provider_keys.insert(ResourceEntry::new(
        "44444444-4444-4444-8444-444444444444",
        provider,
        1,
    ));
    let model: Model = serde_json::from_value(json!({
        "display_name":"native-target", "provider":"openai", "model_name":"provider-model",
        "provider_key_id":"44444444-4444-4444-8444-444444444444"
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(
        "55555555-5555-4555-8555-555555555555",
        model,
        1,
    ));
    let targets = if native_first {
        [
            "55555555-5555-4555-8555-555555555555",
            "11111111-1111-4111-8111-111111111111",
        ]
    } else {
        [
            "11111111-1111-4111-8111-111111111111",
            "55555555-5555-4555-8555-555555555555",
        ]
    };
    let group: Model = serde_json::from_value(json!({
        "display_name":"replay-group", "routing":{"strategy":"failover", "retries":0,
            "targets":targets.map(|id| json!({"model_id":id}))}
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(
        "66666666-6666-4666-8666-666666666666",
        group,
        1,
    ));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("caller"), "allowed_models":["replay-model","replay-group"]
    }))
    .unwrap();
    snapshot.apikeys.insert(ResourceEntry::new(
        "33333333-3333-4333-8333-333333333333",
        key,
        1,
    ));
    drop(snapshot);
    build_router(state)
}

fn request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", "Bearer caller")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn response_output(response: axum::response::Response, streaming: bool) -> Vec<Value> {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let body = if streaming {
        let mut decoder = aisix_gateway::sse::SseDecoder::default();
        decoder
            .feed(bytes.as_ref())
            .unwrap()
            .into_iter()
            .filter_map(|event| match event {
                aisix_gateway::sse::SseEvent::Data(data) => {
                    serde_json::from_str::<Value>(&data).ok()
                }
                aisix_gateway::sse::SseEvent::Done => None,
            })
            .find(|event| event["type"] == "response.completed")
            .unwrap()["response"]
            .clone()
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    body["output"].as_array().unwrap().clone()
}

async fn translated_two_turn_replay(streaming: bool) {
    let upstream = MockServer::start().await;
    let answer = if streaming {
        let chunks = [
            json!({"id":"chatcmpl_replay","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Check two plus two."},"finish_reason":null}]}),
            json!({"id":"chatcmpl_replay","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"4"},"finish_reason":null}]}),
            json!({"id":"chatcmpl_replay","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
        ];
        ResponseTemplate::new(200).set_body_raw(
            format!(
                "{}data: [DONE]\n\n",
                chunks
                    .iter()
                    .map(|chunk| format!("data: {chunk}\n\n"))
                    .collect::<String>()
            ),
            "text/event-stream",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id":"chatcmpl_replay","object":"chat.completion","created":1,"model":"provider-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"4","reasoning_content":"Check two plus two."},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
        }))
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(answer)
        .expect(2)
        .mount(&upstream)
        .await;
    let app = router(&upstream.uri(), false);
    let first = app
        .clone()
        .oneshot(request(json!({
            "model":"replay-model","input":"What is two plus two?","stream":streaming,"store":false
        })))
        .await
        .unwrap();
    let output = response_output(first, streaming).await;
    let reasoning = output
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap();
    let reasoning_id = reasoning["id"].as_str().unwrap();
    assert!(!reasoning_id.is_empty());
    assert!(reasoning.get("encrypted_content").is_none());
    assert_eq!(reasoning["summary"][0]["text"], "Check two plus two.");
    let mut history = vec![json!({"role":"user","content":"What is two plus two?"})];
    history.extend(output.clone());
    history.push(json!({"role":"user","content":"Explain it again."}));
    let second = app
        .oneshot(request(json!({
            "model":"replay-model","input":history,"stream":streaming,"store":false
        })))
        .await
        .unwrap();
    response_output(second, streaming).await;
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "each turn dispatches once");
    let sent: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let assistant = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["reasoning_content"], "Check two plus two.");
    assert_eq!(assistant["content"], "4");
    upstream.verify().await;
}

#[tokio::test]
async fn translated_reasoning_json_history_replays_on_the_next_turn() {
    translated_two_turn_replay(false).await;
}

#[tokio::test]
async fn translated_reasoning_sse_history_replays_on_the_next_turn() {
    translated_two_turn_replay(true).await;
}

#[tokio::test]
async fn native_reasoning_without_authority_stays_rejected_before_dispatch() {
    let upstream = MockServer::start().await;
    let app = router(&upstream.uri(), true);
    let mut refusals = Vec::new();
    for id in ["rs_foreign", "rs_unknown"] {
        let response = app.clone().oneshot(request(json!({
            "model":"replay-model","input":[{
                "type":"reasoning","id":id,"summary":[{"type":"summary_text","text":"visible summary"}]
            }]
        }))).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["error"]["param"], "input[0].id");
        refusals.push(body);
    }
    assert_eq!(refusals[0], refusals[1]);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn translated_id_only_or_opaque_reasoning_still_requires_object_authority() {
    let upstream = MockServer::start().await;
    let app = router(&upstream.uri(), false);
    for item in [
        json!({"type":"reasoning","id":"rs_unknown","summary":[]}),
        json!({"type":"reasoning","id":"rs_unknown","summary":[{"type":"summary_text","text":"  "}]}),
        json!({"type":"reasoning","id":"rs_unknown","content":[{"type":"redacted_thinking","text":"opaque payload"}],"summary":[]}),
        json!({"type":"item_reference","id":"rs_unknown","summary":[{"type":"summary_text","text":"not an inline item"}]}),
    ] {
        let response = app
            .clone()
            .oneshot(request(json!({
                "model":"replay-model","input":[item]
            })))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["error"]["param"], "input[0].id");
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn translated_failure_cannot_send_unowned_reasoning_to_native_fallback() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error":{"message":"unavailable", "type":"server_error"}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = routing_router(&upstream.uri(), false).oneshot(request(json!({
        "model":"replay-group","input":[{
            "type":"reasoning","id":"rs_unknown","summary":[{"type":"summary_text","text":"visible reasoning"}]
        },{"role":"user","content":"continue"}]
    }))).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["param"], "input[0].id");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "unowned native history never reaches an account"
    );
    assert_eq!(requests[0].url.path(), "/chat/completions");
    upstream.verify().await;
}

#[tokio::test]
async fn native_reasoning_authority_failure_cannot_fall_back_to_translation() {
    let upstream = MockServer::start().await;
    let response = routing_router(&upstream.uri(), true).oneshot(request(json!({
        "model":"replay-group","input":[{
            "type":"reasoning","id":"rs_foreign","summary":[{"type":"summary_text","text":"visible reasoning"}]
        },{"role":"user","content":"continue"}]
    }))).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(body["error"]["param"], "input[0].id");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
