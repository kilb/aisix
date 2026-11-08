//! Platform mode relays a native upstream's request error as the upstream
//! sent it (`crate::platform_errors::relayed_status`), on every endpoint
//! family, buffered and as the frame that ends a stream, less what the
//! shared scrubber takes out. Everything else keeps the fixed classes.
//!
//! Each test drives the router against a mock upstream, so the error is read,
//! scrubbed and rendered by the same code a real request goes through. The
//! upstream quotes the credential the gateway sent it, which must not reach
//! the caller.

use std::sync::Arc;

use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProxyConfig};
use aisix_gateway::Hub;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The provider key's secret, which the gateway sends upstream and the
/// upstream quotes back. Shaped so that only the exact-value pass of the
/// scrubber can find it, not a key pattern.
const SECRET: &str = "relay-secret-0f8e7d6c5b4a";
const REDACTED: &str = aisix_gateway::upstream_error_scrub::REDACTED;
const PK_ID: &str = "pk-relay";

#[derive(Clone, Copy)]
enum Upstream {
    /// An OpenAI-wire key: chat, completions, embeddings and Responses
    /// natively, Messages through a translation.
    OpenAi,
    /// An Anthropic-wire key: Messages and count_tokens natively, chat
    /// through a translation.
    Anthropic,
}

fn cfg() -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1:0".into(),
        request_body_limit_bytes: 1_048_576,
        real_ip: Default::default(),
        request_id: Default::default(),
        url_rewrites: Vec::new(),
        expose_debug_headers: false,
        platform: Default::default(),
        tls: None,
        listeners: Vec::new(),
        thread_per_core: None,
        workers: None,
    }
}

fn app(upstream: &MockServer, kind: Upstream, platform: bool) -> axum::Router {
    let (provider, model_name) = match kind {
        Upstream::OpenAi => ("openai", "gpt-4o"),
        Upstream::Anthropic => ("anthropic", "claude-haiku-4-5"),
    };
    let snap = AisixSnapshot::new();
    let pk: aisix_core::ProviderKey = serde_json::from_value(json!({
        "display_name": "relay-up",
        "secret": SECRET,
        "api_base": upstream.uri(),
        "provider": provider,
        "adapter": provider,
    }))
    .unwrap();
    snap.provider_keys.insert(ResourceEntry::new(PK_ID, pk, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name": "m",
        "provider": provider,
        "model_name": model_name,
        "provider_key_id": PK_ID,
    }))
    .unwrap();
    snap.models.insert(ResourceEntry::new("m-1", model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash": ApiKey::hash_bearer("sk-caller"),
        "allowed_models": ["*"],
    }))
    .unwrap();
    snap.apikeys.insert(ResourceEntry::new("k-1", key, 1));

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::with_client(client)),
    );
    hub.register_specialized(
        "anthropic",
        Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
    );
    let state = crate::ProxyState::new(SnapshotHandle::new(snap), hub, &cfg()).without_cache();
    let state = if platform {
        state.with_platform(crate::platform::PlatformSettings::from_secret("relay"))
    } else {
        state
    };
    crate::build_router(state)
}

/// The upstream answers every POST to a path ending in `suffix` with
/// `status` and `body`.
async fn upstream(suffix: &str, status: u16, body: &str, content_type: &str) -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path_regex(format!("{suffix}$")))
        .respond_with(ResponseTemplate::new(status).set_body_raw(body.to_owned(), content_type))
        .mount(&server)
        .await;
    server
}

async fn json_upstream(suffix: &str, status: u16, body: Value) -> MockServer {
    upstream(suffix, status, &body.to_string(), "application/json").await
}

async fn sse_upstream(suffix: &str, frames: &[String]) -> MockServer {
    upstream(suffix, 200, &frames.concat(), "text/event-stream").await
}

async fn post(app: axum::Router, uri: &str, body: Value) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", "Bearer sk-caller")
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn openai_error(message: &str, kind: &str, param: Option<&str>, code: Option<&str>) -> Value {
    json!({"error": {"message": message, "type": kind, "param": param, "code": code}})
}

