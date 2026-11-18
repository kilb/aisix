//! Provider-group 拒绝的路由组件回归：真实 Router、日志、用量事件与 TCP 连接失败。
//! 不调用供应商，也不把组件验证称作供应商端到端测试。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, platform::PlatformSettings, ProxyState};
use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tower::ServiceExt;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DEADLINE: Duration = Duration::from_secs(10);
const PROVIDER: &str = "11111111-1111-4111-8111-111111111111";
const TARGET: &str = "22222222-2222-4222-8222-222222222222";
const CALLER: &str = "33333333-3333-4333-8333-333333333333";

#[derive(Clone, Copy, Debug)]
struct Case {
    path: &'static str,
    provider: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Direct,
    DisabledDirect,
    Routing,
    FixedHash,
    FixedHashMissingProvider,
}

const CASES: &[Case] = &[
    Case {
        path: "/v1/chat/completions",
        provider: "openai",
    },
    Case {
        path: "/v1/chat/completions",
        provider: "anthropic",
    },
    Case {
        path: "/v1/messages",
        provider: "anthropic",
    },
    Case {
        path: "/v1/messages",
        provider: "openai",
    },
    Case {
        path: "/v1/messages/count_tokens",
        provider: "anthropic",
    },
    Case {
        path: "/v1/responses",
        provider: "openai",
    },
    Case {
        path: "/v1/responses",
        provider: "component-compatible",
    },
    Case {
        path: "/v1/completions",
        provider: "openai",
    },
    Case {
        path: "/v1/embeddings",
        provider: "openai",
    },
    Case {
        path: "/v1/rerank",
        provider: "openai",
    },
    Case {
        path: "/v1/images/generations",
        provider: "openai",
    },
    Case {
        path: "/v1/audio/speech",
        provider: "openai",
    },
    Case {
        path: "/v1/moderations",
        provider: "openai",
    },
    Case {
        path: "/v1/content_provenance_checks",
        provider: "openai",
    },
];

impl Case {
    fn body(self, stream: bool) -> Value {
        match self.path {
            "/v1/responses" => json!({"model":"entry","input":"hello","stream":stream}),
            "/v1/completions" => json!({"model":"entry","prompt":"hello","stream":stream}),
            "/v1/embeddings" | "/v1/moderations" => {
                json!({"model":"entry","input":"hello"})
            }
            "/v1/rerank" => json!({"model":"entry","query":"hello","documents":["hello"]}),
            "/v1/images/generations" => json!({"model":"entry","prompt":"a tree"}),
            "/v1/audio/speech" => json!({"model":"entry","input":"hello","voice":"alloy"}),
            _ => {
                json!({"model":"entry","messages":[{"role":"user","content":"hello"}],"max_tokens":8,"stream":stream})
            }
        }
    }

    fn anthropic(self) -> bool {
        self.path.starts_with("/v1/messages")
    }

    fn routing_shapes(self) -> &'static [Shape] {
        match self.path {
            "/v1/moderations" | "/v1/content_provenance_checks" => &[Shape::Direct],
            _ => &[Shape::Direct, Shape::Routing],
        }
    }
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("log lock poisoned"))?
            .write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for LogWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self {
        self.clone()
    }
}

fn app(
    base: &str,
    case: Case,
    grouped: bool,
    shape: Shape,
    platform: bool,
) -> TestResult<(
    axum::Router,
    mpsc::Receiver<UsageEvent>,
    Arc<aisix_obs::Metrics>,
)> {
    let snapshot = AisixSnapshot::new();
    let adapter = if case.provider == "anthropic" {
        "anthropic"
    } else {
        "openai"
    };
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"private-provider","api_key":"component-upstream-secret",
        "provider":case.provider,"adapter":adapter,"api_base":base,
        "groups":["private-provider-group"], "disabled":shape == Shape::DisabledDirect
    }))?;
    if shape != Shape::FixedHashMissingProvider {
        snapshot
            .provider_keys
            .insert(ResourceEntry::new(PROVIDER, provider, 1));
    }
    let routing = !matches!(shape, Shape::Direct | Shape::DisabledDirect);
    let model: Model = serde_json::from_value(json!({
        "display_name":if routing { "private-target" } else { "entry" },
        "provider":case.provider,"model_name":"private-upstream-model",
        "provider_key_id":PROVIDER,"retries":0
    }))?;
    snapshot.models.insert(ResourceEntry::new(TARGET, model, 1));
    if routing {
        let mut config = json!({"strategy":"failover",
            "targets":[{"model_id":TARGET}],"when_all_unavailable":"fail"});
        if matches!(shape, Shape::FixedHash | Shape::FixedHashMissingProvider) {
            config["strategy"] = json!("consistent_hash");
            config["hash_failover"] = json!(false);
            config["hash_on"] = json!([{"type":"header","name":"session-id"}]);
        }
        let model: Model =
            serde_json::from_value(json!({"display_name":"entry","routing":config}))?;
        snapshot
            .models
            .insert(ResourceEntry::new("router-id", model, 1));
    }
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("component-caller-secret"),"allowed_models":["entry"],
        "allowed_provider_groups":[if grouped { "private-caller-group" } else { "private-provider-group" }]
    }))?;
    snapshot.apikeys.insert(ResourceEntry::new(CALLER, key, 1));
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    hub.register_specialized(
        "anthropic",
        Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
    );
    hub.register_family(
        aisix_core::Adapter::Openai,
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"}))?;
    let (tx, events) = mpsc::channel(16);
    let mut state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_usage_sink(UsageSink::new(tx));
    if platform {
        state = state.with_platform(PlatformSettings::from_secret("component-platform-secret"));
    }
    let metrics = state.metrics.clone();
    Ok((build_router(state), events, metrics))
}

