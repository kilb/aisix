//! The projector: stored gateway resources reach kine under the
//! environment's prefix with their credentials in plaintext, in commit
//! order, from exactly one writer; and reconcile never deletes much without
//! an admin's say-so.
//!
//! Most cases run against the real test kine (`CP_TEST_KINE_ENDPOINT`):
//! every control plane mints a fresh `env_id`, so its prefix is its own. Two
//! cases need a kine that fails on cue, which real kine cannot stage; they
//! use [`MemKine`]. Kine is read back with `etcd_client` directly, not with
//! the control plane's own client.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::Db;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::kine::{KeyValue, KineClient, KineError, KineWriter, WriteOutcome};
use aisix_control_plane::projector::{self, key_for, PROJECTOR_LOCK_KEY};
use aisix_control_plane::resources::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use aisix_control_plane::resources::Kind;
use aisix_control_plane::settings::EnvSettings;
use async_trait::async_trait;
use etcd_client::{Client, Compare, CompareOp, EventType, GetOptions, Txn, TxnOp, WatchOptions};
use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::{BareDatabase, Resp, TestCp, TEST_MASTER_KEY};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Every credential the tests write, and its rotated value: distinctive, so
/// "never stored in plaintext" cannot pass by accident.
const SECRET: &str = "sk-proj-secret-5e1b";
const ROTATED: &str = "sk-proj-rotated-77d0";

/// Long enough for a projector that is backing off at its 10 s ceiling.
const PATIENCE: Duration = Duration::from_secs(30);

fn ctx(env: &EnvSettings) -> WriteCtx {
    WriteCtx {
        mk: MasterKey(TEST_MASTER_KEY),
        env: env.clone(),
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..8])
}

fn provider_key(name: &str, api_key: &str) -> Value {
    json!({"display_name": name, "provider": "openai", "api_key": api_key})
}

enum Op {
    Create(Value),
    Replace(Value),
    Delete,
}

/// Runs `writes` through `write_resource`, all in one transaction.
async fn write_all(db: &Db, env: &EnvSettings, writes: Vec<(Kind, Uuid, Op)>) {
    let ctx = ctx(env);
    db.tx(move |conn| {
        async move {
            for (kind, id, op) in &writes {
                let op = match op {
                    Op::Create(doc) => WriteOp::Create(doc),
                    Op::Replace(doc) => WriteOp::Replace(doc),
                    Op::Delete => WriteOp::Delete,
                };
                let write = ResourceWrite {
                    kind: *kind,
                    id: *id,
                    op,
                    expected_version: None,
                    managed_by: None,
                };
                write_resource(conn, &ctx, write).await?;
            }
            Ok::<_, ApiError>(())
        }
        .boxed()
    })
    .await
    .expect("the writes are accepted");
}

/// Creates a provider key named `name`, in a transaction of its own.
async fn create_key(db: &Db, env: &EnvSettings, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let doc = provider_key(name, SECRET);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Create(doc))]).await;
    id
}

#[derive(Debug)]
struct RowState {
    version: i64,
    projected_version: Option<i64>,
    kine_mod_revision: Option<i64>,
    kine_revision: Option<i64>,
    seq: i64,
}

impl RowState {
    fn projected(&self) -> bool {
        self.projected_version == Some(self.version)
    }
}

async fn row_state(db: &Db, kind: Kind, id: Uuid) -> RowState {
    let (version, projected_version, kine_mod_revision, kine_revision, seq) = sqlx::query_as(
        "SELECT version, projected_version, kine_mod_revision, kine_revision, seq \
         FROM gw_resources WHERE kind = $1 AND id = $2",
    )
    .bind(kind.segment())
    .bind(id)
    .fetch_one(db.pool())
    .await
    .expect("load the row");
    RowState {
        version,
        projected_version,
        kine_mod_revision,
        kine_revision,
        seq,
    }
}

async fn dirty_rows(db: &Db) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources WHERE projected_version IS DISTINCT FROM version",
    )
    .fetch_one(db.pool())
    .await
    .expect("count dirty rows")
}

async fn setting(db: &Db, key: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT value FROM cp_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(db.pool())
        .await
        .expect("read a setting")
}

