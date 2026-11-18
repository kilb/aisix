use std::collections::HashSet;
use std::process::{Child, Command, Stdio};

const SPOOL_CHILD_DIR: &str = "AISIX_TEST_USAGE_SPOOL_CHILD_DIR";
const SPOOL_CHILD_URL: &str = "AISIX_TEST_USAGE_SPOOL_CHILD_URL";
const SPOOL_ENV: &str = "usage-spool-test-env";
const SPOOL_DP: &str = "usage-spool-test-dp";

fn spool_test_config(root: &Path, url: String) -> TelemetryConfig {
    let mut cfg = TelemetryConfig::new(
        url,
        MtlsBundle {
            ca_cert_path: root.join("ca.crt"),
            client_cert_path: root.join("client.crt"),
            client_key_path: root.join("client.key"),
            extra_ca_pem: None,
        },
    );
    cfg.interval = Duration::from_millis(10);
    cfg
}

fn spool_event(id: &str) -> UsageEvent {
    UsageEvent {
        event_id: id.into(),
        request_id: id.into(),
        occurred_at: chrono::Utc::now().to_rfc3339(),
        status_code: 200,
        prompt_tokens: 15,
        completion_tokens: 3,
        ..UsageEvent::default()
    }
}

async fn wait_until_spooled(metrics: &Metrics, pending: usize, reconciliation: usize) {
    let pending = format!("aisix_usage_spool_batches{{state=\"pending\"}} {pending}");
    let reconciliation =
        format!("aisix_usage_spool_batches{{state=\"reconciliation\"}} {reconciliation}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rendered = metrics.render();
            if rendered.lines().any(|line| line == pending)
                && rendered.lines().any(|line| line == reconciliation)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("usage spool state did not converge: {}", metrics.render()));
}

#[tokio::test]
async fn usage_spool_writer_child() {
    let Some(root) = std::env::var_os(SPOOL_CHILD_DIR) else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let cfg = spool_test_config(&root, std::env::var(SPOOL_CHILD_URL).unwrap());
    let metrics = Metrics::new(false);
    let (_stop, cancel) = watch::channel(false);
    let (sink, _task) = spawn_spooled(
        cfg,
        metrics.clone(),
        cancel,
        &root.join("queue"),
        SPOOL_ENV,
        SPOOL_DP,
    )
    .unwrap();

    sink.try_emit(
        "chat",
        spool_event("spool-first"),
        UsageEventLabels::default(),
    );
    wait_until_spooled(&metrics, 1, 0).await;
    sink.try_emit(
        "chat",
        spool_event("spool-second"),
        UsageEventLabels::default(),
    );
    wait_until_spooled(&metrics, 2, 0).await;
    std::fs::write(root.join("checkpoint"), b"two batches synced").unwrap();
    std::future::pending::<()>().await;
}

struct SpoolChild(Child);

