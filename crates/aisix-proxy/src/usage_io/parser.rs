//! 只保留协议路径、计数和有限解析状态，不保留提示或回答正文。

use aisix_core::usage_io::TextMeasurement;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    Chat,
    Completions,
    Messages,
    Responses,
    Embeddings,
    Rerank,
    Speech,
    Transcription,
    Images,
    Videos,
    Unknown,
}

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

#[derive(Default)]
struct InputCount {
    count: Count,
    partial: bool,
}

impl InputCount {
    fn content(&mut self, value: &Value, depth: usize) {
        if depth > 64 {
            self.partial = true;
            return;
        }
        match value {
            Value::String(text) => self.count.text(text),
            Value::Array(parts) => {
                for part in parts {
                    self.content(part, depth + 1);
                }
            }
            Value::Object(block) => {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" | "input_text" | "output_text" => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            self.count.text(text);
                        } else {
                            self.partial = true;
                        }
                    }
                    "thinking" | "reasoning" => {
                        let text = block.get("thinking").and_then(Value::as_str).or_else(|| {
                            if kind == "reasoning" {
                                block.get("text").and_then(Value::as_str)
                            } else {
                                None
                            }
                        });
                        if let Some(text) = text {
                            self.count.text(text);
                        } else {
                            self.partial = true;
                        }
                    }
                    "refusal" => {
                        if let Some(text) = block.get("refusal").and_then(Value::as_str) {
                            self.count.text(text);
                        } else {
                            self.partial = true;
                        }
                    }
                    "tool_result" => {
                        if let Some(content) = block.get("content") {
                            self.content(content, depth + 1);
                        }
                    }
                    "tool_use" => {
                        if let Some(input) = block.get("input") {
                            self.json_arguments(input);
                        }
                    }
                    "image_url" | "input_image" | "image" | "input_audio" | "audio" | "file"
                    | "input_file" | "video_url" | "video" | "input_video" | "document"
                    | "redacted_thinking" => {}
                    _ => self.partial = true,
                }
            }
            _ => {}
        }
    }

    fn json_arguments(&mut self, value: &Value) {
        // 紧凑 JSON 写入纯计数器，工具参数对象不创建第二份字符串。
        struct Sink<'a>(&'a mut Count);
        impl std::io::Write for Sink<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.seen = true;
                self.0.bytes = self.0.bytes.saturating_add(bytes.len() as u64);
                self.0.chars = self.0.chars.saturating_add(
                    bytes.iter().filter(|byte| (**byte & 0xc0) != 0x80).count() as u64,
                );
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        if serde_json::to_writer(Sink(&mut self.count), value).is_err() {
            self.partial = true;
        }
    }

    fn extras(&mut self, value: &serde_json::Map<String, Value>) {
        for key in ["refusal", "reasoning_content", "reasoning"] {
            if let Some(text) = value.get(key).and_then(Value::as_str) {
                self.count.text(text);
            }
        }
        if let Some(function) = value.get("function_call") {
            self.function(function);
        }
        if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                if let Some(function) = call.get("function") {
                    self.function(function);
                }
            }
        }
    }

    fn function(&mut self, value: &Value) {
        if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
            self.count.text(arguments);
        }
    }

    fn strings(&mut self, value: &Value) {
        match value {
            Value::String(text) => self.count.text(text),
            Value::Array(values) => {
                for value in values {
                    if let Some(text) = value.as_str() {
                        self.count.text(text);
                    } else {
                        self.partial = true;
                    }
                }
            }
            _ => self.partial = true,
        }
    }

    fn finish(self) -> Option<TextMeasurement> {
        self.count.measurement(!self.partial)
    }
}

pub(super) fn measure_chat_input(chat: &aisix_gateway::ChatFormat) -> Option<TextMeasurement> {
    let mut count = InputCount::default();
    for message in &chat.messages {
        if let Some(blocks) = &message.content_blocks {
            for block in blocks {
                count.content(block, 0);
            }
        } else if let Some(text) = &message.content {
            count.count.text(text);
        }
        count.extras(&message.extra);
    }
    count.finish()
}

