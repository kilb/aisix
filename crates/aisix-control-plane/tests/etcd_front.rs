//! The etcd front: gateways read their environment's configuration from
//! kine through the gateway listener, over h2, confined to their own
//! environment's prefix and the global one, and can write nothing.
//!
//! The client is the gateway's own: etcd-client 0.14.1 connected with the
//! gateway's options and a gateway certificate, and the gateway's
//! configuration provider (`aisix_etcd::EtcdConfigProvider`) where the
//! gateway's own load or watch is what is being tested.
//!
//! Real kine (`CP_TEST_KINE_ENDPOINT`), seeded directly, since the front
//! writes nothing. The cases that compact, or would compact if the front let
//! them, run a kine of their own (`CP_TEST_KINE_BIN`) on a database of their
//! own, so no other test's revisions are touched.

mod support;

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::dp::listener::{self, ConnLimits};
use aisix_control_plane::etcd_front::proto::etcdserverpb::kv_client::KvClient as RawKv;
use aisix_control_plane::etcd_front::proto::etcdserverpb::watch_client::WatchClient as RawWatch;
use aisix_control_plane::etcd_front::proto::etcdserverpb::{
    watch_request, RangeRequest, WatchCreateRequest, WatchRequest,
};
use aisix_control_plane::etcd_front::{self, Front};
use aisix_control_plane::gateways::CREDENTIALS_CHANNEL;
use aisix_control_plane::pki::Pki;
use aisix_control_plane::settings::EnvSettings;
use aisix_etcd::{ConfigProvider, EtcdConfigProvider, ProviderError, WatchEvent};
use etcd_client::{
    Certificate, Client, Compare, CompareOp, ConnectOptions, EventType, GetOptions, Identity,
    TlsOptions, Txn, TxnOp, WatchOptions,
};
use futures::StreamExt;
use hyper_util::rt::TokioIo;
use rcgen::KeyPair;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde_json::json;
use sqlx::{Connection, PgConnection};
use support::{Bundle, OperatorSession, TestCp};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Code;
use uuid::Uuid;
use x509_parser::pem::parse_x509_pem;

/// The refusal for a key outside the gateway's key spaces, as the gateway's
/// own fixtures spell it (`crates/aisix-etcd/src/supervisor.rs`).
fn outside(cp: &TestCp) -> String {
    format!(
        "etcdserver: permission denied: outside env {} prefix",
        cp.env().env_id
    )
}

const READ_ONLY: &str = "etcdserver: permission denied: read-only endpoint";

/// The first key after every key that starts with `prefix` (etcd's
/// `prefix + 1`).
fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return end;
        }
    }
    vec![0]
}

/// The gateway's etcd connect options in managed mode
/// (`build_etcd_connect_options_with_extra_ca` in `crates/aisix-server`,
/// plus the keepalive `LazyEtcdClient::new` adds): pings every 10 s and
/// only while a stream is open, the bundle's certificate, and the bundle's
/// CA as the trust root for the listener's name.
fn gateway_options(bundle: &Bundle) -> ConnectOptions {
    ConnectOptions::new()
        .with_keep_alive_while_idle(false)
        .with_keep_alive(Duration::from_secs(10), Duration::from_secs(10))
        .with_connect_timeout(Duration::from_secs(5))
        .with_tls(
            TlsOptions::new()
                .domain_name("localhost")
                .ca_certificate(Certificate::from_pem(bundle.ca_pem.clone()))
                .identity(Identity::from_pem(
                    bundle.cert_pem.clone(),
                    bundle.key_pem.clone(),
                )),
        )
}

/// The front's endpoint, as the gateway dials it (the REST base URL's
/// host and port).
fn front(cp: &TestCp) -> String {
    format!("https://127.0.0.1:{}", cp.dp_addr().port())
}

async fn gateway(cp: &TestCp, bundle: &Bundle) -> Client {
    Client::connect([front(cp)], Some(gateway_options(bundle)))
        .await
        .expect("connect through the front")
}

/// The gateway's configuration provider for `prefix`, through the front.
async fn provider(cp: &TestCp, bundle: &Bundle, prefix: &str) -> EtcdConfigProvider {
    EtcdConfigProvider::connect(
        &[front(cp)],
        prefix,
        Some(gateway_options(bundle)),
        Some(Duration::from_secs(10)),
        Some(Duration::from_secs(5)),
    )
    .await
    .expect("the gateway's provider connects through the front")
}

/// The status of a failed call, which must be one.
fn status_of<T: std::fmt::Debug>(result: Result<T, etcd_client::Error>) -> tonic::Status {
    match result {
        Err(etcd_client::Error::GRpcStatus(status)) => status,
        other => panic!("expected a gRPC status, got {other:?}"),
    }
}

/// Asserts `result` is the refusal `message`.
fn assert_denied<T: std::fmt::Debug>(result: Result<T, etcd_client::Error>, message: &str) {
    let status = status_of(result);
    assert_eq!(status.code(), Code::PermissionDenied, "{status:?}");
    assert_eq!(status.message(), message);
}

/// A control plane with one gateway credential, and a direct client to the
/// kine it fronts.
struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    bundle: Bundle,
    kine: Client,
}

impl Fixture {
    /// On the shared test kine, or `None` without the test services.
    async fn start() -> Option<Fixture> {
        let endpoint = support::gate_var("CP_TEST_KINE_ENDPOINT")?;
        let cp = TestCp::start().await?;
        Some(Self::with(cp, &endpoint).await)
    }