/// Polls `check` every 50 ms until it holds; fails after `within`.
async fn eventually<F, Fut>(what: &str, within: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    loop {
        if check().await {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn all_projected(db: &Db) {
    eventually("every row to be projected", PATIENCE, || async move {
        dirty_rows(db).await == 0
    })
    .await;
}

// ---- kine, read and written directly -------------------------------------

async fn raw_kine(endpoint: &str) -> Client {
    Client::connect([endpoint], None)
        .await
        .expect("connect to kine")
}

/// The value of `key` and its mod revision.
async fn raw_get(kine: &Client, key: &str) -> Option<(Vec<u8>, i64)> {
    let resp = kine
        .kv_client()
        .get(key, None)
        .await
        .expect("read a key from kine");
    resp.kvs()
        .first()
        .map(|kv| (kv.value().to_vec(), kv.mod_revision()))
}

async fn raw_json(kine: &Client, key: &str) -> Option<(Value, i64)> {
    raw_get(kine, key).await.map(|(bytes, revision)| {
        let value = serde_json::from_slice(&bytes).expect("kine holds JSON");
        (value, revision)
    })
}

async fn raw_keys(kine: &Client, prefix: &str) -> BTreeSet<String> {
    let resp = kine
        .kv_client()
        .get(
            prefix,
            Some(GetOptions::new().with_prefix().with_keys_only()),
        )
        .await
        .expect("list kine keys");
    resp.kvs()
        .iter()
        .map(|kv| String::from_utf8_lossy(kv.key()).into_owned())
        .collect()
}

async fn raw_put(kine: &Client, key: &str, value: &[u8]) {
    kine.kv_client()
        .put(key, value, None)
        .await
        .expect("write a key to kine");
}

/// Deletes `key`, which must exist, at its current revision: kine has no
/// DeleteRange, and deletes only at a revision it is compared with.
async fn raw_delete(kine: &Client, key: &str) {
    let (_, revision) = raw_get(kine, key).await.expect("the key to delete exists");
    let txn = Txn::new()
        .when(vec![Compare::mod_revision(key, CompareOp::Equal, revision)])
        .and_then(vec![TxnOp::delete(key, None)])
        .or_else(vec![TxnOp::get(key, None)]);
    let resp = kine
        .kv_client()
        .txn(txn)
        .await
        .expect("delete a key from kine");
    assert!(resp.succeeded(), "{key} changed while it was being deleted");
    assert_eq!(raw_get(kine, key).await, None, "{key} is still in kine");
}

// ---- the API -------------------------------------------------------------

async fn api_create(cp: &TestCp, cookie: &str, kind: &str, doc: Value) -> Uuid {
    let r = cp
        .send(
            "POST",
            &format!("/admin/resources/{kind}"),
            Some(cookie),
            Some(json!({ "doc": doc })),
        )
        .await;
    assert_eq!(r.status, 201, "create {kind}: {r:?}");
    r.body["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .expect("an id")
}

async fn reconcile_api(cp: &TestCp, cookie: &str, allow_deletes: bool) -> Resp {
    cp.send(
        "POST",
        "/admin/rollout/reconcile",
        Some(cookie),
        Some(json!({ "allow_deletes": allow_deletes })),
    )
    .await
}

// ---- an in-memory kine that fails on cue ---------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The write is refused and not applied.
    Refuse,
    /// The write is applied, and its answer is lost.
    LoseAck,
}

#[derive(Default)]
struct Mem {
    keys: BTreeMap<String, KeyValue>,
    revision: i64,
    writes: usize,
    fault: Option<(usize, Fault)>,
    /// Every write attempted: the operation, the key, and the revision it
    /// was applied at (`None` when it was not).
    log: Vec<(&'static str, String, Option<i64>)>,
}

/// A kine of one process's memory, with kine's compare-and-swap rules.
#[derive(Default)]
struct MemKine(Mutex<Mem>);

impl MemKine {
    /// The `nth` write from now (1 for the next) fails with `fault`.
    fn fail(&self, nth: usize, fault: Fault) {
        let mut m = self.0.lock().expect("lock");
        m.fault = Some((m.writes + nth, fault));
    }

    fn log(&self) -> Vec<(&'static str, String, Option<i64>)> {
        self.0.lock().expect("lock").log.clone()
    }

    fn applied(&self) -> usize {
        self.log().iter().filter(|(_, _, r)| r.is_some()).count()
    }

    fn value(&self, key: &str) -> Option<KeyValue> {
        self.0.lock().expect("lock").keys.get(key).cloned()
    }

    fn write(
        &self,
        op: &'static str,
        key: &str,
        apply: impl FnOnce(&mut Mem) -> WriteOutcome,
    ) -> Result<WriteOutcome, KineError> {
        let mut m = self.0.lock().expect("lock");
        m.writes += 1;
        let writes = m.writes;
        let fault = m.fault.filter(|(n, _)| *n == writes).map(|(_, f)| f);
        if fault == Some(Fault::Refuse) {
            m.log.push((op, key.to_string(), None));
            return Err(KineError::Unavailable("injected: refused".to_string()));
        }
        let outcome = apply(&mut m);
        let applied = match outcome {
            WriteOutcome::Applied { revision } => Some(revision),
            WriteOutcome::Conflict { .. } => None,
        };
        m.log.push((op, key.to_string(), applied));
        if fault == Some(Fault::LoseAck) {
            return Err(KineError::Unavailable(
                "injected: the answer was lost".to_string(),
            ));
        }
        Ok(outcome)
    }
}

impl Mem {
    fn put(&mut self, key: &str, value: &[u8]) -> WriteOutcome {
        self.revision += 1;
        let kv = KeyValue {
            value: value.to_vec(),
            mod_revision: self.revision,
        };
        self.keys.insert(key.to_string(), kv);
        WriteOutcome::Applied {
            revision: self.revision,
        }
    }
}

#[async_trait]
impl KineWriter for MemKine {
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError> {
        self.write("create", key, |m| match m.keys.get(key).cloned() {
            Some(kv) => WriteOutcome::Conflict { current: Some(kv) },
            None => m.put(key, value),
        })
    }

    async fn update(
        &self,
        key: &str,
        expected_mod_revision: i64,
        value: &[u8],
    ) -> Result<WriteOutcome, KineError> {
        self.write("update", key, |m| match m.keys.get(key).cloned() {
            Some(kv) if kv.mod_revision == expected_mod_revision => m.put(key, value),
            current => WriteOutcome::Conflict { current },
        })
    }

    async fn delete(
        &self,
        key: &str,
        expected_mod_revision: i64,
    ) -> Result<WriteOutcome, KineError> {
        self.write("delete", key, |m| match m.keys.get(key).cloned() {
            Some(kv) if kv.mod_revision == expected_mod_revision => {
                m.keys.remove(key);
                m.revision += 1;
                WriteOutcome::Applied {
                    revision: m.revision,
                }
            }
            current => WriteOutcome::Conflict { current },
        })
    }

    async fn get(&self, key: &str) -> Result<Option<KeyValue>, KineError> {
        Ok(self.value(key))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<(String, KeyValue)>, KineError> {
        let m = self.0.lock().expect("lock");
        Ok(m.keys
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, kv)| (k.clone(), kv.clone()))
            .collect())
    }
}

#[tokio::test]
async fn a_failure_does_not_skip_ahead() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let mut ids = Vec::new();
    for name in ["a", "b", "c"] {
        ids.push(create_key(db, env, &unique(name)).await);
    }
    let keys: Vec<String> = ids
        .iter()
        .map(|id| key_for(env, Kind::ProviderKeys, *id))
        .collect();
    let kine = MemKine::default();
    kine.fail(2, Fault::Refuse);

    let failed = projector::project_once(db, &kine, &ctx(env)).await;
    assert!(failed.is_err(), "{failed:?}");
    // The second row failed, so the third was never tried.
    let tried: Vec<String> = kine.log().into_iter().map(|(_, key, _)| key).collect();
    assert_eq!(tried, keys[..2].to_vec());
    assert!(kine.value(&keys[0]).is_some());
    assert!(kine.value(&keys[2]).is_none());
    assert!(row_state(db, Kind::ProviderKeys, ids[0]).await.projected());
    for id in &ids[1..] {
        assert!(!row_state(db, Kind::ProviderKeys, *id).await.projected());
    }

    // Once kine takes writes again, the rest lands in the order written.
    let projected = projector::project_once(db, &kine, &ctx(env))
        .await
        .expect("the rest is projected");
    assert_eq!(projected, 2);
    let (b, c) = (
        kine.value(&keys[1]).expect("b is in kine"),
        kine.value(&keys[2]).expect("c is in kine"),
    );
    assert!(b.mod_revision < c.mod_revision, "{b:?} {c:?}");
    for (id, kv) in [(ids[1], b), (ids[2], c)] {
        let row = row_state(db, Kind::ProviderKeys, id).await;
        assert!(row.projected(), "{row:?}");
        assert_eq!(row.kine_revision, Some(kv.mod_revision));
    }
    assert_eq!(dirty_rows(db).await, 0);
}

#[tokio::test]
async fn a_conflict_from_a_lost_acknowledgement_is_recorded_not_rewritten() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let name = unique("acct");
    let id = create_key(db, env, &name).await;
    let key = key_for(env, Kind::ProviderKeys, id);
    let kine = MemKine::default();

    // A create kine applied but whose answer never arrived.
    kine.fail(1, Fault::LoseAck);
    assert!(projector::project_once(db, &kine, &ctx(env)).await.is_err());
    assert!(!row_state(db, Kind::ProviderKeys, id).await.projected());
    let created = kine.value(&key).expect("kine applied the create");
    assert_eq!(
        projector::project_once(db, &kine, &ctx(env))
            .await
            .expect("projected"),
        1
    );
    let row = row_state(db, Kind::ProviderKeys, id).await;
    assert!(row.projected(), "{row:?}");
    assert_eq!(row.kine_mod_revision, Some(created.mod_revision));
    assert_eq!(row.kine_revision, Some(created.mod_revision));
    assert_eq!(kine.applied(), 1, "written twice: {:?}", kine.log());
    assert_eq!(kine.value(&key), Some(created));

    // The same for an update.
    let doc = provider_key(&name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Replace(doc))]).await;
    kine.fail(1, Fault::LoseAck);
    assert!(projector::project_once(db, &kine, &ctx(env)).await.is_err());
    let updated = kine.value(&key).expect("kine applied the update");
    projector::project_once(db, &kine, &ctx(env))
        .await
        .expect("projected");
    let row = row_state(db, Kind::ProviderKeys, id).await;
    assert!(row.projected(), "{row:?}");
    assert_eq!(row.kine_mod_revision, Some(updated.mod_revision));
    assert_eq!(row.kine_revision, Some(updated.mod_revision));
    assert_eq!(kine.applied(), 2, "written twice: {:?}", kine.log());

    // And for a delete.
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Delete)]).await;
    kine.fail(1, Fault::LoseAck);
    assert!(projector::project_once(db, &kine, &ctx(env)).await.is_err());
    assert!(kine.value(&key).is_none());
    projector::project_once(db, &kine, &ctx(env))
        .await
        .expect("projected");
    let row = row_state(db, Kind::ProviderKeys, id).await;
    assert!(row.projected(), "{row:?}");
    assert_eq!(row.kine_mod_revision, None);
    assert_eq!(kine.applied(), 3, "written twice: {:?}", kine.log());
}

