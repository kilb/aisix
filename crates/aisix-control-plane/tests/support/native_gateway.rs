//! 联合组件测试的真实 mTLS 控制面适配器和本地供应商协议替身。

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use aisix_core::native_metering::{ReconciliationRequest, TypedCompleteRequest};
use aisix_core::native_objects::*;
use aisix_core::AisixSnapshot;
use aisix_etcd::{build_snapshot, PrefixSet, RawEntry};
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use etcd_client::{Client, GetOptions};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};

use crate::support::{Bundle, DpClient, TestCp};

pub const INPUT_FILE: &str = "file_joint_input_v7";
pub const STAGED_FILE: &str = "file_joint_staged_v7";
pub const BATCH: &str = "batch_joint_owned_v7";
pub const OUTPUT_FILE: &str = "file_joint_output_v7";
pub const PROVIDER_SECRET: &str = "sk-joint-component-upstream-v7";
pub const RESPONSE: &str = "resp_joint_owned_v7";
pub const CONTINUED_RESPONSE: &str = "resp_joint_continued_v7";
pub const OUTPUT_MESSAGE: &str = "msg_joint_output_v7";
pub const FUNCTION_ITEM: &str = "fc_joint_classify_v7";
pub const FUNCTION_CALL: &str = "call_joint_classify_v7";
pub const INPUT_FIRST: &str = "msg_joint_input_first_v7";
pub const INPUT_LAST: &str = "msg_joint_input_last_v7";

// 只把 trait 调用送往真实控制面，不在此保存归属、租约或结算状态。
// 传输复用 TestCp 的 mTLS 客户端；不宣称覆盖服务端二进制的客户端实现。
pub struct HttpRegistry {
    dp: DpClient,
}

impl HttpRegistry {
    pub fn new(cp: &TestCp, bundle: &Bundle) -> Self {
        Self {
            dp: cp.dp_client(bundle),
        }
    }

    async fn post<T: Serialize + Sync>(
        &self,
        operation: &str,
        request: &T,
    ) -> Result<NativeReply, NativeRegistryError> {
        let body = serde_json::to_value(request).map_err(|_| NativeRegistryError::Unavailable)?;
        let response = self
            .dp
            .try_send(
                "POST",
                &format!("/dp/v1/native-objects/{operation}"),
                Some(body),
            )
            .await
            .map_err(|_| NativeRegistryError::Unavailable)?;
        if std::env::var_os("AISIX_NATIVE_AUTHORITY_DIAGNOSTICS").is_some() {
            // 只输出固定协议槽和计数，避免把对象标识、请求正文或凭据写进诊断。
            eprintln!(
                "native authority diagnostic: operation={operation} status={} code={} state={} origin_surface={}",
                response.status,
                response.body["code"].as_str().unwrap_or("absent"),
                response.body["state"].as_str().unwrap_or("absent"),
                response.body.pointer("/origin/surface").and_then(Value::as_str).unwrap_or("absent"),
            );
        }
        match response.status {
            200 => {
                serde_json::from_value(response.body).map_err(|_| NativeRegistryError::Unavailable)
            }
            404 if response.body["code"] == "not_found" => Err(NativeRegistryError::NotFound),
            422 if response.body["code"] == "validation_failed" => {
                Err(NativeRegistryError::InvalidRequest)
            }
            409 if matches!(
                response.body["code"].as_str(),
                Some("native_object_conflict" | "native_object_ambiguous")
            ) =>
            {
                Err(NativeRegistryError::Conflict)
            }
            _ => {
                eprintln!(
                    "native authority {operation}: HTTP {}, code {}",
                    response.status, response.body["code"]
                );
                Err(NativeRegistryError::Unavailable)
            }
        }
    }
}

macro_rules! authority_operation {
    ($method:ident, $request:ty) => {
        authority_operation!($method, $request, stringify!($method));
    };
    ($method:ident, $request:ty, $operation:expr) => {
        fn $method(
            &self,
            request: $request,
        ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async move { self.post($operation, &request).await })
        }
    };
}

