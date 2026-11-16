use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_core::usage_io::UpstreamTiming;
use aisix_gateway::response_tap::{self, HttpTimingObserver, ResponseHeaderTap};
use aisix_gateway::upstream_io::{self, Protocol};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const GAP: Duration = Duration::from_millis(60);
const PARTIAL: &[u8] = b"data: {\"choices\":[";
const REST: &[u8] = b"{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n";

struct Server {
    url: String,
    body: mpsc::UnboundedSender<Vec<u8>>,
    task: JoinHandle<io::Result<()>>,
}

// 正文分片由测试逐次放行，避免调度快慢把多个阶段合成一次读取。
async fn server(headers: Option<String>, header_delay: Duration) -> TestResult<Server> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (body, mut chunks) = mpsc::unbounded_channel::<Vec<u8>>();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.set_nodelay(true)?;
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).await?;
            if count == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request head"));
            }
            request.extend_from_slice(&buffer[..count]);
        }
        if let Some(headers) = headers {
            tokio::time::sleep(header_delay).await;
            socket.write_all(headers.as_bytes()).await?;
        }
        while let Some(chunk) = chunks.recv().await {
            socket.write_all(&chunk).await?;
        }
        socket.shutdown().await
    });
    Ok(Server {
        url: format!("http://{address}/v1/chat/completions"),
        body,
        task,
    })
}

fn headers(length: usize) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    )
}

fn client() -> TestResult<reqwest::Client> {
    Ok(reqwest::Client::builder().no_proxy().build()?)
}

fn tap() -> Arc<ResponseHeaderTap> {
    Arc::new(ResponseHeaderTap::new(
        Vec::new(),
        false,
        "timing-test".into(),
    ))
}

fn timing(tap: &ResponseHeaderTap) -> TestResult<HttpTimingObserver> {
    tap.capture()
        .timing
        .ok_or_else(|| "missing HTTP timing observer".into())
}

fn snapshot(observer: &HttpTimingObserver) -> TestResult<UpstreamTiming> {
    observer
        .snapshot()
        .ok_or_else(|| "missing single-exchange timing".into())
}

fn measured(value: Option<u32>, phase: &str) -> TestResult<u32> {
    value.ok_or_else(|| format!("missing {phase}").into())
}

// TCP 不保证一次 write 对应一次 body chunk；在下一片未放行时读完当前片。
async fn read_part(response: &mut reqwest::Response, length: usize) -> TestResult<Vec<u8>> {
    let mut received = Vec::new();
    while received.len() < length {
        let chunk = response
            .chunk()
            .await?
            .ok_or("body ended before released part")?;
        received.extend_from_slice(&chunk);
    }
    Ok(received)
}

// 若把发送起点移回 attempt，或把 headers/byte/frame/canonical/content 合并，此例必须失败。
#[tokio::test]
async fn independent_http_phases_follow_real_delivery_and_keep_the_first_sample() -> TestResult {
    let server = server(Some(headers(PARTIAL.len() + REST.len())), GAP).await?;
    let tap = tap();
    tap.begin_attempt(Instant::now());
    tokio::time::sleep(GAP).await;
    let mut response = response_tap::scope(Arc::clone(&tap), async {
        let response = upstream_io::send(client()?.get(&server.url), Protocol::Chat).await?;
        TestResult::Ok(response)
    })
    .await?;
    let observer = timing(&tap)?;
    let at_headers = snapshot(&observer)?;
    let send = measured(at_headers.http_send_started_ms, "send")?;
    let head = measured(at_headers.response_headers_ms, "headers")?;
    assert!(send >= 50, "send must exclude the deliberate pre-send wait");
    assert!(
        head >= send + 50,
        "headers must include the server's head delay"
    );
    assert_eq!(at_headers.first_response_byte_ms, None);
    assert_eq!(at_headers.first_protocol_frame_ms, None);
    assert_eq!(at_headers.first_canonical_chunk_ms, None);
    assert_eq!(at_headers.first_content_ms, None);

    tokio::time::sleep(GAP).await;
    server.body.send(PARTIAL.to_vec())?;
    assert_eq!(read_part(&mut response, PARTIAL.len()).await?, PARTIAL);
    let partial = snapshot(&observer)?;
    let byte = measured(partial.first_response_byte_ms, "first byte")?;
    assert!(byte >= head + 50);
    assert_eq!(
        partial.first_protocol_frame_ms, None,
        "an incomplete frame is not a frame"
    );

    tokio::time::sleep(GAP).await;
    server.body.send(REST.to_vec())?;
    assert_eq!(read_part(&mut response, REST.len()).await?, REST);
    let framed = snapshot(&observer)?;
    let frame = measured(framed.first_protocol_frame_ms, "protocol frame")?;
    assert!(frame >= byte + 50);
    assert_eq!(framed.first_canonical_chunk_ms, None);
    assert_eq!(framed.first_content_ms, None);

    tokio::time::sleep(GAP).await;
    observer.note_canonical_chunk();
    let canonical = snapshot(&observer)?;
    let chunk = measured(canonical.first_canonical_chunk_ms, "canonical chunk")?;
    assert!(chunk >= frame + 50);
    assert_eq!(canonical.first_content_ms, None);

    tokio::time::sleep(GAP).await;
    observer.note_first_content();
    let content = snapshot(&observer)?;
    assert!(measured(content.first_content_ms, "content")? >= chunk + 50);
    tokio::time::sleep(GAP).await;
    observer.note_canonical_chunk();
    observer.note_first_content();
    assert_eq!(
        snapshot(&observer)?,
        content,
        "later samples must not replace first samples"
    );
    assert!(response.chunk().await?.is_none());
    drop(server.body);
    server.task.await??;
    Ok(())
}