impl Drop for SpoolChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn usage_spool_replays_synced_batches_in_order_after_a_process_kill() {
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };

    #[derive(Clone)]
    struct Peer {
        calls: tokio::sync::mpsc::UnboundedSender<(String, Bytes)>,
        committed: Arc<std::sync::Mutex<HashSet<String>>>,
        first: Arc<tokio::sync::Semaphore>,
    }

    async fn receive(
        State(peer): State<Peer>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, HeaderMap, Json<serde_json::Value>) {
        let mut reply = HeaderMap::new();
        reply.insert("x-aisix-usage-batch-dedup", "1".parse().unwrap());
        let Some(id) = headers.get(USAGE_BATCH_ID_HEADER) else {
            return (
                StatusCode::BAD_REQUEST,
                reply,
                Json(serde_json::json!({"code": "bad_request"})),
            );
        };
        let id = id.to_str().unwrap().to_owned();
        let (fresh, first) = {
            let mut committed = peer.committed.lock().unwrap();
            let first = committed.is_empty();
            (committed.insert(id.clone()), first)
        };
        peer.calls.send((id, body)).unwrap();
        if first {
            let _ = peer.first.acquire().await;
        }
        (
            StatusCode::OK,
            reply,
            Json(serde_json::json!({
                "accepted": usize::from(fresh), "rejected": 0, "duplicates": 0
            })),
        )
    }

    let root = tempfile::tempdir().unwrap();
    write_test_bundle(root.path());
    let (calls, mut captured) = tokio::sync::mpsc::unbounded_channel();
    let committed = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let first = Arc::new(tokio::sync::Semaphore::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{}",
        listener.local_addr().unwrap(),
        TELEMETRY_PATH
    );
    let router = Router::new()
        .route(TELEMETRY_PATH, post(receive))
        .with_state(Peer {
            calls,
            committed: committed.clone(),
            first: first.clone(),
        });
    let peer = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let mut child = SpoolChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "telemetry::tests::usage_spool_writer_child"])
            .env(SPOOL_CHILD_DIR, root.path())
            .env(SPOOL_CHILD_URL, &url)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let first_attempt = tokio::time::timeout(Duration::from_secs(10), captured.recv())
        .await
        .expect("child sent the first stored batch")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.path().join("checkpoint").is_file() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("second batch reached disk while CP held the first receipt");
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    first.add_permits(1);

    let (stop, cancel) = watch::channel(false);
    let metrics = Metrics::new(false);
    let (_sink, task) = spawn_spooled(
        spool_test_config(root.path(), url),
        metrics.clone(),
        cancel,
        &root.path().join("queue"),
        SPOOL_ENV,
        SPOOL_DP,
    )
    .unwrap();
    let retry = tokio::time::timeout(Duration::from_secs(10), captured.recv())
        .await
        .unwrap()
        .unwrap();
    let subsequent = tokio::time::timeout(Duration::from_secs(10), captured.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry, first_attempt, "retry must preserve UUID and bytes");
    assert_ne!(subsequent.0, retry.0);
    for ((_, body), expected_id) in [retry, subsequent]
        .iter()
        .zip(["spool-first", "spool-second"])
    {
        let json: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(json["events"].as_array().unwrap().len(), 1);
        assert_eq!(json["events"][0]["event_id"], expected_id);
        assert_eq!(json["events"][0]["prompt_tokens"], 15);
    }
    wait_until_spooled(&metrics, 0, 0).await;
    assert_eq!(committed.lock().unwrap().len(), 2);
    let files = std::fs::read_dir(root.path().join("queue"))
        .unwrap()
        .count();
    assert_eq!(files, 2, "only the identity and lock files remain");
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    peer.abort();
}

