//! Control-plane configuration, read from the environment at boot.
//!
//! # Environment variables
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `CP_DATABASE_URL` | required | Postgres URL of the control-plane database. Used as written; displayed only reduced to `scheme://host[:port]/`. |
//! | `CP_MASTER_KEY_FILE` | required | Path of a file holding the master key: 64 hex characters, optionally followed by one newline. The key seals the CA private key and upstream credentials at rest; losing it means re-issuing every gateway certificate and re-entering every credential. |
//! | `CP_DP_HOSTS` | required | Comma-separated DNS names or IP addresses that gateways use to reach the gateway listener. They become the SANs of the control plane's server certificate. |
//! | `CP_HTTP_ADDR` | `127.0.0.1:8090` | Browser listener (`/healthz`, `/admin/*`, `/api/*`), meant to sit behind a reverse proxy. |
//! | `CP_DP_ADDR` | `0.0.0.0:7943` | Gateway listener (mTLS: `/dp/*` and the read-only etcd front). |
//! | `CP_DP_BASE_URL` | `https://<first CP_DP_HOSTS entry>:<port of CP_DP_ADDR>` | The URL gateways are given for the gateway listener; must be `https://` and name one of the `CP_DP_HOSTS` (in any case), since the listener's certificate names only those. The gateways' etcd endpoint is its `host:port` (443 when it names no port). |
//! | `CP_KINE_ENDPOINT` | `http://127.0.0.1:2379` | kine's etcd endpoint, plaintext. Must name a loopback address (`127.0.0.0/8`, `::1` or `localhost`), checked at boot: kine has no authentication of its own, so it must never listen beyond this host. |
//! | `CP_ETCD_PREFIX` | `/aisix` | Root of the etcd key space. Gateway resources live under `<prefix>/<env_id>/`, shared ones under `<prefix>/global/`. A trailing `/` is dropped. Fixed by the first boot: a later boot with another prefix refuses to start. |
//! | `CP_ENV_ID` | unset | Seeds the environment id (a UUID) on the first boot; unset, the first boot mints one. A later boot whose `CP_ENV_ID` disagrees with the stored id refuses to start. |
//! | `CP_INSTANCE_STALE_SECS` | `60` | A gateway instance whose last heartbeat is older than this many seconds is shown as offline. |
//! | `CP_PORTAL_URL` | unset | Public URL of the tenant portal. Invitations carry a link only when it is set. |
//! | `CP_DB_LOCK_TIMEOUT_MS` | `5000` | Postgres `lock_timeout` on every connection: a statement waiting longer for a lock fails instead of hanging. |
//! | `CP_DB_STATEMENT_TIMEOUT_MS` | `30000` | Postgres `statement_timeout` on every connection. |
//!
//! Empty values count as unset. Errors name the variable and never its
//! value. The variables are checked in the order of this table, except that
//! the master key file is read last.
//!
//! # Running the tests locally
//!
//! The tests that need Postgres return early unless `CP_TEST_DATABASE_URL`
//! is set (on CI they fail instead). `scripts/cp-test-services.sh` runs a
//! throwaway Postgres cluster and kine for them:
//!
//! ```text
//! scripts/cp-test-services.sh start
//! eval "$(scripts/cp-test-services.sh env)"
//! cargo test -p aisix-control-plane
//! scripts/cp-test-services.sh stop
//! ```
//!
//! `CP_TEST_PG_PORT` (default 55432), `CP_TEST_KINE_PORT` (default 52379)
//! and `CP_TEST_DIR` (default `${TMPDIR:-/tmp}/aisix-cp-test-$USER`) move
//! the services when those ports or that directory are taken; pass the same
//! values to every subcommand. Each test clones its own database from a
//! migrated template on that server and drops it afterwards.

use std::env::VarError;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;

use axum::http::Uri;
use uuid::Uuid;

use crate::crypto::{CryptoError, MasterKey};

