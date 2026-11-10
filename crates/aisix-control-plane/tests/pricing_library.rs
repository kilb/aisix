//! The price library over HTTP: a fetch from its address (here a stand-in
//! the test serves, never the internet), an upload that fills it offline,
//! the browse, and the sync of baselines from it with its preview.
//!
//! The library's text is the committed snapshot of the real dataset, and
//! every expected price is read from that file's own text rather than from
//! the parser. The route table's own fixtures are in `tests/roles.rs`.

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::pricing::library::{FETCH_TIMEOUT, MAX_LIBRARY_BYTES, MAX_SYNC_MODELS};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{routing::any, Router};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::world::{db_now, public_model};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The committed snapshot: the dataset's published `v2/data.json` at the
/// commit in its file name.
const SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/e2e/fixtures/pricing-import/genai-prices-v2-7de757d.json"
);

fn snapshot() -> String {
    std::fs::read_to_string(SNAPSHOT).expect("read the dataset snapshot")
}

/// What the snapshot itself says of `provider`'s `model`: the provider's
/// first pricing page, and the model's flat prices, each as written.
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

/// A decimal text as millionths, so `2.5` and `2.500000` compare equal.
fn micros(text: &str) -> i64 {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    assert!(fraction.len() <= 6, "{text}");
    let whole: i64 = whole.parse().expect("digits");
    let fraction: i64 = format!("{fraction:0<6}").parse().expect("digits");
    whole * 1_000_000 + fraction
}

/// A card's `default` base rate of `dimension`, as millionths.
fn rate(card: &Value, dimension: &str) -> i64 {
    micros(
        card["default"]["base"][dimension]
            .as_str()
            .unwrap_or_else(|| panic!("no {dimension} in {card}")),
    )
}

// ---------------------------------------------------------------------------
// The stand-in for the dataset's address.
// ---------------------------------------------------------------------------

/// What the stand-in answers one path with.
#[derive(Clone)]
enum Answer {
    /// A status and a body, with its length.
    Body(StatusCode, Vec<u8>),
    /// `200` with `n` bytes streamed in chunks, with no length announced.
    Streamed(usize),
    /// A redirect to `location`.
    Redirect(String),
    /// [`Answer::Body`] after `delay`.
    Slow(Duration, Vec<u8>),
}

type Answers = Arc<dyn Fn(&str) -> Answer + Send + Sync>;

#[derive(Clone)]
struct StandIn {
    hits: Arc<Mutex<Vec<String>>>,
    answers: Answers,
}