fn anthropic_error(kind: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

/// The upstream's text as the caller must receive it: the credential out.
fn scrubbed(text: &str) -> String {
    text.replace(SECRET, REDACTED)
}

fn chat_body(stream: bool) -> Value {
    json!({"model": "m", "stream": stream, "messages": [{"role": "user", "content": "hi"}]})
}

fn messages_body(stream: bool) -> Value {
    json!({"model": "m", "stream": stream, "max_tokens": 64, "messages": [{"role": "user", "content": "hi"}]})
}

/// Every JSON payload of an SSE body, with the `event:` name it came under.
fn sse_payloads(text: &str) -> Vec<(Option<String>, Value)> {
    text.split("\n\n")
        .filter_map(|frame| {
            let mut name = None;
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(n) = line.strip_prefix("event:") {
                    name = Some(n.trim().to_string());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
            serde_json::from_str(&data).ok().map(|v| (name, v))
        })
        .collect()
}

// ── buffered: one native request error per family ──────────────────────────

/// An OpenAI-shape context overflow on chat reaches the caller with the
/// upstream's own status, type, code, param and message: what the OpenAI SDK
/// and Codex key off.
#[tokio::test]
async fn chat_relays_a_native_400_as_the_upstream_sent_it() {
    let text = format!(
        "This model's maximum context length is 128000 tokens. However, your messages \
         resulted in 250000 tokens. Request signed with {SECRET}."
    );
    let up = json_upstream(
        "chat/completions",
        400,
        openai_error(
            &text,
            "invalid_request_error",
            Some("messages"),
            Some("context_length_exceeded"),
        ),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/chat/completions",
        chat_body(false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], scrubbed(&text), "{body}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "context_length_exceeded");
    assert_eq!(v["error"]["param"], "messages");
    assert!(!body.contains(SECRET), "{body}");
}

/// Claude Code compacts its conversation when the message says
/// "prompt is too long"; the Anthropic envelope carries the upstream's words.
#[tokio::test]
async fn messages_relays_a_native_400_in_the_anthropic_envelope() {
    let text = format!("prompt is too long: 250000 tokens > 200000 maximum ({SECRET})");
    let up = json_upstream(
        "messages",
        400,
        anthropic_error("invalid_request_error", &text),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::Anthropic, true),
        "/v1/messages",
        messages_body(false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v,
        anthropic_error("invalid_request_error", &scrubbed(&text)),
        "{body}"
    );
}

/// A request too large keeps its own status and Anthropic type.
#[tokio::test]
async fn count_tokens_relays_a_native_413() {
    let text = format!("Request exceeds the maximum allowed number of bytes. ({SECRET})");
    let up = json_upstream(
        "count_tokens",
        413,
        anthropic_error("request_too_large", &text),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::Anthropic, true),
        "/v1/messages/count_tokens",
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v,
        anthropic_error("request_too_large", &scrubbed(&text)),
        "{body}"
    );
}

/// The upstream's own `previous_response_not_found` reaches the caller with
/// its code, which the OpenAI SDK tells clients to handle.
#[tokio::test]
async fn responses_relays_the_upstreams_previous_response_not_found() {
    let text = format!("Previous response with id 'resp_up_1' not found. ({SECRET})");
    let up = json_upstream(
        "responses",
        400,
        openai_error(
            &text,
            "invalid_request_error",
            Some("previous_response_id"),
            Some("previous_response_not_found"),
        ),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/responses",
        json!({"model": "m", "input": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], scrubbed(&text), "{body}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "previous_response_not_found");
    assert_eq!(v["error"]["param"], "previous_response_id");
}

#[tokio::test]
async fn completions_relays_a_native_400() {
    let text = format!("Invalid value for 'max_tokens': 999999. ({SECRET})");
    let up = json_upstream(
        "completions",
        400,
        openai_error(
            &text,
            "invalid_request_error",
            Some("max_tokens"),
            Some("invalid_value"),
        ),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/completions",
        json!({"model": "m", "prompt": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], scrubbed(&text), "{body}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "invalid_value");
    assert_eq!(v["error"]["param"], "max_tokens");
}

/// An unprocessable request keeps its own status.
#[tokio::test]
async fn embeddings_relays_a_native_422() {
    let text = format!("Input is not a list of token ids or strings. ({SECRET})");
    let up = json_upstream(
        "embeddings",
        422,
        openai_error(&text, "invalid_request_error", Some("input"), None),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/embeddings",
        json!({"model": "m", "input": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], scrubbed(&text), "{body}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["param"], "input");
}

// ── buffered: what keeps its class ─────────────────────────────────────────

/// A translated dispatch's error is in another protocol's words: the caller
/// gets the class, and none of the upstream's text.
#[tokio::test]
async fn a_translated_dispatchs_400_keeps_its_class() {
    let text = format!("This model's maximum context length is 8192 tokens. ({SECRET})");
    // Messages reaching an OpenAI-wire upstream through chat completions.
    let up = json_upstream(
        "chat/completions",
        400,
        openai_error(
            &text,
            "invalid_request_error",
            Some("messages"),
            Some("context_length_exceeded"),
        ),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/messages",
        messages_body(false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "context_length_exceeded");
    assert_eq!(
        v["error"]["message"],
        "prompt is too long: the request exceeds the model's context length."
    );
    assert!(v["error"].get("param").is_none(), "{body}");
    assert!(!body.contains("8192"), "{body}");

    // Chat reaching an Anthropic-wire upstream through a translation.
    let text = format!("messages.0.content: Field required ({SECRET})");
    let up = json_upstream(
        "messages",
        400,
        anthropic_error("invalid_request_error", &text),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::Anthropic, true),
        "/v1/chat/completions",
        chat_body(false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "invalid_request");
    assert_eq!(v["error"]["message"], "The request is invalid.");
    assert!(v["error"].get("param").is_none(), "{body}");
    assert!(!body.contains("Field required"), "{body}");
}

/// A native upstream's 401, 429 and 5xx are not request errors: they keep
/// the classes, with none of the upstream's text.
#[tokio::test]
async fn a_native_401_429_and_5xx_keep_their_classes() {
    for (status, kind, code, expected_status, expected_code) in [
        (
            401,
            "invalid_request_error",
            "invalid_api_key",
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
        ),
        (
            429,
            "requests",
            "rate_limit_exceeded",
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
        ),
        (
            500,
            "server_error",
            "server_error",
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
        ),
    ] {
        let text = format!("upstream says {status} for account acct-9 ({SECRET})");
        let up = json_upstream(
            "chat/completions",
            status,
            openai_error(&text, kind, None, Some(code)),
        )
        .await;
        let (got, body) = post(
            app(&up, Upstream::OpenAi, true),
            "/v1/chat/completions",
            chat_body(false),
        )
        .await;
        assert_eq!(got, expected_status, "{status}: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["code"], expected_code, "{status}: {body}");
        assert!(!body.contains("acct-9"), "{status}: {body}");
    }
}

/// Off platform mode a native 400 renders as it always has.
#[tokio::test]
async fn off_platform_mode_a_native_400_renders_as_it_always_has() {
    let text = "Invalid value for 'temperature'.";
    let up = json_upstream(
        "chat/completions",
        400,
        openai_error(text, "invalid_request_error", Some("temperature"), None),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, false),
        "/v1/chat/completions",
        chat_body(false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], text);
    assert_eq!(v["error"]["type"], "upstream_error");
    assert_eq!(v["error"]["param"], "temperature");
}

// ── streams: a native request error mid-stream ─────────────────────────────

#[tokio::test]
async fn a_chat_stream_ends_on_the_native_upstreams_request_error() {
    let text = format!("This model's maximum context length is 128000 tokens. ({SECRET})");
    let up = sse_upstream(
        "chat/completions",
        &[
            format!(
                "data: {}\n\n",
                json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
                       "model": "gpt-4o", "choices": [{"index": 0,
                       "delta": {"role": "assistant", "content": "Hel"}, "finish_reason": null}]})
            ),
            format!(
                "data: {}\n\n",
                openai_error(
                    &text,
                    "invalid_request_error",
                    Some("messages"),
                    Some("context_length_exceeded")
                )
            ),
        ],
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/chat/completions",
        chat_body(true),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let error = sse_payloads(&body)
        .into_iter()
        .find_map(|(_, v)| v.get("error").cloned())
        .unwrap_or_else(|| panic!("no error frame: {body}"));
    assert_eq!(error["message"], scrubbed(&text), "{body}");
    assert_eq!(error["type"], "invalid_request_error");
    assert_eq!(error["code"], "context_length_exceeded");
    assert_eq!(error["param"], "messages");
}

#[tokio::test]
async fn a_native_messages_stream_relays_the_upstreams_request_error_event() {
    let text = format!("messages.1.content.0.image.source: image exceeds 5 MB maximum ({SECRET})");
    let up = sse_upstream(
        "messages",
        &[
            format!(
                "event: message_start\ndata: {}\n\n",
                json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
                       "role": "assistant", "model": "claude-haiku-4-5", "content": [],
                       "stop_reason": null, "stop_sequence": null,
                       "usage": {"input_tokens": 5, "output_tokens": 1}}})
            ),
            format!(
                "event: error\ndata: {}\n\n",
                anthropic_error("invalid_request_error", &text)
            ),
        ],
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::Anthropic, true),
        "/v1/messages",
        messages_body(true),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (name, error) = sse_payloads(&body)
        .into_iter()
        .find(|(_, v)| v["type"] == "error")
        .unwrap_or_else(|| panic!("no error event: {body}"));
    assert_eq!(name.as_deref(), Some("error"));
    assert_eq!(
        error,
        anthropic_error("invalid_request_error", &scrubbed(&text)),
        "{body}"
    );
}

/// Codex reads a failed Response's own code: `context_length_exceeded` is a
/// context overflow it can act on.
#[tokio::test]
async fn a_native_responses_stream_relays_the_upstreams_failed_response() {
    let text = format!("Your input exceeds the context window of this model. ({SECRET})");
    let up = sse_upstream(
        "responses",
        &[
            format!(
                "event: response.created\ndata: {}\n\n",
                json!({"type": "response.created", "sequence_number": 0,
                       "response": {"id": "resp_up_1", "object": "response", "created_at": 1,
                                    "status": "in_progress", "model": "gpt-4o", "output": []}})
            ),
            format!(
                "event: response.failed\ndata: {}\n\n",
                json!({"type": "response.failed", "sequence_number": 1,
                       "response": {"id": "resp_up_1", "object": "response", "created_at": 1,
                                    "status": "failed", "model": "gpt-4o", "output": [],
                                    "error": {"code": "context_length_exceeded", "message": text}}})
            ),
        ],
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/responses",
        json!({"model": "m", "input": "hi", "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (name, failed) = sse_payloads(&body)
        .into_iter()
        .find(|(_, v)| v["type"] == "response.failed")
        .unwrap_or_else(|| panic!("no response.failed: {body}"));
    assert_eq!(name.as_deref(), Some("response.failed"));
    assert_eq!(failed["response"]["status"], "failed");
    assert_eq!(
        failed["response"]["error"],
        json!({"code": "context_length_exceeded", "message": scrubbed(&text)}),
        "{body}"
    );
}

/// A buffered failed Response whose code blames the request is relayed too.
#[tokio::test]
async fn a_native_failed_response_object_relays_its_request_error() {
    let text =
        format!("Invalid prompt: flagged as potentially violating our usage policy. ({SECRET})");
    let up = json_upstream(
        "responses",
        200,
        json!({"id": "resp_up_1", "object": "response", "created_at": 1, "status": "failed",
               "model": "gpt-4o", "output": [],
               "error": {"code": "invalid_prompt", "message": text}}),
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/responses",
        json!({"model": "m", "input": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "failed");
    assert_eq!(
        v["error"],
        json!({"code": "invalid_prompt", "message": scrubbed(&text)}),
        "{body}"
    );
}

#[tokio::test]
async fn a_native_completions_stream_relays_the_upstreams_request_error() {
    let text = format!("This model's maximum context length is 4097 tokens. ({SECRET})");
    let up = sse_upstream(
        "completions",
        &[
            format!(
                "data: {}\n\n",
                json!({"id": "cmpl-1", "object": "text_completion", "created": 1,
                       "model": "gpt-4o", "choices": [{"index": 0, "text": "Hel",
                       "finish_reason": null}]})
            ),
            format!(
                "data: {}\n\n",
                openai_error(
                    &text,
                    "invalid_request_error",
                    Some("prompt"),
                    Some("context_length_exceeded")
                )
            ),
        ],
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::OpenAi, true),
        "/v1/completions",
        json!({"model": "m", "prompt": "hi", "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let error = sse_payloads(&body)
        .into_iter()
        .find_map(|(_, v)| v.get("error").cloned())
        .unwrap_or_else(|| panic!("no error frame: {body}"));
    assert_eq!(error["message"], scrubbed(&text), "{body}");
    assert_eq!(error["code"], "context_length_exceeded");
    assert_eq!(error["param"], "prompt");
}

/// A server error mid-stream is not a request error: the stream ends on the
/// class, as it did.
#[tokio::test]
async fn a_native_stream_server_error_keeps_its_class() {
    let up = sse_upstream(
        "messages",
        &[
            format!(
                "event: message_start\ndata: {}\n\n",
                json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
                       "role": "assistant", "model": "claude-haiku-4-5", "content": [],
                       "stop_reason": null, "stop_sequence": null,
                       "usage": {"input_tokens": 5, "output_tokens": 1}}})
            ),
            format!(
                "event: error\ndata: {}\n\n",
                anthropic_error("overloaded_error", "Overloaded in us-east-1")
            ),
        ],
    )
    .await;
    let (status, body) = post(
        app(&up, Upstream::Anthropic, true),
        "/v1/messages",
        messages_body(true),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, error) = sse_payloads(&body)
        .into_iter()
        .find(|(_, v)| v["type"] == "error")
        .unwrap_or_else(|| panic!("no error event: {body}"));
    assert_eq!(error["error"]["code"], "upstream_unavailable", "{body}");
    assert!(!body.contains("us-east-1"), "{body}");
}
