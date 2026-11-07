//! The gateway listener (`CP_DP_ADDR`): one TLS port that requires a client
//! certificate of this control plane's CA and serves each connection by the
//! protocol it negotiates. `http/1.1`, or no ALPN at all, is the REST
//! surface under `/dp` (the gateway pins HTTP/1.1 for it,
//! `crates/aisix-server/src/heartbeat.rs`); `h2` is the etcd front, offered
//! only when there is one.
//!
//! An h2 connection can be ended by what it serves ([`ConnClose`]): the
//! etcd front closes the connection of a credential it refuses, as the
//! REST surface answers it `Connection: close`. A closing h2 connection
//! sends GOAWAY, and is dropped after its drain deadline if its streams
//! have not finished by then, since a watch never finishes on its own.
//!
//! The server certificate is minted at boot for `CP_DP_HOSTS`, the names
//! gateways reach this listener by.
//!
//! # What a peer can hold
//!
//! The port is open to the network, so what a peer that never authenticates
//! can hold is bounded ([`ConnLimits`]):
//! - at most [`MAX_PENDING_HANDSHAKES`] connections in their TLS handshake,
//!   each for at most [`HANDSHAKE_TIMEOUT`]; a slot frees as soon as its
//!   handshake ends, whichever way;
//! - at most [`MAX_CONNECTIONS`] connections in all, handshakes included,
//!   since a revoked certificate still completes its handshake.
//!
//! A credential that is revoked, expired, unlisted or of another
//! environment is found right after its handshake, by the rule every
//! request is checked by again ([`super::admitted`]). Its connection is
//! then given one answer, so the gateway learns it is refused (`403` on
//! HTTP/1.1, `PermissionDenied` and GOAWAY on h2), and closed, within
//! [`HANDSHAKE_TIMEOUT`]: it holds a slot no longer than a peer in its
//! handshake does.
//!
//! Accepted sockets have TCP keepalive, so a peer that vanished without
//! closing is noticed even while nothing is being sent. On shutdown each
//! connection finishes what is in flight, and is dropped after its drain
//! deadline ([`DRAIN_DEADLINE`] by default) if it has not by then.
//!
//! A connection over either cap is closed at once, unread. The process needs
//! a file descriptor limit above `MAX_CONNECTIONS`, plus the etcd front's
//! connections to kine (at most `MAX_WATCHES`), plus what the browser
//! listener and the database pools use (`ulimit -n` of 8192 or more).
//!
//! These caps are per process and say nothing of who is connecting: one
//! source can still fill them and lock gateways out. Per-source limiting
//! (connection rate and count per address) belongs in front of the
//! listener, in the load balancer or firewall that exposes `CP_DP_ADDR`.

use std::convert::Infallible;
use std::future::Future;
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use hyper::body::Incoming;
use hyper::server::conn::{http1, http2};
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::server::{VerifierBuilderError, WebPkiClientVerifier};
use rustls::{RootCertStore, ServerConfig};
use rustls_pki_types::CertificateDer;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tower::util::Oneshot;
use tower::ServiceExt;

use super::{admitted, closing, ConnClose, ConnIdentity, CredentialCache};
use crate::etcd_front::service::MAX_WATCHES;
use crate::pki::{self, Pki, PkiError};
use crate::settings::EnvSettings;
use crate::stopped;

/// What `h2` connections are served by: the etcd front's gRPC services.
pub type GrpcRoutes = tonic::service::Routes;

const HTTP_1_1: &[u8] = b"http/1.1";
const H2: &[u8] = b"h2";

/// Connections in their TLS handshake at once.
pub const MAX_PENDING_HANDSHAKES: usize = 512;

/// Connections at once, in their handshake or past it.
pub const MAX_CONNECTIONS: usize = 4096;

/// A peer that has not finished its TLS handshake by then is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a closing connection may take to finish what it has in flight
/// before it is dropped: an h2 connection its streams, which a watch never
/// does on its own; an HTTP/1.1 one its request, whose body may have
/// stopped arriving. Both REST endpoints are safe to send again, so a
/// request cut off by a shutdown is re-sent by the gateway.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// How long an accepted socket may stay idle before TCP keepalive probes
/// it, and how often it probes then.
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Streams one h2 connection may hold at once. A gateway's connection
/// carries a read and a watch; each watch holds a connection to kine of its
/// own.
const MAX_STREAMS_PER_CONNECTION: u32 = 16;

/// How often an idle h2 connection is pinged, and how long a ping may go
/// unanswered before the connection is dropped: a gateway that vanished
/// without closing would otherwise hold its watch, and its connection to
/// kine, until TCP gave up.
const H2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Open files spared beyond the connection cap: the browser listener, the
/// database pools and the process's own files.
const FD_HEADROOM: u64 = 512;

