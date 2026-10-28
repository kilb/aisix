//! Reading the protocol-level facts of an A2A call out of its JSON-RPC
//! envelopes: which operation was invoked, and which task / context / task
//! state the call touched.
//!
//! The gateway forwards A2A bodies verbatim (see [`crate::bridge`]), so an
//! operator running a mix of agents sees BOTH wire vocabularies on the same
//! endpoint: an agent pinned to 0.3 is called with `message/send`, one pinned
//! to 1.0 with `SendMessage`, and those are the same operation. Everything
//! here exists to record one fact once — a canonical operation name, a task
//! id, a task state — regardless of which version produced it.
//!
//! Wire references:
//! - Methods: the 1.0 RPC names (`SendMessage`, `GetTask`, …) are section 9.4
//!   of the specification; the `message/send`-style names are their 0.3
//!   spelling. <https://a2a-protocol.org/latest/specification/>
//! - `TaskState`: the 1.0 enum is `TASK_STATE_<NAME>`; its 0.3 wire string is
//!   the name lowercased with `_` → `-`, which is why [`normalize_task_state`]
//!   needs no per-value table.
//! - A `Task` carries `id`, `contextId` and `status.state`; the streaming
//!   update events carry `taskId` / `contextId` instead. Both shapes are read
//!   by [`A2aCallFacts::observe_result`].

use serde_json::Value;

/// Bounded label for an operation this gateway does not recognise. A caller
/// picks the JSON-RPC method, so the raw value can be anything at all; it is
/// kept in `a2a_method` for forensics and collapsed to this in every
/// aggregated position.
pub const UNKNOWN_OPERATION: &str = "unknown";

/// Bounded label for a task state that is absent, unspecified, or not one the
/// specification defines.
pub const UNKNOWN_TASK_STATE: &str = "unknown";

/// The canonical operation names — the vocabulary the dashboard, metrics and
/// docs use. Each is a method's 0.3 spelling where 0.3 defines the operation,
/// and the 1.0 PascalCase RPC name for the same operation maps onto it.
///
/// `ListTasks` is the exception: 1.0 added it, so there is no 0.3 spelling to
/// borrow and only the 1.0 method appears on the left. Its canonical name
/// follows the same shape as the rest so the vocabulary stays uniform.
const OPERATIONS: &[(&str, &str)] = &[
    // (wire method, canonical operation)
    ("message/send", "message/send"),
    ("SendMessage", "message/send"),
    ("message/stream", "message/stream"),
    ("SendStreamingMessage", "message/stream"),
    ("tasks/get", "tasks/get"),
    ("GetTask", "tasks/get"),
    ("ListTasks", "tasks/list"),
    ("tasks/cancel", "tasks/cancel"),
    ("CancelTask", "tasks/cancel"),
    ("tasks/resubscribe", "tasks/resubscribe"),
    ("SubscribeToTask", "tasks/resubscribe"),
    (
        "tasks/pushNotificationConfig/set",
        "tasks/pushNotificationConfig/set",
    ),
    (
        "CreateTaskPushNotificationConfig",
        "tasks/pushNotificationConfig/set",
    ),
    (
        "tasks/pushNotificationConfig/get",
        "tasks/pushNotificationConfig/get",
    ),
    (
        "GetTaskPushNotificationConfig",
        "tasks/pushNotificationConfig/get",
    ),
    (
        "tasks/pushNotificationConfig/list",
        "tasks/pushNotificationConfig/list",
    ),
    (
        "ListTaskPushNotificationConfigs",
        "tasks/pushNotificationConfig/list",
    ),
    (
        "tasks/pushNotificationConfig/delete",
        "tasks/pushNotificationConfig/delete",
    ),
    (
        "DeleteTaskPushNotificationConfig",
        "tasks/pushNotificationConfig/delete",
    ),
    (
        "agent/getAuthenticatedExtendedCard",
        "agent/getAuthenticatedExtendedCard",
    ),
    ("GetExtendedAgentCard", "agent/getAuthenticatedExtendedCard"),
];

/// The task states the specification defines, in their 0.3 wire spelling.
///
/// 0.3's own enum also carries a literal `unknown` (1.0 replaced it with
/// `TASK_STATE_UNSPECIFIED`). It is deliberately absent here so it falls
/// through to [`UNKNOWN_TASK_STATE`] — the recorded string is the same either
/// way, and listing it would only imply the two are distinguishable.
const TASK_STATES: &[&str] = &[
    "submitted",
    "working",
    "input-required",
    "completed",
    "canceled",
    "failed",
    "rejected",
    "auth-required",
];

/// Map a wire method to its canonical operation, collapsing an unrecognised
/// one to [`UNKNOWN_OPERATION`] so it is safe to use as a metric label.
///
/// Both wire vocabularies map onto the 0.3 spelling: `SendStreamingMessage`
/// and `message/stream` are one operation, so a deployment fronting agents on
/// both versions still aggregates as one.
pub fn canonical_operation(method: &str) -> &'static str {
    OPERATIONS
        .iter()
        .find(|(wire, _)| *wire == method)
        .map(|(_, canonical)| *canonical)
        .unwrap_or(UNKNOWN_OPERATION)
}

/// Whether an operation's response is an SSE event stream rather than a single
/// JSON-RPC envelope.
///
/// Takes the CANONICAL operation, so a 1.0 caller's `SendStreamingMessage`
/// cannot be routed down the buffering path just because the match arm listed
/// only the 0.3 spelling.
pub fn is_streaming_operation(operation: &str) -> bool {
    matches!(operation, "message/stream" | "tasks/resubscribe")
}

/// Normalize a wire task state to its 0.3 spelling, or [`UNKNOWN_TASK_STATE`]
/// when it is absent, `TASK_STATE_UNSPECIFIED`, or not a state the
/// specification defines.
///
/// The 1.0 protobuf enum name (`TASK_STATE_INPUT_REQUIRED`) becomes the 0.3
/// wire string (`input-required`) by dropping the prefix, lowercasing and
/// swapping `_` for `-`; the result is validated against the defined set, so a
/// state invented by an upstream lands on `unknown` rather than becoming an
/// unbounded label.
pub fn normalize_task_state(state: &str) -> &'static str {
    let stripped = state.strip_prefix("TASK_STATE_").unwrap_or(state);
    let candidate = stripped.to_ascii_lowercase().replace('_', "-");
    TASK_STATES
        .iter()
        .find(|known| **known == candidate)
        .copied()
        .unwrap_or(UNKNOWN_TASK_STATE)
}

/// Task states that mean the agent has said everything it is going to say on
/// this stream: the task is finished, or it is waiting on the caller.
const STREAM_ENDING_STATES: &[&str] = &[
    "completed",
    "canceled",
    "failed",
    "rejected",
    "input-required",
    "auth-required",
];

/// Whether a streamed event is the last one its stream will carry.
///
/// A caller stops reading here, which makes this — not the upstream's
/// eventual EOF — the moment the response was fully delivered. Anything that
/// waits for the connection to close instead reports a completed task as
/// abandoned whenever the agent leaves the stream open afterwards.
///
/// 0.3 says so with `final`. 1.0's `TaskStatusUpdateEvent` has no such field
/// at all, so there the task's own state is the only signal: a task that is
/// neither `submitted` nor `working` has nothing further to stream until the
/// caller acts.
pub fn is_stream_end(event: &Value) -> bool {
    let Some(result) = event.get("result") else {
        return false;
    };
    let payload = unwrap_payload(result);
    if payload.get("final").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let Some(state) = payload
        .get("status")
        .and_then(|status| str_field(status, "state"))
    else {
        return false;
    };
    STREAM_ENDING_STATES.contains(&normalize_task_state(state))
}

/// Whether a streamed event leaves nothing more for a caller to wait for:
/// the stream's terminal event ([`is_stream_end`]), a Message — an agent that
/// answers a streamed call with a Message rather than a Task says all it has
/// to say in that one event — or a JSON-RPC error.
///
/// Wider than [`is_stream_end`] on purpose. That one marks where a TASK
/// stream ends, for telemetry, where an error envelope is a refusal rather
/// than a stream. Anything holding events back must release them at any of
/// these, or a caller whose agent keeps the connection open waits forever.
pub fn ends_streamed_answer(event: &Value) -> bool {
    if is_stream_end(event) || event.get("error").is_some() {
        return true;
    }
    let Some(result) = event.get("result") else {
        return false;
    };
    match result.get("kind").and_then(Value::as_str) {
        Some(kind) => kind == "message",
        None => result.get("message").is_some_and(Value::is_object),
    }
}

/// What one A2A call touched, accumulated as its request and response(s) are
/// seen.
///
/// A streaming call feeds every event through [`Self::observe_result`], so the
/// recorded state is the LAST one the upstream reported — the state the task
/// was actually left in when the caller stopped watching, which is what an
/// operator auditing a task needs. A caller that walks away mid-task leaves
/// the last state it did see, not a fabricated terminal one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct A2aCallFacts {
    /// Task the call created or acted on. Empty when the call names no task
    /// (a first `message/send` whose agent answers with a bare message).
    pub task_id: String,
    /// Context (conversation) the call belongs to; empty when none was seen.
    pub context_id: String,
    /// Last task state reported, in its 0.3 spelling. Empty when no response
    /// carried one — a state is never invented for a call that failed before
    /// the upstream answered.
    ///
    /// `&'static str` rather than a `String`: the value is always one of the
    /// specification's states or `unknown`, and typing it that way is what
    /// lets it be used as a metric label without a caller having to promise
    /// it is bounded.
    pub task_state: &'static str,
}

impl A2aCallFacts {
    /// Read what the REQUEST already tells us: `message/send` and
    /// `message/stream` carry the task and context they continue on
    /// `params.message`, while the task operations name the task directly.
    ///
    /// Called before the upstream is contacted, so a call that fails outright
    /// still records which task the caller was asking about.
    pub fn observe_request(&mut self, request: &Value) {
        let Some(params) = request.get("params") else {
            return;
        };
        // A send / stream continues a task from its message; the
        // push-notification operations name it as `params.taskId`. Both spell
        // the field the same way in either wire version (1.0's protobuf JSON
        // renders `task_id` / `context_id` in camelCase).
        for source in [params.get("message"), Some(params)].into_iter().flatten() {
            self.set_task_id(str_field(source, "taskId"));
            self.set_context_id(str_field(source, "contextId"));
        }
        // `params.id` is a FALLBACK, never an override: it is the task for
        // `tasks/get` / `tasks/cancel` / `tasks/resubscribe` (both versions)
        // and for the 0.3 push-notification operations, but on their 1.0
        // counterparts it is the push-notification CONFIG's id sitting
        // alongside the task's own `taskId`. Applying it unconditionally would
        // file those calls under a task that does not exist.
        if self.task_id.is_empty() {
            self.set_task_id(str_field(params, "id"));
        }
    }

