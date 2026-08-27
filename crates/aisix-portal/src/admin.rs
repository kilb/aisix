//! 管理端 API：列出用户、发放额度。
//!
//! 归门户所有，不归控制台（计划裁决 3）：用户库只能有一个写入者。控制台的
//! 管理界面通过这里访问。
//!
//! 凭据与用户会话**完全分离**：一个合法的用户会话打不开这里。角色判断错一次
//! 就是全量泄漏，所以这里认的是另一套东西（`PORTAL_ADMIN_TOKEN`），而不是在
//! 同一套会话上挂一个 is_admin 标志。

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::auth::AppState;
use crate::ledger::{Ledger, Source};

/// 校验管理凭据。用常量时间比较，避免把 token 的前缀猜出来。
pub(crate) fn admin_ok(st: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = st.admin_token() else {
        // 没配管理凭据就整个关掉，而不是放行。默认拒绝。
        return false;
    };
    let Some(got) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    constant_time_eq(got.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn forbidden() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "需要管理凭据"})),
    )
        .into_response()
}

/// `GET /admin/users`
///
/// 管理界面据此**选择**发放对象，而不是让人手输 user_id。手输一个 uuid 错一个
/// 字符，网关照常放行、指标打错标签、门户查不到用量 —— 于是永不扣款，用户免费
/// 用而没人会发现。让界面从这份列表里选，那条路就不存在。
pub async fn list_users(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !admin_ok(&st, &headers) {
        return forbidden();
    }
    let ledger = Ledger::new(st.store.clone());
    match st.store.all_users().await {
        Ok(users) => {
            let mut out = Vec::with_capacity(users.len());
            for u in users {
                let balance = ledger.balance(&u.id).await.unwrap_or(0);
                let granted = ledger.total_granted(&u.id).await.unwrap_or(0);
                let allocated = st.store.allocated_to_keys(&u.id).await.unwrap_or(0);
                out.push(json!({
                    "user_id": u.id,
                    "email": u.email,
                    "display_name": u.display_name,
                    "disabled": u.disabled,
                    "balance_micro_usd": balance,
                    "granted_micro_usd": granted,
                    // 已分配到各把密钥上的额度之和。运维看得到「这个人把额度
                    // 分出去多少」，也就看得出把总额调低到什么程度会撞上分配。
                    "allocated_micro_usd": allocated,
                }));
            }
            Json(json!({"users": out})).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "读取失败"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct QuotaReq {
    /// 目标总额度。有意用 `i64`：负数要能进来才能被明确拒绝。
    pub micro_usd: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /admin/users/{id}/quota` —— 把这个用户的总额度**设定**成给定值。
///
/// 跟发放（追加）分开：运维想的是「这个人有多少额度」，而不是「再给他加多少」。
/// 追加那条路留着，充值单确认走的就是它。
///
/// 账本仍然只追加：这里记的是与当前总额的差，可正可负。所以「谁在什么时候把
/// 额度改成了多少」留得下痕，余额与总额也都还能从流水重算。
pub async fn set_quota(
    State(st): State<AppState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<QuotaReq>,
) -> Response {
    if !admin_ok(&st, &headers) {
        return forbidden();
    }
    if req.micro_usd < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "额度不能是负数"})),
        )
            .into_response();
    }
    match st.store.user_by_id(&user_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "没有这个用户"})),
            )
                .into_response()
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "读取失败"})),
            )
                .into_response()
        }
    }

    let ledger = Ledger::new(st.store.clone());
    match ledger
        .set_total_granted(&user_id, req.micro_usd, req.note.as_deref())
        .await
    {
        Ok(delta) => {
            let balance = ledger.balance(&user_id).await.unwrap_or(0);
            // 已分配到各把密钥上的额度之和。调低总额时它可能反过来超过总额 ——
            // 花费仍受用户级那道闸约束，所以不拒绝这次设定；但必须说出来，否则
            // 管理员看不到这个人的分配已经对不上，而用户那边会看到一个负的
            // 「可再分配」而不知道为什么。
            let allocated = st.store.allocated_to_keys(&user_id).await.unwrap_or(0);
            Json(json!({
                "ok": true,
                "granted_micro_usd": req.micro_usd,
                "delta_micro_usd": delta,
                "balance_micro_usd": balance,
                "allocated_micro_usd": allocated,
                "over_allocated_micro_usd": (allocated - req.micro_usd).max(0),
            }))
            .into_response()
        }
        // 荒谬的金额是输入错误，不是服务端故障 —— 回 400 才能让调用方知道该改
        // 什么。都揉成 500 的话，管理员看到的是「服务挂了」。
        Err(crate::store::StoreError::OutOfRange) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "金额超出可表示范围"})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "记账失败"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct GrantReq {
    /// 有意用 `i64` 而不是 `u64`：负数要能进来，才能被明确拒绝并回 400。
    /// 收成 u64 的话反序列化会先失败，错误信息对调用方毫无意义。
    pub micro_usd: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /admin/users/:id/grant`
pub async fn grant(
    State(st): State<AppState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<GrantReq>,
) -> Response {
    if !admin_ok(&st, &headers) {
        return forbidden();
    }
    if req.micro_usd <= 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "发放金额必须为正"})),
        )
            .into_response();
    }
    // 发放对象必须是已存在的用户。挡住手输错 id 的那条路。
    match st.store.user_by_id(&user_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "没有这个用户"})),
            )
                .into_response()
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "读取失败"})),
            )
                .into_response()
        }
    }

    let ledger = Ledger::new(st.store.clone());
    match ledger
        .credit(
            &user_id,
            req.micro_usd as u64,
            Source::AdminGrant,
            req.note.as_deref(),
        )
        .await
    {
        Ok(()) => {
            let balance = ledger.balance(&user_id).await.unwrap_or(0);
            Json(json!({"ok": true, "balance_micro_usd": balance})).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "记账失败"})),
        )
            .into_response(),
    }
}
