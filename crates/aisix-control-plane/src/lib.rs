//! The control plane for managed AISIX gateways.
//!
//! Postgres holds all of its state. One deployment serves one environment
//! (`env_id`), pinned by the first boot. The browser listener serves
//! operators and tenants, the gateway listener (mTLS) the gateways, and a
//! projector writes the gateway resources to kine.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;

pub mod audit;
pub mod auth;
pub mod config;
pub mod crypto;
pub mod db;
pub mod dp;
pub mod error;
pub mod gateways;
pub mod housekeeping;
pub mod http;
pub mod kine;
pub mod members;
pub mod operators;
pub mod pki;
pub mod projector;
pub mod resources;
pub mod rollout;
pub mod settings;
pub mod tenants;

use auth::password::{LoginThrottle, PasswordGate, ARGON2_GATE_PERMITS};
use config::CpConfig;
use db::Db;
use dp::heartbeat::BeatLimiter;
use dp::listener::{ConnLimits, ListenerState, TlsSetupError};
use dp::{CredentialCache, CREDENTIALS_POOL_SIZE};
use error::BootError;
use kine::{KineClient, KineWriter};
use pki::{Pki, PkiError};
use resources::write::WriteCtx;
use settings::EnvSettings;

/// What every request handler shares.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub env: EnvSettings,
    pub cfg: Arc<CpConfig>,
    /// The one gate in front of every argon2 run.
    pub passwords: PasswordGate,
    /// The login cooldown for operator emails.
    pub operator_logins: LoginThrottle,
    /// The login cooldown for member emails, and the signup window.
    pub portal: members::PortalLimits,
    /// The CA that issues gateway certificates, and the platform id key.
    pub pki: Arc<Pki>,
    /// The gateway credentials, which every gateway request is checked
    /// against.
    pub creds: CredentialCache,
    /// How many heartbeats each credential may still send.
    pub beats: BeatLimiter,
    /// Whether the gateway listener runs.
    pub dp_listener: ListenerState,
}

/// A booted control plane. Dropping it stops everything it runs.
pub struct Running {
    pub state: AppState,
    /// Where the browser listener is bound (with the port resolved when
    /// `CP_HTTP_ADDR` names port 0).
    pub http_addr: SocketAddr,
    /// Where the gateway listener is bound, likewise; `None` when it does
    /// not run because the CA can sign no server certificate (it has
    /// expired, or expires within a day).
    pub dp_addr: Option<SocketAddr>,
    serve: Option<JoinHandle<std::io::Result<()>>>,
    /// The gateway listener, housekeeping and the projector: they stop once
    /// told to.
    tasks: Vec<JoinHandle<()>>,
    /// The credential refresh, which holds nothing that needs a clean stop:
    /// aborted, never awaited.
    refresh: JoinHandle<()>,
    /// Passes `shutdown` on to everything above. Aborting it drops the
    /// sender, which they take as the same signal.
    signal: JoinHandle<()>,
}

impl Running {
    /// Waits until the control plane stops: after `shutdown` resolves and
    /// the requests in flight on both listeners have finished, or when the
    /// browser listener fails, which stops the rest too.
    pub async fn wait(mut self) -> Result<(), BootError> {
        let served = match self.serve.take() {
            None => Ok(()),
            Some(serve) => match serve.await {
                Ok(result) => result.map_err(BootError::Serve),
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => Err(BootError::Serve(std::io::Error::other(e))),
            },
        };
        self.signal.abort();
        for task in std::mem::take(&mut self.tasks) {
            if let Err(e) = task.await {
                if e.is_panic() {
                    std::panic::resume_unwind(e.into_panic());
                }
            }
        }
        served
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(serve) = &self.serve {
            serve.abort();
        }
        for task in &self.tasks {
            task.abort();
        }
        self.refresh.abort();
        self.signal.abort();
    }
}

