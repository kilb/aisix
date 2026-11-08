//! The price import over HTTP: an operator uploads a snapshot of the real
//! public dataset, and each priced entry of it becomes a proposal labelled
//! third-party. Nothing becomes a price until an operator confirms one.
//!
//! The uploads are the committed snapshot of the dataset's own `data.json`,
//! sent as its own text, not a hand-written body, and every expected price is
//! read from that file's text with `RawValue` rather than from the importer.
//!
//! The route table's own fixtures are in `tests/roles.rs`.

mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::pricing::import::{DATASET, MAX_IMPORT_BYTES};
use aisix_pricing::Catalogue;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{json, Value};
use sqlx::PgPool;
use support::world::{db_now, seed_provider_key};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The committed snapshot: the dataset's published `v2/data.json` at the
/// commit in its file name.
const SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/e2e/fixtures/pricing-import/genai-prices-v2-7de757d.json"
);

const PATH: &str = "/admin/pricing/imports";

fn snapshot() -> String {
    std::fs::read_to_string(SNAPSHOT).expect("read the dataset snapshot")
}

/// What the snapshot itself says of provider `provider` and its model
/// `model`: the provider's first pricing page, and the model's prices, each
/// number still as written. A test's expected value comes from here, never
/// from the importer.
fn source(provider: &str, model: &str) -> (Option<String>, BTreeMap<String, String>) {
    #[derive(Deserialize)]
    struct Provider<'a> {
        id: String,
        #[serde(default)]
        pricing_urls: Vec<String>,
        #[serde(borrow)]
        models: Vec<Model<'a>>,
    }
    #[derive(Deserialize)]
    struct Model<'a> {
        id: String,
        #[serde(borrow)]
        prices: &'a RawValue,
    }
    let text = snapshot();
    let all: Vec<Provider<'_>> = serde_json::from_str(&text).expect("the snapshot decodes");
    let provider = all
        .iter()
        .find(|p| p.id == provider)
        .unwrap_or_else(|| panic!("no provider {provider} in the snapshot"));
    let prices = provider
        .models
        .iter()
        .find(|m| m.id == model)
        .unwrap_or_else(|| panic!("no model {model} in the snapshot"))
        .prices
        .get();
    // A plain map is the one price in effect; a dated list would need its
    // own reading, and this model has none.
    let prices: BTreeMap<String, &RawValue> =
        serde_json::from_str(prices).expect("one price map, not a dated list");
    (
        provider.pricing_urls.first().cloned(),
        prices
            .into_iter()
            .map(|(field, value)| (field, value.get().to_string()))
            .collect(),
    )
}

/// `POST /admin/pricing/imports` as `op`, with `data` sent as its own text.
async fn upload_text(cp: &TestCp, op: &OperatorSession, dataset_ref: &str, data: &str) -> Resp {
    let body = format!(r#"{{"dataset_ref":{},"data":{data}}}"#, json!(dataset_ref));
    cp.send_raw(
        "POST",
        PATH,
        Some(op.cookie()),
        Some("application/json"),
        Some(body.into_bytes()),
    )
    .await
}

/// An upload of the snapshot, which must answer 201; the batch it answers.
async fn upload(cp: &TestCp, op: &OperatorSession) -> Value {
    let r = upload_text(cp, op, "v2-7de757d", &snapshot()).await;
    assert_eq!(r.status, 201, "{r:?}");
    r.body
}

fn id(v: &Value) -> &str {
    v["id"].as_str().unwrap_or_else(|| panic!("no id in {v}"))
}

/// `GET /admin/pricing/imports/{id}`, with `all=true` when `all`, which must
/// answer 200.
async fn detail(cp: &TestCp, op: &OperatorSession, batch: &Value, all: bool) -> Value {
    let path = match all {
        true => format!("{PATH}/{}?all=true", id(batch)),
        false => format!("{PATH}/{}", id(batch)),
    };
    let r = cp.send("GET", &path, Some(op.cookie()), None).await;
    assert_eq!(r.status, 200, "GET {path}: {r:?}");
    r.body
}

fn listed(detail: &Value) -> Vec<Value> {
    detail["proposals"]
        .as_array()
        .unwrap_or_else(|| panic!("no proposals in {detail}"))
        .clone()
}

/// Every proposal of `batch`, as listed with `all=true`.
async fn proposals(cp: &TestCp, op: &OperatorSession, batch: &Value) -> Vec<Value> {
    listed(&detail(cp, op, batch, true).await)
}

/// The proposal of `provider`'s `model` in effect at upload, if listed.
fn find<'a>(all: &'a [Value], provider: &str, model: &str) -> Option<&'a Value> {
    all.iter()
        .find(|p| p["provider"] == provider && p["model"] == model && p["effective_from"].is_null())
}

