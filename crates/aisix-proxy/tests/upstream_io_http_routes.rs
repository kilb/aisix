//! 上游 I/O 组件回归：真实路由连接本地 HTTP 上游，不声称提供商端到端。

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageIoAuditUpdate, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Case {
    route: &'static str,
    upstream_route: &'static str,
    provider: &'static str,
    request_type: String,
    request: Vec<u8>,
    response_type: &'static str,
    response: Vec<u8>,
}

impl Case {
    fn at_upstream(mut self, upstream_route: &'static str) -> Self {
        self.upstream_route = upstream_route;
        self
    }
}

fn app(
    base: &str,
    provider: &str,
) -> TestResult<(
    axum::Router,
    Receiver<UsageEvent>,
    Receiver<UsageIoAuditUpdate>,
)> {
    let snapshot = AisixSnapshot::new();
    let key: ProviderKey = serde_json::from_value(json!({
        "display_name":"component upstream", "api_key":"component-upstream-key",
        "provider":provider,
        "adapter":if provider == "anthropic" { "anthropic" } else { "openai" },
        "api_base":format!("{base}/v1")
    }))?;
    snapshot.provider_keys.insert(ResourceEntry::new(
        "22222222-2222-4222-8222-222222222222",
        key,
        1,
    ));
    let model: Model = serde_json::from_value(json!({
        "display_name":"audit-model", "provider":provider,
        "model_name":"provider-model-name-longer-than-alias",
        "provider_key_id":"22222222-2222-4222-8222-222222222222"
    }))?;
    snapshot.models.insert(ResourceEntry::new(
        "11111111-1111-4111-8111-111111111111",
        model,
        1,
    ));
    let caller: ApiKey = serde_json::from_value(json!({
        "key_hash":ApiKey::hash_bearer("component-caller"), "allowed_models":["audit-model"]
    }))?;
    snapshot.apikeys.insert(ResourceEntry::new(
        "33333333-3333-4333-8333-333333333333",
        caller,
        1,
    ));
    let config: ProxyConfig = serde_json::from_value(json!({
        "addr":"127.0.0.1:0", "request_body_limit_bytes":1048576
    }))?;
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    hub.register_specialized(
        "anthropic",
        Arc::new(aisix_provider_anthropic::AnthropicBridge::new()),
    );
    let snapshot = SnapshotHandle::new(snapshot);
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let (io_tx, io_rx) = tokio::sync::mpsc::channel(16);
    let state = ProxyState::new(snapshot, hub, &config)
        .without_cache()
        .with_usage_sink(UsageSink::new(tx).with_io_audit_sender(io_tx));
    Ok((build_router(state), rx, io_rx))
}

fn json_case(route: &'static str, provider: &'static str, request: Value, response: Value) -> Case {
    Case {
        route,
        upstream_route: route,
        provider,
        request_type: "application/json".to_owned(),
        request: request.to_string().into_bytes(),
        response_type: "application/json",
        response: response.to_string().into_bytes(),
    }
}

fn multipart_case(route: &'static str, file_field: &str, response: Value) -> Case {
    let boundary = "upstream-io-component-boundary";
    let (filename, content_type, file_bytes) = if file_field == "file" {
        // 有效的八采样 PCM WAV，避免用无效媒体掩盖真实路由行为。
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&44_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8000_u32.to_le_bytes());
        wav.extend_from_slice(&8000_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&8_u32.to_le_bytes());
        wav.extend_from_slice(&[128; 8]);
        ("sample.wav", "audio/wav", wav)
    } else {
        // 一像素透明 PNG，图片字段包含真实二进制而非文本占位。
        (
            "sample.png",
            "image/png",
            vec![
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0,
                1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96,
                0, 2, 0, 0, 5, 0, 1, 122, 94, 171, 63, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
                130,
            ],
        )
    };
    let mut request = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\naudit-model\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nA汉😀\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\n\
         Content-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    request.extend_from_slice(&file_bytes);
    request.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Case {
        route,
        upstream_route: route,
        provider: "openai",
        request_type: format!("multipart/form-data; boundary={boundary}"),
        request,
        response_type: "application/json",
        response: response.to_string().into_bytes(),
    }
}

