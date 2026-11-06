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
//!   - A code interpreter's `container` given as an id, or as an automatic
//!     container with uploaded `file_ids`: `tool_param.py:214` and `:230`.
//!   - A file search's `vector_store_ids`: `file_search_tool_param.py:58`.
//!   - An image generation's `input_image_mask.file_id`: `tool_param.py:256`.
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
//!     (`chat_completion_assistant_message_param.py:21`).
//! - Messages and count_tokens (anthropic `types/`):
//!   - `container`, given as an id or as an object naming one
//!     (`message_create_params.py:135`, `container_params.py:14`), or
//!     loading a `custom` skill (`skill_params.py:11`, `:14`);
//!   - a content block whose `source` is a stored file
//!     (`file_document_source_param.py:9`, `file_image_source_param.py:9`);
//!   - a `container_upload` block (`container_upload_block_param.py:17`).
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
//! - An MCP server: it is named by URL and credential, not by a stored id.
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
        Some(field) => Err(ProxyError::StoredObjectReference(field)),
        None => Ok(()),
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

/// The first node under `root`, depth first, that `check` names a field of,
/// as the path of that field: `input[0].content[1].file_id`.
fn find_under(
    value: &Value,
    path: &mut String,
    check: &dyn Fn(&Value) -> Option<&'static str>,
) -> Option<String> {
    if let Some(field) = check(value) {
        return Some(format!("{path}.{field}"));
    }
    let len = path.len();
    let found = match value {
        Value::Array(items) => items.iter().enumerate().find_map(|(i, item)| {
            path.truncate(len);
            path.push_str(&format!("[{i}]"));
            find_under(item, path, check)
        }),
        Value::Object(map) => map.iter().find_map(|(key, item)| {
            path.truncate(len);
            path.push('.');
            path.push_str(key);
            find_under(item, path, check)
        }),
        _ => None,
    };
    path.truncate(len);
    found
}

fn find_in(
    body: &Value,
    root: &str,
    check: &dyn Fn(&Value) -> Option<&'static str>,
) -> Option<String> {
    let value = body.get(root)?;
    find_under(value, &mut root.to_string(), check)
}

fn responses_reference(body: &Value) -> Option<String> {
    for field in ["conversation", "prompt"] {
        if names(body.get(field)) {
            return Some(field.to_string());
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for (i, tool) in tools.iter().enumerate() {
            if let Some(field) = responses_tool_reference(tool) {
                return Some(format!("tools[{i}].{field}"));
            }
        }
    }
    find_in(body, "input", &responses_input_reference)
}

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

fn responses_input_reference(node: &Value) -> Option<&'static str> {
    match kind(node)? {
        "input_file" | "input_image" | "computer_screenshot" if names(node.get("file_id")) => {
            Some("file_id")
        }
        "code_interpreter_call" | "container_reference" if names(node.get("container_id")) => {
            Some("container_id")
        }
        _ => None,
    }
}

fn chat_reference(req: &aisix_gateway::ChatFormat) -> Option<String> {
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
    find_in(body, "messages", &messages_block_reference)
}

fn messages_block_reference(node: &Value) -> Option<&'static str> {
    if node.get("source").and_then(kind) == Some("file") {
        return Some("source.file_id");
    }
    (kind(node) == Some("container_upload") && names(node.get("file_id"))).then_some("file_id")
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
            refused_chat(json!({"model": "m", "messages": [
                {"role": "user", "content": [
                    {"type": "file", "file": {"file_data": "data:x", "filename": "a.pdf"}},
                    {"type": "image_url", "image_url": {"url": "https://i"}}]}]})),
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
            json!({"container": null, "messages": []}),
        ] {
            assert_eq!(refused_messages(body.clone()), None, "{body}");
        }
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
