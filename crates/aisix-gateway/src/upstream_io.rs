//! 上游真实交换的长度观察；不保存请求或响应正文。

use aisix_core::usage_io::{TextMeasurement, UsageIoAudit};
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use reqwest::ResponseBuilderExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Instant;

mod parser;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
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
    Gemini,
    Converse,
    Realtime,
    Unknown,
}

static NEXT_EXCHANGE_ID: AtomicU64 = AtomicU64::new(1);

tokio::task_local! {
    static RESPONSE_BODY_LIMIT: ResponseBodyLimit;
}

/// 仅显式能力对照安装；上限是交付给解码器的body字节，不是socket物理读取量。
#[derive(Clone, Debug)]
pub struct ResponseBodyLimit {
    maximum: u64,
    exceeded: Arc<AtomicBool>,
    observed_bytes: Arc<AtomicU64>,
}

impl ResponseBodyLimit {
    pub fn new(maximum: u64) -> Self {
        Self {
            maximum,
            exceeded: Arc::new(AtomicBool::new(false)),
            observed_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn exceeded(&self) -> bool {
        self.exceeded.load(Ordering::Relaxed)
    }

    /// 每个body已实际poll到的最大字节数；超限的完整chunk也计算，不截小计量。
    pub fn observed_bytes(&self) -> u64 {
        self.observed_bytes.load(Ordering::Relaxed)
    }

    fn observe(&self, received: u64) -> bool {
        self.observed_bytes.fetch_max(received, Ordering::Relaxed);
        let exceeded = received > self.maximum;
        if exceeded {
            self.exceeded.store(true, Ordering::Relaxed);
        }
        exceeded
    }
}

pub fn response_body_limit() -> Option<ResponseBodyLimit> {
    RESPONSE_BODY_LIMIT.try_with(Clone::clone).ok()
}

pub async fn scope_response_body_limit<F: std::future::Future>(
    limit: Option<ResponseBodyLimit>,
    future: F,
) -> F::Output {
    match limit {
        Some(limit) => RESPONSE_BODY_LIMIT.scope(limit, future).await,
        None => future.await,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("assessment response body exceeds the decoder byte limit")]
struct ResponseBodyLimitExceeded;

type BodyError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Default)]
struct HttpTiming {
    started: Option<Instant>,
    samples: aisix_core::usage_io::UpstreamTiming,
    ambiguous: bool,
}

impl HttpTiming {
    fn offset_at(&self, at: Instant) -> Option<u32> {
        self.started.map(|started| {
            at.saturating_duration_since(started)
                .as_millis()
                .min(u128::from(u32::MAX)) as u32
        })
    }

    fn now(&self) -> Option<u32> {
        self.started?;
        self.offset_at(Instant::now())
    }

    fn snapshot(&self) -> Option<aisix_core::usage_io::UpstreamTiming> {
        self.samples.http_send_started_ms?;
        (!self.ambiguous).then_some(self.samples)
    }
}

struct Exchange {
    timing: HttpTiming,
    id: u64,
    parents: Vec<Weak<Mutex<Collection>>>,
    closed: bool,
    request_finished: bool,
    response_finished: bool,
    input_precomputed: bool,
    protocol: Protocol,
    transport: &'static str,
    request_bytes: Option<u64>,
    response_bytes: Option<u64>,
    request_complete: Option<bool>,
    response_complete: bool,
    request_failed: bool,
    response_failed: bool,
    input: Option<parser::RequestObserver>,
    output: Option<parser::ResponseObserver>,
    input_override: Option<TextMeasurement>,
    output_override: Option<TextMeasurement>,
    input_messages: Option<TextMeasurement>,
    input_partial: bool,
    output_partial: bool,
    output_adapter_complete: Option<bool>,
    output_terminal: bool,
}

/// 直接复用已有交换的 Arc 和锁；后续标记不再查找外层集合。
#[derive(Clone)]
pub struct HttpTimingObserver(Arc<Mutex<Exchange>>);

impl std::fmt::Debug for HttpTimingObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTimingObserver").finish_non_exhaustive()
    }
}

impl PartialEq for HttpTimingObserver {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for HttpTimingObserver {}

impl HttpTimingObserver {
    fn with_timing<T>(&self, observe: impl FnOnce(&mut HttpTiming) -> T) -> T {
        let mut exchange = self.0.lock().unwrap_or_else(|p| p.into_inner());
        observe(&mut exchange.timing)
    }

    pub fn snapshot(&self) -> Option<aisix_core::usage_io::UpstreamTiming> {
        self.with_timing(|timing| timing.snapshot())
    }

    pub fn note_canonical_chunk(&self) {
        self.with_timing(|timing| {
            if timing.samples.first_canonical_chunk_ms.is_none() {
                timing.samples.first_canonical_chunk_ms = timing.now();
            }
        });
    }

    pub fn note_first_content(&self) {
        self.with_timing(|timing| {
            if timing.samples.first_content_ms.is_none() {
                timing.samples.first_content_ms = timing.now();
            }
        });
    }

    pub fn note_first_content_at(&self, at: Instant) {
        self.with_timing(|timing| {
            if timing.samples.first_content_ms.is_none() {
                timing.samples.first_content_ms = timing.offset_at(at);
            }
        });
    }

    pub(crate) fn bind_attempt(&self, started: Option<Instant>) {
        self.with_timing(|timing| timing.started = started);
    }

    pub(crate) fn invalidate(&self) {
        self.with_timing(|timing| timing.ambiguous = true);
    }
}

impl Exchange {
    fn new(protocol: Protocol) -> Self {
        Self {
            timing: HttpTiming::default(),
            id: NEXT_EXCHANGE_ID.fetch_add(1, Ordering::Relaxed),
            parents: Vec::new(),
            closed: false,
            request_finished: false,
            response_finished: false,
            input_precomputed: false,
            protocol,
            transport: "http_entity",
            request_bytes: None,
            response_bytes: None,
            request_complete: None,
            response_complete: false,
            request_failed: false,
            response_failed: false,
            input: Some(parser::RequestObserver::new(protocol)),
            output: Some(parser::ResponseObserver::new(protocol, false)),
            input_override: None,
            output_override: None,
            input_messages: None,
            input_partial: false,
            output_partial: false,
            output_adapter_complete: None,
            output_terminal: false,
        }
    }

    fn snapshot(&self) -> UsageIoAudit {
        let input_complete = self.request_complete == Some(true) && !self.input_partial;
        let input_override = if self.input_precomputed
            && self.request_bytes.is_some()
            && self.request_complete != Some(true)
        {
            None
        } else {
            self.input_override
        };
        let mut input = input_override.or(self.input_messages).or_else(|| {
            self.input
                .clone()
                .and_then(|input| input.finish(input_complete))
        });
        if let Some(text) = &mut input {
            text.complete &= input_complete;
        }
        let output_complete = self.output_adapter_complete.unwrap_or(
            self.response_complete
                || self.output_terminal
                || self
                    .output
                    .as_ref()
                    .is_some_and(parser::ResponseObserver::terminal),
        ) && !self.output_partial;
        let mut output = self.output_override.or_else(|| {
            self.output
                .clone()
                .and_then(|output| output.finish(output_complete))
        });
        if let Some(text) = &mut output {
            text.complete &= output_complete;
        }
        UsageIoAudit {
            version: 2,
            boundary: "upstream_attempt".to_owned(),
            transport: Some(self.transport.to_owned()),
            request_body_bytes: self.request_bytes,
            response_body_bytes: self.response_bytes,
            request_complete: self.request_complete,
            response_complete: self.response_complete,
            exchange_count: Some(1),
            input_text: input,
            output_text: output,
        }
    }

    fn finished(&self) -> bool {
        self.request_finished
            && self.response_finished
            && (self.output_adapter_complete != Some(false) || self.response_failed)
    }

