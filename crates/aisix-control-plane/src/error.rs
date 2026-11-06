//! The errors the control plane shows: the HTTP API's error envelope, and
//! the one-line reasons a boot is refused.

use std::fmt::Display;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use uuid::Uuid;

use crate::db::DbError;

/// One problem in a request, at a JSON path of its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Detail {
    pub path: String,
    pub message: String,
}

/// A resource that still references the one a request tried to delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Referrer {
    pub kind: String,
    pub id: Uuid,
    pub name: Option<String>,
}

/// An error answer of the HTTP API:
/// `{"error": "<message>", "code": "<code>", "details"?, "referenced_by"?, "current_version"?}`.
///
/// `message` is shown to users and is in Chinese; `code` is for programs.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub details: Vec<Detail>,
    pub referenced_by: Vec<Referrer>,
    pub current_version: Option<i64>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
            details: Vec::new(),
            referenced_by: Vec::new(),
            current_version: None,
        }
    }

    pub fn unauthenticated() -> ApiError {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "未登录或登录已过期",
        )
    }

    pub fn invalid_credentials() -> ApiError {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "邮箱或密码错误",
        )
    }

    pub fn forbidden() -> ApiError {
        Self::new(StatusCode::FORBIDDEN, "forbidden", "没有执行此操作的权限")
    }

    pub fn not_found() -> ApiError {
        Self::new(StatusCode::NOT_FOUND, "not_found", "资源不存在")
    }

    pub fn validation(details: Vec<Detail>) -> ApiError {
        ApiError {
            details,
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                "请求内容不合法",
            )
        }
    }

    /// A 409 with its own code, such as `name_taken`.
    pub fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn in_use(referenced_by: Vec<Referrer>) -> ApiError {
        ApiError {
            referenced_by,
            ..Self::new(StatusCode::CONFLICT, "in_use", "该资源仍被引用，无法删除")
        }
    }

    pub fn version_mismatch(current: i64) -> ApiError {
        ApiError {
            current_version: Some(current),
            ..Self::new(
                StatusCode::PRECONDITION_FAILED,
                "version_mismatch",
                "资源已被修改，请刷新后重试",
            )
        }
    }

    pub fn if_match_required() -> ApiError {
        Self::new(
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_required",
            "此请求需要 If-Match 请求头",
        )
    }

    pub fn busy() -> ApiError {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            "请求过于频繁，请稍后再试",
        )
    }

    pub fn too_large() -> ApiError {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "请求内容过大",
        )
    }

    pub fn unavailable() -> ApiError {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "服务暂时不可用，请稍后再试",
        )
    }

    /// Logs `e`; the caller sees only a neutral message, since `e` can name
    /// internals (a query, a constraint, a peer address).
    pub fn internal(e: impl Display) -> ApiError {
        tracing::error!(error = %e, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "服务器内部错误",
        )
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> ApiError {
        ApiError::internal(e)
    }
}

#[derive(Serialize)]
struct Envelope {
    error: String,
    code: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    details: Vec<Detail>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    referenced_by: Vec<Referrer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_version: Option<i64>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let envelope = Envelope {
            error: self.message,
            code: self.code,
            details: self.details,
            referenced_by: self.referenced_by,
            current_version: self.current_version,
        };
        (self.status, Json(envelope)).into_response()
    }
}

/// Why the control plane refused to start. Each message names the variable
/// to look at.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("CP_ENV_ID is {configured}, but this database belongs to environment {stored}")]
    EnvIdMismatch { stored: Uuid, configured: Uuid },
    #[error("CP_ETCD_PREFIX is {configured}, but this database was first booted with {stored}")]
    PrefixChanged { stored: String, configured: String },
    #[error("database (CP_DATABASE_URL): {0}")]
    Db(#[from] DbError),
    #[error("cannot listen on CP_HTTP_ADDR: {0}")]
    Listen(std::io::Error),
    #[error("the browser listener (CP_HTTP_ADDR) stopped: {0}")]
    Serve(std::io::Error),
}
