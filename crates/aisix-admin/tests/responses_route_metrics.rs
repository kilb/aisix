//! 经真实代理路由发出原生操作，再从正式指标路由抓取，防止只定义指标而未接线。
use std::sync::Arc;

use aisix_admin::{metrics_router, ConfigStore, InMemoryStore, ModelsStatusState};
use aisix_core::config::PrometheusConfig;
use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{
    AisixSnapshot, ApiKey, ConfigStatus, Model, ProviderKey, ProxyConfig, ResourceEntry, SourceKind,
};
use aisix_gateway::Hub;
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
// 旧标签和原始 ID 都不得进入指标；管理请求使用下方登记的原始 ID。
const RESPONSE: &str =
    "resp_YWlzaXg6djE7MTExMTExMTEtMTExMS00MTExLTgxMTEtMTExMTExMTExMTExO3Jlc3BfbmF0aXZl";

// 已完成的自有 Response 只支持管理读取，不创建额外推理或后台终态。
struct ResponseAuthority(NativeRecord);

macro_rules! unused_registry {
    ($method:ident,$request:ty) => {
        fn $method(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for ResponseAuthority {
    unused_registry!(begin, BeginRequest);
    unused_registry!(bind, BindRequest);
    unused_registry!(owned, OwnedRequest);
    unused_registry!(renew, LeaseRequest);
    unused_registry!(reschedule, LeaseRequest);
    unused_registry!(complete, CompleteRequest);

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

    fn claim(
        &self,
        request: ClaimRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            if request.record_id != Some(self.0.record_id)
                || request.protocol != Some(NativeProtocol::Openai)
                || request.kind != Some(NativeObjectKind::Response)
                || request.limit != 1
            {
                return Err(NativeRegistryError::NotFound);
            }
            Ok(NativeReply::new("leased"))
        })
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

fn state(base: &str) -> Result<ProxyState, Box<dyn std::error::Error>> {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name": "native", "secret": "upstream-key", "provider": "openai",
        "api_base": base, "adapter": "openai"
    }))?;
    let platform =
        aisix_proxy::platform::PlatformSettings::from_secret("responses-metrics-authority");
    let origin = OriginBinding {
        surface: None,
        model_id: serde_json::from_value(json!(MODEL))?,
        provider_key_id: serde_json::from_value(json!(PROVIDER))?,
        boundary: configured_native_origin(
            &platform.id_key,
            serde_json::from_value(json!(PROVIDER))?,
            &provider,
            NativeProtocol::Openai,
            NativeObjectKind::Response,
        )?,
    };
    let record = serde_json::from_value(json!({
        "record_id":"44444444-4444-4444-8444-444444444444",
        "operation_id":"55555555-5555-4555-8555-555555555555","api_key_id":KEY,
        "requested_model_id":MODEL,"requested_model":"native-model","upstream_model":"provider-model",
        "origin":origin,"protocol":"openai","kind":"response","raw_id":"resp_native","completed":true,"version":1
    }))?;
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name": "native-model", "provider": "openai", "model_name": "provider-model",
        "provider_key_id": PROVIDER
    }))?;
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash": ApiKey::hash_bearer("caller"), "allowed_models": ["native-model"]
    }))?;
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
    Ok(ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_platform(platform)
        .with_native_objects(Arc::new(ResponseAuthority(record))))
}

fn samples<'a>(scrape: &'a str, family: &str, endpoint: &str) -> Vec<&'a str> {
    let prefix = format!("{family}{{");
    let endpoint = format!("endpoint=\"{endpoint}\"");
    scrape
        .lines()
        .filter(|line| line.starts_with(&prefix) && line.contains(&endpoint))
        .collect()
}

#[tokio::test]
async fn native_responses_operations_reach_proxy_metrics_and_only_inference_reaches_llm_metrics(
) -> Result<(), Box<dyn std::error::Error>> {
    for (verb, suffix, endpoint, answer, inference) in [
        (
            "POST",
            "/compact",
            "/v1/responses/compact",
            json!({"id":"cmp_native","object":"response.compaction","output":[],"usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}),
            true,
        ),
        (
            "POST",
            "/input_tokens",
            "/v1/responses/input_tokens",
            json!({"object":"response.input_tokens","input_tokens":317}),
            true,
        ),
        (
            "GET",
            "/resp_native",
            "/v1/responses/:id",
            json!({"id":"resp_native","object":"response","status":"completed","output":[],"usage":{"input_tokens":70,"output_tokens":20,"total_tokens":90}}),
            false,
        ),
        (
            "DELETE",
            "/resp_native",
            "/v1/responses/:id",
            json!({"id":"resp_native","object":"response.deleted","deleted":true}),
            false,
        ),
        (
            "POST",
            "/resp_native/cancel",
            "/v1/responses/:id/cancel",
            json!({"id":"resp_native","object":"response","status":"cancelled","output":[]}),
            false,
        ),
        (
            "GET",
            "/resp_native/input_items",
            "/v1/responses/:id/input_items",
            json!({"object":"list","data":[],"has_more":false}),
            false,
        ),
    ] {
        let upstream = MockServer::builder().start().await;
        Mock::given(method(verb))
            .and(path(format!("/v1/responses{suffix}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer))
            .expect(1)
            .mount(&upstream)
            .await;
        let state = state(&upstream.uri())?;
        let metrics = state.metrics.clone();
        let route = format!("/v1/responses{suffix}");
        let mut request = Request::builder()
            .method(verb)
            .uri(route)
            .header("authorization", "Bearer caller");
        let body = if inference {
            request = request.header("content-type", "application/json");
            Body::from(json!({"model":"native-model","input":"sentinel"}).to_string())
        } else {
            Body::empty()
        };
        let response = build_router(state).oneshot(request.body(body)?).await?;
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 20).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "{verb} {endpoint}: {}",
            String::from_utf8_lossy(&body)
        );
        let _: Value = serde_json::from_slice(&body)?;
        upstream.verify().await;

        let scrape = metrics_router(
            metrics,
            ConfigStatus::new(SourceKind::Etcd),
            &PrometheusConfig {
                enabled: true,
                path: "/metrics".into(),
                addr: "127.0.0.1:0".into(),
            },
            ModelsStatusState {
                store: InMemoryStore::new() as Arc<dyn ConfigStore>,
                runtime_status_tracker: None,
            },
        )
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;
        assert_eq!(scrape.status(), StatusCode::OK);
        let bytes = to_bytes(scrape.into_body(), 8 << 20).await?;
        let scrape = std::str::from_utf8(&bytes)?;
        let proxy = samples(scrape, "aisix_proxy_requests_total", endpoint);
        assert_eq!(proxy.len(), 1, "{verb} {endpoint}: {scrape}");
        assert!(proxy[0].contains("status=\"200\""), "{}", proxy[0]);
        assert_eq!(proxy[0].rsplit_once(' ').map(|(_, count)| count), Some("1"));
        let llm = samples(scrape, "aisix_llm_requests_total", endpoint);
        assert_eq!(
            llm.len(),
            usize::from(inference),
            "{verb} {endpoint}: {scrape}"
        );
        if inference {
            assert_eq!(llm[0].rsplit_once(' ').map(|(_, count)| count), Some("1"));
        }
        // 参数化路径不得把对象 ID 写进指标标签。
        assert!(!scrape.contains(RESPONSE));
        assert!(!scrape.contains("resp_native"));
    }
    Ok(())
}
