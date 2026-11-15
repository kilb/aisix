//! WS 秒量合同：真实 CP/PG/kine/mTLS/GW；供应商仅为受控本地协议组件。
//! 官方 SDK 3dae6090 的 transcription.completed.usage.duration 与 whisper-1 配置，
//! 加上既有公开 AudioSeconds DTO 和已批准的 Unknown/Unpriced 合同决定断言。
//! 不创建秒价或 B token 价，不声称真实供应商处理、CP 二进制或电话端到端覆盖。

use super::*;
use aisix_core::native_metering::{
    AudioMeasurementBasis, MeasurementSource, ObservedMeter, ObservedUsage,
};
use aisix_core::native_objects::NativeEntry;

const DURATION_ALIAS: &str = "duration-asr-public-b";
const DURATION_MODEL: &str = "whisper-1";
const ITEM_FIRST: &str = "item_duration_first";
const ITEM_SECOND: &str = "item_duration_second";

struct DurationPeerState {
    secret: String,
    output: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    inputs: Mutex<Vec<Value>>,
    handshakes: Mutex<Vec<bool>>,
    applied: Mutex<Vec<Value>>,
}

struct DurationPeer {
    base: String,
    state: Arc<DurationPeerState>,
    task: tokio::task::JoinHandle<()>,
}

impl DurationPeer {
    async fn start(secret: String) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the controlled duration WS peer");
        let base = format!("http://{}", listener.local_addr().expect("peer address"));
        let state = Arc::new(DurationPeerState {
            secret,
            output: Mutex::new(None),
            inputs: Mutex::new(Vec::new()),
            handshakes: Mutex::new(Vec::new()),
            applied: Mutex::new(Vec::new()),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept actual provider WS");
            let observed = shared.clone();
            let handshake = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response| {
                    let authorized = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        == Some(format!("Bearer {}", observed.secret).as_str());
                    let project = request
                        .headers()
                        .get("openai-project")
                        .and_then(|value| value.to_str().ok())
                        == Some(PROJECT);
                    let route = request.uri().path() == "/v1/realtime"
                        && request.uri().query().is_some_and(|query| {
                            query.split('&').any(|field| field == "model=gpt-realtime")
                        });
                    observed
                        .handshakes
                        .lock()
                        .expect("handshake history")
                        .push(authorized && project && route);
                    Ok(response)
                },
            )
            .await;
            let mut socket =
                handshake.unwrap_or_else(|_| panic!("actual provider handshake failed"));
            let (sender, mut output) = mpsc::unbounded_channel();
            *shared.output.lock().expect("provider output") = Some(sender);
            let mut session = json!({"type":"realtime","object":"realtime.session","id":SESSION,
                "model":"gpt-realtime","output_modalities":["text"],"tools":[],
                "tool_choice":"auto","max_output_tokens":"inf",
                "expires_at":chrono::Utc::now().timestamp()+3600});
            assert!(
                socket
                    .send(Message::Text(
                        json!({"type":"session.created",
                "event_id":"duration_session_created","session":session})
                        .to_string()
                    ))
                    .await
                    .is_ok(),
                "send the actual provider session"
            );
            loop {
                tokio::select! {
                    message = output.recv() => {
                        let Some(message) = message else { break; };
                        if socket.send(message).await.is_err() { break; }
                    }
                    message = socket.next() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let event: Value = serde_json::from_str(&text)
                                    .expect("controlled provider input is JSON");
                                if event.get("type") == Some(&json!("session.update")) {
                                    let update = event.get("session").and_then(Value::as_object)
                                        .expect("an actual session configuration");
                                    for (key, value) in update { session[key] = value.clone(); }
                                    shared.applied.lock().expect("applied configuration history")
                                        .push(session.clone());
                                    // ACK 只来自供应商实际收到并应用的配置，不从请求别名反造。
                                    if socket.send(Message::Text(json!({"type":"session.updated",
                                        "event_id":"duration_configuration_applied","session":session})
                                        .to_string())).await.is_err() { break; }
                                }
                                shared.inputs.lock().expect("full provider input history").push(event);
                            }
                            Some(Ok(Message::Ping(bytes))) => {
                                if socket.send(Message::Pong(bytes)).await.is_err() { break; }
                            }
                            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                            _ => {}
                        }
                    }
                }
            }
        });
        Self { base, state, task }
    }

    fn send(&self, event: Value) {
        self.state
            .output
            .lock()
            .expect("actual peer output")
            .as_ref()
            .expect("the real provider handshake completed")
            .send(Message::Text(event.to_string()))
            .expect("release an official controlled provider event");
    }

    fn inputs(&self) -> Vec<Value> {
        self.state
            .inputs
            .lock()
            .expect("complete provider input history")
            .clone()
    }

    async fn received(&self, expected: &[&str]) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let events = self.inputs();
                if events.len() >= expected.len() {
                    assert!(
                        events
                            .iter()
                            .map(|event| event.get("type").and_then(Value::as_str))
                            .eq(expected.iter().copied().map(Some)),
                        "actual provider input types match the complete authorized prefix"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the actual provider reader receives each positive client input");
    }

    async fn drained(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .expect("the actual provider reader drains to transport close")
            .expect("the actual provider reader completes without a panic");
    }
}

