//! Keyword + regex blocklist guardrail.
//!
//! Two pattern flavours mix freely in the same blocklist:
//! - `Literal(s)`: case-insensitive substring match.
//! - `Regex(re)`: compiled once at construction; case-sensitivity is
//!   the caller's responsibility (use `(?i)` in the regex if needed).
//!
//! Applies to both input messages (concatenated content of every
//! message) and output content. Whichever side a request triggers, the
//! verdict carries the matching pattern text in `reason` so operators
//! can debug from the access log.

use aisix_gateway::{ChatFormat, ChatResponse};
use async_trait::async_trait;
use regex::Regex;

use crate::{Guardrail, GuardrailVerdict};

#[derive(Debug, Clone)]
pub enum KeywordRule {
    /// 两份：`raw` 是操作者写下的原样（`description` 要把它原封不动放进日志和
    /// 拦截理由里），`lower` 是匹配用的。小写在构造时算一次，而不是每次检查、
    /// 每条规则各算一遍。
    Literal {
        raw: String,
        lower: String,
    },
    Regex(Regex),
}

impl KeywordRule {
    pub fn literal(s: impl Into<String>) -> Self {
        let raw: String = s.into();
        let lower = raw.to_lowercase();
        Self::Literal { raw, lower }
    }

    pub fn regex(pattern: &str) -> Result<Self, regex::Error> {
        Regex::new(pattern).map(Self::Regex)
    }

    /// `haystack_lower` 是调用方小写过一次的同一段文本。字面量规则用它，正则
    /// 用原文（大小写由正则自己的 `(?i)` 决定）。
    fn matches(&self, haystack: &str, haystack_lower: &str) -> bool {
        match self {
            KeywordRule::Literal { raw, lower } => {
                !raw.is_empty() && haystack_lower.contains(lower.as_str())
            }
            KeywordRule::Regex(re) => re.is_match(haystack),
        }
    }