struct Outcome {
    status: StatusCode,
    headers: HeaderMap,
    bytes: Vec<u8>,
    event: UsageEvent,
    logs: String,
    metrics: String,
}

async fn call(
    base: &str,
    case: Case,
    grouped: bool,
    shape: Shape,
    platform: bool,
    stream: bool,
) -> TestResult<Outcome> {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(LogWriter(logs.clone()))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let configured_base = if grouped {
        base.replacen("://", "://component-url-user:component-url-password@", 1)
    } else {
        base.to_owned()
    };
    let (router, mut events, metrics) = app(&configured_base, case, grouped, shape, platform)?;
    let (content_type, body) = if case.path == "/v1/content_provenance_checks" {
        (
            "multipart/form-data; boundary=component-file",
            b"--component-file\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\nPNG\x00\xffcomponent-file\r\n--component-file--\r\n".to_vec(),
        )
    } else {
        (
            "application/json",
            case.body(stream).to_string().into_bytes(),
        )
    };
    let request = Request::builder()
        .method("POST")
        .uri(case.path)
        .header("authorization", "Bearer component-caller-secret")
        .header("content-type", content_type)
        .header("anthropic-version", "2023-06-01")
        .header("x-aisix-model", "entry")
        .header("session-id", "component-session")
        .extension(ConnectInfo("127.0.0.1:41000".parse::<SocketAddr>()?))
        .body(Body::from(body))?;
    let response = tokio::time::timeout(DEADLINE, router.oneshot(request)).await??;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = tokio::time::timeout(DEADLINE, to_bytes(response.into_body(), 1048576))
        .await??
        .to_vec();
    let event = tokio::time::timeout(DEADLINE, events.recv())
        .await?
        .ok_or("missing usage event")?;
    assert!(
        events.try_recv().is_err(),
        "{case:?}: duplicate usage event"
    );
    let logs = String::from_utf8(logs.lock().map_err(|_| "log lock poisoned")?.clone())?;
    Ok(Outcome {
        status,
        headers,
        bytes,
        event,
        logs,
        metrics: metrics.render(),
    })
}

fn assert_operator(outcome: &Outcome, class: &str, dispatched: bool) {
    assert_eq!(outcome.event.error_class, class);
    assert_eq!(outcome.event.upstream_dispatched, Some(dispatched));
    let access: Vec<_> = outcome
        .logs
        .lines()
        .filter(|line| line.contains("proxy request completed"))
        .collect();
    assert_eq!(access.len(), 1, "{}", outcome.logs);
    assert!(
        access[0].contains(&format!("error_class=\"{class}\"")),
        "{}",
        access[0]
    );
    if class == "provider_groups_unavailable" {
        let warning = outcome
            .logs
            .lines()
            .find(|line| line.contains("WARN") && line.contains("provider_groups_unavailable"))
            .unwrap_or_else(|| panic!("missing group refusal warning: {}", outcome.logs));
        for private in [
            "entry",
            "private-target",
            "private-upstream-model",
            "private-provider",
            "private-caller-group",
            "private-provider-group",
            PROVIDER,
            TARGET,
            CALLER,
            "component-url-user",
            "component-url-password",
            "component-upstream-secret",
        ] {
            assert!(
                !warning.contains(private),
                "private field {private} leaked: {warning}"
            );
        }
    }
    for secret in [
        "component-caller-secret",
        "component-upstream-secret",
        "component-platform-secret",
        "component-url-user",
        "component-url-password",
    ] {
        assert!(!outcome.logs.contains(secret), "credential leaked to logs");
    }
    assert!(
        outcome
            .metrics
            .lines()
            .any(|line| line.starts_with("aisix_proxy_requests_total{")
                && line.contains(if dispatched {
                    "error_source=\"upstream\""
                } else {
                    "error_source=\"aisix\""
                })
                && line.ends_with(" 1")),
        "{}",
        outcome.metrics
    );
    assert!(!outcome.metrics.contains("private-caller-group"));
    assert!(!outcome.metrics.contains("private-provider-group"));
}

