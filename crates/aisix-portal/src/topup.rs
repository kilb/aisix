//! 线下充值单。
//!
//! 用户发起申请，管理员在后台确认后才入账。真接支付时把「管理员确认」换成
//! 支付回调，账本那一侧不用改。
//!
//! # 批准不能重复入账
//!
//! 两个管理员同时点确认、或者一次请求重试，都会让同一笔单子被处理两次。防它
//! 的办法不是「先查状态再入账」（查和写之间有窗口），而是把状态变更写成
//! `UPDATE … WHERE status = 'pending'`，**靠影响行数判断自己有没有抢到**：
//! 抢到才入账，没抢到说明别人已经处理过，直接返回冲突。整个过程在一个事务里。

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::auth::AppState;
use crate::ledger::Source;
use crate::store::StoreError;

/// 一行充值单的原始列。抽成别名是因为裸元组写在函数体里读不出哪一列是哪个。
type TopupRow = (
    i64,
    String,
    i64,
    Option<String>,
    String,
    String,
    Option<String>,
);

/// 单笔充值的上限。不是风控，是防手滑 —— 多打两个零在这种表单里很常见。
/// 单个用户同时未处理的充值申请数上限。
const MAX_PENDING_TOPUPS: i64 = 5;

/// 单笔金额上限（$10,000）。充值申请与管理员设额度共用同一个数 —— 两处各自
/// 定一个的话，迟早只有一处被调。
pub const MAX_TOPUP_MICRO_USD: i64 = 10_000_000_000;

#[derive(Deserialize)]
pub struct CreateReq {
    /// 有意收 `i64`：负数要能进来才能被明确拒绝，收 u64 会在反序列化就失败，
    /// 错误信息对调用方毫无意义。
    pub micro_usd: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/topups` —— 发起一笔充值申请。
pub async fn create(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateReq>,
) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return unauthorized();
    };
    if req.micro_usd <= 0 {
        return bad("充值金额必须为正");
    }
    if req.micro_usd > MAX_TOPUP_MICRO_USD {
        return bad("单笔充值金额过大，请联系管理员");
    }
    // 未处理的申请有个数上限。不设的话，一个登录用户可以一直提，把管理员的待办
    // 列表和这张表一起刷爆 —— 而管理员没法批量清理（只能一笔笔驳回）。
    match sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM topups WHERE user_id = ?1 AND status = 'pending'",
    )
    .bind(&uid)
    .fetch_one(st.store.pool())
    .await
    {
        Ok((n,)) if n >= MAX_PENDING_TOPUPS => {
            return bad("你还有未处理的充值申请，请等管理员处理完再提交");
        }
        Ok(_) => {}
        Err(_) => return server_error("读取失败"),
    }
    match sqlx::query(
        "INSERT INTO topups (user_id, micro_usd, note, created_at)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(&uid)
    .bind(req.micro_usd)
    .bind(req.note.as_deref())
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(st.store.pool())
    .await
    {
        Ok(_) => (StatusCode::CREATED, Json(json!({"ok": true}))).into_response(),
        Err(_) => server_error("写入失败"),
    }
}

/// `GET /api/topups` —— 本人的充值单。
pub async fn mine(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return unauthorized();
    };
    match rows_for(&st, Some(&uid), None).await {
        Ok(v) => Json(json!({"topups": v})).into_response(),
        Err(_) => server_error("读取失败"),
    }
}

/// `GET /admin/topups` —— 待处理的充值单（管理端）。
pub async fn pending(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !crate::admin::admin_ok(&st, &headers) {
        return crate::admin::forbidden();
    }
    match rows_for(&st, None, Some("pending")).await {
        Ok(v) => Json(json!({"topups": v})).into_response(),
        Err(_) => server_error("读取失败"),
    }
}

#[derive(Deserialize)]
pub struct DecideReq {
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /admin/topups/{id}/approve`
pub async fn approve(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<DecideReq>,
) -> Response {
    if !crate::admin::admin_ok(&st, &headers) {
        return crate::admin::forbidden();
    }
    match decide(&st, id, true, req.note.as_deref()).await {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        // 没抢到状态：别人已经处理过这笔。返回冲突，而不是假装成功 ——
        // 假装成功会让管理员以为自己刚刚入了账。
        Ok(false) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "这笔充值单已经被处理过了"})),
        )
            .into_response(),
        Err(_) => server_error("处理失败"),
    }
}

