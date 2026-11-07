//! Sender worker for the CP-side `/dp/telemetry` surface.
//!
//! Per prd-09a §9A.7B Phase 1:
//!
//! - Proxy handlers call [`UsageSink::try_emit`] (defined in aisix-obs)
//!   to push one event per chat completion onto an mpsc channel.
//! - This worker drains the channel, serializing each event once as it
//!   leaves the queue, batches up to [`MAX_BATCH`] events or
//!   [`MAX_BATCH_BYTES`], or every [`FLUSH_INTERVAL`] (whichever comes
//!   first), POSTs the batch as `{ events: [...] }` to the CP's
//!   `/dp/telemetry` URL, and logs the outcome. An event too large to
//!   travel with others ([`MAX_EVENT_BYTES`]) is dropped alone.
//! - On HTTP error the batch is re-sent — but only while the control
//!   plane's own answer proves re-sending cannot double-count. Every
//!   POST carries [`USAGE_BATCH_ID_HEADER`], the control plane records
//!   that id in the same transaction as the usage rows, and it answers
//!   every response from that handler with [`USAGE_BATCH_DEDUP_HEADER`].
//!   A failure whose response lacks that header — or carries it with an
//!   undefined value — is dropped, exactly as every failure was before. A
//!   connection that could not be made is the exception: nothing was sent,
//!   so it is re-sent whatever the control plane last said.
//!   There is still no persistent disk queue: a batch is given up on once
//!   its OLDEST event is [`RETRY_BUDGET`] old (which can be before its
//!   first attempt, since the sender is single in-flight), or once the
//!   control plane has answered and refused it [`MAX_ANSWERED_FAILURES`]
//!   times in a row. Two failures do not count toward that cap: an attempt
//!   that got no response, bounded by the budget alone, and a `503`
//!   carrying the de-duplication header, which is the control plane
//!   reporting a transient store failure that the same batch, re-sent
//!   unchanged, gets past. Past [`REPORTED_OUTAGE_BUDGET`] of those, the
//!   next batch is posted once to tell a store outage from a batch the
//!   store keeps failing on: if it is stored, the held batch is re-sent once
//!   more and, failing again, given up on; if it is not, the store is down,
//!   and both are held within the budget. A batch given up on after an attempt that may have
//!   reached the control plane unanswered is counted as unconfirmed rather
//!   than as lost. Every wait between re-sends is spread by
//!   [`RETRY_JITTER`], so gateways that failed together do not re-send
//!   together. The `received_at` column on the control plane side
//!   records when CP saw the row, so dashboards distinguish "DP never sent"
//!   from "DP sent but CP rejected" via log correlation.
//!
//! mTLS: the sender presents the same on-disk bundle the heartbeat
//! worker uses. The control plane derives `env_id` and `dp_id` from the peer
//! cert SAN URI, so the request body doesn't carry them — same wire
//! shape as `/dp/heartbeat`.

use std::sync::Arc;
use std::time::Duration;

use aisix_core::redact_url_for_log;
use anyhow::{anyhow, Context};
use hyper::body::Bytes;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

use aisix_obs::{Metrics, UsageEvent, UsageEventLabels, UsageSink};

use crate::heartbeat::MtlsBundle;

/// Maximum number of events accumulated per outbound POST: the most one
/// batch may carry at the control plane (`MAX_EVENTS_PER_BATCH`), which
/// skips any past it. One POST is in flight at a time, so this is what
/// delivery's throughput is per round trip. A batch of large events is cut
/// at [`MAX_BATCH_BYTES`] first.
const MAX_BATCH: usize = 1000;

/// The most bytes one POST's body may carry: half the control plane's 8 MiB
/// body cap. A body past that cap is answered `413`, which is permanent, and
/// every event in it would be lost.
const MAX_BATCH_BYTES: usize = 4 << 20;

/// The most bytes one event may serialize to. One larger is dropped alone,
/// counted [`DROP_OVERSIZED`], rather than travel with others: far above any
/// row the emit chokepoint lets through, which bounds every text a caller or
/// an upstream chose.
const MAX_EVENT_BYTES: usize = 256 << 10;

/// Cadence at which the worker flushes whatever it has buffered, even
/// if the buffer hasn't filled. Kept short so the control plane sees
/// spend, and applies its budget marks, within a fraction of a second.
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// In-memory bound on the proxy → worker channel. Beyond it `try_emit`
/// warns and the event is dropped (`sink_full`) — telemetry must not
/// back-pressure the request hot path.
///
/// Sized for the control plane being SLOW rather than for the steady
/// rate: one POST is in flight at a time — a batch being re-sent holds
/// that slot for as long as it keeps failing — so the queue is the only
/// thing holding the traffic that arrives meanwhile, and a batch this
/// worker gives up on is gone for good. A stability round measured 1.1–1.8s per batch
/// for 8s at 173 req/s — ~1.4k events behind a queue that held 1024, so
/// 793 were dropped. At 16384 the same stall is absorbed whole, and the
/// queue only overflows once a sustained arrival rate outruns delivery
/// for minutes rather than seconds.
///
/// Cost is bounded by occupancy, not by the bound: the channel allocates
/// in small blocks as events are pushed, so a queue that never fills
/// never holds the memory for one that did.
const QUEUE_CAPACITY: usize = 16_384;

/// Request header carrying the id this sender mints once per batch. Every
/// re-send of that batch repeats the value verbatim, which is what lets the
/// control plane recognise a batch it has already committed and answer it
/// without writing anything twice.
const USAGE_BATCH_ID_HEADER: &str = "X-Aisix-Usage-Batch-Id";

/// Response header the control plane sets on EVERY response its telemetry
/// handler produces — success and failure alike. It is the only evidence
/// this sender has that a re-send cannot double-count.
const USAGE_BATCH_DEDUP_HEADER: &str = "X-Aisix-Usage-Batch-Dedup";

/// The one value [`USAGE_BATCH_DEDUP_HEADER`] is defined to carry.
///
/// Matched exactly rather than by presence: the header is a capability
/// assertion, and the protocol pins its value, so anything else is a
/// control plane this sender does not understand — including a future one
/// that uses the value to say something narrower. Reading such a header as
/// "de-duplication is on" is the one mistake that bills twice.
const USAGE_BATCH_DEDUP_ENABLED: &str = "1";

/// First wait before a batch is re-sent; doubles per attempt up to
/// [`RETRY_MAX_BACKOFF`]. The wait actually slept is spread around each step
/// by [`RETRY_JITTER`].
const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Ceiling on the wait between re-sends.
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How far either side of its step in the schedule a re-send wait may land:
/// each wait is drawn uniformly from ±20% of the step, and a step is taken
/// at no more than five sixths of [`RETRY_MAX_BACKOFF`], so the top of the
/// band reaches the ceiling instead of being cut there and piling waits up
/// on it. A control-plane outage fails every gateway at the
/// same moment, and on a fixed schedule the whole fleet would then re-send
/// on the same beat for as long as the outage lasts.
const RETRY_JITTER: f64 = 0.2;

/// How long one batch may keep meeting `503`s that carry
/// [`USAGE_BATCH_DEDUP_HEADER`], from the first of them, spread by
/// [`RETRY_JITTER`] for each batch.
///
/// The control plane answers that for a store failure the same batch,
/// re-sent unchanged, gets past, so those answers do not count toward
/// [`MAX_ANSWERED_FAILURES`]. But one batch that keeps meeting them would
/// otherwise hold the single in-flight sender for the whole
/// [`RETRY_BUDGET`] while everything queued behind it overflows. Five
/// minutes outlasts a database failover. Past it the sender asks whether
/// the failure is the store's or the batch's: it posts the next batch once
/// ([`probe_with_the_next_batch`]). Stored, the held batch gets one more
/// attempt and, failing again with a reported outage, is given up on alone.
/// Not stored, the store is down: the held batch is kept, within
/// [`RETRY_BUDGET`], and the probe waits under its own id to go next, so an
/// outage longer than five minutes loses nothing the budget would keep.
const REPORTED_OUTAGE_BUDGET: Duration = Duration::from_secs(5 * 60);

/// How long a batch may be re-sent, measured from its OLDEST event's
/// `occurred_at` — NOT from its first attempt.
///
/// Bounded by billing, not by patience: cp-api's `usagepush` seals each
/// UTC hour's billing counters at `hour start + settleDelay + 1h`
/// (`settleDelay` is 1 hour today), and an event whose `occurred_at` falls
/// at the end of an hour is already near `hour start + 1h` when it is
/// emitted. A budget equal to `settleDelay` would therefore leave zero
/// margin: the rows land, and the hour they belong to has already been
/// billed without them. 30 minutes of event age keeps 30 minutes of margin.
///
/// Measuring from the first ATTEMPT would only hold that margin for the
/// batch at the head of the queue: this sender is single in-flight, so a
/// batch waiting behind one that burned the whole budget is already half an
/// hour old before its own first attempt, and its own budget would carry it
/// past the seal. A batch that is already past the budget when it reaches
/// the head is therefore dropped WITHOUT an attempt. Change either side
/// only together with the other's comment — the control-plane half is in
/// `internal/cpapi/usagepush` and prd-09 §9.6.6.
const RETRY_BUDGET: Duration = Duration::from_secs(30 * 60);

/// Consecutive failures that CARRIED A RESPONSE before a batch is given up
/// on, whatever the budget has left.
///
/// The control plane classifies a permanently unacceptable batch as a
/// non-retryable 4xx, which this sender already drops at once. This is the
/// safety net for the case that classification misses: a control plane
/// answering `5xx` for a batch it can never accept would otherwise hold the
/// single in-flight channel for the whole budget, and the queue behind it —
/// every event the gateway records meanwhile — overflows long before that,
/// which is a far larger loss than the batch itself.
///
/// A failure with NO response does not count against this cap: that is the
/// control-plane outage this feature exists for, and it is bounded by the
/// budget instead.
///
/// Nor does a `503` that carries [`USAGE_BATCH_DEDUP_HEADER`]. The control
/// plane answers that only for a transient store failure that the same
/// batch, re-sent unchanged, gets past — a database failover, a lock or pool
/// timeout, an admin shutdown — and answers a permanent one `500`. Counted,
/// a store outage longer than the backoff to the eighth answer (about a
/// minute and a half) would drop the batch at the head of every gateway's
/// queue although none of them was ever stored; it is bounded by
/// [`REPORTED_OUTAGE_BUDGET`] instead, on the same backoff. Every other answered failure counts,
/// including a `503` WITHOUT the header, which a proxy in front of the
/// control plane can send and which says nothing about the store. See
/// [`SendFailure::counts_toward_the_cap`].
const MAX_ANSWERED_FAILURES: usize = 8;

/// How long the shutdown drain keeps posting what is queued.
///
/// A control plane that answers gets through a full queue well inside this.
/// One that has stopped answering costs the request timeout per batch, and the
/// queue is at its fullest exactly then, so without a limit the exit would take
/// the better part of half an hour and the platform would kill it first, losing
/// the count of what was dropped along with the log flush.
pub(crate) const FINAL_DRAIN_BUDGET: Duration = Duration::from_secs(20);

/// `aisix_usage_event_drops_total{reason}` for a batch given up on without
/// ever being re-sent: the control plane answered without
/// [`USAGE_BATCH_DEDUP_HEADER`] (so re-sending could double-count), the
/// failure was not retryable, or the gateway is shutting down.
const DROP_SEND_FAILED: &str = "send_failed";

/// `aisix_usage_event_drops_total{reason}` for a batch given up on although
/// it may have been stored: an attempt of it reached the wire and got no
/// response (a timeout, a connection lost after the request went out), or
/// the shutdown drain ran out of time with it in flight. Distinct from the
/// other reasons, which each say the control plane does not hold the
/// events, so that `expected - drops` stays a bound the stored rows reach
/// and `expected - drops - unconfirmed` one they do not fall below.
const DROP_SEND_UNCONFIRMED: &str = "send_unconfirmed";

/// `aisix_usage_event_drops_total{reason}` for an event that serialized to
/// more than [`MAX_EVENT_BYTES`], given up on alone as it left the queue so
/// that it cannot take the events of its batch with it.
const DROP_OVERSIZED: &str = "oversized";

/// `aisix_usage_event_drops_total{reason}` for a batch that ran out of room
/// to keep trying: its events aged past [`RETRY_BUDGET`] (with or without
/// an attempt ever being made), the answers refusing it reached
/// [`MAX_ANSWERED_FAILURES`], or the control plane kept reporting a store
/// outage on it past [`REPORTED_OUTAGE_BUDGET`]. Distinct from
/// [`DROP_SEND_FAILED`], which says the very first failure could not be
/// re-sent at all.
const DROP_RETRY_BUDGET_EXHAUSTED: &str = "retry_budget_exhausted";

/// Path the telemetry worker POSTs to, under `managed.cp_base_url`.
/// Derived from the heartbeat URL by swapping the suffix, so the two
/// stay in lock-step on a `cp_base_url` change.
pub const TELEMETRY_PATH: &str = "/dp/telemetry";

/// Configuration for the sender. Mirrors `HeartbeatConfig` — the URL
/// is the absolute `/dp/telemetry` endpoint on the control plane, the bundle is
/// the externally provisioned on-disk mTLS material, and `interval`
/// is the flush cadence (kept overridable so tests can speed up).
#[derive(Clone)]
pub struct TelemetryConfig {
    pub url: String,
    pub interval: Duration,
    pub mtls: MtlsBundle,
}

impl std::fmt::Debug for TelemetryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryConfig")
            .field("url", &redact_url_for_log(&self.url))
            .field("interval", &self.interval)
            .field("mtls", &self.mtls)
            .finish()
    }
}

impl TelemetryConfig {
    /// Build with default flush interval (250 ms).
    pub fn new(url: String, mtls: MtlsBundle) -> Self {
        Self {
            url,
            interval: FLUSH_INTERVAL,
            mtls,
        }
    }
}

/// Spawn the worker. Returns:
///   - a [`UsageSink`] the proxy uses to enqueue events;
///   - a [`tokio::task::JoinHandle`] the caller awaits at shutdown
///     so the final in-flight batch drains cleanly.
///
/// The worker stops when `cancel` flips to `true` AND the channel is
/// drained (one final flush so we don't lose the tail). Errors during
/// individual flushes are logged, not propagated — same contract as
/// heartbeat::spawn.
pub fn spawn(
    cfg: TelemetryConfig,
    metrics: Metrics,
    mut cancel: watch::Receiver<bool>,
) -> (UsageSink, tokio::task::JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
    // Attached here as well as at the wiring point: this function holds the
    // handle, so a caller cannot end up with a sink whose queue drops are
    // invisible while the worker's are counted. (The wiring point still
    // attaches it, because the no-control-plane branch builds a
    // `UsageSink::disabled()` that never comes through here and still has
    // `sink_disabled` to report.)
    let sink = UsageSink::new(tx).with_metrics(metrics.clone());
    let handle = tokio::spawn(async move {
        run(cfg, metrics, rx, &mut cancel).await;
    });
    (sink, handle)
}

/// Run `serving` to completion, then flip `stop`, the signal the worker
/// was [`spawn`]ed with, so it drains what is queued and exits.
///
/// The worker must not stop on the drain's `cancel`: a connection accepted
/// before the drain ended is still served after it, and that request's
/// UsageEvent is owed like any other. Stopping only once the proxy has
/// stopped serving is what keeps those events from landing in a queue
/// nobody reads. A Realtime session counts as serving until it has
/// emitted its own event.
pub async fn stop_after<T>(
    serving: impl std::future::Future<Output = T>,
    stop: &watch::Sender<bool>,
) -> T {
    let served = serving.await;
    let _ = stop.send(true);
    served
}

