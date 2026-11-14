//! 真实 SQLite outbox、真实 CP/Postgres 与 mTLS 的交付确认；不访问或模拟模型提供商。

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use aisix_core::native_objects::*;
use aisix_core::resource::ResourceEntry;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey};
use aisix_obs::usage::{DurableUsageEvent, DurableUsageRequest};
use aisix_obs::UsageSink;
use aisix_proxy::native_api::registry::NativeObjectRegistry;
use aisix_proxy::native_api::standalone_store::{run_outbox, StandaloneNativeStore};
use chrono::Utc;
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use support::world::{price, seed_model, world, World};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const SECRET: [u8; 32] = [47; 32];

struct Terminal {
    _dir: tempfile::TempDir,
    store: Arc<StandaloneNativeStore>,
    database: SqlitePool,
    path: std::path::PathBuf,
    event_id: String,
    canonical: String,
    submission: BeginRequest,
}

impl Terminal {
    async fn new(w: &World) -> Self {
        let model = seed_model(&w.cp, &w.admin).await;
        price(&w.cp, model).await;
        let model_response =
            w.cp.send(
                "GET",
                &format!("/admin/resources/models/{model}"),
                Some(w.admin.cookie()),
                None,
            )
            .await;
        assert_eq!(model_response.status, 200, "{model_response:?}");
        let provider = Uuid::parse_str(
            model_response.body["doc"]["provider_key_id"]
                .as_str()
                .expect("the configured account id"),
        )
        .unwrap();
        let provider_doc: ProviderKey = serde_json::from_value(json!({
            "display_name":"durable-test-account", "provider":"openai", "adapter":"openai",
            "api_key":"sk-pricing-upstream-7c1", "api_base":"https://api.openai.com/v1"
        }))
        .unwrap();
        let snapshot = AisixSnapshot::new();
        snapshot.provider_keys.insert(ResourceEntry::new(
            provider.to_string(),
            provider_doc.clone(),
            1,
        ));
        snapshot.models.insert(ResourceEntry::new(
            model.to_string(),
            serde_json::from_value::<Model>(json!({
                "display_name":"durable-test-model", "provider":"openai",
                "provider_key_id":provider, "model_name":"gpt-4o-mini"
            }))
            .unwrap(),
            1,
        ));
        snapshot.apikeys.insert(ResourceEntry::new(
            w.key.clone(),
            serde_json::from_value::<ApiKey>(json!({
                "key_hash":"durable-test-hash", "allowed_models":["*"],
                "team_id":w.tenant, "user_id":w.member
            }))
            .unwrap(),
            1,
        ));
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("authority.sqlite3");
        let store = Arc::new(
            StandaloneNativeStore::open(&path, SECRET, SnapshotHandle::new(snapshot))
                .await
                .unwrap(),
        );
        let submission: BeginRequest = serde_json::from_value(json!({
            "operation_id":Uuid::new_v4(), "api_key_id":w.key,
            "protocol":"openai", "kind":"response", "asynchronous":true,
            "requested_model_id":model, "requested_model":"durable-test-model",
            "upstream_model":"gpt-4o-mini", "upstream_provider":"openai",
            "operation":"responses", "request_id":Uuid::new_v4().to_string(),
            "origin":{
                "model_id":model, "provider_key_id":provider,
                "boundary":configured_native_origin(&SECRET,provider,&provider_doc,NativeProtocol::Openai,NativeObjectKind::Response).unwrap()
            }
        }))
        .unwrap();
        let record_id = create_terminal(&store, &submission).await;
        let database = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        let (event_id, canonical): (String, String) =
            sqlx::query_as("SELECT event_id,event FROM native_terminal_outbox WHERE record_id=?")
                .bind(record_id.to_string())
                .fetch_one(&database)
                .await
                .unwrap();
        Self {
            _dir: dir,
            store,
            database,
            path,
            event_id,
            canonical,
            submission,
        }
    }

    async fn next_terminal(&self) -> String {
        let mut submission = self.submission.clone();
        submission.operation_id = Uuid::new_v4();
        submission.request_id = Uuid::new_v4().to_string();
        let record_id = create_terminal(&self.store, &submission).await;
        sqlx::query_scalar("SELECT event_id FROM native_terminal_outbox WHERE record_id=?")
            .bind(record_id.to_string())
            .fetch_one(&self.database)
            .await
            .unwrap()
    }

