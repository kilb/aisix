//! 完整有界答案只在内存评分；保存前共享脱敏，预览按 UTF-8 字节截取。

use std::collections::BTreeMap;

use aisix_gateway::chat::ChoiceFinishCensus;
use aisix_gateway::response_tap::{ObservedStreamUsage, ResponseHeaderTap};
use aisix_gateway::{BridgeError, ChatChunk, FinishReason, UsageStats};
use futures::StreamExt;
use serde_json::Value;

use super::contracts::{NormalizedAnswer, NormalizedToolCall, Usage};

pub const PREVIEW_BYTES: usize = 1024;
const MAX_CHUNKS: usize = 4096;
const MAX_TOOL_CALLS: usize = 32;

#[derive(Default)]
struct ToolFragments {
    name: String,
    arguments: String,
    structured_arguments: Option<Value>,
    saw_arguments: bool,
}

#[derive(Debug)]
pub struct CollectedAnswer {
    pub answer: NormalizedAnswer,
    pub finish_reason: String,
    pub usage: Usage,
    pub output_bytes: u32,
}

#[derive(Debug)]
pub enum CollectError {
    Bridge(BridgeError),
    Truncated,
    Decode,
}

impl From<BridgeError> for CollectError {
    fn from(error: BridgeError) -> Self {
        Self::Bridge(error)
    }
}

#[derive(Default)]
struct Collector {
    answer: NormalizedAnswer,
    tools: BTreeMap<u64, ToolFragments>,
    finish: ChoiceFinishCensus,
    finish_reason: Option<FinishReason>,
    stats: Option<UsageStats>,
    chunks: usize,
    bytes: usize,
}

/// 计入所有选择、推理、工具和元数据，不能靠丢弃其它 delta 绕过采集限额。
pub async fn collect(
    mut stream: aisix_gateway::bridge::ChatChunkStream,
    tap: &ResponseHeaderTap,
    max_bytes: usize,
    additive_cache: bool,
    usage: &mut Usage,
) -> Result<CollectedAnswer, CollectError> {
    let mut collector = Collector::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        collector.observe(&chunk, max_bytes)?;
        *usage = reported_usage(
            collector.stats.as_ref(),
            tap.observed_stream_usage().as_ref(),
            additive_cache,
        );
        if tap.upstream_io_observer().is_some_and(|io| {
            io.snapshot()
                .response_body_bytes
                .is_some_and(|bytes| bytes > max_bytes as u64)
        }) {
            return Err(CollectError::Truncated);
        }
    }
    collector.complete(tap.observed_stream_usage(), max_bytes, additive_cache)
}

impl Collector {
    fn observe(&mut self, chunk: &ChatChunk, max_bytes: usize) -> Result<(), CollectError> {
        self.chunks += 1;
        self.bytes = self.bytes.saturating_add(
            serde_json::to_vec(chunk)
                .map_err(|_| CollectError::Decode)?
                .len(),
        );
        if self.chunks > MAX_CHUNKS || self.bytes > max_bytes {
            return Err(CollectError::Truncated);
        }
        self.finish.observe(chunk);
        for usage in [chunk.accounting_usage.as_ref(), chunk.usage.as_ref()]
            .into_iter()
            .flatten()
        {
            self.stats = Some(
                self.stats
                    .as_ref()
                    .map_or_else(|| usage.clone(), |stats| stats.max_fieldwise(usage)),
            );
        }
        // 请求固定 n=1；额外选择是协议违约，不能悄悄取其中可通过的一份。
        if chunk.choice_index != 0 && !chunk.has_no_payload() {
            return Err(CollectError::Decode);
        }
        if chunk.choice_index != 0 {
            return Ok(());
        }
        if let Some(reason) = &chunk.finish_reason {
            self.finish_reason = Some(reason.clone());
        }
        if let Some(text) = &chunk.delta.content {
            self.answer.text.push_str(text);
        }
        if let Some(refusal) = chunk.delta.extra.get("refusal").and_then(Value::as_str) {
            self.answer.text.push_str(refusal);
        }
        if let Some(calls) = &chunk.delta.tool_calls {
            for call in calls {
                self.tool_fragment(call, None)?;
            }
        }
        if let Some(call) = &chunk.delta.function_call {
            self.tool_fragment(call, Some(0))?;
        }
        Ok(())
    }

