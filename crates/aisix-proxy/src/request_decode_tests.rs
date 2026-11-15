use std::sync::Arc;

use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

const RESPONSES_JSON: &[u8] =
    br#"{"model":"request-decode-absent","input":"transport-regression"}"#;

// 离线生成并独立还原为 RESPONSES_JSON；测试运行时不调用压缩器。
const RESPONSES_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x40, 0xdd, 0x01, 0x00, 0x02, 0xc4, 0x0d, 0x14, 0xa0, 0xb7, 0x0d,
    0x2e, 0x90, 0xc8, 0x7e, 0xaf, 0x7c, 0x64, 0x3d, 0xd7, 0x06, 0xc0, 0x01, 0x5a, 0x5e, 0xff, 0x9b,
    0x10, 0x87, 0x0f, 0x8f, 0xea, 0x53, 0x78, 0xd6, 0x26, 0x22, 0x3d, 0x93, 0xeb, 0xe2, 0x4d, 0xe8,
    0x8e, 0x89, 0xf7, 0xae, 0x80, 0xa9, 0x1b, 0x06, 0xb7, 0x5a, 0x7d, 0x2a, 0x9e, 0x2e, 0x1e, 0x6e,
    0x48, 0xd8, 0x08, 0x00,
];

fn admitted_state(limit_bytes: usize) -> Result<crate::ProxyState, serde_json::Error> {
    let snapshot = AisixSnapshot::new();
    let key: ApiKey = serde_json::from_value(json!({
        "key_hash": ApiKey::hash_bearer("sk-request-decode-test"),
        "allowed_models": ["*"],
        "allowed_routes": ["*"],
        "allowed_agents": ["*"],
        "mcp_access": {"allow": ["*"]},
        "user_id": "transport-user",
    }))?;
    snapshot
        .apikeys
        .insert(ResourceEntry::new("request-decode-key", key, 1));
    let config = ProxyConfig {
        addr: "127.0.0.1:0".into(),
        request_body_limit_bytes: limit_bytes,
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
    let state =
        crate::ProxyState::new(SnapshotHandle::new(snapshot), Arc::new(Hub::new()), &config)
            .without_cache();
    Ok(state)
}

fn admitted_router() -> Result<axum::Router, serde_json::Error> {
    Ok(crate::build_router(admitted_state(1_048_576)?))
}

// 缺失入站解压时，合法压缩请求停在 JSON 解析，不能到达明确的模型不存在决策。
#[tokio::test]
async fn request_decode_zstd_responses_reaches_model_resolution(
) -> Result<(), Box<dyn std::error::Error>> {
    let app = admitted_router()?;
    for (label, body, encoding) in [
        ("plain control", RESPONSES_JSON, None),
        ("zstd request", RESPONSES_ZSTD, Some("zstd")),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("authorization", "Bearer sk-request-decode-test")
            .header("content-type", "application/json");
        if let Some(encoding) = encoding {
            request = request.header("content-encoding", encoding);
        }
        let response = app.clone().oneshot(request.body(Body::from(body))?).await?;
        let status = response.status();
        let response_body = to_bytes(response.into_body(), 16_384).await?;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{label}: {}",
            String::from_utf8_lossy(&response_body)
        );
        let envelope: Value = serde_json::from_slice(&response_body)?;
        assert_eq!(
            envelope["error"]["type"].as_str(),
            Some("model_not_found"),
            "{label}: {envelope}"
        );
    }
    Ok(())
}

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use axum::extract::Multipart;
use axum::http::{header, Version};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::BodyExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn body_request(path: &str, body: Body, content_type: &str, zstd: bool) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .version(Version::HTTP_2)
        .uri(path)
        .header(header::AUTHORIZATION, "Bearer sk-request-decode-test")
        .header(header::CONTENT_TYPE, content_type);
    if zstd {
        builder = builder.header(header::CONTENT_ENCODING, "zstd");
    }
    builder.body(body).unwrap()
}

async fn response_json(
    response: Response,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024).await?;
    Ok((status, serde_json::from_slice(&bytes)?))
}

fn assert_missing_model(status: StatusCode, envelope: &Value, path: &str) {
    assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {envelope}");
    let expected_type = if path.starts_with("/v1/messages") {
        "not_found_error"
    } else {
        "model_not_found"
    };
    assert_eq!(
        envelope["error"]["type"].as_str(),
        Some(expected_type),
        "{path}: {envelope}"
    );
    assert!(
        envelope["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("request-decode-absent")),
        "{path}: {envelope}"
    );
}

// 任一接口遗漏共享解码层都会在压缩分支返回解析错误，而不是明确的 404。
#[tokio::test]
async fn request_decode_json_and_bytes_families_reach_model_resolution() -> TestResult {
    let app = admitted_router()?;
    for case in JSON_CASES {
        for (label, body, zstd) in [("plain", case.plain, false), ("zstd", case.encoded, true)] {
            let response = app
                .clone()
                .oneshot(body_request(
                    case.path,
                    Body::from(body),
                    "application/json",
                    zstd,
                ))
                .await?;
            let (status, envelope) = response_json(response).await?;
            assert_missing_model(status, &envelope, &format!("{} ({label})", case.path));
        }
    }
    Ok(())
}

fn assert_no_json_upstream_dispatch(metrics: &aisix_obs::Metrics, path: &str) -> TestResult {
    let scrape = metrics.render();
    let mut attempts = 0.0;
    for sample in scrape
        .lines()
        .filter(|line| line.starts_with("aisix_deployment_requests_total{"))
    {
        let (_, count) = sample
            .rsplit_once(' ')
            .ok_or("deployment sample omitted count")?;
        attempts += count.parse::<f64>()?;
    }
    assert_eq!(attempts, 0.0, "{path}: {scrape}");
    Ok(())
}

// 合法首值后的垃圾或第二个 JSON 值必须先报 400，不能忽略尾部后走模型 404。
#[tokio::test]
async fn request_decode_json_families_reject_trailing_data_before_model_resolution() -> TestResult {
    let state = admitted_state(1_048_576)?;
    let metrics = state.metrics.clone();
    let app = crate::build_router(state);
    for case in JSON_CASES {
        for (suffix_name, suffix) in [
            (
                "non-JSON trailing garbage",
                b" \ntrailing-garbage".as_slice(),
            ),
            ("second complete JSON value", b" {}".as_slice()),
        ] {
            let entity = [case.plain, suffix].concat();
            let response = app
                .clone()
                .oneshot(body_request(
                    case.path,
                    Body::from(entity),
                    "application/json",
                    false,
                ))
                .await?;
            assert!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.starts_with("application/json")),
                "{} {suffix_name}",
                case.path
            );
            let (status, envelope) = response_json(response).await?;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{} {suffix_name}: {envelope}",
                case.path
            );
            assert!(
                envelope["error"].is_object(),
                "{} {suffix_name}: {envelope}",
                case.path
            );
            assert_eq!(
                envelope["error"]["type"].as_str(),
                Some("invalid_request_error"),
                "{} {suffix_name}: {envelope}",
                case.path
            );
            assert!(
                envelope["error"]["message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty()),
                "{} {suffix_name}: {envelope}",
                case.path
            );
            if case.path.starts_with("/v1/messages") {
                assert_eq!(
                    envelope["type"].as_str(),
                    Some("error"),
                    "{} {suffix_name}: {envelope}",
                    case.path
                );
            } else {
                assert!(
                    envelope.get("type").is_none(),
                    "{} {suffix_name}: {envelope}",
                    case.path
                );
            }
            assert_no_json_upstream_dispatch(&metrics, case.path)?;
        }
    }
    Ok(())
}

// 仅有 JSON 空白的尾部仍是同一文档，必须像原始合法前缀一样到达模型 404。
#[tokio::test]
async fn request_decode_json_families_allow_trailing_whitespace() -> TestResult {
    let state = admitted_state(1_048_576)?;
    let metrics = state.metrics.clone();
    let app = crate::build_router(state);
    for case in JSON_CASES {
        let entity = [case.plain, b" \t\r\n ".as_slice()].concat();
        let response = app
            .clone()
            .oneshot(body_request(
                case.path,
                Body::from(entity),
                "application/json",
                false,
            ))
            .await?;
        let (status, envelope) = response_json(response).await?;
        assert_missing_model(status, &envelope, case.path);
        assert_no_json_upstream_dispatch(&metrics, case.path)?;
    }
    Ok(())
}

async fn capture_transport(request: Request<Body>) -> Response {
    let headers = request.headers();
    let header_value = |name| {
        headers
            .get(name)
            .map(|value| STANDARD.encode(value.as_bytes()))
    };
    let encoding = header_value(header::CONTENT_ENCODING);
    let length = header_value(header::CONTENT_LENGTH);
    let content_type = header_value(header::CONTENT_TYPE);
    let sentinel = header_value(header::HeaderName::from_static("x-transport-sentinel"));
    match to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => axum::Json(json!({
            "bytes": STANDARD.encode(bytes),
            "content_encoding": encoding,
            "content_length": length,
            "content_type": content_type,
            "sentinel": sentinel,
        }))
        .into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}

fn transport_router(limit: usize) -> Result<axum::Router, serde_json::Error> {
    Ok(axum::Router::new()
        .route("/capture", post(capture_transport))
        .route_layer(middleware::from_fn_with_state(
            admitted_state(limit)?,
            crate::request_decode::decode_request_body,
        )))
}

fn captured_bytes(envelope: &Value) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let value = envelope
        .get("bytes")
        .and_then(Value::as_str)
        .ok_or("capture omitted bytes")?;
    Ok(STANDARD.decode(value)?)
}

// 保留 CE/CL 或改变 Content-Type 会破坏后续解析或重复解码。
#[tokio::test]
async fn request_decode_recovers_exact_bytes_and_consumes_only_encoding_and_length() -> TestResult {
    let app = transport_router(4096)?;
    let request = Request::builder()
        .method("POST")
        .uri("/capture")
        .header(header::CONTENT_ENCODING, " \tZsTd \t")
        .header(header::CONTENT_LENGTH, RESPONSES_ZSTD.len())
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header("x-transport-sentinel", "preserve-this")
        .body(Body::from(RESPONSES_ZSTD))?;
    let (status, envelope) = response_json(app.oneshot(request).await?).await?;
    assert_eq!(status, StatusCode::OK, "{envelope}");
    assert_eq!(captured_bytes(&envelope)?, RESPONSES_JSON);
    assert_eq!(envelope.get("content_encoding"), Some(&Value::Null));
    assert_eq!(envelope.get("content_length"), Some(&Value::Null));
    assert_eq!(
        envelope["content_type"].as_str(),
        Some(STANDARD.encode("application/json; charset=utf-8").as_str())
    );
    assert_eq!(
        envelope["sentinel"].as_str(),
        Some(STANDARD.encode("preserve-this").as_str())
    );
    Ok(())
}