/// At most one warning this often while connections are refused at
/// capacity, so a flood does not flood the log too.
const AT_CAPACITY_WARNING_EVERY: Duration = Duration::from_secs(10);

/// How long to wait after a failed accept (such as running out of file
/// descriptors) before accepting again, rather than spinning.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_secs(1);

/// What connections may hold (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnLimits {
    pub pending_handshakes: usize,
    pub connections: usize,
    pub handshake_timeout: Duration,
    /// How long a connection may take to close once shutting down.
    pub drain_deadline: Duration,
}

impl ConnLimits {
    /// The open-file limit these caps need: every connection, handshakes
    /// included, the etcd front's connections to kine (one per watch), and
    /// the rest of the process.
    pub fn fds_needed(&self) -> u64 {
        self.connections as u64 + MAX_WATCHES as u64 + FD_HEADROOM
    }
}

/// Raises the soft open-file limit (`RLIMIT_NOFILE`) to the hard one, and
/// logs an error when even that is below what `limits` needs: past it,
/// accepting fails, and so do the browser listener and the database pools.
/// The boot goes on either way.
#[cfg(unix)]
pub fn raise_fd_limit(limits: &ConnLimits) {
    use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};

    let limit = getrlimit(Resource::Nofile);
    let raised = Rlimit {
        current: limit.maximum,
        maximum: limit.maximum,
    };
    let current = match setrlimit(Resource::Nofile, raised) {
        Ok(()) => limit.maximum,
        Err(e) => {
            tracing::warn!(error = %e, "cannot raise the open-file limit (RLIMIT_NOFILE)");
            limit.current
        }
    };
    let needed = limits.fds_needed();
    // `None` is no limit at all.
    if let Some(current) = current.filter(|current| *current < needed) {
        tracing::error!(
            limit = current,
            needed,
            "the open-file limit (RLIMIT_NOFILE) is below what the gateway listener's \
             connection caps need: raise it (ulimit -n, or LimitNOFILE for a service)"
        );
    }
}

/// Elsewhere there is no such limit to raise.
#[cfg(not(unix))]
pub fn raise_fd_limit(_limits: &ConnLimits) {}

impl Default for ConnLimits {
    fn default() -> ConnLimits {
        ConnLimits {
            pending_handshakes: MAX_PENDING_HANDSHAKES,
            connections: MAX_CONNECTIONS,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            drain_deadline: DRAIN_DEADLINE,
        }
    }
}

/// Whether the gateway listener runs, as `/admin/environment` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerState {
    Serving,
    /// Not started: the CA could sign no server certificate at boot.
    CaExpired,
}

impl ListenerState {
    pub fn as_str(self) -> &'static str {
        match self {
            ListenerState::Serving => "serving",
            ListenerState::CaExpired => "stopped: ca_expired",
        }
    }
}

/// Why the listener's TLS configuration could not be built.
#[derive(Debug, thiserror::Error)]
pub enum TlsSetupError {
    #[error(transparent)]
    Pki(#[from] PkiError),
    #[error("cannot verify gateway certificates: {0}")]
    Verifier(#[from] VerifierBuilderError),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
}

/// The listener's TLS configuration: a server certificate of `pki` for
/// `hosts`, a client certificate of `pki`'s CA required, and ALPN
/// `http/1.1`, with `h2` ahead of it when `h2` is set.
pub fn server_tls(
    pki: &Pki,
    hosts: &[String],
    h2: bool,
) -> Result<Arc<ServerConfig>, TlsSetupError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(pki.ca_der().to_vec()))?;
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()?;
    let (chain, key) = pki.server_cert(hosts)?;
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)?;
    config.alpn_protocols = if h2 {
        vec![H2.to_vec(), HTTP_1_1.to_vec()]
    } else {
        vec![HTTP_1_1.to_vec()]
    };
    Ok(config.into())
}

/// Who may hold a connection past its handshake: a certificate of a
/// credential of `env` that `creds` admits ([`super::admitted`]).
#[derive(Clone)]
pub struct Admission {
    pub env: EnvSettings,
    pub creds: CredentialCache,
}

