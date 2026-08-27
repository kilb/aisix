//! Rate-limit configuration attached to Models and ApiKeys.
//!
//! All fields are optional; absence means "no limit on that dimension".
//! Windows per spec §3:
//! - `rps` — 1s fixed window (request count only)
//! - `tpm`/`rpm` — 60s fixed window
//! - `rph` — 3600s fixed window (request count only)
//! - `tpd`/`rpd` — 86400s fixed window
//! - `concurrency` — semaphore capacity (not windowed)
//!
//! Token-rate counters are minute/day only; there is no `tps` or `tph`
//! field.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RateLimit {
    /// Tokens per 60-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tpm: Option<u64>,

    /// Tokens per 3,600-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tph: Option<u64>,

    /// Tokens per 86,400-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tpd: Option<u64>,

    /// Requests per 1-second window. There is no per-second token limit field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rps: Option<u64>,

    /// Requests per 60-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u64>,

    /// Requests per 3,600-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rph: Option<u64>,

    /// Requests per 86,400-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpd: Option<u64>,

    /// Max concurrent in-flight requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,

    /// Cumulative spend allowance in micro-USD, in the total-ever-granted
    /// sense rather than what is left.
    ///
    /// **Not a field anyone configures.** It carries no `schemars` or serde
    /// presence, so it cannot be written on a key's or a model's inline
    /// limits: those buckets commit *token* counts through the same argument
    /// the spend buckets commit micro-USD through, and a figure set there
    /// would be compared against the wrong quantity entirely — accepted,
    /// enforced, and meaningless. The user-facing knob is
    /// `RateLimitPolicy::granted_micro_usd`, which the quota layer projects
    /// onto the spend bucket where the units are right.
    ///
    /// Unlike every other field here it has no window: it is checked against
    /// a counter that only grows and is never reset. A request is admitted
    /// while total consumed sits below the figure, so allowing more means
    /// raising it — nothing is reset and nothing needs reconciling.
    #[serde(skip)]
    #[schemars(skip)]
    pub granted_micro_usd: Option<u64>,
}

impl RateLimit {
    pub const fn is_unrestricted(&self) -> bool {
        self.tpm.is_none()
            && self.tph.is_none()
            && self.tpd.is_none()
            && self.rps.is_none()
            && self.rpm.is_none()
            && self.rph.is_none()
            && self.rpd.is_none()
            && self.concurrency.is_none()
            && self.granted_micro_usd.is_none()
    }
}

/// Limits for one API key's calls to one MCP server
/// (`ApiKey::mcp_rate_limits`).
///
/// Carries only the request-count dimensions of [`RateLimit`]: an MCP
/// `tools/call` commits no tokens, so a token-rate cap here would never be
/// consumed — the shape leaves `tpm`/`tpd` out rather than accepting a knob
/// that is silently inert.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpRateLimit {
    /// Tool calls per 1-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rps: Option<u64>,

    /// Tool calls per 60-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u64>,

    /// Tool calls per 3,600-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rph: Option<u64>,

    /// Tool calls per 86,400-second window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpd: Option<u64>,

    /// Max concurrent in-flight tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
}

impl McpRateLimit {
    pub const fn is_unrestricted(&self) -> bool {
        self.rps.is_none()
            && self.rpm.is_none()
            && self.rph.is_none()
            && self.rpd.is_none()
            && self.concurrency.is_none()
    }
}

impl From<&McpRateLimit> for RateLimit {
    fn from(mcp: &McpRateLimit) -> Self {
        Self {
            // MCP 的限流没有累计额度这个概念 —— 那是给花费用的，而 MCP 调用
            // 不经过定价。
            granted_micro_usd: None,
            tpm: None,
            tph: None,
            tpd: None,
            rps: mcp.rps,
            rpm: mcp.rpm,
            rph: mcp.rph,
            rpd: mcp.rpd,
            concurrency: mcp.concurrency,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unrestricted() {
        assert!(RateLimit::default().is_unrestricted());
    }

    #[test]
    fn omits_none_fields_on_serialise() {
        let rl = RateLimit {
            rpm: Some(60),
            ..Default::default()
        };
        let json = serde_json::to_value(&rl).unwrap();
        assert_eq!(json["rpm"], 60);
        assert!(json.get("tpm").is_none());
        assert!(json.get("concurrency").is_none());
    }

    #[test]
    fn tolerates_unknown_fields_for_forward_compat() {
        // The control plane may ship new fields ahead of the DP rolling out; serde must
        // accept them (the write path still rejects them via the strict
        // schema validators in models/schema.rs).
        let rl: RateLimit = serde_json::from_str(r#"{"rpm": 10, "extra": 1}"#).unwrap();
        assert_eq!(rl.rpm, Some(10));
    }
}

#[cfg(test)]
mod allowance_placement {
    use super::*;

    /// 累计发放额**只能**是策略上的旋钮，不能出现在密钥或模型的内联限流里。
    ///
    /// 那两层的 `commit` 传的是 token 数，花费层传的才是 micro-USD —— 同一个
    /// 参数、两种量纲。写在内联限流上会被照常接受、照常执行，然后拿着一个
    /// 花费上限去跟 token 数比较。半生效的旋钮比不生效更糟，因为它看起来在工作。
    ///
    /// 靠 schema 生成检查不出来（生成的是当下的形状）；这里直接断言序列化行为。
    #[test]
    fn 内联限流不接受也不吐出累计发放额() {
        let json = r#"{"rpm":10,"granted_micro_usd":5000000}"#;
        let parsed: RateLimit = serde_json::from_str(json).expect("应当能解析");
        assert_eq!(
            parsed.granted_micro_usd, None,
            "内联限流吃下了累计发放额 —— 那一层的计数器装的是 token，不是钱",
        );

        let internal = RateLimit {
            granted_micro_usd: Some(5_000_000),
            ..RateLimit::default()
        };
        let out = serde_json::to_string(&internal).unwrap();
        assert!(
            !out.contains("granted_micro_usd"),
            "内部投影字段被序列化出去了：{out}",
        );
    }

    /// 但它必须仍然参与 `is_unrestricted`，否则「只配了发放额」的策略会被当成
    /// 无限制而整条跳过 —— 那样闸就不存在了。
    #[test]
    fn 只配发放额的限流不算无限制() {
        let only = RateLimit {
            granted_micro_usd: Some(1),
            ..RateLimit::default()
        };
        assert!(!only.is_unrestricted());
    }
}
