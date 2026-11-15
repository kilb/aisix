//! 原生文件和批次的指标组件验收：驱动正式代理路由后抓取正式 /metrics。
//! 上游 HTTP 与持久注册器为本地替身，不代表提供商端到端验收。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aisix_admin::{metrics_router, ConfigStore, InMemoryStore, ModelsStatusState};
use aisix_core::config::PrometheusConfig;
use aisix_core::native_objects::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{
    AisixSnapshot, ApiKey, ConfigStatus, Model, ProviderKey, ProxyConfig, ResourceEntry, SourceKind,
};
use aisix_gateway::Hub;
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";
const KEY: &str = "33333333-3333-4333-8333-333333333333";
const FILE: &str = "file_metrics_fixture";
const BATCH: &str = "msgbatch_metrics_fixture";
const BEARER: &str = "metrics-private-caller-credential-a7e2";
const UPSTREAM_SECRET: &str = "metrics-upstream-key-private-c46e";

#[derive(Default)]
struct MetricGate {
    entered: Notify,
    released: Notify,
    paused: AtomicBool,
}

impl MetricGate {
    async fn pause(&self) {
        if !self.paused.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.released.notified().await;
        }
    }
}

#[derive(Default)]
struct Registry {
    operations: Mutex<HashMap<String, BeginRequest>>,
    records: Mutex<HashMap<String, NativeRecord>>,
    gate: Arc<MetricGate>,
}

macro_rules! unavailable {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for Registry {
    unavailable!(claim, ClaimRequest);
    unavailable!(renew, LeaseRequest);
    unavailable!(reschedule, LeaseRequest);
    unavailable!(complete, CompleteRequest);

    fn begin(&self, body: BeginRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let mut reply = NativeReply::new("pending_submission");
        reply.operation_id = Some(body.operation_id);
        reply.origin = Some(body.origin.clone());
        self.operations
            .lock()
            .unwrap()
            .insert(body.operation_id.to_string(), body);
        Box::pin(async move {
            self.gate.pause().await;
            Ok(reply)
        })
    }

    fn bind(&self, body: BindRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let submission = self
            .operations
            .lock()
            .unwrap()
            .get(&body.operation_id.to_string())
            .cloned();
        Box::pin(async move {
            let submission = submission.ok_or(NativeRegistryError::Unavailable)?;
            let record = serde_json::from_value(json!({
                "record_id":"00000000-0000-4000-8000-000000000001",
                "operation_id": body.operation_id, "requested_model_id":submission.requested_model_id,
                "requested_model":submission.requested_model,"protocol":submission.protocol,
                "kind":submission.kind,"origin":submission.origin,"raw_id":body.raw_id,
                "entries":submission.entries,"version":1
            })).map_err(|_| NativeRegistryError::Unavailable)?;
            let mut reply = NativeReply::new("bound");
            reply.record = Some(record);
            Ok(reply)
        })
    }

    fn lookup(
        &self,
        body: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let record = self
            .records
            .lock()
            .unwrap()
            .get(&body.locator.raw_id)
            .cloned();
        Box::pin(async move {
            self.gate.pause().await;
            let mut reply = NativeReply::new("found");
            reply.record = Some(record.ok_or(NativeRegistryError::NotFound)?);
            Ok(reply)
        })
    }

    fn owned(&self, body: OwnedRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        let records = self.records.lock().unwrap();
        let mut reply = NativeReply::new("owned");
        reply.records = body
            .raw_ids
            .iter()
            .filter_map(|id| records.get(id).cloned())
            .collect();
        Box::pin(async move {
            self.gate.pause().await;
            Ok(reply)
        })
    }

    fn tombstone(
        &self,
        _: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Ok(NativeReply::new("deleted")) })
    }
}

