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
                .into_response()
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
                .into_response()
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

    let provider = pk.get("provider").and_then(Value::as_str).unwrap_or("openai");
    let raw_key = pk.get("api_key").and_then(Value::as_str).unwrap_or_default();
    // resources.yaml 支持 ${VAR}，这里要解出真实值才能调上游。
    let api_key = if let Some(var) = raw_key.strip_prefix("${").and_then(|v| v.strip_suffix("}")) {
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
                Ok(v) => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("上游 {status}: {v}")})),
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
