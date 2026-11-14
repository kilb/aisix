use std::io;

use async_compression::codecs::zstd::params::DParameter;
use async_compression::tokio::bufread::ZstdDecoder;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use futures::TryStreamExt;
use http_body::Frame;
use http_body_util::{BodyExt, LengthLimitError, Limited, StreamBody};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::state::ProxyState;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// 限制单帧解码窗口，不改变零配置表示请求总长度无限制的约定。
const MAX_ZSTD_WINDOW_LOG: u32 = 27;

pub(crate) async fn decode_request_body(
    State(state): State<ProxyState>,
    mut request: Request,
    next: Next,
) -> Response {
    if !is_zstd_encoded(request.headers()) {
        return next.run(request).await;
    }
    request.headers_mut().remove(CONTENT_ENCODING);
    request.headers_mut().remove(CONTENT_LENGTH);
    let limit_bytes = state.request_body_limit_for(request.uri().path());
    let (parts, body) = request.into_parts();
    let body = decode_zstd_body(body, limit_bytes);
    next.run(Request::from_parts(parts, body)).await
}

fn is_zstd_encoded(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_ENCODING).iter();
    let Some(value) = values.next() else {
        return false;
    };
    values.next().is_none()
        && value
            .to_str()
            .is_ok_and(|value| value.trim().eq_ignore_ascii_case("zstd"))
}

fn decode_zstd_body(body: Body, limit_bytes: usize) -> Body {
    let stream = crate::request_id::in_request_span(async_stream::stream! {
        // 首次 poll 才读取；认证或资源检查提前拒绝时不触碰原始请求体。
        let input = TryStreamExt::map_err(body.into_data_stream(), io::Error::other);
        let mut decoder = ZstdDecoder::with_params(
            StreamReader::new(input),
            &[DParameter::window_log_max(MAX_ZSTD_WINDOW_LOG)],
        );
        decoder.multiple_members(true);
        let frames = ReaderStream::new(decoder)
            .map_ok(Frame::data)
            .map_err(normalize_read_error);
        let decoded = StreamBody::new(frames);
        // 在收集过程中执行限额；必须校验全部帧及 EOF 后才能暴露任何字段。
        let result: Result<Bytes, BoxError> = if limit_bytes > 0 {
            Limited::new(decoded, limit_bytes)
                .collect()
                .await
                .map(|body| body.to_bytes())
        } else {
            decoded.collect().await.map(|body| body.to_bytes())
        };
        yield result;
    });
    // Body::from_stream 会叠加两层 axum::Error，隐藏 multipart 只检查一层的限额错误。
    Body::new(StreamBody::new(stream.map_ok(Frame::data)))
}

// 取回原有限额错误，保留其具体类型以便 JSON 和 multipart 仍将其识别为 413。
fn normalize_read_error(error: io::Error) -> BoxError {
    let error = match error.downcast::<LengthLimitError>() {
        Ok(error) => return Box::new(error),
        Err(error) => error,
    };
    let error = match error.downcast::<axum::Error>() {
        Ok(error) => return normalize_boxed_error(error.into_inner()),
        Err(error) => error,
    };
    match error.downcast::<io::Error>() {
        Ok(error) => normalize_read_error(error),
        Err(error) => Box::new(error),
    }
}

fn normalize_boxed_error(error: BoxError) -> BoxError {
    let error = match error.downcast::<axum::Error>() {
        Ok(error) => return normalize_boxed_error(error.into_inner()),
        Err(error) => error,
    };
    match error.downcast::<io::Error>() {
        Ok(error) => normalize_read_error(*error),
        Err(error) => error,
    }
}