fn state(base: &str, gate: Arc<MetricGate>) -> Result<ProxyState, Box<dyn std::error::Error>> {
    let snapshot = AisixSnapshot::new();
    let provider: ProviderKey = serde_json::from_value(json!({
        "display_name":"native", "secret":UPSTREAM_SECRET, "provider":"anthropic",
        "api_base":base,"adapter":"anthropic"
    }))?;
    let platform = aisix_proxy::platform::PlatformSettings::from_secret("metrics-test-boundary");
    let registry = Arc::new(Registry {
        gate,
        ..Default::default()
    });
    for (kind, raw_id) in [
        (NativeObjectKind::File, FILE),
        (NativeObjectKind::MessageBatch, BATCH),
    ] {
        let boundary = configured_native_origin(
            &platform.id_key,
            serde_json::from_value(json!(PROVIDER))?,
            &provider,
            NativeProtocol::Anthropic,
            kind,
        )?;
        let record: NativeRecord = serde_json::from_value(json!({
            "record_id":"00000000-0000-4000-8000-000000000001", "protocol":"anthropic",
            "kind":kind,"raw_id":raw_id,"completed":true,"requested_model_id":MODEL,
            "requested_model":"native-model","version":1,
            "origin":{"model_id":MODEL,"provider_key_id":PROVIDER,"boundary":boundary},
            "entries": if kind == NativeObjectKind::MessageBatch {json!([{
                "entry_id":"one", "requested_model_id":MODEL,"model_id":MODEL,
                "requested_model":"native-model","upstream_model":"provider-model"
            }])} else {json!([])}
        }))?;
        registry
            .records
            .lock()
            .unwrap()
            .insert(raw_id.to_owned(), record);
    }
    snapshot
        .provider_keys
        .insert(ResourceEntry::new(PROVIDER, provider, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"native-model","provider":"anthropic","model_name":"provider-model",
        "provider_key_id":PROVIDER
    }))?;
    snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer(BEARER),"allowed_models":["native-model"],
        "allowed_endpoints":["files","batches","responses"]
    }))?;
    snapshot.apikeys.insert(ResourceEntry::new(KEY, key, 1));
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"}))?;
    Ok(
        ProxyState::new(SnapshotHandle::new(snapshot), Arc::new(Hub::new()), &config)
            .without_cache()
            .with_platform(platform)
            .with_native_objects(registry),
    )
}

fn metadata() -> Value {
    json!({"id":FILE,"type":"file","filename":"notes.txt","size_bytes":4,
        "mime_type":"text/plain","created_at":"2026-11-01T00:00:00Z"})
}

fn batch() -> Value {
    json!({"id":BATCH,"type":"message_batch","processing_status":"ended",
        "request_counts":{"processing":0,"succeeded":1,"errored":0,"canceled":0,"expired":0},
        "created_at":"2026-11-01T00:00:00Z","ended_at":"2026-11-01T00:00:01Z",
        "expires_at":"2026-11-02T00:00:00Z","results_url":null})
}

fn samples<'a>(scrape: &'a str, family: &str, endpoint: &str) -> Vec<&'a str> {
    let prefix = format!("{family}{{");
    let endpoint = format!("endpoint=\"{endpoint}\"");
    scrape
        .lines()
        .filter(|line| line.starts_with(&prefix) && line.contains(&endpoint))
        .collect()
}