fn assert_public(outcome: &Outcome, case: Case, platform: bool, routing: bool) -> TestResult {
    assert_eq!(
        outcome.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{case:?}: {}",
        String::from_utf8_lossy(&outcome.bytes)
    );
    assert_eq!(
        outcome
            .headers
            .get("retry-after")
            .map(|value| value.to_str())
            .transpose()?,
        if platform && routing {
            Some("30")
        } else {
            None
        }
    );
    let message = if platform {
        "The model is temporarily unavailable. Retry later."
    } else {
        "all routing candidates are unavailable"
    };
    let kind = if case.anthropic() {
        "overloaded_error"
    } else if platform {
        "server_error"
    } else {
        "all_candidates_unavailable"
    };
    let mut expected = json!({"error":{"message":message,"type":kind}});
    if platform {
        expected["error"]["code"] = json!("upstream_unavailable");
    }
    if case.anthropic() {
        expected["type"] = json!("error");
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&outcome.bytes)?,
        expected,
        "{case:?}"
    );
    Ok(())
}

#[tokio::test]
async fn provider_group_refusals_have_operator_reason_on_every_handler_family() -> TestResult {
    // 端口真实关闭；组拒绝必须在任何上游连接之前结束。
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    drop(listener);
    for &case in CASES {
        for platform in [false, true] {
            for &shape in case.routing_shapes() {
                for stream in [false, true] {
                    let outcome = call(&base, case, true, shape, platform, stream).await?;
                    assert_public(&outcome, case, platform, shape == Shape::Routing)?;
                    assert_operator(&outcome, "provider_groups_unavailable", false);
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn provider_group_refusal_and_upstream_down_share_platform_caller_contract() -> TestResult {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    drop(listener);
    for &case in CASES {
        for stream in [false, true] {
            let denied = call(&base, case, true, Shape::Direct, true, stream).await?;
            let down = call(&base, case, false, Shape::Direct, true, stream).await?;
            assert_public(&denied, case, true, false)?;
            assert_eq!(denied.status, down.status, "{case:?}");
            assert_eq!(
                denied.headers.get("retry-after"),
                down.headers.get("retry-after"),
                "{case:?}"
            );
            assert_eq!(denied.bytes, down.bytes, "{case:?}");
            assert_operator(&denied, "provider_groups_unavailable", false);
            assert_operator(&down, "transport", true);
        }
    }
    Ok(())
}

#[tokio::test]
async fn fixed_hash_group_refusal_preserves_its_wire_error_and_missing_provider_class() -> TestResult
{
    let case = CASES[0];
    for platform in [false, true] {
        let denied = call(
            "http://127.0.0.1:0/v1",
            case,
            true,
            Shape::FixedHash,
            platform,
            false,
        )
        .await?;
        let missing = call(
            "http://127.0.0.1:0/v1",
            case,
            true,
            Shape::FixedHashMissingProvider,
            platform,
            false,
        )
        .await?;
        assert_eq!(denied.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(denied.status, missing.status);
        assert_eq!(denied.bytes, missing.bytes);
        assert_eq!(denied.headers.get("retry-after"), None);
        assert_eq!(missing.headers.get("retry-after"), None);
        if platform {
            assert_public(&denied, case, true, false)?;
        } else {
            assert_eq!(denied.bytes, br#"{"error":{"message":"no bridge registered for provider","type":"provider_unavailable"}}"#);
        }
        assert_operator(&denied, "provider_groups_unavailable", false);
        assert_operator(&missing, "provider_unavailable", false);
        assert!(!missing.logs.contains("provider_groups_unavailable"));
    }
    Ok(())
}

#[tokio::test]
async fn account_disabled_stops_physical_dispatch_on_all_supported_families() -> TestResult {
    // 上游端口真实可达；提前拒绝时不得留下任何待接受的 TCP 连接。
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    for &case in CASES {
        for stream in [false, true] {
            let outcome = call(&base, case, false, Shape::DisabledDirect, true, stream).await?;
            assert_eq!(outcome.status, StatusCode::SERVICE_UNAVAILABLE, "{case:?}");
            assert_eq!(outcome.event.upstream_dispatched, Some(false), "{case:?}");
            assert!(
                matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
            );
        }
    }
    Ok(())
}