    fn abandon(&mut self) {
        if !self.request_finished {
            self.request_failed = true;
            self.request_complete = Some(false);
            if let Some(input) = self.input.take() {
                let prefix = input.finish(false);
                if self.input_precomputed || self.input_override.is_none() {
                    self.input_override = prefix;
                    self.input_precomputed = false;
                }
            }
        }
        if !self.response_finished || self.output_adapter_complete == Some(false) {
            if !self.response_finished {
                self.response_complete = false;
            }
            self.response_failed = true;
            if let Some(output) = self.output.take() {
                self.output_terminal |= output.terminal();
                if self.output_override.is_none() {
                    self.output_override = output.finish(self.output_terminal);
                }
            }
        }
        self.input = None;
        self.output = None;
        self.closed = true;
    }
}

/// 聚合只保存已关闭交换的标量摘要，以及仍在进行的交换句柄。
#[derive(Default)]
struct Collection {
    local: Option<Arc<Mutex<Exchange>>>,
    folded: Option<UsageIoAudit>,
    live: BTreeMap<u64, Arc<Mutex<Exchange>>>,
}

impl Drop for Collection {
    fn drop(&mut self) {
        if let Some(exchange) = &self.local {
            let notification = {
                let mut exchange = exchange.lock().unwrap_or_else(|p| p.into_inner());
                if exchange.closed {
                    None
                } else {
                    exchange.abandon();
                    Some((exchange.id, exchange.snapshot(), exchange.parents.clone()))
                }
            };
            if let Some((id, audit, parents)) = notification {
                fold_closed(id, audit, parents);
            }
        }
    }
}

fn fold_closed(id: u64, audit: UsageIoAudit, parents: Vec<Weak<Mutex<Collection>>>) {
    for parent in parents {
        if let Some(parent) = parent.upgrade() {
            let mut parent = parent.lock().unwrap_or_else(|p| p.into_inner());
            if parent.live.remove(&id).is_some() {
                merge_audit(&mut parent.folded, audit.clone());
            }
        }
    }
}

/// 每次真实派发持有自己的句柄；take 后原句柄继续写入原 attempt。
#[derive(Clone)]
pub struct UpstreamIoObserver(Arc<Mutex<Collection>>);

/// 出站消息在 Sink 接受前只暂存标量，绝不克隆或保留 payload。
pub struct PreparedRequestMessage {
    bytes: u64,
    text: Option<TextMeasurement>,
}

impl std::fmt::Debug for UpstreamIoObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamIoObserver")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl PartialEq for UpstreamIoObserver {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for UpstreamIoObserver {}

impl UpstreamIoObserver {
    /// 时序使用已有 local 的持久句柄；不增加 heap 分配，也不受长度折叠/关闭影响。
    pub(crate) fn http_timing_observer(&self) -> Option<HttpTimingObserver> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .local
            .clone()
            .map(HttpTimingObserver)
    }

    pub fn new(protocol: Protocol) -> Self {
        Self(Arc::new(Mutex::new(Collection {
            local: Some(Arc::new(Mutex::new(Exchange::new(protocol)))),
            folded: None,
            live: BTreeMap::new(),
        })))
    }

    pub fn observed(protocol: Protocol) -> Self {
        let observer = Self::new(protocol);
        crate::response_tap::note_upstream_io(&observer);
        observer
    }

    /// WS 消息记录真实 payload；内部合成的 HTTP/SSE 不经过这里。
    pub fn websocket(protocol: Protocol) -> Self {
        let observer = Self::new(protocol);
        observer.update(|exchange| {
            exchange.transport = "websocket_payload";
            exchange.request_bytes = Some(0);
            exchange.response_bytes = Some(0);
            exchange.request_complete = Some(false);
        });
        crate::response_tap::note_upstream_io(&observer);
        observer
    }

    fn members(&self) -> Vec<Arc<Mutex<Exchange>>> {
        let current = self.0.lock().unwrap_or_else(|p| p.into_inner());
        current
            .local
            .iter()
            .chain(current.live.values())
            .cloned()
            .collect()
    }

    fn update(&self, update: impl FnOnce(&mut Exchange)) {
        let local = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .local
            .clone();
        if let Some(local) = local {
            let notification = {
                let mut exchange = local.lock().unwrap_or_else(|p| p.into_inner());
                if exchange.closed {
                    return;
                }
                update(&mut exchange);
                if exchange.finished() {
                    exchange.input = None;
                    exchange.output = None;
                    exchange.closed = true;
                    Some((exchange.id, exchange.snapshot(), exchange.parents.clone()))
                } else {
                    None
                }
            };
            if let Some((id, audit, parents)) = notification {
                fold_closed(id, audit, parents);
            }
        }
    }

    pub(crate) fn aggregate(observer: &Self) -> Self {
        let aggregate = Self(Arc::new(Mutex::new(Collection::default())));
        aggregate.append(observer);
        aggregate
    }

    pub(crate) fn append(&self, observer: &Self) {
        let parent = Arc::downgrade(&self.0);
        for incoming in observer.members() {
            let mut exchange = incoming.lock().unwrap_or_else(|p| p.into_inner());
            if exchange
                .parents
                .iter()
                .any(|previous| previous.ptr_eq(&parent))
            {
                continue;
            }
            exchange.parents.push(parent.clone());
            let mut aggregate = self.0.lock().unwrap_or_else(|p| p.into_inner());
            if exchange.closed {
                merge_audit(&mut aggregate.folded, exchange.snapshot());
            } else {
                aggregate.live.insert(exchange.id, incoming.clone());
            }
        }
    }

