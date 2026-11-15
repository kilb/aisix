//! 首内容前终结的指标组件回归：真实代理路由后抓取真实管理 /metrics。
//! 本地 HTTP 上游替身只验证组件组合，不代表真实供应商 E2E。
//!
//! 上游帧合同：https://platform.claude.com/docs/en/build-with-claude/streaming
//! 缓存指标合同：tests/e2e/src/cases/upstream-protocol-cache-tokens-e2e.test.ts。

use std::sync::Arc;
use std::time::Duration;

use aisix_admin::{metrics_router, ConfigStore, InMemoryStore, ModelsStatusState};
use aisix_core::config::PrometheusConfig;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{
    AisixSnapshot, ApiKey, ConfigStatus, Model, ProviderKey, ProxyConfig, ResourceEntry, SourceKind,
};
use aisix_gateway::Hub;
use aisix_obs::{Metrics, UsageEvent, UsageSink};
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
const PROVIDER: &str = "11111111-1111-4111-8111-111111111111";
const MODEL: &str = "22222222-2222-4222-8222-222222222222";

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
            Self::Chat => json!({"model":"early-accounting","stream":true,"max_tokens":16,
                "messages":[{"role":"user","content":"Look up the requested information."}]}),
            Self::Responses => json!({"model":"early-accounting","stream":true,
                "max_output_tokens":16,"input":"Look up the requested information."}),
        }
    }
}

fn sse(overloaded: bool) -> String {
    let start = json!({"type":"message_start","message":{
        "id":"msg_early_metrics","type":"message","role":"assistant","content":[],
        "model":"claude-sonnet-4-5","stop_reason":null,"stop_sequence":null,
        "usage":{"input_tokens":37,"output_tokens":0,
            "cache_creation_input_tokens":11,"cache_read_input_tokens":23,
            "server_tool_use":{"web_search_requests":7,"web_fetch_requests":0}}
    }});
    let mut body = format!("event: message_start\ndata: {start}\n\n");
    if overloaded {
        body.push_str("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n");
    }
    body
}

fn app(
    base: &str,
    configured_timeout: bool,
) -> (axum::Router, Arc<Metrics>, mpsc::Receiver<UsageEvent>) {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"component-upstream","provider":"anthropic","adapter":"anthropic",
        "api_base":format!("{base}/v1"),"secret":"component-only-key"
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"early-accounting","provider":"anthropic","model_name":"claude-sonnet-4-5",
        "provider_key_id":PROVIDER,"retries":0,"timeout":0,
        "stream_timeout":if configured_timeout { 10_000 } else { 0 }
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("caller"),"allowed_models":["early-accounting"]
    }))
    .unwrap();
    snapshot
        .apikeys
        .insert(ResourceEntry::new("caller-id", key, 1));
    let hub = Arc::new(Hub::new());
    hub.register_specialized("anthropic", Arc::new(AnthropicBridge::new()));
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
    // 每个场景重新构造 ProxyState；其构造器创建独立 Metrics 注册器。
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config).without_cache();
    let metrics = state.metrics.clone();
    let (sender, events) = mpsc::channel(16);
    let state = state.with_usage_sink(UsageSink::new(sender).with_metrics((*metrics).clone()));
    (build_router(state), metrics, events)
}

