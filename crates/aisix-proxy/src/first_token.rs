//! 在交付边界识别首个非空生成内容；不保存整段输出，也不替换既有首帧计时。
//!
//! 协议字段：
//! https://github.com/openai/openai-python/blob/141a89aa7fbb7f13a979bda930831a5fe38bc3a4/src/openai/types/chat/chat_completion_chunk.py
//! https://github.com/openai/openai-python/blob/141a89aa7fbb7f13a979bda930831a5fe38bc3a4/src/openai/types/responses/response_stream_event.py
//! https://github.com/anthropics/anthropic-sdk-python/blob/5139cf2e7d07aa618a6c30b7e0c712f10ccb5ad9/src/anthropic/types/raw_content_block_delta.py
//! https://api-docs.deepseek.com/api/create-chat-completion/
//! https://docs.vllm.ai/en/v0.33.0/features/reasoning_outputs/
//! added/done 正文与后续 delta 共同组成可见输出：
//! https://github.com/openai/openai-python/blob/141a89aa7fbb7f13a979bda930831a5fe38bc3a4/src/openai/lib/streaming/responses/_responses.py#L397-L430

use std::time::Instant;

use aisix_gateway::sse::{parse_frame, SseFrameSplitter};
use serde_json::Value;

use crate::operation::{self, Surface};

// 单帧超限后停止观测，宁可未知，也不为遥测积累不受限的模型输出。
const MAX_FRAME_BYTES: usize = 256 * 1024;
const FEED_BYTES: usize = 4096;

#[derive(Clone, Copy)]
pub(crate) enum Wire {
    Chat,
    Completions,
    Messages,
    Responses,
}

impl Wire {
    pub(crate) fn for_endpoint(endpoint: &str) -> Option<Self> {
        match endpoint {
            "/v1/chat/completions" => Some(Self::Chat),
            "/v1/completions" => Some(Self::Completions),
            "/v1/messages" => Some(Self::Messages),
            "/v1/responses" => Some(Self::Responses),
            _ => None,
        }
    }
}

pub(crate) fn token_surface(surface: Surface) -> bool {
    matches!(
        surface,
        operation::CHAT | operation::COMPLETIONS | operation::MESSAGES | operation::RESPONSES
    )
}

pub(crate) struct Observer {
    wire: Wire,
    started: Instant,
    frames: Option<SseFrameSplitter>,
    latency_ms: Option<u32>,
}

impl Observer {
    pub(crate) fn new(wire: Wire, started: Instant) -> Self {
        Self {
            wire,
            started,
            frames: Some(SseFrameSplitter::new(MAX_FRAME_BYTES)),
            latency_ms: None,
        }
    }

    pub(crate) fn latency_ms(&self) -> Option<u32> {
        self.latency_ms
    }

    pub(crate) fn observing(&self) -> bool {
        self.frames.is_some()
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        let Some(frames) = self.frames.as_mut() else {
            return;
        };
        let delivered = self.started.elapsed();
        // splitter.push 本身不限输入块大小，所以分片后再检查帧上限。
        for part in bytes.chunks(FEED_BYTES) {
            frames.push(part);
            loop {
                match frames.next_frame() {
                    Ok(Some(frame)) if frame.len() <= MAX_FRAME_BYTES => {
                        if has_content(self.wire, &frame) {
                            self.latency_ms = Some(crate::attempt::measured_ms(delivered));
                            self.frames = None;
                            return;
                        }
                    }
                    Ok(None) => break,
                    Ok(Some(_)) | Err(_) => {
                        self.frames = None;
                        return;
                    }
                }
            }
        }
        // 不把未终止的 SSE 尾帧当已交付事件；客户端解析器也还不能消费它。
    }
}

fn nonempty(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !text.is_empty())
}

fn any_nonempty(value: &Value, fields: &[&str]) -> bool {
    fields.iter().any(|field| nonempty(&value[*field]))
}

fn response_part_has_content(part: &Value) -> bool {
    match part["type"].as_str() {
        Some("output_text" | "reasoning_text" | "summary_text") => nonempty(&part["text"]),
        Some("refusal") => nonempty(&part["refusal"]),
        _ => false,
    }
}

