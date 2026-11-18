//! WebSocket 握手必须在查找模型及尝试上游连接之前完成认证。

use aisix_core::native_objects::*;
use aisix_proxy::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ProxyConfig};
use aisix_gateway::Hub;
use aisix_proxy::{build_router, ProxyState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as ClientMessage};
use tower::ServiceExt;

const MODEL: &str = "11111111-1111-4111-8111-111111111111";
const PROVIDER: &str = "22222222-2222-4222-8222-222222222222";
const KEY: &str = "33333333-3333-4333-8333-333333333333";

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Default)]
struct Registry {
    records: Mutex<HashMap<(String, String), NativeRecord>>,
    submissions: Mutex<HashMap<Uuid, BeginRequest>>,
}

macro_rules! unused {
    ($name:ident, $request:ty) => {
        fn $name(&self, _: $request) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Err(NativeRegistryError::Unavailable) })
        }
    };
}

impl NativeObjectRegistry for Registry {
    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let record = self
                .records
                .lock()
                .unwrap()
                .get(&(
                    request.locator.kind.as_str().to_owned(),
                    request.locator.raw_id,
                ))
                .cloned()
                .ok_or(NativeRegistryError::NotFound)?;
            let submissions = self.submissions.lock().unwrap();
            let owned = record
                .operation_id
                .and_then(|id| submissions.get(&id))
                .is_some_and(|begin| begin.api_key_id == request.api_key_id);
            if !owned {
                return Err(NativeRegistryError::NotFound);
            }
            let mut reply = NativeReply::new("found");
            reply.record = Some(record);
            Ok(reply)
        })
    }
    fn begin(
        &self,
        request: BeginRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let mut reply = NativeReply::new("pending_submission");
            reply.operation_id = Some(request.operation_id);
            reply.origin = Some(request.origin.clone());
            self.submissions
                .lock()
                .unwrap()
                .insert(request.operation_id, request);
            Ok(reply)
        })
    }
    fn bind(
        &self,
        request: BindRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async move {
            let submitted = self
                .submissions
                .lock()
                .unwrap()
                .get(&request.operation_id)
                .cloned()
                .ok_or(NativeRegistryError::NotFound)?;
            if request.api_key_id != submitted.api_key_id {
                return Err(NativeRegistryError::NotFound);
            }
            let mut records = self.records.lock().unwrap();
            let key = ("response".to_owned(), request.raw_id.clone());
            let record=records.entry(key).or_insert_with(|| serde_json::from_value(json!({
                "record_id":Uuid::new_v4(), "operation_id":request.operation_id,"api_key_id":submitted.api_key_id,
                "origin":submitted.origin, "raw_id":request.raw_id,
                "protocol":"openai", "kind":"response", "version":1,
                "requested_model_id":submitted.requested_model_id,
                "requested_model":submitted.requested_model,
                "upstream_model":submitted.upstream_model
            })).unwrap()).clone();
            if record.operation_id != Some(request.operation_id)
                || request
                    .existing_record_id
                    .is_some_and(|id| id != record.record_id)
            {
                return Err(NativeRegistryError::Conflict);
            }
            let mut reply = NativeReply::new("bound");
            for object in request.objects {
                let key = (object.kind.as_str().to_owned(), object.raw_id.clone());
                let child=records.entry(key).or_insert_with(||serde_json::from_value(json!({
                    "record_id":Uuid::new_v4(),"operation_id":request.operation_id,"api_key_id":submitted.api_key_id,
                    "parent_id":record.record_id,"origin":submitted.origin,"raw_id":object.raw_id,
                    "protocol":"openai","kind":object.kind,"version":1,
                    "requested_model_id":submitted.requested_model_id,"requested_model":submitted.requested_model,
                    "upstream_model":submitted.upstream_model
                })).unwrap()).clone();
                if child.api_key_id != Some(submitted.api_key_id)
                    || child.origin != Some(submitted.origin.clone())
                {
                    return Err(NativeRegistryError::Conflict);
                }
                reply.records.push(child);
            }
            reply.record = Some(record);
            Ok(reply)
        })
    }
    fn claim(&self, _: ClaimRequest) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Ok(NativeReply::new("leased")) })
    }
    unused!(owned, OwnedRequest);
    unused!(tombstone, TombstoneRequest);
    unused!(renew, LeaseRequest);
    unused!(reschedule, LeaseRequest);
    unused!(complete, CompleteRequest);
}

struct Fixture {
    address: std::net::SocketAddr,
    snapshot: SnapshotHandle<AisixSnapshot>,
    upstream_calls: mpsc::UnboundedReceiver<Value>,
    upstream_messages: CapturedUpstreamMessages,
    usage: mpsc::Receiver<aisix_obs::UsageEvent>,
    audit_updates: mpsc::Receiver<aisix_obs::UsageIoAuditUpdate>,
    livez: Arc<aisix_proxy::health::LivezState>,
    proxy: tokio::task::JoinHandle<()>,
    upstream: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
enum CapturedUpstreamMessage {
    Request(String),
    Response(String),
}

type CapturedUpstreamMessages = Arc<Mutex<Vec<CapturedUpstreamMessage>>>;

async fn send_upstream_message<S>(
    writer: &mut S,
    payload: String,
    captured: &Option<CapturedUpstreamMessages>,
) -> Result<(), S::Error>
where
    S: futures::Sink<axum::extract::ws::Message> + Unpin,
{
    let recorded = captured.as_ref().map(|_| payload.clone());
    writer
        .send(axum::extract::ws::Message::Text(payload))
        .await?;
    if let Some((captured, recorded)) = captured.as_ref().zip(recorded) {
        captured
            .lock()
            .unwrap()
            .push(CapturedUpstreamMessage::Response(recorded));
    }
    Ok(())
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.proxy.abort();
        self.upstream.abort();
    }
}

async fn fixture() -> Fixture {
    fixture_with_options(false, false).await
}

async fn fixture_with_options(stream_timeout: bool, block_input: bool) -> Fixture {
    fixture_full(stream_timeout, block_input, false).await
}

async fn fixture_full(stream_timeout: bool, block_input: bool, platform: bool) -> Fixture {
    fixture_full_audit(stream_timeout, block_input, platform, false).await
}

async fn fixture_full_audit(
    stream_timeout: bool,
    block_input: bool,
    platform: bool,
    io_audit: bool,
) -> Fixture {
    fixture_full_audit_with_limit(
        stream_timeout,
        block_input,
        platform,
        io_audit,
        Some(1048576),
    )
    .await
}

