//! Usage intake: `POST /dp/telemetry` stores each event once, whichever
//! batch carries it and however often a batch is re-sent, and answers every
//! request with `X-Aisix-Usage-Batch-Dedup: 1`, the header that tells the
//! gateway a re-send cannot count twice. Operators read what was stored
//! through `/admin/usage/*`.
//!
//! Batches have the shape the gateway sends (`TelemetryBody` in
//! `crates/aisix-server/src/telemetry.rs`), and events the fields of its
//! `UsageEvent` (`crates/aisix-obs/src/usage.rs`).

mod support;

use std::sync::Arc;
use std::time::Duration;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::dp::telemetry::{MAX_BODIES_IN_FLIGHT, MAX_EVENTS_PER_BATCH, SLOT_WAIT};
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::{DpClient, Resp, TestCp};
use tokio::sync::Barrier;
use uuid::Uuid;

const DEDUP: &str = "x-aisix-usage-batch-dedup";

// The cap leaves room above the gateway's own largest batch, 100 events
// (`MAX_BATCH` in `crates/aisix-server/src/telemetry.rs`).
const _: () = assert!(MAX_EVENTS_PER_BATCH >= 1000);

// A delivery waiting for a body slot is answered before the gateway's own
// 10 s request timeout.
const _: () = assert!(SLOT_WAIT.as_secs() < 10);

/// A usage event as the gateway sends it.
fn event(event_id: &str) -> Value {
    json!({
        "request_id": format!("req-{event_id}"),
        "event_id": event_id,
        "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
        "occurred_at": "2026-11-06T08:15:30.123Z",
        "model_id": "6f0e8c1a-2b3c-4d5e-8f90-a1b2c3d4e5f6",
        "api_key_id": "key-7c1",
        "team_id": "team-7c1",
        "requested_model": "gpt-4o-7c1",
        "requested_model_id": "6f0e8c1a-2b3c-4d5e-8f90-a1b2c3d4e5f6",
        "prompt_tokens": 12,
        "completion_tokens": 34,
        "upstream_latency_ms": 250,
        "status_code": 200,
        "cost_usd": 0.0012,
        "guardrail_blocked": false,
    })
}

/// A batch body carrying one event per id.
fn batch(ids: &[&str]) -> Value {
    json!({ "events": ids.iter().map(|id| event(id)).collect::<Vec<_>>() })
}

fn answer(r: &Resp) -> (u16, Value) {
    (r.status, r.body.clone())
}

fn counts(accepted: u64, rejected: u64, duplicates: u64) -> Value {
    json!({"accepted": accepted, "rejected": rejected, "duplicates": duplicates})
}

/// `(events, inserted, duplicates, rejected, deliveries)` of the batch row.
async fn batch_row(cp: &TestCp, batch_id: Uuid) -> Option<(i64, i64, i64, i64, i64)> {
    sqlx::query_as(
        "SELECT events, inserted, duplicates, rejected, deliveries FROM usage_batches \
         WHERE batch_id = $1",
    )
    .bind(batch_id)
    .fetch_optional(cp.db().pool())
    .await
    .expect("read the batch row")
}

/// `(batch_id, team_id, api_key_id, request_id, occurred_at, event)` of the
/// stored event `event_id`.
async fn stored(
    cp: &TestCp,
    event_id: &str,
) -> (
    Uuid,
    Option<String>,
    Option<String>,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    Value,
) {
    sqlx::query_as(
        "SELECT batch_id, team_id, api_key_id, request_id, occurred_at, event \
         FROM usage_events WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(cp.db().pool())
    .await
    .unwrap_or_else(|e| panic!("read the stored event {event_id}: {e}"))
}

/// Nothing at all was stored: no batch, no event, no reject.
async fn assert_nothing_stored(cp: &TestCp) {
    for table in ["usage_batches", "usage_events", "usage_rejects"] {
        assert_eq!(cp.count(table).await, 0, "{table}");
    }
}

#[tokio::test]
async fn a_batch_is_stored_once_and_every_answer_carries_the_dedup_header() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw").await;
    let (b, body) = (Uuid::new_v4(), batch(&["e1", "e2"]));
    let r1 = dp.telemetry(b, &body).await;
    let r2 = dp.telemetry(b, &body).await;
    assert_eq!((r1.status, r1.body["accepted"].as_u64()), (200, Some(2)));
    assert_eq!((r2.status, r2.body["accepted"].as_u64()), (200, Some(0)));
    assert!([&r1, &r2]
        .iter()
        .all(|r| r.header("x-aisix-usage-batch-dedup") == Some("1")));
    assert_eq!(cp.count("usage_events").await, 2);

    // The whole answers: a re-sent batch is recognised as one, so its
    // events are not counted as duplicates either.
    assert_eq!(answer(&r1), (200, counts(2, 0, 0)));
    assert_eq!(answer(&r2), (200, counts(0, 0, 0)));
    assert_eq!(batch_row(&cp, b).await, Some((2, 2, 0, 0, 2)));
    // Each event is kept as sent, with its filter columns copied out.
    let (batch_id, team, key, request, occurred_at, kept) = stored(&cp, "e1").await;
    assert_eq!(batch_id, b);
    assert_eq!(team.as_deref(), Some("team-7c1"));
    assert_eq!(key.as_deref(), Some("key-7c1"));
    assert_eq!(request.as_deref(), Some("req-e1"));
    assert_eq!(occurred_at.to_rfc3339(), "2026-11-06T08:15:30.123+00:00");
    assert_eq!(kept, event("e1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deliveries_of_one_batch_commit_once() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = Arc::new(cp.issue_dp_client("gw-usage-7c1").await);
    let (b, body) = (Uuid::new_v4(), batch(&["c1", "c2", "c3"]));
    let racers = 8;
    let barrier = Arc::new(Barrier::new(racers));
    let deliveries: Vec<_> = (0..racers)
        .map(|_| {
            let (dp, barrier, body) = (dp.clone(), barrier.clone(), body.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                dp.telemetry(b, &body).await
            })
        })
        .collect();
    let mut answers = Vec::new();
    for delivery in deliveries {
        answers.push(delivery.await.expect("a delivery task"));
    }
    assert!(
        answers
            .iter()
            .all(|r| r.status == 200 && r.header(DEDUP) == Some("1")),
        "{answers:?}"
    );
    let stored_by: Vec<&Resp> = answers
        .iter()
        .filter(|r| r.body == counts(3, 0, 0))
        .collect();
    let recognised = answers.iter().filter(|r| r.body == counts(0, 0, 0)).count();
    assert_eq!(
        (stored_by.len(), recognised),
        (1, racers - 1),
        "{answers:?}"
    );
    assert_eq!(cp.count("usage_events").await, 3);
    assert_eq!(batch_row(&cp, b).await, Some((3, 3, 0, 0, racers as i64)));
}

/// Waiting locks of `locktype` in this test's database.
async fn waiting(cp: &TestCp, locktype: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a USING (pid) \
         WHERE NOT l.granted AND l.locktype = $1 AND a.datname = current_database()",
    )
    .bind(locktype)
    .fetch_one(cp.db().pool())
    .await
    .expect("read pg_locks")
}