fn current<'a>(all: &'a [Value], provider: &str, model: &str) -> &'a Value {
    find(all, provider, model).unwrap_or_else(|| panic!("no proposal of {provider}/{model}"))
}

/// `GET /admin/pricing/imports` as `op`: the batches listed.
async fn batches(cp: &TestCp, op: &OperatorSession) -> Vec<Value> {
    let r = cp.send("GET", PATH, Some(op.cookie()), None).await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items").clone()
}

/// A live direct model of `provider` whose upstream name is `model`, on a
/// provider key of its own; its id.
async fn model(cp: &TestCp, admin: &OperatorSession, provider: &str, model: &str) -> Uuid {
    let pk = seed_provider_key(cp, admin).await;
    let r = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": format!("{model}-{}", Uuid::new_v4().simple()),
                "provider": provider,
                "model_name": model,
                "provider_key_id": pk,
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "create {provider}/{model}: {r:?}");
    Uuid::parse_str(id(&r.body)).expect("a model id")
}

/// Deletes model `id` as `admin`.
async fn delete_model(cp: &TestCp, admin: &OperatorSession, id: Uuid) {
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM gw_resources WHERE kind = 'models' AND id = $1")
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the model's version");
    let status = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .delete(cp.url(&format!("/admin/resources/models/{id}")))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .expect("send the delete")
        .status();
    assert_eq!(status, 200, "delete model {id}");
}

/// `id`'s proposal rejected, as the reject route writes it: a `pending` row
/// moved to `rejected` with its reason and decision, and nothing else
/// touched.
async fn reject_by_hand(cp: &TestCp, id: &str) {
    sqlx::query(
        "UPDATE pricing_import_proposals \
         SET status = 'rejected', reject_reason = '与官方价格不符', decided_at = now() \
         WHERE id = $1::uuid AND status = 'pending'",
    )
    .bind(id)
    .execute(cp.db().pool())
    .await
    .expect("reject one proposal");
}

/// The error an SQL statement is refused with.
async fn refused_sql(cp: &TestCp, sql: &str) -> String {
    sqlx::query(sql)
        .execute(cp.db().pool())
        .await
        .expect_err(sql)
        .to_string()
}

/// Sessions of this test's database waiting for a lock.
async fn lock_waiters(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(pool)
    .await
    .expect("count lock waiters")
}

