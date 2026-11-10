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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aisix_control_plane::auth::{OperatorAuth, OperatorRole};
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::Db;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::housekeeping;
use aisix_control_plane::kine::{KeyValue, KineClient, KineError, KineWriter, WriteOutcome};
use aisix_control_plane::projector::{self, claim_key, key_for, Timing, PROJECTOR_LOCK_KEY};
use aisix_control_plane::resources::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use aisix_control_plane::resources::Kind;
use aisix_control_plane::settings::EnvSettings;
use aisix_core::config::EtcdConfig;
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
        cp_version: aisix_core::HEARTBEAT_VERSION.to_string(),
        stale_secs: 60.0,
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
    assert!(live.iter().all(|key| left.contains(key)), "{left:?}");
    assert_eq!(left, live_keys(cp.db(), env).await);
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

/// How many live documents `db` holds for its platform tenant.
async fn platform_documents(db: &Db) -> usize {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources WHERE NOT deleted \
           AND managed_by = (SELECT 'tenant:' || id FROM tenants WHERE is_internal)",
    )
    .fetch_one(db.pool())
    .await
    .expect("count the platform tenant's documents");
    usize::try_from(n).expect("a count")
}

/// The keys of every live row of `db`: what the prefix holds once it is
/// projected, the platform tenant's own documents included.
async fn live_keys(db: &Db, env: &EnvSettings) -> BTreeSet<String> {
    let rows: Vec<(String, Uuid)> =
        sqlx::query_as("SELECT kind, id FROM gw_resources WHERE NOT deleted")
            .fetch_all(db.pool())
            .await
            .expect("list the live rows");
    rows.into_iter()
        .map(|(kind, id)| key_for(env, Kind::from_segment(&kind).expect("a kind"), id))
        .collect()
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
    // The two, and the platform tenant's own documents.
    let base = raw_keys(&kine, &prefix).await.len();
    let put_strays = |n: usize| {
        let kine = &kine;
        let prefix = &prefix;
        async move {
            for _ in 0..n {
                raw_put(kine, &format!("{prefix}models/{}", Uuid::new_v4()), b"{}").await;
            }
        }
    };

    // 12 strays among a handful of keys: more than 10, and more than 5%.
    put_strays(12).await;
    let r = reconcile_api(&cp, admin.cookie(), false).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({"deleted_strays": 0, "marked_dirty": 0}));
    assert_eq!(raw_keys(&kine, &prefix).await.len(), base + 12);
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(true)));

    let r = reconcile_api(&cp, admin.cookie(), true).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({"deleted_strays": 12, "marked_dirty": 0}));
    assert_eq!(raw_keys(&kine, &prefix).await.len(), base);
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(false)));

    // Among more than 310 keys, 5% is at least 16 (rounded up), so 12
    // strays are a few.
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
    assert_eq!(raw_keys(&kine, &prefix).await.len(), base + 300);
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(false)));
}

/// Puts `n` stray keys under `prefix`.
async fn put_strays(kine: &Client, prefix: &str, n: usize) {
    for _ in 0..n {
        raw_put(kine, &format!("{prefix}models/{}", Uuid::new_v4()), b"{}").await;
    }
}

/// Runs an unasked reconcile on `cp` with `strays` strays among the
/// prefix's `base` keys, and requires it to delete them all when `deleted`
/// and none, blocked, otherwise; strays it leaves are removed with an
/// admin's `allow_deletes` afterwards.
async fn reconcile_with(
    cp: &TestCp,
    kine: &Client,
    cookie: &str,
    base: usize,
    strays: usize,
    deleted: bool,
) {
    let (db, prefix) = (cp.db(), cp.env().env_prefix());
    put_strays(kine, &prefix, strays).await;
    let r = reconcile_api(cp, cookie, false).await;
    assert_eq!(r.status, 200, "{r:?}");
    let removed = if deleted { strays } else { 0 };
    assert_eq!(
        r.body,
        json!({"deleted_strays": removed, "marked_dirty": 0}),
        "{strays} strays among {base} keys"
    );
    assert_eq!(
        setting(db, "reconcile_blocked").await,
        Some(json!(!deleted)),
        "{strays} strays among {base} keys"
    );
    if !deleted {
        let r = reconcile_api(cp, cookie, true).await;
        assert_eq!(r.body["deleted_strays"], strays, "{r:?}");
    }
    assert_eq!(raw_keys(kine, &prefix).await.len(), base);
}

/// However few keys the prefix holds, an unasked reconcile deletes up to 10
/// strays (ruling 28's floor), and blocks at 11.
#[tokio::test]
async fn reconcile_deletes_ten_strays_on_its_own_and_blocks_at_eleven() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let (db, env) = (cp.db(), cp.env());
    for _ in 0..2 {
        create_key(db, env, &unique("acct")).await;
    }
    all_projected(db).await;
    let base = raw_keys(&kine, &env.env_prefix()).await.len();
    // So few keys that 5% of them, strays included, is below the floor.
    assert!((base + 11) * 5 <= 100 * 10, "{base} keys");
    reconcile_with(&cp, &kine, admin.cookie(), base, 10, true).await;
    reconcile_with(&cp, &kine, admin.cookie(), base, 11, false).await;
}