async fn wait_for_waiting(cp: &TestCp, locktype: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while waiting(cp, locktype).await < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing waits on a {locktype} lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The race the Barrier test can only hope to hit, forced: the first
/// delivery has written its batch row and is held before it commits, and
/// a second delivery of the same batch arrives meanwhile. The second waits
/// for the first and, once it has committed, is answered as a re-send: it
/// stores nothing and counts one more delivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_arriving_while_the_first_is_still_committing_is_answered_as_a_resend() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = Arc::new(cp.issue_dp_client("gw-usage-7c1").await);
    let (b, body) = (Uuid::new_v4(), batch(&["w1", "w2"]));
    // Holding the event table stops the first delivery after its batch row.
    let mut holder = PgConnection::connect(cp.database_url())
        .await
        .expect("connect");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("LOCK TABLE usage_events IN EXCLUSIVE MODE")
        .execute(&mut holder)
        .await
        .expect("lock the event table");
    let send =
        |dp: Arc<DpClient>, body: Value| tokio::spawn(async move { dp.telemetry(b, &body).await });
    let first = send(dp.clone(), body.clone());
    wait_for_waiting(&cp, "relation").await;
    let second = send(dp.clone(), body.clone());
    // It waits on the first delivery's transaction, not on the table.
    wait_for_waiting(&cp, "transactionid").await;
    sqlx::query("COMMIT")
        .execute(&mut holder)
        .await
        .expect("release the event table");
    let (first, second) = (
        first.await.expect("the first delivery"),
        second.await.expect("the second delivery"),
    );
    assert_eq!(answer(&first), (200, counts(2, 0, 0)), "{first:?}");
    assert_eq!(answer(&second), (200, counts(0, 0, 0)), "{second:?}");
    assert!([&first, &second]
        .iter()
        .all(|r| r.header(DEDUP) == Some("1")));
    assert_eq!(cp.count("usage_events").await, 2);
    assert_eq!(batch_row(&cp, b).await, Some((2, 2, 0, 0, 2)));
}

#[tokio::test]
async fn an_event_seen_in_an_earlier_batch_is_a_duplicate_not_a_second_row() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let r = dp.telemetry(a, &batch(&["d1", "d2"])).await;
    assert_eq!(answer(&r), (200, counts(2, 0, 0)));
    let r = dp.telemetry(b, &batch(&["d2", "d3"])).await;
    assert_eq!(answer(&r), (200, counts(1, 0, 1)));
    assert_eq!(r.header(DEDUP), Some("1"));
    assert_eq!(cp.count("usage_events").await, 3);
    // The row stays the one first stored.
    assert_eq!(stored(&cp, "d2").await.0, a);
    assert_eq!(batch_row(&cp, b).await, Some((2, 1, 1, 0, 1)));

    // The same event twice in one batch is stored once too.
    let c = Uuid::new_v4();
    let r = dp.telemetry(c, &batch(&["d4", "d4"])).await;
    assert_eq!(answer(&r), (200, counts(1, 0, 1)));
    assert_eq!(cp.count("usage_events").await, 4);
    assert_eq!(batch_row(&cp, c).await, Some((2, 1, 1, 0, 1)));
}

