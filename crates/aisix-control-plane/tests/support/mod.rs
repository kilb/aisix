//! Shared harness for the control-plane integration tests.
//!
//! Every test that needs Postgres gets its own database, cloned from a
//! migrated template, and a real control plane serving on an ephemeral port.
//! The server is named by `CP_TEST_DATABASE_URL` (see the module docs of
//! `aisix_control_plane::config` for running it locally). Unset, a test
//! returns early; unset on CI, it fails, because a skipped suite there is
//! silent loss of coverage.

// Each test binary uses a different part of this module.
#![allow(dead_code)]

pub mod members;
pub mod migrate;
pub mod world;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aisix_control_plane::auth::password;
use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::config::CpConfig;
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::Db;
use aisix_control_plane::settings::EnvSettings;
use aisix_control_plane::{AppState, Running};
use serde_json::Value;
use sqlx::{Connection, PgConnection};
use tokio::sync::{OnceCell, Semaphore, SemaphorePermit};
use uuid::Uuid;

/// The master key every test control plane runs with.
pub const TEST_MASTER_KEY: [u8; 32] = [0x5a; 32];

/// The password of every operator the harness seeds.
pub const TEST_PASSWORD: &str = "seeded-password-4b7e";

/// The argon2 hash of [`TEST_PASSWORD`], computed once per process: hashing
/// is slow in a debug build, and every seeded operator can share one.
static TEST_PASSWORD_HASH: OnceCell<String> = OnceCell::const_new();

/// Serialises template creation across test processes sharing one server.
const TEMPLATE_LOCK_KEY: i64 = 0x6169_7369_7863_7074;

/// The migrated template's name, once per process.
static TEMPLATE: OnceCell<String> = OnceCell::const_new();

/// Reads a gate variable: unset skips the test locally and fails it on CI.
pub fn gate_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => {
            assert!(
                std::env::var("CI").is_err(),
                "{name} is not set on CI: skipping here would silently drop coverage"
            );
            None
        }
    }
}

/// `server_url` with its database name replaced by `database`.
fn with_database(server_url: &str, database: &str) -> String {
    let mut url = reqwest::Url::parse(server_url).expect("CP_TEST_DATABASE_URL is a URL");
    url.set_path(&format!("/{database}"));
    url.to_string()
}

/// `cp_tpl_` plus the first 12 hex characters of a SHA-256 over every file
/// in `migrations/` (names and contents, in name order), so a new or changed
/// migration gets a new template instead of a stale one.
fn template_name() -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read migrations/")
        .map(|e| e.expect("read a migrations/ entry").path())
        .collect();
    files.sort();
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    for file in files {
        let name = file.file_name().expect("a file name").to_string_lossy();
        digest.update(name.as_bytes());
        digest.update(b"\0");
        digest.update(&std::fs::read(&file).expect("read a migration"));
        digest.update(b"\0");
    }
    format!("cp_tpl_{}", &hex::encode(digest.finish())[..12])
}

async fn execute(conn: &mut PgConnection, sql: &str) {
    sqlx::query(sql)
        .execute(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Creates the template if it is missing, under a session advisory lock so
/// concurrent test processes create it once. It is migrated under a staging
/// name and renamed only when complete, so a crash halfway never leaves a
/// half-migrated template behind under the real name.
async fn ensure_template(server_url: &str) -> String {
    let name = template_name();
    let mut admin = PgConnection::connect(server_url)
        .await
        .expect("connect to CP_TEST_DATABASE_URL");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut admin)
        .await
        .expect("take the template lock");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&name)
            .fetch_one(&mut admin)
            .await
            .expect("look the template up");
    if !exists {
        let staging = format!("{name}_staging_{}", Uuid::new_v4().simple());
        execute(&mut admin, &format!("CREATE DATABASE \"{staging}\"")).await;
        let db = Db::connect(&with_database(server_url, &staging), 5_000, 30_000)
            .await
            .expect("migrate the template");
        // No connection may stay open on a template, or cloning it fails.
        db.pool().close().await;
        execute(
            &mut admin,
            &format!("ALTER DATABASE \"{staging}\" RENAME TO \"{name}\""),
        )
        .await;
    }
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut admin)
        .await
        .expect("release the template lock");
    let _ = admin.close().await;
    name
}