/// Among enough keys that 5% of them is past the floor, an unasked
/// reconcile deletes up to 5% of the prefix's keys (strays included,
/// rounded up), and blocks one past it.
#[tokio::test]
async fn reconcile_deletes_up_to_five_percent_of_a_large_prefix_on_its_own() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let (db, env) = (cp.db(), cp.env());
    let writes = (0..400)
        .map(|_| {
            let doc = provider_key(&unique("bulk"), SECRET);
            (Kind::ProviderKeys, Uuid::new_v4(), Op::Create(doc))
        })
        .collect();
    write_all(db, env, writes).await;
    all_projected(db).await;
    let base = raw_keys(&kine, &env.env_prefix()).await.len();
    // The most strays that are at most 5% of every key, theirs included,
    // rounded up.
    let five_percent = |strays: usize| strays <= ((base + strays) * 5).div_ceil(100);
    let most = (0..base)
        .take_while(|&s| five_percent(s))
        .last()
        .expect("some strays are 5%");
    assert!(most > 10, "5% of {base} keys is past the floor");
    reconcile_with(&cp, &kine, admin.cookie(), base, most, true).await;
    reconcile_with(&cp, &kine, admin.cookie(), base, most + 1, false).await;
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
    // The 20, and the original's platform tenant documents.
    let original = raw_keys(&kine, &prefix).await;
    assert_eq!(
        original.len(),
        20 + platform_documents(first.db()).await,
        "{original:?}"
    );

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
    let kept = raw_keys(&kine, &prefix).await;
    assert!(original.iter().all(|key| kept.contains(key)), "{kept:?}");

    let admin = second.admin_login_seeded(OperatorRole::Admin).await;
    let r = reconcile_api(&second, admin.cookie(), true).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body,
        json!({"deleted_strays": original.len(), "marked_dirty": 0})
    );
    // What is left is what the restored copy holds.
    assert_eq!(raw_keys(&kine, &prefix).await, live_keys(db, &env).await);
    assert_eq!(setting(db, "reconcile_blocked").await, Some(json!(false)));
    let audit = reconcile_audit(db).await;
    assert_eq!(audit.len(), 2, "{audit:?}");
    assert_eq!(audit[0].0, "rollout.reconcile.planned");
    assert_eq!(
        audit[0].2["keys"].as_array().map(Vec::len),
        Some(original.len())
    );
    assert_eq!(audit[1].0, "rollout.reconcile");
    for (_, email, _) in &audit {
        assert_eq!(email.as_deref(), Some(admin.email.as_str()));
    }
    assert_eq!(
        audit[1].2,
        json!({"allow_deletes": true, "deleted_strays": original.len(), "marked_dirty": 0})
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
    let original = raw_keys(&kine, &env.env_prefix()).await;

    // Three strays are few, but a database holding none of the
    // environment, nothing but its own platform tenant, is not one to
    // delete by.
    let backup = backup_of(&first).await;
    drop(first);
    let second = restored_copy(backup, &endpoint).await;
    let db = second.db();
    eventually("reconcile to be blocked", PATIENCE, || async move {
        setting(db, "reconcile_blocked").await == Some(json!(true))
    })
    .await;
    let kept = raw_keys(&kine, &env.env_prefix()).await;
    assert!(original.iter().all(|key| kept.contains(key)), "{kept:?}");
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

/// What an armed [`Deposer`] call does.
#[derive(Debug, Clone)]
enum Injection {
    /// Ends the leader's session; the new leader then writes a newer value
    /// to the key, and an update or a delete is answered with the conflict
    /// that write causes.
    Rewritten,
    /// Ends the leader's session; the new leader then deletes the key, and
    /// an update is answered `Conflict { current: None }`, a get `None`.
    Deleted,
    /// Ends the leader's session; the call is answered as kine answers it.
    Answered,
    /// Leaves the session alone: the call is answered as kine answers it,
    /// and then another database takes the environment over, rewriting the
    /// claim at `claim` to name `owner`.
    ThenClaimedBy { claim: String, owner: Uuid },
    /// Leaves the session alone: the write is applied, and its answer is
    /// lost.
    LostAnswer,
    /// As `LostAnswer`, and every later read of the key fails too.
    LostThenUnreadable,
    /// Leaves the session alone: the write is not applied, and the call
    /// fails as if kine could not be reached.
    Refused,
}

/// Another projector, or another control plane, taking over in the middle
/// of the leader's work, as the leader sees it: the call it is armed for,
/// `(operation, key)`, does what its [`Injection`] says. It also notes every
/// write sent while no session held the projector lock: a write only a
/// deposed leader can send.
struct Deposer {
    inner: KineClient,
    db: Db,
    armed: Mutex<Option<(&'static str, String, Injection)>>,
    /// Every call but an unarmed get: the operation and its key (the prefix,
    /// for a list); the armed call as `<operation>:injected`.
    ops: Mutex<Vec<(&'static str, String)>>,
    /// Writes sent while no session held the lock.
    deposed: Mutex<Vec<(&'static str, String)>>,
    /// A key every read of fails, once set.
    blind: Mutex<Option<String>>,
}

impl Deposer {
    async fn new(endpoint: &str, db: &Db) -> Arc<Deposer> {
        Arc::new(Deposer {
            inner: KineClient::connect(endpoint).await.expect("a kine client"),
            db: db.clone(),
            armed: Mutex::default(),
            ops: Mutex::default(),
            deposed: Mutex::default(),
            blind: Mutex::default(),
        })
    }

    fn arm(&self, op: &'static str, key: &str, injection: Injection) {
        *self.armed.lock().expect("lock") = Some((op, key.to_string(), injection));
    }

    /// The injection, when this is the armed call; it fires once.
    fn fires(&self, op: &str, key: &str) -> Option<Injection> {
        let mut armed = self.armed.lock().expect("lock");
        match &*armed {
            Some((o, k, _)) if *o == op && k == key => armed.take().map(|(_, _, i)| i),
            _ => None,
        }
    }

    fn note(&self, op: &'static str, key: &str) {
        self.ops.lock().expect("lock").push((op, key.to_string()));
    }

    async fn note_write(&self, op: &'static str, key: &str) {
        if lock_holders(&self.db).await.is_empty() {
            self.deposed
                .lock()
                .expect("lock")
                .push((op, key.to_string()));
        }
        self.note(op, key);
    }

    fn ops(&self) -> Vec<(&'static str, String)> {
        self.ops.lock().expect("lock").clone()
    }

    fn deposed(&self) -> Vec<(&'static str, String)> {
        self.deposed.lock().expect("lock").clone()
    }

    /// Ends the session holding the projector lock, and waits until it has.
    async fn end_session(&self) {
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
    }

    /// Deletes `key` as the new leader would.
    async fn delete_as_new_leader(&self, key: &str) -> Result<(), KineError> {
        let current = self.inner.get(key).await?.expect("the key exists");
        let deleted = self.inner.delete(key, current.mod_revision).await?;
        assert!(
            matches!(deleted, WriteOutcome::Applied { .. }),
            "{deleted:?}"
        );
        Ok(())
    }

    /// Writes a newer value to `key` as the new leader would; the conflict
    /// the leader is then answered with.
    async fn rewrite_as_new_leader(&self, key: &str) -> Result<WriteOutcome, KineError> {
        let current = self.inner.get(key).await?.expect("the key is projected");
        let newer = br#"{"display_name":"newer-leader-7c1"}"#;
        let WriteOutcome::Applied { revision } =
            self.inner.update(key, current.mod_revision, newer).await?
        else {
            panic!("the new leader's write lands");
        };
        Ok(WriteOutcome::Conflict {
            current: Some(KeyValue {
                value: newer.to_vec(),
                mod_revision: revision,
            }),
        })
    }

    /// Rewrites the claim at `claim` to name `owner`, as a takeover by
    /// another database would.
    async fn reassign(&self, claim: &str, owner: Uuid) {
        let value = json!({ "database_id": owner }).to_string().into_bytes();
        let written = match self.inner.get(claim).await.expect("read the claim") {
            Some(kv) => self.inner.update(claim, kv.mod_revision, &value).await,
            None => self.inner.create(claim, &value).await,
        }
        .expect("rewrite the claim");
        assert!(
            matches!(written, WriteOutcome::Applied { .. }),
            "{written:?}"
        );
    }

    /// The calls after the injected one, up to and including the next list:
    /// the rest of the deposed tenure, and the next tenure's start.
    fn after_injection(&self) -> Vec<(&'static str, String)> {
        let ops = self.ops();
        let at = ops
            .iter()
            .position(|(op, _)| op.ends_with(":injected"))
            .expect("the injection fired");
        let rest = &ops[at + 1..];
        let end = rest
            .iter()
            .position(|(op, _)| *op == "list")
            .unwrap_or_else(|| panic!("no list after the injection: {:?}", &ops[at..]));
        rest[..=end].to_vec()
    }

    /// Every call after the injected one.
    fn since_injection(&self) -> Vec<(&'static str, String)> {
        let ops = self.ops();
        let at = ops
            .iter()
            .position(|(op, _)| op.ends_with(":injected"))
            .expect("the injection fired");
        ops[at + 1..].to_vec()
    }
}

#[async_trait]
impl KineWriter for Deposer {
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError> {
        match self.fires("create", key) {
            Some(Injection::ThenClaimedBy { claim, owner }) => {
                let created = self.inner.create(key, value).await;
                self.note("create:injected", key);
                self.reassign(&claim, owner).await;
                created
            }
            Some(Injection::LostAnswer) => {
                self.inner.create(key, value).await?;
                self.note("create:injected", key);
                Err(KineError::Unavailable(
                    "injected: the answer was lost".to_string(),
                ))
            }
            Some(other) => panic!("a create cannot be armed with {other:?}"),
            None => {
                self.note_write("create", key).await;
                self.inner.create(key, value).await
            }
        }
    }

    async fn update(&self, key: &str, rev: i64, value: &[u8]) -> Result<WriteOutcome, KineError> {
        let Some(injection) = self.fires("update", key) else {
            self.note_write("update", key).await;
            return self.inner.update(key, rev, value).await;
        };
        if let Injection::ThenClaimedBy { claim, owner } = injection {
            let updated = self.inner.update(key, rev, value).await;
            self.note("update:injected", key);
            self.reassign(&claim, owner).await;
            return updated;
        }
        match injection {
            Injection::LostAnswer | Injection::LostThenUnreadable => {
                self.inner.update(key, rev, value).await?;
                self.note("update:injected", key);
                if let Injection::LostThenUnreadable = injection {
                    *self.blind.lock().expect("lock") = Some(key.to_string());
                }
                return Err(KineError::Unavailable(
                    "injected: the answer was lost".to_string(),
                ));
            }
            Injection::Refused => {
                self.note("update:injected", key);
                return Err(KineError::Unavailable(
                    "injected: the write was refused".to_string(),
                ));
            }
            _ => {}
        }
        self.end_session().await;
        self.note("update:injected", key);
        match injection {
            Injection::Rewritten => self.rewrite_as_new_leader(key).await,
            Injection::Deleted => {
                self.delete_as_new_leader(key).await?;
                Ok(WriteOutcome::Conflict { current: None })
            }
            Injection::Answered
            | Injection::ThenClaimedBy { .. }
            | Injection::LostAnswer
            | Injection::LostThenUnreadable
            | Injection::Refused => self.inner.update(key, rev, value).await,
        }
    }

    async fn delete(&self, key: &str, rev: i64) -> Result<WriteOutcome, KineError> {
        let Some(injection) = self.fires("delete", key) else {
            self.note_write("delete", key).await;
            return self.inner.delete(key, rev).await;
        };
        if let Injection::ThenClaimedBy { claim, owner } = injection {
            let deleted = self.inner.delete(key, rev).await;
            self.note("delete:injected", key);
            self.reassign(&claim, owner).await;
            return deleted;
        }
        self.end_session().await;
        self.note("delete:injected", key);
        match injection {
            Injection::Rewritten => self.rewrite_as_new_leader(key).await,
            Injection::Deleted
            | Injection::Answered
            | Injection::ThenClaimedBy { .. }
            | Injection::LostAnswer
            | Injection::LostThenUnreadable
            | Injection::Refused => self.inner.delete(key, rev).await,
        }
    }

    async fn get(&self, key: &str) -> Result<Option<KeyValue>, KineError> {
        if self.blind.lock().expect("lock").as_deref() == Some(key) {
            self.note("get:blind", key);
            return Err(KineError::Unavailable(
                "injected: the key cannot be read".to_string(),
            ));
        }
        let Some(injection) = self.fires("get", key) else {
            return self.inner.get(key).await;
        };
        if let Injection::ThenClaimedBy { claim, owner } = injection {
            let got = self.inner.get(key).await;
            self.note("get:injected", key);
            self.reassign(&claim, owner).await;
            return got;
        }
        self.end_session().await;
        self.note("get:injected", key);
        match injection {
            Injection::Deleted => {
                self.delete_as_new_leader(key).await?;
                Ok(None)
            }
            Injection::Rewritten
            | Injection::Answered
            | Injection::ThenClaimedBy { .. }
            | Injection::LostAnswer
            | Injection::LostThenUnreadable
            | Injection::Refused => self.inner.get(key).await,
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<(Vec<u8>, KeyValue)>, KineError> {
        let listed = self.inner.list(prefix).await;
        match self.fires("list", prefix) {
            Some(Injection::ThenClaimedBy { claim, owner }) => {
                self.note("list:injected", prefix);
                self.reassign(&claim, owner).await;
            }
            // Answered first: the leader has the list in hand when its
            // session ends.
            Some(_) => {
                self.end_session().await;
                self.note("list:injected", prefix);
            }
            None => self.note("list", prefix),
        }
        listed
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
    let deposer = Deposer::new(&endpoint, db).await;
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    let name = unique("acct");
    let id = create_key(db, env, &name).await;
    all_projected(db).await;
    let key = key_for(env, Kind::ProviderKeys, id);

    deposer.arm("update", &key, Injection::Rewritten);
    let doc = provider_key(&name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Replace(doc))]).await;
    all_projected(db).await;

    // After the conflict, the deposed leader wrote nothing: what comes next
    // is the new tenure, which claims the environment and reconciles.
    let claim = claim_key(env);
    let next: Vec<_> = deposer
        .after_injection()
        .into_iter()
        .filter(|(_, key)| *key != claim)
        .collect();
    assert_eq!(next, [("list", env.env_prefix())]);
    assert_eq!(deposer.deposed(), []);
    // That tenure, the only leader, then writes the row.
    let (value, _) = raw_json(&kine, &key).await.expect("the key is in kine");
    assert_eq!(value, provider_key(&name, ROTATED));
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_deposed_leader_does_not_create_a_deleted_key_again() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let deposer = Deposer::new(&endpoint, db).await;
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    let name = unique("acct");
    let id = create_key(db, env, &name).await;
    all_projected(db).await;
    let key = key_for(env, Kind::ProviderKeys, id);

    // The new leader has deleted the key: its retry would be a create.
    deposer.arm("update", &key, Injection::Deleted);
    let doc = provider_key(&name, ROTATED);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Replace(doc))]).await;
    all_projected(db).await;
    let claim = claim_key(env);
    let next: Vec<_> = deposer
        .after_injection()
        .into_iter()
        .filter(|(_, key)| *key != claim)
        .collect();
    assert_eq!(next, [("list", env.env_prefix())]);
    assert_eq!(deposer.deposed(), []);
    let (value, _) = raw_json(&kine, &key).await.expect("the key is in kine");
    assert_eq!(value, provider_key(&name, ROTATED));
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_deposed_leader_does_not_delete_a_rewritten_key() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let deposer = Deposer::new(&endpoint, db).await;
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    let id = create_key(db, env, &unique("acct")).await;
    all_projected(db).await;
    let key = key_for(env, Kind::ProviderKeys, id);

    // The new leader rewrites the key the deposed one is deleting: its retry
    // would delete the new leader's value.
    deposer.arm("delete", &key, Injection::Rewritten);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Delete)]).await;
    all_projected(db).await;
    let window = deposer.after_injection();
    assert!(!window.iter().any(|(op, _)| *op == "delete"), "{window:?}");
    assert_eq!(deposer.deposed(), []);
    // The next tenure deleted it.
    assert_eq!(raw_get(&kine, &key).await, None);
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_deposed_leader_does_not_delete_what_it_found() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let deposer = Deposer::new(&endpoint, db).await;
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    let id = create_key(db, env, &unique("acct")).await;
    all_projected(db).await;
    let key = key_for(env, Kind::ProviderKeys, id);

    // A deleted row whose key revision was never recorded: the projector
    // reads the key, then deletes it at the revision it read.
    sqlx::query("UPDATE gw_resources SET kine_mod_revision = NULL WHERE id = $1")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("forget the key's revision");
    deposer.arm("get", &key, Injection::Answered);
    write_all(db, env, vec![(Kind::ProviderKeys, id, Op::Delete)]).await;
    all_projected(db).await;
    let window = deposer.after_injection();
    assert!(!window.iter().any(|(op, _)| *op == "delete"), "{window:?}");
    assert_eq!(deposer.deposed(), []);
    // The next tenure deleted it.
    assert_eq!(raw_get(&kine, &key).await, None);
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_deposed_leader_does_not_delete_strays() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    create_key(db, env, &unique("acct")).await;
    let prefix = env.env_prefix();
    let strays = [
        format!("{prefix}models/{}", Uuid::new_v4()),
        format!("{prefix}models/{}", Uuid::new_v4()),
    ];
    for stray in &strays {
        raw_put(&kine, stray, b"{}").await;
    }
    // The tenure's reconcile has the list in hand when its session ends.
    let deposer = Deposer::new(&endpoint, db).await;
    deposer.arm("list", &prefix, Injection::Answered);
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    all_projected(db).await;
    eventually("the next tenure to delete the strays", PATIENCE, || {
        let (kine, strays) = (&kine, &strays);
        async move {
            for stray in strays {
                if raw_get(kine, stray).await.is_some() {
                    return false;
                }
            }
            true
        }
    })
    .await;
    let window = deposer.after_injection();
    assert!(!window.iter().any(|(op, _)| *op == "delete"), "{window:?}");
    assert_eq!(deposer.deposed(), []);
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_deposed_leader_stops_between_two_stray_deletes() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    create_key(db, env, &unique("acct")).await;
    let prefix = env.env_prefix();
    let mut strays = [
        format!("{prefix}models/{}", Uuid::new_v4()),
        format!("{prefix}models/{}", Uuid::new_v4()),
    ];
    strays.sort();
    for stray in &strays {
        raw_put(&kine, stray, b"{}").await;
    }
    // The first delete lands as its session ends; the second must not be
    // sent by the same tenure.
    let deposer = Deposer::new(&endpoint, db).await;
    deposer.arm("delete", &strays[0], Injection::Answered);
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    all_projected(db).await;
    eventually(
        "the next tenure to delete the second stray",
        PATIENCE,
        || {
            let (kine, stray) = (&kine, &strays[1]);
            async move { raw_get(kine, stray).await.is_none() }
        },
    )
    .await;
    let window = deposer.after_injection();
    assert!(!window.iter().any(|(op, _)| *op == "delete"), "{window:?}");
    assert_eq!(deposer.deposed(), []);
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_deposed_leader_does_not_claim_again() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let deposer = Deposer::new(&endpoint, db).await;
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    create_key(db, env, &unique("acct")).await;
    all_projected(db).await;
    let claim = claim_key(env);

    // The pass's read of the claim finds it gone, its session ended.
    deposer.arm("get", &claim, Injection::Deleted);
    eventually("a new tenure after the injection", PATIENCE, || {
        let deposer = &deposer;
        async move {
            let ops = deposer.ops();
            ops.iter()
                .position(|(op, _)| op.ends_with(":injected"))
                .is_some_and(|at| ops[at..].iter().any(|(op, _)| *op == "list"))
        }
    })
    .await;
    // Only the new tenure claimed the environment again.
    assert_eq!(deposer.deposed(), []);
    let window = deposer.after_injection();
    let claims = window
        .iter()
        .filter(|(op, key)| *op == "create" && *key == claim)
        .count();
    assert_eq!(claims, 1, "{window:?}");
    assert!(raw_get(&kine, &claim).await.is_some());
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
    let blocked = json!({
        "reason": "another control plane owns this environment",
        "owner_database_id": setting(first.db(), "database_id").await,
        "database_id": setting(db, "database_id").await,
    });
    eventually("the second to report it is blocked", PATIENCE, || {
        let blocked = &blocked;
        async move { setting(db, "projection_blocked").await.as_ref() == Some(blocked) }
    })
    .await;
    // It never got as far as reconciling.
    assert_eq!(reconcile_audit(db).await, []);
    assert_eq!(setting(db, "reconcile_blocked").await, None);
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

    // What a gateway of this environment reads, by its own prefix formula
    // and through its own loader.
    let env = cp.env();
    let gateway = EtcdConfig {
        prefix: env.etcd_prefix.clone(),
        env_id: env.env_id.to_string(),
        ..EtcdConfig::default()
    };
    let resp = kine
        .kv_client()
        .get(
            gateway.effective_prefix(),
            Some(GetOptions::new().with_prefix()),
        )
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
        WatchedPrefix::environment(gateway.effective_prefix()),
        WatchedPrefix::global(gateway.global_prefix()),
    ]);
    let (snapshot, stats) = build_snapshot(&prefixes, &entries);
    // Every document projected loads, the platform tenant's own included.
    assert_eq!(stats.accepted, entries.len(), "{stats:?}");
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