async fn run(
    cfg: TelemetryConfig,
    metrics: Metrics,
    mut rx: tokio::sync::mpsc::Receiver<UsageEvent>,
    cancel: &mut watch::Receiver<bool>,
) {
    let client = match build_client(&cfg.mtls) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(error = %format_args!("{e:#}"), "telemetry: build mTLS client failed; worker disabled");
            // Nothing will ever be delivered: strict budget mode must see it.
            metrics.usage_delivery().note_waiting();
            return;
        }
    };
    let mut ticker = tokio::time::interval(cfg.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut buffer = Buffer::default();
    let mut state = SenderState::default();

    tracing::info!(
        url = %redact_url_for_log(&cfg.url),
        flush_interval_ms = cfg.interval.as_millis() as u64,
        max_batch = MAX_BATCH,
        retry_budget_secs = RETRY_BUDGET.as_secs(),
        "telemetry sender started (mTLS)",
    );

    loop {
        tokio::select! {
            // New event from the proxy. Buffer it; flush if we hit
            // the batch ceiling so a steady high-throughput stream
            // doesn't starve the control plane between ticks.
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        metrics.usage_delivery().note_waiting();
                        buffer.push(event, &metrics);
                        if buffer.holds_a_full_batch() {
                            flush(&client, &cfg, &metrics, &mut state, &mut buffer, &mut rx, Retry::Allowed, cancel).await;
                        }
                    }
                    None => {
                        // All senders dropped — proxy is shutting down.
                        // Drain anything left and exit.
                        while !buffer.is_empty() {
                            flush(&client, &cfg, &metrics, &mut state, &mut buffer, &mut rx, Retry::Disabled, cancel).await;
                        }
                        tracing::info!("telemetry sender: channel closed, exiting");
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                if !buffer.is_empty() {
                    fill_ready_batch(&mut buffer, &mut rx, &metrics);
                    flush(&client, &cfg, &metrics, &mut state, &mut buffer, &mut rx, Retry::Allowed, cancel).await;
                }
            }
            _ = cancel.changed() => {}
        }
        // A POST that reached the control plane cleared the waiting mark;
        // whatever is still queued behind it waits from now.
        if !buffer.is_empty() || !rx.is_empty() {
            metrics.usage_delivery().note_waiting();
        }
        // Checked here rather than inside the `cancel` branch because a
        // flush above may have consumed the change notification while it
        // was waiting to re-send — `changed()` reports each version once,
        // so a branch that only fires on the notification would never see
        // the shutdown and the drain would never run.
        if *cancel.borrow() {
            // Final drain — post whatever is still queued, in
            // batches within MAX_BATCH and MAX_BATCH_BYTES. This is the
            // one path that can meet a FULL queue (every other flush
            // happens at or below the ceilings), and one POST of
            // everything queued is exactly what the ceilings
            // exist to prevent: cp-api rejects an oversized body
            // and the whole backlog is gone, unretried.
            //
            // `Retry::Disabled`: every batch here gets one attempt. Waiting
            // out a backoff would hold the whole shutdown open for a
            // control plane that is already failing, and so would posting
            // into one that has stopped answering, one timeout per batch:
            // past `FINAL_DRAIN_BUDGET` what is left is counted and given up on.
            let drain_deadline = Instant::now() + FINAL_DRAIN_BUDGET;
            loop {
                fill_ready_batch(&mut buffer, &mut rx, &metrics);
                if buffer.is_empty() {
                    break;
                }
                let Some(batch) = buffer.take_next() else {
                    break;
                };
                let delivered = tokio::time::timeout_at(
                    drain_deadline,
                    deliver(
                        &client,
                        &cfg,
                        &metrics,
                        &mut state,
                        &batch,
                        Retry::Disabled,
                        cancel,
                        None,
                    ),
                )
                .await;
                if delivered.is_err() {
                    // The batch in flight may have been stored; what is still
                    // queued was never sent.
                    drop_batch(
                        &metrics,
                        DROP_SEND_UNCONFIRMED,
                        &batch.events,
                        batch.id,
                        0,
                        Some(&SendFailure::NoResponse {
                            never_sent: false,
                            error: anyhow::anyhow!(
                                "the shutdown drain ran out of time before this batch was answered"
                            ),
                        }),
                    );
                    while let Ok(event) = rx.try_recv() {
                        buffer.push(event, &metrics);
                    }
                    let unsent: Vec<Prepared> = std::mem::take(&mut buffer).events.into();
                    if !unsent.is_empty() {
                        drop_batch(
                            &metrics,
                            DROP_SEND_FAILED,
                            &unsent,
                            Uuid::new_v4(),
                            0,
                            Some(&SendFailure::NoResponse {
                                never_sent: true,
                                error: anyhow::anyhow!(
                                    "the shutdown drain ran out of time before this batch was sent"
                                ),
                            }),
                        );
                    }
                    break;
                }
            }
            tracing::info!("telemetry sender shutting down");
            return;
        }
    }
}

/// Whether a failed batch may be re-sent, or must be given up on after one
/// attempt. Shutdown takes [`Retry::Disabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    Allowed,
    Disabled,
}

/// The state that outlives a single batch: what the most recent exchange
/// with the control plane said about batch de-duplication, and how this
/// sender spreads its re-send waits.
#[derive(Debug)]
struct SenderState {
    /// Whether the most recent response this sender received carried
    /// [`USAGE_BATCH_DEDUP_HEADER`]. Starts `false` — until a response
    /// proves otherwise, this sender behaves exactly as it did before
    /// re-sending existed.
    dedup_seen: bool,
    /// Turns a step of the backoff schedule into the wait actually slept:
    /// [`random_jitter`], except in a test that has to see every wait go
    /// through it.
    jitter: fn(Duration) -> Duration,
}

impl Default for SenderState {
    fn default() -> Self {
        SenderState {
            dedup_seen: false,
            jitter: random_jitter,
        }
    }
}

impl SenderState {
    /// Decide whether the batch that just produced `failure` may be re-sent,
    /// and record what the exchange said about the control plane.
    ///
    /// **Judged per response, never cached across exchanges.** dp-manager
    /// runs several replicas, a rolling upgrade puts old and new pods behind
    /// one Service, and a control-plane rollback to a version without batch
    /// de-duplication is supported. A "we saw the header once" flag would
    /// therefore keep re-sending to a pod that counts every re-send again.
    /// So: a response that carries the header and a retryable status may be
    /// re-sent; a response without the header may not, and clears the flag.
    ///
    /// A failure with NO response carries no evidence at all, and that is
    /// the failure a control-plane outage actually produces. A connection
    /// that could not be made sent nothing, so re-sending it can never count
    /// anything twice, and it is re-sent whatever the flag says: a gateway
    /// that boots while the control plane is down has seen no response yet.
    /// Any other — a timeout, a connection lost after the request went out —
    /// may have been stored, so it falls back to the flag: re-send only if
    /// the most recent response this sender received carried the header and
    /// nothing since has come back without it.
    fn may_resend(&mut self, failure: &SendFailure) -> bool {
        match failure {
            SendFailure::Response {
                dedup, retryable, ..
            } => {
                self.dedup_seen = *dedup;
                *dedup && *retryable
            }
            SendFailure::NoResponse { never_sent, .. } => *never_sent || self.dedup_seen,
        }
    }
}

/// One event as it goes out, serialized once as it leaves the queue: its
/// size is known before it joins a batch, and every re-send of its batch
/// repeats the same bytes.
#[derive(Debug)]
struct Prepared {
    json: Vec<u8>,
    /// `occurred_at`, parsed; `None` when it does not parse (see
    /// [`remaining_budget`]).
    occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The member pair a drop is counted against (see [`drop_batch`]).
    user_id: String,
    user_name: String,
}

impl Prepared {
    /// Serialize `event` for the wire, or give it up alone when it is too
    /// large to travel with others.
    fn new(event: UsageEvent, metrics: &Metrics) -> Option<Prepared> {
        let reason = match serde_json::to_vec(&event) {
            Ok(json) if json.len() <= MAX_EVENT_BYTES => {
                return Some(Prepared {
                    json,
                    occurred_at: chrono::DateTime::parse_from_rfc3339(&event.occurred_at)
                        .ok()
                        .map(|at| at.with_timezone(&chrono::Utc)),
                    user_id: event.user_id,
                    user_name: event.user_name,
                });
            }
            Ok(_) => DROP_OVERSIZED,
            Err(_) => DROP_SEND_FAILED,
        };
        metrics.record_usage_event_drop(
            reason,
            UsageEventLabels {
                user_id: non_empty(&event.user_id),
                user_name: non_empty(&event.user_name),
                ..UsageEventLabels::default()
            },
        );
        tracing::warn!(
            request_id = event.request_id.as_str(),
            reason,
            max_bytes = MAX_EVENT_BYTES,
            "usage event too large to send (event dropped)",
        );
        None
    }
}

/// `{"events":[` and `]}`, around the comma-separated events of a body.
const BODY_OPEN: &[u8] = br#"{"events":["#;
const BODY_CLOSE: &[u8] = b"]}";

/// The size of the body that carries `count` events of `event_bytes` in all.
const fn body_len(event_bytes: usize, count: usize) -> usize {
    BODY_OPEN.len() + event_bytes + count.saturating_sub(1) + BODY_CLOSE.len()
}

/// The `{ events: [...] }` body of one batch.
fn batch_body(events: &[Prepared]) -> Bytes {
    let event_bytes = events.iter().map(|e| e.json.len()).sum();
    let mut body = Vec::with_capacity(body_len(event_bytes, events.len()));
    body.extend_from_slice(BODY_OPEN);
    for (i, event) in events.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(&event.json);
    }
    body.extend_from_slice(BODY_CLOSE);
    body.into()
}

/// The events taken off the queue and not yet sent, in arrival order.
#[derive(Debug, Default)]
struct Buffer {
    /// A batch already posted under its id, which must go out next and
    /// under the same id: the probe [`probe_with_the_next_batch`] sent and
    /// the store did not take.
    parked: Option<Parked>,
    events: std::collections::VecDeque<Prepared>,
    /// The serialized size of `events`, in all.
    bytes: usize,
}

/// See [`Buffer::parked`].
#[derive(Debug)]
struct Parked {
    id: Uuid,
    events: Vec<Prepared>,
    /// Whether an attempt of it may have reached the control plane
    /// unanswered.
    maybe_stored: bool,
}

impl Buffer {
    fn push(&mut self, event: UsageEvent, metrics: &Metrics) {
        if let Some(prepared) = Prepared::new(event, metrics) {
            self.bytes += prepared.json.len();
            self.events.push_back(prepared);
        }
    }

    fn is_empty(&self) -> bool {
        self.parked.is_none() && self.events.is_empty()
    }

    /// The next batch to deliver: the parked one under its own id, or
    /// [`Self::take_batch`] under a new one. `None` when nothing is queued.
    fn take_next(&mut self) -> Option<Parked> {
        if let Some(parked) = self.parked.take() {
            return Some(parked);
        }
        let events = self.take_batch();
        (!events.is_empty()).then(|| Parked {
            id: Uuid::new_v4(),
            events,
            maybe_stored: false,
        })
    }

    /// Whether a whole batch is waiting: [`MAX_BATCH`] events, or enough of
    /// them to fill [`MAX_BATCH_BYTES`].
    fn holds_a_full_batch(&self) -> bool {
        self.events.len() >= MAX_BATCH || body_len(self.bytes, self.events.len()) >= MAX_BATCH_BYTES
    }

    /// Take the next batch: the events at the front that fit both
    /// ceilings, and at least one.
    fn take_batch(&mut self) -> Vec<Prepared> {
        let mut batch = Vec::new();
        let mut bytes = 0;
        while batch.len() < MAX_BATCH {
            let Some(next) = self.events.front() else {
                break;
            };
            let grown = bytes + next.json.len();
            if !batch.is_empty() && body_len(grown, batch.len() + 1) > MAX_BATCH_BYTES {
                break;
            }
            let Some(event) = self.events.pop_front() else {
                break;
            };
            bytes = grown;
            self.bytes -= event.json.len();
            batch.push(event);
        }
        batch
    }
}

fn fill_ready_batch(
    buffer: &mut Buffer,
    rx: &mut tokio::sync::mpsc::Receiver<UsageEvent>,
    metrics: &Metrics,
) {
    while !buffer.holds_a_full_batch() {
        match rx.try_recv() {
            Ok(event) => buffer.push(event, metrics),
            Err(_) => break,
        }
    }
}

/// POST the next batch off the buffer, re-sending it while that is safe.
/// Errors are logged, not propagated — telemetry losses must not stall the
/// worker. The batch leaves the buffer whatever happens to it, so a hung
/// control plane cannot grow the buffer (worst case the batch is dropped).
#[allow(clippy::too_many_arguments)]
async fn flush(
    client: &reqwest::Client,
    cfg: &TelemetryConfig,
    metrics: &Metrics,
    state: &mut SenderState,
    buffer: &mut Buffer,
    rx: &mut tokio::sync::mpsc::Receiver<UsageEvent>,
    retry: Retry,
    cancel: &mut watch::Receiver<bool>,
) {
    let Some(batch) = buffer.take_next() else {
        return;
    };
    deliver(
        client,
        cfg,
        metrics,
        state,
        &batch,
        retry,
        cancel,
        Some(Behind { buffer, rx }),
    )
    .await;
}

/// What is queued behind the batch being delivered, which
/// [`probe_with_the_next_batch`] asks with.
struct Behind<'a> {
    buffer: &'a mut Buffer,
    rx: &'a mut tokio::sync::mpsc::Receiver<UsageEvent>,
}

/// Tell a store outage from a batch the store keeps failing on: post the
/// next batch once. `true` when the control plane stored it. A probe that
/// was not stored and may be re-sent goes next, under the id it went out
/// with; one that may not be is given up on as [`deliver`] gives one up.
/// `false` too when nothing is queued to ask with.
async fn probe_with_the_next_batch(
    client: &reqwest::Client,
    cfg: &TelemetryConfig,
    metrics: &Metrics,
    state: &mut SenderState,
    behind: &mut Behind<'_>,
) -> bool {
    fill_ready_batch(behind.buffer, behind.rx, metrics);
    let Some(probe) = behind.buffer.take_next() else {
        return false;
    };
    match send(client, cfg, probe.id, &batch_body(&probe.events)).await {
        Attempt::Delivered { dedup, rejected } => {
            metrics.usage_delivery().note_reached();
            state.dedup_seen = dedup;
            if rejected > 0 {
                metrics.record_usage_events_rejected(rejected);
                tracing::warn!(
                    batch_id = %probe.id,
                    count = probe.events.len(),
                    rejected,
                    "control plane rejected usage events in an accepted telemetry batch (events dropped)",
                );
            }
            tracing::warn!(
                batch_id = %probe.id,
                count = probe.events.len(),
                "telemetry batch stored while the one ahead of it kept meeting a reported store outage",
            );
            true
        }
        Attempt::Failed(failure) => {
            let maybe_stored = probe.maybe_stored
                || matches!(
                    failure,
                    SendFailure::NoResponse {
                        never_sent: false,
                        ..
                    }
                );
            if state.may_resend(&failure) {
                behind.buffer.parked = Some(Parked {
                    maybe_stored,
                    ..probe
                });
            } else {
                drop_batch(
                    metrics,
                    if maybe_stored {
                        DROP_SEND_UNCONFIRMED
                    } else {
                        DROP_SEND_FAILED
                    },
                    &probe.events,
                    probe.id,
                    1,
                    Some(&failure),
                );
            }
            false
        }
    }
}