    pub fn snapshot(&self) -> UsageIoAudit {
        let (mut total, live) = {
            let current = self.0.lock().unwrap_or_else(|p| p.into_inner());
            (
                current.folded.clone(),
                current
                    .local
                    .iter()
                    .chain(current.live.values())
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        for exchange in live {
            merge_audit(
                &mut total,
                exchange
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .snapshot(),
            );
        }
        total.unwrap_or_else(|| Exchange::new(Protocol::Unknown).snapshot())
    }

    fn begin_request_body(&self) {
        self.update(|exchange| {
            exchange.request_bytes.get_or_insert(0);
            exchange.request_complete = Some(false);
            exchange.request_finished = false;
        });
    }

    fn begin_response_body(&self) {
        self.update(|exchange| {
            exchange.response_bytes.get_or_insert(0);
        });
    }

    pub fn observe_request_body(&self, bytes: &[u8]) {
        self.update(|exchange| {
            *exchange.request_bytes.get_or_insert(0) = exchange
                .request_bytes
                .unwrap_or(0)
                .saturating_add(bytes.len() as u64);
            exchange.request_complete = Some(false);
            exchange.request_finished = false;
            if let Some(input) = &mut exchange.input {
                input.feed(bytes);
            }
        });
    }

    pub fn finish_request(&self) {
        self.update(|exchange| {
            exchange.request_complete = Some(!exchange.request_failed);
            exchange.request_finished = true;
            if exchange.transport == "http_entity" {
                if let Some(input) = exchange.input.take() {
                    if exchange.input_override.is_none() {
                        exchange.input_override = input.finish(!exchange.request_failed);
                    }
                }
            }
        });
    }

    pub fn fail_request(&self) {
        self.update(|exchange| {
            exchange.request_failed = true;
            exchange.request_finished = true;
            exchange.request_complete = Some(false);
            if let Some(input) = exchange.input.take() {
                let prefix = input.finish(false);
                if exchange.input_precomputed || exchange.input_override.is_none() {
                    exchange.input_override = prefix;
                    exchange.input_precomputed = false;
                }
            }
        });
    }

    /// SDK 已序列化模型值的文本观察；序列化传输字节保持未知。
    pub fn observe_request_json(&self, value: &Value) {
        self.update(|exchange| {
            let mut input = parser::RequestObserver::new(exchange.protocol);
            write_json(value, |bytes| input.feed(bytes));
            exchange.input_override = input.finish(true);
            exchange.input_precomputed = true;
            exchange.request_finished = true;
            exchange.request_complete = Some(!exchange.request_failed);
        });
    }

    /// 已由最终模型 JSON 计算出的纯标量；用于 SDK 的 base64 信封。
    pub fn observe_input_measurement(&self, measurement: TextMeasurement) {
        self.update(|exchange| {
            exchange.input_override = Some(measurement);
            exchange.input_precomputed = true;
            exchange.request_finished = true;
            exchange.request_complete = Some(!exchange.request_failed);
        });
    }

    pub fn observe_input_texts<'a>(
        &self,
        texts: impl IntoIterator<Item = &'a str>,
        complete: bool,
    ) {
        let mut total = None;
        for text in texts {
            add_text(&mut total, text);
        }
        self.update(|exchange| {
            exchange.input_override = total;
            exchange.input_precomputed = true;
            exchange.request_finished = true;
            exchange.input_partial |= !complete;
            exchange.request_complete = Some(!exchange.request_failed);
        });
    }

    pub fn begin_response(&self, content_type: Option<&str>) {
        self.update(|exchange| {
            if exchange.timing.samples.http_send_started_ms.is_some()
                && exchange.timing.samples.response_headers_ms.is_none()
            {
                exchange.timing.samples.response_headers_ms = exchange.timing.now();
            }
            let mime = content_type
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            if mime == "application/vnd.amazon.eventstream"
                || mime == "application/octet-stream"
                || mime.starts_with("audio/")
                || mime.starts_with("image/")
                || mime.starts_with("video/")
            {
                exchange.output = None;
                exchange.output_adapter_complete = Some(false);
            } else if exchange.protocol == Protocol::Transcription && mime == "text/plain" {
                exchange.output = Some(parser::ResponseObserver::plain_text());
            }
        });
    }

    pub fn observe_response_body(&self, bytes: &[u8]) {
        self.update(|exchange| {
            if !bytes.is_empty()
                && exchange.timing.samples.http_send_started_ms.is_some()
                && exchange.timing.samples.first_response_byte_ms.is_none()
            {
                exchange.timing.samples.first_response_byte_ms = exchange.timing.now();
            }
            *exchange.response_bytes.get_or_insert(0) = exchange
                .response_bytes
                .unwrap_or(0)
                .saturating_add(bytes.len() as u64);
            if let Some(output) = &mut exchange.output {
                output.feed(bytes);
                if exchange.timing.samples.http_send_started_ms.is_some()
                    && exchange.timing.samples.first_protocol_frame_ms.is_none()
                {
                    exchange.timing.samples.first_protocol_frame_ms = output
                        .first_sse_frame_at()
                        .and_then(|at| exchange.timing.offset_at(at));
                }
                if exchange.timing.samples.http_send_started_ms.is_some()
                    && exchange.timing.samples.upstream_first_content_ms.is_none()
                {
                    exchange.timing.samples.upstream_first_content_ms = output
                        .first_sse_content_at()
                        .and_then(|at| exchange.timing.offset_at(at));
                }
            }
        });
    }

    fn observe_rejected_response_bytes(&self, bytes: usize) {
        self.update(|exchange| {
            if bytes > 0
                && exchange.timing.samples.http_send_started_ms.is_some()
                && exchange.timing.samples.first_response_byte_ms.is_none()
            {
                exchange.timing.samples.first_response_byte_ms = exchange.timing.now();
            }
            *exchange.response_bytes.get_or_insert(0) = exchange
                .response_bytes
                .unwrap_or(0)
                .saturating_add(bytes as u64);
            exchange.output_partial = true;
        });
    }

    pub fn observe_response_json(&self, value: &Value) {
        self.update(|exchange| {
            exchange
                .output
                .get_or_insert_with(|| parser::ResponseObserver::new(exchange.protocol, false))
                .feed_json(value);
        });
    }

    /// SDK 解开二进制事件信封后，只向协议解析器交付事件，不重复计算 body。
    pub fn observe_response_event(&self, value: &Value) {
        self.observe_response_json(value);
    }

    /// 原始 SDK 事件已由外层二进制 body 计数，此处只解析模型事件。
    pub fn observe_response_event_bytes(&self, bytes: &[u8]) -> bool {
        let mut terminal = false;
        self.update(|exchange| {
            let output = exchange
                .output
                .get_or_insert_with(|| parser::ResponseObserver::new(exchange.protocol, false));
            output.feed_message(bytes);
            terminal = output.terminal();
        });
        if terminal {
            self.finish_output_text();
        }
        terminal
    }

    /// 借用的工具 JSON 直接写入标量计数器，不构建中间 Value 或字符串。
    pub fn observe_output_serialized<T: serde::Serialize + ?Sized>(
        &self,
        value: &T,
    ) -> Result<(), serde_json::Error> {
        struct Sink {
            bytes: u64,
            scalars: u64,
        }
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes = self.bytes.saturating_add(bytes.len() as u64);
                self.scalars = self.scalars.saturating_add(
                    bytes.iter().filter(|byte| (**byte & 0xc0) != 0x80).count() as u64,
                );
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Sink {
            bytes: 0,
            scalars: 0,
        };
        let result = serde_json::to_writer(&mut sink, value);
        self.update(|exchange| {
            let measured = TextMeasurement {
                utf8_bytes: sink.bytes,
                unicode_scalars: sink.scalars,
                complete: result.is_ok(),
            };
            exchange.output_override = Some(match exchange.output_override {
                Some(previous) => TextMeasurement {
                    utf8_bytes: previous.utf8_bytes.saturating_add(measured.utf8_bytes),
                    unicode_scalars: previous
                        .unicode_scalars
                        .saturating_add(measured.unicode_scalars),
                    complete: previous.complete && measured.complete,
                },
                None => measured,
            });
            exchange.output_partial |= result.is_err();
        });
        result
    }

    pub fn observe_output_text(&self, text: &str) {
        self.update(|exchange| {
            add_text(&mut exchange.output_override, text);
        });
    }

    pub fn finish_output_text(&self) {
        self.update(|exchange| {
            exchange.output_adapter_complete = Some(true);
            if let Some(output) = exchange.output.take() {
                exchange.output_terminal |= output.terminal();
                if exchange.output_override.is_none() {
                    exchange.output_override = output.finish(true);
                }
            }
        });
    }

    pub fn mark_input_text_partial(&self) {
        self.update(|exchange| exchange.input_partial = true);
    }
    pub fn mark_output_text_partial(&self) {
        self.update(|exchange| exchange.output_partial = true);
    }

    pub fn prepare_request_message(&self, bytes: &[u8]) -> PreparedRequestMessage {
        let mut text = None;
        self.update(|exchange| {
            let mut input = parser::RequestObserver::new(exchange.protocol);
            input.feed(bytes);
            text = input.finish(true);
        });
        PreparedRequestMessage {
            bytes: bytes.len() as u64,
            text,
        }
    }

    pub fn observe_prepared_request_message(&self, message: PreparedRequestMessage) {
        self.update(|exchange| {
            *exchange.request_bytes.get_or_insert(0) = exchange
                .request_bytes
                .unwrap_or(0)
                .saturating_add(message.bytes);
            exchange.input_messages = if exchange.input_messages.is_none() {
                message.text
            } else {
                sum_text(exchange.input_messages, message.text)
            };
            exchange.request_complete = Some(false);
            exchange.request_finished = false;
        });
    }

    pub fn observe_request_message(&self, bytes: &[u8]) {
        self.observe_prepared_request_message(self.prepare_request_message(bytes));
    }

    pub fn observe_response_message(&self, bytes: &[u8]) {
        self.update(|exchange| {
            *exchange.response_bytes.get_or_insert(0) = exchange
                .response_bytes
                .unwrap_or(0)
                .saturating_add(bytes.len() as u64);
            // 一条 WS 消息是完整 JSON；解析器流式读取，绝不缓存正文。
            exchange
                .output
                .get_or_insert_with(|| parser::ResponseObserver::new(exchange.protocol, false))
                .feed_message(bytes);
        });
    }

    pub fn finish_response(&self) {
        self.update(|exchange| {
            exchange.response_complete = !exchange.response_failed;
            exchange.response_finished = true;
            if exchange.output_adapter_complete != Some(false) {
                if let Some(output) = exchange.output.take() {
                    exchange.output_terminal |= output.terminal();
                    if exchange.output_override.is_none() {
                        exchange.output_override =
                            output.finish(exchange.response_complete || exchange.output_terminal);
                    }
                }
            }
        });
    }

    pub fn fail_response(&self) {
        self.update(|exchange| {
            exchange.response_failed = true;
            exchange.response_finished = true;
            exchange.response_complete = false;
            if let Some(output) = exchange.output.take() {
                exchange.output_terminal |= output.terminal();
                if exchange.output_override.is_none() {
                    exchange.output_override = output.finish(exchange.output_terminal);
                }
            }
        });
    }
}

fn merge_audit(total: &mut Option<UsageIoAudit>, next: UsageIoAudit) {
    let Some(total) = total else {
        *total = Some(next);
        return;
    };
    total.request_body_bytes = sum_known(total.request_body_bytes, next.request_body_bytes);
    total.response_body_bytes = sum_known(total.response_body_bytes, next.response_body_bytes);
    total.request_complete = match (total.request_complete, next.request_complete) {
        (Some(a), Some(b)) => Some(a && b),
        _ => None,
    };
    total.response_complete &= next.response_complete;
    total.exchange_count = Some(
        total
            .exchange_count
            .unwrap_or(0)
            .saturating_add(next.exchange_count.unwrap_or(0)),
    );
    total.input_text = sum_text(total.input_text, next.input_text);
    total.output_text = sum_text(total.output_text, next.output_text);
    if total.transport != next.transport {
        total.transport = Some("mixed".to_owned());
    }
}

fn sum_known(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    a.zip(b).map(|(a, b)| a.saturating_add(b))
}

fn sum_text(a: Option<TextMeasurement>, b: Option<TextMeasurement>) -> Option<TextMeasurement> {
    match (a, b) {
        (Some(a), Some(b)) => Some(TextMeasurement {
            utf8_bytes: a.utf8_bytes.saturating_add(b.utf8_bytes),
            unicode_scalars: a.unicode_scalars.saturating_add(b.unicode_scalars),
            complete: a.complete && b.complete,
        }),
        (Some(mut measured), None) | (None, Some(mut measured)) => {
            measured.complete = false;
            Some(measured)
        }
        (None, None) => None,
    }
}

fn add_text(total: &mut Option<TextMeasurement>, text: &str) {
    let total = total.get_or_insert(TextMeasurement {
        utf8_bytes: 0,
        unicode_scalars: 0,
        complete: true,
    });
    total.utf8_bytes = total.utf8_bytes.saturating_add(text.len() as u64);
    total.unicode_scalars = total
        .unicode_scalars
        .saturating_add(text.chars().count() as u64);
}

fn write_json(value: &Value, feed: impl FnMut(&[u8])) {
    struct Sink<F>(F);
    impl<F: FnMut(&[u8])> std::io::Write for Sink<F> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            (self.0)(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let _ = serde_json::to_writer(Sink(feed), value);
}

#[derive(Clone, Copy)]
enum Direction {
    Request,
    Response,
}

/// 原样传递 data、trailers、错误及 size_hint，只观察实际被 poll 的前缀。
pub struct ObservedBody<B> {
    inner: Pin<Box<B>>,
    observer: UpstreamIoObserver,
    direction: Direction,
    ended: bool,
    remaining: Option<u64>,
    response_limit: Option<ResponseBodyLimit>,
    received_response_bytes: u64,
    limited_out: bool,
}

impl<B: Body<Data = Bytes>> ObservedBody<B> {
    pub fn request(body: B, observer: UpstreamIoObserver) -> Self {
        Self::with_length(body, observer, Direction::Request, None)
    }

    pub fn response(body: B, observer: UpstreamIoObserver) -> Self {
        Self::with_length(body, observer, Direction::Response, None)
    }

    fn with_length(
        body: B,
        observer: UpstreamIoObserver,
        direction: Direction,
        entity_length: Option<u64>,
    ) -> Self {
        match direction {
            Direction::Request => observer.begin_request_body(),
            Direction::Response => observer.begin_response_body(),
        }
        let remaining = body.size_hint().exact().or(entity_length);
        let ended = body.is_end_stream() || remaining == Some(0);
        if ended {
            match direction {
                Direction::Request => observer.finish_request(),
                Direction::Response => observer.finish_response(),
            }
        }
        Self {
            inner: Box::pin(body),
            observer,
            direction,
            ended,
            remaining,
            response_limit: if matches!(direction, Direction::Response) {
                response_body_limit()
            } else {
                None
            },
            received_response_bytes: 0,
            limited_out: false,
        }
    }

    fn finish(&mut self, failed: bool) {
        if !self.ended {
            match (self.direction, failed) {
                (Direction::Request, false) => self.observer.finish_request(),
                (Direction::Request, true) => self.observer.fail_request(),
                (Direction::Response, false) => self.observer.finish_response(),
                (Direction::Response, true) => self.observer.fail_response(),
            }
            self.ended = true;
        }
    }
}

impl<B: Body<Data = Bytes>> Body for ObservedBody<B>
where
    B::Error: Into<BodyError>,
{
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.limited_out {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(bytes) = frame.data_ref() {
                    if matches!(this.direction, Direction::Response) {
                        this.received_response_bytes = this
                            .received_response_bytes
                            .saturating_add(bytes.len() as u64);
                        if this
                            .response_limit
                            .as_ref()
                            .is_some_and(|limit| limit.observe(this.received_response_bytes))
                        {
                            // 先记实际已读的完整chunk；超额chunk不交给任何正文解码器。
                            this.observer.observe_rejected_response_bytes(bytes.len());
                            this.finish(true);
                            this.limited_out = true;
                            return Poll::Ready(Some(Err(Box::new(ResponseBodyLimitExceeded))));
                        }
                    }
                    match this.direction {
                        Direction::Request => this.observer.observe_request_body(bytes),
                        Direction::Response => this.observer.observe_response_body(bytes),
                    }
                    this.remaining = this
                        .remaining
                        .and_then(|remaining| remaining.checked_sub(bytes.len() as u64));
                }
                if this.inner.is_end_stream() || this.remaining == Some(0) {
                    this.finish(false);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if matches!(this.direction, Direction::Response) {
                    if let Some(limit) = this
                        .response_limit
                        .as_ref()
                        .filter(|limit| limit.exceeded())
                    {
                        // Smithy外层观察拿到的是传输层limit错误，超额frame未交给它；
                        // 补记共享预算已观察的完整原始字节，避免把拒绝的chunk记成零。
                        let actual = limit.observed_bytes();
                        let missing = actual.saturating_sub(this.received_response_bytes);
                        if missing > 0 {
                            this.observer.observe_rejected_response_bytes(
                                missing.min(usize::MAX as u64) as usize,
                            );
                            this.received_response_bytes = actual;
                        }
                        this.limited_out = true;
                    }
                }
                this.finish(true);
                Poll::Ready(Some(Err(error.into())))
            }
            Poll::Ready(None) => {
                this.finish(false);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.limited_out || self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        let original = self.inner.size_hint();
        let Some(limit) = &self.response_limit else {
            return original;
        };
        let remaining = limit.maximum.saturating_sub(self.received_response_bytes);
        let mut bounded = SizeHint::new();
        bounded.set_lower(original.lower().min(remaining));
        bounded.set_upper(original.upper().unwrap_or(remaining).min(remaining));
        bounded
    }
}

impl<B> Drop for ObservedBody<B> {
    fn drop(&mut self) {
        if !self.ended {
            match self.direction {
                Direction::Request => self.observer.fail_request(),
                Direction::Response => self.observer.fail_response(),
            }
        }
    }
}

pub fn wrap_request_body(body: reqwest::Body, observer: UpstreamIoObserver) -> reqwest::Body {
    reqwest::Body::wrap(ObservedBody::request(body, observer))
}

fn entity_content_length(headers: &http::HeaderMap) -> Option<u64> {
    if headers.contains_key(http::header::TRANSFER_ENCODING)
        || headers
            .get(http::header::CONTENT_ENCODING)
            .is_some_and(|value| value.as_bytes() != b"identity")
    {
        return None;
    }
    let mut values = headers.get_all(http::header::CONTENT_LENGTH).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    first.to_str().ok()?.parse().ok()
}

pub fn wrap_response(
    response: reqwest::Response,
    observer: UpstreamIoObserver,
) -> reqwest::Response {
    observer.begin_response(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    );
    let entity_length = entity_content_length(response.headers());
    let url = response.url().clone();
    let response: http::Response<reqwest::Body> = response.into();
    let (mut parts, body) = response.into_parts();
    // reqwest 的公开转换会取走 URL；通过其扩展恢复，同时保留原始 head。
    if let Some(extensions) = http::Response::builder().url(url).extensions_mut() {
        parts.extensions.extend(std::mem::take(extensions));
    }
    http::Response::from_parts(
        parts,
        reqwest::Body::wrap(ObservedBody::with_length(
            body,
            observer,
            Direction::Response,
            entity_length,
        )),
    )
    .into()
}

/// 凭据交换与SDK传输只安装decoder上限，不注册为模型推理/正式用量交换。
pub fn limit_untracked_response(response: reqwest::Response) -> reqwest::Response {
    if response_body_limit().is_some() {
        wrap_response(response, UpstreamIoObserver::new(Protocol::Unknown))
    } else {
        response
    }
}

/// 仅记录已构建请求交给 HTTP client，不注册模型 IO 或改写请求、响应正文。
pub async fn send_unobserved(
    req: reqwest::RequestBuilder,
) -> Result<reqwest::Response, reqwest::Error> {
    let (client, request) = req.build_split();
    let request = request?;
    crate::response_tap::note_http_dispatch_to(|| {
        (
            request.url().scheme(),
            &request.url()[url::Position::BeforeHost..url::Position::BeforePath],
        )
    });
    client.execute(request).await
}

/// 在最终请求已构建、真正交给 client 前建立观察；不改变等待或正文读取时机。
pub async fn send(
    req: reqwest::RequestBuilder,
    protocol: Protocol,
) -> Result<reqwest::Response, reqwest::Error> {
    let (client, request) = req.build_split();
    let mut request = request?;
    let observer = UpstreamIoObserver::observed(protocol);
    let entity_length = entity_content_length(request.headers());
    if let Some(body) = request.body_mut().take() {
        *request.body_mut() = Some(reqwest::Body::wrap(ObservedBody::with_length(
            body,
            observer.clone(),
            Direction::Request,
            entity_length,
        )));
    } else {
        observer.begin_request_body();
        observer.finish_request();
    }
    crate::response_tap::note_http_dispatch_to(|| {
        (
            request.url().scheme(),
            &request.url()[url::Position::BeforeHost..url::Position::BeforePath],
        )
    });
    if let Some(timing) = observer.http_timing_observer() {
        timing.with_timing(|timing| timing.samples.http_send_started_ms = timing.now());
    }
    let response = client.execute(request).await?;
    Ok(wrap_response(response, observer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use reqwest::ResponseBuilderExt;
    use std::sync::Arc;

    async fn assessment_http_body(
        wire: Vec<u8>,
        content_type: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let head = format!("HTTP/1.1 207 Multi-Status\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nX-Assessment-Component: real-http\r\nConnection: close\r\n\r\n", wire.len());
            socket.write_all(head.as_bytes()).await.unwrap();
            // 超限消费者可随时关闭连接，服务器写失败不等于产品错误。
            let _ = socket.write_all(&wire).await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{address}/component-response"), task)
    }

    #[tokio::test]
    async fn assessment_response_body_limit_controls_real_http_before_decoding() {
        let oversized_sse = format!("data: {{\"text\":\"{}\"}}\n\n", "x".repeat(65536));
        let cases = [
            (
                None,
                oversized_sse.as_bytes().to_vec(),
                "text/event-stream",
                false,
            ),
            (Some(0), vec![], "application/json", false),
            (Some(4), "A汉".as_bytes().to_vec(), "text/plain", false),
            (Some(3), "A汉".as_bytes().to_vec(), "text/plain", true),
            (
                Some(16),
                ": metadata-only SSE comment\n\n".repeat(4).into_bytes(),
                "text/event-stream",
                true,
            ),
            (
                Some(65536),
                oversized_sse.into_bytes(),
                "text/event-stream",
                true,
            ),
        ];
        for (maximum, wire, content_type, must_reject) in cases {
            let (url, server) = assessment_http_body(wire.clone(), content_type).await;
            let limit = maximum.map(ResponseBodyLimit::new);
            let tap = Arc::new(crate::ResponseHeaderTap::new(
                vec![],
                false,
                "assessment-http-cap".into(),
            ));
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let (delivered, errored) = scope_response_body_limit(
                limit.clone(),
                crate::response_tap::scope(tap.clone(), async {
                    let response = send(client.get(url), Protocol::Chat).await.unwrap();
                    assert_eq!(response.status(), 207);
                    assert_eq!(response.headers()["x-assessment-component"], "real-http");
                    let mut body = response.bytes_stream();
                    let mut delivered = Vec::new();
                    let mut errored = false;
                    while let Some(frame) = futures::StreamExt::next(&mut body).await {
                        match frame {
                            Ok(bytes) => delivered.extend_from_slice(&bytes),
                            Err(_) => {
                                errored = true;
                                break;
                            }
                        }
                    }
                    (delivered, errored)
                }),
            )
            .await;
            server.await.unwrap();
            assert_eq!(errored, must_reject);
            let audit = tap.upstream_io_observer().unwrap().snapshot();
            if must_reject {
                let maximum = maximum.unwrap();
                assert!(
                    delivered.len() as u64 <= maximum,
                    "超额chunk不能交给decoder"
                );
                assert!(
                    audit.response_body_bytes.unwrap() > maximum,
                    "实际读到的完整超额chunk不能截小记账"
                );
                assert!(!audit.response_complete);
                assert!(limit.as_ref().unwrap().exceeded());
                assert_eq!(
                    limit.as_ref().unwrap().observed_bytes(),
                    audit.response_body_bytes.unwrap()
                );
            } else {
                assert_eq!(delivered, wire);
                assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
                assert!(audit.response_complete);
                assert!(limit.as_ref().is_none_or(|limit| !limit.exceeded()));
            }
            assert!(
                response_body_limit().is_none(),
                "assessment scope不能泄漏到普通请求"
            );
        }
    }

    #[tokio::test]
    async fn assessment_response_body_limit_rejects_a_whole_oversize_single_frame() {
        let limit = ResponseBodyLimit::new(3);
        let io = UpstreamIoObserver::new(Protocol::Chat);
        let mut body = scope_response_body_limit(Some(limit.clone()), async {
            ObservedBody::response(reqwest::Body::from("A汉"), io.clone())
        })
        .await;
        assert_eq!(body.size_hint().upper(), Some(3));
        assert!(
            body.frame().await.unwrap().is_err(),
            "不能交出UTF-8前缀来假装完整回答"
        );
        assert!(body.frame().await.is_none());
        assert_eq!(io.snapshot().response_body_bytes, Some(4));
        assert!(limit.exceeded());
        assert_eq!(limit.observed_bytes(), 4);
    }

    #[tokio::test]
    async fn request_frames_count_transformed_unicode_and_keep_exact_size_hint() {
        let wire = r#"{"messages":[{"role":"user","content":"A汉😀"}]}"#;
        let io = UpstreamIoObserver::new(Protocol::Chat);
        let body = ObservedBody::request(reqwest::Body::from(wire.to_owned()), io.clone());
        assert_eq!(body.size_hint().exact(), Some(wire.len() as u64));
        assert_eq!(body.collect().await.unwrap().to_bytes(), wire.as_bytes());
        let audit = io.snapshot();
        assert_eq!(audit.version, 2);
        assert_eq!(audit.boundary, "upstream_attempt");
        assert_eq!(audit.exchange_count, Some(1));
        assert_eq!(audit.request_body_bytes, Some(wire.len() as u64));
        assert_eq!(audit.request_complete, Some(true));
        assert_eq!(
            audit.input_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true
            })
        );
        let escaped = br#"{"messages":[{"content":"A\u6c49\ud83d\ude00"}]}"#;
        let other = UpstreamIoObserver::new(Protocol::Chat);
        let _ = ObservedBody::request(reqwest::Body::from(escaped.to_vec()), other.clone())
            .collect()
            .await
            .unwrap();
        assert_eq!(other.snapshot().input_text, audit.input_text);
        assert_ne!(
            other.snapshot().request_body_bytes,
            audit.request_body_bytes
        );
    }

    #[tokio::test]
    async fn response_wrapper_preserves_head_url_extensions_size_and_frame_bytes() {
        let url = reqwest::Url::parse("https://upstream.example/v1/chat/completions?q=1").unwrap();
        let wire = br#"{"choices":[{"message":{"content":""}}]}"#;
        let source = http::Response::builder()
            .status(202)
            .version(http::Version::HTTP_2)
            .url(url.clone())
            .header("x-request-id", "upstream-1")
            .extension(73_u32)
            .body(reqwest::Body::from(wire.to_vec()))
            .unwrap();
        let response = reqwest::Response::from(source);
        let io = UpstreamIoObserver::new(Protocol::Chat);
        let response = wrap_response(response, io.clone());
        assert_eq!(response.status(), 202);
        assert_eq!(response.version(), http::Version::HTTP_2);
        assert_eq!(response.url(), &url);
        assert_eq!(response.headers()["x-request-id"], "upstream-1");
        assert_eq!(response.extensions().get::<u32>(), Some(&73));
        assert_eq!(response.content_length(), Some(wire.len() as u64));
        assert_eq!(response.bytes().await.unwrap().as_ref(), wire);
        let audit = io.snapshot();
        assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
        assert!(audit.response_complete);
        assert_eq!(
            audit.output_text,
            Some(TextMeasurement {
                utf8_bytes: 0,
                unicode_scalars: 0,
                complete: true
            })
        );
    }

    #[test]
    fn unpolled_drop_is_observed_zero_and_never_complete() {
        let io = UpstreamIoObserver::new(Protocol::Chat);
        assert_eq!(io.snapshot().request_body_bytes, None);
        assert_eq!(io.snapshot().request_complete, None);
        drop(ObservedBody::request(
            reqwest::Body::from("unpolled"),
            io.clone(),
        ));
        assert_eq!(io.snapshot().request_body_bytes, Some(0));
        assert_eq!(io.snapshot().request_complete, Some(false));
        assert_eq!(io.snapshot().input_text, None);
        drop(ObservedBody::response(
            reqwest::Body::from("unpolled"),
            io.clone(),
        ));
        assert_eq!(io.snapshot().response_body_bytes, Some(0));
        assert!(!io.snapshot().response_complete);
        assert_eq!(io.snapshot().output_text, None);
    }

    #[tokio::test]
    async fn data_prefix_then_body_error_is_not_replaced_or_completed() {
        let io = UpstreamIoObserver::new(Protocol::Chat);
        let source = futures::stream::iter([
            Ok(bytes::Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"ab",
            )),
            Err(std::io::Error::other("intentional body failure")),
        ]);
        let mut body = ObservedBody::response(reqwest::Body::wrap_stream(source), io.clone());
        assert!(body.frame().await.unwrap().is_ok());
        let error = body.frame().await.unwrap().unwrap_err();
        assert!(std::error::Error::source(error.as_ref()).is_some());
        assert!(!io.snapshot().response_complete);
        assert_eq!(
            io.snapshot().output_text,
            Some(TextMeasurement {
                utf8_bytes: 2,
                unicode_scalars: 2,
                complete: false
            })
        );
        drop(body);
        assert!(!io.snapshot().response_complete);
    }

    #[tokio::test]
    async fn capture_record_take_and_late_writes_keep_original_exchange_handles() {
        let tap = Arc::new(crate::ResponseHeaderTap::new(
            Vec::new(),
            false,
            String::new(),
        ));
        let first = crate::response_tap::scope(tap.clone(), async {
            let io = UpstreamIoObserver::observed(Protocol::Chat);
            let stream = crate::response_tap::stream_usage_observer().unwrap();
            crate::note_upstream_response(&http::HeaderMap::new());
            assert!(tap.capture().stream_usage.is_some());
            assert_eq!(tap.capture().stream_usage, Some(stream));
            io
        })
        .await;
        let saved = tap.take();
        assert!(tap.capture().is_empty());
        let saved_io = saved.io.unwrap();
        first.observe_response_body(br#"{"choices":[{"message":{"content":"x"}}]}"#);
        first.finish_response();
        assert_eq!(saved_io.snapshot().output_text.unwrap().utf8_bytes, 1);
        crate::response_tap::scope(tap.clone(), async {
            let io = UpstreamIoObserver::observed(Protocol::Chat);
            io.observe_response_body(br#"{"choices":[{"message":{"content":"yy"}}]}"#);
            io.finish_response();
        })
        .await;
        assert_eq!(
            tap.capture()
                .io
                .unwrap()
                .snapshot()
                .output_text
                .unwrap()
                .utf8_bytes,
            2
        );
        assert_eq!(saved_io.snapshot().output_text.unwrap().utf8_bytes, 1);
    }

    #[tokio::test]
    async fn sequential_exchanges_sum_explicitly_without_turning_unknown_into_zero() {
        let tap = Arc::new(crate::ResponseHeaderTap::new(
            Vec::new(),
            false,
            String::new(),
        ));
        crate::response_tap::scope(tap.clone(), async {
            let first = UpstreamIoObserver::observed(Protocol::Embeddings);
            first.observe_request_json(&serde_json::json!({"input":"A汉😀"}));
            first.finish_response();
            let second = UpstreamIoObserver::observed(Protocol::Embeddings);
            second.observe_request_json(&serde_json::json!({"input":""}));
            second.finish_response();
        })
        .await;
        let audit = tap.capture().io.unwrap().snapshot();
        assert_eq!(audit.exchange_count, Some(2));
        assert_eq!(audit.request_body_bytes, None);
        assert_eq!(
            audit.input_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true
            })
        );
        assert_eq!(audit.output_text, None);
    }

    #[test]
    fn responses_snapshot_does_not_add_delta_done_or_terminal_twice() {
        let io = UpstreamIoObserver::new(Protocol::Responses);
        io.observe_response_json(
            &serde_json::json!({"type":"response.output_text.delta","delta":"A汉😀"}),
        );
        io.observe_response_json(
            &serde_json::json!({"type":"response.output_text.done","text":"A汉😀"}),
        );
        io.observe_response_json(&serde_json::json!({"type":"response.completed","response":{"output":[{"content":[{"text":"A汉😀"}]}]}}));
        io.finish_response();
        assert_eq!(io.snapshot().response_body_bytes, None);
        assert_eq!(
            io.snapshot().output_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true
            })
        );
    }

