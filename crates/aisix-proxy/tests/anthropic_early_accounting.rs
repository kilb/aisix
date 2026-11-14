//! 早于首内容终结的组件回归：真实 Router、UsageSink 和 HTTP 上游替身。
//! 本测试不调用真实供应商，也不把组件验证称作供应商 E2E。
//!
//! message_start 的 Message.usage 与 error 事件遵循官方合同：
//! https://platform.claude.com/docs/en/build-with-claude/streaming
//! https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/usage.py
//! https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/server_tool_usage.py

use std::sync::Arc;
use std::time::Duration;

use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_provider_anthropic::AnthropicBridge;
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tower::ServiceExt;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const DEADLINE: Duration = Duration::from_secs(15);
const PROVIDER_KEY_ID: &str = "11111111-1111-4111-8111-111111111111";
const MODEL_ID: &str = "22222222-2222-4222-8222-222222222222";

#[derive(Clone, Copy, Debug)]
enum Client {
    Chat,
    Responses,
}

impl Client {
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }

    fn request(self) -> Value {
        match self {
            Self::Chat => json!({
                "model":"early-accounting",
                "messages":[{"role":"user","content":"Look up the requested information."}],
                "max_tokens":16,
                "stream":true
            }),
            Self::Responses => json!({
                "model":"early-accounting",
                "input":"Look up the requested information.",
                "max_output_tokens":16,
                "stream":true
            }),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Ending {
    Overloaded,
    Eof,
}

impl Ending {
    fn error_class(self) -> &'static str {
        match self {
            Self::Overloaded => "upstream_in_band",
            Self::Eof => "stream_aborted",
        }
    }
}

fn upstream_sse(ending: Ending) -> String {
    // 计数故意互不相同；清空、相加、换成缓存子集或局部估算都必须失败。
    let start = json!({
        "type":"message_start",
        "message":{
            "id":"msg_early_accounting",
            "type":"message",
            "role":"assistant",
            "content":[],
            "model":"claude-sonnet-4-5",
            "stop_reason":null,
            "stop_sequence":null,
            "usage":{
                "input_tokens":37,
                "output_tokens":0,
                "cache_creation_input_tokens":11,
                "cache_read_input_tokens":23,
                "server_tool_use":{"web_search_requests":7,"web_fetch_requests":0}
            }
        }
    });
    let mut sse = format!("event: message_start\ndata: {start}\n\n");
    if matches!(ending, Ending::Overloaded) {
        sse.push_str(
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
    }
    sse
}

fn app(base: &str, configured_timeout: bool) -> (axum::Router, mpsc::Receiver<UsageEvent>) {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"component-upstream",
        "provider":"anthropic",
        "adapter":"anthropic",
        "api_base":format!("{base}/v1"),
        "secret":"component-only-key"
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER_KEY_ID, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"early-accounting",
        "provider":"anthropic",
        "model_name":"claude-sonnet-4-5",
        "provider_key_id":PROVIDER_KEY_ID,
        "retries":0,
        "timeout":0,
        "stream_timeout":if configured_timeout { 10_000 } else { 0 }
    }))
    .unwrap();
    snapshot
        .models
        .insert(ResourceEntry::new(MODEL_ID, model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("caller"),
        "allowed_models":["early-accounting"]
    }))
    .unwrap();
    snapshot
        .apikeys
        .insert(ResourceEntry::new("caller-id", key, 1));
    let hub = Arc::new(Hub::new());
    hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
    let (tx, events) = mpsc::channel(16);
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_usage_sink(UsageSink::new(tx));
    (build_router(state), events)
}

fn assert_no_assistant_content(client: Client, frames: &[Value]) {
    for frame in frames {
        if let Some(choices) = frame.get("choices").and_then(Value::as_array) {
            for choice in choices {
                for key in ["delta", "message"] {
                    let Some(message) = choice.get(key) else {
                        continue;
                    };
                    for text in ["content", "reasoning_content", "refusal"] {
                        assert!(
                            message.get(text).is_none_or(|value| {
                                value.is_null() || value.as_str() == Some("")
                            }),
                            "{client:?}: invented assistant {text}: {frame}"
                        );
                    }
                    assert!(
                        message.get("tool_calls").is_none_or(|value| {
                            value.is_null() || value.as_array().is_some_and(Vec::is_empty)
                        }),
                        "{client:?}: invented tool calls: {frame}"
                    );
                    assert!(
                        message.get("function_call").is_none(),
                        "{client:?}: invented function call: {frame}"
                    );
                }
            }
        }
        assert!(
            !matches!(
                frame.get("type").and_then(Value::as_str),
                Some(
                    "response.output_item.added"
                        | "response.content_part.added"
                        | "response.output_text.delta"
                        | "response.reasoning_summary_text.delta"
                        | "response.refusal.delta"
                        | "response.function_call_arguments.delta"
                )
            ),
            "{client:?}: invented generated output: {frame}"
        );
        if let Some(output) = frame.pointer("/response/output") {
            assert_eq!(output, &json!([]), "{client:?}: {frame}");
        }
    }
}

