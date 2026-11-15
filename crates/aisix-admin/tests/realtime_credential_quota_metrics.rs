//! 现行 GA 三种 mint 消费真实 key 窗口，并经正式 HTTP 响应与 /metrics 同时汇报。
//! 使用真实受保护 SQLite authority、正式路由和 loopback 供应商 HTTP 组件；不代表真实供应商 E2E。
//! 请求沿 SDK 3dae6090 的合法槽；窗口标签沿公开 metric catalog，不额外发明 protocol 标签。

use std::error::Error;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aisix_admin::{metrics_router, ConfigStore, InMemoryStore, ModelsStatusState};
use aisix_core::config::PrometheusConfig;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{
    AisixSnapshot, ApiKey, ConfigStatus, Model, ProviderKey, ProxyConfig, ResourceEntry, SourceKind,
};
use aisix_gateway::Hub;
use aisix_proxy::native_api::standalone_store::StandaloneNativeStore;
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error + Send + Sync>>;
const PROVIDER: &str = "22222222-2222-4222-8222-222222229953";
const PROJECT: &str = "proj_credential_window_component";

#[derive(Clone, Copy)]
enum Purpose {
    Realtime,
    Transcription,
    Translation,
}

impl Purpose {
    fn name(self) -> &'static str {
        match self {
            Self::Realtime => "realtime",
            Self::Transcription => "transcription",
            Self::Translation => "translation",
        }
    }

    fn key_id(self) -> &'static str {
        match self {
            Self::Realtime => "33333333-3333-4333-8333-333333339951",
            Self::Transcription => "33333333-3333-4333-8333-333333339952",
            Self::Translation => "33333333-3333-4333-8333-333333339953",
        }
    }

    fn model_id(self) -> &'static str {
        match self {
            Self::Realtime => "11111111-1111-4111-8111-111111119951",
            Self::Transcription => "11111111-1111-4111-8111-111111119952",
            Self::Translation => "11111111-1111-4111-8111-111111119953",
        }
    }

    fn alias(self) -> &'static str {
        match self {
            Self::Realtime => "quota-public-realtime-A",
            Self::Transcription => "quota-public-transcription-B",
            Self::Translation => "quota-public-translation-T",
        }
    }

    fn upstream(self) -> &'static str {
        match self {
            Self::Realtime => "gpt-realtime-2.1",
            Self::Transcription => "gpt-live-transcribe",
            Self::Translation => "gpt-realtime-translate",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Realtime | Self::Transcription => "/v1/realtime/client_secrets",
            Self::Translation => "/v1/realtime/translations/client_secrets",
        }
    }

    fn ttl(self) -> i64 {
        if matches!(self, Self::Translation) {
            20
        } else {
            60
        }
    }

    fn request(self, model: &str) -> Value {
        let session = match self {
            Self::Realtime => json!({
                "type":"realtime","model":model,"instructions":"quota component defaults",
                "max_output_tokens":64,"output_modalities":["text"]
            }),
            Self::Transcription => json!({
                "type":"transcription","audio":{"input":{
                    "format":{"type":"audio/pcm","rate":24000},
                    "transcription":{"model":model},"turn_detection":null
                }}
            }),
            Self::Translation => json!({"model":model,"audio":{"output":{"language":"es"}}}),
        };
        json!({"expires_after":{"anchor":"created_at","seconds":self.ttl()},"session":session})
    }
}

struct ProtectedDirectory(PathBuf);

impl ProtectedDirectory {
    fn new(purpose: Purpose) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "aisix-credential-window-{}-{}-{nonce}",
            std::process::id(),
            purpose.name()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        assert_eq!(
            std::fs::metadata(&directory)?.permissions().mode() & 0o777,
            0o700
        );
        Ok(Self(directory))
    }
}