#[tokio::test]
async fn a_created_resource_lands_under_the_env_prefix_with_its_credential_in_plaintext() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let name = unique("acct");
    let id = api_create(
        &cp,
        op.cookie(),
        "provider_keys",
        provider_key(&name, SECRET),
    )
    .await;
    all_projected(cp.db()).await;

    let key = format!("/aisix/{}/provider_keys/{id}", cp.env().env_id);
    assert_eq!(key_for(cp.env(), Kind::ProviderKeys, id), key);
    let (value, _) = raw_json(&kine, &key).await.expect("the key is in kine");
    // The gateway needs the credential itself.
    assert_eq!(value, provider_key(&name, SECRET));
    // Postgres keeps it sealed.
    let stored: String = sqlx::query_scalar("SELECT doc::text FROM gw_resources WHERE id = $1")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("load the stored document");
    assert!(!stored.contains(SECRET), "{stored}");
    assert!(stored.contains("aisixsealed.v1."), "{stored}");
}

#[tokio::test]
async fn the_recorded_revision_is_the_writes_revision() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let (db, env) = (cp.db(), cp.env());
    let name = unique("acct");
    let id = create_key(db, env, &name).await;
    all_projected(db).await;
    let key = key_for(env, Kind::ProviderKeys, id);
    let (_, created) = raw_get(&kine, &key).await.expect("the key is in kine");
    let row = row_state(db, Kind::ProviderKeys, id).await;
    assert_eq!(row.kine_revision, Some(created));
    assert_eq!(row.kine_mod_revision, Some(created));

    let doc = provider_key(&name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Replace(doc))]).await;
    all_projected(db).await;
    let (value, updated) = raw_json(&kine, &key).await.expect("the key is in kine");
    assert_eq!(value["api_key"], ROTATED);
    assert!(updated > created, "{updated} after {created}");
    let row = row_state(db, Kind::ProviderKeys, id).await;
    assert_eq!(row.version, 2);
    assert_eq!(row.kine_revision, Some(updated));
    assert_eq!(row.kine_mod_revision, Some(updated));
}