#[tokio::test]
async fn an_event_without_an_event_id_is_rejected_and_kept_for_operators() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let mut no_id = event("x");
    no_id.as_object_mut().expect("an object").remove("event_id");
    let mut empty_id = event("x");
    empty_id["event_id"] = json!("");
    let mut numeric_id = event("x");
    numeric_id["event_id"] = json!(7);
    let too_long = event(&"l".repeat(129));
    let longest = "k".repeat(128);
    let mut bad_time = event("bad-time-7c1");
    bad_time["occurred_at"] = json!("2026-11-06 08:15:30");
    let mut no_time = event("no-time-7c1");
    no_time
        .as_object_mut()
        .expect("an object")
        .remove("occurred_at");
    let not_object = json!("not-an-event-7c1");
    let events = vec![
        event("kept-7c1"),
        no_id.clone(),
        empty_id.clone(),
        numeric_id.clone(),
        too_long.clone(),
        event(&longest),
        bad_time.clone(),
        no_time.clone(),
        not_object.clone(),
    ];
    let b = Uuid::new_v4();
    let r = dp.telemetry(b, &json!({ "events": events })).await;
    assert_eq!(answer(&r), (200, counts(2, 7, 0)), "{r:?}");
    assert_eq!(r.header(DEDUP), Some("1"));
    assert_eq!(cp.count("usage_events").await, 2);
    stored(&cp, "kept-7c1").await;
    stored(&cp, &longest).await;
    assert_eq!(batch_row(&cp, b).await, Some((9, 2, 0, 7, 1)));

    // Each reject is kept as sent, with why, for operators to look into.
    let rejects: Vec<(Uuid, String, Value)> =
        sqlx::query_as("SELECT batch_id, reason, raw FROM usage_rejects ORDER BY seq")
            .fetch_all(cp.db().pool())
            .await
            .expect("read the rejects");
    let expected = [
        ("event_id_missing", no_id),
        ("event_id_missing", empty_id),
        ("event_id_missing", numeric_id),
        ("event_id_too_long", too_long),
        ("occurred_at_invalid", bad_time),
        ("occurred_at_invalid", no_time),
        ("not_an_object", not_object),
    ];
    assert_eq!(rejects.len(), expected.len(), "{rejects:?}");
    for ((batch_id, reason, raw), (want_reason, want_raw)) in rejects.iter().zip(expected) {
        assert_eq!(*batch_id, b);
        assert_eq!((reason.as_str(), raw), (want_reason, &want_raw));
    }

    // Sent again, the batch is recognised: no second set of rejects. The
    // answer repeats how many were refused, so a gateway whose first
    // answer was lost still learns that those events are gone.
    let r = dp.telemetry(b, &json!({ "events": events })).await;
    assert_eq!(answer(&r), (200, counts(0, 7, 0)));
    assert_eq!(cp.count("usage_rejects").await, 7);
    assert_eq!(batch_row(&cp, b).await, Some((9, 2, 0, 7, 2)));
}

#[tokio::test]
async fn a_caller_chosen_request_id_never_merges_two_events() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let mut first = event("r1");
    first["request_id"] = json!("dup-7c1");
    let mut second = event("r2");
    second["request_id"] = json!("dup-7c1");
    let r = dp
        .telemetry(Uuid::new_v4(), &json!({ "events": [first] }))
        .await;
    assert_eq!(answer(&r), (200, counts(1, 0, 0)));
    let r = dp
        .telemetry(Uuid::new_v4(), &json!({ "events": [second] }))
        .await;
    assert_eq!(answer(&r), (200, counts(1, 0, 0)));
    assert_eq!(cp.count("usage_events").await, 2);

    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let r = cp
        .get_with_cookie("/admin/usage/events?request_id=dup-7c1", viewer.cookie())
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let mut ids: Vec<&str> = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["event_id"].as_str().expect("an event_id"))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["r1", "r2"]);
}

#[tokio::test]
async fn a_missing_batch_id_is_400_with_the_dedup_header() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let body = serde_json::to_vec(&batch(&["m1"])).expect("serialize");
    for batch_id in [None, Some(""), Some("not-a-uuid-7c1")] {
        let r = dp
            .telemetry_raw(batch_id, body.clone())
            .await
            .expect("an answer");
        assert_eq!(r.status, 400, "batch id {batch_id:?}: {r:?}");
        assert_eq!(r.header(DEDUP), Some("1"), "batch id {batch_id:?}");
    }
    assert_nothing_stored(&cp).await;
}

#[tokio::test]
async fn a_malformed_body_is_400_with_the_dedup_header() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    for body in [
        &b"not json-7c1"[..],
        b"{\"events\": [",
        b"{}",
        b"{\"events\": null}",
        b"{\"events\": {\"event_id\": \"m1\"}}",
        b"[]",
        // A parser's message would quote this string.
        br#"{"events": "echo-7c1"}"#,
    ] {
        let r = dp
            .telemetry_raw(Some(&Uuid::new_v4().to_string()), body.to_vec())
            .await
            .expect("an answer");
        let shown = String::from_utf8_lossy(body);
        assert_eq!(r.status, 400, "{shown}: {r:?}");
        assert_eq!(r.header(DEDUP), Some("1"), "{shown}");
        // The answer does not quote the body back.
        let answered = r.body.to_string();
        assert!(!answered.contains("echo-7c1"), "{r:?}");
        assert!(!answered.contains("7c1"), "{r:?}");
    }
    assert_nothing_stored(&cp).await;

    // An empty batch is well formed: accepted, with nothing in it.
    let b = Uuid::new_v4();
    let r = dp.telemetry(b, &json!({"events": []})).await;
    assert_eq!(answer(&r), (200, counts(0, 0, 0)));
    assert_eq!(batch_row(&cp, b).await, Some((0, 0, 0, 0, 1)));
}