// ---- fix round 2 -----------------------------------------------------------

#[tokio::test]
async fn a_leader_stops_once_the_claim_names_another_database() {
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
    create_key(db, env, &unique("acct")).await;
    all_projected(db).await;

    // Another database's control plane is given the environment while this
    // one leads.
    let other = Uuid::new_v4();
    let claim = json!({ "database_id": other }).to_string();
    raw_put(&kine, &claim_key(env), claim.as_bytes()).await;
    let blocked = json!({
        "reason": "another control plane owns this environment",
        "owner_database_id": other,
        "database_id": setting(db, "database_id").await,
    });
    // Within about two passes of a second each.
    eventually("the leader to stop", Duration::from_secs(5), || {
        let blocked = &blocked;
        async move { setting(db, "projection_blocked").await.as_ref() == Some(blocked) }
    })
    .await;
    let stopped = Instant::now();
    assert!(lock_holders(db).await.is_empty());
    let b = create_key(db, env, &unique("b")).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!row_state(db, Kind::ProviderKeys, b).await.projected());
    assert_eq!(
        raw_get(&kine, &key_for(env, Kind::ProviderKeys, b)).await,
        None
    );
    assert_eq!(writers_since(&log, stopped), BTreeSet::new());
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

async fn take_over(cp: &TestCp, cookie: &str, from: Value) -> Resp {
    cp.send(
        "POST",
        "/admin/rollout/ownership",
        Some(cookie),
        Some(json!({ "take_over_from": from })),
    )
    .await
}