impl Drop for DurationPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// 诊断只输出有限协议标签；不显示 HTTP/WS 私有正文、凭据或动态配置 URL。
fn assert_status(status: u16, expected: u16, body: &Value, phase: &str) {
    let code = match body.get("code").and_then(Value::as_str) {
        Some("validation_failed") => "validation_failed",
        Some("gateway_too_old") => "gateway_too_old",
        Some("unavailable") => "unavailable",
        Some("forbidden") => "forbidden",
        Some("not_found") => "not_found",
        _ => "other_or_absent",
    };
    if status != expected {
        let mut reported = false;
        for detail in body
            .get("details")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let path = match detail.get("path").and_then(Value::as_str) {
                Some("api_key") => "api_key",
                Some("api_base") => "api_base",
                Some("provider") => "provider",
                Some("adapter") => "adapter",
                Some("groups") => "groups",
                Some("default header OpenAI-Project") => "default header OpenAI-Project",
                Some("default header openai-project") => "default header openai-project",
                Some("/api_key") => "/api_key",
                Some("/api_base") => "/api_base",
                Some("/request/default_headers/OpenAI-Project") => {
                    "/request/default_headers/OpenAI-Project"
                }
                Some("/doc/request/default_headers/OpenAI-Project") => {
                    "/doc/request/default_headers/OpenAI-Project"
                }
                _ => "other_or_absent",
            };
            eprintln!(
                "duration setup diagnostic: phase={phase} status={status} code={code} path={path}"
            );
            reported = true;
        }
        if !reported {
            eprintln!(
                "duration setup diagnostic: phase={phase} status={status} code={code} path=absent"
            );
        }
    }
    assert_eq!(status, expected, "duration phase={phase} code={code}");
}