    /// Read a JSON-RPC response envelope — or one streamed event — for the
    /// task it concerns and the state it reports.
    ///
    /// Handles every `result` shape the protocol defines: a `Task` (ids on
    /// `id` / `contextId`, state under `status`), a `Message` (no state), and
    /// the streaming status / artifact update events (ids on `taskId`).
    pub fn observe_result(&mut self, response: &Value) {
        let Some(result) = response.get("result") else {
            return;
        };
        let payload = unwrap_payload(result);
        self.set_context_id(str_field(payload, "contextId"));
        self.set_task_id(str_field(payload, "taskId"));
        // `status` is the discriminator between a Task and a Message: every
        // Task has one and no Message does, in either wire version. Neither
        // spells a message id as `id` (both use `messageId`), so this guards
        // only against a nonstandard shape — it costs nothing and keeps a
        // message from ever being filed as a task.
        if let Some(status) = payload.get("status") {
            self.set_task_id(str_field(payload, "id"));
            if let Some(state) = str_field(status, "state") {
                self.task_state = normalize_task_state(state);
            }
        }
    }

    fn set_task_id(&mut self, value: Option<&str>) {
        if let Some(value) = value {
            self.task_id = value.to_string();
        }
    }

    fn set_context_id(&mut self, value: Option<&str>) {
        if let Some(value) = value {
            self.context_id = value.to_string();
        }
    }
}

/// The text a caller sent an agent: every text part of the request's message,
/// newline-joined.
///
/// A `Part` carries its text under `text` in both wire versions (0.3 tags the
/// part with `kind`, 1.0 uses a protobuf `oneof` whose set field has the same
/// name), so one reader serves both. File and data parts contribute nothing —
/// their bytes are not language, and a base64 blob would wreck both the token
/// estimate and any captured content it lands in.
///
/// `push` bounds the buffer because the body limit can be disabled explicitly,
/// and the result is retained for the lifetime of a streamed call.
pub fn request_text(request: &Value, push: impl Fn(&mut String, &str)) -> String {
    let mut buf = String::new();
    if let Some(message) = request.pointer("/params/message") {
        collect_part_text(message, &mut buf, &push);
    }
    buf
}

/// Rewrite every request text part through `mask`, returning how many parts
/// changed.
///
/// The exact inverse of [`request_text`]'s traversal — same pointer, same
/// `parts[].text` fields — so what a guardrail inspected is what gets
/// rewritten. Only those fields: an agent call's ids, method name and
/// metadata are protocol, not caller content, and a blanket string rewrite
/// over the JSON-RPC envelope would corrupt the call it is meant to protect.
pub fn redact_request_text(request: &mut Value, mask: impl Fn(&str) -> Option<String>) -> usize {
    let Some(parts) = request
        .pointer_mut("/params/message/parts")
        .and_then(Value::as_array_mut)
    else {
        return 0;
    };
    let mut rewritten = 0;
    for part in parts {
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        if let Some(masked) = mask(text) {
            part["text"] = Value::String(masked);
            rewritten += 1;
        }
    }
    rewritten
}

/// What a content-policy scan of one A2A call found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CallScan {
    /// Every word the call carries, newline-joined and unbounded: every
    /// object key, string and number, with file content decoded when it is
    /// text (see [`request_scan`]).
    pub text: String,
    /// The part of `text` a request's mask rewrite cannot reach: all of it
    /// but the string `text` of `params.message` parts, which
    /// [`redact_request_text`] rewrites. Always all of it for a response.
    pub unrewritable: String,
    /// Why some content could not be read, when it could not: file content
    /// that does not decode to text and is not of a binary media type, or a
    /// value of the wrong type where the protocol has text. A caller with a
    /// content policy refuses such a call rather than forward what it never
    /// read.
    pub unreadable: Option<&'static str>,
}

/// Scan a whole JSON-RPC request for a content policy.
///
/// Every object key, string and number of the envelope is read, at any depth
/// — no field is skipped for looking like protocol, because whatever reads a
/// field the scan skipped receives it unchecked. File content (a part's `raw`
/// on 1.0; a file's `bytes` / `fileWithBytes` / `file_with_bytes` on 0.3, and
/// a base64 `data:` URI in either) is decoded the way lenient real decoders
/// do — Python's `base64.b64decode`, Node's `Buffer` — and read as text;
/// content of a binary media type (`image/*`, `audio/*`, `video/*`,
/// `application/pdf`) is passed unread, and anything else that does not read
/// as text is reported in [`CallScan::unreadable`]. None of this is metered:
/// [`request_text`] is.
pub fn request_scan(request: &Value) -> CallScan {
    let mut walk = ScanWalk::default();
    walk.value(request, Pos::request_root());
    walk.done()
}

/// Scan a whole JSON-RPC response — `result`, `error`, anything else — the
/// way [`request_scan`] reads a request. Nothing in a response is rewritten.
pub fn result_scan(response: &Value) -> CallScan {
    let mut walk = ScanWalk::default();
    walk.value(response, Pos::response_root());
    walk.done()
}

/// [`request_scan`]'s text alone.
pub fn request_scan_text(request: &Value) -> String {
    request_scan(request).text
}

/// Whether `mask` would change anything the request's text rewrite cannot
/// reach ([`CallScan::unrewritable`]). Run once over the joined text rather
/// than per string, so a body of many small strings costs one pass.
pub fn request_would_mask_unrewritable(
    request: &Value,
    mask: impl Fn(&str) -> Option<String>,
) -> bool {
    let unrewritable = request_scan(request).unrewritable;
    !unrewritable.is_empty() && mask(&unrewritable).is_some_and(|m| m != unrewritable)
}

/// Whether a call asks the agent to deliver task updates by push notification.
///
/// A push notification is the agent POSTing a task's progress straight to a
/// webhook the caller names, so whatever an agent says that way never passes
/// back through the gateway. Two shapes ask for it
/// (<https://a2a-protocol.org/v0.3.0/specification/> §7.1.2 and §7.5;
/// <https://a2a-protocol.org/latest/specification/> §3.2.2 and §9.4.7):
///
/// - setting a task's push config: `tasks/pushNotificationConfig/set`, 1.0
///   `CreateTaskPushNotificationConfig`;
/// - a send carrying one in `params.configuration`: 0.3
///   `pushNotificationConfig`, 1.0 `taskPushNotificationConfig`.
///
/// Both are recognised in any spelling a lenient decoder accepts: camel or
/// snake case, with or without the `task` prefix, and the envelope's own
/// `method` / `params` / `configuration` keys in any casing — some decoders
/// bind keys case-insensitively, so `"Method"` names the same field there.
/// When several keys name the method, any of them that sets push counts. A
/// method of the push-config family that is not one of the reads (`get` /
/// `list` / `delete`) counts as a set, so a draft or vendor spelling of the
/// setter cannot pass as an unknown method. A JSON-RPC batch asks for push
/// delivery when any call in it does.
pub fn requests_push_delivery(request: &Value) -> bool {
    match request {
        Value::Array(calls) => calls.iter().any(requests_push_delivery),
        Value::Object(call) => {
            call.iter()
                .any(|(key, value)| match normalized_name(key).as_str() {
                    "method" => value.as_str().is_some_and(method_sets_push),
                    "params" => params_set_push(value),
                    _ => false,
                })
        }
        _ => false,
    }
}

/// Whether a call's `params` carry a push config: under `configuration` (any
/// non-null push key), or as a push config object directly in `params` (the
/// shape a setter takes, and older drafts' `pushNotification`). Positional
/// params — an array — are read item by item. A push key holding a plain
/// value, such as the `pushNotificationConfigId` a read or delete names, is
/// not a config.
fn params_set_push(params: &Value) -> bool {
    match params {
        Value::Array(items) => items.iter().any(params_set_push),
        Value::Object(params) => params.iter().any(|(key, value)| {
            if normalized_name(key) == "configuration" {
                return value.as_object().is_some_and(|configuration| {
                    configuration
                        .iter()
                        .any(|(key, value)| !value.is_null() && names_push_notification(key))
                });
            }
            value.is_object() && names_push_notification(key)
        }),
        _ => false,
    }
}

/// Whether a method name sets up push delivery: one of the push-config family
/// that is not a read.
fn method_sets_push(method: &str) -> bool {
    let reads_config = matches!(
        canonical_operation(method),
        "tasks/pushNotificationConfig/get"
            | "tasks/pushNotificationConfig/list"
            | "tasks/pushNotificationConfig/delete"
    );
    !reads_config && names_push_notification(method)
}

/// A name lowercased with everything but ASCII letters and digits dropped, so
/// the spellings decoders treat as one field compare equal.
///
/// Lowercased by Unicode rules first, with the two letters that case-fold to
/// an ASCII letter without lowercasing to one mapped by hand: a decoder that
/// matches keys case-insensitively may fold `ſ` (U+017F) to `s` (Go does) and
/// `ı` (U+0131) to `i` (.NET does), and the Kelvin sign (U+212A) lowercases
/// to `k` on its own. Folding more spellings together only ever makes the
/// check match more.
fn normalized_name(name: &str) -> String {
    name.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            '\u{17f}' => 's',
            '\u{131}' => 'i',
            c => c,
        })
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// Whether a method or field name is about push notifications, however it is
/// cased or separated.
fn names_push_notification(name: &str) -> bool {
    normalized_name(name).contains("pushnotification")
}

/// One chunk of a streamed artifact, as a review that reads an answer across
/// its chunks needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactChunk {
    pub artifact_id: String,
    /// The chunk continues the artifact's earlier chunks (`append: true`)
    /// rather than replacing them.
    pub append: bool,
    /// The chunk's text parts, joined as [`ResultText`] joins them.
    pub text: String,
}