#[tokio::test]
async fn an_environment_moves_to_another_database_only_by_takeover() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let env = first.env().clone();
    let first_id = setting(first.db(), "database_id").await.expect("an id");
    create_key(first.db(), &env, &unique("a")).await;
    all_projected(first.db()).await;

    let second = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = endpoint.clone();
        cfg.env_id_seed = Some(env.env_id);
    })
    .await
    .expect("the second control plane boots");
    let second_id = setting(second.db(), "database_id").await.expect("an id");
    let db = second.db();
    // The report names the owner to take the environment over from.
    let blocked = json!({
        "reason": "another control plane owns this environment",
        "owner_database_id": first_id,
        "database_id": second_id,
    });
    eventually("the second to report it is blocked", PATIENCE, || {
        let blocked = &blocked;
        async move { setting(db, "projection_blocked").await.as_ref() == Some(blocked) }
    })
    .await;
    let b = create_key(db, &env, &unique("b")).await;
    let admin = second.admin_login_seeded(OperatorRole::Admin).await;

    // Naming another database moves nothing.
    let r = take_over(&second, admin.cookie(), json!(Uuid::new_v4())).await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "owned_elsewhere");
    let (claim, _) = raw_json(&kine, &claim_key(&env)).await.expect("the claim");
    assert_eq!(claim["database_id"], first_id);
    assert!(!audit_actions(db)
        .await
        .iter()
        .any(|action| action.starts_with("rollout.ownership")));

    // Naming the owner moves the environment.
    let r = take_over(&second, admin.cookie(), first_id.clone()).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({ "owner_database_id": second_id }));
    let (claim, _) = raw_json(&kine, &claim_key(&env)).await.expect("the claim");
    assert_eq!(claim["database_id"], second_id);
    let audit: (Option<String>, Value) = sqlx::query_as(
        "SELECT actor_email, detail FROM audit_log WHERE action = 'rollout.ownership.taken_over'",
    )
    .fetch_one(db.pool())
    .await
    .expect("the takeover is audited");
    assert_eq!(audit.0.as_deref(), Some(admin.email.as_str()));
    assert_eq!(
        audit.1,
        json!({
            "from_database_id": first_id,
            "requested_from": first_id,
            "to_database_id": second_id,
        })
    );

    // The new owner projects; the old one, still running, stops.
    all_projected(db).await;
    assert!(raw_get(&kine, &key_for(&env, Kind::ProviderKeys, b))
        .await
        .is_some());
    let old_blocked = json!({
        "reason": "another control plane owns this environment",
        "owner_database_id": second_id,
        "database_id": first_id,
    });
    let first_db = first.db();
    eventually("the old owner to stop", PATIENCE, || {
        let old_blocked = &old_blocked;
        async move { setting(first_db, "projection_blocked").await.as_ref() == Some(old_blocked) }
    })
    .await;
    let c = create_key(first_db, &env, &unique("c")).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!row_state(first_db, Kind::ProviderKeys, c).await.projected());
    assert_eq!(
        raw_get(&kine, &key_for(&env, Kind::ProviderKeys, c)).await,
        None
    );
}

