//! 原生 Files 组件测试：真实路由和 HTTP 上游，注册器替身只覆盖持久边界。
//! 不替代真实控制面数据库及提供商端到端验收。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
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

#[derive(Default)]
struct Registry {
    operations: Mutex<HashMap<Uuid, BeginRequest>>,
    records: Mutex<HashMap<String, NativeRecord>>,
    deleted: Mutex<Vec<String>>,
    fail_bind: bool,
    lookups: Mutex<Vec<LookupRequest>>,
    endpoint_grants: Mutex<HashMap<Uuid, Vec<EndpointFamily>>>,
}
macro_rules! unused {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}
impl NativeObjectRegistry for Registry {
    unused!(claim, ClaimRequest);
    unused!(renew, LeaseRequest);
    unused!(reschedule, LeaseRequest);
    unused!(complete, CompleteRequest);
    fn begin(
        &self,
        request: BeginRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut reply = NativeReply::new("pending_submission");
        reply.operation_id = Some(request.operation_id);
        reply.origin = Some(request.origin.clone());
        self.operations
            .lock()
            .unwrap()
            .insert(request.operation_id, request);
        Box::pin(async move { Ok(reply) })
    }
    fn bind(
        &self,
        request: BindRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            if self.fail_bind {
                return Err(NativeRegistryError::Unavailable);
            }
            let operations = self.operations.lock().unwrap();
            let operation = &operations[&request.operation_id];
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id": Uuid::new_v4(), "operation_id": request.operation_id,
                "api_key_id":request.api_key_id, "entries":operation.entries,
                "requested_model_id": operation.requested_model_id,
                "requested_model": operation.requested_model, "origin": operation.origin,
                "protocol": operation.protocol, "kind": operation.kind, "raw_id": request.raw_id,
                "version": 1
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
    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.lookups.lock().unwrap().push(request.clone());
        let family = request
            .consumer_family
            .unwrap_or(request.locator.kind.endpoint_family());
        let granted = self
            .endpoint_grants
            .lock()
            .unwrap()
            .get(&request.api_key_id)
            .is_none_or(|grants| grants.contains(&family));
        let context_allowed = request.consumer_family.is_none_or(|consumer| {
            request.locator.kind == NativeObjectKind::File
                && native_file_consumer_allowed(request.locator.protocol, consumer)
        });
        let record = self
            .records
            .lock()
            .unwrap()
            .get(&request.locator.raw_id)
            .filter(|record| {
                record.api_key_id == Some(request.api_key_id)
                    && !record.deleted
                    && granted
                    && context_allowed
            })
            .cloned();
        Box::pin(async move {
            let mut reply = NativeReply::new("found");
            reply.record = Some(record.ok_or(NativeRegistryError::NotFound)?);
            Ok(reply)
        })
    }
    fn owned(
        &self,
        request: OwnedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let records = self.records.lock().unwrap();
        let mut reply = NativeReply::new("owned");
        reply.records = request
            .raw_ids
            .iter()
            .filter_map(|id| records.get(id).cloned())
            .collect();
        Box::pin(async move { Ok(reply) })
    }
    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records
            .values_mut()
            .find(|r| r.record_id == request.record_id)
        {
            record.deleted = true;
            self.deleted.lock().unwrap().push(record.raw_id.clone());
        }
        Box::pin(async { Ok(NativeReply::new("deleted")) })
    }
}

fn app(base: &str, registry: Arc<Registry>) -> (axum::Router, SnapshotHandle<AisixSnapshot>) {
    let (state, snapshot) = app_state(base, registry);
    (build_router(state), snapshot)
}

