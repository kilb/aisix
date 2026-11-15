//! Responses 的存储引用共用协议遍历、当前授权和账号证明；审查副本不改写上游正文。

use std::collections::BTreeSet;

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{
    configured_native_origin, AcceptedReference, NativeObjectKind, NativeProtocol, NativeRecord,
    OriginBinding,
};
use aisix_core::{ProviderKey, ResourceEntry};
use serde_json::Value;

use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::state::ProxyState;

fn conversation_id(body: &Value) -> Option<&str> {
    let conversation = body.get("conversation")?;
    conversation
        .as_str()
        .or_else(|| conversation.get("id").and_then(Value::as_str))
}

pub(crate) fn final_conversation(
    body: &Value,
    admitted: Option<&(String, uuid::Uuid)>,
) -> (Option<uuid::Uuid>, Option<String>) {
    match conversation_id(body) {
        Some(raw) if admitted.is_some_and(|(previous, _)| previous == raw) => {
            (admitted.map(|(_, id)| *id), None)
        }
        Some(raw) => (None, Some(raw.to_owned())),
        None => (None, None),
    }
}

pub(crate) async fn admit_final_conversation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    provider: &ResourceEntry<ProviderKey>,
    affinity: &mut crate::responses_affinity::ContextAffinity,
) -> Result<uuid::Uuid, ProxyError> {
    let (origin, record_id) = crate::attribution::detached(resolve_admitted_reference(
        state,
        auth,
        client,
        raw,
        NativeObjectKind::Conversation,
        affinity,
    ))
    .await?;
    let provider_key_id = super::openai::resource_id(state, "provider_keys", &provider.id)?;
    let boundary = configured_native_origin(
        &state
            .native_origin_key()
            .ok_or(ProxyError::AccountUnavailable)?,
        provider_key_id,
        &provider.value,
        NativeProtocol::Openai,
        NativeObjectKind::Response,
    )
    .map_err(|_| ProxyError::AccountUnavailable)?;
    if origin.provider_key_id != provider_key_id || origin.boundary != boundary {
        return Err(ProxyError::ResponseNotFound);
    }
    Ok(record_id)
}

async fn resolve_admitted_reference(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    raw: &str,
    kind: NativeObjectKind,
    affinity: &mut crate::responses_affinity::ContextAffinity,
) -> Result<(OriginBinding, uuid::Uuid), ProxyError> {
    if let Some(record) = affinity.admitted_reference(kind, raw) {
        let (_, origin) = super::openai::validate_reference_record(
            state,
            auth,
            client,
            raw,
            kind,
            None,
            EndpointFamily::Responses,
            record,
        )?;
        return Ok((origin, record.record_id));
    }
    let (_, origin, record) = super::openai::lookup_reference(
        state,
        auth,
        client,
        raw,
        kind,
        None,
        EndpointFamily::Responses,
    )
    .await?;
    let record_id = record.record_id;
    affinity.note_admitted_reference(kind, record);
    Ok((origin, record_id))
}

pub(crate) fn take_references(body: &mut Value) -> Vec<(NativeObjectKind, String)> {
    let mut references = Vec::new();
    if let Some(raw) = super::openai_conversations::take_response_conversation(body) {
        references.push((NativeObjectKind::Conversation, raw));
    }
    for field in ["input", "tools", "tool_choice"] {
        if let Some(value) = body.get_mut(field) {
            super::openai_conversations::take_references(value, &mut references);
        }
    }
    references.sort_by(|(left_kind, left), (right_kind, right)| {
        (left_kind.as_str(), left).cmp(&(right_kind.as_str(), right))
    });
    references.dedup();
    references
}

pub(crate) struct ReferenceScan<'a> {
    references: Vec<(NativeObjectKind, &'a str)>,
    pub(crate) needs_review_copy: bool,
}

