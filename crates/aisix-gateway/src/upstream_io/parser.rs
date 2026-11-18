//! 有界 JSON/SSE 解析器；只保留路径、有限标签与计数。

use super::Protocol;
use aisix_core::usage_io::TextMeasurement;
use serde_json::Value;
use std::time::Instant;

#[derive(Clone, Copy, Default)]
struct Count {
    bytes: u64,
    chars: u64,
    seen: bool,
}

impl Count {
    fn character(&mut self, ch: char) {
        self.seen = true;
        self.bytes = self.bytes.saturating_add(ch.len_utf8() as u64);
        self.chars = self.chars.saturating_add(1);
    }

    fn text(&mut self, text: &str) {
        self.seen = true;
        self.bytes = self.bytes.saturating_add(text.len() as u64);
        self.chars = self.chars.saturating_add(text.chars().count() as u64);
    }

    fn add(&mut self, other: Self) {
        self.seen |= other.seen;
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.chars = self.chars.saturating_add(other.chars);
    }

    fn measurement(self, complete: bool) -> Option<TextMeasurement> {
        self.seen.then_some(TextMeasurement {
            utf8_bytes: self.bytes,
            unicode_scalars: self.chars,
            complete,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    None,
    Primary,
    Delta,
    Done,
    Snapshot,
    Content,
}

#[derive(Clone, Copy)]
struct Frame {
    array: bool,
    phase: u8,
    key: &'static str,
}

struct Path<'a>(&'a [Frame]);

impl Path<'_> {
    fn is(&self, keys: &[&str]) -> bool {
        self.0.len() == keys.len()
            && self
                .0
                .iter()
                .zip(keys)
                .all(|(frame, key)| (if frame.array { "#" } else { frame.key }) == *key)
    }

    fn starts(&self, keys: &[&str]) -> bool {
        self.0.len() >= keys.len()
            && self
                .0
                .iter()
                .zip(keys)
                .all(|(frame, key)| (if frame.array { "#" } else { frame.key }) == *key)
    }

    fn last(&self) -> &'static str {
        self.0
            .last()
            .map_or("", |frame| if frame.array { "#" } else { frame.key })
    }
}

#[derive(Default, Clone)]
struct Event {
    primary: Count,
    delta: Count,
    done: Count,
    snapshot: Count,
    kind: Label,
    block_kind: Label,
    block_index: Option<u32>,
    empty_tool_input: bool,
    nonempty_tool_delta: bool,
    incomplete: bool,
    response_key: u64,
    content: bool,
}

impl Event {
    fn add(&mut self, target: Target, count: Count) {
        match target {
            Target::Primary => self.primary.add(count),
            Target::Delta => self.delta.add(count),
            Target::Done => self.done.add(count),
            Target::Snapshot => self.snapshot.add(count),
            Target::Content => self.content |= count.bytes != 0,
            Target::None => {}
        }
    }
}

#[derive(Clone, Copy)]
struct Label {
    bytes: [u8; 96],
    len: usize,
    overflow: bool,
}

impl Default for Label {
    fn default() -> Self {
        Self {
            bytes: [0; 96],
            len: 0,
            overflow: false,
        }
    }
}

impl Label {
    fn character(&mut self, ch: char) {
        let mut bytes = [0; 4];
        let encoded = ch.encode_utf8(&mut bytes).as_bytes();
        if self.len + encoded.len() <= self.bytes.len() {
            self.bytes[self.len..self.len + encoded.len()].copy_from_slice(encoded);
            self.len += encoded.len();
        } else {
            self.overflow = true;
        }
    }

    fn text(&self) -> &str {
        if self.overflow {
            ""
        } else {
            std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
        }
    }
}

#[derive(Clone, Copy)]
enum Escape {
    None,
    Slash,
    Unicode {
        digits: u8,
        value: u16,
        high: Option<u16>,
    },
    LowSlash(u16),
    LowU(u16),
}

#[derive(Clone)]
struct JsonString {
    key: bool,
    metadata: bool,
    target: Target,
    count: Count,
    compact_count: Count,
    label: Label,
    escape: Escape,
    utf8_left: u8,
    utf8_value: u32,
    utf8_min: u32,
}

impl JsonString {
    fn new(key: bool, metadata: bool, target: Target) -> Self {
        Self {
            key,
            metadata,
            target,
            count: Count {
                seen: true,
                ..Count::default()
            },
            compact_count: Count::default(),
            label: Label::default(),
            escape: Escape::None,
            utf8_left: 0,
            utf8_value: 0,
            utf8_min: 0,
        }
    }

    fn decoded(&mut self, ch: char) {
        self.count.character(ch);
        match ch {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => self.compact_count.text("xx"),
            '\0'..='\u{1f}' => self.compact_count.text("xxxxxx"),
            _ => self.compact_count.character(ch),
        }
        if self.key || self.metadata {
            self.label.character(ch);
        }
    }

    fn byte(&mut self, byte: u8) -> Result<bool, ()> {
        match self.escape {
            Escape::Slash => {
                self.escape = Escape::None;
                match byte {
                    b'"' => self.decoded('"'),
                    b'\\' => self.decoded('\\'),
                    b'/' => self.decoded('/'),
                    b'b' => self.decoded('\u{8}'),
                    b'f' => self.decoded('\u{c}'),
                    b'n' => self.decoded('\n'),
                    b'r' => self.decoded('\r'),
                    b't' => self.decoded('\t'),
                    b'u' => {
                        self.escape = Escape::Unicode {
                            digits: 0,
                            value: 0,
                            high: None,
                        }
                    }
                    _ => return Err(()),
                }
                return Ok(false);
            }
            Escape::LowSlash(high) => {
                if byte != b'\\' {
                    return Err(());
                }
                self.escape = Escape::LowU(high);
                return Ok(false);
            }
            Escape::LowU(high) => {
                if byte != b'u' {
                    return Err(());
                }
                self.escape = Escape::Unicode {
                    digits: 0,
                    value: 0,
                    high: Some(high),
                };
                return Ok(false);
            }
            Escape::Unicode {
                digits,
                value,
                high,
            } => {
                let hex = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => return Err(()),
                };
                let value = (value << 4) | u16::from(hex);
                if digits == 3 {
                    self.escape = Escape::None;
                    let scalar = if let Some(high) = high {
                        if !(0xdc00..=0xdfff).contains(&value) {
                            return Err(());
                        }
                        0x10000 + ((u32::from(high) - 0xd800) << 10) + u32::from(value) - 0xdc00
                    } else if (0xd800..=0xdbff).contains(&value) {
                        self.escape = Escape::LowSlash(value);
                        return Ok(false);
                    } else {
                        u32::from(value)
                    };
                    self.decoded(char::from_u32(scalar).ok_or(())?);
                } else {
                    self.escape = Escape::Unicode {
                        digits: digits + 1,
                        value,
                        high,
                    };
                }
                return Ok(false);
            }
            Escape::None => {}
        }
        if self.utf8_left != 0 {
            if byte & 0xc0 != 0x80 {
                return Err(());
            }
            self.utf8_value = (self.utf8_value << 6) | u32::from(byte & 0x3f);
            self.utf8_left -= 1;
            if self.utf8_left == 0 {
                if self.utf8_value < self.utf8_min {
                    return Err(());
                }
                self.decoded(char::from_u32(self.utf8_value).ok_or(())?);
            }
            return Ok(false);
        }
        match byte {
            b'"' => return Ok(true),
            b'\\' => self.escape = Escape::Slash,
            0..=0x1f => return Err(()),
            0x20..=0x7f => self.decoded(char::from(byte)),
            0xc2..=0xdf => {
                self.utf8_left = 1;
                self.utf8_value = u32::from(byte & 0x1f);
                self.utf8_min = 0x80;
            }
            0xe0..=0xef => {
                self.utf8_left = 2;
                self.utf8_value = u32::from(byte & 0x0f);
                self.utf8_min = 0x800;
            }
            0xf0..=0xf4 => {
                self.utf8_left = 3;
                self.utf8_value = u32::from(byte & 0x07);
                self.utf8_min = 0x10000;
            }
            _ => return Err(()),
        }
        Ok(false)
    }
}