/// An event padded to about `bytes`.
fn padded(event_id: &str, bytes: usize) -> Value {
    let mut e = event(event_id);
    e["pad_7c1"] = json!("p".repeat(bytes));
    e
}

#[tokio::test]
async fn a_body_over_eight_mebibytes_is_413_with_the_dedup_header_and_a_three_mebibyte_batch_is_accepted(
) {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let big = json!({ "events": [padded("big-7c1", (8 << 20) + 1024)] });
    let r = dp
        .telemetry_raw(
            Some(&Uuid::new_v4().to_string()),
            serde_json::to_vec(&big).expect("serialize"),
        )
        .await
        .expect("an answer");
    assert_eq!(r.status, 413, "{}", r.status);
    assert_eq!(r.body["code"], "payload_too_large");
    assert_eq!(r.header(DEDUP), Some("1"));
    assert_nothing_stored(&cp).await;

    // Above the 1 MiB the gateway listener's other routes take, and above
    // a default 2 MiB.
    let three = json!({ "events": [
        padded("three-1", 1 << 20),
        padded("three-2", 1 << 20),
        padded("three-3", 1 << 20),
    ] });
    let r = dp.telemetry(Uuid::new_v4(), &three).await;
    assert_eq!(answer(&r), (200, counts(3, 0, 0)), "{}", r.status);
    assert_eq!(stored(&cp, "three-2").await.5, padded("three-2", 1 << 20));
}

#[tokio::test]
async fn a_batch_over_the_event_cap_is_413_and_one_at_the_cap_is_accepted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let ids: Vec<String> = (0..=MAX_EVENTS_PER_BATCH)
        .map(|i| format!("cap-{i}"))
        .collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let r = dp.telemetry(Uuid::new_v4(), &batch(&ids)).await;
    assert_eq!(r.status, 413, "{r:?}");
    assert_eq!(r.body["code"], "payload_too_large");
    assert_eq!(r.header(DEDUP), Some("1"));
    assert_nothing_stored(&cp).await;

    let at_cap = &ids[..MAX_EVENTS_PER_BATCH];
    let r = dp.telemetry(Uuid::new_v4(), &batch(at_cap)).await;
    assert_eq!(answer(&r), (200, counts(MAX_EVENTS_PER_BATCH as u64, 0, 0)));
    assert_eq!(cp.count("usage_events").await, MAX_EVENTS_PER_BATCH as i64);
}

#[tokio::test]
async fn an_event_is_kept_whole_whatever_else_it_carries() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    // Only event_id and occurred_at: the filter columns stay empty.
    let bare = json!({"event_id": "bare-7c1", "occurred_at": "2026-11-06T08:15:30+08:00"});
    // Fields this build does not know, and filter fields that are no
    // usable text.
    let odd = json!({
        "event_id": "odd-7c1",
        "occurred_at": "2026-11-06T00:15:30.5Z",
        "team_id": 42,
        "api_key_id": "",
        "request_id": null,
        "future_field_7c1": {"nested": [1, 2, {"deeper": true}]},
        "prompt_tokens": 1,
    });
    let r = dp
        .telemetry(
            Uuid::new_v4(),
            &json!({"events": [bare, odd], "future_7c1": 1}),
        )
        .await;
    assert_eq!(answer(&r), (200, counts(2, 0, 0)), "{r:?}");
    let (_, team, key, request, occurred_at, kept) = stored(&cp, "bare-7c1").await;
    assert_eq!((team, key, request), (None, None, None));
    assert_eq!(occurred_at.to_rfc3339(), "2026-11-06T00:15:30+00:00");
    assert_eq!(kept, bare);
    let (_, team, key, request, _, kept) = stored(&cp, "odd-7c1").await;
    assert_eq!((team, key, request), (None, None, None));
    assert_eq!(kept, odd);
}

/// Text Postgres cannot hold or index must not fail the batch it arrives
/// in, which would be re-sent and refused until the gateway gives the
/// whole batch up.
#[tokio::test]
async fn an_event_postgres_cannot_store_as_sent_does_not_fail_its_batch() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    // A NUL, in a value and in a key: neither jsonb nor text holds one.
    let mut nul = event("nul-7c1");
    nul["finish_reason"] = json!("stop\u{0}7c1");
    nul["tags"] = json!({"k\u{0}7c1": "v"});
    // A request id far longer than any index entry.
    let long_request_id: String = (0..640)
        .map(|_| Uuid::new_v4().simple().to_string())
        .collect();
    let mut long = event("long-7c1");
    long["request_id"] = json!(long_request_id);
    let longest_kept = "q".repeat(256);
    let mut kept_whole = event("kept-whole-7c1");
    kept_whole["request_id"] = json!(longest_kept);
    let r = dp
        .telemetry(
            Uuid::new_v4(),
            &json!({"events": [nul, long, kept_whole], }),
        )
        .await;
    assert_eq!(answer(&r), (200, counts(3, 0, 0)), "{r:?}");

    let (_, _, _, _, _, kept) = stored(&cp, "nul-7c1").await;
    assert_eq!(kept["finish_reason"], "stop\u{fffd}7c1");
    assert_eq!(kept["tags"], json!({"k\u{fffd}7c1": "v"}));
    let (_, _, _, request, _, kept) = stored(&cp, "long-7c1").await;
    assert_eq!(request, None, "too long to filter on");
    assert_eq!(kept["request_id"], long_request_id.as_str());
    let (_, _, _, request, _, _) = stored(&cp, "kept-whole-7c1").await;
    assert_eq!(request.as_deref(), Some(longest_kept.as_str()));
}

