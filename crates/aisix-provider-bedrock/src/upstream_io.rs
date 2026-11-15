//! SDK 在最终序列化、签名之后观察请求；在解码之前观察原始响应。

use std::sync::{Arc, OnceLock};

use aisix_core::usage_io::TextMeasurement;
use aisix_gateway::upstream_io::{ObservedBody, Protocol, UpstreamIoObserver};
use aws_sdk_bedrockruntime::config::interceptors::{
    BeforeDeserializationInterceptorContextMut, BeforeTransmitInterceptorContextMut,
};
use aws_sdk_bedrockruntime::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_bedrockruntime::types::{
    ContentBlockDelta, ContentBlockStart, ConverseStreamOutput, ReasoningContentBlockDelta,
    ToolResultBlockDelta,
};
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::{Document, Number};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};

/// 借用 SDK 的 JSON 树，直接向标量写入器序列化；不复制其中的大字符串。
struct DocumentRef<'a>(&'a Document);

impl Serialize for DocumentRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Document::Null => serializer.serialize_none(),
            Document::Bool(value) => serializer.serialize_bool(*value),
            Document::String(value) => serializer.serialize_str(value),
            Document::Number(Number::PosInt(value)) => serializer.serialize_u64(*value),
            Document::Number(Number::NegInt(value)) => serializer.serialize_i64(*value),
            Document::Number(Number::Float(value)) => serializer.serialize_f64(*value),
            Document::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&DocumentRef(value))?;
                }
                sequence.end()
            }
            Document::Object(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, &DocumentRef(value))?;
                }
                map.end()
            }
        }
    }
}

pub(crate) fn observe_document(io: &UpstreamIoObserver, document: &Document) {
    if io
        .observe_output_serialized(&DocumentRef(document))
        .is_err()
    {
        io.mark_output_text_partial();
    }
}

pub(crate) fn observe_native_event(io: &UpstreamIoObserver, bytes: &[u8]) {
    io.observe_response_event_bytes(bytes);
}

/// 每个 SDK operation 一份；SDK 重试保持禁用，顺序子调用各自注册交换。
#[derive(Clone, Debug)]
pub(crate) struct UpstreamIoInterceptor {
    protocol: Protocol,
    io: Arc<OnceLock<UpstreamIoObserver>>,
    input: Option<TextMeasurement>,
}

impl UpstreamIoInterceptor {
    pub(crate) fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            io: Arc::new(OnceLock::new()),
            input: None,
        }
    }

    pub(crate) fn with_model_input(mut self, input: &serde_json::Value) -> Self {
        // CountTokens 把已成形的模型请求编码进 base64；只保留模型文本长度。
        let observer = UpstreamIoObserver::new(self.protocol);
        observer.observe_request_json(input);
        self.input = observer.snapshot().input_text;
        self
    }

    pub(crate) fn observer(&self) -> Option<&UpstreamIoObserver> {
        self.io.get()
    }

    pub(crate) fn converse_event(&self, event: &ConverseStreamOutput) {
        let Some(io) = self.observer() else { return };
        match event {
            ConverseStreamOutput::ContentBlockDelta(event) => match event.delta() {
                Some(ContentBlockDelta::Text(text)) => io.observe_output_text(text),
                Some(ContentBlockDelta::ToolUse(tool)) => io.observe_output_text(tool.input()),
                Some(ContentBlockDelta::ReasoningContent(ReasoningContentBlockDelta::Text(
                    text,
                ))) => io.observe_output_text(text),
                Some(ContentBlockDelta::ReasoningContent(
                    ReasoningContentBlockDelta::Signature(_),
                )) => {}
                Some(ContentBlockDelta::ToolResult(parts)) => {
                    for part in parts {
                        match part {
                            ToolResultBlockDelta::Text(text) => io.observe_output_text(text),
                            ToolResultBlockDelta::Json(json) => observe_document(io, json),
                            _ => io.mark_output_text_partial(),
                        }
                    }
                }
                Some(ContentBlockDelta::Citation(_)) => {}
                _ => io.mark_output_text_partial(),
            },
            ConverseStreamOutput::ContentBlockStart(event) => match event.start() {
                None
                | Some(ContentBlockStart::ToolUse(_))
                | Some(ContentBlockStart::ToolResult(_)) => {}
                _ => io.mark_output_text_partial(),
            },
            ConverseStreamOutput::MessageStop(_) => io.finish_output_text(),
            ConverseStreamOutput::MessageStart(_) => io.observe_output_text(""),
            ConverseStreamOutput::ContentBlockStop(_) | ConverseStreamOutput::Metadata(_) => {}
            _ => io.mark_output_text_partial(),
        }
    }
}

impl Intercept for UpstreamIoInterceptor {
    fn name(&self) -> &'static str {
        "AisixBedrockUpstreamIo"
    }

    fn modify_before_transmit(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        // SDK 已完成构建、认证与签名；观察者注册本身不代表 HTTP 发送。
        aisix_gateway::response_tap::note_http_dispatch();
        // 只有真正进入传输的 operation 才注册；认证或序列化失败没有虚构交换。
        let io = self
            .io
            .get_or_init(|| UpstreamIoObserver::observed(self.protocol))
            .clone();
        if let Some(input) = self.input {
            io.observe_input_measurement(input);
        }
        let body = std::mem::replace(context.request_mut().body_mut(), SdkBody::taken());
        // SDK 官方的进度观察适配器保留签名所读的原始内容和长度，不改字节。
        *context.request_mut().body_mut() = body.map_preserve_contents(move |body| {
            SdkBody::from_body_1_x(ObservedBody::request(body, io.clone()))
        });
        Ok(())
    }

    fn modify_before_deserialization(
        &self,
        context: &mut BeforeDeserializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let Some(io) = self.observer().cloned() else {
            return Ok(());
        };
        let response = context.response_mut();
        io.begin_response(response.headers().get("content-type"));
        let body = std::mem::replace(response.body_mut(), SdkBody::taken());
        // 响应不保留 bytes 快捷视图，确保 SDK 的完整读取也经过真实 frame 观察。
        *response.body_mut() = SdkBody::from_body_1_x(ObservedBody::response(body, io));
        Ok(())
    }
}