async fn fixture_full_audit_with_limit(
    stream_timeout: bool,
    block_input: bool,
    platform: bool,
    io_audit: bool,
    body_limit: Option<usize>,
) -> Fixture {
    let (sent, upstream_calls) = mpsc::unbounded_channel();
    let upstream_messages = Arc::new(Mutex::new(Vec::new()));
    let captured = io_audit.then(|| upstream_messages.clone());
    let upstream_app = axum::Router::new().route("/v1/responses", axum::routing::get(move |ws: axum::extract::ws::WebSocketUpgrade| {
        let sent = sent.clone();
        let captured = captured.clone();
        async move { ws.on_upgrade(move |socket| async move {
            let (mut writer, mut reader) = socket.split();
            let (output, mut receive) = mpsc::channel::<String>(64);
            let mut turns = std::collections::HashMap::<String, usize>::new();
            let mut cached = std::collections::HashSet::<String>::new();
            let mut responses = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    message = reader.next() => {
                        let Some(Ok(axum::extract::ws::Message::Text(text))) = message else { break; };
                        if let Some(captured) = &captured {
                            captured.lock().unwrap().push(CapturedUpstreamMessage::Request(text.clone()));
                        }
                        let event: Value = serde_json::from_str(&text).unwrap();
                        sent.send(event.clone()).unwrap();
                        let lane = event.get("stream_id").and_then(Value::as_str).unwrap_or("default").to_owned();
                        let input = event.get("input").and_then(Value::as_str);
                        if input == Some("close_before_created") { break; }
                        if input == Some("malformed") {
                            if send_upstream_message(&mut writer, "{invalid".into(), &captured).await.is_err() { break; }
                            continue;
                        }
                        if input == Some("error_before_created") || event.get("previous_response_id").and_then(Value::as_str).is_some_and(|parent| !cached.contains(parent)) {
                            let code = if input == Some("error_before_created") { "server_error" } else { "previous_response_not_found" };
                            let mut error = json!({"type":"error","error":{"type":"server_error","code":code,"message":code}});
                            if lane != "default" { error["stream_id"] = json!(lane); }
                            if send_upstream_message(&mut writer, error.to_string(), &captured).await.is_err() { break; }
                            continue;
                        }
                        let turn = turns.entry(lane.clone()).or_default();
                        *turn += 1;
                        let turn = *turn;
                        let id = format!("resp_{lane}_{turn}");
                        cached.insert(id.clone());
                        let mut created = json!({"type":"response.created","sequence_number":0,"response":{"id":id,"object":"response","status":"in_progress","model":"provider-model","output":[]}});
                        if lane != "default" { created["stream_id"] = json!(lane); }
                        if send_upstream_message(&mut writer, created.to_string(), &captured).await.is_err() { break; }
                        if input == Some("hang") { continue; }
                        let output = output.clone();
                        responses.spawn(async move {
                            if matches!(event.get("input").and_then(Value::as_str), Some("slow" | "slow_text")) { tokio::time::sleep(std::time::Duration::from_millis(250)).await; }
                            if event.get("input").and_then(Value::as_str) == Some("flood") {
                                let text = "x".repeat(128 * 1024);
                                for sequence in 1..1025 {
                                    let mut delta = json!({"type":"response.output_text.delta","sequence_number":sequence,"response_id":id,"output_index":0,"content_index":0,"delta":text});
                                    if lane != "default" { delta["stream_id"] = json!(lane); }
                                    if output.send(delta.to_string()).await.is_err() { return; }
                                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                                }
                                return;
                            }
                            let warmup = event.get("generate").and_then(Value::as_bool) == Some(false);
                            let text_only = event.get("input").and_then(Value::as_str) == Some("slow_text");
                            let items = if warmup { json!([]) } else if turn == 1 && !text_only { json!([{"type":"function_call","id":"fc_native","call_id":"call_native","name":"lookup","arguments":"{}","status":"completed"}]) } else { json!([{"type":"message","id":"msg_native","role":"assistant","status":"completed","content":[{"type":"output_text","text":"tool result used","annotations":[]}]}]) };
                            let usage = if warmup { json!({"input_tokens":0,"output_tokens":0,"total_tokens":0}) } else { json!({"input_tokens":7,"output_tokens":3,"total_tokens":10}) };
                            let mut completed = json!({"type":"response.completed","sequence_number":1,"response":{"id":id,"object":"response","status":"completed","model":"provider-model","output":items,"usage":usage}});
                            if lane != "default" { completed["stream_id"] = json!(lane); }
                            let _ = output.send(completed.to_string()).await;
                        });
                    }
                    message = receive.recv() => {
                        let Some(message) = message else { break; };
                        if send_upstream_message(&mut writer, message, &captured).await.is_err() { break; }
                    }
                }
            }
            responses.abort_all();
            while responses.join_next().await.is_some() {}
        }) }
    }));
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app).await.unwrap();
    });
    let snapshot = AisixSnapshot::new();
    let provider = serde_json::from_value(json!({"display_name":"native","provider":"openai","api_key":"private-upstream-key","api_base":format!("http://{upstream_addr}"),"adapter":"openai"})).unwrap();
    snapshot
        .provider_keys
        .insert(aisix_core::ResourceEntry::new(PROVIDER, provider, 1));
    let model = serde_json::from_value(json!({"display_name":"native-model","provider":"openai","model_name":"provider-model","provider_key_id":PROVIDER,"stream_timeout":if stream_timeout { 1000 } else { 0 }})).unwrap();
    snapshot
        .models
        .insert(aisix_core::ResourceEntry::new(MODEL, model, 1));
    let key = serde_json::from_value(json!({"key_hash":aisix_core::ApiKey::hash_bearer("caller"),"allowed_models":["native-model"]})).unwrap();
    snapshot
        .apikeys
        .insert(aisix_core::ResourceEntry::new(KEY, key, 1));
    if block_input {
        let guardrail = serde_json::from_value(json!({"name":"block-everything","enabled":true,"kind":"custom","hook_point":"input","fail_open":false,"script":"export function checkInput() { return { action: 'block', reason_code: 'blocked' }; }","timeout_ms":5000})).unwrap();
        snapshot
            .guardrails
            .insert(aisix_core::ResourceEntry::new("guardrail-ws", guardrail, 1));
        let attachment = serde_json::from_value(
            json!({"guardrail_id":"guardrail-ws","scope_type":"env","priority":100}),
        )
        .unwrap();
        snapshot
            .guardrail_attachments
            .insert(aisix_core::ResourceEntry::new(
                "attachment-ws",
                attachment,
                1,
            ));
    }
    let snapshot = SnapshotHandle::new(snapshot);
    let mut config = json!({"addr":"127.0.0.1:0"});
    if let Some(limit) = body_limit {
        config["request_body_limit_bytes"] = json!(limit);
    }
    let config: ProxyConfig = serde_json::from_value(config).unwrap();
    let (tx, usage) = mpsc::channel(32);
    let (audit_tx, audit_updates) = mpsc::channel(32);
    let sink = aisix_obs::UsageSink::new(tx);
    let sink = if io_audit {
        sink.with_io_audit_sender(audit_tx)
    } else {
        sink
    };
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let index = aisix_guardrails::LiveGuardrailIndex::new(snapshot.clone(), None);
    let mut state = ProxyState::new(snapshot.clone(), hub, &config)
        .without_cache()
        .with_guardrail_index(index)
        .with_usage_sink(sink);
    if platform {
        state = state
            .with_platform(aisix_proxy::platform::PlatformSettings::from_secret(
                "websocket-test-platform",
            ))
            .with_native_objects(Arc::new(Registry::default()));
    }
    let livez = state.livez.clone();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Fixture {
        address,
        snapshot,
        upstream_calls,
        upstream_messages,
        usage,
        audit_updates,
        livez,
        proxy,
        upstream,
    }
}

async fn connect(f: &Fixture) -> Client {
    connect_with_bearer(f, "caller").await
}

async fn connect_with_bearer(f: &Fixture, bearer: &str) -> Client {
    let mut request = format!("ws://{}/v1/responses", f.address)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    tokio_tungstenite::connect_async(request)
        .await
        .expect("real HTTP upgrade")
        .0
}

