// 旧版封装曾扩展协议 ID；下一轮原生请求必须恢复原始短 ID，而新响应直接透传。
// 固定字节按 f0f2360f 的格式独立生成，避免同一编码器和解码器一起变更仍误报通过。
fn old_long_id(raw: &str) -> &'static str {
    match raw {
        "rs_legacy_470" => "rs_4tCg4HetnN17mlxPMqIX2zlNE5qVZG8pk5ETO_9Rd56lQV9uWgnwN-lqQ1Xi8U6XcwYNCK0INWvaY9fdwcmjNdQcIdeO0fupOcGdKt_vu-H9udY0DgUPZT9WpQ4",
        "fc_legacy_471" => "fc_5cJhY6FKFbV4lwQW7CJnUjV4xzDIn4zUpGNMu0fCybGYxad3pP5lREFkPIVnv9Md1L6_Ub8gNpS3DPvHAaHoGkf0ywLy4De1orTg5lSsxeDAU5lXbtuojeH5LKE",
        "call_legacy_471" => "call_PMz7nLyxmNi-D3uQzDAPQkpKvYzYPjbbL4GQXz57x-Grjv_00Oei7v2mUAUWOkbYxU1uTgVI9UznKa_TlNLUbwo86czVRuWGESFqnoCdGbSnzzLZBpKwrYNr_Kc",
        "ci_legacy_473" => "ci_pai3WEHIdg-N1EpuEQhd9_r17-tpEQnN8vlfxsln6GmnKKRNxKR_XDO1ePoyHZKy8RRbsXuCoomJg-tbCYyosKfHxhKQkkmrD5ngJFpGTGsFDfV8XX-pDbGRxnQ",
        "cntr_legacy_473" => "cntr_EJl7ga4Mq3SuWx0xWa4EPSpE_wU114polw_XJ8gDMAHakX9u3Je0lyZqD80u9ddHOv-ogbx1VX18DqxSDJQEGQbFJAJ8QMvkTti3x8zrpeF-ISXlOjSeNKyMcFw",
        "rs_native_missing" => "rs_i8fZNycqz_4AcvEfUJfyvviVeS_ExhnYCSAR93nQpoX2irD_uKZxQuRQb_eczv2nB-UBabjkD5ODh5fohIn03Ip6dbyG9W1LkYqhyZZohbhlU9iklMSdzoxsdQw",
        _ => panic!("missing historical fixture"),
    }
}

