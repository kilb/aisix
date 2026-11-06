//! Platform mode: request fields that name an object stored upstream.
//!
//! In platform mode every tenant reaches an upstream through the platform's
//! own accounts. So an object that one tenant's request makes an upstream
//! store, such as a container, an uploaded file, a vector store, a
//! conversation, a stored prompt or a custom skill, lives in an account that
//! every other tenant reaches too. Whoever names its id gets to use it.
//!
//! The gateway seals the ids it hands out for the objects it relays, and
//! binds them to their tenant: response ids, output item ids and call ids
//! (`crate::responses_affinity`, `crate::opaque_id`). It binds no other kind,
//! and platform mode opens none of the APIs that create or list them, so a
//! tenant never legitimately holds such an id. A request that names one is
//! refused as an invalid request naming the field, before any upstream sees
//! it, on the native and the translated path alike. Outside platform mode
//! nothing here runs.
//!
//! The fields, from the APIs' own type definitions (openai-python 3.41.0
//! and anthropic-sdk-python 2.3.0, paths under their `types/` directory):
//!
//! - Responses (`responses/`):
//!   - `conversation`: `response_create_params.py:58`.
//!   - `prompt`, a stored prompt by id: `response_create_params.py:156` and
//!     `response_prompt_param.py:23`.
//!   - `prompt_cache_options.comparison_response_id`, a stored response:
//!     `response_create_params.py:169`, `:413`.
//!   - Every tool definition below, wherever it appears: in `tools[]`, in
//!     `tool_choice` as the definitions of an `allowed_tools` choice
//!     (`tool_choice_allowed_param.py:23`), and inside `input[]` in an
//!     `additional_tools` item (`response_input_item_param.py:222`) or a
//!     `tool_search_output` item
//!     (`response_tool_search_output_item_param_param.py:395`). The walk
//!     does not descend into a function tool's JSON Schema, a custom tool's
//!     grammar or a `tool_search_call`'s arguments
//!     (`response_input_item_param.py:203`), which the caller or the model
//!     wrote for their own purposes.
//!   - A code interpreter's `container` given as an id, or as an automatic
//!     container with uploaded `file_ids`: `tool_param.py:214` and `:230`.
//!   - A file search's `vector_store_ids`: `file_search_tool_param.py:58`.
//!   - An image generation's `input_image_mask.file_id`: `tool_param.py:256`.
//!   - An MCP tool's `tunnel_id`, a Secure MCP Tunnel of the organization:
//!     `tool_param.py:193`.
//!   - A shell tool's `environment`, given as a `container_reference`
//!     (`container_reference_param.py:11`) or as a `container_auto` with
//!     `file_ids` or `skill_reference` skills
//!     (`container_auto_param.py:25`, `:34`; `skill_reference_param.py:11`).
//!   - In `input[]`, at any depth:
//!     - the `file_id` of an `input_file` or `input_image` part
//!       (`response_input_file_param.py:42`,
//!       `response_input_image_param.py:41`, and in a tool output's list
//!       `response_input_file_content_param.py:42`,
//!       `response_input_image_content_param.py:41`);
//!     - the `file_id` of a `computer_screenshot`
//!       (`response_computer_tool_call_output_screenshot_param.py:19`);
//!     - the `container_id` of a `code_interpreter_call`
//!       (`response_code_interpreter_tool_call_param.py:43`), since an
//!       automatic container "reuses an active container that was used by a
//!       previous `code_interpreter_call` item in the model's context"
//!       (<https://developers.openai.com/api/docs/guides/tools-code-interpreter>);
//!     - a `container_reference` environment of a shell call
//!       (`response_input_item_param.py:393`).
//! - Chat Completions (`chat/`):
//!   - the `file.file_id` of a `file` content part
//!     (`chat_completion_content_part_param.py:22`);
//!   - an assistant message's `audio.id`, a stored audio response
//!     (`chat_completion_assistant_message_param.py:21`);
//!   - `audio.voice.id`, a custom voice (`chat_completion_audio_param.py:11`).
//! - Messages and count_tokens (anthropic `types/`):
//!   - `container`, given as an id or as an object naming one
//!     (`message_create_params.py:135`, `container_params.py:14`), or
//!     loading a `custom` skill (`skill_params.py:11`, `:14`);
//!   - a block of any type whose `source` is a stored file
//!     (`file_document_source_param.py:9`, `file_image_source_param.py:9`),
//!     at any depth, but never inside the `input` the model wrote for a tool
//!     use;
//!   - a `container_upload` block (`container_upload_block_param.py:17`);
//!   - `diagnostics.previous_message_id`, a stored request
//!     (`message_create_params.py:138`, `diagnostics_param.py:16`);
//!   - `thread`, server-held conversation state
//!     (`beta/message_create_params.py:271`). The client's `anthropic-beta`
//!     header is forwarded, so a beta field reaches the upstream;
//!   - `fallback_credit_token`, as a string or an object
//!     (`beta/message_create_params.py:168`, `:389`;
//!     `beta/beta_fallback_credit_token_param.py`): a server-held token that
//!     "the same organization and workspace" can redeem, which in platform
//!     mode is every tenant.
//!
//! Left alone, on purpose:
//!
//! - Ids the gateway seals and binds, already handled elsewhere:
//!   `previous_response_id`, and the item, call and approval ids of
//!   `input[]`.
//! - What an earlier answer cites and a client echoes back as history: a
//!   `file_search_call`'s results, and the file and container citations in
//!   an `output_text`'s annotations. Neither puts an object in the model's
//!   context.
//! - The `file_id`s of echoed code-execution outputs on Messages
//!   (`code_execution_output_block_param.py:9`,
//!   `bash_code_execution_output_block_param.py:9`): they live in a
//!   container, and a request cannot name that container, since `container`
//!   and `container_upload` are refused.
//! - An MCP server named by URL and credential, not by a stored id.
//! - The Messages API's workspace and user-profile selectors. They travel as
//!   headers (`anthropic-workspace-id` and `anthropic-user-profile-id`), and
//!   the gateway forwards a client header only where the operator
//!   configured it.