async fn check(case: Case) -> TestResult {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path(case.upstream_route))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(case.response.clone())
                .insert_header("content-type", case.response_type),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let (router, mut events, mut io_updates) = app(&upstream.uri(), case.provider)?;
    let request = Request::builder()
        .method("POST")
        .uri(case.route)
        .header("authorization", "Bearer component-caller")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", case.request_type)
        .extension(ConnectInfo("127.0.0.1:41000".parse::<SocketAddr>()?))
        .body(Body::from(case.request.clone()))?;
    let response = router.oneshot(request).await?;
    let status = response.status();
    let returned = to_bytes(response.into_body(), 1048576).await?;
    assert_eq!(status, StatusCode::OK, "{}: {:?}", case.route, returned);
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await?
        .ok_or("usage sink closed")?;
    let audit = event.io_audit.ok_or("original event lacks upstream I/O")?;
    let received = upstream
        .received_requests()
        .await
        .ok_or("upstream did not retain requests")?;
    assert_eq!(received.len(), 1, "{}", case.route);
    assert_eq!(
        received[0].url.path(),
        case.upstream_route,
        "{}",
        case.route
    );
    assert_ne!(received[0].body, case.request, "{}", case.route);
    assert_eq!(audit.version, 2, "{}", case.route);
    assert_eq!(audit.boundary, "upstream_attempt", "{}", case.route);
    assert_eq!(
        audit.request_body_bytes,
        Some(received[0].body.len() as u64),
        "{}",
        case.route
    );
    assert_eq!(
        audit.response_body_bytes,
        Some(case.response.len() as u64),
        "{}",
        case.route
    );
    assert_eq!(audit.request_complete, Some(true), "{}", case.route);
    assert_eq!(audit.exchange_count, Some(1), "{}", case.route);
    if case.route == "/v1/audio/transcriptions" {
        // delta 与 done 都代表同一份转写；缺失文本或重复累计必须报红。
        assert_eq!(
            audit.output_text,
            Some(aisix_core::usage_io::TextMeasurement {
                utf8_bytes: "答😀".len() as u64,
                unicode_scalars: "答😀".chars().count() as u64,
                complete: true,
            }),
            "{} must retain one decoded transcript",
            case.route
        );
    }
    assert!(
        events.try_recv().is_err(),
        "{} duplicated usage",
        case.route
    );
    assert!(
        io_updates.try_recv().is_err(),
        "{} must carry upstream bytes on the original token event, without a separate amendment",
        case.route
    );
    Ok(())
}