/// Boots the control plane: opens and migrates the database, pins the
/// environment, loads the CA and the gateway credentials, and starts the
/// browser listener, the gateway listener, housekeeping and the projector,
/// which stop gracefully once `shutdown` resolves.
pub async fn start<S>(cfg: CpConfig, shutdown: S) -> Result<Running, BootError>
where
    S: Future<Output = ()> + Send + 'static,
{
    let db = Db::connect(
        &cfg.database_url,
        cfg.db_lock_timeout_ms,
        cfg.db_statement_timeout_ms,
    )
    .await?;
    let env = settings::ensure_env(&db, cfg.env_id_seed, &cfg.etcd_prefix).await?;
    // The id this database claims its environment's kine prefix under.
    projector::ensure_database_id(&db).await?;
    let pki = Arc::new(Pki::load_or_init(&db, &cfg.master_key, &env).await?);
    let write_ctx = resources::write::WriteCtx {
        mk: crypto::MasterKey(cfg.master_key.0),
        env: env.clone(),
    };
    tenants::ensure_internal_tenant(&db, &write_ctx).await?;
    let passwords = PasswordGate::new(ARGON2_GATE_PERMITS)?;
    let creds = CredentialCache::load(&db.dedicated(CREDENTIALS_POOL_SIZE)).await?;
    let dp_tls = match dp::listener::server_tls(&pki, &cfg.dp_hosts, false) {
        Ok(tls) => Some(tls),
        // Still a boot, so operators can see why: every gateway certificate
        // has run out with the CA anyway.
        Err(TlsSetupError::Pki(PkiError::CaExpiring)) => {
            tracing::error!(
                "the control-plane CA has expired or expires within a day: the gateway \
                 listener (CP_DP_ADDR) is not started until the CA is replaced"
            );
            None
        }
        Err(e) => return Err(e.into()),
    };
    // Connects on first use: a kine that is down at boot only delays the
    // projection.
    let kine: Arc<dyn KineWriter> = Arc::new(KineClient::connect(&cfg.kine_endpoint).await?);
    let listener = tokio::net::TcpListener::bind(cfg.http_addr)
        .await
        .map_err(BootError::Listen)?;
    let http_addr = listener.local_addr().map_err(BootError::Listen)?;
    let dp_limits = ConnLimits::default();
    let dp = match dp_tls {
        None => None,
        Some(tls) => {
            dp::listener::raise_fd_limit(&dp_limits);
            let listener = tokio::net::TcpListener::bind(cfg.dp_addr)
                .await
                .map_err(BootError::DpListen)?;
            let addr = listener.local_addr().map_err(BootError::DpListen)?;
            Some((listener, tls, addr))
        }
    };
    let dp_addr = dp.as_ref().map(|(_, _, addr)| *addr);
    let dp_listener = if dp.is_some() {
        ListenerState::Serving
    } else {
        ListenerState::CaExpired
    };
    let state = AppState {
        db,
        env,
        cfg: Arc::new(cfg),
        passwords,
        operator_logins: LoginThrottle::default(),
        portal: members::PortalLimits::default(),
        pki,
        creds,
        beats: BeatLimiter::default(),
        dp_listener,
    };

    // Nothing fails from here on, so nothing spawned below is left behind
    // by a refused boot.
    let (stop, stopping) = watch::channel(false);
    let signal = tokio::spawn(async move {
        shutdown.await;
        let _ = stop.send(true);
    });
    let router = http::browser_router(state.clone());
    let mut browser_stop = stopping.clone();
    let serve = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { stopped(&mut browser_stop).await })
            .await
    });
    let mut tasks = vec![housekeeping::spawn(state.db.clone(), stopping.clone())];
    let mut projector_stop = stopping.clone();
    tasks.push(projector::spawn(
        state.db.clone(),
        kine,
        WriteCtx::of(&state),
        async move { stopped(&mut projector_stop).await },
    ));
    if let Some((listener, tls, _)) = dp {
        tasks.push(tokio::spawn(dp::listener::serve_dp(
            listener,
            tls,
            http::dp_router(state.clone()),
            None,
            dp_limits,
            stopping,
        )));
    }
    let refresh = state.creds.spawn_refresh();
    tracing::info!(%http_addr, ?dp_addr, env_id = %state.env.env_id, "control plane listening");
    Ok(Running {
        state,
        http_addr,
        dp_addr,
        serve: Some(serve),
        tasks,
        refresh,
        signal,
    })
}

/// Resolves once `stop` turns true, or its sender is gone: the shutdown
/// signal every listener and task of a [`Running`] watches.
pub(crate) async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

/// Runs the control plane until SIGTERM or Ctrl-C.
pub async fn run(cfg: CpConfig) -> Result<(), BootError> {
    tracing::info!(config = ?cfg, "starting the control plane");
    start(cfg, shutdown_signal()).await?.wait().await
}

/// Resolves on SIGTERM or Ctrl-C. A signal whose handler cannot be installed
/// is not waited for, rather than taken as a request to stop.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}