    fn tool_fragment(&mut self, call: &Value, legacy: Option<u64>) -> Result<(), CollectError> {
        let index = legacy
            .or_else(|| call.get("index").and_then(Value::as_u64))
            .ok_or(CollectError::Decode)?;
        if index >= MAX_TOOL_CALLS as u64
            || (!self.tools.contains_key(&index) && self.tools.len() >= MAX_TOOL_CALLS)
        {
            return Err(CollectError::Truncated);
        }
        let function = if legacy.is_some() {
            call
        } else {
            call.get("function").ok_or(CollectError::Decode)?
        };
        let fragments = self.tools.entry(index).or_default();
        if let Some(name) = function.get("name").and_then(Value::as_str) {
            fragments.name.push_str(name);
        }
        if let Some(arguments) = function.get("arguments") {
            fragments.saw_arguments = true;
            if let Some(text) = arguments.as_str() {
                if fragments.structured_arguments.is_some() {
                    return Err(CollectError::Decode);
                }
                fragments.arguments.push_str(text);
            } else if fragments.arguments.is_empty() && fragments.structured_arguments.is_none() {
                fragments.structured_arguments = Some(arguments.clone());
            } else {
                return Err(CollectError::Decode);
            }
        }
        Ok(())
    }

    fn complete(
        mut self,
        observed: Option<ObservedStreamUsage>,
        max_bytes: usize,
        additive_cache: bool,
    ) -> Result<CollectedAnswer, CollectError> {
        if !self.finish.all_finished() {
            return Err(CollectError::Decode);
        }
        if self.finish_reason == Some(FinishReason::Length) {
            return Err(CollectError::Truncated);
        }
        for (_, fragments) in self.tools {
            if fragments.name.is_empty() || !fragments.saw_arguments {
                return Err(CollectError::Decode);
            }
            // 合法 wire 中的非法参数 JSON 仍是模型答案，评分器按任务约束判失败。
            let arguments = fragments.structured_arguments.unwrap_or_else(|| {
                serde_json::from_str(&fragments.arguments)
                    .unwrap_or(Value::String(fragments.arguments))
            });
            self.answer.tool_calls.push(NormalizedToolCall {
                name: fragments.name,
                arguments,
            });
        }
        if self.answer.text.is_empty() && self.answer.tool_calls.is_empty() {
            return Err(CollectError::Decode);
        }
        let bytes = serde_json::to_vec(&self.answer)
            .map_err(|_| CollectError::Decode)?
            .len();
        if bytes > max_bytes {
            return Err(CollectError::Truncated);
        }
        let usage = reported_usage(self.stats.as_ref(), observed.as_ref(), additive_cache);
        Ok(CollectedAnswer {
            answer: self.answer,
            finish_reason: finish_name(self.finish_reason.as_ref().ok_or(CollectError::Decode)?),
            usage,
            output_bytes: bytes as u32,
        })
    }
}

