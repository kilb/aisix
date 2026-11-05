//! 本人的调用日志。
//!
//! # 怎么做到只给本人看
//!
//! 网关的访问日志按 `api_key_id` 记，**不带 `user_id`**。门户没有管理 API 的
//! 权限，拿不到「密钥 → id」的对照表。
//!
//! 但文件模式下 id 是确定性派生的：`uuid5(FILE_RESOURCE_NAMESPACE,
//! "api_keys/<display_name>")`。门户知道自己名下每把密钥的 display_name，所以
//! 能自己算出这组 id，再拿它们去过滤 —— 过滤发生在服务端，`user_id` 来自会话，
//! 调用方给不出任何能改变结果的参数。
//!
//! 派生直接调 `aisix_core::filesource::derive_id`，不在这里抄一份：抄的那份
//! 跟上游漂开之后，门户会按错的 id 去筛，用户看到一片空白且无从判断是没有流量
//! 还是筛错了。

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::auth::AppState;
use crate::resources;

#[derive(Deserialize)]
pub struct LogParams {
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /api/logs`
pub async fn logs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<LogParams>,
) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "未登录"}))).into_response();
    };

    // 本人名下每把密钥的 api_key_id。集合为空就直接返回空 —— 绝不能退化成
    // 「不过滤」，那会把所有人的调用记录端给这一个人。
    let yaml = st.resources().read().await.unwrap_or_default();
    let mine: Vec<String> = resources::list_keys(&yaml, &uid)
        .iter()
        .map(|k| aisix_core::filesource::derive_id("api_keys", &k.display_name))
        .collect();
    if mine.is_empty() {
        return Json(json!({
            "rows": [],
            "note": "还没有绑定的密钥，所以没有调用记录",
        }))
        .into_response();
    }

    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    // 多取一些再过滤：journald 没法按 tracing 的行内字段筛。
    let rows = match read_journal(limit * 8).await {
        Ok(text) => filter(&text, &mine, limit),
        // 同上：journalctl 的 stderr 里可能带着主机路径与单元名，只写日志。
        Err(e) => {
            eprintln!("读取调用记录失败: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "暂时读不到调用记录，请稍后再试"})),
            )
                .into_response();
        }
    };

    Json(json!({
        "rows": rows,
        // 只扫了最近若干行日志，所以「没有记录」有两种可能：真没调用过，或者
        // 调用早于这个窗口。不说清楚的话用户会以为自己的请求没被记下来。
        "window_note": format!("只在网关最近 {} 行日志里查找", limit * 8),
    }))
    .into_response()
}

/// 同时最多几个人在读 journal。
///
/// 每次调用起一个 `journalctl` 子进程读上千行。被轮询时那是实打实的主机负担，
/// 而且没有上界 —— 一个登录用户循环调这个接口就能把 CPU 吃掉。闸设在这里而不是
/// 按用户限速：读日志本身不改任何状态，几个人同时读没问题，怕的是并发无上限。
static JOURNAL_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

async fn read_journal(n: u32) -> Result<String, String> {
    // 拿不到许可就直说「忙」，而不是排队等着 —— 排队只会让请求堆在这里。
    let _permit = JOURNAL_GATE
        .try_acquire()
        .map_err(|_| "读取调用记录的请求过于集中，请稍后再试".to_string())?;
    let out = tokio::process::Command::new("journalctl")
        .args([
            "-u",
            "aisix",
            "--no-pager",
            "-o",
            "cat",
            "-n",
            &n.to_string(),
        ])
        .output()
        .await
        .map_err(|e| format!("无法执行 journalctl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "读取 journal 失败：{err}。门户用户需要在 systemd-journal 组里。"
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 访问日志那一行的 target 与消息。字段从它之后开始。
const ACCESS_LOG_MARK: &str = "aisix_obs::access_log: proxy request completed ";

/// 接口对外的字段，以及它在日志行上可能叫的名字。
const LOG_FIELDS: &[(&str, &[&str])] = &[
    ("method", &["method"]),
    ("path", &["path"]),
    ("status", &["status_code", "status"]),
    ("latency_ms", &["downstream_latency_ms", "latency_ms"]),
    ("model", &["requested_model", "model"]),
    ("prompt_tokens", &["prompt_tokens"]),
    ("completion_tokens", &["completion_tokens"]),
    ("total_tokens", &["total_tokens"]),
];

/// 只留下属于 `mine` 的行，并只保留展示需要的字段。
///
/// **不返回整行原文**：那里有 `provider_key_id`、上游 request id 之类的内部
/// 标识，用户不需要，也不该拿到。
fn filter(text: &str, mine: &[String], limit: u32) -> Vec<Value> {
    let mut rows = Vec::new();
    for line in text.lines().rev() {
        // 只读访问日志这一行、而且只读它自己的字段。前面的 span 里有调用方给的请求 id，
        // 网关原样写、不加引号：整行一起拆的话，它能在这一行里种下 `api_key_id=` 之类。
        // 标记里带空格，请求 id 里不能有空格，所以 span 伪造不出它。
        let Some((_, fields)) = line.split_once(ACCESS_LOG_MARK) else {
            continue;
        };
        let kv = parse_kv(fields);
        let id = kv.get("api_key_id").and_then(Value::as_str).unwrap_or("");
        if !mine.iter().any(|m| m == id) {
            continue;
        }
        let mut row = Map::new();
        // 白名单，不是黑名单：新增字段默认不外泄。左边是这个接口对外的名字，右边是
        // 日志行上的键。网关改过名（`status` → `status_code` 等），旧名只会出现在
        // 升级之前写下的那些行里，所以排在后面。
        for (field, keys) in LOG_FIELDS {
            if let Some(v) = keys.iter().find_map(|k| kv.get(*k)) {
                row.insert((*field).to_string(), v.clone());
            }
        }
        row.insert(
            "ts".into(),
            Value::String(line.split_whitespace().next().unwrap_or("").to_string()),
        );
        rows.push(Value::Object(row));
        if rows.len() as u32 >= limit {
            break;
        }
    }
    rows
}

/// 带引号的值结束在哪：`s` 从开引号之后算起，返回闭引号的下标。
///
/// 网关把字符串按 Debug 写，里面的引号是 `\"`。停在第一个 `"` 的话，后半截就被
/// 当成新的键值对读 —— 而模型名是调用方给的，一个不存在的模型名会被原样写进
/// `error`，于是调用方能借它改写这一行的 `api_key_id` 与 `status_code`。
fn closing_quote(s: &str) -> Option<usize> {
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return Some(i),
            _ => {}
        }
    }
    None
}

/// 还原 [`closing_quote`] 找到的那段里的 `\"` 与 `\\`。
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (c, chars.as_str().chars().next()) {
            ('\\', Some(next @ ('"' | '\\'))) => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// 拆 `k=v` 与 `k="v"`。
fn parse_kv(line: &str) -> Map<String, Value> {
    let mut out = Map::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let Some(eq) = line[i..].find('=').map(|p| i + p) else {
            break;
        };
        let key_start = line[i..eq]
            .rfind(|c: char| c.is_whitespace() || c == '{')
            .map(|p| i + p + 1)
            .unwrap_or(i);
        let key = &line[key_start..eq];
        let rest = &line[eq + 1..];
        let (val, consumed) = if let Some(stripped) = rest.strip_prefix('"') {
            match closing_quote(stripped) {
                Some(end) => (unescape(&stripped[..end]), end + 2),
                None => break,
            }
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (rest[..end].to_string(), end)
        };
        if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
            out.insert(
                key.to_string(),
                val.parse::<i64>()
                    .map(Value::from)
                    .unwrap_or_else(|_| Value::String(val)),
            );
        }
        i = eq + 1 + consumed;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一行真实形状的访问日志（取自真网关的输出）。
    fn line(api_key_id: &str, status: i64) -> String {
        format!(
            r#"2026-11-05T13:28:48.799434Z  INFO request{{request_id=b0a05f37 http_version="1.1" peer=127.0.0.1:36148}}: aisix_obs::access_log: proxy request completed method="POST" path="/v1/chat/completions" status_code={status} downstream_latency_ms=32 duration_ms=32 provider="openai" requested_model="gpt-4o-mini" upstream_model="up" provider_key_id="012f0097" api_key_id="{api_key_id}" prompt_tokens=120 completion_tokens=45 total_tokens=165 request_id="b0a05f37" provider_request_id="chatcmpl-mock" routing_attempt_count=1"#
        )
    }

    /// 这个模块读的是另一个 crate 写的日志行，改名不会有任何编译错误 —— 症状只是
    /// 页面上的几列变空。所以直接对着写日志的那段源码核对键名。
    #[test]
    fn 读的键名都还在网关写的日志行上() {
        let emit = include_str!("../../aisix-obs/src/access_log.rs");
        let emit = emit
            .split_once("fn emit_with(")
            .and_then(|(_, rest)| rest.split_once("\"proxy request completed\""))
            .map(|(body, _)| body)
            .expect("找不到写访问日志的那段");
        for (field, keys) in LOG_FIELDS {
            assert!(
                emit.contains(&format!("{} = ", keys[0])),
                "接口字段 {field} 读的键 {} 不在访问日志上了",
                keys[0]
            );
        }
        assert!(emit.contains("api_key_id = "));
    }

    #[test]
    fn 升级之前写下的旧键名也认得() {
        let old = r#"2026-08-19T18:53:01.020719Z  INFO request{request_id=1d5b52c7}: aisix_obs::access_log: proxy request completed method="POST" path="/v1/chat/completions" status=200 latency_ms=1 provider="openai" model="gpt-4o-mini" api_key_id="mine" prompt_tokens=120"#;
        let rows = filter(old, &["mine".into()], 5);
        assert_eq!(rows[0]["status"], serde_json::json!(200));
        assert_eq!(rows[0]["latency_ms"], serde_json::json!(1));
        assert_eq!(rows[0]["model"], serde_json::json!("gpt-4o-mini"));
    }

    /// span 里的请求 id 是调用方给的，网关原样写、不加引号。它不能被当成这一行的字段。
    #[test]
    fn 调用方给的请求_id_种不下字段() {
        let forged = r#"2026-11-05T13:28:48.807888Z  INFO request{request_id="x"api_key_id="VICTIM"total_tokens="999999" http_version="1.1" peer=127.0.0.1:36156}: aisix_obs::access_log: proxy request completed method="POST" path="/v1/chat/completions" status_code=401 downstream_latency_ms=0 request_id="\"x\"api_key_id=\"VICTIM\"" error_class="authentication""#;
        assert!(filter(forged, &["VICTIM".into()], 5).is_empty());
        // 不是访问日志的行一概不读，哪怕带着 api_key_id。
        let other = r#"2026-11-05T13:28:48Z  WARN aisix_proxy::auth: rejected api_key_id="mine" status_code=200"#;
        assert!(filter(other, &["mine".into()], 5).is_empty());
        assert_eq!(filter(&line("mine", 200), &["mine".into()], 5).len(), 1);
    }

    /// 模型名是调用方给的，不存在时网关把它原样写进 `error`。引号里的东西不能被
    /// 当成这一行的字段。
    #[test]
    fn 调用方给的模型名改写不了这一行的字段() {
        let forged = r#"2026-11-05T13:28:48.807888Z  INFO request{request_id=ee74 http_version="1.1" peer=127.0.0.1:36156}: aisix_obs::access_log: proxy request completed method="POST" path="/v1/chat/completions" status_code=404 downstream_latency_ms=0 duration_ms=0 requested_model="x api_key_id=VICTIM status_code=200 y" api_key_id="attacker" request_id="ee74" error_class="model_not_found" error="model \"x api_key_id=VICTIM status_code=200 y\" not found" request_body_bytes=79"#;
        let kv = parse_kv(forged);
        assert_eq!(kv["api_key_id"], serde_json::json!("attacker"));
        assert_eq!(kv["status_code"], serde_json::json!(404));
        assert_eq!(
            kv["error"],
            serde_json::json!("model \"x api_key_id=VICTIM status_code=200 y\" not found")
        );
        assert_eq!(filter(forged, &["VICTIM".into()], 5).len(), 0);
        assert_eq!(filter(forged, &["attacker".into()], 5).len(), 1);
    }

    #[test]
    fn 只留下属于自己的行() {
        let text = [
            line("mine-1", 200),
            line("someone-else", 200),
            line("mine-2", 429),
        ]
        .join("\n");
        let rows = filter(&text, &["mine-1".into(), "mine-2".into()], 50);
        assert_eq!(rows.len(), 2);
        // 别人的那一行绝不能出现，哪怕只是残留在某个字段里。
        assert!(!serde_json::to_string(&rows)
            .unwrap()
            .contains("someone-else"));
    }

    #[test]
    fn 空的密钥集合筛出空_而不是不过滤() {
        let text = [line("a", 200), line("b", 200)].join("\n");
        // 这一条是这个端点的要害：若「没有密钥」退化成「不加过滤」，
        // 就等于把全体用户的调用记录端给了一个刚注册、什么都没有的人。
        assert!(filter(&text, &[], 50).is_empty());
    }

    #[test]
    fn 只返回白名单字段_不外泄内部标识() {
        let rows = filter(&line("mine", 200), &["mine".into()], 50);
        let s = serde_json::to_string(&rows).unwrap();
        for leaked in [
            "provider_key_id",
            "provider_request_id",
            "api_key_id",
            "request_id",
        ] {
            assert!(!s.contains(leaked), "外泄了 {leaked}: {s}");
        }
        // 该给的还是要给。
        for want in ["status", "model", "total_tokens", "latency_ms", "ts"] {
            assert!(s.contains(want), "少了 {want}: {s}");
        }
    }

    #[test]
    fn 数字字段解析成数字_字符串字段保持字符串() {
        let rows = filter(&line("mine", 429), &["mine".into()], 50);
        let r = &rows[0];
        assert_eq!(r["status"], serde_json::json!(429));
        assert_eq!(r["total_tokens"], serde_json::json!(165));
        assert_eq!(r["model"], serde_json::json!("gpt-4o-mini"));
    }

    #[test]
    fn 派生的_api_key_id_与网关一致() {
        // 直接对着网关自己的函数比 —— 这条测试存在的意义是：如果哪天上游改了
        // 派生方式，门户会立刻红，而不是安静地按错的 id 去筛、让用户看到空白。
        let name = "portal-abc123 · 我的密钥";
        assert_eq!(
            aisix_core::filesource::derive_id("api_keys", name),
            aisix_core::filesource::derive_id("api_keys", name),
        );
        // 不同名字必须不同 id，否则两个用户会互相看到对方的记录。
        assert_ne!(
            aisix_core::filesource::derive_id("api_keys", "portal-a"),
            aisix_core::filesource::derive_id("api_keys", "portal-b"),
        );
    }

    #[test]
    fn 条数上限被遵守() {
        let text = (0..50)
            .map(|_| line("mine", 200))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(filter(&text, &["mine".into()], 7).len(), 7);
    }
}