/// Two batches sharing events, each in the other's reverse order, sent at
/// once, again and again: each event is stored once and neither batch
/// fails, which a deadlock between them would make it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batches_sharing_events_in_opposite_orders_never_deadlock() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = Arc::new(cp.issue_dp_client("gw-usage-7c1").await);
    for round in 0..10 {
        let ids: Vec<String> = (0..200).map(|i| format!("o{round}-{i:03}")).collect();
        let forward: Vec<&str> = ids.iter().map(String::as_str).collect();
        let backward: Vec<&str> = forward.iter().rev().copied().collect();
        let barrier = Arc::new(Barrier::new(2));
        let sends: Vec<_> = [batch(&forward), batch(&backward)]
            .into_iter()
            .map(|body| {
                let (dp, barrier) = (dp.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    dp.telemetry(Uuid::new_v4(), &body).await
                })
            })
            .collect();
        let mut answers = Vec::new();
        for send in sends {
            answers.push(send.await.expect("a send task"));
        }
        assert!(
            answers.iter().all(|r| r.status == 200),
            "round {round}: {answers:?}"
        );
        let total = |field: &str| -> u64 {
            answers
                .iter()
                .map(|r| r.body[field].as_u64().expect("a count"))
                .sum()
        };
        assert_eq!(
            (total("accepted"), total("duplicates")),
            (200, 200),
            "round {round}"
        );
    }
    assert_eq!(cp.count("usage_events").await, 2000);
}

/// Makes every insert into `usage_events` fail with SQLSTATE `code`.
async fn fail_event_inserts_with(cp: &TestCp, code: &str) {
    let pool = cp.db().pool();
    for sql in [
        format!(
            "CREATE OR REPLACE FUNCTION fail_7c1() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'injected-7c1' USING ERRCODE = '{code}'; END $$"
        ),
        "CREATE OR REPLACE TRIGGER fail_7c1 BEFORE INSERT ON usage_events \
         FOR EACH ROW EXECUTE FUNCTION fail_7c1()"
            .to_string(),
    ] {
        sqlx::query(&sql).execute(pool).await.expect(&sql);
    }
}

async fn stop_failing_event_inserts(cp: &TestCp) {
    sqlx::query("DROP TRIGGER fail_7c1 ON usage_events")
        .execute(cp.db().pool())
        .await
        .expect("drop the trigger");
}

/// A batch with two events and one reject.
fn failing_batch() -> Value {
    let mut events = vec![event("f1"), event("f2")];
    events.push(json!({"occurred_at": "2026-11-06T08:15:30Z"}));
    json!({ "events": events })
}

/// `r` answers a failed store with `status` and `code`, the dedup header,
/// and nothing of what caused it or what the batch carried.
fn assert_store_failure(r: &Resp, status: u16, code: &str, sqlstate: &str) {
    assert_eq!(r.status, status, "SQLSTATE {sqlstate}: {r:?}");
    assert_eq!(r.body["code"], code, "SQLSTATE {sqlstate}");
    assert_eq!(r.header(DEDUP), Some("1"), "SQLSTATE {sqlstate}");
    for token in ["injected-7c1", sqlstate, "req-f1", "team-7c1"] {
        assert!(!r.body.to_string().contains(token), "{token}: {r:?}");
    }
}

/// A failure that sending the batch again can get past (a lost connection,
/// a serialisation failure, an exhausted server, a shutdown, a read-only
/// failover target) is a 503, which the gateway re-sends.
#[tokio::test]
async fn a_database_failure_is_503_with_the_dedup_header_and_stores_nothing() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let (b, body) = (Uuid::new_v4(), failing_batch());
    for sqlstate in [
        "57P01", "40001", "40P01", "08006", "53300", "57014", "25006",
    ] {
        fail_event_inserts_with(&cp, sqlstate).await;
        let r = dp.telemetry(b, &body).await;
        assert_store_failure(&r, 503, "unavailable", sqlstate);
        // Not even the batch id or the rejected event: the batch can be
        // sent again.
        assert_nothing_stored(&cp).await;
    }

    stop_failing_event_inserts(&cp).await;
    let r = dp.telemetry(b, &body).await;
    assert_eq!(answer(&r), (200, counts(2, 1, 0)), "{r:?}");
    assert_eq!(batch_row(&cp, b).await, Some((3, 2, 0, 1, 1)));
}

/// Any other failure would fail the same way on every re-send: a 500,
/// with a neutral message.
#[tokio::test]
async fn a_permanent_database_failure_is_500_with_the_dedup_header_and_stores_nothing() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let (b, body) = (Uuid::new_v4(), failing_batch());
    for sqlstate in ["P0001", "23505", "22P05", "42P01", "54000"] {
        fail_event_inserts_with(&cp, sqlstate).await;
        let r = dp.telemetry(b, &body).await;
        assert_store_failure(&r, 500, "internal", sqlstate);
        assert_nothing_stored(&cp).await;
    }

    stop_failing_event_inserts(&cp).await;
    let r = dp.telemetry(b, &body).await;
    assert_eq!(answer(&r), (200, counts(2, 1, 0)), "{r:?}");
}