fn reported_usage(
    stats: Option<&UsageStats>,
    observed: Option<&ObservedStreamUsage>,
    additive: bool,
) -> Usage {
    let Some(stats) = observed.map(|usage| &usage.stats).or(stats) else {
        return Usage::default();
    };
    let mut usage = Usage::default();
    // 未携带字段存在性标记的适配器只能证明非零值；缺字段的默认零绝不冒充报告零。
    usage.input_tokens = (observed.is_some_and(|usage| usage.prompt_reported)
        || stats.prompt_tokens > 0)
        .then_some(u64::from(stats.prompt_tokens));
    let output = aisix_gateway::chat::recorded_completion_tokens(
        stats.prompt_tokens,
        stats.completion_tokens,
        stats.reasoning_tokens,
        stats.reasoning_folded_into_completion,
        stats.upstream_total_tokens,
    );
    usage.output_tokens = (observed.is_some_and(|usage| usage.completion_reported) || output > 0)
        .then_some(u64::from(output));
    usage.upstream_total_tokens =
        (stats.upstream_total_tokens > 0).then_some(u64::from(stats.upstream_total_tokens));
    if additive {
        usage.cache_read_tokens = (stats.openai_details.cached_tokens_reported
            || stats.cache_read_tokens > 0)
            .then_some(u64::from(stats.cache_read_tokens));
        usage.cache_creation_tokens = (stats.openai_details.cache_creation_tokens_reported
            || stats.cache_creation_tokens > 0)
            .then_some(u64::from(stats.cache_creation_tokens));
    } else {
        usage.cached_prompt_tokens = (stats.openai_details.cached_tokens_reported
            || stats.cached_prompt_tokens > 0
            || stats.cache_usage_reported == Some(true)
            || stats.prompt_cache_hit_tokens.is_some())
        .then_some(u64::from(stats.cached_prompt_tokens));
    }
    usage.reasoning_tokens = (stats.openai_details.reasoning_tokens_reported
        || stats.reasoning_tokens > 0)
        .then_some(u64::from(stats.reasoning_tokens));
    usage
}

pub fn scrub_answer(answer: &NormalizedAnswer) -> (NormalizedAnswer, bool) {
    let mut changed = false;
    let text = scrub(&answer.text, &mut changed);
    let tool_calls = answer
        .tool_calls
        .iter()
        .map(|call| NormalizedToolCall {
            name: scrub(&call.name, &mut changed),
            arguments: scrub_value(&call.arguments, &mut changed),
        })
        .collect();
    (NormalizedAnswer { text, tool_calls }, changed)
}