/// Deliver one batch, re-sending the SAME batch — same id, same events, same
/// order — until it lands, until re-sending stops being safe, or until
/// [`RETRY_BUDGET`] runs out. Nothing else is sent meanwhile: the worker is
/// single in-flight by design, and arriving events queue up behind it.
/// `behind` is what [`probe_with_the_next_batch`] asks with when the batch
/// keeps meeting a reported store outage; `None` asks nothing.
#[allow(clippy::too_many_arguments)]
async fn deliver(
    client: &reqwest::Client,
    cfg: &TelemetryConfig,
    metrics: &Metrics,
    state: &mut SenderState,
    batch: &Parked,
    retry: Retry,
    cancel: &mut watch::Receiver<bool>,
    mut behind: Option<Behind<'_>>,
) {
    let batch_id = batch.id;
    let events = batch.events.as_slice();
    let count = events.len();
    let budget = remaining_budget(events);
    if budget.is_zero() {
        // Already past the billing window the budget protects — see
        // `RETRY_BUDGET`. Sending it would land rows in an hour that has been
        // billed without them, so it is given up on unattempted.
        drop_batch(
            metrics,
            DROP_RETRY_BUDGET_EXHAUSTED,
            events,
            batch_id,
            0,
            None,
        );
        return;
    }
    let deadline = Instant::now() + budget;
    let mut backoff = RETRY_INITIAL_BACKOFF;
    let mut attempts = 0usize;
    let mut answered_failures = 0usize;
    let mut resending = false;
    // When the control plane's report of a store outage on this batch stops
    // being believed: set at the first such report.
    let mut reported_outage_ends: Option<Instant> = None;
    // Whether an attempt may have reached the control plane unanswered, so a
    // drop cannot say the events are not stored.
    let mut maybe_stored = batch.maybe_stored;
    // Set once the batch behind this one was stored while this one kept
    // meeting a reported outage: the store works, so another such report on
    // this batch is about the batch.
    let mut the_store_answers = false;
    // Flipped when shutdown interrupts a backoff: the batch in hand gets one
    // final attempt, and then this worker stops holding the drain open.
    let mut last_attempt = retry == Retry::Disabled;
    let body = batch_body(events);

    loop {
        attempts += 1;
        let failure = match send(client, cfg, batch_id, &body).await {
            Attempt::Delivered { dedup, rejected } => {
                metrics.usage_delivery().note_reached();
                state.dedup_seen = dedup;
                // Delivered and final: the control plane judged these events
                // and would refuse them again, so there is nothing to re-send.
                // What there is, is a hole in the usage record that nothing
                // else reports.
                if rejected > 0 {
                    metrics.record_usage_events_rejected(rejected);
                    tracing::warn!(
                        %batch_id,
                        count,
                        rejected,
                        "control plane rejected usage events in an accepted telemetry batch (events dropped)",
                    );
                }
                if resending {
                    tracing::warn!(
                        %batch_id,
                        count,
                        attempts,
                        "telemetry batch delivered after re-sending",
                    );
                } else {
                    tracing::debug!(count, "telemetry batch flushed");
                }
                return;
            }
            Attempt::Failed(failure) => failure,
        };

        if failure.counts_toward_the_cap() {
            answered_failures += 1;
        }
        if matches!(
            failure,
            SendFailure::NoResponse {
                never_sent: false,
                ..
            }
        ) {
            maybe_stored = true;
        }
        let unconfirmed_or = |reason| {
            if maybe_stored {
                DROP_SEND_UNCONFIRMED
            } else {
                reason
            }
        };
        if let SendFailure::Response { status, .. } = &failure {
            // A content rejection is the control plane judging the batch:
            // delivery works, these events just will not be counted.
            if *status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
                metrics.usage_delivery().note_reached();
            }
        }
        let may_resend = state.may_resend(&failure);
        if !may_resend || last_attempt {
            drop_batch(
                metrics,
                unconfirmed_or(DROP_SEND_FAILED),
                events,
                batch_id,
                attempts,
                Some(&failure),
            );
            return;
        }

        if failure.is_reported_outage() && the_store_answers {
            drop_batch(
                metrics,
                unconfirmed_or(DROP_RETRY_BUDGET_EXHAUSTED),
                events,
                batch_id,
                attempts,
                Some(&failure),
            );
            return;
        }
        let now = Instant::now();
        let outage_over = failure.is_reported_outage()
            && now
                >= *reported_outage_ends
                    .get_or_insert_with(|| now + spread(REPORTED_OUTAGE_BUDGET, rand::random()));
        if outage_over {
            // A new window either way: the next report on this batch starts
            // it again.
            reported_outage_ends = None;
            if let Some(behind) = behind.as_mut() {
                if probe_with_the_next_batch(client, cfg, metrics, state, behind).await {
                    // Re-sent at once: the store took the batch behind it.
                    the_store_answers = true;
                    continue;
                }
            }
        }
        if answered_failures >= MAX_ANSWERED_FAILURES || now >= deadline {
            drop_batch(
                metrics,
                unconfirmed_or(DROP_RETRY_BUDGET_EXHAUSTED),
                events,
                batch_id,
                attempts,
                Some(&failure),
            );
            return;
        }
        if !resending {
            resending = true;
            tracing::warn!(
                %batch_id,
                count,
                error = %failure,
                budget_secs = RETRY_BUDGET.as_secs(),
                "telemetry batch failed; re-sending (the control plane de-duplicates by batch id)",
            );
        }

        // Never sleep past the budget: the last attempt happens at the
        // deadline, not one backoff after it.
        let wait = (state.jitter)(backoff).min(deadline - now);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            // `Err` means the watch itself is gone, which only happens as
            // the process tears down. Treated as shutdown, because a
            // channel that resolves instantly forever would otherwise turn
            // the backoff into a hot loop of POSTs.
            changed = cancel.changed() => {
                if changed.is_err() {
                    last_attempt = true;
                }
            }
        }
        if *cancel.borrow() {
            last_attempt = true;
        }
        backoff = (backoff * 2).min(RETRY_MAX_BACKOFF);
    }
}

/// The wait slept for one step of the backoff schedule: `step`, held at five
/// sixths of [`RETRY_MAX_BACKOFF`], moved to the point `unit`, a draw from
/// `[0, 1)`, picks across ±[`RETRY_JITTER`] of it. The hold comes first, so a
/// wait at the ceiling is still drawn from the whole band, 20 s to 30 s.
fn jittered(step: Duration, unit: f64) -> Duration {
    spread(step.min(RETRY_MAX_BACKOFF * 5 / 6), unit)
}

/// `span` moved to the point `unit`, a draw from `[0, 1)`, picks across
/// ±[`RETRY_JITTER`] of it.
fn spread(span: Duration, unit: f64) -> Duration {
    span.mul_f64(1.0 + RETRY_JITTER * (2.0 * unit - 1.0))
}

/// [`jittered`] with a fresh draw for every wait.
fn random_jitter(step: Duration) -> Duration {
    jittered(step, rand::random::<f64>())
}

/// What is left of [`RETRY_BUDGET`] for this batch, measured from its OLDEST
/// event rather than from now: the sender is single in-flight, so a batch
/// that waited behind a long re-send is already old when it first goes out,
/// and the budget exists to keep its rows inside the control plane's billing
/// window rather than to cap how long this worker tries. `Duration::ZERO`
/// means the batch is already past it.
///
/// An event whose `occurred_at` does not parse cannot be aged, so it does not
/// shorten anything — what to do with a malformed timestamp is the control
/// plane's call, by its own rules.
fn remaining_budget(events: &[Prepared]) -> Duration {
    let now = chrono::Utc::now();
    let Some(oldest) = events.iter().filter_map(|event| event.occurred_at).min() else {
        return RETRY_BUDGET;
    };
    // `to_std` fails on a negative span — an event stamped in the future,
    // which ages nothing.
    let age = (now - oldest).to_std().unwrap_or_default();
    RETRY_BUDGET.saturating_sub(age)
}

/// Give up on a batch: count every event it carried against the existing
/// drop counter under `reason`, and log the one line that closes the retry
/// the `re-sending` line opened.
///
/// Attribution is thinner here than at the queue: `UsageSink::try_emit`
/// takes the model and ProviderKey dimensions from the emitting handler's
/// label set, and the queue carries events, not label sets. The member pair
/// comes off the event exactly as it does there — same fields, same
/// `unknown` fallback — so "whose usage records were lost" stays answerable
/// for the one dimension the event itself supplies.
fn drop_batch(
    metrics: &Metrics,
    reason: &'static str,
    events: &[Prepared],
    batch_id: Uuid,
    attempts: usize,
    failure: Option<&SendFailure>,
) {
    for event in events {
        metrics.record_usage_event_drop(
            reason,
            UsageEventLabels {
                user_id: non_empty(&event.user_id),
                user_name: non_empty(&event.user_name),
                ..UsageEventLabels::default()
            },
        );
    }
    let error = match failure {
        Some(failure) => failure.to_string(),
        // `attempts = 0` beside it: there is no failure to name, because
        // nothing was sent.
        None => "older than the retry budget; not attempted".to_string(),
    };
    tracing::warn!(
        %batch_id,
        count = events.len(),
        attempts,
        reason,
        error,
        // Wording held verbatim from before re-sending existed: the
        // control-plane e2e log scans allowlist this line by its exact text
        // (`e2e/cases/dp_harness_test.go`, `dashboard/tests/e2e/dp-harness.ts`),
        // and an offline window is expected to produce it. The two lines that
        // bracket a re-send are new text and need allowlisting there.
        "telemetry batch failed (events dropped)",
    );
}

fn non_empty(value: &str) -> &str {
    if value.is_empty() {
        "unknown"
    } else {
        value
    }
}

/// The control plane's `2xx` answer to a batch. Decoded loosely — a newer
/// control plane may add fields or drop this one.
#[derive(Debug, Deserialize)]
struct TelemetryAnswer {
    #[serde(default)]
    rejected: u64,
}

/// What one POST of a batch came back as.
#[derive(Debug)]
enum Attempt {
    /// The control plane accepted the batch — including the case where it
    /// recognised a batch id it had already committed and wrote nothing.
    /// `dedup` reports whether the response carried
    /// [`USAGE_BATCH_DEDUP_HEADER`].
    Delivered {
        dedup: bool,
        /// Events the control plane refused inside the accepted batch —
        /// the `rejected` count of its `{accepted, rejected}` answer.
        rejected: u64,
    },
    Failed(SendFailure),
}

#[derive(Debug)]
enum SendFailure {
    /// The control plane answered. `dedup` says whether that response
    /// carried [`USAGE_BATCH_DEDUP_HEADER`]; `retryable` says whether the
    /// status is one a re-send could clear.
    Response {
        status: reqwest::StatusCode,
        dedup: bool,
        retryable: bool,
        error: anyhow::Error,
    },
    /// No response at all — connect failure, timeout, TLS error. This
    /// exchange says nothing about what the control plane supports.
    /// `never_sent` says the connection itself could not be made, so
    /// nothing of the request reached the control plane.
    NoResponse {
        never_sent: bool,
        error: anyhow::Error,
    },
}

impl std::fmt::Display for SendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendFailure::Response { error, .. } | SendFailure::NoResponse { error, .. } => {
                write!(f, "{error:#}")
            }
        }
    }
}

