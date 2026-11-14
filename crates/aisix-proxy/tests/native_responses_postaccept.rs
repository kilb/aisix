//! 上游已接受的原生创建：本地归属故障不能重派发，异步接受不冒充最终推理。
//! 本地协议服务仅用于组件验证，不冒充真实供应商端到端调用。
use aisix_core::native_objects::*;
use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_provider_openai::OpenAiBridge;
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_proxy::{build_router, ProxyState};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const FIRST: &str = "11111111-1111-4111-8111-111111111111";
const SECOND: &str = "11111111-1111-4111-8111-222222222222";
const PARENT: &str = "11111111-1111-4111-8111-333333333333";
const KEY: &str = "33333333-3333-4333-8333-333333333333";

#[derive(Default)]
struct Registry {
    submissions: Mutex<Vec<BeginRequest>>,
    bind_fails: bool,
    claim_fails: bool,
    claims: AtomicUsize,
}
macro_rules! unused {
    ($method:ident,$request:ty) => {
        fn $method(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}
impl NativeObjectRegistry for Registry {
    fn begin(
        &self,
        request: BeginRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let mut reply = NativeReply::new("pending_submission");
            reply.operation_id = Some(request.operation_id);
            reply.origin = Some(request.origin.clone());
            self.submissions.lock().unwrap().push(request);
            Ok(reply)
        })
    }
    fn bind(
        &self,
        request: BindRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            if self.bind_fails {
                return Err(NativeRegistryError::Unavailable);
            }
            let submissions = self.submissions.lock().unwrap();
            let submitted = submissions
                .iter()
                .find(|s| s.operation_id == request.operation_id)
                .unwrap();
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id":Uuid::new_v4(),"operation_id":request.operation_id,
                "origin":submitted.origin,"raw_id":request.raw_id,
                "protocol":"openai","kind":"response","version":1,
                "requested_model_id":submitted.requested_model_id,
                "requested_model":submitted.requested_model,
                "requested_service_tier":submitted.requested_service_tier
            }))
            .unwrap();
            let mut reply = NativeReply::new("bound");
            reply.record = Some(record);
            Ok(reply)
        })
    }
    fn claim(&self, _: ClaimRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.claims.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.claim_fails {
                Err(NativeRegistryError::Unavailable)
            } else {
                Ok(NativeReply::new("leased"))
            }
        })
    }
    unused!(lookup, LookupRequest);
    unused!(owned, OwnedRequest);
    unused!(tombstone, TombstoneRequest);
    unused!(renew, LeaseRequest);
    unused!(reschedule, LeaseRequest);
    unused!(complete, CompleteRequest);
}

fn app(
    first: &str,
    second: &str,
    registry: Arc<Registry>,
) -> (axum::Router, mpsc::Receiver<UsageEvent>) {
    app_with_stream_timeout(first, second, registry, false)
}

fn app_with_stream_timeout(
    first: &str,
    second: &str,
    registry: Arc<Registry>,
    stream_timeout: bool,
) -> (axum::Router, mpsc::Receiver<UsageEvent>) {
    app_with_request_overrides(first, second, registry, stream_timeout, None)
}

