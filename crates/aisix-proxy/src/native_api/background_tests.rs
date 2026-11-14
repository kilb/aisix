//! 后台观察器的组件测试；本地上游只验证协议和记账，不冒充真实提供方 E2E。

use super::*;
use aisix_core::native_objects::*;
use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, Model, ProviderKey, ProxyConfig};
use aisix_gateway::Hub;
use futures::future::BoxFuture;
use serde_json::json;
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "11111111-1111-1111-1111-111111111111";
const PROVIDER: &str = "22222222-2222-2222-2222-222222222222";
const RECORD: &str = "33333333-3333-3333-3333-333333333333";

fn record() -> NativeRecord {
    serde_json::from_value(json!({
        "record_id": RECORD,
        "raw_id": "resp_background_owned",
        "protocol": "openai", "kind": "response",
        "lease_generation": 4,
        "lease_expires_at": (chrono::Utc::now() + chrono::Duration::seconds(30)),
        "requested_service_tier": "flex"
    }))
    .unwrap()
}

#[test]
fn terminal_cache_reporting_distinguishes_zero_from_absent() {
    for (detail, expected) in [
        (json!({"cached_tokens":0}), true),
        (json!({}), false),
        (json!({"cached_tokens":null}), false),
    ] {
        let response = json!({"id":"resp_background_owned","status":"completed","usage":{
            "input_tokens":100,"output_tokens":1,"input_tokens_details":detail
        }});
        let observed = terminal_observation(&record(), &response, chrono::Utc::now()).unwrap();
        let wire = serde_json::to_value(&observed.usage[0]).unwrap();
        assert_eq!(wire["cache_usage_reported"], expected);
        assert_eq!(wire["cached_prompt_tokens"], 0);
    }
}

#[test]
fn terminal_accounting_preserves_only_reported_billable_web_searches() {
    // 明确零和缺失都不能由一个输出动作反推收费；不同非零值能识别硬编码。
    for status in ["completed", "incomplete", "failed", "cancelled", "expired"] {
        for (reported, expected) in [(None, 0), (Some(0), 0), (Some(3), 3), (Some(7), 7)] {
            let mut response = json!({
                "id":"resp_background_owned", "status":status,
                "output":[{"type":"web_search_call","action":{"type":"open_page","url":"https://example.com"}}],
                "usage":{"input_tokens":17,"output_tokens":5}
            });
            if let Some(count) = reported {
                response["tool_usage"] = json!({"web_search":{"num_requests":count}});
            }
            assert_eq!(
                response
                    .pointer("/tool_usage/web_search/num_requests")
                    .and_then(Value::as_u64),
                reported
            );
            let observed = terminal_observation(&record(), &response, chrono::Utc::now()).unwrap();
            let wire = serde_json::to_value(&observed.usage[0]).unwrap();
            assert_eq!(
                wire.get("web_search_requests"),
                Some(&json!(expected)),
                "{status}"
            );
        }
    }
    let failed = json!({"id":"resp_background_owned","status":"failed","usage":null,
        "tool_usage":{"web_search":{"num_requests":5}}});
    let observed = terminal_observation(&record(), &failed, chrono::Utc::now()).unwrap();
    assert_eq!(observed.usage[0].web_search_requests, 5);
}

#[test]
fn terminal_accounting_keeps_reported_usage_and_partial_failed_work() {
    // 把 total 改为 input + output，或忽略失败响应的 usage，都会使断言失败。
    for status in ["completed", "incomplete", "failed", "cancelled", "expired"] {
        let observed = terminal_observation(&record(), &json!({
            "id": "resp_background_owned", "status": status, "service_tier": "priority",
            "usage": {
                "input_tokens": 40, "output_tokens": 12, "total_tokens": 55,
                "input_tokens_details": {"cached_tokens": 10, "audio_tokens": 0, "image_tokens": 8},
                "output_tokens_details": {"reasoning_tokens": 3, "audio_tokens": 2}
            }
        }), chrono::Utc::now()).unwrap();
        assert_eq!(observed.terminal_status, status);
        assert_eq!(observed.lease.generation, 4);
        let usage = &observed.usage[0];
        assert_eq!(usage.prompt_tokens, 40);
        assert_eq!(usage.completion_tokens, 12);
        assert_eq!(usage.total_tokens, 55);
        assert_eq!(usage.cached_prompt_tokens, 10);
        assert_eq!(usage.reasoning_tokens, 3);
        assert_eq!(usage.cache_creation_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.prompt_audio_tokens, Some(0));
        assert_eq!(usage.prompt_image_tokens, Some(8));
        assert_eq!(usage.completion_audio_tokens, Some(2));
        assert_eq!(usage.completion_image_tokens, None);
        assert_eq!(usage.service_tier.as_deref(), Some("priority"));
        assert_eq!(usage.requested_service_tier.as_deref(), Some("flex"));
        assert!(!usage.usage_estimated);
    }
}

