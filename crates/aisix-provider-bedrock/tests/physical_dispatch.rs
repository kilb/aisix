//! 完整 SDK、静态假凭据和本机 TCP；不访问 AWS、默认凭据链或 STS。
use std::sync::Arc;
use std::time::Duration;

use aisix_gateway::{
    Bridge, BridgeContext, BridgeError, ChatFormat, ChatMessage, NativeMessages, ResponseHeaderTap,
};
use aisix_provider_bedrock::BedrockBridge;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Notify};

const MODEL: &str = "anthropic.claude-3-5-sonnet-20241022-v2:0";
const SECRET: &str = r#"{"auth_type":"access_key","access_key_id":"AKIA-local-only","secret_access_key":"local-only-fake-secret","region":"us-west-2"}"#;

fn context(base: &str, secret: &str) -> BridgeContext {
    let model = serde_json::from_value(json!({
        "display_name":"local-sdk", "provider":"bedrock", "model_name":MODEL,
        "provider_key_id":"11111111-1111-1111-1111-111111111111"
    }))
    .unwrap();
    let key = serde_json::from_value(json!({
        "display_name":"local-sdk-key", "api_key":secret, "api_base":base
    }))
    .unwrap();
    BridgeContext::new("local-sdk-dispatch", Arc::new(model), Arc::new(key))
}
fn input() -> Value {
    json!({"model":"client-name","max_tokens":20,"stream":false,
        "messages":[{"role":"user","content":"local transport check"}]})
}

async fn read_request_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        assert!(head.len() < 32768);
        socket.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

async fn respond_json(mut socket: tokio::net::TcpStream, status: &str, body: &str) {
    let head = read_request_head(&mut socket).await;
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    let mut request = vec![0; length];
    socket.read_exact(&mut request).await.unwrap();
    let wire = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(wire.as_bytes()).await.unwrap();
}

struct Provider {
    base: String,
    received: mpsc::Receiver<String>,
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
                    assert!(head.len() < 32768);
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
                    .expect("SDK Blob request preserves its known content length");
                assert!(length < 1048576);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    body.get("anthropic_version"),
                    Some(&json!("bedrock-2023-05-31"))
                );
                tx.send(head).await.unwrap();
                if index == 1 {
                    held.notified().await;
                    let _ = socket.shutdown().await;
                    break;
                }
                // 沿既有 SDK 成功响应 fixture；没有构造供应商异常。
                let body = br#"{"id":"local-sdk-ok","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}}"#;
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
    async fn next(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(10), self.received.recv())
            .await
            .expect("SDK transmits its signed HTTP request to localhost")
            .expect("local provider channel remains open")
    }
    async fn finish(mut self) {
        self.release.notify_one();
        self.task.take().unwrap().await.unwrap();
    }
}

