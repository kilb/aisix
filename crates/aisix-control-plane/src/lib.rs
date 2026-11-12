//! The control plane for managed AISIX gateways.
//!
//! Postgres holds all of its state. One deployment serves one environment
//! (`env_id`), pinned by the first boot. The browser listener serves
//! operators and tenants, the gateway listener (mTLS) the gateways, and a
//! projector writes the gateway resources to kine.

#[cfg(test)]
extern crate self as aisix_control_plane;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;

pub mod audit;
pub mod auth;
pub mod billing;
pub mod channels;
pub mod compat;
pub mod config;
pub mod crypto;
pub mod db;
pub mod dp;
pub mod error;
pub mod etcd_front;
pub mod gateways;
pub mod groups;
pub mod housekeeping;
pub mod http;
pub mod keys;
pub mod kine;
pub mod member_usage;
pub mod members;
pub mod model_probes;
pub mod operators;
pub mod pki;
pub mod pricing;
pub mod projector;
pub mod resources;
pub mod rollout;
pub mod settings;
pub mod tenants;
pub mod upstream_http;
pub mod upstream_models;
pub mod usage;
pub mod usage_metrics;

use auth::password::{LoginThrottle, PasswordGate, ARGON2_GATE_PERMITS};
use config::CpConfig;
use db::Db;
use dp::heartbeat::BeatLimiter;
use dp::listener::{ConnLimits, ListenerState, TlsSetupError};
use dp::{CredentialCache, CREDENTIALS_POOL_SIZE};
use error::BootError;
use kine::{KineClient, KineWriter};
use pki::{Pki, PkiError};
use pricing::store::PriceBookCache;
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
    /// The usage-batch bodies this process may hold at once
    /// ([`dp::telemetry::MAX_BODIES_IN_FLIGHT`]).
    pub telemetry_bodies: Arc<Semaphore>,
    /// The price book every stored usage event is priced from, reloaded when
    /// a price write moves `pricing_version`.
    pub prices: Arc<PriceBookCache>,
    /// The pool the gateway lookups (`/dp/v1/keys/*`) run on, of its own so
    /// they never take the connections usage intake needs
    /// ([`billing::access::LOOKUP_POOL_SIZE`]).
    pub lookups: Db,
    /// The one outbound client, and the model lists it has fetched
    /// ([`upstream_models::Cache`]).
    pub upstream: Arc<upstream_http::Client>,
    pub upstream_models: Arc<upstream_models::Cache>,
    /// 手动模型探测全局有界，不占用数据库连接等待上游。
    pub model_probes: Arc<Semaphore>,
    /// Where the price library is fetched from ([`pricing::library`]).
    pub library: Arc<pricing::library::Source>,
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
    /// The gateway listener, housekeeping, the projector and the billing
    /// settler: they stop once told to.
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

/// Boots the control plane: loads the built-in price catalogue, opens and
/// migrates the database, pins the environment, loads the CA and the gateway
/// credentials, and starts the browser listener, the gateway listener,
/// housekeeping, the projector and the billing settler, which stop
/// gracefully once `shutdown` resolves. A version stamp the gateway-version gate cannot read refuses
/// the boot first, and a catalogue that does not load next.
pub async fn start<S>(cfg: CpConfig, shutdown: S) -> Result<Running, BootError>
where
    S: Future<Output = ()> + Send + 'static,
{
    start_with_catalogue(cfg, shutdown, None).await
}

/// [`start`], pricing from `catalogue` instead of the built-in one when it is
/// given. Only a caller of this library can pass one (the tests, for a
/// catalogue with prices announced far ahead); no configuration or
/// environment variable reaches it.
pub async fn start_with_catalogue<S>(
    cfg: CpConfig,
    shutdown: S,
    catalogue: Option<Arc<aisix_pricing::Catalogue>>,
) -> Result<Running, BootError>
where
    S: Future<Output = ()> + Send + 'static,
{
    start_with_policy(
        cfg,
        shutdown,
        catalogue,
        &aisix_core::config::SsrfProtectionConfig::default(),
    )
    .await
}

/// [`start_with_catalogue`], with the outbound address policy the
/// upstream-model route applies.
///
/// Production takes [`aisix_core::config::SsrfProtectionConfig::default`] —
/// on, with no allowed hosts — and no configuration or environment variable
/// reaches this parameter. Only a caller of this library can pass another
/// policy, which is how a test reaches a fixture server on a loopback
/// address the default refuses.
pub async fn start_with_policy<S>(
    cfg: CpConfig,
    shutdown: S,
    catalogue: Option<Arc<aisix_pricing::Catalogue>>,
    outbound: &aisix_core::config::SsrfProtectionConfig,
) -> Result<Running, BootError>
where
    S: Future<Output = ()> + Send + 'static,
{
    let library = reqwest::Url::parse(pricing::library::LIBRARY_URL)
        .map_err(|e| BootError::LibrarySource(e.to_string()))?;
    start_with_sources(cfg, shutdown, catalogue, outbound, library).await
}