    #[tokio::test]
    async fn generic_wrapper_passes_trailers_and_original_error_type() {
        let io = UpstreamIoObserver::new(Protocol::Unknown);
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-upstream-tail", "kept".parse().unwrap());
        let source = http_body_util::StreamBody::new(futures::stream::iter([
            Ok::<_, std::io::Error>(Frame::data(Bytes::from_static(b"raw"))),
            Ok(Frame::trailers(trailers.clone())),
        ]));
        let collected = ObservedBody::response(source, io.clone())
            .collect()
            .await
            .unwrap();
        assert_eq!(collected.trailers(), Some(&trailers));
        assert_eq!(collected.to_bytes(), "raw");
        assert_eq!(io.snapshot().response_body_bytes, Some(3));
        assert!(io.snapshot().response_complete);
        let source =
            http_body_util::StreamBody::new(futures::stream::iter([Err::<Frame<Bytes>, _>(
                std::io::Error::from_raw_os_error(104),
            )]));
        let mut body = ObservedBody::request(source, io.clone());
        let error = body.frame().await.unwrap().unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(104)
        );
        assert_eq!(io.snapshot().request_complete, Some(false));
    }

    #[test]
    fn binary_body_eof_does_not_complete_unfinished_sdk_text() {
        let io = UpstreamIoObserver::new(Protocol::Converse);
        io.begin_response(Some("application/vnd.amazon.eventstream"));
        io.observe_response_body(b"binary-frame");
        io.observe_output_text("A汉😀");
        io.finish_response();
        let partial = io.snapshot();
        assert!(partial.response_complete);
        assert_eq!(partial.response_body_bytes, Some(12));
        assert_eq!(
            partial.output_text.unwrap(),
            TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: false
            }
        );
        io.finish_output_text();
        assert!(io.snapshot().output_text.unwrap().complete);
        io.mark_output_text_partial();
        io.finish_output_text();
        assert!(!io.snapshot().output_text.unwrap().complete);
    }

    #[test]
    fn completed_and_failed_calls_release_aggregate_parser_handles_at_scale() {
        let first = UpstreamIoObserver::new(Protocol::Embeddings);
        let aggregate = UpstreamIoObserver::aggregate(&first);
        first.observe_request_json(&serde_json::json!({"input":"A汉😀"}));
        first.finish_response();
        let held = first.clone();
        drop(first);
        for index in 1..5000 {
            let call = UpstreamIoObserver::new(Protocol::Embeddings);
            let state = call.members().pop().unwrap();
            let weak = Arc::downgrade(&state);
            aggregate.append(&call);
            call.observe_request_json(&serde_json::json!({"input":"A汉😀"}));
            if index % 2 == 0 {
                call.finish_response();
            } else {
                call.fail_response();
            }
            drop(state);
            drop(call);
            assert!(
                weak.upgrade().is_none(),
                "completed exchange must not be retained at {index}"
            );
        }
        let audit = aggregate.snapshot();
        assert_eq!(audit.exchange_count, Some(5000));
        assert_eq!(audit.input_text.unwrap().utf8_bytes, 5000 * 8);
        assert_eq!(audit.request_body_bytes, None);
        assert_eq!(audit.response_body_bytes, None);
        assert!(!audit.response_complete);
        assert_eq!(held.snapshot().exchange_count, Some(1));
        assert_eq!(held.snapshot().input_text.unwrap().utf8_bytes, 8);
    }

    #[test]
    fn partial_raw_sdk_request_never_reports_full_precomputed_input() {
        let io = UpstreamIoObserver::new(Protocol::Messages);
        io.observe_input_measurement(TextMeasurement {
            utf8_bytes: 9000,
            unicode_scalars: 9000,
            complete: true,
        });
        io.begin_request_body();
        io.observe_request_body(b"{\"input\":");
        io.fail_request();
        assert_eq!(io.snapshot().request_body_bytes, Some(9));
        assert_eq!(io.snapshot().request_complete, Some(false));
        assert_eq!(io.snapshot().input_text, None);
    }

    #[test]
    fn rejected_prepared_websocket_message_does_not_count_planned_payload() {
        let io = UpstreamIoObserver::websocket(Protocol::Responses);
        let payload = br#"{"type":"response.create","response":{"input":"abc"}}"#;
        {
            let _rejected = io.prepare_request_message(payload);
        }
        assert_eq!(io.snapshot().request_body_bytes, Some(0));
        assert_eq!(io.snapshot().input_text, None);
        let accepted = io.prepare_request_message(payload);
        io.observe_prepared_request_message(accepted);
        io.finish_request();
        assert_eq!(io.snapshot().request_body_bytes, Some(payload.len() as u64));
        assert_eq!(io.snapshot().input_text.unwrap().utf8_bytes, 3);
    }

    #[test]
    fn sdk_event_bytes_and_borrowed_tool_serialization_do_not_double_body_bytes() {
        let io = UpstreamIoObserver::new(Protocol::Messages);
        io.begin_response(Some("application/vnd.amazon.eventstream"));
        io.observe_response_body(b"binary");
        assert!(!io.observe_response_event_bytes(
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"A汉😀"}}"#
                .as_bytes()
        ));
        assert!(io.observe_response_event_bytes(br#"{"type":"message_stop"}"#));
        io.finish_response();
        assert_eq!(io.snapshot().response_body_bytes, Some(6));
        assert_eq!(
            io.snapshot().output_text.unwrap(),
            TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true
            }
        );
        let io = UpstreamIoObserver::new(Protocol::Converse);
        io.observe_output_serialized(&serde_json::json!({"x":"A汉😀"}))
            .unwrap();
        io.finish_output_text();
        assert_eq!(
            io.snapshot().output_text.unwrap(),
            TextMeasurement {
                utf8_bytes: 16,
                unicode_scalars: 11,
                complete: true
            }
        );
        assert_eq!(io.snapshot().response_body_bytes, None);
    }

    #[test]
    fn dropped_live_calls_fold_partial_state_and_mixed_transport_once() {
        let http = UpstreamIoObserver::new(Protocol::Embeddings);
        let aggregate = UpstreamIoObserver::aggregate(&http);
        http.observe_request_json(&serde_json::json!({"input":"x"}));
        http.finish_response();
        let websocket = UpstreamIoObserver::websocket(Protocol::Responses);
        aggregate.append(&websocket);
        websocket
            .observe_request_message(br#"{"type":"response.create","response":{"input":"ab"}}"#);
        websocket.finish_request();
        websocket.observe_response_message(br#"{"type":"response.output_text.delta","delta":"z"}"#);
        drop(websocket);
        assert!(aggregate.0.lock().unwrap().live.is_empty());
        let measured = aggregate.snapshot();
        assert_eq!(measured.exchange_count, Some(2));
        assert_eq!(measured.transport.as_deref(), Some("mixed"));
        assert_eq!(measured.request_body_bytes, None);
        assert_eq!(measured.response_body_bytes, None);
        assert_eq!(measured.input_text.unwrap().utf8_bytes, 3);
        assert_eq!(measured.output_text.unwrap().utf8_bytes, 1);
        assert!(!measured.output_text.unwrap().complete);
        assert!(!measured.response_complete);
        assert_eq!(http.snapshot().exchange_count, Some(1));
        let saved = aggregate.snapshot();
        drop(http);
        assert_eq!(aggregate.snapshot(), saved);
    }

    #[tokio::test]
    async fn chat_audio_output_transcripts_preserve_unary_and_streaming_bodies() {
        let unary = r#"{"choices":[{"message":{"content":null,"audio":{"data":"UklGRiQAAABXQVZF","transcript":"A汉😀"}}}]}"#;
        let streaming = concat!(
            "data: {\"choices\":[{\"delta\":{\"audio\":{\"data\":\"UklGRiQAAABXQVZF\",\"transcript\":\"A\"}}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"audio\":{\"transcript\":\"汉😀\"}}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        for wire in [unary, streaming] {
            let io = UpstreamIoObserver::new(Protocol::Chat);
            let source = http::Response::new(reqwest::Body::from(wire.to_owned()));
            let delivered = wrap_response(source.into(), io.clone())
                .bytes()
                .await
                .unwrap();
            assert_eq!(delivered.as_ref(), wire.as_bytes());
            let audit = io.snapshot();
            assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
            assert!(audit.response_complete);
            assert_eq!(
                audit.output_text,
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: false
                })
            );
        }
    }

    #[tokio::test]
    async fn chat_history_audio_transcript_is_visible_prose_with_partial_media_coverage() {
        let wire = r#"{"messages":[{"role":"assistant","content":null,"audio":{"id":"audio_prior","transcript":"A汉😀"}}]}"#;
        let io = UpstreamIoObserver::new(Protocol::Chat);
        let delivered = ObservedBody::request(reqwest::Body::from(wire.to_owned()), io.clone())
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(delivered.as_ref(), wire.as_bytes());
        let audit = io.snapshot();
        assert_eq!(audit.request_body_bytes, Some(wire.len() as u64));
        assert_eq!(audit.request_complete, Some(true));
        assert_eq!(
            audit.input_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: false
            })
        );
    }

    #[tokio::test]
    async fn responses_encrypted_reasoning_is_excluded_and_keeps_visible_text_partial() {
        let request = r#"{"input":[{"role":"user","content":"A汉😀"},{"id":"rs_prior","type":"reasoning","summary":[],"encrypted_content":"opaque_reasoning_ciphertext"}]}"#;
        let io = UpstreamIoObserver::new(Protocol::Responses);
        let delivered = ObservedBody::request(reqwest::Body::from(request.to_owned()), io.clone())
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(delivered.as_ref(), request.as_bytes());
        assert_eq!(io.snapshot().request_body_bytes, Some(request.len() as u64));
        assert_eq!(
            io.snapshot().input_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: false
            })
        );
        let unary = r#"{"output":[{"id":"rs_out","type":"reasoning","summary":[{"type":"summary_text","text":"A汉😀"}],"encrypted_content":"opaque_reasoning_ciphertext"}]}"#;
        let streaming = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"A汉😀\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"opaque_reasoning_ciphertext\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"A汉😀\"}],\"encrypted_content\":\"opaque_reasoning_ciphertext\"}]}}\n\n",
        );
        for wire in [unary, streaming] {
            let io = UpstreamIoObserver::new(Protocol::Responses);
            let source = http::Response::new(reqwest::Body::from(wire.to_owned()));
            let delivered = wrap_response(source.into(), io.clone())
                .bytes()
                .await
                .unwrap();
            assert_eq!(delivered.as_ref(), wire.as_bytes());
            let audit = io.snapshot();
            assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
            assert!(audit.response_complete);
            assert_eq!(
                audit.output_text,
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: false
                })
            );
        }
    }

    #[test]
    fn responses_opaque_only_reasoning_cannot_be_complete_zero_and_empty_fields_are_not_opaque() {
        let io = UpstreamIoObserver::new(Protocol::Responses);
        io.observe_response_json(&serde_json::json!({"output":[{"type":"reasoning","summary":[],"encrypted_content":"opaque"}]}));
        io.finish_response();
        assert_eq!(
            io.snapshot().output_text,
            Some(TextMeasurement {
                utf8_bytes: 0,
                unicode_scalars: 0,
                complete: false
            })
        );
        let io = UpstreamIoObserver::new(Protocol::Responses);
        io.observe_request_json(&serde_json::json!({"input":[{"type":"reasoning","summary":[],"encrypted_content":"opaque"}]}));
        assert_eq!(io.snapshot().input_text, None);
        for encrypted in [serde_json::Value::Null, serde_json::json!("")] {
            let io = UpstreamIoObserver::new(Protocol::Responses);
            io.observe_request_json(&serde_json::json!({"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"A汉😀"}],"encrypted_content":encrypted}]}));
            assert_eq!(
                io.snapshot().input_text,
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: true
                })
            );
            io.observe_response_json(&serde_json::json!({"output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"A汉😀"}],"encrypted_content":encrypted}]}));
            io.finish_response();
            assert_eq!(
                io.snapshot().output_text,
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: true
                })
            );
        }
    }

    #[test]
    fn responses_verified_stored_context_references_keep_local_input_partial() {
        for reference in [
            serde_json::json!({"previous_response_id":"resp_prior"}),
            serde_json::json!({"conversation":"conv_prior"}),
            serde_json::json!({"conversation":{"id":"conv_prior"}}),
            serde_json::json!({"prompt":{"id":"pmpt_saved","version":"1"}}),
        ] {
            let mut request = reference;
            request["input"] = serde_json::json!("A汉😀");
            let io = UpstreamIoObserver::new(Protocol::Responses);
            io.observe_request_json(&request);
            assert_eq!(
                io.snapshot().input_text,
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: false
                })
            );
        }
    }

    #[tokio::test]
    async fn transcription_terminal_text_replaces_deltas_and_marks_real_eof_complete() {
        for wire in [
            "data: {\"type\":\"transcript.text.done\",\"text\":\"A汉😀\"}\n\n".to_owned(),
            concat!(
                "data: {\"type\":\"transcript.text.delta\",\"delta\":\"old partial\"}\n\n",
                "data: {\"type\":\"transcript.text.done\",\"text\":\"A汉😀\"}\n\n",
            )
            .to_owned(),
        ] {
            let io = UpstreamIoObserver::new(Protocol::Transcription);
            let source = http::Response::new(reqwest::Body::from(wire.clone()));
            let delivered = wrap_response(source.into(), io.clone())
                .bytes()
                .await
                .unwrap();
            assert_eq!(delivered.as_ref(), wire.as_bytes());
            let audit = io.snapshot();
            assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
            assert!(audit.response_complete);
            assert_eq!(
                audit.output_text,
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: true
                })
            );
        }
    }

    #[tokio::test]
    async fn transcription_cancellation_counts_only_received_delta_prefix() {
        let prefix = "data: {\"type\":\"transcript.text.delta\",\"delta\":\"A汉😀\"}\n\n";
        let io = UpstreamIoObserver::new(Protocol::Transcription);
        let source = futures::StreamExt::chain(
            futures::stream::once(async move {
                Ok::<_, std::io::Error>(Bytes::from(prefix.to_owned()))
            }),
            futures::stream::pending(),
        );
        let mut body = ObservedBody::response(reqwest::Body::wrap_stream(source), io.clone());
        let delivered = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(delivered.as_ref(), prefix.as_bytes());
        drop(body);
        let audit = io.snapshot();
        assert_eq!(audit.response_body_bytes, Some(prefix.len() as u64));
        assert!(!audit.response_complete);
        assert_eq!(
            audit.output_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: false
            })
        );
    }

    #[tokio::test]
    async fn unary_images_exclude_opaque_payloads_from_text_and_mark_coverage_partial() {
        for wire in [
            r#"{"data":[{"b64_json":"aGVsbG8=","revised_prompt":"A汉😀"}]}"#,
            r#"{"data":[{"url":"https://images.example/generated.png","revised_prompt":"A汉😀"}]}"#,
        ] {
            let io = UpstreamIoObserver::new(Protocol::Images);
            let source = http::Response::new(reqwest::Body::from(wire.to_owned()));
            let delivered = wrap_response(source.into(), io.clone())
                .bytes()
                .await
                .unwrap();
            assert_eq!(delivered.as_ref(), wire.as_bytes());
            let audit = io.snapshot();
            assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
            assert!(audit.response_complete);
            assert_eq!(
                audit.output_text,
                Some(TextMeasurement {
                    utf8_bytes: 8,
                    unicode_scalars: 3,
                    complete: false
                })
            );
        }
    }

    struct ExactLengthPending {
        data: Option<Bytes>,
        trailers: Option<http::HeaderMap>,
        remaining: u64,
    }

    impl Body for ExactLengthPending {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if let Some(data) = self.data.take() {
                self.remaining -= data.len() as u64;
                return Poll::Ready(Some(Ok(Frame::data(data))));
            }
            if let Some(trailers) = self.trailers.take() {
                return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
            }
            Poll::Pending
        }
        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(self.remaining)
        }
        fn is_end_stream(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn exact_entity_length_completes_at_last_data_without_polling_for_eof_or_losing_trailers()
    {
        let request = br#"{"input":"x"}"#;
        let io = UpstreamIoObserver::new(Protocol::Embeddings);
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-tail", "kept".parse().unwrap());
        let mut body = ObservedBody::request(
            ExactLengthPending {
                data: Some(Bytes::from_static(request)),
                trailers: Some(trailers.clone()),
                remaining: request.len() as u64,
            },
            io.clone(),
        );
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            request.as_slice()
        );
        assert_eq!(io.snapshot().request_complete, Some(true));
        assert!(
            !body.is_end_stream(),
            "the wrapped body's original stream semantics must remain intact"
        );
        assert_eq!(
            body.frame()
                .await
                .unwrap()
                .unwrap()
                .into_trailers()
                .unwrap(),
            trailers
        );
        drop(body);
        assert_eq!(io.snapshot().request_body_bytes, Some(request.len() as u64));
        assert_eq!(io.snapshot().request_complete, Some(true));
        let response = r#"{"choices":[{"message":{"content":"A汉😀"}}]}"#;
        let io = UpstreamIoObserver::new(Protocol::Chat);
        let mut body = ObservedBody::response(
            ExactLengthPending {
                data: Some(Bytes::from(response.to_owned())),
                trailers: None,
                remaining: response.len() as u64,
            },
            io.clone(),
        );
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            response.as_bytes()
        );
        assert!(io.snapshot().response_complete);
        drop(body);
        assert!(io.snapshot().response_complete);
        assert_eq!(
            io.snapshot().output_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: true
            })
        );
    }

    #[tokio::test]
    async fn semantic_terminal_completes_model_text_while_chunked_entity_drop_stays_partial() {
        for (protocol, wire) in [
            (Protocol::Chat, "data: {\"choices\":[{\"delta\":{\"content\":\"A汉😀\"}}]}\n\ndata: [DONE]\n\n"),
            (Protocol::Messages, "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"A汉😀\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n"),
        ] {
            let io = UpstreamIoObserver::new(protocol);
            let source = futures::StreamExt::chain(futures::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(wire.to_owned())) }), futures::stream::pending());
            let mut body = ObservedBody::response(reqwest::Body::wrap_stream(source), io.clone());
            assert_eq!(body.frame().await.unwrap().unwrap().into_data().unwrap(), wire.as_bytes());
            drop(body);
            let audit = io.snapshot();
            assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
            assert!(!audit.response_complete);
            assert_eq!(audit.output_text, Some(TextMeasurement { utf8_bytes: 8, unicode_scalars: 3, complete: true }));
        }
    }

    #[tokio::test]
    async fn numeric_embedding_vectors_are_known_zero_text_and_unknown_shapes_remain_unknown() {
        for wire in [
            r#"{"embedding":[0.25,-0.5]}"#,
            r#"{"data":[{"embedding":[0.25,-0.5]}]}"#,
            r#"{"predictions":[{"embeddings":{"values":[0.25,-0.5]}}]}"#,
        ] {
            let io = UpstreamIoObserver::new(Protocol::Embeddings);
            let source = http::Response::new(reqwest::Body::from(wire.to_owned()));
            let delivered = wrap_response(source.into(), io.clone())
                .bytes()
                .await
                .unwrap();
            assert_eq!(delivered.as_ref(), wire.as_bytes());
            let audit = io.snapshot();
            assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
            assert_eq!(
                audit.output_text,
                Some(TextMeasurement {
                    utf8_bytes: 0,
                    unicode_scalars: 0,
                    complete: true
                })
            );
        }
        let io = UpstreamIoObserver::new(Protocol::Embeddings);
        io.observe_response_json(&serde_json::json!({"future_output":"unknown"}));
        io.finish_response();
        assert_eq!(io.snapshot().output_text, None);
    }

    #[tokio::test]
    async fn multipart_known_header_length_finishes_when_real_upstream_has_received_all_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let (header_end, length) = loop {
                let mut bytes = [0; 4096];
                let read = socket.read(&mut bytes).await.unwrap();
                assert!(read > 0);
                received.extend_from_slice(&bytes[..read]);
                if let Some(end) = received.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&received[..end]).unwrap();
                    let length = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .unwrap()
                        .1
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while received.len() < header_end + length {
                let mut bytes = [0; 4096];
                let read = socket.read(&mut bytes).await.unwrap();
                assert!(read > 0);
                received.extend_from_slice(&bytes[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
            received[header_end..header_end + length].to_vec()
        });
        let tap = Arc::new(crate::ResponseHeaderTap::new(
            Vec::new(),
            false,
            String::new(),
        ));
        let request = reqwest::Client::new()
            .post(format!("http://{address}/audio/transcriptions"))
            .multipart(reqwest::multipart::Form::new().text("prompt", "A汉😀"));
        let response =
            crate::response_tap::scope(tap.clone(), send(request, Protocol::Transcription))
                .await
                .unwrap();
        assert_eq!(response.bytes().await.unwrap(), "{}");
        let received = upstream.await.unwrap();
        let audit = tap.capture().io.unwrap().snapshot();
        assert_eq!(audit.request_body_bytes, Some(received.len() as u64));
        assert_eq!(audit.request_complete, Some(true));
    }

    #[tokio::test]
    async fn one_finished_chat_choice_does_not_complete_another_choice_before_entity_eof() {
        let wire = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"A\"},\"finish_reason\":\"stop\"},{\"index\":1,\"delta\":{\"content\":\"汉😀\"},\"finish_reason\":null}]}\n\n";
        let io = UpstreamIoObserver::new(Protocol::Chat);
        let source = futures::StreamExt::chain(
            futures::stream::once(
                async move { Ok::<_, std::io::Error>(Bytes::from(wire.to_owned())) },
            ),
            futures::stream::pending(),
        );
        let mut body = ObservedBody::response(reqwest::Body::wrap_stream(source), io.clone());
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            wire.as_bytes()
        );
        drop(body);
        assert!(!io.snapshot().response_complete);
        assert_eq!(
            io.snapshot().output_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: false
            })
        );
    }

    #[tokio::test]
    async fn clean_sse_eof_does_not_complete_unfinished_chat_or_completion_choices() {
        for (protocol, wire) in [
            (Protocol::Chat, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"A汉😀\"},\"finish_reason\":null}]}\n\n"),
            (Protocol::Chat, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"A\"},\"finish_reason\":\"stop\"},{\"index\":1,\"delta\":{\"content\":\"汉😀\"},\"finish_reason\":null}]}\n\n"),
            (Protocol::Completions, "data: {\"choices\":[{\"index\":0,\"text\":\"A汉😀\",\"finish_reason\":null}]}\n\n"),
            (Protocol::Completions, "data: {\"choices\":[{\"index\":0,\"text\":\"A\",\"finish_reason\":\"stop\"},{\"index\":1,\"text\":\"汉😀\",\"finish_reason\":null}]}\n\n"),
        ] {
            let io = UpstreamIoObserver::new(protocol);
            let source = http::Response::new(reqwest::Body::from(wire.to_owned()));
            let delivered = wrap_response(source.into(), io.clone()).bytes().await.unwrap();
            assert_eq!(delivered.as_ref(), wire.as_bytes());
            let audit = io.snapshot();
            assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
            assert!(audit.response_complete, "clean entity EOF is distinct from model-text completion");
            assert_eq!(audit.output_text, Some(TextMeasurement { utf8_bytes: 8, unicode_scalars: 3, complete: false }));
        }
    }

    #[tokio::test]
    async fn opaque_reasoning_item_done_remains_partial_when_terminal_snapshot_omits_ciphertext() {
        let wire = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"A汉😀\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"opaque_only_here\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"A汉😀\"}]}]}}\n\n",
        );
        let io = UpstreamIoObserver::new(Protocol::Responses);
        let source = http::Response::new(reqwest::Body::from(wire.to_owned()));
        let delivered = wrap_response(source.into(), io.clone())
            .bytes()
            .await
            .unwrap();
        assert_eq!(delivered.as_ref(), wire.as_bytes());
        let audit = io.snapshot();
        assert_eq!(audit.response_body_bytes, Some(wire.len() as u64));
        assert!(audit.response_complete);
        assert_eq!(
            audit.output_text,
            Some(TextMeasurement {
                utf8_bytes: 8,
                unicode_scalars: 3,
                complete: false
            })
        );
    }
}
