//! 预算错误的结构化原因。
//!
//! 这是对调用方的契约的一部分——`error.budget.*` 的形状——所以在预算从
//! 控制平面判定改为本地策略判定后仍然保留，只是换了填充者。

/// 预算被拒绝时面向调用方的详细信息。字段形状是对调用方的契约（429 响应体
/// 的 `error.budget.*`），让程序化客户端能看到具体是哪个预算触发的
/// （`scope` / `scope_ref`）、超了多少（`limit_usd` / `spent_usd`），而不只是
/// 人类可读的 `message`。除 `message` 外全部可选：只有消息、没有结构化细节
/// 的兜底路径仍然可以构造这个类型。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetReason {
    pub message: String,
    pub scope: Option<String>,
    pub scope_ref: Option<String>,
    pub limit_usd: Option<String>,
    pub spent_usd: Option<String>,
    pub period: Option<String>,
    pub period_resets_at: Option<String>,
    pub retry_after_seconds: Option<u64>,
}

impl BudgetReason {
    /// 只带人类可读消息的原因——用于没有结构化 scope 细节的兜底路径。
    /// 本次改动删除了唯一的生产调用方（旧的控制平面 HTTP 预算网关）；
    /// 保留是因为下一个任务会让本地花费层的拒绝走这条构造路径。
    #[allow(dead_code)]
    pub(crate) fn message_only(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Default::default()
        }
    }
}