async fn exercise(client: Client, ending: Ending, configured_timeout: bool) {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(upstream_sse(ending), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let (app, mut events) = app(&upstream.uri(), configured_timeout);
    let request = Request::builder()
        .method("POST")
        .uri(client.path())
        .header("authorization", "Bearer caller")
        .header("content-type", "application/json")
        .body(Body::from(client.request().to_string()))
        .unwrap();
    // 所有 Router 和响应 body 的 sink 所有者都会在收集完成后释放；
    // 用 channel 关闭作终态屏障，不能靠 sleep 后 try_recv 猜没有第二条。
    let response = tokio::time::timeout(DEADLINE, app.oneshot(request))
        .await
        .expect("proxy response stalled")
        .unwrap();
    let status = response.status();
    let bytes = tokio::time::timeout(DEADLINE, to_bytes(response.into_body(), 65_536))
        .await
        .expect("proxy body stalled")
        .unwrap();
    let frames = if status == StatusCode::OK {
        std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|data| !data.is_empty() && *data != "[DONE]")
            .map(|data| serde_json::from_str(data).expect("valid client SSE JSON"))
            .collect::<Vec<Value>>()
    } else {
        vec![serde_json::from_slice(&bytes).expect("valid client error JSON")]
    };
    assert_eq!(
        status,
        if configured_timeout {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::OK
        },
        "{client:?}/{ending:?}/timeout={configured_timeout}: {frames:?}"
    );
    assert!(
        frames.iter().any(|frame| {
            frame.get("error").is_some() || frame.get("type") == Some(&json!("response.failed"))
        }),
        "{client:?}/{ending:?}: client must observe the upstream failure: {frames:?}"
    );
    assert_no_assistant_content(client, &frames);

    let rows = tokio::time::timeout(DEADLINE, async {
        let mut rows = Vec::new();
        while let Some(event) = events.recv().await {
            rows.push(event);
        }
        rows
    })
    .await
    .expect("usage sink did not close after response completion");
    assert_eq!(rows.len(), 1, "exactly one terminal row: {rows:?}");
    let event = &rows[0];
    assert!(event.is_final_attempt);
    assert!(
        !event.usage_estimated,
        "reported output zero must not be estimated"
    );
    let wire = serde_json::to_value(event).unwrap();
    for (field, expected) in [
        ("prompt_tokens", json!(37)),
        ("completion_tokens", json!(0)),
        ("cache_creation_tokens", json!(11)),
        ("cache_read_tokens", json!(23)),
        ("web_search_requests", json!(7)),
        ("cache_usage_reported", json!(true)),
        ("status_code", json!(502)),
        ("error_class", json!(ending.error_class())),
        ("attempt_index", json!(0)),
    ] {
        assert_eq!(
            wire.get(field),
            Some(&expected),
            "{client:?}/{ending:?}/timeout={configured_timeout}: missing or wrong {field}: {wire}"
        );
    }
    assert!(wire.get("first_token_latency_ms").is_none(), "{wire}");
    assert!(wire.get("cached_prompt_tokens").is_none(), "{wire}");
    assert!(wire.get("total_tokens").is_none(), "{wire}");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "retries=0 must make one upstream attempt"
    );
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent.get("stream"), Some(&json!(true)));
}

macro_rules! cases {
    ($module:ident, $client:expr) => {
        mod $module {
            use super::*;

            #[tokio::test]
            async fn overloaded_after_start_with_read_timeout() {
                exercise($client, Ending::Overloaded, true).await;
            }

            #[tokio::test]
            async fn overloaded_after_start_without_read_timeout() {
                exercise($client, Ending::Overloaded, false).await;
            }

            #[tokio::test]
            async fn eof_after_start_with_read_timeout() {
                exercise($client, Ending::Eof, true).await;
            }

            #[tokio::test]
            async fn eof_after_start_without_read_timeout() {
                exercise($client, Ending::Eof, false).await;
            }
        }
    };
}

cases!(chat, Client::Chat);
cases!(responses, Client::Responses);