/// One test's database. Dropped with the last control plane that uses it.
struct TestDatabase {
    server_url: String,
    name: String,
    url: String,
    // Released after `drop` has dropped the database.
    _slot: SemaphorePermit<'static>,
}

/// How many test databases exist at once in one test process: each control
/// plane holds several connections, and a server's `max_connections` is
/// shared by every test binary running beside this one.
///
/// A test holds its slots until it ends, so this is deadlock-free only while
/// fewer than 16 tests of one binary hold two databases at the same time:
/// 16 of them, each holding one slot and waiting for its second, would wait
/// on each other. The wait is bounded by [`DATABASE_SLOT_WAIT`], and reports
/// this.
static DATABASE_SLOTS: Semaphore = Semaphore::const_new(16);

const DATABASE_SLOT_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // Drop cannot await, and the test's runtime may be single-threaded,
        // so the drop runs on a thread with a runtime of its own.
        let server_url = self.server_url.clone();
        let sql = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name);
        let dropped = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(async {
                let mut conn = PgConnection::connect(&server_url)
                    .await
                    .map_err(|e| e.to_string())?;
                sqlx::query(&sql)
                    .execute(&mut conn)
                    .await
                    .map_err(|e| e.to_string())?;
                conn.close().await.map_err(|e| e.to_string())
            })
        })
        .join();
        match dropped {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("could not drop test database {}: {e}", self.name),
            Err(_) => eprintln!(
                "could not drop test database {}: the thread panicked",
                self.name
            ),
        }
    }
}

fn test_config(database_url: &str) -> CpConfig {
    CpConfig {
        database_url: database_url.to_string(),
        master_key: MasterKey(TEST_MASTER_KEY),
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        dp_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        dp_hosts: vec!["localhost".to_string()],
        dp_base_url: "https://localhost:7943".to_string(),
        // Nothing can listen on port 0: a control plane started without
        // kine runs a projector that never reaches one, and never writes
        // to an etcd or kine that happens to run on this host. One that
        // needs the test kine opts in (`TestCp::start_with_kine`).
        kine_endpoint: "http://127.0.0.1:0".to_string(),
        etcd_prefix: "/aisix".to_string(),
        env_id_seed: None,
        instance_stale_secs: 60,
        portal_url: None,
        db_lock_timeout_ms: 5_000,
        db_statement_timeout_ms: 30_000,
        cp_version: aisix_core::HEARTBEAT_VERSION.to_string(),
    }
}

/// A running control plane on its own database.
pub struct TestCp {
    // Declared before `database`: fields drop in order, and the server stops
    // before its database goes.
    running: Running,
    database: Arc<TestDatabase>,
}

impl TestCp {
    /// A control plane on a fresh database, or `None` when
    /// `CP_TEST_DATABASE_URL` is unset (outside CI).
    pub async fn start() -> Option<TestCp> {
        Self::start_with(|_| {}).await
    }

    /// [`TestCp::start`], with `configure` applied to the test
    /// configuration first.
    pub async fn start_with(configure: impl FnOnce(&mut CpConfig)) -> Option<TestCp> {
        let database = new_database().await?;
        let mut cfg = test_config(&database.url);
        configure(&mut cfg);
        Some(Self::boot(database, cfg).await)
    }

    /// A control plane whose projector writes the test kine
    /// (`CP_TEST_KINE_ENDPOINT`), under the fresh environment's own prefix;
    /// `None` when either service is not configured (outside CI).
    pub async fn start_with_kine() -> Option<TestCp> {
        let endpoint = kine_endpoint()?;
        Self::start_with(|cfg| cfg.kine_endpoint = endpoint).await
    }

