//! 真实 SQLite 归属和 HTTP 路由验证最终重命名槽；本地上游仅模拟 Responses 协议。
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_proxy::native_api::standalone_store::StandaloneNativeStore;
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "11111111-1111-4111-8111-111111114831";
const OTHER_MODEL: &str = "11111111-1111-4111-8111-111111114832";
const PROVIDER: &str = "22222222-2222-4222-8222-222222224831";
const OTHER_PROVIDER: &str = "22222222-2222-4222-8222-222222224832";
const KEY: &str = "33333333-3333-4333-8333-333333334831";
const OTHER_KEY: &str = "33333333-3333-4333-8333-333333334832";
// 固定旧版 v2 样本：本测试密钥、owner、MODEL 和首个上游 ID resp_rename_1。
const LEGACY_PREVIOUS: &str = concat!(
    "resp_CwsLCwsLCwsLCwsL9bqqv2NSmpFFg7-RqxjmfLLlUPxqSPo6WnvQLYuAWPrQWrpx2lLfGYtYdf2jVeb-",
    "a3isfDCkmvFlAdPn0KgWxtorMc9dg0HYEO6JoD8HJPeSsOJSDQ0ZKdpAMcPSsTNRnXrnYg9Umn0w_FElEsNmlI",
    "xCLyaLvN0dKZmcfDeaZDaj0OgxrNwZZmfd3REIng5OAfnoG1fUZDKr4MgoUspZf_B3B87srwKESBswBzZOKv6",
    "WsBKg02Y-jcmQkXbDB2gLLb_rfAFgG8NkFrC0Cpi6Uw"
);

struct Fixture {
    app: axum::Router,
    upstream: MockServer,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(authority: bool) -> Self {
        let upstream = MockServer::builder().start().await;
        let sequence = AtomicUsize::new(1);
        Mock::given(method("POST"))
            .and(path_regex("^/v1/responses(/compact|/input_tokens)?$"))
            .respond_with(move |request: &wiremock::Request| {
                let sent: Value = serde_json::from_slice(&request.body).unwrap();
                let id = format!("resp_rename_{}", sequence.fetch_add(1, Ordering::SeqCst));
                let response = json!({
                    "id":id,"object":"response","status":"completed",
                    "model":"upstream-model","output":[],
                    "usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}
                });
                if sent.get("stream").and_then(Value::as_bool) == Some(true) {
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(format!(
                            "event: response.completed\ndata: {}\n\n",
                            json!({"type":"response.completed","sequence_number":0,"response":response})
                        ))
                } else {
                    ResponseTemplate::new(200).set_body_json(response)
                }
            })
            .mount(&upstream)
            .await;

        let snapshot = AisixSnapshot::new();
        for (id, name) in [(PROVIDER, "first"), (OTHER_PROVIDER, "second")] {
            let document = json!({
                "display_name":name,"provider":"openai","adapter":"openai",
                "api_base":upstream.uri(),"api_key":"upstream-key",
                "request":{"param_renames":{"continuation":"previous_response_id"}}
            });
            aisix_core::validate_provider_key(&document).unwrap();
            let provider: ProviderKey = serde_json::from_value(document).unwrap();
            aisix_provider_openai::overrides::validate_content_safe_request_overrides(
                provider.request.as_ref().unwrap(),
            )
            .unwrap();
            snapshot
                .provider_keys
                .insert(ResourceEntry::new(id, provider, 1));
        }
        for (id, name, provider) in [
            (MODEL, "native-model", PROVIDER),
            (OTHER_MODEL, "other-native-model", OTHER_PROVIDER),
        ] {
            let document = json!({
                "display_name":name,"provider":"openai","model_name":"upstream-model",
                "provider_key_id":provider
            });
            aisix_core::validate_model(&document).unwrap();
            let model: Model = serde_json::from_value(document).unwrap();
            snapshot.models.insert(ResourceEntry::new(id, model, 1));
        }
        for (id, bearer) in [(KEY, "owner"), (OTHER_KEY, "other-owner")] {
            let document = json!({
                "key_hash":ApiKey::hash_bearer(bearer),
                "allowed_model_ids":[MODEL,OTHER_MODEL],"allowed_endpoints":["responses"]
            });
            aisix_core::validate_apikey(&document).unwrap();
            let key: ApiKey = serde_json::from_value(document).unwrap();
            snapshot.apikeys.insert(ResourceEntry::new(id, key, 1));
        }
        let handle = SnapshotHandle::new(snapshot);
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let database = directory.path().join("authority.sqlite3");
        drop(
            std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(&database)
                .unwrap(),
        );
        let platform = aisix_proxy::platform::PlatformSettings::from_secret("renamed-references");
        let store = StandaloneNativeStore::open(&database, platform.id_key, handle.clone())
            .await
            .unwrap_or_else(|_| panic!("protected SQLite authority must open"));
        let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
        let hub = Arc::new(Hub::new());
        hub.register_specialized(
            "openai",
            Arc::new(aisix_provider_openai::OpenAiBridge::new()),
        );
        let mut state = ProxyState::new(handle, hub, &config).without_cache();
        if authority {
            state = state
                .with_platform(platform)
                .with_native_objects(Arc::new(store));
        }
        Self {
            app: build_router(state),
            upstream,
            _directory: directory,
        }
    }