async fn setup_duration_model(f: &Fixture, peer: &DurationPeer) -> Uuid {
    let model_path = format!("/admin/resources/models/{}", f.model);
    let model =
        f.cp.send("GET", &model_path, Some(f.admin.cookie()), None)
            .await;
    assert_status(model.status, 200, &model.body, "read_main_model");
    let account = id(&model.body, "/doc/provider_key_id");
    let account_path = format!("/admin/resources/provider_keys/{account}");
    let current =
        f.cp.send("GET", &account_path, Some(f.admin.cookie()), None)
            .await;
    assert_status(current.status, 200, &current.body, "read_original_account");
    let mut doc = current
        .body
        .get("doc")
        .expect("public provider document")
        .clone();
    doc["api_base"] = json!(peer.base);
    // 同一真实 account 在任何 WS admission 前改地址；显式重供原密钥满足目的地变更合同。
    doc["api_key"] = json!(f.provider.state.secret);
    // 真实原始 fixture 只有这个 header；目的地变更须显式重供 GET 已脱敏的原值。
    doc["request"]["default_headers"] = json!({"OpenAI-Project":PROJECT});
    let changed =
        f.cp.send_conditional(
            "PUT",
            &account_path,
            f.admin.cookie(),
            current.body["version"].as_i64().expect("provider version"),
            json!({"doc":doc}),
        )
        .await;
    assert_status(
        changed.status,
        200,
        &changed.body,
        "retarget_same_account_before_admission",
    );
    let secondary =
        f.cp.send(
            "POST",
            "/admin/resources/models",
            Some(f.admin.cookie()),
            Some(
                json!({"doc":{"display_name":DURATION_ALIAS,"provider":"openai",
            "model_name":DURATION_MODEL,"provider_key_id":account}}),
            ),
        )
        .await;
    assert_status(
        secondary.status,
        201,
        &secondary.body,
        "create_unpriced_whisper_alias",
    );
    let secondary = id(&secondary.body, "/id");
    let path = format!("/admin/tenants/{}", f.tenant);
    let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
    assert_status(current.status, 200, &current.body, "read_tenant_grant");
    let granted =
        f.cp.send_conditional(
            "PATCH",
            &path,
            f.admin.cookie(),
            current.body["version"].as_i64().expect("tenant version"),
            json!({"allowed_model_ids":[f.model,secondary]}),
        )
        .await;
    assert_status(
        granted.status,
        200,
        &granted.body,
        "grant_independent_unpriced_B",
    );
    // 不调用 pricing API：token 单价不能冒充供应商秒量单价。
    secondary
}

async fn connect_duration(gateway: &Gateway, bearer: &str) -> Socket {
    let mut request = format!("ws://{}/v1/realtime?model={MODEL}", gateway.address)
        .into_client_request()
        .expect("ordinary raw-key WS request");
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {bearer}")
            .parse()
            .expect("the actual raw API-key header"),
    );
    let result = tokio_tungstenite::connect_async(request).await;
    match result {
        Ok((socket, response)) => {
            assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
            socket
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            panic!(
                "duration phase=raw_ws_admission status={}",
                response.status().as_u16()
            );
        }
        Err(_) => panic!("duration phase=raw_ws_admission transport_error"),
    }
}

async fn event_of_type(socket: &mut Socket, expected: &str, phase: &str) -> Value {
    let event = next_event(socket).await;
    if event.get("type").and_then(Value::as_str) != Some(expected) {
        let code = match event.pointer("/error/code").and_then(Value::as_str) {
            Some("permission_denied") => "permission_denied",
            Some("model_not_found") => "model_not_found",
            Some("native_authority_unavailable") => "native_authority_unavailable",
            Some("unsupported_feature") => "unsupported_feature",
            _ => "other_or_absent",
        };
        panic!("duration phase={phase} expected={expected} code={code}");
    }
    event
}