fn app_state(base: &str, registry: Arc<Registry>) -> (ProxyState, SnapshotHandle<AisixSnapshot>) {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"native","secret":"upstream-key","provider":"anthropic",
        "api_base":base,"adapter":"anthropic"
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"native-model","provider":"anthropic","model_name":"provider-model",
        "provider_key_id":PROVIDER
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("caller"),"allowed_models":["native-model"]
    }))
    .unwrap();
    snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 1));
    let config: ProxyConfig = serde_json::from_value(json!({
        "addr":"127.0.0.1:0", "request_body_limit_bytes":1024*1024
    }))
    .unwrap();
    let snapshot = SnapshotHandle::new(snapshot);
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "anthropic",
        Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
    );
    let state = ProxyState::new(snapshot.clone(), hub, &config)
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "native-files-test-secret",
        ))
        .with_native_objects(registry);
    (state, snapshot)
}
async fn request(
    router: axum::Router,
    method: &str,
    uri: &str,
    multipart: bool,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-api-key", "caller")
        .header("anthropic-version", "2026-10-21")
        .header("anthropic-beta", "files-api-2025-04-14,custom-beta");
    let body = if multipart {
        request = request.header("content-type", "multipart/form-data; boundary=native-test");
        Body::from("--native-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\nfile payload\r\n--native-test--\r\n")
    } else {
        Body::empty()
    };
    router.oneshot(request.body(body).unwrap()).await.unwrap()
}
async fn body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
}
fn metadata(id: &str) -> Value {
    json!({"id":id,"type":"file","filename":"notes.txt","size_bytes":12,
        "mime_type":"text/plain","created_at":"2026-11-01T00:00:00Z","downloadable":false})
}