// 其它编码及编码堆栈仍走原路径，不能新增 415 或误消费头部。
#[tokio::test]
async fn request_decode_other_and_duplicate_codings_remain_untouched() -> TestResult {
    let app = transport_router(4096)?;
    for values in [
        vec![],
        vec!["identity"],
        vec!["gzip"],
        vec!["br"],
        vec!["zstd, gzip"],
        vec!["zstd", "zstd"],
    ] {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/capture")
            .header(header::CONTENT_LENGTH, RESPONSES_JSON.len());
        for value in &values {
            builder = builder.header(header::CONTENT_ENCODING, *value);
        }
        let (status, envelope) = response_json(
            app.clone()
                .oneshot(builder.body(Body::from(RESPONSES_JSON))?)
                .await?,
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{values:?}: {envelope}");
        assert_eq!(captured_bytes(&envelope)?, RESPONSES_JSON, "{values:?}");
        assert_eq!(
            envelope["content_length"].as_str(),
            Some(STANDARD.encode(RESPONSES_JSON.len().to_string()).as_str())
        );
        let expected_encoding = values
            .first()
            .map(|value| Value::String(STANDARD.encode(value)));
        assert_eq!(
            envelope.get("content_encoding"),
            Some(expected_encoding.as_ref().unwrap_or(&Value::Null))
        );
    }
    Ok(())
}

// 只读首帧会截断请求；空帧和可跳过帧也不能吞掉后续有效数据。
#[tokio::test]
async fn request_decode_concatenated_empty_and_skippable_frames_are_complete() -> TestResult {
    let app = transport_router(4096)?;
    let skippable = b"\x50\x2a\x4d\x18\x00\x00\x00\x00";
    for (wire, expected) in [
        (
            [SPLIT_FIRST_ZSTD, SPLIT_SECOND_ZSTD].concat(),
            RESPONSES_JSON,
        ),
        ([EMPTY_ZSTD, RESPONSES_ZSTD].concat(), RESPONSES_JSON),
        (
            [skippable.as_slice(), RESPONSES_ZSTD].concat(),
            RESPONSES_JSON,
        ),
        (EMPTY_ZSTD.to_vec(), b"".as_slice()),
    ] {
        let response = app
            .clone()
            .oneshot(body_request(
                "/capture",
                Body::from(wire),
                "application/json",
                true,
            ))
            .await?;
        let (status, envelope) = response_json(response).await?;
        assert_eq!(status, StatusCode::OK, "{envelope}");
        assert_eq!(captured_bytes(&envelope)?, expected);
    }
    Ok(())
}

// 截断、尾垃圾、伪编码和超大窗口必须变成请求体错误，不能输出已解码前缀。
#[tokio::test]
async fn request_decode_invalid_entities_fail_before_exposing_data() -> TestResult {
    let seen = Arc::new(AtomicUsize::new(0));
    let capture_seen = seen.clone();
    let app = axum::Router::new()
        .route(
            "/capture",
            post(move |request: Request<Body>| {
                let seen = capture_seen.clone();
                async move {
                    let mut body = request.into_body();
                    while let Some(frame) = body.frame().await {
                        match frame {
                            Ok(frame) => {
                                if frame.data_ref().is_some() {
                                    seen.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            Err(_) => return StatusCode::BAD_REQUEST,
                        }
                    }
                    StatusCode::OK
                }
            }),
        )
        .route_layer(middleware::from_fn_with_state(
            admitted_state(0)?,
            crate::request_decode::decode_request_body,
        ));
    for (label, wire) in [
        ("missing frame", vec![]),
        ("wrong magic", b"not-zstd".to_vec()),
        ("false coding", RESPONSES_JSON.to_vec()),
        (
            "truncated frame",
            RESPONSES_ZSTD[..RESPONSES_ZSTD.len() - 1].to_vec(),
        ),
        ("trailing garbage", [RESPONSES_ZSTD, b"garbage"].concat()),
        (
            "truncated second frame",
            [RESPONSES_ZSTD, &RESPONSES_ZSTD[..RESPONSES_ZSTD.len() - 1]].concat(),
        ),
        ("excessive window", WIDE_WINDOW_ZSTD.to_vec()),
    ] {
        seen.store(0, Ordering::SeqCst);
        let response = app
            .clone()
            .oneshot(body_request(
                "/capture",
                Body::from(wire),
                "application/json",
                true,
            ))
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{label}");
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "{label}: decoded prefix escaped"
        );
    }
    Ok(())
}

const MULTIPART_TYPE: &str = "multipart/form-data; boundary=transport-boundary";

// Multipart 也必须实际解析字段并到达模型决策，不能只比较两次解析失败。
#[tokio::test]
async fn request_decode_multipart_families_reach_model_resolution() -> TestResult {
    let app = admitted_router()?;
    for (path, plain, encoded) in [
        ("/v1/audio/transcriptions", MULTIPART_PLAIN, MULTIPART_ZSTD),
        ("/v1/files", MULTIPART_PLAIN, MULTIPART_ZSTD),
        (
            "/v1/images/edits",
            IMAGE_MULTIPART_PLAIN,
            IMAGE_MULTIPART_ZSTD,
        ),
    ] {
        for (bytes, zstd) in [(plain, false), (encoded, true)] {
            let response = app
                .clone()
                .oneshot(body_request(path, Body::from(bytes), MULTIPART_TYPE, zstd))
                .await?;
            let (status, envelope) = response_json(response).await?;
            assert_missing_model(status, &envelope, path);
        }
    }
    Ok(())
}

fn delayed_suffix_body(suffix: Vec<u8>) -> Body {
    Body::from_stream(crate::request_id::in_request_span(async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from_static(MULTIPART_ZSTD));
        tokio::task::yield_now().await;
        yield Ok(Bytes::from(suffix));
    }))
}

// 终止 boundary 后才到达的坏第二帧，必须在第一个 multipart 字段可见前拒绝。
#[tokio::test]
async fn request_decode_delayed_invalid_multipart_suffix_exposes_no_field() -> TestResult {
    let fields = Arc::new(AtomicUsize::new(0));
    let capture_fields = fields.clone();
    let app = axum::Router::new()
        .route(
            "/capture",
            post(move |mut multipart: Multipart| {
                let fields = capture_fields.clone();
                async move {
                    loop {
                        match multipart.next_field().await {
                            Ok(Some(_)) => {
                                fields.fetch_add(1, Ordering::SeqCst);
                            }
                            Ok(None) => return StatusCode::OK,
                            Err(_) => return StatusCode::BAD_REQUEST,
                        }
                    }
                }
            }),
        )
        .route_layer(middleware::from_fn_with_state(
            admitted_state(4096)?,
            crate::request_decode::decode_request_body,
        ));
    for (wire, zstd) in [(MULTIPART_PLAIN, false), (MULTIPART_ZSTD, true)] {
        fields.store(0, Ordering::SeqCst);
        let response = app
            .clone()
            .oneshot(body_request(
                "/capture",
                Body::from(wire),
                MULTIPART_TYPE,
                zstd,
            ))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fields.load(Ordering::SeqCst), 2);
    }
    for suffix in [
        b"invalid-second-frame".to_vec(),
        RESPONSES_ZSTD[..RESPONSES_ZSTD.len() - 1].to_vec(),
    ] {
        fields.store(0, Ordering::SeqCst);
        let response = app
            .clone()
            .oneshot(body_request(
                "/capture",
                delayed_suffix_body(suffix.clone()),
                MULTIPART_TYPE,
                true,
            ))
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(fields.load(Ordering::SeqCst), 0);
        for path in ["/v1/audio/transcriptions", "/v1/files", "/v1/images/edits"] {
            let response = admitted_router()?
                .oneshot(body_request(
                    path,
                    delayed_suffix_body(suffix.clone()),
                    MULTIPART_TYPE,
                    true,
                ))
                .await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        }
    }
    Ok(())
}

fn chunked(bytes: &'static [u8]) -> Body {
    Body::from_stream(futures::stream::iter(
        bytes
            .chunks(11)
            .map(|chunk| Ok::<_, std::io::Error>(Bytes::from_static(chunk))),
    ))
}

// 移除 decoded cap 会把 one-over 用例放到模型决策；边界本身必须正常通过。
#[tokio::test]
async fn request_decode_decoded_limits_apply_at_boundary_for_json_and_multipart() -> TestResult {
    for (path, wire, content_type, decoded_len) in [
        (
            "/v1/responses",
            PADDED_RESPONSES_ZSTD,
            "application/json",
            PADDED_RESPONSES_LEN,
        ),
        (
            "/v1/messages",
            PADDED_MESSAGES_ZSTD,
            "application/json",
            PADDED_MESSAGES_LEN,
        ),
        (
            "/v1/files",
            MULTIPART_ZSTD,
            MULTIPART_TYPE,
            MULTIPART_PLAIN.len(),
        ),
    ] {
        assert!(wire.len() < decoded_len - 1);
        for declared in [false, true] {
            for (limit, expected) in [
                (decoded_len, StatusCode::NOT_FOUND),
                (decoded_len - 1, StatusCode::PAYLOAD_TOO_LARGE),
            ] {
                let app = crate::build_router(admitted_state(limit)?);
                let mut request = body_request(path, chunked(wire), content_type, true);
                if declared {
                    request
                        .headers_mut()
                        .insert(header::CONTENT_LENGTH, wire.len().into());
                }
                let (status, envelope) = response_json(app.oneshot(request).await?).await?;
                assert_eq!(
                    status, expected,
                    "{path} declared={declared} limit={limit}: {envelope}"
                );
                if expected == StatusCode::NOT_FOUND {
                    assert_missing_model(status, &envelope, path);
                } else {
                    assert!(
                        envelope.get("error").is_some_and(Value::is_object),
                        "{envelope}"
                    );
                }
            }
        }
    }
    Ok(())
}

// wire cap 穿过 io/axum 适配层后仍必须保有 LengthLimitError，特别是 multipart。
#[tokio::test]
async fn request_decode_wire_limit_remains_413_with_and_without_declared_length() -> TestResult {
    for (path, wire, content_type, limit) in [
        (
            "/v1/responses",
            RESPONSES_ZSTD,
            "application/json",
            RESPONSES_JSON.len(),
        ),
        (
            "/v1/messages",
            MESSAGES_RAW_ZSTD,
            "application/json",
            MESSAGES_PLAIN.len(),
        ),
        (
            "/v1/files",
            MULTIPART_RAW_ZSTD,
            MULTIPART_TYPE,
            MULTIPART_PLAIN.len(),
        ),
    ] {
        assert!(wire.len() > limit);
        for declared in [false, true] {
            let mut request = body_request(path, chunked(wire), content_type, true);
            if declared {
                request
                    .headers_mut()
                    .insert(header::CONTENT_LENGTH, wire.len().into());
            }
            let response = crate::build_router(admitted_state(limit)?)
                .oneshot(request)
                .await?;
            let (status, envelope) = response_json(response).await?;
            assert_eq!(
                status,
                StatusCode::PAYLOAD_TOO_LARGE,
                "{path} declared={declared}: {envelope}"
            );
            assert!(
                envelope.get("error").is_some_and(Value::is_object),
                "{envelope}"
            );
            if path == "/v1/messages" {
                assert_eq!(envelope["type"].as_str(), Some("error"));
                assert_eq!(
                    envelope["error"]["type"].as_str(),
                    Some("request_too_large")
                );
            }
        }
    }
    Ok(())
}

// zero 不能变成框架的 2 MiB 默认或新增的压缩比限制。
#[tokio::test]
async fn request_decode_zero_limit_accepts_large_high_ratio_requests() -> TestResult {
    const { assert!(LARGE_RESPONSES_LEN > 2 * 1024 * 1024) };
    assert!(LARGE_RESPONSES_LEN > LARGE_RESPONSES_ZSTD.len() * 100);
    for declared in [false, true] {
        let mut request = body_request(
            "/v1/responses",
            chunked(LARGE_RESPONSES_ZSTD),
            "application/json",
            true,
        );
        if declared {
            request
                .headers_mut()
                .insert(header::CONTENT_LENGTH, LARGE_RESPONSES_ZSTD.len().into());
        }
        let response = crate::build_router(admitted_state(0)?)
            .oneshot(request)
            .await?;
        let (status, envelope) = response_json(response).await?;
        assert_missing_model(status, &envelope, "/v1/responses");
    }
    let response = transport_router(0)?
        .oneshot(body_request(
            "/capture",
            Body::from(LARGE_RESPONSES_ZSTD),
            "application/json",
            true,
        ))
        .await?;
    let (status, envelope) = response_json(response).await?;
    assert_eq!(status, StatusCode::OK);
    let expected = format!(
        "{{\"model\":\"request-decode-absent\",\"input\":\"{}\"}}",
        "x".repeat(2 * 1024 * 1024 + 512 * 1024)
    );
    assert_eq!(captured_bytes(&envelope)?, expected.as_bytes());
    Ok(())
}

fn counted_body(polls: Arc<AtomicUsize>) -> Body {
    let mut bytes = Some(Bytes::from_static(b"invalid-zstd-must-stay-unread"));
    Body::from_stream(futures::stream::poll_fn(move |_| {
        polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(bytes.take().map(Ok::<_, std::io::Error>))
    }))
}

// 认证先于首次请求体读取；HTTP/2 与 Expect 路径排除既有 HTTP/1 后台 drain。
#[tokio::test]
async fn request_decode_auth_denials_do_not_poll_compressed_body() -> TestResult {
    let app = admitted_router()?;
    for path in ["/v1/responses", "/v1/messages", "/v1/files"] {
        for expect in [false, true] {
            let polls = Arc::new(AtomicUsize::new(0));
            let mut request = body_request(
                path,
                counted_body(polls.clone()),
                if path == "/v1/files" {
                    MULTIPART_TYPE
                } else {
                    "application/json"
                },
                true,
            );
            request.headers_mut().insert(
                header::AUTHORIZATION,
                "Bearer invalid-transport-key".parse()?,
            );
            if expect {
                *request.version_mut() = Version::HTTP_11;
                request
                    .headers_mut()
                    .insert(header::EXPECT, "100-continue".parse()?);
            }
            let response = app.clone().oneshot(request).await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(polls.load(Ordering::SeqCst), 0, "{path} expect={expect}");
        }
    }
    Ok(())
}