#[test]
fn terminal_accounting_never_reconstructs_unreported_totals_or_adopts_another_id() {
    let response = json!({"id":"resp_background_owned", "status":"completed",
        "usage":{"input_tokens":12,"output_tokens":3}});
    let observed = terminal_observation(&record(), &response, chrono::Utc::now()).unwrap();
    assert_eq!(observed.usage[0].total_tokens, 0);
    for replacement in [
        json!("queued"),
        json!("in_progress"),
        json!(null),
        json!("unknown"),
    ] {
        let mut changed = response.clone();
        changed["status"] = replacement;
        assert!(terminal_observation(&record(), &changed, chrono::Utc::now()).is_none());
    }
    let mut changed = response;
    changed["id"] = json!("resp_other_tenant");
    assert!(terminal_observation(&record(), &changed, chrono::Utc::now()).is_none());
}

#[test]
fn terminal_accounting_requires_reported_counts_and_keeps_explicit_zero() {
    for status in ["completed", "incomplete", "failed", "cancelled", "expired"] {
        for invalid in [
            json!({}),
            json!({"input_tokens":1}),
            json!({"input_tokens":1,"output_tokens":"2"}),
            json!([]),
        ] {
            let response = json!({"id":"resp_background_owned","status":status,"usage":invalid});
            assert!(terminal_observation(&record(), &response, chrono::Utc::now()).is_none());
        }
        let zero = json!({"id":"resp_background_owned","status":status,"usage":{"input_tokens":0,"output_tokens":0}});
        let observed = terminal_observation(&record(), &zero, chrono::Utc::now()).unwrap();
        assert_eq!(observed.usage[0].prompt_tokens, 0);
        assert_eq!(observed.usage[0].completion_tokens, 0);
        for usage in [None, Some(Value::Null)] {
            let mut response = json!({"id":"resp_background_owned","status":status});
            if let Some(usage) = usage {
                response["usage"] = usage;
            }
            assert_eq!(
                terminal_observation(&record(), &response, chrono::Utc::now()).is_some(),
                !matches!(status, "completed" | "incomplete")
            );
        }
    }
}

#[derive(Default)]
struct Registry {
    pending: Mutex<Vec<NativeRecord>>,
    completed: Mutex<Vec<CompleteRequest>>,
    rescheduled: Mutex<Vec<LeaseRequest>>,
}

macro_rules! unavailable {
    ($method:ident, $request:ty) => {
        fn $method(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for Registry {
    unavailable!(begin, BeginRequest);
    unavailable!(bind, BindRequest);
    unavailable!(lookup, LookupRequest);
    unavailable!(owned, OwnedRequest);
    unavailable!(tombstone, TombstoneRequest);
    fn claim(&self, _: ClaimRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async {
            let mut reply = NativeReply::new("leased");
            reply.records = std::mem::take(&mut *self.pending.lock().unwrap());
            Ok(reply)
        })
    }
    fn renew(&self, _: LeaseRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Ok(NativeReply::new("leased")) })
    }
    fn reschedule(
        &self,
        request: LeaseRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            self.rescheduled.lock().unwrap().push(request);
            Ok(NativeReply::new("pending"))
        })
    }
    fn complete(
        &self,
        request: CompleteRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            self.completed.lock().unwrap().push(request);
            Ok(NativeReply::new("completed"))
        })
    }
}