#[tokio::test]
async fn legacy_long_ids_restore_exact_native_wire_at_history_index_470() {
    let upstream = MockServer::builder().start().await;
    let original = json!({
        "id":"resp_legacy_source","object":"response","status":"completed",
        "output":[
            {"type":"reasoning","id":"rs_legacy_470","summary":[{"type":"summary_text","text":"previous reasoning"}]},
            {"type":"function_call","id":"fc_legacy_471","call_id":"call_legacy_471","name":"f","arguments":"{}","status":"completed"},
            {"type":"code_interpreter_call","id":"ci_legacy_473","container_id":"cntr_legacy_473","code":"1","outputs":[],"status":"completed"}
        ],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
    });
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({"input":"legacy-seed"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(original.clone()))
        .with_priority(1)
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"resp_native_follow","object":"response","status":"completed","output":[],
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let authority = Arc::new(Authority::default());
    let app = router(&upstream.uri(), authority.clone());
    let first = app
        .oneshot(request_as(
            "sk-raw-owner",
            json!({
                "model":"raw-id-model","input":"legacy-seed"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(first["id"], original["id"]);
    assert_eq!(
        first["output"], original["output"],
        "new ids never acquire a seal"
    );
    let call = old_long_id("call_legacy_471");
    let container = old_long_id("cntr_legacy_473");
    let mut history = (0..470)
        .map(|index| json!({"role":"user","content":format!("prior turn {index}")}))
        .collect::<Vec<_>>();
    history.extend([
        json!({"type":"reasoning","id":old_long_id("rs_legacy_470"),"summary":[{"type":"summary_text","text":"previous reasoning"}]}),
        json!({"type":"function_call","id":old_long_id("fc_legacy_471"),"call_id":call,"name":"f","arguments":"{}","status":"completed"}),
        json!({"type":"function_call_output","call_id":call,"output":"ok"}),
        json!({"type":"code_interpreter_call","id":old_long_id("ci_legacy_473"),"container_id":container,"code":"1","outputs":[],"status":"completed"}),
        json!({"role":"user","content":"continue"}),
    ]);
    assert!(
        history[470]["id"].as_str().unwrap().len() > 64,
        "exercise the old expanded id, not a raw-id fixture"
    );
    let fresh = router(&upstream.uri(), authority.clone());
    let response = fresh
        .oneshot(request_as(
            "sk-raw-owner",
            json!({
                "model":"raw-id-model","input":history
            }),
        ))
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
    let returned: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(returned["id"], "resp_native_follow");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let sent: Value = serde_json::from_slice(&requests[1].body).unwrap();
    for (pointer, raw) in [
        ("/input/470/id", "rs_legacy_470"),
        ("/input/471/id", "fc_legacy_471"),
        ("/input/471/call_id", "call_legacy_471"),
        ("/input/472/call_id", "call_legacy_471"),
        ("/input/473/id", "ci_legacy_473"),
        ("/input/473/container_id", "cntr_legacy_473"),
    ] {
        let actual = sent.pointer(pointer).and_then(Value::as_str).unwrap();
        assert_eq!(
            actual, raw,
            "{pointer}: old wrappers never reach the provider"
        );
        assert_eq!(
            actual.len(),
            raw.len(),
            "{pointer}: exact original id length"
        );
    }
    let rotated = router_with_credential(&upstream.uri(), authority.clone(), "changed-account");
    for id in ["rs_legacy_470", old_long_id("rs_legacy_470")] {
        let response = rotated
            .clone()
            .oneshot(request_as(
                "sk-raw-owner",
                json!({
                    "model":"raw-id-model","input":[{"type":"reasoning","id":id,
                        "summary":[{"type":"summary_text","text":"previous reasoning"}]}]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "restoring a legacy id cannot authorize a replacement provider account"
        );
    }
    let foreign = router(&upstream.uri(), authority);
    let response = foreign
        .oneshot(request_as(
            "sk-raw-foreign",
            json!({
                "model":"raw-id-model","input":[{"type":"reasoning","id":"rs_legacy_470",
                    "summary":[{"type":"summary_text","text":"previous reasoning"}]}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        2,
        "foreign and rotated-account references never probe an upstream"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn legacy_long_reasoning_id_cannot_create_native_ownership() {
    let upstream = MockServer::builder().start().await;
    let authority = Arc::new(Authority::default());
    let alias = old_long_id("rs_native_missing");
    assert!(alias.len() > 64);
    let app = router(&upstream.uri(), authority);
    let mut denials = Vec::new();
    for id in ["rs_native_missing", alias] {
        let response = app
            .clone()
            .oneshot(request_as(
                "sk-raw-owner",
                json!({
                    "model":"raw-id-model","input":[{"type":"reasoning","id":id,
                        "summary":[{"type":"summary_text","text":"visible summary"}]}]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["error"]["param"], "input[0].id");
        denials.push(body);
    }
    assert_eq!(
        denials[0], denials[1],
        "a legacy alias cannot create durable ownership"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn known_native_reasoning_history_keeps_its_account_affinity_in_a_group() {
    let original = MockServer::builder().start().await;
    let unrelated = MockServer::builder().start().await;
    let sequence = std::sync::atomic::AtomicUsize::new(0);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |_: &wiremock::Request| {
            let index = sequence.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
            "id":format!("resp_native_affinity_{index}"),"object":"response","status":"completed",
            "output":if index == 0 {json!([{"type":"reasoning","id":"rs_native_affinity",
                "summary":[{"type":"summary_text","text":"previous reasoning"}]}])}else{json!([])},
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }))
        })
        .expect(2)
        .mount(&original)
        .await;
    let snapshot = new_snap_openai(&original.uri());
    let provider = openai_pk(&unrelated.uri());
    let unrelated_provider = "77777777-7777-4777-8777-777777777777";
    snapshot.provider_keys.insert(ResourceEntry::new(
        unrelated_provider,
        (*provider.value).clone(),
        1,
    ));
    for (id, name, provider) in [
        (MODEL, "affinity-source", OPENAI_PK_ID),
        (OTHER_MODEL, "affinity-other", unrelated_provider),
    ] {
        let model: aisix_core::Model = serde_json::from_value(json!({
            "display_name":name,"provider":"openai","model_name":"gpt-4o",
            "provider_key_id":provider
        }))
        .unwrap();
        snapshot.models.insert(ResourceEntry::new(id, model, 1));
    }
    let group: aisix_core::Model = serde_json::from_value(json!({
        "display_name":"affinity-group","routing":{"strategy":"failover","retries":0,
            "targets":[{"model_id":OTHER_MODEL},{"model_id":MODEL}]}
    }))
    .unwrap();
    snapshot.models.insert(ResourceEntry::new(
        "78888888-8888-4888-8888-888888888888",
        group,
        1,
    ));
    snapshot.apikeys.insert(tenant_key(
        OWNER,
        "sk-affinity",
        json!({
            "team_id":"raw-owner","allowed_models":["*"]
        }),
    ));
    let authority = Arc::new(Authority::default());
    let hub = Arc::new(Hub::new());
    hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
    let app = crate::build_router(
        crate::ProxyState::new(SnapshotHandle::new(snapshot), hub, &cfg())
            .without_cache()
            .with_platform(crate::platform::PlatformSettings::from_secret(
                PLATFORM_SECRET,
            ))
            .with_native_objects(authority),
    );
    let seeded = app
        .clone()
        .oneshot(request_as(
            "sk-affinity",
            json!({
                "model":"affinity-source","input":"seed"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(seeded.status(), StatusCode::OK);
    let seeded: Value =
        serde_json::from_slice(&to_bytes(seeded.into_body(), 65536).await.unwrap()).unwrap();
    let replayed = app
        .oneshot(request_as(
            "sk-affinity",
            json!({
                "model":"affinity-group","input":seeded["output"]
            }),
        ))
        .await
        .unwrap();
    let status = replayed.status();
    let bytes = to_bytes(replayed.into_body(), 65536).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        unrelated.received_requests().await.unwrap().is_empty(),
        "known native state must stay on the producing account"
    );
    assert_eq!(original.received_requests().await.unwrap().len(), 2);
    original.verify().await;
}

// 更早的租户封装已有 owner AAD，但尚未加入 item/call 的种类字节。
// 固定字节按 f8d9187d^ 独立生成；这里不能改用当前编码器生成期望输入。
fn pre_kind_long_id(raw: &str) -> &'static str {
    match raw {
        "msg_legacy_469" => "msg_DyLmXILD7FEOyuSDyAtREqibUf0QDqPCeUcRLYa-eWY8odC-5FnHNQFyAvMjnyzoSwIRQfSytcekV-TXuiD4IaFWNPg4ImsK9b3strFP1WHKc-St31Hkq_1yBts",
        "rs_legacy_470" => "rs_xXyKXOK9xbY-nTqXUJVggYwiByF-ONeja0QUtt2sz6UoyAxZkOyvVrkatBpV0IbDNrpfQbSAFU9SSv-00RipKdHIicwpG-RtHQmetfihNcbA0GbLnkJi9YDXl7s",
        "fc_legacy_471" => "fc_e1nHOqpPxJD_7KRqtTo2NqoQ1e3x8L0HN4tZMwdW6UVIZUmIQXHtcEro_gkzzf8q4ILTthckl07GkfUZ6EbQkYJoFdLsP7bhuGF7dPA4p0NWEOQ2MGlTxMRN0DQ",
        "call_legacy_471" => "call_491C9Xp9_E2ivfjNxK5kcvmBLCzr5HPmrWF3zPQaHicu7meN4vhVIdwfG2pPC5MlZT1Nj3u5vv_Y38-quXt57Tf7yExGTzuOmsKOExfR9O4w3jELncVUVYyfdQ4",
        _ => panic!("missing historical fixture"),
    }
}

#[tokio::test]
async fn pre_kind_legacy_ids_restore_stateless_native_history_without_native_rows() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"resp_stateless_new_raw","object":"response","status":"completed",
            "output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let authority = Arc::new(Authority::default());
    let app = router(&upstream.uri(), authority.clone());
    let mut history = (0..469)
        .map(|index| json!({"role":"user","content":format!("prior turn {index}")}))
        .collect::<Vec<_>>();
    // 这里只验证不透明状态的逐字传输，不冒充供应商密文解码的端到端验收。
    let ciphertext = "gAAAA-opaque-stateless-wire-fixture";
    history.extend([
        json!({"type":"message","role":"assistant","status":"completed",
            "id":pre_kind_long_id("msg_legacy_469"),
            "content":[{"type":"output_text","text":"previous answer","annotations":[]}]}),
        json!({"type":"reasoning","id":pre_kind_long_id("rs_legacy_470"),
            "summary":[],"encrypted_content":ciphertext}),
        json!({"type":"function_call","id":pre_kind_long_id("fc_legacy_471"),
            "call_id":pre_kind_long_id("call_legacy_471"),"name":"f","arguments":"{}"}),
        json!({"type":"function_call_output","call_id":pre_kind_long_id("call_legacy_471"),"output":"ok"}),
        json!({"role":"user","content":"continue"}),
    ]);
    let response = app
        .oneshot(request_as(
            "sk-raw-owner",
            json!({"model":"raw-id-model","store":false,"input":history}),
        ))
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
    let returned: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(returned["id"], "resp_stateless_new_raw");
    assert!(
        authority.lookups.lock().unwrap().is_empty(),
        "a complete stateless item must not fetch server-held history"
    );
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    for (pointer, raw) in [
        ("/input/469/id", "msg_legacy_469"),
        ("/input/470/id", "rs_legacy_470"),
        ("/input/471/id", "fc_legacy_471"),
        ("/input/471/call_id", "call_legacy_471"),
        ("/input/472/call_id", "call_legacy_471"),
    ] {
        assert_eq!(
            sent.pointer(pointer).and_then(Value::as_str),
            Some(raw),
            "{pointer}"
        );
    }
    assert_eq!(
        sent.pointer("/input/470/encrypted_content")
            .and_then(Value::as_str),
        Some(ciphertext)
    );
    upstream.verify().await;
}

#[tokio::test]
async fn pre_kind_legacy_seal_does_not_grant_native_stored_item_authority() {
    let upstream = MockServer::builder().start().await;
    let authority = Arc::new(Authority::default());
    let app = router(&upstream.uri(), authority);
    for (key, item) in [
        (
            "sk-raw-owner",
            json!({"type":"item_reference","id":pre_kind_long_id("rs_legacy_470")}),
        ),
        (
            "sk-raw-owner",
            json!({"type":"reasoning","id":pre_kind_long_id("rs_legacy_470"),"summary":[]}),
        ),
        (
            "sk-raw-foreign",
            json!({"type":"item_reference","id":pre_kind_long_id("rs_legacy_470")}),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request_as(
                key,
                json!({"model":"raw-id-model","input":[item]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["error"]["param"], "input[0].id");
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn pre_kind_legacy_call_prefix_restores_both_native_wire_slots() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"resp_legacy_call_raw","object":"response","status":"completed",
            "output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    // 旧编码器原样保留调用 ID 的前缀；两个关联槽位都必须还原。
    let call = "toolu_qS7aFeZo-nktJXXBwbAlznNfADpHkgjjNnZlokhNsmnc2RERqRJNOzbv8Bvj5RamQZUBGCHvB2F9We7k6SSOottbe3D50VS2Zhz948_2lV0-llIaETLSLQr-bu4";
    let authority = Arc::new(Authority::default());
    let response = router(&upstream.uri(), authority.clone())
        .oneshot(request_as("sk-raw-owner", json!({"model":"raw-id-model","input":[
            {"type":"function_call","id":pre_kind_long_id("fc_legacy_471"),"call_id":call,"name":"f","arguments":"{}"},
            {"type":"function_call_output","call_id":call,"output":"ok"}
        ]})))
        .await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 65536).await.unwrap();
    assert!(authority.lookups.lock().unwrap().is_empty());
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        sent.pointer("/input/0/id").and_then(Value::as_str),
        Some("fc_legacy_471")
    );
    for pointer in ["/input/0/call_id", "/input/1/call_id"] {
        assert_eq!(
            sent.pointer(pointer).and_then(Value::as_str),
            Some("toolu_legacy_call"),
            "{pointer}"
        );
    }
    upstream.verify().await;
}