/// Every audit action, oldest first.
async fn audit_actions(db: &Db) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM audit_log ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("read the audit log")
}

#[tokio::test]
async fn the_lock_session_keeps_tcp_keepalives() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let setting_of =
        |name: &'static str| format!("SELECT setting FROM pg_settings WHERE name = '{name}'");
    // Over TCP, as every test connection is: keepalives apply to TCP only.
    let mut lock = projector::lock_session(&bare.db)
        .await
        .expect("a lock session");
    for (name, expected) in [
        ("tcp_keepalives_idle", "5"),
        ("tcp_keepalives_interval", "5"),
        ("tcp_keepalives_count", "3"),
        ("tcp_user_timeout", "15000"),
    ] {
        let value: String = sqlx::query_scalar(&setting_of(name))
            .fetch_one(&mut lock)
            .await
            .expect("read a setting");
        assert_eq!(value, expected, "{name}");
    }
    // A pool session keeps the server's own settings.
    let pooled: String = sqlx::query_scalar(&setting_of("tcp_keepalives_idle"))
        .fetch_one(bare.db.pool())
        .await
        .expect("read a setting");
    assert_ne!(pooled, "5");
    let _ = lock.close().await;
}

#[tokio::test]
async fn the_one_off_passes_refuse_while_the_claim_names_another_database() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let kine = MemKine::default();
    create_key(db, env, &unique("acct")).await;
    let claim = json!({ "database_id": Uuid::new_v4() }).to_string();
    kine.put_raw(claim_key(env).as_bytes(), claim.as_bytes());
    let projected = projector::project_once(db, &kine, &ctx(env)).await;
    assert!(
        matches!(
            projected,
            Err(projector::ProjectError::OwnedElsewhere { .. })
        ),
        "{projected:?}"
    );
    let reconciled = projector::reconcile(db, &kine, &ctx(env), true).await;
    assert!(
        matches!(
            reconciled,
            Err(projector::ProjectError::OwnedElsewhere { .. })
        ),
        "{reconciled:?}"
    );
    assert!(kine.log().is_empty(), "{:?}", kine.log());
}

// ---- fix round 3 -----------------------------------------------------------

/// `setting`'s `owner_database_id`, once `projection_blocked` is set.
async fn blocked_by(db: &Db) -> Option<Value> {
    setting(db, "projection_blocked")
        .await
        .map(|blocked| blocked["owner_database_id"].clone())
}

