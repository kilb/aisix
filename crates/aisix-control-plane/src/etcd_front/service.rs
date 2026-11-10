//! The etcd front's gRPC services: KV and Watch, forwarded to kine for an
//! admitted gateway, inside its key spaces, reads only.
//!
//! Every RPC first admits the gateway by its connection's certificate, by
//! the rule the `/dp` endpoints use (`dp::admitted`). A gateway it refuses
//! is answered `PermissionDenied` and its connection is closed, as a
//! refused credential's REST connection is. A range, or a watch create,
//! outside the gateway's key spaces is [`Denial::OutsidePrefix`], and a
//! write [`Denial::ReadOnly`]. What kine answers is passed back as it is,
//! errors included: the gateway's handling of compaction, and of kine
//! cancelling a watch whose reader fell behind, was written against
//! kine's own answers.
//!
//! An open watch holds its credential to account for as long as it lasts:
//! it is checked again on every change to the credential list and every
//! [`RECHECK_EVERY`], and a watch whose credential no longer admits it ends
//! `PermissionDenied`, its connection closed.
//!
//! What kine answers is also held to the key spaces, key by key: a range's
//! keys and a watch's events outside them are dropped, and logged by count
//! as an error (at most once per [`LEAK_LOG_EVERY`]). A range that dropped
//! keys reports the count of what it kept, and a page left empty while kine
//! says more follow is an error, never the end of the gateway's load. A
//! `count_only` range is refused: it would count what cannot be filtered.
//! kine compares keys in its table's collation, which is
//! bytewise only when its `name` column is `COLLATE "C"`: kine creates it
//! so, but leaves an older table as it was, and creates none on
//! CockroachDB. Under any other collation `[key, range_end)` is not a
//! prefix, and another environment's keys can fall inside it.
//!
//! What the front holds is bounded: at most [`MAX_WATCHES`] watch calls,
//! [`MAX_WATCHES_PER_CREDENTIAL`] of one credential's, each with one
//! connection to kine, at most [`MAX_CREATES_PER_CALL`] watch on it, and
//! [`WATCH_BUFFER`] answers queued each way, none longer than
//! [`MAX_WATCH_ANSWER`]; and at most [`MAX_RANGES`] ranges read
//! from kine at once, each answer at most [`MAX_RANGE_ANSWER`] bytes. A
//! range waits up to [`RANGE_WAIT`] for its turn. Past a cap a gateway is
//! told `ResourceExhausted`, which it retries.
//!
//! Nothing read from kine is logged: the documents carry plaintext
//! credentials.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Extensions, Request, Response, Status, Streaming};
use uuid::Uuid;

use super::policy::{check_range, AllowedPrefixes, Denial};
use super::proto::etcdserverpb::kv_client::KvClient;
use super::proto::etcdserverpb::kv_server::Kv;
use super::proto::etcdserverpb::watch_client::WatchClient;
use super::proto::etcdserverpb::watch_request::RequestUnion;
use super::proto::etcdserverpb::watch_server::Watch;
use super::proto::etcdserverpb::{
    CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest,
    PutResponse, RangeRequest, RangeResponse, TxnRequest, TxnResponse, WatchRequest, WatchResponse,
};
use crate::dp::{self, ConnClose, ConnIdentity, CredentialCache};
use crate::settings::EnvSettings;

/// How often an open watch checks its credential again, whatever the
/// credential list announces: the bound for a credential that expires,
/// which nothing announces.
pub const RECHECK_EVERY: Duration = Duration::from_secs(10);

/// Watch calls open at once. Each holds a connection to kine of its own,
/// one file descriptor (counted in `ConnLimits::fds_needed`).
pub const MAX_WATCHES: usize = 1024;

/// Watch calls one credential may hold open at once. A gateway holds two,
/// its environment's and the global prefix's, and a credential is shared
/// by the replicas of a fleet, up to `MAX_INSTANCES` of them.
pub const MAX_WATCHES_PER_CREDENTIAL: usize = 2 * crate::dp::heartbeat::MAX_INSTANCES as usize;

/// Watches one watch call may create. kine keeps every watch a call
/// creates until it is cancelled or the call ends, each with a task of its
/// own, so the caps on calls would bound nothing if one call could create
/// any number. A gateway creates one watch per call (`aisix-etcd`'s
/// `EtcdConfigProvider::watch` opens a call for each), and a second create
/// on a call ends it `ResourceExhausted`.
pub const MAX_CREATES_PER_CALL: usize = 1;