/// Serves `listener` until `shutdown` turns true (or its sender is gone):
/// then it stops accepting, lets each connection finish the requests it has
/// in flight for up to `limits.drain_deadline`, and returns once all have
/// closed. Every request carries its connection's [`ConnIdentity`] as an
/// extension. A connection over `limits` is closed as soon as it is
/// accepted, and one whose credential `admission` refuses after its first
/// answer, or the handshake timeout, whichever comes first. Aborting the
/// task drops every connection at once.
pub async fn serve_dp(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    rest: Router,
    grpc: Option<GrpcRoutes>,
    admission: Admission,
    limits: ConnLimits,
    shutdown: watch::Receiver<bool>,
) {
    let serving = Serving {
        acceptor: TlsAcceptor::from(tls),
        rest,
        grpc,
        admission,
        handshake_timeout: limits.handshake_timeout,
        drain_deadline: limits.drain_deadline,
    };
    let handshake_slots = Arc::new(Semaphore::new(limits.pending_handshakes));
    let connection_slots = Arc::new(Semaphore::new(limits.connections));
    let mut connections = JoinSet::new();
    let mut stop = shutdown.clone();
    let mut refused = 0u64;
    let mut warned: Option<Instant> = None;
    loop {
        tokio::select! {
            () = stopped(&mut stop) => break,
            // Reaps connections that have ended.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((tcp, _)) => {
                    let permits = (
                        handshake_slots.clone().try_acquire_owned(),
                        connection_slots.clone().try_acquire_owned(),
                    );
                    let (Ok(handshake), Ok(connection)) = permits else {
                        // Holding it unread would be what the cap is there
                        // to prevent.
                        drop(tcp);
                        refused += 1;
                        if warned.is_none_or(|at| at.elapsed() >= AT_CAPACITY_WARNING_EVERY) {
                            tracing::warn!(refused, "the gateway listener is at capacity and \
                                closes new connections; see its connection caps");
                            warned = Some(Instant::now());
                            refused = 0;
                        }
                        continue;
                    };
                    if let Err(e) = keep_alive(&tcp) {
                        tracing::debug!(error = %e, "cannot set TCP keepalive on a gateway connection");
                    }
                    connections.spawn(serve_connection(
                        tcp,
                        handshake,
                        connection,
                        serving.clone(),
                        shutdown.clone(),
                    ));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "the gateway listener cannot accept a connection");
                    tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                }
            },
        }
    }
    drop(listener);
    while connections.join_next().await.is_some() {}
}

/// Turns on TCP keepalive for an accepted socket: without it a peer that
/// vanished mid-request, with nothing left to send to it, is never noticed.
pub fn keep_alive(tcp: &TcpStream) -> std::io::Result<()> {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_IDLE)
        .with_interval(TCP_KEEPALIVE_INTERVAL);
    socket2::SockRef::from(tcp).set_tcp_keepalive(&keepalive)
}

/// What every connection of one listener is served with.
#[derive(Clone)]
struct Serving {
    acceptor: TlsAcceptor,
    rest: Router,
    grpc: Option<GrpcRoutes>,
    admission: Admission,
    handshake_timeout: Duration,
    drain_deadline: Duration,
}

/// `handshake` is held for the handshake only; `_connection` for the
/// whole connection.
async fn serve_connection(
    tcp: TcpStream,
    handshake: OwnedSemaphorePermit,
    _connection: OwnedSemaphorePermit,
    serving: Serving,
    shutdown: watch::Receiver<bool>,
) {
    let Serving {
        acceptor,
        rest,
        grpc,
        admission,
        handshake_timeout,
        drain_deadline,
    } = serving;
    let handshook = tokio::time::timeout(handshake_timeout, acceptor.accept(tcp)).await;
    drop(handshake);
    let tls = match handshook {
        Ok(Ok(tls)) => tls,
        // Scanners and gateways of other control planes end up here: worth
        // a debug line, not a warning.
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "a gateway TLS handshake failed");
            return;
        }
        Err(_) => {
            tracing::debug!("a gateway TLS handshake timed out");
            return;
        }
    };
    let (_, session) = tls.get_ref();
    // The verifier has required a certificate of this CA.
    let Some(leaf) = session.peer_certificates().and_then(|chain| chain.first()) else {
        return;
    };
    let identity = match pki::identity_of(leaf) {
        Ok(id) => ConnIdentity {
            env_id: id.env_id,
            dp_id: id.dp_id,
            fingerprint: id.fingerprint_sha256,
        },
        Err(e) => {
            tracing::warn!(error = %e, "refused a client certificate of this CA that names no gateway");
            return;
        }
    };
    // Before anything is served. A refused credential (revoked, expired,
    // unlisted, of another environment) is still told why, by the answer
    // its first request gets from the endpoints' own check: `403` with
    // `Connection: close` on HTTP/1.1, `PermissionDenied` and then GOAWAY
    // on h2. Without it a refused gateway could not tell a refusal from an
    // outage. Its connection is served for at most one request, and for no
    // longer than a handshake may take, so by sending nothing it holds its
    // slot no longer than a peer in its handshake does. Each request of an
    // admitted connection is checked again, which catches a revocation
    // while the connection is open. A credential that cannot be checked
    // now (the database is out) is served: each request answers the outage.
    let refused = match admitted(&identity, &admission.env, &admission.creds).await {
        Ok(found) => found.is_none(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot check a gateway credential at connect");
            false
        }
    };
    let negotiated_h2 = session.alpn_protocol() == Some(H2);
    let io = TokioIo::new(tls);
    let served = async {
        if !negotiated_h2 {
            let conn = http1::Builder::new()
                .timer(TokioTimer::new())
                .keep_alive(!refused)
                .serve_connection(io, WithIdentity::new(rest, identity, None));
            drive(
                conn,
                |conn| conn.graceful_shutdown(),
                shutdown,
                None,
                drain_deadline,
            )
            .await;
        } else if let Some(grpc) = grpc {
            let close = ConnClose::new();
            let conn = http2::Builder::new(TokioExecutor::new())
                .timer(TokioTimer::new())
                .max_concurrent_streams(MAX_STREAMS_PER_CONNECTION)
                .keep_alive_interval(H2_KEEPALIVE_INTERVAL)
                .keep_alive_timeout(H2_KEEPALIVE_TIMEOUT)
                .serve_connection(io, WithIdentity::new(grpc, identity, Some(close.clone())));
            drive(
                conn,
                |conn| conn.graceful_shutdown(),
                shutdown,
                Some(close),
                drain_deadline,
            )
            .await;
        }
    };
    if refused {
        if tokio::time::timeout(handshake_timeout, served)
            .await
            .is_err()
        {
            tracing::debug!("dropped a refused gateway connection that sent no request in time");
        }
    } else {
        served.await;
    }
    // Otherwise h2 was negotiated though not offered: nothing serves it.
}