// count_tokens 不能退回 Result 认证提取器：401/403 都应在 JSON 读取前短路且只记账一次。
#[tokio::test]
async fn request_decode_count_tokens_auth_denials_no_poll() -> TestResult {
    let labels = regex::Regex::new(r#"([a-z_]+)="([^"]*)""#)?;
    for (status, error_type, auth_result, auth_reason) in [
        (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "denied",
            "unknown_key",
        ),
        (StatusCode::FORBIDDEN, "permission_error", "allowed", "none"),
    ] {
        for expect in [false, true] {
            let state = admitted_state(4096)?;
            if status == StatusCode::FORBIDDEN {
                let key: ApiKey = serde_json::from_value(json!({
                    "key_hash": ApiKey::hash_bearer("sk-request-decode-test"),
                    "allowed_models": ["*"],
                    "allowed_endpoints": ["models"],
                }))?;
                state.snapshot.load().apikeys.insert(ResourceEntry::new(
                    "request-decode-key",
                    key,
                    2,
                ));
            }
            let metrics = state.metrics.clone();
            let polls = Arc::new(AtomicUsize::new(0));
            let mut request = body_request(
                "/v1/messages/count_tokens",
                counted_body(polls.clone()),
                "application/json",
                true,
            );
            if status == StatusCode::UNAUTHORIZED {
                request.headers_mut().insert(
                    header::AUTHORIZATION,
                    "Bearer invalid-count-tokens-key".parse()?,
                );
            }
            if expect {
                *request.version_mut() = Version::HTTP_11;
                request
                    .headers_mut()
                    .insert(header::EXPECT, "100-continue".parse()?);
            }
            let (actual_status, envelope) =
                response_json(crate::build_router(state).oneshot(request).await?).await?;
            assert_eq!(actual_status, status, "expect={expect}: {envelope}");
            assert_eq!(envelope["type"].as_str(), Some("error"));
            assert_eq!(envelope["error"]["type"].as_str(), Some(error_type));
            assert!(envelope["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()));
            tokio::task::yield_now().await;
            assert_eq!(polls.load(Ordering::SeqCst), 0, "{status} expect={expect}");

            let rendered = metrics.render();
            for name in [
                "aisix_auth_decisions_total",
                "aisix_proxy_requests_total",
                "aisix_proxy_failed_requests_total",
            ] {
                let prefix = format!("{name}{{");
                let samples: Vec<_> = rendered
                    .lines()
                    .filter(|line| line.starts_with(&prefix))
                    .collect();
                assert_eq!(samples.len(), 1, "{name}: {rendered}");
                let (sample, count) = samples[0]
                    .rsplit_once(' ')
                    .ok_or("metric sample omitted its count")?;
                assert_eq!(count.parse::<u64>()?, 1, "{name}: {rendered}");
                let labels: std::collections::BTreeMap<_, _> = labels
                    .captures_iter(sample)
                    .map(|label| (label[1].to_owned(), label[2].to_owned()))
                    .collect();
                if name == "aisix_auth_decisions_total" {
                    assert_eq!(labels.get("method").map(String::as_str), Some("api_key"));
                    assert_eq!(labels.get("result").map(String::as_str), Some(auth_result));
                    assert_eq!(labels.get("reason").map(String::as_str), Some(auth_reason));
                } else {
                    assert_eq!(
                        labels.get("endpoint").map(String::as_str),
                        Some("/v1/messages/count_tokens")
                    );
                    assert_eq!(
                        labels.get("status").map(String::as_str),
                        Some(status.as_str())
                    );
                }
            }
        }
    }
    Ok(())
}

// 无请求体的已注册接口保持惰性，即使调用方错误声明压缩也不预读。
#[tokio::test]
async fn request_decode_body_ignoring_routes_do_not_poll() -> TestResult {
    let app = admitted_router()?;
    for path in ["/livez", "/v1/models"] {
        for zstd in [false, true] {
            let polls = Arc::new(AtomicUsize::new(0));
            let mut request =
                body_request(path, counted_body(polls.clone()), "application/json", zstd);
            *request.method_mut() = axum::http::Method::GET;
            let response = app.clone().oneshot(request).await?;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(polls.load(Ordering::SeqCst), 0, "{path} zstd={zstd}");
        }
    }
    Ok(())
}

// 资源不存在也应先于解码拒绝，避免为一个不可达的资源读取任意请求体。
#[tokio::test]
async fn request_decode_unknown_resources_do_not_poll() -> TestResult {
    let app = admitted_router()?;
    for path in [
        "/mcp/missing-transport-server",
        "/a2a/missing-transport-agent",
    ] {
        let polls = Arc::new(AtomicUsize::new(0));
        let response = app
            .clone()
            .oneshot(body_request(
                path,
                counted_body(polls.clone()),
                "application/json",
                true,
            ))
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(polls.load(Ordering::SeqCst), 0, "{path}");
    }
    Ok(())
}

// 覆盖每个不消费请求体的注册；错误状态可各异，但压缩声明不能改变其决策或触发读取。
#[tokio::test]
async fn request_decode_all_ignored_method_paths_keep_their_decision_without_polling() -> TestResult
{
    let app = admitted_router()?;
    for (template, method) in BODY_AND_IGNORED_SURFACES {
        let method = match *method {
            "GET" | "DELETE" => *method,
            "POST" if template.ends_with("/cancel") => "POST",
            "ANY" if template.starts_with("/.well-known/") => "GET",
            _ => continue,
        };
        let path = template
            .replace(":id", "missing-transport-id")
            .replace(":server", "missing-transport-server")
            .replace(":agent", "missing-transport-agent")
            .replace(":version", "1");
        let mut plain_status = None;
        for zstd in [false, true] {
            let polls = Arc::new(AtomicUsize::new(0));
            let mut request =
                body_request(&path, counted_body(polls.clone()), "application/json", zstd);
            *request.method_mut() = method.parse()?;
            let response = app.clone().oneshot(request).await?;
            assert_eq!(
                polls.load(Ordering::SeqCst),
                0,
                "{method} {path} zstd={zstd}"
            );
            if let Some(status) = plain_status {
                assert_eq!(response.status(), status, "{method} {path}");
            } else {
                plain_status = Some(response.status());
            }
        }
    }
    Ok(())
}

// 手动读取请求体的 RPC 接口也必须解析成功并触发明确的本地阻断决策。
#[tokio::test]
async fn request_decode_mcp_and_a2a_reach_the_guardrail() -> TestResult {
    for (path, plain, encoded) in [
        ("/mcp", MCP_ALL_PLAIN, MCP_ALL_ZSTD),
        ("/mcp/", MCP_ALL_PLAIN, MCP_ALL_ZSTD),
        ("/mcp/census", MCP_SCOPED_PLAIN, MCP_SCOPED_ZSTD),
        ("/a2a/census", A2A_PLAIN, A2A_ZSTD),
    ] {
        let (app, _usage) = crate::guardrail_coverage::census_router_with_usage();
        for (wire, zstd) in [(plain, false), (encoded, true)] {
            let mut request = body_request(path, Body::from(wire), "application/json", zstd);
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, "Bearer sk-census-caller".parse()?);
            request
                .headers_mut()
                .insert(header::HOST, "census.aisix.example.com".parse()?);
            request.headers_mut().insert(
                header::ACCEPT,
                "application/json, text/event-stream".parse()?,
            );
            let response = app.clone().oneshot(request).await?;
            let bytes = to_bytes(response.into_body(), 16384).await?;
            let text = std::str::from_utf8(&bytes)?;
            assert!(
                text.contains("guardrail 'census-block'"),
                "{path} zstd={zstd}: {text}"
            );
            let envelope: Value = serde_json::from_slice(&bytes)?;
            if path.starts_with("/mcp") {
                assert_eq!(
                    envelope["result"]["isError"].as_bool(),
                    Some(true),
                    "{envelope}"
                );
            } else {
                assert!(envelope["error"].is_object(), "{envelope}");
            }
        }
    }
    Ok(())
}

fn oauth_state() -> Result<crate::ProxyState, serde_json::Error> {
    let state = admitted_state(1_048_576)?;
    let snapshot = state.snapshot.load();
    let server: aisix_core::McpServer = serde_json::from_value(json!({
        "name": "main",
        "url": "http://127.0.0.1:1/mcp",
        "enabled": true,
        "auth_type": "oauth2_authorization_code",
    }))?;
    snapshot
        .mcp_servers
        .insert(ResourceEntry::new("transport-mcp-main", server, 1));
    let settings: aisix_core::models::McpAuthSettings = serde_json::from_value(json!({
        "resource_url": "https://gateway.example/mcp",
    }))?;
    snapshot
        .mcp_auth_settings
        .insert(ResourceEntry::new("transport-mcp-settings", settings, 1));
    drop(snapshot);
    Ok(state)
}

// OAuth JSON 注册必须保留真实字段，表单兑换必须到达真实的未知授权码决策。
#[tokio::test]
async fn request_decode_oauth_registration_and_token_forms_are_decoded() -> TestResult {
    let app = crate::build_router(oauth_state()?);
    for (wire, zstd) in [(REGISTER_PLAIN, false), (REGISTER_ZSTD, true)] {
        let response = app
            .clone()
            .oneshot(body_request(
                "/mcp/main/oauth/register",
                Body::from(wire),
                "application/json",
                zstd,
            ))
            .await?;
        let (status, envelope) = response_json(response).await?;
        assert_eq!(status, StatusCode::CREATED, "{envelope}");
        assert_eq!(envelope["client_id"].as_str(), Some("main"));
        assert_eq!(
            envelope.get("redirect_uris"),
            Some(&json!(["http://localhost/transport-callback"]))
        );
    }
    for (wire, zstd) in [(TOKEN_PLAIN, false), (TOKEN_ZSTD, true)] {
        let response = app
            .clone()
            .oneshot(body_request(
                "/mcp/main/oauth/token",
                Body::from(wire),
                "application/x-www-form-urlencoded",
                zstd,
            ))
            .await?;
        let (status, envelope) = response_json(response).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{envelope}");
        assert_eq!(
            envelope["error"].as_str(),
            Some("invalid_grant"),
            "{envelope}"
        );
    }
    Ok(())
}

// 一个已签名的合法 consent 流程才能取消并重定向；压缩乱码不能满足 flow/CSRF。
#[tokio::test]
async fn request_decode_oauth_consent_form_reaches_cancel_redirect() -> TestResult {
    use aisix_mcp::user_oauth::{FlowStage, FlowState};
    for (wire, zstd) in [(CONSENT_PLAIN, false), (CONSENT_ZSTD, true)] {
        let state = oauth_state()?;
        let keys = state.mcp_oauth.session_keys_for_sign_in().await?;
        let flow = FlowState {
            handle: "a".repeat(64),
            expires_at: chrono::Utc::now().timestamp() + 600,
            server_id: "transport-mcp-main".into(),
            server_name: "main".into(),
            client_id: "main".into(),
            redirect_uri: "http://localhost/transport-callback".into(),
            client_state: Some("transport-state".into()),
            code_challenge: "transport-challenge".into(),
            csrf: "transport-csrf".into(),
            stage: FlowStage::Consent {
                signed_in: None,
                relayed: None,
            },
        };
        let cookie = format!("mcp_oauth_flow_{}={}", flow.handle, keys.seal_flow(&flow));
        let mut request = body_request(
            "/mcp-oauth/consent",
            Body::from(wire),
            "application/x-www-form-urlencoded",
            zstd,
        );
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse()?);
        let response = crate::build_router(state).oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(header::LOCATION)
            .ok_or("missing consent redirect")?
            .to_str()?;
        let location = url::Url::parse(location)?;
        assert_eq!(location.host_str(), Some("localhost"));
        assert_eq!(location.path(), "/transport-callback");
        let params: std::collections::BTreeMap<_, _> =
            location.query_pairs().into_owned().collect();
        assert_eq!(
            params.get("error").map(String::as_str),
            Some("access_denied")
        );
        assert_eq!(
            params.get("state").map(String::as_str),
            Some("transport-state")
        );
    }
    Ok(())
}

struct EchoBackend {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl EchoBackend {
    async fn start() -> Result<Self, std::io::Error> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let app = axum::Router::new().fallback(capture_transport);
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(Self { address, task })
    }
}

impl Drop for EchoBackend {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// 真实本地 echo 后端回报实收字节；它不是供应商端到端验收，也不生成模型答复。
#[tokio::test]
async fn request_decode_raw_path_and_foreign_host_relay_keep_encoded_entity() -> TestResult {
    let backend = EchoBackend::start().await?;
    let state = admitted_state(4096)?;
    let snapshot = state.snapshot.load();
    for (id, selector) in [
        (
            "transport-path-relay",
            json!({"path_prefix":"/transport-raw"}),
        ),
        (
            "transport-host-relay",
            json!({"hosts":["foreign.transport.example"]}),
        ),
    ] {
        let mut row = json!({
            "name": id,
            "target_url": format!("http://{}", backend.address),
            "credential_mode": "forward_client",
        });
        row.as_object_mut()
            .unwrap()
            .extend(selector.as_object().unwrap().clone());
        let route: aisix_core::PassthroughRoute = serde_json::from_value(row)?;
        snapshot
            .passthrough_routes
            .insert(ResourceEntry::new(id, route, 1));
    }
    drop(snapshot);
    let app = crate::build_router(state);
    for (path, host) in [
        ("/transport-raw/echo", "gateway.transport.example"),
        ("/v1/responses", "foreign.transport.example"),
    ] {
        let mut request = body_request(path, Body::from(RESPONSES_ZSTD), "application/json", true);
        request.headers_mut().insert(header::HOST, host.parse()?);
        request
            .headers_mut()
            .insert(header::CONTENT_LENGTH, RESPONSES_ZSTD.len().into());
        let (status, envelope) = response_json(app.clone().oneshot(request).await?).await?;
        assert_eq!(status, StatusCode::OK, "{path}: {envelope}");
        assert_eq!(captured_bytes(&envelope)?, RESPONSES_ZSTD, "{path}");
        assert_eq!(
            envelope["content_encoding"].as_str(),
            Some(STANDARD.encode("zstd").as_str()),
            "{path}"
        );
        assert_eq!(
            envelope["content_type"].as_str(),
            Some(STANDARD.encode("application/json").as_str()),
            "{path}"
        );
    }
    Ok(())
}

// 固定官方 SDK 3dae6090cc31cc64a437fe0bcee740b6d81ed2cc 的 active 原生正文合同。
// CCV 的引用入口：resources/conversations、containers、vector_stores；
// Skills/Voices：resources/skills、audio/voice_consents、audio/voices。
// 仅完整 HTTP 组件与真实本地 SQLite；本地协议替身不代表提供商 E2E。
#[derive(Clone, Copy)]
enum NativeBodyForm {
    Json,
    Multipart,
    JsonAndMultipart,
}

struct NativeBodyCase {
    route: &'static str,
    form: NativeBodyForm,
}

const NATIVE_BODY_CASES: &[NativeBodyCase] = &[
    NativeBodyCase {
        route: "/v1/conversations",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/conversations/:id",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/conversations/:id/items",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/containers",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/containers/:id/files",
        form: NativeBodyForm::JsonAndMultipart,
    },
    NativeBodyCase {
        route: "/v1/vector_stores",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/vector_stores/:id",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/vector_stores/:id/search",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/vector_stores/:id/files",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/vector_stores/:id/files/:file_id",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/vector_stores/:id/file_batches",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/skills",
        form: NativeBodyForm::Multipart,
    },
    NativeBodyCase {
        route: "/v1/skills/:id",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/skills/:id/versions",
        form: NativeBodyForm::Multipart,
    },
    NativeBodyCase {
        route: "/v1/audio/voice_consents",
        form: NativeBodyForm::Multipart,
    },
    NativeBodyCase {
        route: "/v1/audio/voice_consents/:id",
        form: NativeBodyForm::Json,
    },
    NativeBodyCase {
        route: "/v1/audio/voices",
        form: NativeBodyForm::JsonAndMultipart,
    },
];

const NATIVE_BAD_JSON_ZSTD: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x01, 0x09, 0x00, 0x00, 0x7b];
const NATIVE_BAD_MULTIPART: &[u8] = b"--transport-native\r\nnot-a-valid-multipart-header\r\n\r\nsentinel\r\n--transport-native--\r\n";
const NATIVE_BAD_MULTIPART_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x54, 0xa1, 0x02, 0x00, 0x2d, 0x2d, 0x74, 0x72, 0x61, 0x6e, 0x73,
    0x70, 0x6f, 0x72, 0x74, 0x2d, 0x6e, 0x61, 0x74, 0x69, 0x76, 0x65, 0x0d, 0x0a, 0x6e, 0x6f, 0x74,
    0x2d, 0x61, 0x2d, 0x76, 0x61, 0x6c, 0x69, 0x64, 0x2d, 0x6d, 0x75, 0x6c, 0x74, 0x69, 0x70, 0x61,
    0x72, 0x74, 0x2d, 0x68, 0x65, 0x61, 0x64, 0x65, 0x72, 0x0d, 0x0a, 0x0d, 0x0a, 0x73, 0x65, 0x6e,
    0x74, 0x69, 0x6e, 0x65, 0x6c, 0x0d, 0x0a, 0x2d, 0x2d, 0x74, 0x72, 0x61, 0x6e, 0x73, 0x70, 0x6f,
    0x72, 0x74, 0x2d, 0x6e, 0x61, 0x74, 0x69, 0x76, 0x65, 0x2d, 0x2d, 0x0d, 0x0a,
];