// 若把 EOF 当完整协议帧，断开的 data 事件会错误获得首帧时刻。
#[tokio::test]
async fn unterminated_protocol_frame_keeps_raw_frame_unknown() -> TestResult {
    let server = server(Some(headers(PARTIAL.len())), Duration::ZERO).await?;
    let tap = tap();
    tap.begin_attempt(Instant::now());
    let response = response_tap::scope(Arc::clone(&tap), async {
        let response = upstream_io::send(client()?.get(&server.url), Protocol::Chat).await?;
        TestResult::Ok(response)
    })
    .await?;
    let observer = timing(&tap)?;
    server.body.send(PARTIAL.to_vec())?;
    assert_eq!(response.bytes().await?.as_ref(), PARTIAL);
    let timing = snapshot(&observer)?;
    assert!(timing.http_send_started_ms.is_some());
    assert!(timing.response_headers_ms.is_some());
    assert!(timing.first_response_byte_ms.is_some());
    assert_eq!(timing.first_protocol_frame_ms, None);
    assert_eq!(timing.first_canonical_chunk_ms, None);
    assert_eq!(timing.first_content_ms, None);
    drop(server.body);
    server.task.await??;
    Ok(())
}

// 空响应只有 head；若用首次 poll 或 EOF 冒充字节，这里会变红。
#[tokio::test]
async fn empty_body_never_manufactures_a_first_byte_or_frame() -> TestResult {
    let server = server(Some(headers(0)), Duration::ZERO).await?;
    let tap = tap();
    tap.begin_attempt(Instant::now());
    let response = response_tap::scope(Arc::clone(&tap), async {
        let response = upstream_io::send(client()?.get(&server.url), Protocol::Chat).await?;
        TestResult::Ok(response)
    })
    .await?;
    let observer = timing(&tap)?;
    assert!(response.bytes().await?.is_empty());
    let timing = snapshot(&observer)?;
    assert!(timing.http_send_started_ms.is_some());
    assert!(timing.response_headers_ms.is_some());
    assert_eq!(timing.first_response_byte_ms, None);
    assert_eq!(timing.first_protocol_frame_ms, None);
    assert_eq!(timing.first_canonical_chunk_ms, None);
    assert_eq!(timing.first_content_ms, None);
    drop(server.body);
    server.task.await??;
    Ok(())
}

// 请求取消前未收到 head；缺失阶段不能被补为 0 或取消时刻。
#[tokio::test]
async fn cancellation_before_headers_keeps_unobserved_stages_unknown() -> TestResult {
    let server = server(None, Duration::ZERO).await?;
    let tap = tap();
    tap.begin_attempt(Instant::now());
    response_tap::scope(Arc::clone(&tap), async {
        let request = upstream_io::send(client()?.get(&server.url), Protocol::Chat);
        assert!(tokio::time::timeout(GAP, request).await.is_err());
        let timing = snapshot(&timing(&tap)?)?;
        assert!(timing.http_send_started_ms.is_some());
        assert_eq!(timing.response_headers_ms, None);
        assert_eq!(timing.first_response_byte_ms, None);
        assert_eq!(timing.first_protocol_frame_ms, None);
        assert_eq!(timing.first_canonical_chunk_ms, None);
        assert_eq!(timing.first_content_ms, None);
        TestResult::Ok(())
    })
    .await?;
    drop(server.body);
    server.task.await??;
    Ok(())
}