#[tokio::test]
async fn uploading_the_dataset_snapshot_creates_proposals() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let batch = upload(&cp, &op).await;

    assert_eq!(batch["dataset"], DATASET);
    assert_eq!(batch["dataset_ref"], "v2-7de757d");
    assert_eq!(batch["status"], "open");
    assert_eq!(batch["created_by"], json!({"id": op.id, "email": op.email}));
    let count = batch["proposals"].as_i64().expect("a proposal count");
    let flagged = batch["flagged"].as_i64().expect("a flagged count");
    assert!(count > 0, "{batch}");

    let all = proposals(&cp, &op, &batch).await;
    assert_eq!(all.len() as i64, count);
    assert_eq!(
        all.iter().filter(|p| p["flags"] != json!([])).count() as i64,
        flagged
    );
    assert!(all.iter().all(|p| p["status"] == "pending"));

    // The model's in-effect price, as the snapshot's own text has it.
    let (pricing_url, prices) = source("anthropic", "claude-opus-5-5");
    let proposal = current(&all, "anthropic", "claude-opus-5-5");
    let base = &proposal["card"]["default"]["base"];
    for (field, dimension) in [
        ("input_mtok", "input"),
        ("output_mtok", "output"),
        ("cache_read_mtok", "cache_read"),
        ("cache_write_mtok", "cache_write"),
    ] {
        assert_eq!(base[dimension], *prices[field], "{field}: {proposal}");
    }
    assert_eq!(proposal["flags"], json!([]), "{proposal}");
    assert_eq!(proposal["pricing_url"], json!(pricing_url));
    // The published file carries no checked date, so none is recorded.
    assert_eq!(proposal["prices_checked"], Value::Null);

    // The two entries whose comments state that output is free become cards
    // with a zero output; the input price is still the file's own.
    for (provider, model) in [
        ("perplexity", "pplx-decider-v1.2"),
        ("perplexity", "pplx-decider-flash-v1.2"),
    ] {
        let (_, prices) = source(provider, model);
        let p = current(&all, provider, model);
        assert_eq!(p["flags"], json!([]), "{model}");
        assert_eq!(
            p["card"]["default"]["base"]["input"], *prices["input_mtok"],
            "{model}"
        );
        assert_eq!(p["card"]["default"]["base"]["output"], "0", "{model}");
    }
    // A comment that says "output is free" on a model this build does not
    // name is not read: `typesafe/jev` is an ordinary chat model.
    let jev = current(&all, "openrouter", "typesafe/jev-1.13");
    assert_eq!(jev["flags"], json!(["invalid_card"]));
    assert_eq!(jev["card"], Value::Null);
}

#[tokio::test]
async fn a_newer_upload_supersedes_pending_proposals() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let first = upload(&cp, &op).await;
    let first_proposals = proposals(&cp, &op, &first).await;
    // One proposal of the first batch was decided before the next upload.
    // Rejecting it is a legal transition, written as the route will write it;
    // the route itself is another task's, so this is the one place a test
    // decides a proposal by hand, and it decides it the way the route does.
    let decided = id(&first_proposals[0]).to_string();
    reject_by_hand(&cp, &decided).await;

    let second = upload(&cp, &op).await;
    assert_eq!(second["status"], "open");

    let listed = batches(&cp, &op).await;
    assert_eq!(
        listed.iter().map(id).collect::<Vec<_>>(),
        vec![id(&second), id(&first)],
        "newest first"
    );
    assert_eq!(listed[1]["status"], "superseded");
    // Every pending proposal of the older batch is superseded; a decided one
    // keeps its decision.
    for p in proposals(&cp, &op, &first).await {
        let expected = if id(&p) == decided {
            "rejected"
        } else {
            "superseded"
        };
        assert_eq!(p["status"], expected, "{p}");
    }
    let newer = proposals(&cp, &op, &second).await;
    assert!(!newer.is_empty());
    assert!(newer.iter().all(|p| p["status"] == "pending"));
}

/// Two uploads at once leave one open batch: one waits for the other, then
/// supersedes it. The other is stood in for by a transaction that has written
/// its batch and not committed yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_uploads_at_once_leave_one_open_batch() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let pool = cp.db().pool().clone();
    let mut in_flight = pool.begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO pricing_import_batches \
         (id, dataset, dataset_ref, status, proposals, flagged, created_by) \
         VALUES ($1, $2, 'in-flight', 'open', 0, 0, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(DATASET)
    .bind(op.id)
    .execute(&mut *in_flight)
    .await
    .expect("write the other upload's batch");

    let data = r#"[{"id":"p","models":[{"id":"m","prices":{"input_mtok":1,"output_mtok":2}}]}]"#;
    let request = {
        let (cp, op) = (&cp, &op);
        async move { upload_text(cp, op, "v2", data).await }
    };
    tokio::pin!(request);
    // Until the upload waits on the other, or (without the wait) is done.
    let deadline = Instant::now() + Duration::from_secs(20);
    let early = loop {
        tokio::select! {
            r = &mut request => break Some(r),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {
                if lock_waiters(&pool).await > 0 {
                    break None;
                }
                assert!(Instant::now() < deadline, "the upload neither waited nor answered");
            }
        }
    };
    in_flight.commit().await.expect("commit the other upload");
    let r = match early {
        Some(r) => r,
        None => request.await,
    };
    assert_eq!(r.status, 201, "{r:?}");
    let open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pricing_import_batches WHERE status = 'open'")
            .fetch_one(&pool)
            .await
            .expect("count open batches");
    assert_eq!(open, 1, "the later upload supersedes the earlier one");
}