    /// [`TestCp::start`], with the control plane reporting `cp_version`
    /// (`1.9.0+sha-…`, `dev`) as a stamped build would.
    pub async fn start_stamped(cp_version: &str) -> Option<TestCp> {
        Self::start_with(|cfg| cfg.cp_version = cp_version.to_string()).await
    }

    /// [`TestCp::start_with`], with the `seed` statements run on the fresh
    /// database before the control plane first boots on it.
    pub async fn start_seeded(
        configure: impl FnOnce(&mut CpConfig),
        seed: &[String],
    ) -> Option<TestCp> {
        let database = new_database().await?;
        let mut conn = PgConnection::connect(&database.url)
            .await
            .expect("connect to the test database");
        for sql in seed {
            execute(&mut conn, sql).await;
        }
        let _ = conn.close().await;
        let mut cfg = test_config(&database.url);
        configure(&mut cfg);
        Some(Self::boot(database, cfg).await)
    }

    /// [`TestCp::start`], pricing from `catalogue` instead of the built-in
    /// one.
    pub async fn start_with_catalogue(catalogue: Arc<aisix_pricing::Catalogue>) -> Option<TestCp> {
        let database = new_database().await?;
        let cfg = test_config(&database.url);
        let running =
            aisix_control_plane::start_with_catalogue(cfg, std::future::pending(), Some(catalogue))
                .await
                .expect("the control plane boots");
        Some(TestCp { running, database })
    }

    /// [`TestCp::start`], whose outbound fetch may reach a fixture server on
    /// a loopback address. The production policy refuses one
    /// ([`TestCp::start`] keeps it); a test that must watch what the control
    /// plane actually sends needs a real server, and this host has no
    /// publicly routable address to put one on.
    pub async fn start_reaching_loopback() -> Option<TestCp> {
        let database = new_database().await?;
        let cfg = test_config(&database.url);
        let policy = aisix_core::config::SsrfProtectionConfig {
            allowed_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        };
        let running =
            aisix_control_plane::start_with_policy(cfg, std::future::pending(), None, &policy)
                .await
                .expect("the control plane boots");
        Some(TestCp { running, database })
    }

    async fn boot(database: Arc<TestDatabase>, cfg: CpConfig) -> TestCp {
        let running = aisix_control_plane::start(cfg, std::future::pending())
            .await
            .expect("the control plane boots");
        TestCp { running, database }
    }

    /// A new control-plane instance on the same database, with the default
    /// test configuration.
    pub async fn restart(&self) -> TestCp {
        let cfg = test_config(&self.database.url);
        Self::boot(self.database.clone(), cfg).await
    }

    /// [`TestCp::restart`], with the new instance reporting `cp_version`, as
    /// an upgraded stamped build would.
    pub async fn restart_stamped(&self, cp_version: &str) -> TestCp {
        let mut cfg = test_config(&self.database.url);
        cfg.cp_version = cp_version.to_string();
        Self::boot(self.database.clone(), cfg).await
    }

    /// [`TestCp::restart`], with the new instance pricing from `catalogue`
    /// instead of the built-in one, as an upgraded build shipping a newer
    /// catalogue would.
    pub async fn restart_with_catalogue(&self, catalogue: Arc<aisix_pricing::Catalogue>) -> TestCp {
        let cfg = test_config(&self.database.url);
        let running =
            aisix_control_plane::start_with_catalogue(cfg, std::future::pending(), Some(catalogue))
                .await
                .expect("the control plane boots");
        TestCp {
            running,
            database: self.database.clone(),
        }
    }

    pub fn db(&self) -> &Db {
        &self.running.state.db
    }

    pub fn env(&self) -> &EnvSettings {
        &self.running.state.env
    }

    pub fn master_key(&self) -> &MasterKey {
        &self.running.state.cfg.master_key
    }

    /// The database URL, for a second connection with other settings.
    pub fn database_url(&self) -> &str {
        &self.database.url
    }

