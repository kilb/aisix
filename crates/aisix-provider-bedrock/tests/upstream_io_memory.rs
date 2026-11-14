//! 只统计已经存在的 SDK payload 进入审计适配器之后新增的分配。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use aisix_gateway::upstream_io::{Protocol, UpstreamIoObserver};
use aws_smithy_types::{Document, Number};

// 直接编译生产适配器，保持其私有 API；不把测试入口暴露给产品调用方。
#[allow(dead_code)]
#[path = "../src/upstream_io.rs"]
mod sdk_io;

struct CountingAllocator;

thread_local! {
    static ALLOCATED: Cell<Option<usize>> = const { Cell::new(None) };
}

fn count(bytes: usize) {
    let _ = ALLOCATED.try_with(|allocated| {
        if let Some(previous) = allocated.get() {
            allocated.set(Some(previous.saturating_add(bytes)));
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(size);
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocated_by(run: impl FnOnce()) -> usize {
    ALLOCATED.with(|allocated| allocated.set(Some(0)));
    run();
    ALLOCATED.with(|allocated| allocated.replace(None).unwrap())
}

#[test]
fn large_native_event_counts_text_and_ignores_large_unknown_field_without_copying() {
    let repeats = 1 << 18;
    let text = "A汉😀".repeat(repeats);
    let unknown = "x".repeat(4 << 20);
    let event = format!(
        r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}},"unknown":"{unknown}"}}"#
    );
    assert!(event.len() > 6 << 20);
    let io = UpstreamIoObserver::new(Protocol::Messages);
    io.begin_response(Some("application/vnd.amazon.eventstream"));
    let allocated = allocated_by(|| {
        sdk_io::observe_native_event(&io, event.as_bytes());
        sdk_io::observe_native_event(&io, br#"{"type":"message_stop"}"#);
    });
    let audit = io.snapshot();
    assert_eq!(audit.response_body_bytes, None);
    let measured = audit.output_text.unwrap();
    assert_eq!(measured.utf8_bytes, (8 * repeats) as u64);
    assert_eq!(measured.unicode_scalars, (3 * repeats) as u64);
    assert!(measured.complete);
    assert!(
        allocated < 128 << 10,
        "audit allocated {allocated} bytes for a borrowed {}-byte event",
        event.len()
    );
}

#[test]
fn large_converse_document_serializes_to_scalar_sink_without_copying_tree_or_string() {
    let value = "\n\"A汉😀".repeat(1 << 18);
    let expected = serde_json::to_string(&serde_json::json!({
        "value":value,
        "other":[7,-3,0.5,true,null]
    }))
    .unwrap();
    let document = Document::Object(std::collections::HashMap::from([
        ("value".to_owned(), Document::String(value)),
        (
            "other".to_owned(),
            Document::Array(vec![
                Document::Number(Number::PosInt(7)),
                Document::Number(Number::NegInt(-3)),
                Document::Number(Number::Float(0.5)),
                Document::Bool(true),
                Document::Null,
            ]),
        ),
    ]));
    assert!(expected.len() > 3 << 20);
    let io = UpstreamIoObserver::new(Protocol::Converse);
    io.begin_response(Some("application/vnd.amazon.eventstream"));
    let allocated = allocated_by(|| sdk_io::observe_document(&io, &document));
    io.finish_output_text();
    let audit = io.snapshot();
    assert_eq!(audit.response_body_bytes, None);
    let measured = audit.output_text.unwrap();
    assert_eq!(measured.utf8_bytes, expected.len() as u64);
    assert_eq!(measured.unicode_scalars, expected.chars().count() as u64);
    assert!(measured.complete);
    assert!(
        allocated < 128 << 10,
        "audit allocated {allocated} bytes for a borrowed {}-byte JSON document",
        expected.len()
    );
}