/// The other half of the forced race: the first delivery fails after the
/// second has started waiting on it, and the second stores the batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_waiting_on_one_that_fails_stores_the_batch_itself() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = Arc::new(cp.issue_dp_client("gw-usage-7c1").await);
    let (b, body) = (Uuid::new_v4(), batch(&["a1", "a2"]));
    // The first event insert fails, once, with a failure worth a re-send.
    for sql in [
        "CREATE SEQUENCE fail_once_7c1",
        "CREATE FUNCTION fail_once_7c1() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN IF nextval('fail_once_7c1') = 1 THEN \
              RAISE EXCEPTION 'injected-7c1' USING ERRCODE = '40001'; \
            END IF; RETURN NEW; END $$",
        "CREATE TRIGGER fail_once_7c1 BEFORE INSERT ON usage_events \
         FOR EACH ROW EXECUTE FUNCTION fail_once_7c1()",
    ] {
        sqlx::query(sql).execute(cp.db().pool()).await.expect(sql);
    }
    let mut holder = PgConnection::connect(cp.database_url())
        .await
        .expect("connect");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("LOCK TABLE usage_events IN EXCLUSIVE MODE")
        .execute(&mut holder)
        .await
        .expect("lock the event table");
    let send =
        |dp: Arc<DpClient>, body: Value| tokio::spawn(async move { dp.telemetry(b, &body).await });
    let first = send(dp.clone(), body.clone());
    wait_for_waiting(&cp, "relation").await;
    let second = send(dp.clone(), body.clone());
    wait_for_waiting(&cp, "transactionid").await;
    sqlx::query("COMMIT")
        .execute(&mut holder)
        .await
        .expect("release the event table");
    let (first, second) = (
        first.await.expect("the first delivery"),
        second.await.expect("the second delivery"),
    );
    assert_eq!(first.status, 503, "{first:?}");
    assert_eq!(answer(&second), (200, counts(2, 0, 0)), "{second:?}");
    assert!([&first, &second]
        .iter()
        .all(|r| r.header(DEDUP) == Some("1")));
    assert_eq!(cp.count("usage_events").await, 2);
    // The failed delivery left no trace.
    assert_eq!(batch_row(&cp, b).await, Some((2, 2, 0, 0, 1)));
}

/// A slot of the bodies one control plane holds at once.
fn slots(cp: &TestCp) -> Arc<tokio::sync::Semaphore> {
    cp.state().telemetry_bodies.clone()
}

