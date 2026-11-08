//! Platform mode relays a native upstream's request error as the upstream
//! sent it (`crate::platform_errors::relayed_status`), on every endpoint
//! family, buffered and as the frame that ends a stream, less what the
//! shared scrubber takes out. Everything else keeps the fixed classes.
//!
//! Each test drives the router against a mock upstream, so the error is read,
//! scrubbed and rendered by the same code a real request goes through. The
//! upstream quotes the credential the gateway sent it, and names an AWS ARN
//! and account id, a Google Cloud project and an internal host
//! ([`INFRA`]), none of which may reach the caller. Only the scrubber's
//! pattern pass takes the second out: a relayed frame that skipped its
//! rewrite still loses the credential through the pass over its bytes.

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
/// Infrastructure an upstream's text can name: an AWS ARN, a bare AWS account
/// id, a Google Cloud project and an internal host.
const INFRA: &str = "via arn:aws:bedrock:us-east-1:123456789012:inference-profile/relay \
                     in account 210987654321, projects/acme-prod-4411/locations/us-central1, \
                     http://10.0.3.7:8080/v1";
/// [`INFRA`] as the caller must receive it.
const INFRA_SCRUBBED: &str = "via <redacted> in account <redacted>, \
                              projects/<redacted>/locations/us-central1, http://<redacted>/v1";
/// What none of the caller's bytes may contain.
const LEAKS: &[&str] = &[
    SECRET,
    "123456789012",
    "210987654321",
    "acme-prod-4411",
    "10.0.3.7",
];

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
    app_with(upstream, kind, platform, false)
}