#[tokio::test]
async fn a_blocked_control_plane_does_not_claim_a_vanished_claim() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    // Another database owns the environment.
    let other = Uuid::new_v4();
    let claim = claim_key(env);
    let value = json!({ "database_id": other }).to_string();
    raw_put(&kine, &claim, value.as_bytes()).await;
    let timing = Timing {
        owned_elsewhere_retry: Duration::from_secs(1),
        ..Timing::default()
    };
    let log = Arc::new(Mutex::new(Vec::<Write>::new()));
    let began = Instant::now();
    let (stop, task) = spawn_recorded_with("a", &bare, &endpoint, &log, timing).await;
    eventually("the control plane to be blocked", PATIENCE, || async move {
        blocked_by(db).await == Some(json!(other))
    })
    .await;

    // The claim vanishes, as if removed by hand: across several retries,
    // the blocked control plane does not take it.
    raw_delete(&kine, &claim).await;
    let id = create_key(db, env, &unique("acct")).await;
    eventually("the block to name no owner", PATIENCE, || async move {
        blocked_by(db).await == Some(Value::Null)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        raw_get(&kine, &claim).await,
        None,
        "it claimed the environment"
    );
    assert_eq!(blocked_by(db).await, Some(Value::Null));
    assert!(!row_state(db, Kind::ProviderKeys, id).await.projected());
    assert_eq!(writers_since(&log, began), BTreeSet::new());
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_takeover_of_a_vanished_claim_is_audited() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let env = first.env().clone();
    let first_id = setting(first.db(), "database_id").await.expect("an id");
    create_key(first.db(), &env, &unique("a")).await;
    all_projected(first.db()).await;
    let second = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = endpoint.clone();
        cfg.env_id_seed = Some(env.env_id);
    })
    .await
    .expect("the second control plane boots");
    let second_id = setting(second.db(), "database_id").await.expect("an id");
    let db = second.db();
    eventually("the second to be blocked", PATIENCE, || {
        let first_id = &first_id;
        async move { blocked_by(db).await.as_ref() == Some(first_id) }
    })
    .await;
    let b = create_key(db, &env, &unique("b")).await;
    // The old owner is stopped, and its claim removed by hand.
    drop(first);
    let claim = claim_key(&env);
    raw_delete(&kine, &claim).await;
    let admin = second.admin_login_seeded(OperatorRole::Admin).await;

    // A takeover from this database itself is refused.
    let r = take_over(&second, admin.cookie(), second_id.clone()).await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "owned_elsewhere");
    assert_eq!(raw_get(&kine, &claim).await, None);

    // An absent claim is taken by the takeover, and audited.
    let r = take_over(&second, admin.cookie(), first_id.clone()).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, json!({ "owner_database_id": second_id }));
    let (value, _) = raw_json(&kine, &claim).await.expect("the claim");
    assert_eq!(value, json!({ "database_id": second_id }));
    let audit: Vec<Value> = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE action = 'rollout.ownership.taken_over'",
    )
    .fetch_all(db.pool())
    .await
    .expect("read the audit log");
    assert_eq!(
        audit,
        [json!({
            "from_database_id": null,
            "requested_from": first_id,
            "to_database_id": second_id,
        })]
    );
    all_projected(db).await;
    assert!(raw_get(&kine, &key_for(&env, Kind::ProviderKeys, b))
        .await
        .is_some());
}

#[tokio::test]
async fn a_takeover_moves_the_claim_only_from_the_owner_it_read() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let env = first.env().clone();
    let first_id: Uuid = setting(first.db(), "database_id")
        .await
        .and_then(|id| id.as_str().and_then(|id| id.parse().ok()))
        .expect("an id");
    all_projected(first.db()).await;
    let second = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = endpoint.clone();
        cfg.env_id_seed = Some(env.env_id);
    })
    .await
    .expect("the second control plane boots");
    let db = second.db();
    eventually("the second to be blocked", PATIENCE, || async move {
        blocked_by(db).await == Some(json!(first_id))
    })
    .await;

    // A third database takes the environment just after the takeover has
    // read the claim.
    let claim = claim_key(&env);
    let third = Uuid::new_v4();
    let deposer = Deposer::new(&endpoint, db).await;
    deposer.arm(
        "get",
        &claim,
        Injection::ThenClaimedBy {
            claim: claim.clone(),
            owner: third,
        },
    );
    let admin = OperatorAuth {
        id: Uuid::new_v4(),
        email: "takeover@ops.test".to_string(),
        role: OperatorRole::Admin,
    };
    let taken =
        projector::take_over_with(db, &*deposer, &ctx(second.env()), &admin, first_id).await;
    let refused = taken.expect_err("the claim changed under the takeover");
    assert_eq!(
        (refused.status.as_u16(), refused.code),
        (409, "owned_elsewhere")
    );
    let (value, _) = raw_json(&kine, &claim).await.expect("the claim");
    assert_eq!(value, json!({ "database_id": third }));
    // Nothing of the takeover was kept: no audit row, still blocked.
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'rollout.ownership.taken_over'",
    )
    .fetch_one(db.pool())
    .await
    .expect("read the audit log");
    assert_eq!(audited, 0);
    assert_eq!(blocked_by(db).await, Some(json!(first_id)));
}

#[tokio::test]
async fn an_old_owner_stops_within_a_batch_of_losing_the_environment() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    let deposer = Deposer::new(&endpoint, db).await;
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    create_key(db, env, &unique("first")).await;
    all_projected(db).await;

    // Three batches' worth, in one transaction; another database takes the
    // environment as the first batch's last row is written.
    let ids: Vec<Uuid> = (0..130).map(|_| Uuid::new_v4()).collect();
    let keys: Vec<String> = ids
        .iter()
        .map(|id| key_for(env, Kind::ProviderKeys, *id))
        .collect();
    let third = Uuid::new_v4();
    deposer.arm(
        "create",
        &keys[63],
        Injection::ThenClaimedBy {
            claim: claim_key(env),
            owner: third,
        },
    );
    let writes = ids
        .iter()
        .map(|id| {
            let doc = provider_key(&unique("bulk"), SECRET);
            (Kind::ProviderKeys, *id, Op::Create(doc))
        })
        .collect();
    write_all(db, env, writes).await;
    eventually("the old owner to stop", PATIENCE, || async move {
        blocked_by(db).await == Some(json!(third))
    })
    .await;
    let later: BTreeSet<&String> = keys[64..].iter().collect();
    let sent: Vec<_> = deposer
        .since_injection()
        .into_iter()
        .filter(|(_, key)| later.contains(key))
        .collect();
    assert_eq!(sent, [], "written after the environment moved");
    // The first row and the first batch.
    assert_eq!(raw_keys(&kine, &env.env_prefix()).await.len(), 65);
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