// 删除原生委派、误用 Bearer 或吞掉版本/beta/query 均使本场景失败。
#[tokio::test]
async fn upload_preserves_native_metadata_headers_and_forwarded_query() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    let response = request(
        router,
        "POST",
        "/v1/files?model=native-model&beta=true&x=a%2Fb&x=two",
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await, metadata("file_owned"));
    assert!(registry.records.lock().unwrap().contains_key("file_owned"));
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers["x-api-key"], "upstream-key");
    assert_eq!(requests[0].headers["anthropic-version"], "2026-10-21");
    assert_eq!(
        requests[0].headers["anthropic-beta"],
        "files-api-2025-04-14,custom-beta"
    );
    assert_eq!(requests[0].url.query(), Some("beta=true&x=a%2Fb&x=two"));
    assert!(String::from_utf8_lossy(&requests[0].body).contains("file payload"));
}
#[tokio::test]
async fn successful_upload_with_failed_durable_bind_releases_no_file_id() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_unbound")))
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry {
        fail_bind: true,
        ..Default::default()
    });
    let (router, _) = app(&upstream.uri(), registry);
    let response = request(router, "POST", "/v1/files?model=native-model", true).await;
    assert!(response.status().is_server_error());
    let answer = body(response).await;
    assert_eq!(answer["type"], "error");
    assert!(!answer.to_string().contains("file_unbound"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}
#[tokio::test]
async fn unknown_file_is_404_without_an_upstream_probe() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let response = request(
        router,
        "GET",
        "/v1/files/file_foreign?model=native-model",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let answer = body(response).await;
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["error"]["type"], "not_found_error");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn upload_requires_explicit_model_instead_of_selecting_an_account() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let response = request(router, "POST", "/v1/files", true).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body(response).await["type"], "error");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn list_filters_foreign_ids_and_returns_only_owned_cursors() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data":[metadata("file_foreign"),metadata("file_owned")],
            "next_page":null
        })))
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry);
    assert_eq!(
        request(router.clone(), "POST", "/v1/files?model=native-model", true)
            .await
            .status(),
        StatusCode::OK
    );
    let response = request(router, "GET", "/v1/files?model=native-model&limit=2", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let answer = body(response).await;
    assert_eq!(answer["data"], json!([metadata("file_owned")]));
    assert!(answer.get("next_page").is_some());
    assert!(answer["next_page"].is_null());
    assert!(!answer.to_string().contains("file_foreign"));
}
#[tokio::test]
async fn deleting_file_tombstones_it_before_the_next_request() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
        .mount(&upstream)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/files/file_owned"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"file_owned","type":"file_deleted"})),
        )
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    assert_eq!(
        request(router.clone(), "POST", "/v1/files?model=native-model", true)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(router.clone(), "DELETE", "/v1/files/file_owned", false)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(*registry.deleted.lock().unwrap(), vec!["file_owned"]);
    assert_eq!(
        request(router, "GET", "/v1/files/file_owned", false)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn list_page_cursor_is_owner_and_account_bound_and_roundtrips_upstream() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/v1/files"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[],"next_page":"page_upstream_secret"})),
        )
        .mount(&upstream)
        .await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let response = request(
        router.clone(),
        "GET",
        "/v1/files?model=native-model&limit=2",
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let answer = body(response).await;
    let cursor = answer["next_page"].as_str().unwrap();
    assert_ne!(cursor, "page_upstream_secret");
    let uri = url::form_urlencoded::Serializer::new(String::from("/v1/files?"))
        .append_pair("model", "native-model")
        .append_pair("page", cursor)
        .finish();
    assert_eq!(
        request(router.clone(), "GET", &uri, false).await.status(),
        StatusCode::OK
    );
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .url
        .query_pairs()
        .any(|(key, value)| key == "page" && value == "page_upstream_secret"));
    let invalid = request(
        router,
        "GET",
        "/v1/files?model=native-model&page=page_upstream_secret",
        false,
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn rotated_credentials_cannot_reach_the_original_file() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
        .mount(&upstream)
        .await;
    let (router, snapshot) = app(&upstream.uri(), Arc::new(Registry::default()));
    assert_eq!(
        request(router.clone(), "POST", "/v1/files?model=native-model", true)
            .await
            .status(),
        StatusCode::OK
    );
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"native","secret":"rotated-credential","provider":"anthropic",
        "api_base":upstream.uri(),"adapter":"anthropic"
    }))
    .unwrap();
    snapshot
        .load()
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 2));
    let response = request(router, "GET", "/v1/files/file_owned", false).await;
    assert!(response.status().is_server_error());
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn client_cannot_select_an_unbound_workspace() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/v1/files?model=native-model")
                .header("x-api-key", "caller")
                .header("anthropic-version", "2026-10-21")
                .header("anthropic-workspace-id", "wrkspc_other")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body(response).await["type"], "error");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn anthropic_file_auth_rejection_uses_native_error_shape() {
    let upstream = MockServer::builder().start().await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/v1/files?model=native-model")
                .header("x-api-key", "invalid")
                .header("anthropic-version", "2026-10-21")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let answer = body(response).await;
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["error"]["type"], "authentication_error");
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn owned_uploaded_file_can_be_reused_by_messages_and_count_tokens() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"msg_native","type":"message","role":"assistant","model":"provider-model",
            "content":[{"type":"text","text":"read"}],"stop_reason":"end_turn","stop_sequence":null,
            "usage":{"input_tokens":7,"output_tokens":1}
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens":7})))
        .mount(&upstream)
        .await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    assert_eq!(
        request(router.clone(), "POST", "/v1/files?model=native-model", true)
            .await
            .status(),
        StatusCode::OK
    );
    for route in ["/v1/messages", "/v1/messages/count_tokens"] {
        let payload = json!({"model":"native-model","max_tokens":10,"messages":[{"role":"user",
            "content":[{"type":"document","source":{"type":"file","file_id":"file_owned"}}]}]});
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(route)
                    .header("x-api-key", "caller")
                    .header("anthropic-version", "2026-10-21")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let answer = body(response).await;
        assert_eq!(status, StatusCode::OK, "{route}: {answer}");
    }
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            payload["messages"][0]["content"][0]["source"]["file_id"],
            "file_owned"
        );
        assert_eq!(payload["model"], "provider-model");
    }
}

#[tokio::test]
async fn multipart_upload_uses_one_request_quota_for_all_fields() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
        .mount(&upstream)
        .await;
    let (router, snapshot) = app(&upstream.uri(), Arc::new(Registry::default()));
    let key = serde_json::from_value(json!({
        "key_hash": ApiKey::hash_bearer("caller"), "allowed_models": ["native-model"],
        "rate_limit": {"rpd": 1}
    }))
    .unwrap();
    snapshot
        .load()
        .apikeys
        .insert(ResourceEntry::new(KEY, key, 2));
    let payload = "--native-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\nfile payload\r\n--native-test\r\nContent-Disposition: form-data; name=\"expires_in_seconds\"\r\n\r\n3600\r\n--native-test--\r\n";
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/files?model=native-model")
                .header("x-api-key", "caller")
                .header("anthropic-version", "2026-10-21")
                .header("content-type", "multipart/form-data; boundary=native-test")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(status, StatusCode::OK, "{}", body(response).await);
    assert_eq!(
        request(router, "POST", "/v1/files?model=native-model", true)
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent = String::from_utf8_lossy(&requests[0].body);
    assert!(sent.contains("expires_in_seconds"));
    assert!(sent.contains("3600"));
}