/// Answers queued each way on one watch call. Bounded, so a gateway that
/// stops reading stops the front reading kine, and kine's own slow-reader
/// cancel reaches the gateway as it would directly. An answer from kine
/// merges every event queued for the watch (22 MB has been seen), so what
/// one call can hold is this many such answers, not this many bytes.
pub const WATCH_BUFFER: usize = 4;

/// The largest range answer the front reads from kine: 64 MiB. The
/// gateway's load reads pages of 500 keys (`LOAD_PAGE_SIZE` in
/// aisix-etcd), so a full page may average 128 KiB a document; the control
/// plane stores none over 1 MiB. tonic refuses a longer answer before
/// reading it.
pub const MAX_RANGE_ANSWER: usize = 64 << 20;

/// Ranges read from kine at once, across every gateway. A gateway's load
/// reads its pages one after another. A range holds its place while it is
/// read from kine and filtered, and gives it back when it returns its
/// answer: so with [`MAX_RANGE_ANSWER`] this bounds the answers being read,
/// and each range call may still hold one more answer queued for sending to
/// its gateway, outside the cap.
pub const MAX_RANGES: usize = 8;

/// How long a range waits for a place among [`MAX_RANGES`] before it is
/// refused `ResourceExhausted`. Waiting rather than refusing at once: a
/// refused page restarts the gateway's whole load after its backoff (1 s
/// doubling to 60 s, without jitter), so a fleet restarted together would
/// otherwise retry in lockstep.
pub const RANGE_WAIT: Duration = Duration::from_secs(10);

/// The largest watch answer the front reads from kine: 64 MiB. kine merges
/// every event queued for a watch into one answer, and 22 MB has been
/// seen. A longer one ends the watch `ResourceExhausted`; the gateway then
/// loads again, by pages, and watches from there.
pub const MAX_WATCH_ANSWER: usize = 64 << 20;

/// At most one error about dropped keys this often, with the count dropped
/// since the last.
pub const LEAK_LOG_EVERY: Duration = Duration::from_secs(60);

/// How often the connection to kine is pinged while a stream is open (kine
/// allows no pings without one, and none more often than every 5 s).
const KINE_KEEPALIVE: Duration = Duration::from_secs(30);

/// What a gateway whose credential is refused is told.
const CREDENTIAL_REFUSED: &str = "etcdserver: permission denied: gateway credential refused";

/// The start of the message tonic 0.12.3 refuses an over-long message with
/// (`codec/decode.rs`), as `OutOfRange`.
const TOO_LONG: &str = "Error, decoded message length too large";

/// What a gateway is told of an answer longer than the front reads.
const RANGE_TOO_LONG: &str = "etcdserver: range answer too large";
const WATCH_TOO_LONG: &str = "etcdserver: watch answer too large";

/// Logged, with the count ([`LeakLog`]), when kine answers with keys
/// outside the key spaces.
const COLLATION_LEAK: &str = "kine answered with keys outside the requested range, which the \
    etcd front dropped: kine's `name` column is not COLLATE \"C\" (a table created by an \
    older kine, or on CockroachDB), so its ranges are not prefixes";

/// kine's endpoint, as the front dials it: `CP_KINE_ENDPOINT`, plaintext,
/// on this host.
pub fn kine_endpoint(url: &str) -> Result<Endpoint, tonic::transport::Error> {
    Ok(Endpoint::from_shared(url.to_string())?
        .http2_keep_alive_interval(KINE_KEEPALIVE)
        .keep_alive_while_idle(false))
}

/// The KV and Watch services, for one environment. Cheap to clone.
#[derive(Clone)]
pub struct Front {
    /// Ranges share one connection to kine.
    shared: Channel,
    /// Each watch call gets a connection to kine of its own: the HTTP/2
    /// streams of one connection share its flow-control window, so a
    /// gateway that stops reading would otherwise stall every other
    /// gateway's watch.
    endpoint: Endpoint,
    creds: CredentialCache,
    env: EnvSettings,
    allowed: Arc<AllowedPrefixes>,
    /// Places for watch calls: [`MAX_WATCHES`] in all.
    watches: Arc<Semaphore>,
    /// [`MAX_WATCHES_PER_CREDENTIAL`] places for each credential. Not
    /// pruned: it holds one entry per credential admitted since boot, and
    /// the credential cache already holds every credential listed.
    credential_watches: Arc<Mutex<HashMap<Uuid, Arc<Semaphore>>>>,
    /// Places for ranges being read: [`MAX_RANGES`].
    ranges: Arc<Semaphore>,
    /// How long a range waits for a place: [`RANGE_WAIT`].
    range_wait: Duration,
    leaks: Arc<LeakLog>,
}