async fn event(socket: &mut Client) -> Value {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("bounded receive")
            .expect("socket open")
            .unwrap();
        if let ClientMessage::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn completed(socket: &mut Client) -> Value {
    loop {
        let value = event(socket).await;
        assert_ne!(value["type"], "error", "{value}");
        if value["type"] == "response.completed" {
            return value;
        }
    }
}

#[tokio::test]
async fn websocket_default_body_limit_accepts_ping_and_a_response_round() {
    for body_limit in [None, Some(0)] {
        let mut f = fixture_full_audit_with_limit(false, false, false, false, body_limit).await;
        let mut socket = connect(&f).await;
        let nonce = Uuid::new_v4().as_bytes().to_vec();
        socket
            .send(ClientMessage::Ping(nonce.clone()))
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("默认配置应及时回复 Ping")
            .expect("默认配置不应断开连接")
            .expect("默认配置应接受非空控制帧");
        assert_eq!(reply, ClientMessage::Pong(nonce));
        assert!(f.upstream_calls.try_recv().is_err(), "Ping 不得触发上游");
        socket
            .send(ClientMessage::Text(
                json!({"type":"response.create","model":"native-model","input":"prepare","generate":false})
                    .to_string(),
            ))
            .await
            .unwrap();
        let response = completed(&mut socket).await;
        assert_eq!(response["response"]["status"], "completed");
        assert_eq!(response["response"]["model"], "native-model");
        let sent = f.upstream_calls.recv().await.unwrap();
        assert_eq!(sent["type"], "response.create");
        assert_eq!(sent["model"], "provider-model");
        socket.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn websocket_finite_body_limit_still_caps_frames_and_fragmented_messages() {
    use tokio_tungstenite::tungstenite::protocol::frame::{
        coding::{Data, OpCode},
        Frame,
    };
    for fragmented in [false, true] {
        let mut f = fixture_full_audit_with_limit(false, false, false, false, Some(256)).await;
        let mut socket = connect(&f).await;
        let nonce = Uuid::new_v4().as_bytes().to_vec();
        socket
            .send(ClientMessage::Ping(nonce.clone()))
            .await
            .unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Pong(nonce)
        );
        let payload =
            json!({"type":"response.create","model":"native-model","input":"x".repeat(300)})
                .to_string();
        assert!(payload.len() > 256);
        if fragmented {
            let split = payload.len() / 2;
            assert!(split < 256 && payload.len() - split < 256);
            socket
                .send(ClientMessage::Frame(Frame::message(
                    payload.as_bytes()[..split].to_vec(),
                    OpCode::Data(Data::Text),
                    false,
                )))
                .await
                .unwrap();
            socket
                .send(ClientMessage::Frame(Frame::message(
                    payload.as_bytes()[split..].to_vec(),
                    OpCode::Data(Data::Continue),
                    true,
                )))
                .await
                .unwrap();
        } else {
            socket.send(ClientMessage::Text(payload)).await.unwrap();
        }
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("超限帧或消息应终止连接");
        assert!(
            matches!(
                reply,
                None | Some(Err(_)) | Some(Ok(ClientMessage::Close(_)))
            ),
            "超限请求不得进入应用分派：{reply:?}"
        );
        assert!(f.upstream_calls.try_recv().is_err(), "超限请求不得触发上游");
    }
}

#[tokio::test]
async fn websocket_default_body_limit_keeps_the_protocol_frame_cap() {
    let mut f = fixture_full_audit_with_limit(false, false, false, false, None).await;
    let mut socket = connect(&f).await;
    let oversized = "x".repeat((16 << 20) + 1);
    // 客户端可能在服务端读到超限帧头并关闭后才完成发送。
    let _ = socket.send(ClientMessage::Text(oversized)).await;
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("缺省协议帧上限应及时生效");
    assert!(
        matches!(
            reply,
            None | Some(Err(_)) | Some(Ok(ClientMessage::Close(_)))
        ),
        "HTTP 无限正文不能取消协议帧上限：{reply:?}"
    );
    assert!(
        f.upstream_calls.try_recv().is_err(),
        "超限协议帧不得触发上游"
    );
}

#[tokio::test]
async fn io_audit_websocket_concurrent_rounds_have_separate_text_without_http_sizes() {
    let mut f = fixture_full_audit(false, false, false, true).await;
    let mut socket = connect(&f).await;
    for (lane, input) in [("unicode-audit", "A汉😀"), ("ascii-audit", "x")] {
        socket.send(ClientMessage::Text(json!({
            "type":"response.create", "model":"native-model", "stream_id":lane, "input":input
        }).to_string())).await.unwrap();
    }
    for _ in 0..2 {
        let response = completed(&mut socket).await;
        assert_eq!(response["response"]["output"][0]["arguments"], "{}");
    }
    // 期望字节只来自测试上游实际收发的 WS 字符串，不调用产品序列化器或内部 SSE 桥。
    let captured = f.upstream_messages.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        6,
        "two creates and two raw events per round"
    );
    let mut sent_by_lane = HashMap::new();
    let mut received_by_lane = HashMap::<String, (String, u64, usize)>::new();
    for message in captured {
        match message {
            CapturedUpstreamMessage::Request(payload) => {
                let request: Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(request["type"], "response.create");
                assert_eq!(request["model"], "provider-model");
                assert!(request.get("stream").is_none());
                let lane = request["stream_id"].as_str().unwrap().to_owned();
                let (expected_input, expected_text) = match lane.as_str() {
                    "unicode-audit" => ("A汉😀", (8, 3)),
                    "ascii-audit" => ("x", (1, 1)),
                    _ => panic!("unexpected upstream lane {lane}"),
                };
                assert_eq!(request["input"], expected_input);
                assert!(sent_by_lane
                    .insert(lane, (payload.len() as u64, expected_text))
                    .is_none());
            }
            CapturedUpstreamMessage::Response(payload) => {
                let response: Value = serde_json::from_str(&payload).unwrap();
                assert!(matches!(
                    response["type"].as_str(),
                    Some("response.created" | "response.completed")
                ));
                let lane = response["stream_id"].as_str().unwrap().to_owned();
                let response_id = response["response"]["id"].as_str().unwrap().to_owned();
                let entry = received_by_lane
                    .entry(lane)
                    .or_insert((response_id.clone(), 0, 0));
                assert_eq!(entry.0, response_id);
                entry.1 += payload.len() as u64;
                entry.2 += 1;
            }
        }
    }
    assert_eq!(sent_by_lane.len(), 2);
    assert_eq!(received_by_lane.len(), 2);
    let mut observed = Vec::new();
    let mut requests = Vec::new();
    let mut originals = std::collections::HashSet::new();
    for _ in 0..2 {
        let row = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
            .await
            .unwrap()
            .unwrap();
        requests.push(row.request_id.clone());
        assert!(row.is_final_attempt);
        assert!(
            originals.insert(row.event_id.clone()),
            "one original per round"
        );
        assert_eq!(row.api_key_id, KEY);
        assert_eq!((row.prompt_tokens, row.completion_tokens), (7, 3));
        let lane = received_by_lane
            .iter()
            .find_map(|(lane, (id, _, _))| (id == &row.provider_request_id).then(|| lane.clone()))
            .expect("the original row identifies its own upstream response");
        let (request_bytes, expected_text) = sent_by_lane.remove(&lane).unwrap();
        let (_, response_bytes, response_events) = received_by_lane.remove(&lane).unwrap();
        assert_eq!(response_events, 2);
        let audit = row
            .io_audit
            .expect("v2 upstream counts are on the original usage event");
        assert_eq!(audit.version, 2);
        assert_eq!(audit.boundary, "upstream_attempt");
        assert_eq!(audit.transport.as_deref(), Some("websocket_payload"));
        assert_eq!(audit.exchange_count, Some(1));
        assert_eq!(
            audit.request_body_bytes,
            Some(request_bytes),
            "actual upstream WS create payload"
        );
        assert_eq!(
            audit.response_body_bytes,
            Some(response_bytes),
            "raw upstream WS events without synthetic SSE framing"
        );
        assert_eq!(audit.request_complete, Some(true));
        assert!(audit.response_complete);
        let input = audit.input_text.expect("each round has its own input");
        assert_eq!((input.utf8_bytes, input.unicode_scalars), expected_text);
        observed.push((input.utf8_bytes, input.unicode_scalars));
        assert!(input.complete);
        let output = audit
            .output_text
            .expect("generated tool arguments are observed");
        assert_eq!((output.utf8_bytes, output.unicode_scalars), (2, 2));
        assert!(output.complete);
    }
    observed.sort_unstable();
    assert_eq!(observed, vec![(1, 1), (8, 3)]);
    assert_eq!(originals.len(), 2);
    assert!(sent_by_lane.is_empty());
    assert!(received_by_lane.is_empty());
    assert_ne!(requests[0], requests[1]);
    assert!(f.usage.try_recv().is_err(), "no duplicate round rows");
    assert!(
        f.audit_updates.try_recv().is_err(),
        "v2 must not require or publish a metadata sidecar"
    );
}

#[tokio::test]
async fn io_audit_websocket_cancel_has_no_synthetic_sizes_and_one_partial_row() {
    let mut f = fixture_full_audit(false, false, false, true).await;
    let mut socket = connect(&f).await;
    socket
        .send(ClientMessage::Text(
            json!({
                "type":"response.create", "model":"native-model", "input":"hang"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(event(&mut socket).await["type"], "response.created");
    f.upstream_calls.recv().await.unwrap();
    socket.close(None).await.unwrap();
    drop(socket);
    let row = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status_code, 499);
    assert!(row.is_final_attempt);
    assert_eq!(row.api_key_id, KEY);
    let captured = f.upstream_messages.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        2,
        "one actual create and one upstream created prefix"
    );
    let mut request_payload = None;
    let mut response_payload = None;
    for message in captured {
        match message {
            CapturedUpstreamMessage::Request(payload) => {
                let request: Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(request["type"], "response.create");
                assert_eq!(request["model"], "provider-model");
                assert_eq!(request["input"], "hang");
                assert!(request.get("stream").is_none());
                assert!(request_payload.replace(payload).is_none());
            }
            CapturedUpstreamMessage::Response(payload) => {
                let response: Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(response["type"], "response.created");
                assert!(response_payload.replace(payload).is_none());
            }
        }
    }
    let audit = row
        .io_audit
        .expect("cancelled original row carries its upstream prefix");
    assert_eq!(audit.version, 2);
    assert_eq!(audit.boundary, "upstream_attempt");
    assert_eq!(audit.transport.as_deref(), Some("websocket_payload"));
    assert_eq!(audit.exchange_count, Some(1));
    assert_eq!(
        audit.request_body_bytes,
        Some(request_payload.unwrap().len() as u64)
    );
    assert_eq!(
        audit.response_body_bytes,
        Some(response_payload.unwrap().len() as u64)
    );
    assert_eq!(audit.request_complete, Some(true));
    assert!(!audit.response_complete);
    let input = audit
        .input_text
        .expect("actual upstream input is retained as counts");
    assert_eq!((input.utf8_bytes, input.unicode_scalars), (4, 4));
    assert!(input.complete);
    assert!(
        audit.output_text.is_none(),
        "created metadata is not generated text"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), f.usage.recv())
            .await
            .is_err()
    );
    assert!(
        f.audit_updates.try_recv().is_err(),
        "v2 prefix is attached once to the original row without a metadata sidecar"
    );
}

#[tokio::test]
async fn websocket_native_tool_continuation_keeps_connection_and_one_final_per_response() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","input":"use lookup","stream_id":"tools","store":false,"service_tier":"fast","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}).to_string())).await.unwrap();
    let first = completed(&mut socket).await;
    assert_eq!(first["stream_id"], "tools");
    assert_eq!(first["response"]["usage"]["total_tokens"], 10);
    let call = &first["response"]["output"][0];
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"tools","previous_response_id":first["response"]["id"],"input":[{"type":"function_call_output","call_id":call["call_id"],"output":"saved tool result"}]}).to_string())).await.unwrap();
    let second = completed(&mut socket).await;
    assert_eq!(
        second["response"]["output"][0]["content"][0]["text"],
        "tool result used"
    );
    let sent_first = f.upstream_calls.recv().await.unwrap();
    let sent_second = f.upstream_calls.recv().await.unwrap();
    assert_eq!(sent_first["model"], "provider-model");
    assert!(sent_first.get("stream").is_none());
    assert_eq!(sent_first["service_tier"], "fast");
    assert_eq!(sent_first["store"], false);
    assert_eq!(sent_second["previous_response_id"], "resp_tools_1");
    assert_eq!(sent_second["input"][0]["output"], "saved tool result");
    let a = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(a.request_id, b.request_id);
    assert!(a.is_final_attempt && b.is_final_attempt);
    assert_eq!(
        (a.prompt_tokens, a.completion_tokens, a.total_tokens),
        (7, 3, 10)
    );
    assert_eq!(
        (b.prompt_tokens, b.completion_tokens, b.total_tokens),
        (7, 3, 10)
    );
    assert!(
        f.usage.try_recv().is_err(),
        "each response emits exactly one final"
    );
}

