//! 官方对象协议组件测试；本地 HTTP 适配器不替代真实提供商端到端验收。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

const MODEL: &str = "81111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "82222222-2222-4222-8222-222222222222";
const KEY: &str = "83333333-3333-4333-8333-333333333333";
const OTHER: &str = "84444444-4444-4444-8444-444444444444";

#[derive(Default)]
struct Registry {
    operations: Mutex<HashMap<Uuid, BeginRequest>>,
    records: Mutex<HashMap<(String, String, String), (Uuid, NativeRecord)>>,
    accepted: Mutex<Vec<BeginRelatedRequest>>,
    fail_bind: bool,
}

macro_rules! unavailable {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for Registry {
    unavailable!(claim, ClaimRequest);
    unavailable!(renew, LeaseRequest);
    unavailable!(reschedule, LeaseRequest);
    unavailable!(complete, CompleteRequest);

    fn begin_related(
        &self,
        request: BeginRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let records = self.records.lock().unwrap();
        let permitted = request.references.iter().all(|reference| {
            records.values().any(|(owner, record)| {
                *owner == request.submission.api_key_id
                    && record.record_id == reference.record_id
                    && !record.deleted
            })
        });
        drop(records);
        if !permitted {
            return Box::pin(async { Err(NativeRegistryError::NotFound) });
        }
        self.accepted.lock().unwrap().push(request.clone());
        self.begin(request.submission)
    }

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
            let operation = self.operations.lock().unwrap()[&request.operation_id].clone();
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id":Uuid::new_v4(),"operation_id":request.operation_id,
                "api_key_id":request.api_key_id,"requested_model_id":operation.requested_model_id,
                "requested_model":operation.requested_model,"origin":operation.origin,
                "protocol":operation.protocol,"kind":operation.kind,"raw_id":request.raw_id,
                "parent_id":operation.parent_id,"version":1
            }))
            .unwrap();
            let key = (
                operation.protocol.as_str().into(),
                operation.kind.as_str().into(),
                request.raw_id,
            );
            self.records
                .lock()
                .unwrap()
                .insert(key, (request.api_key_id, record.clone()));
            let mut reply = NativeReply::new("bound");
            reply.record = Some(record);
            Ok(reply)
        })
    }

    fn bind_related(
        &self,
        request: BindRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let operation = self.operations.lock().unwrap()[&request.binding.operation_id].clone();
        let existing = self
            .records
            .lock()
            .unwrap()
            .get(&(
                operation.protocol.as_str().into(),
                operation.kind.as_str().into(),
                request.binding.raw_id.clone(),
            ))
            .cloned();
        if operation.kind.as_str() == "message_container" {
            if let Some((owner, record)) = existing {
                let accepted = self.accepted.lock().unwrap();
                let permitted = accepted
                    .iter()
                    .find(|request| request.submission.operation_id == operation.operation_id)
                    .is_some_and(|request| {
                        request
                            .references
                            .iter()
                            .any(|reference| reference.record_id == record.record_id)
                    });
                let valid = owner == request.binding.api_key_id && permitted && !record.deleted;
                return Box::pin(async move {
                    if !valid {
                        return Err(NativeRegistryError::NotFound);
                    }
                    let mut reply = NativeReply::new("bound");
                    reply.record = Some(record);
                    Ok(reply)
                });
            }
        }
        self.bind(request.binding)
    }

    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let key = (
            request.locator.protocol.as_str().into(),
            request.locator.kind.as_str().into(),
            request.locator.raw_id,
        );
        let record = self
            .records
            .lock()
            .unwrap()
            .get(&key)
            .filter(|(owner, record)| {
                *owner == request.api_key_id
                    && !record.deleted
                    && request
                        .locator
                        .origin
                        .as_ref()
                        .is_none_or(|origin| record.origin.as_ref() == Some(origin))
                    && request
                        .locator
                        .parent_id
                        .is_none_or(|parent| record.parent_id == Some(parent))
            })
            .map(|(_, record)| record.clone());
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
            .filter_map(|id| {
                records.get(&(
                    request.protocol.as_str().into(),
                    request.kind.as_str().into(),
                    id.clone(),
                ))
            })
            .filter(|(owner, record)| {
                *owner == request.api_key_id
                    && !record.deleted
                    && record.origin.as_ref() == Some(&request.origin)
            })
            .map(|(_, record)| record.clone())
            .collect();
        Box::pin(async move { Ok(reply) })
    }

    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        for (owner, record) in self.records.lock().unwrap().values_mut() {
            if *owner == request.api_key_id
                && record.record_id == request.record_id
                && record.version == request.version
            {
                record.deleted = true;
            }
        }
        Box::pin(async { Ok(NativeReply::new("deleted")) })
    }
}

