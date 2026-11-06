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
//! Nothing read from kine is logged: the documents carry plaintext
//! credentials.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};
use tonic::{Extensions, Request, Response, Status, Streaming};
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

/// Messages in flight each way on one watch call. Bounded, so a gateway
/// that stops reading stops the front reading kine, and kine's own
/// slow-reader cancel reaches the gateway as it would directly.
const WATCH_BUFFER: usize = 16;

/// How often the connection to kine is pinged while a stream is open (kine
/// allows no pings without one, and none more often than every 5 s).
const KINE_KEEPALIVE: Duration = Duration::from_secs(30);

/// What a gateway whose credential is refused is told.
const CREDENTIAL_REFUSED: &str = "etcdserver: permission denied: gateway credential refused";

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
        }
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
        let answer = KvClient::new(self.shared.clone())
            .max_decoding_message_size(usize::MAX)
            .range(range)
            .await?;
        Ok(Response::new(answer.into_inner()))
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
                .relay_answers(caller, from_gateway, to_gateway, end),
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
    /// nothing of it reaches kine. Returns when the gateway's side of the
    /// call ends, which closes kine's side too, or when the call does.
    async fn forward_requests(
        self,
        caller: Caller,
        mut requests: Streaming<WatchRequest>,
        to_kine: mpsc::Sender<WatchRequest>,
        to_gateway: Answers,
        end: CallEnd,
    ) {
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
            }
            tokio::select! {
                sent = to_kine.send(request) => if sent.is_err() {
                    return;
                },
                () = end.ended() => return,
            }
        }
    }

    /// Kine → gateway: every answer of kine's, as it is, until kine ends
    /// the call, the gateway goes, the other task ends the call, or the
    /// call must end ([`Guard::must_end`]).
    async fn relay_answers(
        self,
        caller: Caller,
        from_gateway: mpsc::Receiver<WatchRequest>,
        to_gateway: Answers,
        end: CallEnd,
    ) {
        let mut guard = Guard::new(&self, caller);
        let mut kine =
            WatchClient::new(self.endpoint.connect_lazy()).max_decoding_message_size(usize::MAX);
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
                Ok(Some(answer)) => answer,
                // kine ended the call, as it does once the gateway's side
                // has.
                Ok(None) => {
                    end.stop();
                    return;
                }
                Err(status) => {
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
        Guard {
            changes: front.creds.changes(),
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