fn app_with_request_overrides(
    first: &str,
    second: &str,
    registry: Arc<Registry>,
    stream_timeout: bool,
    request_overrides: Option<Value>,
) -> (axum::Router, mpsc::Receiver<UsageEvent>) {
    let snapshot = AisixSnapshot::new();
    for (model_id, provider_id, name, base) in [
        (
            FIRST,
            "22222222-2222-4222-8222-111111111111",
            "first-native",
            first,
        ),
        (
            SECOND,
            "22222222-2222-4222-8222-222222222222",
            "second-native",
            second,
        ),
    ] {
        let provider: ProviderKey = serde_json::from_value(json!({"display_name":name,
            "provider":"openai","adapter":"openai","api_base":base,"secret":"upstream-key",
            "request":request_overrides}))
        .unwrap();
        snapshot
            .provider_keys
            .insert(ResourceEntry::new(provider_id, provider, 1));
        let model: Model = serde_json::from_value(json!({"display_name":name,"provider":"openai",
            "model_name":"provider-model","provider_key_id":provider_id,
            "stream_timeout": if stream_timeout { 1000 } else { 0 }}))
        .unwrap();
        snapshot
            .models
            .insert(ResourceEntry::new(model_id, model, 1));
    }
    let parent:Model=serde_json::from_value(json!({"display_name":"native-parent",
        "routing":{"strategy":"failover","retries":0,"targets":[{"model_id":FIRST},{"model_id":SECOND}]}})).unwrap();
    snapshot
        .models
        .insert(ResourceEntry::new(PARENT, parent, 1));
    let key: ApiKey = serde_json::from_value(json!({"key_hash":ApiKey::hash_bearer("caller"),
        "allowed_models":["native-parent"]}))
    .unwrap();
    snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 1));
    let hub = Arc::new(Hub::new());
    hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
    let (tx, rx) = mpsc::channel(16);
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "postaccept-test",
        ))
        .with_native_objects(registry)
        .with_usage_sink(UsageSink::new(tx));
    (build_router(state), rx)
}
async fn send(app: axum::Router, background: bool) -> (StatusCode, Value) {
    let mut body = json!({"model":"native-parent","input":"hello"});
    if background {
        body["background"] = json!(true);
    }
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer caller")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    (status, body)
}
fn complete() -> Value {
    json!({"id":"resp_owned","object":"response","status":"completed",
    "model":"provider-model","output":[{"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":"hello"}]}],
    "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}})
}
async fn mount(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}
fn rows(rx: &mut mpsc::Receiver<UsageEvent>) -> Vec<UsageEvent> {
    let mut out = Vec::new();
    while let Ok(row) = rx.try_recv() {
        out.push(row);
    }
    out
}
#[tokio::test]
async fn sync_create_does_not_claim_the_async_registry_after_binding() {
    let first = MockServer::builder().start().await;
    let second = MockServer::builder().start().await;
    mount(&first, complete()).await;
    mount(&second, complete()).await;
    let registry = Arc::new(Registry {
        claim_fails: true,
        ..Default::default()
    });
    let (app, mut events) = app(&first.uri(), &second.uri(), registry.clone());
    let (status, body) = send(app, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(registry.claims.load(Ordering::SeqCst), 0);
    assert_eq!(first.received_requests().await.unwrap().len(), 1);
    assert!(second.received_requests().await.unwrap().is_empty());
    let rows = rows(&mut events);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].prompt_tokens,
            rows[0].completion_tokens,
            rows[0].total_tokens
        ),
        (7, 3, 10)
    );
    assert!(rows[0].is_final_attempt);
}
#[tokio::test]
async fn sync_binding_failure_preserves_reported_work_without_redispatch() {
    let first = MockServer::builder().start().await;
    let second = MockServer::builder().start().await;
    mount(&first, complete()).await;
    mount(&second, complete()).await;
    let registry = Arc::new(Registry {
        bind_fails: true,
        ..Default::default()
    });
    let (app, mut events) = app(&first.uri(), &second.uri(), registry);
    let (status, body) = send(app, false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(first.received_requests().await.unwrap().len(), 1);
    assert!(second.received_requests().await.unwrap().is_empty());
    let rows = rows(&mut events);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].prompt_tokens,
            rows[0].completion_tokens,
            rows[0].total_tokens
        ),
        (7, 3, 10)
    );
    assert_eq!(rows[0].status_code, 503);
    assert!(rows[0].is_final_attempt);
}
#[tokio::test]
async fn background_terminal_observation_failure_never_redispatches() {
    let first = MockServer::builder().start().await;
    let second = MockServer::builder().start().await;
    mount(&first, complete()).await;
    mount(&second, complete()).await;
    let registry = Arc::new(Registry {
        claim_fails: true,
        ..Default::default()
    });
    let (app, _) = app(&first.uri(), &second.uri(), registry);
    let (status, body) = send(app, true).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(first.received_requests().await.unwrap().len(), 1);
    assert!(second.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn queued_background_acceptance_is_not_a_logical_inference_final() {
    let first = MockServer::builder().start().await;
    let second = MockServer::builder().start().await;
    mount(
        &first,
        json!({"id":"resp_owned","object":"response","status":"queued",
        "model":"provider-model","output":[],"usage":null}),
    )
    .await;
    let registry = Arc::new(Registry::default());
    let (app, mut events) = app(&first.uri(), &second.uri(), registry.clone());
    let (status, body) = send(app, true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = rows(&mut events);
    assert_eq!(rows.len(), 1);
    assert!(
        !rows[0].is_final_attempt,
        "terminal outbox alone owns the inference final"
    );
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (0, 0));
    let wire = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(wire["inference_stage"], "accepted");
    assert_eq!(
        wire["native_operation_id"],
        json!(registry.submissions.lock().unwrap()[0].operation_id)
    );
    assert_eq!(wire["upstream_dispatched"], true);
    assert!(second.received_requests().await.unwrap().is_empty());
}

// response.failed 带已绑定对象 ID 时已经创建后台任务，不得再派发或产生第二条尝试。
#[tokio::test]
async fn background_failed_first_frame_settles_as_acceptance_without_failover() {
    let first = MockServer::builder().start().await;
    let second = MockServer::builder().start().await;
    let terminal = json!({"type":"response.failed","response":{
        "id":"resp_owned","object":"response","status":"failed","output":[],
        "error":{"code":"server_error","message":"upstream failed"},
        "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!("event: response.failed\ndata: {terminal}\n\n")),
        )
        .expect(1)
        .mount(&first)
        .await;
    mount(&second, complete()).await;
    let registry = Arc::new(Registry::default());
    let (app, mut events) =
        app_with_stream_timeout(&first.uri(), &second.uri(), registry.clone(), true);
    let response = app.oneshot(Request::builder().method("POST").uri("/v1/responses")
        .header("authorization", "Bearer caller").header("content-type", "application/json")
        .body(Body::from(json!({"model":"native-parent","input":"hello","stream":true,"background":true}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    assert!(std::str::from_utf8(&body)
        .unwrap()
        .contains("response.failed"));
    first.verify().await;
    assert!(second.received_requests().await.unwrap().is_empty());
    let rows = rows(&mut events);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].inference_stage, Some(InferenceStage::Accepted));
    assert_eq!(
        rows[0].native_operation_id,
        Some(registry.submissions.lock().unwrap()[0].operation_id)
    );
    assert!(!rows[0].is_final_attempt);
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (0, 0));
}

async fn assert_stream_binding_failure_preserves_reported_usage(stream_timeout: bool) {
    let first = MockServer::builder().start().await;
    let second = MockServer::builder().start().await;
    let terminal = json!({"type":"response.completed","response":complete()});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!("event: response.completed\ndata: {terminal}\n\n")),
        )
        .mount(&first)
        .await;
    mount(&second, complete()).await;
    let registry = Arc::new(Registry {
        bind_fails: true,
        ..Default::default()
    });
    let (app, mut events) =
        app_with_stream_timeout(&first.uri(), &second.uri(), registry, stream_timeout);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer caller")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"native-parent","input":"hello","stream":true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    assert!(
        matches!(status, StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE),
        "{status}: {body}"
    );
    assert!(
        !body.contains("resp_owned"),
        "unbound identifiers must never be released: {body}"
    );
    assert!(
        !body.contains("output_text"),
        "unbound output must never be released: {body}"
    );
    assert_eq!(first.received_requests().await.unwrap().len(), 1);
    assert!(
        second.received_requests().await.unwrap().is_empty(),
        "an accepted stream cannot be redispatched"
    );
    let rows = rows(&mut events);
    assert_eq!(rows.len(), 1, "the accepted attempt must settle once");
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens, rows[0].total_tokens), (7, 3, 10),
        "the complete upstream terminal frame reports billable usage even if its ID cannot be bound");
    assert_eq!(rows[0].error_class, "dependency_unavailable");
    assert!(rows[0].is_final_attempt);
}