    async fn post(&self, bearer: &str, body: Value) -> (StatusCode, Vec<u8>) {
        self.post_at("/v1/responses", bearer, body).await
    }

    async fn post_at(&self, uri: &str, bearer: &str, body: Value) -> (StatusCode, Vec<u8>) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("authorization", format!("Bearer {bearer}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1048576)
            .await
            .unwrap()
            .to_vec();
        (status, body)
    }

    async fn create(&self, bearer: &str) -> String {
        let (status, bytes) = self
            .post(bearer, json!({"model":"native-model","input":"hello"}))
            .await;
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    async fn sent(&self) -> Vec<Value> {
        self.upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }
}

fn assert_unknown(status: StatusCode, bytes: &[u8]) {
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(bytes)
    );
    let body: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(body["error"]["code"], "previous_response_not_found");
    assert_eq!(body["error"]["param"], "previous_response_id");
}

#[tokio::test]
async fn renamed_foreign_previous_response_never_reaches_native_http_or_sse() {
    let fixture = Fixture::new(true).await;
    let foreign = fixture.create("owner").await;
    for stream in [false, true] {
        let (status, body) = fixture
            .post(
                "other-owner",
                json!({"model":"native-model","input":"continue",
                    "continuation":foreign,"stream":stream}),
            )
            .await;
        assert_unknown(status, &body);
    }
    assert_eq!(fixture.sent().await.len(), 1);
}

#[tokio::test]
async fn renamed_foreign_previous_response_never_reaches_compact_or_input_tokens() {
    let fixture = Fixture::new(true).await;
    let foreign = fixture.create("owner").await;
    for uri in ["/v1/responses/compact", "/v1/responses/input_tokens"] {
        let (status, body) = fixture
            .post_at(
                uri,
                "other-owner",
                json!({"model":"native-model","input":"continue","continuation":foreign}),
            )
            .await;
        assert_unknown(status, &body);
    }
    assert_eq!(fixture.sent().await.len(), 1);
}

#[tokio::test]
async fn renamed_previous_response_cannot_overwrite_an_admitted_owned_reference() {
    let fixture = Fixture::new(true).await;
    let foreign = fixture.create("owner").await;
    let own = fixture.create("other-owner").await;
    let (status, body) = fixture
        .post(
            "other-owner",
            json!({"model":"native-model","input":"continue",
                "previous_response_id":own,"continuation":foreign}),
        )
        .await;
    assert_unknown(status, &body);
    assert_eq!(fixture.sent().await.len(), 2);
}

#[tokio::test]
async fn renamed_owned_previous_response_stays_on_its_registered_account() {
    let fixture = Fixture::new(true).await;
    let own = fixture.create("owner").await;
    let (status, body) = fixture
        .post(
            "owner",
            json!({"model":"other-native-model","input":"continue","continuation":own}),
        )
        .await;
    assert_unknown(status, &body);
    assert_eq!(fixture.sent().await.len(), 1);
}

#[tokio::test]
async fn renamed_owned_previous_response_still_continues_http_and_sse() {
    let fixture = Fixture::new(true).await;
    let own = fixture.create("owner").await;
    for stream in [false, true] {
        let (status, body) = fixture
            .post(
                "owner",
                json!({"model":"native-model","input":"continue",
                    "continuation":own,"stream":stream}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        if stream {
            let text = std::str::from_utf8(&body).unwrap();
            assert!(text.contains("event: response.completed\n"), "{text}");
        } else {
            let response: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(response["status"], "completed");
        }
    }
    let sent = fixture.sent().await;
    assert_eq!(sent.len(), 3);
    for body in &sent[1..] {
        assert_eq!(body["previous_response_id"], own);
        assert!(body.get("continuation").is_none());
    }
}

#[tokio::test]
async fn renamed_legacy_previous_response_is_canonical_on_the_wire_for_its_owner() {
    let fixture = Fixture::new(true).await;
    let own = fixture.create("owner").await;
    assert_eq!(own, "resp_rename_1");
    let (status, body) = fixture
        .post(
            "other-owner",
            json!({"model":"native-model","input":"continue","continuation":LEGACY_PREVIOUS}),
        )
        .await;
    assert_unknown(status, &body);
    assert_eq!(fixture.sent().await.len(), 1);

    for stream in [false, true] {
        let (status, body) = fixture
            .post(
                "owner",
                json!({"model":"native-model","input":"continue",
                    "continuation":LEGACY_PREVIOUS,"stream":stream}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
    let sent = fixture.sent().await;
    assert_eq!(sent.len(), 3);
    for body in &sent[1..] {
        assert_eq!(body["previous_response_id"], own);
        assert!(body.get("continuation").is_none());
    }
}

#[tokio::test]
async fn renamed_previous_response_keeps_passthrough_without_platform_authority() {
    let fixture = Fixture::new(false).await;
    let (status, body) = fixture
        .post(
            "owner",
            json!({"model":"native-model","input":"continue","continuation":"resp_external"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let sent = fixture.sent().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["previous_response_id"], "resp_external");
}