/// `POST /admin/topups/{id}/reject`
pub async fn reject(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(req): Json<DecideReq>,
) -> Response {
    if !crate::admin::admin_ok(&st, &headers) {
        return crate::admin::forbidden();
    }
    match decide(&st, id, false, req.note.as_deref()).await {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "这笔充值单已经被处理过了"})),
        )
            .into_response(),
        Err(_) => server_error("处理失败"),
    }
}

/// 处理一笔单子。返回是否**由本次调用**完成状态变更。
///
/// 状态变更带 `WHERE status = 'pending'`，靠影响行数判断有没有抢到；抢到才
/// 入账。两步在一个事务里 —— 中间崩了要么都没发生，要么都发生。
async fn decide(
    st: &AppState,
    id: i64,
    approved: bool,
    note: Option<&str>,
) -> Result<bool, StoreError> {
    let note = note.map(str::to_string);
    st.store
        .immediate_tx(move |conn| {
            Box::pin(async move {
                let row: Option<(String, i64)> = sqlx::query_as(
                    "SELECT user_id, micro_usd FROM topups WHERE id = ?1 AND status = 'pending'",
                )
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
                let Some((user_id, micro)) = row else {
                    return Ok(false);
                };

                let affected = sqlx::query(
                    "UPDATE topups SET status = ?2, decided_at = ?3, decided_note = ?4
                     WHERE id = ?1 AND status = 'pending'",
                )
                .bind(id)
                .bind(if approved { "approved" } else { "rejected" })
                .bind(chrono::Utc::now().to_rfc3339())
                .bind(note.as_deref())
                .execute(&mut *conn)
                .await?
                .rows_affected();
                if affected == 0 {
                    // 别人在这两句之间抢先处理了。
                    //
                    // **在 SQLite 上这一句够不到**：`BEGIN IMMEDIATE` 让第二个
                    // 事务在开头就等，等到时 `status` 早已不是 pending，上面那句
                    // SELECT 就已经返回 None 了。
                    //
                    // 留着是因为它在 **Postgres READ COMMITTED** 下才承重：那里
                    // 两个事务的 SELECT 都会读到 pending，谁真的改到了行只能靠
                    // 影响行数决出。设计文档 §8 把多实例门户列为未定，一旦换
                    // Postgres，这一句就是唯一的防线。
                    return Ok(false);
                }

                if approved {
                    crate::ledger::insert_entry(
                        &mut *conn,
                        &user_id,
                        micro,
                        Source::Topup,
                        note.as_deref().or(Some("线下充值")),
                    )
                    .await?;
                }
                Ok(true)
            })
        })
        .await
}

async fn rows_for(
    st: &AppState,
    user_id: Option<&str>,
    status: Option<&str>,
) -> Result<Vec<serde_json::Value>, StoreError> {
    let rows: Vec<TopupRow> = sqlx::query_as(
        "SELECT t.id, t.user_id, t.micro_usd, t.note, t.status, t.created_at, u.email
             FROM topups t JOIN users u ON u.id = t.user_id
             WHERE (?1 IS NULL OR t.user_id = ?1) AND (?2 IS NULL OR t.status = ?2)
             ORDER BY t.id DESC LIMIT 200",
    )
    .bind(user_id)
    .bind(status)
    .fetch_all(st.store.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, uid, micro, note, status, created_at, email)| {
            json!({
                "id": id,
                "user_id": uid,
                "email": email,
                "micro_usd": micro,
                "note": note,
                "status": status,
                "created_at": created_at,
            })
        })
        .collect())
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "未登录"}))).into_response()
}
fn bad(m: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": m}))).into_response()
}
fn server_error(m: &str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": m}))).into_response()
}