impl NativeObjectRegistry for HttpRegistry {
    authority_operation!(begin, BeginRequest);
    authority_operation!(bind, BindRequest);
    authority_operation!(lookup, LookupRequest);
    authority_operation!(owned, OwnedRequest);
    authority_operation!(tombstone, TombstoneRequest);
    authority_operation!(claim, ClaimRequest);
    authority_operation!(renew, LeaseRequest);
    authority_operation!(reschedule, LeaseRequest);
    authority_operation!(complete, CompleteRequest);
    authority_operation!(begin_related, BeginRelatedRequest, "related/begin");
    authority_operation!(bind_related, BindRelatedRequest, "related/bind");
    authority_operation!(
        reserve_relations,
        ReserveRelationsRequest,
        "relations/reserve"
    );
    authority_operation!(commit_relations, CommitRelationsRequest, "relations/commit");
    authority_operation!(lookup_relations, LookupRelationsRequest, "relations/lookup");
    authority_operation!(
        resolve_reservation,
        ResolveReservationRequest,
        "relations/resolve"
    );
    authority_operation!(
        bind_observed_artifacts,
        BindObservedArtifactsRequest,
        "observed-artifacts"
    );
    authority_operation!(reserve_delete, ObjectDeletionRequest, "delete/reserve");
    authority_operation!(commit_delete, ObjectDeletionRequest, "delete/commit");
    authority_operation!(complete_typed, TypedCompleteRequest, "complete-typed");
    authority_operation!(mark_reconciliation, ReconciliationRequest, "reconciliation");
}

// 真正投影到 kine 的文档经网关的生产加载器进入快照，避免手造权限模型。
pub async fn projected_snapshot(cp: &TestCp) -> AisixSnapshot {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let pending: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM gw_resources WHERE projected_version IS DISTINCT FROM version",
            )
            .fetch_one(cp.db().pool())
            .await
            .expect("read outstanding real projections");
            if pending == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the real control-plane projector reaches kine");
    let endpoint = crate::support::kine_endpoint().expect("kine is configured");
    let mut kine = Client::connect([endpoint], None)
        .await
        .expect("connect to the real test kine");
    let prefix = cp.env().env_prefix();
    let response = kine
        .get(prefix.clone(), Some(GetOptions::new().with_prefix()))
        .await
        .expect("read the environment's real projected documents");
    let entries: Vec<_> = response
        .kvs()
        .iter()
        .map(|kv| RawEntry {
            key: String::from_utf8(kv.key().to_vec()).expect("a UTF-8 resource key"),
            value: kv.value().to_vec(),
            revision: kv.mod_revision(),
        })
        .collect();
    let (snapshot, stats) = build_snapshot(&PrefixSet::single(prefix), &entries);
    assert!(
        !entries.is_empty(),
        "the gateway must load actual configuration"
    );
    assert_eq!(stats.accepted, entries.len(), "{stats:?}");
    assert!(stats.rejections.is_empty(), "{stats:?}");
    snapshot
}

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
}

#[derive(Default)]
struct ProviderState {
    terminal: AtomicBool,
    requests: Mutex<Vec<ProviderRequest>>,
    files: Mutex<std::collections::HashMap<String, StoredFile>>,
    batch_input: Mutex<Option<String>>,
}

#[derive(Clone)]
struct StoredFile {
    purpose: String,
    filename: String,
    bytes: Vec<u8>,
}

// 此服务器只替代供应商协议，控制面、持久化、鉴权、worker 和计费均为真实实现。
pub struct ProviderSubstitute {
    pub base: String,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

impl ProviderSubstitute {
    pub async fn start() -> Self {
        let state = Arc::new(ProviderState::default());
        let app = Router::new()
            .fallback(provider_request)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the local provider substitute");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("the bound address")
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve the local provider substitute");
        });
        Self { base, state, task }
    }

    pub fn requests(&self) -> Vec<ProviderRequest> {
        self.state.requests.lock().expect("request log").clone()
    }

    pub fn complete(&self) {
        self.state.terminal.store(true, Ordering::SeqCst);
    }

    pub fn file_bytes(&self, id: &str) -> Vec<u8> {
        self.state
            .files
            .lock()
            .expect("stored provider files")
            .get(id)
            .expect("the uploaded provider file")
            .bytes
            .clone()
    }
}

