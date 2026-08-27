//! AISIX 网关控制台 —— 部署侧独立服务。
//!
//! 刻意不属于 aisix 仓库：那个仓库的 CLAUDE.md 规定 Dashboard 归控制平面，
//! 数据面不带用户界面。这里是单机独立部署的管理面，只依赖网关对外已有的
//! 契约（管理 API 的 GET、Prometheus 抓取口、resources.yaml + SIGHUP），
//! 不碰网关内部。
//!
//! 三个数据来源，各自的能力边界：
//! - **资源读**：网关管理 API（只读，写端点被上游有意移除）。
//! - **用量读**：Prometheus。计数器带 `api_key_id` / `model` / `provider`
//!   标签，所以按 key、按模型的用量是真实数据，不是估算。
//! - **资源写**：改 resources.yaml 后给网关发 SIGHUP。写之前先用网关自带的
//!   `aisix validate` 校验临时文件——坏配置绝不落盘，否则一次手滑就能让
//!   网关重载失败。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

/// 会话有效期。控制台能看到明文上游密钥，所以不做长期免登录。
const SESSION_TTL_SECS: u64 = 8 * 3600;

#[derive(Clone)]
struct AppState {
    /// 网关管理 API 基址（回环）。
    admin_url: String,
    /// 管理密钥。只存在服务端，绝不下发给浏览器。
    admin_key: String,
    /// 自助门户的地址与管理凭据。
    ///
    /// 凭据留在服务端，跟 `admin_key` 同一个道理：浏览器拿到它就等于拿到了
    /// 全部用户的账目和发放额度的权力。控制台只把结果转出去，不把凭据转出去。
    portal_url: String,
    portal_admin_token: Option<String>,
    /// Prometheus 基址（回环）。
    prom_url: String,
    /// 声明式资源文件，控制台写它。
    resources_path: PathBuf,
    /// 网关二进制，用来在落盘前跑 `validate`。
    aisix_bin: PathBuf,
    /// 口令的 argon2 散列。
    password_hash: String,
    /// 活跃会话 token -> 过期时间（unix 秒）。
    sessions: Arc<RwLock<HashMap<String, u64>>>,
    /// 串行化整个「写临时文件 → 校验 → 替换 → SIGHUP」序列。
    ///
    /// 没有它，两个并发保存会用同一条路径：A 写好、B 覆盖、A 校验到的是 B 的
    /// 内容、A 再把它 rename 上线——界面上承诺的「校验不通过就不落盘」在并发
    /// 下就是假的。两个浏览器标签页即可触发。
    save_lock: Arc<tokio::sync::Mutex<()>>,
    /// 限制同时进行的口令校验数。
    ///
    /// argon2 默认单次校验占 19 MiB，而这个服务跑在 MemoryMax=120M 下：
    /// 几个并发登录请求就能把它 OOM 掉，且完全不需要凭据。登录后的 600ms
    /// 延迟挡不住这个——它在计算之后，并发连接直接绕过。
    login_gate: Arc<tokio::sync::Semaphore>,
    http: reqwest::Client,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_token() -> String {
    // rand_core 0.6 与 argon2 依赖的是同一个版本——会话 token 和口令盐
    // 用同一个 OsRng，省掉两套随机源的版本对齐。
    use rand_core::RngCore;
    let mut b = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ── 鉴权 ──────────────────────────────────────────────────────────────

impl AppState {
    /// 会话 cookie 是否有效。顺手清掉过期项，免得内存里无限堆积。
    async fn authed(&self, headers: &HeaderMap) -> bool {
        let Some(tok) = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| {
                c.split(';')
                    .filter_map(|kv| kv.trim().split_once('='))
                    .find(|(k, _)| *k == "aisix_console")
                    .map(|(_, v)| v.to_string())
            })
        else {
            return false;
        };
        let now = now_secs();
        let mut s = self.sessions.write().await;
        s.retain(|_, exp| *exp > now);
        s.get(&tok).is_some_and(|exp| *exp > now)
    }
}

async fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    if state.authed(headers).await {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "未登录"}))).into_response())
    }
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

async fn login(State(st): State<AppState>, Json(req): Json<LoginReq>) -> Response {
    // 未登录即可触达的最贵操作。argon2 默认单次占 19 MiB，本服务 MemoryMax=120M，
    // 所以几个并发请求就能 OOM 掉它——闸住并发数，拿不到许可直接 429。
    let Ok(_permit) = st.login_gate.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "登录校验繁忙，请稍后重试"})),
        )
            .into_response();
    };
    // argon2 是 CPU+内存密集的同步计算，放在异步 worker 上会卡住整个 runtime。
    let hash = st.password_hash.clone();
    let pw = req.password.clone();
    let ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|parsed| {
                Argon2::default()
                    .verify_password(pw.as_bytes(), &parsed)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if !ok {
        // 固定延迟，避免把「口令错」和「解析失败」在时间上区分开。
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "口令不正确"})),
        )
            .into_response();
    }
    let tok = random_token();
    st.sessions
        .write()
        .await
        .insert(tok.clone(), now_secs() + SESSION_TTL_SECS);
    // HttpOnly + SameSite=Strict：控制台能改网关配置，不能让别的站点借用会话。
    // Secure 是因为它只经 nginx 的 HTTPS 暴露。
    let cookie = format!(
        "aisix_console={tok}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}"
    );
    ([(header::SET_COOKIE, cookie)], Json(json!({"ok": true}))).into_response()
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(tok) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .filter_map(|kv| kv.trim().split_once('='))
                .find(|(k, _)| *k == "aisix_console")
                .map(|(_, v)| v.to_string())
        })
    {
        st.sessions.write().await.remove(&tok);
    }
    (
        [(
            header::SET_COOKIE,
            "aisix_console=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0",
        )],
        Json(json!({"ok": true})),
    )
        .into_response()
}

/// 前后端之间的接口契约版本。
///
/// 界面是独立部署的静态产物（`web/`，由 nginx 托管），所以两边可以各自
/// 更新 —— 于是版本偏移成为可能。这个数字是唯一的判据。
///
/// **只在前后端接口真的变化时才 +1**：新增/删除/改名一个端点、改变请求或
/// 响应的字段形状、改变一个字段的含义。不要用构建时间或 git SHA 代替它 ——
/// 那会让任何一侧的无关重建都报一次偏移，而误报是训练运维忽略告警最快的
/// 办法。
///
/// 偏移不是提示级别的问题。一个不带 `base_version` 的旧界面会落进
/// [`stale_write`] 的逃生口（缺版本 = 不检查），于是丢失更新会静默回来：
/// 两个标签页各改一处，后保存的整份覆盖先保存的，而被覆盖的可能是一次
/// 密钥吊销。所以界面在偏移时必须停止写入，不只是显示一条横幅。
const API_CONTRACT_VERSION: u32 = 1;

async fn session_state(State(st): State<AppState>, headers: HeaderMap) -> Json<Value> {
    // 契约版本挂在这个端点上，而不是新开一个：界面启动时本来就要问一次
    // 登录状态，省一个往返。它在鉴权之前就返回，这是有意的 —— 偏移应该在
    // 登录之前就能被发现。
    Json(json!({
        "authed": st.authed(&headers).await,
        "api_contract": API_CONTRACT_VERSION,
    }))
}

// ── 自助门户的管理端（转发，凭据不出服务端）────────────────────────────

/// 门户未配置时的统一回应。控制台可以独立部署，没有门户不是错误。
fn portal_absent() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "未配置自助门户（PORTAL_ADMIN_TOKEN 未设置）"})),
    )
        .into_response()
}

