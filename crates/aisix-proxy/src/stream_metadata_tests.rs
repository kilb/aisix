use axum::body::{to_bytes, Body};
use serde_json::{json, Value};
use tower::ServiceExt;

// 组件合同：真实路由与本地阻断护栏；不访问供应商，也不推断响应传输形态。
async fn assert_stream_intent(path: &str) {
    for requested in [Some(true), Some(false), None] {
        let (app, mut usage) = crate::guardrail_coverage::census_router_with_usage();
        let request = crate::guardrail_coverage::fixture(path).unwrap();
        let (parts, body) = request.into_parts();
        let mut body: Value =
            serde_json::from_slice(&to_bytes(body, 65536).await.unwrap()).unwrap();
        if let Some(stream) = requested {
            body["stream"] = json!(stream);
        }
        let request = axum::http::Request::from_parts(parts, Body::from(body.to_string()));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            422,
            "{path}: guardrail must reject before inference"
        );
        to_bytes(response.into_body(), 65536).await.unwrap();
        let event = usage.try_recv().expect("guardrail refusal must emit usage");
        let wire = serde_json::to_value(event).unwrap();
        assert_eq!(
            wire.get("stream"),
            Some(&json!(requested.unwrap_or(false))),
            "{path}: missing stream is unknown, never an explicit false"
        );
        assert!(usage.try_recv().is_err(), "one terminal event per request");
    }
}

#[tokio::test]
async fn chat_usage_preserves_caller_stream_intent() {
    assert_stream_intent("/v1/chat/completions").await;
}

#[tokio::test]
async fn messages_usage_preserves_caller_stream_intent() {
    assert_stream_intent("/v1/messages").await;
}

#[tokio::test]
async fn responses_usage_preserves_caller_stream_intent() {
    assert_stream_intent("/v1/responses").await;
}

#[test]
fn legacy_usage_has_no_stream_claim() {
    let legacy = serde_json::to_value(aisix_obs::UsageEvent::default()).unwrap();
    assert!(legacy.get("stream").is_none());
    assert_ne!(legacy.get("stream"), Some(&json!(false)));
}

#[tokio::test]
async fn fixed_non_streaming_families_emit_explicit_false() {
    for path in [
        "/v1/messages/count_tokens",
        "/v1/embeddings",
        "/v1/rerank",
        "/v1/moderations",
        "/v1/content_provenance_checks",
        "/v1/videos",
        "/v1/responses/compact",
        "/v1/responses/input_tokens",
    ] {
        let (app, mut usage) = crate::guardrail_coverage::census_router_with_usage();
        let request = crate::guardrail_coverage::fixture(path).unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            422,
            "{path}: guardrail must reject before inference"
        );
        to_bytes(response.into_body(), 65536).await.unwrap();
        let event = serde_json::to_value(usage.try_recv().unwrap()).unwrap();
        assert_eq!(event.get("stream"), Some(&json!(false)), "{path}");
    }
}