/// [`start_with_policy`], fetching the price library from `library` instead
/// of [`pricing::library::LIBRARY_URL`].
///
/// Production always takes the constant, and no configuration, environment
/// variable or request field reaches this parameter. Only a caller of this
/// library can pass another address, which is how a test points the fetch at
/// a stand-in, or at nothing, so that no test reaches the internet.
pub async fn start_with_sources<S>(
    cfg: CpConfig,
    shutdown: S,
    catalogue: Option<Arc<aisix_pricing::Catalogue>>,
    outbound: &aisix_core::config::SsrfProtectionConfig,
    library: reqwest::Url,
) -> Result<Running, BootError>
where
    S: Future<Output = ()> + Send + 'static,
{
    // Every TLS client that takes the process default (the kine client, a
    // gateway client in a test) gets ring, the provider this crate builds its
    // own configurations with. The Postgres driver picks ring itself. A
    // build that unifies the workspace's features also enables aws-lc-rs,
    // and rustls then refuses to pick a default. Already installed is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // A stamp the gate cannot read would leave it judging nothing, unseen.
    if compat::GatewayVersion::parse(&cfg.cp_version) == compat::GatewayVersion::Unrecognized {
        return Err(BootError::UnreadableVersion(cfg.cp_version));
    }
    // A catalogue this build cannot read would leave the built-in list
    // prices that baselines are taken from missing everywhere, unseen.
    let catalogue = match catalogue {
        Some(catalogue) => catalogue,
        None => aisix_pricing::Catalogue::builtin()?,
    };
    let db = Db::connect(
        &cfg.database_url,
        cfg.db_lock_timeout_ms,
        cfg.db_statement_timeout_ms,
    )
    .await?;
    // A tenant an older release created has no billing account yet. One
    // left behind does not stop the boot: housekeeping tries every tick.
    housekeeping::backfill_accounts(&db).await;
    let env = settings::ensure_env(&db, cfg.env_id_seed, &cfg.etcd_prefix).await?;
    // The id this database claims its environment's kine prefix under.
    projector::ensure_database_id(&db).await?;
    let pki = Arc::new(Pki::load_or_init(&db, &cfg.master_key, &env).await?);
    let write_ctx = resources::write::WriteCtx {
        mk: crypto::MasterKey(cfg.master_key.0),
        env: env.clone(),
        cp_version: cfg.cp_version.clone(),
        stale_secs: gateways::stale_window_secs(&cfg),
    };
    tenants::ensure_internal_tenant(&db, &write_ctx).await?;
    // Grants of a channel's generated direct model an older release stored.
    // A tenant left behind does not stop the boot: housekeeping tries every
    // tick.
    housekeeping::replace_generated_grants(&db, &write_ctx).await;
    // Keys an older release wrote. A tenant left behind, whatever the reason,
    // is tried again by every housekeeping tick, so it does not stop the
    // boot: refusing to start would only stop the control plane serving
    // everything else.
    let caught_up = keys::catch_up(&db, &write_ctx, keys::KEY_DOC_VERSION)
        .await
        .map_err(|e| BootError::KeyDocuments(format!("{}: {}", e.code, e.message)))?;
    if !caught_up.failed.is_empty() {
        tracing::error!(
            tenants = ?caught_up.failed,
            "the key documents of these tenants could not be rewritten at boot (each \
             reason is logged above); housekeeping tries them again every tick"
        );
    }
    let passwords = PasswordGate::new(ARGON2_GATE_PERMITS)?;
    let creds = CredentialCache::load(&db.dedicated(CREDENTIALS_POOL_SIZE)).await?;
    let dp_tls = match dp::listener::server_tls(&pki, &cfg.dp_hosts, true) {
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
    // The same kine, read by gateways through the etcd front.
    let front_kine =
        etcd_front::kine_endpoint(&cfg.kine_endpoint).map_err(BootError::KineEndpoint)?;
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
    let lookups = db.dedicated_within(
        billing::access::LOOKUP_POOL_SIZE,
        billing::access::LOOKUP_ACQUIRE_TIMEOUT,
    );
    let upstream = upstream_http::Client::new(outbound).map_err(BootError::OutboundPolicy)?;
    let library = pricing::library::Source::new(library).map_err(BootError::LibrarySource)?;
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
        telemetry_bodies: Arc::new(Semaphore::new(dp::telemetry::MAX_BODIES_IN_FLIGHT)),
        prices: Arc::new(PriceBookCache::new(catalogue)),
        lookups,
        upstream: Arc::new(upstream),
        upstream_models: Arc::new(upstream_models::Cache::default()),
        model_probes: Arc::new(Semaphore::new(model_probes::MAX_IN_FLIGHT)),
        library: Arc::new(library),
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
    let mut tasks = vec![housekeeping::spawn(
        state.db.clone(),
        WriteCtx::of(&state),
        stopping.clone(),
    )];
    let mut projector_stop = stopping.clone();
    tasks.push(projector::spawn(
        state.db.clone(),
        kine,
        WriteCtx::of(&state),
        async move { stopped(&mut projector_stop).await },
    ));
    tasks.push(billing::settler::spawn(
        state.db.clone(),
        WriteCtx::of(&state),
        stopping.clone(),
    ));
    if let Some((listener, tls, _)) = dp {
        tasks.push(tokio::spawn(dp::listener::serve_dp(
            listener,
            tls,
            http::dp_router(state.clone()),
            Some(etcd_front::routes(etcd_front::Front::new(
                front_kine,
                state.creds.clone(),
                state.env.clone(),
            ))),
            dp::listener::Admission {
                env: state.env.clone(),
                creds: state.creds.clone(),
            },
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