impl Front {
    /// The front for `env`'s gateways, admitted by `creds`, reading kine at
    /// `kine`. Connects to kine on first use.
    pub fn new(kine: Endpoint, creds: CredentialCache, env: EnvSettings) -> Front {
        Front {
            shared: kine.connect_lazy(),
            endpoint: kine,
            allowed: Arc::new(AllowedPrefixes::of(&env)),
            creds,
            env,
            watches: Arc::new(Semaphore::new(MAX_WATCHES)),
            credential_watches: Arc::default(),
            ranges: Arc::new(Semaphore::new(MAX_RANGES)),
            range_wait: RANGE_WAIT,
            leaks: Arc::default(),
        }
    }

    /// This front, with ranges waiting `wait` for a place instead of
    /// [`RANGE_WAIT`]: for tests, which cannot wait that long.
    #[doc(hidden)]
    pub fn with_range_wait(mut self, wait: Duration) -> Front {
        self.range_wait = wait;
        self
    }

    /// A place for one more of `caller`'s watch calls, under both caps, or
    /// `None` when either is full.
    fn watch_place(&self, caller: &Caller) -> Option<WatchPlace> {
        let credential = self
            .credential_watches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(caller.dp_id)
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_WATCHES_PER_CREDENTIAL)))
            .clone();
        let credential = credential.try_acquire_owned().ok()?;
        let all = self.watches.clone().try_acquire_owned().ok()?;
        Some(WatchPlace {
            _all: all,
            _credential: credential,
        })
    }

    /// Admits the gateway a request comes from, by the `extensions` the
    /// listener attached to it, or refuses it (`PermissionDenied`, its
    /// connection closed).
    async fn admit(&self, extensions: &Extensions) -> Result<Caller, Status> {
        let close = extensions.get::<ConnClose>().cloned();
        // Only the gateway listener attaches one.
        let Some(conn) = extensions.get::<ConnIdentity>() else {
            return Err(refuse(close.as_ref()));
        };
        match dp::admitted(conn, &self.env, &self.creds).await {
            Ok(Some(dp_id)) => Ok(Caller {
                dp_id,
                fingerprint: conn.fingerprint.clone(),
                close,
            }),
            Ok(None) => Err(refuse(close.as_ref())),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the gateway credentials to admit an etcd call");
                Err(Status::unavailable(
                    "etcdserver: the gateway credential cannot be checked now",
                ))
            }
        }
    }

    /// Whether `caller`'s credential, admitted once, still admits it, by
    /// the credential list as it is held now.
    fn still_admitted(&self, caller: &Caller) -> bool {
        self.creds
            .is_valid(&caller.dp_id.to_string(), &caller.fingerprint)
    }

    /// The refusal of `[key, range_end)`, when it lies outside the
    /// gateway's key spaces.
    fn check(&self, caller: &Caller, key: &[u8], range_end: &[u8]) -> Option<Status> {
        let denial = check_range(&self.allowed, key, range_end).err()?;
        tracing::info!(dp_id = %caller.dp_id,
            "refused an etcd read outside the gateway's environment");
        Some(self.refused(denial))
    }

    fn refused(&self, denial: Denial) -> Status {
        denial.into_status(self.env.env_id)
    }
}

/// A gateway admitted for one call, and its connection's close handle.
#[derive(Clone)]
struct Caller {
    dp_id: Uuid,
    fingerprint: String,
    close: Option<ConnClose>,
}

impl Caller {
    fn refuse(&self) -> Status {
        refuse(self.close.as_ref())
    }
}

/// The refusal of a credential: `PermissionDenied`, and the connection it
/// came on closed.
fn refuse(close: Option<&ConnClose>) -> Status {
    if let Some(close) = close {
        close.close();
    }
    Status::permission_denied(CREDENTIAL_REFUSED)
}

