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

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aisix_control_plane::config::CpConfig;
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::Db;
use aisix_control_plane::settings::EnvSettings;
use aisix_control_plane::Running;
use sqlx::{Connection, PgConnection};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// The master key every test control plane runs with.
pub const TEST_MASTER_KEY: [u8; 32] = [0x5a; 32];

/// Serialises template creation across test processes sharing one server.
const TEMPLATE_LOCK_KEY: i64 = 0x6169_7369_7863_7074;

/// The migrated template's name, once per process.
static TEMPLATE: OnceCell<String> = OnceCell::const_new();

/// Reads a gate variable: unset skips the test locally and fails it on CI.
fn gate_var(name: &str) -> Option<String> {
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
}

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
        kine_endpoint: "http://127.0.0.1:2379".to_string(),
        etcd_prefix: "/aisix".to_string(),
        env_id_seed: None,
        instance_stale_secs: 60,
        portal_url: None,
        db_lock_timeout_ms: 5_000,
        db_statement_timeout_ms: 30_000,
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
        let server_url = gate_var("CP_TEST_DATABASE_URL")?;
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
        let database = Arc::new(TestDatabase {
            url: with_database(&server_url, &name),
            server_url,
            name,
        });
        Some(Self::boot(database).await)
    }

    async fn boot(database: Arc<TestDatabase>) -> TestCp {
        let running =
            aisix_control_plane::start(test_config(&database.url), std::future::pending())
                .await
                .expect("the control plane boots");
        TestCp { running, database }
    }

    /// A new control-plane instance on the same database.
    pub async fn restart(&self) -> TestCp {
        Self::boot(self.database.clone()).await
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
