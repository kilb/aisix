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
use support::migrate::{database_at, drop_database, migrate_rest};
use support::world::{
    db_now, deliver, group_id, insert_rate, public_model, seed_provider_key, usage, world,
    SHARED_GROUP,
};
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

/// `text`, a price as the dataset writes it, in millionths: `"0.3"` is
/// 300 000.
fn millionths(text: &str) -> i64 {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    assert!(fraction.len() <= 6, "{text}");
    let whole: i64 = whole.parse().expect("digits");
    let fraction: i64 = format!("{fraction:0<6}").parse().expect("digits");
    whole * 1_000_000 + fraction
}

/// The setup the channel-style configuration recommends: a public model
/// ticked on an upstream account, which generates a direct document carrying
/// the account's provider and the upstream name. Tenants call the public
/// model, so that is the model the import suggests, and a confirmed
/// suggestion prices the tenant's call.
#[tokio::test]
async fn a_confirmed_suggestion_prices_a_call_to_the_public_model() {
    let Some(w) = world().await else { return };
    let (cp, op) = (&w.cp, &w.admin);
    let r = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(op.cookie()),
            Some(json!({"doc": {
                "display_name": "acct-gp-5d2",
                "provider": "anthropic",
                "api_key": "sk-ant-import-7c1",
                "groups": [SHARED_GROUP],
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let account = id(&r.body).to_string();
    let public = public_model(cp, op, "claude-opus-5-5").await;
    let r = cp
        .send(
            "PUT",
            &format!("/admin/provider-keys/{account}/channel-models/{public}"),
            Some(op.cookie()),
            Some(json!({"priority": 0, "weight": 1})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    // The generated direct document is the one carrying the entry's
    // provider and upstream name.
    let direct: Uuid = sqlx::query_scalar(
        "SELECT id FROM gw_resources WHERE kind = 'models' AND NOT deleted \
           AND doc->>'provider' = 'anthropic' AND doc->>'model_name' = 'claude-opus-5-5'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("the generated direct document");
    assert_ne!(direct, public);
    // The tenant is granted the public model, and its key's group is rated
    // at 1 CNY per USD.
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(w.tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("the tenant's version");
    let granted = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .patch(cp.url(&format!("/admin/tenants/{}", w.tenant)))
        .header(reqwest::header::COOKIE, op.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_model_ids": [public]}))
        .send()
        .await
        .expect("send the grant");
    assert_eq!(granted.status(), 200);
    insert_rate(cp, group_id(cp, SHARED_GROUP).await, db_now(cp).await)
        .await
        .expect("rate the group");

    let batch = upload(cp, op).await;
    let proposal = current(
        &proposals(cp, op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    assert_eq!(proposal["suggested_model_id"], json!(public), "{proposal}");
    // A confirmation naming the direct document instead, as a proposal
    // uploaded by an earlier build still suggests, is refused at
    // `/model_id`, naming the public model to price; the proposal stays
    // pending.
    let r = decide(
        cp,
        op,
        &batch,
        &proposal,
        "confirm",
        json!({"model_id": direct, "effective_from": db_now(cp).await, "attested": true}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(refusal_paths(&r), vec!["/model_id"], "{r:?}");
    assert_eq!(
        r.body["details"][0]["message"],
        "该模型由公共模型 claude-opus-5-5 生成，请为公共模型 claude-opus-5-5 设置基准价",
        "{r:?}"
    );
    let r = decide(
        cp,
        op,
        &batch,
        &proposal,
        "confirm",
        json!({
            "model_id": proposal["suggested_model_id"],
            "effective_from": db_now(cp).await,
            "attested": true,
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");

    // One call to the public model: 1000 input and 200 output tokens, at
    // the snapshot's own prices for the entry, times a rate of 1.
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(
            &w,
            "e-opus",
            public,
            json!({"prompt_tokens": 1000, "completion_tokens": 200}),
        )],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let (_, prices) = source("anthropic", "claude-opus-5-5");
    let owed = 1000 * millionths(&prices["input_mtok"]) + 200 * millionths(&prices["output_mtok"]);
    let expected = (owed + 999_999) / 1_000_000;
    let (status, charge, error): (String, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT status, charge_micro_cny, error_code FROM pricing_charges_cny \
         WHERE event_id = 'e-opus'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("the call's charge");
    assert_eq!(
        (status.as_str(), charge, error),
        ("charged", Some(expected), None),
        "the call to the public model is priced"
    );
    assert!(expected > 0);
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

/// A confirmed proposal always names its attester: the database refuses a
/// confirmation written without one (0030), whoever writes it — the route
/// sets both in one update, so only a write around it could try.
#[tokio::test]
async fn the_database_refuses_a_confirmation_without_its_attester() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    let revision =
        support::world::insert_baseline(&cp, model_id, db_now(&cp).await, support::world::card())
            .await
            .expect("a baseline to name");
    let err = refused_sql(
        &cp,
        &format!(
            "UPDATE pricing_import_proposals \
             SET status = 'confirmed', decided_at = now(), baseline_revision_id = {} \
             WHERE id = '{}'",
            revision.revision_id,
            id(&proposal)
        ),
    )
    .await;
    assert!(err.contains("pricing_import_proposals_attested"), "{err}");
    // The same write naming its attester is what the route writes.
    sqlx::query(&format!(
        "UPDATE pricing_import_proposals \
         SET status = 'confirmed', decided_at = now(), baseline_revision_id = {}, \
             decided_by = '{}', attested_by = '{}' \
         WHERE id = '{}'",
        revision.revision_id,
        op.id,
        op.id,
        id(&proposal)
    ))
    .execute(cp.db().pool())
    .await
    .expect("a confirmation with its attester");
}

/// The first builds of the confirmation route confirmed without writing the
/// attester, and 0026's trigger keeps such a row as it is for good. A
/// database holding one still migrates: the constraint binds the rows
/// written after it, and the old row stays.
#[tokio::test]
async fn the_upgrade_keeps_a_confirmation_an_early_build_left_without_its_attester() {
    let Some(pool) = database_at(29).await else {
        return;
    };
    let operator = Uuid::new_v4();
    let batch = Uuid::new_v4();
    let proposal = Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO operators (id, email, password_hash, role) \
             VALUES ('{operator}', 'early-7c1@ops.test', 'x', 'operator')"
        ),
        "INSERT INTO pricing_baselines \
         (engine_version, model_id, effective_from, card, source_kind, source) \
         VALUES (2, gen_random_uuid(), now(), '{}', 'manual', '{\"note\": \"x\"}')"
            .to_string(),
        format!(
            "INSERT INTO pricing_import_batches \
             (id, dataset, dataset_ref, status, proposals, flagged, created_by) \
             VALUES ('{batch}', 'd', 'r', 'open', 1, 0, '{operator}')"
        ),
        format!(
            "INSERT INTO pricing_import_proposals (id, batch_id, provider, model, card, status) \
             VALUES ('{proposal}', '{batch}', 'p', 'm', '{{}}', 'pending')"
        ),
        // What an early build's confirmation wrote: no attester.
        format!(
            "UPDATE pricing_import_proposals SET status = 'confirmed', decided_at = now(), \
               decided_by = '{operator}', \
               baseline_revision_id = (SELECT max(revision_id) FROM pricing_baselines) \
             WHERE id = '{proposal}'"
        ),
    ] {
        sqlx::query(&sql)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    migrate_rest(&pool).await.expect("0030 applies");

    let kept: (String, Option<Uuid>) =
        sqlx::query_as("SELECT status, attested_by FROM pricing_import_proposals WHERE id = $1")
            .bind(proposal)
            .fetch_one(&pool)
            .await
            .expect("the old confirmation");
    assert_eq!(kept, ("confirmed".to_string(), None));
    drop_database(pool).await;
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

// ---- Deciding a proposal -------------------------------------------------

/// `POST /admin/pricing/imports/{id}/proposals/{pid}/{action}` with `body`:
/// `confirm` or `reject`.
async fn decide(
    cp: &TestCp,
    op: &OperatorSession,
    batch: &Value,
    proposal: &Value,
    action: &str,
    body: Value,
) -> Resp {
    let path = format!("{PATH}/{}/proposals/{}/{action}", id(batch), id(proposal));
    cp.send("POST", &path, Some(op.cookie()), Some(body)).await
}

/// The `details` paths of a refusal.
fn refusal_paths(r: &Resp) -> Vec<String> {
    r.body["details"]
        .as_array()
        .unwrap_or_else(|| panic!("no details in {r:?}"))
        .iter()
        .map(|d| d["path"].as_str().expect("a path").to_string())
        .collect()
}

/// The model's baseline in force now, `null` when it has none.
async fn current_baseline(cp: &TestCp, op: &OperatorSession, model_id: Uuid) -> Value {
    let r = cp
        .send("GET", "/admin/pricing/baselines", Some(op.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let item = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["model_id"] == json!(model_id))
        .unwrap_or_else(|| panic!("no baseline item for {model_id}"));
    item["current"].clone()
}

/// The pricing audit rows, in the order they were written: `(action,
/// target_id, detail)`.
async fn audits(cp: &TestCp) -> Vec<(String, Option<String>, Value)> {
    sqlx::query_as(
        "SELECT action, target_id, detail FROM audit_log \
         WHERE action LIKE 'pricing.%' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log")
}

/// The count `pricing_version` holds.
async fn pricing_version(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT (value::text)::bigint FROM cp_settings WHERE key = 'pricing_version'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read pricing_version")
}

#[tokio::test]
async fn confirming_writes_a_third_party_baseline_with_its_attestation() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    assert_eq!(proposal["flags"], json!([]), "confirmable as it stands");
    let (pricing_url, _) = source("anthropic", "claude-opus-5-5");
    let version = pricing_version(&cp).await;

    let r = decide(
        &cp,
        &op,
        &batch,
        &proposal,
        "confirm",
        json!({
            "model_id": model_id,
            "effective_from": db_now(&cp).await,
            "attested": true,
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let revision = &r.body;
    assert_eq!(revision["model_id"], json!(model_id));
    assert_eq!(revision["card"], proposal["card"]);
    assert_eq!(revision["source_kind"], "third_party");
    assert_eq!(revision["source"]["dataset"], DATASET);
    assert_eq!(revision["source"]["dataset_ref"], "v2-7de757d");
    assert_eq!(revision["source"]["pricing_url"], json!(pricing_url));
    assert_eq!(revision["source"]["prices_checked"], Value::Null);
    assert_eq!(revision["source"]["import_id"], batch["id"]);
    assert_eq!(revision["source"]["proposal_id"], proposal["id"]);
    assert_eq!(revision["source"]["attested_by"], json!(op.id));
    assert!(revision["source"]["attested_at"].is_string(), "{revision}");
    assert_eq!(
        revision["created_by"],
        json!({"id": op.id, "email": op.email})
    );

    // The proposal is decided, by whom, and names the revision it wrote.
    let all = proposals(&cp, &op, &batch).await;
    let confirmed = all
        .iter()
        .find(|p| p["id"] == proposal["id"])
        .expect("the proposal");
    assert_eq!(confirmed["status"], "confirmed");
    assert_eq!(confirmed["decided_by"], json!(op.id));
    assert!(confirmed["decided_at"].is_string(), "{confirmed}");
    // The proposal names its own attester too, not only the baseline: the
    // column is written once, by this confirmation, and no later write may
    // set it.
    assert_eq!(confirmed["attested_by"], json!(op.id), "{confirmed}");
    assert_eq!(confirmed["baseline_revision_id"], revision["revision_id"]);

    // It is what the model bills from now.
    let current = current_baseline(&cp, &op, model_id).await;
    assert_eq!(current["revision_id"], revision["revision_id"]);
    assert_eq!(current["card"], proposal["card"]);
    assert_eq!(pricing_version(&cp).await, version + 1);

    // Two audit rows: the baseline's own, then the decision.
    let rows = audits(&cp).await;
    let actions: Vec<&str> = rows.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        vec![
            "pricing.import.upload",
            "pricing.baseline.create",
            "pricing.import.confirm"
        ],
        "{rows:?}"
    );
    // The sentence the operator affirmed, so it stays readable after a
    // later release rewords the checkbox, and the fields its card leaves out,
    // which for this entry is none.
    assert_eq!(
        rows[2].2,
        json!({
            "proposal_id": proposal["id"],
            "model_id": model_id,
            "attested": true,
            "attestation": "已对照官方价格页核对",
            "dropped": [],
        })
    );
    assert_eq!(rows[2].1, Some(id(&proposal).to_string()));
}

#[tokio::test]
async fn confirming_needs_the_attestation() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    let effective_from = db_now(&cp).await;

    for attested in [Some(json!(false)), None, Some(Value::Null)] {
        let mut body = json!({"model_id": model_id, "effective_from": effective_from});
        if let Some(value) = attested.clone() {
            body["attested"] = value;
        }
        let r = decide(&cp, &op, &batch, &proposal, "confirm", body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed", "{body}");
        assert_eq!(refusal_paths(&r), vec!["/attested"], "{body}");
        assert_eq!(
            r.body["details"][0]["message"],
            "请确认已对照官方价格页核对"
        );
    }

    assert_eq!(cp.count("pricing_baselines").await, 0);
    let all = proposals(&cp, &op, &batch).await;
    assert_eq!(
        current(&all, "anthropic", "claude-opus-5-5")["status"],
        "pending"
    );
    assert_eq!(audits(&cp).await.len(), 1, "only the upload's own row");
}

#[tokio::test]
async fn a_proposal_only_a_dropped_price_flagged_can_be_confirmed() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    // `openai/gpt-4.1` prices file-search storage per thousand searches, a
    // quantity no usage event reports and no card dimension meters. That
    // price is dropped and named, and the rest of the card is complete.
    let model_id = model(&cp, &op, "openai", "gpt-4.1").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(&proposals(&cp, &op, &batch).await, "openai", "gpt-4.1").clone();
    assert_eq!(
        proposal["flags"],
        json!(["unsupported_field"]),
        "{proposal}"
    );
    assert_eq!(
        proposal["dropped"],
        json!(["storage_searches_kcount"]),
        "{proposal}"
    );
    assert!(!proposal["card"].is_null(), "{proposal}");

    let r = decide(
        &cp,
        &op,
        &batch,
        &proposal,
        "confirm",
        json!({
            "model_id": model_id,
            "effective_from": db_now(&cp).await,
            "attested": true,
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let (_, prices) = source("openai", "gpt-4.1");
    assert_eq!(
        r.body["card"]["default"]["base"]["input"], prices["input_mtok"],
        "{r:?}"
    );
    // The card carries the search price it could map, and nothing for the
    // storage price it dropped.
    let per_thousand = aisix_pricing::Rate::parse(&prices["web_searches_kcount"]).expect("a rate");
    let per_million = aisix_pricing::Rate(per_thousand.0 * 1000).to_string();
    assert_eq!(
        r.body["card"]["default"]["base"]["web_search"], per_million,
        "{r:?}"
    );
    let after = proposals(&cp, &op, &batch).await;
    let p = after
        .iter()
        .find(|p| p["id"] == proposal["id"])
        .expect("it");
    assert_eq!(p["status"], "confirmed", "{p}");
    // The audit row records which price the attested card left out.
    let confirm: Vec<(Option<String>, Value)> = sqlx::query_as(
        "SELECT target_id, detail FROM audit_log \
         WHERE action = 'pricing.import.confirm' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log");
    let row = confirm
        .iter()
        .find(|(target, _)| target.as_deref() == Some(id(&proposal)))
        .expect("the confirmation's audit row");
    assert_eq!(
        row.1["dropped"],
        json!(["storage_searches_kcount"]),
        "{row:?}"
    );
    assert_eq!(row.1["attested"], json!(true), "{row:?}");
}

#[tokio::test]
async fn a_flagged_proposal_cannot_be_confirmed() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let all = proposals(&cp, &op, &batch).await;

    // One flag that blocks a confirmation while its card still stands, and
    // one whose prices made no card at all.
    let blocking: Vec<&str> = ["time_of_day", "unsupported_variant", "invalid_card"].to_vec();
    let flagged = all
        .iter()
        .find(|p| {
            !p["card"].is_null()
                && p["flags"]
                    .as_array()
                    .expect("flags")
                    .iter()
                    .any(|f| blocking.contains(&f.as_str().expect("a flag")))
        })
        .expect("a blocked proposal with a card");
    let card_less = all
        .iter()
        .find(|p| p["card"].is_null())
        .expect("a proposal with no card");
    assert_ne!(card_less["flags"], json!([]), "a card-less one is flagged");

    for proposal in [flagged, card_less] {
        let r = decide(
            &cp,
            &op,
            &batch,
            proposal,
            "confirm",
            json!({
                "model_id": model_id,
                "effective_from": db_now(&cp).await,
                "attested": true,
            }),
        )
        .await;
        assert_eq!(r.status, 422, "{proposal}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed", "{proposal}");
        assert_eq!(refusal_paths(&r), vec!["/flags"], "{proposal}");
        assert_eq!(
            r.body["details"][0]["message"],
            "该提议含无法自动换算的价格，请手动录入"
        );
    }

    assert_eq!(cp.count("pricing_baselines").await, 0);
    let after = proposals(&cp, &op, &batch).await;
    for proposal in [flagged, card_less] {
        let p = after
            .iter()
            .find(|p| p["id"] == proposal["id"])
            .expect("it");
        assert_eq!(p["status"], "pending", "{p}");
    }
}

#[tokio::test]
async fn a_second_decision_past_the_lock_writes_nothing() {
    // The predicate's own proof, independent of the row lock: with the row
    // already decided, the exact UPDATE each verb runs matches nothing, so a
    // lost `FOR UPDATE` would refuse the second decision rather than rewrite
    // the first. Without the `AND status = 'pending'` clause this UPDATE
    // reports one affected row and the row is decided a second time.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    let pid = id(&proposal).to_string();

    // Decide it for real, so the row is in the state the guarded UPDATE will
    // meet: a check constraint ties `confirmed` to a baseline revision, which
    // only the verb can write.
    let body = json!({
        "model_id": model_id,
        "effective_from": db_now(&cp).await,
        "attested": true,
    });
    let first = decide(&cp, &op, &batch, &proposal, "confirm", body).await;
    assert_eq!(first.status, 201, "{first:?}");

    for status in ["confirmed", "rejected"] {
        let done = sqlx::query(
            "UPDATE pricing_import_proposals \
             SET status = $2, decided_at = now() \
             WHERE id = $1::uuid AND status = 'pending'",
        )
        .bind(&pid)
        .bind(status)
        .execute(cp.db().pool())
        .await
        .expect("the guarded update runs");
        assert_eq!(done.rows_affected(), 0, "{status}");
    }

    let stored: String =
        sqlx::query_scalar("SELECT status FROM pricing_import_proposals WHERE id = $1::uuid")
            .bind(&pid)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the decided row");
    assert_eq!(stored, "confirmed", "the first decision stands");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_proposal_is_decided_once() {
    // Two guards keep this invariant, and this case catches only their loss
    // TOGETHER: the `FOR UPDATE OF p` that serialises the two decisions, and
    // the `AND status = 'pending'` plus rows-affected check inside each verb.
    // Removing the lock alone leaves this test green, because the loser's
    // predicate matches nothing and it rolls back before its baseline lands.
    // That is deliberate — the predicate is why a lost lock cannot rewrite a
    // decided row — and `a_second_decision_past_the_lock_writes_nothing` is
    // the proof the predicate carries on its own.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    let body = json!({
        "model_id": model_id,
        "effective_from": db_now(&cp).await,
        "attested": true,
    });

    let (a, b) = tokio::join!(
        decide(&cp, &op, &batch, &proposal, "confirm", body.clone()),
        decide(&cp, &op, &batch, &proposal, "confirm", body.clone()),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [201, 409], "{a:?} {b:?}");
    let late = if a.status == 409 { &a } else { &b };
    assert_eq!(late.body["code"], "proposal_state", "{late:?}");
    assert_eq!(late.body["error"], "该提议已被处理，请刷新");
    assert_eq!(cp.count("pricing_baselines").await, 1, "one write, not two");

    // Either verb, afterwards, is the same answer.
    for (action, body) in [("confirm", body), ("reject", json!({"reason": "重复提交"}))] {
        let r = decide(&cp, &op, &batch, &proposal, action, body).await;
        assert_eq!(r.status, 409, "{action}: {r:?}");
        assert_eq!(r.body["code"], "proposal_state", "{action}");
    }
    assert_eq!(cp.count("pricing_baselines").await, 1);
}

#[tokio::test]
async fn rejecting_writes_nothing_but_the_decision() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();

    for reason in [json!({}), json!({"reason": ""}), json!({"reason": "   "})] {
        let r = decide(&cp, &op, &batch, &proposal, "reject", reason.clone()).await;
        assert_eq!(r.status, 422, "{reason}: {r:?}");
        assert_eq!(refusal_paths(&r), vec!["/reason"], "{reason}");
    }
    let long = "很".repeat(501);
    let r = decide(
        &cp,
        &op,
        &batch,
        &proposal,
        "reject",
        json!({"reason": long}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(refusal_paths(&r), vec!["/reason"]);
    assert_eq!(r.body["details"][0]["message"], "拒绝原因最多 500 个字符");

    let version = pricing_version(&cp).await;
    let r = decide(
        &cp,
        &op,
        &batch,
        &proposal,
        "reject",
        json!({"reason": "与官方价格页不符"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["id"], proposal["id"]);
    assert_eq!(r.body["status"], "rejected");
    assert_eq!(r.body["reject_reason"], "与官方价格页不符");
    assert_eq!(r.body["decided_by"], json!(op.id));
    assert!(r.body["decided_at"].is_string(), "{r:?}");
    assert_eq!(r.body["baseline_revision_id"], Value::Null);
    // What the review compares it to still reads.
    assert_eq!(r.body["card"], proposal["card"]);

    // No price, no version bump, and one audit row.
    assert_eq!(cp.count("pricing_baselines").await, 0);
    assert_eq!(current_baseline(&cp, &op, model_id).await, Value::Null);
    assert_eq!(pricing_version(&cp).await, version);
    let rows = audits(&cp).await;
    let actions: Vec<&str> = rows.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        vec!["pricing.import.upload", "pricing.import.reject"]
    );
    assert_eq!(rows[1].1, Some(id(&proposal).to_string()));

    // A reject is a decision: confirming afterwards is refused.
    let r = decide(
        &cp,
        &op,
        &batch,
        &proposal,
        "confirm",
        json!({
            "model_id": model_id,
            "effective_from": db_now(&cp).await,
            "attested": true,
        }),
    )
    .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "proposal_state");
}

/// A proposal locked by another transaction for longer than the control
/// plane's `lock_timeout`: the decision is refused as already handled, which
/// is what it is — the row it would decide is being decided.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_proposal_locked_by_another_writer_answers_proposal_state() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    let body = json!({
        "model_id": model_id,
        "effective_from": db_now(&cp).await,
        "attested": true,
    });
    let body_r = json!({"reason": "并发的拒绝"});

    // Another writer holds the row, as an in-flight upload's sweep or a
    // concurrent decision does.
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT id FROM pricing_import_proposals WHERE id = $1::uuid FOR UPDATE")
        .bind(id(&proposal))
        .execute(&mut *holder)
        .await
        .expect("lock the proposal");

    let started = Instant::now();
    let (confirmed, rejected) = tokio::join!(
        decide(&cp, &op, &batch, &proposal, "confirm", body),
        decide(&cp, &op, &batch, &proposal, "reject", body_r),
    );
    for r in [&confirmed, &rejected] {
        assert_eq!(r.status, 409, "{r:?}");
        assert_eq!(r.body["code"], "proposal_state", "{r:?}");
    }
    // Bounded by `lock_timeout` (5 s), not by the test's own timeout.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    holder.rollback().await.expect("release the lock");

    // Nothing was written while it waited, and the row is still decidable.
    assert_eq!(cp.count("pricing_baselines").await, 0);
    let r = decide(
        &cp,
        &op,
        &batch,
        &proposal,
        "confirm",
        json!({
            "model_id": model_id,
            "effective_from": db_now(&cp).await,
            "attested": true,
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(cp.count("pricing_baselines").await, 1);
}

/// A confirmation refused by the baseline write leaves the proposal exactly
/// as it was: no row, no version bump, still `pending`.
#[tokio::test]
async fn a_refused_baseline_leaves_the_proposal_pending() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &batch).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    let version = pricing_version(&cp).await;

    // A dead model: `insert_baseline` refuses it inside the transaction.
    let r = decide(
        &cp,
        &op,
        &batch,
        &proposal,
        "confirm",
        json!({
            "model_id": Uuid::new_v4(),
            "effective_from": db_now(&cp).await,
            "attested": true,
        }),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "validation_failed");
    assert_eq!(refusal_paths(&r), vec!["/model_id"]);

    assert_eq!(cp.count("pricing_baselines").await, 0);
    assert_eq!(pricing_version(&cp).await, version);
    let all = proposals(&cp, &op, &batch).await;
    let p = all.iter().find(|p| p["id"] == proposal["id"]).expect("it");
    assert_eq!(p["status"], "pending", "{p}");
    assert_eq!(p["attested_by"], Value::Null, "{p}");
    assert_eq!(audits(&cp).await.len(), 1, "only the upload's own row");
}

/// The attestation is the caller's own claim and is checked before the row is
/// read, so a bad body on a pid that names nothing answers `/attested`, not
/// `404`. The 404 answers once the body is good.
#[tokio::test]
async fn the_attestation_is_checked_before_the_proposal_is_read() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let batch = upload(&cp, &op).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let effective_from = db_now(&cp).await;

    for pid in ["not-a-uuid", &Uuid::new_v4().to_string()] {
        let path = format!("{PATH}/{}/proposals/{pid}/confirm", id(&batch));
        for body in [
            json!({"model_id": model_id, "effective_from": effective_from}),
            json!({"model_id": model_id, "effective_from": effective_from, "attested": false}),
        ] {
            let r = cp
                .send("POST", &path, Some(op.cookie()), Some(body.clone()))
                .await;
            assert_eq!(r.status, 422, "{pid} {body}: {r:?}");
            assert_eq!(refusal_paths(&r), vec!["/attested"], "{pid} {body}");
        }
        // The body is good: now the missing row decides.
        let r = cp
            .send(
                "POST",
                &path,
                Some(op.cookie()),
                Some(json!({
                    "model_id": model_id,
                    "effective_from": effective_from,
                    "attested": true,
                })),
            )
            .await;
        assert_eq!(r.status, 404, "{pid}: {r:?}");
        assert_eq!(r.body["code"], "not_found", "{pid}");
    }
}

/// A dataset of one entry, `google`/`model`: input 1 and output 2 now, and
/// input 3 and output 4 from `day` on.
fn dated_dataset(model: &str, day: chrono::NaiveDate) -> String {
    format!(
        r#"[{{"id":"google","pricing_urls":["https://ai.google.dev/pricing"],"models":[
             {{"id":"{model}","prices":[
               {{"prices":{{"input_mtok":1,"output_mtok":2}}}},
               {{"constraint":{{"start_date":"{day}"}},"prices":{{"input_mtok":3,"output_mtok":4}}}}]}}]}}]"#
    )
}

/// An upload of `data`, which must answer 201; the batch it answers.
async fn upload_ok(cp: &TestCp, op: &OperatorSession, data: &str) -> Value {
    let r = upload_text(cp, op, "inline-7c1", data).await;
    assert_eq!(r.status, 201, "{r:?}");
    r.body
}

/// `model`'s two proposals in `all`: the one in effect at upload, and the
/// dated one.
fn now_and_dated(all: &[Value], model: &str) -> (Value, Value) {
    let of = |dated: bool| {
        all.iter()
            .find(|p| p["model"] == model && p["effective_from"].is_null() != dated)
            .unwrap_or_else(|| panic!("no proposal of {model} (dated: {dated}): {all:?}"))
            .clone()
    };
    (of(false), of(true))
}

#[tokio::test]
async fn a_dated_proposal_starts_no_earlier_than_its_date() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "google", "m-dated-7c1").await;
    // Dated from the database's own clock, so the date stays ahead of the
    // upload whenever the test runs.
    let day = (db_now(&cp).await + chrono::TimeDelta::days(30)).date_naive();
    let batch = upload_ok(&cp, &op, &dated_dataset("m-dated-7c1", day)).await;
    let all = proposals(&cp, &op, &batch).await;
    let (_, dated) = now_and_dated(&all, "m-dated-7c1");
    assert_eq!(dated["flags"], json!([]), "{dated}");
    let starts: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(dated["effective_from"].clone()).expect("a date");
    assert_eq!(starts.date_naive(), day, "the dataset's own date");

    let early = decide(
        &cp,
        &op,
        &batch,
        &dated,
        "confirm",
        json!({
            "model_id": model_id,
            "effective_from": starts - chrono::TimeDelta::seconds(1),
            "attested": true,
        }),
    )
    .await;
    assert_eq!(early.status, 422, "{early:?}");
    assert_eq!(refusal_paths(&early), vec!["/effective_from"]);
    assert_eq!(
        early.body["details"][0]["message"],
        "生效时间不能早于该提议自己的生效时间"
    );
    assert_eq!(cp.count("pricing_baselines").await, 0);
    let still = proposals(&cp, &op, &batch).await;
    let p = still.iter().find(|p| p["id"] == dated["id"]).expect("it");
    assert_eq!(p["status"], "pending", "{p}");

    // From its own date on, it is written.
    let r = decide(
        &cp,
        &op,
        &batch,
        &dated,
        "confirm",
        json!({"model_id": model_id, "effective_from": starts, "attested": true}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["effective_from"], dated["effective_from"]);
    let current = current_baseline(&cp, &op, model_id).await;
    assert_eq!(
        current["revision_id"],
        Value::Null,
        "scheduled, not in force"
    );
}

/// An entry's earlier price cannot be confirmed to start once the same
/// upload says a later one has taken over: the dataset itself replaced it by
/// then, and the refusal names the later price's date.
#[tokio::test]
async fn an_earlier_price_cannot_start_after_the_entrys_later_one() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "google", "m-later-7c1").await;
    let day = (db_now(&cp).await + chrono::TimeDelta::days(10)).date_naive();
    let batch = upload_ok(&cp, &op, &dated_dataset("m-later-7c1", day)).await;
    let all = proposals(&cp, &op, &batch).await;
    let (now, dated) = now_and_dated(&all, "m-later-7c1");
    assert_eq!(now["flags"], json!([]), "{now}");
    let later: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(dated["effective_from"].clone()).expect("a date");

    // At the later price's own start, and a day past it: both refused.
    for start in [later, later + chrono::TimeDelta::days(1)] {
        let r = decide(
            &cp,
            &op,
            &batch,
            &now,
            "confirm",
            json!({"model_id": model_id, "effective_from": start, "attested": true}),
        )
        .await;
        assert_eq!(r.status, 422, "{start}: {r:?}");
        assert_eq!(refusal_paths(&r), vec!["/effective_from"], "{start}");
        let message = r.body["details"][0]["message"].as_str().expect("a message");
        assert!(message.contains(&day.to_string()), "{message}");
    }
    assert_eq!(cp.count("pricing_baselines").await, 0);
    let still = proposals(&cp, &op, &batch).await;
    let p = still.iter().find(|p| p["id"] == now["id"]).expect("it");
    assert_eq!(p["status"], "pending", "{p}");

    // Before it, the earlier price is what the dataset says, and it is
    // written.
    let r = decide(
        &cp,
        &op,
        &batch,
        &now,
        "confirm",
        json!({
            "model_id": model_id,
            "effective_from": later - chrono::TimeDelta::seconds(1),
            "attested": true,
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    // And the later one still confirms at its own date.
    let r = decide(
        &cp,
        &op,
        &batch,
        &dated,
        "confirm",
        json!({"model_id": model_id, "effective_from": later, "attested": true}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
}

#[tokio::test]
async fn a_superseded_proposal_cannot_be_confirmed() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let first = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &first).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    // A newer upload supersedes what the older one left pending.
    let second = upload(&cp, &op).await;
    let all = proposals(&cp, &op, &first).await;
    let superseded = all.iter().find(|p| p["id"] == proposal["id"]).expect("it");
    assert_eq!(superseded["status"], "superseded");

    for (action, body) in [
        (
            "confirm",
            json!({
                "model_id": model_id,
                "effective_from": db_now(&cp).await,
                "attested": true,
            }),
        ),
        ("reject", json!({"reason": "过期批次"})),
    ] {
        let r = decide(&cp, &op, &first, &proposal, action, body).await;
        assert_eq!(r.status, 409, "{action}: {r:?}");
        assert_eq!(r.body["code"], "proposal_state", "{action}");
    }
    assert_eq!(cp.count("pricing_baselines").await, 0);
    // The newer batch is untouched and still confirmable.
    let newer = current(
        &proposals(&cp, &op, &second).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    assert_eq!(newer["status"], "pending");
}

#[tokio::test]
async fn another_batchs_proposal_is_not_found() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let first = upload(&cp, &op).await;
    let proposal = current(
        &proposals(&cp, &op, &first).await,
        "anthropic",
        "claude-opus-5-5",
    )
    .clone();
    let second = upload(&cp, &op).await;
    let body = json!({
        "model_id": model_id,
        "effective_from": db_now(&cp).await,
        "attested": true,
    });

    // The older batch's proposal addressed under the newer batch: not found,
    // though it exists and is superseded.
    let path = format!("{PATH}/{}/proposals/{}/confirm", id(&second), id(&proposal));
    let r = cp
        .send("POST", &path, Some(op.cookie()), Some(body.clone()))
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(r.body["code"], "not_found");
    for pid in [Uuid::new_v4().to_string(), "not-a-uuid".to_string()] {
        let path = format!("{PATH}/{}/proposals/{pid}/confirm", id(&second));
        let r = cp
            .send("POST", &path, Some(op.cookie()), Some(body.clone()))
            .await;
        assert_eq!(r.status, 404, "{pid}: {r:?}");
        let path = format!("{PATH}/{}/proposals/{pid}/reject", id(&second));
        let r = cp
            .send(
                "POST",
                &path,
                Some(op.cookie()),
                Some(json!({"reason": "无此提议"})),
            )
            .await;
        assert_eq!(r.status, 404, "{pid}: {r:?}");
    }
    assert_eq!(cp.count("pricing_baselines").await, 0);
}

#[tokio::test]
async fn who_may_decide() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model_id = model(&cp, &op, "anthropic", "claude-opus-5-5").await;
    let batch = upload(&cp, &op).await;
    let all = proposals(&cp, &op, &batch).await;
    let proposal = current(&all, "anthropic", "claude-opus-5-5").clone();
    let other = all
        .iter()
        .find(|p| p["id"] != proposal["id"] && p["flags"] == json!([]))
        .expect("a second confirmable proposal")
        .clone();
    let body = json!({
        "model_id": model_id,
        "effective_from": db_now(&cp).await,
        "attested": true,
    });

    let r = decide(&cp, &viewer, &batch, &proposal, "confirm", body.clone()).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(r.body["code"], "forbidden");
    let r = decide(
        &cp,
        &viewer,
        &batch,
        &proposal,
        "reject",
        json!({"reason": "x"}),
    )
    .await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(cp.count("pricing_baselines").await, 0);
    assert_eq!(audits(&cp).await.len(), 1, "only the upload's own row");

    let r = decide(&cp, &op, &batch, &proposal, "confirm", body).await;
    assert_eq!(r.status, 201, "{r:?}");
    let r = decide(
        &cp,
        &op,
        &batch,
        &other,
        "reject",
        json!({"reason": "与官方价格页不符"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
}