async fn scrape(metrics: Arc<Metrics>) -> String {
    let response = metrics_router(
        metrics,
        ConfigStatus::new(SourceKind::Etcd),
        &PrometheusConfig {
            enabled: true,
            path: "/metrics".into(),
            addr: "127.0.0.1:0".into(),
        },
        ModelsStatusState {
            store: InMemoryStore::new() as Arc<dyn ConfigStore>,
            runtime_status_tracker: None,
        },
    )
    .oneshot(
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(
        to_bytes(response.into_body(), 8 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn samples<'a>(scrape: &'a str, family: &str, endpoint: &str) -> Vec<&'a str> {
    let prefix = format!("{family}{{");
    let endpoint = format!("endpoint=\"{endpoint}\"");
    scrape
        .lines()
        .filter(|line| {
            if !line.starts_with(&prefix) {
                return false;
            }
            let labels = line.split_once('{').unwrap().1.split_once('}').unwrap().0;
            [
                endpoint.as_str(),
                "model=\"early-accounting\"",
                "provider=\"anthropic\"",
            ]
            .iter()
            .all(|expected| labels.split(',').any(|label| label == *expected))
        })
        .collect()
}

fn exact_counter(scrape: &str, family: &str, endpoint: &str, expected: u64) {
    let rows = samples(scrape, family, endpoint);
    assert_eq!(rows.len(), 1, "missing or duplicated {family}: {scrape}");
    let count: f64 = rows[0].rsplit_once(' ').unwrap().1.parse().unwrap();
    assert_eq!(count, expected as f64, "{}", rows[0]);
}

async fn exercise(client: Client, overloaded: bool, configured_timeout: bool) {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse(overloaded), "text/event-stream"))
        .mount(&upstream)
        .await;
    let (proxy, metrics, mut events) = app(&upstream.uri(), configured_timeout);
    let before = scrape(metrics.clone()).await;
    assert!(samples(&before, "aisix_llm_input_tokens_total", client.path()).is_empty());
    let request = Request::builder()
        .method("POST")
        .uri(client.path())
        .header("authorization", "Bearer caller")
        .header("content-type", "application/json")
        .body(Body::from(client.request().to_string()))
        .unwrap();
    let response = tokio::time::timeout(DEADLINE, proxy.oneshot(request))
        .await
        .expect("proxy response stalled")
        .unwrap();
    assert_eq!(
        response.status(),
        if configured_timeout {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::OK
        }
    );
    tokio::time::timeout(DEADLINE, to_bytes(response.into_body(), 65_536))
        .await
        .expect("proxy body stalled")
        .unwrap();
    // 消费完响应并等 sink 关闭，所有终态副作用都已完成；不能靠延时猜测。
    let rows = tokio::time::timeout(DEADLINE, async {
        let mut rows = Vec::new();
        while let Some(event) = events.recv().await {
            rows.push(event);
        }
        rows
    })
    .await
    .expect("usage sink did not close");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].upstream_dispatched, Some(true));
    assert!(rows[0].is_final_attempt);
    assert_eq!(rows[0].status_code, 502);
    assert_eq!(
        (
            rows[0].prompt_tokens,
            rows[0].completion_tokens,
            rows[0].total_tokens
        ),
        (37, 0, 0)
    );
    let event = serde_json::to_value(&rows[0]).unwrap();
    // 没有既存搜索次数 Prometheus 系列；搜索计数仍在真实终态事件上验证。
    assert_eq!(event.get("web_search_requests"), Some(&json!(7)), "{event}");
    assert_eq!(event.get("cache_creation_tokens"), Some(&json!(11)));
    assert_eq!(event.get("cache_read_tokens"), Some(&json!(23)));
    assert_eq!(event.get("cache_usage_reported"), Some(&json!(true)));
    assert_eq!(event.get("cost_usd"), Some(&json!(0.0)));
    assert!(
        event
            .get("upstream_ttft_ms")
            .and_then(Value::as_u64)
            .is_some_and(|ms| ms > 0),
        "已解析 message_start，必须保留上游首帧测量：{event}"
    );
    assert!(event.get("first_token_latency_ms").is_none(), "{event}");
    let after = scrape(metrics).await;
    for (family, expected) in [
        ("aisix_llm_input_tokens_total", 37),
        ("aisix_llm_cache_creation_input_tokens_total", 11),
        ("aisix_llm_cache_read_input_tokens_total", 23),
        ("aisix_llm_total_tokens_total", 71),
        ("aisix_llm_requests_total", 1),
    ] {
        exact_counter(&after, family, client.path(), expected);
    }
    // 零值 token 系列按既有合同稀疏导出。
    for family in [
        "aisix_llm_output_tokens_total",
        "aisix_llm_cached_input_tokens_total",
    ] {
        assert!(
            samples(&after, family, client.path()).is_empty(),
            "unexpected {family}: {after}"
        );
    }
    // message_start 是已解析首帧，独立于未产生的首内容；两套 TTFT 仅各记一次上游。
    for family in [
        "aisix_request_ttft_seconds_count",
        "aisix_llm_time_to_first_token_seconds_count",
    ] {
        exact_counter(&after, family, client.path(), 1);
        let row = samples(&after, family, client.path())[0];
        let labels = row.split_once('{').unwrap().1.split_once('}').unwrap().0;
        assert!(
            labels.split(',').any(|label| label == "side=\"upstream\""),
            "不能把未交付的内容计为 downstream TTFT：{row}"
        );
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

macro_rules! cases {
    ($module:ident, $client:expr) => {
        mod $module {
            use super::*;

            #[tokio::test]
            async fn overloaded_with_read_timeout() {
                exercise($client, true, true).await;
            }
            #[tokio::test]
            async fn overloaded_without_read_timeout() {
                exercise($client, true, false).await;
            }
            #[tokio::test]
            async fn eof_with_read_timeout() {
                exercise($client, false, true).await;
            }
            #[tokio::test]
            async fn eof_without_read_timeout() {
                exercise($client, false, false).await;
            }
        }
    };
}

cases!(chat, Client::Chat);
cases!(responses, Client::Responses);