fn known_key(key: &str) -> &'static str {
    match key {
        "choices" => "choices",
        "message" => "message",
        "delta" => "delta",
        "content" => "content",
        "text" => "text",
        "refusal" => "refusal",
        "reasoning" => "reasoning",
        "reasoning_content" => "reasoning_content",
        "tool_calls" => "tool_calls",
        "function" => "function",
        "function_call" => "function_call",
        "arguments" => "arguments",
        "type" => "type",
        "output" => "output",
        "response" => "response",
        "summary" => "summary",
        "thinking" => "thinking",
        "partial_json" => "partial_json",
        "content_block" => "content_block",
        "input" => "input",
        "finish_reason" => "finish_reason",
        "data" => "data",
        "revised_prompt" => "revised_prompt",
        "error" => "error",
        "index" => "index",
        "messages" => "messages",
        "system" => "system",
        "instructions" => "instructions",
        "prompt" => "prompt",
        "suffix" => "suffix",
        "tools" => "tools",
        "functions" => "functions",
        "response_format" => "response_format",
        "format" => "format",
        "parameters" => "parameters",
        "documents" => "documents",
        "query" => "query",
        "inputText" => "inputText",
        "texts" => "texts",
        "contents" => "contents",
        "parts" => "parts",
        "part" => "part",
        "code" => "code",
        "systemInstruction" => "systemInstruction",
        "functionCall" => "functionCall",
        "functionResponse" => "functionResponse",
        "args" => "args",
        "generationConfig" => "generationConfig",
        "responseSchema" => "responseSchema",
        "responseJsonSchema" => "responseJsonSchema",
        "candidates" => "candidates",
        "finishReason" => "finishReason",
        "toolConfig" => "toolConfig",
        "toolUse" => "toolUse",
        "toolResult" => "toolResult",
        "reasoningContent" => "reasoningContent",
        "reasoningText" => "reasoningText",
        "instances" => "instances",
        "item" => "item",
        "transcript" => "transcript",
        "response_id" => "response_id",
        "id" => "id",
        "status" => "status",
        "session" => "session",
        "previous_response_id" => "previous_response_id",
        "conversation" => "conversation",
        "inlineData" => "inlineData",
        "fileData" => "fileData",
        "image" => "image",
        "image_url" => "image_url",
        "audio" => "audio",
        "document" => "document",
        "video" => "video",
        "encrypted_content" => "encrypted_content",
        "b64_json" => "b64_json",
        "url" => "url",
        "embedding" => "embedding",
        "predictions" => "predictions",
        "embeddings" => "embeddings",
        "values" => "values",
        _ => "",
    }
}

#[derive(Clone)]
struct Compact {
    depth: usize,
    count: Count,
    object: bool,
}

#[derive(Clone)]
struct JsonObserver {
    protocol: Protocol,
    sse: bool,
    request: bool,
    stack: Vec<Frame>,
    string: Option<JsonString>,
    primitive: Label,
    in_primitive: bool,
    root_done: bool,
    invalid: bool,
    event: Event,
    compact: Option<Compact>,
}

impl JsonObserver {
    fn new(protocol: Protocol, sse: bool) -> Self {
        Self {
            protocol,
            sse,
            request: false,
            stack: Vec::with_capacity(16),
            string: None,
            primitive: Label::default(),
            in_primitive: false,
            root_done: false,
            invalid: false,
            event: Event::default(),
            compact: None,
        }
    }

    fn reset(&mut self) {
        self.stack.clear();
        self.string = None;
        self.primitive = Label::default();
        self.in_primitive = false;
        self.root_done = false;
        self.invalid = false;
        self.event = Event::default();
        self.compact = None;
    }

    fn expects_value(&self) -> bool {
        self.stack.last().map_or(!self.root_done, |frame| {
            if frame.array {
                frame.phase == 0 || frame.phase == 2
            } else {
                frame.phase == 2
            }
        })
    }

    fn value_done(&mut self) {
        if let Some(frame) = self.stack.last_mut() {
            frame.phase = if frame.array { 1 } else { 3 };
        } else {
            self.root_done = true;
        }
    }

    fn output_item_type(&self) -> bool {
        if self.request {
            return false;
        }
        if self.protocol != Protocol::Responses {
            return false;
        }
        let path = Path(&self.stack);
        if self.sse {
            path.is(&["response", "output", "#", "type"])
        } else {
            path.is(&["output", "#", "type"])
        }
    }

    fn request_target(&self) -> Target {
        let p = Path(&self.stack);
        let selected = match self.protocol {
            Protocol::Chat | Protocol::Messages | Protocol::Converse => {
                p.is(&["system"])
                    || p.is(&["system", "#", "text"])
                    || p.is(&["messages", "#", "content"])
                    || (p.starts(&["messages", "#", "content", "#"])
                        && matches!(p.last(), "text" | "thinking" | "refusal"))
                    || (self.protocol == Protocol::Chat
                        && p.is(&["messages", "#", "audio", "transcript"]))
                    || p.is(&["messages", "#", "reasoning_content"])
                    || p.is(&["messages", "#", "refusal"])
                    || p.is(&["messages", "#", "function_call", "arguments"])
                    || p.is(&["messages", "#", "tool_calls", "#", "function", "arguments"])
                    || (p.starts(&["messages", "#", "content", "#", "content"])
                        && matches!(p.last(), "text" | "#"))
                    || (p.starts(&["messages", "#", "content", "#", "toolResult", "content"])
                        && p.last() == "text")
            }
            Protocol::Responses | Protocol::Realtime => {
                p.is(&["instructions"])
                    || p.is(&["input"])
                    || p.is(&["response", "instructions"])
                    || p.is(&["response", "input"])
                    || p.is(&["session", "instructions"])
                    || ((p.starts(&["input", "#"])
                        || p.starts(&["response", "input", "#"])
                        || p.starts(&["item"]))
                        && matches!(
                            p.last(),
                            "text" | "transcript" | "arguments" | "input" | "output" | "content"
                        ))
            }
            Protocol::Completions => {
                p.is(&["prompt"]) || p.is(&["prompt", "#"]) || p.is(&["suffix"])
            }
            Protocol::Embeddings => {
                p.is(&["input"])
                    || p.is(&["input", "#"])
                    || p.is(&["inputText"])
                    || p.is(&["texts", "#"])
                    || p.is(&["instances", "#", "content"])
            }
            Protocol::Rerank => {
                p.is(&["query"]) || p.is(&["documents", "#"]) || p.is(&["documents", "#", "text"])
            }
            Protocol::Speech => p.is(&["input"]) || p.is(&["instructions"]),
            Protocol::Images | Protocol::Videos | Protocol::Transcription => p.is(&["prompt"]),
            Protocol::Gemini => {
                p.is(&["contents", "#", "parts", "#", "text"])
                    || p.is(&["systemInstruction", "parts", "#", "text"])
            }
            Protocol::Unknown => false,
        };
        if selected {
            Target::Primary
        } else {
            Target::None
        }
    }

