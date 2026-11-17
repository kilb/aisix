use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::Ordering;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct TrackedAllocator;

fn allocated() {
    let _ = ALLOCATIONS.try_with(|counter| {
        if let Some(count) = counter.get() {
            counter.set(Some(count + 1));
        }
    });
}

// 仅测试二进制使用；计数限定在当前线程的显式范围，其他线程保持 System 行为。
unsafe impl GlobalAlloc for TrackedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated();
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: TrackedAllocator = TrackedAllocator;

struct AllocationScope;

impl Drop for AllocationScope {
    fn drop(&mut self) {
        ALLOCATIONS.with(|counter| counter.set(None));
    }
}

fn count_allocations(work: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|counter| assert!(counter.replace(Some(0)).is_none()));
    let scope = AllocationScope;
    work();
    let count = ALLOCATIONS.with(|counter| counter.get().unwrap());
    drop(scope);
    count
}

fn exporter_kinds() -> Vec<ObservabilityExporter> {
    [
        json!({"kind": "otlp_http", "endpoint": "https://collector.example/v1/traces"}),
        json!({"kind": "aliyun_sls", "endpoint": "ap-southeast-3.log.aliyuncs.com",
            "project": "p", "logstore": "l", "credential_ref": "r"}),
        json!({"kind": "datadog", "site": "datadoghq.com", "credential_ref": "r",
            "service": "s"}),
        json!({"kind": "object_store", "provider": "s3", "bucket": "b",
            "prefix": "p", "credential_ref": "r"}),
    ]
    .into_iter()
    .map(|mut doc| {
        doc["name"] = "selected".into();
        doc["request_headers"] = json!(["x-team", "x-region", "x-client"]);
        doc["upstream_response_headers"] = json!(["x-request-id", "x-region", "x-server"]);
        serde_json::from_value(doc).unwrap()
    })
    .collect()
}

#[test]
fn header_order_preserves_every_exporter_fingerprint() {
    for original in exporter_kinds() {
        let expected = fingerprint_exporter(&original);
        for request_shift in 0..original.request_headers.len() {
            for response_shift in 0..original.upstream_response_headers.len() {
                for reverse in [false, true] {
                    let mut reordered = original.clone();
                    reordered.request_headers.rotate_left(request_shift);
                    reordered
                        .upstream_response_headers
                        .rotate_left(response_shift);
                    if reverse {
                        reordered.request_headers.reverse();
                        reordered.upstream_response_headers.reverse();
                    }
                    assert_eq!(
                        fingerprint_exporter(&reordered),
                        expected,
                        "重排相同 header 集合不能撤销 pipeline: {reordered:?}",
                    );
                }
            }
        }
    }
}