use serde_json::Value;

use crate::error::ProxyError;

/// Refuse a `/v1/responses` request naming an object stored upstream.
pub(crate) fn refuse_in_responses(
    platform: Option<&crate::platform::PlatformSettings>,
    body: &Value,
) -> Result<(), ProxyError> {
    refuse(platform, || responses_reference(body))
}

/// Refuse a `/v1/chat/completions` request naming an object stored upstream.
pub(crate) fn refuse_in_chat(
    platform: Option<&crate::platform::PlatformSettings>,
    req: &aisix_gateway::ChatFormat,
) -> Result<(), ProxyError> {
    refuse(platform, || chat_reference(req))
}

/// Refuse a `/v1/messages` or `/v1/messages/count_tokens` request naming an
/// object stored upstream.
pub(crate) fn refuse_in_messages(
    platform: Option<&crate::platform::PlatformSettings>,
    body: &Value,
) -> Result<(), ProxyError> {
    refuse(platform, || messages_reference(body))
}

fn refuse(
    platform: Option<&crate::platform::PlatformSettings>,
    find: impl FnOnce() -> Option<String>,
) -> Result<(), ProxyError> {
    if platform.is_none() {
        return Ok(());
    }
    match find() {
        Some(field) => Err(ProxyError::StoredObjectReference(capped(field))),
        None => Ok(()),
    }
}

/// The longest field path a refusal names; a longer one is cut here.
const MAX_PATH_LEN: usize = 256;

/// `path`, cut to [`MAX_PATH_LEN`]. Every path is ASCII ([`push_key`]), so
/// any byte is a character boundary.
fn capped(mut path: String) -> String {
    path.truncate(MAX_PATH_LEN);
    path
}

/// Append an object key to a path: `.key` for a key of 1 to 64 ASCII
/// letters, digits and `_`, which every field the APIs define is, and `[?]`
/// for anything else, so a caller's own key never reaches the refusal as
/// written.
fn push_key(path: &mut String, key: &str) {
    if (1..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        path.push('.');
        path.push_str(key);
    } else {
        path.push_str("[?]");
    }
}