#[tokio::test]
async fn a_delete_removes_the_key_and_records_the_delete_revision() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let (db, env) = (cp.db(), cp.env());
    let id = create_key(db, env, &unique("acct")).await;
    all_projected(db).await;
    let key = key_for(env, Kind::ProviderKeys, id);
    let (_, created) = raw_get(&kine, &key).await.expect("the key is in kine");

    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Delete)]).await;
    all_projected(db).await;
    assert_eq!(raw_get(&kine, &key).await, None);
    let row = row_state(db, Kind::ProviderKeys, id).await;
    assert_eq!(row.kine_mod_revision, None);

    // The revision kine gave the delete, from the key's history.
    let (_watcher, mut events) = kine
        .watch_client()
        .watch(
            key.as_str(),
            Some(WatchOptions::new().with_start_revision(created + 1)),
        )
        .await
        .expect("watch the key's history");
    let deleted_at = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let message = events
                .message()
                .await
                .expect("a watch message")
                .expect("the watch stays open");
            if let Some(event) = message
                .events()
                .iter()
                .find(|e| e.event_type() == EventType::Delete)
            {
                return event.kv().map(|kv| kv.mod_revision());
            }
        }
    })
    .await
    .expect("the delete is in the key's history");
    assert_eq!(row.kine_revision, deleted_at);
    assert!(deleted_at > Some(created));
}

#[tokio::test]
async fn rows_are_projected_in_write_order() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let (db, env) = (cp.db(), cp.env());
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let a_name = unique("a");
    // One transaction, so the projector finds all of it dirty at once: `a`
    // was created first and rewritten last, so it is written last.
    write_all(
        db,
        env,
        vec![
            (
                Kind::ProviderKeys,
                a,
                Op::Create(provider_key(&a_name, SECRET)),
            ),
            (
                Kind::ProviderKeys,
                b,
                Op::Create(provider_key(&unique("b"), SECRET)),
            ),
            (
                Kind::ProviderKeys,
                c,
                Op::Create(provider_key(&unique("c"), SECRET)),
            ),
            (
                Kind::ProviderKeys,
                a,
                Op::Replace(provider_key(&a_name, ROTATED)),
            ),
        ],
    )
    .await;
    all_projected(db).await;

    let mut revisions = Vec::new();
    for id in [b, c, a] {
        let key = key_for(env, Kind::ProviderKeys, id);
        let (_, revision) = raw_get(&kine, &key).await.expect("the key is in kine");
        revisions.push(revision);
    }
    assert!(
        revisions.windows(2).all(|w| w[0] < w[1]),
        "b, c, a written at {revisions:?}"
    );
    let (b_seq, a_seq) = (
        row_state(db, Kind::ProviderKeys, b).await.seq,
        row_state(db, Kind::ProviderKeys, a).await.seq,
    );
    assert!(b_seq < a_seq, "the rewrite of a took a later seq");
}

/// A kine process of the test's own, on its own database, that the test can
/// stop and start again.
struct PrivateKine {
    bin: String,
    server_url: String,
    database: String,
    port: u16,
    child: Option<tokio::process::Child>,
}

