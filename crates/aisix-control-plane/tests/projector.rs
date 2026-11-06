//! The projector: stored gateway resources reach kine under the
//! environment's prefix with their credentials in plaintext, in commit
//! order, from exactly one writer; and reconcile never deletes much without
//! an admin's say-so.
//!
//! Most cases run against the real test kine (`CP_TEST_KINE_ENDPOINT`):
//! every control plane mints a fresh `env_id`, so its prefix is its own. The
//! cases that need a kine failing on cue, which real kine cannot stage, use
//! [`MemKine`]. Kine is read back with `etcd_client` directly, not with the
//! control plane's own client.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::Db;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::kine::{KeyValue, KineClient, KineError, KineWriter, WriteOutcome};
use aisix_control_plane::projector::{self, claim_key, key_for, Timing, PROJECTOR_LOCK_KEY};
use aisix_control_plane::resources::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use aisix_control_plane::resources::Kind;
use aisix_control_plane::settings::EnvSettings;
use aisix_etcd::{build_snapshot, PrefixSet, RawEntry, WatchedPrefix};
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
    create_key_as(db, env, id, name).await;
    id
}

/// [`create_key`], with the id given.
async fn create_key_as(db: &Db, env: &EnvSettings, id: Uuid, name: &str) {
    let doc = provider_key(name, SECRET);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Create(doc))]).await;
}

/// `n` fresh ids in ascending order, for tests whose write order must
/// contradict id order: a projector that went by id instead of by write
/// would then fail them every time, not one run in a few.
fn ascending_ids(n: usize) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = (0..n).map(|_| Uuid::new_v4()).collect();
    ids.sort();
    ids
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
    keys: BTreeMap<Vec<u8>, KeyValue>,
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
        self.value_at(key.as_bytes())
    }

    fn value_at(&self, key: &[u8]) -> Option<KeyValue> {
        self.0.lock().expect("lock").keys.get(key).cloned()
    }

    /// Writes `key` as another writer would, bypassing the log and faults.
    fn put_raw(&self, key: &[u8], value: &[u8]) {
        self.0.lock().expect("lock").put(key, value);
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
    fn put(&mut self, key: &[u8], value: &[u8]) -> WriteOutcome {
        self.revision += 1;
        let kv = KeyValue {
            value: value.to_vec(),
            mod_revision: self.revision,
        };
        self.keys.insert(key.to_vec(), kv);
        WriteOutcome::Applied {
            revision: self.revision,
        }
    }
}

#[async_trait]
impl KineWriter for MemKine {
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError> {
        let key_bytes = key.as_bytes();
        self.write("create", key, |m| match m.keys.get(key_bytes).cloned() {
            Some(kv) => WriteOutcome::Conflict { current: Some(kv) },
            None => m.put(key_bytes, value),
        })
    }

    async fn update(
        &self,
        key: &str,
        expected_mod_revision: i64,
        value: &[u8],
    ) -> Result<WriteOutcome, KineError> {
        let key_bytes = key.as_bytes();
        self.write("update", key, |m| match m.keys.get(key_bytes).cloned() {
            Some(kv) if kv.mod_revision == expected_mod_revision => m.put(key_bytes, value),
            current => WriteOutcome::Conflict { current },
        })
    }