#[tokio::test]
async fn a_reconcile_does_not_delete_by_a_list_older_than_a_takeover() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let kine = raw_kine(&endpoint).await;
    let (db, env) = (&bare.db, &bare.env);
    create_key(db, env, &unique("acct")).await;
    let prefix = env.env_prefix();
    let strays = [
        format!("{prefix}models/{}", Uuid::new_v4()),
        format!("{prefix}models/{}", Uuid::new_v4()),
    ];
    for stray in &strays {
        raw_put(&kine, stray, b"{}").await;
    }
    // Another database takes the environment just after the tenure's
    // reconcile has listed it.
    let third = Uuid::new_v4();
    let deposer = Deposer::new(&endpoint, db).await;
    deposer.arm(
        "list",
        &prefix,
        Injection::ThenClaimedBy {
            claim: claim_key(env),
            owner: third,
        },
    );
    let (stop, task) = spawn_on(&bare, deposer.clone(), Timing::default());
    eventually("the old owner to stop", PATIENCE, || async move {
        blocked_by(db).await == Some(json!(third))
    })
    .await;
    for stray in &strays {
        assert!(raw_get(&kine, stray).await.is_some(), "{stray} was deleted");
    }
    let since = deposer.since_injection();
    assert!(!since.iter().any(|(op, _)| *op == "delete"), "{since:?}");
    assert_eq!(reconcile_audit(db).await, []);
    let _ = stop.send(());
    task.await.expect("the projector stops");
}

// ---- with the tenant and gateway tasks ---------------------------------------

/// The stored document of `kind`/`id`.
async fn stored_doc(db: &Db, kind: Kind, id: Uuid) -> Value {
    sqlx::query_scalar("SELECT doc FROM gw_resources WHERE kind = $1 AND id = $2")
        .bind(kind.segment())
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("load the stored document")
}

#[tokio::test]
async fn an_approved_tenant_reaches_kine_as_its_team_and_limits() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let kine = raw_kine(&support::kine_endpoint().expect("kine is configured")).await;
    let org = unique("acme");
    let email = format!("owner-{}@tenant.test", Uuid::new_v4().simple());
    let r = cp
        .send(
            "POST",
            "/api/signup",
            None,
            Some(json!({"org_name": org, "email": email, "password": support::TEST_PASSWORD})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let tenant: Uuid = r.body["tenant"]["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .expect("a tenant id");
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/approve"),
            Some(op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    all_projected(cp.db()).await;

    // The team, named after the tenant, and the policy that limits it, as
    // they are stored.
    let env = cp.env();
    let (team, _) = raw_json(&kine, &key_for(env, Kind::Teams, tenant))
        .await
        .expect("the team is in kine");
    assert_eq!(team["name"], org);
    assert_eq!(team, stored_doc(cp.db(), Kind::Teams, tenant).await);
    let policy: Option<Uuid> =
        sqlx::query_scalar("SELECT limits_policy_id FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the tenant");
    let policy = policy.expect("an approved tenant has a limits policy");
    let (limits, _) = raw_json(&kine, &key_for(env, Kind::RateLimitPolicies, policy))
        .await
        .expect("the limits are in kine");
    assert_eq!(
        limits,
        stored_doc(cp.db(), Kind::RateLimitPolicies, policy).await
    );
    // Its documents no longer wait for the projector.
    let r = cp
        .get_with_cookie(&format!("/admin/tenants/{tenant}"), op.cookie())
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    // No gateway runs here, so nothing can be further along.
    assert_eq!(r.body["rollout"]["state"], "no_gateways", "{r:?}");
}

#[tokio::test]
async fn a_projected_tombstone_is_pruned_and_an_unprojected_one_is_kept() {
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
    let projected = create_key(db, env, &unique("a")).await;
    let pending = create_key(db, env, &unique("b")).await;
    all_projected(db).await;
    // One delete reaches kine; the other is made while no projector runs.
    write_all(db, env, vec![(Kind::ProviderKeys, projected, Op::Delete)]).await;
    all_projected(db).await;
    let _ = stop.send(());
    task.await.expect("the projector stops");
    write_all(db, env, vec![(Kind::ProviderKeys, pending, Op::Delete)]).await;

    let pruned = housekeeping::prune_tombstones(db, Duration::ZERO)
        .await
        .expect("prune the tombstones");
    assert_eq!(pruned, 1);
    let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM gw_resources ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("list the rows");
    assert_eq!(left, [pending]);
    assert!(!row_state(db, Kind::ProviderKeys, pending).await.projected());
    // Kept until its delete reaches kine, where its key still is.
    assert_eq!(
        raw_get(&kine, &key_for(env, Kind::ProviderKeys, projected)).await,
        None
    );
    assert!(raw_get(&kine, &key_for(env, Kind::ProviderKeys, pending))
        .await
        .is_some());
}

#[tokio::test]
async fn a_takeover_whose_answer_was_lost_is_still_audited() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let env = first.env().clone();
    let first_id: Uuid = setting(first.db(), "database_id")
        .await
        .and_then(|id| id.as_str().and_then(|id| id.parse().ok()))
        .expect("an id");
    all_projected(first.db()).await;
    let second = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = endpoint.clone();
        cfg.env_id_seed = Some(env.env_id);
    })
    .await
    .expect("the second control plane boots");
    let second_id = setting(second.db(), "database_id").await.expect("an id");
    let db = second.db();
    eventually("the second to be blocked", PATIENCE, || async move {
        blocked_by(db).await == Some(json!(first_id))
    })
    .await;
    drop(first);

    // The claim moves, and the answer saying so never arrives.
    let claim = claim_key(&env);
    let deposer = Deposer::new(&endpoint, db).await;
    deposer.arm("update", &claim, Injection::LostAnswer);
    let admin = OperatorAuth {
        id: Uuid::new_v4(),
        email: "takeover@ops.test".to_string(),
        role: OperatorRole::Admin,
    };
    let taken = projector::take_over_with(db, &*deposer, &ctx(second.env()), &admin, first_id)
        .await
        .expect("the takeover is reported done");
    assert_eq!(taken, json!({ "owner_database_id": second_id }));
    let (value, _) = raw_json(&kine, &claim).await.expect("the claim");
    assert_eq!(value, json!({ "database_id": second_id }));
    let audit: Vec<Value> = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE action = 'rollout.ownership.taken_over'",
    )
    .fetch_all(db.pool())
    .await
    .expect("read the audit log");
    assert_eq!(
        audit,
        [json!({
            "from_database_id": first_id,
            "requested_from": first_id,
            "to_database_id": second_id,
        })]
    );
    assert_eq!(blocked_by(db).await, None);
}

// ---- integration follow-ups ------------------------------------------------