impl PrivateKine {
    async fn start() -> Option<PrivateKine> {
        let server_url = support::database_server_url()?;
        let bin = support::kine_bin()?;
        let database = format!("kine_{}", Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(&server_url)
            .await
            .expect("connect to the test server");
        sqlx::query(&format!("CREATE DATABASE \"{database}\""))
            .execute(&mut admin)
            .await
            .expect("create kine's database");
        let _ = admin.close().await;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        let mut kine = PrivateKine {
            bin,
            server_url,
            database,
            port,
            child: None,
        };
        kine.resume().await;
        Some(kine)
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    async fn resume(&mut self) {
        let mut datastore = reqwest::Url::parse(&self.server_url).expect("a URL");
        datastore.set_path(&format!("/{}", self.database));
        datastore.set_query(Some("sslmode=disable"));
        let child = tokio::process::Command::new(&self.bin)
            .args([
                "--endpoint",
                datastore.as_str(),
                "--listen-address",
                &format!("127.0.0.1:{}", self.port),
                "--metrics-bind-address",
                "0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start kine");
        self.child = Some(child);
        let port = self.port;
        eventually("kine to listen", PATIENCE, || async move {
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
        })
        .await;
    }

    async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().await.expect("stop kine");
        }
    }
}

impl Drop for PrivateKine {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
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

#[tokio::test]
async fn writes_made_while_kine_is_down_land_in_order_when_it_returns() {
    let Some(mut private) = PrivateKine::start().await else {
        return;
    };
    let endpoint = private.endpoint();
    let Some(cp) = TestCp::start_with(|cfg| cfg.kine_endpoint = endpoint.clone()).await else {
        return;
    };
    let (db, env) = (cp.db(), cp.env());
    let a_name = unique("a");
    let a = create_key(db, env, &a_name).await;
    all_projected(db).await;

    private.stop().await;
    let b = create_key(db, env, &unique("b")).await;
    let c = create_key(db, env, &unique("c")).await;
    let doc = provider_key(&a_name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, a, Op::Replace(doc))]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(dirty_rows(db).await, 3, "nothing lands while kine is down");

    private.resume().await;
    all_projected(db).await;
    let kine = raw_kine(&endpoint).await;
    let mut revisions = Vec::new();
    for id in [b, c, a] {
        let key = key_for(env, Kind::ProviderKeys, id);
        let (value, revision) = raw_json(&kine, &key).await.expect("the key is in kine");
        if id == a {
            assert_eq!(value["api_key"], ROTATED);
        }
        revisions.push(revision);
    }
    assert!(
        revisions.windows(2).all(|w| w[0] < w[1]),
        "b, c, a written at {revisions:?}"
    );
}

/// One write a projector sent kine, and when.
#[derive(Debug, Clone)]
struct Write {
    by: &'static str,
    start: Instant,
    end: Instant,
}

/// The control plane's own kine client, logging every write it sends.
struct Recorder {
    by: &'static str,
    inner: KineClient,
    log: Arc<Mutex<Vec<Write>>>,
}

impl Recorder {
    async fn record<T>(&self, write: impl Future<Output = T>) -> T {
        let start = Instant::now();
        let result = write.await;
        self.log.lock().expect("lock").push(Write {
            by: self.by,
            start,
            end: Instant::now(),
        });
        result
    }
}

#[async_trait]
impl KineWriter for Recorder {
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError> {
        self.record(self.inner.create(key, value)).await
    }

    async fn update(&self, key: &str, rev: i64, value: &[u8]) -> Result<WriteOutcome, KineError> {
        self.record(self.inner.update(key, rev, value)).await
    }

    async fn delete(&self, key: &str, rev: i64) -> Result<WriteOutcome, KineError> {
        self.record(self.inner.delete(key, rev)).await
    }

    async fn get(&self, key: &str) -> Result<Option<KeyValue>, KineError> {
        self.inner.get(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<(String, KeyValue)>, KineError> {
        self.inner.list(prefix).await
    }
}

/// A projector on `bare`, as one control-plane instance would run it.
async fn spawn_recorded(
    by: &'static str,
    bare: &BareDatabase,
    endpoint: &str,
    log: &Arc<Mutex<Vec<Write>>>,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let kine = Recorder {
        by,
        inner: KineClient::connect(endpoint).await.expect("a kine client"),
        log: log.clone(),
    };
    let (stop, stopped) = oneshot::channel::<()>();
    let task = projector::spawn(
        bare.db.clone(),
        Arc::new(kine),
        ctx(&bare.env),
        async move {
            let _ = stopped.await;
        },
    );
    (stop, task)
}

/// The sessions holding the projector lock in `db`'s database.
async fn lock_holders(db: &Db) -> Vec<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
           AND classid = $1::bigint::oid AND objid = $2::bigint::oid AND objsubid = 1 \
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .bind(PROJECTOR_LOCK_KEY >> 32)
    .bind(PROJECTOR_LOCK_KEY & 0xffff_ffff)
    .fetch_all(db.pool())
    .await
    .expect("read pg_locks")
}

fn writers_since(log: &Arc<Mutex<Vec<Write>>>, since: Instant) -> BTreeSet<&'static str> {
    log.lock()
        .expect("lock")
        .iter()
        .filter(|w| w.start >= since)
        .map(|w| w.by)
        .collect()
}

#[tokio::test]
async fn only_one_instance_projects() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let log = Arc::new(Mutex::new(Vec::<Write>::new()));
    let began = Instant::now();
    let mut running = BTreeMap::new();
    for by in ["a", "b"] {
        running.insert(by, spawn_recorded(by, &bare, &endpoint, &log).await);
    }
    eventually("a projector to lead", PATIENCE, || async move {
        lock_holders(db).await.len() == 1
    })
    .await;
    // The other keeps trying, and keeps failing.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(lock_holders(db).await.len(), 1);

    for _ in 0..5 {
        create_key(db, env, &unique("one")).await;
    }
    all_projected(db).await;
    let first = writers_since(&log, began);
    assert_eq!(first.len(), 1, "writes by {first:?}");
    let leader = *first.iter().next().expect("a writer");