impl Drop for ProviderSubstitute {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn provider_request(
    State(state): State<Arc<ProviderState>>,
    request: Request<Body>,
) -> Response {
    let method = request.method().to_string();
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
        .and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {PROVIDER_SECRET}").as_str());
    let body = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .expect("read the provider request");
    state
        .requests
        .lock()
        .expect("request log")
        .push(ProviderRequest {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            body: body.to_vec(),
        });
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match (method.as_str(), path.as_str()) {
        ("POST", "/v1/responses") => {
            let request: Value = serde_json::from_slice(&body).expect("the Responses request JSON");
            Json(response_json(request["previous_response_id"] == RESPONSE)).into_response()
        }
        ("GET", "/v1/responses/resp_joint_owned_v7") => Json(response_json(false)).into_response(),
        ("GET", "/v1/responses/resp_joint_owned_v7/input_items") => response_input_page(&query),
        ("GET", "/v1/files") => provider_file_page(&query),
        ("POST", "/v1/files") => {
            let Some(file) = uploaded_file(&content_type, &body) else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let mut files = state.files.lock().expect("stored provider files");
            let id = match files.len() {
                0 => INPUT_FILE.to_owned(),
                1 => STAGED_FILE.to_owned(),
                count => format!("file_joint_extra_{count}_v7"),
            };
            let metadata = file_metadata(&id, &file);
            files.insert(id, file);
            Json(metadata).into_response()
        }
        ("POST", "/v1/batches") => {
            let request: Value = serde_json::from_slice(&body).expect("the batch create JSON");
            let Some(input) = request["input_file_id"].as_str() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            if !state
                .files
                .lock()
                .expect("stored provider files")
                .contains_key(input)
            {
                return StatusCode::NOT_FOUND.into_response();
            }
            *state.batch_input.lock().expect("the accepted batch input") = Some(input.to_owned());
            Json(batch_json(&state)).into_response()
        }
        ("GET", "/v1/batches/batch_joint_owned_v7") => Json(batch_json(&state)).into_response(),
        ("GET", "/v1/files/file_joint_output_v7/content") => (
            [("content-type", "application/jsonl")],
            format!("{}\n", result_line()),
        )
            .into_response(),
        ("GET", "/v1/files/file_joint_output_v7") => Json(json!({
            "id":OUTPUT_FILE,"object":"file","purpose":"batch_output","filename":"result.jsonl",
            "created_at":1794614500,"bytes":300
        }))
        .into_response(),
        ("GET", path) if path.starts_with("/v1/files/") => {
            let raw = path
                .strip_prefix("/v1/files/")
                .expect("the file path prefix");
            let (id, content) = raw
                .strip_suffix("/content")
                .map_or((raw, false), |id| (id, true));
            let files = state.files.lock().expect("stored provider files");
            let Some(file) = files.get(id) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            if content {
                (
                    [("content-type", "application/octet-stream")],
                    file.bytes.clone(),
                )
                    .into_response()
            } else {
                Json(file_metadata(id, file)).into_response()
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn batch_json(state: &ProviderState) -> Value {
    let mut batch = json!({"id":BATCH,"object":"batch","status":"in_progress",
        "input_file_id":state.batch_input.lock().expect("the accepted batch input").clone(),
        "endpoint":"/v1/chat/completions","created_at":1794614400});
    if state.terminal.load(Ordering::SeqCst) {
        batch["status"] = json!("completed");
        batch["completed_at"] = json!(1794614500);
        batch["output_file_id"] = json!(OUTPUT_FILE);
    }
    batch
}

fn file_metadata(id: &str, file: &StoredFile) -> Value {
    json!({"id":id,"object":"file","purpose":file.purpose,"filename":file.filename,
        "created_at":1794614400,"bytes":file.bytes.len()})
}

// 仅解析 MIME 分隔线和字段头；文件内容按字节保存，不能隐式改写或修剪换行。
fn uploaded_file(content_type: &str, body: &[u8]) -> Option<StoredFile> {
    let boundary = content_type
        .split(';')
        .find_map(|part| part.trim().strip_prefix("boundary="))?
        .trim_matches('"');
    let delimiter = format!("--{boundary}").into_bytes();
    let next_delimiter = format!("\r\n--{boundary}").into_bytes();
    let mut offset = find_bytes(body, &delimiter)? + delimiter.len();
    let mut purpose = None;
    let mut stored = None;
    while !body.get(offset..)?.starts_with(b"--") {
        let start = offset + 2;
        let header_end = start + find_bytes(body.get(start..)?, b"\r\n\r\n")?;
        let headers = std::str::from_utf8(body.get(start..header_end)?).ok()?;
        let content_start = header_end + 4;
        let content_end = content_start + find_bytes(body.get(content_start..)?, &next_delimiter)?;
        let content = body.get(content_start..content_end)?;
        if headers.contains("name=\"purpose\"") {
            purpose = Some(std::str::from_utf8(content).ok()?.to_owned());
        }
        if headers.contains("name=\"file\"") {
            let filename = headers
                .split_once("filename=\"")?
                .1
                .split_once('"')?
                .0
                .to_owned();
            stored = Some((filename, content.to_vec()));
        }
        offset = content_end + 2 + delimiter.len();
    }
    let (filename, bytes) = stored?;
    Some(StoredFile {
        purpose: purpose?,
        filename,
        bytes,
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn response_json(continued: bool) -> Value {
    let output = if continued {
        json!([{"id":"msg_joint_continued_v7","type":"message","status":"completed","role":"assistant",
            "content":[{"type":"output_text","text":"classified","annotations":[]}]}])
    } else {
        json!([
            {"id":OUTPUT_MESSAGE,"type":"message","status":"completed","role":"assistant",
                "content":[{"type":"output_text","text":"I will classify the text.","annotations":[]}]},
            {"id":FUNCTION_ITEM,"type":"function_call","status":"completed",
                "call_id":FUNCTION_CALL,"name":"classify","arguments":"{}"}
        ])
    };
    json!({"id":if continued {CONTINUED_RESPONSE} else {RESPONSE},"object":"response",
        "created_at":1794614400,"status":"completed","model":"gpt-4o-mini",
        "background":false,"store":true,"error":null,"incomplete_details":null,
        "output":output,"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}})
}

fn response_input_page(query: &str) -> Response {
    let url = reqwest::Url::parse(&format!("http://provider.invalid/?{query}"))
        .expect("the response input page query");
    let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let items = [
        json!({"id":INPUT_FIRST,"type":"message","status":"completed","role":"user",
            "content":[{"type":"input_text","text":"classify this text"}]}),
        json!({"id":INPUT_LAST,"type":"message","status":"completed","role":"user",
            "content":[{"type":"input_text","text":"return the classification"}]}),
    ];
    let start: usize = match pairs.get("after") {
        None => 0,
        Some(after) if after == INPUT_FIRST => 1,
        Some(after) if after == INPUT_LAST => 2,
        Some(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let limit = pairs
        .get("limit")
        .map(|value| value.parse::<usize>().expect("an input page limit"))
        .unwrap_or(20);
    let end = start.saturating_add(limit).min(items.len());
    let data = &items[start..end];
    Json(
        json!({"object":"list","data":data,"has_more":end<items.len(),
        "first_id":data.first().map(|item| &item["id"]),
        "last_id":data.last().map(|item| &item["id"])}),
    )
    .into_response()
}

fn provider_file_page(query: &str) -> Response {
    let url = reqwest::Url::parse(&format!("http://provider.invalid/?{query}"))
        .expect("the provider page query");
    let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let limit = pairs
        .get("limit")
        .map(|value| {
            value
                .parse::<usize>()
                .expect("a numeric provider page limit")
        })
        .unwrap_or(10_000);
    let mut files: Vec<_> = (0..100)
        .map(|index| {
            json!({
                "id":format!("file_joint_foreign_{index:03}"),"object":"file","purpose":"batch",
                "filename":"other-tenant.jsonl","created_at":1794614400,"bytes":200
            })
        })
        .collect();
    files.push(json!({"id":INPUT_FILE,"object":"file","purpose":"batch",
        "filename":"joint.jsonl","created_at":1794614400,"bytes":200}));
    let start = match pairs.get("after") {
        Some(after) => match files
            .iter()
            .position(|file| file["id"].as_str() == Some(after.as_str()))
        {
            Some(index) => index + 1,
            None => return StatusCode::BAD_REQUEST.into_response(),
        },
        None => 0,
    };
    let end = start.saturating_add(limit).min(files.len());
    let data = &files[start..end];
    Json(
        json!({"object":"list","data":data,"has_more":end<files.len(),
        "first_id":data.first().map(|file| &file["id"]),
        "last_id":data.last().map(|file| &file["id"])}),
    )
    .into_response()
}

pub fn result_line() -> Value {
    json!({"id":"batch_req_joint_v7","custom_id":"entry-a","response":{
        "status_code":200,"request_id":"upstream-result-joint-v7","body":{
            "id":"chatcmpl_joint_v7","object":"chat.completion","model":"gpt-4o-mini",
            "choices":[{"index":0,"message":{"role":"assistant","content":"classified"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110,
                "prompt_tokens_details":{"cached_tokens":20}}
        }
    },"error":null})
}
