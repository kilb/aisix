//! 原生生命周期组件测试：通过真实路由检查路径、请求字节和 SDK 应答形状。
//! 本地协议服务只验证传输契约，不代替真实提供商端到端验收。
use std::sync::Arc;

use aisix_core::native_objects::*;
use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_provider_openai::OpenAiBridge;
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";
const KEY: &str = "33333333-3333-4333-8333-333333333333";

fn state(base: &str) -> ProxyState {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name": "native", "secret": "upstream-key", "provider": "openai",
        "api_base": base, "adapter": "openai"
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name": "native-model", "provider": "openai", "model_name": "provider-model",
        "provider_key_id": PROVIDER
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash": ApiKey::hash_bearer("caller"), "allowed_models": ["native-model"]
    }))
    .unwrap();
    snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 1));
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
    ProxyState::new(SnapshotHandle::new(snapshot), hub, &config).without_cache()
}

fn app(base: &str) -> axum::Router {
    build_router(state(base))
}

// 托管生命周期要求持久归属和账号密钥；本地替身不绕过正式 resolver 的当前边界检查。
struct LifecycleAuthority(NativeRecord);

macro_rules! unsupported_lifecycle_registry {
    ($method:ident,$request:ty) => {
        fn $method(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for LifecycleAuthority {
    unsupported_lifecycle_registry!(begin, BeginRequest);
    unsupported_lifecycle_registry!(bind, BindRequest);
    unsupported_lifecycle_registry!(owned, OwnedRequest);
    unsupported_lifecycle_registry!(renew, LeaseRequest);
    unsupported_lifecycle_registry!(reschedule, LeaseRequest);
    unsupported_lifecycle_registry!(complete, CompleteRequest);

    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            if request.api_key_id.to_string() != KEY
                || request.locator.protocol != NativeProtocol::Openai
                || request.locator.kind != NativeObjectKind::Response
                || request.locator.raw_id != self.0.raw_id
            {
                return Err(NativeRegistryError::NotFound);
            }
            let mut reply = NativeReply::new("found");
            reply.record = Some(self.0.clone());
            Ok(reply)
        })
    }

    fn claim(&self, _: ClaimRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Ok(NativeReply::new("leased")) })
    }

    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            if request.api_key_id.to_string() != KEY
                || request.record_id != self.0.record_id
                || request.version != self.0.version
            {
                return Err(NativeRegistryError::NotFound);
            }
            Ok(NativeReply::new("deleted"))
        })
    }
}

fn managed_state(state: ProxyState) -> ProxyState {
    let platform =
        aisix_proxy::platform::PlatformSettings::from_secret("lifecycle-managed-authority");
    let provider = state
        .snapshot
        .load()
        .provider_keys
        .get_by_id(PROVIDER)
        .unwrap();
    let origin = OriginBinding {
        surface: None,
        model_id: uuid::Uuid::parse_str(MODEL).unwrap(),
        provider_key_id: uuid::Uuid::parse_str(PROVIDER).unwrap(),
        boundary: configured_native_origin(
            &platform.id_key,
            uuid::Uuid::parse_str(PROVIDER).unwrap(),
            &provider.value,
            NativeProtocol::Openai,
            NativeObjectKind::Response,
        )
        .unwrap(),
    };
    let record=serde_json::from_value(json!({
        "record_id":uuid::Uuid::new_v4(),"operation_id":uuid::Uuid::new_v4(),"api_key_id":KEY,
        "requested_model_id":MODEL,"requested_model":"native-model","upstream_model":"provider-model",
        "origin":origin,"protocol":"openai","kind":"response","raw_id":"resp_native","completed":true,"version":1
    })).unwrap();
    state
        .with_platform(platform)
        .with_native_objects(Arc::new(LifecycleAuthority(record)))
}

// 管理成功/失败都不能冒充推理；真实压缩失败仍保留为上游尝试。
#[tokio::test]
async fn management_usage_and_failed_inference_keep_distinct_lifecycle_stages() {
    for (verb, suffix, status, answer, stage) in [
        (
            "GET",
            "/resp_native",
            200,
            json!({"id":"resp_native","object":"response","status":"completed","output":[],"usage":{"input_tokens":70,"output_tokens":20,"total_tokens":90}}),
            "management",
        ),
        (
            "GET",
            "/resp_native",
            503,
            json!({"error":{"message":"provider unavailable","type":"server_error"}}),
            "management",
        ),
        (
            "DELETE",
            "/resp_native",
            200,
            json!({"id":"resp_native","object":"response.deleted","deleted":true}),
            "management",
        ),
        (
            "POST",
            "/resp_native/cancel",
            200,
            json!({"id":"resp_native","object":"response","status":"cancelled","output":[]}),
            "management",
        ),
        (
            "GET",
            "/resp_native/input_items",
            200,
            json!({"object":"list","data":[],"has_more":false}),
            "management",
        ),
        (
            "POST",
            "/input_tokens",
            200,
            json!({"object":"response.input_tokens","input_tokens":317}),
            "management",
        ),
        (
            "POST",
            "/compact",
            503,
            json!({"error":{"message":"provider unavailable","type":"server_error"}}),
            "attempt",
        ),
    ] {
        let upstream = MockServer::builder().start().await;
        Mock::given(method(verb))
            .and(path(format!("/v1/responses{suffix}")))
            .respond_with(ResponseTemplate::new(status).set_body_json(answer))
            .expect(1)
            .mount(&upstream)
            .await;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UsageEvent>(16);
        let managed = suffix.starts_with("/resp_native");
        let mut proxy = state(&upstream.uri()).with_usage_sink(UsageSink::new(tx));
        if managed {
            proxy = managed_state(proxy);
        }
        let router = build_router(proxy);
        let route = format!("/v1/responses{suffix}");
        let mut request = Request::builder()
            .method(verb)
            .uri(route)
            .header("authorization", "Bearer caller");
        let body = if matches!(suffix, "/compact" | "/input_tokens") {
            request = request.header("content-type", "application/json");
            Body::from(json!({"model":"native-model","input":"sentinel"}).to_string())
        } else {
            Body::empty()
        };
        let response = router.oneshot(request.body(body).unwrap()).await.unwrap();
        let observed_status = response.status().as_u16();
        let _ = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let caller_status = if status == 503 && !managed {
            502
        } else {
            status
        };
        assert_eq!(observed_status, caller_status);
        upstream.verify().await;
        let rows: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(rows.len(), 1, "{verb} {suffix}");
        let wire = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(wire["inference_stage"], stage, "{verb} {suffix}: {wire}");
        assert_eq!(wire["upstream_dispatched"], true);
        assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (0, 0));
    }
}