    /// On `kine`, a kine of the test's own.
    async fn on(kine: &PrivateKine) -> Option<Fixture> {
        let endpoint = kine.endpoint.clone();
        let cp = TestCp::start_with(|cfg| cfg.kine_endpoint = endpoint).await?;
        Some(Self::with(cp, &kine.endpoint).await)
    }

    async fn with(cp: TestCp, kine_endpoint: &str) -> Fixture {
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let bundle = cp.issue_bundle(&admin, "gw-front-7c1").await;
        let kine = Client::connect([kine_endpoint], None)
            .await
            .expect("connect to kine");
        Fixture {
            cp,
            admin,
            bundle,
            kine,
        }
    }

    fn env(&self) -> String {
        self.cp.env().env_prefix()
    }

    /// Writes `key` straight into kine; its revision.
    async fn put(&self, key: &str, value: &str) -> i64 {
        self.kine
            .clone()
            .put(key, value, None)
            .await
            .expect("put into kine")
            .header()
            .expect("a header")
            .revision()
    }

    /// Deletes `key` from kine, the one way kine deletes: a txn on the
    /// key's mod revision (kine recon §2; `DeleteRange` is unimplemented).
    async fn delete(&self, key: &str) {
        let mut kine = self.kine.clone();
        let current = kine.get(key, None).await.expect("read kine");
        let revision = current.kvs()[0].mod_revision();
        let deleted = kine
            .txn(
                Txn::new()
                    .when([Compare::mod_revision(key, CompareOp::Equal, revision)])
                    .and_then([TxnOp::delete(key, None)])
                    .or_else([TxnOp::get(key, None)]),
            )
            .await
            .expect("delete from kine");
        assert!(deleted.succeeded(), "{key} changed before it was deleted");
    }

    async fn revoke(&self, bundle: &Bundle) {
        let r = self
            .cp
            .send(
                "POST",
                &format!("/admin/gateways/credentials/{}/revoke", bundle.dp_id),
                Some(self.admin.cookie()),
                None,
            )
            .await;
        assert_eq!(r.status, 200, "{r:?}");
    }
}

/// A kine of a test's own, on a database of its own on the test server,
/// with automatic compaction off (kine's default), so the test can compact
/// it. Stopped, and its database dropped, on drop.
struct PrivateKine {
    child: std::process::Child,
    endpoint: String,
    server_url: String,
    database: String,
}

impl PrivateKine {
    async fn start() -> Option<PrivateKine> {
        let server_url = support::gate_var("CP_TEST_DATABASE_URL")?;
        let bin = support::gate_var("CP_TEST_KINE_BIN")?;
        let database = format!("kine_{}", Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(&server_url)
            .await
            .expect("connect to CP_TEST_DATABASE_URL");
        sqlx::query(&format!("CREATE DATABASE \"{database}\""))
            .execute(&mut admin)
            .await
            .expect("create kine's database");
        let _ = admin.close().await;
        let mut url = reqwest::Url::parse(&server_url).expect("CP_TEST_DATABASE_URL is a URL");
        url.set_path(&format!("/{database}"));
        url.set_query(Some("sslmode=disable"));
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        let child = std::process::Command::new(bin)
            .args([
                "--endpoint",
                url.as_str(),
                "--listen-address",
                &format!("127.0.0.1:{port}"),
                "--metrics-bind-address",
                "0",
                "--compact-min-retain",
                "0",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start kine");
        let kine = PrivateKine {
            child,
            endpoint: format!("http://127.0.0.1:{port}"),
            server_url,
            database,
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "kine never listened"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Some(kine)
    }
}

impl Drop for PrivateKine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let server_url = self.server_url.clone();
        let sql = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.database);
        let _ = std::thread::spawn(move || {
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
    }
}

/// The next response on `stream` that carries events, within 5 s.
async fn next_events(stream: &mut etcd_client::WatchStream) -> etcd_client::WatchResponse {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = stream
                .message()
                .await
                .expect("the watch is still open")
                .expect("the watch did not end");
            if !response.events().is_empty() {
                return response;
            }
        }
    })
    .await
    .expect("events within 5 s")
}

/// The next event from the gateway's provider, within 5 s.
async fn next_event(
    events: &mut (dyn futures::Stream<Item = Result<WatchEvent, ProviderError>> + Send + Unpin),
) -> Result<WatchEvent, ProviderError> {
    tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("an event within 5 s")
        .expect("the watch did not end")
}

fn keys_of(kvs: &[etcd_client::KeyValue]) -> Vec<String> {
    kvs.iter()
        .map(|kv| kv.key_str().expect("a UTF-8 key").to_string())
        .collect()
}