/// Every variable [`CpConfig::from_env`] reads.
const VARS: [&str; 13] = [
    "CP_DATABASE_URL",
    "CP_MASTER_KEY_FILE",
    "CP_DP_HOSTS",
    "CP_HTTP_ADDR",
    "CP_DP_ADDR",
    "CP_DP_BASE_URL",
    "CP_KINE_ENDPOINT",
    "CP_ETCD_PREFIX",
    "CP_ENV_ID",
    "CP_INSTANCE_STALE_SECS",
    "CP_PORTAL_URL",
    "CP_DB_LOCK_TIMEOUT_MS",
    "CP_DB_STATEMENT_TIMEOUT_MS",
];

/// Postgres takes `lock_timeout` and `statement_timeout` as a 32-bit count of
/// milliseconds.
const MAX_PG_TIMEOUT_MS: u64 = i32::MAX as u64;
const PG_TIMEOUT_REASON: &str = "must be a positive number of milliseconds, at most 2147483647";

/// A configuration error. Its `Display` names the variable, never its value.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{var} is required")]
    Missing { var: &'static str },
    /// `reason` is fixed text, so a value can never end up in it.
    #[error("{var} {reason}")]
    Invalid {
        var: &'static str,
        reason: &'static str,
    },
    #[error("CP_MASTER_KEY_FILE: {0}")]
    MasterKey(CryptoError),
}

pub struct CpConfig {
    pub database_url: String,
    pub master_key: MasterKey,
    pub http_addr: SocketAddr,
    pub dp_addr: SocketAddr,
    pub dp_hosts: Vec<String>,
    pub dp_base_url: String,
    pub kine_endpoint: String,
    pub etcd_prefix: String,
    pub env_id_seed: Option<Uuid>,
    pub instance_stale_secs: u64,
    pub portal_url: Option<String>,
    pub db_lock_timeout_ms: u64,
    pub db_statement_timeout_ms: u64,
    /// The version this control plane reports, and compares gateways'
    /// heartbeat versions with: `aisix_core::HEARTBEAT_VERSION`, stamped the
    /// way a gateway's is. Not read from the environment.
    pub cp_version: String,
}

/// URLs are shown reduced to their origin, and the master key not at all.
impl fmt::Debug for CpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use aisix_core::redact_url_for_log as redact;
        f.debug_struct("CpConfig")
            .field("database_url", &redact(&self.database_url))
            .field("master_key", &self.master_key)
            .field("http_addr", &self.http_addr)
            .field("dp_addr", &self.dp_addr)
            .field("dp_hosts", &self.dp_hosts)
            .field("dp_base_url", &redact(&self.dp_base_url))
            .field("kine_endpoint", &redact(&self.kine_endpoint))
            .field("etcd_prefix", &self.etcd_prefix)
            .field("env_id_seed", &self.env_id_seed)
            .field("instance_stale_secs", &self.instance_stale_secs)
            .field("portal_url", &self.portal_url.as_deref().map(redact))
            .field("db_lock_timeout_ms", &self.db_lock_timeout_ms)
            .field("db_statement_timeout_ms", &self.db_statement_timeout_ms)
            .field("cp_version", &self.cp_version)
            .finish()
    }
}

impl CpConfig {
    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<CpConfig, ConfigError> {
        let mut owned = Vec::new();
        for var in VARS {
            match std::env::var(var) {
                Ok(value) => owned.push((var, value)),
                Err(VarError::NotPresent) => {}
                Err(VarError::NotUnicode(_)) => {
                    return Err(ConfigError::Invalid {
                        var,
                        reason: "is not valid UTF-8",
                    })
                }
            }
        }
        let vars: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        Self::from_vars(&vars)
    }