fn response_parts_have_content(parts: &Value) -> bool {
    parts
        .as_array()
        .is_some_and(|parts| parts.iter().any(response_part_has_content))
}

fn response_item_has_content(item: &Value) -> bool {
    match item["type"].as_str() {
        Some("message") => response_parts_have_content(&item["content"]),
        Some("reasoning") => {
            response_parts_have_content(&item["summary"])
                || response_parts_have_content(&item["content"])
        }
        Some("function_call" | "mcp_call") => nonempty(&item["arguments"]),
        Some("custom_tool_call") => nonempty(&item["input"]),
        Some("code_interpreter_call") => nonempty(&item["code"]),
        _ => false,
    }
}

fn has_content(wire: Wire, frame: &[u8]) -> bool {
    let parsed = parse_frame(frame);
    if parsed.event == Some(b"error".as_slice()) {
        return false;
    }
    let Some(data) = parsed.data else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&data) else {
        return false;
    };
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return false;
    }
    match wire {
        Wire::Chat => value["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                let delta = &choice["delta"];
                any_nonempty(
                    delta,
                    &["content", "reasoning_content", "reasoning", "refusal"],
                ) || nonempty(&delta["function_call"]["arguments"])
                    || delta["tool_calls"].as_array().is_some_and(|calls| {
                        calls
                            .iter()
                            .any(|call| nonempty(&call["function"]["arguments"]))
                    })
            })
        }),
        Wire::Completions => value["choices"]
            .as_array()
            .is_some_and(|choices| choices.iter().any(|choice| nonempty(&choice["text"]))),
        Wire::Messages => match value["type"].as_str() {
            Some("content_block_delta") => match value["delta"]["type"].as_str() {
                Some("text_delta") => nonempty(&value["delta"]["text"]),
                Some("thinking_delta") => nonempty(&value["delta"]["thinking"]),
                Some("input_json_delta") => nonempty(&value["delta"]["partial_json"]),
                _ => false,
            },
            Some("content_block_start") => match value["content_block"]["type"].as_str() {
                Some("text") => nonempty(&value["content_block"]["text"]),
                Some("thinking") => nonempty(&value["content_block"]["thinking"]),
                _ => false,
            },
            _ => false,
        },
        Wire::Responses => match value["type"].as_str() {
            Some(
                "response.output_text.delta"
                | "response.refusal.delta"
                | "response.reasoning_text.delta"
                | "response.reasoning_summary_text.delta"
                | "response.function_call_arguments.delta"
                | "response.custom_tool_call_input.delta"
                | "response.mcp_call_arguments.delta"
                | "response.code_interpreter_call_code.delta",
            ) => nonempty(&value["delta"]),
            // added/done 也可首次携带完整正文；只读取对应内容槽，不把状态计作内容。
            Some(
                "response.content_part.added"
                | "response.content_part.done"
                | "response.reasoning_summary_part.added"
                | "response.reasoning_summary_part.done",
            ) => response_part_has_content(&value["part"]),
            Some("response.output_item.added" | "response.output_item.done") => {
                response_item_has_content(&value["item"])
            }
            Some(
                "response.output_text.done"
                | "response.reasoning_text.done"
                | "response.reasoning_summary_text.done",
            ) => nonempty(&value["text"]),
            Some("response.refusal.done") => nonempty(&value["refusal"]),
            Some("response.function_call_arguments.done" | "response.mcp_call_arguments.done") => {
                nonempty(&value["arguments"])
            }
            Some("response.custom_tool_call_input.done") => nonempty(&value["input"]),
            Some("response.code_interpreter_call_code.done") => nonempty(&value["code"]),
            Some("response.completed" | "response.incomplete") => value["response"]["output"]
                .as_array()
                .is_some_and(|items| items.iter().any(response_item_has_content)),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(value: Value) -> Vec<u8> {
        format!("data: {value}\r\n\r\n").into_bytes()
    }

    #[test]
    fn metadata_empty_errors_and_unterminated_tail_are_not_content() {
        for (wire, value) in [
            (
                Wire::Chat,
                json!({"choices":[{"delta":{"role":"assistant","content":"","tool_calls":[{"id":"call_1","function":{"name":"tool","arguments":""}}]}}]}),
            ),
            (
                Wire::Completions,
                json!({"choices":[{"text":"","finish_reason":"stop"}]}),
            ),
            (
                Wire::Messages,
                json!({"type":"message_start","message":{"content":[]}}),
            ),
            (
                Wire::Messages,
                json!({"type":"content_block_delta","delta":{"type":"signature_delta","signature":"signature"}}),
            ),
            (
                Wire::Responses,
                json!({"type":"response.created","response":{"status":"in_progress"}}),
            ),
            (
                Wire::Responses,
                json!({"type":"response.output_text.delta","delta":""}),
            ),
        ] {
            let mut observer = Observer::new(wire, Instant::now());
            observer.feed(b": ping\n\ndata: [DONE]\n\n");
            observer.feed(&event(value));
            observer.feed(
                b"event: error\ndata: {\"choices\":[{\"delta\":{\"content\":\"error\"}}]}\n\n",
            );
            observer.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}");
            assert_eq!(observer.latency_ms(), None);
        }
    }

    #[test]
    fn text_reasoning_refusal_and_tool_arguments_survive_every_byte_split() {
        for (wire, value) in [
            (Wire::Chat, json!({"choices":[{"delta":{"content":"猫"}}]})),
            (
                Wire::Chat,
                json!({"choices":[{"delta":{"reasoning_content":"thought"}}]}),
            ),
            (
                Wire::Chat,
                json!({"choices":[{"delta":{"refusal":"declined"}}]}),
            ),
            (
                Wire::Chat,
                json!({"choices":[{"delta":{"function_call":{"arguments":"{"}}}]}),
            ),
            (
                Wire::Chat,
                json!({"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{"}}]}}]}),
            ),
            (Wire::Completions, json!({"choices":[{"text":" "}]})),
            (
                Wire::Messages,
                json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"text"}}),
            ),
            (
                Wire::Messages,
                json!({"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"thought"}}),
            ),
            (
                Wire::Messages,
                json!({"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}),
            ),
            (
                Wire::Responses,
                json!({"type":"response.output_text.delta","delta":"text"}),
            ),
            (
                Wire::Responses,
                json!({"type":"response.reasoning_summary_text.delta","delta":"thought"}),
            ),
            (
                Wire::Responses,
                json!({"type":"response.refusal.delta","delta":"declined"}),
            ),
            (
                Wire::Responses,
                json!({"type":"response.function_call_arguments.delta","delta":"{"}),
            ),
            (
                Wire::Responses,
                json!({"type":"response.custom_tool_call_input.delta","delta":"patch"}),
            ),
        ] {
            let bytes = event(value);
            for split in 0..bytes.len() - 2 {
                let mut observer = Observer::new(wire, Instant::now());
                observer.feed(&bytes[..split]);
                assert_eq!(observer.latency_ms(), None);
                observer.feed(&bytes[split..]);
                assert!(observer.latency_ms().is_some());
                assert!(!observer.observing(), "首内容后必须释放解析缓冲区");
            }
        }
    }

    #[test]
    fn responses_added_parts_and_items_with_content_are_first_output() {
        for value in [
            json!({"type":"response.content_part.added","part":{"type":"output_text","text":"first"}}),
            json!({"type":"response.content_part.added","part":{"type":"refusal","refusal":"declined"}}),
            json!({"type":"response.content_part.done","part":{"type":"reasoning_text","text":"thought"}}),
            json!({"type":"response.reasoning_summary_part.added","part":{"type":"summary_text","text":"thought"}}),
            json!({"type":"response.output_item.added","item":{"type":"message","content":[{"type":"output_text","text":"first"}]}}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"type":"summary_text","text":"thought"}]}}),
            json!({"type":"response.output_item.added","item":{"type":"reasoning","content":[{"type":"reasoning_text","text":"thought"}]}}),
            json!({"type":"response.output_item.added","item":{"type":"function_call","name":"tool","arguments":"{}"}}),
            json!({"type":"response.output_item.done","item":{"type":"custom_tool_call","name":"apply_patch","input":"patch"}}),
            json!({"type":"response.output_item.done","item":{"type":"mcp_call","name":"tool","arguments":"{}"}}),
            json!({"type":"response.output_item.done","item":{"type":"code_interpreter_call","code":"print(1)"}}),
        ] {
            let mut observer = Observer::new(Wire::Responses, Instant::now());
            observer.feed(&event(value.clone()));
            assert!(
                observer.latency_ms().is_some(),
                "正文可在 added/done 首次出现: {value}"
            );
        }
    }

    #[test]
    fn responses_done_payloads_are_content_when_no_delta_preceded() {
        for value in [
            json!({"type":"response.output_text.done","text":"first"}),
            json!({"type":"response.refusal.done","refusal":"declined"}),
            json!({"type":"response.reasoning_text.done","text":"thought"}),
            json!({"type":"response.reasoning_summary_text.done","text":"thought"}),
            json!({"type":"response.function_call_arguments.done","arguments":"{}"}),
            json!({"type":"response.custom_tool_call_input.done","input":"patch"}),
            json!({"type":"response.mcp_call_arguments.done","arguments":"{}"}),
            json!({"type":"response.code_interpreter_call_code.done","code":"print(1)"}),
            json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"first"}]}]}}),
        ] {
            let mut observer = Observer::new(Wire::Responses, Instant::now());
            observer.feed(&event(value.clone()));
            assert!(
                observer.latency_ms().is_some(),
                "done 中的非空正文同样已交付: {value}"
            );
        }
    }

    #[test]
    fn responses_empty_snapshots_and_errors_do_not_count_as_content() {
        for value in [
            json!({"type":"response.content_part.added","part":{"type":"output_text","text":""}}),
            json!({"type":"response.output_item.added","item":{"type":"function_call","name":"tool","arguments":""}}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"opaque","summary":[]}}),
            json!({"type":"response.output_text.done","text":""}),
            json!({"type":"response.completed","response":{"output":[],"usage":{"output_tokens":1}}}),
            json!({"type":"response.failed","response":{"error":{"message":"failed"},"output":[{"type":"message","content":[{"type":"output_text","text":"not-delivered-output"}]}]}}),
        ] {
            let mut observer = Observer::new(Wire::Responses, Instant::now());
            observer.feed(&event(value.clone()));
            assert_eq!(
                observer.latency_ms(),
                None,
                "状态、空占位、加密内容和错误不能计时: {value}"
            );
        }
    }

    #[test]
    fn multiple_data_lines_and_coalesced_terminal_keep_first_observation() {
        let mut observer = Observer::new(Wire::Chat, Instant::now());
        observer.feed(
            b"data: {\"choices\":\ndata: [{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n",
        );
        let first = observer.latency_ms();
        assert!(first.is_some());
        observer.feed(&vec![b'x'; MAX_FRAME_BYTES * 2]);
        assert_eq!(observer.latency_ms(), first);
        assert!(!observer.observing());
    }

    #[test]
    fn oversized_complete_and_partial_frames_stop_without_guessing() {
        for complete in [false, true] {
            let mut observer = Observer::new(Wire::Chat, Instant::now());
            let mut bytes = b"data: ".to_vec();
            bytes.extend(vec![b'x'; MAX_FRAME_BYTES]);
            if complete {
                bytes.extend_from_slice(b"\n\n");
            }
            observer.feed(&bytes);
            observer.feed(&event(json!({"choices":[{"delta":{"content":"later"}}]})));
            assert_eq!(observer.latency_ms(), None);
            assert!(!observer.observing());
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Instant;

    use aisix_core::{snapshot::SnapshotHandle, AisixSnapshot, ProxyConfig};
    use aisix_gateway::Hub;
    use aisix_obs::{UsageEvent, UsageEventLabels, UsageSink};
    use axum::body::{Body, Bytes, HttpBody};
    use axum::http::Method;
    use http_body::Frame;

    use crate::{attribution, operation, usage_attr};
    use crate::{ClientCancelGuard, DrainGuard, GuardPhase, ProxyState, TelemetryBody};

    #[derive(Clone, Copy)]
    enum End {
        Data,
        Pending,
        Eof,
        Error,
    }

    // 明确制造 complete-before-yield：终态发布发生在 inner poll 内。
    struct CompletesBeforeReturn {
        state: ProxyState,
        end: End,
        emitted: bool,
    }

    impl HttpBody for CompletesBeforeReturn {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if !self.emitted {
                self.emitted = true;
                let fixture = UsageEvent::default();
                let event = UsageEvent {
                    // NO-GUARDRAIL-CHAIN: body poll 组件夹具没有请求或 guardrail 链。
                    // NO-CALLER-IDENTITY: 该夹具没有认证调用者，只验证发布时序。
                    status_code: 200,
                    request_id: "first-token-poll".into(),
                    request_headers: fixture.request_headers,
                    ..fixture
                };
                usage_attr::emit_usage(
                    &self.state,
                    &self.state.snapshot.load(),
                    operation::COMPLETIONS,
                    event,
                    UsageEventLabels::default(),
                    None,
                    None,
                    true,
                    false,
                );
            }
            match self.end {
                End::Data => Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(
                    b"data: {\"choices\":[{\"text\":\"first\"}]}\n\ndata: [DONE]\n\n",
                ))))),
                End::Pending => Poll::Pending,
                End::Eof => Poll::Ready(None),
                End::Error => {
                    Poll::Ready(Some(Err(std::io::Error::other("controlled body error"))))
                }
            }
        }
    }

    #[tokio::test]
    async fn each_poll_result_flushes_before_return_without_waiting_for_drop() {
        for end in [End::Data, End::Pending, End::Eof, End::Error] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let config: ProxyConfig =
                serde_json::from_value(serde_json::json!({"addr":"127.0.0.1:0"})).unwrap();
            let state = ProxyState::new(
                SnapshotHandle::new(AisixSnapshot::new()),
                Arc::new(Hub::new()),
                &config,
            )
            .without_cache()
            .with_usage_sink(UsageSink::new(tx));
            let cell = Arc::new(attribution::RequestAttribution::default());
            let started = Instant::now();
            cell.observe_first_token(super::Wire::Completions, started);
            let guard = ClientCancelGuard {
                phase: GuardPhase::Body {
                    owed: true,
                    polled: false,
                },
                state: state.clone(),
                attribution: cell.clone(),
                endpoint: "/v1/completions",
                method: Method::POST,
                uri: "/v1/completions".parse().unwrap(),
                request_id: "first-token-poll".into(),
                trace: None,
                started,
            };
            let mut body = TelemetryBody {
                inner: Some(Body::new(CompletesBeforeReturn {
                    state: state.clone(),
                    end,
                    emitted: false,
                })),
                drain: Some(DrainGuard::new(state.livez.clone())),
                guard: Some(guard),
            };
            let mut cx = Context::from_waker(futures::task::noop_waker_ref());
            let _ = Pin::new(&mut body).poll_frame(&mut cx);
            let event = rx
                .try_recv()
                .expect("same-poll delivery must not depend on another poll or drop");
            assert_eq!(
                event.first_token_latency_ms.is_some(),
                matches!(end, End::Data)
            );
            assert!(event.is_final_attempt);
            attribution::sync_scope(&cell, || assert!(!attribution::body_poll_in_progress()));
            drop(body);
            assert!(
                rx.try_recv().is_err(),
                "cancel guard must not duplicate the terminal event"
            );
        }
    }

    #[test]
    fn host_passthrough_cannot_arm_a_detector_for_a_typed_looking_path() {
        let cell = Arc::new(attribution::RequestAttribution::default());
        attribution::sync_scope(&cell, || {
            attribution::note_passthrough_route("host-route");
            cell.observe_first_token(super::Wire::Chat, Instant::now());
            cell.begin_body_poll();
            assert!(!attribution::body_poll_in_progress());
            cell.observe_response_output(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"not-typed\"}}]}\n\n",
            );
            assert_eq!(attribution::first_token_latency_ms(), None);
            cell.finish_body_poll();
        });
    }
}