const NATIVE_LIMIT_MULTIPART_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x60, 0x9d, 0x0f, 0x85, 0x03, 0x00, 0xb2, 0xc6, 0x15, 0x18, 0x90, 0x29,
    0x6d, 0xf8, 0x2b, 0x11, 0xc8, 0x5e, 0x58, 0x58, 0xe5, 0xb7, 0xa6, 0x46, 0xe6, 0xbe, 0x02, 0xfb,
    0x37, 0x40, 0x72, 0xbd, 0xc4, 0xcc, 0x65, 0x77, 0x65, 0x8a, 0xb2, 0xb2, 0x83, 0x89, 0x9e, 0xee,
    0x79, 0x84, 0x03, 0x88, 0x28, 0xa2, 0x1b, 0x8b, 0x91, 0xbd, 0x28, 0x98, 0x55, 0x03, 0xa0, 0x1c,
    0x26, 0x11, 0x9a, 0x18, 0x41, 0x1e, 0xdc, 0x72, 0xe0, 0xf1, 0x70, 0x07, 0x71, 0x52, 0x8c, 0x65,
    0xbd, 0x26, 0xee, 0xa4, 0x84, 0xfb, 0xe6, 0x97, 0x07, 0x65, 0x99, 0xd0, 0xe3, 0xde, 0x11, 0x77,
    0x5a, 0x44, 0xbf, 0x0b, 0x07, 0x00, 0x8a, 0x70, 0x3c, 0x8c, 0xff, 0xb8, 0x87, 0x50, 0xb0, 0xaf,
    0x04, 0x54, 0x26, 0xe8, 0xa0, 0x39, 0x10, 0x28, 0x28, 0x66,
];

fn native_transport_path(route: &str) -> String {
    route
        .replace(":id", "container-transport")
        .replace(":file_id", "file-transport")
        .replace(":item_id", "item-transport")
        .replace(":batch_id", "batch-transport")
        .replace(":version", "1")
}

struct NativeDecodeFixture {
    app: axum::Router,
    provider: wiremock::MockServer,
    _directory: tempfile::TempDir,
}

impl NativeDecodeFixture {
    async fn new(limit: usize) -> Result<Self, Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        use crate::native_api::registry::NativeObjectRegistry;
        use crate::native_api::standalone_store::{stable_resource_id, StandaloneNativeStore};
        use aisix_core::native_objects::{
            configured_native_origin, native_origin_surface, BeginRequest, BindRequest,
            NativeObjectKind, NativeProtocol, OriginBinding,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const MODEL: &str = "81111111-1111-4111-8111-111111111111";
        const PROVIDER: &str = "82222222-2222-4222-8222-222222222222";
        const SECRET: [u8; 32] = [47; 32];
        let provider = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/v1/containers/container-transport"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"container-transport", "object":"container", "name":"transport",
                "created_at":1730000000, "status":"running"
            })))
            .mount(&provider)
            .await;
        let state = admitted_state(limit)?;
        let provider_doc: aisix_core::ProviderKey = serde_json::from_value(json!({
            "display_name":"transport-provider", "provider":"openai", "adapter":"openai",
            "api_key":"transport-provider-test-only", "api_base":format!("{}/v1", provider.uri())
        }))?;
        let model: aisix_core::Model = serde_json::from_value(json!({
            "display_name":"request-decode-native", "provider_key_id":PROVIDER,
            "model_name":"transport-upstream"
        }))?;
        {
            let snapshot = state.snapshot.load();
            snapshot
                .provider_keys
                .insert(ResourceEntry::new(PROVIDER, provider_doc.clone(), 1));
            snapshot.models.insert(ResourceEntry::new(MODEL, model, 1));
        }
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let store = StandaloneNativeStore::open(
            &directory.path().join("authority.sqlite3"),
            SECRET,
            state.snapshot.clone(),
        )
        .await?;
        let kind = NativeObjectKind::ResponseContainer;
        let origin = OriginBinding {
            surface: Some(native_origin_surface(NativeProtocol::Openai, kind)),
            model_id: MODEL.parse()?,
            provider_key_id: PROVIDER.parse()?,
            boundary: configured_native_origin(
                &SECRET,
                PROVIDER.parse()?,
                &provider_doc,
                NativeProtocol::Openai,
                kind,
            )?,
        };
        let operation_id = uuid::Uuid::new_v4();
        let api_key_id = stable_resource_id("apikeys", "request-decode-key");
        let accepted = store
            .begin(BeginRequest {
                upstream_input_text: None,
                input_file_id: None,
                operation_id,
                api_key_id,
                protocol: NativeProtocol::Openai,
                kind,
                origin,
                parent_id: None,
                requested_model_id: MODEL.parse()?,
                requested_model: "request-decode-native".into(),
                upstream_model: "transport-upstream".into(),
                upstream_provider: "openai".into(),
                operation: "containers".into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                asynchronous: false,
                requested_service_tier: None,
                entries: Vec::new(),
                observation_headers: Default::default(),
            })
            .await?;
        assert_eq!(accepted.operation_id, Some(operation_id));
        let bound = store
            .bind(BindRequest {
                existing_record_id: None,
                objects: Vec::new(),
                operation_id,
                api_key_id,
                raw_id: "container-transport".into(),
                provider_created_at: None,
                provider_expires_at: None,
            })
            .await?;
        assert_eq!(
            bound.record.as_ref().map(|record| record.raw_id.as_str()),
            Some("container-transport")
        );
        let app = crate::build_router(state.with_native_objects(Arc::new(store)));
        // 先证明同一 key、真实归属、模型与账号确实能走到 provider，后续零增量才非空验证。
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/containers/container-transport")
                    .header(header::AUTHORIZATION, "Bearer sk-request-decode-test")
                    .body(Body::empty())?,
            )
            .await?;
        let (status, body) = response_json(response).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "native fixture positive control: {body}"
        );
        assert_eq!(body["id"].as_str(), Some("container-transport"));
        assert_eq!(
            provider
                .received_requests()
                .await
                .ok_or("provider request recording is disabled")?
                .len(),
            1
        );
        Ok(Self {
            app,
            provider,
            _directory: directory,
        })
    }
}

async fn assert_native_body_rejected(
    fixture: &NativeDecodeFixture,
    case: &NativeBodyCase,
    content_type: &str,
    plain: &'static [u8],
    encoded: &'static [u8],
) -> TestResult {
    let before = fixture
        .provider
        .received_requests()
        .await
        .ok_or("provider request recording is disabled")?
        .len();
    assert_eq!(
        before, 1,
        "the real provider control must have run exactly once"
    );
    for (label, bytes, zstd) in [("plain", plain, false), ("zstd", encoded, true)] {
        let path = native_transport_path(case.route);
        let mut request = body_request(&path, Body::from(bytes), content_type, zstd);
        request
            .headers_mut()
            .insert("x-aisix-model", "request-decode-native".parse()?);
        let response = fixture.app.clone().oneshot(request).await?;
        let (status, body) = response_json(response).await?;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{} {label}: {body}",
            case.route
        );
        assert_eq!(
            body["error"]["type"].as_str(),
            Some("invalid_request_error"),
            "{} {label}: {body}",
            case.route
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "{} {label}: {body}",
            case.route
        );
        assert_eq!(
            fixture
                .provider
                .received_requests()
                .await
                .ok_or("provider request recording is disabled")?
                .len(),
            before,
            "{} {label}: invalid body reached the provider",
            case.route
        );
    }
    Ok(())
}

// 有效认证与真实父对象不能让畸形 JSON 穿过任何 active 原生写入口。
#[tokio::test]
async fn request_decode_active_native_json_rejects_malformed_body_without_dispatch() -> TestResult {
    let fixture = NativeDecodeFixture::new(1_048_576).await?;
    for case in NATIVE_BODY_CASES.iter().filter(|case| {
        matches!(
            case.form,
            NativeBodyForm::Json | NativeBodyForm::JsonAndMultipart
        )
    }) {
        assert_native_body_rejected(
            &fixture,
            case,
            "application/json",
            b"{",
            NATIVE_BAD_JSON_ZSTD,
        )
        .await?;
    }
    Ok(())
}