fn app(
    protocol: &str,
    base: &str,
    registry: Arc<Registry>,
) -> (axum::Router, SnapshotHandle<AisixSnapshot>) {
    configured_app(protocol, base, registry, |_| {})
}

fn configured_app(
    protocol: &str,
    base: &str,
    registry: Arc<Registry>,
    configure: impl FnOnce(&AisixSnapshot),
) -> (axum::Router, SnapshotHandle<AisixSnapshot>) {
    let snapshot = AisixSnapshot::new();
    let api_base = if protocol == "openai" {
        format!("{base}/v1")
    } else {
        base.into()
    };
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"native","api_key":"upstream-key","provider":protocol,
        "api_base":api_base,"adapter":protocol
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"native-model","provider":protocol,"model_name":"provider-model","provider_key_id":PROVIDER
    })).unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    for (id, bearer) in [(KEY, "caller"), (OTHER, "other")] {
        let key: ApiKey = serde_json::from_value(
            json!({"key_hash":ApiKey::hash_bearer(bearer),"allowed_models":["native-model"]}),
        )
        .unwrap();
        snapshot.apikeys.insert(ResourceEntry::new(id, key, 1));
    }
    configure(&snapshot);
    let snapshot = SnapshotHandle::new(snapshot);
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1_048_576}))
            .unwrap();
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    hub.register_specialized(
        "anthropic",
        Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
    );
    let state = ProxyState::new(snapshot.clone(), hub, &config)
        .without_cache()
        .with_guardrail_index(guardrails)
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "skills-voices-test",
        ))
        .with_native_objects(registry);
    (build_router(state), snapshot)
}

async fn request(
    router: axum::Router,
    protocol: &str,
    verb: &str,
    uri: &str,
    bearer: &str,
    payload: Option<(&str, Vec<u8>)>,
) -> axum::response::Response {
    let mut req = Request::builder()
        .method(verb)
        .uri(uri)
        .header("x-aisix-model", "native-model");
    if protocol == "anthropic" {
        req = req
            .header("x-api-key", bearer)
            .header("anthropic-version", "2026-10-21");
    } else {
        req = req.header("authorization", format!("Bearer {bearer}"));
    }
    let body = if let Some((content_type, bytes)) = payload {
        req = req.header("content-type", content_type);
        Body::from(bytes)
    } else {
        Body::empty()
    };
    router.oneshot(req.body(body).unwrap()).await.unwrap()
}

fn form(parts: &[(&str, Option<&str>, &[u8])]) -> (&'static str, Vec<u8>) {
    let mut bytes = Vec::new();
    for (name, filename, body) in parts {
        let filename = filename
            .map(|name| format!("; filename=\"{name}\""))
            .unwrap_or_default();
        bytes.extend_from_slice(
            format!("--sv12\r\nContent-Disposition: form-data; name=\"{name}\"{filename}\r\n\r\n")
                .as_bytes(),
        );
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"--sv12--\r\n");
    ("multipart/form-data; boundary=sv12", bytes)
}

fn json_payload(value: Value) -> (&'static str, Vec<u8>) {
    ("application/json", value.to_string().into_bytes())
}

async fn answer(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1_048_576).await.unwrap()).unwrap()
}

fn skill(protocol: &str, id: &str) -> Value {
    if protocol == "anthropic" {
        json!({"id":id,"type":"skill","display_name":"Marker Skill","source":{"type":"custom"},"created_at":"2026-11-01T00:00:00Z","updated_at":"2026-11-01T00:00:00Z","latest_version_id":"skver_marker"})
    } else {
        json!({"id":id,"object":"skill","name":"marker","description":"Marker Skill","created_at":1793491200,"default_version":"1","latest_version":"2"})
    }
}

async fn mount_json(server: &MockServer, verb: &str, route: &str, value: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(server)
        .await;
}