    // The leader stops; the other takes over.
    let (stop, task) = running.remove(leader).expect("the leader runs");
    let _ = stop.send(());
    task.await.expect("the leader stops");
    let handed_over = Instant::now();
    for _ in 0..3 {
        create_key(db, env, &unique("two")).await;
    }
    all_projected(db).await;
    let second = writers_since(&log, handed_over);
    let follower = *running.keys().next().expect("the other runs");
    assert_eq!(second, BTreeSet::from([follower]));

    // The new leader's session dies under it while a third instance waits:
    // whichever takes the lock next is then the only writer.
    running.insert("c", spawn_recorded("c", &bare, &endpoint, &log).await);
    let holders = lock_holders(db).await;
    assert_eq!(holders.len(), 1, "{holders:?}");
    let cut_pid = holders[0];
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(cut_pid)
        .execute(db.pool())
        .await
        .expect("terminate the leader's session");
    eventually(
        "another session to take the lock",
        PATIENCE,
        || async move {
            let holders = lock_holders(db).await;
            holders.len() == 1 && holders[0] != cut_pid
        },
    )
    .await;
    let cut = Instant::now();
    for _ in 0..3 {
        create_key(db, env, &unique("three")).await;
    }
    all_projected(db).await;
    let third = writers_since(&log, cut);
    assert_eq!(third.len(), 1, "writes by {third:?}");

    // At no point did two instances write at once.
    let log = log.lock().expect("lock").clone();
    for (i, w) in log.iter().enumerate() {
        for v in &log[i + 1..] {
            if w.by != v.by {
                assert!(w.end <= v.start || v.end <= w.start, "{w:?} overlaps {v:?}");
            }
        }
    }
    for (_, (stop, task)) in running {
        let _ = stop.send(());
        task.await.expect("a projector stops");
    }
}

#[tokio::test]
async fn reconcile_removes_a_few_strays_and_leaves_other_prefixes_alone() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let env = cp.env();
    let mut live = Vec::new();
    for _ in 0..3 {
        let id = api_create(
            &cp,
            admin.cookie(),
            "provider_keys",
            provider_key(&unique("acct"), SECRET),
        )
        .await;
        live.push(key_for(env, Kind::ProviderKeys, id));
    }
    all_projected(cp.db()).await;

    let prefix = env.env_prefix();
    let strays = [
        format!("{prefix}models/{}", Uuid::new_v4()),
        format!("{prefix}teams/{}", Uuid::new_v4()),
    ];
    let elsewhere = [
        format!("/aisix/{}/models/{}", Uuid::new_v4(), Uuid::new_v4()),
        // A sibling whose name starts with this environment's id.
        format!("/aisix/{}x/models/{}", env.env_id, Uuid::new_v4()),
        format!("/aisix/global/pricing/{}", Uuid::new_v4()),
    ];
    for key in strays.iter().chain(&elsewhere) {
        raw_put(&kine, key, b"{}").await;
    }

    let r = reconcile_api(&cp, admin.cookie(), false).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({"deleted_strays": 2, "marked_dirty": 0}));
    let left = raw_keys(&kine, &prefix).await;
    assert_eq!(left, live.iter().cloned().collect::<BTreeSet<_>>());
    for key in &elsewhere {
        assert!(raw_get(&kine, key).await.is_some(), "{key} was deleted");
        raw_delete(&kine, key).await;
    }
    assert_eq!(
        setting(cp.db(), "reconcile_blocked").await,
        Some(json!(false))
    );
}

#[tokio::test]
async fn reconcile_deletes_nothing_when_strays_exceed_the_threshold() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let (db, env) = (cp.db(), cp.env());
    let prefix = env.env_prefix();
    for _ in 0..2 {
        create_key(db, env, &unique("acct")).await;
    }
    all_projected(db).await;
    let put_strays = |n: usize| {
        let kine = &kine;
        let prefix = &prefix;
        async move {
            for _ in 0..n {
                raw_put(kine, &format!("{prefix}models/{}", Uuid::new_v4()), b"{}").await;
            }
        }
    };

    // 12 strays among 14 keys: more than 10, and more than 5% of the keys.
    put_strays(12).await;
    let r = reconcile_api(&cp, admin.cookie(), false).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({"deleted_strays": 0, "marked_dirty": 0}));
    assert_eq!(raw_keys(&kine, &prefix).await.len(), 14);
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(true)));

    let r = reconcile_api(&cp, admin.cookie(), true).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({"deleted_strays": 12, "marked_dirty": 0}));
    assert_eq!(raw_keys(&kine, &prefix).await.len(), 2);
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(false)));

    // Among 314 keys, 5% is 16 (rounded up), so 12 strays are a few.
    let writes = (0..300)
        .map(|_| {
            let doc = provider_key(&unique("bulk"), SECRET);
            (Kind::ProviderKeys, Uuid::new_v4(), Op::Create(doc))
        })
        .collect();
    write_all(db, env, writes).await;
    all_projected(db).await;
    put_strays(12).await;
    let r = reconcile_api(&cp, admin.cookie(), false).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({"deleted_strays": 12, "marked_dirty": 0}));
    assert_eq!(raw_keys(&kine, &prefix).await.len(), 302);
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(false)));
}

