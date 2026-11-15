//! 真实决策路由与本机 TCP；只证明发送/取消归因，不冒称真实提供商端到端。
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Notify};
use tower::ServiceExt;

struct Provider {
    base: String,
    received: mpsc::Receiver<(String, Vec<u8>)>,
    release: Arc<Notify>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.release.notify_one();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
impl Provider {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, received) = mpsc::channel(4);
        let release = Arc::new(Notify::new());
        let held = release.clone();
        let task = tokio::spawn(async move {
            for index in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 32768, "bounded local HTTP request head");
                    socket.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .expect("bytes request has an explicit HTTP content length");
                assert!(length < 1048576);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                tx.send((head, body)).await.unwrap();
                if index == 1 {
                    held.notified().await;
                    let _ = socket.shutdown().await;
                    break;
                }
                // 沿现有 Decisions 成功 envelope；没有构造供应商失败形状。
                let body = br#"{"model":"dec-1","answers":[{"type":"predicate","name":"ok","probability":0.9}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#;
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        Self {
            base,
            received,
            release,
            task: Some(task),
        }
    }
    async fn next(&mut self) -> (String, Value) {
        let (head, body) = tokio::time::timeout(Duration::from_secs(10), self.received.recv())
            .await
            .expect("local provider receives the real POST")
            .expect("provider receive channel remains open");
        (head, serde_json::from_slice(&body).unwrap())
    }
    async fn finish(mut self) {
        self.release.notify_one();
        self.task.take().unwrap().await.unwrap();
    }
}

fn gateway(base: &str) -> (ProxyState, mpsc::Receiver<UsageEvent>) {
    let snapshot = AisixSnapshot::new();
    let pk: ProviderKey = serde_json::from_value(json!({
        "display_name":"local-decisions", "provider":"openai", "adapter":"openai",
        "secret":"local-only-upstream-key", "api_base":format!("{base}/v1")
    }))
    .unwrap();
    snapshot
        .provider_keys
        .insert(ResourceEntry::new("pk-1", pk, 1));
    let model: Model = serde_json::from_value(json!({
        "display_name":"judge", "provider":"openai", "model_name":"dec-1", "provider_key_id":"pk-1"
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new("m-1", model, 1));
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("local-caller"), "allowed_models":["judge"]
    }))
    .unwrap();
    snapshot.apikeys.insert(ResourceEntry::new("k-1", key, 1));
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .unwrap();
    let (tx, events) = mpsc::channel(16);
    (
        ProxyState::new(SnapshotHandle::new(snapshot), Arc::new(Hub::new()), &config)
            .without_cache()
            .with_usage_sink(UsageSink::new(tx)),
        events,
    )
}

fn request(endpoint: &str, bearer: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(endpoint)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn body(endpoint: &str, text: &str) -> Value {
    if endpoint == "/v1/systemone" {
        json!({"model":"judge","state":text,"questions":{"ok":{"type":"noul","instructions":"Is this acceptable?"}}})
    } else {
        json!({"model":"judge","input":text,"questions":[{"type":"predicate","name":"ok","instructions":"Is this acceptable?"}]})
    }
}
async fn event(events: &mut mpsc::Receiver<UsageEvent>) -> UsageEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("actual decision UsageEvent is emitted")
        .expect("usage sink remains open")
}

// 两个公开别名最终共用真实发送；响应头尚未到达时取消也必须保留发送事实。
#[tokio::test]
async fn decisions_and_systemone_real_post_cancel_is_dispatched_with_no_send_controls() {
    for endpoint in ["/v1/decisions", "/v1/systemone"] {
        let mut provider = Provider::start().await;
        let (state, mut events) = gateway(&provider.base);
        let app = build_router(state);

        let denied = app
            .clone()
            .oneshot(request(
                endpoint,
                "wrong-caller",
                body(endpoint, "not sent"),
            ))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        to_bytes(denied.into_body(), 1048576).await.unwrap();
        assert!(
            provider.received.try_recv().is_err(),
            "authentication refusal sends no HTTP"
        );
        // 未鉴权请求可能没有 UsageEvent；这里只核真实 HTTP，无虚构身份或默认 false。
        while events.try_recv().is_ok() {}

        let mut no_model = body(endpoint, "not sent");
        no_model.as_object_mut().unwrap().remove("model");
        let rejected = app
            .clone()
            .oneshot(request(endpoint, "local-caller", no_model))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        to_bytes(rejected.into_body(), 1048576).await.unwrap();
        let rejected = event(&mut events).await;
        assert_eq!(rejected.status_code, 400);
        assert_eq!(
            rejected.upstream_dispatched,
            Some(false),
            "authenticated pre-dispatch rejection explicitly reports false"
        );
        assert!(provider.received.try_recv().is_err());
        assert!(events.try_recv().is_err());

        let healthy = app
            .clone()
            .oneshot(request(endpoint, "local-caller", body(endpoint, "healthy")))
            .await
            .unwrap();
        assert_eq!(healthy.status(), StatusCode::OK);
        let answer: Value =
            serde_json::from_slice(&to_bytes(healthy.into_body(), 1048576).await.unwrap()).unwrap();
        assert_eq!(answer.get("model"), Some(&json!("judge")));
        let (head, sent) = provider.next().await;
        assert!(head.starts_with("POST /v1/decisions HTTP/1.1\r\n"));
        assert_eq!(sent.get("model"), Some(&json!("dec-1")));
        assert_eq!(sent.get("input"), Some(&json!("healthy")));
        let healthy = event(&mut events).await;
        assert_eq!(healthy.status_code, 200);
        assert_eq!(healthy.upstream_dispatched, Some(true));
        assert!(events.try_recv().is_err());

        let pending = tokio::spawn(async move {
            app.oneshot(request(endpoint, "local-caller", body(endpoint, "held")))
                .await
        });
        let (head, sent) = provider.next().await;
        assert!(head.starts_with("POST /v1/decisions HTTP/1.1\r\n"));
        assert_eq!(sent.get("input"), Some(&json!("held")));
        assert!(!pending.is_finished(), "provider has not sent a response");
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        let cancelled = event(&mut events).await;
        assert_eq!(cancelled.status_code, 499);
        assert_eq!(cancelled.requested_model, "judge");
        assert_eq!(
            cancelled.upstream_dispatched,
            Some(true),
            "a received POST remains dispatched before any response header"
        );
        assert_eq!(
            (cancelled.prompt_tokens, cancelled.completion_tokens),
            (0, 0)
        );
        assert!(
            events.try_recv().is_err(),
            "one cancelled request emits one UsageEvent"
        );
        provider.finish().await;
    }
}