#[tokio::test]
async fn addressed_file_preserves_metadata_and_binary_content() {
    let upstream = MockServer::builder().start().await;
    for route in ["/v1/files", "/v1/files/file_owned"] {
        Mock::given(method(if route == "/v1/files" { "POST" } else { "GET" }))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
            .mount(&upstream)
            .await;
    }
    let payload = vec![0, 255, 1, 0, 192, 80];
    Mock::given(method("GET"))
        .and(path("/v1/files/file_owned/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(payload.clone()),
        )
        .mount(&upstream)
        .await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    assert_eq!(
        request(router.clone(), "POST", "/v1/files?model=native-model", true)
            .await
            .status(),
        StatusCode::OK
    );
    let response = request(router.clone(), "GET", "/v1/files/file_owned", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await, metadata("file_owned"));
    let response = request(router, "GET", "/v1/files/file_owned/content", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(
        to_bytes(response.into_body(), 65536)
            .await
            .unwrap()
            .as_ref(),
        payload
    );
}

#[tokio::test]
async fn native_file_request_error_keeps_contract_and_scrubs_sent_credentials() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "type": "error", "error": {"type": "request_too_large", "message": "file exceeds limit; x-api-key: upstream-key"}
        }))).mount(&upstream).await;
    let (router, _) = app(&upstream.uri(), Arc::new(Registry::default()));
    let response = request(router, "POST", "/v1/files?model=native-model", true).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let answer = body(response).await;
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["error"]["type"], "request_too_large");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap()
        .contains("file exceeds limit"));
    assert!(!answer.to_string().contains("upstream-key"));
}

#[tokio::test]
async fn generated_native_file_is_bound_before_it_can_be_downloaded() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"msg_native","type":"message","role":"assistant","model":"provider-model",
            "content":[{"type":"bash_code_execution_tool_result","tool_use_id":"srvtool_1",
                "content":{"type":"bash_code_execution_result","return_code":0,"stdout":"","stderr":"",
                    "content":[{"type":"bash_code_execution_output","file_id":"file_generated"}]}}],
            "stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":1}
        }))).mount(&upstream).await;
    let payload = vec![255, 0, 1];
    Mock::given(method("GET"))
        .and(path("/v1/files/file_generated/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
        .mount(&upstream)
        .await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(&upstream.uri(), registry.clone());
    let response = router.clone().oneshot(Request::builder().method("POST").uri("/v1/messages")
        .header("x-api-key","caller").header("anthropic-version","2026-10-21")
        .header("content-type","application/json")
        .body(Body::from(json!({"model":"native-model","max_tokens":10,"messages":[{"role":"user","content":"produce a file"}]}).to_string())).unwrap())
        .await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let answer = body(response).await;
    assert_eq!(
        answer["content"][0]["content"]["content"][0]["file_id"],
        "file_generated"
    );
    assert!(registry
        .records
        .lock()
        .unwrap()
        .contains_key("file_generated"));
    let response = request(router, "GET", "/v1/files/file_generated/content", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 65536)
            .await
            .unwrap()
            .as_ref(),
        payload
    );
}