    fn request_container(&mut self, object: bool) {
        let p = Path(&self.stack);
        if matches!(
            p.last(),
            "image" | "image_url" | "inlineData" | "fileData" | "audio" | "document" | "video"
        ) {
            self.event.incomplete = true;
        }
        let compact = p.is(&["tools"])
            || p.is(&["functions"])
            || p.is(&["response_format"])
            || p.is(&["text", "format"])
            || p.is(&["toolConfig"])
            || p.is(&["session", "tools"])
            || p.is(&["response", "tools"])
            || p.is(&["generationConfig", "responseSchema"])
            || p.is(&["generationConfig", "responseJsonSchema"])
            || p.is(&["messages", "#", "content", "#", "input"])
            || p.is(&["messages", "#", "content", "#", "toolUse", "input"])
            || p.is(&["contents", "#", "parts", "#", "functionCall", "args"])
            || p.is(&[
                "contents",
                "#",
                "parts",
                "#",
                "functionResponse",
                "response",
            ]);
        if compact && self.compact.is_none() {
            self.compact = Some(Compact {
                depth: self.stack.len(),
                count: Count::default(),
                object,
            });
        }
    }

    fn target(&self) -> Target {
        if self.compact.is_some() {
            return Target::None;
        }
        if self.request {
            return self.request_target();
        }
        let path = Path(&self.stack);
        match self.protocol {
            Protocol::Chat => {
                let choices = path.starts(&["choices", "#"]);
                let message = self.stack.get(2).map(|frame| frame.key);
                let envelope = matches!(message, Some("message" | "delta"));
                if choices
                    && envelope
                    && ((self.stack.len() == 4
                        && matches!(
                            path.last(),
                            "content" | "reasoning" | "reasoning_content" | "refusal"
                        ))
                        || (self.stack.len() == 6
                            && path.last() == "text"
                            && self.stack[3].key == "content")
                        || (self.stack.len() == 7
                            && path.last() == "arguments"
                            && self.stack[3].key == "tool_calls"
                            && self.stack[5].key == "function")
                        || (self.stack.len() == 5
                            && path.last() == "arguments"
                            && self.stack[3].key == "function_call")
                        || (self.stack.len() == 5
                            && path.last() == "transcript"
                            && self.stack[3].key == "audio"))
                {
                    Target::Primary
                } else {
                    Target::None
                }
            }
            Protocol::Completions => {
                if path.is(&["choices", "#", "text"]) {
                    Target::Primary
                } else {
                    Target::None
                }
            }
            Protocol::Messages => {
                if (path.starts(&["content", "#"])
                    && self.stack.len() == 3
                    && matches!(path.last(), "text" | "thinking" | "refusal"))
                    || (path.starts(&["content_block"])
                        && self.stack.len() == 2
                        && matches!(path.last(), "text" | "thinking" | "refusal"))
                    || (path.starts(&["delta"])
                        && self.stack.len() == 2
                        && matches!(path.last(), "text" | "thinking" | "partial_json"))
                {
                    Target::Primary
                } else {
                    Target::None
                }
            }
            Protocol::Responses => {
                if self.sse && path.is(&["delta"]) {
                    return Target::Delta;
                }
                if self.sse
                    && (path.is(&["text"])
                        || path.is(&["arguments"])
                        || path.is(&["refusal"])
                        || path.is(&["input"])
                        || path.is(&["code"]))
                {
                    return Target::Done;
                }
                // added/done 的正文只供首内容识别，不重复加入已有长度计数。
                if self.sse
                    && (path.is(&["part", "text"])
                        || path.is(&["part", "refusal"])
                        || path.is(&["response", "output", "#", "code"])
                        || (path.starts(&["item"])
                            && ((self.stack.len() == 2
                                && matches!(path.last(), "arguments" | "input" | "code"))
                                || (self.stack.len() == 4
                                    && matches!(self.stack[1].key, "content" | "summary")
                                    && matches!(path.last(), "text" | "refusal")))))
                {
                    return Target::Content;
                }
                let prefix = if self.sse {
                    &["response", "output", "#"][..]
                } else {
                    &["output", "#"][..]
                };
                if path.starts(prefix) {
                    let rest = self.stack.len() - prefix.len();
                    if (rest == 1 && matches!(path.last(), "arguments" | "input"))
                        || (rest == 3
                            && matches!(path.last(), "text" | "refusal")
                            && matches!(self.stack[prefix.len()].key, "content" | "summary"))
                    {
                        Target::Snapshot
                    } else {
                        Target::None
                    }
                } else {
                    Target::None
                }
            }
            Protocol::Gemini => {
                if path.is(&["candidates", "#", "content", "parts", "#", "text"]) {
                    Target::Primary
                } else {
                    Target::None
                }
            }
            Protocol::Converse => {
                if path.is(&["output", "message", "content", "#", "text"])
                    || path.is(&[
                        "output",
                        "message",
                        "content",
                        "#",
                        "reasoningContent",
                        "reasoningText",
                        "text",
                    ])
                    || path.is(&["delta", "text"])
                {
                    Target::Primary
                } else {
                    Target::None
                }
            }
            Protocol::Realtime => {
                if self.sse && path.is(&["delta"]) {
                    return Target::Delta;
                }
                if path.starts(&["response", "output", "#"])
                    && (path.is(&["response", "output", "#", "arguments"])
                        || (self.stack.len() == 6 && matches!(path.last(), "text" | "transcript")))
                {
                    Target::Snapshot
                } else {
                    Target::None
                }
            }
            Protocol::Transcription => {
                if self.sse && path.is(&["delta"]) {
                    return Target::Delta;
                }
                if self.sse && path.is(&["text"]) {
                    return Target::Snapshot;
                }
                if path.is(&["text"]) || path.is(&["transcript"]) {
                    Target::Primary
                } else {
                    Target::None
                }
            }
            Protocol::Images => {
                if path.is(&["data", "#", "revised_prompt"]) {
                    Target::Primary
                } else {
                    Target::None
                }
            }
            _ => Target::None,
        }
    }

