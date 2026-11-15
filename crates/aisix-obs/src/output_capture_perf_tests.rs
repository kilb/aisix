use std::cell::Cell;

use super::{OutputCapture, OutputCollector, OutputWire, Part, SlotKey};
use serde_json::{json, Value};

thread_local! {
    // 按测试线程计数真实长度访问，不依赖调度、计时或实现内部的累计值。
    static PART_LEN_VISITS: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn record_part_len_visit() {
    PART_LEN_VISITS.with(|visits| visits.set(visits.get() + 1));
}

fn parts(capture: &OutputCapture) -> Value {
    Value::Array(capture.parts_json().unwrap_or_default())
}

fn feed(collector: &mut OutputCollector, event: Value) {
    collector.feed_sse(format!("data: {event}\n\n").as_bytes());
}

fn alternating_capture_work(fragments: usize) -> usize {
    let mut collector = OutputCollector::new(OutputWire::OpenAiChat, fragments);
    PART_LEN_VISITS.with(|visits| visits.set(0));
    for index in 0..fragments {
        let delta = if index % 2 == 0 {
            json!({"reasoning_content":"r"})
        } else {
            json!({"content":"t"})
        };
        feed(
            &mut collector,
            json!({"choices":[{"index":0,"delta":delta}]}),
        );
    }
    let visits = PART_LEN_VISITS.with(Cell::get);
    let capture = collector.finish();
    assert!(!capture.truncated);
    let output = capture.parts_json().unwrap_or_default();
    assert_eq!(output.len(), fragments);
    for (index, part) in output.iter().enumerate() {
        let expected = if index % 2 == 0 {
            json!({"type":"reasoning","content":"r"})
        } else {
            json!({"type":"text","content":"t"})
        };
        assert_eq!(part, &expected, "fragment {index}");
    }
    visits
}

#[test]
fn alternating_stream_capture_length_work_grows_linearly() {
    // 恢复每次累计所有旧片段的实现时，128/256 片段分别访问 8128/32640 次。
    let small = alternating_capture_work(128);
    let large = alternating_capture_work(256);
    assert!(
        small <= 128 * 4 && large <= 256 * 4 && large <= small * 2 + 4,
        "capture length work must be linear: N={small}, 2N={large}"
    );
}

#[test]
fn late_chat_tool_metadata_counts_toward_the_next_admission() {
    let mut collector = OutputCollector::new(OutputWire::OpenAiChat, 5);
    for event in [
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"ignored","function":{"name":"ignored"}}]}}]}),
        json!({"choices":[{"delta":{"content":"x"}}]}),
        json!({"choices":[{"delta":{"content":"y"}}]}),
    ] {
        feed(&mut collector, event);
    }
    let capture = collector.finish();
    assert!(capture.truncated);
    assert_eq!(
        parts(&capture),
        json!([
            {"type":"tool_call","id":"c","name":"f","arguments":{}},
            {"type":"text","content":"x"},
        ])
    );
}

#[test]
fn late_chat_tool_metadata_keeps_its_existing_over_cap_semantics() {
    let events = [
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f"}}]}}]}),
    ];
    for append_text in [false, true] {
        let mut collector = OutputCollector::new(OutputWire::OpenAiChat, 3);
        for event in &events {
            feed(&mut collector, event.clone());
        }
        if append_text {
            feed(
                &mut collector,
                json!({"choices":[{"delta":{"content":"x"}}]}),
            );
        }
        let capture = collector.finish();
        assert_eq!(capture.truncated, append_text);
        assert_eq!(
            parts(&capture),
            json!([{"type":"tool_call","id":"c","name":"f","arguments":{}}])
        );
    }
}

#[test]
fn anthropic_argument_delta_reclaims_the_empty_object_placeholder() {
    for (cap, expected_args, truncated) in [(5, json!({}), false), (4, json!(""), true)] {
        let mut collector = OutputCollector::new(OutputWire::AnthropicMessages, cap);
        feed(
            &mut collector,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"f","input":{}}}),
        );
        feed(
            &mut collector,
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{ }"}}),
        );
        let capture = collector.finish();
        assert_eq!(capture.truncated, truncated, "cap={cap}");
        assert_eq!(
            parts(&capture),
            json!([{"type":"tool_call","id":"t","name":"f","arguments":expected_args}]),
            "cap={cap}"
        );
    }
}

#[test]
fn responses_final_item_reclaims_only_its_own_slots() {
    let mut collector = OutputCollector::new(OutputWire::OpenAiResponses, 4);
    for event in [
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"ab"}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":1,"delta":"c"}),
        json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"q"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"x"}]}}),
        json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"yz"}),
        json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"!"}),
    ] {
        feed(&mut collector, event);
    }
    let capture = collector.finish();
    assert!(capture.truncated);
    assert_eq!(
        parts(&capture),
        json!([
            {"type":"text","content":"x"},
            {"type":"text","content":"qyz"},
        ])
    );
}

#[test]
fn replacing_a_part_releases_its_previous_byte_budget() {
    let mut collector = OutputCollector::new(OutputWire::OpenAiResponses, 8);
    let first = SlotKey {
        position: 0,
        sub: 0,
    };
    let second = SlotKey {
        position: 1,
        sub: 0,
    };
    collector.put(first, Part::Text("abcd".into()));
    collector.put(second, Part::Text("xy".into()));
    collector.put(first, Part::Text("a".into()));
    collector.append(second, super::unreachable_part, "12345");
    collector.append(first, super::unreachable_part, "!");
    let capture = collector.finish();
    assert!(capture.truncated);
    assert_eq!(
        parts(&capture),
        json!([
            {"type":"text","content":"a"},
            {"type":"text","content":"xy12345"},
        ])
    );
}