async fn skill_contract(protocol: &str) {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app(protocol, &upstream.uri(), registry.clone());
    let root = "/v1/skills/skill_owned";
    let metadata = skill(protocol, "skill_owned");
    mount_json(&upstream, "POST", "/v1/skills", metadata.clone()).await;
    let response = request(
        router.clone(),
        protocol,
        "POST",
        "/v1/skills",
        "caller",
        Some(form(&[
            (
                "files",
                Some("marker/SKILL.md"),
                b"---\nname: marker\ndescription: marker\n---\nMARKER_V1",
            ),
            ("files", Some("marker/data.bin"), b"\x00\xff\x01"),
        ])),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{protocol} skill creation"
    );
    assert_eq!(answer(response).await, metadata);
    assert!(registry.records.lock().unwrap().contains_key(&(
        protocol.into(),
        "skill".into(),
        "skill_owned".into()
    )));
    mount_json(&upstream, "GET", root, metadata.clone()).await;
    let response = request(router.clone(), protocol, "GET", root, "caller", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, metadata);
    let response = request(router.clone(), protocol, "GET", root, "other", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!answer(response).await.to_string().contains("Marker Skill"));

    let version = if protocol == "anthropic" {
        "skver_marker"
    } else {
        "2"
    };
    let version_meta = if protocol == "anthropic" {
        json!({"id":version,"type":"skill_version","skill_id":"skill_owned","created_at":"2026-11-01T00:00:00Z","name":"marker","description":"Marker Skill","directory":"marker"})
    } else {
        json!({"id":"skver_marker","object":"skill.version","skill_id":"skill_owned","version":version,"created_at":1793491200,"name":"marker","description":"Marker Skill"})
    };
    let versions = format!("{root}/versions");
    mount_json(&upstream, "POST", &versions, version_meta.clone()).await;
    let response = request(
        router.clone(),
        protocol,
        "POST",
        &versions,
        "caller",
        Some(form(&[(
            "files",
            Some("marker/SKILL.md"),
            b"---\nname: marker\ndescription: marker\n---\nMARKER_V2",
        )])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, version_meta);
    let version_path = format!("{versions}/{version}");
    mount_json(&upstream, "GET", &version_path, version_meta.clone()).await;
    let response = request(
        router.clone(),
        protocol,
        "GET",
        &version_path,
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, version_meta);
    let version_list = if protocol == "anthropic" {
        json!({"data":[version_meta],"next_page":null})
    } else {
        json!({"data":[version_meta],"object":"list","first_id":"skver_marker","last_id":"skver_marker","has_more":false})
    };
    mount_json(&upstream, "GET", &versions, version_list.clone()).await;
    let response = request(router.clone(), protocol, "GET", &versions, "caller", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, version_list);
    let content = format!("{version_path}/content");
    Mock::given(method("GET"))
        .and(path(&content))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(b"PK\x03\x04\x00\xffMARKER_V2"),
        )
        .mount(&upstream)
        .await;
    let response = request(router.clone(), protocol, "GET", &content, "caller", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
        b"PK\x03\x04\x00\xffMARKER_V2"
    );
    let deleted_version = if protocol == "anthropic" {
        json!({"id":version,"type":"skill_version_deleted"})
    } else {
        json!({"id":"skver_marker","version":version,"object":"skill.version.deleted","deleted":true})
    };
    mount_json(&upstream, "DELETE", &version_path, deleted_version.clone()).await;
    let response = request(
        router.clone(),
        protocol,
        "DELETE",
        &version_path,
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, deleted_version);

    let foreign = skill(protocol, "skill_foreign");
    let list = if protocol == "anthropic" {
        json!({"data":[foreign,metadata],"next_page":null})
    } else {
        json!({"data":[foreign,metadata],"object":"list","first_id":"skill_foreign","last_id":"skill_owned","has_more":false})
    };
    mount_json(&upstream, "GET", "/v1/skills", list).await;
    let response = request(
        router.clone(),
        protocol,
        "GET",
        "/v1/skills",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let list = answer(response).await;
    assert_eq!(list["data"], json!([metadata]));
    assert!(!list.to_string().contains("skill_foreign"));
    if protocol == "openai" {
        mount_json(&upstream, "POST", root, metadata.clone()).await;
        let response = request(
            router.clone(),
            protocol,
            "POST",
            root,
            "caller",
            Some(json_payload(json!({"default_version":"1"}))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(answer(response).await, metadata);
        Mock::given(method("GET"))
            .and(path(format!("{root}/content")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PK\x03\x04MARKER_V1"))
            .mount(&upstream)
            .await;
        let response = request(
            router.clone(),
            protocol,
            "GET",
            &format!("{root}/content"),
            "caller",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
            b"PK\x03\x04MARKER_V1"
        );
    }
    let deletion = if protocol == "anthropic" {
        json!({"id":"skill_owned","type":"skill_deleted"})
    } else {
        json!({"id":"skill_owned","object":"skill.deleted","deleted":true})
    };
    mount_json(&upstream, "DELETE", root, deletion.clone()).await;
    let before = upstream.received_requests().await.unwrap().len();
    let response = request(router.clone(), protocol, "DELETE", root, "other", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
    let response = request(router.clone(), protocol, "DELETE", root, "caller", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, deletion);
    let response = request(router, protocol, "GET", &content, "caller", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let requests = upstream.received_requests().await.unwrap();
    let upload = requests
        .iter()
        .find(|request| request.method.as_str() == "POST" && request.url.path() == "/v1/skills")
        .unwrap();
    assert!(upload
        .body
        .windows(b"marker/SKILL.md".len())
        .any(|part| part == b"marker/SKILL.md"));
    assert!(upload.body.windows(3).any(|part| part == b"\x00\xff\x01"));
}

// 删除任一 typed route、父授权或二进制透传都使相应步骤失败。
#[tokio::test]
async fn openai_eleven_skill_routes_preserve_objects_versions_and_private_ownership() {
    skill_contract("openai").await;
}

#[tokio::test]
async fn anthropic_nine_skill_routes_preserve_objects_versions_and_private_ownership() {
    skill_contract("anthropic").await;
}

#[tokio::test]
async fn creation_cannot_release_an_unbound_skill_id_or_retry_accepted_creation() {
    let upstream = MockServer::start().await;
    mount_json(
        &upstream,
        "POST",
        "/v1/skills",
        skill("openai", "skill_unbound"),
    )
    .await;
    let registry = Arc::new(Registry {
        fail_bind: true,
        ..Default::default()
    });
    let (router, _) = app("openai", &upstream.uri(), registry);
    let response = request(
        router,
        "openai",
        "POST",
        "/v1/skills",
        "caller",
        Some(form(&[(
            "files",
            Some("marker/SKILL.md"),
            b"---\nname: marker\ndescription: marker\n---\nMARKER",
        )])),
    )
    .await;
    assert!(response.status().is_server_error());
    assert!(!answer(response).await.to_string().contains("skill_unbound"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn anthropic_official_skill_is_read_only_without_private_owner_registration() {
    let upstream = MockServer::start().await;
    let official = json!({"id":"pptx","type":"skill","display_name":"Presentations","source":{"type":"anthropic"},"created_at":"2026-11-01T00:00:00Z","updated_at":"2026-11-01T00:00:00Z","latest_version_id":"20261001"});
    mount_json(&upstream, "GET", "/v1/skills/pptx", official.clone()).await;
    let (router, _) = app("anthropic", &upstream.uri(), Arc::new(Registry::default()));
    let response = request(
        router.clone(),
        "anthropic",
        "GET",
        "/v1/skills/pptx",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, official);
    let before = upstream.received_requests().await.unwrap().len();
    let response = request(
        router,
        "anthropic",
        "DELETE",
        "/v1/skills/pptx",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn anthropic_builtin_label_cannot_launder_a_foreign_custom_skill_into_messages() {
    let upstream = MockServer::start().await;
    mount_json(
        &upstream,
        "GET",
        "/v1/skills/skill_foreign",
        skill("anthropic", "skill_foreign"),
    )
    .await;
    let (router, _) = app("anthropic", &upstream.uri(), Arc::new(Registry::default()));
    let response = request(router, "anthropic", "POST", "/v1/messages", "caller", Some(json_payload(json!({
        "model":"native-model","max_tokens":32,"container":{"skills":[{"type":"anthropic","skill_id":"skill_foreign","version":"latest"}]},
        "tools":[{"type":"code_execution_20250825","name":"code_execution"}],"messages":[{"role":"user","content":"Use the skill"}]
    })))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!answer(response).await.to_string().contains("Marker Skill"));
    assert!(upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.url.path() != "/v1/messages"));
}

async fn skill_consumption(protocol: &str) {
    let upstream = MockServer::start().await;
    mount_json(
        &upstream,
        "POST",
        "/v1/skills",
        skill(protocol, "skill_owned"),
    )
    .await;
    let (router, snapshot) = app(protocol, &upstream.uri(), Arc::new(Registry::default()));
    let response = request(
        router.clone(),
        protocol,
        "POST",
        "/v1/skills",
        "caller",
        Some(form(&[(
            "files",
            Some("marker/SKILL.md"),
            b"---\nname: marker\ndescription: marker\n---\nUse marker",
        )])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (route, payload, upstream_answer) = if protocol == "anthropic" {
        (
            "/v1/messages",
            json!({"model":"native-model","max_tokens":32,"container":{"skills":[{"type":"custom","skill_id":"skill_owned","version":"latest"}]},"tools":[{"type":"code_execution_20250825","name":"code_execution"}],"messages":[{"role":"user","content":"Use marker"}]}),
            json!({"id":"msg_marker","type":"message","role":"assistant","model":"provider-model","content":[{"type":"text","text":"SKILL_MARKER"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}}),
        )
    } else {
        (
            "/v1/responses",
            json!({"model":"native-model","store":false,"tools":[{"type":"shell","environment":{"type":"container_auto","skills":[{"type":"skill_reference","skill_id":"skill_owned","version":"1"}]}}],"input":"Use marker"}),
            json!({"id":"resp_marker","object":"response","created_at":1793491200,"status":"completed","model":"provider-model","output":[{"type":"message","id":"msg_marker","status":"completed","role":"assistant","content":[{"type":"output_text","text":"SKILL_MARKER","annotations":[]}]}],"usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}),
        )
    };
    mount_json(&upstream, "POST", route, upstream_answer).await;
    let response = request(
        router.clone(),
        protocol,
        "POST",
        route,
        "caller",
        Some(json_payload(payload.clone())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(answer(response).await.to_string().contains("SKILL_MARKER"));
    let before = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == route)
        .count();
    let response = request(
        router.clone(),
        protocol,
        "POST",
        route,
        "other",
        Some(json_payload(payload.clone())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == route)
            .count(),
        before
    );
    let key: ApiKey = serde_json::from_value(
        json!({"key_hash":ApiKey::hash_bearer("caller"),"allowed_models":[]}),
    )
    .unwrap();
    snapshot
        .load()
        .apikeys
        .insert(ResourceEntry::new(KEY, key, 2));
    let response = request(
        router,
        protocol,
        "POST",
        route,
        "caller",
        Some(json_payload(payload)),
    )
    .await;
    assert!(!response.status().is_success());
    assert_eq!(
        upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == route)
            .count(),
        before
    );
}

#[tokio::test]
async fn uploaded_openai_skill_reaches_responses_and_rechecks_current_entitlement() {
    skill_consumption("openai").await;
}

#[tokio::test]
async fn uploaded_anthropic_skill_reaches_messages_and_rechecks_current_entitlement() {
    skill_consumption("anthropic").await;
}

#[tokio::test]
async fn anthropic_skill_container_survives_gateway_restart_reuse_and_replacement() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app("anthropic", &upstream.uri(), registry.clone());
    mount_json(
        &upstream,
        "POST",
        "/v1/skills",
        skill("anthropic", "skill_owned"),
    )
    .await;
    let response = request(
        router.clone(),
        "anthropic",
        "POST",
        "/v1/skills",
        "caller",
        Some(form(&[(
            "files",
            Some("marker/SKILL.md"),
            b"---\nname: marker\ndescription: marker\n---\nUse marker",
        )])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut payload = json!({"model":"native-model","max_tokens":32,"container":{"skills":[{"type":"custom","skill_id":"skill_owned","version":"latest"}]},"tools":[{"type":"code_execution_20250825","name":"code_execution"}],"messages":[{"role":"user","content":"Use marker"}]});
    let result = json!({"id":"msg_container","type":"message","role":"assistant","model":"provider-model","content":[{"type":"text","text":"CONTAINER_MARKER"}],"stop_reason":"end_turn","stop_sequence":null,"container":{"id":"container_owned","expires_at":"2026-12-01T00:00:00Z"},"usage":{"input_tokens":5,"output_tokens":1}});
    mount_json(&upstream, "POST", "/v1/messages", result.clone()).await;
    let response = request(
        router,
        "anthropic",
        "POST",
        "/v1/messages",
        "caller",
        Some(json_payload(payload.clone())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await["container"]["id"], "container_owned");
    let identity = (
        "anthropic".into(),
        "message_container".into(),
        "container_owned".into(),
    );
    let original = registry
        .records
        .lock()
        .unwrap()
        .get(&identity)
        .unwrap()
        .1
        .clone();
    // 新 router 没有前一次网关内存，归属只能来自外部 authority。
    let (router, _) = app("anthropic", &upstream.uri(), registry.clone());
    payload["container"]["id"] = "container_owned".into();
    let response = request(
        router.clone(),
        "anthropic",
        "POST",
        "/v1/messages",
        "caller",
        Some(json_payload(payload.clone())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await["container"]["id"], "container_owned");
    assert_eq!(
        registry
            .records
            .lock()
            .unwrap()
            .get(&identity)
            .unwrap()
            .1
            .record_id,
        original.record_id
    );
    let mut replacement = result;
    replacement["container"]["id"] = "container_replacement".into();
    mount_json(&upstream, "POST", "/v1/messages", replacement).await;
    let response = request(
        router.clone(),
        "anthropic",
        "POST",
        "/v1/messages",
        "caller",
        Some(json_payload(payload.clone())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        answer(response).await["container"]["id"],
        "container_replacement"
    );
    assert!(registry.records.lock().unwrap().contains_key(&(
        "anthropic".into(),
        "message_container".into(),
        "container_replacement".into()
    )));
    let before = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/v1/messages")
        .count();
    let response = request(
        router,
        "anthropic",
        "POST",
        "/v1/messages",
        "other",
        Some(json_payload(payload)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/v1/messages")
            .count(),
        before
    );
}

#[tokio::test]
async fn streamed_anthropic_delta_container_is_bound_before_its_id_is_released() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app("anthropic", &upstream.uri(), registry.clone());
    mount_json(
        &upstream,
        "POST",
        "/v1/skills",
        skill("anthropic", "skill_owned"),
    )
    .await;
    let response = request(
        router.clone(),
        "anthropic",
        "POST",
        "/v1/skills",
        "caller",
        Some(form(&[(
            "files",
            Some("marker/SKILL.md"),
            b"---\nname: marker\ndescription: marker\n---\nUse marker",
        )])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events=[
        ("message_start",json!({"type":"message_start","message":{"id":"msg_stream","type":"message","role":"assistant","model":"provider-model","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":0}}})),
        ("content_block_start",json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
        ("content_block_delta",json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"STREAM_MARKER"}})),
        ("content_block_stop",json!({"type":"content_block_stop","index":0})),
        ("message_delta",json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null,"container":{"id":"container_stream","expires_at":"2026-12-01T00:00:00Z"}},"usage":{"output_tokens":1}})),
        ("message_stop",json!({"type":"message_stop"})),
    ].into_iter().map(|(event,value)|format!("event: {event}\ndata: {value}\n\n")).collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(events),
        )
        .mount(&upstream)
        .await;
    let response=request(router,"anthropic","POST","/v1/messages","caller",Some(json_payload(json!({"model":"native-model","stream":true,"max_tokens":32,"container":{"skills":[{"type":"custom","skill_id":"skill_owned","version":"latest"}]},"tools":[{"type":"code_execution_20250825","name":"code_execution"}],"messages":[{"role":"user","content":"Use marker"}]})))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("STREAM_MARKER"));
    assert!(text.contains("container_stream"));
    assert!(registry.records.lock().unwrap().contains_key(&(
        "anthropic".into(),
        "message_container".into(),
        "container_stream".into()
    )));
}

fn block_input(snapshot: &AisixSnapshot) {
    let guardrail: aisix_core::Guardrail = serde_json::from_value(json!({
        "name":"block-all","enabled":true,"kind":"custom","hook_point":"input",
        "fail_open":false,"timeout_ms":5000,
        "script":"export function checkInput() { return { action: 'block', reason_code: 'blocked' }; }"
    })).unwrap();
    snapshot
        .guardrails
        .insert(ResourceEntry::new("skills-voices-block", guardrail, 1));
    let attachment: aisix_core::models::GuardrailAttachment = serde_json::from_value(json!({
        "guardrail_id":"skills-voices-block","scope_type":"env","priority":100
    }))
    .unwrap();
    snapshot.guardrail_attachments.insert(ResourceEntry::new(
        "skills-voices-block-attachment",
        attachment,
        1,
    ));
}

#[tokio::test]
async fn native_object_writes_consult_unconditional_guardrail_even_without_text() {
    for protocol in ["openai", "anthropic"] {
        let upstream = MockServer::start().await;
        let registry = Arc::new(Registry::default());
        let (router, _) = app(protocol, &upstream.uri(), registry.clone());
        mount_json(
            &upstream,
            "POST",
            "/v1/skills",
            skill(protocol, "skill_owned"),
        )
        .await;
        let response = request(
            router,
            protocol,
            "POST",
            "/v1/skills",
            "caller",
            Some(form(&[(
                "files",
                Some("marker/SKILL.md"),
                b"---\nname: marker\ndescription: marker\n---\nUse marker",
            )])),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let (router, _) = configured_app(protocol, &upstream.uri(), registry, block_input);
        let mut routes = vec!["/v1/skills", "/v1/skills/skill_owned/versions"];
        if protocol == "openai" {
            routes.extend([
                "/v1/skills/skill_owned",
                "/v1/audio/voices",
                "/v1/audio/voice_consents",
            ]);
        }
        let before = upstream.received_requests().await.unwrap().len();
        for route in routes {
            let response = request(
                router.clone(),
                protocol,
                "POST",
                route,
                "caller",
                Some(form(&[
                    ("name", None, b""),
                    ("files", Some("empty.bin"), b"\x00\xff"),
                ])),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{protocol} {route}"
            );
        }
        assert_eq!(upstream.received_requests().await.unwrap().len(), before);
    }
}

#[tokio::test]
async fn message_body_overrides_cannot_introduce_private_skills_or_containers() {
    for (rule, private) in [
        (
            "rename",
            json!({"skills":[{"type":"custom","skill_id":"skill_foreign","version":"latest"}]}),
        ),
        (
            "default",
            json!({"skills":[{"type":"custom","skill_id":"skill_foreign","version":"latest"}]}),
        ),
        ("rename", json!({"id":"container_foreign"})),
        ("default", json!({"id":"container_foreign"})),
    ] {
        let upstream = MockServer::start().await;
        mount_json(&upstream, "POST", "/v1/messages", json!({"id":"msg_override","type":"message","role":"assistant","model":"provider-model","content":[{"type":"text","text":"OVERRIDE_WAS_SENT"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":1}})).await;
        let (router, snapshot) = app("anthropic", &upstream.uri(), Arc::new(Registry::default()));
        let mut provider = (*snapshot
            .load()
            .provider_keys
            .get_by_id(PROVIDER)
            .unwrap()
            .value)
            .clone();
        provider.request = Some(
            serde_json::from_value(if rule == "rename" {
                json!({"param_renames":{"x":"container"}})
            } else {
                json!({"default_body_fields":{"container":private.clone()}})
            })
            .unwrap(),
        );
        snapshot
            .load()
            .provider_keys
            .insert(ResourceEntry::new(PROVIDER, provider, 2));
        let mut payload = json!({"model":"native-model","max_tokens":32,"messages":[{"role":"user","content":"Use marker"}]});
        if rule == "rename" {
            payload["x"] = private;
        }
        let response = request(
            router,
            "anthropic",
            "POST",
            "/v1/messages",
            "caller",
            Some(json_payload(payload)),
        )
        .await;
        assert!(response.status().is_server_error(), "{rule}");
        let body = answer(response).await.to_string();
        assert!(!body.contains("skill_foreign"));
        assert!(!body.contains("container_foreign"));
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "{rule}"
        );
    }
}

#[tokio::test]
async fn message_body_override_with_no_private_references_remains_available() {
    let upstream = MockServer::start().await;
    mount_json(&upstream, "POST", "/v1/messages", json!({"id":"msg_safe","type":"message","role":"assistant","model":"provider-model","content":[{"type":"text","text":"SAFE_CONTAINER"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":1}})).await;
    let (router, snapshot) = app("anthropic", &upstream.uri(), Arc::new(Registry::default()));
    let mut provider = (*snapshot
        .load()
        .provider_keys
        .get_by_id(PROVIDER)
        .unwrap()
        .value)
        .clone();
    provider.request = Some(
        serde_json::from_value(json!({"default_body_fields":{"container":{"skills":[]}}})).unwrap(),
    );
    snapshot
        .load()
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 2));
    let response = request(router,"anthropic","POST","/v1/messages","caller",Some(json_payload(json!({"model":"native-model","max_tokens":32,"messages":[{"role":"user","content":"Use marker"}]})))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        answer(response).await["content"][0]["text"],
        "SAFE_CONTAINER"
    );
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["container"], json!({"skills":[]}));
}

fn cached_message_app(
    protocol: &str,
    base: &str,
    registry: Arc<Registry>,
) -> (axum::Router, SnapshotHandle<AisixSnapshot>) {
    let snapshot = AisixSnapshot::new();
    let api_base = if protocol == "openai" {
        format!("{base}/v1")
    } else {
        base.into()
    };
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"native","api_key":"upstream-key","provider":protocol,
        "api_base":api_base,"adapter":protocol
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"native-model","provider":protocol,"model_name":"provider-model","provider_key_id":PROVIDER
    })).unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    for (id, bearer) in [(KEY, "caller"), (OTHER, "other")] {
        let key: ApiKey = serde_json::from_value(
            json!({"key_hash":ApiKey::hash_bearer(bearer),"allowed_models":["native-model"]}),
        )
        .unwrap();
        snapshot.apikeys.insert(ResourceEntry::new(id, key, 1));
    }
    let policy: aisix_core::CachePolicy = serde_json::from_value(json!({
        "name":"skills-messages-cache","enabled":true,"backend":"memory","ttl_seconds":60,"applies_to":"all"
    })).unwrap();
    snapshot
        .cache_policies
        .insert(ResourceEntry::new("skills-messages-cache", policy, 1));
    let snapshot = SnapshotHandle::new(snapshot);
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1_048_576}))
            .unwrap();
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    hub.register_specialized(
        "anthropic",
        Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
    );
    let state = ProxyState::new(snapshot.clone(), hub, &config)
        .with_guardrail_index(guardrails)
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            "skills-voices-test",
        ))
        .with_native_objects(registry);
    (build_router(state), snapshot)
}

#[tokio::test]
async fn cached_message_cannot_skip_a_new_private_reference_override() {
    let upstream = MockServer::start().await;
    mount_json(&upstream, "POST", "/v1/messages", json!({"id":"msg_cached","type":"message","role":"assistant","model":"provider-model","content":[{"type":"text","text":"CACHED_SAFE_ANSWER"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":1}})).await;
    let (router, snapshot) =
        cached_message_app("anthropic", &upstream.uri(), Arc::new(Registry::default()));
    let payload = json!({"model":"native-model","max_tokens":32,"x":{"id":"container_foreign"},"messages":[{"role":"user","content":"Use marker"}]});
    for _ in 0..2 {
        let response = request(
            router.clone(),
            "anthropic",
            "POST",
            "/v1/messages",
            "caller",
            Some(json_payload(payload.clone())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            answer(response).await["content"][0]["text"],
            "CACHED_SAFE_ANSWER"
        );
    }
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        1,
        "the second request must really hit the cache"
    );
    let mut provider = (*snapshot
        .load()
        .provider_keys
        .get_by_id(PROVIDER)
        .unwrap()
        .value)
        .clone();
    provider.request =
        Some(serde_json::from_value(json!({"param_renames":{"x":"container"}})).unwrap());
    snapshot
        .load()
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 2));
    let response = request(
        router,
        "anthropic",
        "POST",
        "/v1/messages",
        "caller",
        Some(json_payload(payload)),
    )
    .await;
    assert!(response.status().is_server_error());
    let body = answer(response).await.to_string();
    assert!(!body.contains("CACHED_SAFE_ANSWER"));
    assert!(!body.contains("container_foreign"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn batch_input_does_not_export_a_foreign_skill_to_the_shared_account() {
    let upstream = MockServer::start().await;
    mount_json(&upstream,"POST","/v1/files",json!({"id":"file_leaked_batch","object":"file","purpose":"batch","filename":"batch.jsonl","bytes":512,"created_at":1793491200,"status":"processed"})).await;
    let (router, _) = app("openai", &upstream.uri(), Arc::new(Registry::default()));
    let input = json!({"custom_id":"skill-batch","method":"POST","url":"/v1/responses","body":{"model":"native-model","tools":[{"type":"shell","environment":{"type":"container_auto","skills":[{"type":"skill_reference","skill_id":"skill_foreign","version":"1"}]}}],"input":"Use the private skill"}}).to_string()+"\n";
    let response = request(
        router,
        "openai",
        "POST",
        "/v1/files",
        "caller",
        Some(form(&[
            ("purpose", None, b"batch"),
            ("file", Some("batch.jsonl"), input.as_bytes()),
        ])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!answer(response)
        .await
        .to_string()
        .contains("file_leaked_batch"));
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