#[test]
fn header_membership_and_direction_still_change_every_exporter_fingerprint() {
    for original in exporter_kinds() {
        let expected = fingerprint_exporter(&original);
        for response in [false, true] {
            for change in ["add", "remove", "replace", "clear"] {
                let mut changed = original.clone();
                let selection = if response {
                    &mut changed.upstream_response_headers
                } else {
                    &mut changed.request_headers
                };
                match change {
                    "add" => selection.push("x-added".into()),
                    "remove" => {
                        selection.remove(0);
                    }
                    "replace" => selection[0] = "x-replaced".into(),
                    _ => selection.clear(),
                }
                assert_ne!(
                    fingerprint_exporter(&changed),
                    expected,
                    "成员变化必须仍撤销旧副本: response={response}, {change}",
                );
            }
        }
        let mut swapped = original.clone();
        std::mem::swap(
            &mut swapped.request_headers,
            &mut swapped.upstream_response_headers,
        );
        assert_ne!(fingerprint_exporter(&swapped), expected);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn header_fingerprints_and_warm_pipeline_lookups_do_not_allocate() {
    assert!(
        count_allocations(|| {
            black_box(vec![0_u8; 64]);
        }) > 0,
        "先证明真实堆分配能被计数，避免失效的零分配断言",
    );
    let exporters = exporter_kinds();
    let fan_out = OtlpHttpFanOut::new();
    let otlp = &exporters[0];
    fan_out.pipeline_for(otlp);
    let clones = fan_out.inner.work.config_clones.load(Ordering::Relaxed);
    assert!(clones > 0);
    let allocations = count_allocations(|| {
        for _ in 0..128 {
            for exporter in &exporters {
                black_box(fingerprint_exporter(black_box(exporter)));
            }
            assert!(fan_out
                .inner
                .exporters
                .is_running(&otlp.name, fingerprint_exporter(otlp)));
            black_box(fan_out.pipeline_for(black_box(otlp)));
        }
    });
    assert_eq!(allocations, 0, "逐事件的指纹及已存在 pipeline 查询不能分配");
    assert_eq!(
        fan_out.inner.work.config_clones.load(Ordering::Relaxed),
        clones,
    );
    fan_out.shutdown().await;
}

// 同线程 runtime 在首次 await 前不运行 worker，确保重排发生时记录确实仍在队列里。
#[tokio::test(flavor = "current_thread")]
async fn header_order_keeps_queued_records_and_both_snapshot_generations() {
    for changed in ["request", "response", "both"] {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let mut old = exporter_kinds().remove(0);
        let ExporterKind::OtlpHttp(cfg) = &mut old.kind else {
            unreachable!()
        };
        cfg.endpoint = format!("{}/v1/traces", server.uri());
        let mut new = old.clone();
        if changed != "response" {
            new.request_headers.reverse();
        }
        if changed != "request" {
            new.upstream_response_headers.reverse();
        }
        let fan_out = OtlpHttpFanOut::new();
        let enqueue = |id: &str, generation, exporter| {
            fan_out.fan_out(
                &UsageEvent {
                    request_id: id.into(),
                    status_code: 200,
                    request_headers: [("x-team".into(), "team-one".into())].into(),
                    upstream_response_headers: [("x-request-id".into(), "upstream-one".into())]
                        .into(),
                    ..Default::default()
                },
                None,
                None,
                None,
                generation,
                std::iter::once(exporter),
            );
        };
        enqueue("before-publication", 1, &old);
        assert_eq!(fan_out.exporter_queue_depths(), [(old.name.clone(), 1)]);
        let clones = fan_out.inner.work.config_clones.load(Ordering::Relaxed);
        assert!(clones > 0, "首次建 pipeline 必须真实复制过配置");

        fan_out.reconcile(2, std::iter::once(&new));
        assert_eq!(
            fan_out.exporter_queue_depths(),
            [(old.name.clone(), 1)],
            "{changed}: 纯重排必须保留原队列",
        );
        let reconciles = fan_out.inner.work.reconciles.load(Ordering::Relaxed);
        enqueue("old-in-flight", 1, &old);
        enqueue("new-snapshot", 2, &new);
        assert_eq!(fan_out.exporter_queue_depths(), [(old.name.clone(), 3)]);
        assert_eq!(
            fan_out.inner.work.config_clones.load(Ordering::Relaxed),
            clones,
            "重排后访问旧/新快照均不得重建 pipeline",
        );
        assert_eq!(
            fan_out.inner.work.reconciles.load(Ordering::Relaxed),
            reconciles,
            "事件访问不得增加协调次数",
        );
        fan_out.shutdown().await;

        let mut ids = Vec::new();
        for request in server.received_requests().await.unwrap() {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            for resource in body["resourceSpans"].as_array().unwrap() {
                for scope in resource["scopeSpans"].as_array().unwrap() {
                    for span in scope["spans"].as_array().unwrap() {
                        let attributes = span["attributes"].as_array().unwrap();
                        let value = |name| {
                            &attributes.iter().find(|attr| attr["key"] == name).unwrap()["value"]
                        };
                        ids.push(
                            value("aisix.request_id")["stringValue"]
                                .as_str()
                                .unwrap()
                                .to_string(),
                        );
                        assert_eq!(
                            value("http.request.header.x-team"),
                            &json!({"arrayValue": {"values": [{"stringValue": "team-one"}]}}),
                        );
                        assert_eq!(
                            value("http.response.header.x-request-id"),
                            &json!({"arrayValue": {"values": [{"stringValue": "upstream-one"}]}}),
                        );
                    }
                }
            }
        }
        ids.sort();
        assert_eq!(ids, ["before-publication", "new-snapshot", "old-in-flight"]);
    }
}