/// `GET /api/portal/users` —— 注册用户与各自余额。
async fn portal_users(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    let Some(token) = st.portal_admin_token.clone() else {
        return portal_absent();
    };
    match st
        .http
        .get(format!("{}/admin/users", st.portal_url))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
    {
        Ok(r) => passthrough(r).await,
        Err(e) => portal_unreachable(e),
    }
}

/// `POST /api/portal/users/{id}/grant` —— 给某个用户发放额度。
async fn portal_grant(
    State(st): State<AppState>,
    axum::extract::Path(user_id): axum::extract::Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    let Some(token) = st.portal_admin_token.clone() else {
        return portal_absent();
    };
    match st
        .http
        .post(format!("{}/admin/users/{user_id}/grant", st.portal_url))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => passthrough(r).await,
        Err(e) => portal_unreachable(e),
    }
}

/// `GET /api/portal/topups` —— 待确认的充值单。
async fn portal_topups(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    let Some(token) = st.portal_admin_token.clone() else {
        return portal_absent();
    };
    match st
        .http
        .get(format!("{}/admin/topups", st.portal_url))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
    {
        Ok(r) => passthrough(r).await,
        Err(e) => portal_unreachable(e),
    }
}

/// `POST /api/portal/topups/{id}/{decision}` —— 确认或驳回一笔充值单。
async fn portal_decide_topup(
    State(st): State<AppState>,
    axum::extract::Path((id, decision)): axum::extract::Path<(i64, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    // 白名单，不是把路径段直接拼进 URL：拼的话 `../` 之类能走到门户的别的端点上。
    if decision != "approve" && decision != "reject" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "只能是 approve 或 reject"})),
        )
            .into_response();
    }
    let Some(token) = st.portal_admin_token.clone() else {
        return portal_absent();
    };
    match st
        .http
        .post(format!("{}/admin/topups/{id}/{decision}", st.portal_url))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => passthrough(r).await,
        Err(e) => portal_unreachable(e),
    }
}

fn portal_unreachable(e: reqwest::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"error": format!("门户不可达: {e}")})),
    )
        .into_response()
}

/// 把门户的应答原样转出去（状态码 + JSON）。
async fn passthrough(r: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match r.json::<Value>().await {
        Ok(v) => (status, Json(v)).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("门户返回的不是 JSON: {e}")})),
        )
            .into_response(),
    }
}

// ── 资源读（转发网关管理 API）──────────────────────────────────────────

/// 管理密钥留在服务端：浏览器拿到它就等于拿到了整个网关的读权限。
async fn admin_get(st: &AppState, path: &str) -> Result<Value, String> {
    let url = format!("{}{}", st.admin_url, path);
    let resp = st
        .http
        .get(&url)
        .header("Authorization", format!("Bearer {}", st.admin_key))
        .send()
        .await
        .map_err(|e| format!("管理 API 不可达: {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("管理 API 返回的不是 JSON: {e}"))?;
    if !status.is_success() {
        return Err(format!("管理 API {status}: {body}"));
    }
    Ok(body)
}

async fn api_resources(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    // 一次取齐，前端少打几个来回；单机规模下资源量很小。
    let mut out = serde_json::Map::new();
    for (key, path) in [
        ("models", "/admin/v1/models"),
        ("api_keys", "/admin/v1/api_keys"),
        ("provider_keys", "/admin/v1/provider_keys"),
        ("guardrails", "/admin/v1/guardrails"),
        ("cache_policies", "/admin/v1/cache_policies"),
        // 注意：限流策略没有对应的管理 API 读端点，所以这里读不到它。
        // 策略走 /api/file 从 resources.yaml 直接读——那才是文件模式下的真相来源。
    ] {
        match admin_get(&st, path).await {
            Ok(v) => {
                out.insert(key.to_string(), v);
            }
            // 单项失败不该让整页空白——把错误放进该项，其余照常渲染。
            Err(e) => {
                out.insert(key.to_string(), json!({"error": e}));
            }
        }
    }
    match admin_get(&st, "/admin/v1/models/status").await {
        Ok(v) => {
            out.insert("model_status".into(), v);
        }
        Err(e) => {
            out.insert("model_status".into(), json!({"error": e}));
        }
    }
    Json(Value::Object(out)).into_response()
}

// ── 用量读（Prometheus）────────────────────────────────────────────────

#[derive(Deserialize)]
struct PromQuery {
    query: String,
    /// 有值走 query_range，画曲线；无值走瞬时查询，出总量。
    #[serde(default)]
    range_hours: Option<u32>,
}

async fn api_metrics(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(q): Json<PromQuery>,
) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    // 之前写的是 `!starts_with(..) && !contains(..)`——contains 那一半让前缀
    // 检查彻底失效，`{__name__=~".+"} or aisix_x` 照样通过，等于把这里变成
    // 任意 Prometheus 代理（能读同一台机上任何抓取目标，还能用高基数查询打挂它）。
    // 改成真正的限制：查询里出现的每一个指标名都必须是 aisix_ 前缀。
    if !promql_only_touches_aisix(&q.query) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "只允许查询 aisix_* 指标"})),
        )
            .into_response();
    }
    let (url, params): (String, Vec<(String, String)>) = match q.range_hours {
        Some(h) => {
            let end = now_secs();
            let start = end.saturating_sub(h as u64 * 3600);
            // 目标约 120 个点，够画图又不至于把小机器查爆。
            let step = ((h as u64 * 3600) / 120).max(60);
            (
                format!("{}/api/v1/query_range", st.prom_url),
                vec![
                    ("query".into(), q.query.clone()),
                    ("start".into(), start.to_string()),
                    ("end".into(), end.to_string()),
                    ("step".into(), step.to_string()),
                ],
            )
        }
        None => (
            format!("{}/api/v1/query", st.prom_url),
            vec![("query".into(), q.query.clone())],
        ),
    };
    match st.http.get(&url).query(&params).send().await {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => Json::<Value>(v).into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("Prometheus 返回异常: {e}")})),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("Prometheus 不可达: {e}")})),
        )
            .into_response(),
    }
}

/// 查询里出现的每个指标名是否都带 `aisix_` 前缀。
///
/// 做法是取出所有看起来像指标名的标识符（排除 PromQL 关键字与函数名），
/// 要求它们全部以 `aisix_` 开头。宁可误拒也不放过：这个端点只服务本控制台
/// 自己构造的几条查询，没有必要支持完整的 PromQL 表面。
fn promql_only_touches_aisix(q: &str) -> bool {
    const ALLOWED_WORDS: &[&str] = &[
        "sum",
        "rate",
        "by",
        "avg",
        "max",
        "min",
        "count",
        "increase",
        "irate",
        "topk",
        "bottomk",
        "without",
        "on",
        "group_left",
        "group_right",
        "or",
        "and",
        "unless",
        "offset",
        "le",
        "quantile",
        "histogram_quantile",
    ];
    let mut ident = String::new();
    let mut idents: Vec<String> = Vec::new();
    for ch in q.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' {
            ident.push(ch);
        } else if !ident.is_empty() {
            idents.push(std::mem::take(&mut ident));
        }
    }
    let mut saw_metric = false;
    for id in idents {
        if id.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue; // 数字字面量
        }
        if ALLOWED_WORDS.contains(&id.as_str()) {
            continue;
        }
        if id.starts_with("aisix_") {
            saw_metric = true;
            continue;
        }
        // 标签名与标签值也会落到这里；只允许已知的标签维度。
        const LABELS: &[&str] = &[
            "api_key_id",
            "model",
            "provider",
            "endpoint",
            "policy",
            "outcome",
            "status",
            "team_id",
            "user_id",
            "scope",
            "layer",
            "job",
            "instance",
        ];
        if LABELS.contains(&id.as_str()) {
            continue;
        }
        return false;
    }
    saw_metric
}