#[tokio::test]
async fn media_json_requests_preserve_stream_intent() {
    assert_stream_intent("/v1/images/generations").await;
    assert_stream_intent("/v1/completions").await;
    for (format, expected) in [("sse", true), ("audio", false)] {
        let (app, mut usage) = crate::guardrail_coverage::census_router_with_usage();
        let request = crate::guardrail_coverage::fixture("/v1/audio/speech").unwrap();
        let (parts, body) = request.into_parts();
        let mut body: Value =
            serde_json::from_slice(&to_bytes(body, 65536).await.unwrap()).unwrap();
        body["stream_format"] = json!(format);
        let response = app
            .oneshot(axum::http::Request::from_parts(
                parts,
                Body::from(body.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
        to_bytes(response.into_body(), 65536).await.unwrap();
        let event = serde_json::to_value(usage.try_recv().unwrap()).unwrap();
        assert_eq!(event.get("stream"), Some(&json!(expected)));
    }
}

#[tokio::test]
async fn multipart_media_requests_preserve_stream_intent() {
    for path in ["/v1/images/edits", "/v1/audio/transcriptions"] {
        for requested in [Some(true), Some(false), None] {
            let (app, mut usage) = crate::guardrail_coverage::census_router_with_usage();
            let request = crate::guardrail_coverage::fixture(path).unwrap();
            let (parts, body) = request.into_parts();
            let mut body =
                String::from_utf8(to_bytes(body, 65536).await.unwrap().to_vec()).unwrap();
            if let Some(stream) = requested {
                body = body.replace("--censusboundary--\r\n", &format!(
                    "--censusboundary\r\nContent-Disposition: form-data; name=\"stream\"\r\n\r\n{stream}\r\n--censusboundary--\r\n"
                ));
            }
            let response = app
                .oneshot(axum::http::Request::from_parts(parts, Body::from(body)))
                .await
                .unwrap();
            assert_eq!(response.status(), 422, "{path}");
            to_bytes(response.into_body(), 65536).await.unwrap();
            let event = serde_json::to_value(usage.try_recv().unwrap()).unwrap();
            assert_eq!(
                event.get("stream"),
                Some(&json!(requested.unwrap_or(false))),
                "{path}"
            );
        }
    }
}

#[tokio::test]
async fn invalid_stream_values_remain_unknown() {
    for path in ["/v1/messages", "/v1/responses", "/v1/images/generations"] {
        let (app, mut usage) = crate::guardrail_coverage::census_router_with_usage();
        let request = crate::guardrail_coverage::fixture(path).unwrap();
        let (parts, body) = request.into_parts();
        let mut body: Value =
            serde_json::from_slice(&to_bytes(body, 65536).await.unwrap()).unwrap();
        body["stream"] = json!("not-a-boolean");
        let response = app
            .oneshot(axum::http::Request::from_parts(
                parts,
                Body::from(body.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
        to_bytes(response.into_body(), 65536).await.unwrap();
        let event = serde_json::to_value(usage.try_recv().unwrap()).unwrap();
        assert!(
            event.get("stream").is_none(),
            "{path}: invalid intent must not become false"
        );
    }
}

#[test]
fn stream_metadata_readers_leave_unknown_values_unknown() {
    use crate::stream_metadata::{json_intent, multipart_intent};
    for (body, expected) in [
        (json!({}), Some(false)),
        (json!({"stream":null}), Some(false)),
        (json!({"stream":true}), Some(true)),
        (json!({"stream":false}), Some(false)),
        (json!({"stream":"true"}), None),
        (json!([]), None),
    ] {
        assert_eq!(json_intent(&body), expected);
    }
    assert_eq!(multipart_intent([]), Some(false));
    assert_eq!(
        multipart_intent([("stream", b"true".as_slice())]),
        Some(true)
    );
    assert_eq!(
        multipart_intent([("stream", b"false".as_slice())]),
        Some(false)
    );
    assert_eq!(multipart_intent([("stream", b"invalid".as_slice())]), None);
    assert_eq!(
        multipart_intent([
            ("stream", b"true".as_slice()),
            ("stream", b"false".as_slice())
        ]),
        None
    );
    assert_eq!(
        multipart_intent([
            ("stream", b"false".as_slice()),
            ("stream", b"true".as_slice())
        ]),
        None
    );
}

#[test]
fn stream_metadata_reads_allocate_nothing_even_beside_large_payloads() {
    use aisix_response_reference_test_alloc::AllocationScope;
    use std::hint::black_box;

    for bytes in [16, 4 * 1024 * 1024] {
        let body = json!({"stream":true,"input":"x".repeat(bytes)});
        // 正向对照确保同一计数器确实能抓住正文复制。
        let scope = AllocationScope::start();
        let copied = black_box(body.clone());
        let copying = scope.finish();
        assert!(copying.bytes >= bytes);
        drop(copied);

        let scope = AllocationScope::start();
        let started = std::time::Instant::now();
        for _ in 0..100_000 {
            assert_eq!(
                crate::stream_metadata::json_intent(black_box(&body)),
                Some(true)
            );
        }
        let elapsed = started.elapsed();
        let reading = scope.finish();
        assert_eq!(
            reading.calls, 0,
            "stream metadata must not parse or copy the payload"
        );
        eprintln!("stream read: payload={bytes}, iterations=100000, elapsed_us={}, allocations={reading:?}", elapsed.as_micros());
    }
    assert_eq!(std::mem::size_of::<Option<bool>>(), 1);
    eprintln!(
        "optional stream scalar={} byte; native context Box payload={} bytes",
        std::mem::size_of::<Option<bool>>(),
        std::mem::size_of::<aisix_core::native_objects::NativeUsageContext>()
    );
}

#[tokio::test]
async fn cancellation_restores_request_stream_observation() {
    use crate::attribution::{scope, RequestAttribution, Resolved};
    use std::sync::Arc;

    for stream in [Some(true), Some(false), None] {
        let config = serde_json::from_value(json!({"addr":"127.0.0.1:0"})).unwrap();
        let (tx, mut usage) = tokio::sync::mpsc::channel(4);
        let state = crate::ProxyState::new(
            aisix_core::snapshot::SnapshotHandle::new(aisix_core::AisixSnapshot::new()),
            Arc::new(aisix_gateway::Hub::new()),
            &config,
        )
        .with_usage_sink(aisix_obs::UsageSink::new(tx));
        let cell = Arc::new(RequestAttribution::default());
        scope(cell.clone(), async {
            crate::attribution::with_cancel_for_test(|ctx| {
                ctx.api_key_id = "local-component-key".into();
                ctx.stream = stream;
            });
            let cancelled = cell.take_cancel_context();
            assert_eq!(crate::attribution::requested_stream(), None);
            crate::cancel::emit(
                &state,
                "/v1/chat/completions",
                "cancel-component",
                &Resolved::default(),
                cancelled,
                crate::cancel::Phase::BeforeHead,
                None,
            );
        })
        .await;
        let event = serde_json::to_value(usage.try_recv().unwrap()).unwrap();
        assert_eq!(event["status_code"], 499);
        let expected = stream.map(Value::Bool);
        assert_eq!(event.get("stream"), expected.as_ref());
    }
}

#[tokio::test]
async fn native_submission_context_carries_stream_without_identity() {
    use crate::attribution::{scope, RequestAttribution};
    use std::sync::Arc;
    for stream in [Some(true), Some(false), None] {
        scope(Arc::new(RequestAttribution::default()), async {
            crate::attribution::note_requested_stream(stream);
            let context = crate::usage_attr::native_usage_context(
                &crate::client_ip::ClientContext::default(),
                Default::default(),
            );
            assert_eq!(context.is_some(), stream.is_some());
            let wire = serde_json::to_value(context).unwrap();
            let expected = stream.map(Value::Bool);
            assert_eq!(wire.get("stream"), expected.as_ref());
        })
        .await;
    }
}