/// The artifact chunk a streamed event carries, if it is an artifact update
/// (0.3 `kind: "artifact-update"`, 1.0 `artifactUpdate`).
///
/// An agent streaming an answer sends it in chunks that continue one another,
/// so a phrase can start in one event and end in the next: read one event at
/// a time, neither half is the phrase.
pub fn artifact_chunk(event: &Value) -> Option<ArtifactChunk> {
    let payload = unwrap_payload(event.get("result")?);
    let artifact = payload.get("artifact")?;
    let mut text = String::new();
    collect_part_text(artifact, &mut text, &|buf: &mut String, s: &str| {
        buf.push_str(s)
    });
    Some(ArtifactChunk {
        artifact_id: str_field(artifact, "artifactId")
            .unwrap_or_default()
            .to_string(),
        append: payload.get("append").and_then(Value::as_bool) == Some(true),
        text,
    })
}

/// Where the walk is, for the two things position decides: whether a string
/// is what the mask rewrite reaches, and whether a field holds file content.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pos {
    rewrite: Rewrite,
    part: PartPos,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rewrite {
    /// The request envelope; its `params.message.parts[].text` is rewritten.
    RequestRoot,
    Params,
    Message,
    MessageParts,
    MessagePart,
    /// Nothing below here is rewritten.
    None,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartPos {
    No,
    /// A `parts` list: each element is a part.
    PartsList,
    Part,
    /// A 0.3 part's `file` object.
    File,
}

impl Pos {
    fn request_root() -> Self {
        Pos {
            rewrite: Rewrite::RequestRoot,
            part: PartPos::No,
        }
    }
    fn response_root() -> Self {
        Pos {
            rewrite: Rewrite::None,
            part: PartPos::No,
        }
    }
    fn child(self, key: &str) -> Self {
        let rewrite = match (self.rewrite, key) {
            (Rewrite::RequestRoot, "params") => Rewrite::Params,
            (Rewrite::Params, "message") => Rewrite::Message,
            (Rewrite::Message, "parts") => Rewrite::MessageParts,
            _ => Rewrite::None,
        };
        let part = match (self.part, key) {
            (_, "parts") => PartPos::PartsList,
            (PartPos::Part, "file") => PartPos::File,
            _ => PartPos::No,
        };
        Pos { rewrite, part }
    }
    fn item(self) -> Self {
        Pos {
            rewrite: if self.rewrite == Rewrite::MessageParts {
                Rewrite::MessagePart
            } else {
                Rewrite::None
            },
            part: if self.part == PartPos::PartsList {
                PartPos::Part
            } else {
                PartPos::No
            },
        }
    }
}

#[derive(Default)]
struct ScanWalk {
    text: String,
    unrewritable: String,
    unreadable: Option<&'static str>,
}

impl ScanWalk {
    fn done(self) -> CallScan {
        CallScan {
            text: self.text,
            unrewritable: self.unrewritable,
            unreadable: self.unreadable,
        }
    }

    /// Read `s`; `rewritten` = the mask rewrite reaches it.
    fn push(&mut self, s: &str, rewritten: bool) {
        push_line(&mut self.text, s);
        if !rewritten {
            push_line(&mut self.unrewritable, s);
        }
    }

    fn unreadable(&mut self, why: &'static str) {
        self.unreadable.get_or_insert(why);
    }

    fn value(&mut self, value: &Value, pos: Pos) {
        match value {
            Value::String(s) => self.push(s, false),
            Value::Number(n) => self.push(&n.to_string(), false),
            Value::Array(items) => {
                for item in items {
                    self.value(item, pos.item());
                }
            }
            Value::Object(map) => {
                let media = media_type(map);
                for (key, child) in map {
                    self.push(key, false);
                    self.field(key, child, pos, media.as_deref());
                }
            }
            Value::Bool(_) | Value::Null => {}
        }
    }

    fn field(&mut self, key: &str, child: &Value, pos: Pos, media: Option<&str>) {
        let file_bytes = (pos.part == PartPos::Part && key == "raw")
            || (pos.part == PartPos::File
                && matches!(key, "bytes" | "fileWithBytes" | "file_with_bytes"));
        let file_uri = (pos.part == PartPos::Part && key == "url")
            || (pos.part == PartPos::File
                && matches!(key, "uri" | "fileWithUri" | "file_with_uri"));
        if pos.part == PartPos::Part && key == "text" {
            match child {
                Value::String(s) => self.push(s, pos.rewrite == Rewrite::MessagePart),
                other => {
                    self.unreadable("a text part whose text is not a string");
                    self.value(other, pos.child(key));
                }
            }
        } else if file_bytes {
            match child {
                Value::String(encoded) => self.file_content(encoded, media),
                other => {
                    self.unreadable("file content that is not a base64 string");
                    self.value(other, pos.child(key));
                }
            }
        } else if let (true, Value::String(uri)) = (file_uri, child) {
            self.push(uri, false);
            if let Some((uri_media, payload)) = base64_data_uri(uri) {
                let _ = self.file_content_decoded(payload, uri_media.as_deref().or(media));
            }
        } else {
            self.value(child, pos.child(key));
        }
    }

    /// A field holding base64 file content: its decoding is read when it is
    /// text, whatever media type it claims; the string itself is read too
    /// unless it turns out to be a verified binary file (it may not be base64
    /// at all, and a long base64 blob is only noise).
    fn file_content(&mut self, encoded: &str, media: Option<&str>) {
        if !self.file_content_decoded(encoded, media) {
            self.push(encoded, false);
        }
    }

    /// Returns whether the content was a verified binary file, passed unread.
    fn file_content_decoded(&mut self, encoded: &str, media: Option<&str>) -> bool {
        let Some(bytes) = lenient_base64(encoded) else {
            self.unreadable("file content that is not base64");
            return false;
        };
        // Text is read whatever the label says: a declared media type is the
        // sender's claim, and an agent may hand the bytes to a model anyway.
        if let Some(text) = bytes_as_text(&bytes) {
            self.push(&text, false);
            return false;
        }
        // Binary passes unread only when it is a real file of the family it
        // claims — its leading bytes carry that family's signature.
        if is_binary_media(media) && has_family_signature(media, &bytes) {
            return true;
        }
        self.unreadable("file content that is neither text nor a verified binary file");
        false
    }
}

/// The media type an object declares for its content, lowercased, without
/// parameters. Spelled under more than one alias with different values, it
/// declares nothing: which one a reader honours is not the scan's to guess.
fn media_type(map: &serde_json::Map<String, Value>) -> Option<String> {
    let mut declared = ["mimeType", "mediaType", "mime_type", "media_type"]
        .iter()
        .filter_map(|k| map.get(*k).and_then(Value::as_str))
        .map(|m| m.split(';').next().unwrap_or(m).trim().to_ascii_lowercase());
    let first = declared.next()?;
    declared.all(|other| other == first).then_some(first)
}

/// Whether `bytes` begin with a file signature of the family `media` claims:
/// any image format for `image/*`, any audio or video container for
/// `audio/*` and `video/*`, `%PDF` for `application/pdf`. The family, not
/// the exact subtype, is what matters here — the question is only whether
/// the content is really a binary file nothing would read as text.
fn has_family_signature(media: Option<&str>, bytes: &[u8]) -> bool {
    let at = |offset: usize, sig: &[u8]| bytes.get(offset..offset + sig.len()) == Some(sig);
    let iso_bmff = at(4, b"ftyp");
    let riff = |kind: &[u8]| at(0, b"RIFF") && at(8, kind);
    let ebml = at(0, &[0x1a, 0x45, 0xdf, 0xa3]);
    let image = at(0, &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
        || at(0, &[0xff, 0xd8, 0xff])
        || at(0, b"GIF87a")
        || at(0, b"GIF89a")
        || riff(b"WEBP")
        || at(0, b"BM")
        || at(0, b"II*\0")
        || at(0, b"MM\0*")
        || at(0, &[0x00, 0x00, 0x01, 0x00])
        || iso_bmff;
    let audio = at(0, b"ID3")
        || bytes.first() == Some(&0xff) && bytes.get(1).is_some_and(|b| b & 0xe0 == 0xe0)
        || riff(b"WAVE")
        || at(0, b"OggS")
        || at(0, b"fLaC")
        || ebml
        || iso_bmff;
    let video = iso_bmff || ebml || riff(b"AVI ") || at(0, b"OggS");
    match media.map(|m| m.split('/').next().unwrap_or(m)) {
        Some("image") => image,
        Some("audio") => audio,
        Some("video") => video,
        Some("application") => media == Some("application/pdf") && at(0, b"%PDF-"),
        _ => false,
    }
}

/// Content of these media types is not language and passes unread.
fn is_binary_media(media: Option<&str>) -> bool {
    media.is_some_and(|m| {
        m.starts_with("image/")
            || m.starts_with("audio/")
            || m.starts_with("video/")
            || m == "application/pdf"
    })
}

/// `data:[<media type>][;base64],<payload>` with a base64 payload: its media
/// type (lowercased, no parameters) and the payload.
fn base64_data_uri(uri: &str) -> Option<(Option<String>, &str)> {
    let rest = uri.strip_prefix("data:").or_else(|| {
        uri.get(..5)
            .filter(|p| p.eq_ignore_ascii_case("data:"))
            .map(|_| &uri[5..])
    })?;
    let (meta, payload) = rest.split_once(',')?;
    let mut meta_parts = meta.split(';');
    let media = meta_parts
        .next()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_ascii_lowercase);
    meta_parts
        .any(|p| p.trim().eq_ignore_ascii_case("base64"))
        .then_some((media, payload))
}

/// Decode base64 the way lenient decoders do: anything outside the alphabet
/// (line breaks, spaces) is ignored, both alphabets are accepted, padding is
/// optional and may separate concatenated chunks, and stray trailing bits are
/// allowed. `None` when nothing decodable remains.
fn lenient_base64(encoded: &str) -> Option<Vec<u8>> {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    use base64::Engine as _;
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let mut out = Vec::new();
    let mut decoded_any = false;
    for chunk in encoded.split('=') {
        let mut clean: String = chunk
            .chars()
            .filter_map(|c| match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' => Some(c),
                '-' => Some('+'),
                '_' => Some('/'),
                _ => None,
            })
            .collect();
        // A lone leftover character carries no whole byte.
        if clean.len() % 4 == 1 {
            clean.pop();
        }
        if clean.is_empty() {
            continue;
        }
        out.extend(LENIENT.decode(clean.as_bytes()).ok()?);
        decoded_any = true;
    }
    decoded_any.then_some(out)
}

