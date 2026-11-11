//! 首内容交付计时的组件测试：真实 router + 可控 HTTP 上游；不是 provider E2E。
//! 只观察客户端字节与 UsageEvent JSON，缺字段必须失败，不能把缺失解成零。

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_provider_anthropic::AnthropicBridge;
use aisix_provider_openai::OpenAiBridge;
use aisix_proxy::{build_router, ProxyState};
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, Notify};
use tower::ServiceExt;

const WAIT: Duration = Duration::from_millis(100);
const TIMEOUT: Duration = Duration::from_secs(15);
const PK: &str = "11111111-1111-1111-1111-111111111111";

#[derive(Clone, Copy, Debug)]
enum Wire {
    Chat,
    Completions,
    Messages,
    Responses,
}

impl Wire {
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Completions => "/v1/completions",
            Self::Messages => "/v1/messages",
            Self::Responses => "/v1/responses",
        }
    }
    fn request(self, stream: bool) -> Value {
        match self {
            Self::Chat | Self::Messages => {
                json!({"model":"test-model","messages":[{"role":"user","content":"hello"}],"max_tokens":16,"stream":stream})
            }
            Self::Completions => {
                json!({"model":"test-model","prompt":"hello","max_tokens":16,"stream":stream})
            }
            Self::Responses => json!({"model":"test-model","input":"hello","stream":stream}),
        }
    }
    fn metadata(self) -> String {
        let v = match self {
            Self::Chat => {
                json!({"id":"chat-test","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]})
            }
            Self::Completions => {
                json!({"id":"cmpl-test","model":"gpt-4o","choices":[{"index":0,"text":"","finish_reason":null}]})
            }
            Self::Messages => {
                json!({"type":"message_start","message":{"id":"msg-test","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"usage":{"input_tokens":2,"output_tokens":0}}})
            }
            Self::Responses => {
                json!({"type":"response.created","response":{"id":"resp_test","object":"response","model":"gpt-4o","status":"in_progress","output":[]}})
            }
        };
        format!(": heartbeat\n\n{}", self.event(v))
    }
    fn event(self, v: Value) -> String {
        match v.get("type").and_then(Value::as_str) {
            Some(kind) => format!("event: {kind}\ndata: {v}\n\n"),
            None => format!("data: {v}\n\n"),
        }
    }
    fn content(self) -> String {
        self.event(match self {
            Self::Chat => json!({"id":"chat-test","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"hello-token"},"finish_reason":null}]}),
            Self::Completions => json!({"id":"cmpl-test","model":"gpt-4o","choices":[{"index":0,"text":"hello-token","finish_reason":null}]}),
            Self::Messages => json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello-token"}}),
            Self::Responses => json!({"type":"response.output_text.delta","item_id":"msg_test","output_index":0,"content_index":0,"delta":"hello-token"}),
        })
    }
    fn end(self) -> String {
        match self {
            Self::Chat => format!("{}data: [DONE]\n\n", self.event(json!({"id":"chat-test","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}))),
            Self::Completions => format!("{}data: [DONE]\n\n", self.event(json!({"id":"cmpl-test","model":"gpt-4o","choices":[{"index":0,"text":"","finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}))),
            Self::Messages => format!("{}{}", self.event(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}})), self.event(json!({"type":"message_stop"}))),
            Self::Responses => self.event(json!({"type":"response.completed","response":{"id":"resp_test","object":"response","model":"gpt-4o","status":"completed","output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}})),
        }
    }
    fn terminal_received(self, output: &str) -> bool {
        match self {
            Self::Chat | Self::Completions => output.contains("data: [DONE]"),
            Self::Messages => output.contains("event: message_stop"),
            Self::Responses => output.contains("event: response.completed"),
        }
    }

    fn nonstream(self) -> String {
        match self {
            Self::Chat => json!({"id":"chat-test","model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hello-token"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}),
            Self::Completions => json!({"id":"cmpl-test","model":"gpt-4o","choices":[{"index":0,"text":"hello-token","finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}),
            Self::Messages => json!({"id":"msg-test","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hello-token"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}}),
            Self::Responses => json!({"id":"resp_test","object":"response","model":"gpt-4o","status":"completed","output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}),
        }.to_string()
    }
}

struct Harness {
    app: axum::Router,
    tx: mpsc::Sender<Bytes>,
    events: mpsc::Receiver<UsageEvent>,
    request_seen: Arc<Notify>,
    upstream: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

impl Harness {
    async fn new(wire: Wire, stream: bool, held: bool) -> Self {
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let receiver = Arc::new(Mutex::new(Some(rx)));
        let request_seen = Arc::new(Notify::new());
        let seen = request_seen.clone();
        let upstream = axum::Router::new().fallback(move || {
            let receiver = receiver.clone();
            let seen = seen.clone();
            async move {
                seen.notify_one();
                let rx = receiver.lock().await.take().expect("one upstream request");
                let stream_body = futures::stream::unfold(rx, |mut rx| async {
                    rx.recv()
                        .await
                        .map(|bytes| (Ok::<_, std::io::Error>(bytes), rx))
                });
                (
                    [(
                        "content-type",
                        if stream {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )],
                    Body::from_stream(stream_body),
                )
                    .into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let snap = AisixSnapshot::new();
        let provider = if matches!(wire, Wire::Messages) {
            "anthropic"
        } else {
            "openai"
        };
        let pk: ProviderKey = serde_json::from_value(json!({"display_name":"upstream","secret":"component-only-key","api_base":format!("http://{addr}/v1"),"provider":provider,"adapter":provider})).unwrap();
        snap.provider_keys.insert(ResourceEntry::new(PK, pk, 1));
        let model: Model = serde_json::from_value(json!({"display_name":"test-model","provider":provider,"model_name":if provider == "anthropic" { "claude-sonnet-4-5" } else { "gpt-4o" },"provider_key_id":PK})).unwrap();
        snap.models
            .insert(ResourceEntry::new("test-model-id", model, 1));
        let key: ApiKey = serde_json::from_value(
            json!({"key_hash":ApiKey::hash_bearer("caller"),"allowed_models":["test-model"]}),
        )
        .unwrap();
        snap.apikeys.insert(ResourceEntry::new("caller-id", key, 1));
        if held {
            let guardrail = serde_json::from_value::<aisix_core::Guardrail>(json!({"name":"hold-output","kind":"keyword","hook_point":"output","patterns":[{"kind":"literal","value":"never-match-this-output"}]})).unwrap();
            snap.guardrails
                .insert(ResourceEntry::new("hold", guardrail, 1));
            let attachment = serde_json::from_value(
                json!({"guardrail_id":"hold","scope_type":"env","priority":100}),
            )
            .unwrap();
            snap.guardrail_attachments
                .insert(ResourceEntry::new("hold-attachment", attachment, 1));
        }
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
        let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
        let (event_tx, events) = mpsc::channel(16);
        let state = ProxyState::new(SnapshotHandle::new(snap), hub, &config)
            .without_cache()
            .with_usage_sink(UsageSink::new(event_tx));
        Self {
            app: build_router(state),
            tx,
            events,
            request_seen,
            upstream: server,
        }
    }
    async fn send(&self, wire: Wire, stream: bool) -> axum::response::Response {
        let request = Request::builder()
            .method("POST")
            .uri(wire.path())
            .header("authorization", "Bearer caller")
            .header("content-type", "application/json")
            .body(Body::from(wire.request(stream).to_string()))
            .unwrap();
        let response = tokio::time::timeout(TIMEOUT, self.app.clone().oneshot(request))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response
    }
    async fn event(&mut self) -> Value {
        let event = tokio::time::timeout(TIMEOUT, self.events.recv())
            .await
            .expect("missing usage event")
            .unwrap();
        assert!(event.is_final_attempt);
        assert!(self.events.try_recv().is_err(), "duplicate usage event");
        serde_json::to_value(event).unwrap()
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    MetadataFirst,
    CombinedAndClose,
    Held,
    CancelBefore,
    CancelAfter,
    Empty,
    Nonstream,
}

async fn exercise(wire: Wire, scenario: Scenario) {
    let stream = !matches!(scenario, Scenario::Nonstream);
    let mut h = Harness::new(wire, stream, matches!(scenario, Scenario::Held)).await;
    let tx = h.tx.clone();
    let started = Instant::now();
    let seen = h.request_seen.clone();
    let delivered = Arc::new(Notify::new());
    let delivered_to_producer = delivered.clone();
    let producer = tokio::spawn(async move {
        let first = match scenario {
            Scenario::Nonstream => wire.nonstream(),
            Scenario::CombinedAndClose => format!("{}{}", wire.content(), wire.end()),
            Scenario::Held => format!("{}{}", wire.metadata(), wire.content()),
            _ => wire.metadata(),
        };
        tx.send(Bytes::from(first)).await.unwrap();
        if matches!(scenario, Scenario::MetadataFirst | Scenario::CancelAfter) {
            delivered_to_producer.notified().await;
        } else {
            seen.notified().await;
        }
        tokio::time::sleep(WAIT).await;
        match scenario {
            Scenario::MetadataFirst | Scenario::CancelAfter => {
                let _ = tx.send(Bytes::from(wire.content())).await;
            }
            _ => {}
        }
        if matches!(scenario, Scenario::CancelBefore | Scenario::CancelAfter) {
            // 保持上游未结束，客户端取消不能被正常 EOF 冒充。
            std::future::pending::<()>().await;
        }
        if !matches!(scenario, Scenario::Nonstream | Scenario::CombinedAndClose) {
            let _ = tx.send(Bytes::from(wire.end())).await;
        }
    });
    // 只让 producer 持有 sender，正常场景终止后 HTTP body 才能 EOF。
    let (dummy, _) = mpsc::channel(1);
    h.tx = dummy;
    let response = h.send(wire, stream).await;
    let mut body = response.into_body().into_data_stream();
    let mut output = String::new();
    let mut first_content_ms = None;
    loop {
        let next = tokio::time::timeout(TIMEOUT, body.next())
            .await
            .expect("body stalled");
        let Some(next) = next else {
            break;
        };
        output.push_str(&String::from_utf8_lossy(&next.unwrap()));
        delivered.notify_one();
        if output.contains("hello-token") && first_content_ms.is_none() {
            first_content_ms = Some(started.elapsed().as_millis() as u64);
        }
        if matches!(scenario, Scenario::CancelBefore)
            || (first_content_ms.is_some() && matches!(scenario, Scenario::CancelAfter))
            || (first_content_ms.is_some()
                && matches!(scenario, Scenario::CombinedAndClose)
                && wire.terminal_received(&output))
        {
            break;
        }
    }
    // completions 已在本次首内容 poll 内终结，不能靠下一次 poll 或 Drop 才计费。
    let emitted_before_drop =
        if matches!(wire, Wire::Completions) && matches!(scenario, Scenario::CombinedAndClose) {
            Some(h.event().await)
        } else {
            None
        };
    drop(body);
    let event = match emitted_before_drop {
        Some(event) => event,
        None => h.event().await,
    };
    assert!(
        h.events.try_recv().is_err(),
        "body drop duplicated terminal event"
    );
    producer.abort();
    if matches!(
        scenario,
        Scenario::CancelBefore | Scenario::Empty | Scenario::Nonstream
    ) {
        assert!(
            event.get("first_token_latency_ms").is_none(),
            "no delivered stream content: {event}"
        );
    } else {
        let latency = event
            .get("first_token_latency_ms")
            .and_then(Value::as_u64)
            .expect("delivered content needs an explicit first_token_latency_ms");
        let observed = first_content_ms.expect("client must receive content");
        assert!(
            latency <= observed + 10,
            "metric {latency} exceeds delivery {observed}"
        );
        if matches!(
            scenario,
            Scenario::MetadataFirst | Scenario::Held | Scenario::CancelAfter
        ) {
            assert!(
                latency >= WAIT.as_millis() as u64 - 10,
                "metadata/upstream arrival is not client content delivery: {event}"
            );
        }
    }
    if matches!(scenario, Scenario::CancelBefore | Scenario::CancelAfter) {
        assert_eq!(event["status_code"], 499);
    } else {
        assert_eq!(event["status_code"], 200);
    }
}

macro_rules! cases {
    ($module:ident, $wire:expr) => {
        mod $module {
            use super::*;
            #[tokio::test]
            async fn metadata_first() {
                exercise($wire, Scenario::MetadataFirst).await;
            }
            #[tokio::test]
            async fn combined_terminal_close() {
                exercise($wire, Scenario::CombinedAndClose).await;
            }
            #[tokio::test]
            async fn held_release() {
                exercise($wire, Scenario::Held).await;
            }
            #[tokio::test]
            async fn cancel_before_content() {
                exercise($wire, Scenario::CancelBefore).await;
            }
            #[tokio::test]
            async fn cancel_after_content() {
                exercise($wire, Scenario::CancelAfter).await;
            }
            #[tokio::test]
            async fn metadata_only() {
                exercise($wire, Scenario::Empty).await;
            }
            #[tokio::test]
            async fn nonstream() {
                exercise($wire, Scenario::Nonstream).await;
            }
        }
    };
}
cases!(chat, Wire::Chat);
cases!(completions, Wire::Completions);
cases!(messages, Wire::Messages);
cases!(responses, Wire::Responses);
