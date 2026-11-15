//! 分类器的真实分配/解码计数组件；不把它称为整条 WS 转发基准。

#[path = "../src/native_api/realtime_frame.rs"]
mod realtime_frame;

use aisix_response_reference_test_alloc::{AllocationScope, TrackedAllocator};
use realtime_frame::{Frame, Kind};

#[global_allocator]
static ALLOCATOR: TrackedAllocator = TrackedAllocator;

fn measured<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let scope = AllocationScope::start();
    let value = work();
    (value, scope.finish().bytes)
}

#[test]
fn client_pcm_classification_does_not_copy_the_base64_payload() {
    let payload = format!(
        "{{\"type\":\"input_audio_buffer.append\",\"audio\":\"{}\"}}",
        "A".repeat(32 * 1024)
    );
    let (frame, allocated) = measured(|| Frame::new(payload.as_bytes()));
    assert_eq!(frame.kind(), Kind::AudioAppend);
    assert!(
        allocated < 1024,
        "client PCM classifier allocated {allocated} bytes"
    );
    assert_eq!(frame.decode_count(), 0, "PCM does not require a full Value");
}

#[test]
fn supplier_audio_delta_classification_does_not_copy_the_base64_payload() {
    let payload = format!(
        "{{\"type\":\"response.output_audio.delta\",\"delta\":\"{}\"}}",
        "A".repeat(32 * 1024)
    );
    let (frame, allocated) = measured(|| Frame::new(payload.as_bytes()));
    assert_eq!(frame.kind(), Kind::Other);
    assert!(
        allocated < 1024,
        "supplier audio classifier allocated {allocated} bytes"
    );
    assert_eq!(
        frame.decode_count(),
        0,
        "audio delta does not require a full Value"
    );
}

#[test]
fn policy_readers_share_one_configuration_decode_and_honor_escaped_names() {
    let mut frame = Frame::new(
        br#"{"ty\u0070e":"session.up\u0064ate","session":{"audio":{"input":{"transcription":{"model":"public-asr"}}}}}"#,
    );
    assert_eq!(frame.kind(), Kind::SessionUpdate);
    let first = frame.json().expect("actual configuration JSON");
    let second = frame.json().expect("the same cached configuration JSON");
    assert!(
        std::ptr::eq(first, second),
        "policy readers share the actual parsed Value"
    );
    assert_eq!(
        frame.decode_count(),
        1,
        "exactly one real JSON decode occurred"
    );
    assert_eq!(
        second
            .pointer("/session/audio/input/transcription/model")
            .and_then(serde_json::Value::as_str),
        Some("public-asr")
    );
    *frame
        .json_mut()
        .and_then(|value| value.pointer_mut("/session/audio/input/transcription/model"))
        .expect("the authorized model slot") = serde_json::json!("actual-asr");
    assert_eq!(
        frame
            .json()
            .and_then(|value| value.pointer("/session/audio/input/transcription/model"))
            .and_then(serde_json::Value::as_str),
        Some("actual-asr")
    );
    assert_eq!(frame.decode_count(), 1, "rewriting reuses the parsed Value");
}

#[test]
fn duplicate_type_keeps_last_wins_and_cannot_bypass_configuration_gates() {
    for bytes in [
        br#"{"type":"input_audio_buffer.append","type":"session.update","session":{"voice":{"id":"owned-voice"}}}"#.as_slice(),
        br#"{"type":17,"type":"session.update","session":{"voice":{"id":"owned-voice"}}}"#.as_slice(),
    ] {
        let frame = Frame::new(bytes);
        assert_eq!(frame.kind(), Kind::SessionUpdate);
        assert_eq!(
            frame.json().and_then(|value| value.pointer("/session/voice/id")).and_then(serde_json::Value::as_str),
            Some("owned-voice")
        );
        assert_eq!(frame.decode_count(), 1, "rare fallback shares one actual Value");
    }
    let frame = Frame::new(br#"{"type":"session.update","type":17}"#);
    assert_eq!(frame.kind(), Kind::Other);
    assert_eq!(
        frame.json().and_then(|value| value["type"].as_i64()),
        Some(17)
    );
}

#[test]
fn malformed_json_is_not_reclassified_as_an_authorized_configuration() {
    let frame = Frame::new(br#"{"type":"session.update","session": "#);
    assert_eq!(frame.kind(), Kind::Other);
    assert!(frame.json().is_none());
    let array = Frame::new(br#"["session.update"]"#);
    assert_eq!(
        array.kind(),
        Kind::Other,
        "a sequence is not an event object"
    );
    assert!(array.json().is_some_and(serde_json::Value::is_array));
}

#[test]
fn large_legal_configuration_and_nested_type_remain_intact() {
    let instructions = "x".repeat(64 * 1024);
    let payload = serde_json::json!({
        "type":"session.update",
        "session":{"instructions":instructions,"tools":[{"type":"function","parameters":{"type":"object","properties":{"type":{"type":"string"}}}}]}
    }).to_string();
    let frame = Frame::new(payload.as_bytes());
    assert_eq!(frame.kind(), Kind::SessionUpdate);
    let value = frame.json().expect("complete legal configuration");
    assert_eq!(
        value
            .pointer("/session/instructions")
            .and_then(serde_json::Value::as_str),
        Some(instructions.as_str())
    );
    assert_eq!(
        value
            .pointer("/session/tools/0/parameters/properties/type/type")
            .and_then(serde_json::Value::as_str),
        Some("string")
    );
    assert_eq!(frame.decode_count(), 1);
}