#[tokio::test]
async fn an_upload_needs_a_dataset_ref_and_a_dataset() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let long = "a".repeat(101);
    let cases: Vec<(Value, &str)> = vec![
        (json!({"data": []}), "/dataset_ref"),
        (json!({"dataset_ref": "", "data": []}), "/dataset_ref"),
        (json!({"dataset_ref": "v2 7de", "data": []}), "/dataset_ref"),
        (json!({"dataset_ref": long, "data": []}), "/dataset_ref"),
        (json!({"dataset_ref": "v2"}), "/data"),
        (json!({"dataset_ref": "v2", "data": {"a": 1}}), "/data"),
        (json!({"dataset_ref": "v2", "data": []}), "/data"),
        (
            json!({"dataset_ref": "v2", "data": [{"id": "p", "models": []}]}),
            "/data",
        ),
    ];
    for (body, path) in cases {
        let r = cp
            .send("POST", PATH, Some(op.cookie()), Some(body.clone()))
            .await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed", "{body}");
        let paths: Vec<&str> = r.body["details"]
            .as_array()
            .expect("details")
            .iter()
            .map(|d| d["path"].as_str().expect("a path"))
            .collect();
        assert_eq!(paths, vec![path], "{body}: {r:?}");
    }
    // The longest reference, and every character it may hold, is taken.
    let r = upload_text(
        &cp,
        &op,
        &format!("v2/7de757d_a-b.{}", "c".repeat(85)),
        &snapshot(),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        batches(&cp, &op).await.len(),
        1,
        "only the valid one was written"
    );
}

#[tokio::test]
async fn an_upload_past_the_body_limit_is_413() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    // The snapshot padded past every other route's limit and below this
    // one's: read, and uploaded.
    let padded = format!("{}{}", snapshot(), " ".repeat(3 << 20));
    let r = upload_text(&cp, &op, "v2", &padded).await;
    assert_eq!(r.status, 201, "a body below the upload's own limit: {r:?}");

    // 9 MiB of valid JSON.
    let past = format!("{}{}", snapshot(), " ".repeat(9 << 20));
    assert!(past.len() > MAX_IMPORT_BYTES);
    let r = upload_text(&cp, &op, "v2", &past).await;
    assert_eq!(r.status, 413, "{r:?}");
    assert_eq!(r.body["code"], "payload_too_large");
    assert_eq!(batches(&cp, &op).await.len(), 1);
}

