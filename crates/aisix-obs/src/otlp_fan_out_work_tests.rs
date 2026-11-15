use super::*;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ResourceEntry};
use std::sync::atomic::{AtomicBool, Ordering};

fn exporter(endpoint: &str, credential: &str) -> ObservabilityExporter {
    serde_json::from_value(json!({
        "name": "work-counter-exporter",
        "enabled": true,
        "kind": "otlp_http",
        "endpoint": endpoint,
        "headers": {"authorization": credential},
    }))
    .unwrap()
}

fn snapshot(exporter: ObservabilityExporter) -> AisixSnapshot {
    let snapshot = AisixSnapshot::new();
    snapshot
        .observability_exporters
        .insert(ResourceEntry::new("exporter-id", exporter, 1));
    snapshot
}

fn observed_snapshot(
    fan_out: &OtlpHttpFanOut,
    exporter: ObservabilityExporter,
) -> SnapshotHandle<AisixSnapshot> {
    let handle = SnapshotHandle::new(snapshot(exporter));
    let reconciler = fan_out.clone();
    handle.subscribe_before_publish(move |view| {
        let exporters = view.snapshot.observability_exporters.entries();
        reconciler.reconcile(view.version, exporters.iter().map(|entry| &*entry.value));
    });
    handle
}

fn visit(fan_out: &OtlpHttpFanOut, handle: &SnapshotHandle<AisixSnapshot>, request_id: &str) {
    let view = handle.load_versioned();
    let exporters = view.snapshot.observability_exporters.entries();
    fan_out.fan_out(
        &UsageEvent {
            request_id: request_id.into(),
            status_code: 200,
            ..Default::default()
        },
        None,
        None,
        None,
        view.version,
        exporters.iter().map(|entry| &*entry.value),
    );
}

async fn receiver() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .mount(&server)
        .await;
    server
}

fn request_ids(requests: &[wiremock::Request]) -> Vec<String> {
    let mut ids = Vec::new();
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        for resource in body["resourceSpans"].as_array().unwrap() {
            for scope in resource["scopeSpans"].as_array().unwrap() {
                for span in scope["spans"].as_array().unwrap() {
                    let id = span["attributes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|attr| attr["key"] == "aisix.request_id")
                        .expect("实际投递的 span 必须携带请求 id");
                    ids.push(id["value"]["stringValue"].as_str().unwrap().into());
                }
            }
        }
    }
    ids.sort();
    ids
}

// 计的是实际 manager.reconcile 调用；同一个真实 view 被多次访问不能重做协调。
#[tokio::test]
async fn repeated_real_snapshot_does_not_reconcile_each_event() {
    let server = receiver().await;
    let fan_out = OtlpHttpFanOut::new();
    let handle = observed_snapshot(&fan_out, exporter(&server.uri(), "Bearer test-token"));
    visit(&fan_out, &handle, "warmup");
    let before = fan_out.inner.work.reconciles.load(Ordering::Relaxed);
    assert!(
        before > 0,
        "真实发布 hook 必须已经执行过协调，不能是失效计数器"
    );
    for index in 0..16 {
        visit(&fan_out, &handle, &format!("repeat-{index:02}"));
    }
    let after = fan_out.inner.work.reconciles.load(Ordering::Relaxed);
    fan_out.shutdown().await;

    assert_eq!(after, before, "同版本重复投递不应再次进入协调/锁遍历");
    let mut expected: Vec<String> = (0..16).map(|index| format!("repeat-{index:02}")).collect();
    expected.push("warmup".into());
    expected.sort();
    assert_eq!(
        request_ids(&server.received_requests().await.unwrap()),
        expected
    );
}