async fn scrape_metrics(
    metrics: Arc<aisix_obs::Metrics>,
) -> Result<String, Box<dyn std::error::Error>> {
    let scrape = metrics_router(
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
    .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
    .await?;
    assert_eq!(scrape.status(), StatusCode::OK);
    let bytes = to_bytes(scrape.into_body(), 8 << 20).await?;
    Ok(std::str::from_utf8(&bytes)?.to_owned())
}

fn assert_in_flight(scrape: &str, endpoint: &str, protocol: &str, count: &str) {
    let inflight = samples(scrape, "aisix_proxy_in_flight_requests", endpoint);
    assert_eq!(inflight.len(), 1, "{endpoint}: {scrape}");
    assert!(
        inflight[0].contains(&format!("inbound_protocol=\"{protocol}\"")),
        "{}",
        inflight[0]
    );
    assert_eq!(
        inflight[0].rsplit_once(' ').map(|(_, value)| value),
        Some(count),
        "{}",
        inflight[0]
    );
}

#[tokio::test]
async fn native_files_and_batches_report_each_proxy_operation_without_llm_inflation(
) -> Result<(), Box<dyn std::error::Error>> {
    let batch_create = json!({"requests":[{"custom_id":"one","params":{
        "model":"native-model","max_tokens":10,"messages":[{"role":"user","content":"hello"}]
    }}]});
    let result = json!({"custom_id":"one","result":{"type":"succeeded","message":{
        "id":"msg_result_fixture","type":"message","role":"assistant","model":"provider-model",
        "content":[{"type":"text","text":"done"}],"usage":{"input_tokens":3,"output_tokens":1}
    }}})
    .to_string()
        + "\n";
    // 每个方法使用独立指标集，缺失某个发射器或重复发射都必须失败。
    for (verb, route, endpoint, multipart, request_body, response_body) in [
        (
            "POST",
            "/v1/files?model=native-model",
            "/v1/files",
            true,
            None,
            metadata().to_string(),
        ),
        (
            "GET",
            "/v1/files?model=native-model",
            "/v1/files",
            false,
            None,
            json!({"data":[metadata()],"next_page":null}).to_string(),
        ),
        (
            "GET",
            "/v1/files/file_metrics_fixture",
            "/v1/files/:id",
            false,
            None,
            metadata().to_string(),
        ),
        (
            "DELETE",
            "/v1/files/file_metrics_fixture",
            "/v1/files/:id",
            false,
            None,
            json!({"id":FILE,"type":"file_deleted"}).to_string(),
        ),
        (
            "GET",
            "/v1/files/file_metrics_fixture/content",
            "/v1/files/:id",
            false,
            None,
            "data".to_owned(),
        ),
        (
            "POST",
            "/v1/messages/batches",
            "/v1/messages/batches",
            false,
            Some(batch_create),
            batch().to_string(),
        ),
        (
            "GET",
            "/v1/messages/batches?model=native-model",
            "/v1/messages/batches",
            false,
            None,
            json!({"data":[batch()],"has_more":false,"first_id":BATCH,"last_id":BATCH}).to_string(),
        ),
        (
            "GET",
            "/v1/messages/batches/msgbatch_metrics_fixture",
            "/v1/messages/batches/:id",
            false,
            None,
            batch().to_string(),
        ),
        (
            "DELETE",
            "/v1/messages/batches/msgbatch_metrics_fixture",
            "/v1/messages/batches/:id",
            false,
            None,
            json!({"id":BATCH,"type":"message_batch_deleted"}).to_string(),
        ),
        (
            "POST",
            "/v1/messages/batches/msgbatch_metrics_fixture/cancel",
            "/v1/messages/batches/:id/cancel",
            false,
            None,
            batch().to_string(),
        ),
        (
            "GET",
            "/v1/messages/batches/msgbatch_metrics_fixture/results",
            "/v1/messages/batches/:id/results",
            false,
            None,
            result,
        ),
    ] {
        let upstream = MockServer::builder().start().await;
        Mock::given(method(verb))
            .and(path(route.split('?').next().unwrap()))
            .respond_with(ResponseTemplate::new(200).set_body_string(response_body))
            .expect(1)
            .mount(&upstream)
            .await;
        let gate = Arc::new(MetricGate::default());
        let state = state(&upstream.uri(), gate.clone())?;
        let metrics = state.metrics.clone();
        let mut request = Request::builder()
            .method(verb)
            .uri(route)
            .header("x-api-key", BEARER)
            .header("anthropic-version", "2026-10-21");
        let request_body = if multipart {
            request = request.header("content-type", "multipart/form-data; boundary=metrics-test");
            Body::from("--metrics-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\ndata\r\n--metrics-test--\r\n")
        } else if let Some(value) = request_body {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        } else {
            Body::empty()
        };
        let pending = build_router(state).oneshot(request.body(request_body)?);
        tokio::pin!(pending);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = gate.entered.notified() => {},
                _ = &mut pending => panic!("{verb} {route} ended before the metrics gate"),
            }
        })
        .await?;
        let active = scrape_metrics(metrics.clone()).await?;
        assert_in_flight(&active, endpoint, "anthropic", "1");
        assert!(
            samples(&active, "aisix_proxy_requests_total", endpoint).is_empty(),
            "request recorded before completion: {verb} {route}: {active}"
        );
        gate.released.notify_one();
        let response = pending.await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "{verb} {route}: {}",
            String::from_utf8_lossy(&bytes)
        );
        upstream.verify().await;
        let scrape = scrape_metrics(metrics).await?;
        let proxy = samples(&scrape, "aisix_proxy_requests_total", endpoint);
        assert_eq!(proxy.len(), 1, "{verb} {route}: {scrape}");
        assert!(proxy[0].contains("status=\"200\""), "{}", proxy[0]);
        assert!(
            proxy[0].contains("inbound_protocol=\"anthropic\""),
            "{}",
            proxy[0]
        );
        assert_eq!(proxy[0].rsplit_once(' ').map(|(_, count)| count), Some("1"));
        assert!(
            samples(&scrape, "aisix_llm_requests_total", endpoint).is_empty(),
            "management inflated inference: {verb} {route}: {scrape}"
        );
        assert_in_flight(&scrape, endpoint, "anthropic", "0");
        for raw in [FILE, BATCH, "msg_result_fixture", BEARER, UPSTREAM_SECRET] {
            assert!(!scrape.contains(raw), "metrics leaked raw identity {raw}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn shared_files_protocol_is_recorded_before_authentication(
) -> Result<(), Box<dyn std::error::Error>> {
    for (header, protocol) in [
        (Some("anthropic-version"), "anthropic"),
        (Some("anthropic-beta"), "anthropic"),
        (None, "openai"),
    ] {
        for (verb, route, endpoint) in [
            ("POST", "/v1/files", "/v1/files"),
            ("GET", "/v1/files", "/v1/files"),
            ("GET", "/v1/files/file_metrics_fixture", "/v1/files/:id"),
            ("DELETE", "/v1/files/file_metrics_fixture", "/v1/files/:id"),
            (
                "GET",
                "/v1/files/file_metrics_fixture/content",
                "/v1/files/:id",
            ),
        ] {
            let state = state("http://127.0.0.1:9", Arc::default())?;
            let metrics = state.metrics.clone();
            let mut request = Request::builder().method(verb).uri(route);
            if let Some(header) = header {
                request = request.header(header, "private-header-value-must-not-be-a-label");
            }
            let response = build_router(state)
                .oneshot(request.body(Body::empty())?)
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            to_bytes(response.into_body(), 1 << 20).await?;
            let scrape = scrape_metrics(metrics).await?;
            let proxy = samples(&scrape, "aisix_proxy_requests_total", endpoint);
            assert_eq!(proxy.len(), 1, "{header:?} {verb} {route}: {scrape}");
            assert!(proxy[0].contains("status=\"401\""), "{}", proxy[0]);
            assert!(
                proxy[0].contains(&format!("inbound_protocol=\"{protocol}\"")),
                "{}",
                proxy[0]
            );
            assert_eq!(proxy[0].rsplit_once(' ').map(|(_, count)| count), Some("1"));
            assert_in_flight(&scrape, endpoint, protocol, "0");
            for raw in [FILE, "private-header-value-must-not-be-a-label"] {
                assert!(!scrape.contains(raw), "metrics leaked raw identity {raw}");
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn inference_metrics_follow_the_operation_on_shared_and_management_paths(
) -> Result<(), Box<dyn std::error::Error>> {
    for (verb, route, endpoint, inference) in [
        ("POST", "/v1/chat/completions", "/v1/chat/completions", true),
        ("GET", "/v1/chat/completions", "/v1/chat/completions", false),
        (
            "GET",
            "/v1/chat/completions/chatcmpl_sample",
            "/v1/chat/completions/:id",
            false,
        ),
        (
            "POST",
            "/v1/chat/completions/chatcmpl_sample",
            "/v1/chat/completions/:id",
            false,
        ),
        (
            "DELETE",
            "/v1/chat/completions/chatcmpl_sample",
            "/v1/chat/completions/:id",
            false,
        ),
        (
            "GET",
            "/v1/chat/completions/chatcmpl_sample/messages",
            "/v1/chat/completions/:id/messages",
            false,
        ),
        ("POST", "/v1/responses", "/v1/responses", true),
        ("GET", "/v1/responses", "/v1/responses", false),
        (
            "GET",
            "/v1/responses/resp_sample",
            "/v1/responses/:id",
            false,
        ),
        (
            "DELETE",
            "/v1/responses/resp_sample",
            "/v1/responses/:id",
            false,
        ),
        (
            "POST",
            "/v1/responses/resp_sample/cancel",
            "/v1/responses/:id/cancel",
            false,
        ),
        (
            "GET",
            "/v1/responses/resp_sample/input_items",
            "/v1/responses/:id/input_items",
            false,
        ),
        ("POST", "/v1/moderations", "/v1/moderations", true),
        (
            "POST",
            "/v1/content_provenance_checks",
            "/v1/content_provenance_checks",
            true,
        ),
        ("POST", "/v1/uploads", "/v1/uploads", false),
        (
            "POST",
            "/v1/uploads/upload_sample/parts",
            "/v1/uploads/:id/parts",
            false,
        ),
        (
            "POST",
            "/v1/uploads/upload_sample/complete",
            "/v1/uploads/:id/complete",
            false,
        ),
        (
            "POST",
            "/v1/uploads/upload_sample/cancel",
            "/v1/uploads/:id/cancel",
            false,
        ),
    ] {
        let state = state("http://127.0.0.1:9", Arc::default())?;
        let metrics = state.metrics.clone();
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method(verb)
                    .uri(route)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{verb} {route}"
        );
        to_bytes(response.into_body(), 1 << 20).await?;
        let scrape = scrape_metrics(metrics).await?;
        let proxy = samples(&scrape, "aisix_proxy_requests_total", endpoint);
        assert_eq!(proxy.len(), 1, "{verb} {route}: {scrape}");
        assert!(proxy[0].contains("status=\"401\""), "{}", proxy[0]);
        let llm = samples(&scrape, "aisix_llm_requests_total", endpoint);
        if inference {
            assert_eq!(llm.len(), 1, "{verb} {route}: {scrape}");
            assert_eq!(llm[0].rsplit_once(' ').map(|(_, count)| count), Some("1"));
        } else {
            assert!(
                llm.is_empty(),
                "management inflated inference: {verb} {route}: {scrape}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn responses_websocket_handshake_does_not_count_as_an_inference(
) -> Result<(), Box<dyn std::error::Error>> {
    let state = state("http://127.0.0.1:9", Arc::default())?;
    let metrics = state.metrics.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, build_router(state)).await });
    let mut connection = tokio::net::TcpStream::connect(address).await?;
    connection.write_all(format!(
        "GET /v1/responses HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {BEARER}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    ).as_bytes()).await?;
    let head = tokio::time::timeout(Duration::from_secs(5), async {
        let mut head = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            let count = connection.read(&mut buffer).await?;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "WebSocket handshake closed before its headers",
                ));
            }
            head.extend_from_slice(&buffer[..count]);
            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(head);
            }
        }
    })
    .await??;
    assert!(
        head.starts_with(b"HTTP/1.1 101 "),
        "{}",
        String::from_utf8_lossy(&head)
    );
    let scrape = scrape_metrics(metrics).await?;
    let proxy = samples(&scrape, "aisix_proxy_requests_total", "/v1/responses");
    assert_eq!(proxy.len(), 1, "{scrape}");
    assert!(proxy[0].contains("status=\"101\""), "{}", proxy[0]);
    assert_eq!(proxy[0].rsplit_once(' ').map(|(_, count)| count), Some("1"));
    assert!(
        samples(&scrape, "aisix_llm_requests_total", "/v1/responses").is_empty(),
        "an idle WebSocket has performed no inference: {scrape}"
    );
    drop(connection);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn cancelled_shared_files_keep_the_initial_protocol_in_usage_and_metrics(
) -> Result<(), Box<dyn std::error::Error>> {
    for header in ["anthropic-version", "anthropic-beta"] {
        let gate = Arc::new(MetricGate::default());
        let state = state("http://127.0.0.1:9", gate.clone())?;
        let metrics = state.metrics.clone();
        let (sender, mut events) = tokio::sync::mpsc::channel(8);
        let state = state
            .with_usage_sink(aisix_obs::UsageSink::new(sender).with_metrics((*metrics).clone()));
        let request = Request::builder()
            .method("POST")
            .uri("/v1/files?model=native-model")
            .header("x-api-key", BEARER)
            .header(header, "private-header-value-must-not-be-a-label")
            .header("content-type", "multipart/form-data; boundary=metrics-test")
            .body(Body::from("--metrics-test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\ndata\r\n--metrics-test--\r\n"))?;
        let mut pending = Box::pin(build_router(state).oneshot(request));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = gate.entered.notified() => {},
                _ = &mut pending => panic!("{header} upload ended before cancellation"),
            }
        })
        .await?;
        let active = scrape_metrics(metrics.clone()).await?;
        assert_in_flight(&active, "/v1/files", "anthropic", "1");
        drop(pending);
        let event = events.try_recv()?;
        assert_eq!(event.status_code, 499);
        assert_eq!(event.inbound_protocol, "anthropic");
        assert!(events.try_recv().is_err(), "cancellation emitted twice");
        let scrape = scrape_metrics(metrics).await?;
        assert_in_flight(&scrape, "/v1/files", "anthropic", "0");
        let emitted: Vec<_> = scrape
            .lines()
            .filter(|line| line.starts_with("aisix_usage_events_emitted_total{"))
            .collect();
        assert_eq!(emitted.len(), 1, "{scrape}");
        assert!(emitted[0].contains("status=\"499\""), "{}", emitted[0]);
        assert!(
            emitted[0].contains("inbound_protocol=\"anthropic\""),
            "{}",
            emitted[0]
        );
        assert_eq!(
            emitted[0].rsplit_once(' ').map(|(_, count)| count),
            Some("1")
        );
        for raw in [
            FILE,
            BEARER,
            UPSTREAM_SECRET,
            "private-header-value-must-not-be-a-label",
        ] {
            assert!(!scrape.contains(raw), "metrics leaked raw identity {raw}");
        }
    }
    Ok(())
}
