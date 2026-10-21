//! 处理链提前作答、请求体还没读完时，把剩下的请求体读掉再让连接收尾。
//!
//! 鉴权失败、限额拒绝这类响应在读请求体之前就做出了。hyper 对一个没被读完
//! 就丢掉的请求体，只顺手读一下已经到达的字节，读不完就关闭连接；而调用方此刻
//! 往往还在写它的请求体，写一个对端已关的连接得到的是 `EPIPE` / 连接重置 ——
//! 它永远看不到那个 401，SDK 会把它当成网络故障重试。
//!
//! 主流做法都是先作答、再把剩余请求体读掉：nginx 的 `lingering_close`、Envoy
//! 的 `delayed_close_timeout`、Go `net/http` 在 handler 返回后丢弃未读请求体。
//! 这里同样：响应立即返回，被丢下的请求体交给一个后台任务读完（有字节与时长
//! 上限），读完之前 hyper 不会关连接。

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, Version};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use http_body::{Frame, SizeHint};
use tokio::sync::oneshot;

/// 交还后最多再读这么多字节。超出就放弃，连接照旧关闭 —— 一个被拒绝的大上传
/// 不值得整个收下。
const DRAIN_MAX_BYTES: usize = 1024 * 1024;
/// 读剩余请求体的总时长上限，慢速客户端不能借此长期占住连接。
const DRAIN_DEADLINE: Duration = Duration::from_secs(5);

pub(crate) async fn drain_unread_body(request: Request, next: Next) -> Response {
    // 只有 HTTP/1 会这样断。`Expect: 100-continue` 的调用方在等答复，本来就不会
    // 发请求体；去读它反而会触发 100 Continue。
    let expects_continue = request
        .headers()
        .get(header::EXPECT)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"));
    if request.version() >= Version::HTTP_2
        || expects_continue
        || http_body::Body::is_end_stream(request.body())
    {
        return next.run(request).await;
    }

    let (parts, body) = request.into_parts();
    let (unread_tx, mut unread_rx) = oneshot::channel();
    let body = Body::new(ReturnUnread {
        inner: Some(body),
        unread: Some(unread_tx),
    });
    let response = next.run(Request::from_parts(parts, body)).await;
    // 处理链已经把请求体丢下、又没读完：后台读掉。请求体若还被别处持有（比如
    // 正被流式转发给上游），这里收不到，也不该等。
    if let Ok(body) = unread_rx.try_recv() {
        tokio::spawn(drain(body));
    }
    response
}

async fn drain(body: Body) {
    let mut chunks = body.into_data_stream();
    let _ = tokio::time::timeout(DRAIN_DEADLINE, async {
        let mut read = 0usize;
        while let Some(Ok(chunk)) = chunks.next().await {
            read += chunk.len();
            if read > DRAIN_MAX_BYTES {
                break;
            }
        }
    })
    .await;
}

/// 处理链拿到的请求体：透传真实请求体；没读完就被丢下时，把它交还给
/// [`drain_unread_body`]。
struct ReturnUnread {
    inner: Option<Body>,
    unread: Option<oneshot::Sender<Body>>,
}

impl http_body::Body for ReturnUnread {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = Pin::new(inner).poll_frame(cx);
        // 读到头或出错：没有可交还的了。
        if matches!(polled, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            this.inner = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(http_body::Body::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        self.inner
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), http_body::Body::size_hint)
    }
}

impl Drop for ReturnUnread {
    fn drop(&mut self) {
        if let (Some(body), Some(unread)) = (self.inner.take(), self.unread.take()) {
            if !http_body::Body::is_end_stream(&body) {
                let _ = unread.send(body);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // 这两个上限是被拒绝的请求还能占住连接多久的唯一约束。

    #[tokio::test(start_paused = true)]
    async fn drain_gives_up_on_a_body_that_never_ends() {
        let started = tokio::time::Instant::now();
        drain(Body::from_stream(futures::stream::pending::<
            Result<Bytes, std::io::Error>,
        >()))
        .await;
        assert!(started.elapsed() <= DRAIN_DEADLINE + Duration::from_millis(10));
    }

    #[tokio::test]
    async fn drain_stops_after_the_byte_cap() {
        const CHUNK: usize = 64 * 1024;
        let pulled = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&pulled);
        let endless = futures::stream::repeat_with(move || {
            counter.fetch_add(CHUNK, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from(vec![0u8; CHUNK]))
        });
        drain(Body::from_stream(endless)).await;
        assert!(pulled.load(Ordering::SeqCst) <= DRAIN_MAX_BYTES + CHUNK);
    }
}
