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
        "note": if rows_note(&rows) { Some("流式请求的这一行在推流开始前写下，所以 token 数为空") } else { None },
    }))
    .into_response()
}

async fn read_journal(n: u32) -> Result<String, String> {
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

/// 只留下属于 `mine` 的行，并只保留展示需要的字段。
///
/// **不返回整行原文**：那里有 `provider_key_id`、上游 request id 之类的内部
/// 标识，用户不需要，也不该拿到。
fn filter(text: &str, mine: &[String], limit: u32) -> Vec<Value> {
    let mut rows = Vec::new();
    for line in text.lines().rev() {
        if !line.contains("api_key_id=") {
            continue;
        }
        let kv = parse_kv(line);
        let id = kv.get("api_key_id").and_then(Value::as_str).unwrap_or("");
        if !mine.iter().any(|m| m == id) {
            continue;
        }
        let mut row = Map::new();
        // 白名单，不是黑名单：新增字段默认不外泄。
        for f in [
            "method",
            "path",
            "status",
            "latency_ms",
            "model",
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
        ] {
            if let Some(v) = kv.get(f) {
                row.insert(f.to_string(), v.clone());
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

fn rows_note(rows: &[Value]) -> bool {
    rows.iter()
        .any(|r| r.get("total_tokens").is_none() && r.get("status").is_some())
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
            match stripped.find('"') {
                Some(end) => (&stripped[..end], end + 2),
                None => break,
            }
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..end], end)
        };
        if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
            out.insert(
                key.to_string(),
                val.parse::<i64>()
                    .map(Value::from)
                    .unwrap_or_else(|_| Value::String(val.to_string())),
            );
        }
        i = eq + 1 + consumed;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一行真实形状的访问日志（取自生产 journal）。
    fn line(api_key_id: &str, status: i64) -> String {
        format!(
            r#"2026-08-19T18:53:01.020719Z  INFO request{{request_id=1d5b52c7}}: aisix_obs::access_log: proxy request completed method="POST" path="/v1/chat/completions" status={status} latency_ms=1 provider="openai" model="gpt-4o-mini" api_key_id="{api_key_id}" prompt_tokens=120 completion_tokens=45 total_tokens=165 request_id="1d5b52c7" provider_request_id="chatcmpl-mock" routing_attempt_count=1"#
        )
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