#[tokio::test]
async fn reconcile_rewrites_a_key_whose_bytes_drifted() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let env = cp.env();
    let (a_name, b_name) = (unique("a"), unique("b"));
    let a = api_create(
        &cp,
        admin.cookie(),
        "provider_keys",
        provider_key(&a_name, SECRET),
    )
    .await;
    let b = api_create(
        &cp,
        admin.cookie(),
        "provider_keys",
        provider_key(&b_name, SECRET),
    )
    .await;
    all_projected(cp.db()).await;
    let (a_key, b_key) = (
        key_for(env, Kind::ProviderKeys, a),
        key_for(env, Kind::ProviderKeys, b),
    );
    // One key rewritten behind the control plane's back, one removed.
    raw_put(&kine, &a_key, br#"{"display_name":"tampered-7c1"}"#).await;
    raw_delete(&kine, &b_key).await;

    let r = reconcile_api(&cp, admin.cookie(), false).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({"deleted_strays": 0, "marked_dirty": 2}));
    all_projected(cp.db()).await;
    let (a_value, a_revision) = raw_json(&kine, &a_key).await.expect("a is in kine");
    assert_eq!(a_value, provider_key(&a_name, SECRET));
    let (b_value, _) = raw_json(&kine, &b_key).await.expect("b is back in kine");
    assert_eq!(b_value, provider_key(&b_name, SECRET));
    let row = row_state(cp.db(), Kind::ProviderKeys, a).await;
    assert_eq!(row.kine_mod_revision, Some(a_revision));
}

#[tokio::test]
async fn reconcile_on_an_empty_database_deletes_nothing_until_an_admin_allows_it() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let env = first.env().clone();
    let writes = (0..20)
        .map(|_| {
            let doc = provider_key(&unique("acct"), SECRET);
            (Kind::ProviderKeys, Uuid::new_v4(), Op::Create(doc))
        })
        .collect();
    write_all(first.db(), &env, writes).await;
    all_projected(first.db()).await;
    let prefix = env.env_prefix();
    assert_eq!(raw_keys(&kine, &prefix).await.len(), 20);
    drop(first);

    // The same environment, on a database that holds none of it.
    let second = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = endpoint.clone();
        cfg.env_id_seed = Some(env.env_id);
    })
    .await
    .expect("the second control plane boots");
    assert_eq!(second.env().env_id, env.env_id);
    let db = second.db();
    eventually("reconcile to be blocked", PATIENCE, || async move {
        setting(db, "reconcile_blocked").await == Some(json!(true))
    })
    .await;
    assert_eq!(raw_keys(&kine, &prefix).await.len(), 20);

    let admin = second.admin_login_seeded(OperatorRole::Admin).await;
    let r = reconcile_api(&second, admin.cookie(), true).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({"deleted_strays": 20, "marked_dirty": 0}));
    assert!(raw_keys(&kine, &prefix).await.is_empty());
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(false)));
    let audit: Vec<(String, Option<String>, Value)> = sqlx::query_as(
        "SELECT action, actor_email, detail FROM audit_log WHERE action = 'rollout.reconcile'",
    )
    .fetch_all(db.pool())
    .await
    .expect("read the audit log");
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].1.as_deref(), Some(admin.email.as_str()));
    assert_eq!(
        audit[0].2,
        json!({"allow_deletes": true, "deleted_strays": 20, "marked_dirty": 0})
    );
}

#[tokio::test]
async fn a_document_at_the_size_cap_projects() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let (db, env) = (cp.db(), cp.env());
    // Exactly 1 MiB serialized, the largest document a write accepts.
    let name = unique("big");
    let base = serde_json::to_vec(&provider_key(&name, ""))
        .expect("serialize")
        .len();
    let doc = provider_key(&name, &"k".repeat((1 << 20) - base));
    assert_eq!(serde_json::to_vec(&doc).expect("serialize").len(), 1 << 20);
    let id = Uuid::new_v4();
    write_all(
        db,
        env,
        vec![(Kind::ProviderKeys, id, Op::Create(doc.clone()))],
    )
    .await;
    all_projected(db).await;
    let (bytes, _) = raw_get(&kine, &key_for(env, Kind::ProviderKeys, id))
        .await
        .expect("the key is in kine");
    assert_eq!(bytes.len(), 1 << 20);
    assert_eq!(serde_json::from_slice::<Value>(&bytes).expect("JSON"), doc);
}

#[tokio::test]
async fn a_row_that_cannot_be_projected_holds_back_the_rest_and_says_so() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let (db, env) = (cp.db(), cp.env());
    let a_name = unique("a");
    let a = create_key(db, env, &a_name).await;
    all_projected(db).await;

    // A sealed credential that does not open, as one sealed under another
    // master key would not; then a row written after it.
    sqlx::query(
        "UPDATE gw_resources SET \
           doc = jsonb_set(doc, '{api_key}', to_jsonb('aisixsealed.v1.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'::text)), \
           version = version + 1, seq = nextval('gw_resources_seq') \
         WHERE kind = 'provider_keys' AND id = $1",
    )
    .bind(a)
    .execute(db.pool())
    .await
    .expect("damage the stored credential");
    let b = create_key(db, env, &unique("b")).await;
    let b_key = key_for(env, Kind::ProviderKeys, b);

    eventually("the stall to be reported", PATIENCE, || async move {
        setting(db, "projection_stalled")
            .await
            .is_some_and(|s| s["id"] == json!(a.to_string()))
    })
    .await;
    let stalled = setting(db, "projection_stalled")
        .await
        .expect("the stall is reported");
    assert_eq!(stalled["kind"], "provider_keys", "{stalled}");
    assert!(!stalled.to_string().contains("aisixsealed"), "{stalled}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!row_state(db, Kind::ProviderKeys, b).await.projected());
    assert_eq!(raw_get(&kine, &b_key).await, None, "b skipped ahead of a");

    // Entering the credential again lets everything through.
    let doc = provider_key(&a_name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, a, Op::Replace(doc))]).await;
    all_projected(db).await;
    assert!(raw_get(&kine, &b_key).await.is_some());
    let (a_value, _) = raw_json(&kine, &key_for(env, Kind::ProviderKeys, a))
        .await
        .expect("a is in kine");
    assert_eq!(a_value["api_key"], ROTATED);
    assert_eq!(setting(db, "projection_stalled").await, None);
}

