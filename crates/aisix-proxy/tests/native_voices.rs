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
    deletions: Mutex<HashMap<Uuid, ObjectDeletionRequest>>,
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

    fn reserve_delete(
        &self,
        request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let result = (|| {
            let records = self.records.lock().unwrap();
            let (_, record) = records
                .values()
                .find(|(owner, record)| {
                    *owner == request.api_key_id
                        && record.record_id == request.record_id
                        && !record.deleted
                })
                .ok_or(NativeRegistryError::NotFound)?;
            if record.version != request.version {
                return Err(NativeRegistryError::Conflict);
            }
            let mut deletions = self.deletions.lock().unwrap();
            if deletions.get(&request.operation_id).is_some_and(|pending| {
                pending.record_id != request.record_id
                    || pending.api_key_id != request.api_key_id
                    || pending.version != request.version
            }) || deletions.values().any(|pending| {
                pending.record_id == request.record_id
                    && pending.operation_id != request.operation_id
            }) {
                return Err(NativeRegistryError::Conflict);
            }
            let mut reply = NativeReply::new("reserved");
            reply.operation_id = Some(request.operation_id);
            reply.record = Some(record.clone());
            deletions.insert(request.operation_id, request);
            Ok(reply)
        })();
        Box::pin(async move { result })
    }

    fn commit_delete(
        &self,
        request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let result = (|| {
            let mut records = self.records.lock().unwrap();
            let mut deletions = self.deletions.lock().unwrap();
            let pending = deletions
                .get(&request.operation_id)
                .filter(|pending| {
                    pending.api_key_id == request.api_key_id
                        && pending.record_id == request.record_id
                        && pending.version == request.version
                })
                .ok_or(NativeRegistryError::NotFound)?;
            let (_, record) = records
                .values_mut()
                .find(|(owner, record)| {
                    *owner == pending.api_key_id
                        && record.record_id == pending.record_id
                        && !record.deleted
                })
                .ok_or(NativeRegistryError::NotFound)?;
            if record.version != pending.version {
                return Err(NativeRegistryError::Conflict);
            }
            record.deleted = true;
            record.version += 1;
            let mut reply = NativeReply::new("deleted");
            reply.operation_id = Some(request.operation_id);
            reply.record = Some(record.clone());
            deletions.remove(&request.operation_id);
            Ok(reply)
        })();
        Box::pin(async move { result })
    }

    fn resolve_reservation(
        &self,
        request: ResolveReservationRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut deletions = self.deletions.lock().unwrap();
        let found = deletions
            .get(&request.operation_id)
            .is_some_and(|pending| pending.api_key_id == request.api_key_id);
        if found && request.upstream_rejected {
            deletions.remove(&request.operation_id);
        }
        let reply = NativeReply::new(if request.upstream_rejected {
            "rejected"
        } else {
            "uncertain"
        });
        Box::pin(async move {
            if found {
                Ok(reply)
            } else {
                Err(NativeRegistryError::NotFound)
            }
        })
    }

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
            reply.record = Some(record.clone());
            let mut records = self.records.lock().unwrap();
            for object in request.objects {
                if operation.protocol != NativeProtocol::Openai
                    || operation.kind != NativeObjectKind::Response
                    || !matches!(
                        object.kind,
                        NativeObjectKind::ResponseItem | NativeObjectKind::ResponseContainer
                    )
                {
                    return Err(NativeRegistryError::InvalidRequest);
                }
                let key = (
                    operation.protocol.as_str().to_string(),
                    object.kind.as_str().to_string(),
                    object.raw_id.clone(),
                );
                let child = if let Some((owner, existing)) = records.get(&key) {
                    if *owner != request.api_key_id
                        || existing.deleted
                        || existing.protocol != Some(operation.protocol)
                        || existing.kind != Some(object.kind)
                        || existing.parent_id.is_none()
                        || !existing.origin.as_ref().is_some_and(|origin| {
                            origin.provider_key_id == operation.origin.provider_key_id
                                && origin.boundary == operation.origin.boundary
                        })
                    {
                        return Err(NativeRegistryError::Conflict);
                    }
                    existing.clone()
                } else {
                    let mut derived = operation.clone();
                    derived.operation_id = Uuid::new_v4();
                    derived.kind = object.kind;
                    derived.parent_id = Some(record.record_id);
                    derived.asynchronous = false;
                    let child: NativeRecord = serde_json::from_value(json!({
                        "record_id":Uuid::new_v4(),"operation_id":derived.operation_id,
                        "api_key_id":request.api_key_id,"requested_model_id":derived.requested_model_id,
                        "requested_model":derived.requested_model,"origin":derived.origin,
                        "protocol":derived.protocol,"kind":derived.kind,"raw_id":object.raw_id,
                        "parent_id":record.record_id,"version":1
                    })).unwrap();
                    self.operations
                        .lock()
                        .unwrap()
                        .insert(derived.operation_id, derived);
                    records.insert(key, (request.api_key_id, child.clone()));
                    child
                };
                reply.records.push(child);
            }
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