async fn accepted_audio(
    peer: &DurationPeer,
    socket: &mut Socket,
    item: &str,
    previous: Option<&str>,
    expected: &mut Vec<&'static str>,
) {
    // 每段为 24kHz PCM16 单声道 100ms，本地协议输入不推断真实 supplier 秒量。
    let audio = base64::engine::general_purpose::STANDARD.encode([0_u8; 4800]);
    socket
        .send(Message::Text(
            json!({"type":"input_audio_buffer.append",
        "event_id":format!("append_{item}"),"audio":audio})
            .to_string(),
        ))
        .await
        .expect("send an authorized actual audio append");
    expected.push("input_audio_buffer.append");
    peer.received(expected).await;
    socket
        .send(Message::Text(
            json!({"type":"input_audio_buffer.commit",
        "event_id":format!("commit_{item}")})
            .to_string(),
        ))
        .await
        .expect("commit actual audio before role withdrawal");
    expected.push("input_audio_buffer.commit");
    peer.received(expected).await;
    peer.send(
        json!({"type":"input_audio_buffer.committed","event_id":format!("accepted_{item}"),
        "item_id":item,"previous_item_id":previous}),
    );
    let accepted = event_of_type(socket, "input_audio_buffer.committed", "accepted_input").await;
    assert!(
        accepted.get("item_id") == Some(&json!(item)),
        "the distinct actual item is committed"
    );
    peer.send(
        json!({"type":"conversation.item.created","event_id":format!("created_{item}"),
        "previous_item_id":previous,"item":{"id":item,"object":"realtime.item",
        "type":"message","status":"completed","role":"user",
        "content":[{"type":"input_audio","transcript":null}]}}),
    );
    let created = event_of_type(socket, "conversation.item.created", "accepted_item").await;
    assert!(
        created.pointer("/item/id") == Some(&json!(item)),
        "the accepted item exists before its delayed independent ASR usage"
    );
}

fn duration_done(item: &str, delivery: &str, seconds: f64) -> Value {
    json!({"type":"conversation.item.input_audio_transcription.completed","event_id":delivery,
        "item_id":item,"content_index":0,"transcript":"controlled duration transcription",
        "usage":{"type":"duration","seconds":seconds}})
}

async fn forwarded_duration(socket: &mut Socket, expected: &Value) {
    let actual = event_of_type(
        socket,
        "conversation.item.input_audio_transcription.completed",
        "accepted_duration_tail",
    )
    .await;
    for field in ["event_id", "item_id", "content_index", "usage"] {
        assert!(
            actual.get(field) == expected.get(field),
            "the accepted ASR identity and explicitly reported seconds survive forwarding"
        );
    }
    assert!(
        actual.get("content_index") == Some(&json!(0)),
        "content_index zero is present"
    );
}