/// Runs `conn` to its end, calling `graceful` on it once `shutdown` turns
/// true (or its sender is gone), so it finishes what is in flight and then
/// closes. With `close`, also once `close` is closed; shutting down closes
/// it, which ends the watches it serves. A connection still open
/// `drain_deadline` after `graceful` is dropped, so a shutdown ends.
async fn drive<C>(
    conn: C,
    graceful: impl FnOnce(Pin<&mut C>),
    mut shutdown: watch::Receiver<bool>,
    close: Option<ConnClose>,
    drain_deadline: Duration,
) where
    C: Future<Output = hyper::Result<()>>,
{
    let mut conn = pin!(conn);
    let mut graceful = Some(graceful);
    let mut deadline: Option<Pin<Box<tokio::time::Sleep>>> = None;
    loop {
        tokio::select! {
            served = conn.as_mut() => {
                if let Err(e) = served {
                    tracing::debug!(error = %e, "a gateway connection ended with an error");
                }
                return;
            }
            () = stopped(&mut shutdown), if graceful.is_some() => {
                if let Some(close) = &close {
                    close.close();
                }
            }
            () = closing(close.as_ref()), if graceful.is_some() => {}
            () = expired(&mut deadline) => {
                tracing::debug!("dropped a closing gateway connection whose streams did not finish");
                return;
            }
        }
        if let Some(graceful) = graceful.take() {
            graceful(conn.as_mut());
            deadline = Some(Box::pin(tokio::time::sleep(drain_deadline)));
        }
    }
}

/// Resolves once `deadline` passes; never without one.
async fn expired(deadline: &mut Option<Pin<Box<tokio::time::Sleep>>>) {
    match deadline {
        Some(deadline) => deadline.as_mut().await,
        None => std::future::pending().await,
    }
}

/// `service`, with the connection's identity attached to every request,
/// and on h2 connections the handle that closes the connection.
#[derive(Clone)]
struct WithIdentity<S> {
    service: S,
    identity: ConnIdentity,
    close: Option<ConnClose>,
}

impl<S> WithIdentity<S> {
    fn new(service: S, identity: ConnIdentity, close: Option<ConnClose>) -> WithIdentity<S> {
        WithIdentity {
            service,
            identity,
            close,
        }
    }

    fn attach<B>(&self, request: &mut Request<B>) {
        request.extensions_mut().insert(self.identity.clone());
        if let Some(close) = &self.close {
            request.extensions_mut().insert(close.clone());
        }
    }
}

impl hyper::service::Service<Request<Incoming>> for WithIdentity<Router> {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = Oneshot<Router, Request<Incoming>>;

    fn call(&self, mut request: Request<Incoming>) -> Self::Future {
        self.attach(&mut request);
        self.service.clone().oneshot(request)
    }
}

impl hyper::service::Service<Request<Incoming>> for WithIdentity<GrpcRoutes> {
    type Response = hyper::Response<tonic::body::BoxBody>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Oneshot<GrpcRoutes, Request<tonic::body::BoxBody>>;

    fn call(&self, mut request: Request<Incoming>) -> Self::Future {
        self.attach(&mut request);
        self.service
            .clone()
            .oneshot(request.map(tonic::body::boxed))
    }
}