// ── 资源写（resources.yaml + SIGHUP）──────────────────────────────────

/// 资源文件按原样 `Value` 透传，不映射成强类型：控制台不该比网关更懂
/// schema，多一层类型就多一处会和上游漂移的地方。校验交给 `aisix validate`。
async fn api_file_get(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    match tokio::fs::read_to_string(&st.resources_path).await {
        Ok(s) => match serde_yaml_ng::from_str::<Value>(&s) {
            Ok(v) => {
                let version = sha256_hex(s.as_bytes());
                Json(json!({"doc": v, "raw": s, "version": version})).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("resources.yaml 解析失败: {e}")})),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("读不到 resources.yaml: {e}")})),
        )
            .into_response(),
    }
}

/// 保存失败的两种类型。分成枚举而不是靠比对错误文本来认版本冲突：
/// 文本比对会在任何人改动措辞、加前缀、翻译这条消息时静默失效，而失效的
/// 表现是 409 变成 400 —— 前端据此提示「重新载入」的分支就再也走不到。
enum SaveError {
    /// 调用方基于的版本已经不是磁盘上的版本。
    Stale,
    /// 序列化 / 校验 / 落盘失败，文本直接面向用户。
    Failed(String),
}

impl SaveError {
    fn message(&self) -> String {
        match self {
            Self::Stale => "配置在你编辑期间已被改动，未保存。请重新载入后再改一遍——\
                            直接覆盖会悄悄丢掉别人（或另一个标签页）刚做的修改。"
                .to_string(),
            Self::Failed(m) => m.clone(),
        }
    }
}

impl From<String> for SaveError {
    fn from(m: String) -> Self {
        Self::Failed(m)
    }
}

/// 乐观并发：调用方读到的版本还是不是磁盘上的当前版本。
///
/// 控制台的写入是「整份文档读-改-写」，`save_lock` 只保证两次写不交错，
/// 拦不住丢失更新：两个标签页各自持有 T 时刻的文档，先保存的那份改动会被
/// 后保存的整份覆盖，而且没有任何提示。被静默回退的如果是一次密钥吊销，
/// 那把密钥就又活了。
///
/// 用内容哈希而不是 mtime：mtime 在部分文件系统上只有秒级粒度，且对
/// 「改回等长内容」完全无感。`None` 表示调用方没有带版本（例如用 curl
/// 直接打接口），此时放行——这是一个诚实的逃生口，不是默认路径。
fn stale_write(base_version: Option<&str>, on_disk: &str) -> bool {
    matches!(base_version, Some(v) if v != on_disk)
}

/// 声明了契约版本的调用方必须带 `base_version`。
///
/// `stale_write` 的逃生口（缺版本 = 不检查）是给脚本和 curl 留的，那些调用方
/// 不声明契约。界面声明了，所以它不带版本只有一个解释：它是一个不知道这个
/// 字段存在的旧界面 —— 而放它过去就是把丢失更新重新放回来。
fn contract_client_must_send_version(
    client_contract: Option<u32>,
    base_version: Option<&str>,
) -> bool {
    client_contract.is_some() && base_version.is_none()
}

/// 校验 → 落盘 → SIGHUP。三步都成功才算保存。
///
/// 校验用的是网关自己的 `aisix validate`，不是这里重新实现一份 schema：
/// 重实现一定会和上游漂移，而漂移的方向永远是「控制台放行了网关拒绝的东西」。
async fn save_resources(
    st: &AppState,
    doc: &Value,
    base_version: Option<&str>,
    client_contract: Option<u32>,
) -> Result<String, SaveError> {
    if contract_client_must_send_version(client_contract, base_version) {
        return Err(SaveError::Stale);
    }
    let yaml = serde_yaml_ng::to_string(doc).map_err(|e| format!("序列化失败: {e}"))?;

    // 整个序列串行执行。并发下共用一条临时路径会让 A 校验到 B 的内容
    // 再把它上线——界面承诺的「校验不通过就不落盘」正是死在这里。
    let _guard = st.save_lock.lock().await;

    // 比对必须在锁内、且是重读磁盘：在锁外读会让检查本身可以被插队，
    // 而用请求进来时的旧读数比对等于没比对。文件读不到时不拦——那种情况
    // 下面的替换步骤自己会失败，在这里多编一个理由只会遮住真正的错误。
    if let Ok(current) = tokio::fs::read_to_string(&st.resources_path).await
        && stale_write(base_version, &sha256_hex(current.as_bytes()))
    {
        return Err(SaveError::Stale);
    }

    // 随机后缀：即便将来锁被绕过，两个请求也不会踩同一个文件。
    let tmp = st
        .resources_path
        .with_extension(format!("yaml.console-tmp.{}", &random_token()[..16]));

    // 0600 建文件：这份内容和正式配置一样含明文上游密钥。用 fs::write 会按
    // umask 建成 0644，而 rename 会把这个权限一并带到正式文件上——运维设的
    // 0600 会在第一次保存后被静默降级。
    {
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .await
            .map_err(|e| format!("建临时文件失败: {e}"))?;
        use tokio::io::AsyncWriteExt;
        f.write_all(yaml.as_bytes())
            .await
            .map_err(|e| format!("写临时文件失败: {e}"))?;
        // rename 之前落盘：崩溃在写与 rename 之间会让网关读到半截文件。
        f.sync_all().await.map_err(|e| format!("落盘失败: {e}"))?;
    }

    // 失败路径统一清理，避免留下一份 0600 但内容是废稿的密钥副本。
    let cleanup = |t: std::path::PathBuf| async move {
        let _ = tokio::fs::remove_file(t).await;
    };

    let out = match tokio::process::Command::new(&st.aisix_bin)
        .arg("validate")
        .arg("--resources")
        .arg(&tmp)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            cleanup(tmp).await;
            return Err(SaveError::Failed(format!("无法执行 aisix validate: {e}")));
        }
    };
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            msg
        };
        cleanup(tmp).await;
        return Err(SaveError::Failed(format!(
            "配置校验未通过，未改动网关：\n{msg}"
        )));
    }

    if let Err(e) = tokio::fs::rename(&tmp, &st.resources_path).await {
        cleanup(tmp).await;
        return Err(SaveError::Failed(format!("替换 resources.yaml 失败: {e}")));
    }

    let hup = tokio::process::Command::new("pkill")
        .arg("-HUP")
        .arg("-x")
        .arg("aisix")
        .output()
        .await;
    // 配置已经上线了——SIGHUP 没送到只是「还没热加载」，不是保存失败。
    // 当成失败返回会让调用方以为什么都没发生，而铸密钥那条路径会因此
    // 丢掉刚生成的明文，留下一把没人持有的活密钥。
    let reload_ok = matches!(&hup, Ok(o) if o.status.success());
    let detail = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // 配置改写留痕：谁在什么时候改了网关配置，此前无处可查。
    tracing_line(&format!(
        "resources.yaml rewritten ({} bytes), reload_signal={}",
        yaml.len(),
        if reload_ok { "sent" } else { "FAILED" }
    ));

    if reload_ok {
        Ok(detail)
    } else {
        Ok(format!(
            "{detail}\n注意：配置已保存，但 SIGHUP 未送达网关，需要手动 systemctl reload aisix"
        ))
    }
}