#[tokio::test]
async fn usage_spool_reconciles_unknown_and_partial_receipts_while_other_batches_advance() {
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone)]
    struct Peer {
        supported: Arc<AtomicBool>,
        probes: Arc<std::sync::atomic::AtomicUsize>,
        deliveries: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    async fn receive(
        State(peer): State<Peer>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, HeaderMap, Json<serde_json::Value>) {
        let mut reply = HeaderMap::new();
        let Some(batch_id) = headers.get(USAGE_BATCH_ID_HEADER) else {
            peer.probes.fetch_add(1, Ordering::Relaxed);
            if peer.supported.load(Ordering::Relaxed) {
                reply.insert("x-aisix-usage-batch-dedup", "1".parse().unwrap());
            }
            return (
                StatusCode::BAD_REQUEST,
                reply,
                Json(serde_json::json!({"code": "bad_request"})),
            );
        };
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let id = body["events"][0]["event_id"].as_str().unwrap();
        let attempts = {
            let mut log = peer.deliveries.lock().unwrap();
            log.push((id.into(), batch_id.to_str().unwrap().into()));
            log.iter().filter(|(event, _)| event == id).count()
        };
        if id != "legacy" {
            reply.insert("x-aisix-usage-batch-dedup", "1".parse().unwrap());
        }
        let (status, receipt) = match id {
            "legacy" => (
                StatusCode::OK,
                serde_json::json!({"accepted": 1, "rejected": 0, "duplicates": 0}),
            ),
            "malformed" => (
                StatusCode::OK,
                serde_json::json!({"accepted": 1, "rejected": 0}),
            ),
            "poison" => (
                StatusCode::BAD_REQUEST,
                serde_json::json!({"code": "bad_request"}),
            ),
            "partial" => (
                StatusCode::OK,
                serde_json::json!({"accepted": 1, "rejected": 1, "duplicates": 0}),
            ),
            "transient" if attempts == 1 => {
                (StatusCode::SERVICE_UNAVAILABLE, serde_json::json!({}))
            }
            _ => (
                StatusCode::OK,
                serde_json::json!({"accepted": 1, "rejected": 0, "duplicates": 0}),
            ),
        };
        (status, reply, Json(receipt))
    }

    let root = tempfile::tempdir().unwrap();
    write_test_bundle(root.path());
    let supported = Arc::new(AtomicBool::new(false));
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let deliveries = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{}",
        listener.local_addr().unwrap(),
        TELEMETRY_PATH
    );
    let router = Router::new()
        .route(TELEMETRY_PATH, post(receive))
        .with_state(Peer {
            supported: supported.clone(),
            probes: probes.clone(),
            deliveries: deliveries.clone(),
        });
    let peer = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let cfg = spool_test_config(root.path(), url);
    let (stop, cancel) = watch::channel(false);
    let metrics = Metrics::new(false);
    let queue = root.path().join("queue");
    {
        let mut journal = spool::Spool::open(&queue, &cfg.url, SPOOL_ENV, SPOOL_DP).unwrap();
        let deadline = chrono::Utc::now().timestamp_millis() + 60_000;
        for ids in [
            vec!["legacy"],
            vec!["malformed"],
            vec!["poison"],
            vec!["transient"],
            vec!["partial", "rejected-member"],
            vec!["healthy"],
        ] {
            let events = ids
                .into_iter()
                .map(|id| Prepared::new(spool_event(id), &metrics).unwrap())
                .collect::<Vec<_>>();
            journal
                .append(&batch_body(&events), events.len(), deadline)
                .unwrap();
        }
        let expired = [Prepared::new(spool_event("expired"), &metrics).unwrap()];
        journal.append(&batch_body(&expired), 1, 0).unwrap();
        journal.sync_new_entries().unwrap();
    }
    let (_sink, task) =
        spawn_spooled(cfg, metrics.clone(), cancel, &queue, SPOOL_ENV, SPOOL_DP).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while probes.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        deliveries.lock().unwrap().is_empty(),
        "legacy control plane must receive no metered events"
    );
    supported.store(true, Ordering::Relaxed);
    wait_until_spooled(&metrics, 0, 5).await;

    let retained = std::fs::read_dir(&queue)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "reconcile"))
        .map(|path| {
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();
    for id in ["legacy", "malformed", "poison", "partial", "expired"] {
        assert!(retained
            .iter()
            .any(|payload| payload["events"][0]["event_id"] == id));
    }
    assert!(retained
        .iter()
        .any(|payload| payload["events"][1]["event_id"] == "rejected-member"));
    let attempts = deliveries.lock().unwrap().clone();
    for id in ["legacy", "malformed", "poison", "partial", "healthy"] {
        assert_eq!(
            attempts
                .iter()
                .filter(|(received, _)| received == id)
                .count(),
            1
        );
    }
    let transient = attempts
        .iter()
        .filter(|(id, _)| id == "transient")
        .collect::<Vec<_>>();
    assert_eq!(transient.len(), 2);
    assert_eq!(transient[0].1, transient[1].1);
    let healthy = attempts.iter().position(|(id, _)| id == "healthy").unwrap();
    let second_transient = attempts
        .iter()
        .rposition(|(id, _)| id == "transient")
        .unwrap();
    assert!(
        healthy < second_transient,
        "the later healthy batch must advance during retry"
    );
    assert!(!attempts.iter().any(|(id, _)| id == "expired"));
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    peer.abort();
}
