use super::*;
use aisix_core::native_objects::*;
use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, ProxyConfig};
use futures::future::BoxFuture;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

use crate::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use crate::redact::sse_json_parse_counter::Scope;

#[derive(Default)]
struct NoNativeCalls(AtomicUsize);

macro_rules! refuse_call {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for NoNativeCalls {
    refuse_call!(begin, BeginRequest);
    refuse_call!(bind, BindRequest);
    refuse_call!(lookup, LookupRequest);
    refuse_call!(owned, OwnedRequest);
    refuse_call!(tombstone, TombstoneRequest);
    refuse_call!(claim, ClaimRequest);
    refuse_call!(renew, LeaseRequest);
    refuse_call!(reschedule, LeaseRequest);
    refuse_call!(complete, CompleteRequest);
}

fn native_observer(platform: bool) -> (NativeOutputFiles, Arc<NoNativeCalls>) {
    let snapshot = AisixSnapshot::default();
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash": "stream-observation-test", "allowed_models": ["*"]
    }))
    .unwrap();
    let auth = AuthenticatedKey::new(
        &snapshot,
        Arc::new(ResourceEntry::new("stream-observation-key", key, 1)),
        None,
        false,
    );
    let config = ProxyConfig {
        addr: "127.0.0.1:0".into(),
        request_body_limit_bytes: 1_048_576,
        real_ip: Default::default(),
        request_id: Default::default(),
        url_rewrites: Vec::new(),
        expose_debug_headers: false,
        platform: Default::default(),
        tls: None,
        listeners: Vec::new(),
        thread_per_core: None,
        workers: None,
    };
    let registry = Arc::new(NoNativeCalls::default());
    let mut state = ProxyState::new(
        SnapshotHandle::new(snapshot),
        Arc::new(aisix_gateway::Hub::new()),
        &config,
    )
    .without_cache()
    .with_native_objects(registry.clone());
    if platform {
        state = state.with_platform(crate::platform::PlatformSettings::from_secret(
            "stream-observation-test",
        ));
    }
    (
        NativeOutputFiles {
            state,
            auth,
            client: ClientContext::default(),
            requested_model: "public-model".into(),
            direct_model: "direct-model".into(),
            origin: None,
            container_admission: None,
        },
        registry,
    )
}

fn text_frames(deltas: usize, text: &str, newline: &str) -> Vec<Bytes> {
    let mut events = vec![json!({"type":"message_start","message":{
        "id":"msg_stream_observation","type":"message","role":"assistant",
        "model":"provider-model","content":[],"usage":{"input_tokens":17,"output_tokens":0}
    },"unknown":{"preserve":"verbatim"}})];
    for index in 0..deltas {
        events.push(json!({"type":"content_block_delta","index":index,"delta":{
            "type":"text_delta","text":text
        },"metadata":{"file_id":"ordinary_metadata_is_not_an_artifact"}}));
    }
    events.push(
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
        "usage":{"output_tokens":5}}),
    );
    events.push(json!({"type":"message_stop"}));
    events
        .into_iter()
        .map(|value| {
            Bytes::from(format!(
                "event: {}{newline}data: {value}{newline}{newline}",
                value["type"].as_str().unwrap()
            ))
        })
        .collect()
}

fn chunks(frames: &[Bytes], bytewise: bool) -> Vec<Bytes> {
    let joined = frames.concat();
    if bytewise {
        joined
            .into_iter()
            .map(|byte| Bytes::from(vec![byte]))
            .collect()
    } else {
        vec![Bytes::from(joined)]
    }
}

async fn relay(
    chunks: Vec<Bytes>,
    observer: Option<NativeOutputFiles>,
) -> (Vec<u8>, AnthropicStreamUsage, usize) {
    let captured = Arc::new(Mutex::new(None));
    let captured_out = captured.clone();
    let counter = Scope::start();
    let clock = Instant::now();
    let mut stream = build_anthropic_passthrough_stream_with_files(
        futures::stream::iter(chunks.into_iter().map(Ok)),
        clock,
        clock,
        None,
        "public-model".into(),
        Some(65_536),
        None,
        observer,
        move |usage| *captured_out.lock().unwrap() = Some(usage),
    );
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        output.extend_from_slice(&chunk.unwrap());
    }
    drop(stream);
    let parses = counter.attempts();
    drop(counter);
    let usage = captured.lock().unwrap().take().unwrap();
    (output, usage, parses)
}