#[tokio::test]
async fn sealed_file_page_cannot_be_reused_by_another_owner() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data":[],"next_page":"page_private"
        })))
        .mount(&upstream)
        .await;
    let (router, snapshot) = app(&upstream.uri(), Arc::new(Registry::default()));
    let response = request(router.clone(), "GET", "/v1/files?model=native-model", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let answer = body(response).await;
    let other = serde_json::from_value(json!({
        "key_hash": ApiKey::hash_bearer("other-caller"), "allowed_models": ["native-model"]
    }))
    .unwrap();
    snapshot.load().apikeys.insert(ResourceEntry::new(
        "44444444-4444-4444-8444-444444444444",
        other,
        1,
    ));
    let uri = url::form_urlencoded::Serializer::new(String::from("/v1/files?"))
        .append_pair("model", "native-model")
        .append_pair("page", answer["next_page"].as_str().unwrap())
        .finish();
    let response = router
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("x-api-key", "other-caller")
                .header("anthropic-version", "2026-10-21")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

// 生成进行中的账号轮换不能把旧账号创建的文件登记到新账号。
#[tokio::test]
async fn generated_file_keeps_the_dispatch_origin_across_credential_rotation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (arrived, received) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0u8; 1024];
            let length = socket.read(&mut chunk).await.unwrap();
            assert!(length > 0);
            request.extend_from_slice(&chunk[..length]);
            assert!(request.len() < 65536);
        }
        assert!(String::from_utf8_lossy(&request).contains("upstream-key"));
        arrived.send(()).unwrap();
        released.await.unwrap();
        let payload = json!({"id":"msg_generated","type":"message","role":"assistant","model":"provider-model",
            "content":[{"type":"container_upload","file_id":"file_during_rotation"}],
            "stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":1}}).to_string();
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",payload.len());
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let base = format!("http://{address}");
    let registry = Arc::new(Registry::default());
    let (router, snapshot) = app(&base, registry.clone());
    let call = tokio::spawn(async move {
        router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", "caller")
                    .header("anthropic-version", "2026-10-21")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"native-model","max_tokens":10,
                "messages":[{"role":"user","content":"produce a file"}]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), received)
        .await
        .unwrap()
        .unwrap();
    let provider = serde_json::from_value(json!({
        "display_name":"native","secret":"rotated-key","provider":"anthropic",
        "api_base":base,"adapter":"anthropic"
    }))
    .unwrap();
    snapshot
        .load()
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 2));
    release.send(()).unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), call)
        .await
        .unwrap()
        .unwrap();
    assert!(response.status().is_server_error());
    assert!(!body(response)
        .await
        .to_string()
        .contains("file_during_rotation"));
    assert!(!registry
        .records
        .lock()
        .unwrap()
        .contains_key("file_during_rotation"));
    peer.await.unwrap();
}