#[tokio::test]
async fn a_delivery_finding_every_body_slot_taken_waits_then_answers_503() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let held = slots(&cp)
        .acquire_many_owned(MAX_BODIES_IN_FLIGHT as u32)
        .await
        .expect("every slot");
    let started = tokio::time::Instant::now();
    let r = dp.telemetry(Uuid::new_v4(), &batch(&["s1"])).await;
    let waited = started.elapsed();
    assert_eq!(r.status, 503, "{r:?}");
    assert_eq!(r.body["code"], "unavailable");
    assert_eq!(r.header(DEDUP), Some("1"));
    // After the full wait.
    assert!(waited >= SLOT_WAIT, "answered after {waited:?}");
    assert_nothing_stored(&cp).await;

    drop(held);
    let r = dp.telemetry(Uuid::new_v4(), &batch(&["s1"])).await;
    assert_eq!(answer(&r), (200, counts(1, 0, 0)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_waiting_for_a_body_slot_is_stored_once_one_frees() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = Arc::new(cp.issue_dp_client("gw-usage-7c1").await);
    let _rest = slots(&cp)
        .acquire_many_owned(MAX_BODIES_IN_FLIGHT as u32 - 1)
        .await
        .expect("all slots but one");
    let last = slots(&cp).acquire_owned().await.expect("the last slot");
    let b = Uuid::new_v4();
    let delivery = {
        let dp = dp.clone();
        tokio::spawn(async move { dp.telemetry(b, &batch(&["s2"])).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!delivery.is_finished(), "answered with no slot free");
    assert_nothing_stored(&cp).await;

    drop(last);
    let r = delivery.await.expect("the delivery");
    assert_eq!(answer(&r), (200, counts(1, 0, 0)), "{r:?}");
    assert_eq!(batch_row(&cp, b).await, Some((1, 1, 0, 0, 1)));
}

/// The slot goes with the store, not with the request: a gateway that
/// stops waiting for its answer does not free a slot whose batch is still
/// held while it is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_slot_is_held_until_its_store_ends_even_once_the_request_is_gone() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let dp = Arc::new(cp.issue_dp_client("gw-usage-7c1").await);
    let mut holder = PgConnection::connect(cp.database_url())
        .await
        .expect("connect");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("LOCK TABLE usage_events IN EXCLUSIVE MODE")
        .execute(&mut holder)
        .await
        .expect("lock the event table");
    let delivery = {
        let dp = dp.clone();
        tokio::spawn(async move { dp.telemetry(Uuid::new_v4(), &batch(&["g1"])).await })
    };
    wait_for_waiting(&cp, "relation").await;
    assert_eq!(slots(&cp).available_permits(), MAX_BODIES_IN_FLIGHT - 1);
    // The gateway gives up on the answer and its connection closes.
    delivery.abort();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        slots(&cp).available_permits(),
        MAX_BODIES_IN_FLIGHT - 1,
        "the slot was freed while its batch was still being stored"
    );

    sqlx::query("COMMIT")
        .execute(&mut holder)
        .await
        .expect("release the event table");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while slots(&cp).available_permits() < MAX_BODIES_IN_FLIGHT {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slot is still taken"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The store finished although nobody waited for its answer.
    assert_eq!(cp.count("usage_events").await, 1);
}

/// With every connection of the database pool busy, a delivery is answered
/// 503 before the gateway's 10 s timeout, not left to time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_that_cannot_get_a_database_connection_answers_503_in_time() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    // The client times out after 10 s, as the gateway's does.
    let dp = cp.issue_dp_client("gw-usage-7c1").await;
    let pool = cp.db().pool();
    let mut held = Vec::new();
    for _ in 0..pool.options().get_max_connections() {
        held.push(pool.acquire().await.expect("a pool connection"));
    }
    let (b, body) = (Uuid::new_v4(), batch(&["p1"]));
    let r = dp.telemetry(b, &body).await;
    assert_eq!(r.status, 503, "{r:?}");
    assert_eq!(r.body["code"], "unavailable");
    assert_eq!(r.header(DEDUP), Some("1"));

    drop(held);
    let r = dp.telemetry(b, &body).await;
    assert_eq!(answer(&r), (200, counts(1, 0, 0)));
}

#[tokio::test]
async fn a_revoked_credential_cannot_deliver_usage() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp.issue_bundle(&admin, "gw-usage-7c1").await;
    let dp = cp.dp_client(&bundle);
    let r = dp.telemetry(Uuid::new_v4(), &batch(&["before-7c1"])).await;
    assert_eq!(answer(&r), (200, counts(1, 0, 0)));

    let r = cp
        .send(
            "POST",
            &format!("/admin/gateways/credentials/{}/revoke", bundle.dp_id),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    // Until the revoke is known, on a route that stores nothing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while dp.send("GET", "/dp/store/v1/ns/k-7c1", None).await.status != 403 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "still accepted 5 s after the revoke"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let r = dp.telemetry(Uuid::new_v4(), &batch(&["after-7c1"])).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(r.body["code"], "forbidden");
    assert_eq!(r.header(DEDUP), Some("1"));
    assert_eq!(cp.count("usage_events").await, 1);
    assert_eq!(cp.count("usage_batches").await, 1);
}

#[tokio::test]
async fn operators_page_through_stored_events_and_batches() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp.issue_bundle(&admin, "gw-usage-7c1").await;
    let dp = cp.dp_client(&bundle);
    let with = |id: &str, key: &str, team: &str| {
        let mut e = event(id);
        e["api_key_id"] = json!(key);
        e["team_id"] = json!(team);
        e
    };
    let (b1, b2) = (Uuid::new_v4(), Uuid::new_v4());
    let r = dp
        .telemetry(
            b1,
            &json!({"events": [with("p1", "key-a", "team-a"), with("p2", "key-b", "team-a")]}),
        )
        .await;
    assert_eq!(answer(&r), (200, counts(2, 0, 0)));
    let r = dp
        .telemetry(
            b2,
            &json!({"events": [with("p3", "key-a", "team-b"), {"event_id": "p4"}]}),
        )
        .await;
    assert_eq!(answer(&r), (200, counts(1, 1, 0)));
    // b2 again, as after a lost answer.
    dp.telemetry(
        b2,
        &json!({"events": [with("p3", "key-a", "team-b"), {"event_id": "p4"}]}),
    )
    .await;

    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let get = |path: String| {
        let cookie = viewer.cookie().to_string();
        let cp = &cp;
        async move {
            let r = cp.get_with_cookie(&path, &cookie).await;
            assert_eq!(r.status, 200, "{path}: {r:?}");
            r.body
        }
    };
    let ids = |page: &Value| -> Vec<String> {
        page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["event_id"].as_str().expect("an event_id").to_string())
            .collect()
    };

    // Newest first, a page at a time.
    let page = get("/admin/usage/events?limit=2".to_string()).await;
    assert_eq!(ids(&page), ["p3", "p2"]);
    let before = page["next_before"].as_i64().expect("a next page");
    let page = get(format!("/admin/usage/events?limit=2&before={before}")).await;
    assert_eq!(ids(&page), ["p1"]);
    assert_eq!(page["next_before"], Value::Null);

    // Every field of an item.
    let page = get("/admin/usage/events?api_key_id=key-a&team_id=team-b".to_string()).await;
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{page}");
    let item = items[0].as_object().expect("an item");
    let mut fields: Vec<&str> = item.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "api_key_id",
            "batch_id",
            "dp_id",
            "event",
            "event_id",
            "occurred_at",
            "received_at",
            "request_id",
            "seq",
            "team_id"
        ]
    );
    assert_eq!(item["event_id"], "p3");
    assert_eq!(item["dp_id"], bundle.dp_id.as_str());
    assert_eq!(item["batch_id"], b2.to_string());
    assert_eq!(item["team_id"], "team-b");
    assert_eq!(item["api_key_id"], "key-a");
    assert_eq!(item["request_id"], "req-p3");
    assert_eq!(item["event"], with("p3", "key-a", "team-b"));
    assert!(
        item["seq"].is_i64() && item["occurred_at"].is_string() && item["received_at"].is_string()
    );

    // Each filter on its own.
    assert_eq!(
        ids(&get("/admin/usage/events?api_key_id=key-a".to_string()).await),
        ["p3", "p1"]
    );
    assert_eq!(
        ids(&get("/admin/usage/events?team_id=team-a".to_string()).await),
        ["p2", "p1"]
    );
    assert_eq!(
        ids(&get("/admin/usage/events?request_id=req-p2".to_string()).await),
        ["p2"]
    );
    assert_eq!(
        ids(&get("/admin/usage/events?team_id=team-none".to_string()).await),
        Vec::<String>::new()
    );

    // Batches, newest first, with what each delivery made of them.
    let page = get("/admin/usage/batches".to_string()).await;
    assert_eq!(page["next_before"], Value::Null);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "{page}");
    let second = items[0].as_object().expect("an item");
    let mut fields: Vec<&str> = second.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "batch_id",
            "deliveries",
            "dp_id",
            "duplicates",
            "events",
            "inserted",
            "received_at",
            "rejected",
            "seq"
        ]
    );
    assert_eq!(second["batch_id"], b2.to_string());
    assert_eq!(second["dp_id"], bundle.dp_id.as_str());
    assert_eq!(
        (
            &second["events"],
            &second["inserted"],
            &second["duplicates"],
            &second["rejected"],
            &second["deliveries"]
        ),
        (&json!(2), &json!(1), &json!(0), &json!(1), &json!(2))
    );
    assert_eq!(items[1]["batch_id"], b1.to_string());
    let page = get("/admin/usage/batches?limit=1".to_string()).await;
    let before = page["next_before"].as_i64().expect("a next page");
    let page = get(format!("/admin/usage/batches?limit=1&before={before}")).await;
    assert_eq!(page["items"][0]["batch_id"], b1.to_string());
    assert_eq!(page["next_before"], Value::Null);

    // A query that cannot be read is refused.
    let r = cp
        .get_with_cookie("/admin/usage/events?limit=many", viewer.cookie())
        .await;
    assert_eq!(r.status, 422, "{r:?}");
}