// take 不结束真实响应；旧 body 的更新必须继续写旧 attempt，而不能串进新 attempt。
#[tokio::test]
async fn taken_response_timing_stays_live_and_isolated_from_the_next_attempt() -> TestResult {
    let first = server(Some(headers(PARTIAL.len() + REST.len())), Duration::ZERO).await?;
    let second = server(Some(headers(PARTIAL.len() + REST.len())), Duration::ZERO).await?;
    let tap = tap();
    let (old_response, old, new_response, new) = response_tap::scope(Arc::clone(&tap), async {
        let client = client()?;
        tap.begin_attempt(Instant::now());
        let old_response = upstream_io::send(client.get(&first.url), Protocol::Chat).await?;
        let old = tap.take().timing.ok_or("missing taken timing")?;
        tap.begin_attempt(Instant::now());
        let new_response = upstream_io::send(client.get(&second.url), Protocol::Chat).await?;
        let new = timing(&tap)?;
        TestResult::Ok((old_response, old, new_response, new))
    })
    .await?;
    assert_eq!(snapshot(&old)?.first_response_byte_ms, None);
    assert_eq!(snapshot(&new)?.first_response_byte_ms, None);

    first.body.send([PARTIAL, REST].concat())?;
    assert_eq!(
        old_response.bytes().await?.as_ref(),
        [PARTIAL, REST].concat()
    );
    old.note_canonical_chunk();
    old.note_first_content();
    let old_finished = snapshot(&old)?;
    assert!(old_finished.first_protocol_frame_ms.is_some());
    assert!(old_finished.first_canonical_chunk_ms.is_some());
    assert!(old_finished.first_content_ms.is_some());
    assert_eq!(snapshot(&new)?.first_response_byte_ms, None);
    assert_eq!(snapshot(&new)?.first_protocol_frame_ms, None);
    assert_eq!(snapshot(&new)?.first_canonical_chunk_ms, None);
    assert_eq!(snapshot(&new)?.first_content_ms, None);

    second.body.send([PARTIAL, REST].concat())?;
    assert_eq!(
        new_response.bytes().await?.as_ref(),
        [PARTIAL, REST].concat()
    );
    new.note_canonical_chunk();
    new.note_first_content();
    assert!(snapshot(&new)?.first_protocol_frame_ms.is_some());
    assert_eq!(snapshot(&old)?, old_finished);
    drop(first.body);
    drop(second.body);
    first.task.await??;
    second.task.await??;
    Ok(())
}

// 一个 attempt 内发生两次 HTTP 交换时，拼成一组阶段会制造不存在的单次交换。
#[tokio::test]
async fn multiple_http_exchanges_make_attempt_timing_unavailable() -> TestResult {
    let first = server(Some(headers(0)), Duration::ZERO).await?;
    let second = server(Some(headers(0)), Duration::ZERO).await?;
    let tap = tap();
    tap.begin_attempt(Instant::now());
    response_tap::scope(Arc::clone(&tap), async {
        let client = client()?;
        let response = upstream_io::send(client.get(&first.url), Protocol::Chat).await?;
        assert!(response.bytes().await?.is_empty());
        let old = timing(&tap)?;
        assert!(old.snapshot().is_some());
        let response = upstream_io::send(client.get(&second.url), Protocol::Chat).await?;
        assert!(response.bytes().await?.is_empty());
        assert!(tap
            .capture()
            .timing
            .and_then(|timing| timing.snapshot())
            .is_none());
        assert!(
            old.snapshot().is_none(),
            "existing handles must not keep reporting a single exchange"
        );
        TestResult::Ok(())
    })
    .await?;
    drop(first.body);
    drop(second.body);
    first.task.await??;
    second.task.await??;
    Ok(())
}

// 真实测得的 0 不能在序列化时退化成缺失；断言必须同时检查键的存在和值。
#[test]
fn measured_zero_survives_json_while_unknown_stages_remain_optional() -> TestResult {
    let timing = UpstreamTiming {
        http_send_started_ms: Some(0),
        response_headers_ms: None,
        first_response_byte_ms: None,
        first_protocol_frame_ms: None,
        first_canonical_chunk_ms: None,
        first_content_ms: None,
    };
    let wire = serde_json::to_value(timing)?;
    assert!(wire.get("http_send_started_ms").is_some());
    assert_eq!(
        wire.get("http_send_started_ms")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );
    let decoded: UpstreamTiming = serde_json::from_value(wire)?;
    assert_eq!(decoded.http_send_started_ms, Some(0));
    assert_eq!(decoded.response_headers_ms, None);
    assert_eq!(decoded.first_response_byte_ms, None);
    assert_eq!(decoded.first_protocol_frame_ms, None);
    assert_eq!(decoded.first_canonical_chunk_ms, None);
    assert_eq!(decoded.first_content_ms, None);
    Ok(())
}