// 与 JSON 并列覆盖实际 multipart 消费者，容器文件使用已持久化的 owned parent。
#[tokio::test]
async fn request_decode_active_native_multipart_rejects_malformed_body_without_dispatch(
) -> TestResult {
    let fixture = NativeDecodeFixture::new(1_048_576).await?;
    for case in NATIVE_BODY_CASES.iter().filter(|case| {
        matches!(
            case.form,
            NativeBodyForm::Multipart | NativeBodyForm::JsonAndMultipart
        )
    }) {
        assert_native_body_rejected(
            &fixture,
            case,
            "multipart/form-data; boundary=transport-native",
            NATIVE_BAD_MULTIPART,
            NATIVE_BAD_MULTIPART_ZSTD,
        )
        .await?;
    }
    Ok(())
}

// wire 小于上限而完整实体大于上限；漏掉解压只能读到乱码 400，不能满足这里的 413。
// 每个真实正文入口都执行两种 framing 控制，不以畸形 plain/zstd 同报 400 假称已解码。
#[tokio::test]
async fn request_decode_active_native_decoded_limits_reject_without_dispatch() -> TestResult {
    const LIMIT: usize = 512;
    let fixture = NativeDecodeFixture::new(LIMIT).await?;
    let padding = vec![b'x'; 4096];
    let json = [
        br#"{"model":"request-decode-absent","input":""#.as_slice(),
        padding.as_slice(),
        br#""}"#.as_slice(),
    ]
    .concat();
    let multipart = [
        b"--transport-native\r\nContent-Disposition: form-data; name=\"file\"; filename=\"transport.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n".as_slice(),
        padding.as_slice(),
        b"\r\n--transport-native--\r\n".as_slice(),
    ].concat();
    assert_eq!(json.len(), PADDED_RESPONSES_LEN);
    let json_control: Value = serde_json::from_slice(&json)?;
    assert!(json_control.is_object());
    for case in NATIVE_BODY_CASES {
        for (is_json, content_type, plain, encoded) in [
            (
                true,
                "application/json",
                json.as_slice(),
                PADDED_RESPONSES_ZSTD,
            ),
            (
                false,
                "multipart/form-data; boundary=transport-native",
                multipart.as_slice(),
                NATIVE_LIMIT_MULTIPART_ZSTD,
            ),
        ] {
            if matches!(
                (case.form, is_json),
                (NativeBodyForm::Json, false) | (NativeBodyForm::Multipart, true)
            ) {
                continue;
            }
            assert!(
                encoded.len() < LIMIT,
                "{}: encoded control exceeds wire cap",
                case.route
            );
            assert!(
                plain.len() >= 4 * 1024,
                "{}: decoded control must contain at least 4KiB",
                case.route
            );
            assert!(
                plain.len() > LIMIT,
                "{}: decoded control is within cap",
                case.route
            );
            for (label, bytes, zstd) in [("plain", plain, false), ("zstd", encoded, true)] {
                let path = native_transport_path(case.route);
                let mut request =
                    body_request(&path, Body::from(bytes.to_vec()), content_type, zstd);
                request
                    .headers_mut()
                    .insert("x-aisix-model", "request-decode-native".parse()?);
                let response = fixture.app.clone().oneshot(request).await?;
                let (status, body) = response_json(response).await?;
                assert_eq!(
                    status,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "{} {content_type} {label}: {body}",
                    case.route
                );
                assert!(
                    body.get("error").is_some_and(Value::is_object),
                    "{} {label}: {body}",
                    case.route
                );
                assert_eq!(
                    fixture
                        .provider
                        .received_requests()
                        .await
                        .ok_or("provider request recording is disabled")?
                        .len(),
                    1,
                    "{} {label}: oversized decoded body reached provider",
                    case.route
                );
            }
        }
    }
    Ok(())
}

