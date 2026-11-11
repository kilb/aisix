//! What a price dataset's parse holds before its work limit refuses it.
//!
//! A card's tiers are each a copy of the model's standard price map with the
//! tier's own prices over it, so a small body that names many tiers beside a
//! standard map of many fields asks for their product. The parse must refuse
//! that product before it builds it: a refusal that comes only once the
//! copies exist has already spent the memory the limit is there to bound.
//!
//! The heap is counted by the allocator below, by allocated size, on the
//! test's own thread only, so this file holds one test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use aisix_control_plane::pricing::import::parse;
use chrono::{TimeZone, Utc};
use serde_json::value::RawValue;

struct CountingAllocator;

thread_local! {
    /// Whether this thread's allocations are counted.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    /// This thread's counted bytes live now, and the most there have been.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn count(delta: isize) {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = LIVE.try_with(|live| {
                let now = live.get() + delta;
                live.set(now);
                let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
            });
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size() as isize);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size() as isize);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size as isize - layout.size() as isize);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(-(layout.size() as isize));
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The most bytes `f` held at once on this thread, beyond what was live
/// when it started, and what it returned.
fn peak_of<T>(f: impl FnOnce() -> T) -> (isize, T) {
    LIVE.with(|live| live.set(0));
    PEAK.with(|peak| peak.set(0));
    COUNTING.with(|on| on.set(true));
    let out = f();
    COUNTING.with(|on| on.set(false));
    (PEAK.with(Cell::get), out)
}

/// One model whose standard prices carry `fields` fields no card meters, and
/// `tiers` service-tier variants of no price of their own: every field and
/// every tier within the parse's per-field and per-proposal limits, and a
/// body of under 100 KiB.
fn wide(fields: usize, tiers: usize) -> String {
    let standard = (0..fields)
        .map(|n| format!(r#""junk_{n:05}":1"#))
        .collect::<Vec<_>>()
        .join(",");
    let variants = (0..tiers)
        .map(|n| format!(r#"{{"when":{{"service_tier":"t{n}"}},"prices":{{}}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"[{{"id":"p","pricing_urls":[],"models":[{{"id":"wide","prices":{{"input_mtok":1,"output_mtok":2,{standard}}},"price_variants":[{variants}]}}]}}]"#
    )
}

#[test]
fn a_dataset_whose_tiers_multiply_out_is_refused_before_they_are_copied() {
    let now = Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap();
    // 501 tiers of 5,002 fields each: 2.5 million price entries to copy, past
    // the work limit, from a body of about 80 KiB.
    let text = wide(5_000, 500);
    assert!(text.len() < 100 << 10, "{} bytes", text.len());
    let body = RawValue::from_string(text).expect("valid JSON");
    let (peak, parsed) = peak_of(|| parse(&body, now));
    let err = parsed.expect_err("refused");
    assert_eq!(err.status.as_u16(), 422, "{err:?}");
    assert_eq!(
        err.details
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/data"]
    );
    assert!(err.details[0].message.contains("过多"), "{err:?}");
    // The body's own parse, a few MiB at most, and none of the copies.
    assert!(
        peak < 16 << 20,
        "the parse held {} MiB before it refused the body",
        peak >> 20
    );

    // The same shape a size smaller is a dataset like any other.
    let body = RawValue::from_string(wide(20, 5)).expect("valid JSON");
    let all = parse(&body, now).expect("a dataset");
    assert_eq!(all.len(), 1);
    let card = all[0].card.as_ref().expect("a card");
    assert_eq!(card.0.len(), 6, "the default tier and five named ones");
}

/// One provider whose pricing page is `url_bytes` long, with one model whose
/// prices change on `dates` later days: `dates + 1` proposals, each copying
/// the whole page.
fn repeated_page(url_bytes: usize, dates: usize) -> String {
    let url = format!("https://p.example/{}", "a".repeat(url_bytes));
    let entries = std::iter::once(r#"{"prices":{"input_mtok":1,"output_mtok":2}}"#.to_string())
        .chain((1..=dates).map(|n| {
            format!(
                r#"{{"constraint":{{"start_date":"{}"}},"prices":{{"input_mtok":2,"output_mtok":3}}}}"#,
                chrono::NaiveDate::from_ymd_opt(2026, 12, 1)
                    .expect("a date")
                    .checked_add_days(chrono::Days::new(n as u64))
                    .expect("a date")
            )
        }))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"[{{"id":"p","pricing_urls":["{url}"],"models":[{{"id":"repeated","prices":[{entries}]}}]}}]"#
    )
}

/// A page repeated over many dates is refused before the parse copies it into
/// every proposal: a body of about 1 MiB whose page is 1 MiB long, copied into
/// 128 proposals, would retain 128 MiB. The body itself is held, and none of
/// the copies.
#[test]
fn a_page_repeated_over_many_dates_is_refused_before_it_is_copied() {
    let now = Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap();
    let text = repeated_page(1 << 20, 127);
    let body = RawValue::from_string(text.clone()).expect("valid JSON");
    let (peak, parsed) = peak_of(|| parse(&body, now));
    let err = parsed.expect_err("refused");
    assert_eq!(err.status.as_u16(), 422, "{err:?}");
    assert_eq!(
        err.details
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/data"]
    );
    assert!(
        err.details[0].message.contains("文字过多"),
        "the refusal names the copied text: {err:?}"
    );
    // The body, the raw value it was turned into, and the decoded providers
    // that borrow from it: a few times 1 MiB, not its 128 copies.
    assert!(
        peak < 16 << 20,
        "the parse held {} MiB before it refused the body",
        peak >> 20
    );

    // The same page over no later date is a dataset like any other.
    let body = RawValue::from_string(repeated_page(1 << 20, 0)).expect("valid JSON");
    let all = parse(&body, now).expect("a dataset");
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0].pricing_url.as_ref().map(String::len),
        Some((1 << 20) + "https://p.example/".len())
    );
}