    fn event(&self) -> DurableUsageEvent {
        DurableUsageEvent::from_saved_json(&self.event_id, self.canonical.clone()).unwrap()
    }

    async fn delivered(&self) -> i64 {
        sqlx::query_scalar("SELECT delivered FROM native_terminal_outbox WHERE event_id=?")
            .bind(&self.event_id)
            .fetch_one(&self.database)
            .await
            .unwrap()
    }

    async fn expire_claim(&self) {
        // 只推进测试数据库的租约时钟；不改事件、归属或交付状态。
        sqlx::query("UPDATE native_terminal_outbox SET delivery_until=0 WHERE event_id=?")
            .bind(&self.event_id)
            .execute(&self.database)
            .await
            .unwrap();
    }

    async fn claimed(&self, event_id: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let generation: i64 = sqlx::query_scalar(
                    "SELECT delivery_generation FROM native_terminal_outbox WHERE event_id=?",
                )
                .bind(event_id)
                .fetch_one(&self.database)
                .await
                .unwrap();
                if generation > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the real outbox runner must claim the stored terminal");
    }
}

async fn create_terminal(store: &StandaloneNativeStore, submission: &BeginRequest) -> Uuid {
    store.begin(submission.clone()).await.unwrap();
    let record = store
        .bind(BindRequest {
            operation_id: submission.operation_id,
            api_key_id: submission.api_key_id,
            raw_id: format!("resp_durable_{}", Uuid::new_v4().simple()),
            existing_record_id: None,
            provider_created_at: None,
            provider_expires_at: None,
            objects: vec![],
        })
        .await
        .unwrap()
        .record
        .unwrap();
    let leased = store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    store
        .complete(CompleteRequest {
            lease: LeaseToken {
                record_id: record.record_id,
                generation: leased.lease_generation.unwrap(),
            },
            terminal_status: "completed".into(),
            usage: vec![serde_json::from_value(json!({
                "entry_id":"", "occurred_at":Utc::now(), "status_code":200,
                "prompt_tokens":1000, "cached_prompt_tokens":800,
                "completion_tokens":200, "total_tokens":1200
            }))
            .unwrap()],
            output_file_ids: vec![],
        })
        .await
        .unwrap();
    record.record_id
}

fn proxy(sink: UsageSink) -> aisix_proxy::ProxyState {
    let cfg = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
    aisix_proxy::ProxyState::new(
        SnapshotHandle::new(AisixSnapshot::new()),
        Arc::new(aisix_gateway::Hub::new()),
        &cfg,
    )
    .without_cache()
    .with_usage_sink(sink)
}

async fn stop_runner(stop: watch::Sender<bool>, runner: tokio::task::JoinHandle<()>) {
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("the outbox runner finishes its attempted deliveries")
        .unwrap();
}

async fn receive(rx: &mut mpsc::Receiver<DurableUsageRequest>) -> DurableUsageRequest {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a real delivery reaches the durable worker")
        .expect("the durable queue remains live")
}