#[tokio::test]
async fn websocket_lanes_are_concurrent_and_same_lane_is_fifo() {
    let f = fixture().await;
    let mut socket = connect(&f).await;
    for (lane, input) in [("slow", "slow"), ("slow", "queued"), ("fast", "quick")] {
        socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":lane,"input":input}).to_string())).await.unwrap();
    }
    let first = completed(&mut socket).await;
    assert_eq!(
        first["stream_id"], "fast",
        "separate lanes cannot wait behind a slow response"
    );
    let second = completed(&mut socket).await;
    let third = completed(&mut socket).await;
    assert_eq!(second["stream_id"], "slow");
    assert_eq!(third["stream_id"], "slow");
    assert!(second["response"]["id"]
        .as_str()
        .unwrap()
        .contains("slow_1"));
    assert!(third["response"]["id"].as_str().unwrap().contains("slow_2"));
}

#[tokio::test]
async fn websocket_rechecks_revocation_between_rounds() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"one"}).to_string(),
        ))
        .await
        .unwrap();
    completed(&mut socket).await;
    f.upstream_calls.recv().await.unwrap();
    f.snapshot.load().apikeys.remove(KEY);
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"two"}).to_string(),
        ))
        .await
        .unwrap();
    let rejected = event(&mut socket).await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["status"], 401);
    assert!(f.upstream_calls.try_recv().is_err());
}

#[tokio::test]
async fn websocket_invalid_events_recover_without_dispatch() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    for invalid in [
        "not JSON".to_owned(),
        json!({"type":"response.create","model":"native-model","stream":true}).to_string(),
        json!({"type":"response.create","model":"native-model","background":true}).to_string(),
        json!({"type":"response.create","model":"native-model","stream_id":"invalid lane"})
            .to_string(),
    ] {
        socket.send(ClientMessage::Text(invalid)).await.unwrap();
        let rejected = event(&mut socket).await;
        assert_eq!(rejected["type"], "error");
        assert_eq!(rejected["status"], 400);
        assert_eq!(rejected["error"]["type"], "invalid_request_error");
        assert!(f.upstream_calls.try_recv().is_err());
    }
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"valid"}).to_string(),
        ))
        .await
        .unwrap();
    completed(&mut socket).await;
    assert_eq!(f.upstream_calls.recv().await.unwrap()["input"], "valid");
}

#[tokio::test]
async fn websocket_model_acl_rechecks_between_rounds() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    let create =
        json!({"type":"response.create","model":"native-model","input":"valid"}).to_string();
    socket
        .send(ClientMessage::Text(create.clone()))
        .await
        .unwrap();
    completed(&mut socket).await;
    f.upstream_calls.recv().await.unwrap();
    let key = serde_json::from_value(
        json!({"key_hash":aisix_core::ApiKey::hash_bearer("caller"),"allowed_model_ids":[]}),
    )
    .unwrap();
    f.snapshot
        .load()
        .apikeys
        .insert(aisix_core::ResourceEntry::new(KEY, key, 2));
    socket.send(ClientMessage::Text(create)).await.unwrap();
    let rejected = event(&mut socket).await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["status"], 403);
    assert!(
        f.upstream_calls.try_recv().is_err(),
        "a revoked model grant must not dispatch"
    );
}