    /// Builds the configuration from `(name, value)` pairs; a later pair
    /// overrides an earlier one with the same name.
    pub fn from_vars(vars: &[(&str, &str)]) -> Result<CpConfig, ConfigError> {
        let get = |name: &str| {
            debug_assert!(VARS.contains(&name), "{name} is missing from VARS");
            vars.iter()
                .rev()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| *v)
                .filter(|v| !v.trim().is_empty())
        };
        let database_url = get("CP_DATABASE_URL")
            .ok_or(ConfigError::Missing {
                var: "CP_DATABASE_URL",
            })?
            .to_string();
        let dp_hosts = parse_hosts(get("CP_DP_HOSTS"))?;
        let http_addr = parse_addr(
            "CP_HTTP_ADDR",
            get("CP_HTTP_ADDR"),
            SocketAddr::from(([127, 0, 0, 1], 8090)),
        )?;
        let dp_addr = parse_addr(
            "CP_DP_ADDR",
            get("CP_DP_ADDR"),
            SocketAddr::from(([0, 0, 0, 0], 7943)),
        )?;
        let dp_base_url = match get("CP_DP_BASE_URL") {
            Some(url) => parse_base_url(url, &dp_hosts)?,
            None => default_base_url(&dp_hosts[0], dp_addr.port()),
        };
        let kine_endpoint = parse_kine_endpoint(get("CP_KINE_ENDPOINT"))?;
        let etcd_prefix = parse_prefix(get("CP_ETCD_PREFIX"))?;
        let env_id_seed = get("CP_ENV_ID")
            .map(|v| {
                Uuid::parse_str(v.trim()).map_err(|_| ConfigError::Invalid {
                    var: "CP_ENV_ID",
                    reason: "must be a UUID",
                })
            })
            .transpose()?;
        let instance_stale_secs = parse_positive(
            "CP_INSTANCE_STALE_SECS",
            get("CP_INSTANCE_STALE_SECS"),
            60,
            u64::MAX,
            "must be a positive integer",
        )?;
        let portal_url = get("CP_PORTAL_URL").map(str::to_string);
        let db_lock_timeout_ms = parse_positive(
            "CP_DB_LOCK_TIMEOUT_MS",
            get("CP_DB_LOCK_TIMEOUT_MS"),
            5_000,
            MAX_PG_TIMEOUT_MS,
            PG_TIMEOUT_REASON,
        )?;
        let db_statement_timeout_ms = parse_positive(
            "CP_DB_STATEMENT_TIMEOUT_MS",
            get("CP_DB_STATEMENT_TIMEOUT_MS"),
            30_000,
            MAX_PG_TIMEOUT_MS,
            PG_TIMEOUT_REASON,
        )?;
        let master_key_file = get("CP_MASTER_KEY_FILE").ok_or(ConfigError::Missing {
            var: "CP_MASTER_KEY_FILE",
        })?;
        let master_key =
            MasterKey::from_file(Path::new(master_key_file)).map_err(ConfigError::MasterKey)?;
        Ok(CpConfig {
            database_url,
            master_key,
            http_addr,
            dp_addr,
            dp_hosts,
            dp_base_url,
            kine_endpoint,
            etcd_prefix,
            env_id_seed,
            instance_stale_secs,
            portal_url,
            db_lock_timeout_ms,
            db_statement_timeout_ms,
            cp_version: aisix_core::HEARTBEAT_VERSION.to_string(),
        })
    }

    /// The etcd endpoint gateways are given: the `host:port` of
    /// `dp_base_url`, port 443 when it names none. `None` only for a
    /// `dp_base_url` that [`CpConfig::from_vars`] would have refused.
    pub fn dp_etcd_endpoint(&self) -> Option<String> {
        let uri: Uri = self.dp_base_url.parse().ok()?;
        let host = uri.host()?;
        Some(format!("{host}:{}", uri.port_u16().unwrap_or(443)))
    }
}

fn parse_hosts(raw: Option<&str>) -> Result<Vec<String>, ConfigError> {
    const VAR: &str = "CP_DP_HOSTS";
    let raw = raw.ok_or(ConfigError::Missing { var: VAR })?;
    let hosts: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .collect();
    if hosts.is_empty() {
        return Err(ConfigError::Invalid {
            var: VAR,
            reason: "must list at least one DNS name or IP address",
        });
    }
    if !hosts
        .iter()
        .all(|h| h.parse::<IpAddr>().is_ok() || is_dns_name(h))
    {
        return Err(ConfigError::Invalid {
            var: VAR,
            reason: "must list DNS names or IP addresses (no scheme or port), separated by commas",
        });
    }
    Ok(hosts)
}

