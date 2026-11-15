// 组件协议测试使用本地 HTTP 对端，不把它称为真实控制面端到端验收。
fn durable_test_event() -> UsageEvent {
    UsageEvent {
        event_id: Uuid::new_v4().to_string(),
        occurred_at: aged_rfc3339(Duration::from_secs(86_400)),
        ..sample_event("durable-terminal-delivery")
    }
}

fn durable_test_envelope(event: &UsageEvent) -> aisix_obs::usage::DurableUsageEvent {
    aisix_obs::usage::DurableUsageEvent::from_event(event).unwrap()
}

async fn durable_stop_worker(stop: watch::Sender<bool>, worker: tokio::task::JoinHandle<()>) {
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .expect("the delivery worker stops without discarding the caller's outbox")
        .unwrap();
}

async fn durable_held_receiver() -> (
    LocalServer,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let arrived_at_server = arrived.clone();
    let release_at_server = release.clone();
    let app = axum::Router::new().route(
        TELEMETRY_PATH,
        axum::routing::post(move |body: Bytes| {
            let arrived = arrived_at_server.clone();
            let release = release_at_server.clone();
            async move {
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["events"].as_array().unwrap().len(), 1);
                arrived.notify_one();
                release.notified().await;
                (
                    [(USAGE_BATCH_DEDUP_HEADER, "1")],
                    r#"{"accepted":1,"duplicates":0,"rejected":0}"#,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = LocalServer {
        addr: listener.local_addr().unwrap(),
    };
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (server, arrived, release, task)
}

#[tokio::test]
async fn durable_delivery_waits_for_control_plane_receipt() {
    let (server, arrived, release, receiver) = durable_held_receiver().await;
    let dir = tempfile::tempdir().unwrap();
    let (stop, cancel) = watch::channel(false);
    let (sink, worker) = spawn(
        test_config(format!("{}{TELEMETRY_PATH}", server.uri()), &dir),
        Metrics::new(false),
        cancel,
    );
    let delivery_sink = sink.clone();
    let delivery = tokio::spawn(async move {
        delivery_sink.emit_durable(durable_test_envelope(&durable_test_event()))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), arrived.notified())
        .await
        .expect("the durable event must reach the transport before it can be acknowledged");
    assert!(
        !delivery.is_finished(),
        "enqueue or writing a request is not a control-plane receipt"
    );
    release.notify_one();
    assert!(delivery.await.unwrap().is_ok());
    durable_stop_worker(stop, worker).await;
    drop(sink);
    receiver.abort();
}

#[tokio::test]
async fn durable_delivery_requires_an_explicit_complete_receipt() {
    let refused = [
        Answer {
            body: Some(r#"{"accepted":1,"duplicates":0,"rejected":0}"#),
            ..no_dedup(200)
        },
        Answer {
            body: Some(r#"{"accepted":1,"duplicates":0,"rejected":0}"#),
            ..dedup_value(200, "2")
        },
        Answer {
            body: Some("not-json"),
            ..dedups(200)
        },
        Answer {
            body: Some("{}"),
            ..dedups(200)
        },
        Answer {
            body: Some(r#"{"accepted":1,"duplicates":0}"#),
            ..dedups(200)
        },
        Answer {
            body: Some(r#"{"accepted":0,"duplicates":0,"rejected":1}"#),
            ..dedups(200)
        },
        Answer {
            body: Some(r#"{"accepted":2,"duplicates":0,"rejected":0}"#),
            ..dedups(200)
        },
        dedups(503),
        dedups(400),
    ];
    for answer in refused {
        let (server, posts) = scripted_server(vec![answer]).await;
        let dir = tempfile::tempdir().unwrap();
        let (stop, cancel) = watch::channel(false);
        let (sink, worker) = spawn(
            test_config(format!("{}{TELEMETRY_PATH}", server.uri()), &dir),
            Metrics::new(false),
            cancel,
        );
        let result = sink
            .emit_durable(durable_test_envelope(&durable_test_event()))
            .await;
        assert!(
            result.is_err(),
            "a refused or unconfirmed row stays in its durable outbox: {answer:?}"
        );
        assert_eq!(
            posts.lock().unwrap().len(),
            1,
            "the case must exercise the receiver's answer: {answer:?}"
        );
        durable_stop_worker(stop, worker).await;
    }
}

#[tokio::test]
async fn durable_delivery_preserves_event_and_batch_identity_across_retries() {
    let (server, posts) = scripted_server(vec![
        dedups(503),
        Answer {
            body: Some(r#"{"accepted":1,"duplicates":0,"rejected":0}"#),
            ..dedups(200)
        },
        Answer {
            body: Some(r#"{"accepted":0,"duplicates":0,"rejected":0}"#),
            ..dedups(200)
        },
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (stop, cancel) = watch::channel(false);
    let (sink, worker) = spawn(
        test_config(format!("{}{TELEMETRY_PATH}", server.uri()), &dir),
        Metrics::new(false),
        cancel,
    );
    let event = durable_test_event();
    let expected = serde_json::to_value(&event).unwrap();
    let canonical = durable_test_envelope(&event);
    assert!(sink.emit_durable(canonical.clone()).await.is_err());
    assert!(sink.emit_durable(canonical.clone()).await.is_ok());
    assert!(sink.emit_durable(canonical).await.is_ok());
    let recorded = posts.lock().unwrap().clone();
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[0].0, recorded[1].0);
    assert_eq!(recorded[1].0, recorded[2].0);
    for (_, body) in recorded {
        assert_eq!(body["events"], serde_json::json!([expected]));
    }
    durable_stop_worker(stop, worker).await;
}

#[tokio::test]
async fn durable_delivery_worker_abort_cannot_confirm_an_inflight_event() {
    let (server, arrived, _release, receiver) = durable_held_receiver().await;
    let dir = tempfile::tempdir().unwrap();
    let (_stop, cancel) = watch::channel(false);
    let (sink, worker) = spawn(
        test_config(format!("{}{TELEMETRY_PATH}", server.uri()), &dir),
        Metrics::new(false),
        cancel,
    );
    let delivery = tokio::spawn(async move {
        sink.emit_durable(durable_test_envelope(&durable_test_event()))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), arrived.notified())
        .await
        .expect("the worker must hold a real unconfirmed transport attempt");
    worker.abort();
    assert!(delivery.await.unwrap().is_err());
    receiver.abort();
}