#[tokio::test]
async fn websocket_empty_input_still_runs_unconditional_guardrail() {
    let mut f = fixture_with_options(false, true).await;
    let mut socket = connect(&f).await;
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":[]}).to_string(),
        ))
        .await
        .unwrap();
    let rejected = event(&mut socket).await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["status"], 422);
    assert!(
        f.upstream_calls.try_recv().is_err(),
        "guardrail must run before dispatch"
    );
    let usage = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(usage.guardrail_blocked);
    assert!(usage.is_final_attempt);
}

#[tokio::test]
async fn websocket_peer_normal_close_is_acknowledged_and_releases_active_rounds() {
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
    for active in [false, true] {
        let mut f = fixture_full_audit_with_limit(false, false, false, false, None).await;
        let mut socket = connect(&f).await;
        if active {
            socket
                .send(ClientMessage::Text(
                    json!({"type":"response.create","model":"native-model","input":"hang"})
                        .to_string(),
                ))
                .await
                .unwrap();
            assert_eq!(event(&mut socket).await["type"], "response.created");
            f.upstream_calls.recv().await.unwrap();
        }
        let close = CloseFrame {
            code: CloseCode::Normal,
            reason: format!("complete-{}", Uuid::new_v4()).into(),
        };
        socket
            .send(ClientMessage::Close(Some(close.clone())))
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("正常关闭应及时回复 Close")
            .expect("正常关闭应有回复帧")
            .expect("正常关闭不能直接断开 TCP");
        assert_eq!(reply, ClientMessage::Close(Some(close)));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while f.livez.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("正常关闭必须释放 session 与轮次排空守卫");
        if active {
            let usage = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
                .await
                .expect("活动轮取消必须及时计量")
                .unwrap();
            assert_eq!(usage.status_code, 499);
            assert_eq!(usage.error_class, "client_disconnected");
            assert!(usage.is_final_attempt);
        }
        assert!(f.upstream_calls.try_recv().is_err());
        assert!(
            f.usage.try_recv().is_err(),
            "关闭不得重复计量或制造空会话用量"
        );
    }
}

#[tokio::test]
async fn websocket_close_cancels_active_round_and_emits_one_final() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"hang"}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(event(&mut socket).await["type"], "response.created");
    f.upstream_calls.recv().await.unwrap();
    socket.close(None).await.unwrap();
    drop(socket);
    let usage = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.status_code, 499);
    assert_eq!(usage.error_class, "client_disconnected");
    assert!(usage.is_final_attempt);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), f.usage.recv())
            .await
            .is_err(),
        "disconnect must not emit a second final"
    );
}

#[tokio::test]
async fn websocket_never_replays_a_create_after_uncertain_delivery_or_first_error() {
    for input in ["close_before_created", "error_before_created"] {
        let mut f = fixture_with_options(true, false).await;
        let mut socket = connect(&f).await;
        socket
            .send(ClientMessage::Text(
                json!({"type":"response.create","model":"native-model","input":input}).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(event(&mut socket).await["type"], "error");
        assert_eq!(f.upstream_calls.recv().await.unwrap()["input"], input);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(250),
                f.upstream_calls.recv()
            )
            .await
            .is_err(),
            "a sent create must not be replayed"
        );
        let usage = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(usage.is_final_attempt);
        assert_ne!(usage.status_code, 200);
        assert!(f.usage.try_recv().is_err());
    }
}

async fn provider_close_scenario(active: bool) {
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};

    let mut f = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut provider_key = f
        .snapshot
        .load()
        .provider_keys
        .get_by_id(PROVIDER)
        .unwrap()
        .value
        .as_ref()
        .clone();
    provider_key.api_base = Some(format!("http://{}", listener.local_addr().unwrap()));
    f.snapshot
        .load()
        .provider_keys
        .insert(aisix_core::ResourceEntry::new(PROVIDER, provider_key, 2));
    let mut socket = connect(&f).await;
    let lanes = if active { &['a', 'b'][..] } else { &['a'][..] };
    for lane in lanes {
        socket
            .send(ClientMessage::Text(
                json!({"type":"response.create","model":"native-model","stream_id":lane.to_string(),"input":"before close"}).to_string(),
            ))
            .await
            .unwrap();
    }
    let (tcp, _) = tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
        .await
        .expect("first provider connection")
        .unwrap();
    let mut provider = tokio_tungstenite::accept_async(tcp).await.unwrap();
    for _ in lanes {
        let create = provider.next().await.unwrap().unwrap();
        let create: Value = serde_json::from_str(create.to_text().unwrap()).unwrap();
        assert_eq!(create["input"], "before close");
        let lane = create["stream_id"].as_str().unwrap();
        provider
            .send(ClientMessage::Text(
                json!({"type":"response.created","stream_id":lane,"response":{"id":format!("resp_before_{lane}"),"object":"response","status":"in_progress","output":[]}}).to_string(),
            ))
            .await
            .unwrap();
        if !active {
            provider
                .send(ClientMessage::Text(
                    json!({"type":"response.completed","stream_id":lane,"response":{"id":format!("resp_before_{lane}"),"object":"response","status":"completed","output":[],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}).to_string(),
                ))
                .await
                .unwrap();
        }
    }
    for _ in lanes {
        if active {
            assert_eq!(event(&mut socket).await["type"], "response.created");
        } else {
            completed(&mut socket).await;
        }
    }
    let close = Some(CloseFrame {
        code: CloseCode::Away,
        reason: "provider maintenance".into(),
    });
    provider
        .send(ClientMessage::Close(close.clone()))
        .await
        .unwrap();
    let acknowledged = tokio::time::timeout(std::time::Duration::from_secs(2), provider.next())
        .await
        .expect("provider Close must be acknowledged promptly")
        .expect("provider must receive a Close before disconnect")
        .expect("provider must receive a clean closing handshake");
    assert_eq!(acknowledged, ClientMessage::Close(close));
    if active {
        let mut failed_lanes = std::collections::HashSet::new();
        for _ in lanes {
            let failed = event(&mut socket).await;
            assert_eq!(failed["type"], "error");
            assert_eq!(failed["error"]["code"], "upstream_stream_error");
            assert!(failed_lanes.insert(failed["stream_id"].as_str().unwrap().to_owned()));
        }
        assert_eq!(failed_lanes, ["a".to_owned(), "b".to_owned()].into());
    }
    let mut settled_requests = std::collections::HashSet::new();
    for _ in lanes {
        let row = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(row.is_final_attempt);
        assert_eq!(row.status_code == 200, !active);
        assert!(settled_requests.insert(row.request_id));
    }
    // 同一客户端在 provider 关闭后继续发起新轮，旧 create 不能重派到新连接。
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","stream_id":"a","input":"after close"}).to_string(),
        ))
        .await
        .unwrap();
    let (tcp, _) = tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
        .await
        .expect("next create must open a fresh provider connection")
        .unwrap();
    let mut next_provider = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let create = next_provider.next().await.unwrap().unwrap();
    let create: Value = serde_json::from_str(create.to_text().unwrap()).unwrap();
    assert_eq!(create["input"], "after close");
    assert_eq!(create["stream_id"], "a");
    for (kind, status) in [
        ("response.created", "in_progress"),
        ("response.completed", "completed"),
    ] {
        next_provider
            .send(ClientMessage::Text(
                json!({"type":kind,"stream_id":"a","response":{"id":"resp_after_close","object":"response","status":status,"output":[],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}).to_string(),
            ))
            .await
            .unwrap();
    }
    assert_eq!(
        completed(&mut socket).await["response"]["id"],
        "resp_after_close"
    );
    let row = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(row.is_final_attempt);
    assert_eq!(row.status_code, 200);
    assert!(settled_requests.insert(row.request_id));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), f.usage.recv())
            .await
            .is_err(),
        "each accepted round must settle once"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "sent creates must not be replayed"
    );
}

