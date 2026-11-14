//! 本地组件回归，只验证续接准入和派发边界，不冒充提供方 E2E。
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_provider_openai::OpenAiBridge;
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";
const KEY: &str = "33333333-3333-4333-8333-333333333333";
const FOREIGN_KEY: &str = "33333333-3333-4333-8333-333333333334";

struct Registry {
    records: Mutex<HashMap<String, NativeRecord>>,
    submissions: Mutex<HashMap<Uuid, BeginRequest>>,
    lookups: AtomicUsize,
    fail: bool,
}

macro_rules! unused {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for Registry {
    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail {
                return Err(NativeRegistryError::Unavailable);
            }
            if request.api_key_id.to_string() != KEY {
                return Err(NativeRegistryError::NotFound);
            }
            let record = self
                .records
                .lock()
                .unwrap()
                .get(&request.locator.raw_id)
                .cloned()
                .ok_or(NativeRegistryError::NotFound)?;
            let mut reply = NativeReply::new("found");
            reply.record = Some(record);
            Ok(reply)
        })
    }
    fn begin(
        &self,
        request: BeginRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            if self.fail {
                return Err(NativeRegistryError::Unavailable);
            }
            let mut reply = NativeReply::new("pending_submission");
            reply.operation_id = Some(request.operation_id);
            reply.origin = Some(request.origin.clone());
            self.submissions
                .lock()
                .unwrap()
                .insert(request.operation_id, request);
            Ok(reply)
        })
    }
    fn bind(
        &self,
        request: BindRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let submitted = self
                .submissions
                .lock()
                .unwrap()
                .get(&request.operation_id)
                .cloned()
                .ok_or(NativeRegistryError::NotFound)?;
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id":Uuid::new_v4(), "operation_id":request.operation_id,
                "origin":submitted.origin, "raw_id":request.raw_id,
                "protocol":"openai", "kind":"response", "version":1,
                "requested_model_id":submitted.requested_model_id,
                "requested_model":submitted.requested_model,
                "upstream_model":submitted.upstream_model
            }))
            .unwrap();
            self.records
                .lock()
                .unwrap()
                .insert(record.raw_id.clone(), record.clone());
            let mut reply = NativeReply::new("bound");
            reply.record = Some(record);
            Ok(reply)
        })
    }
    fn claim(&self, _: ClaimRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Ok(NativeReply::new("leased")) })
    }
    unused!(owned, OwnedRequest);
    unused!(tombstone, TombstoneRequest);
    unused!(renew, LeaseRequest);
    unused!(reschedule, LeaseRequest);
    unused!(complete, CompleteRequest);
}

fn fixture(
    base: &str,
    with_registry: bool,
    fail: bool,
) -> (axum::Router, Arc<Registry>, SnapshotHandle<AisixSnapshot>) {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({"display_name":"native",
        "secret":"upstream-key", "provider":"openai", "api_base":base, "adapter":"openai"}))
    .unwrap();
    let platform =
        aisix_proxy::platform::PlatformSettings::from_secret("continuation-test-platform");
    let origin = OriginBinding {
        surface: None,
        model_id: MODEL.parse().unwrap(),
        provider_key_id: PROVIDER.parse().unwrap(),
        boundary: configured_native_origin(
            &platform.id_key,
            PROVIDER.parse().unwrap(),
            &provider,
            NativeProtocol::Openai,
            NativeObjectKind::Response,
        )
        .unwrap(),
    };
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model =
        serde_json::from_value(json!({"display_name":"native-model","provider":"openai",
        "model_name":"provider-model","provider_key_id":PROVIDER}))
        .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    for (id, bearer) in [(KEY, "caller"), (FOREIGN_KEY, "foreign")] {
        let key: ApiKey = serde_json::from_value(json!({"key_hash":ApiKey::hash_bearer(bearer),
            "allowed_models":["native-model"]}))
        .unwrap();
        snapshot.apikeys.insert(ResourceEntry::new(id, key, 1));
    }
    let record: NativeRecord =
        serde_json::from_value(json!({"record_id":Uuid::new_v4(),"raw_id":"resp_owned",
        "origin":origin,"protocol":"openai","kind":"response","requested_model_id":MODEL,
        "requested_model":"native-model","upstream_model":"provider-model","version":1}))
        .unwrap();
    let registry = Arc::new(Registry {
        records: Mutex::new(HashMap::from([("resp_owned".into(), record)])),
        submissions: Mutex::new(HashMap::new()),
        lookups: AtomicUsize::new(0),
        fail,
    });
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .unwrap();
    let handle = SnapshotHandle::new(snapshot);
    let hub = Arc::new(Hub::new());
    hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
    let mut state = ProxyState::new(handle.clone(), hub, &config)
        .without_cache()
        .with_platform(platform);
    if with_registry {
        state = state.with_native_objects(registry.clone());
    }
    (build_router(state), registry, handle)
}