pub(super) fn measure_input(protocol: Protocol, body: &Value) -> Option<TextMeasurement> {
    let mut count = InputCount::default();
    match protocol {
        Protocol::Chat | Protocol::Messages => {
            if let Some(system) = body.get("system") {
                count.content(system, 0);
            }
            if let Some(messages) = body.get("messages").and_then(Value::as_array) {
                for message in messages {
                    if let Some(content) = message.get("content") {
                        count.content(content, 0);
                    }
                    if let Some(object) = message.as_object() {
                        count.extras(object);
                    }
                }
            }
        }
        Protocol::Responses => {
            if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
                count.count.text(instructions);
            }
            if let Some(input) = body.get("input") {
                if let Some(items) = input.as_array() {
                    for item in items {
                        match item
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("message")
                        {
                            "message" => {
                                if let Some(content) = item.get("content") {
                                    count.content(content, 0);
                                }
                            }
                            "function_call" | "custom_tool_call" => {
                                for key in ["arguments", "input"] {
                                    if let Some(text) = item.get(key).and_then(Value::as_str) {
                                        count.count.text(text);
                                    }
                                }
                            }
                            "function_call_output" | "custom_tool_call_output" => {
                                if let Some(output) = item.get("output") {
                                    count.content(output, 0);
                                }
                            }
                            "reasoning" => {
                                if let Some(summary) = item.get("summary").and_then(Value::as_array)
                                {
                                    for part in summary {
                                        if let Some(text) = part.get("text").and_then(Value::as_str)
                                        {
                                            count.count.text(text);
                                        }
                                    }
                                }
                            }
                            "item_reference" | "compaction" => {}
                            _ => count.partial = true,
                        }
                    }
                } else {
                    count.strings(input);
                }
            }
        }
        Protocol::Completions => {
            if let Some(prompt) = body.get("prompt") {
                count.strings(prompt);
            }
            if let Some(suffix) = body.get("suffix").and_then(Value::as_str) {
                count.count.text(suffix);
            }
        }
        Protocol::Embeddings => {
            if let Some(input) = body.get("input") {
                count.strings(input);
            }
        }
        Protocol::Rerank => {
            if let Some(query) = body.get("query").and_then(Value::as_str) {
                count.count.text(query);
            }
            if let Some(documents) = body.get("documents").and_then(Value::as_array) {
                for document in documents {
                    if let Some(text) = document
                        .as_str()
                        .or_else(|| document.get("text").and_then(Value::as_str))
                    {
                        count.count.text(text);
                    } else {
                        count.partial = true;
                    }
                }
            }
        }
        Protocol::Speech => {
            if let Some(input) = body.get("input").and_then(Value::as_str) {
                count.count.text(input);
            }
            if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
                count.count.text(instructions);
            }
        }
        Protocol::Transcription | Protocol::Images | Protocol::Videos => {
            if let Some(prompt) = body.get("prompt").and_then(Value::as_str) {
                count.count.text(prompt);
            }
        }
        Protocol::Unknown => return None,
    }
    count.finish()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    None,
    Primary,
    Delta,
    Done,
    Snapshot,
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

#[derive(Default)]
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
    terminal: bool,
    incomplete: bool,
}

impl Event {
    fn add(&mut self, target: Target, count: Count) {
        match target {
            Target::Primary => self.primary.add(count),
            Target::Delta => self.delta.add(count),
            Target::Done => self.done.add(count),
            Target::Snapshot => self.snapshot.add(count),
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
        "transcript" => "transcript",
        "error" => "error",
        "item" => "item",
        "index" => "index",
        _ => "",
    }
}

struct Compact {
    depth: usize,
    count: Count,
    object: bool,
}

struct JsonObserver {
    protocol: Protocol,
    sse: bool,
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