    fn opaque_string(&self) -> bool {
        let path = Path(&self.stack);
        match self.protocol {
            Protocol::Responses => {
                path.is(&["input", "#", "encrypted_content"])
                    || path.is(&["response", "input", "#", "encrypted_content"])
                    || path.is(&["output", "#", "encrypted_content"])
                    || path.is(&["response", "output", "#", "encrypted_content"])
                    || path.is(&["item", "encrypted_content"])
                    || (self.request
                        && (path.is(&["conversation", "id"])
                            || path.is(&["prompt", "id"])
                            || path.is(&["response", "conversation", "id"])
                            || path.is(&["response", "prompt", "id"])))
            }
            Protocol::Images if !self.request => {
                path.is(&["data", "#", "b64_json"]) || path.is(&["data", "#", "url"])
            }
            _ => false,
        }
    }

    fn container(&mut self, object: bool) {
        if self.request {
            self.request_container(object);
            return;
        }
        let path = Path(&self.stack);
        if self.protocol == Protocol::Chat
            && (path.is(&["choices", "#", "message", "audio"])
                || path.is(&["choices", "#", "delta", "audio"]))
        {
            self.event.incomplete = true;
        }
        match self.protocol {
            Protocol::Embeddings
                if !object
                    && (path.is(&["embedding"])
                        || path.is(&["data", "#", "embedding"])
                        || path.is(&["predictions", "#", "embeddings", "values"])) =>
            {
                self.event.primary.seen = true
            }
            Protocol::Chat | Protocol::Completions if path.is(&["choices"]) => {
                self.event.primary.seen = true
            }
            Protocol::Messages if !self.sse && path.is(&["content"]) => {
                self.event.primary.seen = true
            }
            Protocol::Responses
                if (!self.sse && path.is(&["output"]))
                    || (self.sse && path.is(&["response", "output"])) =>
            {
                self.event.snapshot.seen = true
            }
            Protocol::Gemini if path.is(&["candidates"]) => self.event.primary.seen = true,
            Protocol::Converse if path.is(&["output", "message", "content"]) => {
                self.event.primary.seen = true
            }
            Protocol::Realtime if path.is(&["response", "output"]) => {
                self.event.snapshot.seen = true
            }
            _ => {}
        }
        if matches!(self.protocol, Protocol::Gemini | Protocol::Converse)
            && matches!(
                path.last(),
                "inlineData" | "fileData" | "image" | "audio" | "document" | "video"
            )
        {
            self.event.incomplete = true;
        }
        if path.is(&["error"]) {
            self.event.incomplete = true;
        }
        if ((self.protocol == Protocol::Messages
            && (path.is(&["content", "#", "input"]) || path.is(&["content_block", "input"])))
            || (self.protocol == Protocol::Gemini
                && path.is(&[
                    "candidates",
                    "#",
                    "content",
                    "parts",
                    "#",
                    "functionCall",
                    "args",
                ]))
            || (self.protocol == Protocol::Converse
                && path.is(&["output", "message", "content", "#", "toolUse", "input"])))
            && self.compact.is_none()
        {
            self.compact = Some(Compact {
                depth: self.stack.len(),
                count: Count::default(),
                object,
            });
        }
    }

    fn byte(&mut self, byte: u8) {
        if self.invalid {
            return;
        }
        if let Some(mut string) = self.string.take() {
            match string.byte(byte) {
                Ok(true) => {
                    if let Some(compact) = &mut self.compact {
                        compact.count.add(string.compact_count);
                        compact.count.character('"');
                    }
                    if string.key {
                        if let Some(frame) = self.stack.last_mut() {
                            frame.key = known_key(string.label.text());
                            frame.phase = 1;
                        } else {
                            self.invalid = true;
                        }
                    } else {
                        self.note_tool_delta(string.count);
                        if string.count.bytes != 0 && self.opaque_string() {
                            self.event.incomplete = true;
                        }
                        if self.request
                            && matches!(
                                Path(&self.stack).last(),
                                "previous_response_id" | "conversation"
                            )
                        {
                            self.event.incomplete = true;
                        }
                        self.event.add(string.target, string.count);
                        let path = Path(&self.stack);
                        if matches!(path.last(), "type")
                            && matches!(
                                string.label.text(),
                                "image_url"
                                    | "input_image"
                                    | "image"
                                    | "input_audio"
                                    | "audio"
                                    | "file"
                                    | "input_file"
                                    | "video"
                                    | "video_url"
                                    | "input_video"
                                    | "document"
                                    | "redacted_thinking"
                                    | "item_reference"
                                    | "compaction"
                                    | "image_generation_call"
                            )
                        {
                            self.event.incomplete = true;
                        }
                        if self.protocol == Protocol::Realtime
                            && path.is(&["response", "status"])
                            && string.label.text() != "completed"
                        {
                            self.event.incomplete = true;
                        }
                        if path.is(&["type"]) {
                            self.event.kind = string.label;
                        }
                        if self.protocol == Protocol::Realtime
                            && (path.is(&["response_id"]) || path.is(&["response", "id"]))
                        {
                            use std::hash::{Hash, Hasher};
                            let mut hash = std::collections::hash_map::DefaultHasher::new();
                            string.label.text().hash(&mut hash);
                            self.event.response_key = if string.label.overflow {
                                0
                            } else {
                                hash.finish().max(1)
                            };
                        }
                        if self.protocol == Protocol::Messages
                            && path.is(&["content_block", "type"])
                        {
                            self.event.block_kind = string.label;
                        }
                        if self.output_item_type()
                            && !matches!(
                                string.label.text(),
                                "message"
                                    | "reasoning"
                                    | "function_call"
                                    | "custom_tool_call"
                                    | "image_generation_call"
                            )
                        {
                            // 新工具载体可能含有文字；未解释的槽不能成为完整的零。
                            self.event.incomplete = true;
                        }
                        self.value_done();
                    }
                }
                Ok(false) => self.string = Some(string),
                Err(()) => {
                    self.string = Some(string);
                    self.invalid = true;
                }
            }
            return;
        }
        if self.in_primitive {
            if !matches!(byte, b' ' | b'\r' | b'\n' | b'\t' | b',' | b']' | b'}') {
                self.primitive.character(char::from(byte));
                if let Some(compact) = &mut self.compact {
                    compact.count.character(char::from(byte));
                }
                return;
            }
            self.finish_primitive();
            if self.invalid {
                return;
            }
        }
        match byte {
            b' ' | b'\r' | b'\n' | b'\t' => {}
            b'"' => {
                let key = self
                    .stack
                    .last()
                    .is_some_and(|frame| !frame.array && (frame.phase == 0 || frame.phase == 4));
                if !key && !self.expects_value() {
                    self.invalid = true;
                    return;
                }
                let metadata = (self.protocol == Protocol::Realtime
                    && (Path(&self.stack).is(&["response_id"])
                        || Path(&self.stack).is(&["response", "id"])
                        || Path(&self.stack).is(&["response", "status"])))
                    || Path(&self.stack).last() == "type"
                    || Path(&self.stack).is(&["type"])
                    || self.output_item_type()
                    || (self.protocol == Protocol::Messages
                        && Path(&self.stack).is(&["content_block", "type"]));
                if let Some(compact) = &mut self.compact {
                    compact.count.character('"');
                }
                self.string = Some(JsonString::new(
                    key,
                    metadata,
                    if key { Target::None } else { self.target() },
                ));
            }
            b'{' | b'[' => {
                if !self.expects_value() || self.stack.len() >= 64 {
                    self.invalid = true;
                    return;
                }
                self.container(byte == b'{');
                if let Some(compact) = &mut self.compact {
                    compact.count.character(char::from(byte));
                }
                self.stack.push(Frame {
                    array: byte == b'[',
                    phase: 0,
                    key: "",
                });
            }
            b'}' | b']' => {
                let Some(frame) = self.stack.pop() else {
                    self.invalid = true;
                    return;
                };
                let valid = frame.array == (byte == b']')
                    && if frame.array {
                        frame.phase == 0 || frame.phase == 1
                    } else {
                        frame.phase == 0 || frame.phase == 3
                    };
                if !valid {
                    self.invalid = true;
                    return;
                }
                if let Some(compact) = &mut self.compact {
                    compact.count.character(char::from(byte));
                }
                if self
                    .compact
                    .as_ref()
                    .is_some_and(|compact| compact.depth == self.stack.len())
                {
                    if let Some(compact) = self.compact.take() {
                        // 空对象在 block_stop 才结算；非空参数增量会替换它。
                        if self.sse && compact.object && compact.count.bytes == 2 {
                            self.event.empty_tool_input = true;
                        } else {
                            self.event.primary.add(compact.count);
                            self.event.incomplete |= self.sse;
                        }
                    }
                }
                self.value_done();
            }
            b':' => {
                if let Some(compact) = &mut self.compact {
                    compact.count.character(':');
                }
                if let Some(frame) = self
                    .stack
                    .last_mut()
                    .filter(|frame| !frame.array && frame.phase == 1)
                {
                    frame.phase = 2;
                } else {
                    self.invalid = true;
                }
            }
            b',' => {
                if let Some(compact) = &mut self.compact {
                    compact.count.character(',');
                }
                if let Some(frame) = self.stack.last_mut().filter(|frame| {
                    if frame.array {
                        frame.phase == 1
                    } else {
                        frame.phase == 3
                    }
                }) {
                    frame.phase = if frame.array { 2 } else { 4 };
                } else {
                    self.invalid = true;
                }
            }
            _ => {
                if !self.expects_value() {
                    self.invalid = true;
                    return;
                }
                self.in_primitive = true;
                self.primitive = Label::default();
                self.primitive.character(char::from(byte));
                if let Some(compact) = &mut self.compact {
                    compact.count.character(char::from(byte));
                }
            }
        }
    }