    fn description(&self) -> String {
        match self {
            KeywordRule::Literal { raw, .. } => format!("literal {raw:?}"),
            KeywordRule::Regex(r) => format!("regex /{}/", r.as_str()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeywordBlocklist {
    rules: Vec<KeywordRule>,
    /// Apply on input messages.
    pub check_input_enabled: bool,
    /// Apply on output content.
    pub check_output_enabled: bool,
}

impl KeywordBlocklist {
    pub fn new(rules: Vec<KeywordRule>) -> Self {
        Self {
            rules,
            check_input_enabled: true,
            check_output_enabled: true,
        }
    }

    pub fn input_only(rules: Vec<KeywordRule>) -> Self {
        Self {
            rules,
            check_input_enabled: true,
            check_output_enabled: false,
        }
    }

    pub fn output_only(rules: Vec<KeywordRule>) -> Self {
        Self {
            rules,
            check_input_enabled: false,
            check_output_enabled: true,
        }
    }

    fn first_match<'a>(&'a self, text: &str) -> Option<&'a KeywordRule> {
        // 整段文本只小写一次。原先是每条字面量规则各小写一遍整段 —— L 条规则
        // 对 H 字节的提示词就是 O(L·H) 的分配与拷贝，而这是每个请求都要走的
        // 路径（输入一次、输出再一次）。
        //
        // 全是正则的黑名单不付这个代价：没有字面量规则时根本不小写。
        let lowered = self
            .rules
            .iter()
            .any(|r| matches!(r, KeywordRule::Literal { .. }))
            .then(|| text.to_lowercase());
        let lower = lowered.as_deref().unwrap_or("");
        self.rules.iter().find(|r| r.matches(text, lower))
    }
}

#[async_trait]
impl Guardrail for KeywordBlocklist {
    fn name(&self) -> &'static str {
        "keyword_blocklist"
    }

    /// Only force streamed-output hold-back when this blocklist actually
    /// checks output (#466). An input-only keyword guard must not buffer.
    fn runs_on_output(&self) -> bool {
        self.check_output_enabled
    }

    fn enforces_input(&self) -> bool {
        self.check_input_enabled
    }

    // Local rules: checking costs no remote call.
    fn checks_input_remotely(&self) -> bool {
        false
    }

    async fn check_input(&self, req: &ChatFormat) -> GuardrailVerdict {
        if !self.check_input_enabled {
            return GuardrailVerdict::Allow;
        }
        // Concatenate the message contents — checking each message
        // separately would cost the same and never catch any extra
        // hits since rules don't span messages.
        let combined: String = req
            .messages
            .iter()
            .map(crate::message_scan_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        match self.first_match(&combined) {
            Some(rule) => {
                GuardrailVerdict::block(format!("input blocked by {}", rule.description()))
            }
            None => GuardrailVerdict::Allow,
        }
    }

    async fn check_output(&self, resp: &ChatResponse) -> GuardrailVerdict {
        if !self.check_output_enabled {
            return GuardrailVerdict::Allow;
        }
        // Inspect content + tool-call output (#448), not just content.
        let text = resp.guardrail_output_text();
        match self.first_match(&text) {
            Some(rule) => {
                GuardrailVerdict::block(format!("output blocked by {}", rule.description()))
            }
            None => GuardrailVerdict::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_gateway::{ChatMessage, FinishReason, UsageStats};

    fn req(messages: &[(&str, &str)]) -> ChatFormat {
        let msgs = messages
            .iter()
            .map(|(role, content)| match *role {
                "system" => ChatMessage::system(*content),
                "user" => ChatMessage::user(*content),
                _ => ChatMessage::assistant(*content),
            })
            .collect();
        ChatFormat::new("m", msgs)
    }

    fn resp(content: &str) -> ChatResponse {
        ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: ChatMessage::assistant(content),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::new(0, 0),
        }
    }

    /// 拦截理由里必须出现操作者**原样**写下的那个词。
    ///
    /// 匹配用的是预先小写好的那一份；理由若也用它，运维配的 `AKIA` 会在日志里
    /// 变成 `akia` —— 那是拿去和配置对照的那个字符串，不能被改。
    #[tokio::test]
    async fn the_reported_pattern_keeps_the_operator_casing() {
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("AKIA")]);
        let v = g
            .check_input(&req(&[("user", "my key is akia-secret")]))
            .await;
        match v {
            GuardrailVerdict::Block { reason, .. } => {
                assert!(reason.contains("AKIA"), "理由里没有原样的字面量: {reason}");
                assert!(
                    !reason.contains("akia\""),
                    "理由里出现了被小写过的字面量: {reason}"
                );
            }
            other => panic!("该拦下: {other:?}"),
        }
    }

    /// 多条字面量规则时，整段文本只小写一次 —— 但从行为上看不出来，所以这里
    /// 钉的是「提到多条也仍然逐条正确」：优化写坏最可能的表现是只有第一条生效。
    #[tokio::test]
    async fn every_literal_still_matches_when_several_are_configured() {
        let g = KeywordBlocklist::new(vec![
            KeywordRule::literal("alpha"),
            KeywordRule::literal("BRAVO"),
            KeywordRule::literal("charlie"),
        ]);
        for probe in ["say Alpha", "say bravo", "say CHARLIE"] {
            assert!(
                matches!(
                    g.check_input(&req(&[("user", probe)])).await,
                    GuardrailVerdict::Block { .. }
                ),
                "没拦下: {probe}"
            );
        }
        assert!(matches!(
            g.check_input(&req(&[("user", "say delta")])).await,
            GuardrailVerdict::Allow
        ));
    }

    #[tokio::test]
    async fn literal_match_is_case_insensitive() {
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("Forbidden")]);
        let v = g
            .check_input(&req(&[("user", "say the FORBIDDEN word")]))
            .await;
        assert!(v.is_block());
    }

    #[tokio::test]
    async fn matches_text_in_content_blocks_when_flat_content_empty() {
        // #465: a message whose text lives only in content_blocks (empty
        // top-level content — the round-trip shape) must still be
        // scanned. Before the shared message_scan_text helper this
        // bypassed moderation entirely.
        let msg: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": "",
            "content_blocks": [{"type": "text", "text": "the FORBIDDEN word"}]
        }))
        .unwrap();
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("Forbidden")]);
        let v = g.check_input(&ChatFormat::new("m", vec![msg])).await;
        assert!(v.is_block(), "content-block text must be scanned");
    }

    #[tokio::test]
    async fn blocks_keyword_hidden_in_content_blocks_behind_benign_flat_content() {
        // Guardrail bypass: a caller sends benign top-level `content` but
        // hides the banned keyword in a `content_blocks` text entry, which
        // the provider bridges forward upstream. The scan must catch it —
        // otherwise every input guardrail is trivially bypassed by a valid
        // key holder.
        let msg: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": "what is a good banana bread recipe?",
            "content_blocks": [{"type": "text", "text": "the FORBIDDEN word"}]
        }))
        .unwrap();
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("Forbidden")]);
        let v = g.check_input(&ChatFormat::new("m", vec![msg])).await;
        assert!(
            v.is_block(),
            "keyword hidden in content_blocks must still be blocked"
        );
    }

    #[tokio::test]
    async fn blocks_keyword_hidden_in_tool_call_arguments() {
        // Same bypass via `extra["tool_calls"]`: a history-replay tool
        // call carries the banned keyword in its arguments, which the
        // bridges forward upstream. Must be scanned and blocked.
        let msg: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "c1",
                "type": "function",
                "function": {
                    "name": "lookup",
                    "arguments": "{\"q\":\"the FORBIDDEN word\"}"
                }
            }]
        }))
        .unwrap();
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("Forbidden")]);
        let v = g.check_input(&ChatFormat::new("m", vec![msg])).await;
        assert!(
            v.is_block(),
            "keyword hidden in tool_call arguments must be blocked"
        );
    }

    #[tokio::test]
    async fn empty_literal_pattern_never_matches() {
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("")]);
        let v = g.check_input(&req(&[("user", "anything")])).await;
        assert_eq!(v, GuardrailVerdict::Allow);
    }

    #[tokio::test]
    async fn regex_pattern_matches_when_provided() {
        let g = KeywordBlocklist::new(vec![
            KeywordRule::regex(r"\bssn:\s*\d{3}-\d{2}-\d{4}").unwrap()
        ]);
        let v = g
            .check_input(&req(&[("user", "the user's ssn: 123-45-6789 is")]))
            .await;
        assert!(v.is_block());
    }

    #[tokio::test]
    async fn no_match_returns_allow() {
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("nothing here")]);
        let v = g.check_input(&req(&[("user", "hello world")])).await;
        assert_eq!(v, GuardrailVerdict::Allow);
    }

    #[tokio::test]
    async fn output_check_runs_against_response_content() {
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("dangerous")]);
        let v = g.check_output(&resp("here is a dangerous answer")).await;
        assert!(v.is_block());
    }

    #[tokio::test]
    async fn output_check_inspects_tool_call_arguments() {
        // A forbidden word that appears ONLY inside tool_call arguments
        // (message.content is empty) must still be blocked — tool-call
        // output is client-visible and must not bypass guardrails (#448).
        let g = KeywordBlocklist::new(vec![KeywordRule::literal("dangerous")]);
        let mut msg = ChatMessage::assistant("");
        msg.extra.insert(
            "tool_calls".into(),
            serde_json::json!([{
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "run",
                    "arguments": "{\"cmd\":\"do something dangerous\"}"
                }
            }]),
        );
        let r = ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: msg,
            finish_reason: FinishReason::Stop,
            usage: UsageStats::new(0, 0),
        };
        assert!(g.check_output(&r).await.is_block());
    }

    #[tokio::test]
    async fn input_only_skips_output_checks() {
        let g = KeywordBlocklist::input_only(vec![KeywordRule::literal("zeta")]);
        let v = g.check_output(&resp("zeta zeta zeta")).await;
        assert_eq!(v, GuardrailVerdict::Allow);
    }

    #[tokio::test]
    async fn output_only_skips_input_checks() {
        let g = KeywordBlocklist::output_only(vec![KeywordRule::literal("zeta")]);
        let v = g.check_input(&req(&[("user", "zeta zeta")])).await;
        assert_eq!(v, GuardrailVerdict::Allow);
    }

    #[tokio::test]
    async fn first_matching_rule_wins_so_reason_is_deterministic() {
        let g = KeywordBlocklist::new(vec![
            KeywordRule::literal("alpha"),
            KeywordRule::literal("beta"),
        ]);
        let v = g.check_input(&req(&[("user", "alpha and beta")])).await;
        if let GuardrailVerdict::Block { reason, .. } = v {
            assert!(reason.contains("alpha"));
        } else {
            panic!("expected Block");
        }
    }

    #[test]
    fn invalid_regex_is_a_clean_error_not_a_panic() {
        assert!(KeywordRule::regex("[unclosed").is_err());
    }
}