async fn mount_json(server: &MockServer, verb: &str, route: &str, value: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(server)
        .await;
}

#[tokio::test]
async fn seven_voice_and_consent_routes_keep_native_fields_and_consent_owner() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app("openai", &upstream.uri(), registry.clone());
    let consent = json!({"id":"cons_owned","object":"audio.voice_consent","created_at":1793491200,"name":"Speaker","language":"en-US"});
    mount_json(
        &upstream,
        "POST",
        "/v1/audio/voice_consents",
        consent.clone(),
    )
    .await;
    let response = request(
        router.clone(),
        "openai",
        "POST",
        "/v1/audio/voice_consents",
        "caller",
        Some(form(&[
            ("name", None, b"Speaker"),
            ("language", None, b"en-US"),
            ("recording", Some("consent.wav"), b"RIFF\x00\xffCONSENT"),
        ])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, consent);
    let root = "/v1/audio/voice_consents/cons_owned";
    mount_json(&upstream, "GET", root, consent.clone()).await;
    let response = request(router.clone(), "openai", "GET", root, "caller", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, consent);
    let mut updated = consent.clone();
    updated["name"] = "Renamed Speaker".into();
    mount_json(&upstream, "POST", root, updated.clone()).await;
    let response = request(
        router.clone(),
        "openai",
        "POST",
        root,
        "caller",
        Some(json_payload(json!({"name":"Renamed Speaker"}))),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await["name"], "Renamed Speaker");
    mount_json(&upstream, "GET", "/v1/audio/voice_consents", json!({"data":[consent,json!({"id":"cons_foreign","object":"audio.voice_consent","created_at":1793491200,"name":"PRIVATE","language":"en-US"})],"object":"list","first_id":"cons_owned","last_id":"cons_foreign","has_more":false})).await;
    let response = request(
        router.clone(),
        "openai",
        "GET",
        "/v1/audio/voice_consents",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed = answer(response).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["last_id"], "cons_owned");
    assert!(!listed.to_string().contains("PRIVATE"));
    let voice = json!({"id":"voice_owned","object":"audio.voice","created_at":1793491200,"name":"Speaker","type":"audio_sample"});
    mount_json(&upstream, "POST", "/v1/audio/voices", voice.clone()).await;
    let before = upstream.received_requests().await.unwrap().len();
    let response = request(
        router.clone(),
        "openai",
        "POST",
        "/v1/audio/voices",
        "other",
        Some(form(&[
            ("name", None, b"Speaker"),
            ("consent", None, b"cons_owned"),
            ("audio_sample", Some("sample.wav"), b"RIFF\x00\xffSAMPLE"),
        ])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
    let response = request(
        router.clone(),
        "openai",
        "POST",
        "/v1/audio/voices",
        "caller",
        Some(form(&[
            ("name", None, b"Speaker"),
            ("consent", None, b"cons_owned"),
            ("audio_sample", Some("sample.wav"), b"RIFF\x00\xffSAMPLE"),
        ])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await, voice);
    let accepted = registry.accepted.lock().unwrap();
    let voice_creation = accepted
        .iter()
        .find(|request| request.submission.kind.as_str() == "voice")
        .unwrap();
    assert_eq!(voice_creation.references.len(), 1);
    assert!(voice_creation.submission.parent_id.is_none());
    drop(accepted);
    mount_json(
        &upstream,
        "DELETE",
        "/v1/audio/voices/voice_owned",
        json!({"id":"voice_owned","object":"audio.voice.deleted","deleted":true}),
    )
    .await;
    let response = request(
        router.clone(),
        "openai",
        "DELETE",
        "/v1/audio/voices/voice_owned",
        "caller",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    mount_json(
        &upstream,
        "DELETE",
        root,
        json!({"id":"cons_owned","object":"audio.voice_consent.deleted","deleted":true}),
    )
    .await;
    let response = request(router.clone(), "openai", "DELETE", root, "caller", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let before = upstream.received_requests().await.unwrap().len();
    let response = request(router, "openai", "GET", root, "caller", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn prompt_voice_json_preserves_its_model_independently_of_gateway_selection() {
    let upstream = MockServer::start().await;
    mount_json(&upstream, "POST", "/v1/audio/voices", json!({"id":"voice_prompt","object":"audio.voice","created_at":1793491200,"name":"Narrator","type":"prompt","prompt":"An adult narrator"})).await;
    let (router, _) = app("openai", &upstream.uri(), Arc::new(Registry::default()));
    let payload = json!({"type":"prompt","name":"Narrator","prompt":"An adult narrator","model":"auto","script_hint":"Read this sample sentence."});
    let response = request(
        router,
        "openai",
        "POST",
        "/v1/audio/voices",
        "caller",
        Some(json_payload(payload.clone())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(answer(response).await["type"], "prompt");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        payload
    );
}

async fn create_owned_voice(upstream: &MockServer, router: axum::Router) {
    mount_json(upstream,"POST","/v1/audio/voice_consents",json!({"id":"cons_owned","object":"audio.voice_consent","created_at":1793491200,"name":"Speaker","language":"en-US"})).await;
    let response = request(
        router.clone(),
        "openai",
        "POST",
        "/v1/audio/voice_consents",
        "caller",
        Some(form(&[
            ("name", None, b"Speaker"),
            ("language", None, b"en-US"),
            ("recording", Some("consent.wav"), b"RIFF-CONSENT"),
        ])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    mount_json(upstream,"POST","/v1/audio/voices",json!({"id":"voice_owned","object":"audio.voice","created_at":1793491200,"name":"Speaker","type":"audio_sample"})).await;
    let response = request(
        router,
        "openai",
        "POST",
        "/v1/audio/voices",
        "caller",
        Some(form(&[
            ("name", None, b"Speaker"),
            ("consent", None, b"cons_owned"),
            ("audio_sample", Some("sample.wav"), b"RIFF-SAMPLE"),
        ])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn created_custom_voice_can_generate_speech_and_chat_audio_only_for_its_owner() {
    let upstream = MockServer::start().await;
    let registry = Arc::new(Registry::default());
    let (router, _) = app("openai", &upstream.uri(), registry);
    create_owned_voice(&upstream, router.clone()).await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(b"RIFF\x00\xffVOICE_MARKER"),
        )
        .mount(&upstream)
        .await;
    let speech = json!({"model":"native-model","input":"Read the marker","voice":{"id":"voice_owned"},"response_format":"wav"});
    let response = request(
        router.clone(),
        "openai",
        "POST",
        "/v1/audio/speech",
        "caller",
        Some(json_payload(speech.clone())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
        b"RIFF\x00\xffVOICE_MARKER"
    );
    let chat = json!({"model":"native-model","modalities":["text","audio"],"audio":{"voice":{"id":"voice_owned"},"format":"wav"},"messages":[{"role":"user","content":"Read the marker"}]});
    mount_json(&upstream,"POST","/v1/chat/completions",json!({"id":"chatcmpl_audio","object":"chat.completion","created":1793491200,"model":"provider-model","choices":[{"index":0,"message":{"role":"assistant","content":"VOICE_MARKER","audio":{"id":"audio_result","data":"UklGRg==","expires_at":1800000000,"transcript":"VOICE_MARKER"}},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}})).await;
    let response = request(
        router.clone(),
        "openai",
        "POST",
        "/v1/chat/completions",
        "caller",
        Some(json_payload(chat.clone())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result = answer(response).await;
    assert_eq!(result["choices"][0]["message"]["audio"]["data"], "UklGRg==");
    for (path, payload) in [("/v1/audio/speech", speech), ("/v1/chat/completions", chat)] {
        let before = upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == path)
            .count();
        let response = request(
            router.clone(),
            "openai",
            "POST",
            path,
            "other",
            Some(json_payload(payload)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            upstream
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == path)
                .count(),
            before
        );
    }
}

#[tokio::test]
async fn body_override_cannot_introduce_a_private_voice_after_the_official_slot_was_checked() {
    for (path, target, payload) in [
        (
            "/v1/audio/speech",
            "voice",
            json!({"model":"native-model","input":"Read marker","voice":"alloy","x":{"id":"voice_foreign"}}),
        ),
        (
            "/v1/chat/completions",
            "audio",
            json!({"model":"native-model","messages":[{"role":"user","content":"Read marker"}],"x":{"voice":{"id":"voice_foreign"},"format":"wav"}}),
        ),
    ] {
        let upstream = MockServer::start().await;
        let (router, snapshot) = app("openai", &upstream.uri(), Arc::new(Registry::default()));
        let entry = snapshot.load().provider_keys.get_by_id(PROVIDER).unwrap();
        let mut provider = (*entry.value).clone();
        provider.request =
            Some(serde_json::from_value(json!({"param_renames":{"x":target}})).unwrap());
        snapshot
            .load()
            .provider_keys
            .insert(ResourceEntry::new(PROVIDER, provider, 2));
        let response = request(
            router,
            "openai",
            "POST",
            path,
            "caller",
            Some(json_payload(payload)),
        )
        .await;
        assert!(response.status().is_server_error(), "{path}");
        let body = answer(response).await;
        assert!(!body.to_string().contains("voice_foreign"));
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "{path}"
        );
    }
}