#[tonic::async_trait]
impl Kv for Front {
    async fn range(
        &self,
        request: Request<RangeRequest>,
    ) -> Result<Response<RangeResponse>, Status> {
        let caller = self.admit(request.extensions()).await?;
        let range = request.into_inner();
        if let Some(refused) = self.check(&caller, &range.key, &range.range_end) {
            return Err(refused);
        }
        // The gateway reads pages and their last key, never a count.
        if range.count_only {
            return Err(Status::invalid_argument(
                "etcdserver: count_only is not served",
            ));
        }
        // The semaphore is never closed: the only failure is the wait.
        let Ok(Ok(_reading)) = tokio::time::timeout(self.range_wait, self.ranges.acquire()).await
        else {
            tracing::warn!(dp_id = %caller.dp_id, wait_ms = self.range_wait.as_millis() as u64,
                "refused an etcd range: no place among the ranges the front reads at once");
            return Err(Status::resource_exhausted("etcdserver: too many ranges"));
        };
        let mut answer = KvClient::new(self.shared.clone())
            .max_decoding_message_size(MAX_RANGE_ANSWER)
            .range(range)
            .await
            .map_err(|status| too_long(&caller, status, MAX_RANGE_ANSWER, RANGE_TOO_LONG))?
            .into_inner();
        let read = answer.kvs.len();
        answer.kvs.retain(|kv| self.allowed.contains(&kv.key));
        if answer.kvs.len() < read {
            self.leaks.note(read - answer.kvs.len());
            answer.count = answer.kvs.len() as i64;
        }
        // kine never says more follow on an empty page; after the filter,
        // an empty one would end the gateway's load short.
        if answer.more && answer.kvs.is_empty() {
            return Err(Status::internal(
                "etcdserver: a range page held no key inside the range",
            ));
        }
        Ok(Response::new(answer))
    }

    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        self.admit(request.extensions()).await?;
        Err(self.refused(Denial::ReadOnly))
    }

    async fn delete_range(
        &self,
        request: Request<DeleteRangeRequest>,
    ) -> Result<Response<DeleteRangeResponse>, Status> {
        self.admit(request.extensions()).await?;
        Err(self.refused(Denial::ReadOnly))
    }

    async fn txn(&self, request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        self.admit(request.extensions()).await?;
        Err(self.refused(Denial::ReadOnly))
    }

    async fn compact(
        &self,
        request: Request<CompactionRequest>,
    ) -> Result<Response<CompactionResponse>, Status> {
        self.admit(request.extensions()).await?;
        Err(self.refused(Denial::ReadOnly))
    }
}

#[tonic::async_trait]
impl Watch for Front {
    type WatchStream = ReceiverStream<Result<WatchResponse, Status>>;

    /// One call to kine per gateway call, on a connection of its own, and
    /// two tasks: [`Front::forward_requests`] and [`Front::relay_answers`].
    async fn watch(
        &self,
        request: Request<Streaming<WatchRequest>>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let caller = self.admit(request.extensions()).await?;
        let Some(place) = self.watch_place(&caller) else {
            tracing::warn!(dp_id = %caller.dp_id,
                "refused an etcd watch: the front holds as many watches as it may");
            return Err(Status::resource_exhausted("etcdserver: too many watches"));
        };
        let (to_kine, from_gateway) = mpsc::channel(WATCH_BUFFER);
        let (to_gateway, answers) = mpsc::channel(WATCH_BUFFER);
        let end = CallEnd::new();
        tokio::spawn(self.clone().forward_requests(
            caller.clone(),
            request.into_inner(),
            to_kine,
            to_gateway.clone(),
            end.clone(),
        ));
        tokio::spawn(
            self.clone()
                .relay_answers(caller, place, from_gateway, to_gateway, end),
        );
        Ok(Response::new(ReceiverStream::new(answers)))
    }
}

/// Where a watch call's answers go.
type Answers = mpsc::Sender<Result<WatchResponse, Status>>;