#[tokio::test]
async fn websocket_provider_close_idle_acknowledges_exact_frame_and_reconnects() {
    provider_close_scenario(false).await;
}

#[tokio::test]
async fn websocket_provider_close_active_acknowledges_and_settles_each_round_once() {
    provider_close_scenario(true).await;
}

#[tokio::test]
async fn websocket_uncommitted_steering_returns_input_and_known_lane_without_dispatch() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"steer-target","input":"hang","conversation":"conv_no_steering"}).to_string())).await.unwrap();
    let created = event(&mut socket).await;
    assert_eq!(created["type"], "response.created");
    f.upstream_calls.recv().await.unwrap();
    let input = json!([{"type":"message","role":"user","content":"keep this input for manual continuation"}]);
    socket.send(ClientMessage::Text(json!({"type":"response.steer","previous_response_id":created["response"]["id"],"input":input}).to_string())).await.unwrap();
    let failed = event(&mut socket).await;
    assert_eq!(failed["type"], "response.steer.failed");
    assert_eq!(failed["stream_id"], "steer-target");
    assert_eq!(failed["error"]["code"], "steering_not_supported");
    assert_eq!(
        failed["steer"]["previous_response_id"],
        created["response"]["id"]
    );
    assert_eq!(failed["steer"]["input"], input);
    assert!(
        failed["steer"].get("id").is_none(),
        "unaccepted steering has no allocated ID"
    );
    assert!(f.upstream_calls.try_recv().is_err());
}

#[tokio::test]
async fn websocket_unknown_steering_target_does_not_invent_a_lane_or_accept_input() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket.send(ClientMessage::Text(json!({"type":"response.steer","previous_response_id":"resp_missing","input":"retained"}).to_string())).await.unwrap();
    let failed = event(&mut socket).await;
    assert_eq!(failed["type"], "response.steer.failed");
    assert_eq!(failed["error"]["code"], "response_not_found");
    assert_eq!(failed["steer"]["input"], "retained");
    assert!(failed.get("stream_id").is_none());
    assert!(failed["steer"].get("id").is_none());
    assert!(f.upstream_calls.try_recv().is_err());
}

#[tokio::test]
async fn websocket_generate_false_warmup_keeps_zero_usage_and_its_continuation_state() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"warmup","input":"prepare","generate":false}).to_string())).await.unwrap();
    let prepared = completed(&mut socket).await;
    assert_eq!(prepared["response"]["output"], json!([]));
    assert_eq!(prepared["response"]["usage"]["input_tokens"], 0);
    assert_eq!(prepared["response"]["usage"]["output_tokens"], 0);
    let sent = f.upstream_calls.recv().await.unwrap();
    assert_eq!(sent.get("generate"), Some(&Value::Bool(false)));
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"warmup","input":"answer","previous_response_id":prepared["response"]["id"]}).to_string())).await.unwrap();
    let answered = completed(&mut socket).await;
    assert_eq!(
        answered["response"]["output"][0]["content"][0]["text"],
        "tool result used"
    );
    assert_eq!(
        f.upstream_calls.recv().await.unwrap()["previous_response_id"],
        "resp_warmup_1"
    );
    let warmup = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            warmup.prompt_tokens,
            warmup.completion_tokens,
            warmup.total_tokens
        ),
        (0, 0, 0)
    );
    assert!(
        !warmup.usage_estimated,
        "reported zero warmup usage cannot become an estimate"
    );
    assert_eq!(
        (
            response.prompt_tokens,
            response.completion_tokens,
            response.total_tokens
        ),
        (7, 3, 10)
    );
    assert!(warmup.is_final_attempt && response.is_final_attempt);
    assert_ne!(warmup.request_id, response.request_id);
    assert!(f.usage.try_recv().is_err());
}

#[tokio::test]
async fn websocket_handshake_without_model_authenticates_before_dispatch() {
    let config = ProxyConfig {
        addr: "127.0.0.1:0".into(),
        request_body_limit_bytes: 1024 * 1024,
        real_ip: Default::default(),
        request_id: Default::default(),
        url_rewrites: Vec::new(),
        expose_debug_headers: false,
        platform: Default::default(),
        tls: None,
        listeners: Vec::new(),
        thread_per_core: None,
        workers: None,
    };
    let app = build_router(ProxyState::new(
        SnapshotHandle::new(AisixSnapshot::new()),
        Arc::new(Hub::new()),
        &config,
    ));
    let request = Request::builder()
        .uri("/v1/responses")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        error.get("error").is_some(),
        "authentication needs the OpenAI error envelope"
    );
}

#[tokio::test]
async fn websocket_store_false_forks_share_the_connection_cache_between_lanes() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"source","input":"prepare","store":false,"generate":false}).to_string())).await.unwrap();
    let parent = completed(&mut socket).await;
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"fork","previous_response_id":parent["response"]["id"],"input":"fork","store":false}).to_string())).await.unwrap();
    assert_eq!(completed(&mut socket).await["stream_id"], "fork");
    assert_eq!(
        f.upstream_calls.recv().await.unwrap()["stream_id"],
        "source"
    );
    assert_eq!(
        f.upstream_calls.recv().await.unwrap()["previous_response_id"],
        parent["response"]["id"]
    );
}

#[tokio::test]
async fn websocket_steering_waits_for_tools_then_prepends_input_once() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"tools","input":"slow"}).to_string())).await.unwrap();
    let parent = event(&mut socket).await;
    let input = json!([{"role":"user","content":"new instruction"}]);
    socket.send(ClientMessage::Text(json!({"type":"response.steer","previous_response_id":parent["response"]["id"],"input":input}).to_string())).await.unwrap();
    let accepted = event(&mut socket).await;
    assert_eq!(accepted["type"], "response.steer.accepted");
    assert!(accepted["steer"]["id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    completed(&mut socket).await;
    let pending = event(&mut socket).await;
    assert_eq!(pending["type"], "response.steer.pending");
    assert_eq!(pending["steer"]["id"], accepted["steer"]["id"]);
    assert_eq!(
        pending["required_input"],
        json!([{"type":"function_call_output","call_id":"call_native","name":"lookup"}])
    );
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"tools","previous_response_id":parent["response"]["id"],"temperature":0.2,"input":[{"type":"function_call_output","call_id":"call_native","output":"saved result"}]}).to_string())).await.unwrap();
    completed(&mut socket).await;
    f.upstream_calls.recv().await.unwrap();
    let successor = f.upstream_calls.recv().await.unwrap();
    assert_eq!(
        successor["input"],
        json!([{"role":"user","content":"new instruction"},{"type":"function_call_output","call_id":"call_native","output":"saved result"}])
    );
    assert_eq!(successor["temperature"], 0.2);
    assert!(f.upstream_calls.try_recv().is_err());
}