#[tokio::test]
async fn a_new_leader_reconciles_before_it_projects() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let log = Arc::new(Mutex::new(Vec::<Write>::new()));
    let (stop, task) = spawn_recorded("a", &bare, &endpoint, &log).await;
    let name = unique("acct");
    let id = create_key(db, env, &name).await;
    all_projected(db).await;
    let _ = stop.send(());
    task.await.expect("the first projector stops");

    // While no projector runs, kine loses a key and gains a stray.
    let key = key_for(env, Kind::ProviderKeys, id);
    let stray = format!("{}models/{}", env.env_prefix(), Uuid::new_v4());
    raw_delete(&kine, &key).await;
    raw_put(&kine, &stray, b"{}").await;

    // The next leader is not asked to reconcile, and does.
    let (stop, task) = spawn_recorded("b", &bare, &endpoint, &log).await;
    eventually("the missing key to be written again", PATIENCE, || {
        let (kine, key) = (&kine, &key);
        async move { raw_get(kine, key).await.is_some() }
    })
    .await;
    let (value, _) = raw_json(&kine, &key).await.expect("the key is back");
    assert_eq!(value, provider_key(&name, SECRET));
    assert_eq!(
        raw_get(&kine, &stray).await,
        None,
        "a lone stray is removed"
    );
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(false)));
    let _ = stop.send(());
    task.await.expect("the second projector stops");
}

#[tokio::test]
async fn reconcile_on_an_empty_database_keeps_even_a_few_strays() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let env = first.env().clone();
    for _ in 0..3 {
        create_key(first.db(), &env, &unique("acct")).await;
    }
    all_projected(first.db()).await;
    drop(first);

    // Three strays are few, but a database without a single live row is
    // not one to delete by.
    let second = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = endpoint.clone();
        cfg.env_id_seed = Some(env.env_id);
    })
    .await
    .expect("the second control plane boots");
    let db = second.db();
    eventually("reconcile to be blocked", PATIENCE, || async move {
        setting(db, "reconcile_blocked").await == Some(json!(true))
    })
    .await;
    assert_eq!(raw_keys(&kine, &env.env_prefix()).await.len(), 3);
}

#[tokio::test]
async fn a_delete_not_yet_projected_is_not_a_stray() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let log = Arc::new(Mutex::new(Vec::<Write>::new()));
    let (stop, task) = spawn_recorded("a", &bare, &endpoint, &log).await;
    let kept = create_key(db, env, &unique("kept")).await;
    let gone: Vec<Uuid> = {
        let ids: Vec<Uuid> = (0..12).map(|_| Uuid::new_v4()).collect();
        let writes = ids
            .iter()
            .map(|id| {
                let doc = provider_key(&unique("gone"), SECRET);
                (Kind::ProviderKeys, *id, Op::Create(doc))
            })
            .collect();
        write_all(db, env, writes).await;
        ids
    };
    all_projected(db).await;
    let _ = stop.send(());
    task.await.expect("the first projector stops");

    // Twelve deletes, more than reconcile removes on its own, still to be
    // projected when the next leader reconciles: they are the projector's
    // to make, not strays.
    write_all(
        db,
        env,
        gone.iter()
            .map(|id| (Kind::ProviderKeys, *id, Op::Delete))
            .collect(),
    )
    .await;
    let (stop, task) = spawn_recorded("b", &bare, &endpoint, &log).await;
    all_projected(db).await;
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(false)));
    assert_eq!(
        raw_keys(&kine, &env.env_prefix()).await,
        BTreeSet::from([key_for(env, Kind::ProviderKeys, kept)])
    );
    let _ = stop.send(());
    task.await.expect("the second projector stops");
}

#[tokio::test]
async fn an_instance_that_cannot_reach_kine_leaves_the_lock_to_one_that_can() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let db = &bare.db;
    let log = Arc::new(Mutex::new(Vec::<Write>::new()));
    let (stop_cut_off, cut_off) =
        spawn_recorded("cut-off", &bare, "http://127.0.0.1:0", &log).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(lock_holders(db).await.is_empty());

    let (stop, task) = spawn_recorded("reaches", &bare, &endpoint, &log).await;
    create_key(db, &bare.env, &unique("acct")).await;
    all_projected(db).await;
    assert_eq!(lock_holders(db).await.len(), 1);
    for (stop, task) in [(stop_cut_off, cut_off), (stop, task)] {
        let _ = stop.send(());
        task.await.expect("a projector stops");
    }
}
