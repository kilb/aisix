//! C/C/V 真实 CP、PG、kine、mTLS 与两个生产路由状态的跨面集成。
//! 只有共享上游 HTTP 账号为确定性替身；不称为真实提供商或两个网关进程 E2E。

#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use aisix_admin::{metrics_router, ConfigStore, InMemoryStore, ModelsStatusState};
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::config::PrometheusConfig;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ConfigStatus, ProxyConfig, SourceKind};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use native_gateway::{projected_snapshot, HttpRegistry};
use serde_json::{json, Value};
use support::members::MemberSession;
use support::{Bundle, OperatorSession, TestCp};
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

const BUILD: &str = "dev+sha-12ab345";
const MODEL: &str = "ccv-joint-model";
const SECOND_MODEL: &str = "ccv-joint-second-model";
const UPSTREAM_KEY: &str = "sk-ccv-local-component-account";
const BINARY: &[u8] = &[0, 255, 10, 13, 65, 0, 66];

fn uuid(body: &Value, pointer: &str) -> Uuid {
    Uuid::parse_str(
        body.pointer(pointer)
            .and_then(Value::as_str)
            .expect(pointer),
    )
    .expect("a resource UUID")
}

fn expected_endpoints() -> Value {
    json!([
        "chat_completions",
        "completions",
        "messages",
        "count_tokens",
        "responses",
        "embeddings",
        "models",
        "images",
        "audio",
        "videos",
        "rerank",
        "decisions",
        "realtime",
        "files",
        "batches",
        "fine_tuning",
        "moderations",
        "content_provenance_checks",
        "conversations",
        "containers",
        "vector_stores",
        "skills",
        "evals"
    ])
}

struct Caller {
    id: Uuid,
    bearer: String,
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    manager: MemberSession,
    upstream: Provider,
    tenant: Uuid,
    model: Uuid,
    second_model: Uuid,
    owner: Caller,
    sibling_key: Caller,
    other_member: Caller,
    full_member: Uuid,
    full: Caller,
    foreign: Caller,
    gateway_a: Bundle,
    gateway_b: Bundle,
}

async fn grant(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, models: &[Uuid]) {
    let path = format!("/admin/tenants/{tenant}");
    let old = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(old.status, 200, "{old:?}");
    let changed = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            old.body["version"].as_i64().expect("a tenant version"),
            json!({"allowed_model_ids":models}),
        )
        .await;
    assert_eq!(changed.status, 200, "{changed:?}");
}

async fn mint(
    cp: &TestCp,
    admin: &OperatorSession,
    tenant: Uuid,
    member: Uuid,
    label: &str,
) -> Caller {
    let group = support::world::group_id(cp, "shared").await;
    let created = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(admin.cookie()),
            Some(json!({"label":label,"group_id":group,"member_id":member})),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    let id = uuid(&created.body, "/key/id");
    let (doc, version): (Value, i32) = sqlx::query_as(
        "SELECT r.doc,k.doc_version FROM tenant_keys k JOIN gw_resources r ON r.id=k.id AND r.kind='api_keys' WHERE k.id=$1"
    ).bind(id).fetch_one(cp.db().pool()).await.expect("the real minted projection");
    assert_eq!(
        doc.get("allowed_endpoints"),
        Some(&expected_endpoints()),
        "the new supported fleet must mint the exact closed 23 families"
    );
    assert_eq!(version, 6);
    Caller {
        id,
        bearer: created.body["plaintext"]
            .as_str()
            .expect("the actual minted bearer")
            .to_owned(),
    }
}

async fn heartbeat(cp: &TestCp, bundle: &Bundle, instance: &str) {
    let response = cp
        .dp_client(bundle)
        .heartbeat(json!({
            "dp_id":bundle.dp_id,"instance_id":instance,"hostname":"ccv-joint-host",
            "uptime_seconds":1,"version":BUILD,"supported_guardrail_kinds":[],
            "applied_revision":1,"exporter_health":[],"partially_compatible_resources":[]
        }))
        .await;
    assert_eq!(response.status, 200, "{response:?}");
}

async fn fixture() -> Option<Fixture> {
    let endpoint = support::kine_endpoint()?;
    let cp = TestCp::start_with(|config| {
        config.kine_endpoint = endpoint;
        config.cp_version = BUILD.into();
    })
    .await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let upstream = Provider::start().await;
    let provider = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc":{
                "display_name":"ccv-shared-account","provider":"openai","adapter":"openai",
                "api_base":format!("{}/v1",upstream.base),"api_key":UPSTREAM_KEY,"groups":["shared"]
            }})),
        )
        .await;
    assert_eq!(provider.status, 201, "{provider:?}");
    let provider = uuid(&provider.body, "/id");
    let mut models = Vec::new();
    for name in [MODEL, SECOND_MODEL] {
        let created = cp.send("POST", "/admin/resources/models", Some(admin.cookie()), Some(json!({"doc":{
            "display_name":name,"provider":"openai","model_name":"provider-model","provider_key_id":provider
        }}))).await;
        assert_eq!(created.status, 201, "{created:?}");
        models.push(uuid(&created.body, "/id"));
    }
    let gateway_a = cp.issue_bundle(&admin, "ccv-real-gateway-a").await;
    let gateway_b = cp.issue_bundle(&admin, "ccv-real-gateway-b").await;
    assert_ne!(gateway_a.dp_id, gateway_b.dp_id);
    heartbeat(&cp, &gateway_a, "ccv-a").await;
    heartbeat(&cp, &gateway_b, "ccv-b").await;
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let foreign_tenant = support::world::seed_tenant(&cp, &admin).await;
    for tenant in [tenant, foreign_tenant] {
        let credited = cp
            .send(
                "POST",
                &format!("/admin/tenants/{tenant}/credits"),
                Some(admin.cookie()),
                Some(json!({"amount_cny":"1","reason":"跨面原生对象测试余额"})),
            )
            .await;
        assert_eq!(credited.status, 201, "{credited:?}");
        grant(&cp, &admin, tenant, &models).await;
    }
    let owner_member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let other_member_id = cp.seed_member(tenant, MemberRole::Member).await.id;
    let full_member = cp.seed_member(tenant, MemberRole::Admin).await.id;
    let foreign_member = cp.seed_member(foreign_tenant, MemberRole::Member).await.id;
    let manager = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let owner = mint(&cp, &admin, tenant, owner_member, "ccv-owner").await;
    let sibling_key = mint(&cp, &admin, tenant, owner_member, "ccv-same-member").await;
    let other_member = mint(&cp, &admin, tenant, other_member_id, "ccv-other-member").await;
    let full = mint(&cp, &admin, tenant, full_member, "ccv-full").await;
    let foreign = mint(&cp, &admin, foreign_tenant, foreign_member, "ccv-foreign").await;
    Some(Fixture {
        cp,
        admin,
        manager,
        upstream,
        tenant,
        model: models[0],
        second_model: models[1],
        owner,
        sibling_key,
        other_member,
        full_member,
        full,
        foreign,
        gateway_a,
        gateway_b,
    })
}