// 删除任一处理器的真实发送观察会丢失原事件的审计或把变换前正文长度带进来。
#[tokio::test]
async fn buffered_http_families_report_final_upstream_bytes_on_original_event() -> TestResult {
    let cases = vec![
        json_case(
            "/v1/messages",
            "anthropic",
            json!({"model":"audit-model","max_tokens":32,"messages":[{"role":"user","content":"A汉😀"}]}),
            json!({"id":"msg_component","type":"message","role":"assistant","model":"provider-model-name-longer-than-alias","content":[{"type":"text","text":"答😀"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":2}}),
        ),
        json_case(
            "/v1/messages/count_tokens",
            "anthropic",
            json!({"model":"audit-model","messages":[{"role":"user","content":"A汉😀"}]}),
            json!({"input_tokens":11}),
        ),
        json_case(
            "/v1/rerank",
            "cohere",
            json!({"model":"audit-model","query":"A汉😀","documents":["first","second"]}),
            json!({"results":[{"index":0,"relevance_score":0.9}],"meta":{"billed_units":{"search_units":1,"input_tokens":11}}}),
        )
        .at_upstream("/v2/rerank"),
        json_case(
            "/v1/images/generations",
            "openai",
            json!({"model":"audit-model","prompt":"A汉😀"}),
            json!({"created":1700000000,"data":[{"b64_json":"AAEC","revised_prompt":"答😀"}],"usage":{"input_tokens":11,"output_tokens":2,"total_tokens":13}}),
        ),
        multipart_case(
            "/v1/images/edits",
            "image",
            json!({"created":1700000000,"data":[{"b64_json":"AAEC","revised_prompt":"答😀"}],"usage":{"input_tokens":11,"output_tokens":2,"total_tokens":13}}),
        ),
        multipart_case(
            "/v1/audio/transcriptions",
            "file",
            json!({"text":"答😀","usage":{"type":"tokens","input_tokens":11,"output_tokens":2,"total_tokens":13}}),
        ),
    ];
    futures::future::try_join_all(cases.into_iter().map(check)).await?;
    Ok(())
}

// 二进制合成响应仍按上游正文计数，且不会被当作输出文字。
#[tokio::test]
async fn speech_binary_response_is_counted_before_client_stream_delivery() -> TestResult {
    check(Case {
        route: "/v1/audio/speech",
        upstream_route: "/v1/audio/speech",
        provider: "openai",
        request_type: "application/json".to_owned(),
        request: json!({"model":"audit-model","input":"A汉😀","voice":"alloy"})
            .to_string()
            .into_bytes(),
        response_type: "audio/mpeg",
        response: vec![0, 255, 1, 254, 2, 253, 3, 252],
    })
    .await
}

// SSE 的原始 event/data 包装计入正文大小，不能用渲染后的客户端帧代替。
#[tokio::test]
async fn streaming_http_families_report_original_upstream_framing() -> TestResult {
    let mut messages = json_case(
        "/v1/messages",
        "anthropic",
        json!({"model":"audit-model","stream":true,"max_tokens":32,"messages":[{"role":"user","content":"A汉😀"}]}),
        Value::Null,
    );
    messages.response_type = "text/event-stream";
    messages.response = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_component\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"provider-model-name-longer-than-alias\",\"content\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"答😀\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    )
    .as_bytes()
    .to_vec();
    let mut images = json_case(
        "/v1/images/generations",
        "openai",
        json!({"model":"audit-model","stream":true,"prompt":"A汉😀"}),
        Value::Null,
    );
    images.response_type = "text/event-stream";
    images.response = concat!(
        "event: image_generation.partial_image\ndata: {\"type\":\"image_generation.partial_image\",\"partial_image_index\":0,\"b64_json\":\"AA==\"}\n\n",
        "event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"b64_json\":\"AAEC\",\"revised_prompt\":\"答😀\",\"usage\":{\"input_tokens\":11,\"output_tokens\":2,\"total_tokens\":13}}\n\n"
    )
    .as_bytes()
    .to_vec();
    let mut transcription = multipart_case("/v1/audio/transcriptions", "file", Value::Null);
    let prefix = b"--upstream-io-component-boundary\r\nContent-Disposition: form-data; name=\"stream\"\r\n\r\ntrue\r\n";
    drop(transcription.request.splice(0..0, prefix.iter().copied()));
    transcription.response_type = "text/event-stream";
    transcription.response = concat!(
        "event: transcript.text.delta\ndata: {\"type\":\"transcript.text.delta\",\"delta\":\"答😀\"}\n\n",
        "event: transcript.text.done\ndata: {\"type\":\"transcript.text.done\",\"text\":\"答😀\",\"usage\":{\"type\":\"tokens\",\"input_tokens\":11,\"output_tokens\":2,\"total_tokens\":13}}\n\n"
    )
    .as_bytes()
    .to_vec();
    futures::future::try_join_all([messages, images, transcription].into_iter().map(check)).await?;
    Ok(())
}