impl SendFailure {
    /// Whether this failure counts toward [`MAX_ANSWERED_FAILURES`]: every
    /// answered failure except a `503` that carries
    /// [`USAGE_BATCH_DEDUP_HEADER`], the control plane's report of a store
    /// failure the same batch gets past. A failure with no response never
    /// counts. Neither exception touches the count the others built up.
    fn counts_toward_the_cap(&self) -> bool {
        match self {
            SendFailure::Response { status, dedup, .. } => {
                !(*dedup && *status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
            }
            SendFailure::NoResponse { .. } => false,
        }
    }

    /// Whether this is the control plane reporting a store outage on the
    /// batch: a `503` that carries [`USAGE_BATCH_DEDUP_HEADER`]. Bounded by
    /// [`REPORTED_OUTAGE_BUDGET`] rather than by the cap.
    fn is_reported_outage(&self) -> bool {
        matches!(self, SendFailure::Response { .. }) && !self.counts_toward_the_cap()
    }
}

/// A status a re-send can clear: the control plane was there but could not
/// take the batch right now. Everything else — a `400` for a malformed
/// batch id or body, and every other 4xx — is the control plane refusing
/// this batch, which no amount of re-sending changes.
fn is_retryable(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

async fn send(
    client: &reqwest::Client,
    cfg: &TelemetryConfig,
    batch_id: Uuid,
    body: &Bytes,
) -> Attempt {
    let resp = client
        .post(&cfg.url)
        // Same as /dp/heartbeat — no Authorization header; the control plane
        // derives identity from the peer cert SAN URI.
        .header(USAGE_BATCH_ID_HEADER, batch_id.to_string())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.clone())
        .send()
        .await;
    let resp = match resp {
        Ok(resp) => resp,
        Err(error) => {
            return Attempt::Failed(SendFailure::NoResponse {
                never_sent: error.is_connect(),
                error: anyhow::Error::new(error.without_url())
                    .context(format!("POST {}", redact_url_for_log(&cfg.url))),
            })
        }
    };

    // Read off THIS response, never remembered: see `SenderState::may_resend`.
    let dedup = resp
        .headers()
        .get(USAGE_BATCH_DEDUP_HEADER)
        .is_some_and(|value| value.as_bytes() == USAGE_BATCH_DEDUP_ENABLED.as_bytes());
    let status = resp.status();
    if status.is_success() {
        // A body that is not the expected answer reads as nothing rejected:
        // the batch was accepted, and there is no count to report.
        let rejected = resp
            .text()
            .await
            .ok()
            .and_then(|body| serde_json::from_str::<TelemetryAnswer>(&body).ok())
            .map_or(0, |answer| answer.rejected);
        return Attempt::Delivered { dedup, rejected };
    }
    let body = resp.text().await.unwrap_or_default();
    Attempt::Failed(SendFailure::Response {
        status,
        dedup,
        retryable: is_retryable(status),
        error: anyhow!(
            "telemetry {} returned {} — {}",
            redact_url_for_log(&cfg.url),
            status,
            body.trim().chars().take(200).collect::<String>()
        ),
    })
}

/// Build the mTLS reqwest client. Identical shape to heartbeat's
/// build_client — extracted out of heartbeat.rs would be nicer but
/// pulling that into a shared module is post-MVP polish and would
/// expand this PR's scope. The two clients are independently
/// constructed so a botched bundle on one path doesn't cascade.
fn build_client(mtls: &MtlsBundle) -> anyhow::Result<reqwest::Client> {
    let ca_pem = std::fs::read(&mtls.ca_cert_path)
        .with_context(|| format!("read {}", mtls.ca_cert_path.display()))?;
    let cert_pem = std::fs::read(&mtls.client_cert_path)
        .with_context(|| format!("read {}", mtls.client_cert_path.display()))?;
    let key_pem = std::fs::read(&mtls.client_key_path)
        .with_context(|| format!("read {}", mtls.client_key_path.display()))?;

    // Ensure a newline separates the two PEM blocks (see heartbeat.rs).
    let mut identity_pem = Vec::with_capacity(cert_pem.len() + key_pem.len() + 1);
    identity_pem.extend_from_slice(&key_pem);
    if !key_pem.ends_with(b"\n") {
        identity_pem.push(b'\n');
    }
    identity_pem.extend_from_slice(&cert_pem);
    let identity = reqwest::Identity::from_pem(&identity_pem)
        .context("build mTLS Identity from client cert + key")?;

    let ca = reqwest::Certificate::from_pem(&ca_pem).context("parse CA certificate")?;

    let mut builder = aisix_gateway::control_plane_client_builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("aisix-dp/{}", &*aisix_core::BUILD_VERSION))
        .identity(identity)
        .add_root_certificate(ca)
        // Pin HTTP/1.1 — see heartbeat::build_client. The control plane cmux
        // routes one TLS port to gRPC (h2) vs REST (http1) by ALPN; once
        // the cloud-sink crates pulled reqwest's `http2` feature into the
        // workspace, this telemetry client advertised `h2` and cmux
        // misrouted the /dp/telemetry POSTs to the gRPC handler.
        .http1_only()
        .use_rustls_tls();
    // Mirror heartbeat::build_client — pick up the operator-supplied
    // extra trust root (managed.cp_ca_cert_file) when set.
    if let Some(extra) = mtls.extra_ca_pem.as_ref() {
        let extra_ca = reqwest::Certificate::from_pem(extra)
            .context("parse managed.cp_ca_cert_file as PEM certificate")?;
        builder = builder.add_root_certificate(extra_ca);
    }
    builder.build().context("build reqwest client with mTLS")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, DistinguishedName, KeyPair, PKCS_ECDSA_P256_SHA256};
    use std::path::Path;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_test_bundle(dir: &Path) -> MtlsBundle {
        let ca_kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(rcgen::DnType::CommonName, "aisix-test-ca");
            dn
        };
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_kp).unwrap();

        let leaf_kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut leaf_params = CertificateParams::new(vec!["dp-test".to_string()]).unwrap();
        leaf_params.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(rcgen::DnType::CommonName, "dp-test");
            dn
        };
        let leaf_cert = leaf_params.signed_by(&leaf_kp, &ca_cert, &ca_kp).unwrap();

        let ca_path = dir.join("ca.crt");
        let cert_path = dir.join("client.crt");
        let key_path = dir.join("client.key");
        std::fs::write(&ca_path, ca_cert.pem()).unwrap();
        std::fs::write(&cert_path, leaf_cert.pem()).unwrap();
        std::fs::write(&key_path, leaf_kp.serialize_pem()).unwrap();

        MtlsBundle {
            ca_cert_path: ca_path,
            client_cert_path: cert_path,
            client_key_path: key_path,
            extra_ca_pem: None,
        }
    }

    /// One timestamp for the whole test binary, so two calls building the
    /// "expected" and the "staged" copy of a batch produce byte-identical
    /// events. Fresh rather than fixed, because a batch is now aged by its
    /// oldest `occurred_at` and a hardcoded past date would put every test
    /// event past [`RETRY_BUDGET`].
    fn now_rfc3339() -> &'static str {
        static NOW: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        NOW.get_or_init(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    }

    /// An `occurred_at` `age` in the past — a batch that waited behind a
    /// long re-send.
    fn aged_rfc3339(age: Duration) -> String {
        (chrono::Utc::now() - chrono::Duration::from_std(age).unwrap())
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn sample_event(id: &str) -> UsageEvent {
        UsageEvent {
            request_id: id.into(),
            occurred_at: now_rfc3339().to_string(),
            model_id: "mod-uuid".into(),
            api_key_id: "ak-uuid".into(),
            prompt_tokens: 10,
            completion_tokens: 20,
            upstream_latency_ms: 30,
            status_code: 200,
            cost_usd: 0.001,
            guardrail_blocked: false,
            ..Default::default()
        }
    }

    /// `events` as the worker sends them.
    fn prepared(events: Vec<UsageEvent>) -> Vec<Prepared> {
        let metrics = Metrics::new(false);
        events
            .into_iter()
            .map(|event| Prepared::new(event, &metrics).expect("a test event fits"))
            .collect()
    }

    /// The body the worker posts for `events`.
    fn body_of(events: &[UsageEvent]) -> Bytes {
        batch_body(&prepared(events.to_vec()))
    }

    fn plain_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn send_posts_events_array_with_no_authorization() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/dp/telemetry"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accepted": 2
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mtls = write_test_bundle(dir.path());
        let cfg = TelemetryConfig::new(format!("{}/dp/telemetry", server.uri()), mtls);
        let events = vec![sample_event("req-1"), sample_event("req-2")];
        let batch_id = Uuid::new_v4();

        let attempt = send(&plain_client(), &cfg, batch_id, &body_of(&events)).await;
        // A control plane that does not set the header still delivers; it
        // is re-sending that its absence rules out.
        assert!(
            matches!(attempt, Attempt::Delivered { dedup: false, .. }),
            "{attempt:?}"
        );

        let received = server.received_requests().await.unwrap();
        let req = received.first().unwrap();
        // v3 telemetry MUST NOT carry Authorization — mTLS only.
        assert!(req.headers.get("authorization").is_none());
        assert_eq!(
            req.headers
                .get(USAGE_BATCH_ID_HEADER)
                .expect("every batch carries its id")
                .to_str()
                .unwrap(),
            batch_id.to_string(),
        );
        // Body wraps the array in `{ events: [...] }`.
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["events"].as_array().unwrap().len(), 2);
        assert_eq!(body["events"][0]["request_id"], "req-1");
    }

    /// The captured header values are exporter-only: the body posted to
    /// `/dp/telemetry` carries neither map, while the new metadata fields
    /// ride it.
    #[tokio::test]
    async fn send_never_posts_captured_header_values() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/dp/telemetry"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accepted": 1
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mtls = write_test_bundle(dir.path());
        let cfg = TelemetryConfig::new(format!("{}/dp/telemetry", server.uri()), mtls);
        let mut event = sample_event("req-headers");
        event.request_headers = [("x-sub-user".to_string(), "alice".to_string())].into();
        event.upstream_response_headers = [("x-upstream-id".to_string(), "u-1".to_string())].into();
        event.api_key_name = "key-a".into();
        event.gateway_overhead_ms = Some(4);
        event.upstream_tpot_ms = Some(2.5);
        event.service_tier = Some("priority".into());
        event.modality_tokens.prompt_audio_tokens = Some(7);
        send(&plain_client(), &cfg, Uuid::new_v4(), &body_of(&[event])).await;

        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        let posted = &body["events"][0];
        let text = posted.to_string();
        for absent in [
            "request_headers",
            "upstream_response_headers",
            "alice",
            "u-1",
        ] {
            assert!(!text.contains(absent), "{absent} reached the CP: {text}");
        }
        assert_eq!(posted["api_key_name"], "key-a");
        assert_eq!(posted["gateway_overhead_ms"], 4);
        assert_eq!(posted["upstream_tpot_ms"], 2.5);
        assert_eq!(posted["service_tier"], "priority");
        assert_eq!(posted["prompt_audio_tokens"], 7);
        assert!(posted.get("prompt_image_tokens").is_none());
    }

    #[tokio::test]
    async fn send_propagates_non_success_with_body_excerpt() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/dp/telemetry"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"code": "INVALID_REQUEST", "message": "event 0: bad uuid"}
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mtls = write_test_bundle(dir.path());
        let cfg = TelemetryConfig::new(format!("{}/dp/telemetry", server.uri()), mtls);
        let attempt = send(
            &plain_client(),
            &cfg,
            Uuid::new_v4(),
            &body_of(&[sample_event("req-1")]),
        )
        .await;
        let Attempt::Failed(failure) = attempt else {
            panic!("a 400 is a failure: {attempt:?}");
        };
        assert!(
            matches!(
                failure,
                SendFailure::Response {
                    retryable: false,
                    ..
                }
            ),
            "a 400 is the control plane refusing this batch: {failure:?}",
        );
        let s = format!("{failure}");
        assert!(s.contains("400"), "expected status: {s}");
        assert!(s.contains("INVALID_REQUEST"), "expected body excerpt: {s}");
    }

    /// The capability rule, stated at the one place that decides it: judge
    /// each response on its own, and fall back to the last response seen
    /// only when an exchange produced none at all — unless nothing was sent,
    /// which is re-sent whatever was seen.
    #[test]
    fn resend_permission_is_judged_per_response_and_never_cached() {
        let response = |dedup: bool, retryable: bool| SendFailure::Response {
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            dedup,
            retryable,
            error: anyhow!("boom"),
        };
        let no_response = || SendFailure::NoResponse {
            never_sent: false,
            error: anyhow!("operation timed out"),
        };
        let not_connected = || SendFailure::NoResponse {
            never_sent: true,
            error: anyhow!("connection refused"),
        };

        let mut state = SenderState::default();
        // Nothing seen yet: a timeout cannot be re-sent, a connection that
        // was never made can.
        assert!(!state.may_resend(&no_response()));
        assert!(state.may_resend(&not_connected()));
        assert!(!state.dedup_seen, "sending nothing proves nothing");
        // A retryable failure whose response proves de-duplication.
        assert!(state.may_resend(&response(true, true)));
        // …and a timeout after it inherits that evidence.
        assert!(state.may_resend(&no_response()));
        // A non-retryable status is never re-sent, header or not.
        assert!(!state.may_resend(&response(true, false)));
        assert!(state.may_resend(&no_response()));
        // A header-less response — an older replica behind the same
        // Service, or a rolled-back control plane — clears the evidence…
        assert!(!state.may_resend(&response(false, true)));
        // …so the timeouts after it are dropped again, while a connection
        // that was never made is still re-sent.
        assert!(!state.may_resend(&no_response()));
        assert!(state.may_resend(&not_connected()));
    }

    /// Which failures use up [`MAX_ANSWERED_FAILURES`], pinned as a table
    /// because the delivery loop cannot show one row of it: a `503` without
    /// the header is never re-sent, so it ends its batch whether it counts
    /// or not.
    #[test]
    fn only_a_reported_outage_is_exempt_from_the_cap() {
        use reqwest::StatusCode;
        let answered = |status: StatusCode, dedup: bool| SendFailure::Response {
            status,
            dedup,
            retryable: is_retryable(status),
            error: anyhow!("boom"),
        };

        assert!(
            !answered(StatusCode::SERVICE_UNAVAILABLE, true).counts_toward_the_cap(),
            "a 503 with the header is the control plane reporting an outage",
        );
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::REQUEST_TIMEOUT,
        ] {
            assert!(
                answered(status, true).counts_toward_the_cap(),
                "{status} with the header counts",
            );
        }
        assert!(
            answered(StatusCode::SERVICE_UNAVAILABLE, false).counts_toward_the_cap(),
            "a 503 without the header can come from a proxy in front, and counts",
        );
        assert!(
            !SendFailure::NoResponse {
                never_sent: false,
                error: anyhow!("operation timed out"),
            }
            .counts_toward_the_cap(),
            "a failure with no response is bounded by the budget alone",
        );
    }

    #[test]
    fn build_client_loads_real_mtls_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let mtls = write_test_bundle(dir.path());
        let _ = build_client(&mtls).expect("real bundle should build");
    }

    /// Mirror heartbeat regression: PEM without trailing newline.
    #[test]
    fn build_client_works_without_trailing_newlines() {
        let dir = tempfile::tempdir().unwrap();

        let ca_kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(rcgen::DnType::CommonName, "aisix-test-ca");
            dn
        };
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_kp).unwrap();

        let leaf_kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut leaf_params = CertificateParams::new(vec!["dp-test".to_string()]).unwrap();
        leaf_params.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(rcgen::DnType::CommonName, "dp-test");
            dn
        };
        let leaf_cert = leaf_params.signed_by(&leaf_kp, &ca_cert, &ca_kp).unwrap();

        let ca_path = dir.path().join("ca.crt");
        let cert_path = dir.path().join("client.crt");
        let key_path = dir.path().join("client.key");
        std::fs::write(&ca_path, ca_cert.pem().trim_end()).unwrap();
        std::fs::write(&cert_path, leaf_cert.pem().trim_end()).unwrap();
        std::fs::write(&key_path, leaf_kp.serialize_pem().trim_end()).unwrap();

        let mtls = MtlsBundle {
            ca_cert_path: ca_path,
            client_cert_path: cert_path,
            client_key_path: key_path,
            extra_ca_pem: None,
        };
        build_client(&mtls).expect("build_client must tolerate PEM without trailing newline");
    }
    /// A control plane served from the test's own runtime.
    ///
    /// The paused-clock tests advance the clock while an exchange is on the
    /// wire, and the client's 10s request timeout runs on that same clock. A
    /// mock answering from a thread of its own answers when the OS schedules
    /// that thread, and every real millisecond it waits becomes many virtual
    /// ones: on a loaded machine an answered attempt reached the timeout
    /// first and counted as unanswered. Served here, an exchange moves only as
    /// this runtime polls it, so it takes the same few driver steps however
    /// busy the machine is.
    struct LocalServer {
        addr: std::net::SocketAddr,
    }

    impl LocalServer {
        fn uri(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    /// What [`LocalServer`] answers one request with.
    struct Reply {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
        /// Held this long first, on the runtime's clock.
        delay: Option<Duration>,
    }

    impl Reply {
        fn status(status: u16) -> Self {
            Reply {
                status,
                headers: Vec::new(),
                body: "",
                delay: None,
            }
        }
    }

    async fn local_server(
        respond: impl Fn(&axum::http::HeaderMap, &[u8]) -> Reply + Send + Sync + 'static,
    ) -> LocalServer {
        local_server_on(
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            respond,
        )
    }

    /// [`local_server`] on a listener the test made.
    fn local_server_on(
        listener: tokio::net::TcpListener,
        respond: impl Fn(&axum::http::HeaderMap, &[u8]) -> Reply + Send + Sync + 'static,
    ) -> LocalServer {
        use http_body_util::BodyExt;

        let addr = listener.local_addr().unwrap();
        let respond = Arc::new(respond);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let respond = Arc::clone(&respond);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        move |request: axum::http::Request<hyper::body::Incoming>| {
                            let respond = Arc::clone(&respond);
                            async move {
                                assert_eq!(request.method(), "POST");
                                assert_eq!(request.uri().path(), TELEMETRY_PATH);
                                let (parts, body) = request.into_parts();
                                let body = body.collect().await?.to_bytes();
                                let reply = respond(&parts.headers, &body);
                                if let Some(delay) = reply.delay {
                                    tokio::time::sleep(delay).await;
                                }
                                let mut response =
                                    axum::http::Response::builder().status(reply.status);
                                for (name, value) in reply.headers {
                                    response = response.header(name, value);
                                }
                                Ok::<_, hyper::Error>(
                                    response
                                        .body(http_body_util::Full::new(reply.body.as_bytes()))
                                        .unwrap(),
                                )
                            }
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        LocalServer { addr }
    }

    type RecordedBatches = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

    async fn recording_server(first_status: u16) -> (LocalServer, RecordedBatches) {
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&batches);
        let server = local_server(move |headers, body| {
            assert!(headers.get("authorization").is_none());
            let mut batches = recorded.lock().unwrap();
            batches.push(serde_json::from_slice(body).unwrap());
            Reply::status(if batches.len() == 1 {
                first_status
            } else {
                200
            })
        })
        .await;
        (server, batches)
    }

    /// [`recording_server`] that holds its FIRST response for `held`,
    /// leaving the worker inside one flush while a test stages what
    /// arrives behind it. The body is recorded before the wait, so the
    /// test can tell "in flight" from "not sent yet".
    async fn recording_server_holding_first(held: Duration) -> (LocalServer, RecordedBatches) {
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&batches);
        let server = local_server(move |_, body| {
            let mut batches = recorded.lock().unwrap();
            batches.push(serde_json::from_slice(body).unwrap());
            let mut reply = Reply::status(200);
            if batches.len() == 1 {
                reply.delay = Some(held);
            }
            reply
        })
        .await;
        (server, batches)
    }

    /// One recorded POST: the batch id it carried and its body.
    type RecordedPosts = Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>;

    /// One scripted answer: a status, whether the response carries
    /// [`USAGE_BATCH_DEDUP_HEADER`], and whether it is held past the
    /// client's own timeout.
    #[derive(Debug, Clone, Copy)]
    struct Answer {
        status: u16,
        /// `None` sends no header at all; `Some(v)` sends it with value `v`.
        dedup: Option<&'static str>,
        /// `true` answers so late that the sender gives up waiting — the
        /// failure a control-plane outage actually produces, with NO
        /// response to read a capability off. Deterministic where killing
        /// a mock server is not: a dropped listener keeps answering for an
        /// unspecified while.
        stalled: bool,
        /// JSON body of a `2xx` answer; `None` sends an empty body.
        body: Option<&'static str>,
    }

    /// Answers with the de-duplication header — a control plane whose
    /// ingestion is idempotent per batch.
    fn dedups(status: u16) -> Answer {
        Answer {
            status,
            dedup: Some(USAGE_BATCH_DEDUP_ENABLED),
            stalled: false,
            body: None,
        }
    }

    /// Answers with the header set to something the protocol does not
    /// define — a control plane this sender must not read as capable.
    fn dedup_value(status: u16, value: &'static str) -> Answer {
        Answer {
            status,
            dedup: Some(value),
            stalled: false,
            body: None,
        }
    }

    /// Answers WITHOUT it — an older replica behind the same Service, or a
    /// control plane rolled back below that capability.
    fn no_dedup(status: u16) -> Answer {
        Answer {
            status,
            dedup: None,
            stalled: false,
            body: None,
        }
    }

    /// Never answers in time.
    fn stalls() -> Answer {
        Answer {
            status: 200,
            dedup: Some(USAGE_BATCH_DEDUP_ENABLED),
            stalled: true,
            body: None,
        }
    }

    /// Longer than the sender's own request timeout, so an attempt that
    /// meets it fails with no response at all.
    const BEYOND_THE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

    /// Mock that answers the i-th request with `script[i]`, repeating the
    /// last entry forever, and records what each request carried.
    ///
    /// The header is per RESPONSE rather than per server on purpose: behind
    /// one dp-manager Service a rolling upgrade answers from both kinds of
    /// replica.
    async fn scripted_server(script: Vec<Answer>) -> (LocalServer, RecordedPosts) {
        scripted_server_on(
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            script,
        )
    }

    /// [`scripted_server`] on a listener the test made.
    fn scripted_server_on(
        listener: tokio::net::TcpListener,
        script: Vec<Answer>,
    ) -> (LocalServer, RecordedPosts) {
        let posts: RecordedPosts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&posts);
        let server = local_server_on(listener, move |headers, body| {
            // Read before locking: a failed expectation here must fail
            // the test, not poison the recording behind a confusing
            // second error.
            let batch_id = headers
                .get(USAGE_BATCH_ID_HEADER)
                .expect("every batch carries its id")
                .to_str()
                .unwrap()
                .to_string();
            let mut posts = recorded.lock().unwrap();
            posts.push((batch_id, serde_json::from_slice(body).unwrap()));
            let answer = *script
                .get(posts.len() - 1)
                .unwrap_or_else(|| script.last().unwrap());
            let mut reply = Reply::status(answer.status);
            if let Some(value) = answer.dedup {
                reply.headers.push((USAGE_BATCH_DEDUP_HEADER, value));
            }
            if let Some(body) = answer.body {
                reply.headers.push(("content-type", "application/json"));
                reply.body = body;
            }
            if answer.stalled {
                reply.delay = Some(BEYOND_THE_REQUEST_TIMEOUT);
            }
            reply
        });
        (server, posts)
    }

    fn test_config(url: String, dir: &tempfile::TempDir) -> TelemetryConfig {
        TelemetryConfig::new(url, write_test_bundle(dir.path()))
    }

    /// Sum of `aisix_usage_event_drops_total` samples carrying `reason`.
    fn drops_with_reason(metrics: &Metrics, reason: &str) -> u64 {
        metrics
            .render()
            .lines()
            .filter(|line| {
                line.starts_with("aisix_usage_event_drops_total{")
                    && line.contains(&format!("reason=\"{reason}\""))
            })
            .map(|line| line.rsplit(' ').next().unwrap().parse::<u64>().unwrap())
            .sum()
    }

    /// Virtual-time step for a phase whose control plane is UP: real HTTP is
    /// on the wire, and the client's own 10s timeout is measured on the same
    /// paused clock this advances. Small enough that an exchange in flight
    /// gets ~1000 polls before that timeout could fire, large enough that a
    /// one-second backoff is crossed in a hundred.
    const STEP_WHILE_SERVING: Duration = Duration::from_millis(10);

    /// Virtual-time step for a phase whose control plane never answers in
    /// time. Each attempt ends at the client's own request timeout, which is
    /// measured on this same paused clock, so advancing in seconds is what
    /// ENDS an attempt rather than something that could cut one short — and
    /// a 30-minute budget is 1800 cheap iterations.
    ///
    /// It does NOT give an attempt time to be DELIVERED. Ten steps cross
    /// the 10s timeout, so the request gets ten polls to reach a real
    /// socket and a real wiremock thread, against a thousand at
    /// [`STEP_WHILE_SERVING`] — and those ten pass in microseconds of
    /// wall clock. A phase that needs the server to have RECEIVED an
    /// attempt (anything that then reads `posts`) must reach that point
    /// on the small step and only then switch to this one.
    const STEP_WHILE_UNREACHABLE: Duration = Duration::from_secs(1);

    /// Poll the worker while virtual time moves forward in `step`s, until
    /// `done` holds.
    ///
    /// Time is paused, so a backoff only elapses when this advances it —
    /// which is what keeps a 30-minute retry budget a sub-second test. The
    /// step is the caller's because it is a trade: it has to cross a backoff
    /// in a reasonable number of iterations without racing the request
    /// timeout of an exchange that is on the wire right now.
    ///
    /// **The step also decides whether an attempt gets DELIVERED.**
    /// `posts` is pushed by the wiremock responder, on wiremock's own
    /// runtime, in real time; the client's 10s request timeout runs on
    /// the clock this advances, in instant steps. At one second a step an
    /// attempt gets ten polls to cross a real socket — microseconds of
    /// wall clock — before its own timeout cancels it, and a cancelled
    /// request is never recorded at all. At ten milliseconds it gets a
    /// thousand. So a phase that has to end with the server having
    /// RECEIVED an attempt belongs on the small step, whatever the
    /// condition is written in terms of; reaching it on the big step and
    /// then reading `posts` is the shape that flakes.
    ///
    /// An attempt the server ANSWERS carries no such race in either
    /// direction: the response cannot exist unless the responder ran, and
    /// the responder records the post before it replies.
    ///
    /// The small step is MARGIN, not a guarantee. One delivery measured
    /// 14 to 21 iterations of this loop against a budget of ten at the
    /// big step and a thousand at the small one — two orders of
    /// magnitude, which is why one is reliable and the other is a coin
    /// flip, but neither is an invariant. Making it one would mean
    /// synchronising on a server-side event rather than on a poll
    /// budget; nothing here needs that yet.
    async fn drive_until<F: std::future::Future<Output = ()> + ?Sized>(
        mut sender: std::pin::Pin<&mut F>,
        step: Duration,
        mut done: impl FnMut() -> bool,
        what: &str,
    ) {
        let wall_clock_deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(
                poll_sender(sender.as_mut()).await.is_pending(),
                "sender exited before {what}"
            );
            assert!(std::time::Instant::now() < wall_clock_deadline, "{what}");
            tokio::time::advance(step).await;
            tokio::task::yield_now().await;
        }
    }

    fn ordered_events(count: usize) -> Vec<UsageEvent> {
        (0..count)
            .map(|i| {
                // Distinct attempts may share a request ID; none may be deduplicated.
                let mut event = sample_event(&format!("request-{}", i / 3));
                event.model_id = format!("model-{}", i % 7);
                event.provider_kind = format!("provider-{}", i % 3);
                event.user_id = format!("user-{i}");
                event.prompt_tokens = i as u32;
                event.completion_tokens = (i * 2) as u32;
                event
            })
            .collect()
    }

    /// [`ordered_events`], stamped `age` in the past.
    fn aged_events(count: usize, age: Duration) -> Vec<UsageEvent> {
        let occurred_at = aged_rfc3339(age);
        ordered_events(count)
            .into_iter()
            .map(|mut event| {
                event.occurred_at = occurred_at.clone();
                event
            })
            .collect()
    }

    /// `?Sized` so the boxed `dyn Future` a shared staging helper returns
    /// drives through the same three helpers as an inline `Box::pin(run(..))`.
    async fn poll_sender<F: std::future::Future<Output = ()> + ?Sized>(
        mut sender: std::pin::Pin<&mut F>,
    ) -> std::task::Poll<()> {
        std::future::poll_fn(|cx| std::task::Poll::Ready(sender.as_mut().poll(cx))).await
    }

    async fn finish_sender<F: std::future::Future<Output = ()> + ?Sized>(
        mut sender: std::pin::Pin<&mut F>,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while poll_sender(sender.as_mut()).await.is_pending() {
            assert!(std::time::Instant::now() < deadline, "sender did not exit");
            // Keep the paused runtime runnable while real HTTP I/O completes.
            tokio::task::yield_now().await;
        }
    }

    fn assert_recorded_events(batches: &RecordedBatches, expected: &[UsageEvent]) -> Vec<usize> {
        let batches = batches.lock().unwrap();
        let actual: Vec<_> = batches
            .iter()
            .flat_map(|batch| batch["events"].as_array().unwrap().iter().cloned())
            .collect();
        assert_eq!(
            serde_json::json!(actual),
            serde_json::to_value(expected).unwrap()
        );
        batches
            .iter()
            .map(|batch| batch["events"].as_array().unwrap().len())
            .collect()
    }

    /// Channel bound for the backlog cases below. They are about what the
    /// worker does with a queue it has filled, not about how deep the real
    /// one is, so they size their own channel rather than staging
    /// [`QUEUE_CAPACITY`] events to fill it.
    const BACKLOG_QUEUE: usize = 1024;

    async fn run_interval_with_backlog(first_status: u16) {
        let (server, batches) = recording_server(first_status).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = TelemetryConfig::new(
            format!("{}/dp/telemetry", server.uri()),
            write_test_bundle(dir.path()),
        );
        let (tx, rx) = tokio::sync::mpsc::channel(BACKLOG_QUEUE);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        let expected = ordered_events(BACKLOG_QUEUE + 2);
        let mut events = ordered_events(BACKLOG_QUEUE + 2).into_iter();
        tokio::time::pause();
        let mut sender = Box::pin(run(cfg, Metrics::new(false), rx, &mut cancel_rx));
        // Consume the initial empty tick before staging a partial batch.
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        for event in events.by_ref().take(2) {
            tx.try_send(event).unwrap();
        }
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        assert_eq!(tx.capacity(), BACKLOG_QUEUE);
        assert!(batches.lock().unwrap().is_empty());
        for event in events {
            tx.try_send(event).unwrap();
        }
        assert_eq!(tx.capacity(), 0);
        tokio::time::advance(FLUSH_INTERVAL).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while batches.lock().unwrap().is_empty() {
            assert!(poll_sender(sender.as_mut()).await.is_pending());
            assert!(std::time::Instant::now() < deadline, "no telemetry POST");
            tokio::task::yield_now().await;
        }
        // Both recv and the tick are ready: this must hold whichever wins.
        assert_eq!(
            batches.lock().unwrap()[0]["events"]
                .as_array()
                .unwrap()
                .len(),
            MAX_BATCH
        );
        drop(tx);
        finish_sender(sender.as_mut()).await;
        let sizes = assert_recorded_events(&batches, &expected);
        let total = BACKLOG_QUEUE + 2;
        let full = total / MAX_BATCH;
        assert_eq!(
            sizes,
            [vec![MAX_BATCH; full], vec![total - full * MAX_BATCH]].concat()
        );
    }

    #[tokio::test]
    async fn interval_fills_ready_backlog_without_reordering_events() {
        run_interval_with_backlog(200).await;
    }

    /// [`recording_server`] answers without [`USAGE_BATCH_DEDUP_HEADER`] —
    /// a control plane whose ingestion is not idempotent — so its failures
    /// are still dropped where they fall, and never carried into the next
    /// batch.
    #[tokio::test]
    async fn failed_interval_batch_without_dedup_support_is_dropped_not_retried() {
        run_interval_with_backlog(500).await;
    }

    #[tokio::test]
    async fn a_lone_event_is_sent_within_a_quarter_second() {
        let (server, batches) = recording_server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = TelemetryConfig::new(
            format!("{}/dp/telemetry", server.uri()),
            write_test_bundle(dir.path()),
        );
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let mut sender = Box::pin(run(cfg, Metrics::new(false), rx, &mut cancel_rx));
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        // The first tick lands only once the paused clock has moved; let it
        // fire on an empty buffer so it cannot flush the event staged below.
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        for event in ordered_events(1) {
            tx.try_send(event).unwrap();
        }
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        // A paused clock auto-advances when the runtime idles, so assert on
        // the virtual time that elapsed, not merely that the POST happened.
        let staged_at = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_millis(260)).await;
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while batches.lock().unwrap().is_empty() {
            assert!(poll_sender(sender.as_mut()).await.is_pending());
            assert!(
                std::time::Instant::now() < deadline,
                "a lone event was not sent within 260 ms"
            );
            tokio::task::yield_now().await;
        }
        assert!(
            staged_at.elapsed() < Duration::from_millis(300),
            "a lone event waited {:?} before it was sent",
            staged_at.elapsed()
        );
        drop(tx);
        finish_sender(sender.as_mut()).await;
        assert_eq!(batches.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn interval_flushes_partial_batch_without_waiting_for_more_events() {
        let (server, batches) = recording_server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = TelemetryConfig::new(
            format!("{}/dp/telemetry", server.uri()),
            write_test_bundle(dir.path()),
        );
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let mut sender = Box::pin(run(cfg, Metrics::new(false), rx, &mut cancel_rx));
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        for event in ordered_events(2) {
            tx.try_send(event).unwrap();
        }
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        assert_eq!(tx.capacity(), QUEUE_CAPACITY);
        tokio::time::advance(FLUSH_INTERVAL - Duration::from_millis(1)).await;
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        assert!(batches.lock().unwrap().is_empty());
        tokio::time::advance(Duration::from_millis(1)).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while batches.lock().unwrap().is_empty() {
            assert!(poll_sender(sender.as_mut()).await.is_pending());
            assert!(
                std::time::Instant::now() < deadline,
                "partial batch was not flushed"
            );
            tokio::task::yield_now().await;
        }
        drop(tx);
        finish_sender(sender.as_mut()).await;
        assert_eq!(
            assert_recorded_events(&batches, &ordered_events(2)),
            vec![2]
        );
    }

    #[tokio::test]
    async fn cancellation_drains_buffer_and_ready_queue_without_losing_attempts() {
        let (server, batches) = recording_server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = TelemetryConfig::new(
            format!("{}/dp/telemetry", server.uri()),
            write_test_bundle(dir.path()),
        );
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let mut sender = Box::pin(run(cfg, Metrics::new(false), rx, &mut cancel_rx));
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        let mut events = ordered_events(250).into_iter();
        for event in events.by_ref().take(2) {
            tx.try_send(event).unwrap();
        }
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        assert_eq!(tx.capacity(), QUEUE_CAPACITY);
        for event in events {
            tx.try_send(event).unwrap();
        }
        cancel_tx.send(true).unwrap();
        // Keep tx alive: completion must come from cancellation, not channel closure.
        finish_sender(sender.as_mut()).await;
        assert_recorded_events(&batches, &ordered_events(250));
        assert!(tx.is_closed());
    }

    /// The shutdown drain is the one flush that can meet a FULL queue —
    /// every other one happens at or below MAX_BATCH — and one POST of
    /// everything queued is what the ceiling exists to prevent: cp-api
    /// rejects an oversized body, and a rejected batch is not retried, so
    /// the whole backlog would go at once.
    #[tokio::test]
    async fn cancellation_posts_the_backlog_in_batches_not_in_one_body() {
        const QUEUED: usize = MAX_BATCH * 3 + 7;

        let (server, batches) = recording_server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = TelemetryConfig::new(
            format!("{}/dp/telemetry", server.uri()),
            write_test_bundle(dir.path()),
        );
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUED);
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let mut sender = Box::pin(run(cfg, Metrics::new(false), rx, &mut cancel_rx));
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        for event in ordered_events(QUEUED) {
            tx.try_send(event).unwrap();
        }
        cancel_tx.send(true).unwrap();
        finish_sender(sender.as_mut()).await;

        let sizes = assert_recorded_events(&batches, &ordered_events(QUEUED));
        assert!(
            sizes.iter().all(|n| *n <= MAX_BATCH),
            "no POST may carry more than the batch ceiling: {sizes:?}"
        );
    }

    /// [`recording_server`] that also keeps the size of every body it was
    /// sent.
    async fn sizing_server() -> (
        LocalServer,
        RecordedBatches,
        Arc<std::sync::Mutex<Vec<usize>>>,
    ) {
        let batches: RecordedBatches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sizes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (recorded, sized) = (Arc::clone(&batches), Arc::clone(&sizes));
        let server = local_server(move |_, body| {
            sized.lock().unwrap().push(body.len());
            recorded
                .lock()
                .unwrap()
                .push(serde_json::from_slice(body).unwrap());
            Reply::status(200)
        })
        .await;
        (server, batches, sizes)
    }

    /// `count` events whose `requested_model` makes each about `bytes` long.
    fn sized_events(count: usize, bytes: usize) -> Vec<UsageEvent> {
        ordered_events(count)
            .into_iter()
            .map(|mut event| {
                event.requested_model = "m".repeat(bytes);
                event
            })
            .collect()
    }

    /// The control plane refuses a body over 8 MiB with a `413`, which is
    /// permanent, so a batch is cut by its size as well as by its count:
    /// events that would only fit one body by count go out in several, each
    /// well under the control plane's cap, in order and none lost.
    #[tokio::test]
    async fn a_batch_is_cut_by_its_size_as_well_as_by_its_count() {
        const EVENT_BYTES: usize = 200 * 1024;
        const EVENTS: usize = 30;
        let (server, batches, sizes) = sizing_server().await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (tx, rx) = tokio::sync::mpsc::channel(BACKLOG_QUEUE);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let mut sender = Box::pin(run(cfg, metrics.clone(), rx, &mut cancel_rx));
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        let expected = sized_events(EVENTS, EVENT_BYTES);
        for event in expected.clone() {
            tx.try_send(event).unwrap();
        }
        tokio::time::advance(FLUSH_INTERVAL).await;
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || {
                batches
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|b| b["events"].as_array().unwrap().len())
                    .sum::<usize>()
                    >= EVENTS
            },
            "the events were not all delivered",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_recorded_events(&batches, &expected);
        let sizes = sizes.lock().unwrap().clone();
        assert!(
            sizes.len() >= 2,
            "one body for {EVENTS} large events: {sizes:?}"
        );
        assert!(
            sizes.iter().all(|&n| n <= 4 << 20),
            "a body over 4 MiB: {sizes:?}"
        );
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
    }

    /// The ceilings are set against the control plane's: at most 1000 events
    /// a batch, which it skips past, and an 8 MiB body, which it answers
    /// `413` past. Every other test here compares the sender with itself and
    /// stays green if these move.
    #[test]
    fn the_batch_ceilings_sit_within_the_control_planes() {
        assert_eq!(MAX_BATCH, 1000);
        assert_eq!(MAX_BATCH_BYTES, 4 << 20);
        assert_eq!(MAX_EVENT_BYTES, 256 << 10);
        const {
            assert!(MAX_EVENT_BYTES + body_len(0, 1) <= MAX_BATCH_BYTES);
            assert!(MAX_BATCH_BYTES <= (8 << 20) / 2);
        }
    }

    /// An event too large to travel with others is dropped alone, counted
    /// `oversized` against its member, and the events beside it are
    /// delivered.
    #[tokio::test]
    async fn an_event_too_large_to_travel_is_dropped_alone() {
        let (server, batches, _sizes) = sizing_server().await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (tx, rx) = tokio::sync::mpsc::channel(BACKLOG_QUEUE);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let mut sender = Box::pin(run(cfg, metrics.clone(), rx, &mut cancel_rx));
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        let mut events = ordered_events(3);
        events[1].requested_model = "m".repeat(300 * 1024);
        for event in events.clone() {
            tx.try_send(event).unwrap();
        }
        tokio::time::advance(FLUSH_INTERVAL).await;
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || !batches.lock().unwrap().is_empty(),
            "nothing was delivered",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_recorded_events(&batches, &[events[0].clone(), events[2].clone()]);
        assert_eq!(drops_with_reason(&metrics, "oversized"), 1);
        let rendered = metrics.render();
        assert!(
            rendered
                .lines()
                .any(|line| line.starts_with("aisix_usage_event_drops_total{")
                    && line.contains("reason=\"oversized\"")
                    && line.contains("user_id=\"user-1\"")),
            "{rendered}"
        );
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
    }

    /// A batch this worker gives up on is gone — there is no retry — so the
    /// queue is the whole defence against a control plane that has gone
    /// slow. One POST is in flight at a time, and everything the proxy
    /// emits meanwhile has to fit.
    ///
    /// 1550 events is the shape a stability round measured behind an 8s
    /// control-plane stall at 173 req/s: the queue that held 1024 dropped
    /// 793 of them.
    #[tokio::test]
    async fn a_burst_arriving_while_one_post_is_in_flight_is_not_dropped() {
        // Staged first, to get the worker into a POST. The mock records a
        // request before it answers, so seeing the batch means the flush
        // is in flight rather than finished.
        const OPENING: usize = 150;
        const BURST: usize = 1_400;
        // Outlasts the staging below, which is a few microseconds of
        // non-blocking sends.
        const HELD: Duration = Duration::from_secs(5);

        let (server, batches) = recording_server_holding_first(HELD).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = TelemetryConfig::new(
            format!("{}/dp/telemetry", server.uri()),
            write_test_bundle(dir.path()),
        );
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (sink, worker) = spawn(cfg, Metrics::new(false), cancel_rx);

        let expected = ordered_events(OPENING + BURST);
        let mut events = expected.clone().into_iter();
        for event in events.by_ref().take(OPENING) {
            sink.try_emit("test", event, aisix_obs::UsageEventLabels::default());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while batches.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "no telemetry POST");
            tokio::task::yield_now().await;
        }

        // The control plane is now holding that POST, so nothing is being
        // drained while the rest of the burst arrives.
        for event in events {
            sink.try_emit("test", event, aisix_obs::UsageEventLabels::default());
        }
        drop(sink);
        worker.await.unwrap();

        assert_recorded_events(&batches, &expected);
    }

    /// A request served after the drain's `cancel` emits its event after
    /// `cancel` has flipped. The worker stops only once serving is over, so
    /// that event is still posted.
    #[tokio::test]
    async fn an_event_emitted_after_the_drain_ended_is_delivered() {
        let (server, batches) = recording_server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = TelemetryConfig::new(
            format!("{}/dp/telemetry", server.uri()),
            write_test_bundle(dir.path()),
        );
        let (stop_tx, stop_rx) = watch::channel(false);
        let (sink, worker) = spawn(cfg, Metrics::new(false), stop_rx);

        let expected = ordered_events(2);
        sink.try_emit(
            "test",
            expected[0].clone(),
            aisix_obs::UsageEventLabels::default(),
        );
        stop_after(
            async {
                // Long enough for a worker that had already been told to stop
                // to drain and exit before the late request completes.
                tokio::time::sleep(Duration::from_millis(200)).await;
                sink.try_emit(
                    "test",
                    expected[1].clone(),
                    aisix_obs::UsageEventLabels::default(),
                );
            },
            &stop_tx,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("the worker exits once serving is over")
            .unwrap();

        assert_recorded_events(&batches, &expected);
    }

    /// Stage `events` behind a paused clock and run the worker until the
    /// first POST of them is on the wire. Returns the driving future, the
    /// queue handle (so a test can keep emitting) and the cancel sender.
    ///
    /// Every re-send case below starts here: a batch has to be in flight
    /// before its failure can mean anything.
    async fn staged_sender<'a>(
        cfg: TelemetryConfig,
        metrics: Metrics,
        cancel_rx: &'a mut watch::Receiver<bool>,
        events: usize,
    ) -> (
        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
        tokio::sync::mpsc::Sender<UsageEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(BACKLOG_QUEUE);
        let mut sender: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> =
            Box::pin(run(cfg, metrics, rx, cancel_rx));
        // Consume the initial empty tick before staging the batch.
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        for event in ordered_events(events) {
            tx.try_send(event).unwrap();
        }
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        tokio::time::advance(FLUSH_INTERVAL).await;
        (sender, tx)
    }

    fn assert_identical_resends(posts: &RecordedPosts, attempts: usize) {
        let posts = posts.lock().unwrap();
        assert_eq!(posts.len(), attempts, "unexpected attempt count: {posts:?}");
        for post in posts.iter() {
            assert_eq!(
                (&post.0, &post.1),
                (&posts[0].0, &posts[0].1),
                "a re-send repeats the SAME batch id and the SAME events",
            );
        }
    }

    /// A `2xx` that names events the control plane refused is delivered and
    /// final — re-sending would be refused the same way — but it is a hole
    /// in the usage record, so it is counted and logged instead of passing
    /// silently.
    #[tokio::test]
    async fn rejected_events_in_an_accepted_batch_are_counted_and_not_resent() {
        let (server, posts) = scripted_server(vec![Answer {
            body: Some(r#"{"accepted":1,"rejected":1}"#),
            ..dedups(200)
        }])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let logs = CapturedLogs::default();
        let _subscriber = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        );
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        let rejected_recorded = || {
            metrics
                .render()
                .lines()
                .any(|line| line == "aisix_usage_events_rejected_total 1")
        };
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            rejected_recorded,
            "the rejection was never recorded",
        )
        .await;
        // Long past every re-send backoff: a retry would have happened.
        for _ in 0..20 {
            assert!(poll_sender(sender.as_mut()).await.is_pending());
            tokio::time::advance(RETRY_MAX_BACKOFF).await;
            tokio::task::yield_now().await;
        }
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_eq!(
            posts.lock().unwrap().len(),
            1,
            "a rejection is never re-sent"
        );
        assert!(rejected_recorded(), "{}", metrics.render());
        let logs = logs.text();
        let warn = logs
            .lines()
            .find(|line| line.contains("control plane rejected usage events"))
            .unwrap_or_else(|| panic!("no rejection WARN in: {logs}"));
        assert!(warn.contains("WARN"), "{warn}");
        assert!(
            warn.contains("rejected=1") && warn.contains("count=2"),
            "{warn}"
        );
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
    }

    /// An answer with nothing rejected — or no answer body at all, as an
    /// older control plane may send — records no rejection.
    #[tokio::test]
    async fn an_answer_without_rejections_records_none() {
        for body in [Some(r#"{"accepted":2,"rejected":0}"#), None] {
            let (server, _posts) = scripted_server(vec![Answer {
                body,
                ..dedups(200)
            }])
            .await;
            let dir = tempfile::tempdir().unwrap();
            let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
            let attempt = send(
                &plain_client(),
                &cfg,
                Uuid::new_v4(),
                &body_of(&[sample_event("r")]),
            )
            .await;
            assert!(
                matches!(attempt, Attempt::Delivered { rejected: 0, .. }),
                "{attempt:?}"
            );
        }
    }

    /// `tracing` writer that keeps what was logged for assertions.
    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The outage this exists for: the control plane answers `503`, and its
    /// answer proves a re-send cannot double-count, so the batch goes again
    /// — same id, same events — and lands.
    ///
    /// The second response also covers the duplicate acknowledgement: a
    /// batch the control plane already committed is answered with an
    /// ordinary `200`, which this worker must read as delivered rather than
    /// as anything special.
    #[tokio::test]
    async fn a_retryable_failure_with_the_dedup_header_resends_the_same_batch() {
        let (server, posts) = scripted_server(vec![dedups(503), dedups(200)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() >= 2,
            "the failed batch was never re-sent",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_identical_resends(&posts, 2);
        assert_eq!(
            posts.lock().unwrap()[0].1["events"]
                .as_array()
                .unwrap()
                .len(),
            2,
        );
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
        assert_eq!(drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED), 0);
    }

    /// The header is a capability assertion with one defined value. A
    /// response that carries it with anything else is a control plane this
    /// sender does not understand, and re-sending to it could bill twice —
    /// so it is treated exactly like a response that carries no header.
    #[tokio::test]
    async fn a_dedup_header_with_an_unknown_value_does_not_allow_a_re_send() {
        let (server, posts) = scripted_server(vec![dedup_value(503, "0")]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || drops_with_reason(&metrics, DROP_SEND_FAILED) == 2,
            "the batch was not dropped",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_eq!(posts.lock().unwrap().len(), 1, "it must not be re-sent");
        assert_eq!(drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED), 0);
    }

    /// Usage delivery is healthy again only once a POST reaches the control
    /// plane: a `2xx`, or a `422` judging the events. A `5xx` being re-sent
    /// and an auth refusal leave the events' wait running — strict budget
    /// enforcement reads exactly this.
    #[tokio::test]
    async fn delivery_health_recovers_only_on_a_post_that_reaches_the_control_plane() {
        let health_after = |answer: Answer| async move {
            let (server, posts) = scripted_server(vec![answer]).await;
            let dir = tempfile::tempdir().unwrap();
            let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
            let metrics = Metrics::new(false);
            let (_cancel_tx, mut cancel_rx) = watch::channel(false);
            let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;
            assert!(
                metrics.usage_delivery().is_waiting(),
                "received events wait until delivered"
            );
            // Two answers in for a re-sent status, one dropped batch for the
            // rest: either way the sender has read the first answer.
            drive_until(
                sender.as_mut(),
                STEP_WHILE_SERVING,
                || {
                    !metrics.usage_delivery().is_waiting()
                        || posts.lock().unwrap().len() >= 2
                        || drops_with_reason(&metrics, DROP_SEND_FAILED) == 2
                },
                "the first answer was never read",
            )
            .await;
            let waiting = metrics.usage_delivery().is_waiting();
            drop(tx);
            drop(sender);
            waiting
        };
        tokio::time::pause();
        assert!(health_after(dedups(503)).await, "a 503 reaches nothing");
        assert!(health_after(no_dedup(401)).await, "a 401 reaches nothing");
        assert!(health_after(no_dedup(403)).await, "a 403 reaches nothing");
        assert!(!health_after(no_dedup(422)).await, "a 422 reached it");
        assert!(!health_after(no_dedup(200)).await, "a 200 reached it");
    }

    /// A `400` is the control plane refusing THIS batch — a malformed body,
    /// a batch id it will not accept. Re-sending it changes nothing, so it
    /// is dropped on the first answer even though de-duplication is on.
    #[tokio::test]
    async fn a_non_retryable_status_is_dropped_even_when_dedup_is_supported() {
        let (server, posts) = scripted_server(vec![dedups(400)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || drops_with_reason(&metrics, DROP_SEND_FAILED) == 2,
            "the refused batch was not dropped",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_eq!(posts.lock().unwrap().len(), 1, "a 400 is not re-sent");
        assert_eq!(drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED), 0);
    }

    /// The failure a control-plane outage actually produces: no response at
    /// all. It proves nothing about the far side, so the worker falls back
    /// to the last response it DID see — here one that carried the header,
    /// so the batch is re-sent, and re-sent, until the budget that keeps it
    /// inside the billing window runs out. Its attempts reached the wire
    /// unanswered, so any of them may have been stored: the batch is counted
    /// as unconfirmed, not as lost.
    #[tokio::test]
    async fn no_response_after_a_dedup_bearing_response_resends_until_the_budget_ends() {
        // A retryable failure that proves de-duplication, and from then on
        // a control plane that never answers in time.
        let (server, posts) = scripted_server(vec![dedups(503), stalls()]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let started = Instant::now();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        // The first exchange is real HTTP — small steps, so the paused clock
        // cannot outrun it — and the second attempt going out is the proof
        // that its header-bearing response was received.
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() >= 2,
            "the failed batch was never re-sent",
        )
        .await;
        // Attempt 2 stalled, and the wait above has already delivered it.
        // This one takes the re-send AFTER it — attempt 3 — which is what
        // says a batch that met no response goes again. It stays on the
        // SMALL step because that delivery is what is being waited for:
        // at a second a step an attempt gets ten polls to cross a real
        // socket before its own 10s timeout cancels it, and a cancelled
        // request is never recorded. This wait replaces the `attempts > 2`
        // assertion that used to sit below it.
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() > 2,
            "the stalling batch was never re-sent",
        )
        .await;
        // Only now may the clock move in seconds, to run the budget out.
        drive_until(
            sender.as_mut(),
            STEP_WHILE_UNREACHABLE,
            || drops_with_reason(&metrics, DROP_SEND_UNCONFIRMED) == 2,
            "the batch was not re-sent until the budget ran out",
        )
        .await;
        drop(tx);

        assert!(
            started.elapsed() >= RETRY_BUDGET,
            "the batch was given up on after {:?}, before the budget",
            started.elapsed(),
        );
        // One lock per statement: `assert_identical_resends` takes the lock
        // itself, and a guard still alive from an argument expression would
        // deadlock against it.
        // `attempts > 2` is not asserted here: the wait above establishes
        // it, and repeating it after the fact would be a check that
        // cannot fail. What is worth saying is that every attempt carried
        // the same batch — and that is asserted under ONE lock, because a
        // count read in one statement and re-checked in the next can
        // disagree with itself if a late recording lands in between.
        let posts = posts.lock().unwrap();
        let first = (&posts[0].0, &posts[0].1);
        assert!(
            posts.iter().all(|post| (&post.0, &post.1) == first),
            "a re-send repeats the SAME batch id and the SAME events: {posts:?}",
        );
        assert_eq!(
            drops_with_reason(&metrics, DROP_SEND_FAILED),
            0,
            "a batch re-sent to exhaustion is accounted apart from one \
             that was never re-sent",
        );
        assert_eq!(
            drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED),
            0,
            "a batch that may have been stored is not counted as lost",
        );
    }

    /// The other direction of the same rule: the last response carried no
    /// header — an older replica behind the same Service, or a rolled-back
    /// control plane — so the failures that follow it are dropped where they
    /// fall. Caching "we saw the header once" is what this forbids. The
    /// attempt went out and was never answered, so the drop is counted as
    /// unconfirmed.
    #[tokio::test]
    async fn no_response_after_a_header_less_response_is_dropped_at_once() {
        // Fails with the header first, so the evidence IS set before the
        // header-less response clears it again.
        let (server, posts) = scripted_server(vec![dedups(503), no_dedup(200), stalls()]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        // Attempt 1 fails with the header, attempt 2 delivers without it.
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() >= 2,
            "the failed batch was never re-sent",
        )
        .await;

        for event in ordered_events(2) {
            tx.try_send(event).unwrap();
        }
        tokio::time::advance(FLUSH_INTERVAL).await;
        // Two phases, and the order is the point. The third attempt has to
        // REACH the server before the clock may outrun it — `posts` is
        // recorded by wiremock in real time — so it goes out on the small
        // step. Only once it has landed may the clock jump in seconds to
        // blow its request timeout.
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() >= 3,
            "the batch that met no response was never re-sent",
        )
        .await;
        drive_until(
            sender.as_mut(),
            STEP_WHILE_UNREACHABLE,
            || drops_with_reason(&metrics, DROP_SEND_UNCONFIRMED) == 2,
            "the batch was not dropped after a header-less response",
        )
        .await;

        assert_eq!(
            posts.lock().unwrap().len(),
            3,
            "the batch that met no response was not re-sent",
        );
        assert_eq!(drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED), 0);
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
    }

    /// While a batch is being re-sent nothing else is sent — the worker is
    /// single in-flight — so the queue is what holds the traffic that
    /// arrives meanwhile, and the event that meets a FULL queue is the new
    /// one, dropped as `sink_full` exactly as before. (The queue here is
    /// [`BACKLOG_QUEUE`] rather than [`QUEUE_CAPACITY`] for the reason
    /// given there.)
    #[tokio::test]
    async fn events_keep_queueing_while_a_batch_is_being_resent() {
        let (server, posts) = scripted_server(vec![dedups(503)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() >= 2,
            "the failed batch was never re-sent",
        )
        .await;

        let sink = UsageSink::new(tx.clone()).with_metrics(metrics.clone());
        for event in ordered_events(BACKLOG_QUEUE) {
            sink.try_emit("test", event, UsageEventLabels::default());
        }
        assert_eq!(tx.capacity(), 0, "the queue should now be full");
        assert_eq!(drops_with_reason(&metrics, "sink_full"), 0);
        sink.try_emit(
            "test",
            sample_event("overflow"),
            UsageEventLabels::default(),
        );
        assert_eq!(
            drops_with_reason(&metrics, "sink_full"),
            1,
            "a full queue drops the NEW event, not the batch in hand",
        );

        // …and the batch in hand is still the one being re-sent.
        let attempts = posts.lock().unwrap().len();
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() > attempts,
            "re-sending stopped while the queue filled",
        )
        .await;
        let posts = posts.lock().unwrap();
        let first = (&posts[0].0, &posts[0].1);
        assert!(
            posts.iter().all(|post| (&post.0, &post.1) == first),
            "the batch in hand must keep going unchanged: {posts:?}",
        );
    }

    /// The budget and the backoff schedule are normative in the cross-plane
    /// protocol — the control plane's `settleDelay` is chosen against this
    /// number — so their values are pinned literally. Every other test here
    /// compares the sender against itself and stays green if these move.
    #[test]
    fn the_retry_schedule_is_the_one_the_control_plane_was_sized_against() {
        assert_eq!(RETRY_BUDGET, Duration::from_secs(30 * 60));
        assert_eq!(RETRY_INITIAL_BACKOFF, Duration::from_secs(1));
        assert_eq!(RETRY_MAX_BACKOFF, Duration::from_secs(30));
    }

    /// Each wait lands uniformly within a fifth of its step either way, and
    /// the ceiling holds without waits piling up on it: a step at
    /// [`RETRY_MAX_BACKOFF`] is jittered around five sixths of it, so the
    /// band's top reaches the ceiling and every draw below the top stays
    /// distinct.
    #[test]
    fn a_re_send_wait_lands_within_a_fifth_of_its_step_and_under_the_ceiling() {
        let near = |actual: Duration, expected_ms: u64| {
            let expected = Duration::from_millis(expected_ms);
            let off = actual.max(expected) - actual.min(expected);
            assert!(
                off < Duration::from_millis(1),
                "{actual:?}, expected {expected:?}"
            );
        };
        let step = Duration::from_secs(4);
        near(jittered(step, 0.0), 3_200);
        near(jittered(step, 0.25), 3_600);
        near(jittered(step, 0.5), 4_000);
        near(jittered(step, 0.75), 4_400);
        near(jittered(step, 0.999_999), 4_800);

        near(jittered(RETRY_MAX_BACKOFF, 0.0), 20_000);
        near(jittered(RETRY_MAX_BACKOFF, 0.5), 25_000);
        near(jittered(RETRY_MAX_BACKOFF, 0.75), 27_500);
        near(jittered(RETRY_MAX_BACKOFF, 0.999_999), 30_000);
        assert!(jittered(RETRY_MAX_BACKOFF, 0.999_999) <= RETRY_MAX_BACKOFF);

        // The draw the sender takes stays in the band.
        for _ in 0..1_000 {
            let wait = random_jitter(step);
            assert!(
                (Duration::from_millis(3_200)..=Duration::from_millis(4_800)).contains(&wait),
                "{wait:?}",
            );
            let wait = random_jitter(RETRY_MAX_BACKOFF);
            assert!(
                (Duration::from_secs(20)..=RETRY_MAX_BACKOFF).contains(&wait),
                "{wait:?}",
            );
        }
    }

    /// A status is re-sent only when the control plane could take the same
    /// batch later. Everything else is it refusing this batch, which no
    /// number of re-sends changes.
    #[test]
    fn only_a_temporary_refusal_is_re_sent() {
        use reqwest::StatusCode;
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            assert!(is_retryable(status), "{status} must be re-sent");
        }
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            assert!(!is_retryable(status), "{status} is a refusal of THIS batch");
        }
    }

    /// What the sender-side drop samples can and cannot attribute. The
    /// member pair comes off the event, exactly as the queue's own drops
    /// take it; the model and provider-key dimensions are the emitting
    /// handler's label set, which the queue does not carry, so they read
    /// `unknown`. Documentation quotes this, so it is pinned rather than
    /// described.
    #[tokio::test]
    async fn a_sender_side_drop_names_the_member_and_nothing_else() {
        let (server, _posts) = scripted_server(vec![dedups(400)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || drops_with_reason(&metrics, DROP_SEND_FAILED) == 2,
            "the refused batch was not dropped",
        )
        .await;
        drop(tx);

        let rendered = metrics.render();
        let dropped: Vec<&str> = rendered
            .lines()
            .filter(|line| {
                line.starts_with("aisix_usage_event_drops_total{")
                    && line.contains("reason=\"send_failed\"")
            })
            .collect();
        // `ordered_events` gives each event its own member.
        assert_eq!(dropped.len(), 2, "one series per member: {dropped:?}");
        for line in &dropped {
            assert!(
                line.contains("model=\"unknown\"") && line.contains("provider_key_id=\"unknown\""),
                "the queue carries events, not the handler's labels: {line}",
            );
        }
        for member in ["user-0", "user-1"] {
            assert!(
                dropped
                    .iter()
                    .any(|line| line.contains(&format!("user_id=\"{member}\""))),
                "whose usage was lost must stay answerable, missing {member}: {dropped:?}",
            );
        }
    }

    /// The safety net for a control plane that answers `5xx` for a batch it
    /// can never accept: without a cap the single in-flight channel is held
    /// for the whole budget, and the queue behind it — every event the
    /// gateway records meanwhile — overflows long before that. A `500` is
    /// the control plane's permanent failure, and it counts even though it
    /// carries the de-duplication header.
    #[tokio::test]
    async fn a_batch_answered_and_refused_eight_times_is_given_up_on() {
        let (server, posts) = scripted_server(vec![dedups(500)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED) == 2,
            "the refused batch was re-sent past the cap",
        )
        .await;
        assert_identical_resends(&posts, MAX_ANSWERED_FAILURES);

        // …and the channel is free for what queued up behind it.
        for event in ordered_events(2) {
            tx.try_send(event).unwrap();
        }
        tokio::time::advance(FLUSH_INTERVAL).await;
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() > MAX_ANSWERED_FAILURES,
            "the next batch never got its turn",
        )
        .await;
        let posts = posts.lock().unwrap();
        assert_ne!(
            posts[MAX_ANSWERED_FAILURES].0, posts[0].0,
            "the batch after a give-up is a new batch, with its own id",
        );
    }

    /// A `503` with the de-duplication header is the control plane reporting
    /// a store outage that the same batch, re-sent unchanged, gets past — a
    /// database failover, a lock or pool timeout. It does not use up the
    /// cap, so an outage that outlasts eight answers ends with the batch
    /// delivered instead of dropped although it was never stored.
    #[tokio::test]
    async fn an_outage_the_control_plane_reports_as_one_does_not_drop_the_batch() {
        const OUTAGE: usize = 12;
        let mut script = vec![dedups(503); OUTAGE];
        script.push(dedups(200));
        let (server, posts) = scripted_server(script).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || {
                posts.lock().unwrap().len() > OUTAGE
                    || drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED) > 0
            },
            "the batch was neither re-sent past the outage nor given up on",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_eq!(
            drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED),
            0,
            "a reported outage used up the cap",
        );
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
        assert_identical_resends(&posts, OUTAGE + 1);
        assert!(
            !metrics.usage_delivery().is_waiting(),
            "the answer that ended the outage was read as delivered",
        );
    }

    /// A server that keeps reporting a store outage on the first batch it
    /// sees and stores every other one while `down` is false; while `down`
    /// is true it reports an outage on every batch. It records each post's
    /// batch id, event count and status.
    async fn server_with_an_outage(
        down: Arc<std::sync::atomic::AtomicBool>,
        first_batch_fails: bool,
    ) -> (
        LocalServer,
        Arc<std::sync::Mutex<Vec<(String, usize, u16)>>>,
    ) {
        let posts = Arc::new(std::sync::Mutex::new(Vec::<(String, usize, u16)>::new()));
        let recorded = Arc::clone(&posts);
        let server = local_server(move |headers, body| {
            let id = headers
                .get(USAGE_BATCH_ID_HEADER)
                .expect("every batch carries its id")
                .to_str()
                .unwrap()
                .to_string();
            let events = serde_json::from_slice::<serde_json::Value>(body).unwrap()["events"]
                .as_array()
                .unwrap()
                .len();
            let mut posts = recorded.lock().unwrap();
            let the_first = posts.first().is_none_or(|(first, ..)| *first == id);
            let fails =
                down.load(std::sync::atomic::Ordering::SeqCst) || (first_batch_fails && the_first);
            let status = if fails { 503 } else { 200 };
            posts.push((id, events, status));
            let mut reply = Reply::status(status);
            reply
                .headers
                .push((USAGE_BATCH_DEDUP_HEADER, USAGE_BATCH_DEDUP_ENABLED));
            reply
        })
        .await;
        (server, posts)
    }

    fn drops_of_every_reason(metrics: &Metrics) -> u64 {
        [
            DROP_RETRY_BUDGET_EXHAUSTED,
            DROP_SEND_FAILED,
            DROP_SEND_UNCONFIRMED,
        ]
        .iter()
        .map(|reason| drops_with_reason(metrics, reason))
        .sum()
    }

    /// A batch the store keeps failing on alone, while it stores the one
    /// behind it, holds the single in-flight sender for minutes, not for
    /// the whole budget. Past [`REPORTED_OUTAGE_BUDGET`], spread per batch,
    /// the batch behind it goes out once; it is stored, so the held batch
    /// gets one more attempt and, failing again, is given up on.
    #[tokio::test]
    async fn a_batch_the_store_keeps_failing_alone_is_given_up_on_once_the_next_is_stored() {
        let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (server, posts) = server_with_an_outage(down, true).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || !posts.lock().unwrap().is_empty(),
            "the batch never went out",
        )
        .await;
        let first = Instant::now();
        // What queues up behind it meanwhile.
        for event in ordered_events(3) {
            tx.try_send(event).unwrap();
        }
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED) == 2,
            "a batch the store kept failing on alone was never given up on",
        )
        .await;
        let held = first.elapsed();
        let low = REPORTED_OUTAGE_BUDGET.mul_f64(1.0 - RETRY_JITTER);
        let high = REPORTED_OUTAGE_BUDGET.mul_f64(1.0 + RETRY_JITTER) + RETRY_MAX_BACKOFF;
        assert!(
            (low..=high).contains(&held),
            "given up on {held:?} after its first answer"
        );
        assert_eq!(drops_of_every_reason(&metrics), 2);

        let posts = posts.lock().unwrap();
        let held_id = posts[0].0.clone();
        let probe = posts
            .iter()
            .position(|(id, ..)| *id != held_id)
            .expect("the batch behind it went out");
        assert_eq!(posts[probe].1, 3, "{posts:?}");
        assert_eq!(posts[probe].2, 200, "the batch behind it was stored");
        assert_eq!(
            posts[probe + 1].0,
            held_id,
            "the held batch was re-sent once more"
        );
        assert_eq!(posts.len(), probe + 2, "{posts:?}");
    }

    /// A store outage longer than [`REPORTED_OUTAGE_BUDGET`] loses nothing
    /// the budget would keep. Each window ends with the next batch posted
    /// once, under one id every time; the store takes it no more than the
    /// held batch, so both are kept, and once the store answers again both
    /// are stored, in order.
    #[tokio::test]
    async fn a_store_outage_longer_than_the_window_loses_nothing() {
        let down = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (server, posts) = server_with_an_outage(Arc::clone(&down), false).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || !posts.lock().unwrap().is_empty(),
            "the batch never went out",
        )
        .await;
        for event in ordered_events(3) {
            tx.try_send(event).unwrap();
        }
        // Two windows and more.
        let outage_ends = Instant::now() + Duration::from_secs(12 * 60);
        drive_until(
            sender.as_mut(),
            Duration::from_millis(20),
            || Instant::now() >= outage_ends,
            "the outage never ran its course",
        )
        .await;
        assert_eq!(
            drops_of_every_reason(&metrics),
            0,
            "dropped during the outage"
        );
        let probes: std::collections::BTreeSet<String> = {
            let posts = posts.lock().unwrap();
            let held_id = posts[0].0.clone();
            posts
                .iter()
                .filter(|(id, ..)| *id != held_id)
                .map(|(id, ..)| id.clone())
                .collect()
        };
        assert_eq!(probes.len(), 1, "the probe keeps one id: {probes:?}");

        down.store(false, std::sync::atomic::Ordering::SeqCst);
        let stored = |posts: &[(String, usize, u16)]| -> usize {
            posts
                .iter()
                .filter(|(.., status)| *status == 200)
                .map(|(_, events, _)| *events)
                .sum()
        };
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || stored(&posts.lock().unwrap()) == 5,
            "the batches held through the outage were never stored",
        )
        .await;
        assert_eq!(drops_of_every_reason(&metrics), 0);
        let posts = posts.lock().unwrap();
        let stored_ids: Vec<&String> = posts
            .iter()
            .filter(|(.., status)| *status == 200)
            .map(|(id, ..)| id)
            .collect();
        assert_eq!(
            stored_ids,
            [&posts[0].0, probes.iter().next().unwrap()],
            "stored once each, the held batch first"
        );
    }

    /// The window is drawn per batch, within a fifth of it either way, so
    /// gateways that met one outage together do not all give up together.
    #[test]
    fn a_reported_outage_window_is_spread_around_five_minutes() {
        assert_eq!(REPORTED_OUTAGE_BUDGET, Duration::from_secs(5 * 60));
        assert_eq!(
            spread(REPORTED_OUTAGE_BUDGET, 0.0),
            Duration::from_secs(4 * 60)
        );
        assert_eq!(
            spread(REPORTED_OUTAGE_BUDGET, 0.5),
            Duration::from_secs(5 * 60)
        );
    }

    /// A gateway that boots while the control plane is unreachable has seen
    /// no response yet, so nothing says re-sending is safe — but a
    /// connection that could not be made sent nothing, and re-sending it
    /// cannot count anything twice. The batch waits for the control plane,
    /// within the budget, and lands once it answers, even from one that does
    /// not de-duplicate.
    #[tokio::test]
    async fn a_batch_that_could_not_connect_waits_for_the_control_plane() {
        // Bound but not listening: every connection is refused until the
        // control plane "comes up" on the same address.
        let (socket, addr) = crate::dp_store::test_logs::refusing_addr();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("http://{addr}/dp/telemetry"), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        let down_until = Instant::now() + Duration::from_secs(10);
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || Instant::now() >= down_until,
            "the sender stopped while the control plane was down",
        )
        .await;
        for reason in [
            DROP_SEND_FAILED,
            DROP_SEND_UNCONFIRMED,
            DROP_RETRY_BUDGET_EXHAUSTED,
        ] {
            assert_eq!(drops_with_reason(&metrics, reason), 0, "{reason}");
        }

        socket.listen(128).unwrap();
        socket.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(socket.into()).unwrap();
        let (_server, posts) = scripted_server_on(listener, vec![no_dedup(200)]);
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || !posts.lock().unwrap().is_empty(),
            "the batch never reached the control plane once it was up",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_identical_resends(&posts, 1);
        assert_eq!(
            posts.lock().unwrap()[0].1["events"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
    }

    /// An attempt that went out and was never answered may have been
    /// stored, so giving its batch up is counted as unconfirmed: the stored
    /// rows then lie between `expected - drops - unconfirmed` and
    /// `expected - drops`.
    #[tokio::test]
    async fn a_batch_given_up_on_after_an_unanswered_attempt_is_unconfirmed() {
        let (server, posts) = scripted_server(vec![stalls()]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || !posts.lock().unwrap().is_empty(),
            "the batch never went out",
        )
        .await;
        drive_until(
            sender.as_mut(),
            STEP_WHILE_UNREACHABLE,
            || drops_with_reason(&metrics, DROP_SEND_UNCONFIRMED) == 2,
            "the unanswered batch was not counted as unconfirmed",
        )
        .await;
        drop(tx);
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
        assert_eq!(
            posts.lock().unwrap().len(),
            1,
            "nothing proved a re-send safe"
        );
    }

    /// The exemption is read off the header, never off the status alone. A
    /// `503` without it — from a proxy in front of the control plane, or an
    /// older control plane — proves nothing about de-duplication, so it ends
    /// the batch on its answer exactly as before, even in the middle of an
    /// outage the control plane did report.
    #[tokio::test]
    async fn a_503_without_the_dedup_header_is_dropped_on_its_first_answer() {
        let (server, posts) =
            scripted_server(vec![dedups(503), dedups(503), no_dedup(503), dedups(200)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || drops_with_reason(&metrics, DROP_SEND_FAILED) > 0 || posts.lock().unwrap().len() > 3,
            "the batch was neither dropped nor re-sent after the header-less 503",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;

        assert_identical_resends(&posts, 3);
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 2);
        assert_eq!(drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED), 0);
    }

    /// A reported outage between permanent failures neither counts nor
    /// clears the count: the `500`s on either side of each `503` still add
    /// up to the cap, so the batch is given up on at its eighth `500`.
    #[tokio::test]
    async fn a_reported_outage_between_refusals_neither_counts_nor_resets_the_cap() {
        let script: Vec<Answer> = (0..MAX_ANSWERED_FAILURES * 2)
            .map(|i| if i % 2 == 0 { dedups(503) } else { dedups(500) })
            .collect();
        let (server, posts) = scripted_server(script).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || {
                drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED) == 2
                    || posts.lock().unwrap().len() > MAX_ANSWERED_FAILURES * 2
            },
            "the batch was neither given up on nor re-sent past the cap",
        )
        .await;
        // Before the exit: a sender still re-sending would hold it open.
        assert_identical_resends(&posts, MAX_ANSWERED_FAILURES * 2);
        assert_eq!(drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED), 2);
        drop(tx);
        finish_sender(sender.as_mut()).await;
    }

    /// The wait before a re-send is the one the jitter returns, not the bare
    /// step of the schedule. The jitter is swapped for a probe no schedule
    /// produces — twenty seconds where the first step is one — and the gap
    /// between the two attempts is read off the paused clock.
    #[tokio::test]
    async fn every_re_send_wait_is_drawn_through_the_jitter() {
        const PROBE: Duration = Duration::from_secs(20);
        let (server, posts) = scripted_server(vec![dedups(503)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        let mut state = SenderState {
            jitter: |_| PROBE,
            ..SenderState::default()
        };
        let client = plain_client();
        let batch = Parked {
            id: Uuid::new_v4(),
            events: prepared(ordered_events(2)),
            maybe_stored: false,
        };
        tokio::time::pause();
        let mut delivering = Box::pin(deliver(
            &client,
            &cfg,
            &metrics,
            &mut state,
            &batch,
            Retry::Allowed,
            &mut cancel_rx,
            None,
        ));

        drive_until(
            delivering.as_mut(),
            STEP_WHILE_SERVING,
            || !posts.lock().unwrap().is_empty(),
            "the batch never went out",
        )
        .await;
        let first = Instant::now();
        drive_until(
            delivering.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() >= 2,
            "the batch was never re-sent",
        )
        .await;
        let gap = first.elapsed();
        assert!(
            (PROBE..PROBE + Duration::from_secs(1)).contains(&gap),
            "re-sent {gap:?} after the first attempt; the jitter returned {PROBE:?}",
        );
    }

    /// The cap counts answered failures only. A control plane that has
    /// stopped answering is the outage this feature exists for, and it is
    /// bounded by the budget instead — so attempts that time out neither
    /// consume the cap nor are stopped by it. The batch given up on at the
    /// cap had attempts that went unanswered, so it is counted as
    /// unconfirmed.
    #[tokio::test]
    async fn failures_with_no_response_do_not_consume_the_cap() {
        // Alternating: every second attempt is answered with a failure that
        // counts, the rest stall past the request timeout.
        let script: Vec<Answer> = (0..40)
            .map(|i| if i % 2 == 0 { dedups(500) } else { stalls() })
            .collect();
        let (server, posts) = scripted_server(script).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        // Small steps throughout: the answered attempts are real exchanges,
        // and a clock that outran one would turn it into a no-response
        // failure — the very distinction this test is about.
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || drops_with_reason(&metrics, DROP_SEND_UNCONFIRMED) == 2,
            "the batch was never given up on",
        )
        .await;
        // The stalled attempts in between are recorded on wiremock's
        // clock, and the exact count below includes them. Its own wait,
        // not a conjunct of the one above: folded together, a recording
        // that never arrives would spend the wall-clock deadline and then
        // report that the batch was never given up on — which it was.
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() >= MAX_ANSWERED_FAILURES * 2 - 1,
            "the batch was given up on before its fifteenth attempt — a \
             stalled attempt consumed the cap",
        )
        .await;
        drop(tx);

        // Answered attempts are the odd ones, so the eighth of them is the
        // fifteenth attempt — the stalls in between cost nothing.
        assert_identical_resends(&posts, MAX_ANSWERED_FAILURES * 2 - 1);
    }

    /// The budget is measured from the batch's OLDEST event, not from its
    /// first attempt: the sender is single in-flight, so a batch that waited
    /// behind a long re-send is already old when its turn comes, and sending
    /// it would land rows in an hour the control plane has already billed.
    /// A younger batch behind it is unaffected.
    #[tokio::test]
    async fn a_batch_older_than_the_budget_is_dropped_without_an_attempt() {
        let (server, posts) = scripted_server(vec![dedups(200)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        let (tx, rx) = tokio::sync::mpsc::channel(BACKLOG_QUEUE);
        tokio::time::pause();
        let mut sender = Box::pin(run(cfg, metrics.clone(), rx, &mut cancel_rx));
        assert!(poll_sender(sender.as_mut()).await.is_pending());

        for event in aged_events(2, RETRY_BUDGET + Duration::from_secs(60)) {
            tx.try_send(event).unwrap();
        }
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        tokio::time::advance(FLUSH_INTERVAL).await;
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED) == 2,
            "the stale batch was not dropped",
        )
        .await;
        assert!(
            posts.lock().unwrap().is_empty(),
            "a batch past the billing window must not be sent at all",
        );

        // The batch behind it is fresh, and goes out normally.
        for event in ordered_events(2) {
            tx.try_send(event).unwrap();
        }
        tokio::time::advance(FLUSH_INTERVAL).await;
        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || !posts.lock().unwrap().is_empty(),
            "the fresh batch behind it never went out",
        )
        .await;
        drop(tx);
        finish_sender(sender.as_mut()).await;
        assert_eq!(posts.lock().unwrap().len(), 1);
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 0);
    }

    /// Shutdown gets one last attempt at the batch in hand and then stops —
    /// it never waits out a backoff. The proof is that the worker exits
    /// without this test advancing the clock: every remaining backoff is
    /// still pending on the paused timer.
    #[tokio::test]
    async fn shutdown_does_not_wait_on_a_retrying_batch() {
        let (server, posts) = scripted_server(vec![dedups(503)]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let (mut sender, tx) = staged_sender(cfg, metrics.clone(), &mut cancel_rx, 2).await;

        drive_until(
            sender.as_mut(),
            STEP_WHILE_SERVING,
            || posts.lock().unwrap().len() >= 2,
            "the failed batch was never re-sent",
        )
        .await;
        let attempts = posts.lock().unwrap().len();

        // Queued behind the batch in hand, so the drain has something of its
        // own to post — a backoff waited out there would hold shutdown open
        // just as surely.
        for event in ordered_events(2) {
            tx.try_send(event).unwrap();
        }
        cancel_tx.send(true).unwrap();
        // Keeps tx alive: the exit must come from cancellation, and from no
        // further virtual time passing.
        finish_sender(sender.as_mut()).await;

        assert_eq!(
            posts.lock().unwrap().len(),
            attempts + 2,
            "shutdown makes one final attempt at the batch in hand and one \
             at what it drains",
        );
        assert_eq!(drops_with_reason(&metrics, DROP_SEND_FAILED), 4);
        assert_eq!(drops_with_reason(&metrics, DROP_RETRY_BUDGET_EXHAUSTED), 0);
    }

    /// One deadline for the whole drain, not one timeout per batch. A control
    /// plane that stops answering is what fills the queue, and the exit must
    /// still happen before the platform kills it, with what it could not send
    /// counted.
    #[tokio::test]
    async fn a_control_plane_that_stopped_answering_cannot_hold_the_exit() {
        const QUEUED: usize = MAX_BATCH * 8;
        const STEP: Duration = Duration::from_millis(250);

        let (server, posts) = scripted_server(vec![Answer {
            stalled: true,
            ..dedups(200)
        }])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(format!("{}/dp/telemetry", server.uri()), &dir);
        let metrics = Metrics::new(false);
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUED);
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        tokio::time::pause();
        let mut sender = Box::pin(run(cfg, metrics.clone(), rx, &mut cancel_rx));
        assert!(poll_sender(sender.as_mut()).await.is_pending());
        for event in ordered_events(QUEUED) {
            tx.try_send(event).unwrap();
        }
        cancel_tx.send(true).unwrap();

        let started = Instant::now();
        let wall_clock_deadline = std::time::Instant::now() + Duration::from_secs(60);
        while poll_sender(sender.as_mut()).await.is_pending() {
            assert!(
                started.elapsed() <= FINAL_DRAIN_BUDGET + Duration::from_secs(1),
                "still posting {:?} into the stall, after {} POSTs",
                started.elapsed(),
                posts.lock().unwrap().len(),
            );
            assert!(
                std::time::Instant::now() < wall_clock_deadline,
                "wall clock"
            );
            tokio::time::advance(STEP).await;
            tokio::task::yield_now().await;
        }

        // A batch that went out unanswered may have been stored — the first
        // timed out on its own, the second was still in flight at the
        // deadline — and one the drain never reached was not.
        let unconfirmed = drops_with_reason(&metrics, DROP_SEND_UNCONFIRMED);
        let unsent = drops_with_reason(&metrics, DROP_SEND_FAILED);
        assert_eq!(
            unconfirmed + unsent,
            QUEUED as u64,
            "everything the drain could not deliver is counted",
        );
        assert!(
            unconfirmed >= 2 * MAX_BATCH as u64,
            "the batches that went out unanswered: {unconfirmed}"
        );
        assert!(
            unsent >= MAX_BATCH as u64,
            "the batches the drain never sent: {unsent}"
        );
        assert!(tx.is_closed());
    }

    /// `managed.cp_base_url` may carry a password. It is used as configured
    /// and displayed nowhere: not in `Debug`, the startup line, or the text of
    /// either kind of failure, both of which are logged.
    #[tokio::test]
    async fn a_password_in_the_url_reaches_no_log_line_failure_or_debug_output() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let events = [sample_event("req-1")];
        let answered = test_config(
            format!(
                "{}/dp/telemetry",
                server.uri().replacen("http://", "http://user:hunter2@", 1)
            ),
            &dir,
        );
        let debug = format!("{answered:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        let attempt = send(
            &plain_client(),
            &answered,
            Uuid::new_v4(),
            &body_of(&events),
        )
        .await;
        let Attempt::Failed(failure @ SendFailure::Response { .. }) = attempt else {
            panic!("expected the 400 back: {attempt:?}");
        };
        let text = failure.to_string();
        assert!(text.contains("400"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");

        let (_refusing, addr) = crate::dp_store::test_logs::refusing_addr();
        let unanswered = test_config(format!("http://user:hunter2@{addr}/dp/telemetry"), &dir);
        let attempt = send(
            &plain_client(),
            &unanswered,
            Uuid::new_v4(),
            &body_of(&events),
        )
        .await;
        let Attempt::Failed(failure @ SendFailure::NoResponse { .. }) = attempt else {
            panic!("expected no response: {attempt:?}");
        };
        let text = failure.to_string();
        assert!(text.to_lowercase().contains("refused"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");

        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let (_keep, mut cancel) = watch::channel(false);
        let (_, logged) = crate::dp_store::test_logs::capture_logs(tokio::time::timeout(
            Duration::from_millis(200),
            run(unanswered, Metrics::new(false), rx, &mut cancel),
        ))
        .await;
        assert!(logged.contains("telemetry sender started"), "{logged}");
        assert!(!logged.contains("hunter2"), "{logged}");
    }
}
