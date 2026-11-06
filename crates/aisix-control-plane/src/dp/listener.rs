//! The gateway listener (`CP_DP_ADDR`): one TLS port that requires a client
//! certificate of this control plane's CA and serves each connection by the
//! protocol it negotiates. `http/1.1`, or no ALPN at all, is the REST
//! surface under `/dp` (the gateway pins HTTP/1.1 for it,
//! `crates/aisix-server/src/heartbeat.rs`); `h2` is the etcd front, offered
//! only when there is one.
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
//!   since a revoked certificate still completes its handshake and is then
//!   refused per request.
//!
//! A connection over either cap is closed at once, unread. The process needs
//! a file descriptor limit above `MAX_CONNECTIONS` plus what the browser
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

use super::ConnIdentity;
use crate::pki::{self, Pki, PkiError};
use crate::stopped;

/// What `h2` connections are served by: the etcd front, once it exists.
pub type GrpcRoutes = Router;

const HTTP_1_1: &[u8] = b"http/1.1";
const H2: &[u8] = b"h2";

/// Connections in their TLS handshake at once.
pub const MAX_PENDING_HANDSHAKES: usize = 512;

/// Connections at once, in their handshake or past it.
pub const MAX_CONNECTIONS: usize = 4096;

/// A peer that has not finished its TLS handshake by then is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

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
}

impl ConnLimits {
    /// The open-file limit these caps need: every connection, handshakes
    /// included, and the rest of the process.
    pub fn fds_needed(&self) -> u64 {
        self.connections as u64 + FD_HEADROOM
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

/// Serves `listener` until `shutdown` turns true (or its sender is gone):
/// then it stops accepting, lets each connection finish the requests it has
/// in flight, and returns once all have closed. Every request carries its
/// connection's [`ConnIdentity`] as an extension. A connection over
/// `limits` is closed as soon as it is accepted. Aborting the task drops
/// every connection at once.
pub async fn serve_dp(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    rest: Router,
    grpc: Option<GrpcRoutes>,
    limits: ConnLimits,
    shutdown: watch::Receiver<bool>,
) {
    let serving = Serving {
        acceptor: TlsAcceptor::from(tls),
        rest,
        grpc,
        handshake_timeout: limits.handshake_timeout,
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

/// What every connection of one listener is served with.
#[derive(Clone)]
struct Serving {
    acceptor: TlsAcceptor,
    rest: Router,
    grpc: Option<GrpcRoutes>,
    handshake_timeout: Duration,
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
        handshake_timeout,
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
    let negotiated_h2 = session.alpn_protocol() == Some(H2);
    let io = TokioIo::new(tls);
    if !negotiated_h2 {
        let conn = http1::Builder::new()
            .timer(TokioTimer::new())
            .serve_connection(io, WithIdentity::new(rest, identity));
        drive(conn, |conn| conn.graceful_shutdown(), shutdown).await;
    } else if let Some(grpc) = grpc {
        let conn = http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .serve_connection(io, WithIdentity::new(grpc, identity));
        drive(conn, |conn| conn.graceful_shutdown(), shutdown).await;
    }
    // Otherwise h2 was negotiated though not offered: nothing serves it.
}

/// Runs `conn` to its end, calling `graceful` on it once `shutdown` turns
/// true (or its sender is gone), so it finishes what is in flight and then
/// closes.
async fn drive<C>(conn: C, graceful: impl FnOnce(Pin<&mut C>), mut shutdown: watch::Receiver<bool>)
where
    C: Future<Output = hyper::Result<()>>,
{
    let mut conn = pin!(conn);
    let mut graceful = Some(graceful);
    loop {
        tokio::select! {
            served = conn.as_mut() => {
                if let Err(e) = served {
                    tracing::debug!(error = %e, "a gateway connection ended with an error");
                }
                return;
            }
            () = stopped(&mut shutdown), if graceful.is_some() => {
                if let Some(graceful) = graceful.take() {
                    graceful(conn.as_mut());
                }
            }
        }
    }
}

/// `router`, with the connection's identity attached to every request.
#[derive(Clone)]
struct WithIdentity {
    router: Router,
    identity: ConnIdentity,
}

impl WithIdentity {
    fn new(router: Router, identity: ConnIdentity) -> WithIdentity {
        WithIdentity { router, identity }
    }
}

impl hyper::service::Service<Request<Incoming>> for WithIdentity {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = Oneshot<Router, Request<Incoming>>;

    fn call(&self, mut request: Request<Incoming>) -> Self::Future {
        request.extensions_mut().insert(self.identity.clone());
        self.router.clone().oneshot(request)
    }
}