impl Front {
    /// Gateway → kine: every request of the call, each create checked
    /// first. A create outside the gateway's key spaces, or one sent once
    /// its credential no longer admits it, ends the call refused, and
    /// nothing of it reaches kine; so does one past
    /// [`MAX_CREATES_PER_CALL`], `ResourceExhausted`. Returns when the
    /// gateway's side of the call ends, which closes kine's side too, or
    /// when the call does.
    async fn forward_requests(
        self,
        caller: Caller,
        mut requests: Streaming<WatchRequest>,
        to_kine: mpsc::Sender<WatchRequest>,
        to_gateway: Answers,
        end: CallEnd,
    ) {
        let mut creates = 0usize;
        loop {
            let request = tokio::select! {
                request = requests.message() => request,
                () = end.ended() => return,
            };
            let Ok(Some(request)) = request else {
                return;
            };
            if let Some(RequestUnion::CreateRequest(create)) = &request.request_union {
                let refused = if self.still_admitted(&caller) {
                    self.check(&caller, &create.key, &create.range_end)
                } else {
                    tracing::info!(dp_id = %caller.dp_id,
                        "refused an etcd watch from a credential no longer admitted");
                    Some(caller.refuse())
                };
                if let Some(status) = refused {
                    end.finish(&to_gateway, status).await;
                    return;
                }
                creates += 1;
                if creates > MAX_CREATES_PER_CALL {
                    tracing::warn!(dp_id = %caller.dp_id,
                        "refused an etcd watch: a second create on one watch call");
                    let status = Status::resource_exhausted("etcdserver: one watch per call");
                    end.finish(&to_gateway, status).await;
                    return;
                }
            }
            tokio::select! {
                sent = to_kine.send(request) => if sent.is_err() {
                    return;
                },
                () = end.ended() => return,
            }
        }
    }

    /// Kine → gateway: every answer of kine's, as it is but for events
    /// outside the key spaces ([`Front::within`]), until kine ends the
    /// call, the gateway goes, the other task ends the call, or the call
    /// must end ([`Guard::must_end`]). Holds the call's `place` until then.
    async fn relay_answers(
        self,
        caller: Caller,
        _place: WatchPlace,
        from_gateway: mpsc::Receiver<WatchRequest>,
        to_gateway: Answers,
        end: CallEnd,
    ) {
        let mut guard = Guard::new(&self, caller.clone());
        let mut kine = WatchClient::new(self.endpoint.connect_lazy())
            .max_decoding_message_size(MAX_WATCH_ANSWER);
        let opened = tokio::select! {
            opened = kine.watch(ReceiverStream::new(from_gateway)) => opened,
            () = end.ended() => return,
            () = to_gateway.closed() => return,
            status = guard.must_end() => {
                end.finish(&to_gateway, status).await;
                return;
            }
        };
        let mut answers = match opened {
            Ok(answers) => answers.into_inner(),
            Err(status) => {
                end.finish(&to_gateway, status).await;
                return;
            }
        };
        loop {
            let answer = tokio::select! {
                answer = answers.message() => answer,
                () = end.ended() => return,
                () = to_gateway.closed() => return,
                status = guard.must_end() => {
                    end.finish(&to_gateway, status).await;
                    return;
                }
            };
            let answer = match answer {
                Ok(Some(answer)) => match self.within(answer) {
                    Some(answer) => answer,
                    None => continue,
                },
                // kine ended the call, as it does once the gateway's side
                // has.
                Ok(None) => {
                    end.stop();
                    return;
                }
                Err(status) => {
                    let status = too_long(&caller, status, MAX_WATCH_ANSWER, WATCH_TOO_LONG);
                    end.finish(&to_gateway, status).await;
                    return;
                }
            };
            tokio::select! {
                sent = to_gateway.send(Ok(answer)) => if sent.is_err() {
                    return;
                },
                () = end.ended() => return,
                status = guard.must_end() => {
                    end.finish(&to_gateway, status).await;
                    return;
                }
            }
        }
    }
}

impl Front {
    /// `answer` without its events outside the key spaces, by `kv` and by
    /// `prev_kv`; `None` when that leaves nothing the gateway reads: an
    /// answer that carried only such events, and no create, cancel or
    /// compaction. Answers without events (progress notices among them)
    /// pass as they are.
    fn within(&self, mut answer: WatchResponse) -> Option<WatchResponse> {
        let read = answer.events.len();
        answer.events.retain(|event| {
            event
                .kv
                .as_ref()
                .is_some_and(|kv| self.allowed.contains(&kv.key))
                && event
                    .prev_kv
                    .as_ref()
                    .is_none_or(|kv| self.allowed.contains(&kv.key))
        });
        if answer.events.len() == read {
            return Some(answer);
        }
        self.leaks.note(read - answer.events.len());
        let control = answer.created || answer.canceled || answer.compact_revision != 0;
        (control || !answer.events.is_empty()).then_some(answer)
    }
}

/// What the gateway is told of a range or a watch that failed: kine's
/// answer as it is, unless it was longer than `limit`, which is
/// `ResourceExhausted` with `message`.
fn too_long(caller: &Caller, status: Status, limit: usize, message: &'static str) -> Status {
    if status.code() == Code::OutOfRange && status.message().starts_with(TOO_LONG) {
        tracing::warn!(dp_id = %caller.dp_id, limit,
            "refused an etcd answer longer than the front reads");
        return Status::resource_exhausted(message);
    }
    status
}