async fn operation_and_entry(f: &Fixture, secondary: Uuid) -> (Uuid, String) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sessions: Vec<Uuid> = sqlx::query_scalar("SELECT operation_id FROM native_live_sessions WHERE environment_id=$1 AND api_key_id=$2")
                .bind(f.cp.env().env_id).bind(f.key).fetch_all(f.cp.db().pool()).await
                .expect("observe the actual admitted live operation");
            assert!(sessions.len() <= 1, "one raw-key WS has one original operation");
            if let Some(operation) = sessions.first() {
                assert!(!operation.is_nil(), "the actual operation identity is nonnil");
                let rows: Vec<(String, Value)> = sqlx::query_as("SELECT entry_id,entry FROM native_live_entries WHERE environment_id=$1 AND operation_id=$2")
                    .bind(f.cp.env().env_id).bind(operation).fetch_all(f.cp.db().pool()).await
                    .expect("observe public accepted NativeEntry documents");
                let mut matches = Vec::new();
                for (key, document) in rows {
                    let entry: NativeEntry = serde_json::from_value(document)
                        .expect("a durable entry follows the public NativeEntry DTO");
                    if entry.model_id == secondary {
                        assert!(entry.requested_model_id == secondary
                            && entry.requested_model == DURATION_ALIAS
                            && entry.upstream_model == DURATION_MODEL
                            && entry.entry_id == key && !key.is_empty(),
                            "B is independently accepted with its actual alias and upstream identity");
                        matches.push(key);
                    }
                }
                assert!(matches.len() <= 1, "the configured B has one stable accepted entry");
                if let Some(entry) = matches.pop() { return (*operation, entry); }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the actual admission and B configuration persist public identities")
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DurationFact {
    journal: Uuid,
    resource: String,
    zero: bool,
}

async fn duration_facts(f: &Fixture, operation: Uuid, entry: &str) -> Vec<DurationFact> {
    let rows: Vec<(Uuid, Option<String>, Value, Option<Value>)> = sqlx::query_as(
        "SELECT id,response_id,usage,raw_usage FROM native_live_journal WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_all(f.cp.db().pool()).await
        .expect("observe the actual durable typed usage and supplier usage slots");
    let mut facts = Vec::new();
    for (journal, resource, usage, raw) in rows {
        let array = usage
            .as_array()
            .expect("real journal usage is an explicit array");
        for value in array {
            let observed: ObservedUsage = serde_json::from_value(value.clone())
                .expect("each durable quantity follows the existing public typed usage DTO");
            if observed.entry_id != entry {
                continue;
            }
            assert!(
                observed.upstream_model == DURATION_MODEL,
                "seconds belong to the accepted ASR model rather than main A"
            );
            let (seconds, basis, source) = match observed.meter {
                ObservedMeter::AudioSeconds {
                    seconds,
                    basis,
                    source,
                } => (seconds, basis, source),
                _ => panic!("supplier duration must remain AudioSeconds, never token-shaped zero"),
            };
            assert_eq!(basis, AudioMeasurementBasis::InputAudioSeconds);
            assert_eq!(source, MeasurementSource::SupplierReported);
            let meter = value.get("meter").expect("an explicit typed meter");
            assert!(
                meter.get("seconds").and_then(Value::as_str).is_some(),
                "reported zero seconds has a present string value"
            );
            assert!(
                meter.get("usage").is_none(),
                "no token meter is synthesized for seconds"
            );
            let seconds: f64 = seconds.parse().expect("public decimal seconds");
            assert!(
                seconds == 2.5 || seconds == 0.0,
                "retain exactly the supplier's two measurements"
            );
            let raw = raw
                .as_ref()
                .expect("supplier duration usage remains available for reconciliation");
            assert!(
                raw.get("type") == Some(&json!("duration")),
                "retain the supplier duration variant"
            );
            let raw_seconds = raw
                .get("seconds")
                .and_then(Value::as_f64)
                .expect("supplier seconds is explicitly present, including zero");
            assert_eq!(
                raw_seconds, seconds,
                "typed and raw measured quantities agree"
            );
            for token_slot in [
                "input_tokens",
                "output_tokens",
                "total_tokens",
                "input_token_details",
                "output_token_details",
                "usage",
            ] {
                assert!(
                    raw.get(token_slot).is_none() && meter.get(token_slot).is_none(),
                    "unreported ASR tokens stay absent rather than fabricated zero"
                );
            }
            let resource = resource
                .as_ref()
                .expect("a durable ASR item/content resource key");
            assert!(
                !journal.is_nil() && !resource.is_empty(),
                "durable identities are real and nonempty"
            );
            facts.push(DurationFact {
                journal,
                resource: resource.clone(),
                zero: seconds == 0.0,
            });
        }
    }
    facts.sort();
    facts
}

async fn two_duration_facts(f: &Fixture, operation: Uuid, entry: &str) -> Vec<DurationFact> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let facts = duration_facts(f, operation, entry).await;
            assert!(
                facts.len() <= 2,
                "redelivery cannot create another typed B supplier fact"
            );
            if facts.len() == 2 {
                assert!(
                    facts[0].journal != facts[1].journal && facts[0].resource != facts[1].resource,
                    "two accepted item/content identities remain independent durable facts"
                );
                assert_eq!(
                    facts.iter().filter(|fact| fact.zero).count(),
                    1,
                    "explicit zero is one actual fact, distinct from the nonzero 2.5-second fact"
                );
                return facts;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both accepted supplier seconds facts are durably observed")
}

async fn only_main_is_paid(f: &Fixture, secondary: Uuid) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rows: Vec<(String, String, Option<i64>, Value)> = sqlx::query_as(
                "SELECT c.requested_model_id,c.status,c.charge_micro_cny,u.event FROM pricing_charges_cny c JOIN usage_events u ON u.event_id=c.event_id WHERE c.tenant_id=$1::text")
                .bind(f.tenant).fetch_all(f.cp.db().pool()).await
                .expect("observe every real pricing outcome without requiring a new seconds charge row");
            let mut main = 0;
            for (model, status, charge, event) in rows {
                if model == f.model.to_string() {
                    main += 1;
                    assert!(status == "charged" && charge == Some(144), "main A has its actual nonzero known charge");
                    assert!(event.get("api_key_id") == Some(&json!(f.key)), "the original creator remains the payer");
                    assert!(event.get("operation") == Some(&json!("realtime")), "the known charge belongs to main A");
                    for (slot, amount) in [("prompt_tokens",10),("cached_prompt_tokens",2),
                        ("completion_tokens",3),("total_tokens",13),("prompt_audio_tokens",0),
                        ("completion_audio_tokens",0)] {
                        assert!(event.get(slot) == Some(&json!(amount)),
                            "main tokens and explicitly reported audio zero stay present");
                    }
                } else {
                    assert!(model == secondary.to_string(), "no unrelated model can acquire a charge");
                    assert!(status == "unpriced" && charge.is_none(),
                        "any existing B pricing row is unpriced/None, never paid or settled zero");
                    for slot in ["prompt_audio_tokens","completion_audio_tokens",
                        "prompt_image_tokens","completion_image_tokens"] {
                        assert!(event.get(slot).is_none_or(Value::is_null),
                            "optional unreported ASR token details remain absent");
                    }
                }
            }
            assert!(main <= 1, "A must not be charged again by an aggregate or ASR tail");
            let debits: Vec<(String, i64)> = sqlx::query_as(
                "SELECT c.requested_model_id,l.amount_micro_cny FROM billing_ledger_cny l JOIN pricing_charges_cny c ON c.event_id=l.event_id WHERE l.tenant_id=$1 AND l.kind='usage'")
                .bind(f.tenant).fetch_all(f.cp.db().pool()).await.expect("observe all actual tenant usage debits");
            assert!(debits.len() <= 1 && debits.iter().all(|(model, amount)| model == &f.model.to_string() && *amount == -144),
                "the entire usage ledger contains only A144, with no B debit or parallel aggregate");
            if main == 1 && debits.len() == 1 {
                let balance: i64 = sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
                    .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("observe the actual funded balance");
                assert_eq!(balance, 999_856, "only the one known A charge reduces the funded balance");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the main nonzero positive control is truly priced and debited once");
}

async fn optional_b_outbox_is_unpriced(f: &Fixture, operation: Uuid, entry: &str) {
    let rows: Vec<(Option<String>, Value)> = sqlx::query_as(
        "SELECT o.pricing_state,o.event FROM native_live_outbox o JOIN native_live_journal j ON j.id=o.journal_id WHERE j.environment_id=$1 AND j.operation_id=$2 AND o.entry_id=$3")
        .bind(f.cp.env().env_id).bind(operation).bind(entry).fetch_all(f.cp.db().pool()).await
        .expect("observe optional existing B delivery outcomes");
    // typed journal 正控已经必需；缺价不能凭空要求旧收费通道造一份零价行。
    for (state, event) in rows {
        assert!(
            state.as_deref().is_none_or(|state| state == "unpriced"),
            "any existing B outbox is pending or unpriced, never a completed zero bill"
        );
        for slot in [
            "prompt_audio_tokens",
            "completion_audio_tokens",
            "prompt_image_tokens",
            "completion_image_tokens",
        ] {
            assert!(
                event.get(slot).is_none_or(Value::is_null),
                "no optional B token detail is invented"
            );
        }
    }
}

// 撤权只阻止新工作；已接受 B 的秒量含显式零仍持久化，投递 ID 变化不多记事实或扣款。
#[tokio::test]
async fn accepted_supplier_duration_including_zero_survives_role_withdrawal_without_a_fake_zero_bill(
) {
    let Some(f) = fixture().await else { return };
    let mut peer = DurationPeer::start(f.provider.state.secret.clone()).await;
    let secondary = setup_duration_model(&f, &peer).await;
    let owner = f.cp.member_login_seeded(f.tenant, MemberRole::Owner).await;
    let mut gateway = Gateway::start(&f).await;
    let mut socket = connect_duration(&gateway, &f.bearer).await;
    event_of_type(&mut socket, "session.created", "actual_main_session").await;
    assert!(
        peer.state
            .handshakes
            .lock()
            .expect("actual handshake history")
            .as_slice()
            == [true],
        "the unchanged account credential/project and actual main model reach the retargeted peer"
    );
    socket
        .send(Message::Text(
            json!({"type":"session.update","event_id":"duration_B_configuration",
        "session":{"type":"realtime","audio":{"input":{"transcription":{"model":DURATION_ALIAS}}}}})
            .to_string(),
        ))
        .await
        .expect("configure the separately granted B through the actual gateway");
    let mut expected = vec!["session.update"];
    peer.received(&expected).await;
    let applied = event_of_type(&mut socket, "session.updated", "actual_B_configuration_ack").await;
    let configurations = peer
        .state
        .applied
        .lock()
        .expect("actual applied B model history")
        .clone();
    assert!(
        configurations.len() == 1
            && configurations[0].pointer("/audio/input/transcription/model")
                == Some(&json!(DURATION_MODEL)),
        "the actual provider receives the exact Whisper model, never an untranslated private alias"
    );
    assert!(
        applied.pointer("/session/audio/input/transcription/model") == Some(&json!(DURATION_ALIAS))
            || applied.pointer("/session/audio/input/transcription/model")
                == Some(&json!(DURATION_MODEL)),
        "the real applied configuration acknowledgment reaches the client"
    );
    accepted_audio(&peer, &mut socket, ITEM_FIRST, None, &mut expected).await;
    accepted_audio(
        &peer,
        &mut socket,
        ITEM_SECOND,
        Some(ITEM_FIRST),
        &mut expected,
    )
    .await;
    socket
        .send(Message::Text(
            json!({"type":"response.create","event_id":"duration_main_positive",
        "response":{"output_modalities":["text"]}})
            .to_string(),
        ))
        .await
        .expect("request independent nonzero main A work");
    expected.push("response.create");
    peer.received(&expected).await;
    peer.send(
        json!({"type":"response.created","event_id":"duration_main_accepted",
        "response":{"object":"realtime.response","id":FIRST,"status":"in_progress","output":[]}}),
    );
    let accepted = event_of_type(&mut socket, "response.created", "actual_main_response").await;
    assert!(
        accepted.pointer("/response/id") == Some(&json!(FIRST)),
        "the main response was accepted"
    );
    let main = done(FIRST, "duration_main_done", "completed", true);
    peer.send(main.clone());
    let forwarded = event_of_type(&mut socket, "response.done", "actual_main_usage").await;
    assert!(
        forwarded.pointer("/response/usage") == main.pointer("/response/usage"),
        "the main nonzero tokens and explicit audio zero are actually forwarded"
    );
    only_main_is_paid(&f, secondary).await;
    let (operation, entry) = operation_and_entry(&f, secondary).await;
    let changed =
        f.cp.send(
            "PATCH",
            &format!("/api/members/{}", f.member),
            Some(owner.cookie()),
            Some(json!({"role":"billing"})),
        )
        .await;
    assert_status(
        changed.status,
        200,
        &changed.body,
        "actual_owner_role_withdrawal",
    );
    assert!(
        changed.body.get("role") == Some(&json!("billing")),
        "public response confirms current role withdrawal"
    );
    // 先放出已接受尾量；不以撤权后的新工作权限重新判定旧事实。
    let nonzero = duration_done(ITEM_SECOND, "duration_second_late", 2.5);
    let zero = duration_done(ITEM_FIRST, "duration_first_later_explicit_zero", 0.0);
    peer.send(nonzero.clone());
    peer.send(zero.clone());
    forwarded_duration(&mut socket, &nonzero).await;
    forwarded_duration(&mut socket, &zero).await;
    let original = two_duration_facts(&f, operation, &entry).await;
    for (mut repeated, delivery) in [
        (nonzero, "duration_second_changed_delivery"),
        (zero, "duration_first_changed_delivery"),
    ] {
        repeated["event_id"] = json!(delivery);
        peer.send(repeated.clone());
        forwarded_duration(&mut socket, &repeated).await;
    }
    // 新工作只在真实配置已同步撤销后断言拒绝，不要求传播期间逐音频块访问 CP。
    let key_path = format!("/admin/keys/{}", f.key);
    let key =
        f.cp.send("GET", &key_path, Some(f.admin.cookie()), None)
            .await;
    assert_status(key.status, 200, &key.body, "read_public_key_version");
    let disabled =
        f.cp.send_conditional(
            "PATCH",
            &key_path,
            f.admin.cookie(),
            key.body["version"]
                .as_i64()
                .expect("public AdminKey version"),
            json!({"disabled":true}),
        )
        .await;
    assert_status(
        disabled.status,
        200,
        &disabled.body,
        "explicit_public_key_disable",
    );
    assert!(
        disabled.body.get("disabled") == Some(&json!(true)),
        "the supported operators' switch is explicitly disabled"
    );
    let current = projected_snapshot(&f.cp).await;
    assert!(
        current
            .apikeys
            .get_by_id(&f.key.to_string())
            .expect("actual explicitly disabled key projection")
            .value
            .disabled,
        "the explicit key disable reaches actual CP to kine projection"
    );
    gateway.state.snapshot.store(current);
    assert!(
        gateway
            .state
            .snapshot
            .load()
            .apikeys
            .get_by_id(&f.key.to_string())
            .expect("actual locally synchronized key")
            .value
            .disabled,
        "the real serving gateway has applied the actual disabled key configuration"
    );
    let before = peer.inputs();
    peer.received(&expected).await;
    let attempted = socket.send(Message::Text(json!({"type":"input_audio_buffer.append",
        "event_id":"duration_forbidden_new_audio","audio":base64::engine::general_purpose::STANDARD.encode([0_u8;4800])})
        .to_string())).await;
    if attempted.is_ok() {
        await_denial(&mut socket).await;
    }
    let _ = socket.close(None).await;
    drop(socket);
    peer.drained().await;
    assert!(peer.inputs() == before,
        "after fully draining the actual reader, the locally synchronized key disable admits no additional provider input");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sessions: Vec<(String,String,String)> = sqlx::query_as(
                "SELECT state,coverage,accounting_state FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2 AND api_key_id=$3")
                .bind(f.cp.env().env_id).bind(operation).bind(f.key).fetch_all(f.cp.db().pool()).await
                .expect("observe the original nonempty session accounting outcome");
            assert_eq!(sessions.len(), 1, "the actual original session remains durably represented");
            assert_ne!(sessions[0].2, "settled", "known seconds without a price cannot be settled as zero");
            if sessions[0].0 == "closed" && sessions[0].1 == "unknown"
                && sessions[0].2 == "awaiting_reconciliation" { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("plain transport close with unpriced B facts leaves Unknown accounting");
    let after = two_duration_facts(&f, operation, &entry).await;
    assert_eq!(after, original, "changed delivery IDs preserve exactly the same two durable resource keys and measured facts");
    only_main_is_paid(&f, secondary).await;
    optional_b_outbox_is_unpriced(&f, operation, &entry).await;
    gateway.assert_billing_sink_empty().await;
}