/// Two control planes on one environment: the first owns it, the second is
/// blocked and about to take it over. The second's database, the first's
/// id, and the second's.
async fn a_blocked_second(first: &TestCp, endpoint: &str) -> (TestCp, Uuid, Value) {
    let env = first.env().clone();
    let first_id: Uuid = setting(first.db(), "database_id")
        .await
        .and_then(|id| id.as_str().and_then(|id| id.parse().ok()))
        .expect("an id");
    all_projected(first.db()).await;
    let endpoint = endpoint.to_string();
    let second = TestCp::start_with(|cfg| {
        cfg.kine_endpoint = endpoint;
        cfg.env_id_seed = Some(env.env_id);
    })
    .await
    .expect("the second control plane boots");
    let second_id = setting(second.db(), "database_id").await.expect("an id");
    let db = second.db();
    eventually("the second to be blocked", PATIENCE, || async move {
        blocked_by(db).await == Some(json!(first_id))
    })
    .await;
    (second, first_id, second_id)
}

fn an_admin() -> OperatorAuth {
    OperatorAuth {
        id: Uuid::new_v4(),
        email: "takeover@ops.test".to_string(),
        role: OperatorRole::Admin,
    }
}

async fn takeover_rows(db: &Db) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'rollout.ownership.taken_over'",
    )
    .fetch_one(db.pool())
    .await
    .expect("read the audit log")
}

#[tokio::test]
async fn a_takeover_whose_outcome_cannot_be_confirmed_says_so() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let (second, first_id, second_id) = a_blocked_second(&first, &endpoint).await;
    drop(first);
    let db = second.db();

    // The claim moves, the answer is lost, and the claim cannot be read back.
    let claim = claim_key(second.env());
    let deposer = Deposer::new(&endpoint, db).await;
    deposer.arm("update", &claim, Injection::LostThenUnreadable);
    let taken =
        projector::take_over_with(db, &*deposer, &ctx(second.env()), &an_admin(), first_id).await;
    let refused = taken.expect_err("the outcome is unknown");
    assert_eq!(
        (refused.status.as_u16(), refused.code),
        (500, "takeover_unconfirmed")
    );
    // It read the claim again before giving up.
    let reads = deposer
        .ops()
        .iter()
        .filter(|(op, _)| *op == "get:blind")
        .count();
    assert_eq!(reads, 3);
    // The known gap, as it stands: the claim moved, and no audit row says so.
    let (value, _) = raw_json(&kine, &claim).await.expect("the claim");
    assert_eq!(value, json!({ "database_id": second_id }));
    assert_eq!(takeover_rows(db).await, 0);
}

#[tokio::test]
async fn a_takeover_whose_write_was_refused_moves_nothing() {
    let Some(first) = TestCp::start_with_kine().await else {
        return;
    };
    let endpoint = support::kine_endpoint().expect("kine is configured");
    let kine = raw_kine(&endpoint).await;
    let (second, first_id, _) = a_blocked_second(&first, &endpoint).await;
    let db = second.db();

    // The write never lands, and the call fails as a lost answer would.
    let claim = claim_key(second.env());
    let deposer = Deposer::new(&endpoint, db).await;
    deposer.arm("update", &claim, Injection::Refused);
    let taken =
        projector::take_over_with(db, &*deposer, &ctx(second.env()), &an_admin(), first_id).await;
    let refused = taken.expect_err("nothing moved");
    assert_eq!(
        (refused.status.as_u16(), refused.code),
        (503, "unavailable")
    );
    let (value, _) = raw_json(&kine, &claim).await.expect("the claim");
    assert_eq!(value, json!({ "database_id": first_id }));
    assert_eq!(takeover_rows(db).await, 0);
    assert_eq!(blocked_by(db).await, Some(json!(first_id)));
}

#[tokio::test]
async fn shutdown_frees_the_projector_lock() {
    let Some(mut cp) = support::StoppableCp::start_with_kine().await else {
        return;
    };
    let db = cp.db().clone();
    eventually("this instance to lead", PATIENCE, || {
        let db = &db;
        async move { lock_holders(db).await.len() == 1 }
    })
    .await;

    let took = cp
        .shut_down(Duration::from_secs(10))
        .await
        .expect("wait() returns within seconds of the shutdown");
    assert!(took < Duration::from_secs(10), "{took:?}");
    // Another session takes the lock, as another instance would.
    let url = cp.database_url().to_string();
    eventually("the lock to be free", Duration::from_secs(5), || {
        let url = url.clone();
        async move {
            let Ok(mut other) = PgConnection::connect(&url).await else {
                return false;
            };
            let won: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(PROJECTOR_LOCK_KEY)
                .fetch_one(&mut other)
                .await
                .unwrap_or(false);
            let _ = other.close().await;
            won
        }
    })
    .await;
}

/// A kine whose ranges are not byte ranges, as under a table collation
/// other than `COLLATE "C"`: a list answers every key it holds, whatever
/// the prefix.
#[derive(Default)]
struct CollatingKine(MemKine);

#[async_trait]
impl KineWriter for CollatingKine {
    async fn create(&self, key: &str, value: &[u8]) -> Result<WriteOutcome, KineError> {
        self.0.create(key, value).await
    }

    async fn update(&self, key: &str, rev: i64, value: &[u8]) -> Result<WriteOutcome, KineError> {
        self.0.update(key, rev, value).await
    }

    async fn delete(&self, key: &str, rev: i64) -> Result<WriteOutcome, KineError> {
        self.0.delete(key, rev).await
    }

    async fn get(&self, key: &str) -> Result<Option<KeyValue>, KineError> {
        self.0.get(key).await
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<(Vec<u8>, KeyValue)>, KineError> {
        self.0.list("").await
    }
}

#[tokio::test]
async fn reconcile_never_takes_a_key_outside_its_prefix_for_a_stray() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let (db, env) = (&bare.db, &bare.env);
    let kine = CollatingKine::default();
    create_key(db, env, &unique("acct")).await;
    projector::project_once(db, &kine, &ctx(env))
        .await
        .expect("projected");
    let prefix = env.env_prefix();
    // Another environment's document, and one of a sibling whose name
    // starts with this environment's id: both inside the range under a
    // collation that is not a byte order.
    let foreign = [
        format!("/aisix/{}/models/{}", Uuid::new_v4(), Uuid::new_v4()),
        format!("/aisix/{}x/models/{}", env.env_id, Uuid::new_v4()),
    ];
    for key in &foreign {
        kine.0.put_raw(key.as_bytes(), b"{}");
    }
    let stray = format!("{prefix}models/{}", Uuid::new_v4());
    kine.0.put_raw(stray.as_bytes(), b"{}");

    let report = projector::reconcile(db, &kine, &ctx(env), false)
        .await
        .expect("reconciled");
    assert_eq!((report.deleted_strays, report.blocked), (1, false));
    assert!(kine.0.value(&stray).is_none(), "the stray was kept");
    for key in &foreign {
        assert!(kine.0.value(key).is_some(), "{key} was deleted");
    }
}