fn consumer_request(route: &str, family: EndpointFamily, file_id: &str) -> Request<Body> {
    let params = json!({"model":"native-model","max_tokens":10,"messages":[{"role":"user",
        "content":[{"type":"document","source":{"type":"file","file_id":file_id}}]}]});
    let payload = if family == EndpointFamily::Batches {
        json!({"requests":[{"custom_id":"entry-file","params":params}]})
    } else {
        params
    };
    Request::builder()
        .method("POST")
        .uri(route)
        .header("x-api-key", "caller")
        .header("anthropic-version", "2026-10-21")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

#[tokio::test]
async fn file_references_require_the_actual_consumer_grant_without_granting_file_routes() {
    for (family, route, name) in [
        (EndpointFamily::Messages, "/v1/messages", "messages"),
        (
            EndpointFamily::CountTokens,
            "/v1/messages/count_tokens",
            "count_tokens",
        ),
        (EndpointFamily::Batches, "/v1/messages/batches", "batches"),
    ] {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
            .expect(1)
            .mount(&upstream)
            .await;
        let native_answer = match family {
            EndpointFamily::Messages => {
                json!({"id":"msg_file_consumer","type":"message","role":"assistant","model":"provider-model",
                "content":[{"type":"text","text":"read"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":1}})
            }
            EndpointFamily::CountTokens => json!({"input_tokens":7}),
            _ => {
                json!({"id":"msgbatch_file_consumer","type":"message_batch","processing_status":"in_progress",
                "request_counts":{"processing":1,"succeeded":0,"errored":0,"canceled":0,"expired":0},
                "ended_at":null,"created_at":"2026-11-01T00:00:00Z","expires_at":"2026-11-02T00:00:00Z",
                "cancel_initiated_at":null,"results_url":null})
            }
        };
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(native_answer))
            .expect(1)
            .mount(&upstream)
            .await;
        let registry = Arc::new(Registry::default());
        let (router, snapshot) = app(&upstream.uri(), registry.clone());
        assert_eq!(
            request(router.clone(), "POST", "/v1/files?model=native-model", true)
                .await
                .status(),
            200
        );
        let mut key = (*snapshot.load().apikeys.get_by_id(KEY).unwrap().value).clone();
        key.allowed_endpoints = Some(vec![family]);
        snapshot
            .load()
            .apikeys
            .insert(ResourceEntry::new(KEY, key, 2));
        registry
            .endpoint_grants
            .lock()
            .unwrap()
            .insert(Uuid::parse_str(KEY).unwrap(), vec![family]);
        {
            let mut records = registry.records.lock().unwrap();
            let mut foreign = records["file_owned"].clone();
            foreign.record_id = Uuid::new_v4();
            foreign.raw_id = "file_foreign".into();
            foreign.api_key_id =
                Some(Uuid::parse_str("44444444-4444-4444-8444-444444444444").unwrap());
            records.insert(foreign.raw_id.clone(), foreign);
        }
        let response = router
            .clone()
            .oneshot(consumer_request(route, family, "file_owned"))
            .await
            .unwrap();
        let status = response.status();
        let answer = body(response).await;
        assert_eq!(status, 200, "{name}: {answer}");
        assert_eq!(
            registry
                .lookups
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .consumer_family,
            Some(family),
            "{name}"
        );
        let consumer_calls = upstream.received_requests().await.unwrap();
        assert_eq!(consumer_calls.len(), 2, "{name}");
        let sent: Value = serde_json::from_slice(&consumer_calls[1].body).unwrap();
        let params = if family == EndpointFamily::Batches {
            &sent["requests"][0]["params"]
        } else {
            &sent
        };
        assert_eq!(
            params["messages"][0]["content"][0]["source"]["file_id"], "file_owned",
            "{name}"
        );
        for direct in [
            "/v1/files?model=native-model",
            "/v1/files/file_owned",
            "/v1/files/file_owned/content",
        ] {
            assert_eq!(
                request(router.clone(), "GET", direct, false).await.status(),
                403,
                "{name}: {direct}"
            );
        }
        let foreign = router
            .oneshot(consumer_request(route, family, "file_foreign"))
            .await
            .unwrap();
        assert_eq!(foreign.status(), 404, "{name}: {}", body(foreign).await);
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            2,
            "{name}"
        );
        upstream.verify().await;
    }
}

#[tokio::test]
async fn native_file_model_quota_rejections_keep_resolved_attribution_on_every_route() {
    for (verb, route, multipart) in [
        ("POST", "/v1/files?model=native-model", true),
        ("GET", "/v1/files?model=native-model", false),
        ("GET", "/v1/files/file_owned", false),
    ] {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata("file_owned")))
            .expect(if verb == "POST" { 2 } else { 1 })
            .mount(&upstream)
            .await;
        if verb == "GET" {
            let answer = if route.contains('?') {
                json!({"data":[metadata("file_owned")],"next_page":null})
            } else {
                metadata("file_owned")
            };
            Mock::given(method("GET"))
                .and(path(route.split('?').next().unwrap()))
                .respond_with(ResponseTemplate::new(200).set_body_json(answer))
                .expect(1)
                .mount(&upstream)
                .await;
        }
        let (state, snapshot) = app_state(&upstream.uri(), Arc::new(Registry::default()));
        // 先走上传建立可读文件；这次准备调用尚未配置模型限额。
        let seeded = request(
            build_router(state.clone()),
            "POST",
            "/v1/files?model=native-model",
            true,
        )
        .await;
        assert_eq!(seeded.status(), StatusCode::OK);
        assert_eq!(body(seeded).await, metadata("file_owned"));
        let entry = snapshot.load().models.get_by_id(MODEL).unwrap();
        let mut limited = (*entry.value).clone();
        limited.rate_limit = Some(serde_json::from_value(json!({"rpd":1})).unwrap());
        snapshot
            .load()
            .models
            .insert(ResourceEntry::new(MODEL, limited, 2));
        let (tx, mut events) = tokio::sync::mpsc::channel(8);
        let router = build_router(state.with_usage_sink(aisix_obs::UsageSink::new(tx)));
        let allowed = request(router.clone(), verb, route, multipart).await;
        assert_eq!(allowed.status(), StatusCode::OK, "{verb} {route}");
        body(allowed).await;
        let allowed_event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(allowed_event.model_id, MODEL);
        assert_eq!(allowed_event.upstream_dispatched, Some(true));
        assert_eq!(upstream.received_requests().await.unwrap().len(), 2);

        let denied = request(router, verb, route, multipart).await;
        assert_eq!(
            denied.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "{verb} {route}"
        );
        assert_eq!(body(denied).await["type"], "error");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            2,
            "quota rejection must not dispatch: {verb} {route}"
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.status_code, 429);
        assert_eq!(event.model_id, MODEL, "{verb} {route}");
        assert_eq!(event.requested_model, "native-model");
        assert_eq!(event.requested_model_id, MODEL);
        assert_eq!(event.provider_key_id.as_deref(), Some(PROVIDER));
        assert_eq!(event.upstream_dispatched, Some(false));
        assert_eq!(
            (
                event.prompt_tokens,
                event.completion_tokens,
                event.total_tokens
            ),
            (0, 0, 0)
        );
        assert_eq!(event.cost_usd, 0.0);
        assert!(event.is_final_attempt);
        assert!(!event.error_class.is_empty());
        assert!(events.try_recv().is_err());
    }
}