#[tokio::test]
async fn a_gateway_certificate_loads_its_environment_page_by_page_through_the_front() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let global = t.cp.env().global_prefix();
    let other_env = format!("/aisix/{}/", Uuid::new_v4());
    let sibling = format!("{}-sibling-7c1/", env.trim_end_matches('/'));
    let mut expected = Vec::new();
    for i in 0..8 {
        let key = format!("{env}models/{i:02}-{}", Uuid::new_v4());
        t.put(&key, "{}").await;
        expected.push(key);
    }
    let mut globals = Vec::new();
    for _ in 0..2 {
        let key = format!("{global}pricing/front-7c1-{}", Uuid::new_v4());
        t.put(&key, "{}").await;
        globals.push(key);
    }
    t.put(&format!("{other_env}models/x"), "another-env-secret-7c1")
        .await;
    t.put(&format!("{sibling}models/x"), "sibling-secret-7c1")
        .await;
    let late = format!("{env}models/99-late");

    // The gateway's load: pages of 3, the later pages pinned to the
    // revision of the first, each starting just after the last key read.
    let mut gw = gateway(&t.cp, &t.bundle).await;
    let end = prefix_end(env.as_bytes());
    let mut start = env.as_bytes().to_vec();
    let mut revision = 0;
    let mut loaded = Vec::new();
    let mut pages = 0;
    loop {
        let mut options = GetOptions::new().with_range(end.clone()).with_limit(3);
        if revision > 0 {
            options = options.with_revision(revision);
        }
        let page = gw
            .get(start.clone(), Some(options))
            .await
            .expect("a page through the front");
        pages += 1;
        if revision == 0 {
            revision = page.header().expect("a header").revision();
            assert_eq!(page.count(), 8, "the count is of this environment only");
            // Written after the first page: the pinned pages do not show it.
            t.put(&late, "{}").await;
        }
        loaded.extend(keys_of(page.kvs()));
        match (page.more(), page.kvs().last()) {
            (true, Some(last)) => {
                start = last.key().to_vec();
                start.push(0);
            }
            _ => break,
        }
    }
    assert_eq!(loaded, expected);
    assert_eq!(pages, 3);

    // The gateway's own provider loads the environment, the late key now
    // included, and the global prefix; nothing from anywhere else.
    let (entries, _) = provider(&t.cp, &t.bundle, &env)
        .await
        .load_all()
        .await
        .expect("the gateway's load through the front");
    let mut want = expected.clone();
    want.push(late);
    assert_eq!(
        entries.iter().map(|e| e.key.clone()).collect::<Vec<_>>(),
        want
    );
    let (entries, _) = provider(&t.cp, &t.bundle, &global)
        .await
        .load_all()
        .await
        .expect("the gateway's global load through the front");
    assert!(entries.iter().all(|e| e.key.starts_with(&global)));
    for key in &globals {
        assert!(entries.iter().any(|e| &e.key == key), "{key} not loaded");
    }
    assert!(entries
        .iter()
        .all(|e| !String::from_utf8_lossy(&e.value).contains("secret-7c1")));
}

#[tokio::test]
async fn a_watch_through_the_front_sees_projected_writes() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let seeded = format!("{env}models/m-{}", Uuid::new_v4());
    t.put(&seeded, "v0").await;
    let provider = provider(&t.cp, &t.bundle, &env).await;
    let (entries, revision) = provider.load_all().await.expect("the load");
    assert_eq!(
        entries.iter().map(|e| e.key.clone()).collect::<Vec<_>>(),
        vec![seeded.clone()]
    );
    let mut events = provider
        .watch(revision + 1)
        .await
        .expect("the gateway's watch through the front");

    // Writes elsewhere first: the watch carries only this environment's.
    let other = format!("/aisix/{}/models/x", Uuid::new_v4());
    t.put(&other, "another-env-7c1").await;
    t.put(
        &format!("{}x/models/x", env.trim_end_matches('/')),
        "sibling-7c1",
    )
    .await;
    t.put(&seeded, "v1").await;
    let added = format!("{env}api_keys/k-{}", Uuid::new_v4());
    t.put(&added, "v2").await;
    t.delete(&added).await;

    match next_event(&mut *events).await {
        Ok(WatchEvent::Put(entry)) => {
            assert_eq!(
                (entry.key.as_str(), entry.value.as_slice()),
                (seeded.as_str(), &b"v1"[..])
            );
        }
        other => panic!("expected the update of {seeded}, got {other:?}"),
    }
    match next_event(&mut *events).await {
        Ok(WatchEvent::Put(entry)) => {
            assert_eq!(
                (entry.key.as_str(), entry.value.as_slice()),
                (added.as_str(), &b"v2"[..])
            );
        }
        other => panic!("expected the put of {added}, got {other:?}"),
    }
    match next_event(&mut *events).await {
        Ok(WatchEvent::Delete { key, .. }) => assert_eq!(key, added),
        other => panic!("expected the delete of {added}, got {other:?}"),
    }
}

#[tokio::test]
async fn a_second_watch_for_another_environment_on_the_same_stream_is_refused() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let other = format!("/aisix/{}/", Uuid::new_v4());
    let mut gw = gateway(&t.cp, &t.bundle).await;
    let (mut watcher, mut stream) = gw
        .watch(env.clone(), Some(WatchOptions::new().with_prefix()))
        .await
        .expect("a watch on its own environment");
    watcher
        .watch(other.clone(), Some(WatchOptions::new().with_prefix()))
        .await
        .expect("send the second create");
    let answer = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("answered within 5 s");
    assert_denied(answer, &outside(&t.cp));
    // The call is over: the other environment's writes never arrive.
    t.put(&format!("{other}models/x"), "another-env-7c1").await;
    let after = tokio::time::timeout(Duration::from_secs(1), stream.message()).await;
    assert!(
        !matches!(&after, Ok(Ok(Some(r))) if !r.events().is_empty()),
        "{after:?}"
    );

    // A first create out of bounds is refused the same way.
    let bare = env.trim_end_matches('/').as_bytes().to_vec();
    let sibling = format!("{}-sibling-7c1/", env.trim_end_matches('/'));
    let cases: Vec<(&str, Vec<u8>, Vec<u8>)> = vec![
        (
            "another environment",
            other.clone().into(),
            prefix_end(other.as_bytes()),
        ),
        ("the bare prefix", bare.clone(), prefix_end(&bare)),
        (
            "a sibling",
            sibling.clone().into(),
            prefix_end(sibling.as_bytes()),
        ),
        ("from a key", env.clone().into(), vec![0]),
        ("all keys", vec![0], vec![0]),
        ("an empty key", Vec::new(), prefix_end(env.as_bytes())),
        (
            "past the environment",
            env.clone().into(),
            b"/aisix0".to_vec(),
        ),
    ];
    for (what, key, end) in cases {
        let refused = gw
            .watch(key, Some(WatchOptions::new().with_range(end)))
            .await
            .map(|(watcher, _)| watcher.watch_id());
        let status = status_of(refused);
        assert_eq!(status.code(), Code::PermissionDenied, "{what}: {status:?}");
        assert_eq!(status.message(), outside(&t.cp), "{what}");
    }
}