    /// `http://<browser listener><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.running.http_addr)
    }

    /// Where the gateway listener is bound.
    pub fn dp_addr(&self) -> SocketAddr {
        self.dp_listening()
            .expect("the gateway listener is serving")
    }

    /// Where the gateway listener is bound, or `None` when it does not run.
    pub fn dp_listening(&self) -> Option<SocketAddr> {
        self.running.dp_addr
    }

    /// Issues a gateway credential as `admin`, which must succeed.
    pub async fn issue_bundle(&self, admin: &OperatorSession, name: &str) -> Bundle {
        let r = self
            .send(
                "POST",
                "/admin/gateways/credentials",
                Some(admin.cookie()),
                Some(serde_json::json!({ "name": name })),
            )
            .await;
        assert_eq!(r.status, 201, "issue {name}: {r:?}");
        let text = |field: &str| {
            r.body[field]
                .as_str()
                .unwrap_or_else(|| panic!("{field} in {r:?}"))
                .to_string()
        };
        Bundle {
            dp_id: text("dp_id"),
            cert_pem: text("cert_pem"),
            key_pem: text("key_pem"),
            ca_pem: text("ca_pem"),
        }
    }

    /// Whom a gateway listener of this control plane admits past the
    /// handshake, for a listener a test serves itself.
    pub fn admission(&self) -> aisix_control_plane::dp::listener::Admission {
        aisix_control_plane::dp::listener::Admission {
            env: self.running.state.env.clone(),
            creds: self.running.state.creds.clone(),
        }
    }

    /// A gateway's REST client for `bundle`, on this control plane's
    /// gateway listener.
    pub fn dp_client(&self, bundle: &Bundle) -> DpClient {
        DpClient::new(
            self.dp_addr(),
            &bundle.cert_pem,
            &bundle.key_pem,
            &bundle.ca_pem,
        )
    }

    /// A gateway's REST client for a credential named `name`, issued by an
    /// admin seeded for it.
    pub async fn issue_dp_client(&self, name: &str) -> DpClient {
        let admin = self.admin_login_seeded(OperatorRole::Admin).await;
        let bundle = self.issue_bundle(&admin, name).await;
        self.dp_client(&bundle)
    }

    /// The number of rows in `table`.
    pub async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(self.db().pool())
            .await
            .unwrap_or_else(|e| panic!("count {table}: {e}"))
    }

    pub fn state(&self) -> &AppState {
        &self.running.state
    }

    /// How many argon2 verifications this instance has run.
    pub fn verification_count(&self) -> u64 {
        self.running.state.passwords.verification_count()
    }

    /// Inserts an operator whose password is [`TEST_PASSWORD`].
    pub async fn seed_operator_as(&self, email: &str, role: OperatorRole, disabled: bool) -> Uuid {
        let hash = TEST_PASSWORD_HASH
            .get_or_init(|| async {
                tokio::task::spawn_blocking(|| password::hash(TEST_PASSWORD))
                    .await
                    .expect("join the hashing task")
                    .expect("hash the test password")
            })
            .await;
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO operators (id, email, password_hash, role, disabled) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(email)
        .bind(hash)
        .bind(role.as_str())
        .bind(disabled)
        .execute(self.db().pool())
        .await
        .expect("seed an operator");
        id
    }

    /// An enabled operator with a fresh email and [`TEST_PASSWORD`].
    pub async fn seed_operator(&self, role: OperatorRole) -> SeededOperator {
        let email = format!("{}-{}@ops.test", role.as_str(), Uuid::new_v4().simple());
        let id = self.seed_operator_as(&email, role, false).await;
        SeededOperator { id, email, role }
    }

    /// `POST /admin/login`.
    pub async fn admin_login(&self, email: &str, password: &str) -> Resp {
        self.send(
            "POST",
            "/admin/login",
            None,
            Some(serde_json::json!({ "email": email, "password": password })),
        )
        .await
    }

    /// Logs in as `op`, which must succeed.
    pub async fn admin_login_as(&self, op: &SeededOperator) -> OperatorSession {
        let r = self.admin_login(&op.email, TEST_PASSWORD).await;
        assert_eq!(r.status, 200, "log in as {}: {r:?}", op.email);
        let cookie = r
            .set_cookie
            .as_deref()
            .and_then(|c| c.split(';').next())
            .expect("a session cookie")
            .to_string();
        OperatorSession {
            id: op.id,
            email: op.email.clone(),
            role: op.role,
            cookie,
        }
    }

    /// Seeds an operator with `role` and logs in as it.
    pub async fn admin_login_seeded(&self, role: OperatorRole) -> OperatorSession {
        let op = self.seed_operator(role).await;
        self.admin_login_as(&op).await
    }

    pub async fn get_with_cookie(&self, path: &str, cookie: &str) -> Resp {
        self.send("GET", path, Some(cookie), None).await
    }

    /// Sends `method path` with `cookie` (the `Cookie` header's value) and a
    /// JSON `body`, labelled JSON whether or not there is one, as the
    /// Dashboard sends every request.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> Resp {
        let body = body.map(|b| serde_json::to_vec(&b).expect("serialize the body"));
        self.send_raw(method, path, cookie, Some("application/json"), body)
            .await
    }

    /// Sends `method path` with `cookie`, the `Content-Type` given (none when
    /// `None`) and `body` as raw bytes (none when `None`).
    pub async fn send_raw(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> Resp {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an HTTP client");
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
        let mut request = client.request(method, self.url(path));
        if let Some(cookie) = cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        if let Some(content_type) = content_type {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.expect("send the request");
        Resp::read(response).await
    }
}

/// A gateway credential as issued: what a gateway is given.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub dp_id: String,
    pub cert_pem: String,
    pub key_pem: String,
    pub ca_pem: String,
}