/// 单行审计输出，交给 systemd 收进 journal。
fn tracing_line(msg: &str) {
    println!("[audit] {msg}");
}

#[derive(Deserialize)]
struct WriteReq {
    /// 完整的资源文档（表单页整体提交）。
    #[serde(default)]
    doc: Option<Value>,
    /// 原始 YAML 文本（配置原文页直接提交）。
    ///
    /// 两条入口最终走同一条「校验 → 原子替换 → SIGHUP」路径：没有专用界面的
    /// 资源类型靠这条编辑，而它得到的安全保证与表单完全相同——校验不通过
    /// 就不落盘。给原文一条独立入口而不是让前端先解析成 JSON，是因为那样
    /// 会把 YAML 的解析语义分叉成两份实现。
    #[serde(default)]
    raw: Option<String>,
    /// 调用方这次编辑所基于的版本（`GET /api/file` 返回的 `version`）。
    /// 缺省时不做并发检查，见 [`stale_write`]。
    #[serde(default)]
    base_version: Option<String>,
    /// 调用方自报的契约版本。
    ///
    /// 界面在启动时会比对契约并在偏移时自我拦截，但那只挡得住「新界面 +
    /// 旧后端」。反方向挡不住：一个旧界面根本不读契约字段，于是照常运行，
    /// 并且因为它不带 `base_version` 而落进「缺版本 = 不检查」的逃生口 ——
    /// 丢失更新静默回来。
    ///
    /// 所以服务端也要有一条：声明了契约的调用方必须带版本。脚本和 curl
    /// 不声明契约，继续走逃生口；界面一旦声明，就再也不能忘。
    #[serde(default)]
    client_contract: Option<u32>,
}

async fn api_file_put(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<WriteReq>,
) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    // 原文优先：两个字段都给时以原文为准，因为那是用户实际看到并编辑的东西。
    let doc = match (&req.raw, &req.doc) {
        (Some(text), _) => match serde_yaml_ng::from_str::<Value>(text) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("YAML 解析失败：{e}")})),
                )
                    .into_response();
            }
        },
        (None, Some(d)) => d.clone(),
        (None, None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "请求里既没有 doc 也没有 raw"})),
            )
                .into_response();
        }
    };
    match save_resources(&st, &doc, req.base_version.as_deref(), req.client_contract).await {
        Ok(detail) => Json(json!({"ok": true, "detail": detail})).into_response(),
        // 版本冲突是 409 而不是 400：前端要据此提示「重新载入」，而 400
        // 的其余成员都是「你写的内容不合法」，两者的处理动作完全不同。
        Err(e @ SaveError::Stale) => {
            (StatusCode::CONFLICT, Json(json!({"error": e.message()}))).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.message()}))).into_response(),
    }
}

/// 生成一个调用方密钥：返回明文一次，落盘只存 sha256。
async fn api_mint_key(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    let plaintext = format!("sk-aisix-{}", random_token());
    let hash = sha256_hex(plaintext.as_bytes());
    Json(json!({"plaintext": plaintext, "key_hash": hash})).into_response()
}

/// key_hash 用的 sha256。
///
/// 之前是 shell 出去调 `sha256sum`，二进制缺失时返回空串——于是会铸出一把
/// `key_hash: ""` 的密钥写进配置。凭据计算不能有「失败即空」这种路径。
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ── 调用日志（journald）────────────────────────────────────────────────

/// 每个请求一行的结构化访问日志，从 journald 读回来。
///
/// 网关把它写到 stdout，systemd 收进 journal —— 独立部署下这是唯一不需要
/// 外部服务就能拿到的逐请求记录。两个限制照实说，不在界面上假装没有：
/// 流式请求的行是在 SSE body 开始推之前写的，所以没有 token 数；journald
/// 的留存是它自己的配置，不是无限历史。
#[derive(Deserialize)]
struct LogQuery {
    /// 只看这个 api_key_id 的记录；空则全部。
    #[serde(default)]
    api_key_id: String,
    #[serde(default)]
    limit: Option<u32>,
}

/// 从一行 tracing 输出里取出 `key=value` / `key="value"` 对。
fn parse_kv(line: &str) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // 找一个 `=`，向左回溯出键名
        if b[i] != b'=' {
            i += 1;
            continue;
        }
        let mut ks = i;
        while ks > 0 && (b[ks - 1].is_ascii_alphanumeric() || b[ks - 1] == b'_') {
            ks -= 1;
        }
        if ks == i {
            i += 1;
            continue;
        }
        let key = &line[ks..i];
        let mut j = i + 1;
        let val = if j < b.len() && b[j] == b'"' {
            j += 1;
            let st = j;
            while j < b.len() && b[j] != b'"' {
                j += 1;
            }
            let v = &line[st..j];
            j += 1;
            v.to_string()
        } else {
            let st = j;
            while j < b.len() && b[j] != b' ' {
                j += 1;
            }
            line[st..j].to_string()
        };
        out.insert(key.to_string(), Value::String(val));
        i = j;
    }
    out
}

async fn api_logs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(q): Json<LogQuery>,
) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    // 多取一些再过滤：journald 没法按字段筛 tracing 的行内字段。
    let fetch = q.limit.unwrap_or(100).clamp(1, 500) * 6;
    let out = tokio::process::Command::new("journalctl")
        .args([
            "-u",
            "aisix",
            "--no-pager",
            "-o",
            "cat",
            "-n",
            &fetch.to_string(),
        ])
        .output()
        .await;
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("读取 journal 失败：{err}。控制台用户需要在 systemd-journal 组里。")})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("无法执行 journalctl: {e}")})),
            )
                .into_response();
        }
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows: Vec<Value> = Vec::new();
    for line in text.lines().rev() {
        // 只要带 api_key_id 的行——那是逐请求记录，其余是启动/重载噪声。
        if !line.contains("api_key_id=") {
            continue;
        }
        let kv = parse_kv(line);
        if !q.api_key_id.is_empty()
            && kv.get("api_key_id").and_then(Value::as_str) != Some(q.api_key_id.as_str())
        {
            continue;
        }
        // 行首的 RFC3339 时间戳是 tracing 打的，不在 kv 里。
        let ts = line.split_whitespace().next().unwrap_or("").to_string();
        let mut row = Value::Object(kv);
        row["_ts"] = Value::String(ts);
        rows.push(row);
        if rows.len() as u32 >= q.limit.unwrap_or(100) {
            break;
        }
    }
    Json(json!({"rows": rows})).into_response()
}

// ── 上游模型清单 ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct UpstreamModelsReq {
    /// resources.yaml 里的供应商 display_name。
    provider_key: String,
}