#[tokio::test]
async fn sdk_before_transmit_marks_http_dispatch_before_response_and_survives_cancel() {
    let mut provider = Provider::start().await;
    assert!(provider.base.starts_with("http://127.0.0.1:"));
    let bridge = BedrockBridge::new();
    let healthy_tap = Arc::new(ResponseHeaderTap::default());
    let healthy = aisix_gateway::response_tap::scope(
        healthy_tap.clone(),
        bridge.anthropic_messages(input(), &context(&provider.base, SECRET)),
    )
    .await
    .unwrap();
    let NativeMessages::Json(healthy) = healthy else {
        panic!("nonstream SDK answer");
    };
    assert_eq!(healthy.get("id"), Some(&json!("local-sdk-ok")));
    let healthy_head = provider.next().await;
    assert!(healthy_head.starts_with("POST /model/anthropic."));
    assert!(healthy_head.contains("/invoke HTTP/1.1\r\n"));
    assert!(healthy_head
        .to_ascii_lowercase()
        .contains("authorization: aws4-hmac-sha256"));

    let tap = Arc::new(ResponseHeaderTap::default());
    assert!(
        !tap.http_dispatched(),
        "creating a tap does not imply HTTP dispatch"
    );
    let task_tap = tap.clone();
    let ctx = context(&provider.base, SECRET);
    let pending = tokio::spawn(async move {
        aisix_gateway::response_tap::scope(task_tap, bridge.anthropic_messages(input(), &ctx)).await
    });
    let head = provider.next().await;
    assert!(head.starts_with("POST /model/anthropic."));
    assert!(head.contains("/invoke HTTP/1.1\r\n"));
    assert!(head
        .to_ascii_lowercase()
        .contains("authorization: aws4-hmac-sha256"));
    assert!(
        !pending.is_finished(),
        "local provider is withholding every response byte"
    );
    pending.abort();
    let cancelled = match pending.await {
        Err(error) => error,
        Ok(_) => panic!("held SDK operation must be cancelled"),
    };
    assert!(cancelled.is_cancelled());
    assert!(
        tap.capture().io.is_some(),
        "real SDK before_transmit observation ran"
    );
    assert!(
        tap.http_dispatched(),
        "a signed request received by localhost is physically dispatched before headers"
    );
    assert!(
        tap.capture().http_dispatched,
        "capture preserves the exact physical fact after cancellation"
    );
    assert!(
        healthy_tap.http_dispatched(),
        "completed SDK exchange records the same dispatch fact"
    );
    assert!(
        provider.received.try_recv().is_err(),
        "healthy and cancelled operations transmit exactly twice with no SDK retry"
    );
    provider.finish().await;
}

#[tokio::test]
async fn sdk_static_credential_preflight_error_never_claims_http_dispatch() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    for secret in [
        "not valid credential JSON",
        r#"{"auth_type":"access_key","access_key_id":"AKIA-local-only","region":"us-west-2"}"#,
    ] {
        let tap = Arc::new(ResponseHeaderTap::default());
        let result = aisix_gateway::response_tap::scope(
            tap.clone(),
            BedrockBridge::new().anthropic_messages(input(), &context(&base, secret)),
        )
        .await;
        assert!(
            result.is_err(),
            "invalid static credentials are refused before SDK transmission"
        );
        assert!(!tap.http_dispatched());
        assert!(!tap.capture().http_dispatched);
        assert!(
            tap.capture().io.is_none(),
            "preflight failure must not manufacture a before_transmit exchange"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "an already rejected preflight cannot contact even the local runtime endpoint"
        );
    }
}

#[tokio::test]
async fn openai_compatible_buffered_deadline_distinguishes_success_head() {
    buffered_deadline("us.xai.grok-4", "/openai/v1/chat/completions", false).await;
}

#[tokio::test]
async fn sdk_messages_buffered_deadline_distinguishes_success_head() {
    buffered_deadline(MODEL, "/invoke", true).await;
}

#[tokio::test]
async fn sdk_chat_invoke_buffered_deadline_distinguishes_success_head() {
    buffered_deadline(MODEL, "/invoke", false).await;
}

#[tokio::test]
async fn sdk_chat_converse_buffered_deadline_distinguishes_success_head() {
    buffered_deadline("amazon.nova-lite-v1:0", "/converse", false).await;
}

#[tokio::test]
async fn sdk_count_tokens_buffered_deadline_distinguishes_success_head() {
    buffered_deadline(MODEL, "/count-tokens", true).await;
}

#[tokio::test]
async fn sdk_embed_buffered_deadline_distinguishes_success_head() {
    buffered_deadline("amazon.titan-embed-text-v2:0", "/invoke", false).await;
    buffered_deadline("cohere.embed-english-v3", "/invoke", false).await;
}

