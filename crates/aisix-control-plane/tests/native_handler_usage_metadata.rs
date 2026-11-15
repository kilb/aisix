//! 真实路由、JWT 验签、CP/mTLS、Postgres 和终态 outbox 的元数据回归。
//! 供应商 HTTP 和自定义扫描服务为本地协议替身，不冒充外部供应商端到端验收。
//! 业务断言来自调用者合同；不调用元数据快照、填充或合并 helper。
//! JWT 信任和映射为显式网关本地组件配置，经正式资源加载器读取，不能声称由 CP 投影。
//! 租户、key、模型、防护规则和持久权威仍来自真实 CP；本测试不验证两源混合启动部署。

#[path = "support/native_gateway.rs"]
mod native_gateway;
mod support;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::native_objects::InferenceStage;
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ProxyConfig};
use aisix_etcd::{build_snapshot, PrefixSet, RawEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http_body_util::{BodyExt, Limited};
use native_gateway::{projected_snapshot, HttpRegistry};
use ring::hmac;
use serde_json::{json, Value};
use support::{Bundle, OperatorSession, TestCp};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use uuid::Uuid;

const ALIAS: &str = "metadata-public-alias";
const SECOND: &str = "metadata-second-public";
const LABEL: &str = "metadata-original-key";
const UPSTREAM_KEY: &str = "sk-local-metadata-provider-contract";
const SIGNING_KEY: &str = "metadata-hmac-fixture-at-least-thirty-two-bytes";
const COOKIE_CANARY: &str = "metadata_cookie_secret_31e8";
const UNSELECTED_CANARY: &str = "metadata_unselected_header_secret_77ab";
const RESPONSE_ID: &str = "resp_metadata_background";
const BATCH_ID: &str = "batch_metadata_entries";
const INPUT_FILE: &str = "file_metadata_original";
const STAGED_FILE: &str = "file_metadata_staged";
const OUTPUT_FILE: &str = "file_metadata_output";
const A_TEXT: &str = "metadata-entry-a alice@example.com";
const A_MASKED: &str = "metadata-entry-a MASKED";
const B_TEXT: &str = "metadata-entry-b clean";
const COMPLETED_AT: i64 = 1_794_614_500;

#[derive(Default)]
struct ProviderState {
    terminal: AtomicBool,
    scans: Mutex<Vec<String>>,
    otlp_payloads: Mutex<Vec<Value>>,
    calls: Mutex<Vec<(String, String, Vec<u8>)>>,
    files: Mutex<Vec<(String, Vec<u8>)>>,
    batch_input: Mutex<Option<String>>,
}

struct Provider {
    base: String,
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start() -> Self {
        let state = Arc::new(ProviderState::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the provider protocol substitute");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(provider).with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }

    fn complete(&self) {
        self.state.terminal.store(true, Ordering::SeqCst);
    }

    fn scans(&self) -> Vec<String> {
        self.state.scans.lock().unwrap().clone()
    }

    fn file(&self, id: &str) -> Vec<Value> {
        let files = self.state.files.lock().unwrap();
        let bytes = &files.iter().find(|file| file.0 == id).unwrap().1;
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    // 超时仅输出计数、协议属性名和比较结果；不输出 span 内容、id 或任何 header 值。
    fn otlp_timeout_diagnostics(&self, request_id: &str, phase: &str) -> Value {
        let payloads = self.state.otlp_payloads.lock().unwrap();
        let spans: Vec<&Value> = payloads
            .iter()
            .flat_map(|payload| {
                payload
                    .get("resourceSpans")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .flat_map(|resource| {
                resource
                    .get("scopeSpans")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .flat_map(|scope| {
                scope
                    .get("spans")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .collect();
        let has_key = |span: &Value, key: &str| {
            span.get("attributes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|field| field.get("key").and_then(Value::as_str) == Some(key))
        };
        let payload_keys: std::collections::BTreeSet<_> = payloads
            .iter()
            .filter_map(Value::as_object)
            .flat_map(|object| object.keys())
            .cloned()
            .collect();
        let attribute_keys: std::collections::BTreeSet<_> = spans
            .iter()
            .flat_map(|span| {
                span.get("attributes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(|field| field.get("key").and_then(Value::as_str))
            .collect();
        let value_encoding_keys: std::collections::BTreeSet<_> = spans
            .iter()
            .flat_map(|span| {
                span.get("attributes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(|field| field.get("value").and_then(Value::as_object))
            .flat_map(|object| object.keys())
            .collect();
        let distinct_request_ids = spans
            .iter()
            .filter_map(|span| otlp_text(span, "aisix.request_id"))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let selected_key = "http.request.header.x-metadata-project";
        json!({
            "received_payload_count":payloads.len(),
            "received_span_count":spans.len(),
            "payload_top_level_keys":payload_keys,
            "span_attribute_keys":attribute_keys,
            "attribute_value_encoding_keys":value_encoding_keys,
            "request_id_key_span_count":spans.iter().filter(|span| has_key(span,"aisix.request_id")).count(),
            "distinct_request_id_count":distinct_request_ids,
            "correlated_span_count":spans.iter().filter(|span| otlp_text(span,"aisix.request_id")==Some(request_id)).count(),
            "selected_header_key_span_count":spans.iter().filter(|span| has_key(span,selected_key)).count(),
            "selected_header_expected_value_span_count":spans.iter().filter(|span| otlp_header_values(span,selected_key)==Some(vec![phase])).count(),
            "correlated_selected_header_key_span_count":spans.iter().filter(|span| otlp_text(span,"aisix.request_id")==Some(request_id) && has_key(span,selected_key)).count(),
            "correlated_selected_header_expected_value_span_count":spans.iter().filter(|span| otlp_text(span,"aisix.request_id")==Some(request_id) && otlp_header_values(span,selected_key)==Some(vec![phase])).count(),
            "selected_header_expected_value_on_other_request_count":spans.iter().filter(|span| otlp_text(span,"aisix.request_id").is_some_and(|id| id!=request_id) && otlp_header_values(span,selected_key)==Some(vec![phase])).count(),
            "selected_header_expected_value_without_request_id_count":spans.iter().filter(|span| otlp_text(span,"aisix.request_id").is_none() && otlp_header_values(span,selected_key)==Some(vec![phase])).count()
        })
    }

    // 只观察真实 handler 触发的正式 exporter HTTP，不直接构造或发送 span。
    async fn exported_request_headers(&self, request_id: &str, phase: &str) {
        assert!(
            !request_id.is_empty(),
            "a real accepted/management request has a correlation id"
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let spans: Vec<Value> = self
                    .state
                    .otlp_payloads
                    .lock()
                    .unwrap()
                    .iter()
                    .flat_map(|payload| {
                        payload
                            .get("resourceSpans")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .flat_map(|resource| {
                        resource
                            .get("scopeSpans")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .flat_map(|scope| {
                        scope
                            .get("spans")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .filter(|span| otlp_text(span, "aisix.request_id") == Some(request_id))
                    .cloned()
                    .collect();
                if spans.iter().any(|span| {
                    otlp_header_values(span, "http.request.header.x-metadata-project") == Some(vec![phase])
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("the real DP exporter sends the selected nonempty request header to OTLP HTTP; collector_diagnostics={}",
                self.otlp_timeout_diagnostics(request_id, phase));
        });
        // 调用者先关闭两个正式 fanout；检查全部已完成的真实 HTTP payload，覆盖资源属性和无关联 span 的泄漏。
        for payload in self.state.otlp_payloads.lock().unwrap().iter() {
            let wire = payload.to_string();
            for forbidden in [
                "http.request.header.authorization",
                "http.request.header.cookie",
                "http.request.header.x-metadata-private",
            ] {
                assert!(
                    !wire.contains(forbidden),
                    "credentials and unselected headers never appear in actual exporter output"
                );
            }
            for secret in [COOKIE_CANARY, UNSELECTED_CANARY, SIGNING_KEY, UPSTREAM_KEY] {
                assert!(
                    !wire.contains(secret),
                    "sensitive header values never reach actual OTLP output"
                );
            }
            assert!(
                !wire.contains(&URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#)),
                "the raw signed credential never reaches actual OTLP output"
            );
        }
    }

    fn posts(&self, path: &str) -> Vec<Value> {
        self.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.0 == "POST" && call.1 == path)
            .map(|call| serde_json::from_slice(&call.2).unwrap())
            .collect()
    }
}

fn otlp_text<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span.get("attributes")?
        .as_array()?
        .iter()
        .find(|field| field.get("key").and_then(Value::as_str) == Some(key))?
        .pointer("/value/stringValue")?
        .as_str()
}

// HTTP header 的 OTLP 属性是 string[]，单值也必须精确解码为单元素数组。
// https://opentelemetry.io/docs/specs/semconv/registry/attributes/http/
fn otlp_header_values<'a>(span: &'a Value, key: &str) -> Option<Vec<&'a str>> {
    span.get("attributes")?
        .as_array()?
        .iter()
        .find(|field| field.get("key").and_then(Value::as_str) == Some(key))?
        .pointer("/value/arrayValue/values")?
        .as_array()?
        .iter()
        .map(|value| value.get("stringValue")?.as_str())
        .collect()
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// 形状来自官方 SDK：openai-python/src/openai/types/{responses/response,batch}.py。
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response.py
// https://github.com/openai/openai-python/blob/main/src/openai/types/batch.py
fn response_body(done: bool) -> Value {
    let mut body = json!({
        "id":RESPONSE_ID,"object":"response","created_at":COMPLETED_AT - 100,
        "status":if done { "completed" } else { "queued" },"model":"gpt-4o-mini",
        "background":true,"store":true,"error":null,"incomplete_details":null,
        "output":[],"usage":null
    });
    if done {
        body["completed_at"] = json!(COMPLETED_AT);
        body["output"] = json!([{"id":"msg_metadata_answer","type":"message",
            "role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"classified","annotations":[]}]}]);
        body["usage"] = json!({"input_tokens":7,"output_tokens":3,"total_tokens":10});
    }
    body
}

fn batch_body(state: &ProviderState) -> Value {
    let done = state.terminal.load(Ordering::SeqCst);
    json!({"id":BATCH_ID,"object":"batch","completion_window":"24h",
        "endpoint":"/v1/chat/completions","created_at":COMPLETED_AT - 100,
        "input_file_id":state.batch_input.lock().unwrap().clone(),
        "status":if done { "completed" } else { "in_progress" },
        "completed_at":if done { Some(COMPLETED_AT) } else { None },
        "output_file_id":if done { Some(OUTPUT_FILE) } else { None },
        "request_counts":{"total":2,"completed":if done { 2 } else { 0 },"failed":0}})
}

fn output_lines() -> String {
    [("A", "gpt-4o-mini", 100, 10), ("B", "gpt-4o-mini", 200, 30)]
        .into_iter()
        .map(|(id, model, input, output)| {
            format!("{}\n", json!({"id":format!("batch_req_metadata_{id}"),"custom_id":id,
                "response":{"status_code":200,"request_id":format!("provider-{id}"),"body":{
                    "id":format!("chatcmpl_metadata_{id}"),"object":"chat.completion",
                    "created":COMPLETED_AT,"model":model,
                    "choices":[{"index":0,"message":{"role":"assistant","content":"classified"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":input + output}}},
                "error":null}))
        })
        .collect()
}

fn file_body(id: &str, bytes: usize) -> Value {
    json!({"id":id,"object":"file","purpose":"batch","filename":"metadata.jsonl",
        "created_at":COMPLETED_AT - 100,"bytes":bytes})
}

async fn provider(State(state): State<Arc<ProviderState>>, request: Request<Body>) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {UPSTREAM_KEY}").as_str());
    let bytes = to_bytes(request.into_body(), 1 << 20).await.unwrap();
    if path == "/v1/traces" {
        assert_eq!(method, "POST", "the actual exporter uses OTLP HTTP POST");
        state
            .otlp_payloads
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&bytes).expect("actual OTLP HTTP JSON"));
        return Json(json!({})).into_response();
    }
    if path == "/scan" {
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        state
            .scans
            .lock()
            .unwrap()
            .push(body["text"].as_str().unwrap().to_owned());
        return Json(json!({"ok":true})).into_response();
    }
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state
        .calls
        .lock()
        .unwrap()
        .push((method.clone(), path.clone(), bytes.to_vec()));
    match (method.as_str(), path.as_str()) {
        ("POST", "/v1/responses") => Json(response_body(false)).into_response(),
        ("GET", "/v1/responses/resp_metadata_background") => {
            Json(response_body(state.terminal.load(Ordering::SeqCst))).into_response()
        }
        ("GET", "/v1/responses/resp_metadata_background/input_items") => {
            Json(json!({"object":"list","data":[],"has_more":false})).into_response()
        }
        ("POST", "/v1/files") => {
            let boundary = content_type
                .split("boundary=")
                .nth(1)
                .unwrap()
                .trim_matches('"');
            let text = std::str::from_utf8(&bytes).unwrap();
            let file = text
                .split(&format!("--{boundary}"))
                .find(|part| part.contains("filename=\""))
                .and_then(|part| part.split_once("\r\n\r\n"))
                .map(|(_, body)| body.strip_suffix("\r\n").unwrap().as_bytes().to_vec())
                .expect("the real gateway sent a multipart file");
            let mut files = state.files.lock().unwrap();
            let id = if files.is_empty() {
                INPUT_FILE
            } else {
                STAGED_FILE
            };
            let body = file_body(id, file.len());
            files.push((id.to_owned(), file));
            Json(body).into_response()
        }
        ("POST", "/v1/batches") => {
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let input = body["input_file_id"].as_str().unwrap();
            if !state
                .files
                .lock()
                .unwrap()
                .iter()
                .any(|file| file.0 == input)
            {
                return StatusCode::NOT_FOUND.into_response();
            }
            *state.batch_input.lock().unwrap() = Some(input.to_owned());
            Json(batch_body(&state)).into_response()
        }
        ("GET", "/v1/batches/batch_metadata_entries") => Json(batch_body(&state)).into_response(),
        ("GET", "/v1/files/file_metadata_output/content") => {
            ([("content-type", "application/jsonl")], output_lines()).into_response()
        }
        ("GET", "/v1/files/file_metadata_output") => {
            Json(file_body(OUTPUT_FILE, output_lines().len())).into_response()
        }
        ("GET", path) if path.starts_with("/v1/files/") => {
            let raw = path.strip_prefix("/v1/files/").unwrap();
            let (id, content) = raw
                .strip_suffix("/content")
                .map_or((raw, false), |id| (id, true));
            let files = state.files.lock().unwrap();
            let Some((_, file)) = files.iter().find(|file| file.0 == id) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            if content {
                file.clone().into_response()
            } else {
                Json(file_body(id, file.len())).into_response()
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    provider: Provider,
    bundle: Bundle,
    alias: Uuid,
    target: Uuid,
    second: Uuid,
    account: Uuid,
    tenant: Uuid,
    member: Uuid,
    key: Uuid,
}

fn uuid_at(value: &Value, pointer: &str) -> Uuid {
    Uuid::parse_str(
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .expect(pointer),
    )
    .unwrap()
}

async fn resource(cp: &TestCp, admin: &OperatorSession, kind: &str, doc: Value) -> Uuid {
    let reply = cp
        .send(
            "POST",
            &format!("/admin/resources/{kind}"),
            Some(admin.cookie()),
            Some(json!({"doc":doc})),
        )
        .await;
    assert_eq!(reply.status, 201, "setup: create {kind}: {reply:?}");
    uuid_at(&reply.body, "/id")
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start_with_kine().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = Provider::start().await;
    let account = resource(
        &cp,
        &admin,
        "provider_keys",
        json!({
        "display_name":"metadata-account","provider":"openai","adapter":"openai",
        "api_base":provider.base,"api_key":UPSTREAM_KEY,"groups":["shared"]}),
    )
    .await;
    let target = resource(
        &cp,
        &admin,
        "models",
        json!({"display_name":"metadata-actual-target",
        "provider":"openai","model_name":"gpt-4o-mini","provider_key_id":account}),
    )
    .await;
    // 同一批次文件只使用一个上游模型；两个内部 direct 资源仍保留不同身份。
    // https://developers.openai.com/api/docs/guides/batch#1-prepare-your-batch-file
    let second = resource(
        &cp,
        &admin,
        "models",
        json!({"display_name":SECOND,
        "provider":"openai","model_name":"gpt-4o-mini","provider_key_id":account}),
    )
    .await;
    let alias = resource(
        &cp,
        &admin,
        "models",
        json!({"display_name":ALIAS,
        "routing":{"strategy":"failover","retries":0,"targets":[{"model_id":target}]}}),
    )
    .await;
    assert_ne!(
        alias, target,
        "scenario: the public alias must differ from the dispatched target"
    );
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let funded = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/credits"),
            Some(admin.cookie()),
            Some(json!({"amount_cny":"1","reason":"原生元数据真实旅程余额"})),
        )
        .await;
    assert_eq!(funded.status, 201, "{funded:?}");
    let path = format!("/admin/tenants/{tenant}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let granted = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"].as_i64().unwrap(),
            json!({"allowed_model_ids":[alias,target,second]}),
        )
        .await;
    assert_eq!(granted.status, 200, "{granted:?}");
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    let minted = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(admin.cookie()),
            Some(json!({"label":LABEL,"group_id":group,"member_id":member})),
        )
        .await;
    assert_eq!(minted.status, 201, "{minted:?}");
    let key = uuid_at(&minted.body, "/key/id");
    // CP 尚无 oidc_providers/claim_mappings 写入口；两类 JWT 配置在 gateway() 明确按本地组件资源加载。
    resource(
        &cp,
        &admin,
        "observability_exporters",
        json!({"name":"metadata-selected-headers",
        "kind":"otlp_http","endpoint":format!("{}/v1/traces", provider.base),
        "content_mode":"metadata_only","sample_rate":1,
        "request_headers":["x-metadata-project"]}),
    )
    .await;
    let script = format!(
        r#"export async function checkInput(ctx) {{
        const reply = await fetch("{}/scan", {{method:"POST", headers:{{"content-type":"application/json"}}, body:JSON.stringify({{text:ctx.text}})}});
        if (!reply.ok) throw new Error("scan failed");
        if (ctx.text.includes("metadata-entry-a")) throw new Error("entry-a scanner unavailable");
        return {{action:"allow"}};
    }}"#,
        provider.base
    );
    let rules = [
        json!({"name":"metadata-fail-open","kind":"custom","hook_point":"input","fail_open":true,"script":script,"timeout_ms":5000}),
        json!({"name":"metadata-monitor","kind":"keyword","hook_point":"input","enforcement_mode":"monitor", "patterns":[{"kind":"literal","value":"metadata-entry-a"}]}),
        json!({"name":"metadata-mask","kind":"pii","hook_point":"input","detectors":[],
            "custom_patterns":[{"name":"private_email","regex":"alice@example\\.com","action":"mask","replacement":"MASKED"}]}),
    ];
    for (priority, rule) in rules.into_iter().enumerate() {
        let id = resource(&cp, &admin, "guardrails", rule).await;
        for model in [alias, second] {
            resource(
                &cp,
                &admin,
                "guardrail_attachments",
                json!({"guardrail_id":id,
                "scope_type":"model","scope_id":model,"priority":100 + priority}),
            )
            .await;
        }
    }
    for model in [alias, target, second] {
        support::world::insert_baseline_backdated(cp.db(), model,
            json!({"default":{"base":{"input":"2","output":"4"}},"batch":{"base":{"input":"2","output":"4"}}}),
            "2000-01-01T00:00:00Z".parse().unwrap()).await;
    }
    let bundle = cp.issue_bundle(&admin, "metadata-handler-gateway").await;
    Some(Fixture {
        cp,
        admin,
        provider,
        bundle,
        alias,
        target,
        second,
        account,
        tenant,
        member,
        key,
    })
}

// 只修正测试接线：与既有 JWT 旅程同用规范资源文档和正式 loader，不伪造已认证身份。
// 这里显式组合组件来源，不宣称 CP API 或二进制启动支持混合两种配置源。
fn with_explicit_local_jwt_component_config(snapshot: AisixSnapshot, key: Uuid) -> AisixSnapshot {
    assert!(
        snapshot.apikeys.get_by_id(&key.to_string()).is_some(),
        "setup: the JWT mapping targets the real CP-minted, projected key"
    );
    assert_eq!(
        snapshot.oidc_providers.len(),
        0,
        "setup: JWT providers are not claimed to come from the current CP projection"
    );
    assert_eq!(
        snapshot.claim_mappings.len(),
        0,
        "setup: JWT mappings are not claimed to come from the current CP projection"
    );
    let prefix = "/explicit-gateway-local-jwt-component-config";
    let mut entries = Vec::new();
    for phase in ["admission", "observer"] {
        let provider = json!({
            "name":format!("metadata-{phase}-idp"),"hmac_secret":SIGNING_KEY,
            "issuer":format!("https://metadata-{phase}.example"),
            "tag_claims":{"department":"department"}});
        let mapping = json!({"name":format!("metadata-{phase}-mapping"),
            "jwt_provider":format!("metadata-{phase}-idp"),
            "match":[{"claim":"sub","op":"exact","values":[format!("metadata-{phase}-subject")]}],
            "resolve":{"api_key_id":key}});
        for (kind, document) in [("oidc_providers", provider), ("claim_mappings", mapping)] {
            entries.push(RawEntry {
                key: format!("{prefix}/{kind}/{}", Uuid::new_v4()),
                value: serde_json::to_vec(&document)
                    .expect("canonical local JWT resource document"),
                revision: 1,
            });
        }
    }
    let (local, stats) = build_snapshot(&PrefixSet::single(prefix.to_owned()), &entries);
    assert_eq!(
        stats.accepted,
        entries.len(),
        "setup: the real JWT resource loader accepts all four canonical documents: {stats:?}"
    );
    assert!(
        stats.rejections.is_empty(),
        "setup: local JWT configuration has no rejected rows: {stats:?}"
    );
    assert_eq!(local.oidc_providers.len(), 2);
    assert_eq!(local.claim_mappings.len(), 2);
    for entry in local.oidc_providers.entries() {
        snapshot.oidc_providers.insert(entry.as_ref().clone());
    }
    for entry in local.claim_mappings.entries() {
        snapshot.claim_mappings.insert(entry.as_ref().clone());
    }
    snapshot
}

async fn gateway(f: &Fixture) -> (ProxyState, mpsc::Receiver<UsageEvent>) {
    let projected = projected_snapshot(&f.cp).await;
    let snapshot = SnapshotHandle::new(with_explicit_local_jwt_component_config(projected, f.key));
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let config: ProxyConfig =
        serde_json::from_value(json!({"addr":"127.0.0.1:0","request_body_limit_bytes":1048576}))
            .unwrap();
    let guardrails = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let (tx, rx) = mpsc::channel(64);
    let state = ProxyState::new(snapshot, hub, &config)
        .without_cache()
        .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
            &f.cp.state().pki.platform_id_key_hex(),
        ))
        .with_native_objects(Arc::new(HttpRegistry::new(&f.cp, &f.bundle)))
        .with_guardrail_index(guardrails)
        .with_usage_sink(UsageSink::new(tx));
    (state, rx)
}

async fn verified_auth_preflight(state: &ProxyState) {
    let good = token("admission");
    let (header_and_claims, _) = good.rsplit_once('.').unwrap();
    let bad = format!("{header_and_claims}.{}", URL_SAFE_NO_PAD.encode([0_u8; 32]));
    for (bearer, expected) in [(good, StatusCode::OK), (bad, StatusCode::UNAUTHORIZED)] {
        let request = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap();
        let response = build_router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        Limited::new(response.into_body(), 1 << 20)
            .collect()
            .await
            .unwrap();
        assert_eq!(
            status, expected,
            "setup: real JWT verification accepts the signature and rejects its forgery"
        );
    }
}

fn token(phase: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        json!({"sub":format!("metadata-{phase}-subject"),
        "iss":format!("https://metadata-{phase}.example"),"department":phase,
        "exp":chrono::Utc::now().timestamp() + 3600})
        .to_string(),
    );
    let message = format!("{header}.{claims}");
    let signature = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, SIGNING_KEY.as_bytes()),
        message.as_bytes(),
    );
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
}

async fn send(
    state: &ProxyState,
    method: &str,
    path: &str,
    phase: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {}", token(phase)))
        .header("cookie", format!("session={COOKIE_CANARY}"))
        .header("x-metadata-project", phase)
        .header("x-metadata-private", UNSELECTED_CANARY)
        .header("content-type", content_type)
        .body(Body::from(bytes))
        .unwrap();
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = Limited::new(response.into_body(), 1 << 20)
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let body =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, body)
}

async fn json_request(
    state: &ProxyState,
    method: &str,
    path: &str,
    phase: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        state,
        method,
        path,
        phase,
        "application/json",
        serde_json::to_vec(&body).unwrap(),
    )
    .await
}

fn drain_typed(rx: &mut mpsc::Receiver<UsageEvent>) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn drain(rx: &mut mpsc::Receiver<UsageEvent>) -> Vec<Value> {
    drain_typed(rx)
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

// 先观察 DP 私有出口；UsageEvent 的公共序列化有意不携带请求头。
fn private_request_header(event: &UsageEvent, phase: &str) {
    let headers = serde_json::to_value(&event.request_headers).unwrap();
    assert_eq!(
        headers.get("x-metadata-project").and_then(Value::as_str),
        Some(phase),
        "the actual DP event retains the selected request header before public serialization"
    );
}

fn original_metadata(event: &Value) {
    assert_eq!(
        event["api_key_name"], LABEL,
        "the accepted key label is historical"
    );
    assert_eq!(event["tags"], json!({"department":"admission"}));
    assert_eq!(event["jwt_subject"], "metadata-admission-subject");
    assert_eq!(event["jwt_provider"], "metadata-admission-idp");
    assert_eq!(event["jwt_claim_mapping"], "metadata-admission-mapping");
    assert!(
        !event.as_object().unwrap().contains_key("request_headers"),
        "request headers belong only to private DP observation, never the public billing payload"
    );
    let wire = event.to_string();
    for secret in [COOKIE_CANARY, UNSELECTED_CANARY, SIGNING_KEY, UPSTREAM_KEY] {
        assert!(
            !wire.contains(secret),
            "credential values must not become metadata"
        );
    }
    assert!(
        !wire.contains(&URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#)),
        "the raw signed credential must not become metadata"
    );
}

fn input_audit(event: &Value, is_a: bool) {
    let applied = event["applied_guardrails"]
        .as_array()
        .expect("executed input rules are recorded");
    for kind in ["custom", "keyword", "pii"] {
        assert!(
            applied
                .iter()
                .any(|rule| rule["hook"] == "input" && rule["kind"] == kind),
            "missing {kind}: {event}"
        );
    }
    let bypass = event
        .get("guardrail_bypassed_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let monitor: Vec<_> = event
        .get("guardrail_monitor_hits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let enforced: Vec<_> = event
        .get("guardrail_enforced_hits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    if is_a {
        assert!(
            !bypass.is_empty(),
            "the actually failed-open entry must retain its bypass"
        );
        assert!(monitor
            .iter()
            .any(|hit| hit["guardrail_name"] == "metadata-monitor"
                && hit["hook"] == "input"
                && hit["action"] == "would_block"));
        assert!(enforced
            .iter()
            .any(|hit| hit["guardrail_name"] == "metadata-mask"
                && hit["hook"] == "input"
                && hit["action"] == "masked"
                && hit["counts"]["private_email"] == 1));
    } else {
        assert!(bypass.is_empty(), "entry B never bypassed: {event}");
        assert!(
            monitor.is_empty(),
            "entry A's monitor result must not be broadcast to B: {event}"
        );
        assert!(
            enforced.is_empty(),
            "entry A's mask must not be broadcast to B: {event}"
        );
    }
}

async fn record(f: &Fixture, raw: &str) -> (Uuid, Uuid) {
    let row: (Uuid, Uuid, Uuid, Uuid, Option<Uuid>, Uuid, Uuid) = sqlx::query_as(
        "SELECT id,operation_id,tenant_id,api_key_id,member_id,model_id,provider_key_id FROM native_objects WHERE raw_id=$1")
        .bind(raw).fetch_one(f.cp.db().pool()).await.expect("positive control: admission stored a real native object");
    assert_eq!(
        (row.2, row.3, row.4, row.5, row.6),
        (f.tenant, f.key, Some(f.member), f.target, f.account)
    );
    (row.0, row.1)
}

const CALLER_SNAPSHOT_FIELDS: [&str; 5] = [
    "api_key_name",
    "tags",
    "jwt_subject",
    "jwt_provider",
    "jwt_claim_mapping",
];
const AUDIT_SNAPSHOT_FIELDS: [&str; 5] = [
    "applied_guardrails",
    "guardrail_bypassed_reason",
    "guardrail_monitor_hits",
    "guardrail_enforced_hits",
    "guardrail_scores",
];

// outbox 的内部表示可以从不可变提交恢复十个观测字段，其余权威值一字不改。
fn assert_authoritative_terminal_preserved(raw: &Value, delivered: &Value) {
    let mut raw_authority = raw.as_object().expect("actual raw outbox object").clone();
    let mut delivered_authority = delivered
        .as_object()
        .expect("actual delivered usage object")
        .clone();
    for name in CALLER_SNAPSHOT_FIELDS
        .into_iter()
        .chain(AUDIT_SNAPSHOT_FIELDS)
    {
        if let Some(saved) = raw_authority.remove(name) {
            assert_eq!(
                delivered.get(name),
                Some(&saved),
                "a snapshot field already carried by the raw outbox remains exact: {name}"
            );
        }
        delivered_authority.remove(name);
    }
    assert_eq!(raw_authority, delivered_authority,
        "every authority, token/cache, clock, identity, price and other non-snapshot field remains exact");
}

async fn admission_submission(f: &Fixture, operation: Uuid) -> Value {
    sqlx::query_scalar(
        "SELECT submission FROM native_operations WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.env().env_id)
    .bind(operation)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("read the real immutable admission submission")
}

fn assert_admitted_observations_delivered(caller: &Value, audit: &Value, delivered: &Value) {
    // 调用方五字段另有非空业务正控，不能用缺失值互相比较来买绿。
    for name in CALLER_SNAPSHOT_FIELDS {
        let accepted = caller
            .get(name)
            .expect("the original caller snapshot carries all five fields");
        assert_eq!(
            delivered.get(name),
            Some(accepted),
            "the accepted caller field reaches the final sink: {name}"
        );
    }
    // 公共 usage 可省略空审计字段；所有实际扫描产生的非空值必须完整相等。
    for name in AUDIT_SNAPSHOT_FIELDS {
        let empty = if name == "guardrail_bypassed_reason" {
            json!("")
        } else {
            json!([])
        };
        assert_eq!(
            delivered.get(name).unwrap_or(&empty),
            audit.get(name).unwrap_or(&empty),
            "the entry's own accepted input audit reaches the final sink: {name}"
        );
    }
}

async fn terminals(f: &Fixture, record: Uuid, count: i64) -> Vec<(String, Value, i64)> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let posted: i64 = sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox o JOIN usage_events u ON u.event_id=o.event_id::text JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1 AND o.delivered_at IS NOT NULL")
                .bind(record).fetch_one(f.cp.db().pool()).await.unwrap();
            if posted >= count { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("positive control: real worker, terminal outbox, sink and nonzero ledger all complete");
    let rows: Vec<(String, Value, i64, Value, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT o.entry_id,o.event,c.charge_micro_cny,u.event,-l.amount_micro_cny,u.occurred_at FROM native_terminal_outbox o JOIN pricing_charges_cny c ON c.event_id=o.event_id::text JOIN usage_events u ON u.event_id=o.event_id::text JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1 ORDER BY o.entry_id")
        .bind(record).fetch_all(f.cp.db().pool()).await.unwrap();
    assert_eq!(
        rows.len(),
        count as usize,
        "one priced and delivered terminal per entry"
    );
    rows.into_iter()
        .map(|(entry, event, charge, delivered, debit, occurred)| {
            assert!(charge > 0, "the terminal must actually be billed");
            assert_eq!(charge, debit);
            assert_authoritative_terminal_preserved(&event, &delivered);
            // 后续业务断言观察真实最终 usage_events，不能把允许精简的 outbox 当最终结果。
            let event = delivered;
            assert_eq!(
                occurred.timestamp(),
                COMPLETED_AT,
                "provider completion time remains authoritative"
            );
            assert_eq!(
                chrono::DateTime::parse_from_rfc3339(event["occurred_at"].as_str().unwrap())
                    .unwrap()
                    .timestamp(),
                COMPLETED_AT
            );
            assert_eq!(event["team_id"], json!(f.tenant));
            assert_eq!(event["user_id"], json!(f.member));
            assert_eq!(event["api_key_id"], json!(f.key));
            assert_eq!(event["inference_stage"], "terminal");
            (entry, event, charge)
        })
        .collect()
}

async fn rename_key(f: &Fixture) {
    let path = format!("/admin/keys/{}", f.key);
    let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let changed =
        f.cp.send_conditional(
            "PATCH",
            &path,
            f.admin.cookie(),
            current.body["version"].as_i64().unwrap(),
            json!({"label":"metadata-current-key"}),
        )
        .await;
    assert_eq!(changed.status, 200, "{changed:?}");
}

async fn replay_once(
    f: &Fixture,
    record: Uuid,
    events: Vec<Value>,
    expected_count: i64,
    expected_total: i64,
) {
    let delivered =
        support::world::deliver(&f.cp.dp_client(&f.bundle), Uuid::new_v4(), events).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(delivered.body["accepted"], 0);
    assert_eq!(delivered.body["rejected"], 0);
    assert_eq!(delivered.body["duplicates"], expected_count);
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT count(*),count(l.event_id),COALESCE(sum(-l.amount_micro_cny),0)::bigint FROM native_terminal_outbox o LEFT JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(record).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(counts, (expected_count, expected_count, expected_total));
}

// 公开 exporter 诊断只汇总计数，不打印 exporter 名称、地址、配置或 last_error。
fn otlp_fanout_counter_diagnostics(state: &ProxyState) -> Value {
    let stats = state.otlp_fan_out.exporter_stats();
    let queues = state.otlp_fan_out.exporter_queue_depths();
    let mut totals = [0_u64; 6];
    for snapshot in stats.values() {
        for (total, value) in totals.iter_mut().zip([
            snapshot.sent,
            snapshot.dropped,
            snapshot.rejected,
            snapshot.retries,
            snapshot.delivered_batches,
            snapshot.failed_batches,
        ]) {
            *total = total.saturating_add(value);
        }
    }
    json!({
        "exporter_count":stats.len(),"queue_count":queues.len(),
        "queued_record_count":queues.iter().fold(0_u64,|total,(_,depth)|total.saturating_add(*depth)),
        "sent":totals[0],"dropped":totals[1],"rejected":totals[2],"retries":totals[3],
        "delivered_batches":totals[4],"failed_batches":totals[5]
    })
}

async fn shutdown_exporters(state: &ProxyState) {
    eprintln!(
        "[native-otlp-diagnostics] before_shutdown={}",
        otlp_fanout_counter_diagnostics(state)
    );
    tokio::time::timeout(Duration::from_secs(10), state.otlp_fan_out.shutdown())
        .await
        .expect("the real DP exporter flushes and shuts down within the fixture budget");
    eprintln!(
        "[native-otlp-diagnostics] after_shutdown={}",
        otlp_fanout_counter_diagnostics(state)
    );
}

async fn stop(stop: watch::Sender<bool>, worker: tokio::task::JoinHandle<()>) {
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("the real worker shuts down")
        .unwrap();
}

// 删除 handler 的接受时元数据采集，或用观察者身份重新构建终态，会丢失原始非空值。
#[tokio::test]
async fn background_response_keeps_original_verified_metadata_through_real_authority_and_sink() {
    let Some(f) = fixture().await else { return };
    let (state, mut usage) = gateway(&f).await;
    verified_auth_preflight(&state).await;
    let (status, body) = json_request(
        &state,
        "POST",
        "/v1/responses",
        "admission",
        json!({"model":ALIAS,"background":true,"input":A_TEXT}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: real background admission: {body}"
    );
    assert_eq!(body["id"], RESPONSE_ID);
    assert_eq!(body["status"], "queued");
    let (object, operation) = record(&f, RESPONSE_ID).await;
    let admitted = admission_submission(&f, operation).await;
    let sent = f.provider.posts("/v1/responses");
    assert_eq!(
        sent.len(),
        1,
        "the real handler dispatched one accepted request"
    );
    assert_eq!(sent[0]["model"], "gpt-4o-mini");
    assert_eq!(
        sent[0]["input"], A_MASKED,
        "positive control: actual input mask reached the provider"
    );
    assert_eq!(
        f.provider.scans().len(),
        1,
        "the custom input rule actually ran"
    );
    assert!(f.provider.scans()[0].contains("metadata-entry-a"));
    let accepted: Vec<_> = drain_typed(&mut usage)
        .into_iter()
        .filter(|event| event.inference_stage == Some(InferenceStage::Accepted))
        .collect();
    assert_eq!(
        accepted.len(),
        1,
        "one real accepted event, no fabricated terminal"
    );
    rename_key(&f).await;
    let (recovery, mut observation) = gateway(&f).await;
    f.provider.complete();
    let (cancel, stopped) = watch::channel(false);
    let worker = tokio::spawn(aisix_proxy::native_api::background::run(
        recovery.clone(),
        stopped,
    ));
    let rows = terminals(&f, object, 1).await;
    stop(cancel, worker).await;
    assert_eq!(rows[0].0, "");
    assert_eq!(rows[0].2, 26, "7 input * 2 + 3 output * 4 micro-CNY");
    assert_eq!(rows[0].1["model_id"], json!(f.target));
    assert_eq!(rows[0].1["requested_model_id"], json!(f.alias));
    assert_eq!(rows[0].1["requested_model"], ALIAS);
    assert_eq!(rows[0].1["upstream_model"], "gpt-4o-mini");
    assert_eq!(rows[0].1["native_operation_id"], json!(operation));
    assert_eq!(
        (
            rows[0].1["prompt_tokens"].as_u64(),
            rows[0].1["completion_tokens"].as_u64()
        ),
        (Some(7), Some(3))
    );
    private_request_header(&accepted[0], "admission");
    let accepted_wire = serde_json::to_value(&accepted[0]).unwrap();
    assert_eq!(accepted_wire["native_operation_id"], json!(operation));
    original_metadata(&accepted_wire);
    input_audit(&accepted_wire, true);
    original_metadata(&rows[0].1);
    input_audit(&rows[0].1, true);
    let original_context = admitted
        .get("usage_context")
        .expect("the actual admission retained its observation snapshot");
    original_metadata(original_context);
    input_audit(original_context, true);
    assert_admitted_observations_delivered(original_context, original_context, &rows[0].1);
    assert_eq!(
        admission_submission(&f, operation).await,
        admitted,
        "completion leaves the complete original submission unchanged"
    );
    let (status, body) = json_request(
        &recovery,
        "GET",
        &format!("/v1/responses/{RESPONSE_ID}"),
        "observer",
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a different verified identity on the same current key can read: {body}"
    );
    assert_eq!(body["status"], "completed");
    let observed = drain(&mut observation);
    assert!(
        observed
            .iter()
            .any(|event| event["jwt_subject"] == "metadata-observer-subject"),
        "positive control: the observer JWT was actually verified"
    );
    assert_eq!(
        terminals(&f, object, 1).await[0].1,
        rows[0].1,
        "current observer metadata cannot rewrite historical metadata"
    );
    assert_eq!(f.provider.posts("/v1/responses").len(), 1);
    shutdown_exporters(&state).await;
    shutdown_exporters(&recovery).await;
    let accepted_request_id = accepted_wire["request_id"]
        .as_str()
        .expect("actual accepted request id");
    f.provider
        .exported_request_headers(accepted_request_id, "admission")
        .await;
    let observer = observed
        .iter()
        .find(|event| event["jwt_subject"] == "metadata-observer-subject")
        .unwrap();
    f.provider
        .exported_request_headers(
            observer["request_id"]
                .as_str()
                .expect("actual management request id"),
            "observer",
        )
        .await;
    replay_once(&f, object, vec![rows[0].1.clone()], 1, 26).await;
    assert_eq!(
        admission_submission(&f, operation).await,
        admitted,
        "the current observer and replay cannot rewrite any original submission value"
    );
}

// 让批次根审计广播给每个 entry，B 会错误得到 A 的旁路、monitor 和 mask。
#[tokio::test]
async fn actual_two_entry_batch_keeps_each_executed_input_audit_with_its_own_terminal() {
    let Some(f) = fixture().await else { return };
    let (state, mut usage) = gateway(&f).await;
    verified_auth_preflight(&state).await;
    let lines: String = [("A", ALIAS, A_TEXT), ("B", SECOND, B_TEXT)]
        .into_iter()
        .map(|(id, model, text)| {
            format!(
                "{}\n",
                json!({"custom_id":id,"method":"POST","url":"/v1/chat/completions",
            "body":{"model":model,"messages":[{"role":"user","content":text}]}})
            )
        })
        .collect();
    let boundary = "metadata-batch-input";
    // 文件传输使用同账号、不挂扫描规则的 direct 模型；输入规则在批次准入逐 entry 执行。
    let upload = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nmetadata-actual-target\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"metadata.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{lines}\r\n--{boundary}--\r\n");
    let (status, body) = send(
        &state,
        "POST",
        "/v1/files",
        "admission",
        &format!("multipart/form-data; boundary={boundary}"),
        upload.into_bytes(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: real uploaded input file: {body}"
    );
    assert_eq!(body["id"], INPUT_FILE);
    f.provider.state.scans.lock().unwrap().clear();
    drain(&mut usage);
    let (status, body) = json_request(&state, "POST", "/v1/batches", "admission",
        json!({"input_file_id":INPUT_FILE,"endpoint":"/v1/chat/completions","completion_window":"24h"})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: real two-entry batch admission: {body}"
    );
    assert_eq!(body["id"], BATCH_ID);
    let (object, operation) = record(&f, BATCH_ID).await;
    let admitted = admission_submission(&f, operation).await;
    let scans = f.provider.scans();
    assert_eq!(
        scans.len(),
        2,
        "the actual batch admission independently scans both entries"
    );
    assert_eq!(
        scans
            .iter()
            .filter(|text| text.contains("metadata-entry-a") && !text.contains("metadata-entry-b"))
            .count(),
        1
    );
    assert_eq!(
        scans
            .iter()
            .filter(|text| text.contains("metadata-entry-b") && !text.contains("metadata-entry-a"))
            .count(),
        1
    );
    let staged = f.provider.file(STAGED_FILE);
    assert_eq!(staged.len(), 2);
    assert_eq!(staged[0]["custom_id"], "A");
    assert_eq!(staged[0]["body"]["model"], "gpt-4o-mini");
    assert_eq!(staged[0]["body"]["messages"][0]["content"], A_MASKED);
    assert_eq!(staged[1]["custom_id"], "B");
    assert_eq!(staged[1]["body"]["model"], "gpt-4o-mini");
    assert_eq!(staged[1]["body"]["messages"][0]["content"], B_TEXT);
    assert_eq!(f.provider.posts("/v1/batches").len(), 1);
    let accepted: Vec<_> = drain_typed(&mut usage)
        .into_iter()
        .filter(|event| event.inference_stage == Some(InferenceStage::Accepted))
        .collect();
    rename_key(&f).await;
    let (recovery, _) = gateway(&f).await;
    f.provider.complete();
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&recovery)
            .await
            .unwrap(),
        1
    );
    let rows = terminals(&f, object, 2).await;
    // 先检查两条真实非零终态、独立 entry 和计量，再检查元数据隔离。
    for (index, (entry, event, charge)) in rows.iter().enumerate() {
        let (expected_entry, requested, target, upstream, input, output, expected_charge) =
            if index == 0 {
                ("A", f.alias, f.target, "gpt-4o-mini", 100, 10, 240)
            } else {
                ("B", f.second, f.second, "gpt-4o-mini", 200, 30, 520)
            };
        assert_eq!(entry, expected_entry);
        assert_eq!(event["requested_model_id"], json!(requested));
        assert_eq!(event["model_id"], json!(target));
        assert_eq!(event["upstream_model"], upstream);
        assert_eq!(event["native_operation_id"], json!(operation));
        assert_eq!(event["prompt_tokens"], input);
        assert_eq!(event["completion_tokens"], output);
        assert_eq!(*charge, expected_charge);
        assert_eq!(event["is_batch"], true);
    }
    assert_eq!(accepted.len(), 1, "one actual accepted batch event");
    private_request_header(&accepted[0], "admission");
    let accepted_wire = serde_json::to_value(&accepted[0]).unwrap();
    shutdown_exporters(&state).await;
    shutdown_exporters(&recovery).await;
    f.provider
        .exported_request_headers(
            accepted_wire["request_id"]
                .as_str()
                .expect("actual accepted batch request id"),
            "admission",
        )
        .await;
    let original_caller = admitted
        .get("usage_context")
        .expect("the batch admission retained its caller snapshot");
    original_metadata(original_caller);
    for (entry, event, _) in &rows {
        original_metadata(event);
        input_audit(event, entry == "A");
        let original_entry = admitted["entries"]
            .as_array()
            .expect("the actual admitted batch entries")
            .iter()
            .find(|candidate| candidate["entry_id"].as_str() == Some(entry.as_str()))
            .expect("each final custom_id belongs to its original admitted entry");
        let original_audit = original_entry
            .get("usage_context")
            .expect("each actually scanned entry retained its own input audit");
        input_audit(original_audit, entry == "A");
        assert_admitted_observations_delivered(original_caller, original_audit, event);
    }
    assert_eq!(
        admission_submission(&f, operation).await,
        admitted,
        "batch completion leaves all original caller and per-entry audit values unchanged"
    );
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&recovery)
            .await
            .unwrap(),
        0
    );
    replay_once(
        &f,
        object,
        rows.into_iter().map(|(_, event, _)| event).collect(),
        2,
        760,
    )
    .await;
    assert_eq!(f.provider.posts("/v1/batches").len(), 1);
    assert_eq!(
        admission_submission(&f, operation).await,
        admitted,
        "batch observation and replay cannot rewrite any original submission value"
    );
}

// 本组件只替代供应商协议；实际路由、CP/PG、worker 和账本保持真实，不冒充外部端到端验收。
// 跳过 group 输出链会使 A 保持原文；广播链会改坏 B；同一回答双跑会多一个后缀。
#[tokio::test]
async fn batch_group_output_mask_changes_only_its_own_entry_once_without_changing_settlement() {
    let Some(f) = fixture().await else { return };
    assert_ne!(f.alias, f.target, "the caller addresses a real group alias");
    assert_ne!(
        f.target, f.second,
        "the sibling retains its own actual target identity"
    );
    let guardrail = resource(
        &f.cp,
        &f.admin,
        "guardrails",
        json!({
            "name":"metadata-group-output","kind":"pii","hook_point":"output","detectors":[],
            "custom_patterns":[{"name":"group_result_word","regex":"classified",
                "action":"mask","replacement":"classified:screened"}]
        }),
    )
    .await;
    resource(
        &f.cp,
        &f.admin,
        "guardrail_attachments",
        json!({
            "guardrail_id":guardrail,"scope_type":"model","scope_id":f.alias,"priority":50
        }),
    )
    .await;
    let (state, mut usage) = gateway(&f).await;
    verified_auth_preflight(&state).await;
    let lines: String = [("A", ALIAS, A_TEXT), ("B", SECOND, B_TEXT)]
        .into_iter()
        .map(|(id, model, text)| {
            format!(
                "{}\n",
                json!({"custom_id":id,"method":"POST","url":"/v1/chat/completions",
            "body":{"model":model,"messages":[{"role":"user","content":text}]}})
            )
        })
        .collect();
    let boundary = "metadata-group-output-input";
    let upload = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nmetadata-actual-target\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"metadata.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{lines}\r\n--{boundary}--\r\n");
    let (status, body) = send(
        &state,
        "POST",
        "/v1/files",
        "admission",
        &format!("multipart/form-data; boundary={boundary}"),
        upload.into_bytes(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: actual direct-model file upload: {body}"
    );
    assert_eq!(body["id"], INPUT_FILE);
    let uploaded_bytes = f
        .provider
        .state
        .files
        .lock()
        .unwrap()
        .iter()
        .find(|file| file.0 == INPUT_FILE)
        .expect("the provider actually received the input file")
        .1
        .clone();
    // 上传已执行输入规则；只允许 A 的消息按已配置的替换值变化。
    let expected_upload: String = lines
        .lines()
        .map(|line| {
            let mut entry: Value =
                serde_json::from_str(line).expect("valid original fixture JSONL");
            if entry["custom_id"] == "A" {
                assert_eq!(entry["body"]["messages"][0]["content"], A_TEXT);
                entry["body"]["messages"][0]["content"] = json!(A_MASKED);
            }
            format!("{entry}\n")
        })
        .collect();
    assert_eq!(
        uploaded_bytes.as_slice(),
        expected_upload.as_bytes(),
        "the provider receives exactly A's configured input mask and every other original byte"
    );
    f.provider.state.scans.lock().unwrap().clear();
    drain(&mut usage);
    let (status, body) = json_request(&state, "POST", "/v1/batches", "admission",
        json!({"input_file_id":INPUT_FILE,"endpoint":"/v1/chat/completions","completion_window":"24h"})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "positive control: real batch acceptance: {body}"
    );
    assert_eq!(body["id"], BATCH_ID);
    let (object, operation) = record(&f, BATCH_ID).await;
    let admitted = admission_submission(&f, operation).await;
    let scans = f.provider.scans();
    assert_eq!(
        scans.len(),
        2,
        "both real input scans precede output policy"
    );
    for (own, sibling) in [
        ("metadata-entry-a", "metadata-entry-b"),
        ("metadata-entry-b", "metadata-entry-a"),
    ] {
        assert_eq!(
            scans
                .iter()
                .filter(|text| text.contains(own) && !text.contains(sibling))
                .count(),
            1
        );
    }
    let staged = f.provider.file(STAGED_FILE);
    assert_eq!(staged.len(), 2);
    for (index, custom_id, content) in [(0, "A", A_MASKED), (1, "B", B_TEXT)] {
        assert_eq!(staged[index]["custom_id"], custom_id);
        assert_eq!(
            staged[index]["body"]["model"], "gpt-4o-mini",
            "one upstream model per actual provider batch"
        );
        assert_eq!(staged[index]["body"]["messages"][0]["content"], content);
    }
    let submitted = f.provider.posts("/v1/batches");
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted[0]["input_file_id"], STAGED_FILE);
    assert_eq!(submitted[0]["endpoint"], "/v1/chat/completions");
    f.provider.complete();
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&state)
            .await
            .unwrap(),
        1
    );
    let rows = terminals(&f, object, 2).await;
    for (entry, event, charge) in &rows {
        let (requested, actual, input, output, amount) = match entry.as_str() {
            "A" => (f.alias, f.target, 100, 10, 240),
            "B" => (f.second, f.second, 200, 30, 520),
            _ => panic!("only the two actual custom ids can settle"),
        };
        assert_eq!(event["requested_model_id"], json!(requested));
        assert_eq!(event["model_id"], json!(actual));
        assert_eq!(event["native_operation_id"], json!(operation));
        assert_eq!(event["upstream_model"], "gpt-4o-mini");
        assert_eq!(event["prompt_tokens"], input);
        assert_eq!(event["completion_tokens"], output);
        assert_eq!(
            *charge, amount,
            "output policy never changes actual billed quantities"
        );
        original_metadata(event);
    }
    let (status, batch) = json_request(
        &state,
        "GET",
        &format!("/v1/batches/{BATCH_ID}"),
        "admission",
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "real completed batch management: {batch}"
    );
    assert_eq!(batch["status"], "completed");
    assert_eq!(batch["output_file_id"], OUTPUT_FILE);
    drain(&mut usage);
    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{OUTPUT_FILE}/content"))
        .header("authorization", format!("Bearer {}", token("admission")))
        .header("cookie", format!("session={COOKIE_CANARY}"))
        .header("x-metadata-project", "admission")
        .header("x-metadata-private", UNSELECTED_CANARY)
        .body(Body::empty())
        .unwrap();
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the actual owned batch result can be read"
    );
    let media_type = response
        .headers()
        .get("content-type")
        .expect("the real JSONL result media type")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .trim();
    assert_eq!(media_type, "application/jsonl");
    let bytes = Limited::new(response.into_body(), 1 << 20)
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let text = std::str::from_utf8(&bytes).expect("the actual batch result remains UTF-8 JSONL");
    let results: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each actual result line remains valid JSON"))
        .collect();
    assert_eq!(
        results.len(),
        2,
        "output policy retains both complete result lines"
    );
    for (id, model, content, input, output) in [
        ("A", ALIAS, "classified:screened", 100, 10),
        ("B", SECOND, "classified", 200, 30),
    ] {
        let matching: Vec<_> = results
            .iter()
            .filter(|row| row["custom_id"] == id)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "each original custom id survives exactly once"
        );
        let result = matching[0];
        assert_eq!(result["id"], format!("batch_req_metadata_{id}"));
        assert_eq!(
            result.get("error"),
            Some(&Value::Null),
            "the original explicit error field survives"
        );
        assert_eq!(result["response"]["status_code"], 200);
        assert_eq!(result["response"]["request_id"], format!("provider-{id}"));
        let body = &result["response"]["body"];
        assert_eq!(body["id"], format!("chatcmpl_metadata_{id}"));
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["created"], COMPLETED_AT);
        assert_eq!(body["model"], model);
        assert_eq!(
            body["choices"]
                .as_array()
                .expect("the original complete choices")
                .len(),
            1
        );
        let choice = &body["choices"][0];
        assert_eq!(choice["index"], 0);
        assert_eq!(choice["finish_reason"], "stop");
        assert_eq!(choice["message"]["role"], "assistant");
        assert_eq!(
            choice["message"]["content"], content,
            "group A is screened once and direct sibling B is untouched"
        );
        assert_eq!(body["usage"]["prompt_tokens"], input);
        assert_eq!(body["usage"]["completion_tokens"], output);
        assert_eq!(body["usage"]["total_tokens"], input + output);
    }
    let management = drain_typed(&mut usage);
    assert!(
        management.iter().any(
            |event| event.inference_stage == Some(InferenceStage::Management)
                && event
                    .guardrail_enforced_hits
                    .iter()
                    .any(|hit| hit.guardrail_name == "metadata-group-output"
                        && hit.hook == "output"
                        && hit.action == "masked"
                        && hit.counts.get("group_result_word").copied() == Some(1))
        ),
        "independent positive control: the actual management event records the applied output mask"
    );
    let calls = f.provider.state.calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|call| call.0 == "GET" && call.1 == format!("/v1/files/{OUTPUT_FILE}/content")),
        "the real provider result was fetched, not fabricated by the authority or fixture setup"
    );
    assert_eq!(calls.iter().filter(|call| call.0 == "POST").count(), 3,
        "one original upload, one staged upload and one accepted batch; no extra inference dispatch");
    assert!(f.provider.posts("/v1/responses").is_empty());
    assert_eq!(f.provider.posts("/v1/batches").len(), 1);
    assert_eq!(
        aisix_proxy::native_api::openai_batch_worker::run_once(&state)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        terminals(&f, object, 2).await,
        rows,
        "reading screened results cannot reattribute or rebill the accepted entries"
    );
    replay_once(
        &f,
        object,
        rows.into_iter().map(|(_, event, _)| event).collect(),
        2,
        760,
    )
    .await;
    assert_eq!(
        admission_submission(&f, operation).await,
        admitted,
        "output policy and management never rewrite the original caller or input audit snapshots"
    );
    shutdown_exporters(&state).await;
}