async fn post(router: axum::Router, route: &str, body: Value) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(route)
                .header("authorization", "Bearer caller")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

// 明确报告的零计数不能用本地估算覆盖，JSON 和终态流保持相同计量。
#[tokio::test]
async fn native_reported_zero_usage_survives_json_and_terminal_streams() {
    for streamed in [false, true] {
        for (input, output, text) in [(0, 0, ""), (0, 3, "answer"), (7, 0, "answer")] {
            let upstream = MockServer::builder().start().await;
            let answer = json!({
                "id":"resp_native", "object":"response", "status":"completed",
                "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],
                "usage":{"input_tokens":input,"output_tokens":output,"total_tokens":input+output}
            });
            let template = if streamed {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!(
                        "data: {}\n\n",
                        json!({"type":"response.completed","response":answer})
                    ))
            } else {
                ResponseTemplate::new(200).set_body_json(answer)
            };
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(template)
                .expect(1)
                .mount(&upstream)
                .await;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<UsageEvent>(16);
            let router = build_router(state(&upstream.uri()).with_usage_sink(UsageSink::new(tx)));
            let response = post(router, "/v1/responses", json!({
                "model":"native-model", "input":"prompt that would otherwise be estimated", "stream":streamed
            })).await;
            assert_eq!(response.status(), StatusCode::OK);
            to_bytes(response.into_body(), 1 << 20).await.unwrap();
            let usage = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens
                ),
                (input, output, input + output),
                "streamed={streamed}"
            );
            assert!(
                !usage.usage_estimated,
                "reported values must remain authoritative"
            );
            assert!(usage.is_final_attempt);
            assert!(rx.try_recv().is_err());
            upstream.verify().await;
        }
    }
}

// 删除实际路由，或将原生子路径改为 /responses，都会使这些断言失败。
#[tokio::test]
async fn compact_calls_native_endpoint_and_preserves_compaction_items() {
    let upstream = MockServer::builder().start().await;
    let answer = json!({"id":"cmp_native", "object":"response.compaction", "created_at":1,
        "output":[{"type":"compaction", "id":"cmp_item", "encrypted_content":"native-ciphertext"}],
        "usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}});
    Mock::given(method("POST"))
        .and(path("/v1/responses/compact"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&answer))
        .mount(&upstream)
        .await;
    let response = post(
        app(&upstream.uri()),
        "/v1/responses/compact",
        json!({
            "model":"native-model", "input":[{"role":"user","content":"compact me"}],
            "instructions":"keep decisions", "metadata":{"model":"leave nested model"}
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let actual: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(actual, answer);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "provider-model");
    assert_eq!(sent["metadata"]["model"], "leave nested model");
    assert_eq!(sent["instructions"], "keep decisions");
}

#[tokio::test]
async fn input_tokens_relays_provider_count_and_extra_fields() {
    let upstream = MockServer::builder().start().await;
    let answer = json!({"object":"response.input_tokens", "input_tokens":317, "provider_detail":{"retained":true}});
    Mock::given(method("POST"))
        .and(path("/v1/responses/input_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&answer))
        .mount(&upstream)
        .await;
    let response = post(
        app(&upstream.uri()),
        "/v1/responses/input_tokens",
        json!({
            "model":"native-model", "input":"x", "text":{"format":{"type":"text"}}
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let actual: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(actual, answer);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "provider-model");
    assert_eq!(sent["text"]["format"]["type"], "text");
}

#[tokio::test]
async fn a_modelless_native_operation_never_selects_the_first_account() {
    let upstream = MockServer::builder().start().await;
    let response = post(
        app(&upstream.uri()),
        "/v1/responses/input_tokens",
        json!({"input":"x"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let actual: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert!(actual["error"]["message"]
        .as_str()
        .unwrap()
        .contains("model"));
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
