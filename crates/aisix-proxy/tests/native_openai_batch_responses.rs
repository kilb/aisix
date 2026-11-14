//! Responses 批次的终态组件回归；注册表替身不代替真实控制面计费验收。

use std::sync::{Arc, Mutex};

use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_proxy::ProxyState;
use futures::future::BoxFuture;
use serde_json::{json, Value};
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";

struct Registry {
    record: Mutex<NativeRecord>,
    completed: Mutex<Vec<CompleteRequest>>,
    postponed: Mutex<Vec<LeaseRequest>>,
}

fn unsupported() -> BoxFuture<'static, Result<NativeReply, NativeRegistryError>> {
    Box::pin(async { Err(NativeRegistryError::InvalidRequest) })
}

impl NativeObjectRegistry for Registry {
    fn begin(&self, _: BeginRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        unsupported()
    }
    fn bind(&self, _: BindRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        unsupported()
    }
    fn lookup(&self, _: LookupRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        unsupported()
    }
    fn owned(&self, _: OwnedRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        unsupported()
    }
    fn tombstone(
        &self,
        _: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        unsupported()
    }
    fn claim(
        &self,
        request: ClaimRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        assert_eq!(request.protocol, Some(NativeProtocol::Openai));
        assert_eq!(request.kind, Some(NativeObjectKind::MessageBatch));
        let mut record = self.record.lock().unwrap();
        let mut reply = NativeReply::new("leased");
        if !record.completed && request.record_id.is_none_or(|id| id == record.record_id) {
            record.lease_generation = Some(record.lease_generation.unwrap_or(0) + 1);
            record.lease_expires_at = Some(chrono::Utc::now() + chrono::Duration::seconds(30));
            reply.records.push(record.clone());
        }
        Box::pin(async move { Ok(reply) })
    }
    fn renew(
        &self,
        request: LeaseRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let record = self.record.lock().unwrap();
        assert_eq!(request.lease.record_id, record.record_id);
        assert_eq!(Some(request.lease.generation), record.lease_generation);
        let state = if record.completed {
            "completed"
        } else {
            "leased"
        };
        Box::pin(async move { Ok(NativeReply::new(state)) })
    }
    fn reschedule(
        &self,
        request: LeaseRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        self.postponed.lock().unwrap().push(request);
        Box::pin(async { Ok(NativeReply::new("pending")) })
    }
    fn complete(
        &self,
        request: CompleteRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut record = self.record.lock().unwrap();
        assert_eq!(request.lease.record_id, record.record_id);
        assert_eq!(Some(request.lease.generation), record.lease_generation);
        record.completed = true;
        // 保存每次调用，不能由替身吞掉重复完成而使执行器的回放错误变绿。
        self.completed.lock().unwrap().push(request);
        Box::pin(async { Ok(NativeReply::new("completed")) })
    }
}

fn state(base: &str, ids: &[&str]) -> (ProxyState, Arc<Registry>) {
    let platform = aisix_proxy::platform::PlatformSettings::from_secret("batch-responses-test");
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"batch-account", "api_key":"batch-response-test-key", "provider":"openai",
        "api_base":format!("{base}/v1"), "adapter":"openai"
    }))
    .unwrap();
    let provider_id = Uuid::parse_str(PROVIDER).unwrap();
    let boundary = configured_native_origin(
        &platform.id_key,
        provider_id,
        &provider,
        NativeProtocol::Openai,
        NativeObjectKind::MessageBatch,
    )
    .unwrap();
    let entries: Vec<Value> = ids.iter().map(|id| json!({
        "entry_id":id,"endpoint":"/v1/responses","requested_model_id":MODEL,"model_id":MODEL,
        "requested_model":"public-model","upstream_model":"provider-model",
        "upstream_provider":"openai","requested_service_tier":"flex"
    })).collect();
    let record = serde_json::from_value(json!({
        "record_id":Uuid::new_v4(),"operation_id":Uuid::new_v4(),
        "protocol":"openai","kind":"message_batch","raw_id":"batch_responses",
        "entries":entries,"origin":{"model_id":MODEL,"provider_key_id":PROVIDER,"boundary":boundary}
    }))
    .unwrap();
    let registry = Arc::new(Registry {
        record: Mutex::new(record),
        completed: Mutex::new(Vec::new()),
        postponed: Mutex::new(Vec::new()),
    });
    let snapshot = AisixSnapshot::new();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
    let state = ProxyState::new(SnapshotHandle::new(snapshot), Arc::new(Hub::new()), &config)
        .with_platform(platform)
        .with_native_objects(registry.clone());
    (state, registry)
}