/// A gateway's REST client, built the way the gateway builds it
/// (`build_client` in `crates/aisix-server/src/heartbeat.rs`): the key and
/// the certificate as one identity, the CA as a root, HTTP/1.1 only.
/// `localhost` resolves to the listener's address.
pub struct DpClient {
    client: reqwest::Client,
    base: String,
}

impl DpClient {
    pub fn new(addr: SocketAddr, cert_pem: &str, key_pem: &str, ca_pem: &str) -> DpClient {
        let mut identity = key_pem.to_string();
        if !identity.ends_with('\n') {
            identity.push('\n');
        }
        identity.push_str(cert_pem);
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(10))
            .identity(reqwest::Identity::from_pem(identity.as_bytes()).expect("an mTLS identity"))
            .add_root_certificate(
                reqwest::Certificate::from_pem(ca_pem.as_bytes()).expect("the CA"),
            )
            .http1_only()
            .use_rustls_tls()
            .resolve("localhost", addr)
            .build()
            .expect("the gateway's REST client");
        DpClient {
            client,
            base: format!("https://localhost:{}", addr.port()),
        }
    }

    /// Sends `method path` with a JSON `body`; an error when the request
    /// never got an answer (a refused handshake, say).
    pub async fn try_send(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Resp, reqwest::Error> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(&body);
        }
        Ok(Resp::read(request.send().await?).await)
    }

    pub async fn send(&self, method: &str, path: &str, body: Option<Value>) -> Resp {
        self.try_send(method, path, body)
            .await
            .unwrap_or_else(|e| panic!("{method} {path} got no answer: {e}"))
    }

    /// `POST /dp/heartbeat` with `body`.
    pub async fn heartbeat(&self, body: Value) -> Resp {
        self.send("POST", "/dp/heartbeat", Some(body)).await
    }

    /// `POST /dp/telemetry` as the gateway sends it: `body` as JSON, with
    /// `X-Aisix-Usage-Batch-Id: batch_id`.
    pub async fn telemetry(&self, batch_id: Uuid, body: &Value) -> Resp {
        let bytes = serde_json::to_vec(body).expect("serialize the batch");
        self.telemetry_raw(Some(&batch_id.to_string()), bytes)
            .await
            .unwrap_or_else(|e| panic!("POST /dp/telemetry got no answer: {e}"))
    }

    /// `POST /dp/telemetry` with `body` as raw bytes labelled JSON, and the
    /// batch-id header as given (none when `None`); an error when the
    /// request never got an answer.
    pub async fn telemetry_raw(
        &self,
        batch_id: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Resp, reqwest::Error> {
        let mut request = self
            .client
            .post(format!("{}/dp/telemetry", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(batch_id) = batch_id {
            request = request.header("X-Aisix-Usage-Batch-Id", batch_id);
        }
        Ok(Resp::read(request.send().await?).await)
    }
}

/// A live provider group named `name`, created through the API as `admin`
/// at the default rate `"1"`; its id.
///
/// Every write that names a group names a live one, so a fixture that lists
/// `name` in a provider key, a tenant or the tenant defaults creates it
/// first with this. A group that already exists is kept: a helper that runs
/// once per model, twice in one test whose control plane outlives the first
/// run, would otherwise be refused for a name it created itself.
pub async fn seed_group(cp: &TestCp, admin: &OperatorSession, name: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/provider-groups",
            Some(admin.cookie()),
            Some(serde_json::json!({
                "name": name,
                "display_name": name,
                "description": "测试分组",
                "default_cny_per_usd": "1",
            })),
        )
        .await;
    if r.status == 201 {
        return Uuid::parse_str(
            r.body["id"]
                .as_str()
                .unwrap_or_else(|| panic!("a group id in {}", r.body)),
        )
        .expect("a group id");
    }
    // The routing name is unique, retired groups included, so an existing
    // name answers a 422 at `/name`; the row is read back to give its id.
    assert_eq!(
        r.body["code"], "validation_failed",
        "create group {name}: {r:?}"
    );
    sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = $1")
        .bind(name)
        .fetch_one(cp.db().pool())
        .await
        .unwrap_or_else(|e| panic!("read group {name}: {e}"))
}

/// An operator the harness inserted.
#[derive(Debug, Clone)]
pub struct SeededOperator {
    pub id: Uuid,
    pub email: String,
    pub role: OperatorRole,
}

/// A logged-in operator.
#[derive(Debug, Clone)]
pub struct OperatorSession {
    pub id: Uuid,
    pub email: String,
    pub role: OperatorRole,
    cookie: String,
}

impl OperatorSession {
    /// `aisix_cp_admin=<token>`, for a `Cookie` header.
    pub fn cookie(&self) -> &str {
        &self.cookie
    }
}

/// An answer: the status, the body (JSON, or the text when it is not JSON,
/// or null when empty), the `Set-Cookie` header and all the headers.
#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub body: Value,
    pub set_cookie: Option<String>,
    pub headers: reqwest::header::HeaderMap,
}