#[tokio::test]
async fn every_range_outside_the_environment_is_refused() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let global = t.cp.env().global_prefix();
    let other = format!("/aisix/{}/", Uuid::new_v4());
    let mine = format!("{env}models/m-{}", Uuid::new_v4());
    t.put(&mine, "v1").await;
    t.put(&format!("{other}models/x"), "another-env-7c1").await;
    let bare = env.trim_end_matches('/').as_bytes().to_vec();
    let sibling = format!("{}-sibling-7c1/", env.trim_end_matches('/'));
    let mut past = prefix_end(env.as_bytes());
    past.push(0);
    let cases: Vec<(&str, Vec<u8>, Vec<u8>)> = vec![
        (
            "another environment",
            other.clone().into(),
            prefix_end(other.as_bytes()),
        ),
        (
            "a single key of another environment",
            format!("{other}models/x").into(),
            Vec::new(),
        ),
        ("the bare prefix", bare.clone(), prefix_end(&bare)),
        ("the bare prefix as one key", bare.clone(), Vec::new()),
        (
            "a sibling",
            sibling.clone().into(),
            prefix_end(sibling.as_bytes()),
        ),
        ("from a key", env.clone().into(), vec![0]),
        ("all keys", vec![0], vec![0]),
        ("an empty key", Vec::new(), prefix_end(env.as_bytes())),
        ("one byte past the environment", env.clone().into(), past),
        (
            "the environment through the global prefix",
            env.clone().into(),
            prefix_end(global.as_bytes()),
        ),
        (
            "the whole root",
            b"/aisix/".to_vec(),
            prefix_end(b"/aisix/"),
        ),
    ];
    let mut gw = gateway(&t.cp, &t.bundle).await;
    for (what, key, end) in cases {
        for options in [
            GetOptions::new(),
            GetOptions::new().with_count_only(),
            GetOptions::new().with_keys_only(),
        ] {
            let options = if end.is_empty() {
                options
            } else {
                options.with_range(end.clone())
            };
            let status = status_of(gw.get(key.clone(), Some(options)).await);
            assert_eq!(status.code(), Code::PermissionDenied, "{what}: {status:?}");
            assert_eq!(status.message(), outside(&t.cp), "{what}");
        }
    }

    // What is inside is served, on the same connection.
    let all = gw
        .get(env.clone(), Some(GetOptions::new().with_prefix()))
        .await
        .expect("the environment");
    assert_eq!(keys_of(all.kvs()), vec![mine.clone()]);
    let one = gw.get(mine.clone(), None).await.expect("one key");
    assert_eq!(keys_of(one.kvs()), vec![mine.clone()]);
    let models = format!("{env}models/");
    let part = gw
        .get(
            models.clone(),
            Some(GetOptions::new().with_range(prefix_end(models.as_bytes()))),
        )
        .await
        .expect("a range inside the environment");
    assert_eq!(keys_of(part.kvs()), vec![mine]);
    gw.get(global, Some(GetOptions::new().with_prefix()))
        .await
        .expect("the global prefix");
}

#[tokio::test]
async fn old_revisions_counts_and_replays_through_the_front_stay_in_the_environment() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let key = format!("{env}models/m-{}", Uuid::new_v4());
    let other = format!("/aisix/{}/models/x", Uuid::new_v4());
    let first = t.put(&key, "v1").await;
    t.put(&other, "another-env-7c1").await;
    t.put(&key, "v2").await;
    let mut gw = gateway(&t.cp, &t.bundle).await;

    let old = gw
        .get(
            env.clone(),
            Some(GetOptions::new().with_prefix().with_revision(first)),
        )
        .await
        .expect("the environment at an old revision");
    assert_eq!(keys_of(old.kvs()), vec![key.clone()]);
    assert_eq!(old.kvs()[0].value(), b"v1");
    let counted = gw
        .get(
            env.clone(),
            Some(GetOptions::new().with_prefix().with_count_only()),
        )
        .await
        .expect("a count");
    assert_eq!(counted.count(), 1);

    // A replay from the first write, with the previous values and
    // fragmenting asked for: this environment's history only.
    let (_watcher, mut stream) = gw
        .watch(
            env.clone(),
            Some(
                WatchOptions::new()
                    .with_prefix()
                    .with_start_revision(first)
                    .with_prev_key()
                    .with_fragment(),
            ),
        )
        .await
        .expect("a replay through the front");
    let mut seen = Vec::new();
    while seen.len() < 2 {
        for event in next_events(&mut stream).await.events() {
            let kv = event.kv().expect("a kv");
            let previous = event
                .prev_kv()
                .map(|p| (p.key_str().expect("UTF-8").to_string(), p.value().to_vec()));
            seen.push((
                kv.key_str().expect("UTF-8").to_string(),
                kv.value().to_vec(),
                previous,
            ));
        }
    }
    assert_eq!(
        seen,
        vec![
            (key.clone(), b"v1".to_vec(), None),
            (
                key.clone(),
                b"v2".to_vec(),
                Some((key.clone(), b"v1".to_vec()))
            ),
        ]
    );
    // A later write elsewhere, then one here: only this one arrives.
    t.put(&other, "another-env-again-7c1").await;
    t.put(&key, "v3").await;
    let next = next_events(&mut stream).await;
    let event = &next.events()[0];
    assert_eq!(next.events().len(), 1);
    assert_eq!(event.event_type(), EventType::Put);
    assert_eq!(event.kv().expect("a kv").key_str().expect("UTF-8"), key);
}

