//! 组件回归：真实代理路由和 Anthropic 桥接器读取本地 HTTP 上游。
//! message_start 已报告的 37 + 11 + 23 + 0 = 71 个令牌在提前失败时也必须计入 TPM。
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{
    AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, RateLimit, ResourceEntry,
};
use aisix_gateway::Hub;
use aisix_provider_anthropic::AnthropicBridge;
use aisix_proxy::{build_router, ProxyState};
use aisix_ratelimit::{Clock, Limiter, TestClock};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error>>;

const MODEL_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER_ID: &str = "22222222-2222-4222-8222-222222222222";
const KEY_ID: &str = "33333333-3333-4333-8333-333333333333";
const MODEL: &str = "early-tpm-model";
const CALLER: &str = "early-tpm-fixture-caller";
const CLOCK_SECS: u64 = 120;
const TPM: u64 = 70;
const REPORTED_TOKENS: u64 = 71;
const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
enum Endpoint {
    Chat,
    Responses,
}

impl Endpoint {
    fn request(self) -> Result<Request<Body>, axum::http::Error> {
        let (uri, body) = match self {
            Self::Chat => (
                "/v1/chat/completions",
                json!({
                    "model": MODEL,
                    "messages": [{"role": "user", "content": "Hello"}],
                    "max_tokens": 32,
                    "stream": true
                }),
            ),
            Self::Responses => (
                "/v1/responses",
                json!({
                    "model": MODEL,
                    "input": "Hello",
                    "max_output_tokens": 32,
                    "stream": true
                }),
            ),
        };
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {CALLER}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
    }
}

#[derive(Clone, Copy, Debug)]
enum Ending {
    Overloaded,
    Eof,
}

fn upstream_sse(ending: Ending) -> String {
    let start = json!({
        "type": "message_start",
        "message": {
            "id": "msg_early_tpm_fixture",
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": "claude-sonnet-4-5-20250929",
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": 37,
                "output_tokens": 0,
                "cache_creation_input_tokens": 11,
                "cache_read_input_tokens": 23,
                "server_tool_use": {"web_search_requests": 7, "web_fetch_requests": 0}
            }
        }
    });
    let mut sse = format!("event: message_start\ndata: {start}\n\n");
    if matches!(ending, Ending::Overloaded) {
        let error = json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        sse.push_str(&format!("event: error\ndata: {error}\n\n"));
    }
    sse
}