impl Resp {
    async fn read(response: reqwest::Response) -> Resp {
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let set_cookie = headers
            .get(reqwest::header::SET_COOKIE)
            .map(|v| v.to_str().expect("an ASCII Set-Cookie").to_string());
        let text = response.text().await.expect("read the body");
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Resp {
            status,
            body,
            set_cookie,
            headers,
        }
    }

    /// The header `name`'s value, when it is present and readable as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// A file in the system temp directory, removed on drop.
pub struct TempFile(PathBuf);

impl TempFile {
    pub fn with_contents(contents: &[u8]) -> TempFile {
        let path = std::env::temp_dir().join(format!("aisix-cp-test-{}", Uuid::new_v4().simple()));
        std::fs::write(&path, contents).expect("write a temp file");
        TempFile(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn path_str(&self) -> &str {
        self.0.to_str().expect("the temp dir is UTF-8")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A fresh database cloned from the migrated template, or `None` when
/// `CP_TEST_DATABASE_URL` is unset (outside CI). Waits while
/// [`DATABASE_SLOTS`] are all taken.
async fn new_database() -> Option<Arc<TestDatabase>> {
    let server_url = gate_var("CP_TEST_DATABASE_URL")?;
    let slot = tokio::time::timeout(DATABASE_SLOT_WAIT, DATABASE_SLOTS.acquire())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "waited {DATABASE_SLOT_WAIT:?} for a test database slot: too many tests of \
                 this binary hold two databases at once (see DATABASE_SLOTS)"
            )
        })
        .expect("the database slots are never closed");
    let template = TEMPLATE
        .get_or_init(|| ensure_template(&server_url))
        .await
        .clone();
    let name = format!("t_{}", Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(&server_url)
        .await
        .expect("connect to CP_TEST_DATABASE_URL");
    execute(
        &mut admin,
        &format!("CREATE DATABASE \"{name}\" TEMPLATE \"{template}\""),
    )
    .await;
    let _ = admin.close().await;
    Some(Arc::new(TestDatabase {
        url: with_database(&server_url, &name),
        server_url,
        name,
        _slot: slot,
    }))
}

/// The test kine's etcd endpoint (`CP_TEST_KINE_ENDPOINT`), shared by every
/// test; each control plane keeps to its own environment's prefix.
pub fn kine_endpoint() -> Option<String> {
    gate_var("CP_TEST_KINE_ENDPOINT")
}

/// The kine binary (`CP_TEST_KINE_BIN`), for a test that runs a kine of its
/// own.
pub fn kine_bin() -> Option<String> {
    gate_var("CP_TEST_KINE_BIN")
}

/// The server `CP_TEST_DATABASE_URL` names, for a test that creates a
/// database of its own.
pub fn database_server_url() -> Option<String> {
    gate_var("CP_TEST_DATABASE_URL")
}

/// A migrated database pinned to a fresh environment, with no control plane
/// on it: for tests that run the projector themselves, so that no other
/// projector competes for the database.
pub struct BareDatabase {
    pub db: Db,
    pub env: EnvSettings,
    // Declared last: the pool goes before its database.
    _database: Arc<TestDatabase>,
}

impl BareDatabase {
    pub async fn new() -> Option<BareDatabase> {
        let database = new_database().await?;
        let db = Db::connect(&database.url, 5_000, 30_000)
            .await
            .expect("open the test database");
        let env = aisix_control_plane::settings::ensure_env(&db, None, "/aisix")
            .await
            .expect("pin an environment");
        Some(BareDatabase {
            db,
            env,
            _database: database,
        })
    }

    /// The database URL, for connections of a test's own.
    pub fn url(&self) -> &str {
        &self._database.url
    }
}

/// A control plane on the test kine whose shutdown the test triggers, on a
/// database that lives as long as this does.
pub struct StoppableCp {
    running: Option<Running>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    // A handle of its own: still usable once the control plane has stopped.
    db: Db,
    // Declared last: the database goes after everything using it.
    database: Arc<TestDatabase>,
}

impl StoppableCp {
    /// `None` when either service is not configured (outside CI).
    pub async fn start_with_kine() -> Option<StoppableCp> {
        let endpoint = kine_endpoint()?;
        let database = new_database().await?;
        let mut cfg = test_config(&database.url);
        cfg.kine_endpoint = endpoint;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let running = aisix_control_plane::start(cfg, async move {
            let _ = stopped.await;
        })
        .await
        .expect("the control plane boots");
        let db = running.state.db.clone();
        Some(StoppableCp {
            running: Some(running),
            stop: Some(stop),
            db,
            database,
        })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The database URL, for a session of the test's own.
    pub fn database_url(&self) -> &str {
        &self.database.url
    }

    /// Resolves the shutdown and waits for the control plane to stop: how
    /// long that took, or `None` when it took longer than `within`.
    pub async fn shut_down(&mut self, within: std::time::Duration) -> Option<std::time::Duration> {
        let running = self.running.take().expect("still running");
        let started = std::time::Instant::now();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match tokio::time::timeout(within, running.wait()).await {
            Ok(stopped) => {
                stopped.expect("the control plane stops cleanly");
                Some(started.elapsed())
            }
            Err(_) => None,
        }
    }
}
