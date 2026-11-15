//! JSON 入站解析必须消费完整正文；保留既有媒体类型、限额和字段错误契约。

use axum::extract::{FromRequest, Request};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Json<T>(pub T);

#[derive(Debug)]
pub(crate) enum JsonRejection {
    Axum(axum::extract::rejection::JsonRejection),
    InvalidJson,
}

impl From<axum::extract::rejection::JsonRejection> for JsonRejection {
    fn from(error: axum::extract::rejection::JsonRejection) -> Self {
        Self::Axum(error)
    }
}

impl IntoResponse for JsonRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Axum(error) => error.into_response(),
            Self::InvalidJson => {
                (StatusCode::BAD_REQUEST, "invalid JSON request body").into_response()
            }
        }
    }
}

impl<T: DeserializeOwned> Json<T> {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, JsonRejection> {
        match serde_json::from_slice(bytes) {
            Ok(value) => Ok(Self(value)),
            // 仅在错误路径重用框架的字段路径和拒绝类型；合法前缀后有垃圾时不能放行。
            Err(_) => match axum::Json::<T>::from_bytes(bytes) {
                Err(error) => Err(error.into()),
                Ok(_) => Err(JsonRejection::InvalidJson),
            },
        }
    }
}

#[async_trait::async_trait]
impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = JsonRejection;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !json_content_type(request.headers()) {
            // 让框架构造原有的媒体类型拒绝；此分支不会读取请求正文。
            let result = axum::Json::<T>::from_request(request, state)
                .await
                .map(|value| Self(value.0))
                .map_err(Into::into);
            crate::attribution::note_handler_started(std::time::Instant::now());
            return result;
        }
        let bytes = match Bytes::from_request(request, state).await {
            Ok(bytes) => bytes,
            Err(error) => {
                crate::attribution::note_handler_started(std::time::Instant::now());
                return Err(JsonRejection::Axum(error.into()));
            }
        };
        crate::attribution::measure_json_decode(true, || Self::from_bytes(&bytes))
    }
}

fn json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .is_some_and(|value| {
            value.type_() == "application"
                && (value.subtype() == "json"
                    || value.suffix().is_some_and(|suffix| suffix == "json"))
        })
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

impl<T> std::ops::Deref for Json<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
