//! 同一帧的策略读者共用分类与 JSON 缓存，避免各自复制音频负载。

use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    SessionUpdate,
    TranscriptionUpdate,
    SessionUpdated,
    TranscriptionUpdated,
    AudioAppend,
    AudioCommit,
    AudioCommitted,
    ItemCreate,
    ItemCreated,
    TranscriptionCompleted,
    ResponseCreate,
    ResponseDone,
    Other,
}

impl Kind {
    fn from_name(name: &str) -> Self {
        match name {
            "session.update" => Self::SessionUpdate,
            "transcription_session.update" => Self::TranscriptionUpdate,
            "session.updated" => Self::SessionUpdated,
            "transcription_session.updated" => Self::TranscriptionUpdated,
            "input_audio_buffer.append" => Self::AudioAppend,
            "input_audio_buffer.commit" => Self::AudioCommit,
            "input_audio_buffer.committed" => Self::AudioCommitted,
            "conversation.item.create" => Self::ItemCreate,
            "conversation.item.created" | "conversation.item.added" => Self::ItemCreated,
            "conversation.item.input_audio_transcription.completed" => Self::TranscriptionCompleted,
            "response.create" => Self::ResponseCreate,
            "response.done" => Self::ResponseDone,
            _ => Self::Other,
        }
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'input> serde::de::Visitor<'input> for Visitor {
            type Value = Kind;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an event type")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Kind, E> {
                Ok(Kind::from_name(value))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type", default)]
    kind: Option<Kind>,
}

// 不派生 Debug，避免原始帧或音频/用户正文进入诊断。
pub(crate) struct Frame<'a> {
    bytes: &'a [u8],
    kind: Kind,
    decoded: OnceLock<Option<Value>>,
    #[cfg(test)]
    decodes: std::sync::atomic::AtomicUsize,
}

impl<'a> Frame<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        let mut frame = Self {
            bytes,
            kind: Kind::Other,
            decoded: OnceLock::new(),
            #[cfg(test)]
            decodes: std::sync::atomic::AtomicUsize::new(0),
        };
        let object = bytes
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(b'{');
        match object.then(|| serde_json::from_slice::<Head>(bytes)) {
            Some(Ok(head)) => frame.kind = head.kind.unwrap_or(Kind::Other),
            _ => {
                // 重复 type 等稀有合法 JSON 保留 Value 的 last-wins 语义，不能绕过配置门。
                let decoded = frame.decode();
                frame.kind = decoded
                    .as_ref()
                    .and_then(|event| event["type"].as_str())
                    .map_or(Kind::Other, Kind::from_name);
                let _ = frame.decoded.set(decoded);
            }
        }
        frame
    }

    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    fn decode(&self) -> Option<Value> {
        #[cfg(test)]
        self.decodes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        serde_json::from_slice(self.bytes).ok()
    }

    pub(crate) fn json(&self) -> Option<&Value> {
        self.decoded.get_or_init(|| self.decode()).as_ref()
    }

    pub(crate) fn json_mut(&mut self) -> Option<&mut Value> {
        self.json();
        self.decoded.get_mut().and_then(Option::as_mut)
    }

    #[cfg(test)]
    pub(crate) fn decode_count(&self) -> usize {
        self.decodes.load(std::sync::atomic::Ordering::Relaxed)
    }
}