fn scrub_value(value: &Value, changed: &mut bool) -> Value {
    match value {
        Value::String(text) => Value::String(scrub(text, changed)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| scrub_value(value, changed))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (scrub(key, changed), scrub_value(value, changed)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn scrub(value: &str, changed: &mut bool) -> String {
    let scrubbed = aisix_gateway::upstream_error_scrub::scrub_upstream_error_text(value);
    let scrubbed = redact_urls(&scrubbed);
    let scrubbed = aisix_core::redact_userinfo(&scrubbed).into_owned();
    *changed |= scrubbed != value;
    scrubbed
}

// 共享scrubber负责凭据；URL显示另外使用核心函数删除path/query/fragment。
fn redact_urls(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(separator) = rest.find("://") {
        let scheme_start = rest[..separator]
            .char_indices()
            .rev()
            .find(|(_, character)| {
                !character.is_ascii_alphanumeric() && !matches!(*character, '+' | '-' | '.')
            })
            .map_or(0, |(index, character)| index + character.len_utf8());
        let end = rest[separator + 3..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '\'' | '"' | '`')
            })
            .map_or(rest.len(), |index| separator + 3 + index);
        result.push_str(&rest[..scheme_start]);
        result.push_str(&aisix_core::redact_url_for_log(&rest[scheme_start..end]));
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}

pub fn preview(answer: &NormalizedAnswer) -> String {
    let text = if answer.tool_calls.is_empty() {
        answer.text.clone()
    } else {
        serde_json::to_string(answer).unwrap_or_default()
    };
    utf8_prefix(&text, PREVIEW_BYTES).to_string()
}

pub fn safe_error(detail: &str) -> String {
    let mut changed = false;
    let safe = scrub(detail, &mut changed);
    utf8_prefix(&safe, PREVIEW_BYTES).to_string()
}

pub fn partial_usage(tap: &ResponseHeaderTap, additive_cache: bool) -> Usage {
    reported_usage(None, tap.observed_stream_usage().as_ref(), additive_cache)
}

pub fn retain_reported_usage(usage: &mut Usage, incoming: Usage) {
    for (saved, reported) in [
        (&mut usage.input_tokens, incoming.input_tokens),
        (&mut usage.output_tokens, incoming.output_tokens),
        (
            &mut usage.upstream_total_tokens,
            incoming.upstream_total_tokens,
        ),
        (
            &mut usage.cached_prompt_tokens,
            incoming.cached_prompt_tokens,
        ),
        (&mut usage.cache_read_tokens, incoming.cache_read_tokens),
        (
            &mut usage.cache_creation_tokens,
            incoming.cache_creation_tokens,
        ),
        (&mut usage.reasoning_tokens, incoming.reasoning_tokens),
    ] {
        *saved = (*saved).max(reported);
    }
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn finish_name(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".into(),
        FinishReason::Length => "length".into(),
        FinishReason::ContentFilter => "content_filter".into(),
        FinishReason::ToolCalls => "tool_calls".into(),
        FinishReason::Other(reason) => safe_error(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_gateway::{response_tap, ChatDelta};
    use serde_json::json;
    use std::sync::Arc;

    fn chunk(delta: ChatDelta) -> ChatChunk {
        ChatChunk {
            id: "answer".into(),
            model: "upstream".into(),
            choice_index: 0,
            delta,
            finish_reason: None,
            logprobs: None,
            usage: None,
            native_envelope: None,
            service_tier: None,
            accounting_usage: None,
        }
    }

    #[test]
    fn missing_usage_stays_null_and_reported_zero_requires_presence_evidence() {
        let none = serde_json::to_value(reported_usage(None, None, false)).unwrap();
        assert_eq!(
            none,
            json!({"input_tokens":null,"output_tokens":null,"upstream_total_tokens":null,"cached_prompt_tokens":null,"cache_read_tokens":null,"cache_creation_tokens":null,"reasoning_tokens":null})
        );
        let stats = UsageStats::default();
        assert!(reported_usage(Some(&stats), None, false)
            .input_tokens
            .is_none());
        let observed = ObservedStreamUsage {
            stats,
            prompt_reported: true,
            completion_reported: false,
            service_tier: None,
        };
        let usage = reported_usage(None, Some(&observed), false);
        assert_eq!(usage.input_tokens, Some(0));
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.upstream_total_tokens, None);
    }

    #[test]
    fn tool_argument_fragments_are_collected_in_full_before_scoring() {
        let mut collector = Collector::default();
        for fragment in [
            json!({"index":0,"function":{"name":"lookup","arguments":"{\"city\":"}}),
            json!({"index":0,"function":{"arguments":"\"深圳\"}"}}),
        ] {
            collector
                .observe(
                    &chunk(ChatDelta {
                        tool_calls: Some(vec![fragment]),
                        ..Default::default()
                    }),
                    65536,
                )
                .unwrap_or_else(|_| panic!("有界工具片段"));
        }
        let mut terminal = chunk(ChatDelta::default());
        terminal.finish_reason = Some(FinishReason::ToolCalls);
        collector
            .observe(&terminal, 65536)
            .unwrap_or_else(|_| panic!("结束帧"));
        let answer = collector
            .complete(None, 65536, false)
            .unwrap_or_else(|_| panic!("完整答案"));
        assert_eq!(
            answer.answer.tool_calls[0].arguments,
            json!({"city":"深圳"})
        );
        assert_eq!(answer.answer.tool_calls[0].name, "lookup");
    }

    #[test]
    fn missing_tool_arguments_is_decode_error_but_explicit_invalid_arguments_is_an_answer() {
        for (call, missing) in [
            (json!({"index":0,"function":{"name":"lookup"}}), true),
            (
                json!({"index":0,"function":{"name":"lookup","arguments":""}}),
                false,
            ),
        ] {
            let mut collector = Collector::default();
            let mut answer = chunk(ChatDelta {
                tool_calls: Some(vec![call]),
                ..Default::default()
            });
            answer.finish_reason = Some(FinishReason::ToolCalls);
            collector.observe(&answer, 65536).unwrap();
            let result = collector.complete(None, 65536, false);
            if missing {
                assert!(matches!(result, Err(CollectError::Decode)));
            } else {
                assert_eq!(result.unwrap().answer.tool_calls[0].arguments, json!(""));
            }
        }
    }

    #[tokio::test]
    async fn interrupted_stream_keeps_usage_that_was_actually_reported() {
        let tap = ResponseHeaderTap::new(vec![], false, "partial-usage".into());
        let mut first = chunk(ChatDelta {
            content: Some("partial".into()),
            ..Default::default()
        });
        first.usage = Some(UsageStats::new(11, 5));
        let stream = Box::pin(futures::stream::iter([
            Ok(first),
            Err(BridgeError::StreamAborted),
        ]));
        let mut usage = Usage::default();
        assert!(collect(stream, &tap, 65536, false, &mut usage)
            .await
            .is_err());
        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(5));
        retain_reported_usage(&mut usage, Usage::default());
        assert_eq!(
            usage.input_tokens,
            Some(11),
            "未知的后续计量不能覆盖已报告计量"
        );
    }

    #[test]
    fn complete_looking_json_is_not_scoreable_after_length_finish_or_missing_terminal() {
        for reason in [None, Some(FinishReason::Length)] {
            let mut collector = Collector::default();
            let mut answer = chunk(ChatDelta {
                content: Some("{\"answer\":42}".into()),
                ..Default::default()
            });
            answer.finish_reason = reason;
            collector
                .observe(&answer, 65536)
                .unwrap_or_else(|_| panic!("片段"));
            assert!(collector.complete(None, 65536, false).is_err());
        }
    }

    #[test]
    fn all_delta_bytes_count_and_oversize_is_not_a_partial_answer() {
        let mut collector = Collector::default();
        let answer = chunk(ChatDelta {
            reasoning_content: Some("x".repeat(65536)),
            content: Some("42".into()),
            ..Default::default()
        });
        assert!(matches!(
            collector.observe(&answer, 65536),
            Err(CollectError::Truncated)
        ));
    }

    #[tokio::test]
    async fn sent_credentials_are_scrubbed_before_utf8_safe_preview() {
        let tap = Arc::new(ResponseHeaderTap::new(
            vec![],
            false,
            "assessment-scrub".into(),
        ));
        response_tap::scope(tap, async {
            response_tap::note_sent_credential("unit-secret-credential");
            let original = NormalizedAnswer {
                text: format!("{}unit-secret-credential", "深".repeat(340)),
                tool_calls: vec![NormalizedToolCall {
                    name: "lookup".into(),
                    arguments: json!({"key":"unit-secret-credential"}),
                }],
            };
            let (safe, changed) = scrub_answer(&original);
            assert!(changed);
            assert_eq!(
                original.tool_calls[0].arguments["key"], "unit-secret-credential",
                "评分输入不被原地修改"
            );
            assert!(!serde_json::to_string(&safe)
                .unwrap()
                .contains("unit-secret-credential"));
            let preview = preview(&safe);
            assert!(preview.len() <= PREVIEW_BYTES);
            assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
        })
        .await;
    }

    #[test]
    fn configured_url_paths_queries_and_userinfo_never_enter_saved_evidence() {
        let original = NormalizedAnswer {
            text: "请看https://operator:password@example.com/private-secret?token=query-secret"
                .into(),
            tool_calls: vec![],
        };
        let (safe, changed) = scrub_answer(&original);
        assert!(changed);
        for secret in ["operator", "password", "private-secret", "query-secret"] {
            assert!(!safe.text.contains(secret), "{secret}");
        }
        assert!(safe.text.contains("example.com"));
    }
}