async fn generated_stream_file_is_bound_before_release(fail_bind: bool) {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry {
        fail_bind,
        ..Registry::default()
    });
    let (router, _) = app(&upstream.uri(), registry.clone());
    let file_id = "file_generated_stream_observation";
    // 官方执行结果块中的文件槽；普通文本与任意 metadata 不作为产物。
    let events = [
        json!({"type":"message_start","message":{
            "id":"msg_generated_stream","type":"message","role":"assistant",
            "model":"provider-model","content":[],"usage":{"input_tokens":5,"output_tokens":0}
        }}),
        json!({"type":"content_block_start","index":0,"content_block":{
            "type":"bash_code_execution_tool_result","tool_use_id":"srvtoolu_generated_stream",
            "content":{"type":"bash_code_execution_result","stdout":"","stderr":"","return_code":0,
                "content":[{"type":"bash_code_execution_output","file_id":file_id}]}
        }}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ];
    let sse = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "caller")
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"native-model","stream":true,"max_tokens":32,
                        "messages":[{"role":"user","content":"Execute the supplied task"}]})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut chunks = response.into_body().into_data_stream();
    let mut received = Vec::new();
    let mut saw_file_frame = false;
    while let Some(chunk) = futures::StreamExt::next(&mut chunks).await {
        received.extend_from_slice(&chunk.unwrap());
        if String::from_utf8_lossy(&received).contains(file_id) {
            // 在继续 poll 到下一帧前核持久边界，事后登记不能通过此断言。
            let records = registry.records.lock().unwrap();
            let record = records
                .get(file_id)
                .expect("file frame released before bind");
            assert_eq!(record.api_key_id, Some(Uuid::parse_str(KEY).unwrap()));
            assert_eq!(record.kind, Some(NativeObjectKind::File));
            saw_file_frame = true;
        }
    }
    let operations = registry.operations.lock().unwrap();
    assert_eq!(
        operations.len(),
        1,
        "output registration must actually begin"
    );
    assert_eq!(
        operations.values().next().unwrap().kind,
        NativeObjectKind::File
    );
    if fail_bind {
        assert!(!saw_file_frame);
        assert!(!String::from_utf8_lossy(&received).contains(file_id));
        assert!(registry.records.lock().unwrap().is_empty());
    } else {
        assert!(
            saw_file_frame,
            "successful bind must preserve the original file id"
        );
        assert_eq!(registry.records.lock().unwrap().len(), 1);
    }
}

// 删除流中的产物观察会同时丢失登记，不能靠省掉整个分支优化解析次数。
#[tokio::test]
async fn streamed_generated_file_is_bound_before_its_id_is_released() {
    generated_stream_file_is_bound_before_release(false).await;
}

#[tokio::test]
async fn streamed_generated_file_is_withheld_when_binding_fails() {
    generated_stream_file_is_bound_before_release(true).await;
}