#[tokio::test]
async fn a_proposal_equal_to_the_current_baseline_is_hidden_by_default() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &admin, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &admin).await;
    let shown = listed(&detail(&cp, &admin, &batch, false).await);
    let proposal = current(&shown, "anthropic", "claude-opus-5-5").clone();
    assert_eq!(proposal["same_as_current"], false, "no baseline yet");
    assert_eq!(proposal["current_baseline"], Value::Null);
    let card = proposal["card"].clone();

    // The proposal's own card becomes the model's baseline: nothing is left
    // to confirm.
    let r = cp
        .send(
            "POST",
            "/admin/pricing/baselines",
            Some(admin.cookie()),
            Some(json!({
                "model_id": model_id,
                "effective_from": db_now(&cp).await,
                "card": card,
                "note": "与数据集一致",
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let revision = r.body["revision"]["revision_id"].clone();

    let shown = listed(&detail(&cp, &admin, &batch, false).await);
    assert!(
        find(&shown, "anthropic", "claude-opus-5-5").is_none(),
        "the equal proposal is left out by default"
    );
    let all = proposals(&cp, &admin, &batch).await;
    assert_eq!(shown.len() + 1, all.len(), "only that one is left out");
    let equal = current(&all, "anthropic", "claude-opus-5-5");
    assert_eq!(equal["same_as_current"], true);
    assert_eq!(equal["current_baseline"]["revision_id"], revision);
    assert_eq!(equal["current_baseline"]["card"], card);

    // The built-in catalogue's entry for the same model is shown beside it.
    let catalogue = Catalogue::builtin().expect("the built-in catalogue");
    let entry = catalogue
        .get("anthropic", "claude-opus-5-5", chrono::Utc::now())
        .expect("the catalogue prices this model");
    assert_eq!(
        equal["catalogue_entry"],
        json!({
            "key": entry.key,
            "card": entry.card,
            "source_url": entry.source_url,
            "as_of": entry.as_of,
        })
    );
}

#[tokio::test]
async fn a_deleted_models_proposal_is_no_longer_hidden() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &admin, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &admin).await;
    let card = current(
        &proposals(&cp, &admin, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )["card"]
        .clone();
    let r = cp
        .send(
            "POST",
            "/admin/pricing/baselines",
            Some(admin.cookie()),
            Some(json!({
                "model_id": model_id,
                "effective_from": db_now(&cp).await,
                "card": card,
                "note": "与数据集一致",
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    // Hidden while the model it would price is live.
    let shown = listed(&detail(&cp, &admin, &batch, false).await);
    assert!(find(&shown, "anthropic", "claude-opus-5-5").is_none());

    delete_model(&cp, &admin, model_id).await;
    let shown = listed(&detail(&cp, &admin, &batch, false).await);
    let listed_again = find(&shown, "anthropic", "claude-opus-5-5")
        .expect("a proposal whose model is gone is still listed");
    // It can no longer be confirmed, whatever its card says.
    assert_eq!(listed_again["same_as_current"], false);
    // Its model's baseline is still shown, as what it is.
    assert_eq!(
        listed_again["current_baseline"]["model_id"],
        json!(model_id)
    );
    assert_eq!(listed_again["current_baseline"]["card"], card);
}

#[tokio::test]
async fn the_suggested_model_is_a_direct_model_of_that_provider_and_name() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let opus = model(&cp, &admin, "anthropic", "claude-opus-5-5").await;
    // The same upstream name under another provider.
    model(&cp, &admin, "openai", "claude-sonnet-5-5").await;
    // A deleted model of the entry's provider and name.
    let deleted = model(&cp, &admin, "anthropic", "claude-haiku-5-5").await;
    delete_model(&cp, &admin, deleted).await;
    // A routing model named like an entry, which has no upstream of its own.
    let r = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": "claude-fable-5-1",
                "routing": {"targets": [{"model_id": opus}]},
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");

    let batch = upload(&cp, &admin).await;
    let all = proposals(&cp, &admin, &batch).await;
    let suggestion = |model: &str| current(&all, "anthropic", model)["suggested_model_id"].clone();
    assert_eq!(suggestion("claude-opus-5-5"), json!(opus));
    assert_eq!(
        suggestion("claude-sonnet-5-5"),
        Value::Null,
        "another provider's model"
    );
    assert_eq!(
        suggestion("claude-haiku-5-5"),
        Value::Null,
        "a deleted model"
    );
    assert_eq!(
        suggestion("claude-fable-5-1"),
        Value::Null,
        "a routing model"
    );
}

#[tokio::test]
async fn who_may_upload() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;

    let r = upload_text(&cp, &viewer, "v2-7de757d", &snapshot()).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(r.body["code"], "forbidden");
    assert!(
        batches(&cp, &viewer).await.is_empty(),
        "nothing was written"
    );

    let batch = upload(&cp, &operator).await;
    // A viewer reads what an operator uploaded.
    assert_eq!(batches(&cp, &viewer).await.len(), 1);
    assert_eq!(detail(&cp, &viewer, &batch, false).await["id"], batch["id"]);
}

#[tokio::test]
async fn a_proposal_is_decided_once_and_never_rewritten() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let batch = upload(&cp, &op).await;
    let all = proposals(&cp, &op, &batch).await;
    let one = current(&all, "anthropic", "claude-opus-5-5").clone();
    let pid = id(&one).to_string();
    let filter = format!("WHERE id = '{pid}'");

    // A card and flags are the upload's own.
    for sql in [
        format!(r#"UPDATE pricing_import_proposals SET card = '{{}}'::jsonb {filter}"#),
        format!(r#"UPDATE pricing_import_proposals SET flags = '["bad_number"]'::jsonb {filter}"#),
        format!(r#"UPDATE pricing_import_proposals SET provider = 'x' {filter}"#),
        format!(r#"UPDATE pricing_import_proposals SET model = 'x' {filter}"#),
        format!(r#"UPDATE pricing_import_proposals SET effective_from = now() {filter}"#),
        format!(r#"UPDATE pricing_import_proposals SET batch_id = gen_random_uuid() {filter}"#),
        format!("DELETE FROM pricing_import_proposals {filter}"),
        "TRUNCATE pricing_import_proposals, pricing_baselines".to_string(),
    ] {
        let err = refused_sql(&cp, &sql).await;
        assert!(
            err.to_lowercase().contains("written once")
                || err.to_lowercase().contains("never deleted")
                || err.to_lowercase().contains("append-only")
                || err.contains("cannot truncate"),
            "{sql}: {err}"
        );
    }
    // None of it moved.
    let after = proposals(&cp, &op, &batch).await;
    assert_eq!(after, all, "the rows are as uploaded");

    // A confirmation's attestation is written once, with it.
    let err = refused_sql(
        &cp,
        &format!(
            "UPDATE pricing_import_proposals SET attested_by = '{}' {filter}",
            op.id
        ),
    )
    .await;
    assert!(err.contains("only a confirmation attests"), "{err}");

    // `pending` moves to one decision, and nothing moves out of it.
    reject_by_hand(&cp, &pid).await;
    let decided = proposals(&cp, &op, &batch)
        .await
        .into_iter()
        .find(|p| id(p) == pid)
        .expect("the proposal");
    assert_eq!(decided["status"], "rejected");
    for (sql, expected) in [
        (
            format!(r#"UPDATE pricing_import_proposals SET status = 'confirmed' {filter}"#),
            "final",
        ),
        (
            format!(r#"UPDATE pricing_import_proposals SET status = 'pending' {filter}"#),
            "final",
        ),
        (
            format!(r#"UPDATE pricing_import_proposals SET reject_reason = 'x' {filter}"#),
            "final",
        ),
        (
            format!(
                r#"UPDATE pricing_import_proposals SET attested_by = '{}' {filter}"#,
                op.id
            ),
            "only a confirmation attests",
        ),
    ] {
        let err = refused_sql(&cp, &sql).await;
        assert!(err.contains(expected), "{sql}: {err}");
    }

    // An undecided one takes no other status than the three decisions.
    let pending = all
        .iter()
        .find(|p| p["status"] == "pending" && id(p) != pid)
        .expect("another pending proposal");
    let err = refused_sql(
        &cp,
        &format!(
            "UPDATE pricing_import_proposals SET status = 'pending' WHERE id = '{}'",
            id(pending)
        ),
    )
    .await;
    assert!(err.contains("cannot move from pending"), "{err}");
}

#[tokio::test]
async fn an_unknown_batch_is_not_found() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    for path in [
        format!("{PATH}/{}", Uuid::new_v4()),
        format!("{PATH}/not-a-uuid"),
    ] {
        let r = cp.send("GET", &path, Some(viewer.cookie()), None).await;
        assert_eq!(r.status, 404, "GET {path}: {r:?}");
        assert_eq!(r.body["code"], "not_found");
    }
}