/// 拉取某个供应商上游的模型清单。
///
/// 网关不知道上游有什么模型——它只按配置转发——所以这一步必须直接问上游。
/// 凭据从 resources.yaml 读，只在服务端用，不下发给浏览器。
async fn api_upstream_models(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpstreamModelsReq>,
) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    let doc: Value = match tokio::fs::read_to_string(&st.resources_path)
        .await
        .ok()
        .and_then(|s| serde_yaml_ng::from_str(&s).ok())
    {
        Some(v) => v,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "读不到 resources.yaml"})),
            )
                .into_response();
        }
    };
    let Some(pk) = doc
        .get("provider_keys")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter().find(|p| {
                p.get("display_name").and_then(Value::as_str) == Some(req.provider_key.as_str())
            })
        })
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("没有名为 {} 的供应商", req.provider_key)})),
        )
            .into_response();
    };

    let provider = pk
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("openai");
    let raw_key = pk
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // resources.yaml 支持 ${VAR}，这里要解出真实值才能调上游。
    //
    // 但变量名必须限定前缀：不限定的话，任何能编辑配置的人写一条
    // `api_key: ${AISIX_ADMIN_KEY_FOR_CONSOLE}` 加上自己的 api_base，
    // 点一下「拉取清单」就能把本服务的管理密钥（或口令散列，拿去离线爆破）
    // 当 Bearer 发到他自己的服务器上。
    let api_key = if let Some(var) = raw_key.strip_prefix("${").and_then(|v| v.strip_suffix("}")) {
        if !var.starts_with("PROVIDER_KEY_") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!(
                    "只允许引用 PROVIDER_KEY_ 前缀的环境变量，收到 {var:?}——                     这条限制是为了让上游凭据无法被用来读取本服务自己的密钥"
                )})),
            )
                .into_response();
        }
        std::env::var(var).unwrap_or_default()
    } else {
        raw_key.to_string()
    };
    let base = pk
        .get("api_base")
        .and_then(Value::as_str)
        .map(|b| b.trim_end_matches('/').to_string())
        .unwrap_or_else(|| match provider {
            "anthropic" => "https://api.anthropic.com/v1".into(),
            "deepseek" => "https://api.deepseek.com/v1".into(),
            _ => "https://api.openai.com/v1".into(),
        });

    // 上游地址必须是 https 且不能指向内网：否则同一个入口就是一把 SSRF——
    // 把 api_base 指向 169.254.169.254 或 127.0.0.1:8081 就能借本服务的
    // 身份去读云元数据或网关自己的管理面。
    if let Err(e) = guard_outbound_base(&base) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }
    let url = format!("{base}/models");
    let mut rb = st.http.get(&url);
    // 两家的鉴权头不同，用错了会拿到 401 而不是清单。
    rb = if provider == "anthropic" {
        rb.header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        rb.header("Authorization", format!("Bearer {api_key}"))
    };

    match rb.send().await {
        Ok(r) => {
            let status = r.status();
            match r.json::<Value>().await {
                Ok(v) if status.is_success() => {
                    // 两家都用 {data:[{id,...}]}，取 id 就够。
                    let ids: Vec<String> = v
                        .get("data")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|m| m.get("id").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    Json(json!({"models": ids, "provider": provider})).into_response()
                }
                // 只回状态码，不回显上游响应体：body 可能是被 SSRF 探到的
                // 内网响应，原样吐回去等于把读取结果送给发起者。
                Ok(_) => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("上游返回 {status}")})),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("上游返回的不是 JSON: {e}")})),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("连不上上游: {e}")})),
        )
            .into_response(),
    }
}

/// 本进程唯一的出站 HTTP 客户端。提成函数是为了让测试打到的就是生产用的
/// 那一个——测试自己 new 一个客户端，证明不了生产客户端的任何事。
fn outbound_client() -> reqwest::Client {
    reqwest::Client::builder()
        // 不跟重定向。`guard_outbound_base` 只能校验第一跳，而 reqwest 默认
        // 跟 10 跳：上游返回 `302 Location: http://169.254.169.254/...` 就整个
        // 绕开了防护。更要命的是凭据——reqwest 跨主机时只剥 Authorization /
        // Cookie / Proxy-Authorization / WWW-Authenticate（redirect.rs:239），
        // `x-api-key` 是自定义头，不在名单里，会被原样转发给重定向目标。
        //
        // 不改成「每跳重跑一次防护」的策略：那个回调是同步的、跑在连接任务
        // 上，而防护里有 DNS 解析。这个入口是去问已知供应商要模型清单，真实
        // 供应商不会对 /v1/models 做重定向，直接不跟即可，3xx 变成运维看得见
        // 的错误。
        .redirect(reqwest::redirect::Policy::none())
        // 显式钉住与网关 `client_builder()` 相同的一组值。不直接复用那个
        // 函数：它读的是网关配置里的全局 `UpstreamHttpConfig`，而控制台是
        // 独立进程、从不加载网关配置。
        //
        // 光有总超时不够：reqwest 默认没有连接超时、关掉 TCP keepalive、
        // 连接池里的空闲连接留 90 秒。负载均衡器回收连接之后，池里那条已经
        // 死掉的连接还会被拿去发下一个请求——表现是偶发失败一次、重试又好。
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(5))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("http client")
}