fn same_usage(left: &AnthropicStreamUsage, right: &AnthropicStreamUsage) {
    assert_eq!(left.prompt_tokens, right.prompt_tokens);
    assert_eq!(left.completion_tokens, right.completion_tokens);
    assert_eq!(left.cache_creation_tokens, right.cache_creation_tokens);
    assert_eq!(left.cache_read_tokens, right.cache_read_tokens);
    assert_eq!(left.response_text, right.response_text);
    assert_eq!(left.reached_end, right.reached_end);
    assert_eq!(left.malformed_sse_data, right.malformed_sse_data);
    assert_eq!(left.stream_failure, right.stream_failure);
}

#[test]
fn stream_observation_counter_counts_real_json_attempts_and_resets() {
    for _ in 0..2 {
        let counter = Scope::start();
        assert!(crate::redact::parse_sse_json_event(b"data: {}\n\n")
            .0
            .is_some());
        assert!(crate::redact::parse_sse_json_event(b"data: broken\n\n").1);
        assert_eq!(
            crate::redact::parse_sse_json_event(b"data: [DONE]\n\n"),
            (None, false)
        );
        assert_eq!(
            crate::redact::parse_sse_json_event(b": ping\n\n"),
            (None, false)
        );
        assert_eq!(counter.attempts(), 2);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stream_observation_text_frames_reuse_the_usage_parse() {
    for platform in [false, true] {
        for newline in ["\n", "\r\n", "\r"] {
            for bytewise in [false, true] {
                let frames = text_frames(8, "普通文本 🦀 {\"file_id\":\"prose\"}", newline);
                let input = chunks(&frames, bytewise);
                let (reference, reference_usage, reference_parses) =
                    relay(input.clone(), None).await;
                let (observer, registry) = native_observer(platform);
                let (observed, observed_usage, observed_parses) =
                    relay(input, Some(observer)).await;
                assert_eq!(
                    observed, reference,
                    "platform={platform}, newline={newline:?}, bytewise={bytewise}"
                );
                same_usage(&observed_usage, &reference_usage);
                assert_eq!(
                    (
                        observed_usage.prompt_tokens,
                        observed_usage.completion_tokens
                    ),
                    (17, 5)
                );
                assert!(observed_usage.reached_end);
                assert_eq!(registry.0.load(Ordering::Relaxed), 0);
                assert_eq!(reference_parses, 11);
                assert_eq!(
                    observed_parses, reference_parses,
                    "native observation must reuse the parsed event instead of parsing ordinary text twice"
                );
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stream_observation_multiline_eof_and_terminal_delivery_are_equivalent() {
    let frames = vec![
        Bytes::from_static(b": ping\n\ndata: {\"type\":\"content_block_delta\",\ndata: \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"joined\"}}\n\n"),
        Bytes::from_static(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}"),
    ];
    for bytewise in [false, true] {
        let input = chunks(&frames, bytewise);
        let (reference, usage, _) = relay(input.clone(), None).await;
        let (observer, registry) = native_observer(false);
        let (observed, observed_usage, _) = relay(input, Some(observer)).await;
        assert_eq!(observed, reference);
        same_usage(&observed_usage, &usage);
        assert!(String::from_utf8_lossy(&observed).contains("joined"));
        assert_eq!(observed_usage.completion_tokens, 5);
        assert_eq!(registry.0.load(Ordering::Relaxed), 0);
    }
    let mut frames = text_frames(1, "before", "\n");
    frames.push(Bytes::from_static(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"AFTER_TERMINAL\"}}\n\n"));
    let (observer, _) = native_observer(false);
    let (observed, usage, _) = relay(chunks(&frames, false), Some(observer)).await;
    assert!(usage.reached_end);
    assert!(!String::from_utf8_lossy(&observed).contains("AFTER_TERMINAL"));
}

#[tokio::test(flavor = "current_thread")]
async fn stream_observation_malformed_data_is_not_released() {
    let frames = vec![Bytes::from_static(b"data: not-json\n\n")];
    let (reference, usage, _) = relay(chunks(&frames, false), None).await;
    let (observer, registry) = native_observer(false);
    let (observed, observed_usage, _) = relay(chunks(&frames, false), Some(observer)).await;
    assert_eq!(observed, reference);
    same_usage(&observed_usage, &usage);
    assert!(observed_usage.malformed_sse_data);
    assert!(!String::from_utf8_lossy(&observed).contains("not-json"));
    assert_eq!(registry.0.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn stream_observation_plain_delta_benchmark() {
    let (observer, registry) = native_observer(false);
    for text_bytes in [16, 16_384] {
        let text = "x".repeat(text_bytes);
        for deltas in [1, 2, 8, 64] {
            let frames = text_frames(deltas, &text, "\n");
            let joined = vec![Bytes::from(frames.concat())];
            for (shape, input) in [("one-frame-per-chunk", frames), ("coalesced", joined)] {
                let iterations = if text_bytes == 16 { 128 } else { 8 };
                for enabled in [false, true] {
                    let started = Instant::now();
                    let mut total_parses = 0;
                    let mut total_bytes = 0;
                    for _ in 0..iterations {
                        let (output, usage, parses) =
                            relay(input.clone(), enabled.then(|| observer.clone())).await;
                        assert!(usage.reached_end);
                        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (17, 5));
                        total_parses += parses;
                        total_bytes += output.len();
                        std::hint::black_box(output);
                    }
                    println!(
                        "STREAM_OBSERVATION_BENCH {{\"text_bytes\":{text_bytes},\"deltas\":{deltas},\"shape\":\"{shape}\",\"observer\":{enabled},\"iterations\":{iterations},\"parse_attempts\":{total_parses},\"output_bytes\":{total_bytes},\"elapsed_ns\":{}}}",
                        started.elapsed().as_nanos()
                    );
                }
            }
        }
    }
    assert_eq!(registry.0.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn stream_observation_full_text_blocks_preserve_bytes_and_report_small_shape_cost() {
    let (observer, registry) = native_observer(false);
    let mut all_reused = true;
    for deltas in [1, 2, 8] {
        let envelope = text_frames(0, "", "\n");
        let mut frames = vec![
            envelope[0].clone(),
            Bytes::from_static(b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"),
        ];
        for _ in 0..deltas {
            frames.push(Bytes::from_static(b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ordinary text\"}}\n\n"));
        }
        frames.push(Bytes::from_static(
            b"data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        ));
        frames.extend_from_slice(&envelope[1..]);
        let joined = vec![Bytes::from(frames.concat())];
        for (shape, input) in [("one-frame-per-chunk", frames), ("coalesced", joined)] {
            let (expected, expected_usage, expected_parses) = relay(input.clone(), None).await;
            assert_eq!(expected_parses, deltas + 5);
            for enabled in [false, true] {
                let started = Instant::now();
                let iterations = 128;
                let mut total_parses = 0;
                let mut total_bytes = 0;
                for _ in 0..iterations {
                    let (output, usage, parses) =
                        relay(input.clone(), enabled.then(|| observer.clone())).await;
                    assert_eq!(output, expected);
                    same_usage(&usage, &expected_usage);
                    assert!(usage.reached_end);
                    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (17, 5));
                    total_parses += parses;
                    total_bytes += output.len();
                    std::hint::black_box(output);
                }
                if enabled {
                    all_reused &= total_parses == iterations * expected_parses;
                }
                println!("STREAM_OBSERVATION_FULL_BLOCK_BENCH {{\"deltas\":{deltas},\"shape\":\"{shape}\",\"observer\":{enabled},\"iterations\":{iterations},\"parse_attempts\":{total_parses},\"output_bytes\":{total_bytes},\"elapsed_ns\":{}}}", started.elapsed().as_nanos());
            }
        }
    }
    assert_eq!(registry.0.load(Ordering::Relaxed), 0);
    assert!(
        all_reused,
        "a full text block must not cause duplicate JSON parsing"
    );
}