#[tokio::test]
async fn put_txn_and_compact_through_the_front_are_permission_denied_and_write_nothing() {
    let Some(kine) = PrivateKine::start().await else {
        return;
    };
    let Some(t) = Fixture::on(&kine).await else {
        return;
    };
    let env = t.env();
    let key = format!("{env}models/m");
    let first = t.put(&key, "v1").await;
    let current = t.put(&key, "v2").await;
    let fresh = format!("{env}models/written-through-the-front");
    let mut gw = gateway(&t.cp, &t.bundle).await;

    assert_denied(gw.put(fresh.clone(), "x", None).await, READ_ONLY);
    assert_denied(
        gw.txn(Txn::new().and_then([TxnOp::put(fresh.clone(), "x", None)]))
            .await,
        READ_ONLY,
    );
    assert_denied(gw.delete(key.clone(), None).await, READ_ONLY);
    assert_denied(gw.compact(current, None).await, READ_ONLY);

    // Nothing was written, deleted or compacted.
    let mut direct = t.kine.clone();
    let absent = direct.get(fresh, None).await.expect("read kine");
    assert!(absent.kvs().is_empty());
    let now = direct.get(key.clone(), None).await.expect("read kine");
    assert_eq!(now.kvs()[0].value(), b"v2");
    let old = direct
        .get(key.clone(), Some(GetOptions::new().with_revision(first)))
        .await
        .expect("the first revision is still there: nothing was compacted");
    assert_eq!(old.kvs()[0].value(), b"v1");
    // A refused write does not cost the gateway its reads.
    let read = gw.get(key, None).await.expect("a read after the refusals");
    assert_eq!(read.kvs()[0].value(), b"v2");
}

#[tokio::test]
async fn authenticate_through_the_front_is_unimplemented() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    // A gateway with `etcd.user` set authenticates as it connects.
    let with_user = gateway_options(&t.bundle).with_user("root", "pw-7c1");
    let refused = Client::connect([front(&t.cp)], Some(with_user)).await;
    let status = status_of(refused.map(|_| ()));
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");

    // Nor are leases, the cluster or maintenance served.
    let mut gw = gateway(&t.cp, &t.bundle).await;
    let status = status_of(gw.lease_grant(10, None).await);
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");
    let status = status_of(gw.status().await);
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");
    let status = status_of(gw.member_list().await);
    assert_eq!(status.code(), Code::Unimplemented, "{status:?}");
    // The KV service still is.
    gw.get(t.env(), Some(GetOptions::new().with_prefix()))
        .await
        .expect("a read");
}

fn der_of(pem: &str) -> Vec<u8> {
    parse_x509_pem(pem.as_bytes())
        .expect("a PEM certificate")
        .1
        .contents
}

/// One TLS connection to `addr` as `bundle`, offering `alpn`.
async fn tls_connect(
    addr: SocketAddr,
    bundle: &Bundle,
    alpn: &[&[u8]],
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(der_of(&bundle.ca_pem)))
        .expect("the CA as a root");
    let key = KeyPair::from_pem(&bundle.key_pem).expect("a PKCS#8 key");
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_client_auth_cert(
            vec![CertificateDer::from(der_of(&bundle.cert_pem))],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("a client identity");
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").expect("a name"), tcp)
        .await
        .expect("the mTLS handshake")
}

/// A connection that says when it is gone: `gone` fires when the client
/// drops it, which it does once the server has closed it.
struct Tracked {
    io: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    gone: Option<oneshot::Sender<()>>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let Some(gone) = self.gone.take() {
            let _ = gone.send(());
        }
    }
}

impl AsyncRead for Tracked {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Tracked {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// A gRPC channel over exactly one h2 connection to the front as `bundle`,
/// and what fires once that connection is gone. The channel never dials
/// again.
async fn one_connection(
    cp: &TestCp,
    bundle: &Bundle,
) -> (tonic::transport::Channel, oneshot::Receiver<()>) {
    one_connection_to(cp.dp_addr(), bundle).await
}

/// [`one_connection`] to the listener at `addr`.
async fn one_connection_to(
    addr: SocketAddr,
    bundle: &Bundle,
) -> (tonic::transport::Channel, oneshot::Receiver<()>) {
    let tls = tls_connect(addr, bundle, &[b"h2"]).await;
    let (gone, closed) = oneshot::channel();
    let slot = Arc::new(Mutex::new(Some(Tracked {
        io: tls,
        gone: Some(gone),
    })));
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let io = slot.lock().expect("the slot").take();
            async move {
                io.map(TokioIo::new)
                    .ok_or_else(|| std::io::Error::other("one connection only"))
            }
        }))
        .await
        .expect("an h2 connection");
    (channel, closed)
}