/// 出站地址防护：只允许 https，且解析后的地址不得落在回环 / 链路本地 /
/// 私网段。DNS 在这里解析一次用于判断——这不能完全消除 DNS rebinding，
/// 但挡住了直接写内网地址这一类，而那才是这个入口的现实风险。
fn guard_outbound_base(base: &str) -> Result<(), String> {
    // 用真正的 URL 解析器，不手写切分。手写版本在 bracketed IPv6 上是坏的：
    // 对 `[::ffff:127.0.0.1]` 从最后一个冒号切会得到 `::ffff`，于是守卫
    // 校验的地址和 reqwest 实际会连的地址根本不是一个——放行还是拒绝纯
    // 属巧合。userinfo（`https://good.example@127.0.0.1/`）是同一类问题。
    let url = reqwest::Url::parse(base).map_err(|e| format!("上游地址无法解析：{e}"))?;
    if url.scheme() != "https" {
        return Err(format!("上游地址必须是 https://，收到 {base:?}"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("上游地址里没有主机名：{base:?}"))?
        .to_string();
    // 直接从解析结果拿 IP 字面量，绕开一次没必要的名字解析；只有真正的
    // 主机名才走 DNS。
    use std::net::ToSocketAddrs;
    let addrs: Vec<std::net::SocketAddr> = match url.host() {
        Some(url::Host::Ipv4(v4)) => vec![(std::net::IpAddr::V4(v4), 443).into()],
        Some(url::Host::Ipv6(v6)) => vec![(std::net::IpAddr::V6(v6), 443).into()],
        _ => (host.as_str(), 443_u16)
            .to_socket_addrs()
            .map_err(|e| format!("无法解析上游主机 {host:?}: {e}"))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(format!("上游主机 {host:?} 没有解析到任何地址"));
    }
    for a in addrs {
        // 先规范化。`::ffff:127.0.0.1` 是回环的另一种写法，但
        // `Ipv6Addr::is_loopback()` 对它返回 false —— 不规范化就等于给
        // 每个 IPv4 内网地址开了一扇 IPv6 写法的后门。网关侧的 CIDR 判定
        // （`models/model.rs`）早就踩过并修过同一个坑，用的就是这个函数。
        let ip = a.ip().to_canonical();
        let bad = match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    || v4.is_multicast()
                    // 运营商级 NAT（100.64.0.0/10）。不在 `is_private()` 里，
                    // 但在云环境里同样是别人的内网。
                    || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    // 唯一本地地址 fc00::/7
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    // 链路本地 fe80::/10
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
        if bad {
            return Err(format!(
                "上游主机 {host:?} 解析到内网地址 {ip}，已拒绝——这个入口不允许指向内部服务"
            ));
        }
    }
    Ok(())
}

// ── 静态页面 ──────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());

    // 子命令先处理，且必须排在所有启动前置条件之前：`hash` 正是用来
    // 生成 CONSOLE_PASSWORD_HASH 的，把它放在那条检查后面就等于要求
    // 先有散列才能生成散列——README 记载的第一步会直接失败。
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "hash" {
        let salt = SaltString::generate(&mut rand_core::OsRng);
        let h = Argon2::default()
            .hash_password(args[2].as_bytes(), &salt)
            .expect("hash");
        println!("{h}");
        return;
    }

    let password_hash = match std::env::var("CONSOLE_PASSWORD_HASH") {
        Ok(h) if !h.is_empty() => h,
        _ => {
            eprintln!("CONSOLE_PASSWORD_HASH 未设置——控制台可以改网关配置，不允许无口令启动。");
            eprintln!("用 `aisix-console hash <口令>` 生成散列。");
            std::process::exit(1);
        }
    };

    let admin_key = match std::env::var("AISIX_ADMIN_KEY_FOR_CONSOLE") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("AISIX_ADMIN_KEY_FOR_CONSOLE 未设置——没有它读不到网关资源。");
            std::process::exit(1);
        }
    };

    let state = AppState {
        admin_url: env("AISIX_ADMIN_URL", "http://127.0.0.1:8081"),
        admin_key,
        portal_url: env("PORTAL_URL", "http://127.0.0.1:8091"),
        // 没配就把门户管理页整个关掉 —— 控制台可以独立部署。
        portal_admin_token: std::env::var("PORTAL_ADMIN_TOKEN").ok(),
        prom_url: env("PROMETHEUS_URL", "http://127.0.0.1:9090"),
        resources_path: PathBuf::from(env("AISIX_RESOURCES", "/etc/aisix/resources.yaml")),
        aisix_bin: PathBuf::from(env("AISIX_BIN", "/usr/local/bin/aisix")),
        password_hash,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        save_lock: Arc::new(tokio::sync::Mutex::new(())),
        login_gate: Arc::new(tokio::sync::Semaphore::new(2)),
        http: outbound_client(),
    };

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/session", get(session_state))
        .route("/api/resources", get(api_resources))
        // 原文入口收的是 YAML 文本，而 YAML 的 alias 展开可以把 2KB 变成 GB
        // （billion laughs）。默认 2MB 的 body 限制远大于这里需要的尺寸，
        // 收紧到 256KB——真实配置离这个量级很远。
        .route(
            "/api/file",
            get(api_file_get)
                .put(api_file_put)
                .layer(axum::extract::DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/api/portal/users", get(portal_users))
        .route("/api/portal/users/{id}/grant", post(portal_grant))
        .route("/api/portal/topups", get(portal_topups))
        .route("/api/portal/topups/{id}/{decision}", post(portal_decide_topup))
        .route("/api/mint-key", post(api_mint_key))
        .route("/api/metrics", post(api_metrics))
        .route("/api/logs", post(api_logs))
        .route("/api/upstream-models", post(api_upstream_models))
        .with_state(state);

    let addr = env("CONSOLE_ADDR", "127.0.0.1:8090");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("无法绑定 {addr}: {e}"));
    println!("aisix-console listening on {addr}");
    axum::serve(listener, app).await.expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 界面在启动时自查契约，但那只挡得住「新界面 + 旧后端」。反方向
    /// 挡不住：旧界面不读契约字段，于是照常运行，并且因为它不带
    /// `base_version` 而落进「缺版本 = 不检查」的逃生口 —— 丢失更新静默
    /// 回来，被覆盖的可能是一次密钥吊销。
    ///
    /// 服务端这条补上那个方向：声明了契约的调用方必须带版本。
    #[test]
    fn a_contract_declaring_client_cannot_skip_the_version() {
        // 旧界面：会声明契约（因为它是界面），但不知道 base_version 存在。
        assert!(
            contract_client_must_send_version(Some(1), None),
            "声明了契约却不带版本 —— 只可能是一个不知道这个字段的旧界面",
        );

        // 正常界面。
        assert!(!contract_client_must_send_version(Some(1), Some("abc")));

        // 脚本 / curl：不声明契约，逃生口继续为它们保留。这是有意的，
        // 拦下它们会让所有既有的非浏览器调用方一夜之间全部失败。
        assert!(!contract_client_must_send_version(None, None));
        assert!(!contract_client_must_send_version(None, Some("abc")));
    }

    /// 契约版本在两个地方各写了一份：这里（权威）和界面的
    /// `web/src/lib/contract.ts`（镜像）。它们在同一个提交里必须相等。
    ///
    /// 这不是重复，而是这套机制成立的前提：只有当任一提交都自洽时，运行时
    /// 的不一致才能被解读为「部署偏移」。两边可以各自漂移的话，界面报出的
    /// 偏移就再也分不清是真偏移还是有人忘了改另一半。
    #[test]
    fn the_frontend_mirrors_this_contract_version() {
        let ts = include_str!("../../../web/src/lib/contract.ts");
        let declared = ts
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("export const EXPECTED_API_CONTRACT = ")
            })
            .and_then(|v| v.trim_end_matches(';').trim().parse::<u32>().ok())
            .expect("web/src/lib/contract.ts 必须导出 EXPECTED_API_CONTRACT");

        assert_eq!(
            declared, API_CONTRACT_VERSION,
            "界面期望契约 v{declared}，后端是 v{API_CONTRACT_VERSION} —— \
             改了接口就要同时改两边，否则界面会对着一个自洽的部署报偏移",
        );
    }

    /// 这个二进制只服务 API。界面是 `web/` 下的 React 应用，构建产物由
    /// nginx 直接托管 —— 它本来就在做 TLS 终结和按路径分流，多一段 root
    /// 比多一层进程转发便宜。
    ///
    /// 曾经不是这样：`/` 上挂过一个 `include_str!` 进来的 HTML。那个形状有
    /// 两处代价，都真实付过：改一行文案要重新编译 Rust 并重启服务（光改文件
    /// 重启没用，因为它已经进了二进制），以及构建产物被迫提交进 git 才能让
    /// 没有 node 的 CI 编出带界面的二进制。
    ///
    /// 所以路由表里不该再出现任何返回 HTML 的项。
    /// 门户的管理凭据必须留在服务端。
    ///
    /// 浏览器拿到它就等于拿到了全部注册用户的账目、以及给任何人发放额度的
    /// 权力 —— 而门户的管理端**不认控制台的会话**（两套凭据刻意互不通用），
    /// 所以那把 token 是唯一的钥匙。
    ///
    /// 这里扫的是源码而不是行为：一旦有人把它塞进某个 JSON 响应里，行为测试
    /// 只有在恰好断言那个字段时才会发现。
    #[test]
    fn the_portal_admin_token_never_reaches_a_response() {
        let src = include_str!("main.rs");
        let production = src
            .split_once("\n#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(src);

        for line in production.lines() {
            let l = line.trim();
            // 只允许它出现在两个地方：读环境变量、以及拼成 Authorization 头。
            if !l.contains("portal_admin_token") {
                continue;
            }
            let sanctioned = l.contains("std::env::var")
                || l.contains("portal_admin_token: ")
                || l.contains("let Some(token) = st.portal_admin_token.clone()");
            assert!(sanctioned, "门户凭据出现在了不该出现的地方: {l}");
        }
        // 它只能经 Authorization 头出去。
        assert!(
            production.contains("format!(\"Bearer {token}\")"),
            "转发时没有把门户凭据放进 Authorization 头",
        );
    }

    /// 转发时不把调用方给的路径段直接拼进 URL。
    ///
    /// 拼的话 `../` 之类能从「确认充值单」走到门户的别的管理端点上 —— 控制台
    /// 持有的是门户的**全权**凭据，那等于把整个管理端交出去。
    #[test]
    fn the_forwarded_path_segment_comes_from_a_whitelist() {
        let src = include_str!("main.rs");
        let body = src
            .split_once("async fn portal_decide_topup(")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(b, _)| b)
            .expect("找不到 portal_decide_topup");
        assert!(
            body.contains(r#"decision != "approve" && decision != "reject""#),
            "转发前没有把路径段限制成白名单",
        );
        // 而且校验必须发生在拼 URL 之前。
        let check = body.find("decision != ").expect("没有校验");
        let build = body.find("/admin/topups/").expect("没有拼 URL");
        assert!(check < build, "先拼了 URL 才校验，等于没校验");
    }

    /// 没配门户时管理页整个关掉，而不是崩掉或假装成功。控制台可以独立部署。
    #[test]
    fn an_absent_portal_is_reported_not_crashed() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("SERVICE_UNAVAILABLE"),
            "门户缺席时没有一个明确的状态码",
        );
        assert!(
            src.contains("未配置自助门户"),
            "门户缺席时没有给出可读的原因",
        );
    }

    /// 门户的两个管理端点都必须先过控制台自己的会话检查。少了它，任何人都能
    /// 匿名读到全部注册用户的邮箱与余额。
    #[test]
    fn portal_admin_routes_require_a_console_session() {
        let src = include_str!("main.rs");
        for f in [
            "async fn portal_users(",
            "async fn portal_grant(",
            "async fn portal_topups(",
            "async fn portal_decide_topup(",
        ] {
            let body = src
                .split_once(f)
                .and_then(|(_, rest)| rest.split_once("\n}\n"))
                .map(|(b, _)| b)
                .unwrap_or_else(|| panic!("找不到 {f}"));
            assert!(
                body.contains("require_auth(&st, &headers).await"),
                "{f} 没有过会话检查",
            );
        }
    }

    #[test]
    fn the_console_serves_only_api_never_a_page() {
        let src = include_str!("main.rs");
        let production = src
            .split_once("\n#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(src);

        assert!(
            !production.contains("include_str!(\"../ui/"),
            "界面又被编进二进制了 —— 前端归 web/，由 nginx 托管",
        );
        assert!(
            !production.contains("Html<"),
            "出现了返回 HTML 的 handler —— 这个二进制只服务 API",
        );

        // 路由白名单：新增一条非 /api 的路由要在这里写明理由。
        let routes: Vec<&str> = production
            .lines()
            .filter_map(|l| l.trim().strip_prefix(".route("))
            .filter_map(|l| l.split('"').nth(1))
            .collect();
        assert!(!routes.is_empty(), "路由提取失效 —— 这条守卫成了空转");
        for r in &routes {
            assert!(
                r.starts_with("/api/") || *r == "/healthz",
                "非 API 路由 {r:?}：如果它是给前端用的，前端应该由 nginx 托管",
            );
        }
    }

    // ── 乐观并发（丢失更新）──────────────────────────────────

    /// 控制台的写是「整份读-改-写」。`save_lock` 只保证两次写不交错，
    /// 拦不住后写者用自己那份旧文档整体覆盖先写者的改动，而且没有提示。
    #[test]
    fn a_write_based_on_an_older_version_is_refused() {
        let disk = sha256_hex(b"config as it is now");
        let stale = sha256_hex(b"config as this tab read it");

        assert!(
            stale_write(Some(&stale), &disk),
            "基于旧版本的写入必须被拦下 —— 放行就是静默丢失别人的改动",
        );
        assert!(
            !stale_write(Some(&disk), &disk),
            "版本一致时不能拦，否则正常保存全部失效",
        );
    }

    /// 没带版本的调用方（curl、脚本）不被拦。这是一个明说的逃生口：
    /// 拦下它会让所有既有的非浏览器调用方一夜之间全部失败。
    #[test]
    fn a_caller_that_sends_no_version_is_not_blocked() {
        assert!(!stale_write(None, &sha256_hex(b"anything")));
    }

    /// 版本必须来自内容而不是时间戳：改回同样长度的内容、或同一秒内的
    /// 两次写，mtime 都分辨不出来。
    #[test]
    fn the_version_tracks_content_not_size_or_time() {
        let a = sha256_hex(b"models: [a]");
        let b = sha256_hex(b"models: [b]");
        assert_ne!(a, b, "等长但不同的内容必须是不同的版本");
        assert_eq!(a, sha256_hex(b"models: [a]"), "同样内容必须是同一版本");
    }

    /// 版本冲突必须映射成 409 而不是 400。前端只在 409 上提示「重新载入」；
    /// 退化成 400 之后它会跟校验失败混在一起，用户看到的是「你写的内容不
    /// 合法」，而真实情况是内容没问题、只是基于了旧版本。
    ///
    /// 这条测试的存在是为了钉住那次重构：判定曾经是按错误文本相等来做的，
    /// 任何人改一个字就会静默退化。
    #[test]
    fn a_version_conflict_is_a_distinct_error_kind_not_a_message_match() {
        let stale = SaveError::Stale;
        let failed = SaveError::Failed("配置校验未通过".into());

        assert!(matches!(stale, SaveError::Stale));
        assert!(!matches!(failed, SaveError::Stale));
        assert!(
            !stale.message().is_empty(),
            "冲突必须给出人能读懂的原因 —— 空消息等于静默失败",
        );
        // 关键性质：文本一模一样也必须还能分辨类型。按文本比对的旧实现
        // 在这里正好会认错——这是它与类型化判定的唯一可观测差别。
        let impostor = SaveError::Failed(SaveError::Stale.message());
        assert_eq!(
            impostor.message(),
            SaveError::Stale.message(),
            "构造前提：两者文本相同",
        );
        assert!(
            !matches!(impostor, SaveError::Stale),
            "同文的普通失败被当成了版本冲突 —— 判定又退回按文本比对了",
        );
    }

    // ── PromQL 注入面 ────────────────────────────────────────

    /// 这个接口把查询串直接转发给 Prometheus。放行一条能读到别的指标的
    /// 查询，就等于把 Prometheus 的整个指标面暴露给控制台会话。
    #[test]
    fn promql_gate_admits_only_this_gateways_own_series() {
        for q in [
            "sum by (model) (aisix_llm_requests_total)",
            "rate(aisix_llm_total_tokens_total[5m])",
            "histogram_quantile(0.95, sum by (le) (aisix_request_duration_bucket))",
        ] {
            assert!(promql_gate_allows(q), "本网关自己的查询被误拦: {q}");
        }

        for q in [
            "up",                                 // Prometheus 自带
            "node_cpu_seconds_total",             // 别的 exporter
            "sum(process_resident_memory_bytes)", // Prometheus 自身进程
            "aisix_llm_requests_total or up",     // 掺一条进来
            "{__name__=~\".+\"}",                 // 全量抓取
        ] {
            assert!(!promql_gate_allows(q), "不该放行: {q}");
        }
    }

    /// 一条不含任何 aisix_ 指标的查询即使全是允许的函数名也必须被拒 ——
    /// 否则 `sum(count(rate(...)))` 这类空壳能探出 Prometheus 的行为。
    #[test]
    fn promql_gate_requires_an_actual_gateway_metric() {
        assert!(!promql_gate_allows("sum(count(1))"));
        assert!(!promql_gate_allows("topk(5, 1)"));
    }

    fn promql_gate_allows(q: &str) -> bool {
        promql_only_touches_aisix(q)
    }

    // ── 出站地址防护（SSRF）──────────────────────────────────

    /// 「同步上游模型」让调用方指定一个地址由服务端去请求。不设防的话
    /// 它就是一个位于网关内网里的任意 GET 代理。
    #[tokio::test]
    async fn outbound_guard_refuses_plaintext_and_internal_targets() {
        for base in [
            "http://api.openai.com/v1",       // 明文
            "https://127.0.0.1/v1",           // 回环
            "https://localhost/v1",           // 回环别名
            "https://10.0.0.5/v1",            // 私网
            "https://192.168.1.1/v1",         // 私网
            "https://169.254.169.254/latest", // 云元数据
            "https://[::1]/v1",               // IPv6 回环
        ] {
            assert!(guard_outbound_base(base).is_err(), "必须拒绝: {base}",);
        }
    }

    /// IPv4-mapped IPv6（`::ffff:127.0.0.1`）是同一个回环地址的另一种写法，
    /// 但 `Ipv6Addr::is_loopback()` 对它是 false。不先规范化的话，这个入口
    /// 就能被指回本机——网关那边在别处已经栽过一次同样的坑，修法是
    /// `to_canonical()`（见 `models/model.rs` 的 CIDR 判定）。
    ///
    /// 另外 IPv6 的唯一本地地址（`fc00::/7`）和链路本地（`fe80::/10`）
    /// 此前完全没查，而它们正是内网。
    #[tokio::test]
    async fn outbound_guard_is_not_fooled_by_ipv6_spellings_of_internal_addresses() {
        for base in [
            "https://[::ffff:127.0.0.1]/v1",           // 回环，写成 IPv4-mapped
            "https://[::ffff:10.0.0.5]/v1",            // 私网，写成 IPv4-mapped
            "https://[::ffff:169.254.169.254]/latest", // 云元数据，同上
            "https://[fd00::1]/v1",                    // IPv6 唯一本地
            "https://[fe80::1]/v1",                    // IPv6 链路本地
        ] {
            assert!(
                guard_outbound_base(base).is_err(),
                "必须拒绝（内网地址的另一种写法）: {base}",
            );
        }
    }

    /// 守卫校验的必须是客户端实际会连的那个主机。手写切分做不到这点：
    /// userinfo 里塞一个看起来正常的域名，真正的主机在 `@` 之后，而按
    /// 冒号切分会把两者混成一个既不是前者也不是后者的字符串——它是否
    /// 恰好解析失败纯属运气，而运气不是防护。
    #[tokio::test]
    async fn outbound_guard_reads_the_host_the_client_will_actually_dial() {
        for base in [
            "https://api.openai.com@127.0.0.1/v1",
            "https://api.openai.com@127.0.0.1:443/v1",
            "https://api.openai.com@[::1]/v1",
        ] {
            assert!(
                guard_outbound_base(base).is_err(),
                "userinfo 掩护下的内网地址必须被拒: {base}",
            );
        }
    }

    /// 运营商级 NAT 段（`100.64.0.0/10`）不在 `is_private()` 里，但在云上
    /// 它同样是别人的内网。
    #[tokio::test]
    async fn outbound_guard_refuses_carrier_grade_nat_space() {
        assert!(guard_outbound_base("https://100.64.0.1/v1").is_err());
    }

    /// `guard_outbound_base` 只能校验第一跳。跟随重定向会让上游用一个
    /// `302 Location:` 把这次请求引到任何地方——防护形同虚设。
    ///
    /// 凭据这一面更糟：reqwest 跨主机只剥 Authorization / Cookie /
    /// Proxy-Authorization / WWW-Authenticate（0.13.4 redirect.rs:239-252），
    /// 而 Anthropic 那条路径用的是 `x-api-key`，属于自定义头，会被原样
    /// 转发给重定向目标。
    ///
    /// 用两个真实监听端口，不是 mock：要证明的正是这个客户端在真实 HTTP
    /// 上的行为。
    #[tokio::test]
    async fn the_outbound_client_does_not_follow_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 第二跳：如果被连上，就说明重定向被跟了。
        let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sink_addr = sink.local_addr().unwrap();
        let leaked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let leaked_key = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        {
            let leaked = leaked.clone();
            let leaked_key = leaked_key.clone();
            tokio::spawn(async move {
                if let Ok((mut c, _)) = sink.accept().await {
                    leaked.store(true, std::sync::atomic::Ordering::SeqCst);
                    let mut buf = vec![0u8; 2048];
                    if let Ok(n) = c.read(&mut buf).await {
                        *leaked_key.lock().unwrap() =
                            String::from_utf8_lossy(&buf[..n]).to_string();
                    }
                }
            });
        }

        // 第一跳：无条件 302 到第二跳。
        let hop = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hop_addr = hop.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut c, _)) = hop.accept().await {
                let mut buf = vec![0u8; 2048];
                let _ = c.read(&mut buf).await;
                let body = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{sink_addr}/pwned\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = c.write_all(body.as_bytes()).await;
            }
        });

        let res = outbound_client()
            .get(format!("http://{hop_addr}/v1/models"))
            .header("x-api-key", "sk-secret-provider-key")
            .send()
            .await
            .expect("first hop answers");

        assert_eq!(
            res.status(),
            302,
            "重定向必须原样返回给调用方，而不是被悄悄跟掉",
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !leaked.load(std::sync::atomic::Ordering::SeqCst),
            "重定向目标被连上了 —— 防护只校验了第一跳，这是一把 SSRF",
        );
        assert!(
            !leaked_key
                .lock()
                .unwrap()
                .contains("sk-secret-provider-key"),
            "供应商密钥被转发给了重定向目标",
        );
    }

    // ── 日志行解析 ──────────────────────────────────────────

    /// 日志页把 journald 的行拆成键值展示。拆错不会报错，只会安静地
    /// 少显示字段，所以这里把形状钉死。
    #[test]
    fn log_line_parsing_keeps_quoted_values_intact() {
        let m = parse_kv(r#"level=INFO model="gpt-4o mini" status=200"#);
        assert_eq!(m.get("level").and_then(Value::as_str), Some("INFO"));
        assert_eq!(
            m.get("model").and_then(Value::as_str),
            Some("gpt-4o mini"),
            "带空格的引号值被从中间截断了",
        );
        assert_eq!(m.get("status").and_then(Value::as_str), Some("200"));
    }

    // ── 密钥铸造 ────────────────────────────────────────────

    /// 落盘的只能是散列。这条测试存在的意义是：明文一旦写进 resources.yaml
    /// 就再也收不回来，而这个错误在界面上完全看不出来。
    #[test]
    fn minted_keys_are_stored_as_hashes_only() {
        let plaintext = random_token();
        let stored = sha256_hex(plaintext.as_bytes());
        assert_eq!(stored.len(), 64, "sha256 十六进制应为 64 字符");
        assert!(!stored.contains(&plaintext), "散列里不能含明文");
        assert_ne!(stored, plaintext);
    }

    /// 两次铸造不能撞。撞了意味着两个调用方共用一把密钥，而其中一方
    /// 被吊销时另一方也一起失效 —— 没人会往这个方向排查。
    #[test]
    fn minted_keys_do_not_repeat() {
        let n = 256;
        let set: std::collections::HashSet<String> = (0..n).map(|_| random_token()).collect();
        assert_eq!(set.len(), n, "生成的密钥出现重复");
    }
}