#[tokio::test]
async fn websocket_steering_automatic_successor_rechecks_revocation() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"slow_text"})
                .to_string(),
        ))
        .await
        .unwrap();
    let parent = event(&mut socket).await;
    socket.send(ClientMessage::Text(json!({"type":"response.steer","previous_response_id":parent["response"]["id"],"input":"retained"}).to_string())).await.unwrap();
    let accepted = event(&mut socket).await;
    assert_eq!(accepted["type"], "response.steer.accepted");
    f.snapshot.load().apikeys.remove(KEY);
    completed(&mut socket).await;
    let rejection = event(&mut socket).await;
    assert_eq!(rejection["type"], "error");
    assert_eq!(rejection["status"], 401);
    let failed = event(&mut socket).await;
    assert_eq!(failed["type"], "response.steer.failed");
    assert_eq!(failed["steer"]["id"], accepted["steer"]["id"]);
    assert_eq!(failed["steer"]["input"], "retained");
    assert_eq!(failed["error"]["code"], "successor_creation_failed");
    f.upstream_calls.recv().await.unwrap();
    assert!(f.upstream_calls.try_recv().is_err());
}

#[tokio::test]
async fn websocket_steering_automatic_successor_has_its_own_final_usage() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","input":"slow_text","service_tier":"fast"}).to_string())).await.unwrap();
    let parent = event(&mut socket).await;
    socket.send(ClientMessage::Text(json!({"type":"response.steer","previous_response_id":parent["response"]["id"],"input":"continue"}).to_string())).await.unwrap();
    assert_eq!(event(&mut socket).await["type"], "response.steer.accepted");
    completed(&mut socket).await;
    let successor = event(&mut socket).await;
    assert_eq!(successor["type"], "response.created");
    assert_ne!(successor["response"]["id"], parent["response"]["id"]);
    completed(&mut socket).await;
    f.upstream_calls.recv().await.unwrap();
    let sent = f.upstream_calls.recv().await.unwrap();
    assert_eq!(sent["previous_response_id"], parent["response"]["id"]);
    assert_eq!(sent["service_tier"], "fast");
    assert_eq!(
        sent["input"],
        json!([{"type":"message","role":"user","content":"continue"}])
    );
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.request_id, second.request_id);
    assert!(first.is_final_attempt && second.is_final_attempt);
    assert_eq!(
        (
            first.prompt_tokens,
            first.completion_tokens,
            second.prompt_tokens,
            second.completion_tokens
        ),
        (7, 3, 7, 3)
    );
    assert!(f.usage.try_recv().is_err());
}

#[tokio::test]
async fn websocket_malformed_upstream_event_fails_without_replaying_create() {
    let mut f = fixture_with_options(true, false).await;
    let mut socket = connect(&f).await;
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"malformed"})
                .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(event(&mut socket).await["type"], "error");
    f.upstream_calls.recv().await.unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(100),
        f.upstream_calls.recv()
    )
    .await
    .is_err());
}

#[tokio::test]
async fn websocket_named_lane_budget_excludes_default_and_keeps_reservations() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    for lane in 0..32 {
        socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":format!("lane-{lane}"),"input":"warmup","generate":false}).to_string())).await.unwrap();
        assert_eq!(
            completed(&mut socket).await["stream_id"],
            format!("lane-{lane}")
        );
        f.upstream_calls.recv().await.unwrap();
    }
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"lane-33","input":"over budget"}).to_string())).await.unwrap();
    let rejected = event(&mut socket).await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["stream_id"], "lane-33");
    assert!(f.upstream_calls.try_recv().is_err());
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","input":"default","generate":false}).to_string())).await.unwrap();
    assert!(completed(&mut socket).await.get("stream_id").is_none());
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"lane-0","input":"reuse","generate":false}).to_string())).await.unwrap();
    assert_eq!(completed(&mut socket).await["stream_id"], "lane-0");
}

#[tokio::test]
async fn websocket_active_budget_queues_seventeenth_and_close_finalizes_sixteen() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    for lane in 0..16 {
        socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":format!("active-{lane}"),"input":"hang"}).to_string())).await.unwrap();
        assert_eq!(event(&mut socket).await["type"], "response.created");
        f.upstream_calls.recv().await.unwrap();
    }
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"queued","input":"must wait"}).to_string())).await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            f.upstream_calls.recv()
        )
        .await
        .is_err(),
        "seventeenth active response dispatched before a slot was released"
    );
    socket.close(None).await.unwrap();
    drop(socket);
    let mut ids = std::collections::HashSet::new();
    for _ in 0..16 {
        let usage = tokio::time::timeout(std::time::Duration::from_secs(3), f.usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage.status_code, 499);
        assert!(usage.is_final_attempt);
        assert!(ids.insert(usage.request_id));
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), f.usage.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn websocket_lane_queue_overflow_recovers_and_does_not_block_other_lanes() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"blocked","input":"hang"}).to_string())).await.unwrap();
    assert_eq!(event(&mut socket).await["type"], "response.created");
    f.upstream_calls.recv().await.unwrap();
    for _ in 0..9 {
        socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"blocked","input":"queued"}).to_string())).await.unwrap();
    }
    let rejected = event(&mut socket).await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["stream_id"], "blocked");
    assert!(f.upstream_calls.try_recv().is_err());
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"free","input":"ready"}).to_string())).await.unwrap();
    assert_eq!(completed(&mut socket).await["stream_id"], "free");
    assert_eq!(f.upstream_calls.recv().await.unwrap()["stream_id"], "free");
}

#[tokio::test]
async fn websocket_response_cache_and_steering_targets_are_session_local() {
    let mut f = fixture().await;
    let mut owner = connect(&f).await;
    owner.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","input":"warmup","generate":false,"store":false}).to_string())).await.unwrap();
    let parent = completed(&mut owner).await;
    f.upstream_calls.recv().await.unwrap();
    let mut foreign = connect(&f).await;
    foreign.send(ClientMessage::Text(json!({"type":"response.steer","previous_response_id":parent["response"]["id"],"input":"foreign"}).to_string())).await.unwrap();
    let rejected = event(&mut foreign).await;
    assert_eq!(rejected["error"]["code"], "response_not_found");
    assert!(rejected["steer"].get("id").is_none());
    foreign.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","previous_response_id":parent["response"]["id"],"input":"foreign","store":false}).to_string())).await.unwrap();
    let rejected = event(&mut foreign).await;
    assert_eq!(rejected["type"], "error");
    assert!(!rejected.to_string().contains("private-upstream-key"));
}

#[tokio::test]
async fn websocket_automatic_successor_rechecks_model_acl_and_request_quota() {
    for quota in [false, true] {
        let mut f = fixture().await;
        if quota {
            let key = serde_json::from_value(json!({"key_hash":aisix_core::ApiKey::hash_bearer("caller"),"allowed_models":["native-model"],"rate_limit":{"rpd":1}})).unwrap();
            f.snapshot
                .load()
                .apikeys
                .insert(aisix_core::ResourceEntry::new(KEY, key, 2));
        }
        let mut socket = connect(&f).await;
        socket
            .send(ClientMessage::Text(
                json!({"type":"response.create","model":"native-model","input":"slow_text"})
                    .to_string(),
            ))
            .await
            .unwrap();
        let parent = event(&mut socket).await;
        socket.send(ClientMessage::Text(json!({"type":"response.steer","previous_response_id":parent["response"]["id"],"input":"continue"}).to_string())).await.unwrap();
        assert_eq!(event(&mut socket).await["type"], "response.steer.accepted");
        let key = serde_json::from_value(if quota {
            json!({"key_hash":aisix_core::ApiKey::hash_bearer("caller"),"allowed_models":["native-model"],"rate_limit":{"rpd":1}})
        } else {
            json!({"key_hash":aisix_core::ApiKey::hash_bearer("caller"),"allowed_model_ids":[]})
        }).unwrap();
        f.snapshot
            .load()
            .apikeys
            .insert(aisix_core::ResourceEntry::new(KEY, key, 2));
        completed(&mut socket).await;
        let rejected = event(&mut socket).await;
        assert_eq!(rejected["type"], "error");
        assert_eq!(rejected["status"], if quota { 429 } else { 403 });
        let failed = event(&mut socket).await;
        assert_eq!(failed["type"], "response.steer.failed");
        assert_eq!(failed["steer"]["input"], "continue");
        f.upstream_calls.recv().await.unwrap();
        assert!(f.upstream_calls.try_recv().is_err());
    }
}