/// Decoded file bytes as text, when they are text: UTF-16 behind a byte-order
/// mark, else UTF-8, read lossily so one stray byte (Latin-1, a broken
/// sequence) does not hide the rest. Bytes that are mostly replacement or
/// control characters are binary, and `None`. Control characters in text are
/// read as spaces.
fn bytes_as_text(bytes: &[u8]) -> Option<String> {
    let text = match bytes {
        [0xff, 0xfe, rest @ ..] => {
            let units: Vec<u16> = rest
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        [0xfe, 0xff, rest @ ..] => {
            let units: Vec<u16> = rest
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        [0xef, 0xbb, 0xbf, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    };
    let total = text.chars().count();
    if total == 0 {
        return Some(String::new());
    }
    let noise = text
        .chars()
        .filter(|&c| {
            c == char::REPLACEMENT_CHARACTER || (c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        })
        .count();
    // Text has at most the odd stray byte or control character: one in
    // twenty, or a couple in a short text. A binary blob is noise throughout.
    (noise <= (total / 20).max(2)).then(|| {
        text.chars()
            .map(|c| {
                if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    c
                }
            })
            .collect()
    })
}

/// The text an agent produced on one call, kept as two segments because the
/// protocol updates them by two different rules.
///
/// Artifacts are the answer, delivered in chunks that may continue one
/// another. Statements — a Message, or the message an agent attaches to a
/// status update — are what it says ABOUT the task: "still working", "report
/// generated". Keeping them apart is not tidiness. A single buffer forces one
/// rule on both, and either choice is wrong: appending multiplies an answer
/// that the agent resends as the task progresses, while replacing lets a
/// one-line progress note wipe a report that took a thousand chunks to build.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResultText {
    /// The artifact currently being streamed. `append` is scoped to one
    /// artifact id by the specification ("appended to a previously sent
    /// artifact with the same ID"), so a chunk for a DIFFERENT artifact opens
    /// a new segment rather than discarding what came before.
    artifact_id: String,
    /// The answer so far.
    artifacts: String,
    /// The agent's latest word about the task. Only ever the latest: a
    /// progress note supersedes the one before it and restates nothing.
    statement: String,
}

impl ResultText {
    /// Read one response envelope — or one streamed event — for the words it
    /// carries.
    ///
    /// `push` bounds each segment; an A2A task may stream for hours, and past
    /// the bound the text becomes a prefix rather than the buffer growing
    /// without limit.
    pub fn observe(&mut self, response: &Value, push: impl Fn(&mut String, &str)) {
        let Some(result) = response.get("result") else {
            return;
        };
        let payload = unwrap_payload(result);

        // An artifact update: one chunk of the answer, appended or standalone
        // per the event's own flag, scoped to the artifact it names.
        if let Some(artifact) = payload.get("artifact") {
            let mut fresh = String::new();
            collect_part_text(artifact, &mut fresh, &push);
            if fresh.is_empty() {
                return;
            }
            let id = str_field(artifact, "artifactId").unwrap_or_default();
            if id == self.artifact_id {
                // The same artifact again. Without `append` it is a
                // replacement of itself, not a continuation.
                if payload.get("append").and_then(Value::as_bool) != Some(true) {
                    self.artifacts.clear();
                }
            } else {
                id.clone_into(&mut self.artifact_id);
                if !self.artifacts.is_empty() {
                    push(&mut self.artifacts, "\n");
                }
            }
            push(&mut self.artifacts, &fresh);
            return;
        }

        // A Task snapshot carries the complete artifact set, so it replaces
        // that segment outright — it restates the chunks rather than adding
        // to them.
        if let Some(artifacts) = payload.get("artifacts").and_then(Value::as_array) {
            let mut fresh = String::new();
            for artifact in artifacts {
                collect_part_text(artifact, &mut fresh, &push);
            }
            if !fresh.is_empty() {
                self.artifacts.clear();
                self.artifact_id.clear();
                push(&mut self.artifacts, &fresh);
            }
        }

        // ...and whatever the agent says about the task replaces only the
        // previous such statement.
        let mut fresh = String::new();
        collect_part_text(payload, &mut fresh, &push);
        if let Some(message) = payload.pointer("/status/message") {
            collect_part_text(message, &mut fresh, &push);
        }
        if !fresh.is_empty() {
            self.statement.clear();
            push(&mut self.statement, &fresh);
        }
    }

    /// Everything the agent produced, for counting or capture. Empty when it
    /// produced nothing.
    pub fn joined(&self) -> String {
        match (self.artifacts.is_empty(), self.statement.is_empty()) {
            (true, _) => self.statement.clone(),
            (_, true) => self.artifacts.clone(),
            _ => format!("{}\n{}", self.artifacts, self.statement),
        }
    }
}

/// Every word an agent returns on one call, for a content policy to read:
/// every string the response's `result` carries — a Message's or the status
/// message's parts, every artifact (name and description too), every
/// `history` message, each Task of a `ListTasks` page, data parts, metadata,
/// a text file's decoded bytes — every key and string (see
/// [`collect_call_strings`]).
///
/// The metered reading ([`ResultText`]) is narrower on purpose: it counts an
/// answer, not a document. A policy has to read what the caller actually
/// receives, in every wire version and every shape at once, so this reads
/// without guessing either.
pub fn result_scan_text(response: &Value) -> String {
    result_scan(response).text
}

/// Append every text part of a parts-bearing object (a message or an
/// artifact), through the caller's bounding `push`.
fn collect_part_text(owner: &Value, buf: &mut String, push: &impl Fn(&mut String, &str)) {
    let Some(parts) = owner.get("parts").and_then(Value::as_array) else {
        return;
    };
    for part in parts {
        if let Some(text) = str_field(part, "text") {
            if !buf.is_empty() {
                push(buf, "\n");
            }
            push(buf, text);
        }
    }
}

/// Newline-separated append of a non-empty segment.
fn push_line(buf: &mut String, s: &str) {
    if s.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(s);
}

/// The 1.0 payload `oneof` field names, in `SendMessageResponse` /
/// `StreamResponse` order.
const V1_PAYLOAD_KEYS: [&str; 4] = ["task", "message", "statusUpdate", "artifactUpdate"];

/// Reach the Task / Message / update event a `result` carries.
///
/// 0.3 puts the object directly at `result`, tagged with `kind`. 1.0's
/// `SendMessage` and streaming responses instead wrap it in a `oneof payload`,
/// which protobuf JSON renders as the set field's own name — so the same task
/// arrives as `{"task": {…}}` there. Reading only the 0.3 shape found nothing
/// at all on 1.0, which is the DEFAULT version for a registered agent.
///
/// A `kind` settles it as 0.3 outright; no 1.0 payload has one. Anything else
/// is returned untouched, which is also right for the 1.0 operations that
/// answer with a bare `Task` (`GetTask`, `CancelTask` define no wrapper).
fn unwrap_payload(result: &Value) -> &Value {
    if result.get("kind").is_some() {
        return result;
    }
    V1_PAYLOAD_KEYS
        .iter()
        .find_map(|key| result.get(*key).filter(|value| value.is_object()))
        .unwrap_or(result)
}

/// A non-empty string field, or `None` — an absent field and an empty one are
/// the same "nothing was said" to a caller accumulating facts, and an empty
/// string must never overwrite an id read earlier in the call.
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every phrase in `expected` is in the scanned `text`.
    fn reads(text: &str, expected: &[&str]) {
        for phrase in expected {
            assert!(text.contains(phrase), "{phrase:?} unread in {text:?}");
        }
    }

    /// Every string a call carries is read, whatever field holds it: metadata,
    /// an artifact's name and description, identifiers too — the scan must
    /// not pick fields, or it disagrees with whatever reads the field it
    /// skipped. A file part's base64 bytes are decoded and read when they are
    /// text; binary bytes are not language.
    #[test]
    fn the_scans_read_every_string_and_text_file_bytes() {
        let b64 = |s: &str| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(s)
        };
        let request = json!({"params": {
            "message": {"messageId": "PHRASE-ID", "role": "user",
                        "parts": [{"text": "clean", "metadata": {"note": "PHRASE-E"}},
                                  {"kind": "file", "file": {"bytes": b64("PHRASE-T03"), "mimeType": "text/plain"}},
                                  {"raw": b64("PHRASE-T10"), "mediaType": "text/plain"}],
                        "metadata": {"PHRASE-F": "x"}},
            "metadata": {"tag": "PHRASE-G"}
        }});
        let scanned = request_scan_text(&request);
        for phrase in [
            "PHRASE-E",
            "PHRASE-F",
            "PHRASE-G",
            "PHRASE-ID",
            "PHRASE-T03",
            "PHRASE-T10",
        ] {
            assert!(scanned.contains(phrase), "{phrase} unread: {scanned}");
        }

        let binary = {
            use base64::Engine as _;
            // A real PNG: a verified binary file passes unread.
            base64::engine::general_purpose::STANDARD.encode([
                0x89_u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x9c, 0xff, 0x00,
            ])
        };
        let result = json!({"result": {"kind": "task", "id": "t", "contextId": "c",
            "artifacts": [{"artifactId": "a", "name": "PHRASE-H", "description": "PHRASE-I",
                           "parts": [{"kind": "file", "file": {"bytes": b64("PHRASE-J"), "mimeType": "text/plain"}},
                                     {"kind": "file", "file": {"bytes": binary.clone(), "mimeType": "image/png"}}]}],
            "metadata": {"summary": "PHRASE-K"}}});
        let scanned = result_scan_text(&result);
        for phrase in ["PHRASE-H", "PHRASE-I", "PHRASE-J", "PHRASE-K"] {
            assert!(scanned.contains(phrase), "{phrase} unread: {scanned}");
        }
        assert!(!scanned.contains(&binary), "base64 itself is not scanned");

        // Only `params.message`'s text parts are rewritten: a mask anywhere
        // else — a text file's bytes included — is reported.
        let mask = |s: &str| s.contains("a@b.c").then(|| s.replace("a@b.c", "[M]"));
        let in_metadata = json!({"params": {"message": {"parts": [{"text": "clean"}],
                                 "metadata": {"contact": "a@b.c"}}}});
        assert!(request_would_mask_unrewritable(&in_metadata, mask));
        let in_file = json!({"params": {"message": {"parts": [{"raw": b64("mail a@b.c"), "mediaType": "text/plain"}]}}});
        assert!(request_would_mask_unrewritable(&in_file, mask));
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// Real decoders are lenient (Python's `b64decode`, Node's `Buffer`): the
    /// scan must read what they read, or the difference is a way past it.
    #[test]
    fn file_content_is_decoded_as_leniently_as_real_agents_do() {
        let part = |bytes: Value| {
            json!({"params": {"message": {"parts": [
            {"kind": "file", "file": {"mimeType": "text/plain", "bytes": bytes}}]}}})
        };
        let phrase = b"bad phrase here";
        let encoded = b64(phrase);
        // MIME line breaks inside the base64.
        let wrapped = format!("{}\n{}", &encoded[..8], &encoded[8..]);
        // URL-safe alphabet, no padding.
        let urlsafe = encoded
            .replace('+', "-")
            .replace('/', "_")
            .trim_end_matches('=')
            .to_string();
        for variant in [encoded.clone(), wrapped, urlsafe] {
            reads(
                &request_scan_text(&part(Value::String(variant))),
                &["bad phrase here"],
            );
        }
        // A form feed, or a Latin-1 byte, does not make text binary.
        reads(
            &request_scan_text(&part(Value::String(b64(b"\x0cbad phrase here")))),
            &["bad phrase here"],
        );
        reads(
            &request_scan_text(&part(Value::String(b64(b"caf\xe9 bad phrase here")))),
            &["bad phrase here"],
        );
        // UTF-16 behind a byte-order mark.
        let utf16: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain("bad phrase here".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        reads(
            &request_scan_text(&part(Value::String(b64(&utf16)))),
            &["bad phrase here"],
        );
        // A base64 `data:` URI in a 1.0 part's `url`.
        let uri = format!("data:text/plain;base64,{}", b64(phrase));
        reads(
            &request_scan_text(&json!({"params": {"message": {"parts": [{"url": uri}]}}})),
            &["bad phrase here"],
        );
    }

    #[test]
    fn plain_text_and_numbers_are_read_wherever_they_sit() {
        // A key named like file content outside a file part is just a field.
        let scan = request_scan(&json!({"params": {"message": {"parts": [
            {"kind": "data", "data": {"raw": "bad phrase", "card": 4111111111111111u64}}]},
            "metadata": {"bytes": "alice@corp.example"}}}));
        reads(
            &scan.text,
            &["bad phrase", "4111111111111111", "alice@corp.example"],
        );
        assert_eq!(scan.unreadable, None);
    }

    /// What the scan cannot read is reported, so a content policy can refuse
    /// it instead of forwarding it unread; binary media pass unread.
    #[test]
    fn content_the_scan_cannot_read_is_reported() {
        let file = |file: Value| json!({"params": {"message": {"parts": [{"kind": "file", "file": file}]}}});
        // Not text, and not a binary media type.
        let blob = b64(&[0x00, 0x9c, 0xff, 0x13, 0x02, 0x07, 0x81, 0x00]);
        assert!(request_scan(&file(
            json!({"mimeType": "application/octet-stream", "bytes": blob})
        ))
        .unreadable
        .is_some());
        assert!(request_scan(&file(json!({"bytes": blob})))
            .unreadable
            .is_some());
        // A binary media type passes unread, and is not reported.
        let real_png = b64(&[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x9c, 0xff, 0x00,
        ]);
        let png = request_scan(&file(json!({"mimeType": "image/png", "bytes": real_png})));
        assert_eq!(png.unreadable, None);
        assert!(!png.text.contains(&real_png));
        // The same noise merely labelled as an image is not trusted.
        assert!(
            request_scan(&file(json!({"mimeType": "image/png", "bytes": blob})))
                .unreadable
                .is_some()
        );
        // A byte array where base64 belongs (Node's `Buffer` would decode it).
        assert!(request_scan(&file(
            json!({"mimeType": "text/plain", "bytes": [98, 97, 100]})
        ))
        .unreadable
        .is_some());
        // A text part whose text is not a string.
        let odd = request_scan(
            &json!({"params": {"message": {"parts": [{"kind": "text", "text": ["alice@corp.example"]}]}}}),
        );
        assert!(odd.unreadable.is_some());
        assert!(
            odd.unrewritable.contains("alice@corp.example"),
            "a non-string text is not rewritten"
        );
        // The same reading applies to an answer.
        assert!(result_scan(&json!({"result": {"kind": "message", "parts": [
            {"kind": "file", "file": {"bytes": blob}}]}}))
        .unreadable
        .is_some());
    }

    /// A declared media type is the sender's claim, not a fact: content passes
    /// unread only when it is really binary and really of that family.
    #[test]
    fn a_binary_label_does_not_hide_text_or_unverified_bytes() {
        let file = |media: &str, bytes: &[u8]| {
            json!({"params": {"message": {"parts": [
                {"kind": "file", "file": {"mimeType": media, "bytes": b64(bytes)}}]}}})
        };
        // Text labelled as an image is read like any text.
        let disguised = request_scan(&file("image/png", b"bad phrase dressed as a picture"));
        reads(&disguised.text, &["bad phrase dressed as a picture"]);
        // Noise labelled as an image, without the family's signature, is not
        // trusted.
        let noise = [0x13, 0x9c, 0xff, 0x00, 0x02, 0x07, 0x81, 0x00, 0xfe, 0x01];
        assert!(request_scan(&file("image/png", &noise))
            .unreadable
            .is_some());
        // A real image passes — of whichever image format.
        let png = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
        ];
        assert_eq!(request_scan(&file("image/png", &png)).unreadable, None);
        assert_eq!(request_scan(&file("image/jpeg", &png)).unreadable, None);
        let pdf = [
            b"%PDF-1.7\n".as_slice(),
            &[0xe2, 0xe3, 0xcf, 0xd3, 0x00, 0x9c, 0xff, 0x01],
        ]
        .concat();
        assert_eq!(
            request_scan(&file("application/pdf", &pdf)).unreadable,
            None
        );
        // An image signature does not pass as a PDF.
        assert!(request_scan(&file("application/pdf", &png))
            .unreadable
            .is_some());
        // Conflicting media-type aliases: the binary claim is not taken.
        let conflicting = json!({"params": {"message": {"parts": [{"kind": "file", "file": {
            "mimeType": "image/png", "mediaType": "text/plain", "bytes": b64(&noise)}}]}}});
        assert!(request_scan(&conflicting).unreadable.is_some());
    }

    #[test]
    fn only_the_rewritten_text_is_left_out_of_the_unrewritable_reading() {
        let scan = request_scan(
            &json!({"jsonrpc": "2.0", "id": "rq-1", "method": "message/send",
            "params": {"message": {"parts": [{"kind": "text", "text": "REWRITTEN"}],
                                   "metadata": {"note": "KEPT"}}}}),
        );
        reads(&scan.text, &["REWRITTEN", "KEPT", "rq-1"]);
        assert!(!scan.unrewritable.contains("REWRITTEN"));
        reads(&scan.unrewritable, &["KEPT", "rq-1"]);
        // A batch body has no `params.message`: nothing in it is rewritten.
        let batch =
            request_scan(&json!([{"params": {"message": {"parts": [{"text": "IN A BATCH"}]}}}]));
        reads(&batch.unrewritable, &["IN A BATCH"]);
    }

    /// A result shaped two ways at once must be read every way: a client may
    /// take either reading, so the scan cannot pick one.
    #[test]
    fn the_result_scan_reads_every_wire_shape_at_once() {
        // 0.3 `kind` at the top, and a 1.0 `task` wrapper beside it.
        let both = json!({"result": {
            "kind": "task", "id": "t",
            "status": {"state": "completed", "message": {"parts": [{"kind": "text", "text": "clean"}]}},
            "task": {"id": "t", "status": {"message": {"parts": [{"text": "PHRASE-A"}]}}}
        }});
        assert!(result_scan_text(&both).contains("PHRASE-A"));
        // Two 1.0 wrappers.
        let two = json!({"result": {
            "task": {"id": "t", "status": {"message": {"parts": [{"text": "clean"}]}}},
            "message": {"parts": [{"text": "PHRASE-B"}]}
        }});
        assert!(result_scan_text(&two).contains("PHRASE-B"));
        // A parts list under a wrapper no version defines.
        let odd =
            json!({"result": {"envelope": {"inner": [{"parts": [{"data": {"PHRASE-C": 1}}]}]}}});
        assert!(result_scan_text(&odd).contains("PHRASE-C"));
    }

    #[test]
    fn the_request_scan_reads_every_message_under_params() {
        let request = json!({"params": {
            "message": {"parts": [{"text": "clean"}]},
            "request": {"message": {"parts": [{"text": "PHRASE-D"}]}}
        }});
        assert!(request_scan_text(&request).contains("PHRASE-D"));
    }

    /// Only the text parts of `params.message` are rewritten; a mask that
    /// would change anything else the scan reads must be reported.
    #[test]
    fn a_mask_outside_the_rewritten_text_is_reported() {
        let mask = |s: &str| s.contains("a@b.c").then(|| s.replace("a@b.c", "[M]"));
        let rewritten = json!({"params": {"message": {"parts": [{"text": "mail a@b.c"}]}}});
        assert!(!request_would_mask_unrewritable(&rewritten, mask));
        let elsewhere = json!({"params": {
            "message": {"parts": [{"text": "clean"}]},
            "request": {"message": {"parts": [{"text": "mail a@b.c"}]}}
        }});
        assert!(request_would_mask_unrewritable(&elsewhere, mask));
        let data = json!({"params": {"message": {"parts": [{"data": {"to": "a@b.c"}}]}}});
        assert!(request_would_mask_unrewritable(&data, mask));
    }

    #[test]
    fn both_wire_vocabularies_map_to_one_operation() {
        // The whole point: a gateway fronting a 0.3 agent and a 1.0 agent must
        // aggregate their identical operations as one, not as two.
        for (v03, v10) in [
            ("message/send", "SendMessage"),
            ("message/stream", "SendStreamingMessage"),
            ("tasks/get", "GetTask"),
            ("tasks/cancel", "CancelTask"),
            ("tasks/resubscribe", "SubscribeToTask"),
            (
                "tasks/pushNotificationConfig/set",
                "CreateTaskPushNotificationConfig",
            ),
            (
                "tasks/pushNotificationConfig/get",
                "GetTaskPushNotificationConfig",
            ),
            (
                "tasks/pushNotificationConfig/list",
                "ListTaskPushNotificationConfigs",
            ),
            (
                "tasks/pushNotificationConfig/delete",
                "DeleteTaskPushNotificationConfig",
            ),
            ("agent/getAuthenticatedExtendedCard", "GetExtendedAgentCard"),
        ] {
            assert_eq!(canonical_operation(v03), v03, "0.3 name is the canonical");
            assert_eq!(
                canonical_operation(v10),
                v03,
                "{v10} must canonicalise to {v03}"
            );
        }
    }

    #[test]
    fn a_1_0_only_operation_has_no_0_3_spelling_to_accept() {
        // `ListTasks` was added in 1.0, so `tasks/list` is a name this
        // gateway's own vocabulary uses — not a method any version defines.
        // Accepting it on the wire would invent a 0.3 method.
        assert_eq!(canonical_operation("ListTasks"), "tasks/list");
        assert_eq!(canonical_operation("tasks/list"), UNKNOWN_OPERATION);
    }

    #[test]
    fn an_unrecognised_method_is_bounded() {
        // A caller picks the method, so this is the cardinality gate.
        for method in ["", "message/sendx", "../../etc/passwd", "SendMessage "] {
            assert_eq!(canonical_operation(method), UNKNOWN_OPERATION);
        }
    }

    #[test]
    fn streaming_is_decided_on_the_canonical_operation() {
        for method in [
            "message/stream",
            "SendStreamingMessage",
            "tasks/resubscribe",
            "SubscribeToTask",
        ] {
            assert!(is_streaming_operation(canonical_operation(method)));
        }
        for method in ["message/send", "SendMessage", "tasks/get", "GetTask", ""] {
            assert!(!is_streaming_operation(canonical_operation(method)));
        }
    }

    #[test]
    fn task_states_normalise_across_versions() {
        for (v10, v03) in [
            ("TASK_STATE_SUBMITTED", "submitted"),
            ("TASK_STATE_WORKING", "working"),
            ("TASK_STATE_INPUT_REQUIRED", "input-required"),
            ("TASK_STATE_COMPLETED", "completed"),
            ("TASK_STATE_CANCELED", "canceled"),
            ("TASK_STATE_FAILED", "failed"),
            ("TASK_STATE_REJECTED", "rejected"),
            ("TASK_STATE_AUTH_REQUIRED", "auth-required"),
        ] {
            assert_eq!(normalize_task_state(v10), v03);
            assert_eq!(normalize_task_state(v03), v03);
        }
        // Unspecified and anything an upstream invents are bounded, so a
        // task-state metric cannot be blown up from the far side.
        for state in ["TASK_STATE_UNSPECIFIED", "", "wat", "unknown"] {
            assert_eq!(normalize_task_state(state), UNKNOWN_TASK_STATE);
        }
    }

    #[test]
    fn a_send_that_continues_a_task_is_read_from_the_request() {
        // Recorded before the upstream is contacted, so a call that fails
        // outright still says which task the caller was asking about.
        let mut facts = A2aCallFacts::default();
        facts.observe_request(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "message/send",
            "params": {"message": {"taskId": "t-1", "contextId": "c-1", "role": "user"}}
        }));
        assert_eq!(facts.task_id, "t-1");
        assert_eq!(facts.context_id, "c-1");
        assert_eq!(facts.task_state, "");
    }

    #[test]
    fn task_operations_name_their_subject_in_either_version() {
        // `GetTaskRequest` / `CancelTaskRequest` / `SubscribeToTaskRequest`
        // all carry the task as `id` in both wire versions.
        for method in ["tasks/get", "GetTask", "tasks/cancel", "tasks/resubscribe"] {
            let mut facts = A2aCallFacts::default();
            facts.observe_request(&json!({"method": method, "params": {"id": "t-7"}}));
            assert_eq!(facts.task_id, "t-7", "{method} names its task");
        }

        // The push-notification operations name the parent task instead — and
        // in 1.0 they carry the CONFIG's id in `params.id` right next to it.
        // Reading `id` last would file the call under a task that does not
        // exist, so an explicit `taskId` has to win.
        let mut v10_cfg = A2aCallFacts::default();
        v10_cfg.observe_request(&json!({
            "method": "GetTaskPushNotificationConfig",
            "params": {"taskId": "t-8", "id": "cfg-9"}
        }));
        assert_eq!(v10_cfg.task_id, "t-8");

        // 0.3 spells the same request with the TASK in `id`, so the fallback
        // still has to fire when no `taskId` is present.
        let mut v03_cfg = A2aCallFacts::default();
        v03_cfg.observe_request(&json!({
            "method": "tasks/pushNotificationConfig/get",
            "params": {"id": "t-8", "pushNotificationConfigId": "cfg-9"}
        }));
        assert_eq!(v03_cfg.task_id, "t-8");
    }

    #[test]
    fn a_task_result_yields_the_task_and_its_state() {
        let mut facts = A2aCallFacts::default();
        facts.observe_result(&json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "kind": "task", "id": "t-2", "contextId": "c-2",
                "status": {"state": "working"}
            }
        }));
        assert_eq!(facts.task_id, "t-2");
        assert_eq!(facts.context_id, "c-2");
        assert_eq!(facts.task_state, "working");
    }

    #[test]
    fn a_message_result_is_not_mistaken_for_a_task() {
        // An agent may answer `message/send` with a bare Message rather than a
        // Task. It carries a context but no task and no state, and none of
        // those may be invented for it.
        let mut facts = A2aCallFacts::default();
        facts.observe_result(&json!({
            "result": {"kind": "message", "messageId": "m-1", "role": "agent", "contextId": "c-3"}
        }));
        assert_eq!(facts.task_id, "");
        assert_eq!(facts.context_id, "c-3");
        assert_eq!(facts.task_state, "");
    }

    #[test]
    fn a_1_0_result_is_read_through_its_payload_wrapper() {
        // 1.0 — the DEFAULT version for a registered agent — wraps the object
        // in the response's `oneof payload`, which protobuf JSON renders as
        // the set field's name. Reading only 0.3's flat shape found nothing at
        // all here, so every field this module exists to record came back
        // empty on the default version.
        let mut send = A2aCallFacts::default();
        send.observe_result(&json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"task": {
                "id": "t-10", "contextId": "c-10",
                "status": {"state": "TASK_STATE_COMPLETED"}
            }}
        }));
        assert_eq!(send.task_id, "t-10");
        assert_eq!(send.context_id, "c-10");
        assert_eq!(send.task_state, "completed");

        // The streaming wrapper has two more variants.
        let mut status = A2aCallFacts::default();
        status.observe_result(&json!({
            "result": {"statusUpdate": {
                "taskId": "t-11", "contextId": "c-11",
                "status": {"state": "TASK_STATE_WORKING"}
            }}
        }));
        assert_eq!(status.task_id, "t-11");
        assert_eq!(status.task_state, "working");

        let mut artifact = A2aCallFacts::default();
        artifact.observe_result(&json!({
            "result": {"artifactUpdate": {"taskId": "t-12", "contextId": "c-12"}}
        }));
        assert_eq!(artifact.task_id, "t-12");
        assert_eq!(artifact.context_id, "c-12");

        // A 1.0 Message payload still yields its context and no task.
        let mut message = A2aCallFacts::default();
        message.observe_result(&json!({
            "result": {"message": {"messageId": "m-2", "contextId": "c-13", "role": "agent"}}
        }));
        assert_eq!(message.task_id, "");
        assert_eq!(message.context_id, "c-13");
    }

    #[test]
    fn a_1_0_bare_task_result_is_read_without_a_wrapper() {
        // `GetTask` and `CancelTask` define no response wrapper — they answer
        // with the Task itself, so the unwrapping must not require one.
        let mut facts = A2aCallFacts::default();
        facts.observe_result(&json!({
            "result": {"id": "t-14", "contextId": "c-14", "status": {"state": "TASK_STATE_CANCELED"}}
        }));
        assert_eq!(facts.task_id, "t-14");
        assert_eq!(facts.context_id, "c-14");
        assert_eq!(facts.task_state, "canceled");
    }

    #[test]
    fn a_stream_records_the_last_state_the_caller_saw() {
        // Streamed status updates carry `taskId`, not `id`, and the state
        // advances event by event. The recorded state is the last one the
        // upstream reported — including when the caller walks away mid-task,
        // where inventing a terminal state would be a lie.
        let mut facts = A2aCallFacts::default();
        for state in ["submitted", "working", "input-required"] {
            facts.observe_result(&json!({
                "result": {
                    "kind": "status-update", "taskId": "t-4", "contextId": "c-4",
                    "status": {"state": state}, "final": false
                }
            }));
        }
        assert_eq!(facts.task_id, "t-4");
        assert_eq!(facts.context_id, "c-4");
        assert_eq!(facts.task_state, "input-required");
    }

    #[test]
    fn an_error_envelope_leaves_the_request_facts_standing() {
        // A JSON-RPC error carries no `result`; the task the caller named in
        // the request must survive, so a failed `tasks/get` is still
        // attributable to its task.
        let mut facts = A2aCallFacts::default();
        facts.observe_request(&json!({"method": "tasks/get", "params": {"id": "t-5"}}));
        facts.observe_result(&json!({
            "jsonrpc": "2.0", "id": 1,
            "error": {"code": -32001, "message": "task not found"}
        }));
        assert_eq!(facts.task_id, "t-5");
        assert_eq!(facts.task_state, "");
    }

    /// The bounded push the proxy passes in; unbounded here so what is under
    /// test is the append/replace rule, not the cap.
    fn push(buf: &mut String, s: &str) {
        buf.push_str(s);
    }

    fn observed(events: &[Value]) -> String {
        let mut text = ResultText::default();
        for event in events {
            text.observe(event, push);
        }
        text.joined()
    }

    fn artifact_chunk(id: &str, text: &str, append: Option<bool>) -> Value {
        let mut event = json!({"result": {"kind": "artifact-update", "taskId": "t",
            "artifact": {"artifactId": id, "parts": [{"text": text}]}}});
        if let Some(append) = append {
            event["result"]["append"] = json!(append);
        }
        event
    }

    fn status(state: &str, note: Option<&str>) -> Value {
        let mut st = json!({"state": state});
        if let Some(note) = note {
            st["message"] = json!({"role": "agent", "parts": [{"text": note}]});
        }
        json!({"result": {"kind": "status-update", "taskId": "t", "status": st}})
    }

    #[test]
    fn text_is_read_from_parts_in_both_versions() {
        // 0.3 tags each part with `kind`, 1.0 sets a protobuf oneof — both
        // put the words under `text`, and neither file nor data parts carry
        // language worth counting.
        let v03 = request_text(
            &json!({"params": {"message": {"role": "user", "parts": [
                {"kind": "text", "text": "invoice 42"},
                {"kind": "file", "file": {"bytes": "AAAA", "mimeType": "application/pdf"}},
                {"kind": "text", "text": "please summarise"}
            ]}}}),
            push,
        );
        assert_eq!(v03, "invoice 42\nplease summarise");

        let v10 = request_text(
            &json!({"params": {"message": {"role": "user", "parts": [
                {"text": "invoice 42"},
                {"raw": "AAAA", "mediaType": "application/pdf"},
                {"text": "please summarise"}
            ]}}}),
            push,
        );
        assert_eq!(v10, "invoice 42\nplease summarise");

        assert_eq!(request_text(&json!({"params": {"id": "t-1"}}), push), "");
        assert_eq!(
            request_text(
                &json!({"params": {"message": {"parts": [{"data": {"a": 1}}]}}}),
                push
            ),
            ""
        );
    }

    #[test]
    fn the_request_scan_reads_data_parts_in_both_versions() {
        // A data part reaches the agent verbatim, so a policy has to read its
        // strings and keys — nested ones included — in both spellings of the
        // part, and so does every other string the agent receives: a file's
        // name, the metadata. Numbers and booleans are not words, and binary
        // file bytes are not read.
        let v03 = request_scan_text(&json!({"params": {"message": {"role": "user", "parts": [
            {"kind": "text", "text": "file this form"},
            {"kind": "data", "data": {"form": {"reason": "the reason"}}},
            {"kind": "data", "data": {"tags": ["a-tag", 3, true, null]}},
            {"kind": "file", "file": {"bytes": "AAAA", "name": "x.pdf"}}
        ], "metadata": {"trace": "not-content"}}}}));
        reads(
            &v03,
            &[
                "trace",
                "not-content",
                "file this form",
                "form",
                "reason",
                "the reason",
                "tags",
                "a-tag",
                "x.pdf",
            ],
        );
        assert!(!v03.contains('\0'), "binary bytes are not read: {v03:?}");

        // 1.0: the oneof's `data` is a `google.protobuf.Value`, so it need not
        // be an object at all.
        let v10 = request_scan_text(
            &json!({"params": {"message": {"role": "ROLE_USER", "parts": [
                {"text": "file this form"},
                {"data": {"the key itself": true}},
                {"data": ["listed", 1]},
                {"data": "a bare string"},
                {"raw": "AAAA", "mediaType": "application/pdf"}
            ]}}}),
        );
        reads(
            &v10,
            &[
                "file this form",
                "the key itself",
                "listed",
                "a bare string",
            ],
        );
        assert!(!v10.contains('\0'), "binary bytes are not read: {v10:?}");

        // A data-only message is not an empty one.
        reads(
            &request_scan_text(
                &json!({"params": {"message": {"parts": [{"kind": "data", "data": {"k": "v"}}]}}}),
            ),
            &["k", "v"],
        );
        assert_eq!(request_scan_text(&json!({})), "");
    }

    #[test]
    fn a_mask_that_would_change_a_data_part_is_reported() {
        let mask = |s: &str| {
            s.contains("alice@")
                .then(|| s.replace("alice@", "[EMAIL]@"))
        };
        let with_part = |part: Value| json!({"params": {"message": {"parts": [part]}}});

        // A value, nested or not, and a key — in both part spellings.
        for part in [
            json!({"kind": "data", "data": {"contact": "alice@example.com"}}),
            json!({"kind": "data", "data": {"rows": [{"to": ["alice@example.com"]}]}}),
            json!({"data": {"alice@example.com": true}}),
            json!({"data": "alice@example.com"}),
        ] {
            assert!(
                request_would_mask_unrewritable(&with_part(part.clone()), mask),
                "{part} must be reported"
            );
        }

        // A text part is rewritable, so it is not this function's concern; a
        // data part the mask leaves alone, or answers with the same text, is
        // nothing to refuse.
        for part in [
            json!({"kind": "text", "text": "alice@example.com"}),
            json!({"kind": "data", "data": {"contact": "bob@example.com", "n": 1}}),
        ] {
            assert!(
                !request_would_mask_unrewritable(&with_part(part.clone()), mask),
                "{part}"
            );
        }
        assert!(!request_would_mask_unrewritable(
            &with_part(json!({"data": {"k": "v"}})),
            |s: &str| Some(s.to_string())
        ));
        assert!(!request_would_mask_unrewritable(
            &json!({"params": {}}),
            mask
        ));
    }

    #[test]
    fn the_result_scan_reads_history_artifacts_and_data_in_0_3() {
        let task = json!({"jsonrpc": "2.0", "id": 1, "result": {
            "kind": "task", "id": "t", "contextId": "c",
            "status": {"state": "completed", "message": {"kind": "message", "role": "agent",
                "parts": [{"kind": "text", "text": "Done."}]}},
            "artifacts": [{"artifactId": "a", "parts": [
                {"kind": "data", "data": {"note": "in an artifact"}}
            ]}],
            "history": [
                {"kind": "message", "role": "user", "parts": [{"kind": "text", "text": "the question"}]},
                {"kind": "message", "role": "agent", "parts": [{"kind": "text", "text": "in the history"}]}
            ],
            "metadata": {"trace": "not-content"}
        }});
        let text = result_scan_text(&task);
        for expected in [
            "Done.",
            "note",
            "in an artifact",
            "the question",
            "in the history",
        ] {
            assert!(
                text.contains(expected),
                "{expected:?} missing from {text:?}"
            );
        }
        // Metadata reaches the caller too, so it is read like the rest.
        assert!(text.contains("not-content"), "{text}");

        // A bare Message answer with a data part.
        reads(
            &result_scan_text(
                &json!({"result": {"kind": "message", "role": "agent", "parts": [
                    {"kind": "text", "text": "see the table"},
                    {"kind": "data", "data": {"row": "in a message"}}
                ]}}),
            ),
            &["see the table", "row", "in a message"],
        );
    }

    #[test]
    fn the_result_scan_reads_every_1_0_shape() {
        // `SendMessage` wraps the Task, `GetTask` returns it bare, and
        // `ListTasks` returns a page of them; all three carry the same words.
        let task = json!({
            "id": "t", "contextId": "c",
            "status": {"state": "TASK_STATE_COMPLETED", "message": {"role": "ROLE_AGENT",
                "parts": [{"text": "Done."}]}},
            "artifacts": [{"artifactId": "a", "parts": [{"data": {"in a key": 1}}]}],
            "history": [{"role": "ROLE_AGENT", "parts": [{"text": "in the history"}]}]
        });
        for response in [
            json!({"result": {"task": task.clone()}}),
            json!({"result": task.clone()}),
            json!({"result": {"tasks": [{"id": "other", "status": {"state": "TASK_STATE_WORKING"}},
                                        task.clone()],
                              "nextPageToken": "", "pageSize": 2, "totalSize": 2}}),
        ] {
            let text = result_scan_text(&response);
            for expected in ["Done.", "in a key", "in the history"] {
                assert!(
                    text.contains(expected),
                    "{expected:?} missing from {response}"
                );
            }
        }
        reads(
            &result_scan_text(&json!({"result": {"message": {"role": "ROLE_AGENT",
                "parts": [{"data": {"k": "in a message"}}]}}})),
            &["k", "in a message"],
        );
        // Streamed chunks read the same way, should one be passed in.
        reads(
            &result_scan_text(&json!({"result": {"artifactUpdate": {"taskId": "t",
                "artifact": {"artifactId": "a", "parts": [{"data": {"k": "chunk"}}]}}}})),
            &["k", "chunk"],
        );
        // An error envelope reaches the caller too, and is read like a result.
        reads(
            &result_scan_text(&json!({"error": {"code": -1, "message": "error text"}})),
            &["error text"],
        );
    }

    #[test]
    fn the_metered_reading_still_leaves_history_and_data_out() {
        // Scanning reads more than metering counts, on purpose: `history`
        // restates the conversation and data parts are not language, so
        // counting either would inflate the estimate.
        let mut text = ResultText::default();
        text.observe(
            &json!({"result": {"kind": "task", "id": "t",
                "status": {"state": "completed", "message": {"parts": [{"kind": "text", "text": "Done."}]}},
                "artifacts": [{"artifactId": "a", "parts": [{"kind": "data", "data": {"k": "v"}}]}],
                "history": [{"role": "agent", "parts": [{"kind": "text", "text": "earlier"}]}]}}),
            push,
        );
        assert_eq!(text.joined(), "Done.");
    }

    #[test]
    fn a_progress_note_between_chunks_does_not_erase_the_answer() {
        // The reference pattern: artifact chunks with `update_status(working,
        // message=…)` interleaved, finished by `complete(message=…)`. A single
        // buffer replaced by every statement keeps only the closing note and
        // loses the whole report.
        let text = observed(&[
            artifact_chunk("report", "the first half", None),
            status("working", Some("still working")),
            artifact_chunk("report", " and the second", Some(true)),
            status("completed", Some("Report generated.")),
        ]);
        assert!(
            text.contains("the first half and the second"),
            "the answer survives the notes around it: {text}"
        );
        assert!(text.contains("Report generated."));
        assert!(
            !text.contains("still working"),
            "only the LAST statement is kept: {text}"
        );
    }

    #[test]
    fn append_is_scoped_to_one_artifact() {
        // "appended to a previously sent artifact with the same ID" — a chunk
        // for a different artifact opens a new segment instead of discarding
        // the one before it.
        let text = observed(&[
            artifact_chunk("summary", "the summary", None),
            artifact_chunk("table", "the table", None),
        ]);
        assert!(text.contains("the summary"), "{text}");
        assert!(text.contains("the table"), "{text}");
    }

    #[test]
    fn a_resent_artifact_replaces_itself() {
        // Without `append` a chunk for the same artifact is a replacement, so
        // an agent that resends its answer is not counted twice.
        let text = observed(&[
            artifact_chunk("report", "the answer", None),
            artifact_chunk("report", "the answer", None),
        ]);
        assert_eq!(text, "the answer");
    }

    #[test]
    fn an_appending_chunk_continues_the_previous_one() {
        let text = observed(&[
            artifact_chunk("r", "Hello", Some(false)),
            artifact_chunk("r", ", world", Some(true)),
            artifact_chunk("r", "!", Some(true)),
        ]);
        assert_eq!(text, "Hello, world!");
    }

    #[test]
    fn a_task_snapshot_restates_its_artifacts_rather_than_adding_to_them() {
        // A terminal Task carries the complete set the chunks already
        // delivered; adding it would report the answer twice.
        let text = observed(&[
            artifact_chunk("r", "the answer", None),
            json!({"result": {"kind": "task", "id": "t", "status": {"state": "completed"},
                   "artifacts": [{"artifactId": "r", "parts": [{"text": "the answer"}]}]}}),
        ]);
        assert_eq!(text, "the answer");
    }

    #[test]
    fn a_1_0_wrapped_result_yields_its_text_too() {
        // The payload wrapper has to be seen through here as well, or the
        // default wire version contributes no text at all.
        let mut message = ResultText::default();
        message.observe(
            &json!({"result": {"message": {"role": "agent", "parts": [{"text": "done"}]}}}),
            push,
        );
        assert_eq!(message.joined(), "done");

        let mut streamed = ResultText::default();
        streamed.observe(
            &json!({"result": {"statusUpdate": {"taskId": "t", "status": {
                "state": "TASK_STATE_COMPLETED",
                "message": {"parts": [{"text": "finished"}]}
            }}}}),
            push,
        );
        assert_eq!(streamed.joined(), "finished");

        let mut artifact = ResultText::default();
        artifact.observe(
            &json!({"result": {"artifactUpdate": {"taskId": "t",
                   "artifact": {"artifactId": "r", "parts": [{"text": "chunk"}]}}}}),
            push,
        );
        assert_eq!(artifact.joined(), "chunk");
    }

    #[test]
    fn an_event_with_no_text_leaves_what_came_before() {
        // A bare progress ping must not wipe the answer already collected.
        let text = observed(&[
            artifact_chunk("r", "answer", None),
            status("working", None),
            json!({"error": {"code": -1, "message": "x"}}),
        ]);
        assert_eq!(text, "answer");
    }

    #[test]
    fn the_terminal_event_is_recognised_in_both_versions() {
        // The moment a caller stops reading. 0.3 marks it with `final`; 1.0's
        // status-update has no such field, so only the task's own state says
        // so — and both spellings of that state have to work.
        for terminal in [
            json!({"result": {"kind": "status-update", "taskId": "t", "final": true,
                              "status": {"state": "input-required"}}}),
            json!({"result": {"kind": "task", "id": "t", "status": {"state": "completed"}}}),
            json!({"result": {"statusUpdate": {"taskId": "t",
                              "status": {"state": "TASK_STATE_FAILED"}}}}),
            json!({"result": {"task": {"id": "t", "status": {"state": "TASK_STATE_CANCELED"}}}}),
        ] {
            assert!(is_stream_end(&terminal), "{terminal} ends the stream");
        }
        // A task still running does not, nor does an event with no state at
        // all (an artifact chunk), nor an error envelope.
        for ongoing in [
            json!({"result": {"kind": "status-update", "taskId": "t", "final": false,
                              "status": {"state": "working"}}}),
            json!({"result": {"statusUpdate": {"taskId": "t",
                              "status": {"state": "TASK_STATE_SUBMITTED"}}}}),
            json!({"result": {"artifactUpdate": {"taskId": "t"}}}),
            json!({"error": {"code": -32000, "message": "boom"}}),
        ] {
            assert!(
                !is_stream_end(&ongoing),
                "{ongoing} does not end the stream"
            );
        }
    }

    #[test]
    fn an_empty_id_never_erases_one_already_seen() {
        let mut facts = A2aCallFacts::default();
        facts.observe_request(&json!({"method": "message/send", "params": {
            "message": {"taskId": "t-6", "contextId": "c-6"}
        }}));
        facts.observe_result(&json!({"result": {"taskId": "", "contextId": null}}));
        assert_eq!(facts.task_id, "t-6");
        assert_eq!(facts.context_id, "c-6");
    }

    #[test]
    fn push_delivery_is_recognised_in_every_spelling_that_sets_it() {
        let webhook = json!({"url": "https://hooks.example/a2a"});
        for asks in [
            json!({"method": "tasks/pushNotificationConfig/set", "params": {"taskId": "t"}}),
            json!({"method": "CreateTaskPushNotificationConfig", "params": {"taskId": "t"}}),
            // A setter spelling neither version defines: still a setter.
            json!({"method": "SetTaskPushNotificationConfig", "params": {}}),
            json!({"method": "message/send", "params": {"configuration": {
                "pushNotificationConfig": webhook}}}),
            json!({"method": "SendMessage", "params": {"configuration": {
                "taskPushNotificationConfig": webhook}}}),
            json!({"method": "message/stream", "params": {"configuration": {
                "push_notification_config": webhook}}}),
            json!({"method": "SendStreamingMessage", "params": {"configuration": {
                "task_push_notification_config": webhook}}}),
            // A decoder that matches keys case-insensitively binds these to
            // the same fields, so they ask the same thing.
            json!({"Method": "tasks/pushNotificationConfig/set", "params": {}}),
            json!({"method": "message/send", "METHOD": "CreateTaskPushNotificationConfig"}),
            json!({"method": "message/send", "Params": {"Configuration": {
                "PushNotificationConfig": webhook}}}),
            // Unicode case folding binds these too: Go folds `ſ` to `s` and
            // the Kelvin sign to `k`, .NET folds `ı` to `I`.
            json!({"method": "message/send", "param\u{17f}": {"configuration": {
                "pushNotificationConfig": webhook}}}),
            json!({"method": "message/send", "params": {"conf\u{131}guration": {
                "pu\u{17f}hNotificationConfig": webhook}}}),
            json!({"method": "tasks/pu\u{17f}hNotificationConfig/set", "params": {}}),
            json!({"method": "message/send", "params": {"configuration": {
                "tas\u{212a}PushNotificationConfig": webhook, "pu\u{17f}h": 1}}}),
            // Positional params, and a push config object directly in params.
            json!({"method": "message/send", "params": [{"configuration": {
                "pushNotificationConfig": webhook}}]}),
            json!({"method": "message/send", "params": {"message": {},
                "pushNotification": webhook}}),
            // A JSON-RPC batch asks if any call in it does.
            json!([{"method": "tasks/get", "params": {}},
                   {"method": "tasks/pushNotificationConfig/set", "params": {}}]),
            json!([{"method": "message/send", "params": {"configuration": {
                "pushNotificationConfig": webhook}}}]),
        ] {
            assert!(
                requests_push_delivery(&asks),
                "{asks} asks for push delivery"
            );
        }
        for does_not in [
            json!({"method": "tasks/pushNotificationConfig/get", "params": {"id": "t"}}),
            json!({"method": "GetTaskPushNotificationConfig", "params": {"id": "t"}}),
            json!({"method": "ListTaskPushNotificationConfigs", "params": {}}),
            json!({"method": "tasks/pushNotificationConfig/delete", "params": {}}),
            json!({"method": "message/send", "params": {"configuration": {
                "blocking": true, "pushNotificationConfig": null}}}),
            // Only the configuration asks: a mention elsewhere is content.
            json!({"method": "message/send", "params": {"message": {"metadata": {
                "pushNotificationConfig": webhook}}}}),
            json!([{"method": "message/send", "params": {}}, {"method": "tasks/get"}]),
            // Reading or deleting a config names it by id: not a config.
            json!({"method": "tasks/pushNotificationConfig/delete", "params": {
                "id": "t", "pushNotificationConfigId": "c"}}),
        ] {
            assert!(
                !requests_push_delivery(&does_not),
                "{does_not} does not ask for push delivery"
            );
        }
    }

    #[test]
    fn an_artifact_chunk_is_read_in_both_wire_versions() {
        let v03 = json!({"result": {"kind": "artifact-update", "taskId": "t", "append": true,
            "artifact": {"artifactId": "r", "parts": [{"kind": "text", "text": "half"}]}}});
        assert_eq!(
            super::artifact_chunk(&v03),
            Some(ArtifactChunk {
                artifact_id: "r".into(),
                append: true,
                text: "half".into()
            })
        );
        let v10 = json!({"result": {"artifactUpdate": {"taskId": "t",
            "artifact": {"artifactId": "r", "parts": [{"text": "one"}, {"text": "two"}]}}}});
        assert_eq!(
            super::artifact_chunk(&v10),
            Some(ArtifactChunk {
                artifact_id: "r".into(),
                append: false,
                text: "one\ntwo".into()
            })
        );
        // A status update or a whole Task is not a chunk.
        assert_eq!(
            super::artifact_chunk(&json!({"result": {"kind": "status-update", "taskId": "t"}})),
            None
        );
        assert_eq!(
            super::artifact_chunk(&json!({"result": {"task": {"id": "t", "artifacts": []}}})),
            None
        );
    }

    #[test]
    fn a_message_or_an_error_ends_a_streamed_answer() {
        for ends in [
            json!({"result": {"kind": "message", "role": "agent", "parts": []}}),
            json!({"result": {"message": {"role": "ROLE_AGENT", "parts": []}}}),
            json!({"error": {"code": -32000, "message": "no"}}),
            json!({"result": {"kind": "task", "id": "t", "status": {"state": "completed"}}}),
        ] {
            assert!(ends_streamed_answer(&ends), "{ends} ends the answer");
        }
        for goes_on in [
            json!({"result": {"kind": "status-update", "taskId": "t",
                "status": {"state": "working", "message": {"parts": []}}}}),
            json!({"result": {"statusUpdate": {"taskId": "t",
                "status": {"state": "TASK_STATE_WORKING"}}}}),
            json!({"result": {"artifactUpdate": {"taskId": "t"}}}),
        ] {
            assert!(!ends_streamed_answer(&goes_on), "{goes_on} does not end it");
        }
    }
}
