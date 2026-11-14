//! Responses 的存储引用共用协议遍历、当前授权和账号证明；审查副本不改写上游正文。

use std::collections::BTreeSet;

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::{
    AcceptedReference, NativeObjectKind, NativeRecord, OriginBinding,
};
use serde_json::Value;

use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::state::ProxyState;

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

pub(crate) async fn resolve(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    expected: Option<&OriginBinding>,
) -> Result<Vec<NativeRecord>, ProxyError> {
    let references = take_references(&mut body.clone());
    let mut records = Vec::with_capacity(references.len());
    let mut account = expected.cloned();
    for (kind, raw) in references {
        if kind == NativeObjectKind::Skill && super::skills::curated_openai_skill(&raw) {
            continue;
        }
        let (_, origin, record) = crate::attribution::detached(super::openai::lookup_reference(
            state,
            auth,
            client,
            &raw,
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

pub(crate) async fn admit_affinity(
    state: &ProxyState,
    auth: &AuthenticatedKey,
    client: &ClientContext,
    body: &Value,
    affinity: &mut crate::responses_affinity::ContextAffinity,
) -> Result<(), ProxyError> {
    if state.native_objects.is_none() && state.platform().is_none() {
        return Ok(());
    }
    for record in resolve(state, auth, client, body, None).await? {
        affinity.admit_object_origin(record.origin.ok_or(ProxyError::AccountUnavailable)?)?;
    }
    Ok(())
}
