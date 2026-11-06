//! The control plane for managed AISIX gateways.
//!
//! Postgres holds all of its state. One deployment serves one environment
//! (`env_id`), pinned by the first boot. The browser listener serves
//! operators and tenants; later tasks add the gateway listener and the kine
//! projection.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::task::JoinHandle;

pub mod audit;
pub mod auth;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod http;
pub mod operators;
pub mod settings;

use auth::password::{LoginThrottle, PasswordGate, ARGON2_GATE_PERMITS};
use config::CpConfig;
use db::Db;
use error::BootError;
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
}

/// A booted control plane. Dropping it stops its listener.
pub struct Running {
    pub state: AppState,
    /// Where the browser listener is bound (with the port resolved when
    /// `CP_HTTP_ADDR` names port 0).
    pub http_addr: SocketAddr,
    serve: Option<JoinHandle<std::io::Result<()>>>,
}

impl Running {
    /// Waits until the listener stops: after `shutdown` resolves and the
    /// requests in flight have finished, or on an error.
    pub async fn wait(mut self) -> Result<(), BootError> {
        let Some(serve) = self.serve.take() else {
            return Ok(());
        };
        match serve.await {
            Ok(result) => result.map_err(BootError::Serve),
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => Err(BootError::Serve(std::io::Error::other(e))),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(serve) = &self.serve {
            serve.abort();
        }
    }
}

/// Boots the control plane: opens and migrates the database, pins the
/// environment, and starts the browser listener, which stops gracefully once
/// `shutdown` resolves.
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
    let passwords = PasswordGate::new(ARGON2_GATE_PERMITS)?;
    let listener = tokio::net::TcpListener::bind(cfg.http_addr)
        .await
        .map_err(BootError::Listen)?;
    let http_addr = listener.local_addr().map_err(BootError::Listen)?;
    let state = AppState {
        db,
        env,
        cfg: Arc::new(cfg),
        passwords,
        operator_logins: LoginThrottle::default(),
    };
    let router = http::browser_router(state.clone());
    let serve = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
    });
    tracing::info!(%http_addr, env_id = %state.env.env_id, "control plane listening");
    Ok(Running {
        state,
        http_addr,
        serve: Some(serve),
    })
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