/// A real HTTP server on an ephemeral loopback port, answering each path as
/// `answers` says and recording each path it was asked for.
struct Fixture {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    served: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(answers: impl Fn(&str) -> Answer + Send + Sync + 'static) -> Fixture {
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let state = StandIn {
            hits: hits.clone(),
            answers: Arc::new(answers),
        };
        let router = Router::new().fallback(any(answer)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the stand-in");
        let addr = listener.local_addr().expect("the stand-in's address");
        let served = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Fixture { addr, hits, served }
    }

    /// `http://127.0.0.1:<port><path>`.
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// The paths it was asked for, in order.
    fn hits(&self) -> Vec<String> {
        self.hits.lock().expect("the stand-in's hits").clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.served.abort();
    }
}

async fn answer(State(state): State<StandIn>, uri: Uri, _body: Bytes) -> Response {
    let path = uri.path().to_string();
    state
        .hits
        .lock()
        .expect("the stand-in's hits")
        .push(path.clone());
    match (state.answers)(&path) {
        Answer::Body(status, body) => (status, body).into_response(),
        Answer::Streamed(n) => {
            let chunk = vec![b' '; 64 << 10];
            let chunks: Vec<Result<Bytes, std::io::Error>> = (0..n.div_ceil(chunk.len()))
                .map(|i| {
                    let len = chunk.len().min(n - i * chunk.len());
                    Ok(Bytes::copy_from_slice(&chunk[..len]))
                })
                .collect();
            Response::new(Body::from_stream(futures::stream::iter(chunks)))
        }
        Answer::Redirect(location) => (
            StatusCode::FOUND,
            [(axum::http::header::LOCATION, location)],
        )
            .into_response(),
        Answer::Slow(delay, body) => {
            tokio::time::sleep(delay).await;
            (StatusCode::OK, body).into_response()
        }
    }
}

/// A stand-in serving the snapshot at `/data.json`, and `404` elsewhere.
async fn serving_the_snapshot() -> Fixture {
    let text = snapshot().into_bytes();
    Fixture::start(move |path| match path {
        "/data.json" => Answer::Body(StatusCode::OK, text.clone()),
        _ => Answer::Body(StatusCode::NOT_FOUND, b"404: Not Found".to_vec()),
    })
    .await
}

// ---------------------------------------------------------------------------
// Requests.
// ---------------------------------------------------------------------------

const UPLOAD: &str = "/admin/pricing/imports";
const LIBRARY: &str = "/admin/pricing/library";
const FETCH: &str = "/admin/pricing/library/fetch";
const SYNC: &str = "/admin/pricing/library/sync";

/// `POST /admin/pricing/imports` of the snapshot as `op`, sent as its own
/// text, which must answer 201.
async fn upload(cp: &TestCp, op: &OperatorSession, dataset_ref: &str) {
    let body = format!(
        r#"{{"dataset_ref":{},"data":{}}}"#,
        json!(dataset_ref),
        snapshot()
    );
    let r = cp
        .send_raw(
            "POST",
            UPLOAD,
            Some(op.cookie()),
            Some("application/json"),
            Some(body.into_bytes()),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
}

async fn fetch(cp: &TestCp, op: &OperatorSession) -> Resp {
    cp.send("POST", FETCH, Some(op.cookie()), None).await
}

/// `GET path` as `op`, which must answer 200.
async fn get(cp: &TestCp, op: &OperatorSession, path: &str) -> Value {
    let r = cp.send("GET", path, Some(op.cookie()), None).await;
    assert_eq!(r.status, 200, "GET {path}: {r:?}");
    r.body
}

/// How many library rows are stored.
async fn stored(cp: &TestCp) -> i64 {
    cp.count("pricing_library").await
}

/// The audit rows of `action`, oldest first.
async fn audits(cp: &TestCp, action: &str) -> Vec<Value> {
    sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = $1 ORDER BY id")
        .bind(action)
        .fetch_all(cp.db().pool())
        .await
        .expect("read the audit rows")
}

/// How many baselines are written.
async fn baselines(cp: &TestCp) -> i64 {
    cp.count("pricing_baselines").await
}

/// The refused paths of a `422` or `409`.
fn paths(r: &Resp) -> Vec<String> {
    r.body["details"]
        .as_array()
        .map(|d| {
            d.iter()
                .map(|d| d["path"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Filling the library.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fetch_stores_the_dataset_its_address_serves() {
    let stand_in = serving_the_snapshot().await;
    let Some(cp) = TestCp::start_with_library(&stand_in.url("/data.json")).await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let before = get(&cp, &op, LIBRARY).await;
    assert_eq!(before["library"], Value::Null, "{before}");

    let r = fetch(&cp, &op).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(stand_in.hits(), ["/data.json"]);
    let text = snapshot();
    let text = text.trim();
    assert_eq!(r.body["dataset"], "pydantic/genai-prices");
    assert_eq!(r.body["origin"], "fetch");
    assert_eq!(r.body["source_url"], stand_in.url("/data.json"));
    // The array the dataset is names no revision of its own.
    assert_eq!(r.body["source_ref"], Value::Null);
    assert_eq!(r.body["bytes"], json!(text.len()));
    assert_eq!(
        r.body["sha256"],
        json!(hex::encode(Sha256::digest(text.as_bytes())))
    );
    assert!(
        r.body["entries"].as_i64().unwrap_or_default() > 1_000,
        "{r:?}"
    );
    assert_eq!(r.body["created_by"]["email"], json!(op.email));
    assert!(r.body["fetched_at"].is_string(), "{r:?}");

    let listed = get(&cp, &op, LIBRARY).await;
    assert_eq!(listed["library"], r.body);
    let rows = audits(&cp, "pricing.library.update").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["origin"], "fetch");

    // A second fetch replaces the first: one copy is kept.
    let again = fetch(&cp, &op).await;
    assert_eq!(again.status, 201, "{again:?}");
    assert_ne!(again.body["id"], r.body["id"]);
    assert_eq!(stored(&cp).await, 1);
    assert_eq!(
        audits(&cp, "pricing.library.update").await[1]["replaced"],
        json!([r.body["id"]])
    );
}

#[tokio::test]
async fn a_fetch_follows_a_redirect_only_on_its_own_origin() {
    let stand_in = Arc::new(std::sync::OnceLock::<u16>::new());
    let port = stand_in.clone();
    let text = snapshot().into_bytes();
    let fixture = Fixture::start(move |path| match path {
        "/moved" => Answer::Redirect("/data.json".to_string()),
        // The same server under another name is another origin.
        "/away" => Answer::Redirect(format!(
            "http://localhost:{}/data.json",
            port.get().expect("the port is set")
        )),
        "/data.json" => Answer::Body(StatusCode::OK, text.clone()),
        _ => Answer::Body(StatusCode::NOT_FOUND, Vec::new()),
    })
    .await;
    stand_in.set(fixture.addr.port()).expect("set once");

    let Some(cp) = TestCp::start_with_library(&fixture.url("/moved")).await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let r = fetch(&cp, &op).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(fixture.hits(), ["/moved", "/data.json"]);

    let Some(cp) = TestCp::start_with_library(&fixture.url("/away")).await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let r = fetch(&cp, &op).await;
    assert_eq!(r.status, 502, "{r:?}");
    assert_eq!(r.body["code"], "library_fetch_failed", "{r:?}");
    assert_eq!(paths(&r), ["/source"]);
    // Not followed: the other origin was never asked.
    assert_eq!(fixture.hits(), ["/moved", "/data.json", "/away"]);
    assert_eq!(stored(&cp).await, 0);
}

#[tokio::test]
async fn a_body_past_the_cap_is_refused_and_stores_nothing() {
    let fixture = Fixture::start(|path| match path {
        // Announced: refused before it is read.
        "/announced" => Answer::Body(StatusCode::OK, vec![b' '; MAX_LIBRARY_BYTES + 1]),
        // Not announced: refused once the cap is passed while reading.
        _ => Answer::Streamed(MAX_LIBRARY_BYTES + 1),
    })
    .await;
    for path in ["/announced", "/streamed"] {
        let Some(cp) = TestCp::start_with_library(&fixture.url(path)).await else {
            return;
        };
        let op = cp.admin_login_seeded(OperatorRole::Operator).await;
        let r = fetch(&cp, &op).await;
        assert_eq!(r.status, 502, "{path}: {r:?}");
        assert_eq!(r.body["code"], "library_fetch_failed", "{path}: {r:?}");
        assert_eq!(
            r.body["details"][0]["message"], "价格库数据超过 10 MiB",
            "{path}"
        );
        assert_eq!(stored(&cp).await, 0, "{path}");
    }
}

#[tokio::test]
async fn a_fetch_past_its_timeout_stores_nothing() {
    let text = snapshot().into_bytes();
    let fixture =
        Fixture::start(move |_| Answer::Slow(FETCH_TIMEOUT + Duration::from_secs(5), text.clone()))
            .await;
    let Some(cp) = TestCp::start_with_library(&fixture.url("/data.json")).await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let started = Instant::now();
    let r = fetch(&cp, &op).await;
    let took = started.elapsed();
    assert_eq!(r.status, 504, "{r:?}");
    assert_eq!(r.body["code"], "library_fetch_timeout", "{r:?}");
    assert!(
        took >= FETCH_TIMEOUT - Duration::from_millis(500)
            && took < FETCH_TIMEOUT + Duration::from_secs(3),
        "answered after {took:?}"
    );
    assert_eq!(stored(&cp).await, 0);
}

#[tokio::test]
async fn an_answer_that_is_not_the_dataset_stores_nothing_and_keeps_the_library() {
    let fixture = Fixture::start(|path| match path {
        "/html" => Answer::Body(StatusCode::OK, b"<html>rate limited</html>".to_vec()),
        "/object" => Answer::Body(StatusCode::OK, br#"{"providers": []}"#.to_vec()),
        _ => Answer::Body(
            StatusCode::FORBIDDEN,
            b"denied for Authorization: Bearer sk-live-4f1c9e2b7a6d5c3e8f0a1b2c".to_vec(),
        ),
    })
    .await;
    for path in ["/html", "/object", "/denied"] {
        let Some(cp) = TestCp::start_with_library(&fixture.url(path)).await else {
            return;
        };
        let op = cp.admin_login_seeded(OperatorRole::Operator).await;
        // A library in use, uploaded offline, which a failed fetch leaves.
        upload(&cp, &op, "v2-7de757d").await;
        let before = get(&cp, &op, LIBRARY).await["library"].clone();

        let r = fetch(&cp, &op).await;
        assert_eq!(r.status, 502, "{path}: {r:?}");
        assert_eq!(r.body["code"], "library_fetch_failed", "{path}: {r:?}");
        let detail = r.body["details"][0]["message"].as_str().unwrap_or_default();
        if path == "/denied" {
            // The answer's own words, scrubbed: no credential it quotes.
            assert!(detail.starts_with("价格库地址返回 HTTP 403"), "{detail}");
            assert!(
                !detail.contains("sk-live-4f1c9e2b7a6d5c3e8f0a1b2c"),
                "{detail}"
            );
        } else {
            assert_eq!(detail, "下载的内容不是可识别的价格数据集", "{path}");
        }
        assert_eq!(get(&cp, &op, LIBRARY).await["library"], before, "{path}");
        assert_eq!(stored(&cp).await, 1, "{path}");
    }
}

#[tokio::test]
async fn an_upload_fills_the_library_and_the_browse_finds_by_name_or_provider() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;

    let found = get(&cp, &viewer, &format!("{LIBRARY}?q=COMMAND-A")).await;
    // One change, one audit row: the upload's own names the library.
    let rows = audits(&cp, "pricing.import.upload").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["library_id"], found["library"]["id"]);
    assert!(audits(&cp, "pricing.library.update").await.is_empty());
    assert_eq!(found["library"]["origin"], "upload");
    assert_eq!(found["library"]["source_ref"], "v2-7de757d");
    assert_eq!(found["library"]["source_url"], Value::Null);
    let entry = found["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|e| e["provider"] == "cohere" && e["model"] == "command-a")
        .unwrap_or_else(|| panic!("cohere/command-a in {found}"))
        .clone();
    let (page, prices) = source("cohere", "command-a");
    assert_eq!(rate(&entry["card"], "input"), micros(&prices["input_mtok"]));
    assert_eq!(
        rate(&entry["card"], "output"),
        micros(&prices["output_mtok"])
    );
    assert_eq!(entry["pricing_url"], json!(page));
    assert_eq!(entry["flags"], json!([]));
    // Every listed entry names the text searched for.
    for e in found["items"].as_array().expect("items") {
        assert!(e.to_string().to_lowercase().contains("command-a"), "{e}");
    }

    // By provider, and capped by `limit` while `total` counts them all.
    let cohere = get(&cp, &viewer, &format!("{LIBRARY}?q=cohere&limit=2")).await;
    assert_eq!(
        cohere["items"].as_array().map(Vec::len),
        Some(2),
        "{cohere}"
    );
    assert!(cohere["total"].as_u64().unwrap_or_default() > 2, "{cohere}");
}

// ---------------------------------------------------------------------------
// Syncing.
// ---------------------------------------------------------------------------

/// An upstream account of `provider`; its id.
async fn account(cp: &TestCp, op: &OperatorSession, provider: &str) -> Uuid {
    speaking(cp, op, provider, None).await
}

/// An upstream account of `provider` speaking `adapter`; its id.
async fn speaking(
    cp: &TestCp,
    op: &OperatorSession,
    provider: &str,
    adapter: Option<&str>,
) -> Uuid {
    let mut doc = json!({
        "display_name": format!("acct-lib-{}", Uuid::new_v4().simple()),
        "provider": provider,
        "api_key": "sk-library-upstream-3d9",
        "groups": ["shared"],
    });
    if let Some(adapter) = adapter {
        doc["adapter"] = json!(adapter);
    }
    let r = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(op.cookie()),
            Some(json!({ "doc": doc })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid")
}

/// A public model `name` served by a new account of `provider`; its id.
async fn served(cp: &TestCp, op: &OperatorSession, name: &str, provider: &str) -> Uuid {
    let pk = account(cp, op, provider).await;
    served_by(cp, op, name, pk).await
}

/// A public model `name` served by the account `pk`; its id.
async fn served_by(cp: &TestCp, op: &OperatorSession, name: &str, pk: Uuid) -> Uuid {
    let public = public_model(cp, op, name).await;
    let r = cp
        .send(
            "PUT",
            &format!("/admin/provider-keys/{pk}/channel-models/{public}"),
            Some(op.cookie()),
            Some(json!({"priority": 0, "weight": 1})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    public
}

/// A hand-made direct model named `name` of `provider`; its id.
async fn direct(cp: &TestCp, op: &OperatorSession, name: &str, provider: &str) -> Uuid {
    let pk = account(cp, op, provider).await;
    direct_on(cp, op, name, provider, pk).await
}

/// A hand-made direct model named `name` of `provider` on the account `pk`;
/// its id.
async fn direct_on(
    cp: &TestCp,
    op: &OperatorSession,
    name: &str,
    provider: &str,
    pk: Uuid,
) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(op.cookie()),
            Some(json!({"doc": {
                "display_name": name,
                "provider": provider,
                "model_name": name,
                "provider_key_id": pk,
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid")
}

/// A manual baseline of `model` with `card`, starting now.
async fn manual(cp: &TestCp, op: &OperatorSession, model: Uuid, card: &Value) {
    let r = cp
        .send(
            "POST",
            "/admin/pricing/baselines",
            Some(op.cookie()),
            Some(json!({
                "model_id": model,
                "effective_from": db_now(cp).await,
                "card": card,
                "note": "手动录入",
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
}

/// The preview, every model or one.
async fn preview(cp: &TestCp, op: &OperatorSession, model: Option<Uuid>) -> Value {
    match model {
        Some(id) => get(cp, op, &format!("{SYNC}?model_id={id}")).await,
        None => get(cp, op, SYNC).await,
    }
}

/// The preview's item of `model`.
fn item(preview: &Value, model: Uuid) -> Value {
    preview["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["model_id"] == json!(model))
        .unwrap_or_else(|| panic!("{model} in {preview}"))
        .clone()
}

/// A sync request's item for a previewed `item`, as the page sends it.
fn listed(item: &Value) -> Value {
    json!({
        "model_id": item["model_id"],
        "provider": item["entry"]["provider"],
        "model": item["entry"]["model"],
        "card": item["entry"]["card"],
    })
}

async fn sync(cp: &TestCp, op: &OperatorSession, body: Value) -> Resp {
    cp.send("POST", SYNC, Some(op.cookie()), Some(body)).await
}

/// The baseline in force of `model`, as the baseline list shows it.
async fn baseline_of(cp: &TestCp, op: &OperatorSession, model: Uuid) -> Value {
    let list = get(cp, op, "/admin/pricing/baselines").await;
    list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["model_id"] == json!(model))
        .unwrap_or_else(|| panic!("{model} in {list}"))["current"]
        .clone()
}

#[tokio::test]
async fn the_preview_names_what_a_sync_would_do_to_each_model() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let none = preview(&cp, &op, None).await;
    assert_eq!(none["library"], Value::Null);
    assert_eq!(none["items"], json!([]));
    upload(&cp, &op, "v2-7de757d").await;

    // One provider carries the name: matched whoever serves it.
    let create = served(&cp, &op, "command-a", "openai").await;
    // Matched, with a baseline the library differs from.
    let update = served(&cp, &op, "claude-v1", "anthropic").await;
    manual(
        &cp,
        &op,
        update,
        &json!({"default": {"base": {"input": "1", "output": "2"}}}),
    )
    .await;
    // Two providers carry the name, and the model's own is neither.
    let ambiguous = served(&cp, &op, "gpt-4o", "relay-7c1").await;
    let unmatched = served(&cp, &op, "no-such-model-lib-7c1", "relay-7c1").await;
    // Its own provider's entry carries a price no card meters.
    let flagged = direct(&cp, &op, "gpt-4o-mini", "openai").await;

    let p = preview(&cp, &op, None).await;
    assert_eq!(p["library"]["source_ref"], "v2-7de757d");
    assert_eq!(p["cap"], json!(MAX_SYNC_MODELS));
    let (_, prices) = source("cohere", "command-a");
    let c = item(&p, create);
    assert_eq!(c["state"], "create", "{c}");
    assert_eq!(c["matched_by"], "exact", "{c}");
    assert_eq!(
        (c["entry"]["provider"].clone(), c["entry"]["model"].clone()),
        (json!("cohere"), json!("command-a"))
    );
    assert_eq!(
        rate(&c["entry"]["card"], "input"),
        micros(&prices["input_mtok"])
    );
    assert_eq!(c["current"], Value::Null);

    let (_, prices) = source("anthropic", "claude-v1");
    let u = item(&p, update);
    assert_eq!(u["state"], "update", "{u}");
    assert_eq!(rate(&u["current"]["card"], "input"), micros("1"));
    assert_eq!(
        rate(&u["entry"]["card"], "input"),
        micros(&prices["input_mtok"])
    );

    let a = item(&p, ambiguous);
    assert_eq!(a["state"], "ambiguous", "{a}");
    assert_eq!(a["entry"], Value::Null);
    let mut candidates: Vec<String> = a["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|c| {
            format!(
                "{}/{}",
                c["provider"].as_str().unwrap_or_default(),
                c["model"].as_str().unwrap_or_default()
            )
        })
        .collect();
    candidates.sort();
    assert_eq!(candidates, ["github-copilot/gpt-4o", "openai/gpt-4o"]);

    assert_eq!(item(&p, unmatched)["state"], "unmatched");
    let f = item(&p, flagged);
    assert_eq!(f["state"], "flagged", "{f}");
    assert_eq!(
        (f["entry"]["provider"].clone(), f["entry"]["model"].clone()),
        (json!("openai"), json!("gpt-4o-mini"))
    );
    assert_ne!(f["entry"]["flags"], json!([]));

    // A baseline equal to the library's card: nothing to do.
    let card = c["entry"]["card"].clone();
    manual(&cp, &op, create, &card).await;
    assert_eq!(
        item(&preview(&cp, &op, None).await, create)["state"],
        "same"
    );

    // One model alone; one that is not priceable is not found.
    let one = preview(&cp, &op, Some(update)).await;
    assert_eq!(one["items"].as_array().map(Vec::len), Some(1), "{one}");
    assert_eq!(one["items"][0]["state"], "update");
    let r = cp
        .send(
            "GET",
            &format!("{SYNC}?model_id={}", Uuid::new_v4()),
            Some(op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
}

#[tokio::test]
async fn a_sync_writes_exactly_the_listed_models_as_attested_third_party_baselines() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    let a = served(&cp, &op, "command-a", "cohere").await;
    let b = served(&cp, &op, "command-r", "cohere").await;
    let p = preview(&cp, &op, None).await;
    assert_eq!(item(&p, a)["state"], "create");
    assert_eq!(item(&p, b)["state"], "create");
    // Created after the preview: it would sync, and was never shown.
    let later = served(&cp, &op, "claude-v1", "anthropic").await;
    let before = baselines(&cp).await;

    let r = sync(
        &cp,
        &op,
        json!({
            "library_id": p["library"]["id"],
            "attested": true,
            "items": [listed(&item(&p, a))],
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        r.body["revisions"].as_array().map(Vec::len),
        Some(1),
        "{r:?}"
    );
    // Exactly the one listed: not `b`, which the preview also showed, and
    // not `later`, which it never did.
    assert_eq!(baselines(&cp).await, before + 1);
    let written = baseline_of(&cp, &op, a).await;
    let (page, prices) = source("cohere", "command-a");
    assert_eq!(written["source_kind"], "third_party");
    assert_eq!(
        rate(&written["card"], "input"),
        micros(&prices["input_mtok"])
    );
    assert_eq!(
        rate(&written["card"], "output"),
        micros(&prices["output_mtok"])
    );
    let source = &written["source"];
    assert_eq!(source["dataset"], "pydantic/genai-prices");
    assert_eq!(source["dataset_ref"], "v2-7de757d");
    assert_eq!(source["library_id"], p["library"]["id"]);
    assert_eq!(source["library_entry"], "cohere/command-a");
    assert_eq!(source["pricing_url"], json!(page));
    assert_eq!(source["attested_by"], json!(op.id));
    assert!(source["attested_at"].is_string(), "{source}");
    assert_eq!(source.get("import_id"), None, "{source}");
    assert_eq!(written["created_by"]["email"], json!(op.email));
    assert_eq!(baseline_of(&cp, &op, b).await, Value::Null);
    assert_eq!(baseline_of(&cp, &op, later).await, Value::Null);

    let rows = audits(&cp, "pricing.library.sync").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["model_id"], json!(a));
    assert_eq!(rows[0]["attested"], json!(true));
    assert_eq!(rows[0]["attestation"], "已对照官方价格页核对");
    assert_eq!(rows[0]["library_entry"], "cohere/command-a");

    let p = preview(&cp, &op, None).await;
    assert_eq!(item(&p, a)["state"], "same");
    assert_eq!(item(&p, b)["state"], "create");
}

#[tokio::test]
async fn a_sync_without_the_attestation_or_past_the_cap_writes_nothing() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    let a = served(&cp, &op, "command-a", "cohere").await;
    let p = preview(&cp, &op, None).await;
    let one = listed(&item(&p, a));
    let before = baselines(&cp).await;

    for attested in [json!(false), Value::Null] {
        let mut body = json!({"library_id": p["library"]["id"], "items": [one]});
        if !attested.is_null() {
            body["attested"] = attested;
        }
        let r = sync(&cp, &op, body).await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(paths(&r), ["/attested"]);
        assert_eq!(
            r.body["details"][0]["message"],
            "请确认已对照官方价格页核对"
        );
    }
    let r = sync(
        &cp,
        &op,
        json!({"library_id": p["library"]["id"], "attested": true, "items": []}),
    )
    .await;
    assert_eq!(
        (r.status, paths(&r)),
        (422, vec!["/items".to_string()]),
        "{r:?}"
    );
    let many: Vec<Value> = (0..=MAX_SYNC_MODELS)
        .map(|_| {
            let mut i = one.clone();
            i["model_id"] = json!(Uuid::new_v4());
            i
        })
        .collect();
    let r = sync(
        &cp,
        &op,
        json!({"library_id": p["library"]["id"], "attested": true, "items": many}),
    )
    .await;
    assert_eq!(
        (r.status, paths(&r)),
        (422, vec!["/items".to_string()]),
        "{r:?}"
    );
    let r = sync(
        &cp,
        &op,
        json!({"library_id": p["library"]["id"], "attested": true, "items": [one, one]}),
    )
    .await;
    assert_eq!(
        (r.status, paths(&r)),
        (422, vec!["/items/1/model_id".to_string()]),
        "{r:?}"
    );
    assert_eq!(baselines(&cp).await, before);
    assert!(audits(&cp, "pricing.library.sync").await.is_empty());
}

#[tokio::test]
async fn a_sync_of_a_preview_that_moved_is_refused_and_writes_nothing() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    let a = served(&cp, &op, "command-a", "cohere").await;
    let b = served(&cp, &op, "claude-v1", "anthropic").await;
    // Two providers price it, each with a card a sync could write, and the
    // model's own provider is neither.
    let ambiguous = served(&cp, &op, "command-r", "relay-7c1").await;
    let p = preview(&cp, &op, None).await;
    let one = listed(&item(&p, a));
    let two = listed(&item(&p, b));
    assert_eq!(item(&p, ambiguous)["state"], "ambiguous");
    let library = p["library"]["id"].clone();
    let before = baselines(&cp).await;

    // A card other than the library's: not what the operator attested.
    let mut moved = two.clone();
    moved["card"] = json!({"default": {"base": {"input": "0.01", "output": "0.02"}}});
    // An ambiguous model, listed under an entry that is none of its
    // candidates, with that entry's own card.
    let picked = json!({
        "model_id": ambiguous,
        "provider": "cohere",
        "model": "command-a",
        "card": one["card"],
    });
    // An entry other than the one matched, with that entry's card.
    let mut other = two.clone();
    other["model"] = json!("command-a");
    other["card"] = one["card"].clone();
    for (n, wrong) in [moved, picked, other].into_iter().enumerate() {
        let r = sync(
            &cp,
            &op,
            json!({"library_id": library, "attested": true, "items": [one, wrong]}),
        )
        .await;
        assert_eq!(r.status, 409, "case {n}: {r:?}");
        assert_eq!(r.body["code"], "library_stale", "case {n}");
        assert_eq!(paths(&r), ["/items/1"], "case {n}");
    }
    // Already in force: nothing to sync.
    manual(&cp, &op, a, &one["card"]).await;
    let before_same = baselines(&cp).await;
    let r = sync(
        &cp,
        &op,
        json!({"library_id": library, "attested": true, "items": [one]}),
    )
    .await;
    assert_eq!(
        (r.status, paths(&r)),
        (409, vec!["/items/0".to_string()]),
        "{r:?}"
    );
    assert_eq!(baselines(&cp).await, before_same);
    // A newer library replaced the one previewed.
    upload(&cp, &op, "v2-7de757d-again").await;
    let r = sync(
        &cp,
        &op,
        json!({"library_id": library, "attested": true, "items": [one]}),
    )
    .await;
    assert_eq!(
        (r.status, paths(&r)),
        (409, vec!["/library_id".to_string()]),
        "{r:?}"
    );
    assert_eq!(
        baselines(&cp).await,
        before + 1,
        "only the manual baseline was written"
    );
    assert!(audits(&cp, "pricing.library.sync").await.is_empty());
}

// ---------------------------------------------------------------------------
// Narrowing by the serving accounts' protocol, and choosing a candidate.
// ---------------------------------------------------------------------------

const BINDINGS: &str = "/admin/pricing/library/bindings";

/// The candidate of `provider` among a previewed ambiguous `item`'s.
fn candidate(item: &Value, provider: &str) -> Value {
    item["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .find(|c| c["provider"] == provider)
        .unwrap_or_else(|| panic!("a {provider} candidate in {item}"))
        .clone()
}

/// A sync request's item listing `model` under `candidate`, as the page
/// sends a choice.
fn choosing(model: Uuid, candidate: &Value) -> Value {
    json!({
        "model_id": model,
        "provider": candidate["provider"],
        "model": candidate["model"],
        "card": candidate["card"],
    })
}

/// The entry recorded as chosen for `model`, as `provider/model`.
async fn binding(cp: &TestCp, model: Uuid) -> Option<String> {
    sqlx::query_scalar(
        "SELECT provider || '/' || model FROM pricing_library_bindings WHERE model_id = $1",
    )
    .bind(model)
    .fetch_optional(cp.db().pool())
    .await
    .expect("read the binding")
}

/// `item`'s matched entry as `provider/model`, and how it matched.
fn matched(item: &Value) -> (String, Value) {
    (
        format!(
            "{}/{}",
            item["entry"]["provider"].as_str().unwrap_or_default(),
            item["entry"]["model"].as_str().unwrap_or_default()
        ),
        item["matched_by"].clone(),
    )
}

#[tokio::test]
async fn an_account_of_the_platforms_own_provider_finds_the_entry_of_the_protocol_it_speaks() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    // The production shape: a provider id of the platform's own, and the
    // protocol the account speaks. The first party, a cloud and others all
    // price these names.
    let anthropic = speaking(&cp, &op, "matrix-7c1", Some("anthropic")).await;
    let openai = speaking(&cp, &op, "matrix-7c1", Some("openai")).await;
    let claude = served_by(&cp, &op, "claude-opus-4-5", anthropic).await;
    let gpt = direct_on(&cp, &op, "gpt-4o", "matrix-7c1", openai).await;
    // No adapter: nothing to narrow on.
    let bare = served(&cp, &op, "claude-opus-4-7", "matrix-7c1").await;
    // The model's own provider wins over the protocol its account speaks.
    let google = speaking(&cp, &op, "google", Some("anthropic")).await;
    let own = served_by(&cp, &op, "claude-sonnet-4-5", google).await;

    let p = preview(&cp, &op, None).await;
    let c = item(&p, claude);
    assert_eq!(
        matched(&c),
        ("anthropic/claude-opus-4-5".to_string(), json!("adapter")),
        "{c}"
    );
    assert_eq!(c["state"], "create", "{c}");
    let (_, prices) = source("anthropic", "claude-opus-4-5");
    assert_eq!(
        rate(&c["entry"]["card"], "input"),
        micros(&prices["input_mtok"])
    );
    let g = item(&p, gpt);
    assert_eq!(
        matched(&g),
        ("openai/gpt-4o".to_string(), json!("adapter")),
        "{g}"
    );
    let b = item(&p, bare);
    assert_eq!(b["state"], "ambiguous", "{b}");
    assert!(
        b["candidates"].as_array().map(Vec::len).unwrap_or_default() > 1,
        "{b}"
    );
    let o = item(&p, own);
    assert_eq!(
        matched(&o),
        ("google/claude-sonnet-4-5".to_string(), json!("exact")),
        "{o}"
    );

    // Matched, so synced as any other match, and no choice is recorded.
    let r = sync(
        &cp,
        &op,
        json!({"library_id": p["library"]["id"], "attested": true, "items": [listed(&c)]}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        baseline_of(&cp, &op, claude).await["source"]["library_entry"],
        "anthropic/claude-opus-4-5"
    );
    assert_eq!(binding(&cp, claude).await, None);
    assert!(audits(&cp, "pricing.library.bind").await.is_empty());
}

#[tokio::test]
async fn an_ambiguous_model_syncs_from_the_candidate_chosen_and_keeps_the_choice() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    // Two providers price it, at different prices, and its own is neither.
    let model = served(&cp, &op, "command-r", "relay-7c1").await;
    let p = preview(&cp, &op, None).await;
    let a = item(&p, model);
    assert_eq!(a["state"], "ambiguous", "{a}");
    assert_eq!(a["binding"], Value::Null, "{a}");
    // Each candidate carries what choosing it takes: its rates, its page,
    // its card, and what a sync from it would do.
    let (page, prices) = source("cohere", "command-r");
    let cohere = candidate(&a, "cohere");
    assert_eq!(cohere["model"], "command-r");
    assert_eq!(
        micros(cohere["input"].as_str().unwrap_or("x")),
        micros(&prices["input_mtok"])
    );
    assert_eq!(
        micros(cohere["output"].as_str().unwrap_or("x")),
        micros(&prices["output_mtok"])
    );
    assert_eq!(cohere["pricing_url"], json!(page));
    assert_eq!(
        rate(&cohere["card"], "input"),
        micros(&prices["input_mtok"])
    );
    assert_eq!(cohere["state"], "create", "{cohere}");
    let (_, theirs) = source("openrouter", "command-r");
    let openrouter = candidate(&a, "openrouter");
    assert_eq!(
        micros(openrouter["input"].as_str().unwrap_or("x")),
        micros(&theirs["input_mtok"])
    );
    assert_eq!(openrouter["state"], "create", "{openrouter}");

    let r = sync(
        &cp,
        &op,
        json!({
            "library_id": p["library"]["id"],
            "attested": true,
            "items": [choosing(model, &cohere)],
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let written = baseline_of(&cp, &op, model).await;
    assert_eq!(written["source"]["library_entry"], "cohere/command-r");
    assert_eq!(
        rate(&written["card"], "input"),
        micros(&prices["input_mtok"])
    );
    assert_eq!(
        binding(&cp, model).await.as_deref(),
        Some("cohere/command-r")
    );
    let rows = audits(&cp, "pricing.library.bind").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["model_id"], json!(model));
    assert_eq!(rows[0]["provider"], "cohere");
    assert_eq!(rows[0]["model"], "command-r");
    assert_eq!(audits(&cp, "pricing.library.sync").await.len(), 1);

    // From then on it matches the entry chosen, and needs no choice.
    let c = item(&preview(&cp, &op, None).await, model);
    assert_eq!(
        matched(&c),
        ("cohere/command-r".to_string(), json!("chosen")),
        "{c}"
    );
    assert_eq!(c["state"], "same", "{c}");
    assert_eq!(c["candidates"], json!([]), "{c}");
    assert_eq!(c["binding"]["provider"], "cohere", "{c}");
    assert_eq!(c["binding"]["model"], "command-r", "{c}");
    assert_eq!(c["binding"]["in_library"], json!(true), "{c}");
    // A baseline set by hand since: the chosen entry updates it, listed as
    // any match is, and the choice is not recorded again.
    manual(
        &cp,
        &op,
        model,
        &json!({"default": {"base": {"input": "9", "output": "9"}}}),
    )
    .await;
    let p = preview(&cp, &op, Some(model)).await;
    let u = item(&p, model);
    assert_eq!(u["state"], "update", "{u}");
    assert_eq!(u["matched_by"], "chosen", "{u}");
    let r = sync(
        &cp,
        &op,
        json!({"library_id": p["library"]["id"], "attested": true, "items": [listed(&u)]}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        rate(&baseline_of(&cp, &op, model).await["card"], "input"),
        micros(&prices["input_mtok"])
    );
    assert_eq!(audits(&cp, "pricing.library.bind").await.len(), 1);
    // The other candidate is no longer one: the model has one match.
    let r = sync(
        &cp,
        &op,
        json!({
            "library_id": p["library"]["id"],
            "attested": true,
            "items": [choosing(model, &openrouter)],
        }),
    )
    .await;
    assert_eq!(
        (r.status, paths(&r)),
        (409, vec!["/items/0".to_string()]),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_choice_that_is_not_the_models_candidate_or_its_card_is_refused() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    let fine = served(&cp, &op, "command-a", "cohere").await;
    let model = served(&cp, &op, "command-r", "relay-7c1").await;
    let p = preview(&cp, &op, None).await;
    let one = listed(&item(&p, fine));
    let a = item(&p, model);
    let cohere = candidate(&a, "cohere");
    let openrouter = candidate(&a, "openrouter");
    assert_ne!(cohere["card"], openrouter["card"]);
    let before = baselines(&cp).await;

    // An entry of the library that is none of the model's candidates, with
    // its own card.
    let other = json!({
        "model_id": model,
        "provider": "cohere",
        "model": "command-a",
        "card": one["card"],
    });
    // A candidate, with another candidate's card.
    let mut mixed = choosing(model, &openrouter);
    mixed["card"] = cohere["card"].clone();
    // A candidate, with a card of no candidate.
    let mut edited = choosing(model, &cohere);
    edited["card"] = json!({"default": {"base": {"input": "0.01", "output": "0.02"}}});
    for (n, wrong) in [other, mixed, edited].into_iter().enumerate() {
        let r = sync(
            &cp,
            &op,
            json!({"library_id": p["library"]["id"], "attested": true, "items": [one, wrong]}),
        )
        .await;
        assert_eq!(r.status, 409, "case {n}: {r:?}");
        assert_eq!(r.body["code"], "library_stale", "case {n}");
        assert_eq!(paths(&r), ["/items/1"], "case {n}");
    }
    assert_eq!(baselines(&cp).await, before);
    assert_eq!(binding(&cp, model).await, None);
    assert!(audits(&cp, "pricing.library.bind").await.is_empty());
    assert!(audits(&cp, "pricing.library.sync").await.is_empty());
}

#[tokio::test]
async fn a_choice_whose_entry_left_the_library_is_ignored_and_a_new_choice_replaces_it() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    let model = served(&cp, &op, "command-r", "relay-7c1").await;
    // Chosen from an earlier library, which held an entry this one does not.
    sqlx::query(
        "INSERT INTO pricing_library_bindings (model_id, provider, model, chosen_by) \
         VALUES ($1, 'cohere', 'command-r-retired', $2)",
    )
    .bind(model)
    .bind(op.id)
    .execute(cp.db().pool())
    .await
    .expect("record a choice");

    let p = preview(&cp, &op, None).await;
    let a = item(&p, model);
    // Matched by its name again, and the preview says why.
    assert_eq!(a["state"], "ambiguous", "{a}");
    assert_eq!(a["matched_by"], Value::Null, "{a}");
    assert_eq!(a["binding"]["provider"], "cohere", "{a}");
    assert_eq!(a["binding"]["model"], "command-r-retired", "{a}");
    assert_eq!(a["binding"]["in_library"], json!(false), "{a}");

    // A choice made now replaces it.
    let r = sync(
        &cp,
        &op,
        json!({
            "library_id": p["library"]["id"],
            "attested": true,
            "items": [choosing(model, &candidate(&a, "openrouter"))],
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        binding(&cp, model).await.as_deref(),
        Some("openrouter/command-r")
    );
    let c = item(&preview(&cp, &op, None).await, model);
    assert_eq!(c["matched_by"], "chosen", "{c}");
    assert_eq!(c["binding"]["in_library"], json!(true), "{c}");
}

#[tokio::test]
async fn forgetting_a_choice_makes_the_model_ambiguous_again() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    upload(&cp, &op, "v2-7de757d").await;
    let model = served(&cp, &op, "command-r", "relay-7c1").await;
    let p = preview(&cp, &op, None).await;
    let r = sync(
        &cp,
        &op,
        json!({
            "library_id": p["library"]["id"],
            "attested": true,
            "items": [choosing(model, &candidate(&item(&p, model), "cohere"))],
        }),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        item(&preview(&cp, &op, None).await, model)["matched_by"],
        "chosen"
    );

    let r = cp
        .send(
            "DELETE",
            &format!("{BINDINGS}/{model}"),
            Some(op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    assert_eq!(binding(&cp, model).await, None);
    let rows = audits(&cp, "pricing.library.unbind").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["model_id"], json!(model));
    assert_eq!(rows[0]["provider"], "cohere");
    assert_eq!(rows[0]["model"], "command-r");
    let a = item(&preview(&cp, &op, None).await, model);
    assert_eq!(a["state"], "ambiguous", "{a}");
    assert_eq!(a["binding"], Value::Null, "{a}");
    // The choice is gone, so no entry is the chosen one.
    assert_eq!(a["matched_by"], Value::Null, "{a}");
    // The baseline written from the choice stays: forgetting it writes no
    // price. Each candidate is judged against it.
    assert_eq!(candidate(&a, "cohere")["state"], "same", "{a}");
    assert_eq!(candidate(&a, "openrouter")["state"], "update", "{a}");

    // Nothing left to forget.
    let r = cp
        .send(
            "DELETE",
            &format!("{BINDINGS}/{model}"),
            Some(op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(audits(&cp, "pricing.library.unbind").await.len(), 1);
}
