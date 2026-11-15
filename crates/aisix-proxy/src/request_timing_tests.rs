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
            .unwrap()
            .unwrap();
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