// 路径和 method 都来自实际注册表；新增任何接口必须补齐可解码 fixture 或无体说明。
#[test]
fn request_decode_census_covers_every_registered_method_and_path() {
    let source = include_str!("lib.rs");
    let body = source
        .split_once("pub fn build_router(")
        .unwrap()
        .1
        .split_once("\n}\n")
        .unwrap()
        .0;
    let routes = regex::Regex::new(r#"\.route\(\s*"([^"]+)"\s*,"#).unwrap();
    let methods = regex::Regex::new(
        r"(?:^|\.)\s*(?:axum::routing::)?(get|post|put|patch|delete|head|options|trace|any)\(",
    )
    .unwrap();
    let mut mounted = BTreeSet::new();
    for capture in routes.captures_iter(body) {
        let matched = capture.get(0).unwrap();
        let rest = &body[matched.end()..];
        let mut depth = 1usize;
        let end = rest
            .char_indices()
            .find_map(|(index, ch)| {
                match ch {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(index)
            })
            .expect("route registration must end");
        let expression = rest[..end].trim();
        let parsed: Vec<_> = methods
            .captures_iter(expression)
            .map(|method| method[1].to_uppercase())
            .collect();
        assert!(
            !parsed.is_empty(),
            "unrecognised methods for {}: {expression}",
            &capture[1]
        );
        for method in parsed {
            mounted.insert((capture[1].to_owned(), method));
        }
    }
    let mut covered = BTreeSet::new();
    for case in JSON_CASES {
        covered.insert((case.path.to_owned(), "POST".to_owned()));
    }
    for case in NATIVE_BODY_CASES {
        covered.insert((case.route.to_owned(), "POST".to_owned()));
    }
    for (path, method) in BODY_AND_IGNORED_SURFACES {
        covered.insert(((*path).into(), (*method).into()));
    }
    assert!(!mounted.is_empty());
    assert_eq!(
        mounted, covered,
        "every registered method/path needs an explicit transport test posture"
    );
}

// ANY 记录注册器本身；MCP 的 POST 语义由 RPC 用例覆盖，其余 method 保持既有处理。
// GET/DELETE/取消/发现/健康接口不消费请求体，完整 typed route_layer 仍覆盖这些注册。
const BODY_AND_IGNORED_SURFACES: &[(&str, &str)] = &[
    // 官方 GET/DELETE/取消方法不带正文，继续复用严格 no-poll 合同。
    ("/v1/conversations/:id", "GET"),
    ("/v1/conversations/:id", "DELETE"),
    ("/v1/conversations/:id/items", "GET"),
    ("/v1/conversations/:id/items/:item_id", "GET"),
    ("/v1/conversations/:id/items/:item_id", "DELETE"),
    ("/v1/containers", "GET"),
    ("/v1/containers/:id", "GET"),
    ("/v1/containers/:id", "DELETE"),
    ("/v1/containers/:id/files", "GET"),
    ("/v1/containers/:id/files/:file_id", "GET"),
    ("/v1/containers/:id/files/:file_id", "DELETE"),
    ("/v1/containers/:id/files/:file_id/content", "GET"),
    ("/v1/vector_stores", "GET"),
    ("/v1/vector_stores/:id", "GET"),
    ("/v1/vector_stores/:id", "DELETE"),
    ("/v1/vector_stores/:id/files", "GET"),
    ("/v1/vector_stores/:id/files/:file_id", "GET"),
    ("/v1/vector_stores/:id/files/:file_id", "DELETE"),
    ("/v1/vector_stores/:id/files/:file_id/content", "GET"),
    ("/v1/vector_stores/:id/file_batches/:batch_id", "GET"),
    (
        "/v1/vector_stores/:id/file_batches/:batch_id/cancel",
        "POST",
    ),
    ("/v1/vector_stores/:id/file_batches/:batch_id/files", "GET"),
    ("/v1/skills", "GET"),
    ("/v1/skills/:id", "GET"),
    ("/v1/skills/:id", "DELETE"),
    ("/v1/skills/:id/content", "GET"),
    ("/v1/skills/:id/versions", "GET"),
    ("/v1/skills/:id/versions/:version", "GET"),
    ("/v1/skills/:id/versions/:version", "DELETE"),
    ("/v1/skills/:id/versions/:version/content", "GET"),
    ("/v1/audio/voice_consents", "GET"),
    ("/v1/audio/voice_consents/:id", "GET"),
    ("/v1/audio/voice_consents/:id", "DELETE"),
    ("/v1/audio/voices/:id", "DELETE"),
    // 原生 OpenAI 字段由组件用例验证；全部 typed route_layer 共享已验证的解码层。
    ("/v1/chat/completions", "GET"),
    ("/v1/chat/completions/:id", "GET"),
    ("/v1/chat/completions/:id", "POST"),
    ("/v1/chat/completions/:id", "DELETE"),
    ("/v1/chat/completions/:id/messages", "GET"),
    ("/v1/moderations", "POST"),
    ("/v1/content_provenance_checks", "POST"),
    ("/v1/uploads", "POST"),
    ("/v1/uploads/:id/parts", "POST"),
    ("/v1/uploads/:id/complete", "POST"),
    ("/v1/uploads/:id/cancel", "POST"),
    // WebSocket 握手不消费 HTTP 请求体；每轮 JSON 由帧处理器验证。
    ("/v1/responses", "GET"),
    ("/livez", "GET"),
    ("/readyz", "GET"),
    ("/v1/models", "GET"),
    ("/v1/models/:id", "GET"),
    ("/v1/key", "GET"),
    ("/v1/generation", "GET"),
    ("/v1/images/edits", "POST"),
    ("/v1/audio/transcriptions", "POST"),
    ("/v1/files", "POST"),
    ("/v1/files", "GET"),
    ("/v1/files/:id", "GET"),
    ("/v1/files/:id", "DELETE"),
    ("/v1/files/:id/content", "GET"),
    ("/v1/batches", "GET"),
    ("/v1/batches/:id", "GET"),
    ("/v1/batches/:id/cancel", "POST"),
    // 原生批次的 JSON/zstd 创建路径由 native_anthropic_batches 组件场景覆盖。
    ("/v1/messages/batches", "POST"),
    ("/v1/messages/batches", "GET"),
    ("/v1/messages/batches/:id", "GET"),
    ("/v1/messages/batches/:id", "DELETE"),
    ("/v1/messages/batches/:id/cancel", "POST"),
    ("/v1/messages/batches/:id/results", "GET"),
    ("/v1/videos/:id", "GET"),
    ("/v1/videos/:id/content", "GET"),
    ("/v1/realtime", "GET"),
    ("/v1/realtime/translations", "GET"),
    ("/v1/responses/:id", "GET"),
    ("/v1/responses/:id", "DELETE"),
    ("/v1/responses/:id/cancel", "POST"),
    ("/v1/responses/:id/input_items", "GET"),
    ("/.well-known/oauth-protected-resource", "ANY"),
    ("/.well-known/oauth-protected-resource/mcp", "ANY"),
    ("/.well-known/oauth-protected-resource/mcp/:server", "GET"),
    ("/.well-known/oauth-authorization-server/mcp/:server", "GET"),
    ("/mcp/:server/.well-known/openid-configuration", "GET"),
    ("/mcp/:server/.well-known/jwks.json", "GET"),
    ("/mcp/:server/oauth/register", "POST"),
    ("/mcp/:server/oauth/authorize", "GET"),
    ("/mcp/:server/oauth/token", "POST"),
    ("/mcp/:server/oauth/credential", "DELETE"),
    ("/mcp-oauth/callback", "GET"),
    ("/mcp-oauth/login/callback", "GET"),
    ("/mcp-oauth/consent", "POST"),
    ("/mcp", "ANY"),
    ("/mcp/", "ANY"),
    ("/mcp/:server", "ANY"),
    ("/a2a/:agent", "POST"),
    ("/a2a/:agent/.well-known/agent-card.json", "GET"),
];

// 全部 zstd 字节在写入测试源码前离线生成并独立还原验证。
struct JsonCase {
    path: &'static str,
    plain: &'static [u8],
    encoded: &'static [u8],
}
const JSON_CASES: &[JsonCase] = &[
    JsonCase { path: "/v1/realtime/client_secrets", plain: b"{\"session\":{\"type\":\"realtime\",\"model\":\"request-decode-absent\"}}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x3f,0xf9,0x01,0x00,0x7b,0x22,0x73,0x65,0x73,0x73,0x69,0x6f,0x6e,0x22,0x3a,0x7b,0x22,0x74,0x79,0x70,0x65,0x22,0x3a,0x22,0x72,0x65,0x61,0x6c,0x74,0x69,0x6d,0x65,0x22,0x2c,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x7d,0x7d] },
    JsonCase { path: "/v1/realtime/client_secrets", plain: b"{\"session\":{\"type\":\"transcription\",\"audio\":{\"input\":{\"transcription\":{\"model\":\"request-decode-absent\"}}}}}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x6a,0x51,0x03,0x00,0x7b,0x22,0x73,0x65,0x73,0x73,0x69,0x6f,0x6e,0x22,0x3a,0x7b,0x22,0x74,0x79,0x70,0x65,0x22,0x3a,0x22,0x74,0x72,0x61,0x6e,0x73,0x63,0x72,0x69,0x70,0x74,0x69,0x6f,0x6e,0x22,0x2c,0x22,0x61,0x75,0x64,0x69,0x6f,0x22,0x3a,0x7b,0x22,0x69,0x6e,0x70,0x75,0x74,0x22,0x3a,0x7b,0x22,0x74,0x72,0x61,0x6e,0x73,0x63,0x72,0x69,0x70,0x74,0x69,0x6f,0x6e,0x22,0x3a,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x7d,0x7d,0x7d,0x7d,0x7d] },
    JsonCase { path: "/v1/realtime/translations/client_secrets", plain: b"{\"session\":{\"model\":\"request-decode-absent\",\"audio\":{\"output\":{\"language\":\"en\"}}}}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x52,0x91,0x02,0x00,0x7b,0x22,0x73,0x65,0x73,0x73,0x69,0x6f,0x6e,0x22,0x3a,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x2c,0x22,0x61,0x75,0x64,0x69,0x6f,0x22,0x3a,0x7b,0x22,0x6f,0x75,0x74,0x70,0x75,0x74,0x22,0x3a,0x7b,0x22,0x6c,0x61,0x6e,0x67,0x75,0x61,0x67,0x65,0x22,0x3a,0x22,0x65,0x6e,0x22,0x7d,0x7d,0x7d,0x7d] },
    JsonCase { path: "/v1/responses/compact", plain: RESPONSES_JSON, encoded: RESPONSES_ZSTD },
    JsonCase { path: "/v1/responses/input_tokens", plain: RESPONSES_JSON, encoded: RESPONSES_ZSTD },
    JsonCase { path: "/v1/chat/completions", plain: b"{\"model\":\"request-decode-absent\",\"messages\":[{\"role\":\"user\",\"content\":\"sentinel\"}],\"stream\":false}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x62,0x95,0x02,0x00,0xa2,0xc5,0x11,0x16,0xa0,0xa7,0x39,0x0f,0x68,0x24,0x59,0x10,0x52,0x19,0x7e,0x4b,0xdb,0x02,0x2b,0x80,0x83,0x38,0x63,0xfa,0xdf,0x84,0x87,0x7f,0x26,0xa8,0x1e,0x73,0xa6,0x97,0x00,0x8e,0x9e,0x11,0xa1,0x95,0x10,0x12,0x25,0x4d,0xbf,0x68,0xd5,0x87,0x59,0x63,0xa8,0xde,0x03,0xfb,0x7d,0x4a,0x9a,0x90,0x1f,0xb0,0x62,0x83,0x84,0x4d,0x49,0x77,0x51,0x70,0xb6,0x7a,0x36,0x78,0x6a,0x04,0x02,0x00,0x4f,0x05,0x06,0x28,0x50,0x67] },
    JsonCase { path: "/v1/chat/completions", plain: b"{\"model\":\"request-decode-absent\",\"messages\":[{\"role\":\"user\",\"content\":\"sentinel\"}],\"stream\":true}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x61,0x7d,0x02,0x00,0x92,0x05,0x11,0x14,0xa0,0x57,0x07,0x15,0x10,0x29,0x3e,0xa4,0xdd,0x3e,0x87,0x96,0xdb,0x55,0x09,0xfa,0xe4,0x51,0x15,0x87,0xed,0x2b,0xf7,0x9c,0xca,0x73,0xd5,0x23,0x68,0x46,0x57,0x84,0x6f,0x15,0x1a,0x0f,0xe7,0x75,0xf9,0x7c,0x84,0x69,0x06,0x9c,0xf5,0x43,0x51,0x7d,0xf2,0xf0,0xaa,0xab,0x50,0xc8,0x0f,0x9a,0x1f,0xb2,0xfa,0x55,0x3c,0x7d,0x46,0x3f,0x98,0xcc,0x02,0x02,0x00,0x4f,0x05,0x06,0x28,0x50,0x67] },
    JsonCase { path: "/v1/messages", plain: b"{\"model\":\"request-decode-absent\",\"max_tokens\":16,\"messages\":[{\"role\":\"user\",\"content\":\"sentinel\"}],\"stream\":false}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x72,0xf5,0x02,0x00,0xa2,0xc6,0x14,0x18,0x90,0xa9,0x39,0xf3,0xa8,0x26,0xf9,0x4d,0x68,0xe1,0x81,0x2c,0x6d,0x93,0xf8,0xcd,0xec,0xc4,0x33,0xca,0x7c,0xfd,0x83,0x12,0x89,0x8f,0xce,0x60,0x4d,0xe7,0x95,0x6f,0x19,0x24,0x45,0x56,0x88,0x5a,0x2a,0x34,0x5a,0xba,0xfc,0xa3,0x56,0x23,0x5c,0x45,0x04,0xd6,0xfb,0x70,0xbf,0x53,0x4b,0x00,0x60,0xbd,0x72,0x02,0x93,0xb0,0xb8,0xd4,0xd2,0x54,0x7e,0xe1,0x8c,0x0f,0x1a,0x3e,0x26,0xef,0x43,0xe1,0x55,0x2b,0xf2,0xc1,0x54,0x24,0x02,0x00,0x5f,0x05,0x46,0x38,0x50,0x67] },
    JsonCase { path: "/v1/messages", plain: b"{\"model\":\"request-decode-absent\",\"max_tokens\":16,\"messages\":[{\"role\":\"user\",\"content\":\"sentinel\"}],\"stream\":true}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x71,0xed,0x02,0x00,0x92,0x86,0x14,0x18,0x90,0xa9,0x39,0xf3,0x14,0x91,0xf1,0x43,0x68,0xe1,0xd1,0x26,0x36,0x1a,0xe4,0xff,0x65,0x04,0xb8,0x22,0x4f,0xee,0xa7,0x04,0x89,0x17,0xa6,0x35,0x19,0x63,0xbe,0x45,0x90,0x14,0x59,0x19,0x6a,0xa9,0xee,0x68,0x29,0xfa,0xab,0x56,0xa3,0xc3,0x22,0x01,0xeb,0x5d,0x30,0xbf,0x53,0x0b,0xc0,0xb0,0x5e,0xf9,0x70,0x39,0x48,0x98,0xd4,0xd2,0x54,0x3e,0xc1,0x14,0x3e,0x77,0xf8,0x14,0x79,0x2f,0x84,0xb1,0x56,0xe4,0x73,0xa9,0x48,0x02,0x00,0x5f,0x05,0x46,0x38,0x50,0x67] },
    JsonCase { path: "/v1/responses", plain: b"{\"model\":\"request-decode-absent\",\"input\":\"sentinel\",\"stream\":false}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x43,0x19,0x02,0x00,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x2c,0x22,0x69,0x6e,0x70,0x75,0x74,0x22,0x3a,0x22,0x73,0x65,0x6e,0x74,0x69,0x6e,0x65,0x6c,0x22,0x2c,0x22,0x73,0x74,0x72,0x65,0x61,0x6d,0x22,0x3a,0x66,0x61,0x6c,0x73,0x65,0x7d] },
    JsonCase { path: "/v1/responses", plain: b"{\"model\":\"request-decode-absent\",\"input\":\"sentinel\",\"stream\":true}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x42,0x11,0x02,0x00,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x2c,0x22,0x69,0x6e,0x70,0x75,0x74,0x22,0x3a,0x22,0x73,0x65,0x6e,0x74,0x69,0x6e,0x65,0x6c,0x22,0x2c,0x22,0x73,0x74,0x72,0x65,0x61,0x6d,0x22,0x3a,0x74,0x72,0x75,0x65,0x7d] },
    JsonCase { path: "/v1/completions", plain: b"{\"model\":\"request-decode-absent\",\"prompt\":\"sentinel\"}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x35,0xa9,0x01,0x00,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x2c,0x22,0x70,0x72,0x6f,0x6d,0x70,0x74,0x22,0x3a,0x22,0x73,0x65,0x6e,0x74,0x69,0x6e,0x65,0x6c,0x22,0x7d] },
    JsonCase { path: "/v1/embeddings", plain: b"{\"model\":\"request-decode-absent\",\"input\":\"sentinel\"}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x34,0xa1,0x01,0x00,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x2c,0x22,0x69,0x6e,0x70,0x75,0x74,0x22,0x3a,0x22,0x73,0x65,0x6e,0x74,0x69,0x6e,0x65,0x6c,0x22,0x7d] },
    JsonCase { path: "/v1/images/generations", plain: b"{\"model\":\"request-decode-absent\",\"prompt\":\"sentinel\"}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x35,0xa9,0x01,0x00,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x2c,0x22,0x70,0x72,0x6f,0x6d,0x70,0x74,0x22,0x3a,0x22,0x73,0x65,0x6e,0x74,0x69,0x6e,0x65,0x6c,0x22,0x7d] },
    JsonCase { path: "/v1/messages/count_tokens", plain: b"{\"model\":\"request-decode-absent\",\"messages\":[{\"role\":\"user\",\"content\":\"sentinel\"}]}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x53,0x45,0x02,0x00,0xb2,0x44,0x0f,0x14,0xa0,0x57,0x07,0x15,0xa4,0xd9,0x56,0xda,0xed,0x73,0x68,0xb9,0x5d,0x95,0xa0,0x4f,0x1e,0x55,0x71,0x08,0x2f,0x78,0x45,0x56,0x44,0x91,0xa9,0x34,0x1d,0x5d,0x7e,0x2b,0xd6,0x28,0x57,0x1d,0x80,0xbd,0x87,0xf2,0x3b,0x75,0x94,0x29,0x5f,0x28,0xc4,0x27,0x8d,0x0f,0x61,0x77,0x2b,0xbc,0x8a,0x45,0x3e,0x99,0xea,0x02,0x02,0x00,0x4f,0x05,0x06,0x28,0x50,0x67] },
    JsonCase { path: "/v1/rerank", plain: b"{\"model\":\"request-decode-absent\",\"query\":\"sentinel\",\"documents\":[\"document\"]}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x4d,0x1d,0x02,0x00,0x02,0x04,0x0e,0x15,0xa0,0xb7,0x0d,0xf6,0x01,0x89,0x74,0xca,0xe6,0x87,0x53,0x28,0x31,0x01,0x38,0xba,0x09,0xf3,0x3f,0x0c,0x01,0x47,0x60,0xc0,0xac,0x8b,0x47,0xcb,0xe4,0x99,0xf8,0x1c,0x09,0x9b,0x0b,0xf5,0x95,0x4c,0xbc,0x78,0x1d,0x84,0xf2,0x4b,0xf3,0x2b,0xab,0x5f,0xe9,0xca,0xe6,0xf3,0x4b,0x64,0x23,0x02,0x00,0x61,0x56,0x90,0x04,0x53,0x0f] },
    JsonCase { path: "/v1/audio/speech", plain: b"{\"model\":\"request-decode-absent\",\"input\":\"sentinel\",\"voice\":\"alloy\"}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x44,0x21,0x02,0x00,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x2c,0x22,0x69,0x6e,0x70,0x75,0x74,0x22,0x3a,0x22,0x73,0x65,0x6e,0x74,0x69,0x6e,0x65,0x6c,0x22,0x2c,0x22,0x76,0x6f,0x69,0x63,0x65,0x22,0x3a,0x22,0x61,0x6c,0x6c,0x6f,0x79,0x22,0x7d] },
    JsonCase { path: "/v1/videos", plain: b"{\"model\":\"request-decode-absent\",\"prompt\":\"a cat\"}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x32,0x91,0x01,0x00,0x7b,0x22,0x6d,0x6f,0x64,0x65,0x6c,0x22,0x3a,0x22,0x72,0x65,0x71,0x75,0x65,0x73,0x74,0x2d,0x64,0x65,0x63,0x6f,0x64,0x65,0x2d,0x61,0x62,0x73,0x65,0x6e,0x74,0x22,0x2c,0x22,0x70,0x72,0x6f,0x6d,0x70,0x74,0x22,0x3a,0x22,0x61,0x20,0x63,0x61,0x74,0x22,0x7d] },
    JsonCase { path: "/v1/systemone", plain: b"{\"model\":\"request-decode-absent\",\"state\":\"sentinel\",\"questions\":{\"q\":{\"type\":\"noul\",\"instructions\":\"Is this safe?\"}}}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x75,0xf5,0x02,0x00,0xd2,0x05,0x13,0x18,0xa0,0x37,0x1d,0xa4,0x00,0x5b,0xb2,0x89,0x15,0x79,0xd2,0x83,0x9f,0x4b,0x46,0x41,0x8e,0xe3,0x3b,0x61,0x59,0x3f,0x84,0x01,0x29,0xa5,0x02,0x3c,0x30,0x27,0x6e,0x84,0x24,0x1e,0xe8,0x81,0x94,0xfc,0x19,0xc2,0x6b,0xd5,0x86,0x43,0x26,0x5d,0xaa,0x3f,0xb4,0x16,0x45,0x7c,0xd6,0xaa,0xd3,0xe4,0x5b,0x34,0xcf,0x17,0x18,0xc3,0x07,0x0f,0x1f,0x23,0x6f,0x94,0x58,0x69,0x15,0xf1,0xc1,0x42,0x97,0x05,0x00,0x46,0xa5,0x19,0xca,0x10,0x8a,0x1d,0xa6,0xda,0x48,0xd6,0x59,0x06] },
    JsonCase { path: "/v1/decisions", plain: b"{\"model\":\"request-decode-absent\",\"input\":\"sentinel\",\"questions\":[{\"type\":\"predicate\",\"name\":\"q\",\"instructions\":\"Is this safe?\"}]}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x81,0x35,0x03,0x00,0x82,0x06,0x15,0x18,0xa0,0xa7,0x39,0xad,0x00,0x22,0x72,0x7b,0x13,0x99,0xd3,0x83,0xcf,0xa5,0x6e,0x42,0xc8,0x71,0x7c,0xcd,0x98,0x04,0x15,0xc3,0xc9,0x20,0x31,0xe0,0x85,0xa3,0x45,0xd1,0xb0,0x45,0x03,0x46,0x8d,0xad,0x1e,0xc6,0x49,0x1b,0x7f,0x0f,0xc2,0x74,0xb6,0x7c,0xfe,0x3a,0xac,0x11,0xc1,0x59,0x53,0xa1,0x8d,0x8b,0x27,0xfa,0xbc,0x6d,0x25,0xda,0x78,0xf3,0x4a,0x38,0xc4,0x93,0x42,0x4f,0xc4,0xea,0xdb,0x71,0xe6,0x73,0xf1,0xa4,0x0c,0x23,0x01,0x05,0x00,0x52,0x53,0x85,0xd0,0x6c,0x9b,0x2b,0x33,0x25,0x5b,0x12,0xcc,0x32] },
    JsonCase { path: "/v1/batches", plain: b"{\"input_file_id\":\"aisix-ZmlsZS1zZW50aW5lbDttb2RlbCxyZXF1ZXN0LWRlY29kZS1hYnNlbnQ\",\"endpoint\":\"/v1/chat/completions\",\"completion_window\":\"24h\"}", encoded: &[0x28,0xb5,0x2f,0xfd,0x20,0x8d,0xfd,0x03,0x00,0x32,0x88,0x1d,0x1e,0x50,0xcb,0x74,0xd8,0xdb,0x31,0x1d,0x90,0xc9,0x62,0xfb,0x7a,0x6b,0xce,0xd7,0x95,0x16,0xaf,0x05,0x07,0x06,0x8e,0xc0,0x5e,0x5c,0x61,0x54,0x55,0x85,0x35,0x8f,0x63,0x01,0xe5,0xcc,0x39,0x69,0xf5,0x72,0xd3,0x91,0x6f,0x2d,0x77,0x5e,0xac,0x65,0xb2,0x7a,0x93,0xc2,0xaa,0x07,0x17,0xef,0xcc,0x67,0xaf,0xd4,0xac,0xbe,0x8e,0xfc,0xd0,0x91,0x9b,0x96,0x30,0xf8,0x68,0x42,0xa0,0x48,0xec,0x40,0x86,0xa4,0x89,0x14,0x16,0x22,0x65,0x74,0x83,0xc8,0x8e,0x12,0xe7,0x24,0x44,0x5e,0x20,0x2a,0x59,0x20,0x6d,0xc0,0x47,0x37,0xce,0x14,0xd0,0xed,0xb6,0xd4,0x99,0x5b,0x3b,0x5d,0xbc,0x88,0x69,0x2a,0xb4,0xde,0x3b,0x02,0x01,0x00,0x74,0x6c,0x33,0x0a] },
];
const PADDED_RESPONSES_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x60, 0x2c, 0x0f, 0xad, 0x01, 0x00, 0xd4, 0x02, 0x7b, 0x22, 0x6d, 0x6f,
    0x64, 0x65, 0x6c, 0x22, 0x3a, 0x22, 0x72, 0x65, 0x71, 0x75, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x65,
    0x63, 0x6f, 0x64, 0x65, 0x2d, 0x61, 0x62, 0x73, 0x65, 0x6e, 0x74, 0x22, 0x2c, 0x22, 0x69, 0x6e,
    0x70, 0x75, 0x74, 0x22, 0x3a, 0x22, 0x78, 0x22, 0x7d, 0x01, 0x00, 0xe3, 0xbf, 0x0f, 0xca,
];
const PADDED_RESPONSES_LEN: usize = 4140;
const LARGE_RESPONSES_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x80, 0x58, 0x2c, 0x00, 0x28, 0x00, 0xa4, 0x01, 0x00, 0xb4, 0x02, 0x7b,
    0x22, 0x6d, 0x6f, 0x64, 0x65, 0x6c, 0x22, 0x3a, 0x22, 0x72, 0x65, 0x71, 0x75, 0x65, 0x73, 0x74,
    0x2d, 0x64, 0x65, 0x63, 0x6f, 0x64, 0x65, 0x2d, 0x61, 0x62, 0x73, 0x65, 0x6e, 0x74, 0x22, 0x2c,
    0x22, 0x69, 0x6e, 0x70, 0x75, 0x74, 0x22, 0x3a, 0x22, 0x78, 0x01, 0x00, 0x93, 0xfe, 0xcf, 0x41,
    0x19, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10,
    0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10,
    0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10,
    0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10,
    0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x02, 0x00, 0x10, 0x78, 0x4d, 0x00, 0x00,
    0x18, 0x78, 0x22, 0x7d, 0x01, 0x00, 0x20, 0x20, 0x04,
];
const LARGE_RESPONSES_LEN: usize = 2621484;
const MULTIPART_PLAIN: &[u8] = b"--transport-boundary\x0d\x0aContent-Disposition: form-data; name=\"model\"\x0d\x0a\x0d\x0arequest-decode-absent\x0d\x0a--transport-boundary\x0d\x0aContent-Disposition: form-data; name=\"file\"; filename=\"sentinel.bin\"\x0d\x0aContent-Type: application/octet-stream\x0d\x0a\x0d\x0asentinel\x0d\x0a--transport-boundary--\x0d\x0a";
const MULTIPART_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x60, 0x05, 0x00, 0x55, 0x04, 0x00, 0xb2, 0x48, 0x1b, 0x1b, 0x80, 0xa5,
    0x6d, 0x03, 0xc0, 0x26, 0x6d, 0xa3, 0x57, 0xdc, 0x3d, 0x96, 0x6d, 0x8a, 0x93, 0xe9, 0x83, 0x89,
    0x03, 0x84, 0x80, 0x0c, 0xdb, 0xc5, 0xaf, 0xba, 0x0e, 0xc5, 0xdc, 0x2d, 0x3c, 0x77, 0xbb, 0x7e,
    0x1b, 0x2b, 0x40, 0x23, 0x15, 0x63, 0x78, 0x86, 0xb3, 0x04, 0x00, 0x69, 0x16, 0x01, 0x75, 0xcd,
    0x43, 0xa0, 0xab, 0x3c, 0x8a, 0x69, 0x7d, 0x2b, 0xf0, 0x9e, 0xd6, 0x78, 0xba, 0x6e, 0x47, 0x02,
    0xce, 0xc5, 0x14, 0x83, 0xd4, 0xd3, 0x2e, 0x34, 0xf8, 0x42, 0x3d, 0x04, 0x36, 0xa6, 0x5b, 0xbc,
    0xc7, 0x19, 0x74, 0xb3, 0xb3, 0x2d, 0x6b, 0x59, 0xb8, 0xd6, 0xb7, 0x2e, 0xa1, 0x98, 0x84, 0x31,
    0x29, 0xb2, 0xc5, 0x35, 0x2f, 0x6b, 0x8a, 0xdc, 0x77, 0x01, 0x09, 0x00, 0x13, 0xa3, 0x80, 0x2b,
    0x01, 0x20, 0x8b, 0x8f, 0x65, 0xe1, 0x9b, 0x6c, 0xcf, 0x0a, 0x0b, 0x71, 0x18, 0xa0, 0x0a, 0xac,
    0x0b, 0x28, 0x05, 0x33,
];
const IMAGE_MULTIPART_PLAIN: &[u8] = b"--transport-boundary\x0d\x0aContent-Disposition: form-data; name=\"model\"\x0d\x0a\x0d\x0arequest-decode-absent\x0d\x0a--transport-boundary\x0d\x0aContent-Disposition: form-data; name=\"image\"; filename=\"sentinel.bin\"\x0d\x0aContent-Type: application/octet-stream\x0d\x0a\x0d\x0asentinel\x0d\x0a--transport-boundary--\x0d\x0a";
const IMAGE_MULTIPART_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x60, 0x06, 0x00, 0x55, 0x04, 0x00, 0x02, 0xc9, 0x1b, 0x1a, 0x80, 0x27,
    0x6e, 0x80, 0x5f, 0x88, 0xd8, 0x22, 0x73, 0x45, 0xfc, 0xc4, 0x2f, 0x0e, 0xe3, 0x87, 0x45, 0x63,
    0xb8, 0x43, 0x86, 0x24, 0x34, 0x03, 0xa7, 0x03, 0xc5, 0x20, 0xb2, 0xbc, 0xeb, 0x87, 0xfd, 0x36,
    0x38, 0x90, 0x86, 0xae, 0xd6, 0xf2, 0x10, 0x6f, 0x49, 0x01, 0xad, 0x26, 0xc0, 0xfa, 0xea, 0x97,
    0x1e, 0xa7, 0x40, 0x37, 0x24, 0xd3, 0x62, 0x7a, 0xff, 0x52, 0xe2, 0x15, 0x1b, 0x57, 0xd8, 0xef,
    0xc8, 0xe1, 0xae, 0x98, 0x62, 0xd0, 0xba, 0x62, 0x86, 0x04, 0x67, 0xb9, 0xa7, 0xc8, 0x4e, 0x85,
    0xcc, 0xf1, 0x71, 0x88, 0x65, 0x6d, 0x7d, 0xdc, 0x9e, 0x12, 0xb0, 0xf7, 0xbd, 0x3c, 0x28, 0x26,
    0x71, 0xa9, 0x16, 0xe1, 0x84, 0xed, 0xb8, 0xbd, 0x4d, 0xd7, 0x88, 0x01, 0x08, 0x00, 0x14, 0xa3,
    0x80, 0x2b, 0x01, 0x21, 0x8b, 0xaf, 0x65, 0xe1, 0x9f, 0x6c, 0xef, 0x0a, 0x0b, 0xe4, 0x18, 0xb2,
    0x0b, 0x28, 0x05, 0x33,
];

const MULTIPART_RAW_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x60, 0x05, 0x00, 0x29, 0x08, 0x00, 0x2d, 0x2d, 0x74, 0x72, 0x61, 0x6e,
    0x73, 0x70, 0x6f, 0x72, 0x74, 0x2d, 0x62, 0x6f, 0x75, 0x6e, 0x64, 0x61, 0x72, 0x79, 0x0d, 0x0a,
    0x43, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x44, 0x69, 0x73, 0x70, 0x6f, 0x73, 0x69, 0x74,
    0x69, 0x6f, 0x6e, 0x3a, 0x20, 0x66, 0x6f, 0x72, 0x6d, 0x2d, 0x64, 0x61, 0x74, 0x61, 0x3b, 0x20,
    0x6e, 0x61, 0x6d, 0x65, 0x3d, 0x22, 0x6d, 0x6f, 0x64, 0x65, 0x6c, 0x22, 0x0d, 0x0a, 0x0d, 0x0a,
    0x72, 0x65, 0x71, 0x75, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x65, 0x63, 0x6f, 0x64, 0x65, 0x2d, 0x61,
    0x62, 0x73, 0x65, 0x6e, 0x74, 0x0d, 0x0a, 0x2d, 0x2d, 0x74, 0x72, 0x61, 0x6e, 0x73, 0x70, 0x6f,
    0x72, 0x74, 0x2d, 0x62, 0x6f, 0x75, 0x6e, 0x64, 0x61, 0x72, 0x79, 0x0d, 0x0a, 0x43, 0x6f, 0x6e,
    0x74, 0x65, 0x6e, 0x74, 0x2d, 0x44, 0x69, 0x73, 0x70, 0x6f, 0x73, 0x69, 0x74, 0x69, 0x6f, 0x6e,
    0x3a, 0x20, 0x66, 0x6f, 0x72, 0x6d, 0x2d, 0x64, 0x61, 0x74, 0x61, 0x3b, 0x20, 0x6e, 0x61, 0x6d,
    0x65, 0x3d, 0x22, 0x66, 0x69, 0x6c, 0x65, 0x22, 0x3b, 0x20, 0x66, 0x69, 0x6c, 0x65, 0x6e, 0x61,
    0x6d, 0x65, 0x3d, 0x22, 0x73, 0x65, 0x6e, 0x74, 0x69, 0x6e, 0x65, 0x6c, 0x2e, 0x62, 0x69, 0x6e,
    0x22, 0x0d, 0x0a, 0x43, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x54, 0x79, 0x70, 0x65, 0x3a,
    0x20, 0x61, 0x70, 0x70, 0x6c, 0x69, 0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x2f, 0x6f, 0x63, 0x74,
    0x65, 0x74, 0x2d, 0x73, 0x74, 0x72, 0x65, 0x61, 0x6d, 0x0d, 0x0a, 0x0d, 0x0a, 0x73, 0x65, 0x6e,
    0x74, 0x69, 0x6e, 0x65, 0x6c, 0x0d, 0x0a, 0x2d, 0x2d, 0x74, 0x72, 0x61, 0x6e, 0x73, 0x70, 0x6f,
    0x72, 0x74, 0x2d, 0x62, 0x6f, 0x75, 0x6e, 0x64, 0x61, 0x72, 0x79, 0x2d, 0x2d, 0x0d, 0x0a,
];

const SPLIT_FIRST_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x21, 0x09, 0x01, 0x00, 0x7b, 0x22, 0x6d, 0x6f, 0x64, 0x65, 0x6c,
    0x22, 0x3a, 0x22, 0x72, 0x65, 0x71, 0x75, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x65, 0x63, 0x6f, 0x64,
    0x65, 0x2d, 0x61, 0x62, 0x73, 0x65, 0x6e, 0x74, 0x22, 0x2c,
];

const SPLIT_SECOND_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x1f, 0xf9, 0x00, 0x00, 0x22, 0x69, 0x6e, 0x70, 0x75, 0x74, 0x22,
    0x3a, 0x22, 0x74, 0x72, 0x61, 0x6e, 0x73, 0x70, 0x6f, 0x72, 0x74, 0x2d, 0x72, 0x65, 0x67, 0x72,
    0x65, 0x73, 0x73, 0x69, 0x6f, 0x6e, 0x22, 0x7d,
];

const EMPTY_ZSTD: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x00, 0x01, 0x00, 0x00];
const REGISTER_PLAIN: &[u8] = b"{\"redirect_uris\":[\"http://localhost/transport-callback\"]}";
const REGISTER_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x39, 0xc9, 0x01, 0x00, 0x7b, 0x22, 0x72, 0x65, 0x64, 0x69, 0x72,
    0x65, 0x63, 0x74, 0x5f, 0x75, 0x72, 0x69, 0x73, 0x22, 0x3a, 0x5b, 0x22, 0x68, 0x74, 0x74, 0x70,
    0x3a, 0x2f, 0x2f, 0x6c, 0x6f, 0x63, 0x61, 0x6c, 0x68, 0x6f, 0x73, 0x74, 0x2f, 0x74, 0x72, 0x61,
    0x6e, 0x73, 0x70, 0x6f, 0x72, 0x74, 0x2d, 0x63, 0x61, 0x6c, 0x6c, 0x62, 0x61, 0x63, 0x6b, 0x22,
    0x5d, 0x7d,
];
const TOKEN_PLAIN: &[u8] = b"grant_type=authorization_code&code=unknown-transport-code&client_id=main&redirect_uri=http%3A%2F%2Flocalhost%2Fcb&code_verifier=transport-verifier";
const TOKEN_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x92, 0x95, 0x03, 0x00, 0xb2, 0x87, 0x18, 0x19, 0x80, 0x27, 0x6e,
    0x08, 0x40, 0x2b, 0x77, 0x49, 0x24, 0xe2, 0x24, 0xc1, 0x17, 0x22, 0x71, 0xda, 0x81, 0x42, 0x4a,
    0x9a, 0x51, 0x00, 0x33, 0x04, 0x17, 0x9c, 0x37, 0xce, 0x69, 0x38, 0x9c, 0x37, 0xce, 0x69, 0x48,
    0x02, 0x85, 0x0c, 0xe9, 0x63, 0x59, 0x43, 0xb5, 0x0d, 0x19, 0x82, 0x0c, 0x09, 0x00, 0x88, 0xeb,
    0x66, 0xe7, 0xfc, 0x52, 0xab, 0xe4, 0xaf, 0xe4, 0xa2, 0xc7, 0xc2, 0x59, 0x4f, 0xad, 0xf9, 0xa6,
    0x4a, 0xd1, 0xbc, 0xee, 0x50, 0xe4, 0x2e, 0xf4, 0xb0, 0x4a, 0xd0, 0x77, 0x4a, 0xae, 0x55, 0x49,
    0xf7, 0x8d, 0x89, 0xf3, 0xb2, 0x7e, 0x78, 0xd2, 0x21, 0x3a, 0xb5, 0x22, 0x0f, 0x02, 0x04, 0x00,
    0x58, 0x2c, 0xc2, 0x2b, 0x34, 0xac, 0x73, 0x83, 0x58, 0x4b, 0x1e,
];
const CONSENT_PLAIN: &[u8] = b"flow=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&csrf=transport-csrf&decision=cancel";
const CONSENT_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x69, 0x85, 0x01, 0x00, 0x64, 0x02, 0x66, 0x6c, 0x6f, 0x77, 0x3d,
    0x61, 0x26, 0x63, 0x73, 0x72, 0x66, 0x3d, 0x74, 0x72, 0x61, 0x6e, 0x73, 0x70, 0x6f, 0x72, 0x74,
    0x2d, 0x26, 0x64, 0x65, 0x63, 0x69, 0x73, 0x69, 0x6f, 0x6e, 0x3d, 0x63, 0x61, 0x6e, 0x63, 0x65,
    0x6c, 0x02, 0x00, 0x64, 0x06, 0x8b, 0x12, 0x28, 0x02,
];
const MCP_ALL_PLAIN: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"census__tool\",\"arguments\":{}}}";
const MCP_ALL_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x5e, 0x95, 0x02, 0x00, 0xe2, 0x85, 0x13, 0x19, 0xa0, 0xa7, 0x19,
    0x03, 0x35, 0x34, 0x42, 0x40, 0x0b, 0xa5, 0x32, 0xfb, 0x67, 0x77, 0x89, 0xaa, 0xd1, 0xb1, 0xb7,
    0xbf, 0x11, 0xa0, 0x6a, 0x0c, 0x01, 0xad, 0x35, 0x86, 0xef, 0x4a, 0x06, 0x1d, 0x04, 0x95, 0xdf,
    0x47, 0x75, 0x11, 0xb2, 0xc7, 0x26, 0x53, 0xc6, 0x0d, 0xca, 0x34, 0xc3, 0x17, 0xa5, 0x4a, 0xe3,
    0xfb, 0x9c, 0x2c, 0x80, 0x3d, 0xaa, 0x6b, 0xbc, 0xd0, 0xc2, 0x32, 0xe8, 0x13, 0xe0, 0x85, 0xe1,
    0x3b, 0x40, 0x0c, 0xc6, 0x8b, 0xa1, 0x92, 0xee, 0x61, 0x16, 0x00,
];
const MCP_SCOPED_PLAIN: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"tool\",\"arguments\":{}}}";
const MCP_SCOPED_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x56, 0x6d, 0x02, 0x00, 0xf2, 0x04, 0x11, 0x18, 0x80, 0xa9, 0x2d,
    0x06, 0x4b, 0x83, 0x92, 0x33, 0xe9, 0x7c, 0x04, 0x22, 0x11, 0x9a, 0x90, 0xd7, 0x6d, 0x04, 0xde,
    0x51, 0x47, 0x51, 0x1f, 0x01, 0x6b, 0x2d, 0x75, 0x6f, 0x3a, 0x25, 0x13, 0x05, 0x84, 0xdb, 0x25,
    0xf1, 0xac, 0xee, 0x4d, 0x44, 0x58, 0x6c, 0x1b, 0x83, 0x23, 0xb0, 0x46, 0x35, 0x7d, 0x13, 0x35,
    0xa4, 0x92, 0xdb, 0xe0, 0x26, 0x87, 0x6d, 0x01, 0x80, 0xf0, 0x3d, 0x0a, 0x74, 0x74, 0x11, 0xab,
    0x01, 0x00, 0x1d, 0x32, 0x05, 0x05,
];
const A2A_PLAIN: &[u8] =
    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tasks/get\",\"params\":{\"id\":\"task-1\"}}";
const A2A_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x46, 0x31, 0x02, 0x00, 0x7b, 0x22, 0x6a, 0x73, 0x6f, 0x6e, 0x72,
    0x70, 0x63, 0x22, 0x3a, 0x22, 0x32, 0x2e, 0x30, 0x22, 0x2c, 0x22, 0x69, 0x64, 0x22, 0x3a, 0x31,
    0x2c, 0x22, 0x6d, 0x65, 0x74, 0x68, 0x6f, 0x64, 0x22, 0x3a, 0x22, 0x74, 0x61, 0x73, 0x6b, 0x73,
    0x2f, 0x67, 0x65, 0x74, 0x22, 0x2c, 0x22, 0x70, 0x61, 0x72, 0x61, 0x6d, 0x73, 0x22, 0x3a, 0x7b,
    0x22, 0x69, 0x64, 0x22, 0x3a, 0x22, 0x74, 0x61, 0x73, 0x6b, 0x2d, 0x31, 0x22, 0x7d, 0x7d,
];

const WIDE_WINDOW_ZSTD: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x90, 0x01, 0x00, 0x00];
const MESSAGES_PLAIN: &[u8] = b"{\"model\":\"request-decode-absent\",\"max_tokens\":16,\"messages\":[{\"role\":\"user\",\"content\":\"sentinel\"}]}";
const MESSAGES_RAW_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x63, 0x19, 0x03, 0x00, 0x7b, 0x22, 0x6d, 0x6f, 0x64, 0x65, 0x6c,
    0x22, 0x3a, 0x22, 0x72, 0x65, 0x71, 0x75, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x65, 0x63, 0x6f, 0x64,
    0x65, 0x2d, 0x61, 0x62, 0x73, 0x65, 0x6e, 0x74, 0x22, 0x2c, 0x22, 0x6d, 0x61, 0x78, 0x5f, 0x74,
    0x6f, 0x6b, 0x65, 0x6e, 0x73, 0x22, 0x3a, 0x31, 0x36, 0x2c, 0x22, 0x6d, 0x65, 0x73, 0x73, 0x61,
    0x67, 0x65, 0x73, 0x22, 0x3a, 0x5b, 0x7b, 0x22, 0x72, 0x6f, 0x6c, 0x65, 0x22, 0x3a, 0x22, 0x75,
    0x73, 0x65, 0x72, 0x22, 0x2c, 0x22, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x22, 0x3a, 0x22,
    0x73, 0x65, 0x6e, 0x74, 0x69, 0x6e, 0x65, 0x6c, 0x22, 0x7d, 0x5d, 0x7d,
];

const PADDED_MESSAGES_ZSTD: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x60, 0x5b, 0x0f, 0xa5, 0x02, 0x00, 0x82, 0x05, 0x12, 0x17, 0x90, 0xa7,
    0x3a, 0x13, 0xa6, 0x89, 0x7c, 0x10, 0x92, 0xfa, 0x14, 0x5a, 0x8a, 0x48, 0xec, 0x76, 0x83, 0xcc,
    0x54, 0x6c, 0x72, 0x3f, 0x25, 0x0f, 0xf0, 0xdc, 0x8c, 0xab, 0xd2, 0x78, 0x78, 0x5d, 0x99, 0xd1,
    0x47, 0xb9, 0xec, 0x0a, 0xb4, 0x7e, 0x50, 0x54, 0x9f, 0x3c, 0x88, 0x01, 0xad, 0xea, 0x84, 0xdc,
    0x40, 0x2b, 0xc9, 0xc3, 0xab, 0xae, 0x82, 0x42, 0x7e, 0xd2, 0xfc, 0x90, 0xd5, 0xb3, 0xc2, 0x97,
    0xd1, 0xe8, 0x27, 0x93, 0x5d, 0x02, 0x00, 0xfc, 0xc7, 0xf9, 0x44, 0x98, 0x55, 0x66,
];
const PADDED_MESSAGES_LEN: usize = 4187;