#[tokio::test]
async fn sdk_titan_success_does_not_classify_the_next_operation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (invoked, mut invocations) = mpsc::channel(1);
    let (release, held) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        respond_json(
            socket,
            "200 OK",
            r#"{"embedding":[1.0],"inputTextTokenCount":3}"#,
        )
        .await;
        let (mut second, _) = listener.accept().await.unwrap();
        invoked
            .send(read_request_head(&mut second).await)
            .await
            .unwrap();
        held.await.unwrap();
    });
    let mut ctx = context(&base, SECRET).with_deadline(Duration::from_millis(500));
    Arc::make_mut(&mut ctx.model).model_name = Some("amazon.titan-embed-text-v2:0".into());
    let req = serde_json::from_value(json!({"model":"local","input":["one","two"]})).unwrap();
    let result = BedrockBridge::new().embed(&req, &ctx).await;
    assert!(
        matches!(result, Err(BridgeError::Timeout { .. })),
        "{result:?}"
    );
    assert!(invocations.try_recv().unwrap().contains("/invoke HTTP/1.1"));
    release.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn mantle_count_tokens_classifies_a_success_head_after_runtime_rejection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    aisix_provider_bedrock::init_mantle_endpoint(Some(base.clone()));
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        respond_json(
            socket,
            "400 Bad Request",
            r#"{"message":"The provided model does not support counting tokens."}"#,
        )
        .await;
        let (mut mantle, _) = listener.accept().await.unwrap();
        assert!(read_request_head(&mut mantle)
            .await
            .starts_with("POST /anthropic/v1/messages/count_tokens HTTP/1.1\r\n"));
        mantle.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\nx-amzn-requestid: local-mantle-accepted\r\n\r\n{").await.unwrap();
        std::future::pending::<()>().await;
    });
    let tap = Arc::new(ResponseHeaderTap::default());
    let mut ctx = context(&base, SECRET).with_deadline(Duration::from_millis(500));
    Arc::make_mut(&mut ctx.model).model_name = Some("anthropic.claude-opus-5-5".into());
    let bridge = BedrockBridge::new();
    let result = aisix_gateway::response_tap::scope(
        tap.clone(),
        bridge.anthropic_count_tokens(input(), &ctx),
    )
    .await;
    assert_eq!(tap.capture().request_id, "local-mantle-accepted");
    assert!(
        matches!(
            result,
            Err(BridgeError::UpstreamBodyUnread { status: 200, .. })
        ),
        "{result:?}"
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

async fn buffered_deadline(model_name: &str, path: &'static str, native_messages: bool) {
    for accepted in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_request_head(&mut socket).await;
            let request_line = head.lines().next().unwrap();
            assert!(request_line.starts_with("POST "));
            assert!(
                request_line.ends_with(&format!("{path} HTTP/1.1")),
                "{request_line}"
            );
            assert!(head
                .to_ascii_lowercase()
                .contains("authorization: aws4-hmac-sha256"));
            if accepted {
                // 先交付成功响应头和第一个字节，继续扣住剩余正文直到预算耗尽。
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nx-amzn-requestid: local-accepted\r\n\r\n{")
                    .await
                    .unwrap();
            }
            std::future::pending::<()>().await;
        });
        let tap = Arc::new(ResponseHeaderTap::default());
        let mut ctx = context(&base, SECRET);
        Arc::make_mut(&mut ctx.model).model_name = Some(model_name.into());
        ctx.deadline = Some(Duration::from_millis(500));
        let bridge = BedrockBridge::new();
        let result = aisix_gateway::response_tap::scope(tap.clone(), async {
            if path == "/count-tokens" {
                bridge
                    .anthropic_count_tokens(input(), &ctx)
                    .await
                    .map(|_| ())
            } else if native_messages {
                bridge.anthropic_messages(input(), &ctx).await.map(|_| ())
            } else if model_name.contains("embed") {
                let req = serde_json::from_value(json!({"model":"local","input":["one"]})).unwrap();
                bridge.embed(&req, &ctx).await.map(|_| ())
            } else {
                let req =
                    ChatFormat::new("local", vec![ChatMessage::user("local transport check")]);
                bridge.chat(&req, &ctx).await.map(|_| ())
            }
        })
        .await;
        assert!(tap.http_dispatched());
        if accepted {
            assert_eq!(tap.capture().request_id, "local-accepted");
            assert!(
                matches!(
                    result,
                    Err(BridgeError::UpstreamBodyUnread { status: 200, .. })
                ),
                "{result:?}"
            );
        } else {
            assert!(tap.capture().request_id.is_empty());
            assert!(
                matches!(result, Err(BridgeError::Timeout { .. })),
                "{result:?}"
            );
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