async fn early_failure_spends_reported_tpm(
    endpoint: Endpoint,
    ending: Ending,
    configured_timeout: bool,
) -> TestResult {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse(ending)),
        )
        .mount(&upstream)
        .await;

    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name": "early-tpm-provider",
        "provider": "anthropic",
        "adapter": "anthropic",
        "secret": "early-tpm-fixture-upstream-key",
        "api_base": upstream.uri()
    }))?;
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER_ID, provider, 1));
    let mut model = json!({
        "display_name": MODEL,
        "provider": "anthropic",
        "model_name": "claude-sonnet-4-5-20250929",
        "provider_key_id": PROVIDER_ID,
        "retries": 0,
        "rate_limit": {"tpm": TPM}
    });
    if configured_timeout {
        model["stream_timeout"] = json!(10_000);
    }
    let model: Model = serde_json::from_value(model)?;
    snapshot
        .models
        .insert(ResourceEntry::new(MODEL_ID, model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash": ApiKey::hash_bearer(CALLER),
        "allowed_models": [MODEL]
    }))?;
    snapshot.apikeys.insert(ResourceEntry::new(KEY_ID, key, 1));
    let config: ProxyConfig = serde_json::from_value(json!({
        "addr": "127.0.0.1:0",
        "request_body_limit_bytes": 1024 * 1024
    }))?;
    let hub = Arc::new(Hub::new());
    hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));

    // 集成测试不会启用代理库的 cfg(test)，必须显式固定 Unix 分钟窗口。
    // Tokio 暂停时钟不影响限流器使用的 SystemTime；这里不推进测试时钟。
    let clock = TestClock::new(CLOCK_SECS);
    let limiter = Arc::new(Limiter::local_with_clock(clock.clone()));
    let state = ProxyState::with_limiter(
        SnapshotHandle::new(snapshot),
        hub,
        Arc::clone(&limiter),
        &config,
    )
    .without_cache()
    .with_default_retries(0);
    let app = build_router(state);

    let first = tokio::time::timeout(DEADLINE, app.clone().oneshot(endpoint.request()?)).await??;
    assert_eq!(
        first.status(),
        if configured_timeout {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::OK
        },
        "{endpoint:?}/{ending:?}/timeout={configured_timeout}"
    );
    // 完整排空首个失败流，随后只发一次复测，避免多次调用累积令牌造成假绿。
    let _first_body =
        tokio::time::timeout(DEADLINE, to_bytes(first.into_body(), 1024 * 1024)).await??;

    let second = tokio::time::timeout(DEADLINE, app.oneshot(endpoint.request()?)).await??;
    let second_status = second.status();
    let _second_body =
        tokio::time::timeout(DEADLINE, to_bytes(second.into_body(), 1024 * 1024)).await??;
    let requests = upstream
        .received_requests()
        .await
        .ok_or("wiremock request recording is unavailable")?;
    let limits = RateLimit {
        tpm: Some(TPM),
        ..RateLimit::default()
    };
    let used = limiter
        .peek(&format!("model:{MODEL}"), &limits)
        .await
        .ok_or("model TPM bucket is missing")?
        .tpm_used;

    // 搜索次数不是令牌；任何输入或缓存桶丢失，71 都会落到 70 以下。
    assert_eq!(
        (second_status, requests.len(), used),
        (StatusCode::TOO_MANY_REQUESTS, 1, REPORTED_TOKENS),
        "{endpoint:?}/{ending:?}/timeout={configured_timeout}: 提前失败已消耗 71 个令牌，复测必须在上游前被拒绝"
    );
    assert_eq!(clock.unix_secs(), CLOCK_SECS);
    Ok(())
}

#[tokio::test]
async fn chat_message_start_then_overload_spends_reported_tpm() -> TestResult {
    early_failure_spends_reported_tpm(Endpoint::Chat, Ending::Overloaded, false).await
}

#[tokio::test]
async fn chat_message_start_then_eof_spends_reported_tpm() -> TestResult {
    early_failure_spends_reported_tpm(Endpoint::Chat, Ending::Eof, false).await
}

#[tokio::test]
async fn responses_message_start_then_overload_spends_reported_tpm() -> TestResult {
    early_failure_spends_reported_tpm(Endpoint::Responses, Ending::Overloaded, false).await
}

#[tokio::test]
async fn responses_message_start_then_eof_spends_reported_tpm() -> TestResult {
    early_failure_spends_reported_tpm(Endpoint::Responses, Ending::Eof, false).await
}

#[tokio::test]
async fn chat_message_start_then_overload_with_read_timeout_spends_reported_tpm() -> TestResult {
    early_failure_spends_reported_tpm(Endpoint::Chat, Ending::Overloaded, true).await
}

#[tokio::test]
async fn chat_message_start_then_eof_with_read_timeout_spends_reported_tpm() -> TestResult {
    early_failure_spends_reported_tpm(Endpoint::Chat, Ending::Eof, true).await
}

#[tokio::test]
async fn responses_message_start_then_overload_with_read_timeout_spends_reported_tpm() -> TestResult
{
    early_failure_spends_reported_tpm(Endpoint::Responses, Ending::Overloaded, true).await
}

#[tokio::test]
async fn responses_message_start_then_eof_with_read_timeout_spends_reported_tpm() -> TestResult {
    early_failure_spends_reported_tpm(Endpoint::Responses, Ending::Eof, true).await
}