fn create(prefix: &str) -> WatchRequest {
    WatchRequest {
        request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest {
                key: prefix.as_bytes().to_vec(),
                range_end: prefix_end(prefix.as_bytes()),
                ..Default::default()
            },
        )),
    }
}

/// A watch on `prefix` over `channel`: the request sender (kept open) and
/// the responses, after the create was confirmed.
async fn raw_watch(
    channel: tonic::transport::Channel,
    prefix: &str,
) -> (
    mpsc::Sender<WatchRequest>,
    tonic::Streaming<aisix_control_plane::etcd_front::proto::etcdserverpb::WatchResponse>,
) {
    let (requests, inbound) = mpsc::channel(4);
    requests
        .send(create(prefix))
        .await
        .expect("queue the create");
    let mut responses = RawWatch::new(channel)
        .watch(ReceiverStream::new(inbound))
        .await
        .expect("the watch call")
        .into_inner();
    let created = tokio::time::timeout(Duration::from_secs(5), responses.message())
        .await
        .expect("created within 5 s")
        .expect("created")
        .expect("a response");
    assert!(created.created, "{created:?}");
    (requests, responses)
}

/// How a watch call ends, skipping any responses before that, within `wait`.
async fn ending(
    responses: &mut tonic::Streaming<
        aisix_control_plane::etcd_front::proto::etcdserverpb::WatchResponse,
    >,
    wait: Duration,
) -> Result<(), tonic::Status> {
    tokio::time::timeout(wait, async {
        loop {
            match responses.message().await {
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(()),
                Err(status) => return Err(status),
            }
        }
    })
    .await
    .expect("the watch ended in time")
}

#[tokio::test]
async fn a_revoked_credential_loses_its_watch() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let key = format!("{env}models/m-{}", Uuid::new_v4());
    t.put(&key, "v1").await;
    // Another gateway, whose watch must outlive the revoke.
    let other = t.cp.issue_bundle(&t.admin, "gw-front-other-7c1").await;
    let mut bystander = gateway(&t.cp, &other).await;
    let (_kept, mut kept) = bystander
        .watch(env.clone(), Some(WatchOptions::new().with_prefix()))
        .await
        .expect("the other gateway's watch");

    // The revoked gateway holds a watch on one connection and reads on
    // another, both open across the revoke.
    let (watch_conn, watch_gone) = one_connection(&t.cp, &t.bundle).await;
    let (_requests, mut responses) = raw_watch(watch_conn, &env).await;
    let (range_conn, range_gone) = one_connection(&t.cp, &t.bundle).await;
    let mut kv = RawKv::new(range_conn);
    let one_key = || RangeRequest {
        key: key.as_bytes().to_vec(),
        ..Default::default()
    };
    kv.range(one_key()).await.expect("a read before the revoke");

    t.revoke(&t.bundle).await;
    // Within 5 s, well inside the 10 s re-check: the notification ended it.
    let ended = ending(&mut responses, Duration::from_secs(5)).await;
    let status = ended.expect_err("the watch ends refused");
    assert_eq!(status.code(), Code::PermissionDenied, "{status:?}");
    tokio::time::timeout(Duration::from_secs(2), watch_gone)
        .await
        .expect("the watch's connection is closed within 2 s")
        .expect("closed");

    // The reading connection is refused at its next read, then closed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let refused = loop {
        match kv.range(one_key()).await {
            Ok(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "still served 5 s after the revoke"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(status) => break status,
        }
    };
    assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");
    tokio::time::timeout(Duration::from_secs(2), range_gone)
        .await
        .expect("the reading connection is closed within 2 s")
        .expect("closed");

    // A new connection with the revoked certificate is refused as well.
    let mut again = gateway(&t.cp, &t.bundle).await;
    let status = status_of(again.get(key.clone(), None).await);
    assert_eq!(status.code(), Code::PermissionDenied, "{status:?}");
    let status = status_of(
        again
            .watch(env.clone(), Some(WatchOptions::new().with_prefix()))
            .await
            .map(|(w, _)| w.watch_id()),
    );
    assert_eq!(status.code(), Code::PermissionDenied, "{status:?}");

    // The other gateway's watch never noticed.
    t.put(&key, "v2").await;
    let seen = next_events(&mut kept).await;
    let kv = seen.events()[0].kv().expect("a kv");
    assert_eq!((kv.key(), kv.value()), (key.as_bytes(), &b"v2"[..]));
}

#[tokio::test]
async fn a_watch_ends_when_its_credential_expires() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let (conn, gone) = one_connection(&t.cp, &t.bundle).await;
    let (_requests, mut responses) = raw_watch(conn, &env).await;
    // The credential now runs out in 2 s; the notification carries the new
    // expiry, and no notification marks the moment it passes.
    sqlx::query(
        "UPDATE dp_credentials SET not_after = now() + interval '2 seconds' WHERE dp_id = $1",
    )
    .bind(Uuid::parse_str(&t.bundle.dp_id).expect("a dp_id"))
    .execute(t.cp.db().pool())
    .await
    .expect("shorten the credential");
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CREDENTIALS_CHANNEL)
        .bind(&t.bundle.dp_id)
        .execute(t.cp.db().pool())
        .await
        .expect("notify");
    // Still served before it runs out.
    let key = format!("{env}models/m-{}", Uuid::new_v4());
    t.put(&key, "v1").await;
    let seen = tokio::time::timeout(Duration::from_secs(1), responses.message())
        .await
        .expect("an event within 1 s")
        .expect("the watch is open")
        .expect("a response");
    assert_eq!(seen.events.len(), 1, "{seen:?}");
    // Ended by the periodic re-check (every 10 s) once it has run out.
    let status = ending(&mut responses, Duration::from_secs(15))
        .await
        .expect_err("the watch ends refused");
    assert_eq!(status.code(), Code::PermissionDenied, "{status:?}");
    tokio::time::timeout(Duration::from_secs(2), gone)
        .await
        .expect("the connection is closed within 2 s")
        .expect("closed");
}