/// Letters, digits and hyphens in dot-separated labels of 1 to 63
/// characters, no label starting or ending with a hyphen.
fn is_dns_name(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn parse_addr(
    var: &'static str,
    raw: Option<&str>,
    default: SocketAddr,
) -> Result<SocketAddr, ConfigError> {
    match raw {
        None => Ok(default),
        Some(v) => v.trim().parse().map_err(|_| ConfigError::Invalid {
            var,
            reason: "must be an IP address and port, such as 127.0.0.1:8090",
        }),
    }
}

/// The endpoint as written, once its host is known to be loopback.
fn parse_kine_endpoint(raw: Option<&str>) -> Result<String, ConfigError> {
    let Some(raw) = raw else {
        return Ok("http://127.0.0.1:2379".to_string());
    };
    let loopback = raw
        .trim()
        .parse::<Uri>()
        .ok()
        .and_then(|uri| uri.host().map(is_loopback_host))
        .unwrap_or(false);
    if !loopback {
        return Err(ConfigError::Invalid {
            var: "CP_KINE_ENDPOINT",
            reason: "must name a loopback address (127.0.0.0/8, ::1 or localhost): kine has no authentication of its own",
        });
    }
    Ok(raw.to_string())
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The URL as written, once it is `https://` and its host is one of
/// `hosts`: the gateway listener's certificate names only those, so any
/// other host would fail every gateway's TLS check.
fn parse_base_url(raw: &str, hosts: &[String]) -> Result<String, ConfigError> {
    let invalid = ConfigError::Invalid {
        var: "CP_DP_BASE_URL",
        reason: "must be an https:// URL naming a host",
    };
    let url = raw.trim();
    let host = match url.parse::<Uri>() {
        Ok(uri) if uri.scheme_str() == Some("https") => uri.host().map(str::to_string),
        _ => None,
    };
    let Some(host) = host else {
        return Err(invalid);
    };
    if !is_listed_host(&host, hosts) {
        return Err(ConfigError::Invalid {
            var: "CP_DP_BASE_URL",
            reason: "must name one of the CP_DP_HOSTS: the gateway listener's certificate \
                     names only those",
        });
    }
    Ok(url.to_string())
}

/// Whether `host` (as a URL writes it, IPv6 in brackets) is one of `hosts`:
/// a DNS name in any case, or the same IP address.
fn is_listed_host(host: &str, hosts: &[String]) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let ip = host.parse::<IpAddr>().ok();
    hosts.iter().any(|listed| {
        listed.eq_ignore_ascii_case(host)
            || ip.is_some_and(|ip| listed.parse::<IpAddr>().is_ok_and(|l| l == ip))
    })
}

fn default_base_url(host: &str, port: u16) -> String {
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("https://[{host}]:{port}")
    } else {
        format!("https://{host}:{port}")
    }
}

/// The prefix without its trailing `/`, so `/aisix/` and `/aisix` are the
/// same prefix to the boot check, as they are to the gateway.
fn parse_prefix(raw: Option<&str>) -> Result<String, ConfigError> {
    let Some(raw) = raw else {
        return Ok("/aisix".to_string());
    };
    let raw = raw.trim();
    let trimmed = raw.trim_end_matches('/');
    if !raw.starts_with('/') || trimmed.is_empty() {
        return Err(ConfigError::Invalid {
            var: "CP_ETCD_PREFIX",
            reason: "must start with / and name at least one path segment, such as /aisix",
        });
    }
    Ok(trimmed.to_string())
}

fn parse_positive(
    var: &'static str,
    raw: Option<&str>,
    default: u64,
    max: u64,
    reason: &'static str,
) -> Result<u64, ConfigError> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    match raw.trim().parse::<u64>() {
        Ok(n) if n > 0 && n <= max => Ok(n),
        _ => Err(ConfigError::Invalid { var, reason }),
    }
}