// 计数与真正的 Clone::clone 在同一表达式中；pipeline 命中后不能继续复制投递配置。
#[tokio::test]
async fn repeated_real_snapshot_does_not_clone_delivery_configuration() {
    let server = receiver().await;
    let fan_out = OtlpHttpFanOut::new();
    let handle = observed_snapshot(&fan_out, exporter(&server.uri(), "Bearer test-token"));
    visit(&fan_out, &handle, "warmup");
    let before = fan_out.inner.work.config_clones.load(Ordering::Relaxed);
    assert!(
        before > 0,
        "首次建 pipeline 必须真的复制过配置，不能是失效计数器"
    );
    for index in 0..16 {
        visit(&fan_out, &handle, &format!("repeat-{index:02}"));
    }
    let after = fan_out.inner.work.config_clones.load(Ordering::Relaxed);
    fan_out.shutdown().await;

    assert_eq!(after, before, "命中现有 pipeline 不能重复 clone 配置");
    assert_eq!(
        request_ids(&server.received_requests().await.unwrap()).len(),
        17
    );
}

// 同 endpoint 的凭据更新也必须失效；旧 view 不得复活旧配置，关停后同版本仍能惰性重建。
#[tokio::test]
async fn published_credentials_invalidate_old_views_and_shutdown_does_not_pin_a_dead_handle() {
    let server = receiver().await;
    let fan_out = OtlpHttpFanOut::new();
    let handle = observed_snapshot(&fan_out, exporter(&server.uri(), "Bearer old-test-token"));
    let old = handle.load_versioned();
    visit(&fan_out, &handle, "old-delivered");
    fan_out.shutdown().await;

    let before = fan_out.inner.work.reconciles.load(Ordering::Relaxed);
    handle.store(snapshot(exporter(&server.uri(), "Bearer new-test-token")));
    assert_eq!(
        fan_out.inner.work.reconciles.load(Ordering::Relaxed),
        before + 1
    );
    let old_exporters = old.snapshot.observability_exporters.entries();
    fan_out.fan_out(
        &UsageEvent {
            request_id: "stale-must-not-deliver".into(),
            ..Default::default()
        },
        None,
        None,
        None,
        old.version,
        old_exporters.iter().map(|entry| &*entry.value),
    );
    assert!(
        fan_out.exporter_stats().is_empty(),
        "旧 view 不能重建已撤销的 pipeline"
    );
    visit(&fan_out, &handle, "new-delivered");
    fan_out.shutdown().await;
    visit(&fan_out, &handle, "same-generation-after-shutdown");
    fan_out.shutdown().await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer old-test-token"
    );
    for request in &requests[1..] {
        assert_eq!(request.headers["authorization"], "Bearer new-test-token");
    }
    assert_eq!(
        request_ids(&requests),
        [
            "new-delivered",
            "old-delivered",
            "same-generation-after-shutdown"
        ]
    );
}

// 重试是现有 pipeline 的职责；同版本新事件不能重建并丢掉失败中的批次。
#[tokio::test]
async fn repeated_snapshot_access_keeps_the_retrying_pipeline_and_recovers() {
    let server = wiremock::MockServer::start().await;
    let healthy = Arc::new(AtomicBool::new(false));
    let receiver_state = Arc::clone(&healthy);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            wiremock::ResponseTemplate::new(if receiver_state.load(Ordering::Relaxed) {
                200
            } else {
                503
            })
        })
        .mount(&server)
        .await;
    let fan_out = OtlpHttpFanOut::new();
    let handle = observed_snapshot(&fan_out, exporter(&server.uri(), "Bearer test-token"));
    visit(&fan_out, &handle, "retry-first");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let stats = fan_out.exporter_stats();
            let stats = &stats["work-counter-exporter"];
            if stats.retries > 0 && stats.last_error.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("必须先观察真实 HTTP 503 导致的重试");

    healthy.store(true, Ordering::Relaxed);
    for index in 0..16 {
        visit(&fan_out, &handle, &format!("after-recovery-{index}"));
    }
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let stats = fan_out
                .exporter_stats()
                .remove("work-counter-exporter")
                .unwrap();
            if stats.sent == 17 {
                return stats;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("恢复后全部 17 条事件必须由原 pipeline 送达");
    fan_out.shutdown().await;
    assert!(settled.retries > 0);
    assert_eq!(settled.failed_batches, 0);
    assert_eq!(settled.dropped, 0);
    assert!(settled.last_error.is_none());
    let requests = server.received_requests().await.unwrap();
    assert!(requests.len() >= 2, "一次失败之后必须真的重试过 HTTP");
}