#[tokio::test]
async fn a_client_without_a_certificate_cannot_connect() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let key = format!("{}models/m", t.env());
    t.put(&key, "v1").await;
    let anonymous = ConnectOptions::new()
        .with_keep_alive_while_idle(false)
        .with_connect_timeout(Duration::from_secs(5))
        .with_tls(
            TlsOptions::new()
                .domain_name("localhost")
                .ca_certificate(Certificate::from_pem(t.bundle.ca_pem.clone())),
        );
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        let mut client = Client::connect([front(&t.cp)], Some(anonymous)).await?;
        client.get(key, None).await
    })
    .await
    .expect("decided within 10 s");
    assert!(read.is_err(), "read without a certificate: {read:?}");
}

#[tokio::test]
async fn a_certificate_for_another_environment_is_refused() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    // This control plane's CA, signing for another environment: the
    // certificate chains and is listed, and names another env.
    let other = EnvSettings {
        env_id: Uuid::new_v4(),
        etcd_prefix: t.cp.env().etcd_prefix.clone(),
    };
    let pki = Pki::load_or_init(t.cp.db(), t.cp.master_key(), &other)
        .await
        .expect("the stored CA");
    let dp_id = Uuid::new_v4();
    let issued = pki.issue_client(dp_id, 30).expect("a certificate");
    sqlx::query(
        "INSERT INTO dp_credentials \
         (dp_id, name, serial, fingerprint_sha256, not_before, not_after) \
         VALUES ($1, 'gw-other-env-7c1', $2, $3, $4, $5)",
    )
    .bind(dp_id)
    .bind(&issued.serial)
    .bind(&issued.fingerprint_sha256)
    .bind(issued.not_before)
    .bind(issued.not_after)
    .execute(t.cp.db().pool())
    .await
    .expect("list the credential");
    let foreign = Bundle {
        dp_id: dp_id.to_string(),
        cert_pem: issued.cert_pem,
        key_pem: issued.key_pem,
        ca_pem: t.bundle.ca_pem.clone(),
    };
    t.put(&format!("{}models/m", other.env_prefix()), "its-own-7c1")
        .await;
    for prefix in [other.env_prefix(), t.env()] {
        let mut gw = gateway(&t.cp, &foreign).await;
        let status = status_of(
            gw.get(prefix.clone(), Some(GetOptions::new().with_prefix()))
                .await,
        );
        assert_eq!(
            status.code(),
            Code::PermissionDenied,
            "{prefix}: {status:?}"
        );
        let mut gw = gateway(&t.cp, &foreign).await;
        let status = status_of(
            gw.watch(prefix.clone(), Some(WatchOptions::new().with_prefix()))
                .await
                .map(|(w, _)| w.watch_id()),
        );
        assert_eq!(
            status.code(),
            Code::PermissionDenied,
            "{prefix}: {status:?}"
        );
    }
}

#[tokio::test]
async fn a_watch_from_a_compacted_revision_is_cancelled_with_its_compact_revision() {
    let Some(kine) = PrivateKine::start().await else {
        return;
    };
    let Some(t) = Fixture::on(&kine).await else {
        return;
    };
    let env = t.env();
    let key = format!("{env}models/m");
    let mut revisions = Vec::new();
    for i in 0..5 {
        revisions.push(t.put(&key, &format!("v{i}")).await);
    }
    let compacted = revisions[3];
    t.kine
        .clone()
        .compact(compacted, None)
        .await
        .expect("compact kine directly");

    let mut gw = gateway(&t.cp, &t.bundle).await;
    let (watcher, mut stream) = gw
        .watch(
            env.clone(),
            Some(WatchOptions::new().with_prefix().with_start_revision(1)),
        )
        .await
        .expect("created");
    assert_ne!(watcher.watch_id(), -1);
    let cancelled = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("answered within 5 s")
        .expect("a response, not an error")
        .expect("the call is open");
    assert!(cancelled.canceled(), "{cancelled:?}");
    assert_eq!(cancelled.compact_revision(), compacted);
    assert!(
        cancelled
            .cancel_reason()
            .contains("required revision has been compacted"),
        "{cancelled:?}"
    );

    // The gateway's own watch takes it as a compaction, to resync from.
    let provider = provider(&t.cp, &t.bundle, &env).await;
    let mut events = provider.watch(1).await.expect("created");
    assert!(
        matches!(
            next_event(&mut *events).await,
            Err(ProviderError::Compacted)
        ),
        "not a compaction"
    );
    // And a read at a compacted revision says so.
    let status = status_of(
        gw.get(key, Some(GetOptions::new().with_revision(revisions[0])))
            .await,
    );
    assert_eq!(status.code(), Code::OutOfRange, "{status:?}");
    assert!(status.message().contains("compacted"), "{status:?}");
}