/// The error about dropped keys, at most once per [`LEAK_LOG_EVERY`] with
/// the count since the last: a kine whose collation is not bytewise drops
/// keys on every answer, and would otherwise log once for each.
#[derive(Default)]
struct LeakLog(Mutex<(u64, Option<std::time::Instant>)>);

impl LeakLog {
    fn note(&self, dropped: usize) {
        if let Some(dropped) = self.due(dropped, std::time::Instant::now()) {
            tracing::error!(dropped, "{COLLATION_LEAK}");
        }
    }

    /// Adds `dropped` to the count since the last error, and takes the
    /// count when one is due at `now`.
    fn due(&self, dropped: usize, now: std::time::Instant) -> Option<u64> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.0 += dropped as u64;
        if held
            .1
            .is_some_and(|at| now.duration_since(at) < LEAK_LOG_EVERY)
        {
            return None;
        }
        let total = held.0;
        *held = (0, Some(now));
        Some(total)
    }
}

/// One watch call's place under [`MAX_WATCHES`] and
/// [`MAX_WATCHES_PER_CREDENTIAL`], given back when the call ends.
struct WatchPlace {
    _all: OwnedSemaphorePermit,
    _credential: OwnedSemaphorePermit,
}

/// The end of one watch call, shared by its two tasks: whichever ends the
/// call tells the other to stop. The gateway's answers end once both have.
#[derive(Clone)]
struct CallEnd(Arc<watch::Sender<bool>>);

impl CallEnd {
    fn new() -> CallEnd {
        CallEnd(Arc::new(watch::channel(false).0))
    }

    fn stop(&self) {
        self.0.send_replace(true);
    }

    /// Ends the call with `status`, after the answers already on their way.
    async fn finish(&self, to_gateway: &Answers, status: Status) {
        self.stop();
        let _ = to_gateway.send(Err(status)).await;
    }

    async fn ended(&self) {
        let mut ended = self.0.subscribe();
        let _ = ended.wait_for(|ended| *ended).await;
    }
}

/// Holds one watch call's credential to account while the call lasts.
struct Guard {
    front: Front,
    caller: Caller,
    changes: watch::Receiver<u64>,
    /// Whether `changes` can still announce anything.
    following: bool,
    every: Interval,
}

impl Guard {
    fn new(front: &Front, caller: Caller) -> Guard {
        let mut every = tokio::time::interval_at(Instant::now() + RECHECK_EVERY, RECHECK_EVERY);
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut changes = front.creds.changes();
        // Checked once at once: a change between the call's admission and
        // this subscription is not announced to it.
        changes.mark_changed();
        Guard {
            changes,
            front: front.clone(),
            caller,
            following: true,
            every,
        }
    }

    /// Resolves once the call must end: when its credential no longer
    /// admits the gateway, checked at each change to the credential list
    /// and every [`RECHECK_EVERY`], with the refusal (the connection is
    /// closed); or when its connection is closing for another reason, with
    /// `Unavailable`, which the gateway answers by connecting again.
    async fn must_end(&mut self) -> Status {
        loop {
            let closing = tokio::select! {
                changed = self.changes.changed(), if self.following => {
                    self.following = changed.is_ok();
                    false
                }
                _ = self.every.tick() => false,
                () = dp::closing(self.caller.close.as_ref()) => true,
            };
            if !self.front.still_admitted(&self.caller) {
                tracing::info!(dp_id = %self.caller.dp_id,
                    "ended the etcd watch of a credential no longer admitted");
                return self.caller.refuse();
            }
            if closing {
                return Status::unavailable("etcdserver: the connection is closing");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{LeakLog, LEAK_LOG_EVERY};

    #[test]
    fn dropped_keys_are_logged_at_most_once_a_minute_with_the_count_since() {
        let log = LeakLog::default();
        let start = Instant::now();
        assert_eq!(log.due(3, start), Some(3));
        assert_eq!(log.due(2, start + LEAK_LOG_EVERY / 2), None);
        assert_eq!(log.due(4, start + LEAK_LOG_EVERY / 2), None);
        assert_eq!(log.due(1, start + LEAK_LOG_EVERY), Some(7));
        assert_eq!(log.due(5, start + LEAK_LOG_EVERY * 3), Some(5));
    }
}