async fn result_file(upstream: &MockServer, lines: &[Value], completed: u32, failed: u32) {
    Mock::given(method("GET"))
        .and(path("/v1/batches/batch_responses"))
        .and(header("authorization", "Bearer batch-response-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"batch_responses","object":"batch","status":"completed",
            "completed_at":1793843100,"output_file_id":"file_responses",
            "request_counts":{"total":completed + failed,"completed":completed,"failed":failed}
        })))
        .mount(upstream)
        .await;
    let content = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    Mock::given(method("GET"))
        .and(path("/v1/files/file_responses/content"))
        .and(header("authorization", "Bearer batch-response-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(content))
        .mount(upstream)
        .await;
}

fn response(id: &str, terminal: &str, usage: Value) -> Value {
    json!({"id":format!("batch_req_{id}"),"custom_id":id,"error":null,"response":{
    "status_code":200,"request_id":format!("req_{id}"),"body":{
        "id":format!("resp_{id}"),"object":"response","created_at":1793843000,
        "model":"provider-model","status":terminal,"output":[],"tools":[],
        "tool_choice":"auto","parallel_tool_calls":true,"service_tier":"priority",
        "error":if terminal == "failed" {json!({"code":"server_error","message":"Generation failed"})} else {Value::Null},
        "incomplete_details":if terminal == "incomplete" {json!({"reason":"max_output_tokens"})} else {Value::Null},
        "usage":usage
    }}})
}

#[tokio::test]
async fn completed_batch_settles_success_and_failed_response_without_usage_once() {
    let upstream = MockServer::builder().start().await;
    let (state, registry) = state(&upstream.uri(), &["good", "failed"]);
    result_file(&upstream, &[
        response("good", "completed", json!({
            "input_tokens":31,"output_tokens":7,"total_tokens":38,
            "input_tokens_details":{"cached_tokens":11},"output_tokens_details":{"reasoning_tokens":2}
        })),
        response("failed", "failed", Value::Null),
    ], 1, 1).await;
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&state)
            .await
            .unwrap(),
        1
    );
    assert!(registry.postponed.lock().unwrap().is_empty());
    {
        let completed = registry.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].terminal_status, "completed");
        assert_eq!(completed[0].output_file_ids, vec!["file_responses"]);
        assert_eq!(completed[0].usage.len(), 2);
        let good = completed[0]
            .usage
            .iter()
            .find(|row| row.entry_id == "good")
            .unwrap();
        assert_eq!(good.terminal_status.as_deref(), Some("completed"));
        assert_eq!(good.status_code, 200);
        assert_eq!(
            (
                good.prompt_tokens,
                good.completion_tokens,
                good.total_tokens
            ),
            (31, 7, 38)
        );
        assert_eq!(good.cached_prompt_tokens, 11);
        assert_eq!(good.reasoning_tokens, 2);
        assert_eq!(good.requested_service_tier.as_deref(), Some("flex"));
        assert_eq!(good.service_tier.as_deref(), Some("priority"));
        let failed = completed[0]
            .usage
            .iter()
            .find(|row| row.entry_id == "failed")
            .unwrap();
        assert_eq!(failed.terminal_status.as_deref(), Some("failed"));
        assert_eq!(failed.status_code, 502);
        assert_eq!(failed.error_class, "upstream_status");
        assert_eq!(failed.error_message, "Generation failed");
        assert_eq!(
            (
                failed.prompt_tokens,
                failed.completion_tokens,
                failed.total_tokens
            ),
            (0, 0, 0)
        );
        assert!(!failed.usage_estimated);
    }
    let before = upstream.received_requests().await.unwrap().len();
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&state)
            .await
            .unwrap(),
        0
    );
    assert_eq!(registry.completed.lock().unwrap().len(), 1);
    assert_eq!(upstream.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn terminal_response_failures_and_incomplete_results_retain_reported_usage() {
    let upstream = MockServer::builder().start().await;
    let (state, registry) = state(&upstream.uri(), &["failed", "incomplete"]);
    result_file(&upstream, &[
        response("failed", "failed", json!({
            "input_tokens":17,"output_tokens":6,"total_tokens":23,
            "input_tokens_details":{"cached_tokens":7,"cache_write_tokens":0},
            "output_tokens_details":{"reasoning_tokens":3}
        })),
        response("incomplete", "incomplete", json!({
            "input_tokens":41,"output_tokens":13,"total_tokens":54,
            "input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":5}
        })),
    ], 1, 1).await;
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&state)
            .await
            .unwrap(),
        1
    );
    let completed = registry.completed.lock().unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].usage.len(), 2);
    let failed = completed[0]
        .usage
        .iter()
        .find(|row| row.entry_id == "failed")
        .unwrap();
    assert_eq!(failed.terminal_status.as_deref(), Some("failed"));
    assert_eq!(failed.status_code, 502);
    assert_eq!(failed.error_message, "Generation failed");
    assert_eq!(
        (
            failed.prompt_tokens,
            failed.completion_tokens,
            failed.total_tokens
        ),
        (17, 6, 23)
    );
    assert_eq!(failed.cached_prompt_tokens, 7);
    assert_eq!(failed.cache_write_tokens, Some(0));
    assert_eq!(failed.cache_usage_reported, Some(true));
    assert_eq!(failed.reasoning_tokens, 3);
    let incomplete = completed[0]
        .usage
        .iter()
        .find(|row| row.entry_id == "incomplete")
        .unwrap();
    assert_eq!(incomplete.terminal_status.as_deref(), Some("incomplete"));
    assert_eq!(incomplete.status_code, 200);
    assert_eq!(
        (
            incomplete.prompt_tokens,
            incomplete.completion_tokens,
            incomplete.total_tokens
        ),
        (41, 13, 54)
    );
    assert_eq!(incomplete.cached_prompt_tokens, 0);
    assert_eq!(incomplete.cache_usage_reported, Some(true));
    assert_eq!(incomplete.reasoning_tokens, 5);
}
