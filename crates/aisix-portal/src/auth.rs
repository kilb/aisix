//! 注册、登录、会话。
//!
//! 口令用 argon2（与 `crates/aisix-console` 一致），不用 sha256：口令是低熵的，
//! 无盐无工作因子的散列被拖库后就是离线爆破。sha256 只用于 API 密钥那种高熵
//! 随机串。

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::Argon2;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::store::{Store, StoreError};

/// 口令下限。低于此长度的口令挡在注册处，而不是留给用户自己负责。
const MIN_PASSWORD_LEN: usize = 12;

#[derive(Deserialize)]
pub struct RegisterReq {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `POST /api/register`
pub async fn register(State(store): State<Store>, Json(req): Json<RegisterReq>) -> Response {
    let email = req.email.trim().to_ascii_lowercase();
    if !plausible_email(&email) {
        return bad_request("邮箱格式不正确");
    }
    if req.password.chars().count() < MIN_PASSWORD_LEN {
        return bad_request(&format!("口令至少 {MIN_PASSWORD_LEN} 个字符"));
    }

    // argon2 是故意 CPU 昂贵的：直接在 async handler 里算会占住执行器线程，
    // 并发注册时把整个进程拖慢。丢到阻塞线程池里去。
    let pw = req.password.clone();
    let hash = match tokio::task::spawn_blocking(move || hash_password(&pw)).await {
        Ok(Ok(h)) => h,
        Ok(Err(_)) | Err(_) => return server_error("口令散列失败"),
    };

    let id = uuid::Uuid::new_v4().to_string();
    match store
        .insert_user(&id, &email, &hash, req.display_name.as_deref())
        .await
    {
        Ok(()) => (StatusCode::CREATED, Json(json!({"user_id": id}))).into_response(),
        // 唯一约束冲突是可预期的用户错误，不是服务端故障。
        Err(StoreError::EmailTaken) => {
            (StatusCode::CONFLICT, Json(json!({"error": "邮箱已被注册"}))).into_response()
        }
        Err(_) => server_error("写入失败"),
    }
}

/// 用 argon2 算出 PHC 串。
fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut rand_core::OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

/// 只做最低限度的形状检查。真正的可达性由邮箱验证流程负责（一期不做，
/// 见计划裁决 4），这里挡掉的是明显不是邮箱的输入。
fn plausible_email(s: &str) -> bool {
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
}

fn server_error(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": msg})),
    )
        .into_response()
}