    fn finish_primitive(&mut self) {
        self.in_primitive = false;
        let valid = !self.primitive.overflow
            && serde_json::from_slice::<Value>(&self.primitive.bytes[..self.primitive.len]).is_ok();
        if valid {
            if self.protocol == Protocol::Messages && self.sse && Path(&self.stack).is(&["index"]) {
                match self.primitive.text().parse::<u32>() {
                    Ok(index) => self.event.block_index = Some(index),
                    Err(_) => self.event.incomplete = true,
                }
            }
            if self.compact.is_some()
                && self
                    .primitive
                    .text()
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| *byte == b'-' || byte.is_ascii_digit())
                && self
                    .primitive
                    .text()
                    .bytes()
                    .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
            {
                // 数字的规范化取决于解析器；仅保留已观察表示并注明部分。
                self.event.incomplete = true;
            }
            if self.target() != Target::None && self.primitive.text() != "null" {
                self.event.incomplete = true;
            }
            self.value_done();
        } else {
            self.invalid = true;
        }
    }

    fn finish(&mut self) -> (Event, bool) {
        if self.in_primitive {
            self.finish_primitive();
        }
        if let Some(string) = self.string.take() {
            if let Some(compact) = &mut self.compact {
                compact.count.add(string.compact_count);
            }
            if !string.key {
                self.note_tool_delta(string.count);
                self.event.add(string.target, string.count);
            }
            self.invalid = true;
        }
        if let Some(compact) = self.compact.take() {
            self.event.primary.add(compact.count);
            self.invalid = true;
        }
        let valid = self.root_done && self.stack.is_empty() && !self.invalid;
        (std::mem::take(&mut self.event), valid)
    }

    fn note_tool_delta(&mut self, count: Count) {
        if self.protocol == Protocol::Messages
            && Path(&self.stack).is(&["delta", "partial_json"])
            && count.bytes != 0
        {
            self.event.nonempty_tool_delta = true;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Prefix,
    DataStart,
    Data,
    Ignore,
}

#[derive(Default, Clone)]
struct PlainText {
    count: Count,
    left: u8,
    value: u32,
    minimum: u32,
    invalid: bool,
}

impl PlainText {
    fn feed(&mut self, bytes: &[u8]) {
        self.count.seen = true;
        for byte in bytes {
            if self.invalid {
                break;
            }
            if self.left != 0 {
                if byte & 0xc0 != 0x80 {
                    self.invalid = true;
                    break;
                }
                self.value = (self.value << 6) | u32::from(byte & 0x3f);
                self.left -= 1;
                if self.left == 0 {
                    match char::from_u32(self.value).filter(|_| self.value >= self.minimum) {
                        Some(ch) => self.count.character(ch),
                        None => self.invalid = true,
                    }
                }
            } else {
                match *byte {
                    0..=0x7f => self.count.character(char::from(*byte)),
                    0xc2..=0xdf => {
                        self.left = 1;
                        self.value = u32::from(*byte & 0x1f);
                        self.minimum = 0x80;
                    }
                    0xe0..=0xef => {
                        self.left = 2;
                        self.value = u32::from(*byte & 0x0f);
                        self.minimum = 0x800;
                    }
                    0xf0..=0xf4 => {
                        self.left = 3;
                        self.value = u32::from(*byte & 0x07);
                        self.minimum = 0x10000;
                    }
                    _ => self.invalid = true,
                }
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct RealtimeSlot {
    key: u64,
    count: Count,
}

#[derive(Clone)]
pub(super) struct ResponseObserver {
    first_sse_frame: Option<Instant>,
    first_sse_content: Option<Instant>,
    protocol: Protocol,
    sse: bool,
    json: JsonObserver,
    line_kind: LineKind,
    line_prefix: Label,
    line_nonempty: bool,
    event_data: bool,
    payload_started: bool,
    sentinel: Option<usize>,
    primary: Count,
    deltas: Count,
    dones: Count,
    snapshot: Option<Count>,
    invalid: bool,
    partial: bool,
    terminal: bool,
    plain: Option<PlainText>,
    after_cr: bool,
    tool_placeholders: [u64; 64],
    wire_detected: bool,
    leading_horizontal_whitespace: bool,
    realtime: [RealtimeSlot; 64],
    completed: [u64; 64],
    completed_cursor: usize,
    external_events: bool,
}

impl ResponseObserver {
    pub(super) fn new(protocol: Protocol, sse: bool) -> Self {
        Self {
            first_sse_frame: None,
            first_sse_content: None,
            protocol,
            sse,
            json: JsonObserver::new(protocol, sse),
            line_kind: LineKind::Prefix,
            line_prefix: Label::default(),
            line_nonempty: false,
            event_data: false,
            payload_started: false,
            sentinel: None,
            primary: Count::default(),
            deltas: Count::default(),
            dones: Count::default(),
            snapshot: None,
            invalid: false,
            partial: false,
            terminal: false,
            plain: None,
            after_cr: false,
            tool_placeholders: [u64::MAX; 64],
            wire_detected: false,
            leading_horizontal_whitespace: false,
            realtime: [RealtimeSlot::default(); 64],
            completed: [0; 64],
            completed_cursor: 0,
            external_events: false,
        }
    }

    pub(super) fn plain_text() -> Self {
        let mut observer = Self::new(Protocol::Transcription, false);
        observer.plain = Some(PlainText {
            count: Count {
                seen: true,
                ..Count::default()
            },
            ..PlainText::default()
        });
        observer
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) {
        if let Some(plain) = &mut self.plain {
            plain.feed(bytes);
            return;
        }
        let bytes = if !self.wire_detected {
            let first = bytes
                .iter()
                .position(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'));
            let prefix_len = first.unwrap_or(bytes.len());
            self.leading_horizontal_whitespace |= bytes[..prefix_len]
                .iter()
                .any(|byte| matches!(byte, b' ' | b'\t'));
            let Some(first) = first else {
                return;
            };
            // typed 路径按实际正文选择 JSON/SSE；上游 MIME 可能只是 text/plain。
            self.sse = !matches!(bytes[first], b'{' | b'[');
            self.json.sse = self.sse;
            self.partial |= self.sse && self.leading_horizontal_whitespace;
            self.wire_detected = true;
            &bytes[first..]
        } else {
            bytes
        };
        if !self.sse {
            for byte in bytes {
                self.json.byte(*byte);
            }
            return;
        }
        for byte in bytes {
            if *byte == b'\n' && self.after_cr {
                self.after_cr = false;
                continue;
            }
            self.after_cr = *byte == b'\r';
            if matches!(*byte, b'\r' | b'\n') {
                if !self.line_nonempty {
                    self.end_event();
                }
                self.line_kind = LineKind::Prefix;
                self.line_prefix = Label::default();
                self.line_nonempty = false;
                continue;
            }
            self.line_nonempty = true;
            match self.line_kind {
                LineKind::Prefix => {
                    if *byte == b':' {
                        if self.line_prefix.text() == "data" {
                            if self.event_data {
                                self.payload(b'\n');
                            }
                            self.event_data = true;
                            self.line_kind = LineKind::DataStart;
                        } else {
                            self.line_kind = LineKind::Ignore;
                        }
                    } else {
                        self.line_prefix.character(char::from(*byte));
                        if self.line_prefix.len > 8 {
                            self.line_kind = LineKind::Ignore;
                        }
                    }
                }
                LineKind::DataStart => {
                    self.line_kind = LineKind::Data;
                    if *byte != b' ' {
                        self.payload(*byte);
                    }
                }
                LineKind::Data => self.payload(*byte),
                LineKind::Ignore => {}
            }
        }
    }

    fn payload(&mut self, byte: u8) {
        if !self.payload_started && byte.is_ascii_whitespace() {
            return;
        }
        if !self.payload_started {
            self.payload_started = true;
            if byte == b'[' {
                self.sentinel = Some(0);
            }
        }
        if let Some(index) = self.sentinel {
            if index < b"[DONE]".len() && byte == b"[DONE]"[index] {
                self.sentinel = Some(index + 1);
            } else if !(index == b"[DONE]".len() && byte.is_ascii_whitespace()) {
                self.invalid = true;
            }
        } else {
            self.json.byte(byte);
        }
    }

    fn end_event(&mut self) {
        if !self.event_data {
            return;
        }
        self.first_sse_frame.get_or_insert_with(Instant::now);
        if let Some(index) = self.sentinel {
            self.terminal |= index == b"[DONE]".len();
            self.invalid |= index != b"[DONE]".len();
        } else if self.payload_started {
            let (event, valid) = self.json.finish();
            self.accept(event, valid);
        }
        self.json.reset();
        self.event_data = false;
        self.payload_started = false;
        self.sentinel = None;
    }

    fn accept(&mut self, event: Event, valid: bool) {
        self.invalid |= !valid;
        if self.first_sse_content.is_none() && self.sse && !self.external_events && valid {
            let content = match self.protocol {
                Protocol::Chat | Protocol::Completions => event.primary.bytes != 0,
                Protocol::Messages => {
                    matches!(
                        event.kind.text(),
                        "content_block_start" | "content_block_delta"
                    ) && event.primary.bytes != 0
                }
                Protocol::Responses => match event.kind.text() {
                    "response.output_text.delta"
                    | "response.refusal.delta"
                    | "response.reasoning_text.delta"
                    | "response.reasoning_summary_text.delta"
                    | "response.function_call_arguments.delta"
                    | "response.custom_tool_call_input.delta"
                    | "response.mcp_call_arguments.delta"
                    | "response.code_interpreter_call_code.delta" => event.delta.bytes != 0,
                    "response.output_text.done"
                    | "response.refusal.done"
                    | "response.reasoning_text.done"
                    | "response.reasoning_summary_text.done"
                    | "response.function_call_arguments.done"
                    | "response.custom_tool_call_input.done"
                    | "response.mcp_call_arguments.done"
                    | "response.code_interpreter_call_code.done" => event.done.bytes != 0,
                    "response.content_part.added"
                    | "response.content_part.done"
                    | "response.reasoning_summary_part.added"
                    | "response.reasoning_summary_part.done"
                    | "response.output_item.added"
                    | "response.output_item.done" => event.content,
                    "response.completed" | "response.incomplete" => {
                        event.snapshot.bytes != 0 || event.content
                    }
                    _ => false,
                },
                _ => false,
            };
            if content {
                self.first_sse_content = Some(Instant::now());
            }
        }
        if self.protocol == Protocol::Realtime {
            self.accept_realtime(event);
            return;
        }
        self.partial |= event.incomplete;
        if self.protocol == Protocol::Transcription && self.sse {
            match event.kind.text() {
                "transcript.text.delta" => self.deltas.add(event.delta),
                "transcript.text.done" => {
                    self.snapshot = event.snapshot.seen.then_some(event.snapshot);
                    self.partial |= !event.snapshot.seen;
                    self.terminal = true;
                }
                "error" => self.partial = true,
                _ => self.partial |= event.delta.seen || event.snapshot.seen,
            }
            return;
        }
        if self.protocol != Protocol::Responses {
            if self.protocol == Protocol::Messages && self.sse {
                self.messages_tool_arguments(&event);
            }
            self.primary.add(event.primary);
            if self.protocol == Protocol::Messages && event.kind.text() == "message_stop" {
                self.terminal = true;
            }
            if event.kind.text() == "error" {
                self.partial = true;
            }
            return;
        }
        if !self.sse {
            self.snapshot = event.snapshot.seen.then_some(event.snapshot);
            return;
        }
        let kind = event.kind.text();
        if matches!(
            kind,
            "response.output_text.delta"
                | "response.refusal.delta"
                | "response.reasoning_text.delta"
                | "response.reasoning_summary_text.delta"
                | "response.function_call_arguments.delta"
                | "response.custom_tool_call_input.delta"
        ) {
            self.deltas.add(event.delta);
        }
        if matches!(
            kind,
            "response.output_text.done"
                | "response.refusal.done"
                | "response.reasoning_text.done"
                | "response.reasoning_summary_text.done"
                | "response.function_call_arguments.done"
                | "response.custom_tool_call_input.done"
        ) {
            self.dones.add(event.done);
        }
        if matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.failed"
        ) {
            if event.snapshot.seen {
                self.snapshot = Some(event.snapshot);
            }
            self.terminal = true;
            self.partial |= kind != "response.completed";
        }
    }

    fn accept_realtime(&mut self, event: Event) {
        self.partial |= event.incomplete;
        let kind = event.kind.text();
        let delta = matches!(
            kind,
            "response.text.delta"
                | "response.output_text.delta"
                | "response.audio_transcript.delta"
                | "response.output_audio_transcript.delta"
                | "response.function_call_arguments.delta"
        );
        let done = matches!(
            kind,
            "response.done" | "response.completed" | "response.incomplete" | "response.failed"
        );
        if !delta && !done {
            return;
        }
        let key = event.response_key;
        if key == 0 {
            self.partial = true;
            if delta {
                self.primary.add(event.delta);
            }
            return;
        }
        if self.completed.contains(&key) {
            return;
        }
        let position = self
            .realtime
            .iter()
            .position(|slot| slot.key == key)
            .or_else(|| self.realtime.iter().position(|slot| slot.key == 0));
        let Some(position) = position else {
            self.partial = true;
            return;
        };
        let slot = &mut self.realtime[position];
        slot.key = key;
        if delta {
            slot.count.add(event.delta);
        }
        if done {
            if event.snapshot.seen {
                self.primary.add(event.snapshot);
            } else {
                self.primary.add(slot.count);
                self.partial = true;
            }
            *slot = RealtimeSlot::default();
            self.completed[self.completed_cursor] = key;
            self.completed_cursor = (self.completed_cursor + 1) % self.completed.len();
            self.terminal = true;
            self.partial |= matches!(kind, "response.incomplete" | "response.failed");
        }
    }

    fn messages_tool_arguments(&mut self, event: &Event) {
        let key = event.block_index.map_or(u64::MAX - 1, u64::from);
        match event.kind.text() {
            "content_block_start" if event.empty_tool_input => {
                if event.block_kind.text() != "tool_use" || self.tool_placeholders.contains(&key) {
                    self.partial = true;
                } else if let Some(slot) = self
                    .tool_placeholders
                    .iter_mut()
                    .find(|slot| **slot == u64::MAX)
                {
                    *slot = key;
                } else {
                    // 固定表溢出只降低参考完整性，不影响响应或保存工具正文。
                    self.partial = true;
                }
            }
            "content_block_delta" if event.nonempty_tool_delta => {
                if let Some(slot) = self.tool_placeholders.iter_mut().find(|slot| **slot == key) {
                    *slot = u64::MAX;
                }
            }
            "content_block_stop" => {
                if let Some(slot) = self.tool_placeholders.iter_mut().find(|slot| **slot == key) {
                    *slot = u64::MAX;
                    self.primary.text("{}");
                }
            }
            _ => {}
        }
    }

    pub(super) fn finish(mut self, complete: bool) -> Option<TextMeasurement> {
        if let Some(plain) = self.plain.take() {
            return plain
                .count
                .measurement(complete && !plain.invalid && plain.left == 0);
        }
        if self.sse {
            // SSE 只有空行才提交事件；EOF 尾部可以保留前缀参考，不能声称完整。
            self.partial |= self.event_data;
            self.end_event();
        } else if !self.external_events {
            let (event, valid) = self.json.finish();
            self.accept(event, valid);
        }
        self.partial |= self.tool_placeholders.iter().any(|slot| *slot != u64::MAX);
        if self.protocol == Protocol::Realtime {
            for slot in self.realtime {
                if slot.key != 0 {
                    self.primary.add(slot.count);
                    self.partial = true;
                }
            }
        }
        let mut complete = complete
            && !self.invalid
            && !self.partial
            && (!self.sse
                || self.terminal
                || matches!(self.protocol, Protocol::Gemini | Protocol::Converse));
        let count = if self.protocol == Protocol::Transcription && self.sse {
            self.snapshot.unwrap_or(self.deltas)
        } else if self.protocol == Protocol::Responses {
            if let Some(snapshot) = self.snapshot {
                snapshot
            } else if self.deltas.seen {
                // 没有最终快照，无法证明未遗漏仅有 done 载体的其他槽。
                complete = false;
                self.deltas
            } else {
                complete = false;
                self.dones
            }
        } else {
            self.primary
        };
        count.measurement(complete)
    }
}

#[derive(Clone)]
pub(super) struct RequestObserver {
    json: JsonObserver,
}
impl RequestObserver {
    pub(super) fn new(protocol: Protocol) -> Self {
        let mut json = JsonObserver::new(protocol, false);
        json.request = true;
        Self { json }
    }
    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.json.byte(*byte);
        }
    }
    pub(super) fn finish(mut self, complete: bool) -> Option<TextMeasurement> {
        if self.json.protocol == Protocol::Unknown {
            return None;
        }
        let (event, valid) = self.json.finish();
        event
            .primary
            .measurement(complete && valid && !event.incomplete)
    }
}

impl ResponseObserver {
    pub(super) fn first_sse_frame_at(&self) -> Option<Instant> {
        self.first_sse_frame
    }

    pub(super) fn first_sse_content_at(&self) -> Option<Instant> {
        self.first_sse_content
    }

    pub(super) fn terminal(&self) -> bool {
        self.terminal
    }

    pub(super) fn feed_message(&mut self, bytes: &[u8]) {
        self.external_events = true;
        self.sse = true;
        self.wire_detected = true;
        let mut json = JsonObserver::new(self.protocol, true);
        for byte in bytes {
            json.byte(*byte);
        }
        let (event, valid) = json.finish();
        self.accept(event, valid);
    }

    pub(super) fn feed_json(&mut self, value: &Value) {
        self.external_events = true;
        let event_mode = value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                kind.starts_with("response.")
                    || matches!(kind, "transcript.text.delta" | "transcript.text.done")
                    || matches!(
                        kind,
                        "message_start"
                            | "message_delta"
                            | "message_stop"
                            | "content_block_start"
                            | "content_block_delta"
                            | "content_block_stop"
                            | "error"
                    )
            });
        self.sse |= event_mode;
        self.wire_detected = true;
        let mut json = JsonObserver::new(self.protocol, event_mode);
        struct Sink<'a>(&'a mut JsonObserver);
        impl std::io::Write for Sink<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                for byte in bytes {
                    self.0.byte(*byte);
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        if serde_json::to_writer(Sink(&mut json), value).is_err() {
            self.invalid = true;
        }
        let (event, valid) = json.finish();
        self.accept(event, valid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(protocol: Protocol, wire: &str) -> Option<TextMeasurement> {
        let mut observer = RequestObserver::new(protocol);
        for byte in wire.as_bytes() {
            observer.feed(&[*byte]);
        }
        observer.finish(true)
    }

    fn output(protocol: Protocol, wire: &str) -> Option<TextMeasurement> {
        let mut observer = ResponseObserver::new(protocol, false);
        for byte in wire.as_bytes() {
            observer.feed(&[*byte]);
        }
        observer.finish(true)
    }

    #[test]
    fn transformed_tools_and_schema_are_measured_as_visible_compact_json() {
        let tools = r#"[{"type":"function","function":{"name":"search","description":"A汉😀","parameters":{"type":"object","properties":{"query":{"type":"string"}}}}}]"#;
        let schema =
            r#"{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"string"}}}"#;
        let wire = format!(
            r#"{{"messages":[{{"content":"x"}}],"tools":{tools},"response_format":{schema}}}"#
        );
        let measured = input(Protocol::Chat, &wire).unwrap();
        assert_eq!(
            measured.utf8_bytes,
            1 + tools.len() as u64 + schema.len() as u64
        );
        assert_eq!(
            measured.unicode_scalars,
            1 + tools.chars().count() as u64 + schema.chars().count() as u64
        );
        assert!(measured.complete);
    }

    #[test]
    fn actual_unicode_escapes_and_partial_prefix_are_distinct_from_body_bytes() {
        for wire in [
            r#"{"messages":[{"content":"A汉😀"}]}"#,
            r#"{"messages":[{"content":"A\u6c49\ud83d\ude00"}]}"#,
        ] {
            assert_eq!(
                input(Protocol::Chat, wire),
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: true
                })
            );
        }
        let mut partial = RequestObserver::new(Protocol::Chat);
        partial.feed(br#"{"messages":[{"content":"ab"#);
        assert_eq!(
            partial.finish(false),
            Some(TextMeasurement {
                utf8_bytes: 2,
                unicode_scalars: 2,
                complete: false
            })
        );
    }

    #[test]
    fn media_and_stored_context_keep_observable_prose_partial() {
        let wire = r#"{"input":[{"content":[{"type":"input_text","text":"abc"},{"type":"input_image","image_url":"https://media.example/a"}]}],"previous_response_id":"resp_previous"}"#;
        assert_eq!(
            input(Protocol::Responses, wire),
            Some(TextMeasurement {
                utf8_bytes: 3,
                unicode_scalars: 3,
                complete: false
            })
        );
        assert_eq!(input(Protocol::Embeddings, r#"{"input":[1,2]}"#), None);
        assert_eq!(
            input(Protocol::Embeddings, r#"{"input":""}"#),
            Some(TextMeasurement {
                utf8_bytes: 0,
                unicode_scalars: 0,
                complete: true
            })
        );
    }

    #[test]
    fn native_gemini_and_converse_text_and_tool_json_are_counted() {
        assert_eq!(
            input(
                Protocol::Gemini,
                r#"{"systemInstruction":{"parts":[{"text":"x"}]},"contents":[{"parts":[{"text":"A汉😀"},{"functionCall":{"name":"call","args":{"x":1}}}]}]}"#
            ),
            Some(TextMeasurement {
                utf8_bytes: 16,
                unicode_scalars: 11,
                complete: true
            })
        );
        assert_eq!(
            output(
                Protocol::Gemini,
                r#"{"candidates":[{"content":{"parts":[{"text":"A汉😀","thought":true},{"functionCall":{"name":"call","args":{"x":1}}}]}}]}"#
            ),
            Some(TextMeasurement {
                utf8_bytes: 15,
                unicode_scalars: 10,
                complete: true
            })
        );
        assert_eq!(
            input(
                Protocol::Converse,
                r#"{"system":[{"text":"x"}],"messages":[{"content":[{"text":"A汉😀"},{"toolUse":{"input":{"x":1}}}]}]}"#
            ),
            Some(TextMeasurement {
                utf8_bytes: 16,
                unicode_scalars: 11,
                complete: true
            })
        );
        assert_eq!(
            output(
                Protocol::Converse,
                r#"{"output":{"message":{"content":[{"text":"A汉😀"},{"toolUse":{"input":{"x":1}}}]}}}"#
            ),
            Some(TextMeasurement {
                utf8_bytes: 15,
                unicode_scalars: 10,
                complete: true
            })
        );
    }

    #[test]
    fn native_embedding_slots_and_unknown_protocol_are_not_confused_with_zero() {
        for wire in [
            r#"{"inputText":"A汉😀"}"#,
            r#"{"texts":["A汉😀"]}"#,
            r#"{"instances":[{"content":"A汉😀"}]}"#,
        ] {
            assert_eq!(
                input(Protocol::Embeddings, wire),
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: true
                })
            );
        }
        assert!(input(Protocol::Unknown, r#"{"tools":[{"description":"secret"}]}"#).is_none());
    }

    #[test]
    fn realtime_terminal_snapshots_replace_deltas_per_response_and_keep_distinct_turns() {
        let mut observer = ResponseObserver::new(Protocol::Realtime, false);
        for value in [
            serde_json::json!({"type":"response.output_text.delta","response_id":"r1","delta":"A汉😀"}),
            serde_json::json!({"type":"response.output_text.done","response_id":"r1","text":"A汉😀"}),
            serde_json::json!({"type":"response.done","response":{"id":"r1","output":[{"type":"message","content":[{"type":"output_text","text":"A汉😀"}]}]}}),
            serde_json::json!({"type":"response.done","response":{"id":"r1","output":[{"type":"message","content":[{"type":"output_text","text":"A汉😀"}]}]}}),
            serde_json::json!({"type":"response.function_call_arguments.delta","response_id":"r2","delta":"{}"}),
            serde_json::json!({"type":"response.done","response":{"id":"r2","output":[{"type":"function_call","arguments":"{}"}]}}),
        ] {
            observer.feed_json(&value);
        }
        assert_eq!(
            observer.finish(true),
            Some(TextMeasurement {
                utf8_bytes: 10,
                unicode_scalars: 5,
                complete: true
            })
        );
    }

    #[test]
    fn streaming_reference_is_not_capped_by_content_capture_limits() {
        let mut observer = ResponseObserver::new(Protocol::Chat, true);
        observer.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"");
        for _ in 0..2048 {
            observer.feed(&[b'x'; 1024]);
        }
        observer.feed(b"\"}}]}\n\ndata: [DONE]\n\n");
        assert_eq!(
            observer.finish(true),
            Some(TextMeasurement {
                utf8_bytes: 2 * 1024 * 1024,
                unicode_scalars: 2 * 1024 * 1024,
                complete: true
            })
        );
    }
}