    fn target(&self) -> Target {
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
                            && self.stack[3].key == "function_call"))
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
                        || path.is(&["input"]))
                {
                    return Target::Done;
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
            Protocol::Transcription => {
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

    fn container(&mut self, object: bool) {
        let path = Path(&self.stack);
        match self.protocol {
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
            _ => {}
        }
        if path.is(&["error"]) {
            self.event.incomplete = true;
        }
        if self.protocol == Protocol::Messages
            && (path.is(&["content", "#", "input"]) || path.is(&["content_block", "input"]))
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
                        self.event.add(string.target, string.count);
                        let path = Path(&self.stack);
                        if path.is(&["type"]) {
                            self.event.kind = string.label;
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
                        if matches!(self.protocol, Protocol::Chat | Protocol::Completions)
                            && path.is(&["choices", "#", "finish_reason"])
                        {
                            self.event.terminal = true;
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
                let metadata = Path(&self.stack).is(&["type"])
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

#[derive(Default)]
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

pub(super) struct ResponseObserver {
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
}

impl ResponseObserver {
    pub(super) fn new(protocol: Protocol, sse: bool) -> Self {
        Self {
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
        self.partial |= event.incomplete;
        self.terminal |= event.terminal;
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
        } else {
            let (event, valid) = self.json.finish();
            self.accept(event, valid);
        }
        self.partial |= self.tool_placeholders.iter().any(|slot| *slot != u64::MAX);
        let mut complete =
            complete && !self.invalid && !self.partial && (!self.sse || self.terminal);
        let count = if self.protocol == Protocol::Responses {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(
        protocol: Protocol,
        sse: bool,
        wire: &[u8],
        chunk: usize,
    ) -> Option<TextMeasurement> {
        let mut observer = ResponseObserver::new(protocol, sse);
        for part in wire.chunks(chunk) {
            observer.feed(part);
        }
        observer.finish(true)
    }

    fn assert_text(measurement: Option<TextMeasurement>, bytes: u64, chars: u64, complete: bool) {
        let measurement = measurement.expect("文本槽必须实际存在");
        assert_eq!(measurement.utf8_bytes, bytes);
        assert_eq!(measurement.unicode_scalars, chars);
        assert_eq!(measurement.complete, complete);
    }

    #[test]
    fn decoded_unicode_is_independent_from_json_escaping_and_chunk_boundaries() {
        let literal = r#"{"choices":[{"message":{"content":"A汉😀"}}]}"#;
        let escaped = br#"{"choices":[{"message":{"content":"A\u6c49\ud83d\ude00"}}]}"#;
        for chunk in 1..=literal.len() {
            assert_text(
                measured(Protocol::Chat, false, literal.as_bytes(), chunk),
                8,
                3,
                true,
            );
            assert_text(measured(Protocol::Chat, false, escaped, chunk), 8, 3, true);
        }
    }

    #[test]
    fn messages_observes_named_sse_frames_when_mime_hint_says_json() {
        let wire = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"content\":[]}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"A汉😀\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        for chunk in 1..=19 {
            assert_text(
                measured(Protocol::Messages, false, wire.as_bytes(), chunk),
                8,
                3,
                true,
            );
        }
    }

    #[test]
    fn typed_json_body_ignores_wrong_sse_mime_and_keeps_unicode_counts() {
        let wire = " \n\t{\"content\":[{\"type\":\"text\",\"text\":\"A汉😀\"}]}";
        for chunk in 1..=19 {
            assert_text(
                measured(Protocol::Messages, true, wire.as_bytes(), chunk),
                8,
                3,
                true,
            );
        }
    }

    #[test]
    fn empty_text_is_observed_zero_but_errors_and_unsupported_outputs_are_unknown() {
        assert_text(
            measured(
                Protocol::Chat,
                false,
                br#"{"choices":[{"message":{"content":""}}]}"#,
                1,
            ),
            0,
            0,
            true,
        );
        assert!(measured(
            Protocol::Chat,
            false,
            br#"{"error":{"message":"secret"}}"#,
            1
        )
        .is_none());
        assert!(measured(
            Protocol::Embeddings,
            false,
            br#"{"data":[{"embedding":[1,2]}]}"#,
            1
        )
        .is_none());
    }

    #[test]
    fn all_choices_visible_reasoning_refusal_and_tool_arguments_are_counted() {
        let wire = br#"{"choices":[{"message":{"content":"a","reasoning_content":"bc","refusal":"d","tool_calls":[{"function":{"name":"ignored","arguments":"{}"}}]}},{"message":{"content":"ef","function_call":{"arguments":"[]"}}}],"model":"ignored","metadata":{"text":"ignored"}}"#;
        assert_text(measured(Protocol::Chat, false, wire, 1), 10, 10, true);
    }

    #[test]
    fn responses_terminal_snapshot_replaces_delta_done_and_item_carriers() {
        let wire = concat!(
            "event: response.output_text.delta\ndata: {\"delta\":\"A\\u6c49\\ud83d\\ude00\",\"type\":\"response.output_text.delta\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"text\":\"A\\u6c49\\ud83d\\ude00\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"content\":[{\"text\":\"A\\u6c49\\ud83d\\ude00\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"content\":[{\"text\":\"A\\u6c49\\ud83d\\ude00\"}]}]}}\n\n"
        );
        for chunk in 1..32 {
            assert_text(
                measured(Protocol::Responses, true, wire.as_bytes(), chunk),
                8,
                3,
                true,
            );
        }
    }

    #[test]
    fn responses_empty_terminal_snapshot_replaces_prior_deltas() {
        let wire = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"discarded\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert_text(measured(Protocol::Responses, true, wire, 1), 0, 0, true);
    }

    #[test]
    fn responses_done_only_is_honestly_partial_without_terminal_snapshot() {
        let wire = b"data: {\"type\":\"response.output_text.done\",\"text\":\"abc\"}\n\n";
        assert_text(measured(Protocol::Responses, true, wire, 1), 3, 3, false);
    }

    #[test]
    fn canceled_stream_records_only_decoded_prefix_as_partial() {
        let mut observer = ResponseObserver::new(Protocol::Chat, true);
        observer.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"ab");
        assert_text(observer.finish(false), 2, 2, false);
    }

    #[test]
    fn messages_start_delta_and_tool_json_have_no_empty_input_double_count() {
        let wire = concat!(
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"abc\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        assert_text(
            measured(Protocol::Messages, true, wire.as_bytes(), 1),
            5,
            5,
            true,
        );
    }

    #[test]
    fn messages_empty_tool_input_matches_unary_with_no_delta_or_empty_delta() {
        let unary = br#"{"content":[{"type":"text","text":"x"},{"type":"tool_use","id":"t","name":"noargs","input":{}}]}"#;
        assert_text(measured(Protocol::Messages, false, unary, 1), 3, 3, true);
        for delta in [None, Some(""), Some("{}"), Some("{\"a\":1}")] {
            let mut wire = String::from(concat!(
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"x\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"noargs\",\"input\":{}}}\n\n",
            ));
            if let Some(arguments) = delta {
                let event = serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":arguments}});
                wire.push_str(&format!("data: {event}\n\n"));
            }
            wire.push_str(concat!(
                "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ));
            let expected = 1 + delta
                .filter(|arguments| !arguments.is_empty())
                .map_or(2, str::len) as u64;
            for chunk in 1..=17 {
                assert_text(
                    measured(Protocol::Messages, true, wire.as_bytes(), chunk),
                    expected,
                    expected,
                    true,
                );
            }
        }
    }

    #[test]
    fn messages_tool_placeholders_are_per_block_and_replaced_only_once() {
        let wire = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        assert_text(
            measured(Protocol::Messages, true, wire.as_bytes(), 1),
            4,
            4,
            true,
        );
    }

    #[test]
    fn messages_tool_placeholder_overflow_cannot_claim_complete() {
        let mut wire = String::new();
        for index in 0..128 {
            let event = serde_json::json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","input":{}}});
            wire.push_str(&format!("data: {event}\n\n"));
        }
        for index in 0..128 {
            let event = serde_json::json!({"type":"content_block_stop","index":index});
            wire.push_str(&format!("data: {event}\n\n"));
        }
        wire.push_str("data: {\"type\":\"message_stop\"}\n\n");
        let measurement = measured(Protocol::Messages, true, wire.as_bytes(), 1)
            .expect("bounded known prefix remains observed");
        assert!(
            !measurement.complete,
            "untracked blocks cannot become complete zero"
        );
        assert!(measurement.utf8_bytes > 0 && measurement.utf8_bytes < 256);
    }

    #[test]
    fn capture_caps_do_not_cap_scalar_counting() {
        let mut observer = ResponseObserver::new(Protocol::Chat, true);
        observer.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"");
        for _ in 0..2048 {
            observer.feed(&[b'x'; 1024]);
        }
        observer.feed(b"\"}}]}\n\ndata: [DONE]\n\n");
        assert_text(
            observer.finish(true),
            2 * 1024 * 1024,
            2 * 1024 * 1024,
            true,
        );
    }

    #[test]
    fn borrowed_input_excludes_tools_schema_and_media_but_keeps_tool_results() {
        let value = serde_json::json!({
            "messages":[
                {"role":"system","content":"A汉😀"},
                {"role":"tool","content":[{"type":"text","text":"x"},{"type":"image_url","image_url":{"url":"data:ignored"}}]},
                {"role":"assistant","tool_calls":[{"function":{"name":"ignored","arguments":"{}"}}]}
            ],
            "tools":[{"function":{"description":"ignored","parameters":{"text":"ignored"}}}],
            "response_format":{"json_schema":{"schema":{"text":"ignored"}}}
        });
        assert_text(measure_input(Protocol::Chat, &value), 11, 6, true);
        assert!(measure_input(Protocol::Unknown, &value).is_none());
    }

    #[test]
    fn messages_tool_objects_use_compact_decoded_json_without_original_retention() {
        let wire = r#"{"content":[{"type":"tool_use","input": { "x" : "A\u6c49\ud83d\ude00", "y": [true, 12] }}]}"#;
        let expected = "{\"x\":\"A汉😀\",\"y\":[true,12]}";
        assert_text(
            measured(Protocol::Messages, false, wire.as_bytes(), 1),
            expected.len() as u64,
            expected.chars().count() as u64,
            true,
        );
    }

    #[test]
    fn plain_transcripts_validate_utf8_across_chunks_and_keep_empty_zero() {
        let mut observer = ResponseObserver::plain_text();
        for byte in "A汉😀".as_bytes() {
            observer.feed(&[*byte]);
        }
        assert_text(observer.finish(true), 8, 3, true);
        assert_text(ResponseObserver::plain_text().finish(true), 0, 0, true);
        let mut observer = ResponseObserver::plain_text();
        observer.feed(&[b'a', 0xe6, 0xb1]);
        assert_text(observer.finish(true), 1, 1, false);
    }

    #[test]
    fn sse_crlf_and_cr_line_endings_work_but_raw_string_cr_is_partial() {
        let good =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"abc\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
        assert_text(measured(Protocol::Chat, true, good, 1), 3, 3, true);
        let bad = b"data: {\"choices\":[{\"delta\":{\"content\":\"ab\rc\"}}]}\n\ndata: [DONE]\n\n";
        assert_text(measured(Protocol::Chat, true, bad, 1), 2, 2, false);
    }
    #[test]
    fn unknown_input_block_makes_known_prose_partial_without_mining_unknown_text() {
        let input = serde_json::json!({"messages":[{"content":[
            {"type":"text","text":"abc"},
            {"type":"future_text","text":"uninterpreted"}
        ]}]});
        assert_text(measure_input(Protocol::Chat, &input), 3, 3, false);
    }

    #[test]
    fn excluded_media_and_hidden_context_do_not_make_known_prose_partial() {
        let mut content = vec![serde_json::json!({"type":"text","text":"abc"})];
        for kind in [
            "image_url",
            "input_image",
            "image",
            "input_audio",
            "audio",
            "file",
            "input_file",
            "video_url",
            "video",
            "input_video",
            "document",
            "redacted_thinking",
        ] {
            content.push(serde_json::json!({"type":kind,"text":"excluded"}));
        }
        let input = serde_json::json!({"messages":[{"content":content}]});
        assert_text(measure_input(Protocol::Chat, &input), 3, 3, true);
    }

    #[test]
    fn reasoning_input_text_fallback_counts_decoded_unicode_once() {
        let input = serde_json::json!({"messages":[{"content":[
            {"type":"reasoning","text":"A汉😀"}
        ]}]});
        assert_text(measure_input(Protocol::Chat, &input), 8, 3, true);
    }

    #[test]
    fn recognized_reasoning_without_prose_keeps_known_text_partial() {
        let input = serde_json::json!({"messages":[{"content":[
            {"type":"text","text":"abc"},
            {"type":"reasoning","signature":"opaque"}
        ]}]});
        assert_text(measure_input(Protocol::Chat, &input), 3, 3, false);
    }

    #[test]
    fn responses_unhandled_input_tool_text_keeps_observed_prose_partial() {
        let input = serde_json::json!({"input":[
            {"role":"user","content":"x"},
            {"type":"shell_call_output","call_id":"c","output":[
                {"stdout":"A汉😀","stderr":"","outcome":{"type":"exit","exit_code":0}}
            ]}
        ]});
        assert_text(measure_input(Protocol::Responses, &input), 1, 1, false);
    }

    #[test]
    fn responses_unhandled_input_item_is_unknown_without_a_selected_text_slot() {
        let input = serde_json::json!({"input":[{"type":"future_tool_result","text":"A汉😀"}]});
        assert!(measure_input(Protocol::Responses, &input).is_none());
    }

    #[test]
    fn responses_unhandled_visible_code_never_becomes_complete_zero() {
        let wire = br#"{"output":[{"type":"code_interpreter_call","code":"print(1)"}]}"#;
        assert_text(measured(Protocol::Responses, false, wire, 1), 0, 0, false);
        let wire = b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"future_text_tool\",\"code\":\"print(1)\"}]}}\n\n";
        assert_text(measured(Protocol::Responses, true, wire, 1), 0, 0, false);
    }

    #[test]
    fn undelimited_sse_tail_cannot_claim_complete_even_with_a_terminal_marker() {
        let chat =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n";
        assert_text(measured(Protocol::Chat, true, chat, 1), 1, 1, false);
        let responses = b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"content\":[{\"text\":\"x\"}]}]}}";
        assert_text(
            measured(Protocol::Responses, true, responses, 1),
            1,
            1,
            false,
        );
    }

    #[test]
    fn token_id_inputs_are_unknown_and_known_empty_text_is_zero() {
        assert!(measure_input(Protocol::Embeddings, &serde_json::json!({"input":[1,2]})).is_none());
        assert_text(
            measure_input(Protocol::Embeddings, &serde_json::json!({"input":""})),
            0,
            0,
            true,
        );
        assert_text(
            measure_input(
                Protocol::Responses,
                &serde_json::json!({"input":"A汉😀","instructions":"x"}),
            ),
            9,
            4,
            true,
        );
    }
}