fn fixture(base: &str) -> (ProxyState, Arc<Registry>, NativeRecord) {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"native", "provider":"openai", "adapter":"openai",
        "secret":"test-background-secret", "api_base":base,
        "request":{"default_headers":{"openai-project":"project_owned"}}
    }))
    .unwrap();
    let platform = crate::platform::PlatformSettings::from_secret("background-test-platform");
    let boundary = configured_native_origin(
        &platform.id_key,
        Uuid::parse_str(PROVIDER).unwrap(),
        &provider,
        NativeProtocol::Openai,
        NativeObjectKind::Response,
    )
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"test-model", "provider":"openai", "model_name":"gpt-test",
        "provider_key_id":PROVIDER
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    let mut owned = record();
    owned.origin = Some(OriginBinding {
        model_id: Uuid::parse_str(MODEL).unwrap(),
        provider_key_id: Uuid::parse_str(PROVIDER).unwrap(),
        boundary,
    });
    let registry = Arc::new(Registry::default());
    registry.pending.lock().unwrap().push(owned.clone());
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
    let state = ProxyState::new(SnapshotHandle::new(snapshot), Arc::new(Hub::new()), &config)
        .with_platform(platform)
        .with_native_objects(registry.clone());
    (state, registry, owned)
}

#[tokio::test]
async fn leased_worker_polls_pinned_account_and_submits_one_terminal_observation() {
    // 去掉轮询、静态账号头或 complete 调用都会失败；没有客户端 key 也必须可恢复。
    let upstream = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/v1/responses/resp_background_owned"))
        .and(header("authorization", "Bearer test-background-secret"))
        .and(header("openai-project", "project_owned"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"resp_background_owned", "status":"completed",
            "usage":{"input_tokens":7,"output_tokens":4,"total_tokens":11}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let (state, registry, _) = fixture(&upstream.uri());
    assert_eq!(run_once(&state).await.unwrap(), 1);
    {
        let completed = registry.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].usage[0].total_tokens, 11);
        assert_eq!(completed[0].lease.record_id.to_string(), RECORD);
    }
    assert_eq!(run_once(&state).await.unwrap(), 0);
    assert_eq!(registry.completed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn accepted_response_settles_after_model_deletion_or_repointing() {
    for deleted in [true, false] {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/v1/responses/resp_background_owned"))
            .and(header("authorization", "Bearer test-background-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"resp_background_owned", "status":"completed",
                "usage":{"input_tokens":7,"output_tokens":4,"total_tokens":11}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let (state, registry, _) = fixture(&upstream.uri());
        let snapshot = state.snapshot.load();
        if deleted {
            snapshot.models.remove(MODEL);
        } else {
            let entry = snapshot.models.get_by_id(MODEL).unwrap();
            let mut model = entry.value.as_ref().clone();
            model.provider_key_id = Some(Uuid::new_v4().to_string());
            snapshot.models.insert(ResourceEntry::new(MODEL, model, 2));
        }
        assert_eq!(run_once(&state).await.unwrap(), 1);
        assert_eq!(registry.completed.lock().unwrap().len(), 1);
        assert_eq!(
            registry.completed.lock().unwrap()[0].usage[0].total_tokens,
            11
        );
        assert_eq!(run_once(&state).await.unwrap(), 0);
        assert_eq!(registry.completed.lock().unwrap().len(), 1);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn changed_origin_and_incomplete_claim_cannot_contact_the_upstream() {
    let upstream = MockServer::builder().start().await;
    for missing_origin in [false, true] {
        let (state, registry, mut owned) = fixture(&upstream.uri());
        if missing_origin {
            owned.origin = None;
        } else {
            owned.origin.as_mut().unwrap().provider_key_id = Uuid::new_v4();
        }
        *registry.pending.lock().unwrap() = vec![owned];
        assert_eq!(run_once(&state).await.unwrap(), 0);
        assert!(registry.completed.lock().unwrap().is_empty());
        assert_eq!(registry.rescheduled.lock().unwrap().len(), 1);
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn polling_errors_and_pending_responses_remain_durable_work() {
    for (status, body) in [
        (
            200,
            json!({"id":"resp_background_owned", "status":"in_progress"}),
        ),
        (404, json!({"error":{"message":"not found"}})),
        (
            200,
            json!({"id":"resp_foreign", "status":"completed", "usage":{"input_tokens":9,"output_tokens":2}}),
        ),
    ] {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&upstream)
            .await;
        let (state, registry, _) = fixture(&upstream.uri());
        assert_eq!(run_once(&state).await.unwrap(), 0);
        assert!(registry.completed.lock().unwrap().is_empty());
        let postponed = registry.rescheduled.lock().unwrap();
        assert_eq!(postponed.len(), 1);
        assert!(postponed[0].poll_after_ms > 0);
        assert_eq!(postponed[0].lease.generation, 4);
    }
}

#[tokio::test]
async fn an_upstream_redirect_never_forwards_the_bound_credential() {
    let destination = MockServer::builder().start().await;
    let upstream = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", destination.uri()))
        .expect(1)
        .mount(&upstream)
        .await;
    let (state, registry, _) = fixture(&upstream.uri());
    assert_eq!(run_once(&state).await.unwrap(), 0);
    assert!(destination.received_requests().await.unwrap().is_empty());
    assert!(registry.completed.lock().unwrap().is_empty());
    assert_eq!(registry.rescheduled.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn changed_secondary_credential_never_polls_a_different_account() {
    let upstream = MockServer::builder().start().await;
    let (state, registry, _) = fixture(&upstream.uri());
    let snapshot = state.snapshot.load();
    let entry = snapshot.provider_keys.get_by_id(PROVIDER).unwrap();
    let mut changed = (*entry.value).clone();
    changed
        .request
        .as_mut()
        .unwrap()
        .default_headers
        .insert("x-api-key".into(), "changed-account-secret".into());
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, changed, 2));
    assert_eq!(run_once(&state).await.unwrap(), 0);
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(registry.completed.lock().unwrap().is_empty());
    assert_eq!(registry.rescheduled.lock().unwrap().len(), 1);
}

#[test]
fn terminal_io_uses_durable_input_and_model_output_without_counting_poll_bytes() {
    let mut saved = serde_json::to_value(record()).unwrap();
    saved["upstream_input_text"] = json!({"utf8_bytes":8,"unicode_scalars":3,"complete":false});
    let recovered: NativeRecord = serde_json::from_value(saved).unwrap();
    for (status, complete) in [("completed", true), ("incomplete", false)] {
        let response = json!({"id":"resp_background_owned","status":status,
            "output":[{"type":"message","content":[{"type":"output_text","text":"B界🙂"}]}],
            "usage":{"input_tokens":19,"output_tokens":7}});
        let observed = terminal_observation(&recovered, &response, chrono::Utc::now()).unwrap();
        let audit = observed.usage[0]
            .io_audit
            .as_ref()
            .expect("terminal model IO");
        assert_eq!(audit.version, 2);
        assert_eq!(audit.boundary, "upstream_attempt");
        assert_eq!(audit.request_body_bytes, None);
        assert_eq!(audit.response_body_bytes, None);
        assert_eq!(audit.request_complete, None);
        assert_eq!(
            audit.input_text,
            Some(aisix_core::usage_io::TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: false,
            })
        );
        assert_eq!(
            audit.output_text,
            Some(aisix_core::usage_io::TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete,
            })
        );
    }
    let legacy = terminal_observation(
        &record(),
        &json!({
            "id":"resp_background_owned","status":"completed",
            "output":[{"type":"message","content":[{"type":"output_text","text":""}]}],
            "usage":{"input_tokens":0,"output_tokens":0}
        }),
        chrono::Utc::now(),
    )
    .unwrap();
    let audit = legacy.usage[0].io_audit.as_ref().unwrap();
    assert_eq!(audit.input_text, None);
    assert_eq!(
        audit.output_text,
        Some(aisix_core::usage_io::TextMeasurement {
            utf8_bytes: 0,
            unicode_scalars: 0,
            complete: true,
        })
    );
}