#[tokio::test]
async fn stream_bind_failure_retains_terminal_usage_without_releasing_unbound_output() {
    assert_stream_binding_failure_preserves_reported_usage(false).await;
}

#[tokio::test]
async fn stream_peek_bind_failure_retains_terminal_usage_without_redispatch() {
    assert_stream_binding_failure_preserves_reported_usage(true).await;
}

#[tokio::test]
async fn rejected_background_responses_are_final_attempts_and_false_is_synchronous() {
    for scenario in ["preflight_denied", "native_rejected", "background_false"] {
        let first = MockServer::builder().start().await;
        let second = MockServer::builder().start().await;
        let registry = Arc::new(Registry::default());
        let (app, mut events) = app(&first.uri(), &second.uri(), registry.clone());
        let (body, expected) = match scenario {
            "preflight_denied" => (
                json!({"model":"first-native","input":"hello","background":true}),
                StatusCode::FORBIDDEN,
            ),
            "native_rejected" => (
                json!({"model":"native-parent","input":"hello","background":true}),
                StatusCode::BAD_REQUEST,
            ),
            _ => (
                json!({"model":"native-parent","input":"hello","background":false}),
                StatusCode::OK,
            ),
        };
        if scenario == "native_rejected" {
            Mock::given(method("POST")).and(path("/v1/responses"))
                .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                    "error":{"type":"invalid_request_error","code":"invalid_value","param":"input","message":"input is not valid"}
                }))).expect(1).mount(&first).await;
        } else {
            mount(&first, complete()).await;
        }
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("authorization", "Bearer caller")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if scenario == "preflight_denied" {
                StatusCode::NOT_FOUND
            } else {
                expected
            },
            "{scenario}"
        );
        let _ = to_bytes(response.into_body(), 65536).await.unwrap();
        let rows = rows(&mut events);
        assert_eq!(rows.len(), 1, "{scenario}");
        let event = &rows[0];
        assert_eq!(
            event.inference_stage,
            Some(InferenceStage::Attempt),
            "{scenario}"
        );
        assert!(event.is_final_attempt, "{scenario}");
        assert_eq!(event.status_code, expected.as_u16());
        assert_eq!(
            event.upstream_dispatched,
            Some(scenario != "preflight_denied")
        );
        assert_eq!(
            (event.prompt_tokens, event.completion_tokens),
            if scenario == "background_false" {
                (7, 3)
            } else {
                (0, 0)
            }
        );
        let submissions = registry.submissions.lock().unwrap().clone();
        assert_eq!(
            submissions.len(),
            usize::from(scenario != "preflight_denied")
        );
        if let Some(submission) = submissions.first() {
            assert_eq!(submission.asynchronous, scenario == "native_rejected");
        }
        assert!(second.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn accepted_background_input_scalar_uses_final_body_after_provider_overrides() {
    let first = MockServer::builder().start().await;
    let second = MockServer::builder().start().await;
    mount(
        &first,
        json!({"id":"resp_owned","object":"response","status":"queued","output":[]}),
    )
    .await;
    let registry = Arc::new(Registry::default());
    let (app, _) = app_with_request_overrides(
        &first.uri(),
        &second.uri(),
        registry.clone(),
        false,
        Some(json!({"param_renames":{"input":"vendor_input"}})),
    );
    let request = json!({"model":"native-parent","background":true,"input":"original text",
        "instructions":"B界🙂"});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer caller")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 65536).await.unwrap();
    let calls = first.received_requests().await.unwrap();
    assert_eq!(calls.len(), 1);
    let sent: Value = serde_json::from_slice(&calls[0].body).unwrap();
    assert_eq!(sent.get("input"), None);
    assert_eq!(sent["vendor_input"], "original text");
    assert_eq!(sent["instructions"], "B界🙂");
    let submissions = registry.submissions.lock().unwrap();
    assert_eq!(submissions.len(), 1);
    assert_eq!(
        submissions[0].upstream_input_text,
        Some(aisix_core::usage_io::TextMeasurement {
            utf8_bytes: 8,
            unicode_scalars: 3,
            complete: true,
        })
    );
}