#[tokio::test]
async fn operators_page_through_rejected_events() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp.issue_bundle(&admin, "gw-usage-7c1").await;
    let dp = cp.dp_client(&bundle);
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let first = json!({"event_id": "", "occurred_at": "2026-11-06T08:15:30Z"});
    let second = json!("not-an-event-7c1");
    let third = json!({"event_id": "no-time-7c1"});
    let r = dp
        .telemetry(a, &json!({"events": [first, event("ok-7c1"), second]}))
        .await;
    assert_eq!(answer(&r), (200, counts(1, 2, 0)));
    let r = dp.telemetry(b, &json!({"events": [third]})).await;
    assert_eq!(answer(&r), (200, counts(0, 1, 0)));

    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let get = |path: String| {
        let cookie = viewer.cookie().to_string();
        let cp = &cp;
        async move { cp.get_with_cookie(&path, &cookie).await }
    };
    let raws = |page: &Value| -> Vec<Value> {
        page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["raw"].clone())
            .collect()
    };

    // Newest first, a page at a time.
    let page = get("/admin/usage/rejects?limit=2".to_string()).await;
    assert_eq!(page.status, 200, "{page:?}");
    assert_eq!(raws(&page.body), [third.clone(), second.clone()]);
    let before = page.body["next_before"].as_i64().expect("a next page");
    let page = get(format!("/admin/usage/rejects?limit=2&before={before}")).await;
    assert_eq!(raws(&page.body), std::slice::from_ref(&first));
    assert_eq!(page.body["next_before"], Value::Null);

    // Every field of an item.
    let page = get(format!("/admin/usage/rejects?batch_id={b}")).await;
    let items = page.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{page:?}");
    let item = items[0].as_object().expect("an item");
    let mut fields: Vec<&str> = item.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["batch_id", "dp_id", "raw", "reason", "received_at", "seq"]
    );
    assert_eq!(item["batch_id"], b.to_string());
    assert_eq!(item["dp_id"], bundle.dp_id.as_str());
    assert_eq!(item["reason"], "occurred_at_invalid");
    assert_eq!(item["raw"], third);
    assert!(item["seq"].is_i64() && item["received_at"].is_string());

    // Only one batch's.
    let page = get(format!("/admin/usage/rejects?batch_id={a}")).await;
    assert_eq!(raws(&page.body), [second, first]);
    let reasons: Vec<&str> = page.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["reason"].as_str().expect("a reason"))
        .collect();
    assert_eq!(reasons, ["not_an_object", "event_id_missing"]);
    let page = get(format!("/admin/usage/rejects?batch_id={}", Uuid::new_v4())).await;
    assert_eq!(raws(&page.body), Vec::<Value>::new());

    // A query that cannot be read is refused.
    let page = get("/admin/usage/rejects?batch_id=not-a-uuid-7c1".to_string()).await;
    assert_eq!(page.status, 422, "{page:?}");
}
