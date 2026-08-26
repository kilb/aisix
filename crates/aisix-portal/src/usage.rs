//! 用量查询。
//!
//! **端点不接受调用方提供的查询。** 租户隔离是端点的形状，不是一道过滤器：
//! 查询模板写死在服务端，只有窗口长度是参数，`user_id` 从会话注入。
//!
//! 现有控制台的 `/api/metrics` 是任意 PromQL 透传（只校验指标名以 `aisix_`
//! 开头）。单管理员下无害 —— 唯一的用户就是有权看全部数据的人。多租户下那是
//! 跨租户泄漏：任一用户发一条 `sum by (user_id) (...)` 就读到了所有人的花费。
//! 所以门户不复用它。

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AppState;
use crate::ledger::Ledger;
use crate::resources;

/// 窗口长度。收成枚举式的白名单，而不是把数字直接拼进 PromQL。
#[derive(Debug, Clone, Copy)]
enum Window {
    H1,
    H24,
    D7,
}

impl Window {
    fn from_hours(h: Option<u32>) -> Self {
        match h {
            Some(n) if n <= 1 => Self::H1,
            Some(n) if n > 24 => Self::D7,
            _ => Self::H24,
        }
    }
    fn range(self) -> &'static str {
        match self {
            Self::H1 => "1h",
            Self::H24 => "24h",
            Self::D7 => "7d",
        }
    }
}

#[derive(Deserialize)]
pub struct UsageParams {
    #[serde(default)]
    pub range_hours: Option<u32>,
}

/// `GET /api/usage`
pub async fn usage(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<UsageParams>,
) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "未登录"}))).into_response();
    };

    let win = Window::from_hours(p.range_hours);
    let keys = match st.resources().read().await {
        Ok(yaml) => resources::tally_keys(&yaml, &uid),
        // 读不到配置时不假装「有绑定」。
        Err(_) => resources::KeyTally {
            total: 0,
            disabled: 0,
        },
    };

    let requests = st
        .prom_scalar(&query_for("aisix_llm_requests_total", &uid, win))
        .await;
    let tokens = st
        .prom_scalar(&query_for("aisix_llm_total_tokens_total", &uid, win))
        .await;
    let spend = st
        .prom_scalar(&query_for("aisix_llm_spend_micro_usd_total", &uid, win))
        .await;

    Json(json!({
        "range": win.range(),
        "linked_keys": keys.total,
        "disabled_keys": keys.disabled,
        "requests": requests,
        "tokens": tokens,
        "spend_micro_usd": spend,
        // 一期的密钥由管理员手工创建并填 user_id。数出 0 就明确说出来，
        // 否则「用量一直是 0」跟「还没开始用」在屏幕上没有区别。
        "note": if keys.total == 0 {
            Some("未绑定任何密钥：请让管理员创建密钥并填上你的用户 ID")
        } else {
            None
        },
    }))
    .into_response()
}

/// 拼一条固定形状的查询。**只有指标名和窗口来自服务端的白名单**，
/// `user_id` 是唯一的外部输入，且经过 PromQL 标签值转义。
fn query_for(metric: &str, user_id: &str, win: Window) -> String {
    format!(
        "sum(increase({metric}{{user_id=\"{}\"}}[{}]))",
        escape_label(user_id),
        win.range()
    )
}

/// PromQL 标签值转义。
///
/// 这里的 `user_id` 是我们自己铸的 uuid v4，理论上不含需要转义的字符。仍然转，
/// 因为「这个值的来源永远安全」是一个会随着代码变化而失效的假设，而转义的成本
/// 是零。
pub(crate) fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 把 Prometheus 的即时向量应答取成一个标量。
pub fn scalar_from_prom(body: &Value) -> Option<f64> {
    body.get("data")?
        .get("result")?
        .as_array()?
        .first()?
        .get("value")?
        .as_array()?
        .get(1)?
        .as_str()?
        .parse()
        .ok()
}

/// 与用量查询同一条规矩：`user_id` 只从会话取。
pub async fn balance(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "未登录"}))).into_response();
    };
    let ledger = Ledger::new(st.store.clone());
    let (Ok(balance), Ok(entries)) = (ledger.balance(&uid).await, ledger.entries(&uid).await)
    else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "读取失败"})),
        )
            .into_response();
    };
    Json(json!({
        "balance_micro_usd": balance,
        "entries": entries.iter().map(|e| json!({
            "id": e.id,
            "delta_micro_usd": e.delta_micro_usd,
            "source": e.source,
            "note": e.note,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 窗口来自白名单_不把调用方给的数字拼进查询() {
        // 任何窗口输入都只能落到三个固定值之一。
        for h in [0u32, 1, 5, 24, 999, u32::MAX] {
            let q = query_for(
                "aisix_llm_requests_total",
                "u1",
                Window::from_hours(Some(h)),
            );
            assert!(
                q.ends_with("[1h]))") || q.ends_with("[24h]))") || q.ends_with("[7d]))"),
                "窗口没落在白名单里: {q}"
            );
        }
    }

    #[tokio::test]
    async fn 标签值里的引号与反斜杠被转义() {
        let q = query_for("m", r#"u"1\x"#, Window::H24);
        // 未转义的裸引号会提前闭合标签，把后面的内容变成查询的一部分。
        assert!(q.contains(r#"user_id="u\"1\\x""#), "{q}");
    }

    #[tokio::test]
    async fn 查询里带的是传入的_user_id() {
        let q = query_for("aisix_llm_spend_micro_usd_total", "abc-123", Window::H24);
        assert_eq!(
            q,
            r#"sum(increase(aisix_llm_spend_micro_usd_total{user_id="abc-123"}[24h]))"#
        );
    }

    #[tokio::test]
    async fn 应答解析取到第一条序列的值() {
        let body: Value = serde_json::from_str(
            r#"{"status":"success","data":{"resultType":"vector",
                "result":[{"metric":{},"value":[0,"1234.5"]}]}}"#,
        )
        .unwrap();
        assert_eq!(scalar_from_prom(&body), Some(1234.5));
    }

    #[tokio::test]
    async fn 空应答解析成_none_而不是零() {
        let body: Value =
            serde_json::from_str(r#"{"status":"success","data":{"result":[]}}"#).unwrap();
        // 「没读到」和「是零」必须分得开：前者是读取问题，后者是真没流量。
        assert_eq!(scalar_from_prom(&body), None);
    }
}