#[tokio::test]
async fn websocket_platform_continuation_stays_owned_and_hides_upstream_details() {
    let mut f = fixture_full(false, false, true).await;
    let foreign_key = serde_json::from_value(json!({"key_hash":aisix_core::ApiKey::hash_bearer("foreign"),"allowed_models":["native-model"]})).unwrap();
    f.snapshot
        .load()
        .apikeys
        .insert(aisix_core::ResourceEntry::new(
            "33333333-3333-4333-8333-333333333334",
            foreign_key,
            1,
        ));
    let mut owner = connect(&f).await;
    owner.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"source","input":"warmup","generate":false,"store":false}).to_string())).await.unwrap();
    let parent = completed(&mut owner).await;
    assert_eq!(parent["response"]["model"], "native-model");
    assert!(!parent.to_string().contains("provider-model"));
    f.upstream_calls.recv().await.unwrap();
    owner.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","stream_id":"fork","previous_response_id":parent["response"]["id"],"input":"owned fork","store":false}).to_string())).await.unwrap();
    assert_eq!(completed(&mut owner).await["stream_id"], "fork");
    f.upstream_calls.recv().await.unwrap();
    let mut foreign = connect_with_bearer(&f, "foreign").await;
    foreign.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","previous_response_id":parent["response"]["id"],"input":"foreign"}).to_string())).await.unwrap();
    let denied = event(&mut foreign).await;
    assert_eq!(denied["type"], "error");
    assert_eq!(denied["status"], 400);
    assert_eq!(denied["error"]["code"], "previous_response_not_found");
    assert!(
        f.upstream_calls.try_recv().is_err(),
        "foreign native response must be rejected before upstream dispatch"
    );
    owner
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"error_before_created"})
                .to_string(),
        ))
        .await
        .unwrap();
    let error = event(&mut owner).await;
    assert_eq!(error["type"], "error");
    assert!(!error.to_string().contains("temporary failure"));
    assert!(!error.to_string().contains("private-upstream-key"));
    assert!(!error.to_string().contains("provider-model"));
}

fn attach_keyword(f: &Fixture, hook: &str, token: &str) {
    let rule = serde_json::from_value(json!({"name":"websocket-keyword","enabled":true,"kind":"keyword","hook_point":hook,"patterns":[{"kind":"literal","value":token}]})).unwrap();
    f.snapshot
        .load()
        .guardrails
        .insert(aisix_core::ResourceEntry::new(
            "guardrail-successor",
            rule,
            1,
        ));
    let attachment = serde_json::from_value(
        json!({"guardrail_id":"guardrail-successor","scope_type":"env","priority":100}),
    )
    .unwrap();
    f.snapshot
        .load()
        .guardrail_attachments
        .insert(aisix_core::ResourceEntry::new(
            "attachment-successor",
            attachment,
            1,
        ));
}

#[tokio::test]
async fn websocket_automatic_successor_input_and_output_are_guarded() {
    for hook in ["input", "output"] {
        let mut f = fixture().await;
        if hook == "input" {
            attach_keyword(&f, hook, "blocked-steering");
        }
        let mut socket = connect(&f).await;
        socket
            .send(ClientMessage::Text(
                json!({"type":"response.create","model":"native-model","input":"slow_text"})
                    .to_string(),
            ))
            .await
            .unwrap();
        let parent = event(&mut socket).await;
        socket.send(ClientMessage::Text(json!({"type":"response.steer","previous_response_id":parent["response"]["id"],"input":"blocked-steering"}).to_string())).await.unwrap();
        let accepted = event(&mut socket).await;
        assert_eq!(accepted["type"], "response.steer.accepted");
        if hook == "output" {
            attach_keyword(&f, hook, "tool result used");
        }
        completed(&mut socket).await;
        let mut successor_created = false;
        loop {
            let rejected = event(&mut socket).await;
            assert!(
                !rejected.to_string().contains("tool result used"),
                "blocked output reached the WebSocket caller"
            );
            if rejected["type"] == "response.created" {
                successor_created = true;
            }
            if rejected["type"] == "response.steer.failed" {
                assert_eq!(hook, "input");
                assert_eq!(rejected["steer"]["id"], accepted["steer"]["id"]);
                break;
            }
            if rejected["type"] == "response.failed" {
                assert_eq!(hook, "output");
                break;
            }
        }
        assert_eq!(successor_created, hook == "output");
        f.upstream_calls.recv().await.unwrap();
        if hook == "output" {
            f.upstream_calls.recv().await.unwrap();
        }
        assert!(f.upstream_calls.try_recv().is_err());
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!first.guardrail_blocked);
        assert!(second.guardrail_blocked);
        assert!(first.is_final_attempt && second.is_final_attempt);
        assert_ne!(first.request_id, second.request_id);
    }
}

#[tokio::test]
async fn websocket_dynamic_headers_cannot_grow_upstream_connections_without_bound() {
    let mut f = fixture().await;
    let provider = serde_json::from_value(json!({"display_name":"native","provider":"openai","api_key":"private-upstream-key","api_base":f.snapshot.load().provider_keys.get_by_id(PROVIDER).unwrap().value.api_base,"adapter":"openai","request":{"default_headers":{"x-correlation-id":"${request.id}"}}})).unwrap();
    f.snapshot
        .load()
        .provider_keys
        .insert(aisix_core::ResourceEntry::new(PROVIDER, provider, 2));
    let mut socket = connect(&f).await;
    for _ in 0..16 {
        socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","input":"prepare","generate":false}).to_string())).await.unwrap();
        completed(&mut socket).await;
        f.upstream_calls.recv().await.unwrap();
    }
    socket.send(ClientMessage::Text(json!({"type":"response.create","model":"native-model","input":"budget overflow","generate":false}).to_string())).await.unwrap();
    let rejected = event(&mut socket).await;
    assert_eq!(rejected["type"], "error");
    assert!(
        f.upstream_calls.try_recv().is_err(),
        "new header boundaries opened an unbounded seventeenth connection"
    );
}

#[tokio::test]
async fn websocket_shutdown_releases_active_round_and_session_drain_guards() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"hang"}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(event(&mut socket).await["type"], "response.created");
    assert!(f.livez.in_flight() > 0);
    f.livez.mark_shutting_down();
    let usage = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.status_code, 499);
    assert!(usage.is_final_attempt);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f.livez.in_flight() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(f.usage.try_recv().is_err());
}

#[tokio::test]
async fn websocket_shutdown_cancels_a_writer_blocked_by_a_nonreading_client() {
    let mut f = fixture().await;
    let mut socket = connect(&f).await;
    socket
        .send(ClientMessage::Text(
            json!({"type":"response.create","model":"native-model","input":"flood"}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(event(&mut socket).await["type"], "response.created");
    // 保持socket存活却不再读取，128MiB事件流填满真实TCP发送窗口。
    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
    assert!(f.livez.in_flight() > 0);
    f.livez.mark_shutting_down();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f.livez.in_flight() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let usage = tokio::time::timeout(std::time::Duration::from_secs(2), f.usage.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(usage.is_final_attempt);
    assert_ne!(usage.status_code, 200);
    assert!(f.usage.try_recv().is_err());
    drop(socket);
}