fn gateway(
    cp: &TestCp,
    bundle: &Bundle,
    snapshot: AisixSnapshot,
) -> (ProxyState, mpsc::Receiver<UsageEvent>) {
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .expect("the gateway configuration");
    let (tx, rx) = mpsc::channel(512);
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            &cp.state().pki.platform_id_key_hex(),
        ))
        .with_native_objects(Arc::new(HttpRegistry::new(cp, bundle)))
        .with_usage_sink(UsageSink::new(tx));
    (state, rx)
}

#[derive(Debug)]
struct Reply {
    status: StatusCode,
    body: Value,
    bytes: Vec<u8>,
}

async fn read(response: Response) -> Reply {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 << 20)
        .await
        .expect("the complete response body")
        .to_vec();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        body,
        bytes,
    }
}

async fn send(
    state: &ProxyState,
    method: &str,
    path: &str,
    bearer: &str,
    body: Option<Value>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {bearer}"));
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    read(
        build_router(state.clone())
            .oneshot(request.body(body).expect("a gateway request"))
            .await
            .expect("the production router answers"),
    )
    .await
}

async fn upload(
    state: &ProxyState,
    path: &str,
    bearer: &str,
    filename: &str,
    bytes: &[u8],
) -> Reply {
    let boundary = "ccv-joint-binary";
    let mut body = Vec::new();
    if path.starts_with("/v1/files") {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n").as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-aisix-model", MODEL)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .expect("the multipart request");
    read(
        build_router(state.clone())
            .oneshot(request)
            .await
            .expect("the upload handler answers"),
    )
    .await
}

async fn scrape(state: &ProxyState) -> String {
    let response = metrics_router(
        state.metrics.clone(),
        ConfigStatus::new(SourceKind::Etcd),
        &PrometheusConfig {
            enabled: true,
            path: "/metrics".into(),
            addr: "127.0.0.1:0".into(),
        },
        ModelsStatusState {
            store: InMemoryStore::new() as Arc<dyn ConfigStore>,
            runtime_status_tracker: None,
        },
    )
    .oneshot(
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .expect("a metrics request"),
    )
    .await
    .expect("the production metrics handler answers");
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(
        to_bytes(response.into_body(), 8 << 20)
            .await
            .expect("the metrics exposition")
            .to_vec(),
    )
    .expect("UTF-8 Prometheus text")
}

fn request_count(text: &str, endpoint: &str) -> f64 {
    let wanted = format!("endpoint=\"{endpoint}\"");
    text.lines()
        .filter(|line| {
            line.starts_with("aisix_proxy_requests_total{")
                && line
                    .split_once('{')
                    .and_then(|(_, tail)| tail.split_once('}'))
                    .is_some_and(|(labels, _)| labels.split(',').any(|label| label == wanted))
        })
        .map(|line| {
            line.rsplit_once(' ')
                .expect("a metric sample")
                .1
                .parse::<f64>()
                .expect("a request counter")
        })
        .sum()
}

async fn success(
    state: &ProxyState,
    method: &str,
    path: &str,
    label: &str,
    bearer: &str,
    body: Option<Value>,
    status: StatusCode,
) -> Reply {
    let before = request_count(&scrape(state).await, label);
    let response = send(state, method, path, bearer, body).await;
    assert_eq!(response.status, status, "{method} {path}: {response:?}");
    let after = request_count(&scrape(state).await, label);
    assert_eq!(
        after,
        before + 1.0,
        "every operation appears in GET /metrics: {method} {path} as {label}"
    );
    response
}