    async fn delete(
        &self,
        key: &str,
        expected_mod_revision: i64,
    ) -> Result<WriteOutcome, KineError> {
        let key_bytes = key.as_bytes();
        self.write("delete", key, |m| match m.keys.get(key_bytes).cloned() {
            Some(kv) if kv.mod_revision == expected_mod_revision => {
                m.keys.remove(key_bytes);
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

    async fn list(&self, prefix: &str) -> Result<Vec<(Vec<u8>, KeyValue)>, KineError> {
        let m = self.0.lock().expect("lock");
        Ok(m.keys
            .iter()
            .filter(|(k, _)| k.starts_with(prefix.as_bytes()))
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
    // Written a, b, c, with ids in the opposite order.
    let mut ids = ascending_ids(3);
    ids.reverse();
    for (id, name) in ids.iter().zip(["a", "b", "c"]) {
        create_key_as(db, env, *id, &unique(name)).await;
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
    let ids = ascending_ids(3);
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    let a_name = unique("a");
    // One transaction, so the projector finds all of it dirty at once: `a`
    // was created first and rewritten last, so it is written last, though
    // its id is the smallest.
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

/// kine processes of the test's own, all on one database of their own (as
/// two control-plane hosts would run them), that the test can stop and
/// start again.
struct PrivateKine {
    bin: String,
    server_url: String,
    database: String,
    /// Each process's port, and the process while it runs.
    processes: Vec<(u16, Option<tokio::process::Child>)>,
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
        let mut kine = PrivateKine {
            bin,
            server_url,
            database,
            processes: Vec::new(),
        };
        kine.add().await;
        Some(kine)
    }

    /// Starts one more process on the same database; its index.
    async fn add(&mut self) -> usize {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        self.processes.push((port, None));
        let i = self.processes.len() - 1;
        self.resume(i).await;
        i
    }

    fn endpoint(&self, i: usize) -> String {
        format!("http://127.0.0.1:{}", self.processes[i].0)
    }

    async fn resume(&mut self, i: usize) {
        let port = self.processes[i].0;
        let mut datastore = reqwest::Url::parse(&self.server_url).expect("a URL");
        datastore.set_path(&format!("/{}", self.database));
        datastore.set_query(Some("sslmode=disable"));
        let child = tokio::process::Command::new(&self.bin)
            .args([
                "--endpoint",
                datastore.as_str(),
                "--listen-address",
                &format!("127.0.0.1:{port}"),
                "--metrics-bind-address",
                "0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start kine");
        self.processes[i].1 = Some(child);
        eventually("kine to listen", PATIENCE, || async move {
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
        })
        .await;
    }

    async fn stop(&mut self, i: usize) {
        if let Some(mut child) = self.processes[i].1.take() {
            child.kill().await.expect("stop kine");
        }
    }
}

impl Drop for PrivateKine {
    fn drop(&mut self) {
        for (_, child) in &mut self.processes {
            if let Some(child) = child {
                let _ = child.start_kill();
            }
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
    let endpoint = private.endpoint(0);
    let Some(cp) = TestCp::start_with(|cfg| cfg.kine_endpoint = endpoint.clone()).await else {
        return;
    };
    let (db, env) = (cp.db(), cp.env());
    // Landing b, c, a: against the order of their ids.
    let ids = ascending_ids(3);
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    let a_name = unique("a");
    create_key_as(db, env, a, &a_name).await;
    all_projected(db).await;

    private.stop(0).await;
    create_key_as(db, env, b, &unique("b")).await;
    create_key_as(db, env, c, &unique("c")).await;
    let doc = provider_key(&a_name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, a, Op::Replace(doc))]).await;
    // The stall names kine, not the row it happened to be at.
    eventually("the stall to be reported", PATIENCE, || async move {
        setting(db, "projection_stalled").await == Some(json!({"error": "kine unavailable"}))
    })
    .await;
    assert_eq!(dirty_rows(db).await, 3, "nothing lands while kine is down");

    private.resume(0).await;
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

    async fn list(&self, prefix: &str) -> Result<Vec<(Vec<u8>, KeyValue)>, KineError> {
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
    spawn_recorded_with(by, bare, endpoint, log, Timing::default()).await
}

/// [`spawn_recorded`], at the given timing.
async fn spawn_recorded_with(
    by: &'static str,
    bare: &BareDatabase,
    endpoint: &str,
    log: &Arc<Mutex<Vec<Write>>>,
    timing: Timing,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let kine = Recorder {
        by,
        inner: KineClient::connect(endpoint).await.expect("a kine client"),
        log: log.clone(),
    };
    spawn_on(bare, Arc::new(kine), timing)
}

/// A projector on `bare` writing through `kine`; stopped by the sender.
fn spawn_on(
    bare: &BareDatabase,
    kine: Arc<dyn KineWriter>,
    timing: Timing,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (stop, stopped) = oneshot::channel::<()>();
    let task = projector::spawn_with(bare.db.clone(), kine, ctx(&bare.env), timing, async move {
        let _ = stopped.await;
    });
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
    // What was about to be deleted was audited before the answer was.
    let audit = reconcile_audit(cp.db()).await;
    let actions: Vec<&str> = audit.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(actions, ["rollout.reconcile.planned", "rollout.reconcile"]);
    let mut planned = strays.to_vec();
    planned.sort();
    assert_eq!(audit[0].2, json!({"allow_deletes": false, "keys": planned}));
    assert_eq!(audit[0].1.as_deref(), Some(admin.email.as_str()));
}

/// The `rollout.reconcile*` audit rows, oldest first: `(action, actor
/// email, detail)`.
async fn reconcile_audit(db: &Db) -> Vec<(String, Option<String>, Value)> {
    sqlx::query_as(
        "SELECT action, actor_email, detail FROM audit_log \
         WHERE starts_with(action, 'rollout.reconcile') ORDER BY id",
    )
    .fetch_all(db.pool())
    .await
    .expect("read the audit log")
}

/// What a backup of `cp`'s database taken before any resource was written
/// holds: its database id and environment.
async fn backup_of(cp: &TestCp) -> (String, Uuid) {
    let database_id = setting(cp.db(), "database_id")
        .await
        .expect("a database id")
        .as_str()
        .expect("a string")
        .to_string();
    (database_id, cp.env().env_id)
}

/// A control plane on that backup, restored: the same database id and
/// environment, none of the rows.
async fn restored_copy((database_id, env_id): (String, Uuid), kine_endpoint: &str) -> TestCp {
    let endpoint = kine_endpoint.to_string();
    TestCp::start_seeded(
        move |cfg| {
            cfg.kine_endpoint = endpoint;
            cfg.env_id_seed = Some(env_id);
        },
        &[format!(
            "INSERT INTO cp_settings (key, value) VALUES ('database_id', to_jsonb('{database_id}'::text))"
        )],
    )
    .await
    .expect("the restored copy boots")
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

    // The same database as it was before any of it was written, restored
    // from a backup, in place of the original.
    let backup = backup_of(&first).await;
    drop(first);
    let second = restored_copy(backup, &endpoint).await;
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
    let audit = reconcile_audit(db).await;
    assert_eq!(audit.len(), 2, "{audit:?}");
    assert_eq!(audit[0].0, "rollout.reconcile.planned");
    assert_eq!(audit[0].2["keys"].as_array().map(Vec::len), Some(20));
    assert_eq!(audit[1].0, "rollout.reconcile");
    for (_, email, _) in &audit {
        assert_eq!(email.as_deref(), Some(admin.email.as_str()));
    }
    assert_eq!(
        audit[1].2,
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

    // Three strays are few, but a database without a single live row is
    // not one to delete by.
    let backup = backup_of(&first).await;
    drop(first);
    let second = restored_copy(backup, &endpoint).await;
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

// ---- fix round 1 -----------------------------------------------------------

/// A second projector taking over in the middle of a row, as the leader
/// sees it: the first update it is armed for ends the leader's session,
/// writes a newer value to the key the way the new leader would, and answers
/// with the conflict that write causes.
struct Deposer {
    inner: KineClient,
    db: Db,
    armed: AtomicBool,
    /// Every call but `get`: the operation and its key (the prefix, for a
    /// list).
    ops: Mutex<Vec<(&'static str, String)>>,
}

impl Deposer {
    fn note(&self, op: &'static str, key: &str) {
        self.ops.lock().expect("lock").push((op, key.to_string()));
    }

    fn ops(&self) -> Vec<(&'static str, String)> {
        self.ops.lock().expect("lock").clone()
    }
}

#[async_trait]
impl KineWriter for Deposer {
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError> {
        self.note("create", key);
        self.inner.create(key, value).await
    }

    async fn update(&self, key: &str, rev: i64, value: &[u8]) -> Result<WriteOutcome, KineError> {
        if !self.armed.swap(false, Ordering::SeqCst) {
            self.note("update", key);
            return self.inner.update(key, rev, value).await;
        }
        let holders = lock_holders(&self.db).await;
        assert_eq!(holders.len(), 1, "{holders:?}");
        let pid = holders[0];
        let pool = self.db.pool();
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .execute(pool)
            .await
            .expect("end the leader's session");
        eventually("the leader's session to end", PATIENCE, || async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pg_stat_activity WHERE pid = $1")
                .bind(pid)
                .fetch_one(pool)
                .await
                .is_ok_and(|n| n == 0)
        })
        .await;
        let current = self.inner.get(key).await?.expect("the key is projected");
        let newer = br#"{"display_name":"newer-leader-7c1"}"#;
        let WriteOutcome::Applied { revision } =
            self.inner.update(key, current.mod_revision, newer).await?
        else {
            panic!("the new leader's write lands");
        };
        self.note("update:conflict", key);
        Ok(WriteOutcome::Conflict {
            current: Some(KeyValue {
                value: newer.to_vec(),
                mod_revision: revision,
            }),
        })
    }

    async fn delete(&self, key: &str, rev: i64) -> Result<WriteOutcome, KineError> {
        self.note("delete", key);
        self.inner.delete(key, rev).await
    }

    async fn get(&self, key: &str) -> Result<Option<KeyValue>, KineError> {
        self.inner.get(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<(Vec<u8>, KeyValue)>, KineError> {
        self.note("list", prefix);
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn a_deposed_leader_writes_nothing_more() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let deposer = Arc::new(Deposer {
        inner: KineClient::connect(&endpoint).await.expect("a kine client"),
        db: db.clone(),
        armed: AtomicBool::new(false),
        ops: Mutex::default(),
    });
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    let name = unique("acct");
    let id = create_key(db, env, &name).await;
    all_projected(db).await;

    deposer.armed.store(true, Ordering::SeqCst);
    let doc = provider_key(&name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Replace(doc))]).await;
    all_projected(db).await;

    // After the conflict, the deposed leader wrote nothing: what comes next
    // is the new tenure, which claims the environment and reconciles.
    let ops = deposer.ops();
    let at = ops
        .iter()
        .position(|(op, _)| *op == "update:conflict")
        .expect("the conflict was injected");
    let claim = claim_key(env);
    let next = ops[at + 1..]
        .iter()
        .find(|(_, key)| *key != claim)
        .expect("an operation after the conflict");
    assert_eq!(
        next,
        &("list", env.env_prefix()),
        "after the conflict: {:?}",
        &ops[at..]
    );
    // That tenure, the only leader, then writes the row.
    let (value, _) = raw_json(&kine, &key_for(env, Kind::ProviderKeys, id))
        .await
        .expect("the key is in kine");
    assert_eq!(value, provider_key(&name, ROTATED));
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_tenure_reconciles_again_on_its_own() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let timing = Timing {
        reconcile_every: Duration::from_secs(1),
        ..Timing::default()
    };
    let log = Arc::new(Mutex::new(Vec::<Write>::new()));
    let (stop, task) = spawn_recorded_with("a", &bare, &endpoint, &log, timing).await;
    let name = unique("acct");
    let id = create_key(db, env, &name).await;
    all_projected(db).await;
    let holders = lock_holders(db).await;

    // A write that landed after its session ended, say: no row write, no
    // request, and the tenure finds it anyway.
    let key = key_for(env, Kind::ProviderKeys, id);
    let stray = format!("{}models/{}", env.env_prefix(), Uuid::new_v4());
    raw_delete(&kine, &key).await;
    raw_put(&kine, &stray, b"{}").await;
    eventually("kine to be reconciled", PATIENCE, || {
        let (kine, key, stray) = (&kine, &key, &stray);
        async move { raw_get(kine, key).await.is_some() && raw_get(kine, stray).await.is_none() }
    })
    .await;
    assert_eq!(lock_holders(db).await, holders, "one tenure did it all");
    let audit = reconcile_audit(db).await;
    assert!(
        audit
            .iter()
            .any(|(action, email, _)| action == "rollout.reconcile.planned" && email.is_none()),
        "{audit:?}"
    );
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_leader_whose_kine_stays_down_hands_over() {
    let Some(mut private) = PrivateKine::start().await else {
        return;
    };
    let other = private.add().await;
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let timing = Timing {
        kine_down_step_down: Duration::from_secs(2),
        ..Timing::default()
    };
    let log = Arc::new(Mutex::new(Vec::<Write>::new()));
    let began = Instant::now();
    let a = spawn_recorded_with("a", &bare, &private.endpoint(0), &log, timing).await;
    // `a` leads once it has claimed the environment.
    eventually("a to lead", PATIENCE, || {
        let log = &log;
        async move { writers_since(log, began).contains("a") }
    })
    .await;
    let b = spawn_recorded_with("b", &bare, &private.endpoint(other), &log, timing).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(writers_since(&log, began), BTreeSet::from(["a"]));

    // a's kine goes down; b's, on the same database, stays up.
    private.stop(0).await;
    let down = Instant::now();
    let id = create_key(db, env, &unique("acct")).await;
    all_projected(db).await;
    let kine = raw_kine(&private.endpoint(other)).await;
    let key = key_for(env, Kind::ProviderKeys, id);
    assert!(raw_get(&kine, &key).await.is_some());
    assert!(writers_since(&log, down).contains("b"), "b took over");
    for (stop, task) in [a, b] {
        let _ = stop.send(());
        task.await.expect("a projector stops");
    }
}

/// Holds the update it is armed for until the test lets it through.
struct Gate {
    inner: KineClient,
    armed: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

#[async_trait]
impl KineWriter for Gate {
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError> {
        self.inner.create(key, value).await
    }

    async fn update(&self, key: &str, rev: i64, value: &[u8]) -> Result<WriteOutcome, KineError> {
        let gate = self.armed.lock().expect("lock").take();
        if let Some((reached, go)) = gate {
            let _ = reached.send(());
            let _ = go.await;
        }
        self.inner.update(key, rev, value).await
    }

    async fn delete(&self, key: &str, rev: i64) -> Result<WriteOutcome, KineError> {
        self.inner.delete(key, rev).await
    }

    async fn get(&self, key: &str) -> Result<Option<KeyValue>, KineError> {
        self.inner.get(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<(Vec<u8>, KeyValue)>, KineError> {
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn a_row_lock_wait_costs_a_pass_not_the_lock() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let gate = Arc::new(Gate {
        inner: KineClient::connect(&endpoint).await.expect("a kine client"),
        armed: Mutex::new(None),
    });
    let (stop, task) = spawn_on(&bare, gate.clone(), Timing::default());
    let name = unique("acct");
    let id = create_key(db, env, &name).await;
    all_projected(db).await;
    let holders = lock_holders(db).await;

    let (reached, at_update) = oneshot::channel();
    let (go, wait) = oneshot::channel();
    *gate.armed.lock().expect("lock") = Some((reached, wait));
    let doc = provider_key(&name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Replace(doc))]).await;
    at_update.await.expect("the projector reaches the update");
    // The row is held while the projector records its write, longer than
    // the session's lock_timeout (5 s).
    let mut holder = PgConnection::connect(bare.url())
        .await
        .expect("a connection of the test's own");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("SELECT 1 FROM gw_resources WHERE kind = 'provider_keys' AND id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut holder)
        .await
        .expect("hold the row");
    let _ = go.send(());
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert_eq!(lock_holders(db).await, holders, "a lock wait cost the lock");
    sqlx::query("ROLLBACK")
        .execute(&mut holder)
        .await
        .expect("release the row");
    all_projected(db).await;
    assert_eq!(lock_holders(db).await, holders);
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_reconcile_request_nobody_waits_for_is_never_run() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let (db, env) = (cp.db(), cp.env());
    create_key(db, env, &unique("acct")).await;
    all_projected(db).await;
    let stray = format!("{}models/{}", env.env_prefix(), Uuid::new_v4());
    raw_put(&kine, &stray, b"{}").await;

    // A request as a database restored from backup might hold it: deletes
    // allowed, asked an hour ago.
    let request = format!("reconcile_request:{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO cp_settings (key, value, updated_at) VALUES ($1, $2, now() - interval '1 hour')",
    )
    .bind(&request)
    .bind(json!({"allow_deletes": true, "operator_id": admin.id, "operator_email": admin.email}))
    .execute(db.pool())
    .await
    .expect("store the old request");
    // A write wakes the leader.
    create_key(db, env, &unique("wake")).await;
    eventually("the old request to be dropped", PATIENCE, || {
        let request = &request;
        async move { setting(db, request).await.is_none() }
    })
    .await;
    assert!(raw_get(&kine, &stray).await.is_some(), "the request ran");
    assert!(reconcile_audit(db).await.is_empty());
}

#[tokio::test]
async fn a_key_that_is_not_utf8_is_never_deleted() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let kine = MemKine::default();
    create_key(db, env, &unique("acct")).await;
    projector::project_once(db, &kine, &ctx(env))
        .await
        .expect("projected");
    let prefix = env.env_prefix();
    let mut unreadable = format!("{prefix}models/").into_bytes();
    unreadable.extend_from_slice(&[0xff, 0xfe]);
    let stray = format!("{prefix}models/{}", Uuid::new_v4());
    kine.put_raw(&unreadable, b"{}");
    kine.put_raw(stray.as_bytes(), b"{}");

    // One readable stray is few, but the unreadable key blocks reconcile.
    let report = projector::reconcile(db, &kine, &ctx(env), false)
        .await
        .expect("reconciled");
    assert_eq!((report.deleted_strays, report.blocked), (0, true));
    assert!(kine.value(&stray).is_some());
    // An admin's go-ahead deletes the readable one only.
    let report = projector::reconcile(db, &kine, &ctx(env), true)
        .await
        .expect("reconciled");
    assert_eq!((report.deleted_strays, report.blocked), (1, true));
    assert!(kine.value(&stray).is_none());
    assert!(kine.value_at(&unreadable).is_some(), "never deleted");
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(true)));
}

#[tokio::test]
async fn the_one_off_passes_refuse_while_another_session_leads() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let kine = MemKine::default();
    create_key(db, env, &unique("acct")).await;
    let mut leader = PgConnection::connect(bare.url())
        .await
        .expect("a connection of the test's own");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(PROJECTOR_LOCK_KEY)
        .execute(&mut leader)
        .await
        .expect("take the projector lock");
    let projected = projector::project_once(db, &kine, &ctx(env)).await;
    assert!(
        matches!(projected, Err(projector::ProjectError::NotLeader)),
        "{projected:?}"
    );
    let reconciled = projector::reconcile(db, &kine, &ctx(env), true).await;
    assert!(
        matches!(reconciled, Err(projector::ProjectError::NotLeader)),
        "{reconciled:?}"
    );
    assert!(kine.log().is_empty(), "{:?}", kine.log());
    let _ = leader.close().await;
    assert_eq!(
        projector::project_once(db, &kine, &ctx(env))
            .await
            .expect("projected once the lock is free"),
        1
    );
}

#[tokio::test]
async fn one_environment_has_one_owner_across_databases() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let env = first.env().clone();
    let a = create_key(first.db(), &env, &unique("a")).await;
    all_projected(first.db()).await;
    let (claim, _) = raw_json(&kine, &claim_key(&env))
        .await
        .expect("the first control plane claimed the environment");
    assert_eq!(
        claim,
        json!({"database_id": setting(first.db(), "database_id").await.expect("an id")})
    );

    // Another database, configured with the same environment.
    let second = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = endpoint.clone();
        cfg.env_id_seed = Some(env.env_id);
    })
    .await
    .expect("the second control plane boots");
    let db = second.db();
    eventually(
        "the second to report it is blocked",
        PATIENCE,
        || async move {
            setting(db, "projection_blocked").await
                == Some(json!({"reason": "another control plane owns this environment"}))
        },
    )
    .await;
    let b = create_key(db, &env, &unique("b")).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!row_state(db, Kind::ProviderKeys, b).await.projected());
    assert_eq!(
        raw_get(&kine, &key_for(&env, Kind::ProviderKeys, b)).await,
        None
    );
    assert!(lock_holders(db).await.is_empty());
    let admin = second.admin_login_seeded(OperatorRole::Admin).await;
    assert_eq!(
        reconcile_api(&second, admin.cookie(), true).await.status,
        503
    );

    // The first goes on projecting, and nothing of its was deleted.
    let c = create_key(first.db(), &env, &unique("c")).await;
    all_projected(first.db()).await;
    for id in [a, c] {
        let key = key_for(&env, Kind::ProviderKeys, id);
        assert!(raw_get(&kine, &key).await.is_some(), "{key}");
    }
}

#[tokio::test]
async fn a_projected_document_loads_in_the_gateways_own_loader() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let pk_name = unique("acct");
    let pk = api_create(
        &cp,
        op.cookie(),
        "provider_keys",
        provider_key(&pk_name, SECRET),
    )
    .await;
    let model_name = unique("chat");
    let model = api_create(
        &cp,
        op.cookie(),
        "models",
        json!({
            "display_name": model_name,
            "provider": "openai",
            "model_name": "gpt-4o",
            "provider_key_id": pk,
        }),
    )
    .await;
    all_projected(cp.db()).await;

    // What a gateway of this environment reads, through its own loader.
    let env = cp.env();
    let resp = kine
        .kv_client()
        .get(env.env_prefix(), Some(GetOptions::new().with_prefix()))
        .await
        .expect("read the environment's prefix");
    let entries: Vec<RawEntry> = resp
        .kvs()
        .iter()
        .map(|kv| RawEntry {
            key: String::from_utf8(kv.key().to_vec()).expect("a UTF-8 key"),
            value: kv.value().to_vec(),
            revision: kv.mod_revision(),
        })
        .collect();
    let prefixes = PrefixSet::new(vec![
        WatchedPrefix::environment(env.env_prefix()),
        WatchedPrefix::global(env.global_prefix()),
    ]);
    let (snapshot, stats) = build_snapshot(&prefixes, &entries);
    assert_eq!(stats.accepted, 2, "{stats:?}");
    assert!(stats.rejections.is_empty(), "{stats:?}");
    assert!(stats.partially_compatible.is_empty(), "{stats:?}");
    let key = snapshot
        .provider_keys
        .get_by_id(&pk.to_string())
        .expect("the provider key loads");
    assert_eq!(key.value.display_name, pk_name);
    assert_eq!(key.value.api_key, SECRET);
    let loaded = snapshot
        .models
        .get_by_id(&model.to_string())
        .expect("the model loads");
    assert_eq!(loaded.value.display_name, model_name);
    assert_eq!(
        loaded.value.provider_key_id.as_deref(),
        Some(pk.to_string().as_str())
    );
}
