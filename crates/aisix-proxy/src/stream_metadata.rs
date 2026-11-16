//! 只读调用方已经解析的流式选择；与响应是否实际采用 SSE 无关。

use crate::operation::{self as op, Surface};
use serde_json::Value;

pub(crate) fn json_intent(body: &Value) -> Option<bool> {
    let body = body.as_object()?;
    match body.get("stream") {
        None | Some(Value::Null) => Some(false),
        Some(value) => value.as_bool(),
    }
}

pub(crate) fn note_json(body: &Value) {
    crate::attribution::note_requested_stream(json_intent(body));
}

pub(crate) fn multipart_intent<'a>(
    fields: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Option<bool> {
    let mut intent = None;
    for (name, bytes) in fields {
        if name == "stream" {
            let value = match std::str::from_utf8(bytes).map(str::trim) {
                Ok("true") => true,
                Ok("false") => false,
                _ => return None,
            };
            // 重复字段有歧义时不猜测，正文仍按原路径转发。
            if intent.is_some_and(|previous| previous != value) {
                return None;
            }
            intent = Some(value);
        }
    }
    Some(intent.unwrap_or(false))
}

/// 固定非流接口即使在正文解析前拒绝也可确定；新表面默认未知。
pub(crate) fn fixed_intent(surface: Surface) -> Option<bool> {
    matches!(
        surface,
        op::COUNT_TOKENS
            | op::EMBEDDINGS
            | op::RERANK
            | op::SYSTEMONE
            | op::DECISIONS
            | op::FILES
            | op::UPLOADS
            | op::EVALS
            | op::CONVERSATIONS
            | op::CONTAINERS
            | op::VECTOR_STORES
            | op::BATCHES
            | op::FINE_TUNING
            | op::IMAGE_VARIATION
            | op::MODERATIONS
            | op::CONTENT_PROVENANCE_CHECKS
            | op::TRANSLATION
            | op::VIDEO_GENERATION
            | op::BATCH_COMPLETION
            | op::EVAL_COMPLETION
            | crate::native_api::skills::SURFACE
            | crate::native_api::voices::VOICES_SURFACE
            | crate::native_api::voices::CONSENTS_SURFACE
    )
    .then_some(false)
}