#[derive(Clone, Debug)]
struct Captured {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

#[derive(Default)]
struct ProviderData {
    calls: Vec<Captured>,
    counts: HashMap<String, usize>,
    objects: HashMap<String, Value>,
    children: HashMap<String, Vec<String>>,
    content: HashMap<String, Vec<u8>>,
}

struct Provider {
    base: String,
    data: Arc<Mutex<ProviderData>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Provider {
    async fn start() -> Self {
        let mut data = ProviderData::default();
        // 共享账号页先放120个没有本租户持久归属的对象，不能只过滤第一页。
        for index in 0..120 {
            for family in ["containers", "vector_stores"] {
                let id = format!(
                    "{}_account_{index:03}",
                    if family == "containers" { "cntr" } else { "vs" }
                );
                data.objects
                    .insert(id.clone(), root_json(family, &id, &json!({})));
            }
        }
        let data = Arc::new(Mutex::new(data));
        let app = Router::new()
            .fallback(provider_request)
            .with_state(data.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the local provider");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("the provider address")
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve the provider protocol substitute");
        });
        Self { base, data, task }
    }
    fn calls(&self) -> Vec<Captured> {
        self.data.lock().expect("provider state").calls.clone()
    }
}

fn root_json(family: &str, id: &str, body: &Value) -> Value {
    let mut result = match family {
        "conversations" => {
            json!({"id":id,"object":"conversation","created_at":1730000000i64,"metadata":{}})
        }
        "containers" => {
            json!({"id":id,"object":"container","created_at":1730000000i64,"name":"workspace","status":"running","memory_limit":"1g","last_active_at":1730000001i64,"expires_after":{"anchor":"last_active_at","minutes":20}})
        }
        "vector_stores" => {
            json!({"id":id,"object":"vector_store","created_at":1730000000i64,"name":"knowledge","status":"completed","usage_bytes":0,"metadata":{},"file_counts":{"in_progress":0,"completed":0,"failed":0,"cancelled":0,"total":0}})
        }
        _ => panic!("not a root collection"),
    };
    if let Some(fields) = body.as_object() {
        for (name, value) in fields {
            result[name] = value.clone();
        }
    }
    result["future_field"] = json!({"preserve":true});
    result
}

fn item_json(id: &str) -> Value {
    json!({"id":id,"type":"message","role":"user","status":"completed","content":[{"type":"input_text","text":"hello"}],"future_item":"preserved"})
}
fn vector_file(id: &str, store: &str) -> Value {
    json!({"id":id,"object":"vector_store.file","created_at":1730000001i64,"vector_store_id":store,"status":"completed","usage_bytes":7,"attributes":{"topic":"support"},"last_error":null,"chunking_strategy":{"type":"auto"},"future_file":true})
}
fn batch_json(id: &str, store: &str, status: &str) -> Value {
    json!({"id":id,"object":"vector_store.files_batch","created_at":1730000001i64,"vector_store_id":store,"status":status,"file_counts":{"in_progress":i32::from(status=="in_progress"),"completed":0,"failed":0,"cancelled":i32::from(status=="cancelled"),"total":1},"future_batch":true})
}

fn page(mut values: Vec<Value>, query: &BTreeMap<String, String>) -> Value {
    values.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    if query.get("order").is_some_and(|order| order == "desc") {
        values.reverse();
    }
    let start = query
        .get("after")
        .and_then(|after| {
            values
                .iter()
                .position(|value| value["id"] == after.as_str())
        })
        .map_or(0, |index| index + 1);
    let limit = query.get("limit").map_or(100, |value| {
        value.parse::<usize>().expect("the official page limit")
    });
    let end = (start + limit).min(values.len());
    let data = &values[start..end];
    json!({"object":"list","data":data,"first_id":data.first().map(|value|&value["id"]),"last_id":data.last().map(|value|&value["id"]),"has_more":end<values.len(),"future_page":"preserved"})
}

fn next_id(data: &mut ProviderData, family: &str) -> String {
    let count = data.counts.entry(family.into()).or_default();
    *count += 1;
    format!(
        "{}_ccv_{}",
        match family {
            "conversations" => "conv",
            "containers" => "cntr",
            "vector_stores" => "vs",
            "files" => "file",
            "items" => "msg",
            "container_files" => "cfile",
            "file_batches" => "vsfb",
            _ => panic!("unknown namespace"),
        },
        count
    )
}

fn uploaded(content_type: &str, body: &[u8]) -> Option<Vec<u8>> {
    let boundary = content_type
        .split(';')
        .find_map(|part| part.trim().strip_prefix("boundary="))?
        .trim_matches('"');
    let find = |bytes: &[u8], needle: &[u8]| {
        bytes
            .windows(needle.len())
            .position(|window| window == needle)
    };
    let marker = b"name=\"file\"";
    let header = find(body, marker)?;
    let start = header + find(body.get(header..)?, b"\r\n\r\n")? + 4;
    let end = start + find(body.get(start..)?, format!("\r\n--{boundary}").as_bytes())?;
    Some(body[start..end].to_vec())
}

async fn provider_request(
    State(shared): State<Arc<Mutex<ProviderData>>>,
    request: Request<Body>,
) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {UPSTREAM_KEY}").as_str());
    let bytes = to_bytes(request.into_body(), 1 << 20)
        .await
        .expect("read the provider request");
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let mut data = shared.lock().expect("provider state");
    data.calls.push(Captured {
        method: method.clone(),
        path: path.clone(),
        query: query.clone(),
        body: bytes.to_vec(),
    });
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let query: BTreeMap<String, String> =
        reqwest::Url::parse(&format!("http://provider.invalid/?{query}"))
            .expect("the provider query")
            .query_pairs()
            .into_owned()
            .collect();
    let segments: Vec<&str> = path.trim_start_matches("/v1/").split('/').collect();
    let family = segments[0];
    if family == "files" && segments.len() == 1 && method == "POST" {
        let Some(content) = uploaded(&content_type, &bytes) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let id = next_id(&mut data, "files");
        let object = json!({"id":id,"object":"file","bytes":content.len(),"created_at":1730000000i64,"filename":"input.txt","purpose":"assistants","status":"processed","future_file":true});
        data.content.insert(id.clone(), content);
        data.objects.insert(id, object.clone());
        return Json(object).into_response();
    }
    if !["conversations", "containers", "vector_stores"].contains(&family) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if segments.len() == 1 {
        if method == "POST" {
            let id = next_id(&mut data, family);
            let object = root_json(family, &id, &body);
            data.objects.insert(id, object.clone());
            return Json(object).into_response();
        }
        if method == "GET" && family != "conversations" {
            let object = if family == "containers" {
                "container"
            } else {
                "vector_store"
            };
            let values = data
                .objects
                .values()
                .filter(|value| value["object"] == object)
                .cloned()
                .collect();
            return Json(page(values, &query)).into_response();
        }
    }
    let Some(parent) = segments.get(1).copied() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(root) = data.objects.get(parent).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if segments.len() == 2 {
        return match method.as_str() {
            "GET" => Json(root).into_response(),
            "POST" => {
                let mut updated = root;
                for (key, value) in body.as_object().expect("the official update body") {
                    updated[key] = value.clone();
                }
                data.objects.insert(parent.into(), updated.clone());
                Json(updated).into_response()
            }
            "DELETE" => {
                data.objects.remove(parent);
                if family == "containers" {
                    StatusCode::NO_CONTENT.into_response()
                } else {
                    Json(json!({"id":parent,"object":format!("{}.deleted",if family=="conversations" {"conversation"} else {"vector_store"}),"deleted":true})).into_response()
                }
            }
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        };
    }
    let collection = format!("/v1/{family}/{parent}/{}", segments[2]);
    if family == "vector_stores" && segments[2] == "search" && method == "POST" {
        let rows=data.children.get(&format!("/v1/vector_stores/{parent}/files")).cloned().unwrap_or_default().into_iter().map(|id|json!({"file_id":id,"filename":"input.bin","score":0.93,"attributes":{"topic":"support"},"content":[{"type":"text","text":"tenant-owned result"}]})).collect::<Vec<_>>();
        return Json(json!({"object":"vector_store.search_results.page","search_query":["support"],"data":rows,"has_more":false,"next_page":null,"future_search":true})).into_response();
    }
    if segments.len() == 3 {
        if method == "GET" {
            let values = data
                .children
                .get(&collection)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|id| {
                    if family == "vector_stores" {
                        vector_file(&id, parent)
                    } else {
                        data.objects[&id].clone()
                    }
                })
                .collect();
            return Json(page(values, &query)).into_response();
        }
        if method == "POST" {
            let (id, object) = match (family, segments[2]) {
                ("conversations", "items") => {
                    let id = next_id(&mut data, "items");
                    let item = item_json(&id);
                    (id, item)
                }
                ("containers", "files") => {
                    let Some(content) = uploaded(&content_type, &bytes) else {
                        return StatusCode::BAD_REQUEST.into_response();
                    };
                    let id = next_id(&mut data, "container_files");
                    let object = json!({"id":id,"object":"container.file","container_id":parent,"bytes":content.len(),"created_at":1730000001i64,"path":"/mnt/data/input.bin","source":"user","future_file":true});
                    data.content.insert(id.clone(), content);
                    (id, object)
                }
                ("vector_stores", "files") => {
                    let id = body["file_id"]
                        .as_str()
                        .expect("the attached file")
                        .to_owned();
                    if !data.objects.contains_key(&id) {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let object = vector_file(&id, parent);
                    (id, object)
                }
                ("vector_stores", "file_batches") => {
                    let id = next_id(&mut data, "file_batches");
                    let object = batch_json(&id, parent, "in_progress");
                    let files = body["file_ids"]
                        .as_array()
                        .expect("the batch file IDs")
                        .iter()
                        .map(|id| id.as_str().expect("a file ID").to_owned())
                        .collect::<Vec<_>>();
                    data.children
                        .insert(format!("{collection}/{id}/files"), files.clone());
                    data.children
                        .entry(format!("/v1/vector_stores/{parent}/files"))
                        .or_default()
                        .extend(files);
                    (id, object)
                }
                _ => return StatusCode::NOT_FOUND.into_response(),
            };
            data.children
                .entry(collection)
                .or_default()
                .push(id.clone());
            if family != "vector_stores" || segments[2] == "file_batches" {
                data.objects.insert(id, object.clone());
            }
            return if family == "conversations" {
                Json(page(vec![object], &BTreeMap::new())).into_response()
            } else {
                Json(object).into_response()
            };
        }
    }
    let Some(child) = segments.get(3).copied() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !data
        .children
        .get(&collection)
        .is_some_and(|ids| ids.iter().any(|id| id == child))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if segments.len() == 5 {
        return match (family,segments[2],segments[4],method.as_str()) {
            ("containers","files","content","GET")=> ([ ("content-type","application/octet-stream") ],data.content[child].clone()).into_response(),
            ("vector_stores","files","content","GET")=>Json(json!({"object":"list","data":[{"type":"text","text":"tenant-owned result"}],"future_content":true})).into_response(),
            ("vector_stores","file_batches","cancel","POST")=>{let object=batch_json(child,parent,"cancelled");data.objects.insert(child.into(),object.clone());Json(object).into_response()},
            ("vector_stores","file_batches","files","GET")=>Json(page(data.children.get(&format!("{collection}/{child}/files")).cloned().unwrap_or_default().iter().map(|id|vector_file(id,parent)).collect(),&query)).into_response(),
            _=>StatusCode::NOT_FOUND.into_response(),
        };
    }
    let object = if family == "vector_stores" && segments[2] == "files" {
        vector_file(child, parent)
    } else {
        data.objects[child].clone()
    };
    match method.as_str() {
        "GET" => Json(object).into_response(),
        "POST" => {
            let mut changed = object;
            for (key, value) in body.as_object().expect("the official child update") {
                changed[key] = value.clone();
            }
            Json(changed).into_response()
        }
        "DELETE" => {
            data.children
                .get_mut(&collection)
                .expect("the collection")
                .retain(|id| id != child);
            if family == "conversations" {
                Json(root).into_response()
            } else if family == "containers" {
                StatusCode::NO_CONTENT.into_response()
            } else {
                Json(json!({"id":child,"object":"vector_store.file.deleted","deleted":true}))
                    .into_response()
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn management_events(receivers: &mut [&mut mpsc::Receiver<UsageEvent>]) -> Vec<Value> {
    let mut events = Vec::new();
    for receiver in receivers {
        while let Ok(event) = receiver.try_recv() {
            assert_eq!(
                event.inference_stage,
                Some(aisix_core::native_objects::InferenceStage::Management),
                "every observed object request is management"
            );
            assert_eq!(
                (
                    event.prompt_tokens,
                    event.completion_tokens,
                    event.reasoning_tokens
                ),
                (0, 0, 0)
            );
            assert_eq!(
                (
                    event.cached_prompt_tokens,
                    event.cache_read_tokens,
                    event.cache_creation_tokens,
                    event.total_tokens
                ),
                (0, 0, 0, 0)
            );
            events.push(serde_json::to_value(event).expect("serialize actual gateway telemetry"));
        }
    }
    events
}

async fn assert_management_intake(f: &Fixture, events: Vec<Value>, expected: usize) {
    assert_eq!(
        events.len(),
        expected,
        "every fully consumed management response emits exactly one row"
    );
    let delivered =
        support::world::deliver(&f.cp.dp_client(&f.gateway_a), Uuid::new_v4(), events).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(delivered.body["rejected"], 0, "{delivered:?}");
    let usage =
        f.cp.send(
            "GET",
            "/admin/usage/direct-models",
            Some(f.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(usage.status, 200, "{usage:?}");
    assert_eq!(
        usage.body["totals"].get("attempts"),
        Some(&json!(0)),
        "management adds no LLM attempt: {usage:?}"
    );
    assert_eq!(
        usage.body["totals"].get("succeeded_attempts"),
        Some(&json!(0)),
        "management adds no successful inference: {usage:?}"
    );
}

// 按官方33个操作逐项发流量；删掉任一路由、原始字段或metric接线都会变红。
#[tokio::test]
async fn all_thirty_three_ccv_operations_cross_real_authority_with_raw_wire_and_management_metrics()
{
    let Some(f) = fixture().await else {
        return;
    };
    let (a, mut usage_a) = gateway(&f.cp, &f.gateway_a, projected_snapshot(&f.cp).await);
    let (b, mut usage_b) = gateway(&f.cp, &f.gateway_b, projected_snapshot(&f.cp).await);
    for expected in ["file_ccv_1", "file_ccv_2"] {
        let reply = upload(
            &a,
            &format!("/v1/files?model={MODEL}"),
            &f.owner.bearer,
            "input.txt",
            b"tenant support policy\n",
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "source upload: {reply:?}");
        assert_eq!(reply.body["id"], expected);
    }
    let created = success(
        &a,
        "POST",
        &format!("/v1/conversations?model={MODEL}"),
        "/v1/conversations",
        &f.owner.bearer,
        Some(json!({"metadata":{"purpose":"support"},"future_request":{"keep":true}})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(created.body["id"], "conv_ccv_1");
    assert_eq!(created.body["future_field"], json!({"preserve":true}));
    assert_eq!(created.body["future_request"], json!({"keep":true}));
    let got = success(
        &b,
        "GET",
        "/v1/conversations/conv_ccv_1",
        "/v1/conversations/:id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(got.body, created.body);
    let changed = success(
        &a,
        "POST",
        "/v1/conversations/conv_ccv_1",
        "/v1/conversations/:id",
        &f.owner.bearer,
        Some(json!({"metadata":{"purpose":"updated"}})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(changed.body["metadata"], json!({"purpose":"updated"}));
    let appended = success(
        &a,
        "POST",
        "/v1/conversations/conv_ccv_1/items",
        "/v1/conversations/:id/items",
        &f.owner.bearer,
        Some(json!({"items":[{"role":"user","content":"hello"}]})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(appended.body["data"][0]["id"], "msg_ccv_1");
    let listed = success(
        &b,
        "GET",
        "/v1/conversations/conv_ccv_1/items?limit=1&order=asc",
        "/v1/conversations/:id/items",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed.body["data"], json!([item_json("msg_ccv_1")]));
    assert_eq!(listed.body["first_id"], "msg_ccv_1");
    assert_eq!(listed.body["last_id"], "msg_ccv_1");
    let item = success(
        &b,
        "GET",
        "/v1/conversations/conv_ccv_1/items/msg_ccv_1",
        "/v1/conversations/:id/items/:item_id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(item.body, item_json("msg_ccv_1"));
    let removed = success(
        &a,
        "DELETE",
        "/v1/conversations/conv_ccv_1/items/msg_ccv_1",
        "/v1/conversations/:id/items/:item_id",
        &f.owner.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(removed.body["id"], "conv_ccv_1");
    let deleted = success(
        &b,
        "DELETE",
        "/v1/conversations/conv_ccv_1",
        "/v1/conversations/:id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        deleted.body,
        json!({"id":"conv_ccv_1","object":"conversation.deleted","deleted":true})
    );

    let container = success(
        &a,
        "POST",
        &format!("/v1/containers?model={MODEL}"),
        "/v1/containers",
        &f.owner.bearer,
        Some(json!({"name":"workspace","metadata":{"source":"tenant"}})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(container.body["id"], "cntr_ccv_1");
    let retrieved = success(
        &b,
        "GET",
        "/v1/containers/cntr_ccv_1",
        "/v1/containers/:id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(retrieved.body, container.body);
    let listed = success(
        &b,
        "GET",
        &format!("/v1/containers?model={MODEL}&limit=1&order=asc"),
        "/v1/containers",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed.body["data"], json!([container.body]));
    assert_eq!(listed.body["first_id"], "cntr_ccv_1");
    assert_eq!(listed.body["last_id"], "cntr_ccv_1");
    assert_eq!(listed.body["has_more"], false);
    assert!(!listed.body.to_string().contains("cntr_account_"));
    let before = request_count(&scrape(&a).await, "/v1/containers/:id/files");
    let uploaded = upload(
        &a,
        "/v1/containers/cntr_ccv_1/files",
        &f.owner.bearer,
        "input.txt",
        b"tenant support policy\n",
    )
    .await;
    assert_eq!(
        uploaded.status,
        StatusCode::OK,
        "container upload: {uploaded:?}"
    );
    assert_eq!(uploaded.body["id"], "cfile_ccv_1");
    assert_eq!(
        request_count(&scrape(&a).await, "/v1/containers/:id/files"),
        before + 1.0
    );
    let listed = success(
        &b,
        "GET",
        "/v1/containers/cntr_ccv_1/files",
        "/v1/containers/:id/files",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed.body["data"], json!([uploaded.body]));
    let retrieved = success(
        &b,
        "GET",
        "/v1/containers/cntr_ccv_1/files/cfile_ccv_1",
        "/v1/containers/:id/files/:file_id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(retrieved.body["id"], "cfile_ccv_1");
    let content = success(
        &b,
        "GET",
        "/v1/containers/cntr_ccv_1/files/cfile_ccv_1/content",
        "/v1/containers/:id/files/:file_id/content",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        content.bytes, BINARY,
        "non-UTF8, zero and newline bytes round-trip unchanged"
    );
    let deleted = success(
        &a,
        "DELETE",
        "/v1/containers/cntr_ccv_1/files/cfile_ccv_1",
        "/v1/containers/:id/files/:file_id",
        &f.owner.bearer,
        None,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(deleted.bytes.is_empty());
    let deleted = success(
        &b,
        "DELETE",
        "/v1/containers/cntr_ccv_1",
        "/v1/containers/:id",
        &f.sibling_key.bearer,
        None,
        StatusCode::NO_CONTENT,
    )
    .await;
    assert!(deleted.bytes.is_empty());

    // 文件由模型A创建，向量库由当前同账号的模型B创建；模型UUID不同不能人为禁止消费。
    let store = success(
        &a,
        "POST",
        &format!("/v1/vector_stores?model={SECOND_MODEL}"),
        "/v1/vector_stores",
        &f.owner.bearer,
        Some(json!({"name":"knowledge","metadata":{"topic":"support"}})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(store.body["id"], "vs_ccv_1");
    let retrieved = success(
        &b,
        "GET",
        "/v1/vector_stores/vs_ccv_1",
        "/v1/vector_stores/:id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(retrieved.body, store.body);
    let updated = success(
        &a,
        "POST",
        "/v1/vector_stores/vs_ccv_1",
        "/v1/vector_stores/:id",
        &f.owner.bearer,
        Some(json!({"metadata":{"topic":"updated"}})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(updated.body["metadata"], json!({"topic":"updated"}));
    let listed = success(
        &b,
        "GET",
        &format!("/v1/vector_stores?model={SECOND_MODEL}&limit=1&order=asc"),
        "/v1/vector_stores",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(listed.body["data"], json!([updated.body]));
    assert_eq!(listed.body["first_id"], "vs_ccv_1");
    assert_eq!(listed.body["last_id"], "vs_ccv_1");
    assert_eq!(listed.body["has_more"], false);
    assert!(!listed.body.to_string().contains("vs_account_"));
    let attached = success(
        &a,
        "POST",
        "/v1/vector_stores/vs_ccv_1/files",
        "/v1/vector_stores/:id/files",
        &f.owner.bearer,
        Some(json!({"file_id":"file_ccv_1","attributes":{"topic":"support"}})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(attached.body, vector_file("file_ccv_1", "vs_ccv_1"));
    let batch = success(
        &a,
        "POST",
        "/v1/vector_stores/vs_ccv_1/file_batches",
        "/v1/vector_stores/:id/file_batches",
        &f.owner.bearer,
        Some(json!({"file_ids":["file_ccv_2"]})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(batch.body["id"], "vsfb_ccv_1");
    let listed = success(
        &b,
        "GET",
        "/v1/vector_stores/vs_ccv_1/files?order=asc",
        "/v1/vector_stores/:id/files",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        listed.body["data"],
        json!([
            vector_file("file_ccv_1", "vs_ccv_1"),
            vector_file("file_ccv_2", "vs_ccv_1")
        ])
    );
    let retrieved = success(
        &b,
        "GET",
        "/v1/vector_stores/vs_ccv_1/files/file_ccv_1",
        "/v1/vector_stores/:id/files/:file_id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(retrieved.body, attached.body);
    let changed = success(
        &a,
        "POST",
        "/v1/vector_stores/vs_ccv_1/files/file_ccv_1",
        "/v1/vector_stores/:id/files/:file_id",
        &f.owner.bearer,
        Some(json!({"attributes":{"topic":"updated"}})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(changed.body["attributes"], json!({"topic":"updated"}));
    let content = success(
        &b,
        "GET",
        "/v1/vector_stores/vs_ccv_1/files/file_ccv_1/content",
        "/v1/vector_stores/:id/files/:file_id/content",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        content.body,
        json!({"object":"list","data":[{"type":"text","text":"tenant-owned result"}],"future_content":true})
    );
    let search = success(
        &b,
        "POST",
        "/v1/vector_stores/vs_ccv_1/search",
        "/v1/vector_stores/:id/search",
        &f.sibling_key.bearer,
        Some(json!({"query":"support","max_num_results":4,"rewrite_query":true})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        search.body["data"]
            .as_array()
            .expect("a search result page")
            .len(),
        2
    );
    assert_eq!(search.body["data"][0]["file_id"], "file_ccv_1");
    assert_eq!(search.body["data"][1]["file_id"], "file_ccv_2");
    assert_eq!(search.body["data"][0]["score"], 0.93);
    assert_eq!(search.body["future_search"], true);
    let got = success(
        &b,
        "GET",
        "/v1/vector_stores/vs_ccv_1/file_batches/vsfb_ccv_1",
        "/v1/vector_stores/:id/file_batches/:batch_id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(got.body, batch.body);
    let listed = success(
        &b,
        "GET",
        "/v1/vector_stores/vs_ccv_1/file_batches/vsfb_ccv_1/files",
        "/v1/vector_stores/:id/file_batches/:batch_id/files",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        listed.body["data"],
        json!([vector_file("file_ccv_2", "vs_ccv_1")])
    );
    let cancelled = success(
        &a,
        "POST",
        "/v1/vector_stores/vs_ccv_1/file_batches/vsfb_ccv_1/cancel",
        "/v1/vector_stores/:id/file_batches/:batch_id/cancel",
        &f.owner.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(cancelled.body["status"], "cancelled");
    let detached = success(
        &a,
        "DELETE",
        "/v1/vector_stores/vs_ccv_1/files/file_ccv_1",
        "/v1/vector_stores/:id/files/:file_id",
        &f.owner.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        detached.body,
        json!({"id":"file_ccv_1","object":"vector_store.file.deleted","deleted":true})
    );
    let deleted = success(
        &b,
        "DELETE",
        "/v1/vector_stores/vs_ccv_1",
        "/v1/vector_stores/:id",
        &f.sibling_key.bearer,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        deleted.body,
        json!({"id":"vs_ccv_1","object":"vector_store.deleted","deleted":true})
    );
    let calls = f.upstream.calls();
    for collection in ["/v1/containers", "/v1/vector_stores"] {
        assert!(
            calls
                .iter()
                .filter(|call| call.method == "GET" && call.path == collection)
                .count()
                >= 2,
            "owned list crosses the120 foreign account rows: {collection}"
        );
    }
    let upstream_create = calls
        .iter()
        .find(|call| call.method == "POST" && call.path == "/v1/conversations")
        .expect("the conversation create");
    let forwarded: Value =
        serde_json::from_slice(&upstream_create.body).expect("the forwarded official JSON");
    assert_eq!(forwarded["future_request"], json!({"keep":true}));
    assert!(!forwarded
        .as_object()
        .expect("a request object")
        .contains_key("model"));
    assert!(
        !upstream_create.query.contains("model="),
        "the selector is gateway-only"
    );
    let events = management_events(&mut [&mut usage_a, &mut usage_b]).await;
    assert_management_intake(&f, events, 35).await;
}

async fn create_owned_graph(f: &Fixture, state: &ProxyState) {
    let file = upload(
        state,
        &format!("/v1/files?model={MODEL}"),
        &f.owner.bearer,
        "input.bin",
        BINARY,
    )
    .await;
    assert_eq!(file.status, StatusCode::OK, "{file:?}");
    assert_eq!(file.body["id"], "file_ccv_1");
    for (family, id, body) in [
        (
            "conversations",
            "conv_ccv_1",
            json!({"metadata":{"owner":"member-a"}}),
        ),
        ("containers", "cntr_ccv_1", json!({"name":"workspace"})),
        ("vector_stores", "vs_ccv_1", json!({"name":"knowledge"})),
    ] {
        let reply = send(
            state,
            "POST",
            &format!("/v1/{family}?model={MODEL}"),
            &f.owner.bearer,
            Some(body),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{family}: {reply:?}");
        assert_eq!(reply.body["id"], id);
    }
    let appended = send(
        state,
        "POST",
        "/v1/conversations/conv_ccv_1/items",
        &f.owner.bearer,
        Some(json!({"items":[{"role":"user","content":"hello"}]})),
    )
    .await;
    assert_eq!(appended.status, StatusCode::OK, "{appended:?}");
    assert_eq!(appended.body["data"][0]["id"], "msg_ccv_1");
    let cfile = upload(
        state,
        "/v1/containers/cntr_ccv_1/files",
        &f.owner.bearer,
        "input.bin",
        BINARY,
    )
    .await;
    assert_eq!(cfile.status, StatusCode::OK, "{cfile:?}");
    assert_eq!(cfile.body["id"], "cfile_ccv_1");
    let attached = send(
        state,
        "POST",
        "/v1/vector_stores/vs_ccv_1/files",
        &f.owner.bearer,
        Some(json!({"file_id":"file_ccv_1"})),
    )
    .await;
    assert_eq!(attached.status, StatusCode::OK, "{attached:?}");
    assert_eq!(attached.body["id"], "file_ccv_1");
}

const OWNED_READS: [&str; 6] = [
    "/v1/conversations/conv_ccv_1",
    "/v1/conversations/conv_ccv_1/items/msg_ccv_1",
    "/v1/containers/cntr_ccv_1",
    "/v1/containers/cntr_ccv_1/files/cfile_ccv_1/content",
    "/v1/vector_stores/vs_ccv_1",
    "/v1/vector_stores/vs_ccv_1/files/file_ccv_1/content",
];

// 去掉当前权限、member边界或持久registry读取，分别在拒绝计数和重建后的读取处失败。
#[tokio::test]
async fn ccv_member_isolation_current_grants_and_durable_graph_survive_cp_and_gateway_rebuild() {
    let Some(f) = fixture().await else {
        return;
    };
    let (a, mut usage_a) = gateway(&f.cp, &f.gateway_a, projected_snapshot(&f.cp).await);
    let (b, mut usage_b) = gateway(&f.cp, &f.gateway_b, projected_snapshot(&f.cp).await);
    create_owned_graph(&f, &a).await;
    for raw in [
        "conv_ccv_1",
        "msg_ccv_1",
        "cntr_ccv_1",
        "cfile_ccv_1",
        "vs_ccv_1",
        "file_ccv_1",
    ] {
        let owner: (Uuid, Uuid) =
            sqlx::query_as("SELECT tenant_id,api_key_id FROM native_objects WHERE raw_id=$1")
                .bind(raw)
                .fetch_one(f.cp.db().pool())
                .await
                .expect("every returned raw ID already has real persisted ownership");
        assert_eq!(owner, (f.tenant, f.owner.id), "{raw}");
    }
    for caller in [&f.sibling_key, &f.full] {
        for path in OWNED_READS {
            let reply = send(&b, "GET", path, &caller.bearer, None).await;
            assert_eq!(
                reply.status,
                StatusCode::OK,
                "authorized reader {path}: {reply:?}"
            );
            if path.contains("containers") && path.ends_with("/content") {
                assert_eq!(reply.bytes, BINARY);
            }
        }
    }
    let forbidden = [
        ("GET", "/v1/conversations/conv_ccv_1", None),
        (
            "POST",
            "/v1/conversations/conv_ccv_1",
            Some(json!({"metadata":{}})),
        ),
        ("DELETE", "/v1/conversations/conv_ccv_1", None),
        ("GET", "/v1/conversations/conv_ccv_1/items", None),
        ("GET", "/v1/conversations/conv_ccv_1/items/msg_ccv_1", None),
        (
            "DELETE",
            "/v1/conversations/conv_ccv_1/items/msg_ccv_1",
            None,
        ),
        ("GET", "/v1/containers/cntr_ccv_1", None),
        ("DELETE", "/v1/containers/cntr_ccv_1", None),
        ("GET", "/v1/containers/cntr_ccv_1/files", None),
        (
            "GET",
            "/v1/containers/cntr_ccv_1/files/cfile_ccv_1/content",
            None,
        ),
        (
            "DELETE",
            "/v1/containers/cntr_ccv_1/files/cfile_ccv_1",
            None,
        ),
        ("GET", "/v1/vector_stores/vs_ccv_1", None),
        (
            "POST",
            "/v1/vector_stores/vs_ccv_1",
            Some(json!({"metadata":{}})),
        ),
        ("DELETE", "/v1/vector_stores/vs_ccv_1", None),
        ("GET", "/v1/vector_stores/vs_ccv_1/files", None),
        (
            "GET",
            "/v1/vector_stores/vs_ccv_1/files/file_ccv_1/content",
            None,
        ),
        (
            "DELETE",
            "/v1/vector_stores/vs_ccv_1/files/file_ccv_1",
            None,
        ),
        (
            "POST",
            "/v1/vector_stores/vs_ccv_1/search",
            Some(json!({"query":"support"})),
        ),
    ];
    for caller in [&f.other_member, &f.foreign] {
        let before = f.upstream.calls().len();
        for (method, path, body) in &forbidden {
            let reply = send(&b, method, path, &caller.bearer, body.clone()).await;
            assert_eq!(
                reply.status,
                StatusCode::NOT_FOUND,
                "foreign {method} {path}: {reply:?}"
            );
        }
        assert_eq!(
            f.upstream.calls().len(),
            before,
            "another member or tenant never probes the shared account"
        );
    }
    let demoted =
        f.cp.send(
            "PATCH",
            &format!("/api/members/{}", f.full_member),
            Some(f.manager.cookie()),
            Some(json!({"role":"member"})),
        )
        .await;
    assert_eq!(demoted.status, 200, "{demoted:?}");
    let before = f.upstream.calls().len();
    for path in OWNED_READS {
        let denied = send(&b, "GET", path, &f.full.bearer, None).await;
        assert_eq!(
            denied.status,
            StatusCode::NOT_FOUND,
            "demoted Full reader {path}: {denied:?}"
        );
    }
    assert_eq!(f.upstream.calls().len(), before);
    let restarted = f.cp.restart_stamped(BUILD).await;
    let (recovered, mut recovered_usage) =
        gateway(&restarted, &f.gateway_b, projected_snapshot(&f.cp).await);
    for path in OWNED_READS {
        let reply = send(&recovered, "GET", path, &f.owner.bearer, None).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "durable owner after independent CP and gateway reconstruction {path}: {reply:?}"
        );
    }
    let page = send(
        &recovered,
        "GET",
        "/v1/conversations/conv_ccv_1/items",
        &f.owner.bearer,
        None,
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{page:?}");
    assert_eq!(page.body["data"][0]["id"], "msg_ccv_1");
    grant(&restarted, &f.admin, f.tenant, &[f.second_model]).await;
    let before = f.upstream.calls().len();
    for path in OWNED_READS {
        let denied = send(&recovered, "GET", path, &f.owner.bearer, None).await;
        assert_eq!(
            denied.status,
            StatusCode::NOT_FOUND,
            "withdrawn original model {path}: {denied:?}"
        );
    }
    assert_eq!(
        f.upstream.calls().len(),
        before,
        "current CP authorization defeats a stale gateway snapshot"
    );
    assert_ne!(f.model, f.second_model);
    let (fresh, mut fresh_usage) =
        gateway(&restarted, &f.gateway_a, projected_snapshot(&f.cp).await);
    let allowed = send(
        &fresh,
        "POST",
        &format!("/v1/conversations?model={SECOND_MODEL}"),
        &f.owner.bearer,
        Some(json!({"metadata":{"new":"allowed"}})),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "the same key retains its independently authorized model: {allowed:?}"
    );
    assert_eq!(allowed.body["id"], "conv_ccv_2");
    let events = management_events(&mut [
        &mut usage_a,
        &mut usage_b,
        &mut recovered_usage,
        &mut fresh_usage,
    ])
    .await;
    assert!(!events.is_empty());
}