#[tokio::test]
async fn real_outbox_ack_requires_a_committed_mtls_receipt_and_replay_charges_once() {
    let Some(w) = world().await else { return };
    let f = Terminal::new(&w).await;
    let bundle = w.cp.issue_bundle(&w.admin, "durable-confirmation").await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .identity(
            reqwest::Identity::from_pem(
                format!("{}\n{}", bundle.key_pem, bundle.cert_pem).as_bytes(),
            )
            .unwrap(),
        )
        .add_root_certificate(reqwest::Certificate::from_pem(bundle.ca_pem.as_bytes()).unwrap())
        .resolve("localhost", w.cp.dp_addr())
        .http1_only()
        .use_rustls_tls()
        .build()
        .unwrap();
    let url = format!("https://localhost:{}/dp/telemetry", w.cp.dp_addr().port());
    let (tx, mut rx) = mpsc::channel(1);
    let sink = UsageSink::disabled().with_durable_sender(tx);

    // CP 已收到、进程还没确认本地 outbox 的崩溃边界：恢复必须重放原身份和原字节。
    let old_claim = f.store.pending_terminals(1).await.unwrap().remove(0);
    let first_sink = sink.clone();
    let event = f.event();
    let first = tokio::spawn(async move { first_sink.emit_durable(event).await });
    receive(&mut rx).await.deliver(&client, &url).await;
    first.await.unwrap().unwrap();
    assert_eq!(f.delivered().await, 0);
    f.store.close().await;
    f.expire_claim().await;
    let restarted = Arc::new(
        StandaloneNativeStore::open(&f.path, SECRET, SnapshotHandle::new(AisixSnapshot::new()))
            .await
            .unwrap(),
    );
    let (stop, cancel) = watch::channel(false);
    let runner = tokio::spawn(run_outbox(
        Some(restarted.clone()),
        proxy(sink.clone()),
        cancel,
    ));
    let replay = receive(&mut rx).await;
    assert_eq!(f.delivered().await, 0, "enqueue alone must not acknowledge");
    assert!(restarted
        .acknowledge_terminal_claim(&old_claim)
        .await
        .is_err());
    replay.deliver(&client, &url).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.delivered().await != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("only the confirmed generation acknowledges the durable row");
    stop_runner(stop, runner).await;

    // 原批次已回 0/0/0；再换一个批次也只能命中永久 event_id，而不是再记一笔账。
    let cp_client = w.cp.dp_client(&bundle);
    let duplicate = cp_client
        .telemetry(
            Uuid::new_v4(),
            &json!({"events":[serde_json::from_str::<serde_json::Value>(&f.canonical).unwrap()]}),
        )
        .await;
    assert_eq!(duplicate.status, 200, "{duplicate:?}");
    assert_eq!(duplicate.body["duplicates"].as_u64(), Some(1));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_events WHERE event_id=$1")
        .bind(&f.event_id)
        .fetch_one(w.cp.db().pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
    let charge: (i64, i64) = sqlx::query_as(
        "SELECT count(*)::bigint,COALESCE(sum(-amount_micro_cny),0)::bigint \
         FROM billing_ledger_cny WHERE event_id=$1 AND tenant_id=$2",
    )
    .bind(&f.event_id)
    .bind(w.tenant)
    .fetch_one(w.cp.db().pool())
    .await
    .unwrap();
    assert_eq!(charge, (1, 680));
    let retained: (String, i64) =
        sqlx::query_as("SELECT event,delivered FROM native_terminal_outbox WHERE event_id=?")
            .bind(&f.event_id)
            .fetch_one(&f.database)
            .await
            .unwrap();
    assert_eq!(retained, (f.canonical.clone(), 1));
    restarted.close().await;
    f.database.close().await;
}

#[tokio::test]
async fn real_outbox_retains_disabled_full_closed_and_aborted_deliveries() {
    let Some(w) = world().await else { return };
    for mode in ["disabled", "full", "closed", "aborted"] {
        let f = Terminal::new(&w).await;
        let (tx, mut rx) = mpsc::channel(1);
        let mut filler = None;
        let sink = if mode == "disabled" {
            UsageSink::disabled()
        } else {
            UsageSink::disabled().with_durable_sender(tx.clone())
        };
        if mode == "full" {
            let sink = sink.clone();
            let event = f.event();
            filler = Some(tokio::spawn(async move { sink.emit_durable(event).await }));
            tokio::time::timeout(Duration::from_secs(5), async {
                while tx.capacity() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        if mode == "closed" {
            rx.close();
        }
        let (stop, cancel) = watch::channel(false);
        let runner = tokio::spawn(run_outbox(Some(f.store.clone()), proxy(sink), cancel));
        if mode == "aborted" {
            drop(receive(&mut rx).await);
        } else {
            f.claimed(&f.event_id).await;
        }
        // 新终态只会在下一轮被 claim，证明上一轮发送已完整执行，避免取消先赢造成假绿。
        let next = f.next_terminal().await;
        f.claimed(&next).await;
        stop_runner(stop, runner).await;
        if let Some(filler) = filler {
            filler.abort();
        }
        assert_eq!(f.delivered().await, 0, "{mode} must retain the durable row");
        f.store.close().await;
        f.expire_claim().await;
        let restarted =
            StandaloneNativeStore::open(&f.path, SECRET, SnapshotHandle::new(AisixSnapshot::new()))
                .await
                .unwrap();
        let pending = restarted.pending_terminals(100).await.unwrap();
        assert!(
            pending
                .iter()
                .any(|entry| entry.event_id.to_string() == f.event_id),
            "{mode} is reclaimable after restart"
        );
        let first = pending
            .iter()
            .find(|entry| entry.event_id.to_string() == f.event_id)
            .unwrap();
        assert_eq!(first.canonical_event, f.canonical);
        restarted.close().await;
        f.database.close().await;
    }
}
