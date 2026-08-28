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

/// Prometheus 即时向量应答的三种结果。
///
/// **「没有序列」跟「读不到」必须分开。** Prometheus 对一个匹配不到任何序列的
/// 查询会正常应答 `status: success` + 空结果 —— 那意味着这个用户从来没产生过
/// 流量，对账口径下就是 0。混成「读不到」的后果是：水位线永不推进，查询窗口
/// 一天天变长，而失败计数每轮都涨。生产上真发生过，是门户的指标口刚上线就
/// 把它暴露出来的。
#[derive(Debug, PartialEq)]
pub enum PromScalar {
    /// 读到了一个数。
    Value(f64),
    /// Prometheus 答了，但这个查询没有任何序列 —— 也就是 0。
    NoSeries,
    /// 没能问到：不是 JSON、`status` 不是 success、或者形状不对。
    Unreadable,
}

/// 把 Prometheus 的即时向量应答分类。
pub fn scalar_of(body: &Value) -> PromScalar {
    // `status` 是 Prometheus 自己对这次查询成败的判断，先看它。
    if body.get("status").and_then(Value::as_str) != Some("success") {
        return PromScalar::Unreadable;
    }
    let Some(result) = body.get("data").and_then(|d| d.get("result")) else {
        return PromScalar::Unreadable;
    };
    let Some(items) = result.as_array() else {
        return PromScalar::Unreadable;
    };
    if items.is_empty() {
        return PromScalar::NoSeries;
    }
    match items
        .first()
        .and_then(|i| i.get("value"))
        .and_then(Value::as_array)
        .and_then(|v| v.get(1))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
    {
        Some(v) => PromScalar::Value(v),
        // 有序列却取不出值 —— 形状跟预期不符，当成读不到。
        None => PromScalar::Unreadable,
    }
}

/// 余额页展示的流水条数上限。
///
/// 对账环给有消费的用户每轮写一条，所以这张表只增不减；不限条数的话，几周之后
/// 一次余额请求要吐十几万条，页面先卡死。总额与余额不受影响 —— 那是 `SUM` 出来
/// 的，永远算全部流水。
const ENTRY_PAGE: i64 = 200;

/// 与用量查询同一条规矩：`user_id` 只从会话取。
pub async fn balance(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(uid) = st.session_user(&headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "未登录"}))).into_response();
    };
    let ledger = Ledger::new(st.store.clone());
    let (Ok(balance), Ok(entries)) = (
        ledger.balance(&uid).await,
        ledger.entries(&uid, ENTRY_PAGE).await,
    ) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "读取失败"})),
        )
            .into_response();
    };
    Json(json!({
        "balance_micro_usd": balance,
        // 截断了就说出来。不说的话，用户翻不到更早的记录却以为那就是全部。
        "entries_truncated": entries.len() as i64 >= ENTRY_PAGE,
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
        assert_eq!(scalar_of(&body), PromScalar::Value(1234.5));
    }

    /// 「没有序列」「读不到」「读到了」三者必须分开。
    ///
    /// 空结果曾经被当成「读不到」：于是一个从未产生流量的用户会让水位线永远
    /// 停在原地，查询窗口一天天变长，失败计数每轮都涨 —— 生产上真发生了，是
    /// 门户的指标口刚上线就把它照出来的。
    #[tokio::test]
    async fn 没有序列不等于读不到() {
        let empty: Value = serde_json::from_str(
            r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#,
        )
        .unwrap();
        assert_eq!(scalar_of(&empty), PromScalar::NoSeries);

        // Prometheus 自己说查询失败 —— 这才是读不到。
        let err: Value = serde_json::from_str(
            r#"{"status":"error","errorType":"bad_data","error":"parse error"}"#,
        )
        .unwrap();
        assert_eq!(scalar_of(&err), PromScalar::Unreadable);

        // 形状不对也是读不到，而不是 0。
        for junk in [
            r#"{"status":"success"}"#,
            r#"{"status":"success","data":{"result":{}}}"#,
            r#"{"status":"success","data":{"result":[{"metric":{}}]}}"#,
            r#"{}"#,
        ] {
            let v: Value = serde_json::from_str(junk).unwrap();
            assert_eq!(scalar_of(&v), PromScalar::Unreadable, "{junk}");
        }
    }
}