impl<'a> ReferenceScan<'a> {
    /// 只借用正式协议槽；不复制文本、内联媒体或工具参数。
    pub(crate) fn collect(body: &'a Value) -> Self {
        let mut scan = Self {
            references: Vec::new(),
            needs_review_copy: false,
        };
        if let Some(raw) = conversation_id(body) {
            scan.push(NativeObjectKind::Conversation, raw);
        }
        for field in ["input", "tools", "tool_choice"] {
            if let Some(value) = body.get(field) {
                scan.visit(value);
            }
        }
        // 旧封印检查也处理未声明 type 的直接容器槽，保留其审查路径。
        scan.needs_review_copy |=
            body.get("input")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        ["/container_id", "/environment/container_id"]
                            .iter()
                            .any(|slot| item.pointer(slot).is_some_and(Value::is_string))
                    })
                });
        scan.references
            .sort_by(|(left_kind, left), (right_kind, right)| {
                (left_kind.as_str(), left).cmp(&(right_kind.as_str(), right))
            });
        scan.references.dedup();
        scan
    }

    fn push(&mut self, kind: NativeObjectKind, raw: &'a str) {
        self.references.push((kind, raw));
        self.needs_review_copy = true;
    }

    fn array(&mut self, value: &'a Value, field: &str, kind: NativeObjectKind) {
        if let Some(ids) = value.get(field).and_then(Value::as_array) {
            // 原审查遍历也会清空无字符串 ID 的数组，不能按引用数量略过。
            self.needs_review_copy = true;
            self.references
                .extend(ids.iter().filter_map(Value::as_str).map(|raw| (kind, raw)));
        }
    }

    fn visit(&mut self, value: &'a Value) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(
            kind,
            "function" | "custom" | "tool_search_call" | "mcp_list_tools"
        ) {
            return;
        }
        let field = match kind {
            "item_reference" => Some(("id", NativeObjectKind::ResponseItem)),
            "input_file" | "input_image" | "computer_screenshot" => {
                Some(("file_id", NativeObjectKind::File))
            }
            "code_interpreter_call" | "container_reference" => {
                Some(("container_id", NativeObjectKind::ResponseContainer))
            }
            "skill_reference" => Some(("skill_id", NativeObjectKind::Skill)),
            _ => None,
        };
        if let Some((field, object_kind)) = field {
            if let Some(raw) = value.get(field).and_then(Value::as_str) {
                self.push(object_kind, raw);
                if matches!(kind, "container_reference" | "skill_reference") {
                    return;
                }
            }
        }
        match kind {
            "code_interpreter" => {
                if let Some(raw) = value.get("container").and_then(Value::as_str) {
                    self.push(NativeObjectKind::ResponseContainer, raw);
                } else if let Some(container) = value.get("container") {
                    self.array(container, "file_ids", NativeObjectKind::File);
                }
            }
            "file_search" => self.array(value, "vector_store_ids", NativeObjectKind::VectorStore),
            "container_auto" => self.array(value, "file_ids", NativeObjectKind::File),
            "image_generation" => {
                if let Some(raw) = value
                    .pointer("/input_image_mask/file_id")
                    .and_then(Value::as_str)
                {
                    self.push(NativeObjectKind::File, raw);
                }
            }
            _ => {}
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    self.visit(item);
                }
            }
            Value::Object(_) => {
                let children: &[&str] = match kind {
                    "" | "message" => &["content"],
                    "function_call_output" | "custom_tool_call_output" | "computer_call_output" => {
                        &["output"]
                    }
                    "shell" | "shell_call" => &["environment"],
                    "container_auto" => &["skills"],
                    "additional_tools" | "tool_search_output" | "allowed_tools" => &["tools"],
                    _ => &[],
                };
                for field in children {
                    if let Some(child) = value
                        .get(field)
                        .filter(|child| child.is_array() || child.is_object())
                    {
                        self.visit(child);
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) async fn resolve(
        &self,
        state: &ProxyState,
        auth: &AuthenticatedKey,
        client: &ClientContext,
        expected: Option<&OriginBinding>,
    ) -> Result<Vec<NativeRecord>, ProxyError> {
        let mut records = Vec::with_capacity(self.references.len());
        let mut account = expected.cloned();
        for &(kind, raw) in &self.references {
            if kind == NativeObjectKind::Skill && super::skills::curated_openai_skill(raw) {
                continue;
            }
            let (_, origin, record) =
                crate::attribution::detached(super::openai::lookup_reference(
                    state,
                    auth,
                    client,
                    raw,
                    kind,
                    None,
                    EndpointFamily::Responses,
                ))
                .await?;
            if account.as_ref().is_some_and(|expected| {
                expected.provider_key_id != origin.provider_key_id
                    || expected.boundary != origin.boundary
            }) {
                return Err(ProxyError::ResponseNotFound);
            }
            account.get_or_insert(origin);
            records.push(record);
        }
        Ok(records)
    }

    pub(crate) async fn resolve_admitted(
        &self,
        state: &ProxyState,
        auth: &AuthenticatedKey,
        client: &ClientContext,
        expected: Option<&OriginBinding>,
        affinity: &mut crate::responses_affinity::ContextAffinity,
    ) -> Result<Vec<OriginBinding>, ProxyError> {
        let mut origins = Vec::with_capacity(self.references.len());
        let mut account = expected.cloned();
        for &(kind, raw) in &self.references {
            if kind == NativeObjectKind::Skill && super::skills::curated_openai_skill(raw) {
                continue;
            }
            let (origin, _) = crate::attribution::detached(resolve_admitted_reference(
                state, auth, client, raw, kind, affinity,
            ))
            .await?;
            if account.as_ref().is_some_and(|expected| {
                expected.provider_key_id != origin.provider_key_id
                    || expected.boundary != origin.boundary
            }) {
                return Err(ProxyError::ResponseNotFound);
            }
            account.get_or_insert_with(|| origin.clone());
            origins.push(origin);
        }
        Ok(origins)
    }
}

#[cfg(test)]
pub(crate) async fn resolve(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    expected: Option<&OriginBinding>,
) -> Result<Vec<NativeRecord>, ProxyError> {
    ReferenceScan::collect(body)
        .resolve(state, auth, client, expected)
        .await
}

pub(crate) fn accepted(records: &[NativeRecord]) -> Vec<AcceptedReference> {
    let mut seen = BTreeSet::new();
    records
        .iter()
        .filter(|record| seen.insert(record.record_id))
        .map(|record| AcceptedReference {
            record_id: record.record_id,
            consumer_family: EndpointFamily::Responses,
        })
        .collect()
}

pub(crate) async fn admit_affinity_with_conversation(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    affinity: &mut crate::responses_affinity::ContextAffinity,
) -> Result<Option<(String, uuid::Uuid)>, ProxyError> {
    if state.native_objects.is_none() && state.platform().is_none() {
        return Ok(None);
    }
    for origin in ReferenceScan::collect(body)
        .resolve_admitted(state, auth, client, None, affinity)
        .await?
    {
        affinity.admit_object_origin(origin)?;
    }
    // 只取本次正式 conversation 槽匹配的已授权记录，不能沿用已删除的旧槽。
    Ok(conversation_id(body).and_then(|raw| {
        affinity
            .admitted_reference(NativeObjectKind::Conversation, raw)
            .map(|record| (raw.to_owned(), record.record_id))
    }))
}

#[cfg(test)]
pub(crate) async fn admit_affinity(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    affinity: &mut crate::responses_affinity::ContextAffinity,
) -> Result<(), ProxyError> {
    admit_affinity_with_conversation(state, auth, client, body, affinity)
        .await
        .map(|_| ())
}

#[cfg(test)]
#[path = "response_refs_perf_tests.rs"]
mod perf_tests;

#[cfg(test)]
#[path = "response_refs_scan_tests.rs"]
mod scan_tests;
