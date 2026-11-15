// 通过真实 HTTP 慢上传验证入口阶段；移除共享采样或终态 stamp 必须令断言失败。

#[tokio::test]
async fn request_timing_records_slow_json_rejections_across_typed_families() {
    use futures::stream;
    use std::time::Duration;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let snapshot = seed_snapshot("phase-model", &["phase-model"], "http://unused");
    let app = build_router(
        build_state(snapshot, Arc::new(Hub::new())).with_usage_sink(aisix_obs::UsageSink::new(tx)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    const PREFIX: &[u8] = br#"{"model":"#;
    const SUFFIX: &[u8] = b"invalid";
    for endpoint in [
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/v1/responses",
        "/v1/completions",
        "/v1/embeddings",
        "/v1/rerank",
        "/v1/images/generations",
        "/v1/audio/speech",
        "/v1/videos",
        "/v1/decisions",
        "/v1/systemone",
    ] {
        let body =
            stream::once(async { Ok::<_, std::io::Error>(bytes::Bytes::from_static(PREFIX)) })
                .chain(stream::once(async {
                    tokio::time::sleep(Duration::from_millis(160)).await;
                    Ok::<_, std::io::Error>(bytes::Bytes::from_static(SUFFIX))
                }));
        let response = client
            .post(format!("http://{address}{endpoint}"))
            .bearer_auth("sk-caller")
            .header("content-type", "application/json")
            .body(reqwest::Body::wrap_stream(body))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400, "{endpoint}");
        let _ = response.bytes().await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap_or_else(|error| {
                panic!("{endpoint}: HTTP 400 已返回但终态 UsageEvent 未到达：{error}")
            })
            .unwrap_or_else(|| panic!("{endpoint}: UsageEvent 接收通道已关闭"));
        let wire = serde_json::to_value(event).unwrap();
        let timing = wire
            .get("request_timing")
            .expect("终态事件必须携带实际入口测量");
        let body_ms = timing
            .get("body_read_ms")
            .and_then(serde_json::Value::as_u64)
            .expect("慢上传必须记录读取阶段");
        assert!(body_ms >= 140, "{endpoint}: {timing}");
        assert!(
            timing
                .get("json_decode_ms")
                .and_then(serde_json::Value::as_u64)
                .is_some(),
            "解码阶段即使为零也必须有键：{endpoint}: {timing}"
        );
        let before = timing
            .get("pre_handler_ms")
            .and_then(serde_json::Value::as_u64)
            .expect("最终 extractor 完成必须记录 handler 前时间");
        assert!(before >= body_ms, "{endpoint}: {timing}");
        assert_eq!(
            timing.get("request_body_bytes"),
            Some(&serde_json::json!(PREFIX.len() + SUFFIX.len())),
            "{endpoint}"
        );
        assert_eq!(
            timing.get("request_body_complete"),
            Some(&serde_json::json!(true)),
            "{endpoint}"
        );
        assert!(rx.try_recv().is_err(), "一个拒绝只能发布一次：{endpoint}");
    }
    server.abort();
}

#[tokio::test]
async fn request_timing_cancelled_upload_reports_partial_read_without_decode() {
    use std::time::Duration;
    use tower::ServiceExt;

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let snapshot = seed_snapshot("phase-model", &["phase-model"], "http://unused");
    let app = build_router(
        build_state(snapshot, Arc::new(Hub::new())).with_usage_sink(aisix_obs::UsageSink::new(tx)),
    );
    let (read_tx, read_rx) = tokio::sync::oneshot::channel();
    const PREFIX: &[u8] = br#"{"model":"#;
    let body = async_stream::stream! {
        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(PREFIX));
        let _ = read_tx.send(());
        std::future::pending::<()>().await;
    };
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", "Bearer sk-caller")
        .header("content-type", "application/json")
        .body(Body::from_stream(body))
        .unwrap();
    let task = tokio::spawn(app.oneshot(request));
    read_rx.await.unwrap();
    tokio::time::sleep(Duration::from_millis(160)).await;
    task.abort();
    let _ = task.await;
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.status_code, 499);
    let wire = serde_json::to_value(event).unwrap();
    let timing = wire
        .get("request_timing")
        .expect("取消不能丢失已观测入口窗口");
    let read_ms = timing
        .get("body_read_ms")
        .and_then(serde_json::Value::as_u64)
        .expect("取消发生在真实正文读取窗口内");
    assert!(read_ms >= 140, "{timing}");
    assert_eq!(
        timing.get("request_body_bytes"),
        Some(&serde_json::json!(PREFIX.len()))
    );
    assert_eq!(
        timing.get("request_body_complete"),
        Some(&serde_json::json!(false))
    );
    assert!(timing.get("json_decode_ms").is_none(), "{timing}");
    assert!(timing.get("pre_handler_ms").is_none(), "{timing}");
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn request_timing_read_error_never_claims_complete_or_decoded_json() {
    use std::time::Duration;
    use tower::ServiceExt;

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let snapshot = seed_snapshot("phase-model", &["phase-model"], "http://unused");
    let app = build_router(
        build_state(snapshot, Arc::new(Hub::new())).with_usage_sink(aisix_obs::UsageSink::new(tx)),
    );
    const PREFIX: &[u8] = br#"{"model":"#;
    let body = async_stream::stream! {
        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(PREFIX));
        tokio::time::sleep(Duration::from_millis(160)).await;
        yield Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "client read failed"));
    };
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer sk-caller")
                .header("content-type", "application/json")
                .body(Body::from_stream(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
    let _ = to_bytes(response.into_body(), 65_536).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let wire = serde_json::to_value(event).unwrap();
    let timing = wire.get("request_timing").expect("读失败仍须保留已读窗口");
    assert!(
        timing
            .get("body_read_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap()
            >= 140
    );
    assert_eq!(
        timing.get("request_body_complete"),
        Some(&serde_json::json!(false))
    );
    assert_eq!(
        timing.get("request_body_bytes"),
        Some(&serde_json::json!(PREFIX.len()))
    );
    assert!(timing.get("json_decode_ms").is_none(), "{timing}");
    assert!(
        timing
            .get("pre_handler_ms")
            .and_then(serde_json::Value::as_u64)
            .is_some(),
        "{timing}"
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn request_timing_raw_and_bodyless_errors_record_the_handler_boundary() {
    use std::time::Duration;
    use tower::ServiceExt;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let snapshot = seed_snapshot("phase-model", &["phase-model"], "http://unused");
    let app = build_router(
        build_state(snapshot, Arc::new(Hub::new())).with_usage_sink(aisix_obs::UsageSink::new(tx)),
    );
    for (method, path) in [
        ("POST", "/v1/audio/transcriptions"),
        ("POST", "/v1/audio/translations"),
        ("POST", "/v1/images/edits"),
        ("POST", "/v1/images/variations"),
        ("POST", "/v1/content_provenance_checks"),
        ("POST", "/v1/files"),
        ("GET", "/v1/files"),
        ("GET", "/v1/batches/missing-phase-boundary"),
        ("POST", "/v1/uploads/missing-phase-boundary/parts"),
        ("POST", "/v1/uploads/missing-phase-boundary/cancel"),
        ("GET", "/v1/conversations/missing-phase-boundary"),
        ("GET", "/v1/containers"),
        ("POST", "/v1/containers/missing-phase-boundary/files"),
        ("GET", "/v1/vector_stores"),
        ("GET", "/v1/responses/missing-phase-boundary"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", "Bearer sk-caller")
                    .header("content-type", "text/plain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            !response.status().is_success(),
            "请求没有可用资源或合法上传：{method} {path}"
        );
        let _ = to_bytes(response.into_body(), 65_536).await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap_or_else(|error| panic!("{method} {path}: 错误终态未到达：{error}"))
            .unwrap_or_else(|| panic!("{method} {path}: UsageEvent 通道关闭"));
        let wire = serde_json::to_value(event).unwrap();
        let timing = wire.get("request_timing").expect("HTTP 终态记录入口阶段");
        assert!(
            timing
                .get("pre_handler_ms")
                .and_then(serde_json::Value::as_u64)
                .is_some(),
            "{method} {path}: 已执行 handler 的窗口不能未知：{timing}"
        );
        assert!(
            timing.get("json_decode_ms").is_none(),
            "{method} {path}: 未解码 JSON 的请求不能捏造阶段：{timing}"
        );
        assert!(rx.try_recv().is_err(), "{method} {path}: 不重复发射");
    }
}
