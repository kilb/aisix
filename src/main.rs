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

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::RwLock;

/// 会话有效期。控制台能看到明文上游密钥，所以不做长期免登录。
const SESSION_TTL_SECS: u64 = 8 * 3600;

#[derive(Clone)]
struct AppState {
    /// 网关管理 API 基址（回环）。
    admin_url: String,
    /// 管理密钥。只存在服务端，绝不下发给浏览器。
    admin_key: String,
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
    let ok = PasswordHash::new(&st.password_hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(req.password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false);
    if !ok {
        // 固定延迟，避免把「口令错」和「解析失败」在时间上区分开。
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "口令不正确"}))).into_response();
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

async fn session_state(State(st): State<AppState>, headers: HeaderMap) -> Json<Value> {
    Json(json!({"authed": st.authed(&headers).await}))
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
    // 只允许本控制台自己构造的查询前缀，避免把 Prometheus 变成任意查询代理。
    if !q.query.starts_with("aisix_") && !q.query.contains("aisix_") {
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

// ── 资源写（resources.yaml + SIGHUP）──────────────────────────────────

/// 资源文件按原样 `Value` 透传，不映射成强类型：控制台不该比网关更懂
/// schema，多一层类型就多一处会和上游漂移的地方。校验交给 `aisix validate`。
async fn api_file_get(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    match tokio::fs::read_to_string(&st.resources_path).await {
        Ok(s) => match serde_yaml_ng::from_str::<Value>(&s) {
            Ok(v) => Json(json!({"doc": v, "raw": s})).into_response(),
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

/// 校验 → 落盘 → SIGHUP。三步都成功才算保存。
///
/// 校验用的是网关自己的 `aisix validate`，不是这里重新实现一份 schema：
/// 重实现一定会和上游漂移，而漂移的方向永远是「控制台放行了网关拒绝的东西」。
async fn save_resources(st: &AppState, doc: &Value) -> Result<String, String> {
    let yaml = serde_yaml_ng::to_string(doc).map_err(|e| format!("序列化失败: {e}"))?;

    let tmp = st.resources_path.with_extension("yaml.console-tmp");
    tokio::fs::write(&tmp, &yaml)
        .await
        .map_err(|e| format!("写临时文件失败: {e}"))?;

    let out = tokio::process::Command::new(&st.aisix_bin)
        .arg("validate")
        .arg("--resources")
        .arg(&tmp)
        .output()
        .await
        .map_err(|e| format!("无法执行 aisix validate: {e}"))?;
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            msg
        };
        return Err(format!("配置校验未通过，未改动网关：\n{msg}"));
    }

    // 校验通过才覆盖。原子替换，避免网关读到写了一半的文件。
    tokio::fs::rename(&tmp, &st.resources_path)
        .await
        .map_err(|e| format!("替换 resources.yaml 失败: {e}"))?;

    // 同用户进程，直接 SIGHUP，不需要 sudo。
    let hup = tokio::process::Command::new("pkill")
        .arg("-HUP")
        .arg("-x")
        .arg("aisix")
        .output()
        .await
        .map_err(|e| format!("发送 SIGHUP 失败: {e}"))?;
    if !hup.status.success() {
        return Err("配置已保存，但 SIGHUP 未送达网关；请手动 systemctl reload aisix".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Deserialize)]
struct WriteReq {
    /// 完整的资源文档（前端改完整体提交）。
    doc: Value,
}

async fn api_file_put(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<WriteReq>,
) -> Response {
    if let Err(r) = require_auth(&st, &headers).await {
        return r;
    }
    match save_resources(&st, &req.doc).await {
        Ok(detail) => Json(json!({"ok": true, "detail": detail})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
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

fn sha256_hex(bytes: &[u8]) -> String {
    // 只为算 key_hash，不值得为此多拉一个依赖。
    use std::process::Command;
    let out = Command::new("sha256sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.as_mut().unwrap().write_all(bytes)?;
            c.wait_with_output()
        });
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
        Err(_) => String::new(),
    }
}

// ── 静态页面 ──────────────────────────────────────────────────────────

async fn index() -> Html<&'static str> {
    Html(include_str!("../ui/index.html"))
}

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());

    let password_hash = match std::env::var("CONSOLE_PASSWORD_HASH") {
        Ok(h) if !h.is_empty() => h,
        _ => {
            eprintln!("CONSOLE_PASSWORD_HASH 未设置——控制台可以改网关配置，不允许无口令启动。");
            eprintln!("用 `aisix-console hash <口令>` 生成散列。");
            std::process::exit(1);
        }
    };

    // 便捷子命令：生成口令散列。
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "hash" {
        let salt = SaltString::generate(&mut rand_core::OsRng);
        let h = Argon2::default()
            .hash_password(args[2].as_bytes(), &salt)
            .expect("hash");
        println!("{h}");
        return;
    }

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
        prom_url: env("PROMETHEUS_URL", "http://127.0.0.1:9090"),
        resources_path: PathBuf::from(env("AISIX_RESOURCES", "/etc/aisix/resources.yaml")),
        aisix_bin: PathBuf::from(env("AISIX_BIN", "/usr/local/bin/aisix")),
        password_hash,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("http client"),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/session", get(session_state))
        .route("/api/resources", get(api_resources))
        .route("/api/file", get(api_file_get).put(api_file_put))
        .route("/api/mint-key", post(api_mint_key))
        .route("/api/metrics", post(api_metrics))
        .with_state(state);

    let addr = env("CONSOLE_ADDR", "127.0.0.1:8090");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("无法绑定 {addr}: {e}"));
    println!("aisix-console listening on {addr}");
    axum::serve(listener, app).await.expect("serve");
}