impl Drop for ProtectedDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_gauge(scrape: &str, family: &str, purpose: Purpose, expected: u64) {
    let prefix = format!("{family}{{");
    let lines: Vec<_> = scrape
        .lines()
        .filter(|line| line.starts_with(&prefix))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one actual caller/model window series must exist"
    );
    let line = lines[0];
    assert!(
        line.contains(&format!("api_key_id=\"{}\"", purpose.key_id()))
            && line.contains(&format!("model=\"{}\"", purpose.alias())),
        "window labels must identify the actual creator key and resolved public alias"
    );
    let actual = line
        .rsplit_once(' ')
        .and_then(|(_, value)| value.parse::<u64>().ok());
    assert_eq!(
        actual,
        Some(expected),
        "the actual scrape must contain an explicit exact quantity"
    );
    assert!(
        !line.contains(purpose.upstream()),
        "the window must not be labelled by the upstream name"
    );
}

async fn successful_mints_report_their_actual_window(purpose: Purpose) -> TestResult {
    let supplier = MockServer::start().await;
    let directory = ProtectedDirectory::new(purpose)?;
    let database = directory.0.join("native.sqlite3");
    drop(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&database)?,
    );
    assert_eq!(
        std::fs::metadata(&database)?.permissions().mode() & 0o777,
        0o600
    );
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let caller = format!("sk_component_caller_{nonce}");
    let provider_key = format!("sk_component_supplier_{nonce}");
    let provider_secret = format!("ek_component_supplier_{nonce}");
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"credential-window-original-account","provider":"openai","adapter":"openai",
        "api_base":supplier.uri(),"api_key":provider_key,
        "request":{"default_headers":{"OpenAI-Project":PROJECT}}
    }))?;
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":purpose.alias(),"provider":"openai","model_name":purpose.upstream(),
        "provider_key_id":PROVIDER
    }))?;
    snapshot
        .models
        .insert(ResourceEntry::new(purpose.model_id(), model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer(&caller),"allowed_model_ids":[purpose.model_id()],
        "allowed_endpoints":["realtime"],"rate_limit":{"rpm":7,"tpm":70}
    }))?;
    snapshot
        .apikeys
        .insert(ResourceEntry::new(purpose.key_id(), key, 1));
    let snapshot = SnapshotHandle::new(snapshot);
    let authority = Arc::new(
        StandaloneNativeStore::open(&database, [83; 32], snapshot.clone())
            .await
            .map_err(|_| std::io::Error::other("the real protected SQLite authority must open"))?,
    );
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"}))?;
    let state = ProxyState::new(snapshot, hub, &config)
        .without_cache()
        .with_native_objects(authority.clone());
    let metrics = metrics_router(
        state.metrics.clone(),
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
    );
    let app = build_router(state);

    // 只驱动真实 mint，绝不直接 emit/peek 来预填指标；两次管理请求没有供应商推理 usage。
    // 避开真实 UTC 分钟尾部，仍用真实时钟；若运行跨窗则明确报 setup 失败，不改数量期待。
    let millis = Utc::now().timestamp_millis().rem_euclid(60_000);
    if millis > 57_000 {
        tokio::time::sleep(Duration::from_millis((60_020 - millis) as u64)).await;
    }
    let minute = Utc::now().timestamp().div_euclid(60);
    for (index, remaining) in [(1, 6_u64), (2, 5_u64)] {
        let mut session = purpose.request(purpose.upstream())["session"].clone();
        session["id"] = json!(format!("sess_window_{}_{index}", purpose.name()));
        session["type"] = json!(purpose.name());
        session["expires_at"] = json!(Utc::now().timestamp() + 3600);
        if !matches!(purpose, Purpose::Translation) {
            session["object"] = json!(if matches!(purpose, Purpose::Transcription) {
                "realtime.transcription_session"
            } else {
                "realtime.session"
            });
        }
        let response_body = json!({
            "value":provider_secret,"expires_at":Utc::now().timestamp()+purpose.ttl(),"session":session
        });
        // 每轮只更新受控 HTTP 回复；历史永久累积，不清除请求或重置次数。
        Mock::given(method("POST"))
            .and(path(purpose.path()))
            .and(header("authorization", format!("Bearer {provider_key}")))
            .and(header("OpenAI-Project", PROJECT))
            .and(body_json(purpose.request(purpose.upstream())))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .up_to_n_times(1)
            .mount(&supplier)
            .await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(purpose.path())
                    .header("authorization", format!("Bearer {caller}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(
                        &purpose.request(purpose.alias()),
                    )?))?,
            )
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a legitimate GA mint must reach the real authority/provider"
        );
        assert_eq!(
            Utc::now().timestamp().div_euclid(60),
            minute,
            "setup invalid: actual mint pair crossed a real quota minute"
        );
        let headers = response.headers();
        for (name, expected) in [
            ("x-ratelimit-limit-requests", 7),
            ("x-ratelimit-remaining-requests", remaining),
            ("x-ratelimit-limit-tokens", 70),
            ("x-ratelimit-remaining-tokens", 70),
        ] {
            let actual = headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            assert_eq!(
                actual,
                Some(expected),
                "successful mint must publish its own exact {name}"
            );
        }
        for name in ["x-ratelimit-reset-requests", "x-ratelimit-reset-tokens"] {
            assert!(
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| !value.is_empty()),
                "the existing reset slot must be explicitly present"
            );
        }
        let bytes = to_bytes(response.into_body(), 1 << 20).await?;
        let body: Value = serde_json::from_slice(&bytes)?;
        assert!(
            body.get("value")
                .and_then(Value::as_str)
                .is_some_and(|value| value.starts_with("aisix_rt_") && value != provider_secret),
            "success must contain the authority-issued temporary bearer"
        );
        assert!(
            body.get("expires_at")
                .and_then(Value::as_i64)
                .is_some_and(|expiry| expiry > Utc::now().timestamp()),
            "success must contain a real future expiry"
        );
        let wire = body.to_string();
        assert!(
            !wire.contains(&provider_key) && !wire.contains(&provider_secret),
            "the public envelope cannot disclose supplier credentials"
        );

        let history = supplier
            .received_requests()
            .await
            .ok_or_else(|| std::io::Error::other("actual provider history is required"))?;
        assert_eq!(
            history.len(),
            index,
            "one actual supplier mint for each successful request, with full history retained"
        );
        assert!(
            history
                .iter()
                .all(|request| request.method.as_str() == "POST"
                    && request.url.path() == purpose.path()
                    && serde_json::from_slice::<Value>(&request.body).ok()
                        == Some(purpose.request(purpose.upstream()))),
            "both actual provider requests must preserve the legal purpose/model slots"
        );

        let scrape = metrics
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/metrics")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(
            scrape.status(),
            StatusCode::OK,
            "the production metrics HTTP handler must serve the scrape"
        );
        let bytes = to_bytes(scrape.into_body(), 8 << 20).await?;
        let scrape = std::str::from_utf8(&bytes)?;
        assert_gauge(
            scrape,
            "aisix_ratelimit_remaining_requests",
            purpose,
            remaining,
        );
        assert_gauge(scrape, "aisix_ratelimit_remaining_tokens", purpose, 70);
    }
    drop(app);
    drop(metrics);
    authority.close().await;
    drop(authority);
    Ok(())
}

#[tokio::test]
async fn ga_realtime_mint_reports_real_request_and_token_windows_in_headers_and_metrics(
) -> TestResult {
    successful_mints_report_their_actual_window(Purpose::Realtime).await
}

#[tokio::test]
async fn ga_transcription_mint_reports_its_granted_asr_model_window_in_headers_and_metrics(
) -> TestResult {
    successful_mints_report_their_actual_window(Purpose::Transcription).await
}

#[tokio::test]
async fn ga_translation_mint_reports_real_request_and_token_windows_in_headers_and_metrics(
) -> TestResult {
    successful_mints_report_their_actual_window(Purpose::Translation).await
}