/// [`app`], with a block-capable output guardrail that holds a stream back
/// whole when `held`. Its keyword is in no answer here.
fn app_with(upstream: &MockServer, kind: Upstream, platform: bool, held: bool) -> axum::Router {
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
    if held {
        let row: aisix_core::Guardrail = serde_json::from_value(json!({
            "name": "held",
            "enabled": true,
            "hook_point": "output",
            "kind": "keyword",
            "patterns": [{"kind": "literal", "value": "zq-never-in-any-answer"}],
        }))
        .unwrap();
        crate::seed_env_scoped_guardrail(&snap, ResourceEntry::new("g-held", row, 1));
    }

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

/// The upstream's text as the caller must receive it: the credential and the
/// infrastructure out.
fn scrubbed(text: &str) -> String {
    text.replace(SECRET, REDACTED)
        .replace(INFRA, INFRA_SCRUBBED)
}

fn assert_no_leak(body: &str) {
    for leak in LEAKS {
        assert!(!body.contains(leak), "{leak} in {body}");
    }
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
         resulted in 250000 tokens. Request signed with {SECRET} {INFRA}."
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
    assert_no_leak(&body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], scrubbed(&text), "{body}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "context_length_exceeded");
    assert_eq!(v["error"]["param"], "messages");
}

/// Claude Code compacts its conversation when the message says
/// "prompt is too long"; the Anthropic envelope carries the upstream's words.
#[tokio::test]
async fn messages_relays_a_native_400_in_the_anthropic_envelope() {
    let text = format!("prompt is too long: 250000 tokens > 200000 maximum ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
    let text = format!("Request exceeds the maximum allowed number of bytes. ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
    let text = format!("Previous response with id 'resp_up_1' not found. ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], scrubbed(&text), "{body}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "previous_response_not_found");
    assert_eq!(v["error"]["param"], "previous_response_id");
}

#[tokio::test]
async fn completions_relays_a_native_400() {
    let text = format!("Invalid value for 'max_tokens': 999999. ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], scrubbed(&text), "{body}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "invalid_value");
    assert_eq!(v["error"]["param"], "max_tokens");
}

/// An unprocessable request keeps its own status.
#[tokio::test]
async fn embeddings_relays_a_native_422() {
    let text = format!("Input is not a list of token ids or strings. ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
    let text = format!("This model's maximum context length is 8192 tokens. ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
    let text = format!("messages.0.content: Field required ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
        let text = format!("upstream says {status} for account acct-9 ({SECRET}) {INFRA}");
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
        assert_no_leak(&body);
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
    assert_no_leak(&body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["message"], text);
    assert_eq!(v["error"]["type"], "upstream_error");
    assert_eq!(v["error"]["param"], "temperature");
}

// ── streams: a native request error mid-stream ─────────────────────────────

#[tokio::test]
async fn a_chat_stream_ends_on_the_native_upstreams_request_error() {
    let text = format!("This model's maximum context length is 128000 tokens. ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
    let text =
        format!("messages.1.content.0.image.source: image exceeds 5 MB maximum ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
    let text = format!("Your input exceeds the context window of this model. ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
    let text = format!(
        "Invalid prompt: flagged as potentially violating our usage policy. ({SECRET}) {INFRA}"
    );
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
    assert_no_leak(&body);
    assert_no_leak(&body);
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
    let text = format!("This model's maximum context length is 4097 tokens. ({SECRET}) {INFRA}");
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
    assert_no_leak(&body);
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
    assert_no_leak(&body);
    let (_, error) = sse_payloads(&body)
        .into_iter()
        .find(|(_, v)| v["type"] == "error")
        .unwrap_or_else(|| panic!("no error event: {body}"));
    assert_eq!(error["error"]["code"], "upstream_unavailable", "{body}");
    assert!(!body.contains("us-east-1"), "{body}");
}

/// A native Responses stream that ends on the flat `error` event.
fn responses_ending_on_flat_error(text: &str) -> Vec<String> {
    vec![
        format!(
            "event: response.created\ndata: {}\n\n",
            json!({"type": "response.created", "sequence_number": 0,
                   "response": {"id": "resp_up_1", "object": "response", "created_at": 1,
                                "status": "in_progress", "model": "gpt-4o", "output": []}})
        ),
        format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            json!({"type": "response.output_text.delta", "sequence_number": 1,
                   "item_id": "msg_up_1", "output_index": 0, "content_index": 0, "delta": "Hel"})
        ),
        format!(
            "event: error\ndata: {}\n\n",
            json!({"type": "error", "sequence_number": 2, "code": "context_length_exceeded",
                   "message": text, "param": "input"})
        ),
    ]
}

/// The flat `error` event a native Responses stream ends on, relayed live and
/// released from a stream an output guardrail held back.
#[tokio::test]
async fn a_native_responses_stream_relays_the_upstreams_flat_error_event() {
    let text = format!("Your input exceeds the context window of this model. ({SECRET}) {INFRA}");
    for held in [false, true] {
        let up = sse_upstream("responses", &responses_ending_on_flat_error(&text)).await;
        let (status, body) = post(
            app_with(&up, Upstream::OpenAi, true, held),
            "/v1/responses",
            json!({"model": "m", "input": "hi", "stream": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "held {held}: {body}");
        assert_no_leak(&body);
        let (name, error) = sse_payloads(&body)
            .into_iter()
            .find(|(_, v)| v["type"] == "error")
            .unwrap_or_else(|| panic!("held {held}: no error event: {body}"));
        assert_eq!(name.as_deref(), Some("error"), "held {held}");
        assert_eq!(
            error,
            json!({"type": "error", "sequence_number": 2, "code": "context_length_exceeded",
                   "message": scrubbed(&text), "param": "input"}),
            "held {held}: {body}"
        );
    }
}

// ── the platform's own account is never relayed ────────────────────────────

/// Anthropic's words for a spend limit set on the account
/// (<https://platform.claude.com/docs/en/api/rate-limits>, "Setting your own
/// spend limit"), with the date its access resumes.
const SPEND_LIMIT: &str = "You have reached your specified workspace API usage limits. \
                           You will regain access on 2026-12-01 at 00:00 UTC.";
const CREDIT: &str = "Your credit balance is too low to access the Anthropic API. \
                      Please go to Plans & Billing to upgrade or purchase credits.";
const UNVERIFIED: &str = "Your organization must be verified to stream this model. Please go \
                          to: https://platform.openai.com/settings/organization/general and \
                          click on Verify Organization.";

/// The platform's account answered for: the model unavailable, retryable,
/// in the caller's envelope, with nothing of the upstream's text.
fn assert_account_state_classed(status: StatusCode, body: &str, anthropic: bool) {
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["error"]["code"], "upstream_unavailable", "{body}");
    if anthropic {
        assert_eq!(v["type"], "error", "{body}");
        assert_eq!(v["error"]["type"], "overloaded_error", "{body}");
    } else {
        assert_eq!(v["error"]["type"], "server_error", "{body}");
    }
    for word in [
        "2026-12-01",
        "usage limits",
        "credit",
        "verified",
        "hard limit",
    ] {
        assert!(!body.to_lowercase().contains(word), "{word} in {body}");
    }
}

/// An upstream's 400 or 402 that reports the platform's own account (its
/// spend limit, its credit, its organization's standing) is the model being
/// unavailable, on every family, never the tenant's request error: it
/// describes an account every tenant shares, and no reset date reaches one.
#[tokio::test]
async fn the_platforms_account_state_is_never_relayed() {
    let cases: Vec<(&str, u16, Value, Upstream, &str, Value)> = vec![
        (
            "messages",
            400,
            anthropic_error("invalid_request_error", SPEND_LIMIT),
            Upstream::Anthropic,
            "/v1/messages",
            messages_body(false),
        ),
        (
            "count_tokens",
            400,
            anthropic_error("invalid_request_error", CREDIT),
            Upstream::Anthropic,
            "/v1/messages/count_tokens",
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
        ),
        // Anthropic's own type for billing (<https://platform.claude.com/docs/en/api/errors>).
        (
            "messages",
            402,
            anthropic_error("billing_error", CREDIT),
            Upstream::Anthropic,
            "/v1/messages",
            messages_body(true),
        ),
        (
            "chat/completions",
            400,
            openai_error(
                UNVERIFIED,
                "invalid_request_error",
                Some("stream"),
                Some("unsupported_value"),
            ),
            Upstream::OpenAi,
            "/v1/chat/completions",
            chat_body(true),
        ),
        (
            "responses",
            400,
            openai_error(
                "Billing hard limit has been reached",
                "invalid_request_error",
                None,
                Some("billing_hard_limit_reached"),
            ),
            Upstream::OpenAi,
            "/v1/responses",
            json!({"model": "m", "input": "hi"}),
        ),
        (
            "embeddings",
            400,
            openai_error(
                "Billing hard limit has been reached",
                "invalid_request_error",
                None,
                Some("billing_hard_limit_reached"),
            ),
            Upstream::OpenAi,
            "/v1/embeddings",
            json!({"model": "m", "input": "hi"}),
        ),
    ];
    for (suffix, upstream_status, error, kind, uri, request) in cases {
        let up = json_upstream(suffix, upstream_status, error).await;
        let (status, body) = post(app(&up, kind, true), uri, request).await;
        assert_account_state_classed(status, &body, matches!(kind, Upstream::Anthropic));
    }
}

/// The same reported mid-stream ends the stream on the class's frame.
#[tokio::test]
async fn a_stream_ending_on_the_platforms_account_state_ends_on_the_class() {
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
                anthropic_error("invalid_request_error", SPEND_LIMIT)
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
    assert!(
        !body.contains("2026-12-01") && !body.contains("usage limits"),
        "{body}"
    );
}

/// A body that is no error envelope of the upstream's protocol (an
/// intermediary's HTML page) is in no words a client reads: it keeps the
/// class.
#[tokio::test]
async fn a_native_request_error_that_is_no_error_envelope_keeps_its_class() {
    let up = upstream(
        "chat/completions",
        413,
        "<html><head><title>413 Request Entity Too Large</title></head>\
         <body><center>awselb/2.0</center></body></html>",
        "text/html",
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
    assert_eq!(v["error"]["code"], "invalid_request", "{body}");
    assert!(
        !body.contains("<html") && !body.contains("awselb"),
        "{body}"
    );
}