#[tokio::test]
async fn rest_and_grpc_share_one_port() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let key = format!("{}models/m", t.env());
    t.put(&key, "v1").await;
    let beat =
        t.cp.dp_client(&t.bundle)
            .heartbeat(json!({"dp_id": t.bundle.dp_id, "instance_id": "i-front-7c1"}))
            .await;
    assert_eq!(beat.status, 200, "{beat:?}");
    let mut gw = gateway(&t.cp, &t.bundle).await;
    let read = gw
        .get(key.clone(), None)
        .await
        .expect("a read on the same port");
    assert_eq!(keys_of(read.kvs()), vec![key]);
    // A client offering both gets h2 (the etcd front); one offering only
    // HTTP/1.1, as the gateway's REST clients do, gets that.
    let both = tls_connect(t.cp.dp_addr(), &t.bundle, &[b"h2", b"http/1.1"]).await;
    assert_eq!(both.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let rest = tls_connect(t.cp.dp_addr(), &t.bundle, &[b"http/1.1"]).await;
    assert_eq!(rest.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
}

#[tokio::test]
async fn a_revoked_gateway_that_stops_reading_still_loses_its_connection() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let (conn, gone) = one_connection(&t.cp, &t.bundle).await;
    // The watch is open and never read again: more than the stream's flow
    // control window is written to it, so its refusal cannot get through.
    let (_requests, _unread) = raw_watch(conn, &env).await;
    let value = "x".repeat(64 * 1024);
    for i in 0..96 {
        t.put(&format!("{env}models/big-{i:03}"), &value).await;
    }
    t.revoke(&t.bundle).await;
    // Within the 5 s of the notification plus the 5 s drain deadline.
    tokio::time::timeout(Duration::from_secs(12), gone)
        .await
        .expect("the connection is dropped within 12 s of the revoke")
        .expect("closed");
}

#[tokio::test]
async fn shutting_down_ends_open_watches_unavailable_and_promptly() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let endpoint = support::gate_var("CP_TEST_KINE_ENDPOINT").expect("the kine endpoint");
    let tls = listener::server_tls(&t.cp.state().pki, &["localhost".to_string()], true)
        .expect("the listener's TLS");
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = tcp.local_addr().expect("an address");
    let front = Front::new(
        etcd_front::kine_endpoint(&endpoint).expect("kine's endpoint"),
        t.cp.state().creds.clone(),
        t.cp.env().clone(),
    );
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let served = tokio::spawn(listener::serve_dp(
        tcp,
        tls,
        aisix_control_plane::http::dp_router(t.cp.state().clone()),
        Some(etcd_front::routes(front)),
        ConnLimits::default(),
        stopped,
    ));
    let (conn, gone) = one_connection_to(addr, &t.bundle).await;
    let (_requests, mut responses) = raw_watch(conn, &t.env()).await;

    stop.send(true).expect("signal shutdown");
    let status = ending(&mut responses, Duration::from_secs(1))
        .await
        .expect_err("the watch ends with a status");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    tokio::time::timeout(Duration::from_secs(2), served)
        .await
        .expect("the listener stops within 2 s, not at the drain deadline")
        .expect("the listener task");
    tokio::time::timeout(Duration::from_secs(1), gone)
        .await
        .expect("the connection is closed")
        .expect("closed");
}

#[tokio::test]
async fn a_create_on_an_open_watch_is_refused_once_its_credential_has_run_out() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    let (conn, _gone) = one_connection(&t.cp, &t.bundle).await;
    let (requests, mut responses) = raw_watch(conn, &env).await;
    sqlx::query(
        "UPDATE dp_credentials SET not_after = now() + interval '1 second' WHERE dp_id = $1",
    )
    .bind(Uuid::parse_str(&t.bundle.dp_id).expect("a dp_id"))
    .execute(t.cp.db().pool())
    .await
    .expect("shorten the credential");
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CREDENTIALS_CHANNEL)
        .bind(&t.bundle.dp_id)
        .execute(t.cp.db().pool())
        .await
        .expect("notify");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // Long before the watch's next periodic check, a new create on it is
    // checked, and refused.
    requests
        .send(create(&format!("{env}models/")))
        .await
        .expect("send a second create");
    let status = ending(&mut responses, Duration::from_secs(2))
        .await
        .expect_err("refused");
    assert_eq!(status.code(), Code::PermissionDenied, "{status:?}");
}

#[tokio::test]
async fn answers_over_four_mib_pass_through_the_front() {
    let Some(t) = Fixture::start().await else {
        return;
    };
    let env = t.env();
    // Four values of 1.2 MiB: one page, and one replayed watch answer, of
    // about 5 MiB, past tonic's 4 MiB default.
    let value = "v".repeat(1200 * 1024);
    let mut first = 0;
    for i in 0..4 {
        let revision = t.put(&format!("{env}models/large-{i}"), &value).await;
        if first == 0 {
            first = revision;
        }
    }
    let provider = provider(&t.cp, &t.bundle, &env).await;
    let (entries, _) = provider.load_all().await.expect("the large load");
    assert_eq!(entries.len(), 4);
    assert!(entries.iter().all(|e| e.value.len() == value.len()));
    let mut events = provider.watch(first).await.expect("the replay");
    for _ in 0..4 {
        match next_event(&mut *events).await {
            Ok(WatchEvent::Put(entry)) => assert_eq!(entry.value.len(), value.len()),
            other => panic!("expected a large put, got {other:?}"),
        }
    }
}
