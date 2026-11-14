mod compat_tests {
    use super::*;

    // 这是动态回环组件测试，不是提供商兼容性或真实服务 E2E 的证据。
    struct Loopback {
        base: String,
        requests: tokio::sync::mpsc::Receiver<(axum::http::Uri, HeaderMap, Bytes)>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn loopback(
        reply: impl Fn(&axum::http::Uri) -> (StatusCode, HeaderMap, Bytes) + Send + Sync + 'static,
    ) -> Loopback {
        let (tx, requests) = tokio::sync::mpsc::channel(32);
        let reply = Arc::new(reply);
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let tx = tx.clone();
            let reply = reply.clone();
            async move {
                let (parts, body) = req.into_parts();
                let bytes = to_bytes(body, 1_048_576).await.unwrap();
                let answer = reply(&parts.uri);
                tx.send((parts.uri, parts.headers, bytes)).await.unwrap();
                answer
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Loopback {
            base,
            requests,
            task,
        }
    }

    fn snapshot(base: &str) -> AisixSnapshot {
        let snap = AisixSnapshot::new();
        snap.provider_keys.insert(openai_pk(PK_A, base));
        let value = serde_json::json!({
            "display_name": "jobs-a", "provider": "openai", "model_name": "gpt-4o",
            "provider_key_id": PK_A,
            "cost": {"input_per_1k": 10.0, "output_per_1k": 20.0,
                     "cached_input_per_1k": 1.0}
        });
        snap.models.insert(ResourceEntry::new(
            "m-a",
            serde_json::from_value::<Model>(value).unwrap(),
            1,
        ));
        snap.apikeys.insert(apikey_entry(&["*"]));
        snap
    }

    async fn next_batch_row(rx: &mut tokio::sync::mpsc::Receiver<ObsUsageEvent>) -> ObsUsageEvent {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = rx.recv().await.expect("用量通道关闭");
                if event.is_batch {
                    return event;
                }
            }
        })
        .await
        .expect("合法终态输出必须产生批次用量行")
    }

    async fn retrieve_batch(app: axum::Router) {
        let encoded = encode_routed_id("batch_compat", "jobs-a", None);
        let response = app
            .oneshot(get_as(format!("/v1/batches/{encoded}"), "sk-caller"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 65536).await.unwrap();
    }

    async fn batch_fixture(endpoint: &'static str, bodies: Vec<Value>) -> Loopback {
        let output = bodies
            .into_iter()
            .enumerate()
            .map(|(i, body)| {
                serde_json::json!({
                    "id": format!("batch_req_{i}"), "custom_id": format!("request-{i}"),
                    "response": {"status_code": 200, "body": body}, "error": null
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        loopback(move |uri| {
            let body = if uri.path() == "/v1/batches/batch_compat" {
                serde_json::json!({
                    "id": "batch_compat", "object": "batch", "status": "completed",
                    "endpoint": endpoint, "output_file_id": "file-output",
                    "completed_at": aisix_obs::metrics::process_start_time_seconds() + 1.0,
                    "metadata": {"aisix_api_key_id": "k-1", "aisix_model": "jobs-a"}
                })
                .to_string()
            } else {
                assert_eq!(uri.path(), "/v1/files/file-output/content");
                output.clone()
            };
            (StatusCode::OK, HeaderMap::new(), Bytes::from(body))
        })
        .await
    }

    // 删除按 endpoint 选择 Responses 解析器的分支，必须因没有批次行或计数错误变红。
    #[tokio::test]
    async fn responses_batch_preserves_reported_usage_and_requested_price_on_replay() {
        let upstream = batch_fixture(
            "/v1/responses",
            vec![serde_json::json!({
                "id": "resp-result", "object": "response", "status": "completed",
                "model": "provider-model-b", "service_tier": "default",
                "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15,
                          "input_tokens_details": {"cached_tokens": 3, "cache_write_tokens": 0},
                          "output_tokens_details": {"reasoning_tokens": 2}}
            })],
        )
        .await;
        let mut ids = Vec::new();
        for _ in 0..2 {
            let (app, mut rx) = build_app_with_sink(snapshot(&upstream.base));
            retrieve_batch(app).await;
            let row = next_batch_row(&mut rx).await;
            assert_eq!(
                (
                    row.prompt_tokens,
                    row.completion_tokens,
                    row.cached_prompt_tokens
                ),
                (10, 5, 3)
            );
            assert_eq!(row.reasoning_tokens, 2);
            assert_eq!(row.total_tokens, 15);
            assert_eq!(row.cache_write_tokens, Some(0));
            let wire = serde_json::to_value(&row).unwrap();
            assert_eq!(wire.get("cache_write_tokens"), Some(&serde_json::json!(0)));
            assert_eq!(row.modality_tokens.prompt_audio_tokens, None);
            assert_eq!(row.modality_tokens.completion_audio_tokens, None);
            assert_eq!(row.requested_model_id, "m-a");
            assert_eq!(row.model_id, "m-a");
            assert_eq!(row.api_key_id, "k-1");
            assert_eq!(row.provider_model_version, "provider-model-b");
            assert_eq!(row.service_tier.as_deref(), Some("default"));
            assert!(
                (row.cost_usd - 0.173).abs() < 1e-9,
                "应沿用 A 的现有费率: {}",
                row.cost_usd
            );
            assert!(!row.event_id.is_empty());
            assert_eq!(row.event_id, row.request_id);
            ids.push(row.event_id);
        }
        assert_eq!(ids[0], ids[1], "重启后的同一切片必须沿用去重身份");
    }

    // 任一合法贡献行没报 total，不能把其余行的 total 误当作整片上游总量。
    #[tokio::test]
    async fn responses_batch_total_requires_every_usage_contributor() {
        let upstream = batch_fixture("/v1/responses", vec![
            serde_json::json!({"model": "b", "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}),
            serde_json::json!({"model": "b", "usage": {"input_tokens": 2, "output_tokens": 1}}),
        ]).await;
        let (app, mut rx) = build_app_with_sink(snapshot(&upstream.base));
        retrieve_batch(app).await;
        let row = next_batch_row(&mut rx).await;
        assert_eq!((row.prompt_tokens, row.completion_tokens), (12, 6));
        assert_eq!(row.total_tokens, 0);
    }

    // 错用 Responses 的字段名会丢掉 Chat 的数量，忽略 detail 会丢掉显式零值。
    #[tokio::test]
    async fn chat_batch_preserves_reasoning_and_explicit_zero_cache_write() {
        let upstream = batch_fixture("/v1/chat/completions", vec![serde_json::json!({
            "model": "b", "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
                "prompt_tokens_details": {"cached_tokens": 3, "cache_write_tokens": 0, "audio_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 2}}
        })]).await;
        let (app, mut rx) = build_app_with_sink(snapshot(&upstream.base));
        retrieve_batch(app).await;
        let row = next_batch_row(&mut rx).await;
        assert_eq!(
            (
                row.prompt_tokens,
                row.completion_tokens,
                row.cached_prompt_tokens
            ),
            (10, 5, 3)
        );
        assert_eq!(row.reasoning_tokens, 2);
        assert_eq!(row.cache_write_tokens, Some(0));
        assert_eq!(row.modality_tokens.prompt_audio_tokens, Some(0));
        assert_eq!(row.total_tokens, 15);
    }

    #[tokio::test]
    async fn embeddings_batch_uses_its_prompt_only_usage_contract() {
        let upstream = batch_fixture(
            "/v1/embeddings",
            vec![serde_json::json!({
                "object": "list", "model": "b", "data": [],
                "usage": {"prompt_tokens": 10, "total_tokens": 10}
            })],
        )
        .await;
        let (app, mut rx) = build_app_with_sink(snapshot(&upstream.base));
        retrieve_batch(app).await;
        let row = next_batch_row(&mut rx).await;
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens, row.total_tokens),
            (10, 0, 10)
        );
    }

    // 只解码 after；重复的其它参数、空值与转义必须保持，不能先塞进 HashMap。
    #[tokio::test]
    async fn routed_list_cursors_are_decoded_without_losing_query_pairs() {
        let mut upstream = loopback(|_| {
            (
                StatusCode::OK,
                HeaderMap::new(),
                Bytes::from_static(br#"{"object":"list","data":[]}"#),
            )
        })
        .await;
        let app = build_app(snapshot(&upstream.base));
        for (route, raw) in [
            ("files", "file-cursor"),
            ("batches", "batch-cursor"),
            ("fine_tuning/jobs", "ftjob-cursor"),
        ] {
            let id = encode_routed_id(raw, "jobs-a", None);
            let response = app
                .clone()
                .oneshot(get_as(
                    format!("/v1/{route}?model=jobs-a&after={id}&tag=a&tag=b&empty=&opaque=%2F"),
                    "sk-caller",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let (uri, _, _) = upstream.requests.recv().await.unwrap();
            assert_eq!(
                uri.query(),
                Some(format!("after={raw}&tag=a&tag=b&empty=&opaque=%2F").as_str())
            );
        }
    }

    // 游标不能借另一个模型切换上游账号，也不能绕过原有 owner seal。
    #[tokio::test]
    async fn list_cursors_reject_wrong_target_or_forged_owner_without_dispatch() {
        let mut upstream =
            loopback(|_| (StatusCode::OK, HeaderMap::new(), Bytes::from_static(b"{}"))).await;
        let snap = snapshot(&upstream.base);
        snap.provider_keys.insert(openai_pk(PK_B, &upstream.base));
        snap.models.insert(model("m-b", "jobs-b", PK_B));
        let app = build_app(snap);
        let wrong_target = encode_routed_id("file-other", "jobs-b", None);
        let wrong_owner = encode_routed_id(
            "file-other",
            "jobs-a",
            Some(("team:other", "guessed-secret")),
        );
        for id in [wrong_target, wrong_owner] {
            let response = app
                .clone()
                .oneshot(get_as(
                    format!("/v1/files?model=jobs-a&after={id}"),
                    "sk-caller",
                ))
                .await
                .unwrap();
            assert!(response.status().is_client_error(), "非法游标不能查询上游");
        }
        assert!(upstream.requests.try_recv().is_err());
    }

    // 删除重建请求体前的表示头清理，会产生重复 Content-Type 或错误 Content-Length。
    #[tokio::test]
    async fn rebuilt_jobs_bodies_replace_operator_representation_headers() {
        let mut upstream =
            loopback(|_| (StatusCode::OK, HeaderMap::new(), Bytes::from_static(b"{}"))).await;
        let snap = snapshot(&upstream.base);
        let (_, _, state) = build_app_with_sink_and_state(snap);
        let snap = state.snapshot.load();
        let auth =
            AuthenticatedKey::new(&snap, snap.apikeys.get_by_id("k-1").unwrap(), None, false);
        let mut target =
            resolve_target(&snap, &auth, Some("jobs-a"), &ClientContext::default()).unwrap();
        target.extra_headers = vec![
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/operator-stale"),
            ),
            (header::CONTENT_LENGTH, HeaderValue::from_static("1")),
        ];
        for multipart in [false, true] {
            let body = if multipart {
                UpstreamBody::Multipart(
                    reqwest::multipart::Form::new()
                        .text("purpose", "batch")
                        .part(
                            "file",
                            reqwest::multipart::Part::bytes(b"hello".to_vec())
                                .file_name("input.jsonl"),
                        ),
                )
            } else {
                UpstreamBody::Json(Bytes::from_static(br#"{"endpoint":"/v1/responses"}"#))
            };
            send_upstream(
                &state,
                &target,
                Method::POST,
                &format!("{}/v1/files", upstream.base),
                body,
                "component-headers",
            )
            .await
            .expect("重建后的请求必须可发送");
            let (_, headers, bytes) = upstream.requests.recv().await.unwrap();
            let types: Vec<_> = headers.get_all(header::CONTENT_TYPE).iter().collect();
            assert_eq!(types.len(), 1, "Content-Type 必须只有最终请求体的一项");
            let content_type = types[0].to_str().unwrap();
            if multipart {
                let boundary = content_type
                    .strip_prefix("multipart/form-data; boundary=")
                    .expect("使用实际 multipart boundary");
                assert!(String::from_utf8_lossy(&bytes).starts_with(&format!("--{boundary}\r\n")));
            } else {
                assert_eq!(content_type, "application/json");
                assert_eq!(&bytes[..], br#"{"endpoint":"/v1/responses"}"#);
            }
            let lengths: Vec<_> = headers.get_all(header::CONTENT_LENGTH).iter().collect();
            assert!(lengths.len() <= 1);
            if let Some(length) = lengths.first() {
                assert_eq!(
                    length.to_str().unwrap().parse::<usize>().unwrap(),
                    bytes.len()
                );
            }
        }
    }

    // 托管路由的上游错误断言先具备有效归属，否则测到的只是准入拒绝。
    struct HeaderRegistry {
        origin: aisix_core::native_objects::OriginBinding,
        key: uuid::Uuid,
    }
    macro_rules! unused_header_registry {
        ($name:ident,$request:ty) => {
            fn $name(
                &self,
                _: $request,
            ) -> futures::future::BoxFuture<
                '_,
                Result<
                    aisix_core::native_objects::NativeReply,
                    crate::native_api::registry::NativeRegistryError,
                >,
            > {
                Box::pin(async {
                    Err(crate::native_api::registry::NativeRegistryError::Unavailable)
                })
            }
        };
    }
    impl crate::native_api::registry::NativeObjectRegistry for HeaderRegistry {
        unused_header_registry!(begin, aisix_core::native_objects::BeginRequest);
        unused_header_registry!(bind, aisix_core::native_objects::BindRequest);
        unused_header_registry!(owned, aisix_core::native_objects::OwnedRequest);
        unused_header_registry!(tombstone, aisix_core::native_objects::TombstoneRequest);
        unused_header_registry!(claim, aisix_core::native_objects::ClaimRequest);
        unused_header_registry!(renew, aisix_core::native_objects::LeaseRequest);
        unused_header_registry!(reschedule, aisix_core::native_objects::LeaseRequest);
        unused_header_registry!(complete, aisix_core::native_objects::CompleteRequest);
        fn lookup(
            &self,
            request: aisix_core::native_objects::LookupRequest,
        ) -> futures::future::BoxFuture<
            '_,
            Result<
                aisix_core::native_objects::NativeReply,
                crate::native_api::registry::NativeRegistryError,
            >,
        > {
            assert_eq!(request.api_key_id, self.key);
            let mut reply = aisix_core::native_objects::NativeReply::new("found");
            reply.record=Some(serde_json::from_value(serde_json::json!({
                "record_id":uuid::Uuid::new_v4(),"protocol":"openai","kind":request.locator.kind,
                "origin":self.origin,"requested_model_id":self.origin.model_id,"raw_id":request.locator.raw_id,
                "requested_model":"jobs-a","version":1
            })).unwrap());
            Box::pin(async move { Ok(reply) })
        }
    }

    // JSON 重写不能抹掉 SDK 的重试时机/诊断 id，也不能带回旧长度和连接级头。
    #[tokio::test]
    async fn jobs_error_headers_keep_retry_and_sdk_request_identity() {
        let upstream = loopback(|_| {
            let mut headers = HeaderMap::new();
            headers.insert("retry-after", HeaderValue::from_static("30"));
            headers.insert("retry-after-ms", HeaderValue::from_static("30000"));
            headers.insert("x-request-id", HeaderValue::from_static("req_component"));
            headers.insert("x-provider-account", HeaderValue::from_static("private"));
            (StatusCode::TOO_MANY_REQUESTS, headers, Bytes::from_static(br#"{"error":{"message":"Rate limit reached","type":"rate_limit_error","code":"rate_limit_exceeded","param":null}}"#))
        }).await;
        for platform in [false, true] {
            let (_, _, mut state) = build_app_with_sink_and_state(snapshot(&upstream.base));
            if platform {
                let platform = crate::platform::PlatformSettings::from_secret("component-test-key");
                let snapshot = state.snapshot.load();
                let model = snapshot.models.remove("m-a").unwrap();
                let model_id =
                    uuid::Uuid::parse_str("55555555-5555-4555-8555-555555555555").unwrap();
                snapshot.models.insert(ResourceEntry::new(
                    model_id.to_string(),
                    (*model.value).clone(),
                    1,
                ));
                let key = snapshot.apikeys.remove("k-1").unwrap();
                let key_id = uuid::Uuid::parse_str("66666666-6666-4666-8666-666666666666").unwrap();
                snapshot.apikeys.insert(ResourceEntry::new(
                    key_id.to_string(),
                    (*key.value).clone(),
                    1,
                ));
                let provider = snapshot.provider_keys.get_by_id(PK_A).unwrap();
                let origin = aisix_core::native_objects::OriginBinding {
                    surface: None,
                    model_id,
                    provider_key_id: uuid::Uuid::parse_str(PK_A).unwrap(),
                    boundary: aisix_core::native_objects::configured_native_origin(
                        &platform.id_key,
                        uuid::Uuid::parse_str(PK_A).unwrap(),
                        &provider.value,
                        aisix_core::native_objects::NativeProtocol::Openai,
                        aisix_core::native_objects::NativeObjectKind::File,
                    )
                    .unwrap(),
                };
                drop(snapshot);
                state =
                    state
                        .with_platform(platform)
                        .with_native_objects(Arc::new(HeaderRegistry {
                            origin,
                            key: key_id,
                        }));
            }
            let app = crate::build_router(state);
            for route in [
                "files/file-test",
                "batches/batch-test",
                "fine_tuning/jobs/ftjob-test",
            ] {
                let response = app
                    .clone()
                    .oneshot(get_as(format!("/v1/{route}?model=jobs-a"), "sk-caller"))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                assert_eq!(
                    response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok()),
                    Some("30")
                );
                assert_eq!(
                    response
                        .headers()
                        .get("retry-after-ms")
                        .and_then(|v| v.to_str().ok()),
                    Some("30000")
                );
                if platform {
                    assert!(
                        response.headers().get("x-request-id").is_none(),
                        "平台不暴露上游请求身份"
                    );
                } else {
                    assert_eq!(
                        response
                            .headers()
                            .get("x-request-id")
                            .and_then(|v| v.to_str().ok()),
                        Some("req_component")
                    );
                }
                assert!(response.headers().get("x-provider-account").is_none());
            }
        }
    }

    #[tokio::test]
    async fn legacy_job_quota_rejections_keep_each_resolved_target_without_dispatch() {
        for family in [
            "upload",
            "batch_create",
            "batch_retrieve",
            "fine_tuning_create",
            "forward",
        ] {
            let upstream = MockServer::builder().start().await;
            Mock::given(wm_method("GET"))
                .and(path("/v1/files/file_prime"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id":"file_prime","object":"file","purpose":"user_data"
                })))
                .expect(1)
                .mount(&upstream)
                .await;
            let snap = snapshot(&upstream.uri());
            let entry = snap.models.get_by_id("m-a").unwrap();
            let mut value = serde_json::to_value(&*entry.value).unwrap();
            value["rate_limit"] = serde_json::json!({"rpd":1});
            snap.models.insert(ResourceEntry::new(
                "m-a",
                serde_json::from_value::<Model>(value).unwrap(),
                2,
            ));
            let (app, mut events) = build_app_with_sink(snap);
            let first = app
                .clone()
                .oneshot(get_as(
                    "/v1/files/file_prime?model=jobs-a".into(),
                    "sk-caller",
                ))
                .await
                .unwrap();
            assert_eq!(first.status(), StatusCode::OK, "{family}");
            to_bytes(first.into_body(), 65536).await.unwrap();
            let first_event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(first_event.model_id, "m-a");
            assert_eq!(first_event.upstream_dispatched, Some(true));
            let request = if family == "upload" {
                let boundary = "legacy-quota-upload";
                let body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\njobs-a\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nuser_data\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"sample.txt\"\r\nContent-Type: text/plain\r\n\r\nquota attribution sample\r\n--{boundary}--\r\n");
                Request::builder()
                    .method("POST")
                    .uri("/v1/files")
                    .header("authorization", "Bearer sk-caller")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap()
            } else if family == "batch_retrieve" {
                get_as("/v1/batches/batch_quota?model=jobs-a".into(), "sk-caller")
            } else if family == "forward" {
                get_as(
                    "/v1/fine_tuning/jobs/ft_quota?model=jobs-a".into(),
                    "sk-caller",
                )
            } else {
                let file = encode_routed_id("file_input", "jobs-a", None);
                let (uri, body) = if family == "batch_create" {
                    (
                        "/v1/batches",
                        serde_json::json!({"input_file_id":file,"endpoint":"/v1/chat/completions","completion_window":"24h"}),
                    )
                } else {
                    (
                        "/v1/fine_tuning/jobs",
                        serde_json::json!({"training_file":file,"model":"gpt-4o-mini-2024-07-18"}),
                    )
                };
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("authorization", "Bearer sk-caller")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap()
            };
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{family}");
            to_bytes(response.into_body(), 65536).await.unwrap();
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event.status_code, 429, "{family}");
            assert_eq!(event.model_id, "m-a", "{family}");
            assert_eq!(event.requested_model, "jobs-a", "{family}");
            assert_eq!(event.requested_model_id, "m-a", "{family}");
            assert_eq!(event.provider_key_id.as_deref(), Some(PK_A), "{family}");
            assert_eq!(event.upstream_dispatched, Some(false), "{family}");
            assert_eq!(
                event.inference_stage,
                Some(aisix_core::native_objects::InferenceStage::Management)
            );
            assert!(event.is_final_attempt, "{family}");
            assert_eq!(
                (
                    event.prompt_tokens,
                    event.completion_tokens,
                    event.total_tokens
                ),
                (0, 0, 0),
                "{family}"
            );
            assert_eq!(event.cost_usd, 0.0, "{family}");
            assert_eq!(
                upstream.received_requests().await.unwrap().len(),
                1,
                "{family}: only quota priming may dispatch"
            );
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn legacy_job_validation_failure_does_not_borrow_an_unresolved_target() {
        let upstream = MockServer::builder().start().await;
        let (app, mut events) = build_app_with_sink(snapshot(&upstream.uri()));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/batches")
                    .header("authorization", "Bearer sk-caller")
                    .header("content-type", "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        to_bytes(response.into_body(), 65536).await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.status_code, 400);
        assert!(event.model_id.is_empty());
        assert!(event.requested_model_id.is_empty());
        assert!(event.provider_key_id.is_none_or(|id| id.is_empty()));
        assert_eq!(event.upstream_dispatched, Some(false));
        assert_eq!(
            event.inference_stage,
            Some(aisix_core::native_objects::InferenceStage::Management)
        );
        assert!(event.is_final_attempt);
        assert_eq!(
            (
                event.prompt_tokens,
                event.completion_tokens,
                event.total_tokens
            ),
            (0, 0, 0)
        );
        assert_eq!(event.cost_usd, 0.0);
        assert!(upstream.received_requests().await.unwrap().is_empty());
        assert!(events.try_recv().is_err());
    }
}