/// Whether a value names something: not absent, `null`, `""` or `[]`.
fn names(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    }
}

fn kind(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

/// What a walk does at one node.
enum Visit {
    /// The node names an object stored upstream, in this field under it.
    Refuse(String),
    /// Nothing under the node is a request field: the caller or the model
    /// wrote it for its own purposes.
    Skip,
    /// Everything under the node but this one member, which the model wrote.
    DescendExcept(&'static str),
    Descend,
}

/// The first node under `value`, depth first, that `check` refuses, as the
/// path of the field it names: `input[0].content[1].file_id`.
fn find_under(value: &Value, path: &mut String, check: &dyn Fn(&Value) -> Visit) -> Option<String> {
    let skipped = match check(value) {
        Visit::Refuse(field) => return Some(format!("{path}.{field}")),
        Visit::Skip => return None,
        Visit::DescendExcept(member) => Some(member),
        Visit::Descend => None,
    };
    let len = path.len();
    let found = match value {
        Value::Array(items) => items.iter().enumerate().find_map(|(i, item)| {
            path.truncate(len);
            path.push_str(&format!("[{i}]"));
            find_under(item, path, check)
        }),
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| Some(key.as_str()) != skipped)
            .find_map(|(key, item)| {
                path.truncate(len);
                push_key(path, key);
                find_under(item, path, check)
            }),
        _ => None,
    };
    path.truncate(len);
    found
}

fn find_in(body: &Value, root: &str, check: &dyn Fn(&Value) -> Visit) -> Option<String> {
    let value = body.get(root)?;
    find_under(value, &mut root.to_string(), check)
}

fn responses_reference(body: &Value) -> Option<String> {
    for field in ["conversation", "prompt"] {
        if names(body.get(field)) {
            return Some(field.to_string());
        }
    }
    if names(body.pointer("/prompt_cache_options/comparison_response_id")) {
        return Some("prompt_cache_options.comparison_response_id".into());
    }
    // A tool definition can also arrive in `tool_choice` (the full
    // definitions of an `allowed_tools` choice) and inside `input[]` (an
    // `additional_tools` or `tool_search_output` item), so all three trees
    // are walked with the same check.
    find_in(body, "tools", &responses_node)
        .or_else(|| find_in(body, "tool_choice", &responses_node))
        .or_else(|| find_in(body, "input", &responses_node))
}

fn responses_node(node: &Value) -> Visit {
    // What lies under these is JSON the caller or the model wrote for its
    // own purposes: a function tool's JSON Schema, a custom tool's grammar,
    // a tool search call's arguments, the JSON Schemas an MCP server listed.
    // None of them carries a field the upstream resolves to a stored object.
    if matches!(
        kind(node),
        Some("function" | "custom" | "tool_search_call" | "mcp_list_tools")
    ) {
        return Visit::Skip;
    }
    match responses_tool_reference(node).or_else(|| responses_input_reference(node)) {
        Some(field) => Visit::Refuse(field),
        None => Visit::Descend,
    }
}

/// A tool definition that names a stored object.
fn responses_tool_reference(tool: &Value) -> Option<String> {
    match kind(tool)? {
        "code_interpreter" => match tool.get("container") {
            Some(Value::String(_)) => Some("container".into()),
            Some(container) if names(container.get("file_ids")) => {
                Some("container.file_ids".into())
            }
            _ => None,
        },
        "file_search" => names(tool.get("vector_store_ids")).then(|| "vector_store_ids".into()),
        "image_generation" => names(tool.pointer("/input_image_mask/file_id"))
            .then(|| "input_image_mask.file_id".into()),
        "mcp" => names(tool.get("tunnel_id")).then(|| "tunnel_id".into()),
        "shell" => {
            let environment = tool.get("environment")?;
            container_environment_reference(environment).map(|field| format!("environment.{field}"))
        }
        _ => None,
    }
}

/// A shell environment that names a container, or an automatic one loading
/// uploaded files or skills referenced by id.
fn container_environment_reference(environment: &Value) -> Option<String> {
    match kind(environment)? {
        "container_reference" => Some("container_id".into()),
        "container_auto" => {
            if names(environment.get("file_ids")) {
                return Some("file_ids".into());
            }
            environment
                .get("skills")
                .and_then(Value::as_array)?
                .iter()
                .position(|skill| kind(skill) == Some("skill_reference"))
                .map(|k| format!("skills[{k}].skill_id"))
        }
        _ => None,
    }
}

/// An input item or content part that names a stored object.
fn responses_input_reference(node: &Value) -> Option<String> {
    match kind(node)? {
        "input_file" | "input_image" | "computer_screenshot" if names(node.get("file_id")) => {
            Some("file_id".into())
        }
        "code_interpreter_call" | "container_reference" if names(node.get("container_id")) => {
            Some("container_id".into())
        }
        _ => None,
    }
}

fn chat_reference(req: &aisix_gateway::ChatFormat) -> Option<String> {
    if req
        .extra
        .get("audio")
        .and_then(|audio| audio.get("voice"))
        .is_some_and(|voice| voice.is_object() && names(voice.get("id")))
    {
        return Some("audio.voice.id".into());
    }
    for (i, message) in req.messages.iter().enumerate() {
        for (j, part) in message.content_blocks.iter().flatten().enumerate() {
            if kind(part) == Some("file") && names(part.pointer("/file/file_id")) {
                return Some(format!("messages[{i}].content[{j}].file.file_id"));
            }
        }
        if names(message.extra.get("audio").and_then(|audio| audio.get("id"))) {
            return Some(format!("messages[{i}].audio.id"));
        }
    }
    None
}

fn messages_reference(body: &Value) -> Option<String> {
    match body.get("container") {
        Some(Value::String(s)) if !s.is_empty() => return Some("container".into()),
        Some(container @ Value::Object(_)) => {
            if names(container.get("id")) {
                return Some("container.id".into());
            }
            let custom = container
                .get("skills")
                .and_then(Value::as_array)
                .and_then(|skills| skills.iter().position(|s| kind(s) == Some("custom")));
            if let Some(k) = custom {
                return Some(format!("container.skills[{k}].skill_id"));
            }
        }
        _ => {}
    }
    if names(body.pointer("/diagnostics/previous_message_id")) {
        return Some("diagnostics.previous_message_id".into());
    }
    if names(body.get("thread")) {
        return Some("thread".into());
    }
    if names(body.get("fallback_credit_token")) {
        return Some("fallback_credit_token".into());
    }
    find_in(body, "messages", &messages_node)
}

fn messages_node(node: &Value) -> Visit {
    match kind(node) {
        // Whatever the block's type, a tool use's included: a stored file is
        // a stored file. Checked first so no other arm can pass one through.
        _ if node.get("source").and_then(kind) == Some("file") => {
            Visit::Refuse("source.file_id".into())
        }
        // The model wrote a tool use's `input`; nothing in it is a content
        // block the caller sends. The rest of the block is still read.
        Some(t) if t.ends_with("tool_use") => Visit::DescendExcept("input"),
        Some("container_upload") if names(node.get("file_id")) => Visit::Refuse("file_id".into()),
        _ => Visit::Descend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn platform() -> crate::platform::PlatformSettings {
        crate::platform::PlatformSettings::from_secret("s")
    }

    fn refused_responses(body: Value) -> Option<String> {
        match refuse_in_responses(Some(&platform()), &body) {
            Err(ProxyError::StoredObjectReference(field)) => Some(field),
            Err(other) => panic!("unexpected refusal {other:?}"),
            Ok(()) => None,
        }
    }

    fn refused_messages(body: Value) -> Option<String> {
        match refuse_in_messages(Some(&platform()), &body) {
            Err(ProxyError::StoredObjectReference(field)) => Some(field),
            Err(other) => panic!("unexpected refusal {other:?}"),
            Ok(()) => None,
        }
    }

    fn refused_chat(body: Value) -> Option<String> {
        let req: aisix_gateway::ChatFormat = serde_json::from_value(body).unwrap();
        match refuse_in_chat(Some(&platform()), &req) {
            Err(ProxyError::StoredObjectReference(field)) => Some(field),
            Err(other) => panic!("unexpected refusal {other:?}"),
            Ok(()) => None,
        }
    }

    #[test]
    fn a_responses_request_naming_a_stored_object_names_the_field() {
        let cases = [
            (json!({"conversation": "conv_1"}), "conversation"),
            (json!({"conversation": {"id": "conv_1"}}), "conversation"),
            (
                json!({"prompt": {"id": "pmpt_1", "version": "2"}}),
                "prompt",
            ),
            (
                json!({"tools": [{"type": "function", "name": "f"},
                                 {"type": "code_interpreter", "container": "cntr_1"}]}),
                "tools[1].container",
            ),
            (
                json!({"tools": [{"type": "code_interpreter",
                                  "container": {"type": "auto", "file_ids": ["file-1"]}}]}),
                "tools[0].container.file_ids",
            ),
            (
                json!({"tools": [{"type": "file_search", "vector_store_ids": ["vs_1"]}]}),
                "tools[0].vector_store_ids",
            ),
            (
                json!({"tools": [{"type": "image_generation",
                                  "input_image_mask": {"file_id": "file-1"}}]}),
                "tools[0].input_image_mask.file_id",
            ),
            (
                json!({"tools": [{"type": "shell", "environment":
                    {"type": "container_reference", "container_id": "cntr_1"}}]}),
                "tools[0].environment.container_id",
            ),
            (
                json!({"tools": [{"type": "shell", "environment":
                    {"type": "container_auto", "file_ids": ["file-1"]}}]}),
                "tools[0].environment.file_ids",
            ),
            (
                json!({"tools": [{"type": "shell", "environment": {"type": "container_auto",
                    "skills": [{"type": "inline", "name": "s"},
                               {"type": "skill_reference", "skill_id": "skill_1"}]}}]}),
                "tools[0].environment.skills[1].skill_id",
            ),
            (
                json!({"input": [{"role": "user", "content": [
                    {"type": "input_text", "text": "hi"},
                    {"type": "input_file", "file_id": "file-1"}]}]}),
                "input[0].content[1].file_id",
            ),
            (
                json!({"input": [{"role": "user", "content": [
                    {"type": "input_image", "file_id": "file-1", "detail": "auto"}]}]}),
                "input[0].content[0].file_id",
            ),
            (
                json!({"input": [{"type": "function_call_output", "call_id": "call_1",
                    "output": [{"type": "input_image", "file_id": "file-1"}]}]}),
                "input[0].output[0].file_id",
            ),
            (
                json!({"input": [{"type": "computer_call_output", "call_id": "call_1",
                    "output": {"type": "computer_screenshot", "file_id": "file-1"}}]}),
                "input[0].output.file_id",
            ),
            (
                json!({"input": [{"type": "code_interpreter_call", "id": "ci_1",
                    "container_id": "cntr_1", "code": "1", "outputs": [], "status": "completed"}]}),
                "input[0].container_id",
            ),
            (
                json!({"input": [{"type": "shell_call", "call_id": "call_1",
                    "action": {"commands": ["ls"]},
                    "environment": {"type": "container_reference", "container_id": "cntr_1"}}]}),
                "input[0].environment.container_id",
            ),
            (
                json!({"prompt_cache_options": {"mode": "explicit",
                                                "comparison_response_id": "resp_1"}}),
                "prompt_cache_options.comparison_response_id",
            ),
            (
                json!({"tools": [{"type": "mcp", "server_label": "s", "tunnel_id": "tun_1"}]}),
                "tools[0].tunnel_id",
            ),
            // Tool definitions inside `input[]` are tool definitions too.
            (
                json!({"input": [{"type": "additional_tools", "role": "developer", "tools": [
                    {"type": "code_interpreter", "container": "cntr_1"}]}]}),
                "input[0].tools[0].container",
            ),
            (
                json!({"input": [{"type": "additional_tools", "role": "developer", "tools": [
                    {"type": "function", "name": "f"},
                    {"type": "file_search", "vector_store_ids": ["vs_1"]}]}]}),
                "input[0].tools[1].vector_store_ids",
            ),
            (
                json!({"input": [{"type": "additional_tools", "role": "developer", "tools": [
                    {"type": "shell", "environment": {"type": "container_auto",
                                                      "file_ids": ["file-1"]}}]}]}),
                "input[0].tools[0].environment.file_ids",
            ),
            (
                json!({"input": [{"type": "tool_search_output", "execution": "client",
                    "call_id": "c", "tools": [{"type": "code_interpreter", "container": "cntr_1"}]}]}),
                "input[0].tools[0].container",
            ),
            (
                json!({"input": [{"type": "tool_search_output", "tools": [
                    {"type": "image_generation", "input_image_mask": {"file_id": "file-1"}}]}]}),
                "input[0].tools[0].input_image_mask.file_id",
            ),
            (
                json!({"input": [{"type": "tool_search_output", "tools": [
                    {"type": "shell", "environment": {"type": "container_auto",
                        "skills": [{"type": "skill_reference", "skill_id": "skill_1"}]}}]}]}),
                "input[0].tools[0].environment.skills[0].skill_id",
            ),
            (
                json!({"input": [{"type": "tool_search_output", "tools": [
                    {"type": "mcp", "server_label": "s", "tunnel_id": "tun_1"}]}]}),
                "input[0].tools[0].tunnel_id",
            ),
            (
                json!({"tool_choice": {"type": "allowed_tools", "mode": "auto", "tools": [
                    {"type": "function", "name": "f"},
                    {"type": "code_interpreter", "container": "cntr_1"}]}}),
                "tool_choice.tools[1].container",
            ),
        ];
        for (body, field) in cases {
            assert_eq!(
                refused_responses(body.clone()).as_deref(),
                Some(field),
                "{body}"
            );
        }
    }

    #[test]
    fn a_responses_request_without_one_passes() {
        for body in [
            json!({"model": "m", "input": "hi"}),
            json!({"conversation": null, "prompt": null, "input": []}),
            json!({"tools": [{"type": "code_interpreter", "container": {"type": "auto"}},
                             {"type": "code_interpreter", "container": {"type": "auto", "file_ids": []}},
                             {"type": "shell", "environment": {"type": "container_auto"}},
                             {"type": "shell", "environment": {"type": "local"}},
                             {"type": "image_generation", "input_image_mask": {"image_url": "data:x"}},
                             {"type": "mcp", "server_label": "s", "server_url": "https://m"}]}),
            // JSON the caller or the model wrote for its own purposes: a tool
            // search call's arguments, a function tool's JSON Schema, a
            // custom tool's grammar.
            json!({"input": [{"type": "tool_search_call", "call_id": "c", "execution": "server",
                "arguments": {"filter": {"type": "file_search", "vector_store_ids": ["vs_x"]}}}]}),
            json!({"tools": [{"type": "function", "name": "f", "parameters": {"type": "object",
                "properties": {"t": {"type": "object",
                    "default": {"type": "input_file", "file_id": "file-x"}}}}}]}),
            json!({"tools": [{"type": "custom", "name": "c", "format": {"type": "grammar",
                "syntax": "lark", "definition": "start: \"x\"",
                "examples": {"type": "input_image", "file_id": "file-x"}}}]}),
            // The JSON Schemas an MCP server listed are the server's own.
            json!({"input": [{"type": "mcp_list_tools", "id": "l", "server_label": "s",
                "tools": [{"name": "t", "input_schema": {"type": "object",
                    "default": {"type": "input_file", "file_id": "file-x"}}}]}]}),
            json!({"tool_choice": {"type": "allowed_tools", "mode": "auto",
                                   "tools": [{"type": "mcp", "server_label": "s"}]}}),
            json!({"prompt_cache_options": {"mode": "explicit"},
                   "input": [{"type": "additional_tools", "role": "developer", "tools": [
                       {"type": "function", "name": "f", "parameters": {"type": "object"}},
                       {"type": "code_interpreter", "container": {"type": "auto"}}]}]}),
            json!({"input": [
                {"role": "user", "content": [{"type": "input_image", "image_url": "data:x"},
                                             {"type": "input_file", "file_data": "x", "filename": "a"},
                                             {"type": "input_file", "file_id": null,
                                              "file_url": "https://f"}]},
                {"type": "item_reference", "id": "msg_1"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text",
                    "text": "see", "annotations": [{"type": "container_file_citation",
                        "container_id": "cntr_1", "file_id": "cfile_1"}]}]},
            ]}),
        ] {
            assert_eq!(refused_responses(body.clone()), None, "{body}");
        }
    }

    #[test]
    fn a_chat_request_naming_a_stored_object_names_the_field() {
        assert_eq!(
            refused_chat(json!({"model": "m", "messages": [
                {"role": "user", "content": "hi"},
                {"role": "user", "content": [{"type": "text", "text": "read"},
                                             {"type": "file", "file": {"file_id": "file-1"}}]}]}))
            .as_deref(),
            Some("messages[1].content[1].file.file_id")
        );
        assert_eq!(
            refused_chat(json!({"model": "m", "messages": [
                {"role": "user", "content": "say hi"},
                {"role": "assistant", "audio": {"id": "audio_1"}},
                {"role": "user", "content": "again"}]}))
            .as_deref(),
            Some("messages[1].audio.id")
        );
        assert_eq!(
            refused_chat(json!({"model": "m", "modalities": ["text", "audio"],
                "audio": {"format": "wav", "voice": {"id": "voice_1"}},
                "messages": [{"role": "user", "content": "hi"}]}))
            .as_deref(),
            Some("audio.voice.id")
        );
        assert_eq!(
            refused_chat(json!({"model": "m", "messages": [
                {"role": "user", "content": [
                    {"type": "file", "file": {"file_data": "data:x", "filename": "a.pdf"}},
                    {"type": "image_url", "image_url": {"url": "https://i"}}]}]})),
            None
        );
        assert_eq!(
            refused_chat(
                json!({"model": "m", "audio": {"format": "wav", "voice": "alloy"},
                "messages": [{"role": "user", "content": "hi"}]})
            ),
            None
        );
    }

    #[test]
    fn a_messages_request_naming_a_stored_object_names_the_field() {
        let user = |content: Value| {
            json!({"model": "m", "max_tokens": 1,
                                           "messages": [{"role": "user", "content": content}]})
        };
        let cases = [
            (
                json!({"container": "container_1", "messages": []}),
                "container",
            ),
            (
                json!({"container": {"id": "container_1"}, "messages": []}),
                "container.id",
            ),
            (
                json!({"container": {"skills": [{"type": "anthropic", "skill_id": "pptx"},
                                               {"type": "custom", "skill_id": "skill_1"}]},
                       "messages": []}),
                "container.skills[1].skill_id",
            ),
            (
                user(json!([{"type": "text", "text": "read"},
                            {"type": "document", "source": {"type": "file", "file_id": "file_1"}}])),
                "messages[0].content[1].source.file_id",
            ),
            (
                user(json!([{"type": "image", "source": {"type": "file", "file_id": "file_1"}}])),
                "messages[0].content[0].source.file_id",
            ),
            (
                user(
                    json!([{"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                    {"type": "image", "source": {"type": "file", "file_id": "file_1"}}]}]),
                ),
                "messages[0].content[0].content[0].source.file_id",
            ),
            (
                user(json!([{"type": "container_upload", "file_id": "file_1"}])),
                "messages[0].content[0].file_id",
            ),
            // A tool use's own file source is still a stored file; only its
            // model-written `input` is exempt.
            (
                json!({"model": "m", "max_tokens": 1, "messages": [{"role": "assistant",
                    "content": [{"type": "tool_use", "id": "t", "name": "x", "input": {},
                                 "source": {"type": "file", "file_id": "file_1"}}]}]}),
                "messages[0].content[0].source.file_id",
            ),
            (
                json!({"diagnostics": {"previous_message_id": "msg_1"}, "messages": []}),
                "diagnostics.previous_message_id",
            ),
            (
                json!({"thread": {"type": "continue", "previous_message_id": "msg_1"},
                       "messages": []}),
                "thread",
            ),
            (
                json!({"thread": {"type": "create"}, "messages": []}),
                "thread",
            ),
            (
                json!({"fallback_credit_token": "fct_1", "messages": []}),
                "fallback_credit_token",
            ),
            (
                json!({"fallback_credit_token": {"token": "fct_1", "mode": "best_effort"},
                       "messages": []}),
                "fallback_credit_token",
            ),
            // Any block type naming a stored file is refused, not only the
            // document and image blocks the API defines it on today.
            (
                user(
                    json!([{"type": "search_result", "title": "t", "content": [],
                             "source": {"type": "file", "file_id": "file_1"}}]),
                ),
                "messages[0].content[0].source.file_id",
            ),
            (
                user(json!([{"source": {"type": "file", "file_id": "file_1"}}])),
                "messages[0].content[0].source.file_id",
            ),
            // Only a tool use's `input` is the model's; anything else on the
            // block is still read.
            (
                json!({"messages": [{"role": "assistant", "content": [{"type": "tool_use",
                    "id": "toolu_1", "name": "x", "input": {},
                    "content": [{"type": "document",
                                 "source": {"type": "file", "file_id": "file_1"}}]}]}]}),
                "messages[0].content[0].content[0].source.file_id",
            ),
        ];
        for (body, field) in cases {
            assert_eq!(
                refused_messages(body.clone()).as_deref(),
                Some(field),
                "{body}"
            );
        }
        for body in [
            user(json!("hi")),
            user(
                json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                                      "data": "x"}},
                        {"type": "document", "source": {"type": "url", "url": "https://d"}},
                        {"type": "search_result", "source": "https://s", "title": "t",
                         "content": []}]),
            ),
            json!({"container": {"skills": [{"type": "anthropic", "skill_id": "pptx"}]},
                   "messages": [],
                   "mcp_servers": [{"type": "url", "url": "https://m", "name": "m"}]}),
            json!({"container": null, "diagnostics": {}, "thread": null,
                   "fallback_credit_token": null, "messages": []}),
            // What the model wrote as a tool's input is not a content block,
            // whatever it looks like.
            json!({"messages": [{"role": "assistant", "content": [{"type": "tool_use",
                "id": "toolu_1", "name": "load",
                "input": {"source": {"type": "file", "path": "/tmp/a.csv"}}}]}]}),
            json!({"messages": [{"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "x",
                 "input": {"type": "document", "source": {"type": "file", "file_id": "f"}}},
                {"type": "mcp_tool_use", "id": "mcptoolu_1", "name": "x", "server_name": "s",
                 "input": {"type": "container_upload", "file_id": "f"}}]}]}),
        ] {
            assert_eq!(refused_messages(body.clone()), None, "{body}");
        }
    }

    /// The path names the caller's keys only in the shape the APIs' own
    /// fields have, and is never longer than [`MAX_PATH_LEN`].
    #[test]
    fn a_refused_path_carries_no_caller_text_and_is_bounded() {
        let odd = "k\n".repeat(5);
        assert_eq!(
            refused_responses(json!({"input": [{ odd: {"type": "input_file", "file_id": "f"}}]}))
                .as_deref(),
            Some("input[0][?].file_id")
        );
        assert_eq!(
            refused_responses(json!({"input": [{ "a".repeat(65): {"type": "input_file",
                                                                  "file_id": "f"}}]}))
            .as_deref(),
            Some("input[0][?].file_id")
        );
        let mut deep = json!({"type": "input_file", "file_id": "f"});
        for _ in 0..120 {
            deep = json!([deep]);
        }
        let path = refused_responses(json!({ "input": deep })).unwrap();
        assert_eq!(path.len(), MAX_PATH_LEN, "{path}");
        assert!(path.starts_with("input[0][0]"), "{path}");
    }

    #[test]
    fn without_platform_mode_nothing_is_refused() {
        let body = json!({"conversation": "conv_1", "container": "container_1",
                          "messages": [{"role": "user", "content": "hi"}]});
        assert!(refuse_in_responses(None, &body).is_ok());
        assert!(refuse_in_messages(None, &body).is_ok());
        let req: aisix_gateway::ChatFormat = serde_json::from_value(json!({"model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "file", "file": {"file_id": "file-1"}}]}]}))
        .unwrap();
        assert!(refuse_in_chat(None, &req).is_ok());
    }
}
