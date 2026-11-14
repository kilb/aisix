// 缓存不能替代输入存储对象的实时归属；同一状态热身后撤权/换账号，不能重建空缓存。
async fn cached_native_reasoning_revalidation(change: &str) {
    let upstream = MockServer::builder().start().await;
    let sequence = std::sync::atomic::AtomicUsize::new(0);
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let input: Value = serde_json::from_slice(&request.body).unwrap();
            let index = sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "id":format!("resp_cached_reasoning_{index}"),"object":"response","status":"completed",
                "output":if input["input"] == "seed" {json!([{
                    "type":"reasoning","id":"rs_cached_reasoning",
                    "summary":[{"type":"summary_text","text":"previous reasoning"}]
                }])}else{json!([])},
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
            }))
        }).expect(2).mount(&upstream).await;
    let snapshot = new_snap_openai(&upstream.uri());
    let model = openai_model("cached-native-reasoning");
    snapshot
        .models
        .insert(ResourceEntry::new(MODEL, (*model.value).clone(), 1));
    snapshot.apikeys.insert(tenant_key(
        OWNER,
        "sk-cache-owner",
        json!({
            "team_id":"raw-owner","allowed_models":["cached-native-reasoning"]
        }),
    ));
    let policy: aisix_core::CachePolicy = serde_json::from_value(json!({
        "name":"native-history-cache","enabled":true,"backend":"memory",
        "ttl_seconds":60,"applies_to":"all"
    }))
    .unwrap();
    snapshot
        .cache_policies
        .insert(ResourceEntry::new("native-history-cache", policy, 1));
    let handle = SnapshotHandle::new(snapshot);
    let authority = Arc::new(Authority::default());
    let hub = Arc::new(Hub::new());
    hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
    let app = crate::build_router(
        crate::ProxyState::new(handle.clone(), hub, &cfg())
            .with_platform(crate::platform::PlatformSettings::from_secret(
                PLATFORM_SECRET,
            ))
            .with_native_objects(authority.clone()),
    );
    let seeded = app
        .clone()
        .oneshot(request_as(
            "sk-cache-owner",
            json!({
                "model":"cached-native-reasoning","input":"seed"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(seeded.status(), StatusCode::OK);
    let seeded: Value =
        serde_json::from_slice(&to_bytes(seeded.into_body(), 65536).await.unwrap()).unwrap();
    let followup = json!({"model":"cached-native-reasoning","input":seeded["output"]});
    let mut answer_ids = Vec::new();
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(request_as("sk-cache-owner", followup.clone()))
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        answer_ids.push(body["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        answer_ids,
        ["resp_cached_reasoning_1", "resp_cached_reasoning_1"],
        "the valid repeat must read the exact previously stored answer"
    );
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        2,
        "prove a populated native cache before changing authority"
    );
    let expected = match change {
        "revoke" => {
            authority
                .revoked
                .lock()
                .unwrap()
                .insert(uuid::Uuid::parse_str(OWNER).unwrap());
            StatusCode::BAD_REQUEST
        }
        "rotate" => {
            let snapshot = handle.load();
            let provider = snapshot.provider_keys.get_by_id(OPENAI_PK_ID).unwrap();
            let mut changed = (*provider.value).clone();
            changed.api_key = "replacement-provider-account".into();
            snapshot
                .provider_keys
                .insert(ResourceEntry::new(OPENAI_PK_ID, changed, 2));
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => panic!("unknown authority change"),
    };
    let denied = app
        .oneshot(request_as("sk-cache-owner", followup))
        .await
        .unwrap();
    let status = denied.status();
    let bytes = to_bytes(denied.into_body(), 65536).await.unwrap();
    assert_eq!(
        status,
        expected,
        "{change}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        body.get("error").is_some(),
        "the old successful response is not authorization"
    );
    if change == "revoke" {
        assert_eq!(body["error"]["param"], "input[0].id");
    }
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        2,
        "invalidated native references neither read old cache nor probe a provider"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn cached_native_reasoning_rechecks_revoked_object_authority() {
    cached_native_reasoning_revalidation("revoke").await;
}

#[tokio::test]
async fn cached_native_reasoning_rechecks_rotated_provider_boundary() {
    cached_native_reasoning_revalidation("rotate").await;
}