async fn post(app: axum::Router, bearer: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), 1048576).await.unwrap()).unwrap();
    (status, body)
}

async fn answer(server: &MockServer, status: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id":"resp_next","object":"response",
            "status":status,"model":"provider-model","output":[],
            "usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}),
        ))
        .mount(server)
        .await;
}

#[tokio::test]
async fn owned_raw_previous_response_is_authorized_and_forwarded_after_a_new_router() {
    let upstream = MockServer::builder().start().await;
    answer(&upstream, "completed").await;
    let (app, registry, _) = fixture(&upstream.uri(), true, false);
    let (status, body) = post(
        app,
        "caller",
        json!({"model":"native-model","input":"continue",
        "previous_response_id":"resp_owned"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "resp_next");
    assert_eq!(registry.lookups.load(Ordering::SeqCst), 1);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["previous_response_id"], "resp_owned");
    assert_eq!(sent["model"], "provider-model");
}

#[tokio::test]
async fn a_foreign_or_unknown_raw_response_never_contacts_the_upstream() {
    let upstream = MockServer::builder().start().await;
    answer(&upstream, "completed").await;
    for (bearer, id) in [("foreign", "resp_owned"), ("caller", "resp_unknown")] {
        let (app, _, _) = fixture(&upstream.uri(), true, false);
        let (status, body) = post(
            app,
            bearer,
            json!({"model":"native-model","input":"continue","previous_response_id":id}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "previous_response_not_found");
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_registered_background_request_reaches_native_create() {
    let upstream = MockServer::builder().start().await;
    answer(&upstream, "queued").await;
    let (app, registry, _) = fixture(&upstream.uri(), true, false);
    let (status, body) = post(
        app,
        "caller",
        json!({"model":"native-model","input":"later","background":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "queued");
    let submitted = registry.submissions.lock().unwrap().clone();
    assert_eq!(submitted.len(), 1);
    assert!(submitted.values().next().unwrap().asynchronous);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn background_without_durable_authority_remains_refused() {
    let upstream = MockServer::builder().start().await;
    let (app, _, _) = fixture(&upstream.uri(), false, false);
    let (status, body) = post(
        app,
        "caller",
        json!({"model":"native-model","input":"later","background":true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["param"], "background");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn unavailable_authority_prevents_background_creation() {
    let upstream = MockServer::builder().start().await;
    let (app, _, _) = fixture(&upstream.uri(), true, true);
    let (status, body) = post(
        app,
        "caller",
        json!({"model":"native-model","input":"later","background":true}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn changed_account_boundary_refuses_even_an_owned_previous_response() {
    let upstream = MockServer::builder().start().await;
    answer(&upstream, "completed").await;
    let (app, registry, _) = fixture(&upstream.uri(), true, false);
    registry
        .records
        .lock()
        .unwrap()
        .get_mut("resp_owned")
        .unwrap()
        .origin
        .as_mut()
        .unwrap()
        .boundary = native_origin_digest(
        &[19; 32],
        PROVIDER.parse().unwrap(),
        NativeProtocol::Openai,
        "openai",
        "https://different-account.example.test/v1",
        "other-secret",
        &Default::default(),
    );
    let (status, body) = post(
        app,
        "caller",
        json!({"model":"native-model","input":"continue",
        "previous_response_id":"resp_owned"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(registry.lookups.load(Ordering::SeqCst), 1);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn admitting_background_does_not_admit_other_model_selectors() {
    let upstream = MockServer::builder().start().await;
    let (app, registry, _) = fixture(&upstream.uri(), true, false);
    let (status, body) = post(
        app,
        "caller",
        json!({"model":"native-model","input":"later",
        "background":true,"provider":{"order":["untrusted"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["param"], "provider");
    assert!(registry.submissions.lock().unwrap().is_empty());
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
